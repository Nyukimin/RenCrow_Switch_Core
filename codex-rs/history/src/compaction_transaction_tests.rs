use anyhow::Result;
use codex_protocol::models::ContentItem;
use codex_protocol::models::PermissionProfile;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadSettingsAppliedEvent;
use codex_protocol::protocol::ThreadSettingsSnapshot;
use codex_protocol::protocol::TurnContextItem;
use codex_protocol::protocol::WorldStateItem;
use serde_json::Value;
use serde_json::json;

use super::*;
use crate::CodexHarnessMetadata;
use crate::CompactedItem;
use crate::ResponseItemEnvelope;

fn replacement_envelope(transaction_following_items: Option<u64>) -> ResponseItemEnvelope {
    let mut envelope = ResponseItemEnvelope::new(ResponseItem::Message {
        id: None,
        role: "assistant".to_string(),
        content: vec![ContentItem::InputText {
            text: "summary".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    });
    envelope.metadata = transaction_following_items.map(|count| CodexHarnessMetadata {
        rencrow_compaction: Some(json!({
            "transaction_following_items": count,
            "source": "test"
        })),
        ..Default::default()
    });
    envelope
}

fn prepared(following_items: u64) -> RolloutItem {
    RolloutItem::Compacted(CompactedItem {
        message: "summary".to_string(),
        replacement_history: Some(vec![replacement_envelope(Some(following_items))]),
        guardian_history: None,
        retained_context: None,
        mcp_resource_origins: None,
        window_number: Some(1),
        first_window_id: None,
        previous_window_id: None,
        window_id: None,
        compaction_response_id: None,
        latest_token_usage_record: None,
    })
}

fn legacy_prepared() -> RolloutItem {
    RolloutItem::Compacted(CompactedItem {
        message: "legacy summary".to_string(),
        replacement_history: Some(vec![replacement_envelope(None)]),
        guardian_history: None,
        retained_context: None,
        mcp_resource_origins: None,
        window_number: None,
        first_window_id: None,
        previous_window_id: None,
        window_id: None,
        compaction_response_id: None,
        latest_token_usage_record: None,
    })
}

fn world_state() -> RolloutItem {
    RolloutItem::WorldState(WorldStateItem::full(serde_json::Map::new()))
}

fn turn_context() -> Result<RolloutItem> {
    let context: TurnContextItem = serde_json::from_value(json!({
        "cwd": std::env::current_dir()?,
        "approval_policy": "never",
        "sandbox_policy": { "type": "danger-full-access" },
        "model": "gpt-5",
        "summary": "auto"
    }))?;
    Ok(RolloutItem::TurnContext(context))
}

fn thread_settings() -> Result<ThreadSettingsSnapshot> {
    Ok(serde_json::from_value(json!({
        "model": "gpt-5",
        "model_provider_id": "openai",
        "approval_policy": "never",
        "approvals_reviewer": "user",
        "permission_profile": PermissionProfile::read_only(),
        "cwd": std::env::current_dir()?,
        "collaboration_mode": {
            "mode": "default",
            "settings": {
                "model": "gpt-5",
                "reasoning_effort": null,
                "developer_instructions": null
            }
        },
        "disabled_plugin_ids": []
    }))?)
}

fn settings() -> Result<RolloutItem> {
    Ok(RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(
        ThreadSettingsAppliedEvent {
            thread_id: None,
            thread_settings: thread_settings()?,
        },
    )))
}

fn tail() -> RolloutItem {
    RolloutItem::ResponseItem(ResponseItemEnvelope::new(ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "later turn".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }))
}

fn complete_batch() -> Result<Vec<RolloutItem>> {
    Ok(vec![
        prepared(3),
        world_state(),
        turn_context()?,
        settings()?,
    ])
}

fn marker(hash: String) -> RolloutItem {
    RolloutItem::RenCrowCompactionCommit {
        checkpoint_hash: hash,
    }
}

fn values(items: &[RolloutItem]) -> Vec<Value> {
    items
        .iter()
        .map(|item| serde_json::to_value(item).expect("rollout item serializes"))
        .collect()
}

fn transaction_hash_for_test(items: &[RolloutItem]) -> Result<String> {
    transaction_hash(items).map_err(anyhow::Error::msg)
}

fn committed_items_for_test(items: &[RolloutItem]) -> Result<Vec<RolloutItem>> {
    committed_items(items).map_err(anyhow::Error::msg)
}

#[test]
fn commit_marker_round_trips_and_hash_is_stable() -> Result<()> {
    let batch = complete_batch()?;
    let hash = transaction_hash_for_test(&batch)?;
    assert_eq!(hash, transaction_hash_for_test(&batch)?);

    let marker = marker(hash.clone());
    assert_eq!(
        serde_json::to_value(&marker)?,
        json!({
            "type": "rencrow_compaction_commit",
            "payload": { "checkpoint_hash": hash }
        })
    );
    let restored_marker = serde_json::from_value::<RolloutItem>(serde_json::to_value(&marker)?)?;
    assert_eq!(
        serde_json::to_value(restored_marker)?,
        serde_json::to_value(marker)?
    );

    let serialized = serde_json::to_value(&batch)?;
    let restored = serde_json::from_value::<Vec<RolloutItem>>(serialized)?;
    assert_eq!(values(&batch), values(&restored));
    assert_eq!(
        transaction_hash_for_test(&batch)?,
        transaction_hash_for_test(&restored)?
    );
    Ok(())
}

#[test]
fn valid_transaction_removes_marker_and_commits_checkpoint_metadata() -> Result<()> {
    let batch = complete_batch()?;
    let hash = transaction_hash_for_test(&batch)?;
    let mut input = batch.clone();
    input.push(marker(hash.clone()));
    input.push(tail());
    let source_before = values(&input);

    let committed = committed_items_for_test(&input)?;
    assert_eq!(committed.len(), 5);
    assert_eq!(values(&committed[1..4]), values(&batch[1..]));
    assert_eq!(values(&committed[4..5]), values(&input[5..6]));
    assert_eq!(values(&input), source_before);

    let RolloutItem::Compacted(checkpoint) = &committed[0] else {
        panic!("expected committed compacted checkpoint");
    };
    let metadata = checkpoint
        .replacement_history
        .as_ref()
        .and_then(|history| history.last())
        .and_then(|envelope| envelope.metadata.as_ref())
        .and_then(|metadata| metadata.rencrow_compaction.as_ref())
        .and_then(Value::as_object)
        .expect("committed compaction metadata");
    assert_eq!(
        metadata.get("committed_transaction_hash"),
        Some(&json!(hash))
    );
    assert!(!metadata.contains_key(TRANSACTION_FOLLOWING_ITEMS));
    assert_eq!(
        values(&committed_items_for_test(&committed)?),
        values(&committed)
    );
    Ok(())
}

#[test]
fn every_truncated_transaction_prefix_discards_only_uncommitted_prefix() -> Result<()> {
    let batch = complete_batch()?;
    let hash = transaction_hash_for_test(&batch)?;
    let mut append = batch.clone();
    append.push(marker(hash));
    let accepted = committed_items_for_test(&append)?;
    assert_eq!(accepted.len(), batch.len());

    for prefix_len in 0..append.len() {
        let truncated = committed_items_for_test(&append[..prefix_len])?;
        assert!(
            truncated.is_empty(),
            "truncated prefix {prefix_len} unexpectedly committed"
        );
    }
    assert_eq!(
        values(&committed_items_for_test(&append)?),
        values(&accepted)
    );
    Ok(())
}

#[test]
fn mismatch_and_missing_marker_preserve_later_records() -> Result<()> {
    let prepared = prepared(2);
    let world = world_state();
    let context = turn_context()?;
    let later = tail();
    let mut mismatch = vec![prepared.clone(), world.clone(), context];
    mismatch.push(marker("0".repeat(64)));
    mismatch.push(later.clone());
    assert_eq!(
        values(&committed_items_for_test(&mismatch)?),
        values(std::slice::from_ref(&later))
    );

    let missing_marker = vec![prepared, world, later.clone()];
    assert_eq!(
        values(&committed_items_for_test(&missing_marker)?),
        values(&[later])
    );
    Ok(())
}

#[test]
fn invalid_transaction_metadata_and_hash_are_errors() -> Result<()> {
    let invalid_count = prepared_with_metadata(json!({
        "transaction_following_items": 4
    }));
    assert!(committed_items(&[invalid_count]).is_err());

    let invalid_metadata = prepared_with_metadata(json!("not an object"));
    assert!(committed_items(&[invalid_metadata]).is_err());

    let batch = complete_batch()?;
    let mut invalid_marker = batch;
    invalid_marker.push(marker("not-a-hash".to_string()));
    let error = committed_items(&invalid_marker).unwrap_err();
    // Replay fails the same way after every restart, so it never asks for one (§73 G04).
    assert!(error.contains("deterministic integrity conflict"));
    assert!(error.contains("restarting cannot repair it"));
    assert!(error.contains("invalid RenCrow compaction commit hash"));
    Ok(())
}

#[test]
fn legacy_compacted_items_are_unchanged_and_prepared_marker_is_idempotent() -> Result<()> {
    let legacy = legacy_prepared();
    assert!(!is_prepared_checkpoint(&legacy));
    assert_eq!(
        values(&committed_items_for_test(std::slice::from_ref(&legacy))?),
        values(&[legacy])
    );

    let zero = prepared(0);
    assert!(is_prepared_checkpoint(&zero));
    let hash = transaction_hash_for_test(std::slice::from_ref(&zero))?;
    let input = vec![zero, marker(hash), tail()];
    let once = committed_items_for_test(&input)?;
    let twice = committed_items_for_test(&once)?;
    assert_eq!(values(&once), values(&twice));
    assert_eq!(once.len(), 2);
    Ok(())
}

fn prepared_with_metadata(compaction_metadata: Value) -> RolloutItem {
    let mut envelope = ResponseItemEnvelope::new(ResponseItem::Message {
        id: None,
        role: "assistant".to_string(),
        content: vec![ContentItem::InputText {
            text: "summary".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    });
    envelope.metadata = Some(CodexHarnessMetadata {
        rencrow_compaction: Some(compaction_metadata),
        ..Default::default()
    });
    RolloutItem::Compacted(CompactedItem {
        message: "summary".to_string(),
        replacement_history: Some(vec![envelope]),
        guardian_history: None,
        retained_context: None,
        mcp_resource_origins: None,
        window_number: None,
        first_window_id: None,
        previous_window_id: None,
        window_id: None,
        compaction_response_id: None,
        latest_token_usage_record: None,
    })
}
