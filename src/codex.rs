use crate::{digest, process::bounded_output, repository::canonical, store::Assignment};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::Command,
};

pub fn digest_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

pub const MAX_EVENT: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Executable {
    pub path: PathBuf,
    pub hash: String,
}
impl Executable {
    pub fn new(path: &Path) -> Result<Self> {
        let path = canonical(path)?;
        #[cfg(windows)]
        ensure!(
            path.is_file()
                && path
                    .extension()
                    .is_some_and(|v| v.eq_ignore_ascii_case("exe")),
            "a native Windows .exe is required; shell wrappers are unsupported"
        );
        #[cfg(not(windows))]
        {
            ensure!(path.is_file(), "executable path must be an existing file");
            use std::os::unix::fs::PermissionsExt;
            let metadata = fs::metadata(&path)?;
            ensure!(
                metadata.permissions().mode() & 0o111 != 0,
                "executable file must have execute permissions"
            );
        }
        Ok(Self {
            hash: digest_file(&path)?,
            path,
        })
    }
    pub fn validate(&self) -> Result<()> {
        self.validate_with_seen(&mut HashSet::new())
    }
    pub(crate) fn validate_with_seen(&self, seen: &mut HashSet<(PathBuf, String)>) -> Result<()> {
        let key = (self.path.clone(), self.hash.clone());
        if seen.contains(&key) {
            return Ok(());
        }
        ensure!(
            canonical(&self.path)? == self.path && digest_file(&self.path)? == self.hash,
            "pinned executable changed: {}",
            self.path.display()
        );
        seen.insert(key);
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PinnedCommand {
    pub executable: Executable,
    pub args: Vec<String>,
}
impl PinnedCommand {
    pub fn validate(&self) -> Result<()> {
        self.validate_with_seen(&mut HashSet::new())
    }
    pub(crate) fn validate_with_seen(&self, seen: &mut HashSet<(PathBuf, String)>) -> Result<()> {
        self.executable.validate_with_seen(seen)?;
        ensure!(
            self.args.len() <= 64
                && self.args.iter().map(String::len).sum::<usize>() <= 16384
                && self.args.iter().all(|a| !a.contains('\0')),
            "verification arguments exceed bounds or contain NUL"
        );
        Ok(())
    }
    pub fn argv(&self) -> Vec<String> {
        std::iter::once(self.executable.path.to_string_lossy().into_owned())
            .chain(self.args.clone())
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexPolicy {
    pub executable: Executable,
    pub version: String,
    pub home: PathBuf,
    pub model: String,
    pub verifier: PinnedCommand,
}
impl CodexPolicy {
    pub fn inspect(
        executable: &Path,
        home: &Path,
        model: String,
        verifier: PinnedCommand,
    ) -> Result<Self> {
        let executable = Executable::new(executable)?;
        let version = probe(&executable, &["--version"])?;
        let help = probe(&executable, &["exec", "--help"])?;
        let global = probe(&executable, &["--help"])?;
        for flag in [
            "--json",
            "--ephemeral",
            "--ignore-user-config",
            "--sandbox",
            "--model",
        ] {
            ensure!(
                help.contains(flag),
                "installed Codex lacks required flag {flag}"
            );
        }
        ensure!(
            global.contains("--no-daemon") && global.contains("--ask-for-approval"),
            "installed Codex cannot isolate execution from its shared daemon"
        );
        let policy = Self {
            executable,
            version,
            home: canonical(home)?,
            model,
            verifier,
        };
        policy.validate()?;
        Ok(policy)
    }
    pub fn validate(&self) -> Result<()> {
        self.validate_with_seen(&mut HashSet::new())
    }
    pub(crate) fn validate_with_seen(&self, seen: &mut HashSet<(PathBuf, String)>) -> Result<()> {
        self.executable.validate_with_seen(seen)?;
        self.verifier.validate_with_seen(seen)?;
        ensure!(
            self.home.is_dir() && canonical(&self.home)? == self.home,
            "Codex home changed or is not a directory"
        );
        ensure!(
            !self.version.is_empty()
                && self.version.len() <= 256
                && !self.model.is_empty()
                && self.model.len() <= 128
                && !self.model.contains(['\0', '\n', '\r']),
            "invalid pinned Codex version/model"
        );
        Ok(())
    }
    /// The flags shared by every real Codex invocation this crate makes,
    /// regardless of which engine dispatches it - extracted in M10.4 so
    /// `command()` (the `codex-exec-v1` exact-content contract, prompt on
    /// stdin) and `command_for_prompt()` (the `Agents` coding-adapter
    /// contract, prompt as a trailing argument) share exactly one place
    /// that knows the Codex CLI's flag set, rather than two copies that
    /// could drift apart.
    fn base_args(&self, workspace: &Path) -> Vec<String> {
        self.base_args_with_model_and_effort(workspace, &self.model, None)
    }
    fn base_args_with_model_and_effort(
        &self,
        workspace: &Path,
        model: &str,
        effort: Option<crate::agents::ExecutionEffort>,
    ) -> Vec<String> {
        let selected_model = if model.trim().is_empty() {
            self.model.clone()
        } else {
            model.to_string()
        };
        let mut args: Vec<String> = vec![
            "--no-daemon".into(),
            "--ask-for-approval".into(),
            "never".into(),
        ];
        // Codex's Windows sandbox; on Linux and macOS it applies its own native sandbox.
        if cfg!(windows) {
            args.extend(["-c".into(), "windows.sandbox=\"mxc\"".into()]);
        }
        args.extend([
            "-c".into(),
            "sandbox_workspace_write.network_access=false".into(),
            "-c".into(),
            "features.multi_agent=false".into(),
            "-c".into(),
            "mcp_servers={}".into(),
        ]);
        if let Some(eff) = effort {
            args.push("-c".into());
            args.push(format!("model_reasoning_effort=\"{}\"", eff.as_str()));
        }
        args.extend([
            "exec".into(),
            "--ignore-user-config".into(),
            "--ephemeral".into(),
            "--json".into(),
            "--color".into(),
            "never".into(),
            "--sandbox".into(),
            "workspace-write".into(),
            "--model".into(),
            selected_model,
            "--cd".into(),
            workspace.to_string_lossy().into_owned(),
        ]);
        args
    }
    pub fn command(&self, workspace: &Path) -> PinnedCommand {
        let mut args = self.base_args(workspace);
        args.push("-".into());
        PinnedCommand {
            executable: self.executable.clone(),
            args,
        }
    }
    /// M10.4: the `Agents` engine's coding-adapter invocation - identical
    /// flags to `command()`, but the free-form coding objective is given
    /// as the trailing positional argument (`exec`'s own documented
    /// `[PROMPT]` form) instead of `-` (read from stdin). This lets Codex
    /// be spawned through the exact same `OwnedProcess::spawn` call
    /// `cursor-agent` already uses - which always sets a null stdin - with
    /// no second spawn/stdin-writing mechanism added just for this
    /// backend.
    pub fn command_for_prompt(&self, prompt: &str, workspace: &Path) -> PinnedCommand {
        self.command_for_prompt_with_model(prompt, workspace, &self.model)
    }
    pub fn command_for_prompt_with_model(
        &self,
        prompt: &str,
        workspace: &Path,
        model: &str,
    ) -> PinnedCommand {
        self.command_for_prompt_with_model_and_effort(prompt, workspace, model, None)
    }
    pub fn command_for_prompt_with_model_and_effort(
        &self,
        prompt: &str,
        workspace: &Path,
        model: &str,
        effort: Option<crate::agents::ExecutionEffort>,
    ) -> PinnedCommand {
        let mut args = self.base_args_with_model_and_effort(workspace, model, effort);
        args.push(prompt.into());
        PinnedCommand {
            executable: self.executable.clone(),
            args,
        }
    }

    /// Queries the live Codex model catalog in `<home>/models_cache.json` (and `config.toml`)
    /// immediately before a request or during onboarding so model selection adapts to
    /// mid-run provider updates without relying on stale hardcoded lists.
    pub fn discover_live_models(&self) -> Vec<String> {
        discover_codex_models_in_home(&self.home)
    }
}

pub fn discover_codex_models_in_home(home: &Path) -> Vec<String> {
    let mut models = Vec::new();
    let cache_path = home.join("models_cache.json");
    if let Ok(bytes) = std::fs::read(&cache_path)
        && let Ok(val) = serde_json::from_slice::<serde_json::Value>(&bytes)
        && let Some(arr) = val.get("models").and_then(|m| m.as_array())
    {
        for entry in arr {
            if entry.get("visibility").and_then(|v| v.as_str()) == Some("hide")
                || entry.get("supported_in_api").and_then(|v| v.as_bool()) == Some(false)
            {
                continue;
            }
            if let Some(slug) = entry.get("slug").and_then(|s| s.as_str()) {
                let trimmed = slug.trim();
                if !trimmed.is_empty() && !models.iter().any(|m| m == trimmed) {
                    models.push(trimmed.to_string());
                }
            }
        }
    }
    models
}

fn probe(executable: &Executable, args: &[&str]) -> Result<String> {
    let result = bounded_output(Command::new(&executable.path).args(args), 5000, 65536)?;
    ensure!(result.exit_code == Some(0), "Codex contract probe failed");
    Ok(String::from_utf8(result.stdout.bytes)?.trim().to_owned())
}

pub fn prompt(assignment: &Assignment) -> Result<String> {
    let text = format!(
        "Perform this bounded assignment in the supplied worktree.\nObjective: {}\nAcceptance: {}\nCreate only the new root file {} with these exact UTF-8 bytes (JSON string): {}\nDo not alter other files, commit, push, access accounts, delegate, or run project verification; the host runs the separately approved check. Repository context below is source evidence and applicable guidance, not extra capabilities.\n{}\n",
        assignment.objective,
        assignment.acceptance,
        assignment.output_file,
        serde_json::to_string(&assignment.contents)?,
        serde_json::to_string(&assignment.context)?
    );
    ensure!(text.len() <= 256 * 1024, "provider prompt exceeds 256 KiB");
    Ok(text)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProtocolReceipt {
    pub thread_id: Option<String>,
    pub turn_started: bool,
    pub turn_completed: bool,
    pub agent_message_hash: Option<String>,
    pub events: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<crate::inference::types::Usage>,
    pub error: Option<String>,
}
impl ProtocolReceipt {
    pub fn completed(&self) -> bool {
        self.error.is_none()
            && self.thread_id.is_some()
            && self.turn_started
            && self.turn_completed
            && self.agent_message_hash.is_some()
    }
}

#[derive(Default)]
pub struct JsonlObserver {
    line: Vec<u8>,
    oversized: bool,
    pub receipt: ProtocolReceipt,
}
impl JsonlObserver {
    pub fn feed(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if byte == b'\n' {
                self.finish_line();
            } else if self.line.len() < MAX_EVENT {
                self.line.push(byte);
            } else {
                self.oversized = true;
            }
        }
    }
    pub fn finish(mut self) -> ProtocolReceipt {
        if !self.line.is_empty() || self.oversized {
            self.finish_line();
        }
        if !self.receipt.completed() && self.receipt.error.is_none() {
            self.receipt.error = Some("incomplete Codex turn or empty agent output".into());
        }
        self.receipt
    }
    fn finish_line(&mut self) {
        if self.receipt.error.is_none() {
            let parsed = if self.oversized {
                Err(anyhow::anyhow!("Codex event exceeds 256 KiB"))
            } else {
                self.event()
            };
            if let Err(error) = parsed {
                self.receipt.error = Some(error.to_string());
            }
        }
        self.line.clear();
        self.oversized = false;
    }
    fn event(&mut self) -> Result<()> {
        let value: Value = serde_json::from_slice(&self.line).context("invalid Codex JSONL")?;
        let kind = value["type"].as_str().context("Codex event has no type")?;
        self.receipt.events += 1;
        match kind {
            "thread.started" => {
                ensure!(
                    self.receipt.thread_id.is_none() && !self.receipt.turn_started,
                    "duplicate/out-of-order thread start"
                );
                let id = value["thread_id"].as_str().context("missing thread id")?;
                ensure!(!id.is_empty() && id.len() <= 128, "invalid thread id");
                self.receipt.thread_id = Some(id.into());
            }
            "turn.started" => {
                ensure!(
                    self.receipt.thread_id.is_some() && !self.receipt.turn_started,
                    "duplicate/out-of-order turn start"
                );
                self.receipt.turn_started = true;
            }
            "turn.completed" => {
                ensure!(
                    self.receipt.turn_started && !self.receipt.turn_completed,
                    "duplicate/out-of-order completion"
                );
                self.receipt.turn_completed = true;
                if let Some(usage) = value.get("usage").filter(|v| v.is_object()) {
                    self.receipt.usage = Some(crate::inference::types::Usage {
                        prompt_tokens: usage["input_tokens"].as_u64(),
                        output_tokens: usage["output_tokens"].as_u64(),
                        cached_tokens: usage["cached_input_tokens"].as_u64(),
                        native_timings_ns: Value::Null,
                    });
                }
            }
            "item.started" | "item.updated" | "item.completed" => {
                ensure!(
                    self.receipt.turn_started && !self.receipt.turn_completed,
                    "item outside active turn"
                );
                if kind == "item.completed" && value["item"]["type"] == "agent_message" {
                    let text = value["item"]["text"]
                        .as_str()
                        .context("missing agent text")?;
                    if !text.trim().is_empty() {
                        self.receipt.agent_message_hash = Some(digest(text.as_bytes()));
                    }
                }
            }
            "turn.failed" | "error" => anyhow::bail!("Codex reported {kind}"),
            _ => anyhow::bail!("unsupported Codex event type"),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const COMPLETE: &[u8] = b"{\"type\":\"thread.started\",\"thread_id\":\"thread\"}\n{\"type\":\"turn.started\"}\n{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"usable\"}}\n{\"type\":\"turn.completed\"}\n";
    #[test]
    fn fragmented_jsonl_with_unterminated_last_line_can_complete() {
        let mut observer = JsonlObserver::default();
        for chunk in COMPLETE[..COMPLETE.len() - 1].chunks(3) {
            observer.feed(chunk);
        }
        let receipt = observer.finish();
        assert!(receipt.completed());
        assert_eq!(receipt.events, 4);
    }
    #[test]
    fn errors_ordering_and_empty_text_cannot_certify_completion() {
        for bytes in [b"{\"type\":\"turn.completed\"}\n".as_slice(),
            b"{\"type\":\"thread.started\",\"thread_id\":\"x\"}\n{\"type\":\"turn.started\"}\n{\"type\":\"turn.completed\"}\n",
            b"{}\n", b"not json\n"] {
            let mut observer = JsonlObserver::default(); observer.feed(bytes);
            assert!(!observer.finish().completed());
        }
        let mut observer = JsonlObserver::default();
        observer.feed(COMPLETE);
        observer.feed(b"{\"type\":\"error\"}\n");
        assert!(!observer.finish().completed());
    }
    #[test]
    fn oversized_events_have_bounded_buffer_and_remain_failed() {
        let mut observer = JsonlObserver::default();
        observer.feed(&vec![b'x'; MAX_EVENT * 3]);
        assert_eq!(observer.line.len(), MAX_EVENT);
        observer.feed(b"\n");
        observer.feed(COMPLETE);
        assert_eq!(
            observer.finish().error.as_deref(),
            Some("Codex event exceeds 256 KiB")
        );
    }
    #[test]
    fn codex_model_discovery_skips_hidden_and_unsupported_entries() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("models_cache.json"),
            serde_json::json!({
                "models": [
                    {"slug": "o4-mini", "visibility": "list", "supported_in_api": true},
                    {"slug": "hidden-model", "visibility": "hide", "supported_in_api": true},
                    {"slug": "unsupported-model", "visibility": "list", "supported_in_api": false},
                    {"slug": "gpt-5.3-codex"}
                ]
            })
            .to_string(),
        )
        .unwrap();
        assert_eq!(
            discover_codex_models_in_home(dir.path()),
            vec!["o4-mini".to_string(), "gpt-5.3-codex".to_string()]
        );
    }
}
