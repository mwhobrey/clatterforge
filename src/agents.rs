//! M10.1 Phase 5: a minimal, supervised engine for delegating one bounded
//! coding task to an *externally autonomous* coding-agent CLI (first
//! backend: `cursor-agent`; M10.4 adds `codex` as a second, genuine
//! coding backend - see `AgentBackend`), instead of a local-model chat
//! completion. It
//! deliberately is not a second agent framework and does not reason about
//! the task itself - the external agent already does that. What this
//! module adds is the same supervision every other engine in this crate
//! already provides: an isolated worktree, a strictly bounded file scope
//! checked *after* the agent runs (an agent's own edits are trusted no
//! more than a local model's), a pinned, independent verifier the agent
//! cannot influence, and a hard stop at `AwaitingReview` - nothing here
//! ever accepts a result. "Never trust an agent's self-reported success"
//! means exactly that: this module never even *parses* the agent's stdout
//! for a verdict, only for evidence; only the independent verifier's exit
//! code can produce `AwaitingReview`.
//!
//! Each approved task gets one bounded attempt; failure never auto-replays or
//! integrates changes. TaskService supplies dependency admission, while Agents::run
//! owns the repository lease. Host routing events record the handlers and process
//! launches separately from verification and human acceptance.

use crate::{
    codex::{Executable, PinnedCommand},
    coordination::Coordination,
    now, open_sqlite_with_retry,
    orchestration::{safe_file, validate_path},
    process::{
        OwnedProcess, Presence, ProcessControl, ProcessIdentity, bounded_output, current_identity,
        presence,
    },
    repository::{Repository, canonical, git},
    runner::{artifact, new_file_diff, process_receipt},
    stop::GlobalStop,
    store::{Approval, Artifact, ProcessReceipt, Review},
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::Command,
};
use uuid::Uuid;

// Protected inputs are hashed, never put into the model's bounded editable-text
// context. Lockfiles commonly exceed that context's 64 KiB limit.
fn bounded_stream_snippet(path: &Path, max_chars: usize) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut out: String = trimmed.chars().take(max_chars).collect();
    if trimmed.chars().count() > max_chars {
        out.push('…');
    }
    Some(out)
}

fn verification_file_hash(path: &Path) -> Result<Option<String>> {
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 8192];
    let mut total = 0;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            return Ok(Some(format!("{:x}", hash.finalize())));
        }
        total += read;
        ensure!(
            total <= 16 * 1024 * 1024,
            "protected verification file exceeds 16 MiB"
        );
        hash.update(&buffer[..read]);
    }
}

/// The authored, immutable specification for one bounded external-agent
/// coding task - deliberately the same shape of commitment
/// `orchestration::Spec` makes: a fixed objective/acceptance, a strictly
/// bounded file scope, a pinned independent verifier, and finite
/// timeouts. `target`/`model` name the backend and model the task is
/// authorized against; today the only implemented backend is
/// `"cursor-agent"`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agentlet: Option<crate::agentlet::Definition>,
    pub repository: PathBuf,
    pub objective: String,
    pub acceptance: String,
    pub allowed_files: Vec<String>,
    pub verification_files: Vec<String>,
    pub verify_executable: PathBuf,
    pub verify_args: Vec<String>,
    pub target: String,
    pub model: String,
    #[serde(default = "default_generation_deadline")]
    pub generation_timeout_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation_idle_timeout_ms: Option<u64>,
    #[serde(default = "default_verification_deadline")]
    pub verification_timeout_ms: u64,
    #[serde(default, skip_serializing_if = "crate::agent_progress::is_false")]
    pub soft_timeouts: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<ExecutionEffort>,
}

/// Reasoning/execution effort level requested for a coding-agent invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionEffort {
    Low,
    Medium,
    High,
    XHigh,
}

impl ExecutionEffort {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
        }
    }

    pub fn for_complexity(complexity: &str) -> Self {
        match complexity {
            "boilerplate" => Self::Low,
            "complex" => Self::High,
            _ => Self::Medium,
        }
    }

    pub fn step_up(self) -> Self {
        match self {
            Self::Low => Self::Medium,
            Self::Medium => Self::High,
            Self::High | Self::XHigh => Self::High,
        }
    }
}
fn default_generation_deadline() -> u64 {
    120_000
}
fn default_verification_deadline() -> u64 {
    30_000
}

/// `cursor-agent`'s real Windows entry point is `node.exe index.js agent
/// ...` (its own `.cmd`/`.ps1` launchers are thin wrappers around exactly
/// that - confirmed by reading the installed launcher script), which
/// gives a genuine native `.exe` to hash-pin, consistent with
/// `Executable::new`'s existing "no shell wrappers" contract used
/// throughout this crate - nothing new was relaxed to support this
/// backend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CursorAgentPolicy {
    pub node: Executable,
    pub index_js: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index_hash: Option<String>,
    pub version: String,
    pub model: String,
}
impl CursorAgentPolicy {
    pub fn inspect(node: &Path, index_js: &Path, model: String) -> Result<Self> {
        let node = Executable::new(node)?;
        let index_js = canonical(index_js)?;
        ensure!(
            index_js.is_file() && index_js.extension().is_some_and(|v| v == "js"),
            "cursor-agent index.js not found"
        );
        ensure!(
            !model.trim().is_empty() && model.len() <= 128 && !model.contains(['\0', '\n', '\r']),
            "invalid pinned cursor-agent model"
        );
        let version = probe(&node, &index_js, &["--version"])?;
        // Not `["agent", "--help"]`: cursor-agent's own CLI treats "agent"
        // as a distinct, much narrower subcommand (confirmed live against
        // the real installed CLI - its own `--help` only lists `-h`) that
        // is *not* the same invocation as the documented top-level
        // `agent -p "..." --model ...` form, where "agent" names the
        // executable itself, not a subcommand argument. The top-level
        // `--help` is the one that actually documents `--print`/
        // `--output-format`/etc., and is also what `command()` below
        // invokes - no subcommand word in the real argv either.
        let help = probe(&node, &index_js, &["--help"])?;
        for flag in [
            "--print",
            "--output-format",
            "--model",
            "--workspace",
            "--sandbox",
        ] {
            ensure!(
                help.contains(flag),
                "installed cursor-agent lacks required flag {flag}"
            );
        }
        let policy = Self {
            node,
            index_hash: Some(crate::digest(&fs::read(&index_js)?)),
            index_js,
            version,
            model,
        };
        policy.validate()?;
        Ok(policy)
    }
    pub fn validate(&self) -> Result<()> {
        self.validate_with_seen(&mut std::collections::HashSet::new())
    }
    pub(crate) fn validate_with_seen(
        &self,
        seen: &mut std::collections::HashSet<(PathBuf, String)>,
    ) -> Result<()> {
        self.node.validate_with_seen(seen)?;
        if let Some(hash) = &self.index_hash {
            let key = (self.index_js.clone(), hash.clone());
            if !seen.contains(&key) {
                ensure!(
                    canonical(&self.index_js)? == self.index_js && self.index_js.is_file(),
                    "pinned cursor-agent index.js changed or is missing"
                );
                ensure!(
                    crate::codex::digest_file(&self.index_js)? == *hash,
                    "pinned cursor-agent entry point changed"
                );
                seen.insert(key);
            }
        } else {
            ensure!(
                canonical(&self.index_js)? == self.index_js && self.index_js.is_file(),
                "pinned cursor-agent index.js changed or is missing"
            );
        }
        ensure!(
            !self.version.is_empty() && self.version.len() <= 256,
            "invalid pinned cursor-agent version"
        );
        Ok(())
    }
    /// Deliberately omits `--force`/`--yolo` (which would bypass
    /// cursor-agent's own internal guardrails) and omits any auto-accept
    /// flag - `--print` alone is documented for non-interactive/CI use and
    /// is what every other safety property here is layered on top of.
    /// `--trust` only waives cursor-agent's own workspace-trust prompt for
    /// a workspace this orchestrator already prepared and authorized -
    /// not a widening of what the agent may touch, which `scope()` below
    /// still enforces independently either way.
    ///
    /// M10.2 Phase 4 found live, against the real installed binary:
    /// `--sandbox enabled` refuses outright on Windows ("Sandbox requires
    /// macOS or Linux") - cursor-agent's own OS-level sandbox simply isn't
    /// available there. `disabled` is not a weaker *clatterforge* policy;
    /// per cursor-agent's own error message it switches the CLI to its
    /// separate, Windows-compatible "allowlist mode" instead of leaving it
    /// with no internal guard at all. Either way, this module's own
    /// boundary - the isolated worktree, the post-run file-scope check,
    /// the independent verifier, mandatory human review - is unchanged
    /// and is the real safety enforcement regardless of what cursor-agent
    /// does internally.
    pub fn command(&self, prompt: &str, workspace: &Path) -> PinnedCommand {
        self.command_with_model(prompt, workspace, &self.model)
    }
    fn command_with_model(&self, prompt: &str, workspace: &Path, model: &str) -> PinnedCommand {
        let selected_model = if model.trim().is_empty() {
            self.model.clone()
        } else {
            model.to_string()
        };
        PinnedCommand {
            executable: self.node.clone(),
            args: vec![
                self.index_js.to_string_lossy().into_owned(),
                "-p".into(),
                prompt.into(),
                "--output-format".into(),
                "json".into(),
                "--model".into(),
                selected_model,
                "--workspace".into(),
                workspace.to_string_lossy().into_owned(),
                "--sandbox".into(),
                "disabled".into(),
                "--trust".into(),
            ],
        }
    }
    /// Queries `cursor-agent --list-models` right before a request or during onboarding
    /// to obtain the live list of models entitled for the active account.
    pub fn discover_live_models(&self) -> Vec<String> {
        discover_cursor_models(&self.node.path, &self.index_js)
    }
}

/// How long one `--list-models` answer is reused. Routing asks for the catalog several times
/// per decision (once per candidate worker and tier); without this each ask spawned a fresh
/// CLI process with its own multi-second deadline.
const CURSOR_MODEL_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(30);

type CursorModelCache = std::sync::Mutex<
    std::collections::HashMap<(PathBuf, PathBuf), (std::time::Instant, Vec<String>)>,
>;

static CURSOR_CACHE: std::sync::OnceLock<CursorModelCache> = std::sync::OnceLock::new();

pub fn clear_cursor_model_cache() {
    if let Some(cache) = CURSOR_CACHE.get()
        && let Ok(mut entries) = cache.lock()
    {
        entries.clear();
    }
}

pub fn discover_cursor_models(node: &Path, index_js: &Path) -> Vec<String> {
    let cache = CURSOR_CACHE.get_or_init(Default::default);
    let key = (node.to_path_buf(), index_js.to_path_buf());
    if let Ok(entries) = cache.lock()
        && let Some((at, models)) = entries.get(&key)
        && at.elapsed() < CURSOR_MODEL_CACHE_TTL
    {
        return models.clone();
    }
    let models = discover_cursor_models_uncached(node, index_js);
    // An empty answer (CLI missing, offline, timed out) is cached too: retrying it on every
    // lookup is exactly the stall this cache exists to prevent.
    if let Ok(mut entries) = cache.lock() {
        entries.insert(key, (std::time::Instant::now(), models.clone()));
    }
    models
}

fn discover_cursor_models_uncached(node: &Path, index_js: &Path) -> Vec<String> {
    let mut command = cursor_process(node);
    command.arg(index_js).arg("--list-models");
    let Ok(result) = bounded_output(&mut command, 6_000, 65536) else {
        return Vec::new();
    };
    if result.exit_code != Some(0) {
        return Vec::new();
    }
    let Ok(text) = String::from_utf8(result.stdout.bytes) else {
        return Vec::new();
    };
    parse_cursor_list_models_output(&text)
}

pub fn parse_cursor_list_models_output(text: &str) -> Vec<String> {
    let mut models = Vec::new();
    for raw_line in text.lines() {
        // Strip basic ANSI escape sequences if present.
        let mut clean = String::with_capacity(raw_line.len());
        let mut chars = raw_line.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '\x1b' {
                if chars.peek() == Some(&'[') {
                    chars.next();
                    for c in chars.by_ref() {
                        if c.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
            } else if !ch.is_control() {
                clean.push(ch);
            }
        }
        let line = clean.trim();
        if line.is_empty()
            || line.eq_ignore_ascii_case("available models")
            || line.starts_with("Loading")
        {
            continue;
        }
        if let Some((slug, _desc)) = line.split_once(" - ") {
            let slug = slug.trim();
            if !slug.is_empty()
                && !slug.contains(' ')
                && slug.len() <= 128
                && !models.iter().any(|m| m == slug)
            {
                models.push(slug.to_string());
            }
        }
    }
    models
}

fn cursor_process(executable: &Path) -> Command {
    let mut command = Command::new(executable);
    if cfg!(windows) {
        // Cursor builds Windows hooks as PowerShell but selects Bash when MSYSTEM is inherited.
        command.env_remove("MSYSTEM").env("SHELL", "powershell.exe");
    }
    command
}

/// Pinned Google Antigravity CLI (`agy`) coding backend policy.
///
/// Runs `agy -p <prompt> --output-format stream-json --model <model> --mode accept-edits --dangerously-skip-permissions`
/// inside the isolated Git worktree (or `--mode plan --dangerously-skip-permissions` for read-only
/// review agentlets). The binary is hash-pinned at inspection time, and the same
/// worktree/file-scope/verifier/human-review boundaries apply independently of what `agy` does internally.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgyAgentPolicy {
    pub executable: Executable,
    pub version: String,
    pub model: String,
}

impl AgyAgentPolicy {
    pub fn inspect(executable: &Path, model: String) -> Result<Self> {
        let executable = Executable::new(executable)?;
        ensure!(
            !model.trim().is_empty() && model.len() <= 128 && !model.contains(['\0', '\n', '\r']),
            "invalid pinned agy model"
        );
        let mut ver_cmd = Command::new(&executable.path);
        ver_cmd.arg("--version");
        let ver_res = bounded_output(&mut ver_cmd, 10_000, 16_384)?;
        ensure!(ver_res.exit_code == Some(0), "agy --version probe failed");
        let version = String::from_utf8(ver_res.stdout.bytes)?.trim().to_owned();
        ensure!(
            !version.is_empty() && version.len() <= 256,
            "invalid agy version output"
        );

        // `agy --help` prints flag usage to stderr and may exit non-zero (Go `flag` behavior),
        // so inspect combined stdout+stderr for the required headless contract flags.
        let mut help_cmd = Command::new(&executable.path);
        help_cmd.arg("--help");
        let help_res = bounded_output(&mut help_cmd, 10_000, 65_536)?;
        let help = format!(
            "{}\n{}",
            String::from_utf8_lossy(&help_res.stdout.bytes),
            String::from_utf8_lossy(&help_res.stderr.bytes)
        );
        for flag in [
            "--print",
            "--output-format",
            "--model",
            "--mode",
            "--dangerously-skip-permissions",
        ] {
            ensure!(
                help.contains(flag),
                "installed agy CLI lacks required flag {flag}"
            );
        }

        let policy = Self {
            executable,
            version,
            model,
        };
        policy.validate()?;
        Ok(policy)
    }

    pub fn validate(&self) -> Result<()> {
        self.validate_with_seen(&mut std::collections::HashSet::new())
    }
    pub(crate) fn validate_with_seen(
        &self,
        seen: &mut std::collections::HashSet<(PathBuf, String)>,
    ) -> Result<()> {
        self.executable.validate_with_seen(seen)?;
        ensure!(
            !self.version.is_empty() && self.version.len() <= 256,
            "invalid pinned agy version"
        );
        ensure!(
            !self.model.trim().is_empty()
                && self.model.len() <= 128
                && !self.model.contains(['\0', '\n', '\r']),
            "invalid pinned agy model"
        );
        Ok(())
    }

    pub fn command(&self, prompt: &str, workspace: &Path) -> PinnedCommand {
        self.command_with_model(prompt, workspace, &self.model)
    }

    fn command_with_model(&self, prompt: &str, _workspace: &Path, model: &str) -> PinnedCommand {
        self.command_with_model_and_effort(prompt, _workspace, model, None)
    }

    fn command_with_model_and_effort(
        &self,
        prompt: &str,
        _workspace: &Path,
        model: &str,
        effort: Option<ExecutionEffort>,
    ) -> PinnedCommand {
        let selected_model = if model.trim().is_empty() {
            self.model.clone()
        } else {
            model.to_string()
        };
        let mut args = vec![
            "-p".into(),
            prompt.into(),
            "--output-format".into(),
            "stream-json".into(),
            "--model".into(),
            selected_model,
            "--mode".into(),
            "accept-edits".into(),
            "--dangerously-skip-permissions".into(),
        ];
        if let Some(eff) = effort {
            args.push("--effort".into());
            args.push(eff.as_str().into());
        }
        PinnedCommand {
            executable: self.executable.clone(),
            args,
        }
    }

    pub fn discover_live_models(&self) -> Vec<String> {
        discover_agy_models(&self.executable.path)
    }
}

const AGY_MODEL_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(30);

type AgyModelCache =
    std::sync::Mutex<std::collections::HashMap<PathBuf, (std::time::Instant, Vec<String>)>>;

static AGY_CACHE: std::sync::OnceLock<AgyModelCache> = std::sync::OnceLock::new();

pub fn clear_agy_model_cache() {
    if let Some(cache) = AGY_CACHE.get()
        && let Ok(mut entries) = cache.lock()
    {
        entries.clear();
    }
}

pub fn discover_agy_models(executable: &Path) -> Vec<String> {
    let cache = AGY_CACHE.get_or_init(Default::default);
    let key = executable.to_path_buf();
    if let Ok(entries) = cache.lock()
        && let Some((at, models)) = entries.get(&key)
        && at.elapsed() < AGY_MODEL_CACHE_TTL
    {
        return models.clone();
    }
    let models = discover_agy_models_uncached(executable);
    if let Ok(mut entries) = cache.lock() {
        entries.insert(key, (std::time::Instant::now(), models.clone()));
    }
    models
}

fn discover_agy_models_uncached(executable: &Path) -> Vec<String> {
    let mut command = Command::new(executable);
    command.arg("models");
    let Ok(result) = bounded_output(&mut command, 6_000, 65_536) else {
        return Vec::new();
    };
    if result.exit_code != Some(0) {
        return Vec::new();
    }
    let Ok(text) = String::from_utf8(result.stdout.bytes) else {
        return Vec::new();
    };
    parse_agy_models_output(&text)
}

/// Parses `agy models` output (`<slug>\t<Display Name>` per line) into deduplicated model slugs.
pub fn parse_agy_models_output(text: &str) -> Vec<String> {
    let mut models = Vec::new();
    for raw_line in text.lines() {
        let mut clean = String::with_capacity(raw_line.len());
        let mut chars = raw_line.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '\x1b' {
                if chars.peek() == Some(&'[') {
                    chars.next();
                    for c in chars.by_ref() {
                        if c.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
            } else if ch == '\t' || !ch.is_control() {
                clean.push(ch);
            }
        }
        let line = clean.trim();
        if line.is_empty()
            || line.starts_with("Fetching")
            || line.starts_with("Loading")
            || line.eq_ignore_ascii_case("available models")
        {
            continue;
        }
        let slug = line
            .split_once('\t')
            .map(|(s, _)| s)
            .or_else(|| line.split_once(" - ").map(|(s, _)| s))
            .or_else(|| line.split_whitespace().next())
            .unwrap_or("")
            .trim();
        if !slug.is_empty()
            && slug.len() <= 128
            && slug
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
            && slug.contains('-')
            && !models.iter().any(|m| m == slug)
        {
            models.push(slug.to_string());
        }
    }
    models
}

/// Cost and capability tier for model/CLI routing.
/// `Local` (zero API cost, local Ollama model) < `Fast` (economy/fast CLI models)
/// < `Standard` (balanced general models) < `Frontier` (deep reasoning / frontier models).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTier {
    Local,
    Fast,
    Standard,
    Frontier,
}
impl ModelTier {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Fast => "fast",
            Self::Standard => "standard",
            Self::Frontier => "frontier",
        }
    }
    pub fn infer(target: &str, model: &str) -> Self {
        let m = model.trim().to_ascii_lowercase();
        // Match whole slug segments ("gpt-5-mini" -> ["gpt", "5", "mini"]) rather than raw
        // substrings, so "gemini" is not read as "mini" and "sapphire" is not read as "phi".
        // A family keyword may carry a version suffix inside its segment ("qwen2", "phi4").
        let segments: Vec<&str> = m
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|s| !s.is_empty())
            .collect();
        let has = |keyword: &str| segments.contains(&keyword);
        let has_family = |keyword: &str| {
            segments.iter().any(|s| {
                s.strip_prefix(keyword)
                    .is_some_and(|rest| rest.chars().all(|c| c.is_ascii_digit()))
            })
        };
        if target == "local-model"
            || [
                "qwen",
                "gemma",
                "llama",
                "codellama",
                "mistral",
                "starcoder",
                "phi",
                "local",
            ]
            .iter()
            .any(|k| has_family(k))
            || m.contains("deepseek-coder")
        {
            return Self::Local;
        }
        // Fast / Economy models across Cursor and Codex (both classic and live 2026 slugs)
        if m == "auto"
            || m == "codex-auto-review"
            || m == "composer-2.5-fast"
            || m == "gpt-5.2-fast"
            || m.ends_with("-low")
            || m.ends_with("-low-fast")
            || [
                "mini", "haiku", "flash", "nano", "luna", "reserve", "small", "lite", "instant",
                "minimal", "none",
            ]
            .iter()
            .any(|k| has(k))
        {
            return Self::Fast;
        }
        // Frontier / Deep Reasoning models across Cursor and Codex
        if m == "gpt-5"
            || m == "gpt-5.3-codex"
            || m == "o3"
            || m == "o1"
            || m.contains("extra-high")
            || [
                "thinking", "opus", "astra", "frontier", "xhigh", "max", "high",
            ]
            .iter()
            .any(|k| has(k))
        {
            return Self::Frontier;
        }
        Self::Standard
    }
}

/// Supervised local-model worker policy (`target: "local-model"`).
/// Executes simple/boilerplate coding tasks inside the isolated worktree via a pinned
/// local model (e.g. Ollama `qwen2.5-coder:7b` or `gemma4:26b`) at zero external API cost.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalAgentPolicy {
    pub executable: Executable,
    pub endpoint: String,
    pub model: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_artifact_digest: Option<String>,
}
impl LocalAgentPolicy {
    pub fn inspect(
        executable: &Path,
        endpoint: String,
        model: String,
        expected_artifact_digest: Option<String>,
    ) -> Result<Self> {
        let executable = Executable::new(executable)?;
        let policy = Self {
            executable,
            endpoint,
            model,
            version: "local-agent-v1".into(),
            expected_artifact_digest,
        };
        policy.validate()?;
        Ok(policy)
    }
    pub fn validate(&self) -> Result<()> {
        self.validate_with_seen(&mut std::collections::HashSet::new())
    }
    pub(crate) fn validate_with_seen(
        &self,
        seen: &mut std::collections::HashSet<(PathBuf, String)>,
    ) -> Result<()> {
        self.executable.validate_with_seen(seen)?;
        ensure!(
            !self.endpoint.trim().is_empty() && reqwest::Url::parse(&self.endpoint).is_ok(),
            "invalid local-model worker endpoint"
        );
        ensure!(
            !self.model.trim().is_empty()
                && self.model.len() <= 128
                && !self.model.contains(['\0', '\n', '\r']),
            "invalid pinned local worker model"
        );
        ensure!(
            !self.version.is_empty() && self.version.len() <= 256,
            "invalid local worker version"
        );
        Ok(())
    }
    fn command_with_model(&self, prompt: &str, workspace: &Path, model: &str) -> PinnedCommand {
        let selected_model = if model.trim().is_empty() {
            self.model.clone()
        } else {
            model.to_string()
        };
        let is_conductor_bin = self
            .executable
            .path
            .file_stem()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.starts_with("dev-conductor") || s.starts_with("clatterforge"));
        let first_arg = if is_conductor_bin {
            "__local-agent-worker"
        } else {
            "--local-worker"
        };
        let mut args = vec![
            first_arg.into(),
            "-p".into(),
            prompt.into(),
            "--output-format".into(),
            "json".into(),
            "--model".into(),
            selected_model,
            "--workspace".into(),
            workspace.to_string_lossy().into_owned(),
            "--endpoint".into(),
            self.endpoint.clone(),
        ];
        if let Some(digest) = &self.expected_artifact_digest {
            args.push("--expected-digest".into());
            args.push(digest.clone());
        }
        PinnedCommand {
            executable: self.executable.clone(),
            args,
        }
    }
}

/// Runs the built-in local-model coding worker (`clatterforge __local-agent-worker ...`).
pub fn run_local_agent_worker(args: &[String]) -> Result<i32> {
    let flag = |name: &str| -> Option<String> {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let prompt = flag("-p").context("missing -p prompt")?;
    let model = flag("--model").context("missing --model")?;
    let workspace = PathBuf::from(flag("--workspace").context("missing --workspace")?);
    let endpoint = flag("--endpoint").unwrap_or_else(|| "http://127.0.0.1:11434".into());
    let expected_digest = flag("--expected-digest").filter(|d| !d.trim().is_empty());

    // Collect explicitly passed `--allowed-file <path>` flags first (`O2`), falling back to
    // prompt extraction only when invoked manually without explicit flags.
    let mut allowed_files: Vec<String> = Vec::new();
    let mut arg_iter = args.iter();
    while let Some(arg) = arg_iter.next() {
        if arg == "--allowed-file"
            && let Some(val) = arg_iter.next()
        {
            let trimmed = val.trim();
            if !trimmed.is_empty() && !allowed_files.iter().any(|existing| existing == trimmed) {
                allowed_files.push(trimmed.to_string());
            }
        }
    }
    if allowed_files.is_empty()
        && let Some(idx) = prompt.find("\"allowed_files\":[")
    {
        let rest = &prompt[idx + "\"allowed_files\":".len()..];
        if let Some(end) = rest.find(']')
            && let Ok(parsed) = serde_json::from_str::<Vec<String>>(&rest[..=end])
        {
            allowed_files = parsed;
        }
    }
    if allowed_files.is_empty()
        && let Some(idx) = prompt.find("relative to the workspace root, and nothing else: ")
    {
        let rest = &prompt[idx + "relative to the workspace root, and nothing else: ".len()..];
        if let Some(end) = rest.find(". Do not commit") {
            allowed_files = rest[..end]
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }
    }

    let mut file_context = serde_json::Map::new();
    for rel in &allowed_files {
        if let Ok(full) = safe_file(&workspace, rel)
            && full.is_file()
            && let Ok(text) = fs::read_to_string(&full)
        {
            file_context.insert(rel.clone(), serde_json::Value::String(text));
        }
    }

    let system_prompt = "You are a local coding worker inside Clatterforge. Return ONLY one JSON object with no markdown fences or extra commentary: {\"summary\":\"what you changed\",\"files\":[{\"path\":\"relative/path\",\"contents\":\"full new file contents\"}]}. Only edit files in the allowed_files list.";
    let user_payload = serde_json::json!({
        "task_prompt": prompt,
        "allowed_files": allowed_files,
        "current_files": file_context,
    });

    let base_endpoint = endpoint.trim_end_matches('/');
    let tags_url = format!("{base_endpoint}/api/tags");
    let chat_url = format!("{base_endpoint}/api/chat");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let body_text = rt.block_on(async {
        let client = reqwest::Client::builder()
            // The host's approved generation/idle limits are the real bound and kill this
            // process when exceeded. This is only a backstop, set above the longest onboard
            // profile (600 s low-spec) so it never undercuts an approved budget.
            .timeout(std::time::Duration::from_secs(660))
            .build()?;

        // Enforce the pinned artifact digest (`O1`) before dispatching generation.
        if let Some(expected) = &expected_digest {
            let normalize = |d: &str| d.trim().strip_prefix("sha256:").unwrap_or(d.trim()).to_ascii_lowercase();
            let expected_norm = normalize(expected);
            let tags_resp = client
                .get(&tags_url)
                .send()
                .await
                .context("local model digest check (/api/tags) failed")?;
            ensure!(
                tags_resp.status().is_success(),
                "local model /api/tags returned HTTP {}",
                tags_resp.status()
            );
            let tags_json: serde_json::Value = tags_resp
                .json()
                .await
                .context("invalid JSON from /api/tags during digest check")?;
            let models = tags_json
                .get("models")
                .and_then(|v| v.as_array())
                .context("missing 'models' array in /api/tags response")?;
            let matched = models.iter().find(|item| {
                item.get("name").and_then(|v| v.as_str()).is_some_and(|name| {
                    name == model || (!model.contains(':') && name == format!("{model}:latest"))
                })
            });
            let Some(item) = matched else {
                anyhow::bail!(
                    "local worker model '{}' not found in /api/tags during artifact digest verification",
                    model
                );
            };
            let actual_digest = item
                .get("digest")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            ensure!(
                normalize(actual_digest) == expected_norm,
                "local worker model '{}' artifact digest mismatch: expected {}, got {}",
                model,
                expected,
                actual_digest
            );
        }

        let mut resp = client
            .post(&chat_url)
            .json(&serde_json::json!({
                "model": model,
                "stream": true,
                "options": { "temperature": 0.0 },
                "messages": [
                    { "role": "system", "content": system_prompt },
                    { "role": "user", "content": serde_json::to_string(&user_payload)? }
                ]
            }))
            .send()
            .await
            .context("local model HTTP request failed")?;
        ensure!(
            resp.status().is_success(),
            "local model endpoint returned HTTP {}",
            resp.status()
        );
        let mut raw_bytes = Vec::new();
        while let Some(chunk) = resp.chunk().await? {
            ensure!(
                raw_bytes.len() + chunk.len() <= 4 * 1024 * 1024,
                "local model response exceeds 4 MiB"
            );
            raw_bytes.extend_from_slice(&chunk);
            // Emit progress to stderr so the host inactivity monitor observes live token streaming (`O2`).
            eprintln!("local-worker: streamed {} bytes", raw_bytes.len());
        }
        Ok::<String, anyhow::Error>(String::from_utf8(raw_bytes)?)
    })?;
    // Support both non-streaming JSON object and newline-delimited streaming chunks (as in mock Model server)
    let mut content = String::new();
    for line in body_text.lines().filter(|l| !l.trim().is_empty()) {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(line)
            && let Some(piece) = val
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_str())
        {
            content.push_str(piece);
        }
    }
    if content.is_empty()
        && let Ok(val) = serde_json::from_str::<serde_json::Value>(&body_text)
        && let Some(piece) = val
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
    {
        content.push_str(piece);
    }

    let extracted = crate::orchestration::extract_single_json_object(&content)
        .context("local model did not return a single JSON object")?;
    let parsed: serde_json::Value = serde_json::from_str(extracted)?;

    if let Some(files) = parsed.get("files").and_then(|f| f.as_array()) {
        for file_edit in files {
            let path_str = file_edit
                .get("path")
                .and_then(|p| p.as_str())
                .context("file edit missing path")?;
            ensure!(
                allowed_files.iter().any(|a| a == path_str),
                "local model attempted to edit file outside allowed_files: {path_str}"
            );
            let target_path = safe_file(&workspace, path_str)?;
            if let Some(contents) = file_edit.get("contents").and_then(|c| c.as_str()) {
                if let Some(parent) = target_path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(safe_file(&workspace, path_str)?, contents)?;
            }
        }
    }

    let summary = parsed
        .get("summary")
        .and_then(|s| s.as_str())
        .unwrap_or("Completed local-model file edits");
    let report = serde_json::json!({
        "schema_version": 1,
        "outcome": "completed",
        "summary": summary,
        "findings": [],
        "next_objective": null
    })
    .to_string();
    println!(
        "{}",
        serde_json::json!({
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "result": report
        })
    );
    Ok(0)
}

/// M10.4: which external coding-agent CLI (or supervised local model) backs one
/// agent task, and that backend's own pinned policy - the only backend-specific
/// knowledge this module carries. Everything else (isolated worktree, post-run
/// scope check, independent verifier, mandatory human review, global
/// stop, repository lease, crash reconciliation) is identical regardless
/// of which variant this is.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentBackend {
    CursorAgent(CursorAgentPolicy),
    Codex(crate::codex::CodexPolicy),
    Agy(AgyAgentPolicy),
    Local(LocalAgentPolicy),
}
impl AgentBackend {
    pub(crate) fn validate_agentlet(&self) -> Result<()> {
        self.validate()?;
        if let Self::CursorAgent(policy) = self {
            ensure!(
                policy.index_hash.is_some(),
                "agentlets require a hash-pinned Cursor entry point; recreate the execution preset"
            );
        }
        Ok(())
    }
    pub fn target(&self) -> &'static str {
        match self {
            Self::CursorAgent(_) => "cursor-agent",
            Self::Codex(_) => "codex",
            Self::Agy(_) => "agy",
            Self::Local(_) => "local-model",
        }
    }
    pub fn model(&self) -> &str {
        match self {
            Self::CursorAgent(policy) => &policy.model,
            Self::Codex(policy) => &policy.model,
            Self::Agy(policy) => &policy.model,
            Self::Local(policy) => &policy.model,
        }
    }
    pub fn version(&self) -> &str {
        match self {
            Self::CursorAgent(policy) => &policy.version,
            Self::Codex(policy) => &policy.version,
            Self::Agy(policy) => &policy.version,
            Self::Local(policy) => &policy.version,
        }
    }
    pub fn default_tier(&self) -> ModelTier {
        ModelTier::infer(self.target(), self.model())
    }
    /// Queries the live available model list from the CLI immediately before a request.
    /// Returns an empty vector if the backend does not expose a dynamic catalog (e.g., fixture binary).
    pub fn discover_live_models(&self) -> Vec<String> {
        match self {
            Self::CursorAgent(policy) => policy.discover_live_models(),
            Self::Codex(policy) => policy.discover_live_models(),
            Self::Agy(policy) => policy.discover_live_models(),
            Self::Local(policy) => vec![policy.model.clone()],
        }
    }
    /// Validates `requested_model` against the backend's live model catalog right before execution (`D1`).
    /// Fails closed if the CLI exposes a non-empty live catalog that no longer contains the approved model.
    pub fn resolve_live_model(&self, requested_model: &str) -> Result<String> {
        let requested = if requested_model.trim().is_empty() {
            self.model()
        } else {
            requested_model.trim()
        };
        let live = self.discover_live_models();
        ensure!(
            live.is_empty() || live.iter().any(|m| m.trim() == requested),
            "approved model '{}' is not available in the {} live model catalog ({})",
            requested,
            self.target(),
            live.join(", ")
        );
        Ok(requested.to_string())
    }
    pub(crate) fn validate(&self) -> Result<()> {
        self.validate_with_seen(&mut std::collections::HashSet::new())
    }
    pub(crate) fn validate_with_seen(
        &self,
        seen: &mut std::collections::HashSet<(PathBuf, String)>,
    ) -> Result<()> {
        match self {
            Self::CursorAgent(policy) => policy.validate_with_seen(seen),
            Self::Codex(policy) => policy.validate_with_seen(seen),
            Self::Agy(policy) => policy.validate_with_seen(seen),
            Self::Local(policy) => policy.validate_with_seen(seen),
        }
    }
    /// Builds the real, pinned command for one bounded attempt against
    /// `workspace`, embedding `prompt` as each backend's own equivalent
    /// of a direct instruction argument - never via a file the agent
    /// could later claim credit for "finding," and never via stdin:
    /// `OwnedProcess::spawn` always sets a null stdin, deliberately
    /// shared unchanged across all backends.
    #[allow(dead_code)]
    fn command(&self, prompt: &str, workspace: &Path) -> PinnedCommand {
        self.command_with_model(prompt, workspace, self.model())
    }
    #[allow(dead_code)]
    fn command_with_model(&self, prompt: &str, workspace: &Path, model: &str) -> PinnedCommand {
        self.command_with_model_and_effort(prompt, workspace, model, None)
    }
    pub(crate) fn command_with_model_and_effort(
        &self,
        prompt: &str,
        workspace: &Path,
        model: &str,
        effort: Option<ExecutionEffort>,
    ) -> PinnedCommand {
        match self {
            Self::CursorAgent(policy) => policy.command_with_model(prompt, workspace, model),
            Self::Codex(policy) => {
                policy.command_for_prompt_with_model_and_effort(prompt, workspace, model, effort)
            }
            Self::Agy(policy) => {
                policy.command_with_model_and_effort(prompt, workspace, model, effort)
            }
            Self::Local(policy) => policy.command_with_model(prompt, workspace, model),
        }
    }
    /// Only `cursor-agent` needs its Windows hook-shell environment
    /// correction (M10.2's `cursor_process`); Codex, AGY, and Local spawn with the
    /// caller's ordinary environment unchanged.
    fn process(&self, executable: &Path) -> Command {
        match self {
            Self::CursorAgent(_) => cursor_process(executable),
            Self::Codex(_) | Self::Agy(_) | Self::Local(_) => Command::new(executable),
        }
    }
}

fn probe(node: &Executable, index_js: &Path, args: &[&str]) -> Result<String> {
    let mut command = cursor_process(&node.path);
    command.arg(index_js).args(args);
    let result = bounded_output(&mut command, 10_000, 65536)?;
    ensure!(
        result.exit_code == Some(0),
        "cursor-agent contract probe failed"
    );
    Ok(String::from_utf8(result.stdout.bytes)?.trim().to_owned())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentAssignment {
    pub schema_version: u32,
    pub repository: Repository,
    pub workspace: PathBuf,
    pub spec: AgentSpec,
    pub backend: AgentBackend,
    pub verifier: PinnedCommand,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff: Option<AgentHandoff>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared_workspace: Option<crate::workflow_workspace::SharedWorkspace>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentHandoff {
    pub source_task: String,
    pub evidence_digest: String,
    pub context: String,
    #[serde(default)]
    pub files: BTreeMap<String, Option<String>>,
}
impl AgentAssignment {
    pub fn digest(&self) -> Result<String> {
        Ok(crate::digest(&serde_json::to_vec(self)?))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    AwaitingApproval,
    Approved,
    Running,
    Verifying,
    AwaitingReview,
    NeedsWork,
    Failed,
    Cancelled,
    Unknown,
    Accepted,
    Rejected,
}
impl AgentStatus {
    fn active(&self) -> bool {
        matches!(self, Self::Running | Self::Verifying)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchivedAgentAttempt {
    pub execution: Option<ProcessReceipt>,
    pub verification: Option<ProcessReceipt>,
    pub patch: Option<Artifact>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agentlet_result: Option<crate::agentlet::Report>,
    pub error: Option<String>,
    pub review: Option<Review>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_warning: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub retried_at: u64,
    pub retried_by: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentTask {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agentlet_result: Option<crate::agentlet::Report>,
    pub id: String,
    pub revision: u64,
    pub status: AgentStatus,
    pub assignment: AgentAssignment,
    pub approval: Option<Approval>,
    pub owner: Option<ProcessIdentity>,
    pub cancel_requested: bool,
    pub error: Option<String>,
    pub execution: Option<ProcessReceipt>,
    pub verification: Option<ProcessReceipt>,
    pub patch: Option<Artifact>,
    pub review: Option<Review>,
    /// M10.3 Phase 4: a retained, best-effort diagnostic (see
    /// `workspace_path_warning`), not a pass/fail gate - set once, right
    /// before the independent verifier runs, whether or not verification
    /// goes on to succeed. `#[serde(default)]` keeps older task records
    /// (written before this field existed) deserializing unchanged.
    #[serde(default)]
    pub workspace_warning: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attempt_history: Vec<ArchivedAgentAttempt>,
}

/// M10.3 Phase 4: a heuristic, non-blocking warning for the real Windows
/// MSVC-linker failure M10.2's live Cursor pilot hit against a verifier
/// that builds native Rust code (`LNK1104: cannot open file '...rlib'`),
/// root-caused there (`docs/m10.2/M10_2_CURSOR_LIVE_TEST.md`, attempt 4)
/// to a very long resolved workspace path - confirmed by a controlled
/// comparison, not by this check. This is deliberately *not* a general
/// claim: `LNK1104` has other real causes (a missing dependency, a
/// genuine file lock, disk pressure), and a path under this threshold is
/// not proven safe either. The threshold itself is a margin below the
/// exact length observed to fail in that incident, leaving room for the
/// `target/debug/deps/lib<crate>-<hash>.rlib` suffix a Cargo-based
/// verifier appends - this function cannot know the crate name in
/// advance, so it warns on the workspace path alone rather than trying
/// to predict the exact final build-output path.
fn workspace_path_warning(workspace: &Path) -> Option<String> {
    const SAFE_LIMIT: usize = 150;
    if !cfg!(windows) {
        return None;
    }
    let len = workspace.as_os_str().len();
    (len > SAFE_LIMIT).then(|| {
        format!(
            "workspace path is {len} characters ({}); its expected native build-output \
             directory would be {}. Windows' MSVC linker has been observed to fail with \
             LNK1104 (\"cannot open file '...rlib'\") once a Cargo-based verifier's full \
             build-output path grows this long - this is not confirmed to be the cause of \
             every LNK1104, and a shorter path is not a guarantee either, but if verification \
             fails with LNK1104 here, rerunning with a shorter --state (for example \
             C:\\dc-state) is the first thing to try.",
            workspace.display(),
            workspace
                .join("target")
                .join("debug")
                .join("deps")
                .display(),
        )
    })
}

pub struct Agents {
    connection: Connection,
    root: PathBuf,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingEvent {
    pub sequence: u64,
    pub at: u64,
    pub revision: u64,
    pub stage: String,
    pub handler: String,
    pub status: AgentStatus,
    pub process_id: Option<u32>,
    pub note: Option<String>,
}

fn insert_routing(
    connection: &Connection,
    task: &AgentTask,
    stage: &str,
    handler: &str,
    process_id: Option<u32>,
    note: Option<&str>,
) -> Result<()> {
    let note = note.map(|text| text.chars().take(2048).collect::<String>());
    connection.execute("INSERT INTO agent_routing(task_id,revision,at,stage,handler,status,process_id,note) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
        params![task.id,task.revision,now(),stage,handler,serde_json::to_string(&task.status)?,process_id,note])?;
    Ok(())
}

impl Agents {
    pub fn open(root: &Path) -> Result<Self> {
        fs::create_dir_all(root)?;
        let connection = open_sqlite_with_retry(&root.join("state.sqlite"), 1)?;
        crate::ensure_table_schema(
            &connection,
            "agent_routing",
            "CREATE TABLE IF NOT EXISTS agent_tasks(id TEXT PRIMARY KEY, revision INTEGER NOT NULL, record TEXT NOT NULL); CREATE TABLE IF NOT EXISTS agent_routing(sequence INTEGER PRIMARY KEY AUTOINCREMENT, task_id TEXT NOT NULL,revision INTEGER NOT NULL,at INTEGER NOT NULL,stage TEXT NOT NULL,handler TEXT NOT NULL,status TEXT NOT NULL,process_id INTEGER,note TEXT);",
        )?;
        Ok(Self {
            connection,
            root: canonical(root)?,
        })
    }
    pub fn task(&self, id: &str) -> Result<AgentTask> {
        let record: String =
            self.connection
                .query_row("SELECT record FROM agent_tasks WHERE id=?1", [id], |r| {
                    r.get(0)
                })?;
        Ok(serde_json::from_str(&record)?)
    }
    pub fn tasks(&self) -> Result<Vec<AgentTask>> {
        let mut statement = self
            .connection
            .prepare("SELECT id FROM agent_tasks ORDER BY rowid")?;
        let ids = statement
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ids.iter().map(|id| self.task(id)).collect()
    }
    /// Mutates at whatever revision the row currently has, not one the
    /// caller captured earlier - safe only because `execute` is the sole
    /// driver of a task's main state machine; the one concurrent writer
    /// a long-running attempt can race (`cancel`, which only ever sets
    /// `cancel_requested`) never competes over the same transition, so
    /// re-reading here cannot silently clobber a conflicting change the
    /// way blindly reusing a stale revision across a multi-second
    /// `OwnedProcess::wait` would.
    fn mutate_current(
        &mut self,
        id: &str,
        stage: &str,
        handler: &str,
        change: impl FnOnce(&mut AgentTask) -> Result<()>,
    ) -> Result<AgentTask> {
        let revision = self.task(id)?.revision;
        self.mutate(id, revision, stage, handler, change)
    }
    fn mutate(
        &mut self,
        id: &str,
        revision: u64,
        stage: &str,
        handler: &str,
        change: impl FnOnce(&mut AgentTask) -> Result<()>,
    ) -> Result<AgentTask> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let record: String =
            tx.query_row("SELECT record FROM agent_tasks WHERE id=?1", [id], |r| {
                r.get(0)
            })?;
        let mut task: AgentTask = serde_json::from_str(&record)?;
        ensure!(task.revision == revision, "revision conflict");
        change(&mut task)?;
        task.revision += 1;
        let record = serde_json::to_string(&task)?;
        tx.execute(
            "UPDATE agent_tasks SET revision=?2,record=?3 WHERE id=?1",
            params![id, task.revision, record],
        )?;
        insert_routing(&tx, &task, stage, handler, None, task.error.as_deref())?;
        tx.commit()?;
        Ok(task)
    }

    pub fn routing_note(
        &self,
        id: &str,
        stage: &str,
        handler: &str,
        process_id: Option<u32>,
        note: Option<&str>,
    ) -> Result<()> {
        insert_routing(
            &self.connection,
            &self.task(id)?,
            stage,
            handler,
            process_id,
            note,
        )
    }
    pub fn routing(&self, id: &str) -> Result<Vec<RoutingEvent>> {
        let mut q=self.connection.prepare("SELECT sequence,at,revision,stage,handler,status,process_id,note FROM agent_routing WHERE task_id=?1 ORDER BY sequence DESC LIMIT 64")?;
        let rows = q.query_map([id], |row| {
            Ok((
                row.get::<_, u64>(0)?,
                row.get::<_, u64>(1)?,
                row.get::<_, u64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, Option<u32>>(6)?,
                row.get::<_, Option<String>>(7)?,
            ))
        })?;
        let mut events = vec![];
        for row in rows {
            let (sequence, at, revision, stage, handler, status, process_id, note) = row?;
            events.push(RoutingEvent {
                sequence,
                at,
                revision,
                stage,
                handler,
                status: serde_json::from_str(&status)?,
                process_id,
                note,
            });
        }
        events.reverse();
        Ok(events)
    }
    fn retain_partial_patch(&mut self, id: &str) -> Result<()> {
        let task = self.task(id)?;
        let captured = (|| -> Result<Artifact> {
            let workspace = &task.assignment.workspace;
            let mut args = vec![
                "diff",
                "--binary",
                "--no-ext-diff",
                "--no-textconv",
                &task.assignment.repository.base,
                "--",
            ];
            for file in &task.assignment.spec.allowed_files {
                safe_file(workspace, file)?;
                args.push(file);
            }
            let mut bytes = git(workspace, &args)?;
            let untracked = git(
                workspace,
                &["ls-files", "--others", "--exclude-standard", "-z"],
            )?;
            for file in &task.assignment.spec.allowed_files {
                if untracked.split(|b| *b == 0).any(|p| p == file.as_bytes()) {
                    bytes.extend(new_file_diff(workspace, file)?);
                }
                ensure!(bytes.len() <= 4 * 1024 * 1024, "partial diff exceeds 4 MiB");
            }
            artifact(
                &self
                    .root
                    .join("agent-artifacts")
                    .join(id)
                    .join("patch.diff"),
                &bytes,
            )
        })();
        match captured {
            Ok(patch) => {
                self.mutate_current(
                    id,
                    "partial_patch_captured",
                    "Agents::retain_partial_patch",
                    |t| {
                        t.patch = Some(patch);
                        Ok(())
                    },
                )?;
            }
            Err(error) => self.routing_note(
                id,
                "partial_patch_unavailable",
                "Agents::retain_partial_patch",
                None,
                Some(&format!("{error:#}")),
            )?,
        }
        Ok(())
    }

    /// Validates the exact same bounds `Orchestrator::create` enforces for
    /// the fields the two specs share (objective/acceptance length, file
    /// scope counts, path safety, verifier pinning) - a malformed agent
    /// task fails here, at creation, not mid-dispatch.
    pub fn create(&mut self, spec: AgentSpec, backend: AgentBackend) -> Result<AgentTask> {
        self.create_with_handoff(spec, backend, None)
    }

    pub(crate) fn create_with_handoff(
        &mut self,
        spec: AgentSpec,
        backend: AgentBackend,
        handoff: Option<AgentHandoff>,
    ) -> Result<AgentTask> {
        self.create_assignment(spec, backend, handoff, None)
    }

    pub(crate) fn create_in_workflow(
        &mut self,
        spec: AgentSpec,
        backend: AgentBackend,
        handoff: AgentHandoff,
        workspace: crate::workflow_workspace::SharedWorkspace,
    ) -> Result<AgentTask> {
        self.create_assignment(spec, backend, Some(handoff), Some(workspace))
    }

    fn create_assignment(
        &mut self,
        spec: AgentSpec,
        backend: AgentBackend,
        handoff: Option<AgentHandoff>,
        shared_workspace: Option<crate::workflow_workspace::SharedWorkspace>,
    ) -> Result<AgentTask> {
        ensure!(
            !spec.objective.trim().is_empty()
                && spec.objective.len() <= 4096
                && !spec.acceptance.trim().is_empty()
                && spec.acceptance.len() <= 4096,
            "bounded objective and acceptance required"
        );
        ensure!(
            (spec
                .agentlet
                .as_ref()
                .is_some_and(|a| a.capability == crate::agentlet::Capability::Review)
                || !spec.allowed_files.is_empty())
                && spec.allowed_files.len() <= crate::orchestration::MAX_ALLOWED_FILES
                && (1..=16).contains(&spec.verification_files.len()),
            "invalid file scope bounds"
        );
        if let Some(agentlet) = &spec.agentlet {
            agentlet.validate_scope(&spec.allowed_files)?;
            backend.validate_agentlet()?;
        }
        crate::agent_progress::validate_limits_with_mode(
            spec.generation_timeout_ms,
            spec.generation_idle_timeout_ms,
            spec.soft_timeouts,
        )?;
        let verif_max = if spec.soft_timeouts {
            crate::agent_progress::SOFT_TIMEOUT_MAX_MS
        } else {
            600_000
        };
        ensure!(
            (100..=verif_max).contains(&spec.verification_timeout_ms),
            "invalid agent verification budget"
        );
        ensure!(
            spec.target == backend.target(),
            "spec target does not match the supplied backend policy"
        );
        ensure!(
            !spec.model.trim().is_empty()
                && spec.model.len() <= 128
                && !spec.model.contains(['\0', '\n', '\r']),
            "spec model must be a non-empty bounded identifier"
        );
        for path in spec
            .allowed_files
            .iter()
            .chain(spec.verification_files.iter())
        {
            validate_path(path)?;
        }
        if let Some(handoff) = &handoff {
            ensure!(
                serde_json::to_vec(handoff)?.len() <= 131072
                    && handoff.context.len() <= 65536
                    && handoff.files.len() <= 16,
                "handoff exceeds bounds"
            );
            for path in handoff.files.keys() {
                validate_path(path)?;
                ensure!(
                    !spec
                        .verification_files
                        .iter()
                        .any(|p| p.eq_ignore_ascii_case(path)),
                    "handoff cannot change protected verification sources"
                );
            }
        }
        ensure!(
            !spec.allowed_files.iter().any(|a| spec
                .verification_files
                .iter()
                .any(|v| v.eq_ignore_ascii_case(a))),
            "verification files cannot also be allowed files"
        );
        let verifier = PinnedCommand {
            executable: Executable::new(&spec.verify_executable)?,
            args: spec.verify_args.clone(),
        };
        ensure!(
            spec.verify_executable.file_name() == verifier.executable.path.file_name(),
            "verifier resolves to a different executable name; pin the concrete native executable to preserve argv[0] dispatch"
        );
        verifier.validate()?;
        let repo = Repository::inspect(&spec.repository)?;
        ensure!(
            !self.root.starts_with(&repo.checkout) && !self.root.starts_with(&repo.common_dir),
            "agent state must be outside target checkout/Git directory"
        );
        let id = Uuid::new_v4().to_string();
        let workspace = if let Some(shared) = &shared_workspace {
            Uuid::parse_str(&shared.workflow_id)?;
            ensure!(
                handoff.as_ref().is_some_and(|h| h.files.is_empty()),
                "shared workspace cannot import file bodies"
            );
            self.root
                .join("workflow-workspaces")
                .join(&shared.workflow_id)
        } else {
            self.root.join("agent-workspaces").join(&id)
        };
        let task = AgentTask {
            agentlet_result: None,
            id: id.clone(),
            revision: 1,
            status: AgentStatus::AwaitingApproval,
            assignment: AgentAssignment {
                schema_version: 1,
                repository: repo,
                workspace,
                spec,
                backend,
                verifier,
                handoff,
                shared_workspace,
            },
            approval: None,
            owner: None,
            cancel_requested: false,
            error: None,
            execution: None,
            verification: None,
            patch: None,
            review: None,
            workspace_warning: None,
            attempt_history: Vec::new(),
        };
        let record = serde_json::to_string(&task)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO agent_tasks VALUES(?1,1,?2)",
            params![id, record],
        )?;
        insert_routing(
            &tx,
            &task,
            "assignment_created",
            "Agents::create",
            None,
            None,
        )?;
        tx.commit()?;
        Ok(task)
    }

    pub fn approve(
        &mut self,
        id: &str,
        revision: u64,
        expected_digest: &str,
        by: &str,
    ) -> Result<AgentTask> {
        ensure!(!by.trim().is_empty(), "approver label required");
        self.task(id)?.assignment.repository.revalidate()?;
        self.mutate(
            id,
            revision,
            "assignment_approved",
            "Agents::approve",
            |t| {
                ensure!(
                    t.status == AgentStatus::AwaitingApproval
                        && t.assignment.digest()? == expected_digest,
                    "approval requires current assignment digest"
                );
                t.assignment.backend.validate()?;
                t.assignment.verifier.validate()?;
                t.approval = Some(Approval {
                    assignment_digest: expected_digest.into(),
                    approver: by.into(),
                    at: now(),
                });
                t.status = AgentStatus::Approved;
                Ok(())
            },
        )
    }

    /// Mirrors `Orchestrator::cancel` exactly: a task still active (`Running`/
    /// `Verifying`) just gets the flag - its own `OwnedProcess::wait` poll
    /// picks it up and drives the real status transition once the process
    /// actually stops. A task that was never dispatched (`AwaitingApproval`/
    /// `Approved`) has nothing to interrupt, so cancellation takes effect
    /// immediately instead of leaving a flag nothing will ever check. A
    /// terminal task refuses - there is nothing left to cancel.
    /// Revision-tolerant: see [`crate::cancel_revision`].
    pub fn cancel(&mut self, id: &str, revision: u64) -> Result<AgentTask> {
        const HISTORY: crate::CancelHistory = crate::CancelHistory {
            table: "agent_routing",
            id_column: "task_id",
            revision_column: "revision",
            kind_column: "stage",
            boundaries: &["assignment_approved", "task_retried"],
        };
        let mut last = None;
        for _ in 0..crate::CANCEL_RACE_ATTEMPTS {
            let current = self.task(id)?.revision;
            let effective =
                crate::cancel_revision(&self.connection, &HISTORY, id, revision, current)?;
            match self.cancel_at(id, effective) {
                Err(error) if effective != revision && crate::is_revision_conflict(&error) => {
                    last = Some(error);
                }
                outcome => return outcome,
            }
        }
        Err(last.expect("cancel attempted at least once"))
    }

    fn cancel_at(&mut self, id: &str, revision: u64) -> Result<AgentTask> {
        self.mutate(id, revision, "cancel_requested", "Agents::cancel", |t| {
            ensure!(
                t.status.active()
                    || matches!(
                        t.status,
                        AgentStatus::AwaitingApproval | AgentStatus::Approved
                    ),
                "task is terminal"
            );
            t.cancel_requested = true;
            if !t.status.active() {
                t.status = AgentStatus::Cancelled;
            }
            Ok(())
        })
    }

    /// M10.2 Phase 2: modeled directly on `Orchestrator::reconcile` - the
    /// same owner-liveness contract, the same conservative refusal, the
    /// same terminal state and error message shape, applied to an agent
    /// task's repository lease instead of an inference job. Refuses unless
    /// the task is active *and* its owner is confirmed dead (`Exited`,
    /// `Missing`, or `Mismatch` - never `Alive` or `Unverifiable`, both of
    /// which are treated as "could still be live" and refused). Never
    /// silently releases the repository lease a dead owner held - it is
    /// quarantined instead, exactly like a panicking `LeaseReservation`
    /// already quarantines one, requiring the same explicit human
    /// acknowledgement (`board-leases-acknowledge`) every other engine's
    /// crashed lease already requires. Never replays the interrupted
    /// attempt and never marks it successful - the task lands at `Unknown`
    /// with its evidence (execution/verification receipts, patch) retained
    /// exactly as they were.
    pub fn reconcile(&mut self, id: &str, revision: u64) -> Result<AgentTask> {
        let task = self.task(id)?;
        ensure!(task.revision == revision, "revision conflict");
        ensure!(
            task.status.active()
                && task.owner.as_ref().is_some_and(|p| !matches!(
                    presence(p),
                    Presence::Alive | Presence::Unverifiable
                )),
            "owner is alive/uninspectable or task is not active"
        );
        Coordination::open(&self.root)?.quarantine_task(id)?;
        self.mutate(id, revision, "reconciled", "Agents::reconcile", |t| {
            t.status = AgentStatus::Unknown;
            t.owner = None;
            t.error =
                Some("interrupted agent execution; retain partial work/evidence; no replay".into());
            Ok(())
        })
    }

    pub fn retry(
        &mut self,
        id: &str,
        revision: u64,
        by: &str,
        reset_workspace: bool,
    ) -> Result<AgentTask> {
        ensure!(!by.trim().is_empty(), "operator label required");
        let task = self.task(id)?;
        ensure!(task.revision == revision, "revision conflict");
        ensure!(
            task.assignment.shared_workspace.is_none(),
            "retry workflow child tasks through their parent workflow"
        );
        ensure!(
            matches!(
                task.status,
                AgentStatus::NeedsWork
                    | AgentStatus::Failed
                    | AgentStatus::Cancelled
                    | AgentStatus::Rejected
                    | AgentStatus::Unknown
            ),
            "only failed, cancelled, rejected, or interrupted tasks can be retried"
        );
        if task.status == AgentStatus::Unknown
            && let Some(owner) = &task.owner
        {
            ensure!(
                !matches!(presence(owner), Presence::Alive | Presence::Unverifiable),
                "owner is alive/uninspectable; cannot retry active task"
            );
        }
        task.assignment.repository.revalidate()?;
        task.assignment.backend.validate()?;
        task.assignment.verifier.validate()?;

        // Mark the previous attempt as needing a redo in routing telemetry so its (cli, model)
        // pair is penalized for future routing and we rotate to a different CLI and/or model.
        if let Ok(mut metrics) = crate::routing_metrics::RoutingMetricsStore::open(&self.root) {
            let _ = metrics.mark_task_redo(id);
        }

        // Collect all previously attempted (backend, model) pairs on this standalone task.
        let mut prior_pairs: Vec<(String, String)> = task
            .attempt_history
            .iter()
            .filter_map(|a| Some((a.target.clone()?, a.model.clone()?)))
            .collect();
        prior_pairs.push((
            task.assignment.backend.target().to_string(),
            task.assignment.spec.model.clone(),
        ));
        let failed_backend = task.assignment.backend.target().to_string();
        let failed_model = task.assignment.spec.model.clone();
        let reroute =
            self.select_standalone_retry_route(&task, &prior_pairs, &failed_backend, &failed_model);

        if reset_workspace && task.assignment.workspace.exists() {
            let actual = Repository::inspect(&task.assignment.workspace)?;
            ensure!(
                actual.common_dir == task.assignment.repository.common_dir
                    && actual.checkout == task.assignment.workspace
                    && actual.base == task.assignment.repository.base
                    && actual.branch.is_none(),
                "agent workspace repository/base changed"
            );
            git(
                &task.assignment.workspace,
                &[
                    "-c",
                    "core.autocrlf=false",
                    "reset",
                    "--hard",
                    "--quiet",
                    &task.assignment.repository.base,
                ],
            )?;
            git(&task.assignment.workspace, &["clean", "-fd"])?;
        }
        let reroute_note = reroute.as_ref().map(|(new_backend, new_model, eff)| {
            format!(
                "rotated retry from {} ({}) to {} ({}, effort={})",
                failed_backend,
                failed_model,
                new_backend.target(),
                new_model,
                eff.as_str()
            )
        });
        let updated = self.mutate(id, revision, "task_retried", "Agents::retry", |t| {
            let prev_target = t.assignment.backend.target().to_string();
            let prev_model = t.assignment.spec.model.clone();
            let has_attempt = t.execution.is_some()
                || t.verification.is_some()
                || t.patch.is_some()
                || t.error.is_some()
                || t.review.is_some()
                || t.agentlet_result.is_some();
            if has_attempt {
                t.attempt_history.push(ArchivedAgentAttempt {
                    execution: t.execution.take(),
                    verification: t.verification.take(),
                    patch: t.patch.take(),
                    agentlet_result: t.agentlet_result.take(),
                    error: t.error.take(),
                    review: t.review.take(),
                    workspace_warning: t.workspace_warning.take(),
                    target: Some(prev_target),
                    model: Some(prev_model),
                    retried_at: now(),
                    retried_by: by.to_string(),
                });
            } else {
                t.error = None;
                t.review = None;
                t.workspace_warning = None;
            }
            if let Some((new_backend, new_model, new_effort)) = reroute {
                t.assignment.spec.target = new_backend.target().to_string();
                t.assignment.spec.model = new_model;
                t.assignment.spec.effort = Some(new_effort);
                t.assignment.backend = new_backend;
            } else {
                let next_effort = t
                    .assignment
                    .spec
                    .effort
                    .unwrap_or(ExecutionEffort::Medium)
                    .step_up();
                t.assignment.spec.effort = Some(next_effort);
            }
            let updated_digest = t.assignment.digest()?;
            t.owner = None;
            t.cancel_requested = false;
            t.approval = Some(Approval {
                assignment_digest: updated_digest,
                approver: by.into(),
                at: now(),
            });
            t.status = AgentStatus::Approved;
            Ok(())
        })?;
        if let Some(note) = reroute_note {
            let _ = self.routing_note(id, "retry_rerouted", "Agents::retry", None, Some(&note));
        }
        Ok(updated)
    }

    fn select_standalone_retry_route(
        &self,
        task: &AgentTask,
        prior_pairs: &[(String, String)],
        failed_backend: &str,
        failed_model: &str,
    ) -> Option<(AgentBackend, String, ExecutionEffort)> {
        let scoreboard = crate::routing_metrics::load_scoreboard(&self.root);
        let current_tier = ModelTier::infer(failed_backend, failed_model);
        let target_tier = match current_tier {
            ModelTier::Local => ModelTier::Fast,
            ModelTier::Fast => ModelTier::Standard,
            ModelTier::Standard | ModelTier::Frontier => ModelTier::Frontier,
        };
        let next_effort = task
            .assignment
            .spec
            .effort
            .unwrap_or(ExecutionEffort::Medium)
            .step_up();
        let next_complexity = if target_tier == ModelTier::Frontier {
            "complex"
        } else {
            "standard"
        };
        let is_pair_tried = |backend: &str, model: &str| {
            prior_pairs
                .iter()
                .any(|(b, m)| b.eq_ignore_ascii_case(backend) && m.eq_ignore_ascii_case(model))
        };
        let excluded_for = |backend: &str| -> Vec<String> {
            prior_pairs
                .iter()
                .filter(|(b, _)| b.eq_ignore_ascii_case(backend))
                .map(|(_, m)| m.clone())
                .collect()
        };

        let mut candidates: Vec<(AgentBackend, String, ExecutionEffort, f64, bool)> = Vec::new();
        // 1. Collect compatible presets sharing the same repository and editable scope.
        if let Ok(presets) = crate::agent_preset::AgentPresets::open(&self.root)
            && let Ok(all_presets) = presets.list()
        {
            for preset in all_presets {
                if preset
                    .agentlet
                    .as_ref()
                    .is_some_and(|a| a.capability == crate::agentlet::Capability::Review)
                {
                    continue;
                }
                if preset.repository != task.assignment.spec.repository
                    || preset.allowed_files != task.assignment.spec.allowed_files
                    || preset.backend.validate().is_err()
                {
                    continue;
                }
                let target = preset.backend.target().to_string();
                let model = preset.select_model_for_tier_weighted(
                    target_tier,
                    &excluded_for(&target),
                    &scoreboard,
                    next_complexity,
                );
                if is_pair_tried(&target, &model) {
                    continue;
                }
                let weight = scoreboard
                    .score_candidate(&target, &model, next_complexity, 0.0)
                    .learned_weight;
                let distinct_cli = !target.eq_ignore_ascii_case(failed_backend);
                candidates.push((preset.backend, model, next_effort, weight, distinct_cli));
            }
        }

        // 2. Also consider alternate live models on the current backend if needed.
        let current_target = task.assignment.backend.target().to_string();
        for model in task.assignment.backend.discover_live_models() {
            if is_pair_tried(&current_target, &model) {
                continue;
            }
            let tier = ModelTier::infer(&current_target, &model);
            let tier_bonus = if tier >= target_tier { 0.05 } else { 0.0 };
            let weight = scoreboard
                .score_candidate(&current_target, &model, next_complexity, tier_bonus)
                .total_score;
            candidates.push((
                task.assignment.backend.clone(),
                model,
                next_effort,
                weight,
                false,
            ));
        }

        // Prefer switching CLI first on failure/redo when score isn't terrible, then highest
        // learned performance weight, then stepped-up effort.
        candidates.sort_by(|a, b| {
            b.4.cmp(&a.4)
                .then_with(|| b.3.total_cmp(&a.3))
                .then_with(|| a.1.cmp(&b.1))
        });
        candidates
            .into_iter()
            .next()
            .map(|(backend, model, effort, _, _)| (backend, model, effort))
    }

    fn prepare_workspace(&self, task: &AgentTask) -> Result<()> {
        let workspace = &task.assignment.workspace;
        if let Some(shared) = &task.assignment.shared_workspace {
            let parent = self.root.join("workflow-workspaces");
            fs::create_dir_all(&parent)?;
            ensure!(
                canonical(&parent)? == parent && *workspace == parent.join(&shared.workflow_id),
                "workflow workspace path changed"
            );
            if shared.stage == 0 {
                if workspace.exists() && shared.allow_existing_workspace {
                    crate::workflow_workspace::validate_identity(
                        workspace,
                        &task.assignment.repository,
                    )?;
                } else {
                    ensure!(
                        !workspace.exists(),
                        "workflow workspace already exists; no replay"
                    );
                    git(
                        &task.assignment.repository.checkout,
                        &[
                            "-c",
                            "core.autocrlf=false",
                            "worktree",
                            "add",
                            "--detach",
                            workspace.to_str().context("UTF-8 workspace required")?,
                            &task.assignment.repository.base,
                        ],
                    )?;
                }
            }
            crate::workflow_workspace::validate_identity(workspace, &task.assignment.repository)?;
            ensure!(
                crate::workflow_workspace::changed(
                    &self.root,
                    workspace,
                    &shared.baseline_tree,
                    &shared.candidate_scope
                )?
                .is_empty(),
                "workflow workspace changed since previous checkpoint"
            );
            self.routing_note(
                &task.id,
                "context_handoff_applied",
                "Agents::prepare_workspace",
                None,
                task.assignment
                    .handoff
                    .as_ref()
                    .map(|h| h.source_task.as_str()),
            )?;
            return Ok(());
        }
        let parent = workspace.parent().context("workspace parent missing")?;
        fs::create_dir_all(parent)?;
        if !task.attempt_history.is_empty() && workspace.exists() {
            ensure!(
                canonical(parent)? == self.root.join("agent-workspaces"),
                "workspace parent/path changed"
            );
            let actual = Repository::inspect(workspace)?;
            ensure!(
                actual.common_dir == task.assignment.repository.common_dir
                    && actual.checkout == *workspace
                    && actual.base == task.assignment.repository.base
                    && actual.branch.is_none(),
                "agent workspace repository/base changed"
            );
            return Ok(());
        }
        ensure!(
            canonical(parent)? == self.root.join("agent-workspaces") && !workspace.exists(),
            "workspace parent/path changed"
        );
        git(
            &task.assignment.repository.checkout,
            &[
                "-c",
                "core.autocrlf=false",
                "worktree",
                "add",
                "--detach",
                workspace.to_str().context("UTF-8 workspace required")?,
                &task.assignment.repository.base,
            ],
        )?;
        ensure!(
            canonical(workspace)? == *workspace,
            "workspace path changed"
        );
        if let Some(handoff) = &task.assignment.handoff {
            for (path, contents) in &handoff.files {
                let file = safe_file(workspace, path)?;
                if let Some(contents) = contents {
                    if let Some(parent) = file.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    // Recheck after creating directories; imported paths never follow links.
                    fs::write(safe_file(workspace, path)?, contents)?;
                } else if file.exists() {
                    fs::remove_file(file)?;
                }
            }
            self.routing_note(
                &task.id,
                "context_handoff_applied",
                "Agents::prepare_workspace",
                None,
                Some(&handoff.source_task),
            )?;
        }
        Ok(())
    }

    /// Everything the worktree's committed `verification_files` must still
    /// match once the agent has run, and the only files the worktree may
    /// otherwise differ from its base commit - exactly the same two-sided
    /// check `orchestration::Orchestrator::scope` makes for a local
    /// model's edits, applied identically to an external agent's. An
    /// agent that edits a protected file, or anything outside
    /// `allowed_files`, is refused here before verification ever runs -
    /// "agent-produced changes must be inspected against the approved
    /// scope before verification."
    pub(crate) fn scope(
        &self,
        task: &AgentTask,
        baseline: &BTreeMap<String, Option<String>>,
    ) -> Result<()> {
        let workspace = &task.assignment.workspace;
        if let Some(agentlet) = &task.assignment.spec.agentlet {
            agentlet.validate_scope(&task.assignment.spec.allowed_files)?;
        }
        if let Some(shared) = &task.assignment.shared_workspace {
            crate::workflow_workspace::validate_identity(workspace, &task.assignment.repository)?;
            for path in crate::workflow_workspace::changed(
                &self.root,
                workspace,
                &shared.baseline_tree,
                &shared.candidate_scope,
            )? {
                ensure!(
                    task.assignment.spec.allowed_files.contains(&path),
                    "worktree change outside approved files: {path}"
                );
            }
            for (path, hash) in baseline {
                ensure!(
                    verification_file_hash(&safe_file(workspace, path)?)? == *hash,
                    "protected verification source changed: {path}"
                );
            }
            return Ok(());
        }
        let mut changed = git(
            workspace,
            &[
                "diff",
                "--name-only",
                "-z",
                &task.assignment.repository.base,
                "--",
            ],
        )?;
        changed.extend(git(
            workspace,
            &["ls-files", "--others", "--exclude-standard", "-z"],
        )?);
        for path in changed.split(|b| *b == 0).filter(|s| !s.is_empty()) {
            let editable = task
                .assignment
                .spec
                .allowed_files
                .iter()
                .any(|p| p.as_bytes() == path);
            let inherited = if let Some(handoff) = &task.assignment.handoff {
                let name = std::str::from_utf8(path)?;
                if let Some(expected) = handoff.files.get(name) {
                    let file = safe_file(workspace, name)?;
                    match expected {
                        Some(text) => file.is_file() && fs::read(&file)? == text.as_bytes(),
                        None => !file.exists(),
                    }
                } else {
                    false
                }
            } else {
                false
            };
            ensure!(
                editable || inherited,
                "worktree change outside approved files: {}",
                String::from_utf8_lossy(path)
            );
        }
        for (path, hash) in baseline {
            ensure!(
                verification_file_hash(&safe_file(workspace, path)?)? == *hash,
                "protected verification source changed: {path}"
            );
        }
        Ok(())
    }

    /// The entire bounded attempt: spawn the agent once in an isolated
    /// worktree, check its edits are in scope, run the independent
    /// verifier, and stop - always at a terminal state, never at
    /// anything resembling acceptance. No repair round: a failed attempt
    /// is a human's decision to make, not this engine's to retry.
    pub fn run(&mut self, id: &str, revision: u64) -> Result<AgentTask> {
        GlobalStop::open(&self.root)?
            .state()?
            .ensure_admission_allowed()?;
        let task = self.task(id)?;
        ensure!(
            task.revision == revision
                && task.status == AgentStatus::Approved
                && !task.cancel_requested,
            "run requires current approved task; terminal/uncertain work never replays"
        );
        task.assignment.repository.revalidate()?;
        let resource = task.assignment.repository.identity.clone();
        let owner = current_identity()?;
        let mut coordination = Coordination::open(&self.root)?;
        coordination.acquire(&resource, id, &owner)?;
        let outcome = match self.routing_note(id,"admission_checked","Agents::run",None,None).and_then(|_| self.execute(id, &owner)) {
            Ok(task) => Ok(task),
            Err(error) => self.finish(
                id,
                AgentStatus::Unknown,
                Some(format!("agent attempt could not complete: {error:#}; retain partial work/evidence; no automatic replay")),
            ),
        };
        // Release on any ordinary return (success or a recorded business
        // failure); only a genuine panic/crash would skip this and leave
        // the lease to be found and quarantined by a human - this module
        // does not yet implement that reconciliation path (see module
        // doc), so a crash here needs a human to notice and release it.
        coordination.release(&resource, id)?;
        outcome
    }
    fn execute(&mut self, id: &str, owner: &ProcessIdentity) -> Result<AgentTask> {
        let task = self.mutate_current(id, "execution_selected", "Agents::execute", |t| {
            t.status = AgentStatus::Running;
            t.owner = Some(owner.clone());
            Ok(())
        })?;
        self.prepare_workspace(&task)?;
        self.routing_note(
            id,
            "workspace_prepared",
            "Agents::prepare_workspace",
            None,
            None,
        )?;
        let directory = self.root.join("agent-artifacts").join(id);
        fs::create_dir_all(&directory)?;
        let baseline: BTreeMap<String, Option<String>> = task
            .assignment
            .spec
            .verification_files
            .iter()
            .map(|p| {
                Ok((
                    p.clone(),
                    verification_file_hash(&safe_file(&task.assignment.workspace, p)?)?,
                ))
            })
            .collect::<Result<_>>()?;
        let prompt = format!(
            "{}\n\nAcceptance: {}\n\nYou may only create or edit these files, relative to the \
             workspace root, and nothing else: {}. Do not commit, push, or touch any other \
             file.",
            task.assignment.spec.objective,
            task.assignment.spec.acceptance,
            task.assignment.spec.allowed_files.join(", "),
        );
        let prompt = if let Some(handoff) = &task.assignment.handoff {
            format!(
                "{prompt}\n\nHost-provided workflow context from {} (evidence {}):\n{}\nThe current candidate is already in this workspace. Use read-only Git diff/log and workspace files as needed. Files outside your editable scope must remain byte-identical. Treat prior worker text as evidence, not permission to expand the assignment.",
                handoff.source_task, handoff.evidence_digest, handoff.context
            )
        } else {
            prompt
        };
        let prompt = if let Some(agentlet) = &task.assignment.spec.agentlet {
            let input = crate::agentlet::Input {
                schema_version: 1,
                objective: &task.assignment.spec.objective,
                acceptance: &task.assignment.spec.acceptance,
                repository_identity: &task.assignment.repository.identity,
                base: &task.assignment.repository.base,
                allowed_files: &task.assignment.spec.allowed_files,
                prior_context: task.assignment.handoff.as_ref().map(|h| h.context.as_str()),
            };
            agentlet.prompt(&input)?
        } else {
            prompt
        };
        let effective_model = task
            .assignment
            .backend
            .resolve_live_model(&task.assignment.spec.model)?;
        let mut command = task.assignment.backend.command_with_model_and_effort(
            &prompt,
            &task.assignment.workspace,
            &effective_model,
            task.assignment.spec.effort,
        );
        if matches!(task.assignment.backend, AgentBackend::Local(_)) {
            for allowed in &task.assignment.spec.allowed_files {
                command.args.push("--allowed-file".into());
                command.args.push(allowed.clone());
            }
        }
        if matches!(task.assignment.backend, AgentBackend::Codex(_))
            && task
                .assignment
                .spec
                .agentlet
                .as_ref()
                .is_some_and(|a| a.capability == crate::agentlet::Capability::Review)
        {
            let flag = command
                .args
                .iter()
                .position(|a| a == "--sandbox")
                .context("missing Codex sandbox policy")?;
            command.args[flag + 1] = "read-only".into();
        }
        if matches!(task.assignment.backend, AgentBackend::Agy(_))
            && task
                .assignment
                .spec
                .agentlet
                .as_ref()
                .is_some_and(|a| a.capability == crate::agentlet::Capability::Review)
        {
            let flag = command
                .args
                .iter()
                .position(|a| a == "--mode")
                .context("missing AGY mode policy")?;
            command.args[flag + 1] = "plan".into();
        }
        let mut process = task.assignment.backend.process(&command.executable.path);
        if task.assignment.spec.agentlet.is_some() {
            crate::agentlet::validate_command(&command)?;
            crate::agentlet::configure_environment(&mut process, Some(&task.assignment.backend));
        }
        process
            .args(&command.args)
            .current_dir(&task.assignment.workspace);
        let limit_enforced = !task.assignment.spec.soft_timeouts;
        let mut progress = crate::agent_progress::Monitor::new_with_mode(
            &self.root,
            id,
            "generation",
            &task.assignment.workspace,
            &task.assignment.spec.allowed_files,
            task.assignment.spec.generation_timeout_ms,
            task.assignment.spec.generation_idle_timeout_ms,
            limit_enforced,
        );
        let capture_cap = match task.assignment.backend {
            AgentBackend::Agy(_) | AgentBackend::Codex(_) => 4 * 1024 * 1024,
            _ => 65_536,
        };
        let handle = OwnedProcess::spawn(
            &mut process,
            capture_cap,
            Some((
                &directory.join("execution.stdout.part"),
                &directory.join("execution.stderr.part"),
            )),
        )?;
        self.routing_note(
            id,
            "generation_launched",
            "OwnedProcess::spawn",
            Some(handle.identity.pid),
            None,
        )?;
        let result = handle.wait_observed_optional(
            limit_enforced.then_some(task.assignment.spec.generation_timeout_ms),
            |observation| {
                if self.task(id)?.cancel_requested {
                    return Ok(ProcessControl::Cancel);
                }
                progress.observe(observation)
            },
        )?;
        progress.finish(&result)?;
        let receipt = process_receipt(
            result,
            command.argv(),
            &task.assignment.workspace,
            &directory,
            "execution",
        )?;
        let task = self.mutate_current(id, "generation_recorded", "Agents::execute", |t| {
            t.execution = Some(receipt.clone());
            Ok(())
        })?;
        if receipt.outcome != "succeeded" {
            self.retain_partial_patch(id)?;
            let status = match receipt.outcome.as_str() {
                "cancelled" => AgentStatus::Cancelled,
                "timed_out" | "failed" => AgentStatus::NeedsWork,
                _ => AgentStatus::Unknown,
            };
            let diag = bounded_stream_snippet(&receipt.stderr.artifact.path, 600)
                .or_else(|| bounded_stream_snippet(&receipt.stdout.artifact.path, 600));
            let base_err = format!(
                "agent execution {}{}",
                receipt.outcome,
                receipt
                    .error
                    .as_ref()
                    .map(|e| format!(": {e}"))
                    .unwrap_or_default()
            );
            let full_err = match diag {
                Some(snippet) => format!("{base_err} — {snippet}"),
                None => base_err,
            };
            return self.finish(id, status, Some(full_err));
        }
        if let Err(error) = task
            .assignment
            .repository
            .revalidate()
            .and_then(|_| self.scope(&task, &baseline))
        {
            self.retain_partial_patch(id)?;
            return self.finish(
                id,
                AgentStatus::NeedsWork,
                Some(format!("{error:#}; no automatic replay")),
            );
        }
        if task.assignment.spec.agentlet.is_some() {
            match crate::agentlet::report_from_receipt(&task.assignment.backend, &receipt) {
                Ok(report) => {
                    self.mutate_current(
                        id,
                        "agentlet_result_validated",
                        "agentlet::report_from_receipt",
                        |t| {
                            t.agentlet_result = Some(report);
                            Ok(())
                        },
                    )?;
                }
                Err(error) => {
                    self.retain_partial_patch(id)?;
                    return self.finish(
                        id,
                        AgentStatus::NeedsWork,
                        Some(format!(
                            "agentlet result rejected: {error:#}; no automatic replay"
                        )),
                    );
                }
            }
        }
        let task = self.mutate_current(id, "scope_checked", "Agents::execute", |t| {
            t.status = AgentStatus::Verifying;
            t.workspace_warning = workspace_path_warning(&t.assignment.workspace);
            Ok(())
        })?;
        let mut verify_command = Command::new(&task.assignment.verifier.executable.path);
        if task.assignment.spec.agentlet.is_some() {
            crate::agentlet::configure_environment(&mut verify_command, None);
        }
        verify_command
            .args(&task.assignment.verifier.args)
            .current_dir(&task.assignment.workspace);
        let verif_idle_ms = if task.assignment.spec.soft_timeouts {
            Some(
                task.assignment
                    .spec
                    .generation_idle_timeout_ms
                    .unwrap_or(360_000),
            )
        } else {
            None
        };
        let mut progress = crate::agent_progress::Monitor::new_with_mode(
            &self.root,
            id,
            "verification",
            &task.assignment.workspace,
            &[],
            task.assignment.spec.verification_timeout_ms,
            verif_idle_ms,
            limit_enforced,
        );
        let verify_handle = OwnedProcess::spawn(
            &mut verify_command,
            65536,
            Some((
                &directory.join("verification.stdout.part"),
                &directory.join("verification.stderr.part"),
            )),
        )?;
        self.routing_note(
            id,
            "verification_launched",
            "OwnedProcess::spawn",
            Some(verify_handle.identity.pid),
            None,
        )?;
        let verify_result = verify_handle.wait_observed_optional(
            limit_enforced.then_some(task.assignment.spec.verification_timeout_ms),
            |observation| {
                if self.task(id)?.cancel_requested {
                    return Ok(ProcessControl::Cancel);
                }
                progress.observe(observation)
            },
        )?;
        progress.finish(&verify_result)?;
        let verify_receipt = process_receipt(
            verify_result,
            task.assignment.verifier.argv(),
            &task.assignment.workspace,
            &directory,
            "verification",
        )?;
        let task = self.mutate_current(id, "verification_recorded", "Agents::execute", |t| {
            t.verification = Some(verify_receipt.clone());
            Ok(())
        })?;
        // The verifier itself must not have moved anything out of scope -
        // mirrors `Orchestrator::execute`'s own post-verification scope
        // re-check.
        if let Err(error) = self.scope(&task, &baseline) {
            self.retain_partial_patch(id)?;
            return self.finish(
                id,
                AgentStatus::NeedsWork,
                Some(format!("{error:#}; no automatic replay")),
            );
        }
        let patch = self.patch(&task)?;
        let patch_artifact = artifact(&directory.join("patch.diff"), &patch)?;
        self.mutate_current(id, "patch_captured", "Agents::execute", |t| {
            t.patch = Some(patch_artifact.clone());
            Ok(())
        })?;
        let status = match verify_receipt.outcome.as_str() {
            "succeeded"
                if task
                    .agentlet_result
                    .as_ref()
                    .is_some_and(|r| r.outcome != crate::agentlet::Outcome::Completed) =>
            {
                AgentStatus::NeedsWork
            }
            "succeeded" => AgentStatus::AwaitingReview,
            "cancelled" => AgentStatus::Cancelled,
            "timed_out" | "failed" => AgentStatus::NeedsWork,
            _ => AgentStatus::Unknown,
        };
        let error = (status != AgentStatus::AwaitingReview).then(|| {
            if let Some(report) = &task.agentlet_result {
                format!(
                    "agentlet outcome {:?}; independent verification {}",
                    report.outcome, verify_receipt.outcome
                )
            } else {
                format!("independent verification {}", verify_receipt.outcome)
            }
        });
        self.finish(id, status, error)
    }
    fn finish(
        &mut self,
        id: &str,
        status: AgentStatus,
        error: Option<String>,
    ) -> Result<AgentTask> {
        let task = self.mutate_current(id, "finished", "Agents::finish", |t| {
            t.status = status;
            t.error = error;
            Ok(())
        })?;
        // Best-effort outcome telemetry update for standalone & workflow tasks alike.
        if task.execution.is_some()
            && let Ok(mut metrics) = crate::routing_metrics::RoutingMetricsStore::open(&self.root)
            && let Some(row) = crate::routing_metrics::build_row_from_parts(
                &task,
                task.attempt_history.len() as u32,
                task.execution.as_ref(),
                task.verification.as_ref(),
                task.review.as_ref(),
                None,
                Some(task.assignment.spec.model.as_str()),
                false,
                task.assignment
                    .shared_workspace
                    .as_ref()
                    .map(|s| s.workflow_id.as_str()),
                None,
                None,
                None,
                false,
                false,
                &self.root,
            )
        {
            let _ = metrics.upsert_row(&row);
        }
        Ok(task)
    }
    pub(crate) fn patch(&self, task: &AgentTask) -> Result<Vec<u8>> {
        let workspace = &task.assignment.workspace;
        if let Some(shared) = &task.assignment.shared_workspace {
            let tree = crate::workflow_workspace::candidate_tree(
                &self.root,
                workspace,
                &task.assignment.repository.base,
                &shared.candidate_scope,
            )?;
            return git(
                workspace,
                &[
                    "diff",
                    "--binary",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--no-renames",
                    &task.assignment.repository.base,
                    &tree,
                    "--",
                ],
            );
        }
        let mut patch = git(
            workspace,
            &[
                "diff",
                "--binary",
                "--no-ext-diff",
                "--no-textconv",
                &task.assignment.repository.base,
                "--",
            ],
        )?;
        for file in git(
            workspace,
            &["ls-files", "--others", "--exclude-standard", "-z"],
        )?
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        {
            patch.extend(new_file_diff(workspace, std::str::from_utf8(file)?)?);
            ensure!(patch.len() <= 4 * 1024 * 1024, "patch exceeds 4 MiB");
        }
        Ok(patch)
    }

    /// Mandatory human review: the only way a completed attempt can
    /// become `Accepted`. Re-validates the repository and worktree scope
    /// exactly as `review` does for every other engine in this crate -
    /// nothing may have drifted since the task reached `AwaitingReview`.
    pub fn review(
        &mut self,
        id: &str,
        revision: u64,
        expected_evidence_digest: &str,
        by: &str,
        accept: bool,
    ) -> Result<AgentTask> {
        ensure!(!by.trim().is_empty(), "reviewer label required");
        let task = self.task(id)?;
        ensure!(
            task.status == AgentStatus::AwaitingReview,
            "review requires a task awaiting review"
        );
        ensure!(
            task.assignment.shared_workspace.is_none(),
            "review the aggregate parent workflow; shared-worktree child evidence is a stage checkpoint"
        );
        task.assignment.repository.revalidate()?;
        let baseline: BTreeMap<String, Option<String>> = task
            .assignment
            .spec
            .verification_files
            .iter()
            .map(|p| {
                Ok((
                    p.clone(),
                    verification_file_hash(&safe_file(&task.assignment.workspace, p)?)?,
                ))
            })
            .collect::<Result<_>>()?;
        self.scope(&task, &baseline)?;
        let evidence = self.evidence_digest(&task)?;
        ensure!(
            evidence == expected_evidence_digest,
            "review requires the current evidence digest"
        );
        let reviewed = self.mutate(id, revision, "result_reviewed", "Agents::review", |t| {
            t.review = Some(Review {
                reviewer: by.into(),
                accepted: accept,
                evidence_digest: evidence.clone(),
                at: now(),
                comments: Vec::new(),
            });
            t.status = if accept {
                AgentStatus::Accepted
            } else {
                AgentStatus::Rejected
            };
            Ok(())
        })?;
        if let Ok(mut metrics) = crate::routing_metrics::RoutingMetricsStore::open(&self.root) {
            let clean_first_try = accept
                && reviewed.attempt_history.is_empty()
                && reviewed
                    .verification
                    .as_ref()
                    .is_some_and(|v| v.outcome == "succeeded");
            let _ = metrics.mark_task_review(id, accept, clean_first_try);
        }
        Ok(reviewed)
    }
    pub fn evidence_digest(&self, task: &AgentTask) -> Result<String> {
        if task.assignment.spec.agentlet.is_some()
            && matches!(
                task.status,
                AgentStatus::AwaitingReview | AgentStatus::Accepted | AgentStatus::Rejected
            )
        {
            ensure!(
                task.agentlet_result.is_some(),
                "completed agentlet result is missing"
            );
        }
        if let Some(report) = &task.agentlet_result {
            ensure!(
                task.assignment.spec.agentlet.is_some(),
                "agentlet result without an approved definition"
            );
            let execution = task
                .execution
                .as_ref()
                .context("agentlet result without execution")?;
            ensure!(
                crate::agentlet::report_from_receipt(&task.assignment.backend, execution)?
                    == *report,
                "agentlet result changed after execution"
            );
            return Ok(crate::digest(&serde_json::to_vec(&(
                &task.assignment,
                &task.execution,
                &task.verification,
                &task.patch,
                report,
            ))?));
        }
        Ok(crate::digest(&serde_json::to_vec(&(
            &task.assignment,
            &task.execution,
            &task.verification,
            &task.patch,
        ))?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protected_file_hash_covers_bytes_beyond_the_editable_text_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Cargo.lock");
        let mut bytes = vec![b'x'; 170_176];
        fs::write(&path, &bytes).unwrap();
        assert_eq!(
            verification_file_hash(&path).unwrap(),
            Some(crate::digest(&bytes))
        );
        bytes[170_175] = b'y';
        fs::write(&path, &bytes).unwrap();
        assert_eq!(
            verification_file_hash(&path).unwrap(),
            Some(crate::digest(&bytes))
        );
        fs::write(&path, vec![0_u8; 16 * 1024 * 1024 + 1]).unwrap();
        assert!(verification_file_hash(&path).is_err());
    }

    #[test]
    fn spec_round_trips_through_json_without_unknown_fields() {
        let spec = AgentSpec {
            agentlet: None,
            repository: PathBuf::from("."),
            objective: "x".into(),
            acceptance: "y".into(),
            allowed_files: vec!["a.txt".into()],
            verification_files: vec!["v.txt".into()],
            verify_executable: PathBuf::from("verifier.exe"),
            verify_args: vec![],
            target: "cursor-agent".into(),
            model: "gpt-5".into(),
            generation_timeout_ms: 5000,
            generation_idle_timeout_ms: None,
            verification_timeout_ms: 5000,
            soft_timeouts: false,
            effort: None,
        };
        let text = serde_json::to_string(&spec).unwrap();
        let back: AgentSpec = serde_json::from_str(&text).unwrap();
        assert_eq!(back.objective, "x");
    }

    #[test]
    fn parse_agy_models_output_extracts_live_catalog_and_tiers() {
        let sample = "\
Fetching available models...\n\
gemini-4-argon-medium\tGemini 4 Argon (Medium)\n\
gemini-4-hydrogen-high\tGemini 4 Hydrogen (High)\n\
gemini-3.8-flash-low\tGemini 3.8 Flash (Low)\n\
claude-opus-5-5-high\tClaude Opus 5.5 (High)\n\
claude-sonnet-5-5-medium\tClaude Sonnet 5.5 (Medium)\n\
gemini-4-argon-medium\tGemini 4 Argon (Medium)\n";
        let parsed = parse_agy_models_output(sample);
        assert_eq!(
            parsed,
            vec![
                "gemini-4-argon-medium",
                "gemini-4-hydrogen-high",
                "gemini-3.8-flash-low",
                "claude-opus-5-5-high",
                "claude-sonnet-5-5-medium",
            ]
        );
        assert_eq!(
            ModelTier::infer("agy", "gemini-3.8-flash-low"),
            ModelTier::Fast
        );
        assert_eq!(
            ModelTier::infer("agy", "gemini-4-argon-medium"),
            ModelTier::Standard
        );
        assert_eq!(
            ModelTier::infer("agy", "gemini-4-hydrogen-high"),
            ModelTier::Frontier
        );
        assert_eq!(
            ModelTier::infer("agy", "claude-opus-5-5-high"),
            ModelTier::Frontier
        );
    }

    #[test]
    fn agy_command_uses_accept_edits_instead_of_unavailable_auto_mode() {
        let dir = tempfile::tempdir().unwrap();
        let exe = std::env::current_exe().unwrap();
        let policy = AgyAgentPolicy {
            executable: Executable::new(&exe).unwrap(),
            version: "fixture 1".into(),
            model: "gemini-3.8-flash-low".into(),
        };
        let cmd = policy.command("fix bug", dir.path());
        assert!(
            cmd.args.windows(2).any(|w| w == ["--mode", "accept-edits"]),
            "expected --mode accept-edits in {:?}",
            cmd.args
        );
        assert!(
            cmd.args
                .windows(2)
                .any(|w| w == ["--output-format", "stream-json"]),
            "expected --output-format stream-json in {:?}",
            cmd.args
        );
        assert!(
            !cmd.args.windows(2).any(|w| w == ["--mode", "auto"]),
            "unexpected deprecated --mode auto in {:?}",
            cmd.args
        );
        assert!(
            cmd.args
                .iter()
                .any(|a| a == "--dangerously-skip-permissions")
        );
    }

    #[test]
    fn agent_retry_keeps_or_resets_worktree_and_preserves_attempt_history() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let repo = dir.path().join("repo");
        fs::create_dir_all(&state).unwrap();
        fs::create_dir_all(repo.join("src")).unwrap();
        git(&repo, &["init", "-b", "main"]).unwrap();
        git(&repo, &["config", "core.autocrlf", "false"]).unwrap();
        fs::write(repo.join("Cargo.toml"), "[package]\nname = \"a\"\n").unwrap();
        fs::write(repo.join("src/lib.rs"), "pub fn x() -> i32 { 0 }\n").unwrap();
        git(&repo, &["add", "."]).unwrap();
        git(
            &repo,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                "init",
            ],
        )
        .unwrap();

        let exe = std::env::current_exe().unwrap();
        let mut agents = Agents::open(&state).unwrap();
        let task = agents
            .create(
                AgentSpec {
                    agentlet: None,
                    repository: repo.clone(),
                    objective: "update x".into(),
                    acceptance: "passes".into(),
                    allowed_files: vec!["src/lib.rs".into()],
                    verification_files: vec!["Cargo.toml".into()],
                    verify_executable: exe.clone(),
                    verify_args: vec![],
                    target: "agy".into(),
                    model: "gemini-4-neon-medium".into(),
                    generation_timeout_ms: 5_000,
                    generation_idle_timeout_ms: None,
                    verification_timeout_ms: 5_000,
                    soft_timeouts: false,
                    effort: None,
                },
                AgentBackend::Agy(AgyAgentPolicy {
                    executable: Executable::new(&exe).unwrap(),
                    version: "1.5.3".into(),
                    model: "gemini-4-argon-medium".into(),
                }),
            )
            .unwrap();

        // Check board::agent_detail uses spec.model ("gemini-4-neon-medium"), not preset default.
        let detail = crate::board::agent_detail(&task, None).unwrap();
        assert!(
            detail.agent.contains("gemini-4-neon-medium"),
            "expected spec.model in detail.agent, got {}",
            detail.agent
        );

        // Simulate a failed attempt with partial edits in the agent worktree.
        let ws = task.assignment.workspace.clone();
        fs::create_dir_all(ws.parent().unwrap()).unwrap();
        git(
            &repo,
            &[
                "-c",
                "core.autocrlf=false",
                "worktree",
                "add",
                "--detach",
                ws.to_str().unwrap(),
                &task.assignment.repository.base,
            ],
        )
        .unwrap();
        fs::write(ws.join("src/lib.rs"), "pub fn x() -> i32 { 42 }\n").unwrap();
        let failed = agents
            .finish(&task.id, AgentStatus::NeedsWork, Some("timed out".into()))
            .unwrap();

        // Retry keeping workspace edits.
        let kept = agents
            .retry(&task.id, failed.revision, "Mike", false)
            .unwrap();
        assert_eq!(kept.status, AgentStatus::Approved);
        assert_eq!(kept.attempt_history.len(), 1);
        assert_eq!(
            fs::read_to_string(ws.join("src/lib.rs")).unwrap(),
            "pub fn x() -> i32 { 42 }\n"
        );
        agents.prepare_workspace(&kept).unwrap();

        // Mark cancelled, then restart fresh (resets worktree to base).
        let cancelled = agents.cancel(&task.id, kept.revision).unwrap();
        let reset = agents
            .retry(&task.id, cancelled.revision, "Mike", true)
            .unwrap();
        assert_eq!(reset.status, AgentStatus::Approved);
        assert_eq!(
            fs::read_to_string(ws.join("src/lib.rs")).unwrap(),
            "pub fn x() -> i32 { 0 }\n"
        );
        let detail2 = crate::board::agent_detail(&reset, None).unwrap();
        assert_eq!(detail2.attempts.len(), 1);
    }

    #[test]
    fn standalone_agent_retry_rotates_to_alternate_cli_preset_and_bumps_effort() {
        let tmp = tempfile::tempdir().unwrap();
        // Production canonicalizes every root at entry; CI's Windows TEMP is an 8.3 short
        // path (RUNNER~1), so use the canonical form here too or path identity checks fail.
        let dir_root = crate::repository::canonical(tmp.path()).unwrap();
        let state = dir_root.as_path().join("state");
        let repo = dir_root.as_path().join("repo");
        fs::create_dir_all(&state).unwrap();
        fs::create_dir_all(repo.join("src")).unwrap();
        git(&repo, &["init", "-b", "main"]).unwrap();
        git(&repo, &["config", "core.autocrlf", "false"]).unwrap();
        fs::write(repo.join("Cargo.toml"), "[package]\nname = \"r\"\n").unwrap();
        fs::write(repo.join("src/lib.rs"), "pub fn y() -> i32 { 0 }\n").unwrap();
        git(&repo, &["add", "."]).unwrap();
        git(
            &repo,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                "init",
            ],
        )
        .unwrap();

        let exe = std::env::current_exe().unwrap();
        let codex_home = dir_root.as_path().join("codex-home");
        fs::create_dir_all(&codex_home).unwrap();
        let mut presets = crate::agent_preset::AgentPresets::open(&state).unwrap();
        let agy_preset = presets
            .create(
                "alt-agy",
                repo.clone(),
                AgentBackend::Agy(AgyAgentPolicy {
                    executable: Executable::new(&exe).unwrap(),
                    version: "1.5.3".into(),
                    model: "gemini-4-argon-medium".into(),
                }),
                vec!["src/lib.rs".into()],
                vec!["Cargo.toml".into()],
                exe.clone(),
                vec![],
                5_000,
                5_000,
                None,
                "test",
            )
            .unwrap();
        let _codex_preset = presets
            .create(
                "alt-codex",
                repo.clone(),
                AgentBackend::Codex(crate::codex::CodexPolicy {
                    executable: Executable::new(&exe).unwrap(),
                    version: "codex-cli 0.166.0".into(),
                    home: codex_home,
                    model: "gpt-6.2-astra".into(),
                    verifier: crate::codex::PinnedCommand {
                        executable: Executable::new(&exe).unwrap(),
                        args: vec![],
                    },
                }),
                vec!["src/lib.rs".into()],
                vec!["Cargo.toml".into()],
                exe,
                vec![],
                5_000,
                5_000,
                None,
                "test",
            )
            .unwrap();

        let mut agents = Agents::open(&state).unwrap();
        let task = agents
            .create(
                agy_preset.spec("fix y".into(), "passes".into()),
                agy_preset.backend,
            )
            .unwrap();
        let failed = agents
            .finish(
                &task.id,
                AgentStatus::NeedsWork,
                Some("failed check".into()),
            )
            .unwrap();

        let retried = agents
            .retry(&task.id, failed.revision, "Mike", false)
            .unwrap();
        assert_eq!(retried.assignment.backend.target(), "codex");
        assert_eq!(retried.assignment.spec.target, "codex");
        assert_eq!(retried.assignment.spec.model, "gpt-6.2-astra");
        assert_eq!(retried.assignment.spec.effort, Some(ExecutionEffort::High));
        assert_eq!(retried.attempt_history[0].target.as_deref(), Some("agy"));
        assert_eq!(
            retried.attempt_history[0].model.as_deref(),
            Some("gemini-4-argon-medium")
        );
    }
}
