param(
    [Parameter(Mandatory)][string]$ConfigPath,
    [Parameter(Mandatory)][string]$Target,
    [string]$ResumeTarget,
    [string]$RequestPath
)
$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
$binaryPath = Join-Path $projectRoot 'target/debug/dev-conductor.exe'
if (-not $RequestPath) { $RequestPath = Join-Path $projectRoot 'examples/inference-checked-add.json' }
if (-not $ResumeTarget) { $ResumeTarget = $Target }
$sourceConfig = (Resolve-Path -LiteralPath $ConfigPath).Path
$config = Get-Content -LiteralPath $sourceConfig -Raw | ConvertFrom-Json
foreach ($targetId in @($Target, $ResumeTarget)) {
    if (@($config.targets | Where-Object id -CEQ $targetId).Count -ne 1) { throw "Unknown exact target: $targetId" }
}
if (-not [IO.Path]::IsPathRooted($config.admission_db)) {
    $config.admission_db = [IO.Path]::GetFullPath((Join-Path (Split-Path -Parent $sourceConfig) $config.admission_db))
}
$request = Get-Content -LiteralPath $RequestPath -Raw | ConvertFrom-Json
if ($request.tools -ne 'checked_add') { throw 'This check requires a checked_add request for 2 plus 3' }
$pilotRoot = Join-Path $projectRoot ('.conductor/inference-live/' + [guid]::NewGuid().ToString())
New-Item -ItemType Directory -Path $pilotRoot | Out-Null
$statePath = Join-Path $pilotRoot 'state'
function Save-Receipt($Name, $Value) {
    [IO.File]::WriteAllText((Join-Path $pilotRoot $Name), ($Value | ConvertTo-Json -Depth 60), [Text.UTF8Encoding]::new($false))
}
function Read-JobReceipt($Name, [string[]]$Arguments) {
    $raw = & $binaryPath --state $statePath @Arguments
    $nativeExit = $LASTEXITCODE
    [IO.File]::WriteAllText((Join-Path $pilotRoot ($Name + '.stdout.json')), ($raw -join [Environment]::NewLine), [Text.UTF8Encoding]::new($false))
    Save-Receipt ($Name + '.exit.json') @{native_exit=$nativeExit}
    if ($jobId) {
        $current = & $binaryPath --state $statePath inference-show $jobId | ConvertFrom-Json
        if ($LASTEXITCODE -ne 0) { throw "Cannot inspect job; retain evidence at $pilotRoot" }
    } else {
        $current = $raw | ConvertFrom-Json
    }
    Save-Receipt ($Name + '.json') $current
    if ($nativeExit -ne 0) { throw "Command failed; inspect $pilotRoot; no replay or resource release attempted" }
    return $current
}
Save-Receipt 'config.json' $config
Save-Receipt 'request.json' $request
$jobId = $null
$prepared = Read-JobReceipt 'prepared' @('inference-create', '--config', (Join-Path $pilotRoot 'config.json'), '--request', (Join-Path $pilotRoot 'request.json'), '--target', $Target)
$jobId = $prepared.id
$checkpoint = Read-JobReceipt 'checkpoint' @('inference-run', $jobId, '--revision', [string]$prepared.revision, '--one-turn')
if ($checkpoint.status -ne 'checkpoint' -or $checkpoint.generations.Count -ne 1 -or $checkpoint.owner_token -or
    $checkpoint.generations[0].resolved.target_id -cne $Target) {
    throw "Expected one released tool checkpoint; inspect $pilotRoot"
}
$calls = @($checkpoint.generations[0].tool_calls)
$tools = @($checkpoint.request.messages | Where-Object role -EQ 'tool')
if ($calls.Count -ne 1 -or $calls[0].name -cne 'checked_add' -or
    $calls[0].arguments.a -ne 2 -or $calls[0].arguments.b -ne 3 -or
    $tools.Count -ne 1 -or $tools[0].tool_call_id -cne $calls[0].id -or
    ($tools[0].parts.text -join '') -cne '5') {
    throw "Expected a single linked checked_add(2,3) result of 5; inspect $pilotRoot"
}
$result = Read-JobReceipt 'result' @('inference-run', $jobId, '--revision', [string]$checkpoint.revision, '--target', $ResumeTarget)
$events = & $binaryPath --state $statePath inference-events $jobId | ConvertFrom-Json
if ($LASTEXITCODE -ne 0) { throw "Cannot inspect events; retain evidence at $pilotRoot" }
Save-Receipt 'events.json' @($events)
$allCalls = @($result.generations | ForEach-Object { $_.tool_calls })
$toolResults = @($result.request.messages | Where-Object role -EQ 'tool')
$last = $result.generations[-1]
$badGenerations = @($result.generations | Where-Object { $_.terminal_events -ne 1 -or -not $_.server_completion_observed -or $_.failure })
$passed = $result.status -eq 'completed' -and $result.generations.Count -eq 2 -and
    $allCalls.Count -eq 1 -and $toolResults.Count -eq 1 -and
    $result.generations[0].id -ceq $checkpoint.generations[0].id -and
    $result.generations[0].resolved.target_id -ceq $Target -and $last.resolved.target_id -ceq $ResumeTarget -and
    $toolResults[0].tool_call_id -ceq $calls[0].id -and
    ($toolResults[0].parts.text -join '') -ceq '5' -and
    $badGenerations.Count -eq 0 -and $last.text -match '\b5\b|\bfive\b'
$summary = [pscustomobject]@{
    job=$jobId; status=$result.status; passed=$passed; initial_target=$Target; resume_target=$ResumeTarget
    generations=$result.generations.Count; tool_calls=$allCalls.Count; tool_results=$toolResults.Count
    call_id=$calls[0].id; answer=$last.text; resolved=@($result.generations.resolved)
    full_answer_review_required=$true; result_path=(Join-Path $pilotRoot 'result.json')
}
Save-Receipt 'summary.json' $summary
$summary | ConvertTo-Json -Depth 20
if (-not $passed) { throw "Tool/checkpoint validation failed; inspect $pilotRoot" }
