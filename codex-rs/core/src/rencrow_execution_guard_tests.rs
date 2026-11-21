// Modified by RenCrow: native progress/receipt regression tests.
use super::*;
use pretty_assertions::assert_eq;
use std::future::Future;
use std::num::NonZeroU16;
use std::path::PathBuf;
use std::task::Context;
use std::task::Poll;

#[tokio::test]
async fn prehandler_fatal_is_persisted_before_the_fatal_still_propagates() {
    use crate::session::step_context::StepContext;
    use crate::session::tests::tool_registry_for_test_step;
    use crate::tools::context::ToolPayload;
    use crate::tools::parallel::ToolCallRuntime;
    use crate::tools::router::ToolCall;
    use crate::tools::router::ToolRouter;

    let (mut session, mut turn_context) = crate::session::tests::make_session_and_context().await;
    let policy = RencrowExecutionGuard {
        max_read_only_calls: NonZeroU16::new(1).expect("nonzero test budget"),
        ..Default::default()
    };
    std::sync::Arc::make_mut(&mut turn_context.config).rencrow_execution_guard =
        Some(policy.clone());
    let turn_context = std::sync::Arc::new(turn_context);
    let rollout_path = crate::session::tests::attach_thread_persistence(&mut session).await;
    let session = std::sync::Arc::new(session);
    let active = crate::state::ActiveTurn::default();
    {
        let mut turn_state = active.turn_state.lock().await;
        let guard = &mut turn_state.rencrow_execution_guard;
        guard.observe_diff(None);
        let baseline_stamp = guard.check_stamp();
        guard.finish_check("fixture-baseline-check", baseline_stamp, 1);
        guard
            .charge_read(&policy, "prior baseline diagnostic")
            .expect("failed baseline check grants one bounded diagnostic read");
        assert_eq!(guard.reads, 1);
        assert_eq!(guard.baseline_failure_reads_stamp, Some(guard.diff_stamp));
        guard.budget_rejections = 3;
        guard.arm_fatal_on_next_budget_refusal();
        assert!(guard.fatal_on_next_budget_refusal);
    }
    *session.active_turn.lock().await = Some(active);

    let step_context = StepContext::for_test(std::sync::Arc::clone(&turn_context));
    let (registry, hosted_specs) = tool_registry_for_test_step(step_context.as_ref());
    let router = std::sync::Arc::new(ToolRouter::from_registry(
        step_context.turn.as_ref(),
        step_context.turn.model_info(),
        registry,
        hosted_specs,
        &Default::default(),
    ));
    let step_context = step_context.with_tool_router_for_test(router);
    let runtime = ToolCallRuntime::new(
        std::sync::Arc::clone(&session),
        step_context,
        std::sync::Arc::new(tokio::sync::Mutex::new(
            crate::turn_diff_tracker::TurnDiffTracker::new(),
        )),
    );
    let call_id = "prehandler-fatal-call";
    let result = runtime
        .handle_tool_call(
            ToolCall {
                tool_name: codex_tools::ToolName::plain("exec_command"),
                call_id: call_id.to_owned(),
                payload: ToolPayload::Function {
                    arguments: serde_json::json!({"cmd":"cat README.md"}).to_string(),
                },
                encrypted_function_args: None,
            },
            tokio_util::sync::CancellationToken::new(),
        )
        .await;
    let fatal = match result {
        Err(error) => error,
        Ok(response) => panic!(
            "the fatal read-budget rejection must remain fatal; runtime returned {response:?}"
        ),
    };
    assert!(fatal.to_string().contains("RENCROW_READ_BUDGET_EXCEEDED"));

    let history = session.clone_history().await;
    let response = history
        .annotated_items()
        .iter()
        .find(|envelope| {
            matches!(
                &envelope.item,
                codex_protocol::models::ResponseItem::FunctionCallOutput {
                    call_id: Some(id), ..
                } if id == call_id
            )
        })
        .expect("the blocked call must have a durable false output despite Fatal");
    let codex_protocol::models::ResponseItem::FunctionCallOutput { output, .. } = &response.item
    else {
        unreachable!("the response was selected by its function-call output ID");
    };
    assert_eq!(output.success, Some(false));
    assert!(
        output
            .text_content()
            .is_some_and(|body| body.contains("RENCROW_READ_BUDGET_EXCEEDED"))
    );
    assert!(
        output
            .body
            .to_text()
            .is_some_and(|body| body.contains("RENCROW_READ_BUDGET_EXCEEDED"))
    );
    assert!(
        response
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.rencrow_blocked_tool_call.as_ref())
            .is_some()
    );
    assert!(matches!(
        &response.item,
        codex_protocol::models::ResponseItem::FunctionCallOutput {
            internal_chat_message_metadata_passthrough: Some(metadata),
            ..
        } if metadata.turn_id.is_some()
    ));

    session
        .flush_rollout()
        .await
        .expect("fatal response is flushed to the canonical rollout");
    let codex_history::InitialHistory::Resumed(persisted) =
        crate::rollout::recorder::RolloutRecorder::get_rollout_history(&rollout_path)
            .await
            .expect("read canonical rollout")
    else {
        panic!("expected persisted rollout");
    };
    let persisted_response = persisted
        .history
        .iter()
        .find_map(|item| match item {
            codex_history::RolloutItem::ResponseItem(envelope)
                if matches!(
                    &envelope.item,
                    codex_protocol::models::ResponseItem::FunctionCallOutput {
                        call_id: Some(id), ..
                    } if id == call_id
                ) =>
            {
                Some(envelope)
            }
            _ => None,
        })
        .expect("canonical rollout retains the blocked false result");
    let codex_protocol::models::ResponseItem::FunctionCallOutput { output, .. } =
        &persisted_response.item
    else {
        unreachable!();
    };
    // `success` is an in-memory hint and is intentionally omitted from the
    // canonical FunctionCallOutput wire representation.
    assert_eq!(output.success, None);
    assert!(
        output
            .text_content()
            .is_some_and(|body| body.contains("RENCROW_READ_BUDGET_EXCEEDED"))
    );
    assert!(
        persisted_response
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.rencrow_blocked_tool_call.as_ref())
            .is_some()
    );
    assert!(matches!(
        &persisted_response.item,
        codex_protocol::models::ResponseItem::FunctionCallOutput {
            internal_chat_message_metadata_passthrough: Some(metadata),
            ..
        } if metadata.turn_id.is_some()
    ));
}

#[tokio::test]
async fn parent_handoff_first_tool_preserves_patch_and_exhausted_read_budget() {
    let directory = tempfile::tempdir().unwrap();
    let root = codex_utils_path_uri::PathUri::from_host_native_path(directory.path()).unwrap();
    let mut tracker =
        crate::turn_diff_tracker::TurnDiffTracker::with_environment_display_roots([(
            String::new(),
            root.clone(),
        )]);
    let delta = codex_apply_patch::apply_patch(
        "*** Begin Patch\n*** Add File: a.txt\n+foo\n*** End Patch",
        &root,
        &mut Vec::new(),
        &mut Vec::new(),
        codex_exec_server::LOCAL_FS.as_ref(),
        None,
    )
    .await
    .unwrap();
    tracker.track_delta("", &delta);
    let mut guard = GuardState::default();
    guard.observe_diff(tracker.get_unified_diff().as_deref());
    guard.finish_check("cargo test", guard.check_stamp(), 1);
    guard.reads = 2;
    guard.budget_rejections = 2;
    guard.repeated.insert("cat a.txt".into(), 3);
    let saved = guard.snapshot_with_diff_tracker(Some(&tracker)).unwrap();
    let mut restored = GuardState::from_parent_handoff(&saved).unwrap();
    let mut tracker = restored.take_handoff_diff_tracker().unwrap().unwrap();
    restored.observe_diff(tracker.get_unified_diff().as_deref());
    assert_eq!(
        restored.snapshot_with_diff_tracker(Some(&tracker)).unwrap(),
        saved
    );
    let policy = RencrowExecutionGuard {
        max_read_only_calls: NonZeroU16::new(2).unwrap(),
        ..Default::default()
    };
    assert!(restored.charge_read(&policy, "new read").is_err());
    assert!(restored.record_budget_rejection());
    let delta = codex_apply_patch::apply_patch(
        "*** Begin Patch\n*** Update File: a.txt\n@@\n foo\n+bar\n*** End Patch",
        &root,
        &mut Vec::new(),
        &mut Vec::new(),
        codex_exec_server::LOCAL_FS.as_ref(),
        None,
    )
    .await
    .unwrap();
    tracker.track_delta("", &delta);
    let diff = tracker.get_unified_diff().unwrap();
    assert!(diff.contains("+foo\n+bar"));
    restored.observe_diff(Some(&diff));
    assert!(!restored.checked);
    assert!(restored.diff_nonempty);
}

#[test]
fn repeated_read_is_blocked_until_real_progress() {
    let policy = RencrowExecutionGuard {
        max_repeated_reads: NonZeroU16::new(2).unwrap(),
        ..Default::default()
    };
    let mut state = GuardState::default();
    assert!(state.charge_read(&policy, "cat a").is_ok());
    assert!(state.charge_read(&policy, "cat a").is_ok());
    assert!(state.charge_read(&policy, "cat a").is_err());
    assert!(state.charge_read(&policy, "ls").is_ok());
    assert!(state.charge_read(&policy, "cat b").is_ok());
    state.observe_diff(Some("real patch"));
    state.finish_check("cargo test", state.diff_stamp, 0);
    assert!(state.charge_read(&policy, "cat a").is_ok());
}

#[test]
fn distinct_reads_can_exceed_the_legacy_total_without_progress_fabrication() {
    let policy = RencrowExecutionGuard {
        max_read_only_calls: NonZeroU16::new(1).unwrap(),
        max_repeated_reads: NonZeroU16::new(1).unwrap(),
        task_kind: Some(RencrowTaskKind::ReadOnly),
        ..Default::default()
    };
    let mut state = GuardState::default();
    for index in 0..32 {
        assert!(
            state
                .charge_read(&policy, &format!("inspect source {index}"))
                .is_ok()
        );
    }
    assert!(state.charge_read(&policy, "inspect source 0").is_err());
}

#[test]
fn only_a_successful_check_of_a_new_diff_resets_read_budget() {
    let policy = RencrowExecutionGuard {
        max_repeated_reads: NonZeroU16::new(1).unwrap(),
        ..Default::default()
    };
    let mut state = GuardState::default();
    state.observe_diff(Some("small first patch"));
    state.budget_rejections = 3;
    state.arm_fatal_on_next_budget_refusal();
    state.finish_check("cargo test", state.diff_stamp, 0);
    assert_eq!(state.budget_rejections, 0);
    assert!(!state.fatal_on_next_budget_refusal);
    assert!(state.charge_read(&policy, "cat a").is_ok());
    assert!(state.charge_read(&policy, "cat a").is_err());

    // Repeating the same passing check cannot keep reopening the read budget.
    state.finish_check("cargo test", state.diff_stamp, 0);
    assert!(state.charge_read(&policy, "cat a").is_err());

    // A changed patch must itself pass a check before it counts as progress.
    state.observe_diff(Some("small second patch"));
    assert!(state.charge_read(&policy, "cat a").is_err());
    state.finish_check("cargo test", state.diff_stamp, 0);
    assert!(state.charge_read(&policy, "cat a").is_ok());
}

#[test]
fn failed_current_check_recovers_diagnostic_reads_only_once_per_stamp_and_twice_per_turn() {
    let policy = RencrowExecutionGuard {
        max_read_only_calls: NonZeroU16::new(1).unwrap(),
        ..Default::default()
    };
    let mut state = GuardState::default();
    state.observe_diff(Some("patch A"));
    state.charge_read(&policy, "rg before failure").unwrap();
    let stamp_a = state.diff_stamp;
    state.finish_check("test failed", stamp_a, 1);
    assert!(state.charge_read(&policy, "inspect failure output").is_ok());
    assert!(
        state
            .charge_read(&policy, "same failure output renamed")
            .is_err()
    );

    // Re-running the failure and changing the command identity cannot replenish reads.
    state.finish_check("renamed check", stamp_a, 1);
    assert!(state.charge_read(&policy, "new command name").is_err());

    state.observe_diff(Some("patch B"));
    let stamp_b = state.diff_stamp;
    state.finish_check("B failed", stamp_b, 1);
    assert!(state.charge_read(&policy, "inspect B").is_ok());

    state.observe_diff(Some("patch A"));
    state.finish_check("A failed again", stamp_a, 1);
    assert!(
        state
            .charge_read(&policy, "A cannot recover twice")
            .is_err()
    );
}

#[test]
fn failed_unmodified_baseline_checks_use_the_normal_read_budget() {
    for task_kind in [RencrowTaskKind::Verify, RencrowTaskKind::ReadOnly] {
        let policy = RencrowExecutionGuard {
            max_read_only_calls: NonZeroU16::new(2).unwrap(),
            task_kind: Some(task_kind),
            ..Default::default()
        };
        let mut state = GuardState::default();
        state.observe_diff(None);
        let baseline_stamp = state.diff_stamp;

        state.finish_check("baseline check A", baseline_stamp, 1);
        state.finish_check("baseline check B", baseline_stamp, 1);

        assert!(!state.diff_nonempty);
        assert_eq!(state.diagnostic_reads_stamp, None);
        assert_eq!(state.baseline_failure_reads_stamp, Some(baseline_stamp));
        assert!(state.failed_checks.contains("baseline check A"));
        assert!(state.failed_checks.contains("baseline check B"));
        assert!(!state.checked);
        assert!(
            state
                .charge_read(&policy, "inspect baseline failure A")
                .is_ok()
        );
        assert!(
            state
                .charge_read(&policy, "inspect baseline failure B")
                .is_ok()
        );
        let error = state
            .charge_read(&policy, "inspect baseline failure C")
            .expect_err("ordinary read limit still applies after baseline failures");
        assert!(error.contains("bounded read budget after failed checks"));
        assert!(!error.contains("failed current diff"));
    }
}

#[test]
fn newly_verified_diff_reopens_bounded_diagnostics_but_repeated_or_failed_checks_do_not() {
    let policy = RencrowExecutionGuard {
        max_read_only_calls: NonZeroU16::new(1).unwrap(),
        ..Default::default()
    };
    let mut state = GuardState::default();

    state.observe_diff(Some("patch A"));
    state.finish_check("test A failed", state.diff_stamp, 1);
    assert!(state.charge_read(&policy, "diagnose A").is_ok());

    state.observe_diff(Some("patch B"));
    state.finish_check("test B failed", state.diff_stamp, 1);
    assert!(state.charge_read(&policy, "diagnose B").is_ok());

    state.observe_diff(Some("patch C"));
    state.finish_check("test C failed", state.diff_stamp, 1);
    assert!(
        state
            .charge_read(&policy, "diagnose C before verified progress")
            .is_err()
    );

    state.observe_diff(Some("verified patch D"));
    state.finish_check("test D passed", state.diff_stamp, 0);
    // Repeating a successful check on the same revision is not new progress.
    state.finish_check("test D passed again", state.diff_stamp, 0);

    state.observe_diff(Some("patch E"));
    state.finish_check("test E failed", state.diff_stamp, 1);
    assert!(
        state
            .charge_read(&policy, "diagnose E after verified progress")
            .is_ok()
    );

    // Re-running the failed check on the same revision cannot replenish diagnostics.
    state.finish_check("test E failed again", state.diff_stamp, 1);
    assert!(state.charge_read(&policy, "diagnose E again").is_err());
}

#[test]
fn failed_check_opens_bounded_diagnostics_after_unlimited_normal_reads() {
    let policy = RencrowExecutionGuard {
        max_read_only_calls: NonZeroU16::new(1).unwrap(),
        ..Default::default()
    };
    let mut state = GuardState::default();
    state.observe_diff(Some("real patch"));
    for index in 0..32 {
        state
            .charge_read(&policy, &format!("inspect source {index}"))
            .unwrap();
    }
    let stamp = state.diff_stamp;
    state.finish_check("failed test", stamp, 1);
    assert!(state.charge_read(&policy, "inspect failure output").is_ok());
    assert!(
        state
            .charge_read(&policy, "inspect another diagnostic")
            .is_err()
    );
}

#[test]
fn repeated_budget_refusals_have_a_finite_host_stop_threshold() {
    let mut state = GuardState::default();
    assert!(!state.record_budget_rejection());
    assert!(!state.record_budget_rejection());
    assert!(state.record_budget_rejection());
}

#[tokio::test]
async fn task_context_status_uses_the_read_identity_and_mutations_do_not() {
    let mut invocation = exec_guard_invocation(
        "",
        RencrowExecutionGuard {
            max_repeated_reads: NonZeroU16::new(1).unwrap(),
            ..Default::default()
        },
        None,
    )
    .await;
    invocation.tool_name = codex_tools::ToolName::plain("task_context_boundary");
    invocation.payload = ToolPayload::Function {
        arguments: serde_json::json!({"action": "status", "root_task_id": "root"}).to_string(),
    };
    before_tool(&invocation).await.unwrap();

    let turn_state = {
        let active = invocation.session.active_turn.lock().await;
        std::sync::Arc::clone(&active.as_ref().expect("fixture has active turn").turn_state)
    };
    assert_eq!(
        turn_state
            .lock()
            .await
            .rencrow_execution_guard
            .repeated
            .len(),
        1
    );

    let mut mutation = invocation.clone();
    mutation.payload = ToolPayload::Function {
        arguments: serde_json::json!({"action": "update", "root_task_id": "root"}).to_string(),
    };
    before_tool(&mutation).await.unwrap();
    assert_eq!(
        turn_state
            .lock()
            .await
            .rencrow_execution_guard
            .repeated
            .len(),
        1
    );

    let Err(FunctionCallError::RespondToModel(message)) = before_tool(&invocation).await else {
        panic!("a repeated status read must be refused");
    };
    assert!(message.contains("same read command repeated"));
}

#[tokio::test]
async fn same_sampling_batch_refusals_wait_for_model_feedback_before_fatal() {
    let mut invocation = exec_guard_invocation(
        "",
        RencrowExecutionGuard {
            max_repeated_reads: NonZeroU16::new(1).unwrap(),
            ..Default::default()
        },
        None,
    )
    .await;
    invocation.tool_name = codex_tools::ToolName::plain("task_context_boundary");
    invocation.payload = ToolPayload::Function {
        arguments: serde_json::json!({"action": "status", "root_task_id": "root"}).to_string(),
    };
    before_tool(&invocation).await.unwrap();

    let turn_state = {
        let active = invocation.session.active_turn.lock().await;
        std::sync::Arc::clone(&active.as_ref().expect("fixture has active turn").turn_state)
    };
    let refusals = tokio::join!(
        before_tool(&invocation),
        before_tool(&invocation),
        before_tool(&invocation),
    );
    assert!(matches!(
        refusals.0,
        Err(FunctionCallError::RespondToModel(_))
    ));
    assert!(matches!(
        refusals.1,
        Err(FunctionCallError::RespondToModel(_))
    ));
    assert!(matches!(
        refusals.2,
        Err(FunctionCallError::RespondToModel(_))
    ));
    assert_eq!(
        turn_state
            .lock()
            .await
            .rencrow_execution_guard
            .budget_rejections,
        3
    );

    turn_state
        .lock()
        .await
        .rencrow_execution_guard
        .arm_fatal_on_next_budget_refusal();
    assert!(matches!(
        before_tool(&invocation).await,
        Err(FunctionCallError::Fatal(_))
    ));
}

#[test]
fn handoff_preserves_refusal_count_and_next_sampling_rearms_fatal() {
    let mut state = GuardState {
        budget_rejections: 3,
        ..Default::default()
    };
    state.arm_fatal_on_next_budget_refusal();
    let snapshot = state.parent_handoff_snapshot().unwrap();
    assert!(snapshot.get("fatal_on_next_budget_refusal").is_none());

    let mut restored = GuardState::from_parent_handoff(&snapshot).unwrap();
    assert_eq!(restored.budget_rejections, 3);
    assert!(!restored.fatal_on_next_budget_refusal);
    restored.arm_fatal_on_next_budget_refusal();
    assert!(restored.fatal_on_next_budget_refusal);
}

async fn exec_guard_invocation(
    command: &str,
    policy: RencrowExecutionGuard,
    task_evidence: Option<serde_json::Value>,
) -> ToolInvocation {
    let (session, mut turn) = crate::session::tests::make_session_and_context().await;
    std::sync::Arc::make_mut(&mut turn.config).rencrow_execution_guard = Some(policy);
    std::sync::Arc::make_mut(&mut turn.config).rencrow_autonomous_completion = true;
    let turn = std::sync::Arc::new(turn);
    let session = std::sync::Arc::new(session);
    *session.active_turn.lock().await = Some(crate::state::ActiveTurn::default());

    let mut arguments = serde_json::json!({"cmd": command});
    if let Some(task_evidence) = task_evidence {
        arguments["task_evidence"] = task_evidence;
    }
    ToolInvocation {
        session,
        step_context: crate::session::step_context::StepContext::for_test(std::sync::Arc::clone(
            &turn,
        )),
        turn,
        cancellation_token: tokio_util::sync::CancellationToken::new(),
        tracker: std::sync::Arc::new(tokio::sync::Mutex::new(
            crate::turn_diff_tracker::TurnDiffTracker::new(),
        )),
        call_id: "guard-check".into(),
        tool_name: codex_tools::ToolName::plain("exec_command"),
        source: crate::tools::context::ToolCallSource::Direct,
        payload: ToolPayload::Function {
            arguments: arguments.to_string(),
        },
    }
}

fn task_evidence_request() -> serde_json::Value {
    serde_json::json!({
        "task_id": "unit-1",
        "task_revision": 1,
        "artifact_paths": ["Cargo.toml"],
        "check_kind": "verification"
    })
}

fn scope_for_guard_test() -> crate::state::ActiveTaskScope {
    crate::state::ActiveTaskScope {
        root_task_id: "root".into(),
        task_id: "unit-1".into(),
        task_revision: 1,
        begin_call_id: "begin-1".into(),
        start_history_cursor: 0,
    }
}

#[tokio::test]
async fn rejected_read_check_and_shell_edit_do_not_wait_for_or_register_scope_checks() {
    let command = "python3 -m unittest -c 'os.walk(\".\")'";
    let policy = RencrowExecutionGuard {
        max_repeated_reads: NonZeroU16::new(/*n*/ 1).unwrap(),
        require_edit: true,
        ..Default::default()
    };
    let invocation = exec_guard_invocation(command, policy, Some(task_evidence_request())).await;
    assert!(read_only(command), "fixture must enter the read path");
    assert!(check_command(command), "fixture must enter the check path");
    let turn_state = {
        let active = invocation.session.active_turn.lock().await;
        std::sync::Arc::clone(&active.as_ref().expect("fixture has active turn").turn_state)
    };
    let mut context = Context::from_waker(futures::task::noop_waker_ref());
    let ((result, no_started_check), scope_needs_rebase) =
        crate::session::tests::with_locked_task_scope_for_execution_guard_test(
            &invocation.session,
            scope_for_guard_test(),
            /*needs_rebase*/ true,
            || {
                let repeated_key = format!("{:?}:null:{}", invocation.turn.config.cwd, command);
                {
                    let mut turn = turn_state
                        .try_lock()
                        .expect("turn state is free before the guarded call");
                    turn.rencrow_execution_guard
                        .repeated
                        .insert(repeated_key, 1);
                }

                let mut rejected_check = Box::pin(before_tool(&invocation));
                let result = Future::poll(rejected_check.as_mut(), &mut context);
                let no_started_check = turn_state
                    .try_lock()
                    .expect("synchronous rejection released turn state")
                    .rencrow_execution_guard
                    .started_checks
                    .is_empty();
                (result, no_started_check)
            },
        )
        .await;
    let Poll::Ready(Err(FunctionCallError::RespondToModel(message))) = result else {
        panic!("the exhausted read budget must reject synchronously before scope preparation");
    };
    assert!(message.contains("same read command repeated"));
    assert!(scope_needs_rebase);
    assert!(no_started_check);

    let edit_command = "python3 -m unittest -c 'open(\"x\",\"w\")'";
    assert!(
        check_command(edit_command),
        "fixture must enter the check path"
    );
    assert!(
        crate::rencrow_shell_edit_guard::is_obvious_shell_edit(edit_command),
        "fixture must enter the shell-edit rejection path"
    );
    let invocation = exec_guard_invocation(
        edit_command,
        RencrowExecutionGuard {
            task_kind: Some(RencrowTaskKind::Implement),
            ..Default::default()
        },
        Some(task_evidence_request()),
    )
    .await;
    let turn_state = {
        let active = invocation.session.active_turn.lock().await;
        std::sync::Arc::clone(&active.as_ref().expect("fixture has active turn").turn_state)
    };
    let ((result, no_started_check), scope_needs_rebase) =
        crate::session::tests::with_locked_task_scope_for_execution_guard_test(
            &invocation.session,
            scope_for_guard_test(),
            /*needs_rebase*/ true,
            || {
                let mut rejected_edit = Box::pin(before_tool(&invocation));
                let result = Future::poll(rejected_edit.as_mut(), &mut context);
                let no_started_check = turn_state
                    .try_lock()
                    .expect("synchronous rejection released turn state")
                    .rencrow_execution_guard
                    .started_checks
                    .is_empty();
                (result, no_started_check)
            },
        )
        .await;
    let Poll::Ready(Err(FunctionCallError::RespondToModel(message))) = result else {
        panic!("shell-edit rejection must happen before scope preparation");
    };
    assert_eq!(
        message,
        crate::rencrow_shell_edit_guard::EDIT_METHOD_REQUIRED
    );
    assert!(scope_needs_rebase);
    assert!(no_started_check);
}

#[tokio::test]
async fn check_scope_wait_records_commit_time_stamp_and_charges_read_once() {
    let command = "python3 -m unittest -c 'os.walk(\".\")'";
    let invocation = exec_guard_invocation(
        command,
        RencrowExecutionGuard {
            max_repeated_reads: NonZeroU16::new(/*n*/ 2).unwrap(),
            ..Default::default()
        },
        Some(task_evidence_request()),
    )
    .await;
    let turn_state = {
        let active = invocation.session.active_turn.lock().await;
        std::sync::Arc::clone(&active.as_ref().expect("fixture has active turn").turn_state)
    };
    let mut checking = Box::pin(before_tool(&invocation));
    let mut context = Context::from_waker(futures::task::noop_waker_ref());
    let ((commit_stamp, charged_key), scope_needs_rebase) =
        crate::session::tests::with_locked_task_scope_for_execution_guard_test(
            &invocation.session,
            scope_for_guard_test(),
            /*needs_rebase*/ false,
            || {
                assert!(Future::poll(checking.as_mut(), &mut context).is_pending());
                let mut turn = turn_state
                    .try_lock()
                    .expect("scope wait released turn state");
                turn.rencrow_execution_guard
                    .observe_diff(Some("diff changed during scope preparation"));
                (
                    turn.rencrow_execution_guard.check_stamp(),
                    format!("{:?}:null:{}", invocation.turn.config.cwd, command),
                )
            },
        )
        .await;
    assert!(!scope_needs_rebase);

    checking
        .await
        .expect("check passes guard after scope resolves");
    let turn = turn_state.lock().await;
    assert_eq!(
        turn.rencrow_execution_guard
            .started_checks
            .get(&invocation.call_id)
            .map(|(_, stamp)| *stamp),
        Some(commit_stamp),
        "check receipt uses the stamp current when registration commits"
    );
    assert_eq!(
        turn.rencrow_execution_guard.repeated.get(&charged_key),
        Some(&1),
        "scope preparation does not charge the read budget twice"
    );
}

#[test]
fn patch_verification_failures_reset_only_after_a_changed_diff() {
    let mut state = GuardState::default();
    assert!(!state.record_patch_verification_failure(None));
    assert!(!state.record_patch_verification_failure(None));
    state.observe_diff(None);
    state.finish_check("cargo test", state.diff_stamp, 0);
    assert!(state.record_patch_verification_failure(None));

    let mut state = GuardState::default();
    assert!(!state.record_patch_verification_failure(Some("first diff")));
    assert!(!state.record_patch_verification_failure(Some("first diff")));
    state.observe_diff(Some("accepted changed diff"));
    assert!(!state.record_patch_verification_failure(Some("accepted changed diff")));
}

#[tokio::test]
async fn internal_patch_failures_do_not_receive_recovery_hints() {
    let (session, mut turn) = crate::session::tests::make_session_and_context().await;
    std::sync::Arc::make_mut(&mut turn.config).rencrow_execution_guard =
        Some(RencrowExecutionGuard::default());
    turn.session_source = codex_protocol::protocol::SessionSource::Internal(
        codex_protocol::protocol::InternalSessionSource::Guardian,
    );
    *session.active_turn.lock().await = Some(crate::state::ActiveTurn::default());
    let turn = std::sync::Arc::new(turn);
    let invocation = ToolInvocation {
        session: session.into(),
        step_context: crate::session::step_context::StepContext::for_test(std::sync::Arc::clone(
            &turn,
        )),
        turn,
        cancellation_token: tokio_util::sync::CancellationToken::new(),
        tracker: std::sync::Arc::new(tokio::sync::Mutex::new(
            crate::turn_diff_tracker::TurnDiffTracker::new(),
        )),
        call_id: "internal-patch".into(),
        tool_name: codex_tools::ToolName::plain("apply_patch"),
        source: crate::tools::context::ToolCallSource::Direct,
        payload: ToolPayload::Custom {
            input: "invalid patch".into(),
        },
    };
    let error = FunctionCallError::RespondToModel(format!(
        "{PATCH_RETRYABLE_ERROR_MARKER}{PATCH_CONTEXT_FAILURE_KIND}apply_patch verification failed: missing context"
    ));

    let FunctionCallError::RespondToModel(message) =
        patch_verification_failure(&invocation, error).await
    else {
        panic!("expected a model-visible patch error");
    };
    assert_eq!(message, "apply_patch verification failed: missing context");
    assert!(!message.contains("RENCROW_PATCH_CONTEXT_HINT"));

    let (exec_session, mut exec_turn) = crate::session::tests::make_session_and_context().await;
    std::sync::Arc::make_mut(&mut exec_turn.config).rencrow_execution_guard =
        Some(RencrowExecutionGuard::default());
    *exec_session.active_turn.lock().await = Some(crate::state::ActiveTurn::default());
    let exec_turn = std::sync::Arc::new(exec_turn);
    let exec_invocation = ToolInvocation {
        session: exec_session.into(),
        step_context: crate::session::step_context::StepContext::for_test(std::sync::Arc::clone(
            &exec_turn,
        )),
        turn: exec_turn,
        cancellation_token: tokio_util::sync::CancellationToken::new(),
        tracker: std::sync::Arc::new(tokio::sync::Mutex::new(
            crate::turn_diff_tracker::TurnDiffTracker::new(),
        )),
        call_id: "external-patch".into(),
        tool_name: codex_tools::ToolName::plain("apply_patch"),
        source: crate::tools::context::ToolCallSource::Direct,
        payload: ToolPayload::Custom {
            input: "invalid patch".into(),
        },
    };
    let error = FunctionCallError::RespondToModel(format!(
        "{PATCH_RETRYABLE_ERROR_MARKER}{PATCH_CONTEXT_FAILURE_KIND}apply_patch verification failed: missing context"
    ));

    let FunctionCallError::RespondToModel(message) =
        patch_verification_failure(&exec_invocation, error).await
    else {
        panic!("expected a model-visible patch error");
    };
    assert!(message.contains("RENCROW_PATCH_CONTEXT_HINT"));
    assert!(message.contains("Markdown line prefixes such as `- `"));
}

#[test]
fn patch_format_hint_example_parses_add_and_update_in_one_envelope() {
    let hint = patch_failure_hint(PATCH_SYNTAX_FAILURE_KIND);
    let (_, example) = hint.split_once("Example with two files:\n").unwrap();
    let parsed = codex_apply_patch::parse_patch(example).unwrap();
    assert_eq!(parsed.hunks.len(), 2);
    assert!(matches!(
        parsed.hunks[0],
        codex_apply_patch::Hunk::AddFile { .. }
    ));
    assert!(matches!(
        parsed.hunks[1],
        codex_apply_patch::Hunk::UpdateFile { .. }
    ));
}

#[test]
fn completion_requires_host_diff_and_checks_after_latest_edit() {
    let policy = RencrowExecutionGuard {
        require_edit: true,
        require_check: true,
        ..Default::default()
    };
    let mut state = GuardState::default();
    state.observe_diff(None);
    assert_eq!(state.missing(&policy, false).len(), 2);
    state.observe_diff(Some("first edit"));
    state.failed_checks.insert("go test".into());
    state.finish_check("go test", state.diff_stamp, 1);
    assert_eq!(state.missing(&policy, true).len(), 1);
    state.finish_check("unrelated check", state.diff_stamp, 0);
    assert_eq!(state.missing(&policy, true).len(), 1);
    state.finish_check("go test", state.diff_stamp, 0);
    assert!(state.missing(&policy, true).is_empty());
    let before_edit = state.diff_stamp;
    state.observe_diff(Some("second edit"));
    state.finish_check("go test", before_edit, 0);
    assert_eq!(state.missing(&policy, true).len(), 1);
    state.finish_check("go test", state.diff_stamp, 0);
    assert!(state.missing(&policy, true).is_empty());
}

#[test]
fn unrelated_success_keeps_failed_check_named_in_bounded_diagnostic() {
    let policy = RencrowExecutionGuard {
        require_edit: true,
        require_check: true,
        ..Default::default()
    };
    let mut state = GuardState::default();
    state.observe_diff(Some("patch"));
    state.finish_check("go build ./...", state.check_stamp(), 1);
    state.finish_check("go build ./internal/portal", state.check_stamp(), 0);

    assert_eq!(
        state.missing(&policy, true),
        vec!["successful check after the last edit (failed checks must be rerun)"]
    );
    assert!(
        state
            .failed_check_diagnostic()
            .contains("unresolved failed checks: go build ./...")
    );
    assert!(
        !state
            .failed_check_diagnostic()
            .contains("go build ./internal/portal")
    );
}

#[test]
fn failed_check_diagnostic_redacts_and_bounds_commands() {
    for secret_command in [
        "TOKEN='alpha beta' go test",
        "go test --ToKeN 'alpha beta'",
        "curl -H 'Authorization: Bearer alpha beta' https://example.test",
        "curl https://user:password@example.test/path",
    ] {
        let display = safe_check_command_display(secret_command);
        assert!(display.starts_with("[redacted command "));
        assert!(!display.contains("alpha"));
        assert!(!display.contains("password"));
        assert!(!display.contains("user"));
    }
    assert_eq!(
        safe_check_command_display("cd RenCrow_PORTAL && go build ./..."),
        "cd RenCrow_PORTAL && go build ./..."
    );
    let long_command = format!("go test {}", "x".repeat(300));
    assert!(
        safe_check_command_display(&long_command).chars().count()
            <= MAX_FAILED_CHECK_COMMAND_CHARS + 1
    );
}

#[test]
fn collecting_child_results_is_a_separate_completion_requirement() {
    let policy = RencrowExecutionGuard {
        require_child_results: true,
        ..Default::default()
    };
    let mut state = GuardState::default();
    state.children.insert("child".into());
    assert_eq!(
        state.missing(&policy, false),
        vec!["collected or explicitly closed delegated children"]
    );
    state.children.remove("child");
    assert!(state.missing(&policy, false).is_empty());
}

#[test]
fn typed_implement_and_verify_keep_an_explicit_child_collection_requirement() {
    use codex_config::RencrowTaskKind;
    for task_kind in [RencrowTaskKind::Implement, RencrowTaskKind::Verify] {
        let policy = RencrowExecutionGuard {
            task_kind: Some(task_kind),
            require_child_results: true,
            ..Default::default()
        };
        let mut state = GuardState::default();
        state.children.insert("pending child".into());
        state.observe_diff(Some("current patch"));
        let stamp = state.check_stamp();
        state.finish_check("passing check", stamp, 0);
        assert_eq!(
            state.missing(&policy, true),
            vec!["collected or explicitly closed delegated children"]
        );
    }
}

#[test]
fn orchestrator_requires_successful_child_evidence_and_a_parent_check() {
    use codex_config::RencrowTaskKind;
    use serde_json::json;
    let policy = RencrowExecutionGuard {
        task_kind: Some(RencrowTaskKind::Orchestrate),
        ..Default::default()
    };
    let mut state = GuardState::default();
    assert!(state.missing(&policy, false).is_empty());

    // An implementation child creates an obligation that survives failed/closed
    // worker status and cannot be erased by collecting the terminal result.
    state.implementation_child_delegated = true;
    state.children.insert("worker".into());
    state.observe_child_result("close_agent", &json!({"target":"worker"}), &json!({}));
    assert_eq!(state.missing(&policy, false).len(), 2);
    state
        .child_results
        .insert((codex_protocol::ThreadId::new(), "child-turn".into()));
    assert_eq!(state.missing(&policy, false).len(), 1);
    state.finish_check("parent check", state.diff_stamp, 0);
    assert!(state.missing(&policy, false).is_empty());
    state.checked = true;
    state
        .child_results
        .insert((codex_protocol::ThreadId::new(), "new-child-turn".into()));
    state.checked = false;
    assert_eq!(state.missing(&policy, false).len(), 1);

    let mut self_implementing = GuardState::default();
    self_implementing.observe_diff(Some("parent implementation"));
    assert_eq!(self_implementing.missing(&policy, true).len(), 1);
    self_implementing.finish_check("parent check", self_implementing.diff_stamp, 0);
    assert!(self_implementing.missing(&policy, true).is_empty());
}

#[test]
fn orchestrator_read_only_children_do_not_create_implementation_obligations() {
    use codex_config::RencrowTaskKind;
    use serde_json::json;
    let policy = RencrowExecutionGuard {
        task_kind: Some(RencrowTaskKind::Orchestrate),
        ..Default::default()
    };
    let mut state = GuardState::default();
    state.observe_child_result("spawn_agent", &json!({}), &json!({"agent_id":"reader"}));
    state.observe_child_result(
        "wait_agent",
        &json!({}),
        &json!({"status":{"reader":AgentStatus::Completed(Some("done".into()))}}),
    );
    assert!(state.missing(&policy, false).is_empty());
}

#[test]
fn unknown_child_contract_fails_closed_as_implementation_delegation() {
    assert!(!implementation_child_requires_edit(Some(false)));
    assert!(implementation_child_requires_edit(Some(true)));
    assert!(implementation_child_requires_edit(None));
}

#[test]
fn child_evidence_requires_a_diff_and_passing_check_for_implementation_turns() {
    use codex_config::RencrowTaskKind;
    let orchestrate = RencrowExecutionGuard {
        task_kind: Some(RencrowTaskKind::Orchestrate),
        ..Default::default()
    };
    let implement = RencrowExecutionGuard {
        task_kind: Some(RencrowTaskKind::Implement),
        ..Default::default()
    };
    let verify = RencrowExecutionGuard {
        task_kind: Some(RencrowTaskKind::Verify),
        ..Default::default()
    };
    let mut state = GuardState {
        checked: true,
        ..Default::default()
    };
    assert!(child_execution_evidence_eligible(
        &orchestrate,
        &state,
        true
    ));
    assert!(child_execution_evidence_eligible(&implement, &state, true));
    assert!(!child_execution_evidence_eligible(
        &orchestrate,
        &state,
        false
    ));
    assert!(!child_execution_evidence_eligible(&verify, &state, true));

    state.checked = false;
    assert!(!child_execution_evidence_eligible(
        &orchestrate,
        &state,
        true
    ));
}

#[test]
fn orchestrator_noop_turn_still_rejects_failed_or_incomplete_checks() {
    use codex_config::RencrowTaskKind;
    let policy = RencrowExecutionGuard {
        task_kind: Some(RencrowTaskKind::Orchestrate),
        ..Default::default()
    };
    let mut state = GuardState::default();
    state.failed_checks.insert("failed check".into());
    assert_eq!(state.missing(&policy, false).len(), 1);
    state.failed_checks.clear();
    state.pending_checks.insert(1, ("pending".into(), 0));
    assert_eq!(state.missing(&policy, false).len(), 1);
    state.pending_checks.clear();
    state
        .started_checks
        .insert("call".into(), ("started".into(), 0));
    assert_eq!(state.missing(&policy, false).len(), 1);

    let readonly = RencrowExecutionGuard {
        task_kind: Some(RencrowTaskKind::ReadOnly),
        ..Default::default()
    };
    assert!(state.missing(&readonly, false).is_empty());
}

#[test]
fn proven_task_evidence_settlement_removes_only_exact_keys_and_is_idempotent() {
    let mut state = GuardState::default();
    state.failed_checks.insert("task-evidence:resolved".into());
    state.failed_checks.insert("task-evidence:other".into());
    state.failed_checks.insert("cargo test".into());
    state.reads = 4;
    state.budget_rejections = 2;
    state.verified_diff_stamps.insert(17);
    state.children.insert("child-id".into());
    let before = (
        state.reads,
        state.budget_rejections,
        state.verified_diff_stamps.clone(),
        state.children.clone(),
    );

    state.settle_proven_task_evidence_launches(&["resolved".into()]);
    assert!(!state.failed_checks.contains("task-evidence:resolved"));
    assert!(state.failed_checks.contains("task-evidence:other"));
    assert!(state.failed_checks.contains("cargo test"));
    assert_eq!(
        (
            state.reads,
            state.budget_rejections,
            state.verified_diff_stamps.clone(),
            state.children.clone(),
        ),
        before,
        "settlement does not alter budgets, stamps, or child evidence"
    );

    let after_first = state.failed_checks.clone();
    state.settle_proven_task_evidence_launches(&["resolved".into()]);
    assert_eq!(
        state.failed_checks, after_first,
        "repeating settlement is a no-op"
    );
}

fn go_command_item(
    call_id: &str,
    cwd: &std::path::Path,
    success: bool,
    output: &str,
) -> codex_protocol::items::TurnItem {
    codex_protocol::items::TurnItem::CommandExecution(codex_protocol::items::CommandExecutionItem {
        model_context: None,
        id: call_id.into(),
        plugin_id: None,
        script_path: None,
        process_id: Some(format!("process-{call_id}")),
        command: vec![
            "/bin/zsh".into(),
            "-lc".into(),
            "go vet ./internal/portal".into(),
        ],
        cwd: codex_utils_path_uri::PathUri::from_host_native_path(cwd)
            .expect("fixture cwd converts to a native path URI"),
        parsed_cmd: Vec::new(),
        source: codex_protocol::protocol::ExecCommandSource::UnifiedExecStartup,
        interaction_input: None,
        status: if success {
            codex_protocol::items::CommandExecutionStatus::Completed
        } else {
            codex_protocol::items::CommandExecutionStatus::Failed
        },
        stdout: Some(output.into()),
        stderr: Some(String::new()),
        aggregated_output: Some(output.into()),
        exit_code: Some(if success { 0 } else { 1 }),
        duration: Some(std::time::Duration::from_millis(1)),
        formatted_output: Some(output.into()),
    })
}

#[tokio::test]
async fn proven_launch_correction_settles_the_guard_before_the_next_read() {
    let invocation =
        exec_guard_invocation("cat Cargo.toml", RencrowExecutionGuard::default(), None).await;
    let workspace = tempfile::tempdir().unwrap();
    let canonical_workspace_root = workspace.path().canonicalize().unwrap();
    let source_root = canonical_workspace_root.as_path();
    let target_root = source_root.join("RenCrow_PORTAL");
    let artifact_path = PathBuf::from("internal/portal/action.go");
    let target_artifact = target_root.join(&artifact_path);
    std::fs::create_dir_all(target_artifact.parent().unwrap()).unwrap();
    std::fs::write(&target_artifact, "same retained artifact\n").unwrap();
    std::fs::write(target_root.join("go.mod"), "module example.test/portal\n").unwrap();

    let scope = crate::state::ActiveTaskScope {
        root_task_id: "root".into(),
        task_id: "unit-1".into(),
        task_revision: 1,
        begin_call_id: "begin-1".into(),
        start_history_cursor: 0,
    };
    let command = vec![
        "/bin/zsh".into(),
        "-lc".into(),
        "go vet ./internal/portal".into(),
    ];
    let source_paths = vec![PathBuf::from("RenCrow_PORTAL").join(&artifact_path)];
    let source_before = crate::task_check::hash_workspace_artifacts(source_root, &source_paths)
        .expect("source verification artifacts can be hashed");
    let source_obligation = crate::task_check::TaskCheckObligation::new(
        scope.clone(),
        source_root.to_path_buf(),
        "go vet ./internal/portal",
        source_paths.clone(),
    )
    .expect("source obligation is valid");
    let source_guard_key = format!("task-evidence:{}", source_obligation.key());
    invocation
        .session
        .begin_task_check_with_obligation(
            "failed-go-launch".into(),
            crate::task_check::TaskEvidenceRequest {
                task_id: scope.task_id.clone(),
                task_revision: scope.task_revision,
                artifact_paths: source_paths,
                check_kind: crate::task_check::TaskCheckKind::Verification,
            },
            source_root.to_path_buf(),
            source_before.clone(),
            Some(source_obligation),
        )
        .await;
    let diagnostic = "go: go.mod file not found in current directory or any parent directory; see 'go help modules'\n";
    invocation
        .session
        .emit_turn_item_completed(
            &invocation.turn,
            go_command_item("failed-go-launch", source_root, false, diagnostic),
        )
        .await;
    invocation
        .session
        .observe_task_check_terminal_event("failed-go-launch")
        .await;
    let source_observation = crate::task_check::TaskCheckObservation::from_execution(
        "failed-go-launch",
        &command,
        source_root,
        diagnostic,
        1,
        true,
        false,
    );
    invocation
        .session
        .finish_task_check_observed(
            "failed-go-launch",
            1,
            true,
            source_before,
            Some(source_observation),
        )
        .await
        .expect("failed host verification is retained");

    let target_paths = vec![artifact_path];
    let target_before = crate::task_check::hash_workspace_artifacts(&target_root, &target_paths)
        .expect("target verification artifacts can be hashed");
    let target_obligation = crate::task_check::TaskCheckObligation::new(
        scope,
        target_root.clone(),
        "go vet ./internal/portal",
        target_paths.clone(),
    )
    .expect("target obligation is valid");
    invocation
        .session
        .begin_task_check_with_obligation(
            "corrected-go-success".into(),
            crate::task_check::TaskEvidenceRequest {
                task_id: "unit-1".into(),
                task_revision: 1,
                artifact_paths: target_paths,
                check_kind: crate::task_check::TaskCheckKind::Verification,
            },
            target_root.clone(),
            target_before.clone(),
            Some(target_obligation),
        )
        .await;
    invocation
        .session
        .emit_turn_item_completed(
            &invocation.turn,
            go_command_item("corrected-go-success", &target_root, true, ""),
        )
        .await;
    invocation
        .session
        .observe_task_check_terminal_event("corrected-go-success")
        .await;
    let target_observation = crate::task_check::TaskCheckObservation::from_execution(
        "corrected-go-success",
        &command,
        &target_root,
        "",
        0,
        true,
        false,
    );
    invocation
        .session
        .finish_task_check_observed(
            "corrected-go-success",
            0,
            true,
            target_before,
            Some(target_observation),
        )
        .await
        .expect("corrected host verification is retained");

    assert_eq!(
        invocation
            .session
            .resolved_task_check_launch_obligation_keys()
            .await,
        vec![
            source_guard_key
                .trim_start_matches("task-evidence:")
                .to_owned()
        ]
    );
    let turn_state = {
        let active = invocation.session.active_turn.lock().await;
        std::sync::Arc::clone(&active.as_ref().expect("active turn").turn_state)
    };
    {
        let mut turn = turn_state.lock().await;
        turn.rencrow_execution_guard
            .failed_checks
            .insert(source_guard_key.clone());
        turn.rencrow_execution_guard
            .failed_checks
            .insert("task-evidence:another-obligation".into());
        turn.rencrow_execution_guard
            .failed_checks
            .insert("ordinary failed check".into());
    }

    before_tool(&invocation)
        .await
        .expect("the next read is admitted after proof-backed settlement");
    let turn = turn_state.lock().await;
    assert!(
        !turn
            .rencrow_execution_guard
            .failed_checks
            .contains(&source_guard_key)
    );
    assert!(
        turn.rencrow_execution_guard
            .failed_checks
            .contains("task-evidence:another-obligation")
    );
    assert!(
        turn.rencrow_execution_guard
            .failed_checks
            .contains("ordinary failed check")
    );
}

#[test]
fn only_completed_results_are_candidates_for_host_child_evidence() {
    use serde_json::json;
    let completed = serde_json::to_value(AgentStatus::Completed(Some("done".into()))).unwrap();
    let errored = serde_json::to_value(AgentStatus::Errored("failed".into())).unwrap();
    let shutdown = serde_json::to_value(AgentStatus::Shutdown).unwrap();
    assert_eq!(
        completed_child_ids(
            "wait_agent",
            &json!({}),
            &json!({"status":{"a":completed,"b":errored,"c":shutdown},"timed_out":false})
        ),
        vec!["a".to_string()]
    );
    assert!(
        completed_child_ids(
            "wait_agent",
            &json!({}),
            &json!({"status":{},"timed_out":true})
        )
        .is_empty()
    );
    assert!(
        completed_child_ids(
            "resume_agent",
            &json!({"id":"a"}),
            &json!({"status":errored})
        )
        .is_empty()
    );
    assert_eq!(
        completed_child_ids(
            "resume_agent",
            &json!({"id":"a"}),
            &json!({"status":completed})
        ),
        vec!["a".to_string()]
    );
}

#[test]
fn child_receipt_invalidates_parent_checks_started_before_acceptance() {
    use codex_config::RencrowTaskKind;
    let policy = RencrowExecutionGuard {
        task_kind: Some(RencrowTaskKind::Orchestrate),
        ..Default::default()
    };
    let mut state = GuardState::default();
    state.observe_diff(Some("parent diff"));
    let old_check_stamp = state.check_stamp();
    state
        .child_results
        .insert((codex_protocol::ThreadId::new(), "child-turn".into()));
    state.child_revision += 1;
    state.checked = false;
    state.finish_check("old async check", old_check_stamp, 0);
    assert_eq!(
        state.missing(&policy, true),
        vec!["successful check after the last edit (failed checks must be rerun)"]
    );

    let current_check_stamp = state.check_stamp();
    state.finish_check("fresh parent check", current_check_stamp, 0);
    assert!(state.missing(&policy, true).is_empty());
}

#[test]
fn a_new_child_receipt_applies_budget_progress_once() {
    let mut state = GuardState {
        budget_rejections: 3,
        ..Default::default()
    };
    state.repeated.insert("same read".into(), 2);
    state.arm_fatal_on_next_budget_refusal();
    state.record_child_progress();
    assert!(state.repeated.is_empty());
    assert_eq!(state.budget_rejections, 0);
    assert!(!state.fatal_on_next_budget_refusal);
    assert!(!state.record_budget_rejection());
    assert!(!state.record_budget_rejection());
    assert!(state.record_budget_rejection());
}

#[test]
fn shell_classification_preserves_checks_and_excludes_masked_failures() {
    for command in [
        "cat source.rs",
        "strings source.rs",
        "ls -l",
        "pwd",
        "rg target src",
        "cd /workspace && cat src.rs",
    ] {
        assert!(read_only(command), "{command}");
    }
    for command in [
        "python3 edit.py",
        "echo hello > source.rs",
        "go test ./...",
        "cat a; rm b",
    ] {
        assert!(!read_only(command), "{command}");
    }
    for command in [
        "go test ./...",
        "go test -tags realworld -count=1 -v -run TestRealWorldStoryPlayerRead ./cmd/rencrow",
        "cd /workspace && GOCACHE=/workspace/cache go test ./pkg",
        "cargo nextest run -p codex-core",
        "node --check app.js",
        "node --test internal/example.test.mjs",
        "/usr/local/bin/go test ./internal/example",
        "python3 -m py_compile src/example.py",
        "python3 -B -m py_compile src/example.py",
        "/opt/homebrew/bin/python3 -m py_compile src/example.py",
        "/Library/Frameworks/Python.framework/Versions/3.14/bin/Python -m unittest",
        "/Users/yukimi/.local/bin/rencrow-resource-check -- src/a.rs src/b.rs",
        "cd /Users/yukimi/RenCrow/Tmp/ren2-supervision/full-l1-20261029 && python3 -B -m unittest -v test_collect_films",
        "python -B -m pytest",
        "python3 -B -m compileall src",
        "python3 -m unittest tests",
        "python3 -B -m unittest -v tests",
        "bash scripts/e2e-static.sh",
        "bash scripts/e2e-static.sh 2>&1",
        "bash ./scripts/test-unit_3.sh",
        "bash scripts/test-unit.v1.sh",
        "scripts/e2e-static.sh",
        "./scripts/test-unit_3.sh",
        "RUN_E2E=1 bash scripts/e2e-static.sh",
        "RUN_E2E=1 ./scripts/e2e-static.sh",
        "go test -run \"TestOne|TestTwo\" ./...",
        "go test ./... 2>&1",
        "go test -run 'TestFetchRetries429|TestThrottleCooldown' ./...",
        "go test -run 'TestDollar$Literal|TestQuoted&&Name' ./...",
        "cd '/workspace with spaces' && GOCACHE='/cache|literal' go test -run 'TestOne|TestTwo' ./...",
        "export PATH=/opt/homebrew/bin:/usr/bin:/bin && export RUSTUP_TOOLCHAIN=1.95.0 && just test --locked -p codex-core --lib -E 'test(foo)'",
    ] {
        assert!(check_command(command), "{command}");
    }
    for command in [
        "echo 'go test'",
        "go test || true",
        "go test & true",
        "go test $(echo ./...)",
        "go test; true",
        "go test && echo done",
        "cat tests.log",
        "go test > saved.log",
        "go test ./... 2>&1 | head",
        "export PATH=/opt/homebrew/bin && just test --lib | head",
        "go test ./... 2>&1; echo done",
        "go test ./... 2>errors.log",
        "go test -run 'TestOne|TestTwo' ./... | cat",
        "go test -run 'TestOne|TestTwo' ./... || true",
        "go test -run \"$(echo Test)\" ./...",
        "go test -run \"`echo Test`\" ./...",
        "go test -run 'TestOne|TestTwo ./...",
        "1INVALID=value go test ./...",
        "python3 -B test_collect_films.py",
        "python3 tests.py",
        "python3 tests.py && echo done",
        "python3 -B -c 'print(1)'",
        "/opt/homebrew/bin/python3.14 -c 'print(1)'",
        "python3 -X dev -m unittest",
        "python3 -B -m unittest && echo done",
        "python3 -B -m unittest | cat",
        "python3 -B -m py_compile file.py && echo done",
        "python3 -B -m unittest > saved.log",
        "node -- test.js",
        "echo go test ./...",
        "python3 -m py_compile file.py && echo done",
        "bash scripts/e2e-static.sh extra",
        "bash scripts/../scripts/e2e-static.sh",
        "bash /workspace/scripts/e2e-static.sh",
        "bash scripts/unknown.sh",
        "bash scripts/test-.sh",
        "bash scripts/test-é.sh",
        "bash scripts/nested/test-unit.sh",
        "scripts/e2e-static.sh extra",
        "./scripts/test-unit_3.sh extra",
        "bash -c 'scripts/e2e-static.sh'",
        "bash scripts/e2e-static.sh | cat",
        "bash scripts/e2e-static.sh; echo done",
        "bash scripts/e2e-static.sh && echo done",
        "bash scripts/e2e-static.sh || true",
        "bash scripts/e2e-static.sh > result.log",
    ] {
        assert!(!check_command(command), "{command}");
    }
}

#[test]
fn corrected_environment_resolves_only_a_proven_cargo_launch_failure() {
    let cwd = std::env::current_dir().unwrap();
    let initial = "just test --locked -p codex-core --lib -E 'test(foo)'";
    let repaired = "export PATH=/opt/homebrew/opt/rustup/bin:/opt/homebrew/bin:/usr/bin:/bin && export RUSTUP_TOOLCHAIN=1.95.0 && just test --locked -p codex-core --lib -E 'test(foo)'";
    let parse = |command: &str| {
        crate::rencrow_check_identity::parse(command, &cwd, supported_check_words)
            .expect("literal check command")
            .identity
    };
    let initial_identity = parse(initial);
    let repaired_identity = parse(repaired);
    let startup_output = "RUST_MIN_STACK=16777216 NEXTEST_PROFILE=local cargo nextest run --no-fail-fast \"$@\"\ntest: cargo: command not found\nerror: recipe `test` failed on line 88 with exit code 127";
    let mut state = GuardState::default();
    state.observe_diff(Some("current patch"));
    let stamp = state.check_stamp();
    let initial_key = format!("{}:null:{initial}", cwd.display());
    state
        .check_identities
        .insert(initial_key.clone(), initial_identity.clone());
    state.finish_check_observed(&initial_key, stamp, 127, startup_output, false);
    assert!(state.failed_checks.contains(&initial_key));

    // The structured receipt can have an original token count without its output being
    // truncated. The caller only marks output truncated when the explicit warning is present.
    let repaired_key = format!("{}:null:{repaired}", cwd.display());
    state
        .check_identities
        .insert(repaired_key.clone(), repaired_identity.clone());
    state.finish_check_observed(&repaired_key, stamp, 0, "3 tests passed", false);
    assert!(!state.failed_checks.contains(&initial_key));
    assert!(
        state
            .missing(
                &RencrowExecutionGuard {
                    task_kind: Some(RencrowTaskKind::Orchestrate),
                    ..Default::default()
                },
                true
            )
            .is_empty()
    );

    for (suffix, identity, output) in [
        (
            "different-filter",
            {
                let mut identity = repaired_identity.clone();
                identity.runner.push("test(other)".into());
                identity
            },
            "3 tests passed",
        ),
        (
            "skip-env",
            {
                let mut identity = repaired_identity.clone();
                identity.environment.insert("TEST_SKIP".into(), "1".into());
                identity
            },
            "3 tests passed",
        ),
    ] {
        let mut state = GuardState::default();
        state.observe_diff(Some("current patch"));
        let stamp = state.check_stamp();
        state
            .check_identities
            .insert(initial_key.clone(), initial_identity.clone());
        state.finish_check_observed(&initial_key, stamp, 127, startup_output, false);
        let key = format!("{repaired_key}:{suffix}");
        state.check_identities.insert(key.clone(), identity);
        state.finish_check_observed(&key, stamp, 0, output, false);
        assert!(state.failed_checks.contains(&initial_key), "{suffix}");
    }

    let mut state = GuardState::default();
    state.observe_diff(Some("old patch"));
    let old_stamp = state.check_stamp();
    state
        .check_identities
        .insert(initial_key.clone(), initial_identity.clone());
    state.finish_check_observed(&initial_key, old_stamp, 127, startup_output, false);
    state.observe_diff(Some("new patch"));
    let new_stamp = state.check_stamp();
    state
        .check_identities
        .insert(repaired_key.clone(), repaired_identity);
    state.finish_check_observed(&repaired_key, old_stamp, 0, "3 tests passed", false);
    assert!(state.failed_checks.contains(&initial_key));
    assert_ne!(old_stamp, new_stamp);

    let mut state = GuardState::default();
    state.observe_diff(Some("current patch"));
    let stamp = state.check_stamp();
    state
        .check_identities
        .insert(initial_key.clone(), initial_identity);
    state.finish_check_observed(
        &initial_key,
        stamp,
        127,
        "RUST_MIN_STACK=16777216 NEXTEST_PROFILE=local cargo nextest run --no-fail-fast \"$@\"\ntest: cargo: command not found\nerror: recipe `test` failed on line 88 with exit code 127\nadditional test output",
        false,
    );
    assert!(
        !state
            .startup_failed_check_identities
            .contains_key(&initial_key)
    );
    state
        .check_identities
        .insert(repaired_key.clone(), parse(repaired));
    state.finish_check_observed(&repaired_key, stamp, 0, "3 tests passed", false);
    assert!(state.failed_checks.contains(&initial_key));
}

#[test]
fn equivalent_command_identity_resolves_formatting_variants_but_keeps_environment_distinct() {
    let cwd = std::env::current_dir().unwrap();
    let plain = "just test --locked -p codex-core --lib";
    let with_cd = format!(
        "cd '{}' && just   test --locked -p codex-core --lib",
        cwd.display()
    );
    let parse = |command: &str| {
        crate::rencrow_check_identity::parse(command, &cwd, supported_check_words)
            .expect("literal direct check")
            .identity
    };
    let plain_identity = parse(plain);
    let with_cd_identity = parse(&with_cd);
    assert_eq!(plain_identity, with_cd_identity);

    let mut state = GuardState::default();
    state.observe_diff(Some("current patch"));
    let stamp = state.check_stamp();
    let failed_key = format!("raw:{plain}");
    let passed_key = format!("raw:{with_cd}");
    state
        .check_identities
        .insert(failed_key.clone(), plain_identity);
    state.finish_check_observed(&failed_key, stamp, 1, "test failed", false);
    let mut different_environment = with_cd_identity;
    different_environment
        .environment
        .insert("TEST_SKIP".into(), "1".into());
    state
        .check_identities
        .insert(passed_key.clone(), different_environment);
    state.finish_check_observed(&passed_key, stamp, 0, "tests passed", false);
    assert!(state.failed_checks.contains(&failed_key));
}

#[test]
fn original_token_count_alone_does_not_mark_receipt_output_as_truncated() {
    let receipt = serde_json::json!({
        "exit_code": 127,
        "original_token_count": 44,
        "output": "RUST_MIN_STACK=16777216 NEXTEST_PROFILE=local cargo nextest run --no-fail-fast \"$@\"\ntest: cargo: command not found\nerror: recipe `test` failed on line 88 with exit code 127"
    });
    assert!(!check_output_may_be_truncated(&receipt));

    let truncated = serde_json::json!({
        "exit_code": 127,
        "original_token_count": 44,
        "output": "Warning: truncated output (original token count: 44)\n... 123 bytes omitted ..."
    });
    assert!(check_output_may_be_truncated(&truncated));
}

#[test]
fn embedded_direct_check_detection_is_narrow() {
    assert_eq!(
        direct_check_in_compound_command("cd /tmp && cat value.py && python3 -B -m unittest -q"),
        Some("python3 -B -m unittest -q".into())
    );
    for command in [
        "echo 'python3 -m unittest'",
        "cd /tmp && python3 -B -m unittest -q",
        "python3 tests.py && echo done",
        "cat value.py && env TEST=1 python3 -B -m unittest -q",
        "unsupported-shell-syntax <(python3 -m unittest)",
    ] {
        assert_eq!(direct_check_in_compound_command(command), None, "{command}");
    }
}

#[test]
fn direct_python_scripts_are_not_checks_but_the_unittest_runner_is() {
    use codex_config::RencrowTaskKind;
    let policy = RencrowExecutionGuard {
        task_kind: Some(RencrowTaskKind::Orchestrate),
        ..Default::default()
    };
    let mut state = GuardState::default();
    state.observe_diff(Some("current patch"));
    assert!(!check_command("python3 tests.py"));
    assert!(unrecognized_python_test_script(
        "python3 tests.py && echo done"
    ));
    assert!(!unrecognized_python_test_script("python3 app.py"));
    assert!(!unrecognized_python_test_script(
        "python3 -m unittest tests"
    ));
    assert_eq!(
        state.missing(&policy, true),
        vec!["successful check after the last edit (failed checks must be rerun)"]
    );

    let command = "python3 -m unittest -v tests";
    assert!(check_command(command));
    state.finish_check(command, state.check_stamp(), 0);
    assert!(state.missing(&policy, true).is_empty());
}

#[test]
fn direct_json_tool_input_check_is_credited_only_for_the_current_diff() {
    use codex_config::RencrowTaskKind;
    let policy = RencrowExecutionGuard {
        task_kind: Some(RencrowTaskKind::Orchestrate),
        ..Default::default()
    };
    let command = "python3 -m json.tool /absolute/path/progress.json";
    assert!(check_command(command));
    assert!(check_command(
        "python -m json.tool '/absolute/path/progress file.json'"
    ));
    assert!(check_command(
        "/usr/bin/python3 -m json.tool /absolute/path/progress.json"
    ));

    for command in [
        "python3 -m json.tool",
        "python3 -m json.tool --help",
        "python3 -m json.tool --sort-keys /absolute/path/progress.json",
        "python3 -m json.tool /absolute/path/input.json /absolute/path/output.json",
        "python3 -m json.tool /absolute/path/progress.json && echo valid",
        "echo python3 -m json.tool /absolute/path/progress.json",
        "python3 -m json.tool /absolute/path/progress.json > /absolute/path/output.json",
    ] {
        assert!(!check_command(command), "{command}");
    }

    let mut state = GuardState::default();
    state.observe_diff(Some("progress.json patch"));
    let stamp = state.check_stamp();
    state.finish_check(command, stamp, 1);
    assert!(state.failed_checks.contains(command));
    assert_eq!(
        state.missing(&policy, true),
        vec!["successful check after the last edit (failed checks must be rerun)"]
    );

    state.finish_check(command, stamp, 0);
    assert!(!state.failed_checks.contains(command));
    assert!(state.missing(&policy, true).is_empty());

    state.observe_diff(Some("progress.json changed after validation"));
    assert!(!state.checked);
    assert_eq!(
        state.missing(&policy, true),
        vec!["successful check after the last edit (failed checks must be rerun)"]
    );
}

#[test]
fn successful_unrecognized_test_script_gets_actionable_completion_guidance() {
    use codex_config::RencrowTaskKind;
    let policy = RencrowExecutionGuard {
        task_kind: Some(RencrowTaskKind::Orchestrate),
        ..Default::default()
    };
    let mut state = GuardState::default();
    state.observe_diff(Some("current patch"));
    state.record_uncredited_python_test("python3 tests.py && echo ALL_TESTS_PASSED");
    assert!(!state.checked);

    let mut attempts = 0;
    let prompt = state
        .completion_recovery_prompt(&policy, true, &mut attempts)
        .expect("missing recognized check should request recovery");
    assert!(prompt.contains("successful exec of `python3 tests.py"));
    assert!(prompt.contains("python3 -m unittest -v tests"));
    assert!(prompt.contains("pytest -q <test_path>"));

    state.finish_check("python3 -m unittest -v tests", state.check_stamp(), 0);
    assert!(state.uncredited_python_test().is_none());
    assert!(state.missing(&policy, true).is_empty());
}

#[test]
fn e2e_script_checks_require_a_successful_exit() {
    use codex_config::RencrowTaskKind;
    let policy = RencrowExecutionGuard {
        task_kind: Some(RencrowTaskKind::Orchestrate),
        ..Default::default()
    };
    let command = "./scripts/e2e-static.sh";
    assert!(check_command(command));

    let mut state = GuardState::default();
    state.observe_diff(Some("current patch"));
    state.finish_check(command, state.check_stamp(), 1);
    assert_eq!(
        state.missing(&policy, true),
        vec!["successful check after the last edit (failed checks must be rerun)"]
    );
    state.finish_check(command, state.check_stamp(), 0);
    assert!(state.missing(&policy, true).is_empty());
}

#[test]
fn config_limits_cannot_be_zero_or_weakened_by_a_role() {
    assert!(toml::from_str::<RencrowExecutionGuard>("max_read_only_calls = 0").is_err());
    let parent = RencrowExecutionGuard {
        require_check: true,
        ..Default::default()
    };
    let role = RencrowExecutionGuard {
        max_read_only_calls: NonZeroU16::new(100).unwrap(),
        require_edit: true,
        ..Default::default()
    };
    let merged = RencrowExecutionGuard::narrow(Some(parent.clone()), Some(role)).unwrap();
    assert_eq!(merged.max_read_only_calls, parent.max_read_only_calls);
    assert!(merged.require_edit && merged.require_check);
}

#[test]
fn executor_cannot_redelegate_through_a_direct_agent_cli() {
    assert!(nested_agent_cli("codex exec implement"));
    assert!(nested_agent_cli("/home/user/bin/codex exec implement"));
    assert!(nested_agent_cli("claude implement"));
    assert!(!nested_agent_cli("codex --version"));
    assert!(!nested_agent_cli("cat codex.rs"));
    assert!(!nested_agent_cli("echo 'codex exec implement'"));
}

#[test]
fn child_receipts_require_terminal_wait_or_explicit_close() {
    use serde_json::json;
    let mut state = GuardState::default();
    state.observe_child_result("spawn_agent", &json!({}), &json!({"agent_id":"child"}));
    for status in [
        AgentStatus::Running,
        AgentStatus::NotFound,
        AgentStatus::Interrupted,
    ] {
        state.observe_child_result(
            "wait_agent",
            &json!({}),
            &json!({"status":{"child":status},"timed_out":true}),
        );
        assert!(state.children.contains("child"));
    }
    state.observe_child_result("wait_agent", &json!({}), &json!({"status":{"child":AgentStatus::Errored("missing evidence".into())},"timed_out":false}));
    assert!(state.children.is_empty());
    state.observe_child_result(
        "resume_agent",
        &json!({"id":"child"}),
        &json!({"status":AgentStatus::Running}),
    );
    assert!(state.children.contains("child"));
    state.observe_child_result(
        "resume_agent",
        &json!({"id":"child"}),
        &json!({"status":AgentStatus::Errored("failed".into())}),
    );
    assert!(!state.children.contains("child"));
    state.observe_child_result(
        "close_agent",
        &json!({"target":"child"}),
        &json!({"previous_status":AgentStatus::Running}),
    );
    assert!(state.children.is_empty());
    state.observe_child_result(
        "send_input",
        &json!({"target":"child"}),
        &json!({"submission_id":"turn"}),
    );
    state.observe_child_result(
        "wait_agent",
        &json!({}),
        &json!({"status":{"child":AgentStatus::Completed(Some("done".into()))}}),
    );
    assert!(state.children.is_empty());
}

#[test]
fn child_recovery_is_bounded_and_requires_uncollected_children() {
    let mut state = GuardState::default();
    let mut attempts = 0;
    assert!(state.child_recovery_prompt(&mut attempts).is_none());
    state.children.insert("child-b".into());
    state.children.insert("child-a".into());
    let prompt = state.child_recovery_prompt(&mut attempts).unwrap();
    assert!(prompt.contains("child-a, child-b"));
    assert!(state.child_recovery_prompt(&mut attempts).is_some());
    assert!(state.child_recovery_prompt(&mut attempts).is_none());
    assert_eq!(attempts, 2);
    assert_eq!(state.children.len(), 2);
}

#[test]
fn completion_recovery_is_one_shot_and_uses_current_missing_evidence() {
    let policy = RencrowExecutionGuard {
        require_edit: true,
        require_check: true,
        ..Default::default()
    };
    let mut state = GuardState::default();
    state.observe_diff(None);
    let mut attempts = 0;
    let prompt = state
        .completion_recovery_prompt(&policy, false, &mut attempts)
        .unwrap();
    assert!(prompt.contains("accepted current-turn patch diff"));
    assert!(prompt.contains("successful check after the last edit"));
    assert_eq!(attempts, 1);
    assert!(
        state
            .completion_recovery_prompt(&policy, false, &mut attempts)
            .is_none()
    );

    state.observe_diff(Some("patch"));
    state.finish_check("check", state.diff_stamp, 0);
    assert!(state.missing(&policy, true).is_empty());
}

#[test]
fn typed_recovery_prompts_follow_verify_readonly_and_orchestrate_contracts() {
    use codex_config::RencrowTaskKind;
    let verify = RencrowExecutionGuard {
        task_kind: Some(RencrowTaskKind::Verify),
        require_edit: true,
        ..Default::default()
    };
    let mut state = GuardState::default();
    state.observe_diff(None);
    let mut attempts = 0;
    let prompt = state
        .completion_recovery_prompt(&verify, false, &mut attempts)
        .unwrap();
    assert!(prompt.contains("run and pass the required check"));
    assert!(!prompt.contains("make the required edit"));

    let readonly = RencrowExecutionGuard {
        task_kind: Some(RencrowTaskKind::ReadOnly),
        require_edit: true,
        require_check: true,
        ..Default::default()
    };
    assert!(
        state
            .completion_recovery_prompt(&readonly, false, &mut attempts)
            .is_none()
    );

    let orchestrate = RencrowExecutionGuard {
        task_kind: Some(RencrowTaskKind::Orchestrate),
        ..Default::default()
    };
    state.children.insert("child".into());
    let mut child_attempt = 0;
    assert!(state.child_recovery_prompt(&mut child_attempt).is_some());
    assert!(orchestrate.requires_child_results());
}

#[test]
fn timeout_does_not_collect_children_but_terminal_wait_does() {
    let mut state = GuardState::default();
    state.children.insert("child".into());
    state.observe_child_result(
        "wait_agent",
        &serde_json::json!({}),
        &serde_json::json!({"status": {}, "timed_out": true}),
    );
    assert!(state.children.contains("child"));
    state.observe_child_result("wait_agent", &serde_json::json!({}), &serde_json::json!({"status": {"child": serde_json::to_value(AgentStatus::Completed(Some("done".into()))).unwrap()}, "timed_out": false}));
    assert!(state.children.is_empty());
}

#[test]
fn bounded_read_pipelines_cannot_escape_the_read_budget() {
    for command in [
        "cd /workspace && rg target src | head -40",
        "cd /workspace && cat source.rs | tail -20",
        "cd /workspace; cat source.rs; echo separator; sed -n '1,20p' other.rs",
        "printf '%s' heading; rg target src | cut -c1-100",
    ] {
        assert!(read_only(command), "unmetered read-only command: {command}");
    }
}

#[tokio::test]
async fn host_child_recovery_uses_active_turn_and_stops_after_receipt() {
    let (session, mut turn) = crate::session::tests::make_session_and_context().await;
    std::sync::Arc::make_mut(&mut turn.config).rencrow_execution_guard =
        Some(RencrowExecutionGuard {
            require_child_results: true,
            ..Default::default()
        });
    let active = crate::state::ActiveTurn::default();
    let shared = active.turn_state.clone();
    shared
        .lock()
        .await
        .rencrow_execution_guard
        .children
        .insert("pending-child".into());
    *session.active_turn.lock().await = Some(active);
    let mut attempts = 0;
    assert!(
        child_recovery_prompt(&session, &turn, &mut attempts)
            .await
            .unwrap()
            .contains("pending-child")
    );
    shared
        .lock()
        .await
        .rencrow_execution_guard
        .observe_child_result(
            "close_agent",
            &serde_json::json!({"target":"pending-child"}),
            &serde_json::json!({"previous_status":"shutdown"}),
        );
    assert!(
        child_recovery_prompt(&session, &turn, &mut attempts)
            .await
            .is_none()
    );
    assert_eq!(attempts, 1);
}

#[tokio::test]
async fn session_evidence_is_in_memory_and_cleared_for_a_new_turn() {
    let (session, _) = crate::session::tests::make_session_and_context().await;
    let turn_id = "guarded-child-turn".to_string();
    session.record_started_turn(&turn_id).await;
    session
        .store_rencrow_execution_evidence(GuardExecutionEvidence {
            thread_id: session.thread_id(),
            turn_id: turn_id.clone(),
            parent_thread_id: codex_protocol::ThreadId::new(),
            diff_stamp: 7,
            has_diff: true,
            checked: true,
        })
        .await;
    assert!(
        session
            .rencrow_execution_evidence_for_test()
            .await
            .is_some()
    );

    assert!(
        session
            .rencrow_execution_evidence_for_test()
            .await
            .is_some()
    );

    session.record_started_turn("new-turn-after-resume").await;
    assert!(
        session
            .rencrow_execution_evidence_for_test()
            .await
            .is_none()
    );
}

#[tokio::test]
async fn parent_session_consumes_each_child_turn_receipt_once_across_parent_turns() {
    let (session, _) = crate::session::tests::make_session_and_context().await;
    let child_thread_id = codex_protocol::ThreadId::new();
    let evidence = GuardExecutionEvidence {
        thread_id: child_thread_id,
        turn_id: "child-turn-1".into(),
        parent_thread_id: session.thread_id(),
        diff_stamp: 17,
        has_diff: true,
        checked: true,
    };
    assert!(
        session
            .consume_rencrow_child_execution_evidence(&evidence)
            .await
    );

    // A new parent turn does not make the cached child turn a new receipt.
    session.record_started_turn("parent-turn-2").await;
    assert!(
        !session
            .consume_rencrow_child_execution_evidence(&evidence)
            .await
    );

    // The same child thread's genuinely new turn has independent evidence.
    let new_child_turn = GuardExecutionEvidence {
        turn_id: "child-turn-2".into(),
        ..evidence
    };
    assert!(
        session
            .consume_rencrow_child_execution_evidence(&new_child_turn)
            .await
    );
}
