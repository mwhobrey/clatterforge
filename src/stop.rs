//! A single global on/off switch, separate from any one policy: M7's last open
//! acceptance line, "a global stop prevents new admission and reconciles active
//! attempts." Engaging it refuses every future `TaskService::run` dispatch - any engine,
//! any caller (CLI, HTTP, a policy's `hold_my_beer::run`) - until a human explicitly
//! resumes it; this is deliberately broader than any one `Policy`'s revoke, which only
//! ever stops that policy's own future dispatch. `TaskService::stop_all` engages it and
//! then requests cancellation of every currently active attempt through each engine's
//! own already-reviewed `cancel` path - the same supervised stop a human already has for
//! one task at a time, not a new process-killing mechanism, and "reconciles" in the
//! sense of settling what's in flight through an existing, explicit action rather than
//! silently mutating it. One row in the same `state.sqlite` everything else uses.

use crate::open_sqlite_with_retry;
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, params};
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Clone, Serialize)]
pub struct StopState {
    pub stopped: bool,
    pub by: Option<String>,
    pub reason: Option<String>,
    pub at: Option<u64>,
}

impl StopState {
    /// The one check every real dispatch entry point calls before admitting new work -
    /// `TaskService::run` and both of the raw single-engine CLI `run` commands
    /// (`Commands::Run`, `Commands::OrchestrateRun`) alike, so the switch can't be routed
    /// around by picking a different command. Shared here so all three produce the exact
    /// same refusal message instead of three copies that could drift apart.
    pub fn ensure_admission_allowed(&self) -> Result<()> {
        ensure!(
            !self.stopped,
            "global stop is engaged (by {}, reason: {}) - new admission is refused until it's explicitly resumed",
            self.by.as_deref().unwrap_or("?"),
            self.reason.as_deref().unwrap_or("?")
        );
        Ok(())
    }
}

pub struct GlobalStop {
    connection: Connection,
}

impl GlobalStop {
    pub fn open(root: &Path) -> Result<Self> {
        let connection = open_sqlite_with_retry(&root.join("state.sqlite"), 1)
            .context("open global stop state")?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS global_stop(
                id INTEGER PRIMARY KEY CHECK (id = 0),
                stopped INTEGER NOT NULL,
                by TEXT,
                reason TEXT,
                at INTEGER
            );
            INSERT OR IGNORE INTO global_stop (id, stopped, by, reason, at)
                VALUES (0, 0, NULL, NULL, NULL);",
        )?;
        Ok(Self { connection })
    }

    pub fn state(&self) -> Result<StopState> {
        self.connection
            .query_row(
                "SELECT stopped, by, reason, at FROM global_stop WHERE id = 0",
                [],
                |row| {
                    Ok(StopState {
                        stopped: row.get::<_, i64>(0)? != 0,
                        by: row.get(1)?,
                        reason: row.get(2)?,
                        at: row.get::<_, Option<i64>>(3)?.map(|v| v as u64),
                    })
                },
            )
            .context("read global stop state")
    }

    pub fn engage(&mut self, by: &str, reason: &str) -> Result<StopState> {
        ensure!(
            !by.trim().is_empty() && !reason.trim().is_empty(),
            "engaging a global stop needs an explicit by/reason"
        );
        self.connection.execute(
            "UPDATE global_stop SET stopped = 1, by = ?1, reason = ?2, at = ?3 WHERE id = 0",
            params![by, reason, crate::now() as i64],
        )?;
        self.state()
    }

    /// Only ever clears the switch - never touches a task, exactly as "mode changes do
    /// not silently authorize or alter in-flight work" requires. Nothing auto-resumes or
    /// auto-relaunches anything that was cancelled while the stop was engaged; that stays
    /// a human's explicit next action, same as every other state transition here.
    pub fn disengage(&mut self, by: &str, reason: &str) -> Result<StopState> {
        ensure!(
            !by.trim().is_empty() && !reason.trim().is_empty(),
            "resuming from a global stop needs an explicit by/reason"
        );
        ensure!(self.state()?.stopped, "global stop is not engaged");
        self.connection.execute(
            "UPDATE global_stop SET stopped = 0, by = ?1, reason = ?2, at = ?3 WHERE id = 0",
            params![by, reason, crate::now() as i64],
        )?;
        self.state()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_state_is_not_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let stop = GlobalStop::open(dir.path()).unwrap();
        let state = stop.state().unwrap();
        assert!(!state.stopped);
        assert!(state.by.is_none());
    }

    #[test]
    fn engage_requires_by_and_reason() {
        let dir = tempfile::tempdir().unwrap();
        let mut stop = GlobalStop::open(dir.path()).unwrap();
        assert!(stop.engage("", "reason").is_err());
        assert!(stop.engage("human", "").is_err());
    }

    #[test]
    fn engage_then_disengage_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let mut stop = GlobalStop::open(dir.path()).unwrap();
        let engaged = stop.engage("human", "incident").unwrap();
        assert!(engaged.stopped);
        assert_eq!(engaged.by.as_deref(), Some("human"));
        let resumed = stop.disengage("human", "resolved").unwrap();
        assert!(!resumed.stopped);
        assert_eq!(resumed.reason.as_deref(), Some("resolved"));
    }

    #[test]
    fn disengaging_when_not_engaged_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut stop = GlobalStop::open(dir.path()).unwrap();
        let err = stop.disengage("human", "reason").unwrap_err();
        assert!(err.to_string().contains("not engaged"), "{err}");
    }

    #[test]
    fn state_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        GlobalStop::open(dir.path())
            .unwrap()
            .engage("human", "incident")
            .unwrap();
        let reopened = GlobalStop::open(dir.path()).unwrap().state().unwrap();
        assert!(reopened.stopped);
        assert_eq!(reopened.by.as_deref(), Some("human"));
    }
}
