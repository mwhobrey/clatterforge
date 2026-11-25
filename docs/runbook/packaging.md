# Packaging and updates

Clatterforge ships as one per-user Windows installer (NSIS, no administrator rights) built by the Tauri bundler. It installs the desktop app, the forge backend (`clatterforge.exe`, shipped beside the app as a sidecar) and the compiled web UI (`ui/`, served to remote browsers by the backend). See ADR 0018 for the reasoning.

## Where things live

- **App:** the per-user install folder chosen at install time (by default under `%LOCALAPPDATA%\Programs`).
- **State:** `%LOCALAPPDATA%\Clatterforge\state`, outside the install folder, so reinstalling, updating or uninstalling never touches tasks, presets, tokens or logs. A portable bundle that already has `.clatterforge` (or a legacy `.conductor`) beside the app keeps using it, and `CLATTERFORGE_STATE` overrides both.
- The desktop app starts its own backend while it runs, or reuses a forge already serving on port 8787 when its token matches.

## Building an installer

```powershell
scripts/release/build-installer.ps1 -SkipUpdater         # local test build, no update artifacts
scripts/release/build-installer.ps1 -Notes "What changed" # release build with signed update artifacts
```

The script builds the release backend and stages it as the sidecar, then runs `cargo tauri build --bundles nsis` with release-only settings injected through a config override (sidecar, `ui/` resource, update artifacts, update endpoint). None of those live in the committed `tauri.conf.json`, so ordinary development and CI builds never need them. Output lands in `dist/installer/<version>/`.

## Signed updates

The installed app checks for an update at startup, asks before installing, then downloads it, stops its backend, installs in passive mode and restarts on the new version. Every update must verify against the public key compiled into the app; anything else is refused.

One-time setup by the maintainer:

1. Run `scripts/release/new-update-key.ps1`. It asks you for a password, writes the private key outside the repository (default `~/.clatterforge/release/update.key`) and puts only the public key into `tauri.conf.json`. Keep a backup of the key and password: without them, installed apps cannot be updated in place.
2. Create the private release file `~/.clatterforge/release/updater.json` with the manifest endpoint and asset URL pattern:

```json
{ "endpoint": "https://github.com/<owner>/<repo>/releases/latest/download/latest.json",
  "asset_base_url": "https://github.com/<owner>/<repo>/releases/download/v{version}/" }
```

For each release, bump `version` in `gui/src-tauri/tauri.conf.json`, set `TAURI_SIGNING_PRIVATE_KEY` (path or contents) and `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`, run the build script, then upload the installer, its `.sig` and `latest.json` to a GitHub release tagged `v<version>`. Builds without a public key or endpoint never check for updates. Until the repository is public, the manifest URL returns 404 and the app simply reports nothing.

## Linux

Added November 25, 2026 (#38). `scripts/release/build-installer.sh` builds an AppImage and a `.deb` with the same release-only override as Windows: backend sidecar, `ui/` resource, update artifacts and endpoint.

```sh
scripts/release/build-installer.sh --skip-updater          # local test build
scripts/release/build-installer.sh --notes "What changed"  # signed AppImage update artifact + latest.json
```

It needs Tauri's Linux build dependencies, `tauri-cli`, `dioxus-cli` and the `wasm32-unknown-unknown` target. `scripts/release/linux-build.Dockerfile` pins all of them (Rust 1.90.0, tauri-cli 2.12.4, dioxus-cli 0.7.10). The script sets `APPIMAGE_EXTRACT_AND_RUN=1`, so the bundler's linuxdeploy runs without FUSE. Override the target directory with `INSTALLER_TARGET_DIR`.

- **Layout:** the `.deb` installs `/usr/bin/clatterforge-desktop`, the backend as `/usr/bin/clatterforge` and the UI under `/usr/lib/Clatterforge/ui`, and depends on `libwebkit2gtk-4.1-0` and `libgtk-3-0`. The AppImage carries the same tree.
- **State:** `$XDG_DATA_HOME/clatterforge/state` (default `~/.local/share/clatterforge/state`); `CLATTERFORGE_STATE` overrides it. Removing the package leaves it.
- **Updates:** only the AppImage updates itself (Tauri replaces the file named by `$APPIMAGE`). `.deb` installs update through the package manager. The script's `latest.json` holds only the `linux-x86_64` entry; the release workflow (#40) merges it with the Windows one.

Verified on a clean Debian 12 container as an unprivileged user under Xvfb, for the `.deb` and the extracted AppImage: the app starts its sidecar with the XDG state folder and the bundled UI, the UI is served, the API answers 200 with the token and 401 without it, and uninstalling the `.deb` keeps the state. A signed build with a throwaway key produced the AppImage `.sig` and `latest.json`. Not yet exercised: an in-place AppImage update (it needs a mounted AppImage, so a FUSE-capable desktop) and desktop integration on distributions other than Debian.

## macOS

Added November 25, 2026 (#39). `scripts/release/build-installer.sh` also runs on macOS and builds `Clatterforge.app` and a `.dmg` for the host CPU, with the same release-only override (sidecar, `ui/` resource, update artifacts, endpoint).

```sh
scripts/release/build-installer.sh --skip-updater                # local test build, ad-hoc signed
scripts/release/build-installer.sh --skip-notarize --notes "..."  # signed update artifact, no notarization
scripts/release/build-installer.sh --notes "What changed"        # signed, notarized and stapled (app and dmg)
```

It needs Xcode command line tools, `tauri-cli` and `dioxus-cli` at the versions in `release.yml` and the `wasm32-unknown-unknown` target.

- **Layout:** sidecar `Contents/MacOS/clatterforge`, UI `Contents/Resources/ui`.
- **State:** `~/Library/Application Support/Clatterforge/state`; `CLATTERFORGE_STATE` overrides it. Deleting the app leaves it.
- **Signing and notarization** come from the environment. Set `APPLE_SIGNING_IDENTITY` to the Developer ID Application identity (list yours with `security find-identity -v -p codesigning`), and `APPLE_API_ISSUER`, `APPLE_API_KEY` and `APPLE_API_KEY_PATH` (the `.p8` file) from an App Store Connect API key with the Developer role. Never put these in a file in the repository.
- **Updates:** the update artifact is `Clatterforge_<version>_<arch>.app.tar.gz` plus `.sig`; `latest.json` holds `darwin-aarch64` or `darwin-x86_64`. The release workflow merges both architectures with the other platforms.
- **DMG layout:** the script sets `CI=true` so create-dmg skips its Finder AppleScript step (it hangs without a GUI session); the image has default icon placement.

Verified locally on Apple silicon (Rust 1.90.0, November 25, 2026): an unsigned build produces the app and a valid `.dmg`. A build with the owner's Developer ID and an App Store Connect API key produced a hardened-runtime signed app and `.dmg`, both notarized and stapled; `codesign --verify --deep --strict`, `stapler validate` and `spctl` (source `Notarized Developer ID`) accept them. Launched with a throwaway `HOME`, the signed app starts its sidecar with the Application Support state folder and the bundled UI, serves the UI (200), and the API answers 200 with the token and 401 without it. A throwaway minisign key produced the `.app.tar.gz`, its `.sig` and a `latest.json` with a `darwin-aarch64` entry that `merge-manifests.py` accepts. Not yet exercised: an in-place update with the real update key, an Intel build, and the CI jobs.

## Releasing from CI

Added November 25, 2026 (#40). `.github/workflows/release.yml` runs on a `v<version>` tag (or by hand) once Actions are available on the public repository:

1. `check` confirms the tag matches `version` in `tauri.conf.json` and that the update public key is committed.
2. `build` runs the same scripts as a local release on `windows-latest`, `ubuntu-22.04` (the oldest runner, so the AppImage works on older glibc), `macos-latest` (Apple silicon) and `macos-15-intel`. Each writes `~/.clatterforge/release/updater.json` from the repository URL, so no owner name is committed, and signs with the key from secrets.
3. `release` merges the per-platform manifests with `scripts/release/merge-manifests.py`, which refuses mixed versions, duplicate platforms and unsigned or non-https entries, and creates a **draft** release with every installer, signature and `latest.json`. Only this job can write to the repository.

Before the first run, add repository secrets `TAURI_SIGNING_PRIVATE_KEY` (the key file's contents) and `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`, and commit the public key that `new-update-key.ps1` wrote. To release: bump `version`, commit, tag `v<version>`, push the tag, review the draft and publish it. Publishing makes `releases/latest/download/latest.json` live for installed apps.

macOS builds need these additional repository secrets (Settings, Secrets and variables, Actions). None is ever committed or printed:

| Secret | What it is |
|---|---|
| `APPLE_CERTIFICATE` | Base64 of the exported Developer ID Application certificate and private key (`.p12`): `base64 -i cert.p12 \| pbcopy`. Tauri imports it into a temporary keychain. |
| `APPLE_CERTIFICATE_PASSWORD` | The password chosen when exporting the `.p12`. |
| `APPLE_SIGNING_IDENTITY` | The identity name, for example `Developer ID Application: Name (TEAMID)` (`security find-identity -v -p codesigning`). |
| `APPLE_API_ISSUER` | The issuer ID shown on the App Store Connect API keys page. |
| `APPLE_API_KEY` | The key ID of an App Store Connect API key (Developer role is enough for notarization). |
| `APPLE_API_KEY_P8` | The contents of that key's `AuthKey_<id>.p8` file. The workflow writes it to a temporary file and points `APPLE_API_KEY_PATH` at it. |

Without them a macOS job fails before building: unset secrets are empty strings, and the workflow refuses to ship an ad-hoc signed, un-notarized app.

The workflow is linted (actionlint) and the merge script is unit-tested (`scripts/test-merge-manifests.py`), but the workflow itself has not run: Actions are unavailable until the repository is public.

## Code signing

Installers and binaries are not code-signed yet, so Windows SmartScreen warns on first run. `-SignCommand "<signing command> %1"` passes a signing command to Tauri, which signs each file before update signatures are computed; use it once a certificate or signing service is available.

## Not covered

A separately staged always-on host (`manage-local-host.ps1` with a `runtime\` copy, as in a development forge) is still upgraded by hand: stop it, back up `state.sqlite`, replace the runtime binary and UI, start it.
