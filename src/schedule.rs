//! Durable scheduled submissions (`conductor_schedule_task`).
//!
//! A schedule stores one `conductor_submit_task` request and when to make it. The running
//! `serve` process checks for due schedules on a fixed tick and submits each one through
//! the ordinary `LocalBackend::conductor_submit_task`, with the schedule's own principal and
//! (for an edge client) that enrollment's current repository scope. Scheduling therefore
//! adds no authority of its own: approval and admission are decided at fire time, a
//! standing grant that covers the submission then may auto-authorize it, and
//! `requested_execution_mode: "interactive"` still stops it at approval.
//!
//! Firing is at-least-once with exactly-once task creation: every occurrence carries a
//! deterministic `(workflow_id, idempotency_key)` pair, so a firing retried after a crash
//! or raced by a second `serve` replays the existing task instead of creating another.
//! The schedule only advances with a compare-and-swap on its occurrence number. Nothing
//! fires while the global stop is engaged, and an occurrence missed during downtime or a
//! stop fires once on the next tick, never as a burst.

use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use clatterforge_protocol::{ConductorBackend, ScheduleTaskRequest, SubmitTaskRequest};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    enrollment::{EnrollmentOperation, Enrollments, PRINCIPAL_NAMESPACE_PREFIX},
    mcp::LocalBackend,
    now, open_sqlite_with_retry,
    stop::GlobalStop,
};

/// How often `serve` looks for due schedules.
pub const TICK_MS: u64 = 15_000;
pub const MIN_REPEAT_MS: u64 = 60_000;
pub const MAX_HORIZON_MS: u64 = 366 * 24 * 60 * 60 * 1000;
pub const MAX_OCCURRENCES: u32 = 10_000;
pub const MAX_ACTIVE_PER_PRINCIPAL: usize = 100;
/// A repeating schedule stops after this many failed occurrences in a row.
pub const MAX_CONSECUTIVE_FAILURES: u32 = 3;
const FIRE_BATCH: usize = 20;
const ENGINES: &[&str] = &[
    "local-changes-v1",
    "fake-process-v1",
    "cursor-agent",
    "codex",
    "workflow-v1",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Schedule {
    pub id: String,
    pub principal: String,
    /// Set when an enrolled edge client created it; its enrollment is rechecked at every firing.
    pub client_id: Option<String>,
    pub submission: Value,
    /// `active`, `completed`, `cancelled` or `failed`.
    pub state: String,
    pub next_due_ms: u64,
    pub repeat_every_ms: Option<u64>,
    pub max_occurrences: u32,
    /// Occurrence number the next firing uses, starting at 1.
    pub next_occurrence: u32,
    pub consecutive_failures: u32,
    pub last_error: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
    pub cancelled_by: Option<String>,
    pub fires: Vec<Fire>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Fire {
    pub occurrence: u32,
    pub fired_at: u64,
    pub task_id: Option<String>,
    pub replayed: bool,
    /// Steps a standing grant authorized at fire time, exactly as submission reported them.
    pub authorization: Vec<Value>,
    pub error: Option<String>,
}

pub struct Schedules {
    connection: Connection,
}

/// Deduplication pair for one occurrence; also how a fired task names its schedule.
pub fn occurrence_key(schedule_id: &str, occurrence: u32) -> (String, String) {
    (format!("schedule:{schedule_id}"), occurrence.to_string())
}

/// First slot on the cadence strictly after `now`, so missed slots collapse into one firing.
fn next_slot(due: u64, every: u64, now: u64) -> u64 {
    if now < due {
        return due + every;
    }
    due + every * ((now - due) / every + 1)
}

impl Schedules {
    pub fn open(root: &Path) -> Result<Self> {
        let connection =
            open_sqlite_with_retry(&root.join("state.sqlite"), 1).context("open schedule store")?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS scheduled_submissions(
                id TEXT PRIMARY KEY,
                principal TEXT NOT NULL,
                client_id TEXT,
                submission TEXT NOT NULL,
                state TEXT NOT NULL,
                next_due_ms INTEGER NOT NULL,
                repeat_every_ms INTEGER,
                max_occurrences INTEGER NOT NULL,
                next_occurrence INTEGER NOT NULL,
                consecutive_failures INTEGER NOT NULL DEFAULT 0,
                last_error TEXT,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                cancelled_by TEXT
             );
             CREATE INDEX IF NOT EXISTS scheduled_submissions_due
                ON scheduled_submissions(state, next_due_ms);
             CREATE TABLE IF NOT EXISTS schedule_fires(
                schedule_id TEXT NOT NULL,
                occurrence INTEGER NOT NULL,
                fired_at INTEGER NOT NULL,
                task_id TEXT,
                replayed INTEGER NOT NULL,
                authorization TEXT NOT NULL,
                error TEXT,
                PRIMARY KEY(schedule_id, occurrence)
             );",
        )?;
        Ok(Self { connection })
    }

    /// Validates and stores a schedule. `client_id` is the verified enrollment for an edge
    /// caller, never anything the request itself names.
    pub fn create(
        &mut self,
        request: &ScheduleTaskRequest,
        client_id: Option<&str>,
        at: u64,
    ) -> Result<Schedule> {
        let submission = &request.submission;
        ensure!(
            ENGINES.contains(&submission.backend.as_str()),
            "unknown backend {:?}",
            submission.backend
        );
        ensure!(
            !submission.principal.trim().is_empty(),
            "submission.principal is required"
        );
        ensure!(
            request.not_before_ms <= at.saturating_add(MAX_HORIZON_MS),
            "not_before_ms is more than 366 days away"
        );
        let max_occurrences = request.max_occurrences.unwrap_or(1);
        ensure!(
            (1..=MAX_OCCURRENCES).contains(&max_occurrences),
            "max_occurrences must be between 1 and {MAX_OCCURRENCES}"
        );
        match request.repeat_every_ms {
            Some(every) => {
                ensure!(
                    (MIN_REPEAT_MS..=MAX_HORIZON_MS).contains(&every),
                    "repeat_every_ms must be between {MIN_REPEAT_MS} and {MAX_HORIZON_MS}"
                );
                ensure!(
                    request.max_occurrences.is_some(),
                    "a repeating schedule needs an explicit max_occurrences"
                );
            }
            None => ensure!(
                max_occurrences == 1,
                "max_occurrences above 1 needs repeat_every_ms"
            ),
        }
        let active: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM scheduled_submissions WHERE principal=?1 AND state='active'",
            [&submission.principal],
            |r| r.get(0),
        )?;
        ensure!(
            (active as usize) < MAX_ACTIVE_PER_PRINCIPAL,
            "this principal already has {MAX_ACTIVE_PER_PRINCIPAL} active schedules"
        );

        // Per-occurrence keys replace any caller-supplied pair at fire time.
        let mut stored = submission.clone();
        stored.workflow_id = None;
        stored.idempotency_key = None;
        stored.request_id = None;
        let id = uuid::Uuid::new_v4().to_string();
        self.connection.execute(
            "INSERT INTO scheduled_submissions(id, principal, client_id, submission, state,
                next_due_ms, repeat_every_ms, max_occurrences, next_occurrence, created_at,
                updated_at)
             VALUES (?1, ?2, ?3, ?4, 'active', ?5, ?6, ?7, 1, ?8, ?8)",
            params![
                id,
                submission.principal,
                client_id,
                serde_json::to_string(&stored)?,
                request.not_before_ms as i64,
                request.repeat_every_ms.map(|v| v as i64),
                max_occurrences as i64,
                at as i64,
            ],
        )?;
        self.get(&id)
    }

    pub fn get(&self, id: &str) -> Result<Schedule> {
        let mut schedule = self
            .connection
            .query_row(
                "SELECT id, principal, client_id, submission, state, next_due_ms,
                    repeat_every_ms, max_occurrences, next_occurrence, consecutive_failures,
                    last_error, created_at, updated_at, cancelled_by
                 FROM scheduled_submissions WHERE id=?1",
                [id],
                |r| {
                    Ok(Schedule {
                        id: r.get(0)?,
                        principal: r.get(1)?,
                        client_id: r.get(2)?,
                        submission: serde_json::from_str(&r.get::<_, String>(3)?)
                            .unwrap_or(Value::Null),
                        state: r.get(4)?,
                        next_due_ms: r.get::<_, i64>(5)? as u64,
                        repeat_every_ms: r.get::<_, Option<i64>>(6)?.map(|v| v as u64),
                        max_occurrences: r.get::<_, i64>(7)? as u32,
                        next_occurrence: r.get::<_, i64>(8)? as u32,
                        consecutive_failures: r.get::<_, i64>(9)? as u32,
                        last_error: r.get(10)?,
                        created_at: r.get::<_, i64>(11)? as u64,
                        updated_at: r.get::<_, i64>(12)? as u64,
                        cancelled_by: r.get(13)?,
                        fires: Vec::new(),
                    })
                },
            )
            .optional()?
            .context("unknown schedule")?;
        let mut stmt = self.connection.prepare(
            "SELECT occurrence, fired_at, task_id, replayed, authorization, error
             FROM schedule_fires WHERE schedule_id=?1 ORDER BY occurrence",
        )?;
        schedule.fires = stmt
            .query_map([id], |r| {
                Ok(Fire {
                    occurrence: r.get::<_, i64>(0)? as u32,
                    fired_at: r.get::<_, i64>(1)? as u64,
                    task_id: r.get(2)?,
                    replayed: r.get::<_, i64>(3)? != 0,
                    authorization: serde_json::from_str(&r.get::<_, String>(4)?)
                        .unwrap_or_default(),
                    error: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(schedule)
    }

    /// Every schedule, or only `principal`'s, newest first.
    pub fn list(&self, principal: Option<&str>) -> Result<Vec<Schedule>> {
        let mut stmt = self.connection.prepare(
            "SELECT id FROM scheduled_submissions
             WHERE ?1 IS NULL OR principal=?1 ORDER BY created_at DESC, id",
        )?;
        let ids: Vec<String> = stmt
            .query_map([principal], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        ids.iter().map(|id| self.get(id)).collect()
    }

    /// Cancels an active schedule. `owner` restricts it to that principal's schedules.
    pub fn cancel(&mut self, id: &str, by: &str, owner: Option<&str>) -> Result<Schedule> {
        let schedule = self.get(id)?;
        if let Some(owner) = owner {
            // Same answer as a missing id, so another client's schedules stay invisible.
            ensure!(schedule.principal == owner, "unknown schedule");
        }
        ensure!(
            schedule.state == "active",
            "schedule is already {}",
            schedule.state
        );
        self.connection.execute(
            "UPDATE scheduled_submissions SET state='cancelled', cancelled_by=?2, updated_at=?3
             WHERE id=?1 AND state='active'",
            params![id, by, now() as i64],
        )?;
        self.get(id)
    }

    fn due(&self, at: u64) -> Result<Vec<Schedule>> {
        let mut stmt = self.connection.prepare(
            "SELECT id FROM scheduled_submissions WHERE state='active' AND next_due_ms<=?1
             ORDER BY next_due_ms, id LIMIT ?2",
        )?;
        let ids: Vec<String> = stmt
            .query_map(params![at as i64, FIRE_BATCH as i64], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        ids.iter().map(|id| self.get(id)).collect()
    }

    fn fail(&mut self, schedule: &Schedule, reason: &str, at: u64) -> Result<()> {
        self.connection.execute(
            "UPDATE scheduled_submissions SET state='failed', last_error=?2, updated_at=?3
             WHERE id=?1 AND state='active' AND next_occurrence=?4",
            params![
                schedule.id,
                reason,
                at as i64,
                schedule.next_occurrence as i64
            ],
        )?;
        Ok(())
    }

    /// Records one occurrence and advances the schedule, unless another firing already did.
    fn advance(&mut self, schedule: &Schedule, fire: &Fire, at: u64) -> Result<bool> {
        let tx = self.connection.transaction()?;
        let failures = if fire.error.is_some() {
            schedule.consecutive_failures + 1
        } else {
            0
        };
        let fired = schedule.next_occurrence;
        let (state, next_due) = match schedule.repeat_every_ms {
            _ if fired >= schedule.max_occurrences => (
                if fire.error.is_some() {
                    "failed"
                } else {
                    "completed"
                },
                schedule.next_due_ms,
            ),
            _ if failures >= MAX_CONSECUTIVE_FAILURES => ("failed", schedule.next_due_ms),
            Some(every) => ("active", next_slot(schedule.next_due_ms, every, at)),
            None => ("completed", schedule.next_due_ms),
        };
        let changed = tx.execute(
            "UPDATE scheduled_submissions SET next_occurrence=?2, next_due_ms=?3, state=?4,
                consecutive_failures=?5, last_error=?6, updated_at=?7
             WHERE id=?1 AND state='active' AND next_occurrence=?8",
            params![
                schedule.id,
                fired as i64 + 1,
                next_due as i64,
                state,
                failures as i64,
                fire.error,
                at as i64,
                fired as i64,
            ],
        )?;
        if changed == 1 {
            tx.execute(
                "INSERT OR REPLACE INTO schedule_fires(schedule_id, occurrence, fired_at,
                    task_id, replayed, authorization, error)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    schedule.id,
                    fired as i64,
                    fire.fired_at as i64,
                    fire.task_id,
                    fire.replayed as i64,
                    serde_json::to_string(&fire.authorization)?,
                    fire.error,
                ],
            )?;
        }
        tx.commit()?;
        Ok(changed == 1)
    }
}

/// Backend and principal a firing acts as: the stored stdio principal, or the edge
/// enrollment as it is now (a revoked enrollment or one that lost submit rights stops it).
fn firing_identity(root: &Path, schedule: &Schedule) -> Result<(LocalBackend, String)> {
    let Some(client_id) = &schedule.client_id else {
        return Ok((
            LocalBackend::open(root.to_path_buf())?,
            schedule.principal.clone(),
        ));
    };
    let enrollment = Enrollments::open(root)?
        .get(client_id)
        .map_err(|_| anyhow::anyhow!("the scheduling enrollment no longer exists"))?;
    if enrollment.revoked {
        bail!("the scheduling enrollment was revoked");
    }
    if !enrollment.allows(EnrollmentOperation::Submit) {
        bail!("the scheduling enrollment may no longer submit");
    }
    Ok((
        LocalBackend::open_trusted(root.to_path_buf(), enrollment.repository_scope)?,
        format!("{PRINCIPAL_NAMESPACE_PREFIX}{client_id}"),
    ))
}

#[derive(Debug, Clone, Serialize)]
pub struct FireOutcome {
    pub schedule_id: String,
    pub occurrence: u32,
    pub task_id: Option<String>,
    pub error: Option<String>,
}

/// Submits every schedule due at `at` (a bounded batch). Returns nothing while the global
/// stop is engaged; due schedules then wait and fire once after it is released.
pub async fn fire_due(root: &Path, at: u64) -> Result<Vec<FireOutcome>> {
    if GlobalStop::open(root)?.state()?.stopped {
        return Ok(Vec::new());
    }
    let mut store = Schedules::open(root)?;
    let mut outcomes = Vec::new();
    for schedule in store.due(at)? {
        let occurrence = schedule.next_occurrence;
        let (backend, principal) = match firing_identity(root, &schedule) {
            Ok(identity) => identity,
            Err(error) => {
                store.fail(&schedule, &error.to_string(), at)?;
                outcomes.push(FireOutcome {
                    schedule_id: schedule.id.clone(),
                    occurrence,
                    task_id: None,
                    error: Some(error.to_string()),
                });
                continue;
            }
        };
        let mut request: SubmitTaskRequest = serde_json::from_value(schedule.submission.clone())
            .context("stored schedule submission is unreadable")?;
        let (workflow_id, key) = occurrence_key(&schedule.id, occurrence);
        request.principal = principal;
        request.workflow_id = Some(workflow_id);
        request.idempotency_key = Some(key);
        request.request_id = Some(format!("schedule-{}-{occurrence}", schedule.id));
        let fire = match backend.conductor_submit_task(request).await {
            Ok(response) => Fire {
                occurrence,
                fired_at: at,
                task_id: response
                    .task
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                replayed: response.replayed,
                authorization: response.authorization,
                error: None,
            },
            Err(error) => Fire {
                occurrence,
                fired_at: at,
                task_id: None,
                replayed: false,
                authorization: Vec::new(),
                error: Some(error.message),
            },
        };
        if store.advance(&schedule, &fire, at)? {
            outcomes.push(FireOutcome {
                schedule_id: schedule.id.clone(),
                occurrence,
                task_id: fire.task_id,
                error: fire.error,
            });
        }
    }
    Ok(outcomes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missed_slots_collapse_into_one_future_slot() {
        assert_eq!(next_slot(1_000, 100, 500), 1_100);
        assert_eq!(next_slot(1_000, 100, 1_000), 1_100);
        assert_eq!(next_slot(1_000, 100, 1_050), 1_100);
        assert_eq!(next_slot(1_000, 100, 1_100), 1_200);
        assert_eq!(next_slot(1_000, 100, 5_432), 5_500);
    }
}
