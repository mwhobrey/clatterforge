use crate::{
    codex::prompt,
    digest,
    fixture::Spec,
    launcher::{LaunchSpec, read_receipt},
    now,
    process::{OwnedProcess, ProcessResult, StopReason, current_identity},
    repository::{canonical, git, git_output, git_text},
    store::{
        Artifact, Attempt, LogArtifact, ProcessReceipt, Review, Status, Store, Task,
        VerificationReceipt,
    },
};
use anyhow::{Context, Result, bail, ensure};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::Command,
};
use uuid::Uuid;

pub(crate) fn artifact(path: &Path, bytes: &[u8]) -> Result<Artifact> {
    let temporary = path.with_extension(format!("{}.tmp", Uuid::new_v4()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    Ok(Artifact {
        path: path.to_path_buf(),
        hash: digest(bytes),
        bytes: bytes.len(),
    })
}

pub(crate) fn validate_artifact(artifact: &Artifact) -> Result<()> {
    let bytes = fs::read(&artifact.path)?;
    ensure!(
        bytes.len() == artifact.bytes && digest(&bytes) == artifact.hash,
        "review artifact changed: {}",
        artifact.path.display()
    );
    Ok(())
}

pub(crate) fn process_receipt(
    result: ProcessResult,
    command: Vec<String>,
    workspace: &Path,
    directory: &Path,
    phase: &str,
) -> Result<ProcessReceipt> {
    let stdout = LogArtifact {
        artifact: artifact(
            &directory.join(format!("{phase}.stdout.log")),
            &result.stdout.bytes,
        )?,
        total_bytes: result.stdout.total_bytes,
        truncated: result.stdout.total_bytes > result.stdout.bytes.len() as u64,
    };
    let stderr = LogArtifact {
        artifact: artifact(
            &directory.join(format!("{phase}.stderr.log")),
            &result.stderr.bytes,
        )?,
        total_bytes: result.stderr.total_bytes,
        truncated: result.stderr.total_bytes > result.stderr.bytes.len() as u64,
    };
    let outcome = match result.reason {
        StopReason::Exited if result.exit_code == Some(0) => "succeeded",
        StopReason::Exited => "failed",
        StopReason::Cancelled => "cancelled",
        StopReason::TimedOut => "timed_out",
        StopReason::ObservationFailed => "unknown",
    };
    Ok(ProcessReceipt {
        identity: result.identity,
        command,
        cwd: workspace.into(),
        started_at: result.started_at,
        ended_at: result.ended_at,
        exit_code: result.exit_code,
        outcome: outcome.into(),
        stdout,
        stderr,
        tree_terminated: result.tree_terminated,
        error: result.error,
        delegated: None,
    })
}

pub(crate) fn new_file_diff(workspace: &Path, file: &str) -> Result<Vec<u8>> {
    let output = git_output(
        workspace,
        &[
            "diff",
            "--no-index",
            "--binary",
            "--no-ext-diff",
            "--no-textconv",
            "--",
            "/dev/null",
            file,
        ],
    )?;
    ensure!(
        matches!(output.exit_code, Some(0 | 1)),
        "cannot capture new file diff: {}",
        String::from_utf8_lossy(&output.stderr.bytes)
    );
    Ok(output.stdout.bytes)
}

fn collect_diff(task: &Task, workspace: &Path) -> Result<Vec<u8>> {
    let mut diff = git(
        workspace,
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--binary",
            &task.assignment.repository.base,
            "--",
        ],
    )?;
    let new_files = git(workspace, &["ls-files", "--others", "-z"])?;
    for file in new_files.split(|b| *b == 0).filter(|s| !s.is_empty()) {
        let file = std::str::from_utf8(file)?;
        if task.assignment.backend != "codex-exec-v1" {
            ensure!(
                file == task.assignment.output_file,
                "unexpected untracked file outside approved scope: {file}"
            );
        }
        ensure!(
            canonical(&workspace.join(file))?.starts_with(canonical(workspace)?),
            "new file escapes owned workspace"
        );
        let new = new_file_diff(workspace, file)?;
        ensure!(
            diff.len() + new.len() <= 4 * 1024 * 1024,
            "aggregate patch exceeds 4 MiB"
        );
        diff.extend(new);
    }
    Ok(diff)
}

fn validate_provider_scope(task: &Task, workspace: &Path) -> Result<()> {
    ensure!(
        git(
            workspace,
            &[
                "diff",
                "--name-only",
                &task.assignment.repository.base,
                "--"
            ]
        )?
        .is_empty(),
        "provider changed tracked files outside approved scope"
    );
    let others = git(workspace, &["ls-files", "--others", "-z"])?;
    ensure!(
        others
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .all(|p| p == task.assignment.output_file.as_bytes()),
        "provider created files outside approved scope"
    );
    Ok(())
}

impl Store {
    pub fn run_fake(&mut self, id: &str, expected: u64) -> Result<Task> {
        ensure!(
            self.task(id)?.assignment.backend == "fake-process-v1",
            "run_fake requires a fixture assignment"
        );
        self.run(id, expected)
    }

    pub fn run(&mut self, id: &str, expected: u64) -> Result<Task> {
        let initial = self.task(id)?;
        ensure!(initial.revision == expected, "revision conflict");
        ensure!(
            initial.status == Status::Approved,
            "launch requires current assignment approval"
        );
        initial.assignment.validate()?;
        initial.assignment.repository.revalidate()?;
        initial
            .assignment
            .context
            .revalidate(&initial.assignment.repository)?;
        ensure!(
            !self
                .root
                .starts_with(&initial.assignment.repository.checkout)
                && !self
                    .root
                    .starts_with(&initial.assignment.repository.common_dir),
            "state directory must be outside the registered checkout and Git directory"
        );
        let parent = self.root.join("workspaces");
        fs::create_dir_all(&parent)?;
        ensure!(
            canonical(&parent)?.starts_with(&self.root),
            "workspace parent resolves outside state root"
        );
        let attempt_id = Uuid::new_v4().to_string();
        let workspace = initial.assignment.workspace.clone();
        ensure!(
            workspace == parent.join(&initial.assignment.id),
            "approved workspace does not match this state root"
        );
        let intent = self.mutate(id, expected, "workspace_intent", |task| {
            ensure!(task.status == Status::Approved, "launch requires approval");
            ensure!(
                task.approval
                    .as_ref()
                    .context("missing approval")?
                    .assignment_digest
                    == task.assignment.digest()?,
                "approval is stale"
            );
            task.status = Status::PreparingWorkspace;
            task.attempts.push(Attempt {
                id: attempt_id.clone(),
                assignment_digest: task.assignment.digest()?,
                workspace: workspace.clone(),
                started_at: now(),
                ended_at: None,
                execution: "not_started".into(),
                error: None,
                diff: None,
                output: None,
                receipt: None,
                supervisor: Some(current_identity()?),
                process: None,
                verification_process: None,
                process_receipt: None,
                cancel_requested: None,
            });
            Ok(())
        })?;

        let prepared = (|| -> Result<()> {
            let hooks = self.root.join("disabled-hooks");
            fs::create_dir_all(&hooks)?;
            // Disable configured checkout filters without changing the user's Git config.
            let filters = git_output(
                &intent.assignment.repository.checkout,
                &[
                    "config",
                    "--name-only",
                    "--get-regexp",
                    "^filter\\..*\\.(smudge|process|required)$",
                ],
            )?;
            ensure!(
                matches!(filters.exit_code, Some(0 | 1)),
                "could not inspect checkout filters"
            );
            ensure!(!workspace.exists(), "owned workspace path already exists");
            let workspace_text = workspace.to_str().context("workspace path must be UTF-8")?;
            let mut arguments = vec![
                "-c".to_string(),
                format!("core.hooksPath={}", hooks.display()),
            ];
            for key in std::str::from_utf8(&filters.stdout.bytes)?.lines() {
                arguments.push("-c".into());
                arguments.push(format!(
                    "{key}={}",
                    if key.ends_with(".required") {
                        "false"
                    } else {
                        ""
                    }
                ));
            }
            arguments.extend([
                "worktree".into(),
                "add".into(),
                "--detach".into(),
                workspace_text.into(),
                intent.assignment.repository.base.clone(),
            ]);
            git(
                &intent.assignment.repository.checkout,
                &arguments.iter().map(String::as_str).collect::<Vec<_>>(),
            )?;
            ensure!(
                git_text(&workspace, &["rev-parse", "HEAD"])? == intent.assignment.repository.base,
                "worktree base mismatch"
            );
            for item in &intent.assignment.context.items {
                let path = workspace.join(&item.path);
                if let Some(hash) = &item.hash {
                    let resolved = canonical(&path).context("captured context is absent in committed worktree; dirty context cannot be dispatched")?;
                    ensure!(
                        resolved.starts_with(canonical(&workspace)?),
                        "worktree context escapes workspace"
                    );
                    ensure!(
                        digest(&fs::read(resolved)?) == *hash,
                        "context differs from committed worktree: {}; commit context changes or use another fixture",
                        item.path
                    );
                } else {
                    ensure!(!path.exists(), "worktree context differs: {}", item.path);
                }
            }
            ensure!(
                !workspace.join(&intent.assignment.output_file).exists(),
                "fake backend only creates a new file; output already exists"
            );
            Ok(())
        })();
        if let Err(error) = prepared {
            return self.fail_attempt(id, "workspace_failed", &error.to_string());
        }
        if self
            .task(id)?
            .attempts
            .last()
            .unwrap()
            .cancel_requested
            .is_some()
        {
            return self.finish_process_task(id, "cancelled", None);
        }
        let execution = self.execute_fixture(id, false);
        match execution {
            Ok(execution) => {
                let executed = self.owned_update(id, "execution_finished", |task| {
                    let attempt = task.attempts.last_mut().context("missing attempt")?;
                    attempt.execution = execution.outcome.clone();
                    attempt.process_receipt = Some(execution.clone());
                    Ok(())
                })?;
                let captured = (|| -> Result<(Artifact, Option<Artifact>)> {
                    let directory = self.root.join("artifacts").join(&attempt_id);
                    let diff = artifact(
                        &directory.join("result.patch"),
                        &collect_diff(&executed, &workspace)?,
                    )?;
                    let output_path = workspace.join(&executed.assignment.output_file);
                    let output = if output_path.exists() {
                        Some(artifact(
                            &directory.join("output.txt"),
                            &fs::read(output_path)?,
                        )?)
                    } else {
                        None
                    };
                    Ok((diff, output))
                })();
                let (diff, output) = match captured {
                    Ok(capture) => capture,
                    Err(error) => {
                        return self.fail_attempt(id, "collection_failed", &error.to_string());
                    }
                };
                self.owned_update(id, "artifacts_collected", |task| {
                    let attempt = task.attempts.last_mut().context("missing attempt")?;
                    attempt.diff = Some(diff.clone());
                    attempt.output = output.clone();
                    Ok(())
                })?;
                if executed.assignment.codex.is_some()
                    && execution.outcome == "succeeded"
                    && let Err(error) = validate_provider_scope(&executed, &workspace)
                {
                    return self.fail_attempt(id, "scope_violation", &error.to_string());
                }
                if execution.outcome != "succeeded" {
                    return self.finish_process_task(id, &execution.outcome, None);
                }
                if self
                    .task(id)?
                    .attempts
                    .last()
                    .unwrap()
                    .cancel_requested
                    .is_some()
                {
                    return self.finish_process_task(id, "cancelled", None);
                }
                let verification = match self.execute_fixture(id, true) {
                    Ok(check) => check,
                    Err(error) => {
                        return self.fail_attempt(id, "verification_failed", &error.to_string());
                    }
                };
                let task = self.task(id)?;
                let mut receipt = VerificationReceipt {
                    attempt_id: attempt_id.clone(),
                    assignment_digest: task.assignment.digest()?,
                    workspace: workspace.clone(),
                    base: task.assignment.repository.base.clone(),
                    output_hash: output.as_ref().map(|a| a.hash.clone()).unwrap_or_default(),
                    check: if task.assignment.backend == "codex-exec-v1" {
                        "approved independent verification command and host_file_bytes/source checks".into()
                    } else {
                        "independent fixture command and host_file_bytes (no project tests run)"
                            .into()
                    },
                    result: "pending_host_check".into(),
                    started_at: verification.started_at,
                    ended_at: verification.ended_at,
                    command: Some(verification.command.clone()),
                    exit_code: verification.exit_code,
                    source_hash: Some(diff.hash.clone()),
                    process_receipt: Some(verification.clone()),
                };
                // Save the command outcome before fallible host collection/revalidation.
                self.owned_update(id, "verification_finished", |task| {
                    task.attempts.last_mut().unwrap().receipt = Some(receipt.clone());
                    Ok(())
                })?;
                if verification.outcome != "succeeded" {
                    receipt.result = verification.outcome.clone();
                    return self.finish_process_task(id, &verification.outcome, Some(receipt));
                }
                // Verification itself must not silently alter the reviewed source snapshot.
                let checked = (|| -> Result<(bool, bool)> {
                    let source_unchanged = digest(&collect_diff(&task, &workspace)?) == diff.hash;
                    let bytes_match = fs::read(workspace.join(&task.assignment.output_file))?
                        .as_slice()
                        == task.assignment.contents.as_bytes();
                    Ok((source_unchanged, bytes_match))
                })();
                let (source_unchanged, bytes_match) = match checked {
                    Ok(check) => check,
                    Err(error) => {
                        return self.fail_attempt(
                            id,
                            "verification_collection_failed",
                            &error.to_string(),
                        );
                    }
                };
                let passed = bytes_match && source_unchanged;
                let result = if passed { "passed" } else { "failed" };
                receipt.result = result.into();
                let terminal = if passed {
                    "awaiting_review"
                } else {
                    "verification_failed"
                };
                self.finish_process_task(id, terminal, Some(receipt))
            }
            Err(error) => self.fail_attempt(id, "process_failed", &error.to_string()),
        }
    }

    fn owned_update<F>(&mut self, id: &str, kind: &str, mut change: F) -> Result<Task>
    where
        F: FnMut(&mut Task) -> Result<()>,
    {
        for _ in 0..5 {
            let latest = self.task(id)?;
            let result = self.mutate(id, latest.revision, kind, |task| {
                ensure!(
                    matches!(
                        task.status,
                        Status::PreparingWorkspace | Status::Running | Status::Verifying
                    ),
                    "attempt is no longer owned/active; inspect retained evidence"
                );
                ensure!(
                    task.attempts.last().and_then(|a| a.supervisor.as_ref())
                        == Some(&current_identity()?),
                    "attempt supervisor identity changed"
                );
                change(task)
            });
            match result {
                Err(error) if error.to_string().starts_with("revision conflict") => continue,
                other => return other,
            }
        }
        anyhow::bail!("repeated revision conflicts observing owned attempt")
    }

    fn execute_fixture(&mut self, id: &str, verify: bool) -> Result<ProcessReceipt> {
        let intent = self.owned_update(
            id,
            if verify {
                "verification_intent"
            } else {
                "execution_intent"
            },
            |task| {
                task.status = if verify {
                    Status::Verifying
                } else {
                    Status::Running
                };
                if !verify {
                    task.attempts.last_mut().unwrap().execution = "dispatch_intent".into();
                }
                Ok(())
            },
        )?;
        let attempt = intent.attempts.last().context("missing attempt")?;
        let policy = intent
            .assignment
            .execution_policy
            .as_ref()
            .context("missing process policy")?;
        policy.validate()?;
        let directory = self.root.join("artifacts").join(&attempt.id);
        fs::create_dir_all(&directory)?;
        ensure!(
            canonical(&directory)?.starts_with(&self.root),
            "artifact directory escapes state root"
        );
        let phase = if verify { "verification" } else { "execution" };
        let token = Uuid::new_v4().to_string();
        let spec = Spec {
            workspace: intent.assignment.workspace.clone(),
            output_file: intent.assignment.output_file.clone(),
            contents: intent.assignment.contents.clone(),
            outcome: intent.assignment.fake_outcome.clone(),
            gate: directory.join(format!("{phase}.gate")),
            token: token.clone(),
            owner: current_identity()?,
        };
        let spec_path = directory.join(format!("{phase}.json"));
        let provider = intent.assignment.codex.as_ref();
        let delegated_path = directory.join(format!("{phase}.delegated.json"));
        if let Some(provider) = provider {
            provider.validate()?;
            let delegated = LaunchSpec {
                workspace: spec.workspace.clone(),
                gate: spec.gate.clone(),
                token: token.clone(),
                owner: spec.owner.clone(),
                command: if verify {
                    provider.verifier.clone()
                } else {
                    provider.command(&spec.workspace)
                },
                codex_home: if verify {
                    None
                } else {
                    Some(provider.home.clone())
                },
                input: if verify {
                    String::new()
                } else {
                    prompt(&intent.assignment)?
                },
                receipt_path: delegated_path.clone(),
                parse_codex: !verify,
            };
            artifact(&spec_path, &serde_json::to_vec(&delegated)?)?;
        } else {
            artifact(&spec_path, &serde_json::to_vec(&spec)?)?;
        }
        let mut command = Command::new(&policy.executable);
        command
            .arg(if provider.is_some() {
                "__provider-launcher"
            } else {
                "__fixture-worker"
            })
            .arg(&spec_path)
            .arg(&token)
            .current_dir(&spec.workspace);
        if verify && provider.is_none() {
            command.arg("--verify");
        }
        let command_receipt = std::iter::once(policy.executable.to_string_lossy().into_owned())
            .chain(command.get_args().map(|a| a.to_string_lossy().into_owned()))
            .collect::<Vec<_>>();
        let stdout_path = directory.join(format!("{phase}.stdout.part"));
        let stderr_path = directory.join(format!("{phase}.stderr.part"));
        let process = OwnedProcess::spawn(
            &mut command,
            policy.log_bytes,
            Some((&stdout_path, &stderr_path)),
        )?;
        self.owned_update(
            id,
            if verify {
                "verification_launch_receipt"
            } else {
                "execution_launch_receipt"
            },
            |task| {
                let attempt = task.attempts.last_mut().unwrap();
                if verify {
                    attempt.verification_process = Some(process.identity.clone());
                } else {
                    attempt.process = Some(process.identity.clone());
                    attempt.execution = "running".into();
                }
                Ok(())
            },
        )?;
        if self
            .task(id)?
            .attempts
            .last()
            .unwrap()
            .cancel_requested
            .is_none()
        {
            artifact(&spec.gate, token.as_bytes())?;
        }
        let result = process.wait(
            if verify {
                policy.verification_timeout_ms
            } else {
                policy.timeout_ms
            },
            || {
                let task = self.task(id)?;
                ensure!(
                    matches!(task.status, Status::Running | Status::Verifying),
                    "attempt lost active state"
                );
                let latest = task.attempts.last().context("missing attempt")?;
                ensure!(
                    latest.id == attempt.id && latest.supervisor.as_ref() == Some(&spec.owner),
                    "attempt identity changed"
                );
                Ok(latest.cancel_requested.is_some())
            },
        )?;
        let mut receipt =
            process_receipt(result, command_receipt, &spec.workspace, &directory, phase)?;
        if provider.is_some() {
            receipt.delegated = read_receipt(&delegated_path).ok();
            if receipt.outcome == "succeeded" {
                let delegated = receipt
                    .delegated
                    .as_ref()
                    .context("launcher exited without a delegated receipt")?;
                ensure!(
                    delegated.exit_code == Some(0),
                    "launcher/provider exit evidence disagrees"
                );
                if !verify && !delegated.protocol.as_ref().is_some_and(|p| p.completed()) {
                    receipt.outcome = "failed".into();
                    receipt.error = Some(
                        "Codex JSONL lacks a valid completed turn with usable agent output".into(),
                    );
                }
            }
        }
        Ok(receipt)
    }

    fn finish_process_task(
        &mut self,
        id: &str,
        outcome: &str,
        receipt: Option<VerificationReceipt>,
    ) -> Result<Task> {
        self.owned_update(id, "attempt_finished", |task| {
            task.status = match outcome {
                "awaiting_review" => Status::AwaitingReview,
                "cancelled" => Status::Cancelled,
                "timed_out" => Status::TimedOut,
                "unknown" => Status::Unknown,
                _ => Status::Failed,
            };
            let attempt = task.attempts.last_mut().unwrap();
            attempt.ended_at = Some(now());
            attempt.receipt = receipt.clone();
            if outcome != "awaiting_review" {
                attempt.error = Some(format!("attempt finished: {outcome}"));
            }
            Ok(())
        })
    }

    fn fail_attempt(&mut self, id: &str, kind: &str, message: &str) -> Result<Task> {
        self.owned_update(id, kind, |task| {
            ensure!(
                matches!(
                    task.status,
                    Status::PreparingWorkspace | Status::Running | Status::Verifying
                ),
                "attempt state changed; inspect preserved workspace"
            );
            let attempt = task.attempts.last_mut().context("missing attempt")?;
            let uncertain = (kind == "process_failed" && attempt.process.is_some())
                || (kind == "verification_failed" && attempt.verification_process.is_some());
            task.status = if uncertain {
                Status::Unknown
            } else {
                Status::Failed
            };
            attempt.ended_at = Some(now());
            attempt.execution = if attempt.process.is_none() {
                "not_started"
            } else if attempt.process_receipt.is_none() {
                "unknown"
            } else {
                &attempt.execution
            }
            .into();
            attempt.error = Some(message.into());
            Ok(())
        })
    }

    pub fn evidence_digest(&self, task: &Task) -> Result<String> {
        let attempt = task.attempts.last().context("no attempt to review")?;
        Ok(digest(&serde_json::to_vec(&(
            task.assignment.digest()?,
            attempt,
        ))?))
    }

    pub fn review(
        &mut self,
        id: &str,
        expected: u64,
        reviewed_evidence: &str,
        reviewer: &str,
        accepted: bool,
    ) -> Result<Task> {
        let task = self.task(id)?;
        ensure!(
            task.status == Status::AwaitingReview,
            "task is not awaiting review"
        );
        ensure!(!reviewer.trim().is_empty(), "reviewer is required");
        if let Some(drift) = task.assignment.repository.drift()? {
            bail!(
                "repository changed since this assignment's context was captured: {}; register and prepare a new assignment",
                drift.describe()
            );
        }
        task.assignment
            .context
            .revalidate(&task.assignment.repository)?;
        let attempt = task.attempts.last().context("no attempted result")?;
        if task.assignment.codex.is_some() {
            validate_provider_scope(&task, &attempt.workspace)?;
        }
        let diff = attempt.diff.as_ref().context("missing diff")?;
        validate_artifact(diff)?;
        for process in attempt.process_receipt.iter().chain(
            attempt
                .receipt
                .iter()
                .filter_map(|r| r.process_receipt.as_ref()),
        ) {
            validate_artifact(&process.stdout.artifact)?;
            validate_artifact(&process.stderr.artifact)?;
        }
        ensure!(
            digest(&collect_diff(&task, &attempt.workspace)?) == diff.hash,
            "workspace changed after capture"
        );
        if accepted {
            ensure!(
                attempt.execution == "succeeded",
                "execution did not succeed"
            );
            let receipt = attempt
                .receipt
                .as_ref()
                .context("no host verification receipt")?;
            ensure!(receipt.result == "passed", "host file check did not pass");
            if matches!(
                task.assignment.backend.as_str(),
                "fake-process-v1" | "codex-exec-v1"
            ) {
                let execution = attempt
                    .process_receipt
                    .as_ref()
                    .context("missing execution receipt")?;
                let check = receipt
                    .process_receipt
                    .as_ref()
                    .context("missing verifier process receipt")?;
                ensure!(
                    execution.outcome == "succeeded"
                        && execution.exit_code == Some(0)
                        && execution.tree_terminated
                        && check.outcome == "succeeded"
                        && check.exit_code == Some(0)
                        && check.tree_terminated
                        && receipt.command.as_ref() == Some(&check.command)
                        && receipt.exit_code == Some(0)
                        && receipt.source_hash.as_ref() == Some(&diff.hash)
                        && check.cwd == attempt.workspace
                        && execution.cwd == attempt.workspace,
                    "process evidence does not establish a verified result"
                );
                if let Some(policy) = &task.assignment.codex {
                    let provider = execution
                        .delegated
                        .as_ref()
                        .context("missing provider receipt")?;
                    let verification = check
                        .delegated
                        .as_ref()
                        .context("missing approved command receipt")?;
                    ensure!(
                        provider.command == policy.command(&attempt.workspace).argv()
                            && provider.cwd == attempt.workspace
                            && provider.exit_code == Some(0)
                            && provider.protocol.as_ref().is_some_and(|p| p.completed())
                            && verification.command == policy.verifier.argv()
                            && verification.cwd == attempt.workspace
                            && verification.exit_code == Some(0),
                        "provider or independent verification evidence mismatches approval"
                    );
                }
            }
            let output = attempt.output.as_ref().context("no output artifact")?;
            validate_artifact(output)?;
            ensure!(
                receipt.assignment_digest == task.assignment.digest()?
                    && receipt.attempt_id == attempt.id
                    && receipt.output_hash == output.hash,
                "receipt does not match reviewed assignment or attempt"
            );
            ensure!(
                digest(&fs::read(
                    attempt.workspace.join(&task.assignment.output_file)
                )?) == output.hash,
                "workspace output changed after verification"
            );
        }
        let evidence_digest = self.evidence_digest(&task)?;
        ensure!(
            evidence_digest == reviewed_evidence,
            "evidence digest does not match reviewed result"
        );
        self.mutate(id, expected, "reviewed", |task| {
            ensure!(
                task.status == Status::AwaitingReview,
                "review state changed"
            );
            task.review = Some(Review {
                reviewer: reviewer.into(),
                accepted,
                evidence_digest,
                at: now(),
                comments: Vec::new(),
            });
            task.status = if accepted {
                Status::Accepted
            } else {
                Status::NeedsWork
            };
            Ok(())
        })
    }
}

/// Default state directory name.
pub const STATE_DIR: &str = ".clatterforge";
/// Earlier default; still used when it exists and the new one does not.
pub const LEGACY_STATE_DIR: &str = ".conductor";

/// `--state` default, relative to the working directory: `.clatterforge`, unless only a
/// legacy `.conductor` exists, which then keeps being used in place (never moved or copied).
pub fn default_state() -> PathBuf {
    default_state_in(Path::new(""))
}

pub fn default_state_in(base: &Path) -> PathBuf {
    let current = base.join(STATE_DIR);
    let legacy = base.join(LEGACY_STATE_DIR);
    if !current.exists() && legacy.is_dir() {
        legacy
    } else {
        current
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_state_prefers_the_new_dir_and_keeps_an_existing_legacy_one() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        // Fresh checkout: the new name.
        assert_eq!(default_state_in(base), base.join(STATE_DIR));
        // Only legacy state exists: keep using it in place.
        fs::create_dir(base.join(LEGACY_STATE_DIR)).unwrap();
        assert_eq!(default_state_in(base), base.join(LEGACY_STATE_DIR));
        // Both exist: the new one wins.
        fs::create_dir(base.join(STATE_DIR)).unwrap();
        assert_eq!(default_state_in(base), base.join(STATE_DIR));
        // A legacy *file* (not a state dir) never redirects.
        let other = tempfile::tempdir().unwrap();
        fs::write(other.path().join(LEGACY_STATE_DIR), "x").unwrap();
        assert_eq!(default_state_in(other.path()), other.path().join(STATE_DIR));
    }
}
