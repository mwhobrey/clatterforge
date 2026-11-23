# Git workflow

Inspect actual remotes, branch, HEAD and worktrees before edits. Preserve unrelated changes. Use `codex/` for agent-created branches unless the user specifies another name. Review source, tests, manifests and affected docs before a commit; inspect new untracked files directly.

Run the [contribution checks](../../CONTRIBUTING.md) appropriate to the change and `git diff --check`. Use a focused Gitmoji commit subject without trailers. A requested commit does not authorize creating a remote, pushing, publishing or deploying.

Never publish private development history or runtime state as a side effect of preparing a source snapshot. The public initial tree is independent; the release exporter copies a curated set of tracked files and initializes fresh history with neutral contributor metadata. See [release preparation](../RELEASE.md).
