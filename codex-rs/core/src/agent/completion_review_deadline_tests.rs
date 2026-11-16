// Added by RenCrow Switch Core, 2026-11-16: require exact reviewer abort generation and reason.
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnAbortedEvent;

use super::review_ended_event_matches;

#[test]
fn persisted_abort_requires_the_exact_turn_and_review_ended_reason() {
    let event = |turn_id: &str, reason| TurnAbortedEvent {
        turn_id: Some(turn_id.to_owned()),
        reason,
        started_at: None,
        completed_at: None,
        duration_ms: None,
    };

    assert!(review_ended_event_matches(
        &event("review-turn", TurnAbortReason::ReviewEnded),
        "review-turn"
    ));
    assert!(!review_ended_event_matches(
        &event("other-turn", TurnAbortReason::ReviewEnded),
        "review-turn"
    ));
    assert!(!review_ended_event_matches(
        &event("review-turn", TurnAbortReason::Interrupted),
        "review-turn"
    ));
    let mut unbound_event = event("review-turn", TurnAbortReason::ReviewEnded);
    unbound_event.turn_id = None;
    assert!(!review_ended_event_matches(&unbound_event, "review-turn"));
}
