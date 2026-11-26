mod workflow_ui;

use dioxus::prelude::*;
use serde::{Deserialize, Serialize};
use wasm_bindgen::prelude::*;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct ApiConfig {
    base: String,
    token: String,
}

#[derive(Clone, Debug, PartialEq)]
enum AuthMode {
    Bearer(String),
    Session { principal: String },
}

#[derive(Clone, Debug, PartialEq)]
struct ApiConnection {
    base: String,
    auth: AuthMode,
}

impl ApiConnection {
    fn from_bearer(config: ApiConfig) -> Self {
        Self {
            base: config.base,
            auth: AuthMode::Bearer(config.token),
        }
    }

    fn session_principal(&self) -> Option<&str> {
        match &self.auth {
            AuthMode::Session { principal } => Some(principal.as_str()),
            AuthMode::Bearer(_) => None,
        }
    }

    fn is_bearer(&self) -> bool {
        matches!(self.auth, AuthMode::Bearer(_))
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
struct MeResponse {
    auth: String,
    principal: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
struct PairStartResponse {
    code: String,
    expires_at: u64,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
struct PairResponse {
    principal: String,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
struct SessionSummary {
    id: String,
    principal: String,
    created_at: u64,
    last_seen_at: u64,
    expires_at: u64,
}

// Only resolves inside the actual Tauri webview, which injects `window.__TAURI__` - a
// plain `dx serve` tab (no Tauri) has no such global, so `invoke` rejects and callers
// fall back to the manual connect form. Tauri v2's bridge lives at
// `window.__TAURI__.core.invoke`, not the v1 `window.__TAURI__.invoke`.
#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"], catch)]
    async fn invoke(cmd: &str, args: JsValue) -> Result<JsValue, JsValue>;

    #[wasm_bindgen(js_namespace = console, js_name = error)]
    fn console_error(msg: &str);
}

/// `wasm_bindgen`'s multi-level `js_namespace` path resolves `window.__TAURI__.core` as a
/// plain, unguarded property chain before ever calling `invoke`; under plain `dx serve`
/// `window.__TAURI__` is `undefined`, so that resolution throws a *synchronous* JS
/// TypeError the `catch` attribute never sees (it only wraps the Promise `invoke` itself
/// returns). That raw throw was observed corrupting wasm-bindgen's futures executor
/// (`RefCell already borrowed` panicking on the next unrelated `spawn`), not just failing
/// this one call - so `__TAURI__`'s presence is checked with `js_sys::Reflect::get`
/// first, which never throws for a missing property, before `invoke` is touched at all.
fn tauri_available() -> bool {
    let Some(window) = web_sys::window() else {
        return false;
    };
    js_sys::Reflect::get(&window, &JsValue::from_str("__TAURI__"))
        .map(|value| !value.is_undefined())
        .unwrap_or(false)
}

fn is_allowed_auto_base(base: &str, page_host: Option<&str>) -> bool {
    let trimmed = base.trim().trim_end_matches('/');
    let Some(rest) = trimmed
        .strip_prefix("http://")
        .or_else(|| trimmed.strip_prefix("https://"))
    else {
        return false;
    };
    let host_port = rest.split('/').next().unwrap_or_default();
    let host = host_port
        .rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or(host_port)
        .trim_matches(['[', ']']);
    host == "127.0.0.1"
        || host.eq_ignore_ascii_case("localhost")
        || host == "::1"
        || page_host.is_some_and(|ph| ph.eq_ignore_ascii_case(host))
}

fn browser_query_api_config() -> Option<ApiConfig> {
    let window = web_sys::window()?;
    let location = js_sys::Reflect::get(&window, &JsValue::from_str("location")).ok()?;
    let hash = js_sys::Reflect::get(&location, &JsValue::from_str("hash"))
        .ok()
        .and_then(|v| v.as_string())
        .unwrap_or_default();
    let search = js_sys::Reflect::get(&location, &JsValue::from_str("search"))
        .ok()
        .and_then(|v| v.as_string())
        .unwrap_or_default();
    let page_hostname = js_sys::Reflect::get(&location, &JsValue::from_str("hostname"))
        .ok()
        .and_then(|v| v.as_string());

    // Prefer URL fragment (`#token=...`) so the bearer token is never sent in HTTP request
    // lines or Referer headers (`O3`); retain query-string fallback for older links.
    let raw_params = if !hash.trim_start_matches('#').is_empty() {
        hash.trim_start_matches('#')
    } else {
        search.trim_start_matches('?')
    };
    if raw_params.is_empty() {
        return None;
    }
    let mut token = None;
    let mut base = default_api_base();
    for pair in raw_params.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == "token" && !v.trim().is_empty() {
                token = Some(v.trim().to_string());
            } else if k == "base"
                && !v.trim().is_empty()
                && is_allowed_auto_base(v, page_hostname.as_deref())
            {
                base = v.trim().to_string();
            }
        }
    }
    let token = token?;
    // Drop the fragment/query string from the address bar and session history once read
    // so the credential is not left in a bookmark, copied link or restored tab.
    if let (Ok(history), Ok(path)) = (
        js_sys::Reflect::get(&window, &JsValue::from_str("history")),
        js_sys::Reflect::get(&location, &JsValue::from_str("pathname")),
    ) {
        if let Ok(replace) = js_sys::Reflect::get(&history, &JsValue::from_str("replaceState"))
            .and_then(|value| value.dyn_into::<js_sys::Function>())
        {
            let _ = replace.call3(&history, &JsValue::NULL, &JsValue::from_str(""), &path);
        }
    }
    Some(ApiConfig { base, token })
}

fn browser_page_origin_and_host() -> Option<(String, String, String)> {
    let window = web_sys::window()?;
    let location = window.location();
    let origin = location.origin().ok()?;
    let hostname = location.hostname().ok()?;
    let port = location.port().ok().unwrap_or_default();
    if origin.starts_with("http://") || origin.starts_with("https://") {
        Some((origin, hostname, port))
    } else {
        None
    }
}

fn browser_is_loopback() -> bool {
    let Some((_, host, _)) = browser_page_origin_and_host() else {
        return true;
    };
    host == "127.0.0.1"
        || host.eq_ignore_ascii_case("localhost")
        || host == "::1"
        || host == "[::1]"
        || host.eq_ignore_ascii_case("tauri.localhost")
}

/// Default API base URL for browser sessions:
/// - On a split local dev/UI server (`localhost:8080` or `127.0.0.1:8080`), point to the
///   companion API on `127.0.0.1:8787`.
/// - On any other HTTP/HTTPS origin (unified `serve --ui-assets` over Tailscale, LAN IP,
///   or loopback `:8787`), default to the page's own origin so cookies are same-origin and
///   phones never try to reach their own `127.0.0.1`.
fn default_api_base() -> String {
    if let Some((origin, host, port)) = browser_page_origin_and_host() {
        let is_split_local_ui =
            (host == "127.0.0.1" || host.eq_ignore_ascii_case("localhost")) && port == "8080";
        if !is_split_local_ui && !host.eq_ignore_ascii_case("tauri.localhost") {
            return origin;
        }
    }
    "http://127.0.0.1:8787".to_string()
}

const SAVED_BEARER_KEY: &str = "clatterforge.bearer.v1";

/// The bearer token is the master credential (it can mint pairing codes and revoke
/// sessions), so it lives only in per-tab `sessionStorage`: it survives a reload but not
/// closing the tab, and nothing is left in the browser profile afterwards. New windows get
/// the token from the launcher's `#token=` handoff.
fn bearer_storage() -> Option<web_sys::Storage> {
    web_sys::window()?.session_storage().ok()?
}

/// Earlier builds kept the token in `localStorage`, which persists in the browser profile.
/// Remove any such copy; it is never read again.
fn scrub_legacy_bearer() {
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = storage.remove_item(SAVED_BEARER_KEY);
    }
}

fn load_saved_bearer() -> Option<ApiConfig> {
    scrub_legacy_bearer();
    let storage = bearer_storage()?;
    let raw = storage.get_item(SAVED_BEARER_KEY).ok()??;
    let parsed: ApiConfig = serde_json::from_str(&raw).ok()?;
    let page_host = browser_page_origin_and_host().map(|(_, host, _)| host);
    if parsed.token.trim().is_empty() || !is_allowed_auto_base(&parsed.base, page_host.as_deref()) {
        return None;
    }
    Some(ApiConfig {
        base: parsed.base.trim().to_string(),
        token: parsed.token.trim().to_string(),
    })
}

fn save_bearer(base: &str, token: &str) {
    let trimmed_base = base.trim();
    let trimmed_token = token.trim();
    if trimmed_base.is_empty() || trimmed_token.is_empty() {
        return;
    }
    let page_host = browser_page_origin_and_host().map(|(_, host, _)| host);
    if !is_allowed_auto_base(trimmed_base, page_host.as_deref()) {
        return;
    }
    if let Some(storage) = bearer_storage() {
        let cfg = ApiConfig {
            base: trimmed_base.to_string(),
            token: trimmed_token.to_string(),
        };
        if let Ok(serialized) = serde_json::to_string(&cfg) {
            let _ = storage.set_item(SAVED_BEARER_KEY, &serialized);
        }
    }
}

fn clear_saved_bearer() {
    scrub_legacy_bearer();
    if let Some(storage) = bearer_storage() {
        let _ = storage.remove_item(SAVED_BEARER_KEY);
    }
}

fn read_cookie(name: &str) -> Option<String> {
    let document = web_sys::window()?.document()?;
    let html_doc = document.dyn_into::<web_sys::HtmlDocument>().ok()?;
    let cookies = html_doc.cookie().ok()?;
    cookies.split(';').find_map(|part| {
        let (key, value) = part.trim().split_once('=')?;
        (key == name).then(|| value.to_string())
    })
}

async fn get_api_config() -> Result<ApiConfig, String> {
    if !tauri_available() {
        return browser_query_api_config()
            .ok_or_else(|| "not running inside the Tauri shell".to_string());
    }
    let result = invoke("get_api_config", JsValue::NULL)
        .await
        .map_err(|err| format!("tauri invoke failed: {err:?}"))?;
    serde_wasm_bindgen::from_value(result).map_err(|err| format!("bad invoke response: {err}"))
}

const FAVICON: Asset = asset!("/assets/favicon.ico");
const BRAND_LOGO: Asset = asset!("/assets/clatterforge-logo.svg");
const APPLE_TOUCH_ICON: Asset = asset!("/assets/apple-touch-icon.png");
const MAIN_CSS: Asset = asset!("/assets/main.css");
const TAILWIND_CSS: Asset = asset!("/assets/tailwind.css");

/// Mirrors `board::Row` in the core crate. Duplicated rather than shared because the core
/// crate pulls in rusqlite/windows-sys/native tokio sockets that don't target wasm32; a
/// small shared types-only crate is a reasonable future extraction if this duplication
/// grows past a couple of structs, not before.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub(crate) struct TaskRow {
    pub(crate) kind: String,
    pub(crate) id: String,
    pub(crate) status: String,
    pub(crate) backend: String,
    pub(crate) units: usize,
    pub(crate) review: Option<String>,
    pub(crate) objective: String,
    #[serde(default)]
    pub(crate) step_label: Option<String>,
    #[serde(default)]
    pub(crate) alert: Option<String>,
    #[serde(default)]
    pub(crate) children: Vec<TaskRow>,
}

/// Mirrors `board::Preview`: a bounded slice of a captured artifact (diff, verifier
/// stdout/stderr, or - while an attempt is still active - its live `.part` tail).
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub(crate) struct Preview {
    pub(crate) label: String,
    pub(crate) shown_bytes: usize,
    pub(crate) total_bytes: u64,
    pub(crate) truncated: bool,
    pub(crate) text: String,
}

/// Mirrors `board::AttemptDetail`. `previews` is only ever populated for the latest
/// attempt (the backend's own convention - see `service::fake_last_previews`/
/// `orchestrate_last_previews`), so earlier attempts just carry an empty vec.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub(crate) struct AttemptInfo {
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) started_at: Option<u64>,
    #[serde(default)]
    pub(crate) ended_at: Option<u64>,
    pub(crate) outcome: Option<String>,
    pub(crate) error: Option<String>,
    #[serde(default)]
    pub(crate) previews: Vec<Preview>,
}

/// Mirrors `board::DependencyStatus`: one dependency edge plus its own current status.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub(crate) struct DependencyStatus {
    pub(crate) id: String,
    pub(crate) status: String,
}

/// Mirrors `stop::StopState`. Fetched independently of any selected task - the global
/// stop is a standalone control, not part of any one task's detail.
#[derive(Clone, Debug, PartialEq, Deserialize)]
struct StopState {
    stopped: bool,
    by: Option<String>,
    reason: Option<String>,
}

/// Mirrors `service::StopAllOutcome` - what `POST /api/stop` actually returns (the new
/// switch state plus which tasks it swept), unlike `POST /api/stop/resume`, which only
/// ever returns a bare `StopState`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
struct StopAllOutcome {
    state: StopState,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
struct ClearSkipped {
    id: String,
    reason: String,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
struct ClearOutcome {
    deleted_ids: Vec<String>,
    skipped: Vec<ClearSkipped>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
struct DeleteOutcome {
    deleted_id: String,
}

/// Mirrors `board::Detail` for the workflow/task inspector, same duplication
/// rationale as `TaskRow`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub(crate) struct TaskDetail {
    pub(crate) kind: String,
    pub(crate) id: String,
    pub(crate) revision: u64,
    pub(crate) assignment_digest: String,
    pub(crate) status: String,
    #[serde(default)]
    pub(crate) backend: String,
    pub(crate) objective: String,
    pub(crate) repository: String,
    #[serde(default)]
    pub(crate) branch: Option<String>,
    #[serde(default)]
    pub(crate) workspace: String,
    #[serde(default)]
    pub(crate) agent: String,
    #[serde(default)]
    pub(crate) allowed_scope: Vec<String>,
    pub(crate) approved_by: Option<String>,
    #[serde(default)]
    pub(crate) approved_at: Option<u64>,
    pub(crate) attempts: Vec<AttemptInfo>,
    pub(crate) task_error: Option<String>,
    pub(crate) reviewer: Option<String>,
    pub(crate) accepted: Option<bool>,
    #[serde(default)]
    pub(crate) reviewed_at: Option<u64>,
    pub(crate) evidence_digest: Option<String>,
    #[serde(default)]
    pub(crate) depends_on: Vec<DependencyStatus>,
    #[serde(default)]
    pub(crate) conflict: Option<String>,
    #[serde(default)]
    pub(crate) workspace_warning: Option<String>,
    #[serde(default)]
    pub(crate) progress: Option<AgentProgress>,
    #[serde(default)]
    pub(crate) routing: Vec<RoutingEvent>,
    #[serde(default)]
    pub(crate) workflow: Option<WorkflowView>,
    #[serde(default)]
    pub(crate) integration: Option<IntegrationView>,
    #[serde(default)]
    pub(crate) budget: Vec<String>,
    #[serde(default)]
    pub(crate) parent_workflow_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub(crate) struct IntegrationView {
    pub(crate) revision: u64,
    pub(crate) status: String,
    #[serde(default)]
    pub(crate) target: String,
    #[serde(default)]
    pub(crate) worktree_branch: Option<String>,
    pub(crate) plan_digest: String,
    pub(crate) repository: IntegrationRepository,
    pub(crate) files: Vec<IntegrationChange>,
    pub(crate) error: Option<String>,
    pub(crate) events: Vec<IntegrationEvent>,
}
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub(crate) struct IntegrationEvent {
    pub(crate) stage: String,
    pub(crate) handler: String,
    pub(crate) by: String,
}
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub(crate) struct IntegrationRepository {
    pub(crate) checkout: String,
    pub(crate) base: String,
    pub(crate) branch: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub(crate) struct IntegrationChange {
    pub(crate) path: String,
    pub(crate) before_digest: Option<String>,
    pub(crate) after_digest: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub(crate) struct RoutingEvent {
    pub(crate) stage: String,
    pub(crate) handler: String,
    pub(crate) status: String,
    pub(crate) process_id: Option<u32>,
    pub(crate) note: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub(crate) struct WorkflowView {
    pub(crate) phase: String,
    pub(crate) planner_model: String,
    pub(crate) decisions: Vec<WorkflowDecision>,
    pub(crate) steps: Vec<WorkflowStep>,
    pub(crate) summary: Option<String>,
    pub(crate) planner_progress: Option<PlannerProgress>,
    #[serde(default)]
    pub(crate) events: Vec<WorkflowEvent>,
    #[serde(default)]
    pub(crate) agentlets: Vec<AgentletView>,
    #[serde(default)]
    pub(crate) pending_agentlet_gates: Vec<String>,
    #[serde(default)]
    pub(crate) git_reads: Vec<serde_json::Value>,
}
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub(crate) struct AgentletView {
    pub(crate) worker: String,
    pub(crate) definition: AgentletDefinition,
}
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub(crate) struct AgentletDefinition {
    pub(crate) id: String,
    pub(crate) version: u32,
    pub(crate) purpose: String,
    pub(crate) instructions: String,
    pub(crate) capability: String,
    pub(crate) completion_gate: bool,
}
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub(crate) struct WorkflowEvent {
    pub(crate) revision: u64,
    pub(crate) stage: String,
}
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub(crate) struct PlannerProgress {
    pub(crate) status: String,
    pub(crate) elapsed_ms: u64,
    pub(crate) output_bytes: usize,
}
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub(crate) struct WorkflowDecision {
    #[serde(default)]
    pub(crate) decision: serde_json::Value,
    pub(crate) error: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub(crate) struct WorkflowStep {
    pub(crate) worker: String,
    pub(crate) task_id: String,
    #[serde(default)]
    pub(crate) evidence_digest: Option<String>,
    #[serde(default)]
    pub(crate) checkpoint: Option<serde_json::Value>,
    #[serde(default)]
    pub(crate) result: Option<serde_json::Value>,
}
#[derive(Clone, Debug, PartialEq, Deserialize)]
struct WorkflowProfile {
    name: String,
    repository: String,
    model: Option<String>,
    max_steps: usize,
    #[serde(default)]
    decision_timeout_ms: Option<u64>,
    #[serde(default)]
    generation_timeout_ms: Option<u64>,
    #[serde(default)]
    generation_idle_timeout_ms: Option<u64>,
    #[serde(default)]
    verification_timeout_ms: Option<u64>,
    #[serde(default)]
    soft_timeouts: bool,
    #[serde(default)]
    require_clean_checkout: bool,
    #[serde(default)]
    low_spec: bool,
    #[serde(default)]
    scope: Vec<String>,
    #[serde(default)]
    indexed_files: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
struct ProfileRepoStatus {
    base: String,
    branch: Option<String>,
    clean: bool,
    #[serde(default)]
    dirty_paths: Vec<String>,
    #[serde(default)]
    unsupported_index_entries: bool,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
struct OnboardProgressUi {
    job_id: String,
    profile_name: String,
    stage: String,
    detail: String,
    percent: u8,
    indexed_files: usize,
    total_index_files: usize,
    finished: bool,
    error: Option<String>,
}

fn spawn_poll_onboard_progress(
    conn: ApiConnection,
    job_id: String,
    active_job: Signal<Option<String>>,
    mut progress_sig: Signal<Option<OnboardProgressUi>>,
) {
    spawn(async move {
        gloo_timers::future::TimeoutFuture::new(150).await;
        let path = format!("/api/onboard/progress/{job_id}");
        for _ in 0..300 {
            if active_job.peek().as_deref() != Some(job_id.as_str()) {
                break;
            }
            if let Ok(snap) = api_get::<OnboardProgressUi>(&conn, &path).await {
                if active_job.peek().as_deref() != Some(job_id.as_str()) {
                    break;
                }
                let done = snap.finished;
                progress_sig.set(Some(snap));
                if done {
                    break;
                }
            }
            gloo_timers::future::TimeoutFuture::new(180).await;
        }
    });
}

#[component]
fn OnboardProgressBar(progress: OnboardProgressUi) -> Element {
    let pct = progress.percent.min(100);
    let counts = if progress.total_index_files > 0 {
        format!(
            " · {} / {} files",
            progress.indexed_files, progress.total_index_files
        )
    } else {
        String::new()
    };
    rsx! {
        div { class: "mt-2 rounded border border-neutral-700 bg-neutral-950/90 p-2.5",
            div { class: "flex items-center justify-between gap-2 text-[11px] mb-1",
                span { class: "text-[#66D8CE] font-semibold truncate", "{progress.detail}" }
                span { class: "font-mono text-neutral-300 shrink-0", "{pct}%{counts}" }
            }
            div { class: "w-full bg-neutral-900 rounded-full h-2 overflow-hidden border border-neutral-800",
                div {
                    class: "h-2 rounded-full transition-all duration-150",
                    style: "width: {pct}%; background: linear-gradient(90deg, #FF754F 0%, #66D8CE 100%);",
                }
            }
        }
    }
}

fn parse_timeout_secs(label: &str, raw: &str, max_secs: u64) -> Result<u64, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(format!("{label} timeout is required (in seconds)."));
    }
    let secs = trimmed
        .parse::<u64>()
        .map_err(|_| format!("{label} timeout must be a whole number of seconds."))?;
    if secs == 0 || secs > max_secs {
        return Err(format!(
            "{label} timeout must be between 1 and {max_secs} seconds."
        ));
    }
    secs.checked_mul(1000)
        .ok_or_else(|| format!("{label} timeout overflows milliseconds."))
}

fn parse_dispatch_timeouts(
    exec_raw: &str,
    idle_raw: &str,
    verify_raw: &str,
    soft_timeouts: bool,
) -> Result<(u64, u64, u64), String> {
    let (exec_max, idle_max, verify_max) = if soft_timeouts {
        (86_400, 86_400, 86_400)
    } else {
        (1800, 1800, 600)
    };
    let exec_ms = parse_timeout_secs("Execution", exec_raw, exec_max)?;
    let idle_ms = parse_timeout_secs("Inactivity", idle_raw, idle_max)?;
    if !soft_timeouts && idle_ms > exec_ms {
        return Err("Inactivity timeout cannot exceed the execution timeout.".into());
    }
    let verify_ms = parse_timeout_secs("Verification", verify_raw, verify_max)?;
    Ok((exec_ms, idle_ms, verify_ms))
}

fn apply_profile_timeout_fields(
    p: &WorkflowProfile,
    mut exec_secs: Signal<String>,
    mut idle_secs: Signal<String>,
    mut verify_secs: Signal<String>,
    mut profile_low_spec: Signal<bool>,
    mut soft_timeouts: Signal<bool>,
    mut require_clean_checkout: Signal<bool>,
) {
    let gen_s = p.generation_timeout_ms.unwrap_or(600_000) / 1000;
    let idle_s = p
        .generation_idle_timeout_ms
        .unwrap_or(gen_s.saturating_mul(1000))
        / 1000;
    let verif_s = p.verification_timeout_ms.unwrap_or(180_000) / 1000;
    exec_secs.set(gen_s.max(1).to_string());
    idle_secs.set(idle_s.max(1).to_string());
    verify_secs.set(verif_s.max(1).to_string());
    profile_low_spec.set(p.low_spec);
    soft_timeouts.set(p.soft_timeouts);
    require_clean_checkout.set(p.require_clean_checkout);
}

fn strip_line_col_suffix(token: &str) -> &str {
    let mut s = token;
    for _ in 0..2 {
        if let Some((head, tail)) = s.rsplit_once(':') {
            if !tail.is_empty()
                && tail.bytes().all(|b| b.is_ascii_digit())
                && (head.contains('/') || head.contains('.'))
            {
                s = head;
                continue;
            }
        }
        break;
    }
    s
}

fn extract_objective_repo_files(
    objective: &str,
    repo_files: &[String],
    profile_scope: &[String],
) -> Option<Vec<String>> {
    if repo_files.is_empty() || objective.trim().is_empty() {
        return None;
    }
    let mut matched = Vec::new();
    for raw in objective.split_whitespace() {
        let trimmed = raw.trim_matches(|c: char| {
            matches!(
                c,
                '\'' | '"'
                    | '`'
                    | '('
                    | ')'
                    | '['
                    | ']'
                    | '{'
                    | '}'
                    | '<'
                    | '>'
                    | ','
                    | ';'
                    | ':'
                    | '.'
                    | '!'
                    | '?'
            )
        });
        let cleaned = strip_line_col_suffix(trimmed)
            .replace('\\', "/")
            .trim_start_matches("./")
            .to_string();
        if cleaned.is_empty() || (!cleaned.contains('/') && !cleaned.contains('.')) {
            continue;
        }
        if let Some(canonical) = repo_files.iter().find(|f| f.eq_ignore_ascii_case(&cleaned)) {
            if !matched
                .iter()
                .any(|existing: &String| existing == canonical)
            {
                matched.push(canonical.clone());
                if matched.len() >= 64 {
                    break;
                }
            }
        }
    }
    if matched.is_empty() {
        return None;
    }
    let all_in_scope = matched
        .iter()
        .all(|m| profile_scope.iter().any(|s| s.eq_ignore_ascii_case(m)));
    if all_in_scope {
        None
    } else {
        Some(matched)
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
struct SuggestedFileItem {
    path: String,
    score: f64,
    #[serde(default)]
    reasons: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
struct SuggestFilesResponseUi {
    matched: Vec<SuggestedFileItem>,
    merged_scope: Vec<String>,
    indexed_files: usize,
    used_cached_index: bool,
}

#[derive(Clone, Copy)]
struct SuggestFilesTarget {
    chosen: Signal<String>,
    scope_manually_edited: Signal<bool>,
    suggest_seq: Signal<u64>,
    dispatch_allowed_files: Signal<String>,
    suggested_matches: Signal<Vec<SuggestedFileItem>>,
    indexed_file_count: Signal<Option<usize>>,
    auto_scope_note: Signal<Option<String>>,
}

fn spawn_suggest_files(
    conn: ApiConnection,
    profile_name: String,
    objective_text: String,
    acceptance_text: String,
    mut target: SuggestFilesTarget,
) {
    let my_seq = *target.suggest_seq.peek() + 1;
    target.suggest_seq.set(my_seq);
    if profile_name.is_empty() || objective_text.trim().is_empty() {
        target.suggested_matches.set(Vec::new());
        if !*target.scope_manually_edited.peek() {
            target.dispatch_allowed_files.set(String::new());
            target.auto_scope_note.set(None);
        }
        return;
    }
    spawn(async move {
        gloo_timers::future::TimeoutFuture::new(220).await;
        if *target.suggest_seq.peek() != my_seq || *target.chosen.peek() != profile_name {
            return;
        }
        let path = format!("/api/workflow-profiles/{profile_name}/suggest-files");
        let body = serde_json::json!({
            "objective": objective_text,
            "acceptance": acceptance_text,
        });
        if let Ok(resp) = api_post_json::<SuggestFilesResponseUi>(&conn, &path, body).await {
            if *target.suggest_seq.peek() != my_seq || *target.chosen.peek() != profile_name {
                return;
            }
            target.indexed_file_count.set(Some(resp.indexed_files));
            if resp.matched.is_empty() {
                target.suggested_matches.set(Vec::new());
                if !*target.scope_manually_edited.peek() && target.auto_scope_note.peek().is_some()
                {
                    target.dispatch_allowed_files.set(String::new());
                    target.auto_scope_note.set(None);
                }
            } else {
                let match_count = resp.matched.len();
                let merged_count = resp.merged_scope.len();
                let cache_tag = if resp.used_cached_index {
                    "cached"
                } else {
                    "indexed"
                };
                target.suggested_matches.set(resp.matched);
                if !*target.scope_manually_edited.peek() {
                    target
                        .dispatch_allowed_files
                        .set(resp.merged_scope.join(", "));
                    let match_noun = if match_count == 1 { "file" } else { "files" };
                    let idx_noun = if resp.indexed_files == 1 {
                        "file"
                    } else {
                        "files"
                    };
                    target.auto_scope_note.set(Some(format!(
                        "Auto-selected {match_count} matched {match_noun} merged with top profile files ({merged_count} total · {} indexed {idx_noun}, {cache_tag}).",
                        resp.indexed_files
                    )));
                }
            }
        }
    });
}

fn spawn_load_profile_files(
    conn: ApiConnection,
    profile_name: String,
    chosen: Signal<String>,
    mut repo_files: Signal<Vec<String>>,
) {
    if profile_name.is_empty() {
        repo_files.set(Vec::new());
        return;
    }
    spawn(async move {
        let path = format!("/api/workflow-profiles/{profile_name}/files");
        if let Ok(files) = api_get::<Vec<String>>(&conn, &path).await {
            if *chosen.peek() == profile_name {
                repo_files.set(files);
            }
        }
    });
}

fn spawn_load_profile_repo_status(
    conn: ApiConnection,
    profile_name: String,
    chosen: Signal<String>,
    mut repo_status: Signal<Option<ProfileRepoStatus>>,
) {
    if profile_name.is_empty() {
        repo_status.set(None);
        return;
    }
    spawn(async move {
        let path = format!("/api/workflow-profiles/{profile_name}/repo-status");
        if let Ok(status) = api_get::<ProfileRepoStatus>(&conn, &path).await {
            if *chosen.peek() == profile_name {
                repo_status.set(Some(status));
            }
        }
    });
}

fn default_dispatchable() -> bool {
    true
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
struct ProviderStatusItem {
    id: String,
    name: String,
    available: bool,
    authenticated: bool,
    #[serde(default = "default_dispatchable")]
    dispatchable: bool,
    user_email: Option<String>,
    tier: Option<String>,
    version: Option<String>,
    details: String,
    quota_type: String,
    reset_description: Option<String>,
    #[serde(default)]
    models: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Default)]
struct TokenTelemetrySummary {
    total_prompt_tokens: u64,
    total_output_tokens: u64,
    total_cached_tokens: u64,
    total_tasks_reported: usize,
    #[serde(default)]
    total_tasks_unreported: usize,
    estimated_cost_usd: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
struct ProviderReport {
    #[serde(default)]
    probed_at_ms: u64,
    providers: Vec<ProviderStatusItem>,
    token_summary: TokenTelemetrySummary,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
struct AnalyticsPairRow {
    backend: String,
    model: String,
    #[serde(default)]
    complexity: Option<String>,
    attempts: u64,
    exec_failed: u64,
    timed_out: u64,
    quota_exhausted: u64,
    verify_failed: u64,
    accepted: u64,
    exceptional: u64,
    rejected: u64,
    redone: u64,
    pass_rate: f64,
    avg_duration_ms: u64,
    learned_weight: f64,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Default)]
struct AnalyticsReport {
    #[serde(default)]
    total_attempts: u64,
    #[serde(default)]
    total_input_tokens: u64,
    #[serde(default)]
    total_output_tokens: u64,
    #[serde(default)]
    total_cached_tokens: u64,
    #[serde(default)]
    total_thinking_tokens: u64,
    #[serde(default)]
    total_usage_unreported: u64,
    #[serde(default)]
    by_pair_complexity: Vec<AnalyticsPairRow>,
}

fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub(crate) struct AgentProgress {
    pub(crate) phase: String,
    pub(crate) process_id: u32,
    pub(crate) elapsed_ms: u64,
    pub(crate) hard_limit_ms: u64,
    pub(crate) idle_limit_ms: Option<u64>,
    pub(crate) last_activity_at: Option<u64>,
    pub(crate) idle_ms: u64,
    pub(crate) stdout_bytes: u64,
    pub(crate) stderr_bytes: u64,
    pub(crate) changed_files: Vec<String>,
    pub(crate) file_observation_available: bool,
    pub(crate) finished: bool,
    pub(crate) stop_reason: Option<String>,
    #[serde(default)]
    pub(crate) stale: bool,
    #[serde(default = "default_true")]
    pub(crate) limit_enforced: bool,
    #[serde(default)]
    pub(crate) overtime: bool,
    #[serde(default)]
    pub(crate) overtime_ms: u64,
}

#[derive(Clone, PartialEq)]
pub(crate) enum FetchState<T> {
    Idle,
    Loading,
    Loaded(T),
    Error(String),
}

fn apply_auth(
    mut req: reqwest::RequestBuilder,
    conn: &ApiConnection,
    mutating: bool,
) -> reqwest::RequestBuilder {
    match &conn.auth {
        AuthMode::Bearer(token) => {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        AuthMode::Session { .. } => {
            #[cfg(target_arch = "wasm32")]
            {
                req = req.fetch_credentials_include();
            }
            if mutating {
                // `conductor_csrf` is the pre-rename name; sessions paired before it still use it.
                if let Some(csrf) =
                    read_cookie("clatterforge_csrf").or_else(|| read_cookie("conductor_csrf"))
                {
                    req = req.header("X-CSRF-Token", csrf);
                }
            }
        }
    }
    req
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

fn emit_client_log(
    conn: Option<&ApiConnection>,
    level: &str,
    event: &str,
    request_id: Option<String>,
    message: &str,
    fields: serde_json::Value,
) {
    let payload = serde_json::json!({
        "level": level,
        "component": "gui",
        "event": event,
        "request_id": request_id,
        "message": message,
        "fields": fields,
    });
    if let Ok(line) = serde_json::to_string(&payload) {
        console_error(&line);
    }
    if let Some(conn) = conn {
        let conn = conn.clone();
        let body = serde_json::json!({
            "level": level,
            "event": event,
            "request_id": request_id,
            "message": message,
            "fields": fields,
        });
        spawn(async move {
            let url = format!("{}/api/client-log", conn.base.trim_end_matches('/'));
            let req = apply_auth(reqwest::Client::new().post(url), &conn, true).json(&body);
            let _ = req.send().await;
        });
    }
}

async fn decode_json_response<T: serde::de::DeserializeOwned>(
    conn: Option<&ApiConnection>,
    method: &str,
    path: &str,
    response: reqwest::Response,
) -> Result<T, String> {
    let status = response.status();
    let request_id = response
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let text = response.text().await.map_err(|err| {
        let msg = format!("failed reading response body on {method} {path}: {err}");
        emit_client_log(
            conn,
            "error",
            "client.read_error",
            request_id.clone(),
            &msg,
            serde_json::json!({
                "method": method,
                "path": path,
                "status": status.as_u16(),
            }),
        );
        msg
    })?;
    serde_json::from_str::<T>(&text).map_err(|err| {
        let preview = truncate_chars(text.trim(), 240);
        let req_tag = request_id
            .as_deref()
            .map(|id| format!(" (req={id})"))
            .unwrap_or_default();
        let msg = format!("bad response body on {method} {path}{req_tag}: {err} | body={preview}");
        emit_client_log(
            conn,
            "error",
            "client.decode_error",
            request_id,
            &format!("{method} {path}: {err}"),
            serde_json::json!({
                "method": method,
                "path": path,
                "status": status.as_u16(),
                "expected_type": std::any::type_name::<T>(),
                "serde_error": err.to_string(),
                "body_preview": preview,
            }),
        );
        msg
    })
}

async fn api_get<T: serde::de::DeserializeOwned>(
    conn: &ApiConnection,
    path: &str,
) -> Result<T, String> {
    let url = format!("{}{path}", conn.base.trim_end_matches('/'));
    let req = apply_auth(reqwest::Client::new().get(url), conn, false);
    let response = req.send().await.map_err(|err| {
        let msg = format!("request failed on GET {path}: {err}");
        emit_client_log(
            Some(conn),
            "error",
            "client.network_error",
            None,
            &msg,
            serde_json::json!({"method": "GET", "path": path}),
        );
        msg
    })?;
    if !response.status().is_success() {
        let status = response.status();
        let request_id = response
            .headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let body = response.text().await.unwrap_or_default();
        emit_client_log(
            Some(conn),
            "warn",
            "client.http_error",
            request_id.clone(),
            &format!("GET {path} -> {status}"),
            serde_json::json!({
                "method": "GET",
                "path": path,
                "status": status.as_u16(),
                "body": truncate_chars(body.trim(), 240),
            }),
        );
        return Err(format!("{status}: {body}"));
    }
    decode_json_response(Some(conn), "GET", path, response).await
}

async fn api_post_json<T: serde::de::DeserializeOwned>(
    conn: &ApiConnection,
    path: &str,
    body: serde_json::Value,
) -> Result<T, String> {
    let url = format!("{}{path}", conn.base.trim_end_matches('/'));
    let req = apply_auth(reqwest::Client::new().post(url), conn, true).json(&body);
    let response = req.send().await.map_err(|err| {
        let msg = format!("request failed on POST {path}: {err}");
        emit_client_log(
            Some(conn),
            "error",
            "client.network_error",
            None,
            &msg,
            serde_json::json!({"method": "POST", "path": path}),
        );
        msg
    })?;
    if !response.status().is_success() {
        let status = response.status();
        let request_id = response
            .headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let body = response.text().await.unwrap_or_default();
        emit_client_log(
            Some(conn),
            "warn",
            "client.http_error",
            request_id.clone(),
            &format!("POST {path} -> {status}"),
            serde_json::json!({
                "method": "POST",
                "path": path,
                "status": status.as_u16(),
                "body": truncate_chars(body.trim(), 240),
            }),
        );
        return Err(format!("{status}: {body}"));
    }
    decode_json_response(Some(conn), "POST", path, response).await
}

async fn api_post(
    conn: &ApiConnection,
    path: &str,
    body: serde_json::Value,
) -> Result<TaskDetail, String> {
    api_post_json(conn, path, body).await
}

/// `POST /api/tasks/{id}/depend` returns `204 No Content`, not a `TaskDetail` - the
/// caller re-fetches detail afterward to pick up the new `depends_on` entry, same as every
/// other action's panel refresh, just via a separate GET instead of reusing the response.
async fn api_post_empty(
    conn: &ApiConnection,
    path: &str,
    body: serde_json::Value,
) -> Result<(), String> {
    let url = format!("{}{path}", conn.base.trim_end_matches('/'));
    let req = apply_auth(reqwest::Client::new().post(url), conn, true).json(&body);
    let response = req.send().await.map_err(|err| {
        let msg = format!("request failed on POST {path}: {err}");
        emit_client_log(
            Some(conn),
            "error",
            "client.network_error",
            None,
            &msg,
            serde_json::json!({"method": "POST", "path": path}),
        );
        msg
    })?;
    if response.status().is_success() {
        Ok(())
    } else {
        let status = response.status();
        let request_id = response
            .headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let body = response.text().await.unwrap_or_default();
        emit_client_log(
            Some(conn),
            "warn",
            "client.http_error",
            request_id,
            &format!("POST {path} -> {status}"),
            serde_json::json!({
                "method": "POST",
                "path": path,
                "status": status.as_u16(),
                "body": truncate_chars(body.trim(), 240),
            }),
        );
        Err(format!("{status}: {body}"))
    }
}

async fn api_probe_session(base: &str) -> Result<MeResponse, String> {
    let url = format!("{}/api/me", base.trim_end_matches('/'));
    let req = reqwest::Client::new().get(url);
    #[cfg(target_arch = "wasm32")]
    let req = req.fetch_credentials_include();
    let response = req
        .send()
        .await
        .map_err(|err| format!("request failed: {err}"))?;
    if !response.status().is_success() {
        return Err(format!("{}", response.status()));
    }
    decode_json_response(None, "GET", "/api/me", response).await
}

async fn api_pair_start(conn: &ApiConnection) -> Result<PairStartResponse, String> {
    let url = format!("{}/api/pair/start", conn.base.trim_end_matches('/'));
    let req = apply_auth(reqwest::Client::new().post(url), conn, true);
    let response = req
        .send()
        .await
        .map_err(|err| format!("request failed: {err}"))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!("{status}: {body}"));
    }
    decode_json_response(Some(conn), "POST", "/api/pair/start", response).await
}

async fn api_pair(base: &str, code: &str, principal: &str) -> Result<PairResponse, String> {
    let url = format!("{}/api/pair", base.trim_end_matches('/'));
    let req = reqwest::Client::new()
        .post(url)
        .json(&serde_json::json!({ "code": code, "principal": principal }));
    #[cfg(target_arch = "wasm32")]
    let req = req.fetch_credentials_include();
    let response = req
        .send()
        .await
        .map_err(|err| format!("request failed: {err}"))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!("{status}: {body}"));
    }
    decode_json_response(None, "POST", "/api/pair", response).await
}

async fn api_logout(base: &str) -> Result<(), String> {
    let url = format!("{}/api/logout", base.trim_end_matches('/'));
    let req = reqwest::Client::new().post(url);
    #[cfg(target_arch = "wasm32")]
    let req = req.fetch_credentials_include();
    let _ = req.send().await;
    Ok(())
}

fn main() {
    dioxus::launch(App);
}

#[component]
fn App() -> Element {
    rsx! {
        document::Title { "Clatterforge // Control the Chaos" }
        document::Meta { name: "theme-color", content: "#0C1118" }
        document::Meta { name: "apple-mobile-web-app-capable", content: "yes" }
        document::Meta { name: "apple-mobile-web-app-status-bar-style", content: "black-translucent" }
        document::Meta { name: "mobile-web-app-capable", content: "yes" }
        document::Link { rel: "icon", href: FAVICON }
        document::Link { rel: "apple-touch-icon", sizes: "180x180", href: APPLE_TOUCH_ICON }
        document::Link { rel: "stylesheet", href: MAIN_CSS }
        document::Link { rel: "stylesheet", href: TAILWIND_CSS }
        Dashboard {}
    }
}

#[cfg(target_arch = "wasm32")]
fn notification_permission() -> Option<String> {
    let win = web_sys::window()?;
    let ctor = js_sys::Reflect::get(&win, &JsValue::from_str("Notification")).ok()?;
    if ctor.is_undefined() || ctor.is_null() {
        return None;
    }
    js_sys::Reflect::get(&ctor, &JsValue::from_str("permission"))
        .ok()
        .and_then(|v| v.as_string())
}

#[cfg(not(target_arch = "wasm32"))]
fn notification_permission() -> Option<String> {
    None
}

#[cfg(target_arch = "wasm32")]
fn request_notification_permission(mut perm_signal: Signal<Option<String>>) {
    let Some(win) = web_sys::window() else { return };
    let Ok(ctor_val) = js_sys::Reflect::get(&win, &JsValue::from_str("Notification")) else {
        return;
    };
    let Ok(req_fn_val) = js_sys::Reflect::get(&ctor_val, &JsValue::from_str("requestPermission"))
    else {
        return;
    };
    let Some(req_fn) = req_fn_val.dyn_ref::<js_sys::Function>() else {
        return;
    };
    if let Ok(promise_val) = req_fn.call0(&ctor_val) {
        if let Ok(promise) = promise_val.dyn_into::<js_sys::Promise>() {
            wasm_bindgen_futures::spawn_local(async move {
                if let Ok(res) = wasm_bindgen_futures::JsFuture::from(promise).await {
                    if let Some(p) = res.as_string() {
                        perm_signal.set(Some(p));
                    }
                }
            });
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn request_notification_permission(_perm_signal: Signal<Option<String>>) {}

#[cfg(target_arch = "wasm32")]
fn maybe_send_browser_notification(title: &str, body: &str) {
    if notification_permission().as_deref() != Some("granted") {
        return;
    }
    let Some(win) = web_sys::window() else { return };
    let Ok(ctor_val) = js_sys::Reflect::get(&win, &JsValue::from_str("Notification")) else {
        return;
    };
    let Some(ctor) = ctor_val.dyn_ref::<js_sys::Function>() else {
        return;
    };
    let opts = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&opts, &JsValue::from_str("body"), &JsValue::from_str(body));
    let args = js_sys::Array::of2(&JsValue::from_str(title), &opts);
    let _ = js_sys::Reflect::construct(ctor, &args);
}

#[cfg(not(target_arch = "wasm32"))]
fn maybe_send_browser_notification(_title: &str, _body: &str) {}

fn collect_overtime_alerts(rows: &[TaskRow]) -> Vec<(String, String, String)> {
    let mut alerts = Vec::new();
    for row in rows {
        if let Some(msg) = &row.alert {
            alerts.push((row.id.clone(), row.objective.clone(), msg.clone()));
        } else {
            for child in &row.children {
                if let Some(msg) = &child.alert {
                    alerts.push((child.id.clone(), row.objective.clone(), msg.clone()));
                    break;
                }
            }
        }
    }
    alerts
}

#[component]
fn Dashboard() -> Element {
    let mut base = use_signal(default_api_base);
    let mut token = use_signal(String::new);
    let mut pair_code = use_signal(String::new);
    let mut pair_principal = use_signal(String::new);
    let mut pair_busy = use_signal(|| false);
    let mut pair_error = use_signal(|| None::<String>);
    let mut use_pair_tab = use_signal(|| !browser_is_loopback());
    let mut show_endpoint_override = use_signal(|| false);
    let mut issued_pair = use_signal(|| None::<PairStartResponse>);
    let mut issued_pair_busy = use_signal(|| false);
    let mut issued_pair_error = use_signal(|| None::<String>);
    let mut sessions_open = use_signal(|| false);
    let mut sessions_list = use_signal(|| FetchState::<Vec<SessionSummary>>::Idle);
    let mut sessions_busy = use_signal(|| false);

    let mut rows = use_signal(|| FetchState::Idle);
    let mut selected = use_signal(|| None::<String>);
    let mut inspector_wide = use_signal(|| false);
    let mut detail = use_signal(|| FetchState::Idle);
    let mut actor = use_signal(|| "operator".to_string());
    let mut action_error = use_signal(|| None::<String>);
    let mut connected = use_signal(|| None::<ApiConnection>);
    // Guards only Approve/Run/Accept/Reject against an accidental double-tap - the
    // backend's revision-checked, SQLite-transaction-serialized writes already make a
    // real duplicate dispatch impossible, so this prevents a confusing double-tap error.
    // Cancel is deliberately NOT gated by this so the stop control stays reachable.
    let mut busy = use_signal(|| false);
    let mut clear_busy = use_signal(|| false);
    let mut clear_error = use_signal(|| None::<String>);
    let mut clear_notice = use_signal(|| None::<String>);
    let mut dismissed_alerts = use_signal(std::collections::HashSet::<String>::new);
    let mut notified_alerts = use_signal(std::collections::HashSet::<String>::new);
    let notif_perm = use_signal(notification_permission);

    // Independent of any selected task - the global stop is a standalone emergency control.
    let mut stop_state = use_signal(|| FetchState::Idle);
    let mut stop_by = use_signal(String::new);
    let mut stop_reason = use_signal(String::new);
    let mut stop_busy = use_signal(|| false);
    let mut stop_form_open = use_signal(|| false);

    let refresh_stop = move |conn: ApiConnection| {
        spawn(async move {
            stop_state.set(match api_get::<StopState>(&conn, "/api/stop").await {
                Ok(fetched) => FetchState::Loaded(fetched),
                Err(err) => FetchState::Error(err),
            });
        });
    };

    let refresh_rows = move |conn: ApiConnection| {
        spawn(async move {
            rows.set(FetchState::Loading);
            rows.set(match api_get::<Vec<TaskRow>>(&conn, "/api/tasks").await {
                Ok(fetched) => FetchState::Loaded(fetched),
                Err(err) => FetchState::Error(err),
            });
        });
    };

    let connect_bearer = move |_| {
        pair_error.set(None);
        let endpoint = base().trim().to_string();
        let tok = token().trim().to_string();
        if endpoint.is_empty() || tok.is_empty() {
            return;
        }
        save_bearer(&endpoint, &tok);
        let conn = ApiConnection {
            base: endpoint,
            auth: AuthMode::Bearer(tok),
        };
        connected.set(Some(conn.clone()));
        refresh_rows(conn.clone());
        refresh_stop(conn);
    };

    let connect_pair = move |_| {
        let endpoint = base();
        let code = pair_code().trim().to_ascii_uppercase();
        let principal = pair_principal().trim().to_string();
        if code.is_empty() || principal.is_empty() {
            return;
        }
        pair_busy.set(true);
        pair_error.set(None);
        spawn(async move {
            match api_pair(&endpoint, &code, &principal).await {
                Ok(paired) => {
                    pair_code.set(String::new());
                    actor.set(paired.principal.clone());
                    stop_by.set(paired.principal.clone());
                    let conn = ApiConnection {
                        base: endpoint,
                        auth: AuthMode::Session {
                            principal: paired.principal,
                        },
                    };
                    connected.set(Some(conn.clone()));
                    refresh_rows(conn.clone());
                    refresh_stop(conn);
                }
                Err(err) => pair_error.set(Some(err)),
            }
            pair_busy.set(false);
        });
    };

    let on_start_pair = move |_| {
        let Some(conn) = connected() else { return };
        issued_pair_busy.set(true);
        issued_pair_error.set(None);
        spawn(async move {
            match api_pair_start(&conn).await {
                Ok(resp) => issued_pair.set(Some(resp)),
                Err(err) => issued_pair_error.set(Some(err)),
            }
            issued_pair_busy.set(false);
        });
    };

    let refresh_sessions = move |conn: ApiConnection| {
        spawn(async move {
            sessions_list.set(FetchState::Loading);
            sessions_list.set(
                match api_get::<Vec<SessionSummary>>(&conn, "/api/sessions").await {
                    Ok(list) => FetchState::Loaded(list),
                    Err(err) => FetchState::Error(err),
                },
            );
        });
    };

    let on_toggle_sessions = move |_| {
        let next = !sessions_open();
        sessions_open.set(next);
        if next {
            if let Some(conn) = connected() {
                refresh_sessions(conn);
            }
        }
    };

    let mut on_revoke_session = move |session_id: String| {
        let Some(conn) = connected() else { return };
        sessions_busy.set(true);
        spawn(async move {
            let _ = api_post_json::<serde_json::Value>(
                &conn,
                &format!("/api/sessions/{session_id}/revoke"),
                serde_json::json!({}),
            )
            .await;
            refresh_sessions(conn);
            sessions_busy.set(false);
        });
    };

    let on_revoke_all_sessions = move |_| {
        let Some(conn) = connected() else { return };
        sessions_busy.set(true);
        spawn(async move {
            let _ = api_post_json::<serde_json::Value>(
                &conn,
                "/api/sessions/revoke-all",
                serde_json::json!({}),
            )
            .await;
            refresh_sessions(conn);
            sessions_busy.set(false);
        });
    };

    let on_disconnect = move |_| {
        let current = connected();
        clear_saved_bearer();
        connected.set(None);
        selected.set(None);
        inspector_wide.set(false);
        detail.set(FetchState::Idle);
        rows.set(FetchState::Idle);
        stop_state.set(FetchState::Idle);
        issued_pair.set(None);
        issued_pair_error.set(None);
        action_error.set(None);
        if let Some(conn) = current {
            if conn.session_principal().is_some() {
                let endpoint = conn.base;
                spawn(async move {
                    let _ = api_logout(&endpoint).await;
                });
            }
        }
    };

    // Runs once on mount:
    // 1. Inside Tauri or via `#token=...&base=...` fragment, auto-connects via Bearer (and persists
    //    the bearer config in browser localStorage when outside Tauri so reloads stay connected).
    // 2. Otherwise in a browser, checks saved localStorage bearer config via `/api/me`.
    // 3. Otherwise probes `/api/me` with credentials included so a phone or remote browser with an
    //    active 30-day paired session reconnects automatically.
    use_resource(move || async move {
        if let Ok(config) = get_api_config().await {
            if !tauri_available() {
                save_bearer(&config.base, &config.token);
            }
            base.set(config.base.clone());
            token.set(config.token.clone());
            let conn = ApiConnection::from_bearer(config);
            connected.set(Some(conn.clone()));
            refresh_rows(conn.clone());
            refresh_stop(conn);
            return;
        }
        if let Some(saved) = load_saved_bearer() {
            let conn = ApiConnection::from_bearer(saved.clone());
            if api_get::<MeResponse>(&conn, "/api/me").await.is_ok() {
                base.set(saved.base);
                token.set(saved.token);
                connected.set(Some(conn.clone()));
                refresh_rows(conn.clone());
                refresh_stop(conn);
                return;
            }
            clear_saved_bearer();
        }
        let endpoint = default_api_base();
        base.set(endpoint.clone());
        if let Ok(me) = api_probe_session(&endpoint).await {
            if me.auth == "session" {
                if let Some(principal) = me.principal {
                    actor.set(principal.clone());
                    stop_by.set(principal.clone());
                    let conn = ApiConnection {
                        base: endpoint,
                        auth: AuthMode::Session { principal },
                    };
                    connected.set(Some(conn.clone()));
                    refresh_rows(conn.clone());
                    refresh_stop(conn);
                }
            }
        }
    });

    #[cfg(target_arch = "wasm32")]
    use_effect(move || {
        use wasm_bindgen::closure::Closure;
        let Some(window) = web_sys::window() else {
            return;
        };
        let on_hash_change = Closure::wrap(Box::new(move || {
            let Some(win) = web_sys::window() else {
                return;
            };
            let loc = win.location();
            if loc.hash().is_ok_and(|h| h.contains("token=")) {
                let _ = loc.reload();
            }
        }) as Box<dyn FnMut()>);
        let _ = js_sys::Reflect::set(
            window.as_ref(),
            &JsValue::from_str("onhashchange"),
            on_hash_change.as_ref().unchecked_ref(),
        );
        on_hash_change.forget();
    });

    // Observe work started by any client. Cancels its previous loop on reconnect/unmount.
    use_resource(move || async move {
        let Some(conn) = connected() else {
            return;
        };
        loop {
            match api_get::<Vec<TaskRow>>(&conn, "/api/tasks").await {
                Ok(fetched) => {
                    let active_ids: std::collections::HashSet<String> = fetched
                        .iter()
                        .filter_map(|r| {
                            if r.alert.is_some() {
                                Some(r.id.clone())
                            } else {
                                r.children
                                    .iter()
                                    .find(|c| c.alert.is_some())
                                    .map(|c| c.id.clone())
                            }
                        })
                        .collect();
                    dismissed_alerts
                        .write()
                        .retain(|id| active_ids.contains(id));
                    for (id, objective, msg) in collect_overtime_alerts(&fetched) {
                        let phase_tag = msg.split_whitespace().next().unwrap_or("run");
                        let key = format!("{id}:{phase_tag}");
                        let is_new = {
                            let mut seen = notified_alerts.write();
                            seen.insert(key)
                        };
                        if is_new {
                            let short = workflow_ui::short_id(&id);
                            maybe_send_browser_notification(
                                &format!("Clatterforge #{short} overtime"),
                                &format!("{msg} — {objective}"),
                            );
                        }
                    }
                    rows.set(FetchState::Loaded(fetched));
                }
                Err(error) => {
                    if error.contains("401") {
                        if let AuthMode::Bearer(ref current_token) = conn.auth {
                            if let Ok(fresh) = get_api_config().await {
                                if fresh.base == conn.base && fresh.token != *current_token {
                                    token.set(fresh.token.clone());
                                    connected.set(Some(ApiConnection::from_bearer(fresh)));
                                    return;
                                }
                            }
                            if !tauri_available() {
                                clear_saved_bearer();
                                connected.set(None);
                                rows.set(FetchState::Idle);
                                use_pair_tab.set(false);
                                pair_error.set(Some(
                                    "Bearer token was rotated or rejected (401). Enter the current serve-token to reconnect."
                                        .to_string(),
                                ));
                                return;
                            }
                        } else {
                            // Session expired or revoked; return to pairing view.
                            connected.set(None);
                            rows.set(FetchState::Idle);
                            pair_error.set(Some(
                                "Session expired or revoked. Pair again with a fresh setup code."
                                    .to_string(),
                            ));
                            return;
                        }
                    }
                    rows.set(FetchState::Error(error));
                }
            }
            gloo_timers::future::TimeoutFuture::new(2_000).await;
        }
    });

    use_resource(move || async move {
        let conn = connected();
        action_error.set(None);
        let Some(id) = selected() else {
            detail.set(FetchState::Idle);
            return;
        };
        let Some(conn) = conn else {
            return;
        };
        detail.set(FetchState::Loading);
        loop {
            detail.set(
                match api_get::<TaskDetail>(&conn, &format!("/api/tasks/{id}")).await {
                    Ok(fetched) => FetchState::Loaded(fetched),
                    Err(error) => FetchState::Error(error),
                },
            );
            gloo_timers::future::TimeoutFuture::new(800).await;
        }
    });

    let mut apply_task_detail = move |updated: TaskDetail, conn: ApiConnection| {
        if selected.peek().as_deref() == Some(updated.id.as_str()) {
            detail.set(FetchState::Loaded(updated));
        }
        refresh_rows(conn);
    };

    let mut run_action = move |path: String, body: serde_json::Value, track_busy: bool| {
        let Some(conn) = connected() else { return };
        action_error.set(None);
        if track_busy {
            busy.set(true);
        }
        spawn(async move {
            match api_post(&conn, &path, body).await {
                Ok(updated) => apply_task_detail(updated, conn),
                Err(err) => {
                    if err.contains("revision conflict") {
                        let task_id_opt = path
                            .strip_prefix("/api/tasks/")
                            .and_then(|rest| rest.split('/').next())
                            .map(str::to_string);
                        if let Some(tid) = task_id_opt {
                            if let Ok(fresh) =
                                api_get::<TaskDetail>(&conn, &format!("/api/tasks/{tid}")).await
                            {
                                let expected = if path.ends_with("/approve") {
                                    Some("awaiting_approval")
                                } else if path.ends_with("/run") {
                                    Some("approved")
                                } else if path.ends_with("/review") {
                                    Some("awaiting_review")
                                } else {
                                    None
                                };
                                if expected.is_some_and(|s| fresh.status != s) {
                                    apply_task_detail(fresh, conn);
                                    if track_busy {
                                        busy.set(false);
                                    }
                                    return;
                                }
                            }
                        }
                    }
                    action_error.set(Some(err));
                }
            }
            if track_busy {
                busy.set(false);
            }
        });
    };

    let mut run_approve_and_run = move |task_id: String,
                                        revision: u64,
                                        digest: String,
                                        by: String| {
        let Some(conn) = connected() else { return };
        action_error.set(None);
        busy.set(true);
        spawn(async move {
            match api_post(
                &conn,
                &format!("/api/tasks/{task_id}/approve"),
                serde_json::json!({"revision": revision, "digest": digest, "by": by}),
            )
            .await
            {
                Ok(approved) => {
                    let next_rev = approved.revision;
                    apply_task_detail(approved, conn.clone());
                    match api_post(
                        &conn,
                        &format!("/api/tasks/{task_id}/run"),
                        serde_json::json!({"revision": next_rev}),
                    )
                    .await
                    {
                        Ok(updated) => apply_task_detail(updated, conn),
                        Err(err) => {
                            if err.contains("revision conflict") {
                                if let Ok(fresh) =
                                    api_get::<TaskDetail>(&conn, &format!("/api/tasks/{task_id}"))
                                        .await
                                {
                                    if fresh.status != "approved" {
                                        apply_task_detail(fresh, conn);
                                        busy.set(false);
                                        return;
                                    }
                                }
                            }
                            action_error.set(Some(err));
                        }
                    }
                }
                Err(err) => {
                    if err.contains("revision conflict") {
                        if let Ok(fresh) =
                            api_get::<TaskDetail>(&conn, &format!("/api/tasks/{task_id}")).await
                        {
                            if fresh.status != "awaiting_approval" {
                                apply_task_detail(fresh, conn);
                                busy.set(false);
                                return;
                            }
                        }
                    }
                    action_error.set(Some(err));
                }
            }
            busy.set(false);
        });
    };

    let mut run_retry_and_run = move |task_id: String,
                                      revision: u64,
                                      by: String,
                                      reset_workspace: bool,
                                      from_step: Option<usize>,
                                      guidance: Option<String>| {
        let Some(conn) = connected() else { return };
        action_error.set(None);
        busy.set(true);
        spawn(async move {
            match api_post(
                &conn,
                &format!("/api/tasks/{task_id}/retry"),
                serde_json::json!({
                    "revision": revision,
                    "by": by,
                    "reset_workspace": reset_workspace,
                    "from_step": from_step,
                    "guidance": guidance,
                }),
            )
            .await
            {
                Ok(retried) => {
                    let next_rev = retried.revision;
                    apply_task_detail(retried, conn.clone());
                    match api_post(
                        &conn,
                        &format!("/api/tasks/{task_id}/run"),
                        serde_json::json!({"revision": next_rev}),
                    )
                    .await
                    {
                        Ok(updated) => apply_task_detail(updated, conn),
                        Err(err) => {
                            if err.contains("revision conflict") {
                                if let Ok(fresh) =
                                    api_get::<TaskDetail>(&conn, &format!("/api/tasks/{task_id}"))
                                        .await
                                {
                                    if fresh.status != "approved" {
                                        apply_task_detail(fresh, conn);
                                        busy.set(false);
                                        return;
                                    }
                                }
                            }
                            action_error.set(Some(err));
                        }
                    }
                }
                Err(err) => {
                    if err.contains("revision conflict") {
                        if let Ok(fresh) =
                            api_get::<TaskDetail>(&conn, &format!("/api/tasks/{task_id}")).await
                        {
                            apply_task_detail(fresh, conn);
                        }
                    }
                    action_error.set(Some(err));
                }
            }
            busy.set(false);
        });
    };

    // `POST .../depend` returns no body to show, so this re-fetches detail afterward.
    let on_depend = move |target: String| {
        let Some(task_id) = selected() else { return };
        let Some(conn) = connected() else { return };
        action_error.set(None);
        spawn(async move {
            match api_post_empty(
                &conn,
                &format!("/api/tasks/{task_id}/depend"),
                serde_json::json!({"on": target}),
            )
            .await
            {
                Ok(()) => {
                    if let Ok(fetched) =
                        api_get::<TaskDetail>(&conn, &format!("/api/tasks/{task_id}")).await
                    {
                        detail.set(FetchState::Loaded(fetched));
                    }
                }
                Err(err) => action_error.set(Some(err)),
            }
        });
    };

    // Engages the switch and sweeps every active attempt; resume only clears it.
    let on_global_stop = move |_| {
        let Some(conn) = connected() else { return };
        let by = if let Some(principal) = conn.session_principal() {
            principal.to_string()
        } else {
            stop_by()
        };
        let reason = stop_reason();
        action_error.set(None);
        stop_busy.set(true);
        spawn(async move {
            match api_post_json::<StopAllOutcome>(
                &conn,
                "/api/stop",
                serde_json::json!({"by": by, "reason": reason}),
            )
            .await
            {
                Ok(outcome) => {
                    stop_state.set(FetchState::Loaded(outcome.state));
                    stop_form_open.set(false);
                    refresh_rows(conn);
                }
                Err(err) => action_error.set(Some(err)),
            }
            stop_busy.set(false);
        });
    };

    let on_global_resume = move |_| {
        let Some(conn) = connected() else { return };
        let by = if let Some(principal) = conn.session_principal() {
            principal.to_string()
        } else {
            stop_by()
        };
        let reason = stop_reason();
        action_error.set(None);
        stop_busy.set(true);
        spawn(async move {
            match api_post_json::<StopState>(
                &conn,
                "/api/stop/resume",
                serde_json::json!({"by": by, "reason": reason}),
            )
            .await
            {
                Ok(state) => {
                    stop_state.set(FetchState::Loaded(state));
                    stop_reason.set(String::new());
                }
                Err(err) => action_error.set(Some(err)),
            }
            stop_busy.set(false);
        });
    };

    let has_selection = selected().is_some();
    let active_conn = connected();
    let is_session_auth = active_conn
        .as_ref()
        .is_some_and(|c| c.session_principal().is_some());

    rsx! {
        div { class: "min-h-screen cf-chassis p-3 sm:p-4 md:p-6 font-mono text-sm max-w-full overflow-x-hidden", style: "background-color: #0C1118; color: #F2F5F7;",
            header { class: "mb-4 pb-3 sm:pb-4 border-b", style: "border-color: #202D3A;",
                div { class: "flex flex-col md:flex-row md:items-center md:justify-between gap-3",
                    div { class: "min-w-0",
                        div { class: "flex flex-wrap items-center gap-2 mb-1",
                            span {
                                class: "text-[11px] font-bold tracking-widest px-2 py-0.5 rounded border shrink-0",
                                style: "color: #FF754F; border-color: #FF754F; background-color: rgba(255, 117, 79, 0.08);",
                                "CF // SYSTEMS"
                            }
                            span {
                                class: "text-[10px] sm:text-[11px] tracking-widest uppercase",
                                style: "color: #8D9DAD;",
                                "AGENT ORCHESTRATION / DISTRIBUTED EXECUTION"
                            }
                        }
                        h1 { class: "cf-brand-heading",
                            img {
                                class: "cf-brand-logo",
                                src: BRAND_LOGO,
                                alt: "Clatterforge. Control the chaos.",
                                width: "1600",
                                height: "330",
                            }
                        }
                        p { class: "text-xs mt-1", style: "color: #8D9DAD;",
                            "We're not making agents smarter. We're making their work accountable."
                        }
                    }
                    div {
                        class: "rounded px-3 py-2 border text-xs flex flex-col gap-1 shrink-0",
                        style: "background-color: #202D3A; border-color: #202D3A;",
                        div { class: "flex flex-wrap items-center justify-between gap-3",
                            span { class: "tracking-widest uppercase text-[10px]", style: "color: #8D9DAD;", "EXECUTION / TOPOLOGY" }
                            span { class: "font-bold tracking-wider text-[10px]", style: "color: #66D8CE;", "● SYSTEM NOMINAL" }
                        }
                        div { class: "font-mono text-[11px] tracking-wider", style: "color: #F2F5F7;",
                            span { style: "color: #FF754F;", "01" }
                            " PLAN → "
                            span { style: "color: #FF754F;", "02" }
                            " EXECUTE → "
                            span { style: "color: #66D8CE;", "03 VERIFY" }
                        }
                    }
                }
            }

            if let Some(conn) = active_conn.clone() {
                div { class: "mb-4 rounded border px-3 py-2 flex flex-wrap items-center justify-between gap-2", style: "border-color: #202D3A; background-color: rgba(32, 45, 58, 0.3);",
                    div { class: "flex flex-wrap items-center gap-2 min-w-0 text-xs",
                        span { class: "font-bold", style: "color: #66D8CE;", "● CONNECTED" }
                        span { class: "text-neutral-400 break-all", "{conn.base}" }
                        if let Some(principal) = conn.session_principal() {
                            span { class: "px-2 py-0.5 rounded border border-emerald-700 bg-emerald-950/60 text-emerald-300",
                                "Session: {principal}"
                            }
                        } else {
                            span { class: "px-2 py-0.5 rounded border border-neutral-700 bg-neutral-900 text-neutral-300",
                                "Local Bearer"
                            }
                        }
                    }
                    div { class: "flex flex-wrap items-center gap-2 w-full sm:w-auto",
                        if conn.is_bearer() {
                            button {
                                class: "bg-sky-800 hover:bg-sky-700 rounded px-3 py-1 min-h-11 text-xs font-semibold flex-1 sm:flex-none disabled:opacity-50",
                                disabled: issued_pair_busy(),
                                onclick: on_start_pair,
                                if issued_pair_busy() { "Generating…" } else { "Pair Phone / Device" }
                            }
                            button {
                                class: "bg-neutral-800 hover:bg-neutral-700 border border-neutral-700 rounded px-3 py-1 min-h-11 text-xs flex-1 sm:flex-none",
                                onclick: on_toggle_sessions,
                                if sessions_open() { "Hide Devices" } else { "Paired Devices" }
                            }
                        }
                        button {
                            class: "bg-neutral-800 hover:bg-neutral-700 border border-neutral-700 rounded px-3 py-1 min-h-11 text-xs flex-1 sm:flex-none",
                            onclick: on_disconnect,
                            if conn.session_principal().is_some() { "Log Out" } else { "Disconnect" }
                        }
                    }
                }

                if conn.is_bearer() && sessions_open() {
                    div { class: "mb-4 rounded border border-neutral-700 bg-neutral-900/70 p-3",
                        div { class: "flex flex-wrap items-center justify-between gap-2 mb-2",
                            p { class: "text-xs font-bold uppercase tracking-wider text-neutral-300", "Paired Remote Devices (30-day persistent sessions)" }
                            div { class: "flex items-center gap-2",
                                button {
                                    class: "text-xs bg-neutral-800 hover:bg-neutral-700 border border-neutral-600 rounded px-2.5 py-1 min-h-11",
                                    onclick: {
                                        let conn = conn.clone();
                                        move |_| refresh_sessions(conn.clone())
                                    },
                                    "Refresh"
                                }
                                {
                                    let has_devices = matches!(&*sessions_list.read(), FetchState::Loaded(list) if !list.is_empty());
                                    rsx! {
                                        button {
                                            class: "text-xs bg-red-900/80 hover:bg-red-800 border border-red-700 rounded px-2.5 py-1 min-h-11 disabled:opacity-45 disabled:cursor-not-allowed",
                                            disabled: sessions_busy() || !has_devices,
                                            onclick: on_revoke_all_sessions,
                                            "Revoke All"
                                        }
                                    }
                                }
                            }
                        }
                        match &*sessions_list.read() {
                            FetchState::Idle | FetchState::Loading => rsx! {
                                p { class: "text-xs text-neutral-400", "Loading paired devices…" }
                            },
                            FetchState::Error(err) => rsx! {
                                p { class: "text-xs text-red-400 break-words", "Failed to load devices: {err}" }
                            },
                            FetchState::Loaded(list) if list.is_empty() => rsx! {
                                p { class: "text-xs text-neutral-400", "No paired devices right now. Click Pair Phone / Device to add one." }
                            },
                            FetchState::Loaded(list) => rsx! {
                                div { class: "flex flex-col gap-2",
                                    for s in list.iter() {
                                        {
                                            let short_id: String = s.id.chars().take(8).collect();
                                            let sid = s.id.clone();
                                            rsx! {
                                                div { class: "flex flex-wrap items-center justify-between gap-2 rounded border border-neutral-800 bg-neutral-950 px-3 py-2 text-xs",
                                                    div { class: "min-w-0",
                                                        p { class: "font-semibold text-neutral-200", "{s.principal}" }
                                                        p { class: "text-neutral-500 font-mono", "id: {short_id}" }
                                                    }
                                                    button {
                                                        class: "bg-red-900/80 hover:bg-red-800 border border-red-700 rounded px-3 py-1 min-h-11 text-xs disabled:opacity-50",
                                                        disabled: sessions_busy(),
                                                        onclick: move |_| on_revoke_session(sid.clone()),
                                                        "Revoke"
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            },
                        }
                    }
                }

                if let Some(code_info) = issued_pair() {
                    div { class: "mb-4 rounded border border-sky-700 bg-sky-950/40 p-3",
                        div { class: "flex flex-wrap items-center justify-between gap-2 mb-2",
                            p { class: "text-sky-300 font-bold text-xs uppercase tracking-wider", "One-Time Setup Code (enter within 5 min)" }
                            button {
                                class: "text-xs text-neutral-400 hover:text-white px-2 py-1 min-h-11",
                                onclick: move |_| issued_pair.set(None),
                                "Dismiss"
                            }
                        }
                        p { class: "text-2xl sm:text-3xl font-bold tracking-[0.2em] text-white select-all mb-2", "{code_info.code}" }
                        p { class: "text-xs text-neutral-300",
                            "Open your Tailscale or local-network Clatterforge URL on your phone, enter your name and this one-time code, and tap Pair & Connect. Once paired, your phone stays signed in for 30 days (even across host restarts)."
                        }
                    }
                }
                if let Some(err) = issued_pair_error() {
                    p { class: "mb-3 text-xs text-red-400", "Could not start pairing: {err}" }
                }
            } else {
                div { class: "mb-4 rounded border p-3 sm:p-4", style: "border-color: #202D3A; background-color: rgba(32, 45, 58, 0.35);",
                    div { class: "grid grid-cols-2 sm:inline-flex gap-2 mb-3",
                        button {
                            class: if use_pair_tab() {
                                "rounded px-3 py-2 min-h-11 text-xs font-bold uppercase tracking-wider border border-[#FF754F] bg-[#FF754F]/15 text-[#FF754F]"
                            } else {
                                "rounded px-3 py-2 min-h-11 text-xs font-semibold uppercase tracking-wider border border-neutral-700 bg-neutral-900 text-neutral-400"
                            },
                            onclick: move |_| {
                                use_pair_tab.set(true);
                                pair_error.set(None);
                            },
                            "Pair Device (Phone)"
                        }
                        button {
                            class: if !use_pair_tab() {
                                "rounded px-3 py-2 min-h-11 text-xs font-bold uppercase tracking-wider border border-[#FF754F] bg-[#FF754F]/15 text-[#FF754F]"
                            } else {
                                "rounded px-3 py-2 min-h-11 text-xs font-semibold uppercase tracking-wider border border-neutral-700 bg-neutral-900 text-neutral-400"
                            },
                            onclick: move |_| {
                                use_pair_tab.set(false);
                                pair_error.set(None);
                            },
                            "Bearer Token"
                        }
                    }

                    if use_pair_tab() {
                        div { class: "flex flex-col gap-2",
                            p { class: "text-xs text-neutral-400",
                                "Enter a one-time setup code from your Clatterforge host once — your phone stays signed in for 30 days (with automatic renewal, even across host restarts)."
                            }
                            div { class: "flex flex-col sm:flex-row gap-2",
                                input {
                                    class: "bg-neutral-900 border border-neutral-700 rounded px-3 py-2 min-h-11 flex-1 text-base sm:text-sm",
                                    value: "{pair_principal}",
                                    oninput: move |e| pair_principal.set(e.value()),
                                    placeholder: "Your name (recorded on approvals)",
                                }
                                input {
                                    class: "bg-neutral-900 border border-neutral-700 rounded px-3 py-2 min-h-11 flex-1 text-base sm:text-sm uppercase tracking-widest font-mono",
                                    value: "{pair_code}",
                                    oninput: move |e| pair_code.set(e.value()),
                                    placeholder: "XXXX-XXXX",
                                    autocomplete: "one-time-code",
                                }
                                button {
                                    class: "cf-action-btn rounded px-5 py-2 min-h-11 w-full sm:w-auto disabled:opacity-50",
                                    style: "background-color: #FF754F; color: #0C1118; font-weight: 700;",
                                    disabled: pair_busy() || pair_principal().trim().is_empty() || pair_code().trim().is_empty(),
                                    onclick: connect_pair,
                                    if pair_busy() { "Pairing…" } else { "Pair & Connect" }
                                }
                            }
                            div { class: "flex flex-wrap items-center justify-between gap-2 text-xs text-neutral-500 mt-1",
                                span { class: "break-all", "Server: {base}" }
                                button {
                                    class: "text-sky-400 hover:underline min-h-11 px-1",
                                    onclick: move |_| show_endpoint_override.set(!show_endpoint_override()),
                                    if show_endpoint_override() { "Hide server URL" } else { "Change server URL" }
                                }
                            }
                            if show_endpoint_override() {
                                input {
                                    class: "bg-neutral-900 border border-neutral-700 rounded px-3 py-2 min-h-11 w-full text-base sm:text-sm",
                                    value: "{base}",
                                    oninput: move |e| base.set(e.value()),
                                    placeholder: "http://10.0.0.50:8787",
                                }
                            }
                            if let Some(err) = pair_error() {
                                p { class: "text-xs text-red-400 mt-1", "Pairing failed: {err}" }
                            }
                        }
                    } else {
                        div { class: "flex flex-col gap-2",
                            div { class: "flex flex-col sm:flex-row sm:items-end gap-2",
                                div { class: "flex-1 min-w-0",
                                    label { class: "block text-[11px] text-neutral-400 mb-1", "Server URL" }
                                    input {
                                        class: "bg-neutral-900 border border-neutral-700 rounded px-3 py-2 min-h-11 w-full text-base sm:text-sm",
                                        value: "{base}",
                                        oninput: move |e| base.set(e.value()),
                                        placeholder: "{default_api_base()}",
                                    }
                                }
                                div { class: "flex-1 min-w-0",
                                    label { class: "block text-[11px] text-neutral-400 mb-1", "Bearer Token" }
                                    input {
                                        class: "bg-neutral-900 border border-neutral-700 rounded px-3 py-2 min-h-11 w-full text-base sm:text-sm",
                                        r#type: "password",
                                        value: "{token}",
                                        oninput: move |e| token.set(e.value()),
                                        placeholder: "Paste serve-token here",
                                    }
                                }
                                button {
                                    class: "cf-action-btn rounded px-5 py-2 min-h-11 w-full sm:w-auto",
                                    disabled: token().trim().is_empty(),
                                    onclick: connect_bearer,
                                    "Connect"
                                }
                            }
                            if let Some(err) = pair_error() {
                                p { class: "text-xs text-red-400", "{err}" }
                            }
                        }
                    }
                }
            }

            if let Some(conn) = active_conn.clone() {
                ProviderMonitor { config: conn.clone() }
                ForgeSettings { config: conn.clone() }
                WorkflowIntake {
                    config: conn.clone(),
                    actor,
                    on_created: move |id: String| {
                        selected.set(Some(id));
                        refresh_rows(conn.clone());
                    },
                }

                {
                    let engaged = matches!(&*stop_state.read(), FetchState::Loaded(s) if s.stopped);
                    let actor_ready = is_session_auth || !stop_by().trim().is_empty();
                    rsx! {
                        div {
                            class: if engaged {
                                "mb-4 rounded border border-red-700 bg-red-950/40 px-3 py-2 flex flex-col gap-2"
                            } else {
                                "mb-4 rounded border border-neutral-800 px-3 py-2 flex flex-col gap-2"
                            },
                            match &*stop_state.read() {
                                FetchState::Loaded(s) if s.stopped => rsx! {
                                    div { class: "flex flex-col sm:flex-row sm:items-center justify-between gap-2",
                                        div {
                                            p { class: "text-red-400 font-bold", "GLOBAL STOP ENGAGED" }
                                            p { class: "text-xs text-neutral-300 break-words",
                                                "by {s.by.as_deref().unwrap_or(\"?\")} — {s.reason.as_deref().unwrap_or(\"?\")}"
                                            }
                                        }
                                        div { class: "flex flex-col sm:flex-row gap-2 w-full sm:w-auto sm:flex-1 sm:max-w-xl",
                                            if !is_session_auth {
                                                input {
                                                    class: "bg-neutral-900 border border-neutral-700 rounded px-2 py-1 min-h-11 text-base sm:text-sm sm:w-36",
                                                    value: "{stop_by}",
                                                    oninput: move |e| stop_by.set(e.value()),
                                                    placeholder: "your name",
                                                }
                                            }
                                            input {
                                                class: "bg-neutral-900 border border-neutral-700 rounded px-2 py-1 min-h-11 flex-1 text-base sm:text-sm",
                                                value: "{stop_reason}",
                                                oninput: move |e| stop_reason.set(e.value()),
                                                placeholder: "resume reason",
                                            }
                                            button {
                                                class: "bg-neutral-700 hover:bg-neutral-600 rounded px-4 py-1 min-h-11 w-full sm:w-auto disabled:opacity-50",
                                                disabled: stop_busy() || !actor_ready || stop_reason().trim().is_empty(),
                                                onclick: on_global_resume,
                                                if stop_busy() { "Resuming…" } else { "Resume" }
                                            }
                                        }
                                    }
                                },
                                _ => rsx! {
                                    div { class: "flex flex-col gap-2",
                                        div { class: "flex items-center justify-between gap-2 sm:hidden",
                                            span { class: "text-xs text-neutral-400 uppercase tracking-wider", "Emergency Stop Control" }
                                            button {
                                                class: "bg-red-900/80 hover:bg-red-800 border border-red-700 rounded px-3 py-1 min-h-11 text-xs font-semibold",
                                                onclick: move |_| stop_form_open.set(!stop_form_open()),
                                                if stop_form_open() { "Close" } else { "Global Stop…" }
                                            }
                                        }
                                        div {
                                            class: if stop_form_open() {
                                                "flex flex-col sm:flex-row gap-2 flex-1"
                                            } else {
                                                "hidden sm:flex sm:flex-row gap-2 flex-1"
                                            },
                                            if !is_session_auth {
                                                input {
                                                    class: "bg-neutral-900 border border-neutral-700 rounded px-2 py-1 min-h-11 text-base sm:text-sm sm:w-40",
                                                    value: "{stop_by}",
                                                    oninput: move |e| stop_by.set(e.value()),
                                                    placeholder: "your name",
                                                }
                                            }
                                            input {
                                                class: "bg-neutral-900 border border-neutral-700 rounded px-2 py-1 min-h-11 flex-1 text-base sm:text-sm",
                                                value: "{stop_reason}",
                                                oninput: move |e| stop_reason.set(e.value()),
                                                placeholder: "reason for global stop",
                                            }
                                            button {
                                                class: "bg-red-800 hover:bg-red-700 rounded px-4 py-1 min-h-11 w-full sm:w-auto disabled:opacity-45 disabled:cursor-not-allowed font-semibold",
                                                disabled: stop_busy() || !actor_ready || stop_reason().trim().is_empty(),
                                                onclick: on_global_stop,
                                                if stop_busy() { "Stopping…" } else { "Global Stop" }
                                            }
                                        }
                                    }
                                },
                            }
                        }
                    }
                }
            }

            if let Some(err) = action_error() {
                if !has_selection {
                    p { class: "mb-4 text-red-400 break-words", "action failed: {err}" }
                }
            }

            {
                let active_alerts: Vec<(String, String, String)> = match &*rows.read() {
                    FetchState::Loaded(list) => collect_overtime_alerts(list)
                        .into_iter()
                        .filter(|(id, _, _)| !dismissed_alerts.read().contains(id))
                        .collect(),
                    _ => Vec::new(),
                };
                if !active_alerts.is_empty() {
                    let perm = notif_perm();
                    rsx! {
                        div { class: "mb-4 flex flex-col gap-2",
                            for (alert_id, alert_obj, alert_msg) in active_alerts {
                                {
                                    let inspect_id = alert_id.clone();
                                    let dismiss_id = alert_id.clone();
                                    let short = workflow_ui::short_id(&alert_id);
                                    rsx! {
                                        div {
                                            key: "{alert_id}",
                                            class: "rounded-lg border border-amber-500/75 bg-amber-950/40 px-3.5 py-2.5 flex flex-wrap items-center justify-between gap-2 text-xs",
                                            div { class: "min-w-0 flex-1",
                                                div { class: "flex flex-wrap items-center gap-2",
                                                    span { class: "font-semibold text-amber-300", "⚠ Overtime Alert · #{short}" }
                                                    span { class: "text-amber-100", "{alert_msg}" }
                                                }
                                                p { class: "text-[11px] text-amber-200/80 truncate mt-0.5", "{alert_obj}" }
                                            }
                                            div { class: "flex items-center gap-2 shrink-0",
                                                if perm.as_deref() == Some("default") {
                                                    button {
                                                        r#type: "button",
                                                        class: "rounded px-2.5 py-1 text-[11px] border border-amber-600/60 bg-amber-950/60 text-amber-200 hover:bg-amber-900/60",
                                                        onclick: move |_| request_notification_permission(notif_perm),
                                                        "Desktop Alerts"
                                                    }
                                                }
                                                button {
                                                    r#type: "button",
                                                    class: "rounded px-2.5 py-1 text-[11px] font-semibold bg-amber-500 text-[#0C1118] hover:bg-amber-400",
                                                    onclick: move |_| {
                                                        inspector_wide.set(false);
                                                        selected.set(Some(inspect_id.clone()));
                                                    },
                                                    "Inspect →"
                                                }
                                                button {
                                                    r#type: "button",
                                                    class: "rounded px-2 py-1 text-[11px] text-amber-200/80 hover:text-white",
                                                    onclick: move |_| {
                                                        dismissed_alerts.write().insert(dismiss_id.clone());
                                                    },
                                                    "Dismiss"
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                } else {
                    rsx! {}
                }
            }

            div { class: "flex flex-col md:flex-row md:items-start gap-4 lg:gap-5",
                if !(has_selection && inspector_wide()) {
                    div {
                        class: if has_selection {
                            "hidden md:block md:w-80 lg:w-96 xl:w-[25rem] md:shrink-0 md:sticky md:top-4 min-w-0"
                        } else {
                            "flex-1 min-w-0"
                        },
                        workflow_ui::TaskGrid {
                            rows: rows.read().clone(),
                            selected,
                            compact: has_selection,
                            clearing: clear_busy(),
                            clear_error: clear_error(),
                            clear_notice: clear_notice(),
                            on_clear: move |scope: &'static str| {
                                let Some(conn) = connected() else { return };
                                clear_busy.set(true);
                                clear_error.set(None);
                                clear_notice.set(None);
                                spawn(async move {
                                    match api_post_json::<ClearOutcome>(
                                        &conn,
                                        "/api/tasks/clear",
                                        serde_json::json!({ "scope": scope }),
                                    )
                                    .await
                                    {
                                        Ok(outcome) => {
                                            let current_sel = selected.peek().clone();
                                            if let Some(sel) = current_sel {
                                                if outcome.deleted_ids.iter().any(|id| id == &sel) {
                                                    inspector_wide.set(false);
                                                    selected.set(None);
                                                    detail.set(FetchState::Idle);
                                                }
                                            }
                                            let deleted = outcome.deleted_ids.len();
                                            let skipped = outcome.skipped.len();
                                            let msg = if skipped > 0 {
                                                format!(
                                                    "Cleared {deleted} task(s); skipped {skipped} still needed by active/surviving work."
                                                )
                                            } else {
                                                format!("Cleared {deleted} task(s).")
                                            };
                                            clear_notice.set(Some(msg));
                                            refresh_rows(conn);
                                        }
                                        Err(err) => clear_error.set(Some(err)),
                                    }
                                    clear_busy.set(false);
                                });
                            },
                        }
                    }
                }
                if has_selection {
                    div {
                        class: "flex-1 min-w-0 w-full",
                        button {
                            r#type: "button",
                            class: "md:hidden mb-3 w-full bg-[#0E151F] hover:bg-[#202D3A]/60 border border-[#202D3A] rounded px-3 py-2 min-h-11 text-left text-xs font-semibold text-[#66D8CE]",
                            onclick: move |_| {
                                inspector_wide.set(false);
                                selected.set(None);
                            },
                            "← Back to Workflows & Tasks"
                        }
                        match &*detail.read() {
                            FetchState::Idle | FetchState::Loading => rsx! {
                                div { class: "cf-surface rounded-lg p-6 text-xs text-[#8D9DAD]",
                                    "Loading task telemetry…"
                                }
                            },
                            FetchState::Error(err) => rsx! {
                                div { class: "rounded-lg border border-red-800 bg-red-950/35 p-4 text-xs text-red-300 break-words",
                                    "Error loading task: {err}"
                                }
                            },
                            FetchState::Loaded(task) => rsx! {
                                workflow_ui::TaskInspector {
                                    task: task.clone(),
                                    actor,
                                    session_principal: active_conn.as_ref().and_then(|c| c.session_principal().map(str::to_string)),
                                    busy: busy(),
                                    action_error: action_error(),
                                    wide: inspector_wide,
                                    on_close: move |_| selected.set(None),
                                    on_select_task: move |target_id: String| {
                                        inspector_wide.set(false);
                                        selected.set(Some(target_id));
                                    },
                                    on_approve: move |_| {
                                        let task = match &*detail.read() { FetchState::Loaded(t) => t.clone(), _ => return };
                                        run_action(
                                            format!("/api/tasks/{}/approve", task.id),
                                            serde_json::json!({"revision": task.revision, "digest": task.assignment_digest, "by": actor()}),
                                            true,
                                        );
                                    },
                                    on_approve_and_run: move |_| {
                                        let task = match &*detail.read() { FetchState::Loaded(t) => t.clone(), _ => return };
                                        run_approve_and_run(task.id, task.revision, task.assignment_digest, actor());
                                    },
                                    on_run: move |_| {
                                        let task = match &*detail.read() { FetchState::Loaded(t) => t.clone(), _ => return };
                                        run_action(
                                            format!("/api/tasks/{}/run", task.id),
                                            serde_json::json!({"revision": task.revision}),
                                            true,
                                        );
                                    },
                                    on_cancel: move |_| {
                                        let task = match &*detail.read() { FetchState::Loaded(t) => t.clone(), _ => return };
                                        let Some(conn) = connected() else { return };
                                        let by = actor();
                                        let by_opt = if by.is_empty() { None } else { Some(by) };
                                        action_error.set(None);
                                        spawn(async move {
                                            let path = format!("/api/tasks/{}/cancel", task.id);
                                            let mut attempt_rev = task.revision;
                                            let mut final_res: Option<Result<TaskDetail, String>> = None;
                                            for _ in 0..5 {
                                                let res = api_post(
                                                    &conn,
                                                    &path,
                                                    serde_json::json!({"revision": attempt_rev, "by": by_opt}),
                                                )
                                                .await;
                                                match res {
                                                    Err(ref err) if err.contains("revision conflict") => {
                                                        match api_get::<TaskDetail>(&conn, &format!("/api/tasks/{}", task.id)).await {
                                                            Ok(fresh) => {
                                                                let cancellable = matches!(
                                                                    fresh.status.as_str(),
                                                                    "awaiting_approval" | "approved" | "preparing_workspace" | "running" | "verifying"
                                                                );
                                                                if !cancellable {
                                                                    final_res = Some(Ok(fresh));
                                                                    break;
                                                                }
                                                                attempt_rev = fresh.revision;
                                                                final_res = Some(Err(err.clone()));
                                                            }
                                                            Err(_) => {
                                                                final_res = Some(res);
                                                                break;
                                                            }
                                                        }
                                                    }
                                                    other => {
                                                        final_res = Some(other);
                                                        break;
                                                    }
                                                }
                                            }
                                            match final_res.unwrap_or_else(|| Err("409 Conflict: workflow revision conflict".into())) {
                                                Ok(updated) => {
                                                    if selected.peek().as_deref() == Some(updated.id.as_str()) {
                                                        detail.set(FetchState::Loaded(updated));
                                                    }
                                                    refresh_rows(conn);
                                                }
                                                Err(err) => action_error.set(Some(err)),
                                            }
                                        });
                                    },
                                    on_delete: move |_| {
                                        let task = match &*detail.read() { FetchState::Loaded(t) => t.clone(), _ => return };
                                        let Some(conn) = connected() else { return };
                                        action_error.set(None);
                                        busy.set(true);
                                        spawn(async move {
                                            let path = format!("/api/tasks/{}/delete", task.id);
                                            match api_post_json::<DeleteOutcome>(
                                                &conn,
                                                &path,
                                                serde_json::json!({}),
                                            )
                                            .await
                                            {
                                                Ok(outcome) => {
                                                    if selected.peek().as_deref() == Some(outcome.deleted_id.as_str()) {
                                                        inspector_wide.set(false);
                                                        selected.set(None);
                                                        detail.set(FetchState::Idle);
                                                    }
                                                    refresh_rows(conn);
                                                }
                                                Err(err) => action_error.set(Some(err)),
                                            }
                                            busy.set(false);
                                        });
                                    },
                                    on_review: move |accept: bool| {
                                        let task = match &*detail.read() { FetchState::Loaded(t) => t.clone(), _ => return };
                                        let Some(digest) = task.evidence_digest.clone() else { return };
                                        run_action(
                                            format!("/api/tasks/{}/review", task.id),
                                            serde_json::json!({"revision": task.revision, "digest": digest, "by": actor(), "accept": accept}),
                                            true,
                                        );
                                    },
                                    on_prepare_integration: move |target: String| {
                                        let task = match &*detail.read() { FetchState::Loaded(t) => t.clone(), _ => return };
                                        run_action(
                                            format!("/api/tasks/{}/integration/prepare", task.id),
                                            serde_json::json!({"by": actor(), "target": target}),
                                            true,
                                        );
                                    },
                                    on_integrate: move |rollback: bool| {
                                        let task = match &*detail.read() { FetchState::Loaded(t) => t.clone(), _ => return };
                                        let Some(plan) = task.integration else { return };
                                        run_action(
                                            format!("/api/tasks/{}/integration", task.id),
                                            serde_json::json!({"revision": plan.revision, "digest": plan.plan_digest, "by": actor(), "rollback": rollback}),
                                            true,
                                        );
                                    },
                                    on_depend,
                                    on_retry: move |(reset_workspace, from_step, guidance): (bool, Option<usize>, Option<String>)| {
                                        let task = match &*detail.read() { FetchState::Loaded(t) => t.clone(), _ => return };
                                        let by = {
                                            let raw = actor().trim().to_string();
                                            if raw.is_empty() {
                                                connected()
                                                    .as_ref()
                                                    .and_then(|c| c.session_principal())
                                                    .unwrap_or("desktop-operator")
                                                    .to_string()
                                            } else {
                                                raw
                                            }
                                        };
                                        run_retry_and_run(task.id, task.revision, by, reset_workspace, from_step, guidance);
                                    },
                                }
                            },
                        }
                    }
                }
            }
        }
    }
}

#[derive(Clone, PartialEq, Deserialize)]
struct ProbeReport {
    repository: String,
    git_clean: bool,
    git_base_commit: String,
    git_branch: Option<String>,
    detected_verifier: Option<DetectedVerifier>,
    #[serde(default)]
    additional_verifiers: Vec<DetectedVerifier>,
    detected_verification_files: Vec<String>,
    suggested_allowed_files: Vec<String>,
    #[serde(default)]
    total_source_files: usize,
    detected_clis: Vec<DetectedCli>,
    ollama_running: bool,
    available_models: Vec<OllamaModelInfo>,
    recommended_model: Option<String>,
}

fn parse_file_list(raw: &str) -> Vec<String> {
    let mut cleaned = Vec::new();
    for piece in raw.split([',', '\n', '\r']) {
        let normalized = piece.trim().replace('\\', "/");
        if !normalized.is_empty() && !cleaned.iter().any(|existing| existing == &normalized) {
            cleaned.push(normalized);
        }
    }
    cleaned
}

#[derive(Clone, PartialEq, Deserialize)]
struct DetectedVerifier {
    executable: String,
    args: Vec<String>,
    framework: String,
}

#[derive(Clone, PartialEq, Deserialize)]
struct DetectedCli {
    name: String,
    available: bool,
    details: String,
}

#[derive(Clone, PartialEq, Deserialize)]
struct OllamaModelInfo {
    name: String,
    digest: String,
    size_bytes: Option<u64>,
}

#[component]
fn WorkflowIntake(
    config: ApiConnection,
    actor: Signal<String>,
    on_created: EventHandler<String>,
) -> Element {
    let mut profiles = use_signal(Vec::<WorkflowProfile>::new);
    let mut chosen = use_signal(String::new);
    let mut exec_secs = use_signal(|| "600".to_string());
    let mut idle_secs = use_signal(|| "360".to_string());
    let mut verify_secs = use_signal(|| "180".to_string());
    let mut soft_timeouts = use_signal(|| true);
    let mut require_clean_checkout = use_signal(|| false);
    let mut profile_low_spec = use_signal(|| false);
    let mut profile_busy = use_signal(|| false);
    let mut saving_defaults = use_signal(|| false);
    let mut profile_notice = use_signal(|| None::<String>);
    let mut profile_error = use_signal(|| None::<String>);
    let repo_files = use_signal(Vec::<String>::new);
    let repo_status = use_signal(|| None::<ProfileRepoStatus>);
    let mut scope_manually_edited = use_signal(|| false);
    let mut auto_scope_note = use_signal(|| None::<String>);
    let mut suggested_matches = use_signal(Vec::<SuggestedFileItem>::new);
    let mut indexed_file_count = use_signal(|| None::<usize>);
    let suggest_seq = use_signal(|| 0_u64);
    let mut file_search = use_signal(String::new);
    let mut objective = use_signal(String::new);
    let mut acceptance = use_signal(String::new);
    let mut dispatch_allowed_files = use_signal(String::new);
    let mut busy = use_signal(|| false);
    let mut busy_launch = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);

    let mut onboard_repo = use_signal(|| ".".to_string());
    let mut onboard_name = use_signal(String::new);
    let mut onboard_model = use_signal(String::new);
    let mut onboard_allowed_files = use_signal(String::new);
    let mut onboard_low_spec = use_signal(|| false);
    let mut onboard_probe = use_signal(|| None::<ProbeReport>);
    let mut onboard_verifier_idx = use_signal(|| 0usize);
    let mut onboard_probing = use_signal(|| false);
    let mut onboard_error = use_signal(|| None::<String>);
    let mut onboard_success = use_signal(|| None::<String>);
    let mut active_onboard_job = use_signal(|| None::<String>);
    let mut onboard_progress_state = use_signal(|| None::<OnboardProgressUi>);

    let fetch_config = config.clone();
    use_resource(move || {
        let config = fetch_config.clone();
        async move {
            match api_get::<Vec<WorkflowProfile>>(&config, "/api/workflow-profiles").await {
                Ok(list) => {
                    let active_name = if chosen().is_empty() {
                        if let Some(p) = list.first() {
                            chosen.set(p.name.clone());
                            apply_profile_timeout_fields(
                                p,
                                exec_secs,
                                idle_secs,
                                verify_secs,
                                profile_low_spec,
                                soft_timeouts,
                                require_clean_checkout,
                            );
                            indexed_file_count.set(p.indexed_files);
                            p.name.clone()
                        } else {
                            String::new()
                        }
                    } else {
                        if let Some(p) = list.iter().find(|p| p.name == chosen()) {
                            apply_profile_timeout_fields(
                                p,
                                exec_secs,
                                idle_secs,
                                verify_secs,
                                profile_low_spec,
                                soft_timeouts,
                                require_clean_checkout,
                            );
                            indexed_file_count.set(p.indexed_files);
                        }
                        chosen()
                    };
                    if !active_name.is_empty() {
                        spawn_load_profile_files(
                            config.clone(),
                            active_name.clone(),
                            chosen,
                            repo_files,
                        );
                        spawn_load_profile_repo_status(
                            config.clone(),
                            active_name,
                            chosen,
                            repo_status,
                        );
                    }
                    profiles.set(list);
                }
                Err(e) => error.set(Some(e)),
            }
        }
    });

    let probe_submit = {
        let config = config.clone();
        move |_| {
            let config = config.clone();
            let repo_path = onboard_repo();
            onboard_probing.set(true);
            onboard_error.set(None);
            onboard_success.set(None);
            spawn(async move {
                match api_post_json::<ProbeReport>(
                    &config,
                    "/api/onboard/probe",
                    serde_json::json!({"repository": repo_path}),
                )
                .await
                {
                    Ok(report) => {
                        let default_model = report
                            .recommended_model
                            .clone()
                            .or_else(|| report.available_models.first().map(|m| m.name.clone()))
                            .unwrap_or_default();
                        onboard_model.set(default_model);

                        let repo_name = {
                            let base = workflow_ui::repo_basename(&report.repository);
                            if base.is_empty() {
                                "project".to_string()
                            } else {
                                base.to_string()
                            }
                        };
                        onboard_name.set(repo_name);
                        onboard_allowed_files.set(report.suggested_allowed_files.join("\n"));
                        onboard_verifier_idx.set(0);

                        onboard_probe.set(Some(report));
                    }
                    Err(e) => onboard_error.set(Some(e)),
                }
                onboard_probing.set(false);
            });
        }
    };

    let onboard_save = {
        let config = config.clone();
        move |_| {
            let Some(report) = onboard_probe() else {
                return;
            };
            let config = config.clone();
            let prof_name = onboard_name();
            let model_name = onboard_model();
            let low_spec = onboard_low_spec();

            onboard_probing.set(true);
            onboard_error.set(None);
            let progress_id = format!("onboard-{}", js_sys::Date::now() as u64);
            active_onboard_job.set(Some(progress_id.clone()));
            onboard_progress_state.set(Some(OnboardProgressUi {
                job_id: progress_id.clone(),
                profile_name: prof_name.clone(),
                stage: "starting".into(),
                detail: format!("Starting onboard for '{prof_name}'"),
                percent: 2,
                indexed_files: 0,
                total_index_files: 0,
                finished: false,
                error: None,
            }));
            spawn_poll_onboard_progress(
                config.clone(),
                progress_id.clone(),
                active_onboard_job,
                onboard_progress_state,
            );
            let picked_idx = onboard_verifier_idx();
            spawn(async move {
                let mut verifier_choices: Vec<&DetectedVerifier> = Vec::new();
                if let Some(v) = &report.detected_verifier {
                    verifier_choices.push(v);
                }
                for v in &report.additional_verifiers {
                    verifier_choices.push(v);
                }
                let selected_verifier = verifier_choices
                    .get(picked_idx)
                    .copied()
                    .or(report.detected_verifier.as_ref());
                let verifier_exe = selected_verifier
                    .map(|v| v.executable.clone())
                    .unwrap_or_else(|| "cargo".into());
                let verifier_args = selected_verifier
                    .map(|v| v.args.clone())
                    .unwrap_or_else(|| vec!["test".into(), "--locked".into()]);
                let verification_files = if report.detected_verification_files.is_empty() {
                    vec!["Cargo.toml".into()]
                } else {
                    report.detected_verification_files.clone()
                };
                let edited_allowed = parse_file_list(&onboard_allowed_files());
                let allowed_files = if !edited_allowed.is_empty() {
                    edited_allowed
                } else if report.suggested_allowed_files.is_empty() {
                    vec!["README.md".into()]
                } else {
                    report.suggested_allowed_files.clone()
                };

                let body = serde_json::json!({
                    "profile_name": prof_name,
                    "repository_path": report.repository,
                    "model": model_name,
                    "endpoint": null,
                    "verifier_executable": verifier_exe,
                    "verifier_args": verifier_args,
                    "verification_files": verification_files,
                    "allowed_files": allowed_files,
                    "workers": [],
                    "low_spec": low_spec,
                    "by": config.session_principal().unwrap_or("desktop-operator")
                });

                let onboard_path = format!("/api/onboard?progress_id={progress_id}");
                match api_post_json::<WorkflowProfile>(&config, &onboard_path, body).await {
                    Ok(created_profile) => {
                        let indexed_label = created_profile
                            .indexed_files
                            .map(|n| {
                                let noun = if n == 1 { "file" } else { "files" };
                                format!(" · {n} repo {noun} indexed")
                            })
                            .unwrap_or_default();
                        let scope_noun = if created_profile.scope.len() == 1 {
                            "file"
                        } else {
                            "files"
                        };
                        onboard_success.set(Some(format!(
                            "Onboarded profile '{}' ({} default scope {scope_noun}{})!",
                            created_profile.name,
                            created_profile.scope.len(),
                            indexed_label,
                        )));
                        apply_profile_timeout_fields(
                            &created_profile,
                            exec_secs,
                            idle_secs,
                            verify_secs,
                            profile_low_spec,
                            soft_timeouts,
                            require_clean_checkout,
                        );
                        indexed_file_count.set(created_profile.indexed_files);
                        let mut current_list = profiles();
                        current_list.retain(|p| p.name != created_profile.name);
                        current_list.push(created_profile.clone());
                        current_list.sort_by(|a, b| a.name.cmp(&b.name));
                        profiles.set(current_list);
                        let created_name = created_profile.name.clone();
                        chosen.set(created_name.clone());
                        spawn_load_profile_files(
                            config.clone(),
                            created_name.clone(),
                            chosen,
                            repo_files,
                        );
                        spawn_load_profile_repo_status(
                            config.clone(),
                            created_name,
                            chosen,
                            repo_status,
                        );
                        profile_notice.set(None);
                        profile_error.set(None);
                        onboard_probe.set(None);
                    }
                    Err(e) => onboard_error.set(Some(e)),
                }
                active_onboard_job.set(None);
                onboard_progress_state.set(None);
                onboard_probing.set(false);
            });
        }
    };

    let save_profile_timeouts = {
        let config = config.clone();
        move |_| {
            let profile_name = chosen();
            if profile_name.is_empty() {
                return;
            }
            let soft = soft_timeouts();
            let clean_required = require_clean_checkout();
            let (gen_ms, idle_ms, verify_ms) =
                match parse_dispatch_timeouts(&exec_secs(), &idle_secs(), &verify_secs(), soft) {
                    Ok(parsed) => parsed,
                    Err(err) => {
                        profile_error.set(Some(err));
                        profile_notice.set(None);
                        return;
                    }
                };
            let config = config.clone();
            saving_defaults.set(true);
            profile_error.set(None);
            profile_notice.set(None);
            spawn(async move {
                let path = format!("/api/workflow-profiles/{profile_name}/timeouts");
                let body = serde_json::json!({
                    "generation_timeout_ms": gen_ms,
                    "generation_idle_timeout_ms": idle_ms,
                    "verification_timeout_ms": verify_ms,
                    "soft_timeouts": soft,
                    "require_clean_checkout": clean_required,
                });
                match api_post_json::<WorkflowProfile>(&config, &path, body).await {
                    Ok(updated) => {
                        apply_profile_timeout_fields(
                            &updated,
                            exec_secs,
                            idle_secs,
                            verify_secs,
                            profile_low_spec,
                            soft_timeouts,
                            require_clean_checkout,
                        );
                        let mut current_list = profiles();
                        if let Some(slot) = current_list.iter_mut().find(|p| p.name == updated.name)
                        {
                            *slot = updated.clone();
                        }
                        profiles.set(current_list);
                        let mode_label = if updated.soft_timeouts {
                            "alert-only"
                        } else {
                            "hard cap"
                        };
                        let clean_label = if updated.require_clean_checkout {
                            " · clean repo required"
                        } else {
                            ""
                        };
                        profile_notice.set(Some(format!(
                            "Saved defaults for '{}' (exec {}s · idle {}s · verify {}s · {mode_label}{clean_label}).",
                            updated.name,
                            gen_ms / 1000,
                            idle_ms / 1000,
                            verify_ms / 1000,
                        )));
                    }
                    Err(err) => profile_error.set(Some(err)),
                }
                saving_defaults.set(false);
            });
        }
    };

    let reonboard_selected = {
        let config = config.clone();
        move |_| {
            let profile_name = chosen();
            if profile_name.is_empty() {
                return;
            }
            let config = config.clone();
            let low_spec = profile_low_spec();
            let by = {
                let raw = actor().trim().to_string();
                if raw.is_empty() {
                    config
                        .session_principal()
                        .unwrap_or("desktop-operator")
                        .to_string()
                } else {
                    raw
                }
            };
            profile_busy.set(true);
            profile_error.set(None);
            profile_notice.set(None);
            let progress_id = format!("reonboard-{}", js_sys::Date::now() as u64);
            active_onboard_job.set(Some(progress_id.clone()));
            onboard_progress_state.set(Some(OnboardProgressUi {
                job_id: progress_id.clone(),
                profile_name: profile_name.clone(),
                stage: "starting".into(),
                detail: format!("Starting re-onboard for '{profile_name}'"),
                percent: 2,
                indexed_files: 0,
                total_index_files: 0,
                finished: false,
                error: None,
            }));
            spawn_poll_onboard_progress(
                config.clone(),
                progress_id.clone(),
                active_onboard_job,
                onboard_progress_state,
            );
            spawn(async move {
                let path = format!(
                    "/api/workflow-profiles/{profile_name}/re-onboard?progress_id={progress_id}"
                );
                let body = serde_json::json!({
                    "low_spec": low_spec,
                    "refresh_scope": true,
                    "refresh_verifier": true,
                    "by": by,
                });
                match api_post_json::<WorkflowProfile>(&config, &path, body).await {
                    Ok(updated) => {
                        apply_profile_timeout_fields(
                            &updated,
                            exec_secs,
                            idle_secs,
                            verify_secs,
                            profile_low_spec,
                            soft_timeouts,
                            require_clean_checkout,
                        );
                        indexed_file_count.set(updated.indexed_files);
                        let mut current_list = profiles();
                        if let Some(slot) = current_list.iter_mut().find(|p| p.name == updated.name)
                        {
                            *slot = updated.clone();
                        } else {
                            current_list.push(updated.clone());
                            current_list.sort_by(|a, b| a.name.cmp(&b.name));
                        }
                        profiles.set(current_list);
                        spawn_load_profile_files(
                            config.clone(),
                            updated.name.clone(),
                            chosen,
                            repo_files,
                        );
                        spawn_load_profile_repo_status(
                            config.clone(),
                            updated.name.clone(),
                            chosen,
                            repo_status,
                        );
                        let indexed_label = updated
                            .indexed_files
                            .map(|n| format!(" · {n} indexed"))
                            .unwrap_or_default();
                        profile_notice.set(Some(format!(
                            "Re-onboarded '{}' ({} default scope{} · exec {}s · idle {}s · verify {}s).",
                            updated.name,
                            updated.scope.len(),
                            indexed_label,
                            updated.generation_timeout_ms.unwrap_or(600_000) / 1000,
                            updated.generation_idle_timeout_ms.unwrap_or(360_000) / 1000,
                            updated.verification_timeout_ms.unwrap_or(180_000) / 1000,
                        )));
                        if !objective().trim().is_empty() && !scope_manually_edited() {
                            spawn_suggest_files(
                                config.clone(),
                                updated.name.clone(),
                                objective(),
                                acceptance(),
                                SuggestFilesTarget {
                                    chosen,
                                    scope_manually_edited,
                                    suggest_seq,
                                    dispatch_allowed_files,
                                    suggested_matches,
                                    indexed_file_count,
                                    auto_scope_note,
                                },
                            );
                        }
                    }
                    Err(err) => profile_error.set(Some(err)),
                }
                active_onboard_job.set(None);
                onboard_progress_state.set(None);
                profile_busy.set(false);
            });
        }
    };

    let submit_prepare = {
        let config = config.clone();
        move |_| {
            let soft = soft_timeouts();
            let clean_required = require_clean_checkout();
            let (gen_ms, idle_ms, verify_ms) =
                match parse_dispatch_timeouts(&exec_secs(), &idle_secs(), &verify_secs(), soft) {
                    Ok(parsed) => parsed,
                    Err(err) => {
                        error.set(Some(err));
                        return;
                    }
                };
            let config = config.clone();
            let profile = chosen();
            let goal = objective();
            let checks = acceptance();
            let mut scope = parse_file_list(&dispatch_allowed_files());
            let manual_override = scope_manually_edited();
            if scope.is_empty() && !manual_override {
                let p_scope = profiles()
                    .into_iter()
                    .find(|p| p.name == profile)
                    .map(|p| p.scope)
                    .unwrap_or_default();
                if let Some(auto) = extract_objective_repo_files(&goal, &repo_files(), &p_scope) {
                    let mut merged = auto;
                    for pf in p_scope {
                        if merged.len() >= 64 {
                            break;
                        }
                        if !merged.iter().any(|existing| existing == &pf) {
                            merged.push(pf);
                        }
                    }
                    scope = merged;
                }
            }
            let auto_scope = !manual_override && scope.is_empty();
            busy.set(true);
            busy_launch.set(false);
            error.set(None);
            spawn(async move {
                match api_post_json::<TaskDetail>(
                    &config,
                    "/api/workflows",
                    serde_json::json!({
                        "profile": profile,
                        "objective": goal,
                        "acceptance": checks,
                        "allowed_files": scope,
                        "auto_scope": auto_scope,
                        "generation_timeout_ms": gen_ms,
                        "generation_idle_timeout_ms": idle_ms,
                        "verification_timeout_ms": verify_ms,
                        "soft_timeouts": soft,
                        "require_clean_checkout": clean_required,
                    }),
                )
                .await
                {
                    Ok(task) => {
                        on_created.call(task.id);
                        objective.set(String::new());
                        acceptance.set(String::new());
                        dispatch_allowed_files.set(String::new());
                        scope_manually_edited.set(false);
                        suggested_matches.set(Vec::new());
                        auto_scope_note.set(None);
                        file_search.set(String::new());
                    }
                    Err(e) => error.set(Some(e)),
                }
                busy.set(false);
            });
        }
    };

    let submit_dispatch_and_run = {
        let config = config.clone();
        move |_| {
            let soft = soft_timeouts();
            let clean_required = require_clean_checkout();
            let (gen_ms, idle_ms, verify_ms) =
                match parse_dispatch_timeouts(&exec_secs(), &idle_secs(), &verify_secs(), soft) {
                    Ok(parsed) => parsed,
                    Err(err) => {
                        error.set(Some(err));
                        return;
                    }
                };
            let config = config.clone();
            let profile = chosen();
            let goal = objective();
            let checks = acceptance();
            let mut scope = parse_file_list(&dispatch_allowed_files());
            let manual_override = scope_manually_edited();
            if scope.is_empty() && !manual_override {
                let p_scope = profiles()
                    .into_iter()
                    .find(|p| p.name == profile)
                    .map(|p| p.scope)
                    .unwrap_or_default();
                if let Some(auto) = extract_objective_repo_files(&goal, &repo_files(), &p_scope) {
                    let mut merged = auto;
                    for pf in p_scope {
                        if merged.len() >= 64 {
                            break;
                        }
                        if !merged.iter().any(|existing| existing == &pf) {
                            merged.push(pf);
                        }
                    }
                    scope = merged;
                }
            }
            let auto_scope = !manual_override && scope.is_empty();
            let by = {
                let raw = actor().trim().to_string();
                if raw.is_empty() {
                    config.session_principal().unwrap_or("operator").to_string()
                } else {
                    raw
                }
            };
            busy.set(true);
            busy_launch.set(true);
            error.set(None);
            spawn(async move {
                match api_post_json::<TaskDetail>(
                    &config,
                    "/api/workflows",
                    serde_json::json!({
                        "profile": profile,
                        "objective": goal,
                        "acceptance": checks,
                        "allowed_files": scope,
                        "auto_scope": auto_scope,
                        "generation_timeout_ms": gen_ms,
                        "generation_idle_timeout_ms": idle_ms,
                        "verification_timeout_ms": verify_ms,
                        "soft_timeouts": soft,
                        "require_clean_checkout": clean_required,
                    }),
                )
                .await
                {
                    Ok(created) => {
                        let task_id = created.id.clone();
                        on_created.call(task_id.clone());
                        objective.set(String::new());
                        acceptance.set(String::new());
                        dispatch_allowed_files.set(String::new());
                        scope_manually_edited.set(false);
                        suggested_matches.set(Vec::new());
                        auto_scope_note.set(None);
                        file_search.set(String::new());
                        match api_post(
                            &config,
                            &format!("/api/tasks/{task_id}/approve"),
                            serde_json::json!({
                                "revision": created.revision,
                                "digest": created.assignment_digest,
                                "by": by,
                            }),
                        )
                        .await
                        {
                            Ok(approved) => {
                                if let Err(e) = api_post(
                                    &config,
                                    &format!("/api/tasks/{task_id}/run"),
                                    serde_json::json!({"revision": approved.revision}),
                                )
                                .await
                                {
                                    error.set(Some(format!(
                                        "Prepared & approved, but start failed: {e}"
                                    )));
                                }
                            }
                            Err(e) => error.set(Some(format!("Prepared, but approve failed: {e}"))),
                        }
                    }
                    Err(e) => error.set(Some(e)),
                }
                busy.set(false);
                busy_launch.set(false);
            });
        }
    };

    rsx! {
        div { class: "mb-4",
            details { class: "mb-3 rounded border px-3 py-2", style: "border-color: #202D3A; background-color: rgba(32, 45, 58, 0.35);",
                summary { class: "cursor-pointer font-semibold min-h-11 flex items-center tracking-wider uppercase text-xs", style: "color: #66D8CE;", "+ Onboard Project // Auto-Discovery" }
                p { class: "text-xs mb-2", style: "color: #8D9DAD;", "Point Clatterforge at any local Git repository. It auto-detects the acceptance test framework, probes live CLI models, and configures deterministic zero-footprint or local-LLM routing." }
                div { class: "flex flex-col sm:flex-row gap-2 mb-2",
                    input {
                        class: "bg-neutral-900 border border-neutral-700 rounded px-2 py-1 flex-1 min-h-11 text-base sm:text-sm",
                        value: "{onboard_repo}",
                        oninput: move |e| onboard_repo.set(e.value()),
                        placeholder: "Repository path (e.g. . or C:/path/to/repo)"
                    }
                    button {
                        class: "bg-neutral-800 hover:bg-neutral-700 border border-neutral-600 rounded px-4 py-1 min-h-11 w-full sm:w-auto disabled:opacity-50",
                        disabled: onboard_probing(),
                        onclick: probe_submit,
                        if onboard_probing() { "Scanning…" } else { "Scan Repository" }
                    }
                    if !chosen().is_empty() {
                        button {
                            class: "bg-neutral-900 hover:bg-neutral-800 border border-neutral-700 rounded px-3 py-1 min-h-11 text-xs text-neutral-300 w-full sm:w-auto",
                            onclick: move |_| {
                                let name = chosen();
                                if let Some(p) = profiles().into_iter().find(|p| p.name == name) {
                                    onboard_repo.set(p.repository);
                                    onboard_name.set(p.name);
                                    onboard_low_spec.set(p.low_spec);
                                    onboard_error.set(None);
                                    onboard_success.set(None);
                                }
                            },
                            "Use Selected Profile Repo"
                        }
                    }
                }

                if let Some(report) = onboard_probe() {
                    {
                        let short_commit = report.git_base_commit.chars().take(8).collect::<String>();
                        rsx! {
                            div { class: "rounded border border-neutral-800 bg-neutral-950 p-3 mb-2 text-sm",
                                div { class: "grid grid-cols-1 sm:grid-cols-2 gap-2 mb-2",
                                    div {
                                        span { class: "text-neutral-500", "Repo base: " }
                                        span { class: "font-mono text-neutral-300", "{short_commit}" }
                                        span { class: if report.git_clean { " ml-2 text-emerald-400" } else { " ml-2 text-amber-400" },
                                            if report.git_clean { "(clean)" } else { "(uncommitted changes)" }
                                        }
                                    }
                            div {
                                span { class: "text-neutral-500", "Verifier: " }
                                {
                                    let mut verifier_choices: Vec<DetectedVerifier> = Vec::new();
                                    if let Some(v) = &report.detected_verifier {
                                        verifier_choices.push(v.clone());
                                    }
                                    for v in &report.additional_verifiers {
                                        verifier_choices.push(v.clone());
                                    }
                                    if verifier_choices.len() > 1 {
                                        rsx! {
                                            select {
                                                class: "bg-neutral-900 border border-neutral-700 rounded px-2 py-0.5 text-xs text-sky-300 mt-1 w-full",
                                                value: "{onboard_verifier_idx}",
                                                onchange: move |e| {
                                                    if let Ok(idx) = e.value().parse::<usize>() {
                                                        onboard_verifier_idx.set(idx);
                                                    }
                                                },
                                                for (idx, v) in verifier_choices.iter().enumerate() {
                                                    option { value: "{idx}",
                                                        "{v.framework}: {v.executable} {v.args.join(\" \")}"
                                                    }
                                                }
                                            }
                                        }
                                    } else {
                                        rsx! {
                                            span { class: "text-sky-300",
                                                if let Some(v) = &report.detected_verifier {
                                                    if v.args.is_empty() {
                                                        "{v.framework} ({v.executable})"
                                                    } else {
                                                        "{v.framework} ({v.executable} {v.args.join(\" \")})"
                                                    }
                                                } else {
                                                    "None detected"
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        div { class: "mb-2 flex flex-wrap gap-x-3 gap-y-1",
                            span { class: "text-neutral-500", "Coding CLIs: " }
                            for cli in &report.detected_clis {
                                {
                                    let status_str = if cli.available { "ready" } else { "missing" };
                                    rsx! {
                                        span { class: if cli.available { "text-emerald-400" } else { "text-neutral-600" },
                                            "{cli.name}: {status_str}"
                                        }
                                    }
                                }
                            }
                        }

                        div { class: "flex flex-col sm:flex-row gap-2 mb-2",
                            div { class: "flex-1 min-w-0",
                                label { class: "block text-xs text-neutral-400 mb-1", "Profile name" }
                                input {
                                    class: "bg-neutral-900 border border-neutral-700 rounded px-2 py-1 w-full min-h-11 text-base sm:text-sm",
                                    value: "{onboard_name}",
                                    oninput: move |e| onboard_name.set(e.value()),
                                }
                            }
                            div { class: "flex-1 min-w-0",
                                label { class: "block text-xs text-neutral-400 mb-1", "Routing Engine (Built-in 0 MB or Local LLM)" }
                                if report.available_models.is_empty() {
                                    input {
                                        class: "bg-neutral-900 border border-neutral-700 rounded px-2 py-1 w-full min-h-11 text-base sm:text-sm",
                                        value: "{onboard_model}",
                                        oninput: move |e| onboard_model.set(e.value()),
                                        placeholder: "builtin:router-v1"
                                    }
                                } else {
                                    select {
                                        class: "bg-neutral-900 border border-neutral-700 rounded px-2 py-1 w-full max-w-full min-w-0 min-h-11 text-base sm:text-sm",
                                        value: "{onboard_model}",
                                        onchange: move |e| onboard_model.set(e.value()),
                                        for m in &report.available_models {
                                            option {
                                                value: "{m.name}",
                                                selected: m.name == onboard_model(),
                                                if m.name == "builtin:router-v1" {
                                                    "builtin:router-v1 (Zero-Footprint Deterministic Host Router)"
                                                } else {
                                                    "{m.name}"
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        div { class: "mb-2",
                            {
                                let selected_count = parse_file_list(&onboard_allowed_files()).len();
                                let src_noun = if report.total_source_files == 1 { "file" } else { "files" };
                                rsx! {
                                    label { class: "block text-xs text-neutral-400 mb-1",
                                        "Editable files ({selected_count} / 64 selected; {report.total_source_files} source {src_noun} in repo)"
                                    }
                                    textarea {
                                        class: "bg-neutral-900 border border-neutral-700 rounded px-2 py-1.5 w-full font-mono text-xs text-neutral-200",
                                        rows: 4,
                                        value: "{onboard_allowed_files}",
                                        oninput: move |e| onboard_allowed_files.set(e.value()),
                                        placeholder: "One repo-relative path per line (e.g. crates/core/src/lib.rs)"
                                    }
                                }
                            }
                        }

                        label { r#for: "low_spec_cb", class: "flex items-center gap-2 mb-3 min-h-11 text-xs text-neutral-400 cursor-pointer",
                            input {
                                r#type: "checkbox",
                                id: "low_spec_cb",
                                class: "w-4 h-4 shrink-0",
                                checked: onboard_low_spec(),
                                onchange: move |e| onboard_low_spec.set(e.checked()),
                            }
                            span { "Low-spec machine profile (relaxed decision timeouts & memory bounds for laptop CPU/GPU)" }
                        }

                        {
                            let updating_existing = profiles().iter().any(|p| p.name == onboard_name());
                            rsx! {
                                button {
                                    class: "cf-action-btn rounded px-4 py-1 min-h-11 font-bold text-[#0C1118] w-full sm:w-auto",
                                    disabled: onboard_probing() || onboard_name().is_empty() || onboard_model().is_empty(),
                                    onclick: onboard_save,
                                    if onboard_probing() {
                                        if updating_existing { "Re-Onboarding…" } else { "Creating Profile…" }
                                    } else if updating_existing {
                                        "Re-Onboard / Update Profile"
                                    } else {
                                        "Create Project Profile"
                                    }
                                }
                            }
                        }
                    }
                    }
                    }
                }

                if onboard_probing() {
                    if let Some(prog) = onboard_progress_state() {
                        OnboardProgressBar { progress: prog }
                    }
                }
                if let Some(err) = onboard_error() {
                    p { class: "text-red-400 text-xs mt-1 break-words", "{err}" }
                }
                if let Some(msg) = onboard_success() {
                    p { class: "text-emerald-400 text-xs mt-1", "{msg}" }
                }
            }

            details { class: "rounded border px-3 py-2", style: "border-color: #202D3A; background-color: rgba(32, 45, 58, 0.25);", open: true,
                summary { class: "cursor-pointer font-semibold min-h-11 flex items-center tracking-wider uppercase text-xs", style: "color: #FF754F;", "01 // Dispatch Objective" }
                p { class: "mb-2 text-xs", style: "color: #8D9DAD;", "Select an approved profile. Clatterforge classifies complexity, queries live CLI models, executes within isolated worktree boundaries, and verifies every checkpoint." }
                div { class: "flex flex-col sm:flex-row gap-2 mb-2",
                    select {
                        class: "bg-neutral-900 border border-neutral-700 rounded px-2 py-1 min-h-11 flex-1 min-w-0 text-base sm:text-sm",
                        value: "{chosen}",
                        onchange: {
                            let config = config.clone();
                            move |e| {
                                let val = e.value();
                                chosen.set(val.clone());
                                profile_notice.set(None);
                                profile_error.set(None);
                                scope_manually_edited.set(false);
                                suggested_matches.set(Vec::new());
                                indexed_file_count.set(None);
                                auto_scope_note.set(None);
                                dispatch_allowed_files.set(String::new());
                                if let Some(p) = profiles().iter().find(|p| p.name == val) {
                                    apply_profile_timeout_fields(
                                        p,
                                        exec_secs,
                                        idle_secs,
                                        verify_secs,
                                        profile_low_spec,
                                        soft_timeouts,
                                        require_clean_checkout,
                                    );
                                    indexed_file_count.set(p.indexed_files);
                                } else {
                                    indexed_file_count.set(None);
                                }
                                spawn_load_profile_files(config.clone(), val.clone(), chosen, repo_files);
                                spawn_load_profile_repo_status(config.clone(), val.clone(), chosen, repo_status);
                                if !objective().trim().is_empty() {
                                    spawn_suggest_files(
                                        config.clone(),
                                        val,
                                        objective(),
                                        acceptance(),
                                        SuggestFilesTarget {
                                            chosen,
                                            scope_manually_edited,
                                            suggest_seq,
                                            dispatch_allowed_files,
                                            suggested_matches,
                                            indexed_file_count,
                                            auto_scope_note,
                                        },
                                    );
                                }
                            }
                        },
                        for p in profiles() {
                            option {
                                value: "{p.name}",
                                selected: p.name == chosen(),
                                {format!("{} - {} ({}, up to {} steps)", p.name, p.repository, p.model.as_deref().unwrap_or("builtin:router-v1"), p.max_steps)}
                            }
                        }
                        if profiles().is_empty() {
                            option { value: "", disabled: true, selected: true,
                                "No project profiles yet — scan & create one above"
                            }
                        }
                    }
                    if config.is_bearer() && !chosen().is_empty() {
                        button {
                            class: "bg-neutral-800 hover:bg-neutral-700 border border-neutral-600 rounded px-3 py-1 min-h-11 text-xs font-semibold text-[#66D8CE] shrink-0 disabled:opacity-50",
                            disabled: profile_busy() || saving_defaults() || busy(),
                            onclick: reonboard_selected,
                            title: "Refresh git base commit, available CLIs, live model catalog, and default timeouts for the selected profile",
                            if profile_busy() { "Re-Onboarding…" } else { "Re-Onboard" }
                        }
                    }
                }
                if !chosen().is_empty() {
                    {
                        let soft = soft_timeouts();
                        let exec_max = if soft { "86400" } else { "1800" };
                        let verif_max = if soft { "86400" } else { "600" };
                        rsx! {
                            div { class: "mb-2 rounded border border-neutral-800 bg-neutral-950/70 px-2.5 py-2",
                                div { class: "grid grid-cols-1 sm:grid-cols-3 lg:grid-cols-6 gap-2 items-end",
                                    div {
                                        label { class: "block text-[11px] text-neutral-400 mb-0.5",
                                            if soft { "Exec alert (s)" } else { "Exec limit (s)" }
                                        }
                                        input {
                                            r#type: "number",
                                            min: "1",
                                            max: "{exec_max}",
                                            class: "bg-neutral-900 border border-neutral-700 rounded px-2 py-1 w-full min-h-10 text-xs font-mono",
                                            value: "{exec_secs}",
                                            oninput: move |e| exec_secs.set(e.value()),
                                        }
                                    }
                                    div {
                                        label { class: "block text-[11px] text-neutral-400 mb-0.5", "Quiet / Idle stop (s)" }
                                        input {
                                            r#type: "number",
                                            min: "1",
                                            max: "{exec_max}",
                                            class: "bg-neutral-900 border border-neutral-700 rounded px-2 py-1 w-full min-h-10 text-xs font-mono",
                                            value: "{idle_secs}",
                                            oninput: move |e| idle_secs.set(e.value()),
                                        }
                                    }
                                    div {
                                        label { class: "block text-[11px] text-neutral-400 mb-0.5",
                                            if soft { "Verify alert (s)" } else { "Verify limit (s)" }
                                        }
                                        input {
                                            r#type: "number",
                                            min: "1",
                                            max: "{verif_max}",
                                            class: "bg-neutral-900 border border-neutral-700 rounded px-2 py-1 w-full min-h-10 text-xs font-mono",
                                            value: "{verify_secs}",
                                            oninput: move |e| verify_secs.set(e.value()),
                                        }
                                    }
                                    label { r#for: "dispatch_soft_timeouts_cb", class: "flex items-center gap-2 min-h-10 text-xs text-neutral-300 cursor-pointer px-1",
                                        input {
                                            r#type: "checkbox",
                                            id: "dispatch_soft_timeouts_cb",
                                            class: "w-4 h-4 shrink-0",
                                            checked: soft,
                                            onchange: move |e| soft_timeouts.set(e.checked()),
                                        }
                                        span { title: "Alert when exec/verify targets pass instead of killing active runs; quiet/idle stop still guards against hangs", "Alert only" }
                                    }
                                    label { r#for: "dispatch_clean_repo_cb", class: "flex items-center gap-2 min-h-10 text-xs text-neutral-300 cursor-pointer px-1",
                                        input {
                                            r#type: "checkbox",
                                            id: "dispatch_clean_repo_cb",
                                            class: "w-4 h-4 shrink-0",
                                            checked: require_clean_checkout(),
                                            onchange: move |e| require_clean_checkout.set(e.checked()),
                                        }
                                        span { title: "Require a clean Git checkout before creating, approving, or running tasks (uncheck to allow uncommitted work and integrate through a worktree)", "Require clean repo" }
                                    }
                                    if config.is_bearer() {
                                        label { r#for: "dispatch_low_spec_cb", class: "flex items-center gap-2 min-h-10 text-xs text-neutral-400 cursor-pointer px-1",
                                            input {
                                                r#type: "checkbox",
                                                id: "dispatch_low_spec_cb",
                                                class: "w-4 h-4 shrink-0",
                                                checked: profile_low_spec(),
                                                onchange: move |e| profile_low_spec.set(e.checked()),
                                            }
                                            span { "Low-spec" }
                                        }
                                        button {
                                            class: "bg-neutral-800 hover:bg-neutral-700 border border-neutral-600 rounded px-3 py-1 min-h-10 text-xs font-semibold text-neutral-200 disabled:opacity-50",
                                            disabled: saving_defaults() || profile_busy() || busy(),
                                            onclick: save_profile_timeouts,
                                            if saving_defaults() { "Saving…" } else { "Save Defaults" }
                                        }
                                    }
                                }
                                if let Some(st) = repo_status() {
                                    div { class: "mt-1.5 flex flex-wrap items-center gap-2 text-xs",
                                        if st.clean {
                                            span { class: "text-emerald-300 font-medium",
                                                "✓ Repo checkout clean"
                                            }
                                        } else {
                                            span { class: if require_clean_checkout() { "text-red-300 font-semibold" } else { "text-amber-300 font-medium" },
                                                {format!("⚠ {} dirty path(s) in checkout", st.dirty_paths.len())}
                                            }
                                        }
                                        span { class: "font-mono text-[11px] text-neutral-400",
                                            {
                                                let br = st.branch.as_deref().unwrap_or("detached");
                                                let short_sha = st.base.chars().take(8).collect::<String>();
                                                format!("{br} @ {short_sha}")
                                            }
                                        }
                                        button {
                                            class: "px-2 py-0.5 rounded border border-neutral-700 bg-neutral-900 hover:bg-neutral-800 text-[11px] text-neutral-300",
                                            onclick: {
                                                let config = config.clone();
                                                move |_| spawn_load_profile_repo_status(config.clone(), chosen(), chosen, repo_status)
                                            },
                                            "↻ Check"
                                        }
                                        if !st.dirty_paths.is_empty() {
                                            details { class: "w-full text-[11px] text-neutral-400",
                                                summary { class: "cursor-pointer hover:text-neutral-200", "Show dirty files" }
                                                p { class: "mt-1 font-mono text-[10px] text-neutral-400 break-all",
                                                    {
                                                        let mut list: Vec<String> = st.dirty_paths.iter().take(16).cloned().collect();
                                                        if st.dirty_paths.len() > 16 {
                                                            list.push(format!("… +{} more", st.dirty_paths.len() - 16));
                                                        }
                                                        list.join(", ")
                                                    }
                                                }
                                            }
                                        }
                                        if require_clean_checkout() && !st.clean {
                                            p { class: "w-full text-[11px] text-red-300",
                                                "Clean repo required: commit or stash these changes before creating/running work, or uncheck 'Require clean repo' to integrate through a worktree later."
                                            }
                                        }
                                    }
                                }
                                p { class: "text-[11px] text-neutral-500 mt-1",
                                    if soft {
                                        "Alert-only mode: passing Exec/Verify targets triggers a live warning instead of killing active runs; Quiet/Idle stop still halts stalled processes. Click Save Defaults to persist."
                                    } else {
                                        "Hard cap enforced at Exec/Verify limit. Check Alert only to let long active runs finish (click Save Defaults to persist)."
                                    }
                                }
                                if profile_busy() {
                                    if let Some(prog) = onboard_progress_state() {
                                        OnboardProgressBar { progress: prog }
                                    }
                                }
                                if let Some(err) = profile_error() {
                                    p { class: "text-red-400 text-xs mt-1 break-words", "{err}" }
                                }
                                if let Some(msg) = profile_notice() {
                                    p { class: "text-emerald-400 text-xs mt-1 break-words", "{msg}" }
                                }
                            }
                        }
                    }
                }
                textarea {
                    class: "bg-neutral-900 border border-neutral-700 rounded px-2 py-2 w-full mb-2 text-base sm:text-sm",
                    rows: 3,
                    value: "{objective}",
                    oninput: {
                        let config = config.clone();
                        move |e| {
                            let val = e.value();
                            objective.set(val.clone());
                            if !scope_manually_edited() {
                                let prof_scope = profiles()
                                    .into_iter()
                                    .find(|p| p.name == chosen())
                                    .map(|p| p.scope)
                                    .unwrap_or_default();
                                if let Some(matched) = extract_objective_repo_files(&val, &repo_files(), &prof_scope) {
                                    let mut merged = matched.clone();
                                    for pf in &prof_scope {
                                        if merged.len() >= 64 {
                                            break;
                                        }
                                        if !merged.iter().any(|existing| existing == pf) {
                                            merged.push(pf.clone());
                                        }
                                    }
                                    dispatch_allowed_files.set(merged.join(", "));
                                    let match_noun = if matched.len() == 1 { "file" } else { "files" };
                                    auto_scope_note.set(Some(format!(
                                        "Auto-selected {} matched {match_noun} merged with top profile files ({} total).",
                                        matched.len(),
                                        merged.len()
                                    )));
                                }
                            }
                            spawn_suggest_files(
                                config.clone(),
                                chosen(),
                                val,
                                acceptance(),
                                SuggestFilesTarget {
                                    chosen,
                                    scope_manually_edited,
                                    suggest_seq,
                                    dispatch_allowed_files,
                                    suggested_matches,
                                    indexed_file_count,
                                    auto_scope_note,
                                },
                            );
                        }
                    },
                    placeholder: "What should Clatterforge execute?"
                }
                textarea {
                    class: "bg-neutral-900 border border-neutral-700 rounded px-2 py-2 w-full mb-2 text-base sm:text-sm",
                    rows: 2,
                    value: "{acceptance}",
                    oninput: {
                        let config = config.clone();
                        move |e| {
                            let val = e.value();
                            acceptance.set(val.clone());
                            if !objective().trim().is_empty() {
                                spawn_suggest_files(
                                    config.clone(),
                                    chosen(),
                                    objective(),
                                    val,
                                    SuggestFilesTarget {
                                        chosen,
                                        scope_manually_edited,
                                        suggest_seq,
                                        dispatch_allowed_files,
                                        suggested_matches,
                                        indexed_file_count,
                                        auto_scope_note,
                                    },
                                );
                            }
                        }
                    },
                    placeholder: "What must pass independent verification when done?"
                }
                div { class: "mb-2",
                    div { class: "flex flex-col sm:flex-row gap-2 mb-1",
                        input {
                            class: "bg-neutral-900 border border-neutral-700 rounded px-2 py-1.5 flex-1 font-mono text-xs text-neutral-200 placeholder:text-neutral-500",
                            value: "{dispatch_allowed_files}",
                            oninput: move |e| {
                                let v = e.value();
                                scope_manually_edited.set(!v.trim().is_empty());
                                auto_scope_note.set(None);
                                dispatch_allowed_files.set(v);
                            },
                            placeholder: "Target editable files (optional, comma-separated — auto-filled when objective matches)"
                        }
                        if !suggested_matches().is_empty() {
                            button {
                                class: "bg-neutral-900 hover:bg-neutral-800 border border-[#66D8CE]/50 rounded px-2.5 py-1 text-xs text-[#66D8CE] shrink-0",
                                onclick: move |_| {
                                    let only: Vec<String> = suggested_matches().into_iter().map(|m| m.path).collect();
                                    dispatch_allowed_files.set(only.join(", "));
                                    scope_manually_edited.set(true);
                                    auto_scope_note.set(None);
                                },
                                {format!("Matched Only ({})", suggested_matches().len())}
                            }
                        }
                        if !dispatch_allowed_files().trim().is_empty() {
                            button {
                                class: "bg-neutral-900 hover:bg-neutral-800 border border-neutral-700 rounded px-2.5 py-1 text-xs text-neutral-300 shrink-0",
                                onclick: move |_| {
                                    dispatch_allowed_files.set(String::new());
                                    scope_manually_edited.set(true);
                                    auto_scope_note.set(None);
                                },
                                "Use Profile Default"
                            }
                        }
                    }
                    if let Some(note) = auto_scope_note() {
                        p { class: "text-[11px] text-[#66D8CE] mb-1", "{note}" }
                    }
                    if !suggested_matches().is_empty() {
                        div { class: "flex flex-wrap gap-1 mb-1.5",
                            for m in suggested_matches().into_iter().take(6) {
                                {
                                    let detail_hint = if m.reasons.is_empty() {
                                        format!("score {:.1}", m.score)
                                    } else {
                                        m.reasons.join(" · ")
                                    };
                                    rsx! {
                                        span {
                                            class: "px-2 py-0.5 rounded border border-neutral-800 bg-neutral-950 font-mono text-[10px] text-neutral-300",
                                            title: "{detail_hint}",
                                            "{m.path}"
                                            span { class: "text-neutral-500 ml-1", "({detail_hint})" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    {
                        let prof_scope = profiles()
                            .into_iter()
                            .find(|p| p.name == chosen())
                            .map(|p| p.scope)
                            .unwrap_or_default();
                        let query = file_search().trim().to_ascii_lowercase();
                        let search_matches: Vec<String> = if query.is_empty() {
                            Vec::new()
                        } else {
                            repo_files()
                                .into_iter()
                                .filter(|f| f.to_ascii_lowercase().contains(&query))
                                .take(8)
                                .collect()
                        };
                        rsx! {
                            div { class: "flex flex-wrap items-center gap-2 text-[11px] text-neutral-400",
                                if !prof_scope.is_empty() {
                                    details { class: "text-[11px] text-neutral-400",
                                        summary { class: "cursor-pointer hover:text-neutral-200",
                                            {
                                                let def_noun = if prof_scope.len() == 1 { "file" } else { "files" };
                                                if let Some(n) = indexed_file_count() {
                                                    let idx_noun = if n == 1 { "file" } else { "files" };
                                                    format!("Profile default scope ({} default · {n} repo {idx_noun} indexed)", prof_scope.len())
                                                } else {
                                                    format!("Profile default scope ({} {def_noun})", prof_scope.len())
                                                }
                                            }
                                        }
                                        p { class: "mt-1 font-mono text-[10px] text-neutral-400 break-all",
                                            {prof_scope.join(", ")}
                                        }
                                    }
                                }
                                if !repo_files().is_empty() {
                                    input {
                                        class: "bg-neutral-950 border border-neutral-800 rounded px-2 py-0.5 font-mono text-[11px] text-neutral-200 placeholder:text-neutral-500 min-w-[220px]",
                                        value: "{file_search}",
                                        oninput: move |e| file_search.set(e.value()),
                                        placeholder: "+ Search repo files to target…"
                                    }
                                }
                            }
                            if !search_matches.is_empty() {
                                div { class: "mt-1 flex flex-wrap gap-1",
                                    for path_item in search_matches {
                                        {
                                            let path_clone = path_item.clone();
                                            rsx! {
                                                button {
                                                    class: "bg-neutral-900 hover:bg-neutral-800 border border-neutral-700 rounded px-2 py-0.5 font-mono text-[10px] text-[#66D8CE]",
                                                    onclick: move |_| {
                                                        let mut list = parse_file_list(&dispatch_allowed_files());
                                                        if !list.iter().any(|existing| existing == &path_clone) {
                                                            list.push(path_clone.clone());
                                                        }
                                                        dispatch_allowed_files.set(list.join(", "));
                                                        scope_manually_edited.set(true);
                                                        auto_scope_note.set(None);
                                                        file_search.set(String::new());
                                                    },
                                                    "+ {path_item}"
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                {
                    let clean_blocked = require_clean_checkout()
                        && repo_status().as_ref().is_some_and(|st| !st.clean);
                    rsx! {
                        div { class: "flex flex-wrap items-center gap-2",
                            button {
                                class: "cf-action-btn rounded px-4 py-1 min-h-11 w-full sm:w-auto",
                                disabled: busy() || clean_blocked || chosen().is_empty() || objective().trim().is_empty() || acceptance().trim().is_empty(),
                                onclick: submit_dispatch_and_run,
                                if busy() && busy_launch() { "Launching Workflow…" } else { "Dispatch & Run" }
                            }
                            button {
                                class: "rounded px-4 py-1 min-h-11 w-full sm:w-auto text-xs font-semibold border border-[#202D3A] bg-[#0C1118] hover:bg-[#202D3A]/60 text-[#F2F5F7] disabled:opacity-50",
                                disabled: busy() || clean_blocked || chosen().is_empty() || objective().trim().is_empty() || acceptance().trim().is_empty(),
                                onclick: submit_prepare,
                                if busy() && !busy_launch() { "Preparing…" } else { "Prepare Only" }
                            }
                        }
                    }
                }
                if let Some(error) = error() {
                    p { class: "text-red-400 mt-2 break-words", "{error}" }
                }
            }
        }
    }
}

#[component]
fn ProviderMonitor(config: ApiConnection) -> Element {
    let mut report = use_signal(|| None::<ProviderReport>);
    let mut analytics = use_signal(|| None::<AnalyticsReport>);
    let mut loading = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);

    let fetch_config = config.clone();
    use_resource(move || {
        let config = fetch_config.clone();
        async move {
            loading.set(true);
            match api_get::<ProviderReport>(&config, "/api/providers").await {
                Ok(data) => report.set(Some(data)),
                Err(e) => error.set(Some(e)),
            }
            if let Ok(a) = api_get::<AnalyticsReport>(&config, "/api/analytics").await {
                analytics.set(Some(a));
            }
            loading.set(false);
        }
    });

    let mut refresh = {
        let config = config.clone();
        move |_| {
            let config = config.clone();
            loading.set(true);
            spawn(async move {
                match api_get::<ProviderReport>(&config, "/api/providers?refresh=true").await {
                    Ok(data) => report.set(Some(data)),
                    Err(e) => error.set(Some(e)),
                }
                if let Ok(a) = api_get::<AnalyticsReport>(&config, "/api/analytics").await {
                    analytics.set(Some(a));
                }
                loading.set(false);
            });
        }
    };

    let mut panel_open = use_signal(|| false);

    rsx! {
        div { class: "mb-3 rounded border border-neutral-700 bg-neutral-900/60 p-3",
            div { class: "text-emerald-400 font-semibold min-h-11 flex flex-wrap items-center justify-between gap-2",
                button {
                    r#type: "button",
                    class: "flex items-center gap-2 text-left text-xs sm:text-sm text-emerald-400 hover:text-emerald-300 cursor-pointer py-2",
                    aria_expanded: "{panel_open()}",
                    onclick: move |_| panel_open.toggle(),
                    span { class: "text-[11px] text-neutral-400", if panel_open() { "▾" } else { "▸" } }
                    span { "⚡ Integration Points & Provider Quotas" }
                }
                div { class: "flex flex-wrap items-center gap-2",
                    if let Some(data) = report() {
                        if data.probed_at_ms > 0 {
                            span { class: "text-[11px] font-normal text-neutral-400",
                                {
                                    let secs = (data.probed_at_ms / 1000) % 86_400;
                                    let hh = secs / 3600;
                                    let mm = (secs % 3600) / 60;
                                    let ss = secs % 60;
                                    format!("Updated {hh:02}:{mm:02}:{ss:02} UTC")
                                }
                            }
                        }
                    }
                    button {
                        r#type: "button",
                        class: "text-xs bg-neutral-800 hover:bg-neutral-700 border border-neutral-600 rounded px-3 py-1 min-h-11 text-neutral-300 disabled:opacity-50",
                        disabled: loading(),
                        onclick: move |_| refresh(()),
                        if loading() { "Refreshing…" } else { "Refresh Catalog" }
                    }
                }
            }

            if panel_open() {
                if let Some(err) = error() {
                    p { class: "text-red-400 text-xs mb-2 mt-2", "Failed to load provider status: {err}" }
                }

                if let Some(data) = report() {
                div { class: "mt-2",
                    div { class: "grid grid-cols-2 sm:grid-cols-4 gap-2 mb-3 p-2 bg-neutral-950/80 rounded border border-neutral-800 text-xs",
                        div {
                            span { class: "text-neutral-500 block", "Total Prompt" }
                            span { class: "font-mono text-neutral-200 text-sm font-semibold", "{format_tokens(data.token_summary.total_prompt_tokens)}" }
                        }
                        div {
                            span { class: "text-neutral-500 block", "Total Output" }
                            span { class: "font-mono text-neutral-200 text-sm font-semibold", "{format_tokens(data.token_summary.total_output_tokens)}" }
                        }
                        div {
                            span { class: "text-neutral-500 block", "Total Cached" }
                            span { class: "font-mono text-emerald-300 text-sm font-semibold", "{format_tokens(data.token_summary.total_cached_tokens)}" }
                        }
                        div {
                            span { class: "text-neutral-500 block", "Estimated API Cost" }
                            span { class: "font-mono text-amber-300 text-sm font-semibold",
                                {data.token_summary.estimated_cost_usd.unwrap_or_else(|| "n/a".into())}
                            }
                        }
                    }
                    if data.token_summary.total_tasks_unreported > 0 {
                        p { class: "text-xs text-amber-400/90 -mt-2 mb-3",
                            "{data.token_summary.total_tasks_unreported} CLI run(s) printed no usage receipt; totals exclude them as unknown, not free."
                        }
                    }

                    div { class: "grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-5 gap-3",
                        for prov in data.providers {
                            {
                                let is_active = prov.available && prov.authenticated;
                                let badge_class = if !prov.dispatchable {
                                    "bg-sky-950/80 text-sky-300 border-sky-700"
                                } else if is_active {
                                    "bg-emerald-950/80 text-emerald-400 border-emerald-700"
                                } else if prov.available {
                                    "bg-amber-950/80 text-amber-400 border-amber-700"
                                } else {
                                    "bg-neutral-900 text-neutral-500 border-neutral-800"
                                };
                                let status_label = if !prov.dispatchable {
                                    "Informational Only"
                                } else if is_active {
                                    "Ready"
                                } else if prov.available {
                                    "Unauthenticated"
                                } else {
                                    "Not Installed"
                                };

                                rsx! {
                                    div { class: "rounded border border-neutral-800 bg-neutral-950 p-3 flex flex-col justify-between",
                                        div {
                                            div { class: "flex items-center justify-between mb-2",
                                                span { class: "font-bold text-neutral-200", "{prov.name}" }
                                                span { class: "text-[10px] px-1.5 py-0.5 rounded border {badge_class}",
                                                    "{status_label}"
                                                }
                                            }

                                            if let Some(tier) = prov.tier.as_ref().filter(|tier| !prov.details.to_ascii_lowercase().contains(&tier.to_ascii_lowercase())) {
                                                p { class: "text-xs text-sky-400 font-medium mb-1", "{tier}" }
                                            }
                                            if let Some(email) = &prov.user_email {
                                                p { class: "text-[11px] text-neutral-400 truncate mb-1", "{email}" }
                                            }
                                            p { class: "text-xs text-neutral-400 mb-2 leading-relaxed", "{prov.details}" }
                                        }

                                        div { class: "mt-2 pt-2 border-t border-neutral-900 text-[11px]",
                                            div { class: "flex items-center justify-between text-neutral-500 mb-1",
                                                span { "Quota Policy" }
                                                span { class: "text-neutral-300 font-mono text-[10px]",
                                                    {
                                                        match prov.quota_type.as_str() {
                                                            "local_hardware" => "Local / Zero-Cost",
                                                            "provider_managed" => "Provider Managed",
                                                            "informational_only" => "Informational",
                                                            other => other,
                                                        }
                                                    }
                                                }
                                            }
                                            if let Some(reset) = &prov.reset_description {
                                                p { class: "text-neutral-400 text-[10px] leading-tight",
                                                    span { class: "text-neutral-500 font-medium", "Reset: " }
                                                    "{reset}"
                                                }
                                            }
                                            if !prov.models.is_empty() {
                                                div { class: "flex flex-wrap gap-1 mt-2",
                                                    for m in prov.models.iter().take(3) {
                                                        span { class: "text-[9px] px-1 py-0.5 rounded bg-neutral-900 border border-neutral-800 text-neutral-400",
                                                            "{m}"
                                                        }
                                                    }
                                                    if prov.models.len() > 3 {
                                                        span { class: "text-[9px] text-neutral-500 px-1 py-0.5",
                                                            "+{prov.models.len() - 3}"
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    if let Some(stats) = analytics() {
                        div { class: "mt-3 rounded border border-neutral-800 bg-neutral-950/80 p-3",
                            div { class: "flex flex-wrap items-center justify-between gap-2 mb-2",
                                span { class: "text-xs font-semibold text-sky-300",
                                    "📊 CLI & Model Performance Weights ({stats.total_attempts} tracked attempts · {stats.total_usage_unreported} without usage data · thinking {format_tokens(stats.total_thinking_tokens)})"
                                }
                            }
                            if stats.by_pair_complexity.is_empty() {
                                p { class: "text-xs text-neutral-500", "No delegated task attempts recorded yet." }
                            } else {
                                div { class: "overflow-x-auto",
                                    table { class: "w-full text-left border-collapse text-[11px]",
                                        thead {
                                            tr { class: "border-b border-neutral-800 text-neutral-400 uppercase text-[10px]",
                                                th { class: "py-1 pr-2", "CLI" }
                                                th { class: "py-1 pr-2", "Model" }
                                                th { class: "py-1 pr-2", "Complexity" }
                                                th { class: "py-1 pr-2 text-right", "Runs" }
                                                th { class: "py-1 pr-2 text-right", "Pass%" }
                                                th { class: "py-1 pr-2 text-right", "Acc/Exc" }
                                                th { class: "py-1 pr-2 text-right", "Fail/TO/Redo" }
                                                th { class: "py-1 pr-2 text-right", "Avg ms" }
                                                th { class: "py-1 text-right", "Weight" }
                                            }
                                        }
                                        tbody { class: "divide-y divide-neutral-900",
                                            for row in stats.by_pair_complexity.iter().take(20) {
                                                {
                                                    let weight_cls = if row.learned_weight > 0.05 {
                                                        "text-emerald-400 font-semibold"
                                                    } else if row.learned_weight < -0.05 {
                                                        "text-red-400 font-semibold"
                                                    } else {
                                                        "text-neutral-300"
                                                    };
                                                    let pass_pct = (row.pass_rate * 100.0).round() as u64;
                                                    let fails = row.exec_failed + row.verify_failed;
                                                    rsx! {
                                                        tr {
                                                            td { class: "py-1 pr-2 font-mono text-neutral-200", "{row.backend}" }
                                                            td { class: "py-1 pr-2 font-mono text-neutral-300", "{row.model}" }
                                                            td { class: "py-1 pr-2 text-neutral-400", {row.complexity.as_deref().unwrap_or("-")} }
                                                            td { class: "py-1 pr-2 text-right font-mono", "{row.attempts}" }
                                                            td { class: "py-1 pr-2 text-right font-mono", "{pass_pct}%" }
                                                            td { class: "py-1 pr-2 text-right font-mono text-emerald-300", "{row.accepted}/{row.exceptional}" }
                                                            td { class: "py-1 pr-2 text-right font-mono text-amber-300", "{fails}/{row.timed_out}/{row.redone}" }
                                                            td { class: "py-1 pr-2 text-right font-mono text-neutral-400", "{row.avg_duration_ms}" }
                                                            td { class: "py-1 text-right font-mono {weight_cls}", "{row.learned_weight:+.2}" }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                } else if loading() {
                    p { class: "text-xs text-neutral-500 my-2", "Scanning provider endpoints and token logs…" }
                }
            }
        }
    }
}

fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.2}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// Forge settings, starting with each preset's verifier sandbox (#35). The server decides
/// `editable`: only the main forge's desktop connection may change anything; paired
/// devices get this same view read-only, and Sparks never reach it.
#[component]
fn ForgeSettings(config: ApiConnection) -> Element {
    let mut open = use_signal(|| false);
    let mut view = use_signal(|| None::<serde_json::Value>);
    let mut error = use_signal(|| None::<String>);
    let mut notice = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    let mut preset = use_signal(String::new);
    let mut image = use_signal(String::new);
    let mut command = use_signal(String::new);
    let mut default_level = use_signal(|| "isolated".to_string());
    let mut ceiling = use_signal(|| "network".to_string());
    let mut memory_mb = use_signal(|| "2048".to_string());
    let mut cpus = use_signal(|| "2".to_string());
    let mut pids = use_signal(|| "512".to_string());
    let mut env_names = use_signal(String::new);
    let mut read_only = use_signal(String::new);
    let mut image_filter = use_signal(String::new);
    let mut backend = use_signal(|| "docker".to_string());
    let mut worker_env = use_signal(String::new);

    let load = {
        let config = config.clone();
        move || {
            let config = config.clone();
            spawn(async move {
                match api_get::<serde_json::Value>(&config, "/api/settings/sandbox").await {
                    Ok(v) => {
                        error.set(None);
                        view.set(Some(v));
                    }
                    Err(e) => error.set(Some(e)),
                }
            });
        }
    };

    // Fill the form from a preset's current sandbox, or reset it when there is none.
    let mut select_preset = move |name: String| {
        preset.set(name.clone());
        notice.set(None);
        let record = view().and_then(|v| {
            v["presets"]
                .as_array()
                .and_then(|p| p.iter().find(|p| p["name"] == name.as_str()).cloned())
        });
        worker_env.set(
            record
                .as_ref()
                .and_then(|p| p["env_passthrough"].as_array().cloned())
                .unwrap_or_default()
                .iter()
                .filter_map(|n| n.as_str().map(str::to_string))
                .collect::<Vec<_>>()
                .join(", "),
        );
        let current = record
            .map(|p| p["sandbox"].clone())
            .unwrap_or(serde_json::Value::Null);
        let text = |v: &serde_json::Value| v.as_str().unwrap_or("").to_string();
        if current.is_null() {
            image.set(String::new());
            command.set(String::new());
        } else {
            backend.set(match current["backend"]["kind"].as_str() {
                Some("app_container") => "app_container".to_string(),
                _ => "docker".to_string(),
            });
            image.set(text(&current["backend"]["image"]));
            command.set(
                current["backend"]["command"]
                    .as_array()
                    .map(|a| a.iter().map(text).collect::<Vec<_>>().join("\n"))
                    .unwrap_or_default(),
            );
            default_level.set(text(&current["default_level"]));
            ceiling.set(text(&current["ceiling"]));
            memory_mb.set(current["memory_mb"].to_string());
            cpus.set(current["cpus"].to_string());
            pids.set(current["pids"].to_string());
            env_names.set(
                current["env_passthrough"]
                    .as_array()
                    .map(|a| a.iter().map(text).collect::<Vec<_>>().join(", "))
                    .unwrap_or_default(),
            );
            read_only.set(
                current["read_only_paths"]
                    .as_array()
                    .map(|a| a.iter().map(text).collect::<Vec<_>>().join("\n"))
                    .unwrap_or_default(),
            );
        }
    };

    let mut use_catalog = move |entry: serde_json::Value| {
        let text = |v: &serde_json::Value| v.as_str().unwrap_or("").to_string();
        image.set(text(&entry["image"]));
        command.set(
            entry["command"]
                .as_array()
                .map(|a| a.iter().map(text).collect::<Vec<_>>().join("\n"))
                .unwrap_or_default(),
        );
        default_level.set(text(&entry["default_level"]));
        ceiling.set(text(&entry["ceiling"]));
        memory_mb.set(entry["memory_mb"].to_string());
    };

    let post = {
        let config = config.clone();
        let load = load.clone();
        move |path: String, body: serde_json::Value, done: String| {
            let config = config.clone();
            let load = load.clone();
            busy.set(true);
            notice.set(None);
            spawn(async move {
                match api_post_json::<serde_json::Value>(&config, &path, body).await {
                    Ok(_) => {
                        error.set(None);
                        notice.set(Some(done));
                        load();
                    }
                    Err(e) => error.set(Some(e)),
                }
                busy.set(false);
            });
        }
    };

    let toggle = {
        let load = load.clone();
        move |_| {
            let next = !open();
            open.set(next);
            if next && view().is_none() {
                load();
            }
        }
    };

    let data = view();
    let editable = data
        .as_ref()
        .and_then(|v| v["editable"].as_bool())
        .unwrap_or(false);
    let presets: Vec<serde_json::Value> = data
        .as_ref()
        .and_then(|v| v["presets"].as_array().cloned())
        .unwrap_or_default();
    let catalog: Vec<serde_json::Value> = data
        .as_ref()
        .and_then(|v| v["catalog"].as_array().cloned())
        .unwrap_or_default();
    let filter = image_filter().to_lowercase();
    let local_images: Vec<String> = data
        .as_ref()
        .and_then(|v| v["local_images"].as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|i| i["reference"].as_str().map(str::to_string))
        .filter(|r| filter.is_empty() || r.to_lowercase().contains(&filter))
        .take(40)
        .collect();
    let docker_status = data
        .as_ref()
        .map(|v| {
            match (
                v["docker"]["server_version"].as_str(),
                v["docker"]["error"].as_str(),
            ) {
                (Some(version), _) => format!("Docker {version}"),
                (None, Some(err)) => format!("Docker unavailable: {err}"),
                _ => "Docker unavailable".to_string(),
            }
        })
        .unwrap_or_default();
    let selected = preset();
    let app_container_available = data
        .as_ref()
        .and_then(|v| v["backends"].as_array().cloned())
        .unwrap_or_default()
        .iter()
        .any(|b| b == "app_container");
    let input_class = "w-full rounded border border-neutral-700 bg-neutral-950 px-2 py-1 text-xs text-neutral-100 disabled:opacity-60";

    rsx! {
        div { class: "mb-3 rounded border border-neutral-700 bg-neutral-900/60 p-3",
            button {
                r#type: "button",
                class: "flex items-center gap-2 text-left text-xs sm:text-sm text-emerald-400 hover:text-emerald-300 cursor-pointer py-2 font-semibold",
                aria_expanded: "{open()}",
                onclick: toggle,
                span { class: "text-[11px] text-neutral-400", if open() { "▾" } else { "▸" } }
                span { "⚙ Forge Settings: Verifier Sandbox" }
            }
            if open() {
                div { class: "mt-2 space-y-3 text-xs text-neutral-300",
                    if let Some(err) = error() {
                        p { class: "text-red-400 break-words", "{err}" }
                    }
                    if let Some(msg) = notice() {
                        p { class: "text-emerald-400", "{msg}" }
                    }
                    if data.is_some() {
                        p { class: "text-neutral-400", "{docker_status}" }
                        if !editable {
                            p { class: "text-amber-400/90",
                                "Read-only here. Change sandbox settings from the main forge's desktop app."
                            }
                        }
                        div {
                            label { class: "block text-neutral-400 mb-1", "Preset" }
                            select {
                                class: input_class,
                                value: "{selected}",
                                onchange: move |e| select_preset(e.value()),
                                option { value: "", "Choose a preset" }
                                for p in presets.iter() {
                                    {
                                        let name = p["name"].as_str().unwrap_or("").to_string();
                                        let tag = if p["sandbox"].is_null() { "host verifier" } else { "sandboxed" };
                                        rsx! { option { value: "{name}", "{name} ({tag})" } }
                                    }
                                }
                            }
                        }
                        if !selected.is_empty() {
                            div { class: "space-y-2",
                                div {
                                    label { class: "block text-neutral-400 mb-1", "Backend" }
                                    select {
                                        class: input_class,
                                        disabled: !editable,
                                        value: "{backend}",
                                        onchange: move |e| backend.set(e.value()),
                                        option { value: "docker", "Docker container (any host)" }
                                        if app_container_available {
                                            option { value: "app_container", "Windows AppContainer (this preset's own verifier)" }
                                        }
                                    }
                                }
                                if backend() == "docker" {
                                p { class: "text-neutral-400", "Curated images (pinned by digest)" }
                                div { class: "flex flex-wrap gap-2",
                                    for entry in catalog.iter() {
                                        {
                                            let entry = entry.clone();
                                            let label = entry["label"].as_str().unwrap_or("").to_string();
                                            let note = entry["note"].as_str().unwrap_or("").to_string();
                                            let present = entry["present"].as_bool().unwrap_or(false);
                                            let pinned = entry["image"].as_str().unwrap_or("").to_string();
                                            let mut post = post.clone();
                                            let pick = entry.clone();
                                            rsx! {
                                                div { class: "rounded border border-neutral-700 p-2 max-w-xs", title: "{note}",
                                                    p { class: "font-semibold text-neutral-200", "{label}" }
                                                    p { class: "text-[11px] text-neutral-400", if present { "Present locally" } else { "Not pulled yet" } }
                                                    div { class: "mt-1 flex gap-2",
                                                        button {
                                                            r#type: "button",
                                                            class: "rounded bg-neutral-800 px-2 py-1 hover:bg-neutral-700 disabled:opacity-50",
                                                            disabled: !editable,
                                                            onclick: move |_| use_catalog(pick.clone()),
                                                            "Use"
                                                        }
                                                        if !present {
                                                            button {
                                                                r#type: "button",
                                                                class: "rounded bg-neutral-800 px-2 py-1 hover:bg-neutral-700 disabled:opacity-50",
                                                                disabled: !editable || busy(),
                                                                onclick: move |_| post(
                                                                    "/api/settings/sandbox/pull".to_string(),
                                                                    serde_json::json!({"image": pinned.clone()}),
                                                                    format!("Pulled {}", pinned),
                                                                ),
                                                                "Pull"
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                div {
                                    label { class: "block text-neutral-400 mb-1", "Or pin a local image" }
                                    input {
                                        class: input_class,
                                        placeholder: "Filter local images",
                                        value: "{image_filter}",
                                        oninput: move |e| image_filter.set(e.value()),
                                    }
                                    select {
                                        class: "{input_class} mt-1",
                                        disabled: !editable,
                                        onchange: move |e| if !e.value().is_empty() { image.set(e.value()) },
                                        option { value: "", "Choose a local image" }
                                        for reference in local_images.iter() {
                                            option { value: "{reference}", "{reference}" }
                                        }
                                    }
                                }
                                div {
                                    label { class: "block text-neutral-400 mb-1", "Pinned image" }
                                    input {
                                        class: input_class,
                                        disabled: !editable,
                                        value: "{image}",
                                        oninput: move |e| image.set(e.value()),
                                    }
                                }
                                div {
                                    label { class: "block text-neutral-400 mb-1", "Verifier command (one argument per line)" }
                                    textarea {
                                        class: "{input_class} font-mono",
                                        rows: "3",
                                        disabled: !editable,
                                        value: "{command}",
                                        oninput: move |e| command.set(e.value()),
                                    }
                                }
                                } else {
                                    p { class: "text-neutral-400",
                                        "Runs this preset's own pinned verifier in a fresh Windows AppContainer per run: the worktree is writable, your profile is not readable, and network is on only at the network level."
                                    }
                                }
                                div { class: "grid grid-cols-2 gap-2 sm:grid-cols-5",
                                    div {
                                        label { class: "block text-neutral-400 mb-1", "Default level" }
                                        select {
                                            class: input_class,
                                            disabled: !editable,
                                            value: "{default_level}",
                                            onchange: move |e| default_level.set(e.value()),
                                            option { value: "isolated", "isolated" }
                                            option { value: "network", "network" }
                                            option { value: "trusted_paths", "trusted paths" }
                                        }
                                    }
                                    div {
                                        label { class: "block text-neutral-400 mb-1", "Ceiling" }
                                        select {
                                            class: input_class,
                                            disabled: !editable,
                                            value: "{ceiling}",
                                            onchange: move |e| ceiling.set(e.value()),
                                            option { value: "isolated", "isolated" }
                                            option { value: "network", "network" }
                                            option { value: "trusted_paths", "trusted paths" }
                                        }
                                    }
                                    div {
                                        label { class: "block text-neutral-400 mb-1", "Memory (MB)" }
                                        input { class: input_class, disabled: !editable, value: "{memory_mb}", oninput: move |e| memory_mb.set(e.value()) }
                                    }
                                    div {
                                        label { class: "block text-neutral-400 mb-1", "CPUs" }
                                        input { class: input_class, disabled: !editable, value: "{cpus}", oninput: move |e| cpus.set(e.value()) }
                                    }
                                    div {
                                        label { class: "block text-neutral-400 mb-1", "Processes" }
                                        input { class: input_class, disabled: !editable, value: "{pids}", oninput: move |e| pids.set(e.value()) }
                                    }
                                }
                                div { class: "grid gap-2 sm:grid-cols-2",
                                    div {
                                        label { class: "block text-neutral-400 mb-1", "Environment names at trusted paths (comma separated)" }
                                        input { class: input_class, disabled: !editable, value: "{env_names}", oninput: move |e| env_names.set(e.value()) }
                                    }
                                    div {
                                        label { class: "block text-neutral-400 mb-1", "Read-only host paths at trusted paths (one per line)" }
                                        textarea { class: input_class, rows: "2", disabled: !editable, value: "{read_only}", oninput: move |e| read_only.set(e.value()) }
                                    }
                                }
                                if editable {
                                    div { class: "flex flex-wrap gap-2",
                                        {
                                            let mut post = post.clone();
                                            let name = selected.clone();
                                            rsx! {
                                                button {
                                                    r#type: "button",
                                                    class: "rounded bg-emerald-700 px-3 py-1 text-white hover:bg-emerald-600 disabled:opacity-50",
                                                    disabled: busy(),
                                                    onclick: move |_| {
                                                        let number = |s: String, fallback: u32| s.trim().parse::<u32>().unwrap_or(fallback);
                                                        let body = serde_json::json!({
                                                            "backend": backend(),
                                                            "image": image(),
                                                            "command": command().lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>(),
                                                            "default_level": default_level(),
                                                            "ceiling": ceiling(),
                                                            "memory_mb": number(memory_mb(), 2048),
                                                            "cpus": number(cpus(), 2),
                                                            "pids": number(pids(), 512),
                                                            "env_passthrough": env_names().split(',').map(str::trim).filter(|n| !n.is_empty()).collect::<Vec<_>>(),
                                                            "read_only_paths": read_only().lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>(),
                                                        });
                                                        post(
                                                            format!("/api/settings/sandbox/presets/{}", name),
                                                            body,
                                                            format!("Saved the verifier sandbox for {}", name),
                                                        );
                                                    },
                                                    "Save sandbox"
                                                }
                                            }
                                        }
                                        {
                                            let mut post = post.clone();
                                            let name = selected.clone();
                                            rsx! {
                                                button {
                                                    r#type: "button",
                                                    class: "rounded bg-neutral-800 px-3 py-1 hover:bg-neutral-700 disabled:opacity-50",
                                                    disabled: busy(),
                                                    onclick: move |_| post(
                                                        format!("/api/settings/sandbox/presets/{}", name),
                                                        serde_json::json!({"clear": true}),
                                                        format!("{} now verifies on the host", name),
                                                    ),
                                                    "Remove sandbox"
                                                }
                                            }
                                        }
                                    }
                                    p { class: "text-[11px] text-neutral-500",
                                        "Changes apply to newly assigned tasks; existing tasks keep the sandbox their approval recorded."
                                    }
                                }
                                div { class: "mt-3 border-t border-neutral-800 pt-3",
                                    label { class: "block text-neutral-400 mb-1",
                                        "Worker environment: extra names passed to this preset's CLI and host verifier (comma separated)"
                                    }
                                    p { class: "text-[11px] text-neutral-500 mb-1",
                                        "Everything else in your environment stays out; system paths and profile folders always pass so CLIs find their sign-in."
                                    }
                                    input {
                                        class: input_class,
                                        disabled: !editable,
                                        placeholder: "e.g. OPENAI_API_KEY, HTTPS_PROXY",
                                        value: "{worker_env}",
                                        oninput: move |e| worker_env.set(e.value()),
                                    }
                                    if editable {
                                        {
                                            let mut post = post.clone();
                                            let name = selected.clone();
                                            rsx! {
                                                button {
                                                    r#type: "button",
                                                    class: "mt-2 rounded bg-neutral-800 px-3 py-1 hover:bg-neutral-700 disabled:opacity-50",
                                                    disabled: busy(),
                                                    onclick: move |_| post(
                                                        format!("/api/settings/presets/{}/env", name),
                                                        serde_json::json!({
                                                            "passthrough": worker_env().split(',').map(str::trim).filter(|n| !n.is_empty()).collect::<Vec<_>>(),
                                                        }),
                                                        format!("Saved the worker environment for {}", name),
                                                    ),
                                                    "Save worker environment"
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    } else if error().is_none() {
                        p { class: "text-neutral-400", "Loading..." }
                    }
                }
            }
        }
    }
}
