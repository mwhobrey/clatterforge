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

## Code signing

Installers and binaries are not code-signed yet, so Windows SmartScreen warns on first run. `-SignCommand "<signing command> %1"` passes a signing command to Tauri, which signs each file before update signatures are computed; use it once a certificate or signing service is available.

## Not covered

A separately staged always-on host (`manage-local-host.ps1` with a `runtime\` copy, as in a development forge) is still upgraded by hand: stop it, back up `state.sqlite`, replace the runtime binary and UI, start it.
