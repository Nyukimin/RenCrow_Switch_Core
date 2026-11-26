// Modified by RenCrow Switch Core, 2026-11-27.
// Typed projection and inference trace correlation tests.

use std::fs::File;
use std::io::BufRead;
use std::io::BufReader;
use std::sync::Arc;

use codex_api::ResponsesApiRequest;
use codex_history::task_context::TaskRunPhaseV1;
use codex_history::task_context::TaskRunStateV1;
use codex_history::task_context::TaskRunStatusV1;
use codex_protocol::protocol::TokenUsage;
use codex_rollout_trace::InferenceTraceContext;
use codex_rollout_trace::RawTraceEvent;
use codex_rollout_trace::RawTraceEventPayload;
use codex_rollout_trace::TraceWriter;
use codex_rollout_trace::replay_bundle;
use http::HeaderMap;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;

use super::*;

fn context() -> CommonHarnessInferenceReceiptContext {
    let run_state = TaskRunStateV1 {
        task_id: "task-17".into(),
        request_revision: 3,
        state_version: 11,
        owner_id: "owner-1".into(),
        owner_epoch: 4,
        status: TaskRunStatusV1::Running,
        phase: TaskRunPhaseV1::Review,
        attempt_no: 2,
        target_version: "sha256:target".into(),
        unmet_conditions: Vec::new(),
        next_operation: "review".into(),
        reason_code: None,
        resume_condition: None,
        cancel_requested: false,
        unsettled_operation_refs: Vec::new(),
        waiting: None,
        work_window: None,
    };
    CommonHarnessInferenceReceiptContext::from_persisted_run_state(
        &run_state,
        "openai:responses".into(),
        json!({
            "model": "requested-model",
            "reasoning_summary": "detailed",
            "verbosity": "high",
            "service_tier": "fast"
        }),
        ModelSettingsSupport {
            supports_reasoning_summary: false,
            supports_verbosity: false,
            supports_requested_service_tier: false,
            used_fallback_model_metadata: true,
        },
    )
}

fn final_request() -> ResponsesApiRequest {
    ResponsesApiRequest {
        model: "catalog-model".into(),
        max_output_tokens: Some(4096),
        instructions: String::new(),
        input: Vec::new(),
        tools: None,
        tool_choice: "auto".into(),
        parallel_tool_calls: true,
        reasoning: None,
        store: false,
        stream: true,
        stream_options: None,
        include: Vec::new(),
        service_tier: None,
        prompt_cache_key: None,
        text: None,
        client_metadata: None,
        access_programs: None,
    }
}

#[test]
fn receipt_projection_preserves_request_vs_provider_values_and_support_limits() {
    let request = final_request();
    let receipt = build_model_settings_receipt(&context(), &request)
        .expect("typed final request projects to receipt");

    assert_eq!(receipt.phase, TaskRunPhaseV1::Review);
    assert_eq!(receipt.wire_requested["model"], "requested-model");
    assert_eq!(receipt.model_identity.requested, "requested-model");
    assert_eq!(receipt.effective.model, "catalog-model");
    assert_eq!(receipt.effective.max_output_tokens, Some(4096));
    assert_eq!(receipt.effective_source, "provider_request");
    assert!(!receipt.observed);
    assert_eq!(receipt.model_identity.physical_backend.value, None);
    assert!(!receipt.model_identity.physical_backend.observed);
    assert_eq!(
        receipt.unsupported_fields,
        vec![
            "reasoning_summary".to_string(),
            "verbosity".to_string(),
            "service_tier".to_string(),
        ]
    );
    assert_eq!(
        receipt.fallback_reason.as_deref(),
        Some("fallback_model_metadata_used; unsupported_fields_omitted")
    );
    assert_eq!(receipt.call_ref, None);
    assert_eq!(receipt.usage_ref, None);
}

#[test]
fn final_request_receipt_is_linked_to_the_same_trace_attempt_and_replays() -> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let writer = Arc::new(TraceWriter::create(
        temp.path(),
        "trace-1".into(),
        "rollout-1".into(),
        "thread-root".into(),
    )?);
    writer.append(RawTraceEventPayload::ThreadStarted {
        thread_id: "thread-root".into(),
        agent_path: "/root".into(),
        metadata_payload: None,
    })?;
    writer.append(RawTraceEventPayload::CodexTurnStarted {
        codex_turn_id: "turn-1".into(),
        thread_id: "thread-root".into(),
    })?;
    let host_context = context();
    let trace = InferenceTraceContext::enabled(
        Arc::clone(&writer),
        "thread-root".into(),
        "turn-1".into(),
        "catalog-model".into(),
        "openai".into(),
    )
    .with_model_settings_receipt_context(&host_context);
    let request = final_request();
    let receipt_context = serde_json::from_value::<CommonHarnessInferenceReceiptContext>(
        trace
            .model_settings_receipt_context()
            .expect("host context was attached to the trace"),
    )?;
    let receipt = build_model_settings_receipt(&receipt_context, &request)?;
    let attempt = trace.start_attempt();
    let mut request_headers = HeaderMap::new();
    attempt.add_request_headers(&mut request_headers);
    let header_call_id = request_headers
        .get("x-codex-inference-call-id")
        .expect("same attempt adds call header")
        .to_str()?
        .to_owned();
    attempt.record_started(&request);
    attempt.record_model_settings_receipt(&receipt);
    attempt.record_verified_backend_model(None);
    attempt.record_completed(
        "response-1",
        Some("upstream-1"),
        &Some(TokenUsage {
            input_tokens: 10,
            cached_input_tokens: 0,
            cache_write_input_tokens: 0,
            output_tokens: 4,
            reasoning_output_tokens: 1,
            total_tokens: 14,
            codex_rollout_budget_units: None,
        }),
        &[],
    );
    let verified_attempt = trace.start_attempt();
    verified_attempt.record_started(&request);
    let verified_receipt = build_model_settings_receipt(&receipt_context, &request)?;
    verified_attempt.record_model_settings_receipt(&verified_receipt);
    verified_attempt.record_verified_backend_model(Some("verified-backend-model"));
    verified_attempt.record_completed("response-2", Some("upstream-2"), &None, &[]);
    drop(verified_attempt);
    drop(attempt);

    let legacy_trace = InferenceTraceContext::enabled(
        Arc::clone(&writer),
        "thread-root".into(),
        "turn-1".into(),
        "catalog-model".into(),
        "openai".into(),
    );
    let legacy_attempt = legacy_trace.start_attempt();
    legacy_attempt.record_started(&request);
    legacy_attempt.record_completed("response-legacy", None, &None, &[]);
    drop(legacy_attempt);
    drop(legacy_trace);
    drop(trace);
    drop(writer);

    let event_file = File::open(temp.path().join("trace.jsonl"))?;
    let events: Vec<RawTraceEvent> = BufReader::new(event_file)
        .lines()
        .map(|line| {
            let line = line?;
            Ok::<_, anyhow::Error>(serde_json::from_str::<RawTraceEvent>(&line)?)
        })
        .collect::<Result<_, _>>()?;
    let call_id = events
        .iter()
        .find_map(|event| match &event.payload {
            RawTraceEventPayload::InferenceStarted {
                inference_call_id, ..
            } => Some(inference_call_id.clone()),
            _ => None,
        })
        .expect("request start has its real call id");
    let (receipt_ref, receipt_call_id) = events
        .iter()
        .find_map(|event| match &event.payload {
            RawTraceEventPayload::ModelSettingsReceipt {
                inference_call_id,
                receipt_payload,
            } => Some((receipt_payload.clone(), inference_call_id.clone())),
            _ => None,
        })
        .expect("typed receipt event emitted");
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                &event.payload,
                RawTraceEventPayload::ModelSettingsReceipt { .. }
            ))
            .count(),
        2,
        "legacy trace attempt without context has no receipt event"
    );
    let response_ref = events
        .iter()
        .find_map(|event| match &event.payload {
            RawTraceEventPayload::InferenceCompleted {
                inference_call_id,
                response_payload,
                ..
            } if inference_call_id == &call_id => Some(response_payload.clone()),
            _ => None,
        })
        .expect("completed response payload exists");

    assert_eq!(receipt_call_id, call_id);
    assert_eq!(header_call_id, call_id);
    let replayed = replay_bundle(temp.path())?;
    assert!(
        replayed
            .raw_payloads
            .contains_key(&receipt_ref.raw_payload_id)
    );
    let receipt_json: serde_json::Value =
        serde_json::from_reader(File::open(temp.path().join(&receipt_ref.path))?)?;
    assert_eq!(receipt_json["call_ref"], call_id);
    assert_eq!(receipt_json["usage_ref"], response_ref.raw_payload_id);
    assert_eq!(receipt_json["effective_source"], "provider_request");
    assert_eq!(receipt_json["observed"], false);
    assert_eq!(
        receipt_json["model_identity"]["physical_backend"]["value"],
        serde_json::Value::Null
    );
    assert_eq!(
        receipt_json["model_identity"]["physical_backend"]["observed"],
        false
    );
    // The provider request was constructed independently from task intent.
    assert_eq!(receipt_json["wire_requested"]["model"], "requested-model");
    assert_eq!(receipt_json["effective"]["model"], "catalog-model");
    let response_json: serde_json::Value =
        serde_json::from_reader(File::open(temp.path().join(response_ref.path))?)?;
    assert_eq!(response_json["token_usage"]["total_tokens"], 14);
    let second_receipt_ref = events
        .iter()
        .filter_map(|event| match &event.payload {
            RawTraceEventPayload::ModelSettingsReceipt {
                receipt_payload, ..
            } => Some(receipt_payload),
            _ => None,
        })
        .nth(1)
        .expect("second call receipt exists");
    let second_receipt_json: serde_json::Value =
        serde_json::from_reader(File::open(temp.path().join(&second_receipt_ref.path))?)?;
    assert_eq!(
        second_receipt_json["model_identity"]["physical_backend"]["value"],
        "verified-backend-model"
    );
    assert_eq!(
        second_receipt_json["model_identity"]["physical_backend"]["source"],
        "verified_response_header"
    );
    assert_eq!(
        second_receipt_json["model_identity"]["physical_backend"]["observed"],
        true
    );
    Ok(())
}
