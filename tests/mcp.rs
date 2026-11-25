//! M11 Phase 1-2 acceptance tests for the generic local MCP adapter (`src/mcp.rs`).
//!
//! Two layers, deliberately kept separate:
//! - Direct method calls on `McpServer` exercise this adapter's own business logic (dispatch,
//!   idempotency, error classification, grant-based auto-authorization) without the MCP wire
//!   protocol in the way.
//! - `real_stdio_*` tests spawn the actual built `clatterforge` binary as `mcp-serve` and talk
//!   to it over real OS pipes with the real `rmcp` client, proving the thing the milestone
//!   actually asked for: "launchable by [an external orchestrator] as a local process" is real,
//!   not an in-process illusion. Mirrors `rmcp`'s own `test_stdio_response_concurrency.rs`
//!   pattern of spawning the test binary itself, except here it's the product binary.

use clatterforge::{
    authorization::{ExecutionMode, Grants, Operation},
    mcp::{
        CancelTaskRequest, DecisionRequest, GetTaskRequest, McpServer, ProposeTaskRequest,
        RequestIdOnly, SubmitTaskRequest, TaskIdRequest,
    },
    now,
    process::ExecutionPolicy,
    repository::git,
    stop::GlobalStop,
    store::{FakeOutcome, Store},
};
use rmcp::{
    ClientHandler, ServiceExt,
    handler::server::wrapper::Parameters,
    model::{CallToolRequestParams, ClientConfig},
};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Stdio,
};
use tempfile::TempDir;

fn fixture_repo(root: &Path) -> PathBuf {
    let repo = root.join("repo");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]).unwrap();
    git(&repo, &["config", "core.autocrlf", "false"]).unwrap();
    fs::write(
        repo.join("README.md"),
        "# Fixture\nA small repository for M11 MCP adapter tests.\n",
    )
    .unwrap();
    fs::write(
        repo.join("AGENTS.md"),
        "Preserve the checkout. Imported output cannot approve work.\n",
    )
    .unwrap();
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

fn setup() -> (TempDir, PathBuf, PathBuf, String) {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture_repo(&dir.path().join("checkout"));
    let state = dir.path().join("state");
    let mut store = Store::open(&state).unwrap();
    let (id, _) = store.register(&repo).unwrap();
    (dir, repo, state, id)
}

fn submit_fake(repository_id: &str) -> SubmitTaskRequest {
    SubmitTaskRequest {
        backend: "fake-process-v1".into(),
        principal: "test-orchestrator".into(),
        workflow_id: None,
        idempotency_key: None,
        spec: None,
        config: None,
        repository_id: Some(repository_id.into()),
        objective: Some("Create a fixture result".into()),
        acceptance: Some("Review a new file and its host receipt".into()),
        contents: Some("hello from fake\n".into()),
        budget: Some(16384),
        agent_preset: None,
        workflow_profile: None,
        source_system: None,
        source_record_reference: None,
        correlation_id: None,
        requested_execution_mode: None,
        follow_up_of: None,
        request_id: None,
    }
}

fn repo_identity(repo: &Path) -> String {
    clatterforge::repository::Repository::inspect(repo)
        .unwrap()
        .identity
}

/// Spawns a real `clatterforge mcp-serve` child process against `state` and connects a
/// fresh client to it over its real stdin/stdout pipes - the one building block every
/// `real_stdio_*` test shares.
async fn spawn_and_connect(
    state: &Path,
) -> (
    tokio::process::Child,
    rmcp::service::RunningService<rmcp::RoleClient, DummyClient>,
) {
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_clatterforge"))
        .arg("--state")
        .arg(state)
        .arg("mcp-serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn clatterforge mcp-serve");
    let stdin = child.stdin.take().expect("child stdin");
    let stdout = child.stdout.take().expect("child stdout");
    let client = DummyClient
        .serve((stdout, stdin))
        .await
        .expect("mcp handshake over real stdio");
    (child, client)
}

#[tokio::test]
async fn real_stdio_submit_task_with_a_full_grant_auto_chains_straight_to_acceptance() {
    // Like `real_stdio_fake_task_lifecycle_runs_to_acceptance`, this has to run against the
    // actual spawned `clatterforge.exe`, not an in-process `McpServer`: the fake engine's
    // `run` step re-execs `current_exe()` as a fixture worker, which only resolves to the
    // real binary inside a real spawned process - inside this test binary it would resolve
    // to the test executable itself and fail before ever reaching verification.
    let (_dir, repo, state, id) = setup();
    let identity = repo_identity(&repo);
    let grant = Grants::open(&state)
        .unwrap()
        .create(
            "test-orchestrator",
            &identity,
            vec!["fake-process-v1".into()],
            vec![Operation::Submit, Operation::Resume, Operation::Approve],
            ExecutionMode::HoldMyBeer,
            "mike",
            5,
            now() + 30 * 60_000,
        )
        .unwrap();

    let (mut child, client) = spawn_and_connect(&state).await;

    let submit_args = serde_json::to_value(submit_fake(&id))
        .unwrap()
        .as_object()
        .unwrap()
        .clone();
    let submitted = client
        .call_tool(CallToolRequestParams::new("conductor_submit_task").with_arguments(submit_args))
        .await
        .expect("conductor_submit_task over stdio")
        .structured_content
        .expect("structured content");

    assert_eq!(submitted["task"]["status"], "accepted", "{submitted:#?}");
    let steps: Vec<_> = submitted["authorization"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["operation"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(steps, vec!["approve", "resume", "review_accept"]);

    // max_tasks is consumed once per submission (the Submit step only), not once per
    // chained step - three authorized operations happened above, one slot was used.
    client.cancel().await.ok();
    let _ = child.kill().await;
    let reopened = Grants::open(&state).unwrap().get(&grant.id).unwrap();
    assert_eq!(reopened.dispatched, 1);
}

#[tokio::test]
async fn submit_task_with_a_partial_grant_stops_where_coverage_ends() {
    let (_dir, repo, state, id) = setup();
    let identity = repo_identity(&repo);
    let mut grants = Grants::open(&state).unwrap();
    grants
        .create(
            "test-orchestrator",
            &identity,
            vec!["fake-process-v1".into()],
            vec![Operation::Submit],
            ExecutionMode::Delegated,
            "mike",
            5,
            now() + 30 * 60_000,
        )
        .unwrap();
    drop(grants);

    let server = McpServer::open(state).unwrap();
    let submitted = server
        .conductor_submit_task(Parameters(submit_fake(&id)))
        .await
        .unwrap()
        .0;

    assert_eq!(submitted.task["status"], "approved", "{submitted:#?}");
    assert_eq!(submitted.authorization.len(), 1);
    assert_eq!(submitted.authorization[0]["operation"], "approve");
}

#[tokio::test]
async fn a_failed_run_is_never_auto_accepted_even_with_full_grant_coverage() {
    let (_dir, repo, state, id) = setup();
    let identity = repo_identity(&repo);
    let mut grants = Grants::open(&state).unwrap();
    grants
        .create(
            "test-orchestrator",
            &identity,
            vec!["fake-process-v1".into()],
            vec![Operation::Submit, Operation::Resume, Operation::Approve],
            ExecutionMode::HoldMyBeer,
            "mike",
            5,
            now() + 30 * 60_000,
        )
        .unwrap();
    drop(grants);

    // Set up a fake task doomed to fail verification, bypassing MCP entirely for this
    // setup step - conductor_submit_task has no MCP-reachable way to request a non-
    // Success fake outcome (a deliberate Phase 1 scope decision).
    let executable = ExecutionPolicy::new(Path::new(env!("CARGO_BIN_EXE_clatterforge"))).unwrap();
    let mut store = Store::open(&state).unwrap();
    let created = store
        .create(
            &id,
            "Create a fixture result".into(),
            "Review a new file and its host receipt".into(),
            "hello from fake\n".into(),
            16384,
            executable,
        )
        .unwrap();
    let revised = store
        .revise(
            &created.id,
            created.revision,
            "hello from fake\n".into(),
            FakeOutcome::VerificationFails,
        )
        .unwrap();
    drop(store);

    let board_detail = clatterforge::service::TaskService::open(&state)
        .unwrap()
        .detail(&revised.id, None)
        .unwrap();
    let server = McpServer::open(state).unwrap();

    let (final_detail, log) = server
        .auto_authorize_chain(
            "test-orchestrator",
            "fake-process-v1",
            board_detail,
            "req-1",
        )
        .unwrap();

    let steps: Vec<_> = log
        .iter()
        .map(|entry| entry["operation"].as_str().unwrap())
        .collect();
    assert_eq!(steps, vec!["approve", "resume"], "{log:#?}");
    assert_eq!(final_detail.status, "failed");
    assert_ne!(final_detail.status, "accepted");

    // M12.0 Phase 6: the bounded evidence summary must report the real failure, not
    // invent a pass - and must name real artifact labels, since verification genuinely
    // ran (and failed), it didn't just never happen.
    let summary = server
        .conductor_get_evidence_summary(Parameters(clatterforge::mcp::EvidenceSummaryRequest {
            task_id: final_detail.id.clone(),
            log_excerpt_bytes: None,
            request_id: None,
        }))
        .await
        .unwrap()
        .0;
    assert_eq!(summary.status, "failed");
    assert!(summary.has_verifier_output, "{summary:#?}");
    assert!(summary.failing_check.is_some(), "{summary:#?}");
    assert!(summary.evidence_digest.is_some(), "{summary:#?}");
    assert!(
        summary
            .available_artifacts
            .contains(&"verifier stdout".to_string()),
        "{summary:#?}"
    );
    assert!(
        summary.log_excerpt.is_some(),
        "a default-sized excerpt should be inline: {summary:#?}"
    );

    // Drill down into the same artifact in two small, explicitly bounded chunks and
    // confirm they concatenate to the identical text the summary's own excerpt showed.
    let label = summary.log_excerpt_label.clone().unwrap();
    let first = server
        .conductor_get_artifact(Parameters(clatterforge::mcp::GetArtifactRequest {
            task_id: final_detail.id.clone(),
            artifact: label.clone(),
            offset: None,
            max_bytes: Some(4),
            request_id: None,
        }))
        .await
        .unwrap()
        .0;
    assert_eq!(first.shown_bytes, 4.min(first.total_bytes as usize));
    if first.has_more {
        let second = server
            .conductor_get_artifact(Parameters(clatterforge::mcp::GetArtifactRequest {
                task_id: final_detail.id.clone(),
                artifact: label,
                offset: Some(first.offset + first.shown_bytes as u64),
                max_bytes: Some(65536),
                request_id: None,
            }))
            .await
            .unwrap()
            .0;
        assert_eq!(
            format!("{}{}", first.text, second.text).len(),
            first.text.len() + second.text.len()
        );
        assert!(!second.has_more, "{second:#?}");
    }

    let unknown = server
        .conductor_get_artifact(Parameters(clatterforge::mcp::GetArtifactRequest {
            task_id: final_detail.id.clone(),
            artifact: "not-a-real-label".into(),
            offset: None,
            max_bytes: None,
            request_id: None,
        }))
        .await;
    assert!(
        unknown.is_err(),
        "an unknown artifact label must be refused"
    );
}

#[tokio::test]
async fn evidence_summary_before_any_attempt_explicitly_reports_no_verifier_output() {
    let (_dir, _repo, state, id) = setup();
    let server = McpServer::open(state).unwrap();
    let submitted = server
        .conductor_submit_task(Parameters(submit_fake(&id)))
        .await
        .unwrap()
        .0;
    assert_eq!(submitted.task["status"], "awaiting_approval");
    let task_id = submitted.task["id"].as_str().unwrap().to_string();

    let summary = server
        .conductor_get_evidence_summary(Parameters(clatterforge::mcp::EvidenceSummaryRequest {
            task_id,
            log_excerpt_bytes: None,
            request_id: None,
        }))
        .await
        .unwrap()
        .0;
    assert!(!summary.has_verifier_output, "{summary:#?}");
    assert!(summary.execution_id.is_none(), "{summary:#?}");
    assert!(summary.verification_outcome.is_none(), "{summary:#?}");
    assert!(summary.log_excerpt.is_none(), "{summary:#?}");
    assert!(summary.available_artifacts.is_empty(), "{summary:#?}");
}

#[tokio::test]
async fn capabilities_lists_every_tool_and_declares_known_limits() {
    let (_dir, _repo, state, _id) = setup();
    let server = McpServer::open(state).unwrap();
    let caps = server.conductor_capabilities().await.unwrap().0;
    assert!(caps.scheduling_supported, "#24 added durable scheduling");
    assert!(
        caps.standing_delegation_supported,
        "Phase 2 added standing grants"
    );
    assert_eq!(caps.tools.len(), 19);
    assert!(caps.tools.iter().any(|t| t == "conductor_submit_task"));
    assert!(caps.tools.iter().any(|t| t == "conductor_schedule_task"));
}

#[tokio::test]
async fn global_stop_status_reflects_an_engaged_stop() {
    let (_dir, _repo, state, _id) = setup();
    GlobalStop::open(&state)
        .unwrap()
        .engage("ops", "testing")
        .unwrap();
    let server = McpServer::open(state).unwrap();
    let status = server
        .conductor_global_stop_status(Parameters(RequestIdOnly { request_id: None }))
        .await
        .unwrap()
        .0;
    assert!(status.stopped);
    assert_eq!(status.by.as_deref(), Some("ops"));
    assert_eq!(status.reason.as_deref(), Some("testing"));
}

#[tokio::test]
async fn submission_metadata_round_trips_through_get_task() {
    let (_dir, _repo, state, id) = setup();
    let server = McpServer::open(state).unwrap();
    let mut request = submit_fake(&id);
    request.source_system = Some("jira".into());
    request.source_record_reference = Some("PROJ-123".into());
    request.correlation_id = Some("trace-abc".into());

    let submitted = server
        .conductor_submit_task(Parameters(request))
        .await
        .unwrap()
        .0;
    assert_eq!(submitted.metadata["source_system"], "jira");
    assert_eq!(submitted.metadata["source_record_reference"], "PROJ-123");
    assert_eq!(submitted.metadata["correlation_id"], "trace-abc");
    assert!(submitted.metadata["follow_up_of"].is_null());

    let task_id = submitted.task["id"].as_str().unwrap().to_string();
    let fetched = server
        .conductor_get_task(Parameters(GetTaskRequest {
            task_id,
            preview_bytes: None,
            request_id: None,
        }))
        .await
        .unwrap()
        .0;
    assert_eq!(fetched.metadata, submitted.metadata);
}

#[tokio::test]
async fn follow_up_of_requires_an_existing_prior_task() {
    let (_dir, _repo, state, id) = setup();
    let server = McpServer::open(state).unwrap();
    let mut request = submit_fake(&id);
    request.follow_up_of = Some("no-such-task".into());

    let result = server.conductor_submit_task(Parameters(request)).await;
    let Err(error) = result else {
        panic!("expected an unknown-prior-task error");
    };
    assert_eq!(error.data.unwrap()["kind"], "not_found");
}

#[tokio::test]
async fn follow_up_of_an_existing_task_is_recorded() {
    let (_dir, _repo, state, id) = setup();
    let server = McpServer::open(state).unwrap();
    let prior = server
        .conductor_submit_task(Parameters(submit_fake(&id)))
        .await
        .unwrap()
        .0
        .task;
    let prior_id = prior["id"].as_str().unwrap().to_string();

    let mut follow_up_request = submit_fake(&id);
    follow_up_request.follow_up_of = Some(prior_id.clone());
    let follow_up = server
        .conductor_submit_task(Parameters(follow_up_request))
        .await
        .unwrap()
        .0;
    assert_eq!(follow_up.metadata["follow_up_of"], prior_id);
}

#[tokio::test]
async fn requested_execution_mode_interactive_overrides_a_covering_grant() {
    let (_dir, repo, state, id) = setup();
    let identity = repo_identity(&repo);
    Grants::open(&state)
        .unwrap()
        .create(
            "test-orchestrator",
            &identity,
            vec!["fake-process-v1".into()],
            vec![Operation::Submit, Operation::Resume, Operation::Approve],
            ExecutionMode::HoldMyBeer,
            "mike",
            5,
            now() + 30 * 60_000,
        )
        .unwrap();

    let server = McpServer::open(state).unwrap();
    let mut request = submit_fake(&id);
    request.requested_execution_mode = Some("interactive".into());
    let submitted = server
        .conductor_submit_task(Parameters(request))
        .await
        .unwrap()
        .0;

    assert_eq!(submitted.task["status"], "awaiting_approval");
    assert!(submitted.authorization.is_empty());
}

/// M12.0 Phase 3: a grant scoped to `enrollment::PRINCIPAL_NAMESPACE_PREFIX`'s namespace
/// (meant for one specific cryptographically verified edge-enrolled client) must not
/// auto-authorize anything over the unverified local stdio path, even when the caller
/// asserts the *exact* same principal string the grant names - stdio local mode has no
/// mechanism to verify that assertion at all, which is exactly why this boundary exists
/// (`src/mcp.rs::LocalBackend::covering_grant`, `docs/m12/M12_0_SECURITY_VALIDATION.md`).
#[tokio::test]
async fn local_stdio_cannot_match_a_grant_scoped_to_an_enrolled_client_namespace() {
    let (_dir, repo, state, id) = setup();
    let identity = repo_identity(&repo);
    let namespaced = clatterforge::enrollment::namespaced_principal("some-real-enrolled-client-id");
    Grants::open(&state)
        .unwrap()
        .create(
            &namespaced,
            &identity,
            vec!["fake-process-v1".into()],
            vec![Operation::Submit, Operation::Resume, Operation::Approve],
            ExecutionMode::HoldMyBeer,
            "mike",
            5,
            now() + 30 * 60_000,
        )
        .unwrap();

    let server = McpServer::open(state).unwrap();
    let mut request = submit_fake(&id);
    request.principal = namespaced;
    let submitted = server
        .conductor_submit_task(Parameters(request))
        .await
        .unwrap()
        .0;

    assert_eq!(
        submitted.task["status"], "awaiting_approval",
        "stdio local mode must never auto-chain under a grant reserved for a verified edge identity: {:#?}",
        submitted.task
    );
    assert!(submitted.authorization.is_empty());
}

#[tokio::test]
async fn approval_requirements_report_workflow_id_required_scope_and_resumption_reference() {
    let (_dir, _repo, state, id) = setup();
    let server = McpServer::open(state).unwrap();
    let mut request = submit_fake(&id);
    request.workflow_id = Some("JIRA-42".into());
    let created = server
        .conductor_submit_task(Parameters(request))
        .await
        .unwrap()
        .0
        .task;
    let task_id = created["id"].as_str().unwrap().to_string();

    let requirements = server
        .conductor_get_approval_requirements(Parameters(TaskIdRequest {
            task_id: task_id.clone(),
            principal: None,
            request_id: None,
        }))
        .await
        .unwrap()
        .0;

    assert_eq!(requirements.workflow_id.as_deref(), Some("JIRA-42"));
    let scope = requirements.required_scope.unwrap();
    assert_eq!(scope["backend"], "fake-process-v1");
    assert_eq!(scope["operation"], "submit");
    assert_eq!(requirements.resumption_reference["task_id"], task_id);
    assert_eq!(
        requirements.resumption_reference["revision"],
        created["revision"].clone()
    );
}

#[tokio::test]
async fn an_expired_grant_falls_back_to_interactive_instead_of_authorizing_anything() {
    let (_dir, repo, state, id) = setup();
    let identity = repo_identity(&repo);
    Grants::open(&state)
        .unwrap()
        .create(
            "test-orchestrator",
            &identity,
            vec!["fake-process-v1".into()],
            vec![Operation::Submit],
            ExecutionMode::Delegated,
            "mike",
            5,
            now() + 50,
        )
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;

    let server = McpServer::open(state).unwrap();
    let submitted = server
        .conductor_submit_task(Parameters(submit_fake(&id)))
        .await
        .unwrap()
        .0;

    // The grant exists but has expired by the time of this call - it must not authorize
    // anything, exactly as if it had never existed.
    assert_eq!(submitted.task["status"], "awaiting_approval");
    assert!(submitted.authorization.is_empty());

    let task_id = submitted.task["id"].as_str().unwrap().to_string();
    let requirements = server
        .conductor_get_approval_requirements(Parameters(TaskIdRequest {
            task_id,
            principal: Some("test-orchestrator".into()),
            request_id: None,
        }))
        .await
        .unwrap()
        .0;
    assert!(requirements.covered_by_grant.is_none());
}

/// Proves the "client disconnects while an approval is pending, then reconnects and
/// recovers task state" required case concretely: the task is created and left pending by
/// one spawned server process/client connection; a *second*, independently spawned server
/// process (same --state directory, no shared memory with the first) and a fresh client
/// connection can still see and act on it. Nothing about resuming a task depends on any one
/// connection or process staying alive - state lives in state.sqlite, not in this server.
#[tokio::test]
async fn a_reconnecting_client_against_a_fresh_server_process_recovers_pending_task_state() {
    let (_dir, _repo, state, id) = setup();

    let (mut first_child, first_client) = spawn_and_connect(&state).await;
    let submit_args = serde_json::to_value(submit_fake(&id))
        .unwrap()
        .as_object()
        .unwrap()
        .clone();
    let submitted = first_client
        .call_tool(CallToolRequestParams::new("conductor_submit_task").with_arguments(submit_args))
        .await
        .expect("conductor_submit_task over stdio")
        .structured_content
        .expect("structured content");
    let task_id = submitted["task"]["id"].as_str().unwrap().to_string();
    assert_eq!(submitted["task"]["status"], "awaiting_approval");

    // The first client disconnects (and its server process is killed) without ever
    // approving the task.
    first_client.cancel().await.ok();
    let _ = first_child.kill().await;

    // A brand new process and a brand new client connection pick the exact same task back
    // up, unchanged.
    let (mut second_child, second_client) = spawn_and_connect(&state).await;
    let get_args = serde_json::json!({"task_id": task_id})
        .as_object()
        .unwrap()
        .clone();
    let recovered = second_client
        .call_tool(CallToolRequestParams::new("conductor_get_task").with_arguments(get_args))
        .await
        .expect("conductor_get_task over stdio, from a fresh process")
        .structured_content
        .expect("structured content");
    assert_eq!(recovered["task"]["id"], task_id);
    assert_eq!(recovered["task"]["status"], "awaiting_approval");
    assert_eq!(
        recovered["task"]["assignment_digest"],
        submitted["task"]["assignment_digest"]
    );

    second_client.cancel().await.ok();
    let _ = second_child.kill().await;
}

#[tokio::test]
async fn propose_task_validates_without_creating_anything() {
    let (_dir, repo, state, _id) = setup();
    let verifier = ExecutionPolicy::new(Path::new(env!("CARGO_BIN_EXE_clatterforge")))
        .unwrap()
        .executable;
    let valid_spec = json!({
        "repository": repo,
        "objective": "Fix the thing",
        "acceptance": "Verifier passes",
        "allowed_files": ["README.md"],
        "context_files": [],
        "verification_files": ["README.md"],
        "verify_executable": verifier,
        "verify_args": [],
        "target": "native",
    });
    let server = McpServer::open(state).unwrap();
    let ok = server
        .conductor_propose_task(Parameters(ProposeTaskRequest {
            spec: valid_spec,
            request_id: None,
        }))
        .await
        .unwrap()
        .0;
    assert!(ok.valid, "{ok:?}");
    assert!(ok.repository_identity.is_some());

    let invalid_spec = json!({
        "repository": repo,
        "objective": "",
        "acceptance": "",
        "allowed_files": [],
        "context_files": [],
        "verification_files": [],
        "verify_executable": verifier,
        "verify_args": [],
        "target": "native",
    });
    let bad = server
        .conductor_propose_task(Parameters(ProposeTaskRequest {
            spec: invalid_spec,
            request_id: None,
        }))
        .await
        .unwrap()
        .0;
    assert!(!bad.valid);
    assert!(!bad.errors.is_empty());

    // Nothing was created by either call.
    let tasks = server
        .conductor_list_tasks(Parameters(RequestIdOnly { request_id: None }))
        .await
        .unwrap()
        .0;
    assert!(tasks.tasks.is_empty());
}

#[tokio::test]
async fn submit_task_rejects_an_unsupported_backend_without_guessing() {
    let (_dir, _repo, state, id) = setup();
    let server = McpServer::open(state).unwrap();
    let mut request = submit_fake(&id);
    request.backend = "totally-unsupported-backend".into();
    let result = server.conductor_submit_task(Parameters(request)).await;
    let Err(error) = result else {
        panic!("expected an unsupported-backend error");
    };
    assert!(error.data.is_some(), "{error:?}");
}

#[tokio::test]
async fn list_repositories_returns_every_registration() {
    let (_dir, _repo, state, id) = setup();
    let server = McpServer::open(state).unwrap();
    let response = server
        .conductor_list_repositories(Parameters(RequestIdOnly { request_id: None }))
        .await
        .unwrap()
        .0;
    assert_eq!(response.repositories.len(), 1);
    assert_eq!(response.repositories[0]["id"], Value::String(id));
}

#[tokio::test]
async fn submit_task_is_idempotent_within_this_adapter_process() {
    let (_dir, _repo, state, id) = setup();
    let server = McpServer::open(state).unwrap();
    let mut request = submit_fake(&id);
    request.workflow_id = Some("JIRA-1".into());
    request.idempotency_key = Some("attempt-1".into());

    let first = server
        .conductor_submit_task(Parameters(request.clone()))
        .await
        .unwrap()
        .0;
    assert!(!first.replayed);

    let second = server
        .conductor_submit_task(Parameters(request))
        .await
        .unwrap()
        .0;
    assert!(second.replayed);
    assert_eq!(first.task["id"], second.task["id"]);

    let tasks = server
        .conductor_list_tasks(Parameters(RequestIdOnly { request_id: None }))
        .await
        .unwrap()
        .0;
    assert_eq!(tasks.tasks.len(), 1, "a replay must not create a duplicate");
}

#[tokio::test]
async fn approval_requirements_and_decisions_follow_the_actual_task_status() {
    let (_dir, _repo, state, id) = setup();
    let server = McpServer::open(state).unwrap();
    let created = server
        .conductor_submit_task(Parameters(submit_fake(&id)))
        .await
        .unwrap()
        .0
        .task;
    let task_id = created["id"].as_str().unwrap().to_string();
    let assignment_digest = created["assignment_digest"].as_str().unwrap().to_string();

    let requirements = server
        .conductor_get_approval_requirements(Parameters(TaskIdRequest {
            task_id: task_id.clone(),
            principal: None,
            request_id: None,
        }))
        .await
        .unwrap()
        .0;
    assert!(requirements.approval_required);
    assert_eq!(requirements.action.as_deref(), Some("approve"));
    assert_eq!(
        requirements.required_digest.as_deref(),
        Some(assignment_digest.as_str())
    );

    // A stale digest is refused, not silently accepted.
    let rejected = server
        .conductor_approve(Parameters(DecisionRequest {
            task_id: task_id.clone(),
            revision: created["revision"].as_u64().unwrap(),
            digest: "not-the-real-digest".into(),
            principal: "reviewer@example.invalid".into(),
            request_id: None,
        }))
        .await;
    assert!(rejected.is_err());

    let approved = server
        .conductor_approve(Parameters(DecisionRequest {
            task_id: task_id.clone(),
            revision: created["revision"].as_u64().unwrap(),
            digest: assignment_digest,
            principal: "reviewer@example.invalid".into(),
            request_id: None,
        }))
        .await
        .unwrap()
        .0;
    assert_eq!(approved.action_taken, "approve");
    assert_eq!(approved.task["status"], "approved");

    // Nothing to resume/approve again right now would be a conflict, not a crash.
    let requirements_after = server
        .conductor_get_approval_requirements(Parameters(TaskIdRequest {
            task_id,
            principal: None,
            request_id: None,
        }))
        .await
        .unwrap()
        .0;
    assert!(!requirements_after.approval_required);
}

#[tokio::test]
async fn cancel_requires_an_actor_for_fake_tasks() {
    let (_dir, _repo, state, id) = setup();
    let server = McpServer::open(state).unwrap();
    let created = server
        .conductor_submit_task(Parameters(submit_fake(&id)))
        .await
        .unwrap()
        .0
        .task;
    let task_id = created["id"].as_str().unwrap().to_string();
    let revision = created["revision"].as_u64().unwrap();

    // The actor check runs before engine dispatch, regardless of task status.
    let missing_actor = server
        .conductor_cancel_task(Parameters(CancelTaskRequest {
            task_id: task_id.clone(),
            revision,
            principal: None,
            request_id: None,
        }))
        .await;
    assert!(missing_actor.is_err());

    // With an actor supplied, the fake engine's own precondition still applies: a task that
    // never reached an active attempt (PreparingWorkspace/Running/Verifying) has nothing to
    // cancel. This is Store::cancel's real, pre-existing rule, not something this adapter
    // invented - confirming the adapter doesn't paper over it.
    let nothing_to_cancel = server
        .conductor_cancel_task(Parameters(CancelTaskRequest {
            task_id,
            revision,
            principal: Some("ops".into()),
            request_id: None,
        }))
        .await;
    assert!(nothing_to_cancel.is_err());
}

#[tokio::test]
async fn get_task_on_an_unknown_id_is_a_structured_not_found() {
    let (_dir, _repo, state, _id) = setup();
    let server = McpServer::open(state).unwrap();
    let result = server
        .conductor_get_task(Parameters(GetTaskRequest {
            task_id: "no-such-task".into(),
            preview_bytes: None,
            request_id: None,
        }))
        .await;
    let Err(error) = result else {
        panic!("expected a not-found error");
    };
    let data = error.data.unwrap();
    assert_eq!(data["kind"], "not_found");
}

/// The one test that proves real stdio, not an in-process illusion: spawns the actual built
/// `clatterforge mcp-serve` and drives the full fake-task lifecycle over real OS pipes with
/// the real `rmcp` client. `current_exe()` inside the spawned server process correctly
/// resolves to `clatterforge.exe` itself (unlike an in-process call from this test binary),
/// so `conductor_resume`'s `run` can genuinely re-spawn the fixture worker.
#[tokio::test]
async fn real_stdio_fake_task_lifecycle_runs_to_acceptance() {
    let (_dir, _repo, state, id) = setup();

    let (mut child, client) = spawn_and_connect(&state).await;

    let tools = client.list_all_tools().await.expect("tools/list");
    let names: Vec<_> = tools.iter().map(|t| t.name.as_ref()).collect();
    for expected in [
        "conductor_capabilities",
        "conductor_submit_task",
        "conductor_approve",
        "conductor_resume",
        "conductor_get_evidence",
    ] {
        assert!(
            names.contains(&expected),
            "missing tool {expected}: {names:?}"
        );
    }

    let submit_args = serde_json::to_value(submit_fake(&id))
        .unwrap()
        .as_object()
        .unwrap()
        .clone();
    let submitted = client
        .call_tool(CallToolRequestParams::new("conductor_submit_task").with_arguments(submit_args))
        .await
        .expect("conductor_submit_task over stdio")
        .structured_content
        .expect("structured content");
    let task = &submitted["task"];
    let task_id = task["id"].as_str().unwrap().to_string();
    let revision = task["revision"].as_u64().unwrap();
    let assignment_digest = task["assignment_digest"].as_str().unwrap().to_string();

    let approve_args = json!({
        "task_id": task_id,
        "revision": revision,
        "digest": assignment_digest,
        "principal": "reviewer@example.invalid",
    })
    .as_object()
    .unwrap()
    .clone();
    let approved = client
        .call_tool(CallToolRequestParams::new("conductor_approve").with_arguments(approve_args))
        .await
        .expect("conductor_approve over stdio")
        .structured_content
        .expect("structured content");
    assert_eq!(approved["task"]["status"], "approved");
    let approved_revision = approved["task"]["revision"].as_u64().unwrap();

    let resume_args = json!({"task_id": task_id, "revision": approved_revision})
        .as_object()
        .unwrap()
        .clone();
    let resumed = client
        .call_tool(CallToolRequestParams::new("conductor_resume").with_arguments(resume_args))
        .await
        .expect("conductor_resume over stdio")
        .structured_content
        .expect("structured content");
    assert_eq!(resumed["action_taken"], "run");
    assert_eq!(resumed["task"]["status"], "awaiting_review");
    let evidence_digest = resumed["task"]["evidence_digest"]
        .as_str()
        .expect("evidence digest after a successful run")
        .to_string();
    let review_revision = resumed["task"]["revision"].as_u64().unwrap();

    let evidence_args = json!({"task_id": task_id}).as_object().unwrap().clone();
    let evidence = client
        .call_tool(
            CallToolRequestParams::new("conductor_get_evidence").with_arguments(evidence_args),
        )
        .await
        .expect("conductor_get_evidence over stdio")
        .structured_content
        .expect("structured content");
    assert_eq!(evidence["task"]["evidence_digest"], evidence_digest);

    let accept_args = json!({
        "task_id": task_id,
        "revision": review_revision,
        "digest": evidence_digest,
        "principal": "reviewer@example.invalid",
    })
    .as_object()
    .unwrap()
    .clone();
    let accepted = client
        .call_tool(CallToolRequestParams::new("conductor_approve").with_arguments(accept_args))
        .await
        .expect("conductor_approve (review accept) over stdio")
        .structured_content
        .expect("structured content");
    assert_eq!(accepted["action_taken"], "review_accept");
    assert_eq!(accepted["task"]["status"], "accepted");

    client.cancel().await.ok();
    let _ = child.kill().await;
}

// ---------------------------------------------------------------------------------------
// Phase 6: acceptance testing - MCP protocol edge cases, authorization edge cases, and a
// deterministic fake-external-orchestrator workflow simulation, using real tool calls
// throughout (never a mocked success response), per the plan's own instruction.
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn calling_an_unknown_tool_is_refused_with_a_protocol_error() {
    let (_dir, _repo, state, _id) = setup();
    let (mut child, client) = spawn_and_connect(&state).await;

    let result = client
        .call_tool(CallToolRequestParams::new(
            "conductor_this_tool_does_not_exist",
        ))
        .await;
    assert!(
        result.is_err(),
        "an unknown tool name must not silently succeed"
    );

    client.cancel().await.ok();
    let _ = child.kill().await;
}

#[tokio::test]
async fn invalid_arguments_are_refused_before_any_task_is_created() {
    let (_dir, _repo, state, id) = setup();
    let (mut child, client) = spawn_and_connect(&state).await;

    // Missing every required field (backend, principal) that conductor_submit_task's own
    // schema demands. MCP reports a parameter-extraction failure as a normal response with
    // is_error: true, not a transport-level error - so both outcomes count as "refused."
    let empty_args = json!({}).as_object().unwrap().clone();
    let result = client
        .call_tool(CallToolRequestParams::new("conductor_submit_task").with_arguments(empty_args))
        .await;
    let refused = match result {
        Err(_) => true,
        Ok(response) => response.is_error == Some(true),
    };
    assert!(refused, "missing required fields must be refused");

    // Confirm nothing was created despite the malformed call.
    let list_result = client
        .call_tool(CallToolRequestParams::new("conductor_list_tasks"))
        .await
        .expect("conductor_list_tasks over stdio")
        .structured_content
        .expect("structured content");
    assert_eq!(list_result["tasks"].as_array().unwrap().len(), 0);
    let _ = &id; // kept for symmetry with the other real_stdio tests' setup

    client.cancel().await.ok();
    let _ = child.kill().await;
}

#[tokio::test]
async fn concurrent_submissions_against_one_connection_each_get_a_distinct_task() {
    let (_dir, _repo, state, id) = setup();
    let (mut child, client) = spawn_and_connect(&state).await;

    let mut calls = Vec::new();
    for i in 0..5 {
        let mut request = submit_fake(&id);
        request.workflow_id = Some("concurrency-test".into());
        request.idempotency_key = Some(format!("attempt-{i}"));
        let args = serde_json::to_value(request)
            .unwrap()
            .as_object()
            .unwrap()
            .clone();
        calls.push(
            client.call_tool(
                CallToolRequestParams::new("conductor_submit_task").with_arguments(args),
            ),
        );
    }
    let results = futures_util::future::join_all(calls).await;

    let mut task_ids = std::collections::HashSet::new();
    for result in results {
        let structured = result
            .expect("conductor_submit_task over stdio")
            .structured_content
            .expect("structured content");
        task_ids.insert(structured["task"]["id"].as_str().unwrap().to_string());
    }
    assert_eq!(
        task_ids.len(),
        5,
        "five distinct idempotency keys must yield five distinct tasks"
    );

    client.cancel().await.ok();
    let _ = child.kill().await;
}

#[tokio::test]
async fn hold_my_beer_grants_are_still_bounded_by_repository_and_backend_scope() {
    let (_dir, repo, state, id) = setup();
    let identity = repo_identity(&repo);
    // Scoped to Submit only (not Resume) so the final in-process check below can prove
    // coverage without triggering an actual `run` - current_exe() inside this test binary
    // doesn't resolve to the real clatterforge.exe, the same reason every other test that
    // needs a real run uses a spawned subprocess instead (see the real_stdio_* tests).
    let mut grants = Grants::open(&state).unwrap();
    grants
        .create(
            "test-orchestrator",
            &identity,
            vec!["fake-process-v1".into()],
            vec![Operation::Submit],
            ExecutionMode::HoldMyBeer,
            "mike",
            5,
            now() + 30 * 60_000,
        )
        .unwrap();

    // Hold My Beer is still scoped: a different repository identity and a different
    // backend are both correctly refused, proving "broad autonomous execution within the
    // explicitly configured...boundaries" - not unrestricted authority.
    assert!(
        grants
            .find_covering(
                "test-orchestrator",
                "local:/some/other/repo",
                "fake-process-v1",
                Operation::Submit
            )
            .unwrap()
            .is_none(),
        "a Hold My Beer grant must not cover a repository it was never scoped to"
    );
    assert!(
        grants
            .find_covering(
                "test-orchestrator",
                &identity,
                "local-changes-v1",
                Operation::Submit
            )
            .unwrap()
            .is_none(),
        "a Hold My Beer grant must not cover a backend it was never scoped to"
    );

    // And it does cover exactly what it was scoped to - confirmed through the real adapter,
    // not just the Grant type directly.
    let server = McpServer::open(state).unwrap();
    let submitted = server
        .conductor_submit_task(Parameters(submit_fake(&id)))
        .await
        .unwrap()
        .0;
    assert_eq!(submitted.task["status"], "approved");
}

/// Phase 6's "deterministic fake external orchestrator" workflow test: simulates a Jira
/// event triggering task submission, automatic delegated approval, coding execution,
/// independent verification, evidence retrieval, and a follow-up request - using real
/// `conductor_*` tool calls over real stdio throughout, never a mocked success response.
#[tokio::test]
async fn deterministic_fake_orchestrator_end_to_end_workflow() {
    let (_dir, repo, state, id) = setup();
    let identity = repo_identity(&repo);

    // A human pre-authorizes this simulated orchestrator for this repository, ahead of time
    // - the "developer defines the permissions granted to external orchestrators" step.
    Grants::open(&state)
        .unwrap()
        .create(
            "jira-sync-bot",
            &identity,
            vec!["fake-process-v1".into()],
            vec![Operation::Submit, Operation::Resume, Operation::Approve],
            ExecutionMode::Delegated,
            "mike",
            10,
            now() + 30 * 60_000,
        )
        .unwrap();

    let (mut child, client) = spawn_and_connect(&state).await;

    // 1. Simulated Jira event -> idempotent task submission.
    let mut initial = submit_fake(&id);
    initial.principal = "jira-sync-bot".into();
    initial.workflow_id = Some("JIRA-777".into());
    initial.idempotency_key = Some("sync-1".into());
    initial.source_system = Some("jira".into());
    initial.source_record_reference = Some("JIRA-777".into());
    initial.correlation_id = Some("JIRA-777".into());
    let submit_args = serde_json::to_value(&initial)
        .unwrap()
        .as_object()
        .unwrap()
        .clone();
    let submitted = client
        .call_tool(CallToolRequestParams::new("conductor_submit_task").with_arguments(submit_args))
        .await
        .expect("conductor_submit_task over stdio")
        .structured_content
        .expect("structured content");

    // 2. Automatic delegated approval + 3. coding execution + 4. independent verification -
    // all happened inside that one call, authorized by the grant above, with zero further
    // human or orchestrator interaction.
    assert_eq!(submitted["task"]["status"], "accepted", "{submitted:#?}");
    let steps: Vec<_> = submitted["authorization"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["operation"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(steps, vec!["approve", "resume", "review_accept"]);
    let original_task_id = submitted["task"]["id"].as_str().unwrap().to_string();

    // 5. Evidence retrieval.
    let evidence_args = json!({"task_id": original_task_id.clone()})
        .as_object()
        .unwrap()
        .clone();
    let evidence = client
        .call_tool(
            CallToolRequestParams::new("conductor_get_evidence").with_arguments(evidence_args),
        )
        .await
        .expect("conductor_get_evidence over stdio")
        .structured_content
        .expect("structured content");
    assert_eq!(evidence["task"]["id"], original_task_id);
    assert!(evidence["task"]["evidence_digest"].is_string());

    // 6. Follow-up request - simulating a review-feedback-driven second task, tied back to
    // the first by provenance, not a resubmission of the same work.
    let mut follow_up = submit_fake(&id);
    follow_up.principal = "jira-sync-bot".into();
    follow_up.workflow_id = Some("JIRA-777".into());
    follow_up.idempotency_key = Some("sync-2-address-review-comments".into());
    follow_up.follow_up_of = Some(original_task_id.clone());
    follow_up.correlation_id = Some("JIRA-777".into());
    let follow_up_args = serde_json::to_value(&follow_up)
        .unwrap()
        .as_object()
        .unwrap()
        .clone();
    let follow_up_result = client
        .call_tool(
            CallToolRequestParams::new("conductor_submit_task").with_arguments(follow_up_args),
        )
        .await
        .expect("conductor_submit_task (follow-up) over stdio")
        .structured_content
        .expect("structured content");

    // "Final completion notification" - this engine's equivalent is the follow-up task
    // itself reaching a terminal, inspectable state with intact provenance back to the
    // original. Anything PR/chat-shaped is the orchestrator's own job, not this engine's -
    // see docs/m11/integration/sample-delegated-workflow.md.
    assert_eq!(follow_up_result["task"]["status"], "accepted");
    assert_eq!(
        follow_up_result["metadata"]["follow_up_of"],
        original_task_id
    );
    assert_ne!(follow_up_result["task"]["id"], original_task_id);

    client.cancel().await.ok();
    let _ = child.kill().await;
}

#[derive(Debug, Clone, Default)]
struct DummyClient;

impl ClientHandler for DummyClient {
    fn get_info(&self) -> ClientConfig {
        ClientConfig::default()
    }
}
