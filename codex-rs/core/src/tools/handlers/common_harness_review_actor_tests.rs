// Modified by RenCrow Switch Core, 2026-11-27: keep review output separate from parent work control.
use super::common_task;
use crate::session::common_harness_completion::CommonHarnessTurnEndDecision;
use codex_history::task_context::TaskRunStatusV1;
use pretty_assertions::assert_eq;
use std::sync::Arc;

async fn restore_external_execution_owner(session: &crate::session::session::Session) {
    let mut completion = session
        .rencrow_completion_state("harness-root")
        .await
        .unwrap();
    completion
        .common_harness
        .as_mut()
        .unwrap()
        .request
        .execution_owner = codex_history::task_context::ExecutionOwnerV1::Shiro;
    session
        .set_accepted_rencrow_completion_state("harness-root".into(), completion)
        .await
        .unwrap();
}

#[tokio::test]
async fn common_harness_review_actor_finishes_without_resuming_parent_workflow() {
    let (_workspace, session, turn) = common_task().await;
    let before = session
        .current_common_harness_state("harness-root")
        .await
        .unwrap();
    {
        let mut active = session.active_turn.lock().await;
        let task = active.as_mut().unwrap().task.as_mut().unwrap();
        assert_eq!(task.turn_context.sub_id, turn.sub_id);
        task.kind = crate::state::TaskKind::Review;
        task.task = Arc::new(crate::tasks::ReviewTask::new());
    }
    let decision = session.common_harness_turn_end_gate(&turn).await.unwrap();
    assert!(
        matches!(
            decision,
            CommonHarnessTurnEndDecision::Stopped {
                status: TaskRunStatusV1::Queued,
                ..
            }
        ),
        "review output must end this review turn instead of starting parent coding work: {decision:?}"
    );
    assert_eq!(
        session
            .current_common_harness_state("harness-root")
            .await
            .unwrap(),
        before
    );
}

#[tokio::test]
async fn common_harness_blocked_result_preserves_unmet_conditions_and_resume_reason() {
    let (_workspace, session, turn) = common_task().await;
    let before = session
        .current_common_harness_state("harness-root")
        .await
        .unwrap();
    let blocked = session
        .apply_common_harness_host_event(
            "harness-root",
            codex_history::task_context::StateFenceV1 {
                request_revision: before.request.request_revision,
                expected_state_version: before.run_state.state_version,
                owner_epoch: before.run_state.owner_epoch,
            },
            codex_history::task_context::HostEventV1::Block {
                reason_code: "verifier_unavailable".into(),
                resume_condition: "restore_registered_verifier".into(),
            },
        )
        .await
        .unwrap();
    let result = session
        .common_harness_terminal_result(&turn)
        .await
        .unwrap()
        .expect("blocked tasks must return a host-authored result packet");
    let packet: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(packet["final_state"], "blocked");
    assert_eq!(packet["task_id"], blocked.request.task_id);
    assert_eq!(packet["reason_code"], "verifier_unavailable");
    assert_eq!(packet["resume_condition"], "restore_registered_verifier");
    assert_eq!(
        packet["remaining"],
        serde_json::to_value(&blocked.run_state.unmet_conditions).unwrap()
    );
    let emitted = session
        .emit_common_harness_terminal_result(&turn, turn.model_info().as_ref())
        .await
        .unwrap()
        .expect("the stopped task result must reach the normal response sink");
    assert_eq!(emitted, result);
    let history = session.clone_history().await;
    assert!(history.raw_items().any(|item| matches!(
        item,
        codex_protocol::models::ResponseItem::Message { role, content, .. }
            if role == "assistant" && content.iter().any(|content| matches!(
                content, codex_protocol::models::ContentItem::OutputText { text } if text == &result
            ))
    )), "a terminal projection must be recorded, not merely returned to the controller");
    assert_eq!(
        session
            .current_common_harness_state("harness-root")
            .await
            .unwrap(),
        blocked,
        "result projection cannot change the task contract or consume an attempt"
    );
}

// Additional regressions for the next source gate, not yet adopted.
#[tokio::test]
async fn common_harness_review_inference_does_not_start_parent_attempt_or_deadline() {
    let (_workspace, session, turn) = common_task().await;
    let before = session
        .current_common_harness_state("harness-root")
        .await
        .unwrap();
    {
        let mut active = session.active_turn.lock().await;
        let task = active.as_mut().unwrap().task.as_mut().unwrap();
        task.kind = crate::state::TaskKind::Review;
        task.task = Arc::new(crate::tasks::ReviewTask::new());
    }
    assert!(
        session
            .common_harness_inference_deadline(&turn)
            .await
            .unwrap()
            .is_none(),
        "review inference cannot reserve the parent work budget or start its attempt"
    );
    assert_eq!(
        session
            .current_common_harness_state("harness-root")
            .await
            .unwrap(),
        before
    );
}

#[tokio::test]
async fn common_harness_external_owner_cannot_enter_local_control_loop() {
    let (_workspace, session, turn) = common_task().await;
    // Represents a restored shared contract; S1 admission itself already rejects Shiro.
    restore_external_execution_owner(&session).await;
    let before = session
        .current_common_harness_state("harness-root")
        .await
        .unwrap();
    let result = session.common_harness_turn_end_gate(&turn).await;
    assert!(
        result.is_err(),
        "S1 cannot start or recover external-owned work: {result:?}"
    );
    assert_eq!(
        session
            .current_common_harness_state("harness-root")
            .await
            .unwrap(),
        before
    );
}

#[tokio::test]
async fn common_harness_external_owner_cannot_reserve_local_native_execution() {
    let (_workspace, session, _turn) = common_task().await;
    session
        .ensure_common_harness_attempt("harness-root")
        .await
        .unwrap();
    restore_external_execution_owner(&session).await;
    let before = session
        .current_common_harness_state("harness-root")
        .await
        .unwrap();
    let arguments = serde_json::json!({"executor":"test-native-executor"});
    let argument_bytes = serde_json::to_vec(&arguments).unwrap();
    let argument_ref = crate::common_harness_artifacts::write_operation_artifact(
        session.get_config().await.codex_home.as_path(),
        session.thread_id(),
        &arguments,
    )
    .unwrap();
    let guard = session.completion_owner_guard().await;
    let result = session
        .reserve_common_harness_operation_guarded(
            "harness-root",
            crate::session::common_harness::NativeOperationDefinition {
                operation_id: "test-external-owner-operation".into(),
                operation_kind: codex_history::task_context::OperationKindV1::RunVerifier,
                normalized_arguments: &argument_bytes,
                normalized_arguments_ref: argument_ref,
                permission_decision_ref: before.request.permission_snapshot_ref.clone(),
                executor_ref: "test-native-executor".into(),
                timeout_ms: 1_000,
            },
            &guard,
        )
        .await;
    assert!(
        result.is_err(),
        "local executor must recheck the task execution owner: {result:?}"
    );
    assert_eq!(
        session
            .current_common_harness_state("harness-root")
            .await
            .unwrap(),
        before
    );
}

#[tokio::test]
async fn common_harness_historical_cancel_does_not_block_new_chat_inference() {
    use crate::tools::context::ToolPayload;
    use crate::tools::router::ToolCall;
    use tokio_util::sync::CancellationToken;

    let (_workspace, session, turn, runtime) = super::acceptance_tests::verified_suite_task().await;
    let arguments =
        serde_json::json!({"action":"accept", "result":"Verified the current native suite."});
    super::record_call_with_arguments(
        &session,
        &turn,
        "cancel-scope-close-accept",
        "task_context_boundary",
        &arguments.to_string(),
    )
    .await;
    let output = runtime
        .clone()
        .handle_tool_call(
            ToolCall {
                tool_name: codex_tools::ToolName::plain("task_context_boundary"),
                call_id: "cancel-scope-close-accept".into(),
                payload: ToolPayload::Function {
                    arguments: arguments.to_string(),
                },
                encrypted_function_args: None,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    session
        .record_annotated_conversation_items(&turn, turn.model_info().as_ref(), vec![output])
        .await;
    assert!(
        session
            .commit_prepared_task_boundary(&turn, &CancellationToken::new())
            .await
            .unwrap()
    );
    assert!(session.active_task_scope().await.is_none());
    drop(runtime);
    let before = session
        .current_common_harness_state("harness-root")
        .await
        .unwrap();
    let cancelled = session
        .apply_common_harness_host_event(
            "harness-root",
            codex_history::task_context::StateFenceV1 {
                request_revision: before.request.request_revision,
                expected_state_version: before.run_state.state_version,
                owner_epoch: before.run_state.owner_epoch,
            },
            codex_history::task_context::HostEventV1::RequestCancel,
        )
        .await
        .unwrap();
    // A historical closed scope loaded for a later ordinary user turn.
    session.active_turn.lock().await.take();
    let (chat_turn, _) = session
        .new_turn_with_sub_id(
            "new-chat-after-cancel".into(),
            Default::default(),
            Default::default(),
        )
        .await
        .expect("construct the next ordinary user turn");
    assert!(
        session
            .common_harness_tool_state_for_turn(&chat_turn)
            .await
            .is_none()
    );
    assert!(
        session
            .common_harness_inference_deadline(&chat_turn)
            .await
            .unwrap()
            .is_none(),
        "historical terminal state must not disable the normal conversation route"
    );
    assert_eq!(
        session
            .current_common_harness_state("harness-root")
            .await
            .unwrap(),
        cancelled
    );
}
