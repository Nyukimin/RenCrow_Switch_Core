// Modified by RenCrow Switch Core, 2026-11-27: verify actual process restart after inline compaction.
use super::CommonTaskFixtureVariant;
use super::common_task_in_store_variant_with_provider;
use crate::client_common::Prompt;
use crate::session::common_harness_completion::CommonHarnessTurnEndDecision;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context_with_resumed_history_and_config;
use crate::session::turn::build_prompt;
use crate::session::turn_context::TurnContext;
use crate::tools::context::ToolPayload;
use crate::tools::parallel::ToolCallRuntime;
use crate::tools::router::ToolCall;
use crate::tools::runtimes::apply_patch::PatchTestObserver;
use codex_history::InitialHistory;
use codex_history::task_context::CommonHarnessStateV1;
use codex_history::task_context::ConditionKindV1;
use codex_history::task_context::ConditionVerdictV1;
use codex_history::task_context::EvidenceLifecycleV1;
use codex_history::task_context::OperationKindV1;
use codex_history::task_context::OperationOutcomeV1;
use codex_history::task_context::TaskRunStatusV1;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use core_test_support::responses;
use pretty_assertions::assert_eq;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

const CHILD_MODE: &str = "CODEX_COMMON_PROMPT_AT03_CHILD";
const MANIFEST_ENV: &str = "CODEX_COMMON_PROMPT_AT03_MANIFEST";
const CHILD_TEST: &str = "tools::handlers::task_context_boundary::tests::common_harness_lifecycle_tests::prompt_at03_tests::at03_compaction_patch_worker";
const PROMPT_KIND: &str = "rencrow.common_harness_prompt";
const ROOT_TASK_ID: &str = "harness-root";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct At03ProcessManifestV1 {
    format: String,
    child_pid: u32,
    codex_home: PathBuf,
    workspace_root: PathBuf,
    configured_cwd: PathBuf,
    permission_profile: codex_protocol::models::PermissionProfile,
    go_cache: String,
    rollout_path: PathBuf,
    thread_id: String,
    root_task_id: String,
    baseline_call_id: String,
    patch_operation_id: String,
    patch_effect_count_path: PathBuf,
    state_after_patch: CommonHarnessStateV1,
}

struct ChildWorkspaceCleanup(PathBuf);

impl Drop for ChildWorkspaceCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn write_manifest(path: &Path, manifest: &At03ProcessManifestV1) {
    let bytes = serde_json::to_vec(manifest).expect("encode child manifest");
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, bytes).expect("write AT03 manifest temp");
    std::fs::File::open(&temporary)
        .and_then(|file| file.sync_all())
        .expect("sync manifest temp");
    std::fs::rename(&temporary, path).expect("publish child manifest");
    std::fs::File::open(path)
        .and_then(|file| file.sync_all())
        .expect("sync child manifest");
    std::fs::File::open(path.parent().unwrap())
        .and_then(|directory| directory.sync_all())
        .expect("sync manifest directory");
}

fn prompt_projection(prompt: &Prompt) -> Option<Value> {
    for item in &prompt.input {
        let ResponseItem::Message {
            content,
            internal_chat_message_metadata_passthrough,
            ..
        } = item
        else {
            continue;
        };
        let marked = internal_chat_message_metadata_passthrough
            .as_ref()
            .and_then(|metadata| metadata.content_item_kinds.as_deref())
            .is_some_and(|kinds| kinds.iter().any(|kind| kind.0 == PROMPT_KIND));
        if !marked {
            continue;
        }
        let Some(ContentItem::InputText { text }) = content.first() else {
            continue;
        };
        let body = text
            .strip_prefix("<common_harness_prompt>")?
            .strip_suffix("</common_harness_prompt>")?;
        return serde_json::from_str(body.split_once('\n')?.1).ok();
    }
    None
}

async fn actual_prompt(
    session: &Arc<crate::session::session::Session>,
    turn: &Arc<TurnContext>,
) -> Prompt {
    session
        .common_harness_inference_deadline(turn)
        .await
        .expect("restored task has a bounded inference window");
    let step = StepContext::for_test(Arc::clone(turn));
    let mut input = session
        .clone_history()
        .await
        .for_prompt(&step.settings.model_info.input_modalities);
    session
        .append_common_harness_prompt_context(turn, &mut input)
        .await
        .expect("complete restored Common projection fits");
    build_prompt(
        input,
        step.as_ref(),
        session.get_prompt_base_instructions().await,
    )
}

async fn run_registered_check_at(
    runtime: &ToolCallRuntime,
    session: &Arc<crate::session::session::Session>,
    turn: &Arc<TurnContext>,
    workspace: &Path,
    call_id: &str,
    argv: &[String],
    artifact_paths: &Value,
) -> String {
    let scope = session
        .active_task_scope()
        .await
        .expect("active host task scope");
    let arguments = serde_json::json!({
        "cmd": shlex::try_join(argv.iter().map(String::as_str)).unwrap(),
        "workdir": workspace.to_string_lossy(),
        "yield_time_ms": 30000,
        "timeout_ms": 30000,
        "task_evidence": {
            "task_id": scope.task_id,
            "task_revision": scope.task_revision,
            "artifact_paths": artifact_paths,
            "check_kind": "verification"
        }
    });
    super::record_call_with_arguments(
        session,
        turn,
        call_id,
        "exec_command",
        &arguments.to_string(),
    )
    .await;
    let output = runtime
        .clone()
        .handle_tool_call(
            ToolCall {
                tool_name: codex_tools::ToolName::plain("exec_command"),
                call_id: call_id.into(),
                payload: ToolPayload::Function {
                    arguments: arguments.to_string(),
                },
                encrypted_function_args: None,
            },
            CancellationToken::new(),
        )
        .await
        .expect("native verifier tool result");
    session
        .record_annotated_conversation_items(turn, turn.model_info().as_ref(), vec![output.clone()])
        .await;
    format!("{output:?}")
}

#[tokio::test]
#[cfg(unix)]
async fn at03_compaction_patch_worker() {
    if std::env::var_os(CHILD_MODE).is_none() {
        return;
    }
    let manifest_path =
        PathBuf::from(std::env::var_os(MANIFEST_ENV).expect("parent manifest path"));
    let server = responses::start_mock_server().await;
    let provider = codex_model_provider_info::ModelProviderInfo::create_openai_provider(Some(
        format!("{}/v1", server.uri()),
    ));
    let (workspace, session, fixture_turn) = common_task_in_store_variant_with_provider(
        true,
        true,
        CommonTaskFixtureVariant::At03PreBeginNotes,
        Some(provider),
    )
    .await;
    let workspace_root = workspace.path().canonicalize().unwrap();
    let configured_cwd = fixture_turn.config.cwd.as_path().to_path_buf();
    let codex_home = fixture_turn.config.codex_home.as_path().to_path_buf();
    let go_cache = fixture_turn
        .config
        .permissions
        .shell_environment_policy
        .r#set
        .get("GOCACHE")
        .expect("fixture GOCACHE")
        .clone();
    let thread_id = session.thread_id().to_string();
    let rollout_path = session.current_rollout_path().await.unwrap().unwrap();
    let turn = fixture_turn;
    let initial = session
        .current_common_harness_state(ROOT_TASK_ID)
        .await
        .unwrap();
    let reproduce = initial
        .request
        .conditions
        .iter()
        .find(|condition| condition.kind == ConditionKindV1::ReproduceFailure)
        .unwrap();
    let suite = initial
        .request
        .conditions
        .iter()
        .find(|condition| condition.kind == ConditionKindV1::TestSuite)
        .unwrap();
    let reproduce_check = reproduce.check.as_ref().unwrap();
    let suite_check = suite.check.as_ref().unwrap();
    assert_eq!(reproduce_check.argv, suite_check.argv);
    let artifact_paths = serde_json::to_value(&reproduce_check.cwd_binding_refs).unwrap();
    let runtime = super::producer_tests::native_runtime(&session, &turn);
    let baseline_call_id = "at03-process-baseline";
    let baseline_output = run_registered_check_at(
        &runtime,
        &session,
        &turn,
        &configured_cwd,
        baseline_call_id,
        &reproduce_check.argv,
        &artifact_paths,
    )
    .await;
    let baseline_receipt = session.task_check_receipt(baseline_call_id).await.unwrap();
    assert_eq!(baseline_receipt.exit_code, 1, "{baseline_output}");
    assert!(!baseline_receipt.succeeded);
    let baseline_evidence = session
        .current_common_harness_state(ROOT_TASK_ID)
        .await
        .unwrap()
        .evidence_refs
        .into_iter()
        .rev()
        .find(|evidence| evidence.condition_id == reproduce.condition_id)
        .unwrap();
    assert_eq!(baseline_evidence.verdict, ConditionVerdictV1::Fail);
    assert!(baseline_evidence.host_negative_proof.is_some());

    let patch_effect_count_path = workspace_root.join(".host/at03-native-patch-count.log");
    turn.extension_data.insert(PatchTestObserver {
        counter_path: patch_effect_count_path.clone(),
        stage_trace_path: workspace_root.join(".host/at03-native-patch-stage.log"),
        crash: None,
    });
    let patch = "*** Begin Patch\n*** Update File: value.go\n@@\n-package harness\n-func Value() int { return 1 }\n+package harness\n+func Value() int { return 2 }\n*** End Patch";
    let patch_output = runtime
        .clone()
        .handle_tool_call(
            ToolCall {
                tool_name: codex_tools::ToolName::plain("apply_patch"),
                call_id: "at03-process-patch".into(),
                payload: ToolPayload::Custom {
                    input: patch.into(),
                },
                encrypted_function_args: None,
            },
            CancellationToken::new(),
        )
        .await
        .expect("actual native patch before compaction");
    assert_eq!(
        std::fs::read_to_string(workspace_root.join("value.go")).unwrap(),
        "package harness\nfunc Value() int { return 2 }\n",
        "{patch_output:?}"
    );
    assert_eq!(
        std::fs::read_to_string(workspace_root.join("value_test.go")).unwrap(),
        "package harness\nimport \"testing\"\nfunc TestHarness(t *testing.T) { if Value() != 2 { t.Fatal(\"value expected 2, got 1\") } }\n",
        "native patch keeps the registered verifier source unchanged"
    );
    assert_eq!(
        effect_counter_lines(&patch_effect_count_path),
        vec!["native-effect-count:1"]
    );
    let state_after_patch = session
        .current_common_harness_state(ROOT_TASK_ID)
        .await
        .unwrap();
    let patch_operation_id = state_after_patch
        .operation_refs
        .iter()
        .find(|operation| operation.intent.operation_kind == OperationKindV1::ApplyPatch)
        .expect("actual persisted patch operation")
        .intent
        .operation_id
        .clone();
    assert_eq!(
        state_after_patch
            .operation_refs
            .iter()
            .filter(|operation| operation.intent.operation_kind == OperationKindV1::ApplyPatch)
            .count(),
        1
    );
    let stale_baseline = state_after_patch
        .evidence_refs
        .iter()
        .find(|evidence| evidence.evidence_id == baseline_evidence.evidence_id)
        .unwrap();
    assert_eq!(stale_baseline.lifecycle, EvidenceLifecycleV1::Stale);

    let summary = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_assistant_message(
                "at03-process-summary",
                "Retain the Common task facts.",
            ),
            responses::ev_completed("at03-process-summary-response"),
        ]),
    )
    .await;
    crate::compact::run_inline_auto_compact_task(
        Arc::clone(&session),
        Arc::clone(&turn),
        crate::compact::InitialContextInjection::DoNotInject,
        codex_analytics::CompactionReason::ContextLimit,
        codex_analytics::CompactionPhase::MidTurn,
    )
    .await
    .expect("actual child inline compaction");
    assert_eq!(summary.requests().len(), 1);
    session
        .flush_rollout()
        .await
        .expect("flush actual compacted rollout");
    assert!(matches!(
        crate::rollout::recorder::RolloutRecorder::get_rollout_history(&rollout_path)
            .await
            .unwrap(),
        InitialHistory::Resumed(_)
    ));
    write_manifest(
        &manifest_path,
        &At03ProcessManifestV1 {
            format: "common-harness-prompt-at03/1".into(),
            child_pid: std::process::id(),
            codex_home,
            workspace_root,
            configured_cwd,
            permission_profile: turn.config.permissions.effective_permission_profile(),
            go_cache,
            rollout_path,
            thread_id,
            root_task_id: ROOT_TASK_ID.into(),
            baseline_call_id: baseline_call_id.into(),
            patch_operation_id,
            patch_effect_count_path,
            state_after_patch,
        },
    );
    let _kept_path = workspace.keep();
    std::process::exit(87);
}

#[tokio::test]
#[cfg(unix)]
async fn real_inline_compaction_and_patch_survive_owned_process_restart_then_complete() {
    if std::env::var_os(CHILD_MODE).is_some() {
        return;
    }
    let parent_temp = tempfile::tempdir().expect("parent manifest directory");
    let manifest_path = parent_temp.path().join("common-prompt-at03.json");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg(CHILD_TEST)
        .arg("--nocapture")
        .env(CHILD_MODE, "1")
        .env(MANIFEST_ENV, &manifest_path)
        .spawn()
        .expect("spawn owned compaction worker");
    let child_pid = child.id();
    let status = child.wait().expect("wait for exactly the spawned child");
    assert_eq!(status.code(), Some(87));
    let manifest: At03ProcessManifestV1 = serde_json::from_slice(
        &std::fs::read(&manifest_path).expect("child synced manifest before exit"),
    )
    .expect("decode exact child state manifest");
    assert_eq!(manifest.format, "common-harness-prompt-at03/1");
    assert_eq!(manifest.child_pid, child_pid);
    let _workspace_cleanup = ChildWorkspaceCleanup(manifest.workspace_root.clone());
    assert!(manifest.rollout_path.is_file());
    assert_eq!(
        effect_counter_lines(&manifest.patch_effect_count_path),
        vec!["native-effect-count:1"]
    );
    assert_eq!(
        std::fs::read_to_string(manifest.workspace_root.join("value.go")).unwrap(),
        "package harness\nfunc Value() int { return 2 }\n"
    );

    let resumed = match crate::rollout::recorder::RolloutRecorder::get_rollout_history(
        &manifest.rollout_path,
    )
    .await
    .expect("read exact child rollout")
    {
        InitialHistory::Resumed(resumed) => InitialHistory::Resumed(resumed),
        other => panic!("child rollout is not resumable: {other:?}"),
    };
    let (fresh, resumed_turn) = make_session_and_context_with_resumed_history_and_config(
        &manifest.codex_home,
        &manifest.configured_cwd,
        resumed,
        super::common_harness_native_patch_test_runtime_paths(),
        Some(manifest.permission_profile.clone()),
    )
    .await
    .expect("same-home, same-cwd fresh LocalThreadStore session with trace/runtime");
    assert_eq!(
        resumed_turn
            .config
            .permissions
            .shell_environment_policy
            .r#set
            .get("GOCACHE"),
        Some(&manifest.go_cache),
        "cold helper configures the same Go cache before runtime environment construction"
    );
    assert_eq!(
        resumed_turn.config.cwd.as_path(),
        manifest.configured_cwd.as_path()
    );
    assert_eq!(
        resumed_turn.config.codex_home.as_path(),
        manifest.codex_home.as_path()
    );
    let (root, adopted) = fresh
        .common_harness_turn_state(&resumed_turn)
        .await
        .unwrap()
        .expect("public host state path performs cold writer takeover");
    assert_eq!(root, manifest.root_task_id);
    assert_eq!(adopted.request, manifest.state_after_patch.request);
    assert_eq!(
        adopted.run_state.target_version,
        manifest.state_after_patch.run_state.target_version
    );
    assert_eq!(
        adopted.run_state.attempt_no,
        manifest.state_after_patch.run_state.attempt_no
    );
    assert_eq!(
        adopted.run_state.work_window,
        manifest.state_after_patch.run_state.work_window
    );
    assert_eq!(
        adopted.run_state.unmet_conditions,
        manifest.state_after_patch.run_state.unmet_conditions
    );
    assert_eq!(
        adopted.run_state.next_operation,
        manifest.state_after_patch.run_state.next_operation
    );
    assert_eq!(
        adopted.evidence_refs,
        manifest.state_after_patch.evidence_refs
    );
    assert_ne!(
        adopted.run_state.owner_id,
        manifest.state_after_patch.run_state.owner_id
    );
    let baseline = adopted
        .evidence_refs
        .iter()
        .find(|evidence| {
            adopted.request.conditions.iter().any(|condition| {
                condition.kind == ConditionKindV1::ReproduceFailure
                    && condition.condition_id == evidence.condition_id
            })
        })
        .expect("historical baseline evidence");
    assert_eq!(baseline.lifecycle, EvidenceLifecycleV1::Stale);
    assert!(baseline.host_negative_proof.is_some());
    let raw_baseline = fresh
        .task_check_receipt(&manifest.baseline_call_id)
        .await
        .unwrap();
    assert_eq!(raw_baseline.exit_code, 1);
    assert!(!raw_baseline.succeeded);
    let patch_operation = adopted
        .operation_refs
        .iter()
        .find(|operation| operation.intent.operation_id == manifest.patch_operation_id)
        .unwrap();
    assert_eq!(
        patch_operation.receipt.as_ref().unwrap().outcome,
        OperationOutcomeV1::Succeeded
    );
    assert_eq!(
        adopted
            .operation_refs
            .iter()
            .filter(|operation| operation.intent.operation_kind == OperationKindV1::ApplyPatch)
            .count(),
        1
    );

    let prompt = actual_prompt(&fresh, &resumed_turn).await;
    let projection =
        prompt_projection(&prompt).expect("actual resumed build_prompt includes projection");
    assert_eq!(projection["request"]["task_id"], adopted.request.task_id);
    assert_eq!(projection["request"]["goal"], adopted.request.goal);
    assert_eq!(
        projection["request"]["constraints"],
        serde_json::to_value(&adopted.request.constraints).unwrap()
    );
    assert_eq!(
        projection["run_state"]["target_version"],
        adopted.run_state.target_version
    );
    assert_eq!(
        projection["run_state"]["attempt_no"],
        adopted.run_state.attempt_no
    );
    assert_eq!(
        projection["run_state"]["max_attempts"],
        adopted.request.max_attempts
    );
    assert_eq!(
        projection["run_state"]["next_operation"],
        adopted.run_state.next_operation
    );
    assert_eq!(
        projection["run_state"]["work_window"],
        serde_json::to_value(&adopted.run_state.work_window).unwrap()
    );
    assert!(
        !projection["current_evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["evidence_id"] == baseline.evidence_id)
    );
    for condition in adopted
        .request
        .conditions
        .iter()
        .filter(|condition| condition.required)
    {
        let projected = projection["request"]["conditions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["condition_id"] == condition.condition_id)
            .unwrap();
        assert_eq!(
            projected["condition_revision"],
            condition.condition_revision
        );
        assert_eq!(
            projected["check"],
            serde_json::to_value(condition.check.as_ref().unwrap()).unwrap()
        );
    }
    assert!(projection.get("operation_refs").is_none());
    assert!(projection.get("raw_output").is_none());

    let reproduce = adopted
        .request
        .conditions
        .iter()
        .find(|c| c.kind == ConditionKindV1::ReproduceFailure)
        .unwrap();
    let suite = adopted
        .request
        .conditions
        .iter()
        .find(|c| c.kind == ConditionKindV1::TestSuite)
        .unwrap();
    assert_eq!(
        reproduce.check.as_ref().unwrap().argv,
        suite.check.as_ref().unwrap().argv
    );
    let runtime = super::producer_tests::native_runtime(&fresh, &resumed_turn);
    let candidate_call_id = "at03-process-candidate-after-restart";
    let candidate_output = run_registered_check_at(
        &runtime,
        &fresh,
        &resumed_turn,
        &manifest.configured_cwd,
        candidate_call_id,
        &suite.check.as_ref().unwrap().argv,
        &serde_json::to_value(&suite.check.as_ref().unwrap().cwd_binding_refs).unwrap(),
    )
    .await;
    let candidate_receipt = fresh.task_check_receipt(candidate_call_id).await.unwrap();
    assert_eq!(candidate_receipt.exit_code, 0, "{candidate_output}");
    assert!(candidate_receipt.succeeded);
    let after_candidate = fresh
        .current_common_harness_state(&manifest.root_task_id)
        .await
        .unwrap();
    for id in [&reproduce.condition_id, &suite.condition_id] {
        let evidence = after_candidate
            .evidence_refs
            .iter()
            .rev()
            .find(|evidence| {
                evidence.condition_id == *id
                    && evidence.target_version == after_candidate.run_state.target_version
                    && evidence.lifecycle == EvidenceLifecycleV1::Current
            })
            .unwrap();
        assert_eq!(evidence.verdict, ConditionVerdictV1::Pass);
    }
    assert!(after_candidate.run_state.unmet_conditions.is_empty());
    assert_ne!(after_candidate.run_state.status, TaskRunStatusV1::Completed);
    assert_eq!(
        effect_counter_lines(&manifest.patch_effect_count_path),
        vec!["native-effect-count:1"]
    );

    let accept_call_id = "at03-process-accept-after-restart";
    let accept_args = serde_json::json!({"action":"accept", "result":"Registered suite passed after process restart."});
    super::record_call_with_arguments(
        &fresh,
        &resumed_turn,
        accept_call_id,
        crate::tools::handlers::task_context_boundary_spec::TASK_CONTEXT_BOUNDARY_TOOL_NAME,
        &accept_args.to_string(),
    )
    .await;
    let output = runtime
        .clone()
        .handle_tool_call(
            ToolCall {
                tool_name: codex_tools::ToolName::plain("task_context_boundary"),
                call_id: accept_call_id.into(),
                payload: ToolPayload::Function {
                    arguments: accept_args.to_string(),
                },
                encrypted_function_args: None,
            },
            CancellationToken::new(),
        )
        .await
        .expect("native host-gated Accept");
    assert!(
        matches!(&output.item, ResponseItem::FunctionCallOutput { output, .. }
            if output.success == Some(true)),
        "native post-restart Accept failed: {output:?}"
    );
    fresh
        .record_annotated_conversation_items(
            &resumed_turn,
            resumed_turn.model_info().as_ref(),
            vec![output],
        )
        .await;
    assert!(fresh.pending_task_boundary().await.is_some());
    assert!(
        fresh
            .commit_prepared_task_boundary(&resumed_turn, &CancellationToken::new())
            .await
            .unwrap()
    );
    assert!(fresh.active_task_scope().await.is_none());
    assert!(fresh.autonomous_completion_is_complete().await);
    assert!(matches!(
        fresh
            .common_harness_turn_end_gate(&resumed_turn)
            .await
            .unwrap(),
        CommonHarnessTurnEndDecision::Completed { .. }
    ));
    assert_eq!(
        fresh
            .current_common_harness_state(&manifest.root_task_id)
            .await
            .unwrap()
            .run_state
            .status,
        TaskRunStatusV1::Completed
    );
    assert_eq!(
        effect_counter_lines(&manifest.patch_effect_count_path),
        vec!["native-effect-count:1"]
    );
}

fn effect_counter_lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .expect("synced patch observer counter survives process exit")
        .lines()
        .map(str::to_owned)
        .collect()
}
