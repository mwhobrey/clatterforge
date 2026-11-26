# Security

## Reporting a vulnerability

Report vulnerabilities privately through GitHub: open this repository's **Security** tab and choose **Report a vulnerability**. Private vulnerability reporting is enabled for this repository. Please do not open a public issue for a vulnerability, and do not include secrets or private logs in any report. No response time is promised for the alpha.

## Boundaries

Clatterforge runs local developer tools with the privileges of its user. Git worktrees, path checks, process supervision (Job Objects on Windows, process groups on Linux and macOS) and repository leases do not create a filesystem, credential or network sandbox. The coding CLIs run with your account, profile and network. Use trusted repositories and tools and an appropriate OS account or machine boundary. The optional verifier sandbox (Docker, or a Windows AppContainer) limits verifiers only; see the [sandbox runbook](docs/runbook/sandbox.md).

Assignments bind exact repository identity, base, allowed paths, executable identity and budgets. Model decisions cannot expand those approvals. An accepted result reaches your repository only through two separate operator actions, neither of which a grant, schedule, model or MCP tool can invoke: source integration, which leaves an unstaged diff for you to review and commit, and delivery, which creates a commit on a new branch and pushes it or opens a pull request only when you run those commands. Concurrent hostile edits are outside these cooperative protections. See [integration](docs/runbook/integration.md).

The API uses local bearer tokens and paired sessions with CSRF checks. Keep listeners on loopback behind a trusted authenticated transport. Exact browser origins and external Hosts are operator configuration; see [remote access](docs/REMOTE_ACCESS.md). Runtime state can contain source excerpts, prompts, results, credentials and original integration bytes. Keep `.clatterforge` private and back it up accordingly.

## Release integrity

Self-updates are verified against the update public key committed in `gui/src-tauri/tauri.conf.json` before they are installed. macOS builds are also code-signed with a Developer ID and notarized. Windows and Linux binaries are not code-signed yet. See [releases](docs/RELEASE.md) for what each term covers.
