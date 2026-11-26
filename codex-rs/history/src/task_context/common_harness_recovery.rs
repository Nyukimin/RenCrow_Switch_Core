//! Fenced owner handoff and durable operation recovery for common-harness state.

use super::*;

pub(super) fn validate_initial_claim(
    state: &CommonHarnessStateV1,
    intent: &OperationIntentV1,
    claim: &OperationClaimV1,
) -> Result<(), TransitionErrorV1> {
    let intent_digest = intent
        .content_digest()
        .map_err(|_| TransitionErrorV1::InvalidOperation("intent_digest"))?;
    if claim.intent_digest != intent_digest {
        return Err(TransitionErrorV1::InvalidOperation("intent_digest"));
    }
    if claim.operation_id != intent.operation_id
        || claim.owner_epoch != state.run_state.owner_epoch
        || claim.expected_state_version != state.run_state.state_version
        || claim.kind != OperationClaimKindV1::Execution
        || claim.previous_claim_id.is_some()
        || claim.supersedes_execution_claim_id.is_some()
        || claim.native_executor_dead_proof_ref.is_some()
        || validate_text(&claim.claim_id).is_err()
    {
        return Err(TransitionErrorV1::InvalidOperation("initial_claim_binding"));
    }
    validate_execution_claim_deadline(intent, claim)
}

pub(super) fn validate_new_receipt(
    state: &CommonHarnessStateV1,
    record: &OperationRecordRefV1,
    receipt: &OperationReceiptV1,
) -> Result<(), TransitionErrorV1> {
    let claim = record
        .claims
        .last()
        .filter(|claim| claim.kind == OperationClaimKindV1::Execution)
        .ok_or(TransitionErrorV1::InvalidOperation(
            "execution_claim_missing",
        ))?;
    if claim.owner_epoch != state.run_state.owner_epoch {
        return Err(TransitionErrorV1::InvalidOperation("stale_execution_claim"));
    }
    validate_receipt_for_claim(state, &record.intent, receipt, claim)
}

fn validate_receipt_for_claim(
    state: &CommonHarnessStateV1,
    intent: &OperationIntentV1,
    receipt: &OperationReceiptV1,
    claim: &OperationClaimV1,
) -> Result<(), TransitionErrorV1> {
    if claim.kind != OperationClaimKindV1::Execution
        || receipt.execution_id != claim.claim_id
        || receipt.owner_epoch != claim.owner_epoch
    {
        return Err(TransitionErrorV1::InvalidOperation(
            "receipt_execution_claim",
        ));
    }
    let mut receipt_intent = intent.clone();
    receipt_intent.owner_epoch = claim.owner_epoch;
    state.validate_receipt(&receipt_intent, receipt)
}

pub(super) fn validate_operation_record(
    state: &CommonHarnessStateV1,
    record: &OperationRecordRefV1,
) -> Result<(), TransitionErrorV1> {
    if record.claims.is_empty() || record.claims.len() > MAX_COMMON_HARNESS_RECORDS {
        return Err(TransitionErrorV1::InvalidOperation(
            "claim_chain_missing_or_oversized",
        ));
    }
    let intent_digest = record
        .intent
        .content_digest()
        .map_err(|_| TransitionErrorV1::InvalidOperation("intent_digest"))?;
    let first = &record.claims[0];
    validate_execution_claim_deadline(&record.intent, first)?;
    let mut claim_ids = std::collections::BTreeSet::new();
    let mut latest_execution_claim_id = None;
    for (index, claim) in record.claims.iter().enumerate() {
        if !claim_ids.insert(claim.claim_id.as_str())
            || claim.operation_id != record.intent.operation_id
            || claim.intent_digest != intent_digest
            || claim.owner_epoch == 0
            || claim.owner_epoch > state.run_state.owner_epoch
            || claim.expected_state_version == 0
            || claim.expected_state_version >= state.run_state.state_version
            || claim.claimed_at_ms <= 0
            || claim.deadline_at_ms != first.deadline_at_ms
            || validate_text(&claim.claim_id).is_err()
        {
            return Err(TransitionErrorV1::InvalidOperation("claim_chain_binding"));
        }
        if index == 0 {
            if claim.previous_claim_id.is_some()
                || claim.owner_epoch != record.intent.owner_epoch
                || claim.expected_state_version != record.intent.expected_state_version
                || claim.kind != OperationClaimKindV1::Execution
                || claim.supersedes_execution_claim_id.is_some()
                || claim.native_executor_dead_proof_ref.is_some()
            {
                return Err(TransitionErrorV1::InvalidOperation("initial_claim_binding"));
            }
            latest_execution_claim_id = Some(claim.claim_id.as_str());
            continue;
        }
        let previous = &record.claims[index - 1];
        if claim.previous_claim_id.as_deref() != Some(previous.claim_id.as_str()) {
            return Err(TransitionErrorV1::InvalidOperation("claim_chain_link"));
        }
        match claim.kind {
            OperationClaimKindV1::Finalizer => {
                if claim.supersedes_execution_claim_id.is_some()
                    || claim.native_executor_dead_proof_ref.is_some()
                    || record.intent.operation_kind == OperationKindV1::ExternalAction
                    || !record
                        .receipt
                        .as_ref()
                        .is_some_and(|receipt| receipt.outcome == OperationOutcomeV1::Unknown)
                {
                    return Err(TransitionErrorV1::InvalidOperation(
                        "finalizer_claim_fields",
                    ));
                }
            }
            OperationClaimKindV1::Execution => {
                if record.intent.operation_kind == OperationKindV1::ExternalAction {
                    return Err(TransitionErrorV1::InvalidOperation(
                        "external_reexecution_unsupported",
                    ));
                }
                if claim.supersedes_execution_claim_id.as_deref() != latest_execution_claim_id
                    || claim
                        .native_executor_dead_proof_ref
                        .as_ref()
                        .is_none_or(|proof| validate_text(proof).is_err())
                {
                    return Err(TransitionErrorV1::InvalidOperation(
                        "executor_not_proven_dead",
                    ));
                }
                if claim.claimed_at_ms >= claim.deadline_at_ms {
                    return Err(TransitionErrorV1::InvalidOperation(
                        "operation_deadline_expired",
                    ));
                }
                latest_execution_claim_id = Some(claim.claim_id.as_str());
            }
        }
    }

    if record.finalizations.len() > 1 {
        return Err(TransitionErrorV1::OperationAlreadyFinalized);
    }
    let execution_claims: std::collections::BTreeMap<_, _> = record
        .claims
        .iter()
        .filter(|claim| claim.kind == OperationClaimKindV1::Execution)
        .map(|claim| (claim.claim_id.as_str(), claim))
        .collect();
    if let Some(raw_receipt) = record.receipt.as_ref() {
        let claim = execution_claims
            .get(raw_receipt.execution_id.as_str())
            .ok_or(TransitionErrorV1::InvalidOperation(
                "raw_receipt_execution_claim",
            ))?;
        validate_receipt_for_claim(state, &record.intent, raw_receipt, claim)?;
    }
    for finalization in &record.finalizations {
        let Some(raw_receipt) = record.receipt.as_ref() else {
            return Err(TransitionErrorV1::InvalidOperation("raw_receipt_missing"));
        };
        let Some(finalizer_claim) = record
            .claims
            .iter()
            .find(|claim| claim.claim_id == finalization.finalizer_claim_id)
        else {
            return Err(TransitionErrorV1::InvalidOperation(
                "finalizer_claim_missing",
            ));
        };
        let Some(execution_claim) = execution_claims.get(finalization.execution_claim_id.as_str())
        else {
            return Err(TransitionErrorV1::InvalidOperation(
                "execution_claim_missing",
            ));
        };
        if raw_receipt.outcome != OperationOutcomeV1::Unknown
            || record.intent.operation_kind == OperationKindV1::ExternalAction
            || finalizer_claim.owner_epoch > state.run_state.owner_epoch
            || record.claims.last().map(|claim| claim.claim_id.as_str())
                != Some(finalizer_claim.claim_id.as_str())
            || (finalizer_claim.kind != OperationClaimKindV1::Finalizer
                && !(finalizer_claim.kind == OperationClaimKindV1::Execution
                    && finalizer_claim.claim_id == finalization.execution_claim_id))
            || execution_claim.kind != OperationClaimKindV1::Execution
            || finalization.operation_id != record.intent.operation_id
            || finalization.receipt.operation_id != record.intent.operation_id
            || finalization.receipt.owner_epoch != execution_claim.owner_epoch
            || finalization.receipt.execution_id != execution_claim.claim_id
            || finalization.receipt.actual_before_version != raw_receipt.actual_before_version
            || finalization.receipt.outcome == OperationOutcomeV1::Unknown
            || finalization
                .reconciliation_ref
                .as_ref()
                .is_none_or(|reference| {
                    validate_text(reference).is_err()
                        || finalization.receipt.reconciliation_ref.as_ref() != Some(reference)
                })
            || finalization.source_observation_refs.is_empty()
            || finalization
                .source_observation_refs
                .iter()
                .any(|reference| validate_text(reference).is_err())
        {
            return Err(TransitionErrorV1::InvalidOperation("finalization_binding"));
        }
        validate_receipt_for_claim(
            state,
            &record.intent,
            &finalization.receipt,
            execution_claim,
        )?;
    }
    Ok(())
}

pub(super) fn validate_owner_takeovers(
    state: &CommonHarnessStateV1,
) -> Result<(), TransitionErrorV1> {
    if state.owner_takeovers.len() > MAX_COMMON_HARNESS_RECORDS {
        return Err(TransitionErrorV1::InvalidState("owner_takeover_limit"));
    }
    let Some(last) = state.owner_takeovers.last() else {
        return Ok(());
    };
    for (index, takeover) in state.owner_takeovers.iter().enumerate() {
        if validate_text(&takeover.previous_owner_id).is_err()
            || validate_text(&takeover.new_owner_id).is_err()
            || validate_text(&takeover.takeover_proof_ref).is_err()
            || takeover.previous_owner_id == takeover.new_owner_id
            || takeover.previous_owner_epoch == 0
            || takeover.new_owner_epoch != takeover.previous_owner_epoch.saturating_add(1)
            || takeover.expected_state_version == 0
            || takeover.expected_state_version >= state.run_state.state_version
            || (index > 0
                && (state.owner_takeovers[index - 1].new_owner_id != takeover.previous_owner_id
                    || state.owner_takeovers[index - 1].new_owner_epoch
                        != takeover.previous_owner_epoch
                    || takeover.expected_state_version
                        <= state.owner_takeovers[index - 1].expected_state_version))
        {
            return Err(TransitionErrorV1::InvalidState("owner_takeover_binding"));
        }
    }
    if last.new_owner_id != state.run_state.owner_id
        || last.new_owner_epoch != state.run_state.owner_epoch
    {
        return Err(TransitionErrorV1::InvalidState(
            "owner_takeover_current_owner",
        ));
    }
    Ok(())
}

pub(super) fn apply_recovery_event(
    state: &CommonHarnessStateV1,
    fence: StateFenceV1,
    event: HostEventV1,
) -> Result<CommonHarnessStateV1, TransitionErrorV1> {
    if fence.request_revision != state.request.request_revision
        || fence.expected_state_version != state.run_state.state_version
        || fence.owner_epoch != state.run_state.owner_epoch
    {
        return Err(TransitionErrorV1::FenceMismatch);
    }
    let mut next = state.clone();
    match event {
        HostEventV1::TakeOwnership {
            new_owner_id,
            new_owner_epoch,
            takeover_proof_ref,
        } => {
            validate_text(&new_owner_id)
                .map_err(|_| TransitionErrorV1::InvalidState("new_owner_id"))?;
            validate_text(&takeover_proof_ref)
                .map_err(|_| TransitionErrorV1::InvalidState("takeover_proof_ref"))?;
            if new_owner_id == state.run_state.owner_id
                || new_owner_epoch != state.run_state.owner_epoch.saturating_add(1)
                || state.owner_takeovers.len() >= MAX_COMMON_HARNESS_RECORDS
            {
                return Err(TransitionErrorV1::InvalidState("owner_takeover_epoch"));
            }
            next.owner_takeovers.push(OwnerTakeoverRecordV1 {
                previous_owner_id: state.run_state.owner_id.clone(),
                new_owner_id: new_owner_id.clone(),
                previous_owner_epoch: state.run_state.owner_epoch,
                new_owner_epoch,
                expected_state_version: state.run_state.state_version,
                takeover_proof_ref,
            });
            next.run_state.owner_id = new_owner_id;
            next.run_state.owner_epoch = new_owner_epoch;
        }
        HostEventV1::ObserveMissingOperation {
            operation_id,
            observation_ref,
        } => {
            validate_text(&operation_id)
                .map_err(|_| TransitionErrorV1::InvalidOperation("operation_id"))?;
            validate_text(&observation_ref)
                .map_err(|_| TransitionErrorV1::InvalidOperation("observation_ref"))?;
            let Some(index) = state
                .operation_refs
                .iter()
                .position(|record| record.intent.operation_id == operation_id)
            else {
                return Err(TransitionErrorV1::OperationNotFound);
            };
            let record = &state.operation_refs[index];
            let execution_claim = record
                .claims
                .iter()
                .rev()
                .find(|claim| claim.kind == OperationClaimKindV1::Execution)
                .ok_or(TransitionErrorV1::InvalidOperation(
                    "execution_claim_missing",
                ))?;
            let receipt = OperationReceiptV1 {
                operation_id: record.intent.operation_id.clone(),
                task_id: record.intent.task_id.clone(),
                request_revision: record.intent.request_revision,
                attempt_no: record.intent.attempt_no,
                owner_epoch: execution_claim.owner_epoch,
                execution_id: execution_claim.claim_id.clone(),
                outcome: OperationOutcomeV1::Unknown,
                actual_before_version: record.intent.base_target_version.clone(),
                actual_after_version: None,
                exit_status: None,
                result_ref: Some(observation_ref),
                external_receipt_ref: None,
                reconciliation_ref: None,
            };
            if let Some(existing) = record.receipt.as_ref() {
                return if existing == &receipt {
                    Ok(state.clone())
                } else {
                    Err(TransitionErrorV1::OperationAlreadyFinalized)
                };
            }
            if !matches!(
                state.run_state.status,
                TaskRunStatusV1::Running | TaskRunStatusV1::Verifying | TaskRunStatusV1::Blocked
            ) || state.run_state.cancel_requested
                || !state
                    .run_state
                    .unsettled_operation_refs
                    .contains(&operation_id)
                || !record.finalizations.is_empty()
            {
                return Err(TransitionErrorV1::InvalidOperation(
                    "missing_operation_observation_binding",
                ));
            }
            validate_receipt_for_claim(state, &record.intent, &receipt, execution_claim)?;
            next.operation_refs[index].receipt = Some(receipt);
            next.run_state.status = TaskRunStatusV1::Blocked;
            next.run_state.phase = TaskRunPhaseV1::Reconcile;
            next.run_state.reason_code = Some("operation_outcome_unknown".into());
            next.run_state.resume_condition = Some(operation_id);
            next.run_state.next_operation = "reconcile_operation".into();
        }
        HostEventV1::BindWorkWindow { window } => {
            if let Some(existing) = state.run_state.work_window.as_ref() {
                return if existing == &window {
                    Ok(state.clone())
                } else {
                    Err(TransitionErrorV1::InvalidState("work_window_already_bound"))
                };
            }
            if !matches!(
                state.run_state.status,
                TaskRunStatusV1::Running | TaskRunStatusV1::Verifying
            ) || state.run_state.attempt_no == 0
                || state.run_state.cancel_requested
            {
                return Err(TransitionErrorV1::InvalidState("work_window_status"));
            }
            validate_work_window(&window, state.run_state.attempt_no)?;
            next.run_state.work_window = Some(window);
        }
        HostEventV1::ClaimExistingOperation {
            operation_id,
            claim,
        } => {
            let Some(index) = state
                .operation_refs
                .iter()
                .position(|record| record.intent.operation_id == operation_id)
            else {
                return Err(TransitionErrorV1::OperationNotFound);
            };
            let record = &state.operation_refs[index];
            if claim.operation_id != operation_id
                || claim.owner_epoch != state.run_state.owner_epoch
                || claim.expected_state_version != state.run_state.state_version
                || claim.previous_claim_id.as_deref()
                    != record
                        .claims
                        .last()
                        .map(|previous| previous.claim_id.as_str())
                || validate_text(&claim.claim_id).is_err()
                || claim.claimed_at_ms <= 0
            {
                return Err(TransitionErrorV1::InvalidOperation("claim_binding"));
            }
            if claim.intent_digest
                != record
                    .intent
                    .content_digest()
                    .map_err(|_| TransitionErrorV1::InvalidOperation("intent_digest"))?
            {
                return Err(TransitionErrorV1::InvalidOperation("intent_digest"));
            }
            if claim.deadline_at_ms != record.claims[0].deadline_at_ms {
                return Err(TransitionErrorV1::InvalidOperation("deadline_changed"));
            }
            if record
                .claims
                .iter()
                .any(|previous| previous.claim_id == claim.claim_id)
            {
                return Err(TransitionErrorV1::InvalidOperation("duplicate_claim_id"));
            }
            match claim.kind {
                OperationClaimKindV1::Finalizer => {
                    if claim.supersedes_execution_claim_id.is_some()
                        || claim.native_executor_dead_proof_ref.is_some()
                        || record.intent.operation_kind == OperationKindV1::ExternalAction
                    {
                        return Err(TransitionErrorV1::InvalidOperation(
                            "external_reconciliation_unsupported",
                        ));
                    }
                    if !record
                        .receipt
                        .as_ref()
                        .is_some_and(|receipt| receipt.outcome == OperationOutcomeV1::Unknown)
                    {
                        return Err(TransitionErrorV1::InvalidOperation(
                            "finalizer_requires_unknown_receipt",
                        ));
                    }
                }
                OperationClaimKindV1::Execution => {
                    if record.intent.operation_kind == OperationKindV1::ExternalAction {
                        return Err(TransitionErrorV1::InvalidOperation(
                            "external_reexecution_unsupported",
                        ));
                    }
                    if record
                        .receipt
                        .as_ref()
                        .is_some_and(|receipt| receipt.outcome != OperationOutcomeV1::Unknown)
                        || claim.claimed_at_ms >= claim.deadline_at_ms
                    {
                        return Err(TransitionErrorV1::InvalidOperation(
                            "operation_deadline_or_receipt",
                        ));
                    }
                    let last_execution = record
                        .claims
                        .iter()
                        .rev()
                        .find(|previous| previous.kind == OperationClaimKindV1::Execution)
                        .ok_or(TransitionErrorV1::InvalidOperation(
                            "execution_claim_missing",
                        ))?;
                    if claim.supersedes_execution_claim_id.as_deref()
                        != Some(last_execution.claim_id.as_str())
                        || claim
                            .native_executor_dead_proof_ref
                            .as_ref()
                            .is_none_or(|proof| validate_text(proof).is_err())
                    {
                        return Err(TransitionErrorV1::InvalidOperation(
                            "executor_not_proven_dead",
                        ));
                    }
                }
            }
            let is_execution_claim = claim.kind == OperationClaimKindV1::Execution;
            next.operation_refs[index].claims.push(claim);
            if next.operation_refs[index].claims.len() > MAX_COMMON_HARNESS_RECORDS {
                return Err(TransitionErrorV1::InvalidOperation("claim_limit"));
            }
            if is_execution_claim {
                next.run_state.status = TaskRunStatusV1::Running;
                next.run_state.phase = TaskRunPhaseV1::Reconcile;
            }
            next.run_state.next_operation = operation_id;
        }
        HostEventV1::FinalizeReconciledOperation { finalization } => {
            let Some(index) = state
                .operation_refs
                .iter()
                .position(|record| record.intent.operation_id == finalization.operation_id)
            else {
                return Err(TransitionErrorV1::OperationNotFound);
            };
            let record = &state.operation_refs[index];
            if let Some(existing) = record.finalizations.first() {
                return if existing == &finalization {
                    Ok(state.clone())
                } else {
                    Err(TransitionErrorV1::OperationAlreadyFinalized)
                };
            }
            validate_operation_record(state, record)?;
            if record.intent.operation_kind == OperationKindV1::ExternalAction {
                return Err(TransitionErrorV1::InvalidOperation(
                    "external_reconciliation_unsupported",
                ));
            }
            let raw_receipt = record
                .receipt
                .as_ref()
                .ok_or(TransitionErrorV1::InvalidOperation("raw_receipt_missing"))?;
            if raw_receipt.outcome != OperationOutcomeV1::Unknown {
                return Err(TransitionErrorV1::OperationAlreadyFinalized);
            }
            let finalizer_claim = record
                .claims
                .iter()
                .find(|claim| claim.claim_id == finalization.finalizer_claim_id)
                .ok_or(TransitionErrorV1::InvalidOperation(
                    "finalizer_claim_missing",
                ))?;
            let execution_claim = record
                .claims
                .iter()
                .find(|claim| claim.claim_id == finalization.execution_claim_id)
                .ok_or(TransitionErrorV1::InvalidOperation(
                    "execution_claim_missing",
                ))?;
            let is_current_execution_finalizer = finalizer_claim.kind
                == OperationClaimKindV1::Execution
                && finalizer_claim.claim_id == execution_claim.claim_id;
            if (finalizer_claim.kind != OperationClaimKindV1::Finalizer
                && !is_current_execution_finalizer)
                || finalizer_claim.owner_epoch != state.run_state.owner_epoch
                || record.claims.last().map(|claim| claim.claim_id.as_str())
                    != Some(finalizer_claim.claim_id.as_str())
                || execution_claim.kind != OperationClaimKindV1::Execution
                || finalization.receipt.owner_epoch != execution_claim.owner_epoch
                || finalization.receipt.execution_id != execution_claim.claim_id
                || finalization.receipt.actual_before_version != raw_receipt.actual_before_version
            {
                return Err(TransitionErrorV1::InvalidOperation(
                    "finalization_claim_binding",
                ));
            }
            validate_finalization_refs(&finalization)?;
            validate_receipt_for_claim(
                state,
                &record.intent,
                &finalization.receipt,
                execution_claim,
            )?;
            if finalization.receipt.outcome == OperationOutcomeV1::Unknown {
                return Err(TransitionErrorV1::UnknownOperationOutcome);
            }
            next.operation_refs[index].finalizations.push(finalization);
            next.run_state
                .unsettled_operation_refs
                .retain(|operation| operation != &record.intent.operation_id);
            if next.run_state.cancel_requested && next.run_state.unsettled_operation_refs.is_empty()
            {
                next.run_state.status = TaskRunStatusV1::Cancelled;
                next.run_state.next_operation = "return_cancelled".into();
            } else if next.run_state.unsettled_operation_refs.is_empty() {
                next.run_state.status =
                    if record.intent.operation_kind == OperationKindV1::RunVerifier {
                        TaskRunStatusV1::Verifying
                    } else {
                        TaskRunStatusV1::Running
                    };
                next.run_state.phase = if next.run_state.status == TaskRunStatusV1::Verifying {
                    TaskRunPhaseV1::Verify
                } else {
                    TaskRunPhaseV1::Apply
                };
                next.run_state.reason_code = None;
                next.run_state.resume_condition = None;
            }
            next.run_state.next_operation = "reconciled_operation".into();
        }
        HostEventV1::ResumeReconciled {
            operation_id,
            target_version,
        } => {
            validate_text(&operation_id)
                .map_err(|_| TransitionErrorV1::InvalidOperation("operation_id"))?;
            validate_text(&target_version)
                .map_err(|_| TransitionErrorV1::InvalidState("target_version"))?;
            if state.run_state.status != TaskRunStatusV1::Running
                || state.run_state.phase != TaskRunPhaseV1::Apply
                || state.run_state.reason_code.is_some()
                || state.run_state.resume_condition.is_some()
                || state.run_state.cancel_requested
                || !state.run_state.unsettled_operation_refs.is_empty()
            {
                return Err(TransitionErrorV1::InvalidState("reconciled_resume_binding"));
            }
            let record = state
                .operation_refs
                .iter()
                .find(|record| record.intent.operation_id == operation_id)
                .ok_or(TransitionErrorV1::OperationNotFound)?;
            let raw_receipt = record
                .receipt
                .as_ref()
                .ok_or(TransitionErrorV1::InvalidOperation("raw_receipt_missing"))?;
            let finalization = record
                .finalizations
                .last()
                .ok_or(TransitionErrorV1::InvalidOperation("finalization_missing"))?;
            let effective_receipt =
                record
                    .effective_receipt()
                    .ok_or(TransitionErrorV1::InvalidOperation(
                        "effective_receipt_missing",
                    ))?;
            if record.intent.operation_kind != OperationKindV1::ApplyPatch
                || record.intent.base_target_version != state.run_state.target_version
                || raw_receipt.outcome != OperationOutcomeV1::Unknown
                || raw_receipt.actual_before_version != state.run_state.target_version
                || finalization.operation_id != operation_id
                || effective_receipt.outcome != OperationOutcomeV1::Succeeded
                || effective_receipt.actual_before_version != state.run_state.target_version
                || effective_receipt.actual_after_version.as_deref()
                    != Some(target_version.as_str())
                || target_version == state.run_state.target_version
            {
                return Err(TransitionErrorV1::InvalidOperation(
                    "reconciled_patch_binding",
                ));
            }
            for evidence in &mut next.evidence_refs {
                if evidence.lifecycle == EvidenceLifecycleV1::Current
                    && evidence.target_version == state.run_state.target_version
                {
                    evidence.lifecycle = EvidenceLifecycleV1::Stale;
                }
            }
            next.run_state.target_version = target_version;
            next.run_state.status = TaskRunStatusV1::Running;
            next.run_state.phase = TaskRunPhaseV1::Apply;
            next.run_state.reason_code = None;
            next.run_state.resume_condition = None;
            next.run_state.next_operation = "apply_after_reconcile".into();
            next.refresh_unmet_conditions();
        }
        HostEventV1::ResumeWaitingBound {
            question_ref,
            operation_digest,
            target_version,
            answer_ref,
            answer_digest,
        } => {
            let Some(waiting) = state.run_state.waiting.as_ref() else {
                return Err(TransitionErrorV1::InvalidState("no_waiting_binding"));
            };
            if state.run_state.status != TaskRunStatusV1::Waiting
                || waiting.question_ref != question_ref
                || waiting.operation_digest != operation_digest
                || waiting.target_version != target_version
                || state.run_state.target_version != target_version
            {
                return Err(TransitionErrorV1::InvalidState("waiting_binding_mismatch"));
            }
            validate_text(&answer_ref)
                .map_err(|_| TransitionErrorV1::InvalidState("answer_ref"))?;
            validate_text(&answer_digest)
                .map_err(|_| TransitionErrorV1::InvalidState("answer_digest"))?;
            next.run_state.status = match waiting.resume_phase {
                TaskRunPhaseV1::Verify | TaskRunPhaseV1::Review => TaskRunStatusV1::Verifying,
                TaskRunPhaseV1::Plan | TaskRunPhaseV1::Apply | TaskRunPhaseV1::Reconcile => {
                    TaskRunStatusV1::Running
                }
            };
            next.run_state.phase = waiting.resume_phase;
            next.run_state.waiting = None;
            next.run_state.reason_code = None;
            next.run_state.resume_condition = None;
            next.run_state.next_operation = answer_ref;
        }
        HostEventV1::EndAttempt { reason_code } => {
            validate_text(&reason_code)
                .map_err(|_| TransitionErrorV1::InvalidState("reason_code"))?;
            if state.run_state.status != TaskRunStatusV1::Verifying
                || state.run_state.attempt_no < state.request.max_attempts
                || !state.run_state.unsettled_operation_refs.is_empty()
            {
                return Err(TransitionErrorV1::InvalidState("end_attempt_state"));
            }
            let mut observed_failure = false;
            for condition in state
                .request
                .conditions
                .iter()
                .filter(|condition| condition.required)
            {
                let latest = state.evidence_refs.iter().rev().find(|evidence| {
                    evidence.condition_id == condition.condition_id
                        && evidence.condition_revision == condition.condition_revision
                        && evidence.condition_set_revision == state.request.condition_set_revision
                        && evidence.request_revision == state.request.request_revision
                        && evidence.target_version == state.run_state.target_version
                        && evidence.lifecycle == EvidenceLifecycleV1::Current
                });
                match latest.map(|evidence| evidence.verdict) {
                    Some(ConditionVerdictV1::Fail) => observed_failure = true,
                    Some(ConditionVerdictV1::Pass) => {}
                    Some(ConditionVerdictV1::Indeterminate | ConditionVerdictV1::Error) | None => {
                        return Err(TransitionErrorV1::InvalidState(
                            "required_condition_not_terminal",
                        ));
                    }
                }
            }
            if !observed_failure {
                return Err(TransitionErrorV1::InvalidState("no_current_failure"));
            }
            next.run_state.status = TaskRunStatusV1::Exhausted;
            next.run_state.reason_code = Some(reason_code);
            next.run_state.resume_condition = None;
            next.run_state.next_operation = "return_exhausted".into();
        }
        _ => return Err(TransitionErrorV1::RecoveryEventNotImplemented),
    }
    next.run_state.state_version = state.run_state.state_version + 1;
    next.validate(None)?;
    Ok(next)
}

fn validate_execution_claim_deadline(
    intent: &OperationIntentV1,
    claim: &OperationClaimV1,
) -> Result<(), TransitionErrorV1> {
    let timeout_ms = i64::try_from(intent.timeout_ms)
        .map_err(|_| TransitionErrorV1::InvalidOperation("claim_timeout"))?;
    let latest_deadline = claim
        .claimed_at_ms
        .checked_add(timeout_ms)
        .ok_or(TransitionErrorV1::InvalidOperation("claim_deadline"))?;
    if claim.claimed_at_ms <= 0
        || claim.deadline_at_ms <= claim.claimed_at_ms
        || claim.deadline_at_ms > latest_deadline
    {
        return Err(TransitionErrorV1::InvalidOperation("claim_deadline"));
    }
    Ok(())
}

fn validate_finalization_refs(
    finalization: &OperationFinalizationV1,
) -> Result<(), TransitionErrorV1> {
    if finalization.source_observation_refs.is_empty()
        || finalization.source_observation_refs.len() > 128
        || finalization
            .source_observation_refs
            .iter()
            .any(|reference| validate_text(reference).is_err())
        || finalization
            .reconciliation_ref
            .as_ref()
            .is_none_or(|reference| validate_text(reference).is_err())
        || finalization.receipt.reconciliation_ref.as_ref()
            != finalization.reconciliation_ref.as_ref()
    {
        return Err(TransitionErrorV1::InvalidOperation("finalization_refs"));
    }
    Ok(())
}
