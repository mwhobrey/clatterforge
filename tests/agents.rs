use dev_conductor::{
    agents::{AgentBackend, AgentSpec, AgentStatus, Agents, CursorAgentPolicy},
    repository::git,
    service::TaskService,
    stop::GlobalStop,
};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::OnceLock,
};
use tempfile::TempDir;

fn fake_node() -> &'static Path {
    static FIXTURE: OnceLock<(TempDir, PathBuf)> = OnceLock::new();
    &FIXTURE
        .get_or_init(|| {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("fake_node.exe");
            assert!(
                Command::new("rustc")
                    .args(["tests/fixtures/fake_cursor_agent.rs", "-o"])
                    .arg(&path)
                    .status()
                    .unwrap()
                    .success()
            );
            fs::write(temp.path().join("index.js"), "// fake").unwrap();
            (temp, path)
        })
        .1
}
fn index_js() -> PathBuf {
    fake_node().parent().unwrap().join("index.js")
}
fn verifier() -> &'static Path {
    static FIXTURE: OnceLock<(TempDir, PathBuf)> = OnceLock::new();
    &FIXTURE
        .get_or_init(|| {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("verifier.exe");
            assert!(
                Command::new("rustc")
                    .args(["tests/fixtures/orchestration_verifier.rs", "-o"])
                    .arg(&path)
                    .status()
                    .unwrap()
                    .success()
            );
            (temp, path)
        })
        .1
}

fn fixture_repo(root: &Path) -> PathBuf {
    let repo = root.join("repo");
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::write(repo.join("README.md"), "fixture\n").unwrap();
    fs::write(repo.join("AGENTS.md"), "fixture\n").unwrap();
    fs::write(repo.join("src/result.txt"), "wrong").unwrap();
    fs::write(repo.join("verification.txt"), "fixed by fake cursor-agent").unwrap();
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
            "fixture base",
        ],
    )
    .unwrap();
    repo
}

fn spec(repo: &Path, objective: &str) -> AgentSpec {
    AgentSpec {
        agentlet: None,
        repository: repo.into(),
        objective: objective.into(),
        acceptance: "verifier passes".into(),
        allowed_files: vec!["src/result.txt".into()],
        verification_files: vec!["verification.txt".into()],
        verify_executable: verifier().into(),
        verify_args: vec![],
        target: "cursor-agent".into(),
        model: "fake-model".into(),
        generation_timeout_ms: 5000,
        generation_idle_timeout_ms: None,
        verification_timeout_ms: 5000,
        soft_timeouts: false,
        effort: None,
    }
}
fn policy() -> AgentBackend {
    AgentBackend::CursorAgent(
        CursorAgentPolicy::inspect(fake_node(), &index_js(), "fake-model".into()).unwrap(),
    )
}

fn activity_attempt(
    objective: &str,
    hard_ms: u64,
    idle_ms: Option<u64>,
) -> (TempDir, PathBuf, Agents, dev_conductor::agents::AgentTask) {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture_repo(dir.path());
    let state = dir.path().join("state");
    let mut agents = Agents::open(&state).unwrap();
    let mut specification = spec(&repo, objective);
    specification.generation_timeout_ms = hard_ms;
    specification.generation_idle_timeout_ms = idle_ms;
    let task = agents.create(specification, policy()).unwrap();
    let task = agents
        .approve(
            &task.id,
            task.revision,
            &task.assignment.digest().unwrap(),
            "fixture",
        )
        .unwrap();
    (dir, state, agents, task)
}

fn phase_progress(
    state: &Path,
    id: &str,
    phase: &str,
) -> dev_conductor::agent_progress::AgentProgress {
    serde_json::from_slice(
        &fs::read(
            state
                .join("agent-artifacts")
                .join(id)
                .join(format!("progress-{phase}.json")),
        )
        .unwrap(),
    )
    .unwrap()
}

#[test]
fn timeout_retains_only_approved_partial_diff_without_running_the_verifier() {
    let (_dir, state, mut agents, approved) = activity_attempt("MODE:write_then_hang", 1_000, None);
    let done = agents.run(&approved.id, approved.revision).unwrap();
    assert_eq!(done.status, AgentStatus::NeedsWork);
    assert_eq!(done.execution.as_ref().unwrap().outcome, "timed_out");
    assert!(done.verification.is_none());
    let patch = fs::read_to_string(&done.patch.unwrap().path).unwrap();
    assert!(patch.contains("partial change before timeout"));
    let routing = TaskService::open(&state)
        .unwrap()
        .detail(&done.id, None)
        .unwrap()
        .routing;
    assert!(routing.iter().any(|e| e.stage == "partial_patch_captured"));
    assert!(!routing.iter().any(|e| e.stage == "verification_launched"));
}

#[test]
fn agent_dispatch_must_pass_the_shared_dependency_gate() {
    let (_dir, state, mut agents, approved) = activity_attempt("normal", 4_000, None);
    let prerequisite = agents
        .create(approved.assignment.spec.clone(), policy())
        .unwrap();
    let mut service = TaskService::open(&state).unwrap();
    service
        .add_dependency(&approved.id, &prerequisite.id)
        .unwrap();
    assert!(service.run(&approved.id, approved.revision).is_err());
    assert!(agents.task(&approved.id).unwrap().execution.is_none());
    assert!(
        !agents
            .routing(&approved.id)
            .unwrap()
            .iter()
            .any(|e| e.stage == "generation_launched")
    );
}

#[test]
fn file_activity_renews_inactivity_window_and_is_visible_before_a_receipt_exists() {
    let (_dir, state, mut agents, approved) =
        activity_attempt("MODE:progress", 10_000, Some(2_000));
    let id = approved.id.clone();
    let worker_state = state.clone();
    let worker_id = id.clone();
    let running = std::thread::spawn(move || {
        Agents::open(&worker_state)
            .unwrap()
            .run(&worker_id, approved.revision)
            .unwrap()
    });
    let started = std::time::Instant::now();
    let mut saw_live_change = false;
    while started.elapsed() < std::time::Duration::from_secs(10) {
        let detail = TaskService::open(&state)
            .unwrap()
            .detail(&id, Some(64))
            .unwrap();
        if detail.status == "running"
            && let Some(progress) = detail.progress
        {
            assert!(
                detail.attempts.len() == 1,
                "a live attempt must be visible before its final receipt"
            );
            assert_eq!(progress.phase, "generation");
            assert_eq!(
                progress.stdout_bytes, 0,
                "this fixture changes files silently"
            );
            if progress
                .changed_files
                .contains(&"src/result.txt".to_string())
            {
                saw_live_change = true;
                assert_eq!(
                    detail.revision, 3,
                    "progress must not invalidate a cancellation revision"
                );
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
    let done = running.join().unwrap();
    assert!(saw_live_change);
    assert_eq!(done.status, AgentStatus::AwaitingReview, "{done:?}");
    assert!(
        done.execution.as_ref().unwrap().ended_at - done.execution.as_ref().unwrap().started_at
            > 2_000
    );
    let generation = phase_progress(&state, &id, "generation");
    assert_eq!(generation.stop_reason.as_deref(), Some("succeeded"));
    assert_eq!(generation.changed_files, vec!["src/result.txt"]);
    let final_detail = TaskService::open(&state)
        .unwrap()
        .detail(&id, None)
        .unwrap();
    assert_eq!(final_detail.progress.unwrap().phase, "verification");
    assert!(agents.run(&done.id, done.revision).is_err());
}

#[test]
fn quiet_agent_hits_inactivity_limit_and_never_replays() {
    let (_dir, state, mut agents, approved) = activity_attempt("MODE:hang", 4_000, Some(500));
    let done = agents.run(&approved.id, approved.revision).unwrap();
    assert_eq!(done.status, AgentStatus::NeedsWork);
    let receipt = done.execution.as_ref().unwrap();
    assert_eq!(receipt.outcome, "timed_out");
    assert!(receipt.error.as_deref().unwrap().contains("no observable"));
    assert!(receipt.ended_at - receipt.started_at < 3_500);
    assert!(receipt.tree_terminated);
    let progress = phase_progress(&state, &done.id, "generation");
    assert_eq!(progress.stop_reason.as_deref(), Some("inactivity_limit"));
    assert!(progress.last_activity_at.is_none());
    assert!(agents.run(&done.id, done.revision).is_err());
}

#[test]
fn chatter_beyond_the_log_capture_cap_cannot_extend_the_hard_limit() {
    let (_dir, state, mut agents, approved) = activity_attempt("MODE:chatter", 3_000, Some(2_500));
    let done = agents.run(&approved.id, approved.revision).unwrap();
    assert_eq!(done.status, AgentStatus::NeedsWork);
    let receipt = done.execution.as_ref().unwrap();
    assert_eq!(receipt.outcome, "timed_out");
    assert!(receipt.stdout.total_bytes > 65_536);
    assert!(receipt.stdout.truncated);
    assert!(receipt.ended_at - receipt.started_at >= 3_000);
    assert!(receipt.ended_at - receipt.started_at < 6_000);
    assert!(receipt.tree_terminated);
    let progress = phase_progress(&state, &done.id, "generation");
    assert_eq!(progress.stop_reason.as_deref(), Some("hard_limit"));
    assert_eq!(progress.stdout_bytes, receipt.stdout.total_bytes);
}

#[test]
fn soft_timeouts_emit_overtime_alert_without_killing_active_run() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture_repo(dir.path());
    let state = dir.path().join("state");
    let mut agents = Agents::open(&state).unwrap();
    let mut specification = spec(&repo, "MODE:progress");
    specification.generation_timeout_ms = 800;
    specification.generation_idle_timeout_ms = Some(1_500);
    specification.verification_timeout_ms = 5_000;
    specification.soft_timeouts = true;

    let task = agents.create(specification, policy()).unwrap();
    let digest = task.assignment.digest().unwrap();
    let approved = agents
        .approve(&task.id, task.revision, &digest, "fixture")
        .unwrap();

    let worker_state = state.clone();
    let worker_id = approved.id.clone();
    let worker = std::thread::spawn(move || {
        Agents::open(&worker_state)
            .unwrap()
            .run(&worker_id, approved.revision)
            .unwrap()
    });

    // Wait until the live run crosses its 800ms alert threshold and verify board() reports it.
    let start = std::time::Instant::now();
    let mut saw_board_alert = false;
    loop {
        let rows = TaskService::open(&state).unwrap().board().unwrap();
        if let Some(row) = rows.iter().find(|r| r.id == approved.id) {
            if let Some(alert) = &row.alert {
                assert!(alert.contains("overtime"), "unexpected alert: {alert}");
                saw_board_alert = true;
                break;
            }
            if row.status == "awaiting_review" {
                break;
            }
        }
        assert!(
            start.elapsed() < std::time::Duration::from_secs(15),
            "timed out waiting for soft-timeout progress"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    let done = worker.join().unwrap();
    assert_eq!(done.status, AgentStatus::AwaitingReview);
    assert!(
        saw_board_alert,
        "expected board() to surface live overtime alert"
    );
    let progress = phase_progress(&state, &done.id, "generation");
    assert!(!progress.limit_enforced);
    assert!(progress.overtime);
    assert!(progress.overtime_ms > 0);
    assert_eq!(progress.stop_reason.as_deref(), Some("succeeded"));
}

#[test]
fn soft_timeouts_still_halt_stalled_runs_after_inactivity_limit() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture_repo(dir.path());
    let state = dir.path().join("state");
    let mut agents = Agents::open(&state).unwrap();
    let mut specification = spec(&repo, "MODE:write_then_hang");
    specification.generation_timeout_ms = 300;
    specification.generation_idle_timeout_ms = Some(700);
    specification.verification_timeout_ms = 5_000;
    specification.soft_timeouts = true;

    let task = agents.create(specification, policy()).unwrap();
    let digest = task.assignment.digest().unwrap();
    let approved = agents
        .approve(&task.id, task.revision, &digest, "fixture")
        .unwrap();

    let done = agents.run(&approved.id, approved.revision).unwrap();
    assert_eq!(done.status, AgentStatus::NeedsWork);
    assert_eq!(done.execution.as_ref().unwrap().outcome, "timed_out");
    let progress = phase_progress(&state, &done.id, "generation");
    assert!(!progress.limit_enforced);
    assert!(progress.overtime);
    assert_eq!(progress.stop_reason.as_deref(), Some("inactivity_limit"));
}

#[test]
fn progress_observation_keeps_cancellation_immediate_and_revision_checked() {
    let (_dir, state, mut agents, approved) =
        activity_attempt("MODE:progress", 10_000, Some(2_000));
    let id = approved.id.clone();
    let worker_state = state.clone();
    let worker_id = id.clone();
    let worker = std::thread::spawn(move || {
        Agents::open(&worker_state)
            .unwrap()
            .run(&worker_id, approved.revision)
            .unwrap()
    });
    let start = std::time::Instant::now();
    loop {
        let detail = TaskService::open(&state)
            .unwrap()
            .detail(&id, None)
            .unwrap();
        if detail.progress.is_some() && detail.status == "running" {
            assert!(agents.cancel(&id, 1).is_err());
            agents.cancel(&id, detail.revision).unwrap();
            break;
        }
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let done = worker.join().unwrap();
    assert_eq!(done.status, AgentStatus::Cancelled);
    assert!(done.execution.as_ref().unwrap().tree_terminated);
    assert_eq!(
        phase_progress(&state, &id, "generation")
            .stop_reason
            .as_deref(),
        Some("cancelled")
    );
}

#[test]
fn legacy_spec_digest_is_stable_and_long_runs_need_an_explicit_idle_budget() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture_repo(dir.path());
    let original = spec(&repo, "legacy");
    let wire = serde_json::to_vec(&original).unwrap();
    assert!(!String::from_utf8_lossy(&wire).contains("generation_idle_timeout_ms"));
    let decoded: AgentSpec = serde_json::from_slice(&wire).unwrap();
    assert!(decoded.generation_idle_timeout_ms.is_none());
    assert_eq!(wire, serde_json::to_vec(&decoded).unwrap());
    let mut agents = Agents::open(&dir.path().join("state")).unwrap();
    let mut extended = original.clone();
    extended.generation_timeout_ms = 600_000;
    assert!(agents.create(extended.clone(), policy()).is_err());
    extended.generation_idle_timeout_ms = Some(120_000);
    let task = agents.create(extended.clone(), policy()).unwrap();
    assert_eq!(task.assignment.spec.generation_timeout_ms, 600_000);
    extended.generation_idle_timeout_ms = Some(600_001);
    assert!(agents.create(extended.clone(), policy()).is_err());
    extended.generation_idle_timeout_ms = Some(100);
    extended.generation_timeout_ms = 1_800_001;
    assert!(agents.create(extended, policy()).is_err());
}

fn fake_codex() -> &'static Path {
    static FIXTURE: OnceLock<(TempDir, PathBuf)> = OnceLock::new();
    &FIXTURE
        .get_or_init(|| {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("fake_codex.exe");
            assert!(
                Command::new("rustc")
                    .args(["tests/fixtures/fake_codex_coding_agent.rs", "-o"])
                    .arg(&path)
                    .status()
                    .unwrap()
                    .success()
            );
            (temp, path)
        })
        .1
}
fn codex_home() -> &'static Path {
    static HOME: OnceLock<TempDir> = OnceLock::new();
    HOME.get_or_init(|| tempfile::tempdir().unwrap()).path()
}
fn codex_spec(repo: &Path, objective: &str) -> AgentSpec {
    AgentSpec {
        target: "codex".into(),
        ..spec(repo, objective)
    }
}
fn codex_policy() -> AgentBackend {
    AgentBackend::Codex(
        dev_conductor::codex::CodexPolicy::inspect(
            fake_codex(),
            codex_home(),
            "fake-model".into(),
            dev_conductor::codex::PinnedCommand {
                executable: dev_conductor::codex::Executable::new(verifier()).unwrap(),
                args: vec![],
            },
        )
        .unwrap(),
    )
}

#[cfg(windows)]
#[test]
fn git_bash_parent_uses_native_cursor_shell_without_changing_verifier_environment() {
    const CHILD: &str = "CONDUCTOR_NATIVE_CURSOR_ENV_TEST";
    if std::env::var_os(CHILD).is_none() {
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "git_bash_parent_uses_native_cursor_shell_without_changing_verifier_environment",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("MSYSTEM", "MINGW64")
            .env("SHELL", "/usr/bin/bash")
            .env("CONDUCTOR_ENV_SENTINEL", "preserved")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let repo = fixture_repo(dir.path());
    let mut agents = Agents::open(&dir.path().join("state")).unwrap();
    let task = agents
        .create(spec(&repo, "fix the result"), policy())
        .unwrap();
    let approved = agents
        .approve(
            &task.id,
            task.revision,
            &task.assignment.digest().unwrap(),
            "fixture",
        )
        .unwrap();
    let done = agents.run(&approved.id, approved.revision).unwrap();
    assert_eq!(done.status, AgentStatus::AwaitingReview, "{done:?}");
    assert_eq!(std::env::var("MSYSTEM").unwrap(), "MINGW64");
    assert_eq!(std::env::var("SHELL").unwrap(), "/usr/bin/bash");
}

#[test]
fn agent_task_runs_to_awaiting_review_and_only_accepts_on_explicit_human_review() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture_repo(dir.path());
    let state = dir.path().join("state");

    let mut agents = Agents::open(&state).unwrap();
    let task = agents
        .create(spec(&repo, "fix the result"), policy())
        .unwrap();
    assert_eq!(task.status, AgentStatus::AwaitingApproval);

    let digest = task.assignment.digest().unwrap();
    let approved = agents
        .approve(&task.id, task.revision, &digest, "fixture reviewer")
        .unwrap();
    assert_eq!(approved.status, AgentStatus::Approved);

    let done = agents.run(&approved.id, approved.revision).unwrap();
    assert_eq!(done.status, AgentStatus::AwaitingReview, "{done:?}");
    assert!(done.review.is_none());
    assert!(done.patch.is_some());
    assert_eq!(
        fs::read_to_string(repo.join("src/result.txt")).unwrap(),
        "wrong",
        "the agent must edit only its own isolated worktree, never the registered repository"
    );

    // Not auto-accepted - a human must call review() explicitly.
    let evidence = agents.evidence_digest(&done).unwrap();
    let reviewed = agents
        .review(&done.id, done.revision, &evidence, "fixture reviewer", true)
        .unwrap();
    assert_eq!(reviewed.status, AgentStatus::Accepted);
    assert!(reviewed.review.unwrap().accepted);
    let service = TaskService::open(&state).unwrap();
    let index = fs::read(repo.join(".git/index")).unwrap();
    let prepared = service
        .prepare_integration(&done.id, "fixture reviewer")
        .unwrap()
        .integration
        .unwrap();
    let integrated = service
        .integrate(
            &done.id,
            prepared.revision,
            &prepared.plan_digest,
            "fixture reviewer",
            false,
        )
        .unwrap()
        .integration
        .unwrap();
    assert_eq!(integrated.status, "applied");
    assert_eq!(
        fs::read_to_string(repo.join("src/result.txt")).unwrap(),
        "fixed by fake cursor-agent"
    );
    assert_eq!(fs::read(repo.join(".git/index")).unwrap(), index);
    let restored = service
        .integrate(
            &done.id,
            integrated.revision,
            &prepared.plan_digest,
            "fixture reviewer",
            true,
        )
        .unwrap()
        .integration
        .unwrap();
    assert_eq!(restored.status, "rolled_back");
    assert_eq!(
        fs::read_to_string(repo.join("src/result.txt")).unwrap(),
        "wrong"
    );
}

#[test]
fn agent_edits_outside_the_approved_scope_are_refused_before_verification() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture_repo(dir.path());
    let state = dir.path().join("state");

    let mut agents = Agents::open(&state).unwrap();
    let task = agents
        .create(spec(&repo, "MODE:out_of_scope"), policy())
        .unwrap();
    let digest = task.assignment.digest().unwrap();
    let approved = agents
        .approve(&task.id, task.revision, &digest, "fixture")
        .unwrap();
    let done = agents.run(&approved.id, approved.revision).unwrap();
    assert_eq!(done.status, AgentStatus::NeedsWork, "{done:?}");
    assert!(
        done.error
            .as_deref()
            .unwrap()
            .contains("outside approved files"),
        "{:?}",
        done.error
    );
    assert!(
        done.verification.is_none(),
        "verification must never run once scope is violated"
    );
}

#[test]
fn a_failed_agent_execution_is_retained_as_evidence_not_replayed() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture_repo(dir.path());
    let state = dir.path().join("state");

    let mut agents = Agents::open(&state).unwrap();
    let task = agents.create(spec(&repo, "MODE:fail"), policy()).unwrap();
    let digest = task.assignment.digest().unwrap();
    let approved = agents
        .approve(&task.id, task.revision, &digest, "fixture")
        .unwrap();
    let done = agents.run(&approved.id, approved.revision).unwrap();
    assert_eq!(done.status, AgentStatus::NeedsWork, "{done:?}");
    assert_eq!(done.execution.as_ref().unwrap().outcome, "failed");
    assert_eq!(done.execution.as_ref().unwrap().exit_code, Some(1));

    // Terminal - a second `run` on the same (expired) revision must not replay it.
    let replay = agents.run(&done.id, approved.revision);
    assert!(replay.is_err());
}

#[test]
fn agent_run_is_blocked_by_global_stop() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture_repo(dir.path());
    let state = dir.path().join("state");

    let mut agents = Agents::open(&state).unwrap();
    let task = agents
        .create(spec(&repo, "fix the result"), policy())
        .unwrap();
    let digest = task.assignment.digest().unwrap();
    let approved = agents
        .approve(&task.id, task.revision, &digest, "fixture")
        .unwrap();

    GlobalStop::open(&state)
        .unwrap()
        .engage("fixture", "test")
        .unwrap();

    let error = agents.run(&approved.id, approved.revision).unwrap_err();
    assert!(error.to_string().contains("global stop"), "{error}");
}

#[test]
fn setup_failure_is_durable_and_a_fresh_task_can_acquire_the_repository() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture_repo(dir.path());
    let state = dir.path().join("state");
    let mut agents = Agents::open(&state).unwrap();
    let task = agents
        .create(spec(&repo, "setup failure"), policy())
        .unwrap();
    let approved = agents
        .approve(
            &task.id,
            task.revision,
            &task.assignment.digest().unwrap(),
            "fixture",
        )
        .unwrap();
    fs::create_dir_all(&approved.assignment.workspace).unwrap();
    let failed = agents.run(&task.id, approved.revision).unwrap();
    assert_eq!(failed.status, AgentStatus::Unknown);
    assert!(
        failed
            .error
            .as_deref()
            .unwrap()
            .contains("workspace parent/path changed")
    );
    assert!(failed.execution.is_none());
    assert_eq!(agents.task(&task.id).unwrap().status, AgentStatus::Unknown);
    assert!(agents.run(&failed.id, failed.revision).is_err());
    let next = agents.create(spec(&repo, "fresh task"), policy()).unwrap();
    let approved = agents
        .approve(
            &next.id,
            next.revision,
            &next.assignment.digest().unwrap(),
            "fixture",
        )
        .unwrap();
    assert_eq!(
        agents.run(&approved.id, approved.revision).unwrap().status,
        AgentStatus::AwaitingReview
    );
}

/// M10.2 Phase 1's required test: a genuinely long-running fake agent,
/// stopped by the *real* `global-stop` CLI entry point from a second,
/// independent process - not `GlobalStop::engage` or `Agents::cancel`
/// called directly. Exercises the actual sweep path a human running
/// `dev-conductor global-stop` would get: `TaskService::stop_all` walks
/// the shared board (now including agent tasks) and cancels every active
/// one through the same `cancel` a human uses for one task.
#[test]
fn real_global_stop_cli_terminates_a_genuinely_running_agent_process() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture_repo(dir.path());
    let state = dir.path().join("state");

    let mut agents = Agents::open(&state).unwrap();
    let task = agents.create(spec(&repo, "MODE:hang"), policy()).unwrap();
    let digest = task.assignment.digest().unwrap();
    let approved = agents
        .approve(&task.id, task.revision, &digest, "fixture")
        .unwrap();
    let task_id = approved.id.clone();
    let run_revision = approved.revision;
    drop(agents);

    // The real `agent-run` CLI entry point, as a genuinely separate OS
    // process - not an in-process `Agents::run` call.
    let mut runner = Command::new(env!("CARGO_BIN_EXE_dev-conductor"))
        .arg("--state")
        .arg(&state)
        .args([
            "agent-run",
            &task_id,
            "--revision",
            &run_revision.to_string(),
            "--json",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();

    // Give the fake agent a moment to actually be spawned and hanging
    // before the real global-stop entry point is invoked from a second,
    // independent process.
    std::thread::sleep(std::time::Duration::from_millis(300));
    let stop = Command::new(env!("CARGO_BIN_EXE_dev-conductor"))
        .arg("--state")
        .arg(&state)
        .args([
            "global-stop",
            "--by",
            "fixture",
            "--reason",
            "test",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        stop.status.success(),
        "{}",
        String::from_utf8_lossy(&stop.stderr)
    );
    let stop_outcome: serde_json::Value = serde_json::from_slice(&stop.stdout).unwrap();
    assert!(
        stop_outcome["cancelled"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v.as_str() == Some(task_id.as_str())),
        "global-stop's own outcome must report the agent task as cancelled: {stop_outcome}"
    );

    let status = runner.wait().unwrap();
    assert!(
        status.success(),
        "agent-run should complete (Cancelled is a normal, successfully-reported terminal state), not error out"
    );

    let agents = Agents::open(&state).unwrap();
    let done = agents.task(&task_id).unwrap();
    assert_eq!(done.status, AgentStatus::Cancelled, "{done:?}");
    assert_eq!(
        done.execution.as_ref().unwrap().outcome,
        "cancelled",
        "the owned fake-agent process must have actually been terminated, not merely marked"
    );
    assert!(
        done.verification.is_none(),
        "no verifier may run once cancellation has been requested"
    );

    // No repository lease remains improperly held - `run`'s own release
    // happens after `execute` returns, which it does here because the
    // process was genuinely terminated, not left hanging.
    let leases = dev_conductor::coordination::Coordination::open(&state)
        .unwrap()
        .reservations()
        .unwrap();
    assert!(leases.is_empty(), "{leases:?}");

    // No new agent dispatch is admitted while the stop remains engaged.
    let mut blocked_agents = Agents::open(&state).unwrap();
    let second = blocked_agents
        .create(spec(&repo, "fix the result"), policy())
        .unwrap();
    let second_digest = second.assignment.digest().unwrap();
    let second_approved = blocked_agents
        .approve(&second.id, second.revision, &second_digest, "fixture")
        .unwrap();
    let error = blocked_agents
        .run(&second_approved.id, second_approved.revision)
        .unwrap_err();
    assert!(error.to_string().contains("global stop"), "{error}");

    // No unrelated task was accidentally accepted or otherwise disturbed
    // by the sweep - it never advanced past AwaitingApproval/Approved on
    // its own.
    assert_eq!(second_approved.status, AgentStatus::Approved);
}

#[test]
fn cancelling_a_hung_agent_process_stops_it_and_lands_at_cancelled() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture_repo(dir.path());
    let state = dir.path().join("state");

    let mut agents = Agents::open(&state).unwrap();
    let task = agents.create(spec(&repo, "MODE:hang"), policy()).unwrap();
    let digest = task.assignment.digest().unwrap();
    let approved = agents
        .approve(&task.id, task.revision, &digest, "fixture")
        .unwrap();

    let id = approved.id.clone();
    let revision = approved.revision;
    let state_for_runner = state.clone();
    let runner = std::thread::spawn(move || {
        let mut agents = Agents::open(&state_for_runner).unwrap();
        agents.run(&id, revision).unwrap()
    });

    // Give the fake agent a moment to actually be spawned and hanging,
    // then request cancellation from a second, independent handle - the
    // same discipline the rest of this codebase's concurrency tests use
    // (a real second thread/process, not a simulated flag flip).
    std::thread::sleep(std::time::Duration::from_millis(300));
    let mut canceller = Agents::open(&state).unwrap();
    let live = canceller.task(&approved.id).unwrap();
    canceller.cancel(&approved.id, live.revision).unwrap();

    let done = runner.join().unwrap();
    assert_eq!(done.status, AgentStatus::Cancelled, "{done:?}");
    assert_eq!(done.execution.as_ref().unwrap().outcome, "cancelled");
}

/// M10.2 Phase 2's required test: a *real* owning process, killed
/// abruptly (not a simulated identity, not a graceful shutdown) while it
/// genuinely owns an active agent task, then reconciled from a second,
/// independent call. Mirrors `tests/orchestration.rs`'s own
/// `owner_crash_reconciles_to_unknown_without_relaunching` discipline
/// exactly, applied to the agent engine.
#[test]
fn real_process_kill_leaves_a_dead_owner_and_reconcile_recovers_without_replay() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture_repo(dir.path());
    let state = dir.path().join("state");

    let mut agents = Agents::open(&state).unwrap();
    let task = agents.create(spec(&repo, "MODE:hang"), policy()).unwrap();
    let digest = task.assignment.digest().unwrap();
    let approved = agents
        .approve(&task.id, task.revision, &digest, "fixture")
        .unwrap();
    let task_id = approved.id.clone();
    let run_revision = approved.revision;
    drop(agents);

    let mut child = Command::new(env!("CARGO_BIN_EXE_dev-conductor"))
        .arg("--state")
        .arg(&state)
        .args([
            "agent-run",
            &task_id,
            "--revision",
            &run_revision.to_string(),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();

    let agents = Agents::open(&state).unwrap();
    let until = std::time::Instant::now() + std::time::Duration::from_secs(8);
    let active = loop {
        let current = agents.task(&task_id).unwrap();
        if current.status == AgentStatus::Running && current.owner.is_some() {
            break current;
        }
        assert!(
            std::time::Instant::now() < until,
            "agent never reached Running: {:?}",
            current.status
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    };

    // Abrupt kill of the *owning dev-conductor process itself* - not the fake
    // agent child directly, and not a simulated identity. Windows' own
    // kill-on-job-close semantics (confirmed in `src/process.rs`) take the
    // fake-agent child down with it, so there is no orphaned process left
    // running either.
    child.kill().unwrap();
    child.wait().unwrap();

    let mut agents = Agents::open(&state).unwrap();
    assert!(
        agents.reconcile(&task_id, active.revision - 1).is_err(),
        "a stale revision must still be refused"
    );
    let current = agents.task(&task_id).unwrap();
    let recovered = agents.reconcile(&task_id, current.revision).unwrap();
    assert_eq!(recovered.status, AgentStatus::Unknown, "{recovered:?}");
    assert!(recovered.owner.is_none());
    assert!(recovered.error.as_deref().unwrap().contains("no replay"));

    // The repository lease is not silently released - it is quarantined,
    // exactly like a panicking `LeaseReservation` already quarantines one -
    // requiring the same explicit human acknowledgement every other
    // engine's crashed lease already requires.
    let leases = dev_conductor::coordination::Coordination::open(&state)
        .unwrap()
        .reservations()
        .unwrap();
    assert_eq!(leases.len(), 1, "{leases:?}");
    assert_eq!(leases[0]["task_id"], task_id);
    assert_eq!(leases[0]["quarantined"], true, "{leases:?}");

    // The original, interrupted attempt is never replayed.
    assert!(agents.run(&recovered.id, recovered.revision).is_err());
}

/// The other half of Phase 2's conservative-ownership contract: a task
/// whose owner is still genuinely alive (or, as here, is the very process
/// making the reconcile call) must never be reconciled - "no unsafe
/// release of a live owner's lease."
#[test]
fn reconcile_refuses_a_task_whose_owner_is_still_alive() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture_repo(dir.path());
    let state = dir.path().join("state");

    let mut agents = Agents::open(&state).unwrap();
    let task = agents.create(spec(&repo, "MODE:hang"), policy()).unwrap();
    let digest = task.assignment.digest().unwrap();
    let approved = agents
        .approve(&task.id, task.revision, &digest, "fixture")
        .unwrap();
    let task_id = approved.id.clone();
    let run_revision = approved.revision;

    let state_for_runner = state.clone();
    let runner = std::thread::spawn(move || {
        let mut agents = Agents::open(&state_for_runner).unwrap();
        agents.run(&task_id, run_revision).unwrap()
    });

    let watcher = Agents::open(&state).unwrap();
    let task_id = approved.id.clone();
    let until = std::time::Instant::now() + std::time::Duration::from_secs(8);
    let active = loop {
        let current = watcher.task(&task_id).unwrap();
        if current.status == AgentStatus::Running && current.owner.is_some() {
            break current;
        }
        assert!(std::time::Instant::now() < until);
        std::thread::sleep(std::time::Duration::from_millis(5));
    };

    // The owner is *this test binary's own process* - genuinely alive, not
    // simulated - so reconcile must refuse it outright.
    let mut reconciler = Agents::open(&state).unwrap();
    let error = reconciler.reconcile(&task_id, active.revision).unwrap_err();
    assert!(error.to_string().contains("alive"), "{error}");

    // Clean up: cancel the still-running attempt so the test doesn't leak a
    // hung process.
    let mut canceller = Agents::open(&state).unwrap();
    let live = canceller.task(&task_id).unwrap();
    canceller.cancel(&task_id, live.revision).unwrap();
    runner.join().unwrap();
}

#[test]
fn create_rejects_a_spec_target_that_does_not_match_the_supplied_backend() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture_repo(dir.path());
    let state = dir.path().join("state");

    let mut agents = Agents::open(&state).unwrap();
    // Neither an unrecognized string nor the *other* real backend's own
    // name may be paired with a mismatched policy - `AgentBackend` itself
    // is the actual whitelist now (exactly two variants), `spec.target`
    // is only cross-checked against whichever one was supplied.
    for bad_target in ["some-other-cli", "codex"] {
        let mut bad = spec(&repo, "fix the result");
        bad.target = bad_target.into();
        let error = agents.create(bad, policy()).unwrap_err();
        assert!(error.to_string().contains("does not match"), "{error}");
    }
}

/// M10.2 Phase 3: an agent task is a first-class citizen of the shared
/// board/service, not a second, competing status authority - the exact
/// same `TaskService` the CLI's `board-*` commands and the HTTP service
/// already use for the other two engines.
#[test]
fn agent_tasks_are_visible_and_controllable_through_the_shared_task_service() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture_repo(dir.path());
    let state = dir.path().join("state");

    let mut agents = Agents::open(&state).unwrap();
    let task = agents
        .create(spec(&repo, "fix the result"), policy())
        .unwrap();
    let digest = task.assignment.digest().unwrap();
    agents
        .approve(&task.id, task.revision, &digest, "fixture")
        .unwrap();
    drop(agents);

    let mut service = TaskService::open(&state).unwrap();
    let rows = service.board().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, "agent");
    assert_eq!(rows[0].id, task.id);
    assert_eq!(rows[0].status, "approved");

    // An unambiguous id prefix resolves through the shared service exactly
    // like it does for the other two engines.
    let prefix = &task.id[..8];
    let detail = service.detail(prefix, None).unwrap();
    assert_eq!(detail.kind, "agent");
    assert_eq!(detail.backend, "cursor-agent");
    assert!(detail.agent.contains("fake-model"));
    assert_eq!(detail.allowed_scope, vec!["src/result.txt".to_string()]);

    // Dispatch through the shared `run` - the same call `board-run`
    // makes - not the engine's own `Agents::run` directly.
    let outcome = service.run(&task.id, detail.revision).unwrap();
    assert!(outcome.succeeded, "{:?}", outcome.detail);
    assert_eq!(outcome.detail.status, "awaiting_review");
    assert!(outcome.detail.task_error.is_none());
    assert!(
        outcome
            .detail
            .attempts
            .first()
            .and_then(|a| a.outcome.as_deref())
            == Some("succeeded")
    );

    // Review through the shared service too - still gated, still requires
    // the current evidence digest.
    let evidence = outcome.detail.evidence_digest.clone().unwrap();
    let reviewed = service
        .review(
            &task.id,
            outcome.detail.revision,
            &evidence,
            "fixture",
            true,
        )
        .unwrap();
    assert_eq!(reviewed.status, "accepted");
}

/// Cancellation reaching an agent task through the shared service path -
/// the exact mechanism `global-stop`'s sweep itself relies on, exercised
/// directly rather than only as a side effect of that sweep.
#[test]
fn shared_service_cancel_reaches_a_running_agent_task() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture_repo(dir.path());
    let state = dir.path().join("state");

    let mut agents = Agents::open(&state).unwrap();
    let task = agents.create(spec(&repo, "MODE:hang"), policy()).unwrap();
    let digest = task.assignment.digest().unwrap();
    let approved = agents
        .approve(&task.id, task.revision, &digest, "fixture")
        .unwrap();
    let task_id = approved.id.clone();
    let run_revision = approved.revision;

    let state_for_runner = state.clone();
    let runner = std::thread::spawn(move || {
        let mut agents = Agents::open(&state_for_runner).unwrap();
        agents.run(&task_id, run_revision).unwrap()
    });

    let task_id = approved.id.clone();
    let watcher = Agents::open(&state).unwrap();
    let until = std::time::Instant::now() + std::time::Duration::from_secs(8);
    let active = loop {
        let current = watcher.task(&task_id).unwrap();
        if current.status == AgentStatus::Running && current.owner.is_some() {
            break current;
        }
        assert!(std::time::Instant::now() < until);
        std::thread::sleep(std::time::Duration::from_millis(5));
    };

    let mut service = TaskService::open(&state).unwrap();
    let detail = service.cancel(&task_id, active.revision, None).unwrap();
    assert_eq!(detail.kind, "agent");

    let done = runner.join().unwrap();
    assert_eq!(done.status, AgentStatus::Cancelled, "{done:?}");
}

/// M10.3 Phase 1's required test: the real `global-stop` CLI entry point,
/// from a second, independent process, stopping a task while it is in
/// `Verifying` (a genuinely hanging *independent verifier*), not merely
/// while an agent is still generating. `Agents::execute`'s verification
/// phase polls the identical `cancel_requested` flag through the identical
/// `OwnedProcess::wait` mechanism the execution phase already proved -
/// this test exercises that shared mechanism directly for the phase it
/// had never actually been run against.
#[test]
fn real_global_stop_cli_terminates_a_genuinely_hanging_verifier() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture_repo(dir.path());
    let state = dir.path().join("state");

    let mut agents = Agents::open(&state).unwrap();
    // No MODE: marker - the fake agent writes the correct fix and exits
    // cleanly, so the task genuinely reaches Verifying; only the
    // independent verifier (not the agent) hangs.
    let mut hanging_verify_spec = spec(&repo, "fix the result");
    hanging_verify_spec.verify_args = vec!["hang".into()];
    let task = agents.create(hanging_verify_spec, policy()).unwrap();
    let digest = task.assignment.digest().unwrap();
    let approved = agents
        .approve(&task.id, task.revision, &digest, "fixture")
        .unwrap();
    let task_id = approved.id.clone();
    let run_revision = approved.revision;
    drop(agents);

    // The real `agent-run` CLI entry point, as a genuinely separate OS
    // process - not an in-process `Agents::run` call.
    let mut runner = Command::new(env!("CARGO_BIN_EXE_dev-conductor"))
        .arg("--state")
        .arg(&state)
        .args([
            "agent-run",
            &task_id,
            "--revision",
            &run_revision.to_string(),
            "--json",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();

    // Deterministic synchronization: poll the shared SQLite state for the
    // task to genuinely reach Verifying (execution already succeeded and
    // the independent verifier has actually been spawned and is hanging)
    // before invoking the real global-stop entry point, rather than
    // guessing a sleep duration.
    let watcher = Agents::open(&state).unwrap();
    let until = std::time::Instant::now() + std::time::Duration::from_secs(8);
    loop {
        let current = watcher.task(&task_id).unwrap();
        if current.status == AgentStatus::Verifying && current.owner.is_some() {
            break;
        }
        assert!(
            std::time::Instant::now() < until,
            "task never reached Verifying: {:?}",
            current.status
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    let stop = Command::new(env!("CARGO_BIN_EXE_dev-conductor"))
        .arg("--state")
        .arg(&state)
        .args([
            "global-stop",
            "--by",
            "fixture",
            "--reason",
            "test",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        stop.status.success(),
        "{}",
        String::from_utf8_lossy(&stop.stderr)
    );
    let stop_outcome: serde_json::Value = serde_json::from_slice(&stop.stdout).unwrap();
    assert!(
        stop_outcome["cancelled"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v.as_str() == Some(task_id.as_str())),
        "global-stop's own outcome must report the agent task as cancelled: {stop_outcome}"
    );

    let status = runner.wait().unwrap();
    assert!(
        status.success(),
        "agent-run should complete (Cancelled is a normal, successfully-reported terminal state), not error out"
    );

    let agents = Agents::open(&state).unwrap();
    let done = agents.task(&task_id).unwrap();
    assert_eq!(done.status, AgentStatus::Cancelled, "{done:?}");
    assert_eq!(
        done.execution.as_ref().unwrap().outcome,
        "succeeded",
        "the agent's own generation phase completed normally before cancellation ever applied"
    );
    assert_eq!(
        done.verification.as_ref().unwrap().outcome,
        "cancelled",
        "the owned verifier process tree must have actually been terminated, not merely marked \
         (OwnedProcess::wait itself fails the whole command if the tree cannot be confirmed \
         terminated, which would have made agent-run exit non-zero above)"
    );
    assert!(
        done.review.is_none(),
        "no successful verification or acceptance may be recorded for a cancelled attempt"
    );

    // No repository lease remains improperly held.
    let leases = dev_conductor::coordination::Coordination::open(&state)
        .unwrap()
        .reservations()
        .unwrap();
    assert!(leases.is_empty(), "{leases:?}");
}

/// M10.3 Phase 2's required test: two genuinely independent `agent-run`
/// OS processes dispatching the *same approved task* at the same time,
/// against a fake agent that both records every real invocation to a
/// shared, test-controlled log (so a bug letting both processes reach the
/// fixture would be caught even if the worktree-exists check also would
/// have caught it) and deliberately sleeps mid-attempt to widen the race
/// window. The actual serialization point under test is
/// `Coordination::acquire`'s atomic `INSERT OR IGNORE` against the shared
/// repository lease, not a sleep-based guess.
#[test]
fn concurrent_agent_run_dispatch_admits_exactly_one_execution() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture_repo(dir.path());
    let state = dir.path().join("state");
    let invocation_log = dir.path().join("invocations.log");

    let mut agents = Agents::open(&state).unwrap();
    let task = agents.create(spec(&repo, "MODE:slow"), policy()).unwrap();
    let digest = task.assignment.digest().unwrap();
    let approved = agents
        .approve(&task.id, task.revision, &digest, "fixture")
        .unwrap();
    let task_id = approved.id.clone();
    let run_revision = approved.revision;
    drop(agents);

    let spawn_competitor = || {
        Command::new(env!("CARGO_BIN_EXE_dev-conductor"))
            .arg("--state")
            .arg(&state)
            .args([
                "agent-run",
                &task_id,
                "--revision",
                &run_revision.to_string(),
                "--json",
            ])
            .env("CONDUCTOR_TEST_INVOCATION_LOG", &invocation_log)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap()
    };
    // Launched back-to-back, with no sleep between them, so both race to
    // acquire the same repository lease for the same task id/revision -
    // deterministic contention, not a timing guess.
    let mut first = spawn_competitor();
    let mut second = spawn_competitor();

    let first_status = first.wait().unwrap();
    let second_status = second.wait().unwrap();
    let outcomes = [first_status.success(), second_status.success()];
    assert_eq!(
        outcomes.iter().filter(|ok| **ok).count(),
        1,
        "exactly one of the two competing agent-run processes must succeed, the other must be \
         safely rejected: {outcomes:?}"
    );

    // No duplicate agent invocation: the fixture appends one line per real
    // invocation it receives, regardless of which layer (lease or
    // worktree-exists check) would have stopped a second one.
    let invocations = fs::read_to_string(&invocation_log).unwrap();
    assert_eq!(
        invocations.lines().count(),
        1,
        "the fake agent must have been invoked exactly once: {invocations:?}"
    );

    // The winning attempt retains valid, uncorrupted evidence: it reached
    // AwaitingReview with a real execution receipt, a real passing
    // verification, and a patch - exactly as a single, uncontended run
    // would produce. A failed competing dispatch left no trace on it.
    let agents = Agents::open(&state).unwrap();
    let done = agents.task(&task_id).unwrap();
    assert_eq!(done.status, AgentStatus::AwaitingReview, "{done:?}");
    assert_eq!(done.execution.as_ref().unwrap().outcome, "succeeded");
    assert_eq!(done.verification.as_ref().unwrap().outcome, "succeeded");
    assert!(done.patch.is_some());
    assert!(done.error.is_none());
    // Revision 1 (created) -> 2 (approved) -> 3 (Running) -> 4 (execution
    // receipt stored) -> 5 (Verifying) -> 6 (verification receipt stored)
    // -> 7 (patch stored) -> 8 (finish/AwaitingReview): exactly the single
    // winning attempt's own transitions, nothing extra from the loser,
    // which never got far enough to mutate this task at all.
    assert_eq!(done.revision, 8, "{done:?}");

    // Repository leases remain consistent: released cleanly after the one
    // real attempt finished, not left held or duplicated.
    let leases = dev_conductor::coordination::Coordination::open(&state)
        .unwrap()
        .reservations()
        .unwrap();
    assert!(leases.is_empty(), "{leases:?}");
}

/// M10.3 Phase 3's required test: the full agent-originated crash-recovery
/// cycle, not just dead-owner detection and quarantine (already covered by
/// `real_process_kill_leaves_a_dead_owner_and_reconcile_recovers_without_replay`
/// above). Continues past quarantine through the explicit, human-labeled
/// acknowledgement every other engine's crashed lease already requires,
/// then proves a *separate, freshly approved* task can genuinely dispatch
/// against the same repository afterward - and that the original,
/// interrupted task is never replayed.
#[test]
fn acknowledging_a_quarantined_agent_lease_unblocks_a_fresh_dispatch_without_replaying_the_original()
 {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture_repo(dir.path());
    let state = dir.path().join("state");

    let mut agents = Agents::open(&state).unwrap();
    let crashed = agents.create(spec(&repo, "MODE:hang"), policy()).unwrap();
    let digest = crashed.assignment.digest().unwrap();
    let approved = agents
        .approve(&crashed.id, crashed.revision, &digest, "fixture")
        .unwrap();
    let crashed_id = approved.id.clone();
    let run_revision = approved.revision;
    drop(agents);

    let mut child = Command::new(env!("CARGO_BIN_EXE_dev-conductor"))
        .arg("--state")
        .arg(&state)
        .args([
            "agent-run",
            &crashed_id,
            "--revision",
            &run_revision.to_string(),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();

    let agents = Agents::open(&state).unwrap();
    let until = std::time::Instant::now() + std::time::Duration::from_secs(8);
    loop {
        let current = agents.task(&crashed_id).unwrap();
        if current.status == AgentStatus::Running && current.owner.is_some() {
            break;
        }
        assert!(std::time::Instant::now() < until, "{:?}", current.status);
        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    // 1-2: start a real agent-owning process, then kill it abruptly.
    child.kill().unwrap();
    child.wait().unwrap();

    // 3-4: reconcile the dead task; it becomes Unknown.
    let mut agents = Agents::open(&state).unwrap();
    let current = agents.task(&crashed_id).unwrap();
    let recovered = agents.reconcile(&crashed_id, current.revision).unwrap();
    assert_eq!(recovered.status, AgentStatus::Unknown, "{recovered:?}");

    // 5: the repository lease is quarantined, not released.
    let mut coordination = dev_conductor::coordination::Coordination::open(&state).unwrap();
    let leases = coordination.reservations().unwrap();
    assert_eq!(leases.len(), 1, "{leases:?}");
    assert_eq!(leases[0]["task_id"], crashed_id);
    assert_eq!(leases[0]["quarantined"], true, "{leases:?}");
    let resource = leases[0]["resource"].as_str().unwrap().to_string();

    // 6: new execution against the same repository is blocked while
    // quarantined - approve a second, genuinely distinct task first so the
    // only thing standing between it and dispatch is the lease.
    let fresh = agents
        .create(spec(&repo, "fix the result"), policy())
        .unwrap();
    let fresh_digest = fresh.assignment.digest().unwrap();
    let approved_fresh = agents
        .approve(&fresh.id, fresh.revision, &fresh_digest, "fixture")
        .unwrap();
    let blocked = agents
        .run(&approved_fresh.id, approved_fresh.revision)
        .unwrap_err();
    assert!(blocked.to_string().contains("busy"), "{blocked}");

    // 7: never automatically acknowledged merely because the owner looks
    // dead - only this explicit, human-labeled call releases it.
    coordination
        .acknowledge(&resource, "fixture operator", "process confirmed dead")
        .unwrap();
    let leases_after_ack = coordination.reservations().unwrap();
    assert!(leases_after_ack.is_empty(), "{leases_after_ack:?}");

    // 8: the same, now-unblocked fresh task genuinely executes against the
    // same repository and reaches AwaitingReview.
    let refreshed_fresh = agents.task(&approved_fresh.id).unwrap();
    let done = agents
        .run(&approved_fresh.id, refreshed_fresh.revision)
        .unwrap();
    assert_eq!(done.status, AgentStatus::AwaitingReview, "{done:?}");
    assert_eq!(done.execution.as_ref().unwrap().outcome, "succeeded");

    // Leases end up consistent again once the fresh attempt finishes.
    let leases_final = coordination.reservations().unwrap();
    assert!(leases_final.is_empty(), "{leases_final:?}");

    // 9: the original, interrupted task was never replayed - it still
    // sits at Unknown with its no-replay evidence, untouched by the fresh
    // dispatch that followed it.
    let original_untouched = agents.task(&crashed_id).unwrap();
    assert_eq!(original_untouched.status, AgentStatus::Unknown);
    assert!(
        original_untouched
            .error
            .as_deref()
            .unwrap()
            .contains("no replay")
    );
    assert!(
        agents
            .run(&crashed_id, original_untouched.revision)
            .is_err()
    );
}

/// M10.3 Phase 4's required regression: a workspace path deliberately
/// deepened well past the diagnostic threshold gets a retained warning
/// naming both the resolved worktree and its expected build-output
/// directory, without blocking the attempt - it still reaches
/// AwaitingReview normally. This is a heuristic, not a verifier-failure
/// reproduction: it does not need (and does not attempt) to actually
/// trigger a real Windows linker failure, only to prove the diagnostic
/// itself fires and is retained on the task.
#[cfg(windows)]
#[test]
fn long_workspace_paths_get_a_retained_non_blocking_warning() {
    let dir = tempfile::tempdir().unwrap();
    // Deepen the state root well past the diagnostic threshold without
    // approaching any real Windows path-length limit.
    let deep = dir
        .path()
        .join("a".repeat(20))
        .join("b".repeat(20))
        .join("c".repeat(20));
    fs::create_dir_all(&deep).unwrap();
    let repo = fixture_repo(&deep);
    let state = deep.join("state");

    let mut agents = Agents::open(&state).unwrap();
    let task = agents
        .create(spec(&repo, "fix the result"), policy())
        .unwrap();
    let digest = task.assignment.digest().unwrap();
    let approved = agents
        .approve(&task.id, task.revision, &digest, "fixture")
        .unwrap();
    let done = agents.run(&approved.id, approved.revision).unwrap();

    // Non-blocking: a long path alone does not stop the attempt.
    assert_eq!(done.status, AgentStatus::AwaitingReview, "{done:?}");
    let warning = done
        .workspace_warning
        .as_deref()
        .expect("a workspace path past the threshold must produce a retained warning");
    assert!(warning.contains("LNK1104"), "{warning}");
    assert!(
        warning.contains(&done.assignment.workspace.display().to_string()),
        "{warning}"
    );
    assert!(
        warning.contains(
            &done
                .assignment
                .workspace
                .join("target")
                .join("debug")
                .join("deps")
                .display()
                .to_string()
        ),
        "the expected build-output location must be named in the diagnostic: {warning}"
    );
}

/// The other half of the same contract: an ordinary, short workspace path
/// never gets a warning it doesn't need.
#[test]
fn short_workspace_paths_get_no_warning() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture_repo(dir.path());
    let state = dir.path().join("state");

    let mut agents = Agents::open(&state).unwrap();
    let task = agents
        .create(spec(&repo, "fix the result"), policy())
        .unwrap();
    let digest = task.assignment.digest().unwrap();
    let approved = agents
        .approve(&task.id, task.revision, &digest, "fixture")
        .unwrap();
    let done = agents.run(&approved.id, approved.revision).unwrap();
    assert_eq!(done.status, AgentStatus::AwaitingReview, "{done:?}");
    assert!(done.workspace_warning.is_none(), "{done:?}");
}

// M10.4 Phase 3/4: the Codex backend going through the identical
// lifecycle (create/approve/run/scope/verify/review, global stop,
// crash reconciliation, repository lease, concurrent-dispatch
// admission) the Cursor-backend tests above already prove - each test
// below mirrors one of those, swapped to `codex_spec`/`codex_policy`,
// proving the shared mechanism is genuinely backend-neutral rather than
// merely asserted to be from reading the code.

/// Required test 1/9/10: a successful Codex coding edit reaches
/// `AwaitingReview` with inspectable evidence, and is never auto-accepted.
#[test]
fn codex_coding_task_runs_to_awaiting_review_and_only_accepts_on_explicit_human_review() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture_repo(dir.path());
    let state = dir.path().join("state");

    let mut agents = Agents::open(&state).unwrap();
    let task = agents
        .create(codex_spec(&repo, "fix the result"), codex_policy())
        .unwrap();
    assert_eq!(task.status, AgentStatus::AwaitingApproval);

    let digest = task.assignment.digest().unwrap();
    let approved = agents
        .approve(&task.id, task.revision, &digest, "fixture reviewer")
        .unwrap();
    assert_eq!(approved.status, AgentStatus::Approved);

    let done = agents.run(&approved.id, approved.revision).unwrap();
    assert_eq!(done.status, AgentStatus::AwaitingReview, "{done:?}");
    assert!(done.review.is_none());
    assert!(done.patch.is_some());
    assert_eq!(done.execution.as_ref().unwrap().outcome, "succeeded");
    assert_eq!(done.verification.as_ref().unwrap().outcome, "succeeded");
    assert_eq!(
        fs::read_to_string(repo.join("src/result.txt")).unwrap(),
        "wrong",
        "the agent must edit only its own isolated worktree, never the registered repository"
    );

    let evidence = agents.evidence_digest(&done).unwrap();
    let reviewed = agents
        .review(&done.id, done.revision, &evidence, "fixture reviewer", true)
        .unwrap();
    assert_eq!(reviewed.status, AgentStatus::Accepted);
    assert!(reviewed.review.unwrap().accepted);
}

/// Required test 3: out-of-scope Codex edits are refused before
/// verification ever runs.
#[test]
fn codex_coding_edits_outside_scope_are_refused_before_verification() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture_repo(dir.path());
    let state = dir.path().join("state");

    let mut agents = Agents::open(&state).unwrap();
    let task = agents
        .create(codex_spec(&repo, "MODE:out_of_scope"), codex_policy())
        .unwrap();
    let digest = task.assignment.digest().unwrap();
    let approved = agents
        .approve(&task.id, task.revision, &digest, "fixture")
        .unwrap();
    let done = agents.run(&approved.id, approved.revision).unwrap();
    assert_eq!(done.status, AgentStatus::NeedsWork, "{done:?}");
    assert!(
        done.error
            .as_deref()
            .unwrap()
            .contains("outside approved files"),
        "{:?}",
        done.error
    );
    assert!(
        done.verification.is_none(),
        "verification must never run once scope is violated"
    );
}

/// Required test 2: a genuine independent-verification failure (the
/// agent's own process exits cleanly, but its edit is wrong) lands at
/// the correct non-accepted terminal state - proving verification is a
/// real, separate check, not a rubber stamp on the agent's own exit code.
#[test]
fn codex_coding_wrong_fix_fails_independent_verification_and_lands_at_needs_work() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture_repo(dir.path());
    let state = dir.path().join("state");

    let mut agents = Agents::open(&state).unwrap();
    let task = agents
        .create(codex_spec(&repo, "MODE:wrong"), codex_policy())
        .unwrap();
    let digest = task.assignment.digest().unwrap();
    let approved = agents
        .approve(&task.id, task.revision, &digest, "fixture")
        .unwrap();
    let done = agents.run(&approved.id, approved.revision).unwrap();
    assert_eq!(done.status, AgentStatus::NeedsWork, "{done:?}");
    assert_eq!(done.execution.as_ref().unwrap().outcome, "succeeded");
    assert_eq!(done.verification.as_ref().unwrap().outcome, "failed");
    assert!(
        done.error
            .as_deref()
            .unwrap()
            .contains("independent verification")
    );
}

/// Required test 4: missing/malformed agent protocol output never
/// crashes the engine or gets parsed for a verdict - it's retained
/// purely as evidence, and the real, independent verifier is still what
/// decides the outcome.
#[test]
fn codex_coding_malformed_protocol_output_is_handled_safely_without_crashing() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture_repo(dir.path());
    let state = dir.path().join("state");

    let mut agents = Agents::open(&state).unwrap();
    let task = agents
        .create(codex_spec(&repo, "MODE:malformed"), codex_policy())
        .unwrap();
    let digest = task.assignment.digest().unwrap();
    let approved = agents
        .approve(&task.id, task.revision, &digest, "fixture")
        .unwrap();
    let done = agents.run(&approved.id, approved.revision).unwrap();
    assert_eq!(done.status, AgentStatus::NeedsWork, "{done:?}");
    assert_eq!(done.execution.as_ref().unwrap().outcome, "succeeded");
    assert_eq!(done.verification.as_ref().unwrap().outcome, "failed");
}

/// Required test 5: a generation timeout terminates the real owned
/// process tree, not merely marks the task.
#[test]
fn codex_coding_generation_timeout_terminates_the_process_tree() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture_repo(dir.path());
    let state = dir.path().join("state");

    let mut agents = Agents::open(&state).unwrap();
    let mut hang_spec = codex_spec(&repo, "MODE:hang");
    hang_spec.generation_timeout_ms = 300;
    let task = agents.create(hang_spec, codex_policy()).unwrap();
    let digest = task.assignment.digest().unwrap();
    let approved = agents
        .approve(&task.id, task.revision, &digest, "fixture")
        .unwrap();
    let done = agents.run(&approved.id, approved.revision).unwrap();
    assert_eq!(done.status, AgentStatus::NeedsWork, "{done:?}");
    let execution = done.execution.as_ref().unwrap();
    assert_eq!(execution.outcome, "timed_out");
    assert!(
        execution.tree_terminated,
        "the real hung process tree must have been terminated, not left running"
    );
}

/// Required test 6: the real `global-stop` CLI entry point, from a
/// second, independent process, stops a genuinely running Codex
/// coding-agent process.
#[test]
fn real_global_stop_cli_terminates_a_genuinely_running_codex_coding_process() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture_repo(dir.path());
    let state = dir.path().join("state");

    let mut agents = Agents::open(&state).unwrap();
    let task = agents
        .create(codex_spec(&repo, "MODE:hang"), codex_policy())
        .unwrap();
    let digest = task.assignment.digest().unwrap();
    let approved = agents
        .approve(&task.id, task.revision, &digest, "fixture")
        .unwrap();
    let task_id = approved.id.clone();
    let run_revision = approved.revision;
    drop(agents);

    let mut runner = Command::new(env!("CARGO_BIN_EXE_dev-conductor"))
        .arg("--state")
        .arg(&state)
        .args([
            "agent-run",
            &task_id,
            "--revision",
            &run_revision.to_string(),
            "--json",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();

    std::thread::sleep(std::time::Duration::from_millis(300));
    let stop = Command::new(env!("CARGO_BIN_EXE_dev-conductor"))
        .arg("--state")
        .arg(&state)
        .args([
            "global-stop",
            "--by",
            "fixture",
            "--reason",
            "test",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        stop.status.success(),
        "{}",
        String::from_utf8_lossy(&stop.stderr)
    );
    let stop_outcome: serde_json::Value = serde_json::from_slice(&stop.stdout).unwrap();
    assert!(
        stop_outcome["cancelled"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v.as_str() == Some(task_id.as_str())),
        "{stop_outcome}"
    );

    let status = runner.wait().unwrap();
    assert!(status.success());

    let agents = Agents::open(&state).unwrap();
    let done = agents.task(&task_id).unwrap();
    assert_eq!(done.status, AgentStatus::Cancelled, "{done:?}");
    assert_eq!(done.execution.as_ref().unwrap().outcome, "cancelled");
}

/// Required test 7: dead-owner reconciliation quarantines the
/// repository lease for a Codex-backed task exactly like it already does
/// for Cursor - the same `Agents::reconcile`, unmodified.
#[test]
fn real_process_kill_leaves_a_dead_codex_owner_and_reconcile_recovers_without_replay() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture_repo(dir.path());
    let state = dir.path().join("state");

    let mut agents = Agents::open(&state).unwrap();
    let task = agents
        .create(codex_spec(&repo, "MODE:hang"), codex_policy())
        .unwrap();
    let digest = task.assignment.digest().unwrap();
    let approved = agents
        .approve(&task.id, task.revision, &digest, "fixture")
        .unwrap();
    let task_id = approved.id.clone();
    let run_revision = approved.revision;
    drop(agents);

    let mut child = Command::new(env!("CARGO_BIN_EXE_dev-conductor"))
        .arg("--state")
        .arg(&state)
        .args([
            "agent-run",
            &task_id,
            "--revision",
            &run_revision.to_string(),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();

    let agents = Agents::open(&state).unwrap();
    let until = std::time::Instant::now() + std::time::Duration::from_secs(8);
    let active = loop {
        let current = agents.task(&task_id).unwrap();
        if current.status == AgentStatus::Running && current.owner.is_some() {
            break current;
        }
        assert!(std::time::Instant::now() < until, "{:?}", current.status);
        std::thread::sleep(std::time::Duration::from_millis(5));
    };

    child.kill().unwrap();
    child.wait().unwrap();

    let mut agents = Agents::open(&state).unwrap();
    let current = agents.task(&task_id).unwrap();
    let recovered = agents.reconcile(&task_id, current.revision).unwrap();
    assert_eq!(recovered.status, AgentStatus::Unknown, "{recovered:?}");
    assert!(recovered.owner.is_none());
    assert!(recovered.error.as_deref().unwrap().contains("no replay"));

    let leases = dev_conductor::coordination::Coordination::open(&state)
        .unwrap()
        .reservations()
        .unwrap();
    assert_eq!(leases.len(), 1, "{leases:?}");
    assert_eq!(leases[0]["task_id"], task_id);
    assert_eq!(leases[0]["quarantined"], true, "{leases:?}");
    assert!(agents.run(&recovered.id, recovered.revision).is_err());
    let _ = active;
}

/// Required test 8: concurrent dispatch against a Codex-backed task
/// admits exactly one real execution, via the same repository lease
/// proven for Cursor in M10.3/M10.4 - not a Codex-specific mechanism.
#[test]
fn concurrent_codex_coding_run_dispatch_admits_exactly_one_execution() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture_repo(dir.path());
    let state = dir.path().join("state");
    let invocation_log = dir.path().join("invocations.log");

    let mut agents = Agents::open(&state).unwrap();
    let task = agents
        .create(codex_spec(&repo, "MODE:slow"), codex_policy())
        .unwrap();
    let digest = task.assignment.digest().unwrap();
    let approved = agents
        .approve(&task.id, task.revision, &digest, "fixture")
        .unwrap();
    let task_id = approved.id.clone();
    let run_revision = approved.revision;
    drop(agents);

    let spawn_competitor = || {
        Command::new(env!("CARGO_BIN_EXE_dev-conductor"))
            .arg("--state")
            .arg(&state)
            .args([
                "agent-run",
                &task_id,
                "--revision",
                &run_revision.to_string(),
                "--json",
            ])
            .env("CONDUCTOR_TEST_INVOCATION_LOG", &invocation_log)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap()
    };
    let mut first = spawn_competitor();
    let mut second = spawn_competitor();

    let first_status = first.wait().unwrap();
    let second_status = second.wait().unwrap();
    let outcomes = [first_status.success(), second_status.success()];
    assert_eq!(outcomes.iter().filter(|ok| **ok).count(), 1, "{outcomes:?}");

    let invocations = fs::read_to_string(&invocation_log).unwrap();
    assert_eq!(
        invocations.lines().count(),
        1,
        "the fake codex agent must have been invoked exactly once: {invocations:?}"
    );

    let agents = Agents::open(&state).unwrap();
    let done = agents.task(&task_id).unwrap();
    assert_eq!(done.status, AgentStatus::AwaitingReview, "{done:?}");
    assert_eq!(done.execution.as_ref().unwrap().outcome, "succeeded");
    assert_eq!(done.verification.as_ref().unwrap().outcome, "succeeded");

    let leases = dev_conductor::coordination::Coordination::open(&state)
        .unwrap()
        .reservations()
        .unwrap();
    assert!(leases.is_empty(), "{leases:?}");
}
