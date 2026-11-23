param(
    [string]$ConfigPath = 'examples/orchestration-ollama-config.json',
    [string]$Target = 'gemma12b-coder'
)
$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
$pilotRoot = Join-Path $projectRoot ('.conductor/orchestration-live/' + [guid]::NewGuid().ToString())
$repository = Join-Path $pilotRoot 'repository'
$state = Join-Path $pilotRoot 'state'
New-Item -ItemType Directory -Path (Join-Path $repository 'tests') -Force | Out-Null
foreach ($name in @('Cargo.toml','Cargo.lock','.gitignore','README.md','AGENTS.md','src')) {
    Copy-Item -LiteralPath (Join-Path $projectRoot $name) -Destination $repository -Recurse
}
$contract = @'
use dev_conductor::repository::remote_identity;

#[test]
fn scp_remotes_remove_query_and_fragment_credentials() {
    assert_eq!(remote_identity("git@Git.Example:team/project.git?access_token=secret#anchor").unwrap(), "ssh://git.example/team/project");
    assert_eq!(remote_identity("git@Git.Example:team/project.git#anchor").unwrap(), "ssh://git.example/team/project");
    assert_eq!(remote_identity("https://user:secret@Git.Example/team/project.git?token=secret#anchor").unwrap(), "https://git.example/team/project");
}

#[test]
fn invalid_scp_host_or_repository_is_rejected() {
    assert!(remote_identity("git@:team/project.git").is_err());
    assert!(remote_identity("git@Git.Example:").is_err());
    assert!(remote_identity("git@Git.Example:?token=secret").is_err());
}

#[test]
fn existing_local_and_remote_identity_behavior_is_preserved() {
    assert_eq!(remote_identity("git@Git.Example:team/project.git").unwrap(), "ssh://git.example/team/project");
    assert_eq!(remote_identity(r"C:\repos\project#draft").unwrap(), "file:C:/repos/project#draft");
    assert_eq!(remote_identity("/srv/project#draft").unwrap(), "file:/srv/project#draft");
    assert_eq!(remote_identity("file:///srv/project#draft").unwrap(), "file:///srv/project");
}
'@
[IO.File]::WriteAllText((Join-Path $repository 'tests/remote_identity_contract.rs'), $contract, [Text.UTF8Encoding]::new($false))
& git -C $repository init -b main | Out-Null
if ($LASTEXITCODE -ne 0) { throw 'Cannot initialize isolated pilot' }
& git -C $repository config core.autocrlf false
& git -C $repository add .
if ($LASTEXITCODE -ne 0) { throw 'Cannot index pilot snapshot' }
& git -C $repository -c user.name='Conductor pilot fixture' -c user.email='pilot@example.invalid' -c core.hooksPath= -c commit.gpgsign=false commit -m 'Isolated source snapshot and independent remote identity contract' | Out-Null
if ($LASTEXITCODE -ne 0) { throw 'Cannot commit isolated baseline' }
$configSource = (Resolve-Path -LiteralPath $ConfigPath).Path
$config = Get-Content -LiteralPath $configSource -Raw | ConvertFrom-Json
$config.admission_db = Join-Path $projectRoot '.conductor/inference-admission.sqlite'
$config.fallback_before_dispatch = $false
$selected = @($config.targets | Where-Object id -CEQ $Target)
if ($selected.Count -ne 1 -or $selected[0].provider -ne 'ollama') { throw 'This pilot requires one explicit native Ollama target' }
$config.targets = $selected
$selected[0].context_limit = 32768
$selected[0].output_limit = 4096
$selected[0].capabilities.tools = $false
$cargoExecutable = & (Get-Command rustup.exe).Source which cargo
if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath $cargoExecutable)) { throw 'Resolve a concrete Cargo executable with rustup which cargo' }
$spec = [ordered]@{
    repository=$repository
    objective='In src/repository.rs, only edit the non-Windows SCP branch: the else after if host.len() == 1, currently containing let host = host.rsplit(...).to_lowercase(); followed by format!("ssh://{host}/{path}"). Add query/fragment sanitization for path and nonempty host/path validation between those two existing statements. Keep all other original code verbatim. Do not change the HTTPS or Windows branches or tests. Use a small exact replacement of that unique SCP block.'
    acceptance='The immutable tests/remote_identity_contract.rs tests pass. Only src/repository.rs changes. Existing identity examples and local paths containing # retain their current behavior.'
    allowed_files=@('src/repository.rs')
    context_files=@()
    verification_files=@('tests/remote_identity_contract.rs')
    verify_executable=$cargoExecutable
    verify_args=@('test','--locked','--offline','--test','remote_identity_contract','--target-dir',(Join-Path $pilotRoot 'build'))
    target=$Target
    max_rounds=2
    max_output_tokens=4096
    generation_timeout_ms=120000
    verification_timeout_ms=120000
    reasoning=$false
}
foreach ($item in @(@{name='spec.json';value=$spec},@{name='config.json';value=$config})) {
    [IO.File]::WriteAllText((Join-Path $pilotRoot $item.name), ($item.value | ConvertTo-Json -Depth 30), [Text.UTF8Encoding]::new($false))
}
$binary = Join-Path $projectRoot 'target/debug/dev-conductor.exe'
$prepared = & $binary --state $state orchestrate-prepare --spec (Join-Path $pilotRoot 'spec.json') --config (Join-Path $pilotRoot 'config.json') | ConvertFrom-Json
if ($LASTEXITCODE -ne 0) { throw 'Pilot preparation failed; retain snapshot' }
[IO.File]::WriteAllText((Join-Path $pilotRoot 'prepared.json'), ($prepared | ConvertTo-Json -Depth 60), [Text.UTF8Encoding]::new($false))
[pscustomobject]@{pilot_root=$pilotRoot;state=$state;repository=$repository;task=$prepared.task.id;revision=$prepared.task.revision;assignment_digest=$prepared.assignment_digest;spec_path=(Join-Path $pilotRoot 'spec.json');config_path=(Join-Path $pilotRoot 'config.json');status=$prepared.task.status;scope=$prepared.task.assignment.spec.allowed_files} | ConvertTo-Json -Depth 8
