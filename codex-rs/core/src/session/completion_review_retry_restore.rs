// Added by RenCrow Switch Core, 2026-11-19: verify a bounded legacy retry collection transition.
use codex_history::compaction_candidate::digest;
use codex_history::task_context::CompletionReviewAttempt;
use codex_history::task_context::CompletionReviewAttemptStatus;
use codex_history::task_context::CompletionReviewTerminalStatus;
use codex_history::task_context::CompletionState;
use codex_history::task_context::CompletionUnit;
use codex_history::task_context::FailedApproach;
use codex_history::task_context::UnitStatus;

pub(super) const RETRY_ROLLBACK_WITHOUT_RESERVED_RETRY_ERROR: &str = "completion state validation failed: retry rollback history is not attached to a reserved retry";
pub(super) const REVIEWER_TURN_ENDED_WITH_ERROR_CAUSE: &str = "reviewer turn ended with an error";
pub(super) const REVIEWER_FAILURE_NEXT_ACTION: &str =
    "Diagnose the reviewer failure before creating a new authorized plan";

/// Prove the one historical failure transition whose terminal snapshot was written before its
/// native wait result was collected. The restore caller separately checks snapshot identity,
/// sequence, references, and the ordinary payload validator for both snapshots.
pub(super) fn legacy_retry_terminal_collection_matches(
    previous: &CompletionState,
    terminal: &CompletionState,
    collected: &CompletionState,
    expected_parent_thread_id: &str,
) -> bool {
    if previous.contract_revision != terminal.contract_revision
        || terminal.contract_revision != collected.contract_revision
        || previous.paused != terminal.paused
        || terminal.paused != collected.paused
        || previous.failed_approaches != terminal.failed_approaches
        || collected.failed_approaches.len() != terminal.failed_approaches.len() + 1
        || collected.failed_approaches[..terminal.failed_approaches.len()]
            != terminal.failed_approaches
    {
        return false;
    }

    let changed_unit_id = terminal.units.iter().find_map(|unit| {
        let review = unit.review_state.as_ref()?;
        let attempt = review.active_attempt.as_ref()?;
        (review.retry_reservation_prior_historical_failure.is_some()
            && attempt.status == CompletionReviewAttemptStatus::Failed)
            .then_some(unit.id.as_str())
    });
    let Some(unit_id) = changed_unit_id else {
        return false;
    };
    if terminal
        .units
        .iter()
        .filter(|unit| {
            unit.review_state
                .as_ref()
                .is_some_and(|review| review.retry_reservation_prior_historical_failure.is_some())
        })
        .count()
        != 1
    {
        return false;
    }

    let Some(previous_unit) = previous.units.iter().find(|unit| unit.id == unit_id) else {
        return false;
    };
    let Some(terminal_unit) = terminal.units.iter().find(|unit| unit.id == unit_id) else {
        return false;
    };
    let Some(collected_unit) = collected.units.iter().find(|unit| unit.id == unit_id) else {
        return false;
    };
    let Some(terminal_review) = terminal_unit.review_state.as_ref() else {
        return false;
    };
    let Some(terminal_attempt) = terminal_review.active_attempt.as_ref() else {
        return false;
    };
    let Some(terminal_source) = terminal_attempt.terminal_source.as_ref() else {
        return false;
    };
    let Some(event) = terminal_source.event.as_ref() else {
        return false;
    };
    let Some(collected_review) = collected_unit.review_state.as_ref() else {
        return false;
    };
    let Some(collected_attempt) = collected_review.active_attempt.as_ref() else {
        return false;
    };
    let Some(wait_call_id) = collected_attempt.wait_call_id.as_deref() else {
        return false;
    };
    let Some(wait_result_ref) = collected_attempt.wait_result_ref.as_deref() else {
        return false;
    };

    if !prior_retry_matches_terminal(previous_unit, terminal_unit)
        || terminal_unit.status != UnitStatus::Failed
        || terminal_source.status != CompletionReviewTerminalStatus::Failed
        || terminal_attempt.status != CompletionReviewAttemptStatus::Failed
        || terminal_attempt.wait_result_ref.is_some()
        || terminal_attempt.wait_call_id.is_some()
        || terminal_attempt.consumed_retry_authorization.is_none()
        || terminal_attempt.reviewer_thread_id.as_deref() != Some(event.owner_thread_id.as_str())
        || terminal_attempt.reviewer_turn_id.as_deref() != Some(event.turn_id.as_str())
        || event.owner_parent_thread_id.as_deref() != Some(expected_parent_thread_id)
        || event.owner_agent_role.as_deref() != Some(terminal_attempt.reviewer_role.as_str())
        || event.reason != "ReviewEnded"
        || event.rollout_id.trim().is_empty()
        || collected_attempt.attempt_id != terminal_attempt.attempt_id
        || collected_attempt.status != CompletionReviewAttemptStatus::Failed
        || collected_attempt.terminal_source != terminal_attempt.terminal_source
        || wait_call_id.trim().is_empty()
        || expected_review_ended_wait_result_ref(terminal_attempt, wait_call_id).as_deref()
            != Some(wait_result_ref)
    {
        return false;
    }

    let Some(failed_approach) = collected.failed_approaches.last() else {
        return false;
    };
    if !is_review_ended_wait_failure(failed_approach, unit_id) {
        return false;
    }

    let mut expected = terminal.clone();
    let Some(expected_unit) = expected.units.iter_mut().find(|unit| unit.id == unit_id) else {
        return false;
    };
    let Some(expected_review) = expected_unit.review_state.as_mut() else {
        return false;
    };
    expected_review.retry_reservation_prior_historical_failure = None;
    let Some(expected_attempt) = expected_review.active_attempt.as_mut() else {
        return false;
    };
    expected_attempt.wait_call_id = Some(wait_call_id.to_owned());
    expected_attempt.wait_result_ref = Some(wait_result_ref.to_owned());
    let Ok(projected_unit) =
        codex_history::task_context::project_completion_review_state(expected_unit)
    else {
        return false;
    };
    *expected_unit = projected_unit;
    expected.failed_approaches.push(failed_approach.clone());
    &expected == collected
}

fn expected_review_ended_wait_result_ref(
    attempt: &CompletionReviewAttempt,
    wait_call_id: &str,
) -> Option<String> {
    digest(&serde_json::json!({
        "wait_call_id": wait_call_id,
        "reviewer_thread_id": attempt.reviewer_thread_id,
        "reviewer_turn_id": attempt.reviewer_turn_id,
        "terminal_status": format!("{:?}", crate::agent::AgentStatus::Errored("ReviewEnded".into())),
        "cause": REVIEWER_TURN_ENDED_WITH_ERROR_CAUSE,
    }))
    .ok()
}

fn is_review_ended_wait_failure(approach: &FailedApproach, unit_id: &str) -> bool {
    approach.summary == format!("review unit {unit_id} failed")
        && approach.cause == REVIEWER_TURN_ENDED_WITH_ERROR_CAUSE
        && approach.next_action == REVIEWER_FAILURE_NEXT_ACTION
        && approach.recorded_at > 0
}

fn prior_retry_matches_terminal(
    previous_unit: &CompletionUnit,
    terminal_unit: &CompletionUnit,
) -> bool {
    let Some(previous_review) = previous_unit.review_state.as_ref() else {
        return false;
    };
    let Some(terminal_review) = terminal_unit.review_state.as_ref() else {
        return false;
    };
    let Some(previous_attempt) = previous_review.active_attempt.as_ref() else {
        return false;
    };
    let Some(terminal_attempt) = terminal_review.active_attempt.as_ref() else {
        return false;
    };
    previous_unit.kind == codex_history::task_context::CompletionUnitKind::Review
        && previous_unit.status == UnitStatus::Running
        && previous_attempt.status == CompletionReviewAttemptStatus::Bound
        && previous_attempt.consumed_retry_authorization.is_some()
        && previous_attempt.attempt_id == terminal_attempt.attempt_id
        && previous_attempt.consumed_retry_authorization
            == terminal_attempt.consumed_retry_authorization
        && previous_review
            .retry_reservation_prior_historical_failure
            .is_some()
        && previous_review.retry_reservation_prior_historical_failure
            == terminal_review.retry_reservation_prior_historical_failure
        && previous_review.historical_failure == terminal_review.historical_failure
        && previous_review.previous_attempt_ref == terminal_review.previous_attempt_ref
        && previous_unit.review_target == terminal_unit.review_target
        && previous_unit.condition_refs == terminal_unit.condition_refs
        && previous_attempt.reviewer_thread_id == terminal_attempt.reviewer_thread_id
        && previous_attempt.reviewer_turn_id == terminal_attempt.reviewer_turn_id
        && previous_attempt.reviewer_role == terminal_attempt.reviewer_role
        && previous_attempt.reviewer_model == terminal_attempt.reviewer_model
        && previous_attempt.target_task_id == terminal_attempt.target_task_id
        && previous_attempt.target_task_revision == terminal_attempt.target_task_revision
        && previous_attempt.target_artifact_ref == terminal_attempt.target_artifact_ref
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_history::task_context::CompletionReviewHistoricalFailure;
    use codex_history::task_context::CompletionReviewRetryAuthorization;
    use codex_history::task_context::CompletionReviewSnapshotRef;
    use codex_history::task_context::CompletionReviewState;
    use codex_history::task_context::CompletionReviewTarget;
    use codex_history::task_context::CompletionReviewTerminalEvent;
    use codex_history::task_context::CompletionReviewTerminalSource;
    use codex_history::task_context::CompletionUnitKind;

    fn retry_state(status: CompletionReviewAttemptStatus) -> CompletionState {
        let authorization = CompletionReviewRetryAuthorization {
            authorization_id: "plan-review-call".into(),
            expected_attempt_id: "prior-attempt".into(),
            plan_review_call_id: "plan-review-call".into(),
            reason: "retry the exact failed obligation".into(),
            previous_plan: "previous".into(),
            revised_plan: "revised".into(),
        };
        let failure = CompletionReviewHistoricalFailure {
            attempt_id: "prior-attempt".into(),
            contract_revision: Some(1),
            imported_from: None,
            consumed_retry_authorization: None,
            reviewer_thread_id: "reviewer-thread".into(),
            reviewer_turn_id: "prior-turn".into(),
            target_task_id: "task-a".into(),
            target_task_revision: 1,
            target_artifact_ref: "artifact-a".into(),
            reviewer_role: "analyst".into(),
            reviewer_model: Some("model".into()),
            deadline_unix_ms: Some(10),
            wait_result_ref: Some("a".repeat(64)),
            wait_call_id: Some("prior-wait".into()),
            deadline_escalation: None,
            deadline_safety_fingerprint: None,
            terminal_source: CompletionReviewTerminalSource {
                status: CompletionReviewTerminalStatus::Failed,
                event: None,
            },
        };
        let review_state = CompletionReviewState {
            version: 1,
            obligation_unit_id: "review-a".into(),
            active_attempt: Some(CompletionReviewAttempt {
                attempt_id: "retry-attempt".into(),
                contract_revision: 1,
                condition_refs: vec!["AC-1".into()],
                conditions_sha256: "b".repeat(64),
                status,
                launch_unknown_reason: None,
                consumed_retry_authorization: Some(authorization),
                imported_from: None,
                reviewer_thread_id: Some("reviewer-thread".into()),
                reviewer_turn_id: Some("reviewer-turn".into()),
                reviewer_role: "analyst".into(),
                reviewer_model: Some("model".into()),
                target_task_id: "task-a".into(),
                target_task_revision: 1,
                target_artifact_ref: "artifact-a".into(),
                deadline_unix_ms: Some(20),
                deadline_escalation: None,
                deadline_safety_fingerprint: None,
                terminal_source: None,
                wait_result_ref: None,
                wait_call_id: None,
                adoption_reason: None,
            }),
            historical_failure: Some(failure.clone()),
            retry_reservation_prior_historical_failure: Some(failure),
            previous_attempt_ref: Some(CompletionReviewSnapshotRef {
                version: 2,
                sha256: "c".repeat(64),
            }),
            retry_authorization: None,
            withdrawal: None,
            revalidation: None,
        };
        let unit = CompletionUnit {
            id: "review-a".into(),
            purpose: "review accepted artifact".into(),
            status: if status == CompletionReviewAttemptStatus::Failed {
                UnitStatus::Failed
            } else {
                UnitStatus::Running
            },
            kind: CompletionUnitKind::Review,
            condition_refs: vec!["AC-1".into()],
            depends_on: vec![],
            evidence: vec![],
            task_refs: vec![],
            review_target: Some(CompletionReviewTarget {
                task_id: "task-a".into(),
                task_revision: 1,
            }),
            review_evidence: None,
            review_launch: None,
            review_decision_reason: None,
            review_state: Some(review_state),
            review_revalidation_of: None,
        };
        CompletionState {
            contract_revision: 1,
            units_completed_since_review: 0,
            plan_review_at: None,
            plan_reviews: vec![],
            paused: false,
            consumed_host_events: vec![],
            conditions: vec![],
            units: vec![unit],
            current_unit: Some("review-a".into()),
            failed_approaches: vec![],
            blocker: None,
            next_action: None,
            product_progress_at: None,
            runtime_recovery_at: None,
            activity_at: None,
        }
    }

    fn legacy_pair() -> (CompletionState, CompletionState, CompletionState) {
        let previous = retry_state(CompletionReviewAttemptStatus::Bound);
        let mut terminal = retry_state(CompletionReviewAttemptStatus::Failed);
        let terminal_attempt = terminal.units[0]
            .review_state
            .as_mut()
            .unwrap()
            .active_attempt
            .as_mut()
            .unwrap();
        terminal_attempt.terminal_source = Some(CompletionReviewTerminalSource {
            status: CompletionReviewTerminalStatus::Failed,
            event: Some(CompletionReviewTerminalEvent {
                rollout_id: "reviewer-thread".into(),
                locator: codex_history::task_context::CompletionReviewRecordLocator::Ordinal(9),
                owner_thread_id: "reviewer-thread".into(),
                turn_id: "reviewer-turn".into(),
                owner_parent_thread_id: Some("parent-thread".into()),
                owner_agent_role: Some("analyst".into()),
                reason: "ReviewEnded".into(),
            }),
        });
        let mut collected = terminal.clone();
        let wait_call_id = "wait-call";
        let wait_result_ref = expected_review_ended_wait_result_ref(
            collected.units[0]
                .review_state
                .as_ref()
                .unwrap()
                .active_attempt
                .as_ref()
                .unwrap(),
            wait_call_id,
        )
        .expect("wait result identity is serializable");
        let review = collected.units[0].review_state.as_mut().unwrap();
        review.retry_reservation_prior_historical_failure = None;
        let attempt = review.active_attempt.as_mut().unwrap();
        attempt.wait_call_id = Some(wait_call_id.into());
        attempt.wait_result_ref = Some(wait_result_ref);
        collected.units[0] =
            codex_history::task_context::project_completion_review_state(&collected.units[0])
                .unwrap();
        collected.failed_approaches.push(FailedApproach {
            summary: "review unit review-a failed".into(),
            cause: REVIEWER_TURN_ENDED_WITH_ERROR_CAUSE.into(),
            next_action: REVIEWER_FAILURE_NEXT_ACTION.into(),
            recorded_at: 21,
        });
        (previous, terminal, collected)
    }

    #[test]
    fn accepts_only_exact_review_ended_wait_collection_transition() {
        // This is a pure state-transition predicate fixture. The integration test exercises
        // ordinary snapshot validation and restore sequencing with a live owner-generated retry.
        let (previous, terminal, collected) = legacy_pair();
        assert!(legacy_retry_terminal_collection_matches(
            &previous,
            &terminal,
            &collected,
            "parent-thread"
        ));

        let mut missing_wait = collected.clone();
        let review = missing_wait.units[0].review_state.as_mut().unwrap();
        review.active_attempt.as_mut().unwrap().wait_call_id = None;
        assert!(!legacy_retry_terminal_collection_matches(
            &previous,
            &terminal,
            &missing_wait,
            "parent-thread"
        ));

        let mut changed_wait_ref = collected.clone();
        changed_wait_ref.units[0]
            .review_state
            .as_mut()
            .unwrap()
            .active_attempt
            .as_mut()
            .unwrap()
            .wait_result_ref = Some("d".repeat(64));
        assert!(!legacy_retry_terminal_collection_matches(
            &previous,
            &terminal,
            &changed_wait_ref,
            "parent-thread"
        ));

        let mut changed_attempt = collected.clone();
        changed_attempt.units[0]
            .review_state
            .as_mut()
            .unwrap()
            .active_attempt
            .as_mut()
            .unwrap()
            .attempt_id = "different-attempt".into();
        assert!(!legacy_retry_terminal_collection_matches(
            &previous,
            &terminal,
            &changed_attempt,
            "parent-thread"
        ));

        let mut extra_change = collected.clone();
        extra_change.next_action = Some("unrelated mutation".into());
        assert!(!legacy_retry_terminal_collection_matches(
            &previous,
            &terminal,
            &extra_change,
            "parent-thread"
        ));

        let mut wrong_parent = terminal.clone();
        wrong_parent.units[0]
            .review_state
            .as_mut()
            .unwrap()
            .active_attempt
            .as_mut()
            .unwrap()
            .terminal_source
            .as_mut()
            .unwrap()
            .event
            .as_mut()
            .unwrap()
            .owner_parent_thread_id = Some("other-parent".into());
        assert!(!legacy_retry_terminal_collection_matches(
            &previous,
            &wrong_parent,
            &collected,
            "parent-thread"
        ));

        let mut no_bound_predecessor = previous;
        no_bound_predecessor.units[0]
            .review_state
            .as_mut()
            .unwrap()
            .active_attempt
            .as_mut()
            .unwrap()
            .status = CompletionReviewAttemptStatus::Reserved;
        assert!(!legacy_retry_terminal_collection_matches(
            &no_bound_predecessor,
            &terminal,
            &collected,
            "parent-thread"
        ));
    }
}
