//! Cross-engine task service: the one implementation of "resolve an id/prefix, find
//! which engine (`Store` or `Orchestrator`) owns it, then read or mutate it." Both the
//! CLI's `board-*` commands and the `serve` HTTP service call this, so there is exactly
//! one copy of that dispatch logic rather than two that can drift apart.

use crate::{
    agents::{self, AgentStatus, Agents},
    autonomy::{Policy, PolicyState},
    board,
    coordination::Coordination,
    execution::{CodexEngine, ExecutionEngine},
    orchestration::{self, Orchestrator},
    process,
    repository::{git, git_text},
    stop::{GlobalStop, StopState},
    store::{Status as FakeStatus, Store},
};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    path::{Path, PathBuf},
};

pub struct TaskService {
    store: Store,
    orchestrator: Orchestrator,
    agents: Agents,
    workflows: crate::workflow::Workflows,
    coordination: Coordination,
    stop: GlobalStop,
    root: PathBuf,
}

/// What `stop_all` actually did: the switch's new state, which tasks it successfully
/// requested cancellation for, and which it couldn't (already terminal, or some other
/// per-task refusal) - recorded rather than treated as this call's own failure, the same
/// distinction `autonomy::hold_my_beer::run` draws between an expected business refusal
/// and a genuine infrastructure error.
#[derive(Debug, Serialize)]
pub struct StopAllOutcome {
    pub state: StopState,
    pub cancelled: Vec<String>,
    pub not_cancelled: Vec<String>,
}

/// Holds a repository lease for the duration of one `run` dispatch. `finish` releases it
/// cleanly - correct for *any* return from the inner dispatch, success or an ordinary
/// business error alike, since by the time either happens the owned child process this
/// attempt may have spawned has already been deterministically reaped by the engine's own
/// Job-object/process-ownership machinery. Only a genuine panic/crash skips `finish` and
/// falls through to `Drop`, which quarantines instead of releasing: that's the one case
/// where the repository/workspace's actual state is genuinely unknown, not just "the
/// attempt failed." Mirrors `inference::jobs::Reservation` deliberately, not by accident.
struct LeaseReservation {
    coordination: Coordination,
    resource: String,
    task_id: String,
    finished: bool,
}

impl LeaseReservation {
    fn new(root: &std::path::Path, resource: String, task_id: String) -> Result<Self> {
        Ok(Self {
            coordination: Coordination::open(root)?,
            resource,
            task_id,
            finished: false,
        })
    }

    fn finish(&mut self) -> Result<()> {
        self.coordination.release(&self.resource, &self.task_id)?;
        self.finished = true;
        Ok(())
    }
}

impl Drop for LeaseReservation {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.coordination.quarantine_task(&self.task_id);
        }
    }
}

/// M10.4 Phase 1: the exact repository-lease mechanism `TaskService::run` already used,
/// extracted so the legacy single-engine `run`/`orchestrate-run` CLI commands (`src/main.rs`)
/// can acquire the identical lease around their own existing dispatch - closing the
/// documented gap where those two commands enforced global stop but not repository
/// admission - without duplicating the acquire/reservation/finish logic a second time.
/// `TaskService::run` below calls this too, so both paths are provably the same code, not
/// two copies that could drift apart. Acquires `resource` for `task_id`, runs `dispatch`
/// exactly once, and releases on any ordinary return; a genuine panic/crash instead falls
/// through to `LeaseReservation::drop`, which quarantines rather than releases.
pub fn with_repository_lease<T>(
    root: &std::path::Path,
    resource: String,
    task_id: String,
    dispatch: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let owner = process::current_identity()?;
    Coordination::open(root)?.acquire(&resource, &task_id, &owner)?;
    let mut reservation = LeaseReservation::new(root, resource, task_id)?;
    let outcome = dispatch();
    reservation.finish()?;
    outcome
}

/// A run's result plus whether it reached a state the caller should treat as success,
/// using each engine's own existing definition (not a single reinvented one): the fake
/// engine excludes only Failed/Cancelled/TimedOut/Unknown, while orchestration requires
/// exactly `AwaitingReview`.
pub struct RunOutcome {
    pub detail: board::Detail,
    pub succeeded: bool,
}

fn read_preview(
    label: &str,
    path: &std::path::Path,
    total_bytes: u64,
    capture_truncated: bool,
    max_bytes: usize,
) -> Option<board::Preview> {
    let bytes = std::fs::read(path).ok()?;
    let capped = &bytes[..bytes.len().min(max_bytes)];
    Some(board::build_preview(
        label,
        &path.display().to_string(),
        capped,
        total_bytes,
        capture_truncated,
    ))
}

/// A phase's stdout/stderr `.part` files are written live by the owned child process
/// (`OwnedProcess::spawn`) and persist after completion too; while the phase is still
/// active, these are the only on-disk evidence of what is happening, since the final
/// `.log` artifact and its receipt are only written once the process returns.
fn live_part_previews(
    directory: &std::path::Path,
    phase: &str,
    max_bytes: usize,
) -> Vec<board::Preview> {
    [("stdout", "stdout.part"), ("stderr", "stderr.part")]
        .into_iter()
        .filter_map(|(stream, suffix)| {
            let path = directory.join(format!("{phase}.{suffix}"));
            let total_bytes = std::fs::metadata(&path).ok()?.len();
            read_preview(
                &format!("live {phase} {stream}"),
                &path,
                total_bytes,
                false,
                max_bytes,
            )
        })
        .collect()
}

/// Bounded diff/verifier-output previews for the most recent fake/Codex attempt only;
/// earlier attempts keep their summary without re-reading retained artifacts.
fn fake_last_previews(
    root: &std::path::Path,
    task: &crate::store::Task,
    max_bytes: usize,
) -> Vec<board::Preview> {
    let Some(attempt) = task.attempts.last() else {
        return Vec::new();
    };
    let mut previews = Vec::new();
    if let Some(diff) = &attempt.diff
        && let Some(p) = read_preview("diff", &diff.path, diff.bytes as u64, false, max_bytes)
    {
        previews.push(p);
    }
    let live_phase = match task.status {
        FakeStatus::Running => Some("execution"),
        FakeStatus::Verifying => Some("verification"),
        _ => None,
    };
    if let Some(phase) = live_phase {
        previews.extend(live_part_previews(
            &root.join("artifacts").join(&attempt.id),
            phase,
            max_bytes,
        ));
    }
    if let Some(process_receipt) = attempt
        .receipt
        .as_ref()
        .and_then(|r| r.process_receipt.as_ref())
    {
        if let Some(p) = read_preview(
            "verifier stdout",
            &process_receipt.stdout.artifact.path,
            process_receipt.stdout.total_bytes,
            process_receipt.stdout.truncated,
            max_bytes,
        ) {
            previews.push(p);
        }
        if let Some(p) = read_preview(
            "verifier stderr",
            &process_receipt.stderr.artifact.path,
            process_receipt.stderr.total_bytes,
            process_receipt.stderr.truncated,
            max_bytes,
        ) {
            previews.push(p);
        }
    }
    previews
}

/// Bounded diff/verifier-output previews for the most recent orchestration round only.
/// Generation has no owned child process (it's a direct inference HTTP call), so there
/// is nothing live to tail while `Status::Generating`; only the verifier phase spawns one.
fn orchestrate_last_previews(
    root: &std::path::Path,
    task: &orchestration::Task,
    max_bytes: usize,
) -> Vec<board::Preview> {
    let Some(round) = task.rounds.last() else {
        return Vec::new();
    };
    let mut previews = Vec::new();
    if let Some(patch) = &round.patch
        && let Some(p) = read_preview("diff", &patch.path, patch.bytes as u64, false, max_bytes)
    {
        previews.push(p);
    }
    if task.status == orchestration::Status::Verifying {
        previews.extend(live_part_previews(
            &root
                .join("orchestration-artifacts")
                .join(&task.id)
                .join((task.rounds.len() - 1).to_string()),
            "verify",
            max_bytes,
        ));
    }
    if let Some(receipt) = &round.verification {
        if let Some(p) = read_preview(
            "verifier stdout",
            &receipt.stdout.artifact.path,
            receipt.stdout.total_bytes,
            receipt.stdout.truncated,
            max_bytes,
        ) {
            previews.push(p);
        }
        if let Some(p) = read_preview(
            "verifier stderr",
            &receipt.stderr.artifact.path,
            receipt.stderr.total_bytes,
            receipt.stderr.truncated,
            max_bytes,
        ) {
            previews.push(p);
        }
    }
    previews
}

/// M12.0 Phase 6: bounded diff/verifier-output previews for an agent task's one real
/// attempt - mirrors `fake_last_previews`/`orchestrate_last_previews` exactly (same
/// labels, same `read_preview` primitive), closing the one place `agent_detail_of` never
/// populated this before (it didn't accept `preview_bytes` at all until this phase,
/// because M10.1-M10.4 never needed an evidence-focused caller for agent tasks - the CLI/
/// GUI board views were enough). Active native-agent logs use the same bounded live
/// previews as the other engines.
fn agent_last_previews(
    root: &std::path::Path,
    task: &agents::AgentTask,
    max_bytes: usize,
) -> Vec<board::Preview> {
    let mut previews = Vec::new();
    let phase = match task.status {
        AgentStatus::Running => Some("execution"),
        AgentStatus::Verifying => Some("verification"),
        _ => None,
    };
    if let Some(phase) = phase {
        previews.extend(live_part_previews(
            &root.join("agent-artifacts").join(&task.id),
            phase,
            max_bytes,
        ));
    }
    let diff_label = if task.verification.is_none() {
        "partial diff (unverified)"
    } else {
        "diff"
    };
    if let Some(patch) = &task.patch
        && patch.bytes > 0
        && let Some(p) = read_preview(
            diff_label,
            &patch.path,
            patch.bytes as u64,
            false,
            max_bytes,
        )
    {
        previews.push(p);
    }
    if let Some(receipt) = &task.execution
        && task.status != AgentStatus::Running
    {
        if task.verification.is_none()
            && receipt.stdout.total_bytes > 0
            && let Some(p) = read_preview(
                "execution stdout",
                &receipt.stdout.artifact.path,
                receipt.stdout.total_bytes,
                receipt.stdout.truncated,
                max_bytes,
            )
        {
            previews.push(p);
        }
        if receipt.stderr.total_bytes > 0
            && let Some(p) = read_preview(
                "execution stderr",
                &receipt.stderr.artifact.path,
                receipt.stderr.total_bytes,
                receipt.stderr.truncated,
                max_bytes,
            )
        {
            previews.push(p);
        }
    }
    if let Some(receipt) = &task.verification {
        if let Some(p) = read_preview(
            "verifier stdout",
            &receipt.stdout.artifact.path,
            receipt.stdout.total_bytes,
            receipt.stdout.truncated,
            max_bytes,
        ) {
            previews.push(p);
        }
        if let Some(p) = read_preview(
            "verifier stderr",
            &receipt.stderr.artifact.path,
            receipt.stderr.total_bytes,
            receipt.stderr.truncated,
            max_bytes,
        ) {
            previews.push(p);
        }
    }
    previews
}

fn latest_activity_timestamps(
    root: &std::path::Path,
) -> Result<std::collections::HashMap<String, u64>> {
    let db = root.join("state.sqlite");
    let mut map = std::collections::HashMap::new();
    if !db.exists() {
        return Ok(map);
    }
    let conn = crate::open_sqlite_with_retry(&db, 1)?;
    for sql in [
        "SELECT workflow_id, MAX(at) FROM workflow_events GROUP BY workflow_id",
        "SELECT task_id, MAX(at) FROM agent_routing GROUP BY task_id",
        "SELECT task_id, MAX(at) FROM orchestration_events GROUP BY task_id",
        "SELECT task_id, MAX(at) FROM events GROUP BY task_id",
    ] {
        if let Ok(mut stmt) = conn.prepare(sql)
            && let Ok(rows) = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
        {
            for entry in rows.flatten() {
                let ts = entry.1.max(0) as u64;
                map.entry(entry.0)
                    .and_modify(|existing: &mut u64| *existing = (*existing).max(ts))
                    .or_insert(ts);
            }
        }
    }
    Ok(map)
}

impl TaskService {
    pub fn open(root: &std::path::Path) -> Result<Self> {
        let orchestrator = Orchestrator::open(root)?;
        let store = Store::open(root)?;
        let root = store.root.clone();
        let agents = Agents::open(&root)?;
        let coordination = Coordination::open(&root)?;
        let stop = GlobalStop::open(&root)?;
        let workflows = crate::workflow::Workflows::open(&root)?;
        Ok(Self {
            store,
            orchestrator,
            agents,
            workflows,
            coordination,
            stop,
            root,
        })
    }

    pub fn stop_state(&self) -> Result<StopState> {
        self.stop.state()
    }

    /// Engages the global stop, then walks every task across both engines and requests
    /// cancellation on each through the existing, already-reviewed `cancel` (same method
    /// a human uses for one task). The stop is engaged *before* the sweep starts, so a
    /// task created or run by some other caller mid-sweep still lands under the active
    /// stop rather than racing it.
    pub fn stop_all(&mut self, by: &str, reason: &str) -> Result<StopAllOutcome> {
        let state = self.stop.engage(by, reason)?;
        let mut cancelled = Vec::new();
        let mut not_cancelled = Vec::new();
        for row in self.board()? {
            // A running task advances its own revision (progress, phase changes), so the
            // revision read here can be stale by the time the cancel lands. An emergency
            // stop must not give up on exactly the tasks that are busiest: re-read and
            // retry a bounded number of times before reporting it as not cancelled.
            let mut outcome = Err(anyhow::anyhow!("cancel not attempted"));
            for _ in 0..8 {
                outcome = self
                    .detail(&row.id, None)
                    .and_then(|detail| self.cancel(&row.id, detail.revision, Some(by)));
                match &outcome {
                    Err(error) if format!("{error:#}").contains("revision conflict") => {
                        std::thread::sleep(std::time::Duration::from_millis(25));
                    }
                    _ => break,
                }
            }
            match outcome {
                Ok(_) => cancelled.push(row.id),
                Err(error) => not_cancelled.push(format!("{}: {error}", row.id)),
            }
        }
        Ok(StopAllOutcome {
            state,
            cancelled,
            not_cancelled,
        })
    }

    pub fn resume(&mut self, by: &str, reason: &str) -> Result<StopState> {
        self.stop.disengage(by, reason)
    }

    fn repository_identity_of(&self, id: &str) -> Result<String> {
        if let Ok(task) = self.workflows.get(id) {
            Ok(task.assignment.profile.repository.identity)
        } else if let Ok(task) = self.orchestrator.task(id) {
            Ok(task.assignment.repository.identity)
        } else if let Ok(task) = self.agents.task(id) {
            Ok(task.assignment.repository.identity)
        } else {
            Ok(self.store.task(id)?.assignment.repository.identity)
        }
    }

    fn repository_of(&self, id: &str) -> Result<crate::repository::Repository> {
        if let Ok(task) = self.workflows.get(id) {
            Ok(task.assignment.profile.repository)
        } else if let Ok(task) = self.orchestrator.task(id) {
            Ok(task.assignment.repository)
        } else if let Ok(task) = self.agents.task(id) {
            Ok(task.assignment.repository)
        } else {
            Ok(self.store.task(id)?.assignment.repository)
        }
    }

    /// `task_id`/`depends_on` are resolved id/prefixes like every other `TaskService`
    /// entry point - either may belong to either engine, since dependencies are a
    /// cross-engine relation `Coordination` tracks independent of which engine owns
    /// either side. Self-dependency and cycle rejection live in `Coordination` itself.
    pub fn add_dependency(&mut self, task_id: &str, depends_on: &str) -> Result<()> {
        let task_id = self.resolve(task_id)?;
        let depends_on = self.resolve(depends_on)?;
        self.coordination.add_dependency(&task_id, &depends_on)
    }

    /// Creates a fresh `fake-process-v1` assignment, same as the CLI's plain `create` -
    /// exposed on `TaskService` specifically so `autonomy::hold_my_beer`'s dispatch loop
    /// creates tasks the same way everything else does, not a second copy. Deliberately
    /// has no Codex/orchestration equivalent: M7's first slice is fake-engine-only by
    /// design (see ADR 0009), so there's nothing for this to dispatch to otherwise.
    pub fn create_fake(
        &mut self,
        repository_id: &str,
        objective: String,
        acceptance: String,
        contents: String,
        budget: usize,
        execution_policy: crate::process::ExecutionPolicy,
    ) -> Result<board::Detail> {
        let task = self.store.create(
            repository_id,
            objective,
            acceptance,
            contents,
            budget,
            execution_policy,
        )?;
        self.fake_detail_of(task, None)
    }

    /// Current repository-lease reservations, held or quarantined. Read-only inspection,
    /// mirroring `inference-resources`.
    pub fn leases(&self) -> Result<Vec<serde_json::Value>> {
        self.coordination.reservations()
    }

    /// Explicit, human-labeled release of a quarantined lease - the repository-leasing
    /// equivalent of `inference-release-resource`, refusing while the owning process
    /// still looks alive (see `Coordination::acknowledge`).
    pub fn acknowledge_lease(&mut self, resource: &str, by: &str, reason: &str) -> Result<()> {
        self.coordination.acknowledge(resource, by, reason)
    }

    fn dependency_statuses(&self, id: &str) -> Result<Vec<board::DependencyStatus>> {
        self.coordination
            .dependencies_of(id)?
            .into_iter()
            .map(|dep| {
                let status = self.detail(&dep, None)?.status;
                Ok(board::DependencyStatus { id: dep, status })
            })
            .collect()
    }

    /// `run` refuses to dispatch while any dependency hasn't reached `accepted` - the
    /// enforcement point for "dependent tasks do not consume an unaccepted or failed
    /// result as established truth" (a cancelled or failed dependency never reaches
    /// `accepted`, so this alone also covers "cancellation stops downstream scheduling"
    /// without extra bookkeeping). Not enforced at `approve`: reviewing/signing off a
    /// dependent task's scope ahead of its dependency finishing is fine: only actually
    /// *running* it - where it would act as though the dependency's result already
    /// existed - is what needs blocking.
    fn ensure_dependencies_satisfied(&self, id: &str) -> Result<()> {
        for dep in self.dependency_statuses(id)? {
            ensure!(
                dep.status == "accepted",
                "depends on {} which is not yet accepted (current status: {})",
                dep.id,
                dep.status
            );
        }
        Ok(())
    }

    fn ids(&self) -> Result<Vec<String>> {
        let mut ids: Vec<String> = self
            .orchestrator
            .tasks()?
            .iter()
            .map(|t| t.id.clone())
            .collect();
        ids.extend(self.agents.tasks()?.iter().map(|t| t.id.clone()));
        ids.extend(self.store.tasks()?.iter().map(|t| t.id.clone()));
        ids.extend(self.workflows.list()?.iter().map(|t| t.id.clone()));
        Ok(ids)
    }

    fn resolve(&self, query: &str) -> Result<String> {
        board::resolve(&self.ids()?, query)
    }

    fn orchestrate_detail_of(
        &self,
        task: orchestration::Task,
        preview_bytes: Option<usize>,
    ) -> Result<board::Detail> {
        let digest = self.orchestrator.evidence_digest(&task).ok();
        let mut detail = board::orchestrate_detail(&task, digest)?;
        if let Some(max_bytes) = preview_bytes
            && let Some(last) = detail.attempts.last_mut()
        {
            last.previews = orchestrate_last_previews(&self.root, &task, max_bytes);
        }
        Ok(detail)
    }

    fn fake_detail_of(
        &self,
        task: crate::store::Task,
        preview_bytes: Option<usize>,
    ) -> Result<board::Detail> {
        let digest = if task.attempts.is_empty() {
            None
        } else {
            self.store.evidence_digest(&task).ok()
        };
        let mut detail = board::fake_detail(&task, digest)?;
        if let Some(max_bytes) = preview_bytes
            && let Some(last) = detail.attempts.last_mut()
        {
            last.previews = fake_last_previews(&self.root, &task, max_bytes);
        }
        Ok(detail)
    }

    fn agent_detail_of(
        &self,
        task: agents::AgentTask,
        preview_bytes: Option<usize>,
    ) -> Result<board::Detail> {
        let digest = self.agents.evidence_digest(&task).ok();
        let progress =
            crate::agent_progress::read(&self.root, &task.id, &task.assignment.spec.allowed_files);
        let mut detail = board::agent_detail(&task, digest)?;
        if let Some(last) = detail.attempts.last_mut()
            && last.started_at.is_none()
        {
            last.started_at = progress.as_ref().map(|p| p.started_at);
        }
        detail.progress = progress;
        detail.routing = self.agents.routing(&task.id)?;
        detail.parent_workflow_id = task
            .assignment
            .shared_workspace
            .as_ref()
            .map(|sw| sw.workflow_id.clone())
            .or_else(|| {
                self.workflows.list().ok().and_then(|list| {
                    list.into_iter().find_map(|w| {
                        let owns = w.steps.iter().any(|s| s.task_id == task.id)
                            || w.archived_steps.iter().any(|a| a.step.task_id == task.id);
                        owns.then_some(w.id)
                    })
                })
            });
        if let Some(max_bytes) = preview_bytes
            && let Some(last) = detail.attempts.last_mut()
        {
            last.previews = agent_last_previews(&self.root, &task, max_bytes);
        }
        Ok(detail)
    }

    fn live_overtime_alert(&self, task: &agents::AgentTask) -> Option<String> {
        if !matches!(task.status, AgentStatus::Running | AgentStatus::Verifying) {
            return None;
        }
        let progress =
            crate::agent_progress::read(&self.root, &task.id, &task.assignment.spec.allowed_files)?;
        if !progress.overtime || progress.finished {
            return None;
        }
        let phase_label = if progress.phase == "verification" {
            "Verification"
        } else {
            "Execution"
        };
        Some(format!(
            "{phase_label} passed {} target (+{} overtime)",
            format_duration_short(progress.hard_limit_ms),
            format_duration_short(progress.overtime_ms)
        ))
    }

    pub fn board(&self) -> Result<Vec<board::Row>> {
        let mut rows: Vec<board::Row> = self
            .orchestrator
            .tasks()?
            .iter()
            .map(board::orchestrate_row)
            .collect();
        let agent_tasks = self.agents.tasks()?;
        let agent_map: HashMap<&str, &agents::AgentTask> =
            agent_tasks.iter().map(|t| (t.id.as_str(), t)).collect();
        let workflows = self.workflows.list()?;
        let workflow_ids: HashSet<&str> = workflows.iter().map(|w| w.id.as_str()).collect();
        let mut owned_agent_ids: HashSet<String> = HashSet::new();
        for w in &workflows {
            for step in &w.steps {
                owned_agent_ids.insert(step.task_id.clone());
            }
            for archived in &w.archived_steps {
                owned_agent_ids.insert(archived.step.task_id.clone());
            }
        }
        for t in &agent_tasks {
            if t.assignment
                .shared_workspace
                .as_ref()
                .is_some_and(|sw| workflow_ids.contains(sw.workflow_id.as_str()))
            {
                owned_agent_ids.insert(t.id.clone());
            }
        }
        rows.extend(
            agent_tasks
                .iter()
                .filter(|t| !owned_agent_ids.contains(&t.id))
                .map(|t| {
                    let mut row = board::agent_row(t);
                    row.alert = self.live_overtime_alert(t);
                    row
                }),
        );
        rows.extend(self.store.tasks()?.iter().map(board::fake_row));
        rows.extend(workflows.iter().map(|w| {
            let wf_status = crate::workflow::status(&w.status);
            let wf_live = is_live_task_status(&wf_status);
            let mut wf_alert: Option<String> = None;
            let children: Vec<board::Row> = w
                .steps
                .iter()
                .enumerate()
                .filter_map(|(idx, step)| {
                    let agent_task = agent_map.get(step.task_id.as_str())?;
                    let mut child = board::agent_row(agent_task);
                    child.step_label = Some(format!("S{:02} · {}", idx + 1, step.worker));
                    child.alert = self.live_overtime_alert(agent_task);
                    if wf_live && child.alert.is_some() {
                        wf_alert = child.alert.clone();
                    }
                    if let Some(res) = &step.result {
                        let is_quota = res
                            .get("quota_exhausted")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        if is_quota {
                            child.status = "quota_fallback".into();
                        } else if child.status == "awaiting_review" {
                            let agentlet_outcome = res
                                .get("agentlet_result")
                                .and_then(|a| a.get("outcome"))
                                .and_then(|v| v.as_str());
                            let ver_outcome =
                                res.get("verification_outcome").and_then(|v| v.as_str());
                            child.status = match agentlet_outcome {
                                Some("needs_work" | "blocked") => "needs_work".into(),
                                _ => match ver_outcome {
                                    Some("failed" | "timed_out") => "failed".into(),
                                    _ => "completed".into(),
                                },
                            };
                        }
                    }
                    Some(child)
                })
                .collect();
            board::Row {
                kind: "workflow".into(),
                id: w.id.clone(),
                status: wf_status,
                backend: "workflow-v1".into(),
                units: w.steps.len(),
                review: w.review.as_ref().map(|r| {
                    if r.accepted {
                        "accepted".into()
                    } else {
                        "rejected".into()
                    }
                }),
                objective: w.assignment.objective.clone(),
                step_label: None,
                alert: wf_alert,
                children,
            }
        }));
        let timestamps = latest_activity_timestamps(&self.root).unwrap_or_default();
        rows.sort_by(|a, b| {
            let ta = timestamps.get(&a.id).copied().unwrap_or(0);
            let tb = timestamps.get(&b.id).copied().unwrap_or(0);
            tb.cmp(&ta).then_with(|| a.id.cmp(&b.id))
        });
        Ok(rows)
    }

    /// `preview_bytes`: `None` skips reading any artifact; `Some(n)` caps each preview at `n` bytes.
    pub fn detail(&self, query: &str, preview_bytes: Option<usize>) -> Result<board::Detail> {
        let id = self.resolve(query)?;
        let mut detail = if self.workflows.get(&id).is_ok() {
            self.workflows.detail(&id, preview_bytes)
        } else if let Ok(task) = self.orchestrator.task(&id) {
            self.orchestrate_detail_of(task, preview_bytes)
        } else if let Ok(task) = self.agents.task(&id) {
            self.agent_detail_of(task, preview_bytes)
        } else {
            self.fake_detail_of(self.store.task(&id)?, preview_bytes)
        }?;
        detail.depends_on = self.dependency_statuses(&id)?;
        detail.integration = crate::integration::Integrations::open(&self.root)?.get(&id)?;
        // Only while actually reviewable - review is the one place this matters, and a
        // fresh `git rev-parse` for every other status would be pure overhead. Treats a
        // failed inspection (e.g. checkout removed) as "unknown, show nothing" rather than
        // failing the whole detail view over it, same as `evidence_digest` elsewhere here.
        if detail.status == "awaiting_review" {
            detail.conflict = self
                .repository_of(&id)
                .ok()
                .and_then(|repo| repo.drift().ok())
                .flatten()
                .map(|drift| drift.describe());
        }
        Ok(detail)
    }

    pub fn prepare_integration(&self, query: &str, by: &str) -> Result<board::Detail> {
        self.prepare_integration_with_target(
            query,
            by,
            crate::integration::IntegrationTarget::Checkout,
        )
    }

    pub fn prepare_integration_with_target(
        &self,
        query: &str,
        by: &str,
        target: crate::integration::IntegrationTarget,
    ) -> Result<board::Detail> {
        let id = self.resolve(query)?;
        ensure!(
            self.workflows.get(&id).is_ok() || self.agents.task(&id).is_ok(),
            "source integration supports accepted workflows and CLI-agent tasks"
        );
        crate::integration::Integrations::open(&self.root)?.prepare_with_target(&id, by, target)?;
        self.detail(&id, None)
    }

    pub fn integrate(
        &self,
        query: &str,
        revision: u64,
        digest: &str,
        by: &str,
        rollback: bool,
    ) -> Result<board::Detail> {
        let id = self.resolve(query)?;
        let mut integrations = crate::integration::Integrations::open(&self.root)?;
        if rollback {
            integrations.rollback(&id, revision, digest, by)?;
        } else {
            integrations.apply(&id, revision, digest, by)?;
        }
        self.detail(&id, None)
    }

    /// M12.0 Phase 6: the real file behind one of `board::Preview`'s known labels
    /// ("diff"/"verifier stdout"/"verifier stderr") for a task's single most recent
    /// attempt/round - the same "latest only" scope the existing bounded previews already
    /// use (see `fake_last_previews`'s own doc comment). Returns the path plus the
    /// artifact's own recorded total size and capture-time truncation flag, so a caller
    /// reading a further chunk from it can report both honestly rather than just
    /// re-deriving them from a fresh `fs::metadata` (which, for a capture-truncated
    /// stream, would under-report the true original size).
    pub fn artifact_path(
        &self,
        query: &str,
        label: &str,
    ) -> Result<(std::path::PathBuf, u64, bool)> {
        let id = self.resolve(query)?;
        if let Ok(task) = self.workflows.get(&id) {
            let step = task
                .steps
                .last()
                .context("workflow has no worker artifacts")?;
            return self.artifact_path(&step.task_id, label);
        }
        if let Ok(task) = self.orchestrator.task(&id) {
            let round = task.rounds.last().context("task has no rounds yet")?;
            return match label {
                "diff" => {
                    let a = round.patch.as_ref().context("round has no diff yet")?;
                    Ok((a.path.clone(), a.bytes as u64, false))
                }
                "verifier stdout" => {
                    let r = round
                        .verification
                        .as_ref()
                        .context("round has no verification yet")?;
                    Ok((
                        r.stdout.artifact.path.clone(),
                        r.stdout.total_bytes,
                        r.stdout.truncated,
                    ))
                }
                "verifier stderr" => {
                    let r = round
                        .verification
                        .as_ref()
                        .context("round has no verification yet")?;
                    Ok((
                        r.stderr.artifact.path.clone(),
                        r.stderr.total_bytes,
                        r.stderr.truncated,
                    ))
                }
                other => bail!("unknown artifact label '{other}'"),
            };
        }
        if let Ok(task) = self.agents.task(&id) {
            return match label {
                "diff" => {
                    let a = task.patch.as_ref().context("task has no diff yet")?;
                    Ok((a.path.clone(), a.bytes as u64, false))
                }
                "verifier stdout" => {
                    let r = task
                        .verification
                        .as_ref()
                        .context("task has no verification yet")?;
                    Ok((
                        r.stdout.artifact.path.clone(),
                        r.stdout.total_bytes,
                        r.stdout.truncated,
                    ))
                }
                "verifier stderr" => {
                    let r = task
                        .verification
                        .as_ref()
                        .context("task has no verification yet")?;
                    Ok((
                        r.stderr.artifact.path.clone(),
                        r.stderr.total_bytes,
                        r.stderr.truncated,
                    ))
                }
                other => bail!("unknown artifact label '{other}'"),
            };
        }
        let task = self.store.task(&id)?;
        let attempt = task.attempts.last().context("task has no attempts yet")?;
        match label {
            "diff" => {
                let a = attempt.diff.as_ref().context("attempt has no diff yet")?;
                Ok((a.path.clone(), a.bytes as u64, false))
            }
            "verifier stdout" => {
                let r = attempt
                    .receipt
                    .as_ref()
                    .and_then(|r| r.process_receipt.as_ref())
                    .context("attempt has no verification yet")?;
                Ok((
                    r.stdout.artifact.path.clone(),
                    r.stdout.total_bytes,
                    r.stdout.truncated,
                ))
            }
            "verifier stderr" => {
                let r = attempt
                    .receipt
                    .as_ref()
                    .and_then(|r| r.process_receipt.as_ref())
                    .context("attempt has no verification yet")?;
                Ok((
                    r.stderr.artifact.path.clone(),
                    r.stderr.total_bytes,
                    r.stderr.truncated,
                ))
            }
            other => bail!("unknown artifact label '{other}'"),
        }
    }

    pub fn approve(
        &mut self,
        query: &str,
        revision: u64,
        digest: &str,
        by: &str,
    ) -> Result<board::Detail> {
        let id = self.resolve(query)?;
        if self.workflows.get(&id).is_ok() {
            self.workflows.approve(&id, revision, digest, by)?;
            self.workflows.detail(&id, None)
        } else if self.orchestrator.task(&id).is_ok() {
            let task = self.orchestrator.approve(&id, revision, digest, by)?;
            self.orchestrate_detail_of(task, None)
        } else if self.agents.task(&id).is_ok() {
            self.agents.routing_note(
                &id,
                "assignment_approval_requested",
                "TaskService::approve",
                None,
                None,
            )?;
            let task = self.agents.approve(&id, revision, digest, by)?;
            self.agent_detail_of(task, None)
        } else {
            let task = self.store.approve(&id, revision, digest, by)?;
            self.fake_detail_of(task, None)
        }
    }

    /// Gated two ways before any owned child process is spawned: `ensure_dependencies_satisfied`
    /// (blocks a dependent task from running ahead of an unaccepted dependency) and a
    /// `Coordination` repository lease (blocks two tasks against the *same* repository
    /// from running at once, while leaving different repositories free to run
    /// concurrently - M6's "parallelize work that can be isolated; serialize integration
    /// where changes overlap"). The lease is acquired and released around the *existing*
    /// dispatch logic unchanged below, not threaded into it, so `Store`/`Orchestrator`
    /// stay unaware this exists - exactly why `TaskService` was extracted in the first
    /// place (see its module doc comment).
    pub fn run(&mut self, query: &str, revision: u64) -> Result<RunOutcome> {
        self.stop.state()?.ensure_admission_allowed()?;
        let id = self.resolve(query)?;
        // Agent tasks acquire and release their own repository lease inside
        // `Agents::run` (see its own doc comment) - wrapping it in a second
        // `LeaseReservation` here would be a double-acquire against the same
        // resource/task id and fail every time. Dependencies apply to every
        // engine; only the service-owned lease below is skipped for Agents.
        self.ensure_dependencies_satisfied(&id)?;
        if self.workflows.get(&id).is_ok() {
            let task = self.workflows.run(&id, revision)?;
            return Ok(RunOutcome {
                detail: self.workflows.detail(&id, None)?,
                succeeded: task.status == AgentStatus::AwaitingReview,
            });
        }
        if self.agents.task(&id).is_ok() {
            self.agents
                .routing_note(&id, "dispatch_requested", "TaskService::run", None, None)?;
            let task = self.agents.run(&id, revision)?;
            let succeeded = task.status == AgentStatus::AwaitingReview;
            return Ok(RunOutcome {
                detail: self.agent_detail_of(task, None)?,
                succeeded,
            });
        }
        let resource = self.repository_identity_of(&id)?;
        let root = self.root.clone();
        with_repository_lease(&root, resource, id.clone(), || {
            if self.orchestrator.task(&id).is_ok() {
                let task = self.orchestrator.run(&id, revision)?;
                let succeeded = task.status == orchestration::Status::AwaitingReview;
                Ok(RunOutcome {
                    detail: self.orchestrate_detail_of(task, None)?,
                    succeeded,
                })
            } else {
                let task = if self.store.task(&id)?.assignment.backend == "codex-exec-v1" {
                    CodexEngine.execute(&mut self.store, &id, revision)?
                } else {
                    self.store.run(&id, revision)?
                };
                let succeeded = !matches!(
                    task.status,
                    FakeStatus::Failed
                        | FakeStatus::Cancelled
                        | FakeStatus::TimedOut
                        | FakeStatus::Unknown
                );
                Ok(RunOutcome {
                    detail: self.fake_detail_of(task, None)?,
                    succeeded,
                })
            }
        })
    }

    pub fn cancel(
        &mut self,
        query: &str,
        revision: u64,
        by: Option<&str>,
    ) -> Result<board::Detail> {
        let id = self.resolve(query)?;
        if self.workflows.get(&id).is_ok() {
            self.workflows.cancel(&id, revision)?;
            self.workflows.detail(&id, None)
        } else if self.orchestrator.task(&id).is_ok() {
            let task = self.orchestrator.cancel(&id, revision)?;
            self.orchestrate_detail_of(task, None)
        } else if self.agents.task(&id).is_ok() {
            let task = self.agents.cancel(&id, revision)?;
            self.agent_detail_of(task, None)
        } else {
            let by = by.context("cancelling a fake/Codex task requires an actor (--by)")?;
            let task = self.store.cancel(&id, revision, by)?;
            self.fake_detail_of(task, None)
        }
    }

    pub fn reconcile(&mut self, query: &str, revision: u64) -> Result<board::Detail> {
        let id = self.resolve(query)?;
        if self.workflows.get(&id).is_ok() {
            self.workflows.reconcile(&id, revision)?;
            self.workflows.detail(&id, None)
        } else if self.orchestrator.task(&id).is_ok() {
            let task = self.orchestrator.reconcile(&id, revision)?;
            self.orchestrate_detail_of(task, None)
        } else if self.agents.task(&id).is_ok() {
            let task = self.agents.reconcile(&id, revision)?;
            self.agent_detail_of(task, None)
        } else {
            let task = self.store.reconcile(&id, revision)?;
            self.fake_detail_of(task, None)
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn retry(
        &mut self,
        query: &str,
        revision: u64,
        by: &str,
        reset_workspace: bool,
        from_step: Option<usize>,
        extra_steps: Option<usize>,
        guidance: Option<String>,
    ) -> Result<board::Detail> {
        self.stop.state()?.ensure_admission_allowed()?;
        let id = self.resolve(query)?;
        if self.workflows.get(&id).is_ok() {
            self.workflows.retry(
                &id,
                revision,
                by,
                reset_workspace,
                from_step,
                extra_steps,
                guidance,
            )?;
            self.workflows.detail(&id, None)
        } else if self.orchestrator.task(&id).is_ok() {
            let task = self.orchestrator.retry(&id, revision, by)?;
            self.orchestrate_detail_of(task, None)
        } else if self.agents.task(&id).is_ok() {
            self.agents
                .routing_note(&id, "retry_requested", "TaskService::retry", None, None)?;
            let task = self.agents.retry(&id, revision, by, reset_workspace)?;
            self.agent_detail_of(task, None)
        } else {
            let task = self.store.retry(&id, revision, by)?;
            self.fake_detail_of(task, None)
        }
    }
}

/// `TaskService::board` / `POST /api/tasks/clear` category filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum ClearScope {
    /// Failed, timed out, rejected, cancelled, needs_work, or unknown runs.
    Failed,
    /// Accepted runs.
    Done,
    /// Both `Failed` and `Done` stopped runs (never touches live or pending items).
    Finished,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClearSkipped {
    pub id: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClearOutcome {
    pub scope: ClearScope,
    pub deleted_ids: Vec<String>,
    pub skipped: Vec<ClearSkipped>,
}

fn enum_status_str<S: Serialize>(status: &S) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".into())
}

fn is_live_task_status(status: &str) -> bool {
    matches!(
        status,
        "running" | "verifying" | "preparing_workspace" | "generating" | "applying"
    )
}

fn format_duration_short(ms: u64) -> String {
    let secs = ms / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

fn matches_clear_scope(status: &str, scope: ClearScope) -> bool {
    let is_failed = matches!(
        status,
        "failed" | "timed_out" | "rejected" | "cancelled" | "needs_work" | "unknown"
    );
    let is_done = status == "accepted";
    match scope {
        ClearScope::Failed => is_failed,
        ClearScope::Done => is_done,
        ClearScope::Finished => is_failed || is_done,
    }
}

fn table_exists(tx: &Transaction<'_>, name: &str) -> Result<bool> {
    let found: Option<String> = tx
        .query_row(
            "SELECT name FROM sqlite_master WHERE type='table' AND name=?1",
            [name],
            |r| r.get(0),
        )
        .optional()?;
    Ok(found.is_some())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TaskEngine {
    Workflow,
    Agent,
    Orchestration,
    Store,
}

struct TaskRecordMeta {
    id: String,
    engine: TaskEngine,
    status: String,
    live: bool,
    repository: PathBuf,
    owned_children: Vec<String>,
    attempt_ids: Vec<String>,
}

struct DeletionSnapshot {
    records: BTreeMap<String, TaskRecordMeta>,
    child_parents: HashMap<String, Vec<String>>,
    leased_ids: HashSet<String>,
    dependencies: Vec<(String, String)>,
    busy_integration_ids: HashSet<String>,
    active_policy_tasks: HashMap<String, String>,
}

struct CleanupPlan {
    dirs: Vec<PathBuf>,
    workflow_refs: Vec<(PathBuf, String)>,
    prune_repos: BTreeSet<PathBuf>,
}

impl DeletionSnapshot {
    fn load(tx: &Transaction<'_>) -> Result<Self> {
        let mut records = BTreeMap::new();
        let mut child_parents: HashMap<String, Vec<String>> = HashMap::new();

        if table_exists(tx, "workflows")? {
            let mut stmt = tx.prepare("SELECT record FROM workflows")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            for row in rows {
                let w: crate::workflow::Workflow = serde_json::from_str(&row?)?;
                let status = enum_status_str(&w.status);
                let mut children: Vec<String> = w.steps.iter().map(|s| s.task_id.clone()).collect();
                for archived in &w.archived_steps {
                    if !children.contains(&archived.step.task_id) {
                        children.push(archived.step.task_id.clone());
                    }
                }
                for child_id in &children {
                    child_parents
                        .entry(child_id.clone())
                        .or_default()
                        .push(w.id.clone());
                }
                records.insert(
                    w.id.clone(),
                    TaskRecordMeta {
                        id: w.id,
                        engine: TaskEngine::Workflow,
                        live: is_live_task_status(&status),
                        status,
                        repository: w.assignment.profile.repository.checkout,
                        owned_children: children,
                        attempt_ids: Vec::new(),
                    },
                );
            }
        }

        if table_exists(tx, "agent_tasks")? {
            let mut stmt = tx.prepare("SELECT record FROM agent_tasks")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            for row in rows {
                let t: agents::AgentTask = serde_json::from_str(&row?)?;
                let status = enum_status_str(&t.status);
                records.insert(
                    t.id.clone(),
                    TaskRecordMeta {
                        id: t.id,
                        engine: TaskEngine::Agent,
                        live: is_live_task_status(&status),
                        status,
                        repository: t.assignment.repository.checkout,
                        owned_children: Vec::new(),
                        attempt_ids: Vec::new(),
                    },
                );
            }
        }

        if table_exists(tx, "orchestrations")? {
            let mut stmt = tx.prepare("SELECT record FROM orchestrations")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            for row in rows {
                let t: orchestration::Task = serde_json::from_str(&row?)?;
                let status = enum_status_str(&t.status);
                records.insert(
                    t.id.clone(),
                    TaskRecordMeta {
                        id: t.id,
                        engine: TaskEngine::Orchestration,
                        live: is_live_task_status(&status) || t.owner_token.is_some(),
                        status,
                        repository: t.assignment.repository.checkout,
                        owned_children: Vec::new(),
                        attempt_ids: Vec::new(),
                    },
                );
            }
        }

        if table_exists(tx, "tasks")? {
            let mut stmt = tx.prepare("SELECT record FROM tasks")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            for row in rows {
                let t: crate::store::Task = serde_json::from_str(&row?)?;
                let status = enum_status_str(&t.status);
                records.insert(
                    t.id.clone(),
                    TaskRecordMeta {
                        id: t.id,
                        engine: TaskEngine::Store,
                        live: is_live_task_status(&status),
                        status,
                        repository: t.assignment.repository.checkout,
                        owned_children: Vec::new(),
                        attempt_ids: t.attempts.into_iter().map(|a| a.id).collect(),
                    },
                );
            }
        }

        let mut leased_ids = HashSet::new();
        if table_exists(tx, "leases")? {
            let mut stmt = tx.prepare("SELECT task_id FROM leases")?;
            for row in stmt.query_map([], |r| r.get::<_, String>(0))? {
                leased_ids.insert(row?);
            }
        }

        let mut dependencies = Vec::new();
        if table_exists(tx, "dependencies")? {
            let mut stmt = tx.prepare("SELECT task_id, depends_on FROM dependencies")?;
            for row in
                stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            {
                dependencies.push(row?);
            }
        }

        let mut busy_integration_ids = HashSet::new();
        if table_exists(tx, "source_integrations")? {
            let mut stmt = tx.prepare(
                "SELECT task_id, json_extract(record, '$.status') FROM source_integrations",
            )?;
            for row in stmt.query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
            })? {
                let (task_id, status) = row?;
                if matches!(
                    status.as_deref(),
                    Some("applying" | "rolling_back" | "unknown")
                ) {
                    busy_integration_ids.insert(task_id);
                }
            }
        }

        let mut active_policy_tasks = HashMap::new();
        if table_exists(tx, "policies")? {
            let now = crate::now();
            let mut stmt = tx.prepare("SELECT record FROM policies")?;
            for row in stmt.query_map([], |r| r.get::<_, String>(0))? {
                let policy: Policy = serde_json::from_str(&row?)?;
                if policy.state(now) == PolicyState::Active {
                    for exp in &policy.experiments {
                        if let Some(tid) = &exp.task_id {
                            active_policy_tasks.insert(tid.clone(), policy.id.clone());
                        }
                    }
                    for real in &policy.real_tasks {
                        if let Some(tid) = &real.task_id {
                            active_policy_tasks.insert(tid.clone(), policy.id.clone());
                        }
                    }
                }
            }
        }

        Ok(Self {
            records,
            child_parents,
            leased_ids,
            dependencies,
            busy_integration_ids,
            active_policy_tasks,
        })
    }

    fn direct_blocker_for_id(&self, id: &str) -> Option<String> {
        if let Some(meta) = self.records.get(id)
            && meta.live
        {
            return Some(format!(
                "task {id} is currently active (status: {})",
                meta.status
            ));
        }
        if self.leased_ids.contains(id) {
            return Some(format!("task {id} still holds a repository lease"));
        }
        if self.busy_integration_ids.contains(id) {
            return Some(format!(
                "task {id} has a source integration requiring recovery first"
            ));
        }
        if let Some(policy_id) = self.active_policy_tasks.get(id) {
            return Some(format!(
                "task {id} is referenced by active policy {policy_id}"
            ));
        }
        None
    }

    /// Returns a reason why deleting `top_id` (plus its owned children) is blocked given the
    /// full set of IDs currently planned for deletion (`selected`).
    fn blocker_for_unit(&self, top_id: &str, selected: &HashSet<String>) -> Option<String> {
        let Some(meta) = self.records.get(top_id) else {
            return Some(format!("unknown task '{top_id}'"));
        };
        let mut unit_ids = Vec::with_capacity(1 + meta.owned_children.len());
        unit_ids.push(meta.id.as_str());
        for child_id in &meta.owned_children {
            unit_ids.push(child_id.as_str());
        }

        for id in &unit_ids {
            if let Some(reason) = self.direct_blocker_for_id(id) {
                return Some(reason);
            }
        }
        for (dependent_id, dep_on_id) in &self.dependencies {
            if unit_ids.iter().any(|id| *id == dep_on_id)
                && !selected.contains(dependent_id)
                && self.records.contains_key(dependent_id)
            {
                return Some(format!(
                    "surviving task {dependent_id} depends on {dep_on_id}"
                ));
            }
        }
        if meta.engine == TaskEngine::Agent
            && let Some(parents) = self.child_parents.get(top_id)
        {
            for parent_id in parents {
                if !selected.contains(parent_id) && self.records.contains_key(parent_id) {
                    return Some(format!(
                        "task {top_id} belongs to workflow {parent_id}; delete or clear the parent workflow instead"
                    ));
                }
            }
        }
        None
    }

    fn delete_selected(
        &self,
        tx: &Transaction<'_>,
        root: &Path,
        selected: &HashSet<String>,
    ) -> Result<CleanupPlan> {
        let has_workflow_events = table_exists(tx, "workflow_events")?;
        let has_workflows = table_exists(tx, "workflows")?;
        let has_agent_routing = table_exists(tx, "agent_routing")?;
        let has_agent_tasks = table_exists(tx, "agent_tasks")?;
        let has_orchestration_events = table_exists(tx, "orchestration_events")?;
        let has_orchestrations = table_exists(tx, "orchestrations")?;
        let has_events = table_exists(tx, "events")?;
        let has_tasks = table_exists(tx, "tasks")?;
        let has_dependencies = table_exists(tx, "dependencies")?;
        let has_integrations = table_exists(tx, "source_integrations")?;
        let has_mcp_meta = table_exists(tx, "mcp_task_metadata")?;
        let has_mcp_idemp = table_exists(tx, "mcp_idempotency")?;

        let mut dirs = Vec::new();
        let mut workflow_refs = Vec::new();
        let mut prune_repos = BTreeSet::new();

        for id in selected {
            let Some(meta) = self.records.get(id) else {
                continue;
            };
            prune_repos.insert(meta.repository.clone());
            match meta.engine {
                TaskEngine::Workflow => {
                    if has_workflow_events {
                        tx.execute("DELETE FROM workflow_events WHERE workflow_id=?1", [id])?;
                    }
                    if has_workflows {
                        tx.execute("DELETE FROM workflows WHERE id=?1", [id])?;
                    }
                    dirs.push(root.join("workflow-artifacts").join(id));
                    dirs.push(root.join("workflow-workspaces").join(id));
                    dirs.push(root.join("workflow-inference").join(id));
                    workflow_refs.push((meta.repository.clone(), id.clone()));
                }
                TaskEngine::Agent => {
                    if has_agent_routing {
                        tx.execute("DELETE FROM agent_routing WHERE task_id=?1", [id])?;
                    }
                    if has_agent_tasks {
                        tx.execute("DELETE FROM agent_tasks WHERE id=?1", [id])?;
                    }
                    dirs.push(root.join("agent-artifacts").join(id));
                    dirs.push(root.join("agent-workspaces").join(id));
                }
                TaskEngine::Orchestration => {
                    if has_orchestration_events {
                        tx.execute("DELETE FROM orchestration_events WHERE task_id=?1", [id])?;
                    }
                    if has_orchestrations {
                        tx.execute("DELETE FROM orchestrations WHERE id=?1", [id])?;
                    }
                    dirs.push(root.join("orchestration-artifacts").join(id));
                    dirs.push(root.join("orchestration-workspaces").join(id));
                    dirs.push(root.join("orchestration-inference").join(id));
                }
                TaskEngine::Store => {
                    if has_events {
                        tx.execute("DELETE FROM events WHERE task_id=?1", [id])?;
                    }
                    if has_tasks {
                        tx.execute("DELETE FROM tasks WHERE id=?1", [id])?;
                    }
                    dirs.push(root.join("workspaces").join(id));
                    for attempt_id in &meta.attempt_ids {
                        dirs.push(root.join("artifacts").join(attempt_id));
                    }
                }
            }
            if has_dependencies {
                tx.execute(
                    "DELETE FROM dependencies WHERE task_id=?1 OR depends_on=?1",
                    [id],
                )?;
            }
            if has_integrations {
                tx.execute("DELETE FROM source_integrations WHERE task_id=?1", [id])?;
            }
            if has_mcp_meta {
                tx.execute("DELETE FROM mcp_task_metadata WHERE task_id=?1", [id])?;
            }
            if has_mcp_idemp {
                tx.execute("DELETE FROM mcp_idempotency WHERE task_id=?1", [id])?;
            }
        }

        Ok(CleanupPlan {
            dirs,
            workflow_refs,
            prune_repos,
        })
    }
}

fn cleanup_artifacts_after_commit(root: &Path, plan: CleanupPlan) {
    let canonical_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    for dir in plan.dirs {
        if !dir.exists() {
            continue;
        }
        let within_root = dir.starts_with(root)
            || std::fs::canonicalize(&dir).is_ok_and(|c| c.starts_with(&canonical_root));
        if within_root {
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
    for (repo, wf_id) in plan.workflow_refs {
        if !repo.exists() {
            continue;
        }
        let prefix = format!("refs/conductor/workflows/{wf_id}/");
        if let Ok(output) = git_text(&repo, &["for-each-ref", "--format=%(refname)", &prefix]) {
            for reference in output.lines() {
                let reference = reference.trim();
                if !reference.is_empty() {
                    let _ = git(&repo, &["update-ref", "-d", reference]);
                }
            }
        }
    }
    for repo in plan.prune_repos {
        if repo.exists() {
            let _ = git(&repo, &["worktree", "prune"]);
        }
    }
}

impl TaskService {
    pub fn review(
        &mut self,
        query: &str,
        revision: u64,
        digest: &str,
        by: &str,
        accept: bool,
    ) -> Result<board::Detail> {
        let id = self.resolve(query)?;
        if self.workflows.get(&id).is_ok() {
            self.workflows.review(&id, revision, digest, by, accept)?;
            self.workflows.detail(&id, None)
        } else if self.orchestrator.task(&id).is_ok() {
            let task = self
                .orchestrator
                .review(&id, revision, digest, by, accept)?;
            self.orchestrate_detail_of(task, None)
        } else if self.agents.task(&id).is_ok() {
            self.agents.routing_note(
                &id,
                "review_requested",
                "TaskService::review",
                None,
                Some(if accept {
                    "accept requested"
                } else {
                    "reject requested"
                }),
            )?;
            let task = self.agents.review(&id, revision, digest, by, accept)?;
            self.agent_detail_of(task, None)
        } else {
            let task = self.store.review(&id, revision, digest, by, accept)?;
            self.fake_detail_of(task, None)
        }
    }

    pub fn delete_task(&mut self, query: &str) -> Result<String> {
        let id = self.resolve(query)?;
        let mut conn = crate::open_sqlite_with_retry(&self.root.join("state.sqlite"), 1)?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let snapshot = DeletionSnapshot::load(&tx)?;
        let Some(meta) = snapshot.records.get(&id) else {
            bail!("no task id matches '{query}'");
        };
        let mut selected = HashSet::new();
        selected.insert(id.clone());
        for child_id in &meta.owned_children {
            selected.insert(child_id.clone());
        }
        if let Some(reason) = snapshot.blocker_for_unit(&id, &selected) {
            bail!("cannot delete task: {reason}");
        }
        let plan = snapshot.delete_selected(&tx, &self.root, &selected)?;
        tx.commit()?;
        cleanup_artifacts_after_commit(&self.root, plan);
        Ok(id)
    }

    pub fn clear_tasks(&mut self, scope: ClearScope) -> Result<ClearOutcome> {
        let mut conn = crate::open_sqlite_with_retry(&self.root.join("state.sqlite"), 1)?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let snapshot = DeletionSnapshot::load(&tx)?;

        // Initial top-level candidates: workflows, orchestrations, store tasks, and standalone
        // agent tasks matching `scope`. Workflow-owned children follow their parent workflow.
        let mut top_candidates: BTreeSet<String> = BTreeSet::new();
        let mut selected: HashSet<String> = HashSet::new();
        for meta in snapshot.records.values() {
            let is_owned_child =
                meta.engine == TaskEngine::Agent && snapshot.child_parents.contains_key(&meta.id);
            if is_owned_child || !matches_clear_scope(&meta.status, scope) {
                continue;
            }
            top_candidates.insert(meta.id.clone());
            selected.insert(meta.id.clone());
            for child_id in &meta.owned_children {
                selected.insert(child_id.clone());
            }
        }

        // Also drop any surviving dependency edge effects by removing blocked candidates until
        // a fixed point is reached.
        let mut skipped = Vec::new();
        loop {
            let mut to_remove: Vec<(String, String)> = Vec::new();
            for top_id in &top_candidates {
                if let Some(reason) = snapshot.blocker_for_unit(top_id, &selected) {
                    to_remove.push((top_id.clone(), reason));
                }
            }
            if to_remove.is_empty() {
                break;
            }
            for (top_id, reason) in to_remove {
                top_candidates.remove(&top_id);
                selected.remove(&top_id);
                if let Some(meta) = snapshot.records.get(&top_id) {
                    for child_id in &meta.owned_children {
                        selected.remove(child_id);
                    }
                }
                skipped.push(ClearSkipped { id: top_id, reason });
            }
        }

        let plan = snapshot.delete_selected(&tx, &self.root, &selected)?;
        tx.commit()?;
        cleanup_artifacts_after_commit(&self.root, plan);

        let mut deleted_ids: Vec<String> = selected.into_iter().collect();
        deleted_ids.sort();
        Ok(ClearOutcome {
            scope,
            deleted_ids,
            skipped,
        })
    }
}
