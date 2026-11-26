// Modified by RenCrow Switch Core, 2026-11-26: bounded artifact snapshots and fingerprints.
use super::ArtifactBindingV1;
use super::ArtifactEntryV1;
use super::ArtifactSetV1;
use anyhow::Context;
use anyhow::Result;
use serde::Serialize;
use sha2::Digest;
use std::collections::BTreeSet;
use std::fs;
use std::fs::OpenOptions;
use std::io::Read;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

#[derive(Serialize)]
struct ArtifactFingerprintV1 {
    format: &'static str,
    workspace_binding_ref: String,
    scope_revision: u64,
    git_commit_ref: Option<String>,
    entries: Vec<ArtifactFingerprintEntryV1>,
}

#[derive(Serialize)]
struct ArtifactFingerprintEntryV1 {
    path: String,
    kind: &'static str,
    executable: Option<bool>,
    sha256: Option<String>,
}

struct PreparedEntry<'a> {
    normalized_path: String,
    requested_path: &'a Path,
}

pub(super) fn for_canonical_root(root: &Path) -> Result<ArtifactBindingV1> {
    let canonical_root = fs::canonicalize(root).context("canonicalize artifact workspace root")?;
    let workspace_binding_ref = canonical_root
        .to_str()
        .context("workspace root is not valid UTF-8")?
        .to_owned();
    Ok(ArtifactBindingV1 {
        workspace_binding_ref,
        scope_revision: 1,
        git_commit_ref: None,
    })
}

pub(super) fn capture_bound(
    root: &Path,
    paths: &[PathBuf],
    binding: &ArtifactBindingV1,
) -> Result<ArtifactSetV1> {
    anyhow::ensure!(
        !binding.workspace_binding_ref.trim().is_empty() && binding.scope_revision > 0,
        "artifact binding is incomplete"
    );
    anyhow::ensure!(
        !paths.is_empty() && paths.len() <= super::MAX_ARTIFACT_FILES,
        "artifact set must contain between 1 and {} paths",
        super::MAX_ARTIFACT_FILES
    );
    let canonical_root = fs::canonicalize(root).context("canonicalize artifact workspace root")?;
    anyhow::ensure!(
        canonical_root.is_dir(),
        "artifact workspace root is not a directory"
    );

    let mut prepared = paths
        .iter()
        .map(|path| {
            Ok(PreparedEntry {
                normalized_path: normalize_relative_path(path)?,
                requested_path: path,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    prepared.sort_by(|left, right| {
        left.normalized_path
            .as_bytes()
            .cmp(right.normalized_path.as_bytes())
    });
    for pair in prepared.windows(2) {
        anyhow::ensure!(
            pair[0].normalized_path != pair[1].normalized_path,
            "artifact paths collide after canonical normalization"
        );
    }

    let entries = prepared
        .iter()
        .map(|entry| capture_entry(&canonical_root, entry))
        .collect::<Result<Vec<_>>>()?;
    let digest = fingerprint_digest(binding, &entries)?;
    Ok(ArtifactSetV1 {
        root: canonical_root,
        binding: binding.clone(),
        entries,
        digest,
    })
}

pub(super) fn matches_workspace(artifact_set: &ArtifactSetV1) -> Result<bool> {
    let persisted_digest = fingerprint_digest(&artifact_set.binding, &artifact_set.entries)?;
    anyhow::ensure!(
        persisted_digest == artifact_set.digest,
        "artifact set digest does not match its binding and entries"
    );
    let canonical_root = fs::canonicalize(&artifact_set.root)
        .context("canonicalize artifact workspace root for comparison")?;
    anyhow::ensure!(
        canonical_root == artifact_set.root,
        "artifact workspace root changed after capture"
    );
    let paths = artifact_set
        .entries
        .iter()
        .map(|entry| entry.path.clone())
        .collect::<Vec<_>>();
    let current = capture_bound(&canonical_root, &paths, &artifact_set.binding)?;
    Ok(current.entries == artifact_set.entries && current.digest == artifact_set.digest)
}

#[cfg(test)]
pub(super) fn canonical_fingerprint_json(
    binding: &ArtifactBindingV1,
    entries: &[ArtifactEntryV1],
) -> Result<String> {
    serde_json::to_string(&artifact_fingerprint_v1(binding, entries)?)
        .context("serialize artifact fingerprint")
}

fn capture_entry(root: &Path, prepared: &PreparedEntry<'_>) -> Result<ArtifactEntryV1> {
    let mut current = root.to_path_buf();
    let components = prepared.requested_path.components().collect::<Vec<_>>();
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            anyhow::bail!("artifact path contains a non-normal component");
        };
        current.push(name);
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                anyhow::ensure!(
                    !metadata.file_type().is_symlink(),
                    "artifact path contains a symlink"
                );
                if index + 1 < components.len() {
                    anyhow::ensure!(metadata.is_dir(), "artifact path parent is not a directory");
                } else {
                    return capture_file(root, &current, &prepared.normalized_path, metadata);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ArtifactEntryV1 {
                    path: PathBuf::from(&prepared.normalized_path),
                    present: false,
                    sha256: None,
                    executable: false,
                });
            }
            Err(error) => return Err(error).context("inspect artifact path component"),
        }
    }
    anyhow::bail!("artifact path is empty")
}

fn capture_file(
    root: &Path,
    path: &Path,
    normalized_path: &str,
    before_open: fs::Metadata,
) -> Result<ArtifactEntryV1> {
    anyhow::ensure!(before_open.is_file(), "artifact path is not a regular file");
    anyhow::ensure!(
        before_open.len() <= super::MAX_ARTIFACT_BYTES,
        "artifact file exceeds {} bytes",
        super::MAX_ARTIFACT_BYTES
    );
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let mut file = options
        .open(path)
        .context("open artifact file without following links")?;
    let opened_metadata = file.metadata().context("read opened artifact metadata")?;
    anyhow::ensure!(
        opened_metadata.is_file(),
        "opened artifact is not a regular file"
    );
    ensure_same_file(&before_open, &opened_metadata)?;
    let resolved = fs::canonicalize(path).context("canonicalize artifact file")?;
    anyhow::ensure!(
        resolved.starts_with(root),
        "artifact path resolves outside workspace root"
    );
    let mut bytes = Vec::new();
    file.by_ref()
        .take(super::MAX_ARTIFACT_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("read artifact file")?;
    anyhow::ensure!(
        bytes.len() as u64 <= super::MAX_ARTIFACT_BYTES,
        "artifact file exceeds {} bytes",
        super::MAX_ARTIFACT_BYTES
    );
    let after_read = file
        .metadata()
        .context("recheck opened artifact metadata")?;
    ensure_same_file(&opened_metadata, &after_read)?;
    anyhow::ensure!(
        after_read.len() == bytes.len() as u64,
        "artifact file changed while being read"
    );
    let path_after_read = fs::symlink_metadata(path).context("recheck artifact path")?;
    anyhow::ensure!(
        !path_after_read.file_type().is_symlink(),
        "artifact path became a symlink"
    );
    ensure_same_file(&after_read, &path_after_read)?;
    Ok(ArtifactEntryV1 {
        path: PathBuf::from(normalized_path),
        present: true,
        sha256: Some(format!("{:x}", sha2::Sha256::digest(&bytes))),
        executable: is_executable(&after_read),
    })
}

#[cfg(unix)]
fn ensure_same_file(left: &fs::Metadata, right: &fs::Metadata) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    anyhow::ensure!(
        left.dev() == right.dev()
            && left.ino() == right.ino()
            && left.len() == right.len()
            && left.mtime() == right.mtime()
            && left.mtime_nsec() == right.mtime_nsec()
            && left.ctime() == right.ctime()
            && left.ctime_nsec() == right.ctime_nsec(),
        "artifact file changed while being inspected"
    );
    Ok(())
}

#[cfg(not(unix))]
fn ensure_same_file(left: &fs::Metadata, right: &fs::Metadata) -> Result<()> {
    anyhow::ensure!(
        left.len() == right.len() && left.modified().ok() == right.modified().ok(),
        "artifact file changed while being inspected"
    );
    Ok(())
}

#[cfg(unix)]
fn is_executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_metadata: &fs::Metadata) -> bool {
    false
}

pub(super) fn normalize_relative_path(path: &Path) -> Result<String> {
    anyhow::ensure!(!path.is_absolute(), "artifact path must be relative");
    let components = path
        .components()
        .map(|component| match component {
            Component::Normal(name) => name
                .to_str()
                .map(str::to_owned)
                .context("artifact path must be valid UTF-8"),
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => {
                anyhow::bail!("artifact path contains a non-normal component")
            }
        })
        .collect::<Result<Vec<_>>>()?;
    anyhow::ensure!(!components.is_empty(), "artifact path must not be empty");
    Ok(components.join("/"))
}

pub(super) fn content_sha256(content: &[u8]) -> String {
    format!("{:x}", sha2::Sha256::digest(content))
}

fn artifact_fingerprint_v1(
    binding: &ArtifactBindingV1,
    entries: &[ArtifactEntryV1],
) -> Result<ArtifactFingerprintV1> {
    anyhow::ensure!(
        !binding.workspace_binding_ref.trim().is_empty() && binding.scope_revision > 0,
        "artifact binding is incomplete"
    );
    let mut fingerprint_entries = entries
        .iter()
        .map(|entry| {
            let path = normalize_relative_path(&entry.path)?;
            anyhow::ensure!(
                entry.path.as_path() == Path::new(path.as_str()),
                "artifact entry path is not in canonical form"
            );
            if entry.present {
                let hash = entry
                    .sha256
                    .as_deref()
                    .context("file artifact entry requires a content hash")?;
                anyhow::ensure!(is_sha256_hex(hash), "artifact content hash is malformed");
            } else {
                anyhow::ensure!(
                    entry.sha256.is_none(),
                    "absent artifact entry cannot have a content hash"
                );
            }
            Ok(ArtifactFingerprintEntryV1 {
                path,
                kind: if entry.present { "file" } else { "absent" },
                executable: entry.present.then_some(entry.executable),
                sha256: entry.sha256.clone(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    fingerprint_entries.sort_by(|left, right| left.path.as_bytes().cmp(right.path.as_bytes()));
    let mut unique_paths = BTreeSet::new();
    for entry in &fingerprint_entries {
        anyhow::ensure!(unique_paths.insert(&entry.path), "duplicate artifact path");
    }
    Ok(ArtifactFingerprintV1 {
        format: "common-harness-artifact-set/1",
        workspace_binding_ref: binding.workspace_binding_ref.clone(),
        scope_revision: binding.scope_revision,
        git_commit_ref: binding.git_commit_ref.clone(),
        entries: fingerprint_entries,
    })
}

fn is_sha256_hex(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(super) fn fingerprint_digest(
    binding: &ArtifactBindingV1,
    entries: &[ArtifactEntryV1],
) -> Result<String> {
    codex_history::compaction_candidate::digest(&artifact_fingerprint_v1(binding, entries)?)
        .map_err(anyhow::Error::msg)
}
