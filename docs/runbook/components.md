# Components

Where behavior lives in `src/`, and what each module persists under the state root
(`--state`, `.clatterforge` by default). Read this before changing a module; the
[architecture](../ARCHITECTURE.md) page covers the overall flow.

## Engines and orchestration

| Module | Owns |
|---|---|
| `store.rs`, `runner.rs`, `process.rs` | Fake/Codex task records and revisions, execution and review, owned Windows Job and POSIX process supervision |
| `orchestration.rs` | Supervised local-model coding loop with host-owned edits and independent verification |
| `agents.rs` | Supervised CLI-agent engine (Cursor, Codex, AGY, local worker): approval-bound dispatch, scope checks, verifier |
| `agent_preset.rs`, `agentlet.rs` | Admin-approved dispatch presets with model routing policy; versioned specialist definitions bound to presets |
| `agent_progress.rs` | Host-observed progress, an approved inactivity window and an independent hard cap |
| `workflow.rs`, `workflow_workspace.rs` | Multi-stage workflows (local model or built-in router), review gates, retry/resume, failover; shared worktrees and Git tree checkpoints |
| `integration.rs` | Applying an accepted candidate to its exact source checkout, with a byte journal and restore |
| `service.rs` | `TaskService`: one id-resolution and engine-dispatch path shared by CLI, HTTP and MCP; dependency gate and repository leases |

Cancellation is revision-checked, with one tolerance: a stale revision is accepted when the only events since it are the task's own progress updates. An approval, retry/restart/resume or assignment revision since then still fails with a revision conflict (`cancel_revision` in `lib.rs`).

## Onboarding, scope and routing

| Module | Owns | Persists |
|---|---|---|
| `onboard.rs` | CLI, model and verifier discovery (injectable `DiscoveryEnv` for hermetic tests); profile and preset creation with rollback | Profiles and presets |
| `repo_index.rs` | Per-profile file/symbol index of the tracked files at HEAD (never the working tree); ranks files for an objective to suggest scope (2,500 files, 128 KiB per file at most) | `repo_file_index`, `repo_index_meta` |
| `routing_metrics.rs` | One row per agent attempt; outcome-weighted scoreboard used by workflow routing. Usage is parsed from the end of stdout; `usage_reported` keeps "no receipt" distinct from zero tokens | `cli_model_outcomes` |
| `provider_status.rs` | Bounded provider availability and sign-in probes; token telemetry summary for `/api/providers` | None |

## HTTP, sessions and logs

| Module | Owns | Persists |
|---|---|---|
| `serve.rs` | Authenticated HTTP API: bearer token (desktop) or paired session cookie plus CSRF header (browser/phone) | `serve-token` (persisted; `--rotate-token` replaces it) |
| `sessions.rs` | Paired-device sessions: hashed tokens, 30-day sliding renewal, 90-day absolute cap, revocation | `paired_sessions` |
| `http_access.rs` | Operator Host/Origin allowlist; refuses non-loopback binds without `--allow-insecure-lan` | Reads `http-access.json` |
| `ui_server.rs` | Static compiled-GUI server with the same Host/Origin checks | None |
| `logging.rs` | JSONL logs to stderr and a size-rotated file; stateless per-record appends, rotation under a lock file | `logs/clatterforge.jsonl` (+ `.1`-`.3`) |
| `mcp.rs`, `gateway.rs`, `enrollment.rs` | MCP stdio adapter, loopback edge gateway, enrolled edge clients | Enrollments, grants |
| `schedule.rs` | Durable scheduled submissions. A running `serve` fires due ones every 15 s through `conductor_submit_task` as the scheduling principal: grants are judged at fire time, nothing fires during a global stop, missed occurrences fire once, retries replay instead of duplicating | `scheduled_submissions`, `schedule_fires` |

All tables live in `<state>/state.sqlite` unless noted. Session verification and other SQLite work in HTTP handlers run on the blocking pool, not on async workers.
