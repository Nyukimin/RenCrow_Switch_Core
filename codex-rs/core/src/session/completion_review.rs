// Modified by RenCrow Switch Core, 2026-11-16.
//! Shared selection and host-owned types for autonomous review obligations.

use super::session::Session;
use codex_history::task_context::CompletionReviewAttemptStatus;
use codex_history::task_context::CompletionReviewRetryAuthorization;
use codex_history::task_context::CompletionReviewState;
use codex_history::task_context::CompletionState;
use codex_history::task_context::CompletionUnit;
use codex_history::task_context::CompletionUnitKind;
use codex_history::task_context::UnitStatus;

pub(super) fn projected_completion_review_units(
    contract: &CompletionState,
) -> Result<Vec<CompletionUnit>, String> {
    contract
        .units
        .iter()
        .filter(|unit| unit.kind == CompletionUnitKind::Review)
        .map(codex_history::task_context::project_completion_review_state)
        .collect()
}

pub(super) fn completion_unit_obligation_is_satisfied(
    contract: &codex_history::task_context::CompletionState,
    unit: &codex_history::task_context::CompletionUnit,
    root_task_id: &str,
    receipt: Option<&crate::state::CompletionSnapshotReceipt>,
    accepted_tasks: &std::collections::BTreeMap<
        String,
        codex_history::task_context::AcceptedTaskReceipt,
    >,
    current_artifact_fingerprints: &std::collections::HashMap<String, String>,
) -> bool {
    use codex_history::task_context::CompletionReviewTerminalStatus;
    use codex_history::task_context::CompletionUnitKind;
    use codex_history::task_context::UnitStatus;

    match unit.kind {
        CompletionUnitKind::Execution => {
            matches!(unit.status, UnitStatus::Done | UnitStatus::Cancelled)
        }
        CompletionUnitKind::Review => match unit.status {
            UnitStatus::Done => {
                let is_adopted_review =
                    |candidate: &codex_history::task_context::CompletionUnit,
                     task_id: &str,
                     task_revision: u64,
                     artifact_fingerprint: &str| {
                        let current_condition_digest =
                            codex_history::task_context::completion_condition_definitions_sha256(
                                contract,
                                &candidate.condition_refs,
                            )
                            .ok();
                        candidate.status == UnitStatus::Done
                            && candidate.review_target.as_ref().is_some_and(|target| {
                                target.task_id == task_id && target.task_revision == task_revision
                            })
                            && candidate.review_evidence.as_ref().is_some_and(|evidence| {
                                evidence.target_task_id == task_id
                                    && evidence.target_task_revision == task_revision
                                    && codex_history::task_context::task_check_artifact_fingerprint(
                                        &evidence.target_artifact_ref,
                                    ) == Some(artifact_fingerprint)
                                    && evidence.terminal_status
                                        == Some(CompletionReviewTerminalStatus::Completed)
                                    && evidence
                                        .wait_result_ref
                                        .as_deref()
                                        .is_some_and(|value| !value.trim().is_empty())
                                    && evidence
                                        .wait_call_id
                                        .as_deref()
                                        .is_some_and(|value| !value.trim().is_empty())
                            })
                            && candidate
                                .review_decision_reason
                                .as_deref()
                                .is_some_and(|value| !value.trim().is_empty())
                            && candidate.review_state.as_ref().map_or_else(
                                || {
                                    let source_condition_digest = contract
                                        .units
                                        .iter()
                                        .filter(|child| {
                                            child.review_revalidation_of.as_deref()
                                                == Some(candidate.id.as_str())
                                        })
                                        .find_map(|child| {
                                            child
                                                .review_state
                                                .as_ref()
                                                .and_then(|review| review.revalidation.as_ref())
                                                .map(|source| {
                                                    source
                                                        .source_condition_definitions_sha256
                                                        .as_str()
                                                })
                                        });
                                    source_condition_digest.is_none_or(|source_digest| {
                                        current_condition_digest.as_deref() == Some(source_digest)
                                    })
                                },
                                |review| {
                                    review.active_attempt.as_ref().is_some_and(|attempt| {
                                        current_condition_digest.as_deref()
                                            == Some(attempt.conditions_sha256.as_str())
                                    })
                                },
                            )
                    };
                let current_target_is_adopted =
                    |candidate: &codex_history::task_context::CompletionUnit| {
                        let Some(target) = candidate.review_target.as_ref() else {
                            return false;
                        };
                        let Some(accepted) = accepted_tasks.get(&target.task_id) else {
                            return false;
                        };
                        let Some(fingerprint) = current_artifact_fingerprints.get(&target.task_id)
                        else {
                            return false;
                        };
                        accepted.revision == target.task_revision
                            && is_adopted_review(
                                candidate,
                                &target.task_id,
                                target.task_revision,
                                fingerprint,
                            )
                    };
                let has_revalidation_successor =
                    |candidate: &codex_history::task_context::CompletionUnit| {
                        contract.units.iter().any(|next| {
                            next.review_revalidation_of.as_deref() == Some(candidate.id.as_str())
                                && next.status != UnitStatus::Cancelled
                        })
                    };
                if !has_revalidation_successor(unit) && current_target_is_adopted(unit) {
                    return true;
                }
                let mut ancestor = unit;
                let mut visited = std::collections::HashSet::new();
                visited.insert(unit.id.as_str());
                loop {
                    let mut leaves = contract.units.iter().filter(|candidate| {
                        candidate.review_revalidation_of.as_deref() == Some(ancestor.id.as_str())
                            && candidate.status != UnitStatus::Cancelled
                    });
                    let Some(leaf) = leaves.next() else {
                        return false;
                    };
                    if leaves.next().is_some() || leaf.condition_refs != unit.condition_refs {
                        return false;
                    }
                    let Some(source) = leaf
                        .review_state
                        .as_ref()
                        .and_then(|review| review.revalidation.as_ref())
                    else {
                        return false;
                    };
                    let source_matches = source.source_unit_id == ancestor.id
                        && codex_history::task_context::completion_review_adoption_sha256(ancestor)
                            .ok()
                            .as_deref()
                            == Some(source.source_adoption_sha256.as_str())
                        && !source.update_call_id.trim().is_empty();
                    if !source_matches || !visited.insert(leaf.id.as_str()) {
                        return false;
                    }
                    if !has_revalidation_successor(leaf) && current_target_is_adopted(leaf) {
                        return true;
                    }
                    if leaf.status != UnitStatus::Done
                        || codex_history::task_context::completion_review_adoption_sha256(leaf)
                            .is_err()
                    {
                        return false;
                    }
                    ancestor = leaf;
                }
            }
            UnitStatus::Cancelled => {
                has_authorized_review_withdrawal(contract, &unit.id)
                    || (receipt.is_some_and(|receipt| {
                        receipt.version == 1
                            && receipt.root_task_id == root_task_id
                            && receipt
                                .legacy_review_bindings
                                .get(&unit.id)
                                .is_some_and(|binding| {
                                    binding.contract_revision == contract.contract_revision
                                        && binding.status == UnitStatus::Cancelled
                                        && binding.review_target == unit.review_target
                                        && binding.condition_refs == unit.condition_refs
                                        && binding.depends_on == unit.depends_on
                                        && binding.evidence.is_none()
                                })
                    }) && unit.review_state.is_none()
                        && unit.review_evidence.is_none()
                        && unit.review_launch.is_none()
                        && unit.review_decision_reason.is_none())
            }
            _ => false,
        },
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CompletionReviewRetryRequest {
    pub(crate) unit_id: String,
    pub(crate) expected_attempt_id: String,
    pub(crate) invocation_call_id: String,
    pub(crate) previous_plan: String,
    pub(crate) revised_plan: String,
    pub(crate) reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CompletionReviewSelection {
    pub(crate) obligation_unit_id: String,
    pub(crate) outcome: CompletionReviewSelectionOutcome,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CompletionReviewSelectionOutcome {
    AlreadyAdopted,
    ExistingAttempt {
        attempt_id: Option<String>,
    },
    NeedsReservation {
        expected_attempt_id: Option<String>,
        retry_authorization_id: Option<String>,
    },
}

pub(crate) fn project_completion_state(
    contract: &CompletionState,
) -> Result<CompletionState, String> {
    let mut projected = contract.clone();
    for unit in &mut projected.units {
        if unit.review_state.is_some() {
            *unit = codex_history::task_context::project_completion_review_state(unit)?;
        }
    }
    Ok(projected)
}

pub(crate) fn has_authorized_review_withdrawal(contract: &CompletionState, unit_id: &str) -> bool {
    let Some(unit) = contract.units.iter().find(|unit| unit.id == unit_id) else {
        return false;
    };
    let Some(withdrawal) = unit
        .review_state
        .as_ref()
        .and_then(|review| review.withdrawal.as_ref())
    else {
        return false;
    };
    let source_valid = match &withdrawal.source {
        codex_history::task_context::CompletionReviewWithdrawalSource::HostUpdate {
            update_call_id,
            prior_status,
        } => *prior_status == UnitStatus::Planned && !update_call_id.trim().is_empty(),
        codex_history::task_context::CompletionReviewWithdrawalSource::LegacyV1Snapshot {
            snapshot,
        } => snapshot.version == 1 && snapshot.sha256.len() == 64,
    };
    source_valid && unit.status == UnitStatus::Cancelled
}

pub(crate) fn completion_review_conditions_sha256(
    contract: &CompletionState,
    condition_refs: &[String],
) -> Result<String, String> {
    codex_history::task_context::completion_condition_definitions_sha256(contract, condition_refs)
}

pub(crate) fn select_completion_review(
    contract: &CompletionState,
    target_task_id: &str,
    target_task_revision: u64,
    unit_id: Option<&str>,
) -> Result<CompletionReviewSelection, String> {
    let projected_units = projected_completion_review_units(contract)?;
    let mut matches = projected_units.iter().filter(|unit| {
        unit.kind == CompletionUnitKind::Review
            && unit.review_target.as_ref().is_some_and(|target| {
                target.task_id == target_task_id && target.task_revision == target_task_revision
            })
            && unit_id.is_none_or(|requested| unit.id == requested)
    });
    let unit = matches
        .next()
        .ok_or_else(|| format!("no planned review unit matches task {target_task_id}"))?;
    if matches.next().is_some() {
        return Err("multiple review obligations match; provide an exact unit_id".into());
    }
    if unit.status == UnitStatus::Done {
        return Ok(CompletionReviewSelection {
            obligation_unit_id: unit.id.clone(),
            outcome: CompletionReviewSelectionOutcome::AlreadyAdopted,
        });
    }
    let state: Option<&CompletionReviewState> = unit.review_state.as_ref();
    match state.and_then(|state| state.active_attempt.as_ref()) {
        Some(attempt) if attempt.status == CompletionReviewAttemptStatus::Failed => {
            let grant = state.and_then(|state| state.retry_authorization.as_ref());
            if unit_id.is_none_or(|requested| requested != unit.id) {
                return Err("retry requires an explicit obligation unit_id".into());
            }
            let grant = grant
                .filter(|grant| grant.expected_attempt_id == attempt.attempt_id)
                .ok_or_else(|| {
                    "failed review requires a matching explicit plan-review retry grant".to_string()
                })?;
            Ok(CompletionReviewSelection {
                obligation_unit_id: unit.id.clone(),
                outcome: CompletionReviewSelectionOutcome::NeedsReservation {
                    expected_attempt_id: Some(attempt.attempt_id.clone()),
                    retry_authorization_id: Some(grant.authorization_id.clone()),
                },
            })
        }
        Some(attempt) => Ok(CompletionReviewSelection {
            obligation_unit_id: unit.id.clone(),
            outcome: CompletionReviewSelectionOutcome::ExistingAttempt {
                attempt_id: Some(attempt.attempt_id.clone()),
            },
        }),
        None if unit.review_evidence.is_some() || unit.review_launch.is_some() => {
            Ok(CompletionReviewSelection {
                obligation_unit_id: unit.id.clone(),
                outcome: CompletionReviewSelectionOutcome::ExistingAttempt {
                    attempt_id: unit
                        .review_evidence
                        .as_ref()
                        .map(|evidence| evidence.spawn_call_id.clone())
                        .or_else(|| {
                            unit.review_launch
                                .as_ref()
                                .map(|launch| launch.spawn_call_id.clone())
                        }),
                },
            })
        }
        None if matches!(unit.status, UnitStatus::Planned | UnitStatus::Ready) => {
            Ok(CompletionReviewSelection {
                obligation_unit_id: unit.id.clone(),
                outcome: CompletionReviewSelectionOutcome::NeedsReservation {
                    expected_attempt_id: None,
                    retry_authorization_id: None,
                },
            })
        }
        None => Err(format!(
            "review obligation {} has no canonical attempt and is not launchable ({:?})",
            unit.id, unit.status
        )),
    }
}

impl Session {
    pub(crate) async fn completion_review_owner_guard(&self) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = self
            .state
            .lock()
            .await
            .rencrow_completion_review_lock
            .clone();
        lock.lock_owned().await
    }

    pub(crate) async fn select_completion_review(
        &self,
        target_task_id: &str,
        unit_id: Option<&str>,
    ) -> Result<CompletionReviewSelection, String> {
        let (root, contract) = self
            .autonomous_completion_current_state()
            .await
            .ok_or_else(|| "completion review requires a registered completion plan".to_string())?;
        if let Some(unit_id) = unit_id {
            let unit = contract
                .units
                .iter()
                .find(|unit| unit.id == unit_id && unit.kind == CompletionUnitKind::Review)
                .ok_or_else(|| "explicit review obligation does not exist".to_string())?;
            let target = unit
                .review_target
                .as_ref()
                .ok_or_else(|| "explicit review obligation has no target".to_string())?;
            if target.task_id != target_task_id {
                return Err("explicit review obligation targets a different task".into());
            }
            if unit.status == UnitStatus::Done {
                return Ok(CompletionReviewSelection {
                    obligation_unit_id: unit.id.clone(),
                    outcome: CompletionReviewSelectionOutcome::AlreadyAdopted,
                });
            }
        }
        let task_state = self
            .latest_rencrow_task_state()
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "review target has no accepted task receipt".to_string())?;
        if task_state.boundary.root_task_id != root {
            return Err("review target receipt belongs to a different completion root".into());
        }
        let target = task_state
            .completed
            .get(target_task_id)
            .ok_or_else(|| "review target has no accepted task receipt".to_string())?;
        select_completion_review(&contract, target_task_id, target.revision, unit_id)
    }
}

pub(crate) fn retry_authorization(
    request: &CompletionReviewRetryRequest,
) -> CompletionReviewRetryAuthorization {
    CompletionReviewRetryAuthorization {
        authorization_id: request.invocation_call_id.clone(),
        expected_attempt_id: request.expected_attempt_id.clone(),
        plan_review_call_id: request.invocation_call_id.clone(),
        reason: request.reason.clone(),
        previous_plan: request.previous_plan.clone(),
        revised_plan: request.revised_plan.clone(),
    }
}

#[cfg(test)]
#[path = "completion_review_tests.rs"]
mod tests;
