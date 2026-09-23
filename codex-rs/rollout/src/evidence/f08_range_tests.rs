//! Unwired F08 range-retrieval tests. Wire this module after the F07d Cargo owner releases.

use super::*;
use codex_history::ObservationReference;
use codex_history::RolloutItem;
use codex_history::archive_reference::content_sha256;
use codex_protocol::ThreadId;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use std::collections::HashSet;

fn function_pair(call_id: &str, input: &str, output: &str) -> Vec<RolloutItem> {
    vec![
        RolloutItem::ResponseItem(ResponseItemEnvelope::new(ResponseItem::FunctionCall {
            id: None,
            name: "read_document".into(),
            namespace: None,
            arguments: input.into(),
            encrypted_function_args: None,
            call_id: call_id.into(),
            internal_chat_message_metadata_passthrough: None,
        })),
        RolloutItem::ResponseItem(ResponseItemEnvelope::new(
            ResponseItem::FunctionCallOutput {
                id: None,
                call_id: Some(call_id.into()),
                name: Some("read_document".into()),
                namespace: None,
                output: FunctionCallOutputPayload::from_text(output.into()),
                internal_chat_message_metadata_passthrough: None,
            },
        )),
    ]
}

fn custom_pair(call_id: &str, input: &str, output: &str) -> Vec<RolloutItem> {
    vec![
        RolloutItem::ResponseItem(ResponseItemEnvelope::new(ResponseItem::CustomToolCall {
            id: None,
            status: Some("completed".into()),
            call_id: call_id.into(),
            name: "read_document".into(),
            namespace: None,
            input: input.into(),
            internal_chat_message_metadata_passthrough: None,
        })),
        RolloutItem::ResponseItem(ResponseItemEnvelope::new(
            ResponseItem::CustomToolCallOutput {
                id: None,
                call_id: call_id.into(),
                name: Some("read_document".into()),
                output: FunctionCallOutputPayload::from_text(output.into()),
                internal_chat_message_metadata_passthrough: None,
            },
        )),
    ]
}

fn make_reference(items: &[RolloutItem], thread: &ThreadId, call_id: &str) -> ObservationReference {
    ObservationIndex::new(items, thread)
        .reference(call_id, thread, &HashSet::new())
        .expect("fixture pair should resolve")
        .0
}

#[test]
fn range_reads_native_and_custom_parts_by_independent_whole_part_digest() {
    let thread = ThreadId::from_u128(80_001);
    for items in [
        function_pair("function-1", "{\"path\":\"a\"}", "north-middle-south"),
        custom_pair("custom-1", "{\"path\":\"b\"}", "custom-north-middle-south"),
    ] {
        let call_id = match &items[0] {
            RolloutItem::ResponseItem(envelope) => match &envelope.item {
                ResponseItem::FunctionCall { call_id, .. }
                | ResponseItem::CustomToolCall { call_id, .. } => call_id.as_str(),
                _ => unreachable!(),
            },
            _ => unreachable!(),
        };
        let input = match &items[0] {
            RolloutItem::ResponseItem(envelope) => match &envelope.item {
                ResponseItem::FunctionCall { arguments, .. } => arguments.as_str(),
                ResponseItem::CustomToolCall { input, .. } => input.as_str(),
                _ => unreachable!(),
            },
            _ => unreachable!(),
        };
        let output = match &items[1] {
            RolloutItem::ResponseItem(envelope) => match &envelope.item {
                ResponseItem::FunctionCallOutput { output, .. }
                | ResponseItem::CustomToolCallOutput { output, .. } => {
                    output.text_content().unwrap()
                }
                _ => unreachable!(),
            },
            _ => unreachable!(),
        };
        let reference = make_reference(&items, &thread, call_id);
        let index = ObservationIndex::new(&items, &thread);
        let call_result = index
            .resolve_range(
                &reference,
                &HashSet::new(),
                ObservationPart::Call,
                0,
                input.len(),
                &content_sha256(input),
            )
            .unwrap();
        std::assert_eq!(call_result.reference, reference);
        std::assert_eq!(call_result.part, ObservationPart::Call);
        std::assert_eq!(call_result.total_bytes, input.len());
        std::assert_eq!(call_result.text, input);
        std::assert_eq!(call_result.part_sha256, content_sha256(input));

        let output_result = index
            .resolve_range(
                &reference,
                &HashSet::new(),
                ObservationPart::Output,
                0,
                output.len(),
                &content_sha256(output),
            )
            .unwrap();
        std::assert_eq!(output_result.reference.sha256, content_sha256(output));
        std::assert_eq!(output_result.total_bytes, output.len());
        std::assert_eq!(output_result.text, output);
    }
}

#[test]
fn range_is_exact_utf8_half_open_slice_and_keeps_full_part_length() {
    let thread = ThreadId::from_u128(80_002);
    let text = "a界🙂z";
    let items = function_pair("unicode-1", "read unicode", text);
    let reference = make_reference(&items, &thread, "unicode-1");
    let index = ObservationIndex::new(&items, &thread);

    let result = index
        .resolve_range(
            &reference,
            &HashSet::new(),
            ObservationPart::Output,
            1,
            8,
            &content_sha256(text),
        )
        .unwrap();
    std::assert_eq!(result.text, "界🙂");
    std::assert_eq!(result.start, 1);
    std::assert_eq!(result.end, 8);
    std::assert_eq!(result.total_bytes, text.len());

    for (start, end) in [(2, 5), (1, 2), (4, 5)] {
        std::assert!(
            index
                .resolve_range(
                    &reference,
                    &HashSet::new(),
                    ObservationPart::Output,
                    start,
                    end,
                    &content_sha256(text),
                )
                .is_err()
        );
    }
}

#[test]
fn range_accepts_large_canonical_parts_while_returning_at_most_2048_bytes() {
    let thread = ThreadId::from_u128(80_003);
    let input = format!(
        "{}middle-input{}",
        "x".repeat(1_100_000),
        "y".repeat(1_100_000)
    );
    let output = format!(
        "{}middle-output{}",
        "a".repeat(1_100_000),
        "z".repeat(1_100_000)
    );
    let items = function_pair("large-1", &input, &output);
    let reference = make_reference(&items, &thread, "large-1");
    let index = ObservationIndex::new(&items, &thread);
    for (part, text, start) in [
        (ObservationPart::Call, input.as_str(), 1_099_990),
        (ObservationPart::Output, output.as_str(), 1_099_990),
    ] {
        let end = start + 32;
        let result = index
            .resolve_range(
                &reference,
                &HashSet::new(),
                part,
                start,
                end,
                &content_sha256(text),
            )
            .unwrap();
        std::assert_eq!(result.total_bytes, text.len());
        std::assert_eq!(result.text, &text[start..end]);
        std::assert!(result.text.len() <= 2_048);
    }
}

#[test]
fn range_rejects_bad_digest_shape_size_or_ambiguous_identity_without_fallback() {
    let thread = ThreadId::from_u128(80_004);
    let text = "a well-sized text observation";
    let items = function_pair("bad-1", "input", text);
    let reference = make_reference(&items, &thread, "bad-1");
    let index = ObservationIndex::new(&items, &thread);

    for (start, end, digest) in [
        (0, 0, content_sha256(text)),
        (4, 2, content_sha256(text)),
        (0, text.len() + 1, content_sha256(text)),
        (0, text.len(), content_sha256("different")),
    ] {
        std::assert!(
            index
                .resolve_range(
                    &reference,
                    &HashSet::new(),
                    ObservationPart::Output,
                    start,
                    end,
                    &digest,
                )
                .is_err()
        );
    }
    std::assert!(
        index
            .resolve_range(
                &reference,
                &HashSet::new(),
                ObservationPart::Output,
                0,
                2_049,
                &content_sha256(text),
            )
            .is_err()
    );

    let mut wrong_reference = reference.clone();
    wrong_reference.sha256 = content_sha256("not this output");
    std::assert!(
        index
            .resolve_range(
                &wrong_reference,
                &HashSet::new(),
                ObservationPart::Call,
                0,
                5,
                &content_sha256("input"),
            )
            .is_err()
    );
    std::assert!(
        index
            .resolve_range(
                &reference,
                &HashSet::from(["bad-1".to_owned()]),
                ObservationPart::Output,
                0,
                4,
                &content_sha256(text),
            )
            .is_err()
    );
    let other_thread = ThreadId::from_u128(80_099);
    let other_reference = make_reference(&items, &other_thread, "bad-1");
    std::assert!(
        index
            .resolve_range(
                &other_reference,
                &HashSet::new(),
                ObservationPart::Output,
                0,
                4,
                &content_sha256(text),
            )
            .is_err()
    );

    let duplicate = [items[0].clone(), items[1].clone(), items[1].clone()];
    std::assert!(
        ObservationIndex::new(&duplicate, &thread)
            .resolve_range(
                &reference,
                &HashSet::new(),
                ObservationPart::Output,
                0,
                4,
                &content_sha256(text),
            )
            .is_err()
    );

    let duplicate_call = [items[0].clone(), items[0].clone(), items[1].clone()];
    std::assert!(
        ObservationIndex::new(&duplicate_call, &thread)
            .resolve_range(
                &reference,
                &HashSet::new(),
                ObservationPart::Output,
                0,
                4,
                &content_sha256(text),
            )
            .is_err()
    );
}

#[test]
fn range_part_parser_accepts_only_call_and_output() {
    std::assert_eq!(
        "call".parse::<ObservationPart>().unwrap(),
        ObservationPart::Call
    );
    std::assert_eq!(
        "output".parse::<ObservationPart>().unwrap(),
        ObservationPart::Output
    );
    std::assert!("unknown".parse::<ObservationPart>().is_err());
}

#[test]
fn range_does_not_expand_encrypted_function_arguments() {
    let thread = ThreadId::from_u128(80_005);
    let items = vec![
        RolloutItem::ResponseItem(ResponseItemEnvelope::new(ResponseItem::FunctionCall {
            id: None,
            name: "read_document".into(),
            namespace: None,
            arguments: "display-only input".into(),
            encrypted_function_args: Some(Vec::new()),
            call_id: "encrypted-1".into(),
            internal_chat_message_metadata_passthrough: None,
        })),
        RolloutItem::ResponseItem(ResponseItemEnvelope::new(
            ResponseItem::FunctionCallOutput {
                id: None,
                call_id: Some("encrypted-1".into()),
                name: Some("read_document".into()),
                namespace: None,
                output: FunctionCallOutputPayload::from_text("returned text".into()),
                internal_chat_message_metadata_passthrough: None,
            },
        )),
    ];
    let reference = make_reference(&items, &thread, "encrypted-1");
    let index = ObservationIndex::new(&items, &thread);
    std::assert!(
        index
            .resolve_range(
                &reference,
                &HashSet::new(),
                ObservationPart::Call,
                0,
                "display-only input".len(),
                &content_sha256("display-only input"),
            )
            .is_err()
    );
}
