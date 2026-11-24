# Managed local host

The backend and compiled GUI run as the current Windows user, supervised independently of the desktop window. The GUI listener serves only compiled public assets on loopback port 8080; authenticated task APIs remain on loopback 8787. Remote transport is separately administered; see [remote access](../REMOTE_ACCESS.md).

Build native code with `cargo build --locked`; build public GUI assets with `dx build --platform web` from `gui/`. Install the resulting executable and public assets into `.clatterforge/runtime/` before starting. The deployed copies remain stable while source work continues.

```powershell
scripts/manage-local-host.ps1 -Action Start -Binary .clatterforge/runtime/clatterforge.exe -Assets .clatterforge/runtime/ui
scripts/manage-local-host.ps1 -Action InstallStartup -Binary .clatterforge/runtime/clatterforge.exe -Assets .clatterforge/runtime/ui
scripts/manage-local-host.ps1 -Action Status
scripts/manage-local-host.ps1 -Action Stop
```

`InstallStartup` writes one `Clatterforge.lnk` in the current user's Startup folder. It refuses to overwrite an existing shortcut. `RemoveStartup` removes only that named shortcut and does not stop current processes. Start clears an explicit `host.stop` request; Stop leaves that request in place so an intentional stop survives a later logon. The launcher uses built-in Windows PowerShell, stores no passwords, and does not require elevation. Automatic startup begins at user sign-in, not before login.

The private state root retains `host-runtime.json`, `host-logs/`, supervisor logs and the explicit stop request. Runtime metadata includes process creation identities and roles, not credentials. A crashed watchdog can adopt only matching saved child identities. A duplicate watchdog refuses; a duplicate backend bind preserves the active token. Unexpected child exits restart with bounded backoff. Closing the desktop UI does not stop a backend it reused.

Backend restart rotates its local token; the refreshed native wrapper can retrieve the current token and reconnect to that same endpoint. Browser-only sessions must reconnect using the new local token. Existing enrollment credentials are unaffected.

An interrupted coding task is never replayed on restart. Inspect its owner and retained receipts, then use existing reconciliation/review operations. Restarting listeners is separate from resuming work. Do not stop a busy host merely to refresh its source.

Fixture verification (temporary state and loopback ports, no provider credits):

```powershell
python scripts/test-managed-host.py target/debug/clatterforge.exe
```

For stage evidence use `board-show <task> --json`, MCP `conductor_get_task`, or the GUI's **Workflow routing evidence** disclosure. It reports host handler/stage transitions and owned process launches. Missing legacy events are not reconstructed. Initial assignment approval and final result review are separate decision stages; a retained partial diff is unverified evidence.
