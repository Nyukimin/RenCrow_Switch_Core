//! Read committed V2 observation references without expanding archived source text.

use codex_history::CompactedItem;
use codex_history::ObservationCoverage;
use codex_history::RenCrowCompactionMetadataV2;
use codex_history::RolloutItem;
use codex_history::compaction_transaction;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use serde::Serialize;
use std::collections::HashSet;

const INVENTORY_PAGE_SIZE: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InventoryQuery {
    Page {
        checkpoint_hash: Option<String>,
        offset: usize,
    },
    CallId(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ObservationInventoryEntry {
    pub coverage: ObservationCoverage,
    pub important: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ObservationInventoryPage {
    pub checkpoint_hash: String,
    pub items: Vec<ObservationInventoryEntry>,
    pub next_offset: Option<usize>,
}

/// Return a bounded inventory from the latest committed V2 checkpoint.
///
/// `items` must be raw persisted rollout rows. A normalized in-memory checkpoint with a
/// prefilled `committed_transaction_hash` is rejected. `committed_items` owns transaction
/// filtering and normalization; legacy checkpoints are skipped without promoting them to V2.
pub fn inventory_compaction_from_items(
    items: &[RolloutItem],
    thread_id: &str,
    query: InventoryQuery,
) -> Result<ObservationInventoryPage, String> {
    if thread_id.trim().is_empty() {
        return Err("inventory thread ID must be nonempty".into());
    }
    reject_raw_commit_hash_claims(items)?;
    let committed = compaction_transaction::committed_items(items)?;
    let compacted = committed
        .iter()
        .rev()
        .find_map(|item| match item {
            RolloutItem::Compacted(compacted) if has_v2_metadata(compacted) => Some(compacted),
            _ => None,
        })
        .ok_or_else(|| "no committed V2 compaction checkpoint is available".to_owned())?;
    let (metadata, transaction_hash) = latest_v2_checkpoint(compacted, thread_id)?;

    let important = metadata
        .important_refs
        .iter()
        .map(|reference| {
            (
                reference.thread_id.as_str(),
                reference.call_id.as_str(),
                reference.sha256.as_str(),
            )
        })
        .collect::<HashSet<_>>();
    let entries = metadata
        .observations
        .iter()
        .map(|coverage| ObservationInventoryEntry {
            coverage: coverage.clone(),
            important: important.contains(&(
                coverage.reference.thread_id.as_str(),
                coverage.reference.call_id.as_str(),
                coverage.reference.sha256.as_str(),
            )),
        })
        .collect::<Vec<_>>();

    let (selected, next_offset) = match query {
        InventoryQuery::Page {
            checkpoint_hash: None,
            offset: 0,
        } => page_slice(&entries, 0)?,
        InventoryQuery::Page {
            checkpoint_hash: None,
            ..
        } => return Err("a nonzero inventory offset requires a checkpoint hash".into()),
        InventoryQuery::Page {
            checkpoint_hash: Some(requested_hash),
            offset,
        } => {
            if requested_hash != transaction_hash {
                return Err("inventory continuation checkpoint is stale".into());
            }
            page_slice(&entries, offset)?
        }
        InventoryQuery::CallId(call_id) => {
            if call_id.trim().is_empty() {
                return Err("inventory call ID must be nonempty".into());
            }
            let mut matching = entries
                .iter()
                .filter(|entry| entry.coverage.reference.call_id == call_id);
            let Some(entry) = matching.next() else {
                return Err("observation reference is not present in the latest checkpoint".into());
            };
            if matching.next().is_some() {
                return Err("observation call ID is ambiguous in the latest checkpoint".into());
            }
            (vec![entry.clone()], None)
        }
    };

    Ok(ObservationInventoryPage {
        checkpoint_hash: transaction_hash,
        items: selected,
        next_offset,
    })
}

fn latest_v2_checkpoint(
    compacted: &CompactedItem,
    thread_id: &str,
) -> Result<(RenCrowCompactionMetadataV2, String), String> {
    let replacement_history = compacted
        .replacement_history
        .as_ref()
        .ok_or_else(|| "latest checkpoint has no replacement history".to_owned())?;
    let summary = replacement_history
        .last()
        .ok_or_else(|| "latest checkpoint has no final summary item".to_owned())?;
    let (summary_text, summary_is_typed) = match &summary.item {
        ResponseItem::Message {
            role,
            content,
            internal_chat_message_metadata_passthrough,
            ..
        } if role == "user" => {
            let text = match content.as_slice() {
                [ContentItem::InputText { text }] => text.as_str(),
                _ => return Err("latest checkpoint summary must be one text item".into()),
            };
            let summary_is_typed = internal_chat_message_metadata_passthrough
                .as_ref()
                .and_then(|metadata| metadata.content_item_kinds.as_ref())
                .is_some_and(|kinds| kinds.len() == 1 && kinds[0].0 == "compaction.summary");
            (text, summary_is_typed)
        }
        _ => return Err("latest checkpoint final summary must be a user message".into()),
    };
    if !summary_is_typed {
        return Err("latest checkpoint summary is not marked compaction.summary".into());
    }
    if compacted.message != summary_text {
        return Err("checkpoint message differs from its final summary body".into());
    }
    let encoded = summary
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.rencrow_compaction.clone())
        .ok_or_else(|| "latest checkpoint has no V2 compaction metadata".to_owned())?;
    let metadata =
        RenCrowCompactionMetadataV2::parse_and_validate(encoded, thread_id, summary_text)?;
    let transaction_hash = metadata
        .committed_transaction_hash
        .clone()
        .ok_or_else(|| "latest V2 checkpoint has no committed transaction hash".to_owned())?;
    Ok((metadata, transaction_hash))
}

fn has_v2_metadata(compacted: &CompactedItem) -> bool {
    compacted
        .replacement_history
        .as_ref()
        .into_iter()
        .flatten()
        .filter_map(|summary| summary.metadata.as_ref())
        .filter_map(|metadata| metadata.rencrow_compaction.as_ref())
        .any(looks_like_v2_metadata)
}

fn looks_like_v2_metadata(value: &serde_json::Value) -> bool {
    let Some(fields) = value.as_object() else {
        return true;
    };
    let has_v2_fields = [
        "snapshot_hash",
        "presentation_hash",
        "summary_hash",
        "plan_hash",
        "applied_refs",
        "results",
        "observations",
        "important_refs",
        "committed_transaction_hash",
    ]
    .iter()
    .any(|field| fields.contains_key(*field));
    match fields.get("version") {
        Some(version) if version.as_u64() == Some(1) && !has_v2_fields => false,
        Some(_) => true,
        None => has_v2_fields,
    }
}

fn reject_raw_commit_hash_claims(items: &[RolloutItem]) -> Result<(), String> {
    for item in items {
        let RolloutItem::Compacted(compacted) = item else {
            continue;
        };
        for value in compacted
            .replacement_history
            .iter()
            .flatten()
            .filter_map(|summary| summary.metadata.as_ref())
            .filter_map(|metadata| metadata.rencrow_compaction.as_ref())
        {
            if looks_like_v2_metadata(value)
                && value
                    .as_object()
                    .and_then(|fields| fields.get("committed_transaction_hash"))
                    .is_some_and(|hash| !hash.is_null())
            {
                return Err("raw V2 metadata cannot claim a committed transaction hash".into());
            }
        }
    }
    Ok(())
}

fn page_slice(
    entries: &[ObservationInventoryEntry],
    offset: usize,
) -> Result<(Vec<ObservationInventoryEntry>, Option<usize>), String> {
    if offset > entries.len() {
        return Err("inventory offset is beyond the latest checkpoint".into());
    }
    let end = offset
        .checked_add(INVENTORY_PAGE_SIZE)
        .unwrap_or(usize::MAX)
        .min(entries.len());
    let next_offset = (end < entries.len()).then_some(end);
    Ok((entries[offset..end].to_vec(), next_offset))
}
