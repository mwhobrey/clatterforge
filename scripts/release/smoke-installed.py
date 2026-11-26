"""Smoke-test an installed Clatterforge desktop app: frontend, backend and one UI interaction.

  python scripts/release/smoke-installed.py --app PATH_TO_INSTALLED_DESKTOP_EXECUTABLE
      [--browser PATH_TO_CHROMIUM] [--timeout 90] [--json REPORT.json]

Launches the installed app against a throwaway state folder (CLATTERFORGE_STATE), so it never
touches real state, and checks, in order:

  backend    the app started its bundled backend: health 200, API 200 with the token, 401
             without it.
  frontend   the UI initialized and reached the backend (it shows CONNECTED and a loaded task
             board, which an empty state renders as "No runs yet").
  interact   opening the Verifier Sandbox panel works and the panel loads its settings from the
             backend, so a click reaches the API through the frontend.

Frontend and interaction run inside the app's own WebView where one can be inspected: on
Windows, WebView2 exposes a DevTools port through WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS. Linux
(WebKitGTK) and macOS (WKWebView) cannot be driven this way, so there the same checks run
against the installed UI bundle as served by the installed backend, in headless Chromium given
with --browser, and the report says "served-ui" instead of "native-webview".
On Windows, --browser also enables a fallback for environments whose WebView2 never opens its
DevTools port (GitHub's Windows runners): the report then says "served-ui-fallback" with
"native_inspectable": false, and the native window still needs checking on a real desktop. A missing or
unusable WebView runtime fails the native check: the backend only starts after the window does.

This is not real-provider acceptance and not independent validation; it proves the installed
artifact starts and its UI works. Exits 1 if any check fails. Standard library only.
"""
import argparse
import base64
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

API_PORT = 8787
# The task board has loaded from the backend: an empty board says so, a non-empty one shows its filters.
BOARD_TEXTS = ('No runs yet', 'All (')
CONNECTED_TEXT = 'CONNECTED'
PANEL_TEXT = 'Forge Settings: Verifier Sandbox'


def free_port():
    with socket.socket() as s:
        s.bind(('127.0.0.1', 0))
        return s.getsockname()[1]


def http_status(url, token=None, timeout=3):
    request = urllib.request.Request(url, headers={'Authorization': f'Bearer {token}'} if token else {})
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            return response.status
    except urllib.error.HTTPError as error:
        return error.code
    except OSError:
        return None


class DevTools:
    """A minimal Chrome DevTools Protocol client over a hand-rolled WebSocket."""

    def __init__(self, ws_url):
        rest = ws_url.split('://', 1)[1]
        hostport, path = rest.split('/', 1)
        host, port = hostport.rsplit(':', 1)
        self.sock = socket.create_connection((host, int(port)), timeout=30)
        key = base64.b64encode(os.urandom(16)).decode()
        self.sock.sendall((f'GET /{path} HTTP/1.1\r\nHost: {hostport}\r\nUpgrade: websocket\r\n'
                           f'Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\n'
                           'Sec-WebSocket-Version: 13\r\n\r\n').encode())
        response = b''
        while b'\r\n\r\n' not in response:
            chunk = self.sock.recv(4096)
            if not chunk:
                raise ConnectionError('DevTools closed during handshake')
            response += chunk
        if b' 101 ' not in response.split(b'\r\n', 1)[0]:
            raise ConnectionError('DevTools refused the WebSocket upgrade')
        self.buffer = response.split(b'\r\n\r\n', 1)[1]
        self.next_id = 0

    def _read(self, n):
        while len(self.buffer) < n:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise ConnectionError('DevTools connection closed')
            self.buffer += chunk
        data, self.buffer = self.buffer[:n], self.buffer[n:]
        return data

    def _send(self, text):
        payload = text.encode()
        header = bytearray([0x81])
        if len(payload) < 126:
            header.append(0x80 | len(payload))
        elif len(payload) < 65536:
            header += bytes([0x80 | 126]) + struct.pack('>H', len(payload))
        else:
            header += bytes([0x80 | 127]) + struct.pack('>Q', len(payload))
        mask = os.urandom(4)
        self.sock.sendall(bytes(header) + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(payload)))

    def _receive(self):
        message = b''
        while True:
            first, second = self._read(2)
            length = second & 0x7F
            if length == 126:
                length = struct.unpack('>H', self._read(2))[0]
            elif length == 127:
                length = struct.unpack('>Q', self._read(8))[0]
            message += self._read(length)
            if first & 0x80:
                return json.loads(message)

    def call(self, method, params=None):
        self.next_id += 1
        self._send(json.dumps({'id': self.next_id, 'method': method, 'params': params or {}}))
        while True:
            reply = self._receive()
            if reply.get('id') == self.next_id:
                if 'error' in reply:
                    raise RuntimeError(f"{method}: {reply['error']}")
                return reply.get('result', {})

    def evaluate(self, expression):
        result = self.call('Runtime.evaluate', {'expression': expression, 'awaitPromise': True,
                                                'returnByValue': True})
        return result.get('result', {}).get('value')


def page_target(port, deadline, prefer=None):
    while time.time() < deadline:
        try:
            with urllib.request.urlopen(f'http://127.0.0.1:{port}/json', timeout=2) as response:
                pages = [t for t in json.load(response) if t.get('type') == 'page']
            if prefer:
                pages = [p for p in pages if p.get('url', '').startswith(prefer)] or pages
            if pages:
                return pages[0]['webSocketDebuggerUrl']
        except OSError:
            pass
        time.sleep(0.5)
    return None


def wait_for(check, deadline, interval=0.5):
    value = None
    while time.time() < deadline:
        value = check()
        if value:
            return value
        time.sleep(interval)
    return value


def backend_port(state, deadline):
    """The app uses 8787 unless another instance owns it; then it logs the port it picked."""
    log = state / 'logs' / 'clatterforge.jsonl'

    def from_log():
        if not log.exists():
            return None
        for line in log.read_text(encoding='utf-8', errors='replace').splitlines():
            if '"backend.port_fallback"' in line:
                try:
                    return json.loads(line)['fields']['selected_port']
                except (ValueError, KeyError):
                    pass
        return None

    token_path = state / 'serve-token'
    while time.time() < deadline:
        if token_path.exists():
            token = token_path.read_text(encoding='utf-8').strip()
            for port in filter(None, (from_log(), API_PORT)):
                if http_status(f'http://127.0.0.1:{port}/api/tasks', token) == 200:
                    return port, token
        time.sleep(0.5)
    return None, None


UI_STATE_JS = (
    "(() => { const t = document.body ? document.body.innerText : '';"
    f" return {{ready: document.readyState, connected: t.includes({json.dumps(CONNECTED_TEXT)}),"
    f" board: {json.dumps(list(BOARD_TEXTS))}.some(x => t.includes(x)), title: document.title}}; }})()"
)
PANEL_JS = (
    "(() => { const b = [...document.querySelectorAll('button')]"
    f".find(e => e.textContent.includes({json.dumps(PANEL_TEXT)}));"
    " return b ? b.getAttribute('aria-expanded') : null; })()"
)
CLICK_JS = (
    "(() => { const b = [...document.querySelectorAll('button')]"
    f".find(e => e.textContent.includes({json.dumps(PANEL_TEXT)}));"
    " if (!b) return false; b.click(); return true; })()"
)
LOADED_JS = (
    "(() => { const b = [...document.querySelectorAll('button')]"
    f".find(e => e.textContent.includes({json.dumps(PANEL_TEXT)}));"
    " const panel = b && b.parentElement; const t = panel ? panel.innerText : '';"
    " return t.includes('Preset') || t.includes('Docker'); })()"
)


def ui_checks(devtools, deadline, report):
    state = wait_for(lambda: (lambda s: s if s and s.get('connected') and s.get('board') else None)(
        devtools.evaluate(UI_STATE_JS)), deadline)
    final = devtools.evaluate(UI_STATE_JS) or {}
    report['frontend'] = {'ok': bool(state), 'title': final.get('title'),
                          'connected': final.get('connected'), 'task_board': final.get('board')}
    if not state:
        report['interact'] = {'ok': False, 'detail': 'skipped: frontend did not initialize'}
        return
    before = devtools.evaluate(PANEL_JS)
    clicked = devtools.evaluate(CLICK_JS)
    expanded = wait_for(lambda: devtools.evaluate(PANEL_JS) == 'true', deadline)
    loaded = wait_for(lambda: devtools.evaluate(LOADED_JS), deadline)
    report['interact'] = {'ok': bool(clicked and expanded and loaded), 'expanded_before': before,
                          'expanded_after': devtools.evaluate(PANEL_JS), 'settings_loaded': bool(loaded)}


def windows_webview_diagnostics(process, devtools_port):
    """Why a WebView2 window exposed no page: whether the app and any WebView2 processes are
    running, in which Windows session (0 is the non-interactive service session), and whether
    the DevTools port listens. Best effort; never raises."""
    info = {'app_running': process.poll() is None}
    try:
        with socket.create_connection(('127.0.0.1', devtools_port), timeout=2):
            info['devtools_port_listening'] = True
    except OSError:
        info['devtools_port_listening'] = False
    try:
        rows = subprocess.run(['tasklist', '/FO', 'CSV', '/NH'], capture_output=True, text=True,
                              timeout=20).stdout.splitlines()
        def session_of(image):
            return sorted({row.split('","')[3] for row in rows
                           if row.lower().startswith(f'"{image}"') and row.count('","') >= 4})
        info['app_sessions'] = session_of('clatterforge-desktop.exe')
        info['webview2_sessions'] = session_of('msedgewebview2.exe')
        info['webview2_processes'] = sum(1 for row in rows if row.lower().startswith('"msedgewebview2.exe"'))
    except (OSError, subprocess.SubprocessError) as error:
        info['tasklist_error'] = str(error)
    return info


def stop(process):
    if process.poll() is not None:
        return
    if os.name == 'nt':
        subprocess.run(['taskkill', '/T', '/F', '/PID', str(process.pid)], capture_output=True)
    else:
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
    try:
        process.wait(timeout=15)
    except subprocess.TimeoutExpired:
        process.kill()


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument('--app', required=True, type=Path, help='installed desktop executable')
    parser.add_argument('--browser', type=Path, help='Chromium for the served-UI check (Linux, macOS), '
                        'and on Windows the fallback when the native WebView cannot be inspected')
    parser.add_argument('--timeout', type=int, default=90)
    parser.add_argument('--json', type=Path, help='also write the report here')
    args = parser.parse_args()

    state = Path(tempfile.mkdtemp(prefix='clatterforge-smoke-'))
    # The file name only: a full install path can reveal a user name in shared evidence.
    report = {'app': args.app.name, 'mode': None}
    env = {**os.environ, 'CLATTERFORGE_STATE': str(state)}
    native = os.name == 'nt'
    devtools_port = free_port()
    if native:
        env['WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS'] = f'--remote-debugging-port={devtools_port}'
        # Its own WebView2 profile: a running copy of the app holding the default profile with
        # different browser arguments would otherwise make WebView2 refuse this instance.
        env['WEBVIEW2_USER_DATA_FOLDER'] = str(state / 'webview2')
    deadline = time.time() + args.timeout
    process = subprocess.Popen([str(args.app)], env=env, stdout=subprocess.DEVNULL,
                               stderr=subprocess.DEVNULL, start_new_session=not native)
    browser = None
    try:
        port, token = backend_port(state, deadline)
        if port:
            base = f'http://127.0.0.1:{port}'
            report['backend'] = {
                'ok': http_status(f'{base}/health') == 200 and http_status(f'{base}/api/tasks') == 401,
                'port': port, 'health': http_status(f'{base}/health'),
                'api_with_token': 200, 'api_without_token': http_status(f'{base}/api/tasks')}
        else:
            report['backend'] = {'ok': False, 'detail': 'backend never served the API; if this is a '
                                 'desktop session, check for a WebView runtime error dialog',
                                 'app_running': process.poll() is None}

        def served_ui_checks(mode):
            nonlocal browser
            report['mode'] = mode
            if not port:
                report['frontend'] = {'ok': False, 'detail': 'skipped: no backend to serve the UI'}
                report['interact'] = {'ok': False, 'detail': 'skipped: no backend to serve the UI'}
                return
            browser_port = free_port()
            profile = state / 'browser-profile'  # removed with the throwaway state
            browser = subprocess.Popen(
                [str(args.browser), '--headless=new', '--no-sandbox', '--disable-gpu',
                 f'--remote-debugging-port={browser_port}', f'--user-data-dir={profile}',
                 f'http://127.0.0.1:{port}/#token={token}'],
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
            target = page_target(browser_port, time.time() + args.timeout, prefer=f'http://127.0.0.1:{port}')
            if target:
                ui_checks(DevTools(target), time.time() + args.timeout, report)
            else:
                report['frontend'] = {'ok': False, 'detail': 'browser exposed no page'}
                report['interact'] = {'ok': False, 'detail': 'skipped: no browser page'}

        if native:
            report['mode'] = 'native-webview'
            # With a fallback browser, give the native window a bounded wait so the fallback
            # still has time; without one, the native check is all there is.
            native_deadline = min(deadline, time.time() + 60) if args.browser else deadline
            target = page_target(devtools_port, native_deadline)
            if target:
                report['native_inspectable'] = True
                ui_checks(DevTools(target), deadline, report)
            else:
                diagnostics = windows_webview_diagnostics(process, devtools_port)
                report['native_inspectable'] = False
                report['native_diagnostics'] = diagnostics
                if args.browser:
                    # Some environments (GitHub's Windows runners) never open WebView2's
                    # DevTools port. Fall back to the served UI and say so: this does not show
                    # that the native window renders, which must then be checked on a real
                    # Windows desktop.
                    served_ui_checks('served-ui-fallback')
                else:
                    report['frontend'] = {'ok': False, 'detail': 'the app WebView exposed no page',
                                          'diagnostics': diagnostics}
                    report['interact'] = {'ok': False, 'detail': 'skipped: no WebView page'}
        elif not args.browser:
            report['mode'] = 'served-ui'
            report['frontend'] = {'ok': False, 'detail': 'pass --browser to check the UI on this platform'}
            report['interact'] = {'ok': False, 'detail': 'skipped: no browser'}
        else:
            served_ui_checks('served-ui')
    finally:
        if browser:
            stop(browser)
        stop(process)
        shutil.rmtree(state, ignore_errors=True)

    report['ok'] = all(report.get(k, {}).get('ok') for k in ('backend', 'frontend', 'interact'))
    text = json.dumps(report, indent=2)
    print(text)
    if args.json:
        args.json.write_text(text + '\n', encoding='utf-8')
    return 0 if report['ok'] else 1


if __name__ == '__main__':
    sys.exit(main())
