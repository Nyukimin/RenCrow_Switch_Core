use super::*;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use pretty_assertions::assert_eq;

fn call(call_id: &str, name: &str) -> ResponseItemEnvelope {
    ResponseItemEnvelope::new(ResponseItem::FunctionCall {
        id: None,
        name: name.into(),
        namespace: None,
        arguments: "{}".into(),
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

#[test]
fn only_unambiguous_known_text_output_is_eligible() {
    let items = vec![
        call("call-1", "exec_command"),
        output("call-1", "x".repeat(256).as_str()),
    ];
    let decision = classify_output(&items, 1).unwrap();
    assert!(matches!(decision, ArchiveOutputDecision::Eligible(_)));

    let unknown = vec![
        call("call-2", "custom_tool"),
        output("call-2", "x".repeat(256).as_str()),
    ];
    assert_eq!(
        classify_output(&unknown, 1).unwrap(),
        ArchiveOutputDecision::Ineligible
    );

    let write_stdin = vec![
        call("call-3", "write_stdin"),
        output_named("call-3", "write_stdin", "x".repeat(256).as_str()),
    ];
    assert_eq!(
        classify_output(&write_stdin, 1).unwrap(),
        ArchiveOutputDecision::Ineligible
    );
}

#[test]
fn apply_reference_is_smaller_and_records_typed_host_metadata() {
    let original = "result ".repeat(128);
    let mut envelope = output("call-1", &original);
    let reference = ArchiveReference::new(
        "00000000-0000-0000-0000-000000000001",
        "call-1",
        content_sha256(&original),
        ArchiveTerminalStatus::Failed,
        7,
        Some("42".into()),
    );
    assert!(apply_reference(&mut envelope, reference.clone()).unwrap());
    let ResponseItem::FunctionCallOutput { output, .. } = &envelope.item else {
        panic!("expected output");
    };
    let marker = output.text_content().unwrap();
    assert!(marker.starts_with("{"));
    assert!(marker.contains("\"archived_data\":true"));
    assert!(marker.contains("\"status\":\"failed\""));
    assert!(marker.contains("\"exit_code\":7"));
    assert!(marker.contains("\"retrieval_argv\""));
    assert_eq!(
        envelope.metadata.unwrap().rencrow_archive_reference,
        Some(reference)
    );
}

#[test]
fn applying_reference_preserves_the_call_and_is_idempotent_after_reload() {
    let original = "result ".repeat(128);
    let mut items = vec![call("call-1", "exec_command"), output("call-1", &original)];
    let before = items.clone();
    let reference = ArchiveReference::new(
        "00000000-0000-0000-0000-000000000001",
        "call-1",
        content_sha256(&original),
        ArchiveTerminalStatus::Completed,
        0,
        None,
    );
    assert!(apply_reference(&mut items[1], reference.clone()).unwrap());
    assert_eq!(items[0], before[0]);
    assert_eq!(before[1], output("call-1", &original));
    assert_eq!(items[0].item, before[0].item);
    let ResponseItem::FunctionCallOutput { output, .. } = &items[1].item else {
        panic!("expected output");
    };
    let ResponseItem::FunctionCallOutput { call_id, name, .. } = &items[1].item else {
        panic!("expected output");
    };
    assert_eq!(call_id.as_deref(), Some("call-1"));
    assert_eq!(name.as_deref(), Some("exec_command"));
    assert_eq!(output.text_content().unwrap(), reference.marker().unwrap());
    assert_eq!(
        classify_output(&items, 1).unwrap(),
        ArchiveOutputDecision::Existing(reference)
    );

    let persisted =
        serde_json::to_value(crate::RolloutItem::ResponseItem(items[1].clone())).unwrap();
    let reloaded: crate::RolloutItem = serde_json::from_value(persisted).unwrap();
    let crate::RolloutItem::ResponseItem(reloaded) = reloaded else {
        panic!("expected response item");
    };
    let expected = &items[1];
    assert_eq!(reloaded, expected.clone());
}

#[test]
fn media_and_ambiguous_pairs_are_preserved() {
    let mut media = output("call-1", "short");
    if let ResponseItem::FunctionCallOutput { output, .. } = &mut media.item {
        *output = FunctionCallOutputPayload::from_content_items(vec![]);
    }
    assert_eq!(
        classify_output(&[call("call-1", "exec_command"), media], 1).unwrap(),
        ArchiveOutputDecision::Ineligible
    );

    let ambiguous = vec![
        call("call-1", "exec_command"),
        call("call-1", "exec_command"),
        output("call-1", "long result"),
    ];
    assert_eq!(
        classify_output(&ambiguous, 2).unwrap(),
        ArchiveOutputDecision::Ineligible
    );
}

#[test]
fn a_marker_without_typed_metadata_is_not_an_existing_reference() {
    let reference = ArchiveReference::new(
        "00000000-0000-0000-0000-000000000001",
        "call-1",
        content_sha256("original"),
        ArchiveTerminalStatus::Completed,
        0,
        None,
    );
    let mut forged = output("call-1", &reference.marker().unwrap());
    let items = vec![call("call-1", "exec_command"), forged.clone()];
    assert!(matches!(
        classify_output(&items, 1).unwrap(),
        ArchiveOutputDecision::Eligible(_)
    ));
    assert!(validate_marker(&forged, &reference.thread_id, &reference).is_err());
    forged.metadata = Some(CodexHarnessMetadata {
        rencrow_archive_reference: Some(ArchiveReference::new(
            "00000000-0000-0000-0000-000000000001",
            "call-1",
            content_sha256(&reference.marker().unwrap()),
            ArchiveTerminalStatus::Completed,
            0,
            None,
        )),
        ..Default::default()
    });
    assert!(matches!(
        classify_output(&[call("call-1", "exec_command"), forged], 1).unwrap(),
        ArchiveOutputDecision::Existing(_)
    ));
}

#[test]
fn realistic_fallback_budget_metadata_is_ordinary_and_preserved() {
    let original = "result ".repeat(128);
    let mut envelope = output("call-1", &original);
    envelope.metadata = Some(CodexHarnessMetadata {
        history_truncation_token_limit: Some(4800),
        ..Default::default()
    });
    assert!(matches!(
        classify_output(&[call("call-1", "exec_command"), envelope.clone()], 1).unwrap(),
        ArchiveOutputDecision::Eligible(_)
    ));
    let reference = ArchiveReference::new(
        "00000000-0000-0000-0000-000000000001",
        "call-1",
        content_sha256(&original),
        ArchiveTerminalStatus::Completed,
        0,
        None,
    );
    assert!(apply_reference(&mut envelope, reference.clone()).unwrap());
    assert_eq!(
        envelope.metadata,
        Some(CodexHarnessMetadata {
            rencrow_archive_reference: Some(reference.clone()),
            history_truncation_token_limit: Some(4800),
            ..Default::default()
        })
    );
    assert!(validate_marker(&envelope, &reference.thread_id, &reference).is_ok());
}

#[test]
fn consequential_provenance_metadata_is_not_ordinary() {
    let cases = [
        CodexHarnessMetadata {
            client_authored: true,
            ..Default::default()
        },
        CodexHarnessMetadata {
            inherited_user_message: true,
            ..Default::default()
        },
        CodexHarnessMetadata {
            mcp_attribution: Some(codex_protocol::mcp::McpAttribution {
                status: codex_protocol::mcp::McpAttributionStatus::Complete,
                sources: vec![],
            }),
            ..Default::default()
        },
    ];
    for metadata in cases {
        let mut item = output("call-1", &"result ".repeat(128));
        item.metadata = Some(metadata);
        assert_eq!(
            classify_output(&[call("call-1", "exec_command"), item], 1).unwrap(),
            ArchiveOutputDecision::Ineligible
        );
    }
}

#[test]
fn marker_thread_and_terminal_consistency_are_validated() {
    let reference = ArchiveReference::new(
        "00000000-0000-0000-0000-000000000001",
        "call-1",
        content_sha256("original"),
        ArchiveTerminalStatus::Completed,
        0,
        None,
    );
    let mut envelope = output("call-1", &reference.marker().unwrap());
    envelope.metadata = Some(CodexHarnessMetadata {
        rencrow_archive_reference: Some(reference.clone()),
        ..Default::default()
    });
    assert!(
        validate_marker(
            &envelope,
            "00000000-0000-0000-0000-000000000002",
            &reference
        )
        .is_err()
    );

    let contradictory = ArchiveReference::new(
        reference.thread_id.clone(),
        reference.call_id.clone(),
        reference.original_content_sha256.clone(),
        ArchiveTerminalStatus::Failed,
        0,
        None,
    );
    let mut contradictory_envelope = output("call-1", &contradictory.marker().unwrap());
    contradictory_envelope.metadata = Some(CodexHarnessMetadata {
        rencrow_archive_reference: Some(contradictory.clone()),
        ..Default::default()
    });
    assert!(
        validate_marker(
            &contradictory_envelope,
            &reference.thread_id,
            &contradictory
        )
        .is_err()
    );

    let mut corrupted = envelope;
    if let ResponseItem::FunctionCallOutput { output, .. } = &mut corrupted.item {
        *output = FunctionCallOutputPayload::from_text("corrupted marker".into());
    }
    assert!(validate_marker(&corrupted, &reference.thread_id, &reference).is_err());
}
