param(
    [switch]$Desktop,
    [switch]$SkipUiBuild,
    [switch]$UseDistState,
    [switch]$NoBrowser,
    [string]$State = '',
    [string]$TargetDir = '',
    [string]$Bind = '127.0.0.1',
    [int]$ApiPort = 8787
)

$ErrorActionPreference = 'Stop'
$RepoRoot = Split-Path -Parent $PSScriptRoot
$GuiDir = Join-Path $RepoRoot 'gui'
$DebugUiDir = Join-Path $GuiDir 'target\dx\conductor-gui\debug\web\public'

if ($State) {
    $StateDir = $State
} elseif ($UseDistState) {
    $StateDir = Join-Path $RepoRoot 'dist\clatterforge-windows-x86_64\.conductor'
} elseif ($env:CONDUCTOR_STATE) {
    $StateDir = $env:CONDUCTOR_STATE
} else {
    $StateDir = Join-Path $RepoRoot '.conductor'
}
$StateDir = [IO.Path]::GetFullPath($StateDir)
New-Item -ItemType Directory -Force -Path $StateDir | Out-Null

$EffectiveTarget = if ($TargetDir) {
    $TargetDir
} elseif ($env:CARGO_TARGET_DIR) {
    $env:CARGO_TARGET_DIR
} else {
    Join-Path $RepoRoot 'target'
}
$EffectiveTarget = [IO.Path]::GetFullPath($EffectiveTarget)
$BackendBin = Join-Path $EffectiveTarget 'debug\dev-conductor.exe'

if (-not $Desktop -and -not $SkipUiBuild) {
    if (-not (Get-Command dx -ErrorAction SilentlyContinue)) {
        Write-Error 'Dioxus CLI (dx) not found on PATH. Install dioxus-cli or pass -SkipUiBuild.'
        exit 1
    }
    Write-Host '[CF // DEBUG] Building debug Web UI bundle (dx build --platform web)...' -ForegroundColor Cyan
    Push-Location $GuiDir
    try {
        & dx build --platform web
        if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    } finally {
        Pop-Location
    }
}

if (-not $Desktop -and -not (Test-Path (Join-Path $DebugUiDir 'index.html'))) {
    Write-Error "Debug Web UI assets missing at $DebugUiDir (run without -SkipUiBuild first)."
    exit 1
}

Write-Host "[CF // DEBUG] Building debug backend ($BackendBin)..." -ForegroundColor Cyan
Push-Location $RepoRoot
try {
    & cargo build --locked --target-dir $EffectiveTarget --bin dev-conductor
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
} finally {
    Pop-Location
}

$ServeArgs = @('--state', $StateDir, 'serve', '--bind', $Bind, '--port', "$ApiPort")
if (-not $Desktop) {
    $ServeArgs += @('--ui-assets', $DebugUiDir)
}

Write-Host "[CF // DEBUG] Starting debug API server on ${Bind}:$ApiPort (state: $StateDir)..." -ForegroundColor Cyan
$ApiProc = Start-Process -FilePath $BackendBin -ArgumentList $ServeArgs -WorkingDirectory $RepoRoot -PassThru -NoNewWindow

$TokenPath = Join-Path $StateDir 'serve-token'
try {
    $Deadline = (Get-Date).AddSeconds(20)
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
    $Token = if (Test-Path $TokenPath) { "$(Get-Content $TokenPath -Raw)".Trim() } else { '' }
    if (-not $ApiReady -or -not $Token -or $ApiProc.HasExited) {
        Write-Error "Debug server failed to become ready on ${Bind}:$ApiPort (port in use or binary exited?)."
        exit 1
    }

    if ($Desktop) {
        Write-Host '[CF // DEBUG] Launching live-reload Tauri Desktop (cargo tauri dev)...' -ForegroundColor Cyan
        Push-Location $GuiDir
        try {
            & cargo tauri dev
            exit $LASTEXITCODE
        } finally {
            Pop-Location
        }
    } else {
        $AutoConnectUrl = "http://127.0.0.1:$ApiPort/#token=$Token&base=http://127.0.0.1:$ApiPort"
        Write-Host ''
        Write-Host '==========================================================' -ForegroundColor White
        Write-Host '  CLATTERFORGE // DEBUG SERVER ACTIVE                     ' -ForegroundColor DarkYellow
        Write-Host '==========================================================' -ForegroundColor White
        Write-Host "  Debug Web UI + API:   http://127.0.0.1:$ApiPort/" -ForegroundColor Cyan
        Write-Host "  State Directory:      $StateDir" -ForegroundColor Gray
        Write-Host "  UI Assets:            $DebugUiDir" -ForegroundColor Gray
        Write-Host "  Bearer Token File:    $TokenPath" -ForegroundColor Yellow
        Write-Host '  Press Ctrl+C to stop the debug server.' -ForegroundColor Gray
        Write-Host '==========================================================' -ForegroundColor White
        if (-not $NoBrowser) {
            Start-Process $AutoConnectUrl
        }
        Wait-Process -Id $ApiProc.Id
    }
} finally {
    if ($ApiProc -and -not $ApiProc.HasExited) {
        Stop-Process -Id $ApiProc.Id -Force -ErrorAction SilentlyContinue
    }
}
