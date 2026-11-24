//! M12.0 Phase 5 acceptance (the brief's own Phase 7D: "run a deterministic coding-agent
//! fixture through the remote edge path and the existing Agents engine... verify that the
//! actual Windows execution machinery is used"). Uses the same deterministic fake
//! `cursor-agent` substitute `tests/agents.rs` already uses (`tests/fixtures/
//! fake_cursor_agent.rs`) - never spends a real credit - driven this time through a real
//! `dev-conductor-edge` process and a real authenticated enrollment, proving the full
//! chain: edge -> `/api/edge/tools/call` -> `agent_preset::AgentPresets` -> the unchanged
//! `agents::Agents` engine -> a real (fake) external process -> independent verification
//! -> evidence -> explicit human-equivalent review acceptance.
//! Bot stand-in scenarios also route through the real local edge gateway and seed
//! pre-tier saved tasks to exercise legacy cross-engine lookup and retry recovery.

use dev_conductor::{
    agent_preset::AgentPresets,
    agents::{AgentBackend, CursorAgentPolicy},
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
    process::{Command, Stdio},
    sync::OnceLock,
    time::Duration,
};
use tempfile::TempDir;

#[derive(Clone)]
struct DummyClient;

impl ClientHandler for DummyClient {
    fn get_info(&self) -> ClientConfig {
        ClientConfig::default()
    }
}

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

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Always rebuilds (cheap no-op via Cargo's cache when nothing changed) rather than only
/// building when missing - a stale binary in a separate workspace member can silently
/// drop a wire field nothing else would catch. Keep it alongside this test's main
/// binary so an alternate target directory never tests an unrelated installed edge.
fn edge_binary() -> PathBuf {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let target_dir = Path::new(env!("CARGO_BIN_EXE_dev-conductor"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let status = std::process::Command::new(env!("CARGO"))
        .args([
            "build",
            "--locked",
            "-p",
            "dev-conductor-edge",
            "--target-dir",
        ])
        .arg(target_dir)
        .current_dir(manifest_dir)
        .status()
        .expect("run cargo build -p dev-conductor-edge");
    assert!(status.success(), "building dev-conductor-edge failed");
    target_dir.join("debug").join(format!(
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

/// Polls `conductor_get_task` through `client` until `status` is no longer `approved` or
/// `running` (the two transient states between dispatch and a terminal outcome), or the
/// bound is hit. The fake agent + fake verifier each complete in well under a second
/// (`tests/agents.rs`'s own synchronous equivalent proves this); this bound is generous
/// headroom, not a tuned minimum.
async fn poll_until_settled(
    client: &RunningService<rmcp::RoleClient, DummyClient>,
    task_id: &str,
) -> serde_json::Value {
    for _ in 0..100 {
        let fetched = client
            .call_tool(
                CallToolRequestParams::new("conductor_get_task").with_arguments(
                    serde_json::json!({"task_id": task_id})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            )
            .await
            .expect("conductor_get_task while polling")
            .structured_content
            .expect("structured content");
        let status = fetched["task"]["status"].as_str().unwrap_or("");
        if !matches!(status, "approved" | "running" | "verifying") {
            return fetched;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("task {task_id} never settled out of approved/running/verifying");
}

#[tokio::test]
async fn a_deterministic_cursor_agent_fixture_runs_to_awaiting_review_through_the_real_edge_path() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture_repo(dir.path());
    let state = dir.path().join("state");
    let repo_identity = Repository::inspect(&repo).unwrap().identity;
    Store::open(&state).unwrap().register(&repo).unwrap();

    let backend = AgentBackend::CursorAgent(
        CursorAgentPolicy::inspect(fake_node(), &index_js(), "fake-model".into()).unwrap(),
    );
    AgentPresets::open(&state)
        .unwrap()
        .create(
            "fixture-cursor-preset",
            repo.clone(),
            backend,
            vec!["src/result.txt".into()],
            vec!["verification.txt".into()],
            verifier().into(),
            vec![],
            5_000,
            5_000,
            None,
            "mike",
        )
        .unwrap();

    let (enrollment, token) = Enrollments::open(&state)
        .unwrap()
        .create(
            "Hitch",
            "grok-bot",
            vec![
                EnrollmentOperation::Inspect,
                EnrollmentOperation::Submit,
                EnrollmentOperation::Approve,
            ],
            vec![repo_identity.clone()],
            "mike",
        )
        .unwrap();
    Grants::open(&state)
        .unwrap()
        .create(
            &enrollment::namespaced_principal(&enrollment.client_id),
            &repo_identity,
            vec!["cursor-agent".into()],
            vec![Operation::Submit, Operation::Resume],
            ExecutionMode::HoldMyBeer,
            "mike",
            5,
            now() + 30 * 60_000,
        )
        .unwrap();

    let port = free_port();
    let mut main_process = start_main(&state, port);
    let (mut edge_process, client) = spawn_edge(port, &token).await;

    let submit_args = serde_json::json!({
        "backend": "cursor-agent",
        "principal": "irrelevant-the-server-overrides-this",
        "agent_preset": "fixture-cursor-preset",
        "objective": "fix the result via a real (fake) coding agent through the edge",
        "acceptance": "verifier passes",
    })
    .as_object()
    .unwrap()
    .clone();
    let submitted = client
        .call_tool(CallToolRequestParams::new("conductor_submit_task").with_arguments(submit_args))
        .await
        .expect("conductor_submit_task through the edge")
        .structured_content
        .expect("structured content");

    // The submit call itself must not have blocked for the agent's full run - it returns
    // with the task still "approved" (resume dispatched asynchronously), never already at
    // a terminal outcome synchronously within this one call.
    assert_eq!(
        submitted["task"]["status"], "approved",
        "submit must return immediately once resume is dispatched async, not block for the agent run: {submitted:#?}"
    );
    let steps: Vec<_> = submitted["authorization"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["operation"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(steps, vec!["approve", "resume"]);
    let task_id = submitted["task"]["id"].as_str().unwrap().to_string();

    let settled = poll_until_settled(&client, &task_id).await;
    assert_eq!(settled["task"]["status"], "awaiting_review", "{settled:#?}");
    assert!(
        settled["task"]["evidence_digest"].is_string(),
        "a completed agent run must produce real evidence: {settled:#?}"
    );
    // Confirms the real worktree-isolated agent actually ran: the registered repository's
    // own checkout file is untouched, exactly like the synchronous equivalent in
    // tests/agents.rs checks.
    assert_eq!(
        fs::read_to_string(repo.join("src/result.txt")).unwrap(),
        "wrong",
        "the agent must edit only its own isolated worktree, never the registered repository"
    );

    let evidence_digest = settled["task"]["evidence_digest"]
        .as_str()
        .unwrap()
        .to_string();
    let revision = settled["task"]["revision"].as_u64().unwrap();
    let accept_args = serde_json::json!({
        "task_id": task_id,
        "revision": revision,
        "digest": evidence_digest,
        "principal": "irrelevant-the-server-overrides-this",
    })
    .as_object()
    .unwrap()
    .clone();
    let accepted = client
        .call_tool(CallToolRequestParams::new("conductor_approve").with_arguments(accept_args))
        .await
        .expect("conductor_approve through the edge")
        .structured_content
        .expect("structured content");
    assert_eq!(accepted["task"]["status"], "accepted", "{accepted:#?}");

    client.cancel().await.ok();
    let _ = edge_process.kill().await;
    let _ = main_process.kill();
    let _ = main_process.wait();
}

struct TestProcess(std::process::Child);

impl Drop for TestProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

// Own both servers so an assertion failure cannot leave fixture listeners behind.
struct BotFixture {
    _gateway: TestProcess,
    _main: TestProcess,
    state: PathBuf,
    repo: PathBuf,
    port: u16,
    token: String,
    principal: String,
    _dir: TempDir,
}

impl BotFixture {
    fn new(expected: &str, verify_args: Vec<String>) -> Self {
        Self::with_limit(expected, verify_args, 5)
    }

    fn with_limit(expected: &str, verify_args: Vec<String>, max_tasks: usize) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let repo = fixture_repo(dir.path());
        fs::write(repo.join("verification.txt"), expected).unwrap();
        fs::write(
            repo.join("Cargo.lock"),
            "# protected lockfile\n".repeat(8_509),
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
                "--allow-empty",
                "-m",
                "contract",
            ],
        )
        .unwrap();
        let state = dir.path().join("state");
        let mut store = Store::open(&state).unwrap();
        let (repository_id, repository) = store.register(&repo).unwrap();

        // Real pre-tier wire shape in an unrelated stored task, including the legacy
        // context digest. No production database is copied or modified.
        let legacy = store
            .create(
                &repository_id,
                "old fixture".into(),
                "n/a".into(),
                "legacy".into(),
                4096,
                dev_conductor::process::ExecutionPolicy::new(Path::new(env!(
                    "CARGO_BIN_EXE_dev-conductor"
                )))
                .unwrap(),
            )
            .unwrap();
        let mut record = serde_json::to_value(&legacy).unwrap();
        for item in record["assignment"]["context"]["items"]
            .as_array_mut()
            .unwrap()
        {
            item.as_object_mut().unwrap().remove("tier");
        }
        let context = &record["assignment"]["context"];
        record["assignment"]["context"]["digest"] = dev_conductor::digest(
            &serde_json::to_vec(&(
                &context["objective"],
                &context["repository_identity"],
                &context["base"],
                &context["budget_bytes"],
                &context["items"],
            ))
            .unwrap(),
        )
        .into();
        rusqlite::Connection::open(state.join("state.sqlite"))
            .unwrap()
            .execute(
                "UPDATE tasks SET record=?1 WHERE id=?2",
                rusqlite::params![serde_json::to_string(&record).unwrap(), legacy.id],
            )
            .unwrap();
        Self::preset(&state, &repo, verify_args);
        let (enrollment, token) = Enrollments::open(&state)
            .unwrap()
            .create(
                "Grok Bot stand-in",
                "grok-bot",
                vec![
                    EnrollmentOperation::Inspect,
                    EnrollmentOperation::Submit,
                    EnrollmentOperation::Resume,
                    EnrollmentOperation::Cancel,
                ],
                vec![repository.identity.clone()],
                "fixture-admin",
            )
            .unwrap();
        let principal = enrollment::namespaced_principal(&enrollment.client_id);
        Grants::open(&state)
            .unwrap()
            .create(
                &principal,
                &repository.identity,
                vec!["cursor-agent".into()],
                vec![Operation::Submit, Operation::Resume, Operation::Cancel],
                ExecutionMode::Delegated,
                "fixture-admin",
                max_tasks,
                now() + 300_000,
            )
            .unwrap();
        let main_port = free_port();
        let main = TestProcess(start_main(&state, main_port));
        let port = free_port();
        let gateway = TestProcess(
            Command::new(env!("CARGO_BIN_EXE_dev-conductor"))
                .args([
                    "edge-gateway",
                    "--port",
                    &port.to_string(),
                    "--upstream",
                    &format!("http://127.0.0.1:{main_port}"),
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        for _ in 0..100 {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return Self {
                    _gateway: gateway,
                    _main: main,
                    state,
                    repo,
                    port,
                    token,
                    principal,
                    _dir: dir,
                };
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("fixture gateway never became ready");
    }

    fn preset(state: &Path, repo: &Path, verify_args: Vec<String>) {
        AgentPresets::open(state)
            .unwrap()
            .create(
                "bot-preset",
                repo.into(),
                AgentBackend::CursorAgent(
                    CursorAgentPolicy::inspect(fake_node(), &index_js(), "fake-model".into())
                        .unwrap(),
                ),
                vec!["src/result.txt".into()],
                vec!["verification.txt".into(), "Cargo.lock".into()],
                verifier().into(),
                verify_args,
                10_000,
                10_000,
                None,
                "fixture-admin",
            )
            .unwrap();
    }

    fn submission(&self, objective: &str) -> serde_json::Value {
        serde_json::json!({"backend":"cursor-agent", "principal":"spoofed-admin",
            "agent_preset":"bot-preset", "objective":objective, "acceptance":"protected verifier passes",
            "workflow_id":"bot-e2e", "idempotency_key":"attempt-1",
            "source_system":"fixture", "source_record_reference":"fixture:1", "correlation_id":"bot-e2e-1"})
    }
}

async fn bot_call(
    client: &RunningService<rmcp::RoleClient, DummyClient>,
    tool: &'static str,
    args: serde_json::Value,
) -> serde_json::Value {
    let result = client
        .call_tool(
            CallToolRequestParams::new(tool).with_arguments(args.as_object().unwrap().clone()),
        )
        .await
        .unwrap();
    assert_ne!(result.is_error, Some(true), "{tool}: {result:?}");
    result.structured_content.unwrap()
}

async fn bot_refused(
    client: &RunningService<rmcp::RoleClient, DummyClient>,
    tool: &'static str,
    args: serde_json::Value,
) {
    let result = client
        .call_tool(
            CallToolRequestParams::new(tool).with_arguments(args.as_object().unwrap().clone()),
        )
        .await;
    assert!(
        result.is_err() || result.as_ref().is_ok_and(|r| r.is_error == Some(true)),
        "{tool} must be refused: {result:?}"
    );
}

#[tokio::test]
async fn bot_replays_legacy_state_after_reconnect_and_a_human_reviews_the_real_diff() {
    let fixture = BotFixture::new("fixed by fake cursor-agent", vec![]);
    let (mut edge, client) = spawn_edge(fixture.port, &fixture.token).await;
    let capabilities = bot_call(&client, "conductor_capabilities", serde_json::json!({})).await;
    assert_eq!(capabilities["standing_delegation_supported"], true);
    let args = fixture.submission("fix the fixture result");
    let submitted = bot_call(&client, "conductor_submit_task", args.clone()).await;
    let id = submitted["task"]["id"].as_str().unwrap();
    assert_eq!(
        submitted["metadata"]["initiating_client"],
        fixture.principal
    );
    let settled = poll_until_settled(&client, id).await;
    assert_eq!(settled["task"]["status"], "awaiting_review", "{settled:#?}");
    let requirements = bot_call(
        &client,
        "conductor_get_approval_requirements",
        serde_json::json!({"task_id":id}),
    )
    .await;
    assert_eq!(requirements["approval_required"], true);
    assert!(requirements["covered_by_grant"].is_null());
    bot_refused(&client, "conductor_approve", serde_json::json!({"task_id":id,
        "revision":settled["task"]["revision"], "digest":settled["task"]["evidence_digest"], "principal":"fixture-admin"})).await;

    let summary = bot_call(
        &client,
        "conductor_get_evidence_summary",
        serde_json::json!({"task_id":id,"log_excerpt_bytes":16}),
    )
    .await;
    assert_eq!(summary["verification_outcome"], "succeeded");
    assert_eq!(summary["has_verifier_output"], true);
    assert_eq!(summary["progress"]["phase"], "verification");
    assert_eq!(summary["progress"]["finished"], true);
    assert_eq!(summary["progress"]["stop_reason"], "succeeded");
    assert!(
        !summary["log_excerpt"].as_str().unwrap().is_empty(),
        "empty stderr must not hide the verifier's useful stdout: {summary:#?}"
    );
    let mut diff = String::new();
    let mut offset = 0;
    loop {
        let chunk = bot_call(
            &client,
            "conductor_get_artifact",
            serde_json::json!({"task_id":id,"artifact":"diff","offset":offset,"max_bytes":17}),
        )
        .await;
        diff.push_str(chunk["text"].as_str().unwrap());
        offset += chunk["shown_bytes"].as_u64().unwrap();
        if chunk["has_more"] == false {
            break;
        }
        assert!(
            offset < 4096,
            "fixture pagination must advance and terminate"
        );
    }
    assert!(diff.contains("+fixed by fake cursor-agent"), "{diff}");

    client.cancel().await.ok();
    edge.kill().await.ok();
    let (mut edge, client) = spawn_edge(fixture.port, &fixture.token).await;
    let replayed = bot_call(&client, "conductor_submit_task", args).await;
    assert_eq!(replayed["replayed"], true);
    assert_eq!(replayed["task"]["id"], id);
    assert_eq!(replayed["task"]["revision"], settled["task"]["revision"]);
    let mut agents = dev_conductor::agents::Agents::open(&fixture.state).unwrap();
    assert_eq!(agents.tasks().unwrap().len(), 1);
    let task = agents.task(id).unwrap();
    assert!(
        agents
            .review(
                id,
                task.revision - 1,
                summary["evidence_digest"].as_str().unwrap(),
                "fixture-human",
                true
            )
            .is_err()
    );
    assert!(
        agents
            .review(id, task.revision, "wrong-digest", "fixture-human", true)
            .is_err()
    );
    agents
        .review(
            id,
            task.revision,
            summary["evidence_digest"].as_str().unwrap(),
            "fixture-human",
            true,
        )
        .unwrap();
    let accepted = bot_call(
        &client,
        "conductor_get_task",
        serde_json::json!({"task_id":id}),
    )
    .await;
    assert_eq!(accepted["task"]["status"], "accepted");
    assert_eq!(
        fs::read_to_string(fixture.repo.join("src/result.txt")).unwrap(),
        "wrong"
    );
    client.cancel().await.ok();
    edge.kill().await.ok();
}

#[tokio::test]
async fn bot_sees_failed_verification_and_can_submit_a_linked_follow_up() {
    let fixture = BotFixture::new("a different required result", vec![]);
    let (mut edge, client) = spawn_edge(fixture.port, &fixture.token).await;
    let submitted = bot_call(
        &client,
        "conductor_submit_task",
        fixture.submission("fix fixture"),
    )
    .await;
    let id = submitted["task"]["id"].as_str().unwrap();
    let settled = poll_until_settled(&client, id).await;
    assert_eq!(settled["task"]["status"], "needs_work", "{settled:#?}");
    let summary = bot_call(
        &client,
        "conductor_get_evidence_summary",
        serde_json::json!({"task_id":id}),
    )
    .await;
    assert_eq!(summary["verification_outcome"], "failed");
    assert_eq!(summary["has_verifier_output"], true);
    assert!(
        summary["log_excerpt"]
            .as_str()
            .unwrap()
            .contains("a different required result"),
        "{summary:#?}"
    );
    assert!(
        summary["failing_check"]
            .as_str()
            .unwrap()
            .contains("verification")
    );
    let task = dev_conductor::agents::Agents::open(&fixture.state)
        .unwrap()
        .task(id)
        .unwrap();
    assert_eq!(task.execution.unwrap().outcome, "succeeded");
    assert_eq!(task.verification.unwrap().exit_code, Some(7));
    assert!(
        dev_conductor::agents::Agents::open(&fixture.state)
            .unwrap()
            .review(
                id,
                task.revision,
                summary["evidence_digest"].as_str().unwrap(),
                "fixture-human",
                true
            )
            .is_err()
    );

    // The fixture maintainer corrects the protected contract for a new base. A
    // follow-up captures that base; the failed task retains its original evidence.
    fs::write(
        fixture.repo.join("verification.txt"),
        "fixed by fake cursor-agent",
    )
    .unwrap();
    git(&fixture.repo, &["add", "."]).unwrap();
    git(
        &fixture.repo,
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
            "correct fixture contract",
        ],
    )
    .unwrap();
    let mut args = fixture.submission("follow up without replaying the failure");
    args["idempotency_key"] = "attempt-2".into();
    args["follow_up_of"] = id.into();
    let follow_up = bot_call(&client, "conductor_submit_task", args).await;
    assert_ne!(follow_up["task"]["id"], id);
    assert_eq!(follow_up["metadata"]["follow_up_of"], id);
    let repaired = poll_until_settled(&client, follow_up["task"]["id"].as_str().unwrap()).await;
    assert_eq!(
        repaired["task"]["status"], "awaiting_review",
        "{repaired:#?}"
    );
    assert_eq!(
        dev_conductor::agents::Agents::open(&fixture.state)
            .unwrap()
            .task(id)
            .unwrap()
            .revision,
        task.revision
    );
    client.cancel().await.ok();
    edge.kill().await.ok();
}

#[tokio::test]
async fn bot_cancels_a_running_agent_and_the_repository_lease_is_released() {
    let fixture = BotFixture::new("fixed by fake cursor-agent", vec![]);
    let (mut edge, client) = spawn_edge(fixture.port, &fixture.token).await;
    let args = fixture.submission("MODE:hang");
    let submitted = bot_call(&client, "conductor_submit_task", args.clone()).await;
    let id = submitted["task"]["id"].as_str().unwrap();
    let mut running = None;
    for _ in 0..100 {
        let fetched = bot_call(
            &client,
            "conductor_get_task",
            serde_json::json!({"task_id":id}),
        )
        .await;
        if fetched["task"]["status"] == "running" {
            running = Some(fetched);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let running = running.expect("agent must really be running before cancellation");
    bot_refused(
        &client,
        "conductor_cancel_task",
        serde_json::json!({"task_id":id,"revision":1}),
    )
    .await;
    bot_call(
        &client,
        "conductor_cancel_task",
        serde_json::json!({"task_id":id,"revision":running["task"]["revision"]}),
    )
    .await;
    let settled = poll_until_settled(&client, id).await;
    assert_eq!(settled["task"]["status"], "cancelled", "{settled:#?}");
    let summary = bot_call(
        &client,
        "conductor_get_evidence_summary",
        serde_json::json!({"task_id":id}),
    )
    .await;
    assert_eq!(summary["has_verifier_output"], false);
    let replayed = bot_call(&client, "conductor_submit_task", args).await;
    assert_eq!(replayed["task"]["status"], "cancelled");
    let mut next = fixture.submission("fresh task after cancellation");
    next["idempotency_key"] = "attempt-2".into();
    let submitted = bot_call(&client, "conductor_submit_task", next).await;
    let settled = poll_until_settled(&client, submitted["task"]["id"].as_str().unwrap()).await;
    assert_eq!(settled["task"]["status"], "awaiting_review", "{settled:#?}");
    client.cancel().await.ok();
    edge.kill().await.ok();
}

#[tokio::test]
async fn bot_can_retry_a_rejected_submission_with_the_same_idempotency_key() {
    let fixture = BotFixture::new("fixed by fake cursor-agent", vec![]);
    let (mut edge, client) = spawn_edge(fixture.port, &fixture.token).await;
    let args = fixture.submission("retry after fixing preset");
    let mut presets = AgentPresets::open(&fixture.state).unwrap();
    presets.delete("bot-preset").unwrap();
    bot_refused(&client, "conductor_submit_task", args.clone()).await;
    assert!(
        dev_conductor::agents::Agents::open(&fixture.state)
            .unwrap()
            .tasks()
            .unwrap()
            .is_empty()
    );
    BotFixture::preset(&fixture.state, &fixture.repo, vec![]);
    let submitted = bot_call(&client, "conductor_submit_task", args.clone()).await;
    assert_eq!(submitted["replayed"], false);
    let id = submitted["task"]["id"].as_str().unwrap();
    let settled = poll_until_settled(&client, id).await;
    assert_eq!(settled["task"]["status"], "awaiting_review", "{settled:#?}");
    let replayed = bot_call(&client, "conductor_submit_task", args).await;
    assert_eq!(replayed["replayed"], true);
    assert_eq!(replayed["task"]["id"], id);
    assert_eq!(
        dev_conductor::agents::Agents::open(&fixture.state)
            .unwrap()
            .tasks()
            .unwrap()
            .len(),
        1
    );
    client.cancel().await.ok();
    edge.kill().await.ok();
}

#[tokio::test]
async fn bot_resumes_after_local_approval_and_refuses_a_stale_revision() {
    let fixture = BotFixture::new("fixed by fake cursor-agent", vec![]);
    let (mut edge, client) = spawn_edge(fixture.port, &fixture.token).await;
    let mut args = fixture.submission("recover a task created before its response failed");
    args["requested_execution_mode"] = "interactive".into();
    let submitted = bot_call(&client, "conductor_submit_task", args.clone()).await;
    let id = submitted["task"]["id"].as_str().unwrap();
    assert_eq!(submitted["task"]["status"], "awaiting_approval");
    let replayed = bot_call(&client, "conductor_submit_task", args).await;
    assert_eq!(replayed["replayed"], true);
    assert_eq!(replayed["task"]["status"], "awaiting_approval");
    let mut agents = dev_conductor::agents::Agents::open(&fixture.state).unwrap();
    let task = agents.task(id).unwrap();
    let approved = agents
        .approve(
            id,
            task.revision,
            &task.assignment.digest().unwrap(),
            "fixture-human",
        )
        .unwrap();
    bot_refused(
        &client,
        "conductor_resume",
        serde_json::json!({"task_id":id,"revision":task.revision}),
    )
    .await;
    assert_eq!(agents.task(id).unwrap().revision, approved.revision);
    bot_call(
        &client,
        "conductor_resume",
        serde_json::json!({"task_id":id,"revision":approved.revision}),
    )
    .await;
    let settled = poll_until_settled(&client, id).await;
    assert_eq!(settled["task"]["status"], "awaiting_review", "{settled:#?}");
    assert_eq!(agents.tasks().unwrap().len(), 1);
    client.cancel().await.ok();
    edge.kill().await.ok();
}

#[tokio::test]
async fn bot_evidence_excerpt_is_capped_even_when_a_silent_verifier_leaves_a_large_diff() {
    let fixture = BotFixture::new("fixed by fake cursor-agent", vec!["quiet".into()]);
    fs::write(fixture.repo.join("src/result.txt"), "x".repeat(80_000)).unwrap();
    git(&fixture.repo, &["add", "."]).unwrap();
    git(
        &fixture.repo,
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
            "large fixture diff",
        ],
    )
    .unwrap();
    let (mut edge, client) = spawn_edge(fixture.port, &fixture.token).await;
    let submitted = bot_call(
        &client,
        "conductor_submit_task",
        fixture.submission("replace large fixture result"),
    )
    .await;
    let id = submitted["task"]["id"].as_str().unwrap();
    let settled = poll_until_settled(&client, id).await;
    assert_eq!(settled["task"]["status"], "awaiting_review", "{settled:#?}");
    let summary = bot_call(
        &client,
        "conductor_get_evidence_summary",
        serde_json::json!({"task_id":id,"log_excerpt_bytes":1_000_000}),
    )
    .await;
    assert_eq!(summary["log_excerpt_label"], "diff");
    assert_eq!(summary["log_excerpt"].as_str().unwrap().len(), 65_536);
    assert_eq!(summary["log_excerpt_truncated"], true);
    let omitted = bot_call(
        &client,
        "conductor_get_evidence_summary",
        serde_json::json!({"task_id":id,"log_excerpt_bytes":0}),
    )
    .await;
    assert!(omitted["log_excerpt"].is_null());
    client.cancel().await.ok();
    edge.kill().await.ok();
}

#[tokio::test]
async fn one_slot_grant_conducts_its_admitted_task_but_cannot_start_a_second() {
    let fixture = BotFixture::with_limit("fixed by fake cursor-agent", vec![], 1);
    let (mut edge, client) = spawn_edge(fixture.port, &fixture.token).await;
    let submitted = bot_call(
        &client,
        "conductor_submit_task",
        fixture.submission("fix the fixture"),
    )
    .await;
    let id = submitted["task"]["id"].as_str().unwrap();
    assert_eq!(
        submitted["authorization"][0]["stage"],
        "assignment_approval"
    );
    assert_eq!(
        submitted["authorization"][0]["authorized_operation"],
        "submit"
    );
    assert_eq!(submitted["authorization"][0]["result_accepted"], false);
    assert_eq!(submitted["authorization"][1]["operation"], "resume");
    let done = poll_until_settled(&client, id).await;
    assert_eq!(done["task"]["status"], "awaiting_review");
    let detail = dev_conductor::service::TaskService::open(&fixture.state)
        .unwrap()
        .detail(id, None)
        .unwrap();
    let stages: Vec<_> = detail.routing.iter().map(|e| e.stage.as_str()).collect();
    for stage in [
        "assignment_created",
        "assignment_approved",
        "dispatch_requested",
        "workspace_prepared",
        "generation_launched",
        "generation_recorded",
        "scope_checked",
        "verification_launched",
        "verification_recorded",
        "patch_captured",
        "finished",
    ] {
        assert!(stages.contains(&stage), "missing {stage}: {stages:?}");
    }
    assert!(
        detail
            .routing
            .iter()
            .any(|e| e.handler == "TaskService::run")
    );
    let requirements = bot_call(
        &client,
        "conductor_get_approval_requirements",
        serde_json::json!({"task_id":id}),
    )
    .await;
    assert_eq!(requirements["decision_stage"], "result_review");
    assert!(requirements["covered_by_grant"].is_null());
    let mut second = fixture.submission("another change");
    second["idempotency_key"] = "second-admission".into();
    let blocked = bot_call(&client, "conductor_submit_task", second).await;
    assert_eq!(blocked["task"]["status"], "awaiting_approval");
    assert!(blocked["authorization"].as_array().unwrap().is_empty());
    client.cancel().await.ok();
    let _ = edge.kill().await;
}
