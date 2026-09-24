//! V2 observation markers are verified against canonical raw observations (Part 2 §38–40).

use super::PreparedCompactionReferenceKind;
use super::prepare_compaction_sources;
use codex_history::CodexHarnessMetadata;
use codex_history::ObservationReference;
use codex_history::ResponseItemEnvelope;
use codex_history::RolloutItem;
use codex_history::archive_reference::content_sha256;
use codex_history::observation_marker::observation_marker_body;
use codex_history::observation_marker::observation_marker_metadata;
use codex_protocol::ThreadId;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use pretty_assertions::assert_eq;
use std::collections::HashSet;

const CALL_INPUT: &str = "{\"path\":\"large.log\"}";

fn function_call(call_id: &str) -> ResponseItemEnvelope {
    ResponseItemEnvelope::new(ResponseItem::FunctionCall {
        id: None,
        name: "read_document".into(),
        namespace: None,
        arguments: CALL_INPUT.into(),
        encrypted_function_args: None,
        call_id: call_id.into(),
        internal_chat_message_metadata_passthrough: None,
    })
}

fn function_output(call_id: &str, body: &str) -> ResponseItemEnvelope {
    let mut output = ResponseItemEnvelope::new(ResponseItem::FunctionCallOutput {
        id: None,
        call_id: Some(call_id.into()),
        name: Some("read_document".into()),
        namespace: None,
        output: FunctionCallOutputPayload::from_text(body.into()),
        internal_chat_message_metadata_passthrough: None,
    });
    output.metadata = Some(CodexHarnessMetadata {
        history_truncation_token_limit: Some(12_000),
        ..Default::default()
    });
    output
}

fn custom_call(call_id: &str) -> ResponseItemEnvelope {
    ResponseItemEnvelope::new(ResponseItem::CustomToolCall {
        id: None,
        status: Some("completed".into()),
        call_id: call_id.into(),
        name: "read_document".into(),
        namespace: None,
        input: CALL_INPUT.into(),
        internal_chat_message_metadata_passthrough: None,
    })
}

fn custom_output(call_id: &str, body: &str) -> ResponseItemEnvelope {
    ResponseItemEnvelope::new(ResponseItem::CustomToolCallOutput {
        id: None,
        call_id: call_id.into(),
        name: Some("read_document".into()),
        output: FunctionCallOutputPayload::from_text(body.into()),
        internal_chat_message_metadata_passthrough: None,
    })
}

/// Replace a raw output with the marker that Emergency would write for it.
fn marker_for(
    thread: &ThreadId,
    call_id: &str,
    raw_output: &ResponseItemEnvelope,
    raw: &str,
) -> ResponseItemEnvelope {
    let reference = ObservationReference::new(thread.to_string(), call_id, content_sha256(raw));
    let projection =
        codex_history::project_observation(&reference, "read_document", CALL_INPUT, raw).unwrap();
    let mut marker = raw_output.clone();
    let body = observation_marker_body(&projection);
    match &mut marker.item {
        ResponseItem::FunctionCallOutput { output, .. }
        | ResponseItem::CustomToolCallOutput { output, .. } => {
            *output = FunctionCallOutputPayload::from_text(body);
        }
        _ => unreachable!(),
    }
    marker.metadata = Some(
        observation_marker_metadata(raw_output.metadata.as_ref(), &projection.coverage).unwrap(),
    );
    marker
}

fn raw() -> String {
    format!(
        "{}{}{}",
        "head ".repeat(600),
        "middle ".repeat(600),
        "tail ".repeat(600)
    )
}

#[test]
fn verified_function_and_custom_markers_reuse_canonical_raw_text() {
    let thread = ThreadId::from_u128(52_001);
    let raw = raw();
    for (call, output) in [
        (
            function_call("marker-call"),
            function_output("marker-call", &raw),
        ),
        (
            custom_call("marker-call"),
            custom_output("marker-call", &raw),
        ),
    ] {
        let marker = marker_for(&thread, "marker-call", &output, &raw);
        let selected = vec![call.clone(), marker];
        let canonical = vec![
            RolloutItem::ResponseItem(call),
            RolloutItem::ResponseItem(output),
        ];

        let prepared =
            prepare_compaction_sources(&selected, &canonical, &thread, &HashSet::new()).unwrap();

        assert_eq!(prepared.pairs.len(), 1);
        let pair = &prepared.pairs[0];
        assert_eq!(
            pair.reference_kind,
            PreparedCompactionReferenceKind::ObservationMarker
        );
        assert_eq!(pair.reference.sha256, content_sha256(&raw));
        assert_eq!(pair.canonical_call_input, Some(CALL_INPUT));
        assert_eq!(pair.canonical_output_text, Some(raw.as_str()));
        assert_eq!(pair.output_total_bytes, raw.len());
        assert!(prepared.protected_indices.is_empty());
    }
}

#[test]
fn marker_mismatches_are_integrity_errors_not_protected_raw_output() {
    let thread = ThreadId::from_u128(52_002);
    let raw = raw();
    let call = function_call("marker-call");
    let output = function_output("marker-call", &raw);
    let marker = marker_for(&thread, "marker-call", &output, &raw);
    let canonical = vec![
        RolloutItem::ResponseItem(call.clone()),
        RolloutItem::ResponseItem(output),
    ];
    let prepare = |selected: Vec<ResponseItemEnvelope>, active: &[&str]| {
        let active = active
            .iter()
            .map(|id| (*id).to_owned())
            .collect::<HashSet<_>>();
        prepare_compaction_sources(&selected, &canonical, &thread, &active)
    };

    let mut edited_body = marker.clone();
    if let ResponseItem::FunctionCallOutput { output, .. } = &mut edited_body.item {
        let text = output
            .text_content()
            .unwrap()
            .replace("Archived", "Current");
        *output = FunctionCallOutputPayload::from_text(text);
    }
    assert!(prepare(vec![call.clone(), edited_body], &[]).is_err());

    let mut edited_metadata = marker.clone();
    edited_metadata
        .metadata
        .as_mut()
        .unwrap()
        .history_truncation_token_limit = Some(12_000);
    assert!(prepare(vec![call.clone(), edited_metadata], &[]).is_err());

    // A marker whose call differs from the canonical call does not verify.
    let mut other_call = call.clone();
    if let ResponseItem::FunctionCall { arguments, .. } = &mut other_call.item {
        *arguments = "{\"path\":\"other.log\"}".into();
    }
    assert!(prepare(vec![other_call, marker.clone()], &[]).is_err());

    // A marker for an active call cannot be paired, and never falls back to protected output.
    assert!(prepare(vec![call, marker], &["marker-call"]).is_err());

    // A marker without a canonical source does not verify.
    let orphan = marker_for(
        &thread,
        "orphan-call",
        &function_output("orphan-call", &raw),
        &raw,
    );
    assert!(prepare(vec![function_call("orphan-call"), orphan], &[]).is_err());
}

#[test]
fn replay_normalized_absent_metadata_still_verifies_markers_and_fresh_pairs() {
    // A restart reads replacement-history metadata back as the default value.
    let thread = ThreadId::from_u128(52_003);
    let raw = raw();
    let normalized = |mut item: ResponseItemEnvelope| {
        item.metadata.get_or_insert_default();
        item
    };
    let call = function_call("marker-call");
    let output = custom_output("marker-call", &raw);
    let custom_call = custom_call("marker-call");
    for (canonical_call, selected_call, canonical_output) in [
        (
            call.clone(),
            normalized(call),
            function_output("marker-call", &raw),
        ),
        (custom_call.clone(), normalized(custom_call), output),
    ] {
        let marker = marker_for(&thread, "marker-call", &canonical_output, &raw);
        let canonical = vec![
            RolloutItem::ResponseItem(canonical_call),
            RolloutItem::ResponseItem(canonical_output.clone()),
        ];
        let prepared = prepare_compaction_sources(
            &[selected_call.clone(), marker],
            &canonical,
            &thread,
            &HashSet::new(),
        )
        .unwrap();
        assert_eq!(
            prepared.pairs[0].reference_kind,
            PreparedCompactionReferenceKind::ObservationMarker
        );

        // The same normalization on a fresh pair keeps it verified instead of protected.
        let prepared = prepare_compaction_sources(
            &[selected_call, normalized(canonical_output)],
            &canonical,
            &thread,
            &HashSet::new(),
        )
        .unwrap();
        assert_eq!(prepared.pairs.len(), 1);
        assert!(prepared.protected_indices.is_empty());
    }
}
