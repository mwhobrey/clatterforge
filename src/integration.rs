//! Explicit, evidence-bound source writes. The journal precedes filesystem mutation;
//! interrupted writes require operator recovery, never a replay of the coding task.
use crate::{
    agents::{AgentStatus, Agents},
    coordination::Coordination,
    digest, now, open_sqlite_with_retry,
    orchestration::safe_file,
    process::{Presence, ProcessIdentity, current_identity, presence},
    repository::{Repository, git, git_output, git_text},
    service::with_repository_lease,
    workflow::Workflows,
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    path::{Path, PathBuf},
};

const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Change {
    pub path: String,
    pub before_digest: Option<String>,
    pub after_digest: Option<String>,
    pub before_bytes: usize,
    pub after_bytes: usize,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub at: u64,
    pub stage: String,
    pub handler: String,
    pub by: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationTarget {
    #[default]
    Checkout,
    Worktree,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    pub task_id: String,
    pub revision: u64,
    pub status: String,
    #[serde(default)]
    pub target: IntegrationTarget,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_branch: Option<String>,
    pub repository: Repository,
    pub evidence_digest: String,
    pub plan_digest: String,
    pub files: Vec<Change>,
    pub owner: Option<ProcessIdentity>,
    pub error: Option<String>,
    pub events: Vec<Event>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct FileChange {
    path: String,
    before: Option<String>,
    after: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Plan {
    #[serde(default)]
    target: IntegrationTarget,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_checkout: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    worktree_branch: Option<String>,
    repository: Repository,
    task_revision: u64,
    evidence_digest: String,
    index: PathBuf,
    index_digest: String,
    files: Vec<FileChange>,
}
pub(crate) struct Candidate {
    repository: Repository,
    revision: u64,
    evidence_digest: String,
    files: BTreeMap<String, Option<String>>,
}
fn text(path: &Path) -> Result<Option<String>> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
        Ok(meta) => {
            ensure!(
                meta.is_file() && !meta.file_type().is_symlink() && meta.len() <= MAX_FILE_BYTES,
                "integration requires a regular text file up to 4 MiB: {}",
                path.display()
            );
            Ok(Some(String::from_utf8(fs::read(path)?)?))
        }
    }
}

fn candidate_files(
    workspace: &Path,
    scope: &[String],
    changed: &BTreeSet<String>,
) -> Result<BTreeMap<String, Option<String>>> {
    scope
        .iter()
        .filter(|name| changed.contains(name.as_str()))
        .map(|name| Ok((name.clone(), text(&safe_file(workspace, name)?)?)))
        .collect()
}
fn artifact(a: &crate::store::Artifact) -> Result<Vec<u8>> {
    ensure!(
        a.bytes <= 4 * 1024 * 1024 && fs::metadata(&a.path)?.len() == a.bytes as u64,
        "integration evidence artifact size changed"
    );
    let bytes = fs::read(&a.path)?;
    ensure!(
        bytes.len() == a.bytes && digest(&bytes) == a.hash,
        "integration evidence artifact changed"
    );
    Ok(bytes)
}
fn candidate(root: &Path, id: &str) -> Result<Candidate> {
    let workflows = Workflows::open(root)?;
    let agents = Agents::open(root)?;
    let (repo, revision, evidence, files, child) = if let Ok(w) = workflows.get(id) {
        let evidence = w.evidence_digest()?;
        ensure!(
            w.status == AgentStatus::Accepted
                && w.review
                    .as_ref()
                    .is_some_and(|r| r.accepted && r.evidence_digest == evidence),
            "integration requires accepted workflow evidence"
        );
        for step in &w.steps {
            workflows.validate_step(&w, step)?;
        }
        for read in &w.git_reads {
            crate::workflow_workspace::read_page(read)?;
        }
        let last = w.steps.last().context("workflow has no candidate")?;
        let files = if let Some(checkpoint) = &last.checkpoint {
            let task = agents.task(&last.task_id)?;
            let changed: BTreeSet<String> = crate::workflow_workspace::stage_changes(
                &w.assignment.profile.repository,
                &w.assignment.profile.repository.base,
                checkpoint,
            )?
            .into_iter()
            .collect();
            candidate_files(
                &task.assignment.workspace,
                &w.assignment.profile.scope(),
                &changed,
            )?
        } else {
            last.files.clone()
        };
        (
            w.assignment.profile.repository.clone(),
            w.revision,
            evidence,
            files,
            last.task_id.clone(),
        )
    } else {
        let task = agents.task(id)?;
        let evidence = agents.evidence_digest(&task)?;
        ensure!(
            task.status == AgentStatus::Accepted
                && task
                    .review
                    .as_ref()
                    .is_some_and(|r| r.accepted && r.evidence_digest == evidence),
            "integration requires accepted agent evidence"
        );
        ensure!(
            task.assignment.handoff.is_none(),
            "integrate the aggregate parent workflow instead of its child"
        );
        let changed: BTreeSet<String> = crate::workflow_workspace::changed(
            root,
            &task.assignment.workspace,
            &task.assignment.repository.base,
            &task.assignment.spec.allowed_files,
        )?
        .into_iter()
        .collect();
        let files = candidate_files(
            &task.assignment.workspace,
            &task.assignment.spec.allowed_files,
            &changed,
        )?;
        (
            task.assignment.repository.clone(),
            task.revision,
            evidence,
            files,
            task.id,
        )
    };
    repo.revalidate()?;
    let task = agents.task(&child)?;
    let actual = Repository::inspect(&task.assignment.workspace)?;
    ensure!(
        actual.common_dir == repo.common_dir && actual.base == repo.base && actual.branch.is_none(),
        "candidate repository/base changed"
    );
    for receipt in [&task.execution, &task.verification] {
        let receipt = receipt
            .as_ref()
            .context("integration requires completed process evidence")?;
        ensure!(
            receipt.outcome == "succeeded"
                && receipt.exit_code == Some(0)
                && receipt.tree_terminated,
            "integration requires successful, terminated generation and verification"
        );
        artifact(&receipt.stdout.artifact)?;
        artifact(&receipt.stderr.artifact)?;
    }
    let baseline = task
        .assignment
        .spec
        .verification_files
        .iter()
        .map(|name| {
            let original = git(&repo.checkout, &["show", &format!("{}:{name}", repo.base)])?;
            ensure!(
                fs::read(safe_file(&task.assignment.workspace, name)?)? == original,
                "protected verification file changed: {name}"
            );
            Ok((name.clone(), Some(digest(&original))))
        })
        .collect::<Result<_>>()?;
    agents.scope(&task, &baseline)?;
    let patch = agents.patch(&task)?;
    ensure!(
        patch == artifact(task.patch.as_ref().context("candidate has no patch")?)?,
        "candidate changed after verification/review"
    );
    ensure!(
        !patch
            .split(|b| *b == b'\n')
            .any(|line| line.starts_with(b"old mode ")
                || line.starts_with(b"new mode ")
                || line.starts_with(b"new file mode 100755")),
        "source integration supports content changes, not executable-mode changes"
    );
    for (name, expected) in &files {
        ensure!(
            &text(&safe_file(&task.assignment.workspace, name)?)? == expected,
            "candidate snapshot changed: {name}"
        );
    }
    Ok(Candidate {
        repository: repo,
        revision,
        evidence_digest: evidence,
        files,
    })
}
fn fingerprint(path: &Path) -> Result<String> {
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.is_file()
            && !meta.file_type().is_symlink()
            && crate::repository::canonical(path)? == path,
        "source Git index is aliased or not a regular file"
    );
    ensure!(
        fs::metadata(path)?.len() <= 16 * 1024 * 1024,
        "Git index exceeds integration limit"
    );
    Ok(digest(&fs::read(path)?))
}
fn source_clean(repo: &Repository) -> Result<()> {
    repo.ensure_clean(
        "source checkout is dirty; preserve and resolve its changes before integration",
    )
}

fn integration_worktree_path(root: &Path, task: &str) -> Result<PathBuf> {
    uuid::Uuid::parse_str(task)?;
    Ok(root.join("integrations").join(task))
}

fn unique_integration_branch(source: &Repository, task: &str) -> Result<String> {
    let base = format!("conductor/integrate-{}", &task[..8.min(task.len())]);
    let ref_exists = |branch: &str| -> Result<bool> {
        let out = git_output(
            &source.checkout,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ],
        )?;
        Ok(out.exit_code == Some(0))
    };
    if !ref_exists(&base)? {
        return Ok(base);
    }
    for attempt in 2..=64 {
        let candidate = format!("{base}-{attempt}");
        if !ref_exists(&candidate)? {
            return Ok(candidate);
        }
    }
    let suffix = uuid::Uuid::new_v4().to_string();
    Ok(format!("{base}-{}", &suffix[..8]))
}

fn allocate_worktree(root: &Path, source: &Repository, task: &str) -> Result<(Repository, String)> {
    let wt_dir = integration_worktree_path(root, task)?;
    ensure!(
        !wt_dir.exists(),
        "integration worktree path already exists: {}",
        wt_dir.display()
    );
    if let Some(parent) = wt_dir.parent() {
        fs::create_dir_all(parent)?;
    }
    let branch = unique_integration_branch(source, task)?;
    let wt_str = wt_dir.to_str().context("UTF-8 worktree path required")?;
    git(
        &source.checkout,
        &[
            "-c",
            "core.autocrlf=false",
            "worktree",
            "add",
            "-b",
            &branch,
            wt_str,
            &source.base,
        ],
    )?;
    match Repository::inspect(&wt_dir) {
        Ok(repo) => Ok((repo, branch)),
        Err(err) => {
            remove_unapplied_worktree(&source.checkout, &wt_dir, Some(&branch));
            Err(err)
        }
    }
}

fn remove_unapplied_worktree(source_checkout: &Path, wt_checkout: &Path, branch: Option<&str>) {
    if let Some(wt_str) = wt_checkout.to_str() {
        let _ = git(source_checkout, &["worktree", "remove", "--force", wt_str]);
    }
    if wt_checkout.exists() {
        let _ = fs::remove_dir_all(wt_checkout);
    }
    let _ = git(source_checkout, &["worktree", "prune"]);
    if let Some(branch) = branch {
        let _ = git(source_checkout, &["branch", "-D", branch]);
    }
}

fn plan_matches_candidate(c: &Candidate, plan: &Plan) -> Result<()> {
    ensure!(
        c.revision == plan.task_revision && c.evidence_digest == plan.evidence_digest,
        "accepted candidate/evidence changed"
    );
    match plan.target {
        IntegrationTarget::Checkout => {
            ensure!(
                c.repository == plan.repository,
                "accepted candidate/evidence changed"
            );
        }
        IntegrationTarget::Worktree => {
            c.repository.revalidate()?;
            plan.repository.revalidate()?;
            ensure!(
                c.repository.common_dir == plan.repository.common_dir
                    && c.repository.identity == plan.repository.identity
                    && c.repository.base == plan.repository.base
                    && plan.source_checkout.as_ref() == Some(&c.repository.checkout),
                "accepted candidate/evidence changed"
            );
        }
    }
    Ok(())
}
fn base_text(repo: &Repository, name: &str) -> Result<Option<String>> {
    let entries = git(&repo.checkout, &["ls-tree", "-z", &repo.base, "--", name])?;
    if entries.is_empty() {
        return Ok(None);
    }
    let entry = std::str::from_utf8(&entries)?.trim_end_matches('\0');
    let (meta, path) = entry.split_once('\t').context("invalid Git tree entry")?;
    ensure!(
        path == name && (meta.starts_with("100644 blob ") || meta.starts_with("100755 blob ")),
        "source is not one regular Git file: {name}"
    );
    Ok(Some(String::from_utf8(git(
        &repo.checkout,
        &["show", &format!("{}:{name}", repo.base)],
    )?)?))
}
pub struct Integrations {
    connection: Connection,
    root: PathBuf,
}
impl Integrations {
    pub fn open(root: &Path) -> Result<Self> {
        let root = crate::repository::canonical(root)?;
        let connection = open_sqlite_with_retry(&root.join("state.sqlite"), 1)?;
        crate::ensure_table_schema(
            &connection,
            "source_integrations",
            "CREATE TABLE IF NOT EXISTS source_integrations(task_id TEXT PRIMARY KEY,record TEXT NOT NULL,plan TEXT NOT NULL);",
        )?;
        Ok(Self { connection, root })
    }
    pub fn get(&self, task: &str) -> Result<Option<Record>> {
        let s: Option<String> = self
            .connection
            .query_row(
                "SELECT record FROM source_integrations WHERE task_id=?1",
                [task],
                |r| r.get(0),
            )
            .optional()?;
        s.map(|s| Ok(serde_json::from_str(&s)?)).transpose()
    }
    fn plan(&self, task: &str) -> Result<Plan> {
        let s: String = self.connection.query_row(
            "SELECT plan FROM source_integrations WHERE task_id=?1",
            [task],
            |r| r.get(0),
        )?;
        let plan: Plan = serde_json::from_str(&s)?;
        ensure!(
            digest(&serde_json::to_vec(&plan)?)
                == self.get(task)?.context("missing integration")?.plan_digest,
            "integration journal changed"
        );
        Ok(plan)
    }
    fn change(
        &mut self,
        task: &str,
        revision: u64,
        stage: &str,
        by: &str,
        update: impl FnOnce(&mut Record),
    ) -> Result<Record> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let s: String = tx.query_row(
            "SELECT record FROM source_integrations WHERE task_id=?1",
            [task],
            |r| r.get(0),
        )?;
        let mut r: Record = serde_json::from_str(&s)?;
        ensure!(r.revision == revision, "integration revision conflict");
        update(&mut r);
        r.revision += 1;
        r.events.push(Event {
            at: now(),
            stage: stage.into(),
            handler: match stage {
                "source_file_applied" => "integration::replace",
                "source_file_restored" | "integration_rolled_back" | "integration_unknown" => {
                    "Integrations::restore"
                }
                "integration_restore_started" | "integration_recovery_blocked" => {
                    "Integrations::rollback"
                }
                "integration_recovery_required" => "Integrations::finish_owner",
                _ => "Integrations::apply",
            }
            .into(),
            by: by.into(),
        });
        tx.execute(
            "UPDATE source_integrations SET record=?2 WHERE task_id=?1",
            params![task, serde_json::to_string(&r)?],
        )?;
        tx.commit()?;
        Ok(r)
    }
    pub fn prepare(&mut self, task: &str, by: &str) -> Result<Record> {
        actor(by)?;
        if let Some(record) = self.get(task)? {
            return Ok(record);
        }
        self.prepare_with_target(task, by, IntegrationTarget::Checkout)
    }

    pub fn prepare_with_target(
        &mut self,
        task: &str,
        by: &str,
        target: IntegrationTarget,
    ) -> Result<Record> {
        actor(by)?;
        let existing = self.get(task)?;
        if let Some(record) = &existing {
            if record.target == target {
                return Ok(record.clone());
            }
            ensure!(
                record.status == "prepared",
                "integration target cannot change after status '{}'",
                record.status
            );
        }
        let c = candidate(&self.root, task)?;
        let (dest_repo, worktree_branch, created_worktree) = match target {
            IntegrationTarget::Checkout => {
                source_clean(&c.repository)?;
                (c.repository.clone(), None, false)
            }
            IntegrationTarget::Worktree => {
                let (wt_repo, branch) = allocate_worktree(&self.root, &c.repository, task)?;
                (wt_repo, Some(branch), true)
            }
        };
        let build_result = (|| -> Result<Plan> {
            source_clean(&dest_repo)?;
            let index = PathBuf::from(git_text(
                &dest_repo.checkout,
                &["rev-parse", "--path-format=absolute", "--git-path", "index"],
            )?);
            ensure!(
                crate::repository::canonical(
                    index.parent().context("missing Git index directory")?
                )?
                .starts_with(&dest_repo.common_dir),
                "Git index escaped repository"
            );
            let mut files = vec![];
            for (name, after) in &c.files {
                let base = base_text(&dest_repo, name)?;
                let path = safe_file(&dest_repo.checkout, name)?;
                ensure!(
                    path.parent().is_some_and(Path::is_dir),
                    "integration requires an existing parent directory: {name}"
                );
                let before = text(&path)?;
                ensure!(
                    base.is_some() == before.is_some(),
                    "source file presence conflicts with Git base: {name}"
                );
                if base == *after {
                    continue;
                }
                files.push(FileChange {
                    path: name.clone(),
                    before,
                    after: after.clone(),
                });
            }
            ensure!(
                !files.is_empty() && files.len() <= 16,
                "integration requires 1-16 changed files"
            );
            Ok(Plan {
                target,
                source_checkout: (target == IntegrationTarget::Worktree)
                    .then(|| c.repository.checkout.clone()),
                worktree_branch: worktree_branch.clone(),
                repository: dest_repo.clone(),
                task_revision: c.revision,
                evidence_digest: c.evidence_digest.clone(),
                index_digest: fingerprint(&index)?,
                index,
                files,
            })
        })();
        let plan = match build_result {
            Ok(plan) => plan,
            Err(err) => {
                if created_worktree {
                    remove_unapplied_worktree(
                        &c.repository.checkout,
                        &dest_repo.checkout,
                        worktree_branch.as_deref(),
                    );
                }
                return Err(err);
            }
        };
        let changes: Vec<Change> = plan
            .files
            .iter()
            .map(|f| Change {
                path: f.path.clone(),
                before_digest: f.before.as_ref().map(|s| digest(s.as_bytes())),
                after_digest: f.after.as_ref().map(|s| digest(s.as_bytes())),
                before_bytes: f.before.as_ref().map_or(0, String::len),
                after_bytes: f.after.as_ref().map_or(0, String::len),
            })
            .collect();
        let plan_digest = digest(&serde_json::to_vec(&plan)?);
        if let Some(mut prev) = existing {
            let old_worktree = (prev.target == IntegrationTarget::Worktree).then(|| {
                (
                    prev.repository.checkout.clone(),
                    prev.worktree_branch.clone(),
                )
            });
            prev.revision += 1;
            prev.target = target;
            prev.worktree_branch = worktree_branch;
            prev.repository = dest_repo;
            prev.evidence_digest = c.evidence_digest;
            prev.plan_digest = plan_digest;
            prev.files = changes;
            prev.error = None;
            prev.events.push(Event {
                at: now(),
                stage: "integration_reprepared".into(),
                handler: "Integrations::prepare_with_target".into(),
                by: by.into(),
            });
            let updated = self.connection.execute(
                "UPDATE source_integrations SET record=?2, plan=?3 WHERE task_id=?1",
                params![
                    task,
                    serde_json::to_string(&prev)?,
                    serde_json::to_string(&plan)?
                ],
            );
            if let Err(err) = updated {
                if created_worktree {
                    remove_unapplied_worktree(
                        &c.repository.checkout,
                        &plan.repository.checkout,
                        plan.worktree_branch.as_deref(),
                    );
                }
                return Err(err.into());
            }
            if let Some((old_checkout, old_branch)) = old_worktree {
                remove_unapplied_worktree(
                    &c.repository.checkout,
                    &old_checkout,
                    old_branch.as_deref(),
                );
            }
            return Ok(prev);
        }
        let record = Record {
            id: uuid::Uuid::new_v4().to_string(),
            task_id: task.into(),
            revision: 1,
            status: "prepared".into(),
            target,
            worktree_branch,
            repository: dest_repo,
            evidence_digest: c.evidence_digest,
            plan_digest,
            files: changes,
            owner: None,
            error: None,
            events: vec![Event {
                at: now(),
                stage: "integration_prepared".into(),
                handler: "Integrations::prepare".into(),
                by: by.into(),
            }],
        };
        let insert = self.connection.execute(
            "INSERT OR IGNORE INTO source_integrations VALUES(?1,?2,?3)",
            params![
                task,
                serde_json::to_string(&record)?,
                serde_json::to_string(&plan)?
            ],
        );
        if let Err(err) = insert {
            if created_worktree {
                remove_unapplied_worktree(
                    &c.repository.checkout,
                    &plan.repository.checkout,
                    plan.worktree_branch.as_deref(),
                );
            }
            return Err(err.into());
        }
        self.get(task)?.context("integration was not prepared")
    }
    pub fn apply(
        &mut self,
        task: &str,
        revision: u64,
        expected_digest: &str,
        by: &str,
    ) -> Result<Record> {
        actor(by)?;
        let r = self.get(task)?.context("prepare integration first")?;
        ensure!(
            r.plan_digest == expected_digest,
            "current integration plan digest required"
        );
        if r.status == "applied" {
            return Ok(r);
        }
        ensure!(
            r.status == "prepared" && r.revision == revision,
            "integration requires current prepared plan; interrupted work never replays"
        );
        let root = self.root.clone();
        let outcome =
            with_repository_lease(&root, r.repository.identity.clone(), r.id.clone(), || {
                crate::stop::GlobalStop::open(&root)?
                    .state()?
                    .ensure_admission_allowed()?;
                let plan = self.plan(task)?;
                let c = candidate(&root, task)?;
                plan_matches_candidate(&c, &plan)?;
                source_clean(&plan.repository)?;
                self.check(&plan, false)?;
                let owner = current_identity()?;
                let mut current = self.change(task, revision, "integration_started", by, |r| {
                    r.status = "applying".into();
                    r.owner = Some(owner);
                    r.error = None;
                })?;
                let lock = match IndexLock::acquire(&plan.index, &current.id) {
                    Ok(lock) => lock,
                    Err(e) => {
                        return self.change(
                            task,
                            current.revision,
                            "integration_blocked",
                            by,
                            |r| {
                                r.status = "prepared".into();
                                r.owner = None;
                                r.error = Some(format!("{e:#}"));
                            },
                        );
                    }
                };
                let result = (|| -> Result<()> {
                    for f in &plan.files {
                        crate::stop::GlobalStop::open(&root)?
                            .state()?
                            .ensure_admission_allowed()?;
                        replace(
                            &plan.repository.checkout,
                            f,
                            &f.before,
                            &f.after,
                            &current.id,
                        )?;
                        current =
                            self.change(task, current.revision, "source_file_applied", by, |_| {})?;
                    }
                    self.check(&plan, true)?;
                    Ok(())
                })();
                let output = match result {
                    Ok(()) => self.change(task, current.revision, "integration_applied", by, |r| {
                        r.status = "applied".into();
                        r.owner = None;
                    }),
                    Err(e) => self.restore(
                        task,
                        &plan,
                        current,
                        by,
                        Some(format!("Integration failed: {e:#}")),
                    ),
                };
                drop(lock);
                output
            });
        self.finish_owner(outcome, by)
    }
    // Recovery must not observe a released owner before the old lock/lease guard ends.
    // Otherwise it could acquire a new lock that the previous guard then removes.
    fn finish_owner(&mut self, outcome: Result<Record>, by: &str) -> Result<Record> {
        let r = outcome?;
        if r.status == "unknown" && r.owner.is_some() {
            self.change(
                &r.task_id,
                r.revision,
                "integration_recovery_required",
                by,
                |r| r.owner = None,
            )
        } else {
            Ok(r)
        }
    }
    fn check(&self, plan: &Plan, after: bool) -> Result<()> {
        plan.repository.revalidate()?;
        ensure!(
            fingerprint(&plan.index)? == plan.index_digest,
            "source Git index changed"
        );
        for f in &plan.files {
            ensure!(
                text(&safe_file(&plan.repository.checkout, &f.path)?)?
                    == if after {
                        f.after.clone()
                    } else {
                        f.before.clone()
                    },
                "source file conflicts with integration plan: {}",
                f.path
            );
        }
        Ok(())
    }
    fn restore(
        &mut self,
        task: &str,
        plan: &Plan,
        mut current: Record,
        by: &str,
        cause: Option<String>,
    ) -> Result<Record> {
        let result = (|| -> Result<()> {
            plan.repository.revalidate()?;
            ensure!(
                fingerprint(&plan.index)? == plan.index_digest,
                "source index changed; preserve it for recovery"
            );
            // Check every file before restoring any; preserve external edits verbatim.
            for f in &plan.files {
                let actual = text(&safe_file(&plan.repository.checkout, &f.path)?)?;
                ensure!(
                    actual == f.before || actual == f.after,
                    "source changed outside integration; refusing rollback: {}",
                    f.path
                );
            }
            for f in plan.files.iter().rev() {
                let actual = text(&safe_file(&plan.repository.checkout, &f.path)?)?;
                if actual != f.before {
                    replace(
                        &plan.repository.checkout,
                        f,
                        &actual,
                        &f.before,
                        &current.id,
                    )?;
                }
                current =
                    self.change(task, current.revision, "source_file_restored", by, |_| {})?;
            }
            self.check(plan, false)?;
            for f in &plan.files {
                cleanup_temp(&plan.repository.checkout, f, &current.id)?;
            }
            Ok(())
        })();
        match result {
            Ok(()) => self.change(task, current.revision, "integration_rolled_back", by, |r| {
                r.status = "rolled_back".into();
                r.owner = None;
                r.error = cause;
            }),
            Err(e) => {
                let r=self.change(task,current.revision,"integration_unknown",by,|r| {r.status="unknown".into();r.error=Some(format!("{}; rollback incomplete: {e:#}; inspect the journal and source before recovery",cause.unwrap_or_default()));})?;
                Coordination::open(&self.root)?.quarantine_task(&r.id)?;
                Ok(r)
            }
        }
    }
    pub fn rollback(
        &mut self,
        task: &str,
        revision: u64,
        expected_digest: &str,
        by: &str,
    ) -> Result<Record> {
        actor(by)?;
        let r = self.get(task)?.context("no integration to restore")?;
        ensure!(
            r.plan_digest == expected_digest,
            "current integration plan digest required"
        );
        if r.status == "rolled_back" {
            return Ok(r);
        }
        ensure!(
            r.revision == revision
                && matches!(
                    r.status.as_str(),
                    "applied" | "applying" | "rolling_back" | "unknown"
                ),
            "integration is not recoverable at this revision"
        );
        if let Some(owner) = &r.owner {
            ensure!(
                !matches!(presence(owner), Presence::Alive | Presence::Unverifiable),
                "integration owner is live/unverifiable; cannot restore"
            );
        }
        let mut coordination = Coordination::open(&self.root)?;
        for lease in coordination.reservations()? {
            if lease["resource"] == r.repository.identity {
                ensure!(
                    lease["task_id"] == r.id,
                    "another task holds the source repository lease"
                );
                coordination.acknowledge(
                    &r.repository.identity,
                    by,
                    "Explicit recovery of interrupted source integration",
                )?;
            }
        }
        let root = self.root.clone();
        let outcome =
            with_repository_lease(&root, r.repository.identity.clone(), r.id.clone(), || {
                let plan = self.plan(task)?;
                let owner = current_identity()?;
                let current =
                    self.change(task, revision, "integration_restore_started", by, |r| {
                        r.status = "rolling_back".into();
                        r.owner = Some(owner);
                    })?;
                let lock = match IndexLock::remove_owned(&plan.index, &r.id)
                    .and_then(|_| IndexLock::acquire(&plan.index, &r.id))
                {
                    Ok(lock) => lock,
                    Err(e) => {
                        let blocked = self.change(
                            task,
                            current.revision,
                            "integration_recovery_blocked",
                            by,
                            |r| {
                                r.status = "unknown".into();
                                r.error = Some(format!("Recovery blocked: {e:#}"));
                            },
                        )?;
                        Coordination::open(&root)?.quarantine_task(&blocked.id)?;
                        return Ok(blocked);
                    }
                };
                let output = self.restore(task, &plan, current, by, None);
                drop(lock);
                output
            });
        self.finish_owner(outcome, by)
    }
}
fn actor(by: &str) -> Result<()> {
    ensure!(
        !by.trim().is_empty() && by.len() <= 128,
        "integration actor label required (up to 128 bytes)"
    );
    Ok(())
}
fn temp(root: &Path, f: &FileChange, id: &str) -> Result<PathBuf> {
    Ok(safe_file(root, &f.path)?
        .parent()
        .context("missing parent")?
        .join(format!(
            ".conductor-integration-{id}-{}.tmp",
            &digest(f.path.as_bytes())[..12]
        )))
}
fn cleanup_temp(root: &Path, f: &FileChange, id: &str) -> Result<()> {
    let p = temp(root, f, id)?;
    if let Some(value) = text(&p)? {
        ensure!(
            Some(&value) == f.before.as_ref() || Some(&value) == f.after.as_ref(),
            "unrecognized integration temporary file; preserve it"
        );
        fs::remove_file(p)?;
    }
    Ok(())
}
fn replace(
    root: &Path,
    f: &FileChange,
    expected: &Option<String>,
    desired: &Option<String>,
    id: &str,
) -> Result<()> {
    let target = safe_file(root, &f.path)?;
    ensure!(
        &text(&target)? == expected,
        "source changed before write: {}",
        f.path
    );
    if let Some(value) = desired {
        let permissions = fs::metadata(&target).ok().map(|m| m.permissions());
        ensure!(
            !permissions.as_ref().is_some_and(|p| p.readonly()),
            "source file is read-only: {}",
            f.path
        );
        let staged = temp(root, f, id)?;
        let mut out = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staged)?;
        out.write_all(value.as_bytes())?;
        out.sync_all()?;
        drop(out);
        if let Some(permissions) = permissions {
            fs::set_permissions(&staged, permissions)?;
        }
        ensure!(
            &text(&safe_file(root, &f.path)?)? == expected,
            "source changed during preparation: {}",
            f.path
        );
        fs::rename(&staged, &target)?;
    } else if target.exists() {
        fs::remove_file(&target)?;
    }
    ensure!(
        &text(&target)? == desired,
        "source write did not match candidate: {}",
        f.path
    );
    Ok(())
}
struct IndexLock {
    path: PathBuf,
    marker: String,
}
impl IndexLock {
    fn path(index: &Path) -> PathBuf {
        index.with_extension("lock")
    }
    fn acquire(index: &Path, id: &str) -> Result<Self> {
        let path = Self::path(index);
        // Keeps its pre-rename spelling on purpose: interrupted-write recovery matches existing
        // marker files byte for byte, so renaming it would strand journals already on disk.
        let marker = format!("dev-conductor-integration:{id}\n");
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .context("Git index is locked; source integration did not write files")?;
        file.write_all(marker.as_bytes())?;
        file.sync_all()?;
        Ok(Self { path, marker })
    }
    fn remove_owned(index: &Path, id: &str) -> Result<()> {
        let path = Self::path(index);
        match fs::symlink_metadata(&path) {
            Ok(meta) => ensure!(
                meta.is_file()
                    && !meta.file_type().is_symlink()
                    && meta.len() == format!("dev-conductor-integration:{id}\n").len() as u64,
                "foreign Git index lock; preserve it"
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        }
        match fs::read(&path) {
            Ok(bytes) => {
                ensure!(
                    bytes == format!("dev-conductor-integration:{id}\n").as_bytes(),
                    "foreign Git index lock; preserve it"
                );
                fs::remove_file(path)?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
        Ok(())
    }
}
impl Drop for IndexLock {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.path).ok().is_some_and(|m| {
            m.is_file() && !m.file_type().is_symlink() && m.len() == self.marker.len() as u64
        }) && fs::read(&self.path).ok().as_deref() == Some(self.marker.as_bytes())
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    const TASK: &str = "fixture-task";
    fn setup() -> (tempfile::TempDir, Integrations, Plan, Record) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let state = dir.path().join("state");
        fs::create_dir(&repo).unwrap();
        fs::create_dir(&state).unwrap();
        for (name, value) in [
            ("a.txt", "old-a\r\n"),
            ("b.txt", "old-b"),
            ("deleted.txt", "restore me"),
        ] {
            fs::write(repo.join(name), value).unwrap();
        }
        git(&repo, &["init", "-b", "main"]).unwrap();
        git(&repo, &["config", "core.autocrlf", "false"]).unwrap();
        git(&repo, &["add", "."]).unwrap();
        git(
            &repo,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "core.hooksPath=",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                "integration fixture",
            ],
        )
        .unwrap();
        let repository = Repository::inspect(&repo).unwrap();
        let index = repository.checkout.join(".git/index");
        let files = vec![
            FileChange {
                path: "a.txt".into(),
                before: Some("old-a\r\n".into()),
                after: Some("new-a\n".into()),
            },
            FileChange {
                path: "b.txt".into(),
                before: Some("old-b".into()),
                after: Some("new-b".into()),
            },
            FileChange {
                path: "created.txt".into(),
                before: None,
                after: Some("new file".into()),
            },
            FileChange {
                path: "deleted.txt".into(),
                before: Some("restore me".into()),
                after: None,
            },
        ];
        let plan = Plan {
            target: IntegrationTarget::Checkout,
            source_checkout: None,
            worktree_branch: None,
            repository: repository.clone(),
            task_revision: 1,
            evidence_digest: "reviewed-fixture".into(),
            index_digest: fingerprint(&index).unwrap(),
            index,
            files,
        };
        let r = Record {
            id: uuid::Uuid::new_v4().to_string(),
            task_id: TASK.into(),
            revision: 1,
            status: "prepared".into(),
            target: IntegrationTarget::Checkout,
            worktree_branch: None,
            repository,
            evidence_digest: plan.evidence_digest.clone(),
            plan_digest: digest(&serde_json::to_vec(&plan).unwrap()),
            files: vec![],
            owner: None,
            error: None,
            events: vec![],
        };
        let store = Integrations::open(&state).unwrap();
        store
            .connection
            .execute(
                "INSERT INTO source_integrations VALUES(?1,?2,?3)",
                params![
                    TASK,
                    serde_json::to_string(&r).unwrap(),
                    serde_json::to_string(&plan).unwrap()
                ],
            )
            .unwrap();
        (dir, store, plan, r)
    }
    #[test]
    fn crash_child() {
        let Some(root) = std::env::var_os("CONDUCTOR_INTEGRATION_CRASH_ROOT") else {
            return;
        };
        let mut store = Integrations::open(Path::new(&root)).unwrap();
        let plan = store.plan(TASK).unwrap();
        let r = store.get(TASK).unwrap().unwrap();
        let owner = current_identity().unwrap();
        Coordination::open(&store.root)
            .unwrap()
            .acquire(&r.repository.identity, &r.id, &owner)
            .unwrap();
        let r = store
            .change(
                TASK,
                r.revision,
                "integration_started",
                "crash fixture",
                |r| {
                    r.status = "applying".into();
                    r.owner = Some(owner);
                },
            )
            .unwrap();
        let _lock = IndexLock::acquire(&plan.index, &r.id).unwrap();
        let f = &plan.files[0];
        replace(&plan.repository.checkout, f, &f.before, &f.after, &r.id).unwrap();
        store
            .change(
                TASK,
                r.revision,
                "source_file_applied",
                "crash fixture",
                |_| {},
            )
            .unwrap();
        std::process::exit(17);
    }
    #[test]
    fn actual_owner_exit_retains_partial_source_and_recovers_without_replaying() {
        let (_dir, mut store, plan, r) = setup();
        let index = fs::read(&plan.index).unwrap();
        let result = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "integration::tests::crash_child", "--nocapture"])
            .env("CONDUCTOR_INTEGRATION_CRASH_ROOT", &store.root)
            .output()
            .unwrap();
        assert_eq!(
            result.status.code(),
            Some(17),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let interrupted = store.get(TASK).unwrap().unwrap();
        assert_eq!(interrupted.status, "applying");
        assert!(IndexLock::path(&plan.index).exists());
        assert_eq!(
            text(&plan.repository.checkout.join("a.txt")).unwrap(),
            plan.files[0].after
        );
        assert!(
            store
                .apply(TASK, interrupted.revision, &r.plan_digest, "human")
                .is_err()
        );
        let restored = store
            .rollback(TASK, interrupted.revision, &r.plan_digest, "human")
            .unwrap();
        assert_eq!(restored.status, "rolled_back");
        store.check(&plan, false).unwrap();
        assert_eq!(fs::read(&plan.index).unwrap(), index);
        assert!(!IndexLock::path(&plan.index).exists());
        assert!(
            Coordination::open(&store.root)
                .unwrap()
                .reservations()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            restored
                .events
                .iter()
                .filter(|e| e.stage == "source_file_applied")
                .count(),
            1
        );
    }
    #[test]
    fn create_delete_crlf_and_unrelated_files_survive_explicit_rollback() {
        let (_dir, mut store, plan, r) = setup();
        let index = fs::read(&plan.index).unwrap();
        for f in &plan.files {
            replace(&plan.repository.checkout, f, &f.before, &f.after, &r.id).unwrap();
        }
        fs::write(plan.repository.checkout.join("personal.txt"), "preserve").unwrap();
        let applied = store
            .change(TASK, r.revision, "integration_applied", "fixture", |r| {
                r.status = "applied".into()
            })
            .unwrap();
        let restored = store
            .rollback(TASK, applied.revision, &r.plan_digest, "human")
            .unwrap();
        assert_eq!(restored.status, "rolled_back");
        store.check(&plan, false).unwrap();
        assert_eq!(
            fs::read(plan.repository.checkout.join("a.txt")).unwrap(),
            b"old-a\r\n"
        );
        assert!(!plan.repository.checkout.join("created.txt").exists());
        assert_eq!(fs::read(&plan.index).unwrap(), index);
        assert_eq!(
            fs::read_to_string(plan.repository.checkout.join("personal.txt")).unwrap(),
            "preserve"
        );
    }
    #[test]
    fn failed_second_write_restores_the_first_and_retains_the_error() {
        let (_dir, mut store, plan, r) = setup();
        let first = &plan.files[0];
        let second = &plan.files[1];
        replace(
            &plan.repository.checkout,
            first,
            &first.before,
            &first.after,
            &r.id,
        )
        .unwrap();
        fs::write(
            temp(&plan.repository.checkout, second, &r.id).unwrap(),
            second.after.as_ref().unwrap(),
        )
        .unwrap();
        let failure = replace(
            &plan.repository.checkout,
            second,
            &second.before,
            &second.after,
            &r.id,
        )
        .unwrap_err();
        let current = store
            .change(TASK, r.revision, "integration_started", "fixture", |r| {
                r.status = "applying".into()
            })
            .unwrap();
        let result = store
            .restore(TASK, &plan, current, "human", Some(format!("{failure:#}")))
            .unwrap();
        assert_eq!(result.status, "rolled_back");
        assert!(result.error.is_some());
        store.check(&plan, false).unwrap();
        assert!(
            !temp(&plan.repository.checkout, second, &r.id)
                .unwrap()
                .exists()
        );
    }
    #[test]
    fn changed_index_and_live_owner_block_recovery_without_overwriting() {
        let (_dir, mut store, plan, r) = setup();
        let owner = current_identity().unwrap();
        let active = store
            .change(TASK, r.revision, "integration_started", "fixture", |r| {
                r.status = "applying".into();
                r.owner = Some(owner);
            })
            .unwrap();
        assert!(
            store
                .rollback(TASK, active.revision, &r.plan_digest, "human")
                .is_err()
        );
        let completed = store
            .change(
                TASK,
                active.revision,
                "integration_applied",
                "fixture",
                |r| {
                    r.status = "applied".into();
                    r.owner = None;
                },
            )
            .unwrap();
        fs::write(
            plan.repository.checkout.join("personal.txt"),
            "staged user content",
        )
        .unwrap();
        git(&plan.repository.checkout, &["add", "personal.txt"]).unwrap();
        let index = fs::read(&plan.index).unwrap();
        let uncertain = store
            .rollback(TASK, completed.revision, &r.plan_digest, "human")
            .unwrap();
        assert_eq!(uncertain.status, "unknown");
        assert_eq!(fs::read(&plan.index).unwrap(), index);
        assert_eq!(
            fs::read_to_string(plan.repository.checkout.join("personal.txt")).unwrap(),
            "staged user content"
        );
        assert_eq!(
            text(&plan.repository.checkout.join("a.txt")).unwrap(),
            plan.files[0].before
        );
    }

    #[test]
    fn failed_restore_keeps_its_owner_until_the_old_guards_have_finished() {
        let (_dir, mut store, plan, r) = setup();
        let owner = current_identity().unwrap();
        Coordination::open(&store.root)
            .unwrap()
            .acquire(&r.repository.identity, &r.id, &owner)
            .unwrap();
        let lock = IndexLock::acquire(&plan.index, &r.id).unwrap();
        let active = store
            .change(TASK, r.revision, "integration_started", "fixture", |r| {
                r.status = "applying".into();
                r.owner = Some(owner);
            })
            .unwrap();
        fs::write(plan.repository.checkout.join("a.txt"), "external edit").unwrap();
        let unknown = store.restore(TASK, &plan, active, "human", None).unwrap();
        assert_eq!(unknown.status, "unknown");
        assert!(unknown.owner.is_some());
        assert!(
            store
                .rollback(TASK, unknown.revision, &r.plan_digest, "human")
                .is_err()
        );
        assert!(IndexLock::path(&plan.index).exists());
        drop(lock);
        Coordination::open(&store.root)
            .unwrap()
            .release(&r.repository.identity, &r.id)
            .unwrap();
        let settled = store.finish_owner(Ok(unknown), "human").unwrap();
        assert!(settled.owner.is_none());
        fs::write(
            plan.repository.checkout.join("a.txt"),
            plan.files[0].before.as_ref().unwrap(),
        )
        .unwrap();
        assert_eq!(
            store
                .rollback(TASK, settled.revision, &r.plan_digest, "human")
                .unwrap()
                .status,
            "rolled_back"
        );
    }

    #[test]
    fn large_text_files_up_to_four_mib_apply_and_rollback_and_reject_above_four_mib() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let state = dir.path().join("state");
        fs::create_dir(&repo).unwrap();
        fs::create_dir(&state).unwrap();

        let original_large = "before-line\n".repeat(8000); // ~96 KiB (> 64 KiB)
        let updated_large = "after-line-\n".repeat(9000); // ~108 KiB (> 64 KiB)
        assert!(original_large.len() > 65_536);
        assert!(updated_large.len() > 65_536);

        fs::write(repo.join("large.rs"), &original_large).unwrap();
        git(&repo, &["init", "-b", "main"]).unwrap();
        git(&repo, &["config", "core.autocrlf", "false"]).unwrap();
        git(&repo, &["add", "."]).unwrap();
        git(
            &repo,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "core.hooksPath=",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                "large file fixture",
            ],
        )
        .unwrap();

        let oversized = dir.path().join("too_big.rs");
        fs::write(&oversized, vec![b'x'; 4 * 1024 * 1024 + 1]).unwrap();
        let err = text(&oversized).unwrap_err();
        assert!(err.to_string().contains("4 MiB"), "{err:#}");
        assert_eq!(
            text(&repo.join("large.rs")).unwrap().as_deref(),
            Some(original_large.as_str())
        );

        let repository = Repository::inspect(&repo).unwrap();
        let index = repository.checkout.join(".git/index");
        let plan = Plan {
            target: IntegrationTarget::Checkout,
            source_checkout: None,
            worktree_branch: None,
            repository: repository.clone(),
            task_revision: 1,
            evidence_digest: "reviewed-large-fixture".into(),
            index_digest: fingerprint(&index).unwrap(),
            index,
            files: vec![FileChange {
                path: "large.rs".into(),
                before: Some(original_large.clone()),
                after: Some(updated_large.clone()),
            }],
        };
        let plan_digest = digest(&serde_json::to_vec(&plan).unwrap());
        let record = Record {
            id: uuid::Uuid::new_v4().to_string(),
            task_id: TASK.into(),
            revision: 1,
            status: "prepared".into(),
            target: IntegrationTarget::Checkout,
            worktree_branch: None,
            repository,
            evidence_digest: plan.evidence_digest.clone(),
            plan_digest: plan_digest.clone(),
            files: vec![],
            owner: None,
            error: None,
            events: vec![],
        };
        let mut store = Integrations::open(&state).unwrap();
        store
            .connection
            .execute(
                "INSERT INTO source_integrations VALUES(?1,?2,?3)",
                params![
                    TASK,
                    serde_json::to_string(&record).unwrap(),
                    serde_json::to_string(&plan).unwrap()
                ],
            )
            .unwrap();

        let f = &plan.files[0];
        replace(
            &plan.repository.checkout,
            f,
            &f.before,
            &f.after,
            &record.id,
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(repo.join("large.rs")).unwrap(),
            updated_large
        );
        let rolled = store.restore(TASK, &plan, record, "human", None).unwrap();
        assert_eq!(rolled.status, "rolled_back");
        assert_eq!(
            fs::read_to_string(repo.join("large.rs")).unwrap(),
            original_large
        );
    }
}
