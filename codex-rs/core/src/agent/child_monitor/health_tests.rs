use super::COLLECTION_UNCONFIRMED_AFTER_MS;
use super::ChildHealthNoticeKind;
use super::ChildHealthNotifications;
use super::PROGRESS_SUSPECTED_AFTER_MS;
use super::evaluate_child_health;
use crate::agent::child_monitor::ChildMonitorPhase;
use crate::agent::child_monitor::ChildMonitorSnapshot;

#[test]
fn progress_suspicion_is_observational_and_ignores_input_waiting_or_missing_clock() {
    let mut snapshot = ChildMonitorSnapshot {
        turn_id: Some("turn-a".into()),
        phase: ChildMonitorPhase::Generating,
        last_activity_at_ms: Some(1_000),
        progress_sequence: 7,
        ..ChildMonitorSnapshot::default()
    };
    let before = snapshot.clone();
    let notices = evaluate_child_health(&snapshot, 1_000 + PROGRESS_SUSPECTED_AFTER_MS);
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].kind, ChildHealthNoticeKind::ProgressSuspected);
    assert_eq!(notices[0].reason_code, "other_quiet");
    assert_eq!(snapshot, before);

    snapshot.phase = ChildMonitorPhase::InputWaiting;
    assert!(evaluate_child_health(&snapshot, 900_000).is_empty());

    snapshot.phase = ChildMonitorPhase::Unknown;
    snapshot.last_activity_at_ms = None;
    assert!(evaluate_child_health(&snapshot, 900_000).is_empty());

    snapshot.last_activity_at_ms = Some(1_000);
    snapshot.turn_completed_at_ms = Some(2_000);
    snapshot.timing.child_work_deadline_at_ms = Some(2_000);
    assert!(evaluate_child_health(&snapshot, 900_000).is_empty());

    snapshot.phase = ChildMonitorPhase::Pending;
    snapshot.turn_completed_at_ms = None;
    snapshot.timing.child_work_deadline_at_ms = None;
    snapshot.last_activity_at_ms = Some(1_000);
    snapshot.first_output_at_ms = Some(1_500);
    snapshot.timing.current_request_started_at_ms = Some(2_000);
    let post_tool_wait = evaluate_child_health(&snapshot, 1_000 + PROGRESS_SUSPECTED_AFTER_MS);
    assert_eq!(post_tool_wait[0].reason_code, "first_output_wait");

    snapshot.timing.current_request_first_output_at_ms = Some(2_100);
    let output_received = evaluate_child_health(&snapshot, 1_000 + PROGRESS_SUSPECTED_AFTER_MS);
    assert_eq!(output_received[0].reason_code, "other_quiet");

    snapshot.timing.current_request_first_output_at_ms = None;
    snapshot.timing.current_request_finished_at_ms = Some(2_200);
    let request_finished = evaluate_child_health(&snapshot, 1_000 + PROGRESS_SUSPECTED_AFTER_MS);
    assert_eq!(request_finished[0].reason_code, "other_quiet");
}

#[test]
fn collection_notice_and_dedup_rearm_only_after_real_progress() {
    let mut snapshot = ChildMonitorSnapshot {
        turn_id: Some("turn-b".into()),
        phase: ChildMonitorPhase::Completed,
        turn_completed_at_ms: Some(10_000),
        progress_sequence: 4,
        ..ChildMonitorSnapshot::default()
    };
    let notices = evaluate_child_health(&snapshot, 10_000 + COLLECTION_UNCONFIRMED_AFTER_MS);
    assert_eq!(notices.len(), 1);
    assert_eq!(
        notices[0].kind,
        ChildHealthNoticeKind::CollectionUnconfirmed
    );
    assert_eq!(notices[0].reason_code, "no_collection_receipt_observed");

    let mut delivery = ChildHealthNotifications::default();
    assert!(delivery.should_send(&notices[0]));
    delivery.sent(&notices[0]);
    assert!(!delivery.should_send(&notices[0]));

    snapshot.result_collected_at_ms = Some(50_000);
    assert!(evaluate_child_health(&snapshot, 100_000).is_empty());

    let mut active = ChildMonitorSnapshot {
        turn_id: Some("turn-c".into()),
        phase: ChildMonitorPhase::Generating,
        last_activity_at_ms: Some(100_000),
        progress_sequence: 9,
        ..ChildMonitorSnapshot::default()
    };
    let progress = evaluate_child_health(&active, 100_000 + PROGRESS_SUSPECTED_AFTER_MS);
    delivery.sent(&progress[0]);
    assert!(!delivery.should_send(&progress[0]));
    active.last_activity_at_ms = Some(500_000);
    active.progress_sequence += 1;
    let new_progress = evaluate_child_health(&active, 500_000 + PROGRESS_SUSPECTED_AFTER_MS);
    assert!(delivery.should_send(&new_progress[0]));
}
