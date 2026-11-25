//! Parallel best-of-N attempts on one scoped stage (#22).
//!
//! A race creates one ordinary agent task per admin-approved preset for the same objective
//! and acceptance. Every preset must share the repository, editable files, protected
//! verification files and verifier, so the candidates are comparable. The operator approves
//! the race as one unit, bound to a digest of every attempt's assignment. Running it holds
//! the repository lease once and runs the attempts at the same time, each in its own
//! worktree, each verified independently by the unchanged Agents engine (which also records
//! every attempt and review in the routing scoreboard). `view` lays the candidates side by
//! side; `pick` accepts one through ordinary review and rejects the other reviewable ones.
//! The accepted winner then integrates or delivers like any accepted agent task.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    agent_preset::AgentPresets,
    agents::{AgentStatus, Agents},
    digest, now, open_sqlite_with_retry,
    service::{TaskService, with_repository_lease},
    stop::GlobalStop,
};

pub const MIN_ATTEMPTS: usize = 2;
pub const MAX_ATTEMPTS: usize = 4;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RaceEvent {
    pub at: u64,
    pub stage: String,
    pub by: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Race {
    pub id: String,
    /// `awaiting_approval`, `approved`, `running`, `awaiting_pick`, `picked`, `failed`.
    pub status: String,
    pub repository_identity: String,
    pub objective: String,
    pub acceptance: String,
    pub presets: Vec<String>,
    /// One agent task per preset, in preset order.
    pub tasks: Vec<String>,
    pub winner: Option<String>,
    pub events: Vec<RaceEvent>,
}

pub struct Races {
    connection: Connection,
    root: PathBuf,
}

fn status_name(status: &AgentStatus) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("{status:?}"))
}

/// Changed files and added/removed line counts of a unified diff.
fn diffstat(patch: &[u8]) -> Value {
    let text = String::from_utf8_lossy(patch);
    let (mut files, mut added, mut removed) = (Vec::new(), 0usize, 0usize);
    for line in text.lines() {
        if let Some(path) = line.strip_prefix("+++ b/") {
            files.push(path.to_string());
        } else if line.starts_with('+') && !line.starts_with("+++") {
            added += 1;
        } else if line.starts_with('-') && !line.starts_with("---") {
            removed += 1;
        }
    }
    json!({"files": files, "added": added, "removed": removed})
}

impl Races {
    pub fn open(root: &Path) -> Result<Self> {
        let root = crate::repository::canonical(root)?;
        let connection = open_sqlite_with_retry(&root.join("state.sqlite"), 1)?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS agent_races(id TEXT PRIMARY KEY, record TEXT NOT NULL)",
        )?;
        Ok(Self { connection, root })
    }

    pub fn get(&self, id: &str) -> Result<Race> {
        let record: Option<String> = self
            .connection
            .query_row("SELECT record FROM agent_races WHERE id=?1", [id], |r| {
                r.get(0)
            })
            .optional()?;
        Ok(serde_json::from_str(&record.context("unknown race")?)?)
    }

    fn save(&self, race: &Race) -> Result<()> {
        self.connection.execute(
            "INSERT OR REPLACE INTO agent_races(id, record) VALUES (?1, ?2)",
            params![race.id, serde_json::to_string(race)?],
        )?;
        Ok(())
    }

    fn event(race: &mut Race, stage: &str, by: &str) {
        race.events.push(RaceEvent {
            at: now(),
            stage: stage.into(),
            by: by.into(),
        });
    }

    /// Digest an approval of the whole race must name: every attempt's id and current
    /// assignment digest.
    pub fn digest(&self, race: &Race) -> Result<String> {
        let agents = Agents::open(&self.root)?;
        let parts = race
            .tasks
            .iter()
            .map(|id| Ok((id.clone(), agents.task(id)?.assignment.digest()?)))
            .collect::<Result<Vec<_>>>()?;
        Ok(digest(&serde_json::to_vec(&parts)?))
    }

    pub fn create(
        &mut self,
        presets: &[String],
        objective: &str,
        acceptance: &str,
        by: &str,
    ) -> Result<Race> {
        ensure!(!by.trim().is_empty(), "operator label required");
        ensure!(
            (MIN_ATTEMPTS..=MAX_ATTEMPTS).contains(&presets.len()),
            "a race needs {MIN_ATTEMPTS} to {MAX_ATTEMPTS} presets"
        );
        let mut unique = presets.to_vec();
        unique.sort();
        unique.dedup();
        ensure!(
            unique.len() == presets.len(),
            "race presets must be distinct"
        );
        let catalog = AgentPresets::open(&self.root)?;
        let loaded = presets
            .iter()
            .map(|name| catalog.get(name))
            .collect::<Result<Vec<_>>>()?;
        let first = &loaded[0];
        let sorted = |v: &[String]| {
            let mut v = v.to_vec();
            v.sort();
            v
        };
        for preset in &loaded[1..] {
            ensure!(
                crate::repository::canonical(&preset.repository)?
                    == crate::repository::canonical(&first.repository)?,
                "race presets must share one repository ({} differs)",
                preset.name
            );
            ensure!(
                sorted(&preset.allowed_files) == sorted(&first.allowed_files)
                    && sorted(&preset.verification_files) == sorted(&first.verification_files),
                "race presets must share editable and protected files ({} differs)",
                preset.name
            );
            ensure!(
                preset.verify_executable == first.verify_executable
                    && preset.verify_args == first.verify_args
                    && preset.sandbox == first.sandbox,
                "race presets must share one verifier and verifier sandbox ({} differs)",
                preset.name
            );
        }

        let mut agents = Agents::open(&self.root)?;
        let mut tasks = Vec::new();
        for preset in &loaded {
            let spec = preset.spec(objective.to_string(), acceptance.to_string());
            tasks.push(agents.create(spec, preset.backend.clone())?);
        }
        let base = &tasks[0].assignment.repository.base;
        if tasks.iter().any(|t| &t.assignment.repository.base != base) {
            for task in &tasks {
                let _ = agents.cancel(&task.id, task.revision);
            }
            anyhow::bail!("the repository moved while the race was created; try again");
        }
        let mut race = Race {
            id: uuid::Uuid::new_v4().to_string(),
            status: "awaiting_approval".into(),
            repository_identity: tasks[0].assignment.repository.identity.clone(),
            objective: objective.to_string(),
            acceptance: acceptance.to_string(),
            presets: presets.to_vec(),
            tasks: tasks.iter().map(|t| t.id.clone()).collect(),
            winner: None,
            events: Vec::new(),
        };
        Self::event(&mut race, "race_created", by);
        self.save(&race)?;
        Ok(race)
    }

    /// Approves every attempt at once. `expected` must equal [`Races::digest`].
    pub fn approve(&mut self, id: &str, expected: &str, by: &str) -> Result<Race> {
        ensure!(!by.trim().is_empty(), "approver label required");
        let mut race = self.get(id)?;
        ensure!(
            race.status == "awaiting_approval",
            "race is {}, not awaiting approval",
            race.status
        );
        ensure!(
            self.digest(&race)? == expected,
            "current race digest required"
        );
        let mut agents = Agents::open(&self.root)?;
        for task_id in &race.tasks {
            let task = agents.task(task_id)?;
            agents.approve(task_id, task.revision, &task.assignment.digest()?, by)?;
        }
        race.status = "approved".into();
        Self::event(&mut race, "race_approved", by);
        self.save(&race)?;
        Ok(race)
    }

    /// Runs every attempt at once under one repository lease and waits for all of them.
    pub fn run(&mut self, id: &str, by: &str) -> Result<Race> {
        ensure!(!by.trim().is_empty(), "operator label required");
        GlobalStop::open(&self.root)?
            .state()?
            .ensure_admission_allowed()?;
        let mut race = self.get(id)?;
        ensure!(
            race.status == "approved",
            "race is {}, not approved",
            race.status
        );
        race.status = "running".into();
        Self::event(&mut race, "race_started", by);
        self.save(&race)?;

        let root = self.root.clone();
        let attempts = race.tasks.clone();
        let race_id = race.id.clone();
        let ran = with_repository_lease(
            &root,
            race.repository_identity.clone(),
            race_id.clone(),
            || {
                let handles: Vec<_> = attempts
                    .iter()
                    .cloned()
                    .map(|task_id| {
                        let root = root.clone();
                        let race_id = race_id.clone();
                        std::thread::spawn(move || -> Result<()> {
                            let mut agents = Agents::open(&root)?;
                            let revision = agents.task(&task_id)?.revision;
                            agents.run_within_race(&task_id, revision, &race_id)?;
                            Ok(())
                        })
                    })
                    .collect();
                let mut errors = Vec::new();
                for handle in handles {
                    match handle.join() {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => errors.push(format!("{error:#}")),
                        Err(_) => errors.push("a race attempt panicked".into()),
                    }
                }
                Ok(errors)
            },
        );

        let agents = Agents::open(&self.root)?;
        let reviewable = race
            .tasks
            .iter()
            .filter(|t| {
                agents
                    .task(t)
                    .is_ok_and(|task| task.status == AgentStatus::AwaitingReview)
            })
            .count();
        race.status = if reviewable > 0 {
            "awaiting_pick"
        } else {
            "failed"
        }
        .into();
        Self::event(&mut race, "race_finished", by);
        self.save(&race)?;
        let errors = ran?;
        ensure!(
            errors.is_empty() || reviewable > 0,
            "no race attempt reached review: {}",
            errors.join("; ")
        );
        Ok(race)
    }

    /// The candidates side by side: worker, model, status, verifier outcome, diffstat and
    /// the evidence digest a pick must name.
    pub fn view(&self, id: &str) -> Result<Value> {
        let race = self.get(id)?;
        let agents = Agents::open(&self.root)?;
        let candidates = race
            .tasks
            .iter()
            .zip(&race.presets)
            .map(|(task_id, preset)| {
                let task = agents.task(task_id)?;
                let reviewable = task.status == AgentStatus::AwaitingReview;
                Ok(json!({
                    "task_id": task.id,
                    "preset": preset,
                    "backend": task.assignment.backend.target(),
                    "model": task.assignment.backend.model(),
                    "status": status_name(&task.status),
                    "verification": task.verification.as_ref().map(|v| v.outcome.clone()),
                    "diffstat": task
                        .patch
                        .as_ref()
                        .and_then(|_| agents.patch(&task).ok())
                        .map(|p| diffstat(&p)),
                    "evidence_digest": if reviewable {
                        Some(agents.evidence_digest(&task)?)
                    } else {
                        None
                    },
                    "error": task.error,
                }))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(json!({
            "race": race,
            "approval_digest": if race.status == "awaiting_approval" {
                Some(self.digest(&race)?)
            } else {
                None
            },
            "candidates": candidates,
        }))
    }

    /// Accepts `winner` through ordinary review, bound to its evidence digest, and rejects
    /// every other attempt that reached review.
    pub fn pick(&mut self, id: &str, winner: &str, evidence: &str, by: &str) -> Result<Race> {
        ensure!(!by.trim().is_empty(), "reviewer label required");
        let mut race = self.get(id)?;
        ensure!(
            race.status == "awaiting_pick",
            "race is {}, not awaiting a pick",
            race.status
        );
        ensure!(
            race.tasks.iter().any(|t| t == winner),
            "{winner} is not an attempt in this race"
        );
        let mut service = TaskService::open(&self.root)?;
        let agents = Agents::open(&self.root)?;
        let chosen = agents.task(winner)?;
        service.review(winner, chosen.revision, evidence, by, true)?;
        for other in race.tasks.iter().filter(|t| *t != winner) {
            let task = agents.task(other)?;
            if task.status == AgentStatus::AwaitingReview {
                let digest = agents.evidence_digest(&task)?;
                service.review(other, task.revision, &digest, by, false)?;
            }
        }
        race.status = "picked".into();
        race.winner = Some(winner.to_string());
        Self::event(&mut race, "race_picked", by);
        self.save(&race)?;
        Ok(race)
    }
}
