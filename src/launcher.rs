use crate::{
    codex::{JsonlObserver, PinnedCommand, ProtocolReceipt},
    process::{Presence, ProcessIdentity, child_identity, presence},
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaunchSpec {
    pub workspace: PathBuf,
    pub gate: PathBuf,
    pub token: String,
    pub owner: ProcessIdentity,
    pub command: PinnedCommand,
    pub codex_home: Option<PathBuf>,
    pub input: String,
    pub receipt_path: PathBuf,
    pub parse_codex: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DelegatedReceipt {
    pub identity: ProcessIdentity,
    pub command: Vec<String>,
    pub cwd: PathBuf,
    pub exit_code: Option<i32>,
    pub protocol: Option<ProtocolReceipt>,
}

pub fn read_receipt(path: &Path) -> Result<DelegatedReceipt> {
    let mut bytes = Vec::new();
    fs::File::open(path)?.take(16385).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 16384, "delegated receipt exceeds limit");
    Ok(serde_json::from_slice(&bytes)?)
}

fn save(path: &Path, value: &DelegatedReceipt) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    let temporary = path.with_extension("writing");
    let mut file = fs::File::create(&temporary)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    fs::rename(temporary, path)?;
    Ok(())
}

pub fn run(path: &Path, token: &str) -> Result<i32> {
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(512 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 512 * 1024, "launch spec exceeds limit");
    let spec: LaunchSpec = serde_json::from_slice(&bytes)?;
    ensure!(
        spec.token == token && spec.input.len() <= 256 * 1024,
        "invalid launch token/input"
    );
    let started = Instant::now();
    loop {
        ensure!(
            presence(&spec.owner) == Presence::Alive,
            "launch owner is not alive"
        );
        if fs::read_to_string(&spec.gate).ok().as_deref() == Some(token) {
            break;
        }
        ensure!(
            started.elapsed() < Duration::from_secs(5),
            "launch gate expired"
        );
        thread::sleep(Duration::from_millis(10));
    }
    ensure!(
        std::env::current_dir()? == spec.workspace,
        "launch cwd mismatch"
    );
    spec.command.validate()?;
    let mut command = Command::new(&spec.command.executable.path);
    command
        .args(&spec.command.args)
        .current_dir(&spec.workspace)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    // Only these platform/tool discovery values reach commands; ambient credentials do not.
    // `SystemDrive` added in M10.3 after a real, live Codex finding: its own
    // `-c windows.sandbox="mxc"` mode fails outright ("MXC requires nonempty
    // Unicode host SystemDrive") without it, since this allowlist previously
    // omitted it - a platform value, not a credential, so adding it widens
    // nothing security-sensitive.
    for name in [
        "SystemRoot",
        "SystemDrive",
        "WINDIR",
        "COMSPEC",
        "PATH",
        "PATHEXT",
        "TEMP",
        "TMP",
        "USERPROFILE",
        "LOCALAPPDATA",
        "APPDATA",
        "ProgramFiles",
        "ProgramFiles(x86)",
        "ProgramW6432",
        "ProgramData",
        "HOME",
        "USER",
        "LOGNAME",
        "SHELL",
        "TMPDIR",
        "TERM",
        "LANG",
        "LC_ALL",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    if let Some(home) = &spec.codex_home {
        command.env("CODEX_HOME", home);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    // This launch happens inside the already assigned parent Job. Descendants inherit it.
    let mut child = command.spawn()?;
    let mut receipt = DelegatedReceipt {
        identity: child_identity(&child)?,
        command: spec.command.argv(),
        cwd: spec.workspace.clone(),
        exit_code: None,
        protocol: None,
    };
    save(&spec.receipt_path, &receipt)?;
    let mut input = child.stdin.take().unwrap();
    let writer =
        thread::spawn(move || -> std::io::Result<()> { input.write_all(spec.input.as_bytes()) });
    let mut stream = child.stdout.take().unwrap();
    let mut observer = JsonlObserver::default();
    let mut chunk = [0; 8192];
    let mut output = std::io::stdout().lock();
    loop {
        let count = stream.read(&mut chunk)?;
        if count == 0 {
            break;
        }
        if spec.parse_codex {
            observer.feed(&chunk[..count]);
        }
        output.write_all(&chunk[..count])?;
        output.flush()?;
    }
    receipt.exit_code = child.wait()?.code();
    let wrote_input = writer
        .join()
        .map_err(|_| anyhow::anyhow!("input writer panicked"))?;
    if spec.parse_codex {
        receipt.protocol = Some(observer.finish());
    }
    save(&spec.receipt_path, &receipt)?;
    // A failed prompt pipe cannot be mistaken for a successful provider dispatch.
    wrote_input?;
    Ok(receipt.exit_code.unwrap_or(1))
}
