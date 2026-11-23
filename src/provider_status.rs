use serde::{Deserialize, Serialize};
use std::path::Path;

fn default_dispatchable() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderStatusItem {
    pub id: String,
    pub name: String,
    pub available: bool,
    pub authenticated: bool,
    #[serde(default = "default_dispatchable")]
    pub dispatchable: bool,
    pub user_email: Option<String>,
    pub tier: Option<String>,
    pub version: Option<String>,
    pub details: String,
    pub quota_type: String,
    pub reset_description: Option<String>,
    pub models: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TokenTelemetrySummary {
    pub total_prompt_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cached_tokens: u64,
    pub total_tasks_reported: usize,
    pub estimated_cost_usd: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderReport {
    #[serde(default)]
    pub probed_at_ms: u64,
    pub providers: Vec<ProviderStatusItem>,
    pub token_summary: TokenTelemetrySummary,
}

pub fn probe_all_providers(state_root: &Path, endpoint: Option<&str>) -> ProviderReport {
    probe_all_providers_with_options(state_root, endpoint, false)
}

pub fn probe_all_providers_with_options(
    state_root: &Path,
    endpoint: Option<&str>,
    refresh_cache: bool,
) -> ProviderReport {
    if refresh_cache {
        crate::agents::clear_cursor_model_cache();
        crate::agents::clear_agy_model_cache();
    }
    let providers = vec![
        probe_ollama_provider(endpoint),
        probe_cursor_provider(),
        probe_codex_provider(),
        probe_agy_provider(),
        probe_claude_provider(),
    ];

    let token_summary = collect_token_telemetry(state_root);

    ProviderReport {
        probed_at_ms: crate::now(),
        providers,
        token_summary,
    }
}

pub fn probe_ollama_provider(endpoint: Option<&str>) -> ProviderStatusItem {
    let (running, models, _) = crate::onboard::probe_ollama(endpoint);
    let mut model_names: Vec<String> = models.into_iter().map(|m| m.name).collect();
    if !model_names
        .iter()
        .any(|m| m.eq_ignore_ascii_case(crate::workflow::BUILTIN_ROUTER_MODEL))
    {
        model_names.insert(0, crate::workflow::BUILTIN_ROUTER_MODEL.into());
    }

    if running {
        ProviderStatusItem {
            id: "ollama".into(),
            name: "Local Router / Ollama".into(),
            available: true,
            authenticated: true,
            dispatchable: true,
            user_email: None,
            tier: Some("Built-in Router + Local VRAM".into()),
            version: None,
            details: format!(
                "Online · {} model(s) + builtin:router-v1",
                model_names.len().saturating_sub(1)
            ),
            quota_type: "local_hardware".into(),
            reset_description: Some(
                "Local host execution · builtin:router-v1 (0 MB footprint) + local VRAM".into(),
            ),
            models: model_names,
        }
    } else {
        ProviderStatusItem {
            id: "ollama".into(),
            name: "Local Router (Built-in)".into(),
            available: true,
            authenticated: true,
            dispatchable: true,
            user_email: None,
            tier: Some("Zero-Footprint Host Engine".into()),
            version: Some("router-v1".into()),
            details: "Active · builtin:router-v1 deterministic state machine (Ollama optional)"
                .into(),
            quota_type: "local_hardware".into(),
            reset_description: Some("0 MB VRAM · <1 ms deterministic host routing".into()),
            models: vec![crate::workflow::BUILTIN_ROUTER_MODEL.into()],
        }
    }
}

/// Longest a single CLI status probe may run before the provider is reported as unknown.
const PROBE_TIMEOUT_MS: u64 = 5_000;
const PROBE_OUTPUT_CAP: usize = 65_536;

/// Runs one read-only CLI probe against an already-resolved executable path and returns its
/// stdout on exit zero. The path matters on Windows: `Command::new("codex")` only finds
/// `codex.exe`, so npm-installed `.cmd` shims were reported as "not signed in". The deadline
/// matters everywhere: a CLI waiting on an interactive prompt must not hang the API request.
fn probe_stdout(executable: &Path, leading: &[&Path], args: &[&str]) -> Option<Vec<u8>> {
    let mut command = std::process::Command::new(executable);
    command.args(leading).args(args);
    let result =
        crate::process::bounded_output(&mut command, PROBE_TIMEOUT_MS, PROBE_OUTPUT_CAP).ok()?;
    (result.exit_code == Some(0)).then_some(result.stdout.bytes)
}

fn probe_json(executable: &Path, leading: &[&Path], args: &[&str]) -> Option<serde_json::Value> {
    serde_json::from_slice(&probe_stdout(executable, leading, args)?).ok()
}

fn probe_text(executable: &Path, args: &[&str]) -> Option<String> {
    let text = String::from_utf8_lossy(&probe_stdout(executable, &[], args)?)
        .trim()
        .to_string();
    (!text.is_empty()).then_some(text)
}

pub fn probe_cursor_provider() -> ProviderStatusItem {
    let cursor_bin = crate::onboard::find_binary_on_path("cursor-agent");
    let cursor_pair = crate::onboard::find_cursor_agent();
    if cursor_bin.is_none() && cursor_pair.is_none() {
        return ProviderStatusItem {
            id: "cursor".into(),
            name: "Cursor Agent".into(),
            available: false,
            authenticated: false,
            dispatchable: true,
            user_email: None,
            tier: None,
            version: None,
            details: "cursor-agent executable not found on PATH or LocalAppData".into(),
            quota_type: "provider_managed".into(),
            reset_description: None,
            models: Vec::new(),
        };
    }

    // Prefer the launcher on PATH; fall back to the versioned `node index.js` entry point,
    // which is the only thing present on hosts where the launcher is not on PATH.
    let (executable, leading): (&Path, Vec<&Path>) = match (&cursor_bin, &cursor_pair) {
        (Some(bin), _) => (bin.as_path(), Vec::new()),
        (None, Some((node, index_js))) => (node.as_path(), vec![index_js.as_path()]),
        (None, None) => unreachable!("handled above"),
    };

    let mut version = None;
    let mut tier = None;
    let mut email = None;
    let mut authenticated = false;

    if let Some(val) = probe_json(executable, &leading, &["about", "--format", "json"]) {
        version = val["cliVersion"].as_str().map(String::from);
        tier = val["subscriptionTier"].as_str().map(String::from);
        email = val["userEmail"].as_str().map(String::from);
        authenticated = email.is_some() && tier.is_some();
    }

    if !authenticated
        && let Some(val) = probe_json(executable, &leading, &["status", "--format", "json"])
    {
        authenticated = val["isAuthenticated"].as_bool().unwrap_or(false);
        if email.is_none() {
            email = val["userInfo"]["email"].as_str().map(String::from);
        }
    }

    let live_models = cursor_pair
        .as_ref()
        .map(|(node, index_js)| crate::agents::discover_cursor_models(node, index_js))
        .unwrap_or_default();
    let models = if live_models.is_empty() {
        vec![
            "auto".into(),
            "composer-2.5-fast".into(),
            "composer-2.5".into(),
            "claude-opus-5-thinking-high".into(),
            "gpt-5".into(),
        ]
    } else {
        live_models
    };

    let details = if authenticated {
        format!(
            "Tier: {} · User: {} · {} live models",
            tier.as_deref().unwrap_or("Active"),
            email.as_deref().unwrap_or("authenticated"),
            models.len()
        )
    } else {
        "Installed · Not signed in or credentials expired".into()
    };

    ProviderStatusItem {
        id: "cursor".into(),
        name: "Cursor Agent".into(),
        available: true,
        authenticated,
        dispatchable: true,
        user_email: email,
        tier: tier.clone().or_else(|| {
            if authenticated {
                Some("Active".into())
            } else {
                None
            }
        }),
        version,
        details,
        quota_type: "provider_managed".into(),
        reset_description: Some(
            "Managed by Cursor account (quota window not reported by CLI)".into(),
        ),
        models,
    }
}

pub fn probe_codex_provider() -> ProviderStatusItem {
    let codex_bin = crate::onboard::find_binary_on_path("codex");
    let codex_pair = crate::onboard::find_codex();
    if codex_bin.is_none() && codex_pair.is_none() {
        return ProviderStatusItem {
            id: "codex".into(),
            name: "OpenAI Codex".into(),
            available: false,
            authenticated: false,
            dispatchable: true,
            user_email: None,
            tier: None,
            version: None,
            details: "codex executable not found on PATH".into(),
            quota_type: "provider_managed".into(),
            reset_description: None,
            models: Vec::new(),
        };
    }

    let codex_exe = codex_bin
        .clone()
        .or_else(|| codex_pair.as_ref().map(|(exe, _)| exe.clone()));
    let version = codex_exe
        .as_deref()
        .and_then(|exe| probe_text(exe, &["--version"]));

    // Inspect ~/.codex/auth.json for mode
    let mut auth_mode = None;
    let mut has_tokens = false;
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(std::path::PathBuf::from);

    let mut live_models = Vec::new();
    if let Some(h) = home {
        let codex_home = h.join(".codex");
        live_models = crate::codex::discover_codex_models_in_home(&codex_home);
        let auth_file = codex_home.join("auth.json");
        if let Ok(content) = std::fs::read(&auth_file)
            && let Ok(val) = serde_json::from_slice::<serde_json::Value>(&content)
        {
            auth_mode = val["auth_mode"].as_str().map(String::from);
            // A key that is present but null/empty is a logged-out file, not a credential.
            let present = |key: &str| {
                val.get(key).is_some_and(|v| match v {
                    serde_json::Value::Null => false,
                    serde_json::Value::String(text) => !text.trim().is_empty(),
                    _ => true,
                })
            };
            has_tokens = present("tokens") || present("OPENAI_API_KEY");
        }
    }

    let models = if live_models.is_empty() {
        vec![
            "o4-mini".into(),
            "gpt-4.1".into(),
            "o3".into(),
            "gpt-5.3-codex".into(),
        ]
    } else {
        live_models
    };

    let authenticated = has_tokens;
    let tier = auth_mode.clone().or_else(|| {
        if authenticated {
            Some("ChatGPT Auth".into())
        } else {
            None
        }
    });

    let details = if authenticated {
        format!(
            "Mode: {} · {} live models",
            auth_mode.as_deref().unwrap_or("API"),
            models.len()
        )
    } else {
        "Installed · Run 'codex login'".into()
    };

    ProviderStatusItem {
        id: "codex".into(),
        name: "OpenAI Codex".into(),
        available: true,
        authenticated,
        dispatchable: true,
        user_email: None,
        tier,
        version,
        details,
        quota_type: "provider_managed".into(),
        reset_description: Some(
            "Managed by OpenAI / ChatGPT account (quota window not reported by CLI)".into(),
        ),
        models,
    }
}

pub fn probe_agy_provider() -> ProviderStatusItem {
    let agy_bin = crate::onboard::find_agy();
    let Some(agy_bin) = agy_bin else {
        return ProviderStatusItem {
            id: "agy".into(),
            name: "Antigravity (AGY)".into(),
            available: false,
            authenticated: false,
            dispatchable: true,
            user_email: None,
            tier: None,
            version: None,
            details: "agy CLI not found on PATH or ~/.local/bin".into(),
            quota_type: "provider_managed".into(),
            reset_description: None,
            models: Vec::new(),
        };
    };

    let version = probe_text(&agy_bin, &["--version"]);
    let live_models = crate::agents::discover_agy_models(&agy_bin);
    let authenticated = !live_models.is_empty();
    let details = if authenticated {
        format!(
            "Google AI Subscription · v{} · {} live models",
            version.as_deref().unwrap_or("unknown"),
            live_models.len()
        )
    } else {
        "Installed · Run 'agy' to sign in or refresh credentials".into()
    };

    ProviderStatusItem {
        id: "agy".into(),
        name: "Antigravity (AGY)".into(),
        available: true,
        authenticated,
        dispatchable: true,
        user_email: None,
        tier: if authenticated {
            Some("Google AI Subscription".into())
        } else {
            None
        },
        version,
        details,
        quota_type: "provider_managed".into(),
        reset_description: Some(
            "Managed by Google AI account (live model catalog via agy models)".into(),
        ),
        models: live_models,
    }
}

pub fn probe_claude_provider() -> ProviderStatusItem {
    let claude_bin = crate::onboard::find_binary_on_path("claude");
    let Some(claude_bin) = claude_bin else {
        return ProviderStatusItem {
            id: "claude".into(),
            name: "Claude Code (Informational)".into(),
            available: false,
            authenticated: false,
            dispatchable: false,
            user_email: None,
            tier: None,
            version: None,
            details:
                "claude CLI not found on PATH (informational only — not a dispatchable worker)"
                    .into(),
            quota_type: "informational_only".into(),
            reset_description: Some(
                "Informational CLI detection only — not a dispatchable worker backend".into(),
            ),
            models: Vec::new(),
        };
    };

    let version = probe_text(&claude_bin, &["--version"]);

    let mut logged_in = false;
    let mut email = None;
    let mut tier = None;
    if let Some(val) = probe_json(&claude_bin, &[], &["auth", "status", "--json"]) {
        logged_in = val["loggedIn"].as_bool().unwrap_or(false);
        email = val["email"].as_str().map(String::from);
        tier = val["subscriptionType"].as_str().map(String::from);
    }

    let details = if logged_in {
        format!(
            "Subscription: {} · User: {} (informational only)",
            tier.as_deref().unwrap_or("Active"),
            email.as_deref().unwrap_or("authenticated")
        )
    } else {
        "Installed · Run 'claude auth login' (informational only)".into()
    };

    ProviderStatusItem {
        id: "claude".into(),
        name: "Claude Code (Informational)".into(),
        available: true,
        authenticated: logged_in,
        dispatchable: false,
        user_email: email,
        tier,
        version,
        details,
        quota_type: "informational_only".into(),
        reset_description: Some(
            "Informational CLI detection only — not a dispatchable worker backend".into(),
        ),
        // Claude Code is reported for visibility only; it is not a dispatchable worker
        // backend, so no model catalog is advertised for it.
        models: Vec::new(),
    }
}

fn add_usage(summary: &mut TokenTelemetrySummary, usage: &serde_json::Value) {
    let count = |key: &str| usage.get(key).and_then(|v| v.as_u64());
    let (prompt, output, cached) = (
        count("prompt_tokens").or_else(|| count("input_tokens")),
        count("output_tokens"),
        count("cached_tokens").or_else(|| count("cache_read_tokens")),
    );
    if prompt.is_none() && output.is_none() && cached.is_none() {
        return;
    }
    summary.total_prompt_tokens = summary
        .total_prompt_tokens
        .saturating_add(prompt.unwrap_or(0));
    summary.total_output_tokens = summary
        .total_output_tokens
        .saturating_add(output.unwrap_or(0));
    summary.total_cached_tokens = summary
        .total_cached_tokens
        .saturating_add(cached.unwrap_or(0));
    summary.total_tasks_reported += 1;
}

/// Reads every JSON `record` in `table` of a state database, read-only. A missing file,
/// table or unreadable row contributes nothing: this is a dashboard total, not evidence.
fn for_each_record(db_path: &Path, table: &str, mut visit: impl FnMut(&serde_json::Value)) {
    if !db_path.is_file() {
        return;
    }
    let Ok(conn) =
        rusqlite::Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
    else {
        return;
    };
    let Ok(mut stmt) = conn.prepare(&format!("SELECT record FROM {table}")) else {
        return;
    };
    let Ok(rows) = stmt.query_map([], |row| row.get::<_, String>(0)) else {
        return;
    };
    for record in rows.flatten() {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&record) {
            visit(&value);
        }
    }
}

fn add_inference_db(
    summary: &mut TokenTelemetrySummary,
    priced_pairs: &mut Vec<(
        crate::inference::config::Target,
        crate::inference::types::Usage,
    )>,
    db_path: &Path,
) {
    for_each_record(db_path, "jobs", |job| {
        let config = serde_json::from_value::<crate::inference::config::Config>(
            job.get("config").cloned().unwrap_or_default(),
        )
        .ok();
        for generation in job["generations"].as_array().into_iter().flatten() {
            add_usage(summary, &generation["usage"]);
            if let Some(cfg) = &config
                && let Some(target_id) = generation["resolved"]["target_id"].as_str()
                && let Ok(target) = cfg.target(target_id)
                && target.api_pricing.is_some()
            {
                let u = &generation["usage"];
                let usage = crate::inference::types::Usage {
                    prompt_tokens: u.get("prompt_tokens").and_then(|v| v.as_u64()),
                    output_tokens: u.get("output_tokens").and_then(|v| v.as_u64()),
                    cached_tokens: u.get("cached_tokens").and_then(|v| v.as_u64()),
                    native_timings_ns: u
                        .get("native_timings_ns")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null),
                };
                priced_pairs.push((target.clone(), usage));
            }
        }
    });
}

/// Sums the token usage the host actually recorded: delegated-CLI protocol receipts on
/// `tasks` attempts, plus every direct-inference generation in the root, per-workflow
/// planner and per-orchestration job stores. Wires `telemetry::api_cost` (`O9`) when
/// configured inference targets carry `api_pricing`.
pub fn collect_token_telemetry(state_root: &Path) -> TokenTelemetrySummary {
    let mut summary = TokenTelemetrySummary::default();
    let mut priced_pairs = Vec::new();

    for_each_record(&state_root.join("state.sqlite"), "tasks", |task| {
        for attempt in task["attempts"].as_array().into_iter().flatten() {
            add_usage(
                &mut summary,
                &attempt["process_receipt"]["delegated"]["protocol"]["usage"],
            );
        }
    });

    let state_db = state_root.join("state.sqlite");
    if state_db.is_file()
        && let Ok(conn) = rusqlite::Connection::open(&state_db)
        && conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='agent_tasks'",
                [],
                |_| Ok(()),
            )
            .is_ok()
    {
        drop(conn);
        if let Ok(mut store) = crate::routing_metrics::RoutingMetricsStore::open(state_root)
            && let Ok(board) = store.scoreboard()
        {
            let report = board.analytics_report();
            summary.total_prompt_tokens = summary
                .total_prompt_tokens
                .saturating_add(report.total_input_tokens);
            summary.total_output_tokens = summary
                .total_output_tokens
                .saturating_add(report.total_output_tokens);
            summary.total_cached_tokens = summary
                .total_cached_tokens
                .saturating_add(report.total_cached_tokens);
            let reported = report
                .recent_outcomes
                .iter()
                .filter(|r| r.input_tokens > 0 || r.output_tokens > 0 || r.cached_tokens > 0)
                .count();
            summary.total_tasks_reported = summary.total_tasks_reported.saturating_add(reported);
        }
    }

    add_inference_db(
        &mut summary,
        &mut priced_pairs,
        &state_root.join("inference.sqlite"),
    );
    for scoped in ["workflow-inference", "orchestration-inference"] {
        let Ok(entries) = std::fs::read_dir(state_root.join(scoped)) else {
            continue;
        };
        for entry in entries.flatten() {
            add_inference_db(
                &mut summary,
                &mut priced_pairs,
                &entry.path().join("inference.sqlite"),
            );
        }
    }

    if !priced_pairs.is_empty() {
        let refs: Vec<(
            &crate::inference::config::Target,
            &crate::inference::types::Usage,
        )> = priced_pairs.iter().map(|(t, u)| (t, u)).collect();
        let cost_val = crate::telemetry::api_cost(&refs);
        if let (Some(currency), Some(amount)) = (
            cost_val.get("currency").and_then(|c| c.as_str()),
            cost_val.get("amount").and_then(|a| a.as_str()),
        ) {
            summary.estimated_cost_usd = Some(if currency == "USD" {
                format!("${amount}")
            } else {
                format!("{currency} {amount}")
            });
        }
    }

    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_probe_all_providers_does_not_panic() {
        let temp = tempfile::tempdir().unwrap();
        let report = probe_all_providers(temp.path(), None);
        assert_eq!(report.providers.len(), 5);
        assert!(report.providers.iter().any(|p| p.id == "ollama"));
        assert!(report.providers.iter().any(|p| p.id == "cursor"));
        assert!(report.providers.iter().any(|p| p.id == "codex"));
        assert!(report.providers.iter().any(|p| p.id == "agy"));
        assert!(report.providers.iter().any(|p| p.id == "claude"));
    }

    #[test]
    fn test_collect_token_telemetry_empty_dir() {
        let temp = tempfile::tempdir().unwrap();
        let summary = collect_token_telemetry(temp.path());
        assert_eq!(summary.total_prompt_tokens, 0);
        assert_eq!(summary.total_output_tokens, 0);
        assert_eq!(summary.total_cached_tokens, 0);
        assert_eq!(summary.total_tasks_reported, 0);
        assert!(summary.estimated_cost_usd.is_none());
    }

    #[test]
    fn telemetry_reads_the_real_record_schema() {
        let temp = tempfile::tempdir().unwrap();
        let state = rusqlite::Connection::open(temp.path().join("state.sqlite")).unwrap();
        state
            .execute_batch(
                "CREATE TABLE tasks(id TEXT PRIMARY KEY, revision INTEGER NOT NULL, record TEXT NOT NULL);",
            )
            .unwrap();
        let task = serde_json::json!({"attempts":[
            {"process_receipt":{"delegated":{"protocol":{"usage":{"prompt_tokens":100,"output_tokens":20,"cached_tokens":40}}}}},
            {"process_receipt":null}
        ]});
        state
            .execute("INSERT INTO tasks VALUES ('t', 1, ?1)", [task.to_string()])
            .unwrap();

        let planner = temp.path().join("workflow-inference").join("wf-1");
        std::fs::create_dir_all(&planner).unwrap();
        for db in [
            temp.path().join("inference.sqlite"),
            planner.join("inference.sqlite"),
        ] {
            let jobs = rusqlite::Connection::open(db).unwrap();
            jobs.execute_batch(
                "CREATE TABLE jobs(id TEXT PRIMARY KEY, revision INTEGER NOT NULL, record TEXT NOT NULL);",
            )
            .unwrap();
            let job = serde_json::json!({"generations":[
                {"usage":{"prompt_tokens":7,"output_tokens":3,"cached_tokens":null}},
                {"usage":{"prompt_tokens":null,"output_tokens":null,"cached_tokens":null}}
            ]});
            jobs.execute("INSERT INTO jobs VALUES ('j', 1, ?1)", [job.to_string()])
                .unwrap();
        }

        let summary = collect_token_telemetry(temp.path());
        assert_eq!(summary.total_prompt_tokens, 114);
        assert_eq!(summary.total_output_tokens, 26);
        assert_eq!(summary.total_cached_tokens, 40);
        // One delegated attempt plus one reporting generation per inference database.
        assert_eq!(summary.total_tasks_reported, 3);
        assert!(summary.estimated_cost_usd.is_none());
    }

    #[test]
    fn telemetry_computes_estimated_cost_for_priced_openai_local_targets() {
        use crate::{
            inference::{
                config::{Config, EndpointAccess, Target},
                types::{Capabilities, ProviderKind},
            },
            telemetry::ApiPricing,
        };

        let temp = tempfile::tempdir().unwrap();
        let target = Target {
            id: "priced-local".into(),
            provider: ProviderKind::OpenaiLocal,
            endpoint: "http://127.0.0.1:8000".into(),
            endpoint_access: EndpointAccess::Loopback,
            model: "local-v1".into(),
            expected_artifact_digest: None,
            context_limit: 8192,
            output_limit: 1024,
            capabilities: Capabilities {
                streaming: true,
                ..Default::default()
            },
            capability_source: "test".into(),
            priority: 1,
            resource_id: "gpu-0".into(),
            extensions: None,
            api_pricing: Some(ApiPricing {
                currency: "USD".into(),
                input_micros_per_million: 2_000_000,
                output_micros_per_million: 8_000_000,
                cached_input_micros_per_million: Some(500_000),
            }),
        };
        let cfg = Config {
            schema_version: 1,
            default_target: "priced-local".into(),
            hardware_id: "hw".into(),
            admission_db: temp.path().join("admission.sqlite"),
            max_active_per_resource: 1,
            registry_max_age_ms: 60_000,
            fallback_before_dispatch: false,
            targets: vec![target],
        };
        let job = serde_json::json!({
            "config": cfg,
            "generations": [{
                "resolved": {"target_id": "priced-local"},
                "usage": {
                    "prompt_tokens": 1_000_000,
                    "output_tokens": 500_000,
                    "total_tokens": 1_500_000,
                    "cached_tokens": 0
                }
            }]
        });

        let jobs_db = rusqlite::Connection::open(temp.path().join("inference.sqlite")).unwrap();
        jobs_db
            .execute_batch(
                "CREATE TABLE jobs(id TEXT PRIMARY KEY, revision INTEGER NOT NULL, record TEXT NOT NULL);",
            )
            .unwrap();
        jobs_db
            .execute(
                "INSERT INTO jobs VALUES ('job-1', 1, ?1)",
                [job.to_string()],
            )
            .unwrap();

        let summary = collect_token_telemetry(temp.path());
        assert_eq!(summary.total_prompt_tokens, 1_000_000);
        assert_eq!(summary.total_output_tokens, 500_000);
        // 1M input @ $2/M + 0.5M output @ $8/M = $6.000000
        assert_eq!(summary.estimated_cost_usd.as_deref(), Some("$6.000000"));
    }
}
