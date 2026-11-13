use super::RequestMonitor;
use codex_api::ApiError;
use codex_protocol::ThreadId;
use codex_protocol::protocol::TokenUsage;

#[test]
#[tracing_test::traced_test]
fn request_monitor_preserves_verified_identity_and_incomplete_usage_once() {
    let mut monitor = RequestMonitor::new(
        ThreadId::new(),
        Some(ThreadId::new()),
        Some("child-turn"),
        Some("parent-turn"),
        Some("root-turn"),
        "requested",
        "gateway",
    );
    monitor.headers(Some("request-id"), Some("gateway-verified"));
    monitor.server_model("server-reported");
    monitor.headers(Some("request-id"), /*verified_model*/ None);
    monitor.api_failed(&ApiError::IncompleteResponse {
        diagnostic_code: "RENCROW_OUTPUT_LIMIT".to_string(),
        response_id: Some("response-id".to_string()),
        reason: "max_output_tokens".to_string(),
        token_usage: Some(TokenUsage {
            output_tokens: 8,
            ..Default::default()
        }),
    });
    monitor.eof();
    drop(monitor);
    logs_assert(|lines: &[&str]| {
        let terminal = lines
            .iter()
            .filter(|line| line.contains("event=\"incomplete\""))
            .copied()
            .collect::<Vec<_>>();
        if terminal.len() != 1 {
            return Err(format!(
                "expected one incomplete terminal, got {}",
                terminal.len()
            ));
        }
        let line = terminal[0];
        for field in [
            "request-id",
            "response-id",
            "gateway-verified",
            "server-reported",
            "child-turn",
            "parent-turn",
            "root-turn",
            "output_tokens=8",
            "max_output_tokens",
        ] {
            if !line.contains(field) {
                return Err(format!("missing correlation field: {field}"));
            }
        }
        if lines.iter().any(|line| {
            line.contains("event=\"eof_without_terminal_response\"")
                || line.contains("event=\"abandoned_observation\"")
        }) {
            return Err("terminal was logged twice".to_string());
        }
        Ok(())
    });
}

#[test]
#[tracing_test::traced_test]
fn request_monitor_records_http_failure_category_without_response_body() {
    let mut monitor = RequestMonitor::new(
        ThreadId::new(),
        None,
        Some("turn"),
        None,
        None,
        "model",
        "gateway",
    );
    monitor.api_failed(&ApiError::Api {
        status: http::StatusCode::BAD_GATEWAY,
        message: "private-response-body-marker".to_string(),
    });
    drop(monitor);
    assert!(logs_contain("http_error_502"));
    assert!(!logs_contain("private-response-body-marker"));
}
