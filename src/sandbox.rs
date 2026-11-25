//! Sandboxed verification (ADR 0017, #29).
//!
//! A preset can carry a [`SandboxPolicy`]: a backend, a default level and a ceiling, all set
//! by an administrator. Each task assigned from that preset records an [`AssignedSandbox`]
//! in its spec, so the assignment digest (and therefore approval) covers the level the
//! verifier runs at. A level can rise only up to the ceiling, only by an explicit operator
//! request with a reason, and only before an approval or retry that then binds to it.
//!
//! The Docker backend runs the verifier in a container: the worktree is the only host path
//! mounted read-write, nothing else is mounted unless the level allows listed read-only
//! paths, the network is off unless the level allows it, every capability is dropped,
//! privilege escalation is off, the user is not root, and memory, CPU and process counts
//! are capped. The image is pinned by digest. The host's Docker daemon is trusted.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::codex::Executable;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxLevel {
    /// No network, minimal environment, only the worktree.
    Isolated,
    /// As `isolated`, plus outbound network.
    Network,
    /// As `network`, plus the policy's listed environment names and read-only paths.
    TrustedPaths,
}

impl SandboxLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Isolated => "isolated",
            Self::Network => "network",
            Self::TrustedPaths => "trusted_paths",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        Ok(match value.trim().replace('-', "_").as_str() {
            "isolated" => Self::Isolated,
            "network" => Self::Network,
            "trusted_paths" => Self::TrustedPaths,
            other => {
                bail!("unknown sandbox level {other:?}; use isolated, network or trusted-paths")
            }
        })
    }

    fn allows_network(self) -> bool {
        self >= Self::Network
    }

    fn allows_trusted(self) -> bool {
        self >= Self::TrustedPaths
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SandboxBackend {
    Docker {
        /// The pinned Docker CLI.
        docker: Executable,
        /// `repository@sha256:<digest>`; a tag alone is refused.
        image: String,
        /// The verifier command inside the image, as an argument vector.
        command: Vec<String>,
        /// `uid[:gid]`, never root.
        user: String,
    },
    /// Windows AppContainer (ADR 0017 slice 2): the host's pinned verifier runs in a fresh
    /// AppContainer per run through the `appcontainer-exec` trampoline.
    AppContainer {
        /// The `clatterforge` binary that hosts the trampoline. Defaults to the running
        /// binary; set only when that is not a `clatterforge` executable (tests).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        launcher: Option<PathBuf>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SandboxPolicy {
    pub backend: SandboxBackend,
    pub default_level: SandboxLevel,
    pub ceiling: SandboxLevel,
    /// Host environment names passed through at `trusted_paths`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env_passthrough: Vec<String>,
    /// Host paths mounted read-only at `trusted_paths`, at `/ro/<n>` in the container.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read_only_paths: Vec<PathBuf>,
    pub memory_mb: u32,
    pub cpus: u32,
    pub pids: u32,
}

/// Environment names a container never receives, whatever the policy lists.
const NEVER_PASSED: &[&str] = &[
    "DOCKER_HOST",
    "DOCKER_CONFIG",
    "DOCKER_CERT_PATH",
    "SSH_AUTH_SOCK",
];

impl SandboxPolicy {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.default_level <= self.ceiling,
            "sandbox default level is above its ceiling"
        );
        ensure!(
            (256..=65_536).contains(&self.memory_mb)
                && (1..=64).contains(&self.cpus)
                && (16..=65_536).contains(&self.pids),
            "sandbox limits must be 256-65536 MB memory, 1-64 CPUs and 16-65536 processes"
        );
        ensure!(
            self.env_passthrough.len() <= 32
                && self.env_passthrough.iter().all(|name| {
                    !name.is_empty()
                        && name.len() <= 64
                        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                        && !NEVER_PASSED.iter().any(|n| n.eq_ignore_ascii_case(name))
                }),
            "sandbox environment names must be plain names (at most 32), never Docker or SSH agent settings"
        );
        ensure!(
            self.read_only_paths.len() <= 16,
            "at most 16 sandbox read-only paths"
        );
        for path in &self.read_only_paths {
            ensure!(
                path.is_absolute() && path.exists(),
                "sandbox read-only path must exist and be absolute: {}",
                path.display()
            );
        }
        match &self.backend {
            SandboxBackend::Docker {
                docker,
                image,
                command,
                user,
            } => {
                docker.validate()?;
                let (name, digest) = image
                    .split_once("@sha256:")
                    .context("sandbox image must be pinned as name@sha256:<digest>")?;
                ensure!(
                    !name.is_empty()
                        && !name.starts_with('-')
                        && digest.len() == 64
                        && digest.bytes().all(|b| b.is_ascii_hexdigit()),
                    "sandbox image must be pinned as name@sha256:<64 hex digits>"
                );
                ensure!(
                    !command.is_empty()
                        && command.len() <= 64
                        && command.iter().all(|a| !a.contains('\0'))
                        && command.iter().map(String::len).sum::<usize>() <= 16_384,
                    "sandbox command must be 1-64 arguments without NUL"
                );
                let uid = user.split(':').next().unwrap_or("");
                ensure!(
                    !user.is_empty()
                        && user.split(':').all(
                            |part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit())
                        )
                        && uid != "0",
                    "sandbox user must be a numeric non-root uid[:gid]"
                );
            }
            SandboxBackend::AppContainer { launcher } => {
                ensure!(cfg!(windows), "the AppContainer sandbox needs Windows");
                if let Some(launcher) = launcher {
                    ensure!(
                        launcher.is_absolute() && launcher.is_file(),
                        "AppContainer launcher must be an existing absolute path"
                    );
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AssignedSandbox {
    pub policy: SandboxPolicy,
    pub level: SandboxLevel,
    /// Why the level was raised above the policy default, and by whom.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_by: Option<String>,
}

impl AssignedSandbox {
    pub fn at_default(policy: &SandboxPolicy) -> Self {
        Self {
            policy: policy.clone(),
            level: policy.default_level,
            reason: None,
            requested_by: None,
        }
    }

    pub fn validate(&self) -> Result<()> {
        self.policy.validate()?;
        ensure!(
            self.level <= self.policy.ceiling,
            "sandbox level {} is above the preset ceiling {}",
            self.level.as_str(),
            self.policy.ceiling.as_str()
        );
        Ok(())
    }

    /// Changes the level within the ceiling. Raising it above the default needs a reason.
    pub fn request(&mut self, level: SandboxLevel, reason: &str, by: &str) -> Result<()> {
        ensure!(!by.trim().is_empty(), "operator label required");
        ensure!(
            level <= self.policy.ceiling,
            "sandbox level {} is above the preset ceiling {}; only an administrator editing the preset can raise it",
            level.as_str(),
            self.policy.ceiling.as_str()
        );
        let reason = reason.trim();
        if level > self.policy.default_level {
            ensure!(
                !reason.is_empty() && reason.chars().count() <= 500,
                "raising the sandbox level needs a reason of up to 500 characters"
            );
        }
        self.level = level;
        self.reason = (!reason.is_empty()).then(|| reason.to_string());
        self.requested_by = Some(by.trim().to_string());
        Ok(())
    }
}

/// The verifier invocation for `workspace` under `sandbox`: the program and its arguments.
/// Docker runs the policy's container command, and listed environment names reach it as
/// `--env NAME` from the Docker CLI's own environment. AppContainer runs the host's pinned
/// `verifier` through the trampoline, which hands listed names through itself.
pub fn verifier_invocation(
    sandbox: &AssignedSandbox,
    workspace: &Path,
    verifier: &crate::codex::PinnedCommand,
) -> Result<(PathBuf, Vec<String>)> {
    sandbox.validate()?;
    let (docker, image, command, user) = match &sandbox.policy.backend {
        SandboxBackend::Docker {
            docker,
            image,
            command,
            user,
        } => (docker, image, command, user),
        SandboxBackend::AppContainer { launcher } => {
            verifier.validate()?;
            let program = match launcher {
                Some(path) => path.clone(),
                None => std::env::current_exe().context("cannot locate the clatterforge binary")?,
            };
            let trusted = sandbox.level.allows_trusted();
            let spec = crate::appcontainer::LaunchSpec {
                workspace: workspace.to_path_buf(),
                network: sandbox.level.allows_network(),
                read_only_paths: if trusted {
                    sandbox.policy.read_only_paths.clone()
                } else {
                    Vec::new()
                },
                env_passthrough: if trusted {
                    sandbox.policy.env_passthrough.clone()
                } else {
                    Vec::new()
                },
                memory_mb: sandbox.policy.memory_mb,
                cpus: sandbox.policy.cpus,
                pids: sandbox.policy.pids,
            };
            let mut args = vec![
                "appcontainer-exec".to_string(),
                "--spec".to_string(),
                serde_json::to_string(&spec)?,
                "--".to_string(),
                verifier.executable.path.to_string_lossy().into_owned(),
            ];
            args.extend(verifier.args.iter().cloned());
            return Ok((program, args));
        }
    };
    let workspace = workspace
        .to_str()
        .context("sandbox workspace path must be UTF-8")?;
    ensure!(
        !workspace.contains(','),
        "sandbox workspace path cannot contain a comma"
    );
    let policy = &sandbox.policy;
    let mut args: Vec<String> = [
        "run",
        "--rm",
        "--pull",
        "never",
        "--network",
        if sandbox.level.allows_network() {
            "bridge"
        } else {
            "none"
        },
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges",
        "--user",
        user,
        "--memory",
        &format!("{}m", policy.memory_mb),
        "--cpus",
        &policy.cpus.to_string(),
        "--pids-limit",
        &policy.pids.to_string(),
        "--mount",
        &format!("type=bind,source={workspace},target=/work"),
        "--workdir",
        "/work",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if sandbox.level.allows_trusted() {
        for (n, path) in policy.read_only_paths.iter().enumerate() {
            let source = path
                .to_str()
                .context("sandbox read-only path must be UTF-8")?;
            ensure!(
                !source.contains(','),
                "sandbox read-only path cannot contain a comma"
            );
            args.push("--mount".into());
            args.push(format!("type=bind,source={source},target=/ro/{n},readonly"));
        }
        for name in &policy.env_passthrough {
            // `-e NAME` hands the container the value from the Docker CLI's environment.
            args.push("--env".into());
            args.push(name.clone());
        }
    }
    args.push(image.clone());
    args.extend(command.iter().cloned());
    Ok((docker.path.clone(), args))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(default_level: SandboxLevel, ceiling: SandboxLevel) -> SandboxPolicy {
        SandboxPolicy {
            backend: SandboxBackend::Docker {
                docker: Executable::new(&std::env::current_exe().unwrap()).unwrap(),
                image: format!("example/verifier@sha256:{}", "a".repeat(64)),
                command: vec!["cargo".into(), "test".into()],
                user: "1000:1000".into(),
            },
            default_level,
            ceiling,
            env_passthrough: vec!["CARGO_TERM_COLOR".into()],
            read_only_paths: vec![std::env::temp_dir()],
            memory_mb: 2048,
            cpus: 2,
            pids: 512,
        }
    }

    fn verifier() -> crate::codex::PinnedCommand {
        crate::codex::PinnedCommand {
            executable: Executable::new(&std::env::current_exe().unwrap()).unwrap(),
            args: vec!["--verify".into()],
        }
    }

    fn with_backend(
        mut p: SandboxPolicy,
        change: impl FnOnce(&mut SandboxBackend),
    ) -> SandboxPolicy {
        change(&mut p.backend);
        p
    }

    #[test]
    fn policies_must_pin_images_avoid_root_and_respect_their_ceiling() {
        use SandboxLevel::*;
        assert!(policy(Isolated, Network).validate().is_ok());
        assert!(
            policy(Network, Isolated).validate().is_err(),
            "default above ceiling"
        );
        let tag_only = with_backend(policy(Isolated, Isolated), |b| {
            if let SandboxBackend::Docker { image, .. } = b {
                *image = "example/verifier:latest".into();
            }
        });
        assert!(tag_only.validate().is_err(), "a tag is not a pin");
        let root = with_backend(policy(Isolated, Isolated), |b| {
            if let SandboxBackend::Docker { user, .. } = b {
                *user = "0:0".into();
            }
        });
        assert!(root.validate().is_err(), "root is refused");
        let mut leaky = policy(Isolated, Isolated);
        leaky.env_passthrough = vec!["DOCKER_HOST".into()];
        assert!(
            leaky.validate().is_err(),
            "Docker settings never pass through"
        );
        let mut app = policy(Isolated, Isolated);
        app.backend = SandboxBackend::AppContainer { launcher: None };
        assert_eq!(
            app.validate().is_ok(),
            cfg!(windows),
            "AppContainer is Windows-only"
        );
        app.backend = SandboxBackend::AppContainer {
            launcher: Some(PathBuf::from("relative/launcher.exe")),
        };
        assert!(
            app.validate().is_err(),
            "a launcher must be an absolute file"
        );
    }

    #[test]
    fn requests_stay_under_the_ceiling_and_need_a_reason_to_rise() {
        let mut sandbox =
            AssignedSandbox::at_default(&policy(SandboxLevel::Isolated, SandboxLevel::Network));
        assert!(
            sandbox
                .request(SandboxLevel::TrustedPaths, "need toolchain", "mike")
                .is_err()
        );
        assert!(
            sandbox
                .request(SandboxLevel::Network, "  ", "mike")
                .is_err()
        );
        sandbox
            .request(SandboxLevel::Network, "fetches crates", "mike")
            .unwrap();
        assert_eq!(sandbox.level, SandboxLevel::Network);
        assert_eq!(sandbox.requested_by.as_deref(), Some("mike"));
        sandbox.request(SandboxLevel::Isolated, "", "mike").unwrap();
        assert_eq!(
            sandbox.level,
            SandboxLevel::Isolated,
            "lowering needs no reason"
        );
    }

    #[test]
    fn invocations_grant_network_and_host_material_only_at_their_level() {
        let workspace = std::env::temp_dir();
        let ws = workspace.to_str().unwrap().to_string();
        let mut sandbox = AssignedSandbox::at_default(&policy(
            SandboxLevel::Isolated,
            SandboxLevel::TrustedPaths,
        ));
        let (_, isolated) = verifier_invocation(&sandbox, &workspace, &verifier()).unwrap();
        let joined = isolated.join(" ");
        assert!(joined.contains("--network none"), "{joined}");
        for flag in [
            "--cap-drop ALL",
            "--security-opt no-new-privileges",
            "--user 1000:1000",
            "--pull never",
            "--pids-limit 512",
        ] {
            assert!(joined.contains(flag), "missing {flag}: {joined}");
        }
        assert!(joined.contains(&format!("type=bind,source={ws},target=/work")));
        assert!(
            !joined.contains("--env") && !joined.contains("readonly"),
            "{joined}"
        );
        assert_eq!(&isolated[isolated.len() - 2..], ["cargo", "test"]);

        sandbox
            .request(SandboxLevel::Network, "fetch", "mike")
            .unwrap();
        let (_, network) = verifier_invocation(&sandbox, &workspace, &verifier()).unwrap();
        assert!(network.join(" ").contains("--network bridge"));
        assert!(!network.join(" ").contains("readonly"));

        sandbox
            .request(SandboxLevel::TrustedPaths, "toolchain", "mike")
            .unwrap();
        let (_, trusted) = verifier_invocation(&sandbox, &workspace, &verifier()).unwrap();
        let joined = trusted.join(" ");
        assert!(joined.contains("target=/ro/0,readonly"), "{joined}");
        assert!(joined.contains("--env CARGO_TERM_COLOR"), "{joined}");
    }

    #[cfg(windows)]
    #[test]
    fn appcontainer_invocations_run_the_host_verifier_through_the_trampoline() {
        let workspace = std::env::temp_dir();
        let launcher = std::env::current_exe().unwrap();
        let mut policy = policy(SandboxLevel::Isolated, SandboxLevel::TrustedPaths);
        policy.backend = SandboxBackend::AppContainer {
            launcher: Some(launcher.clone()),
        };
        let mut sandbox = AssignedSandbox::at_default(&policy);
        let verifier = verifier();
        let spec_at = |sandbox: &AssignedSandbox| {
            let (program, args) = verifier_invocation(sandbox, &workspace, &verifier).unwrap();
            assert_eq!(program, launcher);
            assert_eq!(args[0], "appcontainer-exec");
            assert_eq!(args[3], "--");
            assert_eq!(args[4], verifier.executable.path.to_string_lossy());
            assert_eq!(args[5], "--verify");
            serde_json::from_str::<crate::appcontainer::LaunchSpec>(&args[2]).unwrap()
        };
        let isolated = spec_at(&sandbox);
        assert!(!isolated.network && isolated.read_only_paths.is_empty());
        assert!(isolated.env_passthrough.is_empty());
        assert_eq!((isolated.memory_mb, isolated.pids), (2048, 512));

        sandbox
            .request(SandboxLevel::Network, "fetch", "mike")
            .unwrap();
        let network = spec_at(&sandbox);
        assert!(network.network && network.read_only_paths.is_empty());

        sandbox
            .request(SandboxLevel::TrustedPaths, "toolchain", "mike")
            .unwrap();
        let trusted = spec_at(&sandbox);
        assert_eq!(trusted.read_only_paths, vec![std::env::temp_dir()]);
        assert_eq!(
            trusted.env_passthrough,
            vec!["CARGO_TERM_COLOR".to_string()]
        );
    }
}
