# Agent progress and time limits

The `agents` engine (Cursor/Codex adapters) reports observed process output and approved-file changes through task detail, MCP evidence summaries and the GUI. The GUI refreshes connected tasks automatically, including runs started by Grok Bot or another client. Open the task to see execution versus independent verification, elapsed time, output byte counts, changed filenames, inactivity and limits. Live stdout/stderr previews appear beneath the attempt.

Progress means observable activity. Quiet model thinking may produce no signal, and noisy output may produce no useful result. Stale updates and unavailable file observation are labelled explicitly. Final process success, independent verification and human acceptance remain separate.

## Approve an activity-aware limit

Legacy assignments keep their existing hard timeout. For a new spec, explicitly set both:

```json
{
  "generation_timeout_ms": 600000,
  "generation_idle_timeout_ms": 180000,
  "verification_timeout_ms": 120000
}
```

This example permits at most ten minutes of generation and stops after three minutes without observable output or approved-file activity. It is an example, not an installed default or a measurement of provider response time. Activity renews only the inactivity window. The hard cap never extends, and verifier activity never renews its separate deadline.

For a new preset, use `agent-preset-create --generation-timeout-ms 600000 --generation-idle-timeout-ms 180000` together with its other required arguments. Hard caps over two minutes require explicit inactivity and cannot exceed thirty minutes. Both budgets are bound into the assignment digest and require approval. Changing a preset does not alter an already-approved task.

## Inspect and recover

`board-show <task> --json`, `GET /api/tasks/<task>` and MCP `get_task` expose optional `progress`; MCP evidence summaries include the same observations. Files under `<state>/agent-artifacts/<task>/` retain `progress.json` and the two `progress-generation.json` / `progress-verification.json` snapshots when those phases ran. An unfinished snapshot older than five seconds is stale, not evidence that the process stopped. Continue to use existing owner inspection/reconciliation for interrupted tasks; do not replay an uncertain run from a progress snapshot.

Inactivity and hard-cap stops remain timed-out execution receipts with distinct progress outcomes (`inactivity_limit` / `hard_limit`). Cancellation retains its existing revision check and owned process termination. Review the retained candidate and receipts before preparing a new assignment; progress cannot authorize a retry or integration.
