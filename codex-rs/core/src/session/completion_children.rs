//! Collection state follows native tool receipts, not the child's completion prose.
// Modified by RenCrow Switch Core, 2026-11-16.
use super::session::Session;
use codex_history::compaction_candidate::digest;
use codex_history::task_context::CompletionReviewAttempt;
use codex_history::task_context::CompletionReviewAttemptStatus;
use codex_history::task_context::CompletionReviewEvidence;
use codex_history::task_context::CompletionReviewLaunchState;
use codex_history::task_context::CompletionReviewLaunchStatus;
use codex_history::task_context::CompletionReviewState;
use codex_history::task_context::CompletionReviewTarget;
use codex_history::task_context::CompletionReviewTerminalStatus;
use codex_history::task_context::CompletionUnit;
use codex_history::task_context::CompletionUnitKind;
use codex_history::task_context::UnitStatus;
use codex_protocol::ThreadId;
use codex_protocol::protocol::AgentStatus;
use serde_json::Value;

pub(crate) const COMPLETION_REVIEWER_SLA_MS: i64 = 10 * 60 * 1000;

#[derive(Clone, Debug)]
pub(crate) struct CompletionReviewLaunch {
    pub(crate) root_task_id: String,
    pub(crate) unit_id: String,
    pub(crate) target_task_id: String,
    pub(crate) target_task_revision: u64,
    pub(crate) target_artifact_ref: String,
    pub(crate) review_brief: String,
    pub(crate) status: UnitStatus,
    pub(crate) reviewer_thread_id: Option<String>,
    pub(crate) reviewer_turn_id: Option<String>,
    pub(crate) spawn_call_id: Option<String>,
    pub(crate) reviewer_role: Option<String>,
    pub(crate) reviewer_model: Option<String>,
    pub(crate) deadline_escalation: Option<String>,
    pub(crate) unresolved_launch: Option<CompletionReviewLaunchState>,
    pub(crate) should_spawn: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct CompletionReviewAdoption {
    pub(crate) root_task_id: String,
    pub(crate) unit_id: String,
    pub(crate) target_task_id: String,
    pub(crate) target_task_revision: u64,
    pub(crate) reviewer_thread_id: String,
}

#[derive(Clone, Debug)]
pub(crate) struct CompletionReviewChildIdentity {
    pub(crate) target: CompletionReviewTarget,
    pub(crate) target_artifact_ref: String,
    pub(crate) reviewer_thread_id: ThreadId,
    pub(crate) reviewer_role: String,
    pub(crate) spawn_call_id: String,
}

#[derive(Clone, Debug)]
pub(crate) struct CompletionReviewChildGeneration {
    pub(crate) identity: CompletionReviewChildIdentity,
    pub(crate) reviewer_turn_id: String,
}

#[derive(Clone, Debug)]
pub(crate) struct CompletionReviewFollowupPreparation {
    pub(crate) root_task_id: String,
    pub(crate) unit_id: String,
    pub(crate) child_thread_id: ThreadId,
    pub(crate) call_id: String,
    pub(crate) contract_revision: u64,
    pub(crate) prior_unit: CompletionUnit,
    pub(crate) reserved_unit: CompletionUnit,
}

fn terminal(value: &Value) -> bool {
    serde_json::from_value::<AgentStatus>(value.clone()).is_ok_and(|status| {
        matches!(
            status,
            AgentStatus::Completed(_) | AgentStatus::Errored(_) | AgentStatus::Shutdown
        )
    })
}

fn status_matches_review_generation(
    review: &CompletionReviewEvidence,
    child_id: ThreadId,
    turn_id: Option<&str>,
    status: &AgentStatus,
) -> bool {
    ThreadId::try_from(review.reviewer_thread_id.as_str()).is_ok_and(|reviewer_id| {
        reviewer_id == child_id
            && turn_id == Some(review.reviewer_turn_id.as_str())
            && matches!(
                status,
                AgentStatus::Completed(_) | AgentStatus::Errored(_) | AgentStatus::Shutdown
            )
    })
}

fn legacy_review_failure_receipt_can_be_recorded(
    unit: &CompletionUnit,
    review: &CompletionReviewEvidence,
    expected_binding: &str,
    exact_generation: bool,
    status: &AgentStatus,
) -> bool {
    if unit.review_state.is_some()
        || !exact_generation
        || !matches!(unit.status, UnitStatus::Running | UnitStatus::Failed)
    {
        return false;
    }
    let Some(current) = unit.review_evidence.as_ref() else {
        return false;
    };
    if current.reviewer_thread_id != review.reviewer_thread_id
        || current.reviewer_turn_id != expected_binding
        || current.spawn_call_id != review.spawn_call_id
        || current.wait_result_ref.is_some()
        || current.wait_call_id.is_some()
    {
        return false;
    }
    let review_ended_fingerprint =
        super::completion_review_deadline::review_ended_durable_fingerprint(expected_binding);
    if current.deadline_safety_fingerprint.as_deref() == Some(review_ended_fingerprint.as_str())
        && !matches!(status, AgentStatus::Errored(reason) if reason == "ReviewEnded")
    {
        return false;
    }
    matches!(
        (unit.status, current.terminal_status),
        (UnitStatus::Running, None)
            | (
                UnitStatus::Failed,
                None | Some(CompletionReviewTerminalStatus::Failed)
            )
    )
}

fn completion_review_needs_spawn(unit: &CompletionUnit) -> bool {
    match unit.review_state.as_ref() {
        Some(review) => match review.active_attempt.as_ref() {
            None => matches!(unit.status, UnitStatus::Planned | UnitStatus::Ready),
            Some(attempt) => {
                attempt.status == CompletionReviewAttemptStatus::Failed
                    && review
                        .retry_authorization
                        .as_ref()
                        .is_some_and(|grant| grant.expected_attempt_id == attempt.attempt_id)
            }
        },
        None => {
            matches!(unit.status, UnitStatus::Planned | UnitStatus::Ready)
                && unit.review_evidence.is_none()
        }
    }
}

fn close_failure_cause(status: &AgentStatus) -> Option<String> {
    match status {
        AgentStatus::Completed(None) => Some("reviewer completed without a result summary".into()),
        AgentStatus::Errored(reason) => {
            Some(format!("reviewer turn ended with an error: {reason}"))
        }
        AgentStatus::Shutdown => Some("reviewer was shut down before returning a result".into()),
        _ => None,
    }
}

fn pending_completion_review_evidence(
    reviewer_thread_id: ThreadId,
    reviewer_role: String,
    spawn_call_id: String,
    target_task_id: String,
    target_task_revision: u64,
    target_artifact_ref: String,
    deadline_unix_ms: Option<i64>,
) -> CompletionReviewEvidence {
    CompletionReviewEvidence {
        reviewer_thread_id: reviewer_thread_id.to_string(),
        reviewer_turn_id: format!("pending-generation:{spawn_call_id}"),
        reviewer_role,
        spawn_call_id,
        deadline_unix_ms,
        deadline_escalation: None,
        deadline_safety_fingerprint: None,
        wait_result_ref: None,
        terminal_status: None,
        wait_call_id: None,
        target_task_id,
        target_task_revision,
        target_artifact_ref,
    }
}

fn existing_completion_review_launch(
    root_task_id: String,
    unit: &CompletionUnit,
    target_task_id: &str,
) -> Result<CompletionReviewLaunch, String> {
    let projected = codex_history::task_context::project_completion_review_state(unit)?;
    let unit = &projected;
    let attempt = unit
        .review_state
        .as_ref()
        .and_then(|review| review.active_attempt.as_ref());
    let evidence = unit.review_evidence.as_ref();
    let target_task_revision = attempt
        .map(|attempt| attempt.target_task_revision)
        .or_else(|| evidence.map(|evidence| evidence.target_task_revision))
        .or_else(|| {
            unit.review_target
                .as_ref()
                .map(|target| target.task_revision)
        })
        .unwrap_or_default();
    let target_artifact_ref = attempt
        .map(|attempt| attempt.target_artifact_ref.clone())
        .or_else(|| evidence.map(|evidence| evidence.target_artifact_ref.clone()))
        .unwrap_or_default();
    let review_brief = if target_artifact_ref.is_empty() {
        format!(
            "Review purpose: {}\nExisting reviewer launch is unresolved.",
            unit.purpose
        )
    } else {
        format!(
            "Review purpose: {}\nAccepted artifact ref: {}",
            unit.purpose, target_artifact_ref
        )
    };
    Ok(CompletionReviewLaunch {
        root_task_id,
        unit_id: unit.id.clone(),
        target_task_id: target_task_id.to_string(),
        target_task_revision,
        target_artifact_ref,
        review_brief,
        status: unit.status,
        reviewer_thread_id: attempt
            .and_then(|attempt| attempt.reviewer_thread_id.clone())
            .or_else(|| evidence.map(|evidence| evidence.reviewer_thread_id.clone())),
        reviewer_turn_id: attempt
            .and_then(|attempt| attempt.reviewer_turn_id.clone())
            .or_else(|| evidence.map(|evidence| evidence.reviewer_turn_id.clone())),
        spawn_call_id: attempt
            .map(|attempt| attempt.attempt_id.clone())
            .or_else(|| evidence.map(|evidence| evidence.spawn_call_id.clone()))
            .or_else(|| {
                unit.review_launch
                    .as_ref()
                    .map(|launch| launch.spawn_call_id.clone())
            }),
        reviewer_role: attempt
            .map(|attempt| attempt.reviewer_role.clone())
            .or_else(|| evidence.map(|evidence| evidence.reviewer_role.clone())),
        reviewer_model: attempt.and_then(|attempt| attempt.reviewer_model.clone()),
        deadline_escalation: attempt
            .and_then(|attempt| attempt.deadline_escalation.clone())
            .or_else(|| evidence.and_then(|evidence| evidence.deadline_escalation.clone())),
        unresolved_launch: unit.review_launch.clone(),
        should_spawn: false,
    })
}

fn bind_completion_review_generation(
    evidence: &mut CompletionReviewEvidence,
    reviewer_thread_id: ThreadId,
    spawn_call_id: &str,
    reviewer_turn_id: String,
) -> bool {
    if evidence.reviewer_thread_id != reviewer_thread_id.to_string()
        || evidence.spawn_call_id != spawn_call_id
        || !evidence.reviewer_turn_id.starts_with("pending-generation:")
    {
        return false;
    }
    evidence.reviewer_turn_id = reviewer_turn_id;
    true
}

impl Session {
    /// Resolve one plan-declared review unit for the latest accepted task revision.
    /// A running unit without evidence is a conservative unresolved-spawn marker: a
    /// repeated review request returns it instead of spawning a duplicate child.
    pub(crate) async fn prepare_completion_review_launch(
        &self,
        target_task_id: &str,
        unit_id: Option<&str>,
        launch_call_id: &str,
        reviewer_role: Option<&str>,
        reviewer_model: Option<&str>,
    ) -> Result<CompletionReviewLaunch, String> {
        let _review_guard = self.completion_review_owner_guard().await;
        let (root_task_id, current_contract) = self
            .autonomous_completion_current_state()
            .await
            .ok_or_else(|| "completion review requires a registered completion plan".to_string())?;
        let projected_current_units =
            super::completion_review::projected_completion_review_units(&current_contract)?;
        let existing = projected_current_units
            .iter()
            .filter(|unit| {
                unit.kind == CompletionUnitKind::Review
                    && unit
                        .review_target
                        .as_ref()
                        .is_some_and(|target| target.task_id == target_task_id)
                    && unit_id.is_none_or(|requested| unit.id == requested)
                    && (unit.status == UnitStatus::Done
                        || unit
                            .review_state
                            .as_ref()
                            .is_some_and(|review| review.active_attempt.is_some())
                        || unit.review_evidence.is_some()
                        || unit.review_launch.is_some())
            })
            .collect::<Vec<_>>();
        if existing.len() > 1 {
            return Err("multiple existing review attempts match; provide an exact unit_id".into());
        }
        if let Some(unit) = existing.first() {
            let retry_is_authorized = unit
                .review_state
                .as_ref()
                .and_then(|review| review.active_attempt.as_ref())
                .is_some_and(|attempt| {
                    attempt.status == CompletionReviewAttemptStatus::Failed
                        && unit
                            .review_state
                            .as_ref()
                            .and_then(|review| review.retry_authorization.as_ref())
                            .is_some_and(|grant| grant.expected_attempt_id == attempt.attempt_id)
                });
            if !retry_is_authorized {
                return existing_completion_review_launch(root_task_id, unit, target_task_id);
            }
        }
        let task_state = self
            .latest_rencrow_task_state()
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "review target has no accepted task receipt".to_string())?;
        if task_state.boundary.root_task_id != root_task_id {
            return Err("review target receipt belongs to a different completion root".into());
        }
        let accepted = task_state
            .completed
            .get(target_task_id)
            .ok_or_else(|| "review target has no accepted task receipt".to_string())?;
        let target_task_revision = accepted.revision;
        let target_artifact_ref = self
            .current_completion_review_target_ref(target_task_id, target_task_revision)
            .await?;
        let current_contract = self
            .autonomous_completion_current_state()
            .await
            .map(|(_, contract)| contract)
            .ok_or_else(|| "completion plan disappeared before review selection".to_string())?;
        let initial_selection = super::completion_review::select_completion_review(
            &current_contract,
            target_task_id,
            target_task_revision,
            unit_id,
        )?;
        let previous_snapshot_ref = match &initial_selection.outcome {
            super::completion_review::CompletionReviewSelectionOutcome::NeedsReservation {
                expected_attempt_id: Some(expected_attempt_id),
                ..
            } => {
                let attempt = current_contract
                    .units
                    .iter()
                    .find(|unit| unit.id == initial_selection.obligation_unit_id)
                    .and_then(|unit| unit.review_state.as_ref())
                    .and_then(|review| review.active_attempt.as_ref())
                    .ok_or_else(|| "retry selection lost its active attempt".to_string())?;
                if attempt.attempt_id.as_str() != expected_attempt_id.as_str()
                    || attempt.reviewer_thread_id.is_none()
                    || attempt.reviewer_turn_id.is_none()
                {
                    return Err("retry requires the exact bound failed child generation".into());
                }
                attempt.terminal_source.as_ref().ok_or_else(|| {
                    "retry requires a recorded exact terminal failure fact".to_string()
                })?;
                Some(self.persisted_review_attempt_snapshot_ref(
                    &root_task_id,
                    &initial_selection.obligation_unit_id,
                    attempt,
                )
                .await?
                .ok_or_else(|| {
                    "retry requires a previously persisted snapshot for this exact failed attempt".to_string()
                })?)
            }
            _ => None,
        };

        let launch = {
            let mut state = self.state.lock().await;
            if state.rencrow_completion_paused {
                return Err(
                    "completion review is blocked while autonomous completion is paused".into(),
                );
            }
            let task_check_snapshot = state.task_check_ledger.snapshot();
            let Some((active_root, contract)) = state.rencrow_completion_state.as_mut() else {
                return Err("completion plan disappeared before review launch".into());
            };
            if active_root != &root_task_id || contract.paused {
                return Err("completion review root changed or is paused".into());
            }
            let selected = super::completion_review::select_completion_review(
                contract,
                target_task_id,
                target_task_revision,
                unit_id,
            )?;
            if selected != initial_selection {
                return Err("review obligation changed between selection and reservation".into());
            }
            let review_index = contract
                .units
                .iter()
                .position(|unit| unit.id == selected.obligation_unit_id)
                .ok_or_else(|| "selected review obligation disappeared".to_string())?;
            let projected_unit = codex_history::task_context::project_completion_review_state(
                &contract.units[review_index],
            )?;
            contract.units[review_index] = projected_unit;
            let conditions_sha256 = super::completion_review::completion_review_conditions_sha256(
                contract,
                &contract.units[review_index].condition_refs,
            )?;
            let review_unit = &contract.units[review_index];
            let mut brief = String::from("Accepted artifact locations:\n");
            if let Some(check) = task_check_snapshot.iter().find(|entry| {
                entry.task_id == target_task_id && entry.task_revision == target_task_revision
            }) && let crate::task_check::TaskCheckAttemptStatus::Finished(receipt) =
                &check.attempt.status
            {
                for artifact in receipt.artifact_after.iter().take(12) {
                    let absolute_path = receipt.workspace_root.join(&artifact.path);
                    let location = serde_json::json!({
                        "workspace_root": receipt.workspace_root.to_string_lossy(),
                        "absolute_path": absolute_path.to_string_lossy(),
                    });
                    brief.push_str(&format!("Verified artifact location: {location}\n"));
                }
            }
            if brief.chars().count() > 7_000 {
                return Err(
                    "verified artifact locations exceed the completion review brief limit".into(),
                );
            }
            brief.push_str(&format!(
                "Review purpose: {}\nAccepted task result: {}\nAccepted artifact ref: {}",
                review_unit.purpose, accepted.result, target_artifact_ref
            ));
            for condition_id in review_unit.condition_refs.iter().take(8) {
                if let Some(condition) = contract.conditions.iter().find(|c| c.id == *condition_id)
                {
                    brief.push_str(&format!(
                        "\nAcceptance condition {}: result={}; scope={}; verification={}; criteria={}",
                        condition.id,
                        condition.result,
                        condition.scope,
                        condition.verification,
                        condition.acceptance_criteria
                    ));
                }
            }
            let check = task_check_snapshot.into_iter().find(|entry| {
                entry.task_id == target_task_id && entry.task_revision == target_task_revision
            });
            if let Some(check) = check
                && let crate::task_check::TaskCheckAttemptStatus::Finished(receipt) =
                    check.attempt.status
            {
                brief.push_str(&format!(
                    "\nHost verification: call_id={}; exit_code={}; succeeded={}",
                    receipt.call_id, receipt.exit_code, receipt.succeeded
                ));
            }
            if brief.chars().count() > 7_000 {
                brief = brief.chars().take(7_000).collect();
            }
            let dependencies_ready =
                contract.units[review_index]
                    .depends_on
                    .iter()
                    .all(|dependency| {
                        contract
                            .units
                            .iter()
                            .find(|candidate| candidate.id == *dependency)
                            .is_some_and(|candidate| candidate.status == UnitStatus::Done)
                    });
            let unit = &mut contract.units[review_index];
            if unit.status == UnitStatus::Planned && !dependencies_ready {
                return Err(format!(
                    "planned review unit {} is waiting for its declared dependencies; accept those units first",
                    unit.id
                ));
            }
            let should_spawn = match unit.status {
                UnitStatus::Planned | UnitStatus::Ready | UnitStatus::Failed
                    if completion_review_needs_spawn(unit) =>
                {
                    let (Some(reviewer_role), Some(reviewer_model)) =
                        (reviewer_role, reviewer_model)
                    else {
                        return Err(
                            "new review reservation requires the effective reviewer role and model"
                                .into(),
                        );
                    };
                    if reviewer_role.trim().is_empty() || reviewer_model.trim().is_empty() {
                        return Err(
                            "review reservation requires the effective reviewer role and model"
                                .into(),
                        );
                    }
                    let prior_review_state = unit.review_state.clone();
                    let retry_authorization = prior_review_state
                        .as_ref()
                        .and_then(|review| review.retry_authorization.as_ref());
                    let retry = unit.status == UnitStatus::Failed;
                    if retry
                        && retry_authorization.is_none_or(|grant| {
                            prior_review_state
                                .as_ref()
                                .and_then(|review| review.active_attempt.as_ref())
                                .is_none_or(|attempt| {
                                    grant.expected_attempt_id != attempt.attempt_id
                                })
                        })
                    {
                        return Err("failed review retry has no one-shot host authorization".into());
                    }
                    if retry
                        && prior_review_state
                            .as_ref()
                            .and_then(|review| review.active_attempt.as_ref())
                            .is_none_or(|attempt| {
                                attempt.status != CompletionReviewAttemptStatus::Failed
                            })
                    {
                        return Err(
                            "retry authorization no longer matches a definitively failed attempt"
                                .into(),
                        );
                    }
                    let previous_attempt_ref = if retry {
                        previous_snapshot_ref.clone()
                    } else {
                        prior_review_state
                            .as_ref()
                            .and_then(|review| review.previous_attempt_ref.clone())
                    };
                    let consumed_retry_authorization = retry_authorization.cloned();
                    let prior_failure = prior_review_state.as_ref().and_then(|review| {
                        review.active_attempt.as_ref().and_then(|attempt| {
                            Some(
                                codex_history::task_context::CompletionReviewHistoricalFailure {
                                    attempt_id: attempt.attempt_id.clone(),
                                    contract_revision: Some(attempt.contract_revision),
                                    imported_from: attempt.imported_from.clone(),
                                    consumed_retry_authorization: attempt
                                        .consumed_retry_authorization
                                        .clone(),
                                    reviewer_thread_id: attempt.reviewer_thread_id.clone()?,
                                    reviewer_turn_id: attempt.reviewer_turn_id.clone()?,
                                    target_task_id: attempt.target_task_id.clone(),
                                    target_task_revision: attempt.target_task_revision,
                                    target_artifact_ref: attempt.target_artifact_ref.clone(),
                                    reviewer_role: attempt.reviewer_role.clone(),
                                    reviewer_model: attempt.reviewer_model.clone(),
                                    deadline_unix_ms: attempt.deadline_unix_ms,
                                    wait_result_ref: attempt.wait_result_ref.clone(),
                                    wait_call_id: attempt.wait_call_id.clone(),
                                    deadline_escalation: attempt.deadline_escalation.clone(),
                                    deadline_safety_fingerprint: attempt
                                        .deadline_safety_fingerprint
                                        .clone(),
                                    terminal_source: attempt.terminal_source.clone()?,
                                },
                            )
                        })
                    });
                    let deadline_unix_ms =
                        chrono::Utc::now().timestamp_millis() + COMPLETION_REVIEWER_SLA_MS;
                    let mut next_review_state =
                        prior_review_state.unwrap_or_else(|| CompletionReviewState {
                            version: 1,
                            obligation_unit_id: unit.id.clone(),
                            active_attempt: None,
                            historical_failure: None,
                            retry_reservation_prior_historical_failure: None,
                            previous_attempt_ref: None,
                            retry_authorization: None,
                            withdrawal: None,
                            revalidation: None,
                        });
                    if retry {
                        if next_review_state
                            .retry_reservation_prior_historical_failure
                            .is_some()
                        {
                            return Err("retry reservation already has rollback history".into());
                        }
                        next_review_state.retry_reservation_prior_historical_failure =
                            next_review_state.historical_failure.take();
                        next_review_state.historical_failure = prior_failure;
                        next_review_state.retry_authorization = None;
                    }
                    if let Some(previous_attempt_ref) = previous_attempt_ref {
                        next_review_state.previous_attempt_ref = Some(previous_attempt_ref);
                    }
                    next_review_state.active_attempt = Some(CompletionReviewAttempt {
                        attempt_id: launch_call_id.to_string(),
                        contract_revision: contract.contract_revision,
                        condition_refs: unit.condition_refs.clone(),
                        conditions_sha256,
                        status: CompletionReviewAttemptStatus::Reserved,
                        launch_unknown_reason: None,
                        consumed_retry_authorization,
                        reviewer_thread_id: None,
                        reviewer_turn_id: None,
                        reviewer_role: reviewer_role.to_string(),
                        reviewer_model: Some(reviewer_model.to_string()),
                        imported_from: None,
                        target_task_id: target_task_id.to_string(),
                        target_task_revision,
                        target_artifact_ref: target_artifact_ref.clone(),
                        deadline_unix_ms: Some(deadline_unix_ms),
                        deadline_escalation: None,
                        deadline_safety_fingerprint: None,
                        terminal_source: None,
                        wait_result_ref: None,
                        wait_call_id: None,
                        adoption_reason: None,
                    });
                    unit.review_state = Some(next_review_state);
                    unit.status = UnitStatus::Running;
                    unit.review_launch = Some(CompletionReviewLaunchState {
                        spawn_call_id: launch_call_id.to_string(),
                        status: CompletionReviewLaunchStatus::Reserved,
                        deadline_unix_ms: Some(deadline_unix_ms),
                        reason: None,
                    });
                    if !retry {
                        contract.current_unit = Some(unit.id.clone());
                        contract.activity_at = Some(chrono::Utc::now().timestamp());
                    }
                    true
                }
                UnitStatus::Running
                | UnitStatus::Ready
                | UnitStatus::Done
                | UnitStatus::Blocked
                | UnitStatus::Failed => false,
                _ => {
                    return Err(format!(
                        "review unit {} is not available for launch ({:?})",
                        unit.id, unit.status
                    ));
                }
            };
            if unit.review_state.is_some() {
                *unit = codex_history::task_context::project_completion_review_state(unit)?;
            }
            let reviewer_thread_id = unit
                .review_evidence
                .as_ref()
                .map(|evidence| evidence.reviewer_thread_id.clone());
            let projected_reviewer_role = unit
                .review_state
                .as_ref()
                .and_then(|review| review.active_attempt.as_ref())
                .map(|attempt| attempt.reviewer_role.clone())
                .or_else(|| {
                    unit.review_evidence
                        .as_ref()
                        .map(|evidence| evidence.reviewer_role.clone())
                });
            let deadline_escalation = unit
                .review_evidence
                .as_ref()
                .and_then(|evidence| evidence.deadline_escalation.clone());
            let reviewer_turn_id = unit
                .review_evidence
                .as_ref()
                .map(|evidence| evidence.reviewer_turn_id.clone());
            let spawn_call_id = unit
                .review_state
                .as_ref()
                .and_then(|review| review.active_attempt.as_ref())
                .map(|attempt| attempt.attempt_id.clone())
                .or_else(|| {
                    unit.review_evidence
                        .as_ref()
                        .map(|evidence| evidence.spawn_call_id.clone())
                })
                .or_else(|| {
                    unit.review_launch
                        .as_ref()
                        .map(|launch| launch.spawn_call_id.clone())
                });
            CompletionReviewLaunch {
                root_task_id,
                unit_id: unit.id.clone(),
                target_task_id: target_task_id.to_string(),
                target_task_revision,
                target_artifact_ref: target_artifact_ref.clone(),
                review_brief: brief,
                status: unit.status,
                reviewer_thread_id,
                reviewer_turn_id,
                spawn_call_id,
                reviewer_role: projected_reviewer_role
                    .or_else(|| reviewer_role.map(str::to_string)),
                reviewer_model: unit
                    .review_state
                    .as_ref()
                    .and_then(|review| review.active_attempt.as_ref())
                    .and_then(|attempt| attempt.reviewer_model.clone()),
                deadline_escalation,
                unresolved_launch: unit.review_launch.clone(),
                should_spawn,
            }
        };
        if launch.should_spawn
            && let Err(error) = self.persist_autonomous_completion_snapshot().await
        {
            self.pause_autonomous_completion().await;
            return Err(format!(
                "failed to persist review launch reservation: {error}"
            ));
        }
        Ok(launch)
    }

    /// Undo only a reservation whose spawn was rejected by a typed capacity error.
    /// Other spawn errors may have happened after thread creation and stay unresolved.
    pub(crate) async fn rollback_unstarted_completion_review(
        &self,
        unit_id: &str,
    ) -> Result<(), String> {
        let _review_guard = self.completion_review_owner_guard().await;
        let persisted_rollback_ref = self.persisted_retry_rollback_previous_ref(unit_id).await?;
        let changed = {
            let mut state = self.state.lock().await;
            let Some((_, contract)) = state.rencrow_completion_state.as_mut() else {
                return Err("completion review contract disappeared before rollback".into());
            };
            let unit_index = contract
                .units
                .iter()
                .position(|unit| unit.id == unit_id)
                .ok_or_else(|| "completion review unit disappeared before rollback".to_string())?;
            let unit = &contract.units[unit_index];
            if unit.kind != CompletionUnitKind::Review
                || unit.status != UnitStatus::Running
                || unit.review_evidence.is_some()
            {
                return Err(
                    "completion review reservation is no longer safely rollbackable".into(),
                );
            }
            let conditions_sha256 = super::completion_review::completion_review_conditions_sha256(
                contract,
                &unit.condition_refs,
            )?;
            let rollback = unit.review_state.as_ref().and_then(|review_state| {
                review_state.active_attempt.as_ref().and_then(|attempt| {
                    attempt
                        .consumed_retry_authorization
                        .clone()
                        .map(|authorization| {
                            (
                                authorization,
                                review_state.historical_failure.clone(),
                                review_state
                                    .retry_reservation_prior_historical_failure
                                    .clone(),
                            )
                        })
                })
            });
            let retry_previous_attempt_ref = match (&rollback, persisted_rollback_ref) {
                (
                    Some(_),
                    super::completion_persistence::PersistedRetryRollbackRef::Retry(reference),
                ) => Some(reference),
                (None, super::completion_persistence::PersistedRetryRollbackRef::NotRetry) => None,
                _ => {
                    return Err(
                        "persisted retry rollback identity does not match the active reservation"
                            .into(),
                    );
                }
            };
            let unit = &mut contract.units[unit_index];
            if let Some((authorization, failure, prior_historical_failure)) = rollback {
                let failure = failure.ok_or_else(|| {
                    "failed retry reservation lost its prior failure proof".to_string()
                })?;
                let contract_revision = failure.contract_revision.ok_or_else(|| {
                    "failed retry reservation cannot restore its prior contract revision"
                        .to_string()
                })?;
                let imported_from = failure.imported_from.clone();
                let reviewer_model = failure.reviewer_model.clone();
                let previous_attempt_ref = retry_previous_attempt_ref.ok_or_else(|| {
                    "retry reservation rollback reference was not resolved".to_string()
                })?;
                let review_state = unit.review_state.as_mut().ok_or_else(|| {
                    "failed retry reservation lost canonical review state".to_string()
                })?;
                review_state.retry_authorization = Some(authorization);
                review_state.historical_failure = prior_historical_failure;
                review_state.retry_reservation_prior_historical_failure = None;
                review_state.previous_attempt_ref = previous_attempt_ref;
                review_state.active_attempt = Some(CompletionReviewAttempt {
                    attempt_id: failure.attempt_id,
                    contract_revision,
                    condition_refs: unit.condition_refs.clone(),
                    conditions_sha256,
                    status: CompletionReviewAttemptStatus::Failed,
                    launch_unknown_reason: None,
                    consumed_retry_authorization: failure.consumed_retry_authorization,
                    imported_from,
                    reviewer_thread_id: Some(failure.reviewer_thread_id),
                    reviewer_turn_id: Some(failure.reviewer_turn_id),
                    reviewer_role: failure.reviewer_role,
                    reviewer_model,
                    target_task_id: failure.target_task_id,
                    target_task_revision: failure.target_task_revision,
                    target_artifact_ref: failure.target_artifact_ref,
                    deadline_unix_ms: failure.deadline_unix_ms,
                    deadline_escalation: failure.deadline_escalation,
                    deadline_safety_fingerprint: failure.deadline_safety_fingerprint,
                    terminal_source: Some(failure.terminal_source),
                    wait_result_ref: failure.wait_result_ref,
                    wait_call_id: failure.wait_call_id,
                    adoption_reason: None,
                });
                unit.status = UnitStatus::Failed;
            } else {
                if let Some(review_state) = unit.review_state.as_mut() {
                    review_state.active_attempt = None;
                }
                unit.status = UnitStatus::Ready;
            }
            if unit.review_state.is_some() {
                *unit = codex_history::task_context::project_completion_review_state(unit)?;
            }
            unit.review_launch = None;
            true
        };
        if changed {
            self.persist_autonomous_completion_snapshot().await?;
        }
        Ok(())
    }

    /// Persist the real child ID immediately after spawn succeeds, before checking
    /// metadata or generation. A sentinel generation keeps the launch unresolved.
    pub(crate) async fn record_completion_review_child_pending(
        &self,
        unit_id: &str,
        identity: CompletionReviewChildIdentity,
    ) -> Result<(), String> {
        let _review_guard = self.completion_review_owner_guard().await;
        let CompletionReviewChildIdentity {
            target: review_target,
            target_artifact_ref,
            reviewer_thread_id,
            reviewer_role,
            spawn_call_id,
        } = identity;
        let reviewer_thread_id_string = reviewer_thread_id.to_string();
        let (should_persist, binding_result) = {
            let mut state = self.state.lock().await;
            let newly_uncollected = state
                .rencrow_completion_uncollected_children
                .insert(reviewer_thread_id_string.clone());
            let binding_result = (|| -> Result<bool, String> {
                let Some((_, contract)) = state.rencrow_completion_state.as_mut() else {
                    return Err("completion review contract disappeared after child spawn".into());
                };
                let Some(unit) = contract.units.iter_mut().find(|unit| unit.id == unit_id) else {
                    return Err("completion review unit disappeared after child spawn".into());
                };
                let mut candidate =
                    codex_history::task_context::project_completion_review_state(unit)?;
                if let Some(evidence) = candidate.review_evidence.as_ref() {
                    let exact_pending_child = candidate.kind == CompletionUnitKind::Review
                        && candidate.status == UnitStatus::Running
                        && candidate.review_launch.is_none()
                        && evidence.reviewer_thread_id == reviewer_thread_id_string
                        && evidence.reviewer_turn_id
                            == format!("pending-generation:{spawn_call_id}")
                        && evidence.reviewer_role == reviewer_role
                        && evidence.spawn_call_id == spawn_call_id
                        && evidence.target_task_id == review_target.task_id
                        && evidence.target_task_revision == review_target.task_revision
                        && evidence.target_artifact_ref == target_artifact_ref
                        && evidence.wait_result_ref.is_none()
                        && evidence.terminal_status.is_none()
                        && evidence.wait_call_id.is_none()
                        && candidate.review_state.as_ref().is_none_or(|review_state| {
                            review_state.active_attempt.as_ref().is_some_and(|attempt| {
                                attempt.attempt_id == spawn_call_id
                                    && attempt.reviewer_thread_id.as_deref()
                                        == Some(reviewer_thread_id_string.as_str())
                                    && attempt.reviewer_turn_id.is_none()
                                    && attempt.reviewer_role == reviewer_role
                                    && matches!(
                                        attempt.status,
                                        CompletionReviewAttemptStatus::Reserved
                                            | CompletionReviewAttemptStatus::Unknown
                                    )
                            })
                        });
                    return if exact_pending_child {
                        Ok(false)
                    } else {
                        Err("a different child is already recorded for this review launch".into())
                    };
                }
                if candidate.kind != CompletionUnitKind::Review
                    || candidate.status != UnitStatus::Running
                    || candidate
                        .review_launch
                        .as_ref()
                        .is_none_or(|launch| launch.spawn_call_id != spawn_call_id)
                    || candidate
                        .review_target
                        .as_ref()
                        .is_none_or(|existing_target| {
                            existing_target.task_id != review_target.task_id
                                || existing_target.task_revision != review_target.task_revision
                        })
                {
                    return Err("completion review launch changed after child spawn".into());
                }
                let deadline_unix_ms = candidate
                    .review_launch
                    .as_ref()
                    .and_then(|launch| launch.deadline_unix_ms);
                if let Some(review_state) = candidate.review_state.as_mut() {
                    let attempt = review_state.active_attempt.as_mut().ok_or_else(|| {
                        "canonical review reservation disappeared after child spawn".to_string()
                    })?;
                    if attempt.attempt_id != spawn_call_id
                        || attempt.reviewer_thread_id.is_some()
                        || attempt.reviewer_turn_id.is_some()
                        || !matches!(
                            attempt.status,
                            CompletionReviewAttemptStatus::Reserved
                                | CompletionReviewAttemptStatus::Unknown
                        )
                        || attempt.target_task_id != review_target.task_id
                        || attempt.target_task_revision != review_target.task_revision
                        || attempt.target_artifact_ref != target_artifact_ref
                        || attempt.reviewer_role != reviewer_role
                    {
                        return Err(
                            "spawned child does not match its canonical review reservation".into(),
                        );
                    }
                    // This is the configured route, not verified child metadata. The attempt
                    // remains Unknown until the handler confirms role/model and turn binding.
                    attempt.reviewer_thread_id = Some(reviewer_thread_id_string.clone());
                    attempt.status = CompletionReviewAttemptStatus::Unknown;
                    attempt.launch_unknown_reason = None;
                }
                candidate.review_evidence = Some(pending_completion_review_evidence(
                    reviewer_thread_id,
                    reviewer_role,
                    spawn_call_id,
                    review_target.task_id,
                    review_target.task_revision,
                    target_artifact_ref,
                    deadline_unix_ms,
                ));
                candidate.review_launch = None;
                candidate =
                    codex_history::task_context::project_completion_review_state(&candidate)?;
                *unit = candidate;
                Ok(true)
            })();
            let persist =
                newly_uncollected || binding_result.as_ref().is_ok_and(|changed| *changed);
            (persist, binding_result.map(|_| ()))
        };
        if should_persist {
            self.persist_autonomous_completion_snapshot().await?;
        }
        binding_result
    }

    pub(crate) async fn mark_completion_review_launch_unknown(
        &self,
        unit_id: &str,
        spawn_call_id: &str,
        reason: String,
    ) -> Result<(), String> {
        let _review_guard = self.completion_review_owner_guard().await;
        {
            let mut state = self.state.lock().await;
            let Some((_, contract)) = state.rencrow_completion_state.as_mut() else {
                return Err(
                    "completion review contract disappeared while recording unknown launch".into(),
                );
            };
            let unit = contract
                .units
                .iter_mut()
                .find(|unit| unit.id == unit_id)
                .ok_or_else(|| {
                    "completion review unit disappeared while recording unknown launch".to_string()
                })?;
            if unit.review_state.is_some() {
                *unit = codex_history::task_context::project_completion_review_state(unit)?;
            }
            if unit.kind != CompletionUnitKind::Review
                || unit.status != UnitStatus::Running
                || unit.review_evidence.is_some()
            {
                return Err(
                    "completion review launch changed before unknown outcome was recorded".into(),
                );
            }
            let launch = unit.review_launch.as_mut().ok_or_else(|| {
                "completion review launch reservation disappeared before reconciliation".to_string()
            })?;
            if launch.spawn_call_id != spawn_call_id {
                return Err(
                    "completion review launch changed before unknown outcome was recorded".into(),
                );
            }
            launch.status = CompletionReviewLaunchStatus::Unknown;
            launch.reason = Some(reason.clone());
            if let Some(attempt) = unit
                .review_state
                .as_mut()
                .and_then(|review_state| review_state.active_attempt.as_mut())
                .filter(|attempt| attempt.attempt_id == spawn_call_id)
            {
                attempt.status = CompletionReviewAttemptStatus::Unknown;
                attempt.launch_unknown_reason = Some(reason);
            }
            if unit.review_state.is_some() {
                *unit = codex_history::task_context::project_completion_review_state(unit)?;
            }
            contract.activity_at = Some(chrono::Utc::now().timestamp());
        }
        self.persist_autonomous_completion_snapshot().await
    }

    /// Adopt only the exact terminal review generation collected by native wait_agent.
    pub(crate) async fn adopt_completion_review(
        &self,
        unit_id: &str,
        reason: String,
    ) -> Result<CompletionReviewAdoption, String> {
        let _review_guard = self.completion_review_owner_guard().await;
        if reason.trim().is_empty() {
            return Err("review adoption requires a non-empty parent decision reason".into());
        }
        let (root_task_id, unit) = {
            let state = self.state.lock().await;
            if state.rencrow_completion_paused {
                return Err(
                    "review adoption is blocked while autonomous completion is paused".into(),
                );
            }
            let (root, contract) = state
                .rencrow_completion_state
                .as_ref()
                .ok_or_else(|| "no completion plan is registered".to_string())?;
            if contract.paused {
                return Err(
                    "review adoption is blocked while autonomous completion is paused".into(),
                );
            }
            let unit = contract
                .units
                .iter()
                .find(|unit| unit.id == unit_id)
                .map(codex_history::task_context::project_completion_review_state)
                .transpose()?
                .ok_or_else(|| "review unit does not exist in the completion plan".to_string())?;
            (root.clone(), unit)
        };
        if unit.kind != CompletionUnitKind::Review {
            return Err("only a planned review unit can be adopted".into());
        }
        if unit.status == UnitStatus::Done {
            if unit.review_decision_reason.as_deref() == Some(reason.as_str()) {
                let evidence = unit
                    .review_evidence
                    .as_ref()
                    .ok_or_else(|| "adopted review is missing host evidence".to_string())?;
                return Ok(CompletionReviewAdoption {
                    root_task_id,
                    unit_id: unit.id,
                    target_task_id: evidence.target_task_id.clone(),
                    target_task_revision: evidence.target_task_revision,
                    reviewer_thread_id: evidence.reviewer_thread_id.clone(),
                });
            }
            return Err("an adopted review cannot be changed or repeated without a new host-authorized plan".into());
        }
        let evidence = unit
            .review_evidence
            .clone()
            .ok_or_else(|| "review has not been spawned by the host review action".to_string())?;
        if self
            .state
            .lock()
            .await
            .rencrow_completion_uncollected_children
            .contains(&evidence.reviewer_thread_id)
        {
            return Err(
                "review adoption is blocked while its child has uncollected runtime work".into(),
            );
        }
        if evidence
            .wait_result_ref
            .as_deref()
            .is_none_or(str::is_empty)
            || evidence.wait_call_id.as_deref().is_none_or(str::is_empty)
            || evidence.terminal_status != Some(CompletionReviewTerminalStatus::Completed)
        {
            return Err(
                "review result has not been collected from the same completed child generation"
                    .into(),
            );
        }
        let target_ref = self
            .current_completion_review_target_ref(
                &evidence.target_task_id,
                evidence.target_task_revision,
            )
            .await?;
        if target_ref != evidence.target_artifact_ref {
            return Err("review target changed after the reviewer completed".into());
        }
        let adopted = {
            let mut state = self.state.lock().await;
            if state.rencrow_completion_paused {
                return Err("review adoption was paused before commit".into());
            }
            let child_is_uncollected = state
                .rencrow_completion_uncollected_children
                .contains(&evidence.reviewer_thread_id);
            let (root, contract) = state
                .rencrow_completion_state
                .as_mut()
                .ok_or_else(|| "completion plan disappeared before review adoption".to_string())?;
            if root != &root_task_id || contract.paused {
                return Err("completion plan changed before review adoption".into());
            }
            if child_is_uncollected {
                return Err(
                    "review adoption is blocked while its child has uncollected runtime work"
                        .into(),
                );
            }
            let adopted_unit_id = {
                let current = contract
                    .units
                    .iter_mut()
                    .find(|candidate| candidate.id == unit_id)
                    .ok_or_else(|| "review unit disappeared before adoption".to_string())?;
                let projected_current =
                    codex_history::task_context::project_completion_review_state(current)?;
                if projected_current != unit
                    || projected_current.review_evidence.as_ref() != Some(&evidence)
                {
                    return Err("review evidence changed before adoption".into());
                }
                if let Some(attempt) = current
                    .review_state
                    .as_mut()
                    .and_then(|review_state| review_state.active_attempt.as_mut())
                {
                    if attempt.status != CompletionReviewAttemptStatus::Completed
                        || attempt.attempt_id != evidence.spawn_call_id
                        || attempt.reviewer_thread_id.as_deref()
                            != Some(evidence.reviewer_thread_id.as_str())
                        || attempt.reviewer_turn_id.as_deref()
                            != Some(evidence.reviewer_turn_id.as_str())
                        || attempt.wait_result_ref != evidence.wait_result_ref
                        || attempt.wait_call_id != evidence.wait_call_id
                    {
                        return Err(
                            "canonical review attempt is not the exact collected generation".into(),
                        );
                    }
                    attempt.adoption_reason = Some(reason.clone());
                }
                current.status = UnitStatus::Done;
                current.review_decision_reason = Some(reason.clone());
                if current.review_state.is_some() {
                    *current =
                        codex_history::task_context::project_completion_review_state(current)?;
                }
                current.id.clone()
            };
            let done = contract
                .units
                .iter()
                .filter(|candidate| candidate.status == UnitStatus::Done)
                .map(|candidate| candidate.id.as_str())
                .collect::<std::collections::HashSet<_>>();
            contract.current_unit = contract
                .units
                .iter()
                .find(|candidate| {
                    matches!(candidate.status, UnitStatus::Planned | UnitStatus::Ready)
                        && candidate
                            .depends_on
                            .iter()
                            .all(|id| done.contains(id.as_str()))
                })
                .map(|candidate| candidate.id.clone());
            contract.activity_at = Some(chrono::Utc::now().timestamp());
            CompletionReviewAdoption {
                root_task_id,
                unit_id: adopted_unit_id,
                target_task_id: evidence.target_task_id.clone(),
                target_task_revision: evidence.target_task_revision,
                reviewer_thread_id: evidence.reviewer_thread_id.clone(),
            }
        };
        if let Err(error) = self.persist_autonomous_completion_snapshot().await {
            self.pause_autonomous_completion().await;
            return Err(format!(
                "review adoption commit failed and autonomous work was paused: {error}"
            ));
        }
        Ok(adopted)
    }

    pub(crate) async fn revalidate_collected_completion_reviews(&self) {
        let collected = {
            let state = self.state.lock().await;
            let Some((_, contract)) = state.rencrow_completion_state.as_ref() else {
                return;
            };
            let Ok(projected_units) =
                super::completion_review::projected_completion_review_units(contract)
            else {
                return;
            };
            projected_units
                .into_iter()
                .filter_map(|unit| {
                    // Canonical terminal facts are retained as history; their current
                    // freshness is checked by the shared completion predicate. This
                    // compatibility cleanup only handles pre-canonical legacy evidence.
                    if unit.review_state.is_some() || unit.status == UnitStatus::Done {
                        return None;
                    }
                    let evidence = unit.review_evidence.as_ref()?;
                    evidence
                        .wait_result_ref
                        .as_ref()
                        .map(|_| (unit.id, evidence.clone()))
                })
                .collect::<Vec<_>>()
        };
        for (unit_id, evidence) in collected {
            if self
                .current_completion_review_target_ref(
                    &evidence.target_task_id,
                    evidence.target_task_revision,
                )
                .await
                .is_ok_and(|current| current == evidence.target_artifact_ref)
            {
                continue;
            }
            let changed = {
                let mut state = self.state.lock().await;
                let Some((_, contract)) = state.rencrow_completion_state.as_mut() else {
                    return;
                };
                let Some(unit) = contract.units.iter_mut().find(|unit| unit.id == unit_id) else {
                    continue;
                };
                if unit.review_state.is_none() && unit.review_evidence.as_ref() == Some(&evidence) {
                    unit.status = UnitStatus::Ready;
                    unit.review_evidence = None;
                    unit.review_decision_reason = None;
                    contract.current_unit = Some(unit_id);
                    true
                } else {
                    false
                }
            };
            if changed && self.persist_autonomous_completion_snapshot().await.is_err() {
                self.pause_autonomous_completion().await;
                return;
            }
        }
    }

    pub(crate) async fn validate_completion_review_spawn(
        &self,
        unit_id: &str,
        target_task_id: &str,
        target_task_revision: u64,
    ) -> Result<String, String> {
        let Some((_, contract)) = self.autonomous_completion_current_state().await else {
            return Err("completion review requires an active completion contract".into());
        };
        if contract.paused {
            return Err(
                "completion review is blocked while autonomous completion is paused".into(),
            );
        }
        let unit = contract
            .units
            .iter()
            .find(|unit| unit.id == unit_id)
            .ok_or_else(|| "completion review unit is missing".to_string())?;
        if unit.kind != CompletionUnitKind::Review
            || !matches!(unit.status, UnitStatus::Ready | UnitStatus::Running)
            || unit.review_target.as_ref().is_none_or(|target| {
                target.task_id != target_task_id || target.task_revision != target_task_revision
            })
            || unit.review_evidence.is_some()
        {
            return Err("completion review request does not match an available review unit".into());
        }
        self.current_completion_review_target_ref(target_task_id, target_task_revision)
            .await
    }

    pub(crate) async fn reserve_completion_review_launch(
        &self,
        unit_id: &str,
        target_task_id: &str,
        target_task_revision: u64,
        spawn_call_id: &str,
    ) -> Result<String, String> {
        let target_ref = self
            .validate_completion_review_spawn(unit_id, target_task_id, target_task_revision)
            .await?;
        {
            let mut state = self.state.lock().await;
            if state.rencrow_completion_paused {
                return Err(
                    "completion review is blocked while autonomous completion is paused".into(),
                );
            }
            let (_, contract) = state
                .rencrow_completion_state
                .as_mut()
                .ok_or_else(|| "completion plan disappeared before review launch".to_string())?;
            if contract.paused {
                return Err(
                    "completion review is blocked while autonomous completion is paused".into(),
                );
            }
            let unit = contract
                .units
                .iter_mut()
                .find(|unit| unit.id == unit_id)
                .ok_or_else(|| "completion review unit is missing".to_string())?;
            if unit.kind != CompletionUnitKind::Review
                || unit.status != UnitStatus::Ready
                || unit.review_evidence.is_some()
                || unit.review_launch.is_some()
                || unit.review_target.as_ref().is_none_or(|target| {
                    target.task_id != target_task_id || target.task_revision != target_task_revision
                })
            {
                return Err("completion review launch is already reserved or unresolved".into());
            }
            unit.status = UnitStatus::Running;
            unit.review_launch = Some(CompletionReviewLaunchState {
                spawn_call_id: spawn_call_id.to_string(),
                status: CompletionReviewLaunchStatus::Reserved,
                deadline_unix_ms: Some(
                    chrono::Utc::now().timestamp_millis() + COMPLETION_REVIEWER_SLA_MS,
                ),
                reason: None,
            });
            contract.current_unit = Some(unit_id.to_string());
            contract.activity_at = Some(chrono::Utc::now().timestamp());
        }
        if let Err(error) = self.persist_autonomous_completion_snapshot().await {
            self.pause_autonomous_completion().await;
            return Err(format!(
                "failed to persist completion review launch reservation: {error}"
            ));
        }
        Ok(target_ref)
    }

    pub(crate) async fn record_completion_review_spawn(
        &self,
        unit_id: &str,
        generation: CompletionReviewChildGeneration,
    ) -> Result<(), String> {
        let CompletionReviewChildGeneration {
            identity:
                CompletionReviewChildIdentity {
                    target: review_target,
                    target_artifact_ref,
                    reviewer_thread_id,
                    reviewer_role,
                    spawn_call_id,
                },
            reviewer_turn_id,
        } = generation;
        let reviewer_thread_id_string = reviewer_thread_id.to_string();

        // Persist the known child identity before any later generation or freshness
        // check can fail. A spawned child is never rolled back as if no child existed.
        self.record_completion_review_child_pending(
            unit_id,
            CompletionReviewChildIdentity {
                target: review_target.clone(),
                target_artifact_ref: target_artifact_ref.clone(),
                reviewer_thread_id,
                reviewer_role: reviewer_role.clone(),
                spawn_call_id: spawn_call_id.clone(),
            },
        )
        .await?;
        if self
            .current_completion_review_target_ref(
                &review_target.task_id,
                review_target.task_revision,
            )
            .await?
            != target_artifact_ref
        {
            return Err("review target artifact changed while the reviewer was spawning".into());
        }

        let _review_guard = self.completion_review_owner_guard().await;
        let root_task_id = self
            .state
            .lock()
            .await
            .rencrow_completion_state
            .as_ref()
            .map(|(root, _)| root.clone())
            .ok_or_else(|| "completion review contract disappeared during spawn".to_string())?;
        if self
            .persisted_completed_review_predecessor_turn(
                &root_task_id,
                unit_id,
                &spawn_call_id,
                &reviewer_thread_id_string,
            )
            .await?
            .as_deref()
            == Some(reviewer_turn_id.as_str())
        {
            return Err(
                "review follow-up cannot bind the predecessor's already-collected turn".into(),
            );
        }
        {
            let mut state = self.state.lock().await;
            let Some((_, contract)) = state.rencrow_completion_state.as_mut() else {
                return Err("completion review contract disappeared during spawn".into());
            };
            if contract.paused {
                return Err("completion review was paused during spawn".into());
            }
            let unit = contract
                .units
                .iter_mut()
                .find(|unit| unit.id == unit_id)
                .ok_or_else(|| "completion review unit disappeared during spawn".to_string())?;
            if unit.review_state.is_some() {
                *unit = codex_history::task_context::project_completion_review_state(unit)?;
            }
            if unit.kind != CompletionUnitKind::Review
                || unit.review_target.as_ref().is_none_or(|existing_target| {
                    existing_target.task_id != review_target.task_id
                        || existing_target.task_revision != review_target.task_revision
                })
                || unit.review_evidence.as_ref().is_none_or(|evidence| {
                    evidence.reviewer_thread_id != reviewer_thread_id_string
                        || evidence.spawn_call_id != spawn_call_id
                        || evidence.reviewer_turn_id
                            != format!("pending-generation:{spawn_call_id}")
                })
            {
                return Err(
                    "completion review child binding changed during generation lookup".into(),
                );
            }
            let Some(mut evidence) = unit.review_evidence.clone() else {
                return Err(
                    "completion review child evidence disappeared during generation lookup".into(),
                );
            };
            if !bind_completion_review_generation(
                &mut evidence,
                reviewer_thread_id,
                &spawn_call_id,
                reviewer_turn_id,
            ) {
                return Err("review generation does not match the unresolved child launch".into());
            }
            evidence.reviewer_role = reviewer_role.clone();
            evidence.target_artifact_ref = target_artifact_ref;
            let bound_turn_id = evidence.reviewer_turn_id.clone();
            let mut candidate = unit.clone();
            if candidate.review_state.is_some() {
                let attempt = candidate
                    .review_state
                    .as_mut()
                    .and_then(|review_state| review_state.active_attempt.as_mut())
                    .ok_or_else(|| {
                        "canonical review attempt disappeared during generation lookup".to_string()
                    })?;
                if attempt.attempt_id != spawn_call_id
                    || attempt.reviewer_thread_id.as_deref()
                        != Some(reviewer_thread_id_string.as_str())
                    || !matches!(
                        attempt.status,
                        CompletionReviewAttemptStatus::Reserved
                            | CompletionReviewAttemptStatus::Unknown
                    )
                    || attempt.reviewer_role != reviewer_role
                {
                    return Err("review generation does not match its canonical attempt".into());
                }
                attempt.reviewer_turn_id = Some(bound_turn_id);
                attempt.status = CompletionReviewAttemptStatus::Bound;
                attempt.launch_unknown_reason = None;
            }
            candidate.review_evidence = Some(evidence);
            if candidate.review_state.is_some() {
                candidate =
                    codex_history::task_context::project_completion_review_state(&candidate)?;
            }
            *unit = candidate;
        }
        self.persist_autonomous_completion_snapshot().await
    }

    /// Records only terminal results returned by the parent's native wait_agent call.
    pub(crate) async fn collect_completion_review_results(
        &self,
        wait_call_id: &str,
        statuses: &[(ThreadId, Option<String>, AgentStatus)],
    ) {
        let _review_guard = self.completion_review_owner_guard().await;
        let pending = {
            let state = self.state.lock().await;
            let Some((_, contract)) = state.rencrow_completion_state.as_ref() else {
                return;
            };
            let projected_units =
                match super::completion_review::projected_completion_review_units(contract) {
                    Ok(units) => units,
                    Err(_) => return,
                };
            projected_units
                .into_iter()
                .filter_map(|unit| {
                    let review = unit.review_evidence.as_ref()?;
                    review
                        .wait_result_ref
                        .is_none()
                        .then(|| (unit.id, review.clone()))
                })
                .collect::<Vec<_>>()
        };
        for (unit_id, mut review) in pending {
            let launch_pending = review.reviewer_turn_id.starts_with("pending-generation:");
            let pending_after = review.reviewer_turn_id.strip_prefix("pending-after:");
            let reviewer_id = ThreadId::try_from(review.reviewer_thread_id.as_str()).ok();
            let Some(status) = statuses.iter().find(|(thread_id, turn_id, status)| {
                reviewer_id == Some(*thread_id)
                    && if launch_pending {
                        false
                    } else if let Some(previous_turn) = pending_after {
                        matches!(status, AgentStatus::Shutdown)
                            || (turn_id
                                .as_deref()
                                .is_some_and(|current| current != previous_turn)
                                && matches!(
                                    status,
                                    AgentStatus::Completed(_) | AgentStatus::Errored(_)
                                ))
                    } else {
                        status_matches_review_generation(
                            &review,
                            *thread_id,
                            turn_id.as_deref(),
                            status,
                        )
                    }
            }) else {
                continue;
            };
            let expected_binding = review.reviewer_turn_id.clone();
            let exact_generation = !review.reviewer_turn_id.starts_with("pending-");
            if let Some(turn_id) = status.1.as_ref()
                && pending_after.is_some()
            {
                review.reviewer_turn_id = turn_id.clone();
            }
            match &status.2 {
                AgentStatus::Completed(Some(_)) => {}
                AgentStatus::Completed(None) | AgentStatus::Errored(_) | AgentStatus::Shutdown => {
                    let (cause, action) = match &status.2 {
                        AgentStatus::Completed(None) => (
                            "reviewer completed without a result summary",
                            "Use a new host-authorized review plan if another review is needed",
                        ),
                        AgentStatus::Errored(_) => (
                            "reviewer turn ended with an error",
                            "Diagnose the reviewer failure before creating a new authorized plan",
                        ),
                        AgentStatus::Shutdown => (
                            "reviewer was shut down before returning a result",
                            "Resolve the shutdown cause before creating a new authorized plan",
                        ),
                        _ => unreachable!(),
                    };
                    let Ok(failure_ref) = digest(&serde_json::json!({
                        "wait_call_id": wait_call_id,
                        "reviewer_thread_id": review.reviewer_thread_id,
                        "reviewer_turn_id": review.reviewer_turn_id,
                        "terminal_status": format!("{:?}", &status.2),
                        "cause": cause,
                    })) else {
                        continue;
                    };
                    let failure_source =
                        codex_history::task_context::CompletionReviewTerminalSource {
                            status: CompletionReviewTerminalStatus::Failed,
                            event: None,
                        };
                    let changed = {
                        let mut state = self.state.lock().await;
                        let Some((_, contract)) = state.rencrow_completion_state.as_mut() else {
                            return;
                        };
                        let Some(unit) = contract.units.iter_mut().find(|u| u.id == unit_id) else {
                            continue;
                        };
                        if unit.review_state.is_some() {
                            let Ok(projected) =
                                codex_history::task_context::project_completion_review_state(unit)
                            else {
                                continue;
                            };
                            *unit = projected;
                        }
                        let active_attempt_matches = unit
                            .review_state
                            .as_ref()
                            .and_then(|review_state| review_state.active_attempt.as_ref())
                            .is_some_and(|attempt| {
                                attempt.attempt_id == review.spawn_call_id
                                    && attempt.reviewer_thread_id.as_deref()
                                        == Some(review.reviewer_thread_id.as_str())
                                    && attempt.reviewer_turn_id.as_deref()
                                        == Some(review.reviewer_turn_id.as_str())
                            });
                        let exact_failure_already_recorded = active_attempt_matches
                            && unit.status == UnitStatus::Failed
                            && unit
                                .review_state
                                .as_ref()
                                .and_then(|review_state| review_state.active_attempt.as_ref())
                                .is_some_and(|attempt| {
                                    attempt.status == CompletionReviewAttemptStatus::Failed
                                        && attempt.terminal_source.as_ref().is_some_and(|source| {
                                            source.status == CompletionReviewTerminalStatus::Failed
                                        })
                                });
                        let legacy_failure_receipt_can_be_recorded =
                            legacy_review_failure_receipt_can_be_recorded(
                                unit,
                                &review,
                                &expected_binding,
                                exact_generation,
                                &status.2,
                            );
                        if (unit.status != UnitStatus::Running
                            && !exact_failure_already_recorded
                            && !legacy_failure_receipt_can_be_recorded)
                            || unit.review_evidence.as_ref().is_none_or(|current| {
                                current.reviewer_thread_id != review.reviewer_thread_id
                                    || current.reviewer_turn_id != expected_binding
                                    || current.spawn_call_id != review.spawn_call_id
                            })
                            || unit
                                .review_state
                                .as_ref()
                                .and_then(|review_state| review_state.active_attempt.as_ref())
                                .filter(|attempt| {
                                    attempt.attempt_id == review.spawn_call_id
                                        && attempt.reviewer_thread_id.as_deref()
                                            == Some(review.reviewer_thread_id.as_str())
                                        && attempt.reviewer_turn_id.as_deref()
                                            == Some(review.reviewer_turn_id.as_str())
                                })
                                .and_then(|attempt| attempt.terminal_source.as_ref())
                                .is_some_and(|source| {
                                    source.status != CompletionReviewTerminalStatus::Failed
                                })
                            || (unit.review_state.is_none()
                                && unit
                                    .review_evidence
                                    .as_ref()
                                    .and_then(|evidence| evidence.terminal_status)
                                    .is_some_and(|terminal| {
                                        terminal != CompletionReviewTerminalStatus::Failed
                                            || unit.status != UnitStatus::Failed
                                    }))
                        {
                            false
                        } else {
                            let previous_reviewer_model = unit
                                .review_state
                                .as_ref()
                                .and_then(|review_state| review_state.active_attempt.as_ref())
                                .filter(|_| active_attempt_matches)
                                .and_then(|attempt| attempt.reviewer_model.clone());
                            if let Some(review_state) = unit.review_state.as_mut() {
                                if active_attempt_matches {
                                    if let Some(attempt) = review_state.active_attempt.as_mut() {
                                        attempt.status = CompletionReviewAttemptStatus::Failed;
                                        // A later wait receipt adds collection evidence, but it
                                        // must not erase the earlier durable ReviewEnded source.
                                        if attempt.terminal_source.as_ref().is_none_or(|source| {
                                            source.status == CompletionReviewTerminalStatus::Failed
                                                && source.event.is_none()
                                        }) {
                                            attempt.terminal_source = Some(failure_source.clone());
                                        }
                                        attempt.wait_result_ref = Some(failure_ref.clone());
                                        attempt.wait_call_id = Some(wait_call_id.to_string());
                                    }
                                    review_state.retry_reservation_prior_historical_failure = None;
                                } else {
                                    let failure =
                                        codex_history::task_context::CompletionReviewHistoricalFailure {
                                            attempt_id: review.spawn_call_id.clone(),
                                            contract_revision: None,
                                            imported_from: None,
                                            consumed_retry_authorization: None,
                                            reviewer_thread_id: review.reviewer_thread_id.clone(),
                                            reviewer_turn_id: review.reviewer_turn_id.clone(),
                                            target_task_id: review.target_task_id.clone(),
                                            target_task_revision: review.target_task_revision,
                                            target_artifact_ref: review.target_artifact_ref.clone(),
                                            reviewer_role: review.reviewer_role.clone(),
                                            reviewer_model: previous_reviewer_model,
                                            deadline_unix_ms: review.deadline_unix_ms,
                                            wait_result_ref: Some(failure_ref.clone()),
                                            wait_call_id: Some(wait_call_id.to_string()),
                                            deadline_escalation: review.deadline_escalation.clone(),
                                            deadline_safety_fingerprint: review.deadline_safety_fingerprint.clone(),
                                            terminal_source: failure_source.clone(),
                                        };
                                    match review_state.historical_failure.as_mut() {
                                        Some(existing)
                                            if existing.attempt_id == failure.attempt_id
                                                && existing.reviewer_thread_id
                                                    == failure.reviewer_thread_id
                                                && existing.reviewer_turn_id
                                                    == failure.reviewer_turn_id =>
                                        {
                                            existing.wait_result_ref = Some(failure_ref.clone());
                                            existing.wait_call_id = Some(wait_call_id.to_string());
                                            if existing.terminal_source.event.is_none() {
                                                existing.terminal_source = failure.terminal_source;
                                            }
                                        }
                                        None => review_state.historical_failure = Some(failure),
                                        // A stale receipt must not replace a more recent
                                        // bounded historical fact for another attempt.
                                        Some(_) => {}
                                    }
                                }
                            } else if legacy_failure_receipt_can_be_recorded {
                                let Some(current) = unit.review_evidence.as_mut() else {
                                    continue;
                                };
                                current.wait_result_ref = Some(failure_ref.clone());
                                current.wait_call_id = Some(wait_call_id.to_string());
                                current.terminal_status =
                                    Some(CompletionReviewTerminalStatus::Failed);
                            }
                            // A late receipt for an older child records its historical
                            // failure without changing the status of a newer active attempt.
                            if unit.review_state.is_none() || active_attempt_matches {
                                unit.status = UnitStatus::Failed;
                            }
                            contract.failed_approaches.push(
                                codex_history::task_context::FailedApproach {
                                    summary: format!("review unit {} failed", unit.id),
                                    cause: cause.into(),
                                    next_action: action.into(),
                                    recorded_at: chrono::Utc::now().timestamp(),
                                },
                            );
                            if unit.review_state.is_some()
                                && let Ok(projected) =
                                    codex_history::task_context::project_completion_review_state(
                                        unit,
                                    )
                            {
                                *unit = projected;
                            }
                            true
                        }
                    };
                    if changed && self.persist_autonomous_completion_snapshot().await.is_err() {
                        self.pause_autonomous_completion().await;
                        return;
                    }
                    continue;
                }
                _ => continue,
            }
            let AgentStatus::Completed(Some(summary)) = &status.2 else {
                continue;
            };
            let current = self
                .current_completion_review_target_ref(
                    &review.target_task_id,
                    review.target_task_revision,
                )
                .await;
            if !matches!(
                current.as_deref(),
                Ok(current_ref) if current_ref == review.target_artifact_ref
            ) {
                continue;
            }
            let Ok(result_ref) = digest(&serde_json::json!({
                "wait_call_id": wait_call_id,
                "reviewer_thread_id": review.reviewer_thread_id,
                "reviewer_turn_id": review.reviewer_turn_id,
                "status": "completed",
                "summary": summary,
            })) else {
                continue;
            };
            review.wait_result_ref = Some(result_ref);
            review.terminal_status = Some(CompletionReviewTerminalStatus::Completed);
            review.wait_call_id = Some(wait_call_id.to_string());
            let changed = {
                let mut state = self.state.lock().await;
                let Some((_, contract)) = state.rencrow_completion_state.as_mut() else {
                    return;
                };
                let paused = contract.paused;
                let Some(unit) = contract.units.iter_mut().find(|unit| unit.id == unit_id) else {
                    continue;
                };
                if unit.review_state.is_some() {
                    let Ok(projected) =
                        codex_history::task_context::project_completion_review_state(unit)
                    else {
                        continue;
                    };
                    *unit = projected;
                }
                let canonical_attempt_accepts_success =
                    unit.review_state.as_ref().is_none_or(|review_state| {
                        review_state.active_attempt.as_ref().is_some_and(|attempt| {
                            attempt.attempt_id == review.spawn_call_id
                                && attempt.reviewer_thread_id.as_deref()
                                    == Some(review.reviewer_thread_id.as_str())
                                && attempt.reviewer_turn_id.as_deref()
                                    == Some(expected_binding.as_str())
                                && attempt.status != CompletionReviewAttemptStatus::Failed
                                && attempt.status != CompletionReviewAttemptStatus::Cancelled
                                && attempt.terminal_source.as_ref().is_none_or(|source| {
                                    source.status != CompletionReviewTerminalStatus::Failed
                                        && source.status
                                            != CompletionReviewTerminalStatus::Cancelled
                                })
                        })
                    });
                if canonical_attempt_accepts_success
                    && unit.review_evidence.as_ref().is_some_and(|current| {
                        current.reviewer_thread_id == review.reviewer_thread_id
                            && current.reviewer_turn_id == expected_binding
                            && current.spawn_call_id == review.spawn_call_id
                            && current.wait_result_ref.is_none()
                            && !paused
                    })
                {
                    unit.review_evidence = Some(review.clone());
                    if let Some(attempt) = unit
                        .review_state
                        .as_mut()
                        .and_then(|review_state| review_state.active_attempt.as_mut())
                        .filter(|attempt| {
                            attempt.attempt_id == review.spawn_call_id
                                && attempt.reviewer_thread_id.as_deref()
                                    == Some(review.reviewer_thread_id.as_str())
                                && attempt.reviewer_turn_id.as_deref()
                                    == Some(expected_binding.as_str())
                        })
                    {
                        attempt.status = CompletionReviewAttemptStatus::Completed;
                        attempt.terminal_source = Some(
                            codex_history::task_context::CompletionReviewTerminalSource {
                                status: CompletionReviewTerminalStatus::Completed,
                                event: None,
                            },
                        );
                        attempt.wait_result_ref = review.wait_result_ref.clone();
                        attempt.wait_call_id = review.wait_call_id.clone();
                    }
                    if let Some(review_state) = unit.review_state.as_mut()
                        && review_state.active_attempt.as_ref().is_some_and(|attempt| {
                            attempt.attempt_id == review.spawn_call_id
                                && attempt.reviewer_thread_id.as_deref()
                                    == Some(review.reviewer_thread_id.as_str())
                                && attempt.reviewer_turn_id.as_deref()
                                    == Some(expected_binding.as_str())
                        })
                    {
                        review_state.retry_reservation_prior_historical_failure = None;
                    }
                    if contract.blocker.as_ref().is_some_and(|blocker| {
                        blocker.review_owner.as_ref().is_some_and(|owner| {
                            owner.unit_id == unit_id && owner.attempt_id == review.spawn_call_id
                        })
                    }) {
                        contract.blocker = None;
                    }
                    if unit.review_state.is_some()
                        && let Ok(projected) =
                            codex_history::task_context::project_completion_review_state(unit)
                    {
                        *unit = projected;
                    }
                    true
                } else {
                    false
                }
            };
            if changed && self.persist_autonomous_completion_snapshot().await.is_err() {
                self.pause_autonomous_completion().await;
                return;
            }
        }
    }

    pub(super) async fn current_completion_review_target_ref(
        &self,
        task_id: &str,
        task_revision: u64,
    ) -> Result<String, String> {
        let active_root = self
            .autonomous_completion_current_state()
            .await
            .map(|(root, _)| root)
            .ok_or_else(|| "completion review contract is missing".to_string())?;
        let task_state = self
            .latest_rencrow_task_state()
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "review target has no accepted task receipt".to_string())?;
        if task_state.boundary.root_task_id != active_root {
            return Err("review target receipt belongs to a different completion root".into());
        }
        let accepted = task_state
            .completed
            .get(task_id)
            .cloned()
            .ok_or_else(|| "review target has no accepted task receipt".to_string())?;
        if accepted.revision != task_revision {
            return Err("review target task revision is no longer current".into());
        }
        let attempt = self
            .latest_task_check_for_scope(task_id, task_revision)
            .await
            .ok_or_else(|| "review target has no current host verification".to_string())?;
        let receipt = match attempt.status {
            crate::task_check::TaskCheckAttemptStatus::Finished(receipt) => receipt,
            crate::task_check::TaskCheckAttemptStatus::Pending => {
                return Err("review target verification is still running".into());
            }
            crate::task_check::TaskCheckAttemptStatus::ChangedArtifacts => {
                return Err("review target verification is stale".into());
            }
        };
        let host_ref = format!(
            "task-check:{}:{}",
            receipt.call_id,
            task_check_artifact_fingerprint(&receipt)?
        );
        if !receipt.succeeded
            || receipt.exit_code != 0
            || receipt.artifact_before != receipt.artifact_after
            || accepted.artifact_ref != host_ref
            || accepted.verification_ref != host_ref
        {
            return Err("review target does not match its accepted stable verification".into());
        }
        let paths = receipt
            .artifact_after
            .iter()
            .map(|stamp| stamp.path.clone())
            .collect::<Vec<_>>();
        let current = crate::task_check::hash_workspace_artifacts(&receipt.workspace_root, &paths)
            .ok_or_else(|| "review target artifacts are unavailable".to_string())?;
        if current != receipt.artifact_after {
            return Err("review target artifacts changed after verification".into());
        }
        Ok(accepted.artifact_ref)
    }

    pub(crate) async fn has_uncollected_completion_children(&self) -> bool {
        !self
            .state
            .lock()
            .await
            .rencrow_completion_uncollected_children
            .is_empty()
    }

    pub(crate) async fn prepare_completion_review_followup(
        &self,
        child_thread_id: ThreadId,
        call_id: &str,
    ) -> Result<Option<CompletionReviewFollowupPreparation>, String> {
        if call_id.trim().is_empty() {
            return Err("review follow-up requires the host send_input call ID".into());
        }
        let _review_guard = self.completion_review_owner_guard().await;
        let child_id = child_thread_id.to_string();
        let (root_task_id, contract_revision, prior_unit, prior_attempt) = {
            let state = self.state.lock().await;
            let Some((root_task_id, contract)) = state.rencrow_completion_state.as_ref() else {
                return Ok(None);
            };
            let matches = super::completion_review::projected_completion_review_units(contract)?
                .into_iter()
                .filter_map(|unit| {
                    let attempt = unit
                        .review_state
                        .as_ref()
                        .and_then(|review| review.active_attempt.as_ref())?;
                    (unit.status == UnitStatus::Running
                        && unit.kind == CompletionUnitKind::Review
                        && attempt.status == CompletionReviewAttemptStatus::Completed
                        && attempt.adoption_reason.is_none()
                        && attempt.reviewer_thread_id.as_deref() == Some(child_id.as_str())
                        && attempt.terminal_source.as_ref().is_some_and(|source| {
                            source.status == CompletionReviewTerminalStatus::Completed
                        })
                        && attempt
                            .wait_result_ref
                            .as_deref()
                            .is_some_and(|value| !value.is_empty())
                        && attempt
                            .wait_call_id
                            .as_deref()
                            .is_some_and(|value| !value.is_empty()))
                    .then(|| (unit.id, attempt.clone()))
                })
                .collect::<Vec<_>>();
            match matches.as_slice() {
                [] => return Ok(None),
                [(unit_id, attempt)] => {
                    if state.rencrow_completion_paused {
                        return Err(
                            "review follow-up is blocked while autonomous completion is paused"
                                .into(),
                        );
                    }
                    if contract.paused {
                        return Err(
                            "review follow-up is blocked while its completion plan is paused"
                                .into(),
                        );
                    }
                    if state
                        .rencrow_completion_uncollected_children
                        .contains(&child_id)
                    {
                        return Err(
                            "review follow-up child already has uncollected runtime work".into(),
                        );
                    }
                    let prior_unit = contract
                        .units
                        .iter()
                        .find(|candidate| candidate.id == *unit_id)
                        .ok_or_else(|| {
                            "review obligation disappeared during selection".to_string()
                        })?
                        .clone();
                    (
                        root_task_id.clone(),
                        contract.contract_revision,
                        prior_unit,
                        attempt.clone(),
                    )
                }
                _ => return Err("multiple unadopted review attempts match this child".into()),
            }
        };
        let unit_id = prior_unit.id.clone();
        let Some(prior_snapshot_ref) = self
            .persisted_review_attempt_snapshot_ref(&root_task_id, &unit_id, &prior_attempt)
            .await?
        else {
            return Err(
                "review follow-up requires an exact persisted snapshot of the collected attempt"
                    .into(),
            );
        };
        if prior_attempt.attempt_id == call_id {
            return Err("review follow-up call ID reused the predecessor attempt ID".into());
        }
        let (reserved_unit, ledger_inserted) = {
            let mut state = self.state.lock().await;
            if state.rencrow_completion_paused {
                return Err("review follow-up was paused before reservation".into());
            }
            let Some((current_root, contract)) = state.rencrow_completion_state.as_ref() else {
                return Err("completion plan disappeared before review follow-up".into());
            };
            if current_root != &root_task_id
                || contract.paused
                || contract.contract_revision != contract_revision
                || contract.units.iter().find(|unit| unit.id == unit_id) != Some(&prior_unit)
                || state
                    .rencrow_completion_uncollected_children
                    .contains(&child_id)
            {
                return Err("review attempt changed before follow-up submission".into());
            }
            let mut reserved_unit = prior_unit.clone();
            let canonical_attempt = reserved_unit
                .review_state
                .as_mut()
                .and_then(|review| review.active_attempt.as_mut())
                .ok_or_else(|| {
                    "canonical completed review disappeared before follow-up".to_string()
                })?;
            if canonical_attempt != &prior_attempt
                || canonical_attempt.status != CompletionReviewAttemptStatus::Completed
                || canonical_attempt.adoption_reason.is_some()
            {
                return Err("review attempt changed before follow-up reservation".into());
            }
            canonical_attempt.attempt_id = call_id.to_string();
            canonical_attempt.status = CompletionReviewAttemptStatus::Reserved;
            canonical_attempt.launch_unknown_reason = None;
            canonical_attempt.consumed_retry_authorization = None;
            canonical_attempt.imported_from = None;
            canonical_attempt.reviewer_thread_id = Some(child_id.clone());
            canonical_attempt.reviewer_turn_id = None;
            canonical_attempt.deadline_unix_ms =
                Some(chrono::Utc::now().timestamp_millis() + COMPLETION_REVIEWER_SLA_MS);
            canonical_attempt.deadline_escalation = None;
            canonical_attempt.deadline_safety_fingerprint = None;
            canonical_attempt.terminal_source = None;
            canonical_attempt.wait_result_ref = None;
            canonical_attempt.wait_call_id = None;
            canonical_attempt.adoption_reason = None;
            reserved_unit
                .review_state
                .as_mut()
                .ok_or_else(|| "canonical review state disappeared before follow-up".to_string())?
                .previous_attempt_ref = Some(prior_snapshot_ref.clone());
            reserved_unit =
                codex_history::task_context::project_completion_review_state(&reserved_unit)?;
            let contract = state
                .rencrow_completion_state
                .as_mut()
                .map(|(_, contract)| contract)
                .ok_or_else(|| "completion plan disappeared before reservation".to_string())?;
            let unit = contract
                .units
                .iter_mut()
                .find(|unit| unit.id == unit_id)
                .ok_or_else(|| "review obligation disappeared before reservation".to_string())?;
            *unit = reserved_unit.clone();
            let ledger_inserted = state
                .rencrow_completion_uncollected_children
                .insert(child_id.clone());
            (reserved_unit, ledger_inserted)
        };
        if !ledger_inserted {
            self.pause_autonomous_completion().await;
            return Err("review follow-up could not reserve its child exactly once".into());
        }
        if let Err(error) = self.persist_autonomous_completion_snapshot().await {
            self.pause_autonomous_completion().await;
            return Err(format!(
                "could not persist the reviewer follow-up reservation: {error}"
            ));
        }
        Ok(Some(CompletionReviewFollowupPreparation {
            root_task_id,
            unit_id,
            child_thread_id,
            call_id: call_id.to_string(),
            contract_revision,
            prior_unit,
            reserved_unit,
        }))
    }

    pub(crate) async fn record_completion_review_followup_started(
        &self,
        preparation: CompletionReviewFollowupPreparation,
        started_turn_id: String,
    ) -> Result<(), String> {
        let prior_turn_id = preparation
            .prior_unit
            .review_state
            .as_ref()
            .and_then(|review| review.active_attempt.as_ref())
            .and_then(|attempt| attempt.reviewer_turn_id.as_deref());
        if started_turn_id.trim().is_empty() || prior_turn_id == Some(started_turn_id.as_str()) {
            return Err("review follow-up did not start a distinct child generation".into());
        }
        let _review_guard = self.completion_review_owner_guard().await;
        let child_thread_id = preparation.child_thread_id.to_string();
        {
            let mut state = self.state.lock().await;
            if state.rencrow_completion_paused {
                return Err(
                    "review follow-up binding is blocked while autonomous completion is paused"
                        .into(),
                );
            }
            let child_is_uncollected = state
                .rencrow_completion_uncollected_children
                .contains(&child_thread_id);
            let Some((root_task_id, contract)) = state.rencrow_completion_state.as_mut() else {
                return Err("completion plan disappeared after reviewer follow-up".into());
            };
            if root_task_id != &preparation.root_task_id
                || contract.paused
                || contract.contract_revision != preparation.contract_revision
            {
                return Err("completion root or contract changed after reviewer follow-up".into());
            }
            let current_conditions_sha256 =
                super::completion_review::completion_review_conditions_sha256(
                    contract,
                    &preparation.reserved_unit.condition_refs,
                )?;
            let Some(unit) = contract
                .units
                .iter_mut()
                .find(|unit| unit.id == preparation.unit_id)
            else {
                return Err("review obligation disappeared after reviewer follow-up".into());
            };
            if unit != &preparation.reserved_unit || !child_is_uncollected {
                return Err(
                    "review follow-up reservation changed before its generation was bound".into(),
                );
            }
            let mut candidate = unit.clone();
            let attempt = candidate
                .review_state
                .as_mut()
                .and_then(|review| review.active_attempt.as_mut())
                .ok_or_else(|| "review follow-up lost its reserved attempt".to_string())?;
            if attempt.attempt_id != preparation.call_id
                || attempt.status != CompletionReviewAttemptStatus::Reserved
                || attempt.reviewer_thread_id.as_deref() != Some(child_thread_id.as_str())
                || attempt.reviewer_turn_id.is_some()
                || attempt.terminal_source.is_some()
                || attempt.wait_result_ref.is_some()
                || attempt.wait_call_id.is_some()
                || attempt.adoption_reason.is_some()
                || attempt.conditions_sha256 != current_conditions_sha256
            {
                return Err(
                    "review follow-up reservation no longer matches its start conditions".into(),
                );
            }
            attempt.reviewer_turn_id = Some(started_turn_id);
            attempt.status = CompletionReviewAttemptStatus::Bound;
            attempt.launch_unknown_reason = None;
            candidate = codex_history::task_context::project_completion_review_state(&candidate)?;
            *unit = candidate;
        }
        if let Err(error) = self.persist_autonomous_completion_snapshot().await {
            self.pause_autonomous_completion().await;
            return Err(format!(
                "review follow-up started but its new generation could not be persisted: {error}"
            ));
        }
        Ok(())
    }

    pub(crate) async fn cancel_completion_review_followup_not_submitted(
        &self,
        preparation: CompletionReviewFollowupPreparation,
    ) -> Result<(), String> {
        let _review_guard = self.completion_review_owner_guard().await;
        let child_thread_id = preparation.child_thread_id.to_string();
        let rollback = {
            let mut state = self.state.lock().await;
            let Some((root_task_id, contract)) = state.rencrow_completion_state.as_ref() else {
                return Err("completion plan disappeared before send rejection cleanup".to_string());
            };
            if root_task_id != &preparation.root_task_id
                || contract.contract_revision != preparation.contract_revision
                || contract
                    .units
                    .iter()
                    .find(|unit| unit.id == preparation.unit_id)
                    != Some(&preparation.reserved_unit)
                || !state
                    .rencrow_completion_uncollected_children
                    .contains(&child_thread_id)
            {
                Err("review attempt changed before rejected follow-up cleanup".to_string())
            } else {
                let contract = state
                    .rencrow_completion_state
                    .as_mut()
                    .map(|(_, contract)| contract)
                    .ok_or_else(|| "completion plan disappeared during cleanup".to_string())?;
                let unit = contract
                    .units
                    .iter_mut()
                    .find(|unit| unit.id == preparation.unit_id)
                    .ok_or_else(|| "review obligation disappeared during cleanup".to_string())?;
                *unit = preparation.prior_unit.clone();
                state
                    .rencrow_completion_uncollected_children
                    .remove(&child_thread_id);
                Ok(())
            }
        };
        if let Err(error) = rollback {
            self.pause_autonomous_completion().await;
            return Err(error);
        }
        if let Err(error) = self.persist_autonomous_completion_snapshot().await {
            {
                let mut state = self.state.lock().await;
                if let Some((root, contract)) = state.rencrow_completion_state.as_mut()
                    && root == &preparation.root_task_id
                    && contract.contract_revision == preparation.contract_revision
                    && let Some(unit) = contract
                        .units
                        .iter_mut()
                        .find(|unit| unit.id == preparation.unit_id)
                    && unit == &preparation.prior_unit
                {
                    *unit = preparation.reserved_unit.clone();
                }
                state
                    .rencrow_completion_uncollected_children
                    .insert(child_thread_id);
            }
            self.pause_autonomous_completion().await;
            return Err(format!(
                "could not persist rejected follow-up cleanup; child remains unresolved: {error}"
            ));
        }
        Ok(())
    }

    pub(crate) async fn record_completion_review_followup_opaque_or_ambiguous(
        &self,
        preparation: CompletionReviewFollowupPreparation,
        reason: String,
    ) -> Result<(), String> {
        let _review_guard = self.completion_review_owner_guard().await;
        let child_thread_id = preparation.child_thread_id.to_string();
        let reason = reason.chars().take(/*n*/ 1024).collect::<String>();
        let updated = {
            let mut state = self.state.lock().await;
            let child_is_uncollected = state
                .rencrow_completion_uncollected_children
                .contains(&child_thread_id);
            let Some((root, contract)) = state.rencrow_completion_state.as_mut() else {
                return Err("completion plan disappeared during ambiguous follow-up".into());
            };
            if root != &preparation.root_task_id
                || contract.contract_revision != preparation.contract_revision
                || !child_is_uncollected
            {
                false
            } else {
                let Some(unit) = contract
                    .units
                    .iter_mut()
                    .find(|unit| unit.id == preparation.unit_id)
                else {
                    return Err("review obligation disappeared during ambiguous follow-up".into());
                };
                if unit == &preparation.reserved_unit {
                    let mut candidate = unit.clone();
                    let attempt = candidate
                        .review_state
                        .as_mut()
                        .and_then(|review| review.active_attempt.as_mut())
                        .ok_or_else(|| "reserved follow-up lost its attempt".to_string())?;
                    attempt.status = CompletionReviewAttemptStatus::Unknown;
                    attempt.launch_unknown_reason = Some(if reason.trim().is_empty() {
                        "follow-up submission outcome is unknown".into()
                    } else {
                        reason.clone()
                    });
                    candidate =
                        codex_history::task_context::project_completion_review_state(&candidate)?;
                    *unit = candidate;
                    true
                } else {
                    // A failed persistence after exact Started keeps the observed binding.
                    false
                }
            }
        };
        tracing::warn!(
            child_thread_id = %preparation.child_thread_id,
            attempt_id = %preparation.call_id,
            %reason,
            "reviewer follow-up has no exact new generation"
        );
        if updated && let Err(error) = self.persist_autonomous_completion_snapshot().await {
            self.pause_autonomous_completion().await;
            return Err(format!(
                "could not persist the unresolved follow-up attempt: {error}"
            ));
        }
        self.pause_autonomous_completion().await;
        Ok(())
    }

    pub(crate) async fn observe_completion_child_tool(
        &self,
        tool: &str,
        args: &Value,
        value: &Value,
    ) {
        let mut add = Vec::new();
        let mut remove = Vec::new();
        let mut invalidate_review_children = Vec::new();
        let close_status = (tool == "close_agent")
            .then(|| serde_json::from_value::<AgentStatus>(value["previous_status"].clone()).ok())
            .flatten();
        match tool {
            "spawn_agent" => add.extend(value["agent_id"].as_str()),
            "send_input"
                if value["submission_id"]
                    .as_str()
                    .is_some_and(|submission_id| !submission_id.trim().is_empty()) =>
            {
                add.extend(args["target"].as_str());
                invalidate_review_children.extend(args["target"].as_str());
            }
            "send_input" => return,
            "resume_agent" if terminal(&value["status"]) => {
                remove.extend(args["id"].as_str());
                invalidate_review_children.extend(args["id"].as_str());
            }
            "resume_agent" => {
                add.extend(args["id"].as_str());
                invalidate_review_children.extend(args["id"].as_str());
            }
            "close_agent" => {
                remove.extend(args["target"].as_str());
                invalidate_review_children.extend(args["target"].as_str());
            }
            "wait_agent" => {
                if let Some(statuses) = value["status"].as_object() {
                    for (id, status) in statuses {
                        if terminal(status) {
                            remove.push(id.as_str());
                        }
                    }
                }
            }
            _ => return,
        }
        let rebind_generations =
            futures::future::join_all(invalidate_review_children.iter().filter_map(|id| {
                let thread_id = ThreadId::try_from(*id).ok()?;
                Some(async move {
                    let generation = self
                        .services
                        .agent_control
                        .get_agent_status_generation(thread_id)
                        .await
                        .ok()
                        .and_then(|(turn_id, _)| turn_id);
                    (thread_id.to_string(), generation)
                })
            }))
            .await
            .into_iter()
            .collect::<std::collections::HashMap<_, _>>();
        let changed = {
            let mut state = self.state.lock().await;
            let mut changed = false;
            for id in add {
                if codex_protocol::ThreadId::try_from(id).is_ok() {
                    changed |= state
                        .rencrow_completion_uncollected_children
                        .insert(id.to_owned());
                }
            }
            let mut collected = false;
            for id in remove {
                let canonical_review_still_uncollected = state
                    .rencrow_completion_state
                    .as_ref()
                    .is_some_and(|(_, contract)| {
                        contract.units.iter().any(|unit| {
                            unit.review_evidence.as_ref().is_some_and(|evidence| {
                                evidence.reviewer_thread_id == id
                                    && unit.review_state.as_ref().is_some_and(|review| {
                                        review.active_attempt.as_ref().is_none_or(|attempt| {
                                            if attempt.wait_result_ref.is_some() {
                                                false
                                            } else {
                                                let exact_failed_event =
                                                    attempt.status
                                                        == CompletionReviewAttemptStatus::Failed
                                                        && attempt.attempt_id.as_str()
                                                            == evidence.spawn_call_id.as_str()
                                                        && attempt.reviewer_thread_id.as_deref()
                                                            == Some(id)
                                                        && attempt.reviewer_turn_id.as_deref()
                                                            == Some(evidence.reviewer_turn_id.as_str())
                                                        && attempt.terminal_source.as_ref().is_some_and(
                                                            |source| {
                                                                source.status
                                                                    == CompletionReviewTerminalStatus::Failed
                                                                    && source.event.as_ref().is_some_and(
                                                                        |event| {
                                                                            event.owner_thread_id.as_str() == id
                                                                                && event.turn_id.as_str()
                                                                                    == evidence.reviewer_turn_id.as_str()
                                                                                && event.owner_agent_role.as_deref()
                                                                                    == Some(attempt.reviewer_role.as_str())
                                                                                && event.reason == "ReviewEnded"
                                                                        },
                                                                    )
                                                            },
                                                        );
                                                !exact_failed_event
                                            }
                                        })
                                    })
                            })
                        })
                    });
                if !canonical_review_still_uncollected {
                    collected |= state.rencrow_completion_uncollected_children.remove(id);
                }
            }
            let rebind_ids = invalidate_review_children
                .into_iter()
                .filter_map(|id| ThreadId::try_from(id).ok())
                .map(|id| id.to_string())
                .collect::<std::collections::HashSet<_>>();
            if !rebind_ids.is_empty()
                && let Some((_, contract)) = state.rencrow_completion_state.as_mut()
            {
                for unit in &mut contract.units {
                    if unit.review_state.is_some() {
                        // Canonical generations change only through the typed owner path.
                        // A generic follow-up/resume event has no authority to rewrite its
                        // terminal or adoption facts.
                        continue;
                    }
                    let Some(review_thread_id) = unit
                        .review_evidence
                        .as_ref()
                        .map(|review| review.reviewer_thread_id.clone())
                    else {
                        continue;
                    };
                    if unit.status != UnitStatus::Done && rebind_ids.contains(&review_thread_id) {
                        let already_collected_success =
                            unit.review_evidence.as_ref().is_some_and(|evidence| {
                                evidence.terminal_status
                                    == Some(CompletionReviewTerminalStatus::Completed)
                                    && evidence
                                        .wait_result_ref
                                        .as_ref()
                                        .is_some_and(|result_ref| !result_ref.trim().is_empty())
                                    && evidence
                                        .wait_call_id
                                        .as_ref()
                                        .is_some_and(|call_id| !call_id.trim().is_empty())
                            });
                        if tool == "close_agent" && already_collected_success {
                            continue;
                        }
                        if tool == "send_input"
                            && (unit.status == UnitStatus::Failed || !already_collected_success)
                        {
                            continue;
                        }
                        if tool == "close_agent" {
                            if unit.status == UnitStatus::Failed {
                                continue;
                            }
                            let cause = close_status.as_ref().and_then(close_failure_cause);
                            if let Some(cause) = cause {
                                let Some(evidence) = unit.review_evidence.as_mut() else {
                                    continue;
                                };
                                let previous_turn = evidence.reviewer_turn_id.clone();
                                let observed_turn = rebind_generations
                                    .get(&review_thread_id)
                                    .and_then(Clone::clone);
                                let (exact_turn, cause) = if previous_turn
                                    .starts_with("pending-after:")
                                {
                                    match observed_turn.filter(|turn_id| {
                                        turn_id != previous_turn.trim_start_matches("pending-after:")
                                    }) {
                                        Some(turn_id) => (Some(turn_id), cause),
                                        None => (
                                            None,
                                            "reviewer closed before the bound review result was collected".into(),
                                        ),
                                    }
                                } else if previous_turn.starts_with("pending-") {
                                    (
                                        None,
                                        "reviewer closed before the bound review result was collected".into(),
                                    )
                                } else if observed_turn.as_deref() == Some(previous_turn.as_str()) {
                                    (Some(previous_turn.clone()), cause)
                                } else {
                                    (
                                        None,
                                        "reviewer closed before the bound review result was collected".into(),
                                    )
                                };
                                if let Some(exact_turn) = exact_turn {
                                    evidence.reviewer_turn_id = exact_turn;
                                }
                                if let Some(attempt) = unit
                                    .review_state
                                    .as_mut()
                                    .and_then(|review| review.active_attempt.as_mut())
                                    .filter(|attempt| {
                                        attempt.attempt_id == evidence.spawn_call_id
                                            && attempt.reviewer_thread_id.as_deref()
                                                == Some(evidence.reviewer_thread_id.as_str())
                                    })
                                    && matches!(
                                        attempt.status,
                                        CompletionReviewAttemptStatus::Reserved
                                            | CompletionReviewAttemptStatus::Unknown
                                            | CompletionReviewAttemptStatus::Bound
                                            | CompletionReviewAttemptStatus::TerminalUnknown
                                    )
                                {
                                    attempt.reviewer_turn_id =
                                        Some(evidence.reviewer_turn_id.clone());
                                    attempt.status = CompletionReviewAttemptStatus::TerminalUnknown;
                                    attempt.launch_unknown_reason = Some(
                                        "close observed a terminal child status without a collected wait result or exact durable ReviewEnded proof".into(),
                                    );
                                    attempt.wait_result_ref = None;
                                    attempt.wait_call_id = None;
                                    attempt.terminal_source = None;
                                    attempt.adoption_reason = None;
                                }
                                if unit.review_state.is_none() {
                                    unit.status = UnitStatus::Failed;
                                } else if let Ok(projected) =
                                    codex_history::task_context::project_completion_review_state(
                                        unit,
                                    )
                                {
                                    *unit = projected;
                                }
                                let summary = format!("review unit {} failed", unit.id);
                                if !contract.failed_approaches.iter().any(|failure| {
                                    failure.summary == summary && failure.cause == cause
                                }) {
                                    contract.failed_approaches.push(
                                        codex_history::task_context::FailedApproach {
                                            summary,
                                            cause,
                                            next_action: "Diagnose the reviewer failure before creating another authorized review plan".into(),
                                            recorded_at: chrono::Utc::now().timestamp(),
                                        },
                                    );
                                }
                                contract.current_unit = Some(unit.id.clone());
                                changed = true;
                                continue;
                            }
                        }
                        let already_invalidated =
                            unit.review_evidence.as_ref().is_some_and(|evidence| {
                                evidence.reviewer_turn_id.starts_with("pending-after:")
                            });
                        if tool == "close_agent" && already_invalidated {
                            continue;
                        }
                        if let Some(evidence) = unit.review_evidence.as_mut() {
                            let previous_turn = evidence.reviewer_turn_id.clone();
                            evidence.reviewer_turn_id = rebind_generations
                                .get(&review_thread_id)
                                .and_then(Clone::clone)
                                .filter(|turn_id| turn_id != &previous_turn)
                                .unwrap_or_else(|| format!("pending-after:{previous_turn}"));
                            evidence.wait_result_ref = None;
                            evidence.terminal_status = None;
                            evidence.wait_call_id = None;
                            if let Some(attempt) = unit
                                .review_state
                                .as_mut()
                                .and_then(|review| review.active_attempt.as_mut())
                                .filter(|attempt| {
                                    attempt.attempt_id == evidence.spawn_call_id
                                        && attempt.reviewer_thread_id.as_deref()
                                            == Some(evidence.reviewer_thread_id.as_str())
                                })
                            {
                                attempt.reviewer_turn_id = Some(evidence.reviewer_turn_id.clone());
                                attempt.status = CompletionReviewAttemptStatus::Bound;
                                attempt.launch_unknown_reason = None;
                                attempt.wait_result_ref = None;
                                attempt.wait_call_id = None;
                                attempt.terminal_source = None;
                                attempt.adoption_reason = None;
                            }
                        }
                        unit.status = UnitStatus::Running;
                        unit.review_decision_reason = None;
                        if unit.review_state.is_some() {
                            let Ok(projected) =
                                codex_history::task_context::project_completion_review_state(unit)
                            else {
                                continue;
                            };
                            *unit = projected;
                        }
                        contract.current_unit = Some(unit.id.clone());
                        changed = true;
                    }
                }
            }
            if collected && let Some((_, contract)) = state.rencrow_completion_state.as_mut() {
                contract.runtime_recovery_at = Some(chrono::Utc::now().timestamp());
            }
            changed || collected
        };
        if changed && let Err(error) = self.persist_autonomous_completion_snapshot().await {
            tracing::error!(%error, "failed to persist delegated child collection state");
            self.pause_autonomous_completion().await;
        }
    }
}

fn task_check_artifact_fingerprint(
    receipt: &crate::task_check::TaskCheckReceipt,
) -> Result<String, String> {
    let artifact_stamp_set = receipt
        .artifact_after
        .iter()
        .map(|stamp| {
            serde_json::json!({
                "path": stamp.path.to_string_lossy(),
                "sha256": stamp.sha256,
            })
        })
        .collect::<Vec<_>>();
    digest(&serde_json::json!({
        "task_id": receipt.task_id,
        "task_revision": receipt.task_revision,
        "workspace_root": receipt.workspace_root.to_string_lossy(),
        "artifacts": artifact_stamp_set,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::tests::attach_thread_persistence;
    use crate::session::tests::make_session_and_context;
    use codex_history::task_context::AcceptanceCondition;
    use codex_history::task_context::AcceptanceStatus;
    use codex_history::task_context::CompletionReviewAttempt;
    use codex_history::task_context::CompletionReviewAttemptStatus;
    use codex_history::task_context::CompletionReviewState;
    use codex_history::task_context::CompletionReviewTarget;
    use codex_history::task_context::CompletionState;
    use codex_history::task_context::CompletionUnit;
    use serde_json::json;

    fn pending_review_state(child_id: &str, turn_id: &str) -> CompletionState {
        CompletionState {
            contract_revision: 1,
            units_completed_since_review: 0,
            plan_review_at: None,
            plan_reviews: vec![],
            paused: false,
            consumed_host_events: vec![],
            conditions: vec![AcceptanceCondition {
                id: "AC-1".into(),
                revision: 1,
                result: "The task is correct".into(),
                scope: "CLI".into(),
                verification: "Check the artifact".into(),
                acceptance_criteria: "Expected artifact is present".into(),
                status: AcceptanceStatus::Open,
                evidence: vec![],
                unmet_reason: None,
            }],
            units: vec![CompletionUnit {
                id: "review-1".into(),
                purpose: "Review task-a".into(),
                status: UnitStatus::Running,
                kind: CompletionUnitKind::Review,
                condition_refs: vec!["AC-1".into()],
                depends_on: vec![],
                evidence: vec![],
                task_refs: vec![],
                review_target: Some(codex_history::task_context::CompletionReviewTarget {
                    task_id: "task-a".into(),
                    task_revision: 1,
                }),
                review_evidence: Some(CompletionReviewEvidence {
                    reviewer_thread_id: child_id.into(),
                    reviewer_turn_id: turn_id.into(),
                    reviewer_role: "analyst".into(),
                    spawn_call_id: "spawn-review".into(),
                    deadline_unix_ms: None,
                    deadline_escalation: None,
                    deadline_safety_fingerprint: None,
                    wait_result_ref: Some("sha256:old-result".into()),
                    terminal_status: Some(CompletionReviewTerminalStatus::Completed),
                    wait_call_id: Some("wait-old".into()),
                    target_task_id: "task-a".into(),
                    target_task_revision: 1,
                    target_artifact_ref: "task-check:target".into(),
                }),
                review_launch: None,
                review_decision_reason: None,
                review_state: None,
                review_revalidation_of: None,
            }],
            current_unit: Some("review-1".into()),
            failed_approaches: vec![],
            blocker: None,
            next_action: Some("Review task-a".into()),
            product_progress_at: None,
            runtime_recovery_at: None,
            activity_at: None,
        }
    }

    fn failed_legacy_review_state(child_id: &str, turn_id: &str) -> CompletionState {
        let mut contract = pending_review_state(child_id, turn_id);
        contract.units[0].status = UnitStatus::Failed;
        let evidence = contract.units[0]
            .review_evidence
            .as_mut()
            .expect("legacy review evidence exists");
        evidence.deadline_unix_ms = Some(1);
        evidence.deadline_escalation = Some("reviewer exceeded the host SLA".into());
        evidence.deadline_safety_fingerprint = Some(
            super::super::completion_review_deadline::review_ended_durable_fingerprint(turn_id),
        );
        evidence.wait_result_ref = None;
        evidence.terminal_status = Some(CompletionReviewTerminalStatus::Failed);
        evidence.wait_call_id = None;
        contract
            .failed_approaches
            .push(codex_history::task_context::FailedApproach {
                summary: "review unit review-1 failed".into(),
                cause: "reviewer exceeded the host SLA".into(),
                next_action: "Do not resubmit the same generation".into(),
                recorded_at: 1,
            });
        contract
    }

    #[tokio::test]
    async fn legacy_failed_review_collects_exact_native_wait_once_and_preserves_deadline_proof() {
        let (mut session, _) = make_session_and_context().await;
        attach_thread_persistence(&mut session).await;
        let child =
            ThreadId::try_from("01a1c606-48b8-7cf3-bb78-77b47f6b8c2f").expect("valid reviewer ID");
        let turn_id = "review-turn-deadline";
        let before = failed_legacy_review_state(&child.to_string(), turn_id);
        let original_evidence = before.units[0]
            .review_evidence
            .as_ref()
            .expect("legacy review evidence exists")
            .clone();
        session.state.lock().await.rencrow_completion_state = Some(("root".into(), before));

        let terminal_status = AgentStatus::Errored("ReviewEnded".into());
        session
            .collect_completion_review_results(
                "legacy-review-wait-once",
                &[(child, Some(turn_id.into()), terminal_status.clone())],
            )
            .await;
        let collected = session
            .rencrow_completion_state("root")
            .await
            .expect("native wait receipt should persist on the legacy failure");
        let unit = &collected.units[0];
        assert_eq!(unit.status, UnitStatus::Failed);
        assert!(unit.review_state.is_none());
        let evidence = unit
            .review_evidence
            .as_ref()
            .expect("legacy evidence remains the compatibility record");
        assert_eq!(
            evidence.reviewer_thread_id,
            original_evidence.reviewer_thread_id
        );
        assert_eq!(evidence.reviewer_turn_id, turn_id);
        assert_eq!(evidence.spawn_call_id, original_evidence.spawn_call_id);
        assert!(evidence.wait_result_ref.is_some());
        assert_eq!(
            evidence.wait_call_id.as_deref(),
            Some("legacy-review-wait-once")
        );
        assert_eq!(
            evidence.terminal_status,
            Some(CompletionReviewTerminalStatus::Failed)
        );
        assert_eq!(
            evidence.deadline_escalation,
            original_evidence.deadline_escalation
        );
        assert_eq!(
            evidence.deadline_safety_fingerprint,
            original_evidence.deadline_safety_fingerprint
        );
        assert_eq!(collected.failed_approaches.len(), 2);

        session
            .collect_completion_review_results(
                "legacy-review-wait-duplicate",
                &[(child, Some(turn_id.into()), terminal_status)],
            )
            .await;
        assert_eq!(
            session.rencrow_completion_state("root").await,
            Some(collected),
            "an already collected failed generation must not add another wait receipt"
        );

        let mut running = pending_review_state(&child.to_string(), turn_id);
        let running_evidence = running.units[0]
            .review_evidence
            .as_mut()
            .expect("legacy evidence exists");
        running_evidence.wait_result_ref = None;
        running_evidence.terminal_status = None;
        running_evidence.wait_call_id = None;
        session.state.lock().await.rencrow_completion_state = Some(("root".into(), running));
        session
            .collect_completion_review_results(
                "legacy-running-review-wait",
                &[(
                    child,
                    Some(turn_id.into()),
                    AgentStatus::Errored("reviewer failed".into()),
                )],
            )
            .await;
        let collected_running = session.rencrow_completion_state("root").await.unwrap();
        assert_eq!(collected_running.units[0].status, UnitStatus::Failed);
        let running_result = collected_running.units[0]
            .review_evidence
            .as_ref()
            .expect("legacy evidence remains present");
        assert!(running_result.wait_result_ref.is_some());
        assert_eq!(
            running_result.wait_call_id.as_deref(),
            Some("legacy-running-review-wait")
        );
    }

    #[tokio::test]
    async fn legacy_failed_review_rejects_unbound_and_conflicting_wait_generations() {
        let (mut session, _) = make_session_and_context().await;
        attach_thread_persistence(&mut session).await;
        let child =
            ThreadId::try_from("01a1c606-48b8-7cf3-bb78-77b47f6b8c2f").expect("valid reviewer ID");
        let turn_id = "review-turn-deadline";
        let before = failed_legacy_review_state(&child.to_string(), turn_id);
        session.state.lock().await.rencrow_completion_state = Some(("root".into(), before));

        session
            .collect_completion_review_results(
                "legacy-review-wrong-generation",
                &[(
                    child,
                    Some("different-review-turn".into()),
                    AgentStatus::Errored("ReviewEnded".into()),
                )],
            )
            .await;
        let after_wrong_generation = session.rencrow_completion_state("root").await.unwrap();
        let evidence = after_wrong_generation.units[0]
            .review_evidence
            .as_ref()
            .expect("legacy evidence remains present");
        assert!(evidence.wait_result_ref.is_none());
        assert!(evidence.wait_call_id.is_none());

        let mut contradictory = pending_review_state(&child.to_string(), turn_id);
        let contradictory_evidence = contradictory.units[0]
            .review_evidence
            .as_mut()
            .expect("legacy evidence exists");
        contradictory_evidence.wait_result_ref = None;
        contradictory_evidence.wait_call_id = None;
        contradictory_evidence.terminal_status = Some(CompletionReviewTerminalStatus::Completed);
        let contradictory = ("root".into(), contradictory);
        session.state.lock().await.rencrow_completion_state = Some(contradictory.clone());
        session
            .collect_completion_review_results(
                "legacy-review-contradictory-terminal",
                &[(
                    child,
                    Some(turn_id.into()),
                    AgentStatus::Errored("ReviewEnded".into()),
                )],
            )
            .await;
        assert_eq!(
            session.rencrow_completion_state("root").await,
            Some(contradictory.1),
            "a failed native status cannot overwrite an existing successful terminal fact"
        );

        let mut inconsistent = pending_review_state(&child.to_string(), turn_id);
        let inconsistent_evidence = inconsistent.units[0]
            .review_evidence
            .as_mut()
            .expect("legacy evidence exists");
        inconsistent_evidence.wait_result_ref = None;
        inconsistent_evidence.wait_call_id = None;
        inconsistent_evidence.terminal_status = Some(CompletionReviewTerminalStatus::Failed);
        let inconsistent = ("root".into(), inconsistent);
        session.state.lock().await.rencrow_completion_state = Some(inconsistent.clone());
        session
            .collect_completion_review_results(
                "legacy-review-inconsistent-status",
                &[(
                    child,
                    Some(turn_id.into()),
                    AgentStatus::Errored("ReviewEnded".into()),
                )],
            )
            .await;
        assert_eq!(
            session.rencrow_completion_state("root").await,
            Some(inconsistent.1),
            "a terminal failure fact cannot coexist with a Running legacy unit"
        );

        let mut pending_after = failed_legacy_review_state(&child.to_string(), turn_id);
        pending_after.units[0]
            .review_evidence
            .as_mut()
            .unwrap()
            .reviewer_turn_id = format!("pending-after:{turn_id}");
        let pending_after = ("root".into(), pending_after);
        session.state.lock().await.rencrow_completion_state = Some(pending_after.clone());
        session
            .collect_completion_review_results(
                "legacy-review-pending-after",
                &[(
                    child,
                    Some("follow-up-turn".into()),
                    AgentStatus::Errored("follow-up failed".into()),
                )],
            )
            .await;
        assert_eq!(
            session.rencrow_completion_state("root").await,
            Some(pending_after.1),
            "pending-after does not bind a receipt to the previous spawn attempt"
        );

        for (wait_call_id, conflicting_status) in [
            (
                "legacy-review-wrong-error-reason",
                AgentStatus::Errored("different failure".into()),
            ),
            (
                "legacy-review-shutdown-after-review-ended",
                AgentStatus::Shutdown,
            ),
        ] {
            let before = failed_legacy_review_state(&child.to_string(), turn_id);
            session.state.lock().await.rencrow_completion_state =
                Some(("root".into(), before.clone()));
            session
                .collect_completion_review_results(
                    wait_call_id,
                    &[(child, Some(turn_id.into()), conflicting_status)],
                )
                .await;
            assert_eq!(
                session.rencrow_completion_state("root").await,
                Some(before),
                "a persisted ReviewEnded fingerprint only accepts its matching wait status"
            );
        }
    }

    #[tokio::test]
    async fn known_child_pending_record_is_idempotent_only_for_same_attempt_and_child() {
        let (mut session, _) = make_session_and_context().await;
        attach_thread_persistence(&mut session).await;
        let task_workspace = tempfile::tempdir().expect("task workspace");
        let artifact_path = std::path::PathBuf::from("review-target.txt");
        std::fs::write(
            task_workspace.path().join(&artifact_path),
            "accepted artifact\n",
        )
        .expect("write accepted artifact");
        let artifact_paths = vec![artifact_path];
        let artifact_before =
            crate::task_check::hash_workspace_artifacts(task_workspace.path(), &artifact_paths)
                .expect("hash accepted artifact");
        session
            .begin_task_check(
                "check-call".into(),
                crate::task_check::TaskEvidenceRequest {
                    task_id: "task-a".into(),
                    task_revision: 1,
                    artifact_paths,
                    check_kind: crate::task_check::TaskCheckKind::Verification,
                },
                task_workspace.path().to_path_buf(),
                artifact_before.clone(),
            )
            .await;
        let task_receipt = session
            .finish_task_check("check-call", 0, true, artifact_before)
            .await
            .expect("accepted task check receipt");
        let target_artifact_ref = format!(
            "task-check:check-call:{}",
            task_check_artifact_fingerprint(&task_receipt).expect("artifact fingerprint")
        );
        session
            .record_initial_history(codex_history::InitialHistory::Forked(vec![
                crate::session::tests::accepted_task_boundary_item(
                    &session.thread_id().to_string(),
                    &target_artifact_ref,
                ),
            ]))
            .await;
        let child =
            ThreadId::try_from("01a1c606-48b8-7cf3-bb78-77b47f6b8c2f").expect("valid child id");
        let other_child = ThreadId::try_from("01a1c606-48b8-7cf3-bb78-77b47f6b8c30")
            .expect("valid different child id");
        let target = CompletionReviewTarget {
            task_id: "task-a".into(),
            task_revision: 1,
        };
        let spawn_call_id = "spawn-review-attempt".to_string();
        let mut contract = pending_review_state(&child.to_string(), "ignored-turn");
        let contract_revision = contract.contract_revision;
        let conditions_sha256 =
            codex_history::task_context::completion_condition_definitions_sha256(
                &contract,
                &contract.units[0].condition_refs,
            )
            .expect("fixture review condition exists");
        let unit = &mut contract.units[0];
        unit.status = UnitStatus::Running;
        unit.review_evidence = None;
        unit.review_launch = None;
        unit.review_state = Some(CompletionReviewState {
            version: 1,
            obligation_unit_id: unit.id.clone(),
            active_attempt: Some(CompletionReviewAttempt {
                attempt_id: spawn_call_id.clone(),
                contract_revision,
                condition_refs: unit.condition_refs.clone(),
                conditions_sha256,
                status: CompletionReviewAttemptStatus::Reserved,
                launch_unknown_reason: None,
                consumed_retry_authorization: None,
                imported_from: None,
                reviewer_thread_id: None,
                reviewer_turn_id: None,
                reviewer_role: "analyst".into(),
                reviewer_model: Some("review-model".into()),
                target_task_id: target.task_id.clone(),
                target_task_revision: target.task_revision,
                target_artifact_ref: target_artifact_ref.clone(),
                deadline_unix_ms: Some(chrono::Utc::now().timestamp_millis() + 60_000),
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
        });
        session.state.lock().await.rencrow_completion_state = Some(("root".into(), contract));
        let identity = CompletionReviewChildIdentity {
            target: target.clone(),
            target_artifact_ref: target_artifact_ref.clone(),
            reviewer_thread_id: child,
            reviewer_role: "analyst".into(),
            spawn_call_id: spawn_call_id.clone(),
        };

        session
            .record_completion_review_child_pending("review-1", identity.clone())
            .await
            .expect("first known child binding is persisted");
        let after_first = session.rencrow_completion_state("root").await.unwrap();
        assert_eq!(
            after_first.units[0]
                .review_evidence
                .as_ref()
                .unwrap()
                .reviewer_turn_id,
            format!("pending-generation:{spawn_call_id}")
        );
        session
            .record_completion_review_child_pending("review-1", identity)
            .await
            .expect("handler-to-owner duplicate for the exact pending child is idempotent");
        assert_eq!(
            session.rencrow_completion_state("root").await.unwrap(),
            after_first
        );

        let conflicting = CompletionReviewChildIdentity {
            target,
            target_artifact_ref,
            reviewer_thread_id: other_child,
            reviewer_role: "analyst".into(),
            spawn_call_id: "different-spawn-attempt".into(),
        };
        assert!(
            session
                .record_completion_review_child_pending("review-1", conflicting)
                .await
                .is_err(),
            "a second actual child cannot replace the existing attempt binding"
        );
        let after_conflict = session.rencrow_completion_state("root").await.unwrap();
        assert_eq!(after_conflict.units[0], after_first.units[0]);
        assert!(session.has_uncollected_completion_children().await);
    }

    #[test]
    fn later_completed_generation_cannot_complete_bound_review() {
        let reviewer_id =
            ThreadId::try_from("01a1c606-48b8-7cf3-bb78-77b47f6b8c2f").expect("valid reviewer id");
        let review = CompletionReviewEvidence {
            reviewer_thread_id: reviewer_id.to_string(),
            reviewer_turn_id: "review-turn-original".into(),
            reviewer_role: "analyst".into(),
            spawn_call_id: "spawn-review".into(),
            deadline_unix_ms: None,
            deadline_escalation: None,
            deadline_safety_fingerprint: None,
            wait_result_ref: None,
            terminal_status: None,
            wait_call_id: None,
            target_task_id: "task-a".into(),
            target_task_revision: 1,
            target_artifact_ref: "artifact:sha".into(),
        };
        let unrelated_completion = AgentStatus::Completed(Some("unrelated follow-up".into()));

        assert!(!status_matches_review_generation(
            &review,
            reviewer_id,
            Some("later-unrelated-turn"),
            &unrelated_completion,
        ));
        assert!(status_matches_review_generation(
            &review,
            reviewer_id,
            Some("review-turn-original"),
            &unrelated_completion,
        ));
    }

    #[tokio::test]
    async fn terminal_child_is_not_collected_until_native_result_is_returned() {
        let (session, _) = make_session_and_context().await;
        let child = "01a1c606-48b8-7cf3-bb78-77b47f6b8c2f";
        session
            .observe_completion_child_tool("spawn_agent", &json!({}), &json!({"agent_id":child}))
            .await;
        assert!(session.has_uncollected_completion_children().await);
        session
            .observe_completion_child_tool(
                "wait_agent",
                &json!({}),
                &json!({"status":{child:"running"},"timed_out":true}),
            )
            .await;
        assert!(session.has_uncollected_completion_children().await);
        session
            .observe_completion_child_tool(
                "wait_agent",
                &json!({}),
                &json!({"status":{child:{"completed":"result"}},"timed_out":false}),
            )
            .await;
        assert!(!session.has_uncollected_completion_children().await);
        session
            .observe_completion_child_tool(
                "send_input",
                &json!({"target":child}),
                &json!({"submission_id":"next"}),
            )
            .await;
        assert!(
            session.has_uncollected_completion_children().await,
            "new delegation needs a new result"
        );
    }

    #[tokio::test]
    async fn reviewer_followup_invalidates_old_result_without_releasing_child_reservation() {
        let (session, _) = make_session_and_context().await;
        let child = "01a1c606-48b8-7cf3-bb78-77b47f6b8c2f";
        session.state.lock().await.rencrow_completion_state =
            Some(("root".into(), pending_review_state(child, "review-turn-1")));

        session
            .observe_completion_child_tool(
                "send_input",
                &json!({"target":child}),
                &json!({"submission_id":"followup-1"}),
            )
            .await;

        let state = session
            .rencrow_completion_state("root")
            .await
            .expect("completion plan remains registered");
        let unit = &state.units[0];
        assert_eq!(unit.status, UnitStatus::Running);
        let evidence = unit
            .review_evidence
            .as_ref()
            .expect("child binding remains");
        assert_eq!(evidence.reviewer_thread_id, child);
        assert!(evidence.wait_result_ref.is_none());
        assert!(evidence.wait_call_id.is_none());
        assert_ne!(evidence.reviewer_turn_id, "review-turn-1");
        assert!(session.has_uncollected_completion_children().await);
    }

    #[tokio::test]
    async fn close_preserves_wait_collected_completion_review_result() {
        let (session, _) = make_session_and_context().await;
        let child_id = "01a1c77f-15cf-7491-9372-cea9ad0c408a";
        let before = pending_review_state(child_id, "review-turn-1");
        session.state.lock().await.rencrow_completion_state = Some(("root".into(), before.clone()));

        session
            .observe_completion_child_tool(
                "close_agent",
                &json!({"target": child_id}),
                &json!({
                    "previous_status": AgentStatus::Completed(Some("review result".into()))
                }),
            )
            .await;

        let after = session.rencrow_completion_state("root").await.unwrap();
        assert_eq!(
            after.units[0].review_evidence,
            before.units[0].review_evidence
        );
        assert_eq!(after.units[0].status, UnitStatus::Running);
    }

    #[tokio::test]
    async fn close_preserves_host_failed_review_generation() {
        let (session, _) = make_session_and_context().await;
        let child_id = "01a1c77f-15cf-7491-9372-cea9ad0c408a";
        let mut before = pending_review_state(child_id, "review-turn-1");
        before.units[0].status = UnitStatus::Failed;
        let evidence = before.units[0].review_evidence.as_mut().unwrap();
        evidence.deadline_unix_ms = Some(1);
        evidence.deadline_escalation = Some("reviewer turn ended with an error".into());
        evidence.deadline_safety_fingerprint = Some("host-terminal-error".into());
        evidence.wait_result_ref = None;
        evidence.terminal_status = None;
        evidence.wait_call_id = None;
        before
            .failed_approaches
            .push(codex_history::task_context::FailedApproach {
                summary: "review unit review-1 failed".into(),
                cause: "reviewer turn ended with an error".into(),
                next_action: "Add a new authorized review unit".into(),
                recorded_at: 1,
            });
        session.state.lock().await.rencrow_completion_state = Some(("root".into(), before.clone()));

        session
            .observe_completion_child_tool(
                "close_agent",
                &json!({"target": child_id}),
                &json!({"previous_status": {"errored": "ReviewEnded"}}),
            )
            .await;

        let after = session.rencrow_completion_state("root").await.unwrap();
        assert_eq!(after.units[0].status, UnitStatus::Failed);
        assert_eq!(
            after.units[0].review_evidence, before.units[0].review_evidence,
            "close must preserve the exact failed generation and must not fabricate a success receipt"
        );
    }

    #[tokio::test]
    async fn close_records_terminal_error_for_bound_review_without_wait_receipt() {
        let (session, _) = make_session_and_context().await;
        let child_id = "01a1c77f-15cf-7491-9372-cea9ad0c408a";
        let mut before = pending_review_state(child_id, "review-turn-1");
        let evidence = before.units[0].review_evidence.as_mut().unwrap();
        evidence.wait_result_ref = None;
        evidence.terminal_status = None;
        evidence.wait_call_id = None;
        session.state.lock().await.rencrow_completion_state = Some(("root".into(), before));

        session
            .observe_completion_child_tool(
                "close_agent",
                &json!({"target": child_id}),
                &json!({"previous_status": {"errored": "ReviewEnded"}}),
            )
            .await;

        let after = session.rencrow_completion_state("root").await.unwrap();
        let unit = &after.units[0];
        assert_eq!(unit.status, UnitStatus::Failed);
        let evidence = unit.review_evidence.as_ref().unwrap();
        assert_eq!(evidence.reviewer_turn_id, "review-turn-1");
        assert!(evidence.wait_result_ref.is_none());
        assert!(evidence.terminal_status.is_none());
        assert!(evidence.wait_call_id.is_none());
        assert!(after.failed_approaches.iter().any(|failure| {
            failure.summary == "review unit review-1 failed" && !failure.cause.trim().is_empty()
        }));
    }

    #[tokio::test]
    async fn close_uncollected_completion_keeps_shutdown_collection_path() {
        let (session, _) = make_session_and_context().await;
        let child =
            ThreadId::try_from("01a1c77f-15cf-7491-9372-cea9ad0c408a").expect("valid child id");
        let mut contract = pending_review_state(&child.to_string(), "review-turn-1");
        contract.units[0].status = UnitStatus::Running;
        let evidence = contract.units[0].review_evidence.as_mut().unwrap();
        evidence.wait_result_ref = None;
        evidence.terminal_status = None;
        evidence.wait_call_id = None;
        session.state.lock().await.rencrow_completion_state = Some(("root".into(), contract));

        session
            .observe_completion_child_tool(
                "close_agent",
                &json!({"target": child}),
                &json!({
                    "previous_status": AgentStatus::Completed(Some("review finished".into()))
                }),
            )
            .await;
        let closed = session.rencrow_completion_state("root").await.unwrap();
        assert_eq!(closed.units[0].status, UnitStatus::Running);
        assert_eq!(
            closed.units[0]
                .review_evidence
                .as_ref()
                .unwrap()
                .reviewer_turn_id,
            "pending-after:review-turn-1"
        );

        session
            .collect_completion_review_results(
                "wait-after-close",
                &[(child, None, AgentStatus::Shutdown)],
            )
            .await;
        let failed = session.rencrow_completion_state("root").await.unwrap();
        assert_eq!(failed.units[0].status, UnitStatus::Failed);
        let evidence = failed.units[0].review_evidence.as_ref().unwrap();
        assert!(evidence.wait_result_ref.is_none());
        assert!(evidence.terminal_status.is_none());
        assert!(evidence.wait_call_id.is_none());
    }

    #[tokio::test]
    async fn close_after_followup_keeps_old_review_result_invalidated() {
        let (session, _) = make_session_and_context().await;
        let child_id = "01a1c77f-15cf-7491-9372-cea9ad0c408a";
        session.state.lock().await.rencrow_completion_state = Some((
            "root".into(),
            pending_review_state(child_id, "review-turn-1"),
        ));
        session
            .observe_completion_child_tool(
                "send_input",
                &json!({"target": child_id}),
                &json!({"submission_id":"followup-2"}),
            )
            .await;
        session
            .observe_completion_child_tool(
                "close_agent",
                &json!({"target": child_id}),
                &json!({
                    "previous_status": AgentStatus::Completed(Some("review result".into()))
                }),
            )
            .await;

        let state = session.rencrow_completion_state("root").await.unwrap();
        let evidence = state.units[0].review_evidence.as_ref().unwrap();
        assert_eq!(evidence.reviewer_turn_id, "pending-after:review-turn-1");
        assert!(evidence.wait_result_ref.is_none());
        assert!(evidence.terminal_status.is_none());
        assert!(evidence.wait_call_id.is_none());
    }

    #[tokio::test]
    async fn followup_error_generation_is_collected_and_close_needs_no_new_turn() {
        for terminal_status in [
            AgentStatus::Errored("follow-up failed".into()),
            AgentStatus::Shutdown,
        ] {
            let terminal_debug = format!("{terminal_status:?}");
            let (session, _) = make_session_and_context().await;
            let child =
                ThreadId::try_from("01a1c606-48b8-7cf3-bb78-77b47f6b8c2f").expect("valid child id");
            {
                let mut state = session.state.lock().await;
                let mut contract = pending_review_state(&child.to_string(), "pending-after:turn-1");
                contract.units[0]
                    .review_evidence
                    .as_mut()
                    .unwrap()
                    .wait_result_ref = None;
                contract.units[0]
                    .review_evidence
                    .as_mut()
                    .unwrap()
                    .terminal_status = None;
                contract.units[0]
                    .review_evidence
                    .as_mut()
                    .unwrap()
                    .wait_call_id = None;
                state.rencrow_completion_state = Some(("root".into(), contract));
            }
            let generation =
                (!matches!(&terminal_status, AgentStatus::Shutdown)).then(|| "turn-2".to_string());
            session
                .collect_completion_review_results(
                    "wait-followup-terminal",
                    &[(child, generation, terminal_status)],
                )
                .await;
            let state = session
                .rencrow_completion_state("root")
                .await
                .expect("registered review remains available");
            assert_eq!(
                state.units[0].status,
                UnitStatus::Failed,
                "terminal status={terminal_debug}, review evidence={:?}, failures={:?}",
                state.units[0].review_evidence,
                state.failed_approaches
            );
            assert!(
                state
                    .failed_approaches
                    .iter()
                    .any(|failure| { failure.summary.contains("review") })
            );
        }
    }

    #[tokio::test]
    async fn certain_unstarted_capacity_reservation_can_return_to_ready() {
        let (session, _) = make_session_and_context().await;
        let child = "01a1c606-48b8-7cf3-bb78-77b47f6b8c2f";
        {
            let mut state = session.state.lock().await;
            let mut contract = pending_review_state(child, "review-turn-1");
            contract.units[0].status = UnitStatus::Running;
            contract.units[0].review_evidence = None;
            state.rencrow_completion_state = Some(("root".into(), contract));
        }
        session
            .rollback_unstarted_completion_review("review-1")
            .await
            .expect("typed pre-spawn capacity rejection can release reservation");
        let state = session
            .rencrow_completion_state("root")
            .await
            .expect("registered review remains available");
        assert_eq!(state.units[0].status, UnitStatus::Ready);
        assert!(state.units[0].review_evidence.is_none());
    }

    #[test]
    fn unresolved_spawn_reuses_child_and_binds_later_generation() {
        let child =
            ThreadId::try_from("01a1c606-48b8-7cf3-bb78-77b47f6b8c2f").expect("valid child id");
        let mut contract =
            pending_review_state(&child.to_string(), "pending-generation:spawn-review");
        let unit = &mut contract.units[0];
        assert_eq!(unit.status, UnitStatus::Running);
        assert!(!completion_review_needs_spawn(unit));
        let evidence = unit
            .review_evidence
            .as_mut()
            .expect("persisted child binding");
        assert_eq!(evidence.reviewer_thread_id, child.to_string());
        assert_eq!(evidence.spawn_call_id, "spawn-review");
        assert!(bind_completion_review_generation(
            evidence,
            child,
            "spawn-review",
            "review-turn-2".into(),
        ));
        assert_eq!(evidence.reviewer_thread_id, child.to_string());
        assert_eq!(evidence.spawn_call_id, "spawn-review");
        assert_eq!(evidence.reviewer_turn_id, "review-turn-2");
        assert!(!bind_completion_review_generation(
            evidence,
            child,
            "different-spawn",
            "review-turn-3".into(),
        ));
        assert_eq!(evidence.reviewer_turn_id, "review-turn-2");

        // Legacy completion_review spawn calls this binder without the newer
        // pre-persist step. Its initial evidence must use the same sentinel.
        let mut direct = pending_completion_review_evidence(
            child,
            "analyst".into(),
            "legacy-spawn-call".into(),
            "task-a".into(),
            1,
            "task-check:target".into(),
            None,
        );
        assert!(bind_completion_review_generation(
            &mut direct,
            child,
            "legacy-spawn-call",
            "legacy-review-turn".into(),
        ));
        assert_eq!(direct.reviewer_thread_id, child.to_string());
        assert_eq!(direct.spawn_call_id, "legacy-spawn-call");
        assert_eq!(direct.reviewer_turn_id, "legacy-review-turn");
    }

    #[tokio::test]
    async fn failed_review_terminal_is_recorded_and_not_automatically_retried() {
        let (session, _) = make_session_and_context().await;
        let child =
            ThreadId::try_from("01a1c606-48b8-7cf3-bb78-77b47f6b8c2f").expect("valid child id");
        let mut contract = pending_review_state(&child.to_string(), "review-turn-1");
        contract.units[0]
            .review_evidence
            .as_mut()
            .unwrap()
            .wait_result_ref = None;
        contract.units[0]
            .review_evidence
            .as_mut()
            .unwrap()
            .terminal_status = None;
        contract.units[0]
            .review_evidence
            .as_mut()
            .unwrap()
            .wait_call_id = None;
        session.state.lock().await.rencrow_completion_state = Some(("root".into(), contract));

        session
            .collect_completion_review_results(
                "wait-failed",
                &[(
                    child,
                    Some("review-turn-1".into()),
                    AgentStatus::Errored("reviewer failed".into()),
                )],
            )
            .await;

        let state = session
            .rencrow_completion_state("root")
            .await
            .expect("completion plan remains registered");
        assert_eq!(state.units[0].status, UnitStatus::Failed);
        assert_eq!(state.failed_approaches.len(), 1);
        assert!(
            state.failed_approaches[0]
                .cause
                .contains("reviewer turn ended")
        );
    }

    #[tokio::test]
    async fn required_review_unit_keeps_satisfied_execution_plan_pending() {
        let (session, _) = make_session_and_context().await;
        let mut contract = pending_review_state("01a1c606-48b8-7cf3-bb78-77b47f6b8c2f", "turn-1");
        contract.conditions[0].status = AcceptanceStatus::Satisfied;
        contract.conditions[0].evidence =
            vec![codex_history::task_context::CompletionEvidenceRef {
                task_id: "task-a".into(),
                task_revision: 1,
                artifact_ref: "task-check:accepted".into(),
                verification_ref: "task-check:accepted".into(),
            }];
        contract.units[0].status = UnitStatus::Planned;
        contract.units[0].review_evidence = None;
        contract.units[0].depends_on = vec!["task-a".into()];
        contract.units.insert(
            0,
            CompletionUnit {
                id: "task-a".into(),
                purpose: "Execute task A".into(),
                status: UnitStatus::Done,
                kind: CompletionUnitKind::Execution,
                condition_refs: vec!["AC-1".into()],
                depends_on: vec![],
                evidence: vec![],
                task_refs: vec![],
                review_target: None,
                review_evidence: None,
                review_launch: None,
                review_decision_reason: None,
                review_state: None,
                review_revalidation_of: None,
            },
        );
        session.state.lock().await.rencrow_completion_state = Some(("root".into(), contract));

        assert!(
            session.autonomous_completion_pending().await,
            "a required review remains unfinished after execution and AC satisfaction"
        );
    }

    #[tokio::test]
    async fn review_spawn_rejects_unchecked_targets_and_preserves_adopted_review_history() {
        let (session, _) = make_session_and_context().await;
        session.state.lock().await.rencrow_completion_state = Some((
            "root".into(),
            CompletionState {
                contract_revision: 1,
                units_completed_since_review: 0,
                plan_review_at: None,
                plan_reviews: vec![],
                paused: false,
                consumed_host_events: vec![],
                conditions: vec![AcceptanceCondition {
                    id: "AC-1".into(),
                    revision: 1,
                    result: "The requested behavior works".into(),
                    scope: "CLI".into(),
                    verification: "Run the requested check".into(),
                    acceptance_criteria: "Expected behavior is observable".into(),
                    status: AcceptanceStatus::Open,
                    evidence: vec![],
                    unmet_reason: None,
                }],
                units: vec![CompletionUnit {
                    id: "review-1".into(),
                    purpose: "Review task a".into(),
                    status: UnitStatus::Ready,
                    kind: CompletionUnitKind::Review,
                    condition_refs: vec!["AC-1".into()],
                    depends_on: vec![],
                    evidence: vec![],
                    task_refs: vec![],
                    review_target: Some(codex_history::task_context::CompletionReviewTarget {
                        task_id: "task-a".into(),
                        task_revision: 1,
                    }),
                    review_evidence: None,
                    review_launch: None,
                    review_decision_reason: None,
                    review_state: None,
                    review_revalidation_of: None,
                }],
                current_unit: Some("review-1".into()),
                failed_approaches: vec![],
                blocker: None,
                next_action: Some("Collect a host-verified task before review".into()),
                product_progress_at: None,
                runtime_recovery_at: None,
                activity_at: None,
            },
        ));

        let error = session
            .validate_completion_review_spawn("review-1", "task-a", 1)
            .await
            .expect_err("model target without an accepted host check must be refused");
        assert!(error.contains("accepted task receipt"));
        assert!(
            session
                .rencrow_completion_state("root")
                .await
                .is_some_and(|state| state.units[0].review_evidence.is_none())
        );

        let reviewer_id =
            ThreadId::try_from("01a1c606-48b8-7cf3-bb78-77b47f6b8c2f").expect("valid reviewer id");
        session
            .state
            .lock()
            .await
            .rencrow_completion_state
            .as_mut()
            .unwrap()
            .1
            .units[0]
            .review_evidence = Some(CompletionReviewEvidence {
            reviewer_thread_id: reviewer_id.to_string(),
            reviewer_turn_id: "reviewer-turn-1".into(),
            reviewer_role: "analyst".into(),
            spawn_call_id: "spawn-review".into(),
            deadline_unix_ms: None,
            deadline_escalation: None,
            deadline_safety_fingerprint: None,
            wait_result_ref: None,
            terminal_status: None,
            wait_call_id: None,
            target_task_id: "task-a".into(),
            target_task_revision: 1,
            target_artifact_ref: "task-check:fake".into(),
        });
        session
            .collect_completion_review_results(
                "wait-review",
                &[(
                    reviewer_id,
                    Some("reviewer-turn-1".into()),
                    AgentStatus::Completed(Some("review finished".into())),
                )],
            )
            .await;
        assert!(
            session
                .rencrow_completion_state("root")
                .await
                .is_some_and(|state| state.units[0]
                    .review_evidence
                    .as_ref()
                    .is_some_and(|evidence| evidence.wait_result_ref.is_none()))
        );

        let adopted_evidence = CompletionReviewEvidence {
            reviewer_thread_id: reviewer_id.to_string(),
            reviewer_turn_id: "reviewer-turn-1".into(),
            reviewer_role: "analyst".into(),
            spawn_call_id: "spawn-adopted-review".into(),
            deadline_unix_ms: None,
            deadline_escalation: None,
            deadline_safety_fingerprint: None,
            wait_result_ref: Some("sha256:historical-result".into()),
            terminal_status: Some(CompletionReviewTerminalStatus::Completed),
            wait_call_id: Some("wait-adopted-review".into()),
            target_task_id: "task-a".into(),
            target_task_revision: 1,
            target_artifact_ref: "task-check:historical".into(),
        };
        {
            let mut state = session.state.lock().await;
            let contract = &mut state.rencrow_completion_state.as_mut().unwrap().1;
            contract.units[0].status = UnitStatus::Done;
            contract.units[0].review_decision_reason =
                Some("Adopted the historical review for task revision 1".into());
            contract.units[0].review_evidence = Some(adopted_evidence.clone());
        }
        session.revalidate_collected_completion_reviews().await;
        assert!(
            session
                .rencrow_completion_state("root")
                .await
                .is_some_and(|state| state.units[0].status == UnitStatus::Done
                    && state.units[0].review_decision_reason.is_some()
                    && state.units[0].review_evidence.as_ref() == Some(&adopted_evidence))
        );
    }
}
