use super::LineInspection;
use super::TelemetryKind;
use super::inspect_line;

#[test]
fn invalid_root_reason_does_not_echo_raw_string() {
    let raw = r#""secret marker from the rollout body""#;
    let LineInspection::Invalid { reason } = inspect_line(raw) else {
        panic!("expected invalid root");
    };

    assert_eq!(reason, "json data at line 1 column 37");
    assert!(!reason.contains("secret marker"));
}

#[test]
fn non_object_telemetry_payload_is_invalid() {
    let raw = r#"{"timestamp":"2026-10-04T19:00:14.066Z","ordinal":1,"type":"token_usage_record","payload":42}"#;
    assert!(matches!(inspect_line(raw), LineInspection::Invalid { .. }));
}

#[test]
fn telemetry_body_preserves_decimal_and_large_numbers() {
    let large_number = "18446744073709551616";
    let Ok(expected_number) = serde_json::from_str::<serde_json::Value>(large_number) else {
        return;
    };
    if expected_number.to_string() != large_number {
        return;
    }
    let raw = format!(
        r#"{{"timestamp":"2026-10-04T19:00:14.066Z","ordinal":1,"type":"event_msg","payload":{{"type":"token_count","rate_limits":{{"secondary":{{"used_percent":12.5}}}},"total_tokens":{large_number}}}}}"#
    );
    let LineInspection::Parsed {
        value,
        telemetry: Some(TelemetryKind::TokenCount),
    } = inspect_line(&raw)
    else {
        panic!("expected valid token count telemetry");
    };

    assert_eq!(
        value["payload"]["rate_limits"]["secondary"]["used_percent"].to_string(),
        "12.5"
    );
    assert_eq!(value["payload"]["total_tokens"].to_string(), large_number);
}
