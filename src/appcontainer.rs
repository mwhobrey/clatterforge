//! Windows AppContainer verifier launch (ADR 0017 slice 2, #33).
//!
//! The sandboxed verifier runs through `clatterforge appcontainer-exec`, an ordinary child
//! of the existing supervisor, so the Job Object, output capture, timeouts and cancellation
//! stay exactly as they are. The trampoline:
//!
//! 1. creates a fresh AppContainer profile for this run (a new, never-reused SID);
//! 2. grants that SID read-write on the worktree and read/execute on the verifier's folder
//!    and any listed read-only paths;
//! 3. launches the verifier in the container (internetClient capability only when the level
//!    allows network), inheriting only its three standard handles, suspended inside a nested
//!    job with memory, process and CPU limits, then resumes it;
//! 4. waits, revokes the grants, deletes the profile and exits with the verifier's code.
//!
//! A crash before step 4 can leave ACL entries naming the deleted SID. They grant nothing to
//! any later run, because every run uses a new SID.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// What the trampoline needs, passed as one JSON argument so the verification receipt
/// records it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LaunchSpec {
    pub workspace: PathBuf,
    pub network: bool,
    #[serde(default)]
    pub read_only_paths: Vec<PathBuf>,
    #[serde(default)]
    pub env_passthrough: Vec<String>,
    pub memory_mb: u32,
    pub cpus: u32,
    pub pids: u32,
}

/// Quotes one argument the way `CommandLineToArgvW` reads it back.
pub fn quote_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '\n', '\u{0b}', '"']) {
        return arg.to_string();
    }
    let mut quoted = String::from('"');
    let mut slashes = 0usize;
    for ch in arg.chars() {
        match ch {
            '\\' => slashes += 1,
            '"' => {
                quoted.push_str(&"\\".repeat(slashes * 2 + 1));
                quoted.push('"');
                slashes = 0;
            }
            other => {
                quoted.push_str(&"\\".repeat(slashes));
                quoted.push(other);
                slashes = 0;
            }
        }
    }
    quoted.push_str(&"\\".repeat(slashes * 2));
    quoted.push('"');
    quoted
}

/// Runs `command` (absolute executable first) in a fresh AppContainer and returns its exit
/// code. Windows only.
#[cfg(not(windows))]
pub fn run(_spec: &LaunchSpec, _command: &[String]) -> anyhow::Result<i32> {
    anyhow::bail!("the AppContainer sandbox needs Windows")
}

#[cfg(windows)]
pub use windows_impl::run;

#[cfg(windows)]
mod windows_impl {
    use super::{LaunchSpec, quote_arg};
    use anyhow::{Context, Result, bail, ensure};
    use std::{
        ffi::{OsStr, c_void},
        os::windows::ffi::OsStrExt,
        path::Path,
        ptr,
    };
    use windows_sys::Win32::{
        Foundation::{
            CloseHandle, GetLastError, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
            LocalFree, SetHandleInformation, WAIT_OBJECT_0,
        },
        Security::{
            ACL,
            Authorization::{
                ConvertSidToStringSidW, EXPLICIT_ACCESS_W, GRANT_ACCESS, GetNamedSecurityInfoW,
                NO_MULTIPLE_TRUSTEE, REVOKE_ACCESS, SE_FILE_OBJECT, SetEntriesInAclW,
                SetNamedSecurityInfoW, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
            },
            CreateWellKnownSid, DACL_SECURITY_INFORMATION, FreeSid,
            Isolation::{
                CreateAppContainerProfile, DeleteAppContainerProfile, GetAppContainerFolderPath,
            },
            NO_INHERITANCE, PSECURITY_DESCRIPTOR, PSID, SECURITY_CAPABILITIES, SID_AND_ATTRIBUTES,
            SUB_CONTAINERS_AND_OBJECTS_INHERIT, WinCapabilityInternetClientSid,
        },
        System::{
            Com::CoTaskMemFree,
            Console::{GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE},
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_CPU_RATE_CONTROL_ENABLE,
                JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
                JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                JOBOBJECT_CPU_RATE_CONTROL_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JobObjectCpuRateControlInformation, JobObjectExtendedLimitInformation,
                SetInformationJobObject,
            },
            Threading::{
                CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
                DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess,
                INFINITE, InitializeProcThreadAttributeList, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
                PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, PROCESS_INFORMATION, ResumeThread,
                STARTF_USESTDHANDLES, STARTUPINFOEXW, UpdateProcThreadAttribute,
                WaitForSingleObject,
            },
        },
    };

    const FILE_ALL_ACCESS: u32 = 0x001F_01FF;
    const FILE_READ_EXECUTE: u32 = 0x0012_00A9;
    const SE_GROUP_ENABLED: u32 = 0x0000_0004;

    fn wide(value: impl AsRef<OsStr>) -> Vec<u16> {
        value.as_ref().encode_wide().chain(Some(0)).collect()
    }

    /// One AppContainer profile, deleted (with its SID freed) on drop.
    struct Profile {
        name: Vec<u16>,
        sid: PSID,
    }

    impl Profile {
        fn create() -> Result<Self> {
            let name = format!("Clatterforge.Verifier.{}", uuid::Uuid::new_v4().simple());
            let name_w = wide(&name);
            let display = wide("Clatterforge sandboxed verifier");
            let mut sid: PSID = ptr::null_mut();
            let hr = unsafe {
                CreateAppContainerProfile(
                    name_w.as_ptr(),
                    display.as_ptr(),
                    display.as_ptr(),
                    ptr::null(),
                    0,
                    &mut sid,
                )
            };
            ensure!(
                hr >= 0 && !sid.is_null(),
                "CreateAppContainerProfile failed: 0x{hr:08x}"
            );
            Ok(Self { name: name_w, sid })
        }

        fn sid_string(&self) -> Result<Vec<u16>> {
            let mut text: *mut u16 = ptr::null_mut();
            ensure!(
                unsafe { ConvertSidToStringSidW(self.sid, &mut text) } != 0,
                "ConvertSidToStringSidW failed: {}",
                unsafe { GetLastError() }
            );
            let mut len = 0;
            while unsafe { *text.add(len) } != 0 {
                len += 1;
            }
            let mut out: Vec<u16> = unsafe { std::slice::from_raw_parts(text, len) }.to_vec();
            out.push(0);
            unsafe { LocalFree(text as *mut c_void) };
            Ok(out)
        }

        /// The profile's private folder; its `Temp` is the only scratch space it can write.
        fn temp_dir(&self) -> Result<std::path::PathBuf> {
            let sid = self.sid_string()?;
            let mut path: *mut u16 = ptr::null_mut();
            let hr = unsafe { GetAppContainerFolderPath(sid.as_ptr(), &mut path) };
            ensure!(
                hr >= 0 && !path.is_null(),
                "GetAppContainerFolderPath failed: 0x{hr:08x}"
            );
            let mut len = 0;
            while unsafe { *path.add(len) } != 0 {
                len += 1;
            }
            let folder = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(path, len) });
            unsafe { CoTaskMemFree(path as *const c_void) };
            let temp = Path::new(&folder).join("Temp");
            std::fs::create_dir_all(&temp)?;
            Ok(temp)
        }
    }

    impl Drop for Profile {
        fn drop(&mut self) {
            unsafe {
                FreeSid(self.sid);
                DeleteAppContainerProfile(self.name.as_ptr());
            }
        }
    }

    /// Adds (`GRANT_ACCESS`) or removes (`REVOKE_ACCESS`) the profile SID's entry on `path`;
    /// for a directory, the entry is inherited by everything below it.
    fn set_access(path: &Path, sid: PSID, access: u32, mode: i32) -> Result<()> {
        let path_w = wide(path);
        let mut old_dacl: *mut ACL = ptr::null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
        let status = unsafe {
            GetNamedSecurityInfoW(
                path_w.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                &mut old_dacl,
                ptr::null_mut(),
                &mut descriptor,
            )
        };
        ensure!(
            status == 0,
            "reading the ACL of {} failed: {status}",
            path.display()
        );
        let inheritance = if path.is_dir() {
            SUB_CONTAINERS_AND_OBJECTS_INHERIT
        } else {
            NO_INHERITANCE
        };
        let entry = EXPLICIT_ACCESS_W {
            grfAccessPermissions: access,
            grfAccessMode: mode,
            grfInheritance: inheritance,
            Trustee: TRUSTEE_W {
                pMultipleTrustee: ptr::null_mut(),
                MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_UNKNOWN,
                ptstrName: sid as *mut u16,
            },
        };
        let mut new_dacl: *mut ACL = ptr::null_mut();
        let status = unsafe { SetEntriesInAclW(1, &entry, old_dacl, &mut new_dacl) };
        unsafe { LocalFree(descriptor) };
        ensure!(
            status == 0,
            "building the ACL for {} failed: {status}",
            path.display()
        );
        let status = unsafe {
            SetNamedSecurityInfoW(
                path_w.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                new_dacl,
                ptr::null(),
            )
        };
        unsafe { LocalFree(new_dacl as *mut c_void) };
        ensure!(
            status == 0,
            "writing the ACL of {} failed: {status}",
            path.display()
        );
        Ok(())
    }

    /// Grants recorded so they are always revoked, in reverse order, on drop.
    struct Grants {
        sid: PSID,
        paths: Vec<std::path::PathBuf>,
    }

    impl Grants {
        fn grant(&mut self, path: &Path, access: u32) -> Result<()> {
            set_access(path, self.sid, access, GRANT_ACCESS)?;
            self.paths.push(path.to_path_buf());
            Ok(())
        }
    }

    impl Drop for Grants {
        fn drop(&mut self) {
            for path in self.paths.iter().rev() {
                let _ = set_access(path, self.sid, 0, REVOKE_ACCESS);
            }
        }
    }

    struct OwnedHandle(HANDLE);
    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
                unsafe { CloseHandle(self.0) };
            }
        }
    }

    fn limited_job(spec: &LaunchSpec) -> Result<OwnedHandle> {
        let job = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        ensure!(!job.is_null(), "CreateJobObjectW failed: {}", unsafe {
            GetLastError()
        });
        let job = OwnedHandle(job);
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            | JOB_OBJECT_LIMIT_JOB_MEMORY
            | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
        limits.BasicLimitInformation.ActiveProcessLimit = spec.pids;
        limits.JobMemoryLimit = spec.memory_mb as usize * 1024 * 1024;
        ensure!(
            unsafe {
                SetInformationJobObject(
                    job.0,
                    JobObjectExtendedLimitInformation,
                    &limits as *const _ as *const c_void,
                    std::mem::size_of_val(&limits) as u32,
                )
            } != 0,
            "setting job limits failed: {}",
            unsafe { GetLastError() }
        );
        let logical = std::thread::available_parallelism().map_or(1, |n| n.get()) as u32;
        let percent = (spec.cpus.min(logical) * 100 / logical.max(1)).clamp(1, 100);
        let mut rate: JOBOBJECT_CPU_RATE_CONTROL_INFORMATION = unsafe { std::mem::zeroed() };
        rate.ControlFlags =
            JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP;
        rate.Anonymous.CpuRate = percent * 100;
        ensure!(
            unsafe {
                SetInformationJobObject(
                    job.0,
                    JobObjectCpuRateControlInformation,
                    &rate as *const _ as *const c_void,
                    std::mem::size_of_val(&rate) as u32,
                )
            } != 0,
            "setting the CPU cap failed: {}",
            unsafe { GetLastError() }
        );
        Ok(job)
    }

    /// The child's environment: the minimal allowlist, scratch space in the profile folder,
    /// and the listed passthrough names. Nothing else from the host.
    fn environment(spec: &LaunchSpec, temp: &Path) -> Vec<u16> {
        let mut vars: Vec<(String, std::ffi::OsString)> = Vec::new();
        // The profile folder variables are paths, not secrets; Windows needs them in the block
        // to redirect an AppContainer's app-data folders to its own profile (error 203 without).
        for name in [
            "SystemRoot",
            "SystemDrive",
            "WINDIR",
            "COMSPEC",
            "PATH",
            "PATHEXT",
            "USERPROFILE",
            "LOCALAPPDATA",
            "APPDATA",
        ]
        .iter()
        .map(|s| s.to_string())
        .chain(spec.env_passthrough.iter().cloned())
        {
            if let Some(value) = std::env::var_os(&name) {
                vars.push((name, value));
            }
        }
        for name in ["TEMP", "TMP"] {
            vars.push((name.to_string(), temp.as_os_str().to_owned()));
        }
        vars.sort_by_key(|(name, _)| name.to_uppercase());
        vars.dedup_by(|a, b| a.0.eq_ignore_ascii_case(&b.0));
        let mut block = Vec::new();
        for (name, value) in vars {
            block.extend(OsStr::new(&name).encode_wide());
            block.push(u16::from(b'='));
            block.extend(value.encode_wide());
            block.push(0);
        }
        block.push(0);
        block
    }

    /// The verifier's own folder needs read/execute in the container, except where Windows
    /// already grants `ALL APPLICATION PACKAGES` (the Windows and Program Files trees), which
    /// this never edits. A folder that would expose far more than one tool (a drive root,
    /// the profile root or anything above it) is refused.
    fn verifier_folder_grant(program: &Path) -> Result<Option<std::path::PathBuf>> {
        let folder = program.parent().context("verifier has no folder")?;
        let folder = crate::repository::canonical(folder).context("verifier folder is missing")?;
        // Same plain canonical form as `folder`, so the comparisons below cannot miss.
        let canonical = |var: &str| {
            std::env::var_os(var)
                .and_then(|v| crate::repository::canonical(std::path::Path::new(&v)).ok())
        };
        for system in [
            "SystemRoot",
            "ProgramFiles",
            "ProgramFiles(x86)",
            "ProgramW6432",
        ] {
            if let Some(root) = canonical(system)
                && folder.starts_with(&root)
            {
                return Ok(None);
            }
        }
        ensure!(
            folder.parent().is_some(),
            "refusing to grant a drive root to the sandboxed verifier"
        );
        if let Some(profile) = canonical("USERPROFILE") {
            ensure!(
                !profile.starts_with(&folder),
                "refusing to grant {} (your profile or above) to the sandboxed verifier; \
                 move the verifier into its own folder",
                folder.display()
            );
        }
        Ok(Some(folder))
    }

    pub fn run(spec: &LaunchSpec, command: &[String]) -> Result<i32> {
        let program = Path::new(command.first().context("no verifier command")?);
        ensure!(
            program.is_absolute(),
            "the verifier executable must be an absolute path"
        );
        // Plain (non-`\\?\`) form: `cmd.exe` and other tools reject verbatim paths as UNC.
        let workspace =
            crate::repository::canonical(&spec.workspace).context("worktree is missing")?;

        let profile = Profile::create()?;
        let temp = profile.temp_dir()?;
        let mut grants = Grants {
            sid: profile.sid,
            paths: Vec::new(),
        };
        grants.grant(&workspace, FILE_ALL_ACCESS)?;
        if let Some(folder) = verifier_folder_grant(program)? {
            grants.grant(&folder, FILE_READ_EXECUTE)?;
        }
        for path in &spec.read_only_paths {
            grants.grant(path, FILE_READ_EXECUTE)?;
        }

        let mut internet: Vec<u8> = vec![0; 68];
        let mut capabilities: Vec<SID_AND_ATTRIBUTES> = Vec::new();
        if spec.network {
            let mut size = internet.len() as u32;
            ensure!(
                unsafe {
                    CreateWellKnownSid(
                        WinCapabilityInternetClientSid,
                        ptr::null_mut(),
                        internet.as_mut_ptr() as PSID,
                        &mut size,
                    )
                } != 0,
                "CreateWellKnownSid failed: {}",
                unsafe { GetLastError() }
            );
            capabilities.push(SID_AND_ATTRIBUTES {
                Sid: internet.as_mut_ptr() as PSID,
                Attributes: SE_GROUP_ENABLED,
            });
        }
        let security = SECURITY_CAPABILITIES {
            AppContainerSid: profile.sid,
            Capabilities: if capabilities.is_empty() {
                ptr::null_mut()
            } else {
                capabilities.as_mut_ptr()
            },
            CapabilityCount: capabilities.len() as u32,
            Reserved: 0,
        };

        // Only the three standard handles are inherited, and only if they exist.
        let mut handles: Vec<HANDLE> = Vec::new();
        for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            let handle = unsafe { GetStdHandle(which) };
            if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
                unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) };
                if !handles.contains(&handle) {
                    handles.push(handle);
                }
            }
        }

        let mut size = 0usize;
        unsafe { InitializeProcThreadAttributeList(ptr::null_mut(), 2, 0, &mut size) };
        let mut attribute_buffer = vec![0u8; size];
        let attributes = attribute_buffer.as_mut_ptr() as *mut c_void;
        ensure!(
            unsafe { InitializeProcThreadAttributeList(attributes, 2, 0, &mut size) } != 0,
            "InitializeProcThreadAttributeList failed: {}",
            unsafe { GetLastError() }
        );
        let outcome = (|| -> Result<i32> {
            ensure!(
                unsafe {
                    UpdateProcThreadAttribute(
                        attributes,
                        0,
                        PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
                        &security as *const _ as *const c_void,
                        std::mem::size_of_val(&security),
                        ptr::null_mut(),
                        ptr::null(),
                    )
                } != 0,
                "setting AppContainer capabilities failed: {}",
                unsafe { GetLastError() }
            );
            if !handles.is_empty() {
                ensure!(
                    unsafe {
                        UpdateProcThreadAttribute(
                            attributes,
                            0,
                            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                            handles.as_ptr() as *const c_void,
                            std::mem::size_of_val(handles.as_slice()),
                            ptr::null_mut(),
                            ptr::null(),
                        )
                    } != 0,
                    "restricting inherited handles failed: {}",
                    unsafe { GetLastError() }
                );
            }
            let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
            startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
            startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
            startup.StartupInfo.hStdInput = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
            startup.StartupInfo.hStdOutput = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
            startup.StartupInfo.hStdError = unsafe { GetStdHandle(STD_ERROR_HANDLE) };
            startup.lpAttributeList = attributes;

            let mut line: Vec<u16> = wide(
                command
                    .iter()
                    .map(|a| quote_arg(a))
                    .collect::<Vec<_>>()
                    .join(" "),
            );
            let program_w = wide(program);
            let directory = wide(&workspace);
            let env = environment(spec, &temp);
            let job = limited_job(spec)?;
            let mut info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
            let created = unsafe {
                CreateProcessW(
                    program_w.as_ptr(),
                    line.as_mut_ptr(),
                    ptr::null(),
                    ptr::null(),
                    1,
                    EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | CREATE_SUSPENDED,
                    env.as_ptr() as *const c_void,
                    directory.as_ptr(),
                    &startup.StartupInfo,
                    &mut info,
                )
            };
            if created == 0 {
                bail!(
                    "starting the verifier in an AppContainer failed: {}",
                    std::io::Error::last_os_error()
                );
            }
            let process = OwnedHandle(info.hProcess);
            let thread = OwnedHandle(info.hThread);
            if unsafe { AssignProcessToJobObject(job.0, process.0) } == 0 {
                let error = std::io::Error::last_os_error();
                unsafe { windows_sys::Win32::System::Threading::TerminateProcess(process.0, 1) };
                bail!("placing the verifier in its limit job failed: {error}");
            }
            unsafe { ResumeThread(thread.0) };
            ensure!(
                unsafe { WaitForSingleObject(process.0, INFINITE) } == WAIT_OBJECT_0,
                "waiting for the verifier failed: {}",
                std::io::Error::last_os_error()
            );
            let mut code = 1u32;
            unsafe { GetExitCodeProcess(process.0, &mut code) };
            // Dropping the job kills anything the verifier left running.
            drop(job);
            Ok(code as i32)
        })();
        unsafe { DeleteProcThreadAttributeList(attributes) };
        drop(grants);
        drop(profile);
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::quote_arg;

    #[test]
    fn arguments_round_trip_through_windows_quoting() {
        assert_eq!(quote_arg("plain"), "plain");
        assert_eq!(quote_arg(""), "\"\"");
        assert_eq!(quote_arg("two words"), "\"two words\"");
        assert_eq!(quote_arg(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(quote_arg(r"C:\dir with space\"), r#""C:\dir with space\\""#);
        assert_eq!(quote_arg(r"a\\b c"), r#""a\\b c""#);
    }
}
