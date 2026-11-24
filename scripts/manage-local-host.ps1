[CmdletBinding()]
param(
    [ValidateSet('Start','Stop','Status','InstallStartup','RemoveStartup')][string]$Action = 'Status',
    [string]$Binary,
    [string]$StateRoot,
    [string]$Assets,
    [string]$Bind = '127.0.0.1',
    [int]$Port = 8787,
    [int]$UiPort = 8080,
    # Plain-HTTP LAN binds are refused without this; prefer loopback plus Tailscale Serve.
    [switch]$AllowInsecureLan
)
$ErrorActionPreference = 'Stop'
# State directory rule shared with the binary: `.clatterforge`, unless only a pre-rename
# `.conductor` exists, which keeps being used in place.
function Resolve-StateDir([string]$Base) {
    $current = Join-Path $Base '.clatterforge'
    $legacy = Join-Path $Base '.conductor'
    if (-not (Test-Path -LiteralPath $current) -and (Test-Path -LiteralPath $legacy -PathType Container)) { return $legacy }
    return $current
}
if (-not $Binary) {
    $Binary = Join-Path $PSScriptRoot '../target/debug/clatterforge.exe'
    $legacyBinary = Join-Path $PSScriptRoot '../target/debug/dev-conductor.exe'
    if (-not (Test-Path -LiteralPath $Binary) -and (Test-Path -LiteralPath $legacyBinary)) { $Binary = $legacyBinary }
}
if (-not $StateRoot) { $StateRoot = Resolve-StateDir (Join-Path $PSScriptRoot '..') }
if (-not $Assets) { $Assets = Join-Path $StateRoot 'runtime/ui' }
$StateRoot = [IO.Path]::GetFullPath($StateRoot)
$stopPath = Join-Path $StateRoot 'host.stop'
$receiptPath = Join-Path $StateRoot 'host-runtime.json'
$startupDir = [Environment]::GetFolderPath('Startup')
$startup = Join-Path $startupDir 'Clatterforge.lnk'
# Shortcut name used before the rename; recognised so it is never duplicated or left behind.
$legacyStartup = Join-Path $startupDir 'Dev Conductor.lnk'
switch ($Action) {
    'Status' { if (Test-Path -LiteralPath $receiptPath) { Get-Content -LiteralPath $receiptPath -Raw }; return }
    'Stop' { New-Item -ItemType Directory -Path $StateRoot -Force | Out-Null; Set-Content -LiteralPath $stopPath -Value 'Explicit operator stop'; return }
    'RemoveStartup' { foreach ($link in @($startup, $legacyStartup)) { if (Test-Path -LiteralPath $link) { Remove-Item -LiteralPath $link } }; return }
}
$Binary = (Resolve-Path -LiteralPath $Binary).Path
$Assets = (Resolve-Path -LiteralPath $Assets).Path
if (-not (Test-Path -LiteralPath (Join-Path $Assets 'index.html'))) { throw 'Build and install compiled UI assets first' }
$runner = Join-Path $PSScriptRoot 'run-local-host.ps1'
$pwsh = Join-Path $env:SystemRoot 'System32/WindowsPowerShell/v1.0/powershell.exe'
$arguments = "-NoLogo -NoProfile -WindowStyle Hidden -File `"$runner`" -Binary `"$Binary`" -StateRoot `"$StateRoot`" -Assets `"$Assets`" -Bind `"$Bind`" -Port $Port -UiPort $UiPort"
if ($AllowInsecureLan) { $arguments += ' -AllowInsecureLan' }
if ($Action -eq 'InstallStartup') {
    # Per-user logon preserves the real coding provider's user profile and credentials.
    # No LocalSystem account, admin rights, password storage or network widening.
    foreach ($link in @($startup, $legacyStartup)) { if (Test-Path -LiteralPath $link) { throw "Startup shortcut already exists: $link" } }
    $shell = New-Object -ComObject WScript.Shell
    $shortcut = $shell.CreateShortcut($startup)
    $shortcut.TargetPath = $pwsh
    $shortcut.Arguments = $arguments
    $shortcut.WindowStyle = 7
    $shortcut.Description = 'Clatterforge managed local backend and compiled UI'
    $shortcut.Save()
    Write-Output "Installed current-user logon startup: $startup"
    return
}
New-Item -ItemType Directory -Path $StateRoot -Force | Out-Null
if (Test-Path -LiteralPath $stopPath) { Remove-Item -LiteralPath $stopPath }
$child = Start-Process -FilePath $pwsh -ArgumentList $arguments -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $StateRoot 'host-supervisor.stdout.log') -RedirectStandardError (Join-Path $StateRoot 'host-supervisor.stderr.log')
Write-Output "Managed host launched: $($child.Id)"
