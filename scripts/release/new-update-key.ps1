<#
.SYNOPSIS
Creates the Clatterforge update-signing key pair (ADR 0018). Run this yourself, once.

.DESCRIPTION
The Tauri signer asks for a password for the private key. The private key is written
outside the repository (default: ~/.clatterforge/release/update.key) and must never be
committed or shared. Only the public key is written into gui/src-tauri/tauri.conf.json,
where every installed app uses it to refuse updates that were not signed with this key.

Keep a backup of the private key and its password: losing them means installed apps can
no longer be updated in place.
#>
[CmdletBinding()]
param(
    [string]$KeyPath = (Join-Path $HOME '.clatterforge\release\update.key'),
    [switch]$Force
)
$ErrorActionPreference = 'Stop'
$repo = Resolve-Path (Join-Path $PSScriptRoot '..\..')
if ((Resolve-Path -LiteralPath (Split-Path $KeyPath -Parent) -ErrorAction SilentlyContinue) -and
    ([IO.Path]::GetFullPath($KeyPath).StartsWith($repo.Path, [StringComparison]::OrdinalIgnoreCase))) {
    throw 'The private key must live outside the repository.'
}
if ((Test-Path -LiteralPath $KeyPath) -and -not $Force) {
    throw "A key already exists at $KeyPath. Re-run with -Force only if you mean to replace it (installed apps will then reject updates signed with the new key)."
}
New-Item -ItemType Directory -Force -Path (Split-Path $KeyPath -Parent) | Out-Null
$signerArgs = @('tauri', 'signer', 'generate', '--write-keys', $KeyPath)
if ($Force) { $signerArgs += '--force' }
& cargo @signerArgs
if ($LASTEXITCODE -ne 0) { throw 'tauri signer generate failed' }

$public = (Get-Content -LiteralPath "$KeyPath.pub" -Raw).Trim()
$config = Join-Path $repo 'gui\src-tauri\tauri.conf.json'
$json = Get-Content -LiteralPath $config -Raw | ConvertFrom-Json
$json.plugins.updater.pubkey = $public
[IO.File]::WriteAllText($config, ($json | ConvertTo-Json -Depth 32) + "`n", [Text.UTF8Encoding]::new($false))
Write-Host "Private key: $KeyPath (keep it and its password safe, outside the repository)"
Write-Host "Public key written to gui/src-tauri/tauri.conf.json; commit that change."
