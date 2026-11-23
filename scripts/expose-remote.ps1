[CmdletBinding()]
param(
    [string]$StateRoot,
    [int]$Port = 8787,
    [int]$TailscalePort = 8788,
    [string]$LanIp,
    [switch]$SkipTailscale,
    [switch]$SkipFirewallCheck
)

$ErrorActionPreference = 'Stop'
if (-not $StateRoot) {
    if ((Test-Path -LiteralPath (Join-Path $PSScriptRoot 'clatterforge.exe')) -or (Test-Path -LiteralPath (Join-Path $PSScriptRoot 'dev-conductor.exe'))) {
        $StateRoot = Join-Path $PSScriptRoot '.conductor'
    } else {
        $StateRoot = Join-Path $PSScriptRoot '../.conductor'
    }
}
$StateRoot = [IO.Path]::GetFullPath($StateRoot)
New-Item -ItemType Directory -Path $StateRoot -Force | Out-Null

# 1. Discover LAN IPv4 address if not explicitly supplied
if (-not $LanIp) {
    $lanCandidates = Get-NetIPAddress -AddressFamily IPv4 -ErrorAction SilentlyContinue |
        Where-Object {
            $_.IPAddress -notlike '127.*' -and
            $_.IPAddress -notlike '169.254.*' -and
            $_.IPAddress -notlike '100.*' -and
            $_.InterfaceAlias -notlike '*Loopback*' -and
            $_.InterfaceAlias -notlike '*vEthernet*' -and
            $_.InterfaceAlias -notlike '*Tailscale*'
        }
    if ($lanCandidates) {
        $LanIp = ($lanCandidates | Select-Object -First 1).IPAddress
    }
}

# 2. Discover Tailscale DNS name, short hostname, and IP (read-only)
$tsDns = $null
$tsShort = $null
$tsIp = $null
$tsCmd = Get-Command tailscale -ErrorAction SilentlyContinue
if ($tsCmd) {
    try {
        $tsJson = & $tsCmd.Source status --json 2>$null | ConvertFrom-Json
        if ($tsJson -and $tsJson.Self) {
            if ($tsJson.Self.DNSName) { $tsDns = $tsJson.Self.DNSName.TrimEnd('.').ToLowerInvariant() }
            if ($tsJson.Self.HostName) { $tsShort = $tsJson.Self.HostName.Trim().ToLowerInvariant() }
            if ($tsJson.Self.TailscaleIPs) {
                $tsIp = ($tsJson.Self.TailscaleIPs | Where-Object { $_ -match '^\d+\.\d+\.\d+\.\d+$' } | Select-Object -First 1)
            }
        }
    } catch {
        Write-Warning "Could not query tailscale status: $_"
    }
}

# 3. Merge exact hosts into <StateRoot>/http-access.json (preserving https_origins)
$accessPath = Join-Path $StateRoot 'http-access.json'
$existingHttps = @()
$trustedHosts = [System.Collections.Generic.SortedSet[string]]::new([StringComparer]::OrdinalIgnoreCase)

if (Test-Path -LiteralPath $accessPath) {
    $prior = Get-Content -LiteralPath $accessPath -Raw | ConvertFrom-Json
    if ($prior.https_origins) { $existingHttps = @($prior.https_origins) }
    if ($prior.trusted_http_hosts) {
        foreach ($h in $prior.trusted_http_hosts) { [void]$trustedHosts.Add($h.ToLowerInvariant()) }
    }
}

if ($LanIp) { [void]$trustedHosts.Add("${LanIp}:${Port}") }
if ($tsIp) { [void]$trustedHosts.Add("${tsIp}:${Port}") }
if ($tsShort) {
    [void]$trustedHosts.Add("${tsShort}:${Port}")
    [void]$trustedHosts.Add("${tsShort}:${TailscalePort}")
}
if ($tsDns) {
    [void]$trustedHosts.Add("${tsDns}:${TailscalePort}")
}

$policy = [ordered]@{
    trusted_http_hosts = @($trustedHosts)
    https_origins      = @($existingHttps)
}
$jsonText = ($policy | ConvertTo-Json -Depth 4) + "`n"
[IO.File]::WriteAllText($accessPath, $jsonText, [Text.UTF8Encoding]::new($false))
Write-Host "Updated $accessPath" -ForegroundColor Green

# 4. Configure Tailscale Serve on the dedicated TailscalePort only (preserves other ports)
if (-not $SkipTailscale -and $tsCmd -and $tsDns) {
    Write-Host "Configuring Tailscale Serve: http://${tsDns}:${TailscalePort} -> http://127.0.0.1:${Port}" -ForegroundColor Cyan
    & $tsCmd.Source serve --bg --http=$TailscalePort "http://127.0.0.1:${Port}"
    if ($LASTEXITCODE -ne 0) {
        Write-Warning "tailscale serve returned exit code $LASTEXITCODE"
    }
}

# 5. Check Windows Firewall rule for inbound LAN TCP on $Port
if (-not $SkipFirewallCheck) {
    $ruleName = "Clatterforge Local Network UI ($Port)"
    $existingRule = Get-NetFirewallRule -DisplayName $ruleName -ErrorAction SilentlyContinue
    if (-not $existingRule) {
        $isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
        if ($isAdmin) {
            New-NetFirewallRule -DisplayName $ruleName -Direction Inbound -Protocol TCP -LocalPort $Port -Profile Private,Domain -Action Allow | Out-Null
            Write-Host "Added Windows Firewall rule: $ruleName (Private/Domain TCP $Port)" -ForegroundColor Green
        } else {
            Write-Host ""
            Write-Host "NOTE: No inbound Windows Firewall rule found for LAN TCP port $Port." -ForegroundColor Yellow
            Write-Host "      Tailscale Serve works without a firewall rule, but direct LAN phones/laptops" -ForegroundColor Yellow
            Write-Host "      may be blocked by Windows Defender Firewall until you run in an Admin PowerShell:" -ForegroundColor Yellow
            Write-Host "      New-NetFirewallRule -DisplayName `"$ruleName`" -Direction Inbound -Protocol TCP -LocalPort $Port -Profile Private,Domain -Action Allow" -ForegroundColor White
        }
    } else {
        Write-Host "Windows Firewall rule already present: $ruleName" -ForegroundColor Green
    }
}

Write-Host ""
Write-Host "==========================================================" -ForegroundColor White
Write-Host "  CLATTERFORGE // REMOTE ACCESS ENDPOINTS                 " -ForegroundColor Cyan
Write-Host "==========================================================" -ForegroundColor White
if ($tsDns) {
    Write-Host "  Tailscale URL:     http://${tsDns}:${TailscalePort}/" -ForegroundColor Green
}
if ($LanIp) {
    Write-Host "  Local Network URL: http://${LanIp}:${Port}/" -ForegroundColor Green
}
Write-Host "  Localhost URL:     http://127.0.0.1:${Port}/" -ForegroundColor Gray
Write-Host "  Access Policy:     $accessPath" -ForegroundColor Gray
Write-Host ""
Write-Host "  Restart the managed host with -Bind 0.0.0.0 so LAN & Tailscale pick up the policy:" -ForegroundColor Yellow
Write-Host "  scripts/manage-local-host.ps1 -Action Start -Bind 0.0.0.0 -Binary .conductor/runtime/dev-conductor.exe -Assets .conductor/runtime/ui" -ForegroundColor White
Write-Host "==========================================================" -ForegroundColor White
