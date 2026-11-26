// Modified by RenCrow Switch Core, 2026-11-27: recover native patch outcomes from durable provenance.
//! Host-owned recovery for native patch operations whose executor terminated.

use super::common_harness::common_harness_runtime_owner_id;
use super::common_harness_writer::verify_local_writer;
#[path = "common_harness_native_patch_recovery_validation.rs"]
mod validation;
use validation::ensure_same_recovery_fence;
pub(super) use validation::is_current_pending_resume;
use validation::operation_record;
use validation::owner_takeover_chain_reaches_current;
use validation::unix_time_ms;
use validation::validate_completion_marker;
use validation::validate_death_proof;

use super::session::Session;
use super::turn_context::TurnContext;
use crate::common_harness_artifacts::load_operation_artifact;
use crate::common_harness_artifacts::write_operation_artifact;
use crate::common_harness_io::ArtifactBindingV1;
use crate::common_harness_io::PatchReconciliationV1;
use crate::common_harness_io::reconcile_patch_journal;
use crate::common_harness_native_patch::CommonHarnessPatchArtifactV1;
use crate::common_harness_native_patch::NativePatchExecutorDeadProofV1;
use crate::common_harness_native_patch::NativeProcessProbeV1;
use crate::common_harness_native_patch::probe_executor_dead;
use codex_history::task_context::CommonHarnessStateV1;
use codex_history::task_context::HostEventV1;
use codex_history::task_context::OperationClaimKindV1;
use codex_history::task_context::OperationClaimV1;
use codex_history::task_context::OperationFinalizationV1;
use codex_history::task_context::OperationIntentStatusV1;
use codex_history::task_context::OperationKindV1;
use codex_history::task_context::OperationOutcomeV1;
use codex_history::task_context::OperationReceiptV1;
use codex_history::task_context::TaskRunStatusV1;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct NativePatchRecoveryObservationV1 {
    format: String,
    operation_artifact_ref: String,
    provenance_ref: String,
    death_proof_ref: String,
    raw_observation_ref: Option<String>,
    observed_reconciliation: PatchReconciliationV1,
    observed_target_digest: String,
    observed_at_unix_ms: i64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct NativePatchRecoveryResolutionV1 {
    format: String,
    operation_artifact_ref: String,
    provenance_ref: String,
    death_proof_ref: String,
    observation_ref: String,
    operation_id: String,
    execution_claim_id: String,
    predicted_target_version: String,
    finalized_at_unix_ms: i64,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct NativePatchRecoveryBlockedObservationV1<'a> {
    format: &'static str,
    operation_artifact_ref: &'a str,
    provenance_ref: &'a str,
    operation_id: &'a str,
    reason: &'a str,
}

impl Session {
    /// Reconcile only a positively dead native patch executor. This method is
    /// called under the already-held completion owner guard after local-writer
    /// takeover; it never reapplies the native patch.
    pub(crate) async fn reconcile_unsettled_common_harness_native_patch(
        &self,
        root_task_id: &str,
        turn: &TurnContext,
        already_read_state: CommonHarnessStateV1,
        owner_guard: &tokio::sync::OwnedMutexGuard<()>,
    ) -> Result<CommonHarnessStateV1, String> {
        if !matches!(
            turn.session_source,
            codex_protocol::protocol::SessionSource::Cli
                | codex_protocol::protocol::SessionSource::VSCode
                | codex_protocol::protocol::SessionSource::Exec
        ) {
            return Err("native patch recovery requires the root execution route".into());
        }
        let current = self.current_common_harness_state(root_task_id).await?;
        if current != already_read_state {
            return Err("common-harness state changed before native patch reconciliation".into());
        }
        if current.request.execution_owner
            != codex_history::task_context::ExecutionOwnerV1::StandaloneSwitch
        {
            return Err("native patch recovery is unavailable to externally owned tasks".into());
        }
        if current.run_state.status == TaskRunStatusV1::Waiting
            || current.run_state.status == TaskRunStatusV1::Cancelled
            || current.run_state.cancel_requested
        {
            return Err("native patch recovery is blocked by waiting or cancellation state".into());
        }
        if current.run_state.unsettled_operation_refs.is_empty() {
            let pending = current
                .operation_refs
                .iter()
                .filter(|record| is_current_pending_resume(&current, record))
                .collect::<Vec<_>>();
            if pending.len() != 1 {
                return Err("no unique finalized native patch is awaiting target resume".into());
            }
            let record = pending[0];
            if current.run_state.owner_id
                != common_harness_runtime_owner_id(&self.thread_id().to_string())
            {
                return Err(
                    "pending native patch resume requires the current runtime owner".into(),
                );
            }
            let writer = verify_local_writer(self)
                .await
                .map_err(|error| format!("verify native patch resume writer: {error}"))?;
            let canonical_home = turn
                .config
                .codex_home
                .as_path()
                .canonicalize()
                .map_err(|error| format!("resolve native patch resume CODEX_HOME: {error}"))?;
            let rollout_path = self
                .current_rollout_path()
                .await
                .map_err(|error| format!("resolve native patch resume rollout: {error}"))?
                .ok_or_else(|| "native patch resume requires a current local rollout".to_string())?
                .canonicalize()
                .map_err(|error| format!("canonicalize native patch resume rollout: {error}"))?;
            if writer.thread_id() != self.thread_id()
                || writer.codex_home() != canonical_home.as_path()
                || writer.rollout_path() != rollout_path.as_path()
            {
                return Err("pending native patch resume writer does not match this thread".into());
            }
            let target_version = record
                .effective_receipt()
                .and_then(|receipt| receipt.actual_after_version.clone())
                .ok_or_else(|| {
                    "pending native patch finalization has no target version".to_string()
                })?;
            let finalization = record
                .finalizations
                .last()
                .ok_or_else(|| "pending native patch finalization record is missing".to_string())?;
            let resolution_ref = finalization.reconciliation_ref.as_deref().ok_or_else(|| {
                "pending native patch finalization has no resolution artifact".to_string()
            })?;
            let resolution: NativePatchRecoveryResolutionV1 =
                load_operation_artifact(&canonical_home, self.thread_id(), resolution_ref)?;
            if resolution.format != "common-harness-native-patch-recovery-resolution/1"
                || resolution.operation_id != record.intent.operation_id
                || resolution.operation_artifact_ref != record.intent.normalized_arguments_ref
                || resolution.provenance_ref != resolution.operation_artifact_ref
                || resolution.execution_claim_id != finalization.execution_claim_id
                || resolution.predicted_target_version != target_version
                || resolution.finalized_at_unix_ms <= 0
                || !finalization
                    .source_observation_refs
                    .contains(&resolution.observation_ref)
                || !finalization
                    .source_observation_refs
                    .contains(&resolution.death_proof_ref)
                || !finalization
                    .source_observation_refs
                    .contains(&resolution.operation_artifact_ref)
            {
                return Err("pending native patch resolution bindings are invalid".into());
            }
            let patch_artifact: CommonHarnessPatchArtifactV1 = load_operation_artifact(
                &canonical_home,
                self.thread_id(),
                &resolution.operation_artifact_ref,
            )?;
            let canonical_workspace = turn
                .config
                .cwd
                .as_path()
                .canonicalize()
                .map_err(|error| format!("resolve pending patch workspace: {error}"))?;
            let expected_binding = ArtifactBindingV1::for_canonical_root(&canonical_workspace)
                .map_err(|error| format!("bind pending patch workspace: {error}"))?;
            let execution_claim = record
                .claims
                .iter()
                .find(|claim| {
                    claim.claim_id == finalization.execution_claim_id
                        && claim.kind == OperationClaimKindV1::Execution
                })
                .ok_or_else(|| {
                    "pending native patch original execution claim is missing".to_string()
                })?;
            let permission_ref = super::common_harness::permission_snapshot_ref(&turn.config)?;
            let journal_binding = &patch_artifact.journal.binding;
            if patch_artifact.operation_id != record.intent.operation_id
                || journal_binding.operation_id != record.intent.operation_id
                || journal_binding.workspace_root != canonical_workspace
                || journal_binding.artifact_binding != expected_binding
                || expected_binding.workspace_binding_ref != current.request.workspace_ref
                || journal_binding.task_id != current.request.task_id
                || journal_binding.request_revision != current.request.request_revision
                || journal_binding.attempt_no != record.intent.attempt_no
                || journal_binding.owner_epoch != execution_claim.owner_epoch
                || journal_binding.expected_state_version != record.intent.expected_state_version
                || journal_binding.artifact_paths
                    != super::common_harness::request_dependency_paths(&current.request)?
                || patch_artifact.journal.before.digest != current.run_state.target_version
                || patch_artifact.journal.predicted_after.digest != target_version
                || record.intent.permission_decision_ref != permission_ref
                || patch_artifact.executor.operation_id() != record.intent.operation_id
                || patch_artifact.executor.thread_id() != self.thread_id().to_string()
                || patch_artifact.executor.owner_epoch() != execution_claim.owner_epoch
                || !owner_takeover_chain_reaches_current(
                    &current,
                    patch_artifact.executor.runtime_owner_id(),
                    patch_artifact.executor.owner_epoch(),
                )
            {
                return Err(
                    "pending native patch journal no longer matches its task binding".into(),
                );
            }
            let proof: NativePatchExecutorDeadProofV1 = load_operation_artifact(
                &canonical_home,
                self.thread_id(),
                &resolution.death_proof_ref,
            )?;
            validate_death_proof(&proof, &patch_artifact, &resolution.provenance_ref)?;
            validate_completion_marker(&canonical_home, self.thread_id(), &patch_artifact)?;
            if reconcile_patch_journal(&patch_artifact.journal)
                .map_err(|error| format!("recheck pending native patch target: {error}"))?
                != PatchReconciliationV1::AllPost
            {
                return Err("pending native patch target changed before resume".into());
            }
            return self
                .apply_common_harness_host_event_guarded(
                    root_task_id,
                    super::common_harness::common_harness_fence(&current),
                    HostEventV1::ResumeReconciled {
                        operation_id: record.intent.operation_id.clone(),
                        target_version,
                    },
                    owner_guard,
                )
                .await;
        }
        if current.run_state.unsettled_operation_refs.len() != 1 {
            return Err("native patch recovery requires exactly one unsettled operation".into());
        }
        let operation_id = current.run_state.unsettled_operation_refs[0].as_str();
        let record = current
            .operation_refs
            .iter()
            .find(|record| record.intent.operation_id == operation_id)
            .ok_or_else(|| "unsettled native patch operation record is missing".to_string())?;
        if record.intent.operation_kind != OperationKindV1::ApplyPatch
            || record.intent.status != OperationIntentStatusV1::Running
            || record.intent.task_id != current.request.task_id
            || record.intent.request_revision != current.request.request_revision
            || record.intent.attempt_no != current.run_state.attempt_no
            || record.intent.base_target_version != current.run_state.target_version
            || record
                .receipt
                .as_ref()
                .is_some_and(|receipt| receipt.outcome != OperationOutcomeV1::Unknown)
            || !record.finalizations.is_empty()
        {
            return Err(
                "unsettled native patch intent is inconsistent or already finalized".into(),
            );
        }
        let execution_claims = record
            .claims
            .iter()
            .filter(|claim| claim.kind == OperationClaimKindV1::Execution)
            .collect::<Vec<_>>();
        if execution_claims.len() != 1 {
            return Err(
                "native patch recovery requires the single original execution claim".into(),
            );
        }
        let execution_claim = execution_claims[0];
        if execution_claim.operation_id != record.intent.operation_id
            || execution_claim.owner_epoch != record.intent.owner_epoch
            || execution_claim.expected_state_version != record.intent.expected_state_version
            || execution_claim.deadline_at_ms <= 0
        {
            return Err("original native patch execution claim does not match its intent".into());
        }
        if current.run_state.owner_id
            != common_harness_runtime_owner_id(&self.thread_id().to_string())
        {
            return Err("native patch recovery requires the current runtime owner".into());
        }

        // Reconfirm the exact durable local writer and its host boundary at the
        // current consumer turn. This proof is additional to the state fence.
        let writer = verify_local_writer(self)
            .await
            .map_err(|error| format!("verify native patch recovery writer: {error}"))?;
        let canonical_home = turn
            .config
            .codex_home
            .as_path()
            .canonicalize()
            .map_err(|error| format!("resolve native patch recovery CODEX_HOME: {error}"))?;
        let canonical_workspace = turn
            .config
            .cwd
            .as_path()
            .canonicalize()
            .map_err(|error| format!("resolve native patch recovery workspace: {error}"))?;
        let current_rollout = self
            .current_rollout_path()
            .await
            .map_err(|error| format!("resolve current native patch recovery rollout: {error}"))?
            .ok_or_else(|| "native patch recovery requires a current local rollout".to_string())?
            .canonicalize()
            .map_err(|error| format!("canonicalize native patch recovery rollout: {error}"))?;
        if writer.thread_id() != self.thread_id()
            || writer.codex_home() != canonical_home.as_path()
            || writer.rollout_path() != current_rollout.as_path()
            || current_rollout.strip_prefix(&canonical_home).is_err()
        {
            return Err(
                "native patch recovery writer does not match this thread and CODEX_HOME".into(),
            );
        }
        let operation_artifact_ref = record.intent.normalized_arguments_ref.as_str();
        let patch_artifact: CommonHarnessPatchArtifactV1 =
            load_operation_artifact(&canonical_home, self.thread_id(), operation_artifact_ref)?;
        let journal = &patch_artifact.journal;
        let dependency_paths = super::common_harness::request_dependency_paths(&current.request)?;
        let current_binding = ArtifactBindingV1::for_canonical_root(&canonical_workspace)
            .map_err(|error| format!("bind current native patch workspace: {error}"))?;
        let permission_ref = super::common_harness::permission_snapshot_ref(&turn.config)?;
        let executor_thread_id = self.thread_id().to_string();
        let binding_mismatch = [
            (
                "artifact.operation_id",
                patch_artifact.operation_id != record.intent.operation_id,
            ),
            (
                "journal.operation_id",
                journal.binding.operation_id != record.intent.operation_id,
            ),
            (
                "journal.task_id",
                journal.binding.task_id != current.request.task_id,
            ),
            (
                "journal.request_revision",
                journal.binding.request_revision != current.request.request_revision,
            ),
            (
                "journal.attempt_no",
                journal.binding.attempt_no != record.intent.attempt_no,
            ),
            (
                "journal.owner_epoch",
                journal.binding.owner_epoch != execution_claim.owner_epoch,
            ),
            (
                "journal.expected_state_version",
                journal.binding.expected_state_version != record.intent.expected_state_version,
            ),
            (
                "journal.workspace_root",
                journal.binding.workspace_root != canonical_workspace,
            ),
            (
                "journal.artifact_binding",
                journal.binding.artifact_binding != current_binding,
            ),
            (
                "journal.workspace_binding_ref",
                journal.binding.artifact_binding.workspace_binding_ref
                    != current.request.workspace_ref,
            ),
            (
                "journal.artifact_paths",
                journal.binding.artifact_paths != dependency_paths,
            ),
            (
                "journal.before_digest",
                journal.before.digest != record.intent.base_target_version,
            ),
            (
                "intent.permission_decision_ref",
                record.intent.permission_decision_ref != permission_ref,
            ),
            (
                "executor.operation_id",
                patch_artifact.executor.operation_id() != record.intent.operation_id,
            ),
            (
                "executor.thread_id",
                patch_artifact.executor.thread_id() != executor_thread_id,
            ),
            (
                "executor.owner_epoch",
                patch_artifact.executor.owner_epoch() != execution_claim.owner_epoch,
            ),
            (
                "executor.owner_chain",
                !owner_takeover_chain_reaches_current(
                    &current,
                    patch_artifact.executor.runtime_owner_id(),
                    patch_artifact.executor.owner_epoch(),
                ),
            ),
        ]
        .into_iter()
        .find_map(|(field, mismatch)| mismatch.then_some(field));
        if binding_mismatch.is_some() {
            #[cfg(test)]
            if let Some(field) = binding_mismatch {
                tracing::debug!(field, "native patch recovery binding mismatch");
            }
            return Err(
                "native patch artifact, executor, or current workspace binding changed".into(),
            );
        }
        let normalized_digest = format!(
            "sha256:{:x}",
            sha2::Sha256::digest(patch_artifact.normalized_patch.as_bytes())
        );
        if normalized_digest != record.intent.normalized_arguments_digest {
            return Err(
                "native patch immutable payload digest does not match its operation intent".into(),
            );
        }
        if validate_completion_marker(&canonical_home, self.thread_id(), &patch_artifact).is_err() {
            if record.receipt.is_none() {
                let blocked_ref = write_operation_artifact(
                    &canonical_home,
                    self.thread_id(),
                    &NativePatchRecoveryBlockedObservationV1 {
                        format: "common-harness-native-patch-recovery-blocked/1",
                        operation_artifact_ref,
                        provenance_ref: operation_artifact_ref,
                        operation_id,
                        reason: "awaited native patch effect completion marker is missing or invalid",
                    },
                )?;
                let blocked = self
                    .apply_common_harness_host_event_guarded(
                        root_task_id,
                        super::common_harness::common_harness_fence(&current),
                        HostEventV1::ObserveMissingOperation {
                            operation_id: operation_id.to_owned(),
                            observation_ref: blocked_ref,
                        },
                        owner_guard,
                    )
                    .await?;
                return Ok(blocked);
            }
            return Ok(current);
        }

        // Probe the saved host PID before any journal reconciliation reads the
        // target files. A missing receipt becomes Unknown even when the process
        // is still alive; it stays unsettled until a later positive ESRCH.
        let probe = probe_executor_dead(&patch_artifact.executor, operation_artifact_ref)?;
        let NativeProcessProbeV1::ProvenDead(death_proof) = probe else {
            let reason = "original native patch executor is not positively known dead";
            if record.receipt.is_none() {
                let blocked_ref = write_operation_artifact(
                    &canonical_home,
                    self.thread_id(),
                    &NativePatchRecoveryBlockedObservationV1 {
                        format: "common-harness-native-patch-recovery-blocked/1",
                        operation_artifact_ref,
                        provenance_ref: operation_artifact_ref,
                        operation_id,
                        reason,
                    },
                )?;
                return self
                    .apply_common_harness_host_event_guarded(
                        root_task_id,
                        super::common_harness::common_harness_fence(&current),
                        HostEventV1::ObserveMissingOperation {
                            operation_id: operation_id.to_owned(),
                            observation_ref: blocked_ref,
                        },
                        owner_guard,
                    )
                    .await;
            }
            return Ok(current);
        };
        validate_death_proof(&death_proof, &patch_artifact, operation_artifact_ref)?;
        let death_proof_ref =
            write_operation_artifact(&canonical_home, self.thread_id(), &death_proof)?;
        let loaded_proof: NativePatchExecutorDeadProofV1 =
            load_operation_artifact(&canonical_home, self.thread_id(), &death_proof_ref)?;
        if loaded_proof != death_proof {
            return Err("persisted native executor death proof changed".into());
        }

        let reconciliation = reconcile_patch_journal(journal)
            .map_err(|error| format!("reconcile original native patch journal: {error}"))?;
        if reconciliation != PatchReconciliationV1::AllPost {
            let reason = match reconciliation {
                PatchReconciliationV1::AllPre => {
                    "native patch journal is AllPre; re-execution is not enabled"
                }
                PatchReconciliationV1::PartialOrOther => {
                    "native patch journal is ambiguous or partially applied"
                }
                PatchReconciliationV1::AllPost => unreachable!(),
            };
            if record.receipt.is_none() {
                let blocked_ref = write_operation_artifact(
                    &canonical_home,
                    self.thread_id(),
                    &NativePatchRecoveryBlockedObservationV1 {
                        format: "common-harness-native-patch-recovery-blocked/1",
                        operation_artifact_ref,
                        provenance_ref: operation_artifact_ref,
                        operation_id,
                        reason,
                    },
                )?;
                return self
                    .apply_common_harness_host_event_guarded(
                        root_task_id,
                        super::common_harness::common_harness_fence(&current),
                        HostEventV1::ObserveMissingOperation {
                            operation_id: operation_id.to_owned(),
                            observation_ref: blocked_ref,
                        },
                        owner_guard,
                    )
                    .await;
            }
            return Ok(current);
        }
        if journal.predicted_after.digest == journal.before.digest {
            return Err(
                "native patch AllPost target does not advance the bound target version".into(),
            );
        }

        let existing_raw_ref = record
            .receipt
            .as_ref()
            .and_then(|receipt| receipt.result_ref.clone());
        let observation = NativePatchRecoveryObservationV1 {
            format: "common-harness-native-patch-recovery-observation/1".into(),
            operation_artifact_ref: operation_artifact_ref.to_owned(),
            provenance_ref: operation_artifact_ref.to_owned(),
            death_proof_ref: death_proof_ref.clone(),
            raw_observation_ref: existing_raw_ref.clone(),
            observed_reconciliation: PatchReconciliationV1::AllPost,
            observed_target_digest: journal.predicted_after.digest.clone(),
            observed_at_unix_ms: unix_time_ms()?,
        };
        let observation_ref =
            write_operation_artifact(&canonical_home, self.thread_id(), &observation)?;
        let stored_observation: NativePatchRecoveryObservationV1 =
            load_operation_artifact(&canonical_home, self.thread_id(), &observation_ref)?;
        if stored_observation != observation {
            return Err("persisted native patch recovery observation changed".into());
        }

        let mut state = self.current_common_harness_state(root_task_id).await?;
        ensure_same_recovery_fence(&current, &state)?;
        let current_record = operation_record(&state, operation_id)?.clone();
        if current_record.receipt.is_none() {
            state = self
                .apply_common_harness_host_event_guarded(
                    root_task_id,
                    super::common_harness::common_harness_fence(&state),
                    HostEventV1::ObserveMissingOperation {
                        operation_id: operation_id.to_owned(),
                        observation_ref: observation_ref.clone(),
                    },
                    owner_guard,
                )
                .await?;
        } else if current_record
            .receipt
            .as_ref()
            .is_some_and(|receipt| receipt.outcome != OperationOutcomeV1::Unknown)
        {
            return Err("native patch recovery will not replace a known raw receipt".into());
        }

        // The guarded host event returns the durable current state; the same
        // owner guard is still held, so its result is the next fence.
        let current_record = operation_record(&state, operation_id)?.clone();
        let raw_receipt = current_record
            .receipt
            .as_ref()
            .filter(|receipt| receipt.outcome == OperationOutcomeV1::Unknown)
            .cloned()
            .ok_or_else(|| {
                "native patch recovery requires the original raw Unknown receipt".to_string()
            })?;
        if raw_receipt.execution_id != execution_claim.claim_id
            || raw_receipt.owner_epoch != execution_claim.owner_epoch
            || raw_receipt.operation_id != operation_id
            || raw_receipt.actual_before_version != record.intent.base_target_version
            || raw_receipt.actual_after_version.is_some()
            || raw_receipt.exit_status.is_some()
        {
            return Err("raw Unknown receipt does not match the original execution claim".into());
        }
        let finalizer_claim = if let Some(last) = current_record.claims.last().filter(|claim| {
            claim.kind == OperationClaimKindV1::Finalizer
                && claim.owner_epoch == state.run_state.owner_epoch
        }) {
            if last.deadline_at_ms != execution_claim.deadline_at_ms
                || last.intent_digest != current_record.intent.content_digest()?
            {
                return Err("persisted native patch finalizer claim does not match the original deadline or intent".into());
            }
            last.clone()
        } else {
            let previous_claim_id = current_record
                .claims
                .last()
                .map(|claim| claim.claim_id.clone());
            let claim = OperationClaimV1 {
                claim_id: format!(
                    "native-patch-finalizer:{}:{}",
                    operation_id,
                    uuid::Uuid::now_v7()
                ),
                operation_id: operation_id.to_owned(),
                previous_claim_id,
                owner_epoch: state.run_state.owner_epoch,
                expected_state_version: state.run_state.state_version,
                intent_digest: current_record.intent.content_digest()?,
                claimed_at_ms: unix_time_ms()?,
                deadline_at_ms: execution_claim.deadline_at_ms,
                kind: OperationClaimKindV1::Finalizer,
                supersedes_execution_claim_id: None,
                native_executor_dead_proof_ref: None,
            };
            state = self
                .apply_common_harness_host_event_guarded(
                    root_task_id,
                    super::common_harness::common_harness_fence(&state),
                    HostEventV1::ClaimExistingOperation {
                        operation_id: operation_id.to_owned(),
                        claim: claim.clone(),
                    },
                    owner_guard,
                )
                .await?;
            claim
        };

        let resolution = NativePatchRecoveryResolutionV1 {
            format: "common-harness-native-patch-recovery-resolution/1".into(),
            operation_artifact_ref: operation_artifact_ref.to_owned(),
            provenance_ref: operation_artifact_ref.to_owned(),
            death_proof_ref: death_proof_ref.clone(),
            observation_ref: observation_ref.clone(),
            operation_id: operation_id.to_owned(),
            execution_claim_id: execution_claim.claim_id.clone(),
            predicted_target_version: journal.predicted_after.digest.clone(),
            finalized_at_unix_ms: unix_time_ms()?,
        };
        let resolution_ref =
            write_operation_artifact(&canonical_home, self.thread_id(), &resolution)?;
        let stored_resolution: NativePatchRecoveryResolutionV1 =
            load_operation_artifact(&canonical_home, self.thread_id(), &resolution_ref)?;
        if stored_resolution.format != "common-harness-native-patch-recovery-resolution/1"
            || stored_resolution.operation_id != operation_id
            || stored_resolution.operation_artifact_ref != operation_artifact_ref
            || stored_resolution.provenance_ref != operation_artifact_ref
            || stored_resolution.death_proof_ref != death_proof_ref
            || stored_resolution.observation_ref != observation_ref
            || stored_resolution.execution_claim_id != execution_claim.claim_id
            || stored_resolution.predicted_target_version != journal.predicted_after.digest
            || stored_resolution.finalized_at_unix_ms <= 0
        {
            return Err("persisted native patch resolution changed".into());
        }
        let effective_receipt = OperationReceiptV1 {
            operation_id: operation_id.to_owned(),
            task_id: current.request.task_id.clone(),
            request_revision: current.request.request_revision,
            attempt_no: current_record.intent.attempt_no,
            owner_epoch: execution_claim.owner_epoch,
            execution_id: execution_claim.claim_id.clone(),
            outcome: OperationOutcomeV1::Succeeded,
            actual_before_version: raw_receipt.actual_before_version.clone(),
            actual_after_version: Some(journal.predicted_after.digest.clone()),
            exit_status: None,
            result_ref: Some(resolution_ref.clone()),
            external_receipt_ref: None,
            reconciliation_ref: Some(resolution_ref.clone()),
        };
        let finalization = OperationFinalizationV1 {
            operation_id: operation_id.to_owned(),
            finalizer_claim_id: finalizer_claim.claim_id,
            execution_claim_id: execution_claim.claim_id.clone(),
            receipt: effective_receipt,
            reconciliation_ref: Some(resolution_ref),
            source_observation_refs: vec![
                observation_ref,
                death_proof_ref,
                operation_artifact_ref.to_owned(),
            ],
        };
        state = self
            .apply_common_harness_host_event_guarded(
                root_task_id,
                super::common_harness::common_harness_fence(&state),
                HostEventV1::FinalizeReconciledOperation { finalization },
                owner_guard,
            )
            .await?;
        state = self
            .apply_common_harness_host_event_guarded(
                root_task_id,
                super::common_harness::common_harness_fence(&state),
                HostEventV1::ResumeReconciled {
                    operation_id: operation_id.to_owned(),
                    target_version: journal.predicted_after.digest.clone(),
                },
                owner_guard,
            )
            .await?;
        let settled = operation_record(&state, operation_id)?;
        if !settled
            .receipt
            .as_ref()
            .is_some_and(|receipt| receipt.outcome == OperationOutcomeV1::Unknown)
            || settled
                .effective_receipt()
                .is_none_or(|receipt| receipt.outcome != OperationOutcomeV1::Succeeded)
            || !state.run_state.unsettled_operation_refs.is_empty()
            || state.run_state.attempt_no != current.run_state.attempt_no
            || state.run_state.target_version != journal.predicted_after.digest
        {
            return Err(
                "native patch reconciliation did not reach the expected same-attempt AllPost state"
                    .into(),
            );
        }
        Ok(state)
    }
}
