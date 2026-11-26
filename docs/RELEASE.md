# Releases

First-party packages use Apache-2.0. This repository carries the reviewed source and a curated portable documentation tree; private development notes, benchmark reports, local task IDs, deployment policy, credentials and runtime state are not published.

## Status

As of November 25, 2026.

### Completed, with evidence

- **Public repository.** Published November 25, 2026, with private vulnerability reporting enabled and a ruleset that requires a pull request and the `windows-core`, `linux-edge`, `web` and `security` CI jobs before anything reaches `main`.
- **Hosted CI.** The read-only, SHA-pinned CI workflow runs on every pull request and on `main`, and passes there. Coverage differs by platform; see [contributing](../CONTRIBUTING.md).
- **First release.** v0.1.0 was published November 25, 2026 by the tag-triggered release workflow: installers for Windows x64, macOS (Apple silicon and Intel) and Linux x86_64, signed update artifacts for every self-updating build, and one merged `latest.json`.
- **Pre-publication install checks of the CI-built v0.1.0 artifacts.** The `.deb` and the AppImage installed and ran in a clean Debian 12 container: the app started its backend with the per-user state folder, served the UI, and the API answered 200 with its token and 401 without it. Update signatures verified against the committed public key. On a clean Windows Sandbox without the Visual C++ Redistributable, the installed backend started and served the UI and API (the first build could not, which is why the backend is now linked statically). These checks covered installation and the backend; none of them inspected the desktop window itself (see the blocker below).
- **Provider acceptance, bounded.** One disposable project ran with real local Gemma routing to Cursor implementation and Codex review, followed by explicit source integration and restore.

### Release blockers

- **The v0.1.0 Windows desktop window does not load its interface.** Found November 25, 2026 by the installed-artifact smoke test (below), run against the published installer: the backend starts and answers correctly, but the app's WebView navigates to a `file:///D:/a/...` path from the CI build machine and shows `ERR_FILE_NOT_FOUND`. The release build passes that build folder to Tauri as a Windows drive path, which is read as an address to load rather than files to embed. Linux and macOS builds pass a Unix path and are not expected to be affected, but their desktop windows have not been inspected either. Do not treat any Windows build as functional until the smoke test passes against it.

### Outstanding

These are separate obligations; none is complete.

- **Installed-artifact smoke test on every platform.** It passes the backend checks on Windows but fails the frontend check (the blocker above). Linux and macOS can only run its served-UI form, which checks the installed UI bundle but not the native window.

- **Executable code signing for Windows and Linux.** Windows SmartScreen warns on first run.
- **Update verification end to end.** Update artifacts are signed and their signatures check out against the committed key, but no installed release has yet updated itself to a newer one; that needs a second release.
- **Fresh-machine runs of the complete desktop app** on clean Windows and macOS machines. The Windows Sandbox check covered the backend only, because the sandbox cannot use its WebView2 runtime.
- **Fresh-machine setup with real providers** following only the public documentation, and broader trials on unfamiliar projects.
- **Hosted CI parity.** macOS has no hosted test job, and Linux runs library tests only; both platforms' full suites are run locally, which is not hosted evidence.
- **Alpha limits:** guided onboarding, automatic cleanup and recovery of tasks whose owner is uncertain.

## Every release

1. Bump `version` in `gui/src-tauri/tauri.conf.json`, merge it, and confirm CI is green on `main`.
2. Push the tag `v<version>`. The release workflow builds every platform and creates a **draft** release; nothing is public yet.
3. Install the draft's artifacts and run the installed-artifact smoke test against each one before publishing. Building, generating an installer and installing it do not show the app works.
4. If dependencies changed, regenerate the [third-party notices](../THIRD_PARTY_NOTICES.md) with `python scripts/collect-third-party-notices.py`.
5. Publish the draft. That makes `releases/latest/download/latest.json` live for installed apps.

The [packaging runbook](runbook/packaging.md) describes the workflow, its required secrets and local builds.

## Installed-artifact smoke test

`scripts/release/smoke-installed.py --app <installed desktop executable>` launches the installed app against a throwaway state folder and checks three things: the bundled backend starts and enforces its token, the frontend initializes and reaches the backend, and one UI interaction (opening the Verifier Sandbox panel, which loads its settings from the backend) works. It exits non-zero on any failure and can write a JSON report.

- **Windows:** the checks run inside the app's own WebView2 window through a DevTools port, so a missing WebView runtime or a window that fails to load its interface fails the test.
- **Linux and macOS:** their native webviews cannot be driven this way. Pass `--browser <Chromium>` to run the frontend and interaction checks against the installed UI bundle served by the installed backend. That proves the bundle and backend work together, not that the native window renders; confirm the window by eye until that can be automated.

The smoke test shows the installed artifact starts and its interface works. It is not real-provider acceptance, which needs configured CLIs, accounts and a model, and it is not independent validation by anyone outside the project.

## Signing terms

| Term | What it means here | Where it applies |
|---|---|---|
| Signed update | Each update artifact carries a signature that the installed app verifies against the committed update public key before installing. | Windows installer, macOS `.app.tar.gz`, Linux AppImage |
| Self-update | The installed app checks `latest.json` and installs a verified update itself. | Windows, macOS, Linux AppImage. Not the `.deb`, which has no update channel yet: install new releases yourself. |
| Code signing | The executable itself is signed by a platform-trusted identity, so the OS recognizes the publisher. | macOS only (Developer ID Application) |
| Notarization | Apple has scanned the build and a ticket is stapled to the app and the `.dmg`, so Gatekeeper accepts it. | macOS only |
| Unsigned binary | No code signature. The app still verifies its own updates. | Windows and Linux binaries |

## Binary distribution

The [dependency inventory](../THIRD_PARTY_NOTICES.md) retains each package's own terms and full collected license texts. Crates published without a license file get the canonical text of a license they declare, recorded with its source in the collector's `CLARIFICATIONS` table; packages used only at build time are listed separately because they are not redistributed. `scripts/collect-third-party-notices.py` fails if a package linked into a shipped artifact has no license text; `--allow-missing` explicitly generates a preliminary inventory with such gaps listed.

Review all selected runtime features, including copyleft alternatives and obligations, and preserve required notices and source availability. Optional platform dependencies may have maintenance advisories outside the selected graph. Model weights and provider tools are separate installations and license decisions.

The desktop app uses the production web build and a content security policy that allows scripts only from the app itself (plus `unsafe-eval`, which the Dioxus web renderer needs to generate its DOM bindings), connections only to Tauri IPC and the local backend, and no objects, frames or form posts.

## History: initial publication

The repository was first published from a fresh export, so it shares no history with the private development repository. The exporter selects tracked source, lockfiles, tooling, examples and license material, overlays the public documentation templates from `docs/public/tree/`, checks every file against a private deny list, and writes a new directory with one independent commit under neutral contributor metadata:

```sh
python scripts/test-public-release.py
python scripts/prepare-public-release.py --output PATH_TO_NEW_CANDIDATE --privacy-policy PATH_TO_PRIVATE_POLICY --initialize-git
```

Since then, changes reach this repository as reviewed pull requests built by `scripts/sync-public-repo.py`, which exports each development change, checks the full public history against the deny list and the privacy patterns, and runs a secret scan (gitleaks, when installed) before pushing.
