# ADR 0016: Reusable specialist agentlets

October 31, 2026. Source implementation; acceptance evidence is recorded separately.

Agentlets extend the existing agent presets and `workflow-v1` supervisor. They do not introduce another execution engine or grant arbitrary tools to the planner.

## Definition and local binding

A portable version-1 JSON definition contains an ID/version, purpose, instructions, candidate capability (`edit_approved_files` or `review`) and optional required-completion gate. Unknown fields, unsupported schemas, unsafe IDs, oversized instructions and editor completion gates are rejected. Definitions contain no executable, endpoint, account, path scope, shell command or arbitrary tool configuration. Instructions are admin-reviewed data for a pinned CLI, not executable plugins. Imported `(id, version)` records are immutable and retain an importer/time/digest.

`agentlet-bind` creates a new named preset from an unbound local execution preset. Editors retain that exact file scope; reviewers receive an empty editable scope. Neither can expand it. The definition is copied into the binding, workflow profile and child assignment, so changing/importing a registry version or deleting a template cannot alter approved work. The whole snapshot enters the existing assignment approval digest. Optional fields serialize away on legacy assignments; previous approval and evidence formats remain unchanged when no agentlet is present.

## Input, result and routing

The host supplies a versioned input with objective, acceptance, repository/base references, allowed paths and bounded prior-stage context. Source stays in the shared worktree/Git. The CLI must return one version-1 JSON report as its final assistant message: outcome (`completed`, `needs_work`, `blocked`), summary, bounded severity/message findings and optional next objective. Unknown/duplicate fields, oversized or contradictory results, truncated output, provider errors and missing semantic completion fail closed before verification/another dispatch. There is no automatic result-repair retry.

Reports are parsed from successful terminal Cursor stream-JSON or Codex JSONL messages, not log excerpts or agent-written verification claims. Their bytes remain in the normal process artifact; the parsed report is bound to child evidence and rechecked against that artifact for parent review/integration. A blocked report cannot become success merely because the independent verifier passed.

Live acceptance exposed Cursor concatenating progress commentary and the final message into its terminal `result` field. Its adapter accepts exactly one unambiguous JSON object at the end of that field. Multiple objects, trailing prose, malformed/unknown result fields and provider errors remain rejected. Codex uses its final completed agent-message text without this Cursor-specific normalization.

Gemma receives the approved specialist catalog, identity/version/digest/capability, structured results and pending completion gates. It chooses the next existing worker through the same typed dispatch action. A required reviewer must report completed with successful host verification against the current candidate's Git tree. Candidate changes invalidate an older review. Finish and human acceptance refuse missing/stale reviews. Reviews still stop for human acceptance; integration remains separately prepared and authorized.

## Process and compatibility boundaries

Agentlet workers and their verifier receive an explicit platform/tool environment allowlist, excluding ambient API keys, proxy overrides, Node injection options and unrelated variables. Codex receives its pinned account home, uses the existing isolated CLI flags, and requests `read-only` for review specialists. New Cursor policies hash the entry-point JS as well as the native launcher; agentlets reject legacy Cursor presets without the entry-point pin and refuse drift before model dispatch. Recreate those presets explicitly; do not rewrite old approvals.

These policies do not provide a machine sandbox. The host checks candidate changes and protected sources; ignored build outputs and files outside the managed Git candidate are not comprehensively isolated. The provider may read installed login/configuration, hooks and other user-accessible files. Entry-point hashes do not attest to all transitive CLI installation files. Use a trusted repository/account or an OS/VM boundary for hostile work. Legacy direct-agent tasks retain their existing environment behavior; the new allowlist applies to agentlets.

CLI invocation size is checked against Windows' native command-line limit using UTF-16 units and conservative argument quoting/escaping. Oversize input fails before launch without silent truncation. This is separate from model context admission and does not restore the removed workflow source-packing limit. Agentlet stdout must fit the existing 64-KiB receipt, and reports at most 16 KiB; chatty/truncated providers stop rather than guessing a result. Routing retains only outcomes/references for older reports and supplies the latest full structured result rather than accumulating all report bodies.

No new dependency, registry network fetch, public import endpoint, remote tool-authority expansion, automatic acceptance/integration, parallel agent spawning or deployment is introduced.
