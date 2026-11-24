//! Git owns candidate bytes. Private indexes leave the workspace index and HEAD alone.
use crate::{
    orchestration::safe_file,
    repository::{Repository, git, git_output, git_text},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharedWorkspace {
    pub workflow_id: String,
    pub stage: usize,
    pub baseline_tree: String,
    pub candidate_scope: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_existing_workspace: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub tree: String,
    pub reference: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitQuery {
    Status,
    Diff,
    History,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitRead {
    pub query: GitQuery,
    pub base: String,
    pub tree: String,
    pub artifact: crate::store::Artifact,
    pub offset: usize,
    pub limit: usize,
}

pub struct GitReadRequest {
    pub query: GitQuery,
    pub offset: usize,
    pub limit: usize,
}

pub fn file_artifact(path: &Path) -> Result<crate::store::Artifact> {
    let mut input = fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut bytes = 0;
    let mut chunk = [0; 8192];
    loop {
        let count = input.read(&mut chunk)?;
        if count == 0 {
            break;
        }
        bytes += count;
        ensure!(bytes <= 4 * 1024 * 1024, "workflow artifact exceeds 4 MiB");
        hash.update(&chunk[..count]);
    }
    Ok(crate::store::Artifact {
        path: path.to_owned(),
        hash: format!("{:x}", hash.finalize()),
        bytes,
    })
}

pub fn read_page(read: &GitRead) -> Result<serde_json::Value> {
    let actual = file_artifact(&read.artifact.path)?;
    ensure!(
        actual.hash == read.artifact.hash && actual.bytes == read.artifact.bytes,
        "Git read artifact changed"
    );
    ensure!(
        read.offset <= actual.bytes && (1..=8192).contains(&read.limit),
        "Git read page outside bounds"
    );
    let mut input = fs::File::open(&actual.path)?;
    input.seek(SeekFrom::Start(read.offset as u64))?;
    let mut page = vec![0; read.limit.min(actual.bytes - read.offset)];
    input.read_exact(&mut page)?;
    let next = read.offset + page.len();
    Ok(
        serde_json::json!({"query":read.query,"base":read.base,"tree":read.tree,"artifact":read.artifact,"offset":read.offset,"text":String::from_utf8_lossy(&page),"next_offset":if next < actual.bytes {Some(next)} else {None}}),
    )
}

pub fn read_git(
    root: &Path,
    repository: &Repository,
    tree: &str,
    workflow_id: &str,
    sequence: usize,
    request: GitReadRequest,
    cancelled: impl FnMut() -> Result<bool>,
) -> Result<GitRead> {
    let GitReadRequest {
        query,
        offset,
        limit,
    } = request;
    ensure!(
        offset <= 4 * 1024 * 1024 && (1..=8192).contains(&limit),
        "Git read page outside bounds"
    );
    let directory = root.join("workflow-artifacts").join(workflow_id);
    fs::create_dir_all(&directory)?;
    let output = directory.join(format!("git-{sequence}.stdout"));
    let error = directory.join(format!("git-{sequence}.stderr"));
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(&repository.checkout)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0");
    match query {
        GitQuery::History => {
            command.args([
                "log",
                "--no-decorate",
                "--format=%h %s",
                "-n",
                "64",
                &repository.base,
                "--",
            ]);
        }
        GitQuery::Status | GitQuery::Diff => {
            command.args([
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--no-color",
                "--no-renames",
            ]);
            command.arg(if query == GitQuery::Status {
                "--stat"
            } else {
                "--binary"
            });
            command.args([&repository.base, tree, "--"]);
        }
    }
    let result = crate::process::OwnedProcess::spawn_streamed(
        &mut command,
        4 * 1024 * 1024,
        (&output, &error),
    )?
    .wait(30000, cancelled)?;
    ensure!(
        result.exit_code == Some(0)
            && result.reason == crate::process::StopReason::Exited
            && result.tree_terminated,
        "workflow Git inspection failed or interrupted"
    );
    ensure!(
        result.stdout.total_bytes <= 4 * 1024 * 1024
            && result.stderr.total_bytes <= 4 * 1024 * 1024,
        "Git inspection exceeded artifact bound; no truncated result supplied"
    );
    let artifact = file_artifact(&output)?;
    ensure!(offset <= artifact.bytes, "Git read offset exceeds artifact");
    Ok(GitRead {
        query,
        base: repository.base.clone(),
        tree: tree.to_owned(),
        artifact,
        offset,
        limit,
    })
}

struct PrivateIndex(PathBuf);
impl Drop for PrivateIndex {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
        let _ = fs::remove_file(self.0.with_extension("index.lock"));
    }
}
impl PrivateIndex {
    fn new(root: &Path) -> Result<Self> {
        let directory = root.join("workflow-git");
        fs::create_dir_all(&directory)?;
        Ok(Self(
            directory.join(format!("{}.index", uuid::Uuid::new_v4())),
        ))
    }
    fn git(&self, workspace: &Path, args: &[&str]) -> Result<Vec<u8>> {
        let result = crate::process::bounded_output(
            Command::new("git")
                .arg("-C")
                .arg(workspace)
                .args(args)
                .env("GIT_INDEX_FILE", &self.0)
                .env("GIT_TERMINAL_PROMPT", "0")
                .env("GIT_OPTIONAL_LOCKS", "0"),
            30000,
            4 * 1024 * 1024,
        )?;
        ensure!(
            result.exit_code == Some(0),
            "workflow Git operation failed: {}",
            String::from_utf8_lossy(&result.stderr.bytes)
        );
        Ok(result.stdout.bytes)
    }
}

pub fn validate_identity(workspace: &Path, repository: &Repository) -> Result<()> {
    let actual = Repository::inspect(workspace)?;
    ensure!(
        actual.common_dir == repository.common_dir
            && actual.checkout == workspace
            && actual.base == repository.base
            && actual.branch.is_none(),
        "workflow workspace repository/base changed"
    );
    ensure!(
        git(
            workspace,
            &["diff", "--cached", "--name-only", &repository.base, "--"]
        )?
        .is_empty(),
        "workflow workspace index changed; inspect before continuing"
    );
    Ok(())
}

pub fn changed(root: &Path, workspace: &Path, tree: &str, scope: &[String]) -> Result<Vec<String>> {
    let index = PrivateIndex::new(root)?;
    index.git(workspace, &["read-tree", tree])?;
    let mut bytes = index.git(
        workspace,
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-renames",
            "--name-only",
            "-z",
            tree,
            "--",
        ],
    )?;
    bytes.extend(index.git(
        workspace,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )?);
    let mut changed: Vec<String> = bytes
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| Ok(std::str::from_utf8(p)?.to_owned()))
        .collect::<Result<_>>()?;
    // Git's normal diff applies clean/EOL filters. Inherited candidates require raw equality.
    changed.retain(|name| !scope.contains(name));
    let entries = index.git(workspace, &["ls-files", "--stage", "-z"])?;
    let mut expected = std::collections::BTreeMap::new();
    for entry in entries.split(|b| *b == 0).filter(|e| !e.is_empty()) {
        let entry = std::str::from_utf8(entry)?;
        let (metadata, name) = entry.split_once('\t').context("invalid Git index entry")?;
        if scope.iter().any(|s| s == name) {
            expected.insert(
                name,
                metadata
                    .split_whitespace()
                    .nth(1)
                    .context("missing Git blob")?,
            );
        }
    }
    let mut existing = Vec::new();
    for name in scope {
        let path = safe_file(workspace, name)?;
        if path.exists() {
            ensure!(
                fs::symlink_metadata(&path)?.is_file(),
                "candidate is not a regular file: {name}"
            );
            existing.push(name.as_str());
        } else if expected.contains_key(name.as_str()) {
            changed.push(name.clone());
        }
    }
    for chunk in existing.chunks(32) {
        let mut args = vec!["hash-object", "--no-filters", "--"];
        args.extend(chunk.iter().copied());
        let hashes = String::from_utf8(git(workspace, &args)?)?;
        let hashes: Vec<_> = hashes.lines().collect();
        ensure!(
            hashes.len() == chunk.len(),
            "Git candidate hash count changed"
        );
        for (name, hash) in chunk.iter().copied().zip(hashes) {
            if expected.get(name).copied() != Some(hash) {
                changed.push(name.to_owned());
            }
        }
    }
    Ok(changed)
}

pub fn candidate_tree(
    root: &Path,
    workspace: &Path,
    base: &str,
    scope: &[String],
) -> Result<String> {
    let index = PrivateIndex::new(root)?;
    index.git(workspace, &["read-tree", base])?;
    for name in scope {
        let path = safe_file(workspace, name)?;
        if !path.exists() {
            index.git(workspace, &["update-index", "--force-remove", "--", name])?;
            continue;
        }
        ensure!(
            fs::symlink_metadata(&path)?.is_file(),
            "checkpoint requires a regular file: {name}"
        );
        let oid = git_text(
            workspace,
            &["hash-object", "-w", "--no-filters", "--", name],
        )?;
        let entry = index.git(workspace, &["ls-files", "--stage", "--", name])?;
        let mode = if entry.starts_with(b"100755 ") {
            "100755"
        } else {
            "100644"
        };
        index.git(
            workspace,
            &["update-index", "--add", "--cacheinfo", mode, &oid, name],
        )?;
    }
    Ok(String::from_utf8(index.git(workspace, &["write-tree"])?)?
        .trim()
        .to_owned())
}

pub fn capture(
    root: &Path,
    workspace: &Path,
    base: &str,
    scope: &[String],
    workflow_id: &str,
    stage: usize,
) -> Result<Checkpoint> {
    uuid::Uuid::parse_str(workflow_id)?;
    let tree = candidate_tree(root, workspace, base, scope)?;
    let reference = format!("refs/conductor/workflows/{workflow_id}/stage-{stage}");
    git(workspace, &["update-ref", &reference, &tree, ""])?;
    let checkpoint = Checkpoint { tree, reference };
    ensure!(
        changed(root, workspace, &checkpoint.tree, scope)?.is_empty(),
        "candidate changed during checkpoint capture"
    );
    Ok(checkpoint)
}

pub fn validate_checkpoint(repository: &Repository, checkpoint: &Checkpoint) -> Result<()> {
    ensure!(
        git_text(
            &repository.checkout,
            &["rev-parse", "--verify", &checkpoint.reference]
        )? == checkpoint.tree,
        "workflow checkpoint reference changed"
    );
    ensure!(
        git_text(&repository.checkout, &["cat-file", "-t", &checkpoint.tree])? == "tree",
        "workflow checkpoint tree is missing"
    );
    Ok(())
}

pub fn stage_changes(
    repository: &Repository,
    baseline: &str,
    checkpoint: &Checkpoint,
) -> Result<Vec<String>> {
    let bytes = git(
        &repository.checkout,
        &[
            "diff-tree",
            "--no-commit-id",
            "--no-renames",
            "--name-only",
            "-r",
            "-z",
            baseline,
            &checkpoint.tree,
            "--",
        ],
    )?;
    bytes
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| {
            Ok(std::str::from_utf8(p)
                .context("non-UTF8 checkpoint path")?
                .to_owned())
        })
        .collect()
}

pub fn reset_to_tree(
    root: &Path,
    workspace: &Path,
    repository: &Repository,
    tree: &str,
    scope: &[String],
) -> Result<()> {
    git(
        workspace,
        &["reset", "--mixed", "--quiet", &repository.base],
    )?;
    git(workspace, &["clean", "-fd"])?;
    for path in changed(root, workspace, tree, scope)? {
        let target = safe_file(workspace, &path)?;
        let spec = format!("{tree}:{path}");
        let output = git_output(workspace, &["cat-file", "blob", &spec])?;
        if output.exit_code == Some(0) {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&target, output.stdout.bytes)?;
        } else if target.exists() {
            fs::remove_file(&target)?;
        }
    }
    ensure!(
        changed(root, workspace, tree, scope)?.is_empty(),
        "failed to reset workflow workspace to baseline tree"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpoints_retain_new_deleted_and_binary_files_without_changing_head_or_indexes() {
        let tmp = tempfile::tempdir().unwrap();
        // Production canonicalizes every root at entry; CI's Windows TEMP is an 8.3 short
        // path (RUNNER~1), so use the canonical form here too or path identity checks fail.
        let dir_root = crate::repository::canonical(tmp.path()).unwrap();
        let repo = dir_root.as_path().join("repo");
        fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-b", "main"]).unwrap();
        git(&repo, &["config", "core.autocrlf", "false"]).unwrap();
        fs::write(repo.join("gone.txt"), "original").unwrap();
        fs::write(repo.join(".gitattributes"), "new.txt text eol=lf\n").unwrap();
        fs::write(repo.join(".gitignore"), "new.bin\n").unwrap();
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
                "fixture",
            ],
        )
        .unwrap();
        let source = Repository::inspect(&repo).unwrap();
        let source_index = fs::read(repo.join(".git/index")).unwrap();
        let workspace = dir_root.as_path().join("workspace");
        git(
            &repo,
            &[
                "worktree",
                "add",
                "--detach",
                workspace.to_str().unwrap(),
                &source.base,
            ],
        )
        .unwrap();
        let index_path = git_text(
            &workspace,
            &["rev-parse", "--path-format=absolute", "--git-path", "index"],
        )
        .unwrap();
        let workspace_index = fs::read(&index_path).unwrap();
        fs::remove_file(workspace.join("gone.txt")).unwrap();
        let bytes: Vec<_> = (0..180000).map(|n| (n % 256) as u8).collect();
        fs::write(workspace.join("new.bin"), &bytes).unwrap();
        fs::write(workspace.join("new.txt"), "raw\r\nbytes\r\n").unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let scope = vec![
            "gone.txt".to_string(),
            "new.bin".to_string(),
            "new.txt".to_string(),
        ];
        let first = capture(dir_root.as_path(), &workspace, &source.base, &scope, &id, 0).unwrap();
        validate_checkpoint(&source, &first).unwrap();
        assert!(
            changed(dir_root.as_path(), &workspace, &first.tree, &scope)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            git(&repo, &["show", &format!("{}:new.bin", first.tree)]).unwrap(),
            bytes
        );
        assert_eq!(stage_changes(&source, &source.base, &first).unwrap(), scope);
        fs::write(workspace.join("new.txt"), "raw\nbytes\n").unwrap();
        assert!(
            changed(dir_root.as_path(), &workspace, &first.tree, &scope)
                .unwrap()
                .contains(&"new.txt".into())
        );
        fs::write(workspace.join("new.txt"), "raw\r\nbytes\r\n").unwrap();
        fs::write(workspace.join("gone.txt"), "restored by second stage").unwrap();
        fs::remove_file(workspace.join("new.bin")).unwrap();
        let second = capture(dir_root.as_path(), &workspace, &source.base, &scope, &id, 1).unwrap();
        assert!(
            changed(dir_root.as_path(), &workspace, &second.tree, &scope)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            git(&repo, &["show", &format!("{}:new.bin", first.reference)]).unwrap(),
            bytes
        );
        assert!(git(&repo, &["show", &format!("{}:gone.txt", first.tree)]).is_err());
        assert_eq!(fs::read(&index_path).unwrap(), workspace_index);
        assert_eq!(fs::read(repo.join(".git/index")).unwrap(), source_index);
        assert_eq!(Repository::inspect(&workspace).unwrap().base, source.base);
        fs::write(workspace.join("new.bin"), "unexpected reappearance").unwrap();
        assert!(
            changed(dir_root.as_path(), &workspace, &second.tree, &scope)
                .unwrap()
                .contains(&"new.bin".into())
        );
        git(&repo, &["update-ref", &first.reference, &second.tree]).unwrap();
        assert!(validate_checkpoint(&source, &first).is_err());
    }
}
