// Modified by RenCrow Switch Core, 2026-11-13: deterministic child execution observations.
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TurnAbortReason;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashSet;

const MAX_ACTIVE_TOOL_CALLS: usize = 64;
pub(crate) mod health;
pub(crate) mod timing;
mod timing_bridge;
pub(crate) use timing_bridge::SamplingTimingEvent;

/// A read-only summary of a child agent's latest observed execution phase.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, rename_all = "snake_case")]
pub(crate) struct ChildMonitorSnapshot {
    pub(crate) turn_id: Option<String>,
    pub(crate) phase: ChildMonitorPhase,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reason: Option<String>,
    pub(crate) active_tool_count: usize,
    pub(crate) turn_started_at_ms: Option<i64>,
    pub(crate) first_output_at_ms: Option<i64>,
    pub(crate) last_output_at_ms: Option<i64>,
    pub(crate) final_output_at_ms: Option<i64>,
    pub(crate) tool_started_at_ms: Option<i64>,
    pub(crate) tool_ended_at_ms: Option<i64>,
    pub(crate) turn_completed_at_ms: Option<i64>,
    pub(crate) result_collected_at_ms: Option<i64>,
    pub(crate) last_activity_at_ms: Option<i64>,
    pub(crate) progress_sequence: u64,
    #[serde(default)]
    pub(crate) timing: timing::ChildTimingSnapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ChildMonitorPhase {
    Unknown,
    Pending,
    Generating,
    ToolExecution,
    InputWaiting,
    Compacting,
    Completed,
    Failed,
}

impl Default for ChildMonitorSnapshot {
    fn default() -> Self {
        Self {
            turn_id: None,
            phase: ChildMonitorPhase::Unknown,
            reason: Some("no execution event observed".to_string()),
            active_tool_count: 0,
            turn_started_at_ms: None,
            first_output_at_ms: None,
            last_output_at_ms: None,
            final_output_at_ms: None,
            tool_started_at_ms: None,
            tool_ended_at_ms: None,
            turn_completed_at_ms: None,
            result_collected_at_ms: None,
            last_activity_at_ms: None,
            progress_sequence: 0,
            timing: Default::default(),
        }
    }
}

/// Reduces observed protocol events into a bounded child execution snapshot.
#[derive(Debug, Default)]
pub(crate) struct ChildMonitor {
    snapshot: ChildMonitorSnapshot,
    active_tool_calls: HashSet<String>,
    pending_inputs: HashSet<String>,
    tracking_overflowed: bool,
    compaction_active: bool,
    request_timing: timing::RequestTiming,
}

impl ChildMonitor {
    pub(crate) fn snapshot(&self) -> ChildMonitorSnapshot {
        self.snapshot.clone()
    }

    pub(crate) fn observe(&mut self, event_id: &str, msg: &EventMsg, observed_at_ms: i64) {
        if let EventMsg::TurnStarted(_) = msg {
            self.start_turn(event_id, observed_at_ms);
            return;
        }
        let previous_progress_sequence = self.snapshot.progress_sequence;

        if !matches!(msg, EventMsg::ShutdownComplete)
            && self
                .snapshot
                .turn_id
                .as_deref()
                .is_some_and(|turn_id| turn_id != event_id)
        {
            return;
        }
        if matches!(msg, EventMsg::TurnAborted(_)) {
            self.request_timing.finish_series(observed_at_ms);
        }
        if self.tracking_overflowed
            && !matches!(msg, EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_))
        {
            return;
        }
        if matches!(
            self.snapshot.phase,
            ChildMonitorPhase::Completed | ChildMonitorPhase::Failed
        ) && !matches!(
            msg,
            EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_) | EventMsg::ShutdownComplete
        ) {
            return;
        }

        match msg {
            EventMsg::TurnComplete(event) => match &event.error {
                Some(error) => self.set_terminal(
                    ChildMonitorPhase::Failed,
                    Some(error.message.clone()),
                    observed_at_ms,
                ),
                None => self.set_terminal(ChildMonitorPhase::Completed, None, observed_at_ms),
            },
            EventMsg::TurnAborted(event) => match &event.reason {
                TurnAbortReason::Interrupted | TurnAbortReason::BudgetLimited => {
                    self.set_phase(
                        ChildMonitorPhase::Unknown,
                        Some("turn interrupted; parent action may be required"),
                    );
                    self.snapshot
                        .turn_completed_at_ms
                        .get_or_insert(observed_at_ms);
                    self.snapshot.progress_sequence =
                        self.snapshot.progress_sequence.saturating_add(1);
                }
                _ => self.set_terminal(
                    ChildMonitorPhase::Failed,
                    Some(format!("turn aborted: {:?}", event.reason)),
                    observed_at_ms,
                ),
            },
            EventMsg::Error(event) if event.affects_turn_status() => {
                self.set_terminal(
                    ChildMonitorPhase::Failed,
                    Some(event.message.clone()),
                    observed_at_ms,
                );
            }
            EventMsg::ShutdownComplete => {
                self.set_phase(ChildMonitorPhase::Unknown, Some("session shut down"));
                self.snapshot.progress_sequence = self.snapshot.progress_sequence.saturating_add(1);
            }
            EventMsg::AgentMessage(event) if !event.message.is_empty() => {
                self.model_output(event_id, observed_at_ms);
                if event.phase != Some(codex_protocol::models::MessagePhase::Commentary) {
                    self.snapshot
                        .final_output_at_ms
                        .get_or_insert(observed_at_ms);
                }
            }
            EventMsg::AgentMessageContentDelta(event) if !event.delta.is_empty() => {
                self.model_output(&event.turn_id, observed_at_ms);
            }
            EventMsg::ReasoningContentDelta(event) if !event.delta.is_empty() => {
                self.model_output(&event.turn_id, observed_at_ms);
            }
            EventMsg::ReasoningRawContentDelta(event) if !event.delta.is_empty() => {
                self.model_output(&event.turn_id, observed_at_ms);
            }
            EventMsg::RequestUserInput(event) if event.is_blocking => {
                self.input_requested(&event.call_id);
            }
            EventMsg::RequestPermissions(event) => {
                self.input_requested(&event.call_id);
            }
            EventMsg::ExecApprovalRequest(event) => {
                self.input_requested(event.approval_id.as_deref().unwrap_or(&event.call_id));
            }
            EventMsg::ApplyPatchApprovalRequest(event) => {
                self.input_requested(&event.call_id);
            }
            EventMsg::ItemStarted(event) => match &event.item {
                TurnItem::AgentMessage(_) | TurnItem::Reasoning(_)
                    if self.active_tool_calls.is_empty()
                        && !matches!(
                            self.snapshot.phase,
                            ChildMonitorPhase::Compacting | ChildMonitorPhase::InputWaiting
                        ) =>
                {
                    self.set_phase(ChildMonitorPhase::Generating, None);
                }
                TurnItem::ContextCompaction(_) if !self.compaction_active => {
                    self.compaction_active = true;
                    self.set_phase(ChildMonitorPhase::Compacting, None);
                    self.snapshot.progress_sequence =
                        self.snapshot.progress_sequence.saturating_add(1);
                }
                TurnItem::ContextCompaction(_) => {}
                TurnItem::DynamicToolCall(_)
                | TurnItem::CollabAgentToolCall(_)
                | TurnItem::Extension(_)
                | TurnItem::ImageView(_) => {
                    self.tool_started(event.item.id(), observed_at_ms);
                }
                _ => {}
            },
            EventMsg::ItemCompleted(event) => {
                if matches!(&event.item, TurnItem::ContextCompaction(_)) && self.compaction_active {
                    self.compaction_active = false;
                    self.set_phase(
                        ChildMonitorPhase::Pending,
                        Some("compaction ended; next activity unobserved"),
                    );
                    self.snapshot.progress_sequence =
                        self.snapshot.progress_sequence.saturating_add(1);
                } else if let TurnItem::AgentMessage(message) = &event.item {
                    if message.content.iter().any(|item| match item {
                        codex_protocol::items::AgentMessageContent::Text { text } => {
                            !text.is_empty()
                        }
                    }) {
                        self.model_output(&event.turn_id, observed_at_ms);
                        if message.phase != Some(codex_protocol::models::MessagePhase::Commentary) {
                            self.snapshot
                                .final_output_at_ms
                                .get_or_insert(observed_at_ms);
                        }
                    }
                } else if matches!(
                    &event.item,
                    TurnItem::DynamicToolCall(_)
                        | TurnItem::CollabAgentToolCall(_)
                        | TurnItem::Extension(_)
                        | TurnItem::ImageView(_)
                ) {
                    self.tool_finished(event.item.id(), observed_at_ms);
                }
            }
            EventMsg::ExecCommandBegin(event) => {
                self.tool_started(event.call_id.clone(), observed_at_ms);
            }
            EventMsg::ExecCommandEnd(event) => {
                self.tool_finished(event.call_id.clone(), observed_at_ms)
            }
            EventMsg::McpToolCallBegin(event) => {
                self.tool_started(event.call_id.clone(), observed_at_ms);
            }
            EventMsg::McpToolCallEnd(event) => {
                self.tool_finished(event.call_id.clone(), observed_at_ms)
            }
            EventMsg::WebSearchBegin(event) => {
                self.tool_started(event.call_id.clone(), observed_at_ms);
            }
            EventMsg::WebSearchEnd(event) => {
                self.tool_finished(event.call_id.clone(), observed_at_ms)
            }
            EventMsg::ImageGenerationBegin(event) => {
                self.tool_started(event.call_id.clone(), observed_at_ms);
            }
            EventMsg::ImageGenerationEnd(event) => {
                self.tool_finished(event.call_id.clone(), observed_at_ms)
            }
            EventMsg::PatchApplyBegin(event) => {
                self.tool_started(event.call_id.clone(), observed_at_ms);
            }
            EventMsg::PatchApplyEnd(event) => {
                self.tool_finished(event.call_id.clone(), observed_at_ms)
            }
            _ => {}
        }
        if self.snapshot.progress_sequence != previous_progress_sequence {
            self.snapshot.last_activity_at_ms = Some(observed_at_ms);
        }
    }

    /// Records the first host-side result delivery time for the exact terminal turn.
    /// This is not an acknowledgement that the parent model read the result.
    pub(crate) fn collected(&mut self, turn_id: &str, at_ms: i64) -> bool {
        if self.snapshot.turn_id.as_deref() != Some(turn_id)
            || !matches!(
                self.snapshot.phase,
                ChildMonitorPhase::Completed | ChildMonitorPhase::Failed
            )
        {
            return false;
        }
        self.snapshot.result_collected_at_ms.get_or_insert(at_ms);
        true
    }

    pub(crate) fn model_output(&mut self, turn_id: &str, at_ms: i64) {
        if self.snapshot.turn_id.as_deref() != Some(turn_id)
            || matches!(
                self.snapshot.phase,
                ChildMonitorPhase::Completed | ChildMonitorPhase::Failed
            )
        {
            return;
        }
        self.snapshot.first_output_at_ms.get_or_insert(at_ms);
        self.request_timing.output_observed(at_ms);
        self.snapshot.last_output_at_ms = Some(at_ms);
        self.snapshot.last_activity_at_ms = Some(at_ms);
        if !self.tracking_overflowed
            && self.active_tool_calls.is_empty()
            && self.pending_inputs.is_empty()
            && !matches!(self.snapshot.phase, ChildMonitorPhase::Compacting)
        {
            self.set_phase(ChildMonitorPhase::Generating, None);
        }
        self.snapshot.progress_sequence = self.snapshot.progress_sequence.saturating_add(1);
    }

    fn input_requested(&mut self, call_id: &str) {
        if self.pending_inputs.contains(call_id) {
            return;
        }
        if self.pending_inputs.len() >= MAX_ACTIVE_TOOL_CALLS {
            self.tracking_overflowed = true;
            self.set_phase(
                ChildMonitorPhase::Unknown,
                Some("input tracking limit exceeded"),
            );
            self.snapshot.progress_sequence = self.snapshot.progress_sequence.saturating_add(1);
            return;
        }
        self.pending_inputs.insert(call_id.to_string());
        self.snapshot.progress_sequence = self.snapshot.progress_sequence.saturating_add(1);
        self.set_phase(
            ChildMonitorPhase::InputWaiting,
            Some("input or approval response requested"),
        );
    }

    pub(crate) fn input_received(&mut self, call_id: &str) {
        if self.pending_inputs.remove(call_id) {
            self.snapshot.progress_sequence = self.snapshot.progress_sequence.saturating_add(1);
        }
        if self.pending_inputs.is_empty() && self.snapshot.phase == ChildMonitorPhase::InputWaiting
        {
            self.set_phase(
                if self.active_tool_calls.is_empty() {
                    ChildMonitorPhase::Pending
                } else {
                    ChildMonitorPhase::ToolExecution
                },
                None,
            );
        }
    }

    fn start_turn(&mut self, turn_id: &str, at_ms: i64) {
        self.snapshot = ChildMonitorSnapshot {
            turn_id: Some(turn_id.to_string()),
            phase: ChildMonitorPhase::Pending,
            reason: None,
            active_tool_count: 0,
            turn_started_at_ms: Some(at_ms),
            first_output_at_ms: None,
            last_output_at_ms: None,
            final_output_at_ms: None,
            tool_started_at_ms: None,
            tool_ended_at_ms: None,
            turn_completed_at_ms: None,
            result_collected_at_ms: None,
            last_activity_at_ms: Some(at_ms),
            progress_sequence: 1,
            timing: Default::default(),
        };
        self.active_tool_calls.clear();
        self.pending_inputs.clear();
        self.tracking_overflowed = false;
        self.compaction_active = false;
        self.request_timing = Default::default();
    }

    fn set_phase(&mut self, phase: ChildMonitorPhase, reason: Option<&str>) {
        self.snapshot.phase = phase;
        self.snapshot.reason = reason.map(bounded_reason);
    }

    fn set_terminal(&mut self, phase: ChildMonitorPhase, reason: Option<String>, at_ms: i64) {
        let was_terminal = matches!(
            self.snapshot.phase,
            ChildMonitorPhase::Completed | ChildMonitorPhase::Failed
        );
        if was_terminal {
            return;
        }
        self.request_timing.finish_series(at_ms);
        self.snapshot.phase = phase;
        self.snapshot.reason = reason.map(|reason| bounded_reason(&reason));
        self.snapshot.turn_completed_at_ms.get_or_insert(at_ms);
        self.active_tool_calls.clear();
        self.pending_inputs.clear();
        self.snapshot.active_tool_count = 0;
        self.compaction_active = false;
        self.tracking_overflowed = false;
        self.snapshot.progress_sequence = self.snapshot.progress_sequence.saturating_add(1);
    }

    fn tool_started(&mut self, call_id: String, at_ms: i64) {
        if self.active_tool_calls.contains(&call_id) {
            return;
        }
        if self.active_tool_calls.len() >= MAX_ACTIVE_TOOL_CALLS {
            self.tracking_overflowed = true;
            self.set_phase(
                ChildMonitorPhase::Unknown,
                Some("active tool tracking limit exceeded"),
            );
            self.snapshot.progress_sequence = self.snapshot.progress_sequence.saturating_add(1);
            return;
        }
        self.active_tool_calls.insert(call_id);
        self.snapshot.tool_started_at_ms = Some(at_ms);
        self.snapshot.progress_sequence = self.snapshot.progress_sequence.saturating_add(1);
        if self.pending_inputs.is_empty() {
            self.set_phase(ChildMonitorPhase::ToolExecution, None);
        }
        self.snapshot.active_tool_count = self.active_tool_calls.len();
    }

    fn tool_finished(&mut self, call_id: String, at_ms: i64) {
        if self.active_tool_calls.remove(&call_id) {
            self.snapshot.tool_ended_at_ms = Some(at_ms);
            self.snapshot.progress_sequence = self.snapshot.progress_sequence.saturating_add(1);
            if self.active_tool_calls.is_empty()
                && self.snapshot.phase == ChildMonitorPhase::ToolExecution
            {
                self.set_phase(
                    ChildMonitorPhase::Unknown,
                    Some("tool ended; next activity unobserved"),
                );
            }
        }
        self.snapshot.active_tool_count = self.active_tool_calls.len();
    }
}

fn bounded_reason(reason: &str) -> String {
    reason.chars().take(160).collect()
}

#[cfg(test)]
#[path = "child_monitor_tests.rs"]
mod tests;
