use dioxus::prelude::*;

use crate::{
    AgentProgress, AttemptInfo, FetchState, IntegrationView, Preview, TaskDetail, TaskRow,
    WorkflowStep, WorkflowView,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TaskFilter {
    All,
    NeedsAction,
    Active,
    Done,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InspectorTab {
    Pipeline,
    DiffLogs,
    SpecsAudit,
}

pub(crate) fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

pub(crate) fn repo_basename(path: &str) -> &str {
    let trimmed = path.trim_end_matches(['/', '\\']);
    trimmed
        .rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(path)
}

pub(crate) fn is_live_status(status: &str) -> bool {
    matches!(
        status,
        "running" | "verifying" | "preparing_workspace" | "generating" | "applying"
    )
}

pub(crate) fn status_label(status: &str) -> &'static str {
    match status {
        "awaiting_approval" => "Needs Approval",
        "approved" => "Ready to Run",
        "preparing_workspace" => "Preparing",
        "running" => "Running",
        "verifying" => "Verifying",
        "generating" => "Generating",
        "applying" => "Applying",
        "awaiting_review" => "Needs Review",
        "accepted" => "Accepted",
        "completed" => "Passed",
        "quota_fallback" => "Quota Fallback",
        "rejected" => "Rejected",
        "failed" => "Failed",
        "timed_out" => "Timed Out",
        "cancelled" => "Cancelled",
        "needs_work" => "Needs Work",
        _ => "Unknown",
    }
}

pub(crate) fn status_badge_class(status: &str) -> &'static str {
    match status {
        "accepted" | "completed" => "bg-emerald-950/75 text-emerald-300 border-emerald-700/70",
        "awaiting_review" => "bg-sky-950/75 text-sky-300 border-sky-600/70",
        "awaiting_approval" | "approved" | "quota_fallback" => {
            "bg-amber-950/75 text-amber-300 border-amber-600/70"
        }
        "running" | "verifying" | "preparing_workspace" | "generating" | "applying" => {
            "bg-[#FF754F]/15 text-[#FF754F] border-[#FF754F]/75"
        }
        "failed" | "timed_out" | "rejected" | "needs_work" => {
            "bg-red-950/75 text-red-300 border-red-700/70"
        }
        _ => "bg-neutral-900 text-neutral-300 border-neutral-700",
    }
}

fn matches_filter(row: &TaskRow, filter: TaskFilter) -> bool {
    match filter {
        TaskFilter::All => true,
        TaskFilter::NeedsAction => {
            matches!(row.status.as_str(), "awaiting_approval" | "awaiting_review")
        }
        TaskFilter::Active => matches!(
            row.status.as_str(),
            "running"
                | "verifying"
                | "preparing_workspace"
                | "generating"
                | "applying"
                | "approved"
        ),
        TaskFilter::Done => row.status == "accepted",
        TaskFilter::Failed => matches!(
            row.status.as_str(),
            "failed" | "timed_out" | "rejected" | "cancelled" | "needs_work" | "unknown"
        ),
    }
}

pub(crate) fn duration(ms: u64) -> String {
    if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{}m {:02}s", ms / 60_000, (ms / 1000) % 60)
    }
}

fn format_bytes(bytes: u64) -> String {
    if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1_024 {
        format!("{:.1} KB", bytes as f64 / 1_024.0)
    } else {
        format!("{bytes} B")
    }
}

pub(crate) fn budget_label(value: &str) -> String {
    let Some((name, amount)) = value.split_once('=') else {
        return value.into();
    };
    let Ok(ms) = amount.parse::<u64>() else {
        return value.into();
    };
    let label = match name {
        "generation_timeout_ms" | "timeout_ms" => "Execution hard limit",
        "verification_timeout_ms" => "Verification limit",
        "generation_idle_timeout_ms" => "Inactivity limit",
        _ => return value.into(),
    };
    format!("{label}: {}", duration(ms))
}

fn parse_workflow_max_steps(budget: &[String]) -> Option<usize> {
    for line in budget {
        if let Some(rest) = line.strip_prefix("Workflow:") {
            if let Some((_, after_slash)) = rest.split_once('/') {
                if let Some(num) = after_slash.split_whitespace().next() {
                    if let Ok(parsed) = num.parse::<usize>() {
                        return Some(parsed);
                    }
                }
            }
        }
    }
    None
}

#[cfg(target_arch = "wasm32")]
fn write_clipboard(text: String) {
    use wasm_bindgen::{JsCast, JsValue};
    let Some(win) = web_sys::window() else {
        return;
    };
    let fallback = text.clone();
    if let Ok(nav) = js_sys::Reflect::get(&win, &JsValue::from_str("navigator")) {
        if let Ok(clip) = js_sys::Reflect::get(&nav, &JsValue::from_str("clipboard")) {
            if !clip.is_undefined() && !clip.is_null() {
                if let Ok(write_fn) = js_sys::Reflect::get(&clip, &JsValue::from_str("writeText")) {
                    if let Some(func) = write_fn.dyn_ref::<js_sys::Function>() {
                        if let Ok(promise_val) = func.call1(&clip, &JsValue::from_str(&text)) {
                            if let Ok(promise) = promise_val.dyn_into::<js_sys::Promise>() {
                                wasm_bindgen_futures::spawn_local(async move {
                                    if wasm_bindgen_futures::JsFuture::from(promise).await.is_err()
                                    {
                                        copy_via_exec_command(&fallback);
                                    }
                                });
                                return;
                            }
                        }
                    }
                }
            }
        }
    }
    copy_via_exec_command(&text);
}

#[cfg(target_arch = "wasm32")]
fn copy_via_exec_command(text: &str) {
    use wasm_bindgen::{JsCast, JsValue};
    let Some(doc) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    let Ok(create_fn_val) = js_sys::Reflect::get(&doc, &JsValue::from_str("createElement")) else {
        return;
    };
    let Some(create_fn) = create_fn_val.dyn_ref::<js_sys::Function>() else {
        return;
    };
    let Ok(el) = create_fn.call1(&doc, &JsValue::from_str("textarea")) else {
        return;
    };
    let _ = js_sys::Reflect::set(&el, &JsValue::from_str("value"), &JsValue::from_str(text));
    if let Ok(set_attr_val) = js_sys::Reflect::get(&el, &JsValue::from_str("setAttribute")) {
        if let Some(set_attr) = set_attr_val.dyn_ref::<js_sys::Function>() {
            let _ = set_attr.call2(&el, &JsValue::from_str("readonly"), &JsValue::from_str(""));
            let _ = set_attr.call2(
                &el,
                &JsValue::from_str("style"),
                &JsValue::from_str("position:fixed;left:-9999px;top:-9999px;opacity:0;"),
            );
        }
    }
    let Ok(body) = js_sys::Reflect::get(&doc, &JsValue::from_str("body")) else {
        return;
    };
    if body.is_null() || body.is_undefined() {
        return;
    }
    if let Ok(append_val) = js_sys::Reflect::get(&body, &JsValue::from_str("appendChild")) {
        if let Some(append_fn) = append_val.dyn_ref::<js_sys::Function>() {
            let _ = append_fn.call1(&body, &el);
        }
    }
    if let Ok(focus_val) = js_sys::Reflect::get(&el, &JsValue::from_str("focus")) {
        if let Some(f) = focus_val.dyn_ref::<js_sys::Function>() {
            let _ = f.call0(&el);
        }
    }
    if let Ok(sel_val) = js_sys::Reflect::get(&el, &JsValue::from_str("select")) {
        if let Some(f) = sel_val.dyn_ref::<js_sys::Function>() {
            let _ = f.call0(&el);
        }
    }
    if let Some(html_doc) = doc.dyn_ref::<web_sys::HtmlDocument>() {
        let _ = html_doc.exec_command("copy");
    }
    if let Ok(remove_val) = js_sys::Reflect::get(&body, &JsValue::from_str("removeChild")) {
        if let Some(remove_fn) = remove_val.dyn_ref::<js_sys::Function>() {
            let _ = remove_fn.call1(&body, &el);
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn write_clipboard(_text: String) {}

#[component]
fn CopyButton(text: String, label: &'static str, subtle: Option<bool>) -> Element {
    let mut copied_seq = use_signal(|| 0u64);
    let mut is_copied = use_signal(|| false);
    let subtle = subtle.unwrap_or(false);

    let cls = if is_copied() {
        "inline-flex items-center gap-1 rounded px-2.5 py-1 text-[11px] font-semibold border border-emerald-600/80 bg-emerald-950/70 text-emerald-300 transition-colors shrink-0"
    } else if subtle {
        "inline-flex items-center gap-1 rounded px-2.5 py-1 text-[11px] font-medium border border-[#202D3A] bg-[#0C1118] hover:border-[#66D8CE]/60 hover:text-[#F2F5F7] text-[#8D9DAD] transition-colors shrink-0"
    } else {
        "inline-flex items-center gap-1 rounded px-2.5 py-1 text-[11px] font-semibold border border-red-700/70 bg-red-950/60 hover:bg-red-900/70 text-red-100 transition-colors shrink-0"
    };

    rsx! {
        button {
            r#type: "button",
            class: "{cls}",
            onclick: move |_| {
                write_clipboard(text.clone());
                let next_seq = copied_seq().wrapping_add(1);
                copied_seq.set(next_seq);
                is_copied.set(true);
                spawn(async move {
                    gloo_timers::future::TimeoutFuture::new(1800).await;
                    if copied_seq() == next_seq {
                        is_copied.set(false);
                    }
                });
            },
            if is_copied() { "Copied ✓" } else { "{label}" }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum TimelineEntry {
    Step {
        step_num: usize,
        worker: String,
        model: Option<String>,
        tier: Option<String>,
        complexity: Option<String>,
        rationale: Option<String>,
        dispatched_objective: Option<String>,
        step: Option<WorkflowStep>,
    },
    Rejected {
        decision_num: usize,
        error: String,
        raw_action: String,
    },
    Inspect {
        decision_num: usize,
        query: String,
    },
    Finish {
        summary: String,
    },
    Escalate {
        reason: String,
    },
}

fn build_timeline(workflow: &WorkflowView) -> Vec<TimelineEntry> {
    let mut entries = Vec::new();
    let mut step_cursor = 0usize;

    for (idx, record) in workflow.decisions.iter().enumerate() {
        let decision_num = idx + 1;
        if let Some(err) = &record.error {
            let raw_action = record
                .decision
                .get("action")
                .and_then(|v| v.as_str())
                .unwrap_or("malformed")
                .to_string();
            entries.push(TimelineEntry::Rejected {
                decision_num,
                error: err.clone(),
                raw_action,
            });
            continue;
        }

        let action = record
            .decision
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");

        match action {
            "dispatch" => {
                let step = workflow.steps.get(step_cursor).cloned();
                if step.is_some() {
                    step_cursor += 1;
                }
                let worker = step
                    .as_ref()
                    .map(|s| s.worker.clone())
                    .or_else(|| {
                        record
                            .decision
                            .get("worker")
                            .and_then(|v| v.as_str())
                            .map(str::to_string)
                    })
                    .unwrap_or_else(|| "worker".into());
                let model = step
                    .as_ref()
                    .and_then(|s| s.result.as_ref())
                    .and_then(|r| r.get("model"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
                    .or_else(|| {
                        record
                            .decision
                            .get("model")
                            .and_then(|v| v.as_str())
                            .map(str::to_string)
                    });
                let tier = step
                    .as_ref()
                    .and_then(|s| s.result.as_ref())
                    .and_then(|r| r.get("tier"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                let complexity = step
                    .as_ref()
                    .and_then(|s| s.result.as_ref())
                    .and_then(|r| r.get("complexity"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                let rationale = record
                    .decision
                    .get("rationale")
                    .or_else(|| record.decision.get("reason"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                let dispatched_objective = record
                    .decision
                    .get("objective")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                let step_num = step_cursor.max(1);
                entries.push(TimelineEntry::Step {
                    step_num,
                    worker,
                    model,
                    tier,
                    complexity,
                    rationale,
                    dispatched_objective,
                    step,
                });
            }
            "inspect" => {
                let query = record
                    .decision
                    .get("query")
                    .and_then(|v| v.as_str())
                    .unwrap_or("git")
                    .to_string();
                entries.push(TimelineEntry::Inspect {
                    decision_num,
                    query,
                });
            }
            "finish" => {
                let summary = record
                    .decision
                    .get("summary")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Workflow completed")
                    .to_string();
                entries.push(TimelineEntry::Finish { summary });
            }
            "escalate" => {
                let reason = record
                    .decision
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Escalated for human input")
                    .to_string();
                entries.push(TimelineEntry::Escalate { reason });
            }
            _ => {}
        }
    }

    for extra in workflow.steps.iter().skip(step_cursor) {
        step_cursor += 1;
        let model = extra
            .result
            .as_ref()
            .and_then(|r| r.get("model"))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let tier = extra
            .result
            .as_ref()
            .and_then(|r| r.get("tier"))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let complexity = extra
            .result
            .as_ref()
            .and_then(|r| r.get("complexity"))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        entries.push(TimelineEntry::Step {
            step_num: step_cursor,
            worker: extra.worker.clone(),
            model,
            tier,
            complexity,
            rationale: None,
            dispatched_objective: None,
            step: Some(extra.clone()),
        });
    }

    entries
}

fn is_diff_preview(preview: &Preview) -> bool {
    let label = preview.label.to_ascii_lowercase();
    label.contains("patch")
        || label.contains("diff")
        || preview.text.starts_with("diff --git")
        || preview.text.starts_with("--- ")
}

fn latest_verification_status(task: &TaskDetail) -> Option<(bool, Option<i64>, Option<String>)> {
    if let Some(wf) = &task.workflow {
        for s in wf.steps.iter().rev() {
            if let Some(res) = &s.result {
                if let Some(outcome) = res.get("verification_outcome").and_then(|v| v.as_str()) {
                    let exit = res.get("verification_exit_code").and_then(|v| v.as_i64());
                    let stderr = res
                        .get("verification_stderr")
                        .and_then(|v| v.as_str())
                        .map(str::trim)
                        .filter(|v| !v.is_empty())
                        .map(str::to_string);
                    return Some((outcome == "succeeded", exit, stderr));
                }
            }
        }
        return None;
    }
    match task.attempts.last().and_then(|a| a.outcome.as_deref()) {
        Some("succeeded") => Some((true, Some(0), None)),
        Some("failed" | "timed_out" | "cancelled") => Some((false, None, None)),
        _ => None,
    }
}

struct TaskDiagnostic {
    headline: String,
    step_context: Option<String>,
    details_block: Option<String>,
    findings: Vec<(String, String)>,
    clipboard_report: String,
}

fn collect_task_diagnostic(
    task: &TaskDetail,
    action_error: Option<&str>,
) -> Option<TaskDiagnostic> {
    let is_problem_status = matches!(
        task.status.as_str(),
        "failed" | "timed_out" | "rejected" | "needs_work" | "cancelled" | "unknown"
    );

    let int_err = task
        .integration
        .as_ref()
        .and_then(|i| i.error.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let task_err = task
        .task_error
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let attempt_err = task
        .attempts
        .last()
        .and_then(|a| a.error.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let conflict_err = task
        .conflict
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let act_err = action_error
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    // Inspect workflow decisions & steps for root-cause diagnostics when problem state or rejected/escalated
    let mut step_context: Option<String> = None;
    let mut step_detail_parts: Vec<String> = Vec::new();
    let mut findings: Vec<(String, String)> = Vec::new();

    if let Some(wf) = &task.workflow {
        if is_problem_status || task_err.is_some() {
            // Check latest failing/noteworthy step
            for (idx, step) in wf.steps.iter().enumerate().rev() {
                let res = step.result.as_ref();
                let exec_out = res
                    .and_then(|r| r.get("execution_outcome"))
                    .and_then(|v| v.as_str());
                let ver_out = res
                    .and_then(|r| r.get("verification_outcome"))
                    .and_then(|v| v.as_str());
                let ver_exit = res
                    .and_then(|r| r.get("verification_exit_code"))
                    .and_then(|v| v.as_i64());
                let step_err = res
                    .and_then(|r| r.get("error"))
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                let ver_stderr = res
                    .and_then(|r| r.get("verification_stderr"))
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                let ver_stdout = res
                    .and_then(|r| r.get("verification_stdout"))
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                let agent_sum = res
                    .and_then(|r| r.get("agent_summary"))
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                let agentlet = res
                    .and_then(|r| r.get("agentlet_result"))
                    .filter(|v| !v.is_null());
                let agentlet_out = agentlet
                    .and_then(|a| a.get("outcome"))
                    .and_then(|v| v.as_str());
                let agentlet_sum = agentlet
                    .and_then(|a| a.get("summary"))
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty());

                let step_bad = step.result.is_none()
                    || matches!(exec_out, Some("failed" | "timed_out" | "cancelled"))
                    || matches!(ver_out, Some("failed" | "timed_out" | "cancelled"))
                    || matches!(agentlet_out, Some("needs_work" | "blocked"))
                    || step_err.is_some()
                    || (idx + 1 == wf.steps.len() && is_problem_status);

                if step_bad {
                    let model = res
                        .and_then(|r| r.get("model"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("default");
                    let exit_str = ver_exit
                        .filter(|c| *c != 0)
                        .map(|c| format!(" · verifier exit {c}"))
                        .unwrap_or_default();
                    let status_note = if let Some(ao) = agentlet_out {
                        format!(" · verdict: {ao}")
                    } else if matches!(ver_out, Some("failed" | "timed_out")) {
                        " · verification failed".to_string()
                    } else if matches!(exec_out, Some("failed" | "timed_out")) {
                        " · execution failed".to_string()
                    } else {
                        String::new()
                    };
                    step_context = Some(format!(
                        "Step S{:02} · {} ({}){}{}",
                        idx + 1,
                        step.worker,
                        model,
                        status_note,
                        exit_str
                    ));
                    if let Some(e) = step_err {
                        step_detail_parts.push(format!("Step error: {e}"));
                    }
                    if let Some(s) = agentlet_sum {
                        step_detail_parts.push(format!("Review summary: {s}"));
                    }
                    if let Some(err) = ver_stderr {
                        step_detail_parts.push(format!("Verifier stderr:\n{err}"));
                    } else if matches!(ver_out, Some("failed" | "timed_out")) {
                        if let Some(out) = ver_stdout {
                            step_detail_parts.push(format!("Verifier stdout:\n{out}"));
                        }
                    }
                    if let Some(sum) = agent_sum {
                        if ver_stderr.is_none() && agentlet_sum.is_none() {
                            step_detail_parts.push(format!("Worker output:\n{sum}"));
                        }
                    }
                    if let Some(arr) = agentlet
                        .and_then(|a| a.get("findings"))
                        .and_then(|f| f.as_array())
                    {
                        for f in arr {
                            let sev = f.get("severity").and_then(|v| v.as_str()).unwrap_or("note");
                            let msg = f.get("message").and_then(|v| v.as_str()).unwrap_or("");
                            if !msg.is_empty() {
                                findings.push((sev.to_string(), msg.to_string()));
                            }
                        }
                    }
                    break;
                }
            }

            if step_detail_parts.is_empty() {
                if let Some(rej_err) = wf.decisions.iter().rev().find_map(|d| d.error.as_deref()) {
                    step_detail_parts.push(format!("Planner decision rejected: {rej_err}"));
                }
            }
            if let Some(esc) = wf.decisions.iter().rev().find_map(|d| {
                (d.decision.get("action").and_then(|v| v.as_str()) == Some("escalate"))
                    .then(|| d.decision.get("reason").and_then(|v| v.as_str()))
                    .flatten()
            }) {
                if task_err.as_deref() != Some(esc) {
                    step_detail_parts.push(format!("Escalation reason: {esc}"));
                }
            }
        }
    }

    let primary = act_err
        .as_deref()
        .or(task_err.as_deref())
        .or(int_err.as_deref())
        .or(conflict_err.as_deref())
        .or(attempt_err.as_deref())
        .map(str::to_string)
        .or_else(|| {
            if is_problem_status {
                Some(format!(
                    "Task ended in status '{}'.",
                    status_label(&task.status)
                ))
            } else {
                None
            }
        })?;

    if let Some(att) = &attempt_err {
        if att != &primary {
            step_detail_parts.push(format!("Attempt error: {att}"));
        }
    }
    if let Some(conf) = &conflict_err {
        if conf != &primary {
            step_detail_parts.push(format!("Repository conflict: {conf}"));
        }
    }
    if let Some(ie) = &int_err {
        if ie != &primary {
            step_detail_parts.push(format!("Integration error: {ie}"));
        }
    }

    let details_block = (!step_detail_parts.is_empty()).then(|| step_detail_parts.join("\n\n"));

    let mut report = vec![
        format!("Task: {}", task.objective),
        format!(
            "ID: {} ({}) · Status: {} · Repo: {}",
            task.id, task.kind, task.status, task.repository
        ),
        format!("Error: {primary}"),
    ];
    if let Some(sc) = &step_context {
        report.push(format!("Location: {sc}"));
    }
    if !findings.is_empty() {
        report.push("Findings:".to_string());
        for (sev, msg) in &findings {
            report.push(format!("  [{sev}] {msg}"));
        }
    }
    if let Some(det) = &details_block {
        report.push(String::new());
        report.push(det.clone());
    }

    Some(TaskDiagnostic {
        headline: primary,
        step_context,
        details_block,
        findings,
        clipboard_report: report.join("\n"),
    })
}

fn row_self_matches_query(row: &TaskRow, q: &str) -> bool {
    q.is_empty()
        || row.objective.to_ascii_lowercase().contains(q)
        || row.id.to_ascii_lowercase().contains(q)
        || row.kind.to_ascii_lowercase().contains(q)
        || row.backend.to_ascii_lowercase().contains(q)
        || row.status.to_ascii_lowercase().contains(q)
        || row
            .step_label
            .as_deref()
            .is_some_and(|s| s.to_ascii_lowercase().contains(q))
}

fn row_matches_query(row: &TaskRow, q: &str) -> bool {
    row_self_matches_query(row, q) || row.children.iter().any(|c| row_matches_query(c, q))
}

#[component]
pub(crate) fn TaskGrid(
    rows: FetchState<Vec<TaskRow>>,
    selected: Signal<Option<String>>,
    compact: bool,
    clearing: bool,
    clear_error: Option<String>,
    clear_notice: Option<String>,
    on_clear: EventHandler<&'static str>,
) -> Element {
    let mut selected = selected;
    let mut filter = use_signal(|| TaskFilter::All);
    let mut query = use_signal(String::new);
    let mut expanded_overrides = use_signal(std::collections::HashMap::<String, bool>::new);

    match rows {
        FetchState::Idle => rsx! {
            div { class: "cf-surface rounded-lg p-6 text-center",
                p { class: "text-xs uppercase tracking-widest mb-1", style: "color: #8D9DAD;", "Tasks" }
                p { class: "text-neutral-400 text-xs sm:text-sm",
                    "Connect above to view and review tasks."
                }
            }
        },
        FetchState::Loading => rsx! {
            div { class: "cf-surface rounded-lg p-6 text-neutral-400 text-xs", "Loading tasks…" }
        },
        FetchState::Error(err) => rsx! {
            div { class: "rounded-lg border border-red-800 bg-red-950/30 p-4 text-red-300 text-xs break-words flex items-start justify-between gap-3",
                span { "Error loading tasks: {err}" }
                CopyButton { text: err.clone(), label: "Copy", subtle: false }
            }
        },
        FetchState::Loaded(all_rows) if all_rows.is_empty() => rsx! {
            div { class: "cf-surface rounded-lg p-6 text-center",
                if let Some(notice) = &clear_notice {
                    p { class: "text-xs text-[#66D8CE] mb-2", "{notice}" }
                }
                if let Some(err) = &clear_error {
                    p { class: "text-xs text-red-400 mb-2 break-words", "{err}" }
                }
                p { class: "text-neutral-400 text-sm", "No runs yet. Dispatch an objective above to begin." }
            }
        },
        FetchState::Loaded(all_rows) => {
            let active_count = all_rows
                .iter()
                .filter(|r| matches_filter(r, TaskFilter::Active))
                .count();
            let attention_count = all_rows
                .iter()
                .filter(|r| matches_filter(r, TaskFilter::NeedsAction))
                .count();
            let done_count = all_rows
                .iter()
                .filter(|r| matches_filter(r, TaskFilter::Done))
                .count();
            let failed_count = all_rows
                .iter()
                .filter(|r| matches_filter(r, TaskFilter::Failed))
                .count();

            let q = query().trim().to_ascii_lowercase();
            let active_filter = filter();
            let filtered: Vec<&TaskRow> = all_rows
                .iter()
                .filter(|r| matches_filter(r, active_filter))
                .filter(|r| row_matches_query(r, &q))
                .collect();

            let filter_items = [
                (TaskFilter::All, format!("All ({})", all_rows.len())),
                (
                    TaskFilter::NeedsAction,
                    format!("Action ({attention_count})"),
                ),
                (TaskFilter::Active, format!("Active ({active_count})")),
                (TaskFilter::Done, format!("Done ({done_count})")),
                (TaskFilter::Failed, format!("Failed ({failed_count})")),
            ];

            rsx! {
                div { class: "cf-surface rounded-lg overflow-hidden",
                    div { class: "px-3.5 py-3 border-b flex flex-col gap-2.5", style: "border-color: #202D3A; background-color: rgba(32, 45, 58, 0.22);",
                        div { class: "flex flex-wrap items-center justify-between gap-2",
                            div { class: "flex items-center gap-2",
                                span { class: "text-xs font-semibold tracking-wider uppercase", style: "color: #66D8CE;", "Tasks" }
                                if active_count > 0 {
                                    span { class: "inline-flex items-center gap-1.5 text-[11px] font-semibold px-2 py-0.5 rounded-full bg-[#FF754F]/15 text-[#FF754F]",
                                        span { class: "cf-pulse-dot", style: "background-color: #FF754F;" }
                                        "{active_count} live"
                                    }
                                }
                                if attention_count > 0 {
                                    span { class: "text-[11px] font-semibold px-2 py-0.5 rounded-full bg-sky-950/80 text-sky-300",
                                        "{attention_count} waiting on you"
                                    }
                                }
                            }
                            if failed_count > 0 || done_count > 0 {
                                div { class: "flex items-center gap-2 text-[11px]",
                                    if failed_count > 0 {
                                        button {
                                            r#type: "button",
                                            class: "text-red-300/90 hover:text-red-200 disabled:opacity-45",
                                            disabled: clearing,
                                            title: "Remove failed, timed-out, cancelled, and needs-work runs",
                                            onclick: move |_| on_clear.call("failed"),
                                            if clearing { "Clearing…" } else { "Clear failed" }
                                        }
                                    }
                                    if done_count > 0 {
                                        button {
                                            r#type: "button",
                                            class: "text-[#8D9DAD] hover:text-[#F2F5F7] disabled:opacity-45",
                                            disabled: clearing,
                                            title: "Remove accepted runs",
                                            onclick: move |_| on_clear.call("done"),
                                            if clearing { "Clearing…" } else { "Clear done" }
                                        }
                                    }
                                    if failed_count > 0 && done_count > 0 {
                                        button {
                                            r#type: "button",
                                            class: "text-[#8D9DAD] hover:text-amber-300 disabled:opacity-45",
                                            disabled: clearing,
                                            onclick: move |_| on_clear.call("finished"),
                                            "Clear all finished"
                                        }
                                    }
                                }
                            }
                        }

                        input {
                            class: "bg-[#0C1118] border border-[#202D3A] focus:border-[#FF754F] outline-none rounded px-2.5 py-1.5 text-xs w-full text-[#F2F5F7] placeholder:text-[#8D9DAD]/60",
                            value: "{query}",
                            oninput: move |e| query.set(e.value()),
                            placeholder: "Search tasks…",
                        }

                        div { class: "flex flex-wrap items-center gap-1.5",
                            for (variant, label) in filter_items {
                                button {
                                    r#type: "button",
                                    class: if active_filter == variant {
                                        "text-[11px] font-semibold px-2.5 py-1 rounded border border-[#FF754F] bg-[#FF754F]/15 text-[#FF754F] transition-colors"
                                    } else {
                                        "text-[11px] px-2.5 py-1 rounded border border-transparent text-[#8D9DAD] hover:text-[#F2F5F7] hover:bg-[#0C1118]/60 transition-colors"
                                    },
                                    onclick: move |_| filter.set(variant),
                                    "{label}"
                                }
                            }
                        }

                        if let Some(notice) = &clear_notice {
                            p { class: "text-[11px] text-[#66D8CE]", "{notice}" }
                        }
                        if let Some(err) = &clear_error {
                            p { class: "text-[11px] text-red-400 break-words", "Clear failed: {err}" }
                        }
                    }

                    if filtered.is_empty() {
                        div { class: "p-6 text-center text-xs text-[#8D9DAD]",
                            "No tasks match this filter."
                        }
                    } else {
                        div {
                            class: if compact {
                                "flex flex-col divide-y divide-[#202D3A]/70 md:max-h-[calc(100vh-12rem)] overflow-y-auto cf-scroll"
                            } else {
                                "flex flex-col divide-y divide-[#202D3A]/70"
                            },
                            for row in filtered.iter() {
                                {
                                    let sid = short_id(&row.id);
                                    let badge = status_badge_class(&row.status);
                                    let st_label = status_label(&row.status);
                                    let live = is_live_status(&row.status);
                                    let current_selected = selected();
                                    let is_selected =
                                        current_selected.as_deref() == Some(row.id.as_str());
                                    let has_children = !row.children.is_empty();
                                    let child_selected = has_children
                                        && row.children.iter().any(|c| {
                                            current_selected.as_deref() == Some(c.id.as_str())
                                        });
                                    let child_matched_search = has_children
                                        && !q.is_empty()
                                        && !row_self_matches_query(row, &q)
                                        && row.children.iter().any(|c| row_matches_query(c, &q));
                                    let is_expanded = if !has_children {
                                        false
                                    } else if child_selected || child_matched_search {
                                        true
                                    } else {
                                        expanded_overrides
                                            .read()
                                            .get(&row.id)
                                            .copied()
                                            .unwrap_or(live)
                                    };
                                    let card_bg = if is_selected {
                                        "bg-[#202D3A]/65 border-l-2 border-l-[#FF754F]"
                                    } else if child_selected {
                                        "bg-[#202D3A]/25 border-l-2 border-l-[#66D8CE]/70"
                                    } else {
                                        "hover:bg-[#202D3A]/30 border-l-2 border-l-transparent"
                                    };
                                    let id = row.id.clone();
                                    let toggle_id = row.id.clone();
                                    let step_count = if has_children {
                                        row.children.len()
                                    } else {
                                        row.units
                                    };
                                    let steps_text = if step_count == 1 {
                                        "1 step".to_string()
                                    } else {
                                        format!("{step_count} steps")
                                    };
                                    let cue = match row.status.as_str() {
                                        "awaiting_review" => Some(("Review →", "text-sky-300")),
                                        "awaiting_approval" => Some(("Approve →", "text-amber-300")),
                                        "failed" | "timed_out" | "needs_work" | "rejected" => {
                                            Some(("Inspect error →", "text-red-300"))
                                        }
                                        _ => None,
                                    };
                                    let left_pad = if has_children { "pl-1 pr-3.5" } else { "px-3.5" };

                                    rsx! {
                                        div {
                                            key: "{row.id}",
                                            class: "flex flex-col",
                                            div {
                                                class: "flex items-stretch transition-colors {card_bg}",
                                                if has_children {
                                                    button {
                                                        r#type: "button",
                                                        class: "w-7 shrink-0 flex items-start justify-center pt-3.5 text-[#8D9DAD] hover:text-[#66D8CE] transition-colors",
                                                        title: if is_expanded { "Collapse steps" } else { "Expand steps" },
                                                        aria_expanded: "{is_expanded}",
                                                        onclick: move |_| {
                                                            expanded_overrides
                                                                .write()
                                                                .insert(toggle_id.clone(), !is_expanded);
                                                        },
                                                        span {
                                                            class: if is_expanded {
                                                                "inline-block text-[10px] leading-none transition-transform rotate-90"
                                                            } else {
                                                                "inline-block text-[10px] leading-none transition-transform"
                                                            },
                                                            "▸"
                                                        }
                                                    }
                                                }
                                                button {
                                                    r#type: "button",
                                                    class: "flex-1 min-w-0 text-left {left_pad} py-3",
                                                    onclick: move |_| selected.set(Some(id.clone())),
                                                    div { class: "flex items-center justify-between gap-2 mb-1.5",
                                                        div { class: "flex items-center gap-1.5 min-w-0",
                                                            span { class: "inline-flex items-center gap-1.5 text-[10px] font-semibold px-2 py-0.5 rounded border {badge}",
                                                                if live {
                                                                    span { class: "cf-pulse-dot", style: "background-color: #FF754F;" }
                                                                }
                                                                "{st_label}"
                                                            }
                                                            if has_children {
                                                                span { class: "text-[10px] font-mono px-1.5 py-0.5 rounded bg-[#0C1118] border border-[#202D3A] text-[#66D8CE]",
                                                                    "{steps_text}"
                                                                }
                                                            }
                                                            if let Some(alert_msg) = &row.alert {
                                                                span {
                                                                    class: "text-[10px] font-semibold px-1.5 py-0.5 rounded bg-amber-950/80 border border-amber-600/70 text-amber-300 truncate",
                                                                    title: "{alert_msg}",
                                                                    "⚠ Overtime"
                                                                }
                                                            }
                                                        }
                                                        span { class: "text-[11px] font-mono text-[#8D9DAD] shrink-0", "#{sid}" }
                                                    }
                                                    p {
                                                        class: "text-xs sm:text-[13px] font-medium text-[#F2F5F7] cf-clamp-2 leading-snug break-words",
                                                        title: "{row.objective}",
                                                        "{row.objective}"
                                                    }
                                                    div { class: "flex items-center justify-between gap-2 mt-1.5 text-[11px] text-[#8D9DAD]",
                                                        span { class: "truncate",
                                                            if row.backend.is_empty() {
                                                                "{row.kind} · {steps_text}"
                                                            } else {
                                                                "{row.kind} · {row.backend} · {steps_text}"
                                                            }
                                                        }
                                                        if let Some((cue_label, cue_cls)) = cue {
                                                            span { class: "shrink-0 font-semibold {cue_cls}", "{cue_label}" }
                                                        } else if let Some(review) = &row.review {
                                                            span { class: "shrink-0 truncate max-w-[45%] text-[#66D8CE]", "✓ {review}" }
                                                        }
                                                    }
                                                }
                                            }
                                            if is_expanded {
                                                div { class: "bg-[#090E14]/80 border-t border-[#202D3A]/45 pl-4 pr-3 py-2",
                                                    div { class: "border-l border-[#202D3A] pl-2.5 flex flex-col gap-1.5",
                                                        for (cidx, child) in row.children.iter().enumerate() {
                                                            {
                                                                let c_id = child.id.clone();
                                                                let c_sid = short_id(&child.id);
                                                                let c_badge = status_badge_class(&child.status);
                                                                let c_label = status_label(&child.status);
                                                                let c_live = is_live_status(&child.status);
                                                                let c_selected = current_selected.as_deref() == Some(child.id.as_str());
                                                                let step_tag = child
                                                                    .step_label
                                                                    .clone()
                                                                    .unwrap_or_else(|| format!("S{:02}", cidx + 1));
                                                                let child_cls = if c_selected {
                                                                    "bg-[#202D3A]/70 border-[#FF754F]"
                                                                } else {
                                                                    "bg-[#0E151F]/80 border-[#202D3A]/80 hover:border-[#66D8CE]/45 hover:bg-[#131C28]"
                                                                };
                                                                rsx! {
                                                                    button {
                                                                        key: "{child.id}",
                                                                        r#type: "button",
                                                                        class: "w-full text-left rounded border px-2.5 py-2 transition-colors {child_cls}",
                                                                        onclick: move |_| selected.set(Some(c_id.clone())),
                                                                        div { class: "flex items-center justify-between gap-2 mb-1",
                                                                            div { class: "flex items-center gap-1.5 min-w-0",
                                                                                span { class: "font-mono text-[10px] font-semibold text-[#66D8CE] truncate",
                                                                                    "{step_tag}"
                                                                                }
                                                                                span { class: "inline-flex items-center gap-1 text-[10px] font-semibold px-1.5 py-0.5 rounded border shrink-0 {c_badge}",
                                                                                    if c_live {
                                                                                        span { class: "cf-pulse-dot", style: "background-color: #FF754F;" }
                                                                                    }
                                                                                    "{c_label}"
                                                                                }
                                                                                if let Some(c_alert) = &child.alert {
                                                                                    span {
                                                                                        class: "text-[10px] font-semibold px-1.5 py-0.5 rounded bg-amber-950/80 border border-amber-600/70 text-amber-300 shrink-0",
                                                                                        title: "{c_alert}",
                                                                                        "⚠ Overtime"
                                                                                    }
                                                                                }
                                                                            }
                                                                            span { class: "text-[10px] font-mono text-[#8D9DAD] shrink-0",
                                                                                if child.backend.is_empty() {
                                                                                    "#{c_sid}"
                                                                                } else {
                                                                                    "{child.backend} · #{c_sid}"
                                                                                }
                                                                            }
                                                                        }
                                                                        p {
                                                                            class: "text-[11px] text-[#8D9DAD] truncate",
                                                                            title: "{child.objective}",
                                                                            "{child.objective}"
                                                                        }
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[component]
pub(crate) fn ProgressPanel(progress: AgentProgress) -> Element {
    let phase = if progress.phase == "generation" {
        "Running Agent"
    } else {
        "Verifying Changes"
    };
    let activity = if progress.finished {
        "Finished"
    } else if progress.stale {
        "Updates delayed"
    } else if progress.last_activity_at.is_none() {
        "Starting up…"
    } else if progress.idle_ms < 5_000 {
        "Writing output / editing files"
    } else {
        "Thinking…"
    };
    let elapsed = duration(progress.elapsed_ms);
    let hard = duration(progress.hard_limit_ms);
    let remaining = duration(progress.hard_limit_ms.saturating_sub(progress.elapsed_ms));
    let is_overtime = progress.overtime || progress.elapsed_ms >= progress.hard_limit_ms;
    let overtime_str = duration(if progress.overtime_ms > 0 {
        progress.overtime_ms
    } else {
        progress.elapsed_ms.saturating_sub(progress.hard_limit_ms)
    });

    let hard_pct = if progress.hard_limit_ms > 0 {
        ((progress.elapsed_ms as f64 / progress.hard_limit_ms as f64) * 100.0).min(100.0) as u32
    } else {
        0
    };
    let bar_color = if !progress.limit_enforced && is_overtime {
        "bg-amber-400"
    } else if hard_pct > 90 && progress.limit_enforced {
        "bg-red-500"
    } else if hard_pct > 70 {
        "bg-amber-500"
    } else {
        "bg-[#66D8CE]"
    };
    let section_style = if !progress.limit_enforced && is_overtime {
        "border-color: rgba(245, 158, 11, 0.65); background-color: rgba(245, 158, 11, 0.08);"
    } else {
        "border-color: rgba(255, 117, 79, 0.45); background-color: rgba(255, 117, 79, 0.06);"
    };

    rsx! {
        section {
            class: "rounded-lg border px-3.5 py-3",
            style: "{section_style}",
            aria_label: "Agent progress",
            div { class: "flex flex-wrap items-center justify-between gap-2 mb-2",
                div { class: "flex items-center gap-2",
                    if !progress.finished {
                        span { class: "cf-pulse-dot", style: "background-color: #FF754F;" }
                    }
                    span { class: "text-xs font-semibold text-[#FF754F]", "{phase}" }
                    span { class: "text-xs text-[#F2F5F7]", "· {elapsed}" }
                    if !progress.limit_enforced && is_overtime {
                        span { class: "text-[11px] font-semibold px-1.5 py-0.5 rounded bg-amber-950/80 border border-amber-600/70 text-amber-300",
                            "⚠ +{overtime_str} overtime"
                        }
                    }
                }
                span { class: "text-xs text-[#66D8CE]", "{activity}" }
            }

            div { class: "flex justify-between text-[11px] mb-1 text-[#8D9DAD]",
                if progress.limit_enforced {
                    span { "{elapsed} elapsed / {hard} limit" }
                    span { "{remaining} left" }
                } else if is_overtime {
                    span { class: "text-amber-200", "{elapsed} elapsed (passed {hard} target)" }
                    span { class: "text-amber-300", "Still running · alert only" }
                } else {
                    span { "{elapsed} elapsed / {hard} target (alert only)" }
                    span { "{remaining} to alert" }
                }
            }
            div { class: "w-full bg-[#0C1118] rounded-full h-1.5 overflow-hidden",
                div { class: "{bar_color} h-1.5 rounded-full transition-all duration-300", style: "width: {hard_pct}%" }
            }

            div { class: "flex flex-wrap items-center gap-x-3 gap-y-1 mt-2 text-[11px] text-[#8D9DAD]",
                span { "stdout {format_bytes(progress.stdout_bytes)}" }
                if progress.stderr_bytes > 0 {
                    span { class: "text-amber-300", "stderr {format_bytes(progress.stderr_bytes)}" }
                }
                if let Some(limit) = progress.idle_limit_ms {
                    span { "quiet {duration(progress.idle_ms)} / {duration(limit)}" }
                }
                if let Some(reason) = &progress.stop_reason {
                    span { class: "text-[#F2F5F7]", "stop: {reason}" }
                }
            }

            if !progress.changed_files.is_empty() {
                div { class: "flex flex-wrap gap-1.5 mt-2",
                    for file in &progress.changed_files {
                        span { class: "text-[11px] font-mono px-2 py-0.5 rounded bg-emerald-950/60 border border-emerald-800/70 text-emerald-300 break-all",
                            "{file}"
                        }
                    }
                }
            }
        }
    }
}

#[component]
pub(crate) fn TaskInspector(
    task: TaskDetail,
    actor: Signal<String>,
    session_principal: Option<String>,
    busy: bool,
    action_error: Option<String>,
    wide: Signal<bool>,
    on_close: EventHandler<()>,
    on_select_task: EventHandler<String>,
    on_approve: EventHandler<()>,
    on_approve_and_run: EventHandler<()>,
    on_run: EventHandler<()>,
    on_cancel: EventHandler<()>,
    on_delete: EventHandler<()>,
    on_review: EventHandler<bool>,
    on_prepare_integration: EventHandler<String>,
    on_integrate: EventHandler<bool>,
    on_depend: EventHandler<String>,
    on_retry: EventHandler<(bool, Option<usize>, Option<String>)>,
) -> Element {
    let mut actor = actor;
    let mut wide = wide;
    let mut active_tab = use_signal(|| InspectorTab::Pipeline);
    let mut selected_preview = use_signal(|| 0usize);
    let mut last_task_id = use_signal(|| task.id.clone());
    let mut retry_guidance = use_signal(String::new);

    if *last_task_id.read() != task.id {
        last_task_id.set(task.id.clone());
        active_tab.set(InspectorTab::Pipeline);
        selected_preview.set(0);
        retry_guidance.set(String::new());
    }

    let is_workflow_child = task.parent_workflow_id.is_some();
    let can_cancel = matches!(
        task.status.as_str(),
        "awaiting_approval" | "approved" | "preparing_workspace" | "running" | "verifying"
    );
    let can_delete = !is_workflow_child
        && !is_live_status(&task.status)
        && !task
            .integration
            .as_ref()
            .is_some_and(|p| matches!(p.status.as_str(), "applying" | "rolling_back" | "unknown"));
    let has_actor = session_principal.is_some() || !actor().trim().is_empty();
    let badge = status_badge_class(&task.status);
    let st_label = status_label(&task.status);
    let live = is_live_status(&task.status);
    let sid = short_id(&task.id);
    let repo_name = repo_basename(&task.repository);

    let needs_approval = !is_workflow_child && task.status == "awaiting_approval";
    let can_run = !is_workflow_child && task.status == "approved";
    let can_review =
        !is_workflow_child && task.status == "awaiting_review" && task.evidence_digest.is_some();
    let can_retry = !is_workflow_child
        && matches!(
            task.status.as_str(),
            "needs_work" | "failed" | "timed_out" | "cancelled" | "rejected" | "unknown"
        );
    let can_prepare_int = !is_workflow_child
        && task.status == "accepted"
        && matches!(task.kind.as_str(), "workflow" | "agent")
        && task.integration.is_none();
    let can_apply_int = task
        .integration
        .as_ref()
        .is_some_and(|p| p.status == "prepared");
    let can_restore_int = task.integration.as_ref().is_some_and(|p| {
        matches!(
            p.status.as_str(),
            "applied" | "unknown" | "applying" | "rolling_back"
        )
    });
    let needs_actor_input = session_principal.is_none()
        && (needs_approval
            || can_review
            || can_retry
            || can_prepare_int
            || can_apply_int
            || can_restore_int
            || can_cancel);

    let blocking_deps: Vec<_> = task
        .depends_on
        .iter()
        .filter(|d| d.status != "accepted")
        .collect();

    let preview_count: usize = task.attempts.iter().map(|a| a.previews.len()).sum();
    let has_live_preview = task.attempts.iter().any(|a| {
        a.previews
            .iter()
            .any(|p| p.label.to_ascii_lowercase().starts_with("live "))
    });
    let diff_previews: Vec<Preview> = task
        .attempts
        .iter()
        .flat_map(|a| a.previews.iter().cloned())
        .filter(|p| is_diff_preview(p) && !p.text.trim().is_empty())
        .collect();

    let diagnostic = collect_task_diagnostic(&task, action_error.as_deref());
    let ver_status = latest_verification_status(&task);
    let current_tab = active_tab();

    let outcome_summary = task
        .workflow
        .as_ref()
        .and_then(|w| w.summary.as_ref())
        .cloned()
        .or_else(|| {
            task.workflow
                .as_ref()
                .and_then(|w| w.steps.last())
                .and_then(|s| s.result.as_ref())
                .and_then(|r| {
                    r.get("agentlet_result")
                        .and_then(|a| a.get("summary"))
                        .or_else(|| r.get("agent_summary"))
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                })
        });

    rsx! {
        div { class: "cf-surface rounded-lg overflow-hidden flex flex-col",
            // ── TOP HEADER: CLEAN IDENTITY & OBJECTIVE ──
            div { class: "px-4 py-3.5 border-b", style: "border-color: #202D3A; background-color: rgba(32, 45, 58, 0.22);",
                div { class: "flex items-center justify-between gap-2 mb-2",
                    div { class: "flex flex-wrap items-center gap-2 min-w-0 text-xs",
                        span { class: "inline-flex items-center gap-1.5 text-[11px] font-semibold px-2.5 py-0.5 rounded border {badge}",
                            if live {
                                span { class: "cf-pulse-dot", style: "background-color: #FF754F;" }
                            }
                            "{st_label}"
                        }
                        span { class: "text-[#8D9DAD] font-mono", "#{sid}" }
                        span { class: "text-[#8D9DAD]", "· {repo_name}" }
                        if let Some(branch) = &task.branch {
                            span { class: "font-mono text-[#66D8CE]", "· ⎇ {branch}" }
                        }
                    }
                    div { class: "flex items-center gap-1.5 shrink-0",
                        if can_delete {
                            button {
                                r#type: "button",
                                class: "text-xs px-2.5 py-1 rounded border border-[#202D3A] bg-[#0C1118] text-red-300/90 hover:border-red-700/70 hover:text-red-200 disabled:opacity-45 transition-colors",
                                disabled: busy,
                                onclick: move |_| on_delete.call(()),
                                "Delete"
                            }
                        }
                        button {
                            r#type: "button",
                            class: "hidden md:inline-flex items-center text-xs px-2.5 py-1 rounded border border-[#202D3A] bg-[#0C1118] text-[#8D9DAD] hover:text-[#F2F5F7] transition-colors",
                            onclick: move |_| wide.set(!wide()),
                            if wide() { "⤡ Split" } else { "⤢ Expand" }
                        }
                        button {
                            r#type: "button",
                            class: "inline-flex items-center justify-center text-xs px-2.5 py-1 rounded border border-[#202D3A] bg-[#0C1118] text-[#8D9DAD] hover:text-[#F2F5F7] transition-colors",
                            title: "Close",
                            onclick: move |_| {
                                wide.set(false);
                                on_close.call(());
                            },
                            "✕"
                        }
                    }
                }

                h2 { class: "text-sm sm:text-base font-semibold text-[#F2F5F7] leading-snug break-words",
                    "{task.objective}"
                }
                if let Some(parent_id) = &task.parent_workflow_id {
                    {
                        let parent_id_btn = parent_id.clone();
                        let parent_short = short_id(parent_id);
                        rsx! {
                            div { class: "mt-2.5 pt-2.5 border-t border-[#202D3A]/70 flex flex-wrap items-center justify-between gap-2 text-xs",
                                span { class: "text-[#8D9DAD]",
                                    "Step of workflow "
                                    span { class: "font-mono text-[#66D8CE]", "#{parent_short}" }
                                    " — review, retry, and integrate on the parent workflow."
                                }
                                button {
                                    r#type: "button",
                                    class: "text-xs font-semibold text-[#66D8CE] hover:underline shrink-0",
                                    onclick: move |_| on_select_task.call(parent_id_btn.clone()),
                                    "View Parent Workflow →"
                                }
                            }
                        }
                    }
                }
            }

            // ── FLOATING TOP SECTION: ERRORS + NEXT ACTION / VALIDATION (NO DIGGING) ──
            div { class: "p-4 flex flex-col gap-3 border-b", style: "border-color: #202D3A; background-color: rgba(12, 17, 24, 0.55);",
                // 1. FLOATING ERROR / CANCELLATION CARD
                if let Some(diag) = &diagnostic {
                    if task.status == "cancelled" {
                        div { class: "rounded-lg border border-neutral-700/80 bg-neutral-900/60 p-3 flex flex-wrap items-center justify-between gap-2 text-xs text-neutral-300",
                            span { class: "font-semibold text-neutral-200", "✕ Cancelled by operator" }
                            if preview_count > 0 {
                                button {
                                    r#type: "button",
                                    class: "text-[11px] font-medium text-[#66D8CE] hover:underline",
                                    onclick: move |_| active_tab.set(InspectorTab::DiffLogs),
                                    "Logs ({preview_count})"
                                }
                            }
                        }
                    } else {
                        div { class: "rounded-lg border border-red-700/80 bg-red-950/35 p-3.5",
                            div { class: "flex flex-wrap items-center justify-between gap-2 mb-1.5",
                                div { class: "flex items-center gap-2",
                                    span { class: "text-xs font-bold text-red-300", "✕ What went wrong" }
                                    if let Some(loc) = &diag.step_context {
                                        span { class: "text-[11px] font-mono text-red-200/80", "· {loc}" }
                                    }
                                }
                                div { class: "flex items-center gap-2",
                                    if preview_count > 0 {
                                        button {
                                            r#type: "button",
                                            class: "text-[11px] font-medium text-red-200/85 hover:text-white underline",
                                            onclick: move |_| active_tab.set(InspectorTab::DiffLogs),
                                            "Logs ({preview_count})"
                                        }
                                    }
                                    CopyButton {
                                        text: diag.clipboard_report.clone(),
                                        label: "Copy Error",
                                        subtle: false,
                                    }
                                }
                            }

                            p { class: "text-xs sm:text-[13px] text-red-100 leading-relaxed break-words",
                                "{diag.headline}"
                            }

                            if !diag.findings.is_empty() {
                                div { class: "mt-2 flex flex-col gap-1",
                                    for (fidx, (sev, msg)) in diag.findings.iter().enumerate() {
                                        div { key: "{fidx}", class: "text-xs text-red-100/90 break-words",
                                            span { class: "font-mono text-[10px] uppercase text-amber-300 mr-1.5", "[{sev}]" }
                                            "{msg}"
                                        }
                                    }
                                }
                            }

                            if let Some(det) = &diag.details_block {
                                pre { class: "mt-2.5 cf-pre-scroll cf-scroll rounded border border-red-900/60 bg-[#080C12] p-2.5 text-[11px] text-red-200 whitespace-pre-wrap break-words max-h-52 overflow-y-auto",
                                    "{det}"
                                }
                            }
                        }
                    }
                }

                if let Some(warn) = &task.workspace_warning {
                    div { class: "rounded-lg border border-amber-700/70 bg-amber-950/30 px-3 py-2 text-xs text-amber-200 break-words",
                        "⚠ {warn}"
                    }
                }

                if !blocking_deps.is_empty() {
                    div { class: "rounded-lg border border-amber-700/70 bg-amber-950/30 px-3 py-2 text-xs text-amber-200",
                        "Waiting on dependencies: "
                        for (idx, dep) in blocking_deps.iter().enumerate() {
                            span { class: "font-mono",
                                if idx > 0 { ", " }
                                "{short_id(&dep.id)} ({dep.status})"
                            }
                        }
                    }
                }

                // 2. LIVE PROGRESS (shown cleanly only while task is actually live)
                if live {
                    if let Some(progress) = &task.progress {
                        ProgressPanel { progress: progress.clone() }
                    } else if let Some(pp) = task.workflow.as_ref().and_then(|w| w.planner_progress.as_ref()) {
                        div {
                            class: "rounded-lg border px-3.5 py-2.5 flex flex-wrap items-center justify-between gap-2 text-xs",
                            style: "border-color: rgba(255, 117, 79, 0.45); background-color: rgba(255, 117, 79, 0.06);",
                            div { class: "flex items-center gap-2",
                                span { class: "cf-pulse-dot", style: "background-color: #FF754F;" }
                                span { class: "font-semibold text-[#FF754F]", "Planner choosing next step" }
                                if let Some(wf) = &task.workflow {
                                    span { class: "text-[#8D9DAD]", "· {wf.planner_model}" }
                                }
                            }
                            span { class: "text-[#8D9DAD] font-mono text-[11px]",
                                "{duration(pp.elapsed_ms)} · {format_bytes(pp.output_bytes as u64)}"
                            }
                        }
                    } else {
                        div { class: "rounded-lg border border-amber-700/50 bg-amber-950/20 px-3.5 py-2.5 flex items-center gap-2 text-xs text-amber-200",
                            span { class: "cf-pulse-dot", style: "background-color: #F59E0B;" }
                            "Working…"
                        }
                    }
                }

                // 3. CLEAR "WHAT TO DO NEXT" ACTION & VALIDATION PANEL
                if needs_actor_input {
                    div { class: "flex flex-wrap items-center gap-2",
                        span { class: "text-xs text-[#8D9DAD]", "Your name:" }
                        input {
                            class: "bg-[#0C1118] border border-[#202D3A] focus:border-[#FF754F] outline-none rounded px-2.5 py-1.5 w-48 text-xs text-[#F2F5F7] placeholder:text-[#8D9DAD]/70",
                            value: "{actor}",
                            oninput: move |e| actor.set(e.value()),
                            placeholder: "actor name",
                        }
                    }
                }

                // State A: Waiting approval / ready to run
                if needs_approval || can_run {
                    div { class: "rounded-lg border border-[#202D3A] bg-[#0C1118] p-3.5 flex flex-col sm:flex-row sm:items-center justify-between gap-3",
                        div {
                            p { class: "text-xs sm:text-sm font-medium text-[#F2F5F7]",
                                if needs_approval {
                                    "Ready to start on {repo_name}."
                                } else {
                                    "Approved and ready to run."
                                }
                            }
                            p { class: "text-xs text-[#8D9DAD] mt-0.5",
                                if needs_approval {
                                    "Runs in an isolated worktree so your checkout stays untouched until you accept and apply."
                                } else {
                                    "Start execution when ready."
                                }
                            }
                        }
                        div { class: "flex flex-wrap items-center gap-2 shrink-0",
                            if needs_approval {
                                button {
                                    class: "cf-action-btn rounded px-4 py-2 text-xs disabled:opacity-45",
                                    disabled: busy || !has_actor,
                                    onclick: move |_| on_approve_and_run.call(()),
                                    if busy { "Starting…" } else { "Approve & Run" }
                                }
                                button {
                                    class: "rounded px-3 py-2 text-xs font-medium border border-[#202D3A] bg-[#111A26] hover:bg-[#202D3A]/60 text-[#F2F5F7] disabled:opacity-45",
                                    disabled: busy || !has_actor,
                                    onclick: move |_| on_approve.call(()),
                                    "Approve Only"
                                }
                            } else {
                                button {
                                    class: "cf-action-btn rounded px-4 py-2 text-xs disabled:opacity-45",
                                    disabled: busy,
                                    onclick: move |_| on_run.call(()),
                                    if busy { "Running…" } else { "Run Now" }
                                }
                            }
                            if can_cancel {
                                button {
                                    class: "rounded px-3 py-2 text-xs border border-[#202D3A] bg-[#0E151F] text-[#8D9DAD] hover:text-red-300 hover:border-red-900/60",
                                    onclick: move |_| on_cancel.call(()),
                                    "Cancel"
                                }
                            }
                        }
                    }
                }

                // State B: Live task stop control
                if live && can_cancel {
                    div { class: "flex items-center justify-between gap-2",
                        span { class: "text-xs text-[#8D9DAD]",
                            if let Some(principal) = &session_principal {
                                "Signed in as {principal}"
                            } else {
                                "Execution running in background worktree."
                            }
                        }
                        button {
                            class: "rounded px-3.5 py-1.5 text-xs font-semibold bg-red-950/70 hover:bg-red-900/80 border border-red-700/70 text-red-100",
                            onclick: move |_| on_cancel.call(()),
                            "Stop Run"
                        }
                    }
                }

                // State C: READY TO VALIDATE (Verification + Summary + Inline Diff + Accept/Reject right here!)
                if can_review {
                    div { class: "rounded-lg border border-sky-700/60 bg-sky-950/20 p-3.5 flex flex-col gap-3",
                        div { class: "flex flex-wrap items-center justify-between gap-2",
                            div { class: "flex flex-wrap items-center gap-2",
                                span { class: "text-xs sm:text-sm font-semibold text-[#F2F5F7]", "Review & Validate" }
                                if let Some((passed, exit_code, _)) = &ver_status {
                                    if *passed {
                                        span { class: "text-[11px] font-semibold px-2 py-0.5 rounded bg-emerald-950/80 border border-emerald-700/70 text-emerald-300",
                                            "✓ Verification passed"
                                        }
                                    } else {
                                        span { class: "text-[11px] font-semibold px-2 py-0.5 rounded bg-red-950/80 border border-red-700/70 text-red-300",
                                            if let Some(c) = exit_code {
                                                "✕ Verification failed (exit {c})"
                                            } else {
                                                "✕ Verification failed"
                                            }
                                        }
                                    }
                                }
                            }
                            div { class: "flex items-center gap-2 shrink-0",
                                button {
                                    class: "bg-emerald-500 hover:bg-emerald-400 text-[#0C1118] disabled:opacity-45 font-bold rounded px-4 py-2 text-xs",
                                    disabled: busy || !has_actor,
                                    onclick: move |_| on_review.call(true),
                                    if busy { "Accepting…" } else { "✓ Accept Changes" }
                                }
                                button {
                                    class: "rounded px-3.5 py-2 text-xs font-semibold border border-amber-700/70 bg-amber-950/50 hover:bg-amber-900/60 text-amber-200 disabled:opacity-45",
                                    disabled: busy || !has_actor,
                                    onclick: move |_| on_review.call(false),
                                    if busy { "Rejecting…" } else { "✕ Reject" }
                                }
                            }
                        }

                        if let Some(wf) = &task.workflow {
                            if !wf.pending_agentlet_gates.is_empty() {
                                p { class: "text-xs text-amber-200/90",
                                    "Pending gates: {wf.pending_agentlet_gates.join(\", \")}"
                                }
                            }
                        }

                        if let Some(summary) = &outcome_summary {
                            p { class: "text-xs sm:text-[13px] text-[#F2F5F7] leading-relaxed break-words",
                                "{summary}"
                            }
                        }

                        // Inline Diff Preview right inside Validation Card — zero tab digging!
                        if let Some(diff) = diff_previews.first() {
                            div { class: "rounded border border-[#202D3A] bg-[#080C12] overflow-hidden",
                                div { class: "px-3 py-1.5 border-b border-[#202D3A] bg-[#0C1118] flex flex-wrap items-center justify-between gap-2 text-[11px]",
                                    span { class: "font-semibold text-[#F2F5F7]", "{diff.label}" }
                                    div { class: "flex items-center gap-2",
                                        if preview_count > 1 {
                                            button {
                                                r#type: "button",
                                                class: "text-[#66D8CE] hover:underline",
                                                onclick: move |_| active_tab.set(InspectorTab::DiffLogs),
                                                "All logs & diffs ({preview_count}) →"
                                            }
                                        }
                                        CopyButton {
                                            text: diff.text.clone(),
                                            label: "Copy Diff",
                                            subtle: true,
                                        }
                                    }
                                }
                                DiffLinesView { text: diff.text.clone(), max_h_class: "max-h-60" }
                            }
                        } else if preview_count > 0 {
                            button {
                                r#type: "button",
                                class: "self-start text-xs font-medium text-[#66D8CE] hover:underline",
                                onclick: move |_| active_tab.set(InspectorTab::DiffLogs),
                                "Inspect captured logs ({preview_count}) →"
                            }
                        }
                    }
                }

                // State D: ACCEPTED — apply / merge smoothly in one place
                if task.status == "accepted" {
                    if let Some(plan) = &task.integration {
                        IntegrationActionCard {
                            plan: plan.clone(),
                            busy,
                            has_actor,
                            can_prepare: can_prepare_int,
                            can_apply: can_apply_int,
                            can_restore: can_restore_int,
                            reviewer: task.reviewer.clone(),
                            on_prepare_integration,
                            on_integrate,
                        }
                    } else {
                        div { class: "rounded-lg border border-emerald-700/50 bg-emerald-950/20 p-3.5 flex flex-col gap-3",
                            div { class: "flex flex-col sm:flex-row sm:items-center justify-between gap-3",
                                div {
                                    p { class: "text-xs sm:text-sm font-semibold text-emerald-300",
                                        if let Some(rev) = &task.reviewer {
                                            "✓ Accepted by {rev}"
                                        } else {
                                            "✓ Accepted"
                                        }
                                    }
                                    if let Some(summary) = &outcome_summary {
                                        p { class: "text-xs text-[#F2F5F7]/90 mt-1 break-words", "{summary}" }
                                    }
                                }
                                if can_prepare_int {
                                    div { class: "flex flex-wrap items-center gap-2 shrink-0",
                                        button {
                                            class: "cf-action-btn rounded px-3.5 py-2 text-xs disabled:opacity-45",
                                            disabled: busy || !has_actor,
                                            title: "Create an isolated Git worktree and branch for the unstaged changes, so integration works even if your main checkout has uncommitted work",
                                            onclick: move |_| on_prepare_integration.call("worktree".to_string()),
                                            if busy { "Preparing…" } else { "Prepare Worktree Integration" }
                                        }
                                        button {
                                            class: "rounded px-3.5 py-2 text-xs font-semibold border border-[#202D3A] bg-[#111A26] hover:bg-[#202D3A]/70 text-[#F2F5F7] disabled:opacity-45",
                                            disabled: busy || !has_actor,
                                            title: "Prepare unstaged changes directly in the repository checkout (requires a clean checkout)",
                                            onclick: move |_| on_prepare_integration.call("checkout".to_string()),
                                            if busy { "Preparing…" } else { "Prepare Checkout Integration" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                // State E: FAILED / STOPPED / NEEDS_WORK — clean resume or restart controls
                if can_retry {
                    div { class: "rounded-lg border border-[#202D3A] bg-[#0C1118] p-3.5 flex flex-col gap-2.5",
                        if task.workflow.is_some() {
                            input {
                                class: "bg-[#111A26] border border-[#202D3A] focus:border-[#FF754F] outline-none rounded px-3 py-2 text-xs text-[#F2F5F7] placeholder:text-[#8D9DAD]/70 w-full",
                                value: "{retry_guidance}",
                                oninput: move |e| retry_guidance.set(e.value()),
                                placeholder: "Optional note for retry (e.g. scope hint, fix instruction, or workaround)…",
                            }
                        }
                        div { class: "flex flex-wrap items-center gap-2",
                            if let Some(wf) = &task.workflow {
                                if !wf.steps.is_empty() {
                                    button {
                                        class: "cf-action-btn rounded px-4 py-2 text-xs disabled:opacity-45",
                                        disabled: busy || !has_actor,
                                        title: "Keep current worktree edits & checkpoints and continue",
                                        onclick: move |_| {
                                            let g = retry_guidance().trim().to_string();
                                            let guidance = (!g.is_empty()).then_some(g);
                                            on_retry.call((false, None, guidance));
                                        },
                                        if busy { "Resuming…" } else { "↻ Resume & Run" }
                                    }
                                }
                                button {
                                    class: "rounded px-3.5 py-2 text-xs font-medium border border-[#202D3A] bg-[#111A26] hover:bg-[#202D3A]/60 text-[#F2F5F7] disabled:opacity-45",
                                    disabled: busy || !has_actor,
                                    title: "Reset worktree to base commit and start from step 1",
                                    onclick: move |_| {
                                        let g = retry_guidance().trim().to_string();
                                        let guidance = (!g.is_empty()).then_some(g);
                                        on_retry.call((true, Some(1), guidance));
                                    },
                                    if busy { "Restarting…" } else { "⟲ Restart Fresh" }
                                }
                            } else {
                                button {
                                    class: "cf-action-btn rounded px-4 py-2 text-xs disabled:opacity-45",
                                    disabled: busy || !has_actor,
                                    onclick: move |_| on_retry.call((false, None, None)),
                                    if busy { "Retrying…" } else { "↻ Retry & Run" }
                                }
                                if task.kind == "agent" {
                                    button {
                                        class: "rounded px-3.5 py-2 text-xs font-medium border border-[#202D3A] bg-[#111A26] hover:bg-[#202D3A]/60 text-[#F2F5F7] disabled:opacity-45",
                                        disabled: busy || !has_actor,
                                        onclick: move |_| on_retry.call((true, None, None)),
                                        if busy { "Restarting…" } else { "⟲ Restart Fresh" }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // ── CALM 3-TAB BAR ──
            div { class: "cf-tab-strip flex items-center gap-1 px-4 pt-1 border-b", style: "border-color: #202D3A; background-color: #0C1118;",
                {
                    let tabs = [
                        (
                            InspectorTab::Pipeline,
                            if task.workflow.is_some() { "Steps".to_string() } else { "Overview".to_string() },
                            false,
                        ),
                        (
                            InspectorTab::DiffLogs,
                            if preview_count > 0 {
                                format!("Diff & Logs ({preview_count})")
                            } else {
                                "Diff & Logs".to_string()
                            },
                            has_live_preview,
                        ),
                        (InspectorTab::SpecsAudit, "Specs & Audit".to_string(), false),
                    ];
                    rsx! {
                        for (tab_id, label, pulse) in tabs {
                            button {
                                r#type: "button",
                                class: if current_tab == tab_id {
                                    "inline-flex items-center gap-1.5 px-3 py-2 text-xs font-semibold border-b-2 border-[#FF754F] text-[#F2F5F7] whitespace-nowrap shrink-0"
                                } else {
                                    "inline-flex items-center gap-1.5 px-3 py-2 text-xs font-medium border-b-2 border-transparent text-[#8D9DAD] hover:text-[#F2F5F7] whitespace-nowrap shrink-0 transition-colors"
                                },
                                onclick: move |_| active_tab.set(tab_id),
                                if pulse {
                                    span { class: "cf-pulse-dot", style: "background-color: #FF754F;" }
                                }
                                "{label}"
                            }
                        }
                    }
                }
            }

            // ── TAB BODY (natural calm flow, no cramped nested squares) ──
            div { class: "p-4",
                match current_tab {
                    InspectorTab::Pipeline => rsx! {
                        PipelineTab {
                            task: task.clone(),
                            can_retry: can_retry && !busy && has_actor,
                            on_open_diff: move |_| active_tab.set(InspectorTab::DiffLogs),
                            on_retry_step: move |step_num: usize| {
                                let g = retry_guidance().trim().to_string();
                                let guidance = (!g.is_empty()).then_some(g);
                                on_retry.call((false, Some(step_num), guidance));
                            },
                        }
                    },
                    InspectorTab::DiffLogs => rsx! {
                        DiffLogsTab {
                            task: task.clone(),
                            selected_preview,
                            wide: wide(),
                        }
                    },
                    InspectorTab::SpecsAudit => rsx! {
                        SpecsAndAuditTab {
                            task: task.clone(),
                            on_depend,
                        }
                    },
                }
            }
        }
    }
}

#[component]
fn IntegrationActionCard(
    plan: IntegrationView,
    busy: bool,
    has_actor: bool,
    can_prepare: bool,
    can_apply: bool,
    can_restore: bool,
    reviewer: Option<String>,
    on_prepare_integration: EventHandler<String>,
    on_integrate: EventHandler<bool>,
) -> Element {
    let short_base = short_id(&plan.repository.base);
    let is_worktree = plan.target == "worktree";
    let status_cls = match plan.status.as_str() {
        "applied" => "text-emerald-300",
        "prepared" => "text-sky-300",
        "failed" | "unknown" | "rolling_back" => "text-red-300",
        _ => "text-[#8D9DAD]",
    };

    rsx! {
        div { class: "rounded-lg border border-[#202D3A] bg-[#0C1118] p-3.5 flex flex-col gap-2.5",
            div { class: "flex flex-wrap items-center justify-between gap-2",
                div { class: "flex flex-wrap items-center gap-2 text-xs",
                    if let Some(rev) = &reviewer {
                        span { class: "font-semibold text-emerald-300", "✓ Accepted by {rev}" }
                        span { class: "text-[#8D9DAD]", "·" }
                    }
                    span { class: "font-semibold {status_cls}", "Integration plan: {plan.status}" }
                    span {
                        class: if is_worktree { "px-2 py-0.5 rounded bg-sky-950/60 border border-sky-700/50 text-sky-300 font-semibold" } else { "px-2 py-0.5 rounded bg-[#111A26] border border-[#202D3A] text-[#8D9DAD]" },
                        if is_worktree { "Worktree" } else { "Main Checkout" }
                    }
                    if let Some(branch) = &plan.worktree_branch {
                        span { class: "font-mono text-[#66D8CE]", "{branch}" }
                    }
                    span { class: "font-mono text-[#8D9DAD]", "base:{short_base}" }
                }
                div { class: "flex flex-wrap items-center gap-2",
                    if plan.status == "prepared" && can_prepare {
                        if is_worktree {
                            button {
                                class: "rounded px-3 py-1.5 text-xs font-medium border border-[#202D3A] bg-[#111A26] hover:bg-[#202D3A]/60 text-[#F2F5F7] disabled:opacity-45",
                                disabled: busy || !has_actor,
                                onclick: move |_| on_prepare_integration.call("checkout".to_string()),
                                "Switch to Checkout"
                            }
                        } else {
                            button {
                                class: "rounded px-3 py-1.5 text-xs font-medium border border-[#202D3A] bg-[#111A26] hover:bg-[#202D3A]/60 text-[#F2F5F7] disabled:opacity-45",
                                disabled: busy || !has_actor,
                                onclick: move |_| on_prepare_integration.call("worktree".to_string()),
                                "Switch to Worktree"
                            }
                        }
                    }
                    if can_apply {
                        button {
                            class: "bg-emerald-500 hover:bg-emerald-400 text-[#0C1118] font-bold rounded px-4 py-2 text-xs disabled:opacity-45",
                            disabled: busy || !has_actor,
                            onclick: move |_| on_integrate.call(false),
                            if busy {
                                "Applying…"
                            } else if is_worktree {
                                "Apply Unstaged to Worktree"
                            } else {
                                "Apply Unstaged to Repo"
                            }
                        }
                    }
                    if can_restore {
                        button {
                            class: "rounded px-3.5 py-2 text-xs font-semibold border border-amber-700/70 bg-amber-950/50 hover:bg-amber-900/60 text-amber-200 disabled:opacity-45",
                            disabled: busy || !has_actor,
                            onclick: move |_| on_integrate.call(true),
                            if plan.status == "applied" { "Restore Originals" } else { "Recover Originals" }
                        }
                    }
                }
            }

            p { class: "text-[11px] font-mono text-[#8D9DAD] break-all",
                "Destination: {plan.repository.checkout}"
            }

            if !plan.files.is_empty() {
                div { class: "flex flex-col gap-1",
                    for file in &plan.files {
                        {
                            let (tag, tag_cls) = if file.after_digest.is_none() {
                                ("DEL", "text-red-300")
                            } else if file.before_digest.is_none() {
                                ("ADD", "text-emerald-300")
                            } else {
                                ("MOD", "text-sky-300")
                            };
                            rsx! {
                                div { class: "flex items-center gap-2 text-xs font-mono px-2.5 py-1 rounded bg-[#0E151F]",
                                    span { class: "text-[10px] font-bold shrink-0 {tag_cls}", "{tag}" }
                                    span { class: "text-[#F2F5F7] break-all", "{file.path}" }
                                }
                            }
                        }
                    }
                }
            }

            p { class: "text-[11px] text-[#8D9DAD]",
                if plan.status == "prepared" {
                    if is_worktree {
                        "Applies unstaged changes inside the isolated worktree so your main checkout WIP stays untouched. Originals are journaled for safe restore."
                    } else {
                        "Applies as unstaged changes to your checkout. Originals are journaled so you can restore anytime."
                    }
                } else if plan.status == "applied" {
                    if is_worktree {
                        "Applied unstaged in the isolated worktree above. Review/commit on that branch or cherry-pick when ready."
                    } else {
                        "Applied unstaged to your checkout. Review with git diff and commit when ready."
                    }
                } else {
                    ""
                }
            }
        }
    }
}

#[component]
fn PipelineTab(
    task: TaskDetail,
    can_retry: bool,
    on_open_diff: EventHandler<()>,
    on_retry_step: EventHandler<usize>,
) -> Element {
    let preview_count: usize = task.attempts.iter().map(|a| a.previews.len()).sum();

    rsx! {
        div { class: "flex flex-col gap-3",
            if let Some(wf) = &task.workflow {
                {
                    let timeline = build_timeline(wf);
                    let steps_done = wf.steps.len();
                    let max_steps = parse_workflow_max_steps(&task.budget)
                        .unwrap_or_else(|| steps_done.max(1));
                    let wf_live = is_live_status(&task.status);
                    rsx! {
                        div { class: "flex flex-wrap items-center justify-between gap-2 text-xs text-[#8D9DAD]",
                            span { "Step {steps_done} of {max_steps} · {wf.phase}" }
                            if preview_count > 0 {
                                button {
                                    r#type: "button",
                                    class: "font-medium text-[#66D8CE] hover:underline",
                                    onclick: move |_| on_open_diff.call(()),
                                    "View diffs & logs ({preview_count}) →"
                                }
                            }
                        }

                        if timeline.is_empty() {
                            div { class: "rounded-lg border border-[#202D3A] bg-[#0C1118]/60 p-5 text-center text-xs text-[#8D9DAD]",
                                if task.status == "awaiting_approval" {
                                    "Click Approve & Run above to begin."
                                } else if task.status == "approved" {
                                    "Click Run Now above to start planning & execution."
                                } else if wf_live {
                                    "Planner evaluating repository context and picking the first worker…"
                                } else {
                                    "No worker steps were run."
                                }
                            }
                        } else {
                            div { class: "flex flex-col gap-2",
                                for (idx, entry) in timeline.iter().enumerate() {
                                    TimelineNode {
                                        key: "{idx}",
                                        entry: entry.clone(),
                                        workflow_live: wf_live,
                                        can_retry,
                                        on_retry_step,
                                    }
                                }
                            }
                        }
                    }
                }
            } else {
                div { class: "flex items-center justify-between gap-2 text-xs text-[#8D9DAD]",
                    span { "{task.attempts.len()} execution attempt(s)" }
                    if preview_count > 0 {
                        button {
                            r#type: "button",
                            class: "font-medium text-[#66D8CE] hover:underline",
                            onclick: move |_| on_open_diff.call(()),
                            "View diffs & logs ({preview_count}) →"
                        }
                    }
                }
                if task.attempts.is_empty() {
                    div { class: "rounded-lg border border-[#202D3A] bg-[#0C1118]/60 p-5 text-center text-xs text-[#8D9DAD]",
                        "No execution attempts yet."
                    }
                } else {
                    div { class: "flex flex-col gap-2",
                        for (idx, attempt) in task.attempts.iter().enumerate() {
                            AttemptSummaryRow { key: "{attempt.id}-{idx}", index: idx + 1, attempt: attempt.clone() }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn TimelineNode(
    entry: TimelineEntry,
    workflow_live: bool,
    can_retry: bool,
    on_retry_step: EventHandler<usize>,
) -> Element {
    match entry {
        TimelineEntry::Step {
            step_num,
            worker,
            model,
            tier,
            complexity: _,
            rationale,
            dispatched_objective,
            step,
        } => {
            let result = step.as_ref().and_then(|s| s.result.as_ref());
            let is_quota = result
                .and_then(|r| r.get("quota_exhausted"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let exec_outcome = result
                .and_then(|r| r.get("execution_outcome"))
                .and_then(|v| v.as_str());
            let ver_outcome = result
                .and_then(|r| r.get("verification_outcome"))
                .and_then(|v| v.as_str());
            let ver_exit = result
                .and_then(|r| r.get("verification_exit_code"))
                .and_then(|v| v.as_i64());
            let step_err = result
                .and_then(|r| r.get("error"))
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let agent_summary = result
                .and_then(|r| r.get("agent_summary"))
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let ver_stdout = result
                .and_then(|r| r.get("verification_stdout"))
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let ver_stderr = result
                .and_then(|r| r.get("verification_stderr"))
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let agentlet = result
                .and_then(|r| r.get("agentlet_result"))
                .filter(|v| !v.is_null());
            let agentlet_outcome = agentlet
                .and_then(|a| a.get("outcome"))
                .and_then(|v| v.as_str());
            let agentlet_summary = agentlet
                .and_then(|a| a.get("summary"))
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty());

            let step_stopped_early = result.is_none() && !workflow_live;
            let is_running = result.is_none() && workflow_live;
            let failed = !is_quota
                && (step_stopped_early
                    || matches!(ver_outcome, Some("failed" | "timed_out"))
                    || matches!(exec_outcome, Some("failed" | "timed_out")));
            let needs_work = matches!(agentlet_outcome, Some("needs_work" | "blocked"));
            let passed = ver_outcome == Some("succeeded") && !needs_work;

            let (badge_cls, badge_txt) = if is_quota {
                (
                    "bg-amber-950/75 text-amber-300 border-amber-700/70",
                    "Quota fallback".to_string(),
                )
            } else if is_running {
                (
                    "bg-[#FF754F]/15 text-[#FF754F] border-[#FF754F]/70",
                    "Running".to_string(),
                )
            } else if needs_work {
                (
                    "bg-amber-950/75 text-amber-300 border-amber-700/70",
                    "Needs Work".to_string(),
                )
            } else if failed {
                let code_note = ver_exit
                    .filter(|c| *c != 0)
                    .map(|c| format!("Failed (exit {c})"))
                    .unwrap_or_else(|| "Failed".to_string());
                ("bg-red-950/75 text-red-300 border-red-700/70", code_note)
            } else if passed {
                (
                    "bg-emerald-950/75 text-emerald-300 border-emerald-700/70",
                    "Passed".to_string(),
                )
            } else {
                (
                    "bg-neutral-900 text-neutral-300 border-neutral-700",
                    "Completed".to_string(),
                )
            };

            let tree_hash = step
                .as_ref()
                .and_then(|s| s.checkpoint.as_ref())
                .and_then(|c| c.get("tree"))
                .and_then(|v| v.as_str())
                .map(short_id);
            let child_task_id = step.as_ref().map(|s| s.task_id.clone());

            let visible_summary = agentlet_summary.or(agent_summary);
            let has_extra_details = rationale.is_some()
                || dispatched_objective.is_some()
                || ver_stdout.is_some()
                || ver_stderr.is_some()
                || child_task_id.is_some()
                || tree_hash.is_some();

            rsx! {
                div { class: "rounded-lg border p-3 bg-[#0C1118]", style: "border-color: #202D3A;",
                    div { class: "flex flex-wrap items-center justify-between gap-2",
                        div { class: "flex flex-wrap items-center gap-2 min-w-0",
                            span { class: "text-xs font-mono text-[#8D9DAD]", "S{step_num:02}" }
                            span { class: "text-xs sm:text-[13px] font-semibold text-[#F2F5F7]", "{worker}" }
                            if let Some(m) = &model {
                                span { class: "text-xs text-[#8D9DAD]",
                                    if let Some(t) = &tier {
                                        "· {m} ({t})"
                                    } else {
                                        "· {m}"
                                    }
                                }
                            }
                        }
                        div { class: "flex items-center gap-2 shrink-0",
                            span { class: "text-[11px] font-semibold px-2 py-0.5 rounded border {badge_cls}", "{badge_txt}" }
                            if can_retry && step.is_some() {
                                button {
                                    r#type: "button",
                                    class: "text-[11px] font-medium px-2 py-0.5 rounded border border-[#202D3A] bg-[#0E151F] hover:border-[#FF754F]/70 hover:text-[#FF754F] text-[#F2F5F7] transition-colors",
                                    title: "Rewind worktree to before step {step_num} and retry from here",
                                    onclick: move |_| on_retry_step.call(step_num),
                                    "↻ Retry from S{step_num:02}"
                                }
                            }
                        }
                    }

                    if let Some(err) = step_err {
                        p { class: "mt-2 text-xs text-amber-200 break-words", "{err}" }
                    }

                    if let Some(sum) = visible_summary {
                        p { class: "mt-2 text-xs text-neutral-300 leading-relaxed break-words", "{sum}" }
                    }

                    if has_extra_details {
                        details { class: "mt-2 text-xs", open: failed && ver_stderr.is_some(),
                            summary { class: "cursor-pointer text-[11px] text-[#8D9DAD] hover:text-[#F2F5F7] select-none",
                                "Details & verifier logs"
                            }
                            div { class: "mt-2 pt-2 border-t border-[#202D3A] flex flex-col gap-2 text-[11px]",
                                if child_task_id.is_some() || tree_hash.is_some() {
                                    div { class: "flex flex-wrap gap-x-4 gap-y-1 font-mono text-[#8D9DAD]",
                                        if let Some(cid) = &child_task_id {
                                            span { "task: {cid}" }
                                        }
                                        if let Some(tree) = &tree_hash {
                                            span { class: "text-[#66D8CE]", "ckpt: {tree}" }
                                        }
                                    }
                                }
                                if let Some(rat) = &rationale {
                                    div {
                                        span { class: "text-[#66D8CE] font-semibold block mb-0.5", "Planner rationale" }
                                        p { class: "text-neutral-300 leading-relaxed break-words", "{rat}" }
                                    }
                                }
                                if let Some(obj) = &dispatched_objective {
                                    div {
                                        span { class: "text-[#8D9DAD] font-semibold block mb-0.5", "Dispatched prompt" }
                                        p { class: "text-neutral-300 leading-relaxed break-words", "{obj}" }
                                    }
                                }
                                if let Some(err) = ver_stderr {
                                    div {
                                        div { class: "flex items-center justify-between mb-1",
                                            span { class: "text-red-400 font-semibold", "Verifier stderr" }
                                            CopyButton { text: err.to_string(), label: "Copy stderr", subtle: true }
                                        }
                                        pre { class: "cf-pre-scroll cf-scroll bg-[#080C12] border border-red-900/50 rounded p-2 text-[11px] text-red-200 whitespace-pre-wrap break-words max-h-40 overflow-y-auto",
                                            "{err}"
                                        }
                                    }
                                }
                                if let Some(out) = ver_stdout {
                                    div {
                                        span { class: "text-[#8D9DAD] font-semibold block mb-0.5", "Verifier stdout" }
                                        pre { class: "cf-pre-scroll cf-scroll bg-[#080C12] border border-[#202D3A] rounded p-2 text-[11px] text-neutral-300 whitespace-pre-wrap break-words max-h-36 overflow-y-auto",
                                            "{out}"
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        TimelineEntry::Rejected {
            decision_num,
            error,
            raw_action,
        } => rsx! {
            div { class: "rounded-lg border border-amber-700/60 bg-amber-950/20 px-3 py-2 text-xs",
                span { class: "font-semibold text-amber-300", "Decision #{decision_num} ({raw_action}) rejected: " }
                span { class: "text-amber-100 break-words", "{error}" }
            }
        },
        TimelineEntry::Inspect {
            decision_num,
            query,
        } => rsx! {
            div { class: "px-3 py-1.5 text-xs text-[#8D9DAD] flex items-center gap-2",
                span { class: "font-mono text-[11px]", "D{decision_num:02}" }
                span { "Planner inspected git " span { class: "font-mono text-[#F2F5F7]", "{query}" } }
            }
        },
        TimelineEntry::Finish { summary } => rsx! {
            div { class: "rounded-lg border border-[#66D8CE]/40 bg-[#66D8CE]/5 px-3.5 py-2.5 text-xs",
                span { class: "font-semibold text-[#66D8CE] mr-2", "✓ Finished:" }
                span { class: "text-[#F2F5F7] break-words", "{summary}" }
            }
        },
        TimelineEntry::Escalate { reason } => rsx! {
            div { class: "rounded-lg border border-red-700/60 bg-red-950/25 px-3.5 py-2.5 text-xs",
                span { class: "font-semibold text-red-300 mr-2", "✕ Escalated:" }
                span { class: "text-red-100 break-words", "{reason}" }
            }
        },
    }
}

#[component]
fn AttemptSummaryRow(index: usize, attempt: AttemptInfo) -> Element {
    let badge_cls = match attempt.outcome.as_deref() {
        Some("succeeded") => "bg-emerald-950/75 text-emerald-300 border-emerald-700/70",
        Some("failed" | "timed_out") => "bg-red-950/75 text-red-300 border-red-700/70",
        Some("cancelled") => "bg-neutral-900 text-neutral-300 border-neutral-700",
        Some(_) | None => "bg-[#FF754F]/15 text-[#FF754F] border-[#FF754F]/70",
    };
    let badge_txt = attempt.outcome.as_deref().unwrap_or("running");
    let dur = match (attempt.started_at, attempt.ended_at) {
        (Some(s), Some(e)) if e >= s => Some(duration(e - s)),
        _ => None,
    };

    rsx! {
        div { class: "rounded-lg border border-[#202D3A] bg-[#0C1118] px-3.5 py-2.5",
            div { class: "flex flex-wrap items-center justify-between gap-2",
                div { class: "flex items-center gap-2 text-xs",
                    span { class: "font-mono text-[#8D9DAD]", "#{index:02}" }
                    span { class: "font-mono text-[#F2F5F7]", title: "{attempt.id}", "{short_id(&attempt.id)}" }
                    if let Some(d) = dur {
                        span { class: "text-[#8D9DAD]", "· {d}" }
                    }
                }
                span { class: "text-[11px] font-semibold px-2 py-0.5 rounded border {badge_cls}", "{badge_txt}" }
            }
            if let Some(err) = &attempt.error {
                p { class: "mt-1.5 text-xs text-red-300 break-words", "{err}" }
            }
        }
    }
}

#[component]
fn DiffLinesView(text: String, max_h_class: &'static str) -> Element {
    rsx! {
        div { class: "cf-pre-scroll cf-scroll overflow-y-auto {max_h_class} p-2 font-mono text-xs leading-relaxed",
            for (lidx, line) in text.lines().enumerate() {
                {
                    let line_cls = if line.starts_with("+++")
                        || line.starts_with("---")
                        || line.starts_with("diff --git")
                    {
                        "text-[#F2F5F7] font-bold bg-[#202D3A]/40 px-1.5"
                    } else if line.starts_with("@@") {
                        "text-[#66D8CE] bg-[#66D8CE]/10 px-1.5"
                    } else if line.starts_with('+') {
                        "text-emerald-300 bg-emerald-950/35 px-1.5"
                    } else if line.starts_with('-') {
                        "text-red-300 bg-red-950/35 px-1.5"
                    } else {
                        "text-neutral-300 px-1.5"
                    };
                    rsx! {
                        div { key: "{lidx}", class: "whitespace-pre {line_cls}", "{line}" }
                    }
                }
            }
        }
    }
}

#[component]
fn DiffLogsTab(task: TaskDetail, selected_preview: Signal<usize>, wide: bool) -> Element {
    let mut selected_preview = selected_preview;
    let all_previews: Vec<(String, Preview)> = task
        .attempts
        .iter()
        .flat_map(|a| {
            let aid = short_id(&a.id);
            a.previews.iter().cloned().map(move |p| (aid.clone(), p))
        })
        .collect();

    if all_previews.is_empty() && task.attempts.is_empty() {
        return rsx! {
            div { class: "rounded-lg border border-[#202D3A] bg-[#0C1118]/60 p-6 text-center text-xs text-[#8D9DAD]",
                "No diffs or logs captured yet."
            }
        };
    }

    let active_idx = if all_previews.is_empty() {
        0
    } else {
        selected_preview().min(all_previews.len() - 1)
    };
    let max_h = if wide {
        "max-h-[40rem]"
    } else {
        "max-h-[28rem]"
    };

    rsx! {
        div { class: "flex flex-col gap-3",
            if !all_previews.is_empty() {
                div { class: "flex flex-wrap items-center gap-1.5",
                    for (idx, (_, preview)) in all_previews.iter().enumerate() {
                        {
                            let is_live = preview.label.to_ascii_lowercase().starts_with("live ");
                            let is_active = active_idx == idx;
                            rsx! {
                                button {
                                    key: "{idx}-{preview.label}",
                                    r#type: "button",
                                    class: if is_active {
                                        "inline-flex items-center gap-1.5 text-xs font-semibold px-3 py-1.5 rounded border border-[#FF754F] bg-[#FF754F]/15 text-[#F2F5F7]"
                                    } else {
                                        "inline-flex items-center gap-1.5 text-xs px-3 py-1.5 rounded border border-[#202D3A] bg-[#0C1118] text-[#8D9DAD] hover:text-[#F2F5F7]"
                                    },
                                    onclick: move |_| selected_preview.set(idx),
                                    if is_live {
                                        span { class: "cf-pulse-dot", style: "background-color: #FF754F;" }
                                    }
                                    "{preview.label}"
                                }
                            }
                        }
                    }
                }

                if let Some((aid, preview)) = all_previews.get(active_idx) {
                    div { class: "rounded-lg border border-[#202D3A] bg-[#080C12] overflow-hidden",
                        div { class: "px-3 py-2 border-b border-[#202D3A] bg-[#0C1118] flex flex-wrap items-center justify-between gap-2 text-xs",
                            div { class: "flex items-center gap-2",
                                span { class: "font-semibold text-[#F2F5F7]", "{preview.label}" }
                                span { class: "font-mono text-[11px] text-[#8D9DAD]", "attempt:{aid}" }
                            }
                            div { class: "flex items-center gap-2.5 text-[11px] font-mono text-[#8D9DAD]",
                                span { "{format_bytes(preview.shown_bytes as u64)} / {format_bytes(preview.total_bytes)}" }
                                if preview.truncated {
                                    span { class: "px-1.5 py-0.5 rounded bg-amber-950/80 border border-amber-700/80 text-amber-300 font-semibold", "Truncated" }
                                }
                                if !preview.text.trim().is_empty() {
                                    CopyButton {
                                        text: preview.text.clone(),
                                        label: "Copy",
                                        subtle: true,
                                    }
                                }
                            }
                        }
                        if preview.text.trim().is_empty() {
                            div { class: "p-4 text-xs text-[#8D9DAD] italic",
                                "(empty output)"
                            }
                        } else if is_diff_preview(preview) {
                            DiffLinesView { text: preview.text.clone(), max_h_class: max_h }
                        } else {
                            pre { class: "cf-pre-scroll cf-scroll p-3 text-xs text-neutral-200 whitespace-pre-wrap break-words {max_h} overflow-y-auto",
                                "{preview.text}"
                            }
                        }
                    }
                }
            } else {
                div { class: "rounded-lg border border-[#202D3A] bg-[#0C1118]/60 p-4 text-center text-xs text-[#8D9DAD]",
                    "No artifact previews captured on the latest attempt."
                }
            }

            if !task.attempts.is_empty() {
                div {
                    span { class: "block text-xs font-medium mb-2 text-[#8D9DAD]", "Attempts ({task.attempts.len()})" }
                    div { class: "flex flex-col gap-1.5",
                        for (idx, attempt) in task.attempts.iter().enumerate() {
                            AttemptSummaryRow { key: "{attempt.id}-{idx}", index: idx + 1, attempt: attempt.clone() }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn SpecsAndAuditTab(task: TaskDetail, on_depend: EventHandler<String>) -> Element {
    let mut depend_target = use_signal(String::new);
    let wf_events = task
        .workflow
        .as_ref()
        .map(|w| w.events.clone())
        .unwrap_or_default();
    let git_reads = task
        .workflow
        .as_ref()
        .map(|w| w.git_reads.clone())
        .unwrap_or_default();
    let int_events = task
        .integration
        .as_ref()
        .map(|i| i.events.clone())
        .unwrap_or_default();

    rsx! {
        div { class: "flex flex-col gap-3",
            // Target & Provenance
            div { class: "rounded-lg border border-[#202D3A] bg-[#0C1118] p-3.5",
                span { class: "block text-xs font-semibold mb-2 text-[#66D8CE]", "Provenance & Scope" }
                div { class: "grid grid-cols-1 gap-1.5 text-xs",
                    SpecRow { label: "Task ID", value: task.id.clone(), mono: true }
                    SpecRow { label: "Repository", value: task.repository.clone(), mono: true }
                    if let Some(branch) = &task.branch {
                        SpecRow { label: "Branch", value: branch.clone(), mono: true }
                    }
                    if !task.workspace.is_empty() {
                        SpecRow { label: "Worktree", value: task.workspace.clone(), mono: true }
                    }
                    if !task.agent.is_empty() {
                        SpecRow { label: "Agent", value: task.agent.clone(), mono: false }
                    }
                    if let Some(by) = &task.approved_by {
                        SpecRow { label: "Approved By", value: by.clone(), mono: false }
                    }
                    if let Some(rev) = &task.reviewer {
                        SpecRow {
                            label: "Review",
                            value: format!(
                                "{} by {rev}",
                                if task.accepted == Some(true) { "accepted" } else { "rejected" }
                            ),
                            mono: false,
                        }
                    }
                    if !task.assignment_digest.is_empty() {
                        SpecRow { label: "Assignment", value: short_id(&task.assignment_digest), mono: true }
                    }
                    if let Some(ev) = &task.evidence_digest {
                        SpecRow { label: "Evidence", value: short_id(ev), mono: true }
                    }
                }

                if !task.allowed_scope.is_empty() {
                    div { class: "mt-3 pt-3 border-t border-[#202D3A]",
                        span { class: "block text-[11px] text-[#8D9DAD] mb-1.5", "Editable file scope ({task.allowed_scope.len()})" }
                        div { class: "flex flex-wrap gap-1.5",
                            for path in &task.allowed_scope {
                                span { class: "text-[11px] font-mono px-2 py-0.5 rounded bg-[#0E151F] border border-[#202D3A] text-[#F2F5F7] break-all",
                                    "{path}"
                                }
                            }
                        }
                    }
                }
            }

            if !task.budget.is_empty() {
                div { class: "rounded-lg border border-[#202D3A] bg-[#0C1118] p-3.5",
                    span { class: "block text-xs font-semibold mb-2 text-[#8D9DAD]", "Budgets & Limits" }
                    div { class: "flex flex-col gap-1",
                        for limit in &task.budget {
                            p { class: "text-xs text-neutral-300 font-mono break-words",
                                "{budget_label(limit)}"
                            }
                        }
                    }
                }
            }

            if let Some(wf) = &task.workflow {
                if !wf.agentlets.is_empty() {
                    details { class: "rounded-lg border border-[#202D3A] bg-[#0C1118] p-3.5",
                        summary { class: "cursor-pointer text-xs font-semibold text-[#8D9DAD] hover:text-[#F2F5F7]",
                            "Configured specialists ({wf.agentlets.len()})"
                        }
                        div { class: "mt-2.5 flex flex-col gap-2",
                            for agentlet in &wf.agentlets {
                                div { class: "rounded border border-[#202D3A] bg-[#0E151F] p-2.5 text-xs",
                                    div { class: "flex flex-wrap items-center justify-between gap-1.5 mb-1",
                                        span { class: "font-semibold text-[#F2F5F7]", "{agentlet.worker} ({agentlet.definition.id} v{agentlet.definition.version})" }
                                        span { class: "text-[11px] text-[#8D9DAD]",
                                            if agentlet.definition.completion_gate {
                                                "{agentlet.definition.capability} · gate"
                                            } else {
                                                "{agentlet.definition.capability}"
                                            }
                                        }
                                    }
                                    p { class: "text-neutral-300", "{agentlet.definition.purpose}" }
                                }
                            }
                        }
                    }
                }
            }

            // Dependencies
            div { class: "rounded-lg border border-[#202D3A] bg-[#0C1118] p-3.5",
                span { class: "block text-xs font-semibold mb-2 text-[#8D9DAD]", "Dependencies ({task.depends_on.len()})" }
                if !task.depends_on.is_empty() {
                    div { class: "flex flex-col gap-1.5 mb-2.5",
                        for dep in &task.depends_on {
                            div { class: "flex items-center justify-between gap-2 rounded bg-[#0E151F] px-2.5 py-1.5 text-xs",
                                span { class: "font-mono text-[#F2F5F7] break-all", "{dep.id}" }
                                span { class: "text-[11px] px-2 py-0.5 rounded border {status_badge_class(&dep.status)}", "{dep.status}" }
                            }
                        }
                    }
                }
                div { class: "flex flex-col sm:flex-row gap-2",
                    input {
                        class: "bg-[#0E151F] border border-[#202D3A] focus:border-[#FF754F] outline-none rounded px-2.5 py-1.5 flex-1 text-xs text-[#F2F5F7]",
                        value: "{depend_target}",
                        oninput: move |e| depend_target.set(e.value()),
                        placeholder: "Add dependency by task ID or prefix…",
                    }
                    button {
                        class: "bg-[#202D3A] hover:bg-neutral-700 border border-neutral-600 rounded px-3.5 py-1.5 text-xs font-medium disabled:opacity-45",
                        disabled: depend_target().trim().is_empty(),
                        onclick: move |_| {
                            let target = depend_target().trim().to_string();
                            depend_target.set(String::new());
                            on_depend.call(target);
                        },
                        "Add"
                    }
                }
            }

            // Routing & Audit Trail (tucked calmly inside details unless user opens it)
            if !wf_events.is_empty() || !task.routing.is_empty() || !git_reads.is_empty() || !int_events.is_empty() {
                details { class: "rounded-lg border border-[#202D3A] bg-[#0C1118] p-3.5",
                    summary { class: "cursor-pointer text-xs font-semibold text-[#8D9DAD] hover:text-[#F2F5F7]",
                        {
                            let count = wf_events.len() + task.routing.len() + git_reads.len() + int_events.len();
                            let noun = if count == 1 { "event" } else { "events" };
                            format!("Routing & audit trail ({count} {noun})")
                        }
                    }
                    div { class: "mt-3 flex flex-col gap-3 text-xs",
                        if !task.routing.is_empty() {
                            div {
                                span { class: "block text-[11px] text-[#8D9DAD] mb-1", "Worker routing ({task.routing.len()})" }
                                div { class: "flex flex-col gap-1.5",
                                    for event in &task.routing {
                                        div { class: "rounded bg-[#0E151F] px-2.5 py-1.5",
                                            div { class: "flex items-center justify-between gap-2",
                                                span { class: "font-medium text-[#F2F5F7]", "{event.stage}" }
                                                span { class: "font-mono text-[11px] text-[#66D8CE]", "{event.status}" }
                                            }
                                            p { class: "font-mono text-[11px] text-[#8D9DAD] break-all", "{event.handler}" }
                                            if let Some(note) = &event.note {
                                                p { class: "text-[11px] text-amber-300 mt-0.5 break-words", "{note}" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        if !wf_events.is_empty() {
                            div {
                                span { class: "block text-[11px] text-[#8D9DAD] mb-1", "Lifecycle events ({wf_events.len()})" }
                                div { class: "flex flex-col gap-1",
                                    for event in &wf_events {
                                        div { class: "flex items-center gap-2 font-mono text-[11px] text-neutral-300",
                                            span { class: "text-[#8D9DAD]", "r{event.revision}" }
                                            span { "{event.stage}" }
                                        }
                                    }
                                }
                            }
                        }
                        if !git_reads.is_empty() {
                            div {
                                span { class: "block text-[11px] text-[#8D9DAD] mb-1", "Git reads ({git_reads.len()})" }
                                div { class: "flex flex-col gap-1",
                                    for (idx, read) in git_reads.iter().enumerate() {
                                        {
                                            let query = read.get("query").and_then(|v| v.as_str()).unwrap_or("read");
                                            let bytes = read
                                                .get("shown_bytes")
                                                .or_else(|| read.get("total_bytes"))
                                                .and_then(|v| v.as_u64());
                                            rsx! {
                                                div { key: "{idx}", class: "flex items-center justify-between font-mono text-[11px] text-neutral-300",
                                                    span { "#{idx + 1} · git {query}" }
                                                    if let Some(b) = bytes {
                                                        span { class: "text-[#8D9DAD]", "{format_bytes(b)}" }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        if !int_events.is_empty() {
                            div {
                                span { class: "block text-[11px] text-[#8D9DAD] mb-1", "Integration events ({int_events.len()})" }
                                div { class: "flex flex-col gap-1",
                                    for event in &int_events {
                                        div { class: "flex items-center justify-between font-mono text-[11px] text-neutral-300",
                                            span { "{event.stage}" }
                                            span { class: "text-[#8D9DAD]", "{event.handler} · {event.by}" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn SpecRow(label: &'static str, value: String, mono: bool) -> Element {
    rsx! {
        div { class: "flex flex-col sm:flex-row sm:items-baseline gap-0.5 sm:gap-3 py-0.5",
            span { class: "text-[11px] sm:w-28 shrink-0 text-[#8D9DAD]", "{label}" }
            span { class: if mono { "font-mono text-xs text-[#F2F5F7] break-all" } else { "text-xs text-[#F2F5F7] break-words" },
                "{value}"
            }
        }
    }
}
