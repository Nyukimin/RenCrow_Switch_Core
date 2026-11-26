use super::common_task;
use super::common_task_with_baseline_framework_error;
use super::common_task_with_freeform_apply_patch_tool;
use super::common_task_with_inconsistent_suite_expected_counts;
use super::common_task_with_reproduction;
use super::common_task_with_suite_excluding_baseline_failure;
use super::record_call_with_arguments;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::tests::tool_registry_for_test_step;
use crate::session::turn_context::TurnContext;
use crate::tools::context::ToolPayload;
use crate::tools::parallel::ToolCallRuntime;
use crate::tools::registry::ToolExecutor;
use crate::tools::router::ToolCall;
use crate::tools::router::ToolRouter;
use crate::tools::runtimes::apply_patch::PatchTestObserver;
use crate::turn_diff_tracker::TurnDiffTracker;
use codex_history::task_context::ConditionKindV1;
use codex_history::task_context::ConditionVerdictV1;
use codex_history::task_context::EvidenceLifecycleV1;
use codex_history::task_context::OperationKindV1;
use codex_history::task_context::OperationOutcomeV1;
use codex_history::task_context::TaskRunStatusV1;
use pretty_assertions::assert_eq;
use std::process::Command;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub(super) fn native_runtime(session: &Arc<Session>, turn: &Arc<TurnContext>) -> ToolCallRuntime {
    let step_context = StepContext::for_test(Arc::clone(turn));
    let (registry, hosted_specs) = tool_registry_for_test_step(step_context.as_ref());
    let router = Arc::new(ToolRouter::from_registry(
        step_context.turn.as_ref(),
        step_context.turn.model_info(),
        registry,
        hosted_specs,
        &Default::default(),
    ));
    ToolCallRuntime::new(
        Arc::clone(session),
        step_context.with_tool_router_for_test(router),
        Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
    )
}

fn apply_fixture_go_cache(command: &mut Command, turn: &TurnContext) {
    let go_cache = turn
        .config
        .permissions
        .shell_environment_policy
        .r#set
        .get("GOCACHE")
        .expect("common-task fixture binds a writable Go cache");
    command.env("GOCACHE", go_cache);
}

pub(super) async fn run_registered_check(
    runtime: &ToolCallRuntime,
    session: &Arc<Session>,
    turn: &Arc<TurnContext>,
    workspace: &tempfile::TempDir,
    call_id: &str,
    argv: &[String],
    artifact_paths: &serde_json::Value,
) -> String {
    let scope = session
        .active_task_scope()
        .await
        .expect("active task scope");
    let arguments = serde_json::json!({
        "cmd": shlex::try_join(argv.iter().map(String::as_str)).expect("quote registered argv"),
        "workdir": workspace.path().to_string_lossy(),
        "yield_time_ms": 30000,
        "timeout_ms": 30000,
        "task_evidence": {
            "task_id": scope.task_id,
            "task_revision": scope.task_revision,
            "artifact_paths": artifact_paths,
            "check_kind": "verification"
        }
    });
    record_call_with_arguments(
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
        .expect("native verifier result");
    session
        .record_annotated_conversation_items(turn, turn.model_info().as_ref(), vec![output.clone()])
        .await;
    format!("{output:?}")
}

#[tokio::test]
async fn registered_baseline_failure_records_partial_fail_evidence_and_keeps_raw_failure() {
    let (workspace, session, turn) = common_task().await;
    let initial = session
        .current_common_harness_state("harness-root")
        .await
        .expect("host-registered common-harness task");
    let reproduce = initial
        .request
        .conditions
        .iter()
        .find(|condition| condition.condition_id == "AC-reproduce")
        .expect("registered baseline condition");
    let check = reproduce.check.as_ref().expect("registered Go check");
    let go_executable =
        std::path::PathBuf::from(check.argv.first().expect("host-pinned Go executable"));
    assert!(
        go_executable.is_file(),
        "the registered toolchain is resolvable"
    );

    let compile_output = workspace.path().join(".host").join("baseline.test");
    let mut compile_command = Command::new(&go_executable);
    compile_command
        .args(["test", "-c", "-o"])
        .arg(&compile_output)
        .arg(".")
        .current_dir(workspace.path());
    apply_fixture_go_cache(&mut compile_command, &turn);
    let compile = compile_command
        .output()
        .expect("launch compile-only check with the registered Go executable");
    assert!(
        compile.status.success(),
        "the registered Go fixture must compile before its runtime result is evaluated: {}",
        String::from_utf8_lossy(&compile.stderr)
    );

    let step_context = StepContext::for_test(Arc::clone(&turn));
    let (registry, hosted_specs) = tool_registry_for_test_step(step_context.as_ref());
    let router = Arc::new(ToolRouter::from_registry(
        step_context.turn.as_ref(),
        step_context.turn.model_info(),
        registry,
        hosted_specs,
        &Default::default(),
    ));
    let step_context = step_context.with_tool_router_for_test(router);
    let runtime = ToolCallRuntime::new(
        Arc::clone(&session),
        step_context,
        Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
    );
    let call_id = "common-harness-baseline-native";
    let command = shlex::try_join(check.argv.iter().map(String::as_str))
        .expect("quote the registered verifier argv");
    let scope = session
        .active_task_scope()
        .await
        .expect("active execution scope");
    let arguments = serde_json::json!({
        "cmd": command,
        "workdir": workspace.path().to_string_lossy(),
        "yield_time_ms": 30000,
        "timeout_ms": 30000,
        "task_evidence": {
            "task_id": scope.task_id,
            "task_revision": scope.task_revision,
            "artifact_paths": check.cwd_binding_refs,
            "check_kind": "verification"
        }
    });
    let tool_result = runtime
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
        .expect("registered exec tool call returns a tool result");

    let state = session
        .current_common_harness_state("harness-root")
        .await
        .expect("common-harness state after baseline execution");
    assert_eq!(
        state.run_state.status,
        codex_history::task_context::TaskRunStatusV1::Running
    );
    let operation = state
        .operation_refs
        .iter()
        .find(|operation| {
            operation.intent.operation_kind
                == codex_history::task_context::OperationKindV1::RunVerifier
        })
        .unwrap_or_else(|| {
            panic!("registered verifier must reach native execution: {tool_result:?}")
        });
    let receipt = operation.receipt.as_ref().expect("raw execution receipt");
    assert_eq!(receipt.outcome, OperationOutcomeV1::Failed);
    assert_eq!(receipt.exit_status, Some(1));
    let raw_task_check = session.task_check_receipt(call_id).await;
    let raw_task_check_snapshot = session.task_check_snapshot().await;
    let raw_task_check = raw_task_check.unwrap_or_else(|| {
        panic!(
            "normal one-shot settlement stores the raw task-check receipt; tool_result={tool_result:?}; snapshots={raw_task_check_snapshot:#?}"
        )
    });
    assert_eq!(raw_task_check.call_id, call_id);
    assert_eq!(raw_task_check.task_id, "harness-work");
    assert_eq!(raw_task_check.task_revision, 1);
    assert!(raw_task_check.completed);
    assert_eq!(raw_task_check.exit_code, 1);
    assert!(!raw_task_check.succeeded);
    assert_eq!(
        raw_task_check.artifact_before, raw_task_check.artifact_after,
        "the failed check did not modify its registered inputs"
    );
    let failure_observation = raw_task_check.observation.as_ref();
    let diagnostic_excerpt = failure_observation.map(|observation| {
        observation
            .diagnostic_excerpt
            .chars()
            .take(1024)
            .collect::<String>()
    });
    let launch_failure =
        failure_observation.and_then(|observation| observation.launch_failure.as_ref());
    assert!(
        failure_observation.is_some_and(|observation| {
            observation
                .diagnostic_excerpt
                .contains("value expected 2, got 1")
        }),
        "expected fixture regression failure; launch_failure={launch_failure:?}; bounded_diagnostic={diagnostic_excerpt:?}; tool_result={tool_result:?}"
    );
    let evidence = state
        .evidence_refs
        .iter()
        .find(|evidence| evidence.condition_id == "AC-reproduce")
        .unwrap_or_else(|| {
            panic!("baseline failure must become partial reproduction evidence: {tool_result:?}")
        });
    assert_eq!(evidence.verdict, ConditionVerdictV1::Fail);
    assert_eq!(evidence.operation_id, operation.intent.operation_id);
    assert!(evidence.host_negative_proof.as_ref().is_some_and(|proof| {
        proof.expected_failure_identity == "TestHarness: value expected 2, got 1"
            && proof.observed_failure_identity == proof.expected_failure_identity
            && proof.resolution_receipt_ref.is_none()
    }));
    assert_eq!(
        evidence.target_version, state.request.start_target_version,
        "baseline evidence is bound to the registered starting target"
    );
    let persisted_task_check = raw_task_check_snapshot
        .iter()
        .find(|snapshot| snapshot.attempt.call_id == call_id)
        .expect("raw TaskCheck snapshot retains the failed native call");
    let negative_record = serde_json::to_value(persisted_task_check)
        .expect("serialize raw TaskCheck snapshot")["expected_negative"]
        .clone();
    assert_eq!(
        negative_record["resolution"]["observed_failure_identity"],
        "TestHarness: value expected 2, got 1"
    );
    assert_eq!(
        negative_record["resolution"]["raw_output_sha256"],
        raw_task_check
            .observation
            .as_ref()
            .expect("failed receipt carries output facts")
            .diagnostic_sha256
    );
}

#[tokio::test]
async fn registered_go_framework_error_is_persisted_and_stops_the_turn() {
    let (workspace, session, turn) = common_task_with_reproduction(false).await;
    let initial = session
        .current_common_harness_state("harness-root")
        .await
        .expect("host-registered common-harness task");
    let condition = initial
        .request
        .conditions
        .iter()
        .find(|condition| condition.kind == ConditionKindV1::TestSuite)
        .cloned()
        .expect("registered candidate suite condition");
    let check = condition.check.as_ref().expect("registered Go check");

    // Make a framework/compiler error in the actual registered workspace. Refresh the
    // host target before invoking the still-registered Go argv through ToolCallRuntime.
    std::fs::write(
        workspace.path().join("value.go"),
        "package harness\nfunc Value( {\n",
    )
    .expect("write a real Go syntax error");
    assert!(matches!(
        session.common_harness_turn_end_gate(&turn).await.unwrap(),
        crate::session::common_harness_completion::CommonHarnessTurnEndDecision::Continue { .. }
    ));

    let paths = serde_json::to_value(&check.cwd_binding_refs).unwrap();
    let runtime = native_runtime(&session, &turn);
    let output = run_registered_check(
        &runtime,
        &session,
        &turn,
        &workspace,
        "common-harness-framework-error",
        &check.argv,
        &paths,
    )
    .await;
    let raw = session
        .task_check_receipt("common-harness-framework-error")
        .await
        .expect("the real launched Go command retains its raw receipt");
    assert!(raw.completed);
    assert_eq!(raw.exit_code, 1, "{output}");
    assert!(!raw.succeeded);
    let diagnostic = raw.observation.as_ref().expect("raw host observation");
    assert!(
        diagnostic.diagnostic_excerpt.contains("syntax")
            || diagnostic.diagnostic_excerpt.contains("expected"),
        "expected a real Go parser/framework diagnostic: {diagnostic:?}; {output}"
    );
    assert!(diagnostic.launch_failure.is_none());

    let state = session
        .current_common_harness_state("harness-root")
        .await
        .expect("state after framework error");
    let evidence = state
        .evidence_refs
        .iter()
        .rev()
        .find(|evidence| {
            evidence.condition_id == condition.condition_id
                && evidence.target_version == state.run_state.target_version
                && evidence.lifecycle == EvidenceLifecycleV1::Current
        })
        .expect("current evidence records the framework error");
    assert_eq!(evidence.verdict, ConditionVerdictV1::Error);
    assert_eq!(
        evidence.operation_id,
        state.operation_refs.last().unwrap().intent.operation_id
    );

    let decision = session.common_harness_turn_end_gate(&turn).await.unwrap();
    assert!(
        matches!(
            decision,
            crate::session::common_harness_completion::CommonHarnessTurnEndDecision::Stopped {
                status: TaskRunStatusV1::Blocked,
                ..
            }
        ),
        "known framework errors need a finite blocked/recovery decision: {decision:?}"
    );
}

#[tokio::test]
async fn baseline_go_framework_error_is_not_an_expected_negative_pass() {
    let (workspace, session, turn) = common_task_with_baseline_framework_error().await;
    let initial = session
        .current_common_harness_state("harness-root")
        .await
        .expect("normal Begin registers the deliberately broken baseline");
    let reproduction = initial
        .request
        .conditions
        .iter()
        .find(|condition| condition.kind == ConditionKindV1::ReproduceFailure)
        .expect("registered baseline reproduction condition");
    let check = reproduction.check.as_ref().expect("registered Go check");
    assert_eq!(
        initial.run_state.target_version, initial.request.start_target_version,
        "the broken baseline begins at the declared start snapshot"
    );

    let paths = serde_json::to_value(&check.cwd_binding_refs).unwrap();
    let runtime = native_runtime(&session, &turn);
    let output = run_registered_check(
        &runtime,
        &session,
        &turn,
        &workspace,
        "common-harness-baseline-framework-error",
        &check.argv,
        &paths,
    )
    .await;
    let raw = session
        .task_check_receipt("common-harness-baseline-framework-error")
        .await
        .expect("real Go launch must preserve its raw framework-error receipt");
    assert!(raw.completed);
    assert_eq!(raw.exit_code, 1, "{output}");
    assert!(!raw.succeeded);
    let diagnostic = raw.observation.as_ref().expect("bounded Go diagnostics");
    assert!(diagnostic.launch_failure.is_none());
    assert!(
        diagnostic.diagnostic_excerpt.contains("syntax")
            || diagnostic.diagnostic_excerpt.contains("expected"),
        "expected actual Go framework error: {diagnostic:?}; {output}"
    );

    let state = session
        .current_common_harness_state("harness-root")
        .await
        .expect("state after baseline framework error");
    let evidence = state
        .evidence_refs
        .iter()
        .rev()
        .find(|evidence| {
            evidence.condition_id == reproduction.condition_id
                && evidence.target_version == state.request.start_target_version
                && evidence.lifecycle == EvidenceLifecycleV1::Current
        })
        .expect("framework failure must become explicit baseline evidence");
    assert_eq!(evidence.verdict, ConditionVerdictV1::Error);
    assert!(
        evidence.host_negative_proof.is_none(),
        "compiler/framework errors can never resolve the expected-negative baseline"
    );

    let decision = session.common_harness_turn_end_gate(&turn).await.unwrap();
    assert!(
        matches!(
            decision,
            crate::session::common_harness_completion::CommonHarnessTurnEndDecision::Stopped {
                status: TaskRunStatusV1::Blocked,
                ..
            }
        ),
        "baseline framework error must stop with bounded recovery: {decision:?}"
    );
}

#[tokio::test]
async fn registered_baseline_patch_and_same_argv_candidate_use_current_condition() {
    let (workspace, session, turn) = common_task_with_freeform_apply_patch_tool().await;
    let initial = session
        .current_common_harness_state("harness-root")
        .await
        .expect("host-registered task");
    let reproduce = initial
        .request
        .conditions
        .iter()
        .find(|condition| condition.kind == ConditionKindV1::ReproduceFailure)
        .expect("registered reproduction condition");
    let suite = initial
        .request
        .conditions
        .iter()
        .find(|condition| condition.kind == ConditionKindV1::TestSuite)
        .expect("registered suite condition");
    let reproduce_check = reproduce
        .check
        .as_ref()
        .expect("registered reproduction verifier");
    let suite_check = suite.check.as_ref().expect("registered suite verifier");
    assert_eq!(
        reproduce_check.argv, suite_check.argv,
        "this fixture intentionally uses identical argv for distinct registered conditions"
    );
    let argv = reproduce_check.argv.clone();
    let artifact_paths = serde_json::to_value(&reproduce_check.cwd_binding_refs)
        .expect("serialize registered artifact paths");
    let original_go_mod =
        std::fs::read(workspace.path().join("go.mod")).expect("read original module file");
    let original_test = std::fs::read(workspace.path().join("value_test.go"))
        .expect("read original regression test");
    let go_executable = std::path::PathBuf::from(argv.first().expect("host-pinned Go executable"));
    let compile_output = workspace
        .path()
        .join(".host")
        .join("candidate-transition.test");
    let mut compile_command = Command::new(&go_executable);
    compile_command
        .args(["test", "-c", "-o"])
        .arg(&compile_output)
        .arg(".")
        .current_dir(workspace.path());
    apply_fixture_go_cache(&mut compile_command, &turn);
    let compile = compile_command
        .output()
        .expect("compile the real fixture with its registered Go executable");
    assert!(
        compile.status.success(),
        "the native fixture must compile before exercising runtime routing: {}",
        String::from_utf8_lossy(&compile.stderr)
    );

    let step_context = StepContext::for_test(Arc::clone(&turn));
    let (registry, hosted_specs) = tool_registry_for_test_step(step_context.as_ref());
    let router = Arc::new(ToolRouter::from_registry(
        step_context.turn.as_ref(),
        step_context.turn.model_info(),
        registry,
        hosted_specs,
        &Default::default(),
    ));
    let runtime = ToolCallRuntime::new(
        Arc::clone(&session),
        step_context.with_tool_router_for_test(router),
        Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
    );

    let baseline_call_id = "common-harness-transition-baseline";
    let baseline_tool = run_registered_check(
        &runtime,
        &session,
        &turn,
        &workspace,
        baseline_call_id,
        &argv,
        &artifact_paths,
    )
    .await;
    let baseline_raw = session
        .task_check_receipt(baseline_call_id)
        .await
        .expect("raw baseline TaskCheck receipt");
    assert!(baseline_raw.completed);
    assert_eq!(baseline_raw.exit_code, 1, "{baseline_tool}");
    assert!(!baseline_raw.succeeded);
    let after_baseline = session
        .current_common_harness_state("harness-root")
        .await
        .expect("state after baseline");
    let latest_repro = after_baseline
        .evidence_refs
        .iter()
        .rev()
        .find(|evidence| evidence.condition_id == reproduce.condition_id)
        .expect("baseline reproduction evidence");
    assert_eq!(latest_repro.verdict, ConditionVerdictV1::Fail);
    assert!(latest_repro.host_negative_proof.is_some());

    let patch_stage_trace_path = workspace.path().join(".host/native-patch-stage-trace.log");
    let patch_effect_counter_path = workspace.path().join(".host/native-patch-effect-count.log");
    let patch_observer = PatchTestObserver {
        counter_path: patch_effect_counter_path,
        stage_trace_path: patch_stage_trace_path,
        crash: None,
    };
    turn.extension_data.insert(patch_observer.clone());

    // Use the real apply_patch tool through ToolCallRuntime. Only value.go changes.
    let patch = "*** Begin Patch\n*** Update File: value.go\n@@\n-package harness\n-func Value() int { return 1 }\n+package harness\n+func Value() int { return 2 }\n*** End Patch";
    let patch_result = runtime
        .clone()
        .handle_tool_call(
            ToolCall {
                tool_name: codex_tools::ToolName::plain("apply_patch"),
                call_id: "common-harness-transition-patch".into(),
                payload: ToolPayload::Custom {
                    input: patch.to_string(),
                },
                encrypted_function_args: None,
            },
            CancellationToken::new(),
        )
        .await
        .expect("native apply_patch result");
    patch_observer.record_stage("handler:return");
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("value.go")).unwrap(),
        "package harness\nfunc Value() int { return 2 }\n",
        "the native patch must apply successfully; runtime result: {patch_result:?}"
    );
    assert_eq!(
        std::fs::read(workspace.path().join("go.mod")).unwrap(),
        original_go_mod,
        "the patch must leave the module definition unchanged"
    );
    assert_eq!(
        std::fs::read(workspace.path().join("value_test.go")).unwrap(),
        original_test,
        "the patch must leave the registered test unchanged"
    );

    let candidate_call_id = "common-harness-transition-candidate";
    patch_observer.record_stage("candidate:start");
    let candidate_tool = run_registered_check(
        &runtime,
        &session,
        &turn,
        &workspace,
        candidate_call_id,
        &suite_check.argv,
        &artifact_paths,
    )
    .await;
    patch_observer.record_stage("candidate:return");
    let candidate_raw = session
        .task_check_receipt(candidate_call_id)
        .await
        .expect("raw candidate TaskCheck receipt");
    assert!(candidate_raw.completed);
    assert_eq!(
        candidate_raw.exit_code, 0,
        "candidate suite must pass natively: {candidate_tool:?}"
    );
    assert!(candidate_raw.succeeded);

    let final_state = session
        .current_common_harness_state("harness-root")
        .await
        .expect("state after candidate suite");
    for condition_id in [&reproduce.condition_id, &suite.condition_id] {
        let latest = final_state
            .evidence_refs
            .iter()
            .rev()
            .find(|evidence| &evidence.condition_id == condition_id)
            .unwrap_or_else(|| {
                panic!(
                    "latest evidence for {condition_id}; patch={patch_result:?}; candidate={candidate_tool:?}"
                )
            });
        assert_eq!(
            latest.verdict,
            ConditionVerdictV1::Pass,
            "condition {condition_id}"
        );
        assert_eq!(
            latest.target_version, final_state.run_state.target_version,
            "latest evidence must refer to the current patched target"
        );
    }
    assert!(final_state.run_state.unmet_conditions.is_empty());
    assert_ne!(
        final_state.run_state.status,
        TaskRunStatusV1::Completed,
        "native passing evidence does not replace the existing acceptance checkpoint"
    );
    let patch_operations = final_state
        .operation_refs
        .iter()
        .filter(|operation| operation.intent.operation_kind == OperationKindV1::ApplyPatch)
        .collect::<Vec<_>>();
    assert_eq!(
        patch_operations.len(),
        1,
        "one native patch operation was registered"
    );
    assert_eq!(
        patch_operations[0].receipt.as_ref().unwrap().outcome,
        OperationOutcomeV1::Succeeded
    );
}

#[tokio::test]
async fn same_argv_candidate_after_external_target_refresh_satisfies_both_conditions() {
    let (workspace, session, turn) = common_task().await;
    let initial = session
        .current_common_harness_state("harness-root")
        .await
        .expect("host-registered task");
    let reproduce = initial
        .request
        .conditions
        .iter()
        .find(|condition| condition.kind == ConditionKindV1::ReproduceFailure)
        .expect("registered reproduction condition");
    let suite = initial
        .request
        .conditions
        .iter()
        .find(|condition| condition.kind == ConditionKindV1::TestSuite)
        .expect("registered suite condition");
    let reproduce_check = reproduce.check.as_ref().expect("reproduction Go check");
    let suite_check = suite.check.as_ref().expect("suite Go check");
    assert_eq!(reproduce_check.argv, suite_check.argv);
    let argv = reproduce_check.argv.clone();
    let artifact_paths = serde_json::to_value(&reproduce_check.cwd_binding_refs).unwrap();
    let go_executable = std::path::PathBuf::from(&argv[0]);
    let compile_output = workspace.path().join(".host/external-target.test");
    let mut compile_command = Command::new(&go_executable);
    compile_command
        .args(["test", "-c", "-o"])
        .arg(compile_output)
        .arg(".")
        .current_dir(workspace.path());
    apply_fixture_go_cache(&mut compile_command, &turn);
    let compile = compile_command
        .output()
        .expect("compile the fixture before native execution");
    assert!(
        compile.status.success(),
        "fixture compile failed: {}",
        String::from_utf8_lossy(&compile.stderr)
    );

    let runtime = native_runtime(&session, &turn);
    let baseline_output = run_registered_check(
        &runtime,
        &session,
        &turn,
        &workspace,
        "common-harness-external-target-baseline",
        &argv,
        &artifact_paths,
    )
    .await;
    let baseline = session
        .task_check_receipt("common-harness-external-target-baseline")
        .await
        .expect("raw baseline receipt");
    assert_eq!(baseline.exit_code, 1, "{baseline_output}");
    assert!(!baseline.succeeded);

    std::fs::write(
        workspace.path().join("value.go"),
        "package harness\nfunc Value() int { return 2 }\n",
    )
    .expect("apply the external fix to the registered implementation");
    let decision = session
        .common_harness_turn_end_gate(&turn)
        .await
        .expect("turn-end gate observes the changed target");
    assert!(matches!(
        decision,
        crate::session::common_harness_completion::CommonHarnessTurnEndDecision::Continue { .. }
    ));
    let refreshed = session
        .current_common_harness_state("harness-root")
        .await
        .expect("state after target refresh");
    assert_ne!(
        refreshed.run_state.target_version,
        initial.run_state.target_version
    );
    assert!(refreshed.evidence_refs.iter().any(|evidence| {
        evidence.condition_id == reproduce.condition_id
            && evidence.lifecycle == EvidenceLifecycleV1::Stale
    }));

    let candidate_output = run_registered_check(
        &runtime,
        &session,
        &turn,
        &workspace,
        "common-harness-external-target-candidate",
        &suite_check.argv,
        &artifact_paths,
    )
    .await;
    let candidate = session
        .task_check_receipt("common-harness-external-target-candidate")
        .await
        .expect("raw candidate receipt");
    assert_eq!(candidate.exit_code, 0, "{candidate_output}");
    assert!(candidate.succeeded);

    let final_state = session
        .current_common_harness_state("harness-root")
        .await
        .expect("state after same-argv candidate");
    for condition_id in [&reproduce.condition_id, &suite.condition_id] {
        let latest = final_state
            .evidence_refs
            .iter()
            .rev()
            .find(|evidence| {
                &evidence.condition_id == condition_id
                    && evidence.lifecycle == EvidenceLifecycleV1::Current
                    && evidence.target_version == final_state.run_state.target_version
            })
            .unwrap_or_else(|| panic!("current evidence for {condition_id}: {candidate_output}"));
        assert_eq!(latest.verdict, ConditionVerdictV1::Pass, "{condition_id}");
    }
    assert!(final_state.run_state.unmet_conditions.is_empty());
}

#[tokio::test]
async fn passing_suite_that_omits_the_original_failure_does_not_resolve_reproduction() {
    let (workspace, session, turn) = common_task_with_suite_excluding_baseline_failure().await;
    let initial = session
        .current_common_harness_state("harness-root")
        .await
        .expect("host-registered alternate suite task");
    let reproduce = initial
        .request
        .conditions
        .iter()
        .find(|condition| condition.kind == ConditionKindV1::ReproduceFailure)
        .expect("registered reproduction condition");
    let suite = initial
        .request
        .conditions
        .iter()
        .find(|condition| condition.kind == ConditionKindV1::TestSuite)
        .expect("registered unrelated suite condition");
    let reproduce_check = reproduce.check.as_ref().unwrap();
    let suite_check = suite.check.as_ref().unwrap();
    assert_eq!(
        reproduce_check
            .go_test_selection
            .as_ref()
            .expect("reproduction case selection")
            .selected_cases,
        vec!["TestHarness".to_string()]
    );
    assert_eq!(
        suite_check
            .go_test_selection
            .as_ref()
            .expect("suite case selection")
            .selected_cases,
        vec!["TestOther".to_string()]
    );
    let go_executable = std::path::PathBuf::from(&reproduce_check.argv[0]);
    let mut compile_command = Command::new(&go_executable);
    compile_command
        .args(["test", "-c", "-o"])
        .arg(workspace.path().join(".host/unrelated-suite.test"))
        .arg(".")
        .current_dir(workspace.path());
    apply_fixture_go_cache(&mut compile_command, &turn);
    let compile = compile_command
        .output()
        .expect("compile the registered Go fixture before behavior checks");
    assert!(
        compile.status.success(),
        "fixture compile failed: {}",
        String::from_utf8_lossy(&compile.stderr)
    );
    let artifact_paths = serde_json::to_value(&suite_check.cwd_binding_refs).unwrap();
    let runtime = native_runtime(&session, &turn);
    let baseline_output = run_registered_check(
        &runtime,
        &session,
        &turn,
        &workspace,
        "common-harness-unrelated-baseline",
        &reproduce_check.argv,
        &artifact_paths,
    )
    .await;
    let baseline = session
        .task_check_receipt("common-harness-unrelated-baseline")
        .await
        .expect("raw failing reproduction receipt");
    assert_eq!(baseline.exit_code, 1, "{baseline_output}");
    assert!(!baseline.succeeded);

    assert_eq!(
        std::fs::read_to_string(workspace.path().join("value.go")).unwrap(),
        "package harness\nfunc Value() int { return 1 }\nfunc Other() int { return 1 }\n",
        "the original regression remains present while the unrelated suite passes"
    );

    let unrelated_output = run_registered_check(
        &runtime,
        &session,
        &turn,
        &workspace,
        "common-harness-unrelated-suite",
        &suite_check.argv,
        &artifact_paths,
    )
    .await;
    let unrelated = session
        .task_check_receipt("common-harness-unrelated-suite")
        .await
        .expect("raw passing unrelated suite receipt");
    assert_eq!(unrelated.exit_code, 0, "{unrelated_output}");
    assert!(unrelated.succeeded);

    let state = session
        .current_common_harness_state("harness-root")
        .await
        .unwrap();
    let latest_repro = state
        .evidence_refs
        .iter()
        .rev()
        .find(|evidence| evidence.condition_id == reproduce.condition_id)
        .expect("baseline reproduction evidence remains present");
    assert_eq!(
        latest_repro.verdict,
        ConditionVerdictV1::Fail,
        "a passing TestOther suite cannot satisfy the unchanged TestHarness reproduction condition"
    );
    assert!(
        state
            .run_state
            .unmet_conditions
            .contains(&reproduce.condition_id)
    );
    let latest_suite = state
        .evidence_refs
        .iter()
        .rev()
        .find(|evidence| evidence.condition_id == suite.condition_id)
        .expect("suite evidence for the registered unrelated case");
    assert_eq!(latest_suite.verdict, ConditionVerdictV1::Pass);
}

#[tokio::test]
async fn begin_rejects_test_counts_that_exceed_the_selected_case_count() {
    let (_workspace, session, _turn) = common_task_with_inconsistent_suite_expected_counts().await;
    assert!(
        session.active_task_scope().await.is_none(),
        "invalid expected counts must be rejected before registering an active task"
    );
    assert!(
        session
            .current_common_harness_state("harness-root")
            .await
            .is_err()
    );
}

// Modified by RenCrow Switch Core, 2026-11-27: verify selective reuse through real Go execution.

async fn selective_workspace_task() -> (tempfile::TempDir, Arc<Session>, Arc<TurnContext>) {
    let workspace = tempfile::tempdir().expect("two-package Go workspace");
    let host = workspace.path().join(".host");
    std::fs::create_dir_all(&host).expect("fixture host directory");
    let go_cache = std::env::var_os("GOCACHE")
        .map(std::path::PathBuf::from)
        .filter(|path| path.is_dir())
        .unwrap_or_else(|| host.join("go-cache"));
    std::fs::create_dir_all(&go_cache).expect("fixture Go cache");
    std::fs::write(
        workspace.path().join("go.mod"),
        "module example.test/selective\n\ngo 1.24\n",
    )
    .expect("write Go module");
    for package in ["a", "b"] {
        std::fs::create_dir(workspace.path().join(package)).expect("create package directory");
        std::fs::write(
            workspace.path().join(package).join("value.go"),
            format!("package {package}\nfunc Value() int {{ return 1 }}\n"),
        )
        .expect("write package source");
    }
    std::fs::write(
        workspace.path().join("a/value_test.go"),
        "package a\nimport \"testing\"\nfunc TestA(t *testing.T) { if Value() != 1 { t.Fatalf(\"A value = %d\", Value()) } }\n",
    )
    .expect("write A test");
    std::fs::write(
        workspace.path().join("b/value_test.go"),
        "package b\nimport \"testing\"\nfunc TestB(t *testing.T) { if Value() != 1 { t.Fatalf(\"B value = %d\", Value()) } }\n",
    )
    .expect("write B test");

    let (session, turn, _events) =
        crate::session::tests::make_session_and_context_with_auth_and_config_and_rx(
            codex_login::CodexAuth::from_api_key("Test API Key"),
            Vec::new(),
            |config| {
                config
                    .permissions
                    .set_permission_profile(
                        codex_protocol::models::PermissionProfile::workspace_write(),
                    )
                    .expect("fixture requires workspace-write");
                config
                    .permissions
                    .shell_environment_policy
                    .r#set
                    .insert("GOCACHE".into(), go_cache.to_string_lossy().into_owned());
                config.codex_home = host.clone().try_into().expect("absolute fixture home");
                config.cwd = workspace
                    .path()
                    .to_path_buf()
                    .try_into()
                    .expect("absolute fixture workspace");
            },
        )
        .await;
    let mut session = Arc::try_unwrap(session).ok().expect("unique session");
    let mut turn = Arc::try_unwrap(turn).expect("unique turn");
    turn.session_source = codex_protocol::protocol::SessionSource::Cli;
    Arc::make_mut(&mut turn.config).rencrow_compaction = true;
    Arc::make_mut(&mut turn.config).rencrow_autonomous_completion = true;
    let turn = Arc::new(turn);
    super::attach_boundary_owner(&mut session, Arc::clone(&turn)).await;
    let trace_root = host.join("rollout-traces");
    std::fs::create_dir_all(&trace_root).expect("selective task trace fixture root");
    session.services.rollout_thread_trace =
        codex_rollout_trace::ThreadTraceContext::start_root_in_root_for_test(
            &trace_root,
            codex_rollout_trace::ThreadStartedTraceMetadata {
                thread_id: session.thread_id().to_string(),
                agent_path: "/root".into(),
                task_name: None,
                nickname: None,
                agent_role: None,
                session_source: turn.session_source.clone(),
                cwd: turn.config.cwd.as_path().to_path_buf(),
                rollout_path: None,
                model: turn.model_info().slug.clone(),
                provider_name: turn.config.model_provider_id.clone(),
                approval_policy: turn.approval_policy().to_string(),
                sandbox_policy: format!("{:?}", turn.sandbox_policy()),
            },
        )
        .expect("enabled selective task fixture trace");
    session
        .services
        .rollout_thread_trace
        .record_codex_turn_started(turn.sub_id.as_str());
    let session = Arc::new(session);

    let check = |case_name: &str, package: &str| {
        let package_paths = vec![
            "go.mod".to_string(),
            format!("{package}/value.go"),
            format!("{package}/value_test.go"),
        ];
        serde_json::json!({
            "argv": ["go", "test", "-json", "-count=1", "-run", format!("^{case_name}$"), format!("./{package}")],
            "parser": "go_test_json",
            "expected_outcome": {"test_counts": {"passed": 1, "failed": 0, "skipped": 0}},
            "go_test_selection": {"selected_cases": [case_name], "expected_case_count": 1},
            "cwd_binding_refs": package_paths,
            "allowed_environment_refs": [],
            "toolchain_ref": "go",
            "timeout_ms": 30000
        })
    };
    let a_paths = serde_json::json!(["go.mod", "a/value.go", "a/value_test.go"]);
    let b_paths = serde_json::json!(["go.mod", "b/value.go", "b/value_test.go"]);
    let request = serde_json::json!({
        "contract_version": "common-harness/1.0",
        "task_id": "selective-work",
        "request_revision": 1,
        "goal": "Preserve proof for an unchanged package while rechecking a changed package",
        "constraints": ["Keep package A's proof bound to its declared dependencies"],
        "conditions": [
            {"condition_id":"AC-a", "condition_revision":1, "kind":"test_suite",
             "comparison_scope":"TestA", "expected":"TestA passes", "required":true,
             "verifier_ref":"codex-common-harness/go-test-json/1", "verifier_config_revision":1,
             "check":check("TestA", "a"), "dependency_refs":a_paths},
            {"condition_id":"AC-b", "condition_revision":1, "kind":"test_suite",
             "comparison_scope":"TestB", "expected":"TestB passes", "required":true,
             "verifier_ref":"codex-common-harness/go-test-json/1", "verifier_config_revision":1,
             "check":check("TestB", "b"), "dependency_refs":b_paths}
        ],
        "condition_set_revision":1,
        "max_attempts":3,
        "skill_refs":[],
        "execution_owner":"standalone_switch"
    });
    let args = serde_json::json!({
        "action":"begin", "root_task_id":"selective-root", "task_id":"selective-work",
        "task_revision":1, "common_harness_request":request
    });
    let begin_call = "begin-selective-work";
    record_call_with_arguments(
        &session,
        &turn,
        begin_call,
        crate::tools::handlers::task_context_boundary_spec::TASK_CONTEXT_BOUNDARY_TOOL_NAME,
        &args.to_string(),
    )
    .await;
    super::TaskContextBoundaryHandler::new(true)
        .handle(super::invocation_in_session(
            Arc::clone(&session),
            Arc::clone(&turn),
            begin_call,
            args,
        ))
        .await
        .expect("normal Begin registers the selective task");
    super::record_output(
        &session,
        &turn,
        begin_call,
        crate::tools::handlers::task_context_boundary_spec::TASK_CONTEXT_BOUNDARY_TOOL_NAME,
    )
    .await;
    (workspace, session, turn)
}

fn compile_registered_go_package(
    workspace: &tempfile::TempDir,
    turn: &TurnContext,
    registered_argv: &[String],
    package: &str,
    output_name: &str,
) {
    let go = std::path::PathBuf::from(registered_argv.first().expect("registered Go executable"));
    assert!(go.is_file(), "registered Go executable resolves");
    let mut command = Command::new(go);
    command
        .args(["test", "-c", "-o"])
        .arg(workspace.path().join(".host").join(output_name))
        .arg(format!("./{package}"))
        .current_dir(workspace.path());
    apply_fixture_go_cache(&mut command, turn);
    let output = command.output().expect("launch compile-only Go check");
    assert!(
        output.status.success(),
        "registered Go fixture package {package} must compile before runtime evidence: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn selective_rebind_keeps_unchanged_package_current_after_other_package_changes() {
    let (workspace, session, turn) = selective_workspace_task().await;
    let initial = session
        .current_common_harness_state("selective-root")
        .await
        .expect("host-registered two-package task");
    let condition = |id: &str| {
        initial
            .request
            .conditions
            .iter()
            .find(|condition| condition.condition_id == id)
            .expect("registered package condition")
    };
    let check_a = condition("AC-a").check.as_ref().unwrap();
    let check_b = condition("AC-b").check.as_ref().unwrap();
    assert!(std::path::Path::new(&check_a.argv[0]).is_file());
    assert!(std::path::Path::new(&check_b.argv[0]).is_file());
    assert_eq!(
        &check_a.argv[1..],
        ["test", "-json", "-count=1", "-run", "^TestA$", "./a"]
    );
    assert_eq!(
        &check_b.argv[1..],
        ["test", "-json", "-count=1", "-run", "^TestB$", "./b"]
    );
    assert_eq!(
        initial.run_state.target_version,
        initial.request.start_target_version
    );
    assert_eq!(
        initial.request.conditions[0].dependency_refs,
        ["go.mod", "a/value.go", "a/value_test.go"]
    );
    assert_eq!(
        initial.request.conditions[1].dependency_refs,
        ["go.mod", "b/value.go", "b/value_test.go"]
    );
    let global_files = initial
        .request
        .conditions
        .iter()
        .flat_map(|condition| condition.dependency_refs.iter().cloned())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        global_files,
        [
            "go.mod".to_string(),
            "a/value.go".to_string(),
            "a/value_test.go".to_string(),
            "b/value.go".to_string(),
            "b/value_test.go".to_string(),
        ]
        .into_iter()
        .collect()
    );

    compile_registered_go_package(&workspace, &turn, &check_a.argv, "a", "a.test");
    compile_registered_go_package(&workspace, &turn, &check_b.argv, "b", "b.test");
    let runtime = native_runtime(&session, &turn);
    for (call_id, check) in [
        ("selective-a-initial", check_a),
        ("selective-b-initial", check_b),
    ] {
        let paths = serde_json::to_value(&check.cwd_binding_refs).unwrap();
        let output = run_registered_check(
            &runtime,
            &session,
            &turn,
            &workspace,
            call_id,
            &check.argv,
            &paths,
        )
        .await;
        let receipt = session.task_check_receipt(call_id).await.unwrap();
        assert_eq!(receipt.exit_code, 0, "{call_id}: {output}");
        assert!(
            receipt.completed && receipt.succeeded,
            "{call_id}: {output}"
        );
    }
    let after_both_pass = session
        .current_common_harness_state("selective-root")
        .await
        .expect("state after native A and B checks");
    let original_a = after_both_pass
        .evidence_refs
        .iter()
        .rev()
        .find(|item| item.condition_id == "AC-a" && item.lifecycle == EvidenceLifecycleV1::Current)
        .expect("current A evidence");
    assert_eq!(original_a.verdict, ConditionVerdictV1::Pass);
    let original_a_evidence_id = original_a.evidence_id.clone();
    let original_a_operation_id = original_a.operation_id.clone();
    let original_a_execution_id = original_a.execution_id.clone();
    let original_a_target = original_a.target_version.clone();
    let original_a_operation = after_both_pass
        .operation_refs
        .iter()
        .find(|item| item.intent.operation_id == original_a_operation_id)
        .expect("original native A operation");
    let original_a_receipt = original_a_operation.receipt.clone().expect("A raw receipt");

    std::fs::write(
        workspace.path().join("b/value.go"),
        "package b\nfunc Value() int { return 2 }\n",
    )
    .expect("change only package B implementation");
    let changed_target = session
        .common_harness_turn_end_gate(&turn)
        .await
        .expect("actual root turn-end target refresh");
    assert!(matches!(
        changed_target,
        crate::session::common_harness_completion::CommonHarnessTurnEndDecision::Continue { .. }
    ));
    let after_refresh = session
        .current_common_harness_state("selective-root")
        .await
        .expect("state after host target refresh");
    assert_ne!(after_refresh.run_state.target_version, original_a_target);
    let current_a = after_refresh
        .evidence_refs
        .iter()
        .rev()
        .find(|item| {
            item.condition_id == "AC-a"
                && item.lifecycle == EvidenceLifecycleV1::Current
                && item.target_version == after_refresh.run_state.target_version
        })
        .expect("A proof is selectively rebound to the new global target");
    assert_eq!(current_a.verdict, ConditionVerdictV1::Pass);
    assert_ne!(current_a.evidence_id, original_a_evidence_id);
    assert_eq!(
        current_a.source_evidence_ref.as_deref(),
        Some(original_a_evidence_id.as_str())
    );
    assert!(
        current_a
            .comparison_proof_ref
            .as_ref()
            .is_some_and(|proof| !proof.trim().is_empty())
    );
    assert_eq!(current_a.operation_id, original_a_operation_id);
    assert_eq!(current_a.execution_id, original_a_execution_id);
    assert_eq!(current_a.verifier_ref, original_a.verifier_ref);
    assert_eq!(
        current_a.verifier_config_revision,
        original_a.verifier_config_revision
    );
    assert_eq!(current_a.observed, original_a.observed);
    assert_eq!(current_a.log_refs, original_a.log_refs);
    assert_eq!(current_a.artifact_refs, original_a.artifact_refs);
    let current_a_operation = after_refresh
        .operation_refs
        .iter()
        .find(|item| item.intent.operation_id == original_a_operation_id)
        .expect("same A operation remains in history");
    assert_eq!(
        current_a_operation.receipt.as_ref(),
        Some(&original_a_receipt)
    );
    assert_eq!(
        after_refresh
            .operation_refs
            .iter()
            .filter(|item| item.intent.operation_kind == OperationKindV1::RunVerifier)
            .count(),
        2,
        "turn-end must not rerun A or B"
    );
    assert!(after_refresh.evidence_refs.iter().any(|item| {
        item.condition_id == "AC-b" && item.lifecycle == EvidenceLifecycleV1::Stale
    }));
    assert!(!after_refresh.evidence_refs.iter().any(|item| {
        item.condition_id == "AC-b"
            && item.lifecycle == EvidenceLifecycleV1::Current
            && item.target_version == after_refresh.run_state.target_version
    }));
    assert!(
        session
            .task_check_receipt("selective-a-after-refresh")
            .await
            .is_none(),
        "no A verifier call was issued during target refresh"
    );

    // Run only B after the refresh. It must report its real new failure; A remains current.
    compile_registered_go_package(&workspace, &turn, &check_b.argv, "b", "b-changed.test");
    let b_paths = serde_json::to_value(&check_b.cwd_binding_refs).unwrap();
    let b_output = run_registered_check(
        &runtime,
        &session,
        &turn,
        &workspace,
        "selective-b-after-refresh",
        &check_b.argv,
        &b_paths,
    )
    .await;
    let changed_b_receipt = session
        .task_check_receipt("selective-b-after-refresh")
        .await
        .expect("real changed B receipt");
    assert_eq!(changed_b_receipt.exit_code, 1, "{b_output}");
    assert!(
        changed_b_receipt.completed && !changed_b_receipt.succeeded,
        "{b_output}"
    );
    assert!(
        session
            .task_check_receipt("selective-a-after-refresh")
            .await
            .is_none(),
        "no Go A execution is allowed after selective rebind"
    );
    let final_state = session
        .current_common_harness_state("selective-root")
        .await
        .expect("state after B's real failing verification");
    assert_eq!(
        final_state
            .operation_refs
            .iter()
            .filter(|item| item.intent.operation_kind == OperationKindV1::RunVerifier)
            .count(),
        3,
        "the only verifier operation after refresh is B's new check"
    );
    assert!(final_state.evidence_refs.iter().any(|item| {
        item.condition_id == "AC-b"
            && item.lifecycle == EvidenceLifecycleV1::Current
            && item.target_version == final_state.run_state.target_version
            && item.verdict == ConditionVerdictV1::Fail
    }));
    assert!(final_state.evidence_refs.iter().any(|item| {
        item.condition_id == "AC-a"
            && item.lifecycle == EvidenceLifecycleV1::Current
            && item.target_version == final_state.run_state.target_version
            && item.operation_id == original_a_operation_id
            && item.execution_id == original_a_execution_id
            && item.verdict == ConditionVerdictV1::Pass
    }));
    // A later unrelated change must use the previous immutable comparison proof,
    // while preserving the original verifier execution and receipt across the chain.
    let prior_a = final_state
        .evidence_refs
        .iter()
        .rev()
        .find(|item| item.condition_id == "AC-a" && item.lifecycle == EvidenceLifecycleV1::Current)
        .unwrap();
    std::fs::write(
        workspace.path().join("b/value.go"),
        "package b\nfunc Value() int { return 3 }\n",
    )
    .unwrap();
    session.common_harness_turn_end_gate(&turn).await.unwrap();
    let twice = session
        .current_common_harness_state("selective-root")
        .await
        .unwrap();
    let twice_a = twice
        .evidence_refs
        .iter()
        .rev()
        .find(|item| {
            item.condition_id == "AC-a"
                && item.lifecycle == EvidenceLifecycleV1::Current
                && item.target_version == twice.run_state.target_version
        })
        .expect("unchanged A evidence survives a second target change");
    assert_eq!(
        twice_a.source_evidence_ref.as_deref(),
        Some(prior_a.evidence_id.as_str())
    );
    assert_eq!(twice_a.execution_id, original_a_execution_id);
    assert_eq!(twice_a.operation_id, original_a_operation_id);
    assert_eq!(
        twice
            .operation_refs
            .iter()
            .filter(|operation| { operation.intent.operation_kind == OperationKindV1::RunVerifier })
            .count(),
        3,
        "neither comparison may rerun Go A"
    );
    assert_eq!(
        twice
            .operation_refs
            .iter()
            .find(|operation| { operation.intent.operation_id == original_a_operation_id })
            .unwrap()
            .receipt
            .as_ref(),
        Some(&original_a_receipt)
    );
}
