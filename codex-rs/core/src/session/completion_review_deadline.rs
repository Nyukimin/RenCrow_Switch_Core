use super::session::Session;
use crate::agent::CompletionReviewAbort;
use codex_history::task_context::BlockerCategory;
use codex_history::task_context::CompletionBlocker;
use codex_history::task_context::CompletionReviewEvidence;
use codex_history::task_context::CompletionUnitKind;
use codex_history::task_context::FailedApproach;
use codex_history::task_context::UnitStatus;
use codex_protocol::ThreadId;
use std::collections::HashMap;

fn completion_review_safety_fingerprint(value: serde_json::Value) -> String {
    codex_history::compaction_candidate::digest(&value).unwrap_or_else(|_| "0".repeat(64))
}

fn review_ended_durable_fingerprint(turn_id: &str) -> String {
    completion_review_safety_fingerprint(serde_json::json!({
        "turn_id": turn_id,
        "result": "review-ended-durable",
    }))
}

fn has_persisted_review_ended_outcome(
    unit: &codex_history::task_context::CompletionUnit,
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
            state
                .rencrow_completion_state
                .as_ref()
                .into_iter()
                .flat_map(|(_, contract)| contract.units.iter())
                .filter(|unit| unit.kind == CompletionUnitKind::Review)
                .filter_map(|unit| {
                    let review = unit.review_evidence.as_ref()?;
                    (review.deadline_unix_ms.is_some()
                        && review.wait_result_ref.is_none()
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
        state
            .rencrow_completion_state
            .as_ref()?
            .1
            .units
            .iter()
            .filter(|unit| unit.kind == CompletionUnitKind::Review)
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
        state
            .rencrow_completion_state
            .as_ref()?
            .1
            .units
            .iter()
            .filter(|unit| unit.kind == CompletionUnitKind::Review)
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
        contract
            .units
            .iter()
            .filter(|unit| unit.kind == CompletionUnitKind::Review)
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
        state
            .rencrow_completion_state
            .as_ref()
            .into_iter()
            .flat_map(|(_, contract)| contract.units.iter())
            .filter(|unit| unit.kind == CompletionUnitKind::Review)
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
            state
                .rencrow_completion_state
                .as_ref()
                .into_iter()
                .flat_map(|(_, contract)| contract.units.iter())
                .filter(|unit| unit.kind == CompletionUnitKind::Review)
                .filter_map(|unit| {
                    let review = unit.review_evidence.as_ref()?;
                    let deadline = review.deadline_unix_ms?;
                    (deadline <= now
                        && review.wait_result_ref.is_none()
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
                CompletionReviewAbort::AbortedAndObserved => {
                    self.record_completion_review_deadline_outcome(
                        &unit_id,
                        &review,
                        Some(UnitStatus::Failed),
                        Some("reviewer exceeded the host SLA; its exact model-only turn was stopped and the ReviewEnded terminal status and rollout event were observed".into()),
                        Some(review_ended_durable_fingerprint(&review.reviewer_turn_id)),
                        Some("reviewer exceeded the host SLA"),
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
                        None,
                        Some(format!(
                            "reviewer deadline expired; cancellation was withheld: {reason}"
                        )),
                        Some(fingerprint),
                        None,
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
        status: Option<UnitStatus>,
        escalation: Option<String>,
        safety_fingerprint: Option<String>,
        failure_cause: Option<&str>,
    ) {
        let changed = {
            let mut state = self.state.lock().await;
            let Some((_, contract)) = state.rencrow_completion_state.as_mut() else {
                return;
            };
            let Some(unit) = contract.units.iter_mut().find(|unit| unit.id == unit_id) else {
                return;
            };
            let Some(review) = unit.review_evidence.as_mut() else {
                return;
            };
            if unit.kind != CompletionUnitKind::Review
                || unit.status == UnitStatus::Done
                || review.reviewer_thread_id != expected_review.reviewer_thread_id
                || review.reviewer_turn_id != expected_review.reviewer_turn_id
                || review.spawn_call_id != expected_review.spawn_call_id
                || review.deadline_unix_ms != expected_review.deadline_unix_ms
            {
                return;
            }
            if review.deadline_safety_fingerprint == safety_fingerprint && status.is_none() {
                return;
            }
            if let Some(status) = status {
                unit.status = status;
            }
            if review.deadline_escalation.is_none() {
                review.deadline_escalation = escalation.clone();
            }
            review.deadline_safety_fingerprint = safety_fingerprint;
            let escalation = review.deadline_escalation.clone().or(escalation);
            let Some(escalation) = escalation else {
                return;
            };
            contract.next_action = Some(
                "A reviewer exceeded its host deadline. Do not wait for or resubmit the same reviewer; inspect the persisted escalation and decide whether a new host-authorized review is needed.".into(),
            );
            if contract.blocker.is_none() {
                contract.blocker = Some(CompletionBlocker {
                    category: BlockerCategory::Unknown,
                    reason: escalation,
                    required_input: None,
                    authority: None,
                    evidence: Vec::new(),
                    recheck_condition: None,
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
            true
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
    use codex_history::task_context::CompletionReviewEvidence;
    use codex_history::task_context::CompletionReviewTarget;
    use codex_history::task_context::CompletionState;
    use codex_history::task_context::CompletionUnit;
    use codex_history::task_context::CompletionUnitKind;
    use codex_history::task_context::UnitStatus;
    use codex_protocol::error::CodexErr;
    use codex_protocol::protocol::AgentStatus;
    use codex_protocol::protocol::TurnAbortReason;
    use std::future::Future;
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

        fn run(
            self: Arc<Self>,
            _session: Arc<Session>,
            _ctx: Arc<TurnContext>,
            _input: Vec<TurnInput>,
            cancellation_token: CancellationToken,
        ) -> impl Future<Output = SessionTaskResult> + Send {
            async move {
                cancellation_token.cancelled().await;
                Err(CodexErr::TurnAborted)
            }
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
    async fn unknown_expired_reviewer_is_persistently_escalated_without_fake_terminal() {
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
    }
}
