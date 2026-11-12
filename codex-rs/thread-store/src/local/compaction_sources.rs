use std::collections::HashSet;

use codex_protocol::ThreadId;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_rollout::RolloutItem;
use codex_rollout::RolloutLine;
use codex_rollout::open_rollout_line_reader;

use super::LocalThreadStore;
use super::rollout_lineage::RolloutLineageSegment;
use crate::ReadCompactionSourcesParams;
use crate::StoredThreadHistory;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

pub(super) async fn read(
    store: &LocalThreadStore,
    params: ReadCompactionSourcesParams,
) -> ThreadStoreResult<StoredThreadHistory> {
    if params.call_ids.is_empty() {
        return Ok(StoredThreadHistory {
            thread_id: params.thread_id,
            items: Vec::new(),
        });
    }

    let requested = params.call_ids.into_iter().collect::<HashSet<_>>();
    let current =
        super::thread_rollout_resolver::resolve_current_including_archived(store, params.thread_id)
            .await?
            .ok_or(ThreadStoreError::ThreadNotFound {
                thread_id: params.thread_id,
            })?;
    let current_metadata = codex_rollout::read_session_meta_line(current.path.as_path())
        .await
        .map_err(|error| ThreadStoreError::Internal {
            message: format!(
                "failed to read current compaction source {}: {error}",
                current.path.display()
            ),
        })?;
    if current_metadata.meta.id != params.thread_id {
        return Err(ThreadStoreError::InvalidRequest {
            message: "current compaction source belongs to another thread".into(),
        });
    }
    if current_metadata.meta.history_mode != ThreadHistoryMode::Paginated {
        return Err(ThreadStoreError::Unsupported {
            operation: "read_compaction_sources",
        });
    }
    let lineage = store.resolve_rollout_lineage(params.thread_id).await?;
    let mut items = Vec::new();
    for segment in lineage.segments() {
        if !segment_belongs_to_thread(segment, params.thread_id).await? {
            continue;
        }
        items.extend(read_segment_sources(segment, params.thread_id, &requested).await?);
    }

    Ok(StoredThreadHistory {
        thread_id: params.thread_id,
        items,
    })
}

async fn segment_belongs_to_thread(
    segment: &RolloutLineageSegment,
    expected_thread: ThreadId,
) -> ThreadStoreResult<bool> {
    let metadata = codex_rollout::read_session_meta_line(segment.rollout_path.as_path())
        .await
        .map_err(|error| ThreadStoreError::Internal {
            message: format!(
                "failed to read compaction source segment {}: {error}",
                segment.rollout_path.display()
            ),
        })?;
    if metadata.meta.history_mode != ThreadHistoryMode::Paginated {
        return Err(ThreadStoreError::Internal {
            message: format!(
                "compaction source segment {} is not paginated",
                segment.rollout_path.display()
            ),
        });
    }
    Ok(metadata.meta.id == expected_thread)
}

async fn read_segment_sources(
    segment: &RolloutLineageSegment,
    expected_thread: ThreadId,
    requested: &HashSet<String>,
) -> ThreadStoreResult<Vec<RolloutItem>> {
    let mut reader = open_rollout_line_reader(segment.rollout_path.as_path())
        .await
        .map_err(|error| ThreadStoreError::Internal {
            message: format!(
                "failed to open compaction source segment {}: {error}",
                segment.rollout_path.display()
            ),
        })?;
    let mut items = Vec::new();
    while let Some(raw) = reader
        .next_line()
        .await
        .map_err(|error| ThreadStoreError::Internal {
            message: format!(
                "failed to read compaction source segment {}: {error}",
                segment.rollout_path.display()
            ),
        })?
    {
        if raw.trim().is_empty() {
            continue;
        }
        let line: RolloutLine = codex_rollout::parse_rollout_line(&raw).map_err(|error| {
            ThreadStoreError::Internal {
                message: format!(
                    "invalid compaction source record in {}: {error}",
                    segment.rollout_path.display()
                ),
            }
        })?;
        let Some(ordinal) = line.ordinal else {
            if matches!(line.item, RolloutItem::SessionMeta(_)) {
                continue;
            }
            return Err(ThreadStoreError::Internal {
                message: format!(
                    "compaction source record has no ordinal in {}",
                    segment.rollout_path.display()
                ),
            });
        };
        let is_last_included = segment
            .end_ordinal()
            .is_some_and(|end_ordinal| ordinal.checked_add(1) == Some(end_ordinal));
        if segment
            .end_ordinal()
            .is_some_and(|end_ordinal| ordinal >= end_ordinal)
        {
            break;
        }
        if ordinal < segment.start_ordinal() {
            if is_last_included {
                break;
            }
            continue;
        }
        if item_has_requested_call_id(&line.item, expected_thread, requested) {
            items.push(line.item);
        }
        if is_last_included {
            break;
        }
    }
    Ok(items)
}

fn item_has_requested_call_id(
    item: &RolloutItem,
    expected_thread: ThreadId,
    requested: &HashSet<String>,
) -> bool {
    let call_id = match item {
        RolloutItem::ResponseItem(envelope) => match &envelope.item {
            codex_protocol::models::ResponseItem::FunctionCall { call_id, .. }
            | codex_protocol::models::ResponseItem::CustomToolCall { call_id, .. }
            | codex_protocol::models::ResponseItem::CustomToolCallOutput { call_id, .. } => {
                Some(call_id.as_str())
            }
            codex_protocol::models::ResponseItem::FunctionCallOutput {
                call_id: Some(call_id),
                ..
            } => Some(call_id.as_str()),
            _ => None,
        },
        RolloutItem::EventMsg(EventMsg::ItemCompleted(event))
            if event.thread_id == expected_thread =>
        {
            match &event.item {
                TurnItem::CommandExecution(command) => Some(command.id.as_str()),
                _ => None,
            }
        }
        _ => None,
    };
    call_id.is_some_and(|call_id| requested.contains(call_id))
}

#[cfg(test)]
#[path = "compaction_sources_tests.rs"]
mod tests;
