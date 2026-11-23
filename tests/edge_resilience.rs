//! M12.0 Phase 4 "required transport behavior" acceptance (and Phase 7B's "edge restart
//! preserves task state" / "main Conductor restart preserves task state" lines): the edge
//! is deliberately stateless (`docs/m12/M12_0_ARCHITECTURE.md` section 2 - "prefer
//! stateless edge behavior"), so a crashed/restarted edge process loses nothing because it
//! never held anything; all real state lives on the main Conductor's `state.sqlite`, which
//! a restart of `dev-conductor serve` itself reopens unchanged. Both proven here against
//! real separate OS processes, loopback only - see `docs/m12/M12_0_IMPLEMENTATION.md`'s
//! Phase 4 section for why real-network reachability is a separate, not-yet-taken step.

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

fn fixture_repo(root: &Path) -> PathBuf {
    let repo = root.join("repo");
    fs::create_dir_all(&repo).unwrap();
    dev_conductor::repository::git(&repo, &["init", "-b", "main"]).unwrap();
    dev_conductor::repository::git(&repo, &["config", "core.autocrlf", "false"]).unwrap();
    fs::write(repo.join("README.md"), "# Resilience fixture\n").unwrap();
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

fn wait_for_port_free(port: u16) {
    for _ in 0..100 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("port {port} never became free");
}

fn start_main(state: &Path, port: u16) -> std::process::Child {
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
    process
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

/// The edge is stateless by design - killing one mid-task and starting a brand-new process
/// (a different PID, no shared memory with the old one) loses nothing, because nothing was
/// ever held there. A retried `conductor_submit_task` with the same idempotency key through
/// the *new* process still returns the *original* task, proving state genuinely lives on
/// the main Conductor, not in the edge process that happened to forward the first call.
#[tokio::test]
async fn edge_process_restart_loses_nothing_and_does_not_duplicate_the_task() {
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

    let port = free_port();
    let mut main_process = start_main(&state, port);

    let (mut edge_one, client_one) = spawn_edge(port, &token).await;
    let args = serde_json::json!({
        "backend": "fake-process-v1",
        "principal": "irrelevant",
        "repository_id": repository_id,
        "objective": "Survive an edge restart",
        "acceptance": "n/a",
        "contents": "",
        "budget": 4096,
        "workflow_id": "RESTART-1",
        "idempotency_key": "attempt-1",
    })
    .as_object()
    .unwrap()
    .clone();
    let first = client_one
        .call_tool(CallToolRequestParams::new("conductor_submit_task").with_arguments(args.clone()))
        .await
        .expect("submit via the first edge process")
        .structured_content
        .expect("structured content");
    let task_id = first["task"]["id"].as_str().unwrap().to_string();

    // Kill the first edge process outright - no graceful shutdown, no drain.
    client_one.cancel().await.ok();
    let _ = edge_one.kill().await;

    // A second, independent edge process (different PID, shares nothing with the first)
    // picks up where the task actually lives: the main Conductor.
    let (mut edge_two, client_two) = spawn_edge(port, &token).await;
    let fetched = client_two
        .call_tool(
            CallToolRequestParams::new("conductor_get_task").with_arguments(
                serde_json::json!({"task_id": task_id})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .expect("get_task via the second edge process")
        .structured_content
        .expect("structured content");
    assert_eq!(fetched["task"]["id"], task_id);

    let replayed = client_two
        .call_tool(CallToolRequestParams::new("conductor_submit_task").with_arguments(args))
        .await
        .expect("retried submit via the second edge process")
        .structured_content
        .expect("structured content");
    assert_eq!(replayed["replayed"], true, "{replayed:#?}");
    assert_eq!(
        replayed["task"]["id"], task_id,
        "must not create a second task"
    );

    client_two.cancel().await.ok();
    let _ = edge_two.kill().await;
    let _ = main_process.kill();
    let _ = main_process.wait();
}

/// Restarting the main Conductor itself (the actual state owner) preserves every task - a
/// fresh `dev-conductor serve` process reopening the same `state.sqlite` sees exactly what
/// was there before, reachable through a (also fresh) edge process the same way.
#[tokio::test]
async fn main_conductor_restart_preserves_task_state_reachable_through_a_fresh_edge() {
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

    let port = free_port();
    let mut main_process = start_main(&state, port);

    let (mut edge_one, client_one) = spawn_edge(port, &token).await;
    let args = serde_json::json!({
        "backend": "fake-process-v1",
        "principal": "irrelevant",
        "repository_id": repository_id,
        "objective": "Survive a main Conductor restart",
        "acceptance": "n/a",
        "contents": "",
        "budget": 4096,
    })
    .as_object()
    .unwrap()
    .clone();
    let created = client_one
        .call_tool(CallToolRequestParams::new("conductor_submit_task").with_arguments(args))
        .await
        .expect("submit before the restart")
        .structured_content
        .expect("structured content");
    let task_id = created["task"]["id"].as_str().unwrap().to_string();

    client_one.cancel().await.ok();
    let _ = edge_one.kill().await;
    let _ = main_process.kill();
    let _ = main_process.wait();
    wait_for_port_free(port);

    let mut main_process_restarted = start_main(&state, port);
    let (mut edge_two, client_two) = spawn_edge(port, &token).await;
    let fetched = client_two
        .call_tool(
            CallToolRequestParams::new("conductor_get_task").with_arguments(
                serde_json::json!({"task_id": task_id})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .expect("get_task after the restart")
        .structured_content
        .expect("structured content");
    assert_eq!(fetched["task"]["id"], task_id);
    assert_eq!(fetched["task"]["status"], "awaiting_approval");

    client_two.cancel().await.ok();
    let _ = edge_two.kill().await;
    let _ = main_process_restarted.kill();
    let _ = main_process_restarted.wait();
}
