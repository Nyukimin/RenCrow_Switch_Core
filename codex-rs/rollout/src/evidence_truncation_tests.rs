//! Regression coverage for live-history truncation versus canonical rollout observations.

use codex_history::CodexHarnessMetadata;
use codex_history::ResponseItemEnvelope;
use codex_history::RolloutItem;
use codex_history::archive_reference::content_sha256;
use codex_protocol::ThreadId;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::truncate_function_output_payload;
use std::collections::HashSet;

fn function_call(id: &str) -> ResponseItemEnvelope {
    ResponseItemEnvelope::new(ResponseItem::FunctionCall {
        id: None,
        name: "read_document".into(),
        namespace: None,
        arguments: "{\"path\":\"a\"}".into(),
        encrypted_function_args: None,
        call_id: id.into(),
        internal_chat_message_metadata_passthrough: None,
    })
}

fn function_output(id: &str, body: &str) -> ResponseItemEnvelope {
    ResponseItemEnvelope::new(ResponseItem::FunctionCallOutput {
        id: None,
        call_id: Some(id.into()),
        name: Some("read_document".into()),
        namespace: None,
        output: FunctionCallOutputPayload::from_text(body.into()),
        internal_chat_message_metadata_passthrough: None,
    })
}

fn custom_call(id: &str) -> ResponseItemEnvelope {
    ResponseItemEnvelope::new(ResponseItem::CustomToolCall {
        id: None,
        status: Some("completed".into()),
        call_id: id.into(),
        name: "read_document".into(),
        namespace: None,
        input: "{\"path\":\"b\"}".into(),
        internal_chat_message_metadata_passthrough: None,
    })
}

fn custom_output(id: &str, body: &str) -> ResponseItemEnvelope {
    ResponseItemEnvelope::new(ResponseItem::CustomToolCallOutput {
        id: None,
        call_id: id.into(),
        name: Some("read_document".into()),
        output: FunctionCallOutputPayload::from_text(body.into()),
        internal_chat_message_metadata_passthrough: None,
    })
}

fn with_history_budget(
    mut envelope: ResponseItemEnvelope,
    budget: Option<usize>,
) -> ResponseItemEnvelope {
    if let Some(budget) = budget {
        envelope
            .metadata
            .get_or_insert_with(CodexHarnessMetadata::default)
            .history_truncation_token_limit = Some(budget);
    }
    envelope
}

fn truncate_live_output(mut envelope: ResponseItemEnvelope, budget: usize) -> ResponseItemEnvelope {
    let output = match &mut envelope.item {
        ResponseItem::FunctionCallOutput { output, .. }
        | ResponseItem::CustomToolCallOutput { output, .. } => output,
        _ => panic!("fixture must be a tool output"),
    };
    truncate_function_output_payload(output, TruncationPolicy::Tokens(budget), |_| 0);
    envelope
}

#[test]
fn fresh_function_pair_accepts_exact_live_truncation_and_binds_raw_canonical_bytes() {
    let thread = ThreadId::from_u128(41_010);
    let budget = 160;
    let raw = format!(
        "{}{}{}",
        "head segment ".repeat(800),
        "middle-only marker ".repeat(800),
        "tail segment ".repeat(800)
    );
    let call = function_call("function-truncated");
    let canonical_output =
        with_history_budget(function_output("function-truncated", &raw), Some(budget));
    let live_output = truncate_live_output(canonical_output.clone(), budget);
    assert_ne!(live_output.item, canonical_output.item);
    let selected = vec![call.clone(), live_output];
    let canonical = vec![
        RolloutItem::ResponseItem(call),
        RolloutItem::ResponseItem(canonical_output),
    ];

    let prepared =
        super::prepare_compaction_sources(&selected, &canonical, &thread, &HashSet::new()).unwrap();

    assert_eq!(prepared.pairs.len(), 1);
    let pair = &prepared.pairs[0];
    assert_eq!(pair.reference.sha256, content_sha256(&raw));
    assert_eq!(pair.output_total_bytes, raw.len());
    assert_eq!(pair.tool_name, "read_document");
    assert_eq!(pair.canonical_call_input, Some("{\"path\":\"a\"}"));
    let canonical_text = pair.canonical_output_text.unwrap();
    let canonical_body = match &canonical[1] {
        RolloutItem::ResponseItem(envelope) => match &envelope.item {
            ResponseItem::FunctionCallOutput { output, .. } => output.text_content().unwrap(),
            _ => unreachable!(),
        },
        _ => unreachable!(),
    };
    assert!(std::ptr::eq(
        canonical_text.as_ptr(),
        canonical_body.as_ptr()
    ));
    let projected = codex_history::project_observation(
        &pair.reference,
        pair.tool_name,
        pair.canonical_call_input.unwrap(),
        canonical_text,
    )
    .unwrap();
    assert_eq!(projected.coverage.output.sha256, content_sha256(&raw));
    assert_eq!(projected.coverage.output.total_bytes, raw.len());
    assert!(
        projected
            .summary
            .output
            .excerpts
            .iter()
            .all(|text| !text.contains("middle-only marker"))
    );
}

#[test]
fn fresh_custom_pair_accepts_exact_live_truncation_and_keeps_raw_reference() {
    let thread = ThreadId::from_u128(41_011);
    let budget = 144;
    let raw = "custom head/middle/tail observation ".repeat(1_200);
    let call = custom_call("custom-truncated");
    let canonical_output =
        with_history_budget(custom_output("custom-truncated", &raw), Some(budget));
    let live_output = truncate_live_output(canonical_output.clone(), budget);
    assert_ne!(live_output.item, canonical_output.item);
    let selected = vec![call.clone(), live_output];
    let canonical = vec![
        RolloutItem::ResponseItem(call),
        RolloutItem::ResponseItem(canonical_output),
    ];

    let prepared =
        super::prepare_compaction_sources(&selected, &canonical, &thread, &HashSet::new()).unwrap();

    assert_eq!(prepared.pairs.len(), 1);
    assert_eq!(prepared.pairs[0].reference.sha256, content_sha256(&raw));
    assert_eq!(prepared.pairs[0].output_total_bytes, raw.len());
    assert_eq!(
        prepared.pairs[0].canonical_call_input,
        Some("{\"path\":\"b\"}")
    );
    let canonical_body = match &canonical[1] {
        RolloutItem::ResponseItem(envelope) => match &envelope.item {
            ResponseItem::CustomToolCallOutput { output, .. } => output.text_content().unwrap(),
            _ => unreachable!(),
        },
        _ => unreachable!(),
    };
    assert!(std::ptr::eq(
        prepared.pairs[0].canonical_output_text.unwrap().as_ptr(),
        canonical_body.as_ptr()
    ));
}

#[test]
fn fresh_pairs_reject_missing_budget_and_any_noncanonical_change() {
    let thread = ThreadId::from_u128(41_012);
    let budget = 128;
    let raw = "output whose center is removed by history truncation ".repeat(700);

    let canonical_call = function_call("missing-budget");
    let canonical_output = function_output("missing-budget", &raw);
    let live_output = truncate_live_output(canonical_output.clone(), budget);
    assert_protected(
        &thread,
        canonical_call.clone(),
        canonical_call,
        live_output,
        canonical_output,
    );

    let mut canonical_call = function_call("call-metadata");
    canonical_call
        .metadata
        .get_or_insert_default()
        .history_truncation_token_limit = Some(32);
    let mut selected_call = canonical_call.clone();
    selected_call
        .metadata
        .as_mut()
        .unwrap()
        .history_truncation_token_limit = Some(33);
    let canonical_output =
        with_history_budget(function_output("call-metadata", &raw), Some(budget));
    let live_output = truncate_live_output(canonical_output.clone(), budget);
    assert_protected(
        &thread,
        selected_call,
        canonical_call,
        live_output,
        canonical_output,
    );

    let canonical_call = function_call("output-metadata");
    let canonical_output =
        with_history_budget(function_output("output-metadata", &raw), Some(budget));
    let mut live_output = truncate_live_output(canonical_output.clone(), budget);
    live_output
        .metadata
        .as_mut()
        .unwrap()
        .history_truncation_token_limit = Some(budget + 500);
    assert_protected(
        &thread,
        canonical_call.clone(),
        canonical_call,
        live_output,
        canonical_output,
    );

    let canonical_call = function_call("changed-success");
    let canonical_output =
        with_history_budget(function_output("changed-success", &raw), Some(budget));
    let mut live_output = truncate_live_output(canonical_output.clone(), budget);
    if let ResponseItem::FunctionCallOutput { output, .. } = &mut live_output.item {
        output.success = Some(false);
    }
    assert_protected(
        &thread,
        canonical_call.clone(),
        canonical_call,
        live_output,
        canonical_output,
    );

    let canonical_call = function_call("invented-body");
    let canonical_output =
        with_history_budget(function_output("invented-body", &raw), Some(budget));
    let selected_output = function_output("invented-body", "invented replacement");
    assert_protected(
        &thread,
        canonical_call.clone(),
        canonical_call,
        selected_output,
        canonical_output,
    );

    let canonical_call = custom_call("custom-success");
    let canonical_output = with_history_budget(custom_output("custom-success", &raw), Some(budget));
    let mut selected_output = truncate_live_output(canonical_output.clone(), budget);
    if let ResponseItem::CustomToolCallOutput { output, .. } = &mut selected_output.item {
        output.success = Some(false);
    }
    assert_protected(
        &thread,
        canonical_call.clone(),
        canonical_call,
        selected_output,
        canonical_output,
    );

    let canonical_call = function_call("raw-output-metadata");
    let canonical_output =
        with_history_budget(function_output("raw-output-metadata", &raw), Some(budget));
    let mut selected_output = function_output("raw-output-metadata", &raw);
    selected_output.metadata = None;
    assert_protected(
        &thread,
        canonical_call.clone(),
        canonical_call,
        selected_output,
        canonical_output,
    );
}

fn assert_protected(
    thread: &ThreadId,
    selected_call: ResponseItemEnvelope,
    canonical_call: ResponseItemEnvelope,
    selected_output: ResponseItemEnvelope,
    canonical_output: ResponseItemEnvelope,
) {
    let selected = vec![selected_call, selected_output];
    let canonical = vec![
        RolloutItem::ResponseItem(canonical_call),
        RolloutItem::ResponseItem(canonical_output),
    ];
    let prepared =
        super::prepare_compaction_sources(&selected, &canonical, thread, &HashSet::new()).unwrap();
    assert!(prepared.pairs.is_empty());
    assert_eq!(prepared.protected_indices, vec![0, 1]);
}
