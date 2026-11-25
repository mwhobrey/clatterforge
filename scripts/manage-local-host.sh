#!/bin/sh
# POSIX counterpart of manage-local-host.ps1 (Linux, macOS).
#   manage-local-host.sh [status|start|stop|install-startup|remove-startup]
#       [--binary PATH] [--state-root DIR] [--assets DIR] [--bind ADDR] [--port N] [--ui-port N]
#       [--allow-insecure-lan]
# Startup is per user: a systemd user unit on Linux, a LaunchAgent on macOS. It keeps the
# real coding provider's profile and credentials; no root, password storage or network
# widening.
set -eu
here=$(cd "$(dirname "$0")" && pwd -P)
action=${1:-status}
[ $# -eq 0 ] || shift
BINARY='' STATE='' ASSETS='' PASS=''
while [ $# -gt 0 ]; do
    case $1 in
        --binary) BINARY=$2; shift 2 ;;
        --state-root) STATE=$2; shift 2 ;;
        --assets) ASSETS=$2; shift 2 ;;
        --bind | --port | --ui-port) PASS="$PASS $1 $2"; shift 2 ;;
        --allow-insecure-lan) PASS="$PASS $1"; shift ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
done
fail() { echo "$*" >&2; exit 1; }
# State directory rule shared with the binary: `.clatterforge`, unless only a pre-rename
# `.conductor` exists, which keeps being used in place.
if [ -z "$STATE" ]; then
    STATE=$here/../.clatterforge
    if [ ! -e "$STATE" ] && [ -d "$here/../.conductor" ]; then STATE=$here/../.conductor; fi
fi
if [ -z "$BINARY" ]; then BINARY=$here/../target/debug/clatterforge; fi
if [ -z "$ASSETS" ]; then ASSETS=$STATE/runtime/ui; fi
mkdir -p "$STATE"
STATE=$(cd "$STATE" && pwd -P)
runner=$here/run-local-host.sh
case $(uname -s) in
    Darwin) startup=$HOME/Library/LaunchAgents/dev.clatterforge.host.plist ;;
    *) startup=${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/clatterforge-host.service ;;
esac

case $action in
    status) if [ -f "$STATE/host-runtime.json" ]; then cat "$STATE/host-runtime.json"; fi; exit 0 ;;
    stop) echo 'Explicit operator stop' >"$STATE/host.stop"; exit 0 ;;
    remove-startup)
        if [ -f "$startup" ]; then
            case $(uname -s) in
                Darwin) launchctl bootout "gui/$(id -u)" "$startup" 2>/dev/null || true ;;
                *) systemctl --user disable clatterforge-host.service 2>/dev/null || true ;;
            esac
            rm -f "$startup"
        fi
        exit 0 ;;
    start | install-startup) ;;
    *) fail "unknown action: $action" ;;
esac

[ -x "$BINARY" ] || fail "Build the binary first: $BINARY"
[ -f "$ASSETS/index.html" ] || fail 'Build and install compiled UI assets first'
BINARY=$(cd "$(dirname "$BINARY")" && pwd -P)/$(basename "$BINARY")
ASSETS=$(cd "$ASSETS" && pwd -P)
case $BINARY$STATE$ASSETS$runner in *'"'* | *'\'* | *'<'* | *'&'*) fail 'Paths must not contain quotes, backslashes, < or &' ;; esac

if [ "$action" = install-startup ]; then
    [ ! -e "$startup" ] || fail "Startup entry already exists: $startup"
    mkdir -p "$(dirname "$startup")"
    case $(uname -s) in
        Darwin)
            {
                echo '<?xml version="1.0" encoding="UTF-8"?>'
                echo '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">'
                echo '<plist version="1.0"><dict>'
                echo '  <key>Label</key><string>dev.clatterforge.host</string>'
                echo '  <key>ProgramArguments</key><array>'
                for arg in /bin/sh "$runner" --binary "$BINARY" --state-root "$STATE" --assets "$ASSETS" $PASS; do
                    echo "    <string>$arg</string>"
                done
                echo '  </array>'
                echo '  <key>RunAtLoad</key><true/>'
                echo "  <key>StandardErrorPath</key><string>$STATE/host-supervisor.stderr.log</string>"
                echo '</dict></plist>'
            } >"$startup"
            echo "Installed current-user login startup: $startup"
            echo "It starts at next login; to start now: launchctl bootstrap gui/$(id -u) \"$startup\""
            ;;
        *)
            {
                echo '[Unit]'
                echo 'Description=Clatterforge managed local backend and compiled UI'
                echo '[Service]'
                printf 'ExecStart=/bin/sh "%s" --binary "%s" --state-root "%s" --assets "%s"%s\n' \
                    "$runner" "$BINARY" "$STATE" "$ASSETS" "$PASS"
                echo 'Restart=no'
                echo '[Install]'
                echo 'WantedBy=default.target'
            } >"$startup"
            systemctl --user daemon-reload
            systemctl --user enable clatterforge-host.service
            echo "Installed current-user login startup: $startup"
            echo 'To start now: systemctl --user start clatterforge-host.service'
            ;;
    esac
    exit 0
fi

rm -f "$STATE/host.stop"
# shellcheck disable=SC2086
nohup /bin/sh "$runner" --binary "$BINARY" --state-root "$STATE" --assets "$ASSETS" $PASS \
    </dev/null >"$STATE/host-supervisor.stdout.log" 2>"$STATE/host-supervisor.stderr.log" &
echo "Managed host launched: $!"
