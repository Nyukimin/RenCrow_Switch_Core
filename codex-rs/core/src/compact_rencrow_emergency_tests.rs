//! Level 2 deterministic emergency builder, validator, and metadata (Part 2 §25–33, §41–55).

use super::*;
use crate::compact::SUMMARY_PREFIX;
use codex_history::ObservationReference;
use codex_history::compaction_plan::ByteRange;
use codex_history::compaction_preprocess::prune_known_obsolete;
use codex_history::compaction_selection::InstructionSelectionApplication;
use codex_protocol::ResponseItemId;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::InternalChatMessageMetadataPassthrough;
use codex_rollout::PreparedCompactionSourcePair;
use pretty_assertions::assert_eq;
use serde_json::json;

const THREAD_ID: &str = "thread-v2-emergency";
const SMALL_OUTPUT: &str = "ok";

fn message(role: &str, text: &str, id: &str) -> ResponseItemEnvelope {
    ResponseItemEnvelope::new(ResponseItem::Message {
        id: Some(ResponseItemId::from_server(id.into())),
        role: role.into(),
        content: vec![ContentItem::InputText { text: text.into() }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    })
}

fn human(text: &str, id: &str) -> ResponseItemEnvelope {
    let mut item = message("user", text, id);
    item.metadata.get_or_insert_default().rencrow_input = Some(json!({
        "version": 1,
        "author": "human",
        "thread_id": THREAD_ID,
        "selected_text": text,
        "receipt_hash": format!("receipt-{id}"),
    }));
    item
}

fn normal_summary(text: &str) -> ResponseItemEnvelope {
    let body = format!("{SUMMARY_PREFIX}\n{text}");
    let mut summary = message("user", &body, "normal-summary");
    if let ResponseItem::Message {
        internal_chat_message_metadata_passthrough,
        ..
    } = &mut summary.item
    {
        *internal_chat_message_metadata_passthrough =
            Some(InternalChatMessageMetadataPassthrough {
                content_item_kinds: Some(vec![ContentItemKind("compaction.summary".into())]),
                ..Default::default()
            });
    }
    summary.metadata.get_or_insert_default().rencrow_compaction =
        Some(json!({"version": 2, "summary_hash": content_sha256(&body)}));
    summary
}

fn function_call(call_id: &str) -> ResponseItemEnvelope {
    ResponseItemEnvelope::new(ResponseItem::FunctionCall {
        id: Some(ResponseItemId::from_server(format!("{call_id}-item"))),
        name: "exec_command".into(),
        namespace: None,
        arguments: format!("{{\"cmd\":\"run {call_id}\"}}"),
        encrypted_function_args: None,
        call_id: call_id.into(),
        internal_chat_message_metadata_passthrough: None,
    })
}

fn function_output(call_id: &str, output: &str) -> ResponseItemEnvelope {
    ResponseItemEnvelope::new(ResponseItem::FunctionCallOutput {
        id: Some(ResponseItemId::from_server(format!(
            "{call_id}-output-item"
        ))),
        call_id: Some(call_id.into()),
        name: Some("exec_command".into()),
        namespace: None,
        output: FunctionCallOutputPayload::from_text(output.into()),
        internal_chat_message_metadata_passthrough: None,
    })
}

fn pair<'a>(
    call_index: usize,
    call_id: &str,
    call_text: &'a str,
    output_text: &'a str,
    kind: PreparedCompactionReferenceKind,
) -> PreparedCompactionSourcePair<'a> {
    PreparedCompactionSourcePair {
        call_index,
        output_index: call_index + 1,
        reference: ObservationReference::new(THREAD_ID, call_id, content_sha256(output_text)),
        reference_kind: kind,
        output_total_bytes: output_text.len(),
        tool_name: "exec_command",
        canonical_call_input: Some(call_text),
        canonical_output_text: Some(output_text),
        terminal_reference: None,
    }
}

fn item_id(envelope: &ResponseItemEnvelope) -> Option<&str> {
    envelope
        .item
        .id()
        .map(codex_protocol::ResponseItemId::as_str)
}

/// History: an unhandled early pair before the Normal summary, later Work, a large pair, and an
/// unannotated developer item. The Human carries an accepted removal that still applies.
struct Fixture {
    originals: Vec<ResponseItemEnvelope>,
    large_output: String,
    early_call: String,
    large_call: String,
}

impl Fixture {
    fn new() -> Self {
        let early_call = "{\"cmd\":\"run early\"}".to_owned();
        let large_call = "{\"cmd\":\"run large\"}".to_owned();
        let large_output = format!("HEAD{}TAIL", "x".repeat(20_000));
        let originals = vec![
            human("Keep Japanese. Use old-label.", "human-a"),
            message("assistant", "old summarized work", "old-work"),
            function_call("early"),
            function_output("early", SMALL_OUTPUT),
            normal_summary("previous verified state"),
            message("assistant", "new unsummarized work", "new-work"),
            function_call("large"),
            function_output("large", &large_output),
            message("developer", "unannotated context", "developer"),
        ];
        Self {
            originals,
            large_output,
            early_call,
            large_call,
        }
    }

    fn prepared(&self) -> PreparedCompactionSources<'_> {
        PreparedCompactionSources {
            pairs: vec![
                pair(
                    2,
                    "early",
                    &self.early_call,
                    SMALL_OUTPUT,
                    PreparedCompactionReferenceKind::Fresh,
                ),
                pair(
                    6,
                    "large",
                    &self.large_call,
                    &self.large_output,
                    PreparedCompactionReferenceKind::Fresh,
                ),
            ],
            protected_indices: vec![0, 1, 4, 5, 8],
        }
    }
}

struct Prepared {
    input: CandidateInput,
    pruning: InstructionPruning,
    native: NativeProjection,
    projections: Vec<(usize, usize, ObservationProjection)>,
}

fn prepare(fixture: &Fixture) -> Prepared {
    let prepared = fixture.prepared();
    let input = super::super::history::capture(
        &fixture.originals,
        "binding".into(),
        THREAD_ID,
        vec![],
        &super::super::history::verified_pair_projections(&prepared),
    )
    .unwrap();
    let known = input
        .snapshot()
        .unwrap()
        .reference(
            "human-a",
            ByteRange {
                start: "Keep Japanese. ".len(),
                end: "Keep Japanese. Use old-label.".len(),
            },
        )
        .unwrap();
    let pruning = prune_known_obsolete(&input, &[known]).unwrap();
    assert_eq!(pruning.applied.len(), 1);
    let native = super::super::native::filter_retained_instructions(
        &fixture.originals,
        &input,
        InstructionSelectionApplication {
            pruning: pruning.clone(),
            plan_hash: None,
            results: vec![],
        },
    )
    .unwrap();
    let projections = super::super::observation::project_unhandled_observations(
        &fixture.originals,
        &prepared,
        &[],
    )
    .unwrap();
    Prepared {
        input,
        pruning,
        native,
        projections,
    }
}

fn plan<'a>(fixture: &'a Fixture, prepared: &'a Prepared) -> EmergencyPlan<'a> {
    let (summary_text, semantic) = emergency_summary_text(&fixture.originals, Some(4)).unwrap();
    assert!(semantic);
    EmergencyPlan {
        originals: &fixture.originals,
        input: &prepared.input,
        native: &prepared.native,
        unhandled_pairs: vec![(2, 3), (6, 7)],
        markers: select_observation_markers(
            &fixture.originals,
            &fixture.prepared(),
            &prepared.projections,
        ),
        work_boundary: Some(4),
        summary_text,
    }
}

#[test]
fn emergency_carries_the_exact_previous_summary_or_a_non_semantic_placeholder() {
    let fixture = Fixture::new();
    let carried = emergency_summary_text(&fixture.originals, Some(4)).unwrap();
    assert_eq!(
        carried,
        (
            format!("{SUMMARY_PREFIX}\nprevious verified state"),
            /*semantic*/ true
        )
    );
    assert_eq!(
        emergency_summary_text(&fixture.originals, None).unwrap(),
        (
            format!("{SUMMARY_PREFIX}\n{EMERGENCY_PLACEHOLDER}"),
            /*semantic*/ false
        )
    );
    // Only a user summary text item can be carried.
    assert!(emergency_summary_text(&fixture.originals, Some(1)).is_err());
    let unprefixed = vec![message("user", "plain text", "plain")];
    assert!(emergency_summary_text(&unprefixed, Some(0)).is_err());
}

#[test]
fn emergency_markers_only_large_fresh_outputs() {
    let fixture = Fixture::new();
    let prepared = prepare(&fixture);
    let markers = select_observation_markers(
        &fixture.originals,
        &fixture.prepared(),
        &prepared.projections,
    );
    assert_eq!(
        markers.iter().map(|(index, _)| *index).collect::<Vec<_>>(),
        vec![7]
    );

    // Existing V1 references and V2 markers are never re-marked.
    let mut sources = fixture.prepared();
    for pair in &mut sources.pairs {
        pair.reference_kind = PreparedCompactionReferenceKind::ObservationMarker;
    }
    assert!(
        select_observation_markers(&fixture.originals, &sources, &prepared.projections).is_empty()
    );
}

#[test]
fn emergency_keeps_unsummarized_state_and_drops_only_summarized_work() {
    let fixture = Fixture::new();
    let prepared = prepare(&fixture);
    let plan = plan(&fixture, &prepared);

    let candidate = plan.build(&[]).unwrap();

    assert_eq!(
        candidate.iter().map(item_id).collect::<Vec<_>>(),
        vec![
            Some("human-a"),
            Some("early-item"),
            Some("early-output-item"),
            Some("new-work"),
            Some("large-item"),
            Some("large-output-item"),
            Some("developer"),
            None,
        ]
    );
    // The accepted removal applies; nothing new is removed from the Human.
    assert_eq!(
        content_text(match &candidate[0].item {
            ResponseItem::Message { content, .. } => content,
            _ => unreachable!(),
        }),
        "Keep Japanese. "
    );
    // The early pair precedes the summary but is unhandled, so it stays raw.
    assert_eq!(candidate[2], fixture.originals[3]);
    // The large output keeps its identity and becomes a verified marker.
    let marker = &candidate[5];
    assert_eq!(marker.item.id(), fixture.originals[7].item.id());
    let body = live_output_text(marker).unwrap();
    assert!(body.contains("\"rencrow_observation\":true"));
    assert!(body.len() < fixture.large_output.len());
    assert!(
        marker
            .metadata
            .as_ref()
            .unwrap()
            .rencrow_observation_projection
            .is_some()
    );
    // The previous summary is carried exactly as the final typed summary.
    assert_eq!(
        content_text(match &candidate[7].item {
            ResponseItem::Message { content, .. } => content,
            _ => unreachable!(),
        }),
        format!("{SUMMARY_PREFIX}\nprevious verified state")
    );
    assert!(is_user_summary(&candidate[7]));
    assert_eq!(plan.validate(&[], &candidate), Ok(()));
}

#[test]
fn emergency_validation_rejects_dropped_changed_extra_or_misplaced_items() {
    let fixture = Fixture::new();
    let prepared = prepare(&fixture);
    let plan = plan(&fixture, &prepared);
    let candidate = plan.build(&[]).unwrap();

    let mut dropped_human = candidate.clone();
    dropped_human.remove(0);
    assert!(plan.validate(&[], &dropped_human).is_err());

    let mut changed_work = candidate.clone();
    changed_work[3] = message("assistant", "rewritten work", "new-work");
    assert!(plan.validate(&[], &changed_work).is_err());

    let mut edited_marker = candidate.clone();
    if let ResponseItem::FunctionCallOutput { output, .. } = &mut edited_marker[5].item {
        let text = output
            .text_content()
            .unwrap()
            .replace("Archived", "Current");
        *output = FunctionCallOutputPayload::from_text(text);
    }
    assert!(plan.validate(&[], &edited_marker).is_err());

    let mut raw_instead_of_marker = candidate.clone();
    raw_instead_of_marker[5] = fixture.originals[7].clone();
    assert!(plan.validate(&[], &raw_instead_of_marker).is_err());

    let mut extra = candidate.clone();
    extra.insert(3, fixture.originals[1].clone());
    assert!(plan.validate(&[], &extra).is_err());

    let mut other_summary = candidate;
    *other_summary.last_mut().unwrap() = ResponseItemEnvelope::new(ContextualUserFragment::into(
        CompactionSummary::new(format!("{SUMMARY_PREFIX}\n{EMERGENCY_PLACEHOLDER}")),
    ));
    assert!(plan.validate(&[], &other_summary).is_err());

    // Initial context must sit in the host slot.
    let context = vec![message("developer", "fresh environment", "context")];
    let with_context = plan.build(&context).unwrap();
    assert_eq!(plan.validate(&context, &with_context), Ok(()));
    let mut misplaced = with_context.clone();
    let slot = misplaced
        .iter()
        .position(|item| item_id(item) == Some("context"))
        .unwrap();
    let moved = misplaced.remove(slot);
    misplaced.insert(misplaced.len() - 1, moved);
    if slot != misplaced.len() - 2 {
        assert!(plan.validate(&context, &misplaced).is_err());
    }
    assert!(plan.validate(&[], &with_context).is_err());
}

#[test]
fn emergency_metadata_carries_semantic_state_and_adds_markers_only_to_inventory() {
    let fixture = Fixture::new();
    let prepared = prepare(&fixture);
    let plan = plan(&fixture, &prepared);
    let covered = ObservationReference::new(THREAD_ID, "covered", content_sha256("covered"));
    let covered_coverage =
        codex_history::project_observation(&covered, "exec_command", "{}", "covered")
            .unwrap()
            .coverage;
    let previous = RenCrowCompactionMetadataV2 {
        version: 2,
        snapshot_hash: "a".repeat(64),
        presentation_hash: None,
        summary_hash: "b".repeat(64),
        semantic_summary_hash: Some("b".repeat(64)),
        selection_mode: CompactionSelectionMode::NoCandidates,
        plan_hash: None,
        applied_refs: vec![],
        results: vec![],
        observations: vec![covered_coverage.clone()],
        summary_covered_observations: vec![covered.clone()],
        important_refs: vec![covered.clone()],
        model: Some("model".into()),
        effort: None,
        responses: vec![],
        transaction_following_items: None,
        committed_transaction_hash: None,
    };

    let metadata = emergency_checkpoint_metadata(
        &plan.summary_text,
        /*semantic*/ true,
        prepared.input.snapshot().unwrap().hash().to_owned(),
        &prepared.pruning,
        Some(&previous),
        &plan.markers,
    )
    .unwrap();

    assert_eq!(
        metadata.selection_mode,
        CompactionSelectionMode::DeterministicEmergency
    );
    assert_eq!(metadata.model, None);
    assert!(metadata.responses.is_empty());
    assert_eq!(metadata.applied_refs, prepared.pruning.applied);
    assert_eq!(metadata.summary_covered_observations, vec![covered.clone()]);
    assert_eq!(metadata.important_refs, vec![covered]);
    assert_eq!(
        metadata.observations,
        vec![covered_coverage, plan.markers[0].1.coverage.clone()]
    );
    assert_eq!(
        metadata.semantic_summary_hash,
        Some(content_sha256(&plan.summary_text))
    );

    let mut candidate = plan.build(&[]).unwrap();
    let summary = candidate.last_mut().unwrap();
    summary.metadata.get_or_insert_default().rencrow_compaction =
        Some(serde_json::to_value(&metadata).unwrap());
    assert_eq!(
        validate_emergency_summary(summary, &plan.summary_text, THREAD_ID, &metadata),
        Ok(())
    );
    let mut advanced = metadata;
    advanced.summary_covered_observations.clear();
    assert!(validate_emergency_summary(summary, &plan.summary_text, THREAD_ID, &advanced).is_err());

    // A placeholder is not a semantic summary.
    let placeholder = emergency_checkpoint_metadata(
        &format!("{SUMMARY_PREFIX}\n{EMERGENCY_PLACEHOLDER}"),
        /*semantic*/ false,
        "a".repeat(64),
        &prepared.pruning,
        None,
        &[],
    )
    .unwrap();
    assert_eq!(placeholder.semantic_summary_hash, None);
    assert!(placeholder.observations.is_empty());
}
