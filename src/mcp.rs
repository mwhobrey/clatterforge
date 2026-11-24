//! Generic local MCP (Model Context Protocol) stdio adapter — M11 Phase 1, split into
//! schema + backend for M12.0 (see `docs/m12/M12_0_ARCHITECTURE.md` section 2).
//!
//! The tool contract itself (every `conductor_*` request/response DTO, the
//! `ConductorBackend` trait, `BackendError`) now lives in `clatterforge-protocol`, a crate
//! with no dependency on `rmcp`/`rusqlite`/`axum`/any execution engine — see
//! `protocol/src/lib.rs`. `LocalBackend` below is this trait's in-process implementation,
//! calling the existing `TaskService`/`Orchestrator`/`Store`/`GlobalStop` lifecycle
//! directly, exactly as `McpServer`'s tool methods did before this split. `McpServer` is
//! now a thin `rmcp` adapter over one `LocalBackend` — every `#[tool]` method is a
//! one-line forward. This module adds no new execution engine and no new safety boundary.
//! `clatterforge-edge`'s `RemoteBackend` (a separate crate) implements the identical
//! trait over an authenticated HTTP call to this process's new `/api/edge/tools/call`
//! route (`src/serve.rs`) instead of calling these engines in-process.
//!
//! **Authorization (Phase 2 of M11):** every mutating tool first checks
//! `authorization::Grants` for a grant covering the exact `(principal, repository
//! identity, backend, operation)` — see `src/authorization.rs` for the full model. When one
//! covers it, the operation proceeds with `by = "grant:{id}@v{version}"` instead of
//! pausing for an interactive call, and `conductor_submit_task` chains submit → resume →
//! review-accept automatically as far as a single grant's `operations` list covers. When no
//! grant covers it, every tool falls back to the plain, interactive behavior: `principal`
//! is used verbatim as the engine's `by`. Grants are never created, listed, or modified
//! through this adapter — only through the CLI.
//!
//! **Idempotency:** `local-changes-v1` submissions get genuine crash-safe idempotency by
//! reusing `Orchestrator::create_with_id` with a deterministic id derived from
//! `(principal, workflow_id, idempotency_key)`. `fake-process-v1` submissions only dedupe
//! within this adapter process's own lifetime (a table in `state.sqlite`, checked first)
//! since `Store::create` has no caller-supplied-id form; that is a real, documented
//! limitation, not a silent gap, and `fake-process-v1` is a test/demo backend only.

use crate::{
    authorization::{Grant, Grants, Operation},
    board,
    inference::config::Config,
    now, open_sqlite_with_retry,
    orchestration::{self, Orchestrator},
    process::ExecutionPolicy,
    repository::Repository,
    service::TaskService,
    stop::GlobalStop,
    store::Store,
};
use anyhow::{Context, Result};
pub use clatterforge_protocol::{
    AUTHORIZATION_NOTE, ApprovalRequirementsResponse, CancelTaskRequest, CapabilitiesResponse,
    DecisionRequest, DecisionResponse, EvidenceSummaryRequest, EvidenceSummaryResponse,
    GetArtifactRequest, GetArtifactResponse, GetTaskRequest, ListRepositoriesResponse,
    ListTasksResponse, ProposeTaskRequest, ProposeTaskResponse, RequestIdOnly, ResumeRequest,
    ResumeResponse, ScheduleTaskResponse, StopStatusResponse, SubmitTaskRequest,
    SubmitTaskResponse, TOOL_NAMES, TaskIdRequest, TaskResponse,
};
use clatterforge_protocol::{BackendError, ConductorBackend, ErrorKind};
use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    handler::server::wrapper::{Json, Parameters},
    model::{Implementation, ServerCapabilities, ServerConfig},
    tool, tool_handler, tool_router,
};
use rusqlite::params;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// Runs the adapter over stdio until the client disconnects or the process is signalled.
/// This is the entry point `main.rs`'s `mcp-serve` command calls.
pub async fn run(root: PathBuf) -> Result<()> {
    let server = McpServer::open(root)?;
    let service = server.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

fn ensure_idempotency_table(root: &Path) -> Result<()> {
    let connection = open_sqlite_with_retry(&root.join("state.sqlite"), 1)
        .context("open mcp idempotency state")?;
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS mcp_idempotency(
            principal TEXT NOT NULL,
            workflow_id TEXT NOT NULL,
            idempotency_key TEXT NOT NULL,
            task_id TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            PRIMARY KEY (principal, workflow_id, idempotency_key)
        );
        CREATE TABLE IF NOT EXISTS mcp_task_metadata(
            task_id TEXT PRIMARY KEY,
            workflow_id TEXT,
            source_system TEXT,
            source_record_reference TEXT,
            correlation_id TEXT,
            follow_up_of TEXT,
            created_at INTEGER NOT NULL,
            initiating_client TEXT
        );",
    )?;
    // `mcp_task_metadata` predates `initiating_client` (M12.0 Phase 3) - a state root
    // created before this phase has the table without the column. `ALTER TABLE ... ADD
    // COLUMN` is the standard SQLite migration for this; the error on an already-migrated
    // (or freshly created, already-correct) database is "duplicate column name", which is
    // exactly the success case here, so it's deliberately swallowed rather than treated as
    // a real failure.
    let _ =
        connection.execute_batch("ALTER TABLE mcp_task_metadata ADD COLUMN initiating_client TEXT");
    Ok(())
}

/// Deterministic task id for the `local-changes-v1` idempotent-submit path: a stable,
/// UUID-shaped digest of the caller-asserted identity triple. Stable across retries from the
/// same caller with the same key; different for any different principal/workflow/key.
fn deterministic_task_id(principal: &str, workflow_id: &str, idempotency_key: &str) -> String {
    let digest =
        crate::digest(format!("{principal}\u{0}{workflow_id}\u{0}{idempotency_key}").as_bytes());
    format!(
        "{}-{}-{}-{}-{}",
        &digest[0..8],
        &digest[8..12],
        &digest[12..16],
        &digest[16..20],
        &digest[20..32]
    )
}

/// Best-effort request id: echoed back if the caller supplied one, generated otherwise.
fn request_id(supplied: Option<String>) -> String {
    supplied.unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
}

/// M12.0 Phase 5: the only two backends whose `run` step can take real wall-clock minutes
/// (a genuine external CLI, possibly billable) rather than a bounded local computation -
/// see `dispatch_run` for why that distinction is the one this module actually needs to
/// make, not a general "which engine is this" check.
fn is_agent_backend(backend: &str) -> bool {
    matches!(backend, "cursor-agent" | "codex" | "workflow-v1")
}

/// M12.0 Phase 6: every artifact label `TaskService::artifact_path` can resolve, kept as
/// one list so `conductor_get_evidence_summary`'s `available_artifacts` and `conductor_
/// get_artifact`'s accepted `artifact` values can never silently drift apart.
const KNOWN_ARTIFACT_LABELS: &[&str] = &["diff", "verifier stdout", "verifier stderr"];

/// Preference order for `conductor_get_evidence_summary`'s single inline log excerpt:
/// stderr first (most likely to explain a failure), then stdout, then the diff itself
/// (the most useful thing to show on an otherwise-silent success).
const PREFERRED_LOG_EXCERPT_ORDER: &[&str] = &["verifier stderr", "verifier stdout", "diff"];

const DEFAULT_LOG_EXCERPT_BYTES: usize = 2048;
const DEFAULT_ARTIFACT_CHUNK_BYTES: usize = 8192;
/// Hard server-side cap regardless of what a caller's `max_bytes` asks for - the brief's
/// own "do not dump unbounded logs into a single MCP response," enforced here rather than
/// trusted to the caller.
const MAX_ARTIFACT_CHUNK_BYTES: usize = 65536;

/// Starts a task's `run` step without waiting for it - the brief's own "do not block an
/// MCP request for an unbounded coding-agent execution... implement a bounded
/// asynchronous job interface backed by authoritative task state" requirement, applied
/// only to `is_agent_backend` tasks (`local-changes-v1`/`fake-process-v1` keep M11's
/// existing synchronous behavior unchanged - this never makes an already-working path
/// behave differently). The spawned thread opens its own fresh `TaskService` handle (the
/// same "open per call, never share" discipline every other method here already follows)
/// and calls the identical `TaskService::run` every synchronous caller uses - there is no
/// second execution path, only a different caller for the same one. A failure here (a
/// race against a concurrent cancel/reconcile, a repository lease conflict) has no
/// in-flight MCP caller left to report to; it lands in this process's own stderr and,
/// more importantly, in the task's own state (every engine's `run` already records
/// failure *inside* the task record before returning) - a poller always sees the truth
/// either way, just not synchronously from this call.
fn dispatch_run_in_background(root: PathBuf, task_id: String, revision: u64) {
    std::thread::spawn(move || {
        let result =
            TaskService::open(&root).and_then(|mut service| service.run(&task_id, revision));
        if let Err(error) = result {
            if let Ok(agents) = crate::agents::Agents::open(&root) {
                let _ = agents.routing_note(
                    &task_id,
                    "dispatch_refused",
                    "TaskService::run",
                    None,
                    Some(&format!("{error:#}")),
                );
            }
            eprintln!(
                "clatterforge: background agent run for task {task_id} did not start: {error:#}"
            );
        }
    });
}

/// Maps a `BackendError` to a structured MCP error. `NotFound` gets `rmcp`'s dedicated
/// not-found code; `Conflict` and `InvalidParams` both map to `invalid_params` since `rmcp`
/// has no separate conflict code — the same choice the pre-split `to_mcp_error` made, kept
/// so an already-integrated client's `data.kind` branching sees no change.
pub(crate) fn to_mcp_error(err: BackendError) -> McpError {
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

fn detail_value(detail: &board::Detail) -> Result<Value, BackendError> {
    serde_json::to_value(detail)
        .map_err(|e| BackendError::internal(format!("failed to serialize task detail: {e}")))
}

/// In-process implementation of `ConductorBackend`: calls `TaskService`/`Orchestrator`/
/// `Store`/`GlobalStop` directly against `root`, opening a fresh handle per call exactly
/// like `serve.rs` opens a fresh `TaskService` per HTTP request — no long-held lock or
/// connection. Used by stdio `mcp-serve` (via `McpServer`) and by `/api/edge/tools/call`
/// (`src/serve.rs`) — one implementation, two transports in front of it.
#[derive(Debug, Clone)]
pub struct LocalBackend {
    root: PathBuf,
    /// `false` for stdio `mcp-serve` (any caller-asserted `principal`, no enrollment
    /// verification exists on that path); `true` only when opened from
    /// `serve.rs`'s `/api/edge/tools/call`, *after* `authenticate_edge` has already
    /// verified an enrollment and overwritten `principal` with its real identity. Gates
    /// two M12.0 Phase 3 checks: whether an `enrollment::PRINCIPAL_NAMESPACE_PREFIX`-scoped
    /// grant can match at all (`covering_grant`), and whether an interactive (ungranted)
    /// approve/reject/resume/cancel must match the task's recorded initiating client
    /// (every `conductor_approve`-family method). Stdio local mode keeps its exact pre-M12
    /// behavior either way - this field only ever adds restrictions, never removes one
    /// that path already had.
    trusted: bool,
    /// M12.0.1 Phase 1: the calling enrollment's `Enrollment.repository_scope`, verbatim -
    /// empty for stdio (`trusted == false`, where this concept doesn't apply at all) *and*
    /// for an edge-authenticated enrollment that was deliberately created with no
    /// repository restriction (`enrollment-create` with `--repositories` omitted) - that is
    /// a real, documented "unrestricted" state, not an accidental bypass, and is exactly
    /// what let every M12.0 edge-path test (all written before this field existed, all
    /// using an empty scope) keep passing unchanged. A *non-empty* scope is an allow-list:
    /// `ensure_repository_identity_in_scope`/`ensure_task_repository_in_scope` are the only
    /// two places that ever read this field, and a grant - however broad - cannot widen it;
    /// see `src/enrollment.rs`'s module doc for why that ordering matters.
    repository_scope: Vec<String>,
}

impl LocalBackend {
    /// Opens the adapter against `root` (the same state directory every other engine uses),
    /// creating the small idempotency table this module owns if it doesn't exist yet. Used
    /// by stdio `mcp-serve` - `principal` is whatever the caller asserts, unverified,
    /// exactly as M11 documented.
    pub fn open(root: PathBuf) -> Result<Self> {
        ensure_idempotency_table(&root)?;
        Ok(Self {
            root,
            trusted: false,
            repository_scope: Vec::new(),
        })
    }

    /// Opens the adapter for use *only* after the caller's identity has already been
    /// cryptographically verified (`serve.rs::authenticate_edge`) and substituted into
    /// every request's `principal` field (`clatterforge_protocol::SetPrincipal`) - never call
    /// this with a request whose `principal` could still be caller-controlled.
    /// `repository_scope` is that same enrollment's own `Enrollment.repository_scope` -
    /// pass it verbatim, including empty (meaning unrestricted - see the field's own doc
    /// comment), never a value derived from the request itself.
    pub fn open_trusted(root: PathBuf, repository_scope: Vec<String>) -> Result<Self> {
        ensure_idempotency_table(&root)?;
        Ok(Self {
            root,
            trusted: true,
            repository_scope,
        })
    }

    fn task_service(&self) -> Result<TaskService, BackendError> {
        Ok(TaskService::open(&self.root)?)
    }

    fn idempotency_lookup(
        &self,
        principal: &str,
        workflow_id: &str,
        key: &str,
    ) -> Result<Option<String>, BackendError> {
        let connection = open_sqlite_with_retry(&self.root.join("state.sqlite"), 1)?;
        connection
            .query_row(
                "SELECT task_id FROM mcp_idempotency WHERE principal=?1 AND workflow_id=?2 AND idempotency_key=?3",
                params![principal, workflow_id, key],
                |row| row.get(0),
            )
            .map(Some)
            .or_else(|e| if matches!(e, rusqlite::Error::QueryReturnedNoRows) { Ok(None) } else { Err(e) })
            .map_err(|e| BackendError::internal(e.to_string()))
    }

    fn idempotency_record(
        &self,
        principal: &str,
        workflow_id: &str,
        key: &str,
        task_id: &str,
    ) -> Result<(), BackendError> {
        let connection = open_sqlite_with_retry(&self.root.join("state.sqlite"), 1)?;
        connection
            .execute(
                "INSERT INTO mcp_idempotency(principal, workflow_id, idempotency_key, task_id, created_at) VALUES (?1,?2,?3,?4,?5)",
                params![principal, workflow_id, key, task_id, now() as i64],
            )
            .map_err(|e| BackendError::internal(e.to_string()))?;
        Ok(())
    }

    fn record_workflow_origin(
        &self,
        req: &SubmitTaskRequest,
        task_id: &str,
    ) -> Result<(), BackendError> {
        let mut connection = open_sqlite_with_retry(&self.root.join("state.sqlite"), 1)?;
        let tx = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(anyhow::Error::from)?;
        if let (Some(workflow), Some(key)) = (&req.workflow_id, &req.idempotency_key) {
            tx.execute(
                "INSERT OR IGNORE INTO mcp_idempotency VALUES(?1,?2,?3,?4,?5)",
                params![req.principal, workflow, key, task_id, now()],
            )
            .map_err(anyhow::Error::from)?;
            let recorded:String=tx.query_row("SELECT task_id FROM mcp_idempotency WHERE principal=?1 AND workflow_id=?2 AND idempotency_key=?3",params![req.principal,workflow,key],|r|r.get(0)).map_err(anyhow::Error::from)?;
            if recorded != task_id {
                return Err(BackendError::conflict(
                    "idempotency key belongs to another task",
                ));
            }
        }
        tx.execute("INSERT OR IGNORE INTO mcp_task_metadata(task_id,workflow_id,source_system,source_record_reference,correlation_id,follow_up_of,created_at,initiating_client) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",params![task_id,req.workflow_id,req.source_system,req.source_record_reference,req.correlation_id,req.follow_up_of,now(),req.principal]).map_err(anyhow::Error::from)?;
        let principal: String = tx
            .query_row(
                "SELECT initiating_client FROM mcp_task_metadata WHERE task_id=?1",
                [task_id],
                |r| r.get(0),
            )
            .map_err(anyhow::Error::from)?;
        if principal != req.principal {
            return Err(BackendError::forbidden(
                "workflow belongs to another caller",
            ));
        }
        tx.commit().map_err(anyhow::Error::from)?;
        Ok(())
    }

    /// Stores the Phase 3 (M11) provenance fields a `conductor_submit_task` call supplied,
    /// plus `initiating_client` (M12.0 Phase 3) - the exact `principal` that request
    /// carried *after* any edge-path override, i.e. the real submitting identity, recorded
    /// once at creation and never updated afterward (task ownership is immutable). Best-
    /// effort description only for the other fields - nothing here gates dispatch or is
    /// read by any engine; `initiating_client` is the one exception, read back by
    /// `initiating_client_for` to gate cross-client approve/reject/resume/cancel.
    #[allow(clippy::too_many_arguments)]
    fn record_metadata(
        &self,
        task_id: &str,
        workflow_id: Option<&str>,
        source_system: Option<&str>,
        source_record_reference: Option<&str>,
        correlation_id: Option<&str>,
        follow_up_of: Option<&str>,
        initiating_client: &str,
    ) -> Result<(), BackendError> {
        let connection = open_sqlite_with_retry(&self.root.join("state.sqlite"), 1)?;
        connection
            .execute(
                "INSERT INTO mcp_task_metadata(
                    task_id, workflow_id, source_system, source_record_reference, correlation_id, follow_up_of, created_at, initiating_client
                ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                params![
                    task_id,
                    workflow_id,
                    source_system,
                    source_record_reference,
                    correlation_id,
                    follow_up_of,
                    now() as i64,
                    initiating_client
                ],
            )
            .map_err(|e| BackendError::internal(e.to_string()))?;
        Ok(())
    }

    fn metadata_for(&self, task_id: &str) -> Result<Value, BackendError> {
        let connection = open_sqlite_with_retry(&self.root.join("state.sqlite"), 1)?;
        connection
            .query_row(
                "SELECT workflow_id, source_system, source_record_reference, correlation_id, follow_up_of, initiating_client
                 FROM mcp_task_metadata WHERE task_id=?1",
                [task_id],
                |row| {
                    Ok(json!({
                        "workflow_id": row.get::<_, Option<String>>(0)?,
                        "source_system": row.get::<_, Option<String>>(1)?,
                        "source_record_reference": row.get::<_, Option<String>>(2)?,
                        "correlation_id": row.get::<_, Option<String>>(3)?,
                        "follow_up_of": row.get::<_, Option<String>>(4)?,
                        "initiating_client": row.get::<_, Option<String>>(5)?,
                    }))
                },
            )
            .or_else(|e| {
                if matches!(e, rusqlite::Error::QueryReturnedNoRows) {
                    Ok(json!({
                        "workflow_id": null,
                        "source_system": null,
                        "source_record_reference": null,
                        "correlation_id": null,
                        "follow_up_of": null,
                        "initiating_client": null,
                    }))
                } else {
                    Err(e)
                }
            })
            .map_err(|e| BackendError::internal(e.to_string()))
    }

    /// `None` for a task created before M12.0 Phase 3, or outside MCP entirely (CLI/HTTP) -
    /// callers must treat that as "no recorded owner to check against," not as "owned by
    /// nobody, so anyone may act on it": see each `conductor_approve`-family method's own
    /// handling of this case.
    fn initiating_client_for(&self, task_id: &str) -> Result<Option<String>, BackendError> {
        let connection = open_sqlite_with_retry(&self.root.join("state.sqlite"), 1)?;
        connection
            .query_row(
                "SELECT initiating_client FROM mcp_task_metadata WHERE task_id=?1",
                [task_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .or_else(|e| {
                if matches!(e, rusqlite::Error::QueryReturnedNoRows) {
                    Ok(None)
                } else {
                    Err(e)
                }
            })
            .map_err(|e| BackendError::internal(e.to_string()))
    }

    fn grants(&self) -> Result<Grants, BackendError> {
        Ok(Grants::open(&self.root)?)
    }

    /// The stable scoping key a grant is matched against — re-derived fresh from the
    /// checkout path every time, never cached, exactly like `Coordination`'s own lease
    /// resource key. `checkout` is `board::Detail.repository` (a path), not a `Store`
    /// registration id — see `src/authorization.rs`'s module doc for why.
    fn repository_identity(checkout: &str) -> Result<String, BackendError> {
        Ok(Repository::inspect(Path::new(checkout))?.identity)
    }

    /// Looks up a covering grant without using it. Every mutating tool calls this first;
    /// `None` means "fall back to the plain interactive principal." M12.0 Phase 3: a grant
    /// whose principal is in `enrollment::PRINCIPAL_NAMESPACE_PREFIX`'s namespace is
    /// invisible to an untrusted (stdio-local) caller, regardless of what principal string
    /// it asserts - that namespace exists specifically so authority meant for one
    /// cryptographically verified remote identity can never be inherited by an unverified
    /// local one. A trusted (edge) caller's `principal` is already the verified, namespaced
    /// identity by the time it reaches here (`clatterforge_protocol::SetPrincipal`), so this
    /// never filters out a grant the caller was actually, correctly matched against.
    fn covering_grant(
        &self,
        principal: &str,
        repository_identity: &str,
        backend: &str,
        operation: Operation,
        task_id: &str,
    ) -> Result<Option<Grant>, BackendError> {
        let grant = self.grants()?.find_covering_task(
            principal,
            repository_identity,
            backend,
            operation,
            task_id,
        )?;
        Ok(grant.filter(|g| {
            self.trusted
                || !g
                    .principal
                    .starts_with(crate::enrollment::PRINCIPAL_NAMESPACE_PREFIX)
        }))
    }

    /// M12.0 Phase 3's cross-client task-approval gate: called only from the "no grant
    /// covered this" branch of `conductor_approve`/`conductor_reject`/`conductor_resume`/
    /// `conductor_cancel_task`, since a covering grant is already "explicitly delegated
    /// separate authority" and needs no further check. Untrusted (stdio) callers are
    /// exempt entirely - this only restricts the trusted (edge) path, where multiple
    /// enrolled clients can otherwise all reach the same interactive fallback. A task with
    /// no recorded `initiating_client` (created before this phase, or via CLI/HTTP) has
    /// nothing to check against and is allowed through unchanged - this closes a *new*
    /// cross-client gap, not a pre-existing one for tasks this phase never touched.
    fn ensure_self_service_or_owned(
        &self,
        principal: &str,
        task_id: &str,
    ) -> Result<(), BackendError> {
        if !self.trusted {
            return Ok(());
        }
        if let Some(owner) = self.initiating_client_for(task_id)?
            && owner != principal
        {
            return Err(BackendError::forbidden(format!(
                "{principal} cannot act on task {task_id} - it was initiated by a different enrolled client, and no grant delegates this operation"
            )));
        }
        Ok(())
    }

    /// M12.0.1 Phase 1's mandatory security gate: the one place `self.repository_scope` is
    /// ever consulted. Checked *before* any grant lookup, ownership check, or engine
    /// mutation - "effective authorization is the intersection of... enrollment repository
    /// scope... [and] applicable delegated grant," per the brief, and a grant must never be
    /// able to override a narrower enrollment. An empty scope means this enrollment was
    /// deliberately created unrestricted (see `LocalBackend.repository_scope`'s own doc
    /// comment) and always passes.
    fn ensure_repository_identity_in_scope(
        &self,
        repository_identity: &str,
    ) -> Result<(), BackendError> {
        if self.repository_scope.is_empty()
            || self
                .repository_scope
                .iter()
                .any(|allowed| allowed == repository_identity)
        {
            return Ok(());
        }
        Err(BackendError::forbidden(format!(
            "repository '{repository_identity}' is outside this client's enrolled repository scope"
        )))
    }

    /// Same check, starting from an already-resolved task `Detail` - the shape every
    /// `conductor_*` method that already has one needs. Returns the identical "no task id
    /// matches" `NotFound` wording the engine itself uses for a genuinely nonexistent task
    /// (`src/store.rs`/`src/orchestration.rs`/`src/agents.rs`'s own resolve errors), so a
    /// caller cannot tell "this task exists but you can't see it" apart from "this task
    /// never existed" - the brief's own explicit anti-disclosure requirement. Deliberately
    /// a different error shape from `ensure_repository_identity_in_scope`'s (used only
    /// pre-creation, where there is no task identity to protect yet).
    fn ensure_task_repository_in_scope(&self, detail: &board::Detail) -> Result<(), BackendError> {
        if self.repository_scope.is_empty() {
            return Ok(());
        }
        let repository_identity = Self::repository_identity(&detail.repository)?;
        if self
            .repository_scope
            .iter()
            .any(|allowed| allowed == &repository_identity)
        {
            return Ok(());
        }
        Err(BackendError::not_found("no task id matches"))
    }

    /// Records that `grant` authorized `operation` against this task. Call this only after
    /// the underlying engine call it authorized has actually succeeded - use `grant.label()`
    /// directly (a pure function, no I/O) for the `by` string the engine call itself needs
    /// beforehand. Keeping these separate is what lets every call site avoid recording (and,
    /// for `Operation::Submit`, consuming a `max_tasks` slot for) a decision whose engine
    /// call might still fail.
    fn use_grant(
        &self,
        grant: &Grant,
        operation: Operation,
        task_id: &str,
        task_revision: u64,
        evidence_digest: Option<&str>,
        request_id: &str,
    ) -> Result<(), BackendError> {
        self.grants()?.record_use(
            &grant.id,
            operation,
            task_id,
            task_revision,
            evidence_digest,
            Some(request_id),
        )?;
        Ok(())
    }

    /// Advances a freshly created task as far as covering grants authorize, in order:
    /// approve the proposal (`Operation::Submit`), run it (`Operation::Resume`), then
    /// auto-accept the review (`Operation::Approve`) - stopping the instant any step isn't
    /// covered, or the run didn't succeed (a failed/needs-work result is never auto-
    /// accepted). Every step still goes through the unchanged `TaskService` methods, so
    /// global stop, repository leases, dependency gating and independent verification are
    /// exactly as enforced as an interactive call would get - a grant only ever removes the
    /// pause between steps, never a safety check inside them.
    ///
    /// `pub` so `tests/mcp.rs` can exercise the chain directly against a task set up with a
    /// specific `fake_outcome` (via `Store::revise`, bypassing MCP entirely for setup).
    pub fn auto_authorize_chain(
        &self,
        principal: &str,
        backend: &str,
        mut detail: board::Detail,
        request_id: &str,
    ) -> Result<(board::Detail, Vec<Value>), BackendError> {
        let mut log = Vec::new();
        let repository_identity = Self::repository_identity(&detail.repository)?;

        if detail.status == "awaiting_approval" {
            let Some(grant) = self.covering_grant(
                principal,
                &repository_identity,
                backend,
                Operation::Submit,
                &detail.id,
            )?
            else {
                return Ok((detail, log));
            };
            let grant = self.grants()?.reserve_submit(
                &grant.id,
                &detail.id,
                detail.revision,
                request_id,
            )?;
            let by = grant.label();
            let mut service = self.task_service()?;
            detail = match service.approve(
                &detail.id,
                detail.revision,
                &detail.assignment_digest.clone(),
                &by,
            ) {
                Ok(approved) => approved,
                Err(error) => {
                    self.grants()?
                        .release_submit(&grant.id, &detail.id, request_id)?;
                    return Err(error.into());
                }
            };
            self.use_grant(
                &grant,
                Operation::Submit,
                &detail.id,
                detail.revision,
                None,
                request_id,
            )?;
            log.push(json!({"operation": "approve", "stage": "assignment_approval", "authorized_operation": "submit", "handler": "TaskService::approve", "result_accepted": false, "grant_id": grant.id, "grant_version": grant.version}));
        }

        if detail.status == "approved" {
            let Some(grant) = self.covering_grant(
                principal,
                &repository_identity,
                backend,
                Operation::Resume,
                &detail.id,
            )?
            else {
                return Ok((detail, log));
            };
            self.use_grant(
                &grant,
                Operation::Resume,
                &detail.id,
                detail.revision,
                None,
                request_id,
            )?;
            if is_agent_backend(backend) {
                dispatch_run_in_background(self.root.clone(), detail.id.clone(), detail.revision);
                log.push(json!({
                    "operation": "resume",
                    "grant_id": grant.id,
                    "grant_version": grant.version,
                    "dispatched_async": true,
                    "note": "coding-agent execution queued through TaskService::run; poll conductor_get_task for actual stage receipts; no result accepted",
                }));
                return Ok((detail, log));
            }
            let mut service = self.task_service()?;
            let outcome = service.run(&detail.id, detail.revision)?;
            detail = outcome.detail;
            log.push(json!({"operation": "resume", "grant_id": grant.id, "grant_version": grant.version}));
            if !outcome.succeeded {
                return Ok((detail, log));
            }
        }

        if detail.status == "awaiting_review"
            && let Some(evidence_digest) = detail.evidence_digest.clone()
            && let Some(grant) = self.covering_grant(
                principal,
                &repository_identity,
                backend,
                Operation::Approve,
                &detail.id,
            )?
        {
            let by = grant.label();
            let mut service = self.task_service()?;
            detail = service.review(&detail.id, detail.revision, &evidence_digest, &by, true)?;
            self.use_grant(
                &grant,
                Operation::Approve,
                &detail.id,
                detail.revision,
                Some(&evidence_digest),
                request_id,
            )?;
            log.push(json!({"operation": "review_accept", "grant_id": grant.id, "grant_version": grant.version}));
        }

        Ok((detail, log))
    }
}

fn decision_stage(status: &str) -> Option<String> {
    match status {
        "awaiting_approval" => Some("assignment_approval".into()),
        "awaiting_review" => Some("result_review".into()),
        _ => None,
    }
}

impl ConductorBackend for LocalBackend {
    async fn conductor_capabilities(&self) -> Result<CapabilitiesResponse, BackendError> {
        Ok(CapabilitiesResponse {
            protocol: "mcp".to_string(),
            adapter_version: "m12.0-phase6".to_string(),
            engines: vec![
                "local-changes-v1".to_string(),
                "fake-process-v1".to_string(),
                "cursor-agent".to_string(),
                "codex".to_string(),
                "workflow-v1".to_string(),
            ],
            tools: TOOL_NAMES.iter().map(|s| s.to_string()).collect(),
            scheduling_supported: false,
            standing_delegation_supported: true,
            notes: vec![
                "workflow-v1 profiles can include immutable versioned specialist agentlets. Inspect task.workflow.agentlets, pending_agentlet_gates and each step.result.agentlet_result for approved roles and typed findings. Required reviewers must complete on the current candidate before result review. Definitions and local bindings are CLI-administered; remote clients cannot change their instructions or capabilities.".to_string(),
                "workflow-v1 takes workflow_profile (or agent_preset as a profile-name alias for older clients) plus objective/acceptance: the pinned local inference model chooses among admin-approved CLI workers and receives host-captured context/evidence between bounded steps. Final result review is separate. Inspect task.workflow for decisions, child task IDs and event receipts.".to_string(),
                "cursor-agent/codex require agent_preset on conductor_submit_task (agent-preset-create, CLI-only) - a remote caller names a preset, never a raw executable path, verify command, or file scope.".to_string(),
                "cursor-agent/codex execution is dispatched asynchronously: conductor_submit_task/conductor_resume return as soon as the run is started, not when it finishes - poll conductor_get_task/conductor_get_execution for completion.".to_string(),
                "prefer conductor_get_evidence_summary over conductor_get_task/conductor_get_evidence for a bounded, actionable result - it reports has_verifier_output=false explicitly rather than inventing a summary when nothing has run yet, and names the artifact labels conductor_get_artifact can page through for the full log.".to_string(),
                "standing grants (CLI-managed only, never created/listed/modified through MCP) can auto-authorize submit/approve/resume/review-accept; without a covering grant, every decision still needs an explicit interactive call with a caller-asserted principal that is not yet cryptographically verified over stdio local mode.".to_string(),
            ],
        })
    }

    async fn conductor_list_repositories(
        &self,
        req: RequestIdOnly,
    ) -> Result<ListRepositoriesResponse, BackendError> {
        let store = Store::open(&self.root)?;
        let repositories = store
            .repositories()?
            .into_iter()
            // M12.0.1 Phase 1: omit, don't error - a list operation out-of-scope record
            // is simply not this client's to see, not a refused request.
            .filter(|(_, repo)| {
                self.ensure_repository_identity_in_scope(&repo.identity)
                    .is_ok()
            })
            .map(|(id, repo)| json!({"id": id, "repository": repo}))
            .collect();
        Ok(ListRepositoriesResponse {
            repositories,
            request_id: request_id(req.request_id),
        })
    }

    async fn conductor_list_tasks(
        &self,
        req: RequestIdOnly,
    ) -> Result<ListTasksResponse, BackendError> {
        let service = self.task_service()?;
        let tasks = service
            .board()?
            .into_iter()
            // M12.0.1 Phase 1: `Row` carries no repository identity of its own, so each
            // one needs a fresh `detail()` to resolve it against - the same "omit, don't
            // error" rule as `conductor_list_repositories`. Only pays that cost at all
            // when this enrollment actually has a non-empty scope to check.
            .filter(|row| {
                self.repository_scope.is_empty()
                    || service
                        .detail(&row.id, Some(0))
                        .is_ok_and(|detail| self.ensure_task_repository_in_scope(&detail).is_ok())
            })
            .map(|row| serde_json::to_value(row).unwrap_or(Value::Null))
            .collect();
        Ok(ListTasksResponse {
            tasks,
            request_id: request_id(req.request_id),
        })
    }

    async fn conductor_get_task(&self, req: GetTaskRequest) -> Result<TaskResponse, BackendError> {
        let service = self.task_service()?;
        let detail = service.detail(&req.task_id, req.preview_bytes)?;
        self.ensure_task_repository_in_scope(&detail)?;
        let metadata = self.metadata_for(&detail.id)?;
        Ok(TaskResponse {
            task: detail_value(&detail)?,
            metadata,
            request_id: request_id(req.request_id),
        })
    }

    async fn conductor_propose_task(
        &self,
        req: ProposeTaskRequest,
    ) -> Result<ProposeTaskResponse, BackendError> {
        let rid = request_id(req.request_id);
        let spec: orchestration::Spec = match serde_json::from_value(req.spec) {
            Ok(spec) => spec,
            Err(error) => {
                return Ok(ProposeTaskResponse {
                    valid: false,
                    repository_identity: None,
                    errors: vec![format!("spec does not match orchestration::Spec: {error}")],
                    request_id: rid,
                });
            }
        };
        let mut errors = Vec::new();
        if let Err(error) = orchestration::validate_spec_bounds(&spec) {
            errors.push(error.to_string());
        }
        if let Err(error) = orchestration::validate_verifier(&spec) {
            errors.push(error.to_string());
        }
        let repository_identity = match Repository::inspect(&spec.repository) {
            Ok(repo) => {
                // M12.0.1 Phase 1: this is pre-creation validation, not task-existence
                // disclosure - the caller already named this exact repository, so a
                // direct "not in scope" error leaks nothing it doesn't already know.
                if let Err(error) = self.ensure_repository_identity_in_scope(&repo.identity) {
                    errors.push(error.to_string());
                }
                Some(repo.identity)
            }
            Err(error) => {
                errors.push(error.to_string());
                None
            }
        };
        Ok(ProposeTaskResponse {
            valid: errors.is_empty(),
            repository_identity,
            errors,
            request_id: rid,
        })
    }

    async fn conductor_submit_task(
        &self,
        req: SubmitTaskRequest,
    ) -> Result<SubmitTaskResponse, BackendError> {
        let rid = request_id(req.request_id.clone());
        if let (Some(workflow_id), Some(key)) = (&req.workflow_id, &req.idempotency_key)
            && let Some(existing_id) = self.idempotency_lookup(&req.principal, workflow_id, key)?
        {
            let service = self.task_service()?;
            let detail = service.detail(&existing_id, None)?;
            self.ensure_task_repository_in_scope(&detail)?;
            return Ok(SubmitTaskResponse {
                task: detail_value(&detail)?,
                replayed: true,
                authorization: Vec::new(),
                metadata: self.metadata_for(&existing_id)?,
                request_id: rid,
            });
        }
        if let Some(prior) = &req.follow_up_of {
            // M12.0.1 Phase 1: "for follow-up tasks, check both the target repository and
            // any referenced parent task" - an out-of-scope parent is reported identically
            // to a nonexistent one (same message, same error shape), never disclosing
            // that it exists but belongs to a repository this client can't see.
            let parent = self.task_service()?.detail(prior, Some(0)).map_err(|_| {
                BackendError::invalid_params_with(
                    format!("follow_up_of names an unknown task: {prior}"),
                    json!({"kind": "not_found"}),
                )
            })?;
            if self.ensure_task_repository_in_scope(&parent).is_err() {
                return Err(BackendError::invalid_params_with(
                    format!("follow_up_of names an unknown task: {prior}"),
                    json!({"kind": "not_found"}),
                ));
            }
        }
        let interactive_only = req.requested_execution_mode.as_deref() == Some("interactive");
        match req.backend.as_str() {
            "workflow-v1" => {
                let profile_name = req.workflow_profile.as_deref().or(req.agent_preset.as_deref()).ok_or_else(|| {
                    BackendError::invalid_params("workflow_profile (or agent_preset profile alias) is required for workflow-v1")
                })?;
                let objective = req
                    .objective
                    .clone()
                    .ok_or_else(|| BackendError::invalid_params("objective is required"))?;
                let acceptance = req
                    .acceptance
                    .clone()
                    .ok_or_else(|| BackendError::invalid_params("acceptance is required"))?;
                let mut workflows = crate::workflow::Workflows::open(&self.root)?;
                let profile = workflows
                    .profile(profile_name)
                    .map_err(|_| BackendError::not_found("unknown workflow profile"))?;
                self.ensure_repository_identity_in_scope(&profile.repository.identity)?;
                let mut replayed = false;
                let task = if let (Some(workflow), Some(key)) =
                    (&req.workflow_id, &req.idempotency_key)
                {
                    let id = deterministic_task_id(&req.principal, workflow, key);
                    match workflows.create_with_id(profile_name, objective, acceptance, id.clone())
                    {
                        Ok(task) => task,
                        Err(error) => match workflows.get(&id) {
                            Ok(task) => {
                                replayed = true;
                                task
                            }
                            Err(_) => return Err(error.into()),
                        },
                    }
                } else {
                    workflows.create(profile_name, objective, acceptance)?
                };
                self.record_workflow_origin(&req, &task.id)?;
                let detail = self.task_service()?.detail(&task.id, None)?;
                self.ensure_task_repository_in_scope(&detail)?;
                let (detail, authorization) = if interactive_only || replayed {
                    (detail, Vec::new())
                } else {
                    self.auto_authorize_chain(&req.principal, &req.backend, detail, &rid)?
                };
                Ok(SubmitTaskResponse {
                    task: detail_value(&detail)?,
                    replayed,
                    authorization,
                    metadata: self.metadata_for(&task.id)?,
                    request_id: rid,
                })
            }
            "local-changes-v1" => {
                let spec: orchestration::Spec =
                    serde_json::from_value(req.spec.ok_or_else(|| {
                        BackendError::invalid_params(
                            "spec is required for backend local-changes-v1",
                        )
                    })?)
                    .map_err(|e| BackendError::invalid_params(format!("invalid spec: {e}")))?;
                let config: Config = serde_json::from_value(req.config.ok_or_else(|| {
                    BackendError::invalid_params("config is required for backend local-changes-v1")
                })?)
                .map_err(|e| BackendError::invalid_params(format!("invalid config: {e}")))?;
                // M12.0.1 Phase 1: resolve and check *before* creating anything - the
                // authoritative, resolved identity (`Repository::inspect`), never the
                // caller-supplied path string taken on faith.
                let repository_identity = Repository::inspect(&spec.repository)?.identity;
                self.ensure_repository_identity_in_scope(&repository_identity)?;
                let task_id = match (&req.workflow_id, &req.idempotency_key) {
                    (Some(workflow_id), Some(key)) => {
                        deterministic_task_id(&req.principal, workflow_id, key)
                    }
                    _ => uuid::Uuid::new_v4().to_string(),
                };
                let launcher = std::env::current_exe().map_err(anyhow::Error::from)?;
                let mut orchestrator = Orchestrator::open(&self.root)?;
                let task = orchestrator.create_with_id(task_id.clone(), spec, config, &launcher)?;
                if let (Some(workflow_id), Some(key)) = (&req.workflow_id, &req.idempotency_key) {
                    self.idempotency_record(&req.principal, workflow_id, key, &task_id)?;
                }
                self.record_metadata(
                    &task_id,
                    req.workflow_id.as_deref(),
                    req.source_system.as_deref(),
                    req.source_record_reference.as_deref(),
                    req.correlation_id.as_deref(),
                    req.follow_up_of.as_deref(),
                    &req.principal,
                )?;
                let service = self.task_service()?;
                let detail = service.detail(&task.id, None)?;
                let (detail, authorization) = if interactive_only {
                    (detail, Vec::new())
                } else {
                    self.auto_authorize_chain(&req.principal, &req.backend, detail, &rid)?
                };
                Ok(SubmitTaskResponse {
                    task: detail_value(&detail)?,
                    replayed: false,
                    authorization,
                    metadata: self.metadata_for(&task_id)?,
                    request_id: rid,
                })
            }
            "fake-process-v1" => {
                let repository_id = req.repository_id.ok_or_else(|| {
                    BackendError::invalid_params(
                        "repository_id is required for backend fake-process-v1",
                    )
                })?;
                let objective = req.objective.ok_or_else(|| {
                    BackendError::invalid_params(
                        "objective is required for backend fake-process-v1",
                    )
                })?;
                let acceptance = req.acceptance.ok_or_else(|| {
                    BackendError::invalid_params(
                        "acceptance is required for backend fake-process-v1",
                    )
                })?;
                let contents = req.contents.unwrap_or_default();
                let budget = req.budget.unwrap_or(4096);
                // M12.0.1 Phase 1: `repository_id` is a `Store` registration id, not a
                // path - resolve the actual registered `Repository` to get its
                // authoritative identity before checking scope, never trusting the
                // caller's own id string as proof of anything.
                let repository_identity = Store::open(&self.root)?
                    .repository(&repository_id)?
                    .identity;
                self.ensure_repository_identity_in_scope(&repository_identity)?;
                let executable = std::env::current_exe().map_err(anyhow::Error::from)?;
                let execution_policy = ExecutionPolicy::new(&executable)?;
                let mut service = self.task_service()?;
                let detail = service.create_fake(
                    &repository_id,
                    objective,
                    acceptance,
                    contents,
                    budget,
                    execution_policy,
                )?;
                if let (Some(workflow_id), Some(key)) = (&req.workflow_id, &req.idempotency_key) {
                    self.idempotency_record(&req.principal, workflow_id, key, &detail.id)?;
                }
                self.record_metadata(
                    &detail.id,
                    req.workflow_id.as_deref(),
                    req.source_system.as_deref(),
                    req.source_record_reference.as_deref(),
                    req.correlation_id.as_deref(),
                    req.follow_up_of.as_deref(),
                    &req.principal,
                )?;
                let task_id = detail.id.clone();
                let (detail, authorization) = if interactive_only {
                    (detail, Vec::new())
                } else {
                    self.auto_authorize_chain(&req.principal, &req.backend, detail, &rid)?
                };
                Ok(SubmitTaskResponse {
                    task: detail_value(&detail)?,
                    replayed: false,
                    authorization,
                    metadata: self.metadata_for(&task_id)?,
                    request_id: rid,
                })
            }
            "cursor-agent" | "codex" => {
                let preset_name = req.agent_preset.ok_or_else(|| {
                    BackendError::invalid_params(format!(
                        "agent_preset is required for backend {}",
                        req.backend
                    ))
                })?;
                let objective = req.objective.ok_or_else(|| {
                    BackendError::invalid_params("objective is required for this backend")
                })?;
                let acceptance = req.acceptance.ok_or_else(|| {
                    BackendError::invalid_params("acceptance is required for this backend")
                })?;
                let preset = crate::agent_preset::AgentPresets::open(&self.root)?
                    .get(&preset_name)
                    .map_err(|_| {
                        BackendError::not_found(format!("unknown agent preset '{preset_name}'"))
                    })?;
                if preset.backend.target() != req.backend {
                    return Err(BackendError::invalid_params(format!(
                        "agent preset '{preset_name}' is configured for backend '{}', not '{}'",
                        preset.backend.target(),
                        req.backend
                    )));
                }
                // M12.0.1 Phase 1: the preset's own repository is admin-configured, not
                // caller-supplied - but the enrollment's scope must still gate it. A
                // narrow enrollment cannot be widened just because an admin-approved
                // preset happens to target a repository outside it.
                let repository_identity = Repository::inspect(&preset.repository)?.identity;
                self.ensure_repository_identity_in_scope(&repository_identity)?;
                let spec = preset.spec(objective, acceptance);
                let mut agents = crate::agents::Agents::open(&self.root)?;
                let task = agents.create(spec, preset.backend.clone())?;
                if let (Some(workflow_id), Some(key)) = (&req.workflow_id, &req.idempotency_key) {
                    self.idempotency_record(&req.principal, workflow_id, key, &task.id)?;
                }
                self.record_metadata(
                    &task.id,
                    req.workflow_id.as_deref(),
                    req.source_system.as_deref(),
                    req.source_record_reference.as_deref(),
                    req.correlation_id.as_deref(),
                    req.follow_up_of.as_deref(),
                    &req.principal,
                )?;
                let task_id = task.id.clone();
                let service = self.task_service()?;
                let detail = service.detail(&task.id, None)?;
                let (detail, authorization) = if interactive_only {
                    (detail, Vec::new())
                } else {
                    self.auto_authorize_chain(&req.principal, &req.backend, detail, &rid)?
                };
                Ok(SubmitTaskResponse {
                    task: detail_value(&detail)?,
                    replayed: false,
                    authorization,
                    metadata: self.metadata_for(&task_id)?,
                    request_id: rid,
                })
            }
            other => Err(BackendError::invalid_params_with(
                format!(
                    "unsupported backend '{other}' in this milestone; use local-changes-v1, fake-process-v1, cursor-agent, or codex"
                ),
                json!({"kind": "unsupported_backend"}),
            )),
        }
    }

    async fn conductor_schedule_task(
        &self,
        req: RequestIdOnly,
    ) -> Result<ScheduleTaskResponse, BackendError> {
        Ok(ScheduleTaskResponse {
            supported: false,
            explanation: "Durable scheduling is not implemented in this milestone. Own the schedule \
            in your own orchestrator and call conductor_submit_task when the task should actually run."
                .to_string(),
            request_id: request_id(req.request_id),
        })
    }

    async fn conductor_get_execution(
        &self,
        req: GetTaskRequest,
    ) -> Result<TaskResponse, BackendError> {
        self.conductor_get_task(req).await
    }

    async fn conductor_get_evidence(
        &self,
        req: GetTaskRequest,
    ) -> Result<TaskResponse, BackendError> {
        self.conductor_get_task(req).await
    }

    async fn conductor_get_approval_requirements(
        &self,
        req: TaskIdRequest,
    ) -> Result<ApprovalRequirementsResponse, BackendError> {
        let service = self.task_service()?;
        let detail = service.detail(&req.task_id, Some(0))?;
        self.ensure_task_repository_in_scope(&detail)?;
        let (approval_required, action, operation, required_digest, explanation) =
            match detail.status.as_str() {
                "awaiting_approval" => (
                    true,
                    Some("approve".to_string()),
                    Some(Operation::Submit),
                    Some(detail.assignment_digest.clone()),
                    "task is awaiting initial approval of the proposed assignment".to_string(),
                ),
                "awaiting_review" => (
                    true,
                    Some("review".to_string()),
                    Some(Operation::Approve),
                    detail.evidence_digest.clone(),
                    "task has produced a result and is awaiting accept/reject review".to_string(),
                ),
                other => (
                    false,
                    None,
                    None,
                    None,
                    format!("task is not at a decision point (status: {other})"),
                ),
            };
        let required_scope = match operation {
            Some(operation) => {
                let repository_identity = Self::repository_identity(&detail.repository)?;
                Some(json!({
                    "repository_identity": repository_identity,
                    "backend": detail.backend,
                    "operation": operation.as_str(),
                }))
            }
            None => None,
        };
        let covered_by_grant = match (&req.principal, operation, &required_scope) {
            (Some(principal), Some(operation), Some(scope)) => self
                .covering_grant(
                    principal,
                    scope["repository_identity"].as_str().unwrap(),
                    &detail.backend,
                    operation,
                    &detail.id,
                )?
                .map(|g| json!({"grant_id": g.id, "grant_version": g.version})),
            _ => None,
        };
        let workflow_id = self.metadata_for(&detail.id)?["workflow_id"]
            .as_str()
            .map(str::to_string);
        Ok(ApprovalRequirementsResponse {
            task_id: detail.id.clone(),
            workflow_id,
            approval_required,
            decision_stage: decision_stage(&detail.status),
            action,
            required_digest,
            explanation,
            required_scope,
            covered_by_grant,
            resumption_reference: json!({"task_id": detail.id, "revision": detail.revision}),
            authorization_note: AUTHORIZATION_NOTE.to_string(),
            request_id: request_id(req.request_id),
        })
    }

    async fn conductor_approve(
        &self,
        req: DecisionRequest,
    ) -> Result<DecisionResponse, BackendError> {
        let rid = request_id(req.request_id);
        let mut service = self.task_service()?;
        let current = service.detail(&req.task_id, Some(0))?;
        self.ensure_task_repository_in_scope(&current)?;
        let repository_identity = Self::repository_identity(&current.repository)?;
        let operation = match current.status.as_str() {
            "awaiting_approval" => Operation::Submit,
            "awaiting_review" => Operation::Approve,
            other => {
                return Err(BackendError::invalid_params_with(
                    format!("task is not at a decision point (status: {other})"),
                    json!({"kind": "conflict"}),
                ));
            }
        };
        let grant = self.covering_grant(
            &req.principal,
            &repository_identity,
            &current.backend,
            operation,
            &current.id,
        )?;
        if grant.is_none() {
            self.ensure_self_service_or_owned(&req.principal, &req.task_id)?;
        }
        let grant = if operation == Operation::Submit {
            grant
                .map(|g| {
                    self.grants()?
                        .reserve_submit(&g.id, &current.id, current.revision, &rid)
                        .map_err(BackendError::from)
                })
                .transpose()?
        } else {
            grant
        };
        let by = grant
            .as_ref()
            .map_or_else(|| req.principal.clone(), Grant::label);
        let (detail, action_taken) = match current.status.as_str() {
            "awaiting_approval" => (
                match service.approve(&req.task_id, req.revision, &req.digest, &by) {
                    Ok(detail) => detail,
                    Err(error) => {
                        if let Some(g) = &grant {
                            self.grants()?.release_submit(&g.id, &current.id, &rid)?;
                        }
                        return Err(error.into());
                    }
                },
                "approve",
            ),
            "awaiting_review" => (
                service.review(&req.task_id, req.revision, &req.digest, &by, true)?,
                "review_accept",
            ),
            other => unreachable!("status re-checked above: {other}"),
        };
        if let Some(grant) = &grant {
            self.use_grant(
                grant,
                operation,
                &req.task_id,
                detail.revision,
                Some(&req.digest),
                &rid,
            )?;
        }
        Ok(DecisionResponse {
            task: detail_value(&detail)?,
            action_taken: action_taken.to_string(),
            decision_stage: decision_stage(&current.status),
            authorization: grant.map(|g| json!({"grant_id": g.id, "grant_version": g.version})),
            request_id: rid,
        })
    }

    async fn conductor_reject(
        &self,
        req: DecisionRequest,
    ) -> Result<DecisionResponse, BackendError> {
        let rid = request_id(req.request_id);
        let mut service = self.task_service()?;
        let current = service.detail(&req.task_id, Some(0))?;
        self.ensure_task_repository_in_scope(&current)?;
        if !matches!(
            current.status.as_str(),
            "awaiting_approval" | "awaiting_review"
        ) {
            return Err(BackendError::invalid_params_with(
                format!(
                    "task is not at a decision point (status: {})",
                    current.status
                ),
                json!({"kind": "conflict"}),
            ));
        }
        let repository_identity = Self::repository_identity(&current.repository)?;
        let grant = self.covering_grant(
            &req.principal,
            &repository_identity,
            &current.backend,
            Operation::Reject,
            &current.id,
        )?;
        if grant.is_none() {
            self.ensure_self_service_or_owned(&req.principal, &req.task_id)?;
        }
        let by = grant
            .as_ref()
            .map_or_else(|| req.principal.clone(), Grant::label);
        let (detail, action_taken) = match current.status.as_str() {
            "awaiting_approval" => (
                service.cancel(&req.task_id, req.revision, Some(&by))?,
                "cancel",
            ),
            "awaiting_review" => (
                service.review(&req.task_id, req.revision, &req.digest, &by, false)?,
                "review_reject",
            ),
            other => unreachable!("status re-checked above: {other}"),
        };
        if let Some(grant) = &grant {
            self.use_grant(
                grant,
                Operation::Reject,
                &req.task_id,
                detail.revision,
                Some(&req.digest),
                &rid,
            )?;
        }
        Ok(DecisionResponse {
            task: detail_value(&detail)?,
            action_taken: action_taken.to_string(),
            decision_stage: decision_stage(&current.status),
            authorization: grant.map(|g| json!({"grant_id": g.id, "grant_version": g.version})),
            request_id: rid,
        })
    }

    async fn conductor_resume(&self, req: ResumeRequest) -> Result<ResumeResponse, BackendError> {
        let rid = request_id(req.request_id);
        let mut service = self.task_service()?;
        let current = service.detail(&req.task_id, Some(0))?;
        self.ensure_task_repository_in_scope(&current)?;
        match current.status.as_str() {
            "approved" => {
                // Reject stale async requests before recording grant use or reporting
                // dispatch; the background worker's conflict cannot reach this caller.
                if current.revision != req.revision {
                    return Err(BackendError::conflict("revision conflict"));
                }
                let grant = match &req.principal {
                    Some(principal) => {
                        let repository_identity = Self::repository_identity(&current.repository)?;
                        self.covering_grant(
                            principal,
                            &repository_identity,
                            &current.backend,
                            Operation::Resume,
                            &current.id,
                        )?
                    }
                    None => None,
                };
                if grant.is_none()
                    && let Some(principal) = &req.principal
                {
                    self.ensure_self_service_or_owned(principal, &req.task_id)?;
                }
                if is_agent_backend(&current.backend) {
                    if let Some(grant) = &grant {
                        self.use_grant(
                            grant,
                            Operation::Resume,
                            &req.task_id,
                            req.revision,
                            None,
                            &rid,
                        )?;
                    }
                    dispatch_run_in_background(
                        self.root.clone(),
                        req.task_id.clone(),
                        req.revision,
                    );
                    return Ok(ResumeResponse {
                        task: detail_value(&current)?,
                        action_taken: "run".to_string(),
                        succeeded: None,
                        authorization: grant
                            .map(|g| json!({"grant_id": g.id, "grant_version": g.version})),
                        request_id: rid,
                    });
                }
                let outcome = service.run(&req.task_id, req.revision)?;
                if let Some(grant) = &grant {
                    self.use_grant(
                        grant,
                        Operation::Resume,
                        &req.task_id,
                        outcome.detail.revision,
                        None,
                        &rid,
                    )?;
                }
                Ok(ResumeResponse {
                    task: detail_value(&outcome.detail)?,
                    action_taken: "run".to_string(),
                    succeeded: Some(outcome.succeeded),
                    authorization: grant
                        .map(|g| json!({"grant_id": g.id, "grant_version": g.version})),
                    request_id: rid,
                })
            }
            "unknown" | "needs_work" => {
                let detail = service.reconcile(&req.task_id, req.revision)?;
                Ok(ResumeResponse {
                    task: detail_value(&detail)?,
                    action_taken: "reconcile".to_string(),
                    succeeded: None,
                    authorization: None,
                    request_id: rid,
                })
            }
            other => Err(BackendError::invalid_params_with(
                format!("nothing to resume (status: {other})"),
                json!({"kind": "conflict"}),
            )),
        }
    }

    async fn conductor_cancel_task(
        &self,
        req: CancelTaskRequest,
    ) -> Result<DecisionResponse, BackendError> {
        let rid = request_id(req.request_id);
        let mut service = self.task_service()?;
        // M12.0.1 Phase 1: resolved unconditionally now (previously only when `principal`
        // was supplied) so the repository-scope check below runs regardless - an edge
        // (trusted) caller always supplies a principal via `SetPrincipal` anyway, so this
        // changes nothing observable for that path; stdio local mode's empty scope makes
        // the check itself a no-op either way.
        let current = service.detail(&req.task_id, Some(0))?;
        self.ensure_task_repository_in_scope(&current)?;
        let grant = match &req.principal {
            Some(principal) => {
                let repository_identity = Self::repository_identity(&current.repository)?;
                self.covering_grant(
                    principal,
                    &repository_identity,
                    &current.backend,
                    Operation::Cancel,
                    &current.id,
                )?
            }
            None => None,
        };
        if grant.is_none()
            && let Some(principal) = &req.principal
        {
            self.ensure_self_service_or_owned(principal, &req.task_id)?;
        }
        let by = grant
            .as_ref()
            .map(Grant::label)
            .or_else(|| req.principal.clone());
        let detail = service.cancel(&req.task_id, req.revision, by.as_deref())?;
        if let Some(grant) = &grant {
            self.use_grant(
                grant,
                Operation::Cancel,
                &req.task_id,
                detail.revision,
                None,
                &rid,
            )?;
        }
        Ok(DecisionResponse {
            task: detail_value(&detail)?,
            action_taken: "cancel".to_string(),
            decision_stage: None,
            authorization: grant.map(|g| json!({"grant_id": g.id, "grant_version": g.version})),
            request_id: rid,
        })
    }

    async fn conductor_global_stop_status(
        &self,
        req: RequestIdOnly,
    ) -> Result<StopStatusResponse, BackendError> {
        let stop = GlobalStop::open(&self.root)?;
        let state = stop.state()?;
        Ok(StopStatusResponse {
            stopped: state.stopped,
            by: state.by,
            reason: state.reason,
            at: state.at,
            request_id: request_id(req.request_id),
        })
    }

    async fn conductor_get_evidence_summary(
        &self,
        req: EvidenceSummaryRequest,
    ) -> Result<EvidenceSummaryResponse, BackendError> {
        let rid = request_id(req.request_id);
        let service = self.task_service()?;
        let detail = service.detail(&req.task_id, Some(0))?;
        self.ensure_task_repository_in_scope(&detail)?;
        let attempt = detail.attempts.last();
        let execution_id = attempt.map(|a| a.id.clone());
        let verification_outcome = attempt.and_then(|a| a.outcome.clone());
        let error_summary = attempt.and_then(|a| a.error.clone());
        let failing_check = if matches!(
            detail.status.as_str(),
            "failed" | "needs_work" | "cancelled" | "rejected"
        ) {
            error_summary
                .clone()
                .or_else(|| verification_outcome.clone())
        } else {
            None
        };

        let available_artifacts: Vec<String> = KNOWN_ARTIFACT_LABELS
            .iter()
            .filter(|label| service.artifact_path(&req.task_id, label).is_ok())
            .map(|label| label.to_string())
            .collect();
        let has_verifier_output = available_artifacts
            .iter()
            .any(|a| a.starts_with("verifier"));

        let requested_excerpt = req
            .log_excerpt_bytes
            .unwrap_or(DEFAULT_LOG_EXCERPT_BYTES)
            .min(MAX_ARTIFACT_CHUNK_BYTES);
        let mut log_excerpt = None;
        let mut log_excerpt_label = None;
        let mut log_excerpt_truncated = false;
        if requested_excerpt > 0 {
            for label in PREFERRED_LOG_EXCERPT_ORDER {
                let Ok((path, total_bytes, capture_truncated)) =
                    service.artifact_path(&req.task_id, label)
                else {
                    continue;
                };
                let Ok(bytes) = std::fs::read(&path) else {
                    continue;
                };
                if bytes.is_empty() {
                    continue;
                }
                let capped = &bytes[..bytes.len().min(requested_excerpt)];
                log_excerpt = Some(String::from_utf8_lossy(capped).into_owned());
                log_excerpt_label = Some(label.to_string());
                log_excerpt_truncated = capture_truncated || (capped.len() as u64) < total_bytes;
                break;
            }
        }

        Ok(EvidenceSummaryResponse {
            task_id: detail.id.clone(),
            execution_id,
            status: detail.status.clone(),
            backend: detail.backend.clone(),
            verification_outcome,
            failing_check,
            error_summary,
            log_excerpt,
            log_excerpt_label,
            log_excerpt_truncated,
            evidence_digest: detail.evidence_digest.clone(),
            available_artifacts,
            has_verifier_output,
            progress: detail
                .progress
                .as_ref()
                .map(serde_json::to_value)
                .transpose()
                .map_err(anyhow::Error::from)?,
            request_id: rid,
        })
    }

    async fn conductor_get_artifact(
        &self,
        req: GetArtifactRequest,
    ) -> Result<GetArtifactResponse, BackendError> {
        let rid = request_id(req.request_id);
        let service = self.task_service()?;
        // M12.0.1 Phase 1: this method previously went straight to `artifact_path`
        // without ever resolving the task's repository at all - the one place this
        // phase's own test matrix explicitly calls out ("Client A cannot retrieve
        // another repository's evidence or artifacts").
        let detail = service.detail(&req.task_id, Some(0))?;
        self.ensure_task_repository_in_scope(&detail)?;
        let (path, total_bytes, capture_truncated) =
            service.artifact_path(&req.task_id, &req.artifact)?;
        let bytes = std::fs::read(&path)
            .map_err(|e| BackendError::internal(format!("read artifact: {e}")))?;
        let offset = req.offset.unwrap_or(0);
        let max_bytes = req
            .max_bytes
            .unwrap_or(DEFAULT_ARTIFACT_CHUNK_BYTES)
            .min(MAX_ARTIFACT_CHUNK_BYTES);
        let start = (offset as usize).min(bytes.len());
        let end = start.saturating_add(max_bytes).min(bytes.len());
        let chunk = &bytes[start..end];
        Ok(GetArtifactResponse {
            task_id: req.task_id,
            artifact: req.artifact,
            offset: start as u64,
            shown_bytes: chunk.len(),
            total_bytes,
            capture_truncated,
            has_more: (end as u64) < bytes.len() as u64,
            text: String::from_utf8_lossy(chunk).into_owned(),
            request_id: rid,
        })
    }
}

/// The `rmcp` stdio adapter: a thin wrapper over one `LocalBackend`. Every `#[tool]` method
/// is a one-line forward — see this module's doc comment for why the actual logic lives in
/// `LocalBackend` instead.
#[derive(Debug, Clone)]
pub struct McpServer {
    backend: LocalBackend,
}

impl McpServer {
    pub fn open(root: PathBuf) -> Result<Self> {
        Ok(Self {
            backend: LocalBackend::open(root)?,
        })
    }

    /// Forwards to `LocalBackend::auto_authorize_chain` — kept on `McpServer` too since
    /// `tests/mcp.rs` exercises it directly against a server opened the normal way.
    pub fn auto_authorize_chain(
        &self,
        principal: &str,
        backend: &str,
        detail: board::Detail,
        request_id: &str,
    ) -> Result<(board::Detail, Vec<Value>), McpError> {
        self.backend
            .auto_authorize_chain(principal, backend, detail, request_id)
            .map_err(to_mcp_error)
    }
}

#[tool_router]
impl McpServer {
    #[tool(
        description = "Describe what this adapter actually supports: the tool list, which \
    backends are reachable, whether durable scheduling exists (it does not, in this phase), \
    and whether standing delegation exists (it does, since M11 Phase 2). Call this first."
    )]
    pub async fn conductor_capabilities(&self) -> Result<Json<CapabilitiesResponse>, McpError> {
        self.backend
            .conductor_capabilities()
            .await
            .map(Json)
            .map_err(to_mcp_error)
    }

    #[tool(
        description = "List every repository this state root has registered (id + the \
    inspected Repository record: checkout, identity, remotes, base, branch)."
    )]
    pub async fn conductor_list_repositories(
        &self,
        Parameters(req): Parameters<RequestIdOnly>,
    ) -> Result<Json<ListRepositoriesResponse>, McpError> {
        self.backend
            .conductor_list_repositories(req)
            .await
            .map(Json)
            .map_err(to_mcp_error)
    }

    #[tool(
        description = "List every task across all three engines (fake, local-model \
    orchestration, external agent) as the existing CLI `board`/HTTP `GET /api/tasks` already \
    show them: id, kind, status, backend, objective, review state."
    )]
    pub async fn conductor_list_tasks(
        &self,
        Parameters(req): Parameters<RequestIdOnly>,
    ) -> Result<Json<ListTasksResponse>, McpError> {
        self.backend
            .conductor_list_tasks(req)
            .await
            .map(Json)
            .map_err(to_mcp_error)
    }

    #[tool(
        description = "Full detail for one task by exact id or unambiguous prefix: status, \
    repository/workspace, approval/review state, assignment_digest (needed to approve) and \
    evidence_digest (needed to review), dependencies and repository-drift conflict if any."
    )]
    pub async fn conductor_get_task(
        &self,
        Parameters(req): Parameters<GetTaskRequest>,
    ) -> Result<Json<TaskResponse>, McpError> {
        self.backend
            .conductor_get_task(req)
            .await
            .map(Json)
            .map_err(to_mcp_error)
    }

    #[tool(
        description = "Same underlying task record as conductor_get_task, labeled for the \
    execution/attempt view: this engine does not track a separate 'live execution' object, so \
    this returns the same attempts/previews conductor_get_task does."
    )]
    pub async fn conductor_get_execution(
        &self,
        Parameters(req): Parameters<GetTaskRequest>,
    ) -> Result<Json<TaskResponse>, McpError> {
        self.backend
            .conductor_get_execution(req)
            .await
            .map(Json)
            .map_err(to_mcp_error)
    }

    #[tool(
        description = "Same underlying task record as conductor_get_task, labeled for the \
    evidence view: evidence_digest, assignment_digest, and the attempt artifacts (diff/verifier \
    output previews) that evidence_digest is computed over. There is no separate evidence \
    store in this engine — this is a projection of the same canonical task snapshot."
    )]
    pub async fn conductor_get_evidence(
        &self,
        Parameters(req): Parameters<GetTaskRequest>,
    ) -> Result<Json<TaskResponse>, McpError> {
        self.backend
            .conductor_get_evidence(req)
            .await
            .map(Json)
            .map_err(to_mcp_error)
    }

    #[tool(
        description = "Dry-run validation of a local-changes-v1 orchestration::Spec — the \
    exact same bounds/verifier/repository checks Orchestrator::create_with_id runs before any \
    task is created — without creating anything. Use this before conductor_submit_task to get \
    an actionable rejection reason instead of a failed submission."
    )]
    pub async fn conductor_propose_task(
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
        description = "Create a task. backend=\"local-changes-v1\" creates a real \
    Orchestrator task from spec+config (validated exactly like conductor_propose_task, then \
    dispatched through the unchanged engine); backend=\"fake-process-v1\" creates a \
    deterministic fixture task for testing. Never auto-approves or auto-runs — call \
    conductor_approve next. Supply workflow_id+idempotency_key to make retries safe: a repeat \
    call with the same (principal, workflow_id, idempotency_key) returns the existing task \
    instead of creating a duplicate (crash-safe for local-changes-v1; best-effort, this-process-\
    lifetime-only for fake-process-v1 — see this module's doc comment)."
    )]
    pub async fn conductor_submit_task(
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
        description = "Durable/delayed task scheduling does not exist in this engine (M11 \
    Phase 0 confirmed zero prior scheduling infrastructure — no cron table, no timer thread). \
    This always returns supported=false; own scheduling in the external orchestrator and call \
    conductor_submit_task when ready, per this milestone's own fallback instruction."
    )]
    pub async fn conductor_schedule_task(
        &self,
        Parameters(req): Parameters<RequestIdOnly>,
    ) -> Result<Json<ScheduleTaskResponse>, McpError> {
        self.backend
            .conductor_schedule_task(req)
            .await
            .map(Json)
            .map_err(to_mcp_error)
    }

    #[tool(
        description = "What, if anything, a task is currently waiting on: approval \
    (assignment_digest) or review (evidence_digest), with the exact digest the matching \
    conductor_approve/conductor_reject call must echo back."
    )]
    pub async fn conductor_get_approval_requirements(
        &self,
        Parameters(req): Parameters<TaskIdRequest>,
    ) -> Result<Json<ApprovalRequirementsResponse>, McpError> {
        self.backend
            .conductor_get_approval_requirements(req)
            .await
            .map(Json)
            .map_err(to_mcp_error)
    }

    #[tool(
        description = "Say yes to whatever decision point the task is currently at: \
    approves a proposed assignment (status awaiting_approval) or accepts a produced result \
    (status awaiting_review). digest must be the assignment_digest or evidence_digest \
    conductor_get_task/conductor_get_approval_requirements just returned, exactly. principal \
    is not yet a verified identity over stdio local mode."
    )]
    pub async fn conductor_approve(
        &self,
        Parameters(req): Parameters<DecisionRequest>,
    ) -> Result<Json<DecisionResponse>, McpError> {
        self.backend
            .conductor_approve(req)
            .await
            .map(Json)
            .map_err(to_mcp_error)
    }

    #[tool(
        description = "Say no to whatever decision point the task is currently at: \
    refuses a proposed assignment (status awaiting_approval — this cancels it, there being no \
    separate 'decline before approval' state in this engine) or rejects a produced result \
    (status awaiting_review). digest must match exactly, same rule as conductor_approve."
    )]
    pub async fn conductor_reject(
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
        description = "Continue a task after authorization was obtained: runs an approved \
    task towards completion, or reconciles one left behind by a crashed/dead owner — chosen \
    from the task's current status, never replaying completed work (reconcile refuses a live \
    owner and never re-dispatches an already-finished attempt)."
    )]
    pub async fn conductor_resume(
        &self,
        Parameters(req): Parameters<ResumeRequest>,
    ) -> Result<Json<ResumeResponse>, McpError> {
        self.backend
            .conductor_resume(req)
            .await
            .map(Json)
            .map_err(to_mcp_error)
    }

    #[tool(
        description = "Cancel a task. principal is required for fake/Codex tasks and \
    ignored for orchestration/agent tasks — the same engine-asymmetric rule TaskService::cancel \
    already enforces."
    )]
    pub async fn conductor_cancel_task(
        &self,
        Parameters(req): Parameters<CancelTaskRequest>,
    ) -> Result<Json<DecisionResponse>, McpError> {
        self.backend
            .conductor_cancel_task(req)
            .await
            .map(Json)
            .map_err(to_mcp_error)
    }

    #[tool(
        description = "Whether a global stop is currently engaged across every engine, and \
    who engaged it. This adapter's conductor_submit_task/conductor_resume calls are refused \
    the same way the CLI/HTTP surfaces already are while a stop is engaged — this tool is for \
    checking, not for engaging/resuming (that stays an interactive, human-facing action in \
    this milestone)."
    )]
    pub async fn conductor_global_stop_status(
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
    attempt: status, backend, verification outcome, failing check (if any), a concise error \
    line, one inline bounded log excerpt, the evidence digest, and the artifact labels \
    conductor_get_artifact can drill into - never the full CLI/GUI detail view \
    conductor_get_task/conductor_get_evidence return. Explicitly reports has_verifier_output \
    = false rather than inventing a summary when nothing has actually run yet."
    )]
    pub async fn conductor_get_evidence_summary(
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
        description = "Bounded, paginated drill-down into one named artifact (from \
    conductor_get_evidence_summary's available_artifacts - typically \"diff\", \"verifier \
    stdout\", or \"verifier stderr\") of a task's most recent attempt. Pass offset = a \
    previous response's offset + shown_bytes to continue reading; max_bytes is capped \
    server-side regardless of what's requested - this never returns an unbounded log in one \
    response."
    )]
    pub async fn conductor_get_artifact(
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
impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("clatterforge", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Clatterforge local MCP adapter. Generic external-orchestrator interface: not \
                specific to any one client. Lifecycle: conductor_submit_task creates a task (never \
                auto-approves/runs) -> conductor_get_approval_requirements/conductor_approve -> \
                conductor_resume -> conductor_get_task/conductor_get_evidence to inspect the result \
                -> conductor_approve/conductor_reject to accept/reject it. Call \
                conductor_capabilities first.",
            )
    }
}
