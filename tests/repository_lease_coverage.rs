//! M10.4 Phase 1: dedicated regression coverage proving the repository
//! lease (`coordination::Coordination`) - not merely global stop,
//! already covered by `tests/global_stop_coverage.rs` - is now enforced
//! identically by every supported dispatch entry point. Before this
//! milestone, the legacy single-engine `run`/`orchestrate-run` CLI
//! commands checked global stop but never acquired a repository lease
//! (`docs/runbook/04_CURRENT_STATE.md`'s own documented gap) - confirmed
//! by direct code reading, not assumed. `board-run`/HTTP/Hold My Beer
//! already went through `TaskService::run`'s existing lease and are
//! unchanged; `agent-run` already manages its own lease internally and
//! is deliberately not double-wrapped - both are out of scope here.
//! Agent-engine same-repository concurrency is already covered by
//! M10.3's `concurrent_agent_run_dispatch_admits_exactly_one_execution`
//! (`tests/agents.rs`) and is not re-proven here. Cross-engine
//! `orchestrate-run` vs `board-run` contention lives in
//! `tests/orchestration.rs` instead, since it needs that file's own
//! real-HTTP-mock generation harness.

use dev_conductor::{
    coordination::Coordination,
    process::ExecutionPolicy,
    repository::git,
    store::{FakeOutcome, Store},
};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};
use tempfile::TempDir;

fn fixture_repo(parent: &Path) -> PathBuf {
    let repo = parent.join("repo");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]).unwrap();
    git(&repo, &["config", "core.autocrlf", "false"]).unwrap();
    fs::write(repo.join("README.md"), "fixture\n").unwrap();
    fs::write(repo.join("AGENTS.md"), "fixture\n").unwrap();
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
    repo
}

fn execution_policy() -> ExecutionPolicy {
    let mut policy = ExecutionPolicy::new(Path::new(env!("CARGO_BIN_EXE_dev-conductor"))).unwrap();
    policy.timeout_ms = 5000;
    policy.verification_timeout_ms = 5000;
    policy
}

fn cli(state: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_dev-conductor"))
        .arg("--state")
        .arg(state)
        .args(args)
        .output()
        .unwrap()
}

/// Creates one fake-engine task against `repository_id`, revises it to
/// the requested `outcome` (also exercising the only path that lets a
/// test pick a non-default outcome), and approves it. Returns the
/// resulting (id, revision) pair ready for `run`.
fn create_and_approve(
    store: &mut Store,
    repository_id: &str,
    outcome: FakeOutcome,
) -> (String, u64) {
    let task = store
        .create(
            repository_id,
            "objective".into(),
            "acceptance".into(),
            "x".into(),
            4096,
            execution_policy(),
        )
        .unwrap();
    let revised = store
        .revise(&task.id, task.revision, "x".into(), outcome)
        .unwrap();
    let digest = revised.assignment.digest().unwrap();
    let approved = store
        .approve(&revised.id, revised.revision, &digest, "fixture")
        .unwrap();
    (approved.id, approved.revision)
}

/// Waits until the dispatched task has genuinely reached `Running` (not
/// merely until the repository lease is held, which happens slightly
/// earlier, before `Store::run`'s own internal attempt bookkeeping - a
/// `cancel` issued before that point is correctly refused with "no
/// active attempt to cancel", not a race this test should paper over
/// with a blind sleep).
fn await_running(state: &Path, id: &str) {
    let until = Instant::now() + Duration::from_secs(8);
    loop {
        let current = Store::open(state).unwrap().task(id).unwrap();
        if current.status == dev_conductor::store::Status::Running {
            return;
        }
        assert!(
            Instant::now() < until,
            "task never reached Running: {:?}",
            current.status
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Required test 1: two legacy `run` processes, two different tasks,
/// the same registered repository - only one may execute at a time.
#[test]
fn two_legacy_run_processes_racing_the_same_repository_admit_exactly_one() {
    let dir = TempDir::new().unwrap();
    let repo = fixture_repo(dir.path());
    let state = dir.path().join("state");
    let mut store = Store::open(&state).unwrap();
    let (repository_id, _) = store.register(&repo).unwrap();
    let (hang_id, hang_rev) = create_and_approve(&mut store, &repository_id, FakeOutcome::Hang);
    let (other_id, other_rev) =
        create_and_approve(&mut store, &repository_id, FakeOutcome::Success);
    drop(store);

    let mut first = Command::new(env!("CARGO_BIN_EXE_dev-conductor"))
        .arg("--state")
        .arg(&state)
        .args(["run", &hang_id, "--revision", &hang_rev.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    await_running(&state, &hang_id);

    let blocked = cli(
        &state,
        &["run", &other_id, "--revision", &other_rev.to_string()],
    );
    assert!(
        !blocked.status.success(),
        "a second legacy run against the same repository must be refused while the first holds its lease"
    );
    assert!(
        String::from_utf8_lossy(&blocked.stderr).contains("busy"),
        "{}",
        String::from_utf8_lossy(&blocked.stderr)
    );

    // Clean up the hung attempt and confirm the lease is then free again.
    let hung = Store::open(&state).unwrap().task(&hang_id).unwrap();
    let cancelled = cli(
        &state,
        &[
            "cancel",
            &hang_id,
            "--revision",
            &hung.revision.to_string(),
            "--by",
            "fixture",
        ],
    );
    assert!(
        cancelled.status.success(),
        "{}",
        String::from_utf8_lossy(&cancelled.stderr)
    );
    // Unlike `agent-run`, the legacy `run` command's own existing
    // `ensure!` (pre-dating this milestone) treats Cancelled the same as
    // Failed/TimedOut/Unknown - a non-zero exit. Only the lease release
    // that follows is actually under test here.
    assert!(!first.wait().unwrap().success());
    assert!(
        Coordination::open(&state)
            .unwrap()
            .reservations()
            .unwrap()
            .is_empty()
    );

    let now_unblocked = cli(
        &state,
        &["run", &other_id, "--revision", &other_rev.to_string()],
    );
    assert!(
        now_unblocked.status.success(),
        "{}",
        String::from_utf8_lossy(&now_unblocked.stderr)
    );
}

/// Required test 5: a failed (not hung/cancelled) legacy dispatch must
/// not leave its repository lease held.
#[test]
fn a_failed_legacy_run_dispatch_does_not_leak_a_lease() {
    let dir = TempDir::new().unwrap();
    let repo = fixture_repo(dir.path());
    let state = dir.path().join("state");
    let mut store = Store::open(&state).unwrap();
    let (repository_id, _) = store.register(&repo).unwrap();
    let (failing_id, failing_rev) =
        create_and_approve(&mut store, &repository_id, FakeOutcome::FailAfterWrite);
    let (other_id, other_rev) =
        create_and_approve(&mut store, &repository_id, FakeOutcome::Success);
    drop(store);

    // The fake engine's own business failure (not a process crash) makes
    // `run` exit non-zero - expected, not itself the thing under test.
    let failed = cli(
        &state,
        &["run", &failing_id, "--revision", &failing_rev.to_string()],
    );
    assert!(!failed.status.success());

    assert!(
        Coordination::open(&state)
            .unwrap()
            .reservations()
            .unwrap()
            .is_empty(),
        "a failed dispatch must release its repository lease, not leak it"
    );

    // A second, independent task against the same repository can now
    // genuinely dispatch - proof the lease is actually free, not merely
    // unobserved.
    let unblocked = cli(
        &state,
        &["run", &other_id, "--revision", &other_rev.to_string()],
    );
    assert!(
        unblocked.status.success(),
        "{}",
        String::from_utf8_lossy(&unblocked.stderr)
    );
}

/// Required test 6: an interrupted owner of a legacy-`run`-acquired
/// lease still follows the existing, unmodified recovery contract -
/// `board-leases-acknowledge`'s own owner-liveness check, not a new
/// mechanism invented for this milestone.
#[test]
fn an_interrupted_legacy_run_owner_lease_recovers_through_the_existing_acknowledge_mechanism() {
    let dir = TempDir::new().unwrap();
    let repo = fixture_repo(dir.path());
    let state = dir.path().join("state");
    let mut store = Store::open(&state).unwrap();
    let (repository_id, _) = store.register(&repo).unwrap();
    let (hang_id, hang_rev) = create_and_approve(&mut store, &repository_id, FakeOutcome::Hang);
    let (other_id, other_rev) =
        create_and_approve(&mut store, &repository_id, FakeOutcome::Success);
    drop(store);

    let mut child = Command::new(env!("CARGO_BIN_EXE_dev-conductor"))
        .arg("--state")
        .arg(&state)
        .args(["run", &hang_id, "--revision", &hang_rev.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    await_running(&state, &hang_id);

    // Abrupt kill of the owning process itself - no graceful unwind, so
    // `LeaseReservation::drop`'s panic-quarantine path never runs either.
    // The lease is left simply *held*, exactly as a crashed `board-run`
    // dispatch already left one before this milestone.
    child.kill().unwrap();
    child.wait().unwrap();

    let leases = Coordination::open(&state).unwrap().reservations().unwrap();
    assert_eq!(leases.len(), 1, "{leases:?}");
    let resource = leases[0]["resource"].as_str().unwrap().to_string();

    // A fresh dispatch against the same repository is correctly refused
    // while the dead owner's lease is still held.
    let blocked = cli(
        &state,
        &["run", &other_id, "--revision", &other_rev.to_string()],
    );
    assert!(!blocked.status.success());
    assert!(String::from_utf8_lossy(&blocked.stderr).contains("busy"));

    // The existing, unmodified acknowledge mechanism recovers it once the
    // owner is confirmed dead - it does not require a prior "quarantine"
    // marking, since `Store::reconcile` (unlike `Agents::reconcile`) never
    // touches `Coordination` at all; `acknowledge` itself already checks
    // owner liveness independently.
    let acknowledged = cli(
        &state,
        &[
            "board-leases-acknowledge",
            "--resource",
            &resource,
            "--by",
            "fixture operator",
            "--reason",
            "process confirmed dead",
        ],
    );
    assert!(
        acknowledged.status.success(),
        "{}",
        String::from_utf8_lossy(&acknowledged.stderr)
    );
    assert!(
        Coordination::open(&state)
            .unwrap()
            .reservations()
            .unwrap()
            .is_empty()
    );

    // The now-unblocked task genuinely dispatches.
    let unblocked = cli(
        &state,
        &["run", &other_id, "--revision", &other_rev.to_string()],
    );
    assert!(
        unblocked.status.success(),
        "{}",
        String::from_utf8_lossy(&unblocked.stderr)
    );

    // The interrupted task itself was never replayed by any of this -
    // it still sits exactly where the crash left it until a human
    // explicitly reconciles it.
    let crashed = Store::open(&state).unwrap().task(&hang_id).unwrap();
    assert_eq!(crashed.status, dev_conductor::store::Status::Running);
}
