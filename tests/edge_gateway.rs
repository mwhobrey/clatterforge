//! M12.0.1 Cloudflare integration, Phase 3's narrowest-safe-gateway requirement:
//! `clatterforge edge-gateway` must forward exactly `POST /api/edge/tools/call` to
//! its upstream and refuse everything else outright (fail closed - an allow-list of
//! one route, not a deny-list), while still forwarding that one route's body and the
//! relevant auth headers through correctly.

use std::{
    collections::BTreeMap,
    io::Read,
    net::TcpListener,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

type CapturedRequest = (String, BTreeMap<String, String>, Vec<u8>);

struct MockUpstream {
    url: String,
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl MockUpstream {
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
                let url = request.url().to_string();
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
                copy.lock().unwrap().push((url, headers, bytes));
                let _ = request.respond(
                    tiny_http::Response::from_string(r#"{"ok":true}"#)
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

    fn last(&self) -> Option<CapturedRequest> {
        self.requests.lock().unwrap().last().cloned()
    }
}

impl Drop for MockUpstream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

struct Gateway {
    process: Child,
    port: u16,
}

impl Gateway {
    fn start(upstream_url: &str) -> Self {
        let port = free_port();
        let mut process = Command::new(env!("CARGO_BIN_EXE_clatterforge"))
            .args(["edge-gateway", "--port", &port.to_string(), "--upstream"])
            .arg(upstream_url)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn clatterforge edge-gateway");
        for _ in 0..100 {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return Self { process, port };
            }
            thread::sleep(Duration::from_millis(20));
        }
        let _ = process.kill();
        let _ = process.wait();
        panic!("edge-gateway never became reachable on port {port}");
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
}

#[tokio::test]
async fn every_path_except_the_one_allowed_route_is_refused_with_404() {
    let upstream = MockUpstream::start();
    let gateway = Gateway::start(&upstream.url);
    let client = client();

    for path in ["/", "/health", "/api/tasks", "/api/pair", "/api/stop"] {
        let response = client
            .get(format!("{}{path}", gateway.url()))
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            404,
            "path {path} must be refused, not forwarded"
        );
    }
    assert!(
        upstream.last().is_none(),
        "no refused request should ever reach the upstream"
    );
}

#[tokio::test]
async fn the_one_allowed_route_forwards_body_and_the_relevant_headers_to_upstream() {
    let upstream = MockUpstream::start();
    let gateway = Gateway::start(&upstream.url);
    let client = client();

    let body = r#"{"tool":"conductor_capabilities","params":{}}"#;
    let response = client
        .post(format!("{}/api/edge/tools/call", gateway.url()))
        .header("Authorization", "Bearer real-enrollment-token")
        .header("Content-Type", "application/json")
        .header("CF-Access-Client-Id", "a-service-token-id")
        .header("CF-Access-Client-Secret", "a-service-token-secret")
        .header("X-Not-On-The-Allowlist", "must-not-be-forwarded")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), r#"{"ok":true}"#);

    let (path, headers, received_body) = upstream
        .last()
        .expect("upstream should have received exactly the forwarded request");
    assert_eq!(path, "/api/edge/tools/call");
    assert_eq!(String::from_utf8(received_body).unwrap(), body);
    assert_eq!(
        headers.get("authorization").map(String::as_str),
        Some("Bearer real-enrollment-token")
    );
    assert_eq!(
        headers.get("cf-access-client-id").map(String::as_str),
        Some("a-service-token-id")
    );
    assert_eq!(
        headers.get("cf-access-client-secret").map(String::as_str),
        Some("a-service-token-secret")
    );
    assert!(
        !headers.contains_key("x-not-on-the-allowlist"),
        "the gateway must only forward its explicit header allow-list, {headers:#?}"
    );
}

#[tokio::test]
async fn wrong_method_on_the_allowed_path_is_not_forwarded_as_a_success() {
    let upstream = MockUpstream::start();
    let gateway = Gateway::start(&upstream.url);
    let client = client();

    let response = client
        .get(format!("{}/api/edge/tools/call", gateway.url()))
        .send()
        .await
        .unwrap();
    assert_ne!(response.status(), 200, "{:?}", response.status());
    assert!(upstream.last().is_none());
}

#[tokio::test]
async fn an_unreachable_upstream_fails_as_a_gateway_error_not_a_fabricated_success() {
    // A port nothing is listening on, not a mock - the gateway must report failure,
    // never synthesize a success response when it cannot actually reach upstream.
    let dead_port = free_port();
    let gateway = Gateway::start(&format!("http://127.0.0.1:{dead_port}"));
    let client = client();

    let response = client
        .post(format!("{}/api/edge/tools/call", gateway.url()))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 502, "{:?}", response.status());
}
