// Modified by RenCrow Switch Core, 2026-11-26: add sealed local rollout writer proof.

use std::path::Path;
use std::path::PathBuf;

use anyhow::Result;
use codex_protocol::ThreadId;
use codex_thread_store::LocalThreadStore;
use serde::Serialize;

use super::Session;

/// Evidence that this session currently owns a durable local rollout writer.
///
/// Instances are created only after checking the live local store, thread,
/// canonical rollout path, CODEX_HOME boundary, and a successful writer flush.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(super) struct VerifiedLocalWriterProof {
    thread_id: ThreadId,
    rollout_path: PathBuf,
    codex_home: PathBuf,
}

impl VerifiedLocalWriterProof {
    pub(super) fn thread_id(&self) -> ThreadId {
        self.thread_id
    }

    pub(super) fn rollout_path(&self) -> &Path {
        &self.rollout_path
    }

    pub(super) fn codex_home(&self) -> &Path {
        &self.codex_home
    }
}

/// Confirms the current session has a durable writer in its configured local store.
///
/// This must be called at the consumer's current turn, immediately before relying
/// on the proof. The proof is an in-memory observation and is not a substitute for
/// the consumer's durable operation fence.
pub(super) async fn verify_local_writer(session: &Session) -> Result<VerifiedLocalWriterProof> {
    let config = session.get_config().await;
    let codex_home = config
        .codex_home
        .as_path()
        .canonicalize()
        .map_err(|error| anyhow::anyhow!("resolve CODEX_HOME for local writer proof: {error}"))?;
    if !codex_home.is_dir() {
        anyhow::bail!("CODEX_HOME for local writer proof is not a directory");
    }

    let local_store = session
        .services
        .thread_store
        .as_any()
        .downcast_ref::<LocalThreadStore>()
        .ok_or_else(|| anyhow::anyhow!("local writer proof requires LocalThreadStore"))?;
    let live_thread = session.live_thread_for_persistence("verify local rollout writer")?;

    let before_flush = current_canonical_rollout_path(session, local_store, &codex_home).await?;
    live_thread
        .flush()
        .await
        .map_err(|error| anyhow::anyhow!("flush local rollout writer: {error}"))?;
    let after_flush = current_canonical_rollout_path(session, local_store, &codex_home).await?;
    if before_flush != after_flush {
        anyhow::bail!("local rollout writer path changed while verifying its flush");
    }

    Ok(VerifiedLocalWriterProof {
        thread_id: session.thread_id,
        rollout_path: after_flush,
        codex_home,
    })
}

async fn current_canonical_rollout_path(
    session: &Session,
    local_store: &LocalThreadStore,
    codex_home: &Path,
) -> Result<PathBuf> {
    let store_path = local_store
        .live_rollout_path(session.thread_id)
        .await
        .map_err(|error| anyhow::anyhow!("resolve live LocalThreadStore writer path: {error}"))?;
    let session_path = session
        .current_rollout_path()
        .await?
        .ok_or_else(|| anyhow::anyhow!("session has no current local rollout path"))?;
    let canonical_store_path = canonical_rollout_path(&store_path, codex_home)?;
    let canonical_session_path = canonical_rollout_path(&session_path, codex_home)?;
    if canonical_store_path != canonical_session_path {
        anyhow::bail!("session rollout path does not match its LocalThreadStore writer");
    }
    Ok(canonical_session_path)
}

fn canonical_rollout_path(path: &Path, codex_home: &Path) -> Result<PathBuf> {
    let canonical_path = path.canonicalize().map_err(|error| {
        anyhow::anyhow!(
            "resolve canonical local rollout path {}: {error}",
            path.display()
        )
    })?;
    let relative_path = canonical_path
        .strip_prefix(codex_home)
        .map_err(|_| anyhow::anyhow!("local rollout path is outside CODEX_HOME"))?;
    if relative_path.as_os_str().is_empty() || !canonical_path.is_file() {
        anyhow::bail!("local rollout path is not a file below CODEX_HOME");
    }
    Ok(canonical_path)
}

#[cfg(test)]
#[path = "common_harness_writer_tests.rs"]
mod tests;
