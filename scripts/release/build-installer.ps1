<#
.SYNOPSIS
Builds the per-user Clatterforge installer and, unless -SkipUpdater, signed update artifacts
and a latest.json manifest for GitHub Releases (ADR 0018).

.DESCRIPTION
1. Builds the release backend and stages it as the Tauri sidecar.
2. Builds the desktop app and NSIS installer with release-only settings injected through a
   Tauri config override: the sidecar, the web UI as a `ui/` resource, updater artifacts and
   the update endpoint. None of these live in the committed configuration.
3. Writes latest.json next to the installer.

Update artifacts need the private update key: set TAURI_SIGNING_PRIVATE_KEY to its path or
contents and TAURI_SIGNING_PRIVATE_KEY_PASSWORD to its password (see new-update-key.ps1).
The endpoint and asset URL come from a private release file outside the repository
(default ~/.clatterforge/release/updater.json):

  { "endpoint": "https://github.com/<owner>/<repo>/releases/latest/download/latest.json",
    "asset_base_url": "https://github.com/<owner>/<repo>/releases/download/v{version}/" }

-SignCommand adds Authenticode-style signing of the app and installer; Tauri runs it on each
file with %1 replaced by the path, before update signatures are computed.
#>
[CmdletBinding()]
param(
    [string]$ReleaseConfig = (Join-Path $HOME '.clatterforge\release\updater.json'),
    [string]$OutDir,
    [string]$SignCommand,
    [string]$Notes = '',
    [switch]$SkipUpdater
)
$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$tauriDir = Join-Path $repo 'gui\src-tauri'
$config = Get-Content -LiteralPath (Join-Path $tauriDir 'tauri.conf.json') -Raw | ConvertFrom-Json
$version = $config.version
if (-not $OutDir) { $OutDir = Join-Path $repo "dist\installer\$version" }

$override = [ordered]@{
    bundle = [ordered]@{
        externalBin = @('binaries/clatterforge')
        resources = @{ '../target/dx/clatterforge-gui/release/web/public/' = 'ui/' }
        createUpdaterArtifacts = (-not $SkipUpdater)
    }
}
$release = $null
if (-not $SkipUpdater) {
    if ([string]::IsNullOrWhiteSpace($config.plugins.updater.pubkey)) {
        throw 'No update public key in tauri.conf.json; run scripts/release/new-update-key.ps1 first, or pass -SkipUpdater for a local test build.'
    }
    if (-not $env:TAURI_SIGNING_PRIVATE_KEY) {
        throw 'Set TAURI_SIGNING_PRIVATE_KEY (and TAURI_SIGNING_PRIVATE_KEY_PASSWORD) to sign update artifacts, or pass -SkipUpdater.'
    }
    if (-not (Test-Path -LiteralPath $ReleaseConfig)) {
        throw "Release endpoint file not found: $ReleaseConfig (see this script's help)."
    }
    $release = Get-Content -LiteralPath $ReleaseConfig -Raw | ConvertFrom-Json
    foreach ($field in 'endpoint', 'asset_base_url') {
        if (-not ($release.$field -as [string]).StartsWith('https://')) {
            throw "$ReleaseConfig needs an https $field."
        }
    }
    $override.plugins = @{ updater = @{ endpoints = @($release.endpoint) } }
}
if ($SignCommand) {
    $override.bundle.windows = @{ signCommand = $SignCommand }
}

Write-Host "== backend (release)"
$env:CARGO_TARGET_DIR = Join-Path $repo 'target\installer'
Push-Location $repo
try {
    cargo build --release --locked -p clatterforge
    if ($LASTEXITCODE -ne 0) { throw 'backend build failed' }
} finally { Pop-Location }
$triple = ((rustc -vV) | Select-String '^host: ').ToString().Substring(6).Trim()
$binaries = Join-Path $tauriDir 'binaries'
New-Item -ItemType Directory -Force -Path $binaries | Out-Null
Copy-Item (Join-Path $env:CARGO_TARGET_DIR 'release\clatterforge.exe') (Join-Path $binaries "clatterforge-$triple.exe") -Force

Write-Host "== desktop app and installer"
$overrideFile = Join-Path ([IO.Path]::GetTempPath()) "clatterforge-release-$([guid]::NewGuid()).json"
[IO.File]::WriteAllText($overrideFile, ($override | ConvertTo-Json -Depth 8), [Text.UTF8Encoding]::new($false))
$env:CARGO_TARGET_DIR = Join-Path $repo 'target\installer-desktop'
Push-Location $tauriDir
try {
    cargo tauri build --bundles nsis --config $overrideFile
    if ($LASTEXITCODE -ne 0) { throw 'tauri build failed' }
} finally {
    Pop-Location
    Remove-Item -LiteralPath $overrideFile -ErrorAction SilentlyContinue
}

$nsisDir = Join-Path $env:CARGO_TARGET_DIR 'release\bundle\nsis'
$installer = Get-ChildItem -LiteralPath $nsisDir -Filter "*_$($version)_*-setup.exe" | Select-Object -First 1
if (-not $installer) { throw "installer not found in $nsisDir" }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
Copy-Item $installer.FullName $OutDir -Force

if (-not $SkipUpdater) {
    $sigPath = "$($installer.FullName).sig"
    if (-not (Test-Path -LiteralPath $sigPath)) { throw "update signature missing: $sigPath" }
    Copy-Item $sigPath $OutDir -Force
    $manifest = [ordered]@{
        version = $version
        notes = $Notes
        pub_date = (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')
        platforms = @{
            'windows-x86_64' = [ordered]@{
                signature = (Get-Content -LiteralPath $sigPath -Raw).Trim()
                url = ($release.asset_base_url -replace '\{version\}', $version) + $installer.Name
            }
        }
    }
    [IO.File]::WriteAllText((Join-Path $OutDir 'latest.json'), ($manifest | ConvertTo-Json -Depth 8), [Text.UTF8Encoding]::new($false))
}
Get-ChildItem -LiteralPath $OutDir | Select-Object Name, Length
Write-Host "Installer artifacts: $OutDir"
if (-not $SignCommand) {
    Write-Host 'Unsigned build: Windows SmartScreen warns on first run until code signing is added (-SignCommand).'
}
