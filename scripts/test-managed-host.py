"""Exercise host supervision against disposable state and loopback ports only."""
import json
import pathlib
import socket
import os
import signal
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def wait_for(check, timeout=30):
    started = time.monotonic()
    while time.monotonic() - started < timeout:
        try:
            result = check()
            if result:
                return result
        except (OSError, ValueError, urllib.error.URLError):
            pass
        time.sleep(0.2)
    raise AssertionError("fixture did not reach expected state")


root = pathlib.Path(__file__).resolve().parents[1]
binary = pathlib.Path(sys.argv[1]).resolve()
WINDOWS = os.name == "nt"
fixture = pathlib.Path(tempfile.mkdtemp(prefix="conductor host fixture "))
state = fixture / "state with spaces"
assets = fixture / "assets with spaces"
assets.mkdir()
(assets / "index.html").write_text("isolated compiled GUI fixture", encoding="utf-8")
(fixture / "private.txt").write_text("must never be served", encoding="utf-8")
port, ui_port = free_port(), free_port()
if WINDOWS:
    pwsh = pathlib.Path(os.environ["SystemRoot"]) / "System32/WindowsPowerShell/v1.0/powershell.exe"
    args = [str(pwsh), "-NoProfile", "-File", str(root / "scripts/run-local-host.ps1"),
            "-Binary", str(binary), "-StateRoot", str(state), "-Assets", str(assets),
            "-Port", str(port), "-UiPort", str(ui_port)]
else:
    args = ["/bin/sh", str(root / "scripts/run-local-host.sh"), "--binary", str(binary),
            "--state-root", str(state), "--assets", str(assets),
            "--port", str(port), "--ui-port", str(ui_port)]


def start():
    return subprocess.Popen(args, stdout=subprocess.DEVNULL,
                            stderr=open(fixture / f"supervisor-{time.time_ns()}.log", "w"),
                            creationflags=subprocess.CREATE_NO_WINDOW if WINDOWS else 0)


def stop_process(pid):
    """Each target is a fresh process this isolated supervisor just created."""
    if WINDOWS:
        subprocess.run([str(pwsh), "-NoProfile", "-Command", f"Stop-Process -Id {pid} -ErrorAction Stop"],
                       check=True, creationflags=subprocess.CREATE_NO_WINDOW)
    else:
        os.kill(pid, signal.SIGTERM)


def receipt():
    # The supervisor swaps the receipt every 500 ms; a read landing on the swap can see it
    # briefly missing, delete-pending (PermissionError) or partially visible. Retry briefly.
    deadline = time.monotonic() + 5
    while True:
        try:
            return json.loads((state / "host-runtime.json").read_text(encoding="utf-8-sig"))
        except (OSError, ValueError):
            if time.monotonic() >= deadline:
                raise
            time.sleep(0.05)


def complete_receipt():
    """The receipt once it records both roles, else None (wait_for retries)."""
    current = receipt()
    return current if {"backend", "ui"} <= set(current["roles"]) else None


def get(path="/health", ui=False, host=None):
    headers = {"Host": host} if host else {}
    with urllib.request.urlopen(urllib.request.Request(
            f"http://127.0.0.1:{ui_port if ui else port}{path}", headers=headers), timeout=2) as response:
        return response.read()


runner = start()
try:
    wait_for(lambda: get())
    wait_for(lambda: get("/", ui=True) == b"isolated compiled GUI fixture")
    # Both services can answer before the supervisor's first receipt lands: Windows
    # PowerShell is slow to reach its first write on a cold CI runner. Wait for a receipt
    # that records both roles instead of reading once.
    before = wait_for(complete_receipt)
    token = (state / "serve-token").read_bytes()
    duplicate = subprocess.run([str(binary), "--state", str(state), "serve", "--port", str(port)],
                               capture_output=True, timeout=10)
    assert duplicate.returncode != 0
    assert (state / "serve-token").read_bytes() == token, "duplicate bind rotated live authentication"
    for path, host in [("/", "evil.invalid"), ("/%2e%2e/private.txt", None)]:
        try:
            result = get(path, ui=True, host=host)
            assert b"must never be served" not in result
            raise AssertionError("unsafe static request accepted")
        except urllib.error.HTTPError as error:
            assert error.code in (400, 404)
    other = start()
    assert other.wait(timeout=10) != 0, "duplicate supervisor must refuse"
    assert receipt()["roles"]["backend"]["pid"] == before["roles"]["backend"]["pid"]
    for name in ("backend", "ui"):
        old_pid = receipt()["roles"][name]["pid"]
        stop_process(old_pid)
        wait_for(lambda: receipt()["roles"].get(name, {}).get("pid", old_pid) != old_pid)
        wait_for(lambda: get(ui=name == "ui"))
    stable = receipt()
    stable_token = (state / "serve-token").read_bytes()
    runner.kill()
    runner.wait(timeout=10)
    runner = start()
    wait_for(lambda: receipt()["supervisor_pid"] == runner.pid)
    assert receipt()["roles"]["backend"]["pid"] == stable["roles"]["backend"]["pid"]
    assert receipt()["roles"]["ui"]["pid"] == stable["roles"]["ui"]["pid"]
    assert (state / "serve-token").read_bytes() == stable_token, "adoption must preserve healthy service token"
    print(json.dumps({"fixture": str(fixture), "duplicate_bind_preserves_token": True,
                      "static_scope_and_host_checked": True, "duplicate_supervisor_refused": True,
                      "backend_and_ui_restart": True, "crashed_supervisor_adopts_exact_children": True}))
finally:
    state.mkdir(exist_ok=True)
    (state / "host.stop").write_text("fixture cleanup", encoding="utf-8")
    runner.wait(timeout=15)
    for check_port in (port, ui_port):
        with socket.socket() as sock:
            assert sock.connect_ex(("127.0.0.1", check_port)) != 0, "fixture listener survived stop"
