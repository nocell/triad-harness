//! Frozen, tool-readable review inputs shared by every stage of a run.
use crate::model::GitTarget;
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Component, Path},
    process::Command,
};

const DIRECTORY: &str = ".triad-review";
const MANIFEST: &str = "packet.json";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    format_version: u32,
    base_sha: String,
    head_sha: String,
    uncommitted: bool,
    assets: BTreeMap<String, Asset>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Asset {
    sha256: String,
    bytes: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Index {
    base_sha: String,
    head_sha: String,
    uncommitted: bool,
    files: Vec<Change>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Change {
    status: String,
    before: Side,
    after: Side,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Side {
    path: Option<String>,
    mode: Option<String>,
    oid: Option<String>,
    kind: String,
    asset: Option<String>,
}

/// Guard before writing any harness files into a newly created snapshot.
pub fn ensure_reserved_path_absent(snapshot: &Path) -> Result<()> {
    ensure!(
        fs::symlink_metadata(snapshot.join(DIRECTORY))
            .is_err_and(|error| { error.kind() == std::io::ErrorKind::NotFound }),
        "reserved review packet path {DIRECTORY} already exists in reviewed code"
    );
    ensure!(
        git(snapshot, &["ls-files", "-z", "--", DIRECTORY])?.is_empty(),
        "reviewed code tracks reserved review packet path {DIRECTORY}"
    );
    Ok(())
}

/// Build only from the frozen snapshot. No source-checkout files are read.
pub async fn build(snapshot: &Path, target: &GitTarget, diff: &[u8], context: &str) -> Result<()> {
    validate_revision(&target.base_sha)?;
    validate_revision(&target.head_sha)?;
    ensure_head(snapshot, &target.head_sha)?;
    reject_tracked_reserved(snapshot, &target.base_sha, &target.head_sha)?;
    check_destination(snapshot)?;

    let revisions = if target.uncommitted {
        vec![target.head_sha.as_str()]
    } else {
        vec![target.base_sha.as_str(), target.head_sha.as_str()]
    };
    let mut diff_args = vec!["diff", "--binary"];
    diff_args.extend(&revisions);
    ensure!(
        git(snapshot, &diff_args)? == diff,
        "review diff does not match the frozen snapshot and target revisions"
    );
    let mut raw_args = vec!["diff", "--raw", "-z", "--no-abbrev"];
    raw_args.extend(revisions);
    let raw = git(snapshot, &raw_args)?;
    let records: Vec<&[u8]> = raw.split(|byte| *byte == 0).collect();
    let staging = tempfile::Builder::new()
        .prefix(".triad-packet-")
        .tempdir_in(snapshot)?;
    let mut assets = BTreeMap::new();
    write_asset(
        staging.path(),
        "context.md",
        context.as_bytes(),
        &mut assets,
    )?;
    write_asset(staging.path(), "review.diff", diff, &mut assets)?;
    let mut files = Vec::new();
    let mut cursor = 0;
    while cursor < records.len() && !records[cursor].is_empty() {
        let header = std::str::from_utf8(records[cursor])?;
        let fields: Vec<&str> = header.trim_start_matches(':').split(' ').collect();
        ensure!(
            header.starts_with(':') && fields.len() == 5,
            "invalid Git change record"
        );
        cursor += 1;
        let old_path = read_path(records.get(cursor).copied().context("missing Git path")?)?;
        cursor += 1;
        let status = fields[4];
        ensure!(
            matches!(
                status.as_bytes().first(),
                Some(b'A' | b'C' | b'D' | b'M' | b'R' | b'T')
            ),
            "unsupported Git change status {status}"
        );
        let new_path = if status.starts_with(['R', 'C']) {
            let value = read_path(
                records
                    .get(cursor)
                    .copied()
                    .context("missing renamed Git path")?,
            )?;
            cursor += 1;
            value
        } else {
            old_path.clone()
        };
        let number = files.len() + 1;
        let before = side(
            snapshot,
            staging.path(),
            "before",
            number,
            &old_path,
            fields[0],
            fields[2],
            false,
            &mut assets,
        )?;
        let after = side(
            snapshot,
            staging.path(),
            "after",
            number,
            &new_path,
            fields[1],
            fields[3],
            target.uncommitted,
            &mut assets,
        )?;
        files.push(Change {
            status: status.into(),
            before,
            after,
        });
    }
    let index = Index {
        base_sha: target.base_sha.clone(),
        head_sha: target.head_sha.clone(),
        uncommitted: target.uncommitted,
        files,
    };
    write_asset(
        staging.path(),
        "changed-files.json",
        &serde_json::to_vec_pretty(&index)?,
        &mut assets,
    )?;
    let manifest = Manifest {
        format_version: 1,
        base_sha: target.base_sha.clone(),
        head_sha: target.head_sha.clone(),
        uncommitted: target.uncommitted,
        assets,
    };
    write_readonly(
        &staging.path().join(MANIFEST),
        &serde_json::to_vec_pretty(&manifest)?,
    )?;
    replace_context_directory(snapshot, staging.path())?;
    verify_target(snapshot, target)
}

/// Install byte-identical inputs; never reconstruct them from the live checkout.
pub fn install(source_snapshot: &Path, destination_snapshot: &Path) -> Result<()> {
    verify(source_snapshot)?;
    let source = source_snapshot.join(DIRECTORY);
    let manifest = read_manifest(&source)?;
    ensure_head(destination_snapshot, &manifest.head_sha)?;
    reject_tracked_reserved(destination_snapshot, &manifest.base_sha, &manifest.head_sha)?;
    check_destination(destination_snapshot)?;
    let staging = tempfile::Builder::new()
        .prefix(".triad-packet-")
        .tempdir_in(destination_snapshot)?;
    for name in manifest.assets.keys().map(String::as_str).chain([MANIFEST]) {
        let bytes = safe_read(&source, name)?;
        write_readonly(&staging.path().join(name), &bytes)?;
    }
    replace_context_directory(destination_snapshot, staging.path())?;
    ensure!(
        fingerprint(source_snapshot)? == fingerprint(destination_snapshot)?,
        "packet changed while copying"
    );
    Ok(())
}

/// Add the reducer's candidate reports before execution, keeping core inputs intact.
pub fn attach_provider_results(snapshot: &Path, results_path: &Path) -> Result<()> {
    verify(snapshot)?;
    let metadata = fs::symlink_metadata(results_path)?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "unsafe provider results input"
    );
    let bytes = fs::read(results_path)?;
    let _: serde_json::Value =
        serde_json::from_slice(&bytes).context("invalid provider results JSON")?;
    let root = snapshot.join(DIRECTORY);
    let mut manifest = read_manifest(&root)?;
    ensure!(
        !manifest.assets.contains_key("provider-results.json"),
        "provider results already attached"
    );
    write_asset(&root, "provider-results.json", &bytes, &mut manifest.assets)?;
    crate::storage::atomic_write(&root.join(MANIFEST), &serde_json::to_vec_pretty(&manifest)?)?;
    let mut permissions = fs::metadata(root.join(MANIFEST))?.permissions();
    permissions.set_readonly(true);
    fs::set_permissions(root.join(MANIFEST), permissions)?;
    verify(snapshot)
}

/// Fail closed if a required input is missing, altered, unreadable, or symlinked.
pub fn verify(snapshot: &Path) -> Result<()> {
    let root = snapshot.join(DIRECTORY);
    let manifest = read_manifest(&root)?;
    ensure!(
        manifest.format_version == 1,
        "unsupported review packet format"
    );
    validate_revision(&manifest.base_sha)?;
    validate_revision(&manifest.head_sha)?;
    ensure_head(snapshot, &manifest.head_sha)?;
    for required in ["context.md", "review.diff", "changed-files.json"] {
        ensure!(
            manifest.assets.contains_key(required),
            "review packet missing {required}"
        );
    }
    for (name, asset) in &manifest.assets {
        validate_asset_path(name)?;
        let bytes = safe_read(&root, name)?;
        ensure!(
            bytes.len() as u64 == asset.bytes && digest(&bytes) == asset.sha256,
            "review packet integrity mismatch: {name}"
        );
    }
    let mut actual = BTreeSet::new();
    inventory(&root, &root, &mut actual)?;
    let expected: BTreeSet<String> = manifest
        .assets
        .keys()
        .cloned()
        .chain([MANIFEST.into()])
        .collect();
    ensure!(
        actual == expected,
        "review packet contains unexpected or missing assets"
    );
    let index: Index = serde_json::from_slice(&safe_read(&root, "changed-files.json")?)?;
    ensure!(
        index.base_sha == manifest.base_sha
            && index.head_sha == manifest.head_sha
            && index.uncommitted == manifest.uncommitted,
        "review packet revision metadata mismatch"
    );
    for change in &index.files {
        for side in [&change.before, &change.after] {
            if let Some(path) = &side.path {
                validate_repo_path(path)?;
            }
            if let Some(asset) = &side.asset {
                ensure!(
                    manifest.assets.contains_key(asset),
                    "indexed review input missing: {asset}"
                );
            }
        }
    }
    Ok(())
}

pub fn verify_target(snapshot: &Path, target: &GitTarget) -> Result<()> {
    verify(snapshot)?;
    let manifest = read_manifest(&snapshot.join(DIRECTORY))?;
    ensure!(
        manifest.base_sha == target.base_sha
            && manifest.head_sha == target.head_sha
            && manifest.uncommitted == target.uncommitted,
        "review packet does not match the requested target"
    );
    Ok(())
}

/// Retain outside the snapshot before provider execution to detect rehashed edits.
pub fn fingerprint(snapshot: &Path) -> Result<String> {
    verify(snapshot)?;
    Ok(digest(&safe_read(&snapshot.join(DIRECTORY), MANIFEST)?))
}

#[allow(clippy::too_many_arguments)]
fn side(
    snapshot: &Path,
    staging: &Path,
    direction: &str,
    number: usize,
    path: &str,
    mode: &str,
    oid: &str,
    working_tree: bool,
    assets: &mut BTreeMap<String, Asset>,
) -> Result<Side> {
    if mode == "000000" {
        return Ok(Side {
            path: None,
            mode: None,
            oid: None,
            kind: "absent".into(),
            asset: None,
        });
    }
    ensure!(
        matches!(mode, "100644" | "100755" | "120000" | "160000"),
        "unsupported file mode {mode}"
    );
    let mut result = Side {
        path: Some(path.into()),
        mode: Some(mode.into()),
        oid: (!oid.bytes().all(|byte| byte == b'0')).then(|| oid.into()),
        kind: "submodule".into(),
        asset: None,
    };
    if mode == "160000" {
        return Ok(result);
    }
    let bytes = if working_tree {
        safe_worktree_read(snapshot, path, mode)?
    } else {
        validate_revision(oid)?;
        git(snapshot, &["cat-file", "blob", oid])?
    };
    let is_text = !bytes.contains(&0) && std::str::from_utf8(&bytes).is_ok();
    result.kind = if mode == "120000" {
        "symlink"
    } else if is_text {
        "text"
    } else {
        "binary"
    }
    .into();
    let extension = if is_text { "txt" } else { "bin" };
    let name = format!("{direction}/{number:04}.{extension}");
    write_asset(staging, &name, &bytes, assets)?;
    result.asset = Some(name);
    Ok(result)
}

fn safe_worktree_read(snapshot: &Path, path: &str, mode: &str) -> Result<Vec<u8>> {
    validate_repo_path(path)?;
    let mut current = snapshot.to_path_buf();
    let components: Vec<_> = Path::new(path).components().collect();
    for (index, component) in components.iter().enumerate() {
        current.push(component.as_os_str());
        let metadata = fs::symlink_metadata(&current)?;
        if index + 1 != components.len() {
            ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "unsafe snapshot path {path}"
            );
        } else if mode == "120000" {
            ensure!(
                metadata.file_type().is_symlink(),
                "snapshot symlink changed: {path}"
            );
            #[cfg(unix)]
            {
                use std::os::unix::ffi::OsStrExt;
                return Ok(fs::read_link(&current)?.as_os_str().as_bytes().to_vec());
            }
            #[cfg(not(unix))]
            bail!("symlink review packets require Unix");
        } else {
            ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "unsafe snapshot file {path}"
            );
            return Ok(fs::read(&current)?);
        }
    }
    bail!("empty snapshot path")
}

fn read_path(bytes: &[u8]) -> Result<String> {
    let path = std::str::from_utf8(bytes)
        .context("review packet cannot represent a non-UTF-8 Git path")?;
    validate_repo_path(path)?;
    ensure!(
        path != DIRECTORY && !path.starts_with(&format!("{DIRECTORY}/")),
        "reserved review packet path changed in target"
    );
    Ok(path.into())
}

fn validate_repo_path(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty()
            && !path.contains('\0')
            && !path.starts_with('/')
            && path
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != "..")
            && Path::new(path)
                .components()
                .all(|part| matches!(part, Component::Normal(_))),
        "unsafe review path"
    );
    Ok(())
}

fn validate_asset_path(path: &str) -> Result<()> {
    validate_repo_path(path)?;
    ensure!(path != MANIFEST, "manifest cannot hash itself");
    ensure!(
        matches!(
            path,
            "context.md" | "review.diff" | "changed-files.json" | "provider-results.json"
        ) || ((path.starts_with("before/") || path.starts_with("after/"))
            && path.split('/').count() == 2),
        "unexpected review asset path"
    );
    Ok(())
}

fn validate_revision(sha: &str) -> Result<()> {
    ensure!(
        (sha.len() == 40 || sha.len() == 64) && sha.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid review revision"
    );
    Ok(())
}

fn ensure_head(snapshot: &Path, expected: &str) -> Result<()> {
    let head = git(snapshot, &["rev-parse", "HEAD"])?;
    ensure!(
        std::str::from_utf8(&head)?.trim() == expected,
        "snapshot HEAD does not match review packet"
    );
    Ok(())
}

fn reject_tracked_reserved(snapshot: &Path, base: &str, head: &str) -> Result<()> {
    ensure!(
        git(snapshot, &["ls-files", "-z", "--", DIRECTORY])?.is_empty(),
        "reviewed code tracks reserved review packet path"
    );
    for revision in [base, head] {
        ensure!(
            git(
                snapshot,
                &[
                    "ls-tree",
                    "-r",
                    "--name-only",
                    "-z",
                    revision,
                    "--",
                    DIRECTORY
                ]
            )?
            .is_empty(),
            "target revision contains reserved review packet path"
        );
    }
    Ok(())
}

fn check_destination(snapshot: &Path) -> Result<()> {
    let root = snapshot.join(DIRECTORY);
    let metadata = match fs::symlink_metadata(&root) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "unsafe existing review packet directory"
    );
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        ensure!(
            entry.file_name() == "context.md" && entry.file_type()?.is_file(),
            "review packet destination already contains data"
        );
    }
    Ok(())
}

fn replace_context_directory(snapshot: &Path, staging: &Path) -> Result<()> {
    check_destination(snapshot)?;
    let destination = snapshot.join(DIRECTORY);
    if destination.exists() {
        fs::remove_dir_all(&destination)?;
    }
    fs::rename(staging, destination)?;
    Ok(())
}

fn write_asset(
    root: &Path,
    name: &str,
    bytes: &[u8],
    assets: &mut BTreeMap<String, Asset>,
) -> Result<()> {
    write_readonly(&root.join(name), bytes)?;
    assets.insert(
        name.into(),
        Asset {
            sha256: digest(bytes),
            bytes: bytes.len() as u64,
        },
    );
    Ok(())
}

fn write_readonly(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, bytes)?;
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_readonly(true);
    fs::set_permissions(path, permissions)?;
    Ok(())
}

fn safe_read(root: &Path, name: &str) -> Result<Vec<u8>> {
    validate_repo_path(name)?;
    let metadata = fs::symlink_metadata(root).context("review packet directory is missing")?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "unsafe review packet directory"
    );
    let mut current = root.to_path_buf();
    let components: Vec<_> = Path::new(name).components().collect();
    for (index, component) in components.iter().enumerate() {
        current.push(component.as_os_str());
        let metadata = fs::symlink_metadata(&current)
            .with_context(|| format!("review packet input missing: {name}"))?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "symlink in review packet: {name}"
        );
        if index + 1 != components.len() {
            ensure!(metadata.is_dir(), "invalid review packet directory: {name}");
        } else {
            ensure!(metadata.is_file(), "invalid review packet input: {name}");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                ensure!(
                    metadata.permissions().mode() & 0o444 != 0,
                    "unreadable review packet input: {name}"
                );
            }
        }
    }
    fs::read(current).with_context(|| format!("cannot read review packet input {name}"))
}

fn inventory(root: &Path, directory: &Path, files: &mut BTreeSet<String>) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        ensure!(!kind.is_symlink(), "symlink in review packet");
        if kind.is_dir() {
            let name = entry.file_name();
            ensure!(
                directory == root && (name == "before" || name == "after"),
                "unexpected packet directory"
            );
            inventory(root, &entry.path(), files)?;
        } else {
            ensure!(kind.is_file(), "unsupported review packet asset");
            files.insert(
                entry
                    .path()
                    .strip_prefix(root)?
                    .to_str()
                    .context("non-UTF-8 review asset")?
                    .into(),
            );
        }
    }
    Ok(())
}

fn read_manifest(root: &Path) -> Result<Manifest> {
    serde_json::from_slice(&safe_read(root, MANIFEST)?).context("invalid review packet manifest")
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn git(snapshot: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .current_dir(snapshot)
        .args(args)
        .output()?;
    ensure!(
        output.status.success(),
        "Git review input command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output.stdout)
}
