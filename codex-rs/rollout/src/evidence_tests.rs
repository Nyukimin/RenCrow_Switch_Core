use super::*;
use codex_history::ArchiveReference;
use codex_history::ArchiveTerminalStatus;
use codex_history::CodexHarnessMetadata;
use codex_history::ResponseItemEnvelope;
use codex_history::RolloutItem;
use codex_history::RolloutLine;
use codex_history::archive_reference::content_sha256;
use codex_protocol::ThreadId;
use codex_protocol::items::CommandExecutionItem;
use codex_protocol::items::CommandExecutionStatus;
use codex_protocol::items::TurnItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::InternalChatMessageMetadataPassthrough;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecCommandSource;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use pretty_assertions::assert_eq;
use std::collections::HashSet;
use std::time::Duration;

fn call(call_id: &str, name: &str) -> ResponseItemEnvelope {
    call_with_arguments(call_id, name, "{}")
}

fn call_with_arguments(call_id: &str, name: &str, arguments: &str) -> ResponseItemEnvelope {
    ResponseItemEnvelope::new(ResponseItem::FunctionCall {
        id: None,
        name: name.into(),
        namespace: None,
        arguments: arguments.into(),
        encrypted_function_args: None,
        call_id: call_id.into(),
        internal_chat_message_metadata_passthrough: None,
    })
}

fn output(call_id: &str, body: &str) -> ResponseItemEnvelope {
    output_named(call_id, "exec_command", body)
}

fn output_named(call_id: &str, name: &str, body: &str) -> ResponseItemEnvelope {
    ResponseItemEnvelope::new(ResponseItem::FunctionCallOutput {
        id: None,
        call_id: Some(call_id.into()),
        name: Some(name.into()),
        namespace: None,
        output: FunctionCallOutputPayload::from_text(body.into()),
        internal_chat_message_metadata_passthrough: None,
    })
}

fn custom_call(call_id: &str, name: &str) -> ResponseItemEnvelope {
    ResponseItemEnvelope::new(ResponseItem::CustomToolCall {
        id: None,
        status: Some("completed".into()),
        call_id: call_id.into(),
        name: name.into(),
        namespace: None,
        input: "{}".into(),
        internal_chat_message_metadata_passthrough: None,
    })
}

fn custom_output(call_id: &str, name: Option<&str>, body: &str) -> ResponseItemEnvelope {
    ResponseItemEnvelope::new(ResponseItem::CustomToolCallOutput {
        id: None,
        call_id: call_id.into(),
        name: name.map(str::to_owned),
        output: FunctionCallOutputPayload::from_text(body.into()),
        internal_chat_message_metadata_passthrough: None,
    })
}

fn terminal(
    thread_id: ThreadId,
    call_id: &str,
    status: CommandExecutionStatus,
    exit_code: Option<i32>,
) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
        thread_id,
        turn_id: "turn-1".into(),
        item: TurnItem::CommandExecution(CommandExecutionItem {
            model_context: None,
            id: call_id.into(),
            plugin_id: None,
            script_path: None,
            process_id: Some("process-1".into()),
            command: vec!["printf".into()],
            cwd: serde_json::from_value(serde_json::json!("file:///tmp")).unwrap(),
            parsed_cmd: vec![],
            source: ExecCommandSource::UnifiedExecStartup,
            interaction_input: None,
            status,
            stdout: Some("result".into()),
            stderr: None,
            aggregated_output: Some("result".into()),
            exit_code,
            duration: Some(Duration::from_millis(1)),
            formatted_output: Some("result".into()),
        }),
        started_at_ms: Some(1),
        completed_at_ms: 2,
    }))
}

fn fixture(
    status: CommandExecutionStatus,
    exit_code: Option<i32>,
) -> (ThreadId, Vec<RolloutItem>, ResponseItemEnvelope) {
    fixture_for("call-1", status, exit_code)
}

fn fixture_for(
    call_id: &str,
    status: CommandExecutionStatus,
    exit_code: Option<i32>,
) -> (ThreadId, Vec<RolloutItem>, ResponseItemEnvelope) {
    let thread_id = ThreadId::from_u128(1);
    let result = "a persisted command result that is longer than its archive marker ".repeat(128);
    let output = output(call_id, &result);
    let items = vec![
        RolloutItem::ResponseItem(call(call_id, "exec_command")),
        RolloutItem::ResponseItem(output.clone()),
        terminal(thread_id, call_id, status, exit_code),
    ];
    (thread_id, items, output)
}

#[test]
fn prepare_compaction_sources_projects_verified_nonadjacent_completed_pairs() {
    for (status, exit_code, expected_status) in [
        (
            CommandExecutionStatus::Completed,
            Some(0),
            ArchiveTerminalStatus::Completed,
        ),
        (
            CommandExecutionStatus::Failed,
            Some(9),
            ArchiveTerminalStatus::Failed,
        ),
    ] {
        let (thread_id, canonical, output) = fixture(status, exit_code);
        let selected = vec![
            call("call-1", "exec_command"),
            ResponseItemEnvelope::new(
                serde_json::from_value(serde_json::json!({
                    "type":"message",
                    "role":"assistant",
                    "content":[{"type":"output_text","text":"intervening"}]
                }))
                .unwrap(),
            ),
            output,
        ];
        let selected_before = selected.clone();
        let canonical_before = serde_json::to_value(&canonical).unwrap();

        let prepared =
            prepare_compaction_sources(&selected, &canonical, &thread_id, &HashSet::new()).unwrap();

        assert_eq!(
            prepared.pairs,
            vec![PreparedCompactionSourcePair {
                call_index: 0,
                output_index: 2,
                reference_kind: PreparedCompactionReferenceKind::Fresh,
                output_total_bytes: match &selected[2].item {
                    ResponseItem::FunctionCallOutput { output, .. } => {
                        output.text_content().unwrap().len()
                    }
                    _ => unreachable!(),
                },
                reference: ObservationReference {
                    thread_id: thread_id.to_string(),
                    call_id: "call-1".into(),
                    sha256: content_sha256(match &selected[2].item {
                        ResponseItem::FunctionCallOutput { output, .. } => {
                            output.text_content().unwrap()
                        }
                        _ => unreachable!(),
                    }),
                },
                terminal_reference: Some(ArchiveReference::new(
                    thread_id.to_string(),
                    "call-1",
                    content_sha256(match &selected[2].item {
                        ResponseItem::FunctionCallOutput { output, .. } => {
                            output.text_content().unwrap()
                        }
                        _ => unreachable!(),
                    }),
                    expected_status,
                    exit_code.unwrap(),
                    Some("process-1".into()),
                )),
                tool_name: "exec_command",
                canonical_call_input: Some("{}"),
                canonical_output_text: Some(match &selected[2].item {
                    ResponseItem::FunctionCallOutput { output, .. } => {
                        output.text_content().unwrap()
                    }
                    _ => unreachable!(),
                }),
            }]
        );
        assert_eq!(prepared.protected_indices, vec![1]);
        assert_eq!(selected, selected_before);
        assert_eq!(serde_json::to_value(&canonical).unwrap(), canonical_before);
    }
}

#[test]
fn prepare_compaction_sources_accepts_large_terminal_exec_but_legacy_materialization_stays_bounded()
{
    let thread_id = ThreadId::from_u128(47);
    let body = format!(
        "large terminal output:{}",
        "x".repeat(MAX_ARCHIVE_EVIDENCE_BYTES)
    );
    assert!(body.len() > MAX_ARCHIVE_EVIDENCE_BYTES);
    let output = output("large-exec", &body);
    let selected = vec![call("large-exec", "exec_command"), output.clone()];
    let canonical = vec![
        RolloutItem::ResponseItem(selected[0].clone()),
        RolloutItem::ResponseItem(output.clone()),
        terminal(
            thread_id,
            "large-exec",
            CommandExecutionStatus::Completed,
            Some(0),
        ),
    ];

    let prepared =
        prepare_compaction_sources(&selected, &canonical, &thread_id, &HashSet::new()).unwrap();

    assert_eq!(prepared.pairs.len(), 1);
    assert_eq!(prepared.pairs[0].reference.call_id, "large-exec");
    assert_eq!(prepared.pairs[0].reference.sha256, content_sha256(&body));
    assert_eq!(prepared.pairs[0].output_total_bytes, body.len());
    assert_eq!(
        prepared.pairs[0]
            .terminal_reference
            .as_ref()
            .map(|reference| reference.status),
        Some(ArchiveTerminalStatus::Completed)
    );
    assert_eq!(
        prepared.pairs[0]
            .terminal_reference
            .as_ref()
            .map(|reference| reference.original_content_sha256.as_str()),
        Some(prepared.pairs[0].reference.sha256.as_str())
    );
    assert_eq!(
        prepared.pairs[0]
            .terminal_reference
            .as_ref()
            .map(|reference| reference.exit_code),
        Some(0)
    );
    let index = ObservationIndex::new(&canonical, &thread_id);
    let borrowed = index
        .resolve_reference(&prepared.pairs[0].reference, &HashSet::new())
        .unwrap();
    assert_eq!(borrowed.body.len(), body.len());
    assert!(matches!(
        resolve_archive_evidence_from_items(
            &canonical,
            &thread_id,
            "large-exec",
            Some(&output),
            None,
            &HashSet::new(),
        ),
        Err(ArchiveEvidenceError::Ineligible(
            ArchiveEvidenceIneligibility::OversizedOutput
        ))
    ));
}

#[test]
fn prepare_compaction_sources_projects_valid_existing_marker_without_expanding_it() {
    let (thread_id, canonical, output) = fixture(CommandExecutionStatus::Failed, Some(9));
    let evidence = resolve_archive_evidence_from_items(
        &canonical,
        &thread_id,
        "call-1",
        Some(&output),
        None,
        &HashSet::new(),
    )
    .unwrap();
    let mut marker = output;
    assert!(
        codex_history::archive_reference::apply_reference(&mut marker, evidence.reference.clone())
            .unwrap()
    );
    let selected = vec![call("call-1", "exec_command"), marker];
    let selected_before = selected.clone();
    let canonical_before = serde_json::to_value(&canonical).unwrap();

    let prepared =
        prepare_compaction_sources(&selected, &canonical, &thread_id, &HashSet::new()).unwrap();

    let raw_observation = ObservationIndex::new(&canonical, &thread_id)
        .resolve_reference(&prepared.pairs[0].reference, &HashSet::new())
        .unwrap();

    assert_eq!(prepared.pairs.len(), 1);
    assert_eq!(prepared.pairs[0].call_index, 0);
    assert_eq!(prepared.pairs[0].output_index, 1);
    assert_eq!(
        prepared.pairs[0].reference,
        ObservationReference::new(
            evidence.reference.thread_id.clone(),
            evidence.reference.call_id.clone(),
            evidence.reference.original_content_sha256.clone(),
        )
    );
    assert_eq!(
        prepared.pairs[0].reference_kind,
        PreparedCompactionReferenceKind::Existing
    );
    assert_eq!(
        prepared.pairs[0].terminal_reference,
        Some(evidence.reference.clone())
    );
    assert_eq!(
        prepared.pairs[0].output_total_bytes,
        raw_observation.body.len()
    );
    assert!(prepared.pairs[0].canonical_call_input.is_none());
    assert!(prepared.pairs[0].canonical_output_text.is_none());
    let selected_marker_text = match &selected[1].item {
        ResponseItem::FunctionCallOutput { output, .. } => output.text_content().unwrap(),
        _ => unreachable!(),
    };
    assert_ne!(selected_marker_text, raw_observation.body);
    assert!(prepared.protected_indices.is_empty());
    assert_eq!(selected, selected_before);
    assert_eq!(serde_json::to_value(&canonical).unwrap(), canonical_before);
}

#[test]
fn prepare_compaction_sources_protects_existing_reference_with_nonordinary_selected_call() {
    let (thread_id, canonical, output) = fixture(CommandExecutionStatus::Completed, Some(0));
    let evidence = resolve_archive_evidence_from_items(
        &canonical,
        &thread_id,
        "call-1",
        Some(&output),
        None,
        &HashSet::new(),
    )
    .unwrap();
    let mut marker = output;
    assert!(
        codex_history::archive_reference::apply_reference(&mut marker, evidence.reference).unwrap()
    );
    let mut selected_call = call("call-1", "exec_command");
    selected_call.metadata = Some(CodexHarnessMetadata {
        rencrow_input: Some(serde_json::json!({"untrusted":"selected-only"})),
        ..Default::default()
    });

    let prepared = prepare_compaction_sources(
        &[selected_call, marker],
        &canonical,
        &thread_id,
        &HashSet::new(),
    )
    .unwrap();

    assert!(prepared.pairs.is_empty());
    assert_eq!(prepared.protected_indices, vec![0, 1]);
}

#[test]
fn prepare_compaction_sources_indexes_generic_custom_tool_returned_text() {
    let thread_id = ThreadId::from_u128(41);
    let body = "generic returned observation ".repeat(96);
    let call = custom_call("custom-1", "read_document");
    let output = custom_output("custom-1", Some("read_document"), &body);
    let selected = vec![call.clone(), output.clone()];
    let canonical = vec![
        RolloutItem::ResponseItem(call),
        RolloutItem::ResponseItem(output),
    ];

    let prepared =
        prepare_compaction_sources(&selected, &canonical, &thread_id, &HashSet::new()).unwrap();

    assert_eq!(prepared.pairs.len(), 1);
    assert_eq!(prepared.pairs[0].call_index, 0);
    assert_eq!(prepared.pairs[0].output_index, 1);
    assert_eq!(prepared.protected_indices, Vec::<usize>::new());
    assert!(prepared.pairs[0].terminal_reference.is_none());
    let serialized_reference = serde_json::to_value(&prepared.pairs[0].reference).unwrap();
    assert_eq!(serialized_reference["thread_id"], thread_id.to_string());
    assert_eq!(serialized_reference["call_id"], "custom-1");
    assert_eq!(
        serialized_reference["sha256"],
        codex_history::archive_reference::content_sha256(&body)
    );
    let index = ObservationIndex::new(&canonical, &thread_id);
    let (reference, raw) = index
        .reference("custom-1", &thread_id, &HashSet::new())
        .unwrap();
    assert_eq!(reference, prepared.pairs[0].reference);
    let RolloutItem::ResponseItem(canonical_output) = &canonical[1] else {
        unreachable!();
    };
    assert!(std::ptr::eq(raw.output, canonical_output));
    let resolved = index
        .resolve_reference(&prepared.pairs[0].reference, &HashSet::new())
        .unwrap();
    assert!(std::ptr::eq(resolved.output, canonical_output));
    let mut tampered_reference = prepared.pairs[0].reference.clone();
    tampered_reference.sha256 = "00".repeat(32);
    assert!(
        index
            .resolve_reference(&tampered_reference, &HashSet::new())
            .is_err()
    );
    assert!(
        index
            .reference("custom-1", &ThreadId::from_u128(999), &HashSet::new())
            .is_err()
    );
    let active = HashSet::from(["custom-1".to_owned()]);
    assert!(matches!(
        index.reference("custom-1", &thread_id, &active),
        Err(ArchiveEvidenceError::Ineligible(
            ArchiveEvidenceIneligibility::ActiveCall
        ))
    ));
}

#[test]
fn prepare_compaction_sources_indexes_apply_patch_and_regular_function_observations() {
    let thread_id = ThreadId::from_u128(44);
    let selected = vec![
        custom_call("apply-1", "apply_patch"),
        custom_output("apply-1", Some("apply_patch"), "patch response"),
        call("function-1", "lookup_document"),
        output_named("function-1", "lookup_document", "lookup response"),
    ];
    let canonical = selected
        .iter()
        .cloned()
        .map(RolloutItem::ResponseItem)
        .collect::<Vec<_>>();

    let prepared =
        prepare_compaction_sources(&selected, &canonical, &thread_id, &HashSet::new()).unwrap();

    assert_eq!(prepared.pairs.len(), 2);
    assert_eq!(
        (prepared.pairs[0].call_index, prepared.pairs[0].output_index),
        (0, 1)
    );
    assert_eq!(
        (prepared.pairs[1].call_index, prepared.pairs[1].output_index),
        (2, 3)
    );
    let reference_json = serde_json::to_value(&prepared.pairs[0].reference).unwrap();
    assert_eq!(reference_json.as_object().unwrap().len(), 3);
    assert!(reference_json.get("status").is_none());
    assert!(reference_json.get("exit_code").is_none());
}

#[test]
fn prepare_compaction_sources_protects_empty_generic_call_id() {
    let thread_id = ThreadId::from_u128(46);
    let selected = vec![
        call("", "lookup_document"),
        output_named("", "lookup_document", "returned"),
    ];
    let canonical = selected
        .iter()
        .cloned()
        .map(RolloutItem::ResponseItem)
        .collect::<Vec<_>>();
    let index = ObservationIndex::new(&canonical, &thread_id);

    assert!(index.reference("", &thread_id, &HashSet::new()).is_err());
    let prepared =
        prepare_compaction_sources(&selected, &canonical, &thread_id, &HashSet::new()).unwrap();
    assert!(prepared.pairs.is_empty());
    assert_eq!(prepared.protected_indices, vec![0, 1]);
}

#[test]
fn prepare_compaction_sources_protects_custom_kind_mismatch_and_active_calls() {
    let thread_id = ThreadId::from_u128(42);
    let call = custom_call("same-id", "read_document");
    let mismatched_output = output_named("same-id", "read_document", "returned");
    let selected = vec![call.clone(), mismatched_output.clone()];
    let canonical = vec![
        RolloutItem::ResponseItem(call),
        RolloutItem::ResponseItem(mismatched_output),
    ];

    let prepared =
        prepare_compaction_sources(&selected, &canonical, &thread_id, &HashSet::new()).unwrap();
    assert!(prepared.pairs.is_empty());
    assert_eq!(prepared.protected_indices, vec![0, 1]);

    let active_selected = vec![
        custom_call("active-custom", "read_document"),
        custom_output("active-custom", Some("read_document"), "returned"),
    ];
    let active_canonical = active_selected
        .iter()
        .cloned()
        .map(RolloutItem::ResponseItem)
        .collect::<Vec<_>>();
    let active = HashSet::from(["active-custom".to_owned()]);
    let prepared =
        prepare_compaction_sources(&active_selected, &active_canonical, &thread_id, &active)
            .unwrap();
    assert!(prepared.pairs.is_empty());
    assert_eq!(prepared.protected_indices, vec![0, 1]);
}

#[test]
fn prepare_compaction_sources_protects_nonfinal_media_and_changed_selected_observations() {
    let thread_id = ThreadId::from_u128(45);

    let mut nonfinal_call = custom_call("nonfinal", "read_document");
    if let ResponseItem::CustomToolCall { status, .. } = &mut nonfinal_call.item {
        *status = Some("in_progress".into());
    }
    let nonfinal_output = custom_output("nonfinal", Some("read_document"), "pending body");
    let nonfinal_selected = vec![nonfinal_call.clone(), nonfinal_output.clone()];
    let nonfinal_canonical = vec![
        RolloutItem::ResponseItem(nonfinal_call),
        RolloutItem::ResponseItem(nonfinal_output),
    ];
    let prepared = prepare_compaction_sources(
        &nonfinal_selected,
        &nonfinal_canonical,
        &thread_id,
        &HashSet::new(),
    )
    .unwrap();
    assert!(prepared.pairs.is_empty());
    assert_eq!(prepared.protected_indices, vec![0, 1]);

    let mut special_call = custom_call("special", "read_document");
    if let ResponseItem::CustomToolCall {
        internal_chat_message_metadata_passthrough,
        ..
    } = &mut special_call.item
    {
        *internal_chat_message_metadata_passthrough =
            Some(InternalChatMessageMetadataPassthrough {
                cell_id: Some("code-mode-cell".into()),
                ..Default::default()
            });
    }
    let special_output = custom_output("special", Some("read_document"), "returned");
    let special_selected = vec![special_call.clone(), special_output.clone()];
    let special_canonical = vec![
        RolloutItem::ResponseItem(special_call),
        RolloutItem::ResponseItem(special_output),
    ];
    let prepared = prepare_compaction_sources(
        &special_selected,
        &special_canonical,
        &thread_id,
        &HashSet::new(),
    )
    .unwrap();
    assert!(prepared.pairs.is_empty());

    let mut media_output = custom_output("media-custom", Some("read_document"), "ignored");
    if let ResponseItem::CustomToolCallOutput { output, .. } = &mut media_output.item {
        *output = FunctionCallOutputPayload::from_content_items(vec![]);
    }
    let media_selected = vec![
        custom_call("media-custom", "read_document"),
        media_output.clone(),
    ];
    let media_canonical = media_selected
        .iter()
        .cloned()
        .map(RolloutItem::ResponseItem)
        .collect::<Vec<_>>();
    let prepared = prepare_compaction_sources(
        &media_selected,
        &media_canonical,
        &thread_id,
        &HashSet::new(),
    )
    .unwrap();
    assert!(prepared.pairs.is_empty());

    let selected = vec![
        custom_call("changed", "read_document"),
        custom_output("changed", Some("read_document"), "selected text"),
    ];
    let canonical = vec![
        RolloutItem::ResponseItem(custom_call("changed", "read_document")),
        RolloutItem::ResponseItem(custom_output(
            "changed",
            Some("read_document"),
            "different persisted text",
        )),
    ];
    let prepared =
        prepare_compaction_sources(&selected, &canonical, &thread_id, &HashSet::new()).unwrap();
    assert!(prepared.pairs.is_empty());
    assert_eq!(prepared.protected_indices, vec![0, 1]);
}

#[test]
fn prepare_compaction_sources_handles_many_borrowed_generic_observations() {
    let thread_id = ThreadId::from_u128(43);
    let mut selected = Vec::new();
    let mut canonical = Vec::new();
    for index in 0..128 {
        let call_id = format!("custom-{index}");
        let body_size = if index == 0 {
            MAX_ARCHIVE_EVIDENCE_BYTES + 1
        } else {
            4096
        };
        let body = format!("observation-{index}:{}", "x".repeat(body_size));
        let call = custom_call(&call_id, "read_document");
        let output = custom_output(&call_id, Some("read_document"), &body);
        canonical.push(RolloutItem::ResponseItem(call.clone()));
        canonical.push(RolloutItem::ResponseItem(output.clone()));
        selected.push(call);
        selected.push(output);
    }

    let prepared =
        prepare_compaction_sources(&selected, &canonical, &thread_id, &HashSet::new()).unwrap();

    assert_eq!(prepared.pairs.len(), 128);
    assert!(prepared.protected_indices.is_empty());
    assert_eq!(prepared.pairs.first().unwrap().call_index, 0);
    assert_eq!(prepared.pairs.last().unwrap().output_index, 255);
    let first_reference = serde_json::to_string(&prepared.pairs[0].reference).unwrap();
    assert!(!first_reference.contains("observation-0:"));
}

#[test]
fn prepare_compaction_sources_protects_active_pending_media_duplicate_and_out_of_order_items() {
    let thread_id = ThreadId::from_u128(1);
    let mut canonical = Vec::new();
    let mut outputs = Vec::new();
    for call_id in ["active", "media", "duplicate", "call-after-output", "valid"] {
        let (_, items, output) = fixture_for(call_id, CommandExecutionStatus::Completed, Some(0));
        canonical.extend(items);
        outputs.push(output);
    }
    let mut media = outputs[1].clone();
    if let ResponseItem::FunctionCallOutput { output, .. } = &mut media.item {
        *output = FunctionCallOutputPayload::from_content_items(vec![]);
    }
    let selected = vec![
        // There is a call, but no result and it remains in progress.
        call("pending", "exec_command"),
        call("active", "exec_command"),
        outputs[0].clone(),
        call("media", "exec_command"),
        media,
        call("duplicate", "exec_command"),
        call("duplicate", "exec_command"),
        outputs[2].clone(),
        outputs[2].clone(),
        outputs[3].clone(),
        call("call-after-output", "exec_command"),
        call("valid", "exec_command"),
        ResponseItemEnvelope::new(
            serde_json::from_value(serde_json::json!({
                "type":"message",
                "role":"assistant",
                "content":[{"type":"output_text","text":"intervening"}]
            }))
            .unwrap(),
        ),
        outputs[4].clone(),
    ];
    let mut active = HashSet::new();
    active.insert("active".into());
    active.insert("pending".into());

    let prepared = prepare_compaction_sources(&selected, &canonical, &thread_id, &active).unwrap();

    assert_eq!(prepared.pairs.len(), 1);
    assert_eq!(prepared.pairs[0].call_index, 11);
    assert_eq!(prepared.pairs[0].output_index, 13);
    assert_eq!(
        prepared.pairs[0].reference_kind,
        PreparedCompactionReferenceKind::Fresh
    );
    assert_eq!(
        prepared.protected_indices,
        (0..selected.len())
            .filter(|i| *i != 11 && *i != 13)
            .collect::<Vec<_>>()
    );

    let (_, duplicate_call_canonical, duplicate_call_output) =
        fixture_for("duplicate-call", CommandExecutionStatus::Completed, Some(0));
    let duplicate_call = call("duplicate-call", "exec_command");
    assert!(
        resolve_completed_work_evidence_from_items(
            &duplicate_call_canonical,
            &thread_id,
            "duplicate-call",
            &duplicate_call,
            Some(&duplicate_call_output),
            None,
            &HashSet::new(),
        )
        .is_ok()
    );
    let duplicate_calls = vec![
        duplicate_call.clone(),
        duplicate_call,
        duplicate_call_output,
    ];
    let duplicate_call_result = prepare_compaction_sources(
        &duplicate_calls,
        &duplicate_call_canonical,
        &thread_id,
        &HashSet::new(),
    )
    .unwrap();
    assert!(duplicate_call_result.pairs.is_empty());
    assert_eq!(duplicate_call_result.protected_indices, vec![0, 1, 2]);

    let (_, duplicate_output_canonical, duplicate_output) = fixture_for(
        "duplicate-output",
        CommandExecutionStatus::Completed,
        Some(0),
    );
    let duplicate_call = call("duplicate-output", "exec_command");
    assert!(
        resolve_completed_work_evidence_from_items(
            &duplicate_output_canonical,
            &thread_id,
            "duplicate-output",
            &duplicate_call,
            Some(&duplicate_output),
            None,
            &HashSet::new(),
        )
        .is_ok()
    );
    let duplicate_outputs = vec![duplicate_call, duplicate_output.clone(), duplicate_output];
    let duplicate_output_result = prepare_compaction_sources(
        &duplicate_outputs,
        &duplicate_output_canonical,
        &thread_id,
        &HashSet::new(),
    )
    .unwrap();
    assert!(duplicate_output_result.pairs.is_empty());
    assert_eq!(duplicate_output_result.protected_indices, vec![0, 1, 2]);
}

#[test]
fn prepare_compaction_sources_validates_unpaired_existing_markers_and_fails_closed() {
    let (thread_id, canonical, output) = fixture(CommandExecutionStatus::Completed, Some(0));
    let evidence = resolve_archive_evidence_from_items(
        &canonical,
        &thread_id,
        "call-1",
        Some(&output),
        None,
        &HashSet::new(),
    )
    .unwrap();
    let mut marker = output.clone();
    assert!(
        codex_history::archive_reference::apply_reference(&mut marker, evidence.reference.clone())
            .unwrap()
    );

    // A marker without a selected call remains protected, but its canonical evidence is checked.
    let prepared =
        prepare_compaction_sources(&[marker.clone()], &canonical, &thread_id, &HashSet::new())
            .unwrap();
    assert!(prepared.pairs.is_empty());
    assert_eq!(prepared.protected_indices, vec![0]);

    let mut tampered_marker = marker.clone();
    if let ResponseItem::FunctionCallOutput { output, .. } = &mut tampered_marker.item {
        *output = FunctionCallOutputPayload::from_text("tampered".into());
    }
    assert!(
        prepare_compaction_sources(&[tampered_marker], &canonical, &thread_id, &HashSet::new(),)
            .is_err()
    );

    let mut tampered_identity = marker.clone();
    if let ResponseItem::FunctionCallOutput { name, .. } = &mut tampered_identity.item {
        *name = Some("different_tool".into());
    }
    assert!(
        prepare_compaction_sources(
            &[tampered_identity],
            &canonical,
            &thread_id,
            &HashSet::new(),
        )
        .is_err()
    );

    assert!(
        prepare_compaction_sources(
            &[marker.clone()],
            &canonical[..2],
            &thread_id,
            &HashSet::new(),
        )
        .is_err()
    );

    let wrong_thread = ThreadId::from_u128(2);
    assert!(
        prepare_compaction_sources(&[marker], &canonical, &wrong_thread, &HashSet::new(),).is_err()
    );
}

#[test]
fn completed_and_failed_terminal_results_are_retrievable() {
    for (status, exit_code, expected) in [
        (
            CommandExecutionStatus::Completed,
            Some(0),
            ArchiveTerminalStatus::Completed,
        ),
        (
            CommandExecutionStatus::Failed,
            Some(9),
            ArchiveTerminalStatus::Failed,
        ),
    ] {
        let (thread_id, items, output) = fixture(status, exit_code);
        let evidence = resolve_archive_evidence_from_items(
            &items,
            &thread_id,
            "call-1",
            Some(&output),
            None,
            &HashSet::new(),
        )
        .unwrap();
        assert_eq!(evidence.reference.status, expected);
        assert_eq!(evidence.reference.exit_code, exit_code.unwrap());
        assert!(evidence.result.contains("persisted command result"));
        let reloaded_reference = resolve_archive_evidence_from_items(
            &items,
            &thread_id,
            "call-1",
            None,
            Some(&evidence.reference),
            &HashSet::new(),
        )
        .unwrap();
        assert_eq!(reloaded_reference, evidence);
    }
}

#[test]
fn completed_work_requires_the_exact_current_call_and_keeps_v1_lookup_compatible() {
    let (thread_id, mut items, output) = fixture(CommandExecutionStatus::Failed, Some(9));
    items.insert(
        1,
        RolloutItem::ResponseItem(ResponseItemEnvelope::new(
            serde_json::from_value(serde_json::json!({
                "type":"message",
                "role":"assistant",
                "content":[{"type":"output_text","text":"between call and result"}]
            }))
            .unwrap(),
        )),
    );
    let current_call = call_with_arguments("call-1", "exec_command", "{\"cmd\":\"false\"}");
    if let RolloutItem::ResponseItem(canonical_call) = &mut items[0] {
        canonical_call.item = current_call.item.clone();
    }

    let evidence = resolve_completed_work_evidence_from_items(
        &items,
        &thread_id,
        "call-1",
        &current_call,
        Some(&output),
        None,
        &HashSet::new(),
    )
    .unwrap();
    assert_eq!(evidence.reference.status, ArchiveTerminalStatus::Failed);
    assert_eq!(evidence.reference.exit_code, 9);

    let changed_call = call_with_arguments("call-1", "exec_command", "{\"cmd\":\"true\"}");
    assert!(matches!(
        resolve_completed_work_evidence_from_items(
            &items,
            &thread_id,
            "call-1",
            &changed_call,
            Some(&output),
            None,
            &HashSet::new(),
        ),
        Err(ArchiveEvidenceError::Ineligible(
            ArchiveEvidenceIneligibility::OutputMismatch
        ))
    ));

    let mut marker = output.clone();
    assert!(
        codex_history::archive_reference::apply_reference(&mut marker, evidence.reference.clone())
            .unwrap(),
        "fixture result must shrink into a v1 marker"
    );
    let retrieved = resolve_completed_work_evidence_from_items(
        &items,
        &thread_id,
        "call-1",
        &current_call,
        None,
        Some(&evidence.reference),
        &HashSet::new(),
    )
    .unwrap();
    assert_eq!(retrieved, evidence);
    assert_eq!(
        codex_history::archive_reference::validate_marker(
            &marker,
            &thread_id.to_string(),
            &evidence.reference,
        ),
        Ok(())
    );
}

#[test]
fn mismatched_thread_missing_terminal_active_and_ambiguous_are_rejected() {
    let (thread_id, items, output) = fixture(CommandExecutionStatus::Completed, Some(0));
    assert!(
        resolve_archive_evidence_from_items(
            &items,
            &ThreadId::from_u128(2),
            "call-1",
            Some(&output),
            None,
            &HashSet::new(),
        )
        .is_err()
    );
    assert!(
        resolve_archive_evidence_from_items(
            &items[..2],
            &thread_id,
            "call-1",
            Some(&output),
            None,
            &HashSet::new(),
        )
        .is_err()
    );

    let mut active = HashSet::new();
    active.insert("call-1".into());
    assert!(
        resolve_archive_evidence_from_items(
            &items,
            &thread_id,
            "call-1",
            Some(&output),
            None,
            &active,
        )
        .is_err()
    );

    let mut ambiguous = items.clone();
    ambiguous.push(RolloutItem::ResponseItem(call("call-1", "exec_command")));
    assert!(
        resolve_archive_evidence_from_items(
            &ambiguous,
            &thread_id,
            "call-1",
            Some(&output),
            None,
            &HashSet::new(),
        )
        .is_err()
    );
}

#[test]
fn media_unknown_special_and_synthetic_terminal_results_are_rejected() {
    let (thread_id, mut items, output) = fixture(CommandExecutionStatus::Completed, Some(0));
    let mut media = output.clone();
    if let ResponseItem::FunctionCallOutput { output, .. } = &mut media.item {
        *output = FunctionCallOutputPayload::from_content_items(vec![]);
    }
    items[1] = RolloutItem::ResponseItem(media.clone());
    assert!(
        resolve_archive_evidence_from_items(
            &items,
            &thread_id,
            "call-1",
            Some(&media),
            None,
            &HashSet::new(),
        )
        .is_err()
    );

    let (thread_id, mut unknown, output) = fixture(CommandExecutionStatus::Completed, Some(0));
    unknown[0] = RolloutItem::ResponseItem(call("call-1", "custom_tool"));
    assert!(
        resolve_archive_evidence_from_items(
            &unknown,
            &thread_id,
            "call-1",
            Some(&output),
            None,
            &HashSet::new(),
        )
        .is_err()
    );

    let (thread_id, mut special, output) = fixture(CommandExecutionStatus::Completed, Some(0));
    if let RolloutItem::ResponseItem(envelope) = &mut special[0] {
        if let ResponseItem::FunctionCall {
            internal_chat_message_metadata_passthrough,
            ..
        } = &mut envelope.item
        {
            *internal_chat_message_metadata_passthrough =
                Some(InternalChatMessageMetadataPassthrough {
                    cell_id: Some("code-cell".into()),
                    ..Default::default()
                });
        }
    }
    assert!(
        resolve_archive_evidence_from_items(
            &special,
            &thread_id,
            "call-1",
            Some(&output),
            None,
            &HashSet::new(),
        )
        .is_err()
    );

    let (thread_id, mut write_stdin, output) = fixture(CommandExecutionStatus::Completed, Some(0));
    write_stdin[0] = RolloutItem::ResponseItem(call("call-1", "write_stdin"));
    if let RolloutItem::ResponseItem(envelope) = &mut write_stdin[1] {
        if let ResponseItem::FunctionCallOutput { name, .. } = &mut envelope.item {
            *name = Some("write_stdin".into());
        }
    }
    assert!(
        resolve_archive_evidence_from_items(
            &write_stdin,
            &thread_id,
            "call-1",
            Some(&output),
            None,
            &HashSet::new(),
        )
        .is_err()
    );

    let (thread_id, pending, output) = fixture(CommandExecutionStatus::InProgress, Some(0));
    assert!(
        resolve_archive_evidence_from_items(
            &pending,
            &thread_id,
            "call-1",
            Some(&output),
            None,
            &HashSet::new(),
        )
        .is_err()
    );

    let (thread_id, synthetic, output) = fixture(CommandExecutionStatus::Failed, None);
    assert!(
        resolve_archive_evidence_from_items(
            &synthetic,
            &thread_id,
            "call-1",
            Some(&output),
            None,
            &HashSet::new(),
        )
        .is_err()
    );

    for (status, exit_code) in [
        (CommandExecutionStatus::Completed, Some(9)),
        (CommandExecutionStatus::Failed, Some(0)),
    ] {
        let (thread_id, contradictory, output) = fixture(status, exit_code);
        assert!(
            resolve_archive_evidence_from_items(
                &contradictory,
                &thread_id,
                "call-1",
                Some(&output),
                None,
                &HashSet::new(),
            )
            .is_err()
        );
    }
}

#[test]
fn realistic_fallback_metadata_is_preserved_by_evidence_resolution() {
    let (thread_id, mut items, mut output) = fixture(CommandExecutionStatus::Completed, Some(0));
    let metadata = CodexHarnessMetadata {
        history_truncation_token_limit: Some(4800),
        ..Default::default()
    };
    if let RolloutItem::ResponseItem(envelope) = &mut items[1] {
        envelope.metadata = Some(metadata.clone());
    }
    output.metadata = Some(metadata);
    let evidence = resolve_archive_evidence_from_items(
        &items,
        &thread_id,
        "call-1",
        Some(&output),
        None,
        &HashSet::new(),
    )
    .unwrap();
    assert_eq!(evidence.reference.status, ArchiveTerminalStatus::Completed);
    assert_eq!(evidence.reference.exit_code, 0);
}

#[test]
fn mixed_write_stdin_and_completed_exec_select_independently() {
    let thread_id = ThreadId::from_u128(1);
    let pending_output = output_named("stdin-call", "write_stdin", "pending output");
    let pending_exec_output = output("pending-exec-call", "still running");
    let (exec_thread, completed_items, completed_output) =
        fixture_for("exec-call", CommandExecutionStatus::Completed, Some(0));
    assert_eq!(thread_id, exec_thread);
    let mut mixed = vec![
        RolloutItem::ResponseItem(call("stdin-call", "write_stdin")),
        RolloutItem::ResponseItem(pending_output.clone()),
        RolloutItem::ResponseItem(call("pending-exec-call", "exec_command")),
        RolloutItem::ResponseItem(pending_exec_output.clone()),
    ];
    mixed.extend(completed_items);

    assert_eq!(
        select_new_archive_evidence_from_items(
            &mixed,
            &thread_id,
            "stdin-call",
            &pending_output,
            &HashSet::new(),
        )
        .unwrap(),
        ArchiveEvidenceSelection::Ineligible
    );
    assert_eq!(
        select_new_archive_evidence_from_items(
            &mixed,
            &thread_id,
            "pending-exec-call",
            &pending_exec_output,
            &HashSet::new(),
        )
        .unwrap(),
        ArchiveEvidenceSelection::Ineligible
    );
    assert!(matches!(
        select_new_archive_evidence_from_items(
            &mixed,
            &thread_id,
            "exec-call",
            &completed_output,
            &HashSet::new(),
        )
        .unwrap(),
        ArchiveEvidenceSelection::Eligible(_)
    ));
    let RolloutItem::ResponseItem(actual_pending) = &mixed[1] else {
        panic!("expected pending write_stdin response item");
    };
    assert_eq!(actual_pending, &pending_output);
    let RolloutItem::ResponseItem(actual_pending_exec) = &mixed[3] else {
        panic!("expected pending exec response item");
    };
    assert_eq!(actual_pending_exec, &pending_exec_output);
}

#[tokio::test]
async fn persisted_lookup_rejects_hash_and_parse_corruption() {
    let (thread_id, items, _) = fixture(CommandExecutionStatus::Completed, Some(0));
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("rollout.jsonl");
    let mut meta = SessionMeta::default();
    meta.id = thread_id;
    meta.session_id = thread_id.into();
    let meta_item = RolloutItem::SessionMeta(SessionMetaLine { meta, git: None });
    let mut contents = String::new();
    for item in std::iter::once(meta_item).chain(items) {
        contents.push_str(
            &serde_json::to_string(&RolloutLine {
                timestamp: "2026-01-01T00:00:00Z".into(),
                ordinal: None,
                item,
            })
            .unwrap(),
        );
        contents.push('\n');
    }
    contents.push_str("{broken\n");
    std::fs::write(&path, contents).unwrap();
    let result = resolve_archive_evidence(
        &path,
        &thread_id,
        "call-1",
        "0000000000000000000000000000000000000000000000000000000000000000",
    )
    .await;
    assert!(result.is_err());

    let mut clean = std::fs::read_to_string(&path).unwrap();
    clean.truncate(clean.rfind("{broken").unwrap());
    std::fs::write(&path, clean).unwrap();
    let result = resolve_archive_evidence(&path, &thread_id, "call-1", "wrong").await;
    assert!(result.is_err());
}
