use super::*;
use crate::responses_metadata::CompactionTurnMetadata;
use crate::session::tests::make_session_and_context_with_auth_and_config_and_rx;
use codex_analytics::CompactionImplementation;
use codex_analytics::CompactionPhase;
use codex_analytics::CompactionReason;
use codex_analytics::CompactionTrigger;
use codex_login::CodexAuth;
use codex_model_provider_info::ModelProviderInfo;
use codex_protocol::openai_models::ReasoningEffort as ReasoningEffortConfig;
use core_test_support::responses;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::start_streaming_sse_server;
use std::time::Duration;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

const SNAPSHOT_HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const PRESENTATION_HASH: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn payload_with_two_candidates() -> Value {
    json!({
        "schema_version": 1,
        "snapshot_hash": SNAPSHOT_HASH,
        "presentation_hash": PRESENTATION_HASH,
        "sources": [
            {"id":"human-a","origin":"human","role":"user","text":"Old route.","protected":[],"has_opaque":false,"candidate":true},
            {"id":"human-b","origin":"human","role":"user","text":"Old deploy step.","protected":[],"has_opaque":false,"candidate":true}
        ],
        "completion_links": [
            {"instruction_id":"human-a","call_source_id":"call-a","output_source_id":"output-a","tool_call_id":"tool-a","call_text":"run old route","output_text":"Old route was checked.","terminal_status":"completed","terminal_exit_code":0,"candidate_only":true},
            {"instruction_id":"human-b","call_source_id":"call-b","output_source_id":"output-b","tool_call_id":"tool-b","call_text":"run old deploy","output_text":"Deploy is waiting on approval.","terminal_status":"failed","terminal_exit_code":1,"candidate_only":true}
        ]
    })
}

fn proposed_plan_json() -> String {
    json!({"operations":[{
        "action":"drop_superseded",
        "source":"human-a",
        "correction":"human-b",
        "correction_text":"Use the new route."
    }]})
    .to_string()
}

fn compaction_metadata(phase: CompactionPhase) -> CompactionTurnMetadata {
    CompactionTurnMetadata::new(
        CompactionTrigger::Manual,
        CompactionReason::UserRequested,
        CompactionImplementation::Responses,
        phase,
    )
}

async fn test_context(base_uri: String, fork_enabled: bool) -> (Arc<Session>, Arc<TurnContext>) {
    let provider = ModelProviderInfo::create_openai_provider(Some(format!("{base_uri}/v1")));
    let (session, turn, _events) = make_session_and_context_with_auth_and_config_and_rx(
        CodexAuth::create_dummy_chatgpt_auth_for_testing(),
        Vec::new(),
        move |config| {
            config.model = Some("gpt-5.2".to_string());
            config.model_provider = provider;
            config.model_provider.supports_websockets = false;
            config.model_reasoning_effort = Some(ReasoningEffortConfig::High);
            config.rencrow_compaction = fork_enabled;
        },
    )
    .await;
    (session, turn)
}

async fn serialized_history(session: &Session) -> Value {
    let history = session.clone_history().await;
    Value::Array(
        history
            .raw_items()
            .map(|item| serde_json::to_value(item).unwrap())
            .collect(),
    )
}

#[tokio::test]
async fn v2_selection_skips_request_when_candidates_are_absent() {
    let server = responses::start_mock_server().await;
    let (session, turn) = test_context(server.uri().to_string(), true).await;
    let mock = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_assistant_message("unused", "{}"),
            responses::ev_completed("unused"),
        ]),
    )
    .await;
    let cancellation = CancellationToken::new();

    let result = select_obsolete_instructions(
        &session,
        &turn,
        compaction_metadata(CompactionPhase::MidTurn),
        None,
        &mut Vec::new(),
        &cancellation,
    )
    .await
    .unwrap();

    assert!(result.is_none());
    assert!(mock.requests().is_empty());

    let mut protected = payload_with_two_candidates();
    for source in protected["sources"].as_array_mut().unwrap() {
        source["candidate"] = Value::Bool(false);
    }
    let protected_result = select_obsolete_instructions(
        &session,
        &turn,
        compaction_metadata(CompactionPhase::MidTurn),
        Some(protected),
        &mut Vec::new(),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(protected_result.is_none());
    assert!(mock.requests().is_empty());
}

#[tokio::test]
async fn v2_selection_rejects_missing_or_invalid_presentation_binding_before_request() {
    let server = responses::start_mock_server().await;
    let (session, turn) = test_context(server.uri().to_string(), true).await;
    let mock = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_assistant_message("unused", "{}"),
            responses::ev_completed("unused"),
        ]),
    )
    .await;
    let mut missing = payload_with_two_candidates();
    missing.as_object_mut().unwrap().remove("presentation_hash");
    let mut invalid = payload_with_two_candidates();
    invalid["presentation_hash"] = Value::String("invalid".into());
    for payload in [missing, invalid] {
        let result = select_obsolete_instructions(
            &session,
            &turn,
            compaction_metadata(CompactionPhase::MidTurn),
            Some(payload),
            &mut Vec::new(),
            &CancellationToken::new(),
        )
        .await;
        assert!(result.is_err());
    }
    assert!(mock.requests().is_empty());
}

#[tokio::test]
async fn v2_selection_sends_only_candidates_once_and_keeps_host_snapshot_binding() {
    let server = responses::start_mock_server().await;
    let (session, turn) = test_context(server.uri().to_string(), true).await;
    let mock = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_assistant_message("selection", &proposed_plan_json()),
            responses::ev_completed("selection-response"),
        ]),
    )
    .await;
    let original = payload_with_two_candidates();
    let cancellation = CancellationToken::new();
    let before = serialized_history(&session).await;
    let mut receipts = Vec::new();

    let selected = select_obsolete_instructions(
        &session,
        &turn,
        compaction_metadata(CompactionPhase::MidTurn),
        Some(original.clone()),
        &mut receipts,
        &cancellation,
    )
    .await
    .unwrap()
    .unwrap();

    assert_eq!(selected.snapshot_hash(), SNAPSHOT_HASH);
    assert_eq!(selected.presentation_hash(), PRESENTATION_HASH);
    assert_eq!(selected.proposed().operations.len(), 1);
    assert_eq!(mock.requests().len(), 1);
    assert_eq!(
        serde_json::to_value(&original).unwrap(),
        serde_json::to_value(payload_with_two_candidates()).unwrap()
    );
    assert_eq!(serialized_history(&session).await, before);
    let request = mock.single_request();
    let body = request.body_json();
    assert_eq!(body["model"], "gpt-5.2");
    assert_eq!(body["reasoning"]["effort"], "high");
    let user_text = request.message_input_texts("user").join("");
    let submitted: Value = serde_json::from_str(&user_text).unwrap();
    assert_eq!(submitted["sources"], original["sources"]);
    assert_eq!(submitted["completion_links"], original["completion_links"]);
    assert!(submitted.get("snapshot_hash").is_none());
    assert!(submitted.get("presentation_hash").is_none());
    assert!(submitted.get("current_context").is_none());
    let developer_text = request.message_input_texts("developer").join("");
    assert!(developer_text.contains("exact unique"));
    assert!(developer_text.contains("correction_text"));
    assert!(developer_text.contains("evidence"));
    assert!(developer_text.contains("output_source_id"));
    assert!(developer_text.contains("{\"operations\":"));
    assert!(!developer_text.contains(PLAN_PROMPT));
    let metadata: Value = serde_json::from_str(
        body["client_metadata"]["x-codex-turn-metadata"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(metadata["compaction"]["phase"], "mid_turn");
}

async fn assert_selection_rejected(events: Vec<Value>) {
    let server = responses::start_mock_server().await;
    let (session, turn) = test_context(server.uri().to_string(), true).await;
    session
        .record_conversation_items(
            &turn,
            turn.model_info(),
            &[responses::user_message_item("keep this conversation")],
        )
        .await;
    let mock = responses::mount_sse_once(&server, responses::sse(events)).await;
    let before = serialized_history(&session).await;
    let result = select_obsolete_instructions(
        &session,
        &turn,
        compaction_metadata(CompactionPhase::PreTurn),
        Some(payload_with_two_candidates()),
        &mut Vec::new(),
        &CancellationToken::new(),
    )
    .await;

    assert!(result.is_err());
    assert_eq!(serialized_history(&session).await, before);
    assert_eq!(mock.requests().len(), 1);
}

#[tokio::test]
async fn v2_selection_rejects_bad_json_missing_correction_text_and_tool_output() {
    assert_selection_rejected(vec![
        responses::ev_assistant_message("bad-json", "not json"),
        responses::ev_completed("bad-json-response"),
    ])
    .await;

    assert_selection_rejected(vec![
        responses::ev_assistant_message(
            "missing-correction",
            &json!({"operations":[{
                "action":"drop_superseded",
                "source":"human-a",
                "correction":"human-b"
            }]})
            .to_string(),
        ),
        responses::ev_completed("missing-correction-response"),
    ])
    .await;

    assert_selection_rejected(vec![
        responses::ev_assistant_message(
            "blank-correction",
            &json!({"operations":[{
                "action":"drop_superseded",
                "source":"human-a",
                "correction":"human-b",
                "correction_text":"   "
            }]})
            .to_string(),
        ),
        responses::ev_completed("blank-correction-response"),
    ])
    .await;

    assert_selection_rejected(vec![
        responses::ev_function_call("tool-output", "unexpected", "{}"),
        responses::ev_completed("tool-response"),
    ])
    .await;

    assert_selection_rejected(vec![
        json!({
            "type":"response.output_item.done",
            "item": {
                "type":"message",
                "role":"assistant",
                "id":"refusal",
                "content":[{"type":"refusal","refusal":"Cannot select instructions."}]
            }
        }),
        responses::ev_completed("refusal-response"),
    ])
    .await;
}

#[tokio::test]
async fn v2_selection_stream_failure_after_output_done_does_not_change_history() {
    let server = responses::start_mock_server().await;
    let (session, turn) = test_context(server.uri().to_string(), true).await;
    session
        .record_conversation_items(
            &turn,
            turn.model_info(),
            &[responses::user_message_item("keep this conversation")],
        )
        .await;
    let mock = responses::mount_sse_once(
        &server,
        responses::sse(vec![responses::ev_assistant_message("partial", "{}")]),
    )
    .await;
    let before = serialized_history(&session).await;
    let error = select_obsolete_instructions(
        &session,
        &turn,
        compaction_metadata(CompactionPhase::PreTurn),
        Some(payload_with_two_candidates()),
        &mut Vec::new(),
        &CancellationToken::new(),
    )
    .await
    .unwrap_err();

    assert!(error.to_string().contains("stream closed"));
    assert_eq!(serialized_history(&session).await, before);
    assert_eq!(mock.requests().len(), 1);
}

#[tokio::test]
async fn v2_selection_cancel_after_output_done_does_not_change_history() {
    let (gate_tx, gate_rx) = oneshot::channel();
    let (server, _completed) = start_streaming_sse_server(vec![vec![
        StreamingSseChunk {
            gate: None,
            body: responses::sse(vec![responses::ev_assistant_message("partial", "{}")]),
        },
        StreamingSseChunk {
            gate: Some(gate_rx),
            body: responses::sse(vec![responses::ev_completed("selection-response")]),
        },
    ]])
    .await;
    let (session, turn) = test_context(server.uri().to_string(), true).await;
    session
        .record_conversation_items(
            &turn,
            turn.model_info(),
            &[responses::user_message_item("keep this conversation")],
        )
        .await;
    let before = serialized_history(&session).await;
    let cancellation = CancellationToken::new();
    let task_cancel = cancellation.clone();
    let task_session = Arc::clone(&session);
    let task_turn = Arc::clone(&turn);
    let task = tokio::spawn(async move {
        select_obsolete_instructions(
            &task_session,
            &task_turn,
            compaction_metadata(CompactionPhase::PreTurn),
            Some(payload_with_two_candidates()),
            &mut Vec::new(),
            &task_cancel,
        )
        .await
    });
    server.wait_for_request_count(1).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    cancellation.cancel();
    let result = task.await.unwrap();
    let _ = gate_tx.send(());

    assert!(result.is_err());
    assert_eq!(serialized_history(&session).await, before);
    server.shutdown().await;
}

#[tokio::test]
async fn v2_staging_buffers_fork_output_for_every_phase_and_preserves_disabled_behavior() {
    for (fork_enabled, phase, expected_buffered) in [
        (true, CompactionPhase::PreTurn, true),
        (true, CompactionPhase::MidTurn, true),
        (false, CompactionPhase::PreTurn, false),
        (false, CompactionPhase::PostTurn, true),
    ] {
        let server = responses::start_mock_server().await;
        let (session, turn) = test_context(server.uri().to_string(), fork_enabled).await;
        let mock = responses::mount_sse_once(
            &server,
            responses::sse(vec![
                responses::ev_assistant_message("staged-output", "proposal"),
                responses::ev_completed_with_tokens("staging-response", 999),
            ]),
        )
        .await;
        let before = serialized_history(&session).await;
        let metadata = session
            .compaction_responses_metadata(&turn, compaction_metadata(phase))
            .await;
        let prompt = Prompt::default();
        let mut client = session.services.model_client.new_session();
        let response = super::super::drain_to_completed(
            &session,
            &turn,
            &mut client,
            &metadata,
            &prompt,
            phase,
        )
        .await
        .unwrap();

        assert_eq!(response.output.len(), usize::from(expected_buffered));
        assert_eq!(
            serialized_history(&session).await == before,
            expected_buffered
        );
        let token_info = session
            .token_usage_info()
            .await
            .expect("completed response records token usage");
        if fork_enabled {
            let history = session.clone_history().await;
            let base_instructions = session.get_base_instructions().await;
            let expected_active_tokens = history
                .estimate_token_count_with_base_instructions(&base_instructions)
                .expect("current history estimate")
                .max(0);
            assert_eq!(
                token_info.last_token_usage.total_tokens,
                expected_active_tokens
            );
        } else {
            assert_eq!(token_info.last_token_usage.total_tokens, 999);
        }
        assert_eq!(mock.requests().len(), 1);
    }
}
