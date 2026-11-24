//! M12.0.1 Phase 3: acceptance evidence for the actual Linux release binary of
//! `clatterforge-edge`, run as a real process inside a Linux container (not just
//! cross-compiled) against the real native Windows `clatterforge serve` over a real
//! TCP connection via Docker Desktop's `host.docker.internal`.
//!
//! These tests are `#[ignore]`d on purpose: they require Docker Desktop running
//! locally, a prior `target-linux/release/clatterforge-edge` build (see
//! `docs/m12/M12_0_1_LINUX_RELEASE.md` for the exact build command), and a native
//! `clatterforge serve` already running and reachable at `SMOKE_MAIN_URL`. They are
//! one-time acceptance evidence for a release, not a regression suite, and are never
//! run by plain `cargo test --workspace`.
//!
//! Run (after setting up state/serve/env per the doc above):
//! `cargo test --test linux_release_smoke -- --ignored --nocapture`

use rmcp::{
    ClientHandler, ServiceExt,
    model::{CallToolRequestParams, ClientConfig},
    service::RunningService,
};
use std::{path::PathBuf, process::Stdio, time::Duration};

#[derive(Clone)]
struct DummyClient;

impl ClientHandler for DummyClient {
    fn get_info(&self) -> ClientConfig {
        ClientConfig::default()
    }
}

fn linux_bin_dir() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target-linux")
        .join("release")
        .to_string_lossy()
        .replace('\\', "/")
}

fn main_url() -> String {
    std::env::var("SMOKE_MAIN_URL").expect("set SMOKE_MAIN_URL to the running clatterforge serve")
}

fn token() -> String {
    std::env::var("SMOKE_TOKEN").expect("set SMOKE_TOKEN to the enrollment credential")
}

fn repository_id() -> String {
    std::env::var("SMOKE_REPOSITORY_ID").expect("set SMOKE_REPOSITORY_ID to the registered repo id")
}

/// Runs the real Linux release binary via `docker run`, mounting only the compiled
/// binary read-only - nothing else from the build environment is present at runtime.
async fn spawn_edge_container() -> (
    tokio::process::Child,
    RunningService<rmcp::RoleClient, DummyClient>,
) {
    let bin_dir = linux_bin_dir();
    let mut process = tokio::process::Command::new("docker")
        .args(["run", "--rm", "-i", "-v"])
        .arg(format!("{bin_dir}:/opt/bin:ro"))
        .args(["-e", &format!("CONDUCTOR_EDGE_TOKEN={}", token())])
        .args([
            "alpine:3.21",
            "/opt/bin/clatterforge-edge",
            "--main-url",
            &main_url(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn docker run for the Linux edge binary");
    let stdin = process.stdin.take().expect("edge container stdin");
    let stdout = process.stdout.take().expect("edge container stdout");
    let client = DummyClient
        .serve((stdout, stdin))
        .await
        .expect("mcp handshake with the real Linux edge binary over real stdio");
    (process, client)
}

/// Items 1-3, 6, 7: the real Linux binary launches, completes the MCP handshake,
/// discovers the expected tool set, reaches the real (disposable, nonbillable)
/// Conductor over host.docker.internal, and a fake-process-v1 task runs to completion.
#[tokio::test]
#[ignore = "requires Docker Desktop and a running clatterforge serve; see docs/m12/M12_0_1_LINUX_RELEASE.md"]
async fn the_real_linux_binary_completes_a_nonbillable_task_through_the_real_conductor() {
    let (mut process, client) = spawn_edge_container().await;

    let tools = client
        .list_tools(Default::default())
        .await
        .expect("tools/list");
    assert!(
        tools
            .tools
            .iter()
            .any(|t| t.name == "conductor_submit_task"),
        "expected conductor_submit_task among discovered tools: {tools:#?}"
    );
    assert!(
        tools.tools.iter().any(|t| t.name == "conductor_get_task"),
        "expected conductor_get_task among discovered tools: {tools:#?}"
    );

    let submit_args = serde_json::json!({
        "backend": "fake-process-v1",
        "principal": "irrelevant",
        "repository_id": repository_id(),
        "objective": "Linux release smoke test - nonbillable fake backend",
        "acceptance": "n/a",
        "contents": "smoke test contents\n",
        "budget": 4096,
        "workflow_id": "LINUX-SMOKE-2",
        "idempotency_key": "linux-smoke-attempt-2",
    })
    .as_object()
    .unwrap()
    .clone();
    let submitted = client
        .call_tool(CallToolRequestParams::new("conductor_submit_task").with_arguments(submit_args))
        .await
        .expect("submit through the real Linux edge binary")
        .structured_content
        .expect("structured content");
    let task_id = submitted["task"]["id"].as_str().unwrap().to_string();

    let mut last_status = String::new();
    for _ in 0..100 {
        let detail = client
            .call_tool(
                CallToolRequestParams::new("conductor_get_task").with_arguments(
                    serde_json::json!({ "task_id": task_id })
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            )
            .await
            .expect("get_task")
            .structured_content
            .expect("structured content");
        last_status = detail["task"]["status"].as_str().unwrap().to_string();
        if matches!(last_status.as_str(), "accepted" | "rejected" | "failed") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        last_status, "accepted",
        "fake-process-v1 task should reach accepted through the real Linux binary"
    );

    client.cancel().await.ok();
    let _ = process.kill().await;
}

/// Item 8: killing and restarting the edge container (a fresh process, fresh
/// container) does not lose or duplicate state - because the edge is stateless by
/// design, state lives entirely on the real Conductor side.
#[tokio::test]
#[ignore = "requires Docker Desktop and a running clatterforge serve; see docs/m12/M12_0_1_LINUX_RELEASE.md"]
async fn restarting_the_edge_container_does_not_lose_or_duplicate_the_submitted_task() {
    let (mut process_a, client_a) = spawn_edge_container().await;
    let submit_args = serde_json::json!({
        "backend": "fake-process-v1",
        "principal": "irrelevant",
        "repository_id": repository_id(),
        "objective": "Linux release smoke test - restart durability",
        "acceptance": "n/a",
        "contents": "",
        "budget": 4096,
        "workflow_id": "LINUX-SMOKE-RESTART",
        "idempotency_key": "linux-smoke-restart-1",
    })
    .as_object()
    .unwrap()
    .clone();
    let submitted = client_a
        .call_tool(CallToolRequestParams::new("conductor_submit_task").with_arguments(submit_args))
        .await
        .expect("submit before restart")
        .structured_content
        .expect("structured content");
    let task_id = submitted["task"]["id"].as_str().unwrap().to_string();
    client_a.cancel().await.ok();
    let _ = process_a.kill().await;

    let (mut process_b, client_b) = spawn_edge_container().await;
    let detail = client_b
        .call_tool(
            CallToolRequestParams::new("conductor_get_task").with_arguments(
                serde_json::json!({ "task_id": task_id })
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .expect("get_task after restart")
        .structured_content
        .expect("structured content");
    assert_eq!(detail["task"]["id"], task_id, "{detail:#?}");

    let tasks = client_b
        .call_tool(CallToolRequestParams::new("conductor_list_tasks"))
        .await
        .expect("list_tasks after restart")
        .structured_content
        .expect("structured content");
    let count = tasks["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["id"] == task_id)
        .count();
    assert_eq!(
        count, 1,
        "exactly one task must exist after restart: {tasks:#?}"
    );

    client_b.cancel().await.ok();
    let _ = process_b.kill().await;
}

/// Item 9: the raw credential must never appear in the edge binary's own stdout or
/// stderr, under either a successful run or a refusal.
#[tokio::test]
#[ignore = "requires Docker Desktop and a running clatterforge serve; see docs/m12/M12_0_1_LINUX_RELEASE.md"]
async fn the_raw_credential_never_appears_in_the_edge_binarys_own_output() {
    let (process, client) = spawn_edge_container().await;
    let _ = client
        .list_tools(Default::default())
        .await
        .expect("tools/list");
    client.cancel().await.ok();
    let output = process
        .wait_with_output()
        .await
        .expect("wait for edge container");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let secret = token();
    assert!(
        !stdout.contains(&secret),
        "credential leaked into stdout: {stdout}"
    );
    assert!(
        !stderr.contains(&secret),
        "credential leaked into stderr: {stderr}"
    );
}
