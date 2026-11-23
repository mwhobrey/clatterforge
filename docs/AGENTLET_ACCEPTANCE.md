# Agentlet acceptance

October 31, 2026. Source validation and isolated real-provider acceptance; no main-host deployment or publication is claimed.

## Implemented scope

Immutable versioned specialist definitions bind to local CLI presets. The existing workflow snapshots them into approvals, exposes their purposes/capabilities to the local planner, validates typed reports, and requires configured reviewers to complete on the final Git candidate. Candidate changes invalidate earlier required reviews. CLI imports/bindings are local administration; HTTP/MCP can only name approved presets/profiles. The desktop projection shows approved specialists, pending reviews, reports and actual routing evidence.

## Verification

- Final Windows native workspace: **343 passed**, zero failures, three existing Docker tests ignored.
- Eight agentlet unit checks cover immutable versions, authority/unknown-field rejection, malformed/duplicate/contradictory reports, provider completion, artifact truncation/tampering, environment filtering, Windows argument sizing and Cursor aggregate result handling.
- Nine workflow checks cover CLI import/binding and MCP intake through routing/review/integration/restore/replay, skipped/stale required review, reviewer file edits, blocked findings despite a passing verifier, malformed reports, stored report tampering, ambient environment propagation and changed Cursor entry points.
- Formatting, workspace/all-target Clippy and WASM GUI check/Clippy passed. Public-export boundary tests passed. No new dependency or lockfile change was needed.

## Real-provider run

A new disposable Git project contained one implementation stub and 13 protected tests for integer range compression, including generators, duplicates, negatives, large values, input preservation and invalid types. The target checkout, identity and base were resolved explicitly. Its repository/state were separate from existing product projects and the main Conductor task database.

Authenticated HTTP prepared and approved the objective under an explicitly labeled test operator. The native host ran the workflow while its task API was polled for phase, active worker, elapsed time and review gates. Real local `gemma4:26b` made three decisions: dispatch the implementation specialist, dispatch the required review specialist, then finish for human review. No worker order was submitted as a fixed execution script.

Cursor (`gpt-5.3-codex`) edited only the approved file, completing generation in 121,920 ms. It reported that its own test command was refused by its CLI environment. The host's separate pinned verifier nevertheless ran and passed all 13 tests. Codex (`gpt-6.1-sol`) then reviewed that same candidate in requested read-only mode, ran the 13 tests itself and returned a completed report with no findings in 19,873 ms. The host verifier passed again. Both stages reference the same Git tree, and reviewer scope is empty. A completed specialist report was not treated as independent verification or human acceptance.

The parent stopped at `awaiting_review`. A separate operator inspection reviewed the actual source, process/verifier receipts, reports and Git checkpoint and reran the protected contract. The test operator then accepted the aggregate candidate, prepared/applied source integration, reran all 13 tests in the source checkout, and restored the original bytes. HEAD and index remained identical, source status returned clean, and an accepted-task replay was refused with no extra child task. This records Codex acting as an isolated acceptance operator, not the end user's human acceptance.

## Bugs and security findings addressed

The first real attempt stopped at `needs_work`: Cursor concatenated progress commentary and final JSON in its terminal `result`. The strict result decoder refused it before verification. That failed attempt was preserved, not resumed or relabeled. The adapter now accepts exactly one unambiguous trailing JSON report from that terminal field and rejects multiple objects, trailing prose, malformed fields and provider errors. A fresh project/state rerun passed; the failed attempt was not replayed.

Cursor presets previously pinned the native launcher but only checked that their JS entry point existed. New presets now hash the entry point too; agentlets reject older unpinned presets and refuse drift before planner/worker dispatch. Old assignment serialization and approvals remain unchanged. New agentlet worker/verifier environments are allowlisted, dropping ambient keys and execution overrides; Codex uses its pinned home and reviewer read-only policy.

A final routing audit corrected the report-validation receipt label to the function that actually runs, `agentlet::report_from_receipt`. The exported-source E2E checks that label. Prior live receipts retain their original historical label; no evidence was rewritten. This follow-up changes observability, not report validation or execution authority.

## Limits still open

The CLI's installed global instructions/hooks can influence behavior outside the portable role definition. During this run the reviewer attempted an inherited local persona lookup; its network policy refused that connection, and review/tests still completed. The product did not require that private service, but this demonstrates why an approved candidate scope is not a complete process sandbox. Cursor on Windows used its existing disabled-OS-sandbox/CLI-allowlist mode. Entry-point hashes do not attest to all transitive CLI files, and file-based login/profile data remains available to the provider. Use trusted accounts/repositories or an OS/VM boundary for hostile work.

This is one bounded real-provider task, not evidence of general planning reliability, a clean-machine installation, Grok desktop acceptance, broad adversarial isolation or a full independent security audit. Public hosting/CI/reporting, packaging/CSP, dependency notice gaps, upgrade/restore acceptance and broader real-project trials remain separate OSS release gates.

Private model/process receipts, the failed/successful disposable projects, source-restore journal and test logs remain under `.conductor/agentlets-acceptance-20261031/`; they are not public export inputs. See [ADR 0016](adr/0016-reusable-agentlets.md) and [agentlet operations](runbook/agentlets.md).
