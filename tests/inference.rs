use clatterforge::{
    digest,
    execution::{CodexEngine, ExecutionEngine},
    inference::{config::*, jobs, provider::Decoder, state::*, types::*},
    now,
    process::current_identity,
};
use serde_json::{Value, json};
use std::{
    io::Read,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;

struct Server {
    url: String,
    requests: Arc<Mutex<Vec<(String, Value)>>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Server {
    fn start() -> Self {
        let http = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let address = http.server_addr().to_ip().unwrap();
        let requests = Arc::new(Mutex::new(vec![]));
        let stop = Arc::new(AtomicBool::new(false));
        let copy = requests.clone();
        let done = stop.clone();
        let worker = thread::spawn(move || {
            while !done.load(Ordering::SeqCst) {
                let Some(mut request) = http.recv_timeout(Duration::from_millis(10)).unwrap()
                else {
                    continue;
                };
                let path = request.url().to_string();
                let mut bytes = vec![];
                request
                    .as_reader()
                    .take(1048576)
                    .read_to_end(&mut bytes)
                    .unwrap();
                let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
                copy.lock().unwrap().push((path.clone(), body.clone()));
                let (status, response, slow) = reply(&path, &body);
                let status = tiny_http::StatusCode(
                    status.split_whitespace().next().unwrap().parse().unwrap(),
                );
                if slow {
                    let split = response.find('\n').unwrap() + 1;
                    let padded =
                        response[..split].to_string() + &"\n".repeat(9000) + &response[split..];
                    let reader = SlowRead {
                        data: std::io::Cursor::new(padded.into_bytes()),
                        pause_after: split + 9000,
                        paused: false,
                    };
                    let response = tiny_http::Response::new(status, vec![], reader, None, None);
                    let _ = request.respond(response);
                } else {
                    let _ = request.respond(
                        tiny_http::Response::from_string(response).with_status_code(status),
                    );
                }
            }
        });
        Self {
            url: format!("http://{address}/"),
            requests,
            stop,
            worker: Some(worker),
        }
    }
    fn generations(&self) -> Vec<(String, Value)> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(p, _)| p.ends_with("chat") || p.ends_with("chat/completions"))
            .cloned()
            .collect()
    }
}
struct SlowRead {
    data: std::io::Cursor<Vec<u8>>,
    pause_after: usize,
    paused: bool,
}
impl Read for SlowRead {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        let position = self.data.position() as usize;
        if position >= self.pause_after && !self.paused {
            self.paused = true;
            thread::sleep(Duration::from_millis(350));
        }
        let count = if !self.paused {
            output.len().min(self.pause_after - position)
        } else {
            output.len()
        };
        self.data.read(&mut output[..count])
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.worker.take().unwrap().join().unwrap();
    }
}

fn reply(path: &str, body: &Value) -> (&'static str, String, bool) {
    if path.ends_with("api/tags") {
        return (
            "200 OK",
            json!({"models":[{"name":"fixture:model","digest":"a".repeat(64)}]}).to_string()
                + "\n\n",
            path.contains("slow-discovery"),
        );
    }
    if path.ends_with("api/version") {
        return ("200 OK", json!({"version":"fixture-v1"}).to_string(), false);
    }
    if path.ends_with("models") {
        return (
            "200 OK",
            json!({"data":[{"id":"fixture:model","digest":"a".repeat(64)}],"runtime_version":"fixture-v1"}).to_string(),
            false,
        );
    }
    let mode = body["messages"][0]["content"].as_str().unwrap_or("text");
    if mode.starts_with("usage-") {
        let first = json!({"choices":[{"index":0,"delta":{"content":"private fixture text"},"finish_reason":null}],"usage":{"prompt_tokens":9,"prompt_tokens_details":{"cached_tokens":2}}});
        let rest = if mode == "usage-failed" {
            "data: bad JSON\n\n".into()
        } else {
            format!(
                "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                json!({"choices":[],"usage":{"completion_tokens":4}}),
                json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})
            )
        };
        return (
            "200 OK",
            format!("data: {first}\n\n{rest}"),
            mode == "usage-slow",
        );
    }

    if mode == "http-error" {
        return ("429 Too Many Requests", "{}".into(), false);
    }
    if mode == "context-error" {
        return ("400 Bad Request", json!({"error":{"code":"context_length_exceeded","message":"private prompt should never be echoed"}}).to_string(), false);
    }
    let tools = mode == "endless-tools"
        || (mode == "tools"
            && !body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["role"] == "tool"));
    let text = if mode == "tools" {
        "5"
    } else {
        "hÃ©ðŸ™‚ verified"
    };
    let native = path.ends_with("api/chat");
    let mut output = if native {
        if tools {
            format!(
                "{}\n{}\n",
                json!({"model":"fixture:model","message":{"content":"","tool_calls":[{"function":{"index":0,"name":"checked_add","arguments":{"a":2,"b":3}}}]},"done":false}),
                json!({"model":"fixture:model","message":{"content":""},"done":true,"done_reason":"stop","prompt_eval_count":9,"eval_count":4})
            )
        } else {
            format!(
                "{}\n{}\n",
                json!({"model":"fixture:model","message":{"content":if mode=="reasoning-only"{""}else if mode=="slow"{"partial"}else{text},"thinking":"private thought"},"done":false}),
                json!({"model":"fixture:model","message":{"content":""},"done":true,"done_reason":"stop","prompt_eval_count":9,"eval_count":4})
            )
        }
    } else {
        let mut values = vec![];
        if tools {
            values.push(json!({"model":"fixture:model","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"checked_add","arguments":"{\"a\":"}}]},"finish_reason":null}]}));
            values.push(json!({"model":"fixture:model","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"2,\"b\":3}"}}]},"finish_reason":"tool_calls"}]}));
        } else {
            values.push(json!({"model":"fixture:model","choices":[{"index":0,"delta":{"content":if mode=="reasoning-only"{""}else if mode=="slow"{"partial"}else{text},"reasoning_content":"private thought"},"finish_reason":null}]}));
            values.push(json!({"model":"fixture:model","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}));
        }
        values.push(json!({"choices":[],"usage":{"prompt_tokens":9,"completion_tokens":4}}));
        values
            .into_iter()
            .map(|v| format!("data: {v}\n\n"))
            .collect::<String>()
            + "data: [DONE]\n\n"
    };
    if mode == "malformed" {
        output = "bad JSON\n".into();
    }
    if mode == "truncated" {
        output = if native {
            format!(
                "{}\n",
                json!({"model":"fixture:model","message":{"content":"partial"},"done":false})
            )
        } else {
            format!(
                "data: {}\n\n",
                json!({"model":"fixture:model","choices":[{"index":0,"delta":{"content":"partial"},"finish_reason":null}]})
            )
        };
    }
    if mode == "oversized" {
        output = "x".repeat(256 * 1024 + 1) + "\n";
    }
    ("200 OK", output, mode == "slow")
}

fn config(temp: &TempDir, server: &Server) -> Config {
    Config {
        schema_version: 1,
        default_target: "native".into(),
        hardware_id: "fixture-hardware".into(),
        admission_db: temp.path().join("coordinator.sqlite"),
        max_active_per_resource: 1,
        registry_max_age_ms: 60000,
        fallback_before_dispatch: true,
        targets: vec![
            target("native", ProviderKind::Ollama, &server.url),
            target(
                "compatible",
                ProviderKind::OpenaiLocal,
                &format!("{}v1/", server.url),
            ),
        ],
    }
}
fn target(id: &str, provider: ProviderKind, url: &str) -> Target {
    Target {
        id: id.into(),
        provider,
        endpoint: url.into(),
        endpoint_access: EndpointAccess::Loopback,
        model: "fixture:model".into(),
        expected_artifact_digest: None,
        context_limit: 4096,
        output_limit: 512,
        capabilities: Capabilities {
            streaming: true,
            tools: true,
            required_tool_choice: provider == ProviderKind::OpenaiLocal,
            reasoning_control: provider == ProviderKind::Ollama,
            ..Default::default()
        },
        capability_source: "fixture advertisement".into(),
        priority: 1,
        resource_id: "gpu0".into(),
        extensions: None,
        api_pricing: None,
    }
}

#[test]
fn private_endpoints_require_explicit_literal_address_opt_in() {
    let mut value = target("mac", ProviderKind::Ollama, "http://100.127.254.10:11434/");
    assert!(value.validate().is_err());
    value.endpoint_access = EndpointAccess::PrivateIpv4;
    for endpoint in [
        "http://100.127.254.10:11434/",
        "http://100.127.255.254:11434/",
        "http://10.2.3.4:11434/",
        "http://172.16.0.1:11434/",
        "http://192.168.1.2:11434/",
        "http://127.0.0.1:11434/",
        "http://[::1]:11434/",
    ] {
        value.endpoint = endpoint.into();
        value.validate().unwrap();
    }
    for endpoint in [
        "http://example.com:11434/",
        "http://mac.local:11434/",
        "http://100.63.255.255:11434/",
        "http://100.128.0.1:11434/",
        "http://172.15.0.1:11434/",
        "http://172.32.0.1:11434/",
        "http://169.254.169.254:11434/",
        "http://8.8.8.8:11434/",
        "http://0.0.0.0:11434/",
        "http://[::ffff:100.127.254.10]:11434/",
        "http://user:secret@100.127.254.10:11434/",
        "http://100.127.254.10:11434/?key=x",
        "http://100.127.254.10:11434/#fragment",
        "http://100.127.254.10:11434/api",
        "https://100.127.254.10:11434/",
    ] {
        value.endpoint = endpoint.into();
        assert!(value.validate().is_err(), "accepted {endpoint}");
    }
}

#[test]
fn endpoint_opt_in_is_snapshot_bound_and_old_target_fingerprints_survive() {
    let value = target("native", ProviderKind::Ollama, "http://127.0.0.1:11434/");
    let original = serde_json::to_value(&value).unwrap();
    assert!(original.get("endpoint_access").is_none());
    let restored: Target = serde_json::from_value(original.clone()).unwrap();
    assert_eq!(restored.endpoint_access, EndpointAccess::Loopback);
    assert_eq!(value.fingerprint(), restored.fingerprint());
    let mut explicit = original.clone();
    explicit["endpoint_access"] = json!("loopback");
    let restored: Target = serde_json::from_value(explicit).unwrap();
    assert_eq!(value.fingerprint(), restored.fingerprint());
    let mut changed = original.clone();
    changed["endpoint_access"] = json!("private_ipv4");
    let restored: Target = serde_json::from_value(changed).unwrap();
    assert_ne!(value.fingerprint(), restored.fingerprint());
    let mut invalid = original;
    invalid["endpoint_access"] = json!("any");
    assert!(serde_json::from_value::<Target>(invalid).is_err());
}

fn request(mode: &str, tools: bool) -> Request {
    Request {
        schema_version: 1,
        messages: vec![Message::text(Role::User, mode)],
        tools: if tools {
            ToolPolicy::CheckedAdd
        } else {
            ToolPolicy::Disabled
        },
        require_tool: false,
        require_tested: false,
        max_output_tokens: 128,
        temperature: 0.0,
        stop: vec![],
        deadline_ms: 2000,
        reasoning: None,
    }
}

#[test]
fn larger_requests_use_selected_model_admission_instead_of_the_old_64k_cap() {
    let temp = tempfile::tempdir().unwrap();
    let server = Server::start();
    let mut cfg = config(&temp, &server);
    let mut input = request("text", false);
    input.messages = vec![Message::text(Role::User, "x".repeat(96000))];
    input.validate().unwrap();
    cfg.targets[0].context_limit = 131072;
    cfg.targets[0].eligible(&input, None).unwrap();
    cfg.targets[0].context_limit = 32768;
    assert_eq!(
        cfg.targets[0].eligible(&input, None).unwrap_err().kind,
        ErrorKind::ContextOverflow
    );
    input.messages = vec![Message::text(Role::User, "x".repeat(MAX_REQUEST_BYTES))];
    assert!(input.validate().is_err());
    assert!(server.generations().is_empty());
}
fn execute(store: &mut JobStore, job: &Job, target: Option<&str>, one_turn: bool) -> Job {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(jobs::run(
            &store.root,
            &job.id,
            job.revision,
            target,
            one_turn,
        ))
        .unwrap()
}

#[test]
fn telemetry_usage_survives_partial_reports_failure_and_cancellation() {
    use clatterforge::telemetry::{ApiPricing, job_summary};
    for mode in ["usage-complete", "usage-failed", "usage-slow"] {
        let temp = TempDir::new().unwrap();
        let server = Server::start();
        let mut cfg = config(&temp, &server);
        cfg.targets[1].api_pricing = Some(ApiPricing {
            currency: "USD".into(),
            input_micros_per_million: 2000000,
            output_micros_per_million: 4000000,
            cached_input_micros_per_million: Some(1000000),
        });
        let root = temp.path().join("state");
        let mut store = JobStore::open(&root).unwrap();
        let job = store
            .create(cfg, None, request(mode, false), Some("compatible".into()))
            .unwrap();
        let cancel = if mode == "usage-slow" {
            let root = root.clone();
            let id = job.id.clone();
            Some(thread::spawn(move || {
                let mut store = JobStore::open(&root).unwrap();
                let deadline = Instant::now() + Duration::from_secs(3);
                loop {
                    let current = store.job(&id).unwrap();
                    if current
                        .generations
                        .last()
                        .is_some_and(|g| g.usage.prompt_tokens == Some(9))
                        && store.cancel(&id, current.revision).is_ok()
                    {
                        break;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "usage was not persisted before cancellation"
                    );
                    thread::sleep(Duration::from_millis(3));
                }
            }))
        } else {
            None
        };
        let done = execute(&mut store, &job, None, false);
        if let Some(cancel) = cancel {
            cancel.join().unwrap();
        }
        let summary = job_summary(&done);
        assert_eq!(summary["tokens"]["input"]["total"], 9);
        assert_eq!(summary["tokens"]["cached_input"]["total"], 2);
        assert_eq!(
            summary,
            job_summary(&JobStore::open(&root).unwrap().job(&done.id).unwrap())
        );
        if mode == "usage-complete" {
            assert_eq!(done.status, JobStatus::Completed);
            assert_eq!(summary["tokens"]["output"]["total"], 4);
            assert_eq!(summary["estimated_api_cost"]["amount"], "0.000032");
        } else {
            assert_eq!(
                done.status,
                if mode == "usage-failed" {
                    JobStatus::Failed
                } else {
                    JobStatus::Cancelled
                }
            );
            assert!(summary["tokens"]["output"]["total"].is_null());
            assert!(summary["estimated_api_cost"].is_null());
        }
        assert!(!summary.to_string().contains("private fixture text"));
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_clatterforge"))
            .arg("--state")
            .arg(&root)
            .args(["inference-show", &done.id, "--summary"])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            serde_json::from_slice::<Value>(&output.stdout).unwrap(),
            summary
        );
    }
}

#[test]
fn both_adapters_execute_canonical_jobs_with_real_http_streams() {
    let temp = TempDir::new().unwrap();
    let server = Server::start();
    let cfg = config(&temp, &server);
    let mut store = JobStore::open(&temp.path().join("state")).unwrap();
    for target in ["native", "compatible"] {
        let job = store
            .create(
                cfg.clone(),
                None,
                request("text", false),
                Some(target.into()),
            )
            .unwrap();
        let done = execute(&mut store, &job, None, false);
        assert_eq!(done.status, JobStatus::Completed);
        assert_eq!(
            done.request.messages.last().unwrap().content(),
            "hÃ©ðŸ™‚ verified"
        );
        let generation = &done.generations[0];
        assert_eq!(generation.request_digest, done.initial_request_digest);
        assert_eq!(generation.terminal_events, 1);
        assert_eq!(generation.usage.prompt_tokens, Some(9));
        assert_eq!(generation.usage.cached_tokens, None);
        assert_eq!(generation.reasoning, "private thought");
        assert_eq!(generation.resolved.artifact_digest, Some("a".repeat(64)));
        assert!(generation.client_duration_ms.is_some());
        assert_eq!(
            store
                .events(&done.id)
                .unwrap()
                .iter()
                .filter(|e| e["event"]["type"] == "finish")
                .count(),
            1
        );
    }
    let requests = server.generations();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].1["options"]["num_predict"], 128);
    assert_eq!(requests[1].1["max_tokens"], 128);
    assert!(
        Coordinator::open(&cfg.admission_db)
            .unwrap()
            .reservations()
            .unwrap()
            .is_empty()
    );
    assert!(!CodexEngine.capabilities().resumable_inference);
}

#[test]
fn tool_calls_are_host_owned_and_resume_only_after_safe_checkpoint() {
    let temp = TempDir::new().unwrap();
    let server = Server::start();
    let cfg = config(&temp, &server);
    let mut store = JobStore::open(&temp.path().join("state")).unwrap();
    for target in ["native", "compatible"] {
        let job = store
            .create(
                cfg.clone(),
                None,
                request("tools", true),
                Some(target.into()),
            )
            .unwrap();
        let checkpoint = execute(&mut store, &job, None, true);
        assert_eq!(checkpoint.status, JobStatus::Checkpoint);
        assert!(checkpoint.owner_token.is_none());
        assert_eq!(checkpoint.request.messages.len(), 3);
        let call = &checkpoint.request.messages[1].tool_calls[0];
        let result = &checkpoint.request.messages[2];
        assert_eq!(result.tool_call_id.as_ref(), Some(&call.id));
        assert_eq!(result.content(), "5");
        let reopened = store.job(&job.id).unwrap();
        let done = execute(&mut store, &reopened, None, false);
        assert_eq!(done.status, JobStatus::Completed);
        assert_eq!(done.generations.len(), 2);
        assert_eq!(
            done.request
                .messages
                .iter()
                .filter(|m| m.role == Role::Tool)
                .count(),
            1
        );
        assert_eq!(done.request.messages.last().unwrap().content(), "5");
        assert!(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(jobs::run(&store.root, &done.id, done.revision, None, false))
                .is_err()
        );
    }
    let requests = server.generations();
    assert_eq!(requests.len(), 4);
    assert_eq!(requests[1].1["messages"][2]["tool_name"], "checked_add");
    assert_eq!(requests[3].1["messages"][2]["tool_call_id"], "call_1");
}

#[test]
fn unsupported_features_overflow_and_exact_overrides_fail_before_dispatch() {
    let temp = TempDir::new().unwrap();
    let server = Server::start();
    let cfg = config(&temp, &server);
    let mut store = JobStore::open(&temp.path().join("state")).unwrap();
    let mut required = request("tools", true);
    required.require_tool = true;
    let job = store
        .create(cfg.clone(), None, required, Some("native".into()))
        .unwrap();
    let failed = execute(&mut store, &job, None, false);
    assert_eq!(
        failed.failure.unwrap().kind,
        ErrorKind::UnsupportedCapability
    );
    assert!(server.generations().is_empty());
    let mut overflow = request("text", false);
    overflow.messages[0] = Message::text(Role::User, "a".repeat(4096));
    let job = store
        .create(cfg.clone(), None, overflow, Some("native".into()))
        .unwrap();
    let failed = execute(&mut store, &job, None, false);
    assert_eq!(failed.failure.unwrap().kind, ErrorKind::ContextOverflow);
    let mut media = request("text", false);
    media.messages[0].parts = vec![Part::Image {
        url: "file:///private".into(),
    }];
    assert!(store.create(cfg.clone(), None, media, None).is_err());
    let job = store.create(
        cfg.clone(),
        None,
        request("text", false),
        Some("missing".into()),
    );
    assert!(job.is_err());
    let mut tested = request("text", false);
    tested.require_tested = true;
    let job = store.create(cfg, None, tested, None).unwrap();
    assert_eq!(
        execute(&mut store, &job, None, false).failure.unwrap().kind,
        ErrorKind::UnsupportedCapability
    );
    assert!(server.generations().is_empty());
}

#[test]
fn failures_preserve_partial_output_one_terminal_and_no_canonical_answer() {
    let temp = TempDir::new().unwrap();
    let server = Server::start();
    let cfg = config(&temp, &server);
    let mut store = JobStore::open(&temp.path().join("state")).unwrap();
    for target in ["native", "compatible"] {
        for mode in [
            "malformed",
            "truncated",
            "reasoning-only",
            "http-error",
            "oversized",
        ] {
            let job = store
                .create(cfg.clone(), None, request(mode, false), Some(target.into()))
                .unwrap();
            let failed = execute(&mut store, &job, None, false);
            assert_eq!(failed.status, JobStatus::Failed, "{target}/{mode}");
            assert_eq!(failed.request.messages.len(), 1);
            assert_eq!(
                failed.generations.len(),
                1,
                "{target}/{mode}: {:?}",
                failed.failure
            );
            assert_eq!(failed.generations[0].terminal_events, 1);
            if mode == "truncated" {
                assert_eq!(failed.generations[0].text, "partial");
            }
            if mode == "http-error" {
                assert_eq!(
                    failed.failure.as_ref().unwrap().kind,
                    ErrorKind::ResourceExhaustion
                );
            }
            let mut coordinator = Coordinator::open(&cfg.admission_db).unwrap();
            if !coordinator.reservations().unwrap().is_empty() {
                coordinator
                    .acknowledge(
                        "gpu0",
                        "fixture operator",
                        "fixture server generation ended",
                    )
                    .unwrap();
            }
        }
    }
}

#[test]
fn deadline_and_cancellation_close_transport_but_quarantine_unconfirmed_server_work() {
    for cancel in [false, true] {
        let temp = TempDir::new().unwrap();
        let server = Server::start();
        let cfg = config(&temp, &server);
        let root = temp.path().join("state");
        let mut store = JobStore::open(&root).unwrap();
        let mut req = request("slow", false);
        // Cancellation must win well before its deadline; the status assertion below
        // tells the two outcomes apart, so the bounds only need to rule out a hang.
        req.deadline_ms = if cancel { 20_000 } else { 100 };
        let job = store
            .create(cfg.clone(), None, req, Some("native".into()))
            .unwrap();
        let thread = if cancel {
            let root = root.clone();
            let id = job.id.clone();
            Some(thread::spawn(move || {
                let mut store = JobStore::open(&root).unwrap();
                let start = Instant::now();
                loop {
                    let current = store.job(&id).unwrap();
                    if current
                        .generations
                        .last()
                        .is_some_and(|g| !g.text.is_empty())
                        && store.cancel(&id, current.revision).is_ok()
                    {
                        break;
                    }
                    assert!(
                        start.elapsed() < Duration::from_secs(10),
                        "status={:?}, failure={:?}, generations={:?}",
                        current.status,
                        current.failure,
                        current.generations
                    );
                    thread::sleep(Duration::from_millis(3));
                }
            }))
        } else {
            None
        };
        let start = Instant::now();
        let stopped = execute(&mut store, &job, None, false);
        if let Some(t) = thread {
            t.join().unwrap();
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "{:?}",
            start.elapsed()
        );
        assert_eq!(
            stopped.status,
            if cancel {
                JobStatus::Cancelled
            } else {
                JobStatus::TimedOut
            }
        );
        assert!(!stopped.failure.as_ref().unwrap().server_abort_confirmed);
        assert_eq!(stopped.generations[0].text, "partial");
        let mut coordinator = Coordinator::open(&cfg.admission_db).unwrap();
        assert_eq!(coordinator.reservations().unwrap()[0]["quarantined"], true);
        let other = store
            .create(cfg.clone(), None, request("text", false), None)
            .unwrap();
        let result = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(jobs::run(
                &store.root,
                &other.id,
                other.revision,
                None,
                false,
            ));
        assert!(result.is_err());
        assert_eq!(server.generations().len(), 1);
        coordinator
            .acknowledge(
                "gpu0",
                "fixture operator",
                "fixture worker has finished; abort was not asserted",
            )
            .unwrap();
    }
}

#[test]
fn registry_validation_manual_fallback_and_matching_measurements() {
    let temp = TempDir::new().unwrap();
    let server = Server::start();
    let cfg = config(&temp, &server);
    let req = request("text", false);
    let records = cfg
        .targets
        .iter()
        .map(|target| RegistryRecord {
            target_id: target.id.clone(),
            configuration_digest: target.fingerprint(),
            tested: TestedCapabilities {
                capabilities: target.capabilities.clone(),
                tested_at: now(),
                runtime_version: "fixture-v1".into(),
                artifact_digest: "a".repeat(64),
                evidence: "fixture receipt".into(),
            },
            measurement: Some(Measurement {
                hardware_id: cfg.hardware_id.clone(),
                workload: req.workload().into(),
                request_digest: digest(&serde_json::to_vec(&req).unwrap()),
                measured_at: now(),
                latency_ms: Some(if target.id == "native" { 20.0 } else { 10.0 }),
                evidence: "fixture measurement".into(),
            }),
        })
        .collect();
    let mut registry = Registry {
        schema_version: 1,
        records,
    };
    registry.validate(&cfg).unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    // Both fixtures explicitly attest matching artifact/runtime and exact request shape.
    let selected = rt
        .block_on(jobs::route(&cfg, Some(&registry), &req, None))
        .unwrap();
    assert_eq!(selected.0.id, "compatible");
    assert!(selected.2.contains("fresh comparable"));
    let saved = registry.clone();
    registry.records[1].tested.tested_at = now() - cfg.registry_max_age_ms - 1;
    assert_eq!(
        rt.block_on(jobs::route(&cfg, Some(&registry), &req, None))
            .unwrap()
            .0
            .id,
        "native"
    );
    let mut require_tested = req.clone();
    require_tested.require_tested = true;
    assert!(
        rt.block_on(jobs::route(
            &cfg,
            Some(&registry),
            &require_tested,
            Some("compatible")
        ))
        .is_err()
    );
    registry = saved.clone();
    registry.records[1]
        .measurement
        .as_mut()
        .unwrap()
        .request_digest = "b".repeat(64);
    assert_eq!(
        rt.block_on(jobs::route(&cfg, Some(&registry), &req, None))
            .unwrap()
            .0
            .id,
        "native"
    );
    registry = saved;
    registry.records[0].measurement.as_mut().unwrap().latency_ms = None;
    registry.validate(&cfg).unwrap();
    let selected = rt
        .block_on(jobs::route(&cfg, Some(&registry), &req, None))
        .unwrap();
    assert_eq!(selected.0.id, "native");
    assert!(selected.2.contains("manual"));
    registry.records[0].measurement.as_mut().unwrap().latency_ms = Some(0.0);
    assert!(registry.validate(&cfg).is_err());
    registry.records[0].measurement = None;
    registry.records[0].configuration_digest = "wrong".into();
    assert!(registry.validate(&cfg).is_err());
    registry.records.clear();
    registry.schema_version = 2;
    assert!(registry.validate(&cfg).is_err());
    let mut cfg = cfg;
    cfg.targets[0].capabilities.tools = false;
    let selected = rt
        .block_on(jobs::route(&cfg, None, &request("tools", true), None))
        .unwrap();
    assert_eq!(selected.0.id, "compatible");
}

#[test]
fn canonical_call_relationships_and_provider_frames_are_strict() {
    let mut req = request("text", false);
    let mut orphan = Message::text(Role::Tool, "5");
    orphan.tool_call_id = Some("orphan".into());
    req.messages.push(orphan);
    assert!(req.validate().is_err());
    assert!(checked_add("shell", &json!({"a":2,"b":3})).is_err());
    assert!(checked_add("checked_add", &json!({"a":2,"b":3,"extra":true})).is_err());
    assert!(checked_add("checked_add", &json!({"a":i64::MAX,"b":1})).is_err());
    let mut decoder = Decoder::new(ProviderKind::OpenaiLocal, "gen", "fixture:model");
    assert!(decoder.line(b"data: [DONE]").is_err());
    let mut decoder = Decoder::new(ProviderKind::Ollama, "gen", "fixture:model");
    assert!(decoder.line(br#"{"model":"wrong","done":true}"#).is_err());
    let mut decoder = Decoder::new(ProviderKind::OpenaiLocal, "gen", "fixture:model");
    assert!(decoder.line(br#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":1}]},"finish_reason":null}]}"#).is_err());
    assert!(local_url("http://example.com/v1/").is_err());
    assert!(local_url("http://user:secret@localhost/v1/").is_err());
    assert!(local_url("http://localhost/v1/?key=x").is_err());
    let mut decoder = Decoder::new(ProviderKind::Ollama, "gen", "fixture:model");
    assert!(
        decoder
            .line(br#"{"model":"fixture:model","done":true,"eval_count":-1}"#)
            .is_err()
    );
    let mut decoder = Decoder::new(ProviderKind::OpenaiLocal, "gen", "fixture:model");
    assert!(
        decoder
            .line(br#"data: {"choices":[],"usage":{"completion_tokens":"four"}}"#)
            .is_err()
    );
    let mut decoder = Decoder::new(ProviderKind::Ollama, "gen", "fixture:model");
    assert!(decoder.line(br#"{"message":{"tool_calls":{}}}"#).is_err());
    let mut decoder = Decoder::new(ProviderKind::Ollama, "gen", "fixture:model");
    decoder.line(br#"{"message":{"tool_calls":[{"function":{"name":"checked_add","arguments":{"a":2,"b":3}}}]},"done":true,"done_reason":"length"}"#).unwrap();
    assert!(decoder.finish().is_err());
}

/// M10.1 Phase 4: a `done_reason`/`finish_reason` other than "stop" or
/// "tool_calls" is a real, already-tested failure (see the `"length"` case
/// just above) - this confirms *what the operator is told*, not just that
/// it fails. "length" specifically must name the output-token-limit cause
/// and recommend raising the budget or splitting the task, not collapse
/// into one generic "truncated, filtered or unsupported" message that
/// can't be told apart from a content filter or an unknown provider
/// outcome. A trustworthy provider reason is preserved verbatim in every
/// case, never discarded.
#[test]
fn truncated_completion_diagnostics_name_the_actual_cause() {
    let mut decoder = Decoder::new(ProviderKind::Ollama, "gen", "fixture:model");
    decoder
        .line(br#"{"message":{"content":"partial"},"done":true,"done_reason":"length"}"#)
        .unwrap();
    let error = decoder.finish().unwrap_err();
    assert!(
        error.diagnostic.contains("output token limit reached")
            && error.diagnostic.contains("max_output_tokens"),
        "{}",
        error.diagnostic
    );

    let mut decoder = Decoder::new(ProviderKind::OpenaiLocal, "gen", "fixture:model");
    decoder
        .line(br#"data: {"choices":[{"index":0,"delta":{"content":"partial"},"finish_reason":"content_filter"}]}"#)
        .unwrap();
    decoder.line(b"data: [DONE]").unwrap();
    let error = decoder.finish().unwrap_err();
    assert!(
        error.diagnostic.contains("content filter"),
        "{}",
        error.diagnostic
    );

    let mut decoder = Decoder::new(ProviderKind::Ollama, "gen", "fixture:model");
    decoder
        .line(br#"{"message":{"content":"partial"},"done":true,"done_reason":"load"}"#)
        .unwrap();
    let error = decoder.finish().unwrap_err();
    assert!(
        error.diagnostic.contains("\"load\""),
        "an unrecognized-but-trustworthy reason must still be named verbatim: {}",
        error.diagnostic
    );
}

#[test]
fn checkpoint_can_switch_providers_without_replaying_application_tools() {
    let temp = TempDir::new().unwrap();
    let server = Server::start();
    let cfg = config(&temp, &server);
    let mut store = JobStore::open(&temp.path().join("state")).unwrap();
    let job = store
        .create(cfg, None, request("tools", true), Some("native".into()))
        .unwrap();
    let checkpoint = execute(&mut store, &job, None, true);
    let call_id = checkpoint.request.messages[1].tool_calls[0].id.clone();
    let done = execute(&mut store, &checkpoint, Some("compatible"), false);
    assert_eq!(done.status, JobStatus::Completed);
    assert_eq!(done.generations[0].resolved.provider, ProviderKind::Ollama);
    assert_eq!(
        done.generations[1].resolved.provider,
        ProviderKind::OpenaiLocal
    );
    assert_eq!(
        done.request.messages[2].tool_call_id.as_deref(),
        Some(call_id.as_str())
    );
    assert_eq!(server.generations().len(), 2);
    let second = &server.generations()[1].1;
    assert_eq!(second["messages"][1]["tool_calls"][0]["id"], call_id);
    assert_eq!(second["messages"][2]["tool_call_id"], call_id);
    assert_eq!(second["messages"][2]["content"], "5");
}

#[test]
fn generation_budget_ends_repeated_tools_and_releases_known_idle_resource() {
    let temp = TempDir::new().unwrap();
    let server = Server::start();
    let cfg = config(&temp, &server);
    let mut store = JobStore::open(&temp.path().join("state")).unwrap();
    let job = store
        .create(
            cfg.clone(),
            None,
            request("endless-tools", true),
            Some("native".into()),
        )
        .unwrap();
    let done = execute(&mut store, &job, None, false);
    assert_eq!(done.status, JobStatus::Failed);
    assert_eq!(done.failure.unwrap().kind, ErrorKind::ResourceExhaustion);
    assert!(done.owner_token.is_none());
    assert_eq!(done.generations.len(), 3);
    assert_eq!(server.generations().len(), 3);
    assert_eq!(
        done.request
            .messages
            .iter()
            .filter(|m| m.role == Role::Tool)
            .count(),
        3
    );
    assert!(
        Coordinator::open(&cfg.admission_db)
            .unwrap()
            .reservations()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn cancellation_during_discovery_never_dispatches_or_reserves_a_generation() {
    let temp = TempDir::new().unwrap();
    let server = Server::start();
    let mut cfg = config(&temp, &server);
    cfg.targets[0].endpoint = format!("{}slow-discovery/", server.url);
    cfg.fallback_before_dispatch = false;
    let state = temp.path().join("state");
    let mut store = JobStore::open(&state).unwrap();
    let job = store
        .create(cfg.clone(), None, request("text", false), None)
        .unwrap();
    let worker_state = state.clone();
    let worker_job = job.clone();
    let worker = thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(jobs::run(
                &worker_state,
                &worker_job.id,
                worker_job.revision,
                None,
                false,
            ))
            .unwrap()
    });
    let began = Instant::now();
    while !server
        .requests
        .lock()
        .unwrap()
        .iter()
        .any(|(path, _)| path.contains("slow-discovery"))
    {
        assert!(began.elapsed() < Duration::from_secs(5));
        thread::sleep(Duration::from_millis(5));
    }
    let current = store.job(&job.id).unwrap();
    assert_eq!(current.status, JobStatus::Preparing);
    store.cancel(&job.id, current.revision).unwrap();
    let cancelled = worker.join().unwrap();
    assert_eq!(cancelled.status, JobStatus::Cancelled);
    assert!(cancelled.generations.is_empty());
    assert!(server.generations().is_empty());
    assert!(
        Coordinator::open(&cfg.admission_db)
            .unwrap()
            .reservations()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn explicit_backend_extension_provenance_and_sanitized_provider_errors() {
    let temp = TempDir::new().unwrap();
    let server = Server::start();
    let mut cfg = config(&temp, &server);
    cfg.targets[1].extensions = Some(AdapterExtensions {
        ollama_openai: OllamaOpenaiOptions {
            reasoning_effort: "none".into(),
        },
    });
    cfg.validate().unwrap();
    let mut store = JobStore::open(&temp.path().join("state")).unwrap();
    let job = store
        .create(
            cfg.clone(),
            None,
            request("text", false),
            Some("compatible".into()),
        )
        .unwrap();
    let done = execute(&mut store, &job, None, false);
    assert_eq!(done.status, JobStatus::Completed);
    assert_eq!(server.generations()[0].1["reasoning_effort"], "none");
    assert!(
        done.generations[0]
            .resolved
            .artifact_provenance
            .contains("explicit ollama.openai")
    );
    cfg.targets[1]
        .extensions
        .as_mut()
        .unwrap()
        .ollama_openai
        .reasoning_effort = "imaginary".into();
    assert!(cfg.validate().is_err());
    cfg.targets[1].extensions = None;
    cfg.targets[0].expected_artifact_digest = Some("b".repeat(64));
    let pinned = store
        .create(
            cfg.clone(),
            None,
            request("text", false),
            Some("native".into()),
        )
        .unwrap();
    assert_eq!(
        execute(&mut store, &pinned, None, false)
            .failure
            .unwrap()
            .kind,
        ErrorKind::UnavailableTarget
    );
    assert_eq!(server.generations().len(), 1);
    cfg.targets[0].expected_artifact_digest = None;
    for target in ["native", "compatible"] {
        let job = store
            .create(
                cfg.clone(),
                None,
                request("context-error", false),
                Some(target.into()),
            )
            .unwrap();
        let failed = execute(&mut store, &job, None, false);
        let failure = failed.failure.unwrap();
        assert_eq!(failure.kind, ErrorKind::ContextOverflow);
        assert!(!failure.diagnostic.contains("private prompt"));
        assert_eq!(failed.request.messages.len(), 1);
        // M9: a complete, fully-read HTTP error response (this fixture
        // returns a full "400 Bad Request" body, not a dropped stream or
        // timeout) is now a positive confirmation that the provider's
        // handler for this request has already returned, so the resource
        // auto-releases instead of requiring manual acknowledgement -
        // confirmed here by a fresh acquire succeeding with no prior
        // acknowledge call at all.
        assert!(
            Coordinator::open(&cfg.admission_db)
                .unwrap()
                .reservations()
                .unwrap()
                .is_empty(),
            "a confirmed-clean provider error should auto-release, not require manual acknowledgement"
        );
    }
    let mut decoder = Decoder::new(ProviderKind::Ollama, "test", "fixture:model");
    assert_eq!(
        decoder
            .line(br#"{"error":"out of memory"}"#)
            .unwrap_err()
            .kind,
        ErrorKind::ResourceExhaustion
    );
}

fn cli(state: &std::path::Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_clatterforge"))
        .arg("--state")
        .arg(state)
        .args(args)
        .output()
        .unwrap()
}
struct OwnedCli(std::process::Child);
impl Drop for OwnedCli {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn cli_import_and_actual_owner_crash_preserve_unknown_without_dispatch_replay() {
    let temp = TempDir::new().unwrap();
    let server = Server::start();
    let cfg = config(&temp, &server);
    let state = temp.path().join("state");
    let config_path = temp.path().join("config.json");
    let request_path = temp.path().join("request.json");
    let registry_path = temp.path().join("registry.json");
    std::fs::write(&config_path, serde_json::to_vec(&cfg).unwrap()).unwrap();
    std::fs::write(
        &request_path,
        serde_json::to_vec(&request("slow", false)).unwrap(),
    )
    .unwrap();
    std::fs::write(&registry_path, br#"{"schema_version":99,"records":[]}"#).unwrap();
    assert!(
        !cli(
            &state,
            &[
                "inference-registry-import",
                "--config",
                config_path.to_str().unwrap(),
                "--registry",
                registry_path.to_str().unwrap()
            ]
        )
        .status
        .success()
    );
    assert!(!state.join("inference-registry.json").exists());
    std::fs::write(&registry_path, br#"{"schema_version":1,"records":[]}"#).unwrap();
    assert!(
        cli(
            &state,
            &[
                "inference-registry-import",
                "--config",
                config_path.to_str().unwrap(),
                "--registry",
                registry_path.to_str().unwrap()
            ]
        )
        .status
        .success()
    );
    let prepared = cli(
        &state,
        &[
            "inference-create",
            "--config",
            config_path.to_str().unwrap(),
            "--request",
            request_path.to_str().unwrap(),
            "--registry",
            registry_path.to_str().unwrap(),
            "--target",
            "native",
        ],
    );
    assert!(
        prepared.status.success(),
        "{}",
        String::from_utf8_lossy(&prepared.stderr)
    );
    let job: Job = serde_json::from_slice(&prepared.stdout).unwrap();
    assert!(!state.join("state.sqlite").exists()); // Direct jobs don't mutate delegated state.
    let mut child = OwnedCli(
        std::process::Command::new(env!("CARGO_BIN_EXE_clatterforge"))
            .arg("--state")
            .arg(&state)
            .args([
                "inference-run",
                &job.id,
                "--revision",
                &job.revision.to_string(),
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let mut store = JobStore::open(&state).unwrap();
    let began = Instant::now();
    let partial = loop {
        let current = store.job(&job.id).unwrap();
        if current
            .generations
            .first()
            .is_some_and(|g| g.text == "partial")
        {
            break current;
        }
        assert!(
            began.elapsed() < Duration::from_secs(20),
            "worker did not persist partial stream"
        );
        thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(partial.status, JobStatus::Running);
    assert!(store.reconcile(&job.id, partial.revision).is_err());
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    let dead = store.job(&job.id).unwrap();
    let result = cli(
        &state,
        &[
            "inference-reconcile",
            &job.id,
            "--revision",
            &dead.revision.to_string(),
        ],
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let unknown: Job = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(unknown.status, JobStatus::Unknown);
    assert_eq!(unknown.generations[0].text, "partial");
    assert_eq!(unknown.request.messages.len(), 1);
    assert_eq!(unknown.generations[0].terminal_events, 0);
    assert!(
        !cli(
            &state,
            &[
                "inference-run",
                &job.id,
                "--revision",
                &unknown.revision.to_string()
            ]
        )
        .status
        .success()
    );
    assert_eq!(server.generations().len(), 1);
    assert_eq!(
        Coordinator::open(&cfg.admission_db)
            .unwrap()
            .reservations()
            .unwrap()[0]["quarantined"],
        true
    );
    let ready = store
        .create(cfg, None, request("text", false), None)
        .unwrap();
    let cancelled = cli(
        &state,
        &[
            "inference-cancel",
            &ready.id,
            "--revision",
            &ready.revision.to_string(),
        ],
    );
    assert!(cancelled.status.success());
    assert_eq!(
        serde_json::from_slice::<Job>(&cancelled.stdout)
            .unwrap()
            .status,
        JobStatus::Cancelled
    );
    assert_eq!(server.generations().len(), 1);
}

#[test]
fn admission_is_shared_between_state_roots_and_interruption_never_replays() {
    let temp = TempDir::new().unwrap();
    let server = Server::start();
    let cfg = config(&temp, &server);
    let mut first = JobStore::open(&temp.path().join("one")).unwrap();
    let mut second = JobStore::open(&temp.path().join("two")).unwrap();
    let first_job = first
        .create(cfg.clone(), None, request("text", false), None)
        .unwrap();
    let second_job = second
        .create(cfg.clone(), None, request("text", false), None)
        .unwrap();
    let owner = current_identity().unwrap();
    let mut coordinator = Coordinator::open(&cfg.admission_db).unwrap();
    coordinator.acquire("gpu0", &first_job.id, &owner).unwrap();
    assert!(
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(jobs::run(
                &second.root,
                &second_job.id,
                second_job.revision,
                None,
                false
            ))
            .is_err()
    );
    assert!(server.generations().is_empty());
    assert!(
        coordinator
            .acknowledge("gpu0", "fixture", "still live")
            .is_err()
    );
    coordinator.release("gpu0", &first_job.id).unwrap();
    let mut stale = owner;
    stale.created += 1;
    let interrupted = first
        .mutate(
            &first_job.id,
            Some(first_job.revision),
            None,
            "fixture_intent",
            json!({}),
            |j| {
                j.status = JobStatus::Running;
                j.owner = Some(stale);
                j.owner_token = Some("fixture-owner".into());
                Ok(())
            },
        )
        .unwrap();
    coordinator
        .acquire("gpu0", &first_job.id, &current_identity().unwrap())
        .unwrap();
    let unknown = first
        .reconcile(&interrupted.id, interrupted.revision)
        .unwrap();
    assert_eq!(unknown.status, JobStatus::Unknown);
    assert!(
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(jobs::run(
                &first.root,
                &unknown.id,
                unknown.revision,
                None,
                false
            ))
            .is_err()
    );
    assert!(server.generations().is_empty());
    assert_eq!(coordinator.reservations().unwrap()[0]["quarantined"], true);
}

#[test]
fn stale_quarantine_from_a_confirmed_dead_owner_auto_releases_for_the_next_dispatch() {
    // M9: the manual-acknowledge safety bar (owner confirmed not
    // alive/unverifiable) is reused automatically on the dispatch path
    // itself, so a *prior* job's stale quarantine doesn't require a
    // human to run `inference-release-resource` before the *next* job
    // can even try.
    let temp = TempDir::new().unwrap();
    let server = Server::start();
    let cfg = config(&temp, &server);
    let mut store = JobStore::open(&temp.path().join("state")).unwrap();
    let job_a = store
        .create(cfg.clone(), None, request("text", false), None)
        .unwrap();
    let mut stale = current_identity().unwrap();
    stale.created += 1; // a reused/incremented creation time: confirmed not this live process
    let mut coordinator = Coordinator::open(&cfg.admission_db).unwrap();
    coordinator.acquire("gpu0", &job_a.id, &stale).unwrap();
    coordinator.quarantine_job(&job_a.id).unwrap();
    assert_eq!(coordinator.reservations().unwrap()[0]["quarantined"], true);

    let job_b = store
        .create(cfg.clone(), None, request("text", false), None)
        .unwrap();
    let done = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(jobs::run(
            &store.root,
            &job_b.id,
            job_b.revision,
            None,
            false,
        ))
        .unwrap();
    assert_eq!(done.status, JobStatus::Completed, "{:?}", done.failure);
    assert_eq!(
        server.generations().len(),
        1,
        "the second job should have actually dispatched, not been blocked by the first job's stale quarantine"
    );
}

#[test]
fn a_live_owners_quarantine_is_left_alone_not_auto_released() {
    // The inverse of the above: when the quarantine's owner is this same
    // live process (unverifiable-as-dead, by construction, since it IS
    // alive), automatic recovery must never touch it - only a human
    // calling `inference-release-resource` can.
    let temp = TempDir::new().unwrap();
    let server = Server::start();
    let cfg = config(&temp, &server);
    let mut store = JobStore::open(&temp.path().join("state")).unwrap();
    let job_a = store
        .create(cfg.clone(), None, request("text", false), None)
        .unwrap();
    let owner = current_identity().unwrap();
    let mut coordinator = Coordinator::open(&cfg.admission_db).unwrap();
    coordinator.acquire("gpu0", &job_a.id, &owner).unwrap();
    coordinator.quarantine_job(&job_a.id).unwrap();

    let job_b = store
        .create(cfg.clone(), None, request("text", false), None)
        .unwrap();
    assert!(
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(jobs::run(
                &store.root,
                &job_b.id,
                job_b.revision,
                None,
                false
            ))
            .is_err()
    );
    assert!(server.generations().is_empty());
    assert_eq!(coordinator.reservations().unwrap()[0]["quarantined"], true);
}

#[test]
fn real_killed_process_still_auto_recovers_the_next_real_dispatch_without_reconcile() {
    // M9 follow-up: a real OS-level kill (not a simulated owner
    // identity) never runs the dying process's own Drop-based
    // quarantine fallback, so the lease is left "held" with
    // quarantined=0 under a now-dead owner - not "quarantined=1" the
    // way every earlier M9 test (and the existing
    // `cli_import_and_actual_owner_crash...` test, which calls
    // `inference-reconcile` before anything else touches the resource)
    // reaches. Deliberately skips reconcile here to exercise the
    // realistic "crashed, immediately retried" path. Found and fixed a
    // real bug in `try_automatic_release` while writing this test: it
    // bailed out early whenever `quarantined` was false, never reaching
    // the owner-liveness check at all - see `src/inference/state.rs`.
    let temp = TempDir::new().unwrap();
    let server = Server::start();
    let cfg = config(&temp, &server);
    let state = temp.path().join("state");
    let config_path = temp.path().join("config.json");
    let request_path = temp.path().join("request.json");
    std::fs::write(&config_path, serde_json::to_vec(&cfg).unwrap()).unwrap();
    std::fs::write(
        &request_path,
        serde_json::to_vec(&request("slow", false)).unwrap(),
    )
    .unwrap();

    let prepared = cli(
        &state,
        &[
            "inference-create",
            "--config",
            config_path.to_str().unwrap(),
            "--request",
            request_path.to_str().unwrap(),
            "--target",
            "native",
        ],
    );
    assert!(
        prepared.status.success(),
        "{}",
        String::from_utf8_lossy(&prepared.stderr)
    );
    let job_a: Job = serde_json::from_slice(&prepared.stdout).unwrap();

    let mut child = OwnedCli(
        std::process::Command::new(env!("CARGO_BIN_EXE_clatterforge"))
            .arg("--state")
            .arg(&state)
            .args([
                "inference-run",
                &job_a.id,
                "--revision",
                &job_a.revision.to_string(),
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let store = JobStore::open(&state).unwrap();
    let began = Instant::now();
    loop {
        let current = store.job(&job_a.id).unwrap();
        if current
            .generations
            .first()
            .is_some_and(|g| g.text == "partial")
        {
            break;
        }
        assert!(
            began.elapsed() < Duration::from_secs(5),
            "worker did not persist partial stream"
        );
        thread::sleep(Duration::from_millis(5));
    }

    // A real, abrupt process kill - no graceful shutdown, no Drop.
    child.0.kill().unwrap();
    child.0.wait().unwrap();

    let coordinator = Coordinator::open(&cfg.admission_db).unwrap();
    let reservations = coordinator.reservations().unwrap();
    assert_eq!(reservations.len(), 1);
    assert_eq!(
        reservations[0]["quarantined"], false,
        "a killed process never runs its own Drop-based quarantine fallback; this is the exact \
         'held, not quarantined, owner dead' state the fix targets"
    );
    drop(coordinator);

    // A second, fresh job dispatches against the same resource with no
    // manual `inference-release-resource`/`inference-reconcile` call at
    // all, and completes for real - not merely "is allowed to try."
    std::fs::write(
        &request_path,
        serde_json::to_vec(&request("text", false)).unwrap(),
    )
    .unwrap();
    let prepared_b = cli(
        &state,
        &[
            "inference-create",
            "--config",
            config_path.to_str().unwrap(),
            "--request",
            request_path.to_str().unwrap(),
            "--target",
            "native",
        ],
    );
    assert!(
        prepared_b.status.success(),
        "{}",
        String::from_utf8_lossy(&prepared_b.stderr)
    );
    let job_b: Job = serde_json::from_slice(&prepared_b.stdout).unwrap();
    let result_b = cli(
        &state,
        &[
            "inference-run",
            &job_b.id,
            "--revision",
            &job_b.revision.to_string(),
        ],
    );
    assert!(
        result_b.status.success(),
        "{}",
        String::from_utf8_lossy(&result_b.stderr)
    );
    let done_b: Job = serde_json::from_slice(&result_b.stdout).unwrap();
    assert_eq!(done_b.status, JobStatus::Completed);
    assert_eq!(done_b.generations[0].text, "hÃ©ðŸ™‚ verified");

    // No dangling/duplicate reservation: the resource is cleanly held
    // and released by job B alone, never concurrently owned by both.
    let coordinator = Coordinator::open(&cfg.admission_db).unwrap();
    assert!(coordinator.reservations().unwrap().is_empty());
}

#[test]
fn global_stop_still_blocks_admission_even_through_the_auto_recovery_path() {
    // The automatic-recovery path must never become a side door around
    // the M7 global stop: engaging it before a second dispatch attempt
    // must refuse that dispatch exactly as it would with no stale
    // quarantine involved at all, confirmed against the real CLI.
    let temp = TempDir::new().unwrap();
    let server = Server::start();
    let cfg = config(&temp, &server);
    let state = temp.path().join("state");
    let config_path = temp.path().join("config.json");
    let request_path = temp.path().join("request.json");
    std::fs::write(&config_path, serde_json::to_vec(&cfg).unwrap()).unwrap();
    std::fs::write(
        &request_path,
        serde_json::to_vec(&request("slow", false)).unwrap(),
    )
    .unwrap();

    let prepared = cli(
        &state,
        &[
            "inference-create",
            "--config",
            config_path.to_str().unwrap(),
            "--request",
            request_path.to_str().unwrap(),
            "--target",
            "native",
        ],
    );
    assert!(prepared.status.success());
    let job_a: Job = serde_json::from_slice(&prepared.stdout).unwrap();
    let mut child = OwnedCli(
        std::process::Command::new(env!("CARGO_BIN_EXE_clatterforge"))
            .arg("--state")
            .arg(&state)
            .args([
                "inference-run",
                &job_a.id,
                "--revision",
                &job_a.revision.to_string(),
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let store = JobStore::open(&state).unwrap();
    let began = Instant::now();
    loop {
        let current = store.job(&job_a.id).unwrap();
        if current
            .generations
            .first()
            .is_some_and(|g| g.text == "partial")
        {
            break;
        }
        assert!(began.elapsed() < Duration::from_secs(5));
        thread::sleep(Duration::from_millis(5));
    }
    child.0.kill().unwrap();
    child.0.wait().unwrap();

    assert!(
        cli(
            &state,
            &["global-stop", "--by", "fixture", "--reason", "test"]
        )
        .status
        .success()
    );

    std::fs::write(
        &request_path,
        serde_json::to_vec(&request("text", false)).unwrap(),
    )
    .unwrap();
    let prepared_b = cli(
        &state,
        &[
            "inference-create",
            "--config",
            config_path.to_str().unwrap(),
            "--request",
            request_path.to_str().unwrap(),
            "--target",
            "native",
        ],
    );
    assert!(prepared_b.status.success());
    let job_b: Job = serde_json::from_slice(&prepared_b.stdout).unwrap();
    let blocked = cli(
        &state,
        &[
            "inference-run",
            &job_b.id,
            "--revision",
            &job_b.revision.to_string(),
        ],
    );
    assert!(
        !blocked.status.success(),
        "global stop must refuse admission even when a stale quarantine would otherwise auto-recover"
    );
    assert!(String::from_utf8_lossy(&blocked.stderr).contains("global stop"));
}
