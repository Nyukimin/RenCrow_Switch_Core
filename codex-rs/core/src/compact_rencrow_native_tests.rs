use super::*;
use codex_history::ResponseItemEnvelope;
use codex_history::compaction_candidate::CandidateInput;
use codex_history::compaction_candidate::Origin;
use codex_history::compaction_plan::ByteRange;
use codex_history::compaction_preprocess::prune_known_obsolete;
use codex_history::compaction_selection::InstructionSelectionApplication;
use codex_protocol::ResponseItemId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ImageReference;
use codex_protocol::models::InternalChatMessageMetadataPassthrough;
use codex_protocol::models::MessagePhase;
use codex_protocol::models::ResponseItem;
use serde_json::json;

fn message(role: &str, text: &str, id: Option<&str>) -> ResponseItemEnvelope {
    ResponseItemEnvelope::new(ResponseItem::Message {
        id: id.map(|id| ResponseItemId::from_server(id.into())),
        role: role.into(),
        content: vec![ContentItem::InputText { text: text.into() }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    })
}

fn human(text: &str, id: &str) -> ResponseItemEnvelope {
    let mut item = message("user", text, Some(id));
    item.metadata.get_or_insert_default().rencrow_input = Some(json!({
        "version":1,
        "author":"human",
        "thread_id":"thread-native",
        "selected_text":text,
        "receipt_hash":"receipt-original"
    }));
    item
}

fn input(items: &[ResponseItemEnvelope]) -> CandidateInput {
    // Production always passes the current owner context, which Host records require.
    super::super::history::capture(
        items,
        "binding".into(),
        "thread-native",
        vec![json!({"owner":"test"})],
        &[],
    )
    .unwrap()
}

fn application(
    input: &CandidateInput,
    refs: &[codex_history::compaction_plan::SourceRef],
) -> InstructionSelectionApplication {
    InstructionSelectionApplication {
        pruning: prune_known_obsolete(input, refs).unwrap(),
        plan_hash: (!refs.is_empty()).then(|| "plan-hash".into()),
        results: vec![],
    }
}

/// Retained envelopes whose captured record is a verified Human.
fn human_envelopes<'a>(
    projection: &'a NativeProjection,
    captured: &'a CandidateInput,
) -> impl Iterator<Item = &'a ResponseItemEnvelope> {
    projection
        .retained_by_original_index()
        .filter(|(index, _)| captured.records[*index].origin == Origin::Human)
        .map(|(_, envelope)| envelope)
}

fn text(envelope: &ResponseItemEnvelope) -> Option<String> {
    let ResponseItem::Message { content, .. } = &envelope.item else {
        return None;
    };
    Some(
        content
            .iter()
            .filter_map(|part| match part {
                ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                    Some(text.as_str())
                }
                ContentItem::InputImage { .. } | ContentItem::InputAudio { .. } => None,
            })
            .collect(),
    )
}

#[test]
fn v2_native_projection_unicode_removal_spans_parts_and_borrows_the_same_human_projection() {
    let mut item = human("前の指示／keep🙂", "human-stable");
    let ResponseItem::Message {
        content,
        phase,
        internal_chat_message_metadata_passthrough,
        ..
    } = &mut item.item
    else {
        unreachable!();
    };
    *content = vec![
        ContentItem::InputText {
            text: "前の指".into(),
        },
        ContentItem::InputText {
            text: "示／keep🙂".into(),
        },
    ];
    *phase = Some(MessagePhase::Commentary);
    *internal_chat_message_metadata_passthrough = Some(InternalChatMessageMetadataPassthrough {
        turn_id: Some("turn-7".into()),
        create_time: Some(serde_json::Number::from(7)),
        content_item_kinds: Some(vec![
            codex_protocol::models::ContentItemKind("human.first".into()),
            codex_protocol::models::ContentItemKind("human.second".into()),
        ]),
        ..Default::default()
    });
    item.metadata
        .as_mut()
        .unwrap()
        .rencrow_input
        .as_mut()
        .unwrap()["selected_text"] = json!("前の指示／keep🙂");
    let items = vec![item];
    let captured = input(&items);
    let source = captured
        .snapshot()
        .unwrap()
        .reference(
            "human-stable",
            ByteRange {
                start: 0,
                end: "前の指示／".len(),
            },
        )
        .unwrap();
    let projection =
        filter_retained_instructions(&items, &captured, application(&captured, &[source])).unwrap();

    assert_eq!(human_envelopes(&projection, &captured).count(), 1);
    assert_eq!(
        text(human_envelopes(&projection, &captured).next().unwrap()).as_deref(),
        Some("keep🙂")
    );
    let rebuilt = build_native_replacement(&projection, "summary", vec![]).unwrap();
    let ResponseItem::Message {
        id,
        content,
        phase,
        internal_chat_message_metadata_passthrough,
        ..
    } = &rebuilt[0].item
    else {
        panic!("expected the native Human envelope");
    };
    assert_eq!(id.as_ref().unwrap().as_str(), "human-stable");
    assert_eq!(
        content,
        &vec![
            ContentItem::InputText { text: "".into() },
            ContentItem::InputText {
                text: "keep🙂".into()
            },
        ]
    );
    assert_eq!(phase, &Some(MessagePhase::Commentary));
    assert_eq!(
        internal_chat_message_metadata_passthrough
            .as_ref()
            .unwrap()
            .turn_id
            .as_deref(),
        Some("turn-7")
    );
    let intake = rebuilt[0]
        .metadata
        .as_ref()
        .unwrap()
        .rencrow_input
        .as_ref()
        .unwrap();
    assert_eq!(intake["receipt_hash"], "receipt-original");
    assert_eq!(intake["selected_text"], "keep🙂");
    assert_eq!(intake["selection_hash"], "plan-hash");
    assert_eq!(text(&items[0]).as_deref(), Some("前の指示／keep🙂"));
    assert_eq!(
        items[0]
            .metadata
            .as_ref()
            .unwrap()
            .rencrow_input
            .as_ref()
            .unwrap()["selected_text"],
        "前の指示／keep🙂"
    );
    let resumed = input(&rebuilt);
    assert_eq!(resumed.records[0].origin, Origin::Human);
    assert_eq!(resumed.records[0].id, "human-stable");
    assert_eq!(resumed.records[0].text, "keep🙂");
}

#[test]
fn v2_native_projection_oversized_user_text_is_not_cut_by_the_original_fixed_budget() {
    let large = "x".repeat(100_000);
    let items = vec![human(&large, "large-human")];
    let captured = input(&items);
    let projection =
        filter_retained_instructions(&items, &captured, application(&captured, &[])).unwrap();
    assert_eq!(
        human_envelopes(&projection, &captured)
            .next()
            .unwrap()
            .item
            .id()
            .unwrap()
            .as_str(),
        "large-human"
    );
    let rebuilt = build_native_replacement(&projection, "summary", vec![]).unwrap();
    assert_eq!(text(&rebuilt[0]).as_deref(), Some(large.as_str()));

    let empty = vec![human("", "empty-human")];
    let captured = input(&empty);
    let projection =
        filter_retained_instructions(&empty, &captured, application(&captured, &[])).unwrap();
    let rebuilt = build_native_replacement(&projection, "summary", vec![]).unwrap();
    assert_eq!(rebuilt[0].item.id().unwrap().as_str(), "empty-human");
    assert_eq!(text(&rebuilt[0]).as_deref(), Some(""));

    let mixed = vec![human("", "empty-first"), human("kept", "nonempty-second")];
    let captured = input(&mixed);
    let projection =
        filter_retained_instructions(&mixed, &captured, application(&captured, &[])).unwrap();
    let rebuilt = build_native_replacement(&projection, "summary", vec![]).unwrap();
    assert_eq!(rebuilt[0].item.id().unwrap().as_str(), "empty-first");
    assert_eq!(text(&rebuilt[0]).as_deref(), Some(""));
    assert_eq!(rebuilt[1].item.id().unwrap().as_str(), "nonempty-second");
    assert_eq!(text(&rebuilt[1]).as_deref(), Some("kept"));
}

#[test]
fn v2_native_projection_summary_prefix_is_not_provenance_for_dropping_or_promoting_user_items() {
    let human_prefix = format!(
        "{}\nthis is accepted Human text",
        super::super::super::SUMMARY_PREFIX
    );
    let human = human(&human_prefix, "prefix-human");
    let unknown = message("user", &human_prefix, Some("prefix-unknown"));
    let mut actual_summary = message("user", "old summary", Some("old-summary"));
    let metadata = actual_summary.metadata.get_or_insert_default();
    metadata.rencrow_compaction = Some(json!({"version":2}));
    let items = vec![human, unknown, actual_summary];
    let captured = input(&items);
    assert_eq!(captured.records[0].origin, Origin::Human);
    assert_eq!(captured.records[1].origin, Origin::Unknown);
    let projection =
        filter_retained_instructions(&items, &captured, application(&captured, &[])).unwrap();
    let rebuilt = build_native_replacement(&projection, "summary", vec![]).unwrap();
    assert_eq!(rebuilt.len(), 3);
    assert_eq!(text(&rebuilt[0]).as_deref(), Some(human_prefix.as_str()));
    assert_eq!(text(&rebuilt[1]).as_deref(), Some(human_prefix.as_str()));
    assert_eq!(rebuilt[0].item.id().unwrap().as_str(), "prefix-human");
    assert_eq!(rebuilt[1].item.id().unwrap().as_str(), "prefix-unknown");
    assert_eq!(human_envelopes(&projection, &captured).count(), 1);
}

#[test]
fn v2_native_projection_ordinary_work_is_not_readded_but_unknown_and_protected_native_items_survive()
 {
    let pending = serde_json::from_value(json!({
        "type":"function_call","id":"pending-item","call_id":"pending-call","name":"exec_command","arguments":"{}"
    })).unwrap();
    let active = ResponseItemEnvelope::new(pending);
    let protected_work = message(
        "assistant",
        "work that remains protected",
        Some("protected-work"),
    );
    let mut captured = input(&[active.clone(), protected_work.clone()]);
    captured.records[1]
        .protected
        .push(ByteRange { start: 0, end: 4 });
    let projection = filter_retained_instructions(
        &[active.clone(), protected_work.clone()],
        &captured,
        application(&captured, &[]),
    )
    .unwrap();
    let rebuilt = build_native_replacement(&projection, "summary", vec![]).unwrap();
    assert_eq!(rebuilt[0], active);
    assert_eq!(rebuilt[1], protected_work);

    let mut attached_human = human("keep the attached image and audio", "attached-human");
    if let ResponseItem::Message { content, .. } = &mut attached_human.item {
        content.push(ContentItem::InputImage {
            image: ImageReference::Inline {
                image_url: "data:image/png;base64,AA==".into(),
            },
            detail: None,
        });
        content.push(ContentItem::InputAudio {
            audio_url: "data:audio/wav;base64,AA==".into(),
        });
    }
    let media_output = ResponseItemEnvelope::new(ResponseItem::CustomToolCallOutput {
        id: Some(ResponseItemId::from_server("media-output".into())),
        call_id: "media-call".into(),
        name: Some("image_tool".into()),
        output: FunctionCallOutputPayload::from_content_items(vec![
            FunctionCallOutputContentItem::InputText {
                text: "image result".into(),
            },
            FunctionCallOutputContentItem::InputImage {
                image: ImageReference::Inline {
                    image_url: "data:image/png;base64,AQ==".into(),
                },
                detail: None,
            },
            FunctionCallOutputContentItem::InputAudio {
                audio_url: "data:audio/wav;base64,AQ==".into(),
            },
        ]),
        internal_chat_message_metadata_passthrough: None,
    });
    let opaque_items = vec![attached_human.clone(), media_output.clone()];
    let captured = input(&opaque_items);
    let projection =
        filter_retained_instructions(&opaque_items, &captured, application(&captured, &[]))
            .unwrap();
    let rebuilt = build_native_replacement(&projection, "summary", vec![]).unwrap();
    assert_eq!(rebuilt[0], attached_human);
    assert_eq!(rebuilt[1], media_output);

    let work = message(
        "assistant",
        "summarized ordinary work",
        Some("ordinary-work"),
    );
    let kept_human = human("keep me", "kept-human");
    let items = vec![work, kept_human.clone()];
    let captured = input(&items);
    let projection =
        filter_retained_instructions(&items, &captured, application(&captured, &[])).unwrap();
    let rebuilt = build_native_replacement(&projection, "summary", vec![]).unwrap();
    assert_eq!(rebuilt.len(), 3);
    assert_eq!(rebuilt[0], kept_human);
    assert_eq!(text(&rebuilt[0]).as_deref(), Some("keep me"));
    assert!(
        text(&rebuilt[1])
            .unwrap()
            .starts_with("<codex_internal_context source=\"compaction\">")
    );

    let call = ResponseItemEnvelope::new(ResponseItem::FunctionCall {
        id: Some(ResponseItemId::from_server("tool-call-item".into())),
        name: "custom_tool".into(),
        namespace: None,
        arguments: "{}".into(),
        encrypted_function_args: None,
        call_id: "tool-call".into(),
        internal_chat_message_metadata_passthrough: None,
    });
    let output = ResponseItemEnvelope::new(ResponseItem::FunctionCallOutput {
        id: Some(ResponseItemId::from_server("tool-output-item".into())),
        call_id: Some("tool-call".into()),
        name: Some("custom_tool".into()),
        namespace: None,
        output: FunctionCallOutputPayload::from_text("large summarized result".into()),
        internal_chat_message_metadata_passthrough: None,
    });
    let pair = vec![call, human("current instruction", "pair-human"), output];
    let captured = super::super::history::capture(
        &pair,
        "binding".into(),
        "thread-native",
        vec![],
        &[super::super::history::CompletedWorkProjection {
            call_index: 0,
            output_index: 2,
            call_text: "verified call evidence".into(),
            output_text: "verified output evidence".into(),
        }],
    )
    .unwrap();
    let projection =
        filter_retained_instructions(&pair, &captured, application(&captured, &[])).unwrap();
    let rebuilt = build_native_replacement(&projection, "summary", vec![]).unwrap();
    // The retained Human, the timeline note that dates it, and the summary.
    assert_eq!(rebuilt.len(), 3);
    assert_eq!(rebuilt[0], pair[1]);
    assert!(!rebuilt.iter().any(|item| {
        item.item
            .id()
            .is_some_and(|id| id.as_str() == "tool-output-item")
    }));
}

#[test]
fn v2_native_projection_native_and_snapshot_mismatch_or_unstable_removal_ids_fail_closed() {
    let item = human("old passage and current", "native-id");
    let mut captured = input(std::slice::from_ref(&item));
    captured.records[0].id = "item-0".into();
    let err = filter_retained_instructions(&[item], &captured, application(&captured, &[]))
        .err()
        .expect("native/candidate identity mismatch must fail closed");
    assert!(err.contains("identity"));

    let item = human("old passage and current", "native-id");
    let mut captured = input(std::slice::from_ref(&item));
    captured.records[0].id = "item-0".into();
    let source = captured
        .snapshot()
        .unwrap()
        .reference("item-0", ByteRange { start: 0, end: 13 })
        .unwrap();
    let err = filter_retained_instructions(&[item], &captured, application(&captured, &[source]))
        .err()
        .expect("unstable native identity must not authorize a removal");
    assert!(err.contains("identity"));

    let mut item = human("old passage and current", "native-id");
    item.item.set_id(None);
    let captured = input(std::slice::from_ref(&item));
    assert_eq!(captured.records[0].id, "item-0");
    let source = captured
        .snapshot()
        .unwrap()
        .reference("item-0", ByteRange { start: 0, end: 13 })
        .unwrap();
    let err = filter_retained_instructions(&[item], &captured, application(&captured, &[source]))
        .err()
        .expect("unstable native identity must not authorize a removal");
    assert!(err.contains("stable native identity"));
}

#[test]
fn v2_native_projection_initial_context_stays_before_the_last_real_user_even_for_summary_prefix_text()
 {
    let prefixed_human = format!("{}\ncurrent request", super::super::super::SUMMARY_PREFIX);
    let items = vec![human(&prefixed_human, "prefix-human")];
    let captured = input(&items);
    let projection =
        filter_retained_instructions(&items, &captured, application(&captured, &[])).unwrap();
    let context = message("user", "initial context", Some("initial-context"));
    let summary = format!("{}\nsummary", super::super::super::SUMMARY_PREFIX);
    let rebuilt = build_native_replacement(&projection, &summary, vec![context.clone()]).unwrap();
    assert_eq!(rebuilt[0], context);
    assert_eq!(rebuilt[1].item.id().unwrap().as_str(), "prefix-human");
    assert!(
        text(&rebuilt[2])
            .as_deref()
            .unwrap()
            .starts_with(super::super::super::SUMMARY_PREFIX)
    );

    let agent = ResponseItemEnvelope::new(ResponseItem::AgentMessage {
        id: Some(ResponseItemId::from_server("agent-progress".into())),
        author: "assistant".into(),
        recipient: "assistant".into(),
        content: vec![
            codex_protocol::models::AgentMessageInputContent::InputText {
                text: "ongoing progress".into(),
            },
        ],
        internal_chat_message_metadata_passthrough: None,
    });
    let with_agent = vec![items[0].clone(), agent.clone()];
    let captured = input(&with_agent);
    let projection =
        filter_retained_instructions(&with_agent, &captured, application(&captured, &[])).unwrap();
    let rebuilt = build_native_replacement(&projection, &summary, vec![context.clone()]).unwrap();
    assert_eq!(rebuilt[0], with_agent[0]);
    assert_eq!(rebuilt[1], context);
    assert_eq!(rebuilt[2], agent);
}

#[test]
fn v2_native_projection_capture_prefers_existing_item_ids_and_fallback_ids_do_not_define_stability()
{
    let first = message("user", "first", Some("persisted-a"));
    let second = message("user", "second", Some("persisted-b"));
    let captured = input(&[first.clone(), second.clone()]);
    assert_eq!(
        captured
            .records
            .iter()
            .map(|record| record.id.as_str())
            .collect::<Vec<_>>(),
        vec!["persisted-a", "persisted-b"]
    );
    let moved = input(&[second, first]);
    assert_eq!(
        moved
            .records
            .iter()
            .map(|record| record.id.as_str())
            .collect::<Vec<_>>(),
        vec!["persisted-b", "persisted-a"]
    );
    let fallback = message("user", "no durable id", None);
    let captured = input(&[fallback]);
    assert_eq!(captured.records[0].id, "item-0");
}

fn user_at(text: &str, id: &str, create_time: f64) -> ResponseItemEnvelope {
    let mut item = message("user", text, Some(id));
    if let ResponseItem::Message {
        internal_chat_message_metadata_passthrough,
        ..
    } = &mut item.item
    {
        *internal_chat_message_metadata_passthrough =
            Some(InternalChatMessageMetadataPassthrough {
                create_time: serde_json::Number::from_f64(create_time),
                content_item_kinds: Some(vec![ContentItemKind("user.text".into())]),
                ..Default::default()
            });
    }
    item
}

fn internal_context(source: &'static str, body: &str, id: &str) -> ResponseItemEnvelope {
    let mut item = crate::context::ContextualUserFragment::into(
        crate::context::InternalModelContextFragment::new(
            crate::context::InternalContextSource::from_static(source),
            body,
        ),
    );
    if let ResponseItem::Message { id: item_id, .. } = &mut item {
        *item_id = Some(ResponseItemId::from_server(id.into()));
    }
    ResponseItemEnvelope::new(item)
}

fn retained_ids(projection: &NativeProjection) -> Vec<String> {
    projection
        .retained_native_items()
        .filter_map(|envelope| envelope.item.id().map(|id| id.as_str().to_owned()))
        .collect()
}

#[test]
fn v2_native_projection_keeps_only_the_latest_internal_context_per_source() {
    let items = vec![
        user_at("start the task", "user-1", 1_790_306_659.4),
        internal_context("goal", "first continuation", "goal-1"),
        message("assistant", "work", Some("work-1")),
        internal_context("goal", "second continuation", "goal-2"),
        internal_context("compaction", "old timeline", "timeline-1"),
        internal_context("goal", "third continuation", "goal-3"),
    ];
    let captured = input(&items);
    for index in [1, 3, 4, 5] {
        assert_eq!(captured.records[index].origin, Origin::Host);
        assert_eq!(captured.records[index].text, "");
    }

    let projection =
        filter_retained_instructions(&items, &captured, application(&captured, &[])).unwrap();

    assert_eq!(retained_ids(&projection), vec!["user-1", "goal-3"]);
}

#[test]
fn v2_native_replacement_lists_retained_user_messages_before_the_summary() {
    let items = vec![
        user_at("start the task\nwith  details", "user-1", 1_790_306_659.4),
        user_at(
            "observer note: the scheduled backup stopped CORE on purpose, do not restart it",
            "user-2",
            1_790_320_426.6,
        ),
        message("user", "no recorded time", Some("user-3")),
        internal_context("goal", "continue", "goal-1"),
    ];
    let captured = input(&items);
    let projection =
        filter_retained_instructions(&items, &captured, application(&captured, &[])).unwrap();
    let summary = format!("{}\nsummary", super::super::super::SUMMARY_PREFIX);

    let rebuilt = build_native_replacement(&projection, &summary, vec![]).unwrap();

    let note = ResponseItemEnvelope::new(crate::context::ContextualUserFragment::into(
        crate::context::InternalModelContextFragment::new(
            crate::context::InternalContextSource::from_static("compaction"),
            "Earlier user messages kept verbatim above, oldest first:\n\
             1. 2026-09-25 03:24 UTC: \"start the task with details\"\n\
             2. 2026-09-25 07:13 UTC: \"observer note: the scheduled backup stopped CORE on purpose,…\"\n\
             3. time unknown: \"no recorded time\"\n\
             All of them were received before the work summary that follows, which records the \
             latest known state. A situation an older message describes may already be resolved \
             or superseded; check the current state before acting on it.",
        ),
    ));
    assert_eq!(rebuilt.len(), 6);
    assert_eq!(&rebuilt[..4], &items[..]);
    assert_eq!(rebuilt[4], note);
    assert_eq!(text(&rebuilt[5]).as_deref(), Some(summary.as_str()));
    assert_eq!(
        projection
            .retained_native_items_with_initial_context(&[], &rebuilt[5])
            .cloned()
            .collect::<Vec<_>>(),
        rebuilt
    );
}

#[test]
fn v2_native_replacement_has_no_timeline_without_retained_user_messages() {
    let items = vec![
        internal_context("goal", "continue", "goal-1"),
        message("assistant", "work", Some("work-1")),
    ];
    let captured = input(&items);
    let projection =
        filter_retained_instructions(&items, &captured, application(&captured, &[])).unwrap();

    let rebuilt = build_native_replacement(&projection, "summary", vec![]).unwrap();

    assert_eq!(rebuilt.len(), 2);
    assert_eq!(rebuilt[0], items[0]);
    assert_eq!(text(&rebuilt[1]).as_deref(), Some("summary"));
}

#[test]
fn v2_native_replacement_timeline_does_not_move_the_initial_context() {
    let items = vec![user_at("current request", "user-1", 1_790_306_659.4)];
    let captured = input(&items);
    let projection =
        filter_retained_instructions(&items, &captured, application(&captured, &[])).unwrap();
    let context = message("user", "initial context", Some("initial-context"));
    let summary = format!("{}\nsummary", super::super::super::SUMMARY_PREFIX);

    let rebuilt = build_native_replacement(&projection, &summary, vec![context.clone()]).unwrap();

    assert_eq!(rebuilt.len(), 4);
    assert_eq!(rebuilt[0], context);
    assert_eq!(rebuilt[1], items[0]);
    assert!(
        text(&rebuilt[2])
            .unwrap()
            .starts_with("<codex_internal_context source=\"compaction\">")
    );
    assert_eq!(text(&rebuilt[3]).as_deref(), Some(summary.as_str()));
    assert_eq!(
        projection
            .retained_native_items_with_initial_context(std::slice::from_ref(&context), &rebuilt[3])
            .cloned()
            .collect::<Vec<_>>(),
        rebuilt
    );
}
