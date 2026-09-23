// Modified by RenCrow Switch Core, 2026-09-22.
//! Durable transaction validation for prepared RenCrow compaction checkpoints.
//!
//! A compacted checkpoint is written before its commit marker so an interrupted
//! append can be discarded during replay without dropping later accepted rows.

use crate::RolloutItem;
use crate::compaction_candidate::digest;

const TRANSACTION_FOLLOWING_ITEMS: &str = "transaction_following_items";
const MAX_FOLLOWING_ITEMS: u64 = 3;

/// Hash the canonical JSON representation of a prepared checkpoint transaction.
pub fn transaction_hash(items: &[RolloutItem]) -> Result<String, String> {
    let canonical = serde_json::to_value(items).map_err(|_| "serialization failed".to_owned())?;
    digest(&canonical)
}

/// Whether a compacted item carries a host transaction boundary.
///
/// Malformed transaction metadata is treated as prepared so callers that
/// bound history reads do not cut away data before the owner can reject it.
pub fn is_prepared_checkpoint(item: &RolloutItem) -> bool {
    match transaction_following_items(item) {
        Ok(Some(_)) | Err(_) => true,
        Ok(None) => false,
    }
}

/// Keep only committed prepared checkpoints and ordinary rollout rows.
///
/// Commit markers are persistence-only and are removed from the returned
/// replay input. Legacy compacted items without the transaction metadata are
/// retained unchanged.
pub fn committed_items(items: &[RolloutItem]) -> Result<Vec<RolloutItem>, String> {
    let mut committed = Vec::with_capacity(items.len());
    let mut index = 0;
    while index < items.len() {
        if let RolloutItem::RenCrowCompactionCommit { checkpoint_hash } = &items[index] {
            validate_commit_hash(checkpoint_hash)?;
            index += 1;
            continue;
        }

        let Some(following_items) = transaction_following_items(&items[index])? else {
            committed.push(items[index].clone());
            index += 1;
            continue;
        };

        let available = items.len().saturating_sub(index + 1).min(following_items);
        let companion_count = companion_prefix_len(&items[index + 1..index + 1 + available]);
        let marker_index = index + 1 + following_items;
        let marker_hash = items.get(marker_index).and_then(commit_hash);

        if companion_count == following_items
            && let Some(marker_hash) = marker_hash
        {
            validate_commit_hash(marker_hash)?;
            let expected_hash = transaction_hash(&items[index..marker_index])?;
            if marker_hash.eq_ignore_ascii_case(&expected_hash) {
                committed.push(committed_checkpoint(&items[index], &expected_hash)?);
                committed.extend(items[index + 1..marker_index].iter().cloned());
                index = marker_index + 1;
                continue;
            }
            // The marker is consumed on the next iteration; the prepared
            // checkpoint and its declared companions are not committed.
            index = marker_index;
            continue;
        }

        // A missing marker, truncated append, or malformed companion prefix
        // invalidates only this prepared prefix. Later rows remain available.
        index += 1 + companion_count;
    }
    Ok(committed)
}

fn committed_checkpoint(item: &RolloutItem, transaction_hash: &str) -> Result<RolloutItem, String> {
    let RolloutItem::Compacted(compacted) = item else {
        return Err("prepared RenCrow checkpoint is not compacted".into());
    };
    let mut committed = compacted.clone();
    let Some(replacement_history) = committed.replacement_history.as_mut() else {
        return Err("prepared RenCrow checkpoint has no replacement history".into());
    };
    let Some(envelope) = replacement_history.last_mut() else {
        return Err("prepared RenCrow checkpoint has empty replacement history".into());
    };
    let Some(metadata) = envelope.metadata.as_mut() else {
        return Err("prepared RenCrow checkpoint has no metadata".into());
    };
    let Some(compaction_metadata) = metadata.rencrow_compaction.as_mut() else {
        return Err("prepared RenCrow checkpoint has no compaction metadata".into());
    };
    let Some(fields) = compaction_metadata.as_object_mut() else {
        return Err("malformed RenCrow compaction metadata".into());
    };
    fields.remove(TRANSACTION_FOLLOWING_ITEMS);
    fields.insert(
        "committed_transaction_hash".into(),
        serde_json::Value::String(transaction_hash.to_owned()),
    );
    Ok(RolloutItem::Compacted(committed))
}

fn transaction_following_items(item: &RolloutItem) -> Result<Option<usize>, String> {
    let RolloutItem::Compacted(compacted) = item else {
        return Ok(None);
    };
    let Some(replacement_history) = compacted.replacement_history.as_ref() else {
        return Ok(None);
    };
    let Some(envelope) = replacement_history.last() else {
        return Ok(None);
    };
    let Some(metadata) = envelope.metadata.as_ref() else {
        return Ok(None);
    };
    let Some(compaction_metadata) = metadata.rencrow_compaction.as_ref() else {
        return Ok(None);
    };
    let Some(fields) = compaction_metadata.as_object() else {
        return Err("malformed RenCrow compaction metadata".into());
    };
    let Some(value) = fields.get(TRANSACTION_FOLLOWING_ITEMS) else {
        return Ok(None);
    };
    let Some(count) = value.as_u64() else {
        return Err("transaction_following_items must be an integer from 0 through 3".into());
    };
    if count > MAX_FOLLOWING_ITEMS {
        return Err("transaction_following_items must be an integer from 0 through 3".into());
    }
    Ok(Some(count as usize))
}

fn companion_prefix_len(items: &[RolloutItem]) -> usize {
    let mut world_state = false;
    let mut turn_context = false;
    let mut settings = false;
    for item in items {
        let valid = match item {
            RolloutItem::WorldState(_) if !world_state && !turn_context && !settings => {
                world_state = true;
                true
            }
            RolloutItem::TurnContext(_) if !turn_context && !settings => {
                turn_context = true;
                true
            }
            RolloutItem::EventMsg(event) if !settings => {
                if matches!(
                    event,
                    codex_protocol::protocol::EventMsg::ThreadSettingsApplied(_)
                ) {
                    settings = true;
                    true
                } else {
                    false
                }
            }
            _ => false,
        };
        if !valid {
            break;
        }
    }
    world_state as usize + turn_context as usize + settings as usize
}

fn commit_hash(item: &RolloutItem) -> Option<&str> {
    match item {
        RolloutItem::RenCrowCompactionCommit { checkpoint_hash } => Some(checkpoint_hash),
        _ => None,
    }
}

fn validate_commit_hash(hash: &str) -> Result<(), String> {
    if hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err("invalid RenCrow compaction commit hash".into())
    }
}

#[cfg(test)]
#[path = "compaction_transaction_tests.rs"]
mod tests;
