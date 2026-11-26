// Modified by RenCrow Switch Core, 2026-11-27: restore compact task facts at the model boundary.
use super::common_harness::common_harness_fence;
use super::session::Session;
use super::turn_context::TurnContext;
use crate::context::CommonHarnessPromptFragment;
use crate::context::ContextualUserFragment;
use codex_history::task_context::CommonHarnessStateV1;
use codex_history::task_context::ConditionEvidenceV1;
use codex_history::task_context::EvidenceLifecycleV1;
use codex_history::task_context::HostEventV1;
use codex_history::task_context::TaskRunStatusV1;
use codex_protocol::models::ResponseItem;
use codex_utils_string::approx_bytes_for_tokens;
use codex_utils_string::approx_tokens_from_byte_count;
use serde_json::Value;
use std::collections::BTreeMap;

// Keep the context contract within the existing host instruction budget convention.
// These are estimated tokens based on serialized bytes, not tokenizer measurements.
const MAX_COMMON_HARNESS_PROMPT_ESTIMATED_TOKENS: usize = 8_000;
const COMMON_HARNESS_PROMPT_TOO_LARGE: &str = "prompt_context_too_large";

/// Keep only the latest current evidence for each required, still-registered condition.
/// This is a current-state projection, not a replay of the append-only evidence ledger.
fn current_evidence(common: &CommonHarnessStateV1) -> Vec<&ConditionEvidenceV1> {
    let mut latest = BTreeMap::<&str, &ConditionEvidenceV1>::new();
    for evidence in &common.evidence_refs {
        if evidence.lifecycle != EvidenceLifecycleV1::Current
            || evidence.request_revision != common.request.request_revision
            || evidence.condition_set_revision != common.request.condition_set_revision
            || evidence.target_version != common.run_state.target_version
        {
            continue;
        }
        let is_registered_current_condition = common.request.conditions.iter().any(|condition| {
            condition.required
                && condition.condition_id == evidence.condition_id
                && condition.condition_revision == evidence.condition_revision
        });
        if is_registered_current_condition {
            // History appends observations, so replacement retains the latest matching item.
            latest.insert(evidence.condition_id.as_str(), evidence);
        }
    }
    latest.into_values().collect()
}

fn projection(common: &CommonHarnessStateV1, active_scope: Option<Value>) -> Result<Value, String> {
    let conditions = common
        .request
        .conditions
        .iter()
        .filter(|condition| condition.required)
        .map(serde_json::to_value)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("serialize Common Harness conditions for prompt: {error}"))?;

    let evidence = current_evidence(common)
        .into_iter()
        .map(|entry| {
            let negative_proof = entry.host_negative_proof.as_ref().map(|proof| {
                serde_json::json!({
                    "baseline_receipt_ref": proof.baseline_receipt_ref,
                    "expected_failure_identity": proof.expected_failure_identity,
                    "observed_failure_identity": proof.observed_failure_identity,
                    "resolution_receipt_ref": proof.resolution_receipt_ref,
                })
            });
            serde_json::json!({
                "evidence_id": entry.evidence_id,
                "condition_id": entry.condition_id,
                "condition_revision": entry.condition_revision,
                "condition_set_revision": entry.condition_set_revision,
                "target_version": entry.target_version,
                "execution_id": entry.execution_id,
                "operation_id": entry.operation_id,
                "verifier_ref": entry.verifier_ref,
                "verifier_config_revision": entry.verifier_config_revision,
                "observed": entry.observed,
                "verdict": entry.verdict,
                "log_refs": entry.log_refs,
                "artifact_refs": entry.artifact_refs,
                "source_evidence_ref": entry.source_evidence_ref,
                "comparison_proof_ref": entry.comparison_proof_ref,
                "host_negative_proof": negative_proof,
                "lifecycle": entry.lifecycle,
            })
        })
        .collect::<Vec<_>>();

    Ok(serde_json::json!({
        "contract_version": common.contract_version,
        "request": {
            "task_id": common.request.task_id,
            "request_revision": common.request.request_revision,
            "condition_set_revision": common.request.condition_set_revision,
            "goal": common.request.goal,
            "constraints": common.request.constraints,
            "workspace_ref": common.request.workspace_ref,
            "start_target_version": common.request.start_target_version,
            "permission_snapshot_ref": common.request.permission_snapshot_ref,
            "operation_limits_ref": common.request.operation_limits_ref,
            "skill_refs": common.request.skill_refs,
            "conditions": conditions,
        },
        "run_state": {
            "status": common.run_state.status,
            "phase": common.run_state.phase,
            "attempt_no": common.run_state.attempt_no,
            "max_attempts": common.request.max_attempts,
            "target_version": common.run_state.target_version,
            "unmet_conditions": common.run_state.unmet_conditions,
            "next_operation": common.run_state.next_operation,
            "reason_code": common.run_state.reason_code,
            "resume_condition": common.run_state.resume_condition,
            "unsettled_operation_refs": common.run_state.unsettled_operation_refs,
            "waiting": common.run_state.waiting,
            "work_window": common.run_state.work_window,
        },
        "active_scope": active_scope,
        "current_evidence": evidence,
        "authority_note": "Host status only. Existing permissions, registered tool routes, operation fences, and acceptance checks remain authoritative.",
    }))
}

impl Session {
    /// Append a compact canonical Common Harness projection to this model request.
    ///
    /// The Common clone comes from the existing authorization-aware accessor. The prompt
    /// never contains the operation ledger, completion graph, stale evidence, or raw logs.
    pub(crate) async fn append_common_harness_prompt_context(
        &self,
        turn: &TurnContext,
        input: &mut Vec<ResponseItem>,
    ) -> Result<(), String> {
        if self.is_read_only_review_actor_for_turn(turn).await {
            return Ok(());
        }
        let Some((root_task_id, common)) = self.common_harness_tool_state_for_turn(turn).await
        else {
            // Preserve the legacy path without cloning canonical state or scanning history.
            return Ok(());
        };
        let active_scope = self
            .active_task_scope()
            .await
            .filter(|scope| scope.root_task_id == root_task_id)
            .map(|scope| {
                serde_json::json!({
                    "root_task_id": scope.root_task_id,
                    "task_id": scope.task_id,
                    "task_revision": scope.task_revision,
                    "begin_call_id": scope.begin_call_id,
                })
            });
        let body = projection(&common, active_scope)?;
        let serialized = serde_json::to_string(&body)
            .map_err(|error| format!("serialize Common Harness prompt projection: {error}"))?;
        let fragment = CommonHarnessPromptFragment {
            projection: serialized,
        };
        let rendered_bytes = fragment.render().len();
        if rendered_bytes > approx_bytes_for_tokens(MAX_COMMON_HARNESS_PROMPT_ESTIMATED_TOKENS) {
            let estimated_tokens = approx_tokens_from_byte_count(rendered_bytes);
            let reason = format!(
                "complete Common Harness prompt state is {estimated_tokens} estimated tokens and exceeds the {MAX_COMMON_HARNESS_PROMPT_ESTIMATED_TOKENS} estimated-token S1 budget; no constraints or verifier details were omitted"
            );
            if !matches!(
                common.run_state.status,
                TaskRunStatusV1::Blocked
                    | TaskRunStatusV1::Waiting
                    | TaskRunStatusV1::Exhausted
                    | TaskRunStatusV1::Cancelled
                    | TaskRunStatusV1::Completed
            ) {
                self.apply_common_harness_host_event(
                    &root_task_id,
                    common_harness_fence(&common),
                    HostEventV1::Block {
                        reason_code: COMMON_HARNESS_PROMPT_TOO_LARGE.into(),
                        resume_condition: reason.clone(),
                    },
                )
                .await
                .map_err(|error| {
                    format!("persist oversized Common Harness prompt stop: {error}")
                })?;
            }
            return Err(format!("{COMMON_HARNESS_PROMPT_TOO_LARGE}: {reason}"));
        }
        input.push(Box::new(fragment).into_boxed_response_item());
        Ok(())
    }

    /// Return the authorized Common root for Status, including before the first Accept.
    pub(crate) async fn common_harness_root_for_turn(&self, turn: &TurnContext) -> Option<String> {
        self.common_harness_tool_state_for_turn(turn)
            .await
            .map(|(root_task_id, _)| root_task_id)
    }
}
