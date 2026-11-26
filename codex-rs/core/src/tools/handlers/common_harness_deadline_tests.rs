// Modified by RenCrow Switch Core, 2026-11-27: test persisted inference budgets at the normal task seam.
use super::common_task;
use codex_history::task_context::HostEventV1;
use codex_history::task_context::StateFenceV1;
use codex_history::task_context::TaskRunStatusV1;
use codex_history::task_context::WorkWindowV1;
use std::sync::Arc;

#[tokio::test]
async fn common_harness_inference_binds_one_window_across_progress() {
    let (_workspace, session, turn) = common_task().await;
    assert!(
        session
            .common_harness_inference_deadline(&turn)
            .await
            .unwrap()
            .is_some()
    );
    let first = session
        .current_common_harness_state("harness-root")
        .await
        .unwrap();
    assert_eq!(first.run_state.attempt_no, 1);
    let window = first
        .run_state
        .work_window
        .as_ref()
        .expect("durable inference window");
    assert_eq!(window.attempt_no, 1);
    assert!(window.deadline_at_ms > window.started_at_ms);
    assert!(
        session
            .common_harness_inference_deadline(&turn)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        session
            .current_common_harness_state("harness-root")
            .await
            .unwrap(),
        first
    );
}

#[tokio::test]
async fn common_harness_expired_window_blocks_without_starting_a_new_attempt() {
    let (_workspace, session, turn) = common_task().await;
    let state = session
        .ensure_common_harness_attempt("harness-root")
        .await
        .unwrap();
    let now = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    session
        .apply_common_harness_host_event(
            "harness-root",
            StateFenceV1 {
                request_revision: state.request.request_revision,
                expected_state_version: state.run_state.state_version,
                owner_epoch: state.run_state.owner_epoch,
            },
            HostEventV1::BindWorkWindow {
                window: WorkWindowV1 {
                    window_id: "expired-original-window".into(),
                    attempt_no: 1,
                    started_at_ms: now - 1000,
                    deadline_at_ms: now - 1,
                    timeout_ms: 999,
                    policy_ref: "common-harness/test-expiry/1".into(),
                },
            },
        )
        .await
        .unwrap();
    assert!(
        session
            .common_harness_inference_deadline(&turn)
            .await
            .is_err()
    );
    let stopped = session
        .current_common_harness_state("harness-root")
        .await
        .unwrap();
    assert_eq!(stopped.run_state.status, TaskRunStatusV1::Blocked);
    assert_eq!(stopped.run_state.attempt_no, 1);
    assert_eq!(
        stopped.run_state.work_window.unwrap().window_id,
        "expired-original-window"
    );
}

#[tokio::test]
async fn common_harness_expired_window_precedes_trace_unavailable_without_new_attempt() {
    let (_workspace, session_arc, turn) = common_task().await;
    let mut session = Arc::try_unwrap(session_arc)
        .ok()
        .expect("unique fixture session");
    let state = session
        .ensure_common_harness_attempt("harness-root")
        .await
        .unwrap();
    let now = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    session
        .apply_common_harness_host_event(
            "harness-root",
            StateFenceV1 {
                request_revision: state.request.request_revision,
                expected_state_version: state.run_state.state_version,
                owner_epoch: state.run_state.owner_epoch,
            },
            HostEventV1::BindWorkWindow {
                window: WorkWindowV1 {
                    window_id: "expired-before-trace-gate".into(),
                    attempt_no: state.run_state.attempt_no,
                    started_at_ms: now - 1000,
                    deadline_at_ms: now - 1,
                    timeout_ms: 999,
                    policy_ref: "common-harness/test-expiry/1".into(),
                },
            },
        )
        .await
        .unwrap();
    session.services.rollout_thread_trace = codex_rollout_trace::ThreadTraceContext::disabled();

    let error = session
        .common_harness_inference_deadline(&turn)
        .await
        .unwrap_err();
    assert!(error.contains("absolute work window expired"));
    let blocked = session
        .current_common_harness_state("harness-root")
        .await
        .unwrap();
    assert_eq!(
        blocked.run_state.reason_code.as_deref(),
        Some("work_window_expired")
    );
    assert_eq!(blocked.run_state.attempt_no, state.run_state.attempt_no);
}
