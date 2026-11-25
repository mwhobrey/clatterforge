use clatterforge::{
    agent_preset::AgentPresets,
    onboard::{
        DiscoveryEnv, OnboardRequest, detect_suggested_files_with_count, detect_verifier,
        detect_verifiers_in, execute_onboard_in, find_binary_on_path, probe_environment_in,
    },
    repository::git,
    workflow::Workflows,
};
use std::{fs, path::PathBuf, sync::OnceLock};
use tempfile::TempDir;

fn init_fixture_repo(dir: &TempDir) -> std::path::PathBuf {
    let repo = dir.path().join("test-repo");
    fs::create_dir_all(&repo).unwrap();
    fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname = \"dummy\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::write(repo.join("src/lib.rs"), "pub fn dummy() -> bool { true }\n").unwrap();
    fs::write(repo.join("README.md"), "# Dummy Project\n").unwrap();

    git(&repo, &["init", "-b", "main"]).unwrap();
    git(&repo, &["config", "core.autocrlf", "false"]).unwrap();
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
            "initial",
        ],
    )
    .unwrap();
    repo
}

#[test]
fn test_detect_verifier_cargo() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    fs::write(repo.join("Cargo.toml"), "[package]\n").unwrap();

    let (verifier, verification_files) = detect_verifier(&repo);
    let v = verifier.expect("cargo verifier should be detected");
    assert_eq!(v.framework, "Rust (Cargo)");
    // No lockfile: `--locked` would fail every verification run.
    assert_eq!(v.args, vec!["test"]);
    assert_eq!(verification_files, vec!["Cargo.toml"]);

    fs::write(repo.join("Cargo.lock"), "").unwrap();
    let (verifier, verification_files) = detect_verifier(&repo);
    assert_eq!(verifier.unwrap().args, vec!["test", "--locked"]);
    assert_eq!(verification_files, vec!["Cargo.toml"]);
}

#[test]
fn test_detect_verifier_npm() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    fs::create_dir_all(&repo).unwrap();

    // `package.json` without `scripts.test` or with npm's default placeholder is ignored (`O11`).
    fs::write(repo.join("package.json"), "{\"name\": \"test\"}\n").unwrap();
    assert!(detect_verifier(&repo).0.is_none());

    fs::write(
        repo.join("package.json"),
        "{\"name\": \"test\", \"scripts\": {\"test\": \"echo \\\"Error: no test specified\\\" && exit 1\"}}\n",
    )
    .unwrap();
    assert!(detect_verifier(&repo).0.is_none());

    fs::write(
        repo.join("package.json"),
        "{\"name\": \"test\", \"scripts\": {\"test\": \"vitest run\"}}\n",
    )
    .unwrap();
    let (verifier, verification_files) = detect_verifier(&repo);
    let v = verifier.expect("npm verifier should be detected when scripts.test is configured");
    assert_eq!(v.framework, "Node (npm)");
    assert_eq!(v.args, vec!["test"]);
    assert_eq!(verification_files, vec!["package.json"]);
}

#[test]
fn test_detect_verifier_pytest() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    fs::create_dir_all(&repo).unwrap();

    // `requirements.txt` alone must not trigger a Python verifier (`O11`).
    fs::write(repo.join("requirements.txt"), "requests==2.31.0\n").unwrap();
    assert!(detect_verifier(&repo).0.is_none());

    fs::write(repo.join("pytest.ini"), "[pytest]\n").unwrap();
    let (verifier, verification_files) = detect_verifier(&repo);
    let v = verifier.expect("python verifier should be detected");
    assert!(v.framework.contains("Python"));
    assert_eq!(verification_files, vec!["pytest.ini"]);
}

#[test]
fn test_detect_verifiers_mixed_repository_and_file_ranking() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_fixture_repo(&dir);
    fs::write(
        repo.join("package.json"),
        "{\"name\": \"mixed\", \"scripts\": {\"test\": \"jest\"}}\n",
    )
    .unwrap();
    fs::create_dir_all(repo.join("src/nested/deep")).unwrap();
    fs::write(repo.join("src/main.rs"), "fn main() {}\n").unwrap();
    fs::write(repo.join("src/alpha.rs"), "pub fn a() {}\n").unwrap();
    fs::write(repo.join("src/nested/deep/z.rs"), "pub fn z() {}\n").unwrap();
    fs::create_dir_all(repo.join("tests")).unwrap();
    fs::write(repo.join("tests/aaa_test.rs"), "#[test] fn t() {}\n").unwrap();
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
            "add mixed files",
        ],
    )
    .unwrap();

    let all = detect_verifiers_in(&repo, &DiscoveryEnv::from_process());
    assert_eq!(all.len(), 2, "both Rust and Node verifiers should be found");
    assert_eq!(all[0].0.framework, "Rust (Cargo)");
    assert_eq!(all[1].0.framework, "Node (npm)");

    // Entrypoints (`src/lib.rs`, `src/main.rs`) and top-level `src/` modules rank ahead of
    // nested modules and `tests/aaa_test.rs` even though `tests/aaa_test.rs` sorts earlier
    // alphabetically in `src/` vs `tests/` (`O10`).
    let (suggested, total) = detect_suggested_files_with_count(&repo);
    assert_eq!(total, 5);
    assert_eq!(suggested[0], "src/lib.rs");
    assert_eq!(suggested[1], "src/main.rs");
    assert_eq!(suggested[2], "src/alpha.rs");
    assert_eq!(suggested[3], "src/nested/deep/z.rs");
    assert_eq!(suggested[4], "tests/aaa_test.rs");
}

#[test]
fn test_detect_verifier_go() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    fs::write(repo.join("go.mod"), "module test\n").unwrap();

    let (verifier, verification_files) = detect_verifier(&repo);
    let v = verifier.expect("go verifier should be detected");
    assert_eq!(v.framework, "Go");
    assert_eq!(v.args, vec!["test", "./..."]);
    assert_eq!(verification_files, vec!["go.mod"]);
}

#[test]
fn test_find_binary_git() {
    let git = find_binary_on_path("git");
    assert!(git.is_some(), "git should be on PATH");
}

fn hermetic_cursor_env() -> &'static (TempDir, DiscoveryEnv, PathBuf) {
    static ENV: OnceLock<(TempDir, DiscoveryEnv, PathBuf)> = OnceLock::new();
    ENV.get_or_init(|| {
        let temp = tempfile::tempdir().unwrap();
        let node_name = if cfg!(windows) { "node.exe" } else { "node" };
        let node_path = temp.path().join(node_name);
        assert!(
            std::process::Command::new("rustc")
                .args(["tests/fixtures/fake_cursor_agent.rs", "-o"])
                .arg(&node_path)
                .status()
                .unwrap()
                .success()
        );
        let verifier_name = if cfg!(windows) { "cargo.exe" } else { "cargo" };
        let verifier_path = temp.path().join(verifier_name);
        fs::copy(&node_path, &verifier_path).unwrap();
        let versions = temp
            .path()
            .join("cursor-agent")
            .join("versions")
            .join("2026.1.0");
        fs::create_dir_all(&versions).unwrap();
        fs::write(versions.join("index.js"), "// hermetic fixture").unwrap();
        let env = DiscoveryEnv::isolated(temp.path());
        (temp, env, verifier_path)
    })
}

#[test]
fn test_probe_environment_clean_repo() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_fixture_repo(&dir);
    let (_, env, _) = hermetic_cursor_env();

    let report =
        probe_environment_in(&repo, Some("http://127.0.0.1:1"), env).expect("probe should succeed");
    assert!(report.git_clean, "repo should be clean");
    assert_eq!(report.git_branch.as_deref(), Some("main"));
    assert!(report.detected_verifier.is_some());
    assert!(!report.suggested_allowed_files.is_empty());
    assert!(
        report
            .detected_clis
            .iter()
            .any(|c| c.name == "cursor" && c.available)
    );
    assert!(
        report
            .detected_clis
            .iter()
            .any(|c| c.name == "codex" && !c.available)
    );
}

#[test]
fn test_probe_environment_dirty_repo() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_fixture_repo(&dir);
    fs::write(repo.join("dirty.txt"), "uncommitted\n").unwrap();
    let (_, env, _) = hermetic_cursor_env();

    let report =
        probe_environment_in(&repo, Some("http://127.0.0.1:1"), env).expect("probe should succeed");
    assert!(
        !report.git_clean,
        "repo with untracked file should be dirty"
    );
}

#[test]
fn test_execute_onboard_end_to_end() {
    let (_, env, verifier_exe) = hermetic_cursor_env();
    let dir = tempfile::tempdir().unwrap();
    let repo = init_fixture_repo(&dir);
    let state = dir.path().join("state");
    fs::create_dir_all(&state).unwrap();

    let req = OnboardRequest {
        profile_name: "test-profile".into(),
        repository_path: repo.clone(),
        model: "test-model:latest".into(),
        model_digest: Some(
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
        ),
        endpoint: Some("http://127.0.0.1:11434".into()),
        verifier_executable: verifier_exe.clone(),
        verifier_args: vec!["test".into(), "--locked".into()],
        verification_files: vec!["Cargo.toml".into()],
        allowed_files: vec!["src/lib.rs".into()],
        workers: vec![],
        low_spec: true,
        by: "test-operator".into(),
    };

    let profile = execute_onboard_in(&state, req, env).expect("onboard should succeed");
    assert_eq!(profile.name, "test-profile");
    assert_eq!(profile.decision_timeout_ms, 120_000); // Low-spec decision timeout
    assert_eq!(profile.workers[0].preset.verification_timeout_ms, 300_000);
    assert_eq!(profile.created_by, "test-operator");
    assert_eq!(profile.scope(), vec!["src/lib.rs"]);

    // Verify it is persisted in Workflows
    let workflows = Workflows::open(&state).unwrap();
    let retrieved = workflows.profile("test-profile").unwrap();
    assert_eq!(retrieved.name, "test-profile");
    assert_eq!(retrieved.workers.len(), profile.workers.len());

    // Verify presets exist in AgentPresets and have valid CLI models (never passing Ollama model to Cursor/Codex)
    let presets = AgentPresets::open(&state).unwrap();
    for worker in &profile.workers {
        let preset = presets.get(&worker.preset.name).unwrap();
        assert!(preset.agentlet.is_some());
        match &preset.backend {
            clatterforge::agents::AgentBackend::Local(p) => {
                assert_eq!(p.model, "test-model:latest");
                assert_eq!(
                    preset.effective_tier(),
                    clatterforge::agents::ModelTier::Local
                );
                assert_eq!(preset.allowed_models, vec!["test-model:latest"]);
            }
            clatterforge::agents::AgentBackend::CursorAgent(p) => {
                assert_ne!(
                    p.model, "test-model:latest",
                    "Cursor CLI must not be configured with an Ollama model tag"
                );
                assert!(!p.model.trim().is_empty());
                assert!(
                    preset.allowed_models.is_empty(),
                    "CLI presets must not snapshot the live catalog into allowed_models at onboard time (O14)"
                );
            }
            clatterforge::agents::AgentBackend::Codex(p) => {
                assert_ne!(
                    p.model, "test-model:latest",
                    "Codex CLI must not be configured with an Ollama model tag"
                );
                assert!(!p.model.trim().is_empty());
                assert!(preset.allowed_models.is_empty());
            }
            clatterforge::agents::AgentBackend::Agy(p) => {
                assert_ne!(
                    p.model, "test-model:latest",
                    "AGY CLI must not be configured with an Ollama model tag"
                );
                assert!(!p.model.trim().is_empty());
                assert!(preset.allowed_models.is_empty());
            }
        }
    }
    assert!(
        profile
            .workers
            .iter()
            .any(|w| w.preset.effective_tier() == clatterforge::agents::ModelTier::Fast),
        "onboard profile should include a fast tier worker"
    );
    assert!(
        profile
            .workers
            .iter()
            .any(|w| w.preset.effective_tier() == clatterforge::agents::ModelTier::Frontier),
        "onboard profile should include a frontier tier worker for complex/escalated tasks"
    );
}

#[test]
fn test_execute_onboard_builtin_router_and_cursor_only_tiers() {
    let (_, env, verifier_exe) = hermetic_cursor_env();
    let dir = tempfile::tempdir().unwrap();
    let repo = init_fixture_repo(&dir);
    let state = dir.path().join("state");
    fs::create_dir_all(&state).unwrap();

    let req = OnboardRequest {
        profile_name: "cursor-only-profile".into(),
        repository_path: repo.clone(),
        model: "builtin:router-v1".into(),
        model_digest: None,
        endpoint: None,
        verifier_executable: verifier_exe.clone(),
        verifier_args: vec!["test".into(), "--locked".into()],
        verification_files: vec!["Cargo.toml".into()],
        allowed_files: vec!["src/lib.rs".into()],
        workers: vec!["cursor".into()],
        low_spec: false,
        by: "cursor-operator".into(),
    };

    let profile = execute_onboard_in(&state, req, env)
        .expect("builtin:router-v1 + cursor-only onboard should succeed without Ollama");
    assert!(clatterforge::workflow::is_builtin_router(
        &profile.inference
    ));
    // Verify all 3 implementation tiers (Fast, Standard, Frontier) + Reviewer are provisioned on Cursor
    assert!(
        profile
            .workers
            .iter()
            .any(|w| w.preset.effective_tier() == clatterforge::agents::ModelTier::Fast)
    );
    assert!(
        profile
            .workers
            .iter()
            .any(|w| w.preset.effective_tier() == clatterforge::agents::ModelTier::Standard)
    );
    assert!(
        profile
            .workers
            .iter()
            .any(|w| w.preset.effective_tier() == clatterforge::agents::ModelTier::Frontier)
    );
    for w in &profile.workers {
        assert_eq!(w.preset.backend.target(), "cursor-agent");
    }
}

#[test]
fn test_reonboard_updates_existing_profile_and_failed_onboard_rolls_back_presets() {
    let (_, env, verifier_exe) = hermetic_cursor_env();
    let dir = tempfile::tempdir().unwrap();
    let repo = init_fixture_repo(&dir);
    fs::write(repo.join("src/extra.rs"), "pub fn extra() {}\n").unwrap();
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
            "add extra.rs",
        ],
    )
    .unwrap();

    let state = dir.path().join("state");
    fs::create_dir_all(&state).unwrap();

    let req1 = OnboardRequest {
        profile_name: "mutable-profile".into(),
        repository_path: repo.clone(),
        model: "builtin:router-v1".into(),
        model_digest: None,
        endpoint: None,
        verifier_executable: verifier_exe.clone(),
        verifier_args: vec!["test".into()],
        verification_files: vec!["Cargo.toml".into()],
        allowed_files: vec!["src/lib.rs".into()],
        workers: vec!["cursor".into()],
        low_spec: false,
        by: "op-1".into(),
    };
    let p1 = execute_onboard_in(&state, req1, env).unwrap();
    assert_eq!(p1.scope(), vec!["src/lib.rs"]);

    // Re-onboard with updated allowed_files (`O5`): presets and profile must reflect the new scope.
    let req2 = OnboardRequest {
        profile_name: "mutable-profile".into(),
        repository_path: repo.clone(),
        model: "builtin:router-v1".into(),
        model_digest: None,
        endpoint: None,
        verifier_executable: verifier_exe.clone(),
        verifier_args: vec!["test".into()],
        verification_files: vec!["Cargo.toml".into()],
        allowed_files: vec!["src/lib.rs".into(), "src/extra.rs".into()],
        workers: vec!["cursor".into()],
        low_spec: true,
        by: "op-2".into(),
    };
    let p2 = execute_onboard_in(&state, req2, env).unwrap();
    assert_eq!(p2.scope(), vec!["src/extra.rs", "src/lib.rs"]);
    assert_eq!(p2.decision_timeout_ms, 120_000);
    let presets = AgentPresets::open(&state).unwrap();
    assert_eq!(
        presets
            .get("mutable-profile-implementer")
            .unwrap()
            .allowed_files,
        vec!["src/lib.rs", "src/extra.rs"]
    );

    // A failed re-onboard (e.g., requesting unavailable `codex` worker alongside `cursor`)
    // rolls back `mutable-profile-*` presets to their `p2` state (`O5`).
    let bad_req = OnboardRequest {
        profile_name: "mutable-profile".into(),
        repository_path: repo.clone(),
        model: "builtin:router-v1".into(),
        model_digest: None,
        endpoint: None,
        verifier_executable: verifier_exe.clone(),
        verifier_args: vec!["test".into()],
        verification_files: vec!["Cargo.toml".into()],
        allowed_files: vec!["README.md".into()],
        workers: vec!["cursor".into(), "codex".into()],
        low_spec: false,
        by: "op-3".into(),
    };
    assert!(execute_onboard_in(&state, bad_req, env).is_err());
    let presets_after = AgentPresets::open(&state).unwrap();
    assert_eq!(
        presets_after
            .get("mutable-profile-implementer")
            .unwrap()
            .allowed_files,
        vec!["src/lib.rs", "src/extra.rs"]
    );

    // Profile deletion (`M2`).
    let workflows = Workflows::open(&state).unwrap();
    workflows.delete_profile("mutable-profile").unwrap();
    assert!(workflows.profile("mutable-profile").is_err());
}

#[test]
fn test_preset_routing_policy_trims_deduplicates_and_enforces_strict_allowlist() {
    let (_, env, verifier_exe) = hermetic_cursor_env();
    let dir = tempfile::tempdir().unwrap();
    let repo = init_fixture_repo(&dir);
    let state = dir.path().join("state");
    fs::create_dir_all(&state).unwrap();

    let req = OnboardRequest {
        profile_name: "strict-profile".into(),
        repository_path: repo,
        model: "builtin:router-v1".into(),
        model_digest: None,
        endpoint: None,
        verifier_executable: verifier_exe.clone(),
        verifier_args: vec!["test".into()],
        verification_files: vec!["Cargo.toml".into()],
        allowed_files: vec!["src/lib.rs".into()],
        workers: vec!["cursor".into()],
        low_spec: false,
        by: "operator".into(),
    };
    execute_onboard_in(&state, req, env).unwrap();

    let presets = AgentPresets::open(&state).unwrap();
    let updated = presets
        .set_routing_policy(
            "strict-profile-implementer",
            Some(clatterforge::agents::ModelTier::Standard),
            vec![
                "  composer-2.5  ".into(),
                "composer-2.5".into(),
                "claude-4.5-sonnet".into(),
            ],
        )
        .unwrap();
    assert_eq!(
        updated.allowed_models,
        vec!["composer-2.5", "claude-4.5-sonnet"]
    );
    assert!(updated.allows_model("composer-2.5"));
    assert!(updated.allows_model("claude-4.5-sonnet"));
    assert!(!updated.allows_model("claude-opus-5-thinking-high"));
}

#[test]
fn test_provider_status_probe_report() {
    let dir = tempfile::tempdir().unwrap();
    let report = clatterforge::provider_status::probe_all_providers_with_options(
        dir.path(),
        Some("http://127.0.0.1:1"),
        true,
    );
    assert!(report.probed_at_ms > 0);
    assert_eq!(report.providers.len(), 5);
    assert!(report.providers.iter().any(|p| p.id == "agy"));
    for p in &report.providers {
        assert!(!p.name.is_empty());
        assert!(!p.quota_type.is_empty());
        if p.id == "claude" {
            assert!(!p.dispatchable);
            assert_eq!(p.quota_type, "informational_only");
        } else {
            assert!(p.dispatchable);
        }
    }
}

#[test]
fn test_detect_suggested_files_ranks_crate_sources_ahead_of_tsx_tests_and_returns_more_than_eight()
{
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::create_dir_all(repo.join("crates/core-lib/src")).unwrap();
    fs::write(repo.join("Cargo.toml"), "[workspace]\n").unwrap();
    fs::write(repo.join("src/App.tsx"), "export function App() {}\n").unwrap();
    for i in 0..10 {
        fs::write(
            repo.join(format!("src/Companion{i:02}.test.tsx")),
            "test('ok', () => {});\n",
        )
        .unwrap();
    }
    fs::write(
        repo.join("crates/core-lib/src/lib.rs"),
        "pub mod recorder;\n",
    )
    .unwrap();
    fs::write(
        repo.join("crates/core-lib/src/recorder.rs"),
        "pub fn query() {}\n",
    )
    .unwrap();

    assert!(
        clatterforge::repository::git(&repo, &["init", "-b", "main"]).is_ok(),
        "git init"
    );
    clatterforge::repository::git(&repo, &["add", "."]).unwrap();
    clatterforge::repository::git(
        &repo,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "init workspace",
        ],
    )
    .unwrap();

    let (suggested, total) = detect_suggested_files_with_count(&repo);
    assert_eq!(total, 13);
    assert_eq!(suggested.len(), 13);
    assert!(
        suggested
            .iter()
            .any(|p| p == "crates/core-lib/src/recorder.rs")
    );
    let recorder_pos = suggested
        .iter()
        .position(|p| p == "crates/core-lib/src/recorder.rs")
        .unwrap();
    let first_test_pos = suggested
        .iter()
        .position(|p| p.ends_with(".test.tsx"))
        .unwrap();
    assert!(
        recorder_pos < first_test_pos,
        "workspace crate source {recorder_pos} must rank ahead of .test.tsx tests {first_test_pos}: {suggested:?}"
    );
}

#[test]
fn test_execute_onboard_accepts_over_sixteen_allowed_files() {
    let (_, env, verifier_exe) = hermetic_cursor_env();
    let dir = tempfile::tempdir().unwrap();
    let repo = init_fixture_repo(&dir);
    let state = dir.path().join("state");
    fs::create_dir_all(&state).unwrap();

    let allowed_files: Vec<String> = (0..24).map(|i| format!("src/module_{i:02}.rs")).collect();
    let req = OnboardRequest {
        profile_name: "wide-scope".into(),
        repository_path: repo,
        model: "builtin:router-v1".into(),
        model_digest: None,
        endpoint: None,
        verifier_executable: verifier_exe.clone(),
        verifier_args: vec!["test".into()],
        verification_files: vec!["Cargo.toml".into()],
        allowed_files: allowed_files.clone(),
        workers: vec!["cursor".into()],
        low_spec: false,
        by: "operator".into(),
    };

    let profile = execute_onboard_in(&state, req, env).unwrap();
    assert_eq!(profile.workers[0].preset.allowed_files.len(), 24);
    assert_eq!(profile.workers[0].preset.allowed_files, allowed_files);
}

#[test]
fn test_reonboard_profile_refreshes_commit_timeouts_and_preserves_scope() {
    use clatterforge::onboard::reonboard_profile_in;

    let (_, env, verifier_exe) = hermetic_cursor_env();
    let dir = tempfile::tempdir().unwrap();
    let repo = init_fixture_repo(&dir);
    let state = dir.path().join("state");
    fs::create_dir_all(&state).unwrap();

    let custom_scope = vec!["src/custom_a.rs".to_string(), "src/custom_b.rs".to_string()];
    let req = OnboardRequest {
        profile_name: "reonboard-proj".into(),
        repository_path: repo.clone(),
        model: "builtin:router-v1".into(),
        model_digest: None,
        endpoint: None,
        verifier_executable: verifier_exe.clone(),
        verifier_args: vec!["test".into()],
        verification_files: vec!["Cargo.toml".into()],
        allowed_files: custom_scope.clone(),
        workers: vec!["cursor".into()],
        low_spec: false,
        by: "operator".into(),
    };
    let initial = execute_onboard_in(&state, req, env).unwrap();
    let first_base = initial.repository.base.clone();
    assert_eq!(initial.workers[0].preset.generation_timeout_ms, 600_000);
    assert_eq!(
        initial.workers[0].preset.generation_idle_timeout_ms,
        Some(360_000)
    );

    // Advance HEAD in the repo so re-onboarding picks up the new commit while preserving scope.
    fs::write(repo.join("src/new.rs"), "pub fn refreshed() {}\n").unwrap();
    clatterforge::repository::git(&repo, &["add", "."]).unwrap();
    clatterforge::repository::git(
        &repo,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "advance head",
        ],
    )
    .unwrap();

    let refreshed =
        reonboard_profile_in(&state, "reonboard-proj", Some(true), false, "operator", env)
            .expect("re-onboard with low_spec=Some(true), refresh_scope=false");
    assert_ne!(refreshed.repository.base, first_base);
    assert_eq!(refreshed.scope(), custom_scope);
    assert_eq!(refreshed.decision_timeout_ms, 120_000);
    assert_eq!(refreshed.workers[0].preset.generation_timeout_ms, 900_000);
    assert_eq!(
        refreshed.workers[0].preset.generation_idle_timeout_ms,
        Some(600_000)
    );
    assert_eq!(refreshed.workers[0].preset.verification_timeout_ms, 300_000);
    let preset = AgentPresets::open(&state)
        .unwrap()
        .get(&refreshed.workers[0].preset.name)
        .unwrap();
    assert_eq!(preset.generation_timeout_ms, 900_000);
    assert_eq!(preset.generation_idle_timeout_ms, Some(600_000));

    // Re-onboarding with refresh_scope=true replaces stale scope with ranked repo sources.
    let rescoped =
        reonboard_profile_in(&state, "reonboard-proj", Some(false), true, "operator", env)
            .expect("re-onboard with refresh_scope=true");
    assert!(
        rescoped.scope().iter().any(|f| f == "src/new.rs"),
        "expected src/new.rs in refreshed scope, got {:?}",
        rescoped.scope()
    );
    let editable =
        clatterforge::onboard::list_editable_repo_files(&repo, &["Cargo.toml".into()]).unwrap();
    assert!(editable.iter().any(|f| f == "src/new.rs"));
    assert!(!editable.iter().any(|f| f == "Cargo.toml"));
}

#[test]
fn test_repo_index_built_on_onboard_and_resolves_natural_language_and_symbols() {
    use clatterforge::repo_index::{profile_indexed_file_count, suggest_files_for_objective};

    let (_, env, verifier_exe) = hermetic_cursor_env();
    let dir = tempfile::tempdir().unwrap();
    let repo = init_fixture_repo(&dir);
    let state = dir.path().join("state");
    fs::create_dir_all(&state).unwrap();

    fs::create_dir_all(repo.join("src")).unwrap();
    fs::create_dir_all(repo.join("crates/core-lib/src")).unwrap();
    fs::create_dir_all(repo.join("crates/core-lib/tests")).unwrap();
    fs::create_dir_all(repo.join("node_modules/pkg")).unwrap();

    fs::write(repo.join("src/App.tsx"), "export function App() {}\n").unwrap();
    fs::write(
        repo.join("crates/core-lib/src/flight_recorder.rs"),
        r#"
pub struct FlightLogEvent { pub timestamp_raw: String }
pub fn query_session_flight_log(content: &str) {
    // Handles mixed RFC3339 timestamps and UTF-8 summary truncation safely.
    let _base_t = 0;
}
"#,
    )
    .unwrap();
    fs::write(
        repo.join("crates/core-lib/tests/flight_recorder_test.rs"),
        "#[test] fn tests_truncation_and_timestamps() {}\n",
    )
    .unwrap();
    // Even if node_modules was tracked, it must never be indexed or suggested.
    fs::write(
        repo.join("node_modules/pkg/flight_recorder.js"),
        "function query_session_flight_log() {}\n",
    )
    .unwrap();

    clatterforge::repository::git(&repo, &["add", "."]).unwrap();
    clatterforge::repository::git(
        &repo,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "add flight recorder and companion files",
        ],
    )
    .unwrap();

    let req = OnboardRequest {
        profile_name: "indexed-proj".into(),
        repository_path: repo.clone(),
        model: "builtin:router-v1".into(),
        model_digest: None,
        endpoint: None,
        verifier_executable: verifier_exe.clone(),
        verifier_args: vec!["test".into()],
        verification_files: vec!["Cargo.toml".into()],
        allowed_files: vec!["src/App.tsx".into()],
        workers: vec!["cursor".into()],
        low_spec: false,
        by: "operator".into(),
    };
    let profile = execute_onboard_in(&state, req, env).unwrap();
    let indexed_count = profile_indexed_file_count(&state, "indexed-proj")
        .unwrap()
        .expect("onboard builds repo index");
    assert!(indexed_count >= 3);

    // Query with symbol + natural language ONLY (zero filenames mentioned).
    let first_hit = suggest_files_for_objective(
        &state,
        &profile,
        "Fix UTF-8 summary truncation and mixed RFC3339 ordering inside query_session_flight_log",
        "Add unit tests verifying timestamps",
        16,
    )
    .unwrap();
    assert!(first_hit.used_cached_index);
    assert_eq!(
        first_hit.matched[0].path,
        "crates/core-lib/src/flight_recorder.rs"
    );
    assert!(
        !first_hit
            .matched
            .iter()
            .any(|m| m.path.contains("node_modules")),
        "node_modules must never be suggested: {:?}",
        first_hit.matched
    );
    assert!(
        !first_hit.matched.iter().any(|m| m.path == "Cargo.toml"),
        "protected verifier files must never be suggested"
    );
    // Merged scope places matched files first, followed by top profile defaults.
    assert_eq!(
        first_hit.merged_scope[0],
        "crates/core-lib/src/flight_recorder.rs"
    );
    assert!(
        first_hit.merged_scope.iter().any(|p| p == "src/App.tsx"),
        "merged_scope should preserve top profile files: {:?}",
        first_hit.merged_scope
    );

    // Committing a new file triggers automatic cache invalidation on HEAD change.
    fs::write(
        repo.join("crates/core-lib/src/session_store.rs"),
        "pub fn persist_checkpoint_record() {}\n",
    )
    .unwrap();
    clatterforge::repository::git(&repo, &["add", "."]).unwrap();
    clatterforge::repository::git(
        &repo,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "add session store",
        ],
    )
    .unwrap();

    let after_commit = suggest_files_for_objective(
        &state,
        &profile,
        "Update persist_checkpoint_record in session store",
        "Tests pass",
        16,
    )
    .unwrap();
    assert!(!after_commit.used_cached_index);
    assert_eq!(
        after_commit.matched[0].path,
        "crates/core-lib/src/session_store.rs"
    );

    // The index is keyed on HEAD, so it reads HEAD's blobs: uncommitted edits and untracked
    // files never shape suggestions, even when the index is rebuilt with them present.
    fs::write(
        repo.join("crates/core-lib/src/session_store.rs"),
        "pub fn persist_checkpoint_record() {}\npub fn rotate_audit_journal() {}\n",
    )
    .unwrap();
    fs::write(
        repo.join("crates/core-lib/src/uncommitted_ledger.rs"),
        "pub fn rotate_audit_journal() {}\n",
    )
    .unwrap();
    clatterforge::repo_index::delete_profile_index(&state, "indexed-proj").unwrap();
    let rebuilt = suggest_files_for_objective(
        &state,
        &profile,
        "Change rotate_audit_journal",
        "Tests pass",
        16,
    )
    .unwrap();
    assert!(!rebuilt.used_cached_index);
    assert!(
        rebuilt.matched.is_empty(),
        "working-tree-only symbols and files must not be suggested: {:?}",
        rebuilt.matched
    );
}

#[test]
fn test_reonboard_with_progress_emits_monotonic_stages_and_file_counts() {
    use clatterforge::onboard::reonboard_profile_with_progress;

    let (_, env, verifier_exe) = hermetic_cursor_env();
    let dir = tempfile::tempdir().unwrap();
    let repo = init_fixture_repo(&dir);
    let state = dir.path().join("state");
    fs::create_dir_all(&state).unwrap();

    fs::create_dir_all(repo.join("src")).unwrap();
    for i in 0..30 {
        fs::write(repo.join(format!("src/mod_{i:02}.rs")), "pub fn f() {}\n").unwrap();
    }
    clatterforge::repository::git(&repo, &["add", "."]).unwrap();
    clatterforge::repository::git(
        &repo,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "add 30 files",
        ],
    )
    .unwrap();

    let req = OnboardRequest {
        profile_name: "prog-proj".into(),
        repository_path: repo,
        model: "builtin:router-v1".into(),
        model_digest: None,
        endpoint: None,
        verifier_executable: verifier_exe.clone(),
        verifier_args: vec!["test".into()],
        verification_files: vec!["Cargo.toml".into()],
        allowed_files: vec!["src/mod_00.rs".into()],
        workers: vec!["cursor".into()],
        low_spec: false,
        by: "operator".into(),
    };
    execute_onboard_in(&state, req, env).unwrap();

    let mut events = Vec::new();
    let mut cb = |stage: &str, detail: &str, pct: u8, done: usize, total: usize| {
        events.push((stage.to_string(), detail.to_string(), pct, done, total));
    };
    reonboard_profile_with_progress(&state, "prog-proj", None, true, "operator", env, &mut cb)
        .unwrap();

    assert!(events.iter().any(|(s, _, _, _, _)| s == "scan_scope"));
    assert!(events.iter().any(|(s, _, _, _, _)| s == "cursor"));
    assert!(
        events
            .iter()
            .any(|(s, _, _, done, total)| s == "indexing" && *done == *total && *total >= 30)
    );
    assert_eq!(events.last().unwrap().0, "done");
    assert_eq!(events.last().unwrap().2, 100);
    for pair in events.windows(2) {
        assert!(
            pair[0].2 <= pair[1].2,
            "percent must be monotonic: {:?} -> {:?}",
            pair[0],
            pair[1]
        );
    }
}

#[test]
fn test_detect_verifiers_excludes_tauri_bundle_when_dist_untracked() {
    use clatterforge::onboard::{DiscoveryEnv, detect_verifiers_in};

    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("sample-like");
    fs::create_dir_all(repo.join("crates/core/src")).unwrap();
    fs::create_dir_all(repo.join("src-tauri")).unwrap();

    fs::write(
        repo.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/core\", \"src-tauri\"]\n",
    )
    .unwrap();
    fs::write(repo.join("Cargo.lock"), "# lock\n").unwrap();
    fs::write(
        repo.join("crates/core/Cargo.toml"),
        "[package]\nname = \"core\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::write(
        repo.join("src-tauri/Cargo.toml"),
        "[package]\nname = \"sample-app\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::write(
        repo.join("src-tauri/build.rs"),
        "fn main() { let _ = std::fs::read_dir(\"../dist\"); }\n",
    )
    .unwrap();

    git(&repo, &["init", "-b", "main"]).unwrap();
    git(&repo, &["config", "core.autocrlf", "false"]).unwrap();
    git(&repo, &["add", "."]).unwrap();
    git(
        &repo,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "init",
        ],
    )
    .unwrap();

    let env = DiscoveryEnv::default();
    let verifiers = detect_verifiers_in(&repo, &env);
    assert_eq!(verifiers.len(), 2);
    assert_eq!(
        verifiers[0].0.args,
        vec!["test", "--locked", "--workspace", "--exclude", "sample-app"]
    );
    assert_eq!(verifiers[1].0.args, vec!["test", "--locked"]);
}
