# Verifier sandbox

The independent verifier runs code an agent just wrote. A preset can run it in a sandbox instead of on the host. This is opt-in per preset; without a sandbox the verifier runs on the host exactly as before. The coding CLIs (Cursor, Codex, AGY, local worker) are not sandboxed: they need your sign-in and network.

## Backends

- **Docker** (any host with Docker): the verifier runs in a container from an image pinned by digest (`name@sha256:<digest>`; a tag alone is refused, and images are never pulled at run time). The worktree is bind-mounted read-write at `/work` and is the only host path mounted. Every run uses `--rm`, `--cap-drop ALL`, `--security-opt no-new-privileges`, a numeric non-root user, and memory, CPU and process limits. It verifies inside Linux, not on the Windows host.
- **AppContainer** (Windows): planned (#33). Selecting it is refused for now.

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

Docker's namespaces and cgroups enforce the filesystem view, network and limits, to the extent the Docker daemon and its VM do; the daemon is trusted. Code in the container can change the worktree it was given, which the host's normal scope check still inspects afterwards. The verification receipt records the exact Docker invocation. The coding CLIs keep your environment, profile and network; an environment allowlist for them is planned (#34).

## Live check

The normal suite uses a stand-in Docker CLI. To check a real daemon, run the ignored live test with an image present locally that has `sh` (it is never pulled by the test):

```powershell
$env:CLATTERFORGE_SANDBOX_IMAGE = 'alpine@sha256:<digest>'
cargo test -p clatterforge --test workflow live_docker_sandbox -- --ignored
```

It runs a task at `isolated` and at `network` and checks the container user is 1000, only loopback exists without network, an interface exists with it, and no host profile or token variables reach the container.
