//! Read-only cross-engine task board/detail view over `Store` and `Orchestrator` state.
//! No mutation, no new persistence: this renders what the existing engines already record.

use crate::{agents, orchestration, store};
use anyhow::{Result, bail};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Row {
    pub kind: String,
    pub id: String,
    pub status: String,
    pub backend: String,
    pub units: usize,
    pub review: Option<String>,
    pub objective: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alert: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<Row>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AttemptDetail {
    pub id: String,
    pub started_at: Option<u64>,
    pub ended_at: Option<u64>,
    pub outcome: Option<String>,
    pub error: Option<String>,
    #[serde(default)]
    pub previews: Vec<Preview>,
}

/// A bounded slice of a captured artifact's content (diff, verifier stdout/stderr).
/// Built from bytes the caller already read; this type does no I/O of its own.
#[derive(Debug, Clone, Serialize)]
pub struct Preview {
    pub label: String,
    pub path: String,
    pub shown_bytes: usize,
    pub total_bytes: u64,
    pub truncated: bool,
    pub text: String,
}

/// Format already-read bytes into a bounded `Preview`. `total_bytes` and
/// `capture_truncated` come from the artifact's own recorded metadata, not a fresh stat.
pub fn build_preview(
    label: &str,
    path: &str,
    bytes: &[u8],
    total_bytes: u64,
    capture_truncated: bool,
) -> Preview {
    Preview {
        label: label.to_string(),
        path: path.to_string(),
        shown_bytes: bytes.len(),
        total_bytes,
        truncated: capture_truncated || (bytes.len() as u64) < total_bytes,
        text: String::from_utf8_lossy(bytes).into_owned(),
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Detail {
    pub kind: String,
    pub id: String,
    pub revision: u64,
    /// The digest `board-approve`/`POST .../approve` must be given back unchanged - it's
    /// bound to the exact current assignment, not to the evidence an accepted attempt
    /// produced (that's `evidence_digest`, used by review instead). Without this field
    /// there's no way to approve a task from what `board-show`/`GET /api/tasks/{id}` shows
    /// alone; a caller would have to separately keep the digest `create`/`revise` printed.
    pub assignment_digest: String,
    pub status: String,
    pub backend: String,
    pub objective: String,
    pub repository: String,
    pub branch: Option<String>,
    pub workspace: String,
    pub agent: String,
    pub allowed_scope: Vec<String>,
    pub budget: Vec<String>,
    pub approved_by: Option<String>,
    pub approved_at: Option<u64>,
    pub attempts: Vec<AttemptDetail>,
    pub task_error: Option<String>,
    pub reviewer: Option<String>,
    pub accepted: Option<bool>,
    pub reviewed_at: Option<u64>,
    pub evidence_digest: Option<String>,
    /// Populated by `TaskService::detail` (which owns the `Coordination` lookup this pure
    /// module has no access to), not by `fake_detail`/`orchestrate_detail` below - both
    /// default this to empty, same pattern as `AttemptDetail.previews`.
    pub depends_on: Vec<DependencyStatus>,
    /// A human-readable description of repository drift since this assignment's context
    /// was captured (e.g. "base moved from aaa to bbb"), populated by `TaskService::detail`
    /// only while `status == "awaiting_review"` - the one state where attempting `review`
    /// would actually hit this. `None` either means nothing drifted, or the task isn't in
    /// that state yet. Same defaults-empty-here, populated-by-TaskService convention as
    /// `depends_on`.
    pub conflict: Option<String>,
    /// M10.3 Phase 4: the agent engine's best-effort Windows workspace-
    /// path-length diagnostic (see `agents::workspace_path_warning`),
    /// retained as evidence rather than only printed. `None` for every
    /// other engine - they don't yet carry this check (see
    /// `docs/m10.3/M10_3_REMAINING_RISKS.md`) - and for an agent task
    /// whose workspace path never crossed the threshold.
    pub workspace_warning: Option<String>,
    /// Observed host activity; it is not verification or agent-reported completion.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<crate::agent_progress::AgentProgress>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub routing: Vec<crate::agents::RoutingEvent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integration: Option<crate::integration::Record>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_workflow_id: Option<String>,
}

/// One dependency edge plus the dependency's own current status, so `board-show`/
/// `GET /api/tasks/{id}` can explain *why* a task can't `run` yet without a second lookup.
#[derive(Debug, Clone, Serialize)]
pub struct DependencyStatus {
    pub id: String,
    pub status: String,
}

fn status_str<S: Serialize>(status: &S) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".into())
}

fn review_str(review: &Option<store::Review>) -> Option<String> {
    review.as_ref().map(|r| {
        format!(
            "{} by {}",
            if r.accepted { "accepted" } else { "rejected" },
            r.reviewer
        )
    })
}

fn fake_kind(backend: &str) -> &'static str {
    if backend == "codex-exec-v1" {
        "codex"
    } else {
        "fake"
    }
}

pub fn fake_row(task: &store::Task) -> Row {
    Row {
        kind: fake_kind(&task.assignment.backend).into(),
        id: task.id.clone(),
        status: status_str(&task.status),
        backend: task.assignment.backend.clone(),
        units: task.attempts.len(),
        review: review_str(&task.review),
        objective: task.assignment.objective.clone(),
        step_label: None,
        alert: None,
        children: Vec::new(),
    }
}

pub fn orchestrate_row(task: &orchestration::Task) -> Row {
    Row {
        kind: "orchestrate".into(),
        id: task.id.clone(),
        status: status_str(&task.status),
        backend: "local-changes-v1".into(),
        units: task.rounds.len(),
        review: review_str(&task.review),
        objective: task.assignment.spec.objective.clone(),
        step_label: None,
        alert: None,
        children: Vec::new(),
    }
}

fn fake_agent(task: &store::Task) -> String {
    match &task.assignment.codex {
        Some(codex) => format!("codex {} (model {})", codex.version, codex.model),
        None => task.assignment.backend.clone(),
    }
}

fn fake_budget(task: &store::Task) -> Vec<String> {
    task.assignment
        .execution_policy
        .as_ref()
        .map(|p| {
            vec![
                format!("timeout_ms={}", p.timeout_ms),
                format!("verification_timeout_ms={}", p.verification_timeout_ms),
                format!("log_bytes={}", p.log_bytes),
            ]
        })
        .unwrap_or_default()
}

pub fn fake_detail(task: &store::Task, evidence_digest: Option<String>) -> Result<Detail> {
    Ok(Detail {
        kind: fake_kind(&task.assignment.backend).into(),
        id: task.id.clone(),
        revision: task.revision,
        assignment_digest: task.assignment.digest()?,
        status: status_str(&task.status),
        backend: task.assignment.backend.clone(),
        objective: task.assignment.objective.clone(),
        repository: task.assignment.repository.checkout.display().to_string(),
        branch: task.assignment.repository.branch.clone(),
        workspace: task.assignment.workspace.display().to_string(),
        agent: fake_agent(task),
        allowed_scope: task.assignment.allowed_actions.clone(),
        budget: fake_budget(task),
        approved_by: task.approval.as_ref().map(|a| a.approver.clone()),
        approved_at: task.approval.as_ref().map(|a| a.at),
        attempts: task
            .attempts
            .iter()
            .map(|a| AttemptDetail {
                id: a.id.clone(),
                started_at: Some(a.started_at),
                ended_at: a.ended_at,
                outcome: a.receipt.as_ref().map(|r| r.result.clone()),
                error: a.error.clone(),
                previews: Vec::new(),
            })
            .collect(),
        task_error: None,
        reviewer: task.review.as_ref().map(|r| r.reviewer.clone()),
        accepted: task.review.as_ref().map(|r| r.accepted),
        reviewed_at: task.review.as_ref().map(|r| r.at),
        evidence_digest,
        depends_on: Vec::new(),
        conflict: None,
        workspace_warning: None,
        progress: None,
        routing: vec![],
        workflow: None,
        integration: None,
        parent_workflow_id: None,
    })
}

pub fn orchestrate_detail(
    task: &orchestration::Task,
    evidence_digest: Option<String>,
) -> Result<Detail> {
    let spec = &task.assignment.spec;
    Ok(Detail {
        kind: "orchestrate".into(),
        id: task.id.clone(),
        revision: task.revision,
        assignment_digest: task.assignment.digest()?,
        status: status_str(&task.status),
        backend: "local-changes-v1".into(),
        objective: spec.objective.clone(),
        repository: task.assignment.repository.checkout.display().to_string(),
        branch: task.assignment.repository.branch.clone(),
        workspace: task.assignment.workspace.display().to_string(),
        agent: format!("local-changes-v1 (target {})", spec.target),
        allowed_scope: spec.allowed_files.clone(),
        budget: vec![
            format!("max_rounds={}", spec.max_rounds),
            format!("max_output_tokens={}", spec.max_output_tokens),
            format!("generation_timeout_ms={}", spec.generation_timeout_ms),
            format!("verification_timeout_ms={}", spec.verification_timeout_ms),
        ],
        approved_by: task.approval.as_ref().map(|a| a.approver.clone()),
        approved_at: task.approval.as_ref().map(|a| a.at),
        attempts: task
            .rounds
            .iter()
            .map(|r| AttemptDetail {
                id: r.inference_job.clone(),
                started_at: None,
                ended_at: None,
                outcome: r.verification.as_ref().map(|v| v.outcome.clone()),
                error: None,
                previews: Vec::new(),
            })
            .collect(),
        task_error: task.error.clone(),
        reviewer: task.review.as_ref().map(|r| r.reviewer.clone()),
        accepted: task.review.as_ref().map(|r| r.accepted),
        reviewed_at: task.review.as_ref().map(|r| r.at),
        evidence_digest,
        depends_on: Vec::new(),
        conflict: None,
        workspace_warning: None,
        progress: None,
        routing: vec![],
        workflow: None,
        integration: None,
        parent_workflow_id: None,
    })
}

pub fn agent_row(task: &agents::AgentTask) -> Row {
    Row {
        kind: "agent".into(),
        id: task.id.clone(),
        status: status_str(&task.status),
        backend: task.assignment.spec.target.clone(),
        units: usize::from(task.execution.is_some()),
        review: review_str(&task.review),
        objective: task.assignment.spec.objective.clone(),
        step_label: None,
        alert: None,
        children: Vec::new(),
    }
}

pub fn agent_detail(task: &agents::AgentTask, evidence_digest: Option<String>) -> Result<Detail> {
    let spec = &task.assignment.spec;
    let mut budget = vec![
        format!("generation_timeout_ms={}", spec.generation_timeout_ms),
        format!("verification_timeout_ms={}", spec.verification_timeout_ms),
    ];
    if let Some(idle) = spec.generation_idle_timeout_ms {
        budget.push(format!("generation_idle_timeout_ms={idle}"));
    }
    let outcome = task
        .verification
        .as_ref()
        .or(task.execution.as_ref())
        .map(|r| r.outcome.clone());
    let started_at = task.execution.as_ref().map(|r| r.started_at);
    let ended_at = task
        .verification
        .as_ref()
        .or(task.execution.as_ref())
        .map(|r| r.ended_at);
    let display_model = if spec.model.trim().is_empty() {
        task.assignment.backend.model()
    } else {
        spec.model.as_str()
    };
    let mut attempts: Vec<AttemptDetail> = task
        .attempt_history
        .iter()
        .enumerate()
        .map(|(idx, prev)| {
            let prev_outcome = prev
                .verification
                .as_ref()
                .or(prev.execution.as_ref())
                .map(|r| r.outcome.clone());
            AttemptDetail {
                id: format!("{}-attempt-{}", task.id, idx + 1),
                started_at: prev.execution.as_ref().map(|r| r.started_at),
                ended_at: prev
                    .verification
                    .as_ref()
                    .or(prev.execution.as_ref())
                    .map(|r| r.ended_at),
                outcome: prev_outcome,
                error: prev.error.clone(),
                previews: Vec::new(),
            }
        })
        .collect();
    if task.execution.is_some()
        || matches!(
            task.status,
            agents::AgentStatus::Running | agents::AgentStatus::Verifying
        )
    {
        attempts.push(AttemptDetail {
            id: if task.attempt_history.is_empty() {
                format!("{}-attempt", task.id)
            } else {
                format!("{}-attempt-{}", task.id, task.attempt_history.len() + 1)
            },
            started_at,
            ended_at,
            outcome,
            error: task.error.clone(),
            previews: Vec::new(),
        });
    }
    Ok(Detail {
        kind: "agent".into(),
        id: task.id.clone(),
        revision: task.revision,
        assignment_digest: task.assignment.digest()?,
        status: status_str(&task.status),
        backend: spec.target.clone(),
        objective: spec.objective.clone(),
        repository: task.assignment.repository.checkout.display().to_string(),
        branch: task.assignment.repository.branch.clone(),
        workspace: task.assignment.workspace.display().to_string(),
        agent: format!(
            "{} (model {}, version {})",
            spec.target,
            display_model,
            task.assignment.backend.version()
        ),
        allowed_scope: spec.allowed_files.clone(),
        budget,
        approved_by: task.approval.as_ref().map(|a| a.approver.clone()),
        approved_at: task.approval.as_ref().map(|a| a.at),
        attempts,
        task_error: task.error.clone(),
        reviewer: task.review.as_ref().map(|r| r.reviewer.clone()),
        accepted: task.review.as_ref().map(|r| r.accepted),
        reviewed_at: task.review.as_ref().map(|r| r.at),
        evidence_digest,
        depends_on: Vec::new(),
        conflict: None,
        workspace_warning: task.workspace_warning.clone(),
        progress: None,
        routing: vec![],
        workflow: None,
        integration: None,
        parent_workflow_id: None,
    })
}

/// Resolve a task id or unambiguous id prefix against the known ids from both engines.
pub fn resolve(ids: &[String], query: &str) -> Result<String> {
    if ids.iter().any(|id| id == query) {
        return Ok(query.to_string());
    }
    let matches: Vec<&String> = ids.iter().filter(|id| id.starts_with(query)).collect();
    match matches.as_slice() {
        [one] => Ok((*one).clone()),
        [] => bail!("no task id matches {query:?}"),
        many => bail!(
            "ambiguous task id prefix {query:?} matches {} tasks",
            many.len()
        ),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

pub fn render_table(rows: &[Row]) -> String {
    if rows.is_empty() {
        return "No tasks.".to_string();
    }
    let all_rows = rows
        .iter()
        .flat_map(|r| std::iter::once(r).chain(r.children.iter()));
    let kind_w = all_rows
        .clone()
        .map(|r| r.kind.len())
        .chain([4])
        .max()
        .unwrap();
    let status_w = all_rows
        .clone()
        .map(|r| r.status.len())
        .chain([6])
        .max()
        .unwrap();
    let backend_w = all_rows.map(|r| r.backend.len()).chain([7]).max().unwrap();
    let id_w = 36usize;
    let mut out = format!(
        "{:<kind_w$}  {:<id_w$}  {:<status_w$}  {:<backend_w$}  UNITS  REVIEW\n",
        "KIND", "ID", "STATUS", "BACKEND"
    );
    for row in rows {
        out.push_str(&format!(
            "{:<kind_w$}  {:<id_w$}  {:<status_w$}  {:<backend_w$}  {:<5}  {}\n",
            row.kind,
            row.id,
            row.status,
            row.backend,
            row.units,
            row.review.as_deref().unwrap_or("-"),
        ));
        out.push_str(&format!("  {}\n", truncate(&row.objective, 110)));
        for (idx, child) in row.children.iter().enumerate() {
            let branch = if idx + 1 == row.children.len() {
                "└─"
            } else {
                "├─"
            };
            let label = child
                .step_label
                .as_deref()
                .map(|s| format!("[{s}] "))
                .unwrap_or_default();
            out.push_str(&format!(
                "  {branch} {:<kind_w$}  {:<id_w$}  {:<status_w$}  {:<backend_w$}  {:<5}  {label}{}\n",
                child.kind,
                child.id,
                child.status,
                child.backend,
                child.units,
                truncate(&child.objective, 90),
            ));
        }
    }
    out.trim_end().to_string()
}

pub fn render_detail(detail: &Detail) -> String {
    let mut out = format!("{} {}  [{}]\n", detail.kind, detail.id, detail.status);
    if let Some(parent_id) = &detail.parent_workflow_id {
        out.push_str(&format!(
            "parent:     {parent_id}  (workflow child step; review/retry/integrate on parent)\n"
        ));
    }
    out.push_str(&format!(
        "revision:   {}  (pass --revision {} to the next board-approve/run/cancel/reconcile/review)\n",
        detail.revision, detail.revision
    ));
    out.push_str(&format!(
        "digest:     {}  (pass --digest to board-approve for this exact assignment)\n",
        detail.assignment_digest
    ));
    out.push_str(&format!("backend:    {}\n", detail.backend));
    out.push_str(&format!("agent:      {}\n", detail.agent));
    out.push_str(&format!("objective:  {}\n", detail.objective));
    out.push_str(&format!(
        "repository: {}{}\n",
        detail.repository,
        detail
            .branch
            .as_deref()
            .map(|b| format!(" (branch: {b})"))
            .unwrap_or_default()
    ));
    out.push_str(&format!("workspace:  {}\n", detail.workspace));
    out.push_str(&format!(
        "scope:      {}\n",
        if detail.allowed_scope.is_empty() {
            "none recorded".to_string()
        } else {
            detail.allowed_scope.join("; ")
        }
    ));
    out.push_str(&format!(
        "budget:     {}\n",
        if detail.budget.is_empty() {
            "none recorded".to_string()
        } else {
            detail.budget.join(", ")
        }
    ));
    if !detail.depends_on.is_empty() {
        out.push_str("depends on:\n");
        for dep in &detail.depends_on {
            let blocked = if dep.status == "accepted" {
                ""
            } else {
                "  (blocks run)"
            };
            out.push_str(&format!(
                "  - {}  status={}{}\n",
                dep.id, dep.status, blocked
            ));
        }
    }
    if let Some(conflict) = &detail.conflict {
        out.push_str(&format!(
            "conflict:   {conflict}  (review will refuse until a fresh assignment is prepared)\n"
        ));
    }
    match (&detail.approved_by, detail.approved_at) {
        (Some(by), Some(at)) => out.push_str(&format!("approved:   by {by} at {at} (ms epoch)\n")),
        _ => out.push_str("approved:   not yet approved\n"),
    }
    if detail.attempts.is_empty() {
        out.push_str("attempts:   none yet\n");
    } else {
        out.push_str("attempts:\n");
        for a in &detail.attempts {
            let span = match (a.started_at, a.ended_at) {
                (Some(s), Some(e)) => format!("{}ms", e.saturating_sub(s)),
                (Some(_), None) => "in progress".into(),
                _ => "unknown".into(),
            };
            out.push_str(&format!(
                "  - {}  outcome={}  duration={}{}\n",
                a.id,
                a.outcome.as_deref().unwrap_or("pending"),
                span,
                a.error
                    .as_ref()
                    .map(|e| format!("  error={e}"))
                    .unwrap_or_default()
            ));
            for p in &a.previews {
                out.push_str(&format!(
                    "      [{}] {} of {} bytes shown{}  ({})\n",
                    p.label,
                    p.shown_bytes,
                    p.total_bytes,
                    if p.truncated { ", truncated" } else { "" },
                    p.path
                ));
                for line in p.text.lines().take(20) {
                    out.push_str(&format!("        | {line}\n"));
                }
            }
        }
    }
    if let Some(err) = &detail.task_error {
        out.push_str(&format!("task error: {err}\n"));
    }
    match (&detail.reviewer, detail.accepted, detail.reviewed_at) {
        (Some(who), Some(accepted), Some(at)) => out.push_str(&format!(
            "review:     {} by {who} at {at} (ms epoch)\n",
            if accepted { "accepted" } else { "rejected" }
        )),
        _ => out.push_str("review:     awaiting review\n"),
    }
    if let Some(digest) = &detail.evidence_digest {
        out.push_str(&format!("evidence:   {digest}\n"));
    }
    if let Some(integration) = &detail.integration {
        out.push_str(&format!(
            "integration: {} (revision {})\nplan digest: {}\nsource: {}\n",
            integration.status,
            integration.revision,
            integration.plan_digest,
            integration.repository.checkout.display()
        ));
        for file in &integration.files {
            out.push_str(&format!("  - {}\n", file.path));
        }
        if let Some(error) = &integration.error {
            out.push_str(&format!("integration error: {error}\n"));
        }
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(kind: &str, id: &str, status: &str, objective: &str) -> Row {
        Row {
            kind: kind.into(),
            id: id.into(),
            status: status.into(),
            backend: "local-changes-v1".into(),
            units: 2,
            review: None,
            objective: objective.into(),
            step_label: None,
            alert: None,
            children: Vec::new(),
        }
    }

    #[test]
    fn empty_board_says_so_instead_of_an_empty_table() {
        assert_eq!(render_table(&[]), "No tasks.");
    }

    #[test]
    fn table_renders_a_header_and_truncates_long_objectives() {
        let long = "x".repeat(200);
        let rendered = render_table(&[row("orchestrate", "abc-123", "awaiting_review", &long)]);
        assert!(rendered.starts_with("KIND"));
        assert!(rendered.contains("abc-123"));
        assert!(rendered.contains('…'));
        assert!(!rendered.contains(&long));
    }

    #[test]
    fn resolve_prefers_exact_id_match_over_prefix() {
        let ids = vec!["abc123".to_string(), "abc124".to_string()];
        assert_eq!(resolve(&ids, "abc123").unwrap(), "abc123");
    }

    #[test]
    fn resolve_accepts_an_unambiguous_prefix() {
        let ids = vec!["abc123".to_string(), "def456".to_string()];
        assert_eq!(resolve(&ids, "abc").unwrap(), "abc123");
    }

    #[test]
    fn resolve_rejects_an_ambiguous_prefix() {
        let ids = vec!["abc123".to_string(), "abc124".to_string()];
        assert!(resolve(&ids, "abc").is_err());
    }

    #[test]
    fn resolve_rejects_an_unknown_id() {
        let ids = vec!["abc123".to_string()];
        assert!(resolve(&ids, "zzz").is_err());
    }

    #[test]
    fn detail_without_approval_or_review_reads_as_pending_not_blank() {
        let detail = Detail {
            kind: "fake".into(),
            id: "t1".into(),
            revision: 1,
            assignment_digest: "assign0".into(),
            status: "awaiting_approval".into(),
            backend: "fake-process-v1".into(),
            objective: "demo".into(),
            repository: "C:/repo".into(),
            branch: Some("main".into()),
            workspace: "C:/repo-workspace".into(),
            agent: "fake-process-v1".into(),
            allowed_scope: vec![],
            budget: vec![],
            approved_by: None,
            approved_at: None,
            attempts: vec![],
            task_error: None,
            reviewer: None,
            accepted: None,
            reviewed_at: None,
            evidence_digest: None,
            depends_on: vec![],
            conflict: None,
            workspace_warning: None,
            progress: None,
            routing: vec![],
            workflow: None,
            integration: None,
            parent_workflow_id: None,
        };
        let rendered = render_detail(&detail);
        assert!(rendered.contains("not yet approved"));
        assert!(rendered.contains("none yet"));
        assert!(rendered.contains("awaiting review"));
        assert!(rendered.contains("scope:      none recorded"));
        assert!(rendered.contains("budget:     none recorded"));
    }

    #[test]
    fn detail_reports_attempt_outcome_and_duration() {
        let detail = Detail {
            kind: "fake".into(),
            id: "t1".into(),
            revision: 2,
            assignment_digest: "assign0".into(),
            status: "awaiting_review".into(),
            backend: "fake-process-v1".into(),
            objective: "demo".into(),
            repository: "C:/repo".into(),
            branch: None,
            workspace: "C:/repo-workspace".into(),
            agent: "fake-process-v1".into(),
            allowed_scope: vec!["create one specified root file".into()],
            budget: vec!["timeout_ms=5000".into()],
            approved_by: Some("operator".into()),
            approved_at: Some(100),
            attempts: vec![AttemptDetail {
                id: "a1".into(),
                started_at: Some(100),
                ended_at: Some(150),
                outcome: Some("pass".into()),
                error: None,
                previews: vec![build_preview(
                    "diff",
                    "C:/repo/artifacts/a1/result.patch",
                    b"--- a/x\n+++ b/x\n",
                    16,
                    false,
                )],
            }],
            task_error: None,
            reviewer: None,
            accepted: None,
            reviewed_at: None,
            evidence_digest: Some("deadbeef".into()),
            depends_on: vec![],
            conflict: None,
            workspace_warning: None,
            progress: None,
            routing: vec![],
            workflow: None,
            integration: None,
            parent_workflow_id: None,
        };
        let rendered = render_detail(&detail);
        assert!(rendered.contains("revision:   2"));
        assert!(rendered.contains("workspace:  C:/repo-workspace"));
        assert!(rendered.contains("scope:      create one specified root file"));
        assert!(rendered.contains("budget:     timeout_ms=5000"));
        assert!(rendered.contains("outcome=pass"));
        assert!(rendered.contains("duration=50ms"));
        assert!(rendered.contains("deadbeef"));
        assert!(rendered.contains("[diff] 16 of 16 bytes shown"));
        assert!(rendered.contains("| --- a/x"));
        assert!(rendered.contains("| +++ b/x"));
    }

    #[test]
    fn build_preview_flags_truncation_from_either_a_short_read_or_capture_time() {
        let full = build_preview("stdout", "p", b"hello", 5, false);
        assert_eq!(full.shown_bytes, 5);
        assert!(!full.truncated);

        let short_read = build_preview("stdout", "p", b"he", 5, false);
        assert!(
            short_read.truncated,
            "shown bytes < total_bytes must flag truncation"
        );

        let capture_truncated = build_preview("stdout", "p", b"hello", 5, true);
        assert!(
            capture_truncated.truncated,
            "a capture-time truncation must survive even if the preview itself read everything retained"
        );
    }

    #[test]
    fn build_preview_is_lossy_on_invalid_utf8_instead_of_failing() {
        let preview = build_preview("stdout", "p", &[0xff, 0xfe, b'h', b'i'], 4, false);
        assert!(preview.text.contains("hi"));
    }

    #[test]
    fn an_attempt_with_no_previews_renders_no_preview_lines() {
        let detail = Detail {
            kind: "fake".into(),
            id: "t1".into(),
            revision: 1,
            assignment_digest: "assign0".into(),
            status: "awaiting_review".into(),
            backend: "fake-process-v1".into(),
            objective: "demo".into(),
            repository: "C:/repo".into(),
            branch: None,
            workspace: "C:/repo-workspace".into(),
            agent: "fake-process-v1".into(),
            allowed_scope: vec![],
            budget: vec![],
            approved_by: None,
            approved_at: None,
            attempts: vec![AttemptDetail {
                id: "a1".into(),
                started_at: Some(100),
                ended_at: Some(150),
                outcome: Some("pass".into()),
                error: None,
                previews: vec![],
            }],
            task_error: None,
            reviewer: None,
            accepted: None,
            reviewed_at: None,
            evidence_digest: None,
            depends_on: vec![],
            conflict: None,
            workspace_warning: None,
            progress: None,
            routing: vec![],
            workflow: None,
            integration: None,
            parent_workflow_id: None,
        };
        let rendered = render_detail(&detail);
        assert!(rendered.contains("outcome=pass"));
        assert!(
            !rendered.contains("bytes shown"),
            "no preview was attached, so none should be rendered"
        );
    }

    #[test]
    fn table_renders_nested_workflow_children_under_parent() {
        let mut parent = row("workflow", "wf-1", "awaiting_review", "Implement feature X");
        parent.backend = "workflow-v1".into();
        let mut child1 = row("agent", "step-1", "completed", "Write implementation");
        child1.backend = "cursor-agent".into();
        child1.step_label = Some("S01 · coder".into());
        let mut child2 = row("agent", "step-2", "completed", "Review changes");
        child2.backend = "codex".into();
        child2.step_label = Some("S02 · reviewer".into());
        parent.children = vec![child1, child2];

        let rendered = render_table(&[parent]);
        assert!(rendered.contains("wf-1"));
        assert!(rendered.contains("Implement feature X"));
        assert!(rendered.contains("├─ agent"));
        assert!(rendered.contains("[S01 · coder]"));
        assert!(rendered.contains("└─ agent"));
        assert!(rendered.contains("[S02 · reviewer]"));
    }

    #[test]
    fn detail_renders_parent_workflow_when_present() {
        let detail = Detail {
            kind: "agent".into(),
            id: "step-1".into(),
            revision: 1,
            assignment_digest: "assign0".into(),
            status: "awaiting_review".into(),
            backend: "cursor-agent".into(),
            objective: "step objective".into(),
            repository: "C:/repo".into(),
            branch: None,
            workspace: "C:/repo-workspace".into(),
            agent: "cursor-agent".into(),
            allowed_scope: vec![],
            budget: vec![],
            approved_by: None,
            approved_at: None,
            attempts: vec![],
            task_error: None,
            reviewer: None,
            accepted: None,
            reviewed_at: None,
            evidence_digest: None,
            depends_on: vec![],
            conflict: None,
            workspace_warning: None,
            progress: None,
            routing: vec![],
            workflow: None,
            integration: None,
            parent_workflow_id: Some("wf-parent-123".into()),
        };
        let rendered = render_detail(&detail);
        assert!(rendered.contains("parent:     wf-parent-123"));
    }
}
