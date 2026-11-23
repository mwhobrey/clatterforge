//! M12.0 Phase 3: "repair cross-principal authorization" - the brief's own mandatory
//! security gate, exercised with two separately enrolled clients against real `dev-
//! conductor serve` + `dev-conductor-edge` processes (per the brief's Phase 7A: "Use two
//! separately enrolled fake clients").

use dev_conductor::{
    authorization::{ExecutionMode, Grants, Operation},
    enrollment::{self, EnrollmentOperation, Enrollments},
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
    fs::write(repo.join("README.md"), "# Cross-principal fixture\n").unwrap();
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

/// Always rebuilds (cheap no-op via Cargo's cache when nothing changed) rather than only
/// building when missing - a stale binary in a separate workspace member can silently
/// drop a wire field nothing else would catch. See `tests/edge.rs`'s identical helper.
fn edge_binary() -> PathBuf {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let status = std::process::Command::new(env!("CARGO"))
        .args(["build", "-p", "dev-conductor-edge"])
        .current_dir(manifest_dir)
        .status()
        .expect("run cargo build -p dev-conductor-edge");
    assert!(status.success(), "building dev-conductor-edge failed");
    manifest_dir.join("target").join("debug").join(format!(
        "dev-conductor-edge{}",
        std::env::consts::EXE_SUFFIX
    ))
}

fn wait_for_main_ready(state: &Path, port: u16) {
    let token_path = state.join("serve-token");
    for _ in 0..100 {
        if token_path.exists() && std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("dev-conductor serve never became ready on port {port}");
}

fn start_main(state: &Path) -> (std::process::Child, u16) {
    let port = free_port();
    let process = std::process::Command::new(env!("CARGO_BIN_EXE_dev-conductor"))
        .arg("--state")
        .arg(state)
        .arg("serve")
        .arg("--port")
        .arg(port.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn dev-conductor serve");
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
        .expect("spawn dev-conductor-edge");
    let stdin = process.stdin.take().expect("edge stdin");
    let stdout = process.stdout.take().expect("edge stdout");
    let client = DummyClient
        .serve((stdout, stdin))
        .await
        .expect("mcp handshake with edge over real stdio");
    (process, client)
}

fn submit_args(repository_id: &str, objective: &str) -> serde_json::Map<String, serde_json::Value> {
    serde_json::json!({
        "backend": "fake-process-v1",
        "principal": "irrelevant-the-server-overrides-this",
        "repository_id": repository_id,
        "objective": objective,
        "acceptance": "Review a new file and its host receipt",
        "contents": "hello\n",
        "budget": 16384,
    })
    .as_object()
    .unwrap()
    .clone()
}

/// Client A must never be able to discover or use a grant scoped to Client B's enrolled
/// identity - even though both submit against the exact same repository/backend, A's
/// submission stays at `awaiting_approval` (no auto-chain) and A's own
/// `conductor_get_approval_requirements` reports no covering grant, while B's identical
/// shape of submission auto-chains straight through on its own real grant.
#[tokio::test]
async fn client_a_cannot_discover_or_use_client_bs_grant() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture_repo(&dir.path().join("checkout"));
    let state = dir.path().join("state");
    let (repository_id, _) = Store::open(&state).unwrap().register(&repo).unwrap();
    let repo_identity = Repository::inspect(&repo).unwrap().identity;

    let mut enrollments = Enrollments::open(&state).unwrap();
    let (_client_a, token_a) = enrollments
        .create(
            "Client A",
            "generic-mcp-client",
            vec![EnrollmentOperation::Inspect, EnrollmentOperation::Submit],
            vec![repo_identity.clone()],
            "mike",
        )
        .unwrap();
    let (client_b, token_b) = enrollments
        .create(
            "Client B",
            "generic-mcp-client",
            vec![EnrollmentOperation::Inspect, EnrollmentOperation::Submit],
            vec![repo_identity.clone()],
            "mike",
        )
        .unwrap();

    // A grant for B only.
    Grants::open(&state)
        .unwrap()
        .create(
            &enrollment::namespaced_principal(&client_b.client_id),
            &repo_identity,
            vec!["fake-process-v1".into()],
            vec![Operation::Submit, Operation::Resume, Operation::Approve],
            ExecutionMode::HoldMyBeer,
            "mike",
            5,
            now() + 60_000,
        )
        .unwrap();

    let (mut main_process, port) = start_main(&state);

    let (mut edge_a, client_a_conn) = spawn_edge(port, &token_a).await;
    let submitted_a = client_a_conn
        .call_tool(
            CallToolRequestParams::new("conductor_submit_task")
                .with_arguments(submit_args(&repository_id, "A's task")),
        )
        .await
        .expect("A can submit")
        .structured_content
        .expect("structured content");
    assert_eq!(
        submitted_a["task"]["status"], "awaiting_approval",
        "A has no grant of its own and must not auto-chain through B's grant: {submitted_a:#?}"
    );
    assert!(
        submitted_a["authorization"].as_array().unwrap().is_empty(),
        "A's submission must show zero authorized steps: {submitted_a:#?}"
    );

    let a_task_id = submitted_a["task"]["id"].as_str().unwrap().to_string();
    let requirements = client_a_conn
        .call_tool(
            CallToolRequestParams::new("conductor_get_approval_requirements").with_arguments(
                serde_json::json!({"task_id": a_task_id})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .expect("A can inspect its own task")
        .structured_content
        .expect("structured content");
    assert!(
        requirements["covered_by_grant"].is_null(),
        "A must not see any grant as covering its own decision point: {requirements:#?}"
    );

    let (mut edge_b, client_b_conn) = spawn_edge(port, &token_b).await;
    let submitted_b = client_b_conn
        .call_tool(
            CallToolRequestParams::new("conductor_submit_task")
                .with_arguments(submit_args(&repository_id, "B's task")),
        )
        .await
        .expect("B can submit")
        .structured_content
        .expect("structured content");
    assert_eq!(
        submitted_b["task"]["status"], "accepted",
        "B owns the real grant and must auto-chain normally: {submitted_b:#?}"
    );

    client_a_conn.cancel().await.ok();
    client_b_conn.cancel().await.ok();
    let _ = edge_a.kill().await;
    let _ = edge_b.kill().await;
    let _ = main_process.kill();
    let _ = main_process.wait();
}

/// Client B submits its own task (interactively, no grant). Client A - a different
/// enrolled client whose enrollment *does* allow the `Approve` operation in general - must
/// not be able to approve it: no grant delegates that authority, and A is not the task's
/// recorded initiating client. B approving its own task must still succeed (self-service,
/// exactly M11's original interactive design, now checked for identity on the edge path).
#[tokio::test]
async fn client_a_cannot_approve_client_bs_task_without_explicit_delegation() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture_repo(&dir.path().join("checkout"));
    let state = dir.path().join("state");
    let (repository_id, _) = Store::open(&state).unwrap().register(&repo).unwrap();

    let mut enrollments = Enrollments::open(&state).unwrap();
    let (_client_a, token_a) = enrollments
        .create(
            "Client A",
            "generic-mcp-client",
            vec![
                EnrollmentOperation::Inspect,
                EnrollmentOperation::Submit,
                EnrollmentOperation::Approve,
            ],
            vec![],
            "mike",
        )
        .unwrap();
    let (_client_b, token_b) = enrollments
        .create(
            "Client B",
            "generic-mcp-client",
            vec![
                EnrollmentOperation::Inspect,
                EnrollmentOperation::Submit,
                EnrollmentOperation::Approve,
            ],
            vec![],
            "mike",
        )
        .unwrap();

    let (mut main_process, port) = start_main(&state);

    let (mut edge_b, client_b_conn) = spawn_edge(port, &token_b).await;
    let submitted_b = client_b_conn
        .call_tool(
            CallToolRequestParams::new("conductor_submit_task")
                .with_arguments(submit_args(&repository_id, "B's own task")),
        )
        .await
        .expect("B can submit")
        .structured_content
        .expect("structured content");
    assert_eq!(submitted_b["task"]["status"], "awaiting_approval");
    let task_id = submitted_b["task"]["id"].as_str().unwrap().to_string();
    let digest = submitted_b["task"]["assignment_digest"]
        .as_str()
        .unwrap()
        .to_string();
    let revision = submitted_b["task"]["revision"].as_u64().unwrap();

    let (mut edge_a, client_a_conn) = spawn_edge(port, &token_a).await;
    let approve_args = serde_json::json!({
        "task_id": task_id,
        "revision": revision,
        "digest": digest,
        "principal": "irrelevant-the-server-overrides-this",
    })
    .as_object()
    .unwrap()
    .clone();
    let refused = client_a_conn
        .call_tool(
            CallToolRequestParams::new("conductor_approve").with_arguments(approve_args.clone()),
        )
        .await;
    assert!(
        refused.is_err(),
        "Client A must not be able to approve Client B's task without an explicit grant: {refused:#?}"
    );

    let approved_by_owner = client_b_conn
        .call_tool(CallToolRequestParams::new("conductor_approve").with_arguments(approve_args))
        .await
        .expect("B can approve its own task interactively")
        .structured_content
        .expect("structured content");
    assert_eq!(
        approved_by_owner["task"]["status"], "approved",
        "{approved_by_owner:#?}"
    );

    client_a_conn.cancel().await.ok();
    client_b_conn.cancel().await.ok();
    let _ = edge_a.kill().await;
    let _ = edge_b.kill().await;
    let _ = main_process.kill();
    let _ = main_process.wait();
}

/// A retried `conductor_submit_task` with the same (principal, workflow_id,
/// idempotency_key) through the edge returns the already-created task instead of a
/// duplicate - the same guarantee `tests/mcp.rs::submit_task_is_idempotent_within_this_
/// adapter_process` proves for stdio local mode, now proven over the authenticated edge
/// path, where `principal` is actually the server-overridden verified identity, not
/// whatever the request body said.
#[tokio::test]
async fn replayed_submission_through_the_edge_does_not_duplicate_execution() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture_repo(&dir.path().join("checkout"));
    let state = dir.path().join("state");
    let (repository_id, _) = Store::open(&state).unwrap().register(&repo).unwrap();

    let (_enrollment, token) = Enrollments::open(&state)
        .unwrap()
        .create(
            "Hitch",
            "grok-bot",
            vec![EnrollmentOperation::Inspect, EnrollmentOperation::Submit],
            vec![],
            "mike",
        )
        .unwrap();

    let (mut main_process, port) = start_main(&state);
    let (mut edge_process, edge_client) = spawn_edge(port, &token).await;

    let mut args = submit_args(&repository_id, "idempotent via the edge");
    args.insert("workflow_id".into(), serde_json::json!("JIRA-999"));
    args.insert("idempotency_key".into(), serde_json::json!("retry-1"));

    let first = edge_client
        .call_tool(CallToolRequestParams::new("conductor_submit_task").with_arguments(args.clone()))
        .await
        .expect("first submission")
        .structured_content
        .expect("structured content");
    assert_eq!(first["replayed"], false);
    let first_id = first["task"]["id"].as_str().unwrap().to_string();

    let second = edge_client
        .call_tool(CallToolRequestParams::new("conductor_submit_task").with_arguments(args))
        .await
        .expect("retried submission")
        .structured_content
        .expect("structured content");
    assert_eq!(second["replayed"], true, "{second:#?}");
    assert_eq!(second["task"]["id"].as_str().unwrap(), first_id);

    edge_client.cancel().await.ok();
    let _ = edge_process.kill().await;
    let _ = main_process.kill();
    let _ = main_process.wait();
}
