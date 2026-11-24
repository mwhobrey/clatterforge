//! Host observations, not an agent-authored success report. Activity may renew an
//! explicitly approved inactivity window, never the independent hard run deadline.
use crate::{
    now,
    orchestration::safe_file,
    process::{ProcessControl, ProcessObservation, ProcessResult, StopReason},
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    time::SystemTime,
};

pub(crate) const SOFT_TIMEOUT_MAX_MS: u64 = 86_400_000;

fn default_true() -> bool {
    true
}

fn is_true(value: &bool) -> bool {
    *value
}

pub(crate) fn is_false(value: &bool) -> bool {
    !*value
}

fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentProgress {
    pub task_id: String,
    pub phase: String,
    pub process_id: u32,
    pub started_at: u64,
    pub observed_at: u64,
    pub elapsed_ms: u64,
    pub hard_limit_ms: u64,
    pub idle_limit_ms: Option<u64>,
    pub last_activity_at: Option<u64>,
    pub idle_ms: u64,
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
    pub changed_files: Vec<String>,
    pub file_observation_available: bool,
    pub finished: bool,
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub stale: bool,
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub limit_enforced: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub overtime: bool,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub overtime_ms: u64,
}

pub(crate) fn validate_limits(hard_ms: u64, idle_ms: Option<u64>) -> Result<()> {
    validate_limits_with_mode(hard_ms, idle_ms, false)
}

pub(crate) fn validate_limits_with_mode(
    hard_ms: u64,
    idle_ms: Option<u64>,
    soft_timeouts: bool,
) -> Result<()> {
    if soft_timeouts {
        ensure!(
            (100..=SOFT_TIMEOUT_MAX_MS).contains(&hard_ms),
            "alert threshold must be between 100 ms and 24 hours"
        );
        if let Some(idle) = idle_ms {
            ensure!(
                (100..=SOFT_TIMEOUT_MAX_MS).contains(&idle),
                "inactivity limit must be between 100 ms and 24 hours"
            );
        }
        return Ok(());
    }
    let maximum = if idle_ms.is_some() {
        1_800_000
    } else {
        120_000
    };
    ensure!(
        (100..=maximum).contains(&hard_ms),
        "invalid agent generation hard limit; runs over 120 seconds require an explicit inactivity limit (maximum 30 minutes)"
    );
    if let Some(idle) = idle_ms {
        ensure!(
            (100..=hard_ms).contains(&idle),
            "inactivity limit must be at least 100 ms and no greater than the hard limit"
        );
    }
    Ok(())
}

#[derive(Clone, PartialEq, Eq)]
struct FileStamp {
    bytes: u64,
    modified: SystemTime,
}

fn stamp(workspace: &Path, file: &str) -> Result<Option<FileStamp>> {
    let path = safe_file(workspace, file)?;
    match fs::metadata(path) {
        Ok(meta) => {
            ensure!(meta.is_file(), "activity source is not a regular file");
            Ok(Some(FileStamp {
                bytes: meta.len(),
                modified: meta.modified()?,
            }))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub(crate) struct Monitor {
    destination: PathBuf,
    workspace: PathBuf,
    files: Vec<(String, Option<Option<FileStamp>>)>,
    snapshot: AgentProgress,
    last_activity_ms: u64,
    sampled_at: Option<u64>,
    published_at: Option<u64>,
}

impl Monitor {
    #[allow(clippy::too_many_arguments, dead_code)]
    pub(crate) fn new(
        root: &Path,
        task_id: &str,
        phase: &str,
        workspace: &Path,
        files: &[String],
        hard_ms: u64,
        idle_ms: Option<u64>,
    ) -> Self {
        Self::new_with_mode(
            root, task_id, phase, workspace, files, hard_ms, idle_ms, true,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_with_mode(
        root: &Path,
        task_id: &str,
        phase: &str,
        workspace: &Path,
        files: &[String],
        hard_ms: u64,
        idle_ms: Option<u64>,
        limit_enforced: bool,
    ) -> Self {
        let mut available = true;
        let files = files
            .iter()
            .map(|p| {
                let value = match stamp(workspace, p) {
                    Ok(value) => Some(value),
                    Err(_) => {
                        available = false;
                        None
                    }
                };
                (p.clone(), value)
            })
            .collect();
        Self {
            destination: root
                .join("agent-artifacts")
                .join(task_id)
                .join("progress.json"),
            workspace: workspace.into(),
            files,
            last_activity_ms: 0,
            sampled_at: None,
            published_at: None,
            snapshot: AgentProgress {
                task_id: task_id.into(),
                phase: phase.into(),
                process_id: 0,
                started_at: 0,
                observed_at: 0,
                elapsed_ms: 0,
                hard_limit_ms: hard_ms,
                idle_limit_ms: idle_ms,
                last_activity_at: None,
                idle_ms: 0,
                stdout_bytes: 0,
                stderr_bytes: 0,
                changed_files: vec![],
                file_observation_available: available,
                finished: false,
                stop_reason: None,
                stale: false,
                limit_enforced,
                overtime: false,
                overtime_ms: 0,
            },
        }
    }

    pub(crate) fn observe(&mut self, observation: ProcessObservation) -> Result<ProcessControl> {
        self.snapshot.process_id = observation.pid;
        self.snapshot.started_at = observation.started_at;
        self.snapshot.elapsed_ms = observation.elapsed_ms;
        if observation.elapsed_ms >= self.snapshot.hard_limit_ms {
            self.snapshot.overtime = true;
            self.snapshot.overtime_ms = observation
                .elapsed_ms
                .saturating_sub(self.snapshot.hard_limit_ms);
        }
        let mut activity = observation.stdout_bytes != self.snapshot.stdout_bytes
            || observation.stderr_bytes != self.snapshot.stderr_bytes;
        self.snapshot.stdout_bytes = observation.stdout_bytes;
        self.snapshot.stderr_bytes = observation.stderr_bytes;
        if self
            .sampled_at
            .is_none_or(|at| observation.elapsed_ms.saturating_sub(at) >= 100)
        {
            self.sampled_at = Some(observation.elapsed_ms);
            self.snapshot.file_observation_available = true;
            for (path, previous) in &mut self.files {
                match stamp(&self.workspace, path) {
                    Ok(current) => {
                        if previous.as_ref().is_some_and(|old| old != &current) {
                            activity = true;
                            if !self.snapshot.changed_files.contains(path) {
                                self.snapshot.changed_files.push(path.clone());
                            }
                        }
                        *previous = Some(current);
                    }
                    Err(_) => {
                        self.snapshot.file_observation_available = false;
                        *previous = None;
                    }
                }
            }
        }
        if activity {
            self.last_activity_ms = observation.elapsed_ms;
            self.snapshot.last_activity_at = Some(
                observation
                    .started_at
                    .saturating_add(observation.elapsed_ms),
            );
        }
        self.snapshot.idle_ms = observation.elapsed_ms.saturating_sub(self.last_activity_ms);
        if self
            .published_at
            .is_none_or(|at| observation.elapsed_ms.saturating_sub(at) >= 250)
        {
            self.publish()?;
            self.published_at = Some(observation.elapsed_ms);
        }
        if let Some(idle) = self.snapshot.idle_limit_ms
            && self.snapshot.idle_ms >= idle
            && (!self.snapshot.limit_enforced
                || observation.elapsed_ms < self.snapshot.hard_limit_ms)
        {
            return Ok(ProcessControl::Timeout(format!(
                "no observable output or approved-file activity for {idle} ms"
            )));
        }
        Ok(ProcessControl::Continue)
    }

    pub(crate) fn finish(&mut self, result: &ProcessResult) -> Result<()> {
        // Capture writes/output flushed just before exit. This observation cannot
        // change the owned process's already-established outcome or time budget.
        self.sampled_at = None;
        self.observe(ProcessObservation {
            pid: result.identity.pid,
            started_at: result.started_at,
            elapsed_ms: result.ended_at.saturating_sub(result.started_at),
            stdout_bytes: result.stdout.total_bytes,
            stderr_bytes: result.stderr.total_bytes,
        })?;
        self.snapshot.process_id = result.identity.pid;
        self.snapshot.started_at = result.started_at;
        self.snapshot.elapsed_ms = result.ended_at.saturating_sub(result.started_at);
        if self.snapshot.elapsed_ms >= self.snapshot.hard_limit_ms {
            self.snapshot.overtime = true;
            self.snapshot.overtime_ms = self
                .snapshot
                .elapsed_ms
                .saturating_sub(self.snapshot.hard_limit_ms);
        }
        self.snapshot.idle_ms = self
            .snapshot
            .elapsed_ms
            .saturating_sub(self.last_activity_ms);
        self.snapshot.stdout_bytes = result.stdout.total_bytes;
        self.snapshot.stderr_bytes = result.stderr.total_bytes;
        self.snapshot.finished = true;
        self.snapshot.stop_reason = Some(
            match result.reason {
                StopReason::TimedOut if result.error.is_some() => "inactivity_limit",
                StopReason::TimedOut => "hard_limit",
                StopReason::Cancelled => "cancelled",
                StopReason::ObservationFailed => "observation_failed",
                StopReason::Exited if result.exit_code == Some(0) => "succeeded",
                StopReason::Exited => "failed",
            }
            .into(),
        );
        self.publish()
    }

    fn publish(&mut self) -> Result<()> {
        self.snapshot.observed_at = now();
        let phase_path = self
            .destination
            .with_file_name(format!("progress-{}.json", self.snapshot.phase));
        let phase_temporary = phase_path.with_extension("json.part");
        let bytes = serde_json::to_vec(&self.snapshot)?;
        fs::write(&phase_temporary, &bytes)?;
        fs::rename(phase_temporary, phase_path)?;
        let temporary = self.destination.with_extension("json.part");
        fs::write(&temporary, bytes)?;
        fs::rename(temporary, &self.destination)?;
        Ok(())
    }
}

pub(crate) fn read(root: &Path, task_id: &str, allowed_files: &[String]) -> Option<AgentProgress> {
    let file = fs::File::open(
        root.join("agent-artifacts")
            .join(task_id)
            .join("progress.json"),
    )
    .ok()?;
    let mut bytes = vec![];
    file.take(32_769).read_to_end(&mut bytes).ok()?;
    if bytes.len() > 32_768 {
        return None;
    }
    let mut progress: AgentProgress = serde_json::from_slice(&bytes).ok()?;
    if progress.task_id != task_id
        || !matches!(progress.phase.as_str(), "generation" | "verification")
        || progress.changed_files.len() > 16
        || progress
            .changed_files
            .iter()
            .any(|p| !allowed_files.contains(p))
    {
        return None;
    }
    progress.stale = !progress.finished && now().saturating_sub(progress.observed_at) > 5_000;
    Some(progress)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation(elapsed_ms: u64) -> ProcessObservation {
        ProcessObservation {
            pid: 123,
            started_at: 1_000,
            elapsed_ms,
            stdout_bytes: 0,
            stderr_bytes: 0,
        }
    }

    #[test]
    fn silent_file_write_renews_only_idle_window_and_stale_snapshots_are_labelled() {
        let tmp = tempfile::tempdir().unwrap();
        // Production canonicalizes every root at entry; CI's Windows TEMP is an 8.3 short
        // path (RUNNER~1), so use the canonical form here too or path identity checks fail.
        let dir_root = crate::repository::canonical(tmp.path()).unwrap();
        let workspace = dir_root.as_path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(workspace.join("result.txt"), "before").unwrap();
        fs::create_dir_all(dir_root.as_path().join("agent-artifacts/task")).unwrap();
        let files = vec!["result.txt".into()];
        let mut monitor = Monitor::new(
            dir_root.as_path(),
            "task",
            "generation",
            &workspace,
            &files,
            2_000,
            Some(500),
        );
        assert!(matches!(
            monitor.observe(observation(0)).unwrap(),
            ProcessControl::Continue
        ));
        fs::write(workspace.join("result.txt"), "after the edit").unwrap();
        assert!(matches!(
            monitor.observe(observation(400)).unwrap(),
            ProcessControl::Continue
        ));
        assert!(matches!(
            monitor.observe(observation(800)).unwrap(),
            ProcessControl::Continue
        ));
        assert!(matches!(
            monitor.observe(observation(950)).unwrap(),
            ProcessControl::Timeout(_)
        ));
        let snapshot = read(dir_root.as_path(), "task", &files).unwrap();
        assert_eq!(snapshot.hard_limit_ms, 2_000);
        assert_eq!(snapshot.last_activity_at, Some(1_400));
        assert_eq!(snapshot.stdout_bytes, 0);
        assert_eq!(snapshot.changed_files, files);
        assert!(!snapshot.stale);
        let mut delayed = snapshot;
        delayed.observed_at = now().saturating_sub(6_000);
        fs::write(&monitor.destination, serde_json::to_vec(&delayed).unwrap()).unwrap();
        assert!(read(dir_root.as_path(), "task", &files).unwrap().stale);
        delayed.task_id = "someone-else".into();
        fs::write(&monitor.destination, serde_json::to_vec(&delayed).unwrap()).unwrap();
        assert!(read(dir_root.as_path(), "task", &files).is_none());
    }

    #[test]
    fn unavailable_file_observation_does_not_invent_activity_or_renew_a_budget() {
        let tmp = tempfile::tempdir().unwrap();
        // Production canonicalizes every root at entry; CI's Windows TEMP is an 8.3 short
        // path (RUNNER~1), so use the canonical form here too or path identity checks fail.
        let dir_root = crate::repository::canonical(tmp.path()).unwrap();
        let workspace = dir_root.as_path().join("workspace");
        fs::create_dir_all(workspace.join("result.txt")).unwrap();
        fs::create_dir_all(dir_root.as_path().join("agent-artifacts/task")).unwrap();
        let files = vec!["result.txt".into()];
        let mut monitor = Monitor::new(
            dir_root.as_path(),
            "task",
            "generation",
            &workspace,
            &files,
            2_000,
            Some(500),
        );
        monitor.observe(observation(0)).unwrap();
        assert!(matches!(
            monitor.observe(observation(600)).unwrap(),
            ProcessControl::Timeout(_)
        ));
        let snapshot = read(dir_root.as_path(), "task", &files).unwrap();
        assert!(!snapshot.file_observation_available);
        assert!(snapshot.last_activity_at.is_none());
        assert!(snapshot.changed_files.is_empty());
        fs::remove_dir(workspace.join("result.txt")).unwrap();
        fs::write(workspace.join("result.txt"), "recovered observation").unwrap();
        monitor.observe(observation(800)).unwrap();
        assert!(monitor.snapshot.file_observation_available);
        assert!(monitor.snapshot.last_activity_at.is_none());
        assert!(monitor.snapshot.changed_files.is_empty());
        fs::remove_file(workspace.join("result.txt")).unwrap();
        fs::create_dir(workspace.join("result.txt")).unwrap();
        monitor.observe(observation(1_000)).unwrap();
        assert!(!monitor.snapshot.file_observation_available);
        fs::remove_dir(workspace.join("result.txt")).unwrap();
        fs::write(workspace.join("result.txt"), "recovered again").unwrap();
        monitor.observe(observation(1_200)).unwrap();
        assert!(monitor.snapshot.file_observation_available);
        assert!(monitor.snapshot.last_activity_at.is_none());
        assert!(monitor.snapshot.changed_files.is_empty());
        fs::write(&monitor.destination, vec![b' '; 32_769]).unwrap();
        assert!(read(dir_root.as_path(), "task", &files).is_none());
    }
}
