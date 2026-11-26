// Modified by RenCrow Switch Core, 2026-11-27: AT04 cold native-patch recovery acceptance.
//! Exercises controlled child-process interruption and cold recovery through
//! the public Common state path using the ordinary native ApplyPatch runtime.

use crate::common_harness_artifacts::load_operation_artifact;
use crate::common_harness_native_patch::CommonHarnessPatchArtifactV1;
use crate::common_harness_native_patch::NativePatchEffectCompleteV1;
use crate::session::common_harness::common_harness_runtime_owner_id;
use crate::session::tests::make_session_and_context_with_resumed_history_and_config;
use crate::tools::runtimes::apply_patch::PatchTestCrashConfig;
use crate::tools::runtimes::apply_patch::PatchTestCrashPhase;
use crate::tools::runtimes::apply_patch::PatchTestObserver;
use codex_history::InitialHistory;
use codex_protocol::ThreadId;
use codex_protocol::models::PermissionProfile;
use serde::Deserialize;
use serde::Serialize;
use std::path::PathBuf;
use std::process::Command;

const CHILD_MODE: &str = "CODEX_NATIVE_PATCH_AT04_CHILD";
const CHILD_CRASH_PHASE_ENV: &str = "CODEX_NATIVE_PATCH_AT04_CRASH_PHASE";
const SECOND_OPEN_MODE: &str = "CODEX_NATIVE_PATCH_AT04_SECOND_OPEN";
const MANIFEST_ENV: &str = "CODEX_NATIVE_PATCH_AT04_MANIFEST";
const CHILD_TEST: &str = "tools::handlers::task_context_boundary::tests::common_harness_lifecycle_tests::native_patch_recovery_tests::native_patch_crash_worker";
const SECOND_OPEN_TEST: &str = "tools::handlers::task_context_boundary::tests::common_harness_lifecycle_tests::native_patch_recovery_tests::native_patch_second_writer_open_worker";
const ROOT_TASK_ID: &str = "harness-root";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ColdChildManifestV1 {
    format: String,
    codex_home: PathBuf,
    workspace_root: PathBuf,
    rollout_path: PathBuf,
    thread_id: String,
    root_task_id: String,
    permission_snapshot_ref: String,
    permission_approval_policy: String,
    permission_allow_login_shell: bool,
    permission_effective_profile_sha256: String,
    effective_permission_profile: PermissionProfile,
    operation_id: Option<String>,
    operation_artifact_ref: Option<String>,
    operation_executor_owner_id: String,
    operation_executor_pid: u32,
    effect_counter_path: PathBuf,
}

fn write_manifest(path: &std::path::Path, manifest: &ColdChildManifestV1) {
    let bytes = serde_json::to_vec(manifest).expect("encode child manifest");
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, bytes).expect("write child manifest temp");
    std::fs::rename(temporary, path).expect("publish child manifest atomically");
    std::fs::File::open(path)
        .and_then(|file| file.sync_all())
        .expect("sync child manifest");
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .expect("sync child manifest directory");
    }
}

#[tokio::test]
#[cfg(unix)]
async fn cold_child_patch_all_post_recovers_missing_receipt_once() {
    if std::env::var_os(CHILD_MODE).is_some() {
        return;
    }
    let parent_temp = tempfile::tempdir().expect("shared parent fixture root");
    let manifest_path = parent_temp.path().join("cold-child-manifest.json");
    let mut child = Command::new(std::env::current_exe().expect("test executable"))
        .arg("--exact")
        .arg(CHILD_TEST)
        .arg("--nocapture")
        .env(CHILD_MODE, "1")
        .env(CHILD_CRASH_PHASE_ENV, "after_completion_marker")
        .env(MANIFEST_ENV, &manifest_path)
        .spawn()
        .expect("spawn controlled Switch child");
    let child_pid = child.id();
    let status = child.wait().expect("wait only for owned child");
    assert_eq!(
        status.code(),
        Some(86),
        "child must stop at after-effect failpoint"
    );

    let manifest: ColdChildManifestV1 = serde_json::from_slice(
        &std::fs::read(&manifest_path).expect("child persisted manifest before patch"),
    )
    .expect("decode child manifest");
    assert_eq!(manifest.format, "common-harness-native-patch-at04/1");
    assert_eq!(manifest.operation_executor_pid, child_pid);
    assert!(
        manifest.operation_id.is_some(),
        "runtime failpoint must report actual reserved operation"
    );
    assert!(
        manifest
            .operation_artifact_ref
            .as_deref()
            .is_some_and(|reference| reference.starts_with("common-harness-artifact/1:sha256:")),
        "runtime failpoint reports the immutable pre-effect operation artifact ref"
    );
    let operation_artifact = load_operation_artifact::<CommonHarnessPatchArtifactV1>(
        &manifest.codex_home,
        ThreadId::try_from(manifest.thread_id.as_str()).expect("manifest thread id is valid"),
        manifest.operation_artifact_ref.as_deref().unwrap(),
    )
    .expect("load the exact immutable pre-effect operation artifact");
    assert_eq!(
        operation_artifact.operation_id,
        manifest.operation_id.clone().unwrap()
    );
    assert_eq!(operation_artifact.executor.process_id(), child_pid);
    assert_eq!(
        operation_artifact.executor.runtime_owner_id(),
        manifest.operation_executor_owner_id
    );
    let completion_marker = load_operation_artifact::<NativePatchEffectCompleteV1>(
        &manifest.codex_home,
        ThreadId::try_from(manifest.thread_id.as_str()).expect("manifest thread id is valid"),
        &operation_artifact.completion_marker_ref,
    )
    .expect("successful effect marker was synced before child exit");
    assert_eq!(
        completion_marker,
        operation_artifact.native_patch_completion_marker()
    );
    assert_eq!(
        crate::common_harness_artifacts::predict_operation_artifact_ref(&completion_marker)
            .unwrap(),
        operation_artifact.completion_marker_ref,
    );
    assert!(matches!(
        crate::common_harness_native_patch::probe_executor_dead(
            &operation_artifact.executor,
            manifest.operation_artifact_ref.as_deref().unwrap(),
        )
        .unwrap(),
        crate::common_harness_native_patch::NativeProcessProbeV1::ProvenDead(_)
    ));
    assert!(manifest.rollout_path.is_file());
    assert_eq!(
        effect_counter_lines(&manifest.effect_counter_path),
        vec!["native-effect-count:1".to_string()]
    );
    assert_eq!(
        std::fs::read_to_string(manifest.workspace_root.join("value.go")).unwrap(),
        "package harness\nfunc Value() int { return 2 }\n",
        "the separate counter proves the actual patch effect happened before child exit"
    );

    // Reopen the exact same persisted thread, host artifact directory, and
    // workspace after the original executor process has exited. The helper
    // constructs Session::new with InitialHistory::Resumed and the same home.
    let resumed_history = match crate::rollout::recorder::RolloutRecorder::get_rollout_history(
        &manifest.rollout_path,
    )
    .await
    .expect("load exact child rollout")
    {
        InitialHistory::Resumed(resumed) => InitialHistory::Resumed(resumed),
        other => panic!("child rollout is not resumable: {other:?}"),
    };
    let (session, turn) = make_session_and_context_with_resumed_history_and_config(
        &manifest.codex_home,
        &manifest.workspace_root,
        resumed_history,
        super::common_harness_native_patch_test_runtime_paths(),
        Some(manifest.effective_permission_profile.clone()),
    )
    .await
    .expect("cold-load same root session");
    let persisted_state = session
        .current_common_harness_state(ROOT_TASK_ID)
        .await
        .expect("load task permission binding before recovery");
    let cold_permission =
        crate::session::tests::common_harness_permission_snapshot_diagnostics_for_test(
            &turn.config,
        )
        .expect("compute cold permission snapshot");
    assert_eq!(
        cold_permission.snapshot_ref,
        manifest.permission_snapshot_ref,
        "cold permission snapshot must equal the original request snapshot; original policy={} allow_login_shell={} profile_hash={}, cold policy={} allow_login_shell={} profile_hash={}",
        manifest.permission_approval_policy,
        manifest.permission_allow_login_shell,
        manifest.permission_effective_profile_sha256,
        cold_permission.approval_policy,
        cold_permission.allow_login_shell,
        cold_permission.effective_profile_sha256,
    );
    assert_eq!(
        persisted_state.request.permission_snapshot_ref, manifest.permission_snapshot_ref,
        "durable request keeps the original permission snapshot"
    );
    turn.extension_data.insert(PatchTestObserver {
        counter_path: manifest.effect_counter_path.clone(),
        stage_trace_path: manifest
            .effect_counter_path
            .with_extension("stage-trace.log"),
        crash: None,
    });
    let second_writer = Command::new(std::env::current_exe().expect("test executable"))
        .arg("--exact")
        .arg(SECOND_OPEN_TEST)
        .arg("--nocapture")
        .env(SECOND_OPEN_MODE, "1")
        .env(MANIFEST_ENV, &manifest_path)
        .status()
        .expect("run a second process against the same persisted writer");
    assert!(
        second_writer.success(),
        "a second process must observe the existing live-writer exclusion"
    );
    let before = session
        .current_common_harness_state(&manifest.root_task_id)
        .await
        .unwrap();
    assert_eq!(
        before.run_state.owner_id,
        manifest.operation_executor_owner_id
    );
    assert!(!before.run_state.unsettled_operation_refs.is_empty());
    let operation_id = manifest.operation_id.as_deref().unwrap();
    let before_record = before
        .operation_refs
        .iter()
        .find(|record| record.intent.operation_id == operation_id)
        .expect("persisted original operation reservation");
    assert!(
        before_record.receipt.is_none(),
        "child exited before raw receipt persistence"
    );
    let before_execution_claim = before_record
        .claims
        .iter()
        .find(|claim| claim.kind == codex_history::task_context::OperationClaimKindV1::Execution)
        .expect("original execution claim was durable before native effect");
    let original_execution_claim_id = before_execution_claim.claim_id.clone();
    let original_execution_owner_epoch = before_execution_claim.owner_epoch;
    let attempt_no = before.run_state.attempt_no;
    let (root, adopted) = session
        .common_harness_turn_state(&turn)
        .await
        .unwrap()
        .expect("real root state invokes sealed LocalThreadStore writer takeover");
    assert_eq!(root, manifest.root_task_id);
    assert_ne!(
        adopted.run_state.owner_id,
        manifest.operation_executor_owner_id
    );

    let record = adopted
        .operation_refs
        .iter()
        .find(|record| record.intent.operation_id == operation_id)
        .expect("recovered original operation");
    assert_eq!(
        record
            .claims
            .iter()
            .filter(
                |claim| claim.kind == codex_history::task_context::OperationClaimKindV1::Execution
            )
            .count(),
        1,
        "recovery must not create a second execution claim"
    );
    let original_execution_claim = record
        .claims
        .iter()
        .find(|claim| claim.kind == codex_history::task_context::OperationClaimKindV1::Execution)
        .expect("original execution claim");
    assert_eq!(
        original_execution_claim.claim_id,
        original_execution_claim_id
    );
    assert_eq!(
        original_execution_claim.owner_epoch,
        original_execution_owner_epoch
    );
    let raw_unknown = record
        .receipt
        .as_ref()
        .expect("recovery records the missing raw receipt");
    assert_eq!(
        raw_unknown.outcome,
        codex_history::task_context::OperationOutcomeV1::Unknown
    );
    assert_eq!(raw_unknown.execution_id, original_execution_claim.claim_id);
    assert_eq!(
        raw_unknown.owner_epoch,
        original_execution_claim.owner_epoch
    );
    assert_eq!(
        raw_unknown.actual_before_version,
        record.intent.base_target_version
    );
    assert_eq!(raw_unknown.actual_after_version, None);
    assert_eq!(raw_unknown.exit_status, None);
    assert_eq!(record.finalizations.len(), 1);
    let settled = record
        .effective_receipt()
        .expect("canonical AllPost finalization");
    assert_eq!(
        settled.outcome,
        codex_history::task_context::OperationOutcomeV1::Succeeded
    );
    let patch_artifact = load_operation_artifact::<CommonHarnessPatchArtifactV1>(
        &manifest.codex_home,
        ThreadId::try_from(manifest.thread_id.as_str()).expect("manifest thread id is valid"),
        manifest.operation_artifact_ref.as_deref().unwrap(),
    )
    .expect("load original immutable patch journal");
    assert_eq!(
        settled.actual_after_version.as_deref(),
        Some(patch_artifact.journal.predicted_after.digest.as_str())
    );
    assert!(adopted.run_state.unsettled_operation_refs.is_empty());
    assert_eq!(adopted.run_state.attempt_no, attempt_no);
    assert_eq!(
        effect_counter_lines(&manifest.effect_counter_path),
        vec!["native-effect-count:1".to_string()]
    );

    let (duplicate_root, duplicate) = session
        .common_harness_turn_state(&turn)
        .await
        .unwrap()
        .expect("repeated public state gate is idempotent");
    assert_eq!(duplicate_root, root);
    assert_eq!(
        duplicate, adopted,
        "a second gate cannot take over, finalize, or reapply again"
    );
    assert_eq!(
        effect_counter_lines(&manifest.effect_counter_path),
        vec!["native-effect-count:1".to_string()]
    );
}

#[tokio::test]
#[cfg(unix)]
async fn cold_child_patch_without_completion_marker_stays_blocked_and_unsettled() {
    if std::env::var_os(CHILD_MODE).is_some() {
        return;
    }
    let parent_temp = tempfile::tempdir().expect("shared parent fixture root");
    let manifest_path = parent_temp
        .path()
        .join("cold-child-no-marker-manifest.json");
    let mut child = Command::new(std::env::current_exe().expect("test executable"))
        .arg("--exact")
        .arg(CHILD_TEST)
        .arg("--nocapture")
        .env(CHILD_MODE, "1")
        .env(CHILD_CRASH_PHASE_ENV, "before_completion_marker")
        .env(MANIFEST_ENV, &manifest_path)
        .spawn()
        .expect("spawn controlled Switch child before marker persistence");
    let child_pid = child.id();
    let status = child.wait().expect("wait only for owned child");
    assert_eq!(
        status.code(),
        Some(86),
        "child stops after effect before marker"
    );

    let manifest: ColdChildManifestV1 = serde_json::from_slice(
        &std::fs::read(&manifest_path).expect("child persisted manifest before patch"),
    )
    .expect("decode child manifest");
    assert_eq!(manifest.operation_executor_pid, child_pid);
    let operation_id = manifest
        .operation_id
        .as_deref()
        .expect("reserved patch operation");
    let operation_artifact_ref = manifest
        .operation_artifact_ref
        .as_deref()
        .expect("immutable pre-effect operation artifact ref");
    let thread_id =
        ThreadId::try_from(manifest.thread_id.as_str()).expect("manifest thread id is valid");
    let operation_artifact = load_operation_artifact::<CommonHarnessPatchArtifactV1>(
        &manifest.codex_home,
        thread_id,
        operation_artifact_ref,
    )
    .expect("load immutable pre-effect patch artifact");
    assert_eq!(operation_artifact.operation_id, operation_id);
    assert!(matches!(
        crate::common_harness_native_patch::probe_executor_dead(
            &operation_artifact.executor,
            operation_artifact_ref,
        )
        .unwrap(),
        crate::common_harness_native_patch::NativeProcessProbeV1::ProvenDead(_)
    ));
    assert!(
        load_operation_artifact::<NativePatchEffectCompleteV1>(
            &manifest.codex_home,
            thread_id,
            &operation_artifact.completion_marker_ref,
        )
        .is_err(),
        "real child exited before publishing its completion marker"
    );
    assert_eq!(
        std::fs::read_to_string(manifest.workspace_root.join("value.go")).unwrap(),
        "package harness\nfunc Value() int { return 2 }\n",
        "the child completed the native patch effect before the failpoint"
    );
    assert_eq!(
        effect_counter_lines(&manifest.effect_counter_path),
        vec!["native-effect-count:1".to_string()]
    );

    let resumed_history = match crate::rollout::recorder::RolloutRecorder::get_rollout_history(
        &manifest.rollout_path,
    )
    .await
    .expect("load exact child rollout")
    {
        InitialHistory::Resumed(resumed) => InitialHistory::Resumed(resumed),
        other => panic!("child rollout is not resumable: {other:?}"),
    };
    let (session, turn) = make_session_and_context_with_resumed_history_and_config(
        &manifest.codex_home,
        &manifest.workspace_root,
        resumed_history,
        super::common_harness_native_patch_test_runtime_paths(),
        Some(manifest.effective_permission_profile.clone()),
    )
    .await
    .expect("cold-load same root session");
    let cold_permission =
        crate::session::tests::common_harness_permission_snapshot_diagnostics_for_test(
            &turn.config,
        )
        .expect("compute cold permission snapshot");
    assert_eq!(
        cold_permission.snapshot_ref,
        manifest.permission_snapshot_ref,
        "cold permission snapshot must equal the original request snapshot; original policy={} allow_login_shell={} profile_hash={}, cold policy={} allow_login_shell={} profile_hash={}",
        manifest.permission_approval_policy,
        manifest.permission_allow_login_shell,
        manifest.permission_effective_profile_sha256,
        cold_permission.approval_policy,
        cold_permission.allow_login_shell,
        cold_permission.effective_profile_sha256,
    );
    turn.extension_data.insert(PatchTestObserver {
        counter_path: manifest.effect_counter_path.clone(),
        stage_trace_path: manifest
            .effect_counter_path
            .with_extension("stage-trace.log"),
        crash: None,
    });

    let (root, recovered) = session
        .common_harness_turn_state(&turn)
        .await
        .expect("public Common state path keeps unresolved patch visible")
        .expect("restored Common request");
    assert_eq!(root, manifest.root_task_id);
    assert_eq!(
        recovered.run_state.status,
        codex_history::task_context::TaskRunStatusV1::Blocked
    );
    assert!(
        recovered
            .run_state
            .unsettled_operation_refs
            .contains(&operation_id.to_string())
    );
    let record = recovered
        .operation_refs
        .iter()
        .find(|record| record.intent.operation_id == operation_id)
        .expect("original patch operation remains in history");
    let raw_unknown = record
        .receipt
        .as_ref()
        .expect("host records missing receipt as Unknown");
    assert_eq!(
        raw_unknown.outcome,
        codex_history::task_context::OperationOutcomeV1::Unknown
    );
    assert_eq!(raw_unknown.execution_id, record.claims[0].claim_id);
    assert_eq!(
        record.finalizations.len(),
        0,
        "missing marker cannot settle AllPost"
    );
    assert_eq!(
        record
            .claims
            .iter()
            .filter(
                |claim| claim.kind == codex_history::task_context::OperationClaimKindV1::Execution
            )
            .count(),
        1,
        "recovery does not reexecute without the completion marker"
    );
    assert_eq!(recovered.run_state.attempt_no, record.intent.attempt_no);
    assert!(
        load_operation_artifact::<NativePatchEffectCompleteV1>(
            &manifest.codex_home,
            thread_id,
            &operation_artifact.completion_marker_ref,
        )
        .is_err(),
        "recovery does not manufacture a missing effect marker"
    );
    assert_eq!(
        effect_counter_lines(&manifest.effect_counter_path),
        vec!["native-effect-count:1".to_string()],
        "public cold state recovery never applies the patch a second time"
    );
}

fn effect_counter_lines(path: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .expect("native patch runtime synced its pre-effect counter")
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Runs in a separate Switch process. It uses the ordinary Begin and native
/// ApplyPatch runtime; the cfg(test) runtime failpoint exits after the effect,
/// before any receipt/result finalization can be published.
#[tokio::test]
#[cfg(unix)]
async fn native_patch_crash_worker() {
    if std::env::var_os(CHILD_MODE).is_none() {
        return;
    }
    let manifest_path = PathBuf::from(std::env::var_os(MANIFEST_ENV).unwrap());
    let (workspace, session, turn) = super::common_task_in_store_variant(
        true,
        true,
        super::CommonTaskFixtureVariant::FreeformApplyPatchTool,
    )
    .await;
    let canonical_workspace = std::fs::canonicalize(workspace.path()).unwrap();
    let _retained_workspace = workspace.keep();
    let executor_owner_id = common_harness_runtime_owner_id(&session.thread_id().to_string());
    let codex_home = turn.config.codex_home.as_path().to_path_buf();
    let thread_id = session.thread_id();
    let rollout_path = session.current_rollout_path().await.unwrap().unwrap();
    let effect_counter_path = canonical_workspace.join(".host/native-patch-effect-count.log");
    let patch = "*** Begin Patch\n*** Update File: value.go\n@@\n-func Value() int { return 1 }\n+func Value() int { return 2 }\n*** End Patch";
    let manifest = ColdChildManifestV1 {
        format: "common-harness-native-patch-at04/1".into(),
        codex_home,
        workspace_root: canonical_workspace.clone(),
        rollout_path,
        thread_id: thread_id.to_string(),
        root_task_id: ROOT_TASK_ID.into(),
        permission_snapshot_ref: String::new(),
        permission_approval_policy: String::new(),
        permission_allow_login_shell: false,
        permission_effective_profile_sha256: String::new(),
        effective_permission_profile: turn.config.permissions.effective_permission_profile(),
        operation_id: None,
        operation_artifact_ref: None,
        operation_executor_owner_id: executor_owner_id,
        operation_executor_pid: std::process::id(),
        effect_counter_path: effect_counter_path.clone(),
    };
    let mut manifest = manifest;
    let initial_permission =
        crate::session::tests::common_harness_permission_snapshot_diagnostics_for_test(
            &turn.config,
        )
        .expect("compute initial permission snapshot");
    let initial_state = session
        .current_common_harness_state(ROOT_TASK_ID)
        .await
        .expect("load initial request permission binding");
    assert_eq!(
        initial_permission.snapshot_ref, initial_state.request.permission_snapshot_ref,
        "initial test fixture permission snapshot must match Begin's persisted request"
    );
    manifest.permission_snapshot_ref = initial_permission.snapshot_ref;
    manifest.permission_approval_policy = initial_permission.approval_policy;
    manifest.permission_allow_login_shell = initial_permission.allow_login_shell;
    manifest.permission_effective_profile_sha256 = initial_permission.effective_profile_sha256;
    write_manifest(&manifest_path, &manifest);
    turn.extension_data.insert(PatchTestObserver {
        counter_path: effect_counter_path,
        stage_trace_path: canonical_workspace.join(".host/native-patch-stage-trace.log"),
        crash: Some(PatchTestCrashConfig {
            manifest_path: manifest_path.clone(),
            phase: match std::env::var(CHILD_CRASH_PHASE_ENV).as_deref() {
                Ok("before_completion_marker") => PatchTestCrashPhase::BeforeCompletionMarker,
                Ok("after_completion_marker") => PatchTestCrashPhase::AfterCompletionMarker,
                other => panic!("invalid test crash phase: {other:?}"),
            },
        }),
    });

    // Existing test harness invocation follows the real handler/runtime path.
    // The requested cfg(test) failpoint is in ApplyPatchRuntime immediately
    // after apply_patch_with_options returns and before receipt persistence.
    let call_id = "at04-native-patch-call";
    let result = super::producer_tests::native_runtime(&session, &turn)
        .handle_tool_call(
            crate::tools::router::ToolCall {
                tool_name: codex_tools::ToolName::plain("apply_patch"),
                call_id: call_id.into(),
                payload: crate::tools::context::ToolPayload::Custom {
                    input: patch.into(),
                },
                encrypted_function_args: None,
            },
            tokio_util::sync::CancellationToken::new(),
        )
        .await;
    panic!("test failpoint did not terminate the child before receipt: {result:?}");
}

#[tokio::test]
#[cfg(unix)]
async fn native_patch_second_writer_open_worker() {
    if std::env::var_os(SECOND_OPEN_MODE).is_none() {
        return;
    }
    let manifest_path = PathBuf::from(std::env::var_os(MANIFEST_ENV).unwrap());
    let manifest: ColdChildManifestV1 =
        serde_json::from_slice(&std::fs::read(&manifest_path).expect("read cold child manifest"))
            .expect("decode cold child manifest");
    let resumed_history = match crate::rollout::recorder::RolloutRecorder::get_rollout_history(
        &manifest.rollout_path,
    )
    .await
    .expect("load exact existing thread rollout")
    {
        InitialHistory::Resumed(resumed) => InitialHistory::Resumed(resumed),
        other => panic!("child rollout is not resumable: {other:?}"),
    };
    let reopened = make_session_and_context_with_resumed_history_and_config(
        &manifest.codex_home,
        &manifest.workspace_root,
        resumed_history,
        super::common_harness_native_patch_test_runtime_paths(),
        Some(manifest.effective_permission_profile.clone()),
    )
    .await;
    assert!(
        reopened.is_err(),
        "LocalThreadStore must reject a second live writer for this thread and home"
    );
}
