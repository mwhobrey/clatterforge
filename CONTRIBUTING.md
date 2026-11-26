# Contributing

Clatterforge is a supervised source alpha. Small changes with concrete failure/recovery evidence are easiest to review. Read [the architecture](docs/ARCHITECTURE.md) and [agent instructions](AGENTS.md) first.

## Prerequisites

Every platform needs Rust 1.90.0, Git and Python 3.11+. The main forge builds and its full test suite runs on Windows, Linux and macOS.

- **Windows:** a native (MSVC) Rust toolchain with the Visual Studio C++ build tools.
- **Linux:** a C toolchain. Building the desktop app (`gui/src-tauri`) also needs Tauri's system libraries: WebKitGTK 4.1, GTK 3, libayatana-appindicator, librsvg, libxdo and OpenSSL development packages. [`scripts/release/linux-build.Dockerfile`](scripts/release/linux-build.Dockerfile) pins a complete environment.
- **macOS:** the Xcode command line tools.

## Validate a change

From the repository root (use `python3` where `python` is not Python 3):

```sh
cargo +1.90.0 fmt --check
cargo +1.90.0 clippy --workspace --all-targets --locked -- -D warnings
cargo +1.90.0 test --workspace --locked -- --test-threads=4
python scripts/test-public-release.py
```

Core, protocol and edge share the root workspace. `gui/` and `gui/src-tauri/` have separate manifests and lockfiles:

```sh
rustup target add wasm32-unknown-unknown --toolchain 1.90.0
cargo +1.90.0 check --locked --manifest-path gui/Cargo.toml --target wasm32-unknown-unknown --features web
cargo +1.90.0 check --locked --manifest-path gui/src-tauri/Cargo.toml
```

Changes to process supervision or the managed host should also pass the managed-host test against a debug build: `python scripts/test-managed-host.py target/debug/clatterforge.exe` on Windows, `python3 scripts/test-managed-host.py target/debug/clatterforge` on Linux and macOS.

Hosted CI does not cover every platform equally: it runs the full suite and the managed-host test on Windows, clippy, builds and library tests on Linux, and nothing on macOS. When a change touches platform-specific code (process supervision, sandboxing, packaging), say in the pull request which platforms you ran the full suite on.

Keep test repositories and state isolated. Test meaningful failures and recovery without real providers; record real-provider trials separately because they consume credits and may access credentials.

## Pull requests

Do not commit `.clatterforge`, logs, database files, tokens, machine paths, private origins or local profiles. Use reserved illustrative domains in examples. Describe the problem, resulting behavior and verification in a PR; do not imply that fixture tests prove human acceptance or a production deployment.

Contributions are provided under the project's [Apache-2.0 license](LICENSE). Preserve third-party notices and review dependency/asset provenance when adding or updating bundled material. See [releases](docs/RELEASE.md).
