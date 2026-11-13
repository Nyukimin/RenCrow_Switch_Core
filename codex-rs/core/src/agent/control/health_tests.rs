use super::HealthDeliveryState;
use super::ParentGeneration;
use super::StatusKind;
use crate::agent::AgentStatus;
use crate::agent::child_monitor::ChildMonitorPhase;
use crate::agent::child_monitor::health::ChildHealthNotice;
use crate::agent::child_monitor::health::ChildHealthNoticeKind;

#[test]
fn failed_notice_retries_only_after_parent_or_child_generation_changes() {
    let notice = ChildHealthNotice {
        kind: ChildHealthNoticeKind::ProgressSuspected,
        turn_id: Some("child-turn-1".to_string()),
        phase: ChildMonitorPhase::Generating,
        reason_code: "other_quiet".to_string(),
        observed_at_ms: 10_000,
        age_ms: Some(300_000),
        progress_sequence: 4,
    };
    let parent = ParentGeneration::new(Some("parent-turn-1".to_string()), &AgentStatus::Running);
    let mut delivery = HealthDeliveryState::default();

    assert!(delivery.should_attempt(&notice, &parent));
    delivery.failed(&notice, parent.clone());
    assert!(!delivery.should_attempt(&notice, &parent));

    let next_parent =
        ParentGeneration::new(Some("parent-turn-2".to_string()), &AgentStatus::Running);
    assert!(delivery.should_attempt(&notice, &next_parent));

    let mut next_progress = notice.clone();
    next_progress.progress_sequence += 1;
    assert!(delivery.should_attempt(&next_progress, &parent));
    delivery.accepted(&next_progress);
    assert!(!delivery.should_attempt(&next_progress, &next_parent));

    assert_eq!(parent.status, StatusKind::Running);
}
