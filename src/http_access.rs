//! Deployment-specific browser access is operator configuration, never a source default.
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{collections::BTreeSet, fs, path::Path};

const FRONTEND_ORIGINS: &[&str] = &[
    "http://localhost:8080",
    "http://127.0.0.1:8080",
    "http://tauri.localhost",
    "tauri://localhost",
];

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Config {
    trusted_http_hosts: Vec<String>,
    https_origins: Vec<String>,
}

pub struct AccessPolicy {
    origins: Vec<String>,
    external_hosts: BTreeSet<String>,
    trusted_http_hosts: BTreeSet<String>,
}

impl AccessPolicy {
    pub fn default_local() -> Self {
        Self::from_config(Config::default()).expect("default local HTTP access policy is valid")
    }

    pub fn load(root: &Path) -> Result<Self> {
        let path = root.join("http-access.json");
        let config: Config = match fs::metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(error) => return Err(error.into()),
            Ok(meta) => {
                ensure!(
                    meta.is_file() && meta.len() <= 16384,
                    "HTTP access configuration must be a file up to 16 KiB"
                );
                let raw = fs::read(&path)?;
                let body = raw.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&raw);
                serde_json::from_slice(body).context("invalid http-access.json")?
            }
        };
        Self::from_config(config)
    }

    fn from_config(config: Config) -> Result<Self> {
        ensure!(
            config.trusted_http_hosts.len() <= 16 && config.https_origins.len() <= 16,
            "HTTP access configuration exceeds bounds"
        );
        let mut origins: BTreeSet<String> =
            FRONTEND_ORIGINS.iter().map(|s| s.to_string()).collect();
        let mut external_hosts = BTreeSet::new();
        let mut trusted_http_hosts = BTreeSet::new();
        for host in config.trusted_http_hosts {
            ensure!(
                !host.is_empty() && host.len() <= 255 && !host.contains('*'),
                "invalid trusted HTTP host"
            );
            let origin = format!("http://{host}");
            let url = reqwest::Url::parse(&origin).context("invalid trusted HTTP host")?;
            ensure!(
                url.host_str().is_some()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none()
                    && url.as_str() == format!("{origin}/"),
                "trusted HTTP hosts require an exact canonical host[:port], without paths or credentials"
            );
            ensure!(
                trusted_http_hosts.insert(host.clone()),
                "duplicate trusted HTTP host"
            );
            external_hosts.insert(host);
            origins.insert(origin);
        }
        for origin in config.https_origins {
            ensure!(
                origin.len() <= 2048 && !origin.contains('*'),
                "invalid HTTPS origin"
            );
            let url = reqwest::Url::parse(&origin).context("invalid HTTPS origin")?;
            ensure!(
                url.scheme() == "https"
                    && url.host_str().is_some()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.path() == "/"
                    && url.query().is_none()
                    && url.fragment().is_none()
                    && url.origin().ascii_serialization() == origin,
                "HTTPS origins require an exact canonical origin without paths or credentials"
            );
            let host = url
                .as_str()
                .strip_prefix("https://")
                .unwrap()
                .strip_suffix('/')
                .unwrap()
                .to_string();
            ensure!(
                !trusted_http_hosts.contains(&host),
                "host cannot select both trusted HTTP and HTTPS cookie policies"
            );
            external_hosts.insert(host);
            ensure!(origins.insert(origin), "duplicate HTTPS origin");
        }
        Ok(Self {
            origins: origins.into_iter().collect(),
            external_hosts,
            trusted_http_hosts,
        })
    }

    pub fn origins(&self) -> &[String] {
        &self.origins
    }
    pub fn origins_for_port(&self, port: u16) -> Vec<String> {
        let mut all: BTreeSet<String> = self.origins.iter().cloned().collect();
        all.insert(format!("http://localhost:{port}"));
        all.insert(format!("http://127.0.0.1:{port}"));
        all.insert(format!("http://[::1]:{port}"));
        all.into_iter().collect()
    }
    pub fn external_hosts(&self) -> &BTreeSet<String> {
        &self.external_hosts
    }
    pub fn allows_host(&self, host: &str, port: u16) -> bool {
        let host = host.to_ascii_lowercase();
        host == format!("localhost:{port}")
            || host == format!("127.0.0.1:{port}")
            || host == format!("[::1]:{port}")
            || self.external_hosts.contains(&host)
    }
    pub fn allows_origin(&self, origin: &str) -> bool {
        self.origins.iter().any(|value| value == origin)
    }
    pub fn allows_origin_for_port(&self, origin: &str, port: u16) -> bool {
        self.allows_origin(origin)
            || origin == format!("http://localhost:{port}")
            || origin == format!("http://127.0.0.1:{port}")
            || origin == format!("http://[::1]:{port}")
    }
    pub fn secure_cookies(&self, host: Option<&str>) -> bool {
        !host.is_some_and(|h| self.trusted_http_hosts.contains(&h.to_ascii_lowercase()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_are_local_and_http_relaxation_is_explicit_and_exact() {
        let policy = AccessPolicy::from_config(Config::default()).unwrap();
        assert!(policy.allows_host("localhost:9876", 9876));
        assert!(policy.allows_origin("tauri://localhost"));
        assert!(policy.allows_origin_for_port("http://127.0.0.1:9876", 9876));
        assert!(policy.allows_origin_for_port("http://localhost:9876", 9876));
        assert!(!policy.allows_origin_for_port("http://127.0.0.1:9999", 9876));
        assert!(!policy.allows_host("conductor.example.invalid:8788", 9876));
        assert!(policy.secure_cookies(Some("conductor.example.invalid:8788")));
        let policy = AccessPolicy::from_config(Config {
            trusted_http_hosts: vec![
                "conductor.example.invalid:8788".into(),
                "10.0.0.50:8787".into(),
            ],
            https_origins: vec!["https://ui.example.invalid".into()],
        })
        .unwrap();
        assert!(policy.allows_origin("http://conductor.example.invalid:8788"));
        assert!(policy.allows_origin("http://10.0.0.50:8787"));
        assert!(policy.allows_host("10.0.0.50:8787", 8787));
        assert!(policy.allows_host("CONDUCTOR.EXAMPLE.INVALID:8788", 8787));
        assert!(!policy.allows_origin("http://other.conductor.example.invalid:8788"));
        assert!(!policy.secure_cookies(Some("conductor.example.invalid:8788")));
        assert!(!policy.secure_cookies(Some("10.0.0.50:8787")));
        assert!(policy.secure_cookies(Some("ui.example.invalid")));
        assert!(policy.secure_cookies(None));
    }

    #[test]
    fn malformed_wildcard_or_ambiguous_policies_fail_closed() {
        for config in [
            json!({"trusted_http_hosts":["*.example.invalid:8788"]}),
            json!({"trusted_http_hosts":["user:password@conductor.example.invalid"]}),
            json!({"trusted_http_hosts":["conductor.example.invalid/path"]}),
            json!({"trusted_http_hosts":["conductor.example.invalid?query"]}),
            json!({"trusted_http_hosts":["conductor.example.invalid","conductor.example.invalid"]}),
            json!({"https_origins":["http://conductor.example.invalid"]}),
            json!({"https_origins":["https://conductor.example.invalid/path"]}),
            json!({"https_origins":["https://user:password@conductor.example.invalid"]}),
            json!({"trusted_http_hosts":["conductor.example.invalid"],"https_origins":["https://conductor.example.invalid"]}),
        ] {
            assert!(
                AccessPolicy::from_config(serde_json::from_value(config.clone()).unwrap()).is_err(),
                "{config}"
            );
        }
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("http-access.json"),
            r#"{"allow_everything":true}"#,
        )
        .unwrap();
        assert!(AccessPolicy::load(dir.path()).is_err());
    }
}
