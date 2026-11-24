//! Local HTTP service for the GUI: a thin `axum` wrapper over `service::TaskService`.
//! Loopback-bound. Every `/api/*` request needs a matching bearer token (written to a
//! file under the state root on startup, for the Tauri sidecar to read) and a same-origin
//! Host/Origin, because this is a real, bindable network port from the moment it exists,
//! not just an in-process IPC bridge - see docs/REMOTE_ACCESS.md for why that distinction
//! matters even before anything here is exposed beyond loopback.

use crate::{
    enrollment::{self, Enrollment, EnrollmentOperation, Enrollments, VerifyOutcome},
    http_access::AccessPolicy,
    logging::{JsonLogger, sanitize_query, truncate_preview},
    mcp::LocalBackend,
    service::TaskService,
    sessions::{SESSION_TTL_MS, Sessions},
};
use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{Path, Query, Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::{AppendHeaders, IntoResponse, Response},
    routing::{delete, get, post, put},
};
use conductor_protocol::{
    BackendError, CancelTaskRequest, ConductorBackend, DecisionRequest, ErrorKind,
    EvidenceSummaryRequest, GetArtifactRequest, GetTaskRequest, ProposeTaskRequest, RequestIdOnly,
    ResumeRequest, SetPrincipal, SubmitTaskRequest, TaskIdRequest,
};
use serde::Deserialize;
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Instant,
};
use tower_http::{cors::CorsLayer, services::ServeDir};

/// The frontend is a different origin from this API by design (`dx serve`'s dev origin,
/// or a production Tauri build's bundled-frontend origin), not an accident to work around.
/// `localhost:8080`/`127.0.0.1:8080` is what `cargo tauri dev`'s `devUrl` loads;
/// `tauri.localhost`/`tauri://localhost` is where a production Tauri build serves its
/// bundled frontend from. Both the CORS layer (so the browser lets the request happen at
/// all) and `check_origin` (so a request that skips the browser, or whose Origin a proxy
/// stripped weirdly, still gets validated) check a request's `Origin` against this same
/// validated policy - they must agree, or one silently defeats the other.
fn cors_layer(access: &AccessPolicy, port: u16) -> CorsLayer {
    let origins: Vec<HeaderValue> = access
        .origins_for_port(port)
        .iter()
        .map(|origin| {
            origin
                .parse()
                .expect("static origin is a valid header value")
        })
        .collect();
    CorsLayer::new()
        .allow_origin(origins)
        .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
        .allow_headers([
            header::AUTHORIZATION,
            header::CONTENT_TYPE,
            HeaderName::from_static(CSRF_HEADER),
            HeaderName::from_static(REQUEST_ID_HEADER),
        ])
        .expose_headers([HeaderName::from_static(REQUEST_ID_HEADER)])
        // Needed for a cross-origin fetch() to actually send/receive the session and CSRF
        // cookies `/api/pair` sets - without it a browser silently drops both, and the
        // request looks authenticated-with-bearer-only even though session auth exists.
        .allow_credentials(true)
}

struct ServeState {
    root: PathBuf,
    port: u16,
    token: String,
    sessions: Mutex<SessionStore>,
    /// Persisted paired-device sessions, opened once at startup (schema included) and only
    /// ever used from `spawn_blocking`, never on an async worker thread.
    session_db: Arc<Mutex<Sessions>>,
    access: AccessPolicy,
    logger: Arc<JsonLogger>,
    onboard_progress:
        Arc<Mutex<std::collections::HashMap<String, crate::onboard::OnboardProgress>>>,
}

fn random_token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

/// A short code a person can type into a phone, not a secret meant to resist
/// SHA-256-grade brute force on its own - its security comes from being single-use,
/// five-minute-lived, burned after five wrong attempts, and served only on loopback
/// (reached remotely through Tailscale Serve). A LAN bind needs an explicit
/// `--allow-insecure-lan`, which also exposes this code in cleartext; see
/// `http_access::ensure_bind_allowed` and docs/REMOTE_ACCESS.md.
fn short_code() -> String {
    let hex = uuid::Uuid::new_v4()
        .simple()
        .to_string()
        .to_ascii_uppercase();
    format!("{}-{}", &hex[..4], &hex[4..8])
}

const SESSION_COOKIE: &str = "conductor_session";
const CSRF_COOKIE: &str = "conductor_csrf";
const CSRF_HEADER: &str = "x-csrf-token";
const REQUEST_ID_HEADER: &str = "x-request-id";
const PAIR_CODE_TTL_MS: u64 = 5 * 60 * 1000;
const MAX_PAIR_ATTEMPTS: u8 = 5;

struct PairingChallenge {
    code: String,
    expires_at: u64,
    failed_attempts: u8,
}

/// Ephemeral in-memory store for the short-lived 5-minute pairing challenge only.
/// Once paired, device sessions persist as SHA-256 hashes in `state.sqlite` (`src/sessions.rs`)
/// so paired phones survive server restarts and PC reboots while remaining individually revocable.
#[derive(Default)]
struct SessionStore {
    pending: Option<PairingChallenge>,
}

fn cookie_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    raw.split(';').find_map(|part| {
        let (key, value) = part.trim().split_once('=')?;
        (key == name).then_some(value)
    })
}

/// `Origin`/`Host` are browser-supplied and absent from a plain HTTP client (curl, the
/// Tauri sidecar's own request layer), so only reject them when present and wrong -
/// requiring them outright would just break every non-browser caller. The bearer token
/// is the actual authentication; these are defense-in-depth against a malicious page in
/// a real browser hitting this loopback port (DNS rebinding and the like).
type AuthError = (StatusCode, &'static str);

fn check_origin(headers: &HeaderMap, state: &ServeState) -> Result<(), AuthError> {
    // Host reflects the connection target (this server), so it's still checked against
    // our own port - unlike Origin, which reflects the *caller's* page and is legitimately
    // a different origin (the frontend), checked against the same access policy as CORS.
    if let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok())
        && !state.access.allows_host(host, state.port)
    {
        state.logger.warn(
            "serve",
            "auth.host_rejected",
            None,
            "rejected request with unexpected Host header",
            serde_json::json!({ "host": host, "port": state.port }),
        );
        return Err((StatusCode::FORBIDDEN, "unexpected Host"));
    }
    if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok())
        && !state.access.allows_origin_for_port(origin, state.port)
    {
        state.logger.warn(
            "serve",
            "auth.origin_rejected",
            None,
            "rejected request with unexpected Origin header",
            serde_json::json!({ "origin": origin }),
        );
        return Err((StatusCode::FORBIDDEN, "unexpected Origin"));
    }
    Ok(())
}

fn has_valid_bearer(headers: &HeaderMap, state: &ServeState) -> bool {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|presented| constant_time_eq(presented, &state.token))
}

/// Compares two secrets without an early exit on the first differing byte. Both sides are
/// hashed first, so neither the comparison time nor the loop length depends on how much of
/// the presented value matches the real token.
fn constant_time_eq(presented: &str, expected: &str) -> bool {
    use sha2::{Digest, Sha256};
    let (a, b) = (
        Sha256::digest(presented.as_bytes()),
        Sha256::digest(expected.as_bytes()),
    );
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// Who a request is acting as, once authenticated. `Bearer` is the existing desktop-GUI
/// path (trusts the client-supplied `by` field, unchanged). `Session` is the new
/// phone/browser path: the principal was fixed at pairing time, not read from the
/// request body, per docs/REMOTE_ACCESS.md's "record the authenticated principal rather
/// than accepting a caller-supplied approver name as identity."
enum Actor {
    Bearer,
    Session(String),
}

/// `mutating`: session-authenticated POSTs additionally need a matching CSRF header
/// (double-submit cookie pattern) - a bearer-token request can't be CSRF-forged in the
/// first place (a malicious page can't attach a custom `Authorization` header
/// cross-origin without the configured CORS policy allowing it), so this only applies
/// to the cookie-authenticated path.
async fn authenticate(
    headers: &HeaderMap,
    state: &Arc<ServeState>,
    mutating: bool,
) -> Result<Actor, AuthError> {
    check_origin(headers, state)?;
    if has_valid_bearer(headers, state) {
        return Ok(Actor::Bearer);
    }
    let session_token = cookie_value(headers, SESSION_COOKIE)
        .ok_or((StatusCode::UNAUTHORIZED, "missing bearer token or session"))?
        .to_string();
    let csrf_header = mutating.then(|| {
        headers
            .get(CSRF_HEADER)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    });
    // SQLite lookup plus the sliding-expiry write: blocking work, so keep it off the
    // async worker threads that serve every other request.
    with_session_db(state, move |sessions| {
        let Some(session) = sessions.verify(&session_token)? else {
            return Ok(Err((
                StatusCode::UNAUTHORIZED,
                "missing or expired session",
            )));
        };
        if let Some(csrf) = csrf_header
            && (csrf.is_empty() || !sessions.verify_csrf(&session, &csrf))
        {
            return Ok(Err((
                StatusCode::FORBIDDEN,
                "missing or invalid CSRF token",
            )));
        }
        Ok(Ok(Actor::Session(session.principal)))
    })
    .await
    .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "session lookup failed"))?
}

/// Runs `f` against the shared session store on the blocking pool. A poisoned lock is
/// recovered: the connection itself is still valid after a panic in another request.
async fn with_session_db<T: Send + 'static>(
    state: &Arc<ServeState>,
    f: impl FnOnce(&Sessions) -> anyhow::Result<T> + Send + 'static,
) -> anyhow::Result<T> {
    let db = Arc::clone(&state.session_db);
    tokio::task::spawn_blocking(move || {
        let sessions = db.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        f(&sessions)
    })
    .await
    .context("session store task failed")?
}

fn error_response(err: anyhow::Error) -> Response {
    let message = err.to_string();
    let status = if message.starts_with("no task id matches")
        || message.starts_with("ambiguous task id prefix")
        || message.contains("unknown policy id")
    {
        StatusCode::NOT_FOUND
    } else if message.contains("revision conflict")
        || message.starts_with("global stop is engaged")
        || message.contains("already revoked")
        || message.contains("is not engaged")
        || message.starts_with("cannot delete task:")
    {
        StatusCode::CONFLICT
    } else if message.contains("requires an actor") {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    (status, message).into_response()
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({"status": "ok"}))
}

async fn get_me(State(state): State<Arc<ServeState>>, headers: HeaderMap) -> Response {
    match authenticate(&headers, &state, false).await {
        Ok(Actor::Session(principal)) => {
            Json(serde_json::json!({"auth": "session", "principal": principal})).into_response()
        }
        Ok(Actor::Bearer) => {
            Json(serde_json::json!({"auth": "bearer", "principal": serde_json::Value::Null}))
                .into_response()
        }
        Err(err) => err.into_response(),
    }
}

async fn get_tasks(State(state): State<Arc<ServeState>>, headers: HeaderMap) -> Response {
    if let Err(err) = authenticate(&headers, &state, false).await {
        return err.into_response();
    }
    let root = state.root.clone();
    match tokio::task::spawn_blocking(move || TaskService::open(&root)?.board()).await {
        Ok(Ok(rows)) => Json(rows).into_response(),
        Ok(Err(err)) => error_response(err),
        Err(join_err) => (StatusCode::INTERNAL_SERVER_ERROR, join_err.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct DetailQuery {
    preview_bytes: Option<usize>,
    no_preview: Option<bool>,
}

#[derive(Deserialize)]
struct ClearTasksBody {
    scope: crate::service::ClearScope,
}

async fn post_clear_tasks(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
    Json(body): Json<ClearTasksBody>,
) -> Response {
    if let Err(err) = authenticate(&headers, &state, true).await {
        return err.into_response();
    }
    let root = state.root.clone();
    let scope = body.scope;
    match tokio::task::spawn_blocking(move || TaskService::open(&root)?.clear_tasks(scope)).await {
        Ok(Ok(outcome)) => {
            state.logger.info(
                "serve",
                "tasks.cleared",
                None,
                format!("cleared {} task(s)", outcome.deleted_ids.len()),
                serde_json::json!({
                    "scope": outcome.scope,
                    "deleted": outcome.deleted_ids.len(),
                    "skipped": outcome.skipped.len(),
                }),
            );
            Json(outcome).into_response()
        }
        Ok(Err(err)) => error_response(err),
        Err(join_err) => (StatusCode::INTERNAL_SERVER_ERROR, join_err.to_string()).into_response(),
    }
}

async fn delete_task(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(err) = authenticate(&headers, &state, true).await {
        return err.into_response();
    }
    let root = state.root.clone();
    match tokio::task::spawn_blocking(move || TaskService::open(&root)?.delete_task(&id)).await {
        Ok(Ok(deleted_id)) => {
            state.logger.info(
                "serve",
                "task.deleted",
                None,
                format!("deleted task {deleted_id}"),
                serde_json::json!({ "task_id": deleted_id }),
            );
            Json(serde_json::json!({ "deleted_id": deleted_id })).into_response()
        }
        Ok(Err(err)) => error_response(err),
        Err(join_err) => (StatusCode::INTERNAL_SERVER_ERROR, join_err.to_string()).into_response(),
    }
}

async fn get_task(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<DetailQuery>,
) -> Response {
    if let Err(err) = authenticate(&headers, &state, false).await {
        return err.into_response();
    }
    let root = state.root.clone();
    let preview_bytes = if query.no_preview.unwrap_or(false) {
        None
    } else {
        Some(query.preview_bytes.unwrap_or(1024))
    };
    match tokio::task::spawn_blocking(move || TaskService::open(&root)?.detail(&id, preview_bytes))
        .await
    {
        Ok(Ok(detail)) => Json(detail).into_response(),
        Ok(Err(err)) => error_response(err),
        Err(join_err) => (StatusCode::INTERNAL_SERVER_ERROR, join_err.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct RevisionBody {
    revision: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkflowBody {
    profile: String,
    objective: String,
    acceptance: String,
    #[serde(default)]
    allowed_files: Vec<String>,
    #[serde(default)]
    auto_scope: bool,
    #[serde(default)]
    generation_timeout_ms: Option<u64>,
    #[serde(default)]
    generation_idle_timeout_ms: Option<u64>,
    #[serde(default)]
    verification_timeout_ms: Option<u64>,
    #[serde(default)]
    soft_timeouts: Option<bool>,
    #[serde(default)]
    require_clean_checkout: Option<bool>,
}

#[derive(Deserialize, Default)]
struct ProviderQuery {
    #[serde(default)]
    refresh: bool,
}

async fn get_providers(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
    Query(query): Query<ProviderQuery>,
) -> Response {
    if let Err(err) = authenticate(&headers, &state, false).await {
        return err.into_response();
    }
    let root = state.root.clone();
    let refresh = query.refresh;
    match tokio::task::spawn_blocking(
        move || -> anyhow::Result<crate::provider_status::ProviderReport> {
            Ok(crate::provider_status::probe_all_providers_with_options(
                &root, None, refresh,
            ))
        },
    )
    .await
    {
        Ok(Ok(value)) => Json(value).into_response(),
        Ok(Err(err)) => error_response(err),
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
    }
}

async fn get_analytics(State(state): State<Arc<ServeState>>, headers: HeaderMap) -> Response {
    if let Err(err) = authenticate(&headers, &state, false).await {
        return err.into_response();
    }
    let root = state.root.clone();
    match tokio::task::spawn_blocking(
        move || -> anyhow::Result<crate::routing_metrics::AnalyticsReport> {
            let mut store = crate::routing_metrics::RoutingMetricsStore::open(&root)?;
            let board = store.scoreboard()?;
            Ok(board.analytics_report())
        },
    )
    .await
    {
        Ok(Ok(value)) => Json(value).into_response(),
        Ok(Err(err)) => error_response(err),
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
    }
}

fn workflow_profile_summary_with_index(
    p: &crate::workflow::Profile,
    indexed_files: Option<usize>,
) -> serde_json::Value {
    let first = p.workers.first().map(|w| &w.preset);
    let low_spec = p.decision_timeout_ms >= 120_000
        || first.is_some_and(|w| {
            w.generation_timeout_ms >= 900_000 || w.verification_timeout_ms >= 300_000
        });
    serde_json::json!({
        "name": p.name,
        "repository": p.repository.checkout,
        "model": p.inference.target(&p.inference.default_target).ok().map(|t| &t.model),
        "max_steps": p.max_steps,
        "decision_timeout_ms": p.decision_timeout_ms,
        "generation_timeout_ms": first.map(|w| w.generation_timeout_ms),
        "generation_idle_timeout_ms": first.and_then(|w| w.generation_idle_timeout_ms),
        "verification_timeout_ms": first.map(|w| w.verification_timeout_ms),
        "soft_timeouts": first.is_some_and(|w| w.soft_timeouts),
        "require_clean_checkout": p.require_clean_checkout,
        "low_spec": low_spec,
        "scope": p.scope(),
        "indexed_files": indexed_files,
        "workers": p
            .workers
            .iter()
            .map(|w| {
                serde_json::json!({
                    "name": w.preset.name,
                    "purpose": w.purpose,
                    "files": w.preset.allowed_files
                })
            })
            .collect::<Vec<_>>()
    })
}

async fn get_workflow_profiles(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(err) = authenticate(&headers, &state, false).await {
        return err.into_response();
    }
    let root = state.root.clone();
    match tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<serde_json::Value>> {
        Ok(crate::workflow::Workflows::open(&root)?
            .profiles()?
            .iter()
            .map(|p| {
                let count = crate::repo_index::profile_indexed_file_count(&root, &p.name)
                    .ok()
                    .flatten();
                workflow_profile_summary_with_index(p, count)
            })
            .collect())
    })
    .await
    {
        Ok(Ok(value)) => Json(value).into_response(),
        Ok(Err(err)) => error_response(err),
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
    }
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ReonboardBody {
    #[serde(default)]
    low_spec: Option<bool>,
    #[serde(default)]
    refresh_scope: Option<bool>,
    #[serde(default)]
    refresh_verifier: Option<bool>,
    #[serde(default)]
    by: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SuggestFilesBody {
    objective: String,
    #[serde(default)]
    acceptance: String,
}

async fn post_workflow_profile_suggest_files(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(body): Json<SuggestFilesBody>,
) -> Response {
    if let Err(err) = authenticate(&headers, &state, false).await {
        return err.into_response();
    }
    let root = state.root.clone();
    match tokio::task::spawn_blocking(
        move || -> anyhow::Result<crate::repo_index::SuggestFilesResponse> {
            let profile = crate::workflow::Workflows::open(&root)?.profile(&name)?;
            crate::repo_index::suggest_files_for_objective(
                &root,
                &profile,
                &body.objective,
                &body.acceptance,
                16,
            )
        },
    )
    .await
    {
        Ok(Ok(value)) => Json(value).into_response(),
        Ok(Err(err)) => error_response(err),
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
    }
}

async fn get_workflow_profile_files(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    if let Err(err) = authenticate(&headers, &state, false).await {
        return err.into_response();
    }
    let root = state.root.clone();
    match tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<String>> {
        let profile = crate::workflow::Workflows::open(&root)?.profile(&name)?;
        let excluded = profile
            .workers
            .first()
            .map(|w| w.preset.verification_files.as_slice())
            .unwrap_or(&[]);
        crate::onboard::list_editable_repo_files(&profile.repository.checkout, excluded)
    })
    .await
    {
        Ok(Ok(value)) => Json(value).into_response(),
        Ok(Err(err)) => error_response(err),
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
    }
}

async fn get_workflow_profile_repo_status(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    if let Err(err) = authenticate(&headers, &state, false).await {
        return err.into_response();
    }
    let root = state.root.clone();
    match tokio::task::spawn_blocking(move || -> anyhow::Result<serde_json::Value> {
        let profile = crate::workflow::Workflows::open(&root)?.profile(&name)?;
        let current = crate::repository::Repository::inspect(&profile.repository.checkout)?;
        let status = current.checkout_status()?;
        Ok(serde_json::json!({
            "repository": current.checkout,
            "base": current.base,
            "branch": current.branch,
            "clean": status.clean,
            "dirty_paths": status.dirty_paths,
            "unsupported_index_entries": status.unsupported_index_entries,
        }))
    })
    .await
    {
        Ok(Ok(value)) => Json(value).into_response(),
        Ok(Err(err)) => error_response(err),
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
    }
}

async fn put_workflow_profile_timeouts(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(body): Json<crate::workflow::TimeoutOverrides>,
) -> Response {
    match authenticate(&headers, &state, true).await {
        Ok(Actor::Bearer) => {}
        Ok(Actor::Session(_)) => {
            return (
                StatusCode::FORBIDDEN,
                "workflow profile timeout adjustment requires bearer authentication",
            )
                .into_response();
        }
        Err(err) => return err.into_response(),
    }
    let root = state.root.clone();
    let profile_for_log = name.clone();
    match tokio::task::spawn_blocking(move || -> anyhow::Result<serde_json::Value> {
        let updated =
            crate::workflow::Workflows::open(&root)?.update_profile_timeouts(&name, body)?;
        let count = crate::repo_index::profile_indexed_file_count(&root, &updated.name)
            .ok()
            .flatten();
        Ok(workflow_profile_summary_with_index(&updated, count))
    })
    .await
    {
        Ok(Ok(value)) => {
            state.logger.info(
                "onboard",
                "profile.timeouts_updated",
                None,
                format!("updated timeouts for workflow profile '{profile_for_log}'"),
                serde_json::json!({
                    "profile": profile_for_log,
                    "generation_timeout_ms": body.generation_timeout_ms,
                    "generation_idle_timeout_ms": body.generation_idle_timeout_ms,
                    "verification_timeout_ms": body.verification_timeout_ms,
                }),
            );
            Json(value).into_response()
        }
        Ok(Err(err)) => error_response(err),
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
    }
}

#[derive(Deserialize, Default)]
struct ProgressQuery {
    #[serde(default)]
    progress_id: Option<String>,
}

type OnboardProgressMap =
    Arc<Mutex<std::collections::HashMap<String, crate::onboard::OnboardProgress>>>;

fn set_onboard_progress(
    map: &OnboardProgressMap,
    job_id: Option<&str>,
    snapshot: crate::onboard::OnboardProgress,
) {
    let Some(id) = job_id.map(str::trim).filter(|s| !s.is_empty()) else {
        return;
    };
    if let Ok(mut guard) = map.lock() {
        if guard.len() >= 64 {
            guard.retain(|_, v| !v.finished);
        }
        guard.insert(id.to_string(), snapshot);
    }
}

async fn get_onboard_progress(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(err) = authenticate(&headers, &state, false).await {
        return err.into_response();
    }
    let snapshot = state
        .onboard_progress
        .lock()
        .ok()
        .and_then(|guard| guard.get(&id).cloned());
    match snapshot {
        Some(progress) => Json(progress).into_response(),
        None => (StatusCode::NOT_FOUND, "unknown onboard progress id").into_response(),
    }
}

async fn post_workflow_profile_reonboard(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Query(query): Query<ProgressQuery>,
    body: Option<Json<ReonboardBody>>,
) -> Response {
    match authenticate(&headers, &state, true).await {
        Ok(Actor::Bearer) => {}
        Ok(Actor::Session(_)) => {
            return (
                StatusCode::FORBIDDEN,
                "re-onboarding requires bearer authentication",
            )
                .into_response();
        }
        Err(err) => return err.into_response(),
    }
    let body = body.map(|b| b.0).unwrap_or_default();
    let root = state.root.clone();
    let profile_for_log = name.clone();
    let by = body
        .by
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("desktop-operator")
        .to_string();
    let refresh_scope = body.refresh_scope.unwrap_or(true);
    let progress_map = Arc::clone(&state.onboard_progress);
    let progress_id = query
        .progress_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty() && s.len() <= 128)
        .map(str::to_string);
    if let Some(ref pid) = progress_id {
        set_onboard_progress(
            &progress_map,
            Some(pid),
            crate::onboard::OnboardProgress {
                job_id: pid.clone(),
                profile_name: name.clone(),
                stage: "starting".into(),
                detail: format!("Starting re-onboard for '{name}'"),
                percent: 2,
                indexed_files: 0,
                total_index_files: 0,
                finished: false,
                error: None,
            },
        );
    }
    let pid_for_worker = progress_id.clone();
    let name_for_worker = name.clone();
    let progress_for_worker = Arc::clone(&progress_map);
    match tokio::task::spawn_blocking(move || -> anyhow::Result<serde_json::Value> {
        let mut cb = |stage: &str, detail: &str, percent: u8, done: usize, total: usize| {
            if let Some(ref pid) = pid_for_worker {
                set_onboard_progress(
                    &progress_for_worker,
                    Some(pid),
                    crate::onboard::OnboardProgress {
                        job_id: pid.clone(),
                        profile_name: name_for_worker.clone(),
                        stage: stage.to_string(),
                        detail: detail.to_string(),
                        percent,
                        indexed_files: done,
                        total_index_files: total,
                        finished: stage == "done",
                        error: None,
                    },
                );
            }
        };
        let profile = crate::onboard::reonboard_profile_with_options(
            &root,
            &name,
            body.low_spec,
            refresh_scope,
            body.refresh_verifier.unwrap_or(false),
            &by,
            &crate::onboard::DiscoveryEnv::from_process(),
            &mut cb,
        )?;
        let count = crate::repo_index::profile_indexed_file_count(&root, &profile.name)
            .ok()
            .flatten();
        Ok(workflow_profile_summary_with_index(&profile, count))
    })
    .await
    {
        Ok(Ok(value)) => {
            state.logger.info(
                "onboard",
                "onboard.reonboard",
                None,
                format!("re-onboarded workflow profile '{profile_for_log}'"),
                serde_json::json!({
                    "profile": profile_for_log,
                    "low_spec": body.low_spec,
                }),
            );
            Json(value).into_response()
        }
        Ok(Err(err)) => {
            if let Some(ref pid) = progress_id {
                set_onboard_progress(
                    &progress_map,
                    Some(pid),
                    crate::onboard::OnboardProgress {
                        job_id: pid.clone(),
                        profile_name: profile_for_log,
                        stage: "failed".into(),
                        detail: "Re-onboard failed".into(),
                        percent: 100,
                        indexed_files: 0,
                        total_index_files: 0,
                        finished: true,
                        error: Some(format!("{err:#}")),
                    },
                );
            }
            error_response(err)
        }
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
    }
}

async fn delete_workflow_profile(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    match authenticate(&headers, &state, true).await {
        Ok(Actor::Bearer) => {}
        Ok(Actor::Session(_)) => {
            return (
                StatusCode::FORBIDDEN,
                "workflow profile deletion requires bearer authentication",
            )
                .into_response();
        }
        Err(err) => return err.into_response(),
    }
    let root = state.root.clone();
    match tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        crate::workflow::Workflows::open(&root)?.delete_profile(&name)
    })
    .await
    {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(err)) => error_response(err),
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
    }
}

async fn get_agent_presets(State(state): State<Arc<ServeState>>, headers: HeaderMap) -> Response {
    if let Err(err) = authenticate(&headers, &state, false).await {
        return err.into_response();
    }
    let root = state.root.clone();
    match tokio::task::spawn_blocking(
        move || -> anyhow::Result<Vec<crate::agent_preset::AgentPreset>> {
            crate::agent_preset::AgentPresets::open(&root)?.list()
        },
    )
    .await
    {
        Ok(Ok(value)) => Json(value).into_response(),
        Ok(Err(err)) => error_response(err),
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct PresetRoutingBody {
    #[serde(default)]
    tier: Option<crate::agents::ModelTier>,
    #[serde(default)]
    allowed_models: Vec<String>,
}

async fn put_agent_preset_routing(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(body): Json<PresetRoutingBody>,
) -> Response {
    match authenticate(&headers, &state, true).await {
        Ok(Actor::Bearer) => {}
        Ok(Actor::Session(_)) => {
            return (
                StatusCode::FORBIDDEN,
                "agent preset routing configuration requires bearer authentication",
            )
                .into_response();
        }
        Err(err) => return err.into_response(),
    }
    let root = state.root.clone();
    match tokio::task::spawn_blocking(
        move || -> anyhow::Result<crate::agent_preset::AgentPreset> {
            crate::agent_preset::AgentPresets::open(&root)?.set_routing_policy(
                &name,
                body.tier,
                body.allowed_models,
            )
        },
    )
    .await
    {
        Ok(Ok(value)) => Json(value).into_response(),
        Ok(Err(err)) => error_response(err),
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
    }
}

async fn delete_agent_preset(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    match authenticate(&headers, &state, true).await {
        Ok(Actor::Bearer) => {}
        Ok(Actor::Session(_)) => {
            return (
                StatusCode::FORBIDDEN,
                "agent preset deletion requires bearer authentication",
            )
                .into_response();
        }
        Err(err) => return err.into_response(),
    }
    let root = state.root.clone();
    match tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        crate::agent_preset::AgentPresets::open(&root)?.delete(&name)
    })
    .await
    {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(err)) => error_response(err),
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
    }
}

async fn post_workflow(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
    Json(body): Json<WorkflowBody>,
) -> Response {
    if let Err(err) = authenticate(&headers, &state, true).await {
        return err.into_response();
    }
    let root = state.root.clone();
    match tokio::task::spawn_blocking(move || -> anyhow::Result<crate::board::Detail> {
        let mut workflows = crate::workflow::Workflows::open(&root)?;
        let scope = if !body.allowed_files.is_empty() {
            Some(body.allowed_files)
        } else if body.auto_scope {
            let profile = workflows.profile(&body.profile)?;
            let suggestion = crate::repo_index::suggest_files_for_objective(
                &root,
                &profile,
                &body.objective,
                &body.acceptance,
                16,
            )?;
            if suggestion.matched.is_empty() || suggestion.merged_scope.is_empty() {
                None
            } else {
                Some(suggestion.merged_scope)
            }
        } else {
            None
        };
        let w = workflows.create_with_overrides(
            &body.profile,
            body.objective,
            body.acceptance,
            scope,
            crate::workflow::TimeoutOverrides {
                generation_timeout_ms: body.generation_timeout_ms,
                generation_idle_timeout_ms: body.generation_idle_timeout_ms,
                verification_timeout_ms: body.verification_timeout_ms,
                soft_timeouts: body.soft_timeouts,
                require_clean_checkout: body.require_clean_checkout,
            },
        )?;
        TaskService::open(&root)?.detail(&w.id, None)
    })
    .await
    {
        Ok(Ok(value)) => Json(value).into_response(),
        Ok(Err(err)) => error_response(err),
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct ProbeBody {
    #[serde(default = "default_dot_path")]
    repository: PathBuf,
    #[serde(default)]
    endpoint: Option<String>,
}
fn default_dot_path() -> PathBuf {
    PathBuf::from(".")
}

async fn post_onboard_probe(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
    Json(body): Json<ProbeBody>,
) -> Response {
    match authenticate(&headers, &state, false).await {
        Ok(Actor::Bearer) => {}
        Ok(Actor::Session(_)) => {
            return (
                StatusCode::FORBIDDEN,
                "onboarding probe requires bearer authentication",
            )
                .into_response();
        }
        Err(err) => return err.into_response(),
    }
    let repo_for_log = body.repository.clone();
    match tokio::task::spawn_blocking(move || -> anyhow::Result<crate::onboard::ProbeReport> {
        crate::onboard::probe_environment(&body.repository, body.endpoint.as_deref())
    })
    .await
    {
        Ok(Ok(value)) => {
            state.logger.info(
                "onboard",
                "onboard.probe",
                None,
                format!("probed repository {}", value.repository.display()),
                serde_json::json!({
                    "repository": value.repository,
                    "git_clean": value.git_clean,
                    "recommended_model": value.recommended_model,
                    "ollama_running": value.ollama_running,
                    "detected_clis": value.detected_clis.len(),
                }),
            );
            Json(value).into_response()
        }
        Ok(Err(err)) => {
            state.logger.error(
                "onboard",
                "onboard.probe_error",
                None,
                format!("onboard probe failed: {err:#}"),
                serde_json::json!({"repository": repo_for_log}),
            );
            error_response(err)
        }
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
    }
}

async fn post_onboard(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
    Query(query): Query<ProgressQuery>,
    Json(body): Json<crate::onboard::OnboardRequest>,
) -> Response {
    match authenticate(&headers, &state, true).await {
        // Onboarding registers a host verifier executable and repository path (`D4`);
        // restrict it to local bearer-authenticated callers only.
        Ok(Actor::Bearer) => {}
        Ok(Actor::Session(_)) => {
            return (
                StatusCode::FORBIDDEN,
                "onboarding requires bearer authentication",
            )
                .into_response();
        }
        Err(err) => return err.into_response(),
    }
    let root = state.root.clone();
    let profile_for_log = body.profile_name.clone();
    let repo_for_log = body.repository_path.clone();
    let progress_map = Arc::clone(&state.onboard_progress);
    let progress_id = query
        .progress_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty() && s.len() <= 128)
        .map(str::to_string);
    if let Some(ref pid) = progress_id {
        set_onboard_progress(
            &progress_map,
            Some(pid),
            crate::onboard::OnboardProgress {
                job_id: pid.clone(),
                profile_name: profile_for_log.clone(),
                stage: "starting".into(),
                detail: format!("Starting onboard for '{profile_for_log}'"),
                percent: 2,
                indexed_files: 0,
                total_index_files: 0,
                finished: false,
                error: None,
            },
        );
    }
    let pid_for_worker = progress_id.clone();
    let name_for_worker = profile_for_log.clone();
    let progress_for_worker = Arc::clone(&progress_map);
    match tokio::task::spawn_blocking(move || -> anyhow::Result<serde_json::Value> {
        let mut cb = |stage: &str, detail: &str, percent: u8, done: usize, total: usize| {
            if let Some(ref pid) = pid_for_worker {
                set_onboard_progress(
                    &progress_for_worker,
                    Some(pid),
                    crate::onboard::OnboardProgress {
                        job_id: pid.clone(),
                        profile_name: name_for_worker.clone(),
                        stage: stage.to_string(),
                        detail: detail.to_string(),
                        percent,
                        indexed_files: done,
                        total_index_files: total,
                        finished: stage == "done",
                        error: None,
                    },
                );
            }
        };
        let profile = crate::onboard::execute_onboard_with_progress(
            &root,
            body,
            &crate::onboard::DiscoveryEnv::from_process(),
            &mut cb,
        )?;
        let count = crate::repo_index::profile_indexed_file_count(&root, &profile.name)
            .ok()
            .flatten();
        Ok(workflow_profile_summary_with_index(&profile, count))
    })
    .await
    {
        Ok(Ok(value)) => {
            state.logger.info(
                "onboard",
                "onboard.complete",
                None,
                format!("onboarded workflow profile '{profile_for_log}'"),
                serde_json::json!({
                    "profile": profile_for_log,
                    "repository": repo_for_log,
                }),
            );
            Json(value).into_response()
        }
        Ok(Err(err)) => {
            if let Some(ref pid) = progress_id {
                set_onboard_progress(
                    &progress_map,
                    Some(pid),
                    crate::onboard::OnboardProgress {
                        job_id: pid.clone(),
                        profile_name: profile_for_log.clone(),
                        stage: "failed".into(),
                        detail: "Onboarding failed".into(),
                        percent: 100,
                        indexed_files: 0,
                        total_index_files: 0,
                        finished: true,
                        error: Some(format!("{err:#}")),
                    },
                );
            }
            state.logger.error(
                "onboard",
                "onboard.error",
                None,
                format!("onboard failed for '{profile_for_log}': {err:#}"),
                serde_json::json!({
                    "profile": profile_for_log,
                    "repository": repo_for_log,
                }),
            );
            error_response(err)
        }
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct ApproveBody {
    revision: u64,
    digest: String,
    by: String,
}

#[derive(Deserialize)]
struct CancelBody {
    revision: u64,
    #[serde(default)]
    by: Option<String>,
}

#[derive(Deserialize)]
struct ReviewBody {
    revision: u64,
    digest: String,
    by: String,
    accept: bool,
}

#[derive(Deserialize)]
struct RetryBody {
    revision: u64,
    #[serde(default)]
    by: String,
    #[serde(default)]
    reset_workspace: bool,
    #[serde(default)]
    from_step: Option<usize>,
    #[serde(default)]
    extra_steps: Option<usize>,
    #[serde(default)]
    guidance: Option<String>,
}

#[derive(Deserialize)]
struct IntegrationBody {
    revision: u64,
    digest: String,
    by: String,
    #[serde(default)]
    rollback: bool,
}
#[derive(Deserialize)]
struct IntegrationPrepareBody {
    by: String,
    #[serde(default)]
    target: crate::integration::IntegrationTarget,
}
async fn post_integration_prepare(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<IntegrationPrepareBody>,
) -> Response {
    let by = match authenticate(&headers, &state, true).await {
        Ok(Actor::Session(principal)) => principal,
        Ok(Actor::Bearer) => body.by,
        Err(err) => return err.into_response(),
    };
    let root = state.root.clone();
    let target = body.target;
    match tokio::task::spawn_blocking(move || {
        TaskService::open(&root)?.prepare_integration_with_target(&id, &by, target)
    })
    .await
    {
        Ok(Ok(detail)) => Json(detail).into_response(),
        Ok(Err(err)) => error_response(err),
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
    }
}
async fn post_integrate(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<IntegrationBody>,
) -> Response {
    let by = match authenticate(&headers, &state, true).await {
        Ok(Actor::Session(principal)) => principal,
        Ok(Actor::Bearer) => body.by,
        Err(err) => return err.into_response(),
    };
    let root = state.root.clone();
    match tokio::task::spawn_blocking(move || {
        TaskService::open(&root)?.integrate(&id, body.revision, &body.digest, &by, body.rollback)
    })
    .await
    {
        Ok(Ok(detail)) => Json(detail).into_response(),
        Ok(Err(err)) => error_response(err),
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
    }
}

/// Write handlers all return 200 with the task's current `board::Detail`, even when the
/// operation itself didn't "succeed" in the sense `board-run`'s CLI exit code means
/// (e.g. a run that failed verification) - the HTTP request to perform the mutation
/// succeeded; whether the resulting task needs attention is in the body, same as a human
/// reading `board-show`'s text would see it. Conflating the two into the status code is
/// what the CLI's exit-code convention has to do for scripting; HTTP has a body for this.
async fn post_approve(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<ApproveBody>,
) -> Response {
    let actor = match authenticate(&headers, &state, true).await {
        Ok(actor) => actor,
        Err(err) => return err.into_response(),
    };
    let by = match actor {
        Actor::Session(principal) => principal,
        Actor::Bearer => body.by,
    };
    let root = state.root.clone();
    match tokio::task::spawn_blocking(move || {
        TaskService::open(&root)?.approve(&id, body.revision, &body.digest, &by)
    })
    .await
    {
        Ok(Ok(detail)) => Json(detail).into_response(),
        Ok(Err(err)) => error_response(err),
        Err(join_err) => (StatusCode::INTERNAL_SERVER_ERROR, join_err.to_string()).into_response(),
    }
}

async fn post_run(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<RevisionBody>,
) -> Response {
    if let Err(err) = authenticate(&headers, &state, true).await {
        return err.into_response();
    }
    let root = state.root.clone();
    match tokio::task::spawn_blocking(move || TaskService::open(&root)?.run(&id, body.revision))
        .await
    {
        Ok(Ok(outcome)) => Json(outcome.detail).into_response(),
        Ok(Err(err)) => error_response(err),
        Err(join_err) => (StatusCode::INTERNAL_SERVER_ERROR, join_err.to_string()).into_response(),
    }
}

async fn post_cancel(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<CancelBody>,
) -> Response {
    let actor = match authenticate(&headers, &state, true).await {
        Ok(actor) => actor,
        Err(err) => return err.into_response(),
    };
    let by = match actor {
        Actor::Session(principal) => Some(principal),
        Actor::Bearer => body.by,
    };
    let root = state.root.clone();
    match tokio::task::spawn_blocking(move || {
        TaskService::open(&root)?.cancel(&id, body.revision, by.as_deref())
    })
    .await
    {
        Ok(Ok(detail)) => Json(detail).into_response(),
        Ok(Err(err)) => error_response(err),
        Err(join_err) => (StatusCode::INTERNAL_SERVER_ERROR, join_err.to_string()).into_response(),
    }
}

async fn post_reconcile(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<RevisionBody>,
) -> Response {
    if let Err(err) = authenticate(&headers, &state, true).await {
        return err.into_response();
    }
    let root = state.root.clone();
    match tokio::task::spawn_blocking(move || {
        TaskService::open(&root)?.reconcile(&id, body.revision)
    })
    .await
    {
        Ok(Ok(detail)) => Json(detail).into_response(),
        Ok(Err(err)) => error_response(err),
        Err(join_err) => (StatusCode::INTERNAL_SERVER_ERROR, join_err.to_string()).into_response(),
    }
}

async fn post_review(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<ReviewBody>,
) -> Response {
    let actor = match authenticate(&headers, &state, true).await {
        Ok(actor) => actor,
        Err(err) => return err.into_response(),
    };
    let by = match actor {
        Actor::Session(principal) => principal,
        Actor::Bearer => body.by,
    };
    let root = state.root.clone();
    match tokio::task::spawn_blocking(move || {
        TaskService::open(&root)?.review(&id, body.revision, &body.digest, &by, body.accept)
    })
    .await
    {
        Ok(Ok(detail)) => Json(detail).into_response(),
        Ok(Err(err)) => error_response(err),
        Err(join_err) => (StatusCode::INTERNAL_SERVER_ERROR, join_err.to_string()).into_response(),
    }
}

async fn post_retry(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<RetryBody>,
) -> Response {
    let actor = match authenticate(&headers, &state, true).await {
        Ok(actor) => actor,
        Err(err) => return err.into_response(),
    };
    let by = match actor {
        Actor::Session(principal) => principal,
        Actor::Bearer => body.by,
    };
    let root = state.root.clone();
    match tokio::task::spawn_blocking(move || {
        TaskService::open(&root)?.retry(
            &id,
            body.revision,
            &by,
            body.reset_workspace,
            body.from_step,
            body.extra_steps,
            body.guidance,
        )
    })
    .await
    {
        Ok(Ok(detail)) => Json(detail).into_response(),
        Ok(Err(err)) => error_response(err),
        Err(join_err) => (StatusCode::INTERNAL_SERVER_ERROR, join_err.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct DependBody {
    on: String,
}

async fn post_depend(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<DependBody>,
) -> Response {
    if let Err(err) = authenticate(&headers, &state, true).await {
        return err.into_response();
    }
    let root = state.root.clone();
    match tokio::task::spawn_blocking(move || {
        TaskService::open(&root)?.add_dependency(&id, &body.on)
    })
    .await
    {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(err)) => error_response(err),
        Err(join_err) => (StatusCode::INTERNAL_SERVER_ERROR, join_err.to_string()).into_response(),
    }
}

async fn get_leases(State(state): State<Arc<ServeState>>, headers: HeaderMap) -> Response {
    if let Err(err) = authenticate(&headers, &state, false).await {
        return err.into_response();
    }
    let root = state.root.clone();
    match tokio::task::spawn_blocking(move || TaskService::open(&root)?.leases()).await {
        Ok(Ok(leases)) => Json(leases).into_response(),
        Ok(Err(err)) => error_response(err),
        Err(join_err) => (StatusCode::INTERNAL_SERVER_ERROR, join_err.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct AcknowledgeLeaseBody {
    resource: String,
    #[serde(default)]
    by: String,
    reason: String,
}

/// Session-authenticated requests use the session's own principal as `by`, same as
/// approve/cancel/review - this is a real administrative action, and the identity behind
/// it shouldn't be a caller-supplied body field any more than those are.
async fn post_leases_acknowledge(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
    Json(body): Json<AcknowledgeLeaseBody>,
) -> Response {
    let actor = match authenticate(&headers, &state, true).await {
        Ok(actor) => actor,
        Err(err) => return err.into_response(),
    };
    let by = match actor {
        Actor::Session(principal) => principal,
        Actor::Bearer => body.by,
    };
    let root = state.root.clone();
    match tokio::task::spawn_blocking(move || {
        TaskService::open(&root)?.acknowledge_lease(&body.resource, &by, &body.reason)
    })
    .await
    {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(err)) => error_response(err),
        Err(join_err) => (StatusCode::INTERNAL_SERVER_ERROR, join_err.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct PolicyCreateBody {
    repository: String,
    by: String,
    max_tasks: usize,
    expires_in_ms: u64,
    /// Fake-engine policies (unchanged M7 contract). Exactly one of
    /// `experiments`/`real_tasks` is required - a policy authorizes one
    /// engine's worth of work, never a mix.
    #[serde(default)]
    experiments: Option<Vec<crate::autonomy::ExperimentSpec>>,
    /// M10: real `local-changes-v1` tasks - see
    /// `crate::autonomy::RealPolicyAuthorization`.
    #[serde(default)]
    real_tasks: Option<crate::autonomy::RealPolicyAuthorization>,
}

/// Unlike tasks, a policy id is never id-prefix-resolved (`Policies::get` takes an exact
/// id) - there's no cross-engine ambiguity to resolve here, so there was nothing for
/// `TaskService` to add over calling `autonomy::Policies` directly.
async fn post_policy(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
    Json(body): Json<PolicyCreateBody>,
) -> Response {
    if let Err(err) = authenticate(&headers, &state, true).await {
        return err.into_response();
    }
    let root = state.root.clone();
    match tokio::task::spawn_blocking(move || {
        anyhow::ensure!(
            body.experiments.is_some() != body.real_tasks.is_some(),
            "exactly one of experiments/real_tasks is required"
        );
        if let Some(real_tasks) = body.real_tasks {
            crate::autonomy::Policies::open(&root)?.create_real(
                &body.repository,
                &body.by,
                body.max_tasks,
                crate::now() + body.expires_in_ms,
                crate::autonomy::RealPolicyAuthorizationShared {
                    config: real_tasks.config,
                    launcher: std::env::current_exe()?,
                },
                real_tasks.tasks,
            )
        } else {
            crate::autonomy::Policies::open(&root)?.create(
                &body.repository,
                &body.by,
                body.max_tasks,
                crate::now() + body.expires_in_ms,
                body.experiments.unwrap(),
            )
        }
    })
    .await
    {
        Ok(Ok(policy)) => Json(policy).into_response(),
        Ok(Err(err)) => error_response(err),
        Err(join_err) => (StatusCode::INTERNAL_SERVER_ERROR, join_err.to_string()).into_response(),
    }
}

async fn get_policy(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(err) = authenticate(&headers, &state, false).await {
        return err.into_response();
    }
    let root = state.root.clone();
    match tokio::task::spawn_blocking(move || crate::autonomy::Policies::open(&root)?.get(&id))
        .await
    {
        Ok(Ok(policy)) => Json(policy).into_response(),
        Ok(Err(err)) => error_response(err),
        Err(join_err) => (StatusCode::INTERNAL_SERVER_ERROR, join_err.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct PolicyRevokeBody {
    #[serde(default)]
    by: String,
    reason: String,
}

async fn post_policy_revoke(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<PolicyRevokeBody>,
) -> Response {
    let actor = match authenticate(&headers, &state, true).await {
        Ok(actor) => actor,
        Err(err) => return err.into_response(),
    };
    let by = match actor {
        Actor::Session(principal) => principal,
        Actor::Bearer => body.by,
    };
    let root = state.root.clone();
    match tokio::task::spawn_blocking(move || {
        crate::autonomy::Policies::open(&root)?.revoke(&id, &by, &body.reason)
    })
    .await
    {
        Ok(Ok(policy)) => Json(policy).into_response(),
        Ok(Err(err)) => error_response(err),
        Err(join_err) => (StatusCode::INTERNAL_SERVER_ERROR, join_err.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct HoldMyBeerBody {
    #[serde(default = "default_budget")]
    budget: usize,
    #[serde(default = "default_timeout_ms")]
    timeout_ms: u64,
    #[serde(default = "default_timeout_ms")]
    verification_timeout_ms: u64,
    #[serde(default = "default_log_bytes")]
    log_bytes: usize,
}
fn default_budget() -> usize {
    16384
}
fn default_timeout_ms() -> u64 {
    5000
}
fn default_log_bytes() -> usize {
    32768
}

async fn post_policy_hold_my_beer(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<HoldMyBeerBody>,
) -> Response {
    if let Err(err) = authenticate(&headers, &state, true).await {
        return err.into_response();
    }
    let root = state.root.clone();
    match tokio::task::spawn_blocking(move || {
        let mut execution = crate::process::ExecutionPolicy::new(&std::env::current_exe()?)?;
        execution.timeout_ms = body.timeout_ms;
        execution.verification_timeout_ms = body.verification_timeout_ms;
        execution.log_bytes = body.log_bytes;
        crate::autonomy::run(&root, &id, execution, body.budget)
    })
    .await
    {
        Ok(Ok(outcome)) => Json(outcome).into_response(),
        Ok(Err(err)) => error_response(err),
        Err(join_err) => (StatusCode::INTERNAL_SERVER_ERROR, join_err.to_string()).into_response(),
    }
}

async fn get_stop(State(state): State<Arc<ServeState>>, headers: HeaderMap) -> Response {
    if let Err(err) = authenticate(&headers, &state, false).await {
        return err.into_response();
    }
    let root = state.root.clone();
    match tokio::task::spawn_blocking(move || TaskService::open(&root)?.stop_state()).await {
        Ok(Ok(stop_state)) => Json(stop_state).into_response(),
        Ok(Err(err)) => error_response(err),
        Err(join_err) => (StatusCode::INTERNAL_SERVER_ERROR, join_err.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct StopBody {
    #[serde(default)]
    by: String,
    reason: String,
}

/// Session-authenticated requests use the session's own principal as `by`, same
/// reasoning as every other administrative action here (approve/cancel/review/
/// leases-acknowledge). `POST /api/stop` engages the switch and sweeps every active
/// attempt through `TaskService::stop_all`, exactly like `global-stop` on the CLI;
/// `POST /api/stop/resume` only ever clears it.
async fn post_stop(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
    Json(body): Json<StopBody>,
) -> Response {
    let actor = match authenticate(&headers, &state, true).await {
        Ok(actor) => actor,
        Err(err) => return err.into_response(),
    };
    let by = match actor {
        Actor::Session(principal) => principal,
        Actor::Bearer => body.by,
    };
    let reason = body.reason;
    let root = state.root.clone();
    match tokio::task::spawn_blocking(move || TaskService::open(&root)?.stop_all(&by, &reason))
        .await
    {
        Ok(Ok(outcome)) => Json(outcome).into_response(),
        Ok(Err(err)) => error_response(err),
        Err(join_err) => (StatusCode::INTERNAL_SERVER_ERROR, join_err.to_string()).into_response(),
    }
}

async fn post_stop_resume(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
    Json(body): Json<StopBody>,
) -> Response {
    let actor = match authenticate(&headers, &state, true).await {
        Ok(actor) => actor,
        Err(err) => return err.into_response(),
    };
    let by = match actor {
        Actor::Session(principal) => principal,
        Actor::Bearer => body.by,
    };
    let reason = body.reason;
    let root = state.root.clone();
    match tokio::task::spawn_blocking(move || TaskService::open(&root)?.resume(&by, &reason)).await
    {
        Ok(Ok(stop_state)) => Json(stop_state).into_response(),
        Ok(Err(err)) => error_response(err),
        Err(join_err) => (StatusCode::INTERNAL_SERVER_ERROR, join_err.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct PairBody {
    code: String,
    principal: String,
}

/// Secure is relaxed only for an exact host whose operator explicitly asserts a
/// trusted encrypted HTTP transport in private deployment configuration.
fn set_cookie(
    name: &str,
    value: &str,
    max_age_secs: u64,
    http_only: bool,
    secure: bool,
) -> HeaderValue {
    let http_only_attr = if http_only { "; HttpOnly" } else { "" };
    let secure_attr = if secure { "; Secure" } else { "" };
    format!("{name}={value}; Path=/; Max-Age={max_age_secs}; SameSite=Strict{secure_attr}{http_only_attr}")
        .parse()
        .expect("generated cookie is a valid header value")
}

fn clear_cookie(name: &str, http_only: bool, secure: bool) -> HeaderValue {
    set_cookie(name, "", 0, http_only, secure)
}

/// Whether to send cookies with `Secure` for this request, per `set_cookie`'s doc comment.
fn cookies_should_be_secure(headers: &HeaderMap, state: &ServeState) -> bool {
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    state.access.secure_cookies(host)
}

/// Mints a short-lived single-use pairing code, replacing any still-pending one. Bearer
/// auth only - requires host filesystem access to `serve-token`, the same trust boundary
/// that already gates everything else; a session can't mint codes for itself.
async fn post_pair_start(State(state): State<Arc<ServeState>>, headers: HeaderMap) -> Response {
    if let Err(err) = check_origin(&headers, &state) {
        return err.into_response();
    }
    if !has_valid_bearer(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, "missing or invalid bearer token").into_response();
    }
    let code = short_code();
    let expires_at = crate::now() + PAIR_CODE_TTL_MS;
    state
        .sessions
        .lock()
        .expect("sessions mutex poisoned")
        .pending = Some(PairingChallenge {
        code: code.clone(),
        expires_at,
        failed_attempts: 0,
    });
    state.logger.info(
        "serve",
        "pair.start",
        None,
        "minted single-use pairing code",
        serde_json::json!({ "expires_at": expires_at }),
    );
    Json(serde_json::json!({"code": code, "expires_at": expires_at})).into_response()
}

/// Exchanges a valid pairing code for a session. No bearer auth here by design - the
/// code itself is the credential a device without the bearer token presents. `principal`
/// is fixed for the session's lifetime from this one request, never read from a later
/// request body, so a phone session can't impersonate a different approver after pairing.
async fn post_pair(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
    Json(body): Json<PairBody>,
) -> Response {
    if let Err(err) = check_origin(&headers, &state) {
        return err.into_response();
    }
    let principal = body.principal.trim().to_string();
    if principal.is_empty() {
        return (StatusCode::BAD_REQUEST, "principal is required").into_response();
    }
    let presented = body.code.trim().to_ascii_uppercase();
    // The guard lives only inside this block, so it is never held across the `.await` on the
    // session store below (a std `MutexGuard` would also make this handler non-`Send`).
    let (matched, failed_attempts, burned) = {
        let mut store = state.sessions.lock().expect("sessions mutex poisoned");
        let now = crate::now();
        let mut failed_attempts = 0u8;
        let mut burned = false;
        let matched = match store.pending.as_mut() {
            Some(pending) if pending.expires_at <= now => {
                store.pending = None;
                burned = true;
                false
            }
            Some(pending) if constant_time_eq(&presented, &pending.code) => true,
            Some(pending) => {
                pending.failed_attempts = pending.failed_attempts.saturating_add(1);
                failed_attempts = pending.failed_attempts;
                if pending.failed_attempts >= MAX_PAIR_ATTEMPTS {
                    store.pending = None;
                    burned = true;
                }
                false
            }
            None => false,
        };
        if matched {
            // Single use: consume the challenge before the session is created.
            store.pending = None;
        }
        (matched, failed_attempts, burned)
    };
    if !matched {
        state.logger.warn(
            "serve",
            "pair.failed",
            None,
            "pairing code rejected",
            serde_json::json!({
                "principal": principal,
                "failed_attempts": failed_attempts,
                "challenge_burned": burned,
            }),
        );
        return (StatusCode::UNAUTHORIZED, "invalid or expired pairing code").into_response();
    }
    let session_id = random_token();
    let csrf = random_token();
    let created = {
        let (session_id, csrf, principal) = (session_id.clone(), csrf.clone(), principal.clone());
        with_session_db(&state, move |s| {
            s.create(&session_id, &csrf, &principal, SESSION_TTL_MS)
        })
        .await
    };
    let summary = match created {
        Ok(summary) => summary,
        Err(err) => return error_response(err),
    };
    let max_age = SESSION_TTL_MS / 1000;
    let secure = cookies_should_be_secure(&headers, &state);
    state.logger.info(
        "serve",
        "pair.success",
        None,
        "paired remote device session",
        serde_json::json!({
            "session_id": summary.id,
            "principal": summary.principal,
            "secure_cookie": secure,
            "expires_at": summary.expires_at,
        }),
    );
    (
        StatusCode::OK,
        AppendHeaders([
            (
                header::SET_COOKIE,
                set_cookie(SESSION_COOKIE, &session_id, max_age, true, secure),
            ),
            (
                header::SET_COOKIE,
                set_cookie(CSRF_COOKIE, &csrf, max_age, false, secure),
            ),
        ]),
        Json(serde_json::json!({
            "id": summary.id,
            "principal": summary.principal,
            "expires_at": summary.expires_at,
        })),
    )
        .into_response()
}

/// Best-effort: clears the cookies regardless, and revokes the persisted session in SQLite
/// if the request still carried one. Doesn't itself require being authenticated - a session
/// that's already expired or was never valid should still be able to clear its cookies.
async fn post_logout(State(state): State<Arc<ServeState>>, headers: HeaderMap) -> Response {
    if let Err(err) = check_origin(&headers, &state) {
        return err.into_response();
    }
    if let Some(session_id) = cookie_value(&headers, SESSION_COOKIE) {
        let session_id = session_id.to_string();
        let _ = with_session_db(&state, move |s| s.revoke_by_token(&session_id)).await;
    }
    let secure = cookies_should_be_secure(&headers, &state);
    (
        StatusCode::OK,
        AppendHeaders([
            (
                header::SET_COOKIE,
                clear_cookie(SESSION_COOKIE, true, secure),
            ),
            (header::SET_COOKIE, clear_cookie(CSRF_COOKIE, false, secure)),
        ]),
    )
        .into_response()
}

async fn get_sessions(State(state): State<Arc<ServeState>>, headers: HeaderMap) -> Response {
    if let Err(err) = check_origin(&headers, &state) {
        return err.into_response();
    }
    if !has_valid_bearer(&headers, &state) {
        return (
            StatusCode::UNAUTHORIZED,
            "session management requires bearer authentication",
        )
            .into_response();
    }
    match with_session_db(&state, |s| s.list_active()).await {
        Ok(list) => Json(list).into_response(),
        Err(err) => error_response(err),
    }
}

async fn post_session_revoke(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(err) = check_origin(&headers, &state) {
        return err.into_response();
    }
    if !has_valid_bearer(&headers, &state) {
        return (
            StatusCode::UNAUTHORIZED,
            "session revocation requires bearer authentication",
        )
            .into_response();
    }
    match with_session_db(&state, move |s| s.revoke_by_id(&id)).await {
        Ok(revoked) => Json(serde_json::json!({"revoked": revoked})).into_response(),
        Err(err) => error_response(err),
    }
}

async fn post_sessions_revoke_all(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(err) = check_origin(&headers, &state) {
        return err.into_response();
    }
    if !has_valid_bearer(&headers, &state) {
        return (
            StatusCode::UNAUTHORIZED,
            "session revocation requires bearer authentication",
        )
            .into_response();
    }
    match with_session_db(&state, |s| s.revoke_all()).await {
        Ok(revoked) => Json(serde_json::json!({"revoked": revoked})).into_response(),
        Err(err) => error_response(err),
    }
}

/// `{"tool": "conductor_submit_task", "params": {...}}` - one request body shape for all
/// 15 `conductor_*` tools, mirroring MCP's own `tools/call` envelope. This is `dev-
/// conductor-edge`'s `RemoteBackend` on the wire: the edge speaks MCP to its own stdio
/// caller and this one HTTP shape to the main Conductor, both carrying the identical
/// `conductor-protocol` DTOs. See `docs/m12/M12_0_ARCHITECTURE.md` section 3.
#[derive(Deserialize)]
struct EdgeToolCall {
    tool: String,
    #[serde(default)]
    params: serde_json::Value,
}

fn backend_error_response(err: BackendError) -> Response {
    let status = match err.kind {
        ErrorKind::NotFound => StatusCode::NOT_FOUND,
        ErrorKind::Conflict => StatusCode::CONFLICT,
        ErrorKind::InvalidParams => StatusCode::BAD_REQUEST,
        ErrorKind::Forbidden => StatusCode::FORBIDDEN,
        ErrorKind::Internal => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, Json(err)).into_response()
}

fn invalid_tool_params(tool: &str, error: serde_json::Error) -> Response {
    backend_error_response(BackendError::invalid_params(format!(
        "invalid params for {tool}: {error}"
    )))
}

/// Pulls the raw bearer token out of `Authorization: Bearer <token>`, if present at all -
/// deliberately separate from `has_valid_bearer` (which only ever compares against this
/// process's *own* desktop-GUI token), since the edge route now verifies against enrolled
/// clients instead.
fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
}

/// M12.0 Phase 2: authenticates `/api/edge/tools/call` against a real enrolled client
/// instead of Phase 1's placeholder (the shared desktop-GUI bearer token) - see
/// `docs/m12/M12_0_ARCHITECTURE.md` section 3 and `src/enrollment.rs`'s module doc.
/// `enrollment::Enrollments::verify` re-reads `state.sqlite` fresh on every call, so a
/// revocation takes effect on the very next request, never a cached one. Missing/unknown/
/// forged tokens all get the identical generic 401 - deliberately not distinguishing
/// "wrong token" from "no such client" in the response, which would otherwise let a caller
/// enumerate valid client ids by observing a different error for each. A *recognized but
/// revoked* credential gets a 403 naming the actual reason, since the caller already holds
/// a credential a human issued, not one it's guessing at.
/// Small on purpose (`clippy::result_large_err` flags a bare `Response` as an `Err`
/// variant - it's over 100 bytes) - `authenticate_edge` returns this instead and the one
/// call site turns it into a real `Response`.
enum EdgeAuthError {
    Unauthorized(&'static str),
    Forbidden(String),
    Internal(anyhow::Error),
}

impl EdgeAuthError {
    fn into_response(self) -> Response {
        match self {
            Self::Unauthorized(message) => (StatusCode::UNAUTHORIZED, message).into_response(),
            Self::Forbidden(message) => backend_error_response(BackendError::forbidden(message)),
            Self::Internal(error) => backend_error_response(BackendError::from(error)),
        }
    }
}

fn authenticate_edge(
    headers: &HeaderMap,
    root: &std::path::Path,
) -> Result<Enrollment, EdgeAuthError> {
    let Some(token) = bearer_token(headers) else {
        return Err(EdgeAuthError::Unauthorized("missing bearer token"));
    };
    let enrollments = Enrollments::open(root).map_err(EdgeAuthError::Internal)?;
    match enrollments.verify(token).map_err(EdgeAuthError::Internal)? {
        VerifyOutcome::Valid(enrollment) => Ok(enrollment),
        VerifyOutcome::Revoked(enrollment) => Err(EdgeAuthError::Forbidden(format!(
            "enrollment {} was revoked{}",
            enrollment.client_id,
            enrollment
                .revoked_reason
                .as_deref()
                .map(|r| format!(": {r}"))
                .unwrap_or_default()
        ))),
        VerifyOutcome::Unknown => Err(EdgeAuthError::Unauthorized("invalid bearer token")),
    }
}

/// Dispatches to `LocalBackend`, the exact same `ConductorBackend` implementation stdio
/// `mcp-serve` uses - one engine boundary, two transports in front of it. Every call is
/// authenticated against a real enrolled client (`authenticate_edge`), gated by that
/// client's coarse `allowed_operations` before the engine is ever touched, and - for every
/// tool whose request carries a `principal` field - has that field overwritten with the
/// verified `client_id` before the engine sees it: the model never supplies or overrides
/// its own authenticated identity on this path, per this milestone's own requirement.
/// Repository-scope enforcement is deliberately not yet wired in here - see
/// `src/enrollment.rs`'s module doc for exactly why and where that lands (M12.0 Phase 3).
async fn post_edge_tools_call(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
    Json(call): Json<EdgeToolCall>,
) -> Response {
    let enrollment = match authenticate_edge(&headers, &state.root) {
        Ok(enrollment) => enrollment,
        Err(error) => return error.into_response(),
    };
    let Some(required) = EnrollmentOperation::for_tool(&call.tool) else {
        return backend_error_response(BackendError::invalid_params_with(
            format!("unknown tool '{}'", call.tool),
            serde_json::json!({"kind": "unknown_tool"}),
        ));
    };
    if !enrollment.allows(required) {
        return backend_error_response(BackendError::forbidden(format!(
            "enrollment {} is not allowed to call {} (requires '{}', allowed: {})",
            enrollment.client_id,
            call.tool,
            required.as_str(),
            enrollment
                .allowed_operations
                .iter()
                .map(|o| o.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    // M12.0 Phase 3 / M12.0.1 Phase 1: `open_trusted` is only ever correct here because
    // `authenticate_edge` just cryptographically verified `enrollment`, and every
    // principal-bearing request below is about to be overwritten with its *namespaced*
    // identity before `backend` sees it - see `enrollment::PRINCIPAL_NAMESPACE_PREFIX`
    // and `LocalBackend::trusted`. `enrollment.repository_scope` is passed verbatim -
    // never widened, never derived from anything the request itself says.
    let backend =
        match LocalBackend::open_trusted(state.root.clone(), enrollment.repository_scope.clone()) {
            Ok(backend) => backend,
            Err(error) => return backend_error_response(BackendError::from(error)),
        };
    let verified_principal = enrollment::namespaced_principal(&enrollment.client_id);
    macro_rules! dispatch {
        ($method:ident, $req_ty:ty) => {{
            let req: $req_ty = match serde_json::from_value(call.params) {
                Ok(req) => req,
                Err(error) => return invalid_tool_params(&call.tool, error),
            };
            match backend.$method(req).await {
                Ok(response) => Json(response).into_response(),
                Err(error) => backend_error_response(error),
            }
        }};
    }
    macro_rules! dispatch_authenticated {
        ($method:ident, $req_ty:ty) => {{
            let mut req: $req_ty = match serde_json::from_value(call.params) {
                Ok(req) => req,
                Err(error) => return invalid_tool_params(&call.tool, error),
            };
            req.set_principal(&verified_principal);
            match backend.$method(req).await {
                Ok(response) => Json(response).into_response(),
                Err(error) => backend_error_response(error),
            }
        }};
    }
    match call.tool.as_str() {
        "conductor_capabilities" => match backend.conductor_capabilities().await {
            Ok(response) => Json(response).into_response(),
            Err(error) => backend_error_response(error),
        },
        "conductor_list_repositories" => dispatch!(conductor_list_repositories, RequestIdOnly),
        "conductor_list_tasks" => dispatch!(conductor_list_tasks, RequestIdOnly),
        "conductor_get_task" => dispatch!(conductor_get_task, GetTaskRequest),
        "conductor_propose_task" => dispatch!(conductor_propose_task, ProposeTaskRequest),
        "conductor_submit_task" => {
            dispatch_authenticated!(conductor_submit_task, SubmitTaskRequest)
        }
        "conductor_schedule_task" => dispatch!(conductor_schedule_task, RequestIdOnly),
        "conductor_get_execution" => dispatch!(conductor_get_execution, GetTaskRequest),
        "conductor_get_evidence" => dispatch!(conductor_get_evidence, GetTaskRequest),
        "conductor_get_approval_requirements" => {
            dispatch_authenticated!(conductor_get_approval_requirements, TaskIdRequest)
        }
        "conductor_approve" => dispatch_authenticated!(conductor_approve, DecisionRequest),
        "conductor_reject" => dispatch_authenticated!(conductor_reject, DecisionRequest),
        "conductor_resume" => dispatch_authenticated!(conductor_resume, ResumeRequest),
        "conductor_cancel_task" => {
            dispatch_authenticated!(conductor_cancel_task, CancelTaskRequest)
        }
        "conductor_global_stop_status" => dispatch!(conductor_global_stop_status, RequestIdOnly),
        "conductor_get_evidence_summary" => {
            dispatch!(conductor_get_evidence_summary, EvidenceSummaryRequest)
        }
        "conductor_get_artifact" => dispatch!(conductor_get_artifact, GetArtifactRequest),
        other => backend_error_response(BackendError::invalid_params_with(
            format!("unknown tool '{other}'"),
            serde_json::json!({"kind": "unknown_tool"}),
        )),
    }
}

#[derive(Deserialize)]
struct ClientLogBody {
    #[serde(default = "default_client_log_level")]
    level: String,
    event: String,
    #[serde(default)]
    request_id: Option<String>,
    message: String,
    #[serde(default)]
    fields: serde_json::Value,
}

fn default_client_log_level() -> String {
    "error".to_string()
}

#[derive(Deserialize, Default)]
struct LogsQuery {
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    level: Option<String>,
    #[serde(default)]
    event: Option<String>,
}

async fn post_client_log(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
    Json(body): Json<ClientLogBody>,
) -> Response {
    let actor = match authenticate(&headers, &state, true).await {
        Ok(Actor::Bearer) => "bearer".to_string(),
        Ok(Actor::Session(principal)) => format!("session:{principal}"),
        Err(err) => return err.into_response(),
    };
    let event = truncate_preview(body.event.trim(), 120);
    if event.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            "client log event must not be empty",
        )
            .into_response();
    }
    let message = truncate_preview(&body.message, 1024);
    let request_id = body
        .request_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(|id| truncate_preview(id, 64));
    let fields = match serde_json::to_string(&body.fields) {
        Ok(raw) if raw.len() <= 4096 => serde_json::json!({
            "actor": actor,
            "client": body.fields,
        }),
        Ok(raw) => serde_json::json!({
            "actor": actor,
            "client_truncated": truncate_preview(&raw, 4096),
        }),
        Err(_) => serde_json::json!({ "actor": actor }),
    };
    state
        .logger
        .log(&body.level, "gui", &event, request_id, message, fields);
    StatusCode::NO_CONTENT.into_response()
}

async fn get_logs(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
    Query(query): Query<LogsQuery>,
) -> Response {
    match authenticate(&headers, &state, false).await {
        Ok(Actor::Bearer) => {}
        Ok(Actor::Session(_)) => {
            return (
                StatusCode::FORBIDDEN,
                "reading server logs requires bearer authentication",
            )
                .into_response();
        }
        Err(err) => return err.into_response(),
    }
    let limit = query.limit.unwrap_or(100).clamp(1, 500);
    let logger = Arc::clone(&state.logger);
    let level = query.level;
    let event = query.event;
    match tokio::task::spawn_blocking(move || {
        let entries = logger.read_recent(limit, level.as_deref(), event.as_deref())?;
        Ok::<_, anyhow::Error>(serde_json::json!({
            "path": logger.path().display().to_string(),
            "level": logger.effective_level().as_str(),
            "count": entries.len(),
            "entries": entries,
        }))
    })
    .await
    {
        Ok(Ok(value)) => Json(value).into_response(),
        Ok(Err(err)) => error_response(err),
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
    }
}

fn resolve_request_id(headers: &HeaderMap) -> String {
    if let Some(raw) = headers
        .get(REQUEST_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        && !raw.is_empty()
        && raw.len() <= 64
        && raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return raw.to_string();
    }
    let hex = uuid::Uuid::new_v4().simple().to_string();
    format!("req-{}", &hex[..12])
}

async fn trace_http_requests(
    State(state): State<Arc<ServeState>>,
    req: Request,
    next: Next,
) -> Response {
    let start = Instant::now();
    let method = req.method().as_str().to_string();
    let path = req.uri().path().to_string();
    let query = req.uri().query().map(sanitize_query).unwrap_or_default();
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let request_id = resolve_request_id(req.headers());
    let is_mutation = matches!(method.as_str(), "POST" | "PUT" | "DELETE" | "PATCH");
    let is_poll_get = method == "GET"
        && (path == "/health"
            || path == "/api/tasks"
            || path == "/api/stop"
            || path.starts_with("/api/tasks/"));

    let mut response = next.run(req).await;
    let status = response.status();
    let duration_ms = start.elapsed().as_millis() as u64;

    if let Ok(val) = HeaderValue::from_str(&request_id) {
        response
            .headers_mut()
            .insert(HeaderName::from_static(REQUEST_ID_HEADER), val);
    }

    if path == "/api/client-log" && status.is_success() {
        return response;
    }

    let level = if status.is_server_error() {
        "error"
    } else if status.is_client_error() {
        "warn"
    } else if is_mutation || (path.starts_with("/api/") && !is_poll_get) {
        "info"
    } else {
        "debug"
    };

    let parsed = crate::logging::LogLevel::parse(level).unwrap_or(crate::logging::LogLevel::Info);
    if !state.logger.is_enabled(parsed) {
        return response;
    }

    let mut error_body: Option<String> = None;
    if status.is_client_error() || status.is_server_error() {
        let (parts, body) = response.into_parts();
        match to_bytes(body, 64 * 1024).await {
            Ok(bytes) => {
                if !bytes.is_empty() {
                    let text = String::from_utf8_lossy(&bytes);
                    error_body = Some(truncate_preview(text.trim(), 512));
                }
                response = Response::from_parts(parts, Body::from(bytes));
            }
            Err(_) => {
                response = Response::from_parts(parts, Body::empty());
            }
        }
    }

    let mut fields = serde_json::json!({
        "method": method,
        "path": path,
        "status": status.as_u16(),
        "duration_ms": duration_ms,
    });
    if let Some(host_val) = host {
        fields["host"] = serde_json::Value::String(host_val);
    }
    if !query.is_empty() {
        fields["query"] = serde_json::Value::String(query);
    }
    if let Some(err_body) = error_body {
        fields["error_body"] = serde_json::Value::String(err_body);
    }
    let summary = format!(
        "{} {} -> {} ({}ms)",
        method,
        path,
        status.as_u16(),
        duration_ms
    );
    state.logger.log(
        level,
        "serve",
        "http.request",
        Some(request_id),
        summary,
        fields,
    );
    response
}

fn build_router(state: Arc<ServeState>, ui_assets: Option<PathBuf>) -> Result<Router> {
    let cors = cors_layer(&state.access, state.port);
    let mut app = Router::new()
        .route("/health", get(health))
        .route("/api/me", get(get_me))
        .route("/api/logs", get(get_logs))
        .route("/api/client-log", post(post_client_log))
        .route("/api/pair/start", post(post_pair_start))
        .route("/api/pair", post(post_pair))
        .route("/api/logout", post(post_logout))
        .route("/api/sessions", get(get_sessions))
        .route("/api/sessions/{id}/revoke", post(post_session_revoke))
        .route("/api/sessions/revoke-all", post(post_sessions_revoke_all))
        .route("/api/tasks", get(get_tasks))
        .route("/api/providers", get(get_providers))
        .route("/api/analytics", get(get_analytics))
        .route("/api/workflow-profiles", get(get_workflow_profiles))
        .route(
            "/api/workflow-profiles/{name}/files",
            get(get_workflow_profile_files),
        )
        .route(
            "/api/workflow-profiles/{name}/repo-status",
            get(get_workflow_profile_repo_status),
        )
        .route(
            "/api/workflow-profiles/{name}/suggest-files",
            post(post_workflow_profile_suggest_files),
        )
        .route(
            "/api/workflow-profiles/{name}/timeouts",
            put(put_workflow_profile_timeouts).post(put_workflow_profile_timeouts),
        )
        .route(
            "/api/workflow-profiles/{name}/re-onboard",
            post(post_workflow_profile_reonboard),
        )
        .route(
            "/api/workflow-profiles/{name}",
            delete(delete_workflow_profile),
        )
        .route(
            "/api/workflow-profiles/{name}/delete",
            post(delete_workflow_profile),
        )
        .route("/api/agent-presets", get(get_agent_presets))
        .route(
            "/api/agent-presets/{name}/routing",
            put(put_agent_preset_routing).post(put_agent_preset_routing),
        )
        .route("/api/agent-presets/{name}", delete(delete_agent_preset))
        .route(
            "/api/agent-presets/{name}/delete",
            post(delete_agent_preset),
        )
        .route("/api/workflows", post(post_workflow))
        .route("/api/onboard/probe", post(post_onboard_probe))
        .route("/api/onboard", post(post_onboard))
        .route("/api/onboard/progress/{id}", get(get_onboard_progress))
        .route("/api/tasks/clear", post(post_clear_tasks))
        .route("/api/tasks/{id}", get(get_task).delete(delete_task))
        .route("/api/tasks/{id}/delete", post(delete_task))
        .route("/api/tasks/{id}/approve", post(post_approve))
        .route("/api/tasks/{id}/run", post(post_run))
        .route("/api/tasks/{id}/cancel", post(post_cancel))
        .route("/api/tasks/{id}/reconcile", post(post_reconcile))
        .route("/api/tasks/{id}/review", post(post_review))
        .route("/api/tasks/{id}/retry", post(post_retry))
        .route(
            "/api/tasks/{id}/integration/prepare",
            post(post_integration_prepare),
        )
        .route("/api/tasks/{id}/integration", post(post_integrate))
        .route("/api/tasks/{id}/depend", post(post_depend))
        .route("/api/leases", get(get_leases))
        .route("/api/leases/acknowledge", post(post_leases_acknowledge))
        .route("/api/stop", get(get_stop).post(post_stop))
        .route("/api/stop/resume", post(post_stop_resume))
        .route("/api/policies", post(post_policy))
        .route("/api/policies/{id}", get(get_policy))
        .route("/api/policies/{id}/revoke", post(post_policy_revoke))
        .route(
            "/api/policies/{id}/hold-my-beer",
            post(post_policy_hold_my_beer),
        )
        .route("/api/edge/tools/call", post(post_edge_tools_call));

    if let Some(assets) = ui_assets {
        let assets = std::fs::canonicalize(assets).context("resolve compiled GUI assets")?;
        ensure!(
            assets.join("index.html").is_file(),
            "compiled GUI index.html missing"
        );
        let static_state = Arc::clone(&state);
        let static_router =
            Router::new()
                .fallback_service(ServeDir::new(assets))
                .layer(middleware::from_fn(move |req: Request, next: Next| {
                    let state = Arc::clone(&static_state);
                    async move {
                        if let Err(err) = check_origin(req.headers(), &state) {
                            return err.into_response();
                        }
                        next.run(req).await
                    }
                }));
        app = app.fallback_service(static_router);
    }

    Ok(app
        .layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            trace_http_requests,
        ))
        .layer(cors)
        .with_state(state))
}

pub async fn run(root: PathBuf, port: u16) -> Result<()> {
    run_with_rotation(root, IpAddr::V4(Ipv4Addr::LOCALHOST), port, None, false).await
}

pub async fn run_with_options(
    root: PathBuf,
    bind: IpAddr,
    port: u16,
    ui_assets: Option<PathBuf>,
) -> Result<()> {
    run_with_rotation(root, bind, port, ui_assets, false).await
}

fn is_valid_serve_token(candidate: &str) -> bool {
    candidate.len() >= 32 && candidate.bytes().all(|b| b.is_ascii_hexdigit())
}

fn write_serve_token(token_path: &std::path::Path, token: &str) -> Result<()> {
    let tmp_path = token_path.with_extension("tmp");
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp_path)
            .context("create temporary serve-token")?;
        f.write_all(token.as_bytes())
            .context("write temporary serve-token")?;
        f.sync_all().context("sync temporary serve-token")?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&tmp_path, token).context("write temporary serve-token")?;
    }
    if std::fs::rename(&tmp_path, token_path).is_err() {
        let _ = std::fs::remove_file(&tmp_path);
        std::fs::write(token_path, token).context("write serve-token")?;
    }
    Ok(())
}

/// Loads an existing valid bearer token from `<state>/serve-token` so restarts and rebuilds
/// keep local browser/desktop clients connected, or creates a fresh 256-bit hex token when
/// missing, corrupt, or explicitly rotated (`--rotate-token`).
pub(crate) fn load_or_create_serve_token(
    root: &std::path::Path,
    rotate: bool,
) -> Result<(String, bool)> {
    std::fs::create_dir_all(root).context("create state root")?;
    let token_path = root.join("serve-token");
    if !rotate && let Ok(existing) = std::fs::read_to_string(&token_path) {
        let trimmed = existing.trim();
        if is_valid_serve_token(trimmed) {
            return Ok((trimmed.to_string(), true));
        }
    }
    let token = random_token();
    write_serve_token(&token_path, &token)?;
    Ok((token, false))
}

pub async fn run_with_rotation(
    root: PathBuf,
    bind: IpAddr,
    port: u16,
    ui_assets: Option<PathBuf>,
    rotate_token: bool,
) -> Result<()> {
    let access = AccessPolicy::load(&root)?;
    let addr = SocketAddr::new(bind, port);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind {addr}"))?;

    if !bind.is_loopback() && access.external_hosts().is_empty() {
        eprintln!(
            "dev-conductor serve warning: bound to non-loopback {addr} with no external hosts in {}/http-access.json; remote Host headers will be rejected (403) until configured.",
            root.display()
        );
    }

    let (token, token_reused) = load_or_create_serve_token(&root, rotate_token)?;
    let token_path = root.join("serve-token");
    let has_ui = ui_assets.is_some();
    let logger = Arc::new(JsonLogger::new(&root));
    let state = Arc::new(ServeState {
        root: root.clone(),
        port,
        token,
        sessions: Mutex::new(SessionStore::default()),
        session_db: Arc::new(Mutex::new(
            Sessions::open(&root).context("open paired session store")?,
        )),
        access,
        logger: Arc::clone(&logger),
        onboard_progress: Arc::new(Mutex::new(std::collections::HashMap::new())),
    });
    let app = build_router(Arc::clone(&state), ui_assets)?;
    logger.info(
        "serve",
        "server.start",
        None,
        format!("clatterforge serve listening on http://{addr}"),
        serde_json::json!({
            "addr": addr.to_string(),
            "port": port,
            "state_root": root.display().to_string(),
            "log_file": logger.path().display().to_string(),
            "log_level": logger.effective_level().as_str(),
            "ui_assets_enabled": has_ui,
            "token_reused": token_reused,
            "allowed_remote_hosts": state.access.external_hosts().iter().collect::<Vec<_>>(),
        }),
    );
    eprintln!(
        "dev-conductor serve: http://{addr}  (token: {}, logs: {}{})",
        token_path.display(),
        logger.path().display(),
        if has_ui {
            ", unified web UI enabled"
        } else {
            ""
        }
    );
    if !state.access.external_hosts().is_empty() {
        let hosts: Vec<&str> = state
            .access
            .external_hosts()
            .iter()
            .map(String::as_str)
            .collect();
        eprintln!(
            "dev-conductor serve allowed remote hosts: {}",
            hosts.join(", ")
        );
    }
    axum::serve(listener, app).await.context("serve")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn spawn_test_server(
        root: PathBuf,
        access: AccessPolicy,
        ui_assets: Option<PathBuf>,
    ) -> (u16, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let logger = Arc::new(JsonLogger::with_options(&root, false));
        let state = Arc::new(ServeState {
            root: root.clone(),
            port,
            token: "test-bearer-token".into(),
            sessions: Mutex::new(SessionStore::default()),
            session_db: Arc::new(Mutex::new(Sessions::open(&root).unwrap())),
            access,
            logger,
            onboard_progress: Arc::new(Mutex::new(std::collections::HashMap::new())),
        });
        let app = build_router(state, ui_assets).unwrap();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (port, handle)
    }

    #[tokio::test]
    async fn session_lookup_waiting_on_a_locked_database_does_not_stall_other_requests() {
        // Issue #7: session verification ran SQLite (including the sliding-expiry write) on
        // the async worker. On this single-threaded test runtime that blocked every request
        // until the lock cleared. Now it runs on the blocking pool.
        let dir = tempfile::tempdir().unwrap();
        let (port, handle) = spawn_test_server(
            dir.path().to_path_buf(),
            AccessPolicy::default_local(),
            None,
        )
        .await;
        let base = format!("http://127.0.0.1:{port}");
        let client = reqwest::Client::new();
        let code = client
            .post(format!("{base}/api/pair/start"))
            .header("Authorization", "Bearer test-bearer-token")
            .send()
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap()["code"]
            .as_str()
            .unwrap()
            .to_string();
        let paired = client
            .post(format!("{base}/api/pair"))
            .json(&serde_json::json!({"code": code, "principal": "Phone"}))
            .send()
            .await
            .unwrap();
        assert_eq!(paired.status(), reqwest::StatusCode::OK);
        let session_cookie = paired
            .headers()
            .get_all(reqwest::header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find(|v| v.starts_with(SESSION_COOKIE))
            .and_then(|v| v.split(';').next())
            .unwrap()
            .to_string();

        // Age the session so verification must write its sliding renewal, then hold the
        // write lock from another connection so that write has to wait.
        let lock = rusqlite::Connection::open(dir.path().join("state.sqlite")).unwrap();
        lock.execute("UPDATE paired_sessions SET last_seen_at = 0", [])
            .unwrap();
        lock.execute_batch("BEGIN EXCLUSIVE").unwrap();

        let me = tokio::spawn({
            let client = client.clone();
            let url = format!("{base}/api/me");
            async move {
                client
                    .get(url)
                    .header(reqwest::header::COOKIE, session_cookie)
                    .send()
                    .await
                    .unwrap()
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let health = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            client.get(format!("{base}/health")).send(),
        )
        .await
        .expect("/health must answer while a session lookup waits on the database lock")
        .unwrap();
        assert_eq!(health.status(), reqwest::StatusCode::OK);
        assert!(
            !me.is_finished(),
            "the session lookup should still be waiting on the lock"
        );

        lock.execute_batch("ROLLBACK").unwrap();
        let me = me.await.unwrap();
        assert_eq!(me.status(), reqwest::StatusCode::OK);
        let body: serde_json::Value = me.json().await.unwrap();
        assert_eq!(body["auth"], "session");
        assert_eq!(body["principal"], "Phone");
        handle.abort();
    }

    #[tokio::test]
    async fn pair_is_case_insensitive_and_locks_out_after_five_failures() {
        let dir = tempfile::tempdir().unwrap();
        let (port, handle) = spawn_test_server(
            dir.path().to_path_buf(),
            AccessPolicy::default_local(),
            None,
        )
        .await;
        let base = format!("http://127.0.0.1:{port}");
        let client = reqwest::Client::new();

        // Start pairing with bearer token
        let start_resp = client
            .post(format!("{base}/api/pair/start"))
            .header("Authorization", "Bearer test-bearer-token")
            .send()
            .await
            .unwrap();
        assert_eq!(start_resp.status(), reqwest::StatusCode::OK);
        let body: serde_json::Value = start_resp.json().await.unwrap();
        let code = body["code"].as_str().unwrap().to_string();

        // 5 wrong attempts should burn the pending challenge
        for _ in 0..5 {
            let bad = client
                .post(format!("{base}/api/pair"))
                .json(&serde_json::json!({"code": "0000-0000", "principal": "Mike"}))
                .send()
                .await
                .unwrap();
            assert_eq!(bad.status(), reqwest::StatusCode::UNAUTHORIZED);
        }

        // Now even the real code must fail because the challenge was burned
        let burned = client
            .post(format!("{base}/api/pair"))
            .json(&serde_json::json!({"code": code, "principal": "Mike"}))
            .send()
            .await
            .unwrap();
        assert_eq!(burned.status(), reqwest::StatusCode::UNAUTHORIZED);

        // Fresh code succeeds in lowercase and /api/me reports the session principal
        let start2 = client
            .post(format!("{base}/api/pair/start"))
            .header("Authorization", "Bearer test-bearer-token")
            .send()
            .await
            .unwrap();
        let body2: serde_json::Value = start2.json().await.unwrap();
        let code2 = body2["code"].as_str().unwrap().to_ascii_lowercase();

        let paired = client
            .post(format!("{base}/api/pair"))
            .json(&serde_json::json!({"code": format!("  {code2}  "), "principal": "Mike"}))
            .send()
            .await
            .unwrap();
        assert_eq!(paired.status(), reqwest::StatusCode::OK);
        let cookie_header = paired
            .headers()
            .get_all(reqwest::header::SET_COOKIE)
            .iter()
            .map(|v| v.to_str().unwrap().split(';').next().unwrap().to_string())
            .collect::<Vec<_>>()
            .join("; ");

        let me_resp = client
            .get(format!("{base}/api/me"))
            .header(reqwest::header::COOKIE, &cookie_header)
            .send()
            .await
            .unwrap();
        assert_eq!(me_resp.status(), reqwest::StatusCode::OK);
        let me: serde_json::Value = me_resp.json().await.unwrap();
        assert_eq!(me["auth"], "session");
        assert_eq!(me["principal"], "Mike");

        // Simulate a full server restart: abort server 1, spawn server 2 with same state root
        handle.abort();
        let (port2, handle2) = spawn_test_server(
            dir.path().to_path_buf(),
            AccessPolicy::default_local(),
            None,
        )
        .await;
        let base2 = format!("http://127.0.0.1:{port2}");

        // The paired phone cookie must STILL authenticate after server restart!
        let me_after_restart = client
            .get(format!("{base2}/api/me"))
            .header(reqwest::header::COOKIE, &cookie_header)
            .send()
            .await
            .unwrap();
        assert_eq!(me_after_restart.status(), reqwest::StatusCode::OK);
        let me2: serde_json::Value = me_after_restart.json().await.unwrap();
        assert_eq!(me2["principal"], "Mike");

        // Bearer can list active paired sessions and revoke by id
        let list_resp = client
            .get(format!("{base2}/api/sessions"))
            .header("Authorization", "Bearer test-bearer-token")
            .send()
            .await
            .unwrap();
        assert_eq!(list_resp.status(), reqwest::StatusCode::OK);
        let list: Vec<serde_json::Value> = list_resp.json().await.unwrap();
        assert_eq!(list.len(), 1);
        let session_id = list[0]["id"].as_str().unwrap().to_string();

        let rev_resp = client
            .post(format!("{base2}/api/sessions/{session_id}/revoke"))
            .header("Authorization", "Bearer test-bearer-token")
            .send()
            .await
            .unwrap();
        assert_eq!(rev_resp.status(), reqwest::StatusCode::OK);

        // Revoked session is immediately rejected
        let after_revoke = client
            .get(format!("{base2}/api/me"))
            .header(reqwest::header::COOKIE, &cookie_header)
            .send()
            .await
            .unwrap();
        assert_eq!(after_revoke.status(), reqwest::StatusCode::UNAUTHORIZED);

        handle2.abort();
    }

    #[tokio::test]
    async fn unified_ui_assets_enforce_host_allowlist_and_prevent_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let ui_dir = dir.path().join("ui");
        std::fs::create_dir_all(&ui_dir).unwrap();
        std::fs::write(ui_dir.join("index.html"), "<html>clatterforge-ui</html>").unwrap();
        std::fs::write(dir.path().join("secret.txt"), "private").unwrap();
        std::fs::write(
            dir.path().join("http-access.json"),
            r#"{"trusted_http_hosts":["10.0.0.50:8787","devbox.example-tailnet.ts.net:8788"],"https_origins":[]}"#,
        )
        .unwrap();
        let access = AccessPolicy::load(dir.path()).unwrap();
        let (port, handle) =
            spawn_test_server(dir.path().to_path_buf(), access, Some(ui_dir)).await;
        let base = format!("http://127.0.0.1:{port}");
        let client = reqwest::Client::new();

        // Allowed LAN host gets index.html
        let ok = client
            .get(format!("{base}/"))
            .header(reqwest::header::HOST, "10.0.0.50:8787")
            .send()
            .await
            .unwrap();
        assert_eq!(ok.status(), reqwest::StatusCode::OK);
        assert!(ok.text().await.unwrap().contains("clatterforge-ui"));

        // Same-origin module/preload asset requests on the bound loopback port are allowed
        let same_origin_asset = client
            .get(format!("{base}/index.html"))
            .header(reqwest::header::ORIGIN, &base)
            .send()
            .await
            .unwrap();
        assert_eq!(same_origin_asset.status(), reqwest::StatusCode::OK);

        // Foreign port loopback Origin stays rejected
        let wrong_origin = client
            .get(format!("{base}/index.html"))
            .header(reqwest::header::ORIGIN, "http://127.0.0.1:9999")
            .send()
            .await
            .unwrap();
        assert_eq!(wrong_origin.status(), reqwest::StatusCode::FORBIDDEN);

        // Unknown host is rejected 403
        let bad_host = client
            .get(format!("{base}/"))
            .header(reqwest::header::HOST, "evil.invalid:8787")
            .send()
            .await
            .unwrap();
        assert_eq!(bad_host.status(), reqwest::StatusCode::FORBIDDEN);

        // Traversal cannot read sibling secret
        let traversal = client
            .get(format!("{base}/%2e%2e/secret.txt"))
            .header(reqwest::header::HOST, "10.0.0.50:8787")
            .send()
            .await
            .unwrap();
        assert_ne!(traversal.status(), reqwest::StatusCode::OK);

        // Unauthenticated API call is still 401
        let unauth_api = client
            .get(format!("{base}/api/me"))
            .header(reqwest::header::HOST, "10.0.0.50:8787")
            .send()
            .await
            .unwrap();
        assert_eq!(unauth_api.status(), reqwest::StatusCode::UNAUTHORIZED);

        handle.abort();
    }

    #[tokio::test]
    async fn http_tracing_writes_jsonl_and_roundtrips_client_logs() {
        let dir = tempfile::tempdir().unwrap();
        let (port, handle) = spawn_test_server(
            dir.path().to_path_buf(),
            AccessPolicy::default_local(),
            None,
        )
        .await;
        let base = format!("http://127.0.0.1:{port}");
        let client = reqwest::Client::new();

        let health_resp = client.get(format!("{base}/health")).send().await.unwrap();
        assert_eq!(health_resp.status(), reqwest::StatusCode::OK);
        let req_id = health_resp
            .headers()
            .get(REQUEST_ID_HEADER)
            .and_then(|v| v.to_str().ok())
            .expect("X-Request-Id response header")
            .to_string();
        assert!(req_id.starts_with("req-"));

        let post_log = client
            .post(format!("{base}/api/client-log"))
            .header(reqwest::header::AUTHORIZATION, "Bearer test-bearer-token")
            .json(&serde_json::json!({
                "level": "error",
                "event": "client.decode_error",
                "request_id": req_id,
                "message": "invalid type: map, expected a string",
                "fields": {"path": "/api/onboard", "method": "POST"}
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(post_log.status(), reqwest::StatusCode::NO_CONTENT);

        let logs_resp = client
            .get(format!("{base}/api/logs?level=error&event=decode"))
            .header(reqwest::header::AUTHORIZATION, "Bearer test-bearer-token")
            .send()
            .await
            .unwrap();
        assert_eq!(logs_resp.status(), reqwest::StatusCode::OK);
        let payload: serde_json::Value = logs_resp.json().await.unwrap();
        let entries = payload["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["component"], "gui");
        assert_eq!(entries[0]["event"], "client.decode_error");
        assert_eq!(entries[0]["request_id"], req_id);

        assert!(crate::logging::log_file_path(dir.path()).is_file());
        handle.abort();
    }

    #[test]
    fn workflow_profile_summary_emits_flat_repository_string() {
        let profile = crate::workflow::Profile {
            name: "sample-app".into(),
            repository: crate::repository::Repository {
                checkout: PathBuf::from("C:/fixtures/sample-app"),
                common_dir: PathBuf::from("C:/fixtures/sample-app/.git"),
                identity: "test-repo".into(),
                remotes: Vec::new(),
                base: "abc1234".into(),
                branch: Some("main".into()),
                platform: "windows".into(),
            },
            inference: crate::inference::config::Config {
                schema_version: 1,
                default_target: "router".into(),
                hardware_id: "test-host".into(),
                admission_db: PathBuf::from("admission.sqlite"),
                max_active_per_resource: 1,
                registry_max_age_ms: 86_400_000,
                fallback_before_dispatch: false,
                targets: vec![crate::inference::config::Target {
                    id: "router".into(),
                    provider: crate::inference::types::ProviderKind::Ollama,
                    endpoint: "http://127.0.0.1:11434".into(),
                    endpoint_access: crate::inference::config::EndpointAccess::Loopback,
                    model: "builtin:router-v1".into(),
                    expected_artifact_digest: None,
                    context_limit: 8192,
                    output_limit: 2048,
                    capabilities: crate::inference::types::Capabilities {
                        streaming: false,
                        tools: false,
                        required_tool_choice: false,
                        parallel_tools: false,
                        structured_output: true,
                        media: false,
                        reasoning_control: false,
                    },
                    capability_source: "test".into(),
                    priority: 1,
                    resource_id: "router".into(),
                    extensions: None,
                    api_pricing: None,
                }],
            },
            verifier: crate::codex::PinnedCommand {
                executable: crate::codex::Executable {
                    path: "cargo".into(),
                    hash: "0".repeat(64),
                },
                args: vec!["test".into()],
            },
            workers: Vec::new(),
            max_steps: 4,
            decision_timeout_ms: 120000,
            created_by: "test".into(),
            require_clean_checkout: false,
        };

        let flat = workflow_profile_summary_with_index(&profile, None);
        assert_eq!(flat["name"], "sample-app");
        assert_eq!(flat["repository"], "C:/fixtures/sample-app");
        assert_eq!(flat["model"], "builtin:router-v1");
        assert_eq!(flat["max_steps"], 4);
        assert_eq!(flat["decision_timeout_ms"], 120000);
        assert_eq!(flat["low_spec"], true);
    }

    #[tokio::test]
    async fn api_workflow_profile_timeouts_and_dispatch_override_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        // Production canonicalizes every root at entry; CI's Windows TEMP is an 8.3 short
        // path (RUNNER~1), so use the canonical form here too or path identity checks fail.
        let dir_root = crate::repository::canonical(tmp.path()).unwrap();
        let repo_dir = dir_root.as_path().join("repo");
        std::fs::create_dir_all(&repo_dir).unwrap();
        let init = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo_dir)
            .args(["init", "-q"])
            .status()
            .unwrap();
        assert!(init.success());
        for args in [
            ["config", "user.email", "test@example.com"],
            ["config", "user.name", "Test"],
        ] {
            assert!(
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(&repo_dir)
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        std::fs::write(repo_dir.join("README.md"), "hello\n").unwrap();
        std::fs::write(repo_dir.join("app.txt"), "initial\n").unwrap();
        assert!(
            std::process::Command::new("git")
                .arg("-C")
                .arg(&repo_dir)
                .args(["add", "."])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            std::process::Command::new("git")
                .arg("-C")
                .arg(&repo_dir)
                .args(["commit", "-q", "-m", "init"])
                .status()
                .unwrap()
                .success()
        );

        let state_dir = dir_root.as_path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        let exe = std::env::current_exe().unwrap();
        let backend = crate::agents::AgentBackend::Local(
            crate::agents::LocalAgentPolicy::inspect(
                &exe,
                "http://127.0.0.1:11434".into(),
                "gemma4:26b".into(),
                None,
            )
            .unwrap(),
        );
        crate::agent_preset::AgentPresets::open(&state_dir)
            .unwrap()
            .create(
                "worker-p",
                repo_dir.clone(),
                backend,
                vec!["app.txt".into()],
                vec!["README.md".into()],
                exe.clone(),
                vec![],
                5000,
                5000,
                None,
                "test",
            )
            .unwrap();
        crate::workflow::Workflows::open(&state_dir)
            .unwrap()
            .create_profile(
                "proj".into(),
                crate::workflow::builtin_router_config(&state_dir),
                vec![("worker-p".into(), "edit app".into())],
                4,
                60_000,
                "test".into(),
            )
            .unwrap();

        let (port, handle) =
            spawn_test_server(state_dir.clone(), AccessPolicy::default_local(), None).await;
        let base = format!("http://127.0.0.1:{port}");
        let client = reqwest::Client::new();

        // Update saved profile timeouts on the fly.
        let put_resp = client
            .post(format!("{base}/api/workflow-profiles/proj/timeouts"))
            .header(reqwest::header::AUTHORIZATION, "Bearer test-bearer-token")
            .json(&serde_json::json!({
                "generation_timeout_ms": 600_000,
                "generation_idle_timeout_ms": 360_000,
                "verification_timeout_ms": 180_000,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(put_resp.status(), reqwest::StatusCode::OK);
        let updated: serde_json::Value = put_resp.json().await.unwrap();
        assert_eq!(updated["generation_timeout_ms"], 600_000);
        assert_eq!(updated["generation_idle_timeout_ms"], 360_000);
        assert_eq!(updated["verification_timeout_ms"], 180_000);
        assert_eq!(updated["scope"], serde_json::json!(["app.txt"]));

        // Tracked editable files endpoint lists editable repo files while excluding protected verifier files.
        let files_resp = client
            .get(format!("{base}/api/workflow-profiles/proj/files"))
            .header(reqwest::header::AUTHORIZATION, "Bearer test-bearer-token")
            .send()
            .await
            .unwrap();
        assert_eq!(files_resp.status(), reqwest::StatusCode::OK);
        let files_list: Vec<String> = files_resp.json().await.unwrap();
        assert_eq!(files_list, vec!["app.txt".to_string()]);

        // Dispatch a workflow with ad-hoc per-run timeout overrides.
        let wf_resp = client
            .post(format!("{base}/api/workflows"))
            .header(reqwest::header::AUTHORIZATION, "Bearer test-bearer-token")
            .json(&serde_json::json!({
                "profile": "proj",
                "objective": "Update app.txt",
                "acceptance": "Pass checks",
                "generation_timeout_ms": 900_000,
                "generation_idle_timeout_ms": 600_000,
                "verification_timeout_ms": 300_000,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(wf_resp.status(), reqwest::StatusCode::OK);
        let wf_detail: serde_json::Value = wf_resp.json().await.unwrap();
        let wf_id = wf_detail["id"].as_str().unwrap();
        let stored_wf = crate::workflow::Workflows::open(&state_dir)
            .unwrap()
            .get(wf_id)
            .unwrap();
        assert_eq!(
            stored_wf.assignment.profile.workers[0]
                .preset
                .generation_timeout_ms,
            900_000
        );
        assert_eq!(
            stored_wf.assignment.profile.workers[0]
                .preset
                .generation_idle_timeout_ms,
            Some(600_000)
        );
        assert_eq!(
            stored_wf.assignment.profile.workers[0]
                .preset
                .verification_timeout_ms,
            300_000
        );

        // Dispatch a workflow with auto_scope enabled and verify symbol match merges with profile defaults.
        std::fs::create_dir_all(repo_dir.join("crates/core-lib/src")).unwrap();
        std::fs::write(
            repo_dir.join("crates/core-lib/src/flight_recorder.rs"),
            "pub fn query_session_flight_log() {}\n",
        )
        .unwrap();
        for args in [vec!["add", "."], vec!["commit", "-q", "-m", "add recorder"]] {
            assert!(
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(&repo_dir)
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        }

        let suggest_resp = client
            .post(format!("{base}/api/workflow-profiles/proj/suggest-files"))
            .header(reqwest::header::AUTHORIZATION, "Bearer test-bearer-token")
            .json(&serde_json::json!({
                "objective": "Fix bug in query_session_flight_log",
                "acceptance": "Pass checks"
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(suggest_resp.status(), reqwest::StatusCode::OK);
        let suggested: crate::repo_index::SuggestFilesResponse = suggest_resp.json().await.unwrap();
        assert_eq!(
            suggested.matched[0].path,
            "crates/core-lib/src/flight_recorder.rs"
        );
        assert_eq!(
            suggested.merged_scope,
            vec![
                "crates/core-lib/src/flight_recorder.rs".to_string(),
                "app.txt".to_string()
            ]
        );

        let wf_auto_resp = client
            .post(format!("{base}/api/workflows"))
            .header(reqwest::header::AUTHORIZATION, "Bearer test-bearer-token")
            .json(&serde_json::json!({
                "profile": "proj",
                "objective": "Fix bug in query_session_flight_log",
                "acceptance": "Pass checks",
                "auto_scope": true,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(wf_auto_resp.status(), reqwest::StatusCode::OK);
        let wf_auto_detail: serde_json::Value = wf_auto_resp.json().await.unwrap();
        let stored_auto = crate::workflow::Workflows::open(&state_dir)
            .unwrap()
            .get(wf_auto_detail["id"].as_str().unwrap())
            .unwrap();
        assert_eq!(
            stored_auto.assignment.profile.scope(),
            vec![
                "app.txt".to_string(),
                "crates/core-lib/src/flight_recorder.rs".to_string()
            ]
        );

        // Progress endpoint returns 404 for unknown job ID and returns completed progress for onboard.
        let missing_prog = client
            .get(format!("{base}/api/onboard/progress/does-not-exist"))
            .header(reqwest::header::AUTHORIZATION, "Bearer test-bearer-token")
            .send()
            .await
            .unwrap();
        assert_eq!(missing_prog.status(), reqwest::StatusCode::NOT_FOUND);

        let onboard_resp = client
            .post(format!("{base}/api/onboard?progress_id=job-123"))
            .header(reqwest::header::AUTHORIZATION, "Bearer test-bearer-token")
            .json(&serde_json::json!({
                "profile_name": "proj-onboarded",
                "repository_path": repo_dir,
                "model": "builtin:router-v1",
                "model_digest": null,
                "endpoint": null,
                "verifier_executable": exe,
                "verifier_args": [],
                "verification_files": ["README.md"],
                "allowed_files": ["app.txt"],
                "workers": ["codex"],
                "low_spec": false,
                "by": "test"
            }))
            .send()
            .await
            .unwrap();
        // Either succeeds (if codex exists on host) or cleanly marks failed progress on missing CLI.
        let prog_resp = client
            .get(format!("{base}/api/onboard/progress/job-123"))
            .header(reqwest::header::AUTHORIZATION, "Bearer test-bearer-token")
            .send()
            .await
            .unwrap();
        assert_eq!(prog_resp.status(), reqwest::StatusCode::OK);
        let prog: crate::onboard::OnboardProgress = prog_resp.json().await.unwrap();
        assert_eq!(prog.job_id, "job-123");
        assert!(prog.finished);
        assert_eq!(prog.percent, 100);
        if onboard_resp.status() == reqwest::StatusCode::OK {
            let created_json: serde_json::Value = onboard_resp.json().await.unwrap();
            assert!(created_json["indexed_files"].as_u64().unwrap() >= 2);
        }

        handle.abort();
    }

    #[test]
    fn serve_token_persists_across_restarts_and_rotates_when_requested() {
        let dir = tempfile::tempdir().unwrap();
        let (first, reused_first) = load_or_create_serve_token(dir.path(), false).unwrap();
        assert!(!reused_first);
        assert!(is_valid_serve_token(&first));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("serve-token")).unwrap(),
            first
        );

        let (second, reused_second) = load_or_create_serve_token(dir.path(), false).unwrap();
        assert!(reused_second);
        assert_eq!(second, first);

        let (rotated, reused_rotated) = load_or_create_serve_token(dir.path(), true).unwrap();
        assert!(!reused_rotated);
        assert!(is_valid_serve_token(&rotated));
        assert_ne!(rotated, first);

        // Empty or corrupt token file regenerates cleanly
        std::fs::write(dir.path().join("serve-token"), "not-valid-token\n").unwrap();
        let (repaired, reused_repaired) = load_or_create_serve_token(dir.path(), false).unwrap();
        assert!(!reused_repaired);
        assert!(is_valid_serve_token(&repaired));
        assert_ne!(repaired, rotated);
    }

    #[tokio::test]
    async fn api_tasks_clear_and_delete_endpoints_enforce_auth_and_remove_cancelled_tasks() {
        let dir = tempfile::tempdir().unwrap();
        let repo_dir = dir.path().join("repo");
        std::fs::create_dir_all(&repo_dir).unwrap();
        let init = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo_dir)
            .args(["init", "-q"])
            .status()
            .unwrap();
        assert!(init.success());
        let cfg1 = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo_dir)
            .args(["config", "user.email", "test@example.com"])
            .status()
            .unwrap();
        assert!(cfg1.success());
        let cfg2 = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo_dir)
            .args(["config", "user.name", "Test"])
            .status()
            .unwrap();
        assert!(cfg2.success());
        std::fs::write(repo_dir.join("README.md"), "hello\n").unwrap();
        let add = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo_dir)
            .args(["add", "README.md"])
            .status()
            .unwrap();
        assert!(add.success());
        let commit = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo_dir)
            .args(["commit", "-q", "-m", "init"])
            .status()
            .unwrap();
        assert!(commit.success());

        let state_dir = dir.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        let exe = std::env::current_exe().unwrap();
        let mut agents = crate::agents::Agents::open(&state_dir).unwrap();
        let make_spec = |obj: &str, file: &str| crate::agents::AgentSpec {
            agentlet: None,
            repository: repo_dir.clone(),
            objective: obj.into(),
            acceptance: "done".into(),
            allowed_files: vec![file.into()],
            verification_files: vec!["README.md".into()],
            verify_executable: exe.clone(),
            verify_args: vec![],
            target: "local-model".into(),
            model: "gemma4:26b".into(),
            generation_timeout_ms: 5000,
            generation_idle_timeout_ms: None,
            verification_timeout_ms: 5000,
            soft_timeouts: false,
            effort: None,
        };
        let backend = crate::agents::AgentBackend::Local(
            crate::agents::LocalAgentPolicy::inspect(
                &exe,
                "http://127.0.0.1:11434".into(),
                "gemma4:26b".into(),
                None,
            )
            .unwrap(),
        );
        let t1 = agents
            .create(make_spec("first cancelled task", "a.txt"), backend.clone())
            .unwrap();
        let t2 = agents
            .create(make_spec("second cancelled task", "b.txt"), backend.clone())
            .unwrap();
        let t3 = agents
            .create(make_spec("pending task kept", "c.txt"), backend)
            .unwrap();
        drop(agents);

        let mut service = TaskService::open(&state_dir).unwrap();
        service.cancel(&t1.id, t1.revision, Some("op")).unwrap();
        service.cancel(&t2.id, t2.revision, Some("op")).unwrap();
        drop(service);

        let (port, handle) =
            spawn_test_server(state_dir.clone(), AccessPolicy::default_local(), None).await;
        let base = format!("http://127.0.0.1:{port}");
        let client = reqwest::Client::new();

        // Unauthenticated clear is rejected.
        let unauth = client
            .post(format!("{base}/api/tasks/clear"))
            .json(&serde_json::json!({"scope": "failed"}))
            .send()
            .await
            .unwrap();
        assert_eq!(unauth.status(), reqwest::StatusCode::UNAUTHORIZED);

        // Delete one cancelled task via DELETE /api/tasks/{id}.
        let del_resp = client
            .delete(format!("{base}/api/tasks/{}", t1.id))
            .header(reqwest::header::AUTHORIZATION, "Bearer test-bearer-token")
            .send()
            .await
            .unwrap();
        assert_eq!(del_resp.status(), reqwest::StatusCode::OK);
        let del_json: serde_json::Value = del_resp.json().await.unwrap();
        assert_eq!(del_json["deleted_id"], t1.id);

        // Clear remaining failed/cancelled tasks via POST /api/tasks/clear.
        let clear_resp = client
            .post(format!("{base}/api/tasks/clear"))
            .header(reqwest::header::AUTHORIZATION, "Bearer test-bearer-token")
            .json(&serde_json::json!({"scope": "failed"}))
            .send()
            .await
            .unwrap();
        assert_eq!(clear_resp.status(), reqwest::StatusCode::OK);
        let clear_json: crate::service::ClearOutcome = clear_resp.json().await.unwrap();
        assert_eq!(clear_json.deleted_ids, vec![t2.id.clone()]);
        assert!(clear_json.skipped.is_empty());

        // Only t3 (awaiting_approval) remains on the board.
        let board_resp = client
            .get(format!("{base}/api/tasks"))
            .header(reqwest::header::AUTHORIZATION, "Bearer test-bearer-token")
            .send()
            .await
            .unwrap();
        assert_eq!(board_resp.status(), reqwest::StatusCode::OK);
        let rows: Vec<serde_json::Value> = board_resp.json().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["id"], t3.id);

        // Cancel t3, then retry it via POST /api/tasks/{id}/retry.
        let detail_resp = client
            .get(format!("{base}/api/tasks/{}", t3.id))
            .header(reqwest::header::AUTHORIZATION, "Bearer test-bearer-token")
            .send()
            .await
            .unwrap();
        assert_eq!(detail_resp.status(), reqwest::StatusCode::OK);
        let detail_json: serde_json::Value = detail_resp.json().await.unwrap();
        let rev3 = detail_json["revision"].as_u64().unwrap();
        let cancel_resp = client
            .post(format!("{base}/api/tasks/{}/cancel", t3.id))
            .header(reqwest::header::AUTHORIZATION, "Bearer test-bearer-token")
            .json(&serde_json::json!({"revision": rev3, "by": "op"}))
            .send()
            .await
            .unwrap();
        assert_eq!(cancel_resp.status(), reqwest::StatusCode::OK);
        let cancelled_json: serde_json::Value = cancel_resp.json().await.unwrap();
        assert_eq!(cancelled_json["status"], "cancelled");

        let retry_resp = client
            .post(format!("{base}/api/tasks/{}/retry", t3.id))
            .header(reqwest::header::AUTHORIZATION, "Bearer test-bearer-token")
            .json(&serde_json::json!({
                "revision": cancelled_json["revision"].as_u64().unwrap(),
                "by": "op",
                "reset_workspace": false
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(retry_resp.status(), reqwest::StatusCode::OK);
        let retried_json: serde_json::Value = retry_resp.json().await.unwrap();
        assert_eq!(retried_json["status"], "approved");
        assert_eq!(retried_json["approved_by"], "op");

        let analytics_resp = client
            .get(format!("{base}/api/analytics"))
            .header(reqwest::header::AUTHORIZATION, "Bearer test-bearer-token")
            .send()
            .await
            .unwrap();
        assert_eq!(analytics_resp.status(), reqwest::StatusCode::OK);
        let analytics_json: crate::routing_metrics::AnalyticsReport =
            analytics_resp.json().await.unwrap();
        assert_eq!(analytics_json.total_attempts, 0);

        handle.abort();
    }
}
