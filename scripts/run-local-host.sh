#!/bin/sh
# POSIX counterpart of run-local-host.ps1 (Linux, macOS): supervises the backend and the
# compiled UI for one state root, restarts them with backoff, and writes host-runtime.json.
# A crashed supervisor's healthy children are adopted only on an exact PID, start time and
# executable match for the same configuration. Create <state>/host.stop to stop.
set -eu

usage() {
    echo "usage: run-local-host.sh --binary PATH --state-root DIR --assets DIR" \
        "[--bind 127.0.0.1] [--port 8787] [--ui-port 8080] [--allow-insecure-lan]" >&2
    exit 2
}
BINARY='' STATE='' ASSETS='' BIND=127.0.0.1 PORT=8787 UI_PORT=8080 LAN=''
while [ $# -gt 0 ]; do
    case $1 in
        --binary) [ $# -ge 2 ] || usage; BINARY=$2; shift 2 ;;
        --state-root) [ $# -ge 2 ] || usage; STATE=$2; shift 2 ;;
        --assets) [ $# -ge 2 ] || usage; ASSETS=$2; shift 2 ;;
        --bind) [ $# -ge 2 ] || usage; BIND=$2; shift 2 ;;
        --port) [ $# -ge 2 ] || usage; PORT=$2; shift 2 ;;
        --ui-port) [ $# -ge 2 ] || usage; UI_PORT=$2; shift 2 ;;
        # The services are plain HTTP; a non-loopback bind is refused without this opt-in.
        --allow-insecure-lan) LAN=--allow-insecure-lan; shift ;;
        *) usage ;;
    esac
done
[ -n "$BINARY" ] && [ -n "$STATE" ] && [ -n "$ASSETS" ] || usage
fail() { echo "$*" >&2; exit 1; }
case $PORT$UI_PORT in *[!0-9]*) fail 'Ports must be numbers' ;; esac
[ "$PORT" != "$UI_PORT" ] || fail 'Backend and UI require distinct ports'
[ -f "$BINARY" ] && [ -x "$BINARY" ] || fail "Binary is not an executable file: $BINARY"
BINARY=$(cd "$(dirname "$BINARY")" && pwd -P)/$(basename "$BINARY")
mkdir -p "$STATE"
STATE=$(cd "$STATE" && pwd -P)
ASSETS=$(cd "$ASSETS" && pwd -P)
[ -f "$ASSETS/index.html" ] || fail 'Compiled UI index.html is required'
# Paths go into JSON and the adoption record verbatim.
case $BINARY$STATE$ASSETS$BIND in *'"'* | *'\'* | *'|'*) fail 'Managed host paths must not contain quotes, backslashes or |' ;; esac

STOP=$STATE/host.stop
RECEIPT=$STATE/host-runtime.json
ADOPT=$STATE/host-runtime.adopt
LOCK=$STATE/host.lock
LOGS=$STATE/host-logs
mkdir -p "$LOGS"
cd "$STATE"

# Start marker of a process: /proc start ticks on Linux, `ps` lstart elsewhere.
start_of() {
    if [ -r "/proc/$1/stat" ]; then
        sed 's/^.*) //' "/proc/$1/stat" 2>/dev/null | cut -d' ' -f20
    else
        ps -o lstart= -p "$1" 2>/dev/null | tr -s ' ' '_'
    fi
}
state_of() {
    if [ -r "/proc/$1/stat" ]; then
        sed 's/^.*) //' "/proc/$1/stat" 2>/dev/null | cut -d' ' -f1
    else
        ps -o stat= -p "$1" 2>/dev/null
    fi
}
exe_of() {
    if [ -e "/proc/$1/exe" ]; then readlink "/proc/$1/exe" 2>/dev/null; else ps -o comm= -p "$1" 2>/dev/null; fi
}
# Alive means: exists, is not a zombie, and still has the recorded start marker.
alive() {
    [ -n "$1" ] && kill -0 "$1" 2>/dev/null || return 1
    [ "$(start_of "$1")" = "$2" ] || return 1
    case $(state_of "$1") in Z* | X*) return 1 ;; esac
}

# One supervisor per state root. A lock left by a dead supervisor (different start marker or
# no process) is stale and replaced.
take_lock() {
    if mkdir "$LOCK" 2>/dev/null; then
        echo "$$ $(start_of $$)" >"$LOCK/owner"
        return 0
    fi
    owner=$(cat "$LOCK/owner" 2>/dev/null || true)
    [ -n "$owner" ] || return 1
    # shellcheck disable=SC2086
    set -- $owner
    if alive "$1" "${2:-}"; then return 1; fi
    rm -rf "$LOCK"
    mkdir "$LOCK" 2>/dev/null || return 1
    echo "$$ $(start_of $$)" >"$LOCK/owner"
}
take_lock || fail "A managed host already owns this state root (remove $LOCK if that is wrong)"

CONFIG="$BINARY|$ASSETS|$BIND|$PORT|$UI_PORT"
backend_pid='' backend_start='' backend_launches=0 backend_retry=0
ui_pid='' ui_start='' ui_launches=0 ui_retry=0

# Adopt a crashed supervisor's exact children for the same configuration.
if [ -f "$ADOPT" ] && [ "$(sed -n 's/^config=//p' "$ADOPT")" = "$CONFIG" ]; then
    for role in backend ui; do
        saved=$(sed -n "s/^$role=//p" "$ADOPT")
        # shellcheck disable=SC2086
        [ -n "$saved" ] && set -- $saved || continue
        if alive "$1" "$2" && [ "$(exe_of "$1")" = "$BINARY" ]; then
            eval "${role}_pid=\$1 ${role}_start=\$2"
        fi
    done
fi

launch() {
    out=$LOGS/$1.stdout.log
    err=$LOGS/$1.stderr.log
    for log in "$out" "$err"; do
        if [ -f "$log" ]; then cp "$log" "$log.previous"; fi
    done
    if [ "$1" = backend ]; then
        # shellcheck disable=SC2086
        "$BINARY" --state "$STATE" serve --bind "$BIND" --port "$PORT" --ui-assets "$ASSETS" $LAN \
            </dev/null >"$out" 2>"$err" &
    else
        # shellcheck disable=SC2086
        "$BINARY" --state "$STATE" ui-serve --assets "$ASSETS" --bind "$BIND" --port "$UI_PORT" $LAN \
            </dev/null >"$out" 2>"$err" &
    fi
    pid=$!
    start=''
    tries=0
    while [ -z "$start" ] && [ $tries -lt 50 ]; do start=$(start_of "$pid"); tries=$((tries + 1)); done
    eval "launches=\$${1}_launches"
    launches=$((launches + 1))
    backoff=$((launches > 4 ? 30 : 1 << launches))
    retry=$(($(date +%s) + backoff))
    eval "${1}_pid=\$pid ${1}_start=\$start ${1}_launches=\$launches ${1}_retry=\$retry"
}

write_receipt() {
    {
        printf '{"supervisor_pid":%s,"binary":"%s","assets":"%s","state":"%s","bind":"%s","port":%s,"ui_port":%s,"updated_at":"%s","roles":{' \
            $$ "$BINARY" "$ASSETS" "$STATE" "$BIND" "$PORT" "$UI_PORT" "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
        sep=''
        for role in backend ui; do
            eval "pid=\$${role}_pid start=\$${role}_start launches=\$${role}_launches"
            [ -n "$pid" ] || continue
            printf '%s"%s":{"pid":%s,"started":"%s","launches":%s}' "$sep" "$role" "$pid" "$start" "$launches"
            sep=,
        done
        printf '}}\n'
    } >"$RECEIPT.part"
    mv -f "$RECEIPT.part" "$RECEIPT"
    {
        echo "config=$CONFIG"
        [ -z "$backend_pid" ] || echo "backend=$backend_pid $backend_start"
        [ -z "$ui_pid" ] || echo "ui=$ui_pid $ui_start"
    } >"$ADOPT.part"
    mv -f "$ADOPT.part" "$ADOPT"
}

stop_children() {
    for role in backend ui; do
        eval "pid=\$${role}_pid start=\$${role}_start"
        # Only the exact process this supervisor launched or adopted; never a reused PID.
        if alive "$pid" "$start"; then kill -TERM "$pid" 2>/dev/null || true; fi
    done
    waited=0
    while [ $waited -lt 100 ]; do
        left=0
        for role in backend ui; do
            eval "pid=\$${role}_pid start=\$${role}_start"
            if alive "$pid" "$start"; then left=1; fi
        done
        [ $left -eq 1 ] || break
        sleep 0.1
        waited=$((waited + 1))
    done
    for role in backend ui; do
        eval "pid=\$${role}_pid start=\$${role}_start"
        if alive "$pid" "$start"; then kill -KILL "$pid" 2>/dev/null || true; fi
        [ -z "$pid" ] || wait "$pid" 2>/dev/null || true
    done
}

STOPPING=0
trap 'STOPPING=1' TERM INT HUP
trap 'stop_children; rm -rf "$LOCK"' EXIT

while [ $STOPPING -eq 0 ] && [ ! -e "$STOP" ]; do
    now=$(date +%s)
    for role in backend ui; do
        eval "pid=\$${role}_pid start=\$${role}_start retry=\$${role}_retry"
        if [ -n "$pid" ] && ! alive "$pid" "$start"; then
            wait "$pid" 2>/dev/null || true
            eval "${role}_pid='' ${role}_start=''"
            pid=''
        fi
        if [ -z "$pid" ] && [ "$now" -ge "$retry" ]; then launch "$role"; fi
    done
    write_receipt || echo 'host receipt update deferred' >&2
    sleep 0.5
done
