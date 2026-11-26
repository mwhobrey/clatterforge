# Architecture

The main forge is a Rust host that runs on Windows, macOS and Linux. It owns durable SQLite state, objective intake, approvals, routing validation, resource admission, process supervision and evidence. The core is the same on every platform; process supervision, the optional verifier sandbox and startup integration use platform mechanisms (see [platform specifics](#platform-specifics)). The desktop/web interface and authenticated API/MCP expose that lifecycle. Sparks, separate lightweight Linux adapters (`clatterforge-edge`), use shared protocol types to forward narrowly allowed requests to the main forge.

Sparks are the lightweight remote pieces of Clatterforge: the `clatterforge-edge` adapter and the enrolled connections (bots, remote machines) that forward narrowly allowed calls to the main forge. They own no task state and cannot change forge settings. Code, binaries, CLI commands and API paths keep the `edge` name (`clatterforge-edge`, `edge-gateway`, `enrollment-*`, `/api/edge/...`) so existing installations keep working.

## Objective workflow

1. Resolve the repository by credential-free identity, exact checkout and committed base. Prepare an immutable assignment from the objective, acceptance checks and approved worker catalog.
2. Require assignment approval or a matching bounded grant. Preparation alone launches no model or CLI.
3. Ask the pinned local inference model for a structured decision. The host validates worker identity, scope and budgets before dispatch.
4. Run a pinned local frontier CLI and independent verifier under owned process supervision. Capture bounded logs, progress and receipts.
5. Continue stages in one workflow worktree. Git tree checkpoints retain each stage; next steps receive objectives, prior results and references. Bounded status/diff/history pages can be requested instead of copying entire source files into planner context.
6. Finish with aggregate evidence awaiting human review. Only an accepted result may leave the forge, through one of the two separate operator actions described under [boundaries](#boundaries).

Gemma is an example local planner, not a hardcoded requirement. A profile pins model identity, endpoint and configured context limits. The HTTP request bound and the model context window are separate limits; an advertised window is not a hardware benchmark. Direct local-model coding and text/tool inference are separate paths with their own lifecycle.

## Platform specifics

| Concern | Windows | Linux | macOS |
|---|---|---|---|
| Owned process trees | Job Object per tree with kill-on-close, so the tree dies with the supervisor | Own process group; a guard process kills the group if the supervisor dies without disarming it | Same as Linux |
| Process identity after restart | PID and creation time | PID and start time from `/proc`; zombies count as exited | PID and start time from `proc_pidinfo`; zombies count as exited |
| Optional verifier sandbox | Docker (digest-pinned image) or a Windows AppContainer | Docker | Docker |
| Always-on host startup | Startup-folder shortcut (`manage-local-host.ps1`) | systemd user unit (`manage-local-host.sh`) | LaunchAgent (`manage-local-host.sh`); generated and linted, `launchctl bootstrap` not yet exercised |
| Sparks (`clatterforge-edge`) | Builds; not exercised as a Spark | Supported and CI-built | Builds; not exercised as a Spark |
| Hosted CI | Full test suite, managed-host test | Clippy, builds, library tests | None; the full suite is run locally before release |

A restarted supervisor adopts or reconciles only processes whose PID and start time match what it recorded, so a recycled PID is not mistaken for an owned process. See the [managed host runbook](runbook/managed-host.md) and the [sandbox runbook](runbook/sandbox.md).

## Boundaries

Versioned [agentlets](runbook/agentlets.md) bind specialist instructions and strict result contracts to the existing CLI workers. Their definitions and capabilities enter the assignment digest. Required reviewer reports must match the final candidate tree; later candidate changes require another review. Reports are untrusted specialist evidence and cannot authorize additional tools or source integration.

Repository leases coordinate cooperating operations. Worktrees isolate Git state, not OS capabilities. Job Objects (Windows) and process groups (Linux, macOS) bound owned process lifetime, not files, credentials or networking. Unknown owners/results remain uncertain after restart; no model or worker is automatically replayed.

An accepted result leaves the forge only through one of two separate operator actions. Neither runs automatically, and no grant, schedule, model decision or MCP tool can invoke either:

- **Source integration** (desktop, HTTP or CLI) validates the accepted evidence and destination again, journals original bytes and applies a small text-only change as an **unstaged diff**, either in the source checkout or in a new integration worktree on its own branch. It can restore originals while exact preconditions still hold. It does not commit, merge, push or deploy.
- **Delivery** (CLI only) re-derives the accepted candidate, builds one commit on a new branch with Git plumbing without touching any working tree, and then, only as further explicit commands, pushes that branch with your Git credentials and opens a pull request with the GitHub CLI. It never merges or deploys.

See [integration](runbook/integration.md), [workflows](runbook/workflows.md) and [security](../SECURITY.md).
