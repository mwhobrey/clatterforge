//! M10 Phase 0: dedicated regression coverage for every execution/
//! admission entry point against the global stop, found by auditing
//! `src/main.rs`, `src/service.rs`, `src/serve.rs`, `src/autonomy.rs` and
//! `src/inference/jobs.rs` directly rather than assuming any path is
//! covered because a sibling path using the same engine is. Before this
//! file, only `inference-run` (fixed during the M9 follow-up) had an
//! automated test; the legacy fake `run`, `orchestrate-run`, `board-run`/
//! `TaskService::run`, and `hold-my-beer` were all only ever manually
//! curl/CLI-verified per the runbook's own account, confirmed by `grep`
//! finding zero prior references to `GlobalStop`/`global-stop` anywhere
//! in `tests/`.

use dev_conductor::{
    autonomy::{self, ExperimentSpec, Policies},
    process::ExecutionPolicy,
    repository::git,
    service::TaskService,
    stop::GlobalStop,
    store::Store,
};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
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

#[test]
fn legacy_fake_run_is_blocked_by_global_stop() {
    let dir = TempDir::new().unwrap();
    let repo = fixture_repo(dir.path());
    let state = dir.path().join("state");

    let registered = cli(&state, &["register", repo.to_str().unwrap()]);
    assert!(registered.status.success());
    let registration: serde_json::Value = serde_json::from_slice(&registered.stdout).unwrap();
    let registration_id = registration["id"].as_str().unwrap();

    let prepared = cli(
        &state,
        &[
            "create",
            registration_id,
            "objective",
            "--acceptance",
            "a",
            "--contents",
            "x",
            "--timeout-ms",
            "5000",
            "--verification-timeout-ms",
            "5000",
        ],
    );
    assert!(
        prepared.status.success(),
        "{}",
        String::from_utf8_lossy(&prepared.stderr)
    );
    let prepared: serde_json::Value = serde_json::from_slice(&prepared.stdout).unwrap();
    let task_id = prepared["task"]["id"].as_str().unwrap();
    let revision = prepared["task"]["revision"].as_u64().unwrap();
    let digest = prepared["assignment_digest"].as_str().unwrap();

    let approved = cli(
        &state,
        &[
            "approve",
            task_id,
            "--revision",
            &revision.to_string(),
            "--digest",
            digest,
            "--by",
            "fixture",
        ],
    );
    assert!(approved.status.success());
    let approved: serde_json::Value = serde_json::from_slice(&approved.stdout).unwrap();
    let run_revision = approved["task"]["revision"].as_u64().unwrap();

    assert!(
        cli(
            &state,
            &[
                "global-stop",
                "--by",
                "fixture",
                "--reason",
                "phase 0 audit"
            ]
        )
        .status
        .success()
    );

    let blocked = cli(
        &state,
        &["run", task_id, "--revision", &run_revision.to_string()],
    );
    assert!(
        !blocked.status.success(),
        "the legacy fake-engine run command must refuse admission while global stop is engaged"
    );
    assert!(String::from_utf8_lossy(&blocked.stderr).contains("global stop"));
}

#[test]
fn orchestrate_run_is_blocked_by_global_stop() {
    // orchestrate-run's check lives in main.rs's own command handler, not
    // inside `Orchestrator::run` itself - confirmed by reading the code,
    // reconfirmed here by actually engaging stop through the CLI and
    // dispatching through the CLI, not by calling the library function
    // directly (which would silently skip exactly the layer being
    // audited).
    let dir = TempDir::new().unwrap();
    let repo = fixture_repo(dir.path());
    let state = dir.path().join("state");
    let verifier = Command::new("rustc")
        .args(["tests/fixtures/orchestration_verifier.rs", "-o"])
        .arg(dir.path().join("verifier.exe"))
        .status()
        .unwrap();
    assert!(verifier.success());

    fs::write(repo.join("src.txt"), "wrong\n").unwrap();
    fs::write(repo.join("verify.txt"), "correct\n").unwrap();
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
            "add files",
        ],
    )
    .unwrap();

    let spec = serde_json::json!({
        "repository": repo.to_str().unwrap(),
        "objective": "irrelevant: this run is expected to be refused before dispatch",
        "acceptance": "n/a",
        "allowed_files": ["src.txt"],
        "context_files": [],
        "verification_files": ["verify.txt"],
        "verify_executable": dir.path().join("verifier.exe").to_str().unwrap(),
        "verify_args": [],
        "target": "native",
        "max_rounds": 1,
        "max_output_tokens": 128,
        "generation_timeout_ms": 5000,
        "verification_timeout_ms": 5000,
    });
    let config = serde_json::json!({
        "schema_version": 1,
        "default_target": "native",
        "hardware_id": "fixture",
        "admission_db": dir.path().join("admission.sqlite").to_str().unwrap(),
        "max_active_per_resource": 1,
        "registry_max_age_ms": 60000,
        "fallback_before_dispatch": false,
        "targets": [{
            "id": "native", "provider": "ollama", "endpoint": "http://127.0.0.1:1/",
            "model": "fixture", "expected_artifact_digest": null,
            "context_limit": 32768, "output_limit": 128,
            "capabilities": {"streaming": true, "tools": false, "required_tool_choice": false,
                "parallel_tools": false, "structured_output": false, "media": false, "reasoning_control": false},
            "capability_source": "fixture", "priority": 0, "resource_id": "fixture-gpu"
        }]
    });
    let spec_path = dir.path().join("spec.json");
    let config_path = dir.path().join("config.json");
    fs::write(&spec_path, serde_json::to_vec(&spec).unwrap()).unwrap();
    fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();

    let prepared = cli(
        &state,
        &[
            "orchestrate-prepare",
            "--spec",
            spec_path.to_str().unwrap(),
            "--config",
            config_path.to_str().unwrap(),
        ],
    );
    assert!(
        prepared.status.success(),
        "{}",
        String::from_utf8_lossy(&prepared.stderr)
    );
    let prepared: serde_json::Value = serde_json::from_slice(&prepared.stdout).unwrap();
    let task_id = prepared["task"]["id"].as_str().unwrap();
    let revision = prepared["task"]["revision"].as_u64().unwrap();
    let digest = prepared["assignment_digest"].as_str().unwrap();
    let approved = cli(
        &state,
        &[
            "orchestrate-approve",
            task_id,
            "--revision",
            &revision.to_string(),
            "--digest",
            digest,
            "--by",
            "fixture",
        ],
    );
    assert!(
        approved.status.success(),
        "{}",
        String::from_utf8_lossy(&approved.stderr)
    );
    let approved: serde_json::Value = serde_json::from_slice(&approved.stdout).unwrap();
    let run_revision = approved["task"]["revision"].as_u64().unwrap();

    assert!(
        cli(
            &state,
            &[
                "global-stop",
                "--by",
                "fixture",
                "--reason",
                "phase 0 audit"
            ]
        )
        .status
        .success()
    );

    let blocked = cli(
        &state,
        &[
            "orchestrate-run",
            task_id,
            "--revision",
            &run_revision.to_string(),
        ],
    );
    assert!(
        !blocked.status.success(),
        "orchestrate-run must refuse admission while global stop is engaged"
    );
    assert!(String::from_utf8_lossy(&blocked.stderr).contains("global stop"));
}

#[test]
fn task_service_run_board_runs_shared_path_is_blocked_by_global_stop() {
    // board-run/HTTP post_run both call `TaskService::run` directly -
    // this exercises that exact shared choke point, in-process, without
    // assuming a CLI-level wrapper duplicates the check the way the two
    // legacy paths above need.
    let dir = TempDir::new().unwrap();
    let repo = fixture_repo(dir.path());
    let root = dir.path().join("state");
    let mut store = Store::open(&root).unwrap();
    let (registration_id, _) = store.register(&repo).unwrap();
    drop(store);

    let mut service = TaskService::open(&root).unwrap();
    let detail = service
        .create_fake(
            &registration_id,
            "objective".into(),
            "acceptance".into(),
            "x".into(),
            16384,
            execution_policy(),
        )
        .unwrap();
    let approved = service
        .approve(
            &detail.id,
            detail.revision,
            &detail.assignment_digest,
            "fixture",
        )
        .unwrap();

    let mut stop = GlobalStop::open(&root).unwrap();
    stop.engage("fixture", "phase 0 audit").unwrap();

    match service.run(&approved.id, approved.revision) {
        Err(error) => assert!(error.to_string().contains("global stop"), "{error}"),
        Ok(_) => panic!(
            "board-run's shared TaskService::run path must refuse admission while global stop is engaged"
        ),
    }
}

#[test]
fn hold_my_beer_is_blocked_by_global_stop_not_merely_inheriting_it_by_assumption() {
    // Exercises `hold_my_beer::run` end to end, not just the
    // `TaskService::run` call it happens to make internally - per M10's
    // own instruction not to assume safety transitively.
    let dir = TempDir::new().unwrap();
    let repo = fixture_repo(dir.path());
    let root = dir.path().join("state");
    let mut store = Store::open(&root).unwrap();
    let (registration_id, _) = store.register(&repo).unwrap();
    drop(store);

    let mut policies = Policies::open(&root).unwrap();
    let policy = policies
        .create(
            &registration_id,
            "fixture",
            5,
            dev_conductor::now() + 60_000,
            vec![ExperimentSpec {
                objective: "objective".into(),
                acceptance: "acceptance".into(),
                contents: "x".into(),
            }],
        )
        .unwrap();

    let mut stop = GlobalStop::open(&root).unwrap();
    stop.engage("fixture", "phase 0 audit").unwrap();

    let error = autonomy::run(&root, &policy.id, execution_policy(), 16384).unwrap_err();
    assert!(error.to_string().contains("global stop"), "{error}");

    // Pre-existing (M7-era) behavior, not a global-stop-specific bug:
    // `mark_dispatched` happens at task-creation time, before `run()` is
    // ever attempted, so *any* infrastructure-level `run()` failure -
    // global stop included - leaves the experiment "dispatched" with a
    // real task_id, while the task itself sits untouched in `Approved`
    // (global stop's check is the very first line of `TaskService::run`,
    // before anything is mutated). A future `hold-my-beer` call will not
    // retry this experiment; a human has to find and run that task
    // directly (e.g. `board-run`) after resuming. No bypass, no
    // corruption, no silent loss of the task - but worth documenting
    // plainly rather than assuming "dispatched" means "ran."
    let after = policies.get(&policy.id).unwrap();
    let task_id = after.experiments[0]
        .task_id
        .clone()
        .expect("mark_dispatched already ran before the blocked run() call");
    let service = TaskService::open(&root).unwrap();
    let detail = service.detail(&task_id, None).unwrap();
    assert_eq!(detail.status, "approved");
}
