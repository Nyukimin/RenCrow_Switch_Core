// Modified by RenCrow Switch Core, 2026-11-27: return host results from durable task evidence.
use super::common_harness_completion::CommonHarnessTurnEndDecision;
use super::session::Session;
use super::turn_context::TurnContext;
use codex_history::task_context::CommonHarnessStateV1;
use codex_history::task_context::ConditionVerdictV1;
use codex_history::task_context::EvidenceLifecycleV1;
use codex_history::task_context::TaskRunStatusV1;

fn result_packet(state: &CommonHarnessStateV1) -> Result<String, String> {
    let mut conditions = Vec::new();
    for condition in state
        .request
        .conditions
        .iter()
        .filter(|condition| condition.required)
    {
        let evidence = state.evidence_refs.iter().rev().find(|evidence| {
            evidence.condition_id == condition.condition_id
                && evidence.condition_revision == condition.condition_revision
                && evidence.condition_set_revision == state.request.condition_set_revision
                && evidence.request_revision == state.request.request_revision
                && evidence.target_version == state.run_state.target_version
                && evidence.lifecycle == EvidenceLifecycleV1::Current
        });
        if state.run_state.status == TaskRunStatusV1::Completed
            && !evidence.is_some_and(|evidence| evidence.verdict == ConditionVerdictV1::Pass)
        {
            return Err(format!(
                "completed condition {} has no current passing evidence",
                condition.condition_id
            ));
        }
        conditions.push(serde_json::json!({
            "condition_id": condition.condition_id,
            "condition_revision": condition.condition_revision,
            "verdict": evidence.map(|evidence| evidence.verdict),
            "observed": evidence.map(|evidence| &evidence.observed),
            "execution_id": evidence.map(|evidence| &evidence.execution_id),
            "log_refs": evidence.map(|evidence| &evidence.log_refs).cloned().unwrap_or_default(),
            "artifact_refs": evidence.map(|evidence| &evidence.artifact_refs).cloned().unwrap_or_default(),
            "comparison_proof_ref": evidence.and_then(|evidence| evidence.comparison_proof_ref.as_ref()),
        }));
    }
    serde_json::to_string(&serde_json::json!({
        "format": "common-harness/task-result/1",
        "task_id": state.request.task_id,
        "contract_version": state.contract_version,
        "request_revision": state.request.request_revision,
        "condition_set_revision": state.request.condition_set_revision,
        "state_version": state.run_state.state_version,
        "final_state": state.run_state.status,
        "target_version": state.run_state.target_version,
        "attempt_no": state.run_state.attempt_no,
        "max_attempts": state.request.max_attempts,
        "conditions": conditions,
        "remaining": state.run_state.unmet_conditions,
        "reason_code": state.run_state.reason_code,
        "resume_condition": state.run_state.resume_condition,
    }))
    .map_err(|error| format!("serialize common-harness result: {error}"))
}

impl Session {
    pub(crate) async fn emit_common_harness_terminal_result(
        &self,
        turn: &TurnContext,
        model_info: &codex_protocol::openai_models::ModelInfo,
    ) -> Result<Option<String>, String> {
        let Some(result) = self.common_harness_terminal_result(turn).await? else {
            return Ok(None);
        };
        self.record_response_item_and_emit_turn_item(
            turn,
            model_info,
            codex_protocol::models::ResponseItem::Message {
                id: None,
                role: "assistant".into(),
                content: vec![codex_protocol::models::ContentItem::OutputText {
                    text: result.clone(),
                }],
                phase: Some(codex_protocol::models::MessagePhase::FinalAnswer),
                internal_chat_message_metadata_passthrough: None,
            },
        )
        .await;
        Ok(Some(result))
    }

    pub(crate) async fn common_harness_terminal_result(
        &self,
        turn: &TurnContext,
    ) -> Result<Option<String>, String> {
        if self.is_read_only_review_actor_for_turn(turn).await {
            return Ok(None);
        }
        let common = {
            let state = self.state.lock().await;
            let Some((_, completion)) = state.rencrow_completion_state.as_ref() else {
                return Ok(None);
            };
            let Some(common) = completion.common_harness.as_ref() else {
                return Ok(None);
            };
            if !matches!(
                common.run_state.status,
                TaskRunStatusV1::Completed
                    | TaskRunStatusV1::Waiting
                    | TaskRunStatusV1::Blocked
                    | TaskRunStatusV1::Exhausted
                    | TaskRunStatusV1::Cancelled
            ) {
                return Ok(None);
            }
            common.clone()
        };
        result_packet(&common).map(Some)
    }

    pub(crate) async fn common_harness_checkpoint_result(
        &self,
        turn: &TurnContext,
        checkpoint_committed: bool,
        has_pending_input: bool,
    ) -> Result<Option<String>, String> {
        if !checkpoint_committed
            || has_pending_input
            || self.is_read_only_review_actor_for_turn(turn).await
        {
            return Ok(None);
        }
        if !matches!(
            self.common_harness_turn_end_gate(turn).await?,
            CommonHarnessTurnEndDecision::Completed { .. }
        ) {
            return Ok(None);
        }
        self.common_harness_terminal_result(turn).await
    }
}
