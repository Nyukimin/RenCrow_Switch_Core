// Modified by RenCrow Switch Core, 2026-11-13: record host collection of child results.
use super::TurnContext;
use super::session::Session;
use crate::agent::child_monitor::SamplingTimingEvent;
use crate::agent::control::monitor::ChildMonitorReport;
use codex_api::ResponseEvent;
use codex_features::Feature;
use codex_protocol::ThreadId;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InterAgentCommunication;

impl Session {
    pub(super) async fn observe_child_timing(&self, turn_id: &str, event: SamplingTimingEvent) {
        if self.features.enabled(Feature::RencrowChildMonitor) {
            self.child_monitor.lock().await.observe_timing(
                turn_id,
                event,
                crate::turn_timing::now_unix_timestamp_ms(),
            );
        }
    }

    pub(crate) async fn project_child_work_deadlines(
        &self,
        report: &mut ChildMonitorReport,
        children: &[(ThreadId, String)],
    ) {
        for (child, reference) in children {
            if let Some(snapshot) = report.children.get_mut(reference) {
                // Only the existing owner of review deadlines can supply a work deadline.
                snapshot.timing.child_work_deadline_at_ms =
                    if let Some(turn) = snapshot.turn_id.as_deref() {
                        self.completion_review_deadline_for_turn(*child, turn).await
                    } else {
                        None
                    };
            }
        }
    }

    pub(super) fn log_child_event(&self, turn: &TurnContext, message: &EventMsg) {
        if !self.features.enabled(Feature::RencrowChildMonitor) {
            return;
        }
        let (event, call_id, reason) = match message {
            EventMsg::TurnStarted(_) => ("turn_started", None, None),
            EventMsg::TurnComplete(event) => (
                "turn_completed",
                None,
                event.error.as_ref().map(|_| "terminal_turn_error"),
            ),
            EventMsg::TurnAborted(_) => ("turn_aborted", None, None),
            EventMsg::Error(_) => ("error_event", None, Some("see canonical rollout error")),
            EventMsg::ItemStarted(event) => match &event.item {
                TurnItem::ContextCompaction(_) => {
                    ("compaction_started", Some(event.item.id()), None)
                }
                TurnItem::CommandExecution(_)
                | TurnItem::McpToolCall(_)
                | TurnItem::FileChange(_)
                | TurnItem::DynamicToolCall(_)
                | TurnItem::CollabAgentToolCall(_)
                | TurnItem::Extension(_)
                | TurnItem::WebSearch(_)
                | TurnItem::ImageGeneration(_)
                | TurnItem::ImageView(_) => ("tool_started", Some(event.item.id()), None),
                _ => return,
            },
            EventMsg::ItemCompleted(event) => match &event.item {
                TurnItem::ContextCompaction(_) => {
                    ("compaction_completed", Some(event.item.id()), None)
                }
                TurnItem::CommandExecution(_)
                | TurnItem::McpToolCall(_)
                | TurnItem::FileChange(_)
                | TurnItem::DynamicToolCall(_)
                | TurnItem::CollabAgentToolCall(_)
                | TurnItem::Extension(_)
                | TurnItem::WebSearch(_)
                | TurnItem::ImageGeneration(_)
                | TurnItem::ImageView(_) => ("tool_completed", Some(event.item.id()), None),
                _ => return,
            },
            EventMsg::RequestUserInput(_)
            | EventMsg::RequestPermissions(_)
            | EventMsg::ExecApprovalRequest(_)
            | EventMsg::ApplyPatchApprovalRequest(_) => ("input_response_requested", None, None),
            _ => return,
        };
        let reason = reason.map(|reason| reason.chars().take(160).collect::<String>());
        let parent_thread_id = turn.parent_thread_id.map(|id| id.to_string());
        tracing::info!(target: "codex_core::child_monitor", event,
            thread_id = %self.thread_id,
            parent_thread_id = parent_thread_id.as_deref().unwrap_or("none"),
            turn_id = %turn.sub_id, call_id = call_id.as_deref().unwrap_or("none"),
            reason = reason.as_deref().unwrap_or("none"),
            "agent execution event");
    }

    /// Record host injection into parent history, not a model read acknowledgement.
    pub(super) async fn collect_child_communication(
        &self,
        turn: &TurnContext,
        communication: &InterAgentCommunication,
    ) {
        if !self.features.enabled(Feature::RencrowChildMonitor) {
            return;
        }
        let Some((child, child_turn)) = communication
            .id
            .as_ref()
            .and_then(|id| id.as_str().strip_prefix("rencrow_child_result_"))
            .and_then(|suffix| suffix.split_once(':'))
        else {
            return;
        };
        let Ok(child) = ThreadId::from_string(child) else {
            return;
        };
        if self
            .services
            .agent_control
            .resolve_agent_reference(
                self.thread_id,
                &turn.session_source,
                communication.author.as_str(),
            )
            .await
            .ok()
            == Some(child)
        {
            self.services
                .agent_control
                .mark_child_result_collected(
                    child,
                    child_turn,
                    crate::turn_timing::now_unix_timestamp_ms(),
                )
                .await;
        }
    }

    pub(crate) async fn observe_child_model_output(&self, turn_id: &str, event: &ResponseEvent) {
        if !self.features.enabled(Feature::RencrowChildMonitor) {
            return;
        }
        if matches!(event, ResponseEvent::Completed { .. }) {
            self.observe_child_timing(turn_id, SamplingTimingEvent::RequestFinished)
                .await;
        }
        let has_output = match event {
            ResponseEvent::OutputTextDelta(delta)
            | ResponseEvent::ToolCallInputDelta { delta, .. }
            | ResponseEvent::ReasoningSummaryDelta { delta, .. }
            | ResponseEvent::ReasoningContentDelta { delta, .. } => !delta.is_empty(),
            ResponseEvent::OutputItemDone(item) => match item {
                ResponseItem::Message { content, .. } => content.iter().any(
                    |part| matches!(part, ContentItem::OutputText { text } if !text.is_empty()),
                ),
                ResponseItem::FunctionCall {
                    name, arguments, ..
                } => !name.is_empty() || !arguments.is_empty(),
                ResponseItem::CustomToolCall { name, input, .. } => {
                    !name.is_empty() || !input.is_empty()
                }
                ResponseItem::Reasoning {
                    summary,
                    content,
                    encrypted_content,
                    ..
                } => {
                    summary.iter().any(|part| {
                        let codex_protocol::models::ReasoningItemReasoningSummary::SummaryText {
                            text,
                        } = part;
                        !text.is_empty()
                    }) || content.as_ref().is_some_and(|parts| {
                        parts.iter().any(|part| {
                            let (codex_protocol::models::ReasoningItemContent::ReasoningText {
                                text,
                            }
                            | codex_protocol::models::ReasoningItemContent::Text { text }) = part;
                            !text.is_empty()
                        })
                    }) || encrypted_content
                        .as_ref()
                        .is_some_and(|content| !content.is_empty())
                }
                _ => false,
            },
            _ => false,
        };
        if has_output {
            self.child_monitor
                .lock()
                .await
                .model_output(turn_id, crate::turn_timing::now_unix_timestamp_ms());
        }
    }

    pub(crate) async fn mark_child_result_collected(&self, turn_id: &str, at_ms: i64) {
        if self.features.enabled(Feature::RencrowChildMonitor) {
            let mut monitor = self.child_monitor.lock().await;
            let first_collection = monitor.snapshot().result_collected_at_ms.is_none();
            if monitor.collected(turn_id, at_ms) && first_collection {
                tracing::info!(target: "codex_core::child_monitor",
                    event = "result_collected_by_host", thread_id = %self.thread_id,
                    turn_id, at_ms, "child result delivered to parent history or wait result");
            }
        }
    }
}

#[cfg(test)]
#[path = "child_monitor_tests.rs"]
mod tests;
