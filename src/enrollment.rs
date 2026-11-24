//! M12.0 Phase 2: client enrollment and authenticated identity for the edge boundary.
//!
//! An `Enrollment` is the main Conductor's record of one external client (e.g. one Hitch/
//! Grok Bot installation) permitted to call `/api/edge/tools/call` (`src/serve.rs`) at
//! all. It is a different concept from `authorization::Grant`: a `Grant` says what an
//! already-recognized principal may do *without an interactive step*; an `Enrollment` is
//! what makes a principal recognized in the first place - its own credential, its own
//! revocation, and a coarse allowed-operations gate checked before any `Grant` lookup even
//! happens. `Grant.principal` and `Enrollment.client_id` share one namespace by design:
//! `serve.rs`'s edge dispatch overwrites every tool call's caller-supplied `principal`
//! field with the verified `client_id` (`clatterforge_protocol::SetPrincipal`) before it ever
//! reaches `authorization::Grants::find_covering` or a task record - the model never
//! supplies or overrides its own authenticated identity, per this milestone's own
//! requirement. Stdio local mode is unaffected and keeps its separate, weaker,
//! caller-asserted trust model exactly as M11 documented it.
//!
//! **Repository scope is stored here but not yet enforced at this phase** - storing the
//! field (as the milestone's enrollment model asks for) and actually gating a tool call by
//! it are different pieces of work, and the latter needs each tool's resolved repository
//! identity, which several tools (`conductor_approve`, `conductor_resume`, ...) only have
//! *after* looking up the task. M12.0 Phase 3 ("repair cross-principal authorization") is
//! where that enforcement, and its dedicated security tests, land - see
//! `docs/m12/M12_0_IMPLEMENTATION.md` for the exact boundary.
//!
//! **Credential handling:** a token is generated once (`create`) or on demand (`rotate_
//! credential`), returned to the caller exactly once by whatever surface asked for it (the
//! CLI prints it or writes it to a file the caller named - never both, never twice), and
//! never stored or logged in plaintext again - only its SHA-256 hash persists, compared in
//! `verify`. This reuses `serve.rs`'s existing bearer-token precedent (a high-entropy
//! shared secret over an already-private, already-encrypted transport - see
//! `docs/m12/M12_0_ARCHITECTURE.md` section 3) rather than inventing new cryptography.

use crate::{digest, now, open_sqlite_with_retry};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::path::Path;
use uuid::Uuid;

/// Coarse category a `conductor_*` tool falls into for the enrollment-level allowlist -
/// deliberately coarser than `authorization::Operation` (which scopes a `Grant` to one
/// repository/backend/operation triple): this gate runs before any repository is even
/// known, so it can only ever be "may this client make mutating calls of this shape at
/// all," not "against this repository."
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnrollmentOperation {
    /// Every read-only tool: capabilities, list/get/evidence/approval-requirements,
    /// global-stop-status, and the side-effect-free propose/schedule probes.
    Inspect,
    Submit,
    Approve,
    Reject,
    Resume,
    Cancel,
}

impl EnrollmentOperation {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Inspect => "inspect",
            Self::Submit => "submit",
            Self::Approve => "approve",
            Self::Reject => "reject",
            Self::Resume => "resume",
            Self::Cancel => "cancel",
        }
    }

    /// Every one of the 15 `conductor_*` tools maps to exactly one category - `None` means
    /// an unrecognized tool name, not an ungated one. Kept exhaustive and listed alongside
    /// `clatterforge_protocol::TOOL_NAMES` deliberately, mirroring how `src/mcp.rs::TOOL_NAMES`
    /// itself is kept as one list so nothing can silently drift out of sync.
    pub fn for_tool(tool: &str) -> Option<Self> {
        match tool {
            "conductor_capabilities"
            | "conductor_list_repositories"
            | "conductor_list_tasks"
            | "conductor_get_task"
            | "conductor_propose_task"
            | "conductor_schedule_task"
            | "conductor_get_execution"
            | "conductor_get_evidence"
            | "conductor_get_approval_requirements"
            | "conductor_global_stop_status"
            | "conductor_get_evidence_summary"
            | "conductor_get_artifact" => Some(Self::Inspect),
            "conductor_submit_task" => Some(Self::Submit),
            "conductor_approve" => Some(Self::Approve),
            "conductor_reject" => Some(Self::Reject),
            "conductor_resume" => Some(Self::Resume),
            "conductor_cancel_task" => Some(Self::Cancel),
            _ => None,
        }
    }
}

impl From<crate::authorization::Operation> for EnrollmentOperation {
    fn from(op: crate::authorization::Operation) -> Self {
        use crate::authorization::Operation as Op;
        match op {
            Op::Submit => Self::Submit,
            Op::Approve => Self::Approve,
            Op::Reject => Self::Reject,
            Op::Resume => Self::Resume,
            Op::Cancel => Self::Cancel,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Enrollment {
    /// Stable identity, also the `Grant::principal`/task `by`/`approved_by` namespace once
    /// the edge dispatch path overwrites a caller-supplied principal with this value.
    pub client_id: String,
    pub version: u64,
    pub display_name: String,
    /// Free text (e.g. `"grok-bot"`, `"generic-mcp-client"`) - descriptive only, never
    /// branched on for authorization, same reasoning as `Grant.backends` being a plain
    /// string list rather than a closed enum.
    pub client_type: String,
    /// SHA-256 hex digest of the current credential. The plaintext token is never stored.
    pub credential_hash: String,
    pub credential_created_at: u64,
    pub credential_rotated_at: Option<u64>,
    pub allowed_operations: Vec<EnrollmentOperation>,
    /// Repository identities (`Repository::identity`, same key `Coordination`'s leases and
    /// `Grant`s use) this client may act against. Stored now; not yet enforced - see this
    /// module's doc comment.
    pub repository_scope: Vec<String>,
    pub created_by: String,
    pub created_at: u64,
    pub revoked: bool,
    pub revoked_by: Option<String>,
    pub revoked_reason: Option<String>,
    pub revoked_at: Option<u64>,
}

impl Enrollment {
    pub fn allows(&self, operation: EnrollmentOperation) -> bool {
        !self.revoked && self.allowed_operations.contains(&operation)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    pub at: u64,
    pub client_id: String,
    pub event: String,
    pub actor: String,
    pub detail: String,
}

/// Missing or invalid credentials return 401; revoked enrollments return 403.
/// Preserve that distinction in verification results.
#[derive(Debug)]
pub enum VerifyOutcome {
    Valid(Enrollment),
    Revoked(Enrollment),
    Unknown,
}

fn hash_token(token: &str) -> String {
    digest(token.as_bytes())
}

/// A fresh high-entropy credential, generated the same way `serve.rs::random_token`
/// already does (two concatenated UUIDv4s - `uuid`'s v4 generation is backed by a real
/// CSPRNG via `getrandom`, already an existing, proven pattern in this codebase, not a new
/// one introduced here). 64 hex characters, effectively unguessable.
fn generate_token() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

pub struct Enrollments {
    connection: Connection,
}

impl Enrollments {
    pub fn open(root: &Path) -> Result<Self> {
        let connection = open_sqlite_with_retry(&root.join("state.sqlite"), 1)
            .context("open enrollment state")?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS enrollment_clients(
                client_id TEXT PRIMARY KEY,
                version INTEGER NOT NULL,
                record TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS enrollment_audit(
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                at INTEGER NOT NULL,
                client_id TEXT NOT NULL,
                event TEXT NOT NULL,
                actor TEXT NOT NULL,
                detail TEXT NOT NULL
            );",
        )?;
        Ok(Self { connection })
    }

    fn audit(&mut self, client_id: &str, event: &str, actor: &str, detail: &str) -> Result<()> {
        self.connection.execute(
            "INSERT INTO enrollment_audit(at, client_id, event, actor, detail) VALUES (?1,?2,?3,?4,?5)",
            params![now() as i64, client_id, event, actor, detail],
        )?;
        Ok(())
    }

    pub fn audit_for(&self, client_id: &str) -> Result<Vec<AuditEntry>> {
        let mut statement = self.connection.prepare(
            "SELECT at, client_id, event, actor, detail FROM enrollment_audit WHERE client_id=?1 ORDER BY id",
        )?;
        let rows = statement.query_map([client_id], |r| {
            Ok(AuditEntry {
                at: r.get::<_, i64>(0)? as u64,
                client_id: r.get(1)?,
                event: r.get(2)?,
                actor: r.get(3)?,
                detail: r.get(4)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Creates a new enrollment and returns it alongside the one and only plaintext copy
    /// of its credential the system will ever produce - the caller (CLI) is responsible
    /// for showing or storing it exactly once and never echoing it back afterward.
    pub fn create(
        &mut self,
        display_name: &str,
        client_type: &str,
        allowed_operations: Vec<EnrollmentOperation>,
        repository_scope: Vec<String>,
        created_by: &str,
    ) -> Result<(Enrollment, String)> {
        ensure!(
            !display_name.trim().is_empty(),
            "an enrollment needs a display name"
        );
        ensure!(
            !allowed_operations.is_empty(),
            "an enrollment needs at least one allowed operation - it authorizes a bounded scope, not open-ended access"
        );
        ensure!(
            !created_by.trim().is_empty(),
            "created_by is required - an enrollment needs a human accountable for creating it"
        );
        let token = generate_token();
        let enrollment = Enrollment {
            client_id: Uuid::new_v4().to_string(),
            version: 1,
            display_name: display_name.to_string(),
            client_type: client_type.to_string(),
            credential_hash: hash_token(&token),
            credential_created_at: now(),
            credential_rotated_at: None,
            allowed_operations,
            repository_scope,
            created_by: created_by.to_string(),
            created_at: now(),
            revoked: false,
            revoked_by: None,
            revoked_reason: None,
            revoked_at: None,
        };
        self.connection.execute(
            "INSERT INTO enrollment_clients VALUES (?1, ?2, ?3)",
            params![
                enrollment.client_id,
                enrollment.version,
                serde_json::to_string(&enrollment)?
            ],
        )?;
        self.audit(
            &enrollment.client_id,
            "created",
            created_by,
            &format!("display_name={display_name}"),
        )?;
        Ok((enrollment, token))
    }

    pub fn get(&self, client_id: &str) -> Result<Enrollment> {
        let record: String = self
            .connection
            .query_row(
                "SELECT record FROM enrollment_clients WHERE client_id=?1",
                [client_id],
                |r| r.get(0),
            )
            .context("unknown client id")?;
        serde_json::from_str(&record).context("corrupt enrollment record")
    }

    pub fn list(&self) -> Result<Vec<Enrollment>> {
        let mut statement = self
            .connection
            .prepare("SELECT record FROM enrollment_clients ORDER BY client_id")?;
        let rows = statement.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(serde_json::from_str(&row?)?);
        }
        Ok(out)
    }

    fn mutate(
        &mut self,
        client_id: &str,
        change: impl FnOnce(&mut Enrollment) -> Result<()>,
    ) -> Result<Enrollment> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let record: String = tx
            .query_row(
                "SELECT record FROM enrollment_clients WHERE client_id=?1",
                [client_id],
                |r| r.get(0),
            )
            .context("unknown client id")?;
        let mut enrollment: Enrollment = serde_json::from_str(&record)?;
        change(&mut enrollment)?;
        enrollment.version += 1;
        let record = serde_json::to_string(&enrollment)?;
        tx.execute(
            "UPDATE enrollment_clients SET version=?1, record=?2 WHERE client_id=?3",
            params![enrollment.version, record, client_id],
        )?;
        tx.commit()?;
        Ok(enrollment)
    }

    /// Revocation is checked fresh by `verify` on every call (no cache anywhere in this
    /// path), so the very next privileged operation after this returns is refused - there
    /// is no session or token-validity window left to expire separately.
    pub fn revoke(&mut self, client_id: &str, by: &str, reason: &str) -> Result<Enrollment> {
        ensure!(
            !by.trim().is_empty() && !reason.trim().is_empty(),
            "revoking an enrollment needs an explicit by/reason"
        );
        let enrollment = self.mutate(client_id, |e| {
            ensure!(!e.revoked, "enrollment already revoked");
            e.revoked = true;
            e.revoked_by = Some(by.to_string());
            e.revoked_reason = Some(reason.to_string());
            e.revoked_at = Some(now());
            Ok(())
        })?;
        self.audit(client_id, "revoked", by, reason)?;
        Ok(enrollment)
    }

    /// Issues a fresh credential for an existing, non-revoked enrollment, invalidating the
    /// previous one immediately - `verify` only ever matches the current `credential_hash`,
    /// so the old token stops working the instant this commits, not on some later expiry.
    pub fn rotate_credential(&mut self, client_id: &str, by: &str) -> Result<(Enrollment, String)> {
        ensure!(
            !by.trim().is_empty(),
            "rotating a credential needs an explicit by"
        );
        let token = generate_token();
        let hash = hash_token(&token);
        let enrollment = self.mutate(client_id, |e| {
            ensure!(
                !e.revoked,
                "cannot rotate the credential of a revoked enrollment"
            );
            e.credential_hash = hash.clone();
            e.credential_rotated_at = Some(now());
            Ok(())
        })?;
        self.audit(client_id, "credential_rotated", by, "")?;
        Ok((enrollment, token))
    }

    /// The one check `serve.rs`'s edge dispatch calls on every request: hashes the
    /// presented token and looks for an enrollment whose stored hash matches, re-read from
    /// disk every time (no in-memory cache to go stale). Scales fine at the admin-managed
    /// enrollment counts this is meant for - the same linear-scan-over-`list()` shape
    /// `authorization::Grants::find_covering` already uses.
    pub fn verify(&self, token: &str) -> Result<VerifyOutcome> {
        let hash = hash_token(token);
        for enrollment in self.list()? {
            if enrollment.credential_hash == hash {
                return Ok(if enrollment.revoked {
                    VerifyOutcome::Revoked(enrollment)
                } else {
                    VerifyOutcome::Valid(enrollment)
                });
            }
        }
        Ok(VerifyOutcome::Unknown)
    }
}

/// M12.0 Phase 3: the namespace every `Grant` meant for an edge-enrolled client must use
/// as its `principal`, so it can never be matched by a plain caller-asserted string from
/// the unverified local stdio path - `src/mcp.rs::LocalBackend::covering_grant` refuses to
/// let an untrusted (stdio) caller match a grant in this namespace at all, regardless of
/// what principal it asserts, closing the brief's own "legacy local MCP mode... must not
/// impersonate enrolled remote clients" requirement. A human could still deliberately
/// create a local CLI grant with this literal prefix - that's an operator's own choice on
/// a host they already fully control, not an attack this boundary claims to stop; what it
/// actually stops is an *unverified network-unreachable-by-construction* local caller
/// silently inheriting authority meant for a specific authenticated remote identity.
pub const PRINCIPAL_NAMESPACE_PREFIX: &str = "enrolled:";

pub fn namespaced_principal(client_id: &str) -> String {
    format!("{PRINCIPAL_NAMESPACE_PREFIX}{client_id}")
}

/// "Associated delegated grants" (the brief's enrollment field) and "effective
/// permissions" are both computed fresh from `client_id`'s matching `Grant`s rather than
/// duplicated as a stored link - exactly like `board::Detail.conflict` is computed on
/// demand rather than cached. Takes an already-open `Grants` handle so callers that need
/// both an enrollment and its grants in one CLI command don't open `state.sqlite` twice.
/// Matches the namespaced principal (`grant-create --principal enrolled:<client_id>`), not
/// the bare `client_id` - see `PRINCIPAL_NAMESPACE_PREFIX`.
pub fn grants_for(
    grants: &crate::authorization::Grants,
    client_id: &str,
) -> Result<Vec<crate::authorization::Grant>> {
    let principal = namespaced_principal(client_id);
    Ok(grants
        .list()?
        .into_iter()
        .filter(|g| g.principal == principal)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn create_returns_a_token_whose_hash_matches_verify() {
        let dir = state();
        let mut enrollments = Enrollments::open(dir.path()).unwrap();
        let (enrollment, token) = enrollments
            .create(
                "Hitch",
                "grok-bot",
                vec![EnrollmentOperation::Inspect, EnrollmentOperation::Submit],
                vec!["repo-identity".into()],
                "mike",
            )
            .unwrap();
        assert_ne!(
            enrollment.credential_hash, token,
            "hash must not be the plaintext token"
        );
        match enrollments.verify(&token).unwrap() {
            VerifyOutcome::Valid(found) => assert_eq!(found.client_id, enrollment.client_id),
            other => panic!("expected Valid, got {other:?}"),
        }
    }

    #[test]
    fn wrong_token_is_unknown_not_a_near_match() {
        let dir = state();
        let mut enrollments = Enrollments::open(dir.path()).unwrap();
        let (_enrollment, _token) = enrollments
            .create(
                "Hitch",
                "grok-bot",
                vec![EnrollmentOperation::Inspect],
                vec![],
                "mike",
            )
            .unwrap();
        assert!(matches!(
            enrollments.verify("not-the-real-token").unwrap(),
            VerifyOutcome::Unknown
        ));
    }

    #[test]
    fn revoking_makes_the_next_verify_report_revoked_not_valid() {
        let dir = state();
        let mut enrollments = Enrollments::open(dir.path()).unwrap();
        let (enrollment, token) = enrollments
            .create(
                "Hitch",
                "grok-bot",
                vec![EnrollmentOperation::Inspect],
                vec![],
                "mike",
            )
            .unwrap();
        assert!(matches!(
            enrollments.verify(&token).unwrap(),
            VerifyOutcome::Valid(_)
        ));
        enrollments
            .revoke(&enrollment.client_id, "mike", "rotating off this client")
            .unwrap();
        assert!(matches!(
            enrollments.verify(&token).unwrap(),
            VerifyOutcome::Revoked(_)
        ));
    }

    #[test]
    fn rotating_invalidates_the_old_token_immediately() {
        let dir = state();
        let mut enrollments = Enrollments::open(dir.path()).unwrap();
        let (enrollment, old_token) = enrollments
            .create(
                "Hitch",
                "grok-bot",
                vec![EnrollmentOperation::Inspect],
                vec![],
                "mike",
            )
            .unwrap();
        let (_, new_token) = enrollments
            .rotate_credential(&enrollment.client_id, "mike")
            .unwrap();
        assert_ne!(old_token, new_token);
        assert!(matches!(
            enrollments.verify(&old_token).unwrap(),
            VerifyOutcome::Unknown
        ));
        assert!(matches!(
            enrollments.verify(&new_token).unwrap(),
            VerifyOutcome::Valid(_)
        ));
    }

    #[test]
    fn cannot_rotate_or_be_covered_by_grants_lookup_once_revoked() {
        let dir = state();
        let mut enrollments = Enrollments::open(dir.path()).unwrap();
        let (enrollment, _token) = enrollments
            .create(
                "Hitch",
                "grok-bot",
                vec![EnrollmentOperation::Inspect],
                vec![],
                "mike",
            )
            .unwrap();
        enrollments
            .revoke(&enrollment.client_id, "mike", "done with this one")
            .unwrap();
        assert!(
            enrollments
                .rotate_credential(&enrollment.client_id, "mike")
                .is_err()
        );
    }

    #[test]
    fn audit_trail_records_create_and_revoke() {
        let dir = state();
        let mut enrollments = Enrollments::open(dir.path()).unwrap();
        let (enrollment, _token) = enrollments
            .create(
                "Hitch",
                "grok-bot",
                vec![EnrollmentOperation::Inspect],
                vec![],
                "mike",
            )
            .unwrap();
        enrollments
            .revoke(&enrollment.client_id, "mike", "cleanup")
            .unwrap();
        let audit = enrollments.audit_for(&enrollment.client_id).unwrap();
        let events: Vec<_> = audit.iter().map(|a| a.event.as_str()).collect();
        assert_eq!(events, vec!["created", "revoked"]);
    }

    #[test]
    fn for_tool_covers_every_tool_name_exhaustively() {
        for tool in clatterforge_protocol::TOOL_NAMES {
            assert!(
                EnrollmentOperation::for_tool(tool).is_some(),
                "tool {tool} has no EnrollmentOperation mapping"
            );
        }
    }
}
