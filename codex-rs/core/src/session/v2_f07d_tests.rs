use super::*;
use crate::compact::SUMMARY_PREFIX;
use crate::context::CompactionSummary;
use crate::context::ContextualUserFragment;
use crate::state::AutoCompactWindowIds;
use codex_history::CompactedItem;
use codex_history::compaction_transaction;
use codex_protocol::ResponseItemId;
use codex_protocol::SessionId;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::protocol::ThreadSettingsAppliedEvent;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TokenUsageInfo;
use codex_protocol::protocol::TokenUsageRecord;
use serde_json::json;
use uuid::Uuid;

#[tokio::test]
async fn adoption_resets_dirty_window_state_and_server_prefill() {
    assert_adoption_resets_dirty_window_state(/*server_prefill*/ true).await;
}

#[tokio::test]
async fn adoption_resets_dirty_window_state_and_estimated_prefill() {
    assert_adoption_resets_dirty_window_state(/*server_prefill*/ false).await;
}

async fn assert_adoption_resets_dirty_window_state(server_prefill: bool) {
    let (session, _) = make_session_and_context().await;
    let old_ids = AutoCompactWindowIds {
        first_window_id: Uuid::now_v7(),
        previous_window_id: Some(Uuid::now_v7()),
        window_id: Uuid::now_v7(),
    };
    let adopted_ids = AutoCompactWindowIds {
        first_window_id: old_ids.first_window_id,
        previous_window_id: Some(old_ids.window_id),
        window_id: Uuid::now_v7(),
    };
    let adopted_number = 12;
    {
        let mut state = session.state.lock().await;
        state.restore_auto_compact_window(7, old_ids);
        state.request_new_context_window();
        std::assert!(state.claim_token_budget_reminder());
        std::assert!(state.claim_auto_compact_fallback());
        if server_prefill {
            state.ensure_auto_compact_window_server_prefill_from_usage(&TokenUsage {
                input_tokens: 240,
                total_tokens: 260,
                ..TokenUsage::default()
            });
        } else {
            state.set_auto_compact_window_estimated_prefill(240);
        }
        std::assert_eq!(
            state.auto_compact_window_snapshot().prefill_input_tokens,
            Some(240)
        );

        state.adopt_rencrow_compaction_window(adopted_number, adopted_ids);

        std::assert_eq!(state.auto_compact_window_number(), adopted_number);
        std::assert_eq!(state.auto_compact_window_ids(), adopted_ids);
        std::assert_eq!(
            state.auto_compact_window_snapshot().prefill_input_tokens,
            None
        );
        std::assert!(!state.take_new_context_window_request());
        std::assert!(state.claim_token_budget_reminder());
        std::assert!(state.claim_auto_compact_fallback());
    }
}

#[tokio::test]
async fn resumed_checkpoint_recomputes_active_usage_without_changing_billing_receipts() {
    assert_checkpoint_startup_usage(/*forked*/ false, /*include_old_token_count*/ true).await;
}

#[tokio::test]
async fn forked_checkpoint_recomputes_active_usage_without_changing_billing_receipts() {
    assert_checkpoint_startup_usage(/*forked*/ true, /*include_old_token_count*/ true).await;
}

#[tokio::test]
async fn resume_without_token_count_does_not_infer_cumulative_usage_from_receipt() {
    assert_checkpoint_startup_usage(
        /*forked*/ false, /*include_old_token_count*/ false,
    )
    .await;
}

#[tokio::test]
async fn resumed_checkpoint_with_native_pair_does_not_double_count_observation() {
    assert_checkpoint_startup_usage_with_history(
        /*forked*/ false,
        /*include_old_token_count*/ true,
        native_pair_and_summary_history(),
    )
    .await;
}

#[tokio::test]
async fn recomputed_usage_counts_native_pair_and_summary_once() {
    let (session, turn_context, _rx) = make_session_and_context_with_auth_and_config_and_rx(
        CodexAuth::from_api_key("Test API Key"),
        Vec::new(),
        |config| {
            config.rencrow_compaction = true;
            config.model_auto_compact_token_limit_scope =
                codex_protocol::config_types::AutoCompactTokenLimitScope::BodyAfterPrefix;
            config.model_auto_compact_token_limit = Some(1);
        },
    )
    .await;
    session
        .state
        .lock()
        .await
        .replace_history(native_pair_and_summary_history(), None);

    session.recompute_token_usage(&turn_context).await;

    let history = session.clone_history().await;
    let expected_active_tokens = history
        .estimate_token_count_with_base_instructions(&session.get_base_instructions().await)
        .expect("whole history should have a stable estimate")
        .max(0);
    let context_status =
        crate::session::context_window::context_window_token_status(&session, &turn_context).await;
    std::assert_eq!(
        session.get_total_token_usage().await,
        expected_active_tokens
    );
    std::assert_eq!(context_status.active_context_tokens, expected_active_tokens);
    std::assert_eq!(context_status.auto_compact_scope_tokens, 0);
    std::assert_eq!(
        context_status.auto_compact_window_prefill_tokens,
        Some(expected_active_tokens)
    );
}

#[tokio::test]
async fn estimated_usage_tracks_local_suffix_then_normal_usage_returns_to_recorded_semantics() {
    let (session, turn_context, _rx) = make_session_and_context_with_auth_and_config_and_rx(
        CodexAuth::from_api_key("Test API Key"),
        Vec::new(),
        |config| config.rencrow_compaction = true,
    )
    .await;
    session
        .state
        .lock()
        .await
        .replace_history(native_pair_and_summary_history(), None);
    session.recompute_token_usage(&turn_context).await;

    session
        .record_conversation_items(
            &turn_context,
            turn_context.model_info(),
            &[user_message("local item after the model response")],
        )
        .await;
    let history = session.clone_history().await;
    let whole_history_tokens = history
        .estimate_token_count_with_base_instructions(&session.get_base_instructions().await)
        .expect("whole history estimate after local append")
        .max(0);
    std::assert_eq!(session.get_total_token_usage().await, whole_history_tokens);

    session.set_server_reasoning_included(true).await;
    let normal_usage = TokenUsage {
        input_tokens: 71,
        output_tokens: 12,
        total_tokens: 83,
        ..TokenUsage::default()
    };
    session
        .record_token_usage_info(
            &turn_context,
            &turn_context.initial_settings,
            Some(&normal_usage),
        )
        .await
        .expect("normal usage should restore the recorded baseline");
    let history = session.clone_history().await;
    let expected_recorded_total = normal_usage
        .total_tokens
        .saturating_add(history.estimated_tokens_after_last_model_generated_item());
    std::assert_eq!(
        session.get_total_token_usage().await,
        expected_recorded_total
    );
}

#[tokio::test]
async fn stale_auxiliary_usage_guard_preserves_usage_source_after_same_value_write() {
    let (session, turn_context, _rx) = make_session_and_context_with_auth_and_config_and_rx(
        CodexAuth::from_api_key("Test API Key"),
        Vec::new(),
        |config| config.rencrow_compaction = true,
    )
    .await;
    session
        .state
        .lock()
        .await
        .replace_history(native_pair_and_summary_history(), None);
    session.recompute_token_usage(&turn_context).await;
    session.set_server_reasoning_included(true).await;

    let previous_info = session
        .token_usage_info()
        .await
        .expect("estimated active token info");
    let guard = session.capture_compaction_usage_guard(&turn_context).await;
    {
        let mut state = session.state.lock().await;
        // The values stay the same, but this explicit setter changes provenance back to the
        // ordinary recorded-usage baseline once the usage source is tracked.
        state.set_token_info(Some(previous_info.clone()));
    }

    let auxiliary_usage = TokenUsage {
        input_tokens: 9,
        output_tokens: 4,
        total_tokens: 13,
        ..TokenUsage::default()
    };
    session
        .update_compaction_aux_token_usage_info(
            &turn_context,
            &turn_context.initial_settings,
            Some(&auxiliary_usage),
            &guard,
        )
        .await
        .expect("stale auxiliary usage is still billed");

    let after_aux = session
        .token_usage_info()
        .await
        .expect("active token info remains present");
    let mut expected_total = previous_info.total_token_usage.clone();
    expected_total.add_assign(&auxiliary_usage);
    std::assert_eq!(after_aux.total_token_usage, expected_total);
    std::assert_eq!(after_aux.last_token_usage, previous_info.last_token_usage);
    let history = session.clone_history().await;
    let expected_recorded_active = previous_info
        .last_token_usage
        .total_tokens
        .saturating_add(history.estimated_tokens_after_last_model_generated_item());
    std::assert_eq!(
        session.get_total_token_usage().await,
        expected_recorded_active
    );
}

#[tokio::test]
async fn normal_completion_without_usage_keeps_estimate_and_invalidates_auxiliary_guard() {
    let (session, turn_context, _rx) = make_session_and_context_with_auth_and_config_and_rx(
        CodexAuth::from_api_key("Test API Key"),
        Vec::new(),
        |config| config.rencrow_compaction = true,
    )
    .await;
    session
        .state
        .lock()
        .await
        .replace_history(native_pair_and_summary_history(), None);
    session.recompute_token_usage(&turn_context).await;
    session.set_server_reasoning_included(true).await;
    let history = session.clone_history().await;
    let expected_active_tokens = history
        .estimate_token_count_with_base_instructions(&session.get_base_instructions().await)
        .expect("whole history estimate")
        .max(0);
    let generation_before = session.state.lock().await.active_usage_generation;
    let guard = session.capture_compaction_usage_guard(&turn_context).await;

    session
        .record_token_usage_info(&turn_context, &turn_context.initial_settings, None)
        .await
        .expect("normal completion without usage should succeed");
    std::assert!(
        session.state.lock().await.active_usage_generation > generation_before,
        "a normal completion must invalidate older auxiliary usage guards"
    );
    std::assert_eq!(
        session.get_total_token_usage().await,
        expected_active_tokens
    );

    session
        .update_compaction_aux_token_usage_info(
            &turn_context,
            &turn_context.initial_settings,
            Some(&TokenUsage {
                input_tokens: 8,
                output_tokens: 3,
                total_tokens: 11,
                ..TokenUsage::default()
            }),
            &guard,
        )
        .await
        .expect("stale auxiliary usage remains billable");
    std::assert_eq!(
        session.get_total_token_usage().await,
        expected_active_tokens
    );
}

#[tokio::test]
async fn full_context_signal_restores_recorded_usage_source() {
    let (session, turn_context, _rx) = make_session_and_context_with_auth_and_config_and_rx(
        CodexAuth::from_api_key("Test API Key"),
        Vec::new(),
        |config| config.rencrow_compaction = true,
    )
    .await;
    session
        .state
        .lock()
        .await
        .replace_history(native_pair_and_summary_history(), None);
    session.recompute_token_usage(&turn_context).await;
    let context_window = turn_context
        .model_context_window()
        .expect("test model context window");

    session.set_total_tokens_full(&turn_context).await;

    let status =
        crate::session::context_window::context_window_token_status(&session, &turn_context).await;
    std::assert!(status.active_context_tokens >= context_window);
}

#[tokio::test]
async fn forked_recomputed_token_count_is_persisted_after_copied_token_count() {
    let (mut session, _turn_context, _rx) = make_session_and_context_with_auth_and_config_and_rx(
        CodexAuth::from_api_key("Test API Key"),
        Vec::new(),
        |config| config.rencrow_compaction = true,
    )
    .await;
    let rollout_path = attach_thread_persistence(
        Arc::get_mut(&mut session).expect("test session is uniquely owned"),
    )
    .await;
    let thread_id = session.thread_id();
    let record = TokenUsageRecord {
        thread_id,
        turn_id: "prior-turn".into(),
        session_id: SessionId::from(thread_id),
        root_turn_id: "prior-turn".into(),
        response_id: "actual-model-response".into(),
        usage: TokenUsage::default(),
        turn_token_usage: TokenUsage::default(),
        thread_token_usage: TokenUsage::default(),
    };
    let old_info = TokenUsageInfo {
        total_token_usage: TokenUsage::default(),
        last_token_usage: TokenUsage {
            total_tokens: 1_000_000,
            ..TokenUsage::default()
        },
        model_context_window: Some(128_000),
    };
    let mut rollout_items = vec![RolloutItem::EventMsg(EventMsg::TokenCount(
        TokenCountEvent {
            info: Some(old_info),
            rate_limits: None,
        },
    ))];
    rollout_items.extend(
        committed_v2_checkpoint_with_history(&session, &record, native_pair_and_summary_history())
            .await,
    );

    session
        .record_initial_history(InitialHistory::Forked(rollout_items))
        .await;
    session.flush_rollout().await.expect("forked rollout flush");
    let InitialHistory::Resumed(resumed) = RolloutRecorder::get_rollout_history(&rollout_path)
        .await
        .expect("read persisted fork history")
    else {
        panic!("expected persisted fork history");
    };
    let latest_persisted_usage = resumed.history.iter().rev().find_map(|item| match item {
        RolloutItem::EventMsg(EventMsg::TokenCount(event)) => event.info.clone(),
        _ => None,
    });
    let current_usage = session
        .token_usage_info()
        .await
        .expect("startup should recompute active usage");
    std::assert_eq!(latest_persisted_usage.as_ref(), Some(&current_usage));
}

async fn assert_checkpoint_startup_usage(forked: bool, include_old_token_count: bool) {
    assert_checkpoint_startup_usage_with_history(forked, include_old_token_count, Vec::new()).await;
}

async fn assert_checkpoint_startup_usage_with_history(
    forked: bool,
    include_old_token_count: bool,
    replacement_history: Vec<ResponseItem>,
) {
    let (session, turn_context, _rx) = make_session_and_context_with_auth_and_config_and_rx(
        CodexAuth::from_api_key("Test API Key"),
        Vec::new(),
        |config| {
            config.rencrow_compaction = true;
            config.model_auto_compact_token_limit_scope =
                codex_protocol::config_types::AutoCompactTokenLimitScope::BodyAfterPrefix;
            config.model_auto_compact_token_limit = Some(1);
        },
    )
    .await;
    let thread_id = session.thread_id();
    let record = TokenUsageRecord {
        thread_id,
        turn_id: "prior-turn".into(),
        session_id: SessionId::from(thread_id),
        root_turn_id: "prior-turn".into(),
        response_id: "actual-model-response".into(),
        usage: TokenUsage {
            input_tokens: 70,
            output_tokens: 30,
            total_tokens: 100,
            ..TokenUsage::default()
        },
        turn_token_usage: TokenUsage::default(),
        thread_token_usage: TokenUsage::default(),
    };
    let old_info = TokenUsageInfo {
        total_token_usage: TokenUsage {
            input_tokens: 4_000,
            output_tokens: 2_000,
            total_tokens: 6_000,
            ..TokenUsage::default()
        },
        last_token_usage: TokenUsage {
            input_tokens: 1_000_000,
            output_tokens: 300,
            total_tokens: 1_000_000,
            ..TokenUsage::default()
        },
        model_context_window: Some(128_000),
    };
    let mut rollout_items = Vec::new();
    if include_old_token_count {
        rollout_items.push(RolloutItem::EventMsg(EventMsg::TokenCount(
            TokenCountEvent {
                info: Some(old_info.clone()),
                rate_limits: None,
            },
        )));
    }
    rollout_items
        .extend(committed_v2_checkpoint_with_history(&session, &record, replacement_history).await);

    if forked {
        session
            .record_initial_history(InitialHistory::Forked(rollout_items))
            .await;
    } else {
        session
            .record_initial_history(InitialHistory::Resumed(ResumedHistory {
                conversation_id: thread_id,
                history: Arc::new(rollout_items),
                rollout_path: None,
            }))
            .await;
    }

    let history = session.clone_history().await;
    let expected_active_tokens = history
        .estimate_token_count_with_base_instructions(&session.get_base_instructions().await)
        .expect("resume history should have a stable token estimate")
        .max(0);
    let context_status = crate::session::context_window::context_window_token_status(
        session.as_ref(),
        turn_context.as_ref(),
    )
    .await;
    std::assert_eq!(context_status.active_context_tokens, expected_active_tokens);
    std::assert_eq!(context_status.auto_compact_scope_tokens, 0);
    std::assert_eq!(
        context_status.auto_compact_window_prefill_tokens,
        Some(expected_active_tokens)
    );
    std::assert!(!context_status.token_limit_reached);
    std::assert!(!context_status.full_context_window_limit_reached);
    let state = session.state.lock().await;
    let usage = state
        .token_info()
        .expect("active token info should be restored");
    std::assert_eq!(usage.last_token_usage.total_tokens, expected_active_tokens);
    if include_old_token_count {
        std::assert_eq!(usage.total_token_usage, old_info.total_token_usage);
    } else {
        std::assert_eq!(usage.total_token_usage, TokenUsage::default());
    }
    std::assert_eq!(state.latest_token_usage_record.as_ref(), Some(&record));
    std::assert_ne!(
        usage.total_token_usage.total_tokens,
        record.usage.total_tokens
    );
    drop(state);
}

async fn committed_v2_checkpoint_with_history(
    session: &Session,
    record: &TokenUsageRecord,
    preceding_items: Vec<ResponseItem>,
) -> Vec<RolloutItem> {
    let summary_text = format!("{SUMMARY_PREFIX}\nRecovered active context after compaction.");
    let mut summary = ResponseItemEnvelope::new(ContextualUserFragment::into(
        CompactionSummary::new(&summary_text),
    ));
    summary.metadata.get_or_insert_default().rencrow_compaction = Some(json!({
        "version": 2,
        "snapshot_hash": "a".repeat(64),
        "summary_hash": codex_history::archive_reference::content_sha256(&summary_text),
        "semantic_summary_hash": codex_history::archive_reference::content_sha256(&summary_text),
        "selection_mode": "no_candidates",
        "applied_refs": [],
        "results": [],
        "observations": [],
        "summary_covered_observations": [],
        "important_refs": [],
        "model": "test-model",
        "responses": [{
            "stage": "summary",
            "response_id": "summary-response",
            "seconds": 0.1,
            "usage": null
        }],
        "transaction_following_items": 1
    }));
    let window_id = Uuid::now_v7();
    let window_ids = AutoCompactWindowIds {
        first_window_id: window_id,
        previous_window_id: None,
        window_id,
    };
    let mut replacement_history = preceding_items
        .into_iter()
        .map(ResponseItemEnvelope::new)
        .collect::<Vec<_>>();
    replacement_history.push(summary);
    let mut transaction = vec![
        RolloutItem::Compacted(CompactedItem {
            message: summary_text,
            replacement_history: Some(replacement_history),
            retained_context: None,
            guardian_history: None,
            mcp_resource_origins: None,
            window_number: Some(1),
            first_window_id: Some(window_ids.first_window_id.to_string()),
            previous_window_id: None,
            window_id: Some(window_ids.window_id.to_string()),
            compaction_response_id: Some("summary-response".into()),
            latest_token_usage_record: Some(record.clone()),
        }),
        RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(
            ThreadSettingsAppliedEvent {
                thread_id: Some(session.thread_id()),
                thread_settings: session.thread_settings_snapshot().await,
            },
        )),
    ];
    let checkpoint_hash = compaction_transaction::transaction_hash(&transaction)
        .expect("checkpoint transaction should serialize");
    transaction.push(RolloutItem::RenCrowCompactionCommit { checkpoint_hash });
    let committed = compaction_transaction::committed_items(&transaction)
        .expect("V2 checkpoint transaction should be durable");
    let RolloutItem::Compacted(compacted) = committed
        .iter()
        .find(|item| matches!(item, RolloutItem::Compacted(_)))
        .expect("committed compacted row")
    else {
        unreachable!();
    };
    let envelope = compacted
        .replacement_history
        .as_ref()
        .and_then(|items| items.last())
        .expect("committed checkpoint summary");
    let summary_metadata = envelope
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.rencrow_compaction.clone())
        .expect("committed V2 metadata");
    codex_history::RenCrowCompactionMetadataV2::parse_and_validate(
        summary_metadata,
        &session.thread_id().to_string(),
        match &envelope.item {
            ResponseItem::Message { content, .. } => match content.as_slice() {
                [ContentItem::InputText { text }] => text,
                _ => panic!("V2 summary has one text part"),
            },
            _ => panic!("V2 summary is a message"),
        },
    )
    .expect("checkpoint metadata is valid after commit normalization");
    committed
}

fn native_pair_and_summary_history() -> Vec<ResponseItem> {
    vec![
        ResponseItem::Reasoning {
            id: None,
            summary: Vec::new(),
            content: None,
            encrypted_content: Some("encrypted-reasoning-".repeat(100)),
            internal_chat_message_metadata_passthrough: None,
        },
        user_message("real user boundary before the retained native call"),
        ResponseItem::FunctionCall {
            id: Some(ResponseItemId::with_suffix("fc", "usage-source")),
            name: "read_file".to_string(),
            namespace: None,
            arguments: r#"{"path":"large-output.txt"}"#.to_string(),
            encrypted_function_args: None,
            call_id: "usage-source-call".to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::FunctionCallOutput {
            id: Some(ResponseItemId::with_suffix("fco", "usage-source")),
            call_id: Some("usage-source-call".to_string()),
            name: Some("read_file".to_string()),
            namespace: None,
            output: FunctionCallOutputPayload::from_text("tool output body".to_string()),
            internal_chat_message_metadata_passthrough: None,
        },
        ContextualUserFragment::into(CompactionSummary::new(format!(
            "{SUMMARY_PREFIX}\nNative call and output are retained by the checkpoint."
        ))),
    ]
}
