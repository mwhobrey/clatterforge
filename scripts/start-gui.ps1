param(
    [switch]$Web,
    [string]$Bind = '127.0.0.1',
    [int]$ApiPort = 8787,
    [int]$UiPort = 8080
)

$ErrorActionPreference = "Stop"
$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path

# Prefer Native Desktop GUI unless -Web is requested
$DesktopBin = Join-Path $ScriptDir "clatterforge-desktop.exe"
if (-not $Web -and (Test-Path $DesktopBin)) {
    Write-Host "[CF // SYSTEMS] Launching Clatterforge Native Desktop GUI..." -ForegroundColor Cyan
    Start-Process -FilePath $DesktopBin -WorkingDirectory $ScriptDir
    exit 0
}

# Resolve CLI binary
$CliBin = Join-Path $ScriptDir "clatterforge.exe"
if (-not (Test-Path $CliBin)) {
    $CliBin = Join-Path $ScriptDir "dev-conductor.exe"
}
if (-not (Test-Path $CliBin)) {
    $CliBin = (Get-Command clatterforge -ErrorAction Stop).Source
}

# Resolve UI assets directory
$UiDir = Join-Path $ScriptDir "ui"
if (-not (Test-Path $UiDir)) {
    $UiDir = Join-Path $env:LOCALAPPDATA "Clatterforge\ui"
}
if (-not (Test-Path $UiDir)) {
    Write-Error "Compiled Web GUI assets not found at $UiDir"
    exit 1
}

# Same rule as the binary: `.clatterforge`, unless only a pre-rename `.conductor` exists.
$StateBase = if (Test-Path (Join-Path $ScriptDir "clatterforge-desktop.exe")) { $ScriptDir } else { (Get-Location).Path }
$DefaultState = Join-Path $StateBase ".clatterforge"
if (-not (Test-Path -LiteralPath $DefaultState) -and (Test-Path -LiteralPath (Join-Path $StateBase ".conductor") -PathType Container)) {
    $DefaultState = Join-Path $StateBase ".conductor"
}
$StateDir = if ($env:CONDUCTOR_STATE) { $env:CONDUCTOR_STATE } else { $DefaultState }
New-Item -ItemType Directory -Force -Path $StateDir | Out-Null
$TokenPath = Join-Path $StateDir "serve-token"

Write-Host "[CF // SYSTEMS] Starting Clatterforge API + Unified Web UI on ${Bind}:$ApiPort..." -ForegroundColor Cyan
$ApiProc = Start-Process -FilePath $CliBin -ArgumentList @("--state", $StateDir, "serve", "--bind", $Bind, "--port", "$ApiPort", "--ui-assets", $UiDir) -PassThru -WindowStyle Hidden

$Deadline = (Get-Date).AddSeconds(10)
$ApiReady = $false
while (-not $ApiProc.HasExited -and (Get-Date) -lt $Deadline) {
    if (Test-Path $TokenPath) {
        try {
            $Resp = Invoke-WebRequest -UseBasicParsing -Uri "http://127.0.0.1:$ApiPort/health" -TimeoutSec 1 -ErrorAction Stop
            if ($Resp.StatusCode -eq 200) {
                $ApiReady = $true
                break
            }
        } catch {}
    }
    Start-Sleep -Milliseconds 150
}
$Token = if (Test-Path $TokenPath) { "$(Get-Content $TokenPath -Raw)".Trim() } else { "" }
if (-not $ApiReady -or -not $Token -or $ApiProc.HasExited) {
    if (-not $ApiProc.HasExited) { Stop-Process -Id $ApiProc.Id -Force -ErrorAction SilentlyContinue }
    Write-Error "Clatterforge API did not start on ${Bind}:$ApiPort (port in use?). Pass -ApiPort <port> or stop the other server."
    exit 1
}

Write-Host "[CF // SYSTEMS] Starting Clatterforge Standalone UI Server on ${Bind}:$UiPort..." -ForegroundColor Cyan
$UiProc = Start-Process -FilePath $CliBin -ArgumentList @("--state", $StateDir, "ui-serve", "--assets", $UiDir, "--bind", $Bind, "--port", "$UiPort") -PassThru -WindowStyle Hidden

$AutoConnectUrl = "http://127.0.0.1:$ApiPort/#token=$Token&base=http://127.0.0.1:$ApiPort"
Write-Host ""
Write-Host "==========================================================" -ForegroundColor White
Write-Host "  CLATTERFORGE // WEB INSTRUMENTATION GUI ACTIVE          " -ForegroundColor DarkYellow
Write-Host "==========================================================" -ForegroundColor White
Write-Host "  Unified Web UI + API:   http://127.0.0.1:$ApiPort/" -ForegroundColor Cyan
Write-Host "  Standalone UI Mirror:   http://127.0.0.1:$UiPort/" -ForegroundColor Gray
Write-Host "  Bind Address:           $Bind" -ForegroundColor Gray
Write-Host "  Bearer Token File:      $TokenPath" -ForegroundColor Yellow
Write-Host "  Press Ctrl+C to stop both servers." -ForegroundColor Gray
Write-Host "==========================================================" -ForegroundColor White

Start-Process $AutoConnectUrl
try {
    Wait-Process -Id $ApiProc.Id, $UiProc.Id
} finally {
    if (-not $ApiProc.HasExited) { Stop-Process -Id $ApiProc.Id -Force -ErrorAction SilentlyContinue }
    if (-not $UiProc.HasExited) { Stop-Process -Id $UiProc.Id -Force -ErrorAction SilentlyContinue }
}
