// Modified by RenCrow Switch Core, 2026-11-26: test native-proof acceptance through registered tools.
use super::common_task_with_freeform_apply_patch_tool;
use super::common_task_with_reproduction;
use super::record_call_with_arguments;
use super::render_completion_status;
use crate::session::common_harness_completion::CommonHarnessTurnEndDecision;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::tests::tool_registry_for_test_step;
use crate::session::turn_context::TurnContext;
use crate::tools::context::ToolPayload;
use crate::tools::parallel::ToolCallRuntime;
use crate::tools::router::ToolCall;
use crate::tools::router::ToolRouter;
use crate::turn_diff_tracker::TurnDiffTracker;
use codex_history::task_context::ConditionVerdictV1;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub(super) async fn verified_suite_task() -> (
    tempfile::TempDir,
    Arc<Session>,
    Arc<TurnContext>,
    ToolCallRuntime,
) {
    let (workspace, session, turn) = common_task_with_reproduction(false).await;
    assert!(
        session
            .common_harness_acceptance_conditions(&turn)
            .await
            .is_err()
    );
    let state = session
        .current_common_harness_state("harness-root")
        .await
        .unwrap();
    let check = state.request.conditions[0].check.as_ref().unwrap();
    let compile = std::process::Command::new(&check.argv[0])
        .args(["test", "-c", "-o"])
        .arg(workspace.path().join(".host/suite.test"))
        .arg(".")
        .current_dir(workspace.path())
        .env(
            "GOCACHE",
            turn.config
                .permissions
                .shell_environment_policy
                .r#set
                .get("GOCACHE")
                .expect("fixture Go compilation uses the bound writable cache"),
        )
        .output()
        .expect("compile the Go test fixture before behavioral evaluation");
    assert!(
        compile.status.success(),
        "{}",
        String::from_utf8_lossy(&compile.stderr)
    );
    let step = StepContext::for_test(Arc::clone(&turn));
    let (registry, specs) = tool_registry_for_test_step(step.as_ref());
    let router = Arc::new(ToolRouter::from_registry(
        step.turn.as_ref(),
        step.turn.model_info(),
        registry,
        specs,
        &Default::default(),
    ));
    let runtime = ToolCallRuntime::new(
        Arc::clone(&session),
        step.with_tool_router_for_test(router),
        Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
    );
    let scope = session.active_task_scope().await.unwrap();
    let arguments = serde_json::json!({
        "cmd":shlex::try_join(check.argv.iter().map(String::as_str)).unwrap(),
        "workdir":workspace.path(), "yield_time_ms":30000, "timeout_ms":30000,
        "task_evidence":{"task_id":scope.task_id, "task_revision":scope.task_revision,
            "artifact_paths":check.cwd_binding_refs, "check_kind":"verification"}
    });
    record_call_with_arguments(
        &session,
        &turn,
        "common-harness-current-suite",
        "exec_command",
        &arguments.to_string(),
    )
    .await;
    let output = runtime
        .clone()
        .handle_tool_call(
            ToolCall {
                tool_name: codex_tools::ToolName::plain("exec_command"),
                call_id: "common-harness-current-suite".into(),
                payload: ToolPayload::Function {
                    arguments: arguments.to_string(),
                },
                encrypted_function_args: None,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    session
        .record_annotated_conversation_items(
            &turn,
            turn.model_info().as_ref(),
            vec![output.clone()],
        )
        .await;
    let verified = session
        .current_common_harness_state("harness-root")
        .await
        .unwrap();
    assert!(
        verified
            .evidence_refs
            .iter()
            .any(|evidence| evidence.condition_id == "AC-suite"
                && evidence.verdict == ConditionVerdictV1::Pass),
        "the registered native suite must pass before testing the acceptance gate: {output:?}"
    );
    (workspace, session, turn, runtime)
}

#[tokio::test]
async fn common_harness_acceptance_requires_actual_current_native_success() {
    let (workspace, session, turn, _runtime) = verified_suite_task().await;
    assert_eq!(
        session
            .common_harness_acceptance_conditions(&turn)
            .await
            .unwrap(),
        Some(vec!["AC-suite".into()])
    );
    assert_ne!(
        session
            .current_common_harness_state("harness-root")
            .await
            .unwrap()
            .run_state
            .status,
        codex_history::task_context::TaskRunStatusV1::Completed,
        "native success still requires the existing accepted checkpoint before root completion"
    );
    std::fs::write(
        workspace.path().join("value.go"),
        "package harness\nfunc Value() int { return 3 }\n",
    )
    .unwrap();
    assert!(
        session
            .common_harness_acceptance_conditions(&turn)
            .await
            .is_err(),
        "a saved PASS cannot accept different current bytes"
    );
}

#[tokio::test]
async fn common_harness_native_success_commits_existing_checkpoint_before_completion() {
    let (_workspace, session, turn, runtime) = verified_suite_task().await;
    let arguments = serde_json::json!({"action":"accept", "result":"The registered suite passed."});
    record_call_with_arguments(
        &session,
        &turn,
        "common-harness-native-accept",
        "task_context_boundary",
        &arguments.to_string(),
    )
    .await;
    let output = runtime
        .handle_tool_call(
            ToolCall {
                tool_name: codex_tools::ToolName::plain("task_context_boundary"),
                call_id: "common-harness-native-accept".into(),
                payload: ToolPayload::Function {
                    arguments: arguments.to_string(),
                },
                encrypted_function_args: None,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    session
        .record_annotated_conversation_items(
            &turn,
            turn.model_info().as_ref(),
            vec![output.clone()],
        )
        .await;
    assert!(
        session.pending_task_boundary().await.is_some(),
        "current native proof must prepare the ordinary acceptance checkpoint: {output:?}"
    );
    assert!(
        session
            .commit_prepared_task_boundary(&turn, &CancellationToken::new())
            .await
            .unwrap()
    );
    assert!(session.active_task_scope().await.is_none());
    assert!(
        session.autonomous_completion_is_complete().await,
        "the existing accepted receipts and completion obligations must also be satisfied"
    );
    assert!(matches!(
        session.common_harness_turn_end_gate(&turn).await.unwrap(),
        CommonHarnessTurnEndDecision::Completed { .. }
    ));
    let completed = session
        .current_common_harness_state("harness-root")
        .await
        .unwrap();
    assert_eq!(
        completed.run_state.status,
        codex_history::task_context::TaskRunStatusV1::Completed
    );
}

#[tokio::test]
async fn completed_common_harness_status_reserves_new_execution_after_target_change() {
    use codex_history::task_context::TaskRunStatusV1;
    use codex_history::task_context::UnitStatus;

    let (workspace, session, turn, runtime) = verified_suite_task().await;
    let accept_args = serde_json::json!({
        "action": "accept",
        "result": "The registered suite passed."
    });
    record_call_with_arguments(
        &session,
        &turn,
        "common-harness-repeat-accept",
        "task_context_boundary",
        &accept_args.to_string(),
    )
    .await;
    let accepted = runtime
        .clone()
        .handle_tool_call(
            ToolCall {
                tool_name: codex_tools::ToolName::plain("task_context_boundary"),
                call_id: "common-harness-repeat-accept".into(),
                payload: ToolPayload::Function {
                    arguments: accept_args.to_string(),
                },
                encrypted_function_args: None,
            },
            CancellationToken::new(),
        )
        .await
        .expect("native result prepares the ordinary acceptance checkpoint");
    session
        .record_annotated_conversation_items(&turn, turn.model_info().as_ref(), vec![accepted])
        .await;
    assert!(
        session
            .commit_prepared_task_boundary(&turn, &CancellationToken::new())
            .await
            .unwrap()
    );
    assert!(matches!(
        session.common_harness_turn_end_gate(&turn).await.unwrap(),
        CommonHarnessTurnEndDecision::Completed { .. }
    ));
    let completed_before = session
        .current_common_harness_state("harness-root")
        .await
        .expect("accepted Common request remains persisted");
    assert_eq!(
        completed_before.run_state.status,
        TaskRunStatusV1::Completed
    );

    std::fs::write(
        workspace.path().join("value.go"),
        "package harness\nfunc Value() int { return 3 }\n",
    )
    .unwrap();
    let status_args = serde_json::json!({
        "action": "status",
        "root_task_id": "harness-root"
    });
    let status = runtime
        .clone()
        .handle_tool_call(
            ToolCall {
                tool_name: codex_tools::ToolName::plain("task_context_boundary"),
                call_id: "common-harness-repeat-status".into(),
                payload: ToolPayload::Function {
                    arguments: status_args.to_string(),
                },
                encrypted_function_args: None,
            },
            CancellationToken::new(),
        )
        .await
        .expect("host status observes the completed task after scope closure");
    let (success, body) = response_success_and_text(&status);
    assert_eq!(success, Some(true), "status response: {body}");

    let state_before_old_begin = session
        .current_common_harness_state("harness-root")
        .await
        .expect("Common request remains available after status");
    let mut old_request = serde_json::to_value(&completed_before.request)
        .expect("serialize the original logical request");
    old_request["contract_version"] =
        serde_json::json!(codex_history::task_context::COMMON_HARNESS_CONTRACT);
    let old_begin_args = serde_json::json!({
        "action": "begin",
        "root_task_id": "harness-root",
        "task_id": completed_before.request.task_id.clone(),
        "task_revision": completed_before.request.request_revision,
        "common_harness_request": old_request,
    });
    let old_begin_call = "common-harness-repeat-old-begin";
    record_call_with_arguments(
        &session,
        &turn,
        old_begin_call,
        "task_context_boundary",
        &old_begin_args.to_string(),
    )
    .await;
    let old_begin = runtime
        .clone()
        .handle_tool_call(
            ToolCall {
                tool_name: codex_tools::ToolName::plain("task_context_boundary"),
                call_id: old_begin_call.into(),
                payload: ToolPayload::Function {
                    arguments: old_begin_args.to_string(),
                },
                encrypted_function_args: None,
            },
            CancellationToken::new(),
        )
        .await
        .expect("an unreserved old Begin is refused as a tool result");
    let (old_begin_success, old_begin_body) = response_success_and_text(&old_begin);
    assert_eq!(
        old_begin_success,
        Some(false),
        "old Begin refusal: {old_begin_body}"
    );
    assert_eq!(
        session
            .current_common_harness_state("harness-root")
            .await
            .expect("old Begin cannot replace the canonical Common request"),
        state_before_old_begin
    );

    let reopened = session
        .current_common_harness_state("harness-root")
        .await
        .expect("status refresh keeps the logical Common request");
    assert_ne!(
        reopened.run_state.target_version, completed_before.run_state.target_version,
        "status must observe the changed current target"
    );
    assert_eq!(reopened.request.task_id, completed_before.request.task_id);
    assert_eq!(
        reopened.request.request_revision,
        completed_before.request.request_revision
    );
    assert_eq!(
        reopened.run_state.attempt_no,
        completed_before.run_state.attempt_no
    );
    assert_eq!(reopened.run_state.status, TaskRunStatusV1::Queued);
    assert!(
        reopened.evidence_refs.iter().any(|evidence| {
            evidence.condition_id == "AC-suite"
                && evidence.lifecycle == codex_history::task_context::EvidenceLifecycleV1::Stale
        }),
        "the previously accepted proof remains in history as stale"
    );

    let completion = session
        .rencrow_completion_state("harness-root")
        .await
        .expect("outer execution graph is retained");
    let execution_units = completion
        .units
        .iter()
        .filter(|unit| unit.kind == codex_history::task_context::CompletionUnitKind::Execution)
        .collect::<Vec<_>>();
    assert!(
        execution_units.len() >= 2,
        "host must reserve a new execution unit while preserving the accepted unit"
    );
    assert!(
        execution_units
            .iter()
            .any(|unit| unit.status == UnitStatus::Done)
    );
    assert!(
        execution_units
            .iter()
            .any(|unit| unit.status == UnitStatus::Ready)
    );
    assert_eq!(
        completion
            .conditions
            .iter()
            .find(|condition| condition.id == "AC-suite")
            .expect("suite obligation remains in the root graph")
            .status,
        codex_history::task_context::AcceptanceStatus::Open
    );
}

#[tokio::test]
async fn changed_accepted_target_uses_real_reserved_begin_and_native_reverification() {
    use codex_history::task_context::ConditionKindV1;
    use codex_history::task_context::TaskRunStatusV1;

    let (workspace, session, turn) = common_task_with_freeform_apply_patch_tool().await;
    let step = StepContext::for_test(Arc::clone(&turn));
    let (registry, specs) = tool_registry_for_test_step(step.as_ref());
    let router = Arc::new(ToolRouter::from_registry(
        step.turn.as_ref(),
        step.turn.model_info(),
        registry,
        specs,
        &Default::default(),
    ));
    let runtime = ToolCallRuntime::new(
        Arc::clone(&session),
        step.with_tool_router_for_test(router),
        Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
    );
    let initial = session
        .current_common_harness_state("harness-root")
        .await
        .expect("normal Begin registers the Common request");
    let reproduction = initial
        .request
        .conditions
        .iter()
        .find(|condition| condition.kind == ConditionKindV1::ReproduceFailure)
        .expect("registered baseline condition");
    let suite = initial
        .request
        .conditions
        .iter()
        .find(|condition| condition.kind == ConditionKindV1::TestSuite)
        .expect("registered candidate condition");
    let reproduction_check = reproduction.check.as_ref().expect("registered Go verifier");
    let suite_check = suite.check.as_ref().expect("registered Go suite");
    let paths = serde_json::to_value(&suite_check.cwd_binding_refs).unwrap();

    let mut compile = std::process::Command::new(&suite_check.argv[0]);
    compile
        .args(["test", "-c", "-o"])
        .arg(workspace.path().join(".host/iteration.test"))
        .arg(".")
        .current_dir(workspace.path());
    compile.env(
        "GOCACHE",
        turn.config
            .permissions
            .shell_environment_policy
            .r#set
            .get("GOCACHE")
            .expect("fixture has a writable Go cache"),
    );
    let compiled = compile.output().expect("compile the registered Go fixture");
    assert!(
        compiled.status.success(),
        "native Go fixture must compile before iteration: {}",
        String::from_utf8_lossy(&compiled.stderr)
    );

    let baseline = super::producer_tests::run_registered_check(
        &runtime,
        &session,
        &turn,
        &workspace,
        "iteration-original-baseline",
        &reproduction_check.argv,
        &paths,
    )
    .await;
    let baseline_receipt = session
        .task_check_receipt("iteration-original-baseline")
        .await
        .expect("baseline raw receipt");
    assert_eq!(baseline_receipt.exit_code, 1, "{baseline}");
    assert!(!baseline_receipt.succeeded);

    let initial_patch = "*** Begin Patch\n*** Update File: value.go\n@@\n-package harness\n-func Value() int { return 1 }\n+package harness\n+func Value() int { return 2 }\n*** End Patch";
    let initial_patch_output = runtime
        .clone()
        .handle_tool_call(
            ToolCall {
                tool_name: codex_tools::ToolName::plain("apply_patch"),
                call_id: "iteration-initial-patch".into(),
                payload: ToolPayload::Custom {
                    input: initial_patch.into(),
                },
                encrypted_function_args: None,
            },
            CancellationToken::new(),
        )
        .await
        .expect("native initial patch");
    let initial_patch_summary = response_success_and_text(&initial_patch_output);
    assert_eq!(
        initial_patch_summary.0,
        Some(true),
        "initial native patch failed: {}",
        initial_patch_summary.1
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("value.go")).unwrap(),
        "package harness\nfunc Value() int { return 2 }\n",
        "initial native patch result: {initial_patch_output:?}"
    );
    let first_candidate = super::producer_tests::run_registered_check(
        &runtime,
        &session,
        &turn,
        &workspace,
        "iteration-original-candidate",
        &suite_check.argv,
        &paths,
    )
    .await;
    let first_receipt = session
        .task_check_receipt("iteration-original-candidate")
        .await
        .expect("initial candidate raw receipt");
    assert_eq!(first_receipt.exit_code, 0, "{first_candidate}");
    assert!(first_receipt.succeeded);

    let first_accept_args = serde_json::json!({
        "action": "accept",
        "result": "The registered Go regression passes."
    });
    record_call_with_arguments(
        &session,
        &turn,
        "iteration-original-accept",
        "task_context_boundary",
        &first_accept_args.to_string(),
    )
    .await;
    let first_accept = runtime
        .clone()
        .handle_tool_call(
            ToolCall {
                tool_name: codex_tools::ToolName::plain("task_context_boundary"),
                call_id: "iteration-original-accept".into(),
                payload: ToolPayload::Function {
                    arguments: first_accept_args.to_string(),
                },
                encrypted_function_args: None,
            },
            CancellationToken::new(),
        )
        .await
        .expect("native evidence accepts the original execution");
    let first_accept_summary = response_success_and_text(&first_accept);
    assert_eq!(
        first_accept_summary.0,
        Some(true),
        "original native Accept failed: {}",
        first_accept_summary.1
    );
    session
        .record_annotated_conversation_items(&turn, turn.model_info().as_ref(), vec![first_accept])
        .await;
    assert!(
        session
            .commit_prepared_task_boundary(&turn, &CancellationToken::new())
            .await
            .unwrap()
    );
    assert!(matches!(
        session.common_harness_turn_end_gate(&turn).await.unwrap(),
        CommonHarnessTurnEndDecision::Completed { .. }
    ));
    assert_eq!(
        session
            .current_common_harness_state("harness-root")
            .await
            .unwrap()
            .run_state
            .status,
        TaskRunStatusV1::Completed
    );

    std::fs::write(
        workspace.path().join("value.go"),
        "package harness\nfunc Value() int { return 3 }\n",
    )
    .unwrap();
    for call_id in ["iteration-status-first", "iteration-status-duplicate"] {
        let status_args = serde_json::json!({
            "action": "status",
            "root_task_id": "harness-root"
        });
        let status = runtime
            .clone()
            .handle_tool_call(
                ToolCall {
                    tool_name: codex_tools::ToolName::plain("task_context_boundary"),
                    call_id: call_id.into(),
                    payload: ToolPayload::Function {
                        arguments: status_args.to_string(),
                    },
                    encrypted_function_args: None,
                },
                CancellationToken::new(),
            )
            .await
            .expect("host status refresh");
        assert_eq!(response_success_and_text(&status).0, Some(true));
    }
    let reserved_completion = session
        .rencrow_completion_state("harness-root")
        .await
        .expect("host-reserved iteration");
    let reserved_id = reserved_completion
        .current_unit
        .clone()
        .expect("exact host-reserved unit ID");
    let common = reserved_completion
        .common_harness
        .as_ref()
        .expect("reserved iteration retains Common state");
    let reserved_begin_matches = session
        .common_harness_reserved_begin_matches(&turn, "harness-root", &reserved_id, 1)
        .await;
    let required_conditions = common
        .request
        .conditions
        .iter()
        .filter(|condition| condition.required)
        .collect::<Vec<_>>();
    assert_eq!(required_conditions.len(), 2);
    for required in required_conditions {
        let outer = reserved_completion
            .conditions
            .iter()
            .find(|condition| condition.id == required.condition_id)
            .expect("required Common condition has an outer acceptance entry");
        assert_eq!(
            outer.status,
            codex_history::task_context::AcceptanceStatus::Open,
            "a changed target reopens the outer obligation"
        );
        assert_eq!(
            outer.evidence.len(),
            1,
            "the accepted predecessor receipt remains historical evidence"
        );
    }
    assert!(
        reserved_begin_matches,
        "historical evidence on Open conditions must not block the exact host reservation"
    );
    assert!(reserved_id.starts_with("harness-work-iteration-"));
    assert_eq!(
        reserved_completion
            .units
            .iter()
            .filter(|unit| unit.status == codex_history::task_context::UnitStatus::Ready)
            .count(),
        1,
        "duplicate status must not reserve another unit"
    );
    assert_eq!(
        reserved_completion
            .common_harness
            .as_ref()
            .unwrap()
            .run_state
            .attempt_no,
        1,
        "status remains attempt-free"
    );

    let begin_args = serde_json::json!({
        "action": "begin",
        "root_task_id": "harness-root",
        "task_id": reserved_id,
        "task_revision": 1
    });
    let begin_call_id = "iteration-real-reserved-begin";
    record_call_with_arguments(
        &session,
        &turn,
        begin_call_id,
        "task_context_boundary",
        &begin_args.to_string(),
    )
    .await;
    let begun = runtime
        .clone()
        .handle_tool_call(
            ToolCall {
                tool_name: codex_tools::ToolName::plain("task_context_boundary"),
                call_id: begin_call_id.into(),
                payload: ToolPayload::Function {
                    arguments: begin_args.to_string(),
                },
                encrypted_function_args: None,
            },
            CancellationToken::new(),
        )
        .await
        .expect("only the exact real Begin activates the reserved unit");
    assert_eq!(
        response_success_and_text(&begun).0,
        Some(true),
        "the exact reserved Begin must execute successfully: {begun:?}"
    );
    session
        .record_annotated_conversation_items(&turn, turn.model_info().as_ref(), vec![begun])
        .await;
    let active_scope = session
        .active_task_scope()
        .await
        .expect("new actual execution scope");
    assert_eq!(active_scope.root_task_id, "harness-root");
    assert_eq!(active_scope.task_id, reserved_id);
    assert_eq!(active_scope.task_revision, 1);
    assert_eq!(active_scope.begin_call_id, begin_call_id);

    let repair_patch = "*** Begin Patch\n*** Update File: value.go\n@@\n-package harness\n-func Value() int { return 3 }\n+package harness\n+func Value() int { return 2 }\n*** End Patch";
    let repair = runtime
        .clone()
        .handle_tool_call(
            ToolCall {
                tool_name: codex_tools::ToolName::plain("apply_patch"),
                call_id: "iteration-repair-patch".into(),
                payload: ToolPayload::Custom {
                    input: repair_patch.into(),
                },
                encrypted_function_args: None,
            },
            CancellationToken::new(),
        )
        .await
        .expect("native patch repairs the changed accepted target");
    let repair_summary = response_success_and_text(&repair);
    assert_eq!(
        repair_summary.0,
        Some(true),
        "native iteration repair failed: {}",
        repair_summary.1
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("value.go")).unwrap(),
        "package harness\nfunc Value() int { return 2 }\n",
        "native repair result: {repair:?}"
    );
    let iteration_candidate = super::producer_tests::run_registered_check(
        &runtime,
        &session,
        &turn,
        &workspace,
        "iteration-native-candidate",
        &suite_check.argv,
        &paths,
    )
    .await;
    let iteration_receipt = session
        .task_check_receipt("iteration-native-candidate")
        .await
        .expect("iteration native receipt");
    assert_eq!(iteration_receipt.task_id, reserved_id);
    assert_eq!(iteration_receipt.task_revision, 1);
    assert_eq!(iteration_receipt.exit_code, 0, "{iteration_candidate}");
    assert!(iteration_receipt.succeeded);
    let final_state = session
        .current_common_harness_state("harness-root")
        .await
        .expect("updated Common state");
    assert!(final_state.evidence_refs.iter().any(|evidence| {
        evidence.condition_id == suite.condition_id
            && evidence.verdict == ConditionVerdictV1::Pass
            && evidence.target_version == final_state.run_state.target_version
    }));
    assert_eq!(
        final_state.request.task_id, initial.request.task_id,
        "iteration execution identity does not replace the logical Common request"
    );
    assert_eq!(
        final_state.request.request_revision,
        initial.request.request_revision
    );

    let accept_args = serde_json::json!({
        "action": "accept",
        "result": "The changed accepted target was reverified natively."
    });
    record_call_with_arguments(
        &session,
        &turn,
        "iteration-native-accept",
        "task_context_boundary",
        &accept_args.to_string(),
    )
    .await;
    let accepted = runtime
        .handle_tool_call(
            ToolCall {
                tool_name: codex_tools::ToolName::plain("task_context_boundary"),
                call_id: "iteration-native-accept".into(),
                payload: ToolPayload::Function {
                    arguments: accept_args.to_string(),
                },
                encrypted_function_args: None,
            },
            CancellationToken::new(),
        )
        .await
        .expect("native current-iteration evidence permits acceptance");
    let accepted_summary = response_success_and_text(&accepted);
    assert_eq!(
        accepted_summary.0,
        Some(true),
        "native second-iteration Accept failed: {}",
        accepted_summary.1
    );
    session
        .record_annotated_conversation_items(&turn, turn.model_info().as_ref(), vec![accepted])
        .await;
    assert!(
        session.pending_task_boundary().await.is_some(),
        "successful native Accept must prepare its boundary; response: {}",
        accepted_summary.1
    );
    assert!(
        session
            .commit_prepared_task_boundary(&turn, &CancellationToken::new())
            .await
            .unwrap()
    );
    assert!(matches!(
        session.common_harness_turn_end_gate(&turn).await.unwrap(),
        CommonHarnessTurnEndDecision::Completed { .. }
    ));
    assert_eq!(
        session
            .current_common_harness_state("harness-root")
            .await
            .unwrap()
            .run_state
            .status,
        TaskRunStatusV1::Completed
    );
}

#[tokio::test]
async fn common_harness_checkpoint_returns_host_result_without_another_model_request() {
    let (_workspace, session, turn, runtime) = verified_suite_task().await;
    let arguments = serde_json::json!({"action":"accept", "result":"The registered suite passed."});
    let call_id = "common-harness-fast-result-accept";
    record_call_with_arguments(
        &session,
        &turn,
        call_id,
        "task_context_boundary",
        &arguments.to_string(),
    )
    .await;
    let output = runtime
        .handle_tool_call(
            ToolCall {
                tool_name: codex_tools::ToolName::plain("task_context_boundary"),
                call_id: call_id.into(),
                payload: ToolPayload::Function {
                    arguments: arguments.to_string(),
                },
                encrypted_function_args: None,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    session
        .record_annotated_conversation_items(&turn, turn.model_info().as_ref(), vec![output])
        .await;
    assert!(
        session
            .commit_prepared_task_boundary(&turn, &CancellationToken::new())
            .await
            .unwrap()
    );
    assert!(session.active_task_scope().await.is_none());
    let before = session
        .current_common_harness_state("harness-root")
        .await
        .unwrap();
    assert!(
        session
            .common_harness_checkpoint_result(&turn, true, true)
            .await
            .unwrap()
            .is_none(),
        "new user input must be received before emitting the prior task result"
    );
    assert_eq!(
        session
            .current_common_harness_state("harness-root")
            .await
            .unwrap(),
        before
    );
    let result = session.common_harness_checkpoint_result(&turn, true, false).await.unwrap()
        .expect("a durable native checkpoint must finish from host proofs without another model request");
    let packet: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(packet["format"], "common-harness/task-result/1");
    assert_eq!(packet["task_id"], before.request.task_id);
    assert_eq!(packet["final_state"], "completed");
    assert_eq!(packet["target_version"], before.run_state.target_version);
    assert_eq!(packet["remaining"], serde_json::json!([]));
    assert_eq!(packet["conditions"][0]["condition_id"], "AC-suite");
    assert_eq!(packet["conditions"][0]["verdict"], "pass");
    assert!(
        !packet["conditions"][0]["log_refs"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        !packet["conditions"][0]["artifact_refs"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

fn response_success_and_text(
    response: &codex_history::ResponseItemEnvelope,
) -> (Option<bool>, String) {
    match &response.item {
        codex_protocol::models::ResponseItem::FunctionCallOutput { output, .. }
        | codex_protocol::models::ResponseItem::CustomToolCallOutput { output, .. } => {
            (output.success, output.body.to_text().unwrap_or_default())
        }
        _ => (
            None,
            format!("unexpected response item: {:?}", response.item),
        ),
    }
}

#[tokio::test]
async fn common_harness_canonical_policy_survives_scope_close() {
    let (workspace, session, turn, runtime) = verified_suite_task().await;
    let accept_args = serde_json::json!({
        "action": "accept",
        "result": "The registered suite passed."
    });
    record_call_with_arguments(
        &session,
        &turn,
        "common-harness-scope-none-accept",
        "task_context_boundary",
        &accept_args.to_string(),
    )
    .await;
    let accepted = runtime
        .clone()
        .handle_tool_call(
            ToolCall {
                tool_name: codex_tools::ToolName::plain("task_context_boundary"),
                call_id: "common-harness-scope-none-accept".into(),
                payload: ToolPayload::Function {
                    arguments: accept_args.to_string(),
                },
                encrypted_function_args: None,
            },
            CancellationToken::new(),
        )
        .await
        .expect("native host evidence prepares the existing accept checkpoint");
    session
        .record_annotated_conversation_items(&turn, turn.model_info().as_ref(), vec![accepted])
        .await;
    assert!(session.pending_task_boundary().await.is_some());
    assert!(
        session
            .commit_prepared_task_boundary(&turn, &CancellationToken::new())
            .await
            .unwrap()
    );
    assert!(session.active_task_scope().await.is_none());
    let canonical = session
        .rencrow_completion_state("harness-root")
        .await
        .expect("canonical completion snapshot remains persisted after scope closure");
    assert!(canonical.common_harness.is_some());

    let status_args = serde_json::json!({"action":"status", "root_task_id":"harness-root"});
    let status = runtime
        .clone()
        .handle_tool_call(
            ToolCall {
                tool_name: codex_tools::ToolName::plain("task_context_boundary"),
                call_id: "common-harness-scope-none-status".into(),
                payload: ToolPayload::Function {
                    arguments: status_args.to_string(),
                },
                encrypted_function_args: None,
            },
            CancellationToken::new(),
        )
        .await;
    let (status_success, status_body) = response_success_and_text(&status.unwrap());
    assert_eq!(status_success, Some(true), "bounded status: {status_body}");

    let effect_args = serde_json::json!({
        "cmd":"true", "workdir":workspace.path(), "yield_time_ms":1000, "timeout_ms":1000
    });
    let effect = runtime
        .clone()
        .handle_tool_call(
            ToolCall {
                tool_name: codex_tools::ToolName::plain("exec_command"),
                call_id: "common-harness-scope-none-effect".into(),
                payload: ToolPayload::Function {
                    arguments: effect_args.to_string(),
                },
                encrypted_function_args: None,
            },
            CancellationToken::new(),
        )
        .await;
    let (effect_success, effect_body) = response_success_and_text(&effect.unwrap());
    assert_eq!(effect_success, Some(false), "effect refusal: {effect_body}");
    assert!(effect_body.contains("common-harness"), "{effect_body}");
}

#[tokio::test]
async fn common_harness_closed_scope_denies_task_context_umbrella_actions() {
    let (_workspace, session, turn, runtime) = verified_suite_task().await;
    let accept_args = serde_json::json!({
        "action": "accept",
        "result": "The registered suite passed."
    });
    record_call_with_arguments(
        &session,
        &turn,
        "common-harness-actions-accept",
        "task_context_boundary",
        &accept_args.to_string(),
    )
    .await;
    let accepted = runtime
        .clone()
        .handle_tool_call(
            ToolCall {
                tool_name: codex_tools::ToolName::plain("task_context_boundary"),
                call_id: "common-harness-actions-accept".into(),
                payload: ToolPayload::Function {
                    arguments: accept_args.to_string(),
                },
                encrypted_function_args: None,
            },
            CancellationToken::new(),
        )
        .await
        .expect("native host evidence prepares the existing accept checkpoint");
    session
        .record_annotated_conversation_items(&turn, turn.model_info().as_ref(), vec![accepted])
        .await;
    assert!(session.pending_task_boundary().await.is_some());
    assert!(
        session
            .commit_prepared_task_boundary(&turn, &CancellationToken::new())
            .await
            .unwrap()
    );
    assert!(session.active_task_scope().await.is_none());
    let mut completion = session
        .rencrow_completion_state("harness-root")
        .await
        .expect("canonical common-harness state after accepted checkpoint");
    assert!(completion.common_harness.is_some());
    let condition_id = completion.conditions[0].id.clone();
    completion
        .units
        .push(codex_history::task_context::CompletionUnit {
            id: "v1-route-review".into(),
            purpose: "Review the accepted harness task".into(),
            status: codex_history::task_context::UnitStatus::Planned,
            kind: codex_history::task_context::CompletionUnitKind::Review,
            condition_refs: vec![condition_id],
            depends_on: vec!["harness-work".into()],
            evidence: vec![],
            task_refs: vec![],
            review_target: Some(codex_history::task_context::CompletionReviewTarget {
                task_id: "harness-work".into(),
                task_revision: 1,
            }),
            review_evidence: None,
            review_launch: None,
            review_decision_reason: None,
            review_state: None,
            review_revalidation_of: None,
        });
    session
        .set_accepted_rencrow_completion_state("harness-root".into(), completion)
        .await
        .expect("preserve accepted host receipts and add a valid planned review target");

    let actions = [
        (
            "common-harness-scope-none-review",
            serde_json::json!({
                "action":"review", "target_task_id":"harness-work", "unit_id":"v1-route-review"
            }),
        ),
        (
            "common-harness-scope-none-plan-review",
            serde_json::json!({
                "action":"plan_review", "root_task_id":"harness-root",
                "previous_plan":"previous", "revised_plan":"revised", "reason":"route guard regression"
            }),
        ),
        (
            "common-harness-scope-none-adopt-review",
            serde_json::json!({
                "action":"adopt_review", "unit_id":"v1-route-review",
                "reason":"route guard regression"
            }),
        ),
    ];
    let state_before_actions = session
        .rencrow_completion_state("harness-root")
        .await
        .expect("canonical host state before unsupported actions");
    let mut action_results = Vec::new();
    for (call_id, arguments) in actions {
        let response = runtime
            .clone()
            .handle_tool_call(
                ToolCall {
                    tool_name: codex_tools::ToolName::plain("task_context_boundary"),
                    call_id: call_id.into(),
                    payload: ToolPayload::Function {
                        arguments: arguments.to_string(),
                    },
                    encrypted_function_args: None,
                },
                CancellationToken::new(),
            )
            .await
            .expect("action refusal is delivered as a normal tool output");
        action_results.push((call_id, response_success_and_text(&response)));
    }
    assert!(
        action_results.iter().all(|(_, (success, body))| {
            *success == Some(false) && body.contains("common-harness")
        }),
        "all unsupported umbrella actions must be refused by the v1 guard: {action_results:#?}"
    );
    assert_eq!(
        session
            .rencrow_completion_state("harness-root")
            .await
            .unwrap(),
        state_before_actions,
        "denied actions cannot mutate the canonical root or launch a review"
    );
}

#[tokio::test]
async fn common_harness_status_returns_current_observations_and_evidence_references() {
    let (_workspace, session, _turn, _runtime) = verified_suite_task().await;
    let state = session
        .rencrow_completion_state("harness-root")
        .await
        .unwrap();
    let common = state.common_harness.as_ref().unwrap();
    let evidence = common
        .evidence_refs
        .iter()
        .rev()
        .find(|evidence| {
            evidence.condition_id == "AC-suite"
                && evidence.target_version == common.run_state.target_version
        })
        .unwrap();
    let rendered: serde_json::Value =
        serde_json::from_str(&render_completion_status(&state, None, None).unwrap()).unwrap();
    let condition = &rendered["request"]["conditions"][0];
    assert_eq!(condition["evidence_id"], evidence.evidence_id);
    assert_eq!(condition["observed"], evidence.observed);
    assert_eq!(
        condition["log_refs"],
        serde_json::to_value(&evidence.log_refs).unwrap()
    );
    assert_eq!(
        condition["artifact_refs"],
        serde_json::to_value(&evidence.artifact_refs).unwrap()
    );
}

#[tokio::test]
async fn common_harness_evidence_artifact_reference_loads_immutable_after_snapshot() {
    let (workspace, session, turn, _runtime) = verified_suite_task().await;
    let state = session
        .current_common_harness_state("harness-root")
        .await
        .expect("host state after real native suite");
    let condition = state
        .request
        .conditions
        .iter()
        .find(|condition| condition.condition_id == "AC-suite")
        .expect("registered suite condition");
    let evidence = state
        .evidence_refs
        .iter()
        .rev()
        .find(|evidence| {
            evidence.condition_id == condition.condition_id
                && evidence.condition_revision == condition.condition_revision
                && evidence.condition_set_revision == state.request.condition_set_revision
                && evidence.request_revision == state.request.request_revision
                && evidence.target_version == state.run_state.target_version
                && evidence.lifecycle == codex_history::task_context::EvidenceLifecycleV1::Current
        })
        .expect("current evidence for the actual Go execution");
    let reference = evidence
        .artifact_refs
        .iter()
        .find(|reference| reference.starts_with("common-harness-artifact/1:sha256:"))
        .expect("evidence references a loadable immutable host artifact set");
    let snapshot: crate::common_harness_io::ArtifactSetV1 =
        crate::common_harness_artifacts::load_operation_artifact(
            turn.config.codex_home.as_path(),
            session.thread_id,
            reference,
        )
        .expect("resolve the evidence artifact from private per-thread storage");
    assert_eq!(snapshot.digest, evidence.target_version);
    assert!(snapshot.binding.scope_revision > 0);
    let paths = condition
        .check
        .as_ref()
        .expect("registered suite check")
        .cwd_binding_refs
        .iter()
        .map(std::path::PathBuf::from)
        .collect::<Vec<_>>();
    let recaptured = crate::common_harness_io::ArtifactSetV1::capture_bound(
        workspace.path(),
        &paths,
        &snapshot.binding,
    )
    .expect("the captured dependency set remains readable");
    assert_eq!(
        recaptured, snapshot,
        "reference resolves to actual bound file facts"
    );
    for path in &paths {
        assert!(snapshot.entries.iter().any(|entry| entry.path == *path));
    }
}

#[tokio::test]
async fn common_harness_status_preserves_bounded_native_verification_diagnostics() {
    let (_workspace, session, _turn) = common_task_with_reproduction(false).await;
    let state = session
        .rencrow_completion_state("harness-root")
        .await
        .unwrap();
    let diagnostics = serde_json::json!({"count":1,"attempts":[{
        "call_id":"recorded-native-failure", "launch_phase":"verification",
        "diagnostic":{"excerpt":"Go work directory was denied", "complete":true,"truncated":false}
    }]});
    let rendered: serde_json::Value =
        serde_json::from_str(&render_completion_status(&state, None, Some(&diagnostics)).unwrap())
            .unwrap();
    assert_eq!(
        rendered["verification_obligations"], diagnostics,
        "the common projection must retain the existing bounded native diagnostics"
    );
}

#[tokio::test]
async fn common_harness_repeated_native_failure_exhausts_three_attempts() {
    let (workspace, session, turn, runtime) = verified_suite_task().await;
    std::fs::write(
        workspace.path().join("value.go"),
        "package harness\nfunc Value() int { return 1 }\n",
    )
    .unwrap();
    session.common_harness_turn_end_gate(&turn).await.unwrap();
    let task = session
        .current_common_harness_state("harness-root")
        .await
        .unwrap();
    let check = task.request.conditions[0].check.as_ref().unwrap();
    let compile = std::process::Command::new(&check.argv[0])
        .args(["test", "-c", "-o"])
        .arg(workspace.path().join(".host/failing-suite.test"))
        .arg(".")
        .current_dir(workspace.path())
        .env(
            "GOCACHE",
            turn.config
                .permissions
                .shell_environment_policy
                .r#set
                .get("GOCACHE")
                .unwrap(),
        )
        .output()
        .unwrap();
    assert!(
        compile.status.success(),
        "{}",
        String::from_utf8_lossy(&compile.stderr)
    );
    let paths = serde_json::to_value(&check.cwd_binding_refs).unwrap();
    for attempt in 1..=task.request.max_attempts {
        let output = super::producer_tests::run_registered_check(
            &runtime,
            &session,
            &turn,
            &workspace,
            &format!("same-native-failure-{attempt}"),
            &check.argv,
            &paths,
        )
        .await;
        let failed = session
            .current_common_harness_state("harness-root")
            .await
            .unwrap();
        assert_eq!(failed.run_state.attempt_no, attempt);
        assert_eq!(
            failed.evidence_refs.last().unwrap().verdict,
            ConditionVerdictV1::Fail,
            "{output}"
        );
        let decision = session.common_harness_turn_end_gate(&turn).await.unwrap();
        let current = session
            .current_common_harness_state("harness-root")
            .await
            .unwrap();
        if attempt < task.request.max_attempts {
            assert!(matches!(
                decision,
                CommonHarnessTurnEndDecision::Continue { .. }
            ));
            assert_eq!(current.run_state.attempt_no, attempt + 1);
            // A progress-only turn must not spend the following attempt on old evidence.
            assert!(matches!(
                session.common_harness_turn_end_gate(&turn).await.unwrap(),
                CommonHarnessTurnEndDecision::Continue { .. }
            ));
            assert_eq!(
                session
                    .current_common_harness_state("harness-root")
                    .await
                    .unwrap()
                    .run_state
                    .attempt_no,
                attempt + 1
            );
        } else {
            assert!(matches!(
                decision,
                CommonHarnessTurnEndDecision::Stopped {
                    status: codex_history::task_context::TaskRunStatusV1::Exhausted,
                    ..
                }
            ));
            assert_eq!(current.run_state.attempt_no, task.request.max_attempts);
            let count = current.operation_refs.len();
            assert!(matches!(
                session.common_harness_turn_end_gate(&turn).await.unwrap(),
                CommonHarnessTurnEndDecision::Stopped {
                    status: codex_history::task_context::TaskRunStatusV1::Exhausted,
                    ..
                }
            ));
            assert_eq!(
                session
                    .current_common_harness_state("harness-root")
                    .await
                    .unwrap()
                    .operation_refs
                    .len(),
                count
            );
        }
    }
}
