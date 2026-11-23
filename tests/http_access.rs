use reqwest::{StatusCode, header};
use serde_json::{Value, json};
use std::{
    fs,
    net::TcpListener,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;

struct Server {
    _dir: TempDir,
    child: Child,
    base: String,
    token: String,
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Server {
    fn start(config: Option<Value>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        if let Some(config) = config {
            fs::write(dir.path().join("http-access.json"), config.to_string()).unwrap();
        }
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let mut child = Command::new(env!("CARGO_BIN_EXE_dev-conductor"))
            .arg("--state")
            .arg(dir.path())
            .args(["serve", "--port", &port.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if dir.path().join("serve-token").exists()
                && std::net::TcpStream::connect(("127.0.0.1", port)).is_ok()
            {
                break;
            }
            assert!(child.try_wait().unwrap().is_none(), "fixture server exited");
            assert!(Instant::now() < deadline, "fixture server did not start");
            thread::sleep(Duration::from_millis(25));
        }
        let token = fs::read_to_string(dir.path().join("serve-token")).unwrap();
        Self {
            _dir: dir,
            child,
            base: format!("http://127.0.0.1:{port}"),
            token,
        }
    }
}

#[tokio::test]
async fn default_access_is_local_and_deployment_opt_ins_are_exact() {
    let client = reqwest::Client::new();
    let default = Server::start(None);
    let url = format!("{}/api/tasks", default.base);
    assert_eq!(
        client
            .get(&url)
            .bearer_auth(&default.token)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        client
            .get(&url)
            .bearer_auth(&default.token)
            .header(header::HOST, "conductor.example.invalid:8788")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    let configured = Server::start(Some(
        json!({"trusted_http_hosts":["conductor.example.invalid:8788"],"https_origins":["https://ui.example.invalid"]}),
    ));
    let url = format!("{}/api/tasks", configured.base);
    let response = client
        .get(&url)
        .bearer_auth(&configured.token)
        .header(header::HOST, "conductor.example.invalid:8788")
        .header(header::ORIGIN, "http://conductor.example.invalid:8788")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN],
        "http://conductor.example.invalid:8788"
    );
    assert_eq!(
        client
            .get(&url)
            .bearer_auth(&configured.token)
            .header(header::HOST, "other.conductor.example.invalid:8788")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        client
            .get(&url)
            .bearer_auth(&configured.token)
            .header(
                header::ORIGIN,
                "http://other.conductor.example.invalid:8788"
            )
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        client
            .get(&url)
            .header(header::HOST, "conductor.example.invalid:8788")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let preflight = client
        .request(reqwest::Method::OPTIONS, &url)
        .header(header::ORIGIN, "https://ui.example.invalid")
        .header(header::ACCESS_CONTROL_REQUEST_METHOD, "GET")
        .header(header::ACCESS_CONTROL_REQUEST_HEADERS, "authorization")
        .send()
        .await
        .unwrap();
    assert_eq!(
        preflight.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN],
        "https://ui.example.invalid"
    );
}

#[tokio::test]
async fn only_an_explicit_trusted_http_host_relaxes_cookie_security() {
    let server = Server::start(Some(
        json!({"trusted_http_hosts":["conductor.example.invalid:8788"],"https_origins":["https://ui.example.invalid"]}),
    ));
    let client = reqwest::Client::new();
    for (host, secure) in [
        ("conductor.example.invalid:8788", false),
        ("ui.example.invalid", true),
    ] {
        let started: Value = client
            .post(format!("{}/api/pair/start", server.base))
            .bearer_auth(&server.token)
            .header(header::HOST, host)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let paired = client
            .post(format!("{}/api/pair", server.base))
            .header(header::HOST, host)
            .json(&json!({"code":started["code"],"principal":"fixture"}))
            .send()
            .await
            .unwrap();
        assert_eq!(paired.status(), StatusCode::OK);
        let cookies: Vec<_> = paired
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .map(|h| h.to_str().unwrap())
            .collect();
        assert_eq!(cookies.len(), 2);
        assert!(cookies.iter().all(|c| c.contains("; Secure") == secure));
        assert!(cookies.iter().all(|c| c.contains("SameSite=Strict")));
        assert!(cookies.iter().any(|c| c.contains("HttpOnly")));
    }
}

#[test]
fn invalid_access_configuration_preserves_existing_token_and_does_not_start() {
    let dir = tempfile::tempdir().unwrap();
    let prior = "fixture-".repeat(8);
    fs::write(dir.path().join("serve-token"), &prior).unwrap();
    fs::write(
        dir.path().join("http-access.json"),
        r#"{"trusted_http_hosts":["*.example.invalid"]}"#,
    )
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let result = Command::new(env!("CARGO_BIN_EXE_dev-conductor"))
        .arg("--state")
        .arg(dir.path())
        .args(["serve", "--port", &port.to_string()])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert_eq!(
        fs::read_to_string(dir.path().join("serve-token")).unwrap(),
        prior
    );
    assert!(TcpListener::bind(("127.0.0.1", port)).is_ok());
}
