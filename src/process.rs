use crate::{digest, now, repository::canonical};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub created: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    Alive,
    Exited,
    Missing,
    Mismatch,
    Unverifiable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionPolicy {
    pub executable: PathBuf,
    pub executable_hash: String,
    pub timeout_ms: u64,
    pub verification_timeout_ms: u64,
    pub log_bytes: usize,
}

impl ExecutionPolicy {
    pub fn new(executable: &Path) -> Result<Self> {
        let executable = canonical(executable)?;
        Ok(Self {
            executable_hash: digest(&fs::read(&executable)?),
            executable,
            timeout_ms: 5000,
            verification_timeout_ms: 5000,
            log_bytes: 32768,
        })
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (100..=3600000).contains(&self.timeout_ms)
                && (100..=3600000).contains(&self.verification_timeout_ms),
            "process timeouts must be 100..3600000 milliseconds"
        );
        ensure!(
            (256..=262144).contains(&self.log_bytes),
            "per-stream log cap must be 256..262144 bytes"
        );
        ensure!(
            digest(&fs::read(&self.executable)?) == self.executable_hash,
            "approved executable changed"
        );
        Ok(())
    }
}

#[derive(Debug)]
pub struct Capture {
    pub bytes: Vec<u8>,
    pub total_bytes: u64,
}

fn drain(
    mut input: impl Read + Send + 'static,
    cap: usize,
    disk_cap: usize,
    path: Option<PathBuf>,
    observed_bytes: Arc<AtomicU64>,
) -> Result<Capture> {
    let mut file = path.map(File::create).transpose()?;
    let mut bytes = Vec::with_capacity(cap);
    let mut total_bytes = 0;
    let mut chunk = [0; 8192];
    loop {
        let count = input.read(&mut chunk)?;
        if count == 0 {
            break;
        }
        total_bytes += count as u64;
        observed_bytes.store(total_bytes, Ordering::Relaxed);
        let keep = count.min(cap.saturating_sub(bytes.len()));
        bytes.extend_from_slice(&chunk[..keep]);
        if let Some(file) = &mut file {
            let disk_keep =
                count.min(disk_cap.saturating_sub((total_bytes - count as u64) as usize));
            file.write_all(&chunk[..disk_keep])?;
            file.flush()?;
        }
    }
    if let Some(file) = &mut file {
        file.sync_all()?;
    }
    Ok(Capture { bytes, total_bytes })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    Exited,
    Cancelled,
    TimedOut,
    ObservationFailed,
}

pub struct ProcessResult {
    pub identity: ProcessIdentity,
    pub started_at: u64,
    pub ended_at: u64,
    pub exit_code: Option<i32>,
    pub reason: StopReason,
    pub error: Option<String>,
    pub stdout: Capture,
    pub stderr: Capture,
    pub tree_terminated: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct ProcessObservation {
    pub pid: u32,
    pub started_at: u64,
    pub elapsed_ms: u64,
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
}

pub enum ProcessControl {
    Continue,
    Cancel,
    Timeout(String),
}

pub struct OwnedProcess {
    child: Child,
    job: platform::Job,
    stdout: Option<JoinHandle<Result<Capture>>>,
    stderr: Option<JoinHandle<Result<Capture>>>,
    pub identity: ProcessIdentity,
    started_at: u64,
    started: Instant,
    stdout_bytes: Arc<AtomicU64>,
    stderr_bytes: Arc<AtomicU64>,
}

impl OwnedProcess {
    pub fn spawn(command: &mut Command, cap: usize, logs: Option<(&Path, &Path)>) -> Result<Self> {
        Self::spawn_with_retention(command, cap, cap, logs)
    }

    pub(crate) fn spawn_streamed(
        command: &mut Command,
        disk_cap: usize,
        logs: (&Path, &Path),
    ) -> Result<Self> {
        Self::spawn_with_retention(command, 0, disk_cap, Some(logs))
    }

    fn spawn_with_retention(
        command: &mut Command,
        cap: usize,
        disk_cap: usize,
        logs: Option<(&Path, &Path)>,
    ) -> Result<Self> {
        let job = platform::Job::new()?;
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        platform::hidden(command);
        let started_at = now();
        let started = Instant::now();
        let mut child = command.spawn().context("could not spawn owned process")?;
        if let Err(error) = job.assign(&child) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        let identity = platform::child_identity(&child)?;
        let stdout = child.stdout.take().context("missing stdout")?;
        let stderr = child.stderr.take().context("missing stderr")?;
        let out_path = logs.map(|(a, _)| a.to_path_buf());
        let err_path = logs.map(|(_, b)| b.to_path_buf());
        let stdout_bytes = Arc::new(AtomicU64::new(0));
        let stderr_bytes = Arc::new(AtomicU64::new(0));
        let out_counter = stdout_bytes.clone();
        let err_counter = stderr_bytes.clone();
        Ok(Self {
            child,
            job,
            identity,
            started_at,
            started,
            stdout: Some(thread::spawn(move || {
                drain(stdout, cap, disk_cap, out_path, out_counter)
            })),
            stderr: Some(thread::spawn(move || {
                drain(stderr, cap, disk_cap, err_path, err_counter)
            })),
            stdout_bytes,
            stderr_bytes,
        })
    }

    pub fn wait(
        self,
        timeout_ms: u64,
        mut cancelled: impl FnMut() -> Result<bool>,
    ) -> Result<ProcessResult> {
        self.wait_observed(timeout_ms, |_| {
            Ok(if cancelled()? {
                ProcessControl::Cancel
            } else {
                ProcessControl::Continue
            })
        })
    }

    pub fn wait_observed(
        self,
        timeout_ms: u64,
        observe: impl FnMut(ProcessObservation) -> Result<ProcessControl>,
    ) -> Result<ProcessResult> {
        self.wait_observed_optional(Some(timeout_ms), observe)
    }

    pub fn wait_observed_optional(
        mut self,
        timeout_ms: Option<u64>,
        mut observe: impl FnMut(ProcessObservation) -> Result<ProcessControl>,
    ) -> Result<ProcessResult> {
        let mut error = None;
        let reason = loop {
            if self.child.try_wait()?.is_some() {
                break StopReason::Exited;
            }
            match observe(ProcessObservation {
                pid: self.identity.pid,
                started_at: self.started_at,
                elapsed_ms: self.started.elapsed().as_millis() as u64,
                stdout_bytes: self.stdout_bytes.load(Ordering::Relaxed),
                stderr_bytes: self.stderr_bytes.load(Ordering::Relaxed),
            }) {
                Ok(ProcessControl::Cancel) => break StopReason::Cancelled,
                Ok(ProcessControl::Continue) => (),
                Ok(ProcessControl::Timeout(message)) => {
                    error = Some(message);
                    break StopReason::TimedOut;
                }
                Err(failure) => {
                    error = Some(failure.to_string());
                    break StopReason::ObservationFailed;
                }
            }
            if timeout_ms.is_some_and(|ms| self.started.elapsed() >= Duration::from_millis(ms)) {
                break StopReason::TimedOut;
            }
            thread::sleep(Duration::from_millis(10));
        };
        // The owned job, not a reconstructed PID, terminates remaining descendants.
        self.job.terminate()?;
        let stopped = Instant::now();
        // Reap the direct child as it dies: on Unix an unreaped leader stays in its group.
        while {
            let _ = self.child.try_wait();
            self.job.active()? != 0
        } && stopped.elapsed() < Duration::from_secs(2)
        {
            thread::sleep(Duration::from_millis(10));
        }
        let tree_terminated = self.job.active()? == 0;
        ensure!(
            tree_terminated,
            "owned process tree termination could not be verified"
        );
        let exit_code = self.child.wait()?.code();
        let stdout = self
            .stdout
            .take()
            .unwrap()
            .join()
            .map_err(|_| anyhow::anyhow!("stdout collector panicked"))??;
        let stderr = self
            .stderr
            .take()
            .unwrap()
            .join()
            .map_err(|_| anyhow::anyhow!("stderr collector panicked"))??;
        Ok(ProcessResult {
            identity: self.identity.clone(),
            started_at: self.started_at,
            ended_at: now(),
            exit_code,
            reason,
            error,
            stdout,
            stderr,
            tree_terminated,
        })
    }
}

impl Drop for OwnedProcess {
    fn drop(&mut self) {
        let _ = self.job.terminate();
    }
}

pub fn current_identity() -> Result<ProcessIdentity> {
    platform::current_identity()
}
pub fn child_identity(child: &Child) -> Result<ProcessIdentity> {
    platform::child_identity(child)
}
pub fn presence(identity: &ProcessIdentity) -> Presence {
    platform::presence(identity)
}

/// Bound both pipe retention and runtime. Truncated command output is unusable.
pub fn bounded_output(command: &mut Command, timeout_ms: u64, cap: usize) -> Result<ProcessResult> {
    let result = OwnedProcess::spawn(command, cap, None)?.wait(timeout_ms, || Ok(false))?;
    ensure!(
        result.reason == StopReason::Exited,
        "command exceeded its observation deadline"
    );
    ensure!(
        result.stdout.total_bytes <= cap as u64 && result.stderr.total_bytes <= cap as u64,
        "command output exceeded its per-stream limit"
    );
    Ok(result)
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::{
        os::windows::{io::AsRawHandle, process::CommandExt},
        ptr,
    };
    use windows_sys::Win32::{
        Foundation::{
            CloseHandle, ERROR_INVALID_PARAMETER, FILETIME, GetLastError, HANDLE, WAIT_OBJECT_0,
            WAIT_TIMEOUT,
        },
        System::{JobObjects::*, Threading::*},
    };

    pub struct Job(HANDLE);
    impl Job {
        pub fn new() -> Result<Self> {
            let handle = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
            ensure!(
                !handle.is_null(),
                "cannot create Windows Job: {}",
                std::io::Error::last_os_error()
            );
            let job = Self(handle);
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            ensure!(
                unsafe {
                    SetInformationJobObject(
                        handle,
                        JobObjectExtendedLimitInformation,
                        &limits as *const _ as _,
                        std::mem::size_of_val(&limits) as u32,
                    )
                } != 0,
                "cannot set kill-on-close Job policy"
            );
            Ok(job)
        }
        pub fn assign(&self, child: &Child) -> Result<()> {
            ensure!(
                unsafe { AssignProcessToJobObject(self.0, child.as_raw_handle() as _) } != 0,
                "cannot assign owned process tree: {}",
                std::io::Error::last_os_error()
            );
            Ok(())
        }
        pub fn terminate(&self) -> Result<()> {
            ensure!(
                unsafe { TerminateJobObject(self.0, 1223) } != 0,
                "cannot terminate owned Job"
            );
            Ok(())
        }
        pub fn active(&self) -> Result<u32> {
            let mut accounting: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION =
                unsafe { std::mem::zeroed() };
            ensure!(
                unsafe {
                    QueryInformationJobObject(
                        self.0,
                        JobObjectBasicAccountingInformation,
                        &mut accounting as *mut _ as _,
                        std::mem::size_of_val(&accounting) as u32,
                        ptr::null_mut(),
                    )
                } != 0,
                "cannot inspect owned Job"
            );
            Ok(accounting.ActiveProcesses)
        }
    }
    impl Drop for Job {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
    fn identity(handle: HANDLE, pid: u32) -> Result<ProcessIdentity> {
        let mut creation: FILETIME = unsafe { std::mem::zeroed() };
        let mut exit = creation;
        let mut kernel = creation;
        let mut user = creation;
        ensure!(
            unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) }
                != 0,
            "cannot inspect process creation time"
        );
        Ok(ProcessIdentity {
            pid,
            created: ((creation.dwHighDateTime as u64) << 32) | creation.dwLowDateTime as u64,
        })
    }
    pub fn child_identity(child: &Child) -> Result<ProcessIdentity> {
        identity(child.as_raw_handle() as _, child.id())
    }
    pub fn current_identity() -> Result<ProcessIdentity> {
        identity(unsafe { GetCurrentProcess() }, std::process::id())
    }
    pub fn hidden(command: &mut Command) {
        command.creation_flags(CREATE_NO_WINDOW);
    }
    pub fn presence(expected: &ProcessIdentity) -> Presence {
        let handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                0,
                expected.pid,
            )
        };
        if handle.is_null() {
            return if unsafe { GetLastError() } == ERROR_INVALID_PARAMETER {
                Presence::Missing
            } else {
                Presence::Unverifiable
            };
        }
        let result = match identity(handle, expected.pid) {
            Ok(actual) if actual != *expected => Presence::Mismatch,
            Ok(_) => match unsafe { WaitForSingleObject(handle, 0) } {
                WAIT_OBJECT_0 => Presence::Exited,
                WAIT_TIMEOUT => Presence::Alive,
                _ => Presence::Unverifiable,
            },
            Err(_) => Presence::Unverifiable,
        };
        unsafe {
            CloseHandle(handle);
        }
        result
    }
}

#[cfg(unix)]
mod platform {
    use super::*;
    use std::{
        io::PipeWriter,
        os::unix::process::CommandExt,
        sync::{
            Mutex,
            atomic::{AtomicI32, Ordering},
        },
    };

    /// Kills the group when its pipe closes without the disarm line, which is what happens
    /// when the supervisor dies: the Unix stand-in for a kill-on-close Windows Job.
    const GUARD: &str =
        r#"if IFS= read -r line && [ "$line" = disarm ]; then exit 0; fi; kill -KILL "-$1""#;

    pub struct Job {
        pgid: Arc<AtomicI32>,
        guard: Mutex<Option<(Child, PipeWriter)>>,
    }

    impl Job {
        pub fn new() -> Result<Self> {
            Ok(Self {
                pgid: Arc::new(AtomicI32::new(0)),
                guard: Mutex::new(None),
            })
        }

        /// The child leads its own process group (see `hidden`); descendants inherit it.
        pub fn assign(&self, child: &Child) -> Result<()> {
            let pid = child.id() as i32;
            ensure!(pid > 0, "invalid child PID");
            self.pgid.store(pid, Ordering::SeqCst);
            let (reader, writer) = std::io::pipe().context("cannot create the job guard pipe")?;
            let guard = Command::new("/bin/sh")
                .args(["-c", GUARD, "clatterforge-job-guard", &pid.to_string()])
                .stdin(reader)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .process_group(0)
                .spawn()
                .context("cannot start the process-group guard")?;
            *self.guard.lock().unwrap_or_else(|e| e.into_inner()) = Some((guard, writer));
            Ok(())
        }

        pub fn terminate(&self) -> Result<()> {
            let pgid = self.pgid.load(Ordering::SeqCst);
            if pgid > 1 {
                unsafe {
                    let _ = libc::kill(-pgid, libc::SIGTERM);
                    let _ = libc::kill(-pgid, libc::SIGKILL);
                }
            }
            Ok(())
        }

        pub fn active(&self) -> Result<u32> {
            let pgid = self.pgid.load(Ordering::SeqCst);
            if pgid <= 1 {
                return Ok(0);
            }
            // Zombies still answer kill(0) until something reaps them, and orphaned ones wait
            // on init (PID 1 in a container may never reap). Count only live group members.
            #[cfg(target_os = "linux")]
            {
                let mut live = 0;
                for entry in std::fs::read_dir("/proc")?.flatten() {
                    let name = entry.file_name();
                    let Some(pid) = name.to_str().and_then(|n| n.parse::<u32>().ok()) else {
                        continue;
                    };
                    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
                        continue;
                    };
                    let Some(paren) = stat.rfind(')') else {
                        continue;
                    };
                    // After the command name: state, ppid, pgrp, ...
                    let mut fields = stat[paren + 1..].split_whitespace();
                    let state = fields.next();
                    let group = fields.nth(1).and_then(|g| g.parse::<i32>().ok());
                    if group == Some(pgid) && state != Some("Z") && state != Some("X") {
                        live += 1;
                    }
                }
                Ok(live)
            }
            #[cfg(not(target_os = "linux"))]
            {
                // macOS: launchd reaps orphans promptly; the leader is reaped by the caller.
                if unsafe { libc::kill(-pgid, 0) } == 0 {
                    return Ok(1);
                }
                let err = std::io::Error::last_os_error();
                Ok(u32::from(err.raw_os_error() == Some(libc::EPERM)))
            }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            let pgid = self.pgid.load(Ordering::SeqCst);
            if pgid > 1 {
                unsafe {
                    let _ = libc::kill(-pgid, libc::SIGKILL);
                }
            }
            let guard = self
                .guard
                .get_mut()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            if let Some((mut guard, mut writer)) = guard {
                let _ = writer.write_all(
                    b"disarm
",
                );
                drop(writer);
                let _ = guard.wait();
            }
        }
    }

    pub fn child_identity(child: &Child) -> Result<ProcessIdentity> {
        let pid = child.id();
        let created = inspect_created_time(pid)?;
        Ok(ProcessIdentity { pid, created })
    }

    pub fn current_identity() -> Result<ProcessIdentity> {
        let pid = std::process::id();
        let created = inspect_created_time(pid)?;
        Ok(ProcessIdentity { pid, created })
    }

    pub fn hidden(command: &mut Command) {
        command.process_group(0);
        #[cfg(target_os = "linux")]
        unsafe {
            command.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                Ok(())
            });
        }
    }

    fn inspect_created_time(pid: u32) -> Result<u64> {
        #[cfg(target_os = "linux")]
        {
            let stat_path = format!("/proc/{pid}/stat");
            if let Ok(content) = std::fs::read_to_string(&stat_path)
                && let Some(rparen) = content.rfind(')')
            {
                let rest = &content[rparen + 1..];
                let fields: Vec<&str> = rest.split_whitespace().collect();
                if let Some(starttime_str) = fields.get(19)
                    && let Ok(val) = starttime_str.parse::<u64>()
                {
                    return Ok(val);
                }
            }
            if let Ok(meta) = std::fs::metadata(&stat_path)
                && let Ok(created) = meta.created().or_else(|_| meta.modified())
                && let Ok(dur) = created.duration_since(std::time::UNIX_EPOCH)
            {
                return Ok(dur.as_millis() as u64);
            }
            Ok(now())
        }
        #[cfg(target_os = "macos")]
        {
            let mut kinfo: libc::kinfo_proc = unsafe { std::mem::zeroed() };
            let mut size = std::mem::size_of::<libc::kinfo_proc>();
            let mut mib = [
                libc::CTL_KERN,
                libc::KERN_PROC,
                libc::KERN_PROC_PID,
                pid as i32,
            ];
            let res = unsafe {
                libc::sysctl(
                    mib.as_mut_ptr(),
                    4,
                    &mut kinfo as *mut _ as *mut libc::c_void,
                    &mut size,
                    std::ptr::null_mut(),
                    0,
                )
            };
            if res == 0 && size > 0 {
                let tv = kinfo.kp_proc.p_starttime;
                return Ok((tv.tv_sec as u64) * 1_000_000 + (tv.tv_usec as u64));
            }
            Ok(now())
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            Ok(now())
        }
    }

    pub fn presence(expected: &ProcessIdentity) -> Presence {
        let ret = unsafe { libc::kill(expected.pid as i32, 0) };
        if ret == -1 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ESRCH) {
                return Presence::Missing;
            } else if err.raw_os_error() != Some(libc::EPERM) {
                return Presence::Unverifiable;
            }
        }

        if let Ok(actual_created) = inspect_created_time(expected.pid)
            && actual_created != expected.created
            && expected.created != 0
        {
            return Presence::Mismatch;
        }

        #[cfg(target_os = "linux")]
        {
            let status_path = format!("/proc/{}/status", expected.pid);
            if let Ok(content) = std::fs::read_to_string(&status_path) {
                for line in content.lines() {
                    if line.starts_with("State:") && line.contains('Z') {
                        return Presence::Exited;
                    }
                }
            }
        }
        #[cfg(target_os = "macos")]
        {
            let mut kinfo: libc::kinfo_proc = unsafe { std::mem::zeroed() };
            let mut size = std::mem::size_of::<libc::kinfo_proc>();
            let mut mib = [
                libc::CTL_KERN,
                libc::KERN_PROC,
                libc::KERN_PROC_PID,
                expected.pid as i32,
            ];
            let res = unsafe {
                libc::sysctl(
                    mib.as_mut_ptr(),
                    4,
                    &mut kinfo as *mut _ as *mut libc::c_void,
                    &mut size,
                    std::ptr::null_mut(),
                    0,
                )
            };
            if res == 0 && size > 0 && kinfo.kp_proc.p_stat == libc::SZOMB as libc::c_char {
                return Presence::Exited;
            }
        }

        Presence::Alive
    }
}

#[cfg(not(any(windows, unix)))]
mod platform {
    use super::*;
    pub struct Job;
    impl Job {
        pub fn new() -> Result<Self> {
            anyhow::bail!("unsupported host")
        }
        pub fn assign(&self, _: &Child) -> Result<()> {
            anyhow::bail!("unsupported host")
        }
        pub fn terminate(&self) -> Result<()> {
            Ok(())
        }
        pub fn active(&self) -> Result<u32> {
            Ok(0)
        }
    }
    pub fn child_identity(_: &Child) -> Result<ProcessIdentity> {
        anyhow::bail!("unsupported host")
    }
    pub fn current_identity() -> Result<ProcessIdentity> {
        anyhow::bail!("unsupported host")
    }
    pub fn hidden(_: &mut Command) {}
    pub fn presence(_: &ProcessIdentity) -> Presence {
        Presence::Unverifiable
    }
}
