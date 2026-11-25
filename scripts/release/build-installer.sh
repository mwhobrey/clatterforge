#!/bin/sh
# Builds the desktop bundles for this Unix host and, unless --skip-updater, the signed update
# artifact and a latest.json manifest (ADR 0018). Counterpart of build-installer.ps1; see that
# script's help for the private release file and signing key.
#   Linux: AppImage and .deb (only the AppImage updates itself).
#   macOS: .app and .dmg, plus a .app.tar.gz update artifact for this CPU (darwin-aarch64 or
#          darwin-x86_64). Signing and notarization come from the environment, never arguments:
#            APPLE_SIGNING_IDENTITY   e.g. "Developer ID Application: Name (TEAMID)"
#            APPLE_API_ISSUER, APPLE_API_KEY, APPLE_API_KEY_PATH   App Store Connect API key
#          In CI also APPLE_CERTIFICATE (base64 .p12) and APPLE_CERTIFICATE_PASSWORD, which
#          Tauri imports into a temporary keychain. A signing identity without the API key is
#          refused unless --skip-notarize. With no identity the app is only ad-hoc signed.
#
#   scripts/release/build-installer.sh [--skip-updater] [--skip-notarize] [--notes TEXT]
#       [--out-dir DIR] [--release-config FILE]
#
# Needs Rust with the wasm32-unknown-unknown target, tauri-cli, dioxus-cli (dx), python3 and,
# on Linux, Tauri's build dependencies (see linux-build.Dockerfile).
set -eu
repo=$(cd "$(dirname "$0")/../.." && pwd -P)
tauri_dir=$repo/gui/src-tauri
release_config=$HOME/.clatterforge/release/updater.json
out_dir='' notes='' skip_updater=0 skip_notarize=0
os_name=$(uname -s)
while [ $# -gt 0 ]; do
    case $1 in
        --skip-updater) skip_updater=1; shift ;;
        --skip-notarize) skip_notarize=1; shift ;;
        --notes) notes=$2; shift 2 ;;
        --out-dir) out_dir=$2; shift 2 ;;
        --release-config) release_config=$2; shift 2 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
done
fail() { echo "$*" >&2; exit 1; }
version=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["version"])' "$tauri_dir/tauri.conf.json")
[ -n "$out_dir" ] || out_dir=$repo/dist/installer/$version
target_root=${INSTALLER_TARGET_DIR:-$repo/target}

if [ $skip_updater -eq 0 ]; then
    python3 -c 'import json,sys; sys.exit(0 if json.load(open(sys.argv[1]))["plugins"]["updater"]["pubkey"].strip() else 1)' \
        "$tauri_dir/tauri.conf.json" \
        || fail 'No update public key in tauri.conf.json; run scripts/release/new-update-key.ps1 first, or pass --skip-updater for a local test build.'
    [ -n "${TAURI_SIGNING_PRIVATE_KEY:-}" ] \
        || fail 'Set TAURI_SIGNING_PRIVATE_KEY (and TAURI_SIGNING_PRIVATE_KEY_PASSWORD) to sign update artifacts, or pass --skip-updater.'
    [ -f "$release_config" ] || fail "Release endpoint file not found: $release_config"
fi

if [ "$os_name" = Darwin ] && [ -n "${APPLE_SIGNING_IDENTITY:-}" ] && [ $skip_notarize -eq 0 ]; then
    [ -n "${APPLE_API_ISSUER:-}" ] && [ -n "${APPLE_API_KEY:-}" ] && [ -n "${APPLE_API_KEY_PATH:-}" ] \
        || fail 'A signing identity needs APPLE_API_ISSUER, APPLE_API_KEY and APPLE_API_KEY_PATH to notarize, or pass --skip-notarize.'
    [ -f "$APPLE_API_KEY_PATH" ] || fail "APPLE_API_KEY_PATH does not exist: $APPLE_API_KEY_PATH"
fi

# Release-only settings, injected through a config override exactly as on Windows.
override=$(mktemp "${TMPDIR:-/tmp}/clatterforge-release-XXXXXX.json")
trap 'rm -f "$override"' EXIT
bundle_target=$target_root/installer-desktop
# dx honours CARGO_TARGET_DIR, so the web build lands under the bundle target; point Tauri at
# that fresh output rather than whatever an earlier development build left in gui/target.
web_dist=$bundle_target/dx/clatterforge-gui/release/web/public
python3 - "$override" "$skip_updater" "$release_config" "$web_dist" <<'PY'
import json, sys
path, skip, release_config, web_dist = sys.argv[1], sys.argv[2] == '1', sys.argv[3], sys.argv[4]
override = {
    'build': {'frontendDist': web_dist},
    'bundle': {
        'externalBin': ['binaries/clatterforge'],
        'resources': {web_dist + '/': 'ui/'},
        'createUpdaterArtifacts': not skip,
    },
}
if not skip:
    release = json.load(open(release_config, encoding='utf-8-sig'))
    for field in ('endpoint', 'asset_base_url'):
        if not str(release.get(field, '')).startswith('https://'):
            sys.exit(f'{release_config} needs an https {field}.')
    override['plugins'] = {'updater': {'endpoints': [release['endpoint']]}}
json.dump(override, open(path, 'w'))
PY

echo '== backend (release)'
CARGO_TARGET_DIR=$target_root/installer cargo build --release --locked -p clatterforge \
    --manifest-path "$repo/Cargo.toml"
triple=$(rustc -vV | sed -n 's/^host: //p')
mkdir -p "$tauri_dir/binaries"
cp "$target_root/installer/release/clatterforge" "$tauri_dir/binaries/clatterforge-$triple"

echo '== desktop app and bundles'
if [ "$os_name" = Darwin ]; then
    # Tauri notarizes only when all of these are set; --skip-notarize signs without them.
    if [ $skip_notarize -eq 1 ]; then unset APPLE_API_ISSUER APPLE_API_KEY APPLE_API_KEY_PATH APPLE_ID APPLE_PASSWORD APPLE_TEAM_ID; fi
    # CI=true makes the bundler skip create-dmg's Finder AppleScript layout step, which hangs
    # without a logged-in GUI session (CI runners, ssh) and only positions icons.
    (cd "$tauri_dir" && CI=true CARGO_TARGET_DIR=$bundle_target cargo tauri build --bundles app,dmg --config "$override")
else
    # linuxdeploy is itself an AppImage; extract-and-run avoids needing FUSE (containers, CI).
    (cd "$tauri_dir" && CARGO_TARGET_DIR=$bundle_target APPIMAGE_EXTRACT_AND_RUN=1 \
        cargo tauri build --bundles deb,appimage --config "$override")
fi

bundle_dir=$bundle_target/release/bundle
mkdir -p "$out_dir"
if [ "$os_name" = Darwin ]; then
    arch=$(uname -m)
    case $arch in
        arm64) platform=darwin-aarch64; arch_label=aarch64 ;;
        x86_64) platform=darwin-x86_64; arch_label=x86_64 ;;
        *) fail "unsupported macOS CPU: $arch" ;;
    esac
    app=$(ls -d "$bundle_dir"/macos/*.app 2>/dev/null | head -n 1)
    dmg=$(ls "$bundle_dir"/dmg/*_"$version"_*.dmg 2>/dev/null | head -n 1)
    [ -n "$app" ] || fail "app bundle not found in $bundle_dir/macos"
    [ -n "$dmg" ] || fail ".dmg not found in $bundle_dir/dmg"
    if [ -n "${APPLE_SIGNING_IDENTITY:-}" ]; then
        codesign --verify --deep --strict "$app" || fail 'codesign verification failed'
        if [ $skip_notarize -eq 0 ]; then
            xcrun stapler validate "$app" || fail 'notarization ticket is not stapled to the app'
            spctl --assess --type execute "$app" || fail 'Gatekeeper rejects the app'
        fi
    fi
    cp "$dmg" "$out_dir/"
    # The bundler names the update archive after the app only, so two CPUs would collide.
    artifact=$out_dir/Clatterforge_${version}_${arch_label}.app.tar.gz
    if [ $skip_updater -eq 0 ]; then
        src=$(ls "$bundle_dir"/macos/*.app.tar.gz 2>/dev/null | head -n 1)
        [ -n "$src" ] && [ -f "$src.sig" ] || fail "update archive or signature missing in $bundle_dir/macos"
        cp "$src" "$artifact"
        cp "$src.sig" "$artifact.sig"
    fi
else
    platform=linux-x86_64
    artifact=$(ls "$bundle_dir"/appimage/*_"$version"_*.AppImage 2>/dev/null | head -n 1)
    deb=$(ls "$bundle_dir"/deb/*_"$version"_*.deb 2>/dev/null | head -n 1)
    [ -n "$artifact" ] || fail "AppImage not found in $bundle_dir/appimage"
    [ -n "$deb" ] || fail ".deb not found in $bundle_dir/deb"
    cp "$artifact" "$deb" "$out_dir/"
    if [ $skip_updater -eq 0 ]; then
        [ -f "$artifact.sig" ] || fail "update signature missing: $artifact.sig"
        cp "$artifact.sig" "$out_dir/"
    fi
    artifact=$out_dir/$(basename "$artifact")
fi

if [ $skip_updater -eq 0 ]; then
    python3 - "$out_dir/latest.json" "$version" "$notes" "$artifact" "$release_config" "$platform" <<'PY'
import datetime, json, os, sys
out, version, notes, artifact, release_config, platform = sys.argv[1:]
release = json.load(open(release_config, encoding='utf-8-sig'))
manifest = {
    'version': version,
    'notes': notes,
    'pub_date': datetime.datetime.now(datetime.timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ'),
    'platforms': {platform: {
        'signature': open(artifact + '.sig', encoding='utf-8').read().strip(),
        'url': release['asset_base_url'].replace('{version}', version) + os.path.basename(artifact),
    }},
}
json.dump(manifest, open(out, 'w', encoding='utf-8'), indent=2)
PY
fi
ls -l "$out_dir"
echo "Installer artifacts: $out_dir"
