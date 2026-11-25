//! Durable scheduling (`conductor_schedule_task`): due submissions go through the ordinary
//! submit path, so grants are judged at fire time, a global stop holds everything, missed
//! occurrences fire once, and a retried firing never creates a second task.

use clatterforge::{
    authorization::{ExecutionMode, Grants, Operation},
    enrollment::{EnrollmentOperation, Enrollments, PRINCIPAL_NAMESPACE_PREFIX},
    mcp::{
        CancelScheduleRequest, McpServer, RequestIdOnly, ScheduleTaskRequest, SubmitTaskRequest,
    },
    now,
    repository::git,
    schedule::{Schedules, fire_due, occurrence_key},
    stop::GlobalStop,
    store::Store,
};
use rmcp::handler::server::wrapper::Parameters;
use std::{
    fs,
    path::{Path, PathBuf},
};
use tempfile::TempDir;

const MINUTE: u64 = 60_000;

fn setup() -> (TempDir, PathBuf, PathBuf, String) {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("checkout");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]).unwrap();
    fs::write(repo.join("README.md"), "# Fixture\n").unwrap();
    git(&repo, &["add", "."]).unwrap();
    git(
        &repo,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "core.hooksPath=",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "fixture base",
        ],
    )
    .unwrap();
    let state = dir.path().join("state");
    let (id, _) = Store::open(&state).unwrap().register(&repo).unwrap();
    (dir, repo, state, id)
}

fn submission(repository_id: &str) -> SubmitTaskRequest {
    SubmitTaskRequest {
        backend: "fake-process-v1".into(),
        principal: "test-orchestrator".into(),
        workflow_id: Some("caller-workflow".into()),
        idempotency_key: Some("caller-key".into()),
        spec: None,
        config: None,
        repository_id: Some(repository_id.into()),
        objective: Some("Create a fixture result".into()),
        acceptance: Some("Review a new file and its host receipt".into()),
        contents: Some("hello from a schedule\n".into()),
        budget: Some(16384),
        agent_preset: None,
        workflow_profile: None,
        source_system: None,
        source_record_reference: None,
        correlation_id: None,
        requested_execution_mode: None,
        follow_up_of: None,
        request_id: None,
    }
}

fn schedule_request(
    submission: SubmitTaskRequest,
    not_before_ms: u64,
    repeat_every_ms: Option<u64>,
    max_occurrences: Option<u32>,
) -> ScheduleTaskRequest {
    ScheduleTaskRequest {
        submission,
        not_before_ms,
        repeat_every_ms,
        max_occurrences,
        request_id: None,
    }
}

async fn schedule(server: &McpServer, request: ScheduleTaskRequest) -> String {
    let response = server
        .conductor_schedule_task(Parameters(request))
        .await
        .unwrap()
        .0;
    assert!(response.supported);
    response.schedule.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string()
}

fn task_status(state: &Path, task_id: &str) -> String {
    clatterforge::service::TaskService::open(state)
        .unwrap()
        .detail(task_id, Some(0))
        .unwrap()
        .status
}

fn identity(repo: &Path) -> String {
    clatterforge::repository::Repository::inspect(repo)
        .unwrap()
        .identity
}

#[tokio::test]
async fn capabilities_report_scheduling_and_its_tools() {
    let (_dir, _repo, state, _id) = setup();
    let caps = McpServer::open(state)
        .unwrap()
        .conductor_capabilities()
        .await
        .unwrap()
        .0;
    assert!(caps.scheduling_supported);
    for tool in [
        "conductor_schedule_task",
        "conductor_list_schedules",
        "conductor_cancel_schedule",
    ] {
        assert!(caps.tools.iter().any(|t| t == tool), "missing {tool}");
    }
}

#[tokio::test]
async fn a_schedule_fires_once_when_due_and_lands_awaiting_approval_without_a_grant() {
    let (_dir, _repo, state, repo_id) = setup();
    let server = McpServer::open(state.clone()).unwrap();
    let t0 = now();
    let id = schedule(
        &server,
        schedule_request(submission(&repo_id), t0 + MINUTE, None, None),
    )
    .await;

    assert!(
        fire_due(&state, t0).await.unwrap().is_empty(),
        "not due yet"
    );

    let fired = fire_due(&state, t0 + MINUTE).await.unwrap();
    assert_eq!(fired.len(), 1, "{fired:?}");
    let task_id = fired[0].task_id.clone().expect("a task was created");
    assert_eq!(task_status(&state, &task_id), "awaiting_approval");

    let stored = Schedules::open(&state).unwrap().get(&id).unwrap();
    assert_eq!(stored.state, "completed");
    assert_eq!(stored.fires.len(), 1);
    assert!(stored.fires[0].authorization.is_empty());
    // The caller's own idempotency pair never leaks into the per-occurrence key.
    assert!(stored.submission["workflow_id"].is_null());

    assert!(fire_due(&state, t0 + 10 * MINUTE).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_grant_present_at_fire_time_auto_authorizes_and_interactive_still_waits() {
    let (_dir, repo, state, repo_id) = setup();
    let server = McpServer::open(state.clone()).unwrap();
    let t0 = now();
    let delegated = schedule(
        &server,
        schedule_request(submission(&repo_id), t0, None, None),
    )
    .await;
    let mut interactive_submission = submission(&repo_id);
    interactive_submission.requested_execution_mode = Some("interactive".into());
    let interactive = schedule(
        &server,
        schedule_request(interactive_submission, t0, None, None),
    )
    .await;

    // Created after scheduling: coverage is judged when the occurrence fires.
    Grants::open(&state)
        .unwrap()
        .create(
            "test-orchestrator",
            &identity(&repo),
            vec!["fake-process-v1".into()],
            vec![Operation::Submit],
            ExecutionMode::Delegated,
            "mike",
            5,
            now() + 30 * MINUTE,
        )
        .unwrap();

    let fired = fire_due(&state, t0).await.unwrap();
    assert_eq!(fired.len(), 2, "{fired:?}");
    let store = Schedules::open(&state).unwrap();

    let auto = store.get(&delegated).unwrap();
    assert_eq!(auto.fires[0].authorization.len(), 1);
    assert_eq!(auto.fires[0].authorization[0]["operation"], "approve");
    assert_eq!(
        task_status(&state, auto.fires[0].task_id.as_deref().unwrap()),
        "approved"
    );

    let held = store.get(&interactive).unwrap();
    assert!(held.fires[0].authorization.is_empty());
    assert_eq!(
        task_status(&state, held.fires[0].task_id.as_deref().unwrap()),
        "awaiting_approval"
    );
}

#[tokio::test]
async fn a_global_stop_holds_due_schedules_and_missed_occurrences_fire_once() {
    let (_dir, _repo, state, repo_id) = setup();
    let server = McpServer::open(state.clone()).unwrap();
    let t0 = now();
    let id = schedule(
        &server,
        schedule_request(submission(&repo_id), t0, Some(MINUTE), Some(5)),
    )
    .await;

    GlobalStop::open(&state)
        .unwrap()
        .engage("ops", "maintenance")
        .unwrap();
    assert!(fire_due(&state, t0 + 10 * MINUTE).await.unwrap().is_empty());

    GlobalStop::open(&state)
        .unwrap()
        .disengage("ops", "done")
        .unwrap();
    let fired = fire_due(&state, t0 + 10 * MINUTE).await.unwrap();
    assert_eq!(fired.len(), 1, "ten missed slots fire once: {fired:?}");

    let stored = Schedules::open(&state).unwrap().get(&id).unwrap();
    assert_eq!(stored.state, "active");
    assert_eq!(stored.next_occurrence, 2);
    assert_eq!(stored.next_due_ms, t0 + 11 * MINUTE);
    assert!(fire_due(&state, t0 + 10 * MINUTE).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_retried_firing_replays_the_task_instead_of_duplicating_it() {
    let (_dir, _repo, state, repo_id) = setup();
    let server = McpServer::open(state.clone()).unwrap();
    let t0 = now();
    let id = schedule(
        &server,
        schedule_request(submission(&repo_id), t0, None, None),
    )
    .await;

    // A firing that submitted occurrence 1 and crashed before recording it.
    let (workflow_id, key) = occurrence_key(&id, 1);
    let mut earlier = submission(&repo_id);
    earlier.workflow_id = Some(workflow_id);
    earlier.idempotency_key = Some(key);
    let first = server
        .conductor_submit_task(Parameters(earlier))
        .await
        .unwrap()
        .0;
    let first_id = first.task["id"].as_str().unwrap().to_string();

    let fired = fire_due(&state, t0).await.unwrap();
    assert_eq!(fired[0].task_id.as_deref(), Some(first_id.as_str()));
    let stored = Schedules::open(&state).unwrap().get(&id).unwrap();
    assert!(stored.fires[0].replayed);
}

#[tokio::test]
async fn a_cancelled_schedule_never_fires_and_bad_schedules_are_refused() {
    let (_dir, _repo, state, repo_id) = setup();
    let server = McpServer::open(state.clone()).unwrap();
    let t0 = now();
    let id = schedule(
        &server,
        schedule_request(submission(&repo_id), t0, Some(MINUTE), Some(3)),
    )
    .await;
    let cancelled = server
        .conductor_cancel_schedule(Parameters(CancelScheduleRequest {
            schedule_id: id.clone(),
            principal: Some("ops".into()),
            request_id: None,
        }))
        .await
        .unwrap()
        .0;
    assert_eq!(cancelled.schedules[0]["state"], "cancelled");
    assert!(fire_due(&state, t0 + MINUTE).await.unwrap().is_empty());

    let listed = server
        .conductor_list_schedules(Parameters(RequestIdOnly { request_id: None }))
        .await
        .unwrap()
        .0;
    assert_eq!(listed.schedules.len(), 1);

    for (request, why) in [
        (
            schedule_request(submission(&repo_id), t0, Some(MINUTE), None),
            "repeat without max_occurrences",
        ),
        (
            schedule_request(submission(&repo_id), t0, Some(1_000), Some(2)),
            "repeat interval under a minute",
        ),
        (
            schedule_request(submission(&repo_id), t0, None, Some(2)),
            "several occurrences without a repeat",
        ),
        (
            schedule_request(
                submission(&repo_id),
                t0 + 400 * 24 * 60 * MINUTE,
                None,
                None,
            ),
            "more than a year away",
        ),
        (
            {
                let mut bad = submission(&repo_id);
                bad.backend = "shell".into();
                schedule_request(bad, t0, None, None)
            },
            "unknown backend",
        ),
    ] {
        assert!(
            server
                .conductor_schedule_task(Parameters(request))
                .await
                .is_err(),
            "accepted a schedule with {why}"
        );
    }
}

#[tokio::test]
async fn edge_callers_only_see_and_cancel_their_own_schedules() {
    use clatterforge::mcp::LocalBackend;
    use clatterforge_protocol::ConductorBackend;

    let (_dir, _repo, state, repo_id) = setup();
    let edge = |client: &str| {
        let principal = format!("{PRINCIPAL_NAMESPACE_PREFIX}{client}");
        (
            LocalBackend::open_trusted(state.clone(), Vec::new())
                .unwrap()
                .with_verified_principal(principal.clone()),
            principal,
        )
    };
    let (alice, alice_principal) = edge("alice");
    let (bob, bob_principal) = edge("bob");
    let mut request = schedule_request(submission(&repo_id), now() + MINUTE, None, None);
    request.submission.principal = alice_principal.clone();
    let created = alice.conductor_schedule_task(request).await.unwrap();
    let schedule = created.schedule.unwrap();
    assert_eq!(schedule["client_id"], "alice", "the enrollment is recorded");
    let id = schedule["id"].as_str().unwrap().to_string();

    let listing = |backend: &LocalBackend| {
        let backend = backend.clone();
        async move {
            backend
                .conductor_list_schedules(RequestIdOnly { request_id: None })
                .await
                .unwrap()
                .schedules
                .len()
        }
    };
    assert_eq!(listing(&alice).await, 1);
    assert_eq!(listing(&bob).await, 0);
    let unscoped = LocalBackend::open_trusted(state.clone(), Vec::new()).unwrap();
    assert_eq!(
        listing(&unscoped).await,
        0,
        "no verified principal sees nothing"
    );

    let refused = bob
        .conductor_cancel_schedule(CancelScheduleRequest {
            schedule_id: id.clone(),
            principal: Some(bob_principal),
            request_id: None,
        })
        .await
        .unwrap_err();
    assert!(refused.message.contains("unknown schedule"), "{refused:?}");
    alice
        .conductor_cancel_schedule(CancelScheduleRequest {
            schedule_id: id,
            principal: Some(alice_principal),
            request_id: None,
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn an_edge_schedule_stops_once_its_enrollment_is_revoked() {
    let (_dir, _repo, state, repo_id) = setup();
    let mut enrollments = Enrollments::open(&state).unwrap();
    let (enrollment, _token) = enrollments
        .create(
            "bot",
            "generic-mcp-client",
            vec![EnrollmentOperation::Submit],
            Vec::new(),
            "mike",
        )
        .unwrap();
    let principal = format!("{PRINCIPAL_NAMESPACE_PREFIX}{}", enrollment.client_id);
    let mut edge_submission = submission(&repo_id);
    edge_submission.principal = principal.clone();
    let t0 = now();
    let id = Schedules::open(&state)
        .unwrap()
        .create(
            &schedule_request(edge_submission, t0, Some(MINUTE), Some(3)),
            Some(&enrollment.client_id),
            t0,
        )
        .unwrap()
        .id;

    let first = fire_due(&state, t0).await.unwrap();
    assert!(first[0].error.is_none(), "{first:?}");

    enrollments
        .revoke(&enrollment.client_id, "mike", "lost laptop")
        .unwrap();
    let after = fire_due(&state, t0 + MINUTE).await.unwrap();
    assert!(after[0].error.as_deref().unwrap().contains("revoked"));
    let stored = Schedules::open(&state).unwrap().get(&id).unwrap();
    assert_eq!(stored.state, "failed");
    assert_eq!(stored.fires.len(), 1, "no task for the revoked occurrence");
}
