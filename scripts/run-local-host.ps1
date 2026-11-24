[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Binary,
    [Parameter(Mandatory)][string]$StateRoot,
    [Parameter(Mandatory)][string]$Assets,
    [string]$Bind = '127.0.0.1',
    [int]$Port = 8787,
    [int]$UiPort = 8080,
    # The services are plain HTTP; a non-loopback bind is refused without this opt-in.
    # Prefer the loopback default plus Tailscale Serve (scripts/expose-remote.ps1).
    [switch]$AllowInsecureLan
)
$ErrorActionPreference = 'Stop'
$Binary = (Resolve-Path -LiteralPath $Binary).Path
$Assets = (Resolve-Path -LiteralPath $Assets).Path
$StateRoot = [IO.Path]::GetFullPath($StateRoot)
New-Item -ItemType Directory -Path $StateRoot -Force | Out-Null
if (-not (Test-Path -LiteralPath (Join-Path $Assets 'index.html'))) { throw 'Compiled UI index.html is required' }
if ($Port -eq $UiPort) { throw 'Backend and UI require distinct ports' }
$hasher = [Security.Cryptography.SHA256]::Create()
$hash = [BitConverter]::ToString($hasher.ComputeHash([Text.Encoding]::UTF8.GetBytes($StateRoot.ToLowerInvariant()))).Replace('-','')
$hasher.Dispose()
$created = $false
# Pre-rename mutex name kept on purpose: an old and a new supervisor must still exclude each
# other on the same state root.
$mutex = [Threading.Mutex]::new($true, "Local\DevConductor-$hash", [ref]$created)
if (-not $created) { $mutex.Dispose(); throw 'A managed host already owns this state root' }
$stopPath = Join-Path $StateRoot 'host.stop'
$receiptPath = Join-Path $StateRoot 'host-runtime.json'
$logRoot = Join-Path $StateRoot 'host-logs'
New-Item -ItemType Directory -Path $logRoot -Force | Out-Null
$lanArgs = @(if ($AllowInsecureLan) { '--allow-insecure-lan' })
$roles = @{
    backend = @{ Arguments = @('--state', $StateRoot, 'serve', '--bind', $Bind, '--port', "$Port", '--ui-assets', $Assets) + $lanArgs; Process = $null; Restarts = 0; RetryAt = [DateTime]::MinValue }
    ui = @{ Arguments = @('--state', $StateRoot, 'ui-serve', '--assets', $Assets, '--bind', $Bind, '--port', "$UiPort") + $lanArgs; Process = $null; Restarts = 0; RetryAt = [DateTime]::MinValue }
}
function Quote-Argument([string]$Value) {
    if ($Value.Contains('"') -or $Value.EndsWith('\')) { throw 'Managed host arguments must not contain quotes or trailing backslashes' }
    '"' + $Value + '"'
}

try {
    # A crashed watchdog can leave healthy children. Adopt only the exact previous
    # PID/creation-time/executable identities saved for this configuration.
    if (Test-Path -LiteralPath $receiptPath) {
        $prior = Get-Content -LiteralPath $receiptPath -Raw | ConvertFrom-Json
        $priorBind = if ($prior.PSObject.Properties.Name -contains 'bind' -and $prior.bind) { $prior.bind } else { '127.0.0.1' }
        if ($prior.binary -eq $Binary -and $prior.assets -eq $Assets -and $priorBind -eq $Bind -and $prior.port -eq $Port -and $prior.ui_port -eq $UiPort) {
            foreach ($name in $roles.Keys) {
                $saved = $prior.roles.$name
                if ($saved) {
                    $child = Get-Process -Id $saved.pid -ErrorAction SilentlyContinue
                    if ($child -and $child.Path -eq $Binary -and $child.StartTime.ToUniversalTime().Ticks.ToString() -eq $saved.started_ticks) { $roles[$name].Process = $child }
                }
            }
        }
    }
    while (-not (Test-Path -LiteralPath $stopPath)) {
        $records = @{}
        foreach ($name in $roles.Keys) {
            $role = $roles[$name]
            if ($role.Process -and $role.Process.HasExited) { $role.Process.Dispose(); $role.Process = $null }
            if (-not $role.Process -and [DateTime]::UtcNow -ge $role.RetryAt) {
                $out = Join-Path $logRoot "$name.stdout.log"
                $err = Join-Path $logRoot "$name.stderr.log"
                foreach ($path in @($out, $err)) { if (Test-Path -LiteralPath $path) { Copy-Item -LiteralPath $path -Destination "$path.previous" -Force } }
                $arguments = ($role.Arguments | ForEach-Object { Quote-Argument $_ }) -join ' '
                $role.Process = Start-Process -FilePath $Binary -ArgumentList $arguments -WorkingDirectory $StateRoot -WindowStyle Hidden -PassThru -RedirectStandardOutput $out -RedirectStandardError $err
                $role.Restarts++
                $role.RetryAt = [DateTime]::UtcNow.AddSeconds([Math]::Min(30, [Math]::Pow(2, [Math]::Min($role.Restarts, 5))))
            }
            if ($role.Process -and -not $role.Process.HasExited) {
                $records[$name] = @{ pid = $role.Process.Id; started_ticks = $role.Process.StartTime.ToUniversalTime().Ticks.ToString(); launches = $role.Restarts }
            }
        }
        $receipt = @{ supervisor_pid = $PID; binary = $Binary; assets = $Assets; state = $StateRoot; bind = $Bind; port = $Port; ui_port = $UiPort; updated_at = [DateTime]::UtcNow.ToString('o'); roles = $records }
        $receipt | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath "$receiptPath.part" -Encoding UTF8
        # Move-Item -Force deletes the destination before moving, so readers could find no
        # receipt at all for a moment. Replace swaps it in one step once it exists. A reader
        # holding the file open can make the swap fail; keep the old receipt and retry on the
        # next pass rather than letting the supervisor loop die.
        try {
            if (Test-Path -LiteralPath $receiptPath) {
                # [NullString]: PowerShell 5.1 turns $null into "" for a string parameter.
                [IO.File]::Replace("$receiptPath.part", $receiptPath, [NullString]::Value)
            } else {
                [IO.File]::Move("$receiptPath.part", $receiptPath)
            }
        } catch {
            Write-Warning "host receipt update deferred: $($_.Exception.Message)"
        }
        Start-Sleep -Milliseconds 500
    }
} finally {
    foreach ($role in $roles.Values) {
        if ($role.Process -and -not $role.Process.HasExited) {
            # Process objects retain the original creation identity; never kill a
            # reconstructed PID supplied by an external task or remote caller.
            $role.Process.Kill()
            $role.Process.WaitForExit(10000) | Out-Null
        }
    }
    $mutex.ReleaseMutex()
    $mutex.Dispose()
}
