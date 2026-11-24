$ErrorActionPreference = "Stop"

Write-Host "==========================================================" -ForegroundColor White
Write-Host "  CLATTERFORGE (CF // SYSTEMS) — Windows Testing Setup    " -ForegroundColor DarkYellow
Write-Host "  CONTROL THE CHAOS.                                      " -ForegroundColor Gray
Write-Host "==========================================================" -ForegroundColor White

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$BinPath = $null
foreach ($candidate in @(
    (Join-Path $ScriptDir "clatterforge.exe"),
    (Join-Path $ScriptDir "dev-conductor.exe"),
    (Join-Path $ScriptDir "..\dist\windows-x86_64\clatterforge.exe"),
    (Join-Path $ScriptDir "..\dist\windows-x86_64\dev-conductor.exe"))) {
    if (Test-Path $candidate) { $BinPath = (Resolve-Path $candidate).Path; break }
}
if (-not $BinPath) {
    Write-Error "clatterforge.exe / dev-conductor.exe not found in $ScriptDir"
    exit 1
}

$InstallRoot = Join-Path $env:LOCALAPPDATA "Clatterforge"
$InstallDir = Join-Path $InstallRoot "bin"
$UiInstallDir = Join-Path $InstallRoot "ui"
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null

# The pre-rename name stays installed as an alias for existing scripts and configs.
Copy-Item -Force $BinPath (Join-Path $InstallDir "dev-conductor.exe")
Copy-Item -Force $BinPath (Join-Path $InstallDir "clatterforge.exe")
Write-Host "[OK] Installed clatterforge.exe & dev-conductor.exe to $InstallDir" -ForegroundColor Cyan

# Install Native Desktop GUI if bundled
$DesktopBin = Join-Path $ScriptDir "clatterforge-desktop.exe"
if (-not (Test-Path $DesktopBin)) {
    $DistDesktop = Join-Path $ScriptDir "..\dist\windows-x86_64\clatterforge-desktop.exe"
    if (Test-Path $DistDesktop) {
        $DesktopBin = (Resolve-Path $DistDesktop).Path
    }
}
if (Test-Path $DesktopBin) {
    Copy-Item -Force $DesktopBin (Join-Path $InstallDir "clatterforge-desktop.exe")
    Write-Host "[OK] Installed native desktop GUI (clatterforge-desktop.exe) to $InstallDir" -ForegroundColor Cyan
}

# Install compiled Web GUI assets if bundled
$UiSource = Join-Path $ScriptDir "ui"
if (-not (Test-Path $UiSource)) {
    $DistUi = Join-Path $ScriptDir "..\dist\windows-x86_64\ui"
    if (Test-Path $DistUi) {
        $UiSource = (Resolve-Path $DistUi).Path
    }
}
if (Test-Path $UiSource) {
    if (Test-Path $UiInstallDir) {
        Remove-Item -Recurse -Force $UiInstallDir
    }
    Copy-Item -Recurse -Force $UiSource $UiInstallDir
    Write-Host "[OK] Installed Clatterforge Web GUI bundle to $UiInstallDir" -ForegroundColor Cyan
}

$StartGuiSource = Join-Path $ScriptDir "start-gui.ps1"
if (Test-Path $StartGuiSource) {
    Copy-Item -Force $StartGuiSource (Join-Path $InstallDir "start-gui.ps1")
}

$UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
if ($UserPath -notlike "*$InstallDir*") {
    [Environment]::SetEnvironmentVariable("Path", "$UserPath;$InstallDir", "User")
    $env:Path = "$env:Path;$InstallDir"
    Write-Host "[OK] Added $InstallDir to your User PATH" -ForegroundColor Cyan
} else {
    $env:Path = "$env:Path;$InstallDir"
}

if (Get-Command git -ErrorAction SilentlyContinue) {
    $GitVer = (& git --version)
    Write-Host "[OK] Git detected: $GitVer" -ForegroundColor Cyan
} else {
    Write-Host "[!] Git not found on PATH. Install Git for Windows first." -ForegroundColor Yellow
}

Write-Host "[OK] Built-in Zero-Footprint Deterministic Router (builtin:router-v1) active (0 MB VRAM required)." -ForegroundColor Cyan

$CursorVersions = Join-Path $env:LOCALAPPDATA "cursor-agent\versions"
$CodexHome = Join-Path $env:USERPROFILE ".codex"
$CursorFound = (Test-Path $CursorVersions) -or (Get-Command cursor-agent -ErrorAction SilentlyContinue)
$CodexFound = (Test-Path $CodexHome) -or (Get-Command codex -ErrorAction SilentlyContinue)

if ($CursorFound) {
    Write-Host "[OK] Cursor Agent CLI detected (supports single-CLI Fast / Standard / Frontier tier routing)." -ForegroundColor Cyan
}
if ($CodexFound) {
    Write-Host "[OK] OpenAI Codex CLI detected." -ForegroundColor Cyan
}
if (-not $CursorFound -and -not $CodexFound) {
    Write-Host "[!] Neither Cursor Agent nor Codex CLI was detected." -ForegroundColor Yellow
    Write-Host "    Ensure Cursor Agent (%LOCALAPPDATA%\cursor-agent) or Codex CLI is installed." -ForegroundColor Gray
}

Write-Host ""
Write-Host "==========================================================" -ForegroundColor White
Write-Host "SYSTEM NOMINAL. Next steps:" -ForegroundColor Cyan
Write-Host ""
Write-Host "1. Launch the Native Desktop GUI (auto-starts backend & connects):"
Write-Host "   .\clatterforge-desktop.exe   (or .\start-gui.ps1)" -ForegroundColor Yellow
Write-Host ""
Write-Host "2. Or launch the Browser Web GUI on http://127.0.0.1:8080:"
Write-Host "   .\start-gui.ps1 -Web" -ForegroundColor Yellow
Write-Host ""
Write-Host "3. Or onboard a Git repository from the CLI (0 MB local router):"
Write-Host "   clatterforge onboard --repo C:\path\to\repo --model builtin:router-v1" -ForegroundColor Yellow
Write-Host "==========================================================" -ForegroundColor White
