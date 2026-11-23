//! M12.0 Phase 5: named, admin-approved coding-agent dispatch configurations.
//!
//! The brief is explicit: *"Do not allow remote clients to supply arbitrary executable
//! paths or shell commands. The main Conductor must resolve approved backend
//! configurations locally."* `agents::AgentSpec` (the existing M10.1-M10.4 contract) is a
//! human-typed CLI shape that includes exactly those things - `verify_executable`,
//! `verify_args`, the pinned agent binary itself - because a *human operator at the CLI*
//! was always the one supplying them. An `AgentPreset` is the thing a human still creates
//! (CLI-only, mirroring `authorization::Grants`/`enrollment::Enrollments`), and a remote
//! MCP/edge caller references *by name only* - `conductor_submit_task`'s `agent_preset`
//! field - never the raw configuration underneath it. This is the one new piece of
//! infrastructure M12.0 adds to close the Agents-engine MCP gap without creating another
//! execution engine: `AgentPreset::spec` builds the exact same `AgentSpec`
//! `Agents::create` already accepted from a CLI-driven JSON file - every existing bound,
//! path-safety and pinned-executable check in `agents.rs::Agents::create` still runs
//! unchanged on the result.

use crate::{
    agents::{AgentBackend, AgentSpec, ModelTier},
    now, open_sqlite_with_retry,
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentPreset {
    pub name: String,
    pub repository: PathBuf,
    pub backend: AgentBackend,
    pub allowed_files: Vec<String>,
    pub verification_files: Vec<String>,
    pub verify_executable: PathBuf,
    pub verify_args: Vec<String>,
    pub generation_timeout_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation_idle_timeout_ms: Option<u64>,
    pub verification_timeout_ms: u64,
    pub created_by: String,
    pub created_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agentlet: Option<crate::agentlet::Definition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<ModelTier>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_models: Vec<String>,
    #[serde(default, skip_serializing_if = "crate::agent_progress::is_false")]
    pub soft_timeouts: bool,
}

impl AgentPreset {
    /// The only two fields a remote caller ever actually supplies - everything else
    /// (repository, backend/model/executable, file scope, verifier, timeouts) comes from
    /// this preset, resolved locally. `Agents::create` re-validates all of it regardless;
    /// this is a convenience constructor, not a second source of truth for what's allowed.
    pub fn spec(&self, objective: String, acceptance: String) -> AgentSpec {
        self.spec_with_model(objective, acceptance, None)
    }

    pub fn spec_with_model(
        &self,
        objective: String,
        acceptance: String,
        model_override: Option<&str>,
    ) -> AgentSpec {
        self.spec_with_model_and_effort(objective, acceptance, model_override, None)
    }

    pub fn spec_with_model_and_effort(
        &self,
        objective: String,
        acceptance: String,
        model_override: Option<&str>,
        effort: Option<crate::agents::ExecutionEffort>,
    ) -> AgentSpec {
        let model = model_override
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| self.backend.model())
            .to_string();
        AgentSpec {
            repository: self.repository.clone(),
            objective,
            acceptance,
            allowed_files: self.allowed_files.clone(),
            verification_files: self.verification_files.clone(),
            verify_executable: self.verify_executable.clone(),
            verify_args: self.verify_args.clone(),
            target: self.backend.target().to_string(),
            model,
            generation_timeout_ms: self.generation_timeout_ms,
            generation_idle_timeout_ms: self.generation_idle_timeout_ms,
            verification_timeout_ms: self.verification_timeout_ms,
            soft_timeouts: self.soft_timeouts,
            agentlet: self.agentlet.clone(),
            effort,
        }
    }

    pub fn effective_tier(&self) -> ModelTier {
        self.tier
            .unwrap_or_else(|| ModelTier::infer(self.backend.target(), self.backend.model()))
    }

    pub fn effective_allowed_models(&self) -> Vec<String> {
        let default_model = self.backend.model().trim().to_string();
        let mut models = Vec::new();
        if !default_model.is_empty() {
            models.push(default_model);
        }
        for m in &self.allowed_models {
            let trimmed = m.trim();
            if !trimmed.is_empty() && !models.iter().any(|existing| existing == trimmed) {
                models.push(trimmed.to_string());
            }
        }
        models
    }

    /// Returns the effective allowed models for this preset, intersected with the CLI's live
    /// model catalog when `allowed_models` is explicitly configured (`D2`). When `allowed_models`
    /// is empty, defaults to the CLI's live model catalog (or the preset's pinned model when the
    /// backend does not expose a dynamic catalog).
    pub fn live_allowed_models(&self) -> Vec<String> {
        let configured = self.effective_allowed_models();
        let live = self.backend.discover_live_models();
        if self.allowed_models.is_empty() {
            if live.is_empty() {
                return configured;
            }
            let mut catalog = Vec::new();
            for m in live {
                let trimmed = m.trim();
                if !trimmed.is_empty() && !catalog.iter().any(|existing| existing == trimmed) {
                    catalog.push(trimmed.to_string());
                }
            }
            return if catalog.is_empty() {
                configured
            } else {
                catalog
            };
        }
        if live.is_empty() {
            return configured;
        }
        configured
            .into_iter()
            .filter(|m| live.iter().any(|l| l.trim() == m))
            .collect()
    }

    pub fn allows_model(&self, model: &str) -> bool {
        let trimmed = model.trim();
        !trimmed.is_empty() && self.live_allowed_models().iter().any(|m| m == trimmed)
    }

    pub fn select_model_for_tier(&self, target_tier: ModelTier) -> String {
        self.select_model_for_tier_excluding(target_tier, &[])
    }

    pub fn select_model_for_tier_excluding(
        &self,
        target_tier: ModelTier,
        exclude_models: &[String],
    ) -> String {
        let empty = crate::routing_metrics::CliModelScoreboard::default();
        self.select_model_for_tier_weighted(target_tier, exclude_models, &empty, "standard")
    }

    pub fn select_model_for_tier_weighted(
        &self,
        target_tier: ModelTier,
        exclude_models: &[String],
        scoreboard: &crate::routing_metrics::CliModelScoreboard,
        complexity: &str,
    ) -> String {
        let all_candidates = self.live_allowed_models();
        let filtered: Vec<String> = all_candidates
            .iter()
            .filter(|m| !exclude_models.iter().any(|ex| ex.eq_ignore_ascii_case(m)))
            .cloned()
            .collect();
        let candidates = if filtered.is_empty() {
            &all_candidates
        } else {
            &filtered
        };
        let target = self.backend.target();
        let pinned_default = self.backend.model();
        let rank_model = |m: &str| -> f64 {
            let learned = scoreboard
                .score_candidate(target, m, complexity, 0.0)
                .learned_weight;
            // Small effort/capability affinity: avoid -xhigh beating -high/astra/opus on cold start
            // (xhigh tends to stall longer) while keeping -high frontier models ranked above generic.
            let lower = m.to_ascii_lowercase();
            let complexity_affinity = match complexity {
                "complex" => {
                    if lower.contains("xhigh") || lower.contains("extra-high") {
                        0.0
                    } else if lower.ends_with("-high")
                        || lower.contains("astra")
                        || lower.contains("opus")
                        || lower.contains("thinking")
                    {
                        0.06
                    } else {
                        0.02
                    }
                }
                "boilerplate" => {
                    if lower.contains("flash")
                        || lower.contains("mini")
                        || lower.contains("fast")
                        || lower.ends_with("-low")
                    {
                        0.04
                    } else {
                        0.0
                    }
                }
                _ => {
                    if lower.ends_with("-medium")
                        || lower.contains("sol")
                        || lower.contains("sonnet")
                    {
                        0.04
                    } else {
                        0.01
                    }
                }
            };
            let pinned_bonus = if m.eq_ignore_ascii_case(pinned_default) {
                0.08
            } else {
                0.0
            };
            learned + complexity_affinity + pinned_bonus
        };

        let exact_matches: Vec<&String> = candidates
            .iter()
            .filter(|m| ModelTier::infer(target, m) == target_tier)
            .collect();
        if !exact_matches.is_empty() {
            return exact_matches
                .into_iter()
                .enumerate()
                .max_by(|(idx_a, a), (idx_b, b)| {
                    rank_model(a)
                        .total_cmp(&rank_model(b))
                        // Prefer earlier catalog order on exact ties.
                        .then_with(|| idx_b.cmp(idx_a))
                })
                .map(|(_, m)| m.clone())
                .unwrap_or_else(|| pinned_default.to_string());
        }

        match target_tier {
            ModelTier::Local | ModelTier::Fast => candidates
                .iter()
                .enumerate()
                .max_by(|(idx_a, a), (idx_b, b)| {
                    ModelTier::infer(target, b)
                        .cmp(&ModelTier::infer(target, a))
                        .then_with(|| rank_model(a).total_cmp(&rank_model(b)))
                        .then_with(|| idx_b.cmp(idx_a))
                })
                .map(|(_, m)| m.clone())
                .unwrap_or_else(|| pinned_default.to_string()),
            ModelTier::Standard => candidates
                .iter()
                .enumerate()
                .filter(|(_, m)| ModelTier::infer(target, m) <= ModelTier::Standard)
                .max_by(|(idx_a, a), (idx_b, b)| {
                    ModelTier::infer(target, a)
                        .cmp(&ModelTier::infer(target, b))
                        .then_with(|| rank_model(a).total_cmp(&rank_model(b)))
                        .then_with(|| idx_b.cmp(idx_a))
                })
                .map(|(_, m)| m.clone())
                .or_else(|| {
                    candidates
                        .iter()
                        .min_by_key(|m| ModelTier::infer(target, m))
                        .cloned()
                })
                .unwrap_or_else(|| pinned_default.to_string()),
            ModelTier::Frontier => candidates
                .iter()
                .enumerate()
                .max_by(|(idx_a, a), (idx_b, b)| {
                    ModelTier::infer(target, a)
                        .cmp(&ModelTier::infer(target, b))
                        .then_with(|| rank_model(a).total_cmp(&rank_model(b)))
                        .then_with(|| idx_b.cmp(idx_a))
                })
                .map(|(_, m)| m.clone())
                .unwrap_or_else(|| pinned_default.to_string()),
        }
    }
}

pub struct AgentPresets {
    connection: Connection,
}

impl AgentPresets {
    pub fn open(root: &Path) -> Result<Self> {
        let connection = open_sqlite_with_retry(&root.join("state.sqlite"), 1)
            .context("open agent preset state")?;
        crate::ensure_table_schema(
            &connection,
            "agent_presets",
            "CREATE TABLE IF NOT EXISTS agent_presets(
                name TEXT PRIMARY KEY,
                record TEXT NOT NULL
            );",
        )?;
        Ok(Self { connection })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create(
        &mut self,
        name: &str,
        repository: PathBuf,
        backend: AgentBackend,
        allowed_files: Vec<String>,
        verification_files: Vec<String>,
        verify_executable: PathBuf,
        verify_args: Vec<String>,
        generation_timeout_ms: u64,
        verification_timeout_ms: u64,
        generation_idle_timeout_ms: Option<u64>,
        created_by: &str,
    ) -> Result<AgentPreset> {
        ensure!(!name.trim().is_empty(), "a preset needs a name");
        ensure!(
            self.get(name).is_err(),
            "a preset named '{name}' already exists - delete it first or pick a different name"
        );
        ensure!(
            !created_by.trim().is_empty(),
            "created_by is required - a preset needs a human accountable for approving its configuration"
        );
        backend
            .validate()
            .context("preset backend configuration failed validation")?;
        crate::agent_progress::validate_limits(generation_timeout_ms, generation_idle_timeout_ms)?;
        crate::repository::Repository::inspect(&repository)
            .context("preset repository does not resolve")?;
        let preset = AgentPreset {
            name: name.to_string(),
            repository,
            backend,
            allowed_files,
            verification_files,
            verify_executable,
            verify_args,
            generation_timeout_ms,
            generation_idle_timeout_ms,
            verification_timeout_ms,
            created_by: created_by.to_string(),
            created_at: now(),
            agentlet: None,
            tier: None,
            allowed_models: Vec::new(),
            soft_timeouts: false,
        };
        self.connection.execute(
            "INSERT INTO agent_presets VALUES (?1, ?2)",
            params![preset.name, serde_json::to_string(&preset)?],
        )?;
        Ok(preset)
    }

    pub fn set_routing_policy(
        &self,
        name: &str,
        tier: Option<ModelTier>,
        allowed_models: Vec<String>,
    ) -> Result<AgentPreset> {
        let mut preset = self.get(name)?;
        let mut cleaned = Vec::new();
        for model in allowed_models {
            let trimmed = model.trim();
            ensure!(
                !trimmed.is_empty()
                    && trimmed.len() <= 128
                    && !trimmed.contains(['\0', '\n', '\r']),
                "allowed model must be a non-empty bounded identifier"
            );
            if !cleaned.iter().any(|existing: &String| existing == trimmed) {
                cleaned.push(trimmed.to_string());
            }
        }
        preset.tier = tier;
        preset.allowed_models = cleaned;
        self.connection.execute(
            "UPDATE agent_presets SET record=?2 WHERE name=?1",
            params![preset.name, serde_json::to_string(&preset)?],
        )?;
        Ok(preset)
    }

    pub fn set_timeouts(
        &self,
        name: &str,
        generation_timeout_ms: u64,
        generation_idle_timeout_ms: Option<u64>,
        verification_timeout_ms: u64,
    ) -> Result<AgentPreset> {
        let current = self.get(name)?;
        self.set_timeouts_with_mode(
            name,
            generation_timeout_ms,
            generation_idle_timeout_ms,
            verification_timeout_ms,
            current.soft_timeouts,
        )
    }

    pub fn set_timeouts_with_mode(
        &self,
        name: &str,
        generation_timeout_ms: u64,
        generation_idle_timeout_ms: Option<u64>,
        verification_timeout_ms: u64,
        soft_timeouts: bool,
    ) -> Result<AgentPreset> {
        let mut preset = self.get(name)?;
        crate::agent_progress::validate_limits_with_mode(
            generation_timeout_ms,
            generation_idle_timeout_ms,
            soft_timeouts,
        )?;
        let verif_max = if soft_timeouts {
            crate::agent_progress::SOFT_TIMEOUT_MAX_MS
        } else {
            600_000
        };
        ensure!(
            (100..=verif_max).contains(&verification_timeout_ms),
            "invalid worker verification budget"
        );
        preset.generation_timeout_ms = generation_timeout_ms;
        preset.generation_idle_timeout_ms = generation_idle_timeout_ms;
        preset.verification_timeout_ms = verification_timeout_ms;
        preset.soft_timeouts = soft_timeouts;
        self.connection.execute(
            "UPDATE agent_presets SET record=?2 WHERE name=?1",
            params![preset.name, serde_json::to_string(&preset)?],
        )?;
        Ok(preset)
    }

    pub fn set_soft_timeouts(&self, name: &str, soft_timeouts: bool) -> Result<AgentPreset> {
        let current = self.get(name)?;
        self.set_timeouts_with_mode(
            name,
            current.generation_timeout_ms,
            current.generation_idle_timeout_ms,
            current.verification_timeout_ms,
            soft_timeouts,
        )
    }

    pub fn bind_agentlet(
        &self,
        source: &str,
        name: &str,
        definition: crate::agentlet::Definition,
        by: &str,
    ) -> Result<AgentPreset> {
        definition.validate()?;
        ensure!(
            !name.trim().is_empty()
                && name.len() <= 128
                && !by.trim().is_empty()
                && by.len() <= 256,
            "bounded binding identity required"
        );
        let mut preset = self.get(source)?;
        preset.backend.validate_agentlet()?;
        ensure!(
            preset.agentlet.is_none(),
            "bind agentlets from an unbound execution preset"
        );
        preset.name = name.into();
        if definition.capability == crate::agentlet::Capability::Review {
            preset.allowed_files.clear();
        }
        definition.validate_scope(&preset.allowed_files)?;
        preset.agentlet = Some(definition);
        preset.created_by = by.into();
        preset.created_at = now();
        self.connection.execute(
            "INSERT INTO agent_presets VALUES(?1,?2)",
            params![preset.name, serde_json::to_string(&preset)?],
        )?;
        Ok(preset)
    }

    pub fn get(&self, name: &str) -> Result<AgentPreset> {
        let record: String = self
            .connection
            .query_row(
                "SELECT record FROM agent_presets WHERE name=?1",
                [name],
                |r| r.get(0),
            )
            .context("unknown agent preset name")?;
        serde_json::from_str(&record).context("corrupt agent preset record")
    }

    pub fn list(&self) -> Result<Vec<AgentPreset>> {
        let mut statement = self
            .connection
            .prepare("SELECT record FROM agent_presets ORDER BY name")?;
        let rows = statement.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(serde_json::from_str(&row?)?);
        }
        Ok(out)
    }

    pub fn delete(&mut self, name: &str) -> Result<()> {
        let changed = self
            .connection
            .execute("DELETE FROM agent_presets WHERE name=?1", [name])?;
        ensure!(changed == 1, "unknown agent preset name");
        Ok(())
    }

    pub fn delete_if_exists(&mut self, name: &str) -> Result<Option<AgentPreset>> {
        let existing = self.get(name).ok();
        if existing.is_some() {
            self.connection
                .execute("DELETE FROM agent_presets WHERE name=?1", [name])?;
        }
        Ok(existing)
    }

    pub fn put(&mut self, preset: &AgentPreset) -> Result<()> {
        self.connection.execute(
            "INSERT OR REPLACE INTO agent_presets(name, record) VALUES (?1, ?2)",
            params![preset.name, serde_json::to_string(preset)?],
        )?;
        Ok(())
    }
}
