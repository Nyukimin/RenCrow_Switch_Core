//! Candidate checks performed before the session-owned durable checkpoint commit.

use super::native::NativeProjection;
use crate::compact::SUMMARY_PREFIX;
use crate::context_manager::ContextManager;
use crate::session::context_window::ContextWindowTokenStatus;
use codex_history::RenCrowCompactionMetadataV2;
use codex_history::ResponseItemEnvelope;
use codex_protocol::config_types::AutoCompactTokenLimitScope;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;

/// Validate a fully assembled V2 history candidate without changing the live session.
pub(super) fn validate_compaction_candidate(
    original: &ContextManager,
    base: &BaseInstructions,
    projection: &NativeProjection,
    initial_context: &[ResponseItemEnvelope],
    candidate: &[ResponseItemEnvelope],
    thread_id: &str,
    scope: AutoCompactTokenLimitScope,
    limits: &ContextWindowTokenStatus,
) -> Result<(), String> {
    let summary = candidate
        .last()
        .ok_or_else(|| "compaction candidate is empty".to_owned())?;
    let ResponseItem::Message {
        role,
        content,
        internal_chat_message_metadata_passthrough,
        ..
    } = &summary.item
    else {
        return Err("compaction candidate does not end with a typed summary message".into());
    };
    if role != "user" || content.len() != 1 {
        return Err("compaction candidate summary must be one user InputText item".into());
    }
    let [ContentItem::InputText { text }] = content.as_slice() else {
        return Err("compaction candidate summary must contain only InputText".into());
    };
    let summary_prefix = format!("{SUMMARY_PREFIX}\n");
    let Some(summary_body) = text.strip_prefix(&summary_prefix) else {
        return Err("compaction candidate summary has no canonical prefix".into());
    };
    if summary_body.trim().is_empty() {
        return Err("compaction candidate summary body is empty".into());
    }
    let is_typed_summary = internal_chat_message_metadata_passthrough
        .as_ref()
        .and_then(|metadata| metadata.content_item_kinds.as_ref())
        .is_some_and(|kinds| {
            kinds.len() == content.len() && kinds.iter().all(|kind| kind.0 == "compaction.summary")
        });
    if !is_typed_summary {
        return Err("compaction candidate summary is not marked compaction.summary".into());
    }

    let metadata = summary
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.rencrow_compaction.clone())
        .ok_or_else(|| "compaction candidate has no V2 checkpoint metadata".to_owned())?;
    let metadata = RenCrowCompactionMetadataV2::parse_and_validate(metadata, thread_id, text)?;
    if metadata.transaction_following_items.is_some()
        || metadata.committed_transaction_hash.is_some()
    {
        return Err("new compaction candidate cannot carry transaction lifecycle metadata".into());
    }

    let expected = projection.retained_native_items_with_initial_context(initial_context, summary);
    let (expected_len, expected_upper_bound) = expected.size_hint();
    if expected_upper_bound != Some(expected_len)
        || expected_len != candidate.len()
        || !candidate
            .iter()
            .zip(expected)
            .all(|(candidate, expected)| candidate == expected)
    {
        return Err(
            "compaction candidate differs from retained native items or canonical initial context"
                .into(),
        );
    }

    let before = original
        .estimate_token_count_with_base_instructions(base)
        .ok_or_else(|| "original compaction context estimate is unavailable".to_owned())?;
    let mut candidate_history = ContextManager::new();
    candidate_history.replace_annotated(candidate.to_vec());
    let after = candidate_history
        .estimate_token_count_with_base_instructions(base)
        .ok_or_else(|| "candidate compaction context estimate is unavailable".to_owned())?;
    if after >= before {
        return Err("compaction candidate does not shrink the estimated context".into());
    }

    let candidate_scope_tokens = match scope {
        AutoCompactTokenLimitScope::Total => after,
        AutoCompactTokenLimitScope::BodyAfterPrefix => 0,
    };
    if limits
        .auto_compact_scope_limit
        .is_some_and(|limit| candidate_scope_tokens >= limit)
    {
        return Err(
            "compaction candidate reaches the configured auto-compaction scope limit".into(),
        );
    }
    if limits
        .full_context_window_limit
        .is_some_and(|limit| after >= limit)
    {
        return Err("compaction candidate reaches the full context window limit".into());
    }

    Ok(())
}

#[cfg(test)]
#[path = "compact_rencrow_candidate_tests.rs"]
mod tests;
