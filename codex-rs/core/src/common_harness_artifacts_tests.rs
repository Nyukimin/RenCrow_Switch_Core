// Modified by RenCrow Switch Core, 2026-11-26: verify durable host artifact references.
use super::*;
use serde_json::json;

#[test]
fn operation_artifact_round_trips_with_stable_thread_bound_reference() {
    let home = tempfile::tempdir().expect("host storage");
    let thread = ThreadId::new();
    let payload = json!({
        "task_id": "task-1", "operation_id": "patch-1",
        "patch": "*** Begin Patch\n*** Add File: new.txt\n+new\n*** End Patch",
        "journal": {"before": "sha256:before", "after": "sha256:after"}
    });
    let predicted = predict_operation_artifact_ref(&payload).expect("reserve marker address");
    assert!(!home.path().join(OPERATION_ARTIFACT_DIRECTORY).exists());
    let reference =
        write_operation_artifact(home.path(), thread, &payload).expect("durable artifact write");
    assert_eq!(reference, predicted);
    assert!(reference.starts_with(OPERATION_ARTIFACT_REF_PREFIX));
    assert_eq!(
        write_operation_artifact(home.path(), thread, &payload).expect("idempotent artifact write"),
        reference
    );
    let restored: serde_json::Value = load_operation_artifact(home.path(), thread, &reference)
        .expect("read exact durable payload");
    assert_eq!(restored, payload);
    assert!(
        load_operation_artifact::<serde_json::Value>(home.path(), ThreadId::new(), &reference)
            .is_err()
    );
}

#[test]
fn artifact_references_reject_arbitrary_paths_and_oversized_data() {
    let home = tempfile::tempdir().expect("host storage");
    let thread = ThreadId::new();
    for reference in [
        "../secret",
        "/tmp/secret",
        "common-harness-artifact/1:sha256:../../secret",
    ] {
        assert!(
            load_operation_artifact::<serde_json::Value>(home.path(), thread, reference).is_err()
        );
    }
    let payload = json!({"oversized": "x".repeat(MAX_OPERATION_ARTIFACT_BYTES)});
    assert!(predict_operation_artifact_ref(&payload).is_err());
    assert!(write_operation_artifact(home.path(), thread, &payload).is_err());
}

#[test]
fn corrupted_operation_artifact_is_rejected_and_never_overwritten() {
    let home = tempfile::tempdir().expect("host storage");
    let thread = ThreadId::new();
    let payload = json!({"operation_id": "patch-1", "args": "original"});
    let reference =
        write_operation_artifact(home.path(), thread, &payload).expect("durable artifact write");
    let directory = home
        .path()
        .join(OPERATION_ARTIFACT_DIRECTORY)
        .join(thread.to_string());
    let files = std::fs::read_dir(directory)
        .expect("thread artifact directory")
        .map(|entry| entry.expect("artifact entry").path())
        .collect::<Vec<_>>();
    assert_eq!(files.len(), 1);
    std::fs::write(&files[0], b"corrupted immutable payload").expect("simulate corruption");
    assert!(load_operation_artifact::<serde_json::Value>(home.path(), thread, &reference).is_err());
    assert!(write_operation_artifact(home.path(), thread, &payload).is_err());
    assert_eq!(
        std::fs::read(&files[0]).expect("preserve corrupt evidence"),
        b"corrupted immutable payload".to_vec()
    );
}
