use clatterforge::{
    agent_preset::AgentPresets,
    agents::{AgentBackend, AgentStatus, Agents, CursorAgentPolicy},
    inference::{
        config::{Config, EndpointAccess, Target},
        types::{Capabilities, ProviderKind},
    },
    repository::git,
    service::TaskService,
    workflow::{Workflow, Workflows},
};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;

/// A temp directory plus its canonical path. Production canonicalizes every root at entry;
/// CI's Windows TEMP is an 8.3 short path (RUNNER~1), so fixtures must use the canonical
/// form or repository/workspace identity checks reject them.
fn canonical_tempdir() -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = clatterforge::repository::canonical(dir.path()).unwrap();
    (dir, root)
}

fn worker() -> &'static Path {
    static FIXTURE: OnceLock<(TempDir, PathBuf)> = OnceLock::new();
    &FIXTURE
        .get_or_init(|| {
            let (dir, dir_root) = canonical_tempdir();
            let exe = dir_root.as_path().join("worker.exe");
            assert!(
                Command::new("rustc")
                    .args(["tests/fixtures/workflow_worker.rs", "-o"])
                    .arg(&exe)
                    .status()
                    .unwrap()
                    .success()
            );
            fs::write(dir_root.as_path().join("index.js"), "//fixture").unwrap();
            (dir, exe)
        })
        .1
}
struct Model {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    hold: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Model {
    fn new(decisions: Vec<Value>) -> Self {
        Self::texts(decisions.iter().map(Value::to_string).collect())
    }
    fn texts(decisions: Vec<String>) -> Self {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", server.server_addr().to_ip().unwrap());
        let requests = Arc::new(Mutex::new(vec![]));
        let seen = requests.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let hold = Arc::new(AtomicBool::new(false));
        let held = hold.clone();
        let worker = thread::spawn(move || {
            let mut n = 0;
            while !stopping.load(Ordering::SeqCst) {
                let Some(mut req) = server.recv_timeout(Duration::from_millis(10)).unwrap() else {
                    continue;
                };
                let body = match req.url() {
                    "/api/tags" => {
                        json!({"models":[{"name":"gemma4:fixture","digest":"a".repeat(64)}]})
                            .to_string()
                    }
                    "/api/version" => json!({"version":"fixture"}).to_string(),
                    "/api/chat" => {
                        let mut input = String::new();
                        req.as_reader().read_to_string(&mut input).unwrap();
                        seen.lock()
                            .unwrap()
                            .push(serde_json::from_str(&input).unwrap());
                        while held.load(Ordering::SeqCst) && !stopping.load(Ordering::SeqCst) {
                            thread::sleep(Duration::from_millis(10));
                        }
                        let text = &decisions[n.min(decisions.len() - 1)];
                        n += 1;
                        format!(
                            "{}\n{}\n",
                            json!({"message":{"content":text},"done":false}),
                            json!({"message":{"content":""},"done":true,"done_reason":"stop","prompt_eval_count":100,"eval_count":30})
                        )
                    }
                    other => panic!("unexpected endpoint {other}"),
                };
                let _ = req.respond(tiny_http::Response::from_string(body));
            }
        });
        Self {
            url,
            requests,
            hold,
            stop,
            worker: Some(worker),
        }
    }
}
impl Drop for Model {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.worker.take().unwrap().join().unwrap();
    }
}
struct Fixture {
    _dir: TempDir,
    repo: PathBuf,
    state: PathBuf,
    workflow: Workflow,
}
struct HttpFixture(std::process::Child);
impl Drop for HttpFixture {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn dispatch(worker: &str, objective: &str) -> Value {
    json!({"action":"dispatch","worker":worker,"objective":objective,"acceptance":"independent verifier passes","rationale":"This worker owns the required file and receives previous evidence."})
}
fn fixture(model: &Model, max_steps: usize) -> Fixture {
    fixture_with_readme(model, max_steps, "fixture")
}
fn fixture_with_readme(model: &Model, max_steps: usize, readme: &str) -> Fixture {
    let (dir, dir_root) = canonical_tempdir();
    let repo = dir_root.as_path().join("repo");
    fs::create_dir(&repo).unwrap();
    for (name, contents) in [
        ("README.md", readme),
        ("a.txt", "wrong-a"),
        ("b.txt", "wrong-b"),
        (
            "contract.txt",
            "Both files must be fixed. Protected contract.",
        ),
    ] {
        fs::write(repo.join(name), contents).unwrap();
    }
    fs::write(repo.join(".gitattributes"), "*.txt text eol=lf\n").unwrap();
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
            "fixture",
        ],
    )
    .unwrap();
    let state = dir_root.as_path().join("state");
    let mut presets = AgentPresets::open(&state).unwrap();
    let policy = AgentBackend::CursorAgent(
        CursorAgentPolicy::inspect(
            worker(),
            &worker().parent().unwrap().join("index.js"),
            "fixture".into(),
        )
        .unwrap(),
    );
    let verifier = dir_root.as_path().join("verifier.exe");
    fs::copy(worker(), &verifier).unwrap();
    for (name, path) in [("worker-a", "a.txt"), ("worker-b", "b.txt")] {
        presets
            .create(
                name,
                repo.clone(),
                policy.clone(),
                vec![path.into()],
                vec!["contract.txt".into()],
                verifier.clone(),
                vec!["--verify".into()],
                5000,
                5000,
                None,
                "fixture",
            )
            .unwrap();
    }
    let config = Config {
        schema_version: 1,
        default_target: "planner".into(),
        hardware_id: "fixture".into(),
        admission_db: dir_root.as_path().join("admission.sqlite"),
        max_active_per_resource: 1,
        registry_max_age_ms: 60000,
        fallback_before_dispatch: false,
        targets: vec![Target {
            id: "planner".into(),
            provider: ProviderKind::Ollama,
            endpoint: model.url.clone(),
            endpoint_access: EndpointAccess::Loopback,
            model: "gemma4:fixture".into(),
            expected_artifact_digest: Some("a".repeat(64)),
            context_limit: 262144,
            output_limit: 4096,
            capabilities: Capabilities {
                streaming: true,
                reasoning_control: true,
                ..Default::default()
            },
            capability_source: "fixture".into(),
            priority: 0,
            resource_id: "fixture-gpu".into(),
            extensions: None,
            api_pricing: None,
        }],
    };
    let mut workflows = Workflows::open(&state).unwrap();
    workflows
        .create_profile(
            "fixture".into(),
            config,
            vec![
                ("worker-a".into(), "Fix only a.txt".into()),
                ("worker-b".into(), "Fix only b.txt".into()),
            ],
            max_steps,
            5000,
            "fixture".into(),
        )
        .unwrap();
    let w = workflows
        .create(
            "fixture",
            "Fix a and b; choose the appropriate workers".into(),
            "Both files pass the protected verifier".into(),
        )
        .unwrap();
    let workflow = workflows
        .approve(
            &w.id,
            w.revision,
            &w.assignment.digest().unwrap(),
            "fixture",
        )
        .unwrap();
    Fixture {
        _dir: dir,
        repo,
        state,
        workflow,
    }
}
fn run(f: &Fixture) -> Workflow {
    Workflows::open(&f.state)
        .unwrap()
        .run(&f.workflow.id, f.workflow.revision)
        .unwrap()
}

fn agentlet_fixture(model: &Model, steps: usize) -> Fixture {
    use clatterforge::agentlet::{Capability, Definition};
    let mut f = fixture(model, steps);
    let definitions = [Definition { schema_version:1, id:"implementer".into(), version:1,
        purpose:"Implement only the assigned component".into(),
        instructions:"Respect the approved file scope and return a structured summary of the work and unresolved findings".into(),
        capability:Capability::EditApprovedFiles,completion_gate:false },
        Definition {schema_version:1,id:"reviewer".into(),version:1,purpose:"Review the final candidate".into(),
            instructions:"Inspect the candidate and independently reported verification; make no candidate edits. Report unresolved errors honestly".into(),
            capability:Capability::Review,completion_gate:true}];
    for definition in definitions {
        let path = f.state.join(format!("{}.json", definition.id));
        fs::write(&path, serde_json::to_vec(&definition).unwrap()).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_clatterforge"))
            .arg("--state")
            .arg(&f.state)
            .args(["agentlet-import", "--definition"])
            .arg(path)
            .args(["--by", "fixture operator"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    for (id, source, name) in [
        ("implementer", "worker-a", "implement-a"),
        ("implementer", "worker-b", "implement-b"),
        ("reviewer", "worker-a", "review-final"),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_clatterforge"))
            .arg("--state")
            .arg(&f.state)
            .args([
                "agentlet-bind",
                id,
                "--version",
                "1",
                "--preset",
                source,
                "--name",
                name,
                "--by",
                "fixture operator",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let mut workflows = Workflows::open(&f.state).unwrap();
    workflows
        .create_profile(
            "agentlets".into(),
            f.workflow.assignment.profile.inference.clone(),
            vec![
                ("implement-a".into(), "Fix a.txt".into()),
                ("implement-b".into(), "Fix b.txt".into()),
                (
                    "review-final".into(),
                    "Review the complete candidate without changing files".into(),
                ),
            ],
            steps,
            5000,
            "fixture operator".into(),
        )
        .unwrap();
    let w = workflows
        .create(
            "agentlets",
            "Fix both files and review the result".into(),
            "The protected verifier passes and final review has no errors".into(),
        )
        .unwrap();
    f.workflow = workflows
        .approve(
            &w.id,
            w.revision,
            &w.assignment.digest().unwrap(),
            "fixture operator",
        )
        .unwrap();
    f
}

#[tokio::test]
async fn agentlets_end_to_end_mcp_routing_review_integration_restore_and_replay() {
    use clatterforge::{
        authorization::{ExecutionMode, Grants, Operation},
        repository::Repository,
    };
    use clatterforge_protocol::ConductorBackend;
    let model = Model::new(vec![
        dispatch("implement-a", "WORK_A"),
        dispatch("implement-b", "WORK_B"),
        dispatch("review-final", "REVIEW_OK"),
        json!({"action":"finish","summary":"All roles completed and the protected verifier passed"}),
    ]);
    let f = agentlet_fixture(&model, 4);
    Grants::open(&f.state)
        .unwrap()
        .create(
            "agentlet-bot",
            &Repository::inspect(&f.repo).unwrap().identity,
            vec!["workflow-v1".into()],
            vec![Operation::Submit, Operation::Resume],
            ExecutionMode::Delegated,
            "fixture operator",
            1,
            clatterforge::now() + 120000,
        )
        .unwrap();
    let backend = clatterforge::mcp::LocalBackend::open(f.state.clone()).unwrap();
    let args = json!({"backend":"workflow-v1","workflow_profile":"agentlets","objective":"Fix both fixture files and obtain final specialist review","acceptance":"Protected verifier passes; source stays unchanged until human integration","principal":"agentlet-bot","workflow_id":"agentlet-e2e","idempotency_key":"one"});
    let submitted = backend
        .conductor_submit_task(serde_json::from_value(args.clone()).unwrap())
        .await
        .unwrap();
    let id = submitted.task["id"].as_str().unwrap();
    let until = Instant::now() + Duration::from_secs(180);
    loop {
        let w = Workflows::open(&f.state).unwrap().get(id).unwrap();
        if w.status == AgentStatus::AwaitingReview {
            break;
        }
        assert!(
            matches!(w.status, AgentStatus::Approved | AgentStatus::Running),
            "{:?} {:?}",
            w.status,
            w.error
        );
        assert!(Instant::now() < until, "agentlet fixture exceeded deadline");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let mut service = TaskService::open(&f.state).unwrap();
    let d = service.detail(id, None).unwrap();
    let view = d.workflow.as_ref().unwrap();
    assert_eq!(view["agentlets"].as_array().unwrap().len(), 3);
    assert!(
        view["pending_agentlet_gates"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let w = Workflows::open(&f.state).unwrap().get(id).unwrap();
    assert_eq!(w.steps.len(), 3);
    let agents = Agents::open(&f.state).unwrap();
    let first = agents.task(&w.steps[0].task_id).unwrap();
    let reviewer = agents.task(&w.steps[2].task_id).unwrap();
    assert_eq!(first.assignment.workspace, reviewer.assignment.workspace);
    assert!(reviewer.assignment.spec.allowed_files.is_empty());
    assert_eq!(
        w.steps[1].checkpoint.as_ref().unwrap().tree,
        w.steps[2].checkpoint.as_ref().unwrap().tree
    );
    for step in &w.steps {
        let child = agents.task(&step.task_id).unwrap();
        assert!(child.agentlet_result.is_some());
        assert!(
            agents
                .routing(&child.id)
                .unwrap()
                .iter()
                .any(|e| e.handler == "agentlet::report_from_receipt")
        );
    }
    assert_eq!(fs::read_to_string(f.repo.join("a.txt")).unwrap(), "wrong-a");
    assert!(service.prepare_integration(id, "fixture reviewer").is_err());
    let digest = d.evidence_digest.as_ref().unwrap();
    service
        .review(id, d.revision, digest, "fixture reviewer", true)
        .unwrap();
    let original_index = fs::read(f.repo.join(".git/index")).unwrap();
    let original_head = git(&f.repo, &["rev-parse", "HEAD"]).unwrap();
    let plan = service.prepare_integration(id, "fixture operator").unwrap();
    let plan = plan.integration.unwrap();
    let applied = service
        .integrate(
            id,
            plan.revision,
            &plan.plan_digest,
            "fixture operator",
            false,
        )
        .unwrap();
    assert_eq!(fs::read_to_string(f.repo.join("a.txt")).unwrap(), "fixed-a");
    assert_eq!(fs::read_to_string(f.repo.join("b.txt")).unwrap(), "fixed-b");
    assert_eq!(fs::read(f.repo.join(".git/index")).unwrap(), original_index);
    assert_eq!(git(&f.repo, &["rev-parse", "HEAD"]).unwrap(), original_head);
    let applied = applied.integration.unwrap();
    service
        .integrate(
            id,
            applied.revision,
            &applied.plan_digest,
            "fixture operator",
            true,
        )
        .unwrap();
    assert_eq!(fs::read_to_string(f.repo.join("a.txt")).unwrap(), "wrong-a");
    assert_eq!(fs::read_to_string(f.repo.join("b.txt")).unwrap(), "wrong-b");
    let replay = backend
        .conductor_submit_task(serde_json::from_value(args).unwrap())
        .await
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.task["id"], id);
    assert_eq!(model.requests.lock().unwrap().len(), 4);
    let requests = model.requests.lock().unwrap();
    let first_packet: Value =
        serde_json::from_str(requests[0]["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(
        first_packet["approved_workers"][2]["agentlet"]["capability"],
        "review"
    );
    assert_eq!(
        first_packet["pending_agentlet_gates"],
        json!(["review-final"])
    );
}

#[test]
fn agentlets_required_review_cannot_be_skipped() {
    let model = Model::new(vec![
        dispatch("implement-a", "WORK_A"),
        dispatch("implement-b", "WORK_B"),
        json!({"action":"finish","summary":"Skip reviewer"}),
    ]);
    let f = agentlet_fixture(&model, 3);
    let done = run(&f);
    assert_eq!(done.status, AgentStatus::NeedsWork);
    assert!(done.error.unwrap().contains("required agentlet review"));
    assert_eq!(done.steps.len(), 2);
}

#[test]
fn agentlets_cli_execution_does_not_inherit_ambient_credential_variables() {
    let model = Model::new(vec![
        dispatch("implement-a", "WORK_A"),
        dispatch("implement-b", "WORK_B"),
        dispatch("review-final", "REVIEW_OK"),
        json!({"action":"finish","summary":"Done"}),
    ]);
    let f = agentlet_fixture(&model, 4);
    let output = Command::new(env!("CARGO_BIN_EXE_clatterforge"))
        .arg("--state")
        .arg(&f.state)
        .args([
            "board-run",
            &f.workflow.id,
            "--revision",
            &f.workflow.revision.to_string(),
        ])
        .env("CONDUCTOR_AGENTLET_CREDENTIAL_SENTINEL", "synthetic")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let done = Workflows::open(&f.state)
        .unwrap()
        .get(&f.workflow.id)
        .unwrap();
    assert_eq!(done.status, AgentStatus::AwaitingReview, "{:?}", done.error);
}

#[test]
fn agentlets_changed_cursor_entry_point_stops_before_planning() {
    let model = Model::new(vec![dispatch("pinned-editor", "WORK_A")]);
    let f = fixture(&model, 2);
    let path = f._dir.path().join("pinned-index.js");
    fs::write(&path, "//version one").unwrap();
    let policy = CursorAgentPolicy::inspect(worker(), &path, "fixture".into()).unwrap();
    let old = AgentPresets::open(&f.state)
        .unwrap()
        .get("worker-a")
        .unwrap();
    let mut presets = AgentPresets::open(&f.state).unwrap();
    presets
        .create(
            "pinned-base",
            f.repo.clone(),
            AgentBackend::CursorAgent(policy),
            old.allowed_files,
            old.verification_files,
            old.verify_executable,
            old.verify_args,
            5000,
            5000,
            None,
            "fixture",
        )
        .unwrap();
    presets
        .bind_agentlet(
            "pinned-base",
            "pinned-editor",
            clatterforge::agentlet::Definition {
                schema_version: 1,
                id: "pinned".into(),
                version: 1,
                purpose: "Scoped editor".into(),
                instructions: "Edit only the approved file".into(),
                capability: clatterforge::agentlet::Capability::EditApprovedFiles,
                completion_gate: false,
            },
            "fixture",
        )
        .unwrap();
    let mut workflows = Workflows::open(&f.state).unwrap();
    workflows
        .create_profile(
            "pinned".into(),
            f.workflow.assignment.profile.inference.clone(),
            vec![("pinned-editor".into(), "Fix a.txt".into())],
            2,
            5000,
            "fixture".into(),
        )
        .unwrap();
    let w = workflows
        .create(
            "pinned",
            "Fix a.txt".into(),
            "Protected verifier passes".into(),
        )
        .unwrap();
    let w = workflows
        .approve(
            &w.id,
            w.revision,
            &w.assignment.digest().unwrap(),
            "fixture",
        )
        .unwrap();
    fs::write(path, "//version two").unwrap();
    assert!(workflows.run(&w.id, w.revision).is_err());
    assert!(model.requests.lock().unwrap().is_empty());
}

#[test]
fn agentlets_candidate_edits_invalidate_an_earlier_successful_review() {
    let model = Model::new(vec![
        dispatch("implement-a", "WORK_A"),
        dispatch("implement-b", "WORK_B"),
        dispatch("review-final", "REVIEW_OK"),
        dispatch("implement-a", "WORK_A_CHANGED"),
        json!({"action":"finish","summary":"Try to reuse stale review"}),
    ]);
    let f = agentlet_fixture(&model, 5);
    let done = run(&f);
    assert_eq!(done.status, AgentStatus::NeedsWork);
    assert!(done.error.unwrap().contains("required agentlet review"));
    assert_ne!(
        done.steps[2].checkpoint.as_ref().unwrap().tree,
        done.steps[3].checkpoint.as_ref().unwrap().tree
    );
}

#[test]
fn agentlets_report_tampering_blocks_human_review_even_when_verification_passed() {
    let model = Model::new(vec![
        dispatch("implement-a", "WORK_A"),
        dispatch("implement-b", "WORK_B"),
        dispatch("review-final", "REVIEW_OK"),
        json!({"action":"finish","summary":"Done"}),
    ]);
    let f = agentlet_fixture(&model, 4);
    let done = run(&f);
    assert_eq!(done.status, AgentStatus::AwaitingReview);
    let mut service = TaskService::open(&f.state).unwrap();
    let d = service.detail(&done.id, None).unwrap();
    let id = &done.steps[2].task_id;
    let db = rusqlite::Connection::open(f.state.join("state.sqlite")).unwrap();
    let record: String = db
        .query_row("SELECT record FROM agent_tasks WHERE id=?1", [id], |r| {
            r.get(0)
        })
        .unwrap();
    let mut record: Value = serde_json::from_str(&record).unwrap();
    record["agentlet_result"]["summary"] = json!("Altered after execution");
    db.execute(
        "UPDATE agent_tasks SET record=?1 WHERE id=?2",
        rusqlite::params![record.to_string(), id],
    )
    .unwrap();
    assert!(
        service
            .review(
                &done.id,
                d.revision,
                d.evidence_digest.as_ref().unwrap(),
                "fixture reviewer",
                true
            )
            .is_err()
    );
}

#[test]
fn agentlets_read_only_reviewer_cannot_change_an_inherited_candidate() {
    let model = Model::new(vec![
        dispatch("implement-a", "WORK_A"),
        dispatch("implement-b", "WORK_B"),
        dispatch("review-final", "REVIEW_TAMPER"),
    ]);
    let f = agentlet_fixture(&model, 3);
    let done = run(&f);
    assert_eq!(done.status, AgentStatus::NeedsWork);
    let child = Agents::open(&f.state)
        .unwrap()
        .task(&done.steps[2].task_id)
        .unwrap();
    assert!(child.error.unwrap().contains("outside approved files"));
    assert!(child.verification.is_none());
}

#[test]
fn agentlets_malformed_result_stops_before_verification_or_another_worker() {
    let model = Model::new(vec![
        dispatch("implement-a", "WORK_A_BAD_REPORT"),
        dispatch("implement-b", "WORK_B"),
    ]);
    let f = agentlet_fixture(&model, 3);
    let done = run(&f);
    assert_eq!(done.status, AgentStatus::NeedsWork);
    assert_eq!(done.steps.len(), 1);
    assert_eq!(model.requests.lock().unwrap().len(), 1);
    let child = Agents::open(&f.state)
        .unwrap()
        .task(&done.steps[0].task_id)
        .unwrap();
    assert!(child.error.unwrap().contains("agentlet result rejected"));
    assert!(child.verification.is_none());
    assert!(
        Workflows::open(&f.state)
            .unwrap()
            .run(&done.id, done.revision)
            .is_err()
    );
}

#[test]
fn agentlets_blocking_findings_cannot_be_overridden_by_a_passing_verifier() {
    let model = Model::new(vec![
        dispatch("implement-a", "WORK_A"),
        dispatch("implement-b", "WORK_B"),
        dispatch("review-final", "REVIEW_BLOCKED"),
        json!({"action":"finish","summary":"Ignore blocking review"}),
    ]);
    let f = agentlet_fixture(&model, 4);
    let done = run(&f);
    assert_eq!(done.status, AgentStatus::NeedsWork);
    let child = Agents::open(&f.state)
        .unwrap()
        .task(&done.steps[2].task_id)
        .unwrap();
    assert_eq!(child.verification.unwrap().outcome, "succeeded");
    assert_eq!(
        child.agentlet_result.unwrap().outcome,
        clatterforge::agentlet::Outcome::Blocked
    );
}

#[test]
fn accepted_workflow_integrates_once_with_conflict_checks_and_explicit_safe_restore() {
    let model = Model::new(vec![
        dispatch("worker-a", "WORK_A"),
        dispatch("worker-b", "WORK_B"),
        json!({"action":"finish","summary":"Verified candidate"}),
    ]);
    let f = fixture(&model, 3);
    let mut service = TaskService::open(&f.state).unwrap();
    assert!(
        service
            .prepare_integration(&f.workflow.id, "human")
            .is_err()
    );
    let done = run(&f);
    let detail = service.detail(&done.id, None).unwrap();
    assert!(service.prepare_integration(&done.id, "human").is_err());
    service
        .review(
            &done.id,
            detail.revision,
            detail.evidence_digest.as_ref().unwrap(),
            "human",
            true,
        )
        .unwrap();
    let index = f.repo.join(".git/index");
    let original_index = fs::read(&index).unwrap();
    fs::write(f.repo.join("personal.txt"), "preserve unrelated work").unwrap();
    assert!(service.prepare_integration(&done.id, "human").is_err());
    assert_eq!(
        fs::read_to_string(f.repo.join("personal.txt")).unwrap(),
        "preserve unrelated work"
    );
    fs::remove_file(f.repo.join("personal.txt")).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let _server = HttpFixture(
        Command::new(env!("CARGO_BIN_EXE_clatterforge"))
            .arg("--state")
            .arg(&f.state)
            .args(["serve", "--port", &port.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let until = Instant::now() + Duration::from_secs(15);
    while !f.state.join("serve-token").exists()
        || std::net::TcpStream::connect(("127.0.0.1", port)).is_err()
    {
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(25));
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{port}");
    let token = fs::read_to_string(f.state.join("serve-token")).unwrap();
    let endpoint = format!("{url}/api/tasks/{}/integration", done.id);
    let prepared = rt.block_on(async {
        assert_eq!(
            client
                .post(format!("{endpoint}/prepare"))
                .json(&json!({"by":"spoof"}))
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
        assert_eq!(
            client
                .post(&endpoint)
                .json(&json!({"by":"spoof","revision":1,"digest":"invalid"}))
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
        let response = client
            .post(format!("{endpoint}/prepare"))
            .bearer_auth(token.trim())
            .json(&json!({"by":"human"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body: Value = response.json().await.unwrap();
        serde_json::from_value::<clatterforge::integration::Record>(body["integration"].clone())
            .unwrap()
    });
    assert_eq!(prepared.status, "prepared");
    assert_eq!(prepared.files.len(), 2);
    assert!(
        service
            .integrate(
                &done.id,
                prepared.revision + 1,
                &prepared.plan_digest,
                "human",
                false
            )
            .is_err()
    );
    assert!(
        service
            .integrate(&done.id, prepared.revision, "wrong", "human", false)
            .is_err()
    );
    git(&f.repo, &["switch", "-c", "conflicting-branch"]).unwrap();
    assert!(
        service
            .integrate(
                &done.id,
                prepared.revision,
                &prepared.plan_digest,
                "human",
                false
            )
            .is_err()
    );
    git(&f.repo, &["switch", "main"]).unwrap();
    let workspace = Agents::open(&f.state)
        .unwrap()
        .task(&done.steps[1].task_id)
        .unwrap()
        .assignment
        .workspace;
    fs::write(workspace.join("b.txt"), "tampered after review").unwrap();
    assert!(
        service
            .integrate(
                &done.id,
                prepared.revision,
                &prepared.plan_digest,
                "human",
                false
            )
            .is_err()
    );
    fs::write(workspace.join("b.txt"), "fixed-b").unwrap();
    let mut stop = clatterforge::stop::GlobalStop::open(&f.state).unwrap();
    stop.engage("human", "test source stop").unwrap();
    assert!(
        service
            .integrate(
                &done.id,
                prepared.revision,
                &prepared.plan_digest,
                "human",
                false
            )
            .is_err()
    );
    stop.disengage("human", "continue test").unwrap();
    fs::write(index.with_extension("lock"), "foreign lock").unwrap();
    let blocked = service
        .integrate(
            &done.id,
            prepared.revision,
            &prepared.plan_digest,
            "human",
            false,
        )
        .unwrap()
        .integration
        .unwrap();
    assert_eq!(blocked.status, "prepared");
    assert!(blocked.error.unwrap().contains("locked"));
    assert_eq!(
        fs::read_to_string(index.with_extension("lock")).unwrap(),
        "foreign lock"
    );
    assert_eq!(fs::read_to_string(f.repo.join("a.txt")).unwrap(), "wrong-a");
    fs::remove_file(index.with_extension("lock")).unwrap();
    let record = rt.block_on(async {
        let code: Value = client
            .post(format!("{url}/api/pair/start"))
            .bearer_auth(token.trim())
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let paired = client
            .post(format!("{url}/api/pair"))
            .json(&json!({"code":code["code"],"principal":"fixture session"}))
            .send()
            .await
            .unwrap();
        assert_eq!(paired.status(), 200);
        let cookies = paired
            .headers()
            .get_all("set-cookie")
            .iter()
            .map(|v| v.to_str().unwrap().split(';').next().unwrap().to_owned())
            .collect::<Vec<_>>();
        let cookie = cookies.join("; ");
        let csrf = cookies
            .iter()
            .find_map(|c| c.strip_prefix("clatterforge_csrf="))
            .unwrap();
        let body =
            json!({"revision":blocked.revision,"digest":prepared.plan_digest,"by":"spoofed actor"});
        assert_eq!(
            client
                .post(&endpoint)
                .header("cookie", &cookie)
                .json(&body)
                .send()
                .await
                .unwrap()
                .status(),
            403
        );
        assert_eq!(
            client
                .post(format!("{endpoint}/prepare"))
                .header("cookie", &cookie)
                .json(&json!({"by":"spoof"}))
                .send()
                .await
                .unwrap()
                .status(),
            403
        );
        let response = client
            .post(&endpoint)
            .header("cookie", &cookie)
            .header("x-csrf-token", csrf)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body: Value = response.json().await.unwrap();
        let record = serde_json::from_value::<clatterforge::integration::Record>(
            body["integration"].clone(),
        )
        .unwrap();
        assert_eq!(record.events.last().unwrap().by, "fixture session");
        record
    });
    assert_eq!(record.status, "applied");
    assert_eq!(fs::read_to_string(f.repo.join("a.txt")).unwrap(), "fixed-a");
    assert_eq!(fs::read_to_string(f.repo.join("b.txt")).unwrap(), "fixed-b");
    assert_eq!(fs::read(&index).unwrap(), original_index);
    assert_eq!(
        clatterforge::repository::Repository::inspect(&f.repo)
            .unwrap()
            .base,
        f.workflow.assignment.profile.repository.base
    );
    let replay = service
        .integrate(
            &done.id,
            prepared.revision,
            &prepared.plan_digest,
            "human",
            false,
        )
        .unwrap()
        .integration
        .unwrap();
    assert_eq!(replay.revision, record.revision);
    assert_eq!(
        replay
            .events
            .iter()
            .filter(|e| e.stage == "source_file_applied")
            .count(),
        2
    );
    assert_eq!(model.requests.lock().unwrap().len(), 3);
    assert!(
        clatterforge::coordination::Coordination::open(&f.state)
            .unwrap()
            .reservations()
            .unwrap()
            .is_empty()
    );
    fs::write(f.repo.join("a.txt"), "new human edit").unwrap();
    let uncertain = service
        .integrate(
            &done.id,
            record.revision,
            &record.plan_digest,
            "human",
            true,
        )
        .unwrap()
        .integration
        .unwrap();
    assert_eq!(uncertain.status, "unknown");
    assert_eq!(
        fs::read_to_string(f.repo.join("a.txt")).unwrap(),
        "new human edit"
    );
    assert_eq!(fs::read_to_string(f.repo.join("b.txt")).unwrap(), "fixed-b");
    assert_eq!(
        clatterforge::coordination::Coordination::open(&f.state)
            .unwrap()
            .reservations()
            .unwrap()
            .len(),
        1
    );
    fs::write(f.repo.join("a.txt"), "fixed-a").unwrap();
    let restored = service
        .integrate(
            &done.id,
            uncertain.revision,
            &record.plan_digest,
            "human",
            true,
        )
        .unwrap()
        .integration
        .unwrap();
    assert_eq!(restored.status, "rolled_back");
    assert_eq!(fs::read_to_string(f.repo.join("a.txt")).unwrap(), "wrong-a");
    assert_eq!(fs::read_to_string(f.repo.join("b.txt")).unwrap(), "wrong-b");
    assert_eq!(fs::read(&index).unwrap(), original_index);
    assert_eq!(service.detail(&done.id, None).unwrap().status, "accepted");
    assert!(
        clatterforge::coordination::Coordination::open(&f.state)
            .unwrap()
            .reservations()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn local_model_routes_two_scoped_workers_and_hands_off_candidate_and_evidence_before_review() {
    let model = Model::new(vec![
        dispatch("worker-a", "WORK_A"),
        dispatch("worker-b", "WORK_B"),
        json!({"action":"finish","summary":"Both files fixed and independently verified"}),
    ]);
    let f = fixture(&model, 3);
    let done = run(&f);
    assert_eq!(done.status, AgentStatus::AwaitingReview);
    assert_eq!(done.steps.len(), 2);
    assert_eq!(done.decisions.len(), 3);
    let agents = Agents::open(&f.state).unwrap();
    let first = agents.task(&done.steps[0].task_id).unwrap();
    let last = agents.task(&done.steps[1].task_id).unwrap();
    assert_eq!(first.status, AgentStatus::NeedsWork);
    assert_eq!(last.status, AgentStatus::AwaitingReview);
    assert!(
        last.assignment
            .handoff
            .as_ref()
            .unwrap()
            .context
            .contains("failed")
    );
    assert_eq!(
        last.assignment.handoff.as_ref().unwrap().source_task,
        first.id
    );
    assert!(
        agents
            .routing(&last.id)
            .unwrap()
            .iter()
            .any(|e| e.stage == "context_handoff_applied")
    );
    assert_eq!(
        fs::read_to_string(last.assignment.workspace.join("a.txt")).unwrap(),
        "fixed-a"
    );
    assert_eq!(fs::read_to_string(f.repo.join("a.txt")).unwrap(), "wrong-a");
    let requests = model.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    let third: Value =
        serde_json::from_str(requests[2]["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(
        third["stage_results"][0]["result"]["verification_outcome"],
        "failed"
    );
    assert!(third.get("candidate_files").is_none());
    assert!(
        third["repository"]["sources"]["a.txt"]
            .get("text")
            .is_none()
    );
    assert!(third["candidate_checkpoint"]["tree"].is_string());
    assert_eq!(first.assignment.workspace, last.assignment.workspace);
    assert!(
        done.steps
            .iter()
            .all(|s| s.files.is_empty() && s.checkpoint.is_some())
    );
    assert!(last.assignment.handoff.as_ref().unwrap().files.is_empty());
    let first_tree = &done.steps[0].checkpoint.as_ref().unwrap().tree;
    assert_eq!(
        git(&f.repo, &["show", &format!("{first_tree}:b.txt")]).unwrap(),
        b"wrong-b"
    );
    assert_eq!(
        git(
            &last.assignment.workspace,
            &["diff", "--cached", "--name-only"]
        )
        .unwrap(),
        b""
    );
    drop(requests);
    let mut service = TaskService::open(&f.state).unwrap();
    let d = service.detail(&done.id, Some(64)).unwrap();
    let child = service.detail(&last.id, None).unwrap();
    assert!(
        service
            .review(
                &child.id,
                child.revision,
                child.evidence_digest.as_ref().unwrap(),
                "human",
                true
            )
            .is_err()
    );
    assert_eq!(d.backend, "workflow-v1");
    assert!(d.workflow.is_some());
    for child in [&first, &last] {
        assert!(
            d.routing.iter().any(|e| e.stage == "generation_launched"
                && e.note.as_ref().unwrap().contains(&child.id))
        );
    }
    assert!(
        service
            .review(&d.id, d.revision, "wrong", "human", true)
            .is_err()
    );
    let accepted = service
        .review(
            &d.id,
            d.revision,
            d.evidence_digest.as_ref().unwrap(),
            "human",
            true,
        )
        .unwrap();
    assert_eq!(accepted.status, "accepted");
    assert_eq!(accepted.workflow.as_ref().unwrap()["phase"], "accepted");
    assert!(accepted.agent.ends_with(" / accepted"));
    assert_eq!(accepted.evidence_digest, d.evidence_digest);
    assert!(
        accepted.workflow.as_ref().unwrap()["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["stage"] == "workflow_reviewed")
    );
    assert_eq!(
        agents.task(&last.id).unwrap().status,
        AgentStatus::AwaitingReview
    );
    assert!(service.run(&done.id, done.revision).is_err());
    assert_eq!(model.requests.lock().unwrap().len(), 3);
}

#[test]
fn large_sources_and_candidates_stay_in_git_and_inspection_pages_are_requested_explicitly() {
    let model = Model::new(vec![
        json!({"action":"inspect","query":"history","offset":0,"limit":64}),
        dispatch("worker-a", "WORK_A_LARGE"),
        json!({"action":"inspect","query":"diff","offset":0,"limit":128}),
        json!({"action":"inspect","query":"diff","offset":128,"limit":128}),
        dispatch("worker-b", "WORK_B"),
        json!({"action":"finish","summary":"Both stages verified"}),
    ]);
    let f = fixture_with_readme(&model, 3, &"source-body-marker".repeat(15000));
    let done = run(&f);
    assert_eq!(done.status, AgentStatus::AwaitingReview, "{:?}", done.error);
    assert_eq!(done.git_reads.len(), 3);
    assert!(done.git_reads[1].artifact.bytes > 65536);
    assert_eq!(done.git_reads[1].tree, done.git_reads[2].tree);
    assert_eq!(
        done.git_reads[1].artifact.hash,
        done.git_reads[2].artifact.hash
    );
    assert!(serde_json::to_vec(&done).unwrap().len() < 40000);
    let requests = model.requests.lock().unwrap();
    for req in requests.iter() {
        let packet: Value =
            serde_json::from_str(req["messages"][1]["content"].as_str().unwrap()).unwrap();
        assert!(packet.get("candidate_files").is_none());
        assert!(!req.to_string().contains("source-body-marker"));
        assert!(
            !req.to_string()
                .contains(&"candidate-body-marker".repeat(20))
        );
        if let Some(text) = packet["git_page"]["text"].as_str() {
            assert!(text.len() <= 128);
        }
    }
    let packet: Value =
        serde_json::from_str(requests[3]["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(packet["git_page"]["offset"], 0);
    assert_eq!(packet["git_page"]["next_offset"], 128);
    drop(requests);
    let mut service = TaskService::open(&f.state).unwrap();
    let detail = service.detail(&done.id, None).unwrap();
    fs::write(
        &done.git_reads[0].artifact.path,
        "tampered context evidence",
    )
    .unwrap();
    assert!(
        service
            .review(
                &done.id,
                detail.revision,
                detail.evidence_digest.as_ref().unwrap(),
                "human",
                true
            )
            .is_err()
    );
}

#[test]
fn invalid_or_exhausted_git_inspections_launch_no_worker() {
    for (decisions, expected_reads) in [
        (
            vec![json!({"action":"inspect","query":"shell","offset":0,"limit":64})],
            0,
        ),
        (
            vec![json!({"action":"inspect","query":"diff","offset":0,"limit":8193})],
            0,
        ),
        (
            vec![json!({"action":"inspect","query":"history","offset":0,"limit":64}); 5],
            4,
        ),
    ] {
        let model = Model::new(decisions);
        let f = fixture(&model, 3);
        let done = run(&f);
        assert_eq!(done.status, AgentStatus::NeedsWork);
        assert!(done.steps.is_empty());
        assert_eq!(done.git_reads.len(), expected_reads);
    }
}

#[test]
fn invented_worker_invalid_json_early_finish_and_budget_overrun_never_authorize_extra_work() {
    for decisions in [
        vec!["not json".into()],
        vec![dispatch("arbitrary-shell", "WORK_A").to_string()],
        vec![json!({"action":"finish","summary":"trust me"}).to_string()],
        vec![
            dispatch("worker-a", "WORK_A").to_string(),
            dispatch("worker-b", "WORK_B").to_string(),
        ],
    ] {
        let model = Model::texts(decisions);
        let f = fixture(&model, 1);
        let done = run(&f);
        assert_eq!(done.status, AgentStatus::NeedsWork, "{:?}", done.error);
        assert!(done.steps.len() <= 1);
        assert_eq!(fs::read_to_string(f.repo.join("a.txt")).unwrap(), "wrong-a");
    }
}

#[test]
fn next_worker_cannot_rewrite_an_inherited_file_outside_its_own_scope() {
    let model = Model::new(vec![
        dispatch("worker-a", "WORK_A"),
        dispatch("worker-b", "WORK_B EDIT_INHERITED"),
    ]);
    let f = fixture(&model, 3);
    let done = run(&f);
    assert_ne!(done.status, AgentStatus::AwaitingReview);
    assert_eq!(done.steps.len(), 2);
    let task = Agents::open(&f.state)
        .unwrap()
        .task(&done.steps[1].task_id)
        .unwrap();
    assert!(task.verification.is_none());
    assert!(task.error.unwrap().contains("outside approved"));
    assert_eq!(model.requests.lock().unwrap().len(), 2);
}

#[test]
fn inherited_candidate_line_endings_are_checked_as_raw_bytes_before_verification() {
    let model = Model::new(vec![
        dispatch("worker-a", "WORK_A_EOL"),
        dispatch("worker-b", "WORK_B EDIT_EOL"),
    ]);
    let f = fixture(&model, 3);
    let done = run(&f);
    assert_eq!(done.status, AgentStatus::NeedsWork);
    let task = Agents::open(&f.state)
        .unwrap()
        .task(&done.steps[1].task_id)
        .unwrap();
    assert!(task.verification.is_none());
    assert!(task.error.unwrap().contains("outside approved"));
    let checkpoint = done.steps[0].checkpoint.as_ref().unwrap();
    assert_eq!(
        git(
            &f.repo,
            &["show", &format!("{}:a.txt", checkpoint.reference)]
        )
        .unwrap(),
        b"fixed-a\r\n"
    );
}

#[test]
fn changed_candidate_or_artifact_refuses_review_without_losing_evidence() {
    for kind in 0..4 {
        let model = Model::new(vec![
            dispatch("worker-a", "WORK_A"),
            dispatch("worker-b", "WORK_B"),
            json!({"action":"finish","summary":"done"}),
        ]);
        let f = fixture(&model, 3);
        let done = run(&f);
        let task = Agents::open(&f.state)
            .unwrap()
            .task(&done.steps[1].task_id)
            .unwrap();
        let path = if kind == 0 {
            task.patch.unwrap().path
        } else if kind == 1 {
            task.assignment.workspace.join("a.txt")
        } else if kind == 2 {
            task.assignment.workspace.join("contract.txt")
        } else {
            task.assignment.workspace.join("outside.txt")
        };
        fs::write(path, "tampered").unwrap();
        assert!(
            Workflows::open(&f.state)
                .unwrap()
                .review(
                    &done.id,
                    done.revision,
                    &done.evidence_digest().unwrap(),
                    "human",
                    true
                )
                .is_err()
        );
        assert_eq!(
            Workflows::open(&f.state)
                .unwrap()
                .get(&done.id)
                .unwrap()
                .status,
            AgentStatus::AwaitingReview
        );
    }
}

#[test]
fn cancelling_during_local_planning_stops_before_any_cli_dispatch() {
    let model = Model::new(vec![dispatch("worker-a", "WORK_A")]);
    model.hold.store(true, Ordering::SeqCst);
    let f = fixture(&model, 3);
    let mut prepared = Workflows::open(&f.state).unwrap();
    let intake = prepared
        .create("fixture", "Intake only".into(), "No execution".into())
        .unwrap();
    let cancelled = prepared.cancel(&intake.id, intake.revision).unwrap();
    assert_eq!(cancelled.phase, "cancelled");
    assert!(cancelled.decisions.is_empty() && cancelled.steps.is_empty());
    assert_eq!(
        prepared.detail(&intake.id, None).unwrap().workflow.unwrap()["phase"],
        "cancelled"
    );
    let state = f.state.clone();
    let id = f.workflow.id.clone();
    let revision = f.workflow.revision;
    let runner =
        thread::spawn(move || Workflows::open(&state).unwrap().run(&id, revision).unwrap());
    let until = Instant::now() + Duration::from_secs(8);
    while model.requests.lock().unwrap().is_empty() {
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(10));
    }
    let mut workflows = Workflows::open(&f.state).unwrap();
    let current = workflows.get(&f.workflow.id).unwrap();
    let observed = workflows.detail(&current.id, None).unwrap();
    assert_eq!(observed.workflow.as_ref().unwrap()["phase"], "planning");
    assert!(observed.workflow.as_ref().unwrap()["planner_progress"].is_object());
    assert!(observed.progress.is_none());
    workflows.cancel(&current.id, current.revision).unwrap();
    let done = runner.join().unwrap();
    assert_eq!(done.status, AgentStatus::Cancelled);
    assert_eq!(
        workflows.detail(&done.id, None).unwrap().workflow.unwrap()["phase"],
        "cancelled"
    );
    assert!(done.steps.is_empty());
    assert!(Agents::open(&f.state).unwrap().tasks().unwrap().is_empty());
}

#[test]
fn owner_crash_marks_workflow_unknown_and_never_replays_the_model_or_worker() {
    let model = Model::new(vec![dispatch("worker-a", "WORK_A")]);
    model.hold.store(true, Ordering::SeqCst);
    let f = fixture(&model, 3);
    let mut child = Command::new(env!("CARGO_BIN_EXE_clatterforge"))
        .arg("--state")
        .arg(&f.state)
        .args([
            "board-run",
            &f.workflow.id,
            "--revision",
            &f.workflow.revision.to_string(),
            "--json",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let until = Instant::now() + Duration::from_secs(8);
    while model.requests.lock().unwrap().is_empty() {
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(10));
    }
    child.kill().unwrap();
    child.wait().unwrap();
    let mut workflows = Workflows::open(&f.state).unwrap();
    let current = workflows.get(&f.workflow.id).unwrap();
    let unknown = workflows.reconcile(&current.id, current.revision).unwrap();
    assert_eq!(unknown.status, AgentStatus::Unknown);
    assert!(workflows.run(&unknown.id, unknown.revision).is_err());
    assert_eq!(model.requests.lock().unwrap().len(), 1);
}

#[test]
fn dependency_and_global_stop_block_planning_before_model_consumption() {
    let model = Model::new(vec![dispatch("worker-a", "WORK_A")]);
    let f = fixture(&model, 3);
    let mut workflows = Workflows::open(&f.state).unwrap();
    let prerequisite = workflows
        .create("fixture", "other".into(), "other".into())
        .unwrap();
    let mut service = TaskService::open(&f.state).unwrap();
    service
        .add_dependency(&f.workflow.id, &prerequisite.id)
        .unwrap();
    assert!(service.run(&f.workflow.id, f.workflow.revision).is_err());
    assert!(model.requests.lock().unwrap().is_empty());
    clatterforge::stop::GlobalStop::open(&f.state)
        .unwrap()
        .engage("fixture", "test")
        .unwrap();
    assert!(run_result(&f).is_err());
    assert!(model.requests.lock().unwrap().is_empty());
}
fn run_result(f: &Fixture) -> anyhow::Result<Workflow> {
    Workflows::open(&f.state)?.run(&f.workflow.id, f.workflow.revision)
}

#[test]
fn concurrent_runs_claim_the_workflow_once() {
    let model = Model::new(vec![
        dispatch("worker-a", "WORK_A"),
        dispatch("worker-b", "WORK_B"),
        json!({"action":"finish","summary":"done"}),
    ]);
    let f = fixture(&model, 3);
    let gate = Arc::new(std::sync::Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let state = f.state.clone();
            let id = f.workflow.id.clone();
            let revision = f.workflow.revision;
            let gate = gate.clone();
            thread::spawn(move || {
                gate.wait();
                Workflows::open(&state).unwrap().run(&id, revision)
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(model.requests.lock().unwrap().len(), 3);
    assert_eq!(Agents::open(&f.state).unwrap().tasks().unwrap().len(), 2);
}

#[test]
fn cancellation_during_worker_execution_stops_the_owned_process_and_never_advances() {
    let model = Model::new(vec![dispatch("worker-a", "HANG_WORKER")]);
    let f = fixture(&model, 3);
    let state = f.state.clone();
    let id = f.workflow.id.clone();
    let revision = f.workflow.revision;
    let runner =
        thread::spawn(move || Workflows::open(&state).unwrap().run(&id, revision).unwrap());
    // Admission prepares several Git snapshots before the owned worker starts.
    let until = Instant::now() + Duration::from_secs(30);
    loop {
        let w = Workflows::open(&f.state)
            .unwrap()
            .get(&f.workflow.id)
            .unwrap();
        if let Some(step) = w.steps.last()
            && Agents::open(&f.state)
                .unwrap()
                .routing(&step.task_id)
                .unwrap()
                .iter()
                .any(|e| e.stage == "generation_launched")
        {
            break;
        }
        assert!(Instant::now() < until, "worker not launched: {w:?}");
        thread::sleep(Duration::from_millis(10));
    }
    let mut workflows = Workflows::open(&f.state).unwrap();
    let w = workflows.get(&f.workflow.id).unwrap();
    workflows.cancel(&w.id, w.revision).unwrap();
    let done = runner.join().unwrap();
    assert_eq!(done.status, AgentStatus::Cancelled);
    let child = Agents::open(&f.state)
        .unwrap()
        .task(&done.steps[0].task_id)
        .unwrap();
    assert_eq!(child.status, AgentStatus::Cancelled);
    assert!(child.execution.unwrap().tree_terminated);
    assert!(child.verification.is_none());
    assert_eq!(model.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn mcp_objective_entry_runs_one_slot_workflow_asynchronously_and_replays_without_extra_work()
{
    use clatterforge::{
        authorization::{ExecutionMode, Grants, Operation},
        repository::Repository,
    };
    use clatterforge_protocol::ConductorBackend;
    let model = Model::new(vec![
        dispatch("worker-a", "WORK_A"),
        dispatch("worker-b", "WORK_B"),
        json!({"action":"finish","summary":"done"}),
    ]);
    let f = fixture(&model, 3);
    let mut grants = Grants::open(&f.state).unwrap();
    grants
        .create(
            "workflow-bot",
            &Repository::inspect(&f.repo).unwrap().identity,
            vec!["workflow-v1".into()],
            vec![Operation::Submit, Operation::Resume],
            ExecutionMode::Delegated,
            "human",
            1,
            clatterforge::now() + 60000,
        )
        .unwrap();
    let backend = clatterforge::mcp::LocalBackend::open(f.state.clone()).unwrap();
    let args = json!({"backend":"workflow-v1","workflow_profile":"fixture","objective":"Fix both files","acceptance":"The protected verifier passes","principal":"workflow-bot","workflow_id":"workflow-test","idempotency_key":"one"});
    let submitted = backend
        .conductor_submit_task(serde_json::from_value(args.clone()).unwrap())
        .await
        .unwrap();
    assert!(
        submitted
            .authorization
            .iter()
            .any(|a| a["dispatched_async"] == true)
    );
    let id = submitted.task["id"].as_str().unwrap();
    let until = Instant::now() + Duration::from_secs(120);
    loop {
        let d = TaskService::open(&f.state)
            .unwrap()
            .detail(id, None)
            .unwrap();
        if d.status == "awaiting_review" {
            break;
        }
        assert!(Instant::now() < until, "{:?}", d);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let replay = backend
        .conductor_submit_task(serde_json::from_value(args).unwrap())
        .await
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.task["id"], id);
    assert_eq!(replay.task["status"], "awaiting_review");
    assert!(replay.authorization.is_empty());
    assert_eq!(model.requests.lock().unwrap().len(), 3);
    assert_eq!(Agents::open(&f.state).unwrap().tasks().unwrap().len(), 2);
    assert_eq!(grants.list().unwrap()[0].dispatched, 1);
    let requirements = backend
        .conductor_get_approval_requirements(
            serde_json::from_value(json!({"task_id":id,"principal":"workflow-bot"})).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        requirements.decision_stage.as_deref(),
        Some("result_review")
    );
    assert!(requirements.covered_by_grant.is_none());
    let alias=backend.conductor_submit_task(serde_json::from_value(json!({"backend":"workflow-v1","agent_preset":"fixture","objective":"Alias compatibility check","acceptance":"Wait for scope review","principal":"workflow-bot","workflow_id":"workflow-test","idempotency_key":"alias","requested_execution_mode":"interactive"})).unwrap()).await.unwrap();
    assert_eq!(alias.task["status"], "awaiting_approval");
    assert_eq!(model.requests.lock().unwrap().len(), 3);
}

#[test]
fn changed_verifier_pin_refuses_the_workflow_before_planning_or_dispatch() {
    let model = Model::new(vec![dispatch("worker-a", "WORK_A")]);
    let f = fixture(&model, 3);
    fs::write(
        &f.workflow.assignment.profile.verifier.executable.path,
        "replacement binary",
    )
    .unwrap();
    assert!(run_result(&f).is_err());
    assert!(model.requests.lock().unwrap().is_empty());
    assert!(Agents::open(&f.state).unwrap().tasks().unwrap().is_empty());
}

#[test]
fn new_objective_captures_the_current_commit_but_an_existing_approval_cannot_drift() {
    let model = Model::new(vec![dispatch("worker-a", "WORK_A")]);
    let f = fixture(&model, 3);
    fs::write(f.repo.join("a.txt"), "new committed baseline").unwrap();
    git(&f.repo, &["add", "a.txt"]).unwrap();
    git(
        &f.repo,
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
            "new source baseline",
        ],
    )
    .unwrap();
    assert!(run_result(&f).is_err());
    let next = Workflows::open(&f.state)
        .unwrap()
        .create("fixture", "Next objective".into(), "Current source".into())
        .unwrap();
    assert_ne!(
        next.assignment.profile.repository.base,
        f.workflow.assignment.profile.repository.base
    );
    assert_eq!(
        next.assignment.context["sources"]["a.txt"]["git_object"]
            .as_str()
            .unwrap(),
        clatterforge::repository::git_text(&f.repo, &["rev-parse", "HEAD:a.txt"]).unwrap()
    );
    assert!(model.requests.lock().unwrap().is_empty());
}

#[test]
fn one_schema_correction_retains_rejected_decision_and_does_not_dispatch_it() {
    let malformed = r#"{"action":"dispatch","worker":"worker-a","worker":"worker-a","objective":"WORK_A","acceptance":"pass","rationale":"owns file"}"#;
    let model = Model::texts(vec![
        malformed.into(),
        dispatch("worker-a", "WORK_A").to_string(),
        dispatch("worker-b", "WORK_B").to_string(),
        json!({"action":"finish","summary":"done"}).to_string(),
    ]);
    let f = fixture(&model, 3);
    let done = run(&f);
    assert_eq!(done.status, AgentStatus::AwaitingReview);
    assert_eq!(done.steps.len(), 2);
    assert_eq!(done.decisions.len(), 4);
    assert!(done.decisions[0].decision.is_none());
    assert!(
        done.decisions[0]
            .error
            .as_ref()
            .unwrap()
            .contains("duplicate field")
    );
    let requests = model.requests.lock().unwrap();
    let second: Value =
        serde_json::from_str(requests[1]["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert!(
        second["decision_feedback"]
            .as_str()
            .unwrap()
            .contains("duplicate field")
    );
    assert!(second["stage_results"].as_array().unwrap().is_empty());
}

#[test]
fn each_decision_has_one_schema_correction_with_all_attempts_counted() {
    let malformed = r#"{"action":"dispatch","worker":"worker-b","worker":"worker-b","objective":"WORK_B","acceptance":"pass","rationale":"owns file"}"#;
    let model = Model::texts(vec![
        dispatch("worker-a", "WORK_A").to_string(),
        malformed.into(),
        dispatch("worker-b", "WORK_B").to_string(),
        json!({"action":"finish","summary":"done","evidence":"not a permitted key"}).to_string(),
        json!({"action":"finish","summary":"done"}).to_string(),
    ]);
    let f = fixture(&model, 3);
    let done = run(&f);
    assert_eq!(done.status, AgentStatus::AwaitingReview);
    assert_eq!(done.steps.len(), 2);
    assert_eq!(done.decisions.len(), 5);
    assert_eq!(
        done.decisions.iter().filter(|r| r.error.is_some()).count(),
        2
    );
    assert!(done.decisions.len() <= done.assignment.profile.decision_limit());
}

fn setup_tiered_profile(f: &Fixture, include_local: bool, max_steps: usize) {
    use clatterforge::{
        agents::{LocalAgentPolicy, ModelTier},
        codex::{CodexPolicy, Executable, PinnedCommand},
    };
    let mut presets = AgentPresets::open(&f.state).unwrap();
    let old = presets.get("worker-a").unwrap();
    let scope = vec!["a.txt".to_string(), "b.txt".to_string()];

    let mut worker_pairs = Vec::new();
    if include_local {
        let local_policy = LocalAgentPolicy::inspect(
            Path::new(env!("CARGO_BIN_EXE_clatterforge")),
            f.workflow.assignment.profile.inference.targets[0]
                .endpoint
                .clone(),
            "gemma4:fixture".into(),
            Some("a".repeat(64)),
        )
        .unwrap();
        presets
            .create(
                "tier-local",
                f.repo.clone(),
                AgentBackend::Local(local_policy),
                scope.clone(),
                old.verification_files.clone(),
                old.verify_executable.clone(),
                old.verify_args.clone(),
                5000,
                5000,
                None,
                "fixture",
            )
            .unwrap();
        presets
            .set_routing_policy(
                "tier-local",
                Some(ModelTier::Local),
                vec!["gemma4:fixture".into()],
            )
            .unwrap();
        worker_pairs.push((
            "tier-local".into(),
            "Boilerplate local model implementer".into(),
        ));
    }

    let cursor_policy = CursorAgentPolicy::inspect(
        worker(),
        &worker().parent().unwrap().join("index.js"),
        "auto".into(),
    )
    .unwrap();
    presets
        .create(
            "tier-cursor",
            f.repo.clone(),
            AgentBackend::CursorAgent(cursor_policy),
            scope.clone(),
            old.verification_files.clone(),
            old.verify_executable.clone(),
            old.verify_args.clone(),
            5000,
            5000,
            None,
            "fixture",
        )
        .unwrap();
    presets
        .set_routing_policy(
            "tier-cursor",
            Some(ModelTier::Fast),
            vec![
                "auto".into(),
                "gpt-4o-mini".into(),
                "claude-3-7-sonnet".into(),
                "sonnet-4-thinking".into(),
                "gpt-5".into(),
            ],
        )
        .unwrap();
    worker_pairs.push((
        "tier-cursor".into(),
        "Fast and standard Cursor CLI implementer".into(),
    ));

    let codex_home = f._dir.path().join("codex-home");
    fs::create_dir_all(&codex_home).unwrap();
    let verifier_cmd = PinnedCommand {
        executable: Executable::new(&old.verify_executable).unwrap(),
        args: old.verify_args.clone(),
    };
    let codex_policy =
        CodexPolicy::inspect(worker(), &codex_home, "o3".into(), verifier_cmd).unwrap();
    presets
        .create(
            "tier-codex",
            f.repo.clone(),
            AgentBackend::Codex(codex_policy),
            scope,
            old.verification_files,
            old.verify_executable,
            old.verify_args,
            5000,
            5000,
            None,
            "fixture",
        )
        .unwrap();
    presets
        .set_routing_policy(
            "tier-codex",
            Some(ModelTier::Frontier),
            vec![
                "o4-mini".into(),
                "gpt-4.1".into(),
                "o3".into(),
                "gpt-5.3-codex".into(),
            ],
        )
        .unwrap();
    worker_pairs.push((
        "tier-codex".into(),
        "Frontier Codex CLI implementer for complex or escalated tasks".into(),
    ));

    let workflows = Workflows::open(&f.state).unwrap();
    workflows
        .create_profile(
            "tiered".into(),
            f.workflow.assignment.profile.inference.clone(),
            worker_pairs,
            max_steps,
            5000,
            "fixture".into(),
        )
        .unwrap();
}

#[test]
fn boilerplate_tasks_route_to_local_or_fast_model_and_never_use_frontier() {
    // 1. With local worker available: planner attempts to dispatch frontier `tier-codex` with `o3`
    // for a boilerplate task, but Conductor downgrades to `tier-local` (local-model) and never uses frontier.
    let local_worker_response = json!({
        "summary": "Updated boilerplate files locally",
        "files": [
            {"path": "a.txt", "contents": "fixed-a"},
            {"path": "b.txt", "contents": "fixed-b"}
        ]
    });
    let model = Model::texts(vec![
        json!({
            "action": "dispatch",
            "worker": "tier-codex",
            "model": "o3",
            "objective": "WORK_BOTH Add boilerplate getter and setter comments",
            "acceptance": "independent verifier passes",
            "rationale": "Attempt frontier dispatch on boilerplate task"
        })
        .to_string(),
        local_worker_response.to_string(),
        json!({"action": "finish", "summary": "Boilerplate completed by local model"}).to_string(),
    ]);
    let f = fixture(&model, 3);
    setup_tiered_profile(&f, true, 3);
    let mut workflows = Workflows::open(&f.state).unwrap();
    let w = workflows
        .create(
            "tiered",
            "Add boilerplate getter and setter comments to a.txt and b.txt".into(),
            "Both files pass the protected verifier".into(),
        )
        .unwrap();
    let w = workflows
        .approve(
            &w.id,
            w.revision,
            &w.assignment.digest().unwrap(),
            "fixture",
        )
        .unwrap();
    let done = workflows.run(&w.id, w.revision).unwrap();
    assert_eq!(done.status, AgentStatus::AwaitingReview, "{:?}", done.error);
    assert_eq!(done.steps.len(), 1);
    assert_eq!(done.steps[0].worker, "tier-local");
    let step_res = done.steps[0].result.as_ref().unwrap();
    assert_eq!(step_res["backend"], "local-model");
    assert_eq!(step_res["model"], "gemma4:fixture");
    assert_eq!(step_res["tier"], "local");
    assert_eq!(step_res["complexity"], "boilerplate");

    // 2. With CLI workers only (`tier-cursor` and `tier-codex`): boilerplate task routes to `tier-cursor`
    // with `auto` (fast tier) and passes `--model auto` to the CLI, never `o3` or `gpt-5`.
    let model2 = Model::new(vec![
        json!({
            "action": "dispatch",
            "worker": "tier-codex",
            "model": "gpt-5.3-codex",
            "objective": "WORK_BOTH Fix typo and add boilerplate doc comment",
            "acceptance": "independent verifier passes",
            "rationale": "Attempt frontier model on boilerplate task"
        }),
        json!({"action": "finish", "summary": "Boilerplate completed by fast CLI model"}),
    ]);
    let f2 = fixture(&model2, 3);
    setup_tiered_profile(&f2, false, 3);
    let mut workflows2 = Workflows::open(&f2.state).unwrap();
    let w2 = workflows2
        .create(
            "tiered",
            "Fix typo and add boilerplate doc comment in a.txt and b.txt".into(),
            "Both files pass the protected verifier".into(),
        )
        .unwrap();
    let w2 = workflows2
        .approve(
            &w2.id,
            w2.revision,
            &w2.assignment.digest().unwrap(),
            "fixture",
        )
        .unwrap();
    let done2 = workflows2.run(&w2.id, w2.revision).unwrap();
    assert_eq!(done2.status, AgentStatus::AwaitingReview);
    assert_eq!(done2.steps[0].worker, "tier-cursor");
    let res2 = done2.steps[0].result.as_ref().unwrap();
    assert_eq!(res2["backend"], "cursor-agent");
    assert_eq!(res2["model"], "auto");
    assert_eq!(res2["tier"], "fast");
    assert_eq!(res2["complexity"], "boilerplate");
    assert!(
        res2["agent_summary"]
            .as_str()
            .unwrap()
            .contains("cli_model=auto")
    );
}

#[test]
fn complex_tasks_route_to_frontier_cli_and_failed_stages_switch_cli_and_escalate_tier() {
    // 1. Complex architectural/concurrency task routes directly to `tier-codex` with frontier model `o3`.
    let model = Model::new(vec![
        json!({
            "action": "dispatch",
            "worker": "tier-cursor",
            "objective": "WORK_BOTH Redesign concurrent state machine and fix race condition",
            "acceptance": "independent verifier passes",
            "rationale": "Dispatch implementation"
        }),
        json!({"action": "finish", "summary": "Complex concurrency fix verified"}),
    ]);
    let f = fixture(&model, 3);
    setup_tiered_profile(&f, true, 3);
    let mut workflows = Workflows::open(&f.state).unwrap();
    let w = workflows
        .create(
            "tiered",
            "Redesign concurrent state machine and fix race condition across a.txt and b.txt"
                .into(),
            "Both files pass the protected verifier".into(),
        )
        .unwrap();
    let w = workflows
        .approve(
            &w.id,
            w.revision,
            &w.assignment.digest().unwrap(),
            "fixture",
        )
        .unwrap();
    let done = workflows.run(&w.id, w.revision).unwrap();
    assert_eq!(done.status, AgentStatus::AwaitingReview);
    assert_eq!(done.steps[0].worker, "tier-codex");
    let res = done.steps[0].result.as_ref().unwrap();
    assert_eq!(res["backend"], "codex");
    assert_eq!(res["model"], "o3");
    assert_eq!(res["tier"], "frontier");
    assert_eq!(res["complexity"], "complex");
    assert!(
        res["agent_summary"]
            .as_str()
            .unwrap()
            .contains("cli_model=o3")
    );

    // 2. Standard task starts on `tier-cursor` (`auto`, fast tier). When step 0 fails verification
    // (`WORK_A` leaves `b.txt` unfixed), step 1 automatically switches CLI from `cursor-agent` to `codex`
    // (`tier-codex`) and escalates model tier from `fast` (`auto`) to `frontier` (`o3`).
    let model_esc = Model::new(vec![
        json!({
            "action": "dispatch",
            "worker": "tier-cursor",
            "objective": "WORK_A Implement initial feature handler",
            "acceptance": "independent verifier passes",
            "rationale": "Start with fast Cursor model"
        }),
        json!({
            "action": "dispatch",
            "worker": "tier-cursor",
            "objective": "WORK_B Complete remaining feature handler",
            "acceptance": "independent verifier passes",
            "rationale": "Repair after failed verification"
        }),
        json!({"action": "finish", "summary": "Escalated repair succeeded"}),
    ]);
    let f_esc = fixture(&model_esc, 3);
    setup_tiered_profile(&f_esc, true, 3);
    let mut workflows_esc = Workflows::open(&f_esc.state).unwrap();
    let w_esc = workflows_esc
        .create(
            "tiered",
            "Implement feature handler updates in a.txt and b.txt".into(),
            "Both files pass the protected verifier".into(),
        )
        .unwrap();
    let w_esc = workflows_esc
        .approve(
            &w_esc.id,
            w_esc.revision,
            &w_esc.assignment.digest().unwrap(),
            "fixture",
        )
        .unwrap();
    let done_esc = workflows_esc.run(&w_esc.id, w_esc.revision).unwrap();
    assert_eq!(done_esc.status, AgentStatus::AwaitingReview);
    assert_eq!(done_esc.steps.len(), 2);
    assert_eq!(done_esc.steps[0].worker, "tier-cursor");
    assert_eq!(
        done_esc.steps[0].result.as_ref().unwrap()["backend"],
        "cursor-agent"
    );
    assert_eq!(done_esc.steps[0].result.as_ref().unwrap()["model"], "auto");
    assert_eq!(done_esc.steps[0].result.as_ref().unwrap()["tier"], "fast");
    assert_eq!(
        done_esc.steps[0].result.as_ref().unwrap()["verification_outcome"],
        "failed"
    );

    assert_eq!(done_esc.steps[1].worker, "tier-codex");
    assert_eq!(
        done_esc.steps[1].result.as_ref().unwrap()["backend"],
        "codex"
    );
    assert_eq!(done_esc.steps[1].result.as_ref().unwrap()["model"], "o3");
    assert_eq!(
        done_esc.steps[1].result.as_ref().unwrap()["tier"],
        "frontier"
    );
    assert_eq!(
        done_esc.steps[1].result.as_ref().unwrap()["verification_outcome"],
        "succeeded"
    );
}

#[test]
fn builtin_router_executes_workflows_deterministically_with_zero_local_llm_footprint() {
    use clatterforge::{agents::ModelTier, workflow::builtin_router_config};

    // Create fixture with NO responses queued on the fake local HTTP model — proving zero local LLM calls occur.
    let unused_model = Model::new(vec![]);
    let f = fixture(&unused_model, 3);
    let mut presets = AgentPresets::open(&f.state).unwrap();
    let old = presets.get("worker-a").unwrap();
    let scope = vec!["a.txt".to_string(), "b.txt".to_string()];

    let cursor_dir = f._dir.path().join("cursor-cli");
    fs::create_dir_all(&cursor_dir).unwrap();
    let index_js = cursor_dir.join("index.js");
    fs::write(&index_js, "console.log('cursor fixture');").unwrap();

    for (name, default_model, tier) in [
        ("cursor-fast", "composer-2.5-fast", ModelTier::Fast),
        ("cursor-standard", "composer-2.5", ModelTier::Standard),
        (
            "cursor-frontier",
            "claude-opus-5-thinking-high",
            ModelTier::Frontier,
        ),
    ] {
        let policy = CursorAgentPolicy::inspect(worker(), &index_js, default_model.into()).unwrap();
        presets
            .create(
                name,
                f.repo.clone(),
                AgentBackend::CursorAgent(policy),
                scope.clone(),
                old.verification_files.clone(),
                old.verify_executable.clone(),
                old.verify_args.clone(),
                5000,
                5000,
                None,
                "fixture",
            )
            .unwrap();
        presets
            .set_routing_policy(
                name,
                Some(tier),
                vec![
                    "composer-2.5-fast".into(),
                    "composer-2.5".into(),
                    "claude-opus-5-thinking-high".into(),
                ],
            )
            .unwrap();
    }

    let workflows = Workflows::open(&f.state).unwrap();
    workflows
        .create_profile(
            "zero-llm".into(),
            builtin_router_config(&f.state),
            vec![
                (
                    "cursor-fast".into(),
                    "Fast Cursor tier for boilerplate".into(),
                ),
                (
                    "cursor-standard".into(),
                    "Standard Cursor tier for feature work".into(),
                ),
                (
                    "cursor-frontier".into(),
                    "Frontier Cursor tier for complex or escalated work".into(),
                ),
            ],
            3,
            5000,
            "fixture".into(),
        )
        .unwrap();

    let mut workflows = Workflows::open(&f.state).unwrap();
    let w = workflows
        .create(
            "zero-llm",
            "WORK_BOTH Add boilerplate getter and setter doc comment".into(),
            "Both files pass the protected verifier".into(),
        )
        .unwrap();
    let w = workflows
        .approve(
            &w.id,
            w.revision,
            &w.assignment.digest().unwrap(),
            "fixture",
        )
        .unwrap();
    let done = workflows.run(&w.id, w.revision).unwrap();
    assert_eq!(done.status, AgentStatus::AwaitingReview);
    assert_eq!(done.steps.len(), 1);
    assert_eq!(done.steps[0].worker, "cursor-fast");
    assert_eq!(
        done.steps[0].result.as_ref().unwrap()["model"],
        "composer-2.5-fast"
    );
    assert_eq!(done.steps[0].result.as_ref().unwrap()["tier"], "fast");
    assert_eq!(done.decisions.len(), 2);
    assert_eq!(done.decisions[0].job_id, "builtin-step-1");
    assert_eq!(done.decisions[1].job_id, "builtin-step-2");
}

#[test]
fn single_cli_cursor_only_escalates_across_tiers_and_refreshes_live_models_mid_run() {
    use clatterforge::{agents::ModelTier, workflow::builtin_router_config};

    let unused_model = Model::new(vec![]);
    let f = fixture(&unused_model, 3);
    let mut presets = AgentPresets::open(&f.state).unwrap();
    let old = presets.get("worker-a").unwrap();
    let scope = vec!["a.txt".to_string(), "b.txt".to_string()];

    let cursor_dir = f._dir.path().join("cursor-live-cli");
    fs::create_dir_all(&cursor_dir).unwrap();
    let index_js = cursor_dir.join("index.js");
    fs::write(&index_js, "console.log('cursor fixture');").unwrap();
    // Simulate a mid-run CLI catalog update where `sonnet-4-thinking` was deprecated and replaced by
    // `claude-opus-5-thinking-high` in `cursor-agent --list-models`!
    fs::write(
        cursor_dir.join("live_models.txt"),
        "Available models\n\ncomposer-2.5-fast - Composer 2.5 Fast\ncomposer-2.5 - Composer 2.5\nclaude-opus-5-thinking-high - Claude 5 Opus Thinking\n",
    )
    .unwrap();

    for (name, default_model, tier) in [
        ("cursor-fast", "composer-2.5-fast", ModelTier::Fast),
        ("cursor-standard", "composer-2.5", ModelTier::Standard),
        ("cursor-frontier", "sonnet-4-thinking", ModelTier::Frontier),
    ] {
        let policy = CursorAgentPolicy::inspect(worker(), &index_js, default_model.into()).unwrap();
        presets
            .create(
                name,
                f.repo.clone(),
                AgentBackend::CursorAgent(policy),
                scope.clone(),
                old.verification_files.clone(),
                old.verify_executable.clone(),
                old.verify_args.clone(),
                5000,
                5000,
                None,
                "fixture",
            )
            .unwrap();
        // Leave `allowed_models` empty (`O14`, `D2`) so the preset dynamically uses the live
        // CLI catalog (`live_models.txt`) when selecting a model for the tier.
        presets
            .set_routing_policy(name, Some(tier), Vec::new())
            .unwrap();
    }

    let workflows = Workflows::open(&f.state).unwrap();
    workflows
        .create_profile(
            "cursor-single-cli".into(),
            builtin_router_config(&f.state),
            vec![
                ("cursor-fast".into(), "Fast Cursor tier".into()),
                ("cursor-standard".into(), "Standard Cursor tier".into()),
                ("cursor-frontier".into(), "Frontier Cursor tier".into()),
            ],
            3,
            5000,
            "fixture".into(),
        )
        .unwrap();

    let mut workflows = Workflows::open(&f.state).unwrap();
    // Standard task starts on `cursor-standard` (`composer-2.5`), which fails verification because
    // `WORK_IF_FRONTIER` only fixes `b.txt` when executed by a Frontier model.
    // Step 2 escalates on the SAME `cursor-agent` CLI to `cursor-frontier`, discovers via `--list-models`
    // that `sonnet-4-thinking` was replaced by `claude-opus-5-thinking-high`, and succeeds!
    let w = workflows
        .create(
            "cursor-single-cli",
            "WORK_IF_FRONTIER Implement pagination handler across files".into(),
            "Both files pass the protected verifier".into(),
        )
        .unwrap();
    let w = workflows
        .approve(
            &w.id,
            w.revision,
            &w.assignment.digest().unwrap(),
            "fixture",
        )
        .unwrap();
    let done = workflows.run(&w.id, w.revision).unwrap();
    assert_eq!(done.status, AgentStatus::AwaitingReview);
    assert_eq!(done.steps.len(), 2);

    // Step 0: ran on Standard tier (`cursor-standard`, `composer-2.5`) and failed verification
    assert_eq!(done.steps[0].worker, "cursor-standard");
    assert_eq!(
        done.steps[0].result.as_ref().unwrap()["backend"],
        "cursor-agent"
    );
    assert_eq!(
        done.steps[0].result.as_ref().unwrap()["model"],
        "composer-2.5"
    );
    assert_eq!(done.steps[0].result.as_ref().unwrap()["tier"], "standard");
    assert_eq!(
        done.steps[0].result.as_ref().unwrap()["verification_outcome"],
        "failed"
    );

    // Step 1: escalated on the SAME CLI (`cursor-agent`) to `cursor-frontier` and dynamically resolved
    // the live available frontier model `claude-opus-5-thinking-high` via pre-request `--list-models`!
    assert_eq!(done.steps[1].worker, "cursor-frontier");
    assert_eq!(
        done.steps[1].result.as_ref().unwrap()["backend"],
        "cursor-agent"
    );
    assert_eq!(
        done.steps[1].result.as_ref().unwrap()["model"],
        "claude-opus-5-thinking-high"
    );
    assert!(
        done.steps[1].result.as_ref().unwrap()["agent_summary"]
            .as_str()
            .unwrap()
            .contains("cli_model=claude-opus-5-thinking-high"),
        "expected pre-request live model discovery to select claude-opus-5-thinking-high, got: {}",
        done.steps[1].result.as_ref().unwrap()["agent_summary"]
    );
    assert_eq!(
        done.steps[1].result.as_ref().unwrap()["verification_outcome"],
        "succeeded"
    );
}

#[test]
fn approved_model_missing_from_live_catalog_fails_closed_at_execution() {
    let unused_model = Model::new(vec![]);
    let f = fixture(&unused_model, 1);
    let presets = AgentPresets::open(&f.state).unwrap();
    let old = presets.get("worker-a").unwrap();

    let cursor_dir = f._dir.path().join("cursor-fail-closed-cli");
    fs::create_dir_all(&cursor_dir).unwrap();
    let index_js = cursor_dir.join("index.js");
    fs::write(&index_js, "console.log('cursor fixture');").unwrap();
    fs::write(
        cursor_dir.join("live_models.txt"),
        "Available models\n\ncomposer-2.5-fast - Composer 2.5 Fast\n",
    )
    .unwrap();
    clatterforge::agents::clear_cursor_model_cache();

    let policy =
        CursorAgentPolicy::inspect(worker(), &index_js, "deprecated-model".into()).unwrap();
    let mut spec = old.spec("WORK_BOTH".into(), "pass verifier".into());
    spec.model = "deprecated-model".into();

    let mut agents = Agents::open(&f.state).unwrap();
    let created = agents
        .create(spec, AgentBackend::CursorAgent(policy))
        .unwrap();
    let approved = agents
        .approve(
            &created.id,
            created.revision,
            &created.assignment.digest().unwrap(),
            "fixture",
        )
        .unwrap();
    let outcome = agents.run(&approved.id, approved.revision).unwrap();
    assert_eq!(outcome.status, AgentStatus::Unknown);
    assert!(
        outcome.execution.is_none(),
        "no child process should be spawned"
    );
    assert!(
        outcome
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("approved model 'deprecated-model' is not available"),
        "expected fail-closed error, got: {:?}",
        outcome.error
    );
}

#[test]
fn reviewer_needs_work_attributes_failure_to_implementer_and_multi_scope_router_scopes_objectives()
{
    use clatterforge::{
        agentlet::{Capability, Definition},
        agents::ModelTier,
        workflow::{Decision, Step, builtin_router_config, plan_next_decision, recommend_route},
    };

    let unused_model = Model::new(vec![]);
    let f = fixture(&unused_model, 4);
    let mut presets = AgentPresets::open(&f.state).unwrap();
    let old = presets.get("worker-a").unwrap();

    // 1. Verify multi-scope `plan_next_decision` scopes each worker's objective (`O8`).
    let w_multi = Workflows::open(&f.state)
        .unwrap()
        .create(
            "fixture",
            "WORK_A Fix both a.txt and b.txt".into(),
            "Both files pass".into(),
        )
        .unwrap();
    match plan_next_decision(&w_multi) {
        Decision::Dispatch {
            worker, objective, ..
        } => {
            assert_eq!(worker, "worker-a");
            assert!(
                objective.starts_with("WORK_A Fix both a.txt and b.txt"),
                "stage objective must start with workflow objective: {objective}"
            );
            assert!(
                objective.contains("Stage scope 1/2: worker 'worker-a', purpose: Fix only a.txt, editable files: a.txt"),
                "stage objective must include per-scope decomposition: {objective}"
            );
        }
        other => panic!("expected Dispatch, got {other:?}"),
    }

    // 2. Verify reviewer `needs_work` attributes failure to the implementer (`O7`), not the reviewer.
    let cursor_dir = f._dir.path().join("cursor-o7");
    fs::create_dir_all(&cursor_dir).unwrap();
    let index_js = cursor_dir.join("index.js");
    fs::write(&index_js, "// fixture").unwrap();

    let codex_home = f._dir.path().join("codex-o7");
    fs::create_dir_all(&codex_home).unwrap();
    let verifier_cmd = clatterforge::codex::PinnedCommand {
        executable: clatterforge::codex::Executable::new(&old.verify_executable).unwrap(),
        args: old.verify_args.clone(),
    };

    let cursor_fast = CursorAgentPolicy::inspect(worker(), &index_js, "auto".into()).unwrap();
    let codex_frontier = clatterforge::codex::CodexPolicy::inspect(
        worker(),
        &codex_home,
        "o3".into(),
        verifier_cmd.clone(),
    )
    .unwrap();
    let codex_rev = clatterforge::codex::CodexPolicy::inspect(
        worker(),
        &codex_home,
        "o4-mini".into(),
        verifier_cmd,
    )
    .unwrap();

    presets
        .create(
            "impl-cursor-fast",
            f.repo.clone(),
            AgentBackend::CursorAgent(cursor_fast),
            vec!["a.txt".into(), "b.txt".into()],
            old.verification_files.clone(),
            old.verify_executable.clone(),
            old.verify_args.clone(),
            5000,
            5000,
            None,
            "fixture",
        )
        .unwrap();
    presets
        .set_routing_policy(
            "impl-cursor-fast",
            Some(ModelTier::Fast),
            vec!["auto".into()],
        )
        .unwrap();

    presets
        .create(
            "impl-codex-frontier",
            f.repo.clone(),
            AgentBackend::Codex(codex_frontier),
            vec!["a.txt".into(), "b.txt".into()],
            old.verification_files.clone(),
            old.verify_executable.clone(),
            old.verify_args.clone(),
            5000,
            5000,
            None,
            "fixture",
        )
        .unwrap();
    presets
        .set_routing_policy(
            "impl-codex-frontier",
            Some(ModelTier::Frontier),
            vec!["o3".into()],
        )
        .unwrap();

    presets
        .create(
            "rev-codex-base",
            f.repo.clone(),
            AgentBackend::Codex(codex_rev),
            vec!["a.txt".into()],
            old.verification_files.clone(),
            old.verify_executable.clone(),
            old.verify_args.clone(),
            5000,
            5000,
            None,
            "fixture",
        )
        .unwrap();
    presets
        .bind_agentlet(
            "rev-codex-base",
            "rev-codex",
            Definition {
                schema_version: 1,
                id: "reviewer".into(),
                version: 1,
                purpose: "Review candidate".into(),
                instructions: "Inspect candidate".into(),
                capability: Capability::Review,
                completion_gate: true,
            },
            "fixture",
        )
        .unwrap();
    presets
        .set_routing_policy("rev-codex", Some(ModelTier::Fast), vec!["o4-mini".into()])
        .unwrap();

    let workflows = Workflows::open(&f.state).unwrap();
    let profile = workflows
        .create_profile(
            "o7-profile".into(),
            builtin_router_config(&f.state),
            vec![
                ("impl-cursor-fast".into(), "Fast Cursor implementer".into()),
                (
                    "impl-codex-frontier".into(),
                    "Frontier Codex implementer".into(),
                ),
                ("rev-codex".into(), "Codex review gate".into()),
            ],
            4,
            5000,
            "fixture".into(),
        )
        .unwrap();

    // Step 0: `impl-cursor-fast` (`cursor-agent`, `auto`) passed verification.
    // Step 1: `rev-codex` (`codex`, `o4-mini`) returned `needs_work`.
    // Escalation must blame `impl-cursor-fast` (`cursor-agent`) and steer repair to `impl-codex-frontier` (`codex`, `o3`),
    // NOT blame `rev-codex`'s `codex` backend and steer back to `impl-cursor-fast`!
    let prior_steps = vec![
        Step {
            worker: "impl-cursor-fast".into(),
            task_id: "t1".into(),
            evidence_digest: Some("d1".into()),
            result: Some(json!({
                "status": "awaiting_review",
                "backend": "cursor-agent",
                "model": "auto",
                "tier": "fast",
                "execution_outcome": "succeeded",
                "verification_outcome": "succeeded"
            })),
            files: Default::default(),
            checkpoint: None,
        },
        Step {
            worker: "rev-codex".into(),
            task_id: "t2".into(),
            evidence_digest: Some("d2".into()),
            result: Some(json!({
                "status": "needs_work",
                "backend": "codex",
                "model": "o4-mini",
                "tier": "fast",
                "execution_outcome": "succeeded",
                "verification_outcome": "succeeded",
                "agentlet_result": {
                    "schema_version": 1,
                    "outcome": "needs_work",
                    "summary": "Missing edge case",
                    "findings": [{"severity": "error", "message": "Edge case missing"}],
                    "next_objective": "Fix edge case"
                }
            })),
            files: Default::default(),
            checkpoint: None,
        },
    ];

    let route = recommend_route(&profile, "Implement feature", "Pass tests", &prior_steps).unwrap();
    assert_eq!(
        route.recommended_worker, "impl-codex-frontier",
        "repair after Codex reviewer needs_work must escalate from Cursor implementer to Frontier Codex implementer: {route:?}"
    );
    assert_eq!(route.recommended_backend, "codex");
    assert_eq!(route.recommended_model, "o3");
}

#[test]
fn local_agent_worker_enforces_expected_artifact_digest_and_explicit_allowed_files() {
    let model = Model::new(vec![json!({
        "summary": "Updated a.txt via local worker",
        "files": [{"path": "a.txt", "contents": "fixed-a-by-local-worker"}]
    })]);
    let (_dir, dir_root) = canonical_tempdir();
    let ws = dir_root.as_path().join("ws");
    fs::create_dir_all(&ws).unwrap();
    fs::write(ws.join("a.txt"), "old-a").unwrap();

    // 1. Mismatched `--expected-digest` must fail closed (`O1`) before touching any file.
    let bad_args = vec![
        "-p".to_string(),
        "Update file".to_string(),
        "--model".to_string(),
        "gemma4:fixture".to_string(),
        "--workspace".to_string(),
        ws.to_string_lossy().into_owned(),
        "--endpoint".to_string(),
        model.url.clone(),
        "--expected-digest".to_string(),
        "b".repeat(64),
        "--allowed-file".to_string(),
        "a.txt".to_string(),
    ];
    let err = clatterforge::agents::run_local_agent_worker(&bad_args).unwrap_err();
    assert!(
        err.to_string().contains("artifact digest mismatch"),
        "expected digest mismatch error, got: {err:#}"
    );
    assert_eq!(fs::read_to_string(ws.join("a.txt")).unwrap(), "old-a");

    // 2. Matching `--expected-digest` and explicit `--allowed-file` (without `"allowed_files":[` in prompt) succeeds (`O1`, `O2`).
    let ok_args = vec![
        "-p".to_string(),
        "Update file without prompt-embedded allowed_files".to_string(),
        "--model".to_string(),
        "gemma4:fixture".to_string(),
        "--workspace".to_string(),
        ws.to_string_lossy().into_owned(),
        "--endpoint".to_string(),
        model.url.clone(),
        "--expected-digest".to_string(),
        "a".repeat(64),
        "--allowed-file".to_string(),
        "a.txt".to_string(),
    ];
    let code = clatterforge::agents::run_local_agent_worker(&ok_args).unwrap();
    assert_eq!(code, 0);
    assert_eq!(
        fs::read_to_string(ws.join("a.txt")).unwrap(),
        "fixed-a-by-local-worker"
    );
}

#[test]
fn quota_429_swaps_cli_to_equivalent_tier_and_board_sorts_most_recent_first() {
    use clatterforge::{
        agents::{AgyAgentPolicy, ModelTier},
        service::TaskService,
        workflow::builtin_router_config,
    };

    let unused_model = Model::new(vec![]);
    let f = fixture(&unused_model, 4);
    let mut presets = AgentPresets::open(&f.state).unwrap();
    let old = presets.get("worker-a").unwrap();
    let scope = vec!["a.txt".to_string(), "b.txt".to_string()];

    let cursor_dir = f._dir.path().join("quota-cursor");
    fs::create_dir_all(&cursor_dir).unwrap();
    let index_js = cursor_dir.join("index.js");
    fs::write(&index_js, "console.log('cursor quota fixture');").unwrap();

    presets
        .create(
            "agy-frontier",
            f.repo.clone(),
            AgentBackend::Agy(
                AgyAgentPolicy::inspect(worker(), "claude-opus-5-5-high".into()).unwrap(),
            ),
            scope.clone(),
            old.verification_files.clone(),
            old.verify_executable.clone(),
            old.verify_args.clone(),
            5000,
            5000,
            None,
            "fixture",
        )
        .unwrap();
    presets
        .set_routing_policy(
            "agy-frontier",
            Some(ModelTier::Frontier),
            vec!["claude-opus-5-5-high".into(), "gemini-3.8-flash-low".into()],
        )
        .unwrap();

    presets
        .create(
            "cursor-frontier",
            f.repo.clone(),
            AgentBackend::CursorAgent(
                CursorAgentPolicy::inspect(
                    worker(),
                    &index_js,
                    "claude-opus-5-thinking-high".into(),
                )
                .unwrap(),
            ),
            scope,
            old.verification_files.clone(),
            old.verify_executable.clone(),
            old.verify_args.clone(),
            5000,
            5000,
            None,
            "fixture",
        )
        .unwrap();
    presets
        .set_routing_policy(
            "cursor-frontier",
            Some(ModelTier::Frontier),
            vec!["claude-opus-5-thinking-high".into(), "composer-2.5".into()],
        )
        .unwrap();

    let workflows = Workflows::open(&f.state).unwrap();
    workflows
        .create_profile(
            "quota-fallback-profile".into(),
            builtin_router_config(&f.state),
            vec![
                ("agy-frontier".into(), "Primary Frontier CLI".into()),
                ("cursor-frontier".into(), "Fallback Frontier CLI".into()),
            ],
            4,
            5000,
            "fixture".into(),
        )
        .unwrap();

    let mut workflows = Workflows::open(&f.state).unwrap();
    let older_wf = workflows
        .create(
            "quota-fallback-profile",
            "Older workflow created first".into(),
            "Both files pass".into(),
        )
        .unwrap();
    // Ensure timestamp advances cleanly (unix secs precision on timestamps) if needed; at minimum
    // updating/running the second workflow yields a newer or higher rowid event.
    std::thread::sleep(Duration::from_millis(1100));
    let newer_wf = workflows
        .create(
            "quota-fallback-profile",
            "WORK_BOTH QUOTA_FAIL_claude-opus-5-5-high Redesign concurrent state machine and fix deadlock across both files".into(),
            "Both files pass the protected verifier".into(),
        )
        .unwrap();
    let approved = workflows
        .approve(
            &newer_wf.id,
            newer_wf.revision,
            &newer_wf.assignment.digest().unwrap(),
            "fixture",
        )
        .unwrap();
    let outcome = workflows.run(&newer_wf.id, approved.revision).unwrap();
    assert_eq!(
        outcome.status,
        AgentStatus::AwaitingReview,
        "expected quota fallback workflow to recover to AwaitingReview: {outcome:?}"
    );
    assert_eq!(outcome.steps.len(), 2);
    assert_eq!(outcome.steps[0].worker, "agy-frontier");
    assert_eq!(
        outcome.steps[0].result.as_ref().unwrap()["quota_exhausted"],
        json!(true)
    );
    assert_eq!(
        outcome.steps[0].result.as_ref().unwrap()["provider_wide_quota"],
        json!(true)
    );
    assert_eq!(outcome.steps[1].worker, "cursor-frontier");
    assert_eq!(
        outcome.steps[1].result.as_ref().unwrap()["model"],
        json!("claude-opus-5-thinking-high")
    );
    assert_eq!(
        outcome.steps[1].result.as_ref().unwrap()["verification_outcome"],
        json!("succeeded")
    );

    // Verify `Workflows::list()` and `TaskService::board()` place newest / most recently active items first.
    let listed = workflows.list().unwrap();
    let older_pos = listed.iter().position(|w| w.id == older_wf.id).unwrap();
    let newer_pos = listed.iter().position(|w| w.id == newer_wf.id).unwrap();
    assert!(
        newer_pos < older_pos,
        "expected newer/active workflow first, got newer={newer_pos}, older={older_pos}"
    );

    let board = TaskService::open(&f.state).unwrap().board().unwrap();
    let board_older_pos = board.iter().position(|t| t.id == older_wf.id).unwrap();
    let board_newer_pos = board.iter().position(|t| t.id == newer_wf.id).unwrap();
    assert!(
        board_newer_pos < board_older_pos,
        "expected newer/active workflow first on board, got newer={board_newer_pos}, older={board_older_pos}"
    );
}

#[test]
fn standard_task_quota_fallback_swaps_across_all_clis_and_review_gate() {
    use clatterforge::{
        agentlet::{Capability, Definition},
        agents::{AgyAgentPolicy, ModelTier},
        codex::CodexPolicy,
        workflow::builtin_router_config,
    };

    let unused_model = Model::new(vec![]);
    let f = fixture(&unused_model, 4);
    let mut presets = AgentPresets::open(&f.state).unwrap();
    let old = presets.get("worker-a").unwrap();
    let scope = vec!["a.txt".to_string(), "b.txt".to_string()];

    let cursor_dir = f._dir.path().join("all-cli-cursor");
    fs::create_dir_all(&cursor_dir).unwrap();
    let index_js = cursor_dir.join("index.js");
    fs::write(&index_js, "console.log('cursor all cli fixture');").unwrap();
    fs::write(
        cursor_dir.join("live_models.txt"),
        "Available models\n\ncomposer-2.5-fast - Composer 2.5 Fast\ncomposer-2.5 - Composer 2.5\nclaude-opus-5-thinking-high - Claude 5 Opus\n",
    )
    .unwrap();

    let codex_home = f._dir.path().join("all-cli-codex");
    fs::create_dir_all(&codex_home).unwrap();
    fs::write(
        codex_home.join("models_cache.json"),
        serde_json::json!({
            "models": [
                {"slug": "o4-mini", "visibility": "list", "supported_in_api": true},
                {"slug": "gpt-5.2-codex", "visibility": "list", "supported_in_api": true},
                {"slug": "gpt-5.3-codex", "visibility": "list", "supported_in_api": true},
                {"slug": "hidden-internal-codex", "visibility": "hide", "supported_in_api": true}
            ]
        })
        .to_string(),
    )
    .unwrap();
    let verifier_cmd = clatterforge::codex::PinnedCommand {
        executable: clatterforge::codex::Executable::new(&old.verify_executable).unwrap(),
        args: old.verify_args.clone(),
    };

    let impl_def = Definition {
        schema_version: 1,
        id: "implementer".into(),
        version: 1,
        purpose: "Implement".into(),
        instructions: "Edit approved files".into(),
        capability: Capability::EditApprovedFiles,
        completion_gate: false,
    };
    let rev_def = Definition {
        schema_version: 1,
        id: "reviewer".into(),
        version: 1,
        purpose: "Review".into(),
        instructions: "Review candidate".into(),
        capability: Capability::Review,
        completion_gate: true,
    };

    // Mirrors `onboard.rs` multi-CLI profile:
    // - `agy-implementer` (Standard, `gemini-4-argon-medium`)
    // - `agy-frontier-implementer` (Frontier, `gemini-4-hydrogen-high`)
    // - `cursor-implementer` (Fast, `composer-2.5-fast`, live catalog includes Standard `composer-2.5`)
    // - `codex-frontier-implementer` (Frontier, `gpt-5.3-codex`, live catalog includes `gpt-5.2-codex`, `o4-mini`)
    // - `codex-reviewer` (Fast, `o4-mini`, completion gate)
    presets
        .create(
            "agy-std-base",
            f.repo.clone(),
            AgentBackend::Agy(
                AgyAgentPolicy::inspect(worker(), "gemini-4-argon-medium".into()).unwrap(),
            ),
            scope.clone(),
            old.verification_files.clone(),
            old.verify_executable.clone(),
            old.verify_args.clone(),
            5000,
            5000,
            None,
            "fixture",
        )
        .unwrap();
    presets
        .bind_agentlet(
            "agy-std-base",
            "agy-implementer",
            impl_def.clone(),
            "fixture",
        )
        .unwrap();
    presets
        .set_routing_policy(
            "agy-implementer",
            Some(ModelTier::Standard),
            vec![
                "gemini-4-argon-medium".into(),
                "gemini-3.8-flash-low".into(),
            ],
        )
        .unwrap();

    presets
        .create(
            "agy-frontier-base",
            f.repo.clone(),
            AgentBackend::Agy(
                AgyAgentPolicy::inspect(worker(), "gemini-4-hydrogen-high".into()).unwrap(),
            ),
            scope.clone(),
            old.verification_files.clone(),
            old.verify_executable.clone(),
            old.verify_args.clone(),
            5000,
            5000,
            None,
            "fixture",
        )
        .unwrap();
    presets
        .bind_agentlet(
            "agy-frontier-base",
            "agy-frontier-implementer",
            impl_def.clone(),
            "fixture",
        )
        .unwrap();
    presets
        .set_routing_policy(
            "agy-frontier-implementer",
            Some(ModelTier::Frontier),
            vec!["gemini-4-hydrogen-high".into()],
        )
        .unwrap();

    presets
        .create(
            "cursor-base",
            f.repo.clone(),
            AgentBackend::CursorAgent(
                CursorAgentPolicy::inspect(worker(), &index_js, "composer-2.5-fast".into())
                    .unwrap(),
            ),
            scope.clone(),
            old.verification_files.clone(),
            old.verify_executable.clone(),
            old.verify_args.clone(),
            5000,
            5000,
            None,
            "fixture",
        )
        .unwrap();
    presets
        .bind_agentlet(
            "cursor-base",
            "cursor-implementer",
            impl_def.clone(),
            "fixture",
        )
        .unwrap();
    presets
        .set_routing_policy("cursor-implementer", Some(ModelTier::Fast), Vec::new())
        .unwrap();

    presets
        .create(
            "codex-frontier-base",
            f.repo.clone(),
            AgentBackend::Codex(
                CodexPolicy::inspect(
                    worker(),
                    &codex_home,
                    "gpt-5.3-codex".into(),
                    verifier_cmd.clone(),
                )
                .unwrap(),
            ),
            scope,
            old.verification_files.clone(),
            old.verify_executable.clone(),
            old.verify_args.clone(),
            5000,
            5000,
            None,
            "fixture",
        )
        .unwrap();
    presets
        .bind_agentlet(
            "codex-frontier-base",
            "codex-frontier-implementer",
            impl_def,
            "fixture",
        )
        .unwrap();
    presets
        .set_routing_policy(
            "codex-frontier-implementer",
            Some(ModelTier::Frontier),
            Vec::new(),
        )
        .unwrap();

    presets
        .create(
            "codex-rev-base",
            f.repo.clone(),
            AgentBackend::Codex(
                CodexPolicy::inspect(worker(), &codex_home, "o4-mini".into(), verifier_cmd)
                    .unwrap(),
            ),
            vec!["a.txt".into()],
            old.verification_files.clone(),
            old.verify_executable.clone(),
            old.verify_args.clone(),
            5000,
            5000,
            None,
            "fixture",
        )
        .unwrap();
    presets
        .bind_agentlet("codex-rev-base", "codex-reviewer", rev_def, "fixture")
        .unwrap();
    presets
        .set_routing_policy("codex-reviewer", Some(ModelTier::Fast), Vec::new())
        .unwrap();

    let workflows = Workflows::open(&f.state).unwrap();
    workflows
        .create_profile(
            "all-cli-profile".into(),
            builtin_router_config(&f.state),
            vec![
                ("cursor-implementer".into(), "Cursor implementer".into()),
                (
                    "codex-frontier-implementer".into(),
                    "Codex implementer".into(),
                ),
                ("agy-implementer".into(), "AGY standard implementer".into()),
                (
                    "agy-frontier-implementer".into(),
                    "AGY frontier implementer".into(),
                ),
                ("codex-reviewer".into(), "Codex reviewer".into()),
            ],
            5,
            5000,
            "fixture".into(),
        )
        .unwrap();

    // Standard-complexity task (NO complex keywords!):
    // - S01 starts on `agy-implementer` (`gemini-4-argon-medium`) -> hits AGY 429
    // - S02 MUST swap away from `agy-implementer` (`codex` or `cursor` Standard) -> fails Cursor `composer-2.5` or Codex `gpt-5.2-codex`
    // - S03 swaps to remaining CLI -> succeeds!
    // - S04 runs `codex-reviewer` (`o4-mini`, hits model 429) -> S05 swaps reviewer model to `gpt-5.2-codex` and passes!
    let mut workflows = Workflows::open(&f.state).unwrap();
    let wf = workflows
        .create(
            "all-cli-profile",
            "WORK_BOTH QUOTA_FAIL_gemini-4-argon-medium QUOTA_FAIL_composer-2.5 QUOTA_FAIL_o4-mini Fix both files cleanly".into(),
            "Both files pass protected tests".into(),
        )
        .unwrap();
    let approved = workflows
        .approve(
            &wf.id,
            wf.revision,
            &wf.assignment.digest().unwrap(),
            "fixture",
        )
        .unwrap();
    let outcome = workflows.run(&wf.id, approved.revision).unwrap();
    assert_eq!(
        outcome.status,
        AgentStatus::AwaitingReview,
        "expected Standard workflow to recover across AGY -> Codex/Cursor and finish review: {outcome:?}"
    );
    assert_eq!(outcome.steps[0].worker, "agy-implementer");
    assert_eq!(
        outcome.steps[0].result.as_ref().unwrap()["model"],
        json!("gemini-4-argon-medium")
    );
    assert_eq!(
        outcome.steps[0].result.as_ref().unwrap()["quota_exhausted"],
        json!(true)
    );
    // S02 must NOT repeat `agy-implementer` + `gemini-4-argon-medium`.
    assert_ne!(
        outcome.steps[1].worker, "agy-implementer",
        "step 2 looped back onto exhausted AGY implementer: {:?}",
        outcome.steps
    );
    assert!(
        outcome.steps.iter().any(|s| s.worker == "codex-reviewer"
            && s.result
                .as_ref()
                .is_some_and(|r| r["verification_outcome"] == "succeeded"
                    && r["agentlet_result"]["outcome"] == "completed")),
        "expected reviewer completion gate to pass after fallback: {:?}",
        outcome.steps
    );
    // Hidden model must never be chosen.
    assert!(
        !outcome.steps.iter().any(|s| s
            .result
            .as_ref()
            .is_some_and(|r| r["model"] == "hidden-internal-codex")),
        "hidden Codex model should be ignored"
    );

    let digest = outcome.evidence_digest().unwrap();
    let accepted = workflows
        .review(&outcome.id, outcome.revision, &digest, "reviewer", true)
        .unwrap();
    assert_eq!(accepted.status, AgentStatus::Accepted);
}

#[test]
fn quota_fallback_handles_stage2_shared_worktree_and_long_jsonl_stream() {
    use clatterforge::{
        agentlet::{Capability, Definition},
        agents::{AgyAgentPolicy, ModelTier},
        codex::CodexPolicy,
        workflow::builtin_router_config,
    };

    let unused_model = Model::new(vec![]);
    let f = fixture(&unused_model, 4);
    let mut presets = AgentPresets::open(&f.state).unwrap();
    let old = presets.get("worker-a").unwrap();

    let codex_home = f._dir.path().join("stage2-codex");
    fs::create_dir_all(&codex_home).unwrap();
    let verifier_cmd = clatterforge::codex::PinnedCommand {
        executable: clatterforge::codex::Executable::new(&old.verify_executable).unwrap(),
        args: old.verify_args.clone(),
    };
    let impl_def = Definition {
        schema_version: 1,
        id: "implementer".into(),
        version: 1,
        purpose: "Implement".into(),
        instructions: "Edit approved files".into(),
        capability: Capability::EditApprovedFiles,
        completion_gate: false,
    };

    presets
        .create(
            "scope-a-base",
            f.repo.clone(),
            AgentBackend::Agy(
                AgyAgentPolicy::inspect(worker(), "gemini-4-argon-medium".into()).unwrap(),
            ),
            vec!["a.txt".into()],
            old.verification_files.clone(),
            old.verify_executable.clone(),
            old.verify_args.clone(),
            5000,
            5000,
            None,
            "fixture",
        )
        .unwrap();
    presets
        .bind_agentlet("scope-a-base", "scope-a-agy", impl_def.clone(), "fixture")
        .unwrap();

    presets
        .create(
            "scope-b-codex-base",
            f.repo.clone(),
            AgentBackend::Codex(
                CodexPolicy::inspect(worker(), &codex_home, "gpt-5.2-codex".into(), verifier_cmd)
                    .unwrap(),
            ),
            vec!["b.txt".into()],
            old.verification_files.clone(),
            old.verify_executable.clone(),
            old.verify_args.clone(),
            5000,
            5000,
            None,
            "fixture",
        )
        .unwrap();
    presets
        .bind_agentlet(
            "scope-b-codex-base",
            "scope-b-codex",
            impl_def.clone(),
            "fixture",
        )
        .unwrap();
    presets
        .set_routing_policy(
            "scope-b-codex",
            Some(ModelTier::Standard),
            vec!["gpt-5.2-codex".into()],
        )
        .unwrap();

    presets
        .create(
            "scope-b-agy-base",
            f.repo.clone(),
            AgentBackend::Agy(
                AgyAgentPolicy::inspect(worker(), "gemini-4-argon-medium".into()).unwrap(),
            ),
            vec!["b.txt".into()],
            old.verification_files.clone(),
            old.verify_executable.clone(),
            old.verify_args.clone(),
            5000,
            5000,
            None,
            "fixture",
        )
        .unwrap();
    presets
        .bind_agentlet("scope-b-agy-base", "scope-b-agy", impl_def, "fixture")
        .unwrap();
    presets
        .set_routing_policy(
            "scope-b-agy",
            Some(ModelTier::Standard),
            vec!["gemini-4-argon-medium".into()],
        )
        .unwrap();

    let workflows = Workflows::open(&f.state).unwrap();
    workflows
        .create_profile(
            "stage2-quota-profile".into(),
            builtin_router_config(&f.state),
            vec![
                ("scope-a-agy".into(), "Edit a.txt".into()),
                ("scope-b-codex".into(), "Edit b.txt via Codex".into()),
                ("scope-b-agy".into(), "Edit b.txt via AGY".into()),
            ],
            5,
            5000,
            "fixture".into(),
        )
        .unwrap();

    let mut workflows = Workflows::open(&f.state).unwrap();
    let wf = workflows
        .create(
            "stage2-quota-profile",
            "STAGE_SCOPED_WORK QUOTA_FAIL_LONG_JSONL_gpt-5.2-codex Update a.txt then b.txt".into(),
            "Both files pass".into(),
        )
        .unwrap();
    let approved = workflows
        .approve(
            &wf.id,
            wf.revision,
            &wf.assignment.digest().unwrap(),
            "fixture",
        )
        .unwrap();
    let outcome = workflows.run(&wf.id, approved.revision).unwrap();
    let step_errors: Vec<_> = outcome
        .steps
        .iter()
        .map(|s| {
            let t = Agents::open(&f.state).unwrap().task(&s.task_id).unwrap();
            (s.worker.clone(), t.status, t.error)
        })
        .collect();
    assert_eq!(
        outcome.status,
        AgentStatus::AwaitingReview,
        "expected stage-2 quota fallback after stage-1 edits to recover: step_errors={step_errors:?} outcome={outcome:?}"
    );
    assert_eq!(outcome.steps.len(), 3);
    assert_eq!(outcome.steps[0].worker, "scope-a-agy");
    assert_eq!(
        outcome.steps[0].result.as_ref().unwrap()["execution_outcome"],
        json!("succeeded")
    );
    assert_eq!(outcome.steps[1].worker, "scope-b-codex");
    assert_eq!(
        outcome.steps[1].result.as_ref().unwrap()["quota_exhausted"],
        json!(true)
    );
    assert_eq!(outcome.steps[2].worker, "scope-b-agy");
    assert_eq!(
        outcome.steps[2].result.as_ref().unwrap()["verification_outcome"],
        json!("succeeded")
    );

    let digest = outcome.evidence_digest().unwrap();
    let accepted = workflows
        .review(&outcome.id, outcome.revision, &digest, "reviewer", true)
        .unwrap();
    assert_eq!(accepted.status, AgentStatus::Accepted);
}

#[test]
fn quota_fallback_falls_back_to_local_when_cloud_clis_exhausted() {
    use clatterforge::{
        agentlet::{Capability, Definition},
        agents::{AgyAgentPolicy, LocalAgentPolicy, ModelTier},
        workflow::builtin_router_config,
    };

    let unused_model = Model::new(vec![]);
    let f = fixture(&unused_model, 4);
    let mut presets = AgentPresets::open(&f.state).unwrap();
    let old = presets.get("worker-a").unwrap();
    let scope = vec!["a.txt".to_string(), "b.txt".to_string()];
    let impl_def = Definition {
        schema_version: 1,
        id: "implementer".into(),
        version: 1,
        purpose: "Implement".into(),
        instructions: "Edit approved files".into(),
        capability: Capability::EditApprovedFiles,
        completion_gate: false,
    };

    presets
        .create(
            "agy-std-base",
            f.repo.clone(),
            AgentBackend::Agy(
                AgyAgentPolicy::inspect(worker(), "gemini-4-argon-medium".into()).unwrap(),
            ),
            scope.clone(),
            old.verification_files.clone(),
            old.verify_executable.clone(),
            old.verify_args.clone(),
            5000,
            5000,
            None,
            "fixture",
        )
        .unwrap();
    presets
        .bind_agentlet("agy-std-base", "agy-impl", impl_def.clone(), "fixture")
        .unwrap();
    presets
        .set_routing_policy(
            "agy-impl",
            Some(ModelTier::Standard),
            vec!["gemini-4-argon-medium".into()],
        )
        .unwrap();

    presets
        .create(
            "local-base",
            f.repo.clone(),
            AgentBackend::Local(
                LocalAgentPolicy::inspect(
                    worker(),
                    "http://127.0.0.1:11434".into(),
                    "gemma4:26b".into(),
                    None,
                )
                .unwrap(),
            ),
            scope,
            old.verification_files.clone(),
            old.verify_executable.clone(),
            old.verify_args.clone(),
            5000,
            5000,
            None,
            "fixture",
        )
        .unwrap();
    presets
        .bind_agentlet("local-base", "local-impl", impl_def, "fixture")
        .unwrap();
    presets
        .set_routing_policy(
            "local-impl",
            Some(ModelTier::Local),
            vec!["gemma4:26b".into()],
        )
        .unwrap();

    let workflows = Workflows::open(&f.state).unwrap();
    workflows
        .create_profile(
            "agy-to-local".into(),
            builtin_router_config(&f.state),
            vec![
                ("agy-impl".into(), "Cloud AGY".into()),
                ("local-impl".into(), "Local fallback".into()),
            ],
            4,
            5000,
            "fixture".into(),
        )
        .unwrap();

    let mut workflows = Workflows::open(&f.state).unwrap();
    let wf = workflows
        .create(
            "agy-to-local",
            "WORK_BOTH QUOTA_FAIL_gemini-4-argon-medium Update both files".into(),
            "Both files pass".into(),
        )
        .unwrap();
    let approved = workflows
        .approve(
            &wf.id,
            wf.revision,
            &wf.assignment.digest().unwrap(),
            "fixture",
        )
        .unwrap();
    let outcome = workflows.run(&wf.id, approved.revision).unwrap();
    assert_eq!(outcome.status, AgentStatus::AwaitingReview);
    assert_eq!(outcome.steps.len(), 2);
    assert_eq!(outcome.steps[0].worker, "agy-impl");
    assert_eq!(outcome.steps[1].worker, "local-impl");
    assert_eq!(
        outcome.steps[1].result.as_ref().unwrap()["model"],
        json!("gemma4:26b")
    );
}

#[test]
fn verifier_timeout_records_step_result_and_escalates_blocked_agentlet_cleanly() {
    use clatterforge::{
        agentlet::{Capability, Definition},
        agents::{AgyAgentPolicy, ModelTier},
        workflow::builtin_router_config,
    };

    let unused_model = Model::new(vec![]);
    let f = fixture(&unused_model, 4);
    let mut presets = AgentPresets::open(&f.state).unwrap();
    let old = presets.get("worker-a").unwrap();
    let scope = vec!["a.txt".to_string(), "b.txt".to_string()];
    let impl_def = Definition {
        schema_version: 1,
        id: "implementer".into(),
        version: 1,
        purpose: "Implement".into(),
        instructions: "Edit approved files".into(),
        capability: Capability::EditApprovedFiles,
        completion_gate: false,
    };

    presets
        .create(
            "agy-timeout-base",
            f.repo.clone(),
            AgentBackend::Agy(
                AgyAgentPolicy::inspect(worker(), "gemini-4-neon-medium".into()).unwrap(),
            ),
            scope,
            old.verification_files.clone(),
            old.verify_executable.clone(),
            vec!["--verify-hang".into()],
            5000,
            250,
            None,
            "fixture",
        )
        .unwrap();
    presets
        .bind_agentlet("agy-timeout-base", "agy-impl", impl_def, "fixture")
        .unwrap();
    presets
        .set_routing_policy(
            "agy-impl",
            Some(ModelTier::Standard),
            vec!["gemini-4-neon-medium".into()],
        )
        .unwrap();

    let workflows = Workflows::open(&f.state).unwrap();
    workflows
        .create_profile(
            "verifier-timeout-profile".into(),
            builtin_router_config(&f.state),
            vec![("agy-impl".into(), "Implementer".into())],
            5,
            5000,
            "fixture".into(),
        )
        .unwrap();

    let mut workflows = Workflows::open(&f.state).unwrap();
    let wf = workflows
        .create(
            "verifier-timeout-profile",
            "BLOCKED Attempt change before verifier times out".into(),
            "Both files pass".into(),
        )
        .unwrap();
    let approved = workflows
        .approve(
            &wf.id,
            wf.revision,
            &wf.assignment.digest().unwrap(),
            "fixture",
        )
        .unwrap();
    let outcome = workflows.run(&wf.id, approved.revision).unwrap();
    assert_eq!(outcome.status, AgentStatus::NeedsWork);
    assert_eq!(outcome.phase, "attention");
    assert_eq!(outcome.steps.len(), 1);
    let step_result = outcome.steps[0]
        .result
        .as_ref()
        .expect("step result must be recorded even when verifier times out");
    assert_eq!(step_result["execution_outcome"], json!("succeeded"));
    assert_eq!(step_result["verification_outcome"], json!("timed_out"));
    assert_eq!(step_result["agentlet_result"]["outcome"], json!("blocked"));
    assert!(
        outcome
            .error
            .as_deref()
            .unwrap_or("")
            .contains("Needs operator attention"),
        "expected escalation reason to include blocked summary, got: {:?}",
        outcome.error
    );
}

#[test]
fn clear_tasks_removes_failed_workflow_and_children_while_preserving_pending_and_dependents() {
    use clatterforge::{
        agentlet::{Capability, Definition},
        agents::{AgyAgentPolicy, ModelTier},
        service::{ClearScope, TaskService},
        workflow::builtin_router_config,
    };

    let unused_model = Model::new(vec![]);
    let f = fixture(&unused_model, 4);
    let mut presets = AgentPresets::open(&f.state).unwrap();
    let impl_def = Definition {
        schema_version: 1,
        id: "implementer".into(),
        version: 1,
        purpose: "Implement".into(),
        instructions: "Edit approved files".into(),
        capability: Capability::EditApprovedFiles,
        completion_gate: false,
    };
    let old = presets.get("worker-a").unwrap();
    presets
        .create(
            "agy-clear-base",
            f.repo.clone(),
            AgentBackend::Agy(
                AgyAgentPolicy::inspect(worker(), "gemini-4-neon-medium".into()).unwrap(),
            ),
            vec!["a.txt".into(), "b.txt".into()],
            old.verification_files.clone(),
            old.verify_executable.clone(),
            vec!["--verify-hang".into()],
            5000,
            250,
            None,
            "fixture",
        )
        .unwrap();
    presets
        .bind_agentlet("agy-clear-base", "agy-impl", impl_def, "fixture")
        .unwrap();
    presets
        .set_routing_policy(
            "agy-impl",
            Some(ModelTier::Standard),
            vec!["gemini-4-neon-medium".into()],
        )
        .unwrap();

    let workflows = Workflows::open(&f.state).unwrap();
    workflows
        .create_profile(
            "clear-test-profile".into(),
            builtin_router_config(&f.state),
            vec![("agy-impl".into(), "Implementer".into())],
            5,
            5000,
            "fixture".into(),
        )
        .unwrap();

    let mut workflows = Workflows::open(&f.state).unwrap();
    let failed_wf = workflows
        .create(
            "clear-test-profile",
            "BLOCKED Fail verifier timeout cleanly".into(),
            "Tests pass".into(),
        )
        .unwrap();
    let approved = workflows
        .approve(
            &failed_wf.id,
            failed_wf.revision,
            &failed_wf.assignment.digest().unwrap(),
            "fixture",
        )
        .unwrap();
    let failed_wf = workflows.run(&failed_wf.id, approved.revision).unwrap();
    assert_eq!(failed_wf.status, AgentStatus::NeedsWork);
    assert_eq!(failed_wf.steps.len(), 1);
    let failed_child_id = failed_wf.steps[0].task_id.clone();

    let pending_wf = workflows
        .create(
            "clear-test-profile",
            "Keep awaiting approval workflow".into(),
            "Tests pass".into(),
        )
        .unwrap();

    // Also create cancelled workflows (one depended on by pending_wf so surviving
    // dependency protection skips deleting it, plus an unreferenced cancelled workflow).
    let blocker_cancelled = workflows
        .create(
            "clear-test-profile",
            "cancelled dependency target".into(),
            "Tests pass".into(),
        )
        .unwrap();
    let free_cancelled = workflows
        .create(
            "clear-test-profile",
            "cancelled standalone workflow".into(),
            "Tests pass".into(),
        )
        .unwrap();
    drop(workflows);

    let mut service = TaskService::open(&f.state).unwrap();
    service
        .cancel(
            &blocker_cancelled.id,
            blocker_cancelled.revision,
            Some("fixture"),
        )
        .unwrap();
    service
        .cancel(&free_cancelled.id, free_cancelled.revision, Some("fixture"))
        .unwrap();
    service
        .add_dependency(&pending_wf.id, &blocker_cancelled.id)
        .unwrap();

    // Deleting a child AgentTask of a surviving/existing workflow directly is refused.
    let err = service.delete_task(&failed_child_id).unwrap_err();
    assert!(
        err.to_string().contains("belongs to workflow"),
        "expected child-of-workflow protection, got: {err:#}"
    );

    let outcome = service.clear_tasks(ClearScope::Failed).unwrap();
    assert!(outcome.deleted_ids.contains(&failed_wf.id));
    assert!(outcome.deleted_ids.contains(&failed_child_id));
    assert!(outcome.deleted_ids.contains(&free_cancelled.id));
    assert!(!outcome.deleted_ids.contains(&blocker_cancelled.id));
    assert!(!outcome.deleted_ids.contains(&pending_wf.id));
    assert!(
        outcome
            .skipped
            .iter()
            .any(|s| s.id == blocker_cancelled.id && s.reason.contains("depends on")),
        "expected dependency skip reason, got: {:?}",
        outcome.skipped
    );

    assert!(
        !f.state
            .join("workflow-artifacts")
            .join(&failed_wf.id)
            .exists()
    );
    assert!(
        !f.state
            .join("agent-artifacts")
            .join(&failed_child_id)
            .exists()
    );

    let remaining: std::collections::HashSet<String> =
        service.board().unwrap().into_iter().map(|r| r.id).collect();
    assert!(remaining.contains(&pending_wf.id));
    assert!(remaining.contains(&blocker_cancelled.id));
    assert!(!remaining.contains(&failed_wf.id));
    assert!(!remaining.contains(&failed_child_id));
    assert!(!remaining.contains(&free_cancelled.id));
}

#[test]
fn workflow_create_with_scope_overrides_edit_workers_and_keeps_reviewer_readonly() {
    use clatterforge::agentlet::{Capability, Definition};
    use clatterforge::workflow::builtin_router_config;

    let unused_model = Model::new(vec![]);
    let f = fixture(&unused_model, 4);
    let mut presets = AgentPresets::open(&f.state).unwrap();
    let old = presets.get("worker-a").unwrap();
    let impl_def = Definition {
        schema_version: 1,
        id: "implementer".into(),
        version: 1,
        purpose: "Implement".into(),
        instructions: "Edit approved files".into(),
        capability: Capability::EditApprovedFiles,
        completion_gate: false,
    };
    let rev_def = Definition {
        schema_version: 1,
        id: "reviewer".into(),
        version: 1,
        purpose: "Review".into(),
        instructions: "Inspect diff".into(),
        capability: Capability::Review,
        completion_gate: true,
    };
    presets
        .bind_agentlet("worker-a", "impl-scoped", impl_def, "fixture")
        .unwrap();
    presets
        .create(
            "rev-base",
            f.repo.clone(),
            old.backend.clone(),
            vec![],
            old.verification_files.clone(),
            old.verify_executable.clone(),
            old.verify_args.clone(),
            5000,
            5000,
            None,
            "fixture",
        )
        .unwrap();
    presets
        .bind_agentlet("rev-base", "rev-scoped", rev_def, "fixture")
        .unwrap();

    let mut workflows = Workflows::open(&f.state).unwrap();
    workflows
        .create_profile(
            "scoped-profile".into(),
            builtin_router_config(&f.state),
            vec![
                ("impl-scoped".into(), "Implement".into()),
                ("rev-scoped".into(), "Review".into()),
            ],
            4,
            5000,
            "fixture".into(),
        )
        .unwrap();

    let wf = workflows
        .create_with_scope(
            "scoped-profile",
            "Edit only b.txt".into(),
            "Tests pass".into(),
            Some(vec![" b.txt ".into(), "b.txt".into()]),
        )
        .unwrap();
    assert_eq!(
        wf.assignment.profile.workers[0].preset.allowed_files,
        vec!["b.txt"]
    );
    assert!(
        wf.assignment.profile.workers[1]
            .preset
            .allowed_files
            .is_empty()
    );

    let overlap = workflows.create_with_scope(
        "scoped-profile",
        "Try to edit protected verifier file".into(),
        "Tests pass".into(),
        Some(vec!["contract.txt".into()]),
    );
    assert!(overlap.is_err());
}

#[test]
fn execution_timeout_records_step_result_resets_workspace_and_fails_over_cleanly() {
    use clatterforge::{
        agentlet::{Capability, Definition},
        agents::{AgyAgentPolicy, ModelTier},
        codex::{CodexPolicy, Executable, PinnedCommand},
        workflow::builtin_router_config,
    };

    let unused_model = Model::new(vec![]);
    let f = fixture(&unused_model, 4);
    let mut presets = AgentPresets::open(&f.state).unwrap();
    let old = presets.get("worker-a").unwrap();
    let scope = vec!["a.txt".to_string(), "b.txt".to_string()];
    let impl_def = Definition {
        schema_version: 1,
        id: "implementer".into(),
        version: 1,
        purpose: "Implement".into(),
        instructions: "Edit approved files".into(),
        capability: Capability::EditApprovedFiles,
        completion_gate: false,
    };
    let codex_home = f._dir.path().join("exec-timeout-codex");
    fs::create_dir_all(&codex_home).unwrap();
    let verifier_cmd = PinnedCommand {
        executable: Executable::new(&old.verify_executable).unwrap(),
        args: old.verify_args.clone(),
    };

    presets
        .create(
            "agy-hang-base",
            f.repo.clone(),
            AgentBackend::Agy(
                AgyAgentPolicy::inspect(worker(), "gemini-4-neon-medium".into()).unwrap(),
            ),
            scope.clone(),
            old.verification_files.clone(),
            old.verify_executable.clone(),
            old.verify_args.clone(),
            250,
            5000,
            None,
            "fixture",
        )
        .unwrap();
    presets
        .bind_agentlet("agy-hang-base", "agy-impl", impl_def.clone(), "fixture")
        .unwrap();
    presets
        .set_routing_policy(
            "agy-impl",
            Some(ModelTier::Standard),
            vec!["gemini-4-neon-medium".into()],
        )
        .unwrap();

    presets
        .create(
            "codex-failover-base",
            f.repo.clone(),
            AgentBackend::Codex(
                CodexPolicy::inspect(worker(), &codex_home, "gpt-5".into(), verifier_cmd).unwrap(),
            ),
            scope,
            old.verification_files.clone(),
            old.verify_executable.clone(),
            old.verify_args.clone(),
            5000,
            5000,
            None,
            "fixture",
        )
        .unwrap();
    presets
        .bind_agentlet("codex-failover-base", "codex-impl", impl_def, "fixture")
        .unwrap();
    presets
        .set_routing_policy(
            "codex-impl",
            Some(ModelTier::Frontier),
            vec!["gpt-5".into()],
        )
        .unwrap();

    let workflows = Workflows::open(&f.state).unwrap();
    workflows
        .create_profile(
            "exec-timeout-profile".into(),
            builtin_router_config(&f.state),
            vec![
                ("agy-impl".into(), "Primary Implementer".into()),
                ("codex-impl".into(), "Escalation Implementer".into()),
            ],
            4,
            5000,
            "fixture".into(),
        )
        .unwrap();

    let mut workflows = Workflows::open(&f.state).unwrap();
    let wf = workflows
        .create(
            "exec-timeout-profile",
            "HANG_DIRTY_gemini-4-neon-medium WORK_BOTH Fix both a.txt and b.txt".into(),
            "Both files pass".into(),
        )
        .unwrap();
    let approved = workflows
        .approve(
            &wf.id,
            wf.revision,
            &wf.assignment.digest().unwrap(),
            "fixture",
        )
        .unwrap();

    let outcome = workflows.run(&wf.id, approved.revision).unwrap();
    assert_eq!(outcome.status, AgentStatus::AwaitingReview);
    assert_eq!(outcome.steps.len(), 2);
    assert_eq!(outcome.steps[0].worker, "agy-impl");
    assert_eq!(
        outcome.steps[0].result.as_ref().unwrap()["execution_failed"],
        json!(true)
    );
    assert_eq!(
        outcome.steps[0].result.as_ref().unwrap()["verification_outcome"],
        json!("skipped")
    );
    assert_eq!(outcome.steps[1].worker, "codex-impl");
    assert_eq!(
        outcome.steps[1].result.as_ref().unwrap()["verification_outcome"],
        json!("succeeded")
    );
}

#[test]
fn update_profile_timeouts_and_dispatch_timeout_overrides_work_live() {
    use clatterforge::workflow::TimeoutOverrides;

    let model = Model::new(vec![]);
    let f = fixture(&model, 4);
    let mut workflows = Workflows::open(&f.state).unwrap();

    // Invalid bounds (idle > hard limit) are rejected cleanly without mutating the profile.
    let invalid = workflows.update_profile_timeouts(
        "fixture",
        TimeoutOverrides {
            generation_timeout_ms: Some(10_000),
            generation_idle_timeout_ms: Some(20_000),
            verification_timeout_ms: Some(10_000),
            soft_timeouts: None,
            ..Default::default()
        },
    );
    assert!(invalid.is_err());

    // Live profile timeout adjustment persists across both workflow_profiles and agent_presets.
    let updated = workflows
        .update_profile_timeouts(
            "fixture",
            TimeoutOverrides {
                generation_timeout_ms: Some(450_000),
                generation_idle_timeout_ms: Some(240_000),
                verification_timeout_ms: Some(120_000),
                soft_timeouts: None,
                ..Default::default()
            },
        )
        .unwrap();
    for w in &updated.workers {
        assert_eq!(w.preset.generation_timeout_ms, 450_000);
        assert_eq!(w.preset.generation_idle_timeout_ms, Some(240_000));
        assert_eq!(w.preset.verification_timeout_ms, 120_000);
        let stored_preset = AgentPresets::open(&f.state)
            .unwrap()
            .get(&w.preset.name)
            .unwrap();
        assert_eq!(stored_preset.generation_timeout_ms, 450_000);
        assert_eq!(stored_preset.generation_idle_timeout_ms, Some(240_000));
        assert_eq!(stored_preset.verification_timeout_ms, 120_000);
    }

    // Per-workflow timeout overrides adjust the created workflow without overwriting profile defaults.
    let wf = workflows
        .create_with_overrides(
            "fixture",
            "Per-dispatch timeout override".into(),
            "Tests pass".into(),
            None,
            TimeoutOverrides {
                generation_timeout_ms: Some(900_000),
                generation_idle_timeout_ms: Some(600_000),
                verification_timeout_ms: Some(300_000),
                soft_timeouts: None,
                ..Default::default()
            },
        )
        .unwrap();
    for w in &wf.assignment.profile.workers {
        assert_eq!(w.preset.generation_timeout_ms, 900_000);
        assert_eq!(w.preset.generation_idle_timeout_ms, Some(600_000));
        assert_eq!(w.preset.verification_timeout_ms, 300_000);
    }
    let profile_after = workflows.profile("fixture").unwrap();
    assert_eq!(
        profile_after.workers[0].preset.generation_timeout_ms,
        450_000
    );

    // Alert-only (`soft_timeouts: true`) permits targets beyond the legacy 600s/1800s hard caps.
    let soft_updated = workflows
        .update_profile_timeouts(
            "fixture",
            TimeoutOverrides {
                generation_timeout_ms: Some(3_600_000),
                generation_idle_timeout_ms: Some(900_000),
                verification_timeout_ms: Some(1_200_000),
                soft_timeouts: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(soft_updated.workers[0].preset.soft_timeouts);
    assert_eq!(
        soft_updated.workers[0].preset.verification_timeout_ms,
        1_200_000
    );
}

#[test]
fn board_nests_workflow_step_tasks_under_single_parent_row() {
    use clatterforge::service::TaskService;

    let model = Model::new(vec![
        dispatch("worker-a", "WORK_A Implement step one"),
        dispatch("worker-b", "WORK_B Implement step two"),
        json!({
            "action": "finish",
            "summary": "All steps done"
        }),
    ]);
    let f = fixture(&model, 4);

    let mut workflows = Workflows::open(&f.state).unwrap();
    let outcome = workflows.run(&f.workflow.id, f.workflow.revision).unwrap();
    assert_eq!(outcome.status, AgentStatus::AwaitingReview);
    assert_eq!(outcome.steps.len(), 2);

    let service = TaskService::open(&f.state).unwrap();
    let board = service.board().unwrap();
    assert_eq!(
        board.len(),
        1,
        "workflow with 2 steps must produce 1 top-level row in board(), got: {board:?}"
    );
    let parent = &board[0];
    assert_eq!(parent.id, f.workflow.id);
    assert_eq!(parent.kind, "workflow");
    assert_eq!(parent.children.len(), 2);
    assert_eq!(parent.children[0].id, outcome.steps[0].task_id);
    assert_eq!(
        parent.children[0].step_label.as_deref(),
        Some("S01 · worker-a")
    );
    assert_eq!(parent.children[0].status, "needs_work");
    assert_eq!(parent.children[1].id, outcome.steps[1].task_id);
    assert_eq!(
        parent.children[1].step_label.as_deref(),
        Some("S02 · worker-b")
    );
    assert_eq!(parent.children[1].status, "completed");

    let child_detail = service.detail(&outcome.steps[0].task_id, None).unwrap();
    assert_eq!(
        child_detail.parent_workflow_id.as_deref(),
        Some(f.workflow.id.as_str())
    );
    let parent_detail = service.detail(&f.workflow.id, None).unwrap();
    assert_eq!(parent_detail.parent_workflow_id, None);

    // Rewinding from step 1 archives prior steps; archived child tasks must not leak to top level.
    let mut service = TaskService::open(&f.state).unwrap();
    let evidence_digest = parent_detail
        .evidence_digest
        .expect("awaiting_review workflow should carry evidence_digest");
    let rejected = service
        .review(
            &f.workflow.id,
            outcome.revision,
            &evidence_digest,
            "operator",
            false,
        )
        .unwrap();
    let retried = service
        .retry(
            &f.workflow.id,
            rejected.revision,
            "operator",
            false,
            Some(1),
            None,
            None,
        )
        .unwrap();
    assert_eq!(retried.status, "approved");
    let board_after_retry = service.board().unwrap();
    assert_eq!(board_after_retry.len(), 1);
    assert_eq!(board_after_retry[0].id, f.workflow.id);
    assert!(board_after_retry[0].children.is_empty());

    // Deleting the workflow must also remove archived child tasks cleanly.
    let cancelled = service
        .cancel(&f.workflow.id, retried.revision, None)
        .unwrap();
    assert_eq!(cancelled.status, "cancelled");
    service.delete_task(&f.workflow.id).unwrap();
    assert!(service.board().unwrap().is_empty());
}

#[test]
fn shared_worktree_workflow_integration_ignores_untouched_oversized_scope_files() {
    use clatterforge::agentlet::{Capability, Definition};
    let model = Model::new(vec![
        dispatch("implement-a", "WORK_A"),
        dispatch("implement-b", "WORK_B"),
        dispatch("review-final", "REVIEW_OK"),
        json!({"action":"finish","summary":"Verified candidate with untouched large scoped file"}),
    ]);
    let (_dir, dir_root) = canonical_tempdir();
    let repo = dir_root.as_path().join("repo");
    fs::create_dir(&repo).unwrap();
    for (name, contents) in [
        ("README.md", "fixture"),
        ("a.txt", "wrong-a"),
        ("b.txt", "wrong-b"),
        (
            "contract.txt",
            "Both files must be fixed. Protected contract.",
        ),
    ] {
        fs::write(repo.join(name), contents).unwrap();
    }
    // Untouched file > 4 MiB (exceeding even the integration text limit if needlessly read)
    // and well above the old 64 KiB cap in the shared workflow's scope.
    let untouched_large = vec![b'u'; 4 * 1024 * 1024 + 128];
    fs::write(repo.join("untouched_large.rs"), &untouched_large).unwrap();
    fs::write(repo.join(".gitattributes"), "*.txt text eol=lf\n").unwrap();
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
            "fixture with large untouched scoped file",
        ],
    )
    .unwrap();

    let state = dir_root.as_path().join("state");
    let mut presets = AgentPresets::open(&state).unwrap();
    let policy = AgentBackend::CursorAgent(
        CursorAgentPolicy::inspect(
            worker(),
            &worker().parent().unwrap().join("index.js"),
            "fixture".into(),
        )
        .unwrap(),
    );
    let verifier = dir_root.as_path().join("verifier.exe");
    fs::copy(worker(), &verifier).unwrap();
    for (name, allowed) in [
        ("worker-a", vec!["a.txt".to_string()]),
        (
            "worker-b",
            vec!["b.txt".to_string(), "untouched_large.rs".to_string()],
        ),
    ] {
        presets
            .create(
                name,
                repo.clone(),
                policy.clone(),
                allowed,
                vec!["contract.txt".into()],
                verifier.clone(),
                vec!["--verify".into()],
                5000,
                5000,
                None,
                "fixture",
            )
            .unwrap();
    }
    let config = Config {
        schema_version: 1,
        default_target: "planner".into(),
        hardware_id: "fixture".into(),
        admission_db: dir_root.as_path().join("admission.sqlite"),
        max_active_per_resource: 1,
        registry_max_age_ms: 60000,
        fallback_before_dispatch: false,
        targets: vec![Target {
            id: "planner".into(),
            provider: ProviderKind::Ollama,
            endpoint: model.url.clone(),
            endpoint_access: EndpointAccess::Loopback,
            model: "gemma4:fixture".into(),
            expected_artifact_digest: Some("a".repeat(64)),
            context_limit: 262144,
            output_limit: 4096,
            capabilities: Capabilities {
                streaming: true,
                reasoning_control: true,
                ..Default::default()
            },
            capability_source: "fixture".into(),
            priority: 0,
            resource_id: "fixture-gpu".into(),
            extensions: None,
            api_pricing: None,
        }],
    };
    let mut workflows = Workflows::open(&state).unwrap();

    for definition in [
        Definition {
            schema_version: 1,
            id: "implementer".into(),
            version: 1,
            purpose: "Implement only the assigned component".into(),
            instructions: "Respect approved scope".into(),
            capability: Capability::EditApprovedFiles,
            completion_gate: false,
        },
        Definition {
            schema_version: 1,
            id: "reviewer".into(),
            version: 1,
            purpose: "Review the final candidate".into(),
            instructions: "Inspect candidate without editing".into(),
            capability: Capability::Review,
            completion_gate: true,
        },
    ] {
        let path = state.join(format!("{}.json", definition.id));
        fs::write(&path, serde_json::to_vec(&definition).unwrap()).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_clatterforge"))
            .arg("--state")
            .arg(&state)
            .args(["agentlet-import", "--definition"])
            .arg(path)
            .args(["--by", "fixture operator"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    for (id, source, name) in [
        ("implementer", "worker-a", "implement-a"),
        ("implementer", "worker-b", "implement-b"),
        ("reviewer", "worker-a", "review-final"),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_clatterforge"))
            .arg("--state")
            .arg(&state)
            .args([
                "agentlet-bind",
                id,
                "--version",
                "1",
                "--preset",
                source,
                "--name",
                name,
                "--by",
                "fixture operator",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    workflows
        .create_profile(
            "agentlets".into(),
            config,
            vec![
                ("implement-a".into(), "Fix a.txt".into()),
                ("implement-b".into(), "Fix b.txt".into()),
                ("review-final".into(), "Review final candidate".into()),
            ],
            4,
            5000,
            "fixture operator".into(),
        )
        .unwrap();
    let w = workflows
        .create(
            "agentlets",
            "Fix a and b without touching large scoped file".into(),
            "Protected verifier passes and final review succeeds".into(),
        )
        .unwrap();
    let approved = workflows
        .approve(&w.id, w.revision, &w.assignment.digest().unwrap(), "human")
        .unwrap();
    let done = workflows.run(&approved.id, approved.revision).unwrap();
    assert_eq!(done.status, AgentStatus::AwaitingReview);
    assert!(
        done.assignment
            .profile
            .scope()
            .contains(&"untouched_large.rs".to_string())
    );
    assert!(done.steps.last().unwrap().checkpoint.is_some());

    let mut service = TaskService::open(&state).unwrap();
    let detail = service.detail(&done.id, None).unwrap();
    service
        .review(
            &done.id,
            detail.revision,
            detail.evidence_digest.as_ref().unwrap(),
            "human",
            true,
        )
        .unwrap();

    let prepared = service.prepare_integration(&done.id, "human").unwrap();
    let plan = prepared.integration.unwrap();
    let paths: Vec<_> = plan.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, vec!["a.txt", "b.txt"]);

    let applied = service
        .integrate(&done.id, plan.revision, &plan.plan_digest, "human", false)
        .unwrap()
        .integration
        .unwrap();
    assert_eq!(applied.status, "applied");
    assert_eq!(fs::read_to_string(repo.join("a.txt")).unwrap(), "fixed-a");
    assert_eq!(fs::read_to_string(repo.join("b.txt")).unwrap(), "fixed-b");
    assert_eq!(
        fs::read(repo.join("untouched_large.rs")).unwrap().len(),
        untouched_large.len()
    );

    let rolled = service
        .integrate(
            &done.id,
            applied.revision,
            &applied.plan_digest,
            "human",
            true,
        )
        .unwrap()
        .integration
        .unwrap();
    assert_eq!(rolled.status, "rolled_back");
    assert_eq!(fs::read_to_string(repo.join("a.txt")).unwrap(), "wrong-a");
    assert_eq!(fs::read_to_string(repo.join("b.txt")).unwrap(), "wrong-b");
}

#[test]
fn require_clean_checkout_blocks_create_approve_and_run_while_dirty() {
    use clatterforge::workflow::TimeoutOverrides;

    let model = Model::new(vec![
        dispatch("worker-a", "WORK_A"),
        dispatch("worker-b", "WORK_B"),
        json!({"action":"finish","summary":"Verified candidate on clean checkout"}),
    ]);
    let f = fixture(&model, 3);
    let mut workflows = Workflows::open(&f.state).unwrap();
    let updated = workflows
        .update_profile_timeouts(
            "fixture",
            TimeoutOverrides {
                require_clean_checkout: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(updated.require_clean_checkout);

    // Dirty checkout blocks create immediately.
    fs::write(f.repo.join("wip.txt"), "uncommitted wip").unwrap();
    let err = workflows
        .create("fixture", "Fix both".into(), "Verifier passes".into())
        .unwrap_err()
        .to_string();
    assert!(err.contains("clean checkout required"), "{err}");

    // Clean -> create succeeds; dirty again -> approve blocks.
    fs::remove_file(f.repo.join("wip.txt")).unwrap();
    let created = workflows
        .create("fixture", "Fix both".into(), "Verifier passes".into())
        .unwrap();
    let digest = created.assignment.digest().unwrap();
    fs::write(f.repo.join("wip.txt"), "uncommitted wip").unwrap();
    let err = workflows
        .approve(&created.id, created.revision, &digest, "human")
        .unwrap_err()
        .to_string();
    assert!(err.contains("clean checkout required"), "{err}");

    // Clean -> approve succeeds; dirty again -> run blocks.
    fs::remove_file(f.repo.join("wip.txt")).unwrap();
    let approved = workflows
        .approve(&created.id, created.revision, &digest, "human")
        .unwrap();
    fs::write(f.repo.join("wip.txt"), "uncommitted wip").unwrap();
    let err = workflows
        .run(&approved.id, approved.revision)
        .unwrap_err()
        .to_string();
    assert!(err.contains("clean checkout required"), "{err}");

    // Clean -> run proceeds to awaiting_review.
    fs::remove_file(f.repo.join("wip.txt")).unwrap();
    let done = workflows.run(&approved.id, approved.revision).unwrap();
    assert_eq!(done.status, AgentStatus::AwaitingReview);
}

#[test]
fn dirty_checkout_integrates_into_isolated_worktree_and_rolls_back() {
    use clatterforge::integration::IntegrationTarget;

    let model = Model::new(vec![
        dispatch("worker-a", "WORK_A"),
        dispatch("worker-b", "WORK_B"),
        json!({"action":"finish","summary":"Verified candidate"}),
    ]);
    let f = fixture(&model, 3);
    let done = run(&f);
    let mut service = TaskService::open(&f.state).unwrap();
    let detail = service.detail(&done.id, None).unwrap();
    service
        .review(
            &done.id,
            detail.revision,
            detail.evidence_digest.as_ref().unwrap(),
            "human",
            true,
        )
        .unwrap();

    // Main checkout has unrelated dirty WIP.
    fs::write(f.repo.join("wip.txt"), "preserve main wip").unwrap();
    assert!(
        service
            .prepare_integration_with_target(&done.id, "human", IntegrationTarget::Checkout)
            .is_err()
    );

    // Worktree integration succeeds without touching main checkout WIP.
    let prepared = service
        .prepare_integration_with_target(&done.id, "human", IntegrationTarget::Worktree)
        .unwrap();
    let plan = prepared.integration.unwrap();
    assert_eq!(plan.status, "prepared");
    assert_eq!(plan.target, IntegrationTarget::Worktree);
    assert!(
        plan.worktree_branch
            .as_deref()
            .is_some_and(|b| b.starts_with("conductor/integrate-"))
    );
    assert_ne!(plan.repository.checkout, f.repo);

    let applied = service
        .integrate(&done.id, plan.revision, &plan.plan_digest, "human", false)
        .unwrap()
        .integration
        .unwrap();
    assert_eq!(applied.status, "applied");
    assert_eq!(
        fs::read_to_string(plan.repository.checkout.join("a.txt")).unwrap(),
        "fixed-a"
    );
    assert_eq!(
        fs::read_to_string(plan.repository.checkout.join("b.txt")).unwrap(),
        "fixed-b"
    );
    assert_eq!(fs::read_to_string(f.repo.join("a.txt")).unwrap(), "wrong-a");
    assert_eq!(
        fs::read_to_string(f.repo.join("wip.txt")).unwrap(),
        "preserve main wip"
    );

    let rolled = service
        .integrate(
            &done.id,
            applied.revision,
            &applied.plan_digest,
            "human",
            true,
        )
        .unwrap()
        .integration
        .unwrap();
    assert_eq!(rolled.status, "rolled_back");
    assert_eq!(
        fs::read_to_string(plan.repository.checkout.join("a.txt")).unwrap(),
        "wrong-a"
    );
    assert_eq!(
        fs::read_to_string(plan.repository.checkout.join("b.txt")).unwrap(),
        "wrong-b"
    );
}

#[test]
fn rejected_review_comments_are_anchored_and_handed_to_the_repair_round() {
    use clatterforge::store::ReviewComment;
    let model = Model::new(vec![
        dispatch("worker-a", "WORK_A"),
        dispatch("worker-b", "WORK_B"),
        json!({"action":"finish","summary":"done"}),
        dispatch("worker-b", "WORK_B"),
        json!({"action":"finish","summary":"repaired"}),
    ]);
    let f = fixture(&model, 3);
    let done = run(&f);
    assert_eq!(done.status, AgentStatus::AwaitingReview, "{done:?}");
    let digest = done.evidence_digest().unwrap();
    let comment = |path: &str, line: u32, body: &str| ReviewComment {
        path: path.into(),
        line,
        end_line: None,
        body: body.into(),
        excerpt: "caller-supplied excerpt is ignored".into(),
    };
    let mut workflows = Workflows::open(&f.state).unwrap();

    for (bad, why) in [
        (
            comment("README.md", 1, "not part of the diff"),
            "unchanged file",
        ),
        (comment("a.txt", 99, "past the end"), "missing line"),
        (comment("a.txt", 1, "   "), "empty body"),
        (comment("a.txt", 0, "zero line"), "line zero"),
    ] {
        assert!(
            workflows
                .review_with_comments(&done.id, done.revision, &digest, "human", false, vec![bad])
                .is_err(),
            "accepted a comment with {why}"
        );
    }
    assert_eq!(
        workflows.get(&done.id).unwrap().status,
        AgentStatus::AwaitingReview,
        "a refused comment records nothing"
    );

    let rejected = workflows
        .review_with_comments(
            &done.id,
            done.revision,
            &digest,
            "human",
            false,
            vec![comment(
                "a.txt",
                1,
                "Keep the canonical value but explain it",
            )],
        )
        .unwrap();
    assert_eq!(rejected.status, AgentStatus::Rejected);
    let recorded = &rejected.review.as_ref().unwrap().comments;
    assert_eq!(recorded.len(), 1);
    assert_eq!(
        recorded[0].excerpt, "fixed-a",
        "the host captures the candidate line"
    );

    let retried = workflows
        .retry(
            &done.id,
            rejected.revision,
            "human",
            false,
            None,
            None,
            None,
        )
        .unwrap();
    assert_eq!(retried.resume_review_comments, *recorded);

    let seen = model.requests.lock().unwrap().len();
    let repaired = workflows.run(&done.id, retried.revision).unwrap();
    let requests = model.requests.lock().unwrap();
    assert!(
        requests[seen..].iter().any(|r| r
            .to_string()
            .contains("Keep the canonical value but explain it")),
        "the planner receives the review comments"
    );
    let stage = repaired.steps.last().unwrap();
    let objective = Agents::open(&f.state)
        .unwrap()
        .task(&stage.task_id)
        .unwrap()
        .assignment
        .spec
        .objective;
    assert!(
        objective.contains("Reviewer comments on the rejected candidate")
            && objective.contains("- a.txt:1: Keep the canonical value but explain it")
            && objective.contains("> fixed-a"),
        "the repair stage objective carries the anchored comments: {objective}"
    );
}
