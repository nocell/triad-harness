use crate::{cli::ReviewArgs, model::GitTarget, storage};
use anyhow::{Context, Result};
use serde::Deserialize;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Stdio,
};
use tokio::{io::AsyncWriteExt, process::Command};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhPr {
    number: u64,
    base_ref_oid: String,
    head_ref_oid: String,
    base_ref_name: String,
    head_ref_name: String,
    title: String,
}

pub async fn resolve_target(args: &ReviewArgs, run_dir: &Path) -> Result<GitTarget> {
    let source_repo = match git_text(Path::new("."), &["rev-parse", "--show-toplevel"]).await {
        Ok(path) => PathBuf::from(path.trim()),
        Err(_) => {
            let target = args
                .target
                .as_deref()
                .context("not in a Git repository; provide a GitHub PR URL")?;
            let remote = github_remote_from_pr_url(target)
                .context("outside a repository only a GitHub PR URL is supported")?;
            let source = run_dir.join("source");
            run(Command::new("git")
                .args(["clone", "--no-checkout", &remote])
                .arg(&source))
            .await?;
            source
        }
    };
    let remote_url = git_text(&source_repo, &["remote", "get-url", "origin"])
        .await
        .ok()
        .map(|value| value.trim().to_string());

    if let Some(target) = args
        .target
        .as_deref()
        .filter(|target| target.contains("/pull/") || target.chars().all(|c| c.is_ascii_digit()))
    {
        let output = command_output(Command::new("gh").current_dir(&source_repo).args([
            "pr",
            "view",
            target,
            "--json",
            "number,url,baseRefOid,headRefOid,baseRefName,headRefName,title",
        ]))
        .await?;
        let pr: GhPr = serde_json::from_slice(&output)?;
        return Ok(GitTarget {
            source_repo,
            remote_url,
            base_sha: pr.base_ref_oid,
            head_sha: pr.head_ref_oid,
            title: format!(
                "PR #{} {} ({} → {})",
                pr.number, pr.title, pr.head_ref_name, pr.base_ref_name
            ),
            uncommitted: false,
            patch_path: None,
            untracked_files: Vec::new(),
        });
    }

    if args.uncommitted {
        let head = git_text(&source_repo, &["rev-parse", "HEAD"])
            .await?
            .trim()
            .to_string();
        let patch = git_bytes(&source_repo, &["diff", "--binary", "HEAD"]).await?;
        let patch_path = run_dir.join("target.patch");
        storage::atomic_write(&patch_path, &patch)?;
        let untracked_raw = git_bytes(
            &source_repo,
            &["ls-files", "--others", "--exclude-standard", "-z"],
        )
        .await?;
        let untracked_files = untracked_raw
            .split(|byte| *byte == 0)
            .filter(|part| !part.is_empty())
            .map(|part| PathBuf::from(String::from_utf8_lossy(part).to_string()))
            .collect();
        return Ok(GitTarget {
            source_repo,
            remote_url,
            base_sha: head.clone(),
            head_sha: head,
            title: "uncommitted working tree".into(),
            uncommitted: true,
            patch_path: Some(patch_path),
            untracked_files,
        });
    }

    if let Some(commit) = &args.commit {
        let head = git_text(&source_repo, &["rev-parse", commit])
            .await?
            .trim()
            .to_string();
        let base = git_text(&source_repo, &["rev-parse", &format!("{head}^")])
            .await?
            .trim()
            .to_string();
        return Ok(GitTarget {
            source_repo,
            remote_url,
            base_sha: base,
            head_sha: head,
            title: format!("commit {commit}"),
            uncommitted: false,
            patch_path: None,
            untracked_files: Vec::new(),
        });
    }

    let head = git_text(&source_repo, &["rev-parse", "HEAD"])
        .await?
        .trim()
        .to_string();
    let base_ref = match &args.base {
        Some(base) => base.clone(),
        None => default_base_ref(&source_repo).await?,
    };
    let base = git_text(&source_repo, &["merge-base", &base_ref, &head])
        .await?
        .trim()
        .to_string();
    Ok(GitTarget {
        source_repo,
        remote_url,
        base_sha: base,
        head_sha: head,
        title: format!("current branch vs {base_ref}"),
        uncommitted: false,
        patch_path: None,
        untracked_files: Vec::new(),
    })
}

pub async fn create_snapshot(
    target: &GitTarget,
    destination: &Path,
    context_markdown: &str,
) -> Result<()> {
    if destination.exists() {
        fs::remove_dir_all(destination)
            .with_context(|| format!("remove stale snapshot {}", destination.display()))?;
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    run(Command::new("git")
        .args(["clone", "--no-checkout", "--shared"])
        .arg(&target.source_repo)
        .arg(destination))
    .await?;
    if git_status(destination, &["checkout", "--detach", &target.head_sha])
        .await
        .is_err()
    {
        let remote = target
            .remote_url
            .as_deref()
            .context("target commit is missing locally and no origin URL is available")?;
        run(Command::new("git").current_dir(destination).args([
            "fetch",
            "--no-tags",
            remote,
            &target.head_sha,
        ]))
        .await?;
        run(Command::new("git").current_dir(destination).args([
            "checkout",
            "--detach",
            &target.head_sha,
        ]))
        .await?;
    }
    ensure_commit(destination, target.remote_url.as_deref(), &target.base_sha).await?;
    // All required objects are present now. A provider snapshot must not retain
    // a destination that an agent could push to.
    let _ = git_status(destination, &["remote", "remove", "origin"]).await;

    if target.uncommitted {
        if let Some(patch_path) = &target.patch_path {
            let patch = fs::read(patch_path)?;
            if !patch.is_empty() {
                run_with_stdin(
                    Command::new("git")
                        .current_dir(destination)
                        .args(["apply", "--binary", "-"]),
                    &patch,
                )
                .await?;
            }
        }
        for relative in &target.untracked_files {
            let source = target.source_repo.join(relative);
            let dest = destination.join(relative);
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            if source.is_dir() {
                copy_dir(&source, &dest)?;
            } else {
                fs::copy(&source, &dest)
                    .with_context(|| format!("copy untracked {}", relative.display()))?;
            }
        }
    }
    let triad_dir = destination.join(".triad-review");
    fs::create_dir_all(&triad_dir)?;
    fs::write(triad_dir.join("context.md"), context_markdown)?;
    Ok(())
}

pub async fn diff_for_target(snapshot: &Path, target: &GitTarget) -> Result<Vec<u8>> {
    if target.uncommitted {
        add_intent_for_untracked(snapshot).await?;
        git_bytes(snapshot, &["diff", "--binary", "HEAD"]).await
    } else {
        git_bytes(
            snapshot,
            &["diff", "--binary", &target.base_sha, &target.head_sha],
        )
        .await
    }
}

pub async fn diff_stat(snapshot: &Path, target: &GitTarget) -> Result<String> {
    if target.uncommitted {
        Ok(git_text(snapshot, &["diff", "--stat", "HEAD"]).await?)
    } else {
        Ok(git_text(
            snapshot,
            &["diff", "--stat", &target.base_sha, &target.head_sha],
        )
        .await?)
    }
}

pub async fn status_signature(snapshot: &Path) -> Result<Vec<u8>> {
    let mut signature = git_bytes(snapshot, &["rev-parse", "HEAD"]).await?;
    signature.push(0);
    signature.extend(git_bytes(snapshot, &["status", "--porcelain=v1", "-z"]).await?);
    Ok(signature)
}

pub async fn working_patch(snapshot: &Path) -> Result<Vec<u8>> {
    let triad = snapshot.join(".triad-review");
    if triad.exists() {
        fs::remove_dir_all(&triad)?;
    }
    add_intent_for_untracked(snapshot).await?;
    git_bytes(snapshot, &["diff", "--binary", "HEAD"]).await
}

async fn add_intent_for_untracked(snapshot: &Path) -> Result<()> {
    let raw = git_bytes(
        snapshot,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )
    .await?;
    let files: Vec<String> = raw
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).to_string())
        .filter(|path| !path.starts_with(".triad-review/"))
        .collect();
    if files.is_empty() {
        return Ok(());
    }
    let mut command = Command::new("git");
    command.current_dir(snapshot).args(["add", "-N", "--"]);
    command.args(files);
    run(&mut command).await
}

async fn ensure_commit(repo: &Path, remote: Option<&str>, sha: &str) -> Result<()> {
    if git_status(repo, &["cat-file", "-e", &format!("{sha}^{{commit}}")])
        .await
        .is_ok()
    {
        return Ok(());
    }
    let remote = remote.context("commit missing and no remote configured")?;
    run(Command::new("git")
        .current_dir(repo)
        .args(["fetch", "--no-tags", remote, sha]))
    .await
}

async fn default_base_ref(repo: &Path) -> Result<String> {
    if let Ok(value) = git_text(repo, &["symbolic-ref", "refs/remotes/origin/HEAD"]).await {
        return Ok(value.trim().to_string());
    }
    for candidate in ["origin/main", "main", "origin/master", "master"] {
        if git_status(repo, &["rev-parse", "--verify", candidate])
            .await
            .is_ok()
        {
            return Ok(candidate.into());
        }
    }
    anyhow::bail!("cannot determine default base; pass --base")
}

fn github_remote_from_pr_url(value: &str) -> Option<String> {
    let prefix = "https://github.com/";
    let rest = value.strip_prefix(prefix)?;
    let (repo, _) = rest.split_once("/pull/")?;
    Some(format!("{prefix}{repo}.git"))
}

fn copy_dir(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

async fn git_text(repo: &Path, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8(git_bytes(repo, args).await?)?)
}

async fn git_bytes(repo: &Path, args: &[&str]) -> Result<Vec<u8>> {
    command_output(Command::new("git").current_dir(repo).args(args)).await
}

async fn git_status(repo: &Path, args: &[&str]) -> Result<()> {
    run(Command::new("git").current_dir(repo).args(args)).await
}

async fn run(command: &mut Command) -> Result<()> {
    let output = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;
    if !output.status.success() {
        anyhow::bail!(
            "command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

async fn command_output(command: &mut Command) -> Result<Vec<u8>> {
    let output = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;
    if !output.status.success() {
        anyhow::bail!(
            "command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(output.stdout)
}

async fn run_with_stdin(command: &mut Command, input: &[u8]) -> Result<()> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .as_mut()
        .context("child stdin missing")?
        .write_all(input)
        .await?;
    let output = child.wait_with_output().await?;
    if !output.status.success() {
        anyhow::bail!(
            "command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as StdCommand;

    #[test]
    fn parses_github_pr_url() {
        assert_eq!(
            github_remote_from_pr_url("https://github.com/acme/repo/pull/42"),
            Some("https://github.com/acme/repo.git".into())
        );
    }

    #[tokio::test]
    async fn snapshot_does_not_modify_source_checkout() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        for args in [
            vec!["init", "-b", "main"],
            vec!["config", "user.email", "triad@test.invalid"],
            vec!["config", "user.name", "Triad Test"],
        ] {
            assert!(
                StdCommand::new("git")
                    .current_dir(&source)
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        fs::write(source.join("file.txt"), "base\n").unwrap();
        assert!(
            StdCommand::new("git")
                .current_dir(&source)
                .args(["add", "."])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            StdCommand::new("git")
                .current_dir(&source)
                .args(["commit", "-m", "base"])
                .status()
                .unwrap()
                .success()
        );
        let base = String::from_utf8(
            StdCommand::new("git")
                .current_dir(&source)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        fs::write(source.join("file.txt"), "base\nhead\n").unwrap();
        assert!(
            StdCommand::new("git")
                .current_dir(&source)
                .args(["commit", "-am", "head"])
                .status()
                .unwrap()
                .success()
        );
        let head = String::from_utf8(
            StdCommand::new("git")
                .current_dir(&source)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        let target = GitTarget {
            source_repo: source.clone(),
            remote_url: None,
            base_sha: base,
            head_sha: head,
            title: "test".into(),
            uncommitted: false,
            patch_path: None,
            untracked_files: Vec::new(),
        };
        let snapshot = temp.path().join("snapshot");
        create_snapshot(&target, &snapshot, "context")
            .await
            .unwrap();
        let diff = String::from_utf8(diff_for_target(&snapshot, &target).await.unwrap()).unwrap();
        assert!(diff.contains("+head"));
        assert!(
            !StdCommand::new("git")
                .current_dir(&snapshot)
                .args(["remote"])
                .output()
                .unwrap()
                .stdout
                .iter()
                .any(|byte| !byte.is_ascii_whitespace()),
            "disposable provider snapshots must not retain a push remote"
        );
        let source_status = StdCommand::new("git")
            .current_dir(&source)
            .args(["status", "--porcelain"])
            .output()
            .unwrap();
        assert!(source_status.stdout.is_empty());
    }
}
