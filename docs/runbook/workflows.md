# Local-model workflows

Use a workflow when the user supplies an objective and Conductor should choose the next approved CLI worker from evidence. Use a direct agent task for an already-selected single worker.

New assignments use a shared Git worktree; legacy snapshot assignments retain their approved interpretation.

## Configure the envelope

Create normal agent presets first. Each worker has a purpose and an exact editable file scope. All workers in one workflow profile must share the same checkout/base, protected sources and acceptance verifier. They can use different supported CLI adapters/models (currently Cursor and Codex). The profile pins the verifier executable and one local inference model artifact; implicit fallback is disabled.

```powershell
# Worker presets are created with the existing agent-preset-create command.
dev-conductor workflow-profile-create --name project-workflow `
  --config examples/orchestration-gemma26b-config.json `
  --worker "project-cursor=Implement the batching component" `
  --worker "project-codex=Implement ordered deduplication" `
  --max-steps 4 --decision-timeout-ms 120000 --by "Your name"

dev-conductor workflow-profile-list
dev-conductor workflow-create --profile project-workflow `
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

After acceptance, [source integration](integration.md) is a separate operator action. Prepare and inspect its destination/file plan, then explicitly apply the accepted candidate as unstaged source changes. It retains original bytes for rollback and recovery. Source integration is not part of the planner's tool authority or the workflow's submit/resume grant.

No new dependency is required. Build with `cargo build --locked`; tests use disposable Git repositories, a fixture local-model endpoint and fake CLI processes. Real model/CLI acceptance must be recorded separately.
