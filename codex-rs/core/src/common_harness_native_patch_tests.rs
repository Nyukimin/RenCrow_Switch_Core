// Modified by RenCrow Switch Core, 2026-11-27: verify host-owned patch runner identity.
use super::NativePatchExecutorProvenanceV1;
use super::NativeProcessProbeV1;
use super::capture_before_effect;
use super::probe_executor_dead;
use pretty_assertions::assert_eq;

const ARTIFACT_REF: &str = "common-harness-artifact/1:sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn provenance_for_process(process_id: u32) -> NativePatchExecutorProvenanceV1 {
    let thread_id = "thread-native-patch-provenance-test";
    NativePatchExecutorProvenanceV1 {
        format: "common-harness-native-patch-executor/1".into(),
        operation_id: "patch:provenance-test".into(),
        thread_id: thread_id.into(),
        runtime_owner_id: crate::session::common_harness::common_harness_runtime_owner_id(
            thread_id,
        ),
        owner_epoch: 3,
        process_id,
    }
}

#[test]
fn capture_uses_actual_process_and_host_runtime_owner() {
    let thread_id = "thread-capture-test";
    let provenance = capture_before_effect("patch:capture-test", thread_id, 5)
        .expect("capture before native effect");
    assert_eq!(provenance.process_id(), std::process::id());
    assert_eq!(provenance.operation_id(), "patch:capture-test");
    assert_eq!(provenance.thread_id(), thread_id);
    assert_eq!(provenance.owner_epoch(), 5);
    assert_eq!(
        provenance.runtime_owner_id(),
        crate::session::common_harness::common_harness_runtime_owner_id(thread_id)
    );
}

#[test]
fn serialized_provenance_rejects_model_supplied_process_identity() {
    let mut json = serde_json::to_value(provenance_for_process(std::process::id())).unwrap();
    json.as_object_mut()
        .unwrap()
        .insert("model_pid".into(), serde_json::json!(9));
    assert!(serde_json::from_value::<NativePatchExecutorProvenanceV1>(json).is_err());
}

#[cfg(unix)]
#[test]
fn live_current_process_is_not_proven_dead() {
    let probe = probe_executor_dead(&provenance_for_process(std::process::id()), ARTIFACT_REF)
        .expect("probe current process");
    assert!(matches!(probe, NativeProcessProbeV1::NotProven(_)));
}

#[cfg(unix)]
#[test]
fn owned_waited_child_gets_positive_esrch_proof() {
    let mut child = std::process::Command::new("sh")
        .arg("-c")
        .arg("exit 0")
        .spawn()
        .expect("spawn controlled own child");
    let child_pid = child.id();
    child.wait().expect("wait owned child");
    let probe = probe_executor_dead(&provenance_for_process(child_pid), ARTIFACT_REF)
        .expect("probe only the waited child");
    assert!(matches!(probe, NativeProcessProbeV1::ProvenDead(proof)
        if proof.process_id == child_pid && proof.errno == libc::ESRCH));
}

#[test]
fn malformed_identity_and_process_ids_are_rejected() {
    let mut invalid = provenance_for_process(std::process::id());
    invalid.operation_id.clear();
    assert!(probe_executor_dead(&invalid, ARTIFACT_REF).is_err());

    let mut invalid = provenance_for_process(std::process::id());
    invalid.thread_id.clear();
    assert!(probe_executor_dead(&invalid, ARTIFACT_REF).is_err());

    assert!(probe_executor_dead(&provenance_for_process(0), ARTIFACT_REF).is_err());
    assert!(probe_executor_dead(&provenance_for_process(u32::MAX), ARTIFACT_REF).is_err());
    assert!(probe_executor_dead(&provenance_for_process(std::process::id()), "model-ref").is_err());
}
