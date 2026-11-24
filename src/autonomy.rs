//! M7's first slice: standing policies and "Hold My Beer" unattended dispatch.
//!
//! A `Policy` is a human-authored, explicit, revocable, budgeted, expiring pre-
//! authorization to create/approve/run a *finite, pre-declared* list of fake-engine
//! experiments against one registered repository - never an open-ended "do things"
//! grant. `hold_my_beer::run` is the only thing that ever consumes it: it creates each
//! undispatched experiment via `TaskService::create_fake` (so it's bound by exactly the
//! same narrow, hardcoded `fake-process-v1` scope every other task gets - nothing new is
//! authorized at the engine level), approves it with `by` set to the policy's own id and
//! version (not a human name - this is the literal "autonomous assignments carry the
//! standing policy version... that authorized them" acceptance line), then runs it
//! through `TaskService::run`, which already enforces M6's dependency gate and repository
//! lease unchanged. It stops at `awaiting_review`: nothing here ever accepts a result.
//! That stays a human's call, same as it already is for every other task in this system -
//! "no automatic integration" is this project's single most repeated invariant, and
//! unattended dispatch doesn't get an exception from it.
//!
//! Revoking a policy, or it expiring/exhausting its budget, only ever stops *future*
//! dispatch from `hold_my_beer::run`'s next call; it never touches a task already
//! created, exactly as "mode changes do not silently authorize or alter in-flight work"
//! requires - each `ExperimentRecord` keeps its own `task_id` once dispatched, and no code
//! here ever mutates a task directly.

use crate::{
    inference::config::Config, open_sqlite_with_retry, orchestration, orchestration::Orchestrator,
    process::ExecutionPolicy, service::TaskService, store::Store,
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// M10: the authored, immutable task specification for one real
/// `local-changes-v1` coding task a standing policy pre-authorizes -
/// everything `orchestration::Spec` needs except `repository`, which
/// comes from the policy's own `repository_id` (resolved fresh, through
/// the existing registration, at dispatch time) so a real-task policy
/// gets the exact same "registered repository identity" guarantee a
/// fake-engine policy already has, not a second, looser one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RealTaskSpec {
    pub objective: String,
    pub acceptance: String,
    pub allowed_files: Vec<String>,
    pub context_files: Vec<String>,
    pub verification_files: Vec<String>,
    pub verify_executable: PathBuf,
    pub verify_args: Vec<String>,
    pub target: String,
    #[serde(default = "default_rounds")]
    pub max_rounds: usize,
    #[serde(default = "default_tokens")]
    pub max_output_tokens: u32,
    #[serde(default = "default_generation_deadline")]
    pub generation_timeout_ms: u64,
    #[serde(default = "default_verification_deadline")]
    pub verification_timeout_ms: u64,
    #[serde(default)]
    pub reasoning: Option<bool>,
}
impl RealTaskSpec {
    /// The one place a `RealTaskSpec` becomes an `orchestration::Spec` -
    /// used both for early validation at `policy-create` time (Phase 3,
    /// with a placeholder `repository` the bound/path checks never read)
    /// and for real dispatch (with the repository actually resolved from
    /// the policy's registration), so the two can never drift apart.
    fn to_orchestration_spec(&self, repository: PathBuf) -> orchestration::Spec {
        orchestration::Spec {
            repository,
            objective: self.objective.clone(),
            acceptance: self.acceptance.clone(),
            allowed_files: self.allowed_files.clone(),
            context_files: self.context_files.clone(),
            verification_files: self.verification_files.clone(),
            verify_executable: self.verify_executable.clone(),
            verify_args: self.verify_args.clone(),
            target: self.target.clone(),
            max_rounds: self.max_rounds,
            max_output_tokens: self.max_output_tokens,
            generation_timeout_ms: self.generation_timeout_ms,
            verification_timeout_ms: self.verification_timeout_ms,
            reasoning: self.reasoning,
        }
    }
}
fn default_rounds() -> usize {
    2
}
fn default_tokens() -> u32 {
    4096
}
fn default_generation_deadline() -> u64 {
    120000
}
fn default_verification_deadline() -> u64 {
    30000
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RealTaskRecord {
    pub spec: RealTaskSpec,
    pub task_id: Option<String>,
    pub dispatched_at: Option<u64>,
}

/// The CLI-input file shape for `policy-create --real-tasks`: one shared
/// inference configuration every task under the policy dispatches with -
/// a policy authorizes *one* coherent configuration for its whole batch,
/// not a separately negotiable one per task, matching "a policy must
/// never grant broader permissions than its individual tasks." The
/// launcher is never read from this file - like every other real
/// dispatch in this system, it is always the running `clatterforge`
/// binary itself, resolved fresh at policy-create time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RealPolicyAuthorization {
    pub config: Config,
    pub tasks: Vec<RealTaskSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExperimentSpec {
    pub objective: String,
    pub acceptance: String,
    pub contents: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExperimentRecord {
    pub objective: String,
    pub acceptance: String,
    pub contents: String,
    pub task_id: Option<String>,
    pub dispatched_at: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyState {
    Active,
    Revoked,
    Expired,
    Exhausted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Policy {
    pub id: String,
    pub version: u64,
    pub repository_id: String,
    pub created_by: String,
    pub created_at: u64,
    pub expires_at: u64,
    pub max_tasks: usize,
    pub revoked: bool,
    pub revoked_by: Option<String>,
    pub revoked_reason: Option<String>,
    #[serde(default)]
    pub experiments: Vec<ExperimentRecord>,
    /// M10: populated instead of `experiments` for a real-code policy;
    /// always empty for a fake-engine one. `#[serde(default)]` keeps
    /// every M7/M8/M9 policy record readable unchanged.
    #[serde(default)]
    pub real_tasks: Vec<RealTaskRecord>,
    #[serde(default)]
    pub real_authorization: Option<RealPolicyAuthorizationShared>,
}

/// The shared config/launcher, stored once per policy rather than
/// duplicated onto every `RealTaskRecord`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RealPolicyAuthorizationShared {
    pub config: Config,
    pub launcher: PathBuf,
}

impl Policy {
    /// Computed fresh from the record, not stored - a policy's status depends on wall
    /// clock and on what's been dispatched, neither of which this system tracks with a
    /// background process (nothing here runs unless a human invokes `hold-my-beer run`).
    pub fn state(&self, now: u64) -> PolicyState {
        if self.revoked {
            return PolicyState::Revoked;
        }
        if now >= self.expires_at {
            return PolicyState::Expired;
        }
        let dispatched = self
            .experiments
            .iter()
            .filter(|e| e.task_id.is_some())
            .count()
            + self
                .real_tasks
                .iter()
                .filter(|t| t.task_id.is_some())
                .count();
        let any_undispatched =
            self.next_undispatched().is_some() || self.next_undispatched_real().is_some();
        if dispatched >= self.max_tasks || !any_undispatched {
            return PolicyState::Exhausted;
        }
        PolicyState::Active
    }

    fn next_undispatched(&self) -> Option<usize> {
        self.experiments.iter().position(|e| e.task_id.is_none())
    }
    fn next_undispatched_real(&self) -> Option<usize> {
        self.real_tasks.iter().position(|t| t.task_id.is_none())
    }
}

pub struct Policies {
    connection: Connection,
}

impl Policies {
    pub fn open(root: &Path) -> Result<Self> {
        let connection =
            open_sqlite_with_retry(&root.join("state.sqlite"), 1).context("open policies state")?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS policies(id TEXT PRIMARY KEY, version INTEGER NOT NULL, record TEXT NOT NULL);",
        )?;
        Ok(Self { connection })
    }

    pub fn create(
        &mut self,
        repository_id: &str,
        created_by: &str,
        max_tasks: usize,
        expires_at: u64,
        experiments: Vec<ExperimentSpec>,
    ) -> Result<Policy> {
        ensure!(
            !experiments.is_empty(),
            "a policy needs at least one experiment - it authorizes a finite, pre-declared list, not open-ended work"
        );
        ensure!(max_tasks > 0, "max_tasks must be positive");
        ensure!(
            expires_at > crate::now(),
            "expires_at must be in the future"
        );
        ensure!(
            !created_by.trim().is_empty(),
            "created_by is required - a standing policy needs a human accountable for turning it on"
        );
        let policy = Policy {
            id: Uuid::new_v4().to_string(),
            version: 1,
            repository_id: repository_id.to_string(),
            created_by: created_by.to_string(),
            created_at: crate::now(),
            expires_at,
            max_tasks,
            revoked: false,
            revoked_by: None,
            revoked_reason: None,
            experiments: experiments
                .into_iter()
                .map(|spec| ExperimentRecord {
                    objective: spec.objective,
                    acceptance: spec.acceptance,
                    contents: spec.contents,
                    task_id: None,
                    dispatched_at: None,
                })
                .collect(),
            real_tasks: vec![],
            real_authorization: None,
        };
        self.connection.execute(
            "INSERT INTO policies VALUES (?1, ?2, ?3)",
            params![policy.id, policy.version, serde_json::to_string(&policy)?],
        )?;
        Ok(policy)
    }

    /// M10: the real-code counterpart to `create` above - a finite,
    /// pre-declared list of `local-changes-v1` orchestration tasks
    /// against one registered repository, one shared inference config
    /// and pinned launcher, bounded by the exact same `max_tasks`/
    /// `expires_at`/revocation machinery `create`'s fake-engine policies
    /// already use. Never populates `experiments`, so `hold_my_beer::run`
    /// can tell which engine a policy authorizes purely from which list
    /// is non-empty.
    pub fn create_real(
        &mut self,
        repository_id: &str,
        created_by: &str,
        max_tasks: usize,
        expires_at: u64,
        authorization: RealPolicyAuthorizationShared,
        tasks: Vec<RealTaskSpec>,
    ) -> Result<Policy> {
        ensure!(
            !tasks.is_empty(),
            "a policy needs at least one task - it authorizes a finite, pre-declared list, not open-ended work"
        );
        ensure!(max_tasks > 0, "max_tasks must be positive");
        ensure!(
            expires_at > crate::now(),
            "expires_at must be in the future"
        );
        ensure!(
            !created_by.trim().is_empty(),
            "created_by is required - a standing policy needs a human accountable for turning it on"
        );
        authorization.config.validate()?;
        for task in &tasks {
            authorization.config.target(&task.target)?;
            // M10.1 Phase 3: the exact same checks `Orchestrator::create_with_id`
            // runs at dispatch time, run now instead - a malformed real task
            // (an out-of-bounds timeout, an invalid file-scope count, a
            // verifier that doesn't exist) fails `policy-create`, not the
            // first `hold-my-beer` call that happens to reach it. The
            // placeholder repository path is never read by either check.
            let spec = task.to_orchestration_spec(PathBuf::new());
            orchestration::validate_spec_bounds(&spec)?;
            orchestration::validate_verifier(&spec)?;
        }
        let policy = Policy {
            id: Uuid::new_v4().to_string(),
            version: 1,
            repository_id: repository_id.to_string(),
            created_by: created_by.to_string(),
            created_at: crate::now(),
            expires_at,
            max_tasks,
            revoked: false,
            revoked_by: None,
            revoked_reason: None,
            experiments: vec![],
            real_tasks: tasks
                .into_iter()
                .map(|spec| RealTaskRecord {
                    spec,
                    task_id: None,
                    dispatched_at: None,
                })
                .collect(),
            real_authorization: Some(authorization),
        };
        self.connection.execute(
            "INSERT INTO policies VALUES (?1, ?2, ?3)",
            params![policy.id, policy.version, serde_json::to_string(&policy)?],
        )?;
        Ok(policy)
    }

    pub fn get(&self, id: &str) -> Result<Policy> {
        let record: String = self
            .connection
            .query_row("SELECT record FROM policies WHERE id=?1", [id], |r| {
                r.get(0)
            })
            .context("unknown policy id")?;
        serde_json::from_str(&record).context("corrupt policy record")
    }

    fn mutate(
        &mut self,
        id: &str,
        change: impl FnOnce(&mut Policy) -> Result<()>,
    ) -> Result<Policy> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let record: String = tx
            .query_row("SELECT record FROM policies WHERE id=?1", [id], |r| {
                r.get(0)
            })
            .context("unknown policy id")?;
        let mut policy: Policy = serde_json::from_str(&record)?;
        change(&mut policy)?;
        policy.version += 1;
        let record = serde_json::to_string(&policy)?;
        tx.execute(
            "UPDATE policies SET version=?1, record=?2 WHERE id=?3",
            params![policy.version, record, id],
        )?;
        tx.commit()?;
        Ok(policy)
    }

    pub fn revoke(&mut self, id: &str, by: &str, reason: &str) -> Result<Policy> {
        ensure!(
            !by.trim().is_empty() && !reason.trim().is_empty(),
            "revoking a policy needs an explicit by/reason"
        );
        self.mutate(id, |p| {
            ensure!(!p.revoked, "policy already revoked");
            p.revoked = true;
            p.revoked_by = Some(by.to_string());
            p.revoked_reason = Some(reason.to_string());
            Ok(())
        })
    }

    /// Called once per experiment, immediately after its task is created - before
    /// `approve`/`run` are even attempted - so a crash or failure anywhere after that
    /// point still leaves the experiment correctly marked dispatched; `hold_my_beer::run`
    /// never creates a second task for the same experiment slot on a later call.
    fn mark_dispatched(&mut self, id: &str, index: usize, task_id: &str) -> Result<Policy> {
        self.mutate(id, |p| {
            let experiment = p
                .experiments
                .get_mut(index)
                .context("no such experiment index")?;
            ensure!(
                experiment.task_id.is_none(),
                "experiment already dispatched"
            );
            experiment.task_id = Some(task_id.to_string());
            experiment.dispatched_at = Some(crate::now());
            Ok(())
        })
    }

    /// M10.1: closes the exact crash/race window `mark_dispatched_real`
    /// (M10) used to leave open - it recorded a task's id only *after*
    /// `Orchestrator::create` had already succeeded, so a crash or a second
    /// concurrent dispatcher between those two calls could create a second,
    /// orphaned orchestration task for the same slot. This reserves the
    /// slot's task id *before* any orchestration task exists, inside the
    /// same transactional read-check-write `mutate` every other policy
    /// mutation already uses - so two genuinely concurrent callers racing
    /// for the same index serialize through SQLite's own locking, and only
    /// one of them ever receives a given slot's id to create. The returned
    /// id is durable from this point on, letting a crashed caller recover
    /// (`Orchestrator::task(id)` either already exists - recover its
    /// current status - or doesn't - safe to retry `create_with_id` with
    /// this exact id, which can insert at most once).
    fn reserve_real(&mut self, id: &str, index: usize) -> Result<(Policy, String)> {
        let fresh = Uuid::new_v4().to_string();
        let mut reserved = fresh.clone();
        let policy = self.mutate(id, |p| {
            let task = p
                .real_tasks
                .get_mut(index)
                .context("no such real task index")?;
            match &task.task_id {
                Some(existing) => {
                    reserved = existing.clone();
                }
                None => {
                    task.task_id = Some(fresh.clone());
                    task.dispatched_at = Some(crate::now());
                }
            }
            Ok(())
        })?;
        Ok((policy, reserved))
    }
}

#[derive(Debug, Serialize)]
pub struct HoldMyBeerOutcome {
    pub policy_id: String,
    pub dispatched: Vec<String>,
    pub stopped: String,
}

/// The entire unattended loop, one synchronous call - consistent with every other
/// dispatch in this system (no background scheduler anywhere). Stops, rather than erroring
/// out entirely, on policy exhaustion/expiry/revocation or running out of undispatched
/// experiments - those are expected end states, not failures, and the caller gets back
/// what was actually accomplished either way. A genuine `Err` from create/approve/run
/// (infrastructure trouble - a busy repository lease, a revision conflict - as opposed to
/// an experiment's own attempt simply failing, which is a normal, expected outcome this
/// loop records and moves past) stops the batch immediately rather than continuing to
/// hammer a wall every remaining experiment would likely also hit.
pub fn run(
    root: &Path,
    policy_id: &str,
    execution: ExecutionPolicy,
    context_budget_bytes: usize,
) -> Result<HoldMyBeerOutcome> {
    let mut policies = Policies::open(root)?;
    let mut service = TaskService::open(root)?;
    let mut dispatched = Vec::new();
    loop {
        let policy = policies.get(policy_id)?;
        let state = policy.state(crate::now());
        // Revoked/expired stop immediately, before even attempting recovery below -
        // an unfulfilled reservation is still future work, and "revocation/expiry
        // stops future dispatch" covers it exactly as it covers an undispatched
        // slot. Exhausted is different: a policy can look Exhausted purely because
        // its one remaining slot is an *orphaned reservation* (see below), and
        // finishing that is recovery, not new dispatch - so Exhausted alone must
        // not short-circuit before the orphan scan gets a chance to run.
        if matches!(state, PolicyState::Revoked | PolicyState::Expired) {
            return Ok(HoldMyBeerOutcome {
                policy_id: policy.id,
                dispatched,
                stopped: format!("{state:?}"),
            });
        }
        if !policy.real_tasks.is_empty() {
            // M10.1: before picking a fresh slot, recover any slot whose task id was
            // already reserved (by this process or a crashed/losing earlier one) but
            // whose orchestration task doesn't actually exist yet - never leaves a
            // reservation stranded, and never reserves a second, different id for the
            // same slot. `reserve_real`'s own transaction guarantees at most one caller
            // ever receives a *fresh* id per slot; this scan is what makes that id
            // durable across a crash between reservation and the task actually existing.
            let scan = Orchestrator::open(root)?;
            let orphan = policy.real_tasks.iter().enumerate().find_map(|(i, t)| {
                let task_id = t.task_id.as_ref()?;
                scan.task(task_id).is_err().then(|| (i, task_id.clone()))
            });
            drop(scan);
            let (index, task_id) = match orphan {
                Some(found) => found,
                None => {
                    if state != PolicyState::Active {
                        return Ok(HoldMyBeerOutcome {
                            policy_id: policy.id,
                            dispatched,
                            stopped: format!("{state:?}"),
                        });
                    }
                    let Some(index) = policy.next_undispatched_real() else {
                        return Ok(HoldMyBeerOutcome {
                            policy_id: policy.id,
                            dispatched,
                            stopped: "no undispatched real tasks remain".into(),
                        });
                    };
                    let (_, task_id) = policies.reserve_real(policy_id, index)?;
                    (index, task_id)
                }
            };
            dispatched.push(task_id.clone());
            let record = policy.real_tasks[index].spec.clone();
            let authorization = policy
                .real_authorization
                .clone()
                .context("real-task policy missing its authorization")?;
            let repository = Store::open(root)?.repository(&policy.repository_id)?;
            let spec = record.to_orchestration_spec(repository.checkout);
            // `create`/`approve` are the same unprotected preparation-only operations
            // every other orchestration task uses - no new authority is granted here.
            // Only the actual dispatch below goes through `TaskService::run`, which is
            // where global stop, the dependency gate, the repository lease and GPU/
            // inference admission are all already enforced for every other caller.
            let mut orchestrator = Orchestrator::open(root)?;
            let task = match orchestrator.task(&task_id) {
                Ok(existing) => existing,
                Err(_) => orchestrator.create_with_id(
                    task_id.clone(),
                    spec,
                    authorization.config,
                    &authorization.launcher,
                )?,
            };
            let by = format!("policy:{}@v{}", policy.id, policy.version);
            // Status-gated: never re-approve or re-run a task recovery found already
            // past these stages - that would be replaying an attempt whose outcome is
            // already recorded, exactly what "no automatic replay" forbids.
            let task = if task.status == orchestration::Status::AwaitingApproval {
                let digest = task.assignment.digest()?;
                orchestrator.approve(&task.id, task.revision, &digest, &by)?
            } else {
                task
            };
            if task.status == orchestration::Status::Approved {
                // A failed/timed-out *attempt* is a normal task outcome, visible for
                // human review - not a reason to stop the batch. Only an `Err`
                // (infrastructure trouble) does that, via `?` below.
                service.run(&task.id, task.revision)?;
            }
            continue;
        }
        if state != PolicyState::Active {
            return Ok(HoldMyBeerOutcome {
                policy_id: policy.id,
                dispatched,
                stopped: format!("{state:?}"),
            });
        }
        let Some(index) = policy.next_undispatched() else {
            return Ok(HoldMyBeerOutcome {
                policy_id: policy.id,
                dispatched,
                stopped: "no undispatched experiments remain".into(),
            });
        };
        let experiment = policy.experiments[index].clone();
        let detail = service.create_fake(
            &policy.repository_id,
            experiment.objective,
            experiment.acceptance,
            experiment.contents,
            context_budget_bytes,
            execution.clone(),
        )?;
        policies.mark_dispatched(policy_id, index, &detail.id)?;
        dispatched.push(detail.id.clone());
        let by = format!("policy:{}@v{}", policy.id, policy.version);
        let approved =
            service.approve(&detail.id, detail.revision, &detail.assignment_digest, &by)?;
        // A failed/timed-out *attempt* is a normal experiment outcome, visible on the
        // task itself for human review - not a reason to stop the batch. Only an `Err`
        // (infrastructure trouble) does that, via `?` below.
        service.run(&approved.id, approved.revision)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(objective: &str) -> ExperimentSpec {
        ExperimentSpec {
            objective: objective.into(),
            acceptance: "file exists".into(),
            contents: "y".into(),
        }
    }

    #[test]
    fn create_rejects_empty_experiment_list() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = Policies::open(dir.path()).unwrap();
        let err = p
            .create("repo-a", "tester", 5, crate::now() + 10_000, vec![])
            .unwrap_err();
        assert!(err.to_string().contains("at least one experiment"), "{err}");
    }

    #[test]
    fn create_rejects_past_expiry() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = Policies::open(dir.path()).unwrap();
        let err = p
            .create("repo-a", "tester", 5, 1, vec![spec("a")])
            .unwrap_err();
        assert!(err.to_string().contains("future"), "{err}");
    }

    #[test]
    fn fresh_policy_is_active() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = Policies::open(dir.path()).unwrap();
        let policy = p
            .create(
                "repo-a",
                "tester",
                5,
                crate::now() + 10_000,
                vec![spec("a")],
            )
            .unwrap();
        assert_eq!(policy.state(crate::now()), PolicyState::Active);
    }

    #[test]
    fn expired_policy_is_expired_even_with_budget_and_experiments_left() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = Policies::open(dir.path()).unwrap();
        let policy = p
            .create(
                "repo-a",
                "tester",
                5,
                crate::now() + 10_000,
                vec![spec("a")],
            )
            .unwrap();
        assert_eq!(policy.state(crate::now() + 20_000), PolicyState::Expired);
    }

    #[test]
    fn revoked_policy_is_revoked_regardless_of_budget_or_expiry() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = Policies::open(dir.path()).unwrap();
        let policy = p
            .create(
                "repo-a",
                "tester",
                5,
                crate::now() + 10_000,
                vec![spec("a")],
            )
            .unwrap();
        let revoked = p.revoke(&policy.id, "tester", "changed my mind").unwrap();
        assert_eq!(revoked.state(crate::now()), PolicyState::Revoked);
    }

    #[test]
    fn revoking_twice_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = Policies::open(dir.path()).unwrap();
        let policy = p
            .create(
                "repo-a",
                "tester",
                5,
                crate::now() + 10_000,
                vec![spec("a")],
            )
            .unwrap();
        p.revoke(&policy.id, "tester", "first").unwrap();
        let err = p.revoke(&policy.id, "tester", "second").unwrap_err();
        assert!(err.to_string().contains("already revoked"), "{err}");
    }

    #[test]
    fn exhausted_once_every_experiment_is_dispatched() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = Policies::open(dir.path()).unwrap();
        let policy = p
            .create(
                "repo-a",
                "tester",
                5,
                crate::now() + 10_000,
                vec![spec("a")],
            )
            .unwrap();
        let policy = p.mark_dispatched(&policy.id, 0, "task-1").unwrap();
        assert_eq!(policy.state(crate::now()), PolicyState::Exhausted);
    }

    #[test]
    fn exhausted_once_max_tasks_is_reached_even_with_experiments_left() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = Policies::open(dir.path()).unwrap();
        let policy = p
            .create(
                "repo-a",
                "tester",
                1,
                crate::now() + 10_000,
                vec![spec("a"), spec("b")],
            )
            .unwrap();
        let policy = p.mark_dispatched(&policy.id, 0, "task-1").unwrap();
        assert_eq!(policy.state(crate::now()), PolicyState::Exhausted);
        assert!(policy.experiments[1].task_id.is_none());
    }

    #[test]
    fn mark_dispatched_twice_for_the_same_experiment_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = Policies::open(dir.path()).unwrap();
        let policy = p
            .create(
                "repo-a",
                "tester",
                5,
                crate::now() + 10_000,
                vec![spec("a")],
            )
            .unwrap();
        p.mark_dispatched(&policy.id, 0, "task-1").unwrap();
        let err = p.mark_dispatched(&policy.id, 0, "task-2").unwrap_err();
        assert!(err.to_string().contains("already dispatched"), "{err}");
    }

    fn real_authorization() -> RealPolicyAuthorizationShared {
        RealPolicyAuthorizationShared {
            config: crate::inference::config::Config {
                schema_version: 1,
                default_target: "native".into(),
                hardware_id: "fixture".into(),
                admission_db: PathBuf::from("admission.sqlite"),
                max_active_per_resource: 1,
                registry_max_age_ms: 60_000,
                fallback_before_dispatch: false,
                targets: vec![crate::inference::config::Target {
                    id: "native".into(),
                    provider: crate::inference::types::ProviderKind::Ollama,
                    endpoint: "http://127.0.0.1:1/".into(),
                    endpoint_access: crate::inference::config::EndpointAccess::Loopback,
                    model: "fixture".into(),
                    expected_artifact_digest: None,
                    context_limit: 32768,
                    output_limit: 128,
                    capabilities: crate::inference::types::Capabilities {
                        streaming: true,
                        ..Default::default()
                    },
                    capability_source: "fixture".into(),
                    priority: 0,
                    resource_id: "fixture-gpu".into(),
                    extensions: None,
                    api_pricing: None,
                }],
            },
            launcher: PathBuf::from("launcher"),
        }
    }

    fn real_task(objective: &str) -> RealTaskSpec {
        RealTaskSpec {
            objective: objective.into(),
            acceptance: "verifier passes".into(),
            allowed_files: vec!["a.txt".into()],
            context_files: vec![],
            verification_files: vec!["v.txt".into()],
            verify_executable: std::env::current_exe().unwrap(),
            verify_args: vec![],
            target: "native".into(),
            max_rounds: 1,
            max_output_tokens: 128,
            generation_timeout_ms: 5000,
            verification_timeout_ms: 5000,
            reasoning: None,
        }
    }

    #[test]
    fn create_real_rejects_empty_task_list() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = Policies::open(dir.path()).unwrap();
        let err = p
            .create_real(
                "repo-a",
                "tester",
                5,
                crate::now() + 10_000,
                real_authorization(),
                vec![],
            )
            .unwrap_err();
        assert!(err.to_string().contains("at least one task"), "{err}");
    }

    #[test]
    fn create_real_rejects_past_expiry() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = Policies::open(dir.path()).unwrap();
        let err = p
            .create_real(
                "repo-a",
                "tester",
                5,
                1,
                real_authorization(),
                vec![real_task("a")],
            )
            .unwrap_err();
        assert!(err.to_string().contains("future"), "{err}");
    }

    #[test]
    fn create_real_rejects_a_target_no_task_references() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = Policies::open(dir.path()).unwrap();
        let mut task = real_task("a");
        task.target = "nonexistent".into();
        let err = p
            .create_real(
                "repo-a",
                "tester",
                5,
                crate::now() + 10_000,
                real_authorization(),
                vec![task],
            )
            .unwrap_err();
        assert!(err.to_string().contains("target"), "{err}");
    }

    /// M10.1 Phase 3: reproduces, at `create_real` time, the exact mistake
    /// M10's live pilot only caught at `hold-my-beer` dispatch time - a
    /// `generation_timeout_ms` exceeding `Orchestrator::create`'s own
    /// 100-120000ms bound. The fix means this now fails before the policy
    /// is even created, not three real-model generations later.
    #[test]
    fn create_real_rejects_an_out_of_bounds_timeout_before_the_policy_exists() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = Policies::open(dir.path()).unwrap();
        let mut task = real_task("a");
        task.generation_timeout_ms = 180_000;
        let err = p
            .create_real(
                "repo-a",
                "tester",
                5,
                crate::now() + 10_000,
                real_authorization(),
                vec![task],
            )
            .unwrap_err();
        assert!(err.to_string().contains("budgets"), "{err}");
    }

    #[test]
    fn create_real_rejects_a_path_traversal_allowed_file_before_the_policy_exists() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = Policies::open(dir.path()).unwrap();
        let mut task = real_task("a");
        task.allowed_files = vec!["../outside.txt".into()];
        let err = p
            .create_real(
                "repo-a",
                "tester",
                5,
                crate::now() + 10_000,
                real_authorization(),
                vec![task],
            )
            .unwrap_err();
        assert!(err.to_string().contains("path"), "{err}");
    }

    #[test]
    fn create_real_rejects_a_missing_verifier_executable() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = Policies::open(dir.path()).unwrap();
        let mut task = real_task("a");
        task.verify_executable = dir.path().join("no-such-verifier.exe");
        let err = p
            .create_real(
                "repo-a",
                "tester",
                5,
                crate::now() + 10_000,
                real_authorization(),
                vec![task],
            )
            .unwrap_err();
        assert!(err.to_string().contains("verifier"), "{err}");
    }

    #[test]
    fn a_real_task_policy_leaves_experiments_empty_and_vice_versa() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = Policies::open(dir.path()).unwrap();
        let real = p
            .create_real(
                "repo-a",
                "tester",
                5,
                crate::now() + 10_000,
                real_authorization(),
                vec![real_task("a")],
            )
            .unwrap();
        assert!(real.experiments.is_empty());
        assert_eq!(real.real_tasks.len(), 1);

        let fake = p
            .create(
                "repo-a",
                "tester",
                5,
                crate::now() + 10_000,
                vec![spec("a")],
            )
            .unwrap();
        assert!(fake.real_tasks.is_empty());
        assert_eq!(fake.experiments.len(), 1);
    }

    #[test]
    fn exhausted_once_every_real_task_is_reserved() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = Policies::open(dir.path()).unwrap();
        let policy = p
            .create_real(
                "repo-a",
                "tester",
                5,
                crate::now() + 10_000,
                real_authorization(),
                vec![real_task("a")],
            )
            .unwrap();
        let (policy, _task_id) = p.reserve_real(&policy.id, 0).unwrap();
        assert_eq!(policy.state(crate::now()), PolicyState::Exhausted);
    }

    #[test]
    fn reserving_the_same_slot_twice_is_idempotent_not_a_second_fresh_id() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = Policies::open(dir.path()).unwrap();
        let policy = p
            .create_real(
                "repo-a",
                "tester",
                5,
                crate::now() + 10_000,
                real_authorization(),
                vec![real_task("a")],
            )
            .unwrap();
        let (_, first) = p.reserve_real(&policy.id, 0).unwrap();
        let (_, second) = p.reserve_real(&policy.id, 0).unwrap();
        assert_eq!(
            first, second,
            "a slot that already has a reserved id must keep it - never a second, different id"
        );
    }

    fn fixture_repository(root: &Path) -> (PathBuf, String) {
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        crate::repository::git(&repo, &["init", "-b", "main"]).unwrap();
        crate::repository::git(&repo, &["config", "core.autocrlf", "false"]).unwrap();
        std::fs::write(repo.join("a.txt"), "x\n").unwrap();
        std::fs::write(repo.join("v.txt"), "y\n").unwrap();
        crate::repository::git(&repo, &["add", "."]).unwrap();
        crate::repository::git(
            &repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.invalid",
                "-c",
                "core.hooksPath=",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                "x",
            ],
        )
        .unwrap();
        let state = root.join("state");
        let registration_id = {
            let mut store = crate::store::Store::open(&state).unwrap();
            let (id, _) = store.register(&repo).unwrap();
            id
        };
        (state, registration_id)
    }

    /// M10.1 Phase 2: the actual crash scenario the atomic reservation
    /// closes - a slot's id is durably reserved, but the process dies (or
    /// loses a race) before `Orchestrator::create_with_id` ever runs, so no
    /// orchestration task exists for that id yet. The *next* `hold-my-beer`
    /// call must recover using the exact same id - never generate a second,
    /// different one for the same slot - and must not blindly replay past
    /// whatever stage the (nonexistent) task would have reached.
    #[test]
    fn crash_between_reservation_and_create_is_recovered_with_the_same_id() {
        let dir = tempfile::tempdir().unwrap();
        let (state, registration_id) = fixture_repository(dir.path());

        let real_exe: PathBuf = std::env::current_exe().unwrap();
        let mut task = real_task("fix the thing");
        task.allowed_files = vec!["a.txt".into()];
        task.verification_files = vec!["v.txt".into()];
        task.verify_executable = real_exe.clone();

        let mut p = Policies::open(&state).unwrap();
        let policy = p
            .create_real(
                &registration_id,
                "tester",
                5,
                crate::now() + 10_000,
                RealPolicyAuthorizationShared {
                    config: real_authorization().config,
                    launcher: real_exe.clone(),
                },
                vec![task],
            )
            .unwrap();

        // Simulate the crash: reserve the slot, then stop - exactly what a
        // process death right after `reserve_real` commits would leave
        // behind. No orchestration task exists yet.
        let (_, reserved_id) = p.reserve_real(&policy.id, 0).unwrap();
        assert!(
            Orchestrator::open(&state)
                .unwrap()
                .task(&reserved_id)
                .is_err(),
            "nothing should exist yet - this is the simulated crash window"
        );

        // Recovery: a fresh `hold_my_beer::run` call (standing in for a
        // restarted process) must find the orphaned reservation and create
        // the task under that exact id. `run()` itself will fail shortly
        // after (the config's target is an unreachable fixture endpoint -
        // this test only needs create+approve to succeed using the
        // reserved id, not a real generation).
        let outcome = run(
            &state,
            &policy.id,
            ExecutionPolicy::new(&real_exe).unwrap(),
            16384,
        )
        .unwrap();
        // The unreachable endpoint makes generation fail - a normal, recorded
        // business outcome (not an infrastructure `Err`), same as any other
        // model-attempt failure elsewhere in this system.
        assert_eq!(outcome.dispatched, vec![reserved_id.clone()]);

        let tasks = Orchestrator::open(&state).unwrap().tasks().unwrap();
        assert_eq!(
            tasks.len(),
            1,
            "recovery must create exactly one task, under the reserved id - never a second one"
        );
        assert_eq!(tasks[0].id, reserved_id);
        assert_ne!(
            tasks[0].status,
            crate::orchestration::Status::AwaitingApproval,
            "recovery must carry the task through approval, not just create and stop"
        );

        let after = p.get(&policy.id).unwrap();
        assert_eq!(
            after.real_tasks[0].task_id.as_deref(),
            Some(reserved_id.as_str())
        );
    }
}
