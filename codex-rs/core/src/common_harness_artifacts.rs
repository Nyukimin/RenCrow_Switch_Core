// Modified by RenCrow Switch Core, 2026-11-26: store immutable host operation artifacts.
//! Private operation payloads referenced by the existing durable task state.

use codex_protocol::ThreadId;
use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::Digest;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;

const MAX_OPERATION_ARTIFACT_BYTES: usize = 8 * 1024 * 1024;
const OPERATION_ARTIFACT_REF_PREFIX: &str = "common-harness-artifact/1:sha256:";
const OPERATION_ARTIFACT_DIRECTORY: &str = "common-harness-artifacts";

fn encode_operation_artifact(payload: &impl Serialize) -> Result<(String, String), String> {
    let encoded = serde_json::to_string(payload).map_err(|error| error.to_string())?;
    if encoded.len() > MAX_OPERATION_ARTIFACT_BYTES {
        return Err("operation artifact exceeds its storage limit".into());
    }
    let digest = format!("{:x}", sha2::Sha256::digest(encoded.as_bytes()));
    Ok((encoded, digest))
}

/// Reserve the address of a known future host marker without publishing it.
pub(crate) fn predict_operation_artifact_ref(payload: &impl Serialize) -> Result<String, String> {
    let (_, digest) = encode_operation_artifact(payload)?;
    Ok(format!("{OPERATION_ARTIFACT_REF_PREFIX}{digest}"))
}

/// Write bounded, content-addressed host data before publishing an operation intent.
pub(crate) fn write_operation_artifact(
    codex_home: &Path,
    thread_id: ThreadId,
    payload: &impl Serialize,
) -> Result<String, String> {
    let (encoded, digest) = encode_operation_artifact(payload)?;
    let directory = artifact_directory(codex_home, thread_id, true)?;
    let path = directory.join(format!("{digest}.json"));
    match std::fs::symlink_metadata(&path) {
        Ok(_) => {
            let existing = read_verified_artifact(&path, &digest)?;
            if existing != encoded.as_bytes() {
                return Err("immutable operation artifact has conflicting contents".into());
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            codex_utils_path::write_atomically(&path, &encoded)
                .map_err(|error| format!("write operation artifact: {error}"))?;
            File::open(&path)
                .and_then(|file| file.sync_all())
                .map_err(|error| format!("sync operation artifact: {error}"))?;
            #[cfg(unix)]
            File::open(&directory)
                .and_then(|directory| directory.sync_all())
                .map_err(|error| format!("sync operation artifact directory: {error}"))?;
            read_verified_artifact(&path, &digest)?;
        }
        Err(error) => return Err(format!("inspect operation artifact: {error}")),
    }
    Ok(format!("{OPERATION_ARTIFACT_REF_PREFIX}{digest}"))
}

/// Resolve only a digest reference inside this thread's private host artifact storage.
pub(crate) fn load_operation_artifact<T: DeserializeOwned>(
    codex_home: &Path,
    thread_id: ThreadId,
    reference: &str,
) -> Result<T, String> {
    let digest = reference
        .strip_prefix(OPERATION_ARTIFACT_REF_PREFIX)
        .filter(|digest| {
            digest.len() == 64
                && digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
        .ok_or_else(|| "invalid operation artifact reference".to_string())?;
    let directory = artifact_directory(codex_home, thread_id, false)?;
    let bytes = read_verified_artifact(&directory.join(format!("{digest}.json")), digest)?;
    serde_json::from_slice(&bytes).map_err(|error| format!("decode operation artifact: {error}"))
}

fn artifact_directory(
    codex_home: &Path,
    thread_id: ThreadId,
    create: bool,
) -> Result<PathBuf, String> {
    let mut directory = codex_home
        .canonicalize()
        .map_err(|error| format!("resolve host artifact storage: {error}"))?;
    for component in [
        OPERATION_ARTIFACT_DIRECTORY.to_owned(),
        thread_id.to_string(),
    ] {
        directory.push(component);
        if create {
            match std::fs::create_dir(&directory) {
                Ok(()) => {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        std::fs::set_permissions(
                            &directory,
                            std::fs::Permissions::from_mode(0o700),
                        )
                        .map_err(|error| format!("protect host artifact directory: {error}"))?;
                        let parent = directory
                            .parent()
                            .ok_or_else(|| "host artifact parent is missing".to_string())?;
                        File::open(parent)
                            .and_then(|parent| parent.sync_all())
                            .map_err(|error| format!("sync host artifact parent: {error}"))?;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(format!("create host artifact directory: {error}")),
            }
        }
        let metadata = std::fs::symlink_metadata(&directory)
            .map_err(|error| format!("inspect host artifact directory: {error}"))?;
        if !metadata.file_type().is_dir() {
            return Err("host artifact storage must contain real directories".into());
        }
    }
    Ok(directory)
}

fn read_verified_artifact(path: &Path, digest: &str) -> Result<Vec<u8>, String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("inspect stored operation artifact: {error}"))?;
    if !metadata.file_type().is_file() || metadata.len() > MAX_OPERATION_ARTIFACT_BYTES as u64 {
        return Err("stored operation artifact is not a bounded regular file".into());
    }
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|file| {
            file.take(MAX_OPERATION_ARTIFACT_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
        })
        .map_err(|error| format!("read stored operation artifact: {error}"))?;
    if bytes.len() > MAX_OPERATION_ARTIFACT_BYTES
        || format!("{:x}", sha2::Sha256::digest(&bytes)) != digest
    {
        return Err("stored operation artifact digest mismatch".into());
    }
    Ok(bytes)
}

#[cfg(test)]
#[path = "common_harness_artifacts_tests.rs"]
mod tests;
