[CmdletBinding()]
param(
    [ValidateSet('Start','Stop','Status','InstallStartup','RemoveStartup')][string]$Action = 'Status',
    [string]$Binary,
    [string]$StateRoot,
    [string]$Assets,
    [string]$Bind = '127.0.0.1',
    [int]$Port = 8787,
    [int]$UiPort = 8080
)
$ErrorActionPreference = 'Stop'
if (-not $Binary) { $Binary = Join-Path $PSScriptRoot '../target/debug/dev-conductor.exe' }
if (-not $StateRoot) { $StateRoot = Join-Path $PSScriptRoot '../.conductor' }
if (-not $Assets) { $Assets = Join-Path $PSScriptRoot '../.conductor/runtime/ui' }
$StateRoot = [IO.Path]::GetFullPath($StateRoot)
$stopPath = Join-Path $StateRoot 'host.stop'
$receiptPath = Join-Path $StateRoot 'host-runtime.json'
$startup = Join-Path ([Environment]::GetFolderPath('Startup')) 'Dev Conductor.lnk'
switch ($Action) {
    'Status' { if (Test-Path -LiteralPath $receiptPath) { Get-Content -LiteralPath $receiptPath -Raw }; return }
    'Stop' { New-Item -ItemType Directory -Path $StateRoot -Force | Out-Null; Set-Content -LiteralPath $stopPath -Value 'Explicit operator stop'; return }
    'RemoveStartup' { if (Test-Path -LiteralPath $startup) { Remove-Item -LiteralPath $startup }; return }
}
$Binary = (Resolve-Path -LiteralPath $Binary).Path
$Assets = (Resolve-Path -LiteralPath $Assets).Path
if (-not (Test-Path -LiteralPath (Join-Path $Assets 'index.html'))) { throw 'Build and install compiled UI assets first' }
$runner = Join-Path $PSScriptRoot 'run-local-host.ps1'
$pwsh = Join-Path $env:SystemRoot 'System32/WindowsPowerShell/v1.0/powershell.exe'
$arguments = "-NoLogo -NoProfile -WindowStyle Hidden -File `"$runner`" -Binary `"$Binary`" -StateRoot `"$StateRoot`" -Assets `"$Assets`" -Bind `"$Bind`" -Port $Port -UiPort $UiPort"
if ($Action -eq 'InstallStartup') {
    # Per-user logon preserves the real coding provider's user profile and credentials.
    # No LocalSystem account, admin rights, password storage or network widening.
    if (Test-Path -LiteralPath $startup) { throw "Startup shortcut already exists: $startup" }
    $shell = New-Object -ComObject WScript.Shell
    $shortcut = $shell.CreateShortcut($startup)
    $shortcut.TargetPath = $pwsh
    $shortcut.Arguments = $arguments
    $shortcut.WindowStyle = 7
    $shortcut.Description = 'Dev Conductor managed local backend and compiled UI'
    $shortcut.Save()
    Write-Output "Installed current-user logon startup: $startup"
    return
}
New-Item -ItemType Directory -Path $StateRoot -Force | Out-Null
if (Test-Path -LiteralPath $stopPath) { Remove-Item -LiteralPath $stopPath }
$child = Start-Process -FilePath $pwsh -ArgumentList $arguments -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $StateRoot 'host-supervisor.stdout.log') -RedirectStandardError (Join-Path $StateRoot 'host-supervisor.stderr.log')
Write-Output "Managed host launched: $($child.Id)"
