use super::*;
use codex_extension_api::ExtensionData;
use codex_extension_api::TurnItemContributor;
use codex_protocol::ResponseItemId;
use codex_protocol::items::AgentMessageContent;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use tracing_subscriber::prelude::*;

struct RewriteAgentMessageContributor;

impl TurnItemContributor for RewriteAgentMessageContributor {
    fn contribute<'a>(
        &'a self,
        _thread_store: &'a ExtensionData,
        _turn_store: &'a ExtensionData,
        item: &'a mut TurnItem,
    ) -> codex_extension_api::ExtensionFuture<'a, Result<(), String>> {
        Box::pin(async move {
            if let TurnItem::AgentMessage(agent_message) = item {
                agent_message.content = vec![AgentMessageContent::Text {
                    text: "plan contributed assistant text".to_string(),
                }];
            }
            Ok(())
        })
    }
}

fn assistant_output_text(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: Some(ResponseItemId::with_suffix("msg", "1")),
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

#[test]
fn post_sampling_token_estimate_is_disabled_by_always_on_sinks() {
    let feedback = codex_feedback::CodexFeedback::new();
    let subscriber = tracing_subscriber::registry()
        .with(feedback.logger_layer())
        .with(tracing_subscriber::fmt::layer().with_filter(codex_state::log_db::default_filter()));

    tracing::subscriber::with_default(subscriber, || {
        tracing::callsite::rebuild_interest_cache();
        assert!(!tracing::event_enabled!(
            target: POST_SAMPLING_TOKEN_ESTIMATE_TARGET,
            tracing::Level::TRACE,
            turn_id,
            estimated_token_count,
            message
        ));
    });
}

#[tokio::test]
async fn plan_mode_uses_contributed_turn_item_for_last_agent_message() {
    let (mut session, turn_context) = crate::session::tests::make_session_and_context().await;
    let mut builder = codex_extension_api::ExtensionRegistryBuilder::new();
    builder.turn_item_contributor(Arc::new(RewriteAgentMessageContributor));
    session.services.extensions = Arc::new(builder.build());
    let turn_store = ExtensionData::new(turn_context.sub_id.clone());
    let mut state = PlanModeStreamState::new(&turn_context.sub_id);
    let mut last_agent_message = None;
    let item = assistant_output_text("original assistant text");

    let step_context = StepContext::for_test(Arc::new(turn_context));
    let handled = handle_assistant_item_done_in_plan_mode(
        &session,
        &step_context,
        &turn_store,
        &item,
        &mut state,
        /*previously_active_item*/ None,
        &mut last_agent_message,
    )
    .await;

    assert!(handled);
    assert_eq!(
        last_agent_message.as_deref(),
        Some("plan contributed assistant text")
    );
}

#[test]
fn realtime_user_verification_notice_excludes_request_payload() {
    let event = EventMsg::ElicitationRequest(codex_protocol::approvals::ElicitationRequestEvent {
        turn_id: None,
        server_name: "private-server-name".to_string(),
        id: codex_protocol::mcp::RequestId::String("private-request-id".to_string()),
        request: codex_protocol::approvals::ElicitationRequest::UserVerification {
            meta: None,
            title: "private-title".to_string(),
            description: "private-description".to_string(),
            challenge: "private-challenge".to_string(),
        },
    });
    assert_eq!(
        realtime_text_for_event(&event),
        Some((
            "<user_verification_notice>User verification is required. Please respond in the app.</user_verification_notice>".to_string(),
            None,
        )),
    );
}

#[test]
fn turn_end_completion_action_includes_review_status_and_saved_action() {
    let current_review_reason =
        "Review review-a remains historical; add a linked revalidation through Update.";
    let recorded_action = "Start the already completed task-b";

    let current_review_action = turn_end_autonomous_completion_action(
        None,
        Some(current_review_reason),
        Some(recorded_action),
    );
    assert!(current_review_action.contains(current_review_reason));
    assert!(current_review_action.contains(recorded_action));
    assert!(
        current_review_action
            .contains("carry forward existing accepted task execution evidence unchanged")
    );

    let plan_review_action = turn_end_autonomous_completion_action(
        Some("Keep the existing plan"),
        Some(current_review_reason),
        Some(recorded_action),
    );
    assert!(plan_review_action.contains("previous_plan exactly `Keep the existing plan`"));
    assert!(!plan_review_action.contains(current_review_reason));

    assert_eq!(
        turn_end_autonomous_completion_action(None, None, Some(recorded_action)),
        recorded_action
    );
    assert_eq!(
        turn_end_autonomous_completion_action(None, None, None),
        "recover terminal child results, select the next ready unit, or review the plan"
    );
}
