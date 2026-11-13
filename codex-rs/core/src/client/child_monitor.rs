// Modified by RenCrow Switch Core, 2026-11-13: metadata-only parent/child request correlation.
use std::time::Instant;

use codex_protocol::ThreadId;
use codex_protocol::protocol::TokenUsage;

const LOG_TARGET: &str = "codex_core::child_monitor";
const MAX_FIELD_CHARS: usize = 160;

/// Metadata-only lifecycle logging for one upstream model request attempt.
pub(super) struct RequestMonitor {
    local_attempt_id: String,
    thread_id: ThreadId,
    parent_thread_id: Option<ThreadId>,
    turn_id: Option<String>,
    parent_turn_id: Option<String>,
    root_turn_id: Option<String>,
    model: String,
    provider: String,
    started_at: Instant,
    upstream_request_id: Option<String>,
    verified_model: Option<String>,
    server_reported_model: Option<String>,
    terminal: bool,
}

impl RequestMonitor {
    pub(super) fn new(
        thread_id: ThreadId,
        parent_thread_id: Option<ThreadId>,
        turn_id: Option<&str>,
        parent_turn_id: Option<&str>,
        root_turn_id: Option<&str>,
        model: &str,
        provider: &str,
    ) -> Self {
        let monitor = Self {
            local_attempt_id: uuid::Uuid::new_v4().to_string(),
            thread_id,
            parent_thread_id,
            turn_id: turn_id.map(bounded_field),
            parent_turn_id: parent_turn_id.map(bounded_field),
            root_turn_id: root_turn_id.map(bounded_field),
            model: bounded_field(model),
            provider: bounded_field(provider),
            started_at: Instant::now(),
            upstream_request_id: None,
            verified_model: None,
            server_reported_model: None,
            terminal: false,
        };
        monitor.log_event("request_started", None, None, None, None);
        monitor
    }

    pub(super) fn headers(
        &mut self,
        upstream_request_id: Option<&str>,
        verified_model: Option<&str>,
    ) {
        if self.terminal {
            return;
        }
        if let Some(id) = upstream_request_id {
            self.upstream_request_id = Some(bounded_field(id));
        }
        if let Some(model) = verified_model {
            self.verified_model = Some(bounded_field(model));
        }
        self.log_event("response_headers", None, None, None, None);
    }

    pub(super) fn server_model(&mut self, model: &str) {
        if self.terminal {
            return;
        }
        self.server_reported_model = Some(bounded_field(model));
        self.log_event("server_reported_model", None, None, None, None);
    }

    pub(super) fn completed(
        &mut self,
        response_id: &str,
        usage: Option<&TokenUsage>,
        end_turn: Option<bool>,
    ) {
        self.finish("completed", None, Some(response_id), usage, end_turn);
    }

    pub(super) fn api_failed(&mut self, error: &codex_api::ApiError) {
        match error {
            codex_api::ApiError::IncompleteResponse {
                diagnostic_code,
                response_id,
                reason,
                token_usage,
            } => {
                self.finish(
                    "incomplete",
                    Some(&format!("{diagnostic_code}: {reason}")),
                    response_id.as_deref(),
                    token_usage.as_ref(),
                    None,
                );
            }
            codex_api::ApiError::ResponseProtocolFailure {
                response_id,
                reason,
            } => {
                self.finish(
                    "protocol_failure",
                    Some(reason),
                    response_id.as_deref(),
                    None,
                    None,
                );
            }
            codex_api::ApiError::Api { status, .. }
            | codex_api::ApiError::Transport(codex_api::TransportError::Http { status, .. }) => {
                self.failed(&format!("http_error_{}", status.as_u16()))
            }
            codex_api::ApiError::Transport(_) => self.failed("transport_error"),
            codex_api::ApiError::Stream(_) => self.failed("response_stream_error"),
            codex_api::ApiError::ContextWindowExceeded => self.failed("context_window_exceeded"),
            codex_api::ApiError::QuotaExceeded => self.failed("quota_exceeded"),
            codex_api::ApiError::UsageNotIncluded => self.failed("usage_not_included"),
            codex_api::ApiError::Retryable { .. } => self.failed("retryable_response_error"),
            codex_api::ApiError::RateLimitExceeded { .. } | codex_api::ApiError::RateLimit(_) => {
                self.failed("rate_limit_exceeded")
            }
            codex_api::ApiError::InvalidRequest { .. } => self.failed("invalid_request"),
            codex_api::ApiError::CyberPolicy { .. } => self.failed("cyber_policy"),
            codex_api::ApiError::BioPolicy { .. } => self.failed("bio_policy"),
            codex_api::ApiError::MisalignmentPolicyViolation { .. } => {
                self.failed("misalignment_policy")
            }
            codex_api::ApiError::ServerOverloaded => self.failed("server_overloaded"),
        }
    }

    pub(super) fn failed(&mut self, reason: &str) {
        self.finish("failed", Some(reason), None, None, None);
    }

    pub(super) fn cancelled(&mut self, reason: &str) {
        self.finish("cancelled", Some(reason), None, None, None);
    }

    pub(super) fn eof(&mut self) {
        self.finish(
            "eof_without_terminal_response",
            Some("stream ended before response.completed"),
            None,
            None,
            None,
        );
    }

    fn finish(
        &mut self,
        event: &'static str,
        reason: Option<&str>,
        response_id: Option<&str>,
        usage: Option<&TokenUsage>,
        end_turn: Option<bool>,
    ) {
        if self.terminal {
            return;
        }
        self.terminal = true;
        self.log_event(event, reason, response_id, usage, end_turn);
    }

    fn log_event(
        &self,
        event: &'static str,
        reason: Option<&str>,
        response_id: Option<&str>,
        usage: Option<&TokenUsage>,
        end_turn: Option<bool>,
    ) {
        let elapsed_ms = i64::try_from(self.started_at.elapsed().as_millis()).unwrap_or(i64::MAX);
        let parent_thread_id = self.parent_thread_id.map(|id| id.to_string());
        let response_id = response_id.map(bounded_field);
        let reason = reason.map(bounded_field);
        tracing::info!(
            target: LOG_TARGET,
            event = event,
            local_attempt_id = %self.local_attempt_id,
            thread_id = %self.thread_id,
            parent_thread_id = parent_thread_id.as_deref().unwrap_or("unknown"),
            turn_id = self.turn_id.as_deref().unwrap_or("unknown"),
            parent_turn_id = self.parent_turn_id.as_deref().unwrap_or("unknown"),
            root_turn_id = self.root_turn_id.as_deref().unwrap_or("unknown"),
            model = %self.model,
            provider = %self.provider,
            upstream_request_id = self.upstream_request_id.as_deref().unwrap_or("unknown"),
            response_id = response_id.as_deref().unwrap_or("unknown"),
            verified_model = self.verified_model.as_deref().unwrap_or("unknown"),
            verified_model_source = if self.verified_model.is_some() { "x-rencrow-verified-model" } else { "unknown" },
            server_reported_model = self.server_reported_model.as_deref().unwrap_or("unknown"),
            reason = reason.as_deref().unwrap_or("unknown"),
            end_turn = ?end_turn,
            usage_available = usage.is_some(),
            input_tokens = usage.map(|usage| usage.input_tokens),
            cached_input_tokens = usage.map(|usage| usage.cached_input_tokens),
            cache_write_input_tokens = usage.map(|usage| usage.cache_write_input_tokens),
            output_tokens = usage.map(|usage| usage.output_tokens),
            reasoning_output_tokens = usage.map(|usage| usage.reasoning_output_tokens),
            total_tokens = usage.map(|usage| usage.total_tokens),
            rollout_budget_units = ?usage.and_then(|usage| usage.codex_rollout_budget_units.as_ref()),
            elapsed_ms,
        );
    }
}

impl Drop for RequestMonitor {
    fn drop(&mut self) {
        if !self.terminal {
            self.terminal = true;
            self.log_event(
                "abandoned_observation",
                Some("terminal response was not observed"),
                None,
                None,
                None,
            );
        }
    }
}

fn bounded_field(value: &str) -> String {
    let mut chars = value.chars();
    let bounded = chars.by_ref().take(MAX_FIELD_CHARS).collect::<String>();
    if chars.next().is_some() {
        format!(
            "{}…",
            bounded
                .chars()
                .take(MAX_FIELD_CHARS - 1)
                .collect::<String>()
        )
    } else {
        bounded
    }
}

#[cfg(test)]
#[path = "child_monitor_tests.rs"]
mod tests;
