param([string]$Target = 'ollama-local', [switch]$OllamaCompatibility)
$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
$pilotRoot = Join-Path $projectRoot ('.conductor/inference-live/' + [guid]::NewGuid().ToString())
New-Item -ItemType Directory -Path $pilotRoot -Force | Out-Null
$statePath = Join-Path $pilotRoot 'state'
$binaryPath = Join-Path $projectRoot 'target/debug/dev-conductor.exe'
$config = Get-Content -LiteralPath (Join-Path $projectRoot 'examples/inference-config.json') -Raw | ConvertFrom-Json
$config.admission_db = Join-Path $projectRoot '.conductor/inference-admission.sqlite'
if ($OllamaCompatibility) {
    $compatible = $config.targets | Where-Object id -eq 'ollama-compatible'
    if ($Target -ne 'ollama-compatible' -or $null -eq $compatible) { throw 'Ollama compatibility option requires the exact ollama-compatible target' }
    $compatible | Add-Member -NotePropertyName extensions -NotePropertyValue @{'ollama.openai'=@{reasoning_effort='none'}}
}
$request = Get-Content -LiteralPath (Join-Path $projectRoot 'examples/inference-request.json') -Raw | ConvertFrom-Json
if ($Target -eq 'ollama-local') { $request.reasoning = $false }
$configPath = Join-Path $pilotRoot 'config.json'
$requestPath = Join-Path $pilotRoot 'request.json'
[IO.File]::WriteAllText($configPath, ($config | ConvertTo-Json -Depth 20), [Text.UTF8Encoding]::new($false))
[IO.File]::WriteAllText($requestPath, ($request | ConvertTo-Json -Depth 20), [Text.UTF8Encoding]::new($false))
$prepared = & $binaryPath --state $statePath inference-create --config $configPath --request $requestPath --target $Target | ConvertFrom-Json
if ($LASTEXITCODE -ne 0) { throw 'Live preparation failed' }
[IO.File]::WriteAllText((Join-Path $pilotRoot 'prepared.json'), ($prepared | ConvertTo-Json -Depth 30), [Text.UTF8Encoding]::new($false))
$result = & $binaryPath --state $statePath inference-run $prepared.id --revision $prepared.revision | ConvertFrom-Json
$runExit = $LASTEXITCODE
$resultPath = Join-Path $pilotRoot 'result.json'
[IO.File]::WriteAllText($resultPath, ($result | ConvertTo-Json -Depth 40), [Text.UTF8Encoding]::new($false))
$generation = $result.generations[-1]
$summary = [pscustomobject]@{target=$Target; job=$result.id; status=$result.status; native_exit=$runExit;
    answer=$generation.text; reasoning_bytes=[Text.Encoding]::UTF8.GetByteCount($generation.reasoning); terminal_events=$generation.terminal_events;
    resolved=$generation.resolved; usage=$generation.usage; client_duration_ms=$generation.client_duration_ms;
    failure=$result.failure; result_path=$resultPath; state_path=$statePath; answer_contains_expected_value=($generation.text -match '\b5\b|five');
    full_answer_review_required=$true; request_digest=$generation.request_digest;
    independent_runtime_verified=$false}
[IO.File]::WriteAllText((Join-Path $pilotRoot 'summary.json'), ($summary | ConvertTo-Json -Depth 20), [Text.UTF8Encoding]::new($false))
$summary | ConvertTo-Json -Depth 20
if ($runExit -ne 0 -or -not $summary.answer_contains_expected_value) { throw 'Bounded live check failed; inspect retained result and full answer' }
