// Modified by RenCrow Switch Core, 2026-09-22.
use super::*;
use crate::session::tests::make_session_and_context;
use codex_history::task_checkpoint_metadata::RenCrowTaskCheckpointMetadataV3;
use codex_history::task_checkpoint_metadata::TaskBoundarySourceFence;
use codex_history::task_checkpoint_metadata::TaskBoundarySourceRef;
use codex_history::task_context::AcceptedTaskReceipt;
use codex_history::task_context::BoundaryCandidate;
use codex_history::task_context::BoundaryMode;
use codex_history::task_context::StoredTaskState;
use codex_history::task_context::TaskState;
use codex_history::task_context::TaskStatus;
use std::collections::BTreeMap;

fn stored_task_state(thread_id: &str) -> StoredTaskState {
    let boundary = BoundaryCandidate {
        version: 1,
        mode: BoundaryMode::Task,
        thread_id: thread_id.into(),
        root_task_id: "root".into(),
        closed_task_id: "task-a".into(),
        revision: 2,
        closed_task_revision: 1,
        goal: "finish task A".into(),
        tasks: vec![TaskState {
            id: "task-a".into(),
            purpose: "implement".into(),
            status: TaskStatus::Completed,
            depends_on: vec![],
        }],
        current_task_id: None,
        artifact_ref: "artifact:a".into(),
        verification_ref: "check:a".into(),
        decisions: vec![],
        constraints: vec![],
        completion_state: None,
    };
    let candidate_hash = codex_history::archive_reference::content_sha256(
        &serde_json::to_string(&boundary).unwrap(),
    );
    let state = StoredTaskState {
        boundary,
        completed: BTreeMap::from([(
            "task-a".into(),
            AcceptedTaskReceipt {
                revision: 1,
                result: "done".into(),
                artifact_ref: "artifact:a".into(),
                verification_ref: "check:a".into(),
            },
        )]),
        parent_review_ref: "review:a".into(),
        previous_revision: None,
        previous_state_hash: None,
        candidate_hash,
        boundary_nonce: "nonce-a".into(),
    };
    state.validate(thread_id).unwrap();
    state
}

fn boundary_summary(thread_id: &str, history_hash: String, hash: String) -> ResponseItemEnvelope {
    let summary_text = "task boundary context";
    let mut envelope = ResponseItemEnvelope::new(ResponseItem::Message {
        id: None,
        role: "user".into(),
        content: vec![codex_protocol::models::ContentItem::InputText {
            text: summary_text.into(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: Some(
            codex_protocol::models::InternalChatMessageMetadataPassthrough {
                content_item_kinds: Some(vec![codex_protocol::models::ContentItemKind(
                    "compaction.summary".into(),
                )]),
                ..Default::default()
            },
        ),
    });
    let checkpoint = RenCrowTaskCheckpointMetadataV3::task_boundary(
        summary_text,
        serde_json::to_value(stored_task_state(thread_id)).unwrap(),
        TaskBoundarySourceFence {
            history_hash,
            removed_refs: vec![TaskBoundarySourceRef {
                id: "item:0".into(),
                hash,
            }],
        },
    );
    envelope.metadata.get_or_insert_default().rencrow_compaction =
        Some(serde_json::to_value(checkpoint).unwrap());
    envelope
}

#[test]
fn task_boundary_source_fence_binds_exact_snapshot_hash_and_removed_items() {
    let source = ResponseItemEnvelope::new(ResponseItem::Message {
        id: None,
        role: "assistant".into(),
        content: vec![codex_protocol::models::ContentItem::InputText {
            text: "task A work".into(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    });
    let thread_id = "thread-source-fence";
    let originals = vec![source.clone()];
    let history_hash = history_digest(&originals).unwrap();
    let source_hash = digest(&(&source.item, &source.metadata)).unwrap();
    let summary = boundary_summary(thread_id, history_hash.clone(), source_hash);
    let summary_text = "task boundary context";
    assert!(
        validate_task_boundary_source_fence(
            &originals,
            &[summary.clone()],
            &history_hash,
            thread_id,
            summary_text,
        )
        .is_ok()
    );

    let mut changed = summary.clone();
    let metadata = changed
        .metadata
        .as_mut()
        .unwrap()
        .rencrow_compaction
        .as_mut()
        .unwrap();
    metadata["source_fence"]["history_hash"] = "f".repeat(64).into();
    assert!(
        validate_task_boundary_source_fence(
            &originals,
            &[changed],
            &history_hash,
            thread_id,
            summary_text,
        )
        .is_err()
    );

    assert!(
        validate_task_boundary_source_fence(
            &originals,
            &[source, summary],
            &history_hash,
            thread_id,
            summary_text,
        )
        .is_err()
    );
}

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
    let result = session
        .commit_rencrow_checkpoint(RenCrowCheckpoint {
            items: vec![],
            expected_history_hash: "not-the-current-history".into(),
            expected_settings: session.thread_settings_snapshot().await,
            reference_context: None,
            world_state: None,
            summary: "unused".into(),
            response_id: Some("unused".into()),
            expected_turn: "unused".into(),
            expected_base_text: session.get_base_instructions().await.text,
            expected_world: before.world_state_checkpoint(),
        })
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
    let result = session
        .commit_rencrow_checkpoint(RenCrowCheckpoint {
            items: vec![],
            expected_history_hash: history_digest(before.annotated_items()).unwrap(),
            expected_settings: session.thread_settings_snapshot().await,
            reference_context: None,
            world_state: None,
            summary: "unused".into(),
            response_id: Some("unused".into()),
            expected_turn: "unused".into(),
            expected_base_text: session.get_base_instructions().await.text,
            expected_world: before.world_state_checkpoint(),
        })
        .await;
    assert!(matches!(
        result.unwrap_err().details(),
        codex_protocol::error::CodexErrorDetails::TurnAborted
    ));
    assert!(!session.state.lock().await.rencrow_checkpoint_failed);
    {
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
    }
    assert_eq!(
        history_digest(before.annotated_items()).unwrap(),
        history_digest(session.clone_history().await.annotated_items()).unwrap()
    );
}
