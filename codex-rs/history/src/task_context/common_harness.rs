//! Host-owned, versioned state for a bounded common-harness task.

use serde::Deserialize;
use serde::Serialize;

#[path = "common_harness_recovery.rs"]
mod recovery;

pub const COMMON_HARNESS_CONTRACT: &str = "common-harness/1.0";
pub const COMMON_HARNESS_VERSION: u32 = 1;
pub const MAX_COMMON_HARNESS_ATTEMPTS: u32 = 32;
pub const MAX_COMMON_HARNESS_RECORDS: usize = 512;
const MAX_COMMON_HARNESS_TEXT_BYTES: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommonHarnessStateV1 {
    pub contract_version: u32,
    pub request: TaskRequestV1,
    pub run_state: TaskRunStateV1,
    #[serde(default)]
    pub operation_refs: Vec<OperationRecordRefV1>,
    #[serde(default)]
    pub evidence_refs: Vec<ConditionEvidenceV1>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub owner_takeovers: Vec<OwnerTakeoverRecordV1>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskRequestV1 {
    pub task_id: String,
    pub request_revision: u64,
    pub authorized_request_ref: String,
    pub goal: String,
    pub workspace_ref: String,
    pub start_target_version: String,
    pub permission_snapshot_ref: String,
    pub constraints: Vec<String>,
    pub conditions: Vec<ConditionSpecV1>,
    pub condition_set_revision: u64,
    pub max_attempts: u32,
    pub operation_limits_ref: String,
    pub skill_refs: Vec<SkillRefV1>,
    pub execution_owner: ExecutionOwnerV1,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConditionSpecV1 {
    pub condition_id: String,
    pub condition_revision: u64,
    pub kind: ConditionKindV1,
    pub comparison_scope: String,
    pub expected: String,
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verifier_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verifier_config_revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<CheckSpecV1>,
    #[serde(default)]
    pub dependency_refs: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckSpecV1 {
    pub argv: Vec<String>,
    pub parser: CheckOutputParserV1,
    pub expected_outcome: ExpectedCheckOutcomeV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub go_test_selection: Option<GoTestSelectionV1>,
    #[serde(default)]
    pub cwd_binding_refs: Vec<String>,
    #[serde(default)]
    pub allowed_environment_refs: Vec<String>,
    #[serde(default)]
    pub toolchain_ref: Option<String>,
    #[serde(default)]
    pub timeout_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoTestSelectionV1 {
    pub selected_cases: Vec<String>,
    pub expected_case_count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillRefV1 {
    pub id: String,
    pub version_ref: String,
    pub content_digest: String,
    pub source_ref: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionOwnerV1 {
    StandaloneSwitch,
    Shiro,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConditionKindV1 {
    ReproduceFailure,
    TestSuite,
    ArtifactComparison,
    SemanticReview,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckOutputParserV1 {
    RustTestJson,
    GoTestJson,
    ExitCode,
    JsonField,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExpectedCheckOutcomeV1 {
    ExitCode(i32),
    JsonField {
        path: String,
        value: String,
    },
    TestCounts {
        passed: u32,
        failed: u32,
        skipped: u32,
    },
    FailureIdentity {
        test_name: String,
        reason: String,
    },
    AllSelectedTestsPass,
    SpecifiedTestFailure {
        test_name: String,
        reason: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskRunStateV1 {
    pub task_id: String,
    pub request_revision: u64,
    pub state_version: u64,
    pub owner_id: String,
    pub owner_epoch: u64,
    pub status: TaskRunStatusV1,
    pub phase: TaskRunPhaseV1,
    pub attempt_no: u32,
    pub target_version: String,
    #[serde(default)]
    pub unmet_conditions: Vec<String>,
    pub next_operation: String,
    #[serde(default)]
    pub reason_code: Option<String>,
    #[serde(default)]
    pub resume_condition: Option<String>,
    pub cancel_requested: bool,
    #[serde(default)]
    pub unsettled_operation_refs: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting: Option<WaitingStateV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_window: Option<WorkWindowV1>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaitingStateV1 {
    pub question_ref: String,
    pub operation_digest: String,
    pub target_version: String,
    pub resume_phase: TaskRunPhaseV1,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkWindowV1 {
    pub window_id: String,
    pub attempt_no: u32,
    pub started_at_ms: i64,
    pub deadline_at_ms: i64,
    pub timeout_ms: u64,
    pub policy_ref: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskRunStatusV1 {
    Queued,
    Running,
    Verifying,
    Waiting,
    Blocked,
    Exhausted,
    Completed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskRunPhaseV1 {
    Plan,
    Apply,
    Reconcile,
    Verify,
    Review,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationRecordRefV1 {
    pub intent: OperationIntentV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt: Option<OperationReceiptV1>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub claims: Vec<OperationClaimV1>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub finalizations: Vec<OperationFinalizationV1>,
}

impl OperationRecordRefV1 {
    /// Return a host-reconciled finalization, or a known legacy receipt. Unknown raw receipts
    /// remain audit evidence and are never interpreted as a settled result.
    pub fn effective_receipt(&self) -> Option<&OperationReceiptV1> {
        self.finalizations
            .last()
            .map(|finalization| &finalization.receipt)
            .or_else(|| {
                self.receipt
                    .as_ref()
                    .filter(|receipt| receipt.outcome != OperationOutcomeV1::Unknown)
            })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationClaimV1 {
    pub claim_id: String,
    pub operation_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_claim_id: Option<String>,
    pub owner_epoch: u64,
    pub expected_state_version: u64,
    pub intent_digest: String,
    pub claimed_at_ms: i64,
    pub deadline_at_ms: i64,
    pub kind: OperationClaimKindV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes_execution_claim_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_executor_dead_proof_ref: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationClaimKindV1 {
    Execution,
    Finalizer,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationFinalizationV1 {
    pub operation_id: String,
    pub finalizer_claim_id: String,
    pub execution_claim_id: String,
    pub receipt: OperationReceiptV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reconciliation_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_observation_refs: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnerTakeoverRecordV1 {
    pub previous_owner_id: String,
    pub new_owner_id: String,
    pub previous_owner_epoch: u64,
    pub new_owner_epoch: u64,
    pub expected_state_version: u64,
    pub takeover_proof_ref: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationIntentV1 {
    pub operation_id: String,
    pub task_id: String,
    pub request_revision: u64,
    pub attempt_no: u32,
    pub owner_epoch: u64,
    pub expected_state_version: u64,
    pub base_target_version: String,
    pub operation_kind: OperationKindV1,
    pub normalized_arguments_ref: String,
    pub normalized_arguments_digest: String,
    pub permission_decision_ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_ref: Option<String>,
    pub executor_ref: String,
    pub timeout_ms: u64,
    pub status: OperationIntentStatusV1,
}

impl OperationIntentV1 {
    /// Digest the complete immutable intent, including permission and executor bindings.
    pub fn content_digest(&self) -> Result<String, String> {
        crate::compaction_candidate::digest(self)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationReceiptV1 {
    pub operation_id: String,
    pub task_id: String,
    pub request_revision: u64,
    pub attempt_no: u32,
    pub owner_epoch: u64,
    pub execution_id: String,
    pub outcome: OperationOutcomeV1,
    pub actual_before_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_after_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_status: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_receipt_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reconciliation_ref: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationKindV1 {
    ApplyPatch,
    ExecuteCommand,
    RunVerifier,
    Review,
    ExternalAction,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationIntentStatusV1 {
    Reserved,
    AwaitingApproval,
    Running,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationOutcomeV1 {
    Succeeded,
    Failed,
    Unknown,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConditionEvidenceV1 {
    pub evidence_id: String,
    pub condition_id: String,
    pub condition_revision: u64,
    pub condition_set_revision: u64,
    pub request_revision: u64,
    pub target_version: String,
    pub comparison_scope_digest: String,
    pub execution_id: String,
    pub operation_id: String,
    pub verifier_ref: String,
    pub verifier_config_revision: u64,
    pub observed: String,
    pub verdict: ConditionVerdictV1,
    #[serde(default)]
    pub log_refs: Vec<String>,
    #[serde(default)]
    pub artifact_refs: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_evidence_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comparison_proof_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_negative_proof: Option<HostNegativeProofV1>,
    pub lifecycle: EvidenceLifecycleV1,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostNegativeProofV1 {
    pub baseline_receipt_ref: String,
    pub expected_failure_identity: String,
    pub observed_failure_identity: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution_receipt_ref: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConditionVerdictV1 {
    Pass,
    Fail,
    Indeterminate,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceLifecycleV1 {
    Current,
    Stale,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateFenceV1 {
    pub request_revision: u64,
    pub expected_state_version: u64,
    pub owner_epoch: u64,
}

/// Events accepted only from the host state owner. This type deliberately cannot be
/// deserialized from model supplied JSON.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostEventV1 {
    StartAttempt {
        target_version: String,
    },
    BeginVerification {
        target_version: String,
    },
    ReserveOperation {
        intent: OperationIntentV1,
        claim: OperationClaimV1,
    },
    ObserveMissingOperation {
        operation_id: String,
        observation_ref: String,
    },
    BindWorkWindow {
        window: WorkWindowV1,
    },
    FinalizeOperation {
        receipt: OperationReceiptV1,
    },
    TakeOwnership {
        new_owner_id: String,
        new_owner_epoch: u64,
        takeover_proof_ref: String,
    },
    ClaimExistingOperation {
        operation_id: String,
        claim: OperationClaimV1,
    },
    FinalizeReconciledOperation {
        finalization: OperationFinalizationV1,
    },
    ResumeReconciled {
        operation_id: String,
        target_version: String,
    },
    RecordEvidence {
        evidence: ConditionEvidenceV1,
    },
    RebindEvidence {
        evidence: ConditionEvidenceV1,
    },
    InvalidateEvidence {
        evidence_ids: Vec<String>,
        new_target_version: String,
    },
    EnterWaiting {
        question_ref: String,
        operation_digest: String,
        resume_phase: TaskRunPhaseV1,
    },
    ResumeWaiting {
        answer_ref: String,
        answer_digest: String,
    },
    ResumeWaitingBound {
        question_ref: String,
        operation_digest: String,
        target_version: String,
        answer_ref: String,
        answer_digest: String,
    },
    RequestCancel,
    ConfirmCancelled,
    EndAttempt {
        reason_code: String,
    },
    Block {
        reason_code: String,
        resume_condition: String,
    },
    Complete {
        target_version: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransitionErrorV1 {
    InvalidContract,
    InvalidRequest(&'static str),
    InvalidState(&'static str),
    FenceMismatch,
    AttemptBudgetExhausted,
    InvalidOperation(&'static str),
    DuplicateOperation,
    OperationNotFound,
    OperationAlreadyFinalized,
    UnknownOperationOutcome,
    InvalidEvidence(&'static str),
    EvidenceNotFound,
    CannotComplete(&'static str),
    UnsettledOperations,
    RecoveryEventNotImplemented,
}

impl CommonHarnessStateV1 {
    pub fn queued(
        request: TaskRequestV1,
        owner_id: String,
        owner_epoch: u64,
    ) -> Result<Self, TransitionErrorV1> {
        validate_request(&request)?;
        validate_request_for_admission(&request)?;
        validate_text(&owner_id).map_err(|_| TransitionErrorV1::InvalidState("owner_id"))?;
        if owner_epoch == 0 {
            return Err(TransitionErrorV1::InvalidState("owner_epoch"));
        }
        let unmet_conditions = request
            .conditions
            .iter()
            .filter(|condition| condition.required)
            .map(|condition| condition.condition_id.clone())
            .collect();
        Ok(Self {
            contract_version: COMMON_HARNESS_VERSION,
            request: request.clone(),
            run_state: TaskRunStateV1 {
                task_id: request.task_id,
                request_revision: request.request_revision,
                state_version: 1,
                owner_id,
                owner_epoch,
                status: TaskRunStatusV1::Queued,
                phase: TaskRunPhaseV1::Plan,
                attempt_no: 0,
                target_version: request.start_target_version,
                unmet_conditions,
                next_operation: "start_attempt".into(),
                reason_code: None,
                resume_condition: None,
                cancel_requested: false,
                unsettled_operation_refs: Vec::new(),
                waiting: None,
                work_window: None,
            },
            operation_refs: Vec::new(),
            evidence_refs: Vec::new(),
            owner_takeovers: Vec::new(),
        })
    }

    /// Apply one host-authored event against a CAS and owner fence.
    pub fn apply_host_event(
        &self,
        fence: StateFenceV1,
        event: HostEventV1,
    ) -> Result<Self, TransitionErrorV1> {
        self.validate(None)?;
        if fence.request_revision != self.request.request_revision
            || fence.expected_state_version != self.run_state.state_version
            || fence.owner_epoch != self.run_state.owner_epoch
        {
            return Err(TransitionErrorV1::FenceMismatch);
        }

        let mut next = self.clone();
        match event {
            HostEventV1::StartAttempt { target_version } => {
                validate_text(&target_version)
                    .map_err(|_| TransitionErrorV1::InvalidState("target_version"))?;
                if self.run_state.cancel_requested {
                    return Err(TransitionErrorV1::InvalidState("cancel_requested"));
                }
                if !self.run_state.unsettled_operation_refs.is_empty() {
                    return Err(TransitionErrorV1::UnsettledOperations);
                }
                if !matches!(
                    self.run_state.status,
                    TaskRunStatusV1::Queued | TaskRunStatusV1::Running | TaskRunStatusV1::Verifying
                ) {
                    return Err(TransitionErrorV1::InvalidState("start_attempt_status"));
                }
                if self.run_state.attempt_no >= self.request.max_attempts {
                    return Err(TransitionErrorV1::AttemptBudgetExhausted);
                }
                if self.run_state.attempt_no > 0
                    && self.run_state.status != TaskRunStatusV1::Queued
                    && !self.has_retryable_failure()
                {
                    return Err(TransitionErrorV1::InvalidState("retry_without_failure"));
                }
                next.run_state.attempt_no += 1;
                next.run_state.work_window = None;
                next.run_state.status = TaskRunStatusV1::Running;
                next.run_state.phase = TaskRunPhaseV1::Plan;
                next.run_state.target_version = target_version;
                next.run_state.reason_code = None;
                next.run_state.resume_condition = None;
                next.run_state.next_operation = "plan".into();
            }
            HostEventV1::BeginVerification { target_version } => {
                if self.run_state.status != TaskRunStatusV1::Running
                    || self.run_state.target_version != target_version
                    || !self.run_state.unsettled_operation_refs.is_empty()
                {
                    return Err(TransitionErrorV1::InvalidState("begin_verification"));
                }
                next.run_state.status = TaskRunStatusV1::Verifying;
                next.run_state.phase = TaskRunPhaseV1::Verify;
                next.run_state.next_operation = "verify_conditions".into();
            }
            HostEventV1::ReserveOperation { intent, claim } => {
                if !matches!(
                    self.run_state.status,
                    TaskRunStatusV1::Running | TaskRunStatusV1::Verifying
                ) {
                    return Err(TransitionErrorV1::InvalidState("reserve_operation_status"));
                }
                self.validate_current_intent(&intent)?;
                recovery::validate_initial_claim(self, &intent, &claim)?;
                if self.operation_refs.len() >= MAX_COMMON_HARNESS_RECORDS {
                    return Err(TransitionErrorV1::InvalidOperation("operation_limit"));
                }
                if self
                    .operation_refs
                    .iter()
                    .any(|record| record.intent.operation_id == intent.operation_id)
                {
                    return Err(TransitionErrorV1::DuplicateOperation);
                }
                next.run_state.next_operation = intent.operation_id.clone();
                next.run_state
                    .unsettled_operation_refs
                    .push(intent.operation_id.clone());
                next.operation_refs.push(OperationRecordRefV1 {
                    intent,
                    receipt: None,
                    claims: vec![claim],
                    finalizations: Vec::new(),
                });
            }
            HostEventV1::FinalizeOperation { receipt } => {
                let Some(index) = self
                    .operation_refs
                    .iter()
                    .position(|record| record.intent.operation_id == receipt.operation_id)
                else {
                    return Err(TransitionErrorV1::OperationNotFound);
                };
                recovery::validate_new_receipt(self, &self.operation_refs[index], &receipt)?;
                if let Some(existing) = self.operation_refs[index].receipt.as_ref() {
                    return if existing == &receipt {
                        Ok(self.clone())
                    } else {
                        Err(TransitionErrorV1::OperationAlreadyFinalized)
                    };
                }
                next.operation_refs[index].receipt = Some(receipt.clone());
                if receipt.outcome == OperationOutcomeV1::Unknown {
                    next.run_state.status = TaskRunStatusV1::Blocked;
                    next.run_state.phase = TaskRunPhaseV1::Reconcile;
                    next.run_state.reason_code = Some("operation_outcome_unknown".into());
                    next.run_state.resume_condition = Some(receipt.operation_id);
                    next.run_state.next_operation = "reconcile_operation".into();
                } else {
                    next.run_state
                        .unsettled_operation_refs
                        .retain(|id| id != &receipt.operation_id);
                    if self.run_state.cancel_requested
                        && next.run_state.unsettled_operation_refs.is_empty()
                    {
                        next.run_state.status = TaskRunStatusV1::Cancelled;
                        next.run_state.next_operation = "return_cancelled".into();
                    }
                }
            }
            event @ (HostEventV1::TakeOwnership { .. }
            | HostEventV1::ClaimExistingOperation { .. }
            | HostEventV1::ObserveMissingOperation { .. }
            | HostEventV1::BindWorkWindow { .. }
            | HostEventV1::FinalizeReconciledOperation { .. }
            | HostEventV1::ResumeReconciled { .. }
            | HostEventV1::ResumeWaitingBound { .. }
            | HostEventV1::EndAttempt { .. }) => {
                return recovery::apply_recovery_event(self, fence, event);
            }
            HostEventV1::RecordEvidence { evidence } => {
                self.validate_new_evidence(&evidence, false)?;
                if self.evidence_refs.len() >= MAX_COMMON_HARNESS_RECORDS {
                    return Err(TransitionErrorV1::InvalidEvidence("evidence_limit"));
                }
                next.evidence_refs.push(evidence);
                next.refresh_unmet_conditions();
            }
            HostEventV1::RebindEvidence { evidence } => {
                self.validate_new_evidence(&evidence, true)?;
                if self.evidence_refs.len() >= MAX_COMMON_HARNESS_RECORDS {
                    return Err(TransitionErrorV1::InvalidEvidence("evidence_limit"));
                }
                next.evidence_refs.push(evidence);
                next.refresh_unmet_conditions();
            }
            HostEventV1::InvalidateEvidence {
                evidence_ids,
                new_target_version,
            } => {
                let completed = self.run_state.status == TaskRunStatusV1::Completed;
                let active = matches!(
                    self.run_state.status,
                    TaskRunStatusV1::Running | TaskRunStatusV1::Verifying
                );
                if (!completed && !active) || (completed && evidence_ids.is_empty()) {
                    return Err(TransitionErrorV1::InvalidState(
                        "invalidate_evidence_status",
                    ));
                }
                validate_text(&new_target_version)
                    .map_err(|_| TransitionErrorV1::InvalidState("target_version"))?;
                let ids: std::collections::BTreeSet<_> =
                    evidence_ids.iter().map(String::as_str).collect();
                if ids.len() != evidence_ids.len()
                    || ids.iter().any(|id| {
                        !self
                            .evidence_refs
                            .iter()
                            .any(|evidence| evidence.evidence_id.as_str() == *id)
                    })
                {
                    return Err(TransitionErrorV1::EvidenceNotFound);
                }
                for evidence in &mut next.evidence_refs {
                    if ids.contains(evidence.evidence_id.as_str()) {
                        evidence.lifecycle = EvidenceLifecycleV1::Stale;
                    }
                }
                next.run_state.target_version = new_target_version;
                if completed {
                    next.run_state.status = if self.run_state.attempt_no < self.request.max_attempts
                    {
                        TaskRunStatusV1::Queued
                    } else {
                        TaskRunStatusV1::Exhausted
                    };
                    next.run_state.phase = TaskRunPhaseV1::Plan;
                    next.run_state.next_operation =
                        if next.run_state.status == TaskRunStatusV1::Queued {
                            "start_attempt".into()
                        } else {
                            "return_exhausted".into()
                        };
                }
                next.refresh_unmet_conditions();
            }
            HostEventV1::EnterWaiting {
                question_ref,
                operation_digest,
                resume_phase,
            } => {
                if !matches!(
                    self.run_state.status,
                    TaskRunStatusV1::Running | TaskRunStatusV1::Verifying
                ) {
                    return Err(TransitionErrorV1::InvalidState("waiting_status"));
                }
                validate_text(&question_ref)
                    .map_err(|_| TransitionErrorV1::InvalidState("question_ref"))?;
                validate_text(&operation_digest)
                    .map_err(|_| TransitionErrorV1::InvalidState("operation_digest"))?;
                next.run_state.waiting = Some(WaitingStateV1 {
                    question_ref,
                    operation_digest,
                    target_version: self.run_state.target_version.clone(),
                    resume_phase,
                });
                next.run_state.status = TaskRunStatusV1::Waiting;
                next.run_state.reason_code = Some("awaiting_bound_answer".into());
                next.run_state.resume_condition = Some("answer_matches_waiting_binding".into());
                next.run_state.next_operation = "await_answer".into();
            }
            HostEventV1::ResumeWaiting { .. } => {
                return Err(TransitionErrorV1::InvalidState(
                    "resume_requires_saved_binding",
                ));
            }
            HostEventV1::RequestCancel => {
                next.run_state.cancel_requested = true;
                next.run_state.status = if self.run_state.unsettled_operation_refs.is_empty() {
                    TaskRunStatusV1::Cancelled
                } else {
                    TaskRunStatusV1::Blocked
                };
                next.run_state.reason_code =
                    Some(if self.run_state.unsettled_operation_refs.is_empty() {
                        "cancelled_before_new_operation".into()
                    } else {
                        "cancel_waits_for_operation_reconciliation".into()
                    });
                next.run_state.resume_condition =
                    if self.run_state.unsettled_operation_refs.is_empty() {
                        None
                    } else {
                        Some("settle_all_operations".into())
                    };
                next.run_state.next_operation =
                    if self.run_state.unsettled_operation_refs.is_empty() {
                        "return_cancelled".into()
                    } else {
                        "reconcile_operations".into()
                    };
            }
            HostEventV1::ConfirmCancelled => {
                if !self.run_state.cancel_requested {
                    return Err(TransitionErrorV1::InvalidState("cancel_not_requested"));
                }
                if !self.run_state.unsettled_operation_refs.is_empty() {
                    return Err(TransitionErrorV1::UnsettledOperations);
                }
                next.run_state.status = TaskRunStatusV1::Cancelled;
                next.run_state.reason_code = Some("cancellation_confirmed".into());
                next.run_state.resume_condition = None;
                next.run_state.next_operation = "return_cancelled".into();
            }
            HostEventV1::Block {
                reason_code,
                resume_condition,
            } => {
                validate_text(&reason_code)
                    .map_err(|_| TransitionErrorV1::InvalidState("reason_code"))?;
                validate_text(&resume_condition)
                    .map_err(|_| TransitionErrorV1::InvalidState("resume_condition"))?;
                next.run_state.status = TaskRunStatusV1::Blocked;
                next.run_state.reason_code = Some(reason_code);
                next.run_state.resume_condition = Some(resume_condition);
                next.run_state.next_operation = "await_recovery".into();
            }
            HostEventV1::Complete { target_version } => {
                if self.run_state.status != TaskRunStatusV1::Verifying
                    || self.run_state.target_version != target_version
                {
                    return Err(TransitionErrorV1::InvalidState("complete_status_or_target"));
                }
                if !self.run_state.unsettled_operation_refs.is_empty() {
                    return Err(TransitionErrorV1::UnsettledOperations);
                }
                self.validate_completion()?;
                next.run_state.status = TaskRunStatusV1::Completed;
                next.run_state.phase = TaskRunPhaseV1::Verify;
                next.run_state.reason_code = None;
                next.run_state.resume_condition = None;
                next.run_state.next_operation = "return_completed".into();
            }
        }
        next.run_state.state_version = self.run_state.state_version + 1;
        next.validate(None)?;
        Ok(next)
    }

    pub fn validate(
        &self,
        completion_conditions: Option<&[super::AcceptanceCondition]>,
    ) -> Result<(), TransitionErrorV1> {
        if self.contract_version != COMMON_HARNESS_VERSION {
            return Err(TransitionErrorV1::InvalidContract);
        }
        validate_request(&self.request)?;
        if self.run_state.task_id != self.request.task_id
            || self.run_state.request_revision != self.request.request_revision
            || self.run_state.state_version == 0
            || self.run_state.owner_epoch == 0
            || self.run_state.attempt_no > self.request.max_attempts
            || self.operation_refs.len() > MAX_COMMON_HARNESS_RECORDS
            || self.evidence_refs.len() > MAX_COMMON_HARNESS_RECORDS
            || self.run_state.unsettled_operation_refs.len() > MAX_COMMON_HARNESS_RECORDS
        {
            return Err(TransitionErrorV1::InvalidState(
                "run_state_identity_or_bounds",
            ));
        }
        validate_text(&self.run_state.owner_id)
            .map_err(|_| TransitionErrorV1::InvalidState("owner_id"))?;
        validate_text(&self.run_state.target_version)
            .map_err(|_| TransitionErrorV1::InvalidState("target_version"))?;
        if let Some(window) = self.run_state.work_window.as_ref() {
            validate_work_window(window, self.run_state.attempt_no)?;
        }
        if (self.run_state.status == TaskRunStatusV1::Waiting) != self.run_state.waiting.is_some() {
            return Err(TransitionErrorV1::InvalidState("waiting_binding"));
        }
        if let Some(waiting) = self.run_state.waiting.as_ref()
            && (waiting.target_version != self.run_state.target_version
                || validate_text(&waiting.question_ref).is_err()
                || validate_text(&waiting.operation_digest).is_err())
        {
            return Err(TransitionErrorV1::InvalidState("waiting_binding"));
        }
        let mut operation_ids = std::collections::BTreeSet::new();
        let mut unsettled_ids = std::collections::BTreeSet::new();
        for record in &self.operation_refs {
            self.validate_historical_intent(&record.intent)?;
            recovery::validate_operation_record(self, record)?;
            if !operation_ids.insert(record.intent.operation_id.as_str()) {
                return Err(TransitionErrorV1::DuplicateOperation);
            }
            if let Some(receipt) = record.receipt.as_ref()
                && receipt.outcome != OperationOutcomeV1::Unknown
                && self
                    .run_state
                    .unsettled_operation_refs
                    .contains(&receipt.operation_id)
            {
                return Err(TransitionErrorV1::InvalidOperation(
                    "settled_operation_unsettled",
                ));
            }
            if record.effective_receipt().is_none() {
                unsettled_ids.insert(record.intent.operation_id.as_str());
            }
        }
        if unsettled_ids
            != self
                .run_state
                .unsettled_operation_refs
                .iter()
                .map(String::as_str)
                .collect()
        {
            return Err(TransitionErrorV1::InvalidState("unsettled_operation_refs"));
        }
        let mut evidence_ids = std::collections::BTreeSet::new();
        for evidence in &self.evidence_refs {
            self.validate_evidence(evidence, completion_conditions)?;
            if evidence.source_evidence_ref.is_some() {
                let source = self.validate_rebind_source_chain(evidence)?;
                let operation = self
                    .operation_refs
                    .iter()
                    .find(|record| record.intent.operation_id == evidence.operation_id)
                    .ok_or(TransitionErrorV1::OperationNotFound)?;
                let receipt = operation
                    .effective_receipt()
                    .ok_or(TransitionErrorV1::InvalidEvidence("receipt_missing"))?;
                if receipt.execution_id != evidence.execution_id
                    || receipt.actual_before_version != source.target_version
                {
                    return Err(TransitionErrorV1::InvalidEvidence("rebind_receipt_origin"));
                }
            }
            if !evidence_ids.insert(evidence.evidence_id.as_str()) {
                return Err(TransitionErrorV1::InvalidEvidence("duplicate_evidence_id"));
            }
        }
        recovery::validate_owner_takeovers(self)?;
        if self.run_state.status == TaskRunStatusV1::Completed {
            self.validate_completion()?;
        }
        Ok(())
    }

    fn validate_current_intent(&self, intent: &OperationIntentV1) -> Result<(), TransitionErrorV1> {
        if intent.task_id != self.request.task_id
            || intent.request_revision != self.request.request_revision
            || intent.attempt_no != self.run_state.attempt_no
            || intent.owner_epoch != self.run_state.owner_epoch
            || intent.expected_state_version != self.run_state.state_version
            || intent.base_target_version != self.run_state.target_version
            || intent.timeout_ms == 0
        {
            return Err(TransitionErrorV1::InvalidOperation("intent_binding"));
        }
        self.validate_historical_intent(intent)
    }

    fn validate_historical_intent(
        &self,
        intent: &OperationIntentV1,
    ) -> Result<(), TransitionErrorV1> {
        if intent.task_id != self.request.task_id
            || intent.request_revision != self.request.request_revision
            || intent.attempt_no == 0
            || intent.attempt_no > self.request.max_attempts
            || intent.owner_epoch == 0
            || intent.expected_state_version == 0
        {
            return Err(TransitionErrorV1::InvalidOperation("intent_binding"));
        }
        for field in [
            &intent.operation_id,
            &intent.normalized_arguments_ref,
            &intent.normalized_arguments_digest,
            &intent.permission_decision_ref,
            &intent.executor_ref,
        ] {
            validate_text(field)
                .map_err(|_| TransitionErrorV1::InvalidOperation("intent_field"))?;
        }
        if intent
            .approval_ref
            .as_ref()
            .is_some_and(|value| validate_text(value).is_err())
        {
            return Err(TransitionErrorV1::InvalidOperation("approval_ref"));
        }
        Ok(())
    }

    fn validate_receipt(
        &self,
        intent: &OperationIntentV1,
        receipt: &OperationReceiptV1,
    ) -> Result<(), TransitionErrorV1> {
        if receipt.operation_id != intent.operation_id
            || receipt.task_id != intent.task_id
            || receipt.request_revision != intent.request_revision
            || receipt.attempt_no != intent.attempt_no
            || receipt.owner_epoch != intent.owner_epoch
            || receipt.actual_before_version != intent.base_target_version
        {
            return Err(TransitionErrorV1::InvalidOperation("receipt_binding"));
        }
        validate_text(&receipt.execution_id)
            .map_err(|_| TransitionErrorV1::InvalidOperation("execution_id"))?;
        validate_text(&receipt.actual_before_version)
            .map_err(|_| TransitionErrorV1::InvalidOperation("actual_before_version"))?;
        for field in [
            &receipt.actual_after_version,
            &receipt.result_ref,
            &receipt.external_receipt_ref,
            &receipt.reconciliation_ref,
        ]
        .into_iter()
        .flatten()
        {
            validate_text(field).map_err(|_| TransitionErrorV1::InvalidOperation("receipt_ref"))?;
        }
        if receipt.outcome == OperationOutcomeV1::Unknown && receipt.reconciliation_ref.is_some() {
            return Err(TransitionErrorV1::InvalidOperation(
                "unknown_reconciliation_ref",
            ));
        }
        Ok(())
    }

    fn validate_new_evidence(
        &self,
        evidence: &ConditionEvidenceV1,
        rebind: bool,
    ) -> Result<(), TransitionErrorV1> {
        let condition = self
            .request
            .conditions
            .iter()
            .find(|condition| condition.condition_id == evidence.condition_id);
        let running_baseline_reproduction = self.run_state.status == TaskRunStatusV1::Running
            && !rebind
            && condition.is_some_and(|condition| {
                condition.required && condition.kind == ConditionKindV1::ReproduceFailure
            });
        if (self.run_state.status != TaskRunStatusV1::Verifying && !running_baseline_reproduction)
            || evidence.request_revision != self.request.request_revision
            || evidence.condition_set_revision != self.request.condition_set_revision
            || evidence.target_version != self.run_state.target_version
            || evidence.lifecycle != EvidenceLifecycleV1::Current
            || evidence.source_evidence_ref.is_some() != rebind
            || (rebind
                && evidence
                    .comparison_proof_ref
                    .as_ref()
                    .is_none_or(|proof| validate_text(proof).is_err()))
        {
            return Err(TransitionErrorV1::InvalidEvidence("evidence_binding"));
        }
        self.validate_evidence(evidence, None)?;
        let Some(operation) = self
            .operation_refs
            .iter()
            .find(|record| record.intent.operation_id == evidence.operation_id)
        else {
            return Err(TransitionErrorV1::OperationNotFound);
        };
        let Some(receipt) = operation.effective_receipt() else {
            return Err(TransitionErrorV1::InvalidEvidence("receipt_missing"));
        };
        if receipt.execution_id != evidence.execution_id
            || (!rebind && receipt.actual_before_version != evidence.target_version)
        {
            return Err(TransitionErrorV1::InvalidEvidence("receipt_mismatch"));
        }
        if rebind {
            let source = self.validate_rebind_source_chain(evidence)?;
            if source.target_version != receipt.actual_before_version {
                return Err(TransitionErrorV1::InvalidEvidence("rebind_receipt_origin"));
            }
        }
        if running_baseline_reproduction {
            if evidence.verdict == ConditionVerdictV1::Error {
                let Some(raw_receipt) = operation.receipt.as_ref() else {
                    return Err(TransitionErrorV1::InvalidEvidence(
                        "baseline_error_raw_receipt_missing",
                    ));
                };
                if evidence.host_negative_proof.is_some()
                    || operation.intent.operation_kind != OperationKindV1::RunVerifier
                    || !operation.finalizations.is_empty()
                    || operation.intent.attempt_no != self.run_state.attempt_no
                    || operation.intent.request_revision != self.request.request_revision
                    || operation.intent.base_target_version != self.request.start_target_version
                    || raw_receipt != receipt
                    || raw_receipt.outcome != OperationOutcomeV1::Failed
                    || raw_receipt.actual_before_version != self.request.start_target_version
                    || raw_receipt.actual_after_version.as_deref()
                        != Some(self.request.start_target_version.as_str())
                    || raw_receipt.exit_status.is_none_or(|status| status == 0)
                    || raw_receipt
                        .result_ref
                        .as_deref()
                        .is_none_or(|reference| validate_text(reference).is_err())
                    || !raw_receipt
                        .result_ref
                        .as_ref()
                        .is_some_and(|reference| evidence.log_refs.contains(reference))
                    || raw_receipt.execution_id != evidence.execution_id
                {
                    return Err(TransitionErrorV1::InvalidEvidence(
                        "baseline_verifier_error_binding",
                    ));
                }
                return Ok(());
            }
            let Some(proof) = evidence.host_negative_proof.as_ref() else {
                return Err(TransitionErrorV1::InvalidEvidence(
                    "baseline_negative_proof_missing",
                ));
            };
            let Some(raw_receipt) = operation.receipt.as_ref() else {
                return Err(TransitionErrorV1::InvalidEvidence(
                    "baseline_raw_receipt_missing",
                ));
            };
            if evidence.verdict != ConditionVerdictV1::Fail
                || evidence.target_version != self.request.start_target_version
                || operation.intent.operation_kind != OperationKindV1::RunVerifier
                || operation.intent.base_target_version != self.request.start_target_version
                || raw_receipt.outcome != OperationOutcomeV1::Failed
                || raw_receipt.actual_before_version != self.request.start_target_version
                || proof.baseline_receipt_ref != operation.intent.operation_id
                || proof.resolution_receipt_ref.is_some()
            {
                return Err(TransitionErrorV1::InvalidEvidence(
                    "baseline_negative_proof_binding",
                ));
            }
        }
        Ok(())
    }

    fn validate_rebind_source_chain<'a>(
        &'a self,
        evidence: &ConditionEvidenceV1,
    ) -> Result<&'a ConditionEvidenceV1, TransitionErrorV1> {
        if evidence
            .comparison_proof_ref
            .as_ref()
            .is_none_or(|proof| validate_text(proof).is_err())
        {
            return Err(TransitionErrorV1::InvalidEvidence(
                "comparison_proof_missing",
            ));
        }
        let source_ref = evidence
            .source_evidence_ref
            .as_deref()
            .ok_or(TransitionErrorV1::InvalidEvidence("rebind_source"))?;
        let mut source = self
            .evidence_refs
            .iter()
            .find(|prior| {
                prior.evidence_id == source_ref && prior.lifecycle == EvidenceLifecycleV1::Stale
            })
            .ok_or(TransitionErrorV1::InvalidEvidence("rebind_source"))?;
        if source.target_version == evidence.target_version {
            return Err(TransitionErrorV1::InvalidEvidence(
                "rebind_target_unchanged",
            ));
        }
        let mut seen = std::collections::BTreeSet::new();
        loop {
            if !seen.insert(source.evidence_id.as_str()) || !same_rebind_facts(source, evidence) {
                return Err(TransitionErrorV1::InvalidEvidence("rebind_source_facts"));
            }
            if source.source_evidence_ref.is_some()
                && source
                    .comparison_proof_ref
                    .as_ref()
                    .is_none_or(|proof| validate_text(proof).is_err())
            {
                return Err(TransitionErrorV1::InvalidEvidence(
                    "rebind_source_comparison_proof",
                ));
            }
            let Some(parent_ref) = source.source_evidence_ref.as_deref() else {
                break;
            };
            let parent = self
                .evidence_refs
                .iter()
                .find(|prior| {
                    prior.evidence_id == parent_ref && prior.lifecycle == EvidenceLifecycleV1::Stale
                })
                .ok_or(TransitionErrorV1::InvalidEvidence("rebind_source_chain"))?;
            if parent.target_version == source.target_version {
                return Err(TransitionErrorV1::InvalidEvidence(
                    "rebind_source_target_unchanged",
                ));
            }
            source = parent;
        }
        Ok(source)
    }

    fn validate_evidence(
        &self,
        evidence: &ConditionEvidenceV1,
        completion_conditions: Option<&[super::AcceptanceCondition]>,
    ) -> Result<(), TransitionErrorV1> {
        let condition = self
            .request
            .conditions
            .iter()
            .find(|condition| condition.condition_id == evidence.condition_id);
        if condition
            .is_none_or(|condition| condition.condition_revision != evidence.condition_revision)
            || evidence.request_revision != self.request.request_revision
            || evidence.condition_set_revision != self.request.condition_set_revision
            || validate_text(&evidence.evidence_id).is_err()
            || validate_text(&evidence.target_version).is_err()
            || validate_text(&evidence.comparison_scope_digest).is_err()
            || validate_text(&evidence.observed).is_err()
            || validate_text(&evidence.verifier_ref).is_err()
            || evidence.verifier_config_revision == 0
        {
            return Err(TransitionErrorV1::InvalidEvidence("evidence_identity"));
        }
        if let Some(conditions) = completion_conditions {
            let canonical = conditions
                .iter()
                .find(|condition| condition.id == evidence.condition_id);
            if canonical.is_none_or(|condition| condition.revision != evidence.condition_revision) {
                return Err(TransitionErrorV1::InvalidEvidence(
                    "canonical_condition_mismatch",
                ));
            }
        }
        if evidence.lifecycle == EvidenceLifecycleV1::Current
            && evidence.host_negative_proof.as_ref().is_some_and(|proof| {
                validate_text(&proof.baseline_receipt_ref).is_err()
                    || validate_text(&proof.expected_failure_identity).is_err()
                    || validate_text(&proof.observed_failure_identity).is_err()
                    || proof
                        .resolution_receipt_ref
                        .as_ref()
                        .is_some_and(|value| validate_text(value).is_err())
                    || proof.expected_failure_identity != proof.observed_failure_identity
            })
        {
            return Err(TransitionErrorV1::InvalidEvidence("negative_proof"));
        }
        Ok(())
    }

    fn validate_completion(&self) -> Result<(), TransitionErrorV1> {
        if !self.run_state.unsettled_operation_refs.is_empty() {
            return Err(TransitionErrorV1::UnsettledOperations);
        }
        for condition in self
            .request
            .conditions
            .iter()
            .filter(|condition| condition.required)
        {
            let evidence = self.evidence_refs.iter().rev().find(|evidence| {
                evidence.condition_id == condition.condition_id
                    && evidence.condition_revision == condition.condition_revision
                    && evidence.condition_set_revision == self.request.condition_set_revision
                    && evidence.request_revision == self.request.request_revision
                    && evidence.target_version == self.run_state.target_version
                    && evidence.lifecycle == EvidenceLifecycleV1::Current
            });
            let Some(evidence) = evidence else {
                return Err(TransitionErrorV1::CannotComplete(
                    "required_evidence_missing",
                ));
            };
            if evidence.verdict != ConditionVerdictV1::Pass {
                return Err(TransitionErrorV1::CannotComplete(
                    "required_condition_not_pass",
                ));
            }
            if condition.kind == ConditionKindV1::ReproduceFailure {
                let Some(proof) = evidence.host_negative_proof.as_ref() else {
                    return Err(TransitionErrorV1::CannotComplete("negative_proof_missing"));
                };
                let baseline_is_start_failure = self.operation_refs.iter().any(|record| {
                    record.intent.operation_id == proof.baseline_receipt_ref
                        && record.intent.operation_kind == OperationKindV1::RunVerifier
                        && record.intent.base_target_version == self.request.start_target_version
                        && record.receipt.as_ref().is_some_and(|receipt| {
                            receipt.outcome == OperationOutcomeV1::Failed
                                && receipt.actual_before_version
                                    == self.request.start_target_version
                        })
                });
                let candidate_target = if evidence.source_evidence_ref.is_some() {
                    self.validate_rebind_source_chain(evidence)?
                        .target_version
                        .as_str()
                } else {
                    evidence.target_version.as_str()
                };
                let candidate_resolution_matches = proof.resolution_receipt_ref.as_deref()
                    == Some(evidence.operation_id.as_str())
                    && self.operation_refs.iter().any(|record| {
                        record.intent.operation_id == evidence.operation_id
                            && record.intent.operation_kind == OperationKindV1::RunVerifier
                            && record.intent.base_target_version.as_str() == candidate_target
                            && record.effective_receipt().is_some_and(|receipt| {
                                receipt.outcome == OperationOutcomeV1::Succeeded
                                    && receipt.actual_before_version.as_str() == candidate_target
                                    && receipt.execution_id == evidence.execution_id
                            })
                    });
                if !baseline_is_start_failure || !candidate_resolution_matches {
                    return Err(TransitionErrorV1::CannotComplete(
                        "negative_proof_unresolved",
                    ));
                }
            }
        }
        Ok(())
    }

    fn has_retryable_failure(&self) -> bool {
        self.request.conditions.iter().any(|condition| {
            condition.required
                && self
                    .evidence_refs
                    .iter()
                    .rev()
                    .find(|evidence| {
                        evidence.condition_id == condition.condition_id
                            && evidence.condition_revision == condition.condition_revision
                            && evidence.condition_set_revision
                                == self.request.condition_set_revision
                            && evidence.request_revision == self.request.request_revision
                            && evidence.target_version == self.run_state.target_version
                            && evidence.lifecycle == EvidenceLifecycleV1::Current
                    })
                    .is_some_and(|evidence| evidence.verdict == ConditionVerdictV1::Fail)
        })
    }

    fn refresh_unmet_conditions(&mut self) {
        self.run_state.unmet_conditions = self
            .request
            .conditions
            .iter()
            .filter(|condition| {
                condition.required
                    && !self.evidence_refs.iter().any(|evidence| {
                        evidence.condition_id == condition.condition_id
                            && evidence.condition_revision == condition.condition_revision
                            && evidence.condition_set_revision
                                == self.request.condition_set_revision
                            && evidence.request_revision == self.request.request_revision
                            && evidence.target_version == self.run_state.target_version
                            && evidence.lifecycle == EvidenceLifecycleV1::Current
                            && evidence.verdict == ConditionVerdictV1::Pass
                    })
            })
            .map(|condition| condition.condition_id.clone())
            .collect();
    }
}

fn same_rebind_facts(source: &ConditionEvidenceV1, candidate: &ConditionEvidenceV1) -> bool {
    source.condition_id == candidate.condition_id
        && source.condition_revision == candidate.condition_revision
        && source.condition_set_revision == candidate.condition_set_revision
        && source.request_revision == candidate.request_revision
        && source.comparison_scope_digest == candidate.comparison_scope_digest
        && source.execution_id == candidate.execution_id
        && source.operation_id == candidate.operation_id
        && source.verifier_ref == candidate.verifier_ref
        && source.verifier_config_revision == candidate.verifier_config_revision
        && source.observed == candidate.observed
        && source.verdict == candidate.verdict
        && source.log_refs == candidate.log_refs
        && source.artifact_refs == candidate.artifact_refs
        && source.host_negative_proof == candidate.host_negative_proof
}

fn validate_request(request: &TaskRequestV1) -> Result<(), TransitionErrorV1> {
    for field in [
        &request.task_id,
        &request.authorized_request_ref,
        &request.goal,
        &request.workspace_ref,
        &request.start_target_version,
        &request.permission_snapshot_ref,
        &request.operation_limits_ref,
    ] {
        validate_text(field).map_err(|_| TransitionErrorV1::InvalidRequest("required_field"))?;
    }
    if request.request_revision == 0
        || request.condition_set_revision == 0
        || request.max_attempts == 0
        || request.max_attempts > MAX_COMMON_HARNESS_ATTEMPTS
        || request.conditions.is_empty()
        || request.conditions.len() > 128
        || request.constraints.len() > 128
        || request.skill_refs.len() > 128
    {
        return Err(TransitionErrorV1::InvalidRequest("revision_or_bounds"));
    }
    let mut condition_ids = std::collections::BTreeSet::new();
    for condition in &request.conditions {
        if !condition.condition_id.starts_with("AC-")
            || condition.condition_revision == 0
            || !condition_ids.insert(condition.condition_id.as_str())
        {
            return Err(TransitionErrorV1::InvalidRequest("condition_identity"));
        }
        for value in [&condition.comparison_scope, &condition.expected] {
            validate_text(value)
                .map_err(|_| TransitionErrorV1::InvalidRequest("condition_text"))?;
        }
        if condition
            .verifier_ref
            .as_ref()
            .is_some_and(|value| validate_text(value).is_err())
            || condition.verifier_config_revision == Some(0)
            || condition.verifier_ref.is_some() != condition.verifier_config_revision.is_some()
        {
            return Err(TransitionErrorV1::InvalidRequest("verifier_binding"));
        }
        if let Some(check) = condition.check.as_ref() {
            if check.argv.is_empty()
                || check.argv.len() > 128
                || check.argv.iter().any(|value| validate_text(value).is_err())
                || check.cwd_binding_refs.len() > 16
                || check
                    .cwd_binding_refs
                    .iter()
                    .any(|value| validate_text(value).is_err())
                || check.allowed_environment_refs.len() > 128
                || check
                    .allowed_environment_refs
                    .iter()
                    .any(|value| validate_text(value).is_err())
                || check
                    .toolchain_ref
                    .as_ref()
                    .is_some_and(|value| validate_text(value).is_err())
                || check.go_test_selection.as_ref().is_some_and(|selection| {
                    selection.selected_cases.is_empty()
                        || selection.expected_case_count == 0
                        || selection.selected_cases.len() != selection.expected_case_count
                        || selection
                            .selected_cases
                            .iter()
                            .any(|value| validate_text(value).is_err())
                })
            {
                return Err(TransitionErrorV1::InvalidRequest("check_argv"));
            }
            if check.timeout_ms > 24 * 60 * 60 * 1000 {
                return Err(TransitionErrorV1::InvalidRequest("check_timeout"));
            }
        }
        if condition.dependency_refs.len() > 128
            || condition
                .dependency_refs
                .iter()
                .any(|value| validate_text(value).is_err())
        {
            return Err(TransitionErrorV1::InvalidRequest("dependency_refs"));
        }
    }
    for constraint in &request.constraints {
        validate_text(constraint).map_err(|_| TransitionErrorV1::InvalidRequest("constraint"))?;
    }
    for skill in &request.skill_refs {
        for value in [
            &skill.id,
            &skill.version_ref,
            &skill.content_digest,
            &skill.source_ref,
        ] {
            validate_text(value).map_err(|_| TransitionErrorV1::InvalidRequest("skill_ref"))?;
        }
    }
    Ok(())
}

fn validate_request_for_admission(request: &TaskRequestV1) -> Result<(), TransitionErrorV1> {
    for condition in &request.conditions {
        if matches!(
            condition.kind,
            ConditionKindV1::TestSuite | ConditionKindV1::ReproduceFailure
        ) {
            let Some(check) = condition.check.as_ref() else {
                return Err(TransitionErrorV1::InvalidRequest(
                    "executable_condition_check",
                ));
            };
            if condition.verifier_ref.is_none()
                || condition.verifier_config_revision.is_none()
                || check.timeout_ms == 0
                || check.cwd_binding_refs.is_empty()
                || check.toolchain_ref.is_none()
                || (check.parser == CheckOutputParserV1::GoTestJson
                    && check.go_test_selection.is_none())
            {
                return Err(TransitionErrorV1::InvalidRequest("incomplete_check_spec"));
            }
        }
    }
    Ok(())
}

fn validate_text(value: &str) -> Result<(), ()> {
    if value.trim().is_empty() || value.len() > MAX_COMMON_HARNESS_TEXT_BYTES {
        Err(())
    } else {
        Ok(())
    }
}

fn validate_work_window(
    window: &WorkWindowV1,
    active_attempt_no: u32,
) -> Result<(), TransitionErrorV1> {
    if active_attempt_no == 0
        || window.attempt_no != active_attempt_no
        || window.started_at_ms <= 0
        || window.deadline_at_ms <= 0
        || window.timeout_ms == 0
        || validate_text(&window.window_id).is_err()
        || validate_text(&window.policy_ref).is_err()
    {
        return Err(TransitionErrorV1::InvalidState("work_window_binding"));
    }
    let timeout_ms = i64::try_from(window.timeout_ms)
        .map_err(|_| TransitionErrorV1::InvalidState("work_window_timeout"))?;
    let expected_deadline = window
        .started_at_ms
        .checked_add(timeout_ms)
        .ok_or(TransitionErrorV1::InvalidState("work_window_deadline"))?;
    if window.deadline_at_ms != expected_deadline {
        return Err(TransitionErrorV1::InvalidState("work_window_deadline"));
    }
    Ok(())
}

#[cfg(test)]
#[path = "common_harness_tests.rs"]
mod tests;
