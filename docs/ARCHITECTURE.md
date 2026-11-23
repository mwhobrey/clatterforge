# Architecture

The Windows Rust host owns durable SQLite state, objective intake, approvals, routing validation, resource admission, process supervision and evidence. The desktop/web interface and authenticated API/MCP expose that lifecycle. A separate Linux edge uses shared protocol types to forward narrowly allowed requests.

## Objective workflow

1. Resolve the repository by credential-free identity, exact checkout and committed base. Prepare an immutable assignment from the objective, acceptance checks and approved worker catalog.
2. Require assignment approval or a matching bounded grant. Preparation alone launches no model or CLI.
3. Ask the pinned local inference model for a structured decision. The host validates worker identity, scope and budgets before dispatch.
4. Run a pinned local frontier CLI and independent verifier under owned process supervision. Capture bounded logs, progress and receipts.
5. Continue stages in one workflow worktree. Git tree checkpoints retain each stage; next steps receive objectives, prior results and references. Bounded status/diff/history pages can be requested instead of copying entire source files into planner context.
6. Finish with aggregate evidence awaiting human review. Only an accepted result may enter separately prepared source integration.

Gemma is an example local planner, not a hardcoded requirement. A profile pins model identity, endpoint and configured context limits. The HTTP request bound and the model context window are separate limits; an advertised window is not a hardware benchmark. Direct local-model coding and text/tool inference are separate paths with their own lifecycle.

## Boundaries

Versioned [agentlets](runbook/agentlets.md) bind specialist instructions and strict result contracts to the existing CLI workers. Their definitions and capabilities enter the assignment digest. Required reviewer reports must match the final candidate tree; later candidate changes require another review. Reports are untrusted specialist evidence and cannot authorize additional tools or source integration.

Repository leases coordinate cooperating operations. Worktrees isolate Git state, not OS capabilities. Windows process jobs bound owned process lifetime, not files, credentials or networking. Unknown owners/results remain uncertain after restart; no model or worker is automatically replayed.

Source integration validates the accepted evidence and destination again, journals original bytes and applies a small text-only change as an unstaged diff. It can restore originals while exact preconditions still hold. It does not merge, commit, push or deploy. See [integration](runbook/integration.md), [workflows](runbook/workflows.md) and [security](../SECURITY.md).
