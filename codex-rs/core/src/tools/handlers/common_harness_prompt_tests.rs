// Modified by RenCrow Switch Core, 2026-11-27: verify canonical facts at the model input boundary.

use super::TaskContextBoundaryHandler;
use super::common_task;
use super::invocation_in_session;
use super::make_session_and_context;
use crate::client_common::Prompt;
use crate::session::step_context::StepContext;
use crate::session::turn::build_prompt;
use crate::tools::registry::ToolExecutor;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use pretty_assertions::assert_eq;
use serde_json::Value;
use std::sync::Arc;

const COMMON_PROMPT_KIND: &str = "rencrow.common_harness_prompt";

fn common_prompt_projection(prompt: &Prompt) -> Option<Value> {
    for item in &prompt.input {
        let ResponseItem::Message {
            content,
            internal_chat_message_metadata_passthrough,
            ..
        } = item
        else {
            continue;
        };
        let is_common_fragment = internal_chat_message_metadata_passthrough
            .as_ref()
            .and_then(|metadata| metadata.content_item_kinds.as_deref())
            .is_some_and(|kinds| {
                kinds
                    .iter()
                    .any(|kind| kind.0.as_str() == COMMON_PROMPT_KIND)
            });
        if !is_common_fragment {
            continue;
        }
        let Some(ContentItem::InputText { text }) = content.first() else {
            continue;
        };
        let body = text
            .strip_prefix("<common_harness_prompt>")?
            .strip_suffix("</common_harness_prompt>")?;
        let (_, json) = body.split_once('\n')?;
        return serde_json::from_str(json).ok();
    }
    None
}

async fn actual_prompt(
    session: &Arc<crate::session::session::Session>,
    turn: &Arc<crate::session::turn_context::TurnContext>,
) -> Prompt {
    let step = StepContext::for_test(Arc::clone(turn));
    let mut input = session
        .clone_history()
        .await
        .for_prompt(&step.settings.model_info.input_modalities);
    session
        .append_common_harness_prompt_context(turn, &mut input)
        .await
        .expect("bounded Common Harness prompt projection");
    build_prompt(
        input,
        step.as_ref(),
        session.get_prompt_base_instructions().await,
    )
}

#[tokio::test]
async fn common_harness_prompt_uses_current_host_state_at_build_prompt_boundary() {
    let (_workspace, session, turn) = common_task().await;
    // The production loop does this immediately before taking prompt state. It starts
    // or reuses the durable attempt/window, so this assertion observes request state.
    session
        .common_harness_inference_deadline(&turn)
        .await
        .expect("bind current durable inference window");
    let (root, common) = session
        .common_harness_tool_state_for_turn(&turn)
        .await
        .expect("fixture has an authorized Common task");
    let scope = session
        .active_task_scope()
        .await
        .expect("normal Begin established the active execution scope");

    let prompt = actual_prompt(&session, &turn).await;
    let projected = common_prompt_projection(&prompt)
        .expect("actual build_prompt input contains the host Common fragment");
    assert_eq!(projected["request"]["task_id"], common.request.task_id);
    assert_eq!(
        projected["request"]["request_revision"],
        common.request.request_revision
    );
    assert_eq!(projected["request"]["goal"], common.request.goal);
    assert_eq!(
        projected["request"]["constraints"][0],
        common.request.constraints[0]
    );
    assert_eq!(
        projected["request"]["workspace_ref"],
        common.request.workspace_ref
    );
    assert_eq!(
        projected["run_state"]["target_version"],
        common.run_state.target_version
    );
    assert_eq!(
        projected["run_state"]["attempt_no"],
        common.run_state.attempt_no
    );
    assert_eq!(
        projected["run_state"]["max_attempts"],
        common.request.max_attempts
    );
    assert_eq!(projected["active_scope"]["root_task_id"], root);
    assert_eq!(projected["active_scope"]["task_id"], scope.task_id);
    assert_eq!(
        projected["active_scope"]["task_revision"],
        scope.task_revision
    );
    assert_eq!(
        projected["active_scope"]["begin_call_id"],
        scope.begin_call_id
    );

    let projected_conditions = projected["request"]["conditions"]
        .as_array()
        .expect("required conditions remain structured");
    for condition in common
        .request
        .conditions
        .iter()
        .filter(|condition| condition.required)
    {
        let projected_condition = projected_conditions
            .iter()
            .find(|candidate| candidate["condition_id"] == condition.condition_id)
            .expect("required condition survives prompt projection");
        assert_eq!(
            projected_condition["condition_revision"],
            condition.condition_revision
        );
        assert_eq!(
            projected_condition["kind"],
            serde_json::to_value(condition.kind).unwrap()
        );
        let check = condition
            .check
            .as_ref()
            .expect("fixture condition is executable");
        assert_eq!(
            projected_condition["check"]["argv"],
            serde_json::to_value(&check.argv).unwrap(),
            "registered verifier argv must be included in model input"
        );
        assert_eq!(
            projected_condition["check"]["parser"],
            serde_json::to_value(check.parser).unwrap()
        );
        assert_eq!(
            projected_condition["check"]["expected_outcome"],
            serde_json::to_value(&check.expected_outcome).unwrap()
        );
        assert_eq!(
            projected_condition["check"]["go_test_selection"],
            serde_json::to_value(&check.go_test_selection).unwrap()
        );
    }
    assert!(projected["request"]["skill_refs"].is_array());
    assert!(projected["current_evidence"].is_array());
    assert!(projected.get("operation_refs").is_none());
    assert!(projected.get("owner_takeovers").is_none());
    assert!(projected.get("raw_output").is_none());
}

#[tokio::test]
async fn legacy_prompt_path_adds_no_common_harness_fragment() {
    let (session, turn) = make_session_and_context().await;
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let step = StepContext::for_test(Arc::clone(&turn));
    let before = session
        .clone_history()
        .await
        .for_prompt(&step.settings.model_info.input_modalities);
    let prompt = actual_prompt(&session, &turn).await;
    assert!(common_prompt_projection(&prompt).is_none());
    assert_eq!(prompt.input.len(), before.len());
}

#[tokio::test]
async fn common_harness_status_without_root_works_before_first_accept() {
    let (_workspace, session, turn) = common_task().await;
    let before = session
        .current_common_harness_state("harness-root")
        .await
        .unwrap();
    TaskContextBoundaryHandler::new(true)
        .handle(invocation_in_session(
            Arc::clone(&session),
            turn,
            "common-harness-status-before-checkpoint",
            serde_json::json!({"action":"status"}),
        ))
        .await
        .expect("current Common root exists before any accepted boundary");
    assert_eq!(
        session
            .current_common_harness_state("harness-root")
            .await
            .unwrap(),
        before,
        "status cannot consume a work attempt"
    );
}
