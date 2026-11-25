use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, Subcommand};
use clatterforge::{
    codex::{CodexPolicy, Executable, PinnedCommand},
    context::prepare,
    execution::{CodexEngine, ExecutionEngine},
    process::ExecutionPolicy,
    runner::default_state,
    store::{CodexRequest, FakeOutcome, Status, Store, Task},
};
use serde::Serialize;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    version,
    about = "Clatterforge (CF // SYSTEMS) — Control the chaos: deterministic agent orchestration, multi-tier CLI routing, and verified execution"
)]
struct Cli {
    #[arg(long, global = true, default_value_os_t = default_state())]
    state: PathBuf,
    /// Structured JSON log level (error, warn, info, debug, trace).
    /// Overrides CLATTERFORGE_LOG / CONDUCTOR_LOG / RUST_LOG.
    #[arg(long, global = true)]
    log_level: Option<String>,
    /// Optional path for rotating JSONL log output (defaults to <state>/logs/clatterforge.jsonl
    /// for server commands; overrides CLATTERFORGE_LOG_FILE).
    #[arg(long, global = true)]
    log_file: Option<PathBuf>,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Import an immutable portable specialist definition; executes no model or CLI.
    AgentletImport {
        #[arg(long)]
        definition: PathBuf,
        #[arg(long)]
        by: String,
    },
    AgentletList,
    AgentletShow {
        id: String,
        #[arg(long)]
        version: u32,
    },
    /// Bind a specialist to a local execution preset under a new approved name.
    AgentletBind {
        id: String,
        #[arg(long)]
        version: u32,
        #[arg(long)]
        preset: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        by: String,
    },
    /// Define the approved local planner and the worker catalog for an objective.
    WorkflowProfileCreate {
        #[arg(long)]
        name: String,
        #[arg(long)]
        config: PathBuf,
        /// Repeat NAME=PURPOSE for each existing agent preset.
        #[arg(long = "worker", required = true)]
        workers: Vec<String>,
        #[arg(long, default_value_t = 4)]
        max_steps: usize,
        #[arg(long, default_value_t = 120000)]
        decision_timeout_ms: u64,
        #[arg(long)]
        by: String,
    },
    WorkflowProfileList,
    /// Update an existing workflow profile's planner, worker catalog, or budgets (`M2`).
    WorkflowProfileUpdate {
        #[arg(long)]
        name: String,
        #[arg(long)]
        config: PathBuf,
        /// Repeat NAME=PURPOSE for each existing agent preset.
        #[arg(long = "worker", required = true)]
        workers: Vec<String>,
        #[arg(long, default_value_t = 4)]
        max_steps: usize,
        #[arg(long, default_value_t = 120000)]
        decision_timeout_ms: u64,
        #[arg(long)]
        by: String,
    },
    /// Update timeouts on an existing workflow profile and its worker presets.
    WorkflowProfileTimeouts {
        #[arg(long)]
        name: String,
        #[arg(long)]
        generation_timeout_ms: Option<u64>,
        #[arg(long)]
        generation_idle_timeout_ms: Option<u64>,
        #[arg(long)]
        verification_timeout_ms: Option<u64>,
        #[arg(long)]
        require_clean: Option<bool>,
    },
    /// Delete a workflow profile (`M2`).
    WorkflowProfileDelete {
        name: String,
    },
    WorkflowCreate {
        #[arg(long)]
        profile: String,
        #[arg(long)]
        objective: String,
        #[arg(long)]
        acceptance: String,
        #[arg(long = "allowed-file")]
        allowed_files: Vec<String>,
        #[arg(long)]
        auto_scope: bool,
        #[arg(long)]
        generation_timeout_ms: Option<u64>,
        #[arg(long)]
        generation_idle_timeout_ms: Option<u64>,
        #[arg(long)]
        verification_timeout_ms: Option<u64>,
        #[arg(long)]
        require_clean: Option<bool>,
    },
    /// Re-onboard an existing workflow profile in place with refreshed repo/CLI discovery.
    ReOnboard {
        #[arg(long)]
        name: String,
        #[arg(long)]
        low_spec: Option<bool>,
        #[arg(long)]
        refresh_scope: bool,
        #[arg(long)]
        refresh_verifier: bool,
        #[arg(long, default_value = "cli-operator")]
        by: String,
    },
    WorkflowShow {
        task: String,
    },
    /// Automatically inspect a repository, probe models/CLIs, and onboard a new project profile.
    Onboard {
        /// Target project repository path (default: current directory)
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Name for the workflow profile (default: repository folder name)
        #[arg(long)]
        name: Option<String>,
        /// Inference model to select (e.g. qwen2.5-coder:7b, gemma4:12b)
        #[arg(long)]
        model: Option<String>,
        /// Local or LAN Ollama endpoint (default: http://127.0.0.1:11434)
        #[arg(long)]
        endpoint: Option<String>,
        /// Verifier executable (auto-detected if omitted)
        #[arg(long)]
        verifier: Option<PathBuf>,
        /// Verifier arguments (auto-detected if omitted)
        #[arg(long = "verifier-arg")]
        verifier_args: Vec<String>,
        /// Custom allowed editable files
        #[arg(long = "allowed-file")]
        allowed_files: Vec<String>,
        /// Optional coding worker filter (e.g. --worker cursor, --worker codex, --worker agy, --worker local)
        #[arg(long = "worker")]
        workers: Vec<String>,
        /// Accept auto-detected defaults non-interactively
        #[arg(long, default_value_t = false)]
        yes: bool,
        /// Enable low-spec machine profile (relaxed timeouts, conservative memory limits)
        #[arg(long, default_value_t = false)]
        low_spec: bool,
        /// Only probe the environment and print report without creating profile
        #[arg(long, default_value_t = false)]
        probe_only: bool,
        /// Operator recording this configuration
        #[arg(long, default_value = "local-operator")]
        by: String,
        /// Output result as JSON
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Prepare a source integration plan from an accepted workflow or CLI agent.
    IntegrationPrepare {
        task: String,
        #[arg(long)]
        by: String,
        #[arg(long)]
        worktree: bool,
    },
    /// Inspect the source integration journal summary.
    IntegrationShow {
        task: String,
    },
    /// Apply the reviewed plan to its exact source checkout, leaving an unstaged diff.
    IntegrationApply {
        task: String,
        #[arg(long)]
        revision: u64,
        #[arg(long)]
        digest: String,
        #[arg(long)]
        by: String,
    },
    /// Restore journaled original bytes; interrupted owners must be confirmed dead.
    IntegrationRollback {
        task: String,
        #[arg(long)]
        revision: u64,
        #[arg(long)]
        digest: String,
        #[arg(long)]
        by: String,
    },
    /// Prepare scoped local coding work with immutable context and verification.
    #[command(name = "orchestrate-prepare")]
    OrchestratePrepare {
        #[arg(long)]
        spec: PathBuf,
        #[arg(long)]
        config: PathBuf,
    },
    #[command(name = "orchestrate-list")]
    OrchestrateList,
    #[command(name = "orchestrate-show")]
    OrchestrateShow {
        task: String,
        #[arg(long)]
        summary: bool,
    },
    #[command(name = "orchestrate-events")]
    OrchestrateEvents {
        task: String,
    },
    #[command(name = "orchestrate-approve")]
    OrchestrateApprove {
        task: String,
        #[arg(long)]
        revision: u64,
        #[arg(long)]
        digest: String,
        #[arg(long)]
        by: String,
    },
    /// Run the approved coding/verification/repair loop; never merge or replay.
    #[command(name = "orchestrate-run")]
    OrchestrateRun {
        task: String,
        #[arg(long)]
        revision: u64,
    },
    #[command(name = "orchestrate-cancel")]
    OrchestrateCancel {
        task: String,
        #[arg(long)]
        revision: u64,
    },
    #[command(name = "orchestrate-reconcile")]
    OrchestrateReconcile {
        task: String,
        #[arg(long)]
        revision: u64,
    },
    #[command(name = "orchestrate-review")]
    OrchestrateReview {
        task: String,
        #[arg(long)]
        revision: u64,
        #[arg(long)]
        digest: String,
        #[arg(long)]
        by: String,
        #[arg(long)]
        accept: bool,
    },
    /// M10.1: delegate one bounded coding task to an external coding-agent
    /// CLI (today: cursor-agent). `--spec` is an AgentSpec JSON file.
    #[command(name = "agent-create")]
    AgentCreate {
        #[arg(long)]
        spec: PathBuf,
        #[arg(long)]
        cursor_node: PathBuf,
        #[arg(long)]
        cursor_index_js: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// M10.4: delegate one bounded coding task to the real Codex CLI
    /// through the same Agents lifecycle `agent-create` uses for Cursor -
    /// Codex independently inspects, diagnoses and edits the permitted
    /// source; it is never handed the expected patch. `--spec` is the
    /// same `AgentSpec` JSON shape, with `target: "codex"`.
    #[command(name = "agent-create-codex")]
    AgentCreateCodex {
        #[arg(long)]
        spec: PathBuf,
        #[arg(long)]
        codex_executable: PathBuf,
        #[arg(long)]
        codex_home: PathBuf,
        #[arg(long)]
        json: bool,
    },
    #[command(name = "agent-create-agy")]
    AgentCreateAgy {
        #[arg(long)]
        spec: PathBuf,
        #[arg(long)]
        agy_executable: PathBuf,
        #[arg(long)]
        json: bool,
    },
    #[command(name = "agent-list")]
    AgentList {
        #[arg(long)]
        json: bool,
    },
    #[command(name = "agent-show")]
    AgentShow {
        task: String,
        #[arg(long)]
        json: bool,
    },
    #[command(name = "agent-approve")]
    AgentApprove {
        task: String,
        #[arg(long)]
        revision: u64,
        #[arg(long)]
        digest: String,
        #[arg(long)]
        by: String,
        #[arg(long)]
        json: bool,
    },
    /// Spawn the agent once in an isolated worktree, check its edits are in
    /// scope, run the independent verifier, and stop - never merges,
    /// pushes, or accepts.
    #[command(name = "agent-run")]
    AgentRun {
        task: String,
        #[arg(long)]
        revision: u64,
        #[arg(long)]
        json: bool,
    },
    #[command(name = "agent-cancel")]
    AgentCancel {
        task: String,
        #[arg(long)]
        revision: u64,
        #[arg(long)]
        json: bool,
    },
    #[command(name = "agent-review")]
    AgentReview {
        task: String,
        #[arg(long)]
        revision: u64,
        #[arg(long)]
        digest: String,
        #[arg(long)]
        by: String,
        #[arg(long)]
        accept: bool,
        #[arg(long)]
        json: bool,
    },
    /// M10.2: recover an agent task whose owning process is confirmed
    /// dead - refuses while the owner looks alive or uninspectable.
    /// Lands at `unknown`, quarantining (not releasing) its repository
    /// lease; see `board-leases-acknowledge` to recover that afterward.
    #[command(name = "agent-reconcile")]
    AgentReconcile {
        task: String,
        #[arg(long)]
        revision: u64,
        #[arg(long)]
        json: bool,
    },
    /// M12.0 Phase 5: an admin-approved, named coding-agent dispatch configuration - the
    /// only thing `conductor_submit_task` lets a remote MCP/edge caller reference for
    /// backend "cursor-agent"/"codex" (never a raw executable path, verify command, or
    /// file scope - see `src/agent_preset.rs`). `--cursor-node`/`--cursor-index-js` pin a
    /// Cursor backend; `--codex-executable`/`--codex-home` pin Codex instead - exactly one
    /// pair is required, matching `agent-create`/`agent-create-codex`'s own split.
    #[command(name = "agent-preset-create")]
    AgentPresetCreate {
        #[arg(long)]
        name: String,
        #[arg(long)]
        repository: PathBuf,
        #[arg(long)]
        model: String,
        #[arg(long)]
        cursor_node: Option<PathBuf>,
        #[arg(long)]
        cursor_index_js: Option<PathBuf>,
        #[arg(long)]
        codex_executable: Option<PathBuf>,
        #[arg(long)]
        codex_home: Option<PathBuf>,
        #[arg(long)]
        agy_executable: Option<PathBuf>,
        /// Comma-separated, relative to the repository - the agent may only edit these.
        #[arg(long, value_delimiter = ',')]
        allowed_files: Vec<String>,
        /// Comma-separated, relative to the repository - the independent verifier reads
        /// these; disjoint from --allowed-files.
        #[arg(long, value_delimiter = ',')]
        verification_files: Vec<String>,
        #[arg(long)]
        verify_executable: PathBuf,
        #[arg(long, value_delimiter = ',')]
        verify_args: Vec<String>,
        #[arg(long, default_value_t = 120_000)]
        generation_timeout_ms: u64,
        /// Optional inactivity limit. Observed output or approved-file changes renew
        /// this window, while --generation-timeout-ms remains an independent hard cap.
        #[arg(long)]
        generation_idle_timeout_ms: Option<u64>,
        #[arg(long, default_value_t = 30_000)]
        verification_timeout_ms: u64,
        #[arg(long)]
        by: String,
        #[arg(long)]
        json: bool,
    },
    #[command(name = "agent-preset-list")]
    AgentPresetList {
        #[arg(long)]
        json: bool,
    },
    #[command(name = "agent-preset-show")]
    AgentPresetShow {
        name: String,
        #[arg(long)]
        json: bool,
    },
    #[command(name = "agent-preset-delete")]
    AgentPresetDelete {
        name: String,
    },
    /// Update or clear an agent preset's routing tier and allowed model allowlist (`M1`).
    #[command(name = "agent-preset-routing")]
    AgentPresetRouting {
        name: String,
        /// Optional tier override: local, fast, standard, frontier (omit to clear override).
        #[arg(long)]
        tier: Option<String>,
        /// Comma-separated allowed models (omit or pass empty to clear custom allowlist).
        #[arg(long, value_delimiter = ',')]
        allowed_models: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    /// Read-only cross-engine task board; human table by default, --json for scripting.
    Board {
        #[arg(long)]
        json: bool,
    },
    /// Serve a compiled GUI without a compiler or development server (loopback by default).
    UiServe {
        #[arg(long)]
        assets: PathBuf,
        #[arg(long, default_value = "127.0.0.1")]
        bind: std::net::IpAddr,
        #[arg(long, default_value_t = 8080)]
        port: u16,
        /// Accept a non-loopback bind over plain HTTP. Prefer loopback plus Tailscale Serve.
        #[arg(long, default_value_t = false)]
        allow_insecure_lan: bool,
    },
    /// Run the HTTP service a GUI talks to: loopback by default (a non-loopback --bind also
    /// needs --allow-insecure-lan, since the service is plain HTTP),
    /// bearer-token (desktop) or paired-session (phone/browser) auth, full read+write task API,
    /// and optional same-origin compiled UI serving via --ui-assets.
    Serve {
        #[arg(long, default_value = "127.0.0.1")]
        bind: std::net::IpAddr,
        #[arg(long, default_value_t = 8787)]
        port: u16,
        #[arg(long)]
        ui_assets: Option<PathBuf>,
        /// Generate and write a fresh bearer token to `<state>/serve-token` instead of reusing the persisted one.
        #[arg(long, default_value_t = false)]
        rotate_token: bool,
        /// Accept a non-loopback bind over plain HTTP. Prefer loopback plus Tailscale Serve.
        #[arg(long, default_value_t = false)]
        allow_insecure_lan: bool,
    },
    /// Read recent structured JSONL log entries from `<state>/logs/clatterforge.jsonl`.
    Logs {
        #[arg(long, default_value_t = 50)]
        tail: usize,
        #[arg(long)]
        level: Option<String>,
        #[arg(long)]
        event: Option<String>,
        #[arg(long, default_value_t = false)]
        pretty: bool,
    },
    /// List active persisted paired-device sessions.
    #[command(name = "sessions-list")]
    SessionsList {
        #[arg(long)]
        json: bool,
    },
    /// Revoke one persisted paired-device session by id.
    #[command(name = "sessions-revoke")]
    SessionsRevoke {
        id: String,
    },
    /// Revoke all persisted paired-device sessions immediately.
    #[command(name = "sessions-revoke-all")]
    SessionsRevokeAll,
    /// Run the dedicated loopback gateway: a narrow allow-list reverse proxy that is
    /// the only thing a public-facing tunnel (Cloudflare or otherwise) should ever be
    /// pointed at. Forwards exactly `POST /api/edge/tools/call` to `--upstream`
    /// (the real `clatterforge serve` instance); every other path or method gets a
    /// plain 404 before anything resembling the rest of the Conductor's HTTP API is
    /// ever reached. Performs no authentication of its own - `authenticate_edge` on
    /// the real Conductor still has to pass. Always binds loopback only, by design -
    /// there is no flag to bind it anywhere else.
    #[command(name = "edge-gateway")]
    EdgeGateway {
        #[arg(long, default_value_t = 8789)]
        port: u16,
        /// Base URL of the real clatterforge serve instance this gateway forwards
        /// to, e.g. http://127.0.0.1:8787.
        #[arg(long)]
        upstream: String,
        #[arg(long, default_value_t = 15_000)]
        timeout_ms: u64,
    },
    /// Run the local MCP (Model Context Protocol) adapter over stdio for an external
    /// orchestrator (e.g. Grok Bot) to submit and manage coding tasks. See `docs/m11/`.
    #[command(name = "mcp-serve")]
    McpServe,
    /// Read-only cross-engine task detail; accepts an unambiguous id prefix.
    /// Shows a bounded preview of the latest attempt/round's diff and verifier
    /// stdout/stderr, read fresh from disk; omitted with --no-preview.
    #[command(name = "board-show")]
    BoardShow {
        task: String,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        no_preview: bool,
        #[arg(long, default_value_t = 1024)]
        preview_bytes: usize,
    },
    /// Approve a task in either engine; accepts an unambiguous id prefix.
    #[command(name = "board-approve")]
    BoardApprove {
        task: String,
        #[arg(long)]
        revision: u64,
        #[arg(long)]
        digest: String,
        #[arg(long)]
        by: String,
        #[arg(long)]
        json: bool,
    },
    /// Run the approved task in either engine; accepts an unambiguous id prefix.
    #[command(name = "board-run")]
    BoardRun {
        task: String,
        #[arg(long)]
        revision: u64,
        #[arg(long)]
        json: bool,
    },
    /// Cancel a task in either engine; accepts an unambiguous id prefix.
    /// `--by` is required for a fake/Codex task and ignored for an orchestration task,
    /// which does not record a cancelling actor.
    #[command(name = "board-cancel")]
    BoardCancel {
        task: String,
        #[arg(long)]
        revision: u64,
        #[arg(long)]
        by: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Mark an interrupted task unknown in either engine; accepts an unambiguous id prefix.
    #[command(name = "board-reconcile")]
    BoardReconcile {
        task: String,
        #[arg(long)]
        revision: u64,
        #[arg(long)]
        json: bool,
    },
    /// Record human review in either engine; accepts an unambiguous id prefix.
    #[command(name = "board-review")]
    BoardReview {
        task: String,
        #[arg(long)]
        revision: u64,
        #[arg(long)]
        digest: String,
        #[arg(long)]
        by: String,
        #[arg(long)]
        accept: bool,
        /// Line-anchored note on the candidate (workflow-v1), as PATH:LINE[-END]:TEXT.
        /// Repeatable. A later board-retry of a rejected workflow hands these to the next
        /// repair round.
        #[arg(long = "comment", value_parser = parse_review_comment)]
        comments: Vec<clatterforge::store::ReviewComment>,
        #[arg(long)]
        json: bool,
    },
    /// Retry, resume, or restart a failed/cancelled/rejected/interrupted task or workflow.
    #[command(name = "board-retry")]
    BoardRetry {
        task: String,
        #[arg(long)]
        revision: u64,
        #[arg(long)]
        by: String,
        #[arg(long)]
        reset: bool,
        #[arg(long)]
        from_step: Option<usize>,
        #[arg(long)]
        extra_steps: Option<usize>,
        #[arg(long)]
        guidance: Option<String>,
        #[arg(long)]
        run: bool,
        #[arg(long)]
        json: bool,
    },
    /// Record that `task` cannot `board-run` until `--on` reaches `accepted`; accepts
    /// unambiguous id prefixes for either side, in either engine. Rejects a self-dependency
    /// and anything that would create a cycle.
    #[command(name = "board-depend")]
    BoardDepend {
        task: String,
        #[arg(long = "on")]
        on: String,
    },
    /// List current repository-lease reservations (held or quarantined) that make
    /// `board-run` serialize same-repository tasks and parallelize different ones.
    #[command(name = "board-leases")]
    BoardLeases {
        #[arg(long)]
        json: bool,
    },
    /// Explicitly release a quarantined repository lease after confirming its owning
    /// process is actually gone; refuses while it still looks alive or unverifiable.
    #[command(name = "board-leases-acknowledge")]
    BoardLeasesAcknowledge {
        #[arg(long)]
        resource: String,
        #[arg(long)]
        by: String,
        #[arg(long)]
        reason: String,
    },
    /// Delete a single non-live task or workflow (plus its owned workflow step tasks
    /// and workspace artifacts); accepts an unambiguous id prefix.
    #[command(name = "board-delete")]
    BoardDelete {
        task: String,
        #[arg(long)]
        json: bool,
    },
    /// Clear finished/failed tasks and workflows from the board (never touches live or
    /// pending-approval/review tasks or tasks holding repository leases).
    #[command(name = "board-clear")]
    BoardClear {
        #[arg(long, value_enum, default_value = "failed")]
        scope: clatterforge::service::ClearScope,
        #[arg(long)]
        json: bool,
    },
    /// M7/M10: create a standing policy - a finite, pre-declared list of tasks a human
    /// explicitly authorizes ahead of time, bounded by max-tasks and an expiry. Exactly
    /// one of `--experiments` (fake-engine, a JSON array of
    /// {"objective","acceptance","contents"}) or `--real-tasks` (M10: real
    /// `local-changes-v1` coding tasks, a JSON object `{"config": <inference config>,
    /// "tasks": [<orchestration::Spec fields minus "repository">, ...]}`) is required - a
    /// policy authorizes one engine's worth of work, never a mix.
    #[command(name = "policy-create")]
    PolicyCreate {
        #[arg(long)]
        repository: String,
        #[arg(long)]
        by: String,
        #[arg(long)]
        max_tasks: usize,
        #[arg(long)]
        expires_in_ms: u64,
        #[arg(long)]
        experiments: Option<PathBuf>,
        #[arg(long)]
        real_tasks: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    #[command(name = "policy-show")]
    PolicyShow {
        policy: String,
        #[arg(long)]
        json: bool,
    },
    /// Only stops *future* `hold-my-beer` dispatch under this policy - never touches a
    /// task already created.
    #[command(name = "policy-revoke")]
    PolicyRevoke {
        policy: String,
        #[arg(long)]
        by: String,
        #[arg(long)]
        reason: String,
        #[arg(long)]
        json: bool,
    },
    /// M11 Phase 2: create a standing authorization grant for an external MCP caller -
    /// never reachable through the MCP adapter itself, only this CLI (or the HTTP surface,
    /// if later wired). Scoped to one principal, one repository identity and a finite list
    /// of backends/operations; bounded by max-tasks and an expiry, exactly like
    /// `policy-create`. `--repository` is a checkout path, resolved to its
    /// `Repository::identity` at creation time - the actual scoping key.
    #[command(name = "grant-create")]
    GrantCreate {
        #[arg(long)]
        principal: String,
        #[arg(long)]
        repository: PathBuf,
        /// Comma-separated backend ids, e.g. "fake-process-v1,local-changes-v1".
        #[arg(long, value_delimiter = ',')]
        backends: Vec<String>,
        /// Comma-separated operations: submit,approve,reject,resume,cancel.
        #[arg(long, value_delimiter = ',')]
        operations: Vec<String>,
        #[arg(long)]
        hold_my_beer: bool,
        #[arg(long)]
        by: String,
        #[arg(long)]
        max_tasks: usize,
        #[arg(long)]
        expires_in_ms: u64,
        #[arg(long)]
        json: bool,
    },
    #[command(name = "grant-list")]
    GrantList {
        #[arg(long)]
        json: bool,
    },
    #[command(name = "grant-show")]
    GrantShow {
        grant: String,
        #[arg(long)]
        json: bool,
    },
    /// Only stops *future* auto-authorization under this grant - never touches a task
    /// already created or already-recorded decisions, same invariant `policy-revoke` keeps.
    #[command(name = "grant-revoke")]
    GrantRevoke {
        grant: String,
        #[arg(long)]
        by: String,
        #[arg(long)]
        reason: String,
        #[arg(long)]
        json: bool,
    },
    /// M12.0 Phase 2: enroll a new external client (e.g. one Hitch/Grok Bot installation)
    /// allowed to call `/api/edge/tools/call`. Prints the one and only plaintext copy of
    /// the issued credential, or writes it to --token-file instead of stdout - never both.
    /// `--repositories` resolves each checkout path to its `Repository::identity` at
    /// creation time, same as `grant-create --repository`; stored, not yet enforced (see
    /// `src/enrollment.rs`).
    #[command(name = "enrollment-create")]
    EnrollmentCreate {
        #[arg(long)]
        display_name: String,
        #[arg(long, default_value = "generic-mcp-client")]
        client_type: String,
        /// Comma-separated: inspect,submit,approve,reject,resume,cancel.
        #[arg(long, value_delimiter = ',')]
        operations: Vec<String>,
        /// Comma-separated checkout paths this client may act against.
        #[arg(long, value_delimiter = ',')]
        repositories: Vec<PathBuf>,
        #[arg(long)]
        by: String,
        #[arg(long)]
        token_file: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    #[command(name = "enrollment-list")]
    EnrollmentList {
        #[arg(long)]
        json: bool,
    },
    #[command(name = "enrollment-show")]
    EnrollmentShow {
        client: String,
        #[arg(long)]
        json: bool,
    },
    /// Immediately blocks the client's next privileged call - `Enrollments::verify`
    /// re-reads state fresh on every request, so there is no cache to invalidate.
    #[command(name = "enrollment-revoke")]
    EnrollmentRevoke {
        client: String,
        #[arg(long)]
        by: String,
        #[arg(long)]
        reason: String,
        #[arg(long)]
        json: bool,
    },
    /// Issues a fresh credential, invalidating the previous one immediately. Same
    /// print-or-write-to-file rule as `enrollment-create`.
    #[command(name = "enrollment-rotate-credential")]
    EnrollmentRotateCredential {
        client: String,
        #[arg(long)]
        by: String,
        #[arg(long)]
        token_file: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// What this client can actually do right now: its enrollment's allowed operations/
    /// repository scope/revocation state, plus every standing `Grant` whose `principal`
    /// matches its client id - the brief's "inspect effective permissions" administration
    /// command.
    #[command(name = "enrollment-effective-permissions")]
    EnrollmentEffectivePermissions {
        client: String,
        #[arg(long)]
        json: bool,
    },
    /// M7: unattended dispatch - create+approve+run every undispatched experiment in the
    /// named policy, stopping each at awaiting_review (never auto-accepts). Stops the
    /// whole batch early, visibly, on exhaustion/expiry/revocation or a genuine dispatch
    /// error; an experiment's own failed/timed-out attempt does not stop the batch.
    #[command(name = "hold-my-beer")]
    HoldMyBeer {
        #[arg(long)]
        policy: String,
        #[arg(long, default_value_t = 16384)]
        budget: usize,
        #[arg(long, default_value_t = 5000)]
        timeout_ms: u64,
        #[arg(long, default_value_t = 5000)]
        verification_timeout_ms: u64,
        #[arg(long, default_value_t = 32768)]
        log_bytes: usize,
        #[arg(long)]
        json: bool,
    },
    /// M7: the big red button - refuses all future `run` dispatch (any engine, any
    /// caller) and requests cancellation of every currently active attempt through each
    /// engine's own existing `cancel`. Broader than `policy-revoke`, which only ever
    /// stops one policy's own future dispatch.
    #[command(name = "global-stop")]
    GlobalStop {
        #[arg(long)]
        by: String,
        #[arg(long)]
        reason: String,
        #[arg(long)]
        json: bool,
    },
    #[command(name = "global-stop-status")]
    GlobalStopStatus {
        #[arg(long)]
        json: bool,
    },
    /// Only clears the switch - never relaunches or alters anything cancelled while it
    /// was engaged.
    #[command(name = "global-resume")]
    GlobalResume {
        #[arg(long)]
        by: String,
        #[arg(long)]
        reason: String,
        #[arg(long)]
        json: bool,
    },
    /// Report execution-engine and direct-inference capabilities separately.
    Capabilities,
    /// Prepare a bounded canonical direct-generation job; never dispatch here.
    InferenceCreate {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        request: PathBuf,
        #[arg(long)]
        target: Option<String>,
        #[arg(long)]
        registry: Option<PathBuf>,
    },
    /// Run or resume only at a safe canonical checkpoint; no automatic replay.
    InferenceRun {
        job: String,
        #[arg(long)]
        revision: u64,
        #[arg(long)]
        target: Option<String>,
        #[arg(long)]
        one_turn: bool,
    },
    InferenceShow {
        job: String,
        #[arg(long)]
        summary: bool,
    },
    InferenceEvents {
        job: String,
    },
    InferenceCancel {
        job: String,
        #[arg(long)]
        revision: u64,
    },
    InferenceReconcile {
        job: String,
        #[arg(long)]
        revision: u64,
    },
    /// Validate and import a versioned capability/measurement registry.
    InferenceRegistryImport {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        registry: PathBuf,
    },
    InferenceResources {
        #[arg(long)]
        config: PathBuf,
    },
    /// Acknowledge external runtime idleness after unconfirmed abort/owner loss.
    InferenceReleaseResource {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        resource: String,
        #[arg(long)]
        by: String,
        #[arg(long)]
        reason: String,
    },
    /// Register the exact root of an existing committed Git checkout.
    Register {
        path: PathBuf,
    },
    /// Inspect a bounded context packet without creating an assignment.
    Context {
        repository: String,
        objective: String,
        #[arg(long, default_value_t = 16384)]
        budget: usize,
    },
    /// Prepare a deterministic assignment; this does not launch anything.
    Create {
        repository: String,
        objective: String,
        #[arg(long)]
        acceptance: String,
        #[arg(long)]
        contents: String,
        #[arg(long, default_value_t = 16384)]
        budget: usize,
        #[arg(long, default_value_t = 5000)]
        timeout_ms: u64,
        #[arg(long, default_value_t = 5000)]
        verification_timeout_ms: u64,
        #[arg(long, default_value_t = 32768)]
        log_bytes: usize,
    },
    List,
    /// Prepare a pinned Codex new-file assignment and an independent check; never launch here.
    CreateCodex {
        repository: String,
        objective: String,
        #[arg(long)]
        acceptance: String,
        #[arg(long)]
        contents: String,
        #[arg(long)]
        output_file: String,
        #[arg(long)]
        codex_executable: PathBuf,
        #[arg(long)]
        codex_home: PathBuf,
        #[arg(long)]
        model: String,
        #[arg(long)]
        verify_executable: PathBuf,
        #[arg(long, allow_hyphen_values = true)]
        verify_arg: Vec<String>,
        #[arg(long, default_value_t = 16384)]
        budget: usize,
        #[arg(long, default_value_t = 120000)]
        timeout_ms: u64,
        #[arg(long, default_value_t = 30000)]
        verification_timeout_ms: u64,
        #[arg(long, default_value_t = 32768)]
        log_bytes: usize,
    },
    Show {
        task: String,
        #[arg(long)]
        summary: bool,
    },
    Events {
        task: String,
    },
    /// Revise a fake assignment and invalidate its prior approval.
    Revise {
        task: String,
        #[arg(long)]
        revision: u64,
        #[arg(long)]
        contents: String,
        #[arg(long, value_enum, default_value = "success")]
        outcome: FakeOutcome,
    },
    /// Approve the exact assignment digest printed by show/create.
    Approve {
        task: String,
        #[arg(long)]
        revision: u64,
        #[arg(long)]
        digest: String,
        #[arg(long)]
        by: String,
    },
    /// Execute the approved assignment in a new detached worktree.
    Run {
        task: String,
        #[arg(long)]
        revision: u64,
    },
    /// Mark an interrupted attempt unknown; never automatically replay it.
    Reconcile {
        task: String,
        #[arg(long)]
        revision: u64,
    },
    /// Request cancellation through the proven live supervisor.
    Cancel {
        task: String,
        #[arg(long)]
        revision: u64,
        #[arg(long)]
        by: String,
    },
    /// Record human review, bound to the captured evidence digest.
    Review {
        task: String,
        #[arg(long)]
        revision: u64,
        #[arg(long)]
        digest: String,
        #[arg(long)]
        by: String,
        #[arg(long)]
        accept: bool,
    },
    #[command(name = "analytics")]
    Analytics {
        #[arg(long)]
        json: bool,
    },
    #[command(name = "__fixture-worker", hide = true)]
    FixtureWorker {
        spec: PathBuf,
        token: String,
        #[arg(long)]
        verify: bool,
    },
    #[command(name = "__fixture-descendant", hide = true)]
    FixtureDescendant {
        spec: PathBuf,
        token: String,
    },
    #[command(name = "__provider-launcher", hide = true)]
    ProviderLauncher {
        spec: PathBuf,
        token: String,
    },
    #[command(name = "__local-agent-worker", hide = true)]
    LocalAgentWorker {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

fn print(value: &impl Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn print_task(store: &Store, task: &Task) -> Result<()> {
    print(
        &serde_json::json!({"task": task, "assignment_digest": task.assignment.digest()?, "evidence_digest": if task.attempts.is_empty() { None } else { Some(store.evidence_digest(task)?) }, "process_sandbox": false}),
    )
}

/// This CLI's clap-derived `Commands` enum is large enough (50+ variants) that its own
/// generated parsing/help code overflows the OS default thread stack in debug builds -
/// confirmed by `RUST_MIN_STACK=16MiB` fixing it outright, with no actual runtime
/// recursion anywhere in this codebase. Spawning the real work on a thread with explicit
/// headroom fixes it at the source for every caller (direct CLI use, every test
/// subprocess, the GUI's sidecar spawn), instead of requiring each one to set an
/// environment variable main's own stack can't retroactively grow.
fn main() -> Result<()> {
    let result = std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(run)?
        .join()
        .unwrap_or_else(|_| Err(anyhow::anyhow!("worker thread panicked")));
    if let Err(ref err) = result {
        clatterforge::logging::emit(
            clatterforge::logging::LogLevel::Error,
            "cli",
            "command failed",
            Some(clatterforge::logging::error_fields(err)),
        );
    }
    result
}

fn is_server_command(command: &Commands) -> bool {
    matches!(
        command,
        Commands::Serve { .. }
            | Commands::UiServe { .. }
            | Commands::EdgeGateway { .. }
            | Commands::McpServe
    )
}

fn configure_logging(cli: &Cli) -> Result<()> {
    let default_level = if is_server_command(&cli.command) {
        clatterforge::logging::LogLevel::Info
    } else {
        clatterforge::logging::LogLevel::Warn
    };
    let level = match cli.log_level.as_deref() {
        Some(raw) => clatterforge::logging::LogLevel::parse(raw)?,
        None => clatterforge::logging::LogLevel::from_env(default_level),
    };
    let file = cli
        .log_file
        .clone()
        .or_else(|| std::env::var_os("CLATTERFORGE_LOG_FILE").map(PathBuf::from))
        .or_else(|| {
            is_server_command(&cli.command)
                .then(|| cli.state.join("logs").join("clatterforge.jsonl"))
        });
    clatterforge::logging::init(clatterforge::logging::LogConfig {
        level,
        file,
        ..Default::default()
    });
    Ok(())
}

fn run() -> Result<()> {
    let raw_args: Vec<String> = std::env::args().collect();
    if let Some(pos) = raw_args
        .iter()
        .position(|a| a == "__local-agent-worker" || a == "--local-worker")
    {
        std::process::exit(clatterforge::agents::run_local_agent_worker(
            &raw_args[pos + 1..],
        )?);
    }
    let cli = Cli::parse();
    configure_logging(&cli)?;
    match &cli.command {
        Commands::LocalAgentWorker { args } => {
            std::process::exit(clatterforge::agents::run_local_agent_worker(args)?)
        }
        Commands::FixtureWorker {
            spec,
            token,
            verify,
        } => std::process::exit(clatterforge::fixture::run(spec, token, *verify)?),
        Commands::FixtureDescendant { spec, token } => {
            return clatterforge::fixture::descendant(spec, token);
        }
        Commands::ProviderLauncher { spec, token } => {
            std::process::exit(clatterforge::launcher::run(spec, token)?)
        }
        Commands::McpServe => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            return runtime.block_on(clatterforge::mcp::run(cli.state.clone()));
        }
        Commands::UiServe {
            assets,
            bind,
            port,
            allow_insecure_lan,
        } => {
            if let Some(warning) =
                clatterforge::http_access::ensure_bind_allowed(*bind, *allow_insecure_lan)?
            {
                eprintln!("{warning}");
            }
            let access = clatterforge::http_access::AccessPolicy::load(&cli.state)?;
            let runtime = tokio::runtime::Runtime::new()?;
            return runtime.block_on(clatterforge::ui_server::run_with_options(
                assets.clone(),
                *bind,
                *port,
                access,
            ));
        }
        Commands::Serve {
            bind,
            port,
            ui_assets,
            rotate_token,
            allow_insecure_lan,
        } => {
            if let Some(warning) =
                clatterforge::http_access::ensure_bind_allowed(*bind, *allow_insecure_lan)?
            {
                eprintln!("{warning}");
            }
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            return runtime.block_on(clatterforge::serve::run_with_rotation(
                cli.state.clone(),
                *bind,
                *port,
                ui_assets.clone(),
                *rotate_token,
            ));
        }
        Commands::Logs {
            tail,
            level,
            event,
            pretty,
        } => {
            let path = clatterforge::logging::log_file_path(&cli.state);
            let entries = clatterforge::logging::read_recent_from_path(
                &path,
                *tail,
                level.as_deref(),
                event.as_deref(),
            )?;
            for entry in entries {
                if *pretty {
                    println!("{}", serde_json::to_string_pretty(&entry)?);
                } else {
                    println!("{}", serde_json::to_string(&entry)?);
                }
            }
            return Ok(());
        }
        Commands::SessionsList { json } => {
            let list = clatterforge::sessions::Sessions::open(&cli.state)?.list_active()?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&list)?);
            } else if list.is_empty() {
                println!("No active paired device sessions.");
            } else {
                for s in list {
                    println!(
                        "{}  principal={}  expires_at={}",
                        s.id, s.principal, s.expires_at
                    );
                }
            }
            return Ok(());
        }
        Commands::SessionsRevoke { id } => {
            let revoked = clatterforge::sessions::Sessions::open(&cli.state)?.revoke_by_id(id)?;
            println!("{}", serde_json::json!({"id": id, "revoked": revoked}));
            return Ok(());
        }
        Commands::SessionsRevokeAll => {
            let revoked = clatterforge::sessions::Sessions::open(&cli.state)?.revoke_all()?;
            println!("{}", serde_json::json!({"revoked": revoked}));
            return Ok(());
        }
        Commands::EdgeGateway {
            port,
            upstream,
            timeout_ms,
        } => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            return runtime.block_on(clatterforge::gateway::run(
                *port,
                upstream.clone(),
                *timeout_ms,
            ));
        }
        _ => (),
    }
    if board_command(&cli)?
        || board_write_command(&cli)?
        || board_coordination_command(&cli)?
        || agent_command(&cli)?
        || agent_preset_command(&cli)?
        || workflow_command(&cli)?
        || policy_command(&cli)?
        || grant_command(&cli)?
        || enrollment_command(&cli)?
        || stop_command(&cli)?
        || orchestration_command(&cli)?
        || inference_command(&cli)?
    {
        return Ok(());
    }
    let is_run = matches!(&cli.command, Commands::Run { .. });
    let mut store = Store::open(&cli.state)?;
    let task = match cli.command {
        Commands::Register { path } => {
            let (id, repository) = store.register(&path)?;
            return print(&serde_json::json!({"id":id,"repository":repository}));
        }
        Commands::Context {
            repository,
            objective,
            budget,
        } => {
            let repo = store.repository(&repository)?;
            return print(&prepare(&repo, &objective, budget)?);
        }
        Commands::Create {
            repository,
            objective,
            acceptance,
            contents,
            budget,
            timeout_ms,
            verification_timeout_ms,
            log_bytes,
        } => {
            let mut policy = ExecutionPolicy::new(&std::env::current_exe()?)?;
            policy.timeout_ms = timeout_ms;
            policy.verification_timeout_ms = verification_timeout_ms;
            policy.log_bytes = log_bytes;
            store.create(&repository, objective, acceptance, contents, budget, policy)?
        }
        Commands::List => return print(&store.tasks()?),
        Commands::CreateCodex {
            repository,
            objective,
            acceptance,
            contents,
            output_file,
            codex_executable,
            codex_home,
            model,
            verify_executable,
            verify_arg,
            budget,
            timeout_ms,
            verification_timeout_ms,
            log_bytes,
        } => {
            let provider = CodexPolicy::inspect(
                &codex_executable,
                &codex_home,
                model,
                PinnedCommand {
                    executable: Executable::new(&verify_executable)?,
                    args: verify_arg,
                },
            )?;
            let mut execution = ExecutionPolicy::new(&std::env::current_exe()?)?;
            execution.timeout_ms = timeout_ms;
            execution.verification_timeout_ms = verification_timeout_ms;
            execution.log_bytes = log_bytes;
            store.create_codex(CodexRequest {
                repository_id: repository,
                objective,
                acceptance,
                contents,
                output_file,
                budget,
                execution,
                policy: provider,
            })?
        }
        Commands::Show { task, summary } => {
            let task = store.task(&task)?;
            if summary {
                return print(&clatterforge::telemetry::engine_summary(&task));
            }
            task
        }
        Commands::Events { task } => return print(&store.events(&task)?),
        Commands::Revise {
            task,
            revision,
            contents,
            outcome,
        } => store.revise(&task, revision, contents, outcome)?,
        Commands::Approve {
            task,
            revision,
            digest,
            by,
        } => store.approve(&task, revision, &digest, &by)?,
        Commands::Run { task, revision } => {
            clatterforge::stop::GlobalStop::open(&cli.state)?
                .state()?
                .ensure_admission_allowed()?;
            // M10.4 Phase 1: the same `Coordination` repository lease
            // `TaskService::run`/`board-run` already enforce, closing the
            // documented gap where this legacy command checked global stop
            // but not repository admission. Shares the identical
            // `with_repository_lease` helper `TaskService::run` itself now
            // calls - not a second, look-alike mechanism.
            let resource = store.task(&task)?.assignment.repository.identity.clone();
            clatterforge::service::with_repository_lease(
                &cli.state,
                resource,
                task.clone(),
                || {
                    if store.task(&task)?.assignment.backend == "codex-exec-v1" {
                        CodexEngine.execute(&mut store, &task, revision)
                    } else {
                        store.run(&task, revision)
                    }
                },
            )?
        }
        Commands::Reconcile { task, revision } => store.reconcile(&task, revision)?,
        Commands::Cancel { task, revision, by } => store.cancel(&task, revision, &by)?,
        Commands::Review {
            task,
            revision,
            digest,
            by,
            accept,
        } => store.review(&task, revision, &digest, &by, accept)?,
        Commands::FixtureWorker { .. }
        | Commands::FixtureDescendant { .. }
        | Commands::ProviderLauncher { .. } => unreachable!(),
        _ => unreachable!("inference commands handled before opening execution store"),
    };
    print_task(&store, &task)?;
    ensure!(
        !(is_run
            && matches!(
                task.status,
                Status::Failed | Status::Cancelled | Status::TimedOut | Status::Unknown
            )),
        "attempt failed; inspect the recorded task and preserved workspace"
    );
    Ok(())
}

fn board_command(cli: &Cli) -> Result<bool> {
    use clatterforge::{board, service::TaskService};
    match &cli.command {
        Commands::Board { json } => {
            let rows = TaskService::open(&cli.state)?.board()?;
            if *json {
                print(&rows)?;
            } else {
                println!("{}", board::render_table(&rows));
            }
        }
        Commands::BoardShow {
            task,
            json,
            no_preview,
            preview_bytes,
        } => {
            let service = TaskService::open(&cli.state)?;
            let preview_bytes = if *no_preview {
                None
            } else {
                Some(*preview_bytes)
            };
            let detail = service.detail(task, preview_bytes)?;
            if *json {
                print(&detail)?;
            } else {
                println!("{}", board::render_detail(&detail));
            }
        }
        _ => return Ok(false),
    }
    Ok(true)
}

/// `board-*` write commands all print the same `board::Detail` shape for `--json`,
/// matching `board-show` rather than each engine's own raw-task shape from before this
/// refactor. Nothing outside this session's own testing has consumed the old shape yet,
/// so standardizing now is a deliberate simplification, not a compatibility break.
fn board_write_command(cli: &Cli) -> Result<bool> {
    use clatterforge::{board, service::TaskService};
    let mut service = match &cli.command {
        Commands::BoardApprove { .. }
        | Commands::BoardRun { .. }
        | Commands::BoardCancel { .. }
        | Commands::BoardReconcile { .. }
        | Commands::BoardReview { .. }
        | Commands::BoardRetry { .. } => TaskService::open(&cli.state)?,
        _ => return Ok(false),
    };
    let (detail, json, failed) = match &cli.command {
        Commands::BoardApprove {
            task,
            revision,
            digest,
            by,
            json,
        } => (service.approve(task, *revision, digest, by)?, *json, false),
        Commands::BoardRun {
            task,
            revision,
            json,
        } => {
            let outcome = service.run(task, *revision)?;
            (outcome.detail, *json, !outcome.succeeded)
        }
        Commands::BoardCancel {
            task,
            revision,
            by,
            json,
        } => (
            service.cancel(task, *revision, by.as_deref())?,
            *json,
            false,
        ),
        Commands::BoardReconcile {
            task,
            revision,
            json,
        } => (service.reconcile(task, *revision)?, *json, false),
        Commands::BoardReview {
            task,
            revision,
            digest,
            by,
            accept,
            comments,
            json,
        } => (
            service.review_with_comments(task, *revision, digest, by, *accept, comments.clone())?,
            *json,
            false,
        ),
        Commands::BoardRetry {
            task,
            revision,
            by,
            reset,
            from_step,
            extra_steps,
            guidance,
            run,
            json,
        } => {
            let retried = service.retry(
                task,
                *revision,
                by,
                *reset,
                *from_step,
                *extra_steps,
                guidance.clone(),
            )?;
            if *run {
                let outcome = service.run(&retried.id, retried.revision)?;
                (outcome.detail, *json, !outcome.succeeded)
            } else {
                (retried, *json, false)
            }
        }
        _ => unreachable!(),
    };
    if json {
        print(&detail)?;
    } else {
        println!("{}", board::render_detail(&detail));
    }
    ensure!(
        !failed,
        "attempt failed; inspect the recorded task and preserved workspace"
    );
    Ok(true)
}

/// `board-depend`/`board-leases`/`board-leases-acknowledge`: M6's task-dependency and
/// repository-leasing CLI surface. Separate from `board_write_command` because none of
/// these return a `board::Detail` for a single task the way every other board-* write
/// command does.
fn board_coordination_command(cli: &Cli) -> Result<bool> {
    use clatterforge::service::TaskService;
    match &cli.command {
        Commands::BoardDepend { task, on } => {
            TaskService::open(&cli.state)?.add_dependency(task, on)?;
            println!("recorded: {task} depends on {on}");
        }
        Commands::BoardLeases { json } => {
            let leases = TaskService::open(&cli.state)?.leases()?;
            if *json {
                print(&leases)?;
            } else if leases.is_empty() {
                println!("no active leases");
            } else {
                for lease in &leases {
                    println!(
                        "{}  task={}  quarantined={}  owner={}",
                        lease["resource"].as_str().unwrap_or("?"),
                        lease["task_id"].as_str().unwrap_or("?"),
                        lease["quarantined"].as_bool().unwrap_or(false),
                        lease["owner"]
                    );
                }
            }
        }
        Commands::BoardLeasesAcknowledge {
            resource,
            by,
            reason,
        } => {
            TaskService::open(&cli.state)?.acknowledge_lease(resource, by, reason)?;
            println!("acknowledged: {resource} released");
        }
        Commands::BoardDelete { task, json } => {
            let deleted_id = TaskService::open(&cli.state)?.delete_task(task)?;
            if *json {
                print(&serde_json::json!({"deleted_id": deleted_id}))?;
            } else {
                println!("deleted: {deleted_id}");
            }
        }
        Commands::BoardClear { scope, json } => {
            let outcome = TaskService::open(&cli.state)?.clear_tasks(*scope)?;
            if *json {
                print(&outcome)?;
            } else {
                println!(
                    "cleared {} task(s) ({} skipped)",
                    outcome.deleted_ids.len(),
                    outcome.skipped.len()
                );
                for skipped in &outcome.skipped {
                    println!("  skipped {}: {}", skipped.id, skipped.reason);
                }
            }
        }
        _ => return Ok(false),
    }
    Ok(true)
}

fn render_policy(policy: &clatterforge::autonomy::Policy) -> String {
    let state = policy.state(clatterforge::now());
    let mut out = format!("policy {}  [{:?}]\n", policy.id, state);
    out.push_str(&format!("version:    {}\n", policy.version));
    out.push_str(&format!("repository: {}\n", policy.repository_id));
    out.push_str(&format!(
        "created:    by {} at {} (ms epoch)\n",
        policy.created_by, policy.created_at
    ));
    out.push_str(&format!("expires:    {} (ms epoch)\n", policy.expires_at));
    out.push_str(&format!("max tasks:  {}\n", policy.max_tasks));
    if let Some(reason) = &policy.revoked_reason {
        out.push_str(&format!(
            "revoked:    by {} - {reason}\n",
            policy.revoked_by.as_deref().unwrap_or("?")
        ));
    }
    if !policy.real_tasks.is_empty() {
        out.push_str("real tasks (local-changes-v1):\n");
        for task in &policy.real_tasks {
            out.push_str(&format!(
                "  - {}  task={}\n",
                task.spec.objective,
                task.task_id.as_deref().unwrap_or("(not dispatched)")
            ));
        }
        return out;
    }
    out.push_str("experiments:\n");
    for experiment in &policy.experiments {
        out.push_str(&format!(
            "  - {}  task={}\n",
            experiment.objective,
            experiment.task_id.as_deref().unwrap_or("(not dispatched)")
        ));
    }
    out
}

fn agent_command(cli: &Cli) -> Result<bool> {
    use clatterforge::{
        agents::{AgentBackend, AgentSpec, Agents, AgyAgentPolicy, CursorAgentPolicy},
        codex::{CodexPolicy, Executable, PinnedCommand},
        inference::config::read_json,
    };
    match &cli.command {
        Commands::AgentCreate {
            spec,
            cursor_node,
            cursor_index_js,
            json,
        } => {
            let spec: AgentSpec = read_json(spec)?;
            let cursor =
                CursorAgentPolicy::inspect(cursor_node, cursor_index_js, spec.model.clone())?;
            let mut agents = Agents::open(&cli.state)?;
            let task = agents.create(spec, AgentBackend::CursorAgent(cursor))?;
            let digest = task.assignment.digest()?;
            if *json {
                print(&serde_json::json!({"task": task, "assignment_digest": digest}))?;
            } else {
                println!("agent task {}  revision {}", task.id, task.revision);
                println!("assignment_digest: {digest}");
            }
        }
        Commands::AgentCreateCodex {
            spec,
            codex_executable,
            codex_home,
            json,
        } => {
            let spec: AgentSpec = read_json(spec)?;
            // The verifier is pinned twice from the exact same source -
            // once here for `CodexPolicy::inspect`'s own integrity check
            // (it validates its own `verifier` field), once again inside
            // `Agents::create` for the engine's own scope/verify logic -
            // not two independently-trusted pins, the same one reused.
            let verifier = PinnedCommand {
                executable: Executable::new(&spec.verify_executable)?,
                args: spec.verify_args.clone(),
            };
            let codex =
                CodexPolicy::inspect(codex_executable, codex_home, spec.model.clone(), verifier)?;
            let mut agents = Agents::open(&cli.state)?;
            let task = agents.create(spec, AgentBackend::Codex(codex))?;
            let digest = task.assignment.digest()?;
            if *json {
                print(&serde_json::json!({"task": task, "assignment_digest": digest}))?;
            } else {
                println!("agent task {}  revision {}", task.id, task.revision);
                println!("assignment_digest: {digest}");
            }
        }
        Commands::AgentCreateAgy {
            spec,
            agy_executable,
            json,
        } => {
            let spec: AgentSpec = read_json(spec)?;
            let agy = AgyAgentPolicy::inspect(agy_executable, spec.model.clone())?;
            let mut agents = Agents::open(&cli.state)?;
            let task = agents.create(spec, AgentBackend::Agy(agy))?;
            let digest = task.assignment.digest()?;
            if *json {
                print(&serde_json::json!({"task": task, "assignment_digest": digest}))?;
            } else {
                println!("agent task {}  revision {}", task.id, task.revision);
                println!("assignment_digest: {digest}");
            }
        }
        Commands::AgentList { json } => {
            let agents = Agents::open(&cli.state)?;
            let tasks = agents.tasks()?;
            if *json {
                print(&tasks)?;
            } else {
                for task in &tasks {
                    println!("{}  {:?}", task.id, task.status);
                }
            }
        }
        Commands::AgentShow { task, json } => {
            let agents = Agents::open(&cli.state)?;
            let task = agents.task(task)?;
            if *json {
                print(&task)?;
            } else {
                println!("{task:#?}");
            }
        }
        Commands::AgentApprove {
            task,
            revision,
            digest,
            by,
            json,
        } => {
            let mut agents = Agents::open(&cli.state)?;
            let task = agents.approve(task, *revision, digest, by)?;
            if *json {
                print(&task)?;
            } else {
                println!("{task:#?}");
            }
        }
        Commands::AgentRun {
            task,
            revision,
            json,
        } => {
            let mut agents = Agents::open(&cli.state)?;
            let task = agents.run(task, *revision)?;
            if *json {
                print(&task)?;
            } else {
                println!("{task:#?}");
            }
        }
        Commands::AgentCancel {
            task,
            revision,
            json,
        } => {
            let mut agents = Agents::open(&cli.state)?;
            let task = agents.cancel(task, *revision)?;
            if *json {
                print(&task)?;
            } else {
                println!("{task:#?}");
            }
        }
        Commands::AgentReview {
            task,
            revision,
            digest,
            by,
            accept,
            json,
        } => {
            let mut agents = Agents::open(&cli.state)?;
            let task = agents.review(task, *revision, digest, by, *accept)?;
            if *json {
                print(&task)?;
            } else {
                println!("{task:#?}");
            }
        }
        Commands::AgentReconcile {
            task,
            revision,
            json,
        } => {
            let mut agents = Agents::open(&cli.state)?;
            let task = agents.reconcile(task, *revision)?;
            if *json {
                print(&task)?;
            } else {
                println!("{task:#?}");
            }
        }
        _ => return Ok(false),
    }
    Ok(true)
}

fn render_agent_preset(preset: &clatterforge::agent_preset::AgentPreset) -> String {
    let mut out = format!("agent preset '{}'\n", preset.name);
    if let Some(agentlet) = &preset.agentlet {
        out.push_str(&format!("agentlet:     {} v{} ({:?}, required review: {})\npurpose:      {}\ninstructions:  {}\n",
            agentlet.id,agentlet.version,agentlet.capability,agentlet.completion_gate,
            agentlet.purpose,agentlet.instructions));
    }
    out.push_str(&format!("repository:   {}\n", preset.repository.display()));
    out.push_str(&format!(
        "backend:      {} (model: {}, tier: {})\n",
        match &preset.backend {
            clatterforge::agents::AgentBackend::CursorAgent(_) => "cursor-agent",
            clatterforge::agents::AgentBackend::Codex(_) => "codex",
            clatterforge::agents::AgentBackend::Agy(_) => "agy",
            clatterforge::agents::AgentBackend::Local(_) => "local-model",
        },
        preset.backend.model(),
        preset.effective_tier().as_str()
    ));
    out.push_str(&format!(
        "allowed models:     {}\n",
        if preset.allowed_models.is_empty() {
            "(live CLI catalog / default)".to_string()
        } else {
            preset.allowed_models.join(", ")
        }
    ));
    out.push_str(&format!(
        "allowed files:      {}\n",
        preset.allowed_files.join(", ")
    ));
    out.push_str(&format!(
        "verification files: {}\n",
        preset.verification_files.join(", ")
    ));
    out.push_str(&format!(
        "verifier:     {} {}\n",
        preset.verify_executable.display(),
        preset.verify_args.join(" ")
    ));
    out.push_str(&format!(
        "timeouts:     generation {}ms, verification {}ms\n",
        preset.generation_timeout_ms, preset.verification_timeout_ms
    ));
    if let Some(idle) = preset.generation_idle_timeout_ms {
        out.push_str(&format!(
            "generation inactivity limit: {idle}ms (does not extend the hard limit)\n"
        ));
    }
    out.push_str(&format!(
        "created:      by {} at {} (ms epoch)\n",
        preset.created_by, preset.created_at
    ));
    out
}

fn agent_preset_command(cli: &Cli) -> Result<bool> {
    use clatterforge::{
        agent_preset::AgentPresets,
        agents::{AgentBackend, AgyAgentPolicy, CursorAgentPolicy},
        codex::{CodexPolicy, Executable, PinnedCommand},
    };
    match &cli.command {
        Commands::AgentletImport { definition, by } => {
            print(
                &clatterforge::agentlet::Agentlets::open(&cli.state)?
                    .import(clatterforge::agentlet::Definition::load(definition)?, by)?,
            )?;
        }
        Commands::AgentletList => {
            print(&clatterforge::agentlet::Agentlets::open(&cli.state)?.list()?)?
        }
        Commands::AgentletShow { id, version } => {
            print(&clatterforge::agentlet::Agentlets::open(&cli.state)?.get(id, *version)?)?;
        }
        Commands::AgentletBind {
            id,
            version,
            preset,
            name,
            by,
        } => {
            let record = clatterforge::agentlet::Agentlets::open(&cli.state)?.get(id, *version)?;
            print(&AgentPresets::open(&cli.state)?.bind_agentlet(
                preset,
                name,
                record.definition,
                by,
            )?)?;
        }
        Commands::AgentPresetCreate {
            name,
            repository,
            model,
            cursor_node,
            cursor_index_js,
            codex_executable,
            codex_home,
            agy_executable,
            allowed_files,
            verification_files,
            verify_executable,
            verify_args,
            generation_timeout_ms,
            generation_idle_timeout_ms,
            verification_timeout_ms,
            by,
            json,
        } => {
            let backend = match (
                cursor_node,
                cursor_index_js,
                codex_executable,
                codex_home,
                agy_executable,
            ) {
                (Some(node), Some(index_js), None, None, None) => AgentBackend::CursorAgent(
                    CursorAgentPolicy::inspect(node, index_js, model.clone())?,
                ),
                (None, None, Some(executable), Some(home), None) => {
                    let verifier = PinnedCommand {
                        executable: Executable::new(verify_executable)?,
                        args: verify_args.clone(),
                    };
                    AgentBackend::Codex(CodexPolicy::inspect(
                        executable,
                        home,
                        model.clone(),
                        verifier,
                    )?)
                }
                (None, None, None, None, Some(executable)) => {
                    AgentBackend::Agy(AgyAgentPolicy::inspect(executable, model.clone())?)
                }
                _ => bail!(
                    "specify exactly one backend: --cursor-node + --cursor-index-js, --codex-executable + --codex-home, or --agy-executable"
                ),
            };
            let preset = AgentPresets::open(&cli.state)?.create(
                name,
                repository.clone(),
                backend,
                allowed_files.clone(),
                verification_files.clone(),
                verify_executable.clone(),
                verify_args.clone(),
                *generation_timeout_ms,
                *verification_timeout_ms,
                *generation_idle_timeout_ms,
                by,
            )?;
            if *json {
                print(&preset)?;
            } else {
                println!("{}", render_agent_preset(&preset));
            }
        }
        Commands::AgentPresetList { json } => {
            let presets = AgentPresets::open(&cli.state)?.list()?;
            if *json {
                print(&presets)?;
            } else if presets.is_empty() {
                println!("no agent presets");
            } else {
                for preset in &presets {
                    println!("{}", render_agent_preset(preset));
                }
            }
        }
        Commands::AgentPresetShow { name, json } => {
            let preset = AgentPresets::open(&cli.state)?.get(name)?;
            if *json {
                print(&preset)?;
            } else {
                println!("{}", render_agent_preset(&preset));
            }
        }
        Commands::AgentPresetDelete { name } => {
            AgentPresets::open(&cli.state)?.delete(name)?;
            println!("deleted agent preset '{name}'");
        }
        Commands::AgentPresetRouting {
            name,
            tier,
            allowed_models,
            json,
        } => {
            let parsed_tier = match tier.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
                None | Some("auto") | Some("none") => None,
                Some("local") => Some(clatterforge::agents::ModelTier::Local),
                Some("fast") => Some(clatterforge::agents::ModelTier::Fast),
                Some("standard") => Some(clatterforge::agents::ModelTier::Standard),
                Some("frontier") => Some(clatterforge::agents::ModelTier::Frontier),
                Some(other) => bail!(
                    "unknown model tier '{other}'; expected one of: local, fast, standard, frontier"
                ),
            };
            let cleaned: Vec<String> = allowed_models
                .iter()
                .map(|m| m.trim().to_string())
                .filter(|m| !m.is_empty())
                .collect();
            let preset =
                AgentPresets::open(&cli.state)?.set_routing_policy(name, parsed_tier, cleaned)?;
            if *json {
                print(&preset)?;
            } else {
                println!("{}", render_agent_preset(&preset));
            }
        }
        _ => return Ok(false),
    }
    Ok(true)
}

fn policy_command(cli: &Cli) -> Result<bool> {
    use clatterforge::{
        autonomy::{Policies, run as hold_my_beer_run},
        inference::config::read_json,
        process::ExecutionPolicy,
    };
    match &cli.command {
        Commands::PolicyCreate {
            repository,
            by,
            max_tasks,
            expires_in_ms,
            experiments,
            real_tasks,
            json,
        } => {
            ensure!(
                experiments.is_some() != real_tasks.is_some(),
                "exactly one of --experiments or --real-tasks is required"
            );
            let policy = if let Some(real_tasks) = real_tasks {
                let authorization: clatterforge::autonomy::RealPolicyAuthorization =
                    read_json(real_tasks)?;
                Policies::open(&cli.state)?.create_real(
                    repository,
                    by,
                    *max_tasks,
                    clatterforge::now() + expires_in_ms,
                    clatterforge::autonomy::RealPolicyAuthorizationShared {
                        config: authorization.config,
                        launcher: std::env::current_exe()?,
                    },
                    authorization.tasks,
                )?
            } else {
                let experiments = read_json(experiments.as_ref().unwrap())?;
                Policies::open(&cli.state)?.create(
                    repository,
                    by,
                    *max_tasks,
                    clatterforge::now() + expires_in_ms,
                    experiments,
                )?
            };
            if *json {
                print(&policy)?;
            } else {
                println!("{}", render_policy(&policy));
            }
        }
        Commands::PolicyShow { policy, json } => {
            let policy = Policies::open(&cli.state)?.get(policy)?;
            if *json {
                print(&policy)?;
            } else {
                println!("{}", render_policy(&policy));
            }
        }
        Commands::PolicyRevoke {
            policy,
            by,
            reason,
            json,
        } => {
            let policy = Policies::open(&cli.state)?.revoke(policy, by, reason)?;
            if *json {
                print(&policy)?;
            } else {
                println!("{}", render_policy(&policy));
            }
        }
        Commands::HoldMyBeer {
            policy,
            budget,
            timeout_ms,
            verification_timeout_ms,
            log_bytes,
            json,
        } => {
            let mut execution = ExecutionPolicy::new(&std::env::current_exe()?)?;
            execution.timeout_ms = *timeout_ms;
            execution.verification_timeout_ms = *verification_timeout_ms;
            execution.log_bytes = *log_bytes;
            let outcome = hold_my_beer_run(&cli.state, policy, execution, *budget)?;
            if *json {
                print(&outcome)?;
            } else {
                println!("policy: {}", outcome.policy_id);
                println!("dispatched: {}", outcome.dispatched.len());
                for id in &outcome.dispatched {
                    println!("  - {id}");
                }
                println!("stopped: {}", outcome.stopped);
            }
        }
        _ => return Ok(false),
    }
    Ok(true)
}

fn parse_review_comment(value: &str) -> Result<clatterforge::store::ReviewComment, String> {
    clatterforge::store::ReviewComment::from_anchor_text(value).map_err(|e| e.to_string())
}

fn parse_operations(values: &[String]) -> Result<Vec<clatterforge::authorization::Operation>> {
    use clatterforge::authorization::Operation;
    values
        .iter()
        .map(|v| match v.trim() {
            "submit" => Ok(Operation::Submit),
            "approve" => Ok(Operation::Approve),
            "reject" => Ok(Operation::Reject),
            "resume" => Ok(Operation::Resume),
            "cancel" => Ok(Operation::Cancel),
            other => bail!(
                "unknown operation '{other}' - expected one of submit,approve,reject,resume,cancel"
            ),
        })
        .collect()
}

fn render_grant(grant: &clatterforge::authorization::Grant) -> String {
    let state = grant.state(clatterforge::now());
    let mut out = format!("grant {}  [{:?}]\n", grant.id, state);
    out.push_str(&format!("version:     {}\n", grant.version));
    out.push_str(&format!("principal:   {}\n", grant.principal));
    out.push_str(&format!("repository:  {}\n", grant.repository_identity));
    out.push_str(&format!("backends:    {}\n", grant.backends.join(", ")));
    out.push_str(&format!(
        "operations:  {}\n",
        grant
            .operations
            .iter()
            .map(|o| o.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    ));
    out.push_str(&format!("mode:        {:?}\n", grant.mode));
    out.push_str(&format!(
        "created:     by {} at {} (ms epoch)\n",
        grant.created_by, grant.created_at
    ));
    out.push_str(&format!("expires:     {} (ms epoch)\n", grant.expires_at));
    out.push_str(&format!(
        "max tasks:   {} (dispatched: {})\n",
        grant.max_tasks, grant.dispatched
    ));
    if let Some(reason) = &grant.revoked_reason {
        out.push_str(&format!(
            "revoked:     by {} - {reason}\n",
            grant.revoked_by.as_deref().unwrap_or("?")
        ));
    }
    out
}

fn grant_command(cli: &Cli) -> Result<bool> {
    use clatterforge::{authorization::Grants, repository::Repository};
    match &cli.command {
        Commands::GrantCreate {
            principal,
            repository,
            backends,
            operations,
            hold_my_beer,
            by,
            max_tasks,
            expires_in_ms,
            json,
        } => {
            use clatterforge::authorization::ExecutionMode;
            ensure!(
                !backends.is_empty(),
                "--backends requires at least one value"
            );
            let identity = Repository::inspect(repository)?.identity;
            let operations = parse_operations(operations)?;
            let mode = if *hold_my_beer {
                ExecutionMode::HoldMyBeer
            } else {
                ExecutionMode::Delegated
            };
            let grant = Grants::open(&cli.state)?.create(
                principal,
                &identity,
                backends.clone(),
                operations,
                mode,
                by,
                *max_tasks,
                clatterforge::now() + expires_in_ms,
            )?;
            if *json {
                print(&grant)?;
            } else {
                println!("{}", render_grant(&grant));
            }
        }
        Commands::GrantList { json } => {
            let grants = Grants::open(&cli.state)?.list()?;
            if *json {
                print(&grants)?;
            } else if grants.is_empty() {
                println!("no grants");
            } else {
                for grant in &grants {
                    println!("{}", render_grant(grant));
                }
            }
        }
        Commands::GrantShow { grant, json } => {
            let grant = Grants::open(&cli.state)?.get(grant)?;
            if *json {
                print(&grant)?;
            } else {
                println!("{}", render_grant(&grant));
            }
        }
        Commands::GrantRevoke {
            grant,
            by,
            reason,
            json,
        } => {
            let grant = Grants::open(&cli.state)?.revoke(grant, by, reason)?;
            if *json {
                print(&grant)?;
            } else {
                println!("{}", render_grant(&grant));
            }
        }
        _ => return Ok(false),
    }
    Ok(true)
}

fn parse_enrollment_operations(
    values: &[String],
) -> Result<Vec<clatterforge::enrollment::EnrollmentOperation>> {
    use clatterforge::enrollment::EnrollmentOperation;
    values
        .iter()
        .map(|v| match v.trim() {
            "inspect" => Ok(EnrollmentOperation::Inspect),
            "submit" => Ok(EnrollmentOperation::Submit),
            "approve" => Ok(EnrollmentOperation::Approve),
            "reject" => Ok(EnrollmentOperation::Reject),
            "resume" => Ok(EnrollmentOperation::Resume),
            "cancel" => Ok(EnrollmentOperation::Cancel),
            other => bail!(
                "unknown operation '{other}' - expected one of inspect,submit,approve,reject,resume,cancel"
            ),
        })
        .collect()
}

fn render_enrollment(enrollment: &clatterforge::enrollment::Enrollment) -> String {
    let mut out = format!(
        "enrollment {}  [{}]\n",
        enrollment.client_id,
        if enrollment.revoked {
            "Revoked"
        } else {
            "Active"
        }
    );
    out.push_str(&format!("version:      {}\n", enrollment.version));
    out.push_str(&format!("display name: {}\n", enrollment.display_name));
    out.push_str(&format!("client type:  {}\n", enrollment.client_type));
    out.push_str(&format!(
        "operations:   {}\n",
        enrollment
            .allowed_operations
            .iter()
            .map(|o| o.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    ));
    out.push_str(&format!(
        "repositories: {}\n",
        if enrollment.repository_scope.is_empty() {
            "(none)".to_string()
        } else {
            enrollment.repository_scope.join(", ")
        }
    ));
    out.push_str(&format!(
        "created:      by {} at {} (ms epoch)\n",
        enrollment.created_by, enrollment.created_at
    ));
    out.push_str(&format!(
        "credential:   created {} (ms epoch){}\n",
        enrollment.credential_created_at,
        enrollment
            .credential_rotated_at
            .map(|t| format!(", last rotated {t} (ms epoch)"))
            .unwrap_or_default()
    ));
    if let Some(reason) = &enrollment.revoked_reason {
        out.push_str(&format!(
            "revoked:      by {} at {} (ms epoch) - {reason}\n",
            enrollment.revoked_by.as_deref().unwrap_or("?"),
            enrollment
                .revoked_at
                .map(|a| a.to_string())
                .unwrap_or_else(|| "?".into())
        ));
    }
    out
}

/// Shared by `enrollment-create` and `enrollment-rotate-credential`: a freshly issued
/// plaintext credential is shown exactly once, either on stdout with an explicit
/// one-time warning or written to `--token-file` - never both, and never logged anywhere
/// else afterward (see `src/enrollment.rs`'s module doc).
fn emit_credential(token: &str, token_file: &Option<PathBuf>) -> Result<()> {
    match token_file {
        Some(path) => {
            std::fs::write(path, token)
                .with_context(|| format!("write credential to {}", path.display()))?;
            println!("credential written to {} (not printed)", path.display());
        }
        None => {
            println!("credential: {token}");
            println!("(save this now - it will not be shown again)");
        }
    }
    Ok(())
}

fn enrollment_command(cli: &Cli) -> Result<bool> {
    use clatterforge::{
        authorization::Grants,
        enrollment::{self, Enrollments},
        repository::Repository,
    };
    match &cli.command {
        Commands::EnrollmentCreate {
            display_name,
            client_type,
            operations,
            repositories,
            by,
            token_file,
            json,
        } => {
            let operations = parse_enrollment_operations(operations)?;
            let mut repository_scope = Vec::with_capacity(repositories.len());
            for repository in repositories {
                repository_scope.push(Repository::inspect(repository)?.identity);
            }
            let (enrollment, token) = Enrollments::open(&cli.state)?.create(
                display_name,
                client_type,
                operations,
                repository_scope,
                by,
            )?;
            if *json {
                print(&enrollment)?;
            } else {
                println!("{}", render_enrollment(&enrollment));
            }
            emit_credential(&token, token_file)?;
        }
        Commands::EnrollmentList { json } => {
            let enrollments = Enrollments::open(&cli.state)?.list()?;
            if *json {
                print(&enrollments)?;
            } else if enrollments.is_empty() {
                println!("no enrollments");
            } else {
                for enrollment in &enrollments {
                    println!("{}", render_enrollment(enrollment));
                }
            }
        }
        Commands::EnrollmentShow { client, json } => {
            let enrollment = Enrollments::open(&cli.state)?.get(client)?;
            if *json {
                print(&enrollment)?;
            } else {
                println!("{}", render_enrollment(&enrollment));
            }
        }
        Commands::EnrollmentRevoke {
            client,
            by,
            reason,
            json,
        } => {
            let enrollment = Enrollments::open(&cli.state)?.revoke(client, by, reason)?;
            if *json {
                print(&enrollment)?;
            } else {
                println!("{}", render_enrollment(&enrollment));
            }
        }
        Commands::EnrollmentRotateCredential {
            client,
            by,
            token_file,
            json,
        } => {
            let (enrollment, token) =
                Enrollments::open(&cli.state)?.rotate_credential(client, by)?;
            if *json {
                print(&enrollment)?;
            } else {
                println!("{}", render_enrollment(&enrollment));
            }
            emit_credential(&token, token_file)?;
        }
        Commands::EnrollmentEffectivePermissions { client, json } => {
            let enrollments = Enrollments::open(&cli.state)?;
            let enrollment = enrollments.get(client)?;
            let grants = Grants::open(&cli.state)?;
            let grants = enrollment::grants_for(&grants, client)?;
            if *json {
                print(&serde_json::json!({"enrollment": enrollment, "grants": grants}))?;
            } else {
                println!("{}", render_enrollment(&enrollment));
                if grants.is_empty() {
                    println!("associated grants: (none)");
                } else {
                    println!("associated grants:");
                    for grant in &grants {
                        println!("{}", render_grant(grant));
                    }
                }
            }
        }
        _ => return Ok(false),
    }
    Ok(true)
}

fn render_stop_state(state: &clatterforge::stop::StopState) -> String {
    if state.stopped {
        format!(
            "global stop: ENGAGED\n  by:     {}\n  reason: {}\n  at:     {} (ms epoch)\n",
            state.by.as_deref().unwrap_or("?"),
            state.reason.as_deref().unwrap_or("?"),
            state
                .at
                .map(|a| a.to_string())
                .unwrap_or_else(|| "?".into())
        )
    } else {
        "global stop: not engaged\n".to_string()
    }
}

fn stop_command(cli: &Cli) -> Result<bool> {
    use clatterforge::service::TaskService;
    match &cli.command {
        Commands::GlobalStop { by, reason, json } => {
            let outcome = TaskService::open(&cli.state)?.stop_all(by, reason)?;
            if *json {
                print(&outcome)?;
            } else {
                print!("{}", render_stop_state(&outcome.state));
                println!("cancelled: {}", outcome.cancelled.len());
                for id in &outcome.cancelled {
                    println!("  - {id}");
                }
                if !outcome.not_cancelled.is_empty() {
                    println!("not cancelled (already settled or refused):");
                    for line in &outcome.not_cancelled {
                        println!("  - {line}");
                    }
                }
            }
        }
        Commands::GlobalStopStatus { json } => {
            let state = TaskService::open(&cli.state)?.stop_state()?;
            if *json {
                print(&state)?;
            } else {
                print!("{}", render_stop_state(&state));
            }
        }
        Commands::GlobalResume { by, reason, json } => {
            let state = TaskService::open(&cli.state)?.resume(by, reason)?;
            if *json {
                print(&state)?;
            } else {
                print!("{}", render_stop_state(&state));
            }
        }
        _ => return Ok(false),
    }
    Ok(true)
}

fn orchestration_command(cli: &Cli) -> Result<bool> {
    use clatterforge::{
        inference::config::{Config, read_json},
        orchestration::{Orchestrator, Status},
    };
    if !matches!(
        &cli.command,
        Commands::OrchestratePrepare { .. }
            | Commands::OrchestrateList
            | Commands::OrchestrateShow { .. }
            | Commands::OrchestrateEvents { .. }
            | Commands::OrchestrateApprove { .. }
            | Commands::OrchestrateRun { .. }
            | Commands::OrchestrateCancel { .. }
            | Commands::OrchestrateReconcile { .. }
            | Commands::OrchestrateReview { .. }
    ) {
        return Ok(false);
    }
    let mut orchestrator = Orchestrator::open(&cli.state)?;
    let task = match &cli.command {
        Commands::OrchestratePrepare { spec, config } => orchestrator.create(
            read_json(spec)?,
            Config::load(config)?,
            &std::env::current_exe()?,
        )?,
        Commands::OrchestrateList => {
            let tasks = orchestrator.tasks()?.into_iter().map(|t| serde_json::json!({"id":t.id,"revision":t.revision,"status":t.status,"objective":t.assignment.spec.objective,"repository":t.assignment.repository.checkout,"workspace":t.assignment.workspace,"rounds":t.rounds.len()})).collect::<Vec<_>>();
            print(&tasks)?;
            return Ok(true);
        }
        Commands::OrchestrateShow { task, summary } => {
            let task = orchestrator.task(task)?;
            if *summary {
                print(&orchestrator.summary(&task)?)?;
                return Ok(true);
            }
            task
        }
        Commands::OrchestrateEvents { task } => {
            print(&orchestrator.events(task)?)?;
            return Ok(true);
        }
        Commands::OrchestrateApprove {
            task,
            revision,
            digest,
            by,
        } => orchestrator.approve(task, *revision, digest, by)?,
        Commands::OrchestrateRun { task, revision } => {
            clatterforge::stop::GlobalStop::open(&cli.state)?
                .state()?
                .ensure_admission_allowed()?;
            // M10.4 Phase 1: same lease, same shared helper, same rationale
            // as `Commands::Run` above.
            let resource = orchestrator
                .task(task)?
                .assignment
                .repository
                .identity
                .clone();
            clatterforge::service::with_repository_lease(
                &cli.state,
                resource,
                task.clone(),
                || orchestrator.run(task, *revision),
            )?
        }
        Commands::OrchestrateCancel { task, revision } => orchestrator.cancel(task, *revision)?,
        Commands::OrchestrateReconcile { task, revision } => {
            orchestrator.reconcile(task, *revision)?
        }
        Commands::OrchestrateReview {
            task,
            revision,
            digest,
            by,
            accept,
        } => orchestrator.review(task, *revision, digest, by, *accept)?,
        _ => unreachable!(),
    };
    print(
        &serde_json::json!({"assignment_digest":task.assignment.digest()?,"evidence_digest":orchestrator.evidence_digest(&task)?,"task":task,"process_sandbox":false}),
    )?;
    if matches!(cli.command, Commands::OrchestrateRun { .. }) {
        ensure!(
            task.status == Status::AwaitingReview,
            "orchestration did not pass independent verification; inspect retained task/evidence"
        );
    }
    Ok(true)
}

fn inference_command(cli: &Cli) -> Result<bool> {
    use clatterforge::inference::{
        config::{Config, Registry, read_json},
        jobs,
        state::{Coordinator, JobStatus, JobStore},
        types::Request,
    };
    match &cli.command {
        Commands::Capabilities => print(&serde_json::json!({
            "orchestrator":{"backend":"local-changes-v1","supervised":true,"scoped_file_replacements":true,"independent_verifier":true,"bounded_repair":true,"automatic_integration":false,"process_sandbox":false,"parallel_scheduler":false},
            "execution_engines":[CodexEngine.capabilities()],
            "inference_providers":{"contract_version":1,"adapters":["ollama","openai_local"],
                "scope":"bounded text generation and application-owned pure checked_add; no coding assistant",
                "adapter_support":{"streaming":true,"serial_tools":true,"media":false,"structured_output":false,"parallel_tools":false,"exact_token_counting":false,"runtime_load_unload":false,"server_abort_confirmation":false},
                "target_capabilities":"configured/advertised and separately tested registry provenance; not inferred from adapter support"}}))?,
        Commands::InferenceCreate {
            config,
            request,
            target,
            registry,
        } => {
            let config = Config::load(config)?;
            let registry = registry
                .as_ref()
                .map(|p| Registry::import(p, &config))
                .transpose()?;
            let request: Request = read_json(request)?;
            print(&JobStore::open(&cli.state)?.create(
                config,
                registry,
                request,
                target.clone(),
            )?)?;
        }
        Commands::InferenceRun {
            job,
            revision,
            target,
            one_turn,
        } => {
            clatterforge::stop::GlobalStop::open(&cli.state)?
                .state()?
                .ensure_admission_allowed()?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let result = runtime.block_on(jobs::run(
                &cli.state,
                job,
                *revision,
                target.as_deref(),
                *one_turn,
            ))?;
            print(&result)?;
            ensure!(
                matches!(result.status, JobStatus::Completed | JobStatus::Checkpoint),
                "direct job failed; inspect persisted generation/provenance; never automatically replay"
            );
        }
        Commands::InferenceShow { job, summary } => {
            let job = JobStore::open(&cli.state)?.job(job)?;
            if *summary {
                print(&clatterforge::telemetry::job_summary(&job))?;
            } else {
                print(&job)?;
            }
        }
        Commands::InferenceEvents { job } => print(&JobStore::open(&cli.state)?.events(job)?)?,
        Commands::InferenceCancel { job, revision } => {
            print(&JobStore::open(&cli.state)?.cancel(job, *revision)?)?
        }
        Commands::InferenceReconcile { job, revision } => {
            print(&JobStore::open(&cli.state)?.reconcile(job, *revision)?)?
        }
        Commands::InferenceRegistryImport { config, registry } => {
            let config = Config::load(config)?;
            let registry = Registry::import(registry, &config)?;
            std::fs::create_dir_all(&cli.state)?;
            let path = cli.state.join("inference-registry.json");
            let tmp = path.with_extension("writing");
            std::fs::write(&tmp, serde_json::to_vec_pretty(&registry)?)?;
            std::fs::rename(tmp, &path)?;
            print(
                &serde_json::json!({"schema_version":registry.schema_version,"records":registry.records.len(),"path":path,"measurement_authority":"imported assertions; validation is not a benchmark run"}),
            )?;
        }
        Commands::InferenceResources { config } => {
            print(&Coordinator::open(&Config::load(config)?.admission_db)?.reservations()?)?
        }
        Commands::InferenceReleaseResource {
            config,
            resource,
            by,
            reason,
        } => {
            Coordinator::open(&Config::load(config)?.admission_db)?
                .acknowledge(resource, by, reason)?;
            print(
                &serde_json::json!({"resource":resource,"released_by_explicit_acknowledgement":true,"server_abort_confirmed":false}),
            )?;
        }
        _ => return Ok(false),
    }
    Ok(true)
}

fn workflow_command(cli: &Cli) -> Result<bool> {
    use clatterforge::{inference::config::Config, workflow::Workflows};
    match &cli.command {
        Commands::IntegrationPrepare { task, by, worktree } => print(
            &clatterforge::service::TaskService::open(&cli.state)?
                .prepare_integration_with_target(
                    task,
                    by,
                    if *worktree {
                        clatterforge::integration::IntegrationTarget::Worktree
                    } else {
                        clatterforge::integration::IntegrationTarget::Checkout
                    },
                )?,
        )?,
        Commands::IntegrationShow { task } => {
            let service = clatterforge::service::TaskService::open(&cli.state)?;
            print(&service.detail(task, None)?.integration)?;
        }
        Commands::IntegrationApply {
            task,
            revision,
            digest,
            by,
        }
        | Commands::IntegrationRollback {
            task,
            revision,
            digest,
            by,
        } => {
            let rollback = matches!(&cli.command, Commands::IntegrationRollback { .. });
            let d = clatterforge::service::TaskService::open(&cli.state)?
                .integrate(task, *revision, digest, by, rollback)?;
            print(&d)?;
            ensure!(
                d.integration
                    .as_ref()
                    .is_some_and(|r| r.status == if rollback { "rolled_back" } else { "applied" }),
                "source integration needs attention; inspect its recorded state"
            );
        }
        Commands::WorkflowProfileCreate {
            name,
            config,
            workers,
            max_steps,
            decision_timeout_ms,
            by,
        } => {
            let workers = workers
                .iter()
                .map(|v| {
                    v.split_once('=')
                        .map(|(a, b)| (a.to_owned(), b.to_owned()))
                        .context("worker must be NAME=PURPOSE")
                })
                .collect::<Result<Vec<_>>>()?;
            print(&Workflows::open(&cli.state)?.create_profile(
                name.clone(),
                Config::load(config)?,
                workers,
                *max_steps,
                *decision_timeout_ms,
                by.clone(),
            )?)?;
        }
        Commands::WorkflowProfileList => print(&Workflows::open(&cli.state)?.profiles()?)?,
        Commands::WorkflowProfileUpdate {
            name,
            config,
            workers,
            max_steps,
            decision_timeout_ms,
            by,
        } => {
            let workers = workers
                .iter()
                .map(|v| {
                    v.split_once('=')
                        .map(|(a, b)| (a.to_owned(), b.to_owned()))
                        .context("worker must be NAME=PURPOSE")
                })
                .collect::<Result<Vec<_>>>()?;
            print(&Workflows::open(&cli.state)?.upsert_profile(
                name.clone(),
                Config::load(config)?,
                workers,
                *max_steps,
                *decision_timeout_ms,
                by.clone(),
            )?)?;
        }
        Commands::WorkflowProfileTimeouts {
            name,
            generation_timeout_ms,
            generation_idle_timeout_ms,
            verification_timeout_ms,
            require_clean,
        } => {
            print(&Workflows::open(&cli.state)?.update_profile_timeouts(
                name,
                clatterforge::workflow::TimeoutOverrides {
                    generation_timeout_ms: *generation_timeout_ms,
                    generation_idle_timeout_ms: *generation_idle_timeout_ms,
                    verification_timeout_ms: *verification_timeout_ms,
                    soft_timeouts: None,
                    require_clean_checkout: *require_clean,
                },
            )?)?;
        }
        Commands::WorkflowProfileDelete { name } => {
            Workflows::open(&cli.state)?.delete_profile(name)?;
            println!("deleted workflow profile '{name}'");
        }
        Commands::WorkflowCreate {
            profile,
            objective,
            acceptance,
            allowed_files,
            auto_scope,
            generation_timeout_ms,
            generation_idle_timeout_ms,
            verification_timeout_ms,
            require_clean,
        } => {
            let mut workflows = Workflows::open(&cli.state)?;
            let scope = if !allowed_files.is_empty() {
                Some(allowed_files.clone())
            } else if *auto_scope {
                let prof = workflows.profile(profile)?;
                let suggestion = clatterforge::repo_index::suggest_files_for_objective(
                    &cli.state, &prof, objective, acceptance, 16,
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
                profile,
                objective.clone(),
                acceptance.clone(),
                scope,
                clatterforge::workflow::TimeoutOverrides {
                    generation_timeout_ms: *generation_timeout_ms,
                    generation_idle_timeout_ms: *generation_idle_timeout_ms,
                    verification_timeout_ms: *verification_timeout_ms,
                    soft_timeouts: None,
                    require_clean_checkout: *require_clean,
                },
            )?;
            print(&serde_json::json!({"assignment_digest":w.assignment.digest()?,"workflow":w}))?;
        }
        Commands::ReOnboard {
            name,
            low_spec,
            refresh_scope,
            refresh_verifier,
            by,
        } => {
            let mut cb = |_: &str, _: &str, _: u8, _: usize, _: usize| {};
            let profile = clatterforge::onboard::reonboard_profile_with_options(
                &cli.state,
                name,
                *low_spec,
                *refresh_scope,
                *refresh_verifier,
                by,
                &clatterforge::onboard::DiscoveryEnv::from_process(),
                &mut cb,
            )?;
            print(&profile)?;
        }
        Commands::WorkflowShow { task } => print(&Workflows::open(&cli.state)?.get(task)?)?,
        Commands::Onboard {
            repo,
            name,
            model,
            endpoint,
            verifier,
            verifier_args,
            allowed_files,
            workers,
            yes,
            low_spec,
            probe_only,
            by,
            json,
        } => {
            use clatterforge::onboard::{OnboardRequest, execute_onboard, probe_environment};
            let report = probe_environment(repo, endpoint.as_deref())?;
            if *probe_only {
                print(&report)?;
                return Ok(true);
            }

            println!("== CLATTERFORGE // CF SYSTEMS — Project Onboard Probe ==");
            println!("Repository: {}", report.repository.display());
            println!(
                "Git clean: {}",
                if report.git_clean {
                    "yes"
                } else {
                    "NO (uncommitted changes detected)"
                }
            );
            println!("Base commit: {}", report.git_base_commit);
            if let Some(branch) = &report.git_branch {
                println!("Branch: {branch}");
            }
            if let Some(v) = &report.detected_verifier {
                println!(
                    "Detected verifier: {} (args: {:?}) [{}]",
                    v.executable.display(),
                    v.args,
                    v.framework
                );
                if !report.additional_verifiers.is_empty() {
                    let extras = report
                        .additional_verifiers
                        .iter()
                        .map(|av| format!("{} ({:?})", av.framework, av.args))
                        .collect::<Vec<_>>()
                        .join(", ");
                    println!("Additional verifiers detected: {extras}");
                }
            } else {
                println!("Detected verifier: None");
            }
            println!(
                "Available routing engines: {}",
                if report.available_models.is_empty() {
                    "builtin:router-v1 (0 MB footprint)".to_string()
                } else {
                    report
                        .available_models
                        .iter()
                        .map(|m| {
                            if m.name == "builtin:router-v1" {
                                "builtin:router-v1 (0 MB host router)".to_string()
                            } else {
                                m.name.clone()
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                }
            );
            println!(
                "Detected coding CLIs: {}",
                report
                    .detected_clis
                    .iter()
                    .map(|c| format!(
                        "{} ({})",
                        c.name,
                        if c.available { "ready" } else { "missing" }
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            );

            ensure!(
                report.git_clean || *yes,
                "repository has uncommitted changes; commit or stash them before onboarding (or use --yes to override)"
            );

            let selected_model = model
                .clone()
                .or_else(|| report.recommended_model.clone())
                .unwrap_or_else(|| "builtin:router-v1".into());

            let (verifier_exe, v_args, v_files) = if let Some(v_exe) = verifier {
                (
                    v_exe.clone(),
                    verifier_args.clone(),
                    report.detected_verification_files.clone(),
                )
            } else if let Some(dv) = &report.detected_verifier {
                (
                    dv.executable.clone(),
                    dv.args.clone(),
                    report.detected_verification_files.clone(),
                )
            } else {
                bail!(
                    "no test verifier detected; specify --verifier <path> and --verifier-arg <args>"
                );
            };

            let profile_name = name.clone().unwrap_or_else(|| {
                report
                    .repository
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string()
            });

            let final_allowed_files = if !allowed_files.is_empty() {
                allowed_files.clone()
            } else if !report.suggested_allowed_files.is_empty() {
                if report.total_source_files > report.suggested_allowed_files.len() {
                    println!(
                        "Note: repository has {} tracked source files; selecting top {} entrypoints/modules ({}) — pass --allowed-file to customize.",
                        report.total_source_files,
                        report.suggested_allowed_files.len(),
                        report.suggested_allowed_files.join(", ")
                    );
                }
                report.suggested_allowed_files.clone()
            } else {
                vec!["README.md".to_string()]
            };

            let req = OnboardRequest {
                profile_name: profile_name.clone(),
                repository_path: report.repository.clone(),
                model: selected_model.clone(),
                model_digest: None,
                endpoint: endpoint.clone(),
                verifier_executable: verifier_exe,
                verifier_args: v_args,
                verification_files: if v_files.is_empty() {
                    vec!["Cargo.toml".into()]
                } else {
                    v_files
                },
                allowed_files: final_allowed_files,
                workers: workers.clone(),
                low_spec: *low_spec,
                by: by.clone(),
            };

            let profile = execute_onboard(&cli.state, req)?;
            println!(
                "\n[CF // NOMINAL] Onboarded project profile '{}'!",
                profile.name
            );
            println!("- Routing Engine: {selected_model}");
            println!("- Repository: {}", profile.repository.checkout.display());
            println!(
                "- Workers: {}",
                profile
                    .workers
                    .iter()
                    .map(|w| format!(
                        "{} ({}:{}, {:?} tier)",
                        w.preset.name,
                        w.preset.backend.target(),
                        w.preset.backend.model(),
                        w.preset.effective_tier()
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            println!("\nReady to prepare workflow with:");
            println!(
                "  clatterforge workflow-create --profile {} --objective \"<objective>\" --acceptance \"<criteria>\"",
                profile.name
            );
            if *json {
                print(&profile)?;
            }
        }
        Commands::Analytics { json } => {
            let mut store = clatterforge::routing_metrics::RoutingMetricsStore::open(&cli.state)?;
            let board = store.scoreboard()?;
            let report = board.analytics_report();
            if *json {
                print(&report)?;
            } else {
                println!(
                    "== CLI / Model Routing Analytics ({} attempts) ==",
                    report.total_attempts
                );
                println!(
                    "Tokens: in={} out={} cached={} thinking={}",
                    report.total_input_tokens,
                    report.total_output_tokens,
                    report.total_cached_tokens,
                    report.total_thinking_tokens
                );
                println!("\nBy Backend:");
                for b in &report.by_backend {
                    println!(
                        "  {:12} attempts={:<3} pass={:.0}% fail={} timeout={} quota={} acc={} exc={} redo={} avg={}ms weight={:+.2}",
                        b.backend,
                        b.attempts,
                        b.pass_rate * 100.0,
                        b.exec_failed + b.verify_failed,
                        b.timed_out,
                        b.quota_exhausted,
                        b.accepted,
                        b.exceptional,
                        b.redone,
                        b.avg_duration_ms,
                        b.learned_weight
                    );
                }
                println!("\nBy Model & Complexity:");
                for m in &report.by_pair_complexity {
                    println!(
                        "  {:10} {:24} {:10} attempts={:<3} pass={:.0}% acc={} exc={} fail={} timeout={} redo={} avg={}ms weight={:+.2}",
                        m.backend,
                        m.model,
                        m.complexity.as_deref().unwrap_or("-"),
                        m.attempts,
                        m.pass_rate * 100.0,
                        m.accepted,
                        m.exceptional,
                        m.exec_failed + m.verify_failed,
                        m.timed_out,
                        m.redone,
                        m.avg_duration_ms,
                        m.learned_weight
                    );
                }
            }
        }
        _ => return Ok(false),
    }
    Ok(true)
}
