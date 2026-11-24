#!/usr/bin/env bash
set -euo pipefail

echo "=========================================================="
echo "  CLATTERFORGE (CF // SYSTEMS) — Coworker Testing Setup   "
echo "  CONTROL THE CHAOS.                                      "
echo "=========================================================="

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN_PATH=""
for candidate in "${SCRIPT_DIR}/clatterforge" "${SCRIPT_DIR}/dev-conductor"     "${SCRIPT_DIR}/dist/linux-x86_64/clatterforge" "${SCRIPT_DIR}/dist/linux-x86_64/dev-conductor"; do
    if [[ -f "${candidate}" ]]; then BIN_PATH="${candidate}"; break; fi
done

if [[ -z "${BIN_PATH}" ]]; then
    echo "[ERROR] clatterforge / dev-conductor binary not found in ${SCRIPT_DIR}"
    exit 1
fi

chmod +x "${BIN_PATH}"

# 1. Install both `clatterforge` and `dev-conductor` to ~/.local/bin atomically (`O12`)
# so updating while a binary is running never fails with `Text file busy` (ETXTBSY).
TARGET_DIR="${HOME}/.local/bin"
SHARE_DIR="${HOME}/.local/share/clatterforge"
mkdir -p "${TARGET_DIR}" "${SHARE_DIR}"
for bin_name in dev-conductor clatterforge; do
    tmp_dest="${TARGET_DIR}/.${bin_name}.tmp.$$"
    cp "${BIN_PATH}" "${tmp_dest}"
    chmod +x "${tmp_dest}"
    mv -f "${tmp_dest}" "${TARGET_DIR}/${bin_name}"
done
echo "[✓] Installed clatterforge & dev-conductor to ${TARGET_DIR}"

# 2. Install compiled Web GUI bundle + `clatterforge-gui` launcher if present
UI_SRC="${SCRIPT_DIR}/ui"
if [[ ! -d "${UI_SRC}" && -d "${SCRIPT_DIR}/dist/linux-x86_64/ui" ]]; then
    UI_SRC="${SCRIPT_DIR}/dist/linux-x86_64/ui"
fi
if [[ -d "${UI_SRC}" ]]; then
    rm -rf "${SHARE_DIR}/ui"
    cp -r "${UI_SRC}" "${SHARE_DIR}/ui"
    echo "[✓] Installed Clatterforge Web GUI bundle to ${SHARE_DIR}/ui"
fi

GUI_LAUNCHER_INSTALLED=false
if [[ -f "${SCRIPT_DIR}/start-gui.sh" ]]; then
    tmp_gui="${TARGET_DIR}/.clatterforge-gui.tmp.$$"
    cp "${SCRIPT_DIR}/start-gui.sh" "${tmp_gui}"
    chmod +x "${tmp_gui}" "${SCRIPT_DIR}/start-gui.sh"
    mv -f "${tmp_gui}" "${TARGET_DIR}/clatterforge-gui"
    GUI_LAUNCHER_INSTALLED=true
    echo "[✓] Installed clatterforge-gui launcher to ${TARGET_DIR}/clatterforge-gui"
fi

if [[ ":$PATH:" != *":${TARGET_DIR}:"* ]]; then
    echo "    Notice: ${TARGET_DIR} is not currently in your PATH."
    echo "    Add this to your ~/.bashrc or ~/.zshrc:"
    echo "      export PATH=\"\$HOME/.local/bin:\$PATH\""
    export PATH="${TARGET_DIR}:${PATH}"
fi

# 3. Check Git
if command -v git >/dev/null 2>&1; then
    echo "[✓] Git detected: $(git --version)"
else
    echo "[!] Git is not installed. Please install git (e.g. sudo apt install git)."
fi

# 4. Routing Engine Status (Zero-Footprint Built-in Router + Optional Ollama)
echo "[✓] Built-in Zero-Footprint Deterministic Router (builtin:router-v1) active (0 MB VRAM required)."
if command -v ollama >/dev/null 2>&1; then
    if curl -s http://127.0.0.1:11434/api/tags >/dev/null 2>&1; then
        echo "[✓] Optional Ollama daemon also detected on http://127.0.0.1:11434"
    else
        echo "[i] Ollama is installed but offline (optional; builtin:router-v1 will be used by default)."
    fi
fi

# 5. Check Coding Worker CLIs (Cursor / Codex)
CURSOR_FOUND=false
CODEX_FOUND=false

if command -v cursor-agent >/dev/null 2>&1 || [[ -d "${HOME}/.cursor-agent" ]] || [[ -d "${HOME}/.local/share/cursor-agent" ]]; then
    CURSOR_FOUND=true
    echo "[✓] Cursor Agent CLI detected (supports single-CLI Fast / Standard / Frontier tier routing)."
fi

if command -v codex >/dev/null 2>&1 || [[ -d "${HOME}/.codex" ]]; then
    CODEX_FOUND=true
    echo "[✓] OpenAI Codex CLI detected."
fi

if [[ "${CURSOR_FOUND}" = false && "${CODEX_FOUND}" = false ]]; then
    echo "[!] Neither Cursor CLI nor Codex CLI was detected."
    echo "    Clatterforge requires at least one coding CLI (Cursor or Codex)."
    echo "    - Cursor: ensure node is on PATH and ~/.cursor-agent or ~/.local/share/cursor-agent exists."
    echo "    - Codex: install codex CLI and run 'codex login'."
fi

echo "=========================================================="
echo "SYSTEM NOMINAL. Next steps:"
echo ""
if [[ "${GUI_LAUNCHER_INSTALLED}" = true ]]; then
    echo "1. Launch the Clatterforge Instrumentation GUI (API + Web UI):"
    echo "   ./start-gui.sh   (or: clatterforge-gui)"
    echo ""
    echo "2. Or onboard your project repository from the CLI (0 MB router):"
else
    echo "1. Onboard your project repository from the CLI (0 MB router):"
fi
echo "   clatterforge onboard --repo /path/to/your/repo --model builtin:router-v1"
echo "=========================================================="
