# Specialist agentlets

Agentlets are reusable specialist instructions and a strict result contract, bound to locally approved Cursor/Codex execution presets. Gemma chooses them through the existing workflow catalog. The host owns dispatch, scope checks, verification and review.

## Import and bind

Create an ordinary execution preset for your exact repository, CLI executable/model, verifier, paths and budgets first. Use `agent-preset-create --help` for required arguments. Existing credentials and models are installed separately. The following commands import data and create local bindings; they launch no model or coding task:

```powershell
clatterforge agentlet-import --definition examples/agentlets/implementer.json --by "Your name"
clatterforge agentlet-import --definition examples/agentlets/reviewer.json --by "Your name"
clatterforge agentlet-list
clatterforge agentlet-show reviewer --version 2
clatterforge agentlet-bind implementer --version 1 --preset project-cli --name project-implement --by "Your name"
clatterforge agentlet-bind reviewer --version 2 --preset project-cli --name project-review --by "Your name"
```

Definitions have a schema version, ID, positive integer version, purpose, instructions, capability and completion gate. Imported versions are immutable. Review imported instructions before binding. A definition cannot declare arbitrary executables, tools, endpoints, credentials or editable paths. Changing a specialist requires a new version and a new binding/profile; existing approvals keep their snapshot.

An editor inherits the base preset's exact editable paths. A reviewer gets no editable candidate paths. Cursor bindings require a newly inspected preset with a hash-pinned JS entry point; recreate an older preset explicitly if binding reports that its pin is missing. Executables, models, verifier commands and budgets stay under the local preset's control. Codex uses its pinned account home; Cursor uses the installed CLI's login profile for the current OS user.

## Required review coverage

Newly onboarded projects use reviewer definition version 2 with `require_review_evidence: true`. The host supplies an immutable diff artifact, its digest, the candidate tree and changed-file list. Small diffs are also included in the review prompt; larger diffs must be read from the artifact. Reviewers can use this evidence when their CLI sandbox cannot run Git.

A completed required review must report `review_evidence` with `coverage: complete`, the exact `candidate_tree` and `diff_digest`, every `reviewed_files` path, and an empty `limitations` list. Missing, limited, stale or mismatched coverage stops the attempt; a passing verifier cannot override it. Later candidate edits invalidate the review. Artifact or report tampering is rejected at result review.

Coverage is a specialist's structured attestation tied to host evidence, not proof of semantic correctness. Human review remains required. Existing definitions, approvals and legacy reports retain their original meaning; explicitly re-onboard an older project or bind the new reviewer version to opt into this contract.

## Route and inspect

```powershell
clatterforge workflow-profile-create --name project-specialists --config examples/orchestration-gemma26b-config.json --worker "project-implement=Implement the scoped objective" --worker "project-review=Review the completed candidate" --max-steps 4 --by "Your name"
clatterforge workflow-create --profile project-specialists --objective "Your objective" --acceptance "Independent acceptance checks"
```

Edit the illustrative model/hardware configuration for your host before creating the profile. Approve the returned assignment digest/revision, then run it through the ordinary [workflow controls](workflows.md). HTTP/MCP clients still use `workflow-v1` and the named profile; they cannot import definitions or change the approved catalog. In the UI, inspect **Approved specialists**, pending reviews, individual findings and actual worker routing receipts.

Gemma receives specialist purpose/capability/version, prior reports and verifier evidence. CLI workers receive a versioned objective/acceptance/repository input and operate in the existing shared worktree. Reports contain outcome (`completed`, `needs_work`, `blocked`), summary, findings (`severity`, `message`) and an optional next objective. Host verification and human acceptance remain separate.

A reviewer with `completion_gate: true` must report completed with passing independent verification on the final candidate tree. Further candidate changes require a fresh review. A passing verifier cannot override a blocked report. Malformed reports, false-success error findings, missing terminal CLI events, truncated output, scope violations and evidence tampering stop advancement with retained evidence; no worker is automatically replayed.

## Security and limits

Candidate permissions are checked by the host. They do not sandbox CLI filesystem/network access or installed hooks. Review-mode Codex requests its own read-only sandbox; Cursor uses its existing platform policy. Use trusted repositories/accounts and an OS boundary for hostile inputs. Worker and verifier environments allow only platform/tool discovery values; ambient API keys, proxy overrides and unrelated variables are excluded. CLI file-based authentication remains separately available; Codex uses its pinned home.

CLI global instructions, login configuration and hooks can still influence a worker. Inference routing is local, while frontier CLI prompts/results may reach their configured provider. Keep secrets out of definitions, objectives and reports, and keep runtime evidence private. These limits also apply when a specialist reports no findings; its report is not an independent security audit.

Definitions are at most 16 KiB, instructions 8 KiB, reports 16 KiB and retained CLI stdout 64 KiB. The complete Windows invocation is checked against its native command-line capacity, including UTF-16 units and quoting. Reduce instruction/prior-result size if a run reports this limit; no CLI is launched and input is never silently truncated. These transport limits are separate from the configured model context window. Source and diffs stay in Git; role reports must not duplicate entire files. Older routing results retain outcomes/references rather than accumulating every report body.

After aggregate human acceptance, use separately reviewed [source integration](integration.md). Agentlet findings and suggested next objectives never authorize source application, commit, push or deployment.

See the [contract and authority decision](../adr/0016-reusable-agentlets.md) for compatibility and evidence details.
