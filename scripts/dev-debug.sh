#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
GUI_DIR="${REPO_ROOT}/gui"
DEBUG_UI_DIR="${GUI_DIR}/target/dx/conductor-gui/debug/web/public"

DESKTOP=0
SKIP_UI_BUILD=0
USE_DIST_STATE=0
NO_BROWSER=0
STATE_ARG=""
TARGET_DIR="${CARGO_TARGET_DIR:-${REPO_ROOT}/target}"
BIND_ADDR="${CLATTERFORGE_BIND:-127.0.0.1}"
API_PORT="${CLATTERFORGE_API_PORT:-8787}"

usage() {
    cat <<'EOF'
Usage: scripts/dev-debug.sh [OPTIONS]

Run Clatterforge in debug mode directly from source (Linux / macOS).

Options:
  --desktop         Start debug backend and launch `cargo tauri dev` (live reload)
  --skip-ui-build   Skip `dx build --platform web` and reuse existing debug UI bundle
  --dist-state      Use existing bundle state (`dist/linux-x86_64/.conductor` or macOS equiv)
  --state DIR       Explicit `--state` directory (default: $CONDUCTOR_STATE or ./.conductor)
  --target-dir DIR  Cargo target directory (default: $CARGO_TARGET_DIR or ./target)
  --bind ADDR       Bind address (default: 127.0.0.1)
  --port PORT       API/UI server port (default: 8787)
  --no-browser      Do not auto-open browser in Web debug mode
  -h, --help        Show this help
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --desktop) DESKTOP=1; shift ;;
        --skip-ui-build) SKIP_UI_BUILD=1; shift ;;
        --dist-state) USE_DIST_STATE=1; shift ;;
        --no-browser) NO_BROWSER=1; shift ;;
        --state) STATE_ARG="$2"; shift 2 ;;
        --target-dir) TARGET_DIR="$2"; shift 2 ;;
        --bind) BIND_ADDR="$2"; shift 2 ;;
        --port) API_PORT="$2"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "Unknown option: $1" >&2; usage >&2; exit 2 ;;
    esac
done

if [[ -n "${STATE_ARG}" ]]; then
    STATE_DIR="${STATE_ARG}"
elif [[ "${USE_DIST_STATE}" -eq 1 ]]; then
    if [[ "$(uname -s)" == "Darwin" && -d "${REPO_ROOT}/dist/clatterforge-macos-arm64/.conductor" ]]; then
        STATE_DIR="${REPO_ROOT}/dist/clatterforge-macos-arm64/.conductor"
    elif [[ -d "${REPO_ROOT}/dist/clatterforge-linux-x86_64/.conductor" ]]; then
        STATE_DIR="${REPO_ROOT}/dist/clatterforge-linux-x86_64/.conductor"
    else
        STATE_DIR="${REPO_ROOT}/dist/linux-x86_64/.conductor"
    fi
elif [[ -n "${CONDUCTOR_STATE:-}" ]]; then
    STATE_DIR="${CONDUCTOR_STATE}"
else
    STATE_DIR="${REPO_ROOT}/.conductor"
fi
mkdir -p "${STATE_DIR}"
STATE_DIR="$(cd "${STATE_DIR}" && pwd)"

if [[ "${DESKTOP}" -eq 0 && "${SKIP_UI_BUILD}" -eq 0 ]]; then
    if ! command -v dx >/dev/null 2>&1; then
        echo "[ERROR] Dioxus CLI (dx) not found on PATH. Install dioxus-cli or pass --skip-ui-build." >&2
        exit 1
    fi
    echo "[CF // DEBUG] Building debug Web UI bundle (dx build --platform web)..."
    (cd "${GUI_DIR}" && dx build --platform web)
fi

if [[ "${DESKTOP}" -eq 0 && ! -f "${DEBUG_UI_DIR}/index.html" ]]; then
    echo "[ERROR] Debug Web UI assets missing at ${DEBUG_UI_DIR} (run without --skip-ui-build first)." >&2
    exit 1
fi

BACKEND_BIN="${TARGET_DIR}/debug/dev-conductor"
echo "[CF // DEBUG] Building debug backend (${BACKEND_BIN})..."
(cd "${REPO_ROOT}" && cargo build --locked --target-dir "${TARGET_DIR}" --bin dev-conductor)

API_PID=""
cleanup() {
    if [[ -n "${API_PID}" ]] && kill -0 "${API_PID}" 2>/dev/null; then
        kill "${API_PID}" 2>/dev/null || true
        wait "${API_PID}" 2>/dev/null || true
    fi
}
trap cleanup EXIT INT TERM

SERVE_ARGS=(--state "${STATE_DIR}" serve --bind "${BIND_ADDR}" --port "${API_PORT}")
if [[ "${DESKTOP}" -eq 0 ]]; then
    SERVE_ARGS+=(--ui-assets "${DEBUG_UI_DIR}")
fi

echo "[CF // DEBUG] Starting debug API server on ${BIND_ADDR}:${API_PORT} (state: ${STATE_DIR})..."
"${BACKEND_BIN}" "${SERVE_ARGS[@]}" &
API_PID=$!

TOKEN_FILE="${STATE_DIR}/serve-token"
API_READY=0
for _ in $(seq 1 100); do
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
    sleep 0.15
done

TOKEN="$(tr -d '[:space:]' < "${TOKEN_FILE}" 2>/dev/null || true)"
if [[ "${API_READY}" -ne 1 ]] || [[ -z "${TOKEN}" ]] || ! kill -0 "${API_PID}" 2>/dev/null; then
    echo "[ERROR] Debug server failed to become ready on ${BIND_ADDR}:${API_PORT} (port in use?)." >&2
    exit 1
fi

if [[ "${DESKTOP}" -eq 1 ]]; then
    echo "[CF // DEBUG] Launching live-reload Tauri Desktop (cargo tauri dev)..."
    (cd "${GUI_DIR}" && cargo tauri dev)
else
    AUTO_URL="http://127.0.0.1:${API_PORT}/#token=${TOKEN}&base=http://127.0.0.1:${API_PORT}"
    echo ""
    echo "=========================================================="
    echo "  CLATTERFORGE // DEBUG SERVER ACTIVE                     "
    echo "=========================================================="
    echo "  Debug Web UI + API:   http://127.0.0.1:${API_PORT}/"
    echo "  State Directory:      ${STATE_DIR}"
    echo "  UI Assets:            ${DEBUG_UI_DIR}"
    echo "  Bearer Token File:    ${TOKEN_FILE}"
    echo "  Press Ctrl+C to stop the debug server."
    echo "=========================================================="

    if [[ "${NO_BROWSER}" -eq 0 ]]; then
        if [[ "$(uname -s)" == "Darwin" ]] && command -v open >/dev/null 2>&1; then
            open "${AUTO_URL}" >/dev/null 2>&1 || true
        elif command -v xdg-open >/dev/null 2>&1 && [[ -n "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ]]; then
            xdg-open "${AUTO_URL}" >/dev/null 2>&1 || true
        fi
    fi

    wait "${API_PID}"
fi
