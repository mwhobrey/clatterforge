# Source alpha and release preparation

First-party packages use Apache-2.0. The public source candidate contains a curated portable documentation tree and a fresh initial Git history. Private development notes, benchmark reports, local task IDs, deployment policy, credentials and runtime state are excluded. Existing private history and worktree/checkpoint references remain preserved in the development repository.

## Prepare a public candidate

From the private development checkout, stage the reviewed source and public templates under `docs/public/tree/`. The exporter selects tracked source, lockfiles, tooling, examples and license material, then overlays those templates at their final public paths. Untracked files and other documentation are not exported. Supply deny terms in an ignored private JSON file with `literal_terms` and optional `private_email`.

```powershell
python scripts/test-public-release.py
python scripts/prepare-public-release.py --output PATH_TO_NEW_CANDIDATE --privacy-policy PATH_TO_PRIVATE_POLICY --initialize-git
```

The output must be a new directory. It has one independent initial commit on `main`, neutral contributor metadata and no remote. The exporter never rewrites the development repository. First-party attribution remains in NOTICE. Inspect the output and run a complete-history secret scan before publication. Do not mirror-push the private development checkout.

## Remaining publication checks

- Create the intended public repository only with owner authorization; enable private security reporting and branch/ruleset checks.
- Run the SHA-pinned read-only hosted CI for Windows, Linux Spark (`edge`) and protocol, WASM and security; local results do not establish hosted CI success.
- Exercise documented setup on a clean Windows machine and one real disposable project using explicitly configured local model/CLI accounts. Current fixtures verify host behavior without provider usage; they do not prove fresh real-provider acceptance.
- Agentlet acceptance has exercised one disposable project with real local Gemma routing to Cursor implementation and Codex review, followed by explicit source application/restore. This is a bounded provider check; fresh-machine installation and broader unfamiliar-project trials remain open.

## Binary distribution gates

The [dependency inventory](../THIRD_PARTY_NOTICES.md) retains each package's own terms and full collected license texts. Crates published without a license file get the canonical text of a license they declare, recorded with its source in the collector's `CLARIFICATIONS` table; packages used only at build time are listed separately because they are not redistributed. Regenerate with `python scripts/collect-third-party-notices.py`; it fails if a package linked into a shipped artifact has no license text. `--allow-missing` explicitly generates a preliminary inventory with such gaps listed.

Review all selected runtime features, including copyleft alternatives/obligations, and preserve required notices/source availability. Optional platform dependencies may have maintenance advisories outside the selected WASM/Windows graph. Model weights and provider tools are separate installations and license decisions.

The Windows desktop app ships as a per-user NSIS installer with signed self-update (see the [packaging runbook](runbook/packaging.md)). It uses the production web build and a content security policy that allows scripts only from the app itself (plus `unsafe-eval`, which the Dioxus web renderer needs to generate its DOM bindings), connections only to Tauri IPC and the local backend, and no objects, frames or form posts. Before distributing it: add code signing, publish the first signed release and verify a fresh machine. The forge supervisor also runs on Linux and macOS (the full test suite passes on both). Linux builds as an AppImage (self-updating) and a `.deb`; macOS builds as a Developer ID signed, notarized `.app` and `.dmg` with a self-updating `.app.tar.gz` (see the packaging runbook). Windows and Linux binaries are not code-signed yet. General onboarding, automatic cleanup and uncertain-owner recovery remain alpha limits.
