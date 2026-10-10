use serde_json::Value;
use std::{fs, path::Path, process::Command};
use triad::{git, model::GitTarget, packet};

fn command(repo: &Path, args: &[&str]) -> Vec<u8> {
    let output = Command::new("git")
        .current_dir(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn revision(repo: &Path) -> String {
    String::from_utf8(command(repo, &["rev-parse", "HEAD"]))
        .unwrap()
        .trim()
        .into()
}

fn repository(path: &Path) {
    fs::create_dir_all(path).unwrap();
    command(path, &["init", "-b", "main"]);
    command(path, &["config", "user.email", "review@test.invalid"]);
    command(path, &["config", "user.name", "Review Test"]);
    command(path, &["config", "core.autocrlf", "false"]);
    command(path, &["config", "diff.renames", "true"]);
}

fn commit(repo: &Path, message: &str) -> String {
    command(repo, &["add", "."]);
    command(repo, &["commit", "-m", message]);
    revision(repo)
}

fn target(repo: &Path, base: String, head: String) -> GitTarget {
    GitTarget {
        source_repo: repo.into(),
        remote_url: None,
        base_sha: base,
        head_sha: head,
        title: "review fixture".into(),
        uncommitted: false,
        patch_path: None,
        untracked_files: vec![],
    }
}

async fn prepare(snapshot: &Path, target: &GitTarget) -> Vec<u8> {
    git::create_snapshot(target, snapshot, "preparation context")
        .await
        .unwrap();
    let diff = git::diff_for_target(snapshot, target).await.unwrap();
    packet::build(snapshot, target, &diff, "Frozen review context")
        .await
        .unwrap();
    diff
}

fn index(snapshot: &Path) -> Value {
    serde_json::from_slice(&fs::read(snapshot.join(".triad-review/changed-files.json")).unwrap())
        .unwrap()
}

fn change<'a>(index: &'a Value, path: &str) -> &'a Value {
    index["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["before"]["path"] == path || item["after"]["path"] == path)
        .unwrap_or_else(|| panic!("missing changed path {path}"))
}

fn asset(snapshot: &Path, side: &Value) -> Vec<u8> {
    fs::read(
        snapshot
            .join(".triad-review")
            .join(side["asset"].as_str().unwrap()),
    )
    .unwrap()
}

fn replace(path: &Path, bytes: &[u8]) {
    // Packet assets are intentionally read-only. Delete only the exact test asset.
    fs::remove_file(path).unwrap();
    fs::write(path, bytes).unwrap();
}

#[tokio::test]
async fn committed_packet_preserves_renames_deletions_binary_and_unusual_paths() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    repository(&source);
    fs::write(source.join("old name.txt"), "rename me exactly\n").unwrap();
    fs::write(source.join("removed.txt"), "deleted content\n").unwrap();
    fs::write(source.join("space λ\nname.txt"), "before unicode\n").unwrap();
    fs::write(source.join("binary.dat"), b"before\0binary").unwrap();
    let base = commit(&source, "base");
    fs::rename(source.join("old name.txt"), source.join("new name.txt")).unwrap();
    fs::remove_file(source.join("removed.txt")).unwrap();
    fs::write(source.join("space λ\nname.txt"), "after unicode\n").unwrap();
    fs::write(source.join("binary.dat"), b"after\0binary").unwrap();
    fs::write(source.join("added.txt"), "new content\n").unwrap();
    let head = commit(&source, "head");
    let target = target(&source, base, head);
    let snapshot = temp.path().join("snapshot");
    let diff = prepare(&snapshot, &target).await;
    assert_eq!(
        fs::read(snapshot.join(".triad-review/review.diff")).unwrap(),
        diff
    );
    let index = index(&snapshot);
    assert_eq!(index["files"].as_array().unwrap().len(), 5);
    let rename = change(&index, "old name.txt");
    assert_eq!(rename["status"], "R100");
    assert_eq!(rename["after"]["path"], "new name.txt");
    assert_eq!(asset(&snapshot, &rename["before"]), b"rename me exactly\n");
    assert_eq!(asset(&snapshot, &rename["after"]), b"rename me exactly\n");
    let deleted = change(&index, "removed.txt");
    assert_eq!(deleted["after"]["kind"], "absent");
    assert_eq!(asset(&snapshot, &deleted["before"]), b"deleted content\n");
    let added = change(&index, "added.txt");
    assert_eq!(added["before"]["kind"], "absent");
    assert_eq!(asset(&snapshot, &added["after"]), b"new content\n");
    let binary = change(&index, "binary.dat");
    assert_eq!(binary["before"]["kind"], "binary");
    assert_eq!(asset(&snapshot, &binary["after"]), b"after\0binary");
    let unicode = change(&index, "space λ\nname.txt");
    assert_eq!(asset(&snapshot, &unicode["before"]), b"before unicode\n");
    assert_eq!(asset(&snapshot, &unicode["after"]), b"after unicode\n");
    packet::verify_target(&snapshot, &target).unwrap();
    assert!(command(&source, &["status", "--porcelain"]).is_empty());

    let other = temp.path().join("other-reviewer");
    git::create_snapshot(&target, &other, "other preparation context")
        .await
        .unwrap();
    packet::install(&snapshot, &other).unwrap();
    assert_eq!(
        packet::fingerprint(&snapshot).unwrap(),
        packet::fingerprint(&other).unwrap()
    );
    assert_eq!(
        fs::read(other.join(".triad-review/context.md")).unwrap(),
        b"Frozen review context"
    );
    let mut wrong_target = target.clone();
    wrong_target.uncommitted = true;
    assert!(packet::verify_target(&snapshot, &wrong_target).is_err());
}

#[tokio::test]
async fn uncommitted_packet_uses_frozen_staged_unstaged_and_untracked_content() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    repository(&source);
    fs::write(source.join("staged.txt"), "base staged\n").unwrap();
    fs::write(source.join("unstaged.txt"), "base unstaged\n").unwrap();
    let base = commit(&source, "base");
    fs::write(source.join("staged.txt"), "new staged\n").unwrap();
    command(&source, &["add", "staged.txt"]);
    fs::write(source.join("unstaged.txt"), "new unstaged\n").unwrap();
    fs::write(source.join("untracked λ.txt"), "frozen untracked\n").unwrap();
    let mut target = target(&source, base.clone(), base);
    target.uncommitted = true;
    target.untracked_files = vec!["untracked λ.txt".into()];
    target.patch_path = Some(temp.path().join("target.patch"));
    fs::write(
        target.patch_path.as_ref().unwrap(),
        command(&source, &["diff", "--binary", "HEAD"]),
    )
    .unwrap();
    let original_status = command(&source, &["status", "--porcelain=v1", "-z"]);
    let snapshot = temp.path().join("snapshot");
    git::create_snapshot(&target, &snapshot, "context")
        .await
        .unwrap();
    let diff = git::diff_for_target(&snapshot, &target).await.unwrap();
    fs::write(source.join("untracked λ.txt"), "live checkout changed\n").unwrap();
    packet::build(&snapshot, &target, &diff, "context")
        .await
        .unwrap();
    let index = index(&snapshot);
    assert_eq!(index["files"].as_array().unwrap().len(), 3);
    assert_eq!(index["uncommitted"], true);
    assert_eq!(
        asset(&snapshot, &change(&index, "staged.txt")["after"]),
        b"new staged\n"
    );
    assert_eq!(
        asset(&snapshot, &change(&index, "unstaged.txt")["before"]),
        b"base unstaged\n"
    );
    assert_eq!(
        asset(&snapshot, &change(&index, "untracked λ.txt")["after"]),
        b"frozen untracked\n"
    );
    assert_eq!(
        command(&source, &["status", "--porcelain=v1", "-z"]),
        original_status
    );
    packet::verify(&snapshot).unwrap();
}

#[tokio::test]
async fn corrupted_missing_unreadable_or_rehashed_packet_is_detected() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    repository(&source);
    fs::write(source.join("file.txt"), "before\n").unwrap();
    let base = commit(&source, "base");
    fs::write(source.join("file.txt"), "after\n").unwrap();
    let head = commit(&source, "head");
    let target = target(&source, base, head);
    let snapshot = temp.path().join("snapshot");
    prepare(&snapshot, &target).await;
    for (number, name) in [
        "review.diff",
        "packet.json",
        "context.md",
        "before/0001.txt",
        "after/0001.txt",
        "changed-files.json",
    ]
    .iter()
    .enumerate()
    {
        let destination = temp.path().join(format!("tamper-{number}"));
        git::create_snapshot(&target, &destination, "context")
            .await
            .unwrap();
        packet::install(&snapshot, &destination).unwrap();
        let path = destination.join(".triad-review").join(name);
        let original = fs::read(&path).unwrap();
        replace(&path, b"corrupt");
        assert!(packet::verify(&destination).is_err(), "modified {name}");
        replace(&path, &original);
        packet::verify(&destination).unwrap();
        fs::remove_file(path).unwrap();
        assert!(packet::verify(&destination).is_err(), "missing {name}");
    }
    let original_fingerprint = packet::fingerprint(&snapshot).unwrap();
    let manifest_path = snapshot.join(".triad-review/packet.json");
    let mut manifest: Value = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    let bytes = b"different context";
    use sha2::{Digest, Sha256};
    manifest["assets"]["context.md"]["sha256"] = format!("{:x}", Sha256::digest(bytes)).into();
    manifest["assets"]["context.md"]["bytes"] = (bytes.len() as u64).into();
    replace(&snapshot.join(".triad-review/context.md"), bytes);
    replace(&manifest_path, &serde_json::to_vec(&manifest).unwrap());
    assert_ne!(
        packet::fingerprint(&snapshot).unwrap(),
        original_fingerprint
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let diff_path = snapshot.join(".triad-review/review.diff");
        fs::set_permissions(&diff_path, fs::Permissions::from_mode(0o0)).unwrap();
        assert!(packet::verify(&snapshot).is_err());
        fs::set_permissions(diff_path, fs::Permissions::from_mode(0o444)).unwrap();
    }
}

#[tokio::test]
async fn missing_manifest_assets_unsafe_paths_and_wrong_diff_fail_closed() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    repository(&source);
    fs::write(source.join("file.txt"), "before\n").unwrap();
    let base = commit(&source, "base");
    fs::write(source.join("file.txt"), "after\n").unwrap();
    let head = commit(&source, "head");
    let target = target(&source, base, head);
    let snapshot = temp.path().join("snapshot");
    git::create_snapshot(&target, &snapshot, "context")
        .await
        .unwrap();
    assert!(
        packet::build(&snapshot, &target, b"", "context")
            .await
            .is_err()
    );
    assert!(!snapshot.join(".triad-review/packet.json").exists());
    let diff = git::diff_for_target(&snapshot, &target).await.unwrap();
    packet::build(&snapshot, &target, &diff, "context")
        .await
        .unwrap();
    let path = snapshot.join(".triad-review/packet.json");
    let original: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let mut manifest = original.clone();
    manifest["assets"]
        .as_object_mut()
        .unwrap()
        .remove("review.diff");
    replace(&path, &serde_json::to_vec(&manifest).unwrap());
    assert!(packet::verify(&snapshot).is_err());
    for unsafe_path in [
        "../outside",
        "/outside",
        "before/../../outside",
        "before//asset",
    ] {
        let mut manifest = original.clone();
        manifest["assets"][unsafe_path] = original["assets"]["review.diff"].clone();
        replace(&path, &serde_json::to_vec(&manifest).unwrap());
        assert!(packet::verify(&snapshot).is_err());
    }
    replace(&path, &serde_json::to_vec(&original).unwrap());
    fs::write(snapshot.join(".triad-review/unexpected.txt"), "unexpected").unwrap();
    assert!(packet::verify(&snapshot).is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_inputs_are_described_without_following_targets_and_packet_links_are_rejected() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    repository(&source);
    fs::write(source.join("keep.txt"), "keep\n").unwrap();
    symlink("/definitely-not-readable-outside", source.join("link")).unwrap();
    let base = commit(&source, "base");
    fs::remove_file(source.join("link")).unwrap();
    symlink("../../another-outside", source.join("link")).unwrap();
    let head = commit(&source, "head");
    let target = target(&source, base, head);
    let snapshot = temp.path().join("snapshot");
    prepare(&snapshot, &target).await;
    let index = index(&snapshot);
    let link = change(&index, "link");
    assert_eq!(link["before"]["kind"], "symlink");
    assert_eq!(
        asset(&snapshot, &link["before"]),
        b"/definitely-not-readable-outside"
    );
    assert_eq!(asset(&snapshot, &link["after"]), b"../../another-outside");
    let diff_path = snapshot.join(".triad-review/review.diff");
    let external = temp.path().join("external.diff");
    fs::copy(&diff_path, &external).unwrap();
    fs::remove_file(&diff_path).unwrap();
    symlink(&external, &diff_path).unwrap();
    assert!(packet::verify(&snapshot).is_err());
    let other = temp.path().join("other");
    git::create_snapshot(&target, &other, "context")
        .await
        .unwrap();
    fs::remove_dir_all(other.join(".triad-review")).unwrap();
    symlink(temp.path(), other.join(".triad-review")).unwrap();
    assert!(packet::ensure_reserved_path_absent(&other).is_err());
    assert!(packet::install(&snapshot, &other).is_err());
}

#[tokio::test]
async fn tracked_reserved_paths_in_either_revision_are_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    repository(&source);
    fs::create_dir(source.join(".triad-review")).unwrap();
    fs::write(
        source.join(".triad-review/context.md"),
        "repository data, not harness data",
    )
    .unwrap();
    fs::write(source.join("keep.txt"), "keep").unwrap();
    let base = commit(&source, "base");
    assert!(packet::ensure_reserved_path_absent(&source).is_err());
    fs::remove_dir_all(source.join(".triad-review")).unwrap();
    let head = commit(&source, "head");
    let target = target(&source, base, head);
    let snapshot = temp.path().join("snapshot");
    git::create_snapshot(&target, &snapshot, "context")
        .await
        .unwrap();
    let diff = git::diff_for_target(&snapshot, &target).await.unwrap();
    assert!(
        packet::build(&snapshot, &target, &diff, "context")
            .await
            .is_err()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn uncommitted_symlinks_and_non_utf8_git_paths_are_not_followed_or_lossily_encoded() {
    use std::{io::Write, os::unix::fs::symlink, process::Stdio};
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    repository(&source);
    fs::write(source.join("keep.txt"), "before").unwrap();
    symlink("missing-base-target", source.join("link")).unwrap();
    let head = commit(&source, "base");
    fs::remove_file(source.join("link")).unwrap();
    symlink("missing-worktree-target", source.join("link")).unwrap();
    let mut target = target(&source, head.clone(), head);
    target.uncommitted = true;
    let diff = git::diff_for_target(&source, &target).await.unwrap();
    packet::build(&source, &target, &diff, "context")
        .await
        .unwrap();
    let index = index(&source);
    assert_eq!(
        asset(&source, &change(&index, "link")["after"]),
        b"missing-worktree-target"
    );
    fs::remove_dir_all(source.join(".triad-review")).unwrap();
    // Git trees can contain names the host filesystem cannot create (e.g. APFS).
    let blob = String::from_utf8(command(&source, &["rev-parse", "HEAD:keep.txt"])).unwrap();
    let mut child = Command::new("git")
        .current_dir(&source)
        .args(["update-index", "-z", "--index-info"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut record = format!("100644 {}\tf", blob.trim()).into_bytes();
    record.extend([0xff, 0]);
    child.stdin.take().unwrap().write_all(&record).unwrap();
    assert!(child.wait().unwrap().success());
    command(&source, &["commit", "-m", "non-UTF-8 path"]);
    target.head_sha = revision(&source);
    target.uncommitted = false;
    let diff = command(
        &source,
        &["diff", "--binary", &target.base_sha, &target.head_sha],
    );
    let error = packet::build(&source, &target, &diff, "context")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("non-UTF-8 Git path"));
    assert!(!source.join(".triad-review").exists());
}

#[tokio::test]
async fn submodules_are_indexed_as_commits_without_opening_their_worktrees() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    repository(&source);
    fs::write(source.join("file.txt"), "base\n").unwrap();
    let base = commit(&source, "base");
    command(
        &source,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{base},module"),
        ],
    );
    command(&source, &["commit", "-m", "add gitlink"]);
    let head = revision(&source);
    let target = target(&source, base.clone(), head);
    let snapshot = temp.path().join("snapshot");
    prepare(&snapshot, &target).await;
    let index = index(&snapshot);
    let module = change(&index, "module");
    assert_eq!(module["after"]["kind"], "submodule");
    assert_eq!(module["after"]["oid"], base);
    assert!(module["after"]["asset"].is_null());
    packet::verify(&snapshot).unwrap();
}

#[tokio::test]
async fn reducer_results_are_hashed_without_changing_core_inputs() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    repository(&source);
    fs::write(source.join("file.txt"), "before\n").unwrap();
    let base = commit(&source, "base");
    fs::write(source.join("file.txt"), "after\n").unwrap();
    let head = commit(&source, "head");
    let target = target(&source, base, head);
    let snapshot = temp.path().join("snapshot");
    prepare(&snapshot, &target).await;
    let old_fingerprint = packet::fingerprint(&snapshot).unwrap();
    let old_manifest: Value =
        serde_json::from_slice(&fs::read(snapshot.join(".triad-review/packet.json")).unwrap())
            .unwrap();
    let results = temp.path().join("results.json");
    fs::write(&results, br#"{"reports":[]}"#).unwrap();
    packet::attach_provider_results(&snapshot, &results).unwrap();
    packet::verify_target(&snapshot, &target).unwrap();
    assert_ne!(packet::fingerprint(&snapshot).unwrap(), old_fingerprint);
    let new_manifest: Value =
        serde_json::from_slice(&fs::read(snapshot.join(".triad-review/packet.json")).unwrap())
            .unwrap();
    for (name, asset) in old_manifest["assets"].as_object().unwrap() {
        assert_eq!(&new_manifest["assets"][name], asset);
    }
    assert_eq!(
        fs::read(snapshot.join(".triad-review/provider-results.json")).unwrap(),
        br#"{"reports":[]}"#
    );
    assert!(packet::attach_provider_results(&snapshot, &results).is_err());
    replace(
        &snapshot.join(".triad-review/provider-results.json"),
        br#"{"reports":["changed"]}"#,
    );
    assert!(packet::verify(&snapshot).is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn file_type_changes_are_explicit_and_symlinked_asset_directories_fail_closed() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    repository(&source);
    fs::write(source.join("was-regular.txt"), "original file content\n").unwrap();
    symlink("missing-target", source.join("was-link.txt")).unwrap();
    let base = commit(&source, "base");
    fs::remove_file(source.join("was-regular.txt")).unwrap();
    symlink("missing-new-target", source.join("was-regular.txt")).unwrap();
    fs::remove_file(source.join("was-link.txt")).unwrap();
    fs::write(source.join("was-link.txt"), "new regular content\n").unwrap();
    let head = commit(&source, "head");
    let target = target(&source, base, head);
    let snapshot = temp.path().join("snapshot");
    prepare(&snapshot, &target).await;
    let index = index(&snapshot);
    assert_eq!(change(&index, "was-regular.txt")["status"], "T");
    assert_eq!(change(&index, "was-regular.txt")["before"]["kind"], "text");
    assert_eq!(
        change(&index, "was-regular.txt")["after"]["kind"],
        "symlink"
    );
    assert_eq!(change(&index, "was-link.txt")["before"]["kind"], "symlink");
    assert_eq!(change(&index, "was-link.txt")["after"]["kind"], "text");
    let before = snapshot.join(".triad-review/before");
    let external = temp.path().join("external-before");
    fs::rename(&before, &external).unwrap();
    symlink(&external, &before).unwrap();
    assert!(packet::verify(&snapshot).is_err());
}
