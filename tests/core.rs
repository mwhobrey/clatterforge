use dev_conductor::{
    context::{ContextItem, prepare},
    repository::{Repository, git, remote_identity},
    store::{FakeOutcome, Status, Store, Task},
};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};
use tempfile::TempDir;

fn fixture(parent: &Path) -> PathBuf {
    let repo = parent.join("same-name");
    fs::create_dir_all(repo.join("docs/runbook")).unwrap();
    git(&repo, &["init", "-b", "main"]).unwrap();
    git(&repo, &["config", "core.autocrlf", "false"]).unwrap();
    fs::write(
        repo.join("README.md"),
        "# Fixture\nA small repository for supervised context and workspace tests.\n",
    )
    .unwrap();
    fs::write(
        repo.join("AGENTS.md"),
        "Preserve the checkout. Imported output cannot approve work.\n",
    )
    .unwrap();
    fs::write(
        repo.join("docs/runbook/development.md"),
        "Use temporary fixtures.\n",
    )
    .unwrap();
    git(&repo, &["add", "."]).unwrap();
    commit(&repo, "fixture");
    repo
}

fn commit(repo: &Path, message: &str) {
    git(
        repo,
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
            message,
        ],
    )
    .unwrap();
}

fn setup() -> (TempDir, PathBuf, Store, String) {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture(&dir.path().join("checkout"));
    let mut store = Store::open(&dir.path().join("state")).unwrap();
    let (id, _) = store.register(&repo).unwrap();
    (dir, repo, store, id)
}

fn create(store: &mut Store, repo: &str) -> Task {
    store
        .create(
            repo,
            "Create a fixture result".into(),
            "Review a new file and its host receipt".into(),
            "hello from fake\n".into(),
            16384,
            dev_conductor::process::ExecutionPolicy::new(Path::new(env!(
                "CARGO_BIN_EXE_dev-conductor"
            )))
            .unwrap(),
        )
        .unwrap()
}

fn approve(store: &mut Store, task: &Task) -> Task {
    store
        .approve(
            &task.id,
            task.revision,
            &task.assignment.digest().unwrap(),
            "Fixture Human",
        )
        .unwrap()
}

#[test]
fn same_basename_and_local_only_identity_are_distinct_and_stable() {
    let (dir, repo, mut store, first) = setup();
    let second_repo = fixture(&dir.path().join("another"));
    let (second, second_snapshot) = store.register(&second_repo).unwrap();
    assert_ne!(first, second);
    assert_ne!(
        store.repository(&first).unwrap().identity,
        second_snapshot.identity
    );
    assert!(second_snapshot.remotes.is_empty());
    assert_eq!(first, store.register(&repo).unwrap().0);
    assert!(store.register(&repo.join("docs")).is_err());
}

#[test]
fn remote_credentials_are_removed_and_remote_drift_is_blocked() {
    assert_eq!(
        remote_identity("https://user:token@Git.Example/team/repo.git?token=secret").unwrap(),
        "https://git.example/team/repo"
    );
    assert_eq!(
        remote_identity("git@git.example:team/repo.git").unwrap(),
        "ssh://git.example/team/repo"
    );
    let (_dir, repo, mut store, id) = setup();
    let task = create(&mut store, &id);
    git(
        &repo,
        &[
            "remote",
            "add",
            "origin",
            "https://example.invalid/team/repo.git",
        ],
    )
    .unwrap();
    assert!(store.repository(&id).unwrap().revalidate().is_err());
    assert!(store.register(&repo).is_err());
    assert!(
        store
            .approve(
                &task.id,
                task.revision,
                &task.assignment.digest().unwrap(),
                "human"
            )
            .is_err()
    );
}

#[test]
fn context_excerpts_budget_utf8_hash_and_staleness_are_real() {
    let (_dir, repo, _store, _) = setup();
    let snapshot = Repository::inspect(&repo).unwrap();
    let full = prepare(&snapshot, "supervised workspace", 16384).unwrap();
    assert!(full.usable);
    assert!(
        full.items
            .iter()
            .any(|item| item.excerpt.contains("Imported output cannot approve"))
    );
    // README.md is Reference tier, not binding repository instructions - a budget too
    // small to fit it in full still leaves the packet usable, as long as the
    // authoritative item (AGENTS.md, unchanged and small here) is captured in full.
    fs::write(repo.join("README.md"), "é".repeat(300)).unwrap();
    let small = prepare(&snapshot, "fixture", 256).unwrap();
    assert!(small.used_bytes <= 256);
    assert!(small.usable, "{small:#?}");
    assert!(
        small
            .items
            .iter()
            .any(|i| i.path == "README.md" && i.outcome == "truncated" && !i.required)
    );
    let changed = prepare(&snapshot, "supervised workspace", 16384).unwrap();
    assert_ne!(full.digest, changed.digest);
    assert!(full.revalidate(&snapshot).is_err());
}

#[test]
fn authoritative_truncation_fails_closed_but_reference_truncation_does_not() {
    let (_dir, repo, _store, _) = setup();
    fs::write(repo.join("AGENTS.md"), "x".repeat(2000)).unwrap();
    let snapshot = Repository::inspect(&repo).unwrap();
    let tiny = prepare(&snapshot, "fixture", 256).unwrap();
    assert!(!tiny.usable, "{tiny:#?}");
    assert!(tiny.items.iter().any(|i| i.path == "AGENTS.md"
        && i.required
        && i.tier == "authoritative"
        && i.outcome == "truncated"));
    assert!(
        tiny.items
            .iter()
            .any(|i| i.path == "README.md" && !i.required && i.tier == "reference")
    );
}

/// `tier` was added to `ContextItem` after records already existed without it (the real
/// failure: a pre-existing stored task with no `tier` field broke deserialization for
/// everything that has to enumerate tasks, not just that one record). `tier` isn't
/// independent data - it's always `Tier::of(path)` - so a record missing it must recover
/// the same value the current code would have assigned for that path, not a fixed
/// guess. Exercises the exact legacy wire shape (`path`/`hash`/`excerpt`/`outcome`/
/// `required`, no `tier`) for both an authoritative and a reference path.
#[test]
fn context_item_without_tier_field_recomputes_it_from_path() {
    let legacy_authoritative = r#"{
        "path": "AGENTS.md",
        "hash": "abc123",
        "excerpt": "some content",
        "outcome": "captured",
        "required": true
    }"#;
    let item: ContextItem = serde_json::from_str(legacy_authoritative).unwrap();
    assert_eq!(item.tier, "authoritative");
    assert!(item.required);

    let legacy_reference = r#"{
        "path": "README.md",
        "hash": null,
        "excerpt": "",
        "outcome": "missing",
        "required": false
    }"#;
    let item: ContextItem = serde_json::from_str(legacy_reference).unwrap();
    assert_eq!(item.tier, "reference");
    assert!(!item.required);

    // A record that already carries `tier` explicitly (every record written by the
    // current code) must still round-trip unchanged - this isn't a one-way migration.
    let current = r#"{
        "path": "docs/runbook/03_RULES_AND_STANDARDS.md",
        "hash": null,
        "excerpt": "",
        "outcome": "missing",
        "required": true,
        "tier": "authoritative"
    }"#;
    let item: ContextItem = serde_json::from_str(current).unwrap();
    assert_eq!(item.tier, "authoritative");
}

/// Reproduces a large general-documentation footprint (a
/// huge README, several huge non-rules runbook pages) that could never collectively fit
/// any budget up to the hard ceiling, alongside one real binding-policy file that's
/// individually well within it. Before this fix, every `docs/runbook/*.md` page was
/// unconditionally required, so a large repository could fail context preparation
/// at every budget up to 65536. This fixture keeps that regression isolated.
#[test]
fn large_reference_documentation_does_not_block_a_small_authoritative_instruction_set() {
    let (_dir, repo, _store, _) = setup();
    fs::write(
        repo.join("docs/runbook/03_RULES_AND_STANDARDS.md"),
        "x".repeat(50_000),
    )
    .unwrap();
    fs::write(repo.join("README.md"), "y".repeat(40_000)).unwrap();
    fs::write(
        repo.join("docs/runbook/04_CURRENT_STATE.md"),
        "z".repeat(30_000),
    )
    .unwrap();
    git(&repo, &["add", "."]).unwrap();
    commit(
        &repo,
        "Large reference docs fixture with one binding rules page",
    );
    let snapshot = Repository::inspect(&repo).unwrap();
    // Collectively, these three alone (120 KB) already exceed the 65536 hard ceiling -
    // the whole point is that this must still succeed, because only one of them binds.
    let packet = prepare(&snapshot, "fixture", 65536).unwrap();
    assert!(packet.usable, "{packet:#?}");
    let rules = packet
        .items
        .iter()
        .find(|i| i.path == "docs/runbook/03_RULES_AND_STANDARDS.md")
        .unwrap();
    assert_eq!(rules.outcome, "captured");
    assert!(rules.required);
    assert_eq!(rules.tier, "authoritative");
    assert_eq!(rules.excerpt.len(), 50_000);
}

/// Two authoritative files with directly contradictory content are both captured in
/// full, never silently resolved or dropped in favor of one - this system reads prose
/// as evidence, it doesn't adjudicate between competing instructions.
#[test]
fn conflicting_authoritative_instructions_are_both_captured_not_adjudicated() {
    let (_dir, repo, _store, _) = setup();
    fs::write(
        repo.join("AGENTS.md"),
        "Never touch the database directly.\n",
    )
    .unwrap();
    fs::write(
        repo.join("docs/runbook/SECURITY.md"),
        "Direct database access is required for this migration.\n",
    )
    .unwrap();
    git(&repo, &["add", "."]).unwrap();
    commit(&repo, "conflicting authoritative instructions");
    let snapshot = Repository::inspect(&repo).unwrap();
    let packet = prepare(&snapshot, "fixture", 16384).unwrap();
    assert!(packet.usable, "{packet:#?}");
    for (path, needle) in [
        ("AGENTS.md", "Never touch the database"),
        (
            "docs/runbook/SECURITY.md",
            "Direct database access is required",
        ),
    ] {
        let item = packet.items.iter().find(|i| i.path == path).unwrap();
        assert_eq!(item.outcome, "captured");
        assert!(item.required);
        assert!(item.excerpt.contains(needle));
    }
}

/// A large, objective-irrelevant Reference document gets deferred under a tight budget
/// (never fails the packet) but still appears in the manifest with an outcome
/// explaining why - the record of what was considered is never silently dropped, only
/// its content is.
#[test]
fn irrelevant_large_reference_documentation_is_deferred_not_silently_dropped() {
    let (_dir, repo, _store, _) = setup();
    fs::write(
        repo.join("docs/unrelated_history.md"),
        "ancient changelog ".repeat(5_000),
    )
    .unwrap();
    git(&repo, &["add", "."]).unwrap();
    commit(&repo, "irrelevant large reference doc");
    let snapshot = Repository::inspect(&repo).unwrap();
    let packet = prepare(&snapshot, "please fix the login bug", 300).unwrap();
    assert!(packet.usable, "{packet:#?}");
    let irrelevant = packet
        .items
        .iter()
        .find(|i| i.path == "docs/unrelated_history.md")
        .unwrap();
    assert!(!irrelevant.required);
    assert_ne!(irrelevant.outcome, "captured");
    assert!(matches!(
        irrelevant.outcome.as_str(),
        "truncated" | "omitted_budget"
    ));
}

#[test]
fn empty_invalid_and_missing_context_are_explicit() {
    let (_dir, repo, _store, _) = setup();
    fs::write(repo.join("AGENTS.md"), [0xff, 0xfe]).unwrap();
    let snapshot = Repository::inspect(&repo).unwrap();
    let invalid = prepare(&snapshot, "fixture", 16384).unwrap();
    assert!(!invalid.usable);
    assert!(invalid.items.iter().any(|i| i.outcome == "invalid_utf8"));
    fs::remove_file(repo.join("AGENTS.md")).unwrap();
    fs::remove_file(repo.join("README.md")).unwrap();
    fs::remove_file(repo.join("docs/runbook/development.md")).unwrap();
    let empty = prepare(&snapshot, "fixture", 16384).unwrap();
    assert!(!empty.usable);
    assert!(empty.items.iter().all(|i| i.outcome == "missing"));
}

#[test]
fn revisions_digest_and_approval_invalidation_stop_stale_launches() {
    let (dir, _repo, mut store, id) = setup();
    let task = create(&mut store, &id);
    assert!(store.run_fake(&task.id, task.revision).is_err());
    assert!(
        store
            .approve(&task.id, task.revision, "wrong digest", "human")
            .is_err()
    );
    let mut competing = Store::open(&dir.path().join("state")).unwrap();
    let approved = approve(&mut store, &task);
    assert!(
        competing
            .revise(
                &task.id,
                task.revision,
                "stale".into(),
                FakeOutcome::Success
            )
            .is_err()
    );
    let revised = store
        .revise(
            &approved.id,
            approved.revision,
            "new content".into(),
            FakeOutcome::Success,
        )
        .unwrap();
    assert!(revised.approval.is_none());
    assert_ne!(
        revised.assignment.digest().unwrap(),
        approved.assignment.digest().unwrap()
    );
    assert!(store.run_fake(&revised.id, revised.revision).is_err());
    assert_eq!(store.events(&task.id).unwrap().len(), 3);
}

#[test]
fn supervised_flow_preserves_dirty_checkout_and_captures_new_file_patch() {
    let (dir, repo, mut store, id) = setup();
    fs::write(repo.join("personal.txt"), "private untracked data").unwrap();
    fs::write(repo.join("tracked.txt"), "staged work").unwrap();
    git(&repo, &["add", "tracked.txt"]).unwrap();
    let before = git(&repo, &["status", "--porcelain=v1", "-z"]).unwrap();
    let task = create(&mut store, &id);
    let approved = approve(&mut store, &task);
    let run = store.run_fake(&task.id, approved.revision).unwrap();
    assert_eq!(run.status, Status::AwaitingReview, "{run:#?}");
    let attempt = run.attempts.last().unwrap();
    assert_eq!(attempt.execution, "succeeded");
    let receipt = attempt.receipt.as_ref().unwrap();
    assert_eq!(receipt.result, "passed");
    assert!(receipt.check.contains("no project tests"));
    assert!(
        fs::read_to_string(&attempt.diff.as_ref().unwrap().path)
            .unwrap()
            .contains("+hello from fake")
    );
    assert_eq!(
        before,
        git(&repo, &["status", "--porcelain=v1", "-z"]).unwrap()
    );
    assert_eq!(
        fs::read_to_string(repo.join("personal.txt")).unwrap(),
        "private untracked data"
    );
    assert!(!repo.join("conductor-result.md").exists());
    assert!(!attempt.workspace.join("personal.txt").exists());
    let accepted = store
        .review(
            &run.id,
            run.revision,
            &store.evidence_digest(&run).unwrap(),
            "Fixture Human",
            true,
        )
        .unwrap();
    assert_eq!(accepted.status, Status::Accepted);
    drop(store);
    let reopened = Store::open(&dir.path().join("state")).unwrap();
    assert_eq!(reopened.task(&task.id).unwrap().status, Status::Accepted);
    let events = reopened.events(&task.id).unwrap();
    assert_eq!(
        events.iter().map(|e| e.sequence).collect::<Vec<_>>(),
        (1..=accepted.revision).collect::<Vec<_>>()
    );
}

#[test]
fn dirty_context_and_changed_base_do_not_launch() {
    let (_dir, repo, mut store, id) = setup();
    let task = create(&mut store, &id);
    let approved = approve(&mut store, &task);
    fs::write(repo.join("README.md"), "changed after approval").unwrap();
    assert!(store.run_fake(&task.id, approved.revision).is_err());
    let dirty = create(&mut store, &id);
    let dirty = approve(&mut store, &dirty);
    let failed = store.run_fake(&dirty.id, dirty.revision).unwrap();
    assert_eq!(failed.status, Status::Failed);
    assert_eq!(failed.attempts.last().unwrap().execution, "not_started");
    assert!(
        failed
            .attempts
            .last()
            .unwrap()
            .error
            .as_ref()
            .unwrap()
            .contains("context differs")
    );
    git(&repo, &["add", "README.md"]).unwrap();
    commit(&repo, "base drift");
    assert!(task.assignment.repository.revalidate().is_err());
}

#[test]
fn fake_failure_text_does_not_mean_success_and_failed_attempts_are_retained() {
    let (_dir, _repo, mut store, id) = setup();
    for outcome in [FakeOutcome::FailBeforeWrite, FakeOutcome::FailAfterWrite] {
        let task = create(&mut store, &id);
        let revised = store
            .revise(
                &task.id,
                task.revision,
                "tests passed (agent claim only)".into(),
                outcome.clone(),
            )
            .unwrap();
        let approved = approve(&mut store, &revised);
        let failed = store.run_fake(&task.id, approved.revision).unwrap();
        assert_eq!(failed.status, Status::Failed);
        let attempt = failed.attempts.last().unwrap();
        assert_eq!(attempt.execution, "failed");
        assert!(attempt.diff.is_some());
        assert!(attempt.receipt.is_none());
        assert_eq!(
            attempt.process_receipt.as_ref().unwrap().exit_code,
            Some(if outcome == FakeOutcome::FailBeforeWrite {
                3
            } else {
                4
            })
        );
        assert_eq!(
            attempt.output.is_some(),
            outcome == FakeOutcome::FailAfterWrite
        );
        assert!(
            store
                .review(
                    &failed.id,
                    failed.revision,
                    &store.evidence_digest(&failed).unwrap(),
                    "human",
                    true
                )
                .is_err()
        );
        let revised = store
            .revise(
                &failed.id,
                failed.revision,
                "retry".into(),
                FakeOutcome::Success,
            )
            .unwrap();
        let approved = approve(&mut store, &revised);
        let retried = store.run_fake(&task.id, approved.revision).unwrap();
        assert_eq!(retried.attempts.len(), 2);
        assert_ne!(retried.attempts[0].id, retried.attempts[1].id);
    }
}

#[test]
fn existing_output_records_workspace_failure_without_overwriting() {
    let (dir, repo, _old_store, _old_id) = setup();
    fs::write(repo.join("conductor-result.md"), "existing result").unwrap();
    git(&repo, &["add", "."]).unwrap();
    commit(&repo, "existing output");
    let mut store = Store::open(&dir.path().join("fresh-state")).unwrap();
    let (new_id, _) = store.register(&repo).unwrap();
    let task = create(&mut store, &new_id);
    let approved = approve(&mut store, &task);
    let failed = store.run_fake(&task.id, approved.revision).unwrap();
    assert_eq!(failed.status, Status::Failed);
    assert_eq!(failed.attempts[0].execution, "not_started");
    assert_eq!(
        fs::read_to_string(repo.join("conductor-result.md")).unwrap(),
        "existing result"
    );
}

#[test]
fn changed_artifacts_workspace_and_evidence_cannot_be_accepted() {
    for tamper in [
        "artifact",
        "workspace",
        "digest",
        "execution_log",
        "verification_log",
    ] {
        let (_dir, _repo, mut store, id) = setup();
        let task = create(&mut store, &id);
        let approved = approve(&mut store, &task);
        let run = store.run_fake(&task.id, approved.revision).unwrap();
        let attempt = &run.attempts[0];
        let evidence = store.evidence_digest(&run).unwrap();
        match tamper {
            "artifact" => fs::write(&attempt.diff.as_ref().unwrap().path, "tampered").unwrap(),
            "workspace" => {
                fs::write(attempt.workspace.join("conductor-result.md"), "tampered").unwrap()
            }
            "execution_log" => fs::write(
                &attempt
                    .process_receipt
                    .as_ref()
                    .unwrap()
                    .stdout
                    .artifact
                    .path,
                "tampered",
            )
            .unwrap(),
            "verification_log" => fs::write(
                &attempt
                    .receipt
                    .as_ref()
                    .unwrap()
                    .process_receipt
                    .as_ref()
                    .unwrap()
                    .stdout
                    .artifact
                    .path,
                "tampered",
            )
            .unwrap(),
            _ => (),
        }
        let reviewed_digest = if tamper == "digest" {
            "wrong digest"
        } else {
            &evidence
        };
        assert!(
            store
                .review(&run.id, run.revision, reviewed_digest, "human", true)
                .is_err()
        );
        assert_eq!(store.task(&task.id).unwrap().status, Status::AwaitingReview);
    }
}

#[test]
fn cli_help_exposes_the_workflow() {
    let output = Command::new(env!("CARGO_BIN_EXE_dev-conductor"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    for name in [
        "register",
        "context",
        "create",
        "approve",
        "run",
        "review",
        "reconcile",
        "cancel",
    ] {
        assert!(help.contains(name));
    }
}

#[test]
fn newly_selected_docs_and_oversized_required_sources_block_dispatch() {
    let (_dir, repo, _store, _) = setup();
    let snapshot = Repository::inspect(&repo).unwrap();
    let packet = prepare(&snapshot, "fixture", 16384).unwrap();
    fs::write(repo.join("docs/runbook/new.md"), "New applicable guidance").unwrap();
    git(&repo, &["add", "docs/runbook/new.md"]).unwrap();
    assert!(packet.revalidate(&snapshot).is_err());
    fs::write(repo.join("AGENTS.md"), "a".repeat(128 * 1024 + 1)).unwrap();
    let oversized = prepare(&snapshot, "fixture", 16384).unwrap();
    assert!(!oversized.usable);
    assert!(
        oversized
            .items
            .iter()
            .any(|i| i.outcome.contains("128 KiB"))
    );
}

#[test]
fn git_worktree_creation_failure_is_durable_and_not_running() {
    let (_dir, repo, mut store, id) = setup();
    let task = create(&mut store, &id);
    let approved = approve(&mut store, &task);
    fs::write(repo.join(".git/worktrees"), "fixture obstruction").unwrap();
    let failed = store.run_fake(&task.id, approved.revision).unwrap();
    assert_eq!(failed.status, Status::Failed);
    assert_eq!(failed.attempts[0].execution, "not_started");
    assert!(
        failed.attempts[0]
            .error
            .as_ref()
            .unwrap()
            .contains("Git operation")
    );
    assert_eq!(
        store.events(&task.id).unwrap().last().unwrap().kind,
        "workspace_failed"
    );
}

#[test]
fn ignored_new_output_is_still_part_of_review_patch() {
    let (dir, repo, _store, _id) = setup();
    fs::write(repo.join(".gitignore"), "conductor-result.md\n").unwrap();
    git(&repo, &["add", ".gitignore"]).unwrap();
    commit(&repo, "ignore output fixture");
    let mut store = Store::open(&dir.path().join("fresh")).unwrap();
    let (id, _) = store.register(&repo).unwrap();
    let task = create(&mut store, &id);
    let approved = approve(&mut store, &task);
    let run = store.run_fake(&task.id, approved.revision).unwrap();
    assert_eq!(run.status, Status::AwaitingReview);
    assert!(
        fs::read_to_string(&run.attempts[0].diff.as_ref().unwrap().path)
            .unwrap()
            .contains("+hello from fake")
    );
}

fn cli(state: &Path, args: &[&str]) -> serde_json::Value {
    let output = Command::new(env!("CARGO_BIN_EXE_dev-conductor"))
        .arg("--state")
        .arg(state)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn cli_register_to_review_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture(&dir.path().join("repository with spaces"));
    let state = dir.path().join("state with spaces");
    let registration = cli(&state, &["register", repo.to_str().unwrap()]);
    let repository_id = registration["id"].as_str().unwrap();
    let created = cli(
        &state,
        &[
            "create",
            repository_id,
            "Create result",
            "--acceptance",
            "One reviewed fixture file",
            "--contents",
            "CLI fixture result",
        ],
    );
    let id = created["task"]["id"].as_str().unwrap();
    assert_eq!(created["task"]["status"], "awaiting_approval");
    let approved = cli(
        &state,
        &[
            "approve",
            id,
            "--revision",
            "1",
            "--digest",
            created["assignment_digest"].as_str().unwrap(),
            "--by",
            "Fixture Human",
        ],
    );
    assert_eq!(approved["task"]["status"], "approved");
    let run = cli(&state, &["run", id, "--revision", "2"]);
    assert_eq!(run["task"]["status"], "awaiting_review");
    assert_eq!(run["process_sandbox"], false);
    let summary = cli(&state, &["show", id, "--summary"]);
    assert_eq!(summary["status"], "awaiting_review");
    assert!(summary["executions"][0]["duration_ms"].is_u64());
    assert!(summary["executions"][0]["tokens"]["input"]["total"].is_null());
    assert!(summary["executions"][0]["estimated_api_cost"].is_null());
    let reviewed = cli(
        &state,
        &[
            "review",
            id,
            "--revision",
            &run["task"]["revision"].as_u64().unwrap().to_string(),
            "--digest",
            run["evidence_digest"].as_str().unwrap(),
            "--by",
            "Fixture Human",
            "--accept",
        ],
    );
    assert_eq!(reviewed["task"]["status"], "accepted");
    assert_eq!(
        cli(&state, &["events", id]).as_array().unwrap().len() as u64,
        reviewed["task"]["revision"].as_u64().unwrap()
    );
    assert_eq!(cli(&state, &["show", id])["task"]["status"], "accepted");
}

#[test]
fn copied_state_cannot_silently_retarget_an_approved_workspace() {
    let (dir, _repo, mut store, id) = setup();
    let task = create(&mut store, &id);
    let approved = approve(&mut store, &task);
    let original_root = store.root.clone();
    drop(store);
    let copied_root = dir.path().join("copied-state");
    fs::create_dir(&copied_root).unwrap();
    fs::copy(
        original_root.join("state.sqlite"),
        copied_root.join("state.sqlite"),
    )
    .unwrap();
    let mut copied = Store::open(&copied_root).unwrap();
    let error = copied.run_fake(&task.id, approved.revision).unwrap_err();
    assert!(error.to_string().contains("approved workspace"));
    assert_eq!(copied.task(&task.id).unwrap().status, Status::Approved);
}

#[test]
fn cli_failed_attempt_has_nonzero_exit_and_persisted_failure() {
    let (dir, _repo, mut store, id) = setup();
    let task = create(&mut store, &id);
    let revised = store
        .revise(
            &task.id,
            task.revision,
            "tests passed".into(),
            FakeOutcome::FailBeforeWrite,
        )
        .unwrap();
    let approved = approve(&mut store, &revised);
    let output = Command::new(env!("CARGO_BIN_EXE_dev-conductor"))
        .arg("--state")
        .arg(dir.path().join("state"))
        .args([
            "run",
            &task.id,
            "--revision",
            &approved.revision.to_string(),
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let recorded: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(recorded["task"]["status"], "failed");
    assert_eq!(store.task(&task.id).unwrap().status, Status::Failed);
    let summary = cli(&store.root, &["show", &task.id, "--summary"]);
    assert_eq!(summary["status"], "failed");
    assert!(summary["executions"][0]["duration_ms"].is_u64());
    assert!(summary["executions"][0]["tokens"]["input"]["total"].is_null());
}

fn process_task(store: &mut Store, repo: &str, outcome: FakeOutcome, timeout: u64) -> Task {
    let mut policy = dev_conductor::process::ExecutionPolicy::new(Path::new(env!(
        "CARGO_BIN_EXE_dev-conductor"
    )))
    .unwrap();
    policy.timeout_ms = timeout;
    policy.verification_timeout_ms = timeout;
    policy.log_bytes = 512;
    let task = store
        .create(
            repo,
            "Process fixture".into(),
            "Inspect actual fixture evidence".into(),
            "expected output\n".into(),
            16384,
            policy,
        )
        .unwrap();
    let revised = store
        .revise(
            &task.id,
            task.revision,
            task.assignment.contents.clone(),
            outcome,
        )
        .unwrap();
    approve(store, &revised)
}

fn poll_task(store: &Store, id: &str, ready: impl Fn(&Task) -> bool) -> Task {
    let started = std::time::Instant::now();
    loop {
        let task = store.task(id).unwrap();
        if ready(&task) {
            return task;
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(15),
            "timed out awaiting fixture state: {task:#?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn descendant_identity(root: &Path, task: &Task) -> dev_conductor::process::ProcessIdentity {
    let path = root
        .join("artifacts")
        .join(&task.attempts.last().unwrap().id)
        .join("execution.descendant.json");
    let started = std::time::Instant::now();
    loop {
        if let Ok(bytes) = fs::read(&path)
            && let Ok(identity) = serde_json::from_slice(&bytes)
        {
            return identity;
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "descendant never started"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn assert_stopped(identity: &dev_conductor::process::ProcessIdentity) {
    use dev_conductor::process::{Presence, presence};
    let started = std::time::Instant::now();
    loop {
        if matches!(presence(identity), Presence::Exited | Presence::Missing) {
            return;
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "owned process still alive or unproven: {identity:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[test]
fn pipe_floods_are_drained_but_retained_logs_are_bounded() {
    let (_dir, _repo, mut store, id) = setup();
    let task = process_task(&mut store, &id, FakeOutcome::OutputFlood, 15000);
    let run = store.run_fake(&task.id, task.revision).unwrap();
    assert_eq!(run.status, Status::AwaitingReview, "{run:#?}");
    let attempt = &run.attempts[0];
    let execution = attempt.process_receipt.as_ref().unwrap();
    for log in [&execution.stdout, &execution.stderr] {
        assert!(log.total_bytes >= 2 * 1024 * 1024);
        assert!(log.truncated);
        assert_eq!(log.artifact.bytes, 512);
        assert_eq!(fs::metadata(&log.artifact.path).unwrap().len(), 512);
        assert_eq!(
            fs::metadata(log.artifact.path.with_extension("part"))
                .unwrap()
                .len(),
            512
        );
    }
    assert!(execution.tree_terminated);
    let check = attempt
        .receipt
        .as_ref()
        .unwrap()
        .process_receipt
        .as_ref()
        .unwrap();
    assert_ne!(execution.identity, check.identity);
    assert_eq!(check.exit_code, Some(0));
    assert!(!check.stdout.truncated);
}

#[test]
fn timeout_preserves_logs_and_stops_the_owned_tree() {
    let (_dir, _repo, mut store, id) = setup();
    let task = process_task(&mut store, &id, FakeOutcome::SpawnDescendant, 1500);
    let root = store.root.clone();
    let task_id = task.id.clone();
    let run = std::thread::spawn(move || store.run_fake(&task.id, task.revision).unwrap());
    let observer = Store::open(&root).unwrap();
    let active = poll_task(&observer, &task_id, |t| {
        t.status == Status::Running && t.attempts[0].process.is_some()
    });
    let descendant = descendant_identity(&root, &active);
    let finished = run.join().unwrap();
    assert_eq!(finished.status, Status::TimedOut, "{finished:#?}");
    let attempt = &finished.attempts[0];
    let receipt = attempt.process_receipt.as_ref().unwrap();
    assert_eq!(receipt.outcome, "timed_out");
    assert!(receipt.tree_terminated);
    assert!(
        fs::read_to_string(&receipt.stdout.artifact.path)
            .unwrap()
            .contains("tests passed")
    );
    assert!(attempt.receipt.is_none());
    assert!(attempt.diff.is_some());
    assert_stopped(&receipt.identity);
    assert_stopped(&descendant);
}

#[test]
fn revision_checked_cancellation_uses_live_owner_and_stops_descendants() {
    let (_dir, _repo, mut store, id) = setup();
    let task = process_task(&mut store, &id, FakeOutcome::SpawnDescendant, 30000);
    let root = store.root.clone();
    let task_id = task.id.clone();
    let run = std::thread::spawn(move || store.run_fake(&task.id, task.revision).unwrap());
    let mut observer = Store::open(&root).unwrap();
    let active = poll_task(&observer, &task_id, |t| {
        t.status == Status::Running && t.attempts[0].process.is_some()
    });
    let descendant = descendant_identity(&root, &active);
    assert_eq!(
        dev_conductor::process::presence(&descendant),
        dev_conductor::process::Presence::Alive
    );
    assert!(
        observer
            .reconcile(&task_id, active.revision)
            .unwrap_err()
            .to_string()
            .contains("supervisor is alive")
    );
    assert!(observer.run_fake(&task_id, active.revision).is_err());
    assert!(
        observer
            .cancel(&task_id, active.revision - 1, "fixture human")
            .is_err()
    );
    let requested = observer
        .cancel(&task_id, active.revision, "fixture human")
        .unwrap();
    assert_eq!(
        requested.attempts[0].cancel_requested.as_ref().unwrap().by,
        "fixture human"
    );
    let finished = run.join().unwrap();
    assert_eq!(finished.status, Status::Cancelled, "{finished:#?}");
    let receipt = finished.attempts[0].process_receipt.as_ref().unwrap();
    assert_eq!(receipt.outcome, "cancelled");
    assert!(receipt.tree_terminated);
    assert_stopped(&receipt.identity);
    assert_stopped(&descendant);
    let revised = observer
        .revise(
            &task_id,
            finished.revision,
            "new attempt".into(),
            FakeOutcome::Success,
        )
        .unwrap();
    assert!(revised.approval.is_none());
    assert_ne!(revised.assignment.workspace, finished.assignment.workspace);
}

#[test]
fn independent_verification_failure_or_wrong_bytes_cannot_be_accepted() {
    for outcome in [FakeOutcome::VerificationFails, FakeOutcome::WrongOutput] {
        let (_dir, _repo, mut store, id) = setup();
        let task = process_task(&mut store, &id, outcome, 5000);
        let run = store.run_fake(&task.id, task.revision).unwrap();
        assert_eq!(run.status, Status::Failed, "{run:#?}");
        let attempt = &run.attempts[0];
        assert_eq!(attempt.execution, "succeeded");
        assert_eq!(attempt.process_receipt.as_ref().unwrap().exit_code, Some(0));
        let receipt = attempt.receipt.as_ref().unwrap();
        assert_eq!(receipt.result, "failed");
        assert_eq!(receipt.exit_code, Some(7));
        assert_eq!(
            receipt.source_hash.as_ref(),
            Some(&attempt.diff.as_ref().unwrap().hash)
        );
        assert!(
            store
                .review(
                    &run.id,
                    run.revision,
                    &store.evidence_digest(&run).unwrap(),
                    "human",
                    true
                )
                .is_err()
        );
    }
}

#[test]
fn verifier_has_its_own_timeout_and_cancellation_outcome() {
    for cancel in [false, true] {
        let (_dir, _repo, mut store, id) = setup();
        let task = process_task(
            &mut store,
            &id,
            FakeOutcome::VerificationHang,
            if cancel { 30000 } else { 1000 },
        );
        let root = store.root.clone();
        let task_id = task.id.clone();
        let run = std::thread::spawn(move || store.run_fake(&task.id, task.revision).unwrap());
        let mut observer = Store::open(&root).unwrap();
        let active = poll_task(&observer, &task_id, |t| {
            t.status == Status::Verifying && t.attempts[0].verification_process.is_some()
        });
        if cancel {
            // Cancellation remains a process outcome even if external workspace drift
            // would make subsequent host checks fail.
            fs::remove_file(
                active
                    .assignment
                    .workspace
                    .join(&active.assignment.output_file),
            )
            .unwrap();
            observer
                .cancel(&task_id, active.revision, "fixture human")
                .unwrap();
        }
        let finished = run.join().unwrap();
        assert_eq!(
            finished.status,
            if cancel {
                Status::Cancelled
            } else {
                Status::TimedOut
            }
        );
        let attempt = &finished.attempts[0];
        assert_eq!(attempt.execution, "succeeded");
        let receipt = attempt.receipt.as_ref().unwrap();
        assert_eq!(
            receipt.result,
            if cancel { "cancelled" } else { "timed_out" }
        );
        assert!(receipt.process_receipt.as_ref().unwrap().tree_terminated);
        assert_stopped(attempt.verification_process.as_ref().unwrap());
    }
}

struct OwnedCli(std::process::Child);
impl Drop for OwnedCli {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn supervisor_crash_kills_job_tree_and_restart_never_replays() {
    let (_dir, _repo, mut store, id) = setup();
    let task = process_task(&mut store, &id, FakeOutcome::SpawnDescendant, 30000);
    let mut cli = OwnedCli(
        Command::new(env!("CARGO_BIN_EXE_dev-conductor"))
            .arg("--state")
            .arg(&store.root)
            .args(["run", &task.id, "--revision", &task.revision.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let active = poll_task(&store, &task.id, |t| {
        t.status == Status::Running && t.attempts[0].process.is_some()
    });
    let descendant = descendant_identity(&store.root, &active);
    cli.0.kill().unwrap();
    cli.0.wait().unwrap();
    let interrupted = store.task(&task.id).unwrap();
    assert_eq!(interrupted.status, Status::Running);
    assert_stopped(interrupted.attempts[0].process.as_ref().unwrap());
    assert_stopped(&descendant);
    assert!(
        store
            .cancel(&task.id, interrupted.revision, "human")
            .is_err()
    );
    let root = store.root.clone();
    drop(store);
    let mut reopened = Store::open(&root).unwrap();
    let unknown = reopened.reconcile(&task.id, interrupted.revision).unwrap();
    assert_eq!(unknown.status, Status::Unknown);
    assert_eq!(unknown.attempts.len(), 1);
    assert!(reopened.run_fake(&task.id, unknown.revision).is_err());
    assert!(
        !unknown
            .assignment
            .workspace
            .join(&unknown.assignment.output_file)
            .exists()
    );
    assert!(
        root.join("artifacts")
            .join(&unknown.attempts[0].id)
            .join("execution.stdout.part")
            .exists()
    );
}

#[test]
fn crash_during_verification_preserves_proven_execution_but_never_claims_a_pass() {
    let (_dir, _repo, mut store, id) = setup();
    let task = process_task(&mut store, &id, FakeOutcome::VerificationHang, 30000);
    let mut cli = OwnedCli(
        Command::new(env!("CARGO_BIN_EXE_dev-conductor"))
            .arg("--state")
            .arg(&store.root)
            .args(["run", &task.id, "--revision", &task.revision.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let active = poll_task(&store, &task.id, |t| {
        t.status == Status::Verifying && t.attempts[0].verification_process.is_some()
    });
    cli.0.kill().unwrap();
    cli.0.wait().unwrap();
    assert_stopped(active.attempts[0].verification_process.as_ref().unwrap());
    let unknown = store.reconcile(&task.id, active.revision).unwrap();
    assert_eq!(unknown.status, Status::Unknown);
    assert_eq!(unknown.attempts[0].execution, "succeeded");
    assert_eq!(
        unknown.attempts[0]
            .process_receipt
            .as_ref()
            .unwrap()
            .exit_code,
        Some(0)
    );
    assert!(unknown.attempts[0].receipt.is_none());
    assert!(store.run_fake(&task.id, unknown.revision).is_err());
}

#[test]
fn failed_log_finalization_keeps_outcome_unknown_and_owned_process_stopped() {
    let (_dir, _repo, mut store, id) = setup();
    let task = process_task(&mut store, &id, FakeOutcome::Hang, 30000);
    let root = store.root.clone();
    let task_id = task.id.clone();
    let run = std::thread::spawn(move || store.run_fake(&task.id, task.revision).unwrap());
    let mut observer = Store::open(&root).unwrap();
    let active = poll_task(&observer, &task_id, |t| {
        t.status == Status::Running && t.attempts[0].process.is_some()
    });
    let obstruction = root
        .join("artifacts")
        .join(&active.attempts[0].id)
        .join("execution.stdout.log");
    fs::create_dir(&obstruction).unwrap();
    observer
        .cancel(&task_id, active.revision, "fixture human")
        .unwrap();
    let finished = run.join().unwrap();
    assert_eq!(finished.status, Status::Unknown, "{finished:#?}");
    assert_eq!(finished.attempts[0].execution, "unknown");
    assert!(finished.attempts[0].process_receipt.is_none());
    assert_stopped(active.attempts[0].process.as_ref().unwrap());
    assert!(observer.run_fake(&task_id, finished.revision).is_err());
}

#[test]
fn gated_worker_cannot_change_workspace_before_launch_receipt() {
    use dev_conductor::{
        fixture::Spec,
        process::{OwnedProcess, current_identity},
    };
    for durable_identity in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let spec = Spec {
            workspace: dir.path().into(),
            output_file: "result.txt".into(),
            contents: "must not appear".into(),
            outcome: FakeOutcome::Success,
            gate: dir.path().join("gate"),
            token: "fixture token".into(),
            owner: current_identity().unwrap(),
        };
        let path = dir.path().join("spec.json");
        fs::write(&path, serde_json::to_vec(&spec).unwrap()).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_dev-conductor"));
        command
            .args(["__fixture-worker", path.to_str().unwrap(), &spec.token])
            .current_dir(dir.path());
        let owned = OwnedProcess::spawn(&mut command, 256, None).unwrap();
        let identity = owned.identity.clone();
        if durable_identity {
            fs::write(
                dir.path().join("launch.json"),
                serde_json::to_vec(&identity).unwrap(),
            )
            .unwrap();
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(!dir.path().join("result.txt").exists());
        drop(owned);
        assert_stopped(&identity);
        assert!(!dir.path().join("result.txt").exists());
    }
}

#[test]
fn process_limits_and_executable_are_approval_bound_and_revalidated() {
    let (dir, _repo, mut store, id) = setup();
    let task = create(&mut store, &id);
    let mut assignment = task.assignment.clone();
    assignment.execution_policy.as_mut().unwrap().timeout_ms += 1;
    assert_ne!(
        assignment.digest().unwrap(),
        task.assignment.digest().unwrap()
    );
    assignment.execution_policy.as_mut().unwrap().log_bytes = 0;
    assert!(assignment.validate().is_err());
    let exe = dir.path().join("owned-fixture.exe");
    fs::copy(env!("CARGO_BIN_EXE_dev-conductor"), &exe).unwrap();
    let policy = dev_conductor::process::ExecutionPolicy::new(&exe).unwrap();
    let task = store
        .create(
            &id,
            "fixture".into(),
            "fixture".into(),
            "result".into(),
            16384,
            policy,
        )
        .unwrap();
    let approved = approve(&mut store, &task);
    fs::write(&exe, "changed executable").unwrap();
    assert!(
        store
            .run_fake(&task.id, approved.revision)
            .unwrap_err()
            .to_string()
            .contains("executable changed")
    );
    assert!(store.task(&task.id).unwrap().attempts.is_empty());
}

// These helpers inject old/interrupted records only into expendable fixture databases.
fn replace_fixture_record(root: &Path, task: &Task) {
    let mut connection = rusqlite::Connection::open(root.join("state.sqlite")).unwrap();
    let transaction = connection.transaction().unwrap();
    let record = serde_json::to_string(task).unwrap();
    transaction
        .execute(
            "UPDATE tasks SET record=?1 WHERE id=?2",
            rusqlite::params![record, task.id],
        )
        .unwrap();
    transaction
        .execute(
            "UPDATE events SET record=?1 WHERE task_id=?2 AND sequence=?3",
            rusqlite::params![record, task.id, task.revision],
        )
        .unwrap();
    transaction.commit().unwrap();
}

#[test]
fn stale_creation_identity_declines_cancellation_and_never_targets_unrelated_process() {
    use dev_conductor::process::{Presence, current_identity, presence};
    let (_dir, _repo, mut store, id) = setup();
    let task = create(&mut store, &id);
    let approved = approve(&mut store, &task);
    let mut interrupted = store.run_fake(&task.id, approved.revision).unwrap();
    let current = current_identity().unwrap();
    let mut stale = current.clone();
    stale.created += 1;
    assert_eq!(presence(&stale), Presence::Mismatch);
    interrupted.status = Status::Running;
    interrupted.attempts[0].supervisor = Some(current.clone());
    interrupted.attempts[0].process = Some(stale.clone());
    interrupted.attempts[0].verification_process = None;
    replace_fixture_record(&store.root, &interrupted);
    assert!(
        store
            .cancel(&task.id, interrupted.revision, "human")
            .unwrap_err()
            .to_string()
            .contains("identity is unproven")
    );
    assert!(store.reconcile(&task.id, interrupted.revision).is_err());
    interrupted.attempts[0].supervisor = Some(stale);
    replace_fixture_record(&store.root, &interrupted);
    let unknown = store.reconcile(&task.id, interrupted.revision).unwrap();
    assert_eq!(unknown.status, Status::Unknown);
    assert!(
        unknown.attempts[0]
            .error
            .as_ref()
            .unwrap()
            .contains("Mismatch")
    );
    assert_eq!(presence(&current), Presence::Alive);
}

#[test]
fn version_zero_migration_preserves_legacy_approval_receipts_and_evidence() {
    let (_dir, _repo, mut store, id) = setup();
    let task = create(&mut store, &id);
    let approved = approve(&mut store, &task);
    let mut legacy = store.run_fake(&task.id, approved.revision).unwrap();
    legacy.assignment.backend = "fake-inline-v1".into();
    legacy.assignment.execution_policy = None;
    let assignment_digest = legacy.assignment.digest().unwrap();
    legacy.approval.as_mut().unwrap().assignment_digest = assignment_digest.clone();
    let attempt = &mut legacy.attempts[0];
    attempt.assignment_digest = assignment_digest.clone();
    attempt.supervisor = None;
    attempt.process = None;
    attempt.verification_process = None;
    attempt.process_receipt = None;
    attempt.cancel_requested = None;
    let receipt = attempt.receipt.as_mut().unwrap();
    receipt.assignment_digest = assignment_digest.clone();
    receipt.command = None;
    receipt.exit_code = None;
    receipt.source_hash = None;
    receipt.process_receipt = None;
    let evidence = store.evidence_digest(&legacy).unwrap();
    replace_fixture_record(&store.root, &legacy);
    let root = store.root.clone();
    drop(store);
    let connection = rusqlite::Connection::open(root.join("state.sqlite")).unwrap();
    connection.execute_batch("PRAGMA user_version=0;").unwrap();
    let before: String = connection
        .query_row("SELECT record FROM tasks WHERE id=?1", [&task.id], |r| {
            r.get(0)
        })
        .unwrap();
    drop(connection);
    let mut reopened = Store::open(&root).unwrap();
    let loaded = reopened.task(&task.id).unwrap();
    assert_eq!(loaded.assignment.digest().unwrap(), assignment_digest);
    assert_eq!(reopened.evidence_digest(&loaded).unwrap(), evidence);
    assert!(
        loaded
            .assignment
            .validate()
            .unwrap_err()
            .to_string()
            .contains("legacy")
    );
    let connection = rusqlite::Connection::open(root.join("state.sqlite")).unwrap();
    let after: String = connection
        .query_row("SELECT record FROM tasks WHERE id=?1", [&task.id], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(before, after);
    assert_eq!(
        connection
            .query_row("PRAGMA user_version", [], |r| r.get::<_, u32>(0))
            .unwrap(),
        1
    );
    let accepted = reopened
        .review(&task.id, loaded.revision, &evidence, "Fixture Human", true)
        .unwrap();
    assert_eq!(accepted.status, Status::Accepted);
    drop(reopened);
    connection.execute_batch("PRAGMA user_version=2;").unwrap();
    assert!(
        Store::open(&root)
            .err()
            .unwrap()
            .to_string()
            .contains("newer")
    );
}

#[test]
fn bounded_commands_reject_truncation_and_timeouts_instead_of_using_partial_output() {
    use dev_conductor::{
        fixture::Spec,
        process::{bounded_output, current_identity},
    };
    for timeout in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let spec = Spec {
            workspace: dir.path().into(),
            output_file: "result.txt".into(),
            contents: "result".into(),
            outcome: if timeout {
                FakeOutcome::Hang
            } else {
                FakeOutcome::OutputFlood
            },
            gate: dir.path().join("gate"),
            token: "fixture token".into(),
            owner: current_identity().unwrap(),
        };
        let path = dir.path().join("spec.json");
        fs::write(&path, serde_json::to_vec(&spec).unwrap()).unwrap();
        fs::write(&spec.gate, &spec.token).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_dev-conductor"));
        command
            .args(["__fixture-worker", path.to_str().unwrap(), &spec.token])
            .current_dir(dir.path());
        let error = bounded_output(&mut command, if timeout { 100 } else { 5000 }, 256)
            .err()
            .unwrap();
        assert!(
            error
                .to_string()
                .contains(if timeout { "deadline" } else { "limit" })
        );
    }
}

const PROVIDER_CONTENTS: &str = "def add(a, b):\n    return a + b\n";
fn codex_stub() -> PathBuf {
    static STUB: std::sync::OnceLock<TempDir> = std::sync::OnceLock::new();
    STUB.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        let result = Command::new("rustc")
            .args(["--edition=2024", "tests/fixtures/codex_stub.rs", "-o"])
            .arg(dir.path().join("codex-stub.exe"))
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        dir
    })
    .path()
    .join("codex-stub.exe")
}

fn codex_task(store: &mut Store, id: &str, model: &str, verifier_mode: &str, timeout: u64) -> Task {
    use dev_conductor::{
        codex::{CodexPolicy, Executable, PinnedCommand},
        process::ExecutionPolicy,
        store::CodexRequest,
    };
    let home = store.root.join("stub-home");
    fs::create_dir_all(&home).unwrap();
    let binary = Executable::new(&codex_stub()).unwrap();
    let mut execution =
        ExecutionPolicy::new(Path::new(env!("CARGO_BIN_EXE_dev-conductor"))).unwrap();
    execution.timeout_ms = timeout;
    execution.verification_timeout_ms = timeout;
    execution.log_bytes = 512;
    let policy = CodexPolicy::inspect(
        &binary.path,
        &home,
        model.into(),
        PinnedCommand {
            executable: binary.clone(),
            args: vec!["--check".into(), verifier_mode.into()],
        },
    )
    .unwrap();
    let task = store
        .create_codex(CodexRequest {
            repository_id: id.into(),
            objective: "Create a Python add function".into(),
            acceptance: "Approved exact function, independently checked".into(),
            output_file: "result.py".into(),
            contents: PROVIDER_CONTENTS.into(),
            budget: 16384,
            execution,
            policy,
        })
        .unwrap();
    approve(store, &task)
}

fn delegated_identity(
    store: &Store,
    task: &Task,
    phase: &str,
) -> dev_conductor::process::ProcessIdentity {
    let path = store
        .root
        .join("artifacts")
        .join(&task.attempts[0].id)
        .join(format!("{phase}.delegated.json"));
    let started = std::time::Instant::now();
    loop {
        if let Ok(receipt) = dev_conductor::launcher::read_receipt(&path) {
            return receipt.identity;
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "provider launch receipt absent"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[test]
fn codex_adapter_routes_pinned_commands_and_context_to_owned_children() {
    let (_dir, repo, mut store, id) = setup();
    let before = git(&repo, &["status", "--porcelain=v1", "-z"]).unwrap();
    let task = codex_task(&mut store, &id, "success", "pass", 30000);
    let run = store.run(&task.id, task.revision).unwrap();
    assert_eq!(run.status, Status::AwaitingReview, "{run:#?}");
    let attempt = &run.attempts[0];
    let execution = attempt.process_receipt.as_ref().unwrap();
    let provider = execution.delegated.as_ref().unwrap();
    let policy = run.assignment.codex.as_ref().unwrap();
    assert_eq!(provider.command, policy.command(&attempt.workspace).argv());
    assert_eq!(provider.exit_code, Some(0));
    assert!(provider.protocol.as_ref().unwrap().completed());
    let summary = cli(&store.root, &["show", &run.id, "--summary"]);
    assert_eq!(summary["executions"][0]["tokens"]["output"]["total"], 8);
    assert!(summary["executions"][0]["tokens"]["input"]["total"].is_null());
    assert!(summary["executions"][0]["estimated_api_cost"].is_null());
    assert!(summary["executions"][0]["inference_requests"].is_null());
    assert_ne!(execution.identity, provider.identity);
    assert_stopped(&provider.identity);
    let verification = attempt
        .receipt
        .as_ref()
        .unwrap()
        .process_receipt
        .as_ref()
        .unwrap();
    let check = verification.delegated.as_ref().unwrap();
    assert_eq!(check.command, policy.verifier.argv());
    assert_ne!(check.identity, provider.identity);
    assert!(verification.tree_terminated && execution.tree_terminated);
    assert_eq!(
        before,
        git(&repo, &["status", "--porcelain=v1", "-z"]).unwrap()
    );
    assert!(!repo.join("result.py").exists());
    let accepted = store
        .review(
            &run.id,
            run.revision,
            &store.evidence_digest(&run).unwrap(),
            "Fixture Human",
            true,
        )
        .unwrap();
    assert_eq!(accepted.status, Status::Accepted);
}

#[test]
fn codex_reports_need_valid_completion_and_separate_verification() {
    let (_dir, _repo, mut store, id) = setup();
    for model in [
        "incomplete",
        "malformed",
        "empty",
        "failed-event",
        "oversize",
        "nonzero",
        "wrong",
        "no-write",
    ] {
        let task = codex_task(&mut store, &id, model, "pass", 30000);
        let run = store.run(&task.id, task.revision).unwrap();
        assert_eq!(run.status, Status::Failed, "model={model} {run:#?}");
        let attempt = &run.attempts[0];
        let execution = attempt.process_receipt.as_ref().unwrap();
        if matches!(model, "wrong" | "no-write") {
            assert_eq!(attempt.execution, "succeeded");
            assert_eq!(attempt.receipt.as_ref().unwrap().result, "failed");
        } else {
            assert_eq!(attempt.execution, "failed");
            assert!(attempt.receipt.is_none());
            if model != "nonzero" {
                assert_eq!(execution.exit_code, Some(0));
            }
        }
        assert!(execution.tree_terminated);
        assert!(
            store
                .review(
                    &run.id,
                    run.revision,
                    &store.evidence_digest(&run).unwrap(),
                    "Fixture Human",
                    true
                )
                .is_err()
        );
    }
    for verifier in ["fail", "mutate"] {
        let task = codex_task(&mut store, &id, "success", verifier, 30000);
        let run = store.run(&task.id, task.revision).unwrap();
        assert_eq!(run.status, Status::Failed, "{run:#?}");
        assert_eq!(run.attempts[0].execution, "succeeded");
        assert_eq!(run.attempts[0].receipt.as_ref().unwrap().result, "failed");
    }
}

#[test]
fn codex_scope_violations_preserve_patch_and_block_verification() {
    let (_dir, _repo, mut store, id) = setup();
    for model in ["tracked", "extra"] {
        let task = codex_task(&mut store, &id, model, "pass", 30000);
        let run = store.run(&task.id, task.revision).unwrap();
        assert_eq!(run.status, Status::Failed, "{run:#?}");
        let attempt = &run.attempts[0];
        assert_eq!(attempt.execution, "succeeded");
        assert!(
            attempt
                .error
                .as_ref()
                .unwrap()
                .contains("outside approved scope")
        );
        assert!(attempt.receipt.is_none());
        let patch = fs::read_to_string(&attempt.diff.as_ref().unwrap().path).unwrap();
        assert!(patch.contains(if model == "tracked" {
            "README.md"
        } else {
            "extra.txt"
        }));
    }
}

#[test]
fn codex_completion_survives_log_truncation_without_unbounded_protocol_storage() {
    let (_dir, _repo, mut store, id) = setup();
    let task = codex_task(&mut store, &id, "flood", "pass", 30000);
    let run = store.run(&task.id, task.revision).unwrap();
    assert_eq!(run.status, Status::AwaitingReview, "{run:#?}");
    let execution = run.attempts[0].process_receipt.as_ref().unwrap();
    assert!(execution.stdout.truncated);
    assert!(execution.stdout.total_bytes > 2 * 1024 * 1024);
    assert_eq!(execution.stdout.artifact.bytes, 512);
    let protocol = execution
        .delegated
        .as_ref()
        .unwrap()
        .protocol
        .as_ref()
        .unwrap();
    assert!(protocol.completed());
    assert_eq!(protocol.events, 50004);
    assert!(serde_json::to_vec(protocol).unwrap().len() < 1024);
}

#[test]
fn codex_launcher_stop_timeout_and_crash_preserve_no_replay_contract() {
    for mode in [
        "cancel",
        "timeout",
        "crash",
        "verify-cancel",
        "verify-crash",
    ] {
        let (_dir, _repo, mut store, id) = setup();
        let verify = mode.starts_with("verify");
        let task = codex_task(
            &mut store,
            &id,
            if verify { "success" } else { "hang" },
            if verify { "hang" } else { "pass" },
            if mode == "timeout" { 1500 } else { 30000 },
        );
        let mut cli = OwnedCli(
            Command::new(env!("CARGO_BIN_EXE_dev-conductor"))
                .arg("--state")
                .arg(&store.root)
                .args(["run", &task.id, "--revision", &task.revision.to_string()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
        let active = poll_task(&store, &task.id, |t| {
            if verify {
                t.status == Status::Verifying && t.attempts[0].verification_process.is_some()
            } else {
                t.status == Status::Running && t.attempts[0].process.is_some()
            }
        });
        let child = delegated_identity(
            &store,
            &active,
            if verify { "verification" } else { "execution" },
        );
        if mode.ends_with("crash") {
            cli.0.kill().unwrap();
        } else if mode != "timeout" {
            store
                .cancel(&task.id, active.revision, "Fixture Human")
                .unwrap();
        }
        cli.0.wait().unwrap();
        assert_stopped(&child);
        let latest = store.task(&task.id).unwrap();
        if mode.ends_with("crash") {
            let unknown = store.reconcile(&task.id, latest.revision).unwrap();
            assert_eq!(unknown.status, Status::Unknown);
            assert_eq!(
                unknown.attempts[0].execution,
                if verify { "succeeded" } else { "unknown" }
            );
            assert!(store.run(&task.id, unknown.revision).is_err());
        } else {
            assert_eq!(
                latest.status,
                if mode == "timeout" {
                    Status::TimedOut
                } else {
                    Status::Cancelled
                },
                "{latest:#?}"
            );
        }
    }
}

#[test]
fn invalid_codex_scope_does_not_create_a_task_and_policy_changes_change_approval() {
    let (_dir, _repo, mut store, id) = setup();
    let task = codex_task(&mut store, &id, "success", "pass", 30000);
    let mut assignment = task.assignment.clone();
    assignment.codex.as_mut().unwrap().model = "different".into();
    assert_ne!(
        assignment.digest().unwrap(),
        task.assignment.digest().unwrap()
    );
    assignment = task.assignment.clone();
    assignment
        .codex
        .as_mut()
        .unwrap()
        .verifier
        .args
        .push("different".into());
    assert_ne!(
        assignment.digest().unwrap(),
        task.assignment.digest().unwrap()
    );
    let count = store.tasks().unwrap().len();
    let invalid = dev_conductor::store::CodexRequest {
        repository_id: id,
        objective: "fixture".into(),
        acceptance: "fixture".into(),
        output_file: "../escape.py".into(),
        contents: PROVIDER_CONTENTS.into(),
        budget: 16384,
        execution: task.assignment.execution_policy.unwrap(),
        policy: task.assignment.codex.unwrap(),
    };
    assert!(store.create_codex(invalid).is_err());
    assert_eq!(store.tasks().unwrap().len(), count);
}

#[test]
fn codex_cli_preparation_requires_approval_and_scrubs_ambient_key_variables() {
    let dir = tempfile::tempdir().unwrap();
    let repo = fixture(&dir.path().join("checkout"));
    let state = dir.path().join("state");
    let home = dir.path().join("empty-codex-home");
    fs::create_dir(&home).unwrap();
    let registered = cli(&state, &["register", repo.to_str().unwrap()]);
    let stub = codex_stub();
    let created = cli(
        &state,
        &[
            "create-codex",
            registered["id"].as_str().unwrap(),
            "Create add function",
            "--acceptance",
            "Independent function check",
            "--contents",
            PROVIDER_CONTENTS,
            "--output-file",
            "result.py",
            "--codex-executable",
            stub.to_str().unwrap(),
            "--codex-home",
            home.to_str().unwrap(),
            "--model",
            "success",
            "--verify-executable",
            stub.to_str().unwrap(),
            "--verify-arg=--check",
            "--verify-arg=pass",
        ],
    );
    let id = created["task"]["id"].as_str().unwrap();
    assert_eq!(created["task"]["status"], "awaiting_approval");
    assert_eq!(
        created["task"]["assignment"]["execution_policy"]["timeout_ms"],
        120000
    );
    assert!(Store::open(&state).unwrap().run(id, 1).is_err());
    let approved = cli(
        &state,
        &[
            "approve",
            id,
            "--revision",
            "1",
            "--digest",
            created["assignment_digest"].as_str().unwrap(),
            "--by",
            "Fixture Human",
        ],
    );
    let output = Command::new(env!("CARGO_BIN_EXE_dev-conductor"))
        .arg("--state")
        .arg(&state)
        .args([
            "run",
            id,
            "--revision",
            &approved["task"]["revision"].as_u64().unwrap().to_string(),
        ])
        .env("CODEX_API_KEY", "fixture-canary")
        .env("OPENAI_API_KEY", "fixture-canary")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["task"]["status"], "awaiting_review");
    assert!(fs::read_dir(&home).unwrap().next().is_none());
}

#[test]
fn delegated_command_cannot_launch_before_owned_parent_gate_is_released() {
    use dev_conductor::{
        codex::{Executable, PinnedCommand},
        launcher::LaunchSpec,
        process::{OwnedProcess, current_identity},
    };
    let dir = tempfile::tempdir().unwrap();
    let spec = LaunchSpec {
        workspace: dir.path().into(),
        gate: dir.path().join("gate"),
        token: "fixture token".into(),
        owner: current_identity().unwrap(),
        command: PinnedCommand {
            executable: Executable::new(&codex_stub()).unwrap(),
            args: vec!["--check".into()],
        },
        codex_home: None,
        input: String::new(),
        receipt_path: dir.path().join("delegated.json"),
        parse_codex: false,
    };
    let path = dir.path().join("launch.json");
    fs::write(&path, serde_json::to_vec(&spec).unwrap()).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_dev-conductor"));
    command
        .args(["__provider-launcher", path.to_str().unwrap(), &spec.token])
        .current_dir(dir.path());
    let parent = OwnedProcess::spawn(&mut command, 256, None).unwrap();
    let identity = parent.identity.clone();
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert!(!spec.receipt_path.exists());
    drop(parent);
    assert_stopped(&identity);
    assert!(!spec.receipt_path.exists());
}

#[cfg(windows)]
fn read_fixture_process_identity(pid: u32) -> dev_conductor::process::ProcessIdentity {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, FILETIME},
        System::Threading::{GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    };
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    assert!(!handle.is_null());
    let mut creation: FILETIME = unsafe { std::mem::zeroed() };
    let mut exit = creation;
    let mut kernel = creation;
    let mut user = creation;
    let result =
        unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) };
    unsafe {
        CloseHandle(handle);
    }
    assert_ne!(result, 0);
    dev_conductor::process::ProcessIdentity {
        pid,
        created: ((creation.dwHighDateTime as u64) << 32) | creation.dwLowDateTime as u64,
    }
}

#[test]
#[cfg(windows)]
fn delegated_provider_descendants_inherit_owned_job_and_stop_on_cancellation() {
    let (_dir, _repo, mut store, id) = setup();
    let task = codex_task(&mut store, &id, "descendant", "pass", 30000);
    let mut cli = OwnedCli(
        Command::new(env!("CARGO_BIN_EXE_dev-conductor"))
            .arg("--state")
            .arg(&store.root)
            .args(["run", &task.id, "--revision", &task.revision.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let active = poll_task(&store, &task.id, |t| {
        t.status == Status::Running && t.attempts[0].process.is_some()
    });
    let child = delegated_identity(&store, &active, "execution");
    let path = active.assignment.workspace.join("descendant.pid");
    let started = std::time::Instant::now();
    let pid = loop {
        if let Ok(text) = fs::read_to_string(&path)
            && let Ok(pid) = text.parse()
        {
            break pid;
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    let descendant = read_fixture_process_identity(pid);
    store
        .cancel(&task.id, active.revision, "Fixture Human")
        .unwrap();
    cli.0.wait().unwrap();
    assert_stopped(&child);
    assert_stopped(&descendant);
    let stopped = store.task(&task.id).unwrap();
    assert_eq!(stopped.status, Status::Cancelled, "{stopped:#?}");
    assert!(
        stopped.attempts[0]
            .process_receipt
            .as_ref()
            .unwrap()
            .tree_terminated
    );
}
