//! Normalized CLI/model execution telemetry and outcome-weighted routing scoreboard.
//!
//! Tracks real execution outcomes (execution pass/fail, timeouts, quota exhaustion,
//! verification pass/fail, human accept/exceptional/reject, and manual redo/retry) alongside
//! normalized token and turn receipts from AGY, Codex, Cursor, and Local workers.
//!
//! The router uses this scoreboard to boost `(cli, model)` pairs that succeed or perform
//! exceptionally for a task complexity, penalize pairs that fail, time out, or require redos,
//! and break cold-start ties fairly across CLIs instead of relying on catalog order.

use crate::{now, open_sqlite_with_retry};
use anyhow::{Context, Result};
use rusqlite::{Connection, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
};

const MAX_STDOUT_PARSE_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CliUsageMetrics {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    pub thinking_tokens: u64,
    pub total_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub num_turns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cli_duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

/// Extracts the actual `--model` / `-m` value passed to the delegated CLI process,
/// avoiding preset-vs-override drift when historical records stored the preset default.
pub fn extract_model_from_command(cmd: &[String]) -> Option<String> {
    for (idx, arg) in cmd.iter().enumerate() {
        if (arg == "--model" || arg == "-m")
            && let Some(val) = cmd.get(idx + 1)
        {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
        if let Some(rest) = arg.strip_prefix("--model=") {
            let trimmed = rest.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

/// Extracts `--effort` or `-c model_reasoning_effort=...` from a recorded CLI command.
pub fn extract_effort_from_command(cmd: &[String]) -> Option<String> {
    for (idx, arg) in cmd.iter().enumerate() {
        if arg == "--effort"
            && let Some(val) = cmd.get(idx + 1)
        {
            let trimmed = val.trim().to_ascii_lowercase();
            if !trimmed.is_empty() {
                return Some(trimmed);
            }
        }
        if let Some(rest) = arg.strip_prefix("--effort=") {
            let trimmed = rest.trim().to_ascii_lowercase();
            if !trimmed.is_empty() {
                return Some(trimmed);
            }
        }
        if arg == "-c"
            && let Some(val) = cmd.get(idx + 1)
            && let Some(effort_val) = val.strip_prefix("model_reasoning_effort=")
        {
            let cleaned = effort_val.trim().trim_matches('"').to_ascii_lowercase();
            if !cleaned.is_empty() {
                return Some(cleaned);
            }
        }
    }
    None
}

fn read_bounded_utf8(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    file.take(MAX_STDOUT_PARSE_BYTES)
        .read_to_end(&mut buf)
        .ok()?;
    if buf.is_empty() {
        return None;
    }
    Some(String::from_utf8_lossy(&buf).into_owned())
}

fn u64_any(obj: &Value, keys: &[&str]) -> u64 {
    for key in keys {
        if let Some(v) = obj.get(*key).and_then(|v| v.as_u64()) {
            return v;
        }
    }
    0
}

fn parse_usage_object(usage: &Value) -> CliUsageMetrics {
    let input = u64_any(usage, &["input_tokens", "inputTokens", "prompt_tokens"]);
    let output = u64_any(
        usage,
        &["output_tokens", "outputTokens", "completion_tokens"],
    );
    let cached = u64_any(
        usage,
        &[
            "cache_read_tokens",
            "cached_input_tokens",
            "cacheReadTokens",
            "cached_tokens",
        ],
    );
    let thinking = u64_any(
        usage,
        &[
            "thinking_tokens",
            "reasoning_output_tokens",
            "reasoningTokens",
        ],
    );
    let total_raw = u64_any(usage, &["total_tokens", "totalTokens"]);
    let total = if total_raw > 0 {
        total_raw
    } else {
        input.saturating_add(output)
    };
    CliUsageMetrics {
        input_tokens: input,
        output_tokens: output,
        cached_tokens: cached,
        thinking_tokens: thinking,
        total_tokens: total,
        ..Default::default()
    }
}

/// Parses token, turn, and CLI-reported duration telemetry from raw stdout logs produced by
/// `agy`, `codex`, `cursor-agent`, or local JSON workers.
pub fn parse_cli_usage_from_path(_backend: &str, stdout_path: &Path) -> Option<CliUsageMetrics> {
    let text = read_bounded_utf8(stdout_path)?;
    parse_cli_usage_from_str(&text)
}

pub fn parse_cli_usage_from_str(text: &str) -> Option<CliUsageMetrics> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }

    // Try single JSON document first (AGY `--output-format json`, Cursor `--output-format json`).
    if let Ok(doc) = serde_json::from_str::<Value>(trimmed)
        && let Some(parsed) = extract_metrics_from_json(&doc)
    {
        return Some(parsed);
    }

    // Fallback to JSONL stream (Codex `--json`, AGY `stream-json`).
    let mut best: Option<CliUsageMetrics> = None;
    let mut assistant_turns: u64 = 0;
    for line in trimmed
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with('{'))
    {
        let Ok(obj) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if obj.get("type").and_then(|v| v.as_str()) == Some("item.completed")
            && obj
                .get("item")
                .and_then(|i| i.get("type"))
                .and_then(|t| t.as_str())
                == Some("agent_message")
        {
            assistant_turns += 1;
        }
        if let Some(mut m) = extract_metrics_from_json(&obj) {
            if m.num_turns.is_none() && assistant_turns > 0 {
                m.num_turns = Some(assistant_turns);
            }
            best = Some(m);
        }
    }
    best
}

fn extract_metrics_from_json(doc: &Value) -> Option<CliUsageMetrics> {
    let usage = doc.get("usage").filter(|v| v.is_object())?;
    let mut m = parse_usage_object(usage);
    m.num_turns = doc.get("num_turns").and_then(|v| v.as_u64());
    m.cli_duration_ms = doc
        .get("duration_seconds")
        .and_then(|v| v.as_f64())
        .filter(|secs| secs.is_finite() && *secs >= 0.0)
        .map(|secs| (secs * 1000.0).round() as u64);
    m.session_id = doc
        .get("session_id")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    m.request_id = doc
        .get("request_id")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    Some(m)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliModelOutcomeRow {
    pub task_id: String,
    pub attempt_seq: u32,
    pub workflow_id: Option<String>,
    pub profile_name: Option<String>,
    pub worker: Option<String>,
    pub backend: String,
    pub model: String,
    pub tier: String,
    pub complexity: String,
    pub effort: Option<String>,
    pub exec_outcome: String,
    pub verify_outcome: Option<String>,
    pub review_outcome: String,
    pub quota_exhausted: bool,
    pub timed_out: bool,
    pub exec_failed: bool,
    pub verify_failed: bool,
    pub redone: bool,
    pub exceptional: bool,
    pub duration_ms: u64,
    pub verify_ms: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    pub thinking_tokens: u64,
    pub num_turns: u64,
    pub recorded_at: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct OutcomeAggregate {
    pub attempts: u64,
    pub exec_succeeded: u64,
    pub exec_failed: u64,
    pub timed_out: u64,
    pub quota_exhausted: u64,
    pub verify_passed: u64,
    pub verify_failed: u64,
    pub accepted: u64,
    pub exceptional: u64,
    pub rejected: u64,
    pub redone: u64,
    pub total_duration_ms: u64,
    pub total_verify_ms: u64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cached_tokens: u64,
    pub total_thinking_tokens: u64,
    pub total_turns: u64,
    pub last_used_at: u64,
    signal_sum: f64,
}

impl OutcomeAggregate {
    fn observe(&mut self, row: &CliModelOutcomeRow) {
        self.attempts += 1;
        if row.exec_outcome == "succeeded" {
            self.exec_succeeded += 1;
        }
        if row.exec_failed {
            self.exec_failed += 1;
        }
        if row.timed_out {
            self.timed_out += 1;
        }
        if row.quota_exhausted {
            self.quota_exhausted += 1;
        }
        match row.verify_outcome.as_deref() {
            Some("succeeded") => self.verify_passed += 1,
            Some("failed") | Some("timed_out") => self.verify_failed += 1,
            _ => {}
        }
        if row.review_outcome == "accepted" || row.exceptional {
            self.accepted += 1;
        }
        if row.exceptional {
            self.exceptional += 1;
        }
        if row.review_outcome == "rejected" {
            self.rejected += 1;
        }
        if row.redone {
            self.redone += 1;
        }
        self.total_duration_ms = self.total_duration_ms.saturating_add(row.duration_ms);
        self.total_verify_ms = self.total_verify_ms.saturating_add(row.verify_ms);
        self.total_input_tokens = self.total_input_tokens.saturating_add(row.input_tokens);
        self.total_output_tokens = self.total_output_tokens.saturating_add(row.output_tokens);
        self.total_cached_tokens = self.total_cached_tokens.saturating_add(row.cached_tokens);
        self.total_thinking_tokens = self
            .total_thinking_tokens
            .saturating_add(row.thinking_tokens);
        self.total_turns = self.total_turns.saturating_add(row.num_turns);
        self.last_used_at = self.last_used_at.max(row.recorded_at);
        self.signal_sum += outcome_signal(row);
    }

    pub fn avg_duration_ms(&self) -> u64 {
        self.total_duration_ms
            .checked_div(self.attempts)
            .unwrap_or(0)
    }

    pub fn pass_rate(&self) -> f64 {
        let verified_total = self.verify_passed + self.verify_failed;
        if verified_total > 0 {
            self.verify_passed as f64 / verified_total as f64
        } else if self.attempts > 0 {
            self.exec_succeeded as f64 / self.attempts as f64
        } else {
            0.0
        }
    }

    pub fn smoothed_weight(&self) -> f64 {
        const PRIOR_K: f64 = 2.0;
        if self.attempts == 0 {
            return 0.0;
        }
        let speed_nudge = if self.avg_duration_ms() == 0 {
            0.0
        } else if self.avg_duration_ms() <= 45_000 && self.verify_passed > 0 {
            0.10
        } else if self.avg_duration_ms() >= 240_000 {
            -0.12
        } else {
            0.0
        };
        (self.signal_sum / (self.attempts as f64 + PRIOR_K)) + speed_nudge
    }
}

/// Computes the signed scalar reward/penalty for a single observed task attempt.
fn outcome_signal(row: &CliModelOutcomeRow) -> f64 {
    let mut s = 0.0_f64;

    if row.exceptional {
        s += 1.40;
    } else if row.review_outcome == "accepted" {
        s += 1.00;
    } else if row.verify_outcome.as_deref() == Some("succeeded") {
        s += 0.65;
    } else if row.exec_outcome == "succeeded" && row.verify_outcome.is_none() {
        s += 0.15;
    }

    if row.verify_failed {
        s -= 0.75;
    }
    if row.exec_failed {
        s -= 0.95;
    }
    if row.timed_out {
        s -= 1.35;
    }
    if row.quota_exhausted {
        s -= 0.90;
    }
    if row.review_outcome == "rejected" {
        s -= 1.15;
    }
    if row.redone {
        s -= 1.10;
    }

    s.clamp(-1.8, 1.5)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CandidateScore {
    pub backend: String,
    pub model: String,
    pub complexity: String,
    pub total_score: f64,
    pub learned_weight: f64,
    pub complexity_fit: f64,
    pub complexity_attempts: u64,
    pub global_attempts: u64,
    pub last_used_at: u64,
}

#[derive(Debug, Clone, Default)]
pub struct CliModelScoreboard {
    by_pair_complexity: HashMap<(String, String, String), OutcomeAggregate>,
    by_pair: HashMap<(String, String), OutcomeAggregate>,
    by_backend: HashMap<String, OutcomeAggregate>,
    rows: Vec<CliModelOutcomeRow>,
}

impl CliModelScoreboard {
    pub fn from_rows(rows: Vec<CliModelOutcomeRow>) -> Self {
        let mut board = Self::default();
        for row in rows {
            let b = row.backend.to_ascii_lowercase();
            let m = row.model.to_ascii_lowercase();
            let c = row.complexity.to_ascii_lowercase();
            board
                .by_pair_complexity
                .entry((b.clone(), m.clone(), c))
                .or_default()
                .observe(&row);
            board
                .by_pair
                .entry((b.clone(), m))
                .or_default()
                .observe(&row);
            board.by_backend.entry(b).or_default().observe(&row);
            board.rows.push(row);
        }
        board
    }

    /// Blends `(backend, model, complexity)` history with global `(backend, model)` shrinkage
    /// and a complexity/effort/tier appropriateness score so proven performers rise and failures/
    /// timeouts/redos drop cleanly.
    pub fn score_candidate(
        &self,
        backend: &str,
        model: &str,
        complexity: &str,
        tier_fit: f64,
    ) -> CandidateScore {
        let b = backend.trim().to_ascii_lowercase();
        let m = model.trim().to_ascii_lowercase();
        let c = complexity.trim().to_ascii_lowercase();

        let scoped = self
            .by_pair_complexity
            .get(&(b.clone(), m.clone(), c.clone()));
        let global = self.by_pair.get(&(b, m));

        let scoped_attempts = scoped.map_or(0, |a| a.attempts);
        let global_attempts = global.map_or(0, |a| a.attempts);
        let last_used_at = global.map_or(0, |a| a.last_used_at);

        let learned_weight = match (scoped, global) {
            (Some(s), Some(g)) => {
                // Blend task-complexity-specific performance (weight 0.70) with overall pair
                // track record (weight 0.30).
                (0.70 * s.smoothed_weight()) + (0.30 * g.smoothed_weight())
            }
            (None, Some(g)) => 0.65 * g.smoothed_weight(),
            (Some(s), None) => s.smoothed_weight(),
            (None, None) => 0.0,
        };

        CandidateScore {
            backend: backend.to_string(),
            model: model.to_string(),
            complexity: complexity.to_string(),
            total_score: tier_fit + learned_weight,
            learned_weight,
            complexity_fit: tier_fit,
            complexity_attempts: scoped_attempts,
            global_attempts,
            last_used_at,
        }
    }

    pub fn pair_attempts(&self, backend: &str, model: &str) -> u64 {
        self.by_pair
            .get(&(backend.to_ascii_lowercase(), model.to_ascii_lowercase()))
            .map_or(0, |a| a.attempts)
    }

    pub fn pair_last_used(&self, backend: &str, model: &str) -> u64 {
        self.by_pair
            .get(&(backend.to_ascii_lowercase(), model.to_ascii_lowercase()))
            .map_or(0, |a| a.last_used_at)
    }

    pub fn backend_attempts(&self, backend: &str) -> u64 {
        self.by_backend
            .get(&backend.to_ascii_lowercase())
            .map_or(0, |a| a.attempts)
    }

    pub fn analytics_report(&self) -> AnalyticsReport {
        let mut by_pair_complexity: Vec<AnalyticsPairRow> = self
            .by_pair_complexity
            .iter()
            .map(|((backend, model, complexity), agg)| {
                let score = self.score_candidate(backend, model, complexity, 0.0);
                AnalyticsPairRow::from_agg(
                    backend.clone(),
                    model.clone(),
                    Some(complexity.clone()),
                    agg,
                    score.learned_weight,
                )
            })
            .collect();
        by_pair_complexity.sort_by(|a, b| {
            b.learned_weight
                .total_cmp(&a.learned_weight)
                .then_with(|| b.attempts.cmp(&a.attempts))
                .then_with(|| a.backend.cmp(&b.backend))
                .then_with(|| a.model.cmp(&b.model))
        });

        let mut by_model: Vec<AnalyticsPairRow> = self
            .by_pair
            .iter()
            .map(|((backend, model), agg)| {
                AnalyticsPairRow::from_agg(
                    backend.clone(),
                    model.clone(),
                    None,
                    agg,
                    agg.smoothed_weight(),
                )
            })
            .collect();
        by_model.sort_by(|a, b| {
            b.learned_weight
                .total_cmp(&a.learned_weight)
                .then_with(|| b.attempts.cmp(&a.attempts))
                .then_with(|| a.backend.cmp(&b.backend))
                .then_with(|| a.model.cmp(&b.model))
        });

        let mut by_backend: Vec<AnalyticsBackendRow> = self
            .by_backend
            .iter()
            .map(|(backend, agg)| AnalyticsBackendRow {
                backend: backend.clone(),
                attempts: agg.attempts,
                exec_succeeded: agg.exec_succeeded,
                exec_failed: agg.exec_failed,
                timed_out: agg.timed_out,
                quota_exhausted: agg.quota_exhausted,
                verify_passed: agg.verify_passed,
                verify_failed: agg.verify_failed,
                accepted: agg.accepted,
                exceptional: agg.exceptional,
                rejected: agg.rejected,
                redone: agg.redone,
                pass_rate: agg.pass_rate(),
                avg_duration_ms: agg.avg_duration_ms(),
                total_input_tokens: agg.total_input_tokens,
                total_output_tokens: agg.total_output_tokens,
                total_cached_tokens: agg.total_cached_tokens,
                total_thinking_tokens: agg.total_thinking_tokens,
                learned_weight: agg.smoothed_weight(),
            })
            .collect();
        by_backend.sort_by(|a, b| {
            b.learned_weight
                .total_cmp(&a.learned_weight)
                .then_with(|| b.attempts.cmp(&a.attempts))
        });

        let mut totals = OutcomeAggregate::default();
        for row in &self.rows {
            totals.observe(row);
        }

        let mut recent = self.rows.clone();
        recent.sort_by_key(|b| std::cmp::Reverse(b.recorded_at));
        recent.truncate(25);

        AnalyticsReport {
            generated_at: now(),
            total_attempts: totals.attempts,
            total_input_tokens: totals.total_input_tokens,
            total_output_tokens: totals.total_output_tokens,
            total_cached_tokens: totals.total_cached_tokens,
            total_thinking_tokens: totals.total_thinking_tokens,
            by_backend,
            by_model,
            by_pair_complexity,
            recent_outcomes: recent,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalyticsPairRow {
    pub backend: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub complexity: Option<String>,
    pub attempts: u64,
    pub exec_succeeded: u64,
    pub exec_failed: u64,
    pub timed_out: u64,
    pub quota_exhausted: u64,
    pub verify_passed: u64,
    pub verify_failed: u64,
    pub accepted: u64,
    pub exceptional: u64,
    pub rejected: u64,
    pub redone: u64,
    pub pass_rate: f64,
    pub avg_duration_ms: u64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cached_tokens: u64,
    pub total_thinking_tokens: u64,
    pub total_turns: u64,
    pub learned_weight: f64,
    pub last_used_at: u64,
}

impl AnalyticsPairRow {
    fn from_agg(
        backend: String,
        model: String,
        complexity: Option<String>,
        agg: &OutcomeAggregate,
        learned_weight: f64,
    ) -> Self {
        Self {
            backend,
            model,
            complexity,
            attempts: agg.attempts,
            exec_succeeded: agg.exec_succeeded,
            exec_failed: agg.exec_failed,
            timed_out: agg.timed_out,
            quota_exhausted: agg.quota_exhausted,
            verify_passed: agg.verify_passed,
            verify_failed: agg.verify_failed,
            accepted: agg.accepted,
            exceptional: agg.exceptional,
            rejected: agg.rejected,
            redone: agg.redone,
            pass_rate: agg.pass_rate(),
            avg_duration_ms: agg.avg_duration_ms(),
            total_input_tokens: agg.total_input_tokens,
            total_output_tokens: agg.total_output_tokens,
            total_cached_tokens: agg.total_cached_tokens,
            total_thinking_tokens: agg.total_thinking_tokens,
            total_turns: agg.total_turns,
            learned_weight,
            last_used_at: agg.last_used_at,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalyticsBackendRow {
    pub backend: String,
    pub attempts: u64,
    pub exec_succeeded: u64,
    pub exec_failed: u64,
    pub timed_out: u64,
    pub quota_exhausted: u64,
    pub verify_passed: u64,
    pub verify_failed: u64,
    pub accepted: u64,
    pub exceptional: u64,
    pub rejected: u64,
    pub redone: u64,
    pub pass_rate: f64,
    pub avg_duration_ms: u64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cached_tokens: u64,
    pub total_thinking_tokens: u64,
    pub learned_weight: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalyticsReport {
    pub generated_at: u64,
    pub total_attempts: u64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cached_tokens: u64,
    pub total_thinking_tokens: u64,
    pub by_backend: Vec<AnalyticsBackendRow>,
    pub by_model: Vec<AnalyticsPairRow>,
    pub by_pair_complexity: Vec<AnalyticsPairRow>,
    pub recent_outcomes: Vec<CliModelOutcomeRow>,
}

pub struct RoutingMetricsStore {
    connection: Connection,
    root: PathBuf,
}

impl RoutingMetricsStore {
    pub fn open(root: &Path) -> Result<Self> {
        let connection = open_sqlite_with_retry(&root.join("state.sqlite"), 1)
            .context("open routing metrics state")?;
        crate::ensure_table_schema(
            &connection,
            "cli_model_outcomes",
            "CREATE TABLE IF NOT EXISTS cli_model_outcomes(
                task_id TEXT NOT NULL,
                attempt_seq INTEGER NOT NULL,
                workflow_id TEXT,
                profile_name TEXT,
                worker TEXT,
                backend TEXT NOT NULL,
                model TEXT NOT NULL,
                tier TEXT NOT NULL,
                complexity TEXT NOT NULL,
                effort TEXT,
                exec_outcome TEXT NOT NULL,
                verify_outcome TEXT,
                review_outcome TEXT NOT NULL,
                quota_exhausted INTEGER NOT NULL DEFAULT 0,
                timed_out INTEGER NOT NULL DEFAULT 0,
                exec_failed INTEGER NOT NULL DEFAULT 0,
                verify_failed INTEGER NOT NULL DEFAULT 0,
                redone INTEGER NOT NULL DEFAULT 0,
                exceptional INTEGER NOT NULL DEFAULT 0,
                duration_ms INTEGER NOT NULL DEFAULT 0,
                verify_ms INTEGER NOT NULL DEFAULT 0,
                input_tokens INTEGER NOT NULL DEFAULT 0,
                output_tokens INTEGER NOT NULL DEFAULT 0,
                cached_tokens INTEGER NOT NULL DEFAULT 0,
                thinking_tokens INTEGER NOT NULL DEFAULT 0,
                num_turns INTEGER NOT NULL DEFAULT 0,
                recorded_at INTEGER NOT NULL,
                PRIMARY KEY(task_id, attempt_seq)
            );",
        )?;
        Ok(Self {
            connection,
            root: root.to_path_buf(),
        })
    }

    pub fn upsert_row(&mut self, row: &CliModelOutcomeRow) -> Result<()> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO cli_model_outcomes(
                task_id, attempt_seq, workflow_id, profile_name, worker, backend, model, tier,
                complexity, effort, exec_outcome, verify_outcome, review_outcome,
                quota_exhausted, timed_out, exec_failed, verify_failed, redone, exceptional,
                duration_ms, verify_ms, input_tokens, output_tokens, cached_tokens,
                thinking_tokens, num_turns, recorded_at
            ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26,?27)
            ON CONFLICT(task_id, attempt_seq) DO UPDATE SET
                workflow_id=COALESCE(excluded.workflow_id, cli_model_outcomes.workflow_id),
                profile_name=COALESCE(excluded.profile_name, cli_model_outcomes.profile_name),
                worker=COALESCE(excluded.worker, cli_model_outcomes.worker),
                backend=excluded.backend,
                model=excluded.model,
                tier=excluded.tier,
                complexity=excluded.complexity,
                effort=COALESCE(excluded.effort, cli_model_outcomes.effort),
                exec_outcome=excluded.exec_outcome,
                verify_outcome=excluded.verify_outcome,
                review_outcome=CASE
                    WHEN cli_model_outcomes.redone = 1 THEN cli_model_outcomes.review_outcome
                    WHEN cli_model_outcomes.review_outcome IN ('accepted', 'exceptional', 'rejected') THEN cli_model_outcomes.review_outcome
                    ELSE excluded.review_outcome
                END,
                quota_exhausted=MAX(cli_model_outcomes.quota_exhausted, excluded.quota_exhausted),
                timed_out=MAX(cli_model_outcomes.timed_out, excluded.timed_out),
                exec_failed=MAX(cli_model_outcomes.exec_failed, excluded.exec_failed),
                verify_failed=MAX(cli_model_outcomes.verify_failed, excluded.verify_failed),
                redone=MAX(cli_model_outcomes.redone, excluded.redone),
                exceptional=CASE
                    WHEN cli_model_outcomes.redone = 1 OR excluded.redone = 1 THEN 0
                    ELSE MAX(cli_model_outcomes.exceptional, excluded.exceptional)
                END,
                duration_ms=MAX(cli_model_outcomes.duration_ms, excluded.duration_ms),
                verify_ms=MAX(cli_model_outcomes.verify_ms, excluded.verify_ms),
                input_tokens=MAX(cli_model_outcomes.input_tokens, excluded.input_tokens),
                output_tokens=MAX(cli_model_outcomes.output_tokens, excluded.output_tokens),
                cached_tokens=MAX(cli_model_outcomes.cached_tokens, excluded.cached_tokens),
                thinking_tokens=MAX(cli_model_outcomes.thinking_tokens, excluded.thinking_tokens),
                num_turns=MAX(cli_model_outcomes.num_turns, excluded.num_turns),
                recorded_at=MAX(cli_model_outcomes.recorded_at, excluded.recorded_at)",
            params![
                row.task_id,
                row.attempt_seq,
                row.workflow_id,
                row.profile_name,
                row.worker,
                row.backend,
                row.model,
                row.tier,
                row.complexity,
                row.effort,
                row.exec_outcome,
                row.verify_outcome,
                row.review_outcome,
                i32::from(row.quota_exhausted),
                i32::from(row.timed_out),
                i32::from(row.exec_failed),
                i32::from(row.verify_failed),
                i32::from(row.redone),
                i32::from(row.exceptional),
                row.duration_ms as i64,
                row.verify_ms as i64,
                row.input_tokens as i64,
                row.output_tokens as i64,
                row.cached_tokens as i64,
                row.thinking_tokens as i64,
                row.num_turns as i64,
                row.recorded_at as i64,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn mark_task_review(
        &mut self,
        task_id: &str,
        accepted: bool,
        clean_first_try: bool,
    ) -> Result<()> {
        let review_outcome = if accepted {
            if clean_first_try {
                "exceptional"
            } else {
                "accepted"
            }
        } else {
            "rejected"
        };
        let exceptional = i32::from(accepted && clean_first_try);
        let redone = i32::from(!accepted);
        self.connection.execute(
            "UPDATE cli_model_outcomes
             SET review_outcome=?2, exceptional=?3, redone=MAX(redone, ?4)
             WHERE task_id=?1 AND attempt_seq=(SELECT MAX(attempt_seq) FROM cli_model_outcomes WHERE task_id=?1)",
            params![task_id, review_outcome, exceptional, redone],
        )?;
        Ok(())
    }

    pub fn mark_task_redo(&mut self, task_id: &str) -> Result<()> {
        self.connection.execute(
            "UPDATE cli_model_outcomes
             SET redone=1, exceptional=0,
                 review_outcome=CASE WHEN review_outcome='pending' THEN 'redone' ELSE review_outcome END
             WHERE task_id=?1 AND attempt_seq=(SELECT MAX(attempt_seq) FROM cli_model_outcomes WHERE task_id=?1)",
            params![task_id],
        )?;
        Ok(())
    }

    pub fn rows(&self) -> Result<Vec<CliModelOutcomeRow>> {
        let mut stmt = self.connection.prepare(
            "SELECT task_id, attempt_seq, workflow_id, profile_name, worker, backend, model, tier,
                    complexity, effort, exec_outcome, verify_outcome, review_outcome,
                    quota_exhausted, timed_out, exec_failed, verify_failed, redone, exceptional,
                    duration_ms, verify_ms, input_tokens, output_tokens, cached_tokens,
                    thinking_tokens, num_turns, recorded_at
             FROM cli_model_outcomes",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(CliModelOutcomeRow {
                    task_id: r.get(0)?,
                    attempt_seq: r.get::<_, u32>(1)?,
                    workflow_id: r.get(2)?,
                    profile_name: r.get(3)?,
                    worker: r.get(4)?,
                    backend: r.get(5)?,
                    model: r.get(6)?,
                    tier: r.get(7)?,
                    complexity: r.get(8)?,
                    effort: r.get(9)?,
                    exec_outcome: r.get(10)?,
                    verify_outcome: r.get(11)?,
                    review_outcome: r.get(12)?,
                    quota_exhausted: r.get::<_, i32>(13)? != 0,
                    timed_out: r.get::<_, i32>(14)? != 0,
                    exec_failed: r.get::<_, i32>(15)? != 0,
                    verify_failed: r.get::<_, i32>(16)? != 0,
                    redone: r.get::<_, i32>(17)? != 0,
                    exceptional: r.get::<_, i32>(18)? != 0,
                    duration_ms: r.get::<_, i64>(19)?.max(0) as u64,
                    verify_ms: r.get::<_, i64>(20)?.max(0) as u64,
                    input_tokens: r.get::<_, i64>(21)?.max(0) as u64,
                    output_tokens: r.get::<_, i64>(22)?.max(0) as u64,
                    cached_tokens: r.get::<_, i64>(23)?.max(0) as u64,
                    thinking_tokens: r.get::<_, i64>(24)?.max(0) as u64,
                    num_turns: r.get::<_, i64>(25)?.max(0) as u64,
                    recorded_at: r.get::<_, i64>(26)?.max(0) as u64,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Backfills/syncs `cli_model_outcomes` from `agent_tasks` and `workflows` tables when
    /// historical runs exist on disk that have not yet been indexed into `cli_model_outcomes`.
    pub fn sync_from_history(&mut self) -> Result<()> {
        let has_agent_tasks: Option<String> = self
            .connection
            .query_row(
                "SELECT name FROM sqlite_master WHERE type='table' AND name='agent_tasks'",
                [],
                |r| r.get(0),
            )
            .ok();
        if has_agent_tasks.is_none() {
            return Ok(());
        }

        // Map task_id -> (workflow_id, profile_name, worker, complexity, quota_exhausted, wf_status, prior_failures)
        #[derive(Clone)]
        struct StepMeta {
            workflow_id: String,
            profile_name: String,
            worker: String,
            complexity: String,
            model_override: Option<String>,
            quota_exhausted: bool,
            wf_accepted: bool,
            wf_rejected: bool,
            redone: bool,
            clean_first_try: bool,
        }
        let mut step_meta: HashMap<String, StepMeta> = HashMap::new();

        let has_workflows: Option<String> = self
            .connection
            .query_row(
                "SELECT name FROM sqlite_master WHERE type='table' AND name='workflows'",
                [],
                |r| r.get(0),
            )
            .ok();
        if has_workflows.is_some()
            && let Ok(mut stmt) = self.connection.prepare("SELECT record FROM workflows")
            && let Ok(records) = stmt.query_map([], |r| r.get::<_, String>(0))
        {
            for rec_str in records.flatten() {
                let Ok(w) = serde_json::from_str::<crate::workflow::Workflow>(&rec_str) else {
                    continue;
                };
                let wf_accepted = w.status == crate::agents::AgentStatus::Accepted;
                let wf_rejected = w.status == crate::agents::AgentStatus::Rejected;
                let profile_name = w.assignment.profile.name.clone();
                let had_any_failure_or_archive = !w.archived_steps.is_empty()
                    || w.steps.iter().any(|s| {
                        s.result
                            .as_ref()
                            .and_then(|r| r["verification_outcome"].as_str())
                            == Some("failed")
                    });

                for archived in &w.archived_steps {
                    let res = archived.step.result.as_ref();
                    step_meta.insert(
                        archived.step.task_id.clone(),
                        StepMeta {
                            workflow_id: w.id.clone(),
                            profile_name: profile_name.clone(),
                            worker: archived.step.worker.clone(),
                            complexity: res
                                .and_then(|r| r["complexity"].as_str())
                                .unwrap_or("standard")
                                .to_string(),
                            model_override: res
                                .and_then(|r| r["model"].as_str())
                                .map(str::to_string),
                            quota_exhausted: res.is_some_and(|r| r["quota_exhausted"] == true),
                            wf_accepted: false,
                            wf_rejected: false,
                            redone: true,
                            clean_first_try: false,
                        },
                    );
                }
                for (idx, s) in w.steps.iter().enumerate() {
                    let res = s.result.as_ref();
                    let quota = res.is_some_and(|r| r["quota_exhausted"] == true);
                    let ver_ok =
                        res.and_then(|r| r["verification_outcome"].as_str()) == Some("succeeded");
                    let clean_first_try =
                        wf_accepted && ver_ok && idx == 0 && !had_any_failure_or_archive;
                    step_meta.insert(
                        s.task_id.clone(),
                        StepMeta {
                            workflow_id: w.id.clone(),
                            profile_name: profile_name.clone(),
                            worker: s.worker.clone(),
                            complexity: res
                                .and_then(|r| r["complexity"].as_str())
                                .unwrap_or("standard")
                                .to_string(),
                            model_override: res
                                .and_then(|r| r["model"].as_str())
                                .map(str::to_string),
                            quota_exhausted: quota,
                            wf_accepted,
                            wf_rejected,
                            redone: false,
                            clean_first_try,
                        },
                    );
                }
            }
        }

        let records = {
            let mut stmt = self.connection.prepare("SELECT record FROM agent_tasks")?;
            stmt.query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };

        for rec_str in records {
            let Ok(task) = serde_json::from_str::<crate::agents::AgentTask>(&rec_str) else {
                continue;
            };
            let meta = step_meta.get(&task.id);
            for (idx, arch) in task.attempt_history.iter().enumerate() {
                if let Some(row) = build_row_from_parts(
                    &task,
                    idx as u32,
                    arch.execution.as_ref(),
                    arch.verification.as_ref(),
                    arch.review.as_ref(),
                    arch.target.as_deref(),
                    arch.model.as_deref(),
                    true,
                    meta.map(|m| m.workflow_id.as_str()),
                    meta.map(|m| m.profile_name.as_str()),
                    meta.map(|m| m.worker.as_str()),
                    meta.map(|m| m.complexity.as_str()),
                    meta.is_some_and(|m| m.quota_exhausted),
                    false,
                    &self.root,
                ) {
                    self.upsert_row(&row)?;
                }
            }
            if task.execution.is_some() {
                let is_accepted = task.status == crate::agents::AgentStatus::Accepted
                    || meta.is_some_and(|m| m.wf_accepted);
                let is_rejected = task.status == crate::agents::AgentStatus::Rejected
                    || meta.is_some_and(|m| m.wf_rejected);
                let redone = meta.is_some_and(|m| m.redone) || is_rejected;
                let ver_ok = task
                    .verification
                    .as_ref()
                    .is_some_and(|v| v.outcome == "succeeded");
                let clean_first_try = meta.map_or(
                    is_accepted && ver_ok && task.attempt_history.is_empty(),
                    |m| m.clean_first_try,
                );
                let seq = task.attempt_history.len() as u32;
                let override_model = meta
                    .and_then(|m| m.model_override.as_deref())
                    .or(Some(task.assignment.spec.model.as_str()));
                if let Some(mut row) = build_row_from_parts(
                    &task,
                    seq,
                    task.execution.as_ref(),
                    task.verification.as_ref(),
                    task.review.as_ref(),
                    None,
                    override_model,
                    redone,
                    meta.map(|m| m.workflow_id.as_str()),
                    meta.map(|m| m.profile_name.as_str()),
                    meta.map(|m| m.worker.as_str()),
                    meta.map(|m| m.complexity.as_str()),
                    meta.is_some_and(|m| m.quota_exhausted),
                    clean_first_try,
                    &self.root,
                ) {
                    if is_accepted {
                        row.review_outcome = if clean_first_try {
                            "exceptional".into()
                        } else {
                            "accepted".into()
                        };
                        row.exceptional = clean_first_try;
                    } else if is_rejected {
                        row.review_outcome = "rejected".into();
                    }
                    self.upsert_row(&row)?;
                }
            }
        }
        Ok(())
    }

    pub fn scoreboard(&mut self) -> Result<CliModelScoreboard> {
        let _ = self.sync_from_history();
        Ok(CliModelScoreboard::from_rows(self.rows()?))
    }
}

#[allow(clippy::too_many_arguments)]
pub fn build_row_from_parts(
    task: &crate::agents::AgentTask,
    attempt_seq: u32,
    execution: Option<&crate::store::ProcessReceipt>,
    verification: Option<&crate::store::ProcessReceipt>,
    review: Option<&crate::store::Review>,
    target_override: Option<&str>,
    model_override: Option<&str>,
    redone: bool,
    workflow_id: Option<&str>,
    profile_name: Option<&str>,
    worker: Option<&str>,
    complexity: Option<&str>,
    quota_exhausted: bool,
    clean_first_try: bool,
    root: &Path,
) -> Option<CliModelOutcomeRow> {
    let exec = execution?;
    let backend = target_override
        .unwrap_or_else(|| task.assignment.backend.target())
        .to_string();
    let cmd_model = extract_model_from_command(&exec.command);
    let model = cmd_model
        .or_else(|| model_override.map(str::to_string))
        .unwrap_or_else(|| task.assignment.spec.model.clone());
    let effort = extract_effort_from_command(&exec.command)
        .or_else(|| task.assignment.spec.effort.map(|e| e.as_str().to_string()));
    let tier = crate::agents::ModelTier::infer(&backend, &model)
        .as_str()
        .to_string();
    let complexity = complexity.unwrap_or("standard").to_string();

    let duration_ms = exec.ended_at.saturating_sub(exec.started_at);
    let verify_ms = verification
        .map(|v| v.ended_at.saturating_sub(v.started_at))
        .unwrap_or(0);
    let stdout_path = if exec.stdout.artifact.path.exists() {
        exec.stdout.artifact.path.clone()
    } else {
        root.join("agent-artifacts")
            .join(&task.id)
            .join("execution.stdout.log")
    };
    let usage = parse_cli_usage_from_path(&backend, &stdout_path).unwrap_or_default();
    let verify_outcome = verification.map(|v| v.outcome.clone());
    let timed_out = exec.outcome == "timed_out";
    let exec_failed = exec.outcome != "succeeded" && !timed_out && !quota_exhausted;
    let verify_failed = verification.is_some_and(|v| v.outcome != "succeeded");

    let review_outcome = match review {
        Some(r) if r.accepted => {
            if clean_first_try && !redone {
                "exceptional".to_string()
            } else {
                "accepted".to_string()
            }
        }
        Some(_) => "rejected".to_string(),
        None if redone => "redone".to_string(),
        None => "pending".to_string(),
    };
    let exceptional = review_outcome == "exceptional" && !redone && !quota_exhausted && !timed_out;

    Some(CliModelOutcomeRow {
        task_id: task.id.clone(),
        attempt_seq,
        workflow_id: workflow_id.map(str::to_string),
        profile_name: profile_name.map(str::to_string),
        worker: worker.map(str::to_string),
        backend,
        model,
        tier,
        complexity,
        effort,
        exec_outcome: exec.outcome.clone(),
        verify_outcome,
        review_outcome,
        quota_exhausted,
        timed_out,
        exec_failed,
        verify_failed,
        redone,
        exceptional,
        duration_ms,
        verify_ms,
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cached_tokens: usage.cached_tokens,
        thinking_tokens: usage.thinking_tokens,
        num_turns: usage.num_turns.unwrap_or(0),
        recorded_at: exec.ended_at.max(now()),
    })
}

/// Loads the live outcome scoreboard using a lightweight read-only query if `state.sqlite`
/// and `cli_model_outcomes` already exist (avoiding any write-lock or full-history scan on the
/// hot planner dispatch path). Falls back to an empty neutral scoreboard when absent.
pub fn load_scoreboard(root: &Path) -> CliModelScoreboard {
    let db_path = root.join("state.sqlite");
    if !db_path.is_file() {
        return CliModelScoreboard::default();
    }
    let flags =
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let Ok(connection) = Connection::open_with_flags(&db_path, flags) else {
        return CliModelScoreboard::default();
    };
    let _ = connection.busy_timeout(std::time::Duration::from_millis(250));
    let store = RoutingMetricsStore {
        connection,
        root: root.to_path_buf(),
    };
    store
        .rows()
        .map(CliModelScoreboard::from_rows)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_agy_codex_and_cursor_usage_receipts() {
        let agy = r#"{"type":"result","response":"ok","duration_seconds":190.44,"num_turns":2,"usage":{"input_tokens":55581,"output_tokens":18200,"thinking_tokens":9577,"cache_read_tokens":254683,"total_tokens":73781}}"#;
        let parsed_agy = parse_cli_usage_from_str(agy).expect("agy metrics");
        assert_eq!(parsed_agy.input_tokens, 55581);
        assert_eq!(parsed_agy.output_tokens, 18200);
        assert_eq!(parsed_agy.thinking_tokens, 9577);
        assert_eq!(parsed_agy.cached_tokens, 254683);
        assert_eq!(parsed_agy.num_turns, Some(2));
        assert_eq!(parsed_agy.cli_duration_ms, Some(190_440));

        let codex = "{\"type\":\"thread.started\",\"thread_id\":\"t1\"}\n\
                     {\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"done\"}}\n\
                     {\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":108211,\"cached_input_tokens\":85888,\"output_tokens\":471,\"reasoning_output_tokens\":22}}";
        let parsed_codex = parse_cli_usage_from_str(codex).expect("codex metrics");
        assert_eq!(parsed_codex.input_tokens, 108211);
        assert_eq!(parsed_codex.cached_tokens, 85888);
        assert_eq!(parsed_codex.output_tokens, 471);
        assert_eq!(parsed_codex.thinking_tokens, 22);
        assert_eq!(parsed_codex.num_turns, Some(1));

        let cursor = r#"{"type":"result","session_id":"s1","request_id":"r1","usage":{"inputTokens":39183,"outputTokens":2497,"cacheReadTokens":348672,"cacheWriteTokens":0}}"#;
        let parsed_cursor = parse_cli_usage_from_str(cursor).expect("cursor metrics");
        assert_eq!(parsed_cursor.input_tokens, 39183);
        assert_eq!(parsed_cursor.output_tokens, 2497);
        assert_eq!(parsed_cursor.cached_tokens, 348672);
        assert_eq!(parsed_cursor.session_id.as_deref(), Some("s1"));
    }

    #[test]
    fn scoreboard_boosts_exceptional_winners_and_penalizes_failures_and_redos() {
        let mut rows = Vec::new();
        for i in 0..3 {
            rows.push(CliModelOutcomeRow {
                task_id: format!("win-{i}"),
                attempt_seq: 0,
                workflow_id: None,
                profile_name: None,
                worker: None,
                backend: "codex".into(),
                model: "gpt-6.2-astra".into(),
                tier: "frontier".into(),
                complexity: "complex".into(),
                effort: Some("high".into()),
                exec_outcome: "succeeded".into(),
                verify_outcome: Some("succeeded".into()),
                review_outcome: "exceptional".into(),
                quota_exhausted: false,
                timed_out: false,
                exec_failed: false,
                verify_failed: false,
                redone: false,
                exceptional: true,
                duration_ms: 28_000,
                verify_ms: 4_000,
                input_tokens: 100_000,
                output_tokens: 600,
                cached_tokens: 80_000,
                thinking_tokens: 20,
                num_turns: 1,
                recorded_at: 1000 + i,
            });
            rows.push(CliModelOutcomeRow {
                task_id: format!("lose-{i}"),
                attempt_seq: 0,
                workflow_id: None,
                profile_name: None,
                worker: None,
                backend: "agy".into(),
                model: "gemini-4-neon-xhigh".into(),
                tier: "frontier".into(),
                complexity: "complex".into(),
                effort: Some("xhigh".into()),
                exec_outcome: "timed_out".into(),
                verify_outcome: None,
                review_outcome: "redone".into(),
                quota_exhausted: false,
                timed_out: true,
                exec_failed: false,
                verify_failed: false,
                redone: true,
                exceptional: false,
                duration_ms: 360_000,
                verify_ms: 0,
                input_tokens: 0,
                output_tokens: 0,
                cached_tokens: 0,
                thinking_tokens: 0,
                num_turns: 0,
                recorded_at: 2000 + i,
            });
        }
        let board = CliModelScoreboard::from_rows(rows);
        let winner = board.score_candidate("codex", "gpt-6.2-astra", "complex", 1.0);
        let untried = board.score_candidate("cursor", "claude-opus-5", "complex", 1.0);
        let loser = board.score_candidate("agy", "gemini-4-neon-xhigh", "complex", 1.0);
        assert!(winner.learned_weight > 0.5);
        assert_eq!(untried.learned_weight, 0.0);
        assert!(loser.learned_weight < -0.5);
        assert!(winner.total_score > untried.total_score);
        assert!(untried.total_score > loser.total_score);
    }
}
