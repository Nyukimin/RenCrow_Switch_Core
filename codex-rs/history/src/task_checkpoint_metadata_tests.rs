use super::*;
use crate::compaction_checkpoint_metadata::CheckpointResponseStage;
use crate::compaction_checkpoint_metadata::CompactionModelResponseReceipt;
use crate::compaction_checkpoint_metadata::CompactionSelectionMode;
use crate::task_context::*;
use codex_protocol::openai_models::ReasoningEffort;
use pretty_assertions::assert_eq;
use serde_json::json;

const THREAD: &str = "task-thread";
const SUMMARY: &str = "task boundary summary";

fn fence() -> TaskBoundarySourceFence {
    TaskBoundarySourceFence {
        history_hash: "a".repeat(64),
        removed_refs: vec![TaskBoundarySourceRef {
            id: "item-1".into(),
            hash: "b".repeat(64),
        }],
    }
}

fn stored_task_state() -> Value {
    let candidate = BoundaryCandidate {
        version: 1,
        mode: BoundaryMode::Task,
        thread_id: THREAD.into(),
        root_task_id: "root".into(),
        closed_task_id: "task-a".into(),
        revision: 7,
        closed_task_revision: 4,
        goal: "Ship feature".into(),
        tasks: vec![TaskState {
            id: "task-a".into(),
            purpose: "Implement".into(),
            status: TaskStatus::Completed,
            depends_on: vec![],
        }],
        current_task_id: None,
        artifact_ref: "artifact:1".into(),
        verification_ref: "check:1".into(),
        decisions: vec![],
        constraints: vec![],
        completion_state: None,
    };
    let facts = HostBoundaryFacts {
        thread_id: THREAD.into(),
        authorized_parent: true,
        root_task_id: "root".into(),
        current_revision: 7,
        closed_task_id: "task-a".into(),
        closed_task_revision: 4,
        artifact_ref: Some("artifact:1".into()),
        artifact_revision: Some(4),
        artifact_result: Some("done".into()),
        verification_ref: Some("check:1".into()),
        verification_artifact_ref: Some("artifact:1".into()),
        verification_artifact_revision: Some(4),
        verification_succeeded: true,
        parent_review_ref: Some("review:1".into()),
        parent_review_matches: true,
        unresolved_effects: vec![],
        accepted_completed: Default::default(),
    };
    serde_json::to_value(
        validate_boundary(candidate, &facts)
            .unwrap()
            .into_stored_state(None, None, "nonce-1".into())
            .unwrap(),
    )
    .unwrap()
}

fn capacity_metadata() -> RenCrowCompactionMetadataV2 {
    RenCrowCompactionMetadataV2 {
        version: 2,
        snapshot_hash: "c".repeat(64),
        presentation_hash: None,
        summary_hash: content_sha256(SUMMARY),
        semantic_summary_hash: Some(content_sha256(SUMMARY)),
        selection_mode: CompactionSelectionMode::NoCandidates,
        plan_hash: None,
        applied_refs: vec![],
        results: vec![],
        observations: vec![],
        summary_covered_observations: vec![],
        important_refs: vec![],
        model: Some("test-model".into()),
        effort: Some(ReasoningEffort::High),
        responses: vec![CompactionModelResponseReceipt {
            stage: CheckpointResponseStage::Summary,
            response_id: "response-1".into(),
            seconds: 1.0,
            usage: None,
        }],
        transaction_following_items: None,
        committed_transaction_hash: None,
    }
}

#[test]
fn task_boundary_round_trips_without_model_receipts_or_v2_metadata() {
    let state = stored_task_state();
    let checkpoint =
        RenCrowTaskCheckpointMetadataV3::task_boundary(SUMMARY, state.clone(), fence());
    checkpoint
        .validate_for_checkpoint(THREAD, SUMMARY)
        .expect("task boundary should validate");
    let encoded = serde_json::to_value(&checkpoint).unwrap();
    assert_eq!(encoded["version"], json!(3));
    assert_eq!(encoded["mode"], json!("task_boundary"));
    assert!(encoded.get("capacity_metadata").is_none());
    assert!(encoded.get("responses").is_none());
    let parsed = RenCrowTaskCheckpointMetadataV3::parse_and_validate(encoded, THREAD, SUMMARY)
        .expect("task boundary should parse");
    assert_eq!(parsed.task_state, Some(state));
}

#[test]
fn capacity_wraps_strict_v2_and_carries_task_state_and_transaction_fields() {
    let mut v2 = capacity_metadata();
    v2.transaction_following_items = Some(7);
    let state = stored_task_state();
    let wrapped =
        RenCrowTaskCheckpointMetadataV3::capacity(SUMMARY, Some(state.clone()), v2.clone());
    wrapped
        .validate_for_checkpoint(THREAD, SUMMARY)
        .expect("wrapped capacity metadata should validate");
    assert_eq!(wrapped.task_state, Some(state));
    assert_eq!(wrapped.transaction_following_items, Some(7));
    let parsed = RenCrowTaskCheckpointMetadataV3::parse_and_validate(
        serde_json::to_value(wrapped).unwrap(),
        THREAD,
        SUMMARY,
    )
    .unwrap();
    assert_eq!(parsed.into_capacity_metadata().unwrap(), v2);
}

#[test]
fn task_boundary_rejects_changed_summary_and_invalid_source_fence() {
    let checkpoint =
        RenCrowTaskCheckpointMetadataV3::task_boundary(SUMMARY, json!({"revision": 1}), fence());
    assert!(
        checkpoint
            .validate_for_checkpoint(THREAD, "changed")
            .is_err()
    );
    let mut invalid = checkpoint;
    invalid.source_fence.as_mut().unwrap().removed_refs[0].hash = "not-a-hash".into();
    assert!(invalid.validate_for_checkpoint(THREAD, SUMMARY).is_err());
}

#[test]
fn task_boundary_requires_task_state_and_does_not_fall_back_to_v2() {
    let mut encoded = serde_json::to_value(RenCrowTaskCheckpointMetadataV3::task_boundary(
        SUMMARY,
        stored_task_state(),
        fence(),
    ))
    .unwrap();
    encoded["task_state"] = Value::Null;
    assert!(RenCrowTaskCheckpointMetadataV3::parse_and_validate(encoded, THREAD, SUMMARY).is_err());

    let mut unknown = serde_json::to_value(RenCrowTaskCheckpointMetadataV3::task_boundary(
        SUMMARY,
        stored_task_state(),
        fence(),
    ))
    .unwrap();
    unknown["version"] = json!(4);
    assert!(RenCrowTaskCheckpointMetadataV3::parse_and_validate(unknown, THREAD, SUMMARY).is_err());
}

#[test]
fn capacity_rejects_mismatched_outer_transaction_fence() {
    let wrapped = RenCrowTaskCheckpointMetadataV3::capacity(SUMMARY, None, capacity_metadata());
    let mut encoded = serde_json::to_value(wrapped).unwrap();
    encoded["transaction_following_items"] = json!(9);
    assert!(RenCrowTaskCheckpointMetadataV3::parse_and_validate(encoded, THREAD, SUMMARY).is_err());
}
