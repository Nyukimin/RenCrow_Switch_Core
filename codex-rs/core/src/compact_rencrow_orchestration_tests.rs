//! Integration regressions for the V2 original-loop connection.
//!
//! These cases target the pure `compact_rencrow_summary` boundary, not the model/session
//! transport. `ServerReasoningIncluded` is websocket metadata, so its drain dispatch boundary needs
//! a transport-level regression in addition to the pure policy cases below.

use super::*;
use crate::compact::SUMMARY_PREFIX;
use codex_history::ResponseItemEnvelope;
use codex_history::archive_reference::ObservationReference;
use codex_history::archive_reference::content_sha256;
use codex_history::compaction_candidate::CandidateInput;
use codex_history::compaction_candidate::Origin;
use codex_history::compaction_checkpoint_metadata::RenCrowCompactionMetadataV2;
use codex_history::compaction_plan::ByteRange;
use codex_history::compaction_preprocess::prune_known_obsolete;
use codex_history::compaction_selection::InstructionSelectionApplication;
use codex_protocol::ResponseItemId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::InternalChatMessageMetadataPassthrough;
use codex_protocol::models::ResponseItem;
use codex_rollout::PreparedCompactionReferenceKind;
use codex_rollout::PreparedCompactionSourcePair;
use codex_rollout::PreparedCompactionSources;
use serde_json::json;

const THREAD_ID: &str = "thread-v2-orchestration";

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

fn checkpoint_summary(text: &str) -> ResponseItemEnvelope {
    let summary_body = format!("{SUMMARY_PREFIX}\n{text}");
    let mut summary = message("user", &summary_body, "old-summary");
    let ResponseItem::Message {
        internal_chat_message_metadata_passthrough,
        ..
    } = &mut summary.item
    else {
        unreachable!();
    };
    *internal_chat_message_metadata_passthrough = Some(InternalChatMessageMetadataPassthrough {
        content_item_kinds: Some(vec![ContentItemKind("compaction.summary".into())]),
        ..Default::default()
    });
    summary.metadata.get_or_insert_default().rencrow_compaction = Some(json!({
        "version": 2,
        "snapshot_hash": "a".repeat(64),
        "summary_hash": content_sha256(&summary_body),
        "selection_mode": "no_candidates",
        "applied_refs": [],
        "results": [],
        "observations": [],
        "important_refs": [],
        "model": "orchestration-test-model",
        "responses": [{
            "stage": "summary",
            "response_id": "adopted-summary-response",
            "seconds": 0.1,
            "usage": null,
        }],
        "committed_transaction_hash": "b".repeat(64),
    }));
    summary
}

fn candidate_input(
    originals: &[ResponseItemEnvelope],
    completed_work: &[super::history::CompletedWorkProjection],
) -> CandidateInput {
    super::history::capture(
        originals,
        "snapshot-binding".into(),
        THREAD_ID,
        vec![],
        completed_work,
    )
    .unwrap()
}

fn application(input: &CandidateInput) -> InstructionSelectionApplication {
    InstructionSelectionApplication {
        pruning: prune_known_obsolete(input, &[]).unwrap(),
        plan_hash: None,
        results: vec![],
    }
}

fn item_text(envelope: &ResponseItemEnvelope) -> String {
    match &envelope.item {
        ResponseItem::Message { content, .. } => content
            .iter()
            .filter_map(|part| match part {
                ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                    Some(text.as_str())
                }
                ContentItem::InputImage { .. } | ContentItem::InputAudio { .. } => None,
            })
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

fn model_items(envelopes: &[ResponseItemEnvelope]) -> Vec<&ResponseItem> {
    envelopes.iter().map(|envelope| &envelope.item).collect()
}

fn item_id(envelope: &ResponseItemEnvelope) -> Option<&str> {
    envelope.item.id().map(|id| id.as_str())
}

#[test]
fn v2_summary_projection_uses_one_adopted_summary_and_only_later_ordinary_work() {
    let old_work = message("assistant", "covered before checkpoint", "old-work");
    let adopted = checkpoint_summary("prior verified state");
    let request = human("keep this request", "human-request");
    let later_work = message("assistant", "new work after checkpoint", "new-work");
    let originals = vec![old_work, adopted, request.clone(), later_work.clone()];
    let input = candidate_input(&originals, &[]);
    let native =
        super::native::filter_retained_instructions(&originals, &input, application(&input))
            .unwrap();

    let projected =
        super::summary::build_summary_history(&originals, &input, &native, &[], Some(1)).unwrap();

    let ids = projected.iter().filter_map(item_id).collect::<Vec<_>>();
    assert_eq!(ids.iter().filter(|id| **id == "old-summary").count(), 1);
    assert!(ids.contains(&"new-work"));
    assert!(ids.contains(&"human-request"));
    assert!(!ids.contains(&"old-work"));
    assert!(projected.iter().any(|item| item == &request));
    assert!(projected.iter().any(|item| item == &later_work));
}

#[test]
fn v2_summary_projection_replaces_both_verified_pair_slots_with_one_bounded_observation() {
    let old_work = message("assistant", "covered earlier", "old-work");
    let adopted = checkpoint_summary("prior facts");
    let request = human("check the file", "human-request");
    let call_text = r#"{"path":"large.txt"}"#;
    let output_text = format!("HEAD:{}:TAIL", "x".repeat(5_000));
    let call_item: ResponseItem = serde_json::from_value(json!({
        "type": "function_call",
        "id": "verified-call-item",
        "call_id": "verified-call",
        "name": "read_file",
        "arguments": call_text,
    }))
    .unwrap();
    let call = ResponseItemEnvelope::new(call_item);
    let output = ResponseItemEnvelope::new(ResponseItem::FunctionCallOutput {
        id: Some(ResponseItemId::from_server("verified-output-item".into())),
        call_id: Some("verified-call".into()),
        name: Some("read_file".into()),
        namespace: None,
        output: FunctionCallOutputPayload::from_text(output_text.clone()),
        internal_chat_message_metadata_passthrough: None,
    });
    let active_call = ResponseItemEnvelope::new(
        serde_json::from_value(json!({
            "type": "function_call",
            "id": "active-call-item",
            "call_id": "active-call",
            "name": "exec_command",
            "arguments": "{\"cmd\":\"still-running\"}",
        }))
        .unwrap(),
    );
    let opaque_media = ResponseItemEnvelope::new(ResponseItem::CustomToolCallOutput {
        id: Some(ResponseItemId::from_server("opaque-media-item".into())),
        call_id: "media-call".into(),
        name: Some("image_tool".into()),
        output: FunctionCallOutputPayload::from_content_items(vec![
            codex_protocol::models::FunctionCallOutputContentItem::InputText {
                text: "media metadata".into(),
            },
            codex_protocol::models::FunctionCallOutputContentItem::InputImage {
                image: codex_protocol::models::ImageReference::Inline {
                    image_url: "data:image/png;base64,AQ==".into(),
                },
                detail: None,
            },
        ]),
        internal_chat_message_metadata_passthrough: None,
    });
    let later_work = message("assistant", "checked the file header", "new-work");
    let originals = vec![
        old_work,
        adopted,
        request.clone(),
        call,
        output,
        later_work.clone(),
        active_call.clone(),
        opaque_media.clone(),
    ];
    let completed = [super::history::CompletedWorkProjection {
        call_index: 3,
        output_index: 4,
        call_text: call_text.into(),
        output_text: output_text.clone(),
    }];
    let input = candidate_input(&originals, &completed);
    let native =
        super::native::filter_retained_instructions(&originals, &input, application(&input))
            .unwrap();
    let reference =
        ObservationReference::new(THREAD_ID, "verified-call", content_sha256(&output_text));
    let observation =
        codex_history::project_observation(&reference, "read_file", call_text, &output_text)
            .unwrap()
            .summary;

    let projected = super::summary::build_summary_history(
        &originals,
        &input,
        &native,
        &[(3, 4, observation)],
        Some(1),
    )
    .unwrap();

    assert!(projected.iter().any(|item| item == &request));
    assert!(projected.iter().any(|item| item == &later_work));
    assert!(projected.iter().any(|item| item == &active_call));
    assert!(projected.iter().any(|item| item == &opaque_media));
    assert!(projected.iter().any(|item| {
        match &item.item {
            ResponseItem::Message {
                internal_chat_message_metadata_passthrough: Some(metadata),
                ..
            } => metadata
                .content_item_kinds
                .as_ref()
                .is_some_and(|kinds| kinds.iter().any(|kind| kind.0 == "compaction.observation")),
            _ => false,
        }
    }));
    let serialized = serde_json::to_string(&model_items(&projected)).unwrap();
    assert!(!serialized.contains("verified-call-item"));
    assert!(!serialized.contains("verified-output-item"));
    assert!(!serialized.contains(&"x".repeat(5_000)));
    assert!(serialized.contains("verified-call"));
    assert!(serialized.contains("HEAD:"));
    assert!(serialized.contains(":TAIL"));
}

#[test]
fn v2_generic_capture_projects_large_custom_tool_input_from_canonical_raw_pair() {
    let request = human("inspect the large custom result", "human-request");
    let canonical_call = format!("HEAD:{}:CANONICAL-CALL-TAIL", "c".repeat(5_000));
    let canonical_output = format!("RESULT-HEAD:{}:CANONICAL-OUTPUT-TAIL", "o".repeat(5_000));
    // The selected history item was already truncated by normal intake. F01 supplies the
    // verified canonical call/output text from the existing rollout to this projection.
    let current_custom_call = ResponseItemEnvelope::new(ResponseItem::CustomToolCall {
        id: Some(ResponseItemId::from_server("custom-call-item".into())),
        status: Some("completed".into()),
        call_id: "custom-call".into(),
        name: "custom_read".into(),
        namespace: None,
        input: "HEAD:[normal-history-truncated]".into(),
        internal_chat_message_metadata_passthrough: None,
    });
    let current_custom_output = ResponseItemEnvelope::new(ResponseItem::CustomToolCallOutput {
        id: Some(ResponseItemId::from_server("custom-output-item".into())),
        call_id: "custom-call".into(),
        name: Some("custom_read".into()),
        output: FunctionCallOutputPayload::from_text("[normal-history-truncated]".into()),
        internal_chat_message_metadata_passthrough: None,
    });
    let active_custom_call = ResponseItemEnvelope::new(ResponseItem::CustomToolCall {
        id: Some(ResponseItemId::from_server(
            "active-custom-call-item".into(),
        )),
        status: Some("in_progress".into()),
        call_id: "active-custom-call".into(),
        name: "custom_write".into(),
        namespace: None,
        input: "must remain opaque".into(),
        internal_chat_message_metadata_passthrough: None,
    });
    let originals = vec![
        request.clone(),
        current_custom_call,
        current_custom_output,
        active_custom_call.clone(),
    ];
    let reference =
        ObservationReference::new(THREAD_ID, "custom-call", content_sha256(&canonical_output));
    let prepared = PreparedCompactionSources {
        pairs: vec![PreparedCompactionSourcePair {
            call_index: 1,
            output_index: 2,
            reference: reference.clone(),
            reference_kind: PreparedCompactionReferenceKind::Fresh,
            output_total_bytes: canonical_output.len(),
            tool_name: "custom_read",
            canonical_call_input: Some(&canonical_call),
            canonical_output_text: Some(&canonical_output),
            terminal_reference: None,
        }],
        protected_indices: vec![3],
    };

    // The host-proven custom pair becomes ordinary Work so the same native projection removes
    // it. Its large raw bodies stay in the rollout; CandidateInput does not copy them (Annex A F07).
    let input = super::history::capture(
        &originals,
        "snapshot-binding".into(),
        THREAD_ID,
        vec![],
        &super::history::verified_pair_projections(&prepared),
    )
    .unwrap();
    for index in [1, 2] {
        assert_eq!(input.records[index].origin, Origin::Work);
        assert!(input.records[index].opaque.is_none());
        assert!(input.records[index].text.is_empty());
    }

    let native =
        super::native::filter_retained_instructions(&originals, &input, application(&input))
            .unwrap();
    let observation = codex_history::project_observation(
        &reference,
        "custom_read",
        &canonical_call,
        &canonical_output,
    )
    .unwrap()
    .summary;
    let summary_items = super::summary::build_summary_history(
        &originals,
        &input,
        &native,
        &[(1, 2, observation)],
        None,
    )
    .unwrap();
    let candidate = super::native::build_native_replacement(&native, "summary", vec![]).unwrap();

    assert!(summary_items.iter().any(|item| item == &request));
    assert!(summary_items.iter().any(|item| item == &active_custom_call));
    assert!(candidate.iter().any(|item| item == &active_custom_call));
    for projected in [&summary_items, &candidate] {
        let ids = projected.iter().filter_map(item_id).collect::<Vec<_>>();
        assert!(!ids.contains(&"custom-call-item"));
        assert!(!ids.contains(&"custom-output-item"));
        let serialized = serde_json::to_string(&model_items(projected)).unwrap();
        assert!(!serialized.contains(&"c".repeat(5_000)));
        assert!(!serialized.contains(&"o".repeat(5_000)));
    }
    let summary_json = serde_json::to_string(&model_items(&summary_items)).unwrap();
    assert!(summary_json.contains("CANONICAL-CALL-TAIL"));
    assert!(summary_json.contains("CANONICAL-OUTPUT-TAIL"));
    assert!(!summary_json.contains("normal-history-truncated"));
}

#[test]
fn v2_verified_pair_capture_keeps_full_text_only_for_bounded_pairs() {
    let small_call = r#"{"cmd":"cargo test"}"#;
    let small_output = "test result: ok";
    let large_call = r#"{"path":"large.txt"}"#;
    let large_output = "L".repeat(2_049);
    let reference =
        |output: &str| ObservationReference::new(THREAD_ID, "call", content_sha256(output));
    let prepared = PreparedCompactionSources {
        pairs: vec![
            PreparedCompactionSourcePair {
                call_index: 0,
                output_index: 1,
                reference: reference(small_output),
                reference_kind: PreparedCompactionReferenceKind::Fresh,
                output_total_bytes: small_output.len(),
                tool_name: "exec_command",
                canonical_call_input: Some(small_call),
                canonical_output_text: Some(small_output),
                terminal_reference: None,
            },
            PreparedCompactionSourcePair {
                call_index: 2,
                output_index: 3,
                reference: reference(&large_output),
                reference_kind: PreparedCompactionReferenceKind::Fresh,
                output_total_bytes: large_output.len(),
                tool_name: "read_file",
                canonical_call_input: Some(large_call),
                canonical_output_text: Some(&large_output),
                terminal_reference: None,
            },
            PreparedCompactionSourcePair {
                call_index: 4,
                output_index: 5,
                reference: reference("archived output"),
                reference_kind: PreparedCompactionReferenceKind::Existing,
                output_total_bytes: "archived output".len(),
                tool_name: "exec_command",
                canonical_call_input: None,
                canonical_output_text: None,
                terminal_reference: None,
            },
        ],
        protected_indices: vec![],
    };

    assert_eq!(
        super::history::verified_pair_projections(&prepared),
        vec![
            super::history::CompletedWorkProjection {
                call_index: 0,
                output_index: 1,
                call_text: small_call.into(),
                output_text: small_output.into(),
            },
            super::history::CompletedWorkProjection {
                call_index: 2,
                output_index: 3,
                call_text: String::new(),
                output_text: String::new(),
            },
            super::history::CompletedWorkProjection {
                call_index: 4,
                output_index: 5,
                call_text: String::new(),
                output_text: String::new(),
            },
        ]
    );
}

#[test]
fn v2_summary_projection_reuses_the_same_pruned_human_envelope_as_native_replacement() {
    let mut request = human("remove this; keep🙂", "human-request");
    let ResponseItem::Message { content, .. } = &mut request.item else {
        unreachable!();
    };
    *content = vec![
        ContentItem::InputText {
            text: "remove this; ".into(),
        },
        ContentItem::InputText {
            text: "keep🙂".into(),
        },
    ];
    request
        .metadata
        .as_mut()
        .unwrap()
        .rencrow_input
        .as_mut()
        .unwrap()["selected_text"] = json!("remove this; keep🙂");
    let originals = vec![request.clone()];
    let input = candidate_input(&originals, &[]);
    let source = input
        .snapshot()
        .unwrap()
        .reference(
            "human-request",
            ByteRange {
                start: 0,
                end: "remove this; ".len(),
            },
        )
        .unwrap();
    let application = InstructionSelectionApplication {
        pruning: prune_known_obsolete(&input, &[source]).unwrap(),
        plan_hash: Some("host-plan".into()),
        results: vec![],
    };
    let native =
        super::native::filter_retained_instructions(&originals, &input, application).unwrap();
    let summary_items =
        super::summary::build_summary_history(&originals, &input, &native, &[], None).unwrap();
    let candidate = super::native::build_native_replacement(&native, "summary", vec![]).unwrap();
    let summary_human = summary_items
        .iter()
        .find(|item| item_id(item) == Some("human-request"))
        .unwrap();
    let candidate_human = candidate
        .iter()
        .find(|item| item_id(item) == Some("human-request"))
        .unwrap();

    assert_eq!(summary_human, candidate_human);
    assert_eq!(item_text(summary_human), "keep🙂");
    assert_eq!(
        summary_human
            .metadata
            .as_ref()
            .unwrap()
            .rencrow_input
            .as_ref()
            .unwrap()["selected_text"],
        json!("keep🙂")
    );
}

#[test]
fn v2_summary_response_validation_accepts_assistant_text_with_reasoning() {
    let valid = [
        ResponseItem::Reasoning {
            id: None,
            summary: vec![],
            content: None,
            encrypted_content: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Message {
            id: None,
            role: "assistant".into(),
            content: vec![ContentItem::OutputText {
                text: "Current work and unresolved checks.".into(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
    ];
    assert_eq!(
        super::summary::summary_suffix_from_staged_output(&valid).unwrap(),
        "Current work and unresolved checks."
    );
}

#[test]
fn v2_summary_response_validation_rejects_nonassistant_or_tool_output_even_with_text() {
    let user = [ResponseItem::Message {
        id: None,
        role: "user".into(),
        content: vec![ContentItem::OutputText {
            text: "not an assistant summary".into(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }];
    assert!(super::summary::summary_suffix_from_staged_output(&user).is_err());

    let assistant = ResponseItem::Message {
        id: None,
        role: "assistant".into(),
        content: vec![ContentItem::OutputText {
            text: "Current work and unresolved checks.".into(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    let tool = [
        assistant,
        serde_json::from_value(json!({
            "type": "function_call",
            "id": "unexpected-tool",
            "call_id": "unexpected",
            "name": "exec_command",
            "arguments": "{}",
        }))
        .unwrap(),
    ];
    assert!(super::summary::summary_suffix_from_staged_output(&tool).is_err());
    assert!(super::summary::summary_suffix_from_staged_output(&[]).is_err());
}

#[test]
fn v2_important_refs_resolve_only_explicit_json_string_markers_and_deduplicate() {
    let call_id = "call:\"quoted\"/path";
    let reference =
        ObservationReference::new(THREAD_ID, call_id, content_sha256("historical output"));
    let marker = format!(
        "Keep this citation observation:{} and repeat observation:{}.",
        serde_json::to_string(call_id).unwrap(),
        serde_json::to_string(call_id).unwrap(),
    );

    assert_eq!(
        super::summary::important_refs_from_summary(&marker, std::slice::from_ref(&reference)),
        super::summary::ImportantRefResolution {
            refs: vec![reference],
            ignored_markers: 0,
        }
    );
    assert_eq!(
        super::summary::important_refs_from_summary("No archive citation.", &[]),
        super::summary::ImportantRefResolution::default()
    );
}

#[test]
fn v2_important_refs_ignore_malformed_unknown_or_ambiguous_markers_without_failing() {
    let known = ObservationReference::new(THREAD_ID, "known-call", content_sha256("known output"));
    let ambiguous = [
        known.clone(),
        ObservationReference::new(THREAD_ID, "known-call", content_sha256("different output")),
    ];
    let ignored = |ignored_markers| super::summary::ImportantRefResolution {
        refs: vec![],
        ignored_markers,
    };

    assert_eq!(
        super::summary::important_refs_from_summary(
            "key observation: useful; observation:not-json; observation: maybe",
            std::slice::from_ref(&known),
        ),
        ignored(0)
    );
    assert_eq!(
        super::summary::important_refs_from_summary(
            "observation:\"unterminated",
            std::slice::from_ref(&known),
        ),
        ignored(1)
    );
    assert_eq!(
        super::summary::important_refs_from_summary(
            r#"observation:"bad\x" then text"#,
            std::slice::from_ref(&known),
        ),
        ignored(1)
    );
    assert_eq!(
        super::summary::important_refs_from_summary(
            "observation:\"unknown-call\"",
            std::slice::from_ref(&known),
        ),
        ignored(1)
    );
    assert_eq!(
        super::summary::important_refs_from_summary("observation:\"known-call\"", &ambiguous),
        ignored(1)
    );
    assert_eq!(
        super::summary::important_refs_from_summary(
            "Use observation:\"known-call\", skip observation:\"missing\", reuse observation:\"known-call\".",
            std::slice::from_ref(&known),
        ),
        super::summary::ImportantRefResolution {
            refs: vec![known],
            ignored_markers: 1,
        }
    );
}

#[test]
fn v2_important_refs_do_not_promote_an_inventory_without_explicit_markers() {
    let inventory = (0..256)
        .map(|index| {
            let call_id = format!("call-{index}");
            ObservationReference::new(
                THREAD_ID,
                call_id.clone(),
                content_sha256(&format!("output-{index}")),
            )
        })
        .collect::<Vec<_>>();
    let marker = format!(
        "Important source: observation:{}",
        serde_json::to_string("call-197").unwrap(),
    );

    assert_eq!(
        super::summary::important_refs_from_summary(&marker, &inventory),
        super::summary::ImportantRefResolution {
            refs: vec![inventory[197].clone()],
            ignored_markers: 0,
        }
    );
}

#[test]
fn fork_auxiliary_server_reasoning_metadata_does_not_change_active_context_policy() {
    // This pure dispatch-policy case does not claim an SSE fixture exercises the actual
    // Responses websocket ServerReasoningIncluded event. The drain integration still needs that
    // event or a direct test of the dispatch boundary after the policy is wired.
    assert!(!super::summary::should_apply_server_reasoning_included(
        /*rencrow_compaction*/ true
    ));
}

#[test]
fn disabled_compaction_keeps_server_reasoning_sideband_policy() {
    assert!(super::summary::should_apply_server_reasoning_included(
        /*rencrow_compaction*/ false
    ));
}

fn with_compaction_metadata(
    mut summary: ResponseItemEnvelope,
    update: impl FnOnce(&mut serde_json::Value),
) -> ResponseItemEnvelope {
    update(
        summary
            .metadata
            .as_mut()
            .unwrap()
            .rencrow_compaction
            .as_mut()
            .unwrap(),
    );
    summary
}

fn adopted_checkpoint(
    summary: &ResponseItemEnvelope,
    index: usize,
) -> super::summary::AdoptedV2Checkpoint {
    let summary_text = item_text(summary);
    let metadata = summary
        .metadata
        .as_ref()
        .unwrap()
        .rencrow_compaction
        .clone()
        .unwrap();
    super::summary::AdoptedV2Checkpoint {
        index,
        metadata: RenCrowCompactionMetadataV2::parse_and_validate(
            metadata,
            THREAD_ID,
            &summary_text,
        )
        .unwrap(),
        summary_text,
    }
}

#[test]
fn v2_adopted_checkpoint_accepts_live_and_replayed_transaction_lifecycles() {
    let replayed = checkpoint_summary("replayed state");
    let live = with_compaction_metadata(checkpoint_summary("live state"), |metadata| {
        metadata
            .as_object_mut()
            .unwrap()
            .remove("committed_transaction_hash");
        metadata["transaction_following_items"] = json!(1);
    });

    for summary in [replayed, live] {
        let history = vec![
            message("assistant", "earlier work", "work"),
            summary.clone(),
        ];
        assert_eq!(
            super::summary::find_adopted_v2_checkpoint(&history, THREAD_ID),
            Ok(Some(adopted_checkpoint(&summary, 1)))
        );
    }
}

#[test]
fn v2_adopted_checkpoint_rejects_invalid_typed_metadata_instead_of_falling_back() {
    let valid = checkpoint_summary("older valid state");
    let both_lifecycles = with_compaction_metadata(checkpoint_summary("ambiguous"), |metadata| {
        metadata["transaction_following_items"] = json!(1);
    });
    let tampered = with_compaction_metadata(checkpoint_summary("tampered"), |metadata| {
        metadata["summary_hash"] = json!("c".repeat(64));
    });
    let mut untyped = message("assistant", "not a summary", "assistant-with-v2");
    untyped.metadata.get_or_insert_default().rencrow_compaction = checkpoint_summary("state")
        .metadata
        .unwrap()
        .rencrow_compaction;

    for broken in [both_lifecycles, tampered, untyped] {
        let history = vec![valid.clone(), message("assistant", "work", "work"), broken];
        assert!(super::summary::find_adopted_v2_checkpoint(&history, THREAD_ID).is_err());
    }
}

#[test]
fn v2_adopted_checkpoint_selects_the_latest_v2_and_ignores_legacy_summaries() {
    let older = checkpoint_summary("older state");
    let latest = checkpoint_summary("latest state");
    let legacy = with_compaction_metadata(checkpoint_summary("legacy state"), |metadata| {
        *metadata = json!({"version": 1, "input_hash": "legacy"});
    });
    let history = vec![
        older,
        message("assistant", "first work", "work-1"),
        latest.clone(),
        message("assistant", "second work", "work-2"),
        legacy.clone(),
    ];

    assert_eq!(
        super::summary::find_adopted_v2_checkpoint(&history, THREAD_ID),
        Ok(Some(adopted_checkpoint(&latest, 2)))
    );
    assert_eq!(
        super::summary::find_adopted_v2_checkpoint(
            &[message("assistant", "work", "work"), legacy],
            THREAD_ID,
        ),
        Ok(None)
    );
}

fn function_call(call_id: &str, arguments: &str) -> ResponseItemEnvelope {
    ResponseItemEnvelope::new(
        serde_json::from_value(json!({
            "type": "function_call",
            "id": format!("{call_id}-item"),
            "call_id": call_id,
            "name": "exec_command",
            "arguments": arguments,
        }))
        .unwrap(),
    )
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

fn prepared_pair<'a>(
    call_index: usize,
    reference: ObservationReference,
    reference_kind: PreparedCompactionReferenceKind,
    texts: Option<(&'a str, &'a str)>,
    output_total_bytes: usize,
) -> PreparedCompactionSourcePair<'a> {
    PreparedCompactionSourcePair {
        call_index,
        output_index: call_index + 1,
        reference,
        reference_kind,
        output_total_bytes,
        tool_name: "exec_command",
        canonical_call_input: texts.map(|(call, _)| call),
        canonical_output_text: texts.map(|(_, output)| output),
        terminal_reference: None,
    }
}

#[test]
fn v2_observation_handling_projects_only_uncovered_identities() {
    let covered_call = r#"{"cmd":"cargo test"}"#;
    let covered_output = "test result: ok";
    let fresh_call = r#"{"cmd":"cat large.txt"}"#;
    let fresh_output = format!("HEAD:{}:TAIL", "x".repeat(5_000));
    let existing_call = r#"{"cmd":"cat archived.txt"}"#;
    let existing_output = "archived output body";
    // Position does not decide handling: the uncovered pair at index 0 was active at the previous
    // checkpoint and completed later, so it is still projected.
    let originals = vec![
        function_call("fresh-call", fresh_call),
        function_output("fresh-call", "[truncated in live history]"),
        function_call("covered-call", covered_call),
        function_output("covered-call", covered_output),
        function_call("existing-call", existing_call),
        function_output("existing-call", "[archived reference marker]"),
    ];
    let reference = |call_id: &str, output: &str| {
        ObservationReference::new(THREAD_ID, call_id, content_sha256(output))
    };
    let prepared = PreparedCompactionSources {
        pairs: vec![
            prepared_pair(
                0,
                reference("fresh-call", &fresh_output),
                PreparedCompactionReferenceKind::Fresh,
                Some((fresh_call, fresh_output.as_str())),
                fresh_output.len(),
            ),
            prepared_pair(
                2,
                reference("covered-call", covered_output),
                PreparedCompactionReferenceKind::Fresh,
                Some((covered_call, covered_output)),
                covered_output.len(),
            ),
            prepared_pair(
                4,
                reference("existing-call", existing_output),
                PreparedCompactionReferenceKind::Existing,
                None,
                existing_output.len(),
            ),
        ],
        protected_indices: vec![],
    };

    let projections = super::observation::project_unhandled_observations(
        &originals,
        &prepared,
        &[reference("covered-call", covered_output)],
    )
    .unwrap();

    assert_eq!(
        projections,
        vec![
            (
                0,
                1,
                codex_history::project_observation(
                    &reference("fresh-call", &fresh_output),
                    "exec_command",
                    fresh_call,
                    &fresh_output,
                )
                .unwrap(),
            ),
            (
                4,
                5,
                codex_history::project_existing_observation(
                    &reference("existing-call", existing_output),
                    "exec_command",
                    existing_call,
                    existing_output.len(),
                )
                .unwrap(),
            ),
        ]
    );
    assert!(projections[0].2.coverage.output.partial);
    assert!(projections[1].2.coverage.output.partial);
}

#[test]
fn v2_observation_handling_rejects_a_recorded_call_id_with_a_different_digest() {
    let call = r#"{"cmd":"cat file"}"#;
    let output = "current output";
    let originals = vec![
        function_call("same-call", call),
        function_output("same-call", output),
    ];
    let prepared = PreparedCompactionSources {
        pairs: vec![prepared_pair(
            0,
            ObservationReference::new(THREAD_ID, "same-call", content_sha256(output)),
            PreparedCompactionReferenceKind::Fresh,
            Some((call, output)),
            output.len(),
        )],
        protected_indices: vec![],
    };

    assert!(
        super::observation::project_unhandled_observations(
            &originals,
            &prepared,
            &[ObservationReference::new(
                THREAD_ID,
                "same-call",
                content_sha256("a different recorded output"),
            )],
        )
        .is_err()
    );
}

#[test]
fn v2_cumulative_observation_coverage_deduplicates_identity_and_rejects_digest_conflicts() {
    let coverage = |call_id: &str, output: &str| {
        codex_history::project_observation(
            &ObservationReference::new(THREAD_ID, call_id, content_sha256(output)),
            "exec_command",
            "{}",
            output,
        )
        .unwrap()
        .coverage
    };
    let first = coverage("first-call", "first output");
    let second = coverage("second-call", "second output");

    assert_eq!(
        super::observation::cumulative_observation_coverage(
            std::slice::from_ref(&first),
            &[first.clone(), second.clone()],
        ),
        Ok(vec![first.clone(), second])
    );
    assert!(
        super::observation::cumulative_observation_coverage(
            std::slice::from_ref(&first),
            &[coverage("first-call", "changed output")],
        )
        .is_err()
    );
}
