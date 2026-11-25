use crate::{
    codex::CodexPolicy,
    context::{ContextPacket, prepare},
    digest, now,
    process::{ExecutionPolicy, Presence, ProcessIdentity, presence},
    repository::{Repository, canonical},
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    AwaitingApproval,
    Approved,
    PreparingWorkspace,
    Running,
    Verifying,
    Cancelled,
    TimedOut,
    AwaitingReview,
    Failed,
    Unknown,
    Accepted,
    NeedsWork,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum FakeOutcome {
    Success,
    FailBeforeWrite,
    FailAfterWrite,
    Hang,
    SpawnDescendant,
    OutputFlood,
    WrongOutput,
    VerificationFails,
    VerificationHang,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Assignment {
    pub id: String,
    pub repository_id: String,
    pub repository: Repository,
    pub workspace: PathBuf,
    pub objective: String,
    pub acceptance: String,
    pub context: ContextPacket,
    pub backend: String,
    pub output_file: String,
    pub contents: String,
    pub fake_outcome: FakeOutcome,
    pub allowed_actions: Vec<String>,
    pub verification: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_policy: Option<ExecutionPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex: Option<CodexPolicy>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::git;

    #[test]
    fn review_comment_anchor_text_parses_ranges_and_colons() {
        let single =
            ReviewComment::from_anchor_text("src/lib.rs:12:Handle the empty case").unwrap();
        assert_eq!(
            (single.path.as_str(), single.line, single.end_line),
            ("src/lib.rs", 12, None)
        );
        assert_eq!(single.body, "Handle the empty case");

        let range = ReviewComment::from_anchor_text("a.txt:3-7: rename: keep it short").unwrap();
        assert_eq!((range.line, range.end_line), (3, Some(7)));
        assert_eq!(range.body, "rename: keep it short");

        let colon_path = ReviewComment::from_anchor_text("docs/a:b.md:2:typo").unwrap();
        assert_eq!(colon_path.path, "docs/a:b.md");

        for bad in ["no anchor at all", "a.txt:x:text", ":4:text", "a.txt:4:   "] {
            assert!(
                ReviewComment::from_anchor_text(bad).is_err(),
                "accepted {bad:?}"
            );
        }
    }

    #[test]
    fn restart_reconciliation_preserves_uncertain_attempt_without_replay() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-b", "main"]).unwrap();
        std::fs::write(repo.join("README.md"), "Fixture context").unwrap();
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
                "fixture",
            ],
        )
        .unwrap();
        let state = temp.path().join("state");
        let mut store = Store::open(&state).unwrap();
        let (repo_id, _) = store.register(&repo).unwrap();
        let task = store
            .create(
                &repo_id,
                "fixture".into(),
                "fixture file".into(),
                "result".into(),
                16384,
                ExecutionPolicy::new(&std::env::current_exe().unwrap()).unwrap(),
            )
            .unwrap();
        let approved = store
            .approve(
                &task.id,
                task.revision,
                &task.assignment.digest().unwrap(),
                "Fixture Human",
            )
            .unwrap();
        let workspace = store.root.join("workspaces/interrupted");
        let interrupted = store
            .mutate(&task.id, approved.revision, "workspace_intent", |task| {
                task.status = Status::PreparingWorkspace;
                task.attempts.push(Attempt {
                    id: "fixture-attempt".into(),
                    assignment_digest: task.assignment.digest()?,
                    workspace: workspace.clone(),
                    started_at: now(),
                    ended_at: None,
                    execution: "not_started".into(),
                    error: None,
                    diff: None,
                    output: None,
                    receipt: None,
                    supervisor: None,
                    process: None,
                    verification_process: None,
                    process_receipt: None,
                    cancel_requested: None,
                });
                Ok(())
            })
            .unwrap();
        drop(store);
        let mut reopened = Store::open(&state).unwrap();
        assert_eq!(
            reopened.task(&task.id).unwrap().status,
            Status::PreparingWorkspace
        );
        assert!(reopened.run_fake(&task.id, interrupted.revision).is_err());
        let unknown = reopened.reconcile(&task.id, interrupted.revision).unwrap();
        assert_eq!(unknown.status, Status::Unknown);
        assert_eq!(unknown.attempts.len(), 1);
        assert_eq!(unknown.attempts[0].execution, "unknown");
        assert!(!workspace.exists());
        assert!(reopened.run_fake(&task.id, unknown.revision).is_err());
        assert!(
            reopened
                .revise(
                    &task.id,
                    unknown.revision,
                    "retry".into(),
                    FakeOutcome::Success
                )
                .is_err()
        );
    }
}

impl Assignment {
    pub fn digest(&self) -> Result<String> {
        Ok(digest(&serde_json::to_vec(self)?))
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.objective.trim().is_empty() && !self.acceptance.trim().is_empty(),
            "objective and acceptance criteria are required"
        );
        let path = Path::new(&self.output_file);
        ensure!(
            path.components().count() == 1
                && matches!(path.components().next(), Some(Component::Normal(_))),
            "fake output must be one root file"
        );
        let lower = self.output_file.to_lowercase();
        let stem = lower.split('.').next().unwrap_or("");
        let reserved = [
            "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7",
            "com8", "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
        ];
        ensure!(
            !self
                .output_file
                .contains(['/', '\\', ':', '\0', '<', '>', '|', '?', '*'])
                && !lower.starts_with('.')
                && !lower.ends_with(['.', ' '])
                && !reserved.contains(&stem),
            "unsafe fake output filename"
        );
        ensure!(self.contents.len() <= 65536, "fake output exceeds 64 KiB");
        ensure!(
            matches!(self.backend.as_str(), "fake-process-v1" | "codex-exec-v1"),
            "legacy or unsupported backend; prepare a new process assignment"
        );
        self.execution_policy
            .as_ref()
            .context("missing approved process policy")?
            .validate()?;
        if self.backend == "codex-exec-v1" {
            let codex = self.codex.as_ref().context("missing Codex policy")?;
            codex.validate()?;
            ensure!(
                self.fake_outcome == FakeOutcome::Success,
                "fixture outcomes cannot alter Codex execution"
            );
            ensure!(
                !codex.home.starts_with(&self.repository.checkout)
                    && !codex.home.starts_with(&self.repository.common_dir),
                "Codex home must be outside the target checkout/Git directory"
            );
            crate::codex::prompt(self)?;
        } else {
            ensure!(
                self.codex.is_none(),
                "fake assignment contains an unexpected provider policy"
            );
            let policy = self.execution_policy.as_ref().unwrap();
            ensure!(
                policy.timeout_ms <= 60000 && policy.verification_timeout_ms <= 60000,
                "fixture timeouts cannot exceed 60000 milliseconds"
            );
        }
        ensure!(
            self.context.usable,
            "context is not usable; inspect outcomes or increase budget"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Approval {
    pub assignment_digest: String,
    pub approver: String,
    pub at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artifact {
    pub path: PathBuf,
    pub hash: String,
    pub bytes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationReceipt {
    pub attempt_id: String,
    pub assignment_digest: String,
    pub workspace: PathBuf,
    pub base: String,
    pub output_hash: String,
    pub check: String,
    pub result: String,
    pub started_at: u64,
    pub ended_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_receipt: Option<ProcessReceipt>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogArtifact {
    pub artifact: Artifact,
    pub total_bytes: u64,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessReceipt {
    pub identity: ProcessIdentity,
    pub command: Vec<String>,
    pub cwd: PathBuf,
    pub started_at: u64,
    pub ended_at: u64,
    pub exit_code: Option<i32>,
    pub outcome: String,
    pub stdout: LogArtifact,
    pub stderr: LogArtifact,
    pub tree_terminated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegated: Option<crate::launcher::DelegatedReceipt>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelRequest {
    pub by: String,
    pub at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attempt {
    pub id: String,
    pub assignment_digest: String,
    pub workspace: PathBuf,
    pub started_at: u64,
    pub ended_at: Option<u64>,
    pub execution: String,
    pub error: Option<String>,
    pub diff: Option<Artifact>,
    pub output: Option<Artifact>,
    pub receipt: Option<VerificationReceipt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supervisor: Option<ProcessIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<ProcessIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification_process: Option<ProcessIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_receipt: Option<ProcessReceipt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel_requested: Option<CancelRequest>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Review {
    pub reviewer: String,
    pub accepted: bool,
    pub evidence_digest: String,
    pub at: u64,
    /// Line-anchored reviewer notes on the reviewed candidate. A retry of a rejected
    /// workflow hands them to the next repair round.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub comments: Vec<ReviewComment>,
}

/// A reviewer note anchored to lines of the candidate a review decided on.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewComment {
    /// Repository-relative path of a file the candidate changed.
    pub path: String,
    /// First anchored line (1-based) in the candidate's version of the file.
    pub line: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u32>,
    pub body: String,
    /// The anchored candidate lines, captured by the host when the review was recorded.
    #[serde(default)]
    pub excerpt: String,
}

impl ReviewComment {
    /// Parses `PATH:LINE[-END]:TEXT`. The anchor is the first `:LINE:` or `:LINE-END:`
    /// segment, so paths containing `:` still work.
    pub fn from_anchor_text(text: &str) -> Result<Self> {
        for (at, _) in text.match_indices(':') {
            let rest = &text[at + 1..];
            let Some(close) = rest.find(':') else {
                break;
            };
            let (first, end) = match rest[..close].split_once('-') {
                Some((a, b)) => (a, Some(b)),
                None => (&rest[..close], None),
            };
            let (Ok(line), Ok(end_line)) =
                (first.parse::<u32>(), end.map(str::parse::<u32>).transpose())
            else {
                continue;
            };
            let path = text[..at].trim();
            let body = rest[close + 1..].trim();
            ensure!(
                !path.is_empty() && !body.is_empty(),
                "review comment needs a path and text: {text}"
            );
            return Ok(Self {
                path: path.to_string(),
                line,
                end_line,
                body: body.to_string(),
                excerpt: String::new(),
            });
        }
        anyhow::bail!("review comment must look like PATH:LINE[-END]:TEXT, got {text:?}")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub revision: u64,
    pub status: Status,
    pub assignment: Assignment,
    pub approval: Option<Approval>,
    pub attempts: Vec<Attempt>,
    pub review: Option<Review>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Event {
    pub sequence: u64,
    pub at: u64,
    pub kind: String,
    pub task: Task,
}

pub struct Store {
    pub(crate) connection: Connection,
    pub root: PathBuf,
}

pub struct CodexRequest {
    pub repository_id: String,
    pub objective: String,
    pub acceptance: String,
    pub output_file: String,
    pub contents: String,
    pub budget: usize,
    pub execution: ExecutionPolicy,
    pub policy: CodexPolicy,
}

impl Store {
    pub fn open(root: &Path) -> Result<Self> {
        std::fs::create_dir_all(root)?;
        let root = canonical(root)?;
        let connection = crate::open_sqlite_with_retry(&root.join("state.sqlite"), 1)?;
        crate::ensure_table_schema(
            &connection,
            "events",
            "CREATE TABLE IF NOT EXISTS repositories(id TEXT PRIMARY KEY, checkout TEXT NOT NULL UNIQUE, record TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS tasks(id TEXT PRIMARY KEY, revision INTEGER NOT NULL, record TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS events(task_id TEXT NOT NULL REFERENCES tasks(id), sequence INTEGER NOT NULL, at INTEGER NOT NULL, kind TEXT NOT NULL, record TEXT NOT NULL, PRIMARY KEY(task_id, sequence));
            PRAGMA user_version=1;",
        )?;
        let version: u32 = connection.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version == 0 {
            connection.execute_batch("PRAGMA user_version=1;")?;
        }
        Ok(Self { connection, root })
    }

    pub fn register(&mut self, path: &Path) -> Result<(String, Repository)> {
        let repo = Repository::inspect(path)?;
        ensure!(
            !self.root.starts_with(&repo.checkout) && !self.root.starts_with(&repo.common_dir),
            "state directory must be outside the registered checkout and Git directory"
        );
        let checkout = repo
            .checkout
            .to_str()
            .context("checkout path must be UTF-8")?;
        let existing: Option<(String, String)> = self
            .connection
            .query_row(
                "SELECT id, record FROM repositories WHERE checkout=?1",
                [checkout],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((id, record)) = existing {
            ensure!(
                serde_json::from_str::<Repository>(&record)? == repo,
                "registered repository changed; create a separate state directory or explicitly resolve registration drift"
            );
            return Ok((id, repo));
        }
        let id = Uuid::new_v4().to_string();
        self.connection.execute(
            "INSERT INTO repositories VALUES (?1, ?2, ?3)",
            params![id, checkout, serde_json::to_string(&repo)?],
        )?;
        Ok((id, repo))
    }

    pub fn repository(&self, id: &str) -> Result<Repository> {
        let record: String = self
            .connection
            .query_row("SELECT record FROM repositories WHERE id=?1", [id], |row| {
                row.get(0)
            })
            .context("unknown repository registration")?;
        Ok(serde_json::from_str(&record)?)
    }

    /// Every registration this state root has ever recorded, oldest first. Added for the
    /// M11 MCP adapter's `conductor_list_repositories` tool - no prior caller needed a full
    /// listing, only lookup by id (`repository`) or by checkout path (`register`'s own
    /// dedup query above).
    pub fn repositories(&self) -> Result<Vec<(String, Repository)>> {
        let mut statement = self
            .connection
            .prepare("SELECT id, record FROM repositories ORDER BY id")?;
        let rows = statement.query_map([], |row| {
            let id: String = row.get(0)?;
            let record: String = row.get(1)?;
            Ok((id, record))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, record) = row?;
            out.push((id, serde_json::from_str(&record)?));
        }
        Ok(out)
    }

    fn draft_assignment(
        &self,
        repository_id: &str,
        objective: String,
        acceptance: String,
        contents: String,
        budget: usize,
        execution_policy: ExecutionPolicy,
    ) -> Result<Assignment> {
        let repo = self.repository(repository_id)?;
        let context = prepare(&repo, &objective, budget)?;
        let assignment_id = Uuid::new_v4().to_string();
        let assignment = Assignment {
            workspace: self.root.join("workspaces").join(&assignment_id),
            id: assignment_id,
            repository_id: repository_id.into(),
            repository: repo,
            objective,
            acceptance,
            context,
            backend: "fake-process-v1".into(),
            output_file: "conductor-result.md".into(),
            contents,
            fake_outcome: FakeOutcome::Success,
            allowed_actions: vec![
                "create one specified root file in an owned worktree".into(),
                "launch owned fixture worker/verifier processes (including fixture descendants); retain control and log artifacts under state root".into(),
            ],
            verification: "independent fixture verifier command and host output hash; not a project test suite".into(),
            execution_policy: Some(execution_policy),
            codex: None,
        };
        Ok(assignment)
    }

    pub fn create(
        &mut self,
        repository_id: &str,
        objective: String,
        acceptance: String,
        contents: String,
        budget: usize,
        execution_policy: ExecutionPolicy,
    ) -> Result<Task> {
        let assignment = self.draft_assignment(
            repository_id,
            objective,
            acceptance,
            contents,
            budget,
            execution_policy,
        )?;
        self.insert_assignment(assignment)
    }

    fn insert_assignment(&mut self, assignment: Assignment) -> Result<Task> {
        assignment.validate()?;
        let task = Task {
            id: Uuid::new_v4().to_string(),
            revision: 1,
            status: Status::AwaitingApproval,
            assignment,
            approval: None,
            attempts: Vec::new(),
            review: None,
        };
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let record = serde_json::to_string(&task)?;
        tx.execute(
            "INSERT INTO tasks VALUES (?1, ?2, ?3)",
            params![task.id, task.revision, record],
        )?;
        tx.execute(
            "INSERT INTO events VALUES (?1, ?2, ?3, 'created', ?4)",
            params![task.id, task.revision, now(), record],
        )?;
        tx.commit()?;
        Ok(task)
    }

    pub fn create_codex(&mut self, request: CodexRequest) -> Result<Task> {
        request.policy.validate()?;
        let mut assignment = self.draft_assignment(
            &request.repository_id,
            request.objective,
            request.acceptance,
            request.contents,
            request.budget,
            request.execution,
        )?;
        assignment.backend = "codex-exec-v1".into();
        assignment.output_file = request.output_file;
        assignment.codex = Some(request.policy);
        assignment.allowed_actions = vec![
                "create the specified new root file; other tracked/untracked changes are rejected".into(),
                "invoke pinned Codex with explicitly selected home/model, ephemeral JSONL and workspace-write/MXC requested; no daemon, user config, delegation or configured MCP".into(),
                "run the separately approved pinned verification command in the owned worktree; retain bounded logs/control artifacts under state root".into(),
            ];
        assignment.verification =
            "approved verification command, unchanged diff and exact approved output bytes".into();
        self.insert_assignment(assignment)
    }

    pub fn task(&self, id: &str) -> Result<Task> {
        let record: String = self
            .connection
            .query_row("SELECT record FROM tasks WHERE id=?1", [id], |row| {
                row.get(0)
            })
            .context("unknown task")?;
        Ok(serde_json::from_str(&record)?)
    }

    pub fn tasks(&self) -> Result<Vec<Task>> {
        let mut query = self
            .connection
            .prepare("SELECT record FROM tasks ORDER BY rowid")?;
        query
            .query_map([], |row| row.get::<_, String>(0))?
            .map(|record| Ok(serde_json::from_str(&record?)?))
            .collect()
    }

    pub fn events(&self, id: &str) -> Result<Vec<Event>> {
        let mut query = self.connection.prepare(
            "SELECT sequence, at, kind, record FROM events WHERE task_id=?1 ORDER BY sequence",
        )?;
        let rows = query.query_map([id], |row| {
            Ok((
                row.get::<_, u64>(0)?,
                row.get::<_, u64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        rows.map(|row| {
            let (sequence, at, kind, record) = row?;
            Ok(Event {
                sequence,
                at,
                kind,
                task: serde_json::from_str(&record)?,
            })
        })
        .collect()
    }

    pub(crate) fn mutate<F>(
        &mut self,
        id: &str,
        expected: u64,
        kind: &str,
        change: F,
    ) -> Result<Task>
    where
        F: FnOnce(&mut Task) -> Result<()>,
    {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let record: String = tx.query_row("SELECT record FROM tasks WHERE id=?1", [id], |row| {
            row.get(0)
        })?;
        let mut task: Task = serde_json::from_str(&record)?;
        ensure!(
            task.revision == expected,
            "revision conflict: expected {expected}, current {}",
            task.revision
        );
        change(&mut task)?;
        task.revision += 1;
        let record = serde_json::to_string(&task)?;
        ensure!(
            tx.execute(
                "UPDATE tasks SET revision=?1, record=?2 WHERE id=?3 AND revision=?4",
                params![task.revision, record, id, expected]
            )? == 1,
            "revision conflict"
        );
        tx.execute(
            "INSERT INTO events VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, task.revision, now(), kind, record],
        )?;
        tx.commit()?;
        Ok(task)
    }

    pub fn revise(
        &mut self,
        id: &str,
        expected: u64,
        contents: String,
        outcome: FakeOutcome,
    ) -> Result<Task> {
        let parent = self.root.join("workspaces");
        self.mutate(id, expected, "assignment_revised", |task| {
            ensure!(
                matches!(
                    task.status,
                    Status::AwaitingApproval
                        | Status::Approved
                        | Status::Failed
                        | Status::NeedsWork
                        | Status::Cancelled
                        | Status::TimedOut
                ),
                "task cannot be revised in this state"
            );
            task.assignment.id = Uuid::new_v4().to_string();
            task.assignment.workspace = parent.join(&task.assignment.id);
            task.assignment.contents = contents;
            task.assignment.fake_outcome = outcome;
            task.assignment.validate()?;
            task.approval = None;
            task.review = None;
            task.status = Status::AwaitingApproval;
            Ok(())
        })
    }

    pub fn approve(
        &mut self,
        id: &str,
        expected: u64,
        reviewed_digest: &str,
        approver: &str,
    ) -> Result<Task> {
        let task = self.task(id)?;
        task.assignment.repository.revalidate()?;
        task.assignment
            .context
            .revalidate(&task.assignment.repository)?;
        self.mutate(id, expected, "approved", |task| {
            ensure!(
                task.status == Status::AwaitingApproval,
                "task is not awaiting approval"
            );
            ensure!(!approver.trim().is_empty(), "approver is required");
            ensure!(
                task.assignment.digest()? == reviewed_digest,
                "assignment digest does not match reviewed assignment"
            );
            task.approval = Some(Approval {
                assignment_digest: reviewed_digest.into(),
                approver: approver.into(),
                at: now(),
            });
            task.status = Status::Approved;
            Ok(())
        })
    }

    pub fn retry(&mut self, id: &str, expected: u64, by: &str) -> Result<Task> {
        crate::stop::GlobalStop::open(&self.root)?
            .state()?
            .ensure_admission_allowed()?;
        ensure!(!by.trim().is_empty(), "operator label required");
        let task = self.task(id)?;
        task.assignment.repository.revalidate()?;
        task.assignment
            .context
            .revalidate(&task.assignment.repository)?;
        let parent = self.root.join("workspaces");
        self.mutate(id, expected, "task_retried", |task| {
            ensure!(
                matches!(
                    task.status,
                    Status::Failed
                        | Status::NeedsWork
                        | Status::Cancelled
                        | Status::TimedOut
                        | Status::Unknown
                ),
                "only failed, timed-out, cancelled, or unknown tasks can be retried"
            );
            task.assignment.id = Uuid::new_v4().to_string();
            task.assignment.workspace = parent.join(&task.assignment.id);
            task.assignment.validate()?;
            let digest = task.assignment.digest()?;
            task.review = None;
            task.approval = Some(Approval {
                assignment_digest: digest,
                approver: by.into(),
                at: now(),
            });
            task.status = Status::Approved;
            Ok(())
        })
    }

    pub fn reconcile(&mut self, id: &str, expected: u64) -> Result<Task> {
        self.mutate(id, expected, "recovery_unknown", |task| {
            ensure!(matches!(task.status, Status::PreparingWorkspace | Status::Running | Status::Verifying), "no interrupted attempt to reconcile");
            let attempt = task.attempts.last().context("missing attempt")?;
            if let Some(owner) = &attempt.supervisor {
                ensure!(!matches!(presence(owner), Presence::Alive | Presence::Unverifiable), "supervisor is alive or cannot be inspected; reconciliation cannot stop it");
            }
            task.status = Status::Unknown;
            let attempt = task.attempts.last_mut().context("missing attempt")?;
            if attempt.process_receipt.is_none() {
                attempt.execution = "unknown".into();
            }
            attempt.error = Some(format!("interrupted attempt; main process {:?}, verification process {:?}; inspect preserved workspace/logs; never redispatched", attempt.process.as_ref().map(presence), attempt.verification_process.as_ref().map(presence)));
            Ok(())
        })
    }

    /// Revision-tolerant: see [`crate::cancel_revision`]. A stale `expected` is accepted
    /// only while the task is still on the attempt and assignment the caller saw.
    pub fn cancel(&mut self, id: &str, expected: u64, by: &str) -> Result<Task> {
        const HISTORY: crate::CancelHistory = crate::CancelHistory {
            table: "events",
            id_column: "task_id",
            revision_column: "sequence",
            kind_column: "kind",
            boundaries: &["approved", "task_retried", "assignment_revised"],
        };
        let mut last = None;
        for _ in 0..crate::CANCEL_RACE_ATTEMPTS {
            let current: u64 =
                self.connection
                    .query_row("SELECT revision FROM tasks WHERE id=?1", [id], |r| r.get(0))?;
            let revision =
                crate::cancel_revision(&self.connection, &HISTORY, id, expected, current)?;
            match self.cancel_at(id, revision, by) {
                Err(error) if revision != expected && crate::is_revision_conflict(&error) => {
                    last = Some(error);
                }
                outcome => return outcome,
            }
        }
        Err(last.expect("cancel attempted at least once"))
    }

    fn cancel_at(&mut self, id: &str, expected: u64, by: &str) -> Result<Task> {
        self.mutate(id, expected, "cancel_requested", |task| {
            ensure!(
                matches!(
                    task.status,
                    Status::PreparingWorkspace | Status::Running | Status::Verifying
                ),
                "no active attempt to cancel"
            );
            ensure!(!by.trim().is_empty(), "cancellation requester is required");
            let attempt = task.attempts.last_mut().context("missing attempt")?;
            ensure!(
                presence(
                    attempt
                        .supervisor
                        .as_ref()
                        .context("no proven supervisor identity; reconcile instead")?
                ) == Presence::Alive,
                "supervisor identity is not proven alive"
            );
            let active = attempt
                .verification_process
                .as_ref()
                .or(attempt.process.as_ref());
            if let Some(process) = active {
                ensure!(
                    matches!(
                        presence(process),
                        Presence::Alive | Presence::Exited | Presence::Missing
                    ),
                    "process identity is unproven; decline unsafe cancellation"
                );
            }
            ensure!(
                attempt.cancel_requested.is_none(),
                "cancellation already requested"
            );
            attempt.cancel_requested = Some(CancelRequest {
                by: by.into(),
                at: now(),
            });
            Ok(())
        })
    }
}
