// Modified by RenCrow Switch Core, 2026-09-22.
use super::*;
use codex_history::CodexHarnessMetadata;
use codex_history::SenderUserMessages;
use codex_history::compaction_pipeline::InstructionSelection;
use codex_history::compaction_pipeline::ProposedOperation;
use codex_history::compaction_pipeline::ProposedPlan;
use codex_history::compaction_preprocess::collect_instruction_candidates;
use codex_history::compaction_preprocess::prune_known_obsolete;
use codex_history::compaction_selection::validate_and_apply_selection;
use codex_protocol::ResponseItemId;
use codex_protocol::mcp::McpAttribution;
use codex_protocol::mcp::McpAttributionSource;
use codex_protocol::mcp::McpAttributionStatus;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::InternalChatMessageMetadataPassthrough;
use codex_protocol::models::ReasoningItemContent;
use codex_protocol::models::ReasoningItemReasoningSummary;

fn message(role: &str, text: &str) -> ResponseItemEnvelope {
    ResponseItemEnvelope::new(
        serde_json::from_value(
            json!({"type":"message","role":role,"content":[{"type":"input_text","text":text}]}),
        )
        .unwrap(),
    )
}
fn command_call(call_id: &str, arguments: &str) -> ResponseItemEnvelope {
    ResponseItemEnvelope::new(
        serde_json::from_value(json!({
            "type":"function_call",
            "call_id":call_id,
            "name":"exec_command",
            "arguments":arguments
        }))
        .unwrap(),
    )
}
fn command_output(call_id: &str, text: &str) -> ResponseItemEnvelope {
    ResponseItemEnvelope::new(ResponseItem::FunctionCallOutput {
        id: None,
        call_id: Some(call_id.into()),
        name: Some("exec_command".into()),
        namespace: None,
        output: FunctionCallOutputPayload::from_text(text.into()),
        internal_chat_message_metadata_passthrough: None,
    })
}
fn human(text: &str) -> ResponseItemEnvelope {
    let mut item = message("user", text);
    item.metadata.get_or_insert_default().rencrow_input = Some(
        json!({"version":1,"author":"human","thread_id":"test-thread","selected_text":text,"receipt_hash":"accepted-receipt"}),
    );
    item
}
fn reasoning_item(
    summary: Vec<ReasoningItemReasoningSummary>,
    content: Option<Vec<ReasoningItemContent>>,
    encrypted_content: Option<String>,
    internal_chat_message_metadata_passthrough: Option<InternalChatMessageMetadataPassthrough>,
) -> ResponseItemEnvelope {
    ResponseItemEnvelope::new(ResponseItem::Reasoning {
        id: None,
        summary,
        content,
        encrypted_content,
        internal_chat_message_metadata_passthrough,
    })
}
fn with_id(mut envelope: ResponseItemEnvelope, native_id: &str) -> ResponseItemEnvelope {
    let ResponseItem::Message { id, .. } = &mut envelope.item else {
        panic!("only messages get fixture IDs");
    };
    *id = Some(ResponseItemId::from_server(native_id.into()));
    envelope
}
/// Project retained native items for an accepted selection without operations.
fn native_without_selection(
    input: &CandidateInput,
    items: &[ResponseItemEnvelope],
) -> Result<super::super::native::NativeProjection, String> {
    let application =
        validate_and_apply_selection(input, &prune_known_obsolete(input, &[])?, &[], None)?;
    super::super::native::filter_retained_instructions(items, input, application)
}
fn retained(
    input: &CandidateInput,
    items: &[ResponseItemEnvelope],
) -> Result<Vec<ResponseItemEnvelope>, String> {
    Ok(native_without_selection(input, items)?
        .retained_native_items()
        .cloned()
        .collect())
}

#[test]
fn plaintext_reasoning_is_summary_visible_and_not_retained() {
    let mut item = reasoning_item(
        vec![ReasoningItemReasoningSummary::SummaryText {
            text: "summary: ".into(),
        }],
        Some(vec![
            ReasoningItemContent::ReasoningText {
                text: "reasoning: ".into(),
            },
            ReasoningItemContent::Text {
                text: "continued".into(),
            },
        ]),
        /*encrypted_content*/ None,
        Some(InternalChatMessageMetadataPassthrough {
            turn_id: Some("turn-ordinary".into()),
            create_time: Some(serde_json::Number::from(123)),
            ..Default::default()
        }),
    );
    // Qwen emits client_authored:false, which is equivalent to Default.
    item.metadata = Some(CodexHarnessMetadata::default());
    let items = vec![item];
    let original = items.clone();
    let input = capture(
        &items,
        "binding".into(),
        "test-thread",
        vec![json!({"current":true})],
        &[],
    )
    .unwrap();
    assert_eq!(input.records[0].origin, Origin::Work);
    assert_eq!(input.records[0].role, "assistant");
    assert_eq!(input.records[0].text, "summary: reasoning: continued");
    assert_eq!(input.records[0].opaque, None);

    let native = native_without_selection(&input, &items).unwrap();
    let summary_items =
        super::super::summary::build_summary_history(&items, &input, &native, &[], None).unwrap();
    assert_eq!(summary_items, items);
    assert!(retained(&input, &items).unwrap().is_empty());
    assert_eq!(items, original);
}

#[test]
fn protected_reasoning_variants_remain_opaque_and_are_retained() {
    let encrypted = reasoning_item(
        vec![ReasoningItemReasoningSummary::SummaryText {
            text: "encrypted summary".into(),
        }],
        Some(vec![ReasoningItemContent::ReasoningText {
            text: "encrypted body".into(),
        }]),
        Some(String::new()),
        Some(InternalChatMessageMetadataPassthrough {
            turn_id: Some("turn-encrypted".into()),
            ..Default::default()
        }),
    );
    let empty = reasoning_item(
        vec![ReasoningItemReasoningSummary::SummaryText {
            text: String::new(),
        }],
        Some(vec![ReasoningItemContent::Text {
            text: String::new(),
        }]),
        /*encrypted_content*/ None,
        /*internal_chat_message_metadata_passthrough*/ None,
    );
    let protected_passthrough = reasoning_item(
        vec![ReasoningItemReasoningSummary::SummaryText {
            text: "protected passthrough".into(),
        }],
        /*content*/ None,
        /*encrypted_content*/ None,
        Some(InternalChatMessageMetadataPassthrough {
            content_item_kinds: Some(vec![codex_protocol::models::ContentItemKind(
                "protected".into(),
            )]),
            ..Default::default()
        }),
    );
    let mut protected_harness = reasoning_item(
        vec![ReasoningItemReasoningSummary::SummaryText {
            text: "protected harness metadata".into(),
        }],
        /*content*/ None,
        /*encrypted_content*/ None,
        Some(InternalChatMessageMetadataPassthrough {
            turn_id: Some("turn-harness".into()),
            ..Default::default()
        }),
    );
    protected_harness.metadata = Some(CodexHarnessMetadata {
        rencrow_input: Some(json!({"receipt":"input"})),
        rencrow_compaction: Some(json!({"checkpoint":"compaction"})),
        client_authored: true,
        inherited_user_message: true,
        mcp_attribution: Some(McpAttribution {
            status: McpAttributionStatus::Complete,
            sources: vec![McpAttributionSource {
                connector_id: Some("connector".into()),
                plugin_id: Some("plugin".into()),
                server_name: "server".into(),
                tool_name: "tool".into(),
                first_turn_id: "turn-harness".into(),
            }],
        }),
        sender_user_messages: Some(Box::new(SenderUserMessages {
            receiver_turn_id: "turn-harness".into(),
            receiver_message_id: "message".into(),
            text: "sender evidence".into(),
        })),
        ..Default::default()
    });
    let items = vec![encrypted, empty, protected_passthrough, protected_harness];
    let original = items.clone();
    let input = capture(
        &items,
        "binding".into(),
        "test-thread",
        vec![json!({"current":true})],
        &[],
    )
    .unwrap();
    assert!(
        input
            .records
            .iter()
            .all(|record| record.origin == Origin::Unknown && record.opaque.is_some())
    );
    assert_eq!(retained(&input, &items).unwrap(), items);
    assert_eq!(items, original);
}

#[test]
fn partial_selection_preserves_identity_and_survives_second_capture() {
    let items = vec![
        with_id(human("Keep Japanese. Use old-label."), "human-a"),
        with_id(human("Use new-label instead of old-label."), "human-b"),
    ];
    let input = capture(
        &items,
        "binding".into(),
        "test-thread",
        vec![json!({"current":true})],
        &[],
    )
    .unwrap();
    let pruning = prune_known_obsolete(&input, &[]).unwrap();
    let payload = collect_instruction_candidates(&input, &pruning, &[])
        .unwrap()
        .unwrap();
    let selection = InstructionSelection::from_host(
        payload["snapshot_hash"].as_str().unwrap().into(),
        payload["presentation_hash"].as_str().unwrap().into(),
        ProposedPlan {
            operations: vec![ProposedOperation::DropSuperseded {
                source: "human-a".into(),
                source_text: Some("Use old-label.".into()),
                correction: "human-b".into(),
                correction_text: Some("Use new-label instead of old-label.".into()),
            }],
        },
    );
    let application = validate_and_apply_selection(&input, &pruning, &[], Some(selection)).unwrap();
    let selected = super::super::native::filter_retained_instructions(&items, &input, application)
        .unwrap()
        .retained_native_items()
        .cloned()
        .collect::<Vec<_>>();
    let second = capture(
        &selected,
        "second".into(),
        "test-thread",
        vec![json!({"current":true})],
        &[],
    )
    .unwrap();
    assert_eq!(second.records[0].text, "Keep Japanese. ");
    assert_eq!(second.records[0].origin, Origin::Human);
    assert_eq!(second.records[1].text, input.records[1].text);
    let persisted = serde_json::to_vec(
        &selected
            .iter()
            .cloned()
            .map(codex_history::RolloutItem::ResponseItem)
            .collect::<Vec<_>>(),
    )
    .unwrap();
    let resumed: Vec<codex_history::RolloutItem> = serde_json::from_slice(&persisted).unwrap();
    let resumed: Vec<_> = resumed
        .into_iter()
        .map(|item| match item {
            codex_history::RolloutItem::ResponseItem(item) => item,
            _ => panic!("unexpected rollout item"),
        })
        .collect();
    assert_eq!(resumed, selected);
}

#[test]
fn unknown_automation_and_attachments_are_not_prunable_human_text() {
    let mut automation = human("automated input");
    automation
        .metadata
        .as_mut()
        .unwrap()
        .rencrow_input
        .as_mut()
        .unwrap()["author"] = json!("automation");
    let mut attached = human("Keep this image");
    if let ResponseItem::Message { content, .. } = &mut attached.item {
        content.push(
            serde_json::from_value(
                json!({"type":"input_image","image_url":"data:image/png;base64,AA=="}),
            )
            .unwrap(),
        );
    }
    let items = vec![message("user", "unknown"), automation, attached];
    let input = capture(
        &items,
        "binding".into(),
        "test-thread",
        vec![json!({"current":true})],
        &[],
    )
    .unwrap();
    assert_eq!(
        input.records.iter().map(|r| &r.origin).collect::<Vec<_>>(),
        vec![&Origin::Unknown, &Origin::Unknown, &Origin::Human]
    );
    assert_eq!(retained(&input, &items).unwrap(), items);
    // Neither unknown or automated input nor attachment-bearing Human text is offered for removal.
    let pruning = prune_known_obsolete(&input, &[]).unwrap();
    assert_eq!(
        collect_instruction_candidates(&input, &pruning, &[]),
        Ok(None)
    );
}

#[test]
fn inherited_receipt_and_mixed_host_text_remain_unknown() {
    let mut inherited = human("old instruction");
    inherited.metadata.as_mut().unwrap().inherited_user_message = true;
    let mut mixed = human("original");
    if let ResponseItem::Message { content, .. } = &mut mixed.item {
        content.push(ContentItem::InputText {
            text: "host suffix".into(),
        });
    }
    let input = capture(
        &[inherited, mixed],
        "binding".into(),
        "test-thread",
        vec![json!({"current":true})],
        &[],
    )
    .unwrap();
    assert!(input.records.iter().all(|r| r.origin == Origin::Unknown));
}

#[test]
fn unannotated_developer_is_not_guessed_to_be_current_host_context() {
    let items = vec![message("developer", "legacy client instruction")];
    let input = capture(
        &items,
        "binding".into(),
        "test-thread",
        vec![json!({"current":true})],
        &[],
    )
    .unwrap();
    assert_eq!(input.records[0].origin, Origin::Unknown);
    assert_eq!(retained(&input, &items).unwrap(), items);
}

#[test]
fn owner_annotated_legacy_summary_is_work_without_human_promotion() {
    let item = crate::context::ContextualUserFragment::into(
        crate::context::CompactionSummary::new("Old work summary"),
    );
    let input = capture(
        &[ResponseItemEnvelope::new(item)],
        "binding".into(),
        "test-thread",
        vec![json!({"current":true})],
        &[],
    )
    .unwrap();
    assert_eq!(input.records[0].origin, Origin::Work);
    assert_eq!(input.records[0].intake_ref, None);
}

#[test]
fn completed_nonadjacent_pair_is_work_and_removed_from_retained_history() {
    let items = vec![
        command_call("call-1", "{\"cmd\":\"printf done\"}"),
        message("developer", "intervening unknown context"),
        command_output("call-1", "done\n"),
        human("Keep Japanese and preserve the attachment."),
    ];
    let original = items.clone();
    let projection = CompletedWorkProjection {
        call_index: 0,
        output_index: 2,
        call_text: "{\"cmd\":\"printf done\"}".into(),
        output_text: "done\n".into(),
    };
    let input = capture(
        &items,
        "binding".into(),
        "test-thread",
        vec![json!({"current":true})],
        &[projection],
    )
    .unwrap();
    assert_eq!(input.records[0].origin, Origin::Work);
    assert_eq!(input.records[0].opaque, None);
    assert!(!input.records[0].execution_evidence);
    assert_eq!(input.records[2].origin, Origin::Work);
    assert_eq!(input.records[2].opaque, None);
    assert!(input.records[2].execution_evidence);
    assert_eq!(input.records[3].origin, Origin::Human);

    assert_eq!(input.records[0].text, "{\"cmd\":\"printf done\"}");
    assert_eq!(input.records[2].text, "done\n");

    let replacement = retained(&input, &items).unwrap();
    assert_eq!(replacement, vec![items[1].clone(), items[3].clone()]);
    assert_eq!(items, original);

    let resumed = capture(
        &replacement,
        "resumed".into(),
        "test-thread",
        vec![json!({"current":true})],
        &[],
    )
    .unwrap();
    assert_eq!(resumed.records[0].origin, Origin::Unknown);
    assert_eq!(resumed.records[1].origin, Origin::Human);
}

#[test]
fn existing_v1_pair_is_removed_as_work_without_expanding_archived_output() {
    let huge_original = "archived command output ".repeat(20_000);
    let reference = codex_history::ArchiveReference::new(
        "test-thread",
        "call-archive",
        codex_history::archive_reference::content_sha256(&huge_original),
        codex_history::ArchiveTerminalStatus::Failed,
        7,
        Some("process-1".into()),
    );
    let marker = reference.marker().unwrap();
    let items = vec![
        command_call("call-archive", "{\"cmd\":\"long-running-task\"}"),
        command_output("call-archive", &marker),
    ];
    let input = capture(
        &items,
        "binding".into(),
        "test-thread",
        vec![json!({"current":true})],
        // Existing references keep empty CandidateInput text; the archived body stays in rollout.
        &[CompletedWorkProjection {
            call_index: 0,
            output_index: 1,
            call_text: String::new(),
            output_text: String::new(),
        }],
    )
    .unwrap();
    assert!(
        input
            .records
            .iter()
            .all(|record| record.origin == Origin::Work && record.opaque.is_none())
    );
    assert!(
        !serde_json::to_string(&input)
            .unwrap()
            .contains(&huge_original)
    );
    assert!(retained(&input, &items).unwrap().is_empty());
}

#[test]
fn unverified_pending_media_and_provenance_variants_stay_opaque() {
    let pending = command_call("pending", "{\"cmd\":\"sleep 5\"}");
    let mut media_call = command_call("media", "{\"cmd\":\"cat image\"}");
    if let ResponseItem::FunctionCall {
        internal_chat_message_metadata_passthrough,
        ..
    } = &mut media_call.item
    {
        *internal_chat_message_metadata_passthrough =
            Some(InternalChatMessageMetadataPassthrough {
                cell_id: Some("special-provenance".into()),
                ..Default::default()
            });
    }
    let mut media_output = command_output("media", "placeholder");
    if let ResponseItem::FunctionCallOutput { output, .. } = &mut media_output.item {
        *output = FunctionCallOutputPayload::from_content_items(vec![
            serde_json::from_value(
                json!({"type":"input_image","image_url":"data:image/png;base64,AA=="}),
            )
            .unwrap(),
        ]);
    }
    let unknown_call = command_call("unknown", "{\"cmd\":\"ls\"}");
    let unknown_output = command_output("unknown", "unproved");
    let items = vec![
        pending,
        media_call,
        media_output,
        unknown_call,
        unknown_output,
    ];
    let input = capture(&items, "binding".into(), "test-thread", vec![], &[]).unwrap();
    assert!(input.records.iter().all(|record| record.opaque.is_some()));
    assert!(
        input
            .records
            .iter()
            .any(|record| record.origin == Origin::Unknown)
    );
    assert_eq!(retained(&input, &items).unwrap(), items);
}
