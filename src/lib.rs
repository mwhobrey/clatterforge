pub mod agent_preset;
pub mod agent_progress;
pub mod agentlet;
pub mod agents;
pub mod authorization;
pub mod autonomy;
pub mod board;
pub mod codex;
pub mod context;
pub mod coordination;
pub mod enrollment;
pub mod execution;
pub mod fixture;
pub mod gateway;
pub mod http_access;
pub mod inference;
pub mod integration;
pub mod launcher;
pub mod logging;
pub mod mcp;
pub mod orchestration;
pub mod process;
pub mod repository;
pub mod runner;
pub mod serve;
pub mod service;
pub mod stop;
pub mod store;
pub mod telemetry;
pub(crate) mod workflow_workspace;

use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before epoch")
        .as_millis() as u64
}

/// Opens (creating if absent) a WAL-mode SQLite connection, retrying past the first-open
/// BUSY race that switching `journal_mode` to WAL can hit without going through SQLite's
/// ordinary busy handler. `inference::state`, `coordination` and `autonomy` each had their
/// own near-identical copy of this; this is their third occurrence, the point at which
/// duplicating a fourth time stopped being worth it (see `coordination.rs`'s own earlier
/// note). The two existing copies are left as they are rather than refactored to call
/// this, since both are already tested and working and there's no reason to touch them
/// just to remove duplication that was already judged acceptable twice before.
pub(crate) fn open_sqlite_with_retry(
    path: &std::path::Path,
    max_version: u32,
) -> anyhow::Result<rusqlite::Connection> {
    use anyhow::ensure;
    use std::time::{Duration, Instant};
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let result = (|| -> anyhow::Result<rusqlite::Connection> {
            let connection = rusqlite::Connection::open(path)?;
            connection.busy_timeout(Duration::from_secs(5))?;
            let version: u32 = connection.query_row("PRAGMA user_version", [], |r| r.get(0))?;
            ensure!(
                version <= max_version,
                "schema is newer than this application"
            );
            let mode: String = connection.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
            if !mode.eq_ignore_ascii_case("wal") {
                connection.execute_batch("PRAGMA journal_mode=WAL;")?;
            }
            connection.execute_batch("PRAGMA foreign_keys=ON;")?;
            Ok(connection)
        })();
        match result {
            Ok(connection) => {
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
                    return Err(
                        error.context("database initialization remained locked after 5 seconds")
                    );
                }
                std::thread::sleep(remaining.min(Duration::from_millis(20)));
            }
        }
    }
}

/// Skip taking a SQLite schema write lock when `sentinel_table` already exists; otherwise
/// run `ddl` inside an `IMMEDIATE` transaction with busy-lock retries.
pub(crate) fn ensure_table_schema(
    connection: &rusqlite::Connection,
    sentinel_table: &str,
    ddl: &str,
) -> anyhow::Result<()> {
    use rusqlite::OptionalExtension;
    let exists: Option<i64> = connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1 LIMIT 1",
            [sentinel_table],
            |r| r.get(0),
        )
        .optional()?;
    if exists.is_some() {
        return Ok(());
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match connection.execute_batch(ddl) {
            Ok(()) => return Ok(()),
            Err(error) => {
                let locked = matches!(
                    error,
                    rusqlite::Error::SqliteFailure(code, _)
                        if matches!(
                            code.code,
                            rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                        )
                );
                if !locked {
                    return Err(error.into());
                }
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    return Err(anyhow::Error::from(error)
                        .context("schema initialization remained locked after 5 seconds"));
                }
                std::thread::sleep(remaining.min(std::time::Duration::from_millis(20)));
            }
        }
    }
}

pub mod onboard;
pub mod provider_status;
pub mod repo_index;
pub mod routing_metrics;
pub mod sessions;
pub mod ui_server;
pub mod workflow;

#[cfg(test)]
mod sqlite_concurrency_tests {
    use super::{ensure_table_schema, open_sqlite_with_retry};

    #[test]
    fn concurrent_open_and_schema_init_does_not_fail_with_database_locked() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("concurrent.db");
        let ddl = "BEGIN IMMEDIATE; CREATE TABLE IF NOT EXISTS items (id INTEGER PRIMARY KEY, val TEXT); COMMIT;";
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let db = db.clone();
                std::thread::spawn(move || {
                    for j in 0..5 {
                        let conn = open_sqlite_with_retry(&db, 1).unwrap();
                        ensure_table_schema(&conn, "items", ddl).unwrap();
                        conn.execute("INSERT INTO items (val) VALUES (?1)", [format!("{i}-{j}")])
                            .unwrap();
                        let count: i64 = conn
                            .query_row("SELECT COUNT(*) FROM items", [], |r| r.get(0))
                            .unwrap();
                        assert!(count >= 1);
                    }
                })
            })
            .collect();
        for handle in threads {
            handle.join().unwrap();
        }
    }
}
