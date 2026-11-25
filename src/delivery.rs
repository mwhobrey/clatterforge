//! Explicit delivery of an accepted candidate to a Git branch, and optionally a push and a
//! pull request (#19).
//!
//! Every step is a separate operator action and none is ever automatic: no grant, schedule
//! or MCP tool reaches this module. `commit` is bound to the accepted evidence digest and
//! re-derives the candidate through the same revalidation integration uses. It builds the
//! commit with Git plumbing in a private index (`read-tree` of the recorded base, one blob
//! per candidate file, `write-tree`, `commit-tree`), so no working tree, index or checked-out
//! branch is touched, and creates the branch create-only. `push` and `open_pr` act on that
//! recorded commit only, and only when the branch still points at it.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::{
    integration::{actor, candidate},
    now, open_sqlite_with_retry,
    process::bounded_output,
    repository::{git, git_text},
};

const MAX_FILES: usize = 64;
const NETWORK_TIMEOUT_MS: u64 = 120_000;
const ZERO_OID: &str = "0000000000000000000000000000000000000000";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeliveryEvent {
    pub at: u64,
    pub stage: String,
    pub by: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Delivery {
    pub task_id: String,
    /// `committed`, `pushed` or `pr_opened`.
    pub status: String,
    pub repository_identity: String,
    pub checkout: PathBuf,
    pub base: String,
    pub evidence_digest: String,
    pub branch: String,
    pub tree: String,
    pub commit: String,
    pub message: String,
    pub files: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
    pub events: Vec<DeliveryEvent>,
}

pub struct Deliveries {
    connection: Connection,
    root: PathBuf,
}

/// Git with a private index file and a caller-chosen timeout.
fn git_with(repo: &Path, index: Option<&Path>, args: &[&str], timeout_ms: u64) -> Result<Vec<u8>> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0");
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }
    let output =
        bounded_output(&mut command, timeout_ms, 4 * 1024 * 1024).context("could not run git")?;
    ensure!(
        output.exit_code == Some(0),
        "git {} failed: {}",
        args.first().unwrap_or(&""),
        String::from_utf8_lossy(&output.stderr.bytes).trim()
    );
    Ok(output.stdout.bytes)
}

fn trimmed(bytes: Vec<u8>) -> Result<String> {
    Ok(String::from_utf8(bytes)?.trim().to_string())
}

/// `clatterforge/<first 8 of the task id>` unless the operator names one.
pub fn default_branch(task: &str) -> String {
    let short: String = task
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(8)
        .collect();
    format!("clatterforge/{short}")
}

impl Deliveries {
    pub fn open(root: &Path) -> Result<Self> {
        let root = crate::repository::canonical(root)?;
        let connection = open_sqlite_with_retry(&root.join("state.sqlite"), 1)?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS source_deliveries(
                task_id TEXT PRIMARY KEY, record TEXT NOT NULL)",
        )?;
        Ok(Self { connection, root })
    }

    pub fn get(&self, task: &str) -> Result<Option<Delivery>> {
        let record: Option<String> = self
            .connection
            .query_row(
                "SELECT record FROM source_deliveries WHERE task_id=?1",
                [task],
                |r| r.get(0),
            )
            .optional()?;
        record.map(|r| Ok(serde_json::from_str(&r)?)).transpose()
    }

    fn save(&self, delivery: &Delivery) -> Result<()> {
        self.connection.execute(
            "INSERT OR REPLACE INTO source_deliveries(task_id, record) VALUES (?1, ?2)",
            params![delivery.task_id, serde_json::to_string(delivery)?],
        )?;
        Ok(())
    }

    /// Commits exactly the accepted candidate onto a new branch from the recorded base.
    /// `expected_evidence` must equal the evidence digest the task was accepted on.
    pub fn commit(
        &mut self,
        task: &str,
        expected_evidence: &str,
        branch: Option<&str>,
        message: Option<&str>,
        by: &str,
    ) -> Result<Delivery> {
        actor(by)?;
        if let Some(existing) = self.get(task)? {
            bail!(
                "task {task} was already delivered to branch {} at {}",
                existing.branch,
                existing.commit
            );
        }
        let c = candidate(&self.root, task)?;
        ensure!(
            c.evidence_digest == expected_evidence,
            "delivery must name the accepted evidence digest"
        );
        ensure!(
            !c.files.is_empty() && c.files.len() <= MAX_FILES,
            "delivery requires 1-{MAX_FILES} changed files"
        );
        let repo = &c.repository;
        let branch = branch
            .map(str::trim)
            .filter(|b| !b.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| default_branch(task));
        let checked = git_text(&repo.checkout, &["check-ref-format", "--branch", &branch])
            .with_context(|| format!("{branch:?} is not a valid branch name"))?;
        ensure!(checked == branch, "{branch:?} is not a plain branch name");
        let reference = format!("refs/heads/{branch}");
        ensure!(
            git(
                &repo.checkout,
                &["rev-parse", "--verify", "--quiet", &reference]
            )
            .is_err(),
            "branch {branch} already exists; name another with --branch"
        );

        let scratch = self
            .root
            .join("delivery")
            .join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(&scratch)?;
        let built = self.build_commit(&c, task, &scratch, message);
        let _ = std::fs::remove_dir_all(&scratch);
        let (tree, commit, message) = built?;
        git(
            &repo.checkout,
            &[
                "update-ref",
                "-m",
                "clatterforge delivery",
                &reference,
                &commit,
                ZERO_OID,
            ],
        )
        .with_context(|| format!("could not create branch {branch}"))?;

        let delivery = Delivery {
            task_id: task.to_string(),
            status: "committed".into(),
            repository_identity: repo.identity.clone(),
            checkout: repo.checkout.clone(),
            base: repo.base.clone(),
            evidence_digest: c.evidence_digest.clone(),
            branch,
            tree,
            commit,
            message,
            files: c.files.keys().cloned().collect(),
            remote: None,
            pr_url: None,
            events: vec![DeliveryEvent {
                at: now(),
                stage: "delivery_committed".into(),
                by: by.into(),
                detail: None,
            }],
        };
        self.save(&delivery)?;
        Ok(delivery)
    }

    fn build_commit(
        &self,
        c: &crate::integration::Candidate,
        task: &str,
        scratch: &Path,
        message: Option<&str>,
    ) -> Result<(String, String, String)> {
        let repo = &c.repository;
        let index = scratch.join("index");
        git_with(
            &repo.checkout,
            Some(&index),
            &["read-tree", &repo.base],
            30_000,
        )?;
        for (n, (path, content)) in c.files.iter().enumerate() {
            match content {
                Some(text) => {
                    let staged = scratch.join(format!("blob-{n}"));
                    std::fs::write(&staged, text.as_bytes())?;
                    let staged_str = staged.to_str().context("UTF-8 scratch path required")?;
                    // --path applies the repository's attributes exactly as `git add` would.
                    let blob = trimmed(git(
                        &repo.checkout,
                        &["hash-object", "-w", &format!("--path={path}"), staged_str],
                    )?)?;
                    let entry = git(&repo.checkout, &["ls-tree", &repo.base, "--", path])?;
                    let mode = if String::from_utf8_lossy(&entry).starts_with("100755 ") {
                        "100755"
                    } else {
                        "100644"
                    };
                    git_with(
                        &repo.checkout,
                        Some(&index),
                        &[
                            "update-index",
                            "--add",
                            "--cacheinfo",
                            &format!("{mode},{blob},{path}"),
                        ],
                        30_000,
                    )?;
                }
                None => {
                    git_with(
                        &repo.checkout,
                        Some(&index),
                        &["update-index", "--force-remove", "--", path],
                        30_000,
                    )?;
                }
            }
        }
        let tree = trimmed(git_with(
            &repo.checkout,
            Some(&index),
            &["write-tree"],
            30_000,
        )?)?;
        let base_tree = git_text(
            &repo.checkout,
            &["rev-parse", &format!("{}^{{tree}}", repo.base)],
        )?;
        ensure!(tree != base_tree, "the candidate does not change the base");

        let message = match message.map(str::trim).filter(|m| !m.is_empty()) {
            Some(m) => m.to_string(),
            None => format!(
                "Deliver accepted task {}\n\nEvidence digest: {}\nFiles:\n{}",
                &task[..task.len().min(8)],
                c.evidence_digest,
                c.files
                    .keys()
                    .map(|f| format!("- {f}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
        };
        let message_file = scratch.join("message");
        std::fs::write(&message_file, &message)?;
        let message_str = message_file
            .to_str()
            .context("UTF-8 scratch path required")?;
        let commit = trimmed(git(
            &repo.checkout,
            &["commit-tree", &tree, "-p", &repo.base, "-F", message_str],
        )?)
        .context("commit-tree failed; is user.name/user.email configured for this repository?")?;
        Ok((tree, commit, message))
    }

    fn current(&self, task: &str) -> Result<Delivery> {
        let delivery = self
            .get(task)?
            .context("this task has not been delivered to a branch")?;
        let head = git_text(
            &delivery.checkout,
            &[
                "rev-parse",
                "--verify",
                &format!("refs/heads/{}", delivery.branch),
            ],
        )
        .with_context(|| format!("branch {} no longer exists", delivery.branch))?;
        ensure!(
            head == delivery.commit,
            "branch {} moved since delivery ({} is not the delivered {})",
            delivery.branch,
            head,
            delivery.commit
        );
        Ok(delivery)
    }

    /// Pushes the delivered branch to a configured remote. Uses the operator's own Git
    /// credentials; never prompts.
    pub fn push(&mut self, task: &str, remote: &str, by: &str) -> Result<Delivery> {
        actor(by)?;
        let mut delivery = self.current(task)?;
        ensure!(
            delivery.status == "committed",
            "delivery is already {}",
            delivery.status
        );
        let remotes = git_text(&delivery.checkout, &["remote"])?;
        ensure!(
            remotes.lines().any(|r| r.trim() == remote),
            "{remote:?} is not a configured remote of this repository"
        );
        let refspec = format!("refs/heads/{0}:refs/heads/{0}", delivery.branch);
        git_with(
            &delivery.checkout,
            None,
            &["push", "--porcelain", remote, &refspec],
            NETWORK_TIMEOUT_MS,
        )?;
        delivery.status = "pushed".into();
        delivery.remote = Some(remote.to_string());
        delivery.events.push(DeliveryEvent {
            at: now(),
            stage: "delivery_pushed".into(),
            by: by.into(),
            detail: Some(remote.to_string()),
        });
        self.save(&delivery)?;
        Ok(delivery)
    }

    /// Opens a pull request for a pushed delivery with the GitHub CLI at `gh`, as a draft
    /// unless `ready`. `base_branch` defaults to the remote's default branch.
    pub fn open_pr(
        &mut self,
        task: &str,
        gh: &Path,
        base_branch: Option<&str>,
        ready: bool,
        by: &str,
    ) -> Result<Delivery> {
        actor(by)?;
        let mut delivery = self.current(task)?;
        ensure!(
            delivery.status == "pushed",
            "push the delivery before opening a pull request (status {})",
            delivery.status
        );
        let (title, body) = match delivery.message.split_once('\n') {
            Some((title, body)) => (title.trim().to_string(), body.trim().to_string()),
            None => (delivery.message.trim().to_string(), String::new()),
        };
        let mut command = Command::new(gh);
        command
            .current_dir(&delivery.checkout)
            .args([
                "pr",
                "create",
                "--head",
                &delivery.branch,
                "--title",
                &title,
                "--body",
            ])
            .arg(if body.is_empty() { title.clone() } else { body })
            .env("GH_PROMPT_DISABLED", "1");
        if let Some(base) = base_branch.map(str::trim).filter(|b| !b.is_empty()) {
            command.args(["--base", base]);
        }
        if !ready {
            command.arg("--draft");
        }
        let output = bounded_output(&mut command, NETWORK_TIMEOUT_MS, 1024 * 1024)
            .context("could not run the GitHub CLI")?;
        ensure!(
            output.exit_code == Some(0),
            "gh pr create failed: {}",
            String::from_utf8_lossy(&output.stderr.bytes).trim()
        );
        let url = String::from_utf8_lossy(&output.stdout.bytes)
            .lines()
            .rev()
            .map(str::trim)
            .find(|l| l.starts_with("https://") || l.starts_with("http://"))
            .context("gh pr create did not report a pull request URL")?
            .to_string();
        delivery.status = "pr_opened".into();
        delivery.pr_url = Some(url.clone());
        delivery.events.push(DeliveryEvent {
            at: now(),
            stage: "delivery_pr_opened".into(),
            by: by.into(),
            detail: Some(url),
        });
        self.save(&delivery)?;
        Ok(delivery)
    }
}
