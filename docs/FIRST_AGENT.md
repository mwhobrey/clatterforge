# First-agent guide

Dev Conductor is a standalone product. It must build and run without the maintainer's private services, task stores or installed model profiles. Start with [architecture](ARCHITECTURE.md), [the runbook](runbook/00_INDEX.md) and [release limits](RELEASE.md).

Use actual Git remotes/HEAD and explicit checkout paths. Conductor coordinates approved tasks; it does not grant a model unrestricted tools. The current workflow uses one shared worktree per objective, immutable Git tree checkpoints, bounded prior results and on-demand Git evidence. The local planner chooses an approved worker or finishes for review. CLI execution and independent verification remain host-controlled.

Run the account-free fixture demo before configuring a real provider. Preserve existing runtime state; never use private repositories as test fixtures. Scope changes need corresponding architecture/runbook updates and failure/recovery checks. Keep process completion, verifier success and human acceptance distinct.
