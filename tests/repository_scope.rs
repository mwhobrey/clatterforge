//! M12.0.1 Phase 0/1: enrollment repository scope, actually enforced.
//!
//! `Enrollment.repository_scope` was introduced in M12.0 Phase 2 and stored/returned
//! faithfully ever since, but nothing ever checked a tool call's actual repository
//! against it (documented honestly as an open gap in every M12.0 remaining-risks doc).
//! This file's first test is the Phase 0 reproduction: it encodes the *correct* behavior
//! and is expected to fail against the pre-fix code, then pass once Phase 1 lands the
//! enforcement. Later tests in this file cover the rest of Phase 1's required matrix.

use dev_conductor::{
    authorization::{ExecutionMode, Grants, Operation},
    enrollment::{self, EnrollmentOperation, Enrollments},
    now,
    process::ExecutionPolicy,
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

fn fixture_repo(root: &Path, name: &str) -> PathBuf {
    let repo = root.join(name);
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]).unwrap();
    git(&repo, &["config", "core.autocrlf", "false"]).unwrap();
    fs::write(repo.join("README.md"), format!("# {name} fixture\n")).unwrap();
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

/// Phase 0's required reproduction: Client A is enrolled scoped to repo A only, but a
/// grant (scoped correctly to repo B for a *different* purpose in a real deployment, but
/// here standing in for "anything that would otherwise authorize the call") exists for
/// repo B too. Client A must not be able to touch repo B through conductor_submit_task
/// merely because nothing currently checks its enrollment's own repository_scope.
#[tokio::test]
async fn client_scoped_to_repo_a_cannot_submit_to_repo_b() {
    let dir = tempfile::tempdir().unwrap();
    let repo_a = fixture_repo(&dir.path().join("checkout"), "repo-a");
    let repo_b = fixture_repo(&dir.path().join("checkout"), "repo-b");
    let state = dir.path().join("state");
    let (repo_a_id, _) = Store::open(&state).unwrap().register(&repo_a).unwrap();
    let (repo_b_id, _) = Store::open(&state).unwrap().register(&repo_b).unwrap();
    let repo_a_identity = Repository::inspect(&repo_a).unwrap().identity;
    let repo_b_identity = Repository::inspect(&repo_b).unwrap().identity;

    let (client_a, token_a) = Enrollments::open(&state)
        .unwrap()
        .create(
            "Client A",
            "generic-mcp-client",
            vec![EnrollmentOperation::Inspect, EnrollmentOperation::Submit],
            vec![repo_a_identity.clone()],
            "mike",
        )
        .unwrap();

    // A grant exists covering repo B for this same principal - in the pre-fix world,
    // this is exactly the kind of "broader grant overrides narrower enrollment" bug the
    // brief names explicitly: a grant must never override a narrower enrollment scope.
    Grants::open(&state)
        .unwrap()
        .create(
            &enrollment::namespaced_principal(&client_a.client_id),
            &repo_b_identity,
            vec!["fake-process-v1".into()],
            vec![Operation::Submit, Operation::Resume, Operation::Approve],
            ExecutionMode::HoldMyBeer,
            "mike",
            5,
            now() + 60_000,
        )
        .unwrap();

    let (mut main_process, port) = start_main(&state);
    let (mut edge_process, edge_client) = spawn_edge(port, &token_a).await;

    let submit_args = serde_json::json!({
        "backend": "fake-process-v1",
        "principal": "irrelevant",
        "repository_id": repo_b_id,
        "objective": "Should never be created - out of enrollment scope",
        "acceptance": "n/a",
        "contents": "",
        "budget": 4096,
    })
    .as_object()
    .unwrap()
    .clone();
    let result = edge_client
        .call_tool(CallToolRequestParams::new("conductor_submit_task").with_arguments(submit_args))
        .await;
    assert!(
        result.is_err(),
        "Client A is enrolled for repo A only and must not be able to submit to repo B, \
         even though a grant exists for repo B - a grant must never override a narrower \
         enrollment scope: {result:#?}"
    );

    // Confirm A's own repository still works normally - this isn't a blanket refusal.
    let own_repo_args = serde_json::json!({
        "backend": "fake-process-v1",
        "principal": "irrelevant",
        "repository_id": repo_a_id,
        "objective": "Allowed - inside enrollment scope",
        "acceptance": "n/a",
        "contents": "",
        "budget": 4096,
    })
    .as_object()
    .unwrap()
    .clone();
    let allowed = edge_client
        .call_tool(
            CallToolRequestParams::new("conductor_submit_task").with_arguments(own_repo_args),
        )
        .await;
    assert!(
        allowed.is_ok(),
        "Client A must still be able to operate on its own enrolled repository: {allowed:#?}"
    );

    edge_client.cancel().await.ok();
    let _ = edge_process.kill().await;
    let _ = main_process.kill();
    let _ = main_process.wait();
}

/// Common fixture for the rest of this file's matrix: Client A scoped to repo A only,
/// Client B scoped to repo B only, one real task already sitting in repo B (created
/// directly through `Store`, standing in for "Client B's own work" without needing a
/// second edge connection for setup).
struct TwoRepoTwoClients {
    _dir: tempfile::TempDir,
    repo_a_id: String,
    port: u16,
    main_process: std::process::Child,
    token_a: String,
    repo_b_task_id: String,
}

fn set_up_two_repo_two_clients() -> TwoRepoTwoClients {
    let dir = tempfile::tempdir().unwrap();
    let repo_a = fixture_repo(&dir.path().join("checkout"), "repo-a");
    let repo_b = fixture_repo(&dir.path().join("checkout"), "repo-b");
    let state = dir.path().join("state");
    let (repo_a_id, _) = Store::open(&state).unwrap().register(&repo_a).unwrap();
    let (repo_b_id, _) = Store::open(&state).unwrap().register(&repo_b).unwrap();
    let repo_a_identity = Repository::inspect(&repo_a).unwrap().identity;
    let repo_b_identity = Repository::inspect(&repo_b).unwrap().identity;

    let (_client_a, token_a) = Enrollments::open(&state)
        .unwrap()
        .create(
            "Client A",
            "generic-mcp-client",
            vec![
                EnrollmentOperation::Inspect,
                EnrollmentOperation::Submit,
                EnrollmentOperation::Approve,
                EnrollmentOperation::Resume,
                EnrollmentOperation::Cancel,
            ],
            vec![repo_a_identity],
            "mike",
        )
        .unwrap();
    let (_client_b, _token_b) = Enrollments::open(&state)
        .unwrap()
        .create(
            "Client B",
            "generic-mcp-client",
            vec![EnrollmentOperation::Inspect, EnrollmentOperation::Submit],
            vec![repo_b_identity.clone()],
            "mike",
        )
        .unwrap();

    // Client B's own task, created directly against the engine (standing in for a real
    // submission - setup, not something Client A's connection ever touches).
    let mut store = Store::open(&state).unwrap();
    let executable = ExecutionPolicy::new(Path::new(env!("CARGO_BIN_EXE_dev-conductor"))).unwrap();
    let repo_b_task = store
        .create(
            &repo_b_id,
            "Client B's own task".into(),
            "n/a".into(),
            "hello from repo B\n".into(),
            4096,
            executable,
        )
        .unwrap();
    drop(store);

    let (main_process, port) = start_main(&state);

    TwoRepoTwoClients {
        _dir: dir,
        repo_a_id,
        port,
        main_process,
        token_a,
        repo_b_task_id: repo_b_task.id,
    }
}

#[tokio::test]
async fn client_a_cannot_discover_repo_b_through_list_tools() {
    let mut fixture = set_up_two_repo_two_clients();
    let (mut edge_process, edge_client) = spawn_edge(fixture.port, &fixture.token_a).await;

    let repos = edge_client
        .call_tool(CallToolRequestParams::new("conductor_list_repositories"))
        .await
        .expect("list_repositories")
        .structured_content
        .expect("structured content");
    let listed: Vec<String> = repos["repositories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect();
    assert!(listed.contains(&fixture.repo_a_id), "{repos:#?}");
    assert_eq!(
        listed.len(),
        1,
        "repo B must be omitted entirely: {repos:#?}"
    );

    let tasks = edge_client
        .call_tool(CallToolRequestParams::new("conductor_list_tasks"))
        .await
        .expect("list_tasks")
        .structured_content
        .expect("structured content");
    let task_ids: Vec<String> = tasks["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_str().unwrap().to_string())
        .collect();
    assert!(
        !task_ids.contains(&fixture.repo_b_task_id),
        "Client B's task must be omitted from the list entirely: {tasks:#?}"
    );

    edge_client.cancel().await.ok();
    let _ = edge_process.kill().await;
    let _ = fixture.main_process.kill();
    let _ = fixture.main_process.wait();
}

#[tokio::test]
async fn client_a_cannot_read_or_act_on_repo_bs_task() {
    let mut fixture = set_up_two_repo_two_clients();
    let (mut edge_process, edge_client) = spawn_edge(fixture.port, &fixture.token_a).await;
    let task_id = fixture.repo_b_task_id.clone();

    let get_task = edge_client
        .call_tool(
            CallToolRequestParams::new("conductor_get_task").with_arguments(
                serde_json::json!({"task_id": task_id})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await;
    assert!(get_task.is_err(), "conductor_get_task: {get_task:#?}");

    let evidence_summary = edge_client
        .call_tool(
            CallToolRequestParams::new("conductor_get_evidence_summary").with_arguments(
                serde_json::json!({"task_id": task_id})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await;
    assert!(
        evidence_summary.is_err(),
        "conductor_get_evidence_summary: {evidence_summary:#?}"
    );

    let artifact = edge_client
        .call_tool(
            CallToolRequestParams::new("conductor_get_artifact").with_arguments(
                serde_json::json!({"task_id": task_id, "artifact": "diff"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await;
    assert!(artifact.is_err(), "conductor_get_artifact: {artifact:#?}");

    let approval_requirements = edge_client
        .call_tool(
            CallToolRequestParams::new("conductor_get_approval_requirements").with_arguments(
                serde_json::json!({"task_id": task_id})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await;
    assert!(
        approval_requirements.is_err(),
        "conductor_get_approval_requirements: {approval_requirements:#?}"
    );

    let approve = edge_client
        .call_tool(
            CallToolRequestParams::new("conductor_approve").with_arguments(
                serde_json::json!({
                    "task_id": task_id, "revision": 1, "digest": "irrelevant", "principal": "irrelevant"
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        )
        .await;
    assert!(approve.is_err(), "conductor_approve: {approve:#?}");

    let resume = edge_client
        .call_tool(
            CallToolRequestParams::new("conductor_resume").with_arguments(
                serde_json::json!({"task_id": task_id, "revision": 1})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await;
    assert!(resume.is_err(), "conductor_resume: {resume:#?}");

    let cancel = edge_client
        .call_tool(
            CallToolRequestParams::new("conductor_cancel_task").with_arguments(
                serde_json::json!({"task_id": task_id, "revision": 1})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await;
    assert!(cancel.is_err(), "conductor_cancel_task: {cancel:#?}");

    edge_client.cancel().await.ok();
    let _ = edge_process.kill().await;
    let _ = fixture.main_process.kill();
    let _ = fixture.main_process.wait();
}

#[tokio::test]
async fn client_a_cannot_create_a_follow_up_referencing_an_inaccessible_parent() {
    let mut fixture = set_up_two_repo_two_clients();
    let (mut edge_process, edge_client) = spawn_edge(fixture.port, &fixture.token_a).await;

    let follow_up_args = serde_json::json!({
        "backend": "fake-process-v1",
        "principal": "irrelevant",
        "repository_id": fixture.repo_a_id,
        "objective": "Should never be created",
        "acceptance": "n/a",
        "contents": "",
        "budget": 4096,
        "follow_up_of": fixture.repo_b_task_id,
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
        "a follow-up referencing an inaccessible parent task must be refused: {result:#?}"
    );

    edge_client.cancel().await.ok();
    let _ = edge_process.kill().await;
    let _ = fixture.main_process.kill();
    let _ = fixture.main_process.wait();
}
