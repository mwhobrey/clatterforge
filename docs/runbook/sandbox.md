# Verifier sandbox

The independent verifier runs code an agent just wrote. A preset can run it in a sandbox instead of on the host. This is opt-in per preset; without a sandbox the verifier runs on the host exactly as before. The coding CLIs (Cursor, Codex, AGY, local worker) are not sandboxed: they need your sign-in and network. Their environment is scoped (see Worker environment).

## Backends

- **Docker** (any host with Docker): the verifier runs in a container from an image pinned by digest (`name@sha256:<digest>`; a tag alone is refused, and images are never pulled at run time). The worktree is bind-mounted read-write at `/work` and is the only host path mounted. Every run uses `--rm`, `--cap-drop ALL`, `--security-opt no-new-privileges`, a numeric non-root user, and memory, CPU and process limits. It verifies inside Linux, not on the Windows host.
- **AppContainer** (Windows): the preset's own pinned verifier runs in a fresh AppContainer for each run, started through the `clatterforge appcontainer-exec` trampoline so the existing job, output capture, timeouts and cancellation still apply. The run's profile is granted read/write on the worktree and read/execute on the verifier's own folder (never the Windows or Program Files trees, which containers can already read; a drive root or your profile root is refused). At `trusted-paths` it also gets the listed read-only paths and passthrough names. Network is off unless the level grants the `internetClient` capability; loopback stays blocked by Windows. The verifier inherits only its standard handles, gets a minimal environment with its temp and app-data folders redirected into the container profile, and runs in a nested job with memory, process and CPU limits. Afterwards the grants are revoked and the profile is deleted, so a crash can leave only entries for a deleted identity. Choose it with `agent-preset-sandbox <preset> --backend app-container` or in the settings panel.

## Levels

| Level | Network | Host environment | Extra read-only mounts |
|---|---|---|---|
| `isolated` | none (`--network none`) | none | none |
| `network` | outbound (`--network bridge`) | none | none |
| `trusted-paths` | outbound | listed names only | listed paths at `/ro/<n>` |

An administrator sets a default level and a ceiling on the preset. Docker, Docker-config and SSH-agent variables can never be listed.

```powershell
clatterforge agent-preset-sandbox my-preset --docker "C:\Program Files\Docker\Docker\resources\bin\docker.exe" --image "rust@sha256:<digest>" --arg cargo --arg test --default-level isolated --ceiling network
clatterforge agent-preset-sandbox my-preset --clear
```

Every task assigned from the preset records the sandbox and its level in its assignment, so the approval digest covers them. Tasks already assigned keep what their assignment recorded.

## Requesting more access

When a task needs more, for example network access to fetch test data, an operator requests it:

```powershell
clatterforge sandbox-request <task> --revision N --level network --reason "fetches test fixtures" --by NAME
```

- Allowed before approval (the approval must then name the changed assignment digest) or after a failed, rejected or cancelled attempt (the next `board-retry` approves it). Refused between approval and run.
- Never past the preset ceiling; only an administrator editing the preset can raise that.
- Raising needs a reason, recorded with who asked.
- Not available through MCP, grants or schedules, so standing grants only ever approve tasks at their preset's default level.
- Workflow child tasks and race attempts use their preset's default level. A race requires every attempt to share one sandbox policy.

## What is enforced

AppContainer: Windows enforces file access by ACL against the run's container identity and network by capability. Anything readable by `ALL APPLICATION PACKAGES` (much of Windows and Program Files) stays readable, and the verifier's own folder is readable for the run. Verifiers should be native executables: `cmd.exe /c` parses quoted arguments differently from other Windows programs.

Docker's namespaces and cgroups enforce the filesystem view, network and limits, to the extent the Docker daemon and its VM do; the daemon is trusted. Code in the container can change the worktree it was given, which the host's normal scope check still inspects afterwards. The verification receipt records the exact Docker invocation. The coding CLIs keep your profile and network; their environment is scoped as described under Worker environment.

## Live check

The normal suite uses a stand-in Docker CLI. To check a real daemon, run the ignored live test with an image present locally that has `sh` (it is never pulled by the test):

```powershell
$env:CLATTERFORGE_SANDBOX_IMAGE = 'alpine@sha256:<digest>'
cargo test -p clatterforge --test workflow live_docker_sandbox -- --ignored
```

It runs a task at `isolated` and at `network` and checks the container user is 1000, only loopback exists without network, an interface exists with it, and no host profile or token variables reach the container.

## Settings page

The forge UI's **Forge Settings: Verifier Sandbox** panel edits the same preset policy without the CLI. Pick a preset, then:

- **Curated images:** Clatterforge ships an opinionated catalog (`src/sandbox_catalog.json`) of images pinned by digest: a minimal Alpine shell and Rust, Node.js and Python toolchains, each with a starting command, levels and memory. **Use** fills the form; **Pull** fetches the pinned image if it is not present locally (by digest, so the bytes cannot differ from the pin). Toolchain entries that install dependencies default to `network`; the agent CLI already has the repository and network, so the sandbox's job there is keeping host secrets and your profile out.
- **Local images:** pick any local image that carries a digest and it is pinned as `name@sha256:<digest>`; a tag is never stored.
- Edit the command (one argument per line), default level, ceiling, limits, read-only paths and passthrough names, then **Save sandbox** or **Remove sandbox**. Validation matches `agent-preset-sandbox`.

Only the main forge can change these: the bearer-token desktop/local UI reached through a loopback host. Paired browsers and phones see the panel read-only, a bearer request through a remote host name (such as Tailscale Serve) is refused, and Sparks never reach it. Changes apply to newly assigned tasks; existing tasks keep the sandbox their approval recorded.

Refresh the catalog's digests from their tags with `python scripts/refresh-sandbox-catalog.py` (or `--check` to report drift); review the diff, since a new digest is new code the verifier will run.

## Worker environment

Every preset assignment runs its coding CLI and its host verifier on an environment allowlist: system locations, profile folders (so Cursor, Codex and other CLIs still find their own sign-in), non-secret machine details (processor, OS, user and computer names, common program folders) and nothing else. API keys, cloud credentials and Git tokens in your environment do not reach them unless the preset lists the name:

```powershell
clatterforge agent-preset-env my-preset --pass OPENAI_API_KEY --pass HTTPS_PROXY
clatterforge agent-preset-env my-preset          # pass nothing extra
```

The settings panel's **Worker environment** field does the same from the main forge. The names are recorded in each task's assignment, so approval binds to exactly what the worker sees. Tasks assigned before the allowlist existed keep the full environment they were approved with. This is environment scoping only: the CLIs still run with your user account, profile files and network.
