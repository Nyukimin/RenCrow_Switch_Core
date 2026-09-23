// Modified by RenCrow Switch Core, 2026-09-22.
use super::*;
use crate::session::tests::make_session_and_context;

#[tokio::test]
async fn stale_history_rejects_before_persistence_or_replacement() {
    let (session, _) = make_session_and_context().await;
    let (window_number, window_ids) = {
        let mut state = session.state.lock().await;
        state.request_new_context_window();
        assert!(state.claim_token_budget_reminder());
        assert!(state.claim_auto_compact_fallback());
        state.set_auto_compact_window_estimated_prefill(123);
        (
            state.auto_compact_window_number(),
            state.auto_compact_window_ids(),
        )
    };
    let before = session.clone_history().await;
    let (activity, _) = session.input_queue.subscribe_activity(None).await;
    let result = session
        .commit_rencrow_checkpoint(
            RenCrowCheckpoint {
                items: vec![],
                expected_history_hash: "not-the-current-history".into(),
                expected_settings: session.thread_settings_snapshot().await,
                reference_context: None,
                world_state: None,
                summary: "unused".into(),
                response_id: "unused".into(),
                expected_turn: "unused".into(),
                expected_base_text: session.get_base_instructions().await.text,
                expected_world: before.world_state_checkpoint(),
            },
            &activity,
        )
        .await;
    assert!(result.unwrap_err().to_string().contains("stale"));
    assert_eq!(
        history_digest(before.annotated_items()).unwrap(),
        history_digest(session.clone_history().await.annotated_items()).unwrap()
    );
    let mut state = session.state.lock().await;
    assert!(!state.rencrow_checkpoint_failed);
    assert_eq!(state.auto_compact_window_number(), window_number);
    assert_eq!(state.auto_compact_window_ids(), window_ids);
    assert_eq!(
        state.auto_compact_window_snapshot().prefill_input_tokens,
        Some(123)
    );
    assert!(state.take_new_context_window_request());
    assert!(!state.claim_token_budget_reminder());
    assert!(!state.claim_auto_compact_fallback());
    drop(state);
}

#[tokio::test]
async fn uncertain_checkpoint_prevents_subsequent_sampling() {
    let (session, _) = make_session_and_context().await;
    session.state.lock().await.rencrow_checkpoint_failed = true;
    assert!(
        session
            .check_rencrow_checkpoint()
            .await
            .unwrap_err()
            .to_string()
            .contains("restart")
    );
}

#[tokio::test]
async fn missing_active_task_rejects_before_persistence() {
    let (session, _) = make_session_and_context().await;
    let (window_number, window_ids) = {
        let mut state = session.state.lock().await;
        state.request_new_context_window();
        assert!(state.claim_token_budget_reminder());
        assert!(state.claim_auto_compact_fallback());
        state.ensure_auto_compact_window_server_prefill_from_usage(
            &codex_protocol::protocol::TokenUsage {
                input_tokens: 321,
                total_tokens: 321,
                ..Default::default()
            },
        );
        (
            state.auto_compact_window_number(),
            state.auto_compact_window_ids(),
        )
    };
    let before = session.clone_history().await;
    let (activity, _) = session.input_queue.subscribe_activity(None).await;
    let result = session
        .commit_rencrow_checkpoint(
            RenCrowCheckpoint {
                items: vec![],
                expected_history_hash: history_digest(before.annotated_items()).unwrap(),
                expected_settings: session.thread_settings_snapshot().await,
                reference_context: None,
                world_state: None,
                summary: "unused".into(),
                response_id: "unused".into(),
                expected_turn: "unused".into(),
                expected_base_text: session.get_base_instructions().await.text,
                expected_world: before.world_state_checkpoint(),
            },
            &activity,
        )
        .await;
    assert!(matches!(
        result.unwrap_err().details(),
        codex_protocol::error::CodexErrorDetails::TurnAborted
    ));
    assert!(!session.state.lock().await.rencrow_checkpoint_failed);
    let mut state = session.state.lock().await;
    assert_eq!(state.auto_compact_window_number(), window_number);
    assert_eq!(state.auto_compact_window_ids(), window_ids);
    assert_eq!(
        state.auto_compact_window_snapshot().prefill_input_tokens,
        Some(321)
    );
    assert!(state.take_new_context_window_request());
    assert!(!state.claim_token_budget_reminder());
    assert!(!state.claim_auto_compact_fallback());
    drop(state);
    assert_eq!(
        history_digest(before.annotated_items()).unwrap(),
        history_digest(session.clone_history().await.annotated_items()).unwrap()
    );
}
