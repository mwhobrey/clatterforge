use crate::{
    process::{Presence, ProcessIdentity, current_identity, presence},
    store::FakeOutcome,
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::{Write, stderr, stdout},
    path::PathBuf,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Spec {
    pub workspace: PathBuf,
    pub output_file: String,
    pub contents: String,
    pub outcome: FakeOutcome,
    pub gate: PathBuf,
    pub token: String,
    pub owner: ProcessIdentity,
}

pub fn run(spec_path: &std::path::Path, token: &str, verify: bool) -> Result<i32> {
    let spec: Spec = serde_json::from_slice(&fs::read(spec_path)?)?;
    ensure!(spec.token == token, "fixture launch token mismatch");
    let start = Instant::now();
    loop {
        ensure!(
            presence(&spec.owner) == Presence::Alive,
            "fixture supervisor is no longer alive"
        );
        if fs::read_to_string(&spec.gate).ok().as_deref() == Some(token) {
            break;
        }
        ensure!(
            start.elapsed() < Duration::from_secs(5),
            "fixture launch gate expired"
        );
        thread::sleep(Duration::from_millis(10));
    }
    ensure!(
        std::env::current_dir()? == spec.workspace,
        "fixture cwd differs from assigned workspace"
    );
    let output = spec.workspace.join(&spec.output_file);
    if verify {
        if spec.outcome == FakeOutcome::VerificationHang {
            loop {
                thread::sleep(Duration::from_millis(100));
            }
        }
        println!("independent fixture output check");
        return Ok(
            if spec.outcome != FakeOutcome::VerificationFails
                && fs::read(output)? == spec.contents.as_bytes()
            {
                0
            } else {
                7
            },
        );
    }
    println!("fixture started; tests passed is only agent text");
    if spec.outcome == FakeOutcome::FailBeforeWrite {
        return Ok(3);
    }
    if spec.outcome == FakeOutcome::SpawnDescendant {
        let mut command = Command::new(std::env::current_exe()?);
        command
            .arg("__fixture-descendant")
            .arg(spec_path)
            .arg(token)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let _child = command.spawn()?;
    }
    if matches!(
        spec.outcome,
        FakeOutcome::Hang | FakeOutcome::SpawnDescendant
    ) {
        loop {
            thread::sleep(Duration::from_millis(100));
        }
    }
    if spec.outcome == FakeOutcome::OutputFlood {
        let chunk = vec![b'x'; 8192];
        let mut out = stdout().lock();
        let mut err = stderr().lock();
        for _ in 0..256 {
            out.write_all(&chunk)?;
            err.write_all(&chunk)?;
        }
        out.flush()?;
        err.flush()?;
    }
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(output)?;
    file.write_all(if spec.outcome == FakeOutcome::WrongOutput {
        b"wrong fixture output"
    } else {
        spec.contents.as_bytes()
    })?;
    file.sync_all()?;
    Ok(if spec.outcome == FakeOutcome::FailAfterWrite {
        4
    } else {
        0
    })
}

pub fn descendant(spec_path: &std::path::Path, token: &str) -> Result<()> {
    let spec: Spec = serde_json::from_slice(&fs::read(spec_path)?)?;
    ensure!(spec.token == token, "descendant token mismatch");
    fs::write(
        spec_path.with_extension("descendant.json"),
        serde_json::to_vec(&current_identity()?)?,
    )?;
    loop {
        thread::sleep(Duration::from_millis(100));
    }
}
