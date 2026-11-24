# Clatterforge

> **Renamed from Dev Conductor.** The binary is `clatterforge` (setup scripts also install the old `dev-conductor` name as an alias). State defaults to `.clatterforge`; an existing `.conductor` directory keeps being used in place when no `.clatterforge` exists. Sessions paired under the old cookie names stay signed in.

A local-first development conductor: give it an objective, approve its scope, and review the resulting change and verification evidence.

Clatterforge is a supervised Windows source alpha. A local model chooses the next approved coding worker; local Cursor and Codex CLI adapters do the coding. The host validates routing decisions, owns process supervision and retains durable task state. Workflow stages share one Git worktree with checkpoints and pass objectives, prior results and references between stages. Git supplies source and diff evidence on demand.

The desktop and authenticated HTTP/MCP interfaces expose the same task lifecycle. Finishing a workflow produces a candidate for human review. Applying an accepted candidate to its source checkout is a separate, explicit integration action.

[Specialist agentlets](docs/runbook/agentlets.md) add reusable versioned instructions and typed reports to approved CLI presets. Conductor enforces candidate edit/review scope and required reviews of the current Git checkpoint; Gemma routes among the approved specialists.

## Try it

Prerequisites: Windows, Git, native Rust 1.90.0 with Visual Studio C++ build tools, and Python 3.11 or later. SQLite is bundled. Tests and the fixture demo need no model download, provider account or existing private service.

```powershell
cargo +1.90.0 fetch --locked
cargo +1.90.0 test --workspace --locked -- --test-threads=4
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/demo.ps1
```

The demo creates an expendable repository under `.clatterforge/demos/`, uses simulated approval and leaves a patch awaiting human review. It does not represent real-provider acceptance.

## Documentation

- [First-agent guide](docs/FIRST_AGENT.md) and [architecture](docs/ARCHITECTURE.md)
- [Runbook index](docs/runbook/00_INDEX.md): development, workflows, review, integration and host operations
- [Remote access](docs/REMOTE_ACCESS.md) and [local inference](docs/INFERENCE.md)
- [Contribution guide](CONTRIBUTING.md), [security boundaries](SECURITY.md) and [release gates](docs/RELEASE.md)

The Windows supervisor, Linux edge/protocol and WASM web client have different support boundaries. Generic onboarding, unattended scheduling, automatic recovery and portable installers are still work in progress. A worktree isolates Git changes; it does not sandbox agent processes or credentials.

## License

First-party code is licensed under [Apache-2.0](LICENSE), with attribution in [NOTICE](NOTICE). Dependencies retain their own terms; [third-party notices](THIRD_PARTY_NOTICES.md) list collected texts and unresolved binary redistribution gaps. Model weights and external CLI tools are separately installed and licensed.
