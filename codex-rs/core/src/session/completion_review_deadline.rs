// Modified by RenCrow Switch Core, 2026-11-16.
use super::session::Session;
use crate::agent::CompletionReviewAbort;
use codex_history::task_context::BlockerCategory;
use codex_history::task_context::CompletionBlocker;
use codex_history::task_context::CompletionReviewAttemptStatus;
use codex_history::task_context::CompletionReviewEvidence;

struct CompletionReviewDeadlineOutcome<'a> {
    status: Option<UnitStatus>,
    escalation: Option<String>,
    safety_fingerprint: Option<String>,
    failure_cause: Option<&'a str>,
    terminal_proof: Option<codex_thread_store::PersistedTurnAbortEvidence>,
}
use codex_history::task_context::CompletionReviewHistoricalFailure;
use codex_history::task_context::CompletionReviewRecordLocator;
use codex_history::task_context::CompletionReviewTerminalEvent;
use codex_history::task_context::CompletionReviewTerminalSource;
use codex_history::task_context::CompletionReviewTerminalStatus;
use codex_history::task_context::CompletionUnit;
use codex_history::task_context::CompletionUnitKind;
use codex_history::task_context::FailedApproach;
use codex_history::task_context::UnitStatus;
use codex_protocol::ThreadId;

pub(super) fn terminal_source_from_abort(
    proof: &codex_thread_store::PersistedTurnAbortEvidence,
) -> Option<CompletionReviewTerminalSource> {
    Some(CompletionReviewTerminalSource {
        status: CompletionReviewTerminalStatus::Failed,
        event: Some(CompletionReviewTerminalEvent {
            rollout_id: proof.source.rollout_id.to_string(),
            locator: match &proof.source.locator {
                codex_thread_store::PersistedTurnAbortLocator::Ordinal(value) => {
                    CompletionReviewRecordLocator::Ordinal(*value)
                }
                codex_thread_store::PersistedTurnAbortLocator::LegacyLineIndex(value) => {
                    CompletionReviewRecordLocator::LegacyLineIndex(*value)
                }
            },
            owner_thread_id: proof.owner.thread_id.to_string(),
            turn_id: proof.event.turn_id.clone()?,
            owner_parent_thread_id: proof
                .owner
                .parent_thread_id
                .as_ref()
                .map(ToString::to_string),
            owner_agent_role: proof.owner.agent_role.clone(),
            reason: format!("{:?}", &proof.event.reason),
        }),
    })
}
use std::collections::HashMap;

fn completion_review_safety_fingerprint(value: serde_json::Value) -> String {
    codex_history::compaction_candidate::digest(&value).unwrap_or_else(|_| "0".repeat(64))
}

pub(super) fn review_ended_durable_fingerprint(turn_id: &str) -> String {
    completion_review_safety_fingerprint(serde_json::json!({
        "turn_id": turn_id,
        "result": "review-ended-durable",
    }))
}

pub(super) fn has_host_terminal_review_failure(
    unit: &CompletionUnit,
    review: &CompletionReviewEvidence,
) -> bool {
    unit.status == UnitStatus::Failed
        && unit.kind == CompletionUnitKind::Review
        && (review.terminal_status == Some(CompletionReviewTerminalStatus::Failed)
            || has_persisted_review_ended_outcome(unit, review))
        && review.deadline_unix_ms.is_some()
        && !review.reviewer_turn_id.trim().is_empty()
        && !review.reviewer_turn_id.starts_with("pending-")
        && ThreadId::try_from(review.reviewer_thread_id.as_str()).is_ok()
        && review.wait_result_ref.is_none()
        && review.wait_call_id.is_none()
        && (unit.review_state.as_ref().is_some_and(|state| {
            state.active_attempt.as_ref().is_some_and(|attempt| {
                attempt.status == CompletionReviewAttemptStatus::Failed
                    && attempt.attempt_id == review.spawn_call_id
                    && attempt.reviewer_thread_id.as_deref()
                        == Some(review.reviewer_thread_id.as_str())
                    && attempt.reviewer_turn_id.as_deref() == Some(review.reviewer_turn_id.as_str())
                    && attempt.terminal_source.as_ref().is_some_and(|source| {
                        source.status == CompletionReviewTerminalStatus::Failed
                    })
            })
        }) || has_persisted_review_ended_outcome(unit, review))
}

fn has_persisted_review_ended_outcome(
    unit: &CompletionUnit,
    review: &CompletionReviewEvidence,
) -> bool {
    unit.status == UnitStatus::Failed
        && review.deadline_escalation.is_some()
        && review.deadline_safety_fingerprint.as_deref()
            == Some(review_ended_durable_fingerprint(&review.reviewer_turn_id).as_str())
}

impl Session {
    pub(crate) async fn has_pending_completion_review_deadline(&self) -> bool {
        let pending = {
            let state = self.state.lock().await;
            let Some((_, contract)) = state.rencrow_completion_state.as_ref() else {
                return false;
            };
            let Ok(units) = super::completion_review::projected_completion_review_units(contract)
            else {
                return true;
            };
            units
                .iter()
                .filter_map(|unit| {
                    let review = unit.review_evidence.as_ref()?;
                    (review.deadline_unix_ms.is_some()
                        && review.wait_result_ref.is_none()
                        && !has_host_terminal_review_failure(unit, review)
                        && !has_persisted_review_ended_outcome(unit, review))
                    .then(|| {
                        (
                            review.reviewer_thread_id.clone(),
                            review.reviewer_turn_id.clone(),
                        )
                    })
                })
                .collect::<Vec<_>>()
        };
        for (reviewer_thread_id, expected_turn_id) in pending {
            let Ok(reviewer_thread_id) = ThreadId::try_from(reviewer_thread_id.as_str()) else {
                return true;
            };
            let Ok((turn_id, status)) = self
                .services
                .agent_control
                .get_agent_status_generation(reviewer_thread_id)
                .await
            else {
                return true;
            };
            if turn_id.as_deref() != Some(expected_turn_id.as_str())
                || !crate::agent::status::is_final(&status)
            {
                return true;
            }
        }
        false
    }

    pub(crate) async fn next_completion_review_deadline(
        &self,
        reviewer_ids: &[ThreadId],
    ) -> Option<i64> {
        let reviewer_ids = reviewer_ids
            .iter()
            .map(ToString::to_string)
            .collect::<std::collections::HashSet<_>>();
        let state = self.state.lock().await;
        let (_, contract) = state.rencrow_completion_state.as_ref()?;
        let units = super::completion_review::projected_completion_review_units(contract).ok()?;
        units
            .iter()
            .filter(|unit| {
                unit.review_evidence
                    .as_ref()
                    .is_some_and(|review| !has_host_terminal_review_failure(unit, review))
            })
            .filter_map(|unit| unit.review_evidence.as_ref())
            .filter(|review| {
                reviewer_ids.contains(&review.reviewer_thread_id)
                    && review.deadline_escalation.is_none()
                    && review.wait_result_ref.is_none()
            })
            .filter_map(|review| review.deadline_unix_ms)
            .min()
    }

    /// Project a deadline only onto the exact reviewer generation that owns it.
    pub(crate) async fn completion_review_deadline_for_turn(
        &self,
        reviewer: ThreadId,
        reviewer_turn_id: &str,
    ) -> Option<i64> {
        let reviewer = reviewer.to_string();
        let state = self.state.lock().await;
        let (_, contract) = state.rencrow_completion_state.as_ref()?;
        let units = super::completion_review::projected_completion_review_units(contract).ok()?;
        units
            .iter()
            .filter(|unit| {
                unit.review_evidence
                    .as_ref()
                    .is_some_and(|review| !has_host_terminal_review_failure(unit, review))
            })
            .filter_map(|unit| unit.review_evidence.as_ref())
            .filter(|review| {
                review.reviewer_thread_id == reviewer
                    && review.reviewer_turn_id == reviewer_turn_id
                    && review.deadline_escalation.is_none()
                    && review.wait_result_ref.is_none()
            })
            .filter_map(|review| review.deadline_unix_ms)
            .min()
    }

    pub(crate) async fn completion_review_deadline_escalations(
        &self,
        reviewer_ids: &[ThreadId],
    ) -> HashMap<ThreadId, (String, String)> {
        let reviewer_ids = reviewer_ids
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>();
        let state = self.state.lock().await;
        let Some((_, contract)) = state.rencrow_completion_state.as_ref() else {
            return HashMap::new();
        };
        let Ok(units) = super::completion_review::projected_completion_review_units(contract)
        else {
            return HashMap::new();
        };
        units
            .iter()
            .filter_map(|unit| unit.review_evidence.as_ref())
            .filter_map(|review| {
                let thread_id = ThreadId::try_from(review.reviewer_thread_id.as_str()).ok()?;
                if !reviewer_ids.contains(&thread_id) {
                    return None;
                }
                review
                    .deadline_escalation
                    .as_ref()
                    .map(|reason| (thread_id, (review.reviewer_turn_id.clone(), reason.clone())))
            })
            .collect()
    }

    pub(crate) async fn expired_completion_review_generations(
        &self,
        reviewer_thread_id: ThreadId,
    ) -> Vec<String> {
        let now = chrono::Utc::now().timestamp_millis();
        let reviewer_thread_id = reviewer_thread_id.to_string();
        let state = self.state.lock().await;
        let Some((_, contract)) = state.rencrow_completion_state.as_ref() else {
            return Vec::new();
        };
        let Ok(units) = super::completion_review::projected_completion_review_units(contract)
        else {
            return Vec::new();
        };
        units
            .iter()
            .filter_map(|unit| unit.review_evidence.as_ref())
            .filter(|review| {
                review.reviewer_thread_id == reviewer_thread_id
                    && review.deadline_unix_ms.is_some()
                    && review.wait_result_ref.is_none()
                    && (review
                        .deadline_unix_ms
                        .is_some_and(|deadline| deadline <= now)
                        || review.deadline_escalation.is_some())
            })
            .map(|review| review.reviewer_turn_id.clone())
            .collect()
    }

    pub(crate) async fn enforce_due_completion_review_deadlines(&self) {
        let now = chrono::Utc::now().timestamp_millis();
        let due = {
            let state = self.state.lock().await;
            let Some((_, contract)) = state.rencrow_completion_state.as_ref() else {
                return;
            };
            let Ok(units) = super::completion_review::projected_completion_review_units(contract)
            else {
                return;
            };
            units
                .iter()
                .filter_map(|unit| {
                    let review = unit.review_evidence.as_ref()?;
                    let deadline = review.deadline_unix_ms?;
                    (deadline <= now
                        && review.wait_result_ref.is_none()
                        && !has_host_terminal_review_failure(unit, review)
                        && !has_persisted_review_ended_outcome(unit, review))
                    .then(|| (unit.id.clone(), review.clone()))
                })
                .collect::<Vec<_>>()
        };

        for (unit_id, review) in due {
            let result = if review.reviewer_turn_id.starts_with("pending-generation:") {
                CompletionReviewAbort::Unsafe {
                    reason: "the host has not bound an exact reviewer generation".into(),
                    fingerprint: completion_review_safety_fingerprint(serde_json::json!({
                        "reason": "generation-unbound",
                        "reviewer_thread_id": review.reviewer_thread_id,
                        "spawn_call_id": review.spawn_call_id,
                    })),
                }
            } else if let (Ok(thread_id), Some(role)) = (
                ThreadId::try_from(review.reviewer_thread_id.as_str()),
                Some(review.reviewer_role.as_str()),
            ) {
                self.services
                    .agent_control
                    .abort_completion_review_turn_if_model_only(
                        self.thread_id,
                        thread_id,
                        &review.reviewer_turn_id,
                        role,
                        review.deadline_safety_fingerprint.as_deref(),
                    )
                    .await
            } else {
                CompletionReviewAbort::Unsafe {
                    reason: "reviewer identity is invalid".into(),
                    fingerprint: completion_review_safety_fingerprint(serde_json::json!({
                        "reason": "reviewer-identity-invalid",
                        "reviewer_thread_id": review.reviewer_thread_id,
                        "reviewer_role": review.reviewer_role,
                    })),
                }
            };

            match result {
                CompletionReviewAbort::AbortedAndObserved(proof) => {
                    self.record_completion_review_deadline_outcome(
                        &unit_id,
                        &review,
                        CompletionReviewDeadlineOutcome {
                            status: Some(UnitStatus::Failed),
                            escalation: Some("reviewer exceeded the host SLA; its exact model-only turn was stopped and the ReviewEnded terminal status and rollout event were observed".into()),
                            safety_fingerprint: Some(review_ended_durable_fingerprint(&review.reviewer_turn_id)),
                            failure_cause: Some("reviewer exceeded the host SLA"),
                            terminal_proof: Some(proof),
                        },
                    )
                    .await;
                }
                CompletionReviewAbort::AlreadyTerminal => {}
                CompletionReviewAbort::Unsafe {
                    reason,
                    fingerprint,
                } => {
                    self.record_completion_review_deadline_outcome(
                        &unit_id,
                        &review,
                        CompletionReviewDeadlineOutcome {
                            status: None,
                            escalation: Some(format!(
                                "reviewer deadline expired; cancellation was withheld: {reason}"
                            )),
                            safety_fingerprint: Some(fingerprint),
                            failure_cause: None,
                            terminal_proof: None,
                        },
                    )
                    .await;
                }
            }
        }
    }

    async fn record_completion_review_deadline_outcome(
        &self,
        unit_id: &str,
        expected_review: &CompletionReviewEvidence,
        outcome: CompletionReviewDeadlineOutcome<'_>,
    ) {
        let CompletionReviewDeadlineOutcome {
            status,
            escalation,
            safety_fingerprint,
            failure_cause,
            terminal_proof,
        } = outcome;
        let _review_guard = self.completion_review_owner_guard().await;
        let changed = {
            let mut state = self.state.lock().await;
            let Some((_, contract)) = state.rencrow_completion_state.as_mut() else {
                return;
            };
            let Some(unit) = contract.units.iter_mut().find(|unit| unit.id == unit_id) else {
                return;
            };
            if unit.kind != CompletionUnitKind::Review {
                return;
            }
            if unit.review_state.is_some() {
                let Ok(projected) =
                    codex_history::task_context::project_completion_review_state(unit)
                else {
                    return;
                };
                *unit = projected;
            }
            if unit.status == UnitStatus::Done {
                return;
            }
            let current_matches = unit.review_evidence.as_ref().is_some_and(|review| {
                review.reviewer_thread_id == expected_review.reviewer_thread_id
                    && review.reviewer_turn_id == expected_review.reviewer_turn_id
                    && review.spawn_call_id == expected_review.spawn_call_id
                    && review.deadline_unix_ms == expected_review.deadline_unix_ms
            });

            if !current_matches {
                let Some(proof) = terminal_proof.as_ref() else {
                    return;
                };
                if proof.event.turn_id.as_deref() != Some(expected_review.reviewer_turn_id.as_str())
                    || proof.event.reason != codex_protocol::protocol::TurnAbortReason::ReviewEnded
                    || proof.owner.thread_id.to_string() != expected_review.reviewer_thread_id
                    || proof.owner.parent_thread_id != Some(self.thread_id)
                    || proof.owner.agent_role.as_deref()
                        != Some(expected_review.reviewer_role.as_str())
                {
                    return;
                }
                let Some(review_state) = unit.review_state.as_mut() else {
                    return;
                };
                let Some(terminal_source) = terminal_source_from_abort(proof) else {
                    return;
                };
                let failure = CompletionReviewHistoricalFailure {
                    attempt_id: expected_review.spawn_call_id.clone(),
                    contract_revision: None,
                    imported_from: None,
                    consumed_retry_authorization: None,
                    reviewer_thread_id: expected_review.reviewer_thread_id.clone(),
                    reviewer_turn_id: expected_review.reviewer_turn_id.clone(),
                    target_task_id: expected_review.target_task_id.clone(),
                    target_task_revision: expected_review.target_task_revision,
                    target_artifact_ref: expected_review.target_artifact_ref.clone(),
                    reviewer_role: expected_review.reviewer_role.clone(),
                    reviewer_model: None,
                    deadline_unix_ms: expected_review.deadline_unix_ms,
                    wait_result_ref: expected_review.wait_result_ref.clone(),
                    wait_call_id: expected_review.wait_call_id.clone(),
                    deadline_escalation: expected_review.deadline_escalation.clone(),
                    deadline_safety_fingerprint: expected_review
                        .deadline_safety_fingerprint
                        .clone(),
                    terminal_source,
                };
                match review_state.historical_failure.as_mut() {
                    Some(existing)
                        if existing.attempt_id == failure.attempt_id
                            && existing.reviewer_thread_id == failure.reviewer_thread_id
                            && existing.reviewer_turn_id == failure.reviewer_turn_id =>
                    {
                        if existing.terminal_source.event.is_none() {
                            existing.terminal_source = failure.terminal_source;
                        }
                    }
                    None => review_state.historical_failure = Some(failure),
                    Some(_) => return,
                }
                contract.activity_at = Some(chrono::Utc::now().timestamp());
                true
            } else {
                let Some(review) = unit.review_evidence.as_mut() else {
                    return;
                };
                if let Some(proof) = terminal_proof.as_ref()
                    && (proof.event.turn_id.as_deref()
                        != Some(expected_review.reviewer_turn_id.as_str())
                        || proof.event.reason
                            != codex_protocol::protocol::TurnAbortReason::ReviewEnded
                        || proof.owner.thread_id.to_string() != expected_review.reviewer_thread_id
                        || proof.owner.parent_thread_id != Some(self.thread_id)
                        || proof.owner.agent_role.as_deref()
                            != Some(expected_review.reviewer_role.as_str()))
                {
                    return;
                }
                if review.deadline_safety_fingerprint == safety_fingerprint && status.is_none() {
                    return;
                }
                let active_is_exact = unit
                    .review_state
                    .as_ref()
                    .and_then(|review_state| review_state.active_attempt.as_ref())
                    .is_some_and(|attempt| {
                        attempt.attempt_id == expected_review.spawn_call_id
                            && attempt.reviewer_thread_id.as_deref()
                                == Some(expected_review.reviewer_thread_id.as_str())
                            && attempt.reviewer_turn_id.as_deref()
                                == Some(expected_review.reviewer_turn_id.as_str())
                    });
                if active_is_exact
                    && unit
                        .review_state
                        .as_ref()
                        .and_then(|review_state| review_state.active_attempt.as_ref())
                        .and_then(|attempt| attempt.terminal_source.as_ref())
                        .is_some_and(|source| {
                            source.status != CompletionReviewTerminalStatus::Failed
                        })
                {
                    return;
                }
                let effective_safety_fingerprint = safety_fingerprint
                    .clone()
                    .or_else(|| review.deadline_safety_fingerprint.clone());
                if let Some(status) = status
                    && (unit.review_state.is_none() || active_is_exact)
                {
                    unit.status = status;
                }
                if let Some(attempt) = unit
                    .review_state
                    .as_mut()
                    .and_then(|review_state| review_state.active_attempt.as_mut())
                    .filter(|_| active_is_exact)
                {
                    attempt.deadline_escalation = escalation.clone();
                    attempt.deadline_safety_fingerprint = effective_safety_fingerprint.clone();
                }
                if let Some(proof) = terminal_proof.as_ref() {
                    let Some(terminal_source) = terminal_source_from_abort(proof) else {
                        return;
                    };
                    if let Some(review_state) = unit.review_state.as_mut() {
                        if active_is_exact {
                            if let Some(attempt) = review_state.active_attempt.as_mut() {
                                attempt.status = CompletionReviewAttemptStatus::Failed;
                                if attempt.terminal_source.as_ref().is_none_or(|source| {
                                    source.status == CompletionReviewTerminalStatus::Failed
                                        && source.event.is_none()
                                }) {
                                    attempt.terminal_source = Some(terminal_source);
                                }
                            }
                        } else {
                            let failure = CompletionReviewHistoricalFailure {
                                attempt_id: expected_review.spawn_call_id.clone(),
                                contract_revision: None,
                                imported_from: None,
                                consumed_retry_authorization: None,
                                reviewer_thread_id: expected_review.reviewer_thread_id.clone(),
                                reviewer_turn_id: expected_review.reviewer_turn_id.clone(),
                                target_task_id: expected_review.target_task_id.clone(),
                                target_task_revision: expected_review.target_task_revision,
                                target_artifact_ref: expected_review.target_artifact_ref.clone(),
                                reviewer_role: expected_review.reviewer_role.clone(),
                                reviewer_model: None,
                                deadline_unix_ms: expected_review.deadline_unix_ms,
                                wait_result_ref: expected_review.wait_result_ref.clone(),
                                wait_call_id: expected_review.wait_call_id.clone(),
                                deadline_escalation: expected_review.deadline_escalation.clone(),
                                deadline_safety_fingerprint: expected_review
                                    .deadline_safety_fingerprint
                                    .clone(),
                                terminal_source,
                            };
                            match review_state.historical_failure.as_mut() {
                                Some(existing)
                                    if existing.attempt_id == failure.attempt_id
                                        && existing.reviewer_thread_id
                                            == failure.reviewer_thread_id
                                        && existing.reviewer_turn_id
                                            == failure.reviewer_turn_id =>
                                {
                                    if existing.terminal_source.event.is_none() {
                                        existing.terminal_source = failure.terminal_source;
                                    }
                                }
                                None => review_state.historical_failure = Some(failure),
                                Some(_) => {}
                            }
                        }
                    }
                    if unit.review_state.is_none() || active_is_exact {
                        review.terminal_status = Some(CompletionReviewTerminalStatus::Failed);
                    }
                }
                if escalation.is_some() {
                    review.deadline_escalation = escalation.clone();
                }
                review.deadline_safety_fingerprint = effective_safety_fingerprint.clone();
                let escalation = review.deadline_escalation.clone().or(escalation);
                let Some(escalation) = escalation else {
                    return;
                };
                let review_owner = if active_is_exact {
                    effective_safety_fingerprint.map(|safety_fingerprint| {
                        codex_history::task_context::CompletionReviewBlockerOwner {
                            unit_id: unit_id.to_string(),
                            attempt_id: expected_review.spawn_call_id.clone(),
                            safety_fingerprint,
                        }
                    })
                } else {
                    None
                };
                contract.next_action = Some(
                "A reviewer exceeded its host deadline. Do not wait for or resubmit the same reviewer; inspect the persisted escalation and decide whether a new host-authorized review is needed.".into(),
            );
                if let Some(blocker) = contract.blocker.as_mut() {
                    if review_owner.as_ref().is_some_and(|owner| {
                        blocker.review_owner.as_ref().is_some_and(|existing| {
                            existing.unit_id == owner.unit_id
                                && existing.attempt_id == owner.attempt_id
                        })
                    }) {
                        blocker.reason = escalation;
                        blocker.review_owner = review_owner;
                    }
                } else {
                    contract.blocker = Some(CompletionBlocker {
                        category: BlockerCategory::Unknown,
                        reason: escalation,
                        required_input: None,
                        authority: None,
                        evidence: Vec::new(),
                        recheck_condition: None,
                        review_owner,
                    });
                }
                if let Some(cause) = failure_cause
                    && !contract.failed_approaches.iter().any(|approach| {
                        approach.summary == format!("review unit {} failed", unit.id)
                            && approach.cause == cause
                    })
                {
                    contract.failed_approaches.push(FailedApproach {
                        summary: format!("review unit {} failed", unit.id),
                        cause: cause.into(),
                        next_action: "Diagnose the reviewer timeout before creating another authorized review plan".into(),
                        recorded_at: chrono::Utc::now().timestamp(),
                    });
                }
                if unit.review_state.is_some()
                    && let Ok(projected) =
                        codex_history::task_context::project_completion_review_state(unit)
                {
                    *unit = projected;
                }
                true
            }
        };
        if changed && self.persist_autonomous_completion_snapshot().await.is_err() {
            self.pause_autonomous_completion().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::input_queue::TurnInput;
    use crate::session::tests::make_session_and_context;
    use crate::session::turn_context::TurnContext;
    use crate::state::TaskKind;
    use crate::tasks::SessionTask;
    use crate::tasks::SessionTaskResult;
    use codex_history::task_context::AcceptanceCondition;
    use codex_history::task_context::AcceptanceStatus;
    use codex_history::task_context::CompletionReviewAttempt;
    use codex_history::task_context::CompletionReviewAttemptStatus;
    use codex_history::task_context::CompletionReviewEvidence;
    use codex_history::task_context::CompletionReviewRecordLocator;
    use codex_history::task_context::CompletionReviewState;
    use codex_history::task_context::CompletionReviewTarget;
    use codex_history::task_context::CompletionReviewTerminalEvent;
    use codex_history::task_context::CompletionReviewTerminalSource;
    use codex_history::task_context::CompletionReviewTerminalStatus;
    use codex_history::task_context::CompletionState;
    use codex_history::task_context::CompletionUnit;
    use codex_history::task_context::CompletionUnitKind;
    use codex_history::task_context::UnitStatus;
    use codex_protocol::error::CodexErr;
    use codex_protocol::protocol::AgentStatus;
    use codex_protocol::protocol::TurnAbortReason;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    struct WaitingTask;

    impl SessionTask for WaitingTask {
        fn kind(&self) -> TaskKind {
            TaskKind::Regular
        }

        fn span_name(&self) -> &'static str {
            "completion_review_deadline_test"
        }

        async fn run(
            self: Arc<Self>,
            _session: Arc<Session>,
            _ctx: Arc<TurnContext>,
            _input: Vec<TurnInput>,
            cancellation_token: CancellationToken,
        ) -> SessionTaskResult {
            cancellation_token.cancelled().await;
            Err(CodexErr::TurnAborted)
        }
    }

    #[tokio::test]
    async fn exact_model_only_abort_waits_for_queued_tool_guard_to_drop() {
        let (session, turn) = make_session_and_context().await;
        let session = Arc::new(session);
        let turn = Arc::new(turn);
        session
            .start_task(turn.clone(), Vec::new(), WaitingTask)
            .await;
        let generation = turn.sub_id.clone();
        let wrong_generation = format!("{generation}-other");
        let (inventory_epoch, pending_tools) = session
            .reviewer_tool_dispatch_snapshot(&generation)
            .await
            .expect("the exact regular generation is active");
        assert_eq!(pending_tools, 0);
        assert!(
            session
                .register_in_flight_tool_call(&wrong_generation)
                .await
                .is_none()
        );
        let (queued_tool, queued_epoch) = session
            .register_in_flight_tool_call(&generation)
            .await
            .expect("OutputItemDone registration retains the future before poll");
        assert!(queued_epoch > inventory_epoch);
        assert!(
            !session
                .abort_model_only_turn_if_active(
                    &generation,
                    queued_epoch,
                    TurnAbortReason::ReviewEnded,
                )
                .await
                .expect("queued tool must prevent abort")
        );
        assert!(
            !session
                .abort_model_only_turn_if_active(
                    &wrong_generation,
                    queued_epoch,
                    TurnAbortReason::ReviewEnded,
                )
                .await
                .expect("a different generation must not be stopped")
        );

        drop(queued_tool);
        assert!(
            !session
                .abort_model_only_turn_if_active(
                    &generation,
                    inventory_epoch,
                    TurnAbortReason::ReviewEnded,
                )
                .await
                .expect("a completed tool still invalidates the earlier inventory snapshot")
        );
        let (current_epoch, pending_tools) = session
            .reviewer_tool_dispatch_snapshot(&generation)
            .await
            .expect("the exact regular generation remains active");
        assert_eq!(pending_tools, 0);
        assert!(
            session
                .abort_model_only_turn_if_active(
                    &generation,
                    current_epoch,
                    TurnAbortReason::ReviewEnded,
                )
                .await
                .expect("model-only generation can be stopped after its tool future is gone")
        );
        let (observed_generation, status) = session.agent_status_generation().await;
        assert_eq!(observed_generation.as_deref(), Some(generation.as_str()));
        assert!(matches!(status, AgentStatus::Errored(reason) if reason == "ReviewEnded"));
    }

    #[tokio::test]
    async fn host_failed_review_is_not_rescheduled_but_remains_in_generation_audit() {
        let (session, first) = escalated_unknown_reviewer().await;
        let mut contract = first;
        let contract_revision = contract.contract_revision;
        let failed_condition_refs = contract.units[0].condition_refs.clone();
        let failed_conditions_sha256 =
            codex_history::task_context::completion_condition_definitions_sha256(
                &contract,
                &failed_condition_refs,
            )
            .expect("fixture review condition exists");
        let old_review = {
            let unit = &mut contract.units[0];
            unit.status = UnitStatus::Failed;
            let evidence = unit.review_evidence.as_mut().unwrap();
            evidence.deadline_escalation = None;
            evidence.deadline_safety_fingerprint = None;
            evidence.wait_result_ref = None;
            evidence.terminal_status = None;
            evidence.wait_call_id = None;
            let failed_source = CompletionReviewTerminalSource {
                status: CompletionReviewTerminalStatus::Failed,
                event: Some(CompletionReviewTerminalEvent {
                    rollout_id: "rollout-review-1".into(),
                    locator: CompletionReviewRecordLocator::Ordinal(7),
                    owner_thread_id: evidence.reviewer_thread_id.clone(),
                    turn_id: evidence.reviewer_turn_id.clone(),
                    owner_parent_thread_id: Some(session.thread_id.to_string()),
                    owner_agent_role: Some(evidence.reviewer_role.clone()),
                    reason: "ReviewEnded".into(),
                }),
            };
            let target = unit.review_target.as_ref().unwrap().clone();
            unit.review_state = Some(CompletionReviewState {
                version: 1,
                obligation_unit_id: unit.id.clone(),
                active_attempt: Some(CompletionReviewAttempt {
                    attempt_id: evidence.spawn_call_id.clone(),
                    contract_revision,
                    condition_refs: unit.condition_refs.clone(),
                    conditions_sha256: failed_conditions_sha256,
                    status: CompletionReviewAttemptStatus::Failed,
                    launch_unknown_reason: None,
                    consumed_retry_authorization: None,
                    imported_from: None,
                    reviewer_thread_id: Some(evidence.reviewer_thread_id.clone()),
                    reviewer_turn_id: Some(evidence.reviewer_turn_id.clone()),
                    reviewer_role: evidence.reviewer_role.clone(),
                    reviewer_model: Some("review-model".into()),
                    target_task_id: target.task_id,
                    target_task_revision: target.task_revision,
                    target_artifact_ref: evidence.target_artifact_ref.clone(),
                    deadline_unix_ms: evidence.deadline_unix_ms,
                    deadline_escalation: None,
                    deadline_safety_fingerprint: None,
                    terminal_source: Some(failed_source),
                    wait_result_ref: None,
                    wait_call_id: None,
                    adoption_reason: None,
                }),
                historical_failure: None,
                retry_reservation_prior_historical_failure: None,
                previous_attempt_ref: None,
                retry_authorization: None,
                withdrawal: None,
                revalidation: None,
            });
            evidence.terminal_status = Some(CompletionReviewTerminalStatus::Failed);
            evidence.clone()
        };
        let mut status_only = contract.clone();
        status_only.units[0].review_state = None;
        status_only.units[0]
            .review_evidence
            .as_mut()
            .unwrap()
            .terminal_status = None;
        session.state.lock().await.rencrow_completion_state = Some(("root".into(), status_only));
        assert!(
            session.has_pending_completion_review_deadline().await,
            "a compatibility Failed status without a terminal proof remains unresolved"
        );

        let mut retry = contract.units[0].clone();
        retry.id = "review-2".into();
        retry.purpose = "Retry after the terminal provider failure".into();
        retry.status = UnitStatus::Planned;
        retry.review_evidence = None;
        retry.review_state = None;
        contract.units.push(retry);
        contract.blocker = None;
        session.state.lock().await.rencrow_completion_state = Some(("root".into(), contract));

        let reviewer = ThreadId::try_from(old_review.reviewer_thread_id.as_str()).unwrap();
        assert!(!session.has_pending_completion_review_deadline().await);
        assert_eq!(
            session.next_completion_review_deadline(&[reviewer]).await,
            None
        );
        assert_eq!(
            session
                .completion_review_deadline_for_turn(reviewer, &old_review.reviewer_turn_id)
                .await,
            None
        );
        assert!(
            session
                .completion_review_deadline_escalations(&[reviewer])
                .await
                .is_empty()
        );
        assert_eq!(
            session
                .expired_completion_review_generations(reviewer)
                .await,
            vec![old_review.reviewer_turn_id.clone()],
            "expired-generation audit and same-turn submission rejection remain intact"
        );

        session.enforce_due_completion_review_deadlines().await;
        let after = session.rencrow_completion_state("root").await.unwrap();
        assert_eq!(after.units[0].status, UnitStatus::Failed);
        assert_eq!(after.units[0].review_evidence.as_ref(), Some(&old_review));
        assert_eq!(after.units[1].status, UnitStatus::Planned);
        assert!(after.blocker.is_none());
    }

    async fn escalated_unknown_reviewer() -> (Session, CompletionState) {
        let (session, _) = make_session_and_context().await;
        let reviewer_thread_id = "01a1c606-48b8-7cf3-bb78-77b47f6b8c2f";
        let state = CompletionState {
            contract_revision: 1,
            units_completed_since_review: 0,
            plan_review_at: None,
            plan_reviews: Vec::new(),
            paused: false,
            consumed_host_events: Vec::new(),
            conditions: vec![AcceptanceCondition {
                id: "AC-1".into(),
                revision: 1,
                result: "Review the accepted artifact".into(),
                scope: "CLI".into(),
                verification: "Inspect the accepted artifact".into(),
                acceptance_criteria: "The reviewer reports findings".into(),
                status: AcceptanceStatus::Open,
                evidence: Vec::new(),
                unmet_reason: None,
            }],
            units: vec![CompletionUnit {
                id: "review-1".into(),
                purpose: "Review task-a".into(),
                status: UnitStatus::Running,
                kind: CompletionUnitKind::Review,
                condition_refs: vec!["AC-1".into()],
                depends_on: Vec::new(),
                evidence: Vec::new(),
                task_refs: Vec::new(),
                review_target: Some(CompletionReviewTarget {
                    task_id: "task-a".into(),
                    task_revision: 1,
                }),
                review_evidence: Some(CompletionReviewEvidence {
                    reviewer_thread_id: reviewer_thread_id.into(),
                    reviewer_turn_id: "reviewer-turn-1".into(),
                    reviewer_role: "analyst".into(),
                    spawn_call_id: "host-review-call".into(),
                    deadline_unix_ms: Some(chrono::Utc::now().timestamp_millis() - 1),
                    deadline_escalation: None,
                    deadline_safety_fingerprint: None,
                    wait_result_ref: None,
                    terminal_status: None,
                    wait_call_id: None,
                    target_task_id: "task-a".into(),
                    target_task_revision: 1,
                    target_artifact_ref: "task-check:accepted-artifact".into(),
                }),
                review_launch: None,
                review_decision_reason: None,
                review_state: None,
                review_revalidation_of: None,
            }],
            current_unit: Some("review-1".into()),
            failed_approaches: Vec::new(),
            blocker: None,
            next_action: Some("Wait for the reviewer".into()),
            product_progress_at: None,
            runtime_recovery_at: None,
            activity_at: None,
        };
        session.state.lock().await.rencrow_completion_state = Some(("root".into(), state));

        session.enforce_due_completion_review_deadlines().await;
        let first = session.rencrow_completion_state("root").await.unwrap();
        (session, first)
    }

    #[tokio::test]
    async fn deadline_escalation_preserves_unowned_legacy_and_external_blockers() {
        let (session, first) = escalated_unknown_reviewer().await;
        assert_eq!(first.units[0].status, UnitStatus::Running);
        let evidence = first.units[0].review_evidence.as_ref().unwrap();
        assert!(evidence.wait_result_ref.is_none());
        assert_eq!(evidence.terminal_status, None);
        assert!(evidence.deadline_escalation.is_some());
        assert!(evidence.deadline_safety_fingerprint.is_some());
        assert_eq!(
            first.blocker.as_ref().unwrap().category,
            BlockerCategory::Unknown
        );

        session.enforce_due_completion_review_deadlines().await;
        let second = session.rencrow_completion_state("root").await.unwrap();
        assert_eq!(
            second, first,
            "unchanged unknown state is not escalated twice"
        );
        let old_review = first.units[0].review_evidence.as_ref().unwrap().clone();
        let old_reason = old_review.deadline_escalation.as_deref().unwrap();
        assert_eq!(first.blocker.as_ref().unwrap().reason, old_reason);
        session
            .record_completion_review_deadline_outcome(
                "review-1",
                &old_review,
                CompletionReviewDeadlineOutcome {
                    status: None,
                    escalation: Some("reviewer thread is unavailable".into()),
                    safety_fingerprint: Some(completion_review_safety_fingerprint(
                        serde_json::json!({
                            "cause": "reviewer thread is unavailable",
                        }),
                    )),
                    failure_cause: None,
                    terminal_proof: None,
                },
            )
            .await;
        let refreshed = session.rencrow_completion_state("root").await.unwrap();
        let current_reason = "reviewer thread is unavailable";
        assert_eq!(
            refreshed.units[0]
                .review_evidence
                .as_ref()
                .unwrap()
                .deadline_escalation
                .as_deref(),
            Some(current_reason)
        );
        assert_eq!(
            refreshed.blocker.as_ref().unwrap().reason,
            old_reason,
            "legacy blocker ownership is unknown and must not be inferred from matching text"
        );

        let current_review = refreshed.units[0].review_evidence.as_ref().unwrap().clone();
        session
            .state
            .lock()
            .await
            .rencrow_completion_state
            .as_mut()
            .unwrap()
            .1
            .blocker = Some(CompletionBlocker {
            category: BlockerCategory::Authority,
            reason: current_reason.into(),
            required_input: Some("operator decision".into()),
            authority: Some("operator".into()),
            evidence: Vec::new(),
            recheck_condition: None,
            review_owner: None,
        });
        session
            .record_completion_review_deadline_outcome(
                "review-1",
                &current_review,
                CompletionReviewDeadlineOutcome {
                    status: None,
                    escalation: Some("reviewer has a different current cause".into()),
                    safety_fingerprint: Some(completion_review_safety_fingerprint(
                        serde_json::json!({
                            "cause": "reviewer has a different current cause",
                        }),
                    )),
                    failure_cause: None,
                    terminal_proof: None,
                },
            )
            .await;
        let after_external = session.rencrow_completion_state("root").await.unwrap();
        assert_eq!(
            after_external.units[0]
                .review_evidence
                .as_ref()
                .unwrap()
                .deadline_escalation
                .as_deref(),
            Some("reviewer has a different current cause")
        );
        assert_eq!(
            after_external.blocker.as_ref().unwrap().reason,
            current_reason
        );
        assert_eq!(
            after_external
                .blocker
                .as_ref()
                .unwrap()
                .authority
                .as_deref(),
            Some("operator")
        );
    }

    #[tokio::test]
    async fn deadline_refreshes_only_the_exact_review_owned_blocker() {
        let (mut session, _) = make_session_and_context().await;
        crate::session::tests::attach_thread_persistence(&mut session).await;
        let reviewer_thread_id = "01a1c606-48b8-7cf3-bb78-77b47f6b8c2f";
        let reviewer_turn_id = "reviewer-turn-owned";
        let target = CompletionReviewTarget {
            task_id: "task-a".into(),
            task_revision: 1,
        };
        let evidence = CompletionReviewEvidence {
            reviewer_thread_id: reviewer_thread_id.into(),
            reviewer_turn_id: reviewer_turn_id.into(),
            reviewer_role: "analyst".into(),
            spawn_call_id: "host-review-call-owned".into(),
            deadline_unix_ms: Some(chrono::Utc::now().timestamp_millis() - 1),
            deadline_escalation: None,
            deadline_safety_fingerprint: None,
            wait_result_ref: None,
            terminal_status: None,
            wait_call_id: None,
            target_task_id: target.task_id.clone(),
            target_task_revision: target.task_revision,
            target_artifact_ref: "task-check:accepted-artifact".into(),
        };
        let unit = CompletionUnit {
            id: "review-owned".into(),
            purpose: "Review task-a".into(),
            status: UnitStatus::Running,
            kind: CompletionUnitKind::Review,
            condition_refs: vec!["AC-1".into()],
            depends_on: Vec::new(),
            evidence: Vec::new(),
            task_refs: Vec::new(),
            review_target: Some(target.clone()),
            review_evidence: Some(evidence.clone()),
            review_launch: None,
            review_decision_reason: None,
            review_state: Some(CompletionReviewState {
                version: 1,
                obligation_unit_id: "review-owned".into(),
                active_attempt: Some(CompletionReviewAttempt {
                    attempt_id: evidence.spawn_call_id.clone(),
                    contract_revision: 1,
                    condition_refs: vec!["AC-1".into()],
                    conditions_sha256: "a".repeat(64),
                    status: CompletionReviewAttemptStatus::Bound,
                    launch_unknown_reason: None,
                    consumed_retry_authorization: None,
                    imported_from: None,
                    reviewer_thread_id: Some(reviewer_thread_id.into()),
                    reviewer_turn_id: Some(reviewer_turn_id.into()),
                    reviewer_role: "analyst".into(),
                    reviewer_model: Some("review-model".into()),
                    target_task_id: target.task_id.clone(),
                    target_task_revision: target.task_revision,
                    target_artifact_ref: evidence.target_artifact_ref.clone(),
                    deadline_unix_ms: evidence.deadline_unix_ms,
                    deadline_escalation: None,
                    deadline_safety_fingerprint: None,
                    terminal_source: None,
                    wait_result_ref: None,
                    wait_call_id: None,
                    adoption_reason: None,
                }),
                historical_failure: None,
                retry_reservation_prior_historical_failure: None,
                previous_attempt_ref: None,
                retry_authorization: None,
                withdrawal: None,
                revalidation: None,
            }),
            review_revalidation_of: None,
        };
        let contract = CompletionState {
            contract_revision: 1,
            units_completed_since_review: 0,
            plan_review_at: None,
            plan_reviews: Vec::new(),
            paused: false,
            consumed_host_events: Vec::new(),
            conditions: vec![AcceptanceCondition {
                id: "AC-1".into(),
                revision: 1,
                result: "Review the accepted artifact".into(),
                scope: "CLI".into(),
                verification: "Inspect the accepted artifact".into(),
                acceptance_criteria: "The reviewer reports findings".into(),
                status: AcceptanceStatus::Open,
                evidence: Vec::new(),
                unmet_reason: None,
            }],
            units: vec![unit],
            current_unit: Some("review-owned".into()),
            failed_approaches: Vec::new(),
            blocker: None,
            next_action: Some("Wait for the reviewer".into()),
            product_progress_at: None,
            runtime_recovery_at: None,
            activity_at: None,
        };
        session.state.lock().await.rencrow_completion_state = Some(("root".into(), contract));

        let first_fingerprint = completion_review_safety_fingerprint(serde_json::json!({
            "cause": "first observed deadline cause",
        }));
        session
            .record_completion_review_deadline_outcome(
                "review-owned",
                &evidence,
                CompletionReviewDeadlineOutcome {
                    status: None,
                    escalation: Some("first deadline cause".into()),
                    safety_fingerprint: Some(first_fingerprint.clone()),
                    failure_cause: None,
                    terminal_proof: None,
                },
            )
            .await;
        let first = session.rencrow_completion_state("root").await.unwrap();
        let first_owner = first
            .blocker
            .as_ref()
            .unwrap()
            .review_owner
            .as_ref()
            .unwrap();
        assert_eq!(first_owner.unit_id, "review-owned");
        assert_eq!(first_owner.attempt_id, evidence.spawn_call_id);
        assert_eq!(first_owner.safety_fingerprint, first_fingerprint);

        let current_review = first.units[0].review_evidence.as_ref().unwrap().clone();
        let second_fingerprint = completion_review_safety_fingerprint(serde_json::json!({
            "cause": "second observed deadline cause",
        }));
        session
            .record_completion_review_deadline_outcome(
                "review-owned",
                &current_review,
                CompletionReviewDeadlineOutcome {
                    status: None,
                    escalation: Some("second deadline cause".into()),
                    safety_fingerprint: Some(second_fingerprint.clone()),
                    failure_cause: None,
                    terminal_proof: None,
                },
            )
            .await;
        let refreshed = session.rencrow_completion_state("root").await.unwrap();
        let refreshed_blocker = refreshed.blocker.as_ref().unwrap();
        assert_eq!(refreshed_blocker.reason, "second deadline cause");
        assert_eq!(
            refreshed_blocker
                .review_owner
                .as_ref()
                .unwrap()
                .safety_fingerprint,
            second_fingerprint
        );
    }
}

#[cfg(test)]
#[path = "completion_review_provenance_tests.rs"]
mod provenance_tests;
