use super::ChildMonitorPhase;
use super::ChildMonitorSnapshot;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;

pub(crate) const PROGRESS_SUSPECTED_AFTER_MS: i64 = 300_000;
pub(crate) const COLLECTION_UNCONFIRMED_AFTER_MS: i64 = 30_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ChildHealthNoticeKind {
    ProgressSuspected,
    WorkDeadlinePassed,
    CollectionUnconfirmed,
    ObservationUnavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct ChildHealthNotice {
    pub(crate) kind: ChildHealthNoticeKind,
    pub(crate) turn_id: Option<String>,
    pub(crate) phase: ChildMonitorPhase,
    pub(crate) reason_code: String,
    pub(crate) observed_at_ms: i64,
    pub(crate) age_ms: Option<i64>,
    pub(crate) progress_sequence: u64,
}

/// Evaluate time based observations without changing monitor state or inferring
/// that a child has stopped.
pub(crate) fn evaluate_child_health(
    snapshot: &ChildMonitorSnapshot,
    now_ms: i64,
) -> Vec<ChildHealthNotice> {
    let mut notices = Vec::new();
    let terminal = matches!(
        snapshot.phase,
        ChildMonitorPhase::Completed | ChildMonitorPhase::Failed
    );

    if snapshot.turn_id.is_some()
        && snapshot.turn_completed_at_ms.is_none()
        && matches!(
            snapshot.phase,
            ChildMonitorPhase::Pending
                | ChildMonitorPhase::Generating
                | ChildMonitorPhase::ToolExecution
                | ChildMonitorPhase::Compacting
                | ChildMonitorPhase::Unknown
        )
        && let Some(last_activity_at_ms) = snapshot.last_activity_at_ms
    {
        let age_ms = elapsed_ms(last_activity_at_ms, now_ms);
        if age_ms >= PROGRESS_SUSPECTED_AFTER_MS {
            notices.push(notice(
                snapshot,
                ChildHealthNoticeKind::ProgressSuspected,
                progress_reason(snapshot),
                now_ms,
                Some(age_ms),
            ));
        }
    }

    if snapshot.turn_id.is_some()
        && snapshot.turn_completed_at_ms.is_none()
        && !terminal
        && let Some(deadline_at_ms) = snapshot.timing.child_work_deadline_at_ms
    {
        let age_ms = elapsed_ms(deadline_at_ms, now_ms);
        if now_ms >= deadline_at_ms {
            notices.push(notice(
                snapshot,
                ChildHealthNoticeKind::WorkDeadlinePassed,
                "owned_work_deadline_passed",
                now_ms,
                Some(age_ms),
            ));
        }
    }

    if terminal
        && snapshot.result_collected_at_ms.is_none()
        && let Some(completed_at_ms) = snapshot.turn_completed_at_ms
    {
        let age_ms = elapsed_ms(completed_at_ms, now_ms);
        if age_ms >= COLLECTION_UNCONFIRMED_AFTER_MS {
            notices.push(notice(
                snapshot,
                ChildHealthNoticeKind::CollectionUnconfirmed,
                "no_collection_receipt_observed",
                now_ms,
                Some(age_ms),
            ));
        }
    }

    notices
}

fn progress_reason(snapshot: &ChildMonitorSnapshot) -> &'static str {
    match snapshot.phase {
        ChildMonitorPhase::Pending
            if snapshot.timing.current_request_started_at_ms.is_some()
                && snapshot.timing.current_request_first_output_at_ms.is_none()
                && snapshot.timing.current_request_finished_at_ms.is_none() =>
        {
            "first_output_wait"
        }
        ChildMonitorPhase::ToolExecution => "tool_quiet",
        ChildMonitorPhase::Compacting => "compaction_quiet",
        ChildMonitorPhase::Pending | ChildMonitorPhase::Generating | ChildMonitorPhase::Unknown => {
            "other_quiet"
        }
        ChildMonitorPhase::InputWaiting
        | ChildMonitorPhase::Completed
        | ChildMonitorPhase::Failed => "not_applicable",
    }
}

fn notice(
    snapshot: &ChildMonitorSnapshot,
    kind: ChildHealthNoticeKind,
    reason_code: &'static str,
    observed_at_ms: i64,
    age_ms: Option<i64>,
) -> ChildHealthNotice {
    ChildHealthNotice {
        kind,
        turn_id: snapshot.turn_id.clone(),
        phase: snapshot.phase,
        reason_code: reason_code.to_string(),
        observed_at_ms,
        age_ms,
        progress_sequence: snapshot.progress_sequence,
    }
}

fn elapsed_ms(start_ms: i64, end_ms: i64) -> i64 {
    end_ms.saturating_sub(start_ms).max(0)
}

/// Suppresses duplicate delivery for the same notice kind, turn, and observed
/// progress generation. Call `sent` only after successful delivery.
#[derive(Debug, Default)]
pub(crate) struct ChildHealthNotifications {
    sent: HashMap<ChildHealthNoticeKind, (Option<String>, u64)>,
}

impl ChildHealthNotifications {
    pub(crate) fn should_send(&self, notice: &ChildHealthNotice) -> bool {
        self.sent.get(&notice.kind) != Some(&(notice.turn_id.clone(), notice.progress_sequence))
    }

    pub(crate) fn sent(&mut self, notice: &ChildHealthNotice) {
        self.sent.insert(
            notice.kind,
            (notice.turn_id.clone(), notice.progress_sequence),
        );
    }
}

#[cfg(test)]
#[path = "health_tests.rs"]
mod tests;
