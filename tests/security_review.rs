//! M12.0.1 Phase 2: a focused security pass over the remote edge API, covering the two
//! negative cases the existing M12.0/M12.0.1 Phase 1 suites didn't already exercise
//! directly - a replayed submission with a *changed* payload, and whether `follow_up_of`
//! metadata could ever expand authorization rather than merely recording provenance.
//! Every other required negative case (missing/incorrect credential, revoked enrollment,
//! forged principal, unauthorized repository/task/artifact, cross-client grant reuse and
//! task manipulation, stale evidence digest) is already covered by `tests/edge.rs`,
//! `tests/cross_principal.rs`, and `tests/repository_scope.rs` - see
//! `docs/m12/M12_0_1_SECURITY_VALIDATION.md` for the full accounting rather than
//! duplicating those tests here.

use dev_conductor::{enrollment::EnrollmentOperation, enrollment::Enrollments, store::Store};
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

fn fixture_repo(root: &Path, name: &str) -> PathBuf {
    let repo = root.join(name);
    fs::create_dir_all(&repo).unwrap();
    dev_conductor::repository::git(&repo, &["init", "-b", "main"]).unwrap();
    dev_conductor::repository::git(&repo, &["config", "core.autocrlf", "false"]).unwrap();
    fs::write(repo.join("README.md"), format!("# {name} fixture\n")).unwrap();
    dev_conductor::repository::git(&repo, &["add", "."]).unwrap();
    dev_conductor::repository::git(
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
    for _ in 0..100 {
        if state.join("serve-token").exists()
            && std::net::TcpStream::connect(("127.0.0.1", port)).is_ok()
        {
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

/// "Replayed request with changed payload": a retried `conductor_submit_task` under the
/// same idempotency key, but with a *different* objective/contents than the original.
/// The system must return the original task unmodified - never create a second task,
/// never mutate the first to match the new payload, and never silently merge the two.
/// Ignoring the new payload entirely is the safe behavior here, not a missed validation.
#[tokio::test]
async fn replaying_a_submission_with_a_changed_payload_returns_the_original_unmodified() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture_repo(&dir.path().join("checkout"), "repo");
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

    let original_args = serde_json::json!({
        "backend": "fake-process-v1",
        "principal": "irrelevant",
        "repository_id": repository_id,
        "objective": "Original objective",
        "acceptance": "n/a",
        "contents": "original contents\n",
        "budget": 4096,
        "workflow_id": "REPLAY-1",
        "idempotency_key": "attempt-1",
    })
    .as_object()
    .unwrap()
    .clone();
    let original = edge_client
        .call_tool(
            CallToolRequestParams::new("conductor_submit_task").with_arguments(original_args),
        )
        .await
        .expect("original submission")
        .structured_content
        .expect("structured content");
    assert_eq!(original["replayed"], false, "{original:#?}");
    let original_id = original["task"]["id"].as_str().unwrap().to_string();
    let original_objective = original["task"]["objective"].as_str().unwrap().to_string();

    let changed_args = serde_json::json!({
        "backend": "fake-process-v1",
        "principal": "irrelevant",
        "repository_id": repository_id,
        "objective": "A completely different objective - this must be ignored",
        "acceptance": "n/a",
        "contents": "different contents entirely\n",
        "budget": 8192,
        "workflow_id": "REPLAY-1",
        "idempotency_key": "attempt-1",
    })
    .as_object()
    .unwrap()
    .clone();
    let replayed = edge_client
        .call_tool(CallToolRequestParams::new("conductor_submit_task").with_arguments(changed_args))
        .await
        .expect("replayed submission with a changed payload")
        .structured_content
        .expect("structured content");
    assert_eq!(replayed["replayed"], true, "{replayed:#?}");
    assert_eq!(
        replayed["task"]["id"], original_id,
        "a replay must never create a second task: {replayed:#?}"
    );
    assert_eq!(
        replayed["task"]["objective"], original_objective,
        "a replay with a changed payload must return the original unmodified, not merge or update it: {replayed:#?}"
    );

    let tasks = edge_client
        .call_tool(CallToolRequestParams::new("conductor_list_tasks"))
        .await
        .expect("list_tasks")
        .structured_content
        .expect("structured content");
    let count = tasks["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["id"] == original_id)
        .count();
    assert_eq!(
        count, 1,
        "exactly one task must exist, never a duplicate: {tasks:#?}"
    );

    edge_client.cancel().await.ok();
    let _ = edge_process.kill().await;
    let _ = main_process.kill();
    let _ = main_process.wait();
}

/// "Attempted privilege expansion through follow-up metadata": a follow-up task's
/// `follow_up_of` link is provenance only. Referencing an accessible parent task must not
/// let a client submit the follow-up itself against a repository that client's own
/// enrollment doesn't separately authorize - the link grants no capability of its own.
#[tokio::test]
async fn follow_up_metadata_cannot_expand_authorization_to_a_different_repository() {
    let dir = tempfile::tempdir().unwrap();
    let repo_a = fixture_repo(&dir.path().join("checkout"), "repo-a");
    let repo_b = fixture_repo(&dir.path().join("checkout"), "repo-b");
    let state = dir.path().join("state");
    let (repo_a_id, _) = Store::open(&state).unwrap().register(&repo_a).unwrap();
    let (repo_b_id, _) = Store::open(&state).unwrap().register(&repo_b).unwrap();
    let repo_a_identity = dev_conductor::repository::Repository::inspect(&repo_a)
        .unwrap()
        .identity;

    let (_enrollment, token) = Enrollments::open(&state)
        .unwrap()
        .create(
            "Hitch",
            "grok-bot",
            vec![EnrollmentOperation::Inspect, EnrollmentOperation::Submit],
            vec![repo_a_identity],
            "mike",
        )
        .unwrap();

    let (mut main_process, port) = start_main(&state);
    let (mut edge_process, edge_client) = spawn_edge(port, &token).await;

    // A legitimate, accessible parent task in repo A - this client's own enrolled scope.
    let parent_args = serde_json::json!({
        "backend": "fake-process-v1",
        "principal": "irrelevant",
        "repository_id": repo_a_id,
        "objective": "Parent task in the client's own repository",
        "acceptance": "n/a",
        "contents": "",
        "budget": 4096,
    })
    .as_object()
    .unwrap()
    .clone();
    let parent = edge_client
        .call_tool(CallToolRequestParams::new("conductor_submit_task").with_arguments(parent_args))
        .await
        .expect("parent submission")
        .structured_content
        .expect("structured content");
    let parent_id = parent["task"]["id"].as_str().unwrap().to_string();

    // The follow-up itself targets repo B - outside this enrollment's scope. Naming an
    // accessible parent must not launder access to a different repository.
    let follow_up_args = serde_json::json!({
        "backend": "fake-process-v1",
        "principal": "irrelevant",
        "repository_id": repo_b_id,
        "objective": "Should never be created - repo B is out of scope regardless of the parent link",
        "acceptance": "n/a",
        "contents": "",
        "budget": 4096,
        "follow_up_of": parent_id,
    })
    .as_object()
    .unwrap()
    .clone();
    let result = edge_client
        .call_tool(
            CallToolRequestParams::new("conductor_submit_task").with_arguments(follow_up_args),
        )
        .await;
    assert!(
        result.is_err(),
        "an accessible parent task must not expand authorization to a different, \
         out-of-scope repository for the follow-up itself: {result:#?}"
    );

    edge_client.cancel().await.ok();
    let _ = edge_process.kill().await;
    let _ = main_process.kill();
    let _ = main_process.wait();
}
