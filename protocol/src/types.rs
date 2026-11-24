//! The MCP tool contract: every `conductor_*` request/response shape, moved verbatim
//! (field-for-field, doc comment for doc comment) out of `clatterforge`'s `src/mcp.rs` so
//! both the in-process `LocalBackend` and the edge's `RemoteBackend` implement identical
//! types, not merely identically-shaped ones. See `M12_0_ARCHITECTURE.md` for why this
//! crate exists. Nothing here changed behavior from the M11 Phase 1-3 contract this
//! replaces — an already-integrated client (Grok Bot or otherwise) sees the same schema.

/// Every tool this contract exposes — kept as one list so `conductor_capabilities` and
/// both backends' tool-router registrations can't silently drift apart.
pub const TOOL_NAMES: &[&str] = &[
    "conductor_capabilities",
    "conductor_list_repositories",
    "conductor_list_tasks",
    "conductor_get_task",
    "conductor_propose_task",
    "conductor_submit_task",
    "conductor_schedule_task",
    "conductor_get_execution",
    "conductor_get_evidence",
    "conductor_get_approval_requirements",
    "conductor_approve",
    "conductor_reject",
    "conductor_resume",
    "conductor_cancel_task",
    "conductor_global_stop_status",
    "conductor_get_evidence_summary",
    "conductor_get_artifact",
];

pub const AUTHORIZATION_NOTE: &str = "If a standing grant (CLI-managed, never created/listed/modified \
through MCP) covers this decision for the calling principal/repository/backend/operation, calling \
conductor_approve/conductor_reject/conductor_resume with that principal uses the grant automatically \
and records it in this adapter's authorization provenance instead of pausing. Otherwise every \
decision still needs an explicit call with a caller-asserted principal, exactly like the existing \
CLI --by flag - that principal is not yet a cryptographically verified identity. Initial assignment approval is authorized by submit; accepting a completed result requires approve. Exhausted grants cover later stages only for tasks that same grant already admitted, until expiry or revocation.";

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TaskIdRequest {
    /// Exact task id, or an unambiguous prefix of one (same resolution rule the CLI's
    /// `board-show <prefix>` and the HTTP `/api/tasks/{id}` route already use).
    pub task_id: String,
    /// When set, checked against any grant covering this task's current decision point -
    /// lets a caller find out in advance whether its next conductor_approve/reject call
    /// would be grant-authorized instead of interactive. Purely informational: nothing
    /// is recorded or consumed by this read-only check.
    #[serde(default)]
    pub principal: Option<String>,
    /// Optional per-call tracing id, echoed back unchanged in the response.
    #[serde(default)]
    pub request_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GetTaskRequest {
    pub task_id: String,
    /// Bytes of each artifact preview to include (diff/verifier output). Omit for the
    /// server's default bound; pass 0 to skip previews entirely.
    #[serde(default)]
    pub preview_bytes: Option<usize>,
    #[serde(default)]
    pub request_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TaskResponse {
    pub task: Value,
    /// `source_system`/`source_record_reference`/`correlation_id`/`follow_up_of` as stored
    /// by `conductor_submit_task` - every field `null` if the task has no recorded Phase 3
    /// metadata (created before this phase, or outside MCP).
    pub metadata: Value,
    pub request_id: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CapabilitiesResponse {
    pub protocol: String,
    pub adapter_version: String,
    pub engines: Vec<String>,
    pub tools: Vec<String>,
    pub scheduling_supported: bool,
    pub standing_delegation_supported: bool,
    pub notes: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ListRepositoriesResponse {
    pub repositories: Vec<Value>,
    pub request_id: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RequestIdOnly {
    #[serde(default)]
    pub request_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ListTasksResponse {
    pub tasks: Vec<Value>,
    pub request_id: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProposeTaskRequest {
    /// A `local-changes-v1` `orchestration::Spec`, exactly the JSON shape
    /// `orchestrate-prepare --spec <file>` already reads. This is the only backend this
    /// contract can validate ahead of creation.
    pub spec: Value,
    #[serde(default)]
    pub request_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProposeTaskResponse {
    pub valid: bool,
    pub repository_identity: Option<String>,
    pub errors: Vec<String>,
    pub request_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SubmitTaskRequest {
    /// `"local-changes-v1"` (real local-model coding, dispatched through the unchanged
    /// `Orchestrator`), `"fake-process-v1"` (deterministic test/demo fixture — see this
    /// module's idempotency note for its narrower guarantee), or `"cursor-agent"` /
    /// `"codex"` (a real external coding-agent CLI, dispatched through the unchanged
    /// `Agents` engine — requires `agent_preset`; see that field's doc comment for why a
    /// remote caller can't supply the executable/verifier/scope directly).
    /// `"workflow-v1"` accepts an objective and workflow_profile; its pinned local model
    /// chooses the approved CLI workers and next stages.
    pub backend: String,
    /// Caller-asserted identity. Not yet cryptographically verified over stdio local mode;
    /// over the edge path, the verified enrollment identity overrides this field entirely
    /// (see `M12_0_SECURITY_VALIDATION.md`).
    pub principal: String,
    /// Caller's own workflow/correlation identifier (e.g. a Jira issue key). Required when
    /// `idempotency_key` is set, since the two together form the dedup key.
    #[serde(default)]
    pub workflow_id: Option<String>,
    /// When set, a retried submission with the same (principal, workflow_id, key) returns
    /// the already-created task instead of creating a second one.
    #[serde(default)]
    pub idempotency_key: Option<String>,
    /// Required when backend == "local-changes-v1": an `orchestration::Spec`.
    #[serde(default)]
    pub spec: Option<Value>,
    /// Required when backend == "local-changes-v1": an `inference::config::Config`.
    #[serde(default)]
    pub config: Option<Value>,
    /// Required when backend == "fake-process-v1".
    #[serde(default)]
    pub repository_id: Option<String>,
    #[serde(default)]
    pub objective: Option<String>,
    #[serde(default)]
    pub acceptance: Option<String>,
    #[serde(default)]
    pub contents: Option<String>,
    #[serde(default)]
    pub budget: Option<usize>,
    /// Required when backend == "cursor-agent" or "codex": the name of an admin-created
    /// `agent_preset::AgentPreset` (`agent-preset-create`). Carries the pinned executable,
    /// model, verify command and file scope the main Conductor resolves locally - this
    /// field is the *only* thing a remote caller supplies for agent dispatch, per the
    /// milestone's own "do not allow remote clients to supply arbitrary executable paths
    /// or shell commands" requirement. `objective`/`acceptance` above are still used.
    #[serde(default)]
    pub agent_preset: Option<String>,
    /// Required for backend workflow-v1: admin-approved planner, worker catalog and budgets.
    /// Older clients may supply this profile name in agent_preset instead.
    /// The caller supplies an objective; Conductor's local model chooses individual workers.
    #[serde(default)]
    pub workflow_profile: Option<String>,
    /// Free-text name of the system that originated this task (e.g. `"jira"`, `"linear"`).
    /// Stored and echoed back by `conductor_get_task`'s `metadata`; never interpreted or
    /// acted on by this adapter.
    #[serde(default)]
    pub source_system: Option<String>,
    /// The originating record's own reference in its system (e.g. a Jira issue key).
    #[serde(default)]
    pub source_record_reference: Option<String>,
    /// Caller's own cross-system correlation id, distinct from `workflow_id` (which exists
    /// for idempotency keying) - e.g. a trace id spanning Jira, this task, and a later PR.
    /// Purely descriptive; never used for dedup.
    #[serde(default)]
    pub correlation_id: Option<String>,
    /// `"interactive"` forces this task to stop at `awaiting_approval` regardless of any
    /// covering grant - an explicit per-task opt-out of auto-authorization for a task the
    /// caller wants a human to look at even under broad standing delegation. Omitted, or
    /// any other value (`"delegated"`/`"hold_my_beer"`), lets grant coverage decide exactly
    /// as it already does; those two labels carry no separate runtime behavior.
    #[serde(default)]
    pub requested_execution_mode: Option<String>,
    /// Ties this submission to a prior task as a follow-up (fixing failed tests, acting on
    /// review comments, a subsequent change) without losing provenance. The referenced task
    /// must already exist - an unknown id is rejected with an actionable reason, not
    /// silently ignored. This is a provenance link only; it does not add a dependency gate.
    #[serde(default)]
    pub follow_up_of: Option<String>,
    #[serde(default)]
    pub request_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SubmitTaskResponse {
    pub task: Value,
    /// True when this call returned an already-created task instead of creating a new one.
    pub replayed: bool,
    /// Every automatic step a covering grant authorized, in order (e.g. `approve` then
    /// `resume` then `review_accept`), each naming the grant id/version that authorized
    /// it. Empty when no grant covered anything, `requested_execution_mode` was
    /// `"interactive"`, and/or the task is sitting at `awaiting_approval`. Absent (never
    /// populated) when replaying an already-created task.
    #[serde(default)]
    pub authorization: Vec<Value>,
    /// `source_system`/`source_record_reference`/`correlation_id`/`follow_up_of`, as stored
    /// - `null` for any field the caller didn't supply.
    pub metadata: Value,
    pub request_id: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DecisionRequest {
    pub task_id: String,
    /// Must equal the task's current `revision` exactly (optimistic concurrency).
    pub revision: u64,
    /// Must equal the task's current `assignment_digest` (while awaiting approval) or
    /// `evidence_digest` (while awaiting review), exactly as `conductor_get_task` returned
    /// it. Binds the decision to the exact thing being decided on.
    pub digest: String,
    pub principal: String,
    #[serde(default)]
    pub request_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DecisionResponse {
    pub task: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_stage: Option<String>,
    /// Which underlying operation this call actually performed, chosen from the task's
    /// current status: `"approve"` / `"review_accept"` / `"review_reject"` / `"cancel"`.
    pub action_taken: String,
    /// Present when a covering grant was used instead of the caller-supplied `principal`
    /// for this decision's `by` field - `{"grant_id", "grant_version"}`.
    pub authorization: Option<Value>,
    pub request_id: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ResumeRequest {
    pub task_id: String,
    pub revision: u64,
    /// Caller-asserted identity, checked against any grant covering `Operation::Resume`
    /// for this task's repository/backend. Optional: a caller resuming a task it already
    /// has no grant for can simply omit this and proceed (`run` always proceeds once a
    /// task is `approved`, regardless of who's asking - `resume` is a continuation of an
    /// already-made decision, not itself a new one; a grant only changes whose name ends
    /// up recorded in this adapter's authorization provenance).
    #[serde(default)]
    pub principal: Option<String>,
    #[serde(default)]
    pub request_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ResumeResponse {
    pub task: Value,
    /// `"run"` or `"reconcile"`, chosen from the task's current status.
    pub action_taken: String,
    pub succeeded: Option<bool>,
    /// Present when a covering grant authorized this `run` - `{"grant_id", "grant_version"}`.
    /// Always absent for `reconcile`, which stays an ungated recovery action.
    pub authorization: Option<Value>,
    pub request_id: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CancelTaskRequest {
    pub task_id: String,
    pub revision: u64,
    /// Required for fake/Codex tasks; optional (ignored) for orchestration/agent tasks —
    /// exactly `TaskService::cancel`'s existing, engine-asymmetric rule.
    #[serde(default)]
    pub principal: Option<String>,
    #[serde(default)]
    pub request_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ApprovalRequirementsResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_stage: Option<String>,
    pub task_id: String,
    /// This task's `conductor_submit_task`-supplied `workflow_id`, if any - `null` for a
    /// task with no recorded Phase 3 metadata.
    pub workflow_id: Option<String>,
    pub approval_required: bool,
    /// `"approve"` or `"review"` when `approval_required` is true; absent otherwise.
    pub action: Option<String>,
    /// The exact digest the matching `conductor_approve`/`conductor_reject` call must echo
    /// back — `assignment_digest` for an approval decision, `evidence_digest` for a review.
    pub required_digest: Option<String>,
    pub explanation: String,
    /// The shape of grant that would cover this decision automatically -
    /// `{"repository_identity", "backend", "operation"}` - for a human deciding whether to
    /// `grant-create` one. `null` when `approval_required` is false (nothing to cover).
    pub required_scope: Option<Value>,
    /// Set when `principal` was supplied and a grant already covers the pending decision -
    /// `{"grant_id", "grant_version"}`. When absent, the next conductor_approve/reject call
    /// will be interactive (or `principal` wasn't supplied to check).
    pub covered_by_grant: Option<Value>,
    /// `{"task_id", "revision"}` - the exact pair every subsequent call against this task
    /// needs, stable across client disconnects/reconnects and across this adapter's own
    /// process restarts.
    pub resumption_reference: Value,
    pub authorization_note: String,
    pub request_id: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct StopStatusResponse {
    pub stopped: bool,
    pub by: Option<String>,
    pub reason: Option<String>,
    pub at: Option<u64>,
    pub request_id: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ScheduleTaskResponse {
    pub supported: bool,
    pub explanation: String,
    pub request_id: String,
}

/// Implemented by every request DTO that carries a caller-asserted `principal`-shaped
/// field, so the main Conductor's authenticated edge dispatch
/// (`serve.rs`'s `/api/edge/tools/call`, M12.0 Phase 2) can overwrite it with the
/// verified enrollment identity in one place instead of five hand-written special cases.
/// Stdio local mode never calls this - its `principal` stays exactly as documented, a
/// caller-asserted string with a separate, weaker trust model (see `src/mcp.rs`'s module
/// doc and `docs/m12/M12_0_ARCHITECTURE.md`).
pub trait SetPrincipal {
    fn set_principal(&mut self, client_id: &str);
}

impl SetPrincipal for SubmitTaskRequest {
    fn set_principal(&mut self, client_id: &str) {
        self.principal = client_id.to_string();
    }
}

impl SetPrincipal for DecisionRequest {
    fn set_principal(&mut self, client_id: &str) {
        self.principal = client_id.to_string();
    }
}

impl SetPrincipal for ResumeRequest {
    fn set_principal(&mut self, client_id: &str) {
        self.principal = Some(client_id.to_string());
    }
}

impl SetPrincipal for CancelTaskRequest {
    fn set_principal(&mut self, client_id: &str) {
        self.principal = Some(client_id.to_string());
    }
}

impl SetPrincipal for TaskIdRequest {
    fn set_principal(&mut self, client_id: &str) {
        self.principal = Some(client_id.to_string());
    }
}

/// M12.0 Phase 6: "evidence designed for AI orchestrators" - a request for the bounded,
/// AI-shaped summary (`conductor_get_evidence_summary`), distinct from `conductor_get_
/// evidence`'s full `board::Detail` (CLI/GUI-oriented, many more fields, already a tested
/// contract this phase doesn't touch).
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct EvidenceSummaryRequest {
    pub task_id: String,
    /// Bytes of the one most relevant log excerpt to include inline. Omit for a sensible
    /// default; pass 0 to skip it entirely and rely on `conductor_get_artifact` instead.
    /// Capped server-side at 65536 bytes, like an artifact chunk.
    #[serde(default)]
    pub log_excerpt_bytes: Option<usize>,
    #[serde(default)]
    pub request_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct EvidenceSummaryResponse {
    pub task_id: String,
    /// The one real attempt/round this summary describes, if the task has reached one
    /// yet - `null` while still `awaiting_approval`/`approved` with nothing to report.
    pub execution_id: Option<String>,
    pub status: String,
    pub backend: String,
    /// The attempt's own recorded outcome string (e.g. `"Success"`, `"VerificationFails"`)
    /// - `null` when there's no attempt yet, never a guessed/invented value.
    pub verification_outcome: Option<String>,
    /// Set only when verification actually ran and did not pass - the brief's "failing
    /// check, if any." Mirrors `verification_outcome` when it names a specific failure.
    pub failing_check: Option<String>,
    /// A short, human-readable error line when the attempt recorded one - never fabricated
    /// from the absence of one.
    pub error_summary: Option<String>,
    /// The single most relevant excerpt for a failure (verifier stderr/stdout, or the
    /// diff on success) - `null` if nothing has been captured yet or `log_excerpt_bytes`
    /// was `0`.
    pub log_excerpt: Option<String>,
    pub log_excerpt_label: Option<String>,
    pub log_excerpt_truncated: bool,
    pub evidence_digest: Option<String>,
    /// Labels `conductor_get_artifact`'s `artifact` field accepts for this task right now
    /// (e.g. `["diff", "verifier stdout", "verifier stderr"]`) - the brief's "handles for
    /// detailed logs and artifacts," deliberately opaque labels rather than filesystem
    /// paths; nothing here ever exposes where a file actually lives on the main
    /// Conductor's own disk.
    pub available_artifacts: Vec<String>,
    /// Explicit per the brief's own instruction ("if a result lacks actual verifier
    /// output, explicitly state that rather than inventing a useful-looking summary") -
    /// `false` whenever there's genuinely nothing to report yet, so a caller can't
    /// mistake an empty/null summary for a real pass.
    pub has_verifier_output: bool,
    /// Optional host-observed coding-agent phase, activity and approved time limits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<Value>,
    pub request_id: String,
}

/// M12.0 Phase 6: bounded, paginated drill-down into one named artifact of a task's
/// single most recent attempt/round - the brief's own "do not dump unbounded logs into a
/// single MCP response... support pagination or bounded chunks."
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GetArtifactRequest {
    pub task_id: String,
    /// One of `conductor_get_evidence_summary`'s `available_artifacts` labels - typically
    /// `"diff"`, `"verifier stdout"`, or `"verifier stderr"`.
    pub artifact: String,
    /// Byte offset to resume from - pass back a previous response's `offset +
    /// shown_bytes` to continue reading where it left off.
    #[serde(default)]
    pub offset: Option<u64>,
    /// Hard-capped server-side regardless of what's requested - see the adapter's own
    /// bound, never a caller-controlled unbounded read.
    #[serde(default)]
    pub max_bytes: Option<usize>,
    #[serde(default)]
    pub request_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GetArtifactResponse {
    pub task_id: String,
    pub artifact: String,
    pub offset: u64,
    pub shown_bytes: usize,
    /// The artifact's own recorded total size, which can exceed what's actually readable
    /// from disk if the original capture was itself truncated (`capture_truncated`) -
    /// this is the *intended* size, not necessarily the retrievable one.
    pub total_bytes: u64,
    /// Set when the original process-output capture was truncated at write time
    /// (independent of this call's own pagination) - distinguishes "there's more to
    /// page through" from "the original recording itself is incomplete."
    pub capture_truncated: bool,
    /// Whether another `conductor_get_artifact` call with `offset: offset + shown_bytes`
    /// would return more data from what's actually on disk.
    pub has_more: bool,
    pub text: String,
    pub request_id: String,
}
