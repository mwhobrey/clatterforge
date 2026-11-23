//! M11 Phase 2: delegated authority for externally initiated MCP workflows.
//!
//! A `Grant` is a human-authored, explicit, revocable, expiring, usage-bounded
//! pre-authorization letting one named external principal submit/approve/resume/
//! reject/cancel tasks against one repository identity and one set of backends
//! without an interactive human click for each one. It is the MCP-driven analog of
//! `autonomy::Policy`'s standing policy, generalized from a fixed pre-declared task
//! list to an open-ended but bounded *scope* (repository + backend + operation set) -
//! an external orchestrator submits real objectives at call time (e.g. from a Jira
//! issue), not ones a human pre-wrote, so the M7 "finite, pre-declared list" shape
//! doesn't fit; a bounded scope does.
//!
//! **Grants are never created, modified, or even listed through the MCP adapter
//! itself** - only through the CLI (`grant-create`/`grant-list`/`grant-show`/
//! `grant-revoke`), mirroring `Policy`'s own CLI-only precedent. Exposing grant
//! management over MCP would let an external caller authorize itself, which is
//! exactly the privilege escalation the milestone's "the developer or organization
//! defines the permissions granted to external orchestrators... retains the ability
//! to revoke authority" principle rules out. `src/mcp.rs` only ever *reads* grants to
//! decide whether an operation can skip an interactive step; nothing in that module
//! can create or widen one.
//!
//! **Grants do not widen what the underlying engine allows.** They only decide
//! whether an operation the engine would otherwise require an explicit human/caller
//! decision for can instead proceed with `by = "grant:{id}@v{version}"`, exactly like
//! `Policy`'s own `policy:{id}@v{version}` convention - a real, inspectable label, not
//! a bare `"user_approved": true` the plan explicitly says not to trust. Independent
//! verification, global stop, repository leases and dependency gating are completely
//! unchanged and still run for every grant-authorized task; a grant only ever removes
//! the need for a separate approve/resume/review call, never the safety checks inside
//! them.
//!
//! Scoping key: a grant is matched against a repository's `Repository::identity`
//! (the same stable string `Coordination`'s lease system already keys on), not a
//! `Store` registration id - `board::Detail` only ever exposes a checkout path, so
//! every lookup after creation re-inspects the repository fresh via
//! `Repository::inspect`, the same pattern every other module already uses rather
//! than trusting a cached value.

use crate::{now, open_sqlite_with_retry};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::path::Path;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Submit,
    Approve,
    Reject,
    Resume,
    Cancel,
}

impl Operation {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Submit => "submit",
            Self::Approve => "approve",
            Self::Reject => "reject",
            Self::Resume => "resume",
            Self::Cancel => "cancel",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    /// Operations covered by this grant proceed automatically; anything out of scope
    /// still needs an explicit interactive call.
    Delegated,
    /// Same runtime behavior as `Delegated` - the distinction is how broad a scope a
    /// human was willing to pre-configure, not a different code path. Named
    /// separately because the CLI requires a louder, explicit confirmation to create
    /// one (see `grant-create --hold-my-beer`), matching this project's existing
    /// Hold My Beer naming and ceremony for broad unattended authority.
    HoldMyBeer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantState {
    Active,
    Revoked,
    Expired,
    Exhausted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Grant {
    pub id: String,
    pub version: u64,
    /// The external caller this grant authorizes - matched exactly against the MCP
    /// tool call's own `principal` field. Not yet a cryptographically verified
    /// identity (same limitation `src/mcp.rs` already states for Phase 1); this is
    /// still a real, local, human-issued boundary, not a client-asserted one - only a
    /// human with CLI access can create the mapping from this string to real
    /// authority in the first place.
    pub principal: String,
    pub repository_identity: String,
    pub backends: Vec<String>,
    pub operations: Vec<Operation>,
    pub mode: ExecutionMode,
    pub created_by: String,
    pub created_at: u64,
    pub expires_at: u64,
    pub max_tasks: usize,
    /// Count of distinct tasks this grant has authorized the *creation* of (the
    /// `Submit` operation only - approving/resuming/reviewing an already-counted
    /// task does not consume another slot). Mirrors `Policy`'s own `max_tasks`
    /// semantics: a bound on how much new work this grant can start, not on how many
    /// lifecycle calls happen against work it already started.
    pub dispatched: u64,
    pub revoked: bool,
    pub revoked_by: Option<String>,
    pub revoked_reason: Option<String>,
}

impl Grant {
    /// Computed fresh from the record and the wall clock, not stored - identical
    /// reasoning to `autonomy::Policy::state`: nothing here runs a background
    /// process, so there is nothing to keep a stored status in sync with.
    pub fn state(&self, now: u64) -> GrantState {
        if self.revoked {
            return GrantState::Revoked;
        }
        if now >= self.expires_at {
            return GrantState::Expired;
        }
        if self.dispatched >= self.max_tasks as u64 {
            return GrantState::Exhausted;
        }
        GrantState::Active
    }

    pub fn covers(
        &self,
        now: u64,
        principal: &str,
        repository_identity: &str,
        backend: &str,
        operation: Operation,
    ) -> bool {
        self.state(now) == GrantState::Active
            && self.principal == principal
            && self.repository_identity == repository_identity
            && self.backends.iter().any(|b| b == backend)
            && self.operations.contains(&operation)
    }

    pub fn label(&self) -> String {
        format!("grant:{}@v{}", self.id, self.version)
    }
}

pub struct Grants {
    connection: Connection,
}

impl Grants {
    pub fn open(root: &Path) -> Result<Self> {
        let connection = open_sqlite_with_retry(&root.join("state.sqlite"), 1)
            .context("open authorization state")?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS authorization_grants(
                id TEXT PRIMARY KEY,
                version INTEGER NOT NULL,
                record TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS authorization_decisions(
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                at INTEGER NOT NULL,
                grant_id TEXT NOT NULL,
                grant_version INTEGER NOT NULL,
                principal TEXT NOT NULL,
                operation TEXT NOT NULL,
                task_id TEXT NOT NULL,
                task_revision INTEGER NOT NULL,
                evidence_digest TEXT,
                request_id TEXT
            );",
        )?;
        Ok(Self { connection })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create(
        &mut self,
        principal: &str,
        repository_identity: &str,
        backends: Vec<String>,
        operations: Vec<Operation>,
        mode: ExecutionMode,
        created_by: &str,
        max_tasks: usize,
        expires_at: u64,
    ) -> Result<Grant> {
        ensure!(
            !principal.trim().is_empty(),
            "a grant needs the external principal it authorizes"
        );
        ensure!(
            !repository_identity.trim().is_empty(),
            "a grant needs a repository identity to scope it to"
        );
        ensure!(!backends.is_empty(), "a grant needs at least one backend");
        ensure!(
            !operations.is_empty(),
            "a grant needs at least one operation - it authorizes a bounded scope, not open-ended authority"
        );
        ensure!(max_tasks > 0, "max_tasks must be positive");
        ensure!(expires_at > now(), "expires_at must be in the future");
        ensure!(
            !created_by.trim().is_empty(),
            "created_by is required - a standing grant needs a human accountable for turning it on"
        );
        let grant = Grant {
            id: Uuid::new_v4().to_string(),
            version: 1,
            principal: principal.to_string(),
            repository_identity: repository_identity.to_string(),
            backends,
            operations,
            mode,
            created_by: created_by.to_string(),
            created_at: now(),
            expires_at,
            max_tasks,
            dispatched: 0,
            revoked: false,
            revoked_by: None,
            revoked_reason: None,
        };
        self.connection.execute(
            "INSERT INTO authorization_grants VALUES (?1, ?2, ?3)",
            params![grant.id, grant.version, serde_json::to_string(&grant)?],
        )?;
        Ok(grant)
    }

    pub fn get(&self, id: &str) -> Result<Grant> {
        let record: String = self
            .connection
            .query_row(
                "SELECT record FROM authorization_grants WHERE id=?1",
                [id],
                |r| r.get(0),
            )
            .context("unknown grant id")?;
        serde_json::from_str(&record).context("corrupt grant record")
    }

    pub fn list(&self) -> Result<Vec<Grant>> {
        let mut statement = self
            .connection
            .prepare("SELECT record FROM authorization_grants ORDER BY id")?;
        let rows = statement.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(serde_json::from_str(&row?)?);
        }
        Ok(out)
    }

    fn mutate(&mut self, id: &str, change: impl FnOnce(&mut Grant) -> Result<()>) -> Result<Grant> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let record: String = tx
            .query_row(
                "SELECT record FROM authorization_grants WHERE id=?1",
                [id],
                |r| r.get(0),
            )
            .context("unknown grant id")?;
        let mut grant: Grant = serde_json::from_str(&record)?;
        change(&mut grant)?;
        grant.version += 1;
        let record = serde_json::to_string(&grant)?;
        tx.execute(
            "UPDATE authorization_grants SET version=?1, record=?2 WHERE id=?3",
            params![grant.version, record, id],
        )?;
        tx.commit()?;
        Ok(grant)
    }

    pub fn revoke(&mut self, id: &str, by: &str, reason: &str) -> Result<Grant> {
        ensure!(
            !by.trim().is_empty() && !reason.trim().is_empty(),
            "revoking a grant needs an explicit by/reason"
        );
        self.mutate(id, |g| {
            ensure!(!g.revoked, "grant already revoked");
            g.revoked = true;
            g.revoked_by = Some(by.to_string());
            g.revoked_reason = Some(reason.to_string());
            Ok(())
        })
    }

    /// The one read `src/mcp.rs` actually calls at every decision point. Returns the
    /// first active grant covering this exact (principal, repository, backend,
    /// operation), or `None` - never partial credit for a near match, so a caller
    /// can't accidentally run under broader authority than a human actually granted.
    pub fn find_covering(
        &self,
        principal: &str,
        repository_identity: &str,
        backend: &str,
        operation: Operation,
    ) -> Result<Option<Grant>> {
        let at = now();
        Ok(self
            .list()?
            .into_iter()
            .find(|g| g.covers(at, principal, repository_identity, backend, operation)))
    }

    /// Exhaustion limits new admissions, not the remaining authorized stages of a
    /// task admitted by this same grant. Expiry and revocation still stop coverage.
    pub fn find_covering_task(
        &self,
        principal: &str,
        repository_identity: &str,
        backend: &str,
        operation: Operation,
        task_id: &str,
    ) -> Result<Option<Grant>> {
        let at = now();
        for grant in self.list()? {
            if grant.covers(at, principal, repository_identity, backend, operation) {
                return Ok(Some(grant));
            }
            if operation != Operation::Submit
                && grant.state(at) == GrantState::Exhausted
                && grant.principal == principal
                && grant.repository_identity == repository_identity
                && grant.backends.iter().any(|b| b == backend)
                && grant.operations.contains(&operation)
                && self.connection.query_row(
                    "SELECT EXISTS(SELECT 1 FROM authorization_decisions
                     WHERE grant_id=?1 AND task_id=?2 AND operation='submit')",
                    params![grant.id, task_id],
                    |row| row.get::<_, bool>(0),
                )?
            {
                return Ok(Some(grant));
            }
        }
        Ok(None)
    }

    /// Reserve the last slot before a fallible engine approval, so concurrent
    /// requests cannot both auto-authorize against the same capacity snapshot.
    pub(crate) fn reserve_submit(
        &mut self,
        id: &str,
        task_id: &str,
        revision: u64,
        request_id: &str,
    ) -> Result<Grant> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let record: String = tx.query_row(
            "SELECT record FROM authorization_grants WHERE id=?1",
            [id],
            |row| row.get(0),
        )?;
        let mut grant: Grant = serde_json::from_str(&record)?;
        ensure!(
            grant.state(now()) == GrantState::Active
                && grant.operations.contains(&Operation::Submit),
            "grant has no available admission slots"
        );
        grant.dispatched += 1;
        grant.version += 1;
        tx.execute(
            "UPDATE authorization_grants SET version=?1,record=?2 WHERE id=?3",
            params![grant.version, serde_json::to_string(&grant)?, id],
        )?;
        tx.execute("INSERT INTO authorization_decisions(at,grant_id,grant_version,principal,operation,task_id,task_revision,request_id) VALUES(?1,?2,?3,?4,'submit_pending',?5,?6,?7)",params![now(),grant.id,grant.version,grant.principal,task_id,revision,request_id])?;
        tx.commit()?;
        Ok(grant)
    }
    pub(crate) fn release_submit(
        &mut self,
        id: &str,
        task_id: &str,
        request_id: &str,
    ) -> Result<()> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed=tx.execute("UPDATE authorization_decisions SET operation='submit_failed' WHERE grant_id=?1 AND task_id=?2 AND request_id=?3 AND operation='submit_pending'",params![id,task_id,request_id])?;
        if changed > 0 {
            let record: String = tx.query_row(
                "SELECT record FROM authorization_grants WHERE id=?1",
                [id],
                |row| row.get(0),
            )?;
            let mut grant: Grant = serde_json::from_str(&record)?;
            grant.dispatched = grant
                .dispatched
                .checked_sub(changed as u64)
                .context("invalid admission accounting")?;
            grant.version += 1;
            tx.execute(
                "UPDATE authorization_grants SET version=?1,record=?2 WHERE id=?3",
                params![grant.version, serde_json::to_string(&grant)?, id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Records that `grant` authorized `operation` against `task_id`, and - only for
    /// `Operation::Submit` - consumes one of its `max_tasks` slots. Called once,
    /// immediately after the underlying engine call it authorized actually succeeded
    /// (mirroring `autonomy::Policies::mark_dispatched`'s "record success, don't
    /// pre-charge against a call that might still fail" ordering).
    pub fn record_use(
        &mut self,
        id: &str,
        operation: Operation,
        task_id: &str,
        task_revision: u64,
        evidence_digest: Option<&str>,
        request_id: Option<&str>,
    ) -> Result<Grant> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let record: String = tx.query_row(
            "SELECT record FROM authorization_grants WHERE id=?1",
            [id],
            |row| row.get(0),
        )?;
        let mut grant: Grant = serde_json::from_str(&record)?;
        let pending:Option<i64>=tx.query_row("SELECT id FROM authorization_decisions WHERE grant_id=?1 AND task_id=?2 AND operation='submit_pending' AND request_id IS ?3",params![id,task_id,request_id],|row|row.get(0)).optional()?;
        if operation == Operation::Submit && pending.is_none() {
            ensure!(
                grant.state(now()) == GrantState::Active,
                "grant has no available admission slots"
            );
            grant.dispatched += 1;
            grant.version += 1;
            tx.execute(
                "UPDATE authorization_grants SET version=?1,record=?2 WHERE id=?3",
                params![grant.version, serde_json::to_string(&grant)?, id],
            )?;
        }
        if operation == Operation::Submit
            && let Some(pending) = pending
        {
            tx.execute("UPDATE authorization_decisions SET operation='submit',task_revision=?1,evidence_digest=?2 WHERE id=?3",params![task_revision,evidence_digest,pending])?;
        } else {
            tx.execute("INSERT INTO authorization_decisions(at,grant_id,grant_version,principal,operation,task_id,task_revision,evidence_digest,request_id) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",params![now(),grant.id,grant.version,grant.principal,operation.as_str(),task_id,task_revision,evidence_digest,request_id])?;
        }
        tx.commit()?;
        Ok(grant)
    }

    /// Full provenance trail for one task: every grant-authorized decision made
    /// against it, oldest first. Audit-only; nothing here feeds back into dispatch.
    pub fn decisions_for(&self, task_id: &str) -> Result<Vec<serde_json::Value>> {
        let mut statement = self.connection.prepare(
            "SELECT at, grant_id, grant_version, principal, operation, task_revision, evidence_digest, request_id
             FROM authorization_decisions WHERE task_id=?1 ORDER BY id",
        )?;
        let rows = statement.query_map([task_id], |r| {
            Ok(serde_json::json!({
                "at": r.get::<_, i64>(0)?,
                "grant_id": r.get::<_, String>(1)?,
                "grant_version": r.get::<_, i64>(2)?,
                "principal": r.get::<_, String>(3)?,
                "operation": r.get::<_, String>(4)?,
                "task_revision": r.get::<_, i64>(5)?,
                "evidence_digest": r.get::<_, Option<String>>(6)?,
                "request_id": r.get::<_, Option<String>>(7)?,
            }))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn grants() -> (TempDir, Grants) {
        let dir = tempfile::tempdir().unwrap();
        let grants = Grants::open(dir.path()).unwrap();
        (dir, grants)
    }

    #[test]
    fn admission_reservation_serializes_the_last_slot_and_failed_approval_releases_it() {
        let (dir, mut grants) = grants();
        let mut grant = create_default(&mut grants);
        grant.max_tasks = 1;
        grants
            .connection
            .execute(
                "UPDATE authorization_grants SET record=?1 WHERE id=?2",
                params![serde_json::to_string(&grant).unwrap(), grant.id],
            )
            .unwrap();
        grants
            .reserve_submit(&grant.id, "failed", 1, "failure-request")
            .unwrap();
        assert!(
            grants
                .find_covering_task(
                    "grok-bot",
                    "local:/repo",
                    "fake-process-v1",
                    Operation::Resume,
                    "failed"
                )
                .unwrap()
                .is_none()
        );
        grants
            .release_submit(&grant.id, "failed", "failure-request")
            .unwrap();
        assert_eq!(grants.get(&grant.id).unwrap().dispatched, 0);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = (0..2)
            .map(|n| {
                let path = dir.path().to_path_buf();
                let id = grant.id.clone();
                let gate = barrier.clone();
                std::thread::spawn(move || {
                    let mut g = Grants::open(&path).unwrap();
                    gate.wait();
                    let task = format!("task-{n}");
                    (task.clone(), g.reserve_submit(&id, &task, 1, &task).is_ok())
                })
            })
            .collect();
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(results.iter().filter(|(_, ok)| *ok).count(), 1);
        let winner = &results.iter().find(|(_, ok)| *ok).unwrap().0;
        grants
            .record_use(&grant.id, Operation::Submit, winner, 2, None, Some(winner))
            .unwrap();
        assert_eq!(grants.get(&grant.id).unwrap().dispatched, 1);
        assert!(
            grants
                .find_covering_task(
                    "grok-bot",
                    "local:/repo",
                    "fake-process-v1",
                    Operation::Resume,
                    winner
                )
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn exhausted_grant_only_covers_its_own_admitted_task_and_still_expires_or_revokes() {
        let (_dir, mut grants) = grants();
        let mut grant = create_default(&mut grants);
        grant.max_tasks = 1;
        grants
            .connection
            .execute(
                "UPDATE authorization_grants SET record=?1 WHERE id=?2",
                params![serde_json::to_string(&grant).unwrap(), grant.id],
            )
            .unwrap();
        grants
            .record_use(&grant.id, Operation::Submit, "admitted", 2, None, None)
            .unwrap();
        let covered = |g: &Grants, who: &str, repo: &str, backend: &str, op, task| {
            g.find_covering_task(who, repo, backend, op, task)
                .unwrap()
                .is_some()
        };
        assert!(covered(
            &grants,
            "grok-bot",
            "local:/repo",
            "fake-process-v1",
            Operation::Resume,
            "admitted"
        ));
        assert!(!covered(
            &grants,
            "grok-bot",
            "local:/repo",
            "fake-process-v1",
            Operation::Submit,
            "admitted"
        ));
        assert!(!covered(
            &grants,
            "grok-bot",
            "local:/repo",
            "fake-process-v1",
            Operation::Resume,
            "other"
        ));
        assert!(!covered(
            &grants,
            "other",
            "local:/repo",
            "fake-process-v1",
            Operation::Resume,
            "admitted"
        ));
        assert!(!covered(
            &grants,
            "grok-bot",
            "local:/other",
            "fake-process-v1",
            Operation::Resume,
            "admitted"
        ));
        assert!(!covered(
            &grants,
            "grok-bot",
            "local:/repo",
            "cursor-agent",
            Operation::Resume,
            "admitted"
        ));
        assert!(!covered(
            &grants,
            "grok-bot",
            "local:/repo",
            "fake-process-v1",
            Operation::Cancel,
            "admitted"
        ));
        grants
            .mutate(&grant.id, |g| {
                g.expires_at = now();
                Ok(())
            })
            .unwrap();
        assert!(!covered(
            &grants,
            "grok-bot",
            "local:/repo",
            "fake-process-v1",
            Operation::Resume,
            "admitted"
        ));
        grants
            .mutate(&grant.id, |g| {
                g.expires_at = now() + 60_000;
                Ok(())
            })
            .unwrap();
        grants.revoke(&grant.id, "test", "stop").unwrap();
        assert!(!covered(
            &grants,
            "grok-bot",
            "local:/repo",
            "fake-process-v1",
            Operation::Resume,
            "admitted"
        ));
    }

    fn create_default(grants: &mut Grants) -> Grant {
        grants
            .create(
                "grok-bot",
                "local:/repo",
                vec!["fake-process-v1".into()],
                vec![Operation::Submit, Operation::Approve, Operation::Resume],
                ExecutionMode::Delegated,
                "mike",
                5,
                now() + 60_000,
            )
            .unwrap()
    }

    #[test]
    fn a_fresh_grant_covers_exactly_what_it_lists_and_nothing_else() {
        let (_dir, mut grants) = grants();
        let grant = create_default(&mut grants);
        let at = now();
        assert!(grant.covers(
            at,
            "grok-bot",
            "local:/repo",
            "fake-process-v1",
            Operation::Submit
        ));
        assert!(
            !grant.covers(
                at,
                "someone-else",
                "local:/repo",
                "fake-process-v1",
                Operation::Submit
            ),
            "wrong principal"
        );
        assert!(
            !grant.covers(
                at,
                "grok-bot",
                "local:/other-repo",
                "fake-process-v1",
                Operation::Submit
            ),
            "wrong repository"
        );
        assert!(
            !grant.covers(
                at,
                "grok-bot",
                "local:/repo",
                "codex-exec-v1",
                Operation::Submit
            ),
            "wrong backend"
        );
        assert!(
            !grant.covers(
                at,
                "grok-bot",
                "local:/repo",
                "fake-process-v1",
                Operation::Cancel
            ),
            "wrong operation"
        );
    }

    #[test]
    fn expired_and_revoked_grants_stop_covering_anything() {
        let (_dir, mut grants) = grants();
        let expiring = grants
            .create(
                "grok-bot",
                "local:/repo",
                vec!["fake-process-v1".into()],
                vec![Operation::Submit],
                ExecutionMode::Delegated,
                "mike",
                5,
                now() + 60_000,
            )
            .unwrap();
        assert_eq!(expiring.state(expiring.expires_at - 1), GrantState::Active);
        assert_eq!(expiring.state(expiring.expires_at), GrantState::Expired);

        let created = create_default(&mut grants);
        let revoked = grants
            .revoke(&created.id, "mike", "no longer needed")
            .unwrap();
        assert_eq!(revoked.state(now()), GrantState::Revoked);
        assert!(!revoked.covers(
            now(),
            "grok-bot",
            "local:/repo",
            "fake-process-v1",
            Operation::Submit
        ));
    }

    #[test]
    fn max_tasks_bounds_submit_but_not_other_operations() {
        let (_dir, mut grants) = grants();
        let grant = grants
            .create(
                "grok-bot",
                "local:/repo",
                vec!["fake-process-v1".into()],
                vec![Operation::Submit, Operation::Approve],
                ExecutionMode::Delegated,
                "mike",
                1,
                now() + 60_000,
            )
            .unwrap();
        let after_submit = grants
            .record_use(&grant.id, Operation::Submit, "task-1", 1, None, None)
            .unwrap();
        assert_eq!(after_submit.state(now()), GrantState::Exhausted);
        assert!(!after_submit.covers(
            now(),
            "grok-bot",
            "local:/repo",
            "fake-process-v1",
            Operation::Submit
        ));

        // A non-Submit operation against an already-counted task does not consume a
        // second slot and does not fail just because the grant is exhausted for new
        // submissions - exhaustion only ever blocks starting new work.
        let after_approve = grants
            .record_use(
                &grant.id,
                Operation::Approve,
                "task-1",
                2,
                Some("digest"),
                Some("req-1"),
            )
            .unwrap();
        assert_eq!(after_approve.dispatched, 1);

        let decisions = grants.decisions_for("task-1").unwrap();
        assert_eq!(decisions.len(), 2);
        assert_eq!(decisions[0]["operation"], "submit");
        assert_eq!(decisions[1]["operation"], "approve");
    }

    #[test]
    fn find_covering_never_returns_a_near_match() {
        let (_dir, mut grants) = grants();
        create_default(&mut grants);
        assert!(
            grants
                .find_covering(
                    "grok-bot",
                    "local:/repo",
                    "fake-process-v1",
                    Operation::Submit
                )
                .unwrap()
                .is_some()
        );
        assert!(
            grants
                .find_covering(
                    "grok-bot",
                    "local:/repo",
                    "fake-process-v1",
                    Operation::Cancel
                )
                .unwrap()
                .is_none()
        );
        assert!(
            grants
                .find_covering(
                    "grok-bot",
                    "local:/repo",
                    "codex-exec-v1",
                    Operation::Submit
                )
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn revoking_a_grant_requires_a_reason() {
        let (_dir, mut grants) = grants();
        let grant = create_default(&mut grants);
        assert!(grants.revoke(&grant.id, "mike", "").is_err());
        assert!(grants.revoke(&grant.id, "", "reason").is_err());
    }
}
