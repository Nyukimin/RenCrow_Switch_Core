// Modified by RenCrow Switch Core, 2026-11-26: test takeover against the actual local writer.
use super::*;
use crate::session::common_harness::common_harness_runtime_owner_id;
use pretty_assertions::assert_eq;

async fn persisted_previous_owner(
    session: &Session,
) -> codex_history::task_context::CommonHarnessStateV1 {
    let started = session
        .ensure_common_harness_attempt("harness-root")
        .await
        .unwrap();
    let waiting = session
        .apply_common_harness_host_event(
            "harness-root",
            StateFenceV1 {
                request_revision: started.request.request_revision,
                expected_state_version: started.run_state.state_version,
                owner_epoch: started.run_state.owner_epoch,
            },
            HostEventV1::EnterWaiting {
                question_ref: "approval-question".into(),
                operation_digest: "bound-approval-operation".into(),
                resume_phase: TaskRunPhaseV1::Apply,
            },
        )
        .await
        .unwrap();
    session
        .apply_common_harness_host_event(
            "harness-root",
            StateFenceV1 {
                request_revision: waiting.request.request_revision,
                expected_state_version: waiting.run_state.state_version,
                owner_epoch: waiting.run_state.owner_epoch,
            },
            HostEventV1::TakeOwnership {
                new_owner_id: "fixture-previous-runtime".into(),
                new_owner_epoch: waiting.run_state.owner_epoch + 1,
                takeover_proof_ref: "fixture:previous-writer".into(),
            },
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn common_harness_local_writer_takeover_preserves_waiting_without_new_attempt() {
    let (_workspace, session, turn) = common_task_in_store(true, true).await;
    let before = persisted_previous_owner(&session).await;
    let (root, after) = session
        .common_harness_turn_state(&turn)
        .await
        .expect("a verified local writer can recover its persisted owner fence")
        .unwrap();
    assert_eq!(root, "harness-root");
    assert_eq!(
        after.run_state.owner_id,
        common_harness_runtime_owner_id(&session.thread_id().to_string())
    );
    assert_eq!(
        after.run_state.owner_epoch,
        before.run_state.owner_epoch + 1
    );
    assert_eq!(after.run_state.attempt_no, before.run_state.attempt_no);
    assert_eq!(after.run_state.status, TaskRunStatusV1::Waiting);
    assert_eq!(after.run_state.waiting, before.run_state.waiting);
    assert_eq!(after.request, before.request);
    assert!(matches!(
        session.common_harness_turn_end_gate(&turn).await.unwrap(),
        CommonHarnessTurnEndDecision::Stopped {
            status: TaskRunStatusV1::Waiting,
            ..
        }
    ));
    assert_eq!(
        session
            .current_common_harness_state("harness-root")
            .await
            .unwrap(),
        after,
        "re-reading a recovered waiting task cannot consume another epoch or attempt"
    );
    let takeover = after.owner_takeovers.last().unwrap();
    let proof: serde_json::Value = crate::common_harness_artifacts::load_operation_artifact(
        turn.config.codex_home.as_path(),
        session.thread_id(),
        &takeover.takeover_proof_ref,
    )
    .expect("the new writer fence must refer to a real durable proof artifact");
    assert!(proof.is_object());
}

#[tokio::test]
async fn common_harness_in_memory_writer_cannot_take_over_or_run_host_events() {
    let (_workspace, session, turn) = common_task_in_store(true, false).await;
    let before = persisted_previous_owner(&session).await;
    assert!(session.common_harness_turn_state(&turn).await.is_err());
    assert!(
        session
            .apply_common_harness_host_event(
                "harness-root",
                StateFenceV1 {
                    request_revision: before.request.request_revision,
                    expected_state_version: before.run_state.state_version,
                    owner_epoch: before.run_state.owner_epoch,
                },
                HostEventV1::Block {
                    reason_code: "forged-update".into(),
                    resume_condition: "none".into()
                }
            )
            .await
            .is_err()
    );
    assert_eq!(
        session
            .current_common_harness_state("harness-root")
            .await
            .unwrap(),
        before
    );
}
