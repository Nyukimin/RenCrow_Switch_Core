//! Unwired F08 committed-inventory tests. Wire this module after the F07d Cargo owner releases.

use super::*;
use codex_history::CheckpointResponseStage;
use codex_history::CodexHarnessMetadata;
use codex_history::CompactedItem;
use codex_history::CompactionModelResponseReceipt;
use codex_history::CompactionSelectionMode;
use codex_history::ObservationCoverage;
use codex_history::ObservationReference;
use codex_history::RenCrowCompactionMetadataV2;
use codex_history::ResponseItemEnvelope;
use codex_history::RolloutItem;
use codex_history::archive_reference::content_sha256;
use codex_history::compaction_transaction;
use codex_history::observation_projection::project_observation;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::InternalChatMessageMetadataPassthrough;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::WorldStateItem;
use serde_json::json;

const THREAD: &str = "thread-f08-inventory";

fn coverage(call_id: &str, call_text: &str, output_text: &str) -> ObservationCoverage {
    let reference = ObservationReference::new(THREAD, call_id, content_sha256(output_text));
    project_observation(&reference, "read_document", call_text, output_text)
        .expect("coverage fixture should be valid")
        .coverage
}

fn metadata(summary: &str, observations: Vec<ObservationCoverage>) -> RenCrowCompactionMetadataV2 {
    RenCrowCompactionMetadataV2 {
        version: 2,
        snapshot_hash: "a".repeat(64),
        presentation_hash: None,
        summary_hash: content_sha256(summary),
        semantic_summary_hash: Some(content_sha256(summary)),
        selection_mode: CompactionSelectionMode::NoCandidates,
        plan_hash: None,
        applied_refs: Vec::new(),
        results: Vec::new(),
        important_refs: observations
            .iter()
            .map(|observation| observation.reference.clone())
            .take(1)
            .collect(),
        summary_covered_observations: observations
            .iter()
            .map(|observation| observation.reference.clone())
            .collect(),
        observations,
        model: Some("qwen-test".into()),
        effort: None,
        responses: vec![CompactionModelResponseReceipt {
            stage: CheckpointResponseStage::Summary,
            response_id: "resp-summary".into(),
            seconds: 1.0,
            usage: None,
        }],
        transaction_following_items: None,
        committed_transaction_hash: None,
    }
}

fn prepared_checkpoint(
    summary: &str,
    metadata: RenCrowCompactionMetadataV2,
    malformed_summary_body: Option<&str>,
) -> RolloutItem {
    let mut encoded = serde_json::to_value(metadata).unwrap();
    encoded["transaction_following_items"] = json!(1);
    checkpoint_with_encoded_metadata(summary, encoded, malformed_summary_body)
}

fn checkpoint_with_encoded_metadata(
    summary: &str,
    encoded: serde_json::Value,
    malformed_summary_body: Option<&str>,
) -> RolloutItem {
    let envelope_text = malformed_summary_body.unwrap_or(summary);
    let mut envelope = ResponseItemEnvelope::new(ResponseItem::Message {
        id: None,
        role: "user".into(),
        content: vec![ContentItem::InputText {
            text: envelope_text.into(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: Some(InternalChatMessageMetadataPassthrough {
            content_item_kinds: Some(vec![ContentItemKind("compaction.summary".into())]),
            ..Default::default()
        }),
    });
    envelope.metadata = Some(CodexHarnessMetadata {
        rencrow_compaction: Some(encoded),
        ..Default::default()
    });
    RolloutItem::Compacted(CompactedItem {
        message: summary.into(),
        replacement_history: Some(vec![envelope]),
        guardian_history: None,
        retained_context: None,
        mcp_resource_origins: None,
        window_number: Some(1),
        first_window_id: None,
        previous_window_id: None,
        window_id: Some("window-f08".into()),
        compaction_response_id: Some("resp-summary".into()),
        latest_token_usage_record: None,
    })
}

fn legacy_checkpoint(summary: &str) -> RolloutItem {
    checkpoint_with_encoded_metadata(summary, json!({"version":1,"source":"legacy"}), None)
}

fn append_committed_checkpoint(items: &mut Vec<RolloutItem>, checkpoint: RolloutItem) {
    let mut prepared_batch = vec![
        checkpoint,
        RolloutItem::WorldState(WorldStateItem::full(serde_json::Map::new())),
    ];
    let checkpoint_hash = compaction_transaction::transaction_hash(&prepared_batch).unwrap();
    items.append(&mut prepared_batch);
    items.push(RolloutItem::RenCrowCompactionCommit { checkpoint_hash });
}

fn inventory_observations(count: usize) -> Vec<ObservationCoverage> {
    (0..count)
        .map(|index| {
            coverage(
                &format!("call-{index}"),
                &format!("input-{index}"),
                &format!("output-{index}"),
            )
        })
        .collect()
}

#[test]
fn inventory_reads_committed_owner_metadata_only_and_exact_call_lookup() {
    let summary = "summary one";
    let observations = inventory_observations(2);
    let expected_refs = observations
        .iter()
        .map(|observation| observation.reference.clone())
        .collect::<Vec<_>>();
    let mut items = Vec::new();
    append_committed_checkpoint(
        &mut items,
        prepared_checkpoint(summary, metadata(summary, observations), None),
    );

    let page = inventory_compaction_from_items(
        &items,
        THREAD,
        InventoryQuery::Page {
            checkpoint_hash: None,
            offset: 0,
        },
    )
    .unwrap();
    std::assert_eq!(page.items.len(), 2);
    std::assert_eq!(page.next_offset, None);
    std::assert_eq!(page.items[0].coverage.reference, expected_refs[0]);
    std::assert!(page.items[0].important);
    std::assert!(!page.items[1].important);
    let encoded_page = serde_json::to_value(&page).unwrap();
    std::assert!(
        encoded_page["items"][0]["coverage"]["call"]
            .get("excerpts")
            .is_none()
    );
    std::assert!(encoded_page["items"][0].get("text").is_none());

    let exact =
        inventory_compaction_from_items(&items, THREAD, InventoryQuery::CallId("call-1".into()))
            .unwrap();
    std::assert_eq!(exact.items.len(), 1);
    std::assert_eq!(exact.items[0].coverage.reference.call_id, "call-1");
    std::assert_eq!(
        exact.items[0].coverage.output.sha256,
        expected_refs[1].sha256
    );
    std::assert!(
        inventory_compaction_from_items(
            &items,
            THREAD,
            InventoryQuery::CallId("missing-call".into()),
        )
        .is_err()
    );
}

#[test]
fn inventory_excludes_uncommitted_and_legacy_but_rejects_raw_claimed_commit_hash() {
    let summary = "prepared only";
    let observations = inventory_observations(1);
    let prepared = prepared_checkpoint(summary, metadata(summary, observations.clone()), None);
    let uncommitted = vec![
        prepared,
        RolloutItem::WorldState(WorldStateItem::full(serde_json::Map::new())),
    ];
    std::assert!(
        inventory_compaction_from_items(
            &uncommitted,
            THREAD,
            InventoryQuery::Page {
                checkpoint_hash: None,
                offset: 0,
            },
        )
        .is_err()
    );

    let mut forged = metadata(summary, observations);
    forged.committed_transaction_hash = Some("b".repeat(64));
    let raw_forgery = vec![checkpoint_with_encoded_metadata(
        summary,
        serde_json::to_value(forged).unwrap(),
        None,
    )];
    std::assert!(
        inventory_compaction_from_items(
            &raw_forgery,
            THREAD,
            InventoryQuery::Page {
                checkpoint_hash: None,
                offset: 0,
            },
        )
        .is_err()
    );

    let legacy = vec![legacy_checkpoint(summary)];
    std::assert!(
        inventory_compaction_from_items(&legacy, THREAD, InventoryQuery::CallId("call-0".into()),)
            .is_err()
    );

    let duplicate_summary = "duplicate observations";
    let duplicate = coverage("duplicate-call", "input", "output");
    let mut items = Vec::new();
    append_committed_checkpoint(
        &mut items,
        prepared_checkpoint(
            duplicate_summary,
            metadata(duplicate_summary, vec![duplicate.clone(), duplicate]),
            None,
        ),
    );
    std::assert!(
        inventory_compaction_from_items(
            &items,
            THREAD,
            InventoryQuery::CallId("duplicate-call".into()),
        )
        .is_err()
    );
}

#[test]
fn latest_valid_empty_inventory_wins_and_malformed_latest_never_falls_back() {
    let mut items = Vec::new();
    let older_summary = "older with refs";
    append_committed_checkpoint(
        &mut items,
        prepared_checkpoint(
            older_summary,
            metadata(older_summary, inventory_observations(1)),
            None,
        ),
    );
    let empty_summary = "new valid empty";
    append_committed_checkpoint(
        &mut items,
        prepared_checkpoint(empty_summary, metadata(empty_summary, Vec::new()), None),
    );
    let empty = inventory_compaction_from_items(
        &items,
        THREAD,
        InventoryQuery::Page {
            checkpoint_hash: None,
            offset: 0,
        },
    )
    .unwrap();
    std::assert!(empty.items.is_empty());
    std::assert_eq!(empty.next_offset, None);

    let broken_summary = "malformed latest";
    let mut malformed = metadata(broken_summary, inventory_observations(1));
    malformed.observations[0].output.sha256 = "invalid".into();
    append_committed_checkpoint(
        &mut items,
        prepared_checkpoint(broken_summary, malformed, None),
    );
    std::assert!(
        inventory_compaction_from_items(
            &items,
            THREAD,
            InventoryQuery::Page {
                checkpoint_hash: None,
                offset: 0,
            },
        )
        .is_err()
    );
}

#[test]
fn later_legacy_checkpoint_does_not_hide_latest_v2_inventory() {
    let mut items = Vec::new();
    let summary = "latest v2 inventory";
    let observations = inventory_observations(1);
    append_committed_checkpoint(
        &mut items,
        prepared_checkpoint(summary, metadata(summary, observations.clone()), None),
    );
    items.push(legacy_checkpoint("later legacy checkpoint"));

    let page = inventory_compaction_from_items(
        &items,
        THREAD,
        InventoryQuery::Page {
            checkpoint_hash: None,
            offset: 0,
        },
    )
    .unwrap();
    std::assert_eq!(page.items.len(), 1);
    std::assert_eq!(page.items[0].coverage.reference, observations[0].reference);
}

#[test]
fn latest_v2_with_missing_version_is_malformed_and_never_falls_back() {
    for version in [None, Some(json!("2")), Some(json!(3))] {
        let mut items = Vec::new();
        let old_summary = "older v2 inventory";
        append_committed_checkpoint(
            &mut items,
            prepared_checkpoint(
                old_summary,
                metadata(old_summary, inventory_observations(1)),
                None,
            ),
        );

        let latest_summary = "invalid version on latest v2";
        let mut encoded =
            serde_json::to_value(metadata(latest_summary, inventory_observations(1))).unwrap();
        if let Some(version) = version {
            encoded["version"] = version;
        } else {
            encoded.as_object_mut().unwrap().remove("version");
        }
        append_committed_checkpoint(
            &mut items,
            checkpoint_with_encoded_metadata(latest_summary, encoded, None),
        );

        std::assert!(
            inventory_compaction_from_items(
                &items,
                THREAD,
                InventoryQuery::Page {
                    checkpoint_hash: None,
                    offset: 0,
                },
            )
            .is_err()
        );
    }
}

#[test]
fn latest_v2_summary_requires_compaction_summary_content_kind() {
    let summary = "typed summary required";
    let observations = inventory_observations(1);
    for wrong_kind in [None, Some("user.text")] {
        let mut checkpoint =
            prepared_checkpoint(summary, metadata(summary, observations.clone()), None);
        let RolloutItem::Compacted(compacted) = &mut checkpoint else {
            unreachable!();
        };
        let envelope = compacted
            .replacement_history
            .as_mut()
            .and_then(|history| history.last_mut())
            .unwrap();
        let ResponseItem::Message {
            internal_chat_message_metadata_passthrough,
            ..
        } = &mut envelope.item
        else {
            unreachable!();
        };
        *internal_chat_message_metadata_passthrough =
            wrong_kind.map(|kind| InternalChatMessageMetadataPassthrough {
                content_item_kinds: Some(vec![ContentItemKind(kind.into())]),
                ..Default::default()
            });
        let mut items = Vec::new();
        append_committed_checkpoint(&mut items, checkpoint);
        std::assert!(
            inventory_compaction_from_items(
                &items,
                THREAD,
                InventoryQuery::Page {
                    checkpoint_hash: None,
                    offset: 0,
                },
            )
            .is_err()
        );
    }
}

#[test]
fn inventory_pages_are_fixed_at_32_and_bound_to_latest_committed_checkpoint() {
    let mut items = Vec::new();
    let summary = "large metadata inventory";
    append_committed_checkpoint(
        &mut items,
        prepared_checkpoint(summary, metadata(summary, inventory_observations(33)), None),
    );
    let first = inventory_compaction_from_items(
        &items,
        THREAD,
        InventoryQuery::Page {
            checkpoint_hash: None,
            offset: 0,
        },
    )
    .unwrap();
    std::assert_eq!(first.items.len(), 32);
    std::assert_eq!(first.next_offset, Some(32));

    let second = inventory_compaction_from_items(
        &items,
        THREAD,
        InventoryQuery::Page {
            checkpoint_hash: Some(first.checkpoint_hash.clone()),
            offset: 32,
        },
    )
    .unwrap();
    std::assert_eq!(second.items.len(), 1);
    std::assert_eq!(second.next_offset, None);

    let newer_summary = "newer committed checkpoint";
    append_committed_checkpoint(
        &mut items,
        prepared_checkpoint(newer_summary, metadata(newer_summary, Vec::new()), None),
    );
    std::assert!(
        inventory_compaction_from_items(
            &items,
            THREAD,
            InventoryQuery::Page {
                checkpoint_hash: Some(first.checkpoint_hash),
                offset: 32,
            },
        )
        .is_err()
    );
}

#[test]
fn committed_inventory_requires_summary_body_and_compacted_message_to_match() {
    let summary = "checkpoint message";
    let checkpoint = prepared_checkpoint(
        summary,
        metadata(summary, inventory_observations(1)),
        Some("different final summary body"),
    );
    let mut items = Vec::new();
    append_committed_checkpoint(&mut items, checkpoint);
    std::assert!(
        inventory_compaction_from_items(
            &items,
            THREAD,
            InventoryQuery::Page {
                checkpoint_hash: None,
                offset: 0,
            },
        )
        .is_err()
    );
}
