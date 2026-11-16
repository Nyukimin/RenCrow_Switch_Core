//! Durable host snapshots reuse the rollout envelope, outside model-written content.
// Modified by RenCrow Switch Core, 2026-11-16.

use super::session::Session;
use crate::context::AutonomousCompletionSnapshot;
use crate::context::ContextualUserFragment;
use crate::task_check::TaskCheckLedger;
use crate::task_check::TaskCheckSnapshot;
use codex_history::CodexHarnessMetadata;
use codex_history::ResponseItemEnvelope;
use codex_history::RolloutItem;
use codex_history::task_context::CompletionState;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TurnAbortReason;
use codex_utils_output_truncation::TruncationPolicy;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashSet;

const MAX_COMPLETION_SNAPSHOT_BYTES: usize = 256 * 1024;
const MAX_COMPLETION_TASK_CHECKS: usize = 256;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MaintenanceRepairEvidenceDigest {
    pub kind: String,
    pub path: String,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MaintenanceRepairReceipt {
    pub version: u32,
    pub thread_id: String,
    pub root_task_id: String,
    pub request_sha256: String,
    pub snapshot_before_sha256: String,
    pub shutdown_before_snapshot_sha256: String,
    pub shutdown_paused_snapshot_sha256: String,
    pub legacy_scope_mismatch_snapshot_sha256: Vec<String>,
    pub evidence: Vec<MaintenanceRepairEvidenceDigest>,
    pub resume_generation_before: u64,
    pub resume_generation_after: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MaintenanceRepairSnapshotInfo {
    pub value: Value,
    pub snapshot_sha256: String,
    pub root_task_id: String,
    pub resume_generation: u64,
    pub state: CompletionState,
    pub active_scope: Option<Value>,
    pub uncollected_children: Vec<String>,
}

fn validate_snapshot_sequence_transition(
    previous: Option<(&str, u64, u64, bool)>,
    next: &Snapshot,
) -> Result<(), &'static str> {
    let Some((previous_root, previous_revision, previous_generation, previous_paused)) = previous
    else {
        return Ok(());
    };
    if previous_root != next.root_task_id
        || next.state.contract_revision < previous_revision
        || next.resume_generation < previous_generation
        || (next.resume_generation == previous_generation && previous_paused && !next.state.paused)
    {
        return Err("completion snapshot sequence violates restore monotonicity");
    }
    Ok(())
}

fn collect_task_check_call_id(reference: &str, calls: &mut HashSet<String>) {
    if let Some(call_id) = reference
        .strip_prefix("task-check:")
        .and_then(|value| value.rsplit_once(':').map(|(call_id, _)| call_id))
        .filter(|call_id| !call_id.is_empty())
    {
        calls.insert(call_id.to_owned());
    }
}

fn completion_task_check_references(state: &CompletionState) -> HashSet<String> {
    let mut calls = HashSet::new();
    let evidence = state
        .conditions
        .iter()
        .flat_map(|condition| condition.evidence.iter())
        .chain(
            state
                .units
                .iter()
                .flat_map(|unit| unit.evidence.iter().chain(unit.task_refs.iter())),
        )
        .chain(
            state
                .blocker
                .iter()
                .flat_map(|blocker| blocker.evidence.iter()),
        );
    for reference in evidence {
        collect_task_check_call_id(&reference.artifact_ref, &mut calls);
        collect_task_check_call_id(&reference.verification_ref, &mut calls);
    }
    for reference in state
        .units
        .iter()
        .filter_map(|unit| unit.review_evidence.as_ref())
    {
        collect_task_check_call_id(&reference.target_artifact_ref, &mut calls);
    }
    calls
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn validate_repair_receipt(
    receipt: &MaintenanceRepairReceipt,
    snapshot: &Snapshot,
    expected_thread_id: &str,
) -> Result<(), String> {
    let allowed_kinds = [
        "deployment_result",
        "deployment_preflight",
        "maintenance_ticket",
        "pre_deploy_snapshot",
        "pre_deploy_facts",
        "deployer_source",
    ];
    let unique_hashes = receipt
        .legacy_scope_mismatch_snapshot_sha256
        .iter()
        .collect::<BTreeSet<_>>();
    let unique_kinds = receipt
        .evidence
        .iter()
        .map(|evidence| evidence.kind.as_str())
        .collect::<BTreeSet<_>>();
    if receipt.version != 1
        || receipt.thread_id != expected_thread_id
        || receipt.thread_id != snapshot.thread_id
        || receipt.root_task_id != snapshot.root_task_id
        || receipt.request_sha256.is_empty()
        || !is_sha256(&receipt.request_sha256)
        || !is_sha256(&receipt.snapshot_before_sha256)
        || !is_sha256(&receipt.shutdown_before_snapshot_sha256)
        || !is_sha256(&receipt.shutdown_paused_snapshot_sha256)
        || receipt.legacy_scope_mismatch_snapshot_sha256.is_empty()
        || receipt.legacy_scope_mismatch_snapshot_sha256.len() > 32
        || unique_hashes.len() != receipt.legacy_scope_mismatch_snapshot_sha256.len()
        || receipt
            .legacy_scope_mismatch_snapshot_sha256
            .iter()
            .any(|hash| !is_sha256(hash))
        || receipt.evidence.len() != allowed_kinds.len()
        || unique_kinds.len() != allowed_kinds.len()
        || receipt.evidence.iter().any(|evidence| {
            !allowed_kinds.contains(&evidence.kind.as_str())
                || evidence.path.trim().is_empty()
                || evidence.sha256.is_empty()
                || !is_sha256(&evidence.sha256)
        })
        || receipt.resume_generation_before.checked_add(1) != Some(receipt.resume_generation_after)
        || receipt.resume_generation_after != snapshot.resume_generation
        || snapshot.state.paused
    {
        return Err("maintenance repair receipt is invalid".into());
    }
    Ok(())
}

fn validate_snapshot_payload(
    snapshot: &Snapshot,
    expected_thread_id: &str,
    receipts: &BTreeMap<String, codex_history::task_context::AcceptedTaskReceipt>,
) -> Result<(), String> {
    let has_review_state = snapshot
        .state
        .units
        .iter()
        .any(|unit| unit.review_state.is_some());
    if !matches!(snapshot.version, 1 | 2)
        || (snapshot.version == 1 && has_review_state)
        || (snapshot.version == 2 && !has_review_state)
        || !completion_review_event_parents_match_snapshot(snapshot)
        || snapshot.thread_id != expected_thread_id
        || snapshot.root_task_id.trim().is_empty()
        || snapshot.root_task_id.len() > 4096
        || snapshot.state.contract_revision == 0
        || snapshot.task_checks.len() > MAX_COMPLETION_TASK_CHECKS
        || snapshot.uncollected_children.len() > 256
        || snapshot
            .uncollected_children
            .iter()
            .any(|id| codex_protocol::ThreadId::try_from(id.as_str()).is_err())
        || !serde_json::to_vec(snapshot)
            .is_ok_and(|bytes| bytes.len() <= MAX_COMPLETION_SNAPSHOT_BYTES)
        || snapshot.active_scope.as_ref().is_some_and(|scope| {
            scope.task_id.trim().is_empty()
                || scope.task_revision == 0
                || scope.begin_call_id.trim().is_empty()
        })
        || snapshot.patch_attempts.len() > 256
        || snapshot.patch_attempts.iter().any(|(call, attempt)| {
            call.trim().is_empty()
                || !snapshot
                    .active_scope
                    .as_ref()
                    .is_some_and(|scope| scope.same_execution(&attempt.scope))
                || attempt.paths.len() > 16
        })
        || codex_history::task_context::validate_completion_state_snapshot(
            Some(&snapshot.state),
            receipts,
        )
        .is_err()
    {
        return Err("completion snapshot payload is invalid".into());
    }
    let mut ledger = TaskCheckLedger::default();
    ledger.restore_snapshot(snapshot.task_checks.clone())?;
    let referenced_task_checks = completion_task_check_references(&snapshot.state);
    let retained_call_ids = snapshot
        .task_checks
        .iter()
        .map(|entry| entry.attempt.call_id.as_str())
        .collect::<HashSet<_>>();
    if referenced_task_checks
        .iter()
        .any(|call_id| !retained_call_ids.contains(call_id.as_str()))
    {
        return Err("completion snapshot references a missing task-check receipt".into());
    }
    Ok(())
}

fn completion_review_event_parents_match_snapshot(snapshot: &Snapshot) -> bool {
    let expected_parent = snapshot.thread_id.as_str();
    snapshot.state.units.iter().all(|unit| {
        let Some(review) = unit.review_state.as_ref() else {
            return true;
        };
        let active_event = review
            .active_attempt
            .as_ref()
            .and_then(|attempt| attempt.terminal_source.as_ref())
            .and_then(|source| source.event.as_ref());
        let historical_event = review
            .historical_failure
            .as_ref()
            .map(|failure| &failure.terminal_source)
            .and_then(|source| source.event.as_ref());
        let rollback_event = review
            .retry_reservation_prior_historical_failure
            .as_ref()
            .and_then(|failure| failure.terminal_source.event.as_ref());
        [active_event, historical_event, rollback_event]
            .into_iter()
            .flatten()
            .all(|event| event.owner_parent_thread_id.as_deref() == Some(expected_parent))
    })
}

fn revalidation_authorization_snapshot_resolves(
    snapshot: &Snapshot,
    prior_host_snapshots: &BTreeMap<String, Value>,
    unit: &codex_history::task_context::CompletionUnit,
    revalidation: &codex_history::task_context::CompletionReviewRevalidationSource,
) -> bool {
    prior_host_snapshots.iter().any(|(snapshot_sha256, value)| {
        if codex_history::compaction_candidate::digest(value)
            .ok()
            .as_deref()
            != Some(snapshot_sha256.as_str())
        {
            return false;
        }
        let Ok(authorized) = serde_json::from_value::<Snapshot>(value.clone()) else {
            return false;
        };
        if !matches!(authorized.version, 1 | 2)
            || authorized.thread_id != snapshot.thread_id
            || authorized.root_task_id != snapshot.root_task_id
            || !completion_review_event_parents_match_snapshot(&authorized)
        {
            return false;
        }
        let Some(authorized_unit) = authorized
            .state
            .units
            .iter()
            .find(|candidate| candidate.id == unit.id)
        else {
            return false;
        };
        let Some(authorized_review) = authorized_unit.review_state.as_ref() else {
            return false;
        };
        let Some(authorized_revalidation) = authorized_review.revalidation.as_ref() else {
            return false;
        };
        let Some(source_unit) = authorized
            .state
            .units
            .iter()
            .find(|candidate| candidate.id == revalidation.source_unit_id)
        else {
            return false;
        };
        authorized_revalidation == revalidation
            && authorized_unit.review_revalidation_of.as_deref()
                == Some(revalidation.source_unit_id.as_str())
            && authorized_unit.review_target == unit.review_target
            && authorized_unit.depends_on == unit.depends_on
            && authorized_unit.condition_refs == unit.condition_refs
            && codex_history::task_context::completion_condition_definitions_sha256(
                &authorized.state,
                &authorized_unit.condition_refs,
            )
            .is_ok_and(|digest| digest == revalidation.required_condition_definitions_sha256)
            && source_unit.status == codex_history::task_context::UnitStatus::Done
            && codex_history::task_context::completion_review_adoption_sha256(source_unit)
                .is_ok_and(|digest| digest == revalidation.source_adoption_sha256)
            && codex_history::task_context::completion_review_revalidation_dependencies_match(
                &authorized.state,
                &authorized.state,
                source_unit,
                authorized_unit,
                &revalidation.required_artifact_fingerprint,
            )
    })
}

fn revalidation_source_snapshot_resolves(
    snapshot: &Snapshot,
    current_sha256: &str,
    prior_host_snapshots: &BTreeMap<String, Value>,
    unit: &codex_history::task_context::CompletionUnit,
    revalidation: &codex_history::task_context::CompletionReviewRevalidationSource,
) -> bool {
    if revalidation.source_snapshot.sha256 == current_sha256
        || revalidation.source_snapshot.sha256.len() != 64
        || revalidation.source_snapshot.version != 1 && revalidation.source_snapshot.version != 2
        || revalidation.update_call_id.trim().is_empty()
        || unit.review_revalidation_of.as_deref() != Some(revalidation.source_unit_id.as_str())
    {
        return false;
    }
    let Some(value) = prior_host_snapshots.get(&revalidation.source_snapshot.sha256) else {
        return false;
    };
    if maintenance_snapshot_digest(value).ok().as_deref()
        != Some(revalidation.source_snapshot.sha256.as_str())
    {
        return false;
    }
    let Ok(previous) = serde_json::from_value::<Snapshot>(value.clone()) else {
        return false;
    };
    if previous.version != revalidation.source_snapshot.version
        || previous.thread_id != snapshot.thread_id
        || previous.root_task_id != snapshot.root_task_id
        || !completion_review_event_parents_match_snapshot(&previous)
        || !revalidation_authorization_snapshot_resolves(
            snapshot,
            prior_host_snapshots,
            unit,
            revalidation,
        )
    {
        return false;
    }
    previous.state.units.iter().any(|source_unit| {
        let source_digest =
            codex_history::task_context::completion_review_adoption_sha256(source_unit);
        let source_conditions =
            codex_history::task_context::completion_condition_definitions_sha256(
                &previous.state,
                &source_unit.condition_refs,
            );
        let source_fingerprint = source_unit.review_evidence.as_ref().and_then(|evidence| {
            codex_history::task_context::task_check_artifact_fingerprint(
                &evidence.target_artifact_ref,
            )
        });
        source_unit.id == revalidation.source_unit_id
            && source_unit.kind == codex_history::task_context::CompletionUnitKind::Review
            && source_unit.status == codex_history::task_context::UnitStatus::Done
            && source_digest.ok().as_deref() == Some(revalidation.source_adoption_sha256.as_str())
            && source_conditions.ok().as_deref()
                == Some(revalidation.source_condition_definitions_sha256.as_str())
            && source_fingerprint.is_some_and(|fingerprint| {
                fingerprint != revalidation.required_artifact_fingerprint
                    || revalidation.source_condition_definitions_sha256
                        != revalidation.required_condition_definitions_sha256
            })
            && unit.condition_refs == source_unit.condition_refs
            && codex_history::task_context::completion_review_revalidation_dependencies_match(
                &previous.state,
                &previous.state,
                source_unit,
                unit,
                &revalidation.required_artifact_fingerprint,
            )
    })
}

fn host_update_withdrawn_revalidation_alias_is_valid(
    unit: &codex_history::task_context::CompletionUnit,
    review: &codex_history::task_context::CompletionReviewState,
    reference: &codex_history::task_context::CompletionReviewSnapshotRef,
) -> bool {
    use codex_history::task_context::CompletionReviewWithdrawalSource;

    let Some(withdrawal) = review.withdrawal.as_ref() else {
        return false;
    };
    let CompletionReviewWithdrawalSource::HostUpdate {
        update_call_id,
        prior_status,
    } = &withdrawal.source
    else {
        return false;
    };
    let source_alias = review
        .revalidation
        .as_ref()
        .is_some_and(|source| &source.source_snapshot == reference);
    let target_matches = unit.review_target.as_ref().is_some_and(|target| {
        target.task_id == withdrawal.target_task_id
            && target.task_revision == withdrawal.target_task_revision
    });
    source_alias
        && !update_call_id.trim().is_empty()
        && *prior_status == codex_history::task_context::UnitStatus::Planned
        && unit.kind == codex_history::task_context::CompletionUnitKind::Review
        && unit.status == codex_history::task_context::UnitStatus::Cancelled
        && unit.review_decision_reason.as_deref() == Some(withdrawal.reason.as_str())
        && !withdrawal.reason.trim().is_empty()
        && target_matches
        && withdrawal.condition_refs == unit.condition_refs
        && withdrawal.depends_on == unit.depends_on
        && unit.review_evidence.is_none()
        && unit.review_launch.is_none()
        && review.active_attempt.is_none()
        && review.historical_failure.is_none()
        && review.retry_reservation_prior_historical_failure.is_none()
        && review.retry_authorization.is_none()
}

fn review_snapshot_references_resolve(
    snapshot: &Snapshot,
    current_sha256: &str,
    prior_host_snapshots: &BTreeMap<String, Value>,
) -> bool {
    snapshot.state.units.iter().all(|unit| {
        let Some(review) = unit.review_state.as_ref() else {
            return true;
        };
        if review.withdrawal.as_ref().is_some_and(|withdrawal| {
            matches!(
                &withdrawal.source,
                codex_history::task_context::CompletionReviewWithdrawalSource::LegacyV1Snapshot {
                    snapshot,
                } if review.previous_attempt_ref.as_ref() != Some(snapshot)
            )
        }) {
            return false;
        }
        let previous_ref_is_revalidation_source = review.revalidation.as_ref().is_some_and(
            |revalidation| review.previous_attempt_ref.as_ref() == Some(&revalidation.source_snapshot),
        );
        let has_consumed_retry_grant = review
            .active_attempt
            .as_ref()
            .is_some_and(|attempt| attempt.consumed_retry_authorization.is_some());
        if has_consumed_retry_grant
            && (review.previous_attempt_ref.is_none() || previous_ref_is_revalidation_source)
        {
            return false;
        }
        if review.revalidation.as_ref().is_some_and(|revalidation| {
            !revalidation_source_snapshot_resolves(
                snapshot,
                current_sha256,
                prior_host_snapshots,
                unit,
                revalidation,
            )
        }) {
            return false;
        }
        let previous_attempt_ref_resolves = review.previous_attempt_ref.as_ref().is_none_or(
            |reference| {
                let Some(previous) = load_review_snapshot_reference(
                    snapshot,
                    current_sha256,
                    prior_host_snapshots,
                    reference,
                ) else {
                    return false;
                };
                if previous_ref_is_revalidation_source
                    && !has_consumed_retry_grant
                        && (review.withdrawal.is_none()
                            || host_update_withdrawn_revalidation_alias_is_valid(
                                unit, review, reference,
                            ))
                {
                    // This ref proves the source adoption, which can be another unit and
                    // predates creation of the revalidation unit itself. A typed HostUpdate
                    // cancellation retains this source meaning only for its unstarted unit.
                    return true;
                }
                let Some(previous_unit) = previous
                    .state
                    .units
                    .iter()
                    .find(|previous_unit| previous_unit.id == unit.id)
                else {
                    return false;
                };
                if let Some(withdrawal) = review.withdrawal.as_ref()
                    && let codex_history::task_context::CompletionReviewWithdrawalSource::LegacyV1Snapshot {
                        snapshot: withdrawal_ref,
                    } = &withdrawal.source
                {
                    return withdrawal_ref == reference
                        && previous.version == 1
                        && previous_unit.kind
                            == codex_history::task_context::CompletionUnitKind::Review
                        && previous_unit.status
                            == codex_history::task_context::UnitStatus::Cancelled
                        && previous_unit.review_state.is_none()
                        && previous_unit.review_evidence.is_none()
                        && previous_unit.review_launch.is_none()
                        && previous_unit.review_decision_reason.is_none()
                        && previous_unit.review_target.as_ref().is_some_and(|target| {
                            target.task_id == withdrawal.target_task_id
                                && target.task_revision == withdrawal.target_task_revision
                        })
                        && previous_unit.condition_refs == withdrawal.condition_refs
                        && previous_unit.depends_on == withdrawal.depends_on;
                }
                if previous_unit.kind != codex_history::task_context::CompletionUnitKind::Review
                    || previous_unit.condition_refs != unit.condition_refs
                    || previous_unit.depends_on != unit.depends_on
                    || previous_unit.review_target != unit.review_target
                {
                    return false;
                }
                let Some(current_attempt) = review.active_attempt.as_ref() else {
                    return false;
                };
                let prior_state = previous_unit.review_state.as_ref();
                let prior_attempt = prior_state.and_then(|state| state.active_attempt.as_ref());
                if prior_attempt.is_some_and(|prior| prior == current_attempt) {
                    return true;
                }
                if let Some(grant) = current_attempt.consumed_retry_authorization.as_ref() {
                    return prior_state.is_some_and(|previous_state| {
                        let prior_matches = previous_state.active_attempt.as_ref().is_some_and(|prior| {
                            let same_artifact_fingerprint = codex_history::task_context::task_check_artifact_fingerprint(
                                &prior.target_artifact_ref,
                            )
                            .zip(codex_history::task_context::task_check_artifact_fingerprint(
                                &current_attempt.target_artifact_ref,
                            ))
                            .is_some_and(|(old, current)| old == current);
                            prior.attempt_id == grant.expected_attempt_id
                                && prior.status
                                    == codex_history::task_context::CompletionReviewAttemptStatus::Failed
                                && prior.target_task_id == current_attempt.target_task_id
                                && prior.target_task_revision == current_attempt.target_task_revision
                                && same_artifact_fingerprint
                                && prior.condition_refs == current_attempt.condition_refs
                                && prior.conditions_sha256 == current_attempt.conditions_sha256
                                && prior.terminal_source.as_ref().is_some_and(|source| {
                                    source.status
                                        == codex_history::task_context::CompletionReviewTerminalStatus::Failed
                                })
                        });
                        let historical_matches = review
                            .historical_failure
                            .as_ref()
                            .is_some_and(|failure| historical_failure_matches_prior(failure, prior_attempt));
                        previous_state.retry_authorization.as_ref() == Some(grant)
                            && prior_matches
                            && historical_matches
                            && current_attempt.attempt_id != grant.expected_attempt_id
                    });
                }
                if current_attempt.imported_from.as_ref() == Some(reference)
                    && previous.version == 1
                {
                    return LegacyReviewAttemptBinding::from_attempt(current_attempt)
                        .is_some_and(|binding| {
                            legacy_review_attempt_binding_matches(unit, previous_unit, binding)
                        });
                }
                prior_state
                    .and_then(|previous_state| previous_state.active_attempt.as_ref())
                    .is_some_and(|prior| {
                        prior.status
                                == codex_history::task_context::CompletionReviewAttemptStatus::Completed
                            && previous_unit.status
                                == codex_history::task_context::UnitStatus::Running
                            && prior.adoption_reason.is_none()
                            && prior.attempt_id != current_attempt.attempt_id
                            && prior.reviewer_thread_id == current_attempt.reviewer_thread_id
                            && prior.reviewer_turn_id.is_some()
                            && (match current_attempt.status {
                                codex_history::task_context::CompletionReviewAttemptStatus::Reserved
                                | codex_history::task_context::CompletionReviewAttemptStatus::Unknown => {
                                    current_attempt.reviewer_turn_id.is_none()
                                }
                                codex_history::task_context::CompletionReviewAttemptStatus::Bound
                                | codex_history::task_context::CompletionReviewAttemptStatus::TerminalUnknown
                                | codex_history::task_context::CompletionReviewAttemptStatus::Failed
                                | codex_history::task_context::CompletionReviewAttemptStatus::Completed
                                | codex_history::task_context::CompletionReviewAttemptStatus::Cancelled => {
                                    current_attempt.reviewer_turn_id.as_ref().is_some_and(|turn| {
                                        prior.reviewer_turn_id.as_ref() != Some(turn)
                                    })
                                }
                            })
                            && prior.target_task_id == current_attempt.target_task_id
                            && prior.target_task_revision == current_attempt.target_task_revision
                            && prior.target_artifact_ref == current_attempt.target_artifact_ref
                            && prior.reviewer_role == current_attempt.reviewer_role
                            && prior.reviewer_model == current_attempt.reviewer_model
                            && prior.condition_refs == current_attempt.condition_refs
                            && prior.conditions_sha256 == current_attempt.conditions_sha256
                            && prior.terminal_source.as_ref().is_some_and(|source| {
                                source.status
                                    == codex_history::task_context::CompletionReviewTerminalStatus::Completed
                            })
                            && prior.wait_result_ref.as_deref().is_some_and(|value| !value.is_empty())
                            && prior.wait_call_id.as_deref().is_some_and(|value| !value.is_empty())
                    })
            },
        );
        let imported_attempts_resolve = review
            .active_attempt
            .as_ref()
            .and_then(|attempt| attempt.imported_from.as_ref().map(|reference| (attempt, reference)))
            .is_none_or(|(attempt, reference)| {
                let Some(previous) = load_review_snapshot_reference(
                    snapshot,
                    current_sha256,
                    prior_host_snapshots,
                    reference,
                ) else {
                    return false;
                };
                previous.version == 1
                    && previous
                        .state
                        .units
                        .iter()
                        .find(|prior| prior.id == unit.id)
                        .is_some_and(|prior| {
                            LegacyReviewAttemptBinding::from_attempt(attempt).is_some_and(
                                |binding| legacy_review_attempt_binding_matches(unit, prior, binding),
                            )
                        })
            })
            && [&review.historical_failure, &review.retry_reservation_prior_historical_failure]
                .into_iter()
                .flatten()
                .all(|failure| {
                    let Some(reference) = failure.imported_from.as_ref() else {
                        return true;
                    };
                    let Some(previous) = load_review_snapshot_reference(
                        snapshot,
                        current_sha256,
                        prior_host_snapshots,
                        reference,
                    ) else {
                        return false;
                    };
                    previous.version == 1
                        && previous
                            .state
                            .units
                            .iter()
                            .find(|prior| prior.id == unit.id)
                            .is_some_and(|prior| {
                                legacy_review_attempt_binding_matches(
                                    unit,
                                    prior,
                                    LegacyReviewAttemptBinding::from_failure(failure),
                                )
                            })
                });
        previous_attempt_ref_resolves && imported_attempts_resolve
    })
}

fn load_review_snapshot_reference(
    current: &Snapshot,
    current_sha256: &str,
    prior_host_snapshots: &BTreeMap<String, Value>,
    reference: &codex_history::task_context::CompletionReviewSnapshotRef,
) -> Option<Snapshot> {
    if reference.sha256 == current_sha256 {
        return None;
    }
    let value = prior_host_snapshots.get(&reference.sha256)?;
    if maintenance_snapshot_digest(value).ok().as_deref() != Some(reference.sha256.as_str()) {
        return None;
    }
    let previous = serde_json::from_value::<Snapshot>(value.clone()).ok()?;
    (previous.version == reference.version
        && previous.thread_id == current.thread_id
        && previous.root_task_id == current.root_task_id
        && completion_review_event_parents_match_snapshot(&previous))
    .then_some(previous)
}

#[derive(Clone, Copy)]
struct LegacyReviewAttemptBinding<'a> {
    attempt_id: &'a str,
    reviewer_thread_id: &'a str,
    reviewer_turn_id: &'a str,
    target_task_id: &'a str,
    target_task_revision: u64,
    target_artifact_ref: &'a str,
    reviewer_role: &'a str,
    wait_result_ref: Option<&'a str>,
    wait_call_id: Option<&'a str>,
}

impl<'a> LegacyReviewAttemptBinding<'a> {
    fn from_attempt(
        attempt: &'a codex_history::task_context::CompletionReviewAttempt,
    ) -> Option<Self> {
        Some(Self {
            attempt_id: &attempt.attempt_id,
            reviewer_thread_id: attempt.reviewer_thread_id.as_deref()?,
            reviewer_turn_id: attempt.reviewer_turn_id.as_deref()?,
            target_task_id: &attempt.target_task_id,
            target_task_revision: attempt.target_task_revision,
            target_artifact_ref: &attempt.target_artifact_ref,
            reviewer_role: &attempt.reviewer_role,
            wait_result_ref: attempt.wait_result_ref.as_deref(),
            wait_call_id: attempt.wait_call_id.as_deref(),
        })
    }

    fn from_failure(
        failure: &'a codex_history::task_context::CompletionReviewHistoricalFailure,
    ) -> Self {
        Self {
            attempt_id: &failure.attempt_id,
            reviewer_thread_id: &failure.reviewer_thread_id,
            reviewer_turn_id: &failure.reviewer_turn_id,
            target_task_id: &failure.target_task_id,
            target_task_revision: failure.target_task_revision,
            target_artifact_ref: &failure.target_artifact_ref,
            reviewer_role: &failure.reviewer_role,
            wait_result_ref: failure.wait_result_ref.as_deref(),
            wait_call_id: failure.wait_call_id.as_deref(),
        }
    }
}

fn legacy_review_attempt_binding_matches(
    current_unit: &codex_history::task_context::CompletionUnit,
    prior_unit: &codex_history::task_context::CompletionUnit,
    binding: LegacyReviewAttemptBinding<'_>,
) -> bool {
    let Some(evidence) = prior_unit.review_evidence.as_ref() else {
        return false;
    };
    prior_unit.id == current_unit.id
        && prior_unit.kind == codex_history::task_context::CompletionUnitKind::Review
        && prior_unit.condition_refs == current_unit.condition_refs
        && prior_unit.depends_on == current_unit.depends_on
        && prior_unit.review_target == current_unit.review_target
        && prior_unit.status == codex_history::task_context::UnitStatus::Failed
        && prior_unit.review_state.is_none()
        && prior_unit.review_launch.is_none()
        && prior_unit.review_decision_reason.is_none()
        && prior_unit.review_target.as_ref().is_some_and(|target| {
            target.task_id == binding.target_task_id
                && target.task_revision == binding.target_task_revision
        })
        && evidence.spawn_call_id == binding.attempt_id
        && evidence.reviewer_thread_id == binding.reviewer_thread_id
        && evidence.reviewer_turn_id == binding.reviewer_turn_id
        && evidence.reviewer_role == binding.reviewer_role
        && evidence.target_task_id == binding.target_task_id
        && evidence.target_task_revision == binding.target_task_revision
        && evidence.target_artifact_ref == binding.target_artifact_ref
        && evidence.wait_result_ref.as_deref() == binding.wait_result_ref
        && evidence.wait_call_id.as_deref() == binding.wait_call_id
        && evidence.terminal_status
            == Some(codex_history::task_context::CompletionReviewTerminalStatus::Failed)
}

fn historical_failure_matches_prior(
    failure: &codex_history::task_context::CompletionReviewHistoricalFailure,
    prior: Option<&codex_history::task_context::CompletionReviewAttempt>,
) -> bool {
    let Some(prior) = prior else {
        return false;
    };
    prior.status == codex_history::task_context::CompletionReviewAttemptStatus::Failed
        && failure.attempt_id == prior.attempt_id
        && failure.contract_revision == Some(prior.contract_revision)
        && failure.imported_from == prior.imported_from
        && failure.consumed_retry_authorization == prior.consumed_retry_authorization
        && prior.reviewer_thread_id.as_deref() == Some(failure.reviewer_thread_id.as_str())
        && prior.reviewer_turn_id.as_deref() == Some(failure.reviewer_turn_id.as_str())
        && failure.target_task_id == prior.target_task_id
        && failure.target_task_revision == prior.target_task_revision
        && failure.target_artifact_ref == prior.target_artifact_ref
        && failure.reviewer_role == prior.reviewer_role
        && failure.reviewer_model == prior.reviewer_model
        && failure.deadline_unix_ms == prior.deadline_unix_ms
        && failure.wait_result_ref == prior.wait_result_ref
        && failure.wait_call_id == prior.wait_call_id
        && failure.deadline_escalation == prior.deadline_escalation
        && failure.deadline_safety_fingerprint == prior.deadline_safety_fingerprint
        && prior.terminal_source.as_ref() == Some(&failure.terminal_source)
}

/// Hash a completion snapshot using the canonical JSON digest used by compaction history.
pub fn maintenance_snapshot_digest(value: &Value) -> Result<String, String> {
    codex_history::compaction_candidate::digest(value)
}

/// Validate one snapshot in canonical history with the restore payload validator.
pub fn inspect_maintenance_repair_snapshot(
    items: &[RolloutItem],
    target_snapshot_sha256: &str,
    expected_thread_id: &str,
    expected_root_task_id: &str,
    allow_legacy_scope_root_mismatch: bool,
) -> Result<MaintenanceRepairSnapshotInfo, String> {
    if !is_sha256(target_snapshot_sha256) {
        return Err("target snapshot digest is invalid".into());
    }
    let committed_history = codex_history::compaction_transaction::committed_items(items)?;
    let mut response_items = Vec::new();
    let mut accepted_receipts = None;
    let mut previous_snapshot: Option<(String, u64, u64, bool)> = None;
    let mut target_info = None;
    for item in &committed_history {
        let checkpoint_changed = match item {
            RolloutItem::ResponseItem(envelope) => {
                response_items.push(envelope.clone());
                envelope
                    .metadata
                    .as_ref()
                    .is_some_and(|metadata| metadata.rencrow_compaction.is_some())
            }
            RolloutItem::Compacted(compacted) => {
                if let Some(replacement) = &compacted.replacement_history {
                    response_items = replacement.clone();
                }
                true
            }
            _ => false,
        };
        if checkpoint_changed {
            accepted_receipts =
                crate::compact::latest_task_state_in_history(&response_items, expected_thread_id)?;
        }
        let RolloutItem::ResponseItem(envelope) = item else {
            continue;
        };
        let Some(metadata) = envelope.metadata.as_ref() else {
            continue;
        };
        let Some(value) = metadata.rencrow_completion.as_ref() else {
            continue;
        };
        let is_host = matches!(&envelope.item, ResponseItem::Message {
            role, internal_chat_message_metadata_passthrough: Some(kind), ..
        } if role == "developer" && kind.content_item_kinds.as_ref().is_some_and(|kinds| {
            kinds.len() == 1 && kinds[0].0 == "rencrow.completion_snapshot"
        })) && !metadata.client_authored;
        if !is_host {
            if maintenance_snapshot_digest(value)? == target_snapshot_sha256 {
                return Err("target completion snapshot is not host-authored".into());
            }
            continue;
        }
        let snapshot = serde_json::from_value::<Snapshot>(value.clone())
            .map_err(|error| format!("completion snapshot is malformed: {error}"))?;
        let previous = previous_snapshot
            .as_ref()
            .map(|(root, revision, generation, paused)| {
                (root.as_str(), *revision, *generation, *paused)
            });
        validate_snapshot_sequence_transition(previous, &snapshot)?;
        previous_snapshot = Some((
            snapshot.root_task_id.clone(),
            snapshot.state.contract_revision,
            snapshot.resume_generation,
            snapshot.state.paused,
        ));
        if maintenance_snapshot_digest(value)? != target_snapshot_sha256 {
            continue;
        }
        if snapshot.thread_id != expected_thread_id
            || snapshot.root_task_id != expected_root_task_id
        {
            return Err("target completion snapshot belongs to another thread or root".into());
        }
        let scope_root_mismatch = snapshot
            .active_scope
            .as_ref()
            .is_some_and(|scope| scope.root_task_id != snapshot.root_task_id);
        if scope_root_mismatch != allow_legacy_scope_root_mismatch {
            return Err(
                "target completion snapshot has an unexpected scope/root relationship".into(),
            );
        }
        if accepted_receipts
            .as_ref()
            .is_some_and(|state| state.boundary.root_task_id != snapshot.root_task_id)
        {
            return Err(
                "target completion snapshot disagrees with the accepted task boundary root".into(),
            );
        }
        let receipts = accepted_receipts
            .as_ref()
            .map(|state| state.completed.clone())
            .unwrap_or_default();
        validate_snapshot_payload(&snapshot, expected_thread_id, &receipts)?;
        if target_info.is_none() {
            target_info = Some(MaintenanceRepairSnapshotInfo {
                value: value.clone(),
                snapshot_sha256: target_snapshot_sha256.to_owned(),
                root_task_id: snapshot.root_task_id,
                resume_generation: snapshot.resume_generation,
                state: snapshot.state,
                active_scope: snapshot
                    .active_scope
                    .map(|scope| serde_json::to_value(scope).map_err(|error| error.to_string()))
                    .transpose()?,
                uncollected_children: snapshot.uncollected_children,
            });
        }
    }
    target_info
        .ok_or_else(|| "target completion snapshot was not found in canonical history".into())
}

/// Build a generation-bumped pause repair without changing contract, scope, checks or children.
pub fn build_maintenance_repair_snapshot(
    current_value: &Value,
    receipt: MaintenanceRepairReceipt,
) -> Result<Value, String> {
    let mut snapshot = serde_json::from_value::<Snapshot>(current_value.clone())
        .map_err(|error| format!("current completion snapshot is malformed: {error}"))?;
    let current_sha256 = maintenance_snapshot_digest(current_value)?;
    if !snapshot.state.paused
        || receipt.snapshot_before_sha256 != current_sha256
        || receipt.resume_generation_before != snapshot.resume_generation
    {
        return Err("repair receipt does not match the paused snapshot".into());
    }
    snapshot.state.paused = false;
    snapshot.resume_generation = snapshot
        .resume_generation
        .checked_add(1)
        .ok_or_else(|| "completion resume generation overflow".to_string())?;
    snapshot.maintenance_repair_receipt = Some(receipt);
    let receipt = snapshot
        .maintenance_repair_receipt
        .as_ref()
        .ok_or_else(|| "repair receipt was not attached".to_string())?;
    validate_repair_receipt(receipt, &snapshot, &snapshot.thread_id)?;
    let value = serde_json::to_value(snapshot).map_err(|error| error.to_string())?;
    if serde_json::to_vec(&value)
        .map_err(|error| error.to_string())?
        .len()
        > MAX_COMPLETION_SNAPSHOT_BYTES
    {
        return Err("repaired completion snapshot exceeds its size bound".into());
    }
    Ok(value)
}

fn maintenance_receipt_matches_history(
    receipt: &MaintenanceRepairReceipt,
    repair_value: &Value,
    host_snapshots: &BTreeMap<String, Value>,
) -> bool {
    let Some(shutdown_before) = host_snapshots.get(&receipt.shutdown_before_snapshot_sha256) else {
        return false;
    };
    let Some(shutdown_paused) = host_snapshots.get(&receipt.shutdown_paused_snapshot_sha256) else {
        return false;
    };
    let Some(paused_current) = host_snapshots.get(&receipt.snapshot_before_sha256) else {
        return false;
    };
    let Ok(before_snapshot) = serde_json::from_value::<Snapshot>(shutdown_before.clone()) else {
        return false;
    };
    let Ok(paused_snapshot) = serde_json::from_value::<Snapshot>(shutdown_paused.clone()) else {
        return false;
    };
    let Ok(repair_snapshot) = serde_json::from_value::<Snapshot>(repair_value.clone()) else {
        return false;
    };
    if before_snapshot.state.paused
        || !paused_snapshot.state.paused
        || before_snapshot.thread_id != paused_snapshot.thread_id
        || before_snapshot.root_task_id != paused_snapshot.root_task_id
        || before_snapshot.state.contract_revision != paused_snapshot.state.contract_revision
        || before_snapshot.resume_generation != paused_snapshot.resume_generation
        || receipt.thread_id != before_snapshot.thread_id
        || receipt.root_task_id != before_snapshot.root_task_id
    {
        return false;
    }
    let mut expected_paused = shutdown_before.clone();
    let Some(paused_flag) = expected_paused
        .get_mut("state")
        .and_then(Value::as_object_mut)
        .and_then(|state| state.get_mut("paused"))
    else {
        return false;
    };
    *paused_flag = Value::Bool(true);
    if expected_paused != *shutdown_paused {
        return false;
    }
    let Ok(expected_repair) = build_maintenance_repair_snapshot(paused_current, receipt.clone())
    else {
        return false;
    };
    expected_repair == *repair_value
        && repair_snapshot.root_task_id == paused_snapshot.root_task_id
        && repair_snapshot.resume_generation == paused_snapshot.resume_generation.saturating_add(1)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    version: u32,
    thread_id: String,
    root_task_id: String,
    state: CompletionState,
    #[serde(default)]
    resume_generation: u64,
    #[serde(default)]
    task_checks: Vec<TaskCheckSnapshot>,
    #[serde(default)]
    active_scope: Option<crate::state::ActiveTaskScope>,
    #[serde(default)]
    patch_attempts: Vec<(String, crate::state::TaskPatchAttempt)>,
    #[serde(default)]
    uncollected_children: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    maintenance_repair_receipt: Option<MaintenanceRepairReceipt>,
}

pub(super) enum PersistedRetryRollbackRef {
    NotRetry,
    Retry(Option<codex_history::task_context::CompletionReviewSnapshotRef>),
}

fn persisted_legacy_review_binding(
    contract_revision: u64,
    unit: &codex_history::task_context::CompletionUnit,
) -> Option<(String, crate::state::PersistedLegacyReviewBinding)> {
    if unit.kind != codex_history::task_context::CompletionUnitKind::Review
        || unit.review_state.is_some()
        || unit.review_launch.is_some()
    {
        return None;
    }
    let evidence = match (unit.status, unit.review_evidence.as_ref()) {
        (codex_history::task_context::UnitStatus::Cancelled, None)
            if unit.review_decision_reason.is_none() =>
        {
            None
        }
        (_, Some(evidence)) => Some(evidence.clone()),
        _ => return None,
    };
    Some((
        unit.id.clone(),
        crate::state::PersistedLegacyReviewBinding {
            contract_revision,
            status: unit.status,
            review_target: unit.review_target.clone(),
            condition_refs: unit.condition_refs.clone(),
            depends_on: unit.depends_on.clone(),
            evidence,
        },
    ))
}

impl Session {
    pub(super) async fn persisted_retry_rollback_previous_ref(
        &self,
        unit_id: &str,
    ) -> Result<PersistedRetryRollbackRef, String> {
        use codex_history::task_context::CompletionReviewAttemptStatus;

        let (root_task_id, unit_value, receipt, history_items) = {
            let state = self.state.lock().await;
            let Some((root_task_id, contract)) = state.rencrow_completion_state.as_ref() else {
                return Err("completion review contract disappeared before rollback".into());
            };
            let Some(unit) = contract.units.iter().find(|unit| unit.id == unit_id) else {
                return Err("completion review unit disappeared before rollback".into());
            };
            let Some(attempt) = unit
                .review_state
                .as_ref()
                .and_then(|review| review.active_attempt.as_ref())
            else {
                return Ok(PersistedRetryRollbackRef::NotRetry);
            };
            if attempt.consumed_retry_authorization.is_none() {
                return Ok(PersistedRetryRollbackRef::NotRetry);
            }
            if !matches!(attempt.status, CompletionReviewAttemptStatus::Reserved)
                || attempt.reviewer_thread_id.is_some()
                || attempt.reviewer_turn_id.is_some()
            {
                return Err("failed retry reservation is no longer safely rollbackable".into());
            }
            let receipt = state
                .rencrow_completion_last_persisted_snapshot
                .clone()
                .ok_or_else(|| {
                    "failed retry reservation has no persisted snapshot receipt".to_string()
                })?;
            let attempt_value = serde_json::to_value(attempt).map_err(|error| error.to_string())?;
            let attempt_sha256 = maintenance_snapshot_digest(&attempt_value)?;
            if receipt.version != 2
                || receipt.root_task_id.as_str() != root_task_id.as_str()
                || receipt.review_attempts.get(unit_id).is_none_or(|identity| {
                    identity.attempt_id != attempt.attempt_id
                        || identity.status != CompletionReviewAttemptStatus::Reserved
                        || identity.reviewer_thread_id.is_some()
                        || identity.reviewer_turn_id.is_some()
                        || identity.target_task_id != attempt.target_task_id
                        || identity.target_task_revision != attempt.target_task_revision
                        || identity.target_artifact_ref != attempt.target_artifact_ref
                        || identity.attempt_sha256 != attempt_sha256
                })
            {
                return Err(
                    "persisted retry reservation receipt does not match the active attempt".into(),
                );
            }
            (
                root_task_id.clone(),
                serde_json::to_value(unit).map_err(|error| error.to_string())?,
                receipt,
                state.history.annotated_items().to_vec(),
            )
        };

        let mut host_snapshots = BTreeMap::new();
        for envelope in &history_items {
            let Some(metadata) = envelope.metadata.as_ref() else {
                continue;
            };
            let Some(value) = metadata.rencrow_completion.as_ref() else {
                continue;
            };
            let is_host = matches!(&envelope.item, ResponseItem::Message {
                role,
                internal_chat_message_metadata_passthrough: Some(kind),
                ..
            } if role == "developer" && kind.content_item_kinds.as_ref().is_some_and(|kinds| {
                kinds.len() == 1 && kinds[0].0 == "rencrow.completion_snapshot"
            })) && !metadata.client_authored;
            if is_host {
                let digest = maintenance_snapshot_digest(value)?;
                host_snapshots.insert(digest, value.clone());
            }
        }
        let value = host_snapshots
            .get(&receipt.sha256)
            .ok_or_else(|| "persisted retry reservation snapshot is unavailable".to_string())?;
        if maintenance_snapshot_digest(value)? != receipt.sha256 {
            return Err("persisted retry reservation snapshot digest is invalid".into());
        }
        let reserved = serde_json::from_value::<Snapshot>(value.clone())
            .map_err(|error| format!("persisted retry reservation snapshot is invalid: {error}"))?;
        if reserved.version != receipt.version
            || reserved.thread_id != self.thread_id.to_string()
            || reserved.root_task_id != root_task_id
            || reserved
                .state
                .units
                .iter()
                .find(|unit| unit.id == unit_id)
                .and_then(|unit| serde_json::to_value(unit).ok())
                .as_ref()
                != Some(&unit_value)
        {
            return Err(
                "persisted retry reservation snapshot does not match the current reservation"
                    .into(),
            );
        }
        if !review_snapshot_references_resolve(&reserved, &receipt.sha256, &host_snapshots) {
            return Err(
                "persisted retry reservation snapshot does not validate its source chain".into(),
            );
        }
        let reserved_unit = reserved
            .state
            .units
            .iter()
            .find(|unit| unit.id == unit_id)
            .ok_or_else(|| {
                "persisted retry reservation snapshot lost its review unit".to_string()
            })?;
        let reserved_review = reserved_unit
            .review_state
            .as_ref()
            .ok_or_else(|| "persisted retry reservation lost canonical review state".to_string())?;
        let grant = reserved_review
            .active_attempt
            .as_ref()
            .and_then(|attempt| attempt.consumed_retry_authorization.as_ref())
            .ok_or_else(|| "persisted retry reservation lost its consumed grant".to_string())?;
        let prior_ref = reserved_review
            .previous_attempt_ref
            .as_ref()
            .ok_or_else(|| {
                "persisted retry reservation lost its predecessor reference".to_string()
            })?;
        let prior =
            load_review_snapshot_reference(&reserved, &receipt.sha256, &host_snapshots, prior_ref)
                .ok_or_else(|| {
                    "retry predecessor snapshot is unavailable or invalid".to_string()
                })?;
        let prior_unit = prior
            .state
            .units
            .iter()
            .find(|unit| unit.id == unit_id)
            .ok_or_else(|| "retry predecessor snapshot lost its review unit".to_string())?;
        let prior_review = prior_unit
            .review_state
            .as_ref()
            .ok_or_else(|| "retry predecessor snapshot lost canonical review state".to_string())?;
        let prior_attempt = prior_review
            .active_attempt
            .as_ref()
            .ok_or_else(|| "retry predecessor snapshot lost its failed attempt".to_string())?;
        let current_review = reserved_review;
        if prior.version != prior_ref.version
            || prior_unit.review_target != reserved_unit.review_target
            || prior_unit.condition_refs != reserved_unit.condition_refs
            || prior_unit.depends_on != reserved_unit.depends_on
            || prior_attempt.status != CompletionReviewAttemptStatus::Failed
            || prior_attempt.attempt_id != grant.expected_attempt_id
            || prior_review.retry_authorization.as_ref() != Some(grant)
            || current_review
                .retry_reservation_prior_historical_failure
                .as_ref()
                != prior_review.historical_failure.as_ref()
            || !current_review
                .historical_failure
                .as_ref()
                .is_some_and(|failure| {
                    historical_failure_matches_prior(failure, Some(prior_attempt))
                })
        {
            return Err(
                "retry predecessor does not match the exact persisted failed attempt".into(),
            );
        }
        if !review_snapshot_references_resolve(&prior, &prior_ref.sha256, &host_snapshots) {
            return Err(
                "retry predecessor snapshot does not resolve its own persisted source references"
                    .into(),
            );
        }
        Ok(PersistedRetryRollbackRef::Retry(
            prior_review.previous_attempt_ref.clone(),
        ))
    }

    pub(crate) async fn persisted_legacy_review_snapshot_ref(
        &self,
        root_task_id: &str,
        unit_id: &str,
        expected: &crate::state::PersistedLegacyReviewBinding,
    ) -> Option<codex_history::task_context::CompletionReviewSnapshotRef> {
        let state = self.state.lock().await;
        let receipt = state.rencrow_completion_last_persisted_snapshot.as_ref()?;
        if receipt.version != 1
            || receipt.root_task_id != root_task_id
            || receipt.legacy_review_bindings.get(unit_id) != Some(expected)
        {
            return None;
        }
        let (current_root, current) = state.rencrow_completion_state.as_ref()?;
        let current_unit = current.units.iter().find(|unit| unit.id == unit_id)?;
        if current_root != root_task_id
            || current.contract_revision != expected.contract_revision
            || current_unit.status != expected.status
            || current_unit.review_target != expected.review_target
            || current_unit.condition_refs != expected.condition_refs
            || current_unit.depends_on != expected.depends_on
            || current_unit.review_state.is_some()
            || current_unit.review_evidence != expected.evidence
            || current_unit.review_launch.is_some()
            || current_unit.review_decision_reason.is_some()
        {
            return None;
        }
        Some(codex_history::task_context::CompletionReviewSnapshotRef {
            version: receipt.version,
            sha256: receipt.sha256.clone(),
        })
    }

    pub(crate) async fn persisted_review_attempt_snapshot_ref(
        &self,
        root_task_id: &str,
        unit_id: &str,
        expected_attempt: &codex_history::task_context::CompletionReviewAttempt,
    ) -> Result<Option<codex_history::task_context::CompletionReviewSnapshotRef>, String> {
        let expected_attempt_sha256 = serde_json::to_value(expected_attempt)
            .map_err(|error| error.to_string())
            .and_then(|value| maintenance_snapshot_digest(&value))?;
        let expected_terminal_source_sha256 = expected_attempt
            .terminal_source
            .as_ref()
            .map(|source| serde_json::to_value(source).map_err(|error| error.to_string()))
            .transpose()?
            .map(|value| codex_history::compaction_candidate::digest(&value))
            .transpose()?;
        let Some(expected_terminal_source_sha256) = expected_terminal_source_sha256.as_deref()
        else {
            return Ok(None);
        };
        let terminal_status_matches_attempt = expected_attempt
            .terminal_source
            .as_ref()
            .is_some_and(|source| match expected_attempt.status {
                codex_history::task_context::CompletionReviewAttemptStatus::Failed => {
                    source.status
                        == codex_history::task_context::CompletionReviewTerminalStatus::Failed
                        && expected_attempt.adoption_reason.is_none()
                        && (source.event.is_some() || expected_attempt.wait_result_ref.is_some())
                }
                codex_history::task_context::CompletionReviewAttemptStatus::Completed => {
                    source.status
                        == codex_history::task_context::CompletionReviewTerminalStatus::Completed
                        && expected_attempt.adoption_reason.is_none()
                        && expected_attempt.wait_result_ref.is_some()
                        && expected_attempt.wait_call_id.is_some()
                }
                _ => false,
            });
        if !terminal_status_matches_attempt {
            return Ok(None);
        }
        let state = self.state.lock().await;
        let Some(receipt) = state.rencrow_completion_last_persisted_snapshot.as_ref() else {
            return Ok(None);
        };
        let expected_parent_thread_id = self.thread_id.to_string();
        let source_matches_parent = state
            .rencrow_completion_state
            .as_ref()
            .filter(|(root, _)| root == root_task_id)
            .and_then(|(_, contract)| contract.units.iter().find(|unit| unit.id == unit_id))
            .and_then(|unit| unit.review_state.as_ref())
            .and_then(|review| review.active_attempt.as_ref())
            .filter(|attempt| {
                attempt.attempt_id == expected_attempt.attempt_id
                    && attempt.reviewer_thread_id == expected_attempt.reviewer_thread_id
                    && attempt.reviewer_turn_id == expected_attempt.reviewer_turn_id
            })
            .and_then(|attempt| attempt.terminal_source.as_ref())
            .is_some_and(|source| {
                source.event.as_ref().is_none_or(|event| {
                    event.owner_parent_thread_id.as_deref()
                        == Some(expected_parent_thread_id.as_str())
                }) && serde_json::to_value(source)
                    .ok()
                    .and_then(|value| codex_history::compaction_candidate::digest(&value).ok())
                    .as_deref()
                    == Some(expected_terminal_source_sha256)
            });
        if receipt.root_task_id != root_task_id
            || !source_matches_parent
            || receipt.review_attempts.get(unit_id).is_none_or(|attempt| {
                attempt.attempt_id != expected_attempt.attempt_id
                    || attempt.reviewer_thread_id != expected_attempt.reviewer_thread_id
                    || attempt.reviewer_turn_id != expected_attempt.reviewer_turn_id
                    || attempt.target_task_id != expected_attempt.target_task_id
                    || attempt.target_task_revision != expected_attempt.target_task_revision
                    || attempt.target_artifact_ref != expected_attempt.target_artifact_ref
                    || attempt.status != expected_attempt.status
                    || attempt.attempt_sha256 != expected_attempt_sha256
                    || attempt.terminal_source_sha256.as_deref()
                        != Some(expected_terminal_source_sha256)
            })
        {
            return Ok(None);
        }
        Ok(Some(
            codex_history::task_context::CompletionReviewSnapshotRef {
                version: receipt.version,
                sha256: receipt.sha256.clone(),
            },
        ))
    }

    pub(crate) async fn persisted_completed_review_predecessor_turn(
        &self,
        root_task_id: &str,
        unit_id: &str,
        attempt_id: &str,
        child_thread_id: &str,
    ) -> Result<Option<String>, String> {
        let (previous_ref, receipt, history_items) = {
            let state = self.state.lock().await;
            let Some((root, contract)) = state.rencrow_completion_state.as_ref() else {
                return Err("review predecessor lookup has no completion contract".into());
            };
            if root != root_task_id {
                return Err("review predecessor lookup root does not match".into());
            }
            let Some(attempt) = contract
                .units
                .iter()
                .find(|unit| unit.id == unit_id)
                .and_then(|unit| unit.review_state.as_ref())
                .and_then(|review| review.active_attempt.as_ref())
            else {
                return Ok(None);
            };
            if attempt.attempt_id != attempt_id
                || attempt.reviewer_thread_id.as_deref() != Some(child_thread_id)
                || attempt.reviewer_turn_id.is_some()
                || !matches!(
                    attempt.status,
                    codex_history::task_context::CompletionReviewAttemptStatus::Reserved
                        | codex_history::task_context::CompletionReviewAttemptStatus::Unknown
                )
            {
                return Ok(None);
            }
            let Some(previous_ref) = contract
                .units
                .iter()
                .find(|unit| unit.id == unit_id)
                .and_then(|unit| unit.review_state.as_ref())
                .and_then(|review| review.previous_attempt_ref.clone())
            else {
                return Ok(None);
            };
            let Some(receipt) = state.rencrow_completion_last_persisted_snapshot.as_ref() else {
                return Err(
                    "review predecessor lookup has no persisted current snapshot receipt".into(),
                );
            };
            (
                previous_ref,
                (
                    receipt.version,
                    receipt.sha256.clone(),
                    receipt.root_task_id.clone(),
                    receipt.review_attempts.get(unit_id).cloned(),
                ),
                state.history.annotated_items().to_vec(),
            )
        };
        let (receipt_version, current_sha256, receipt_root, persisted_identity) = receipt;
        let Some(persisted_identity) = persisted_identity else {
            return Err(
                "review predecessor lookup has no persisted active-attempt identity".into(),
            );
        };
        if receipt_root != root_task_id
            || persisted_identity.attempt_id != attempt_id
            || persisted_identity.reviewer_thread_id.as_deref() != Some(child_thread_id)
            || persisted_identity.reviewer_turn_id.is_some()
            || !matches!(
                persisted_identity.status,
                codex_history::task_context::CompletionReviewAttemptStatus::Reserved
                    | codex_history::task_context::CompletionReviewAttemptStatus::Unknown
            )
        {
            return Err(
                "review predecessor lookup receipt does not match the unresolved attempt".into(),
            );
        }
        let mut host_snapshots = BTreeMap::new();
        for envelope in &history_items {
            let Some(metadata) = envelope.metadata.as_ref() else {
                continue;
            };
            let Some(value) = metadata.rencrow_completion.as_ref() else {
                continue;
            };
            let is_host = matches!(&envelope.item, ResponseItem::Message {
                role,
                internal_chat_message_metadata_passthrough: Some(kind),
                ..
            } if role == "developer" && kind.content_item_kinds.as_ref().is_some_and(|kinds| {
                kinds.len() == 1 && kinds[0].0 == "rencrow.completion_snapshot"
            })) && !metadata.client_authored;
            if is_host {
                let digest = maintenance_snapshot_digest(value)?;
                host_snapshots.insert(digest, value.clone());
            }
        }
        let Some(current_value) = host_snapshots.get(&current_sha256) else {
            return Err(
                "review predecessor lookup cannot find the persisted current host snapshot".into(),
            );
        };
        let current = serde_json::from_value::<Snapshot>(current_value.clone())
            .map_err(|error| format!("persisted current review snapshot is invalid: {error}"))?;
        let digest_matches = maintenance_snapshot_digest(current_value)? == current_sha256;
        let child_is_uncollected = current
            .uncollected_children
            .iter()
            .any(|id| id == child_thread_id);
        if receipt_version != 2
            || current.version != 2
            || current.thread_id != self.thread_id.to_string()
            || current.root_task_id != root_task_id
            || !digest_matches
            || !child_is_uncollected
        {
            return Err(
                "review predecessor lookup cannot validate its persisted source chain".into(),
            );
        }
        if !review_snapshot_references_resolve(&current, &current_sha256, &host_snapshots) {
            return Err(
                "review predecessor lookup cannot validate its persisted source chain".into(),
            );
        }
        let Some(current_unit) = current.state.units.iter().find(|unit| unit.id == unit_id) else {
            return Err("persisted review predecessor lookup lost its current unit".into());
        };
        let Some(current_attempt) = current_unit
            .review_state
            .as_ref()
            .and_then(|review| review.active_attempt.as_ref())
        else {
            return Err("persisted review predecessor lookup lost its active attempt".into());
        };
        let current_attempt_value =
            serde_json::to_value(current_attempt).map_err(|error| error.to_string())?;
        if current_attempt.attempt_id != attempt_id
            || current_attempt.reviewer_thread_id.as_deref() != Some(child_thread_id)
            || current_attempt.reviewer_turn_id.is_some()
            || maintenance_snapshot_digest(&current_attempt_value)?
                != persisted_identity.attempt_sha256
        {
            return Err(
                "persisted review predecessor lookup has a different active attempt".into(),
            );
        }
        let Some(previous) = load_review_snapshot_reference(
            &current,
            &current_sha256,
            &host_snapshots,
            &previous_ref,
        ) else {
            return Err("review predecessor snapshot reference is unavailable or invalid".into());
        };
        let Some(previous_unit) = previous.state.units.iter().find(|unit| unit.id == unit_id)
        else {
            // Revalidation snapshots can validly predate creation of this unit.
            return Ok(None);
        };
        let Some(previous_attempt) = previous_unit
            .review_state
            .as_ref()
            .and_then(|review| review.active_attempt.as_ref())
        else {
            return Ok(None);
        };
        if previous_attempt.status
            != codex_history::task_context::CompletionReviewAttemptStatus::Completed
            || previous_attempt.reviewer_thread_id.as_deref() != Some(child_thread_id)
        {
            return Ok(None);
        }
        let Some(previous_turn_id) = previous_attempt.reviewer_turn_id.as_ref() else {
            return Err("completed predecessor has no exact reviewer turn".into());
        };
        if previous_unit.status != codex_history::task_context::UnitStatus::Running
            || previous_attempt.adoption_reason.is_some()
            || previous_attempt
                .terminal_source
                .as_ref()
                .is_none_or(|source| {
                    source.status
                        != codex_history::task_context::CompletionReviewTerminalStatus::Completed
                })
            || previous_attempt
                .wait_result_ref
                .as_deref()
                .is_none_or(str::is_empty)
            || previous_attempt
                .wait_call_id
                .as_deref()
                .is_none_or(str::is_empty)
            || previous_attempt.target_task_id != current_attempt.target_task_id
            || previous_attempt.target_task_revision != current_attempt.target_task_revision
            || previous_attempt.reviewer_role != current_attempt.reviewer_role
            || previous_attempt.reviewer_model != current_attempt.reviewer_model
            || previous_attempt.condition_refs != current_attempt.condition_refs
            || previous_attempt.conditions_sha256 != current_attempt.conditions_sha256
        {
            return Err("completed predecessor is not an exact unadopted wait result".into());
        }
        Ok(Some(previous_turn_id.clone()))
    }

    pub(crate) async fn autonomous_completion_event_seen(&self, event_key: &str) -> bool {
        self.state
            .lock()
            .await
            .rencrow_completion_state
            .as_ref()
            .is_some_and(|(_, state)| {
                state
                    .consumed_host_events
                    .iter()
                    .any(|key| key == event_key)
            })
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "dedicated append lock serializes capture and asynchronous rollout persistence; SessionState locks are scoped separately"
    )]
    pub(crate) async fn persist_autonomous_completion_snapshot(&self) -> Result<(), String> {
        let persist_lock = self
            .state
            .lock()
            .await
            .rencrow_completion_persist_lock
            .clone();
        // Serialize capture and the async append together; this dedicated lock never
        // protects SessionState and prevents older snapshots overtaking newer ones.
        let _persist_guard = persist_lock.lock().await;
        {
            let mut state = self.state.lock().await;
            let legacy_receipt = state
                .rencrow_completion_last_persisted_snapshot
                .as_ref()
                .filter(|receipt| receipt.version == 1)
                .cloned();
            if let (Some(receipt), Some((root, contract))) =
                (legacy_receipt, state.rencrow_completion_state.as_mut())
                && receipt.root_task_id.as_str() == root.as_str()
                && contract
                    .units
                    .iter()
                    .any(|unit| unit.review_state.is_some())
            {
                let current_contract_revision = contract.contract_revision;
                for unit in &mut contract.units {
                    if unit.kind != codex_history::task_context::CompletionUnitKind::Review
                        || unit.status != codex_history::task_context::UnitStatus::Cancelled
                        || unit.review_state.is_some()
                        || unit.review_evidence.is_some()
                        || unit.review_launch.is_some()
                        || unit.review_decision_reason.is_some()
                    {
                        continue;
                    }
                    let Some((_, mut expected_binding)) =
                        persisted_legacy_review_binding(current_contract_revision, unit)
                    else {
                        continue;
                    };
                    let Some(source_binding) = receipt.legacy_review_bindings.get(&unit.id) else {
                        continue;
                    };
                    // The overall contract revision may advance during the explicit operation
                    // opting into v2. The old unit identity and all of its own binding fields
                    // must still match the exact v1 snapshot.
                    expected_binding.contract_revision = source_binding.contract_revision;
                    if source_binding != &expected_binding {
                        continue;
                    }
                    let Some(target) = unit.review_target.clone() else {
                        continue;
                    };
                    let reference = codex_history::task_context::CompletionReviewSnapshotRef {
                        version: 1,
                        sha256: receipt.sha256.clone(),
                    };
                    let review_state = codex_history::task_context::CompletionReviewState {
                        version: 1,
                        obligation_unit_id: unit.id.clone(),
                        active_attempt: None,
                        historical_failure: None,
                        retry_reservation_prior_historical_failure: None,
                        previous_attempt_ref: Some(reference.clone()),
                        retry_authorization: None,
                        withdrawal: Some(
                            codex_history::task_context::CompletionReviewWithdrawal {
                                source: codex_history::task_context::CompletionReviewWithdrawalSource::LegacyV1Snapshot {
                                    snapshot: reference,
                                },
                                reason: "Preserved the exact unstarted cancellation from the v1 completion plan.".into(),
                                target_task_id: target.task_id,
                                target_task_revision: target.task_revision,
                                condition_refs: unit.condition_refs.clone(),
                                depends_on: unit.depends_on.clone(),
                            },
                        ),
                            revalidation: None,
                    };
                    unit.review_state = Some(review_state);
                    if let Ok(projected) =
                        codex_history::task_context::project_completion_review_state(unit)
                    {
                        *unit = projected;
                    }
                }
            }
        }
        let (
            completion_state,
            task_checks,
            referenced_task_checks,
            resume_generation,
            uncollected_children,
            active_scope,
            patch_attempts,
        ) = {
            let state = self.state.lock().await;
            let referenced_task_checks = state
                .rencrow_completion_state
                .as_ref()
                .map(|(_, contract)| completion_task_check_references(contract))
                .unwrap_or_default();
            (
                state.rencrow_completion_state.clone(),
                state
                    .task_check_ledger
                    .snapshot_with_references(&referenced_task_checks),
                referenced_task_checks,
                state.rencrow_completion_resume_generation,
                state
                    .rencrow_completion_uncollected_children
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>(),
                state.active_task_scope.clone(),
                state
                    .task_patch_attempts
                    .iter()
                    .filter(|(_, attempt)| {
                        state
                            .active_task_scope
                            .as_ref()
                            .is_some_and(|scope| scope.same_execution(&attempt.scope))
                    })
                    .map(|(call, attempt)| (call.clone(), attempt.clone()))
                    .collect::<Vec<_>>(),
            )
        };
        let Some((root_task_id, contract)) = completion_state else {
            return Ok(());
        };
        let retained_call_ids = task_checks
            .iter()
            .map(|snapshot| snapshot.attempt.call_id.as_str())
            .collect::<HashSet<_>>();
        if referenced_task_checks
            .iter()
            .any(|call_id| !retained_call_ids.contains(call_id.as_str()))
        {
            self.mark_completion_paused_in_memory().await;
            return Err("completion snapshot references a missing task-check receipt".to_string());
        }
        if task_checks.len() > MAX_COMPLETION_TASK_CHECKS {
            self.mark_completion_paused_in_memory().await;
            return Err("completion snapshot exceeds the task-check bound".to_string());
        }
        let snapshot = Snapshot {
            version: if contract
                .units
                .iter()
                .any(|unit| unit.review_state.is_some())
            {
                2
            } else {
                1
            },
            thread_id: self.thread_id().to_string(),
            root_task_id,
            state: contract,
            resume_generation,
            task_checks,
            active_scope,
            patch_attempts,
            uncollected_children,
            maintenance_repair_receipt: None,
        };
        let value = serde_json::to_value(&snapshot).map_err(|error| error.to_string())?;
        if serde_json::to_vec(&value)
            .map_err(|error| error.to_string())?
            .len()
            > MAX_COMPLETION_SNAPSHOT_BYTES
        {
            self.mark_completion_paused_in_memory().await;
            return Err("completion snapshot exceeds its size bound".to_string());
        }
        let typed_review_snapshot = snapshot.version == 2;
        let snapshot_sha256 = maintenance_snapshot_digest(&value)?;
        let item = ResponseItemEnvelope {
            item: Box::new(AutonomousCompletionSnapshot).into_boxed_response_item(),
            metadata: Some(CodexHarnessMetadata {
                rencrow_completion: Some(value),
                ..Default::default()
            }),
        };
        let persist_result = match self.live_thread() {
            Some(live_thread) => live_thread
                .append_items(&[RolloutItem::ResponseItem(item.clone())])
                .await
                .map(|()| true)
                .map_err(|error| error.to_string()),
            None if typed_review_snapshot => Err(
                "typed review snapshot cannot be durably recorded without a live thread store"
                    .into(),
            ),
            None => Ok(false),
        };
        let durable_snapshot_written = match persist_result {
            Ok(written) => written,
            Err(error) => {
                let mut state = self.state.lock().await;
                state.rencrow_completion_paused = true;
                if let Some((_, contract)) = state.rencrow_completion_state.as_mut() {
                    contract.paused = true;
                }
                return Err(format!(
                    "completion snapshot persistence failed; autonomous work paused: {error}"
                ));
            }
        };
        self.state
            .lock()
            .await
            .history
            .record_annotated_items(&[item], TruncationPolicy::Tokens(1024));
        if !durable_snapshot_written {
            return Ok(());
        }
        let review_attempts = snapshot
            .state
            .units
            .iter()
            .filter_map(|unit| {
                let attempt = unit.review_state.as_ref()?.active_attempt.as_ref()?;
                let attempt_sha256 = serde_json::to_value(attempt)
                    .ok()
                    .and_then(|value| maintenance_snapshot_digest(&value).ok())?;
                let terminal_source_sha256 = attempt
                    .terminal_source
                    .as_ref()
                    .and_then(|source| serde_json::to_value(source).ok())
                    .and_then(|source| maintenance_snapshot_digest(&source).ok());
                Some((
                    unit.id.clone(),
                    crate::state::PersistedReviewAttemptIdentity {
                        attempt_id: attempt.attempt_id.clone(),
                        reviewer_thread_id: attempt.reviewer_thread_id.clone(),
                        reviewer_turn_id: attempt.reviewer_turn_id.clone(),
                        target_task_id: attempt.target_task_id.clone(),
                        target_task_revision: attempt.target_task_revision,
                        target_artifact_ref: attempt.target_artifact_ref.clone(),
                        status: attempt.status,
                        attempt_sha256,
                        terminal_source_sha256,
                    },
                ))
            })
            .collect();
        let legacy_review_bindings = snapshot
            .state
            .units
            .iter()
            .filter_map(|unit| {
                persisted_legacy_review_binding(snapshot.state.contract_revision, unit)
            })
            .collect();
        self.state
            .lock()
            .await
            .rencrow_completion_last_persisted_snapshot =
            Some(crate::state::CompletionSnapshotReceipt {
                version: snapshot.version,
                sha256: snapshot_sha256,
                root_task_id: snapshot.root_task_id,
                review_attempts,
                legacy_review_bindings,
            });
        Ok(())
    }

    pub(crate) async fn restore_autonomous_completion_snapshot(&self, items: &[RolloutItem]) {
        let mut restored: Option<(String, CompletionState)> = None;
        let mut restored_checks = None;
        let mut restored_scope = None;
        let mut restored_patches = Vec::new();
        let mut restored_children = Vec::new();
        let mut restored_revision = None;
        let mut restored_generation = None;
        let mut restored_snapshot_version = None;
        let mut restored_paused = false;
        let mut fail_closed = false;
        let mut response_items = Vec::new();
        let mut accepted_receipts = None;
        let authorized_legacy_scope_mismatch_hashes = items
            .iter()
            .filter_map(|item| {
                let RolloutItem::ResponseItem(envelope) = item else {
                    return None;
                };
                let metadata = envelope.metadata.as_ref()?;
                let value = metadata.rencrow_completion.as_ref()?;
                let is_host = matches!(&envelope.item, ResponseItem::Message {
                    role, internal_chat_message_metadata_passthrough: Some(kind), ..
                } if role == "developer" && kind.content_item_kinds.as_ref().is_some_and(|kinds| {
                    kinds.len() == 1 && kinds[0].0 == "rencrow.completion_snapshot"
                })) && !metadata.client_authored;
                if !is_host {
                    return None;
                }
                let snapshot = serde_json::from_value::<Snapshot>(value.clone()).ok()?;
                let receipt = snapshot.maintenance_repair_receipt.as_ref()?;
                validate_repair_receipt(receipt, &snapshot, &self.thread_id().to_string()).ok()?;
                Some(receipt.legacy_scope_mismatch_snapshot_sha256.clone())
            })
            .flatten()
            .collect::<HashSet<_>>();
        let declared_shutdown_paused_hashes = items
            .iter()
            .filter_map(|item| {
                let RolloutItem::ResponseItem(envelope) = item else {
                    return None;
                };
                let metadata = envelope.metadata.as_ref()?;
                let value = metadata.rencrow_completion.as_ref()?;
                let is_host = matches!(&envelope.item, ResponseItem::Message {
                    role, internal_chat_message_metadata_passthrough: Some(kind), ..
                } if role == "developer" && kind.content_item_kinds.as_ref().is_some_and(|kinds| {
                    kinds.len() == 1 && kinds[0].0 == "rencrow.completion_snapshot"
                })) && !metadata.client_authored;
                if !is_host {
                    return None;
                }
                let snapshot = serde_json::from_value::<Snapshot>(value.clone()).ok()?;
                let receipt = snapshot.maintenance_repair_receipt?;
                Some(receipt.shutdown_paused_snapshot_sha256)
            })
            .collect::<HashSet<_>>();
        let mut observed_legacy_scope_mismatch_hashes = HashSet::new();
        let mut last_valid_snapshot_sha256 = None;
        let mut host_snapshot_values = BTreeMap::<String, Value>::new();
        let mut shutdown_pair_seen = false;
        let mut interrupted_after_shutdown_pair = false;
        for item in items {
            // Validate each snapshot against receipts already committed at that position,
            // never against a future revision which may legitimately supersede its evidence.
            let checkpoint_changed = match item {
                RolloutItem::ResponseItem(envelope) => {
                    response_items.push(envelope.clone());
                    envelope
                        .metadata
                        .as_ref()
                        .is_some_and(|metadata| metadata.rencrow_compaction.is_some())
                }
                RolloutItem::Compacted(compacted) => {
                    if let Some(replacement) = &compacted.replacement_history {
                        response_items = replacement.clone();
                    }
                    true
                }
                _ => false,
            };
            if checkpoint_changed {
                match crate::compact::latest_task_state_in_history(
                    &response_items,
                    &self.thread_id().to_string(),
                ) {
                    Ok(task_state) => accepted_receipts = task_state,
                    Err(_) => fail_closed = true,
                }
            }
            match item {
                RolloutItem::ResponseItem(envelope) => {
                    let Some(metadata) = envelope.metadata.as_ref() else {
                        continue;
                    };
                    let Some(value) = metadata.rencrow_completion.as_ref() else {
                        continue;
                    };
                    // Model/user text, even with matching markers, is not a host snapshot.
                    let is_host = matches!(&envelope.item, ResponseItem::Message {
                        role, internal_chat_message_metadata_passthrough: Some(kind), ..
                    } if role == "developer" && kind.content_item_kinds.as_ref().is_some_and(|kinds| {
                        kinds.len() == 1 && kinds[0].0 == "rencrow.completion_snapshot"
                    })) && !metadata.client_authored;
                    if !is_host {
                        continue;
                    }
                    let parsed = serde_json::from_value::<Snapshot>(value.clone());
                    let Ok(snapshot) = parsed else {
                        fail_closed = true;
                        continue;
                    };
                    let snapshot_sha256 =
                        codex_history::compaction_candidate::digest(value).unwrap_or_default();
                    host_snapshot_values.insert(snapshot_sha256.clone(), value.clone());
                    if declared_shutdown_paused_hashes.contains(&snapshot_sha256) {
                        shutdown_pair_seen = true;
                    }
                    let has_review_state = snapshot
                        .state
                        .units
                        .iter()
                        .any(|unit| unit.review_state.is_some());
                    if !matches!(snapshot.version, 1 | 2)
                        || (snapshot.version == 1 && has_review_state)
                        || (snapshot.version == 2 && !has_review_state)
                        || !review_snapshot_references_resolve(
                            &snapshot,
                            &snapshot_sha256,
                            &host_snapshot_values,
                        )
                        || snapshot.thread_id != self.thread_id().to_string()
                        || snapshot.root_task_id.trim().is_empty()
                        || snapshot.root_task_id.len() > 4096
                        || snapshot.state.contract_revision == 0
                        || snapshot.task_checks.len() > MAX_COMPLETION_TASK_CHECKS
                        || snapshot.uncollected_children.len() > 256
                        || snapshot
                            .uncollected_children
                            .iter()
                            .any(|id| codex_protocol::ThreadId::try_from(id.as_str()).is_err())
                        || !serde_json::to_vec(&snapshot)
                            .is_ok_and(|bytes| bytes.len() <= MAX_COMPLETION_SNAPSHOT_BYTES)
                    {
                        fail_closed = true;
                        continue;
                    }
                    if snapshot.active_scope.as_ref().is_some_and(|scope| {
                        scope.task_id.trim().is_empty()
                            || scope.task_revision == 0
                            || scope.begin_call_id.trim().is_empty()
                    }) || snapshot.patch_attempts.len() > 256
                        || snapshot.patch_attempts.iter().any(|(call, attempt)| {
                            call.trim().is_empty()
                                || !snapshot
                                    .active_scope
                                    .as_ref()
                                    .is_some_and(|scope| scope.same_execution(&attempt.scope))
                                || attempt.paths.len() > 16
                        })
                    {
                        fail_closed = true;
                        continue;
                    }
                    let previous_snapshot = restored.as_ref().map(|(root, previous)| {
                        (
                            root.as_str(),
                            restored_revision.unwrap_or_default(),
                            restored_generation.unwrap_or_default(),
                            previous.paused,
                        )
                    });
                    if validate_snapshot_sequence_transition(previous_snapshot, &snapshot).is_err()
                    {
                        fail_closed = true;
                        continue;
                    }
                    if accepted_receipts.as_ref().is_some_and(|task_state| {
                        task_state.boundary.root_task_id != snapshot.root_task_id
                    }) {
                        fail_closed = true;
                        continue;
                    }
                    let receipts = accepted_receipts
                        .as_ref()
                        .map(|state| &state.completed)
                        .cloned()
                        .unwrap_or_default();
                    let scope_root_mismatch = snapshot
                        .active_scope
                        .as_ref()
                        .is_some_and(|scope| scope.root_task_id != snapshot.root_task_id);
                    if scope_root_mismatch {
                        observed_legacy_scope_mismatch_hashes.insert(snapshot_sha256.clone());
                        if authorized_legacy_scope_mismatch_hashes.contains(&snapshot_sha256)
                            && validate_snapshot_payload(
                                &snapshot,
                                &self.thread_id().to_string(),
                                &receipts,
                            )
                            .is_ok()
                        {
                            continue;
                        }
                        fail_closed = true;
                        continue;
                    }
                    if validate_snapshot_payload(
                        &snapshot,
                        &self.thread_id().to_string(),
                        &receipts,
                    )
                    .is_err()
                    {
                        fail_closed = true;
                        continue;
                    }
                    if let Some(receipt) = &snapshot.maintenance_repair_receipt
                        && (validate_repair_receipt(
                            receipt,
                            &snapshot,
                            &self.thread_id().to_string(),
                        )
                        .is_err()
                            || last_valid_snapshot_sha256.as_deref()
                                != Some(receipt.snapshot_before_sha256.as_str())
                            || receipt.resume_generation_before
                                != restored_generation.unwrap_or_default()
                            || receipt
                                .legacy_scope_mismatch_snapshot_sha256
                                .iter()
                                .cloned()
                                .collect::<HashSet<_>>()
                                != observed_legacy_scope_mismatch_hashes
                            || interrupted_after_shutdown_pair
                            || !maintenance_receipt_matches_history(
                                receipt,
                                value,
                                &host_snapshot_values,
                            ))
                    {
                        fail_closed = true;
                        continue;
                    }
                    if restored_generation != Some(snapshot.resume_generation) {
                        restored_paused = snapshot.state.paused;
                    } else {
                        restored_paused |= snapshot.state.paused;
                    }
                    restored_revision = Some(snapshot.state.contract_revision);
                    restored_generation = Some(snapshot.resume_generation);
                    restored_snapshot_version = Some(snapshot.version);
                    restored_checks = Some(snapshot.task_checks);
                    restored_scope = snapshot.active_scope;
                    restored_patches = snapshot.patch_attempts;
                    restored_children = snapshot.uncollected_children;
                    restored = Some((snapshot.root_task_id, snapshot.state));
                    last_valid_snapshot_sha256 = Some(snapshot_sha256);
                    if snapshot.maintenance_repair_receipt.is_some() {
                        shutdown_pair_seen = false;
                    }
                }
                RolloutItem::EventMsg(EventMsg::TurnAborted(event))
                    if event.reason == TurnAbortReason::Interrupted =>
                {
                    if shutdown_pair_seen {
                        interrupted_after_shutdown_pair = true;
                    }
                    if restored.is_some() {
                        restored_paused = true;
                    }
                }
                _ => {}
            }
        }
        if let Some((root, mut contract)) = restored {
            let paused = {
                let mut current = self.state.lock().await;
                let current_generation = current.rencrow_completion_resume_generation;
                let snapshot_generation = restored_generation.unwrap_or_default();
                if current_generation > snapshot_generation {
                    if fail_closed {
                        current.rencrow_completion_paused = true;
                        if let Some((_, existing)) = current.rencrow_completion_state.as_mut() {
                            existing.paused = true;
                        }
                    }
                    return;
                }
                let current_paused = current.rencrow_completion_paused
                    || current
                        .rencrow_completion_state
                        .as_ref()
                        .is_some_and(|(_, existing)| existing.paused);
                let current_is_newer_or_same =
                    restored_generation.is_some_and(|generation| current_generation >= generation);
                fail_closed || restored_paused || (current_is_newer_or_same && current_paused)
            };
            contract.paused = paused;
            {
                let mut state = self.state.lock().await;
                if let Some(task_checks) = restored_checks
                    && state
                        .task_check_ledger
                        .restore_snapshot(task_checks)
                        .is_err()
                {
                    let _ = state.task_check_ledger.restore_snapshot(Vec::new());
                    contract.paused = true;
                }
                state.rencrow_completion_paused = contract.paused;
                state.rencrow_completion_resume_generation =
                    restored_generation.unwrap_or_default();
                state.rencrow_completion_uncollected_children =
                    restored_children.into_iter().collect();
                // The old cursor belongs to the original history, not its compacted projection.
                // active_task_scope() rebinds it to the retained host begin call after hydration.
                if let Some(scope) = restored_scope.as_mut() {
                    scope.start_history_cursor = 0;
                }
                state.rencrow_scope_needs_rebase = restored_scope.is_some();
                state.active_task_scope = restored_scope;
                state.task_patch_attempts = restored_patches
                    .into_iter()
                    .map(|(call, mut attempt)| {
                        attempt.scope.start_history_cursor = 0;
                        (call, attempt)
                    })
                    .collect();
                state.rencrow_completion_last_persisted_snapshot =
                    last_valid_snapshot_sha256.clone().map(|sha256| {
                        let review_attempts = contract
                            .units
                            .iter()
                            .filter_map(|unit| {
                                let attempt =
                                    unit.review_state.as_ref()?.active_attempt.as_ref()?;
                                let attempt_sha256 = serde_json::to_value(attempt)
                                    .ok()
                                    .and_then(|value| maintenance_snapshot_digest(&value).ok())?;
                                let terminal_source_sha256 = attempt
                                    .terminal_source
                                    .as_ref()
                                    .and_then(|source| serde_json::to_value(source).ok())
                                    .and_then(|source| maintenance_snapshot_digest(&source).ok());
                                Some((
                                    unit.id.clone(),
                                    crate::state::PersistedReviewAttemptIdentity {
                                        attempt_id: attempt.attempt_id.clone(),
                                        reviewer_thread_id: attempt.reviewer_thread_id.clone(),
                                        reviewer_turn_id: attempt.reviewer_turn_id.clone(),
                                        target_task_id: attempt.target_task_id.clone(),
                                        target_task_revision: attempt.target_task_revision,
                                        target_artifact_ref: attempt.target_artifact_ref.clone(),
                                        status: attempt.status,
                                        attempt_sha256,
                                        terminal_source_sha256,
                                    },
                                ))
                            })
                            .collect();
                        let version = restored_snapshot_version.unwrap_or_default();
                        let legacy_review_bindings = contract
                            .units
                            .iter()
                            .filter_map(|unit| {
                                persisted_legacy_review_binding(contract.contract_revision, unit)
                            })
                            .collect();
                        crate::state::CompletionSnapshotReceipt {
                            version,
                            sha256,
                            root_task_id: root.clone(),
                            review_attempts,
                            legacy_review_bindings,
                        }
                    });
                state.rencrow_completion_state = Some((root, contract));
            }
            self.revalidate_collected_completion_reviews().await;
        } else if fail_closed {
            self.state.lock().await.rencrow_completion_paused = true;
        }
    }

    async fn mark_completion_paused_in_memory(&self) {
        let mut state = self.state.lock().await;
        state.rencrow_completion_paused = true;
        if let Some((_, contract)) = state.rencrow_completion_state.as_mut() {
            contract.paused = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::tests::make_session_and_context;
    use crate::session::turn_context::TurnContext;
    use crate::state::TaskKind;
    use crate::tasks::SessionTask;
    use crate::tasks::SessionTaskResult;
    use codex_history::task_context::AcceptanceCondition;
    use codex_history::task_context::AcceptanceStatus;
    use codex_history::task_context::CompletionUnit;
    use codex_history::task_context::UnitStatus;
    use pretty_assertions::assert_eq;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    struct PendingBoundaryTask;

    impl SessionTask for PendingBoundaryTask {
        fn kind(&self) -> TaskKind {
            TaskKind::Regular
        }

        fn span_name(&self) -> &'static str {
            "completion_persistence_boundary_fixture"
        }

        async fn run(
            self: Arc<Self>,
            _session: Arc<Session>,
            _ctx: Arc<TurnContext>,
            _input: Vec<crate::session::input_queue::TurnInput>,
            cancellation_token: CancellationToken,
        ) -> SessionTaskResult {
            cancellation_token.cancelled().await;
            Err(codex_protocol::error::CodexErr::TurnAborted)
        }
    }

    async fn commit_accepted_task_boundary(
        session: &Arc<Session>,
        turn: Arc<TurnContext>,
        artifact_ref: &str,
    ) -> String {
        let turn_id = turn.sub_id.clone();
        session
            .spawn_task(turn, Vec::new(), PendingBoundaryTask)
            .await;
        let history = session.clone_history().await;
        let history_hash =
            codex_history::compaction_candidate::history_digest(history.annotated_items())
                .expect("hash source history for accepted boundary");
        let removed_refs = history
            .annotated_items()
            .iter()
            .enumerate()
            .map(
                |(index, item)| codex_history::task_checkpoint_metadata::TaskBoundarySourceRef {
                    id: format!("item:{index}"),
                    hash: codex_history::compaction_candidate::digest(&(
                        &item.item,
                        &item.metadata,
                    ))
                    .expect("hash source history item"),
                },
            )
            .collect();
        let boundary = codex_history::task_context::BoundaryCandidate {
            version: 1,
            mode: codex_history::task_context::BoundaryMode::Task,
            thread_id: session.thread_id().to_string(),
            root_task_id: "root".into(),
            closed_task_id: "task-a".into(),
            revision: 2,
            closed_task_revision: 1,
            goal: "review the accepted artifact".into(),
            tasks: vec![codex_history::task_context::TaskState {
                id: "task-a".into(),
                purpose: "prepare the accepted review artifact".into(),
                status: codex_history::task_context::TaskStatus::Completed,
                depends_on: Vec::new(),
            }],
            current_task_id: None,
            artifact_ref: artifact_ref.into(),
            verification_ref: artifact_ref.into(),
            decisions: Vec::new(),
            constraints: Vec::new(),
            completion_state: None,
        };
        let candidate_hash = codex_history::archive_reference::content_sha256(
            &serde_json::to_string(&boundary).expect("serialize accepted boundary"),
        );
        let task_state = codex_history::task_context::StoredTaskState {
            boundary,
            completed: std::collections::BTreeMap::from([(
                "task-a".into(),
                codex_history::task_context::AcceptedTaskReceipt {
                    revision: 1,
                    result: "accepted review artifact is ready".into(),
                    artifact_ref: artifact_ref.into(),
                    verification_ref: artifact_ref.into(),
                },
            )]),
            parent_review_ref: "review:task-a".into(),
            previous_revision: None,
            previous_state_hash: None,
            candidate_hash,
            boundary_nonce: "accepted-review-boundary".into(),
        };
        task_state
            .validate(&session.thread_id().to_string())
            .expect("accepted task boundary validates");

        let summary_text = "Accepted artifact and verification receipt.";
        let summary = codex_history::ResponseItemEnvelope::new(
            codex_protocol::models::ResponseItem::Message {
                id: None,
                role: "user".into(),
                content: vec![codex_protocol::models::ContentItem::InputText {
                    text: summary_text.into(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: Some(
                    codex_protocol::models::InternalChatMessageMetadataPassthrough {
                        content_item_kinds: Some(vec![codex_protocol::models::ContentItemKind(
                            "compaction.summary".into(),
                        )]),
                        ..Default::default()
                    },
                ),
            },
        );
        session
            .commit_task_boundary_checkpoint(
                history_hash.clone(),
                turn_id.clone(),
                vec![summary],
                summary_text.to_string(),
                task_state,
                codex_history::task_checkpoint_metadata::TaskBoundarySourceFence {
                    history_hash,
                    removed_refs,
                },
            )
            .await
            .expect("persist accepted task boundary through owner checkpoint");
        assert_eq!(
            session
                .latest_rencrow_task_state()
                .await
                .expect("read committed task state")
                .expect("committed task state exists")
                .completed["task-a"]
                .artifact_ref,
            artifact_ref
        );
        turn_id
    }

    fn open_state(paused: bool) -> CompletionState {
        CompletionState {
            contract_revision: 1,
            units_completed_since_review: 0,
            plan_review_at: None,
            plan_reviews: Vec::new(),
            paused,
            consumed_host_events: Vec::new(),
            conditions: vec![AcceptanceCondition {
                id: "AC-1".into(),
                revision: 1,
                result: "Result".into(),
                scope: "Scope".into(),
                verification: "Check".into(),
                acceptance_criteria: "Pass".into(),
                status: AcceptanceStatus::Open,
                evidence: Vec::new(),
                unmet_reason: None,
            }],
            units: vec![CompletionUnit {
                id: "unit-1".into(),
                purpose: "Complete the result".into(),
                status: UnitStatus::Ready,
                kind: codex_history::task_context::CompletionUnitKind::Execution,
                condition_refs: vec!["AC-1".into()],
                depends_on: Vec::new(),
                evidence: Vec::new(),
                task_refs: Vec::new(),
                review_target: None,
                review_evidence: None,
                review_launch: None,
                review_decision_reason: None,
                review_state: None,
                review_revalidation_of: None,
            }],
            current_unit: Some("unit-1".into()),
            failed_approaches: Vec::new(),
            blocker: None,
            next_action: Some("Start unit-1".into()),
            product_progress_at: None,
            runtime_recovery_at: None,
            activity_at: None,
        }
    }

    fn snapshot_item(value: serde_json::Value) -> RolloutItem {
        let item = Box::new(AutonomousCompletionSnapshot).into_boxed_response_item();
        RolloutItem::ResponseItem(ResponseItemEnvelope {
            item,
            metadata: Some(CodexHarnessMetadata {
                rencrow_completion: Some(value),
                ..Default::default()
            }),
        })
    }

    async fn record_test_artifact_receipt(
        session: &Session,
        workspace: &std::path::Path,
        task_id: &str,
        task_revision: u64,
        call_id: &str,
    ) -> codex_history::task_context::CompletionEvidenceRef {
        let artifact_paths = vec![std::path::PathBuf::from("artifact.txt")];
        let before = crate::task_check::hash_workspace_artifacts(workspace, &artifact_paths)
            .expect("hash accepted artifact");
        let request = crate::task_check::TaskEvidenceRequest {
            task_id: task_id.to_string(),
            task_revision,
            artifact_paths,
            check_kind: crate::task_check::TaskCheckKind::Verification,
        };
        session
            .begin_task_check(
                call_id.to_string(),
                request,
                workspace.to_path_buf(),
                before.clone(),
            )
            .await;
        let receipt = session
            .finish_task_check(call_id, 0, true, before)
            .await
            .expect("successful accepted-task receipt");
        let artifacts = receipt
            .artifact_after
            .iter()
            .map(|stamp| {
                serde_json::json!({
                    "path": stamp.path.to_string_lossy(),
                    "sha256": stamp.sha256,
                })
            })
            .collect::<Vec<_>>();
        let fingerprint = codex_history::compaction_candidate::digest(&serde_json::json!({
            "task_id": receipt.task_id,
            "task_revision": receipt.task_revision,
            "workspace_root": receipt.workspace_root.to_string_lossy(),
            "artifacts": artifacts,
        }))
        .expect("canonical accepted-task artifact fingerprint");
        codex_history::task_context::CompletionEvidenceRef {
            task_id: task_id.to_string(),
            task_revision,
            artifact_ref: format!("task-check:{call_id}:{fingerprint}"),
            verification_ref: format!("task-check:{call_id}:{fingerprint}"),
        }
    }

    fn maintenance_repair_history(
        thread_id: &str,
        include_interrupted: bool,
        fake_shutdown_pair: bool,
    ) -> Vec<RolloutItem> {
        let shutdown_before = serde_json::json!({
            "version": 1, "thread_id": thread_id, "root_task_id": "root",
            "resume_generation": 0, "state": open_state(false),
            "active_scope": {"root_task_id":"old-root", "task_id":"unit-1", "task_revision":1,
                "begin_call_id":"begin-1", "start_history_cursor":0}
        });
        let shutdown_before_hash = maintenance_snapshot_digest(&shutdown_before).unwrap();
        let paused = serde_json::json!({
            "version": 1, "thread_id": thread_id, "root_task_id": "root",
            "resume_generation": 0, "state": open_state(true),
            "active_scope": {"root_task_id":"old-root", "task_id":"unit-1", "task_revision":1,
                "begin_call_id":"begin-1", "start_history_cursor":0}
        });
        let paused_hash = maintenance_snapshot_digest(&paused).unwrap();
        let current_paused = serde_json::json!({
            "version": 1, "thread_id": thread_id, "root_task_id": "root",
            "resume_generation": 0, "state": open_state(true), "active_scope": null
        });
        let current_paused_hash = maintenance_snapshot_digest(&current_paused).unwrap();
        let receipt = MaintenanceRepairReceipt {
            version: 1,
            thread_id: thread_id.into(),
            root_task_id: "root".into(),
            request_sha256: "1111111111111111111111111111111111111111111111111111111111111111"
                .into(),
            snapshot_before_sha256: current_paused_hash,
            shutdown_before_snapshot_sha256: if fake_shutdown_pair {
                "2222222222222222222222222222222222222222222222222222222222222222".into()
            } else {
                shutdown_before_hash.clone()
            },
            shutdown_paused_snapshot_sha256: paused_hash.clone(),
            legacy_scope_mismatch_snapshot_sha256: vec![shutdown_before_hash, paused_hash],
            evidence: [
                "deployment_result",
                "deployment_preflight",
                "maintenance_ticket",
                "pre_deploy_snapshot",
                "pre_deploy_facts",
                "deployer_source",
            ]
            .into_iter()
            .enumerate()
            .map(|(index, kind)| MaintenanceRepairEvidenceDigest {
                kind: kind.into(),
                path: format!("evidence-{index}.json"),
                sha256: format!("{:064x}", index + 1),
            })
            .collect(),
            resume_generation_before: 0,
            resume_generation_after: 1,
        };
        let repair = build_maintenance_repair_snapshot(&current_paused, receipt).unwrap();
        let mut items = vec![snapshot_item(shutdown_before), snapshot_item(paused)];
        if include_interrupted {
            items.push(RolloutItem::EventMsg(EventMsg::TurnAborted(
                codex_protocol::protocol::TurnAbortedEvent {
                    turn_id: Some("turn-between-pause-and-repair".into()),
                    reason: TurnAbortReason::Interrupted,
                    started_at: None,
                    completed_at: None,
                    duration_ms: None,
                },
            )));
        }
        items.push(snapshot_item(current_paused));
        items.push(snapshot_item(repair));
        items
    }

    #[test]
    fn maintenance_repair_inspection_rejects_same_generation_pause_clear() {
        let thread_id = "01a1c0b9-582b-71c0-a6d6-552f2d581a8e";
        let paused = serde_json::json!({
            "version": 1,
            "thread_id": thread_id,
            "root_task_id": "root",
            "resume_generation": 4,
            "state": open_state(true),
        });
        let unpaused_same_generation = serde_json::json!({
            "version": 1,
            "thread_id": thread_id,
            "root_task_id": "root",
            "resume_generation": 4,
            "state": open_state(false),
        });
        let target = maintenance_snapshot_digest(&unpaused_same_generation).unwrap();
        let items = vec![
            snapshot_item(paused),
            snapshot_item(unpaused_same_generation),
        ];

        let error = inspect_maintenance_repair_snapshot(&items, &target, thread_id, "root", false)
            .expect_err("same-generation resume must remain fail-closed");
        assert!(error.contains("restore monotonicity"));
    }

    #[test]
    fn maintenance_repair_inspection_rejects_checkpoint_failure_after_target_snapshot() {
        let thread_id = "01a1c0b9-582b-71c0-a6d6-552f2d581a8e";
        let target = serde_json::json!({
            "version": 1,
            "thread_id": thread_id,
            "root_task_id": "root",
            "resume_generation": 0,
            "state": open_state(false),
        });
        let target_hash = maintenance_snapshot_digest(&target).unwrap();
        let mut invalid_checkpoint = snapshot_item(serde_json::json!({
            "version": 1,
            "thread_id": thread_id,
            "root_task_id": "root",
            "resume_generation": 1,
            "state": open_state(false),
        }));
        let RolloutItem::ResponseItem(envelope) = &mut invalid_checkpoint else {
            panic!("repair item must be a response item");
        };
        envelope
            .metadata
            .as_mut()
            .expect("completion metadata")
            .rencrow_compaction = Some(serde_json::json!({"version":99}));

        let items = vec![snapshot_item(target), invalid_checkpoint];
        let error =
            inspect_maintenance_repair_snapshot(&items, &target_hash, thread_id, "root", false)
                .expect_err("invalid later compaction checkpoint must fail closed");
        assert!(error.contains("unsupported RenCrow compaction checkpoint version"));
    }

    #[test]
    fn maintenance_repair_inspection_ignores_an_uncommitted_task_checkpoint() {
        let thread_id = "01a1c0b9-582b-71c0-a6d6-552f2d581a8e";
        let mut checkpoint = ResponseItemEnvelope::new(ResponseItem::Message {
            id: None,
            role: "user".into(),
            content: vec![codex_protocol::models::ContentItem::InputText {
                text: "pending checkpoint".into(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: Some(
                codex_protocol::models::InternalChatMessageMetadataPassthrough {
                    content_item_kinds: Some(vec![codex_protocol::models::ContentItemKind(
                        "compaction.summary".into(),
                    )]),
                    ..Default::default()
                },
            ),
        });
        checkpoint
            .metadata
            .get_or_insert_default()
            .rencrow_compaction = Some(serde_json::json!({
            "version": 3,
            "mode": "capacity",
            "summary_hash": "a".repeat(64),
            "task_state": {"invalid": true},
            "transaction_following_items": 0
        }));
        let compacted = RolloutItem::Compacted(codex_history::CompactedItem {
            message: "pending checkpoint".into(),
            replacement_history: Some(vec![checkpoint]),
            retained_context: None,
            guardian_history: None,
            mcp_resource_origins: None,
            window_number: Some(1),
            first_window_id: Some("window-1".into()),
            previous_window_id: None,
            window_id: Some("window-1".into()),
            compaction_response_id: Some("pending-response".into()),
            latest_token_usage_record: None,
        });
        let target = serde_json::json!({
            "version": 1,
            "thread_id": thread_id,
            "root_task_id": "root",
            "resume_generation": 0,
            "state": open_state(false),
        });
        let target_hash = maintenance_snapshot_digest(&target).unwrap();
        let items = vec![compacted, snapshot_item(target)];

        assert!(
            inspect_maintenance_repair_snapshot(&items, &target_hash, thread_id, "root", false)
                .is_ok()
        );
    }

    #[tokio::test]
    async fn patch_proof_survives_snapshot_when_history_cursors_differ() {
        let (session, _) = make_session_and_context().await;
        let scope = crate::state::ActiveTaskScope {
            root_task_id: "root".into(),
            task_id: "unit-a".into(),
            task_revision: 1,
            begin_call_id: "begin-a".into(),
            start_history_cursor: 3,
        };
        let mut patch_scope = scope.clone();
        patch_scope.start_history_cursor = 4;
        {
            let mut state = session.state.lock().await;
            state.rencrow_completion_state = Some(("root".into(), open_state(false)));
            state.active_task_scope = Some(scope.clone());
            state.task_patch_attempts.insert(
                "patch-a".into(),
                crate::state::TaskPatchAttempt {
                    scope: patch_scope,
                    local_environment: true,
                    paths: vec![std::path::PathBuf::from("/tmp/verified-artifact.txt")],
                    completed_successfully: true,
                },
            );
        }
        session
            .persist_autonomous_completion_snapshot()
            .await
            .unwrap();
        let snapshot = {
            let state = session.state.lock().await;
            state
                .history
                .annotated_items()
                .iter()
                .find_map(|item| {
                    item.metadata
                        .as_ref()
                        .and_then(|metadata| metadata.rencrow_completion.clone())
                })
                .expect("captured host snapshot")
        };
        assert_eq!(snapshot["patch_attempts"].as_array().unwrap().len(), 1);
        {
            let mut state = session.state.lock().await;
            state.active_task_scope = None;
            state.task_patch_attempts.clear();
            state.history.replace(vec![serde_json::from_value(serde_json::json!({
                "type":"function_call", "call_id":"begin-a", "name":"task_context_boundary",
                "arguments":serde_json::json!({"action":"begin", "root_task_id":"root", "task_id":"unit-a", "task_revision":1}).to_string()
            })).unwrap()]);
        }
        session
            .restore_autonomous_completion_snapshot(&[snapshot_item(snapshot)])
            .await;
        let restored = session
            .active_task_scope()
            .await
            .expect("scope rebound to retained begin");
        assert!(restored.same_execution(&scope));
        assert!(!session.autonomous_completion_paused().await);
        let state = session.state.lock().await;
        let patch = state
            .task_patch_attempts
            .get("patch-a")
            .expect("host patch proof retained");
        assert!(restored.same_execution(&patch.scope));
        assert_eq!(
            restored.start_history_cursor,
            patch.scope.start_history_cursor
        );
        assert!(patch.completed_successfully);
    }

    #[tokio::test]
    async fn cold_restore_preserves_review_unit_target_without_fabricated_evidence() {
        let (session, _) = make_session_and_context().await;
        let mut contract = open_state(false);
        contract.units[0].kind = codex_history::task_context::CompletionUnitKind::Review;
        contract.units[0].review_target =
            Some(codex_history::task_context::CompletionReviewTarget {
                task_id: "task-a".into(),
                task_revision: 2,
            });
        contract.units[0].status = UnitStatus::Running;
        contract.units[0].review_launch =
            Some(codex_history::task_context::CompletionReviewLaunchState {
                spawn_call_id: "review-launch-call".into(),
                status: codex_history::task_context::CompletionReviewLaunchStatus::Unknown,
                deadline_unix_ms: Some(1_798_000_000_000),
                reason: Some("spawn outcome could not be confirmed".into()),
            });
        {
            session.state.lock().await.rencrow_completion_state = Some(("root".into(), contract));
        }
        session
            .persist_autonomous_completion_snapshot()
            .await
            .expect("persist typed review target");
        let snapshot = {
            let mut state = session.state.lock().await;
            let snapshot = state
                .history
                .annotated_items()
                .iter()
                .find_map(|item| {
                    item.metadata
                        .as_ref()
                        .and_then(|metadata| metadata.rencrow_completion.clone())
                })
                .expect("captured host snapshot");
            state.rencrow_completion_state = None;
            snapshot
        };

        session
            .restore_autonomous_completion_snapshot(&[snapshot_item(snapshot)])
            .await;
        let restored = session.rencrow_completion_state("root").await.unwrap();
        assert_eq!(
            restored.units[0].kind,
            codex_history::task_context::CompletionUnitKind::Review
        );
        assert_eq!(
            restored.units[0].review_target,
            Some(codex_history::task_context::CompletionReviewTarget {
                task_id: "task-a".into(),
                task_revision: 2,
            })
        );
        assert!(restored.units[0].review_evidence.is_none());
        assert_eq!(
            restored.units[0].review_launch,
            Some(codex_history::task_context::CompletionReviewLaunchState {
                spawn_call_id: "review-launch-call".into(),
                status: codex_history::task_context::CompletionReviewLaunchStatus::Unknown,
                deadline_unix_ms: Some(1_798_000_000_000),
                reason: Some("spawn outcome could not be confirmed".into()),
            })
        );
        assert!(!session.autonomous_completion_paused().await);
    }

    #[tokio::test]
    async fn cold_restore_preserves_bound_review_deadline_without_extension() {
        let (mut session, turn) = make_session_and_context().await;
        let workspace = tempfile::tempdir().expect("workspace");
        std::fs::write(workspace.path().join("artifact.txt"), "accepted artifact\n").unwrap();
        let accepted_target =
            record_test_artifact_receipt(&session, workspace.path(), "task-a", 1, "host-review")
                .await;
        let rollout_path = crate::session::tests::attach_thread_persistence(&mut session).await;
        let session = Arc::new(session);
        let turn_id =
            commit_accepted_task_boundary(&session, Arc::new(turn), &accepted_target.artifact_ref)
                .await;
        let deadline_unix_ms = 1_798_000_000_000;
        let mut contract = open_state(false);
        let unit = &mut contract.units[0];
        unit.id = "review-1".into();
        unit.purpose = "Review task-a".into();
        unit.status = UnitStatus::Running;
        unit.kind = codex_history::task_context::CompletionUnitKind::Review;
        unit.review_target = Some(codex_history::task_context::CompletionReviewTarget {
            task_id: "task-a".into(),
            task_revision: 1,
        });
        unit.review_evidence = Some(codex_history::task_context::CompletionReviewEvidence {
            reviewer_thread_id: "01a1c606-48b8-7cf3-bb78-77b47f6b8c2f".into(),
            reviewer_turn_id: "reviewer-turn-1".into(),
            reviewer_role: "analyst".into(),
            spawn_call_id: "host-review-call".into(),
            deadline_unix_ms: Some(deadline_unix_ms),
            deadline_escalation: None,
            deadline_safety_fingerprint: None,
            wait_result_ref: None,
            terminal_status: None,
            wait_call_id: None,
            target_task_id: "task-a".into(),
            target_task_revision: 1,
            target_artifact_ref: accepted_target.artifact_ref.clone(),
        });
        contract.current_unit = Some("review-1".into());
        contract.next_action = Some("Wait for the bound reviewer to finish task-a@1.".into());
        session.state.lock().await.rencrow_completion_state = Some(("root".into(), contract));
        session
            .persist_autonomous_completion_snapshot()
            .await
            .expect("persist host-bound reviewer deadline");
        session
            .flush_rollout()
            .await
            .expect("flush deadline snapshot");
        let codex_history::InitialHistory::Resumed(persisted) =
            crate::rollout::recorder::RolloutRecorder::get_rollout_history(&rollout_path)
                .await
                .expect("read persisted snapshot")
        else {
            panic!("expected persisted rollout");
        };
        let snapshot = persisted
            .history
            .iter()
            .find_map(|item| match item {
                RolloutItem::ResponseItem(envelope) => envelope
                    .metadata
                    .as_ref()
                    .and_then(|metadata| metadata.rencrow_completion.clone()),
                _ => None,
            })
            .expect("captured host snapshot");
        assert_eq!(
            snapshot["state"]["units"][0]["review_evidence"]["deadline_unix_ms"],
            deadline_unix_ms
        );
        let persisted_response_items = persisted
            .history
            .iter()
            .find_map(|item| match item {
                RolloutItem::Compacted(compacted) => compacted.replacement_history.clone(),
                _ => None,
            })
            .expect("persisted compaction checkpoint carries replacement history");
        let accepted_task_state = crate::compact::latest_task_state_in_history(
            &persisted_response_items,
            &session.thread_id().to_string(),
        )
        .expect("parse persisted accepted task boundary")
        .expect("accepted task boundary must be in the disk rollout");
        let parsed_snapshot: Snapshot =
            serde_json::from_value(snapshot.clone()).expect("parse persisted deadline snapshot");
        let state_validation = codex_history::task_context::validate_completion_state(
            Some(&parsed_snapshot.state),
            &accepted_task_state.completed,
        );
        assert!(
            state_validation.is_ok(),
            "persisted completion state should validate against accepted task receipt: {state_validation:?}"
        );
        let payload_validation = validate_snapshot_payload(
            &parsed_snapshot,
            &session.thread_id().to_string(),
            &accepted_task_state.completed,
        );
        assert!(
            payload_validation.is_ok(),
            "persisted deadline snapshot should validate against accepted task receipt: {payload_validation:?}; accepted task target={:?}",
            accepted_task_state.completed.get("task-a")
        );
        session.state.lock().await.rencrow_completion_state = None;

        session
            .restore_autonomous_completion_snapshot(persisted.history.as_ref())
            .await;
        let restored = session.rencrow_completion_state("root").await.unwrap();
        assert_eq!(
            restored.units[0]
                .review_evidence
                .as_ref()
                .and_then(|review| review.deadline_unix_ms),
            Some(deadline_unix_ms),
            "restore keeps the original absolute deadline"
        );
        session
            .abort_turn_if_active(
                &turn_id,
                codex_protocol::protocol::TurnAbortReason::Interrupted,
            )
            .await;
    }

    #[tokio::test]
    async fn cold_restore_preserves_a_persisted_stop() {
        let (session, _) = make_session_and_context().await;
        let snapshot = serde_json::json!({
            "version": 1,
            "thread_id": session.thread_id().to_string(),
            "root_task_id": "root",
            "state": open_state(true),
        });
        session
            .restore_autonomous_completion_snapshot(&[snapshot_item(snapshot)])
            .await;

        assert!(session.autonomous_completion_paused().await);
        assert_eq!(
            session.rencrow_completion_state("root").await,
            Some(open_state(true))
        );
    }

    #[tokio::test]
    async fn invalid_first_snapshot_fails_closed_without_prior_valid_state() {
        let (session, _) = make_session_and_context().await;
        let snapshot = serde_json::json!({
            "version": 99,
            "thread_id": session.thread_id().to_string(),
            "root_task_id": "root",
            "state": open_state(false),
        });
        session
            .restore_autonomous_completion_snapshot(&[snapshot_item(snapshot)])
            .await;

        assert!(session.autonomous_completion_paused().await);
        assert_eq!(session.rencrow_completion_state("root").await, None);
    }

    #[tokio::test]
    async fn older_or_invalid_later_snapshots_cannot_rewind_state_or_clear_stop() {
        let (session, _) = make_session_and_context().await;
        let mut newer = open_state(false);
        newer.contract_revision = 2;
        let snapshots = [
            snapshot_item(serde_json::json!({
                "version": 1,
                "thread_id": session.thread_id().to_string(),
                "root_task_id": "root",
                "state": newer,
            })),
            snapshot_item(serde_json::json!({
                "version": 1,
                "thread_id": session.thread_id().to_string(),
                "root_task_id": "root",
                "state": open_state(false),
            })),
        ];
        session
            .restore_autonomous_completion_snapshot(&snapshots)
            .await;

        assert!(session.autonomous_completion_paused().await);
        let restored = session.rencrow_completion_state("root").await.unwrap();
        assert_eq!(restored.contract_revision, 2);
        assert!(restored.paused);
    }

    #[tokio::test]
    async fn cold_restore_keeps_an_unfinished_task_check_pending() {
        let (session, _) = make_session_and_context().await;
        let snapshot = serde_json::json!({
            "version": 1,
            "thread_id": session.thread_id().to_string(),
            "root_task_id": "root",
            "state": open_state(false),
            "task_checks": [{
                "task_id": "task-a",
                "task_revision": 1,
                "attempt": {"call_id": "call-a", "status": "Pending"}
            }]
        });
        session
            .restore_autonomous_completion_snapshot(&[snapshot_item(snapshot)])
            .await;

        let state = session.state.lock().await;
        let attempt = state
            .task_check_ledger
            .latest_for_task("task-a", 1)
            .expect("pending host check is restored");
        assert!(matches!(
            &attempt.status,
            crate::task_check::TaskCheckAttemptStatus::Pending
        ));
    }

    #[tokio::test]
    async fn higher_host_resume_generation_can_clear_a_persisted_stop() {
        let (session, _) = make_session_and_context().await;
        let thread_id = session.thread_id().to_string();
        let paused = serde_json::json!({
            "version": 1,
            "thread_id": thread_id,
            "root_task_id": "root",
            "resume_generation": 0,
            "state": open_state(true),
        });
        let resumed = serde_json::json!({
            "version": 1,
            "thread_id": session.thread_id().to_string(),
            "root_task_id": "root",
            "resume_generation": 1,
            "state": open_state(false),
        });
        session
            .restore_autonomous_completion_snapshot(&[
                snapshot_item(paused),
                snapshot_item(resumed),
            ])
            .await;

        assert!(!session.autonomous_completion_paused().await);
        assert!(
            session
                .rencrow_completion_state("root")
                .await
                .is_some_and(|state| !state.paused)
        );
        assert_eq!(
            session
                .state
                .lock()
                .await
                .rencrow_completion_resume_generation,
            1
        );
    }

    #[tokio::test]
    async fn host_repair_receipt_clears_only_declared_legacy_scope_mismatch() {
        let (session, _) = make_session_and_context().await;
        session
            .restore_autonomous_completion_snapshot(&maintenance_repair_history(
                &session.thread_id().to_string(),
                false,
                false,
            ))
            .await;

        assert!(
            !session.autonomous_completion_paused().await,
            "a valid host repair receipt should exempt only its exact legacy scope mismatch"
        );
        assert_eq!(
            session
                .state
                .lock()
                .await
                .rencrow_completion_resume_generation,
            1
        );
    }

    #[tokio::test]
    async fn repair_receipt_cannot_clear_with_unresolved_shutdown_pair_hashes() {
        let (session, _) = make_session_and_context().await;
        session
            .restore_autonomous_completion_snapshot(&maintenance_repair_history(
                &session.thread_id().to_string(),
                false,
                true,
            ))
            .await;
        assert!(session.autonomous_completion_paused().await);
    }

    #[tokio::test]
    async fn repair_receipt_cannot_clear_after_interrupted_turn() {
        let (session, _) = make_session_and_context().await;
        session
            .restore_autonomous_completion_snapshot(&maintenance_repair_history(
                &session.thread_id().to_string(),
                true,
                false,
            ))
            .await;
        assert!(session.autonomous_completion_paused().await);
    }

    #[tokio::test]
    async fn same_generation_snapshot_cannot_clear_a_persisted_stop() {
        let (session, _) = make_session_and_context().await;
        let thread_id = session.thread_id().to_string();
        let paused = serde_json::json!({
            "version": 1,
            "thread_id": thread_id,
            "root_task_id": "root",
            "resume_generation": 3,
            "state": open_state(true),
        });
        let invalid_resume = serde_json::json!({
            "version": 1,
            "thread_id": session.thread_id().to_string(),
            "root_task_id": "root",
            "resume_generation": 3,
            "state": open_state(false),
        });
        session
            .restore_autonomous_completion_snapshot(&[
                snapshot_item(paused),
                snapshot_item(invalid_resume),
            ])
            .await;

        assert!(session.autonomous_completion_paused().await);
        assert!(
            session
                .rencrow_completion_state("root")
                .await
                .is_some_and(|state| state.paused)
        );
        assert_eq!(
            session
                .state
                .lock()
                .await
                .rencrow_completion_resume_generation,
            3
        );
    }
}
