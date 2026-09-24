use super::*;
use crate::archive_reference::ObservationReference;
use crate::archive_reference::content_sha256;
use crate::compaction_plan::ByteRange;
use crate::compaction_plan::SourceRef;
use crate::observation_projection::project_observation;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::TokenUsage;
use pretty_assertions::assert_eq;
use serde_json::json;

const THREAD_ID: &str = "thread-v2";
const SUMMARY: &str = "### Work Summary\nThe verified task remains partial.";

fn source_ref(id: &str, range: std::ops::Range<usize>) -> SourceRef {
    SourceRef {
        id: id.to_owned(),
        hash: content_sha256("original source fragment"),
        range: ByteRange {
            start: range.start,
            end: range.end,
        },
    }
}

fn observation(call_id: &str, call: &str, output: &str) -> crate::ObservationCoverage {
    let reference = ObservationReference::new(THREAD_ID, call_id, content_sha256(output));
    project_observation(&reference, "exec_command", call, output)
        .expect("fixture observation should project")
        .coverage
}

fn receipt(stage: CheckpointResponseStage, response_id: &str) -> CompactionModelResponseReceipt {
    CompactionModelResponseReceipt {
        stage,
        response_id: response_id.to_owned(),
        seconds: 1.25,
        usage: Some(TokenUsage::default()),
    }
}

fn metadata(mode: CompactionSelectionMode) -> RenCrowCompactionMetadataV2 {
    let model_selection = mode == CompactionSelectionMode::ModelSelection;
    let emergency = mode == CompactionSelectionMode::DeterministicEmergency;
    let coverage = observation("call-1", "ls", "done");
    RenCrowCompactionMetadataV2 {
        version: 2,
        snapshot_hash: "1".repeat(64),
        presentation_hash: model_selection.then(|| "2".repeat(64)),
        summary_hash: content_sha256(SUMMARY),
        semantic_summary_hash: Some(content_sha256(SUMMARY)),
        selection_mode: mode,
        plan_hash: model_selection.then(|| "3".repeat(64)),
        applied_refs: vec![source_ref("human-1", 3..9)],
        results: vec![],
        observations: vec![coverage.clone()],
        summary_covered_observations: vec![coverage.reference.clone()],
        important_refs: vec![coverage.reference],
        model: (!emergency).then(|| "qwen-test".to_owned()),
        effort: (!emergency).then_some(ReasoningEffort::High),
        responses: if model_selection {
            vec![
                receipt(CheckpointResponseStage::InstructionSelection, "resp-select"),
                receipt(CheckpointResponseStage::Summary, "resp-summary"),
            ]
        } else if emergency {
            vec![]
        } else {
            vec![receipt(CheckpointResponseStage::Summary, "resp-summary")]
        },
        transaction_following_items: None,
        committed_transaction_hash: None,
    }
}

#[test]
fn round_trip_valid_model_selection_preserves_exact_source_ranges_and_host_only_inventory() {
    let mut metadata = metadata(CompactionSelectionMode::ModelSelection);
    let source = metadata.applied_refs[0].clone();
    metadata
        .results
        .push(crate::compaction_plan::DerivedResult {
            source,
            evidence: source_ref("call-1", 0..4),
            text: "verified result".into(),
        });
    metadata
        .validate_for_checkpoint(THREAD_ID, SUMMARY)
        .expect("valid selected checkpoint should validate");

    let encoded = serde_json::to_value(&metadata).expect("metadata should serialize");
    assert_eq!(encoded["version"], json!(2));
    assert!(encoded.get("candidate_bundle").is_none());
    assert!(encoded.get("plan_review").is_none());
    assert!(encoded["observations"][0].get("excerpts").is_none());

    let parsed = RenCrowCompactionMetadataV2::parse_and_validate(encoded, THREAD_ID, SUMMARY)
        .expect("valid metadata should parse");
    assert_eq!(parsed, metadata);
    assert_eq!(parsed.applied_refs[0].range, ByteRange { start: 3, end: 9 });
}

#[test]
fn no_candidate_mode_can_carry_prior_refs_without_presentation_or_plan() {
    let mut metadata = metadata(CompactionSelectionMode::NoCandidates);
    metadata.effort = None;
    metadata
        .validate_for_checkpoint(THREAD_ID, SUMMARY)
        .expect("no-candidate summary can omit model effort");
}

#[test]
fn model_selection_with_zero_applied_refs_still_requires_selection_receipt_and_hashes() {
    let mut metadata = metadata(CompactionSelectionMode::ModelSelection);
    metadata.applied_refs.clear();
    metadata
        .validate_for_checkpoint(THREAD_ID, SUMMARY)
        .expect("selection mode reflects an actual model stage, not operation count");
}

#[test]
fn selection_mode_receipts_hashes_and_summary_body_must_agree() {
    let mut selected = metadata(CompactionSelectionMode::ModelSelection);
    selected.responses.pop();
    assert!(
        selected
            .validate_for_checkpoint(THREAD_ID, SUMMARY)
            .is_err()
    );

    let mut no_candidates = metadata(CompactionSelectionMode::NoCandidates);
    no_candidates.presentation_hash = Some("2".repeat(64));
    assert!(
        no_candidates
            .validate_for_checkpoint(THREAD_ID, SUMMARY)
            .is_err()
    );

    let selected = metadata(CompactionSelectionMode::ModelSelection);
    assert!(
        selected
            .validate_for_checkpoint(THREAD_ID, "summary without prefix")
            .is_err()
    );

    let mut no_candidates = metadata(CompactionSelectionMode::NoCandidates);
    no_candidates
        .results
        .push(crate::compaction_plan::DerivedResult {
            source: no_candidates.applied_refs[0].clone(),
            evidence: source_ref("call-1", 0..4),
            text: "result without model selection".into(),
        });
    assert!(
        no_candidates
            .validate_for_checkpoint(THREAD_ID, SUMMARY)
            .is_err()
    );
}

#[test]
fn observation_inventory_requires_unique_same_thread_ids_and_important_refs_in_inventory() {
    let mut duplicate = metadata(CompactionSelectionMode::NoCandidates);
    duplicate
        .observations
        .push(observation("call-1", "ls", "other output"));
    assert!(
        duplicate
            .validate_for_checkpoint(THREAD_ID, SUMMARY)
            .is_err()
    );

    let mut wrong_thread = metadata(CompactionSelectionMode::NoCandidates);
    wrong_thread.observations[0].reference.thread_id = "other-thread".into();
    assert!(
        wrong_thread
            .validate_for_checkpoint(THREAD_ID, SUMMARY)
            .is_err()
    );

    let mut invalid_important_ref = metadata(CompactionSelectionMode::NoCandidates);
    invalid_important_ref.important_refs[0].sha256 = "f".repeat(64);
    assert!(
        invalid_important_ref
            .validate_for_checkpoint(THREAD_ID, SUMMARY)
            .is_err()
    );
}

#[test]
fn metadata_hashes_references_receipts_and_schema_are_strict() {
    let mut invalid = metadata(CompactionSelectionMode::ModelSelection);
    invalid.snapshot_hash = "bad".into();
    assert!(invalid.validate_for_checkpoint(THREAD_ID, SUMMARY).is_err());

    let mut invalid = metadata(CompactionSelectionMode::ModelSelection);
    let start = invalid.applied_refs[0].range.start;
    invalid.applied_refs[0].range.end = start;
    assert!(invalid.validate_for_checkpoint(THREAD_ID, SUMMARY).is_err());

    let mut invalid = metadata(CompactionSelectionMode::ModelSelection);
    invalid.responses[1].response_id = invalid.responses[0].response_id.clone();
    assert!(invalid.validate_for_checkpoint(THREAD_ID, SUMMARY).is_err());

    let mut invalid = metadata(CompactionSelectionMode::ModelSelection);
    invalid.model = Some(" ".into());
    assert!(invalid.validate_for_checkpoint(THREAD_ID, SUMMARY).is_err());

    let mut invalid = metadata(CompactionSelectionMode::ModelSelection);
    invalid.version = 1;
    assert!(invalid.validate_for_checkpoint(THREAD_ID, SUMMARY).is_err());

    let mut invalid = metadata(CompactionSelectionMode::ModelSelection);
    invalid.observations[0].output.sha256 = "f".repeat(64);
    assert!(invalid.validate_for_checkpoint(THREAD_ID, SUMMARY).is_err());

    let mut encoded = serde_json::to_value(metadata(CompactionSelectionMode::NoCandidates))
        .expect("metadata should serialize");
    encoded["unrecognized"] = json!(true);
    assert!(RenCrowCompactionMetadataV2::parse_and_validate(encoded, THREAD_ID, SUMMARY,).is_err());

    // The handled-observation set is part of the frozen schema, not an optional extension.
    let mut encoded = serde_json::to_value(metadata(CompactionSelectionMode::NoCandidates))
        .expect("metadata should serialize");
    encoded
        .as_object_mut()
        .expect("metadata object")
        .remove("summary_covered_observations");
    assert!(RenCrowCompactionMetadataV2::parse_and_validate(encoded, THREAD_ID, SUMMARY).is_err());
}

#[test]
fn deterministic_emergency_has_no_model_receipts_selection_or_results() {
    let carried = metadata(CompactionSelectionMode::DeterministicEmergency);
    carried
        .validate_for_checkpoint(THREAD_ID, SUMMARY)
        .expect("emergency may carry the previous accepted semantic summary");
    let encoded = serde_json::to_value(&carried).expect("metadata should serialize");
    assert_eq!(encoded["selection_mode"], json!("deterministic_emergency"));
    assert!(encoded.get("model").is_none());
    assert!(encoded.get("effort").is_none());
    assert_eq!(encoded["responses"], json!([]));
    assert_eq!(
        RenCrowCompactionMetadataV2::parse_and_validate(encoded, THREAD_ID, SUMMARY),
        Ok(carried.clone())
    );

    // A host placeholder is not a semantic summary.
    let mut placeholder = carried.clone();
    placeholder.semantic_summary_hash = None;
    placeholder
        .validate_for_checkpoint(THREAD_ID, SUMMARY)
        .expect("emergency placeholder has no semantic summary hash");

    let mut invalid = carried.clone();
    invalid.model = Some("qwen-test".into());
    assert!(invalid.validate_for_checkpoint(THREAD_ID, SUMMARY).is_err());

    let mut invalid = carried.clone();
    invalid.effort = Some(ReasoningEffort::High);
    assert!(invalid.validate_for_checkpoint(THREAD_ID, SUMMARY).is_err());

    let mut invalid = carried.clone();
    invalid.responses = vec![receipt(CheckpointResponseStage::Summary, "fake-summary")];
    assert!(invalid.validate_for_checkpoint(THREAD_ID, SUMMARY).is_err());

    let mut invalid = carried.clone();
    invalid.plan_hash = Some("3".repeat(64));
    assert!(invalid.validate_for_checkpoint(THREAD_ID, SUMMARY).is_err());

    let mut invalid = carried.clone();
    invalid.presentation_hash = Some("2".repeat(64));
    assert!(invalid.validate_for_checkpoint(THREAD_ID, SUMMARY).is_err());

    let mut invalid = carried.clone();
    let source = invalid.applied_refs[0].clone();
    invalid.results.push(crate::compaction_plan::DerivedResult {
        source,
        evidence: source_ref("call-1", 0..4),
        text: "invented result".into(),
    });
    assert!(invalid.validate_for_checkpoint(THREAD_ID, SUMMARY).is_err());

    let mut invalid = carried;
    invalid.semantic_summary_hash = Some("4".repeat(64));
    assert!(invalid.validate_for_checkpoint(THREAD_ID, SUMMARY).is_err());
}

#[test]
fn model_generated_summaries_require_a_model_and_their_own_semantic_hash() {
    for mode in [
        CompactionSelectionMode::NoCandidates,
        CompactionSelectionMode::ModelSelection,
    ] {
        let mut invalid = metadata(mode);
        invalid.model = None;
        invalid.effort = None;
        assert!(invalid.validate_for_checkpoint(THREAD_ID, SUMMARY).is_err());

        let mut invalid = metadata(mode);
        invalid.semantic_summary_hash = None;
        assert!(invalid.validate_for_checkpoint(THREAD_ID, SUMMARY).is_err());

        let mut invalid = metadata(mode);
        invalid.semantic_summary_hash = Some("4".repeat(64));
        assert!(invalid.validate_for_checkpoint(THREAD_ID, SUMMARY).is_err());
    }
}

#[test]
fn summary_covered_observations_are_unique_inventory_refs_but_need_not_cover_all() {
    // An observation kept only as a deterministic marker stays in inventory without coverage.
    let mut partial = metadata(CompactionSelectionMode::DeterministicEmergency);
    partial
        .observations
        .push(observation("call-2", "cat log", "marker only"));
    partial
        .validate_for_checkpoint(THREAD_ID, SUMMARY)
        .expect("inventory may exceed summary coverage");

    let mut absent = metadata(CompactionSelectionMode::NoCandidates);
    absent.summary_covered_observations[0].sha256 = "f".repeat(64);
    assert!(absent.validate_for_checkpoint(THREAD_ID, SUMMARY).is_err());

    let mut duplicate = metadata(CompactionSelectionMode::NoCandidates);
    let reference = duplicate.summary_covered_observations[0].clone();
    duplicate.summary_covered_observations.push(reference);
    assert!(
        duplicate
            .validate_for_checkpoint(THREAD_ID, SUMMARY)
            .is_err()
    );

    let mut wrong_thread = metadata(CompactionSelectionMode::NoCandidates);
    wrong_thread.summary_covered_observations[0].thread_id = "other-thread".into();
    assert!(
        wrong_thread
            .validate_for_checkpoint(THREAD_ID, SUMMARY)
            .is_err()
    );
}

#[test]
fn transaction_owner_fields_allow_candidate_prepared_or_committed_shapes_but_not_both() {
    let mut metadata = metadata(CompactionSelectionMode::NoCandidates);
    metadata.transaction_following_items = Some(2);
    metadata
        .validate_for_checkpoint(THREAD_ID, SUMMARY)
        .expect("prepared transaction shape is valid metadata");

    metadata.transaction_following_items = None;
    metadata.committed_transaction_hash = Some("a".repeat(64));
    metadata
        .validate_for_checkpoint(THREAD_ID, SUMMARY)
        .expect("normalized committed metadata shape is valid");

    metadata.transaction_following_items = Some(2);
    assert!(
        metadata
            .validate_for_checkpoint(THREAD_ID, SUMMARY)
            .is_err()
    );
}
