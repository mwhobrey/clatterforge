//! Forge settings for verifier sandboxes (#35): the curated image catalog, local Docker
//! discovery, pulling a pinned image, and applying a preset's sandbox from a form.
//!
//! Only the main forge may change these (see `serve.rs`): Sparks never reach this module,
//! and paired browser/phone sessions get the read-only overview.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    agent_preset::AgentPresets,
    process::bounded_output,
    sandbox::{SandboxBackend, SandboxLevel, SandboxPolicy},
};

const CATALOG: &str = include_str!("sandbox_catalog.json");
const PULL_TIMEOUT_MS: u64 = 15 * 60_000;

/// One curated, digest-pinned image with an opinionated starting command and levels.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CatalogEntry {
    pub id: String,
    pub label: String,
    /// The human tag the digest was resolved from (`scripts/refresh-sandbox-catalog.py`).
    pub tag: String,
    pub image: String,
    pub command: Vec<String>,
    pub default_level: SandboxLevel,
    pub ceiling: SandboxLevel,
    pub memory_mb: u32,
    pub note: String,
}

pub fn catalog() -> Result<Vec<CatalogEntry>> {
    serde_json::from_str(CATALOG).context("built-in sandbox catalog is invalid")
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LocalImage {
    /// `repository@sha256:<digest>`, ready to pin.
    pub reference: String,
    pub repository: String,
    pub tag: String,
    pub size: String,
}

/// The Docker CLI: on PATH, else Docker Desktop's default Windows location.
pub fn find_docker() -> Option<PathBuf> {
    let name = if cfg!(windows) {
        "docker.exe"
    } else {
        "docker"
    };
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
        .or_else(|| {
            let desktop = PathBuf::from(r"C:\Program Files\Docker\Docker\resources\bin\docker.exe");
            (cfg!(windows) && desktop.is_file()).then_some(desktop)
        })
}

fn docker_output(docker: &Path, args: &[&str], timeout_ms: u64) -> Result<String> {
    let output = bounded_output(Command::new(docker).args(args), timeout_ms, 4 * 1024 * 1024)
        .context("could not run docker")?;
    ensure!(
        output.exit_code == Some(0),
        "docker {} failed: {}",
        args.first().unwrap_or(&""),
        String::from_utf8_lossy(&output.stderr.bytes).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout.bytes).into_owned())
}

/// Local images that carry a registry digest, so they can be pinned. Images built locally
/// without a digest are left out.
pub fn local_images(docker: &Path) -> Result<Vec<LocalImage>> {
    let listing = docker_output(
        docker,
        &["image", "ls", "--digests", "--format", "{{json .}}"],
        30_000,
    )?;
    let mut images: Vec<LocalImage> = listing
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|row| {
            let repository = row["Repository"].as_str()?.to_string();
            let digest = row["Digest"].as_str()?;
            if repository == "<none>" || !digest.starts_with("sha256:") {
                return None;
            }
            Some(LocalImage {
                reference: format!("{repository}@{digest}"),
                tag: row["Tag"].as_str().unwrap_or("").to_string(),
                size: row["Size"].as_str().unwrap_or("").to_string(),
                repository,
            })
        })
        .collect();
    images.sort_by(|a, b| a.reference.cmp(&b.reference));
    images.dedup_by(|a, b| a.reference == b.reference);
    Ok(images)
}

/// Whether a pinned reference names an image present locally. Catalog digests name a
/// multi-platform index, which Docker records against the pulled image too.
fn present(images: &[LocalImage], reference: &str) -> bool {
    let normalized = |r: &str| r.trim_start_matches("docker.io/library/").to_string();
    images
        .iter()
        .any(|image| normalized(&image.reference) == normalized(reference))
}

/// Pulls a digest-pinned image. Content-addressed, so the bytes cannot differ from the pin.
pub fn pull(docker: &Path, image: &str) -> Result<()> {
    let (name, digest) = image
        .split_once("@sha256:")
        .context("only digest-pinned images (name@sha256:<digest>) can be pulled here")?;
    ensure!(
        !name.is_empty()
            && !name.starts_with('-')
            && digest.len() == 64
            && digest.bytes().all(|b| b.is_ascii_hexdigit()),
        "only digest-pinned images (name@sha256:<64 hex digits>) can be pulled here"
    );
    docker_output(docker, &["pull", "--quiet", image], PULL_TIMEOUT_MS)?;
    Ok(())
}

/// Everything the settings page shows. `editable` says whether this caller may change it.
pub fn overview(root: &Path, editable: bool) -> Result<Value> {
    let docker = find_docker();
    let (version, images, docker_error) = match &docker {
        None => (None, Vec::new(), Some("Docker CLI not found".to_string())),
        Some(path) => match docker_output(
            path,
            &["version", "--format", "{{.Server.Version}}"],
            15_000,
        ) {
            Ok(version) => match local_images(path) {
                Ok(images) => (Some(version.trim().to_string()), images, None),
                Err(error) => (
                    Some(version.trim().to_string()),
                    Vec::new(),
                    Some(format!("{error:#}")),
                ),
            },
            Err(error) => (None, Vec::new(), Some(format!("{error:#}"))),
        },
    };
    let catalog: Vec<Value> = catalog()?
        .into_iter()
        .map(|entry| {
            let available = present(&images, &entry.image);
            let mut value = serde_json::to_value(&entry).unwrap_or_default();
            value["present"] = json!(available);
            value
        })
        .collect();
    let presets: Vec<Value> = AgentPresets::open(root)?
        .list()?
        .into_iter()
        .map(|preset| {
            json!({
                "name": preset.name,
                "repository": preset.repository,
                "backend": preset.backend.target(),
                "sandbox": preset.sandbox,
                "env_passthrough": preset.env_passthrough,
            })
        })
        .collect();
    Ok(json!({
        "editable": editable,
        "docker": {"path": docker, "server_version": version, "error": docker_error},
        "catalog": catalog,
        "local_images": images,
        "presets": presets,
        "levels": ["isolated", "network", "trusted_paths"],
        "backends": if cfg!(windows) { json!(["docker", "app_container"]) } else { json!(["docker"]) },
    }))
}

/// A preset's sandbox as the settings page submits it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxForm {
    #[serde(default)]
    pub clear: bool,
    #[serde(default)]
    pub image: String,
    #[serde(default)]
    pub command: Vec<String>,
    #[serde(default = "default_user")]
    pub user: String,
    #[serde(default = "default_level")]
    pub default_level: SandboxLevel,
    #[serde(default = "default_level")]
    pub ceiling: SandboxLevel,
    #[serde(default)]
    pub env_passthrough: Vec<String>,
    #[serde(default)]
    pub read_only_paths: Vec<PathBuf>,
    #[serde(default = "default_memory")]
    pub memory_mb: u32,
    #[serde(default = "default_cpus")]
    pub cpus: u32,
    #[serde(default = "default_pids")]
    pub pids: u32,
    /// `docker` (default) or `app_container` (Windows; image and command are ignored).
    #[serde(default = "default_backend")]
    pub backend: String,
}

fn default_backend() -> String {
    "docker".into()
}

fn default_user() -> String {
    "1000:1000".into()
}
fn default_level() -> SandboxLevel {
    SandboxLevel::Isolated
}
fn default_memory() -> u32 {
    2048
}
fn default_cpus() -> u32 {
    2
}
fn default_pids() -> u32 {
    512
}

/// Applies a form to `preset` with the discovered Docker CLI pinned. Same validation as
/// `agent-preset-sandbox`.
pub fn apply(root: &Path, preset: &str, form: SandboxForm) -> Result<Value> {
    let policy = if form.clear {
        None
    } else {
        let backend = match form.backend.trim().replace('-', "_").as_str() {
            "docker" => {
                let docker = find_docker().context("Docker CLI not found on this forge")?;
                SandboxBackend::Docker {
                    docker: crate::codex::Executable::new(&docker)?,
                    image: form.image.trim().to_string(),
                    command: form.command,
                    user: form.user.trim().to_string(),
                }
            }
            "app_container" => SandboxBackend::AppContainer { launcher: None },
            other => anyhow::bail!("unknown sandbox backend {other:?}"),
        };
        Some(SandboxPolicy {
            backend,
            default_level: form.default_level,
            ceiling: form.ceiling,
            env_passthrough: form
                .env_passthrough
                .iter()
                .map(|n| n.trim().to_string())
                .filter(|n| !n.is_empty())
                .collect(),
            read_only_paths: form.read_only_paths,
            memory_mb: form.memory_mb,
            cpus: form.cpus,
            pids: form.pids,
        })
    };
    let updated = AgentPresets::open(root)?.set_sandbox(preset, policy)?;
    Ok(json!({"name": updated.name, "sandbox": updated.sandbox}))
}

/// Sets the names a preset's worker and host verifier receive beyond the allowlist.
pub fn apply_env(root: &Path, preset: &str, passthrough: Vec<String>) -> Result<Value> {
    let updated = AgentPresets::open(root)?.set_env_passthrough(preset, passthrough)?;
    Ok(json!({"name": updated.name, "env_passthrough": updated.env_passthrough}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_built_in_catalog_is_pinned_and_consistent() {
        let entries = catalog().unwrap();
        assert!(entries.iter().any(|e| e.id == "shell"));
        for entry in &entries {
            let (_, digest) = entry
                .image
                .split_once("@sha256:")
                .expect("pinned by digest");
            assert_eq!(digest.len(), 64, "{}", entry.id);
            assert!(entry.default_level <= entry.ceiling, "{}", entry.id);
            assert!(!entry.command.is_empty(), "{}", entry.id);
            assert!((256..=65_536).contains(&entry.memory_mb), "{}", entry.id);
        }
        let mut ids: Vec<_> = entries.iter().map(|e| &e.id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), entries.len(), "catalog ids are unique");
    }

    #[test]
    fn presence_ignores_the_docker_hub_library_prefix() {
        let images = vec![LocalImage {
            reference: "alpine@sha256:abc".into(),
            repository: "alpine".into(),
            tag: "3.21".into(),
            size: "12MB".into(),
        }];
        assert!(present(&images, "alpine@sha256:abc"));
        assert!(present(&images, "docker.io/library/alpine@sha256:abc"));
        assert!(!present(&images, "alpine@sha256:def"));
    }

    #[test]
    fn only_digest_pinned_images_can_be_pulled() {
        let docker = std::env::current_exe().unwrap();
        for bad in [
            "alpine:latest",
            "alpine",
            "--help@sha256:abc",
            "alpine@sha256:short",
        ] {
            assert!(pull(&docker, bad).is_err(), "accepted {bad:?}");
        }
    }
}
