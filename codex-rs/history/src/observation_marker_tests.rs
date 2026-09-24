use super::*;
use crate::archive_reference::ObservationReference;
use crate::archive_reference::content_sha256;
use crate::observation_projection::project_observation;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

fn projection(output: &str) -> ObservationProjection {
    let reference = ObservationReference::new("thread-v2", "call-1", content_sha256(output));
    project_observation(&reference, "exec_command", "{\"cmd\":\"cat log\"}", output)
        .expect("fixture observation should project")
}

#[test]
fn marker_body_is_deterministic_historical_data_with_only_presented_excerpts() {
    let output = format!("HEAD{}TAIL", "m".repeat(10_000));
    let projection = projection(&output);
    let body = observation_marker_body(&projection);
    assert_eq!(body, observation_marker_body(&projection));
    assert!(body.len() < output.len());

    let value: Value = serde_json::from_str(&body).expect("marker body is JSON");
    assert_eq!(value["rencrow_observation"], json!(true));
    assert_eq!(value["version"], json!(2));
    assert_eq!(value["call_id"], json!("call-1"));
    assert_eq!(value["tool"], json!("exec_command"));
    assert_eq!(value["sha256"], json!(content_sha256(&output)));
    assert_eq!(value["total_bytes"], json!(output.len()));
    assert_eq!(value["partial"], json!(true));
    assert_eq!(value["instruction"], json!(OBSERVATION_MARKER_INSTRUCTION));
    let excerpts = value["excerpts"].as_array().expect("excerpts");
    assert_eq!(excerpts.len(), 2);
    assert!(excerpts[0].as_str().unwrap().starts_with("HEAD"));
    assert!(excerpts[1].as_str().unwrap().ends_with("TAIL"));
    // The unpresented middle is not copied into the marker.
    assert!(!body.contains(&"m".repeat(2_049)));
}

#[test]
fn marker_metadata_keeps_bookkeeping_drops_truncation_and_rejects_provenance() {
    let projection = projection("done");
    let canonical = CodexHarnessMetadata {
        history_truncation_token_limit: Some(12_000),
        ..Default::default()
    };
    let metadata = observation_marker_metadata(Some(&canonical), &projection.coverage).unwrap();
    assert_eq!(metadata.history_truncation_token_limit, None);
    assert_eq!(
        metadata.rencrow_observation_projection,
        Some(projection.coverage.clone())
    );

    for provenance in [
        CodexHarnessMetadata {
            rencrow_input: Some(json!({"author": "human"})),
            ..Default::default()
        },
        CodexHarnessMetadata {
            rencrow_compaction: Some(json!({"version": 2})),
            ..Default::default()
        },
        CodexHarnessMetadata {
            rencrow_observation_projection: Some(projection.coverage.clone()),
            ..Default::default()
        },
    ] {
        assert!(observation_marker_metadata(Some(&provenance), &projection.coverage).is_err());
    }
}

#[test]
fn marker_verification_requires_exact_regenerated_body_and_metadata() {
    let output = "x".repeat(5_000);
    let projection = projection(&output);
    let metadata = observation_marker_metadata(None, &projection.coverage).unwrap();
    let body = observation_marker_body(&projection);
    assert_eq!(
        verify_observation_marker(&projection, None, Some(&metadata), &body),
        Ok(())
    );

    assert!(verify_observation_marker(&projection, None, None, &body).is_err());
    let mut changed = metadata.clone();
    changed.history_truncation_token_limit = Some(1);
    assert!(verify_observation_marker(&projection, None, Some(&changed), &body).is_err());
    let edited = body.replace("Archived", "Current");
    assert!(verify_observation_marker(&projection, None, Some(&metadata), &edited).is_err());
    // A marker made from different canonical bytes does not verify.
    let other = self::projection(&"y".repeat(5_000));
    assert!(verify_observation_marker(&other, None, Some(&metadata), &body).is_err());
}
