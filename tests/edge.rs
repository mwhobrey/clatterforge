//! M12.0 Phase 1-2 edge acceptance: a real `clatterforge serve` process and a real
//! `clatterforge-edge` process, two separate OS processes talking over loopback HTTP,
//! with the edge then driven over its own real MCP stdio by a third, independent `rmcp`
//! client - the full chain this milestone asked for, not an in-process shortcut.
//! Authenticated against a real `enrollment::Enrollments` record (Phase 2), not Phase 1's
//! placeholder shared desktop-GUI token.

use clatterforge::{
    authorization::{ExecutionMode, Grants, Operation},
    enrollment::{EnrollmentOperation, Enrollments},
    now,
    repository::{Repository, git},
    store::Store,
};
use rmcp::{
    ClientHandler, ServiceExt,
    model::{CallToolRequestParams, ClientConfig},
    service::RunningService,
};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

#[derive(Clone)]
struct DummyClient;

impl ClientHandler for DummyClient {
    fn get_info(&self) -> ClientConfig {
        ClientConfig::default()
    }
}

fn fixture_repo(root: &Path) -> PathBuf {
    let repo = root.join("repo");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]).unwrap();
    git(&repo, &["config", "core.autocrlf", "false"]).unwrap();
    fs::write(repo.join("README.md"), "# Edge fixture\n").unwrap();
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

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// `clatterforge-edge` lives in a separate workspace member with no declared dependency
/// relationship to this test crate, so Cargo doesn't set `CARGO_BIN_EXE_clatterforge-edge`
/// the way it does for this crate's own binary. Building it on demand (a no-op once it
/// already exists) keeps this test self-contained without assuming a prior manual build
/// step. Assumes the default `target/` layout; a customized `CARGO_TARGET_DIR` would need
/// this adjusted, same as `CARGO_BIN_EXE_*` itself would.
/// Builds (or confirms up to date - cheap, Cargo's own incremental cache) the
/// `clatterforge-edge` binary. Always invoking `cargo build` here, not just when the
/// binary is missing, is deliberate: this binary lives in a separate workspace member
/// with no declared dependency on this test crate, so nothing else triggers a rebuild
/// when its sources (or `clatterforge-protocol`'s, which it shares with this crate) change -
/// an existing-but-stale binary silently missing a new wire field is exactly the kind of
/// bug a path-exists check would hide (found live: Phase 5's `agent_preset` field was
/// silently dropped by a stale edge binary until this was fixed).
fn edge_binary() -> PathBuf {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    // Build into the same target dir as this test binary so a CARGO_TARGET_DIR
    // override never falls back to a stale edge binary elsewhere.
    let target_dir = Path::new(env!("CARGO_BIN_EXE_clatterforge"))
        .parent()
        .and_then(Path::parent)
        .expect("cargo target dir");
    let status = std::process::Command::new(env!("CARGO"))
        .args(["build", "-p", "clatterforge-edge", "--target-dir"])
        .arg(target_dir)
        .current_dir(manifest_dir)
        .status()
        .expect("run cargo build -p clatterforge-edge");
    assert!(status.success(), "building clatterforge-edge failed");
    target_dir
        .join("debug")
        .join(format!("clatterforge-edge{}", std::env::consts::EXE_SUFFIX))
}

/// Polls for `<state>/serve-token` to appear - `serve::run` writes it, then binds the
/// listener; this is just a readiness signal, not a credential this test uses (the edge
/// route this file exercises is enrollment-authenticated, not bearer-token-authenticated).
fn wait_for_main_ready(state: &Path, port: u16) {
    let token_path = state.join("serve-token");
    for _ in 0..100 {
        if token_path.exists() && std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("clatterforge serve never became ready on port {port}");
}

fn start_main(state: &Path) -> (std::process::Child, u16) {
    let port = free_port();
    let process = std::process::Command::new(env!("CARGO_BIN_EXE_clatterforge"))
        .arg("--state")
        .arg(state)
        .arg("serve")
        .arg("--port")
        .arg(port.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn clatterforge serve");
    wait_for_main_ready(state, port);
    (process, port)
}

async fn spawn_edge(
    port: u16,
    token: &str,
) -> (
    tokio::process::Child,
    RunningService<rmcp::RoleClient, DummyClient>,
) {
    let mut process = tokio::process::Command::new(edge_binary())
        .arg("--main-url")
        .arg(format!("http://127.0.0.1:{port}"))
        .env("CONDUCTOR_EDGE_TOKEN", token)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn clatterforge-edge");
    let stdin = process.stdin.take().expect("edge stdin");
    let stdout = process.stdout.take().expect("edge stdout");
    let client = DummyClient
        .serve((stdout, stdin))
        .await
        .expect("mcp handshake with edge over real stdio");
    (process, client)
}

/// Proves the Phase 2 requirement at the center of this milestone's enrollment model: the
/// model never supplies or overrides its own authenticated principal. The stdio caller
/// asserts `"principal": "someone-else-entirely"` - if the server trusted that, the grant
/// below (scoped to the *enrollment's own client_id*) would never match, and the task
/// would stop at `awaiting_approval` instead of auto-chaining to `accepted`. It only
/// reaches `accepted` because `serve.rs` overwrote the field with the verified identity
/// before `authorization::Grants::find_covering` ever saw it.
#[tokio::test]
async fn edge_dispatch_overrides_caller_supplied_principal_with_the_verified_enrollment_identity() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture_repo(&dir.path().join("checkout"));
    let state = dir.path().join("state");
    let (repository_id, _) = Store::open(&state).unwrap().register(&repo).unwrap();
    let repo_identity = Repository::inspect(&repo).unwrap().identity;

    let (enrollment, token) = Enrollments::open(&state)
        .unwrap()
        .create(
            "Hitch",
            "grok-bot",
            vec![EnrollmentOperation::Inspect, EnrollmentOperation::Submit],
            vec![repo_identity.clone()],
            "mike",
        )
        .unwrap();

    Grants::open(&state)
        .unwrap()
        .create(
            &clatterforge::enrollment::namespaced_principal(&enrollment.client_id),
            &repo_identity,
            vec!["fake-process-v1".into()],
            vec![Operation::Submit, Operation::Resume, Operation::Approve],
            ExecutionMode::HoldMyBeer,
            "mike",
            5,
            now() + 30 * 60_000,
        )
        .unwrap();

    let (mut main_process, port) = start_main(&state);
    let (mut edge_process, edge_client) = spawn_edge(port, &token).await;

    let caps = edge_client
        .call_tool(CallToolRequestParams::new("conductor_capabilities"))
        .await
        .expect("conductor_capabilities through the edge")
        .structured_content
        .expect("structured content");
    assert_eq!(caps["tools"].as_array().unwrap().len(), 17);

    let submit_args = serde_json::json!({
        "backend": "fake-process-v1",
        "principal": "someone-else-entirely",
        "repository_id": repository_id,
        "objective": "Create a fixture result via the edge",
        "acceptance": "Review a new file and its host receipt",
        "contents": "hello from the edge\n",
        "budget": 16384,
    })
    .as_object()
    .unwrap()
    .clone();
    let submitted = edge_client
        .call_tool(CallToolRequestParams::new("conductor_submit_task").with_arguments(submit_args))
        .await
        .expect("conductor_submit_task through the edge")
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

    edge_client.cancel().await.ok();
    let _ = edge_process.kill().await;
    let _ = main_process.kill();
    let _ = main_process.wait();
}

/// M12.0 Phase 7B: "evidence retrieval works" and "follow-up linkage works," both over the
/// real edge path specifically - the Phase 6 evidence tools were only ever exercised
/// in-process (`tests/mcp.rs`) before this, and follow-up linkage was only ever exercised
/// over stdio, never through a real separate edge process.
#[tokio::test]
async fn evidence_retrieval_and_follow_up_linkage_both_work_through_the_real_edge_path() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture_repo(&dir.path().join("checkout"));
    let state = dir.path().join("state");
    let (repository_id, _) = Store::open(&state).unwrap().register(&repo).unwrap();
    let repo_identity = Repository::inspect(&repo).unwrap().identity;

    let (enrollment, token) = Enrollments::open(&state)
        .unwrap()
        .create(
            "Hitch",
            "grok-bot",
            vec![
                clatterforge::enrollment::EnrollmentOperation::Inspect,
                clatterforge::enrollment::EnrollmentOperation::Submit,
            ],
            vec![repo_identity.clone()],
            "mike",
        )
        .unwrap();
    Grants::open(&state)
        .unwrap()
        .create(
            &clatterforge::enrollment::namespaced_principal(&enrollment.client_id),
            &repo_identity,
            vec!["fake-process-v1".into()],
            vec![Operation::Submit, Operation::Resume, Operation::Approve],
            ExecutionMode::HoldMyBeer,
            "mike",
            5,
            now() + 30 * 60_000,
        )
        .unwrap();

    let (mut main_process, port) = start_main(&state);
    let (mut edge_process, edge_client) = spawn_edge(port, &token).await;

    let first_args = serde_json::json!({
        "backend": "fake-process-v1",
        "principal": "irrelevant",
        "repository_id": repository_id,
        "objective": "First task",
        "acceptance": "Review a new file and its host receipt",
        "contents": "hello from the edge\n",
        "budget": 16384,
    })
    .as_object()
    .unwrap()
    .clone();
    let first = edge_client
        .call_tool(CallToolRequestParams::new("conductor_submit_task").with_arguments(first_args))
        .await
        .expect("first submission through the edge")
        .structured_content
        .expect("structured content");
    assert_eq!(first["task"]["status"], "accepted", "{first:#?}");
    let first_id = first["task"]["id"].as_str().unwrap().to_string();

    // Evidence retrieval through the edge.
    let summary = edge_client
        .call_tool(
            CallToolRequestParams::new("conductor_get_evidence_summary").with_arguments(
                serde_json::json!({"task_id": first_id})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .expect("conductor_get_evidence_summary through the edge")
        .structured_content
        .expect("structured content");
    assert_eq!(summary["status"], "accepted", "{summary:#?}");
    assert!(
        summary["has_verifier_output"].as_bool().unwrap(),
        "{summary:#?}"
    );
    assert!(summary["evidence_digest"].is_string(), "{summary:#?}");
    let label = summary["log_excerpt_label"].as_str().unwrap().to_string();

    let artifact = edge_client
        .call_tool(
            CallToolRequestParams::new("conductor_get_artifact").with_arguments(
                serde_json::json!({"task_id": first_id, "artifact": label, "max_bytes": 4})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .expect("conductor_get_artifact through the edge")
        .structured_content
        .expect("structured content");
    assert_eq!(artifact["task_id"], first_id, "{artifact:#?}");
    assert!(artifact["shown_bytes"].as_u64().unwrap() <= 4);

    // Follow-up linkage through the edge.
    let follow_up_args = serde_json::json!({
        "backend": "fake-process-v1",
        "principal": "irrelevant",
        "repository_id": repository_id,
        "objective": "Follow-up task",
        "acceptance": "Review a new file and its host receipt",
        "contents": "follow-up from the edge\n",
        "budget": 16384,
        "follow_up_of": first_id,
    })
    .as_object()
    .unwrap()
    .clone();
    let follow_up = edge_client
        .call_tool(
            CallToolRequestParams::new("conductor_submit_task").with_arguments(follow_up_args),
        )
        .await
        .expect("follow-up submission through the edge")
        .structured_content
        .expect("structured content");
    assert_eq!(follow_up["task"]["status"], "accepted", "{follow_up:#?}");
    assert_eq!(
        follow_up["metadata"]["follow_up_of"], first_id,
        "{follow_up:#?}"
    );
    assert_ne!(
        follow_up["task"]["id"], first_id,
        "a follow-up is a genuinely distinct task"
    );

    edge_client.cancel().await.ok();
    let _ = edge_process.kill().await;
    let _ = main_process.kill();
    let _ = main_process.wait();
}

#[tokio::test]
async fn an_unknown_or_wrong_bearer_token_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let (mut main_process, port) = start_main(&state);

    let (mut edge_process, edge_client) = spawn_edge(port, "not-a-real-token").await;
    let refused = edge_client
        .call_tool(CallToolRequestParams::new("conductor_capabilities"))
        .await;
    assert!(
        refused.is_err(),
        "an unenrolled token must be refused, not silently served: {refused:#?}"
    );

    edge_client.cancel().await.ok();
    let _ = edge_process.kill().await;
    let _ = main_process.kill();
    let _ = main_process.wait();
}

#[tokio::test]
async fn a_revoked_enrollment_is_refused_on_its_very_next_call_not_just_new_connections() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let mut enrollments = Enrollments::open(&state).unwrap();
    let (enrollment, token) = enrollments
        .create(
            "Hitch",
            "grok-bot",
            vec![EnrollmentOperation::Inspect],
            vec![],
            "mike",
        )
        .unwrap();

    let (mut main_process, port) = start_main(&state);
    let (mut edge_process, edge_client) = spawn_edge(port, &token).await;

    edge_client
        .call_tool(CallToolRequestParams::new("conductor_capabilities"))
        .await
        .expect("first call succeeds while the enrollment is still active");

    enrollments
        .revoke(
            &enrollment.client_id,
            "mike",
            "rotating off this installation",
        )
        .unwrap();

    let refused_after_revocation = edge_client
        .call_tool(CallToolRequestParams::new("conductor_capabilities"))
        .await;
    assert!(
        refused_after_revocation.is_err(),
        "the very next call on the same already-open connection must be refused once revoked: {refused_after_revocation:#?}"
    );

    edge_client.cancel().await.ok();
    let _ = edge_process.kill().await;
    let _ = main_process.kill();
    let _ = main_process.wait();
}

#[tokio::test]
async fn an_enrollment_without_the_submit_operation_can_inspect_but_cannot_submit() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture_repo(&dir.path().join("checkout"));
    let state = dir.path().join("state");
    let (repository_id, _) = Store::open(&state).unwrap().register(&repo).unwrap();

    let (_enrollment, token) = Enrollments::open(&state)
        .unwrap()
        .create(
            "Read-only dashboard",
            "generic-mcp-client",
            vec![EnrollmentOperation::Inspect],
            vec![],
            "mike",
        )
        .unwrap();

    let (mut main_process, port) = start_main(&state);
    let (mut edge_process, edge_client) = spawn_edge(port, &token).await;

    edge_client
        .call_tool(CallToolRequestParams::new("conductor_capabilities"))
        .await
        .expect("an Inspect-only enrollment can still call an Inspect-category tool");

    let submit_args = serde_json::json!({
        "backend": "fake-process-v1",
        "principal": "irrelevant-since-it-gets-overridden",
        "repository_id": repository_id,
        "objective": "Should never be created",
        "acceptance": "n/a",
        "contents": "",
        "budget": 4096,
    })
    .as_object()
    .unwrap()
    .clone();
    let refused = edge_client
        .call_tool(CallToolRequestParams::new("conductor_submit_task").with_arguments(submit_args))
        .await;
    assert!(
        refused.is_err(),
        "an enrollment without Submit in allowed_operations must not be able to call conductor_submit_task: {refused:#?}"
    );

    edge_client.cancel().await.ok();
    let _ = edge_process.kill().await;
    let _ = main_process.kill();
    let _ = main_process.wait();
}

/// M12.0 Phase 7A: "missing credential" as its own, literal case - every other test in
/// this file drives the edge binary, which always sends *some* `Authorization` header, so
/// none of them actually exercise a request with no header at all. A raw HTTP call direct
/// to `/api/edge/tools/call` is the only way to prove this specific case.
#[tokio::test]
async fn a_request_with_no_authorization_header_at_all_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let (mut main_process, port) = start_main(&state);

    let response = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/api/edge/tools/call"))
        .json(&serde_json::json!({"tool": "conductor_capabilities", "params": {}}))
        .send()
        .await
        .expect("send request with no Authorization header");
    assert_eq!(
        response.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "{response:#?}"
    );

    let _ = main_process.kill();
    let _ = main_process.wait();
}

/// M12.0 Phase 7A: "client B remains unaffected" - the brief's own explicit acceptance
/// line, distinct from (and not implied by) "revoking client A blocks client A."
#[tokio::test]
async fn revoking_one_enrolled_client_does_not_affect_a_second_enrolled_client() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let mut enrollments = Enrollments::open(&state).unwrap();
    let (client_a, token_a) = enrollments
        .create(
            "Client A",
            "generic-mcp-client",
            vec![EnrollmentOperation::Inspect],
            vec![],
            "mike",
        )
        .unwrap();
    let (_client_b, token_b) = enrollments
        .create(
            "Client B",
            "generic-mcp-client",
            vec![EnrollmentOperation::Inspect],
            vec![],
            "mike",
        )
        .unwrap();

    let (mut main_process, port) = start_main(&state);
    let (mut edge_a, client_a_conn) = spawn_edge(port, &token_a).await;
    let (mut edge_b, client_b_conn) = spawn_edge(port, &token_b).await;

    enrollments
        .revoke(
            &client_a.client_id,
            "mike",
            "rotating off this installation",
        )
        .unwrap();

    let a_refused = client_a_conn
        .call_tool(CallToolRequestParams::new("conductor_capabilities"))
        .await;
    assert!(
        a_refused.is_err(),
        "revoked client A must be refused: {a_refused:#?}"
    );

    let b_still_works = client_b_conn
        .call_tool(CallToolRequestParams::new("conductor_capabilities"))
        .await;
    assert!(
        b_still_works.is_ok(),
        "client B must be completely unaffected by client A's revocation: {b_still_works:#?}"
    );

    client_a_conn.cancel().await.ok();
    client_b_conn.cancel().await.ok();
    let _ = edge_a.kill().await;
    let _ = edge_b.kill().await;
    let _ = main_process.kill();
    let _ = main_process.wait();
}
