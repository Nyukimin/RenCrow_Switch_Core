// Modified by RenCrow Switch Core, 2026-11-27: validate native recovery bindings without applying effects.
use crate::common_harness_artifacts::load_operation_artifact;
use crate::common_harness_artifacts::predict_operation_artifact_ref;
use crate::common_harness_native_patch::CommonHarnessPatchArtifactV1;
use crate::common_harness_native_patch::NativePatchEffectCompleteV1;
use crate::common_harness_native_patch::NativePatchExecutorDeadProofV1;
use codex_history::task_context::CommonHarnessStateV1;
use codex_history::task_context::OperationClaimKindV1;
use codex_history::task_context::OperationKindV1;
use codex_history::task_context::OperationOutcomeV1;
use codex_history::task_context::TaskRunPhaseV1;
use codex_history::task_context::TaskRunStatusV1;

/// Identify only the reducer state after FinalizeReconciledOperation and before
/// ResumeReconciled. Historical finalized operations remain in the ledger; their
/// base digest can become current again after a later patch cycle, so selection
/// also binds phase, next operation, latest same-attempt operation, raw/effective
/// receipts, and both claims. This intentionally does not infer pending status from
/// state-version arithmetic: harmless host events or repeated writer takeovers can
/// advance that version without consuming the pending resume.
pub(in crate::session) fn is_current_pending_resume(
    state: &CommonHarnessStateV1,
    record: &codex_history::task_context::OperationRecordRefV1,
) -> bool {
    if state.run_state.status != TaskRunStatusV1::Running
        || state.run_state.phase != TaskRunPhaseV1::Apply
        || state.run_state.cancel_requested
        || state.run_state.reason_code.is_some()
        || state.run_state.resume_condition.is_some()
        || !state.run_state.unsettled_operation_refs.is_empty()
        || state.run_state.next_operation != "reconciled_operation"
        || record.intent.operation_kind != OperationKindV1::ApplyPatch
        || record.intent.request_revision != state.request.request_revision
        || record.intent.attempt_no != state.run_state.attempt_no
        || record.intent.base_target_version != state.run_state.target_version
        || !record.receipt.as_ref().is_some_and(|receipt| {
            receipt.outcome == OperationOutcomeV1::Unknown
                && receipt.actual_before_version == state.run_state.target_version
        })
    {
        return false;
    }
    let Some(finalization) = record.finalizations.last() else {
        return false;
    };
    let Some(effective) = record.effective_receipt() else {
        return false;
    };
    if finalization.operation_id != record.intent.operation_id
        || finalization.receipt != *effective
        || effective.outcome != OperationOutcomeV1::Succeeded
        || effective.actual_before_version != state.run_state.target_version
        || effective
            .actual_after_version
            .as_deref()
            .is_none_or(|version| version == state.run_state.target_version)
    {
        return false;
    }
    let Some(finalizer_claim) = record.claims.iter().find(|claim| {
        claim.claim_id == finalization.finalizer_claim_id
            && claim.kind == OperationClaimKindV1::Finalizer
    }) else {
        return false;
    };
    let Some(execution_claim) = record.claims.iter().find(|claim| {
        claim.claim_id == finalization.execution_claim_id
            && claim.kind == OperationClaimKindV1::Execution
    }) else {
        return false;
    };
    if record.claims.last().map(|claim| claim.claim_id.as_str())
        != Some(finalizer_claim.claim_id.as_str())
        || finalization.receipt.execution_id != execution_claim.claim_id
        || finalization.receipt.owner_epoch != execution_claim.owner_epoch
        || execution_claim.operation_id != record.intent.operation_id
        || finalizer_claim.operation_id != record.intent.operation_id
        || finalizer_claim.deadline_at_ms != execution_claim.deadline_at_ms
        || !record
            .intent
            .content_digest()
            .is_ok_and(|digest| digest == finalizer_claim.intent_digest)
    {
        return false;
    }
    let latest_relevant_operation = state.operation_refs.iter().rev().find(|candidate| {
        candidate.intent.request_revision == state.request.request_revision
            && candidate.intent.attempt_no == state.run_state.attempt_no
    });
    latest_relevant_operation
        .is_some_and(|candidate| candidate.intent.operation_id == record.intent.operation_id)
}

pub(super) fn operation_record<'a>(
    state: &'a CommonHarnessStateV1,
    operation_id: &str,
) -> Result<&'a codex_history::task_context::OperationRecordRefV1, String> {
    state
        .operation_refs
        .iter()
        .find(|record| record.intent.operation_id == operation_id)
        .ok_or_else(|| "native patch recovery operation record disappeared".into())
}

pub(super) fn ensure_same_recovery_fence(
    before: &CommonHarnessStateV1,
    after: &CommonHarnessStateV1,
) -> Result<(), String> {
    if before.request.task_id != after.request.task_id
        || before.request.request_revision != after.request.request_revision
        || before.run_state.owner_id != after.run_state.owner_id
        || before.run_state.owner_epoch != after.run_state.owner_epoch
        || before.run_state.attempt_no != after.run_state.attempt_no
    {
        return Err("common-harness owner or attempt changed during native patch recovery".into());
    }
    Ok(())
}

pub(super) fn validate_completion_marker(
    codex_home: &std::path::Path,
    thread_id: codex_protocol::ThreadId,
    artifact: &CommonHarnessPatchArtifactV1,
) -> Result<(), String> {
    let expected = artifact.native_patch_completion_marker();
    let expected_ref = predict_operation_artifact_ref(&expected)?;
    if artifact.completion_marker_ref != expected_ref {
        return Err("completion marker reference differs from the reserved content address".into());
    }
    let observed: NativePatchEffectCompleteV1 =
        load_operation_artifact(codex_home, thread_id, &artifact.completion_marker_ref)?;
    if observed != expected {
        return Err("completion marker does not match the exact saved patch effect".into());
    }
    Ok(())
}

pub(super) fn validate_death_proof(
    proof: &NativePatchExecutorDeadProofV1,
    artifact: &CommonHarnessPatchArtifactV1,
    provenance_ref: &str,
) -> Result<(), String> {
    if proof.format != "common-harness-native-patch-dead-proof/1"
        || proof.operation_id != artifact.operation_id
        || proof.thread_id != artifact.executor.thread_id()
        || proof.runtime_owner_id != artifact.executor.runtime_owner_id()
        || proof.owner_epoch != artifact.executor.owner_epoch()
        || proof.process_id != artifact.executor.process_id()
        || !is_esrch_errno(proof.errno)
        || proof.provenance_ref != provenance_ref
    {
        return Err(
            "native patch death proof does not match the immutable executor provenance".into(),
        );
    }
    Ok(())
}

pub(super) fn owner_takeover_chain_reaches_current(
    state: &CommonHarnessStateV1,
    original_owner_id: &str,
    original_owner_epoch: u64,
) -> bool {
    if original_owner_id.trim().is_empty() || original_owner_epoch == 0 {
        return false;
    }
    if original_owner_id == state.run_state.owner_id
        && original_owner_epoch == state.run_state.owner_epoch
    {
        return true;
    }
    let mut cursor_owner = original_owner_id.to_owned();
    let mut cursor_epoch = original_owner_epoch;
    let mut started = false;
    for takeover in &state.owner_takeovers {
        if !started {
            if takeover.previous_owner_id != cursor_owner
                || takeover.previous_owner_epoch != cursor_epoch
            {
                continue;
            }
            started = true;
        } else if takeover.previous_owner_id != cursor_owner
            || takeover.previous_owner_epoch != cursor_epoch
        {
            return false;
        }
        if takeover.new_owner_id.trim().is_empty()
            || takeover.new_owner_epoch != cursor_epoch.saturating_add(1)
        {
            return false;
        }
        cursor_owner.clone_from(&takeover.new_owner_id);
        cursor_epoch = takeover.new_owner_epoch;
    }
    started
        && cursor_owner == state.run_state.owner_id
        && cursor_epoch == state.run_state.owner_epoch
}

#[cfg(unix)]
fn is_esrch_errno(errno: i32) -> bool {
    errno == libc::ESRCH
}

#[cfg(not(unix))]
fn is_esrch_errno(_errno: i32) -> bool {
    false
}

pub(super) fn unix_time_ms() -> Result<i64, String> {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| format!("read native recovery timestamp: {error}"))?;
    i64::try_from(elapsed.as_millis())
        .map_err(|_| "native recovery timestamp is out of range".into())
}
