use super::ResponsesStreamRequest;
use super::is_non_retryable_response_outcome;
use super::log_retry;
use crate::session::tests::make_session_and_context;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use std::time::Duration;
use tracing_test::internal::MockWriter;

#[tokio::test]
async fn sampling_retry_logs_stream_error_context() {
    let (_session, turn_context) = make_session_and_context().await;
    let buffer: &'static std::sync::Mutex<Vec<u8>> =
        Box::leak(Box::new(std::sync::Mutex::new(Vec::new())));
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .with_writer(MockWriter::new(buffer))
        .finish();
    let _subscriber_guard = tracing::subscriber::set_default(subscriber);

    log_retry(
        ResponsesStreamRequest::Sampling,
        &turn_context,
        &CodexErr::Stream("websocket closed by server before response.completed".to_string()),
        /*retries*/ 2,
        /*max_retries*/ 5,
        Duration::from_secs(1),
    );

    let logs = String::from_utf8(
        buffer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone(),
    )
    .expect("retry log should be valid utf-8");
    assert!(logs.contains("stream disconnected - retrying sampling request"));
    assert!(logs.contains(&format!("turn_id={}", turn_context.sub_id)));
    assert!(logs.contains("retries=2"));
    assert!(logs.contains("max_retries=5"));
    assert!(logs.contains(
        "sampling_error=stream disconnected before completion: websocket closed by server before response.completed"
    ));
}

#[test]
fn incomplete_and_protocol_terminal_errors_are_not_retried() {
    let errors = [
        CodexErr::new(CodexErrorDetails::IncompleteResponse {
            diagnostic_code: "OUTPUT_TOKEN_LIMIT".to_string(),
            response_id: Some("resp-1".to_string()),
            reason: "max_output_tokens".to_string(),
            token_usage: None,
        }),
        CodexErr::new(CodexErrorDetails::ResponseProtocolFailure {
            response_id: Some("resp-2".to_string()),
            reason: "invalid terminal status".to_string(),
        }),
    ];

    for error in errors {
        assert!(is_non_retryable_response_outcome(error.details()));
        assert!(error.retry_delay(1).is_none());
        assert!(!crate::compact_model_fallback::should_retry_with_current_model(&error));
    }

    let transport_error = CodexErr::new(CodexErrorDetails::Stream("disconnected".to_string()));
    assert!(!is_non_retryable_response_outcome(
        transport_error.details()
    ));
    assert!(transport_error.retry_delay(1).is_some());
}
