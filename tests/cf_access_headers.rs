//! M12.0.1 Cloudflare Access integration: proves `clatterforge-edge` sends the two
//! service-token headers when configured, preserves the existing bearer-token
//! Conductor auth alongside them, fails clearly on a half-configured credential
//! pair, and sends neither header when unconfigured - existing direct/private-network
//! operation must see no difference at all.
//!
//! Uses a `tiny_http`-based mock in place of the real `clatterforge serve`, the same
//! pattern `tests/inference.rs` already uses for a bare HTTP backend - this is about
//! the edge binary's own outbound request, not the real Conductor's behavior.

use std::{
    collections::BTreeMap,
    io::Read,
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

struct MockConductor {
    url: String,
    requests: Arc<Mutex<Vec<BTreeMap<String, String>>>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl MockConductor {
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
                let headers: BTreeMap<String, String> = request
                    .headers()
                    .iter()
                    .map(|h| {
                        (
                            h.field.as_str().as_str().to_lowercase(),
                            h.value.to_string(),
                        )
                    })
                    .collect();
                let mut bytes = vec![];
                let _ = request.as_reader().take(1_048_576).read_to_end(&mut bytes);
                copy.lock().unwrap().push(headers);
                let body = serde_json::json!({"capabilities": {"tools": []}}).to_string();
                let _ = request.respond(
                    tiny_http::Response::from_string(body)
                        .with_status_code(tiny_http::StatusCode(200)),
                );
            }
        });
        Self {
            url: format!("http://{address}"),
            requests,
            stop,
            worker: Some(worker),
        }
    }

    fn last_request_headers(&self) -> BTreeMap<String, String> {
        for _ in 0..200 {
            if let Some(headers) = self.requests.lock().unwrap().last().cloned() {
                return headers;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("mock conductor never received a request");
    }
}

impl Drop for MockConductor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// `clatterforge-edge` lives in a separate workspace member, so Cargo does not
/// auto-inject a `CARGO_BIN_EXE_*` variable for it the way it does for a binary in
/// this same package - same always-rebuild pattern `tests/edge.rs` already uses, and
/// for the same reason (a stale binary in a separate workspace member can silently
/// drop a wire/CLI-flag change nothing else would catch).
fn edge_binary() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // Build into the same target dir as this test binary so a CARGO_TARGET_DIR
    // override never falls back to a stale edge binary elsewhere.
    let target_dir = std::path::Path::new(env!("CARGO_BIN_EXE_clatterforge"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("cargo target dir");
    let status = std::process::Command::new(env!("CARGO"))
        .args(["build", "-p", "clatterforge-edge", "--target-dir"])
        .arg(target_dir)
        .current_dir(&manifest_dir)
        .status()
        .expect("run cargo build -p clatterforge-edge");
    assert!(status.success(), "building clatterforge-edge failed");
    target_dir
        .join("debug")
        .join(format!("clatterforge-edge{}", std::env::consts::EXE_SUFFIX))
}

/// A single `conductor_capabilities` call through a real edge process, over real
/// stdio, against the mock conductor above - close enough to a real MCP round trip to
/// prove the actual outbound HTTP request's headers, without needing the real
/// Conductor's own auth/task machinery at all for this specific question.
fn run_edge_and_capture(extra_env: &[(&str, &str)]) -> BTreeMap<String, String> {
    let mock = MockConductor::start();
    let mut command = std::process::Command::new(edge_binary());
    command
        .arg("--main-url")
        .arg(&mock.url)
        .env("CONDUCTOR_EDGE_TOKEN", "test-bearer-token")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in extra_env {
        command.env(key, value);
    }
    let mut child = command.spawn().expect("spawn clatterforge-edge");
    // Write one framed MCP initialize + tools/call request directly over stdio -
    // deliberately not pulling in the full `rmcp` client for this test, since all
    // that's needed is to make the edge binary emit exactly one outbound HTTP call.
    use std::io::Write;
    let stdin = child.stdin.as_mut().unwrap();
    let initialize = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "0"}
        }
    });
    writeln!(stdin, "{initialize}").unwrap();
    let initialized = serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
    writeln!(stdin, "{initialized}").unwrap();
    let call = serde_json::json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "conductor_capabilities", "arguments": {}}
    });
    writeln!(stdin, "{call}").unwrap();

    let headers = mock.last_request_headers();
    let _ = child.kill();
    let _ = child.wait();
    headers
}

#[test]
fn cf_access_headers_are_sent_when_both_credentials_are_configured() {
    let headers = run_edge_and_capture(&[
        ("CF_ACCESS_CLIENT_ID", "test-client-id"),
        ("CF_ACCESS_CLIENT_SECRET", "test-client-secret"),
    ]);
    assert_eq!(
        headers.get("cf-access-client-id").map(String::as_str),
        Some("test-client-id"),
        "{headers:#?}"
    );
    assert_eq!(
        headers.get("cf-access-client-secret").map(String::as_str),
        Some("test-client-secret"),
        "{headers:#?}"
    );
    // The Conductor's own bearer auth must still be present alongside the Cloudflare
    // Access headers - additive, never a replacement.
    assert_eq!(
        headers.get("authorization").map(String::as_str),
        Some("Bearer test-bearer-token"),
        "{headers:#?}"
    );
}

#[test]
fn no_cf_access_headers_are_sent_when_unconfigured_direct_operation_is_unaffected() {
    let headers = run_edge_and_capture(&[]);
    assert!(!headers.contains_key("cf-access-client-id"), "{headers:#?}");
    assert!(
        !headers.contains_key("cf-access-client-secret"),
        "{headers:#?}"
    );
    assert_eq!(
        headers.get("authorization").map(String::as_str),
        Some("Bearer test-bearer-token"),
        "{headers:#?}"
    );
}

/// Missing configuration must fail clearly, not silently send a half-formed pair or
/// silently skip - the edge process should refuse to even start.
#[test]
fn a_half_configured_cf_access_credential_pair_fails_clearly_at_startup() {
    let child = std::process::Command::new(edge_binary())
        .arg("--main-url")
        .arg("http://127.0.0.1:1") // never reached - this must fail before any request
        .env("CONDUCTOR_EDGE_TOKEN", "test-bearer-token")
        .env("CF_ACCESS_CLIENT_ID", "only-the-id-is-set")
        .env_remove("CF_ACCESS_CLIENT_SECRET")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn clatterforge-edge");
    let output = child.wait_with_output().unwrap();
    assert!(
        !output.status.success(),
        "a half-configured credential pair must not start successfully"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Client Secret") && stderr.contains("Client ID"),
        "error should name which half is missing, not just fail generically: {stderr}"
    );
    assert!(
        !stderr.contains("only-the-id-is-set"),
        "the configured credential value must never appear in output, even on error: {stderr}"
    );
}

/// The reverse half-configured case.
#[test]
fn a_secret_without_a_client_id_also_fails_clearly_at_startup() {
    let child = std::process::Command::new(edge_binary())
        .arg("--main-url")
        .arg("http://127.0.0.1:1")
        .env("CONDUCTOR_EDGE_TOKEN", "test-bearer-token")
        .env_remove("CF_ACCESS_CLIENT_ID")
        .env("CF_ACCESS_CLIENT_SECRET", "only-the-secret-is-set")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn clatterforge-edge");
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Client Secret") && stderr.contains("Client ID"),
        "{stderr}"
    );
    assert!(!stderr.contains("only-the-secret-is-set"), "{stderr}");
}

/// Credentials from files, not just environment variables - and files take
/// precedence over an env var of the same purpose when both are set.
#[test]
fn cf_access_credentials_can_be_read_from_files_and_files_take_precedence() {
    let dir = tempfile::tempdir().unwrap();
    let id_file = dir.path().join("client-id");
    let secret_file = dir.path().join("client-secret");
    std::fs::write(&id_file, "file-client-id\n").unwrap();
    std::fs::write(&secret_file, "file-client-secret\n").unwrap();

    let mock = MockConductor::start();
    let mut command = std::process::Command::new(edge_binary());
    command
        .arg("--main-url")
        .arg(&mock.url)
        .arg("--cf-access-client-id-file")
        .arg(&id_file)
        .arg("--cf-access-client-secret-file")
        .arg(&secret_file)
        .env("CONDUCTOR_EDGE_TOKEN", "test-bearer-token")
        // Env vars present too, with different (wrong) values - files must win.
        .env("CF_ACCESS_CLIENT_ID", "env-client-id-should-not-be-used")
        .env(
            "CF_ACCESS_CLIENT_SECRET",
            "env-client-secret-should-not-be-used",
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("spawn clatterforge-edge");
    use std::io::Write;
    let stdin = child.stdin.as_mut().unwrap();
    writeln!(
        stdin,
        "{}",
        serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}
        })
    )
    .unwrap();
    writeln!(
        stdin,
        "{}",
        serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
    )
    .unwrap();
    writeln!(
        stdin,
        "{}",
        serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name": "conductor_capabilities", "arguments": {}}
        })
    )
    .unwrap();

    let headers = mock.last_request_headers();
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(
        headers.get("cf-access-client-id").map(String::as_str),
        Some("file-client-id"),
        "{headers:#?}"
    );
    assert_eq!(
        headers.get("cf-access-client-secret").map(String::as_str),
        Some("file-client-secret"),
        "{headers:#?}"
    );
}
