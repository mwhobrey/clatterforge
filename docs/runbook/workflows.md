# Local-model workflows

Use a workflow when the user supplies an objective and Conductor should choose the next approved CLI worker from evidence. Use a direct agent task for an already-selected single worker.

New assignments use a shared Git worktree; legacy snapshot assignments retain their approved interpretation.

## Configure the envelope

Create normal agent presets first. Each worker has a purpose and an exact editable file scope. All workers in one workflow profile must share the same checkout/base, protected sources and acceptance verifier. They can use different supported CLI adapters/models (currently Cursor and Codex). The profile pins the verifier executable and one local inference model artifact; implicit fallback is disabled.

```powershell
# Worker presets are created with the existing agent-preset-create command.
clatterforge workflow-profile-create --name project-workflow `
  --config examples/orchestration-gemma26b-config.json `
  --worker "project-cursor=Implement the batching component" `
  --worker "project-codex=Implement ordered deduplication" `
  --max-steps 4 --decision-timeout-ms 120000 --by "Your name"

clatterforge workflow-profile-list
clatterforge workflow-create --profile project-workflow `
  --objective "Harden the collection helpers" `
  --acceptance "The protected acceptance suite passes"
```

The example configuration illustrates an Ollama `gemma4:26b` digest recorded in that file on loopback 11434. Inspect the actual runtime/model before provisioning a profile. Profiles snapshot configuration, repository identity and worker presets. Each new objective captures the current committed base; an existing approved workflow cannot drift to a newer commit. Create a new profile after a repository-identity, worker, verifier executable or model change. Existing local inference admission/reservation rules apply, independently of CLI execution.

Prepare from the desktop's **New objective** disclosure or `POST /api/workflows` with `{profile, objective, acceptance}`. `GET /api/workflow-profiles` supplies authenticated profile summaries. Preparation starts no model or CLI. Inspect the workflow's assignment digest and budgets, then use ordinary task approve/run controls. A running workflow decides and dispatches subsequent steps without the caller choosing each worker.

MCP clients submit backend `workflow-v1`, `workflow_profile`, `objective`, `acceptance`, their principal and a stable workflow/idempotency key. Older connectors can put that same profile name in `agent_preset` as a compatibility alias. This names the whole approved envelope, not the individual worker. Use the existing `conductor_get_task`, evidence/artifact, approval-requirements, resume and cancel tools. Submission can auto-approve/resume under a repository-scoped `workflow-v1` grant. One admitted workflow may execute up to its explicitly approved profile limit. Do not call resume again when asynchronous resume is already queued. A replay returns the existing task and never reruns it.

## Inspect, stop and review

`workflow-show <id>` returns the durable workflow record. `board-show <id> --json`, HTTP task detail and MCP reads include optional `workflow` decisions, job references, rationales, child IDs and event history. Planner inference state lives under `<state>/workflow-inference/<workflow-id>/`; existing inference inspection commands work against that directory. Child tasks retain normal artifacts, process receipts, routing events and progress. Parent diff/verifier artifacts refer to the latest candidate, including inherited edits.

The parent's `routing` contains every child's actual handler receipts, annotated with its task ID. Parent transitions, including `workflow_reviewed`, belong to `workflow.events` and appear in the desktop's **Workflow lifecycle events** disclosure. Workflow review calls `Workflows::review`; it does not pretend that each child received an `Agents::review`. Accepted/rejected detail projects the terminal stage even for older records that retained `review` in their raw phase. Exact-key submit replays omit artifact previews; retrieve bounded evidence explicitly when needed.

Each next worker continues in the same workflow worktree with the latest uncommitted changes and bounded prior-stage evidence. It can edit its own approved paths only; inherited files outside that scope are immutable. Git trees pinned at `refs/conductor/workflows/<id>/stage-<n>` retain stage contents without commits or index changes. `workflow-show`/task detail expose checkpoint tree/reference IDs. Read those trees with ordinary `git show <tree>:<path>` or compare them with `git diff <earlier-tree> <later-tree>`; no files need to be carried in routing context. These checkpoints do not authorize an automatic reset or retry. A full acceptance verifier can fail after an incomplete intermediate stage, informing the next model decision. Passing model prose never substitutes for that verifier.

The local planner can request `inspect` with `query` equal to `status`, `diff` or `history`, a byte `offset`, and `limit` of 1..8192. Reads use fixed Git revisions, stream into a bounded artifact and return only the requested page on the next decision. `git_reads` retains artifact hashes/revision/page references; full requests and supplied pages remain in canonical inference receipts. At most four reads share the existing total decision budget. They cannot supply shell commands, change revisions, stage files or expand scope. Checkpoint/read-artifact tampering blocks review/integration.

Preparation records Git object references rather than loading whole source files, so the old 16,000-byte packing limit no longer applies to new assignments. Inference has a separate 4-MiB request transport bound plus configured model context admission. The example profile's 32,768-token `num_ctx` is not changed by this refactor; larger advertised model capacity is not a hardware benchmark. The admission estimate remains conservative serialized bytes plus per-message overhead. Source integration retains its separate text/64-KiB-per-file limit.

Use ordinary task cancellation or global stop. An interrupted owner can be reconciled to `unknown`; inspect its child tasks and inference/repository reservations separately. There is no automatic replay or guessed recovery. Each malformed decision can receive one recorded schema-correction turn within the total limit of twice (max worker steps + one finish); it launches no worker. Repeated malformed output, budget exhaustion, unknown workers, unavailable models and unsafe worker outcomes stop with retained evidence.

A valid model finish produces `awaiting_review`. Inspect the aggregate decisions, final diff and independent verifier before accepting/rejecting through normal task review. Acceptance applies to the workflow candidate; intermediate child records remain honest stage evidence. No merge, source-checkout edit, commit or push happens as a workflow completion side effect.

Rejecting a workflow result can carry line-anchored comments (`board-review --comment PATH:LINE[-END]:TEXT`, the HTTP review body's `comments`, or `conductor_reject`'s `comments`). The host checks each anchor against the candidate's checkpoint tree (the path must be a file the candidate changed, the lines must exist), records the anchored lines as an excerpt with the review, and refuses the whole review otherwise. At most 50 comments of up to 2,000 characters each. A later `board-retry` of that rejected workflow hands the comments to the planner as `review_comments` and appends them, with their excerpts, to the next dispatched stage's objective beside any operator guidance. Comments are reviewer input, not evidence; the independent verifier still decides the next candidate. Other engines refuse comments.

After acceptance, [source integration](integration.md) is a separate operator action. Prepare and inspect its destination/file plan, then explicitly apply the accepted candidate as unstaged source changes. It retains original bytes for rollback and recovery. Source integration is not part of the planner's tool authority or the workflow's submit/resume grant.

No new dependency is required. Build with `cargo build --locked`; tests use disposable Git repositories, a fixture local-model endpoint and fake CLI processes. Real model/CLI acceptance must be recorded separately.

## Planning in a desktop chat app

Desktop chat apps can do the planning without spending metered CLI usage, but nothing drives them headlessly; this handoff keeps a person in the loop. `workflow-plan-export <workflow> [--out FILE]` (or `GET /api/tasks/{id}/context-pack`) prints a bounded Markdown brief, at most 32 KiB: objective, acceptance, approved files and verifier, excerpts of up to 12 scoped files at the base commit (protected verification files are not excerpted), and prior stage results and review comments. Paste it into the chat, then save the plan you get back and run `workflow-plan-import <workflow> --revision N --file plan.md --by NAME` (or `POST /api/tasks/{id}/plan` with `revision`, `text`, optional `source`).

Import works only while the workflow awaits approval. The plan, at most 16 KiB, becomes part of the assignment as `operator_plan` with its SHA-256 digest, importer, time and source label (default `desktop_chat`), so the approval that follows binds to that exact plan and importing again replaces it. Once approved it cannot change. During the run the planner receives the plan and every dispatched stage objective carries it (bounded to 8 KiB), but implementation still runs through supervised workers limited to the approved files and the independent verifier still decides. Task detail shows the plan as the operator-supplied `plan` stage with its digest. Automating a consumer chat app is out of scope: it would breach its terms and bypass host-captured evidence.

## Best-of-N races

A race runs 2-4 attempts at the same scoped stage at once and lets you pick one. `race-create --preset A --preset B [...] --objective TEXT --acceptance TEXT --by NAME` creates one ordinary agent task per admin-approved preset. The presets must share the repository, editable files, protected verification files and verifier, so the candidates are comparable; different CLIs or models are the point. `race-show <race>` gives the `approval_digest`; `race-approve <race> --digest D --by NAME` approves every attempt at once. `race-run <race> --by NAME` holds the repository lease once for the whole race and runs the attempts at the same time, each in its own worktree and each checked by the independent verifier. The repository still admits one unit of work at a time, and a global stop refuses the run.

`race-show` then lists the candidates side by side: preset, CLI and model, status, verifier outcome, changed files with added and removed lines, error, and the evidence digest of each one that reached review. `race-pick <race> --task <winner> --digest <its evidence digest> --by NAME` accepts the winner through ordinary review and rejects the other reviewable attempts; attempts that failed stay as they are. Every attempt and review lands in the routing scoreboard as usual. The accepted winner integrates (`integration-prepare`, `integration-apply`) or delivers (`deliver-commit`) like any accepted agent task. Races run outside workflows; a workflow stage still runs one worker at a time.
