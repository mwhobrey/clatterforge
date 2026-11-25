//! `clatterforge-edge`: the M12.0 Phase 1 lightweight Linux edge adapter. Launched over
//! MCP stdio exactly like `clatterforge mcp-serve`, presenting the identical
//! `conductor_*` tool contract (`clatterforge-protocol`) — but every call is forwarded over an
//! authenticated HTTP request to the main Windows Clatterforge's `/api/edge/tools/call`
//! route (`src/serve.rs`) instead of touching a local task database. This binary owns no
//! task state, execution state, repository lease, approval grant, or coding-agent process
//! — see `docs/m12/M12_0_ARCHITECTURE.md` for the full "edge must not own" list.
//!
//! Depends on nothing but `clatterforge-protocol`, `reqwest`, `rmcp` (stdio server only) and a
//! narrow `tokio` feature set — no `rusqlite`, no `axum`, no Windows FFI, no execution
//! engine reaches this binary's dependency graph at all.

use anyhow::{Context, Result, bail};
use clap::Parser;
use clatterforge_protocol::{
    ApprovalRequirementsResponse, BackendError, CancelScheduleRequest, CancelTaskRequest,
    CapabilitiesResponse, ConductorBackend, DecisionRequest, DecisionResponse, ErrorKind,
    EvidenceSummaryRequest, EvidenceSummaryResponse, GetArtifactRequest, GetArtifactResponse,
    GetTaskRequest, ListRepositoriesResponse, ListSchedulesResponse, ListTasksResponse,
    ProposeTaskRequest, ProposeTaskResponse, RequestIdOnly, ResumeRequest, ResumeResponse,
    ScheduleTaskRequest, ScheduleTaskResponse, StopStatusResponse, SubmitTaskRequest,
    SubmitTaskResponse, TaskIdRequest, TaskResponse,
};
use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    handler::server::wrapper::{Json, Parameters},
    model::{Implementation, ServerCapabilities, ServerConfig},
    tool, tool_handler, tool_router,
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::json;
use std::{path::PathBuf, time::Duration};

#[derive(Parser, Debug)]
#[command(
    name = "clatterforge-edge",
    about = "Clatterforge Spark: lightweight MCP adapter forwarding conductor_* tool calls to the main forge"
)]
struct Cli {
    /// Base URL of the main Clatterforge's HTTP service, e.g.
    /// https://conductor.example.invalid (configure your deployment's actual endpoint).
    #[arg(long)]
    main_url: String,

    /// Environment variable holding this enrollment's bearer token. Never pass the token
    /// itself on the command line - it would sit in the process list and shell history in
    /// plain text. Ignored if --token-file is set.
    #[arg(long, default_value = "CONDUCTOR_EDGE_TOKEN")]
    token_env: String,

    /// Read the token from this file instead of an environment variable (whole-file
    /// contents, trimmed). Takes precedence over --token-env when set.
    #[arg(long)]
    token_file: Option<PathBuf>,

    /// Per-request timeout against the main Conductor, in milliseconds.
    #[arg(long, default_value_t = 15_000)]
    timeout_ms: u64,

    /// Environment variable holding a Cloudflare Access service token's Client ID.
    /// Entirely optional - when neither this variable nor --cf-access-client-id-file
    /// resolves to a value, no Cloudflare Access headers are sent at all, and direct/
    /// private-network operation is unaffected. Never pass either credential on the
    /// command line - same reasoning as the main bearer token.
    #[arg(long, default_value = "CF_ACCESS_CLIENT_ID")]
    cf_access_client_id_env: String,

    /// Read the Cloudflare Access Client ID from this file instead (whole-file
    /// contents, trimmed). Takes precedence over --cf-access-client-id-env when set.
    #[arg(long)]
    cf_access_client_id_file: Option<PathBuf>,

    /// Environment variable holding a Cloudflare Access service token's Client Secret.
    /// Same optionality as the Client ID above.
    #[arg(long, default_value = "CF_ACCESS_CLIENT_SECRET")]
    cf_access_client_secret_env: String,

    /// Read the Cloudflare Access Client Secret from this file instead (trimmed).
    /// Takes precedence over --cf-access-client-secret-env when set.
    #[arg(long)]
    cf_access_client_secret_file: Option<PathBuf>,
}

/// A Cloudflare Access service token: both halves present, or this type does not
/// exist. There is no state representing "half configured" - `load_cf_access` is the
/// one place that distinction is enforced, before this type is ever constructed.
struct CfAccessCredential {
    client_id: String,
    client_secret: String,
}

/// Resolves one optional credential half (file takes precedence over env). An unset
/// environment variable or an empty value in either source means "not configured" -
/// `Ok(None)`, never an error on its own; only `load_cf_access`'s combination of the
/// two halves decides whether that is actually an error.
fn load_optional_credential(file: &Option<PathBuf>, env_var: &str) -> Result<Option<String>> {
    if let Some(path) = file {
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let trimmed = raw.trim();
        return Ok(if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        });
    }
    match std::env::var(env_var) {
        Ok(value) if !value.trim().is_empty() => Ok(Some(value.trim().to_string())),
        Ok(_) => Ok(None),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => {
            bail!("environment variable {env_var} is not valid UTF-8")
        }
    }
}

/// Optional by design: `(None, None)` means the operator never intended to use
/// Cloudflare Access at all, so direct/private-network operation stays exactly as it
/// was. Exactly one half present is always a configuration mistake worth failing on
/// loudly, not guessing which half was meant or silently sending a half-formed,
/// certainly-rejected pair of headers.
fn load_cf_access(cli: &Cli) -> Result<Option<CfAccessCredential>> {
    let client_id =
        load_optional_credential(&cli.cf_access_client_id_file, &cli.cf_access_client_id_env)?;
    let client_secret = load_optional_credential(
        &cli.cf_access_client_secret_file,
        &cli.cf_access_client_secret_env,
    )?;
    match (client_id, client_secret) {
        (Some(client_id), Some(client_secret)) => Ok(Some(CfAccessCredential {
            client_id,
            client_secret,
        })),
        (None, None) => Ok(None),
        (Some(_), None) => bail!(
            "Cloudflare Access Client ID is configured but the Client Secret is not (checked {} / --cf-access-client-secret-file) - set both or neither",
            cli.cf_access_client_secret_env
        ),
        (None, Some(_)) => bail!(
            "Cloudflare Access Client Secret is configured but the Client ID is not (checked {} / --cf-access-client-id-file) - set both or neither",
            cli.cf_access_client_id_env
        ),
    }
}

fn load_token(cli: &Cli) -> Result<String> {
    if let Some(path) = &cli.token_file {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("read token file {}", path.display()))?;
        return Ok(raw.trim().to_string());
    }
    std::env::var(&cli.token_env)
        .with_context(|| format!("environment variable {} is not set", cli.token_env))
}

/// HTTP client implementation of `ConductorBackend`: every tool call becomes one
/// `POST {main_url}/api/edge/tools/call` with `{"tool": "<name>", "params": <request>}`,
/// bearer-authenticated. The main Conductor dispatches this to the identical
/// `LocalBackend` the stdio-local `mcp-serve` command uses - one implementation, two
/// transports. A non-2xx response is expected to carry a JSON-serialized `BackendError`
/// body (the two processes share that type); anything else becomes an `Internal` error
/// naming what actually happened, never a fabricated success.
struct RemoteBackend {
    client: reqwest::Client,
    base_url: String,
    token: String,
    cf_access: Option<CfAccessCredential>,
}

impl RemoteBackend {
    fn new(
        base_url: String,
        token: String,
        timeout: Duration,
        cf_access: Option<CfAccessCredential>,
    ) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .context("build HTTP client")?;
        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            token,
            cf_access,
        })
    }

    async fn call<Req: Serialize, Resp: DeserializeOwned>(
        &self,
        tool: &str,
        req: Req,
    ) -> Result<Resp, BackendError> {
        let url = format!("{}/api/edge/tools/call", self.base_url);
        let body = json!({"tool": tool, "params": req});
        // Two bounded retries on a connection-level failure only (DNS, refused connection,
        // timeout) - never on a response the main Conductor actually sent, successful or
        // not: retrying an already-executed mutating call risks a second attempt, and
        // `conductor_submit_task`'s own idempotency key is the correct tool for that, not a
        // blind HTTP retry here.
        let mut attempt = 0;
        let response = loop {
            let mut request = self.client.post(&url).bearer_auth(&self.token).json(&body);
            // Cloudflare Access service-token headers, additive to and never a
            // replacement for the main Conductor's own bearer auth above - the tunnel
            // gate and the application gate are deliberately separate layers. Absent
            // entirely when `cf_access` is `None` (the default, unconfigured case), so
            // direct/private-network operation sends exactly the request it always did.
            if let Some(cf) = &self.cf_access {
                request = request
                    .header("CF-Access-Client-Id", &cf.client_id)
                    .header("CF-Access-Client-Secret", &cf.client_secret);
            }
            match request.send().await {
                Ok(response) => break response,
                Err(error) if attempt < 2 && (error.is_connect() || error.is_timeout()) => {
                    attempt += 1;
                    tokio::time::sleep(Duration::from_millis(200 * attempt as u64)).await;
                }
                Err(error) => {
                    return Err(BackendError::internal(format!(
                        "could not reach main conductor at {url}: {error}"
                    )));
                }
            }
        };
        let status = response.status();
        let bytes = response.bytes().await.map_err(|e| {
            BackendError::internal(format!("failed to read main conductor response: {e}"))
        })?;
        if status.is_success() {
            serde_json::from_slice::<Resp>(&bytes).map_err(|e| {
                BackendError::internal(format!("malformed response from main conductor: {e}"))
            })
        } else {
            Err(
                serde_json::from_slice::<BackendError>(&bytes).unwrap_or_else(|_| {
                    BackendError::internal(format!(
                        "main conductor returned {status}: {}",
                        String::from_utf8_lossy(&bytes)
                    ))
                }),
            )
        }
    }

    async fn call_no_params<Resp: DeserializeOwned>(
        &self,
        tool: &str,
    ) -> Result<Resp, BackendError> {
        self.call(tool, RequestIdOnly { request_id: None }).await
    }
}

impl ConductorBackend for RemoteBackend {
    async fn conductor_capabilities(&self) -> Result<CapabilitiesResponse, BackendError> {
        self.call_no_params("conductor_capabilities").await
    }

    async fn conductor_list_repositories(
        &self,
        req: RequestIdOnly,
    ) -> Result<ListRepositoriesResponse, BackendError> {
        self.call("conductor_list_repositories", req).await
    }

    async fn conductor_list_tasks(
        &self,
        req: RequestIdOnly,
    ) -> Result<ListTasksResponse, BackendError> {
        self.call("conductor_list_tasks", req).await
    }

    async fn conductor_get_task(&self, req: GetTaskRequest) -> Result<TaskResponse, BackendError> {
        self.call("conductor_get_task", req).await
    }

    async fn conductor_propose_task(
        &self,
        req: ProposeTaskRequest,
    ) -> Result<ProposeTaskResponse, BackendError> {
        self.call("conductor_propose_task", req).await
    }

    async fn conductor_submit_task(
        &self,
        req: SubmitTaskRequest,
    ) -> Result<SubmitTaskResponse, BackendError> {
        self.call("conductor_submit_task", req).await
    }

    async fn conductor_schedule_task(
        &self,
        req: ScheduleTaskRequest,
    ) -> Result<ScheduleTaskResponse, BackendError> {
        self.call("conductor_schedule_task", req).await
    }

    async fn conductor_list_schedules(
        &self,
        req: RequestIdOnly,
    ) -> Result<ListSchedulesResponse, BackendError> {
        self.call("conductor_list_schedules", req).await
    }

    async fn conductor_cancel_schedule(
        &self,
        req: CancelScheduleRequest,
    ) -> Result<ListSchedulesResponse, BackendError> {
        self.call("conductor_cancel_schedule", req).await
    }

    async fn conductor_get_execution(
        &self,
        req: GetTaskRequest,
    ) -> Result<TaskResponse, BackendError> {
        self.call("conductor_get_execution", req).await
    }

    async fn conductor_get_evidence(
        &self,
        req: GetTaskRequest,
    ) -> Result<TaskResponse, BackendError> {
        self.call("conductor_get_evidence", req).await
    }

    async fn conductor_get_approval_requirements(
        &self,
        req: TaskIdRequest,
    ) -> Result<ApprovalRequirementsResponse, BackendError> {
        self.call("conductor_get_approval_requirements", req).await
    }

    async fn conductor_approve(
        &self,
        req: DecisionRequest,
    ) -> Result<DecisionResponse, BackendError> {
        self.call("conductor_approve", req).await
    }

    async fn conductor_reject(
        &self,
        req: DecisionRequest,
    ) -> Result<DecisionResponse, BackendError> {
        self.call("conductor_reject", req).await
    }

    async fn conductor_resume(&self, req: ResumeRequest) -> Result<ResumeResponse, BackendError> {
        self.call("conductor_resume", req).await
    }

    async fn conductor_cancel_task(
        &self,
        req: CancelTaskRequest,
    ) -> Result<DecisionResponse, BackendError> {
        self.call("conductor_cancel_task", req).await
    }

    async fn conductor_global_stop_status(
        &self,
        req: RequestIdOnly,
    ) -> Result<StopStatusResponse, BackendError> {
        self.call("conductor_global_stop_status", req).await
    }

    async fn conductor_get_evidence_summary(
        &self,
        req: EvidenceSummaryRequest,
    ) -> Result<EvidenceSummaryResponse, BackendError> {
        self.call("conductor_get_evidence_summary", req).await
    }

    async fn conductor_get_artifact(
        &self,
        req: GetArtifactRequest,
    ) -> Result<GetArtifactResponse, BackendError> {
        self.call("conductor_get_artifact", req).await
    }
}

fn to_mcp_error(err: BackendError) -> McpError {
    let BackendError {
        kind,
        message,
        data,
    } = err;
    match kind {
        ErrorKind::NotFound => McpError::resource_not_found(message, data),
        ErrorKind::Conflict | ErrorKind::InvalidParams | ErrorKind::Forbidden => {
            McpError::invalid_params(message, data)
        }
        ErrorKind::Internal => McpError::internal_error(message, data),
    }
}

/// The `rmcp` stdio adapter: a thin wrapper over one `RemoteBackend`, structurally
/// identical to `clatterforge`'s `McpServer` over `LocalBackend` - same tool names, same
/// schemas, same descriptions, different backend underneath. See this module's doc
/// comment and `docs/m12/M12_0_ARCHITECTURE.md` section 2 for why this duplication is
/// schema plumbing, not logic.
#[derive(Clone)]
struct EdgeMcpServer {
    backend: std::sync::Arc<RemoteBackend>,
}

#[tool_router]
impl EdgeMcpServer {
    #[tool(description = "Describe what this adapter actually supports. Call this first.")]
    async fn conductor_capabilities(&self) -> Result<Json<CapabilitiesResponse>, McpError> {
        self.backend
            .conductor_capabilities()
            .await
            .map(Json)
            .map_err(to_mcp_error)
    }

    #[tool(description = "List every repository the main Conductor's state root has registered.")]
    async fn conductor_list_repositories(
        &self,
        Parameters(req): Parameters<RequestIdOnly>,
    ) -> Result<Json<ListRepositoriesResponse>, McpError> {
        self.backend
            .conductor_list_repositories(req)
            .await
            .map(Json)
            .map_err(to_mcp_error)
    }

    #[tool(description = "List every task across all engines on the main Conductor.")]
    async fn conductor_list_tasks(
        &self,
        Parameters(req): Parameters<RequestIdOnly>,
    ) -> Result<Json<ListTasksResponse>, McpError> {
        self.backend
            .conductor_list_tasks(req)
            .await
            .map(Json)
            .map_err(to_mcp_error)
    }

    #[tool(description = "Full detail for one task by exact id or unambiguous prefix.")]
    async fn conductor_get_task(
        &self,
        Parameters(req): Parameters<GetTaskRequest>,
    ) -> Result<Json<TaskResponse>, McpError> {
        self.backend
            .conductor_get_task(req)
            .await
            .map(Json)
            .map_err(to_mcp_error)
    }

    #[tool(description = "Same task record as conductor_get_task, labeled for the execution view.")]
    async fn conductor_get_execution(
        &self,
        Parameters(req): Parameters<GetTaskRequest>,
    ) -> Result<Json<TaskResponse>, McpError> {
        self.backend
            .conductor_get_execution(req)
            .await
            .map(Json)
            .map_err(to_mcp_error)
    }

    #[tool(description = "Same task record as conductor_get_task, labeled for the evidence view.")]
    async fn conductor_get_evidence(
        &self,
        Parameters(req): Parameters<GetTaskRequest>,
    ) -> Result<Json<TaskResponse>, McpError> {
        self.backend
            .conductor_get_evidence(req)
            .await
            .map(Json)
            .map_err(to_mcp_error)
    }

    #[tool(description = "Dry-run validation of a local-changes-v1 orchestration::Spec.")]
    async fn conductor_propose_task(
        &self,
        Parameters(req): Parameters<ProposeTaskRequest>,
    ) -> Result<Json<ProposeTaskResponse>, McpError> {
        self.backend
            .conductor_propose_task(req)
            .await
            .map(Json)
            .map_err(to_mcp_error)
    }

    #[tool(
        description = "Create a task on the main Conductor. Never auto-approves or auto-runs \
    unless a standing grant covers it - call conductor_approve next."
    )]
    async fn conductor_submit_task(
        &self,
        Parameters(req): Parameters<SubmitTaskRequest>,
    ) -> Result<Json<SubmitTaskResponse>, McpError> {
        self.backend
            .conductor_submit_task(req)
            .await
            .map(Json)
            .map_err(to_mcp_error)
    }

    #[tool(
        description = "Schedule a conductor_submit_task request for later (not_before_ms, \
    optional bounded repeat). Each occurrence is submitted as this client under the same \
    approval rules; a standing grant may auto-authorize it at that moment."
    )]
    async fn conductor_schedule_task(
        &self,
        Parameters(req): Parameters<ScheduleTaskRequest>,
    ) -> Result<Json<ScheduleTaskResponse>, McpError> {
        self.backend
            .conductor_schedule_task(req)
            .await
            .map(Json)
            .map_err(to_mcp_error)
    }

    #[tool(description = "List this client's schedules and every firing.")]
    async fn conductor_list_schedules(
        &self,
        Parameters(req): Parameters<RequestIdOnly>,
    ) -> Result<Json<ListSchedulesResponse>, McpError> {
        self.backend
            .conductor_list_schedules(req)
            .await
            .map(Json)
            .map_err(to_mcp_error)
    }

    #[tool(description = "Cancel one of this client's active schedules.")]
    async fn conductor_cancel_schedule(
        &self,
        Parameters(req): Parameters<CancelScheduleRequest>,
    ) -> Result<Json<ListSchedulesResponse>, McpError> {
        self.backend
            .conductor_cancel_schedule(req)
            .await
            .map(Json)
            .map_err(to_mcp_error)
    }

    #[tool(description = "What, if anything, a task is currently waiting on.")]
    async fn conductor_get_approval_requirements(
        &self,
        Parameters(req): Parameters<TaskIdRequest>,
    ) -> Result<Json<ApprovalRequirementsResponse>, McpError> {
        self.backend
            .conductor_get_approval_requirements(req)
            .await
            .map(Json)
            .map_err(to_mcp_error)
    }

    #[tool(description = "Say yes to whatever decision point the task is currently at.")]
    async fn conductor_approve(
        &self,
        Parameters(req): Parameters<DecisionRequest>,
    ) -> Result<Json<DecisionResponse>, McpError> {
        self.backend
            .conductor_approve(req)
            .await
            .map(Json)
            .map_err(to_mcp_error)
    }

    #[tool(description = "Say no to whatever decision point the task is currently at.")]
    async fn conductor_reject(
        &self,
        Parameters(req): Parameters<DecisionRequest>,
    ) -> Result<Json<DecisionResponse>, McpError> {
        self.backend
            .conductor_reject(req)
            .await
            .map(Json)
            .map_err(to_mcp_error)
    }

    #[tool(
        description = "Continue a task after authorization was obtained, or reconcile a crashed one."
    )]
    async fn conductor_resume(
        &self,
        Parameters(req): Parameters<ResumeRequest>,
    ) -> Result<Json<ResumeResponse>, McpError> {
        self.backend
            .conductor_resume(req)
            .await
            .map(Json)
            .map_err(to_mcp_error)
    }

    #[tool(description = "Cancel a task.")]
    async fn conductor_cancel_task(
        &self,
        Parameters(req): Parameters<CancelTaskRequest>,
    ) -> Result<Json<DecisionResponse>, McpError> {
        self.backend
            .conductor_cancel_task(req)
            .await
            .map(Json)
            .map_err(to_mcp_error)
    }

    #[tool(description = "Whether a global stop is currently engaged on the main Conductor.")]
    async fn conductor_global_stop_status(
        &self,
        Parameters(req): Parameters<RequestIdOnly>,
    ) -> Result<Json<StopStatusResponse>, McpError> {
        self.backend
            .conductor_global_stop_status(req)
            .await
            .map(Json)
            .map_err(to_mcp_error)
    }

    #[tool(
        description = "A bounded, AI-orchestrator-shaped summary of one task's most recent \
    attempt - status, verification outcome, failing check, a concise error line, one inline \
    bounded log excerpt, the evidence digest, and artifact labels conductor_get_artifact can \
    drill into."
    )]
    async fn conductor_get_evidence_summary(
        &self,
        Parameters(req): Parameters<EvidenceSummaryRequest>,
    ) -> Result<Json<EvidenceSummaryResponse>, McpError> {
        self.backend
            .conductor_get_evidence_summary(req)
            .await
            .map(Json)
            .map_err(to_mcp_error)
    }

    #[tool(
        description = "Bounded, paginated drill-down into one named artifact of a task's most \
    recent attempt. Pass offset = a previous response's offset + shown_bytes to continue."
    )]
    async fn conductor_get_artifact(
        &self,
        Parameters(req): Parameters<GetArtifactRequest>,
    ) -> Result<Json<GetArtifactResponse>, McpError> {
        self.backend
            .conductor_get_artifact(req)
            .await
            .map(Json)
            .map_err(to_mcp_error)
    }
}

#[tool_handler]
impl ServerHandler for EdgeMcpServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "clatterforge-edge",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Clatterforge Spark (edge adapter). Forwards every conductor_* call, authenticated, \
                to the main forge - a Spark owns no task state of its own. Same lifecycle \
                as the local adapter: conductor_submit_task -> conductor_get_approval_requirements/ \
                conductor_approve -> conductor_resume -> conductor_get_task/conductor_get_evidence -> \
                conductor_approve/conductor_reject. Call conductor_capabilities first.",
            )
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let token = load_token(&cli)?;
    let cf_access = load_cf_access(&cli)?;
    let cf_access_active = cf_access.is_some();
    let backend = std::sync::Arc::new(RemoteBackend::new(
        cli.main_url.clone(),
        token,
        Duration::from_millis(cli.timeout_ms),
        cf_access,
    )?);
    eprintln!(
        "clatterforge-edge: forwarding to {} (timeout {}ms, Cloudflare Access headers {})",
        cli.main_url,
        cli.timeout_ms,
        if cf_access_active {
            "enabled"
        } else {
            "disabled"
        }
    );
    let server = EdgeMcpServer { backend };
    let service = server
        .serve(rmcp::transport::stdio())
        .await
        .context("start edge MCP stdio server")?;
    service.waiting().await?;
    Ok(())
}
