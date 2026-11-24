param(
    [Parameter(Mandatory=$true)][string]$Model,
    [Parameter(Mandatory=$true)][string]$CodexHome,
    [Parameter(Mandatory=$true)][string]$CodexExecutable,
    [Parameter(Mandatory=$true)][string]$PythonExecutable
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
$projectRoot = Split-Path -Parent $PSScriptRoot
$pilotRoot = Join-Path (Resolve-StateDir $projectRoot) ('provider-pilots/' + [guid]::NewGuid().ToString())
$fixtureRoot = Join-Path $pilotRoot 'fixture-repo'
$statePath = Join-Path $pilotRoot 'state'
$binaryPath = Join-Path $projectRoot 'target/debug/clatterforge.exe'
$expectedCode = "def add(a, b):`n    return a + b`n"
$checkCode = "import runpy; m=runpy.run_path('result.py'); f=m['add']; assert f(2,3)==5; assert f(-4,1)==-3; assert f(0,0)==0; print('3 independent Python checks passed')"

Push-Location -LiteralPath $projectRoot
try {
    & cargo build --locked
    if ($LASTEXITCODE -ne 0) { throw 'Build failed' }
    New-Item -ItemType Directory -Path $fixtureRoot -Force | Out-Null
    function Invoke-PilotGit {
        param([string[]]$CommandArgs)
        & git -C $fixtureRoot @CommandArgs | Out-Null
        if ($LASTEXITCODE -ne 0) { throw 'Expendable fixture Git failed' }
    }
    Invoke-PilotGit @('init','-b','main')
    Invoke-PilotGit @('config','core.autocrlf','false')
    [IO.File]::WriteAllText((Join-Path $fixtureRoot 'README.md'), "# Expendable provider pilot`nCreate the approved result.py; the host verifies it independently.`n", [Text.UTF8Encoding]::new($false))
    [IO.File]::WriteAllText((Join-Path $fixtureRoot 'AGENTS.md'), "Create only result.py. Do not modify other files, commit, push, delegate or access other accounts.`n", [Text.UTF8Encoding]::new($false))
    Invoke-PilotGit @('add','.')
    Invoke-PilotGit @('-c','user.name=Pilot Fixture','-c','user.email=fixture@example.invalid','-c','core.hooksPath=','-c','commit.gpgsign=false','commit','-m','Expendable provider pilot')
    $registration = & $binaryPath --state $statePath register $fixtureRoot | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0) { throw 'Registration failed' }
    $prepared = & $binaryPath --state $statePath create-codex $registration.id 'Create a Python add function' --acceptance 'Exact approved function; positive, negative and zero cases pass the independent Python check' --contents $expectedCode --output-file result.py --codex-executable $CodexExecutable --codex-home $CodexHome --model $Model --verify-executable $PythonExecutable --verify-arg=-c --verify-arg $checkCode | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0) { throw 'Provider preparation failed' }
    $prepared | Add-Member -NotePropertyName state_path -NotePropertyValue $statePath
    $prepared | Add-Member -NotePropertyName fixture_path -NotePropertyValue $fixtureRoot
    $prepared | Add-Member -NotePropertyName human_approval -NotePropertyValue 'pending; no provider launched'
    $resultPath = Join-Path $pilotRoot 'prepared-assignment.json'
    [IO.File]::WriteAllText($resultPath, ($prepared | ConvertTo-Json -Depth 40), [Text.UTF8Encoding]::new($false))
    [pscustomobject]@{task_id=$prepared.task.id; revision=$prepared.task.revision; status=$prepared.task.status;
        assignment_digest=$prepared.assignment_digest; state_path=$statePath; fixture_path=$fixtureRoot;
        model=$Model; account_home=$CodexHome; provider_version=$prepared.task.assignment.codex.version;
        provider_launch='not performed'; prepared_assignment=$resultPath} | ConvertTo-Json
} finally {
    Pop-Location
}
