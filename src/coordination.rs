//! Cross-engine task dependencies and repository leasing - the two M6 "controlled parallel
//! work" primitives that don't belong to either coding engine's own schema. Dependencies
//! are a pure ordering constraint ("don't run B until A is accepted"); leases are what
//! actually lets two tasks against *different* repositories run concurrently while two
//! against the *same* one serialize, per the milestone's own acceptance line. Both live in
//! the same `state.sqlite` file `Store` already owns (a second, independent connection to
//! it - this codebase already opens that file fresh per operation everywhere else), not a
//! separate file: unlike `inference.sqlite`, which is a genuinely separate job subsystem,
//! these are new relations *on* the existing tasks, not a new domain.

use crate::process::{Presence, ProcessIdentity, presence};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::{
    collections::HashSet,
    path::Path,
    time::{Duration, Instant},
};

const VERSION: u32 = 1;

/// Opens (creating if absent) a WAL-mode connection to `state.sqlite`, retrying past the
/// same first-open BUSY race `inference::state`'s own `database()` helper was written to
/// fix (switching journal_mode to WAL can return BUSY without going through SQLite's
/// ordinary busy handler, so the normal `busy_timeout` doesn't cover it). Duplicated here
/// rather than shared, since `Store::open` already has its own simpler version against the
/// same file and three near-identical small copies isn't yet worth an extraction - revisit
/// if a fourth caller shows up.
fn database(path: &Path) -> Result<Connection> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let result = (|| -> Result<Connection> {
            let connection = Connection::open(path)?;
            connection.busy_timeout(Duration::ZERO)?;
            let version: u32 = connection.query_row("PRAGMA user_version", [], |r| r.get(0))?;
            ensure!(
                version <= VERSION,
                "coordination schema is newer than this application"
            );
            connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
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
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(error.context(
                        "coordination database initialization remained locked after 5 seconds",
                    ));
                }
                std::thread::sleep(remaining.min(Duration::from_millis(20)));
            }
        }
    }
}

pub struct Coordination {
    connection: Connection,
}

impl Coordination {
    pub fn open(root: &Path) -> Result<Self> {
        std::fs::create_dir_all(root)?;
        let connection = database(&root.join("state.sqlite")).context("open coordination state")?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS dependencies(
                task_id TEXT NOT NULL,
                depends_on TEXT NOT NULL,
                PRIMARY KEY (task_id, depends_on)
            );
            CREATE TABLE IF NOT EXISTS leases(
                resource TEXT PRIMARY KEY,
                task_id TEXT NOT NULL,
                owner TEXT NOT NULL,
                quarantined INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS lease_acknowledgements(
                at INTEGER NOT NULL,
                resource TEXT NOT NULL,
                task_id TEXT NOT NULL,
                by_label TEXT NOT NULL,
                reason TEXT NOT NULL
            );",
        )?;
        Ok(Self { connection })
    }

    pub fn dependencies_of(&self, task_id: &str) -> Result<Vec<String>> {
        let mut stmt = self
            .connection
            .prepare("SELECT depends_on FROM dependencies WHERE task_id=?1 ORDER BY depends_on")?;
        let rows = stmt.query_map([task_id], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("read dependencies")
    }

    /// Depth-first search from `candidate_depends_on` through its own already-recorded
    /// dependencies; if that walk ever reaches `task_id`, recording
    /// `task_id -> candidate_depends_on` would close a cycle (a dependency chain that can
    /// never resolve, since every task in the cycle waits on another task in the same
    /// cycle). Unbounded in theory, bounded in practice by `seen` - a real dependency
    /// graph in this system is expected to stay small.
    fn creates_cycle(&self, task_id: &str, candidate_depends_on: &str) -> Result<bool> {
        let mut stack = vec![candidate_depends_on.to_string()];
        let mut seen = HashSet::new();
        while let Some(current) = stack.pop() {
            if current == task_id {
                return Ok(true);
            }
            if !seen.insert(current.clone()) {
                continue;
            }
            stack.extend(self.dependencies_of(&current)?);
        }
        Ok(false)
    }

    /// Records that `task_id` cannot `run` until `depends_on` reaches `accepted`. Does not
    /// itself validate that either id resolves to a real task - `TaskService` does that
    /// with its existing id/prefix resolution before calling this, so this module stays
    /// free of any engine-specific lookup.
    pub fn add_dependency(&mut self, task_id: &str, depends_on: &str) -> Result<()> {
        ensure!(task_id != depends_on, "a task cannot depend on itself");
        ensure!(
            !self.creates_cycle(task_id, depends_on)?,
            "would create a dependency cycle"
        );
        self.connection.execute(
            "INSERT OR IGNORE INTO dependencies VALUES (?1, ?2)",
            params![task_id, depends_on],
        )?;
        Ok(())
    }

    /// Atomic: `INSERT OR IGNORE` either inserts the one row for `resource` (lease
    /// acquired) or touches nothing because a row for it already exists (lease held by
    /// someone else, `changed` is 0). No blocking/retry/queueing - callers that lose the
    /// race get a clear, immediate error, same as the existing revision-conflict and
    /// inference-admission conventions in this codebase; a human (or a future scheduler)
    /// decides whether and when to try again.
    pub fn acquire(
        &mut self,
        resource: &str,
        task_id: &str,
        owner: &ProcessIdentity,
    ) -> Result<()> {
        let changed = self.connection.execute(
            "INSERT OR IGNORE INTO leases VALUES(?1,?2,?3,0)",
            params![resource, task_id, serde_json::to_string(owner)?],
        )?;
        ensure!(
            changed == 1,
            "repository {resource:?} busy or quarantined; no dispatch; inspect coordination-leases before preparing a fresh assignment"
        );
        Ok(())
    }

    pub fn release(&mut self, resource: &str, task_id: &str) -> Result<()> {
        self.connection.execute(
            "DELETE FROM leases WHERE resource=?1 AND task_id=?2 AND quarantined=0",
            params![resource, task_id],
        )?;
        Ok(())
    }

    /// Marks a held lease as needing explicit human acknowledgement instead of silently
    /// freeing it - the conservative default when a reservation's `Drop` guard fires
    /// because its owning process unwound without ever calling `finish`, meaning the
    /// actual state of the repository/workspace that lease was protecting is genuinely
    /// unknown, not just "the attempt failed" (which is already handled by an ordinary,
    /// deterministic `release`).
    pub fn quarantine_task(&mut self, task_id: &str) -> Result<()> {
        self.connection.execute(
            "UPDATE leases SET quarantined=1 WHERE task_id=?1",
            [task_id],
        )?;
        Ok(())
    }

    /// Explicit, human-labeled release of a quarantined lease, mirroring
    /// `inference::state::Coordinator::acknowledge`. Refuses while the owning process
    /// still looks alive (or its liveness can't be determined at all) - quarantine exists
    /// precisely to stop a second dispatch from racing a process that might still be
    /// touching that repository.
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
                "SELECT task_id,owner,quarantined FROM leases WHERE resource=?1",
                [resource],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let (task_id, owner, quarantined) =
            row.ok_or_else(|| anyhow::anyhow!("no lease reservation for {resource:?}"))?;
        let owner: ProcessIdentity = serde_json::from_str(&owner)?;
        ensure!(
            quarantined || !matches!(presence(&owner), Presence::Alive | Presence::Unverifiable),
            "live owner still holds this lease"
        );
        tx.execute(
            "INSERT INTO lease_acknowledgements VALUES (?1,?2,?3,?4,?5)",
            params![crate::now(), resource, task_id, by, reason],
        )?;
        tx.execute("DELETE FROM leases WHERE resource=?1", [resource])?;
        tx.commit()?;
        Ok(())
    }

    pub fn reservations(&self) -> Result<Vec<serde_json::Value>> {
        let mut stmt = self
            .connection
            .prepare("SELECT resource,task_id,owner,quarantined FROM leases ORDER BY resource")?;
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
            let (resource, task_id, owner, quarantined) = row?;
            records.push(serde_json::json!({
                "resource": resource,
                "task_id": task_id,
                "owner": serde_json::from_str::<ProcessIdentity>(&owner)?,
                "quarantined": quarantined,
            }));
        }
        Ok(records)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::current_identity;

    fn test_owner_identity() -> ProcessIdentity {
        current_identity().expect("current process identity")
    }

    #[test]
    fn dependency_cycle_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = Coordination::open(dir.path()).unwrap();
        c.add_dependency("b", "a").unwrap();
        c.add_dependency("c", "b").unwrap();
        let err = c.add_dependency("a", "c").unwrap_err();
        assert!(err.to_string().contains("cycle"), "{err}");
    }

    #[test]
    fn self_dependency_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = Coordination::open(dir.path()).unwrap();
        let err = c.add_dependency("a", "a").unwrap_err();
        assert!(err.to_string().contains("itself"), "{err}");
    }

    #[test]
    fn duplicate_dependency_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = Coordination::open(dir.path()).unwrap();
        c.add_dependency("b", "a").unwrap();
        c.add_dependency("b", "a").unwrap();
        assert_eq!(c.dependencies_of("b").unwrap(), vec!["a".to_string()]);
    }

    #[test]
    fn second_acquire_of_a_held_resource_fails() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = Coordination::open(dir.path()).unwrap();
        let owner = test_owner_identity();
        c.acquire("repo-a", "task-1", &owner).unwrap();
        let err = c.acquire("repo-a", "task-2", &owner).unwrap_err();
        assert!(err.to_string().contains("busy"), "{err}");
    }

    #[test]
    fn different_resources_do_not_contend() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = Coordination::open(dir.path()).unwrap();
        let owner = test_owner_identity();
        c.acquire("repo-a", "task-1", &owner).unwrap();
        c.acquire("repo-b", "task-2", &owner).unwrap();
    }

    #[test]
    fn release_then_reacquire_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = Coordination::open(dir.path()).unwrap();
        let owner = test_owner_identity();
        c.acquire("repo-a", "task-1", &owner).unwrap();
        c.release("repo-a", "task-1").unwrap();
        c.acquire("repo-a", "task-2", &owner).unwrap();
    }

    #[test]
    fn quarantined_lease_blocks_reacquire_until_acknowledged() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = Coordination::open(dir.path()).unwrap();
        // A fake PID astronomically unlikely to be alive, so `presence` reports it isn't.
        let dead_owner = ProcessIdentity {
            pid: u32::MAX,
            created: 1,
        };
        c.acquire("repo-a", "task-1", &dead_owner).unwrap();
        c.quarantine_task("task-1").unwrap();
        let err = c.acquire("repo-a", "task-2", &dead_owner).unwrap_err();
        assert!(err.to_string().contains("busy"), "{err}");
        c.acknowledge("repo-a", "tester", "process confirmed dead")
            .unwrap();
        c.acquire("repo-a", "task-2", &dead_owner).unwrap();
    }
}
