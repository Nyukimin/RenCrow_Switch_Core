// Modified by RenCrow Switch Core, 2026-09-22: validate checkpoints before startup metadata consumers.
//! Reconstructs model context and preserves source runtime metadata across fork cutoffs.

use std::io;

use codex_protocol::protocol::HistoryPosition;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_rollout::ModelContextScan;
use codex_rollout::ModelContextScanProgress;
use codex_rollout::ReverseJsonlScanner;
use codex_rollout::RolloutItem;
use codex_rollout::ScanOutcome;

use super::LocalThreadStore;
use super::read_thread;
use super::rollout_lineage::RolloutLineage;
use super::thread_rollout_resolver;
use crate::LoadThreadHistoryParams;
use crate::StoredModelContext;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

#[cfg(test)]
#[path = "model_context_tests.rs"]
mod tests;

/// Loads rollout items needed to reconstruct the latest model-visible context.
///
/// Paginated JSONL rollouts use a reverse scan. When it finds both a usable replacement-
/// history checkpoint and the completed user-turn context needed for resume metadata, the returned
/// replay starts with the canonical `SessionMeta` followed by that newest suffix. When no
/// bounded cutoff is available, the scan continues to the beginning and returns the complete
/// replay it already accumulated.
///
/// Compressed segments are decoded before applying their original JSONL offsets. Legacy rollouts
/// keep the existing full-history path.
pub(super) async fn load_latest_model_context(
    store: &LocalThreadStore,
    params: LoadThreadHistoryParams,
) -> ThreadStoreResult<StoredModelContext> {
    let resolved = if params.include_archived {
        thread_rollout_resolver::resolve_current_including_archived(store, params.thread_id).await?
    } else {
        thread_rollout_resolver::resolve_current(store, params.thread_id).await?
    };
    let path =
        resolved
            .map(|resolved| resolved.path)
            .ok_or_else(|| ThreadStoreError::InvalidRequest {
                message: format!("no rollout found for thread id {}", params.thread_id),
            })?;

    let session_meta = codex_rollout::read_session_meta_line(path.as_path())
        .await
        .map_err(|err| ThreadStoreError::Internal {
            message: format!("failed to read session metadata {}: {err}", path.display()),
        })?;
    if session_meta.meta.id != params.thread_id {
        return Err(ThreadStoreError::InvalidRequest {
            message: format!(
                "rollout at {} belongs to thread {}, not {}",
                path.display(),
                session_meta.meta.id,
                params.thread_id
            ),
        });
    }

    let items = if matches!(session_meta.meta.history_mode, ThreadHistoryMode::Paginated) {
        let lineage = store.resolve_rollout_lineage(params.thread_id).await?;
        scan_model_context_from_lineage(lineage, session_meta).await?
    } else {
        let items = read_thread::load_history_items(path.as_path()).await?;
        normalize_committed_items(items)?
    };

    Ok(StoredModelContext {
        thread_id: params.thread_id,
        items,
    })
}

/// Loads startup context from a fork's frozen inherited prefix.
pub(super) async fn load_for_fork(
    lineage: RolloutLineage,
    history_base: Option<HistoryPosition>,
) -> ThreadStoreResult<Vec<RolloutItem>> {
    let source_path = lineage
        .segments()
        .last()
        .map(|segment| segment.rollout_path.as_path())
        .ok_or_else(|| ThreadStoreError::Internal {
            message: "fork lineage has no source segment".to_string(),
        })?;
    let mut session_meta = codex_rollout::read_session_meta_line(source_path)
        .await
        .map_err(|err| ThreadStoreError::Internal {
            message: format!(
                "failed to read session metadata {}: {err}",
                source_path.display()
            ),
        })?;
    if session_meta.meta.multi_agent_version.is_none() {
        // Recover the runtime version from the newest committed source context before applying
        // the fork cutoff. Keep this bounded so a valid current-segment context does not require
        // reading unavailable older segments; a prepared transaction is validated first so its
        // uncommitted companion context cannot supply the version.
        let source_lineage = lineage.clone();
        session_meta.meta.multi_agent_version =
            tokio::task::spawn_blocking(move || recover_fork_runtime_version(&source_lineage))
                .await
                .map_err(|err| ThreadStoreError::Internal {
                    message: format!("failed to join fork runtime version scan: {err}"),
                })?
                .map_err(|err| ThreadStoreError::Internal {
                    message: format!("failed to read fork runtime version: {err}"),
                })?;
    }
    match history_base {
        Some(history_base) => {
            let lineage = lineage.truncate_at(history_base).await?;
            scan_model_context_from_lineage(lineage, session_meta).await
        }
        None => Ok(vec![RolloutItem::SessionMeta(session_meta)]),
    }
}

async fn scan_model_context_from_lineage(
    lineage: RolloutLineage,
    session_meta: SessionMetaLine,
) -> ThreadStoreResult<Vec<RolloutItem>> {
    let scan = tokio::task::spawn_blocking(move || {
        scan_model_context_from_lineage_blocking(&lineage, session_meta)
    })
    .await
    .map_err(|err| ThreadStoreError::Internal {
        message: format!("failed to join model context scan: {err}"),
    })?;
    match scan {
        Ok(items) => normalize_committed_items(items),
        Err(err) => Err(ThreadStoreError::Internal {
            message: format!("failed to scan paginated model context lineage: {err}"),
        }),
    }
}

fn normalize_committed_items(items: Vec<RolloutItem>) -> ThreadStoreResult<Vec<RolloutItem>> {
    codex_rollout::compaction_transaction::committed_items(&items).map_err(|err| {
        ThreadStoreError::Internal {
            message: format!("failed to validate compaction transactions: {err}"),
        }
    })
}

struct PendingRuntimeVersion {
    version: MultiAgentVersion,
    candidate: RolloutItem,
    /// Newest-to-oldest rows surrounding the candidate. The scanner has already seen the
    /// newer suffix; older rows are appended until the possible prepared checkpoint is found.
    window: Vec<RolloutItem>,
    saw_older_world_state: bool,
}

fn recover_fork_runtime_version(lineage: &RolloutLineage) -> io::Result<Option<MultiAgentVersion>> {
    let mut recent = Vec::new();
    let mut pending: Option<PendingRuntimeVersion> = None;

    for segment in lineage.segments().iter().rev() {
        let file = codex_rollout::open_rollout_seekable_reader(segment.rollout_path.as_path())?;
        let mut scanner = match segment.end.map(|end| end.end_byte_offset) {
            Some(end_byte_offset) => ReverseJsonlScanner::new_at(file, end_byte_offset)?,
            None => ReverseJsonlScanner::new(file)?,
        };
        while let Some(outcome) = scanner.scan_next_rollout_line()? {
            let ScanOutcome::Parsed(line) = outcome else {
                continue;
            };
            let item = line.item;

            if matches!(&item, RolloutItem::SessionMeta(_)) {
                if pending
                    .as_ref()
                    .is_some_and(|pending_version| !pending_version.saw_older_world_state)
                    && let Some(pending_version) = pending.take()
                {
                    if let Some(version) = committed_runtime_version(pending_version)? {
                        return Ok(Some(version));
                    }
                    recent.clear();
                }
                // A world state may be the first older row of a prepared transaction whose
                // checkpoint is in an ancestor segment. Keep waiting across this segment head.
                recent.clear();
                break;
            }

            if let Some(mut pending_version) = pending.take() {
                if !pending_version.saw_older_world_state
                    && matches!(&item, RolloutItem::WorldState(_))
                {
                    pending_version.window.push(item);
                    pending_version.saw_older_world_state = true;
                    pending = Some(pending_version);
                    continue;
                }
                let is_checkpoint = matches!(&item, RolloutItem::Compacted(_));
                if is_checkpoint {
                    pending_version.window.push(item.clone());
                }
                if let Some(version) = committed_runtime_version(pending_version)? {
                    return Ok(Some(version));
                }
                recent.clear();
                if is_checkpoint {
                    continue;
                }
            }

            if let RolloutItem::TurnContext(context) = &item
                && let Some(version) = context.multi_agent_version
            {
                let mut window = recent.clone();
                window.push(item.clone());
                pending = Some(PendingRuntimeVersion {
                    version,
                    candidate: item,
                    window,
                    saw_older_world_state: false,
                });
            } else {
                recent.push(item);
                if recent.len() > 4 {
                    recent.remove(0);
                }
            }
        }
    }

    pending
        .map(committed_runtime_version)
        .transpose()
        .map(std::option::Option::flatten)
}

fn committed_runtime_version(
    pending: PendingRuntimeVersion,
) -> io::Result<Option<MultiAgentVersion>> {
    let chronological = pending.window.iter().rev().cloned().collect::<Vec<_>>();
    let committed = codex_rollout::compaction_transaction::committed_items(&chronological)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    let candidate = serde_json::to_value(&pending.candidate)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    for item in &committed {
        let value = serde_json::to_value(item)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        if value == candidate {
            return Ok(Some(pending.version));
        }
    }
    Ok(None)
}

fn scan_model_context_from_lineage_blocking(
    lineage: &RolloutLineage,
    session_meta: SessionMetaLine,
) -> io::Result<Vec<RolloutItem>> {
    let mut scan = ModelContextScan::default();
    'segments: for segment in lineage.segments().iter().rev() {
        let file = codex_rollout::open_rollout_seekable_reader(segment.rollout_path.as_path())?;
        let mut scanner = match segment.end.map(|end| end.end_byte_offset) {
            Some(end_byte_offset) => ReverseJsonlScanner::new_at(file, end_byte_offset)?,
            None => ReverseJsonlScanner::new(file)?,
        };
        while let Some(outcome) = scanner.scan_next_rollout_line()? {
            let ScanOutcome::Parsed(line) = outcome else {
                continue;
            };
            // Each rollout segment contributes only its local delta. Its session metadata is
            // replaced with the requested thread's canonical SessionMeta after replay.
            if matches!(&line.item, RolloutItem::SessionMeta(_)) {
                break;
            }
            match scan.push(line.item) {
                ModelContextScanProgress::Continue => {}
                ModelContextScanProgress::Complete => break 'segments,
            }
        }
    }

    let canonical_meta = session_meta.clone();
    let mut items = scan.finish(session_meta);
    if !matches!(items.first(), Some(RolloutItem::SessionMeta(_))) {
        items.insert(0, RolloutItem::SessionMeta(canonical_meta));
    }
    Ok(items)
}
