# Security

Conductor runs local developer tools with the privileges of its user. Git worktrees, path checks, Windows Job Objects and repository leases do not create a filesystem, credential or network sandbox. Use trusted repositories/tools and an appropriate OS account or machine boundary.

Assignments bind exact repository identity, base, allowed paths, executable identity and budgets. Model decisions cannot expand those approvals. Source integration is a separate operator action and leaves an unstaged diff. Concurrent hostile edits are outside these cooperative protections.

The API uses local bearer tokens and paired sessions with CSRF checks. Keep listeners on loopback behind a trusted authenticated transport. Exact browser origins and external Hosts are operator configuration; see [remote access](docs/REMOTE_ACCESS.md). Runtime state can contain source excerpts, prompts, results, credentials and original integration bytes. Keep `.conductor` private and back it up accordingly.

Report vulnerabilities using the repository's private security reporting feature when enabled. If no private channel is available, open an issue requesting a private contact without exploit details, secrets or private logs. This source candidate has no configured public repository yet; enabling private reporting is a [publication gate](docs/RELEASE.md). No response SLA is promised for the alpha.
