//! Host-side blocker recovery mutation and lifecycle helpers.
// Modified by RenCrow Switch Core, 2026-11-11.

use super::session::Session;
use crate::task_check::TaskCheckAttempt;
use crate::task_check::TaskCheckAttemptStatus;
use codex_history::task_context::BlockerCategory;
use codex_history::task_context::CompletionEvidenceRef;
use codex_history::task_context::CompletionState;
use codex_history::task_context::UnitStatus;
use tokio_util::task::AbortOnDropHandle;

const MAX_COMPLETION_NEXT_ACTION_BYTES: usize = 4096;

impl Session {
    pub(crate) async fn reconcile_rencrow_completion_evidence(
        &self,
        expected_root_task_id: &str,
    ) -> Result<bool, String> {
        loop {
            let Some((root_task_id, expected)) = self.autonomous_completion_current_state().await
            else {
                return Ok(false);
            };
            if root_task_id != expected_root_task_id {
                return Ok(false);
            }

            let mut invalidated = Vec::new();
            for condition in expected.conditions.iter().filter(|condition| {
                condition.status == codex_history::task_context::AcceptanceStatus::Satisfied
            }) {
                let mut condition_is_current = !condition.evidence.is_empty();
                for evidence in &condition.evidence {
                    if !self
                        .completion_evidence_matches_current_artifact(evidence)
                        .await
                    {
                        condition_is_current = false;
                        break;
                    }
                }
                if !condition_is_current {
                    invalidated.push(condition.id.clone());
                }
            }
            if invalidated.is_empty() {
                return Ok(false);
            }

            let mut invalidated_units = Vec::with_capacity(invalidated.len());
            {
                let mut state = self.state.lock().await;
                let Some((root, current)) = state.rencrow_completion_state.as_mut() else {
                    return Ok(false);
                };
                if root != &root_task_id {
                    return Ok(false);
                }
                if current != &expected {
                    continue;
                }
                for condition_id in &invalidated {
                    let Some(condition) = current
                        .conditions
                        .iter_mut()
                        .find(|condition| condition.id == *condition_id)
                    else {
                        continue;
                    };
                    if condition.status != codex_history::task_context::AcceptanceStatus::Satisfied
                    {
                        continue;
                    }
                    condition.status = codex_history::task_context::AcceptanceStatus::Open;
                    condition.unmet_reason = Some(
                        "Previously accepted evidence no longer matches the current artifact revision."
                            .into(),
                    );
                    let task_id = format!(
                        "reverify-{}-{}",
                        condition.id,
                        uuid::Uuid::new_v4().simple()
                    );
                    let original_tasks = condition
                        .evidence
                        .iter()
                        .map(|evidence| format!("{}@{}", evidence.task_id, evidence.task_revision))
                        .collect::<Vec<_>>()
                        .join("&");
                    invalidated_units.push((condition.id.clone(), original_tasks, task_id));
                }
                if invalidated_units.is_empty() {
                    return Ok(false);
                }
                current.next_action = Some(revalidation_next_action(&invalidated_units));
            }
            self.persist_autonomous_completion_snapshot().await?;
            return Ok(true);
        }
    }

    pub(super) async fn completion_evidence_matches_current_artifact(
        &self,
        evidence: &CompletionEvidenceRef,
    ) -> bool {
        let Some(call_id) = evidence
            .artifact_ref
            .strip_prefix("task-check:")
            .and_then(|value| value.rsplit_once(':').map(|(call_id, _)| call_id))
        else {
            return false;
        };
        if evidence.verification_ref != evidence.artifact_ref {
            return false;
        }
        let Some(attempt) = self.task_check_attempt_by_call(call_id).await else {
            return false;
        };
        Self::completion_evidence_matches_attempt(evidence, call_id, attempt)
    }

    fn completion_evidence_matches_attempt(
        evidence: &CompletionEvidenceRef,
        expected_call_id: &str,
        attempt: TaskCheckAttempt,
    ) -> bool {
        let TaskCheckAttemptStatus::Finished(receipt) = attempt.status else {
            return false;
        };
        if !receipt.succeeded
            || receipt.exit_code != 0
            || receipt.call_id != expected_call_id
            || receipt.task_id != evidence.task_id
            || receipt.task_revision != evidence.task_revision
            || receipt.artifact_before != receipt.artifact_after
        {
            return false;
        }
        let paths = receipt
            .artifact_after
            .iter()
            .map(|stamp| stamp.path.clone())
            .collect::<Vec<_>>();
        let Some(current_artifacts) =
            crate::task_check::hash_workspace_artifacts(&receipt.workspace_root, &paths)
        else {
            return false;
        };
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
        let fingerprint = codex_history::compaction_candidate::digest(&serde_json::json!({
            "task_id": receipt.task_id,
            "task_revision": receipt.task_revision,
            "workspace_root": receipt.workspace_root.to_string_lossy(),
            "artifacts": artifact_stamp_set,
        }))
        .ok();
        let prior_fingerprint = evidence
            .artifact_ref
            .rsplit_once(':')
            .map(|(_, suffix)| suffix);
        current_artifacts == receipt.artifact_after && fingerprint.as_deref() == prior_fingerprint
    }

    pub(crate) async fn has_pending_completion_check(&self) -> bool {
        self.state
            .lock()
            .await
            .task_check_ledger
            .snapshot()
            .iter()
            .any(|snapshot| matches!(&snapshot.attempt.status, TaskCheckAttemptStatus::Pending))
    }

    pub(crate) async fn install_completion_recovery_task(
        &self,
        handle: AbortOnDropHandle<()>,
    ) -> bool {
        let mut state = self.state.lock().await;
        if state.rencrow_completion_recovery_task.is_some() {
            return false;
        }
        state.rencrow_completion_recovery_task = Some(handle);
        true
    }

    pub(crate) async fn clear_completion_recovery_task(&self) {
        self.state
            .lock()
            .await
            .rencrow_completion_recovery_task
            .take();
    }

    /// Clears a matching external blocker only if the state inspected before probing is still
    /// current and no host-owned verification/runtime work became unresolved meanwhile.
    pub(crate) async fn clear_recovered_external_blocker(
        &self,
        expected_root_task_id: &str,
        expected_state: &CompletionState,
    ) -> Result<bool, String> {
        if self
            .autonomous_completion_has_unresolved_runtime_work()
            .await
        {
            return Ok(false);
        }
        let now = chrono::Utc::now().timestamp();
        {
            let mut state = self.state.lock().await;
            if state.rencrow_completion_paused
                || !state.rencrow_completion_uncollected_children.is_empty()
                || state.task_check_ledger.snapshot().iter().any(|snapshot| {
                    matches!(&snapshot.attempt.status, TaskCheckAttemptStatus::Pending)
                })
            {
                return Ok(false);
            }
            let Some((root_task_id, current)) = state.rencrow_completion_state.as_mut() else {
                return Ok(false);
            };
            if root_task_id != expected_root_task_id
                || current != expected_state
                || current.paused
                || !current.blocker.as_ref().is_some_and(|blocker| {
                    blocker.category == BlockerCategory::ExternalService
                        && blocker.recheck_condition.as_deref()
                            == Some(crate::agent::completion_recovery::SUPPORTED_RECHECK_CONDITION)
                })
            {
                return Ok(false);
            }
            current.blocker = None;
            current.runtime_recovery_at = Some(now);
        }
        self.persist_autonomous_completion_snapshot().await?;
        Ok(true)
    }

    pub(crate) async fn autonomous_completion_recovery_candidate(
        &self,
    ) -> Option<(String, CompletionState)> {
        let state = self.state.lock().await;
        if state.rencrow_completion_paused
            || !state.rencrow_completion_uncollected_children.is_empty()
            || state
                .task_check_ledger
                .snapshot()
                .iter()
                .any(|snapshot| matches!(&snapshot.attempt.status, TaskCheckAttemptStatus::Pending))
        {
            return None;
        }
        let (root, completion) = state.rencrow_completion_state.as_ref()?;
        if completion.paused
            || !completion.blocker.as_ref().is_some_and(|blocker| {
                blocker.category == BlockerCategory::ExternalService
                    && blocker.recheck_condition.as_deref()
                        == Some(crate::agent::completion_recovery::SUPPORTED_RECHECK_CONDITION)
            })
            || completion
                .units
                .iter()
                .any(|unit| unit.status == UnitStatus::Running)
        {
            return None;
        }
        Some((root.clone(), completion.clone()))
    }
}

fn revalidation_next_action(targets: &[(String, String, String)]) -> String {
    let mut action = String::from(
        "Revalidate affected AC/task revisions with new execution unit IDs: use task_context_boundary update, then begin, verify, and accept. Add an independent review unit if required; retain prior Done units and receipts. Targets: ",
    );
    let mut included = 0;
    for (condition_id, original_tasks, new_task_id) in targets {
        let separator = if included == 0 { "" } else { "; " };
        let target = format!("{separator}{condition_id}[{original_tasks}]->{new_task_id}");
        if action.len() + target.len() > MAX_COMPLETION_NEXT_ACTION_BYTES - 100 {
            break;
        }
        action.push_str(&target);
        included += 1;
    }
    if included < targets.len() {
        action.push_str("; remaining affected ACs and original task revisions are in their retained conditions/evidence");
    }
    action
}
