use super::{
    config::{Config, Registry},
    types::*,
};
use crate::{
    now,
    process::{Presence, ProcessIdentity, presence},
};
use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Ready,
    Preparing,
    Running,
    Checkpoint,
    Completed,
    Failed,
    Cancelled,
    TimedOut,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub schema_version: u32,
    pub id: String,
    pub revision: u64,
    pub status: JobStatus,
    pub config: Config,
    pub registry: Option<Registry>,
    pub request: Request,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_request_digest: Option<String>,
    pub target_override: Option<String>,
    pub selected_target: Option<String>,
    pub generations: Vec<Generation>,
    pub max_generations: usize,
    pub owner: Option<ProcessIdentity>,
    pub owner_token: Option<String>,
    pub cancel_requested: bool,
    pub failure: Option<Failure>,
    pub routing_basis: Option<String>,
}

pub struct JobStore {
    connection: Connection,
    pub root: PathBuf,
}
impl JobStore {
    pub fn open(root: &Path) -> Result<Self> {
        std::fs::create_dir_all(root)?;
        let root = crate::repository::canonical(root)?;
        let connection = database(&root.join("inference.sqlite"))?;
        connection.execute_batch("CREATE TABLE IF NOT EXISTS jobs(id TEXT PRIMARY KEY, revision INTEGER NOT NULL, record TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS events(job_id TEXT NOT NULL, sequence INTEGER NOT NULL, at INTEGER NOT NULL, kind TEXT NOT NULL, record TEXT NOT NULL, PRIMARY KEY(job_id, sequence));")?;
        Ok(Self { connection, root })
    }
    pub fn create(
        &mut self,
        config: Config,
        registry: Option<Registry>,
        request: Request,
        target_override: Option<String>,
    ) -> Result<Job> {
        config.validate()?;
        request.validate()?;
        if let Some(registry) = &registry {
            registry.validate(&config)?;
        }
        if let Some(id) = &target_override {
            config.target(id)?;
        }
        let job = Job {
            schema_version: VERSION,
            id: uuid::Uuid::new_v4().to_string(),
            revision: 1,
            status: JobStatus::Ready,
            config,
            registry,
            initial_request_digest: Some(request.fingerprint()),
            request,
            target_override,
            selected_target: None,
            generations: vec![],
            max_generations: 3,
            owner: None,
            owner_token: None,
            cancel_requested: false,
            failure: None,
            routing_basis: None,
        };
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let record = serde_json::to_string(&job)?;
        tx.execute(
            "INSERT INTO jobs VALUES(?1,?2,?3)",
            params![job.id, job.revision, record],
        )?;
        tx.execute(
            "INSERT INTO events VALUES(?1,1,?2,'prepared',?3)",
            params![job.id, now(), record],
        )?;
        tx.commit()?;
        Ok(job)
    }
    pub fn job(&self, id: &str) -> Result<Job> {
        let body: String =
            self.connection
                .query_row("SELECT record FROM jobs WHERE id=?1", [id], |r| r.get(0))?;
        let job: Job = serde_json::from_str(&body)?;
        ensure!(
            job.schema_version == VERSION,
            "newer canonical job contract"
        );
        Ok(job)
    }
    pub fn events(&self, id: &str) -> Result<Vec<serde_json::Value>> {
        let mut stmt = self.connection.prepare(
            "SELECT sequence,at,kind,record FROM events WHERE job_id=?1 ORDER BY sequence",
        )?;
        let rows = stmt.query_map([id], |r| {
            Ok((
                r.get::<_, u64>(0)?,
                r.get::<_, u64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        let mut output = Vec::new();
        for row in rows {
            let (revision, at, kind, body) = row?;
            output.push(serde_json::json!({"revision":revision,"at":at,"kind":kind,"event":serde_json::from_str::<serde_json::Value>(&body)?}));
        }
        Ok(output)
    }
    pub fn mutate(
        &mut self,
        id: &str,
        expected: Option<u64>,
        owner: Option<&str>,
        kind: &str,
        event: serde_json::Value,
        change: impl FnOnce(&mut Job) -> Result<()>,
    ) -> Result<Job> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let body: String =
            tx.query_row("SELECT record FROM jobs WHERE id=?1", [id], |r| r.get(0))?;
        let mut job: Job = serde_json::from_str(&body)?;
        ensure!(
            expected.is_none_or(|v| job.revision == v),
            "job revision conflict"
        );
        ensure!(
            owner.is_none_or(|v| job.owner_token.as_deref() == Some(v)),
            "job owner changed"
        );
        change(&mut job)?;
        job.revision += 1;
        let body = serde_json::to_string(&job)?;
        ensure!(body.len() <= 1048576, "job snapshot exceeds 1 MiB");
        tx.execute(
            "UPDATE jobs SET revision=?2,record=?3 WHERE id=?1",
            params![id, job.revision, body],
        )?;
        tx.execute(
            "INSERT INTO events VALUES(?1,?2,?3,?4,?5)",
            params![id, job.revision, now(), kind, event.to_string()],
        )?;
        tx.commit()?;
        Ok(job)
    }
    pub fn cancel(&mut self, id: &str, revision: u64) -> Result<Job> {
        self.mutate(
            id,
            Some(revision),
            None,
            "cancel_requested",
            serde_json::json!({}),
            |j| {
                ensure!(
                    matches!(
                        j.status,
                        JobStatus::Ready
                            | JobStatus::Preparing
                            | JobStatus::Running
                            | JobStatus::Checkpoint
                    ),
                    "job is terminal"
                );
                j.cancel_requested = true;
                if !matches!(j.status, JobStatus::Running | JobStatus::Preparing) {
                    j.status = JobStatus::Cancelled;
                }
                Ok(())
            },
        )
    }
    pub fn reconcile(&mut self, id: &str, revision: u64) -> Result<Job> {
        let job=self.mutate(id,Some(revision),None,"recovery_unknown",serde_json::json!({}),|j|{
            ensure!(matches!(j.status,JobStatus::Running|JobStatus::Preparing|JobStatus::Checkpoint),"job is not interrupted");
            ensure!(j.owner.as_ref().is_some_and(|o|!matches!(presence(o),Presence::Alive|Presence::Unverifiable)),"owner is alive or uninspectable");
            j.status=JobStatus::Unknown;
            j.failure=Some(Failure::new(ErrorKind::InternalFailure,"interrupted generation; partial output is evidence, not canonical assistant state; no replay"));
            Ok(())
        })?;
        Coordinator::open(&job.config.admission_db)?.quarantine_job(&job.id)?;
        Ok(job)
    }
    pub fn stream_event(&mut self, id: &str, owner: &str, event: StreamEvent) -> Result<Job> {
        let record = serde_json::to_value(&event)?;
        self.mutate(id, None, Some(owner), "stream", record, |j| {
            ensure!(j.status == JobStatus::Running, "no running generation");
            let generation = j
                .generations
                .last_mut()
                .ok_or_else(|| anyhow::anyhow!("generation intent missing"))?;
            match event {
                StreamEvent::TextDelta { text } => generation.text.push_str(&text),
                StreamEvent::ReasoningDelta { text } => generation.reasoning.push_str(&text),
                StreamEvent::UsageUpdate { usage } => generation.usage.merge_report(&usage),
                StreamEvent::Finish { reason } => {
                    generation.finish_reason = Some(reason);
                    generation.terminal_events += 1;
                }
                StreamEvent::Error { failure } => {
                    generation.failure = Some(failure);
                    generation.terminal_events += 1;
                }
                StreamEvent::ToolCallDelta { .. } => (),
            }
            ensure!(
                generation.text.len() + generation.reasoning.len() <= 65536
                    && generation.terminal_events <= 1,
                "stream state exceeds bounds/duplicate terminal"
            );
            Ok(())
        })
    }
}

fn database(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let result = (|| -> Result<Connection> {
            let connection = Connection::open(path)?;
            // WAL initialization can return BUSY without invoking SQLite's busy handler.
            connection.busy_timeout(Duration::ZERO)?;
            let version: u32 = connection.query_row("PRAGMA user_version", [], |r| r.get(0))?;
            ensure!(version <= VERSION, "newer inference/coordinator schema");
            connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA user_version=1;")?;
            Ok(connection)
        })();
        match result {
            Ok(connection) => {
                connection.busy_timeout(Duration::from_secs(5))?;
                return Ok(connection);
            }
            Err(error) => {
                let locked = matches!(
                    error.downcast_ref::<rusqlite::Error>(),
                    Some(rusqlite::Error::SqliteFailure(code, _))
                        if matches!(code.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
                );
                if !locked {
                    return Err(error);
                }
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                std::thread::sleep(remaining.min(Duration::from_millis(20)));
                if std::time::Instant::now() >= deadline {
                    return Err(error.context(
                        "inference/coordinator database initialization remained locked after 5 seconds",
                    ));
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoRelease {
    /// Nothing was reserved for this resource at all.
    NoReservation,
    /// A reservation exists, but the owning process is still alive or its
    /// liveness can't be determined - left exactly as-is for a human to
    /// resolve, same bar `inference-release-resource` already enforces.
    UnsafeOwnerLiveOrUnverifiable,
    /// Owner confirmed not alive/unverifiable, quarantined or not:
    /// released automatically, recorded in `acknowledgements` as
    /// `"automatic"`.
    Released,
}

pub struct Coordinator {
    connection: Connection,
}
impl Coordinator {
    pub fn open(path: &Path) -> Result<Self> {
        let connection = database(path)?;
        connection.execute_batch("CREATE TABLE IF NOT EXISTS leases(resource TEXT PRIMARY KEY,job TEXT NOT NULL,owner TEXT NOT NULL,quarantined INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS acknowledgements(at INTEGER NOT NULL,resource TEXT NOT NULL,job TEXT NOT NULL,by_label TEXT NOT NULL,reason TEXT NOT NULL);")?;
        Ok(Self { connection })
    }
    pub fn acquire(&mut self, resource: &str, job: &str, owner: &ProcessIdentity) -> Result<()> {
        let changed = self.connection.execute(
            "INSERT OR IGNORE INTO leases VALUES(?1,?2,?3,0)",
            params![resource, job, serde_json::to_string(owner)?],
        )?;
        ensure!(
            changed == 1,
            "resource {resource:?} busy or abort uncertainty quarantined; no dispatch; inspect inference-resources before preparing a fresh assignment"
        );
        Ok(())
    }
    pub fn release(&mut self, resource: &str, job: &str) -> Result<()> {
        self.connection.execute(
            "DELETE FROM leases WHERE resource=?1 AND job=?2 AND quarantined=0",
            params![resource, job],
        )?;
        Ok(())
    }
    pub fn quarantine_job(&mut self, job: &str) -> Result<()> {
        self.connection
            .execute("UPDATE leases SET quarantined=1 WHERE job=?1", [job])?;
        Ok(())
    }
    pub fn acknowledge(&mut self, resource: &str, by: &str, reason: &str) -> Result<()> {
        ensure!(
            !by.trim().is_empty()
                && by.len() <= 128
                && !reason.trim().is_empty()
                && reason.len() <= 512,
            "explicit acknowledgement label/reason required"
        );
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row: Option<(String, String, bool)> = tx
            .query_row(
                "SELECT job,owner,quarantined FROM leases WHERE resource=?1",
                [resource],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let (job, owner, quarantined) =
            row.ok_or_else(|| anyhow::anyhow!("no resource reservation"))?;
        let owner: ProcessIdentity = serde_json::from_str(&owner)?;
        ensure!(
            quarantined || !matches!(presence(&owner), Presence::Alive | Presence::Unverifiable),
            "live owner still holds resource"
        );
        tx.execute(
            "INSERT INTO acknowledgements VALUES(?1,?2,?3,?4,?5)",
            params![now(), resource, job, by, reason],
        )?;
        tx.execute("DELETE FROM leases WHERE resource=?1", [resource])?;
        tx.commit()?;
        Ok(())
    }
    /// Bounded, conservative automatic recovery for the admission path: a
    /// job trying to dispatch against a resource that an *earlier* job
    /// already left quarantined. Unlike `acknowledge`, this never takes a
    /// human label - it only releases when the exact same safety bar
    /// `acknowledge` already enforces is met (the owning process is
    /// confirmed not alive/unverifiable), and it is only ever reached from
    /// the automated dispatch path, never substituting for a human's
    /// explicit acknowledgement when that owner is still alive or
    /// unverifiable. Distinguishes every outcome M9 asked to keep
    /// separate: nothing to do, unsafe to touch, or positively recovered.
    pub fn try_automatic_release(&mut self, resource: &str) -> Result<AutoRelease> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row: Option<(String, String, bool)> = tx
            .query_row(
                "SELECT job,owner,quarantined FROM leases WHERE resource=?1",
                [resource],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((job, owner, _quarantined)) = row else {
            return Ok(AutoRelease::NoReservation);
        };
        // Deliberately does not gate on `quarantined` the way an earlier
        // version of this function did: a process killed outright
        // (SIGKILL, or any abrupt termination) never runs its own
        // Drop-based quarantine fallback, so a dead owner's lease can be
        // sitting here with `quarantined=0` - "held," not "quarantined" -
        // even though the owner is just as dead either way. `acknowledge`
        // (the manual path) already checks owner liveness regardless of
        // the quarantined flag for exactly this reason; this automatic
        // path now matches it, found and fixed by testing against a real
        // killed process instead of only a simulated quarantined state.
        let owner: ProcessIdentity = serde_json::from_str(&owner)?;
        if matches!(presence(&owner), Presence::Alive | Presence::Unverifiable) {
            return Ok(AutoRelease::UnsafeOwnerLiveOrUnverifiable);
        }
        tx.execute(
            "INSERT INTO acknowledgements VALUES(?1,?2,?3,?4,?5)",
            params![
                now(),
                resource,
                job,
                "automatic",
                "owner process confirmed not alive/unverifiable; recovered without human action"
            ],
        )?;
        tx.execute("DELETE FROM leases WHERE resource=?1", [resource])?;
        tx.commit()?;
        Ok(AutoRelease::Released)
    }
    pub fn reservations(&self) -> Result<Vec<serde_json::Value>> {
        let mut stmt = self
            .connection
            .prepare("SELECT resource,job,owner,quarantined FROM leases ORDER BY resource")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, bool>(3)?,
            ))
        })?;
        let mut records = vec![];
        for row in rows {
            let (resource, job, owner, quarantined) = row?;
            records.push(serde_json::json!({"resource":resource,"job":job,"owner":serde_json::from_str::<ProcessIdentity>(&owner)?,"quarantined":quarantined}));
        }
        Ok(records)
    }
}
