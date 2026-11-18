// Modified by RenCrow Switch Core, 2026-11-18.
use super::turn::ExplicitExecCancelReceipt;
use super::turn::PendingApproval;
use super::turn::TurnState;
use codex_protocol::protocol::ReviewDecision;
use std::sync::Arc;
use tokio::sync::oneshot;

fn pending_approval(
    call_id: &str,
    turn_id: &str,
    receipt: Option<Arc<ExplicitExecCancelReceipt>>,
) -> (PendingApproval, oneshot::Receiver<ReviewDecision>) {
    let (tx, rx) = oneshot::channel::<ReviewDecision>();
    (
        PendingApproval {
            tx,
            call_id: call_id.to_string(),
            turn_id: turn_id.to_string(),
            explicit_exec_cancel_receipt: receipt,
        },
        rx,
    )
}

#[test]
fn explicit_cancel_is_exact_turn_scoped_and_survives_pending_cleanup() {
    let receipt = Arc::new(ExplicitExecCancelReceipt::new(
        "launch-call".to_string(),
        "launch-turn".to_string(),
    ));
    let mut turn_state = TurnState::default();
    let (pending, _approval_rx) =
        pending_approval("launch-call", "approval-turn", Some(Arc::clone(&receipt)));
    turn_state.insert_pending_approval("approval-1".to_string(), pending);

    assert_eq!(
        turn_state.record_explicit_exec_cancel(
            "stale-approval",
            Some("approval-turn"),
            "approval-turn",
        ),
        None
    );
    assert_eq!(
        turn_state.record_explicit_exec_cancel("approval-1", Some("other-turn"), "approval-turn",),
        None
    );
    assert_eq!(
        turn_state.record_explicit_exec_cancel(
            "approval-1",
            Some("approval-turn"),
            "replacement-turn",
        ),
        None
    );
    assert!(!receipt.was_cancelled_for_command("launch-call", "launch-turn"));

    assert_eq!(
        turn_state.record_explicit_exec_cancel("approval-1", None, "approval-turn",),
        Some("approval-turn".to_string())
    );
    assert!(receipt.was_cancelled_for_command("launch-call", "launch-turn"));
    turn_state.clear_pending_waiters();
    assert!(receipt.was_cancelled_for_command("launch-call", "launch-turn"));
    assert!(!receipt.was_cancelled_for_command("sibling-call", "launch-turn"));
    assert!(!receipt.was_cancelled_for_command("launch-call", "approval-turn"));
}

#[test]
fn unrelated_calls_and_fresh_retry_receipts_do_not_inherit_cancel() {
    let receipt = Arc::new(ExplicitExecCancelReceipt::new(
        "launch-call".to_string(),
        "launch-turn".to_string(),
    ));
    let mut turn_state = TurnState::default();
    let (pending, _approval_rx) =
        pending_approval("sibling-call", "approval-turn", Some(Arc::clone(&receipt)));
    turn_state.insert_pending_approval("approval-1".to_string(), pending);
    assert_eq!(
        turn_state.record_explicit_exec_cancel(
            "approval-1",
            Some("approval-turn"),
            "approval-turn",
        ),
        None
    );
    assert!(!receipt.was_cancelled_for_command("launch-call", "launch-turn"));

    let retry_receipt =
        ExplicitExecCancelReceipt::new("launch-call".to_string(), "retry-turn".to_string());
    assert!(!retry_receipt.was_cancelled_for_command("launch-call", "retry-turn"));
}

#[test]
fn generic_pending_cleanup_does_not_create_an_explicit_cancel_receipt() {
    let receipt = Arc::new(ExplicitExecCancelReceipt::new(
        "launch-call".to_string(),
        "launch-turn".to_string(),
    ));
    let mut turn_state = TurnState::default();
    let (pending, _approval_rx) =
        pending_approval("launch-call", "approval-turn", Some(Arc::clone(&receipt)));
    turn_state.insert_pending_approval("approval-1".to_string(), pending);
    turn_state.clear_pending_waiters();
    assert!(!receipt.was_cancelled_for_command("launch-call", "launch-turn"));

    let mut ordinary_turn = TurnState::default();
    let (pending, _ordinary_rx) = pending_approval("ordinary-call", "approval-turn", None);
    ordinary_turn.insert_pending_approval("ordinary-approval".to_string(), pending);
    assert_eq!(
        ordinary_turn.record_explicit_exec_cancel("ordinary-approval", None, "approval-turn",),
        Some("approval-turn".to_string())
    );
}

#[test]
fn closed_approval_receiver_cannot_authorize_cancellation() {
    let receipt = Arc::new(ExplicitExecCancelReceipt::new(
        "launch-call".to_string(),
        "launch-turn".to_string(),
    ));
    let mut turn_state = TurnState::default();
    let (pending, approval_rx) =
        pending_approval("launch-call", "approval-turn", Some(Arc::clone(&receipt)));
    drop(approval_rx);
    turn_state.insert_pending_approval("approval-1".to_string(), pending);

    assert_eq!(
        turn_state.record_explicit_exec_cancel(
            "approval-1",
            Some("approval-turn"),
            "approval-turn",
        ),
        None
    );
    assert!(!receipt.was_cancelled_for_command("launch-call", "launch-turn"));
}
