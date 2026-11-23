# Development and fixtures

Follow [CONTRIBUTING](../../CONTRIBUTING.md) for all workspace commands. Use Rust 1.90.0 native Windows tooling and Python 3.11+. SQLite is bundled; Git must be on PATH. Test fixtures use fake agents, disposable Git repositories and local HTTP servers.

```powershell
cargo +1.90.0 build --workspace --locked
cargo +1.90.0 run --locked -- --help
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/demo.ps1
```

The demo creates fresh state and an expendable source repository under `.conductor/demos/`. Its approval is explicitly simulated. It prints a patch, worker receipt and independent verifier receipt, then stops awaiting human review. Existing repositories and provider accounts are not used.

Core supervisor lifecycle tests require Windows. Three opt-in Docker fixtures are ignored in the ordinary suite. Linux checks the edge/protocol crates; WASM and desktop have separate manifests. Provider requests and fresh-machine installer tests are separate release evidence.

`--state` defaults to `.conductor` under the current directory. Use an explicit state path for manual runs and keep it outside registered source repositories. Retain evidence when a run fails; never delete a lease, index lock or owner record to force a retry. Inspect the CLI subcommand help for revision/digest requirements.
