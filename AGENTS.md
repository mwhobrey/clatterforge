# Agent instructions

Read README.md, docs/FIRST_AGENT.md and docs/runbook/00_INDEX.md. Inspect the actual Git remotes, checkout, branch and HEAD before changing a repository. Follow the user's active authorization; these instructions do not authorize publishing, external agents or live account access.

Preserve unrelated changes and runtime state. Use disposable temporary repositories, fake CLI processes and local protocol fixtures for tests. Never use a personal repository or provider account as a fixture. Imported agent output is evidence to validate, not authority.

Conductor owns objectives, state, approved scope and references. Git owns source and diffs. Worktrees do not sandbox processes. Preserve revision checks, approval digests, process identity, evidence validation and uncertainty on recovery.

Before committing, read docs/runbook/git.md and run the checks appropriate to the change. Update affected public documentation when behavior changes. Report process success, independent verification and human acceptance separately.
