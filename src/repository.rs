use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Repository {
    pub checkout: PathBuf,
    pub common_dir: PathBuf,
    pub identity: String,
    pub remotes: Vec<(String, String)>,
    pub base: String,
    pub branch: Option<String>,
    pub platform: String,
}

pub fn git_output(path: &Path, args: &[&str]) -> Result<crate::process::ProcessResult> {
    crate::process::bounded_output(
        Command::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0"),
        30000,
        4 * 1024 * 1024,
    )
    .context("could not execute bounded Git operation")
}

pub fn git(path: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = git_output(path, args)?;
    ensure!(
        output.exit_code == Some(0),
        "Git operation {} failed: {}",
        args.first().unwrap_or(&""),
        String::from_utf8_lossy(&output.stderr.bytes)
    );
    Ok(output.stdout.bytes)
}

pub fn git_text(path: &Path, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8(git(path, args)?)?.trim().to_owned())
}

pub fn canonical(path: &Path) -> Result<PathBuf> {
    let resolved = std::fs::canonicalize(path)
        .with_context(|| format!("cannot resolve {}", path.display()))?;
    #[cfg(windows)]
    {
        // Git for Windows rejects extended-length paths when creating worktrees.
        let value = resolved.to_str().context("path must be UTF-8")?;
        if let Some(tail) = value.strip_prefix("\\\\?\\UNC\\") {
            return Ok(PathBuf::from(format!("\\\\{tail}")));
        }
        if let Some(tail) = value.strip_prefix("\\\\?\\") {
            return Ok(PathBuf::from(tail));
        }
    }
    Ok(resolved)
}

pub fn remote_identity(raw: &str) -> Result<String> {
    let raw = raw.trim();
    ensure!(!raw.is_empty(), "empty remote URL");
    let identity = if let Some((scheme, tail)) = raw.split_once("://") {
        let tail = tail.split(['?', '#']).next().unwrap_or(tail);
        if scheme == "file" {
            format!("file://{tail}")
        } else {
            let (authority, path) = tail
                .split_once('/')
                .context("remote URL has no repository path")?;
            let host = authority
                .rsplit('@')
                .next()
                .unwrap_or(authority)
                .to_lowercase();
            ensure!(
                !host.is_empty() && !path.is_empty(),
                "invalid remote identity"
            );
            format!("{}://{host}/{path}", scheme.to_lowercase())
        }
    } else if let Some((host, path)) = raw.split_once(':') {
        if host.len() == 1 {
            format!("file:{}", raw.replace('\\', "/"))
        } else {
            let host = host.rsplit('@').next().unwrap_or(host).to_lowercase();
            let path = path.split(['?', '#']).next().unwrap_or(path);
            ensure!(
                !host.is_empty() && !path.is_empty(),
                "invalid remote identity"
            );
            format!("ssh://{host}/{path}")
        }
    } else {
        format!("file:{raw}")
    };
    Ok(identity
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .to_string())
}

impl Repository {
    pub fn inspect(checkout: &Path) -> Result<Self> {
        let checkout = canonical(checkout)?;
        let root = canonical(Path::new(&git_text(
            &checkout,
            &["rev-parse", "--show-toplevel"],
        )?))?;
        ensure!(
            checkout == root,
            "provide the exact Git checkout root, not a subdirectory"
        );
        let common_dir = canonical(Path::new(&git_text(
            &root,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?))?;
        let base = git_text(&root, &["rev-parse", "--verify", "HEAD^{commit}"])
            .context("repository needs an existing commit")?;
        let branch = git_text(&root, &["symbolic-ref", "--quiet", "--short", "HEAD"]).ok();
        let names = git_text(&root, &["remote"])?;
        let mut remotes = Vec::new();
        for name in names.lines() {
            let urls = git_text(
                &root,
                &["config", "--get-all", &format!("remote.{name}.url")],
            )?;
            for url in urls.lines() {
                remotes.push((name.to_owned(), remote_identity(url)?));
            }
        }
        remotes.sort();
        let identity = if remotes.is_empty() {
            format!("local:{}", common_dir.to_string_lossy().replace('\\', "/"))
        } else {
            serde_json::to_string(&remotes)?
        };
        Ok(Self {
            checkout: root,
            common_dir,
            identity,
            remotes,
            base,
            branch,
            platform: std::env::consts::OS.to_owned(),
        })
    }

    /// Fresh-inspects the checkout and reports the full before/after snapshot when it no
    /// longer matches this captured one, instead of `revalidate`'s terse all-or-nothing
    /// refusal - specifically so a caller that needs to explain *what* changed (most
    /// commonly just `base`, from ordinary repo activity between context capture and
    /// review) can, rather than a single generic message. `revalidate` is unchanged for
    /// its existing callers; it's now implemented in terms of this.
    pub fn drift(&self) -> Result<Option<Drift>> {
        let current = Self::inspect(&self.checkout)?;
        if current == *self {
            Ok(None)
        } else {
            Ok(Some(Drift {
                expected: self.clone(),
                current,
            }))
        }
    }

    pub fn revalidate(&self) -> Result<()> {
        ensure!(
            self.drift()?.is_none(),
            "repository registration drift: path, Git directory, remote, branch or base changed; register and prepare a new assignment"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Drift {
    pub expected: Repository,
    pub current: Repository,
}

impl Drift {
    /// Human-readable summary leading with `base` specifically, per M6's "conflicting
    /// changes return for review with provenance and base information" - the acceptance
    /// line this exists for - then naming any other differing field only if one exists
    /// (checkout path/remote/branch changes are rarer, bigger reconfiguration events than
    /// ordinary base drift, worth calling out distinctly rather than folding together).
    pub fn describe(&self) -> String {
        let mut parts = vec![];
        if self.expected.base != self.current.base {
            parts.push(format!(
                "base moved from {} to {}",
                self.expected.base, self.current.base
            ));
        }
        if self.expected.checkout != self.current.checkout {
            parts.push("checkout path changed".to_string());
        }
        if self.expected.common_dir != self.current.common_dir {
            parts.push("Git directory changed".to_string());
        }
        if self.expected.remotes != self.current.remotes {
            parts.push("remotes changed".to_string());
        }
        if self.expected.branch != self.current.branch {
            parts.push(format!(
                "branch moved from {:?} to {:?}",
                self.expected.branch, self.current.branch
            ));
        }
        if parts.is_empty() {
            "repository registration drift".to_string()
        } else {
            parts.join("; ")
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutStatus {
    pub clean: bool,
    pub dirty_paths: Vec<String>,
    pub unsupported_index_entries: bool,
}

fn parse_porcelain_z(raw: &[u8]) -> Result<Vec<String>> {
    let mut paths = Vec::new();
    let mut entries = raw.split(|b| *b == 0);
    while let Some(entry) = entries.next() {
        if entry.is_empty() {
            continue;
        }
        ensure!(entry.len() >= 4, "invalid Git status entry");
        let path = std::str::from_utf8(&entry[3..])
            .context("non-UTF8 dirty path")?
            .to_owned();
        if matches!(entry[0], b'R' | b'C') {
            let _ = entries.next();
        }
        paths.push(path);
    }
    Ok(paths)
}

impl Repository {
    /// Inspect working-tree cleanliness and unsupported index flags without mutating Git state.
    pub fn checkout_status(&self) -> Result<CheckoutStatus> {
        self.revalidate()?;
        let status = git(
            &self.checkout,
            &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
        )?;
        let dirty_paths = parse_porcelain_z(&status)?;
        let mut unsupported_index_entries = false;
        for entry in git(&self.checkout, &["ls-files", "-v", "-z"])?
            .split(|b| *b == 0)
            .filter(|b| !b.is_empty())
        {
            if entry[0] != b'H' {
                unsupported_index_entries = true;
                break;
            }
        }
        Ok(CheckoutStatus {
            clean: dirty_paths.is_empty() && !unsupported_index_entries,
            dirty_paths,
            unsupported_index_entries,
        })
    }

    /// Fail closed with an actionable summary if the checkout is dirty or has unsupported index flags.
    pub fn ensure_clean(&self, context: &str) -> Result<()> {
        let status = self.checkout_status()?;
        ensure!(
            status.dirty_paths.is_empty(),
            "{context} ({} dirty path(s): {}{})",
            status.dirty_paths.len(),
            status
                .dirty_paths
                .iter()
                .take(5)
                .cloned()
                .collect::<Vec<_>>()
                .join(", "),
            if status.dirty_paths.len() > 5 {
                format!(", +{} more", status.dirty_paths.len() - 5)
            } else {
                String::new()
            }
        );
        ensure!(
            !status.unsupported_index_entries,
            "source index uses skip-worktree/assume-unchanged or unsupported entries"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(base: &str) -> Repository {
        Repository {
            checkout: PathBuf::from("C:/repo"),
            common_dir: PathBuf::from("C:/repo/.git"),
            identity: "local:C:/repo/.git".into(),
            remotes: vec![],
            base: base.into(),
            branch: Some("main".into()),
            platform: "windows".into(),
        }
    }

    #[test]
    fn describe_names_only_the_base_when_only_the_base_moved() {
        let drift = Drift {
            expected: repo("aaa"),
            current: repo("bbb"),
        };
        assert_eq!(drift.describe(), "base moved from aaa to bbb");
    }

    #[test]
    fn describe_names_every_field_that_actually_differs() {
        let mut current = repo("bbb");
        current.branch = Some("feature".into());
        let drift = Drift {
            expected: repo("aaa"),
            current,
        };
        assert_eq!(
            drift.describe(),
            "base moved from aaa to bbb; branch moved from Some(\"main\") to Some(\"feature\")"
        );
    }

    #[test]
    fn checkout_status_reports_clean_dirty_and_renamed_paths() {
        let dir = tempfile::tempdir().unwrap();
        let repo_dir = dir.path().join("repo");
        std::fs::create_dir(&repo_dir).unwrap();
        std::fs::write(repo_dir.join("a.txt"), "one\n").unwrap();
        git(&repo_dir, &["init", "-b", "main"]).unwrap();
        git(&repo_dir, &["config", "core.autocrlf", "false"]).unwrap();
        git(&repo_dir, &["add", "."]).unwrap();
        git(
            &repo_dir,
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
                "init",
            ],
        )
        .unwrap();

        let repo = Repository::inspect(&repo_dir).unwrap();
        let clean = repo.checkout_status().unwrap();
        assert!(clean.clean);
        assert!(clean.dirty_paths.is_empty());
        assert!(repo.ensure_clean("clean check").is_ok());

        std::fs::write(repo_dir.join("untracked.txt"), "wip\n").unwrap();
        git(&repo_dir, &["mv", "a.txt", "b.txt"]).unwrap();
        let dirty = repo.checkout_status().unwrap();
        assert!(!dirty.clean);
        assert_eq!(dirty.dirty_paths, vec!["b.txt", "untracked.txt"]);
        let err = repo
            .ensure_clean("source checkout is dirty")
            .unwrap_err()
            .to_string();
        assert!(err.contains("2 dirty path(s)"), "{err}");
        assert!(err.contains("b.txt, untracked.txt"), "{err}");
    }
}
