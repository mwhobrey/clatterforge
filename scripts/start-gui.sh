#!/usr/bin/env bash
set -euo pipefail

BIND_ADDR="${CLATTERFORGE_BIND:-127.0.0.1}"
API_PORT="${CLATTERFORGE_API_PORT:-8787}"
UI_PORT="${CLATTERFORGE_UI_PORT:-8080}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# 1. Resolve clatterforge binary
if [[ -x "${SCRIPT_DIR}/clatterforge" ]]; then
    CLI_BIN="${SCRIPT_DIR}/clatterforge"
elif [[ -x "${SCRIPT_DIR}/dev-conductor" ]]; then
    CLI_BIN="${SCRIPT_DIR}/dev-conductor"
elif command -v clatterforge >/dev/null 2>&1; then
    CLI_BIN="$(command -v clatterforge)"
else
    echo "[ERROR] clatterforge binary not found." >&2
    exit 1
fi

# 2. Resolve compiled Web GUI assets directory
if [[ -d "${SCRIPT_DIR}/ui" ]]; then
    UI_DIR="${SCRIPT_DIR}/ui"
elif [[ -d "${HOME}/.local/share/clatterforge/ui" ]]; then
    UI_DIR="${HOME}/.local/share/clatterforge/ui"
else
    echo "[ERROR] Compiled GUI assets (ui/) not found in ${SCRIPT_DIR}/ui or ~/.local/share/clatterforge/ui" >&2
    exit 1
fi

# Same rule as the binary: `.clatterforge`, unless only a pre-rename `.conductor` exists.
if [[ -n "${CLATTERFORGE_STATE:-}" ]]; then
    STATE_DIR="${CLATTERFORGE_STATE}"
elif [[ -n "${CONDUCTOR_STATE:-}" ]]; then
    STATE_DIR="${CONDUCTOR_STATE}"
elif [[ ! -e "$(pwd)/.clatterforge" && -d "$(pwd)/.conductor" ]]; then
    STATE_DIR="$(pwd)/.conductor"
else
    STATE_DIR="$(pwd)/.clatterforge"
fi
mkdir -p "${STATE_DIR}"
TOKEN_FILE="${STATE_DIR}/serve-token"

cleanup() {
    [[ -n "${API_PID:-}" ]] && kill "${API_PID}" 2>/dev/null || true
    [[ -n "${UI_PID:-}" ]] && kill "${UI_PID}" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

echo "[CF // SYSTEMS] Starting Clatterforge API + Unified Web UI on ${BIND_ADDR}:${API_PORT}..."
"${CLI_BIN}" --state "${STATE_DIR}" serve --bind "${BIND_ADDR}" --port "${API_PORT}" --ui-assets "${UI_DIR}" &
API_PID=$!

API_READY=0
for _ in {1..50}; do
    if ! kill -0 "${API_PID}" 2>/dev/null; then
        break
    fi
    if [[ -s "${TOKEN_FILE}" ]]; then
        if command -v curl >/dev/null 2>&1; then
            if curl -sf "http://127.0.0.1:${API_PORT}/health" >/dev/null 2>&1; then
                API_READY=1
                break
            fi
        elif (exec 3<>"/dev/tcp/127.0.0.1/${API_PORT}") 2>/dev/null; then
            exec 3>&-
            API_READY=1
            break
        fi
    fi
    sleep 0.1
done

TOKEN="$(tr -d '[:space:]' < "${TOKEN_FILE}" 2>/dev/null || true)"
if [[ "${API_READY}" -ne 1 ]] || [[ -z "${TOKEN}" ]] || ! kill -0 "${API_PID}" 2>/dev/null; then
    # Usually the API port is already taken. Continuing would print a sign-in URL that
    # cannot work, or silently attach the GUI to someone else's server.
    echo "[ERROR] Clatterforge API did not start on ${BIND_ADDR}:${API_PORT} (port in use?)." >&2
    echo "        Set CLATTERFORGE_API_PORT to another port or stop the other server." >&2
    exit 1
fi

echo "[CF // SYSTEMS] Starting Clatterforge Standalone Web UI on ${BIND_ADDR}:${UI_PORT}..."
"${CLI_BIN}" --state "${STATE_DIR}" ui-serve --assets "${UI_DIR}" --bind "${BIND_ADDR}" --port "${UI_PORT}" &
UI_PID=$!

AUTO_URL="http://127.0.0.1:${API_PORT}/#token=${TOKEN}&base=http://127.0.0.1:${API_PORT}"

echo ""
echo "=========================================================="
echo "  CLATTERFORGE (CF // SYSTEMS) — INSTRUMENTATION GUI      "
echo "=========================================================="
echo "  Unified Web UI + API:   http://127.0.0.1:${API_PORT}/"
echo "  Standalone UI Mirror:   http://127.0.0.1:${UI_PORT}/"
echo "  Bind Address:           ${BIND_ADDR}"
echo "  Bearer Token File:      ${TOKEN_FILE}"
echo "  Press Ctrl+C to shut down API & Web GUI."
echo "=========================================================="

if command -v xdg-open >/dev/null 2>&1 && [[ -n "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ]]; then
    xdg-open "${AUTO_URL}" >/dev/null 2>&1 || true
fi

# Exit (and let the trap stop the survivor) as soon as either server goes away, rather
# than leaving a GUI up with no API behind it.
# Polled instead of `wait -n` so this also runs on the bash 3.2 that macOS ships.
while kill -0 "${API_PID}" 2>/dev/null && kill -0 "${UI_PID}" 2>/dev/null; do
    sleep 1
done
