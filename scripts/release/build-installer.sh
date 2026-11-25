#!/bin/sh
# Builds the Linux desktop bundles (AppImage and .deb) and, unless --skip-updater, the signed
# AppImage update artifact and a latest.json manifest (ADR 0018). Linux counterpart of
# build-installer.ps1; see that script's help for the private release file and signing key.
#
#   scripts/release/build-installer.sh [--skip-updater] [--notes TEXT] [--out-dir DIR]
#       [--release-config FILE]
#
# Needs Rust with the wasm32-unknown-unknown target, tauri-cli, dioxus-cli (dx), python3 and
# Tauri's Linux build dependencies (see linux-build.Dockerfile). Only the AppImage updates
# itself; .deb installs are updated through the package manager.
set -eu
repo=$(cd "$(dirname "$0")/../.." && pwd -P)
tauri_dir=$repo/gui/src-tauri
release_config=$HOME/.clatterforge/release/updater.json
out_dir='' notes='' skip_updater=0
while [ $# -gt 0 ]; do
    case $1 in
        --skip-updater) skip_updater=1; shift ;;
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
# linuxdeploy is itself an AppImage; extract-and-run avoids needing FUSE (containers, CI).
(cd "$tauri_dir" && CARGO_TARGET_DIR=$bundle_target APPIMAGE_EXTRACT_AND_RUN=1 \
    cargo tauri build --bundles deb,appimage --config "$override")

bundle_dir=$bundle_target/release/bundle
appimage=$(ls "$bundle_dir"/appimage/*_"$version"_*.AppImage 2>/dev/null | head -n 1)
deb=$(ls "$bundle_dir"/deb/*_"$version"_*.deb 2>/dev/null | head -n 1)
[ -n "$appimage" ] || fail "AppImage not found in $bundle_dir/appimage"
[ -n "$deb" ] || fail ".deb not found in $bundle_dir/deb"
mkdir -p "$out_dir"
cp "$appimage" "$deb" "$out_dir/"

if [ $skip_updater -eq 0 ]; then
    [ -f "$appimage.sig" ] || fail "update signature missing: $appimage.sig"
    cp "$appimage.sig" "$out_dir/"
    python3 - "$out_dir/latest.json" "$version" "$notes" "$appimage" "$release_config" <<'PY'
import datetime, json, os, sys
out, version, notes, appimage, release_config = sys.argv[1:]
release = json.load(open(release_config, encoding='utf-8-sig'))
manifest = {
    'version': version,
    'notes': notes,
    'pub_date': datetime.datetime.now(datetime.timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ'),
    'platforms': {'linux-x86_64': {
        'signature': open(appimage + '.sig', encoding='utf-8').read().strip(),
        'url': release['asset_base_url'].replace('{version}', version) + os.path.basename(appimage),
    }},
}
json.dump(manifest, open(out, 'w', encoding='utf-8'), indent=2)
PY
fi
ls -l "$out_dir"
echo "Installer artifacts: $out_dir"
