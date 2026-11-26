// Modified by RenCrow Switch Core, 2026-11-26: return actionable completion identity validation errors.
// Modified by RenCrow Switch Core, 2026-11-21: retain exact accepted task-revision evidence history.
// Modified by RenCrow Switch Core, 2026-11-20: separate caller and host checkpoint budgets.
//! Typed, host-verified task-boundary state and deterministic derived context.

// Modified by RenCrow Switch Core, 2026-11-16.
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

#[path = "task_context/accepted_evidence.rs"]
mod accepted_evidence;
pub use accepted_evidence::AcceptedTaskEvidenceReceipt;
pub use accepted_evidence::AcceptedTaskEvidenceResolution;
pub use accepted_evidence::resolve_accepted_task_evidence;
use accepted_evidence::retain_referenced_task_evidence;
use accepted_evidence::validate_accepted_history;

const MAX_BOUNDARY_BYTES: usize = 32 * 1024;
const MAX_TEXT_BYTES: usize = 4096;
pub const MAX_COMPLETION_INPUT_BYTES: usize = MAX_BOUNDARY_BYTES;
pub const MAX_COMPLETION_CHECKPOINT_BYTES: usize = 256 * 1024;

pub fn validate_completion_state_input_size(value: &serde_json::Value) -> Result<(), String> {
    let encoded = serde_json::to_vec(value).map_err(|error| error.to_string())?;
    if encoded.len() > MAX_COMPLETION_INPUT_BYTES {
        return Err("completion state input exceeds caller size limit".into());
    }
    Ok(())
}

fn is_default<T: Default + PartialEq>(value: &T) -> bool {
    value == &T::default()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoundaryCandidate {
    pub version: u32,
    pub mode: BoundaryMode,
    pub thread_id: String,
    pub root_task_id: String,
    pub closed_task_id: String,
    pub revision: u64,
    /// Revision of the closed subtask artifact/check, independent of the task-board revision.
    pub closed_task_revision: u64,
    pub goal: String,
    pub tasks: Vec<TaskState>,
    pub current_task_id: Option<String>,
    pub artifact_ref: String,
    pub verification_ref: String,
    pub decisions: Vec<String>,
    pub constraints: Vec<String>,
    /// Optional autonomous completion ledger. Missing legacy state is always unknown/open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_state: Option<CompletionState>,
}

/// Persisted finite acceptance ledger and compaction-safe continuation state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompletionState {
    pub contract_revision: u64,
    /// Host-owned count of accepted units since the last plan review.
    #[serde(default)]
    pub units_completed_since_review: u32,
    /// Host-owned time of the last plan review; independent of product progress.
    #[serde(default)]
    pub plan_review_at: Option<i64>,
    /// Host-authored history of explicit plan review decisions.
    #[serde(default)]
    pub plan_reviews: Vec<PlanReviewRecord>,
    /// Host-owned pause state; callers must not allow a model update to clear it.
    #[serde(default)]
    pub paused: bool,
    /// Host event keys already consumed for durable resume/idempotence.
    #[serde(default)]
    pub consumed_host_events: Vec<String>,
    pub conditions: Vec<AcceptanceCondition>,
    #[serde(default)]
    pub units: Vec<CompletionUnit>,
    #[serde(default)]
    pub current_unit: Option<String>,
    #[serde(default)]
    pub failed_approaches: Vec<FailedApproach>,
    #[serde(default)]
    pub blocker: Option<CompletionBlocker>,
    #[serde(default)]
    pub next_action: Option<String>,
    #[serde(default)]
    pub product_progress_at: Option<i64>,
    #[serde(default)]
    pub runtime_recovery_at: Option<i64>,
    #[serde(default)]
    pub activity_at: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcceptanceStatus {
    Open,
    Satisfied,
    Rejected,
    Waived,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptanceCondition {
    pub id: String,
    pub revision: u64,
    pub result: String,
    pub scope: String,
    pub verification: String,
    pub acceptance_criteria: String,
    pub status: AcceptanceStatus,
    #[serde(default)]
    pub evidence: Vec<CompletionEvidenceRef>,
    #[serde(default)]
    pub unmet_reason: Option<String>,
}

/// Canonical task receipt binding; callers must resolve it against host accepted receipts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompletionEvidenceRef {
    pub task_id: String,
    pub task_revision: u64,
    pub artifact_ref: String,
    pub verification_ref: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompletionUnit {
    pub id: String,
    pub purpose: String,
    pub status: UnitStatus,
    /// Missing legacy values are ordinary executable work units.
    #[serde(default, skip_serializing_if = "is_default")]
    pub kind: CompletionUnitKind,
    #[serde(default)]
    pub condition_refs: Vec<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub evidence: Vec<CompletionEvidenceRef>,
    #[serde(default)]
    pub task_refs: Vec<CompletionEvidenceRef>,
    /// Exact task revision a read-only reviewer was asked to inspect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_target: Option<CompletionReviewTarget>,
    /// Host-collected spawn/wait evidence. Model proposals are overwritten by the host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_evidence: Option<CompletionReviewEvidence>,
    /// Host-owned reservation retained when a review spawn outcome is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_launch: Option<CompletionReviewLaunchState>,
    /// Parent's reason for adopting this review, distinct from task acceptance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_decision_reason: Option<String>,
    /// Canonical host-owned logical review obligation and attempt lifecycle. Missing only in
    /// legacy v1 snapshots; legacy fields above remain a derived compatibility projection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_state: Option<CompletionReviewState>,
    /// Explicit link to an adopted review obligation being revalidated against a newer
    /// accepted artifact. This intent is checked and bound to host provenance on Update.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_revalidation_of: Option<String>,
}

/// Versioned state for one logical review obligation. Attempts are bounded to the active
/// attempt; prior state is retained by an immutable persisted-snapshot reference.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompletionReviewState {
    pub version: u32,
    pub obligation_unit_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_attempt: Option<CompletionReviewAttempt>,
    /// Most recent exact historical failure that is independent of the current active
    /// generation. Older facts remain reachable through the persisted snapshot chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub historical_failure: Option<CompletionReviewHistoricalFailure>,
    /// Preserves the pre-retry bounded failure slot until the new attempt has a
    /// terminal result, allowing a known no-spawn rollback to restore the exact state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_reservation_prior_historical_failure: Option<CompletionReviewHistoricalFailure>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_attempt_ref: Option<CompletionReviewSnapshotRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_authorization: Option<CompletionReviewRetryAuthorization>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub withdrawal: Option<CompletionReviewWithdrawal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revalidation: Option<CompletionReviewRevalidationSource>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompletionReviewRevalidationSource {
    pub source_unit_id: String,
    pub source_adoption_sha256: String,
    pub source_snapshot: CompletionReviewSnapshotRef,
    pub required_artifact_fingerprint: String,
    pub source_condition_definitions_sha256: String,
    pub required_condition_definitions_sha256: String,
    pub update_call_id: String,
}

pub fn task_check_artifact_fingerprint(artifact_ref: &str) -> Option<&str> {
    let (prefix, fingerprint) = artifact_ref.rsplit_once(':')?;
    if prefix
        .strip_prefix("task-check:")
        .is_none_or(|call_id| call_id.trim().is_empty())
        || fingerprint.len() != 64
        || fingerprint
            .bytes()
            .any(|byte| !byte.is_ascii_hexdigit() || byte.is_ascii_uppercase())
    {
        return None;
    }
    Some(fingerprint)
}

pub fn completion_condition_definitions_sha256(
    contract: &CompletionState,
    condition_refs: &[String],
) -> Result<String, String> {
    use crate::compaction_candidate::digest;

    let definitions = condition_refs
        .iter()
        .map(|id| {
            contract
                .conditions
                .iter()
                .find(|condition| condition.id == *id)
                .map(|condition| {
                    serde_json::json!({
                        "id": condition.id,
                        "revision": condition.revision,
                        "result": condition.result,
                        "scope": condition.scope,
                        "verification": condition.verification,
                        "acceptance_criteria": condition.acceptance_criteria,
                    })
                })
                .ok_or_else(|| format!("review condition {id} is missing"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    digest(&serde_json::Value::Array(definitions))
}

pub fn completion_review_adoption_sha256(unit: &CompletionUnit) -> Result<String, String> {
    use crate::compaction_candidate::digest;

    let evidence = unit
        .review_evidence
        .as_ref()
        .ok_or_else(|| "revalidation source has no collected review evidence".to_string())?;
    let adopted = unit.review_state.as_ref().is_none_or(|review| {
        review.active_attempt.as_ref().is_some_and(|attempt| {
            attempt.status == CompletionReviewAttemptStatus::Completed
                && attempt.adoption_reason.as_deref() == unit.review_decision_reason.as_deref()
        })
    });
    if unit.kind != CompletionUnitKind::Review
        || unit.status != UnitStatus::Done
        || !adopted
        || evidence.terminal_status != Some(CompletionReviewTerminalStatus::Completed)
        || evidence
            .wait_result_ref
            .as_deref()
            .is_none_or(str::is_empty)
        || evidence.wait_call_id.as_deref().is_none_or(str::is_empty)
        || unit
            .review_decision_reason
            .as_deref()
            .is_none_or(str::is_empty)
    {
        return Err("revalidation source is not an exact collected and adopted review".into());
    }
    let bytes = serde_json::to_value(unit).map_err(|error| error.to_string())?;
    digest(&bytes)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompletionReviewWithdrawal {
    pub source: CompletionReviewWithdrawalSource,
    pub reason: String,
    pub target_task_id: String,
    pub target_task_revision: u64,
    pub condition_refs: Vec<String>,
    pub depends_on: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CompletionReviewWithdrawalSource {
    HostUpdate {
        update_call_id: String,
        prior_status: UnitStatus,
    },
    LegacyV1Snapshot {
        snapshot: CompletionReviewSnapshotRef,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompletionReviewHistoricalFailure {
    pub attempt_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract_revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imported_from: Option<CompletionReviewSnapshotRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumed_retry_authorization: Option<CompletionReviewRetryAuthorization>,
    pub reviewer_thread_id: String,
    pub reviewer_turn_id: String,
    pub target_task_id: String,
    pub target_task_revision: u64,
    pub target_artifact_ref: String,
    pub reviewer_role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_unix_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_result_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_escalation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_safety_fingerprint: Option<String>,
    pub terminal_source: CompletionReviewTerminalSource,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompletionReviewAttempt {
    /// Host spawn call id, also the unique attempt id.
    pub attempt_id: String,
    pub contract_revision: u64,
    pub condition_refs: Vec<String>,
    pub conditions_sha256: String,
    pub status: CompletionReviewAttemptStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_unknown_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumed_retry_authorization: Option<CompletionReviewRetryAuthorization>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imported_from: Option<CompletionReviewSnapshotRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer_thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer_turn_id: Option<String>,
    pub reviewer_role: String,
    pub reviewer_model: Option<String>,
    pub target_task_id: String,
    pub target_task_revision: u64,
    pub target_artifact_ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_unix_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_escalation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_safety_fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_source: Option<CompletionReviewTerminalSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_result_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adoption_reason: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionReviewAttemptStatus {
    Reserved,
    Unknown,
    Bound,
    TerminalUnknown,
    Failed,
    Completed,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompletionReviewTerminalSource {
    pub status: CompletionReviewTerminalStatus,
    pub event: Option<CompletionReviewTerminalEvent>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompletionReviewTerminalEvent {
    pub rollout_id: String,
    pub locator: CompletionReviewRecordLocator,
    pub owner_thread_id: String,
    pub turn_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_parent_thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_agent_role: Option<String>,
    pub reason: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionReviewRecordLocator {
    Ordinal(u64),
    LegacyLineIndex(u64),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompletionReviewSnapshotRef {
    pub version: u32,
    pub sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompletionReviewRetryAuthorization {
    pub authorization_id: String,
    pub expected_attempt_id: String,
    pub plan_review_call_id: String,
    pub reason: String,
    pub previous_plan: String,
    pub revised_plan: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionUnitKind {
    #[default]
    Execution,
    Review,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompletionReviewTarget {
    pub task_id: String,
    pub task_revision: u64,
}

/// Host-bound proof that the configured read-only child reviewed the target and
/// its terminal result was collected by the parent through native wait_agent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompletionReviewEvidence {
    pub reviewer_thread_id: String,
    /// Child turn generation sent with the review request, not a later follow-up.
    pub reviewer_turn_id: String,
    pub reviewer_role: String,
    pub spawn_call_id: String,
    /// Host-owned absolute deadline for this reviewer generation. Legacy reviews have no SLA
    /// deadline and must not be cancelled by a deadline inferred during restore.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_unix_ms: Option<i64>,
    /// Host escalation recorded when expiry could not safely stop the active reviewer turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_escalation: Option<String>,
    /// Last unchanged safety observation; permits a retry only after runtime state changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_safety_fingerprint: Option<String>,
    /// `None` means the spawned review is pending or not yet collected.
    #[serde(default)]
    pub wait_result_ref: Option<String>,
    /// Host-observed successful terminal state from the collected reviewer.
    #[serde(default)]
    pub terminal_status: Option<CompletionReviewTerminalStatus>,
    /// Native parent wait_agent call that returned the terminal review result.
    #[serde(default)]
    pub wait_call_id: Option<String>,
    pub target_task_id: String,
    pub target_task_revision: u64,
    pub target_artifact_ref: String,
}

/// Durable host reservation for a review spawn that may have created a child.
/// A reserved or unknown launch must not be repeated until host reconciliation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompletionReviewLaunchState {
    pub spawn_call_id: String,
    pub status: CompletionReviewLaunchStatus,
    /// Host-owned absolute deadline reserved before attempting this spawn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_unix_ms: Option<i64>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionReviewLaunchStatus {
    Reserved,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionReviewTerminalStatus {
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnitStatus {
    Planned,
    Ready,
    Running,
    Blocked,
    Done,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FailedApproach {
    pub summary: String,
    pub cause: String,
    pub next_action: String,
    #[serde(default)]
    pub recorded_at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanReviewRecord {
    pub at: i64,
    pub previous_plan: String,
    pub revised_plan: String,
    pub reason: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockerCategory {
    UserInput,
    Authority,
    ExternalService,
    MissingEvidence,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompletionBlocker {
    pub category: BlockerCategory,
    pub reason: String,
    #[serde(default)]
    pub required_input: Option<String>,
    #[serde(default)]
    pub authority: Option<String>,
    #[serde(default)]
    pub evidence: Vec<CompletionEvidenceRef>,
    #[serde(default)]
    pub recheck_condition: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_owner: Option<CompletionReviewBlockerOwner>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompletionReviewBlockerOwner {
    pub unit_id: String,
    pub attempt_id: String,
    pub safety_fingerprint: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoundaryMode {
    Task,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskState {
    pub id: String,
    pub purpose: String,
    pub status: TaskStatus,
    #[serde(default)]
    pub depends_on: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
    Blocked,
}

/// Facts resolved by the host, never decoded from a model's boundary proposal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostBoundaryFacts {
    pub thread_id: String,
    pub authorized_parent: bool,
    pub root_task_id: String,
    pub current_revision: u64,
    pub closed_task_id: String,
    pub closed_task_revision: u64,
    pub artifact_ref: Option<String>,
    pub artifact_revision: Option<u64>,
    pub artifact_result: Option<String>,
    pub verification_ref: Option<String>,
    pub verification_artifact_ref: Option<String>,
    pub verification_artifact_revision: Option<u64>,
    pub verification_succeeded: bool,
    pub parent_review_ref: Option<String>,
    pub parent_review_matches: bool,
    pub unresolved_effects: Vec<String>,
    pub accepted_completed: BTreeMap<String, AcceptedTaskReceipt>,
    /// Prior exact receipt identities copied only from a validated host checkpoint.
    pub accepted_history: Vec<AcceptedTaskEvidenceReceipt>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptedTaskReceipt {
    pub revision: u64,
    pub result: String,
    pub artifact_ref: String,
    pub verification_ref: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BoundaryError {
    UnsupportedVersion,
    WrongMode,
    WrongThread,
    UnauthorizedParent,
    StaleRevision,
    WrongRootTask,
    InvalidTasks,
    DependencyMissing,
    DependencyCycle,
    ClosedTaskNotCompleted,
    InvalidArtifact,
    InvalidVerification,
    FailedVerification,
    InvalidParentReview,
    UnresolvedEffects,
    EmptyField,
    Oversized,
    UnverifiedCompletedTask,
    InvalidCompletionState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatedBoundary {
    candidate: BoundaryCandidate,
    parent_review_ref: String,
    completed: BTreeMap<String, AcceptedTaskReceipt>,
    accepted_history: Vec<AcceptedTaskEvidenceReceipt>,
    current_result: String,
}

/// Host-accepted task state persisted in a V3 checkpoint. This is intentionally distinct from
/// the model-facing boundary proposal and contains no host authorization facts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredTaskState {
    pub boundary: BoundaryCandidate,
    pub completed: BTreeMap<String, AcceptedTaskReceipt>,
    /// Bounded identities for accepted receipts retained by completion evidence history.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accepted_history: Vec<AcceptedTaskEvidenceReceipt>,
    pub parent_review_ref: String,
    pub previous_revision: Option<u64>,
    pub previous_state_hash: Option<String>,
    pub candidate_hash: String,
    /// Host-issued idempotency identifier. It identifies a submission and grants no authority.
    pub boundary_nonce: String,
}

impl ValidatedBoundary {
    pub fn candidate(&self) -> &BoundaryCandidate {
        &self.candidate
    }
    pub fn parent_review_ref(&self) -> &str {
        &self.parent_review_ref
    }

    pub fn completed(&self) -> &BTreeMap<String, AcceptedTaskReceipt> {
        &self.completed
    }

    /// Create persisted state only from a boundary already checked against host facts.
    pub fn into_stored_state(
        self,
        previous_revision: Option<u64>,
        previous_state_hash: Option<String>,
        boundary_nonce: String,
    ) -> Result<StoredTaskState, String> {
        let current_receipt = AcceptedTaskReceipt {
            // The completion receipt is scoped to the closed task, not the board snapshot.
            revision: self.candidate.closed_task_revision,
            result: self.current_result,
            artifact_ref: self.candidate.artifact_ref.clone(),
            verification_ref: self.candidate.verification_ref.clone(),
        };
        let current_task_id = self.candidate.closed_task_id.clone();
        let mut history_sources = self.accepted_history;
        history_sources.extend(self.completed.iter().map(|(task_id, receipt)| {
            AcceptedTaskEvidenceReceipt {
                task_id: task_id.clone(),
                task_revision: receipt.revision,
                artifact_ref: receipt.artifact_ref.clone(),
                verification_ref: receipt.verification_ref.clone(),
            }
        }));
        let mut completed = self.completed;
        completed.insert(current_task_id, current_receipt);
        let accepted_history = retain_referenced_task_evidence(
            self.candidate.completion_state.as_ref(),
            &completed,
            history_sources,
        )?;
        let candidate_hash = candidate_hash(&self.candidate)?;
        let state = StoredTaskState {
            boundary: self.candidate,
            completed,
            accepted_history,
            parent_review_ref: self.parent_review_ref,
            previous_revision,
            previous_state_hash,
            candidate_hash,
            boundary_nonce,
        };
        state.validate(&state.boundary.thread_id)?;
        Ok(state)
    }
}

impl StoredTaskState {
    pub fn state_hash(&self) -> Result<String, String> {
        self.validate(&self.boundary.thread_id)?;
        crate::compaction_candidate::digest(self)
    }

    pub fn parse_and_validate(value: serde_json::Value, thread_id: &str) -> Result<Self, String> {
        let state: Self = serde_json::from_value(value)
            .map_err(|error| format!("invalid stored task state: {error}"))?;
        state.validate(thread_id)?;
        Ok(state)
    }

    pub fn validate(&self, thread_id: &str) -> Result<(), String> {
        if self.boundary.version != 1
            || self.boundary.mode != BoundaryMode::Task
            || thread_id.trim().is_empty()
            || self.boundary.thread_id != thread_id
            || self.boundary.root_task_id.trim().is_empty()
            || self.boundary.revision == 0
            || self.boundary.closed_task_revision == 0
            || self.parent_review_ref.trim().is_empty()
            || self.boundary_nonce.trim().is_empty()
            || self.boundary_nonce.len() > MAX_TEXT_BYTES
        {
            return Err("stored task state identity or review receipt is invalid".into());
        }
        if candidate_hash(&self.boundary)? != self.candidate_hash {
            return Err("stored task state candidate hash does not match its boundary".into());
        }
        match (self.previous_revision, self.previous_state_hash.as_deref()) {
            (None, None) => {}
            (Some(revision), Some(hash))
                if revision < self.boundary.revision && is_sha256(hash) => {}
            _ => return Err("stored task state previous revision binding is invalid".into()),
        }
        validate_host_size_and_text(&self.boundary)
            .map_err(|error| format!("invalid stored task state: {error:?}"))?;
        if serde_json::to_vec(self)
            .map_err(|error| error.to_string())?
            .len()
            > MAX_COMPLETION_CHECKPOINT_BYTES
        {
            return Err("stored task state exceeds the checkpoint size limit".into());
        }
        validate_tasks(&self.boundary)
            .map_err(|error| format!("invalid stored task state: {error:?}"))?;
        validate_completion_state_with_history(
            self.boundary.completion_state.as_ref(),
            &self.completed,
            &self.accepted_history,
        )
        .map_err(|error| format!("invalid stored completion state: {error}"))?;
        for task in self
            .boundary
            .tasks
            .iter()
            .filter(|task| task.status == TaskStatus::Completed)
        {
            let Some(receipt) = self.completed.get(&task.id) else {
                return Err("stored task state omits an accepted completion receipt".into());
            };
            if receipt.revision == 0
                || receipt.result.trim().is_empty()
                || receipt.artifact_ref.trim().is_empty()
                || receipt.verification_ref.trim().is_empty()
            {
                return Err("stored task state has an invalid completion receipt".into());
            }
        }
        if self.completed[&self.boundary.closed_task_id].revision
            != self.boundary.closed_task_revision
        {
            return Err("stored closed-task receipt has a stale revision".into());
        }
        if self.completed.keys().any(|id| {
            self.boundary
                .tasks
                .iter()
                .all(|task| task.id != *id || task.status != TaskStatus::Completed)
        }) {
            return Err("stored task state has an unbound completion receipt".into());
        }
        validate_accepted_history(
            self.boundary.completion_state.as_ref(),
            &self.completed,
            &self.accepted_history,
        )?;
        Ok(())
    }

    pub fn render_context(&self) -> Result<String, String> {
        self.validate(&self.boundary.thread_id)?;
        let mut completed = self.completed.clone();
        completed.remove(&self.boundary.closed_task_id);
        let validated = ValidatedBoundary {
            candidate: self.boundary.clone(),
            parent_review_ref: self.parent_review_ref.clone(),
            completed,
            accepted_history: self.accepted_history.clone(),
            current_result: self.completed[&self.boundary.closed_task_id].result.clone(),
        };
        Ok(render_task_context(&validated))
    }
}

fn candidate_hash(candidate: &BoundaryCandidate) -> Result<String, String> {
    let encoded = serde_json::to_string(candidate)
        .map_err(|error| format!("failed to encode task boundary candidate: {error}"))?;
    Ok(crate::archive_reference::content_sha256(&encoded))
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub fn validate_boundary(
    candidate: BoundaryCandidate,
    facts: &HostBoundaryFacts,
) -> Result<ValidatedBoundary, BoundaryError> {
    validate_boundary_with_size(candidate, facts, BoundarySizeContract::CallerInput)
}

pub fn validate_host_boundary_checkpoint(
    candidate: BoundaryCandidate,
    facts: &HostBoundaryFacts,
) -> Result<ValidatedBoundary, BoundaryError> {
    validate_boundary_with_size(candidate, facts, BoundarySizeContract::HostCheckpoint)
}

#[derive(Clone, Copy)]
enum BoundarySizeContract {
    CallerInput,
    HostCheckpoint,
}

fn validate_boundary_with_size(
    candidate: BoundaryCandidate,
    facts: &HostBoundaryFacts,
    size_contract: BoundarySizeContract,
) -> Result<ValidatedBoundary, BoundaryError> {
    if candidate.version != 1 {
        return Err(BoundaryError::UnsupportedVersion);
    }
    if candidate.mode != BoundaryMode::Task {
        return Err(BoundaryError::WrongMode);
    }
    if candidate.thread_id != facts.thread_id {
        return Err(BoundaryError::WrongThread);
    }
    if !facts.authorized_parent {
        return Err(BoundaryError::UnauthorizedParent);
    }
    if candidate.root_task_id != facts.root_task_id {
        return Err(BoundaryError::WrongRootTask);
    }
    if candidate.revision != facts.current_revision {
        return Err(BoundaryError::StaleRevision);
    }
    if candidate.closed_task_id != facts.closed_task_id
        || candidate.closed_task_revision != facts.closed_task_revision
    {
        return Err(BoundaryError::StaleRevision);
    }
    match size_contract {
        BoundarySizeContract::CallerInput => validate_size_and_text(&candidate)?,
        BoundarySizeContract::HostCheckpoint => validate_host_size_and_text(&candidate)?,
    }
    let artifact = facts
        .artifact_ref
        .as_deref()
        .ok_or(BoundaryError::InvalidArtifact)?;
    if artifact != candidate.artifact_ref {
        return Err(BoundaryError::InvalidArtifact);
    }
    if facts.artifact_revision != Some(candidate.closed_task_revision)
        || facts.artifact_result.as_deref().is_none_or(str::is_empty)
    {
        return Err(BoundaryError::InvalidArtifact);
    }
    let current_result = facts
        .artifact_result
        .clone()
        .ok_or(BoundaryError::InvalidArtifact)?;
    let verification = facts
        .verification_ref
        .as_deref()
        .ok_or(BoundaryError::InvalidVerification)?;
    if verification != candidate.verification_ref {
        return Err(BoundaryError::InvalidVerification);
    }
    if !facts.verification_succeeded {
        return Err(BoundaryError::FailedVerification);
    }
    if facts.verification_artifact_ref.as_deref() != Some(artifact)
        || facts.verification_artifact_revision != Some(candidate.closed_task_revision)
    {
        return Err(BoundaryError::InvalidVerification);
    }
    if !facts.parent_review_matches {
        return Err(BoundaryError::InvalidParentReview);
    }
    let review = facts
        .parent_review_ref
        .clone()
        .ok_or(BoundaryError::InvalidParentReview)?;
    if !facts.unresolved_effects.is_empty() {
        return Err(BoundaryError::UnresolvedEffects);
    }
    validate_tasks(&candidate)?;
    for task in candidate
        .tasks
        .iter()
        .filter(|t| t.status == TaskStatus::Completed && t.id != candidate.closed_task_id)
    {
        let Some(receipt) = facts.accepted_completed.get(&task.id) else {
            return Err(BoundaryError::UnverifiedCompletedTask);
        };
        if receipt.revision == 0
            || receipt.result.trim().is_empty()
            || receipt.artifact_ref.trim().is_empty()
            || receipt.verification_ref.trim().is_empty()
        {
            return Err(BoundaryError::UnverifiedCompletedTask);
        }
    }
    let mut receipts = facts.accepted_completed.clone();
    receipts.insert(
        candidate.closed_task_id.clone(),
        AcceptedTaskReceipt {
            revision: candidate.closed_task_revision,
            result: current_result.clone(),
            artifact_ref: artifact.to_owned(),
            verification_ref: verification.to_owned(),
        },
    );
    let mut accepted_history = facts.accepted_history.clone();
    accepted_history.extend(facts.accepted_completed.iter().map(|(task_id, receipt)| {
        AcceptedTaskEvidenceReceipt {
            task_id: task_id.clone(),
            task_revision: receipt.revision,
            artifact_ref: receipt.artifact_ref.clone(),
            verification_ref: receipt.verification_ref.clone(),
        }
    }));
    validate_completion_state_with_history(
        candidate.completion_state.as_ref(),
        &receipts,
        &accepted_history,
    )
    .map_err(|_| BoundaryError::InvalidCompletionState)?;
    Ok(ValidatedBoundary {
        candidate,
        parent_review_ref: review,
        completed: facts.accepted_completed.clone(),
        accepted_history: facts.accepted_history.clone(),
        current_result,
    })
}

fn validate_completion_review_state(
    unit: &CompletionUnit,
    review: &CompletionReviewState,
    contract_revision: u64,
) -> Result<(), String> {
    let snapshot_ref_invalid = |reference: &CompletionReviewSnapshotRef, versions: &[u32]| {
        !versions.contains(&reference.version)
            || reference.sha256.len() != 64
            || reference
                .sha256
                .bytes()
                .any(|byte| !byte.is_ascii_hexdigit() || byte.is_ascii_uppercase())
    };
    if review.version != 1 || review.obligation_unit_id != unit.id {
        return Err("completion review state version or obligation identity is invalid".into());
    }
    if review
        .previous_attempt_ref
        .as_ref()
        .is_some_and(|reference| snapshot_ref_invalid(reference, &[1, 2]))
    {
        return Err("completion review snapshot reference is invalid".into());
    }
    if review
        .revalidation
        .as_ref()
        .is_some_and(|source| snapshot_ref_invalid(&source.source_snapshot, &[1, 2]))
    {
        return Err("review revalidation source snapshot reference is invalid".into());
    }
    let previous_ref_is_revalidation_source = review.revalidation.as_ref().is_some_and(|source| {
        review.previous_attempt_ref.as_ref() == Some(&source.source_snapshot)
    });
    if review
        .active_attempt
        .as_ref()
        .is_some_and(|attempt| attempt.consumed_retry_authorization.is_some())
        && (review.previous_attempt_ref.is_none() || previous_ref_is_revalidation_source)
    {
        return Err(
            "consumed retry authorization requires its same-obligation grant snapshot reference"
                .into(),
        );
    }
    let historical_failure_invalid = |failure: &CompletionReviewHistoricalFailure| {
        failure.attempt_id.trim().is_empty()
            || failure
                .contract_revision
                .is_some_and(|revision| revision == 0 || revision > contract_revision)
            || failure
                .imported_from
                .as_ref()
                .is_some_and(|source| snapshot_ref_invalid(source, &[1]))
            || failure.reviewer_thread_id.trim().is_empty()
            || failure.reviewer_turn_id.trim().is_empty()
            || failure.target_task_id
                != unit
                    .review_target
                    .as_ref()
                    .map_or("", |target| target.task_id.as_str())
            || failure.target_task_revision
                != unit
                    .review_target
                    .as_ref()
                    .map_or(0, |target| target.task_revision)
            || failure.target_artifact_ref.trim().is_empty()
            || failure.reviewer_role.trim().is_empty()
            || failure
                .reviewer_model
                .as_ref()
                .is_some_and(|model| model.trim().is_empty())
            || failure
                .deadline_unix_ms
                .is_some_and(|deadline| deadline <= 0)
            || failure.wait_result_ref.is_some() != failure.wait_call_id.is_some()
            || (failure.terminal_source.event.is_none() && failure.wait_result_ref.is_none())
            || failure.terminal_source.status != CompletionReviewTerminalStatus::Failed
            || failure.terminal_source.event.as_ref().is_some_and(|event| {
                event.rollout_id.trim().is_empty()
                    || event.owner_thread_id != failure.reviewer_thread_id
                    || event.turn_id != failure.reviewer_turn_id
                    || event.reason != "ReviewEnded"
                    || event.owner_agent_role.as_deref() != Some(failure.reviewer_role.as_str())
                    || event
                        .owner_parent_thread_id
                        .as_ref()
                        .is_some_and(|parent| parent.trim().is_empty())
            })
    };
    if review
        .historical_failure
        .as_ref()
        .is_some_and(historical_failure_invalid)
        || review
            .retry_reservation_prior_historical_failure
            .as_ref()
            .is_some_and(historical_failure_invalid)
    {
        return Err("historical completion review failure identity or proof is invalid".into());
    }
    let has_imported_source = review
        .active_attempt
        .as_ref()
        .and_then(|attempt| attempt.imported_from.as_ref())
        .is_some()
        || review
            .historical_failure
            .as_ref()
            .and_then(|failure| failure.imported_from.as_ref())
            .is_some()
        || review
            .retry_reservation_prior_historical_failure
            .as_ref()
            .and_then(|failure| failure.imported_from.as_ref())
            .is_some();
    if has_imported_source && review.previous_attempt_ref.is_none() {
        return Err("imported legacy failure has no prior attempt snapshot reference".into());
    }
    if review.retry_reservation_prior_historical_failure.is_some()
        && review.active_attempt.as_ref().is_none_or(|attempt| {
            attempt.consumed_retry_authorization.is_none()
                || !matches!(
                    attempt.status,
                    CompletionReviewAttemptStatus::Reserved
                        | CompletionReviewAttemptStatus::Unknown
                        | CompletionReviewAttemptStatus::Bound
                )
        })
    {
        return Err("retry rollback history is not attached to a reserved retry".into());
    }
    if let Some(attempt) = review.active_attempt.as_ref() {
        let Some(target) = unit.review_target.as_ref() else {
            return Err("completion review attempt has no declared target".into());
        };
        if attempt.attempt_id.trim().is_empty()
            || attempt.attempt_id.len() > MAX_TEXT_BYTES
            || attempt.reviewer_role.trim().is_empty()
            || attempt.reviewer_role.len() > MAX_TEXT_BYTES
            || attempt
                .reviewer_model
                .as_ref()
                .is_some_and(|model| model.trim().is_empty() || model.len() > MAX_TEXT_BYTES)
            || attempt.reviewer_model.is_none()
                != (attempt.status == CompletionReviewAttemptStatus::Failed
                    && attempt
                        .imported_from
                        .as_ref()
                        .is_some_and(|source| source.version == 1))
            || attempt
                .imported_from
                .as_ref()
                .is_some_and(|source| snapshot_ref_invalid(source, &[1]))
            || attempt
                .launch_unknown_reason
                .as_ref()
                .is_some_and(|reason| reason.trim().is_empty() || reason.len() > MAX_TEXT_BYTES)
            || attempt.contract_revision == 0
            || attempt.contract_revision > contract_revision
            || attempt.condition_refs != unit.condition_refs
            || attempt.conditions_sha256.len() != 64
            || attempt
                .conditions_sha256
                .bytes()
                .any(|byte| !byte.is_ascii_hexdigit() || byte.is_ascii_uppercase())
            || attempt.target_task_id != target.task_id
            || attempt.target_task_revision != target.task_revision
            || attempt.target_artifact_ref.trim().is_empty()
            || attempt.target_artifact_ref.len() > MAX_TEXT_BYTES
            || attempt
                .deadline_unix_ms
                .is_some_and(|deadline| deadline <= 0)
            || attempt
                .deadline_escalation
                .as_ref()
                .is_some_and(|reason| reason.trim().is_empty() || reason.len() > MAX_TEXT_BYTES)
            || attempt
                .deadline_safety_fingerprint
                .as_ref()
                .is_some_and(|value| {
                    value.len() != 64
                        || value
                            .bytes()
                            .any(|byte| !byte.is_ascii_hexdigit() || byte.is_ascii_uppercase())
                })
            || attempt
                .reviewer_thread_id
                .as_ref()
                .is_some_and(|id| id.trim().is_empty() || id.len() > MAX_TEXT_BYTES)
            || attempt
                .reviewer_turn_id
                .as_ref()
                .is_some_and(|id| id.trim().is_empty() || id.len() > MAX_TEXT_BYTES)
            || attempt.reviewer_turn_id.is_some() && attempt.reviewer_thread_id.is_none()
            || attempt.wait_result_ref.is_some() != attempt.wait_call_id.is_some()
            || attempt.terminal_source.as_ref().is_some_and(|source| {
                source.event.is_some() && source.status != CompletionReviewTerminalStatus::Failed
            })
            || attempt
                .terminal_source
                .as_ref()
                .and_then(|source| source.event.as_ref())
                .is_some_and(|event| {
                    event.rollout_id.trim().is_empty()
                        || attempt.reviewer_thread_id.as_deref()
                            != Some(event.owner_thread_id.as_str())
                        || attempt.reviewer_turn_id.as_deref() != Some(event.turn_id.as_str())
                        || event.owner_agent_role.as_deref() != Some(attempt.reviewer_role.as_str())
                        || event
                            .owner_parent_thread_id
                            .as_ref()
                            .is_some_and(|parent| parent.trim().is_empty())
                        || event.reason != "ReviewEnded"
                })
            || attempt
                .adoption_reason
                .as_ref()
                .is_some_and(|reason| reason.trim().is_empty() || reason.len() > MAX_TEXT_BYTES)
        {
            return Err("completion review attempt identity or target is invalid".into());
        }
        let terminal = attempt.terminal_source.as_ref().map(|source| source.status);
        match attempt.status {
            CompletionReviewAttemptStatus::Reserved
            | CompletionReviewAttemptStatus::Unknown
            | CompletionReviewAttemptStatus::Bound => {
                if attempt.terminal_source.is_some()
                    || attempt.wait_result_ref.is_some()
                    || attempt.adoption_reason.is_some()
                {
                    return Err("nonterminal review attempt contains terminal facts".into());
                }
            }
            CompletionReviewAttemptStatus::TerminalUnknown => {
                if attempt.terminal_source.is_some() || attempt.adoption_reason.is_some() {
                    return Err("unknown review state contains an unproven terminal fact".into());
                }
            }
            CompletionReviewAttemptStatus::Failed => {
                if terminal != Some(CompletionReviewTerminalStatus::Failed)
                    || attempt.adoption_reason.is_some()
                    || (attempt
                        .terminal_source
                        .as_ref()
                        .is_some_and(|source| source.event.is_none())
                        && attempt.wait_result_ref.is_none())
                {
                    return Err("failed review attempt has no exact failure fact".into());
                }
            }
            CompletionReviewAttemptStatus::Cancelled => {
                if terminal != Some(CompletionReviewTerminalStatus::Cancelled)
                    || attempt.adoption_reason.is_some()
                {
                    return Err("cancelled review attempt has no exact cancellation fact".into());
                }
            }
            CompletionReviewAttemptStatus::Completed => {
                if terminal != Some(CompletionReviewTerminalStatus::Completed)
                    || attempt.wait_result_ref.is_none()
                    || attempt.adoption_reason.is_some() != (unit.status == UnitStatus::Done)
                {
                    return Err(
                        "completed review attempt collection/adoption is inconsistent".into(),
                    );
                }
            }
        }
    }
    if let Some(grant) = review.retry_authorization.as_ref()
        && (grant.authorization_id.trim().is_empty()
            || grant.expected_attempt_id.trim().is_empty()
            || grant.plan_review_call_id.trim().is_empty()
            || grant.authorization_id != grant.plan_review_call_id
            || grant.reason.trim().is_empty()
            || grant.previous_plan.trim().is_empty()
            || grant.revised_plan.trim().is_empty()
            || grant.previous_plan == grant.revised_plan
            || review.active_attempt.as_ref().is_none_or(|attempt| {
                attempt.attempt_id != grant.expected_attempt_id
                    || attempt.status != CompletionReviewAttemptStatus::Failed
            }))
    {
        return Err("completion review retry authorization is invalid or stale".into());
    }
    if let Some(withdrawal) = review.withdrawal.as_ref() {
        let target = unit.review_target.as_ref();
        let source_invalid = match &withdrawal.source {
            CompletionReviewWithdrawalSource::HostUpdate {
                update_call_id,
                prior_status,
            } => update_call_id.trim().is_empty() || *prior_status != UnitStatus::Planned,
            CompletionReviewWithdrawalSource::LegacyV1Snapshot { snapshot } => {
                snapshot.version != 1
                    || snapshot.sha256.len() != 64
                    || snapshot
                        .sha256
                        .bytes()
                        .any(|byte| !byte.is_ascii_hexdigit() || byte.is_ascii_uppercase())
                    || review.previous_attempt_ref.as_ref() != Some(snapshot)
            }
        };
        if review.active_attempt.is_some()
            || review.historical_failure.is_some()
            || review.retry_authorization.is_some()
            || unit.status != UnitStatus::Cancelled
            || unit.kind != CompletionUnitKind::Review
            || unit.review_evidence.is_some()
            || unit.review_launch.is_some()
            || unit.review_decision_reason.as_deref() != Some(withdrawal.reason.as_str())
            || source_invalid
            || withdrawal.reason.trim().is_empty()
            || target.is_none_or(|target| {
                target.task_id != withdrawal.target_task_id
                    || target.task_revision != withdrawal.target_task_revision
            })
            || withdrawal.condition_refs != unit.condition_refs
            || withdrawal.depends_on != unit.depends_on
        {
            return Err(
                "completion review withdrawal fact does not match the retired obligation".into(),
            );
        }
    }
    Ok(())
}

/// Derive legacy completion fields from canonical review state. The canonical state is never
/// inferred from the compatibility projection here.
pub fn project_completion_review_state(unit: &CompletionUnit) -> Result<CompletionUnit, String> {
    let Some(review) = unit.review_state.as_ref() else {
        return Ok(unit.clone());
    };
    let mut projected = unit.clone();
    let Some(attempt) = review.active_attempt.as_ref() else {
        if let Some(withdrawal) = review.withdrawal.as_ref() {
            projected.review_evidence = None;
            projected.review_launch = None;
            projected.review_decision_reason = Some(withdrawal.reason.clone());
            projected.status = UnitStatus::Cancelled;
            return Ok(projected);
        }
        if !matches!(unit.status, UnitStatus::Planned | UnitStatus::Ready) {
            return Err(
                "canonical review without an active attempt must be unstarted or withdrawn".into(),
            );
        }
        projected.review_evidence = None;
        projected.review_launch = None;
        projected.review_decision_reason = None;
        projected.status = unit.status;
        return Ok(projected);
    };
    let evidence = match (
        attempt.reviewer_thread_id.as_ref(),
        attempt.reviewer_turn_id.as_ref(),
    ) {
        (Some(thread_id), Some(turn_id)) => Some(CompletionReviewEvidence {
            reviewer_thread_id: thread_id.clone(),
            reviewer_turn_id: turn_id.clone(),
            reviewer_role: attempt.reviewer_role.clone(),
            spawn_call_id: attempt.attempt_id.clone(),
            deadline_unix_ms: attempt.deadline_unix_ms,
            deadline_escalation: attempt.deadline_escalation.clone(),
            deadline_safety_fingerprint: attempt.deadline_safety_fingerprint.clone(),
            wait_result_ref: attempt.wait_result_ref.clone(),
            terminal_status: attempt.terminal_source.as_ref().map(|source| source.status),
            wait_call_id: attempt.wait_call_id.clone(),
            target_task_id: attempt.target_task_id.clone(),
            target_task_revision: attempt.target_task_revision,
            target_artifact_ref: attempt.target_artifact_ref.clone(),
        }),
        (Some(thread_id), None) => Some(CompletionReviewEvidence {
            reviewer_thread_id: thread_id.clone(),
            reviewer_turn_id: format!("pending-generation:{}", attempt.attempt_id),
            reviewer_role: attempt.reviewer_role.clone(),
            spawn_call_id: attempt.attempt_id.clone(),
            deadline_unix_ms: attempt.deadline_unix_ms,
            deadline_escalation: attempt.deadline_escalation.clone(),
            deadline_safety_fingerprint: attempt.deadline_safety_fingerprint.clone(),
            wait_result_ref: None,
            terminal_status: None,
            wait_call_id: None,
            target_task_id: attempt.target_task_id.clone(),
            target_task_revision: attempt.target_task_revision,
            target_artifact_ref: attempt.target_artifact_ref.clone(),
        }),
        _ => None,
    };
    projected.review_evidence = evidence;
    projected.review_launch = if projected.review_evidence.is_none()
        && matches!(
            attempt.status,
            CompletionReviewAttemptStatus::Reserved | CompletionReviewAttemptStatus::Unknown
        ) {
        Some(CompletionReviewLaunchState {
            spawn_call_id: attempt.attempt_id.clone(),
            status: if attempt.status == CompletionReviewAttemptStatus::Unknown {
                CompletionReviewLaunchStatus::Unknown
            } else {
                CompletionReviewLaunchStatus::Reserved
            },
            deadline_unix_ms: attempt.deadline_unix_ms,
            reason: attempt.launch_unknown_reason.clone(),
        })
    } else {
        None
    };
    projected.status = match attempt.status {
        CompletionReviewAttemptStatus::Reserved
        | CompletionReviewAttemptStatus::Unknown
        | CompletionReviewAttemptStatus::Bound
        | CompletionReviewAttemptStatus::TerminalUnknown => UnitStatus::Running,
        CompletionReviewAttemptStatus::Failed => UnitStatus::Failed,
        CompletionReviewAttemptStatus::Completed if attempt.adoption_reason.is_some() => {
            UnitStatus::Done
        }
        CompletionReviewAttemptStatus::Completed => UnitStatus::Running,
        // Attempt cancellation does not revoke the logical obligation.
        CompletionReviewAttemptStatus::Cancelled => UnitStatus::Blocked,
    };
    projected.review_decision_reason = attempt.adoption_reason.clone();
    Ok(projected)
}

/// Proves a newly authorized revalidation against the execution facts available at the time
/// of authorization. Historical readers should validate its stored provenance instead.
pub fn completion_review_target_has_exact_execution_dependency(
    state: &CompletionState,
    review: &CompletionUnit,
    required_artifact_fingerprint: &str,
) -> bool {
    let Some(target) = review.review_target.as_ref() else {
        return false;
    };
    !matching_review_execution_dependency_ids(
        state,
        review,
        target,
        Some(required_artifact_fingerprint),
    )
    .is_empty()
}

pub fn completion_review_revalidation_dependencies_match(
    source_state: &CompletionState,
    current_state: &CompletionState,
    source: &CompletionUnit,
    current: &CompletionUnit,
    required_artifact_fingerprint: &str,
) -> bool {
    completion_review_dependency_structure_matches(source_state, source, current)
        && current.review_target.as_ref().is_some_and(|target| {
            !matching_review_execution_dependency_ids(
                current_state,
                current,
                target,
                Some(required_artifact_fingerprint),
            )
            .is_empty()
        })
}

/// Confirms that a revalidation carries forward unrelated dependencies from its source.
pub fn completion_review_dependency_structure_matches(
    source_state: &CompletionState,
    source: &CompletionUnit,
    current: &CompletionUnit,
) -> bool {
    let Some(source_target) = source.review_target.as_ref() else {
        return false;
    };
    if current.review_target.is_none() {
        return false;
    }
    let source_dependencies =
        matching_review_execution_dependency_ids(source_state, source, source_target, None);
    let mut unrelated_source_dependencies = source
        .depends_on
        .iter()
        .filter(|dependency| !source_dependencies.contains(dependency));
    unrelated_source_dependencies.all(|dependency| current.depends_on.contains(dependency))
}

fn matching_review_execution_dependency_ids(
    state: &CompletionState,
    review: &CompletionUnit,
    target: &CompletionReviewTarget,
    required_artifact_fingerprint: Option<&str>,
) -> Vec<String> {
    review
        .depends_on
        .iter()
        .filter_map(|dependency_id| {
            let dependency = state
                .units
                .iter()
                .find(|unit| unit.id.as_str() == dependency_id.as_str())?;
            let has_target_evidence = dependency
                .task_refs
                .iter()
                .chain(dependency.evidence.iter())
                .any(|reference| {
                    reference.task_id == target.task_id
                        && reference.task_revision == target.task_revision
                        && required_artifact_fingerprint.is_none_or(|fingerprint| {
                            reference.artifact_ref == reference.verification_ref
                                && task_check_artifact_fingerprint(&reference.artifact_ref)
                                    == Some(fingerprint)
                        })
                });
            (dependency.kind == CompletionUnitKind::Execution
                && (required_artifact_fingerprint.is_none()
                    || dependency.status == UnitStatus::Done)
                && has_target_evidence)
                .then(|| dependency_id.clone())
        })
        .collect()
}

pub fn validate_completion_state(
    state: Option<&CompletionState>,
    receipts: &BTreeMap<String, AcceptedTaskReceipt>,
) -> Result<(), String> {
    validate_completion_state_with_history(state, receipts, &[])
}

/// Validate current proof against latest receipts and unit history against exact prior receipts.
pub fn validate_completion_state_with_history(
    state: Option<&CompletionState>,
    receipts: &BTreeMap<String, AcceptedTaskReceipt>,
    history: &[AcceptedTaskEvidenceReceipt],
) -> Result<(), String> {
    validate_completion_state_inner(state, receipts, history, true)
}

/// Validate an immutable host-authored snapshot without comparing its historical evidence
/// references to the latest accepted receipt for each task.
pub fn validate_completion_state_snapshot(
    state: Option<&CompletionState>,
    receipts: &BTreeMap<String, AcceptedTaskReceipt>,
) -> Result<(), String> {
    validate_completion_state_inner(state, receipts, &[], false)
}

fn validate_completion_state_inner(
    state: Option<&CompletionState>,
    receipts: &BTreeMap<String, AcceptedTaskReceipt>,
    history: &[AcceptedTaskEvidenceReceipt],
    enforce_current_evidence_receipts: bool,
) -> Result<(), String> {
    let Some(state) = state else {
        return Ok(());
    };
    if state.contract_revision == 0
        || state.conditions.is_empty()
        || state.conditions.len() > 128
        || state.units.len() > 256
    {
        return Err("completion state revision or bounded collection size is invalid".into());
    }
    if serde_json::to_vec(state)
        .map_err(|error| error.to_string())?
        .len()
        > MAX_COMPLETION_CHECKPOINT_BYTES
    {
        return Err("completion state exceeds checkpoint size limit".into());
    }
    let mut conditions = BTreeMap::new();
    for condition in &state.conditions {
        if !condition.id.starts_with("AC-")
            || condition.id.len() > MAX_TEXT_BYTES
            || condition.revision == 0
            || conditions
                .insert(condition.id.as_str(), condition)
                .is_some()
        {
            return Err(format!(
                "acceptance condition identity is invalid: IDs must be unique, start with AC-, and be at most {MAX_TEXT_BYTES} bytes; revision must be positive"
            ));
        }
        for (field, text) in [
            ("result", &condition.result),
            ("scope", &condition.scope),
            ("verification", &condition.verification),
            ("acceptance_criteria", &condition.acceptance_criteria),
        ] {
            if text.trim().is_empty() || text.len() > MAX_TEXT_BYTES {
                return Err(format!(
                    "acceptance condition {} field `{field}` must be non-empty and at most {MAX_TEXT_BYTES} bytes",
                    condition.id
                ));
            }
        }
        if condition.status == AcceptanceStatus::Satisfied && condition.evidence.is_empty() {
            return Err("satisfied condition has no canonical evidence".into());
        }
        for evidence in &condition.evidence {
            if enforce_current_evidence_receipts {
                validate_current_completion_evidence(evidence, receipts)?;
            } else {
                validate_completion_evidence(evidence, receipts, history, false)?;
            }
        }
        if condition
            .unmet_reason
            .as_ref()
            .is_some_and(|reason| reason.trim().is_empty() || reason.len() > MAX_TEXT_BYTES)
        {
            return Err("condition unmet reason is invalid".into());
        }
    }
    let mut units = BTreeSet::new();
    let by_unit: BTreeMap<_, _> = state
        .units
        .iter()
        .map(|unit| (unit.id.as_str(), unit))
        .collect();
    for unit in &state.units {
        if unit.id.trim().is_empty()
            || unit.id.len() > MAX_TEXT_BYTES
            || unit.purpose.trim().is_empty()
            || unit.purpose.len() > MAX_TEXT_BYTES
            || !units.insert(unit.id.as_str())
            || unit.condition_refs.is_empty()
            || unit
                .condition_refs
                .iter()
                .any(|id| !conditions.contains_key(id.as_str()))
        {
            return Err("completion unit identity or condition reference is invalid".into());
        }
        match unit.kind {
            CompletionUnitKind::Execution => {
                if unit.review_target.is_some()
                    || unit.review_evidence.is_some()
                    || unit.review_launch.is_some()
                    || unit.review_decision_reason.is_some()
                    || unit.review_state.is_some()
                    || unit.review_revalidation_of.is_some()
                {
                    return Err("execution unit contains review-only fields".into());
                }
            }
            CompletionUnitKind::Review => {
                let Some(target) = unit.review_target.as_ref() else {
                    return Err("review unit has no target task revision".into());
                };
                if target.task_id.trim().is_empty()
                    || target.task_id.len() > MAX_TEXT_BYTES
                    || target.task_revision == 0
                {
                    return Err("review unit target task revision is invalid".into());
                }
                if let Some(review_state) = unit.review_state.as_ref() {
                    validate_completion_review_state(unit, review_state, state.contract_revision)?;
                    let projected = project_completion_review_state(unit)?;
                    if projected.status != unit.status
                        || projected.review_evidence != unit.review_evidence
                        || projected.review_launch != unit.review_launch
                        || projected.review_decision_reason != unit.review_decision_reason
                    {
                        return Err(
                            "review compatibility fields differ from canonical review state".into(),
                        );
                    }
                }
                if unit.review_revalidation_of.is_some()
                    != unit
                        .review_state
                        .as_ref()
                        .is_some_and(|review| review.revalidation.is_some())
                {
                    return Err(
                        "review revalidation intent is detached from its host provenance".into(),
                    );
                }
                if let Some(evidence) = unit.review_evidence.as_ref()
                    && ([
                        &evidence.reviewer_thread_id,
                        &evidence.reviewer_turn_id,
                        &evidence.reviewer_role,
                        &evidence.spawn_call_id,
                        &evidence.target_task_id,
                        &evidence.target_artifact_ref,
                    ]
                    .iter()
                    .any(|text| text.trim().is_empty() || text.len() > MAX_TEXT_BYTES)
                        || evidence.wait_result_ref.as_ref().is_some_and(|result_ref| {
                            result_ref.trim().is_empty() || result_ref.len() > MAX_TEXT_BYTES
                        })
                        || evidence.wait_call_id.as_ref().is_some_and(|call_id| {
                            call_id.trim().is_empty() || call_id.len() > MAX_TEXT_BYTES
                        })
                        || evidence
                            .deadline_unix_ms
                            .is_some_and(|deadline| deadline <= 0)
                        || evidence.deadline_escalation.as_ref().is_some_and(|reason| {
                            reason.trim().is_empty() || reason.len() > MAX_TEXT_BYTES
                        })
                        || evidence
                            .deadline_safety_fingerprint
                            .as_ref()
                            .is_some_and(|value| {
                                value.len() != 64
                                    || value.bytes().any(|byte| {
                                        !byte.is_ascii_hexdigit() || byte.is_ascii_uppercase()
                                    })
                            })
                        || (evidence.deadline_escalation.is_some()
                            && evidence.deadline_unix_ms.is_none())
                        || evidence.wait_result_ref.is_some() != evidence.wait_call_id.is_some()
                        || evidence.target_task_id != target.task_id
                        || evidence.target_task_revision != target.task_revision
                        || evidence.target_artifact_ref.trim().is_empty())
                {
                    return Err("review evidence does not match its host-accepted target".into());
                }
                if unit.status == UnitStatus::Done
                    && (unit
                        .review_evidence
                        .as_ref()
                        .and_then(|evidence| evidence.wait_result_ref.as_ref())
                        .is_none_or(|wait_ref| wait_ref.trim().is_empty())
                        || unit
                            .review_evidence
                            .as_ref()
                            .and_then(|evidence| evidence.terminal_status)
                            != Some(CompletionReviewTerminalStatus::Completed)
                        || unit
                            .review_evidence
                            .as_ref()
                            .and_then(|evidence| evidence.wait_call_id.as_ref())
                            .is_none_or(|call_id| call_id.trim().is_empty())
                        || unit
                            .review_decision_reason
                            .as_ref()
                            .is_none_or(|reason| reason.trim().is_empty()))
                {
                    return Err(
                        "completed review unit needs collected review evidence and parent decision"
                            .into(),
                    );
                }
                if unit
                    .review_decision_reason
                    .as_ref()
                    .is_some_and(|reason| reason.trim().is_empty() || reason.len() > MAX_TEXT_BYTES)
                {
                    return Err("review decision reason is invalid".into());
                }
                if let Some(launch) = unit.review_launch.as_ref()
                    && (unit.status != UnitStatus::Running
                        || unit.review_evidence.is_some()
                        || launch.spawn_call_id.trim().is_empty()
                        || launch.spawn_call_id.len() > MAX_TEXT_BYTES
                        || launch.reason.as_ref().is_some_and(|reason| {
                            reason.trim().is_empty() || reason.len() > MAX_TEXT_BYTES
                        })
                        || launch
                            .deadline_unix_ms
                            .is_some_and(|deadline| deadline <= 0)
                        || (launch.status == CompletionReviewLaunchStatus::Unknown
                            && launch.reason.is_none()))
                {
                    return Err(
                        "review launch state is invalid or conflicts with child evidence".into(),
                    );
                }
            }
        }
        for evidence in unit.evidence.iter().chain(&unit.task_refs) {
            validate_completion_evidence(
                evidence,
                receipts,
                history,
                enforce_current_evidence_receipts,
            )?;
        }
        if unit.kind == CompletionUnitKind::Execution
            && unit.status == UnitStatus::Done
            && unit.evidence.is_empty()
            && unit.task_refs.is_empty()
        {
            return Err("completed unit has no canonical evidence".into());
        }
    }
    let mut active_revalidation_sources = BTreeSet::new();
    for unit in &state.units {
        let Some(source_id) = unit.review_revalidation_of.as_deref() else {
            continue;
        };
        let Some(source) = by_unit.get(source_id) else {
            return Err("review revalidation source unit is missing".into());
        };
        let source_proof = unit
            .review_state
            .as_ref()
            .and_then(|review| review.revalidation.as_ref())
            .ok_or_else(|| "review revalidation provenance is missing".to_string())?;
        let source_digest = completion_review_adoption_sha256(source)?;
        let source_artifact_fingerprint = source
            .review_evidence
            .as_ref()
            .and_then(|evidence| task_check_artifact_fingerprint(&evidence.target_artifact_ref));
        if unit.kind != CompletionUnitKind::Review
            || unit.id == source_id
            || source.kind != CompletionUnitKind::Review
            || unit.review_target.is_none()
            || source.review_target.is_none()
            || source_proof.source_unit_id != source_id
            || source_proof.source_adoption_sha256 != source_digest
            || source_proof.update_call_id.trim().is_empty()
            || source_proof.source_condition_definitions_sha256.len() != 64
            || source_proof
                .source_condition_definitions_sha256
                .bytes()
                .any(|byte| !byte.is_ascii_hexdigit() || byte.is_ascii_uppercase())
            || source_proof.required_artifact_fingerprint.len() != 64
            || source_proof
                .required_artifact_fingerprint
                .bytes()
                .any(|byte| !byte.is_ascii_hexdigit() || byte.is_ascii_uppercase())
            || source_proof.required_condition_definitions_sha256.len() != 64
            || source_proof
                .required_condition_definitions_sha256
                .bytes()
                .any(|byte| !byte.is_ascii_hexdigit() || byte.is_ascii_uppercase())
            || (Some(source_proof.required_artifact_fingerprint.as_str())
                == source_artifact_fingerprint
                && source_proof.source_condition_definitions_sha256
                    == source_proof.required_condition_definitions_sha256)
            || source
                .review_state
                .as_ref()
                .and_then(|review| review.active_attempt.as_ref())
                .is_some_and(|attempt| {
                    attempt.conditions_sha256 != source_proof.source_condition_definitions_sha256
                })
            || source_proof.source_snapshot.version != 1
                && source_proof.source_snapshot.version != 2
            || source_proof.source_snapshot.sha256.len() != 64
            || source_proof
                .source_snapshot
                .sha256
                .bytes()
                .any(|byte| !byte.is_ascii_hexdigit() || byte.is_ascii_uppercase())
            || unit.condition_refs != source.condition_refs
            || unit.status == UnitStatus::Cancelled
                && unit
                    .review_state
                    .as_ref()
                    .is_none_or(|review| review.withdrawal.is_none())
        {
            return Err("review revalidation source, scope, or host proof is invalid".into());
        }
        if unit.status != UnitStatus::Cancelled && !active_revalidation_sources.insert(source_id) {
            return Err("adopted review obligation has multiple active revalidation leaves".into());
        }
        if unit
            .review_state
            .as_ref()
            .and_then(|review| review.active_attempt.as_ref())
            .is_some_and(|attempt| {
                task_check_artifact_fingerprint(&attempt.target_artifact_ref)
                    != Some(source_proof.required_artifact_fingerprint.as_str())
                    || attempt.conditions_sha256
                        != source_proof.required_condition_definitions_sha256
            })
        {
            return Err("revalidation attempt is detached from its host-bound artifact".into());
        }
        let mut visited = BTreeSet::new();
        let mut cursor = Some(source_id);
        while let Some(current) = cursor {
            if current == unit.id || !visited.insert(current) {
                return Err("review revalidation relation contains a cycle".into());
            }
            cursor = by_unit
                .get(current)
                .and_then(|ancestor| ancestor.review_revalidation_of.as_deref());
        }
    }
    if state.units.iter().any(|unit| {
        unit.depends_on
            .iter()
            .any(|id| !units.contains(id.as_str()))
    }) {
        return Err("completion unit dependency is missing".into());
    }
    for owner in &state.units {
        let grant = owner
            .review_state
            .as_ref()
            .and_then(|review| review.retry_authorization.as_ref())
            .or_else(|| {
                owner
                    .review_state
                    .as_ref()
                    .and_then(|review| review.active_attempt.as_ref())
                    .and_then(|attempt| attempt.consumed_retry_authorization.as_ref())
            });
        if let Some(grant) = grant
            && !state.plan_reviews.iter().any(|record| {
                record.previous_plan == grant.previous_plan
                    && record.revised_plan == grant.revised_plan
                    && record.reason == grant.reason
            })
        {
            return Err("retry grant has no matching recorded plan review decision".into());
        }
    }
    fn visit_unit<'a>(
        id: &'a str,
        map: &BTreeMap<&'a str, &'a CompletionUnit>,
        active: &mut BTreeSet<&'a str>,
        done: &mut BTreeSet<&'a str>,
    ) -> bool {
        if done.contains(id) {
            return true;
        }
        if !active.insert(id) {
            return false;
        }
        if map[id]
            .depends_on
            .iter()
            .all(|dependency| visit_unit(dependency, map, active, done))
        {
            active.remove(id);
            done.insert(id);
            true
        } else {
            false
        }
    }
    let (mut active, mut done) = (BTreeSet::new(), BTreeSet::new());
    if !by_unit
        .keys()
        .all(|id| visit_unit(id, &by_unit, &mut active, &mut done))
    {
        return Err("completion unit dependency cycle".into());
    }
    for unit in &state.units {
        if unit.status != UnitStatus::Planned
            && unit
                .depends_on
                .iter()
                .any(|dependency| by_unit[dependency.as_str()].status != UnitStatus::Done)
        {
            return Err("active completion unit has an unfinished dependency".into());
        }
    }
    if state.consumed_host_events.len() > 256
        || state
            .consumed_host_events
            .iter()
            .any(|event| event.trim().is_empty() || event.len() > MAX_TEXT_BYTES)
        || state
            .consumed_host_events
            .iter()
            .collect::<BTreeSet<_>>()
            .len()
            != state.consumed_host_events.len()
    {
        return Err("consumed host event list is invalid".into());
    }
    if state
        .current_unit
        .as_ref()
        .is_some_and(|id| !units.contains(id.as_str()))
    {
        return Err("current completion unit is missing".into());
    }
    for text in [
        state.next_action.as_ref(),
        state.blocker.as_ref().map(|blocker| &blocker.reason),
        state
            .blocker
            .as_ref()
            .and_then(|blocker| blocker.required_input.as_ref()),
        state
            .blocker
            .as_ref()
            .and_then(|blocker| blocker.authority.as_ref()),
        state
            .blocker
            .as_ref()
            .and_then(|blocker| blocker.recheck_condition.as_ref()),
    ]
    .into_iter()
    .flatten()
    {
        if text.trim().is_empty() || text.len() > MAX_TEXT_BYTES {
            return Err("completion continuation text is invalid".into());
        }
    }
    if let Some(blocker) = &state.blocker {
        if let Some(owner) = blocker.review_owner.as_ref()
            && (owner.unit_id.trim().is_empty()
                || owner.attempt_id.trim().is_empty()
                || owner.safety_fingerprint.len() != 64
                || owner
                    .safety_fingerprint
                    .bytes()
                    .any(|byte| !byte.is_ascii_hexdigit() || byte.is_ascii_uppercase())
                || by_unit.get(owner.unit_id.as_str()).is_none_or(|unit| {
                    unit.review_state
                        .as_ref()
                        .and_then(|review| review.active_attempt.as_ref())
                        .is_none_or(|attempt| {
                            attempt.attempt_id != owner.attempt_id
                                || attempt.deadline_safety_fingerprint.as_deref()
                                    != Some(owner.safety_fingerprint.as_str())
                        })
                }))
        {
            return Err("review-owned blocker does not match its active host attempt".into());
        }
        if blocker.evidence.iter().any(|evidence| {
            if enforce_current_evidence_receipts {
                validate_current_completion_evidence(evidence, receipts).is_err()
            } else {
                validate_completion_evidence(evidence, receipts, history, false).is_err()
            }
        }) {
            return Err("completion blocker evidence is invalid".into());
        }
        if blocker.category == BlockerCategory::UserInput && blocker.required_input.is_none() {
            return Err("user-input blocker omits required input".into());
        }
        if blocker.category == BlockerCategory::Authority && blocker.authority.is_none() {
            return Err("authority blocker omits authority boundary".into());
        }
        if blocker.category == BlockerCategory::ExternalService
            && blocker.recheck_condition.is_none()
        {
            return Err("external-service blocker omits its recheck condition".into());
        }
    }
    if state.plan_reviews.len() > 128
        || state.plan_reviews.iter().any(|record| {
            record.at < 0
                || [&record.previous_plan, &record.revised_plan, &record.reason]
                    .iter()
                    .any(|text| text.trim().is_empty() || text.len() > MAX_TEXT_BYTES)
        })
        || state.failed_approaches.len() > 128
        || state.failed_approaches.iter().any(|entry| {
            [&entry.summary, &entry.cause, &entry.next_action]
                .iter()
                .any(|text| text.trim().is_empty() || text.len() > MAX_TEXT_BYTES)
                || entry.recorded_at < 0
        })
        || [
            state.product_progress_at,
            state.runtime_recovery_at,
            state.activity_at,
            state.plan_review_at,
        ]
        .into_iter()
        .flatten()
        .any(|time| time < 0)
    {
        return Err("completion history or timestamp is invalid".into());
    }
    Ok(())
}

fn validate_completion_evidence(
    evidence: &CompletionEvidenceRef,
    receipts: &BTreeMap<String, AcceptedTaskReceipt>,
    history: &[AcceptedTaskEvidenceReceipt],
    enforce_current_receipt: bool,
) -> Result<(), String> {
    if evidence.task_id.trim().is_empty()
        || evidence.task_revision == 0
        || evidence.artifact_ref.trim().is_empty()
        || evidence.verification_ref.trim().is_empty()
    {
        return Err("completion evidence identity is invalid".into());
    }
    if !enforce_current_receipt {
        return Ok(());
    }
    match resolve_accepted_task_evidence(evidence, receipts, history) {
        Some(
            AcceptedTaskEvidenceResolution::Current(_)
            | AcceptedTaskEvidenceResolution::Historical(_),
        ) => Ok(()),
        Some(AcceptedTaskEvidenceResolution::CurrentRevision(_)) | None => {
            Err("completion evidence is stale or does not match an accepted host receipt".into())
        }
    }
}

fn validate_current_completion_evidence(
    evidence: &CompletionEvidenceRef,
    receipts: &BTreeMap<String, AcceptedTaskReceipt>,
) -> Result<(), String> {
    if evidence.task_id.trim().is_empty()
        || evidence.task_revision == 0
        || evidence.artifact_ref.trim().is_empty()
        || evidence.verification_ref.trim().is_empty()
    {
        return Err("completion evidence identity is invalid".into());
    }
    if matches!(
        resolve_accepted_task_evidence(evidence, receipts, &[]),
        Some(AcceptedTaskEvidenceResolution::Current(_))
    ) {
        return Ok(());
    }
    Err("completion evidence is stale or does not match the current accepted host receipt".into())
}

fn validate_size_and_text(candidate: &BoundaryCandidate) -> Result<(), BoundaryError> {
    let encoded = serde_json::to_vec(candidate).map_err(|_| BoundaryError::InvalidTasks)?;
    if encoded.len() > MAX_BOUNDARY_BYTES {
        return Err(BoundaryError::Oversized);
    }
    validate_boundary_text(candidate)
}

fn validate_host_size_and_text(candidate: &BoundaryCandidate) -> Result<(), BoundaryError> {
    let mut generic_boundary = candidate.clone();
    generic_boundary.completion_state = None;
    let generic_encoded =
        serde_json::to_vec(&generic_boundary).map_err(|_| BoundaryError::InvalidTasks)?;
    if generic_encoded.len() > MAX_BOUNDARY_BYTES {
        return Err(BoundaryError::Oversized);
    }
    let encoded = serde_json::to_vec(candidate).map_err(|_| BoundaryError::InvalidTasks)?;
    if encoded.len() > MAX_COMPLETION_CHECKPOINT_BYTES {
        return Err(BoundaryError::Oversized);
    }
    validate_boundary_text(candidate)
}

fn validate_boundary_text(candidate: &BoundaryCandidate) -> Result<(), BoundaryError> {
    let required = [
        &candidate.thread_id,
        &candidate.root_task_id,
        &candidate.closed_task_id,
        &candidate.goal,
        &candidate.artifact_ref,
        &candidate.verification_ref,
    ];
    if required.iter().any(|s| s.trim().is_empty())
        || required.iter().any(|s| s.len() > MAX_TEXT_BYTES)
        || candidate
            .tasks
            .iter()
            .any(|t| t.id.len() > MAX_TEXT_BYTES || t.purpose.len() > MAX_TEXT_BYTES)
        || candidate
            .decisions
            .iter()
            .chain(&candidate.constraints)
            .any(|s| s.trim().is_empty() || s.len() > MAX_TEXT_BYTES)
    {
        return Err(BoundaryError::EmptyField);
    }
    Ok(())
}

fn validate_tasks(candidate: &BoundaryCandidate) -> Result<(), BoundaryError> {
    let by_id: BTreeMap<_, _> = candidate.tasks.iter().map(|t| (t.id.as_str(), t)).collect();
    if by_id.len() != candidate.tasks.len()
        || candidate
            .tasks
            .iter()
            .any(|t| t.id.trim().is_empty() || t.purpose.trim().is_empty())
    {
        return Err(BoundaryError::InvalidTasks);
    }
    for task in &candidate.tasks {
        if task
            .depends_on
            .iter()
            .any(|d| !by_id.contains_key(d.as_str()))
        {
            return Err(BoundaryError::DependencyMissing);
        }
    }
    fn visit<'a>(
        id: &'a str,
        map: &BTreeMap<&'a str, &'a TaskState>,
        active: &mut BTreeSet<&'a str>,
        done: &mut BTreeSet<&'a str>,
    ) -> bool {
        if done.contains(id) {
            return true;
        }
        if !active.insert(id) {
            return false;
        }
        if map[id]
            .depends_on
            .iter()
            .all(|d| visit(d, map, active, done))
        {
            active.remove(id);
            done.insert(id);
            true
        } else {
            false
        }
    }
    let (mut active, mut done) = (BTreeSet::new(), BTreeSet::new());
    if !by_id
        .keys()
        .all(|id| visit(id, &by_id, &mut active, &mut done))
    {
        return Err(BoundaryError::DependencyCycle);
    }
    for task in &candidate.tasks {
        if task
            .depends_on
            .iter()
            .any(|dependency| by_id[dependency.as_str()].status != TaskStatus::Completed)
        {
            return Err(BoundaryError::InvalidTasks);
        }
    }
    if by_id
        .get(candidate.closed_task_id.as_str())
        .is_none_or(|t| t.status != TaskStatus::Completed)
    {
        return Err(BoundaryError::ClosedTaskNotCompleted);
    }
    if let Some(current) = &candidate.current_task_id
        && by_id.get(current.as_str()).is_none_or(|t| {
            !matches!(
                t.status,
                TaskStatus::Pending | TaskStatus::InProgress | TaskStatus::Blocked
            )
        })
    {
        return Err(BoundaryError::InvalidTasks);
    }
    if candidate.current_task_id.is_none()
        && candidate
            .tasks
            .iter()
            .any(|t| t.status != TaskStatus::Completed)
    {
        return Err(BoundaryError::InvalidTasks);
    }
    Ok(())
}

/// Build a deterministic, compact derived context. Constraints are labeled as quoted source,
/// so they remain preserved without being promoted into trusted instructions.
pub fn render_task_context(boundary: &ValidatedBoundary) -> String {
    let c = &boundary.candidate;
    let completion_state_reference = c.completion_state.as_ref().and_then(|state| {
        let canonical_state = serde_json::to_string(state).ok()?;
        Some(serde_json::json!({
            "root_task_id": c.root_task_id,
            "contract_revision": state.contract_revision,
            "canonical_state_sha256": crate::archive_reference::content_sha256(&canonical_state),
            "paused": state.paused,
            "current_unit": state.current_unit,
            "next_action": state.next_action,
            "conditions": state.conditions.iter().map(|condition| {
                serde_json::json!({"id": condition.id, "status": condition.status})
            }).collect::<Vec<_>>(),
            "units": state.units.iter().map(|unit| {
                serde_json::json!({"id": unit.id, "kind": unit.kind, "status": unit.status})
            }).collect::<Vec<_>>(),
            "status_lookup": "Call task_context_boundary status for full host state.",
        }))
    });
    let completed = c.tasks.iter().filter(|t| t.status == TaskStatus::Completed).map(|t| {
        if t.id == c.closed_task_id {
            serde_json::json!({"id":t.id,"purpose":t.purpose,"result":boundary_current_result(boundary),"artifact_ref":c.artifact_ref,"verification_ref":c.verification_ref,"parent_review_ref":boundary.parent_review_ref})
        } else {
            let receipt = &boundary.completed[&t.id];
            serde_json::json!({"id":t.id,"purpose":t.purpose,"result":receipt.result,"artifact_ref":receipt.artifact_ref,"verification_ref":receipt.verification_ref})
        }
    }).collect::<Vec<_>>();
    serde_json::json!({"format":"task_context_v1","goal":c.goal,"completed":completed,
        "remaining":c.tasks.iter().filter(|t| t.status != TaskStatus::Completed).collect::<Vec<_>>(),
        "current_task_id":c.current_task_id,"decisions":c.decisions,"constraints_as_source":c.constraints,
        "completion_state":completion_state_reference}).to_string()
}

fn boundary_current_result(boundary: &ValidatedBoundary) -> &str {
    // The host-resolved result is copied into this private snapshot during validation.
    &boundary.current_result
}

#[cfg(test)]
#[path = "task_context_tests.rs"]
mod tests;
