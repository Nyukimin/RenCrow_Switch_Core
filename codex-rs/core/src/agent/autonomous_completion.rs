//! Wake an opted-in idle parent after a real child terminal notification.
// Modified by RenCrow Switch Core, 2026-11-11.

use crate::codex_thread::CodexThread;
use crate::context::AutonomousChildResult;
use crate::context::AutonomousCompletionContinue;
use crate::context::ContextualUserFragment;
use codex_protocol::turn_input::TurnInput;
use codex_protocol::turn_input::TurnInputRequest;
use codex_protocol::turn_input::TurnInputSubmission;

pub(super) async fn wake_parent(parent: &CodexThread, event_key: &str) {
    if !parent.config().await.rencrow_autonomous_completion
        || !parent.session.autonomous_completion_pending().await
        || parent
            .session
            .autonomous_completion_event_seen(event_key)
            .await
    {
        return;
    }
    let Some(previous_turn_id) = parent
        .session
        .autonomous_completion_previous_turn_id()
        .await
    else {
        return;
    };
    let request = TurnInputRequest::new(TurnInput::ResponseItem(
        Box::new(AutonomousChildResult).into_boxed_response_item(),
    ));
    match parent
        .continue_autonomous_turn_if_idle(request, previous_turn_id.clone())
        .await
    {
        Ok(TurnInputSubmission::Started { .. }) => {
            parent
                .session
                .mark_autonomous_completion_continued(&previous_turn_id, event_key)
                .await;
        }
        Ok(_) => {}
        Err(error) => {
            tracing::warn!(%error, "autonomous child-result continuation was not admitted")
        }
    }
}

pub(crate) async fn wake_after_cold_resume(parent: &CodexThread) {
    if !parent.config().await.rencrow_autonomous_completion {
        return;
    }
    if let Some((root_task_id, _)) = parent.session.autonomous_completion_current_state().await
        && let Err(error) = parent
            .session
            .reconcile_rencrow_completion_evidence(&root_task_id)
            .await
    {
        tracing::warn!(%error, "autonomous completion evidence reconcile failed after cold resume");
        return;
    }
    if !parent.session.autonomous_completion_pending().await
        || parent
            .session
            .autonomous_completion_has_unresolved_runtime_work()
            .await
    {
        return;
    }
    let Some(previous_turn_id) = parent
        .session
        .autonomous_completion_cold_resume_turn_id()
        .await
    else {
        return;
    };
    let event_key = format!("cold-resume:{previous_turn_id}");
    if parent
        .session
        .autonomous_completion_event_seen(&event_key)
        .await
    {
        return;
    }
    let Some((_, contract)) = parent.session.autonomous_completion_current_state().await else {
        return;
    };
    let next_action = contract
        .next_action
        .filter(|action| !action.trim().is_empty())
        .unwrap_or_else(|| "Review the registered conditions and select a safe ready unit".into());
    let request = TurnInputRequest::new(TurnInput::ResponseItem(
        Box::new(AutonomousCompletionContinue { next_action }).into_boxed_response_item(),
    ));
    match parent
        .continue_autonomous_turn_if_idle(request, previous_turn_id.clone())
        .await
    {
        Ok(TurnInputSubmission::Started { .. }) => {
            parent
                .session
                .mark_autonomous_completion_continued(&previous_turn_id, &event_key)
                .await;
        }
        Ok(_) => {}
        Err(error) => {
            tracing::warn!(%error, "autonomous cold-resume continuation was not admitted")
        }
    }
}
