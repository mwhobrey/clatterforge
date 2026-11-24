# Contributing

Clatterforge is a supervised source alpha. Small changes with concrete failure/recovery evidence are easiest to review. Read [the architecture](docs/ARCHITECTURE.md) and [agent instructions](AGENTS.md) first.

Use a native Windows Rust 1.90.0 toolchain, Git, Visual Studio C++ build tools and Python 3.11+. From the repository root:

```powershell
cargo +1.90.0 fmt --check
cargo +1.90.0 clippy --workspace --all-targets --locked -- -D warnings
cargo +1.90.0 test --workspace --locked -- --test-threads=4
python scripts/test-public-release.py
```

Core/protocol/edge share the root workspace. `gui/` and `gui/src-tauri/` have separate manifests and lockfiles:

```powershell
rustup target add wasm32-unknown-unknown --toolchain 1.90.0
cargo +1.90.0 check --locked --manifest-path gui/Cargo.toml --target wasm32-unknown-unknown --features web
cargo +1.90.0 check --locked --manifest-path gui/src-tauri/Cargo.toml
```

Linux supports the edge and protocol crates, not the Windows supervisor. Keep test repositories/state isolated. Test meaningful failures and recovery without real providers; record real-provider trials separately because they consume credits and may access credentials.

Do not commit `.clatterforge`, logs, database files, tokens, machine paths, private origins or local profiles. Use reserved illustrative domains in examples. Describe the problem, resulting behavior and verification in a PR; do not imply that fixture tests prove human acceptance or a production deployment.

Contributions are provided under the project's [Apache-2.0 license](LICENSE). Preserve third-party notices and review dependency/asset provenance when adding or updating bundled material. See [release gates](docs/RELEASE.md).
