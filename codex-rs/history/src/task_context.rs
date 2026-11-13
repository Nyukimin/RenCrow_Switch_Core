//! Typed, host-verified task-boundary state and deterministic derived context.

use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

const MAX_BOUNDARY_BYTES: usize = 32 * 1024;
const MAX_TEXT_BYTES: usize = 4096;

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
    #[serde(default, skip_serializing_if = "is_execution_unit")]
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
}

fn is_execution_unit(kind: &CompletionUnitKind) -> bool {
    *kind == CompletionUnitKind::Execution
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
    current_result: String,
}

/// Host-accepted task state persisted in a V3 checkpoint. This is intentionally distinct from
/// the model-facing boundary proposal and contains no host authorization facts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredTaskState {
    pub boundary: BoundaryCandidate,
    pub completed: BTreeMap<String, AcceptedTaskReceipt>,
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
        let mut completed = self.completed;
        completed.insert(
            self.candidate.closed_task_id.clone(),
            AcceptedTaskReceipt {
                // The completion receipt is scoped to the closed task, not the board snapshot.
                revision: self.candidate.closed_task_revision,
                result: self.current_result,
                artifact_ref: self.candidate.artifact_ref.clone(),
                verification_ref: self.candidate.verification_ref.clone(),
            },
        );
        let candidate_hash = candidate_hash(&self.candidate)?;
        let state = StoredTaskState {
            boundary: self.candidate,
            completed,
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
        validate_size_and_text(&self.boundary)
            .map_err(|error| format!("invalid stored task state: {error:?}"))?;
        if serde_json::to_vec(self)
            .map_err(|error| error.to_string())?
            .len()
            > MAX_BOUNDARY_BYTES
        {
            return Err("stored task state exceeds the checkpoint size limit".into());
        }
        validate_tasks(&self.boundary)
            .map_err(|error| format!("invalid stored task state: {error:?}"))?;
        validate_completion_state(self.boundary.completion_state.as_ref(), &self.completed)
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
    validate_size_and_text(&candidate)?;
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
    validate_completion_state(candidate.completion_state.as_ref(), &receipts)
        .map_err(|_| BoundaryError::InvalidCompletionState)?;
    Ok(ValidatedBoundary {
        candidate,
        parent_review_ref: review,
        completed: facts.accepted_completed.clone(),
        current_result,
    })
}

pub fn validate_completion_state(
    state: Option<&CompletionState>,
    receipts: &BTreeMap<String, AcceptedTaskReceipt>,
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
        > MAX_BOUNDARY_BYTES
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
            return Err("acceptance condition identity is invalid".into());
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
            validate_completion_evidence(evidence, receipts)?;
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
                if let Some(evidence) = unit.review_evidence.as_ref() {
                    let receipt = receipts
                        .get(&evidence.target_task_id)
                        .filter(|receipt| receipt.revision == evidence.target_task_revision);
                    if [
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
                        || evidence.target_artifact_ref.trim().is_empty()
                        || receipt.is_none_or(|receipt| {
                            receipt.artifact_ref != evidence.target_artifact_ref
                        })
                    {
                        return Err(
                            "review evidence does not match its host-accepted target".into()
                        );
                    }
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
            validate_completion_evidence(evidence, receipts)?;
        }
        if unit.kind == CompletionUnitKind::Execution
            && unit.status == UnitStatus::Done
            && unit.evidence.is_empty()
            && unit.task_refs.is_empty()
        {
            return Err("completed unit has no canonical evidence".into());
        }
    }
    if state.units.iter().any(|unit| {
        unit.depends_on
            .iter()
            .any(|id| !units.contains(id.as_str()))
    }) {
        return Err("completion unit dependency is missing".into());
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
        if blocker
            .evidence
            .iter()
            .any(|evidence| validate_completion_evidence(evidence, receipts).is_err())
        {
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
) -> Result<(), String> {
    let Some(receipt) = receipts.get(&evidence.task_id) else {
        return Err("completion evidence has no host receipt".into());
    };
    if receipt.revision != evidence.task_revision
        || receipt.artifact_ref != evidence.artifact_ref
        || receipt.verification_ref != evidence.verification_ref
    {
        return Err("completion evidence is stale or does not match its host receipt".into());
    }
    Ok(())
}

fn validate_size_and_text(candidate: &BoundaryCandidate) -> Result<(), BoundaryError> {
    let encoded = serde_json::to_vec(candidate).map_err(|_| BoundaryError::InvalidTasks)?;
    if encoded.len() > MAX_BOUNDARY_BYTES {
        return Err(BoundaryError::Oversized);
    }
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
        "completion_state":c.completion_state}).to_string()
}

fn boundary_current_result(boundary: &ValidatedBoundary) -> &str {
    // The host-resolved result is copied into this private snapshot during validation.
    &boundary.current_result
}

#[cfg(test)]
#[path = "task_context_tests.rs"]
mod tests;
