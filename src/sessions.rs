//! Persistent, hashed, revocable paired-device sessions for remote web/mobile access.
//!
//! Pairing codes remain ephemeral (5 minutes, in-memory, single-use, 5-attempt lockout in
//! `serve.rs`), while the resulting device sessions persist in `state.sqlite` as SHA-256
//! hashes so a paired phone stays signed in across server restarts and PC reboots without
//! ever storing plaintext session or CSRF secrets on disk.

use crate::{digest, now, open_sqlite_with_retry};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::path::Path;
use uuid::Uuid;

pub const SESSION_TTL_MS: u64 = 30 * 24 * 60 * 60 * 1000;
/// Absolute cap from pairing time. Sliding renewal never extends a session past this, so a
/// device in regular use must still re-pair periodically.
pub const SESSION_MAX_LIFETIME_MS: u64 = 90 * 24 * 60 * 60 * 1000;
const SLIDING_TOUCH_INTERVAL_MS: u64 = 60 * 60 * 1000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionSummary {
    pub id: String,
    pub principal: String,
    pub created_at: u64,
    pub last_seen_at: u64,
    pub expires_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedSession {
    pub id: String,
    pub principal: String,
    pub csrf_hash: String,
    pub expires_at: u64,
}

pub struct Sessions {
    conn: Connection,
}

impl Sessions {
    pub fn open(root: &Path) -> Result<Self> {
        std::fs::create_dir_all(root).context("create state root for sessions")?;
        let conn = open_sqlite_with_retry(&root.join("state.sqlite"), 1)
            .context("open state.sqlite for paired sessions")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS paired_sessions (
                id TEXT PRIMARY KEY,
                token_hash TEXT NOT NULL UNIQUE,
                csrf_hash TEXT NOT NULL,
                principal TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                last_seen_at INTEGER NOT NULL,
                expires_at INTEGER NOT NULL,
                revoked_at INTEGER
            );
            CREATE INDEX IF NOT EXISTS idx_paired_sessions_active
                ON paired_sessions(revoked_at, expires_at);",
        )
        .context("initialize paired_sessions schema")?;
        Ok(Self { conn })
    }

    pub fn create(
        &self,
        raw_session_token: &str,
        raw_csrf_token: &str,
        principal: &str,
        ttl_ms: u64,
    ) -> Result<SessionSummary> {
        let principal = principal.trim();
        ensure!(!principal.is_empty(), "principal is required");
        ensure!(!raw_session_token.is_empty(), "session token is required");
        ensure!(!raw_csrf_token.is_empty(), "csrf token is required");

        let id = Uuid::new_v4().to_string();
        let token_hash = digest(raw_session_token.as_bytes());
        let csrf_hash = digest(raw_csrf_token.as_bytes());
        let created_at = now();
        let expires_at = created_at.saturating_add(ttl_ms.min(SESSION_MAX_LIFETIME_MS));

        self.conn
            .execute(
                "INSERT INTO paired_sessions
                    (id, token_hash, csrf_hash, principal, created_at, last_seen_at, expires_at, revoked_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6, NULL)",
                params![
                    &id,
                    &token_hash,
                    &csrf_hash,
                    principal,
                    created_at as i64,
                    expires_at as i64,
                ],
            )
            .context("insert paired session")?;

        Ok(SessionSummary {
            id,
            principal: principal.to_string(),
            created_at,
            last_seen_at: created_at,
            expires_at,
        })
    }

    /// Verifies a presented raw session cookie value against the stored SHA-256 hash.
    /// Automatically slides `expires_at` forward when `last_seen_at` is older than 1 hour.
    pub fn verify(&self, raw_session_token: &str) -> Result<Option<VerifiedSession>> {
        let token_hash = digest(raw_session_token.as_bytes());
        let current_now = now();
        #[allow(clippy::type_complexity)]
        let row: Option<(String, String, String, i64, i64, Option<i64>, i64)> = self
            .conn
            .query_row(
                "SELECT id, principal, csrf_hash, last_seen_at, expires_at, revoked_at, created_at
                 FROM paired_sessions
                 WHERE token_hash = ?1",
                params![&token_hash],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                },
            )
            .optional()
            .context("query paired session")?;

        let Some((id, principal, csrf_hash, last_seen_at, expires_at, revoked_at, created_at)) =
            row
        else {
            return Ok(None);
        };
        let hard_limit = (created_at as u64).saturating_add(SESSION_MAX_LIFETIME_MS);
        // `hard_limit` also covers rows written before the cap existed, whose stored
        // `expires_at` may already have slid past it.
        if revoked_at.is_some() || (expires_at as u64).min(hard_limit) <= current_now {
            return Ok(None);
        }

        let mut effective_expires = (expires_at as u64).min(hard_limit);
        if current_now.saturating_sub(last_seen_at as u64) >= SLIDING_TOUCH_INTERVAL_MS {
            let renewed = current_now.saturating_add(SESSION_TTL_MS).min(hard_limit);
            let _ = self.conn.execute(
                "UPDATE paired_sessions
                 SET last_seen_at = ?1, expires_at = ?2
                 WHERE id = ?3 AND revoked_at IS NULL",
                params![current_now as i64, renewed as i64, &id],
            );
            effective_expires = renewed;
        }

        Ok(Some(VerifiedSession {
            id,
            principal,
            csrf_hash,
            expires_at: effective_expires,
        }))
    }

    pub fn verify_csrf(&self, session: &VerifiedSession, presented_csrf: &str) -> bool {
        let presented_hash = digest(presented_csrf.as_bytes());
        constant_time_eq_bytes(presented_hash.as_bytes(), session.csrf_hash.as_bytes())
    }

    pub fn revoke_by_token(&self, raw_session_token: &str) -> Result<bool> {
        let token_hash = digest(raw_session_token.as_bytes());
        let updated = self
            .conn
            .execute(
                "UPDATE paired_sessions SET revoked_at = ?1 WHERE token_hash = ?2 AND revoked_at IS NULL",
                params![now() as i64, &token_hash],
            )
            .context("revoke paired session by token")?;
        Ok(updated > 0)
    }

    pub fn revoke_by_id(&self, id: &str) -> Result<bool> {
        let updated = self
            .conn
            .execute(
                "UPDATE paired_sessions SET revoked_at = ?1 WHERE id = ?2 AND revoked_at IS NULL",
                params![now() as i64, id],
            )
            .context("revoke paired session by id")?;
        Ok(updated > 0)
    }

    pub fn revoke_all(&self) -> Result<usize> {
        let current_now = now() as i64;
        let updated = self
            .conn
            .execute(
                "UPDATE paired_sessions SET revoked_at = ?1 WHERE revoked_at IS NULL AND expires_at > ?1",
                params![current_now],
            )
            .context("revoke all paired sessions")?;
        Ok(updated)
    }

    pub fn list_active(&self) -> Result<Vec<SessionSummary>> {
        let current_now = now() as i64;
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, principal, created_at, last_seen_at, expires_at
                 FROM paired_sessions
                 WHERE revoked_at IS NULL AND expires_at > ?1 AND created_at + ?2 > ?1
                 ORDER BY last_seen_at DESC, created_at DESC",
            )
            .context("prepare list active paired sessions")?;
        let rows = stmt
            .query_map(params![current_now, SESSION_MAX_LIFETIME_MS as i64], |r| {
                Ok(SessionSummary {
                    id: r.get(0)?,
                    principal: r.get(1)?,
                    created_at: r.get::<_, i64>(2)? as u64,
                    last_seen_at: r.get::<_, i64>(3)? as u64,
                    expires_at: r.get::<_, i64>(4)? as u64,
                })
            })
            .context("query active paired sessions")?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .context("collect active paired sessions")
    }
}

fn constant_time_eq_bytes(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_persist_across_reopen_hash_secrets_and_revoke_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let summary = {
            let store = Sessions::open(dir.path()).unwrap();
            store
                .create(
                    "raw-session-secret",
                    "raw-csrf-secret",
                    "Mike",
                    SESSION_TTL_MS,
                )
                .unwrap()
        };

        // Reopen fresh connection (simulating server restart / PC reboot)
        let reopened = Sessions::open(dir.path()).unwrap();
        let verified = reopened
            .verify("raw-session-secret")
            .unwrap()
            .expect("session must survive reopen");
        assert_eq!(verified.id, summary.id);
        assert_eq!(verified.principal, "Mike");
        assert!(reopened.verify_csrf(&verified, "raw-csrf-secret"));
        assert!(!reopened.verify_csrf(&verified, "wrong-csrf"));
        assert!(reopened.verify("wrong-token").unwrap().is_none());

        // Plaintext secrets must never appear in SQLite
        let raw_db = std::fs::read(dir.path().join("state.sqlite")).unwrap();
        let db_str = String::from_utf8_lossy(&raw_db);
        assert!(!db_str.contains("raw-session-secret"));
        assert!(!db_str.contains("raw-csrf-secret"));

        // Listed in active sessions and revocable by id
        let active = reopened.list_active().unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, summary.id);
        assert!(reopened.revoke_by_id(&summary.id).unwrap());
        assert!(reopened.verify("raw-session-secret").unwrap().is_none());
        assert!(reopened.list_active().unwrap().is_empty());
    }

    #[test]
    fn sliding_renewal_never_extends_past_the_absolute_lifetime() {
        // Issue #12: a device used at least hourly used to stay signed in forever.
        let dir = tempfile::tempdir().unwrap();
        let store = Sessions::open(dir.path()).unwrap();
        store
            .create("tok", "csrf", "Phone", SESSION_TTL_MS)
            .unwrap();
        let day: i64 = 24 * 60 * 60 * 1000;

        // Paired 85 days ago, last seen 2 hours ago: renewal is capped at day 90.
        let created = now() as i64 - 85 * day;
        store
            .conn
            .execute(
                "UPDATE paired_sessions SET created_at=?1, last_seen_at=?2",
                params![created, now() as i64 - 2 * 60 * 60 * 1000],
            )
            .unwrap();
        let verified = store.verify("tok").unwrap().expect("still inside the cap");
        assert_eq!(
            verified.expires_at,
            created as u64 + SESSION_MAX_LIFETIME_MS
        );

        // Paired 91 days ago: rejected even though a legacy row still claims a later expiry.
        store
            .conn
            .execute(
                "UPDATE paired_sessions SET created_at=?1, expires_at=?2",
                params![now() as i64 - 91 * day, now() as i64 + 10 * day],
            )
            .unwrap();
        assert!(store.verify("tok").unwrap().is_none());
        assert!(store.list_active().unwrap().is_empty());
    }

    #[test]
    fn expired_sessions_are_rejected_and_revoke_all_clears_multiple() {
        let dir = tempfile::tempdir().unwrap();
        let store = Sessions::open(dir.path()).unwrap();
        store.create("expired-tok", "csrf", "Mike", 0).unwrap();
        assert!(store.verify("expired-tok").unwrap().is_none());

        store
            .create("tok-1", "csrf-1", "Phone", SESSION_TTL_MS)
            .unwrap();
        store
            .create("tok-2", "csrf-2", "Tablet", SESSION_TTL_MS)
            .unwrap();
        assert_eq!(store.list_active().unwrap().len(), 2);
        assert_eq!(store.revoke_all().unwrap(), 2);
        assert!(store.list_active().unwrap().is_empty());
        assert!(store.verify("tok-1").unwrap().is_none());
        assert!(store.verify("tok-2").unwrap().is_none());
    }
}
