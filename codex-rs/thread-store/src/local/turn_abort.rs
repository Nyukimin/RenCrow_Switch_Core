// Added by RenCrow Switch Core, 2026-11-16: verify owned persisted turn-abort records.
use codex_protocol::ThreadId;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_rollout::RolloutItem;
use codex_rollout::RolloutLine;
use codex_rollout::open_rollout_line_reader;

use super::LocalThreadStore;
use super::thread_rollout_resolver;
use crate::PersistedTurnAbortEvidence;
use crate::PersistedTurnAbortLocator;
use crate::PersistedTurnAbortOwner;
use crate::PersistedTurnAbortSource;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

/// Reads one exact historical abort record from canonical history owned by `thread_id`.
///
/// The caller must flush the live writer before invoking this read. The result proves that this
/// abort record exists; later distinct turns do not invalidate the historical fact. Paginated
/// parent segments are deliberately excluded because an abort is evidence about the owning thread.
pub(super) async fn read(
    store: &LocalThreadStore,
    thread_id: ThreadId,
    turn_id: &str,
) -> ThreadStoreResult<Option<PersistedTurnAbortEvidence>> {
    let current = thread_rollout_resolver::resolve_current_including_archived(store, thread_id)
        .await?
        .ok_or(ThreadStoreError::ThreadNotFound { thread_id })?;
    let metadata = codex_rollout::read_session_meta_line(current.path.as_path())
        .await
        .map_err(|error| internal_error("failed to read turn-abort rollout metadata", error))?;
    if metadata.meta.id != thread_id {
        return Err(ThreadStoreError::InvalidRequest {
            message: "turn-abort rollout belongs to another thread".into(),
        });
    }

    match metadata.meta.history_mode {
        ThreadHistoryMode::Legacy => {
            scan_rollout(
                current.path.as_path(),
                current.rollout_id,
                owner_from_meta(&metadata.meta),
                turn_id,
                /*start_ordinal*/ None,
                /*end_ordinal*/ None,
            )
            .await
        }
        ThreadHistoryMode::Paginated => {
            let lineage = store.resolve_rollout_lineage(thread_id).await?;
            let mut current_rollout_seen = false;
            let mut found = None;
            for segment in lineage.segments() {
                let segment_metadata =
                    codex_rollout::read_session_meta_line(segment.rollout_path.as_path())
                        .await
                        .map_err(|error| {
                            internal_error("failed to read turn-abort segment metadata", error)
                        })?;
                if segment_metadata.meta.history_mode != ThreadHistoryMode::Paginated {
                    return Err(ThreadStoreError::Internal {
                        message: "turn-abort lineage contains a non-paginated segment".into(),
                    });
                }
                if segment.rollout_id() == current.rollout_id {
                    current_rollout_seen = segment_metadata.meta.id == thread_id;
                }
                if segment_metadata.meta.id != thread_id {
                    continue;
                }
                let event = scan_rollout(
                    segment.rollout_path.as_path(),
                    segment.rollout_id(),
                    owner_from_meta(&segment_metadata.meta),
                    turn_id,
                    Some(segment.start_ordinal()),
                    segment.end_ordinal(),
                )
                .await?;
                if event.is_some() && found.is_some() {
                    return Err(ThreadStoreError::Internal {
                        message: "turn-abort lineage contains duplicate events for the turn".into(),
                    });
                }
                found = event.or(found);
            }
            if !current_rollout_seen {
                return Err(ThreadStoreError::InvalidRequest {
                    message: "current paginated rollout does not belong to its owner lineage"
                        .into(),
                });
            }
            Ok(found)
        }
    }
}

async fn scan_rollout(
    path: &std::path::Path,
    rollout_id: ThreadId,
    owner: PersistedTurnAbortOwner,
    target_turn_id: &str,
    start_ordinal: Option<u64>,
    end_ordinal: Option<u64>,
) -> ThreadStoreResult<Option<PersistedTurnAbortEvidence>> {
    let mut reader = open_rollout_line_reader(path)
        .await
        .map_err(|error| internal_error("failed to open persisted turn-abort rollout", error))?;
    let mut previous_ordinal = None;
    let mut found = None;
    let mut line_index = 0_u64;
    while let Some(raw) = reader
        .next_line()
        .await
        .map_err(|error| internal_error("failed to read persisted turn-abort rollout", error))?
    {
        let current_line_index = line_index;
        line_index = line_index
            .checked_add(1)
            .ok_or_else(|| ThreadStoreError::Internal {
                message: "persisted turn-abort rollout line index overflowed".into(),
            })?;
        if raw.trim().is_empty() {
            continue;
        }
        let line: RolloutLine = codex_rollout::parse_rollout_line(&raw).map_err(|error| {
            internal_error("invalid persisted turn-abort rollout record", error)
        })?;
        if let Some(ordinal) = line.ordinal {
            if end_ordinal.is_some_and(|end| ordinal >= end) {
                break;
            }
            if previous_ordinal.is_some_and(|previous| ordinal <= previous) {
                return Err(ThreadStoreError::Internal {
                    message: "persisted turn-abort rollout ordinals are not increasing".into(),
                });
            }
            previous_ordinal = Some(ordinal);
        } else if start_ordinal.is_some() && !matches!(&line.item, RolloutItem::SessionMeta(_)) {
            return Err(ThreadStoreError::Internal {
                message: "persisted turn-abort rollout item has no ordinal".into(),
            });
        }
        if start_ordinal.is_some_and(|start| line.ordinal.is_some_and(|ordinal| ordinal < start)) {
            continue;
        }
        match line.item {
            RolloutItem::EventMsg(EventMsg::TurnAborted(event))
                if event.turn_id.as_deref() == Some(target_turn_id) =>
            {
                if found.is_some() {
                    return Err(ThreadStoreError::Internal {
                        message: "persisted rollout contains multiple abort events for the turn"
                            .into(),
                    });
                }
                let locator = match line.ordinal {
                    Some(ordinal) => PersistedTurnAbortLocator::Ordinal(ordinal),
                    None if start_ordinal.is_none() => {
                        PersistedTurnAbortLocator::LegacyLineIndex(current_line_index)
                    }
                    None => {
                        return Err(ThreadStoreError::Internal {
                            message: "paginated turn-abort event has no ordinal".into(),
                        });
                    }
                };
                found = Some(PersistedTurnAbortEvidence {
                    event,
                    owner: owner.clone(),
                    source: PersistedTurnAbortSource {
                        rollout_id,
                        locator,
                    },
                });
            }
            RolloutItem::EventMsg(EventMsg::TurnComplete(event))
                if event.turn_id == target_turn_id && found.is_some() =>
            {
                return Err(ThreadStoreError::Internal {
                    message: "persisted rollout contains conflicting terminal events for the turn"
                        .into(),
                });
            }
            RolloutItem::EventMsg(EventMsg::TurnComplete(event))
                if event.turn_id == target_turn_id =>
            {
                return Err(ThreadStoreError::Internal {
                    message: "persisted rollout contains a completed turn instead of its abort"
                        .into(),
                });
            }
            _ => {}
        }
    }
    Ok(found)
}

fn owner_from_meta(meta: &codex_protocol::protocol::SessionMeta) -> PersistedTurnAbortOwner {
    PersistedTurnAbortOwner {
        thread_id: meta.id,
        parent_thread_id: meta.parent_thread_id,
        agent_role: meta.agent_role.clone(),
    }
}

fn internal_error(context: &str, error: impl std::fmt::Display) -> ThreadStoreError {
    ThreadStoreError::Internal {
        message: format!("{context}: {error}"),
    }
}

#[cfg(test)]
#[path = "turn_abort_tests.rs"]
mod tests;
