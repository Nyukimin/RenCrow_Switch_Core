use super::*;
use crate::rencrow_execution_guard::ToolReceipt;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context_with_auth_and_config_and_rx;
use crate::session::turn_context::TurnContext;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::registry::ToolExecutor;
use codex_history::task_context::AcceptanceCondition;
use codex_history::task_context::AcceptanceStatus;
use codex_history::task_context::CheckOutputParserV1;
use codex_history::task_context::CheckSpecV1;
use codex_history::task_context::CommonHarnessStateV1;
use codex_history::task_context::CompletionState;
use codex_history::task_context::CompletionUnit;
use codex_history::task_context::CompletionUnitKind;
use codex_history::task_context::ConditionKindV1;
use codex_history::task_context::ConditionSpecV1;
use codex_history::task_context::ExecutionOwnerV1;
use codex_history::task_context::ExpectedCheckOutcomeV1;
use codex_history::task_context::GoTestSelectionV1;
use codex_history::task_context::TaskRequestV1;
use codex_history::task_context::UnitStatus;
use codex_login::CodexAuth;
use codex_protocol::protocol::SessionSource;
use std::num::NonZeroU16;
use std::sync::Arc;

fn request_with_paths(paths: Vec<String>) -> TaskRequestV1 {
    TaskRequestV1 {
        task_id: "workspace-read-task".into(),
        request_revision: 1,
        authorized_request_ref: "host-auth".into(),
        goal: "Read bounded task files".into(),
        workspace_ref: "host-workspace".into(),
        start_target_version: "sha256:baseline".into(),
        permission_snapshot_ref: "sha256:permissions".into(),
        constraints: vec![],
        conditions: vec![ConditionSpecV1 {
            condition_id: "AC-1".into(),
            condition_revision: 1,
            kind: ConditionKindV1::TestSuite,
            comparison_scope: "TestBoundedRead".into(),
            expected: "selected test passes".into(),
            required: true,
            verifier_ref: Some("codex-common-harness/go-test-json/1".into()),
            verifier_config_revision: Some(1),
            check: Some(CheckSpecV1 {
                argv: vec![
                    "go".into(),
                    "test".into(),
                    "-json".into(),
                    "-count=1".into(),
                    "./...".into(),
                ],
                parser: CheckOutputParserV1::GoTestJson,
                expected_outcome: ExpectedCheckOutcomeV1::AllSelectedTestsPass,
                go_test_selection: Some(GoTestSelectionV1 {
                    selected_cases: vec!["TestBoundedRead".into()],
                    expected_case_count: 1,
                }),
                cwd_binding_refs: paths.clone(),
                allowed_environment_refs: vec![],
                toolchain_ref: Some("/host/go:go version go1.26.3".into()),
                timeout_ms: 10_000,
            }),
            dependency_refs: paths,
        }],
        condition_set_revision: 1,
        max_attempts: 2,
        operation_limits_ref: "host-limits".into(),
        skill_refs: vec![],
        execution_owner: ExecutionOwnerV1::StandaloneSwitch,
    }
}

#[test]
fn declared_path_union_is_limited_to_sixteen_paths() {
    let paths = (0..17).map(|index| format!("file-{index}.txt")).collect();
    let result = declared_paths(&request_with_paths(paths));
    assert!(matches!(result, Err(FunctionCallError::RespondToModel(_))));
}

#[test]
fn workspace_read_uses_a_default_cap_and_marks_truncated_output() {
    assert_eq!(
        validate_output_limit(None).expect("default output cap"),
        MAX_OUTPUT_BYTES
    );
    assert!(matches!(
        validate_output_limit(Some(MAX_OUTPUT_BYTES + 1)),
        Err(FunctionCallError::RespondToModel(_))
    ));
    let bounded = bound_output("x".repeat(MAX_OUTPUT_BYTES + 100), MAX_OUTPUT_BYTES);
    assert!(bounded.len() <= MAX_OUTPUT_BYTES);
    assert!(bounded.ends_with("[output truncated by host]"));
}

#[cfg(unix)]
#[test]
fn workspace_read_rejects_parent_and_symlink_paths_before_returning_contents() {
    use std::os::unix::fs::symlink;

    let parent = tempfile::tempdir().expect("workspace parent fixture");
    let workspace = parent.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace directory");
    let outside = parent.path().join("secret.txt");
    std::fs::write(&outside, "private").expect("outside file");
    symlink(&outside, workspace.join("link.txt")).expect("symlink fixture");

    assert!(read_bound_file(&workspace, Path::new("../secret.txt")).is_err());
    assert!(read_bound_file(&workspace, Path::new("link.txt")).is_err());
}

async fn read_invocation() -> (
    ToolInvocation,
    Arc<crate::session::session::Session>,
    Arc<TurnContext>,
    tempfile::TempDir,
) {
    let workspace = tempfile::tempdir().expect("bounded read workspace");
    std::fs::write(
        workspace.path().join("fixture.txt"),
        "bounded fixture contents\n",
    )
    .expect("fixture file");
    let (session, turn, _events) = make_session_and_context_with_auth_and_config_and_rx(
        CodexAuth::from_api_key("Test API Key"),
        Vec::new(),
        |config| {
            config.cwd = workspace
                .path()
                .to_path_buf()
                .try_into()
                .expect("absolute read workspace");
        },
    )
    .await;
    let mut turn = Arc::try_unwrap(turn).expect("unique fixture turn context");
    turn.session_source = SessionSource::Cli;
    let config = Arc::make_mut(&mut turn.config);
    config.rencrow_compaction = true;
    config.rencrow_autonomous_completion = true;
    config.rencrow_execution_guard = Some(codex_config::RencrowExecutionGuard {
        max_repeated_reads: NonZeroU16::new(1).expect("nonzero read cap"),
        ..Default::default()
    });
    let turn = Arc::new(turn);

    let root = workspace
        .path()
        .canonicalize()
        .expect("canonical workspace");
    let binding = crate::common_harness_io::ArtifactBindingV1::for_canonical_root(&root)
        .expect("bind workspace");
    let mut request = request_with_paths(vec!["fixture.txt".into()]);
    request.workspace_ref = binding.workspace_binding_ref;
    let common = CommonHarnessStateV1::queued(request, "writer".into(), 1)
        .expect("valid common-harness read request");
    let completion = CompletionState {
        contract_revision: 1,
        units_completed_since_review: 0,
        plan_review_at: None,
        plan_reviews: vec![],
        paused: false,
        consumed_host_events: vec![],
        conditions: vec![AcceptanceCondition {
            id: "AC-1".into(),
            revision: 1,
            result: "selected test passes".into(),
            scope: "TestBoundedRead".into(),
            verification: "go test".into(),
            acceptance_criteria: "selected test passes".into(),
            status: AcceptanceStatus::Open,
            evidence: vec![],
            unmet_reason: None,
        }],
        units: vec![CompletionUnit {
            id: "workspace-read-task".into(),
            purpose: "Read bounded task files".into(),
            status: UnitStatus::Ready,
            kind: CompletionUnitKind::Execution,
            condition_refs: vec!["AC-1".into()],
            depends_on: vec![],
            evidence: vec![],
            task_refs: vec![],
            review_target: None,
            review_evidence: None,
            review_launch: None,
            review_decision_reason: None,
            review_state: None,
            review_revalidation_of: None,
        }],
        current_unit: Some("workspace-read-task".into()),
        failed_approaches: vec![],
        blocker: None,
        next_action: Some("Read a declared task file".into()),
        product_progress_at: None,
        runtime_recovery_at: None,
        activity_at: None,
        common_harness: Some(common),
    };
    session
        .set_rencrow_completion_state(
            "workspace-read-root".into(),
            serde_json::to_value(completion).unwrap(),
        )
        .await
        .expect("store v1 task state");
    session
        .begin_task_scope(
            "workspace-read-root".into(),
            "workspace-read-task".into(),
            1,
            "workspace-read-begin".into(),
            false,
        )
        .await
        .expect("activate v1 task scope");
    *session.active_turn.lock().await = Some(crate::state::ActiveTurn::default());
    let invocation = ToolInvocation {
        session: Arc::clone(&session),
        step_context: StepContext::for_test(Arc::clone(&turn)),
        turn: Arc::clone(&turn),
        cancellation_token: tokio_util::sync::CancellationToken::new(),
        tracker: Arc::new(tokio::sync::Mutex::new(
            crate::turn_diff_tracker::TurnDiffTracker::new(),
        )),
        call_id: "workspace-read-call".into(),
        tool_name: ToolName::plain(WORKSPACE_READ_TOOL_NAME),
        source: ToolCallSource::Direct,
        payload: ToolPayload::Function {
            arguments: serde_json::json!({"action":"read", "paths":["fixture.txt"]}).to_string(),
        },
    };
    (invocation, session, turn, workspace)
}

#[tokio::test]
async fn repeated_workspace_read_is_charged_once_and_second_identical_read_is_refused() {
    let (invocation, _session, _turn, _workspace) = read_invocation().await;
    crate::rencrow_execution_guard::before_tool(&invocation)
        .await
        .expect("first admitted read is allowed");
    let output = WorkspaceReadHandler
        .handle(invocation.clone())
        .await
        .expect("bounded workspace read succeeds");
    assert!(output.log_output().contains("bounded fixture contents"));
    crate::rencrow_execution_guard::after_tool(
        &invocation,
        ToolReceipt {
            value: output.code_mode_result(&invocation.payload),
            successful: output.success_for_logging(),
        },
    )
    .await;
    let second = crate::rencrow_execution_guard::before_tool(&invocation).await;
    assert!(
        matches!(second, Err(FunctionCallError::RespondToModel(message)) if message.contains("same read command repeated")),
        "repeated structured reads use the existing execution guard budget"
    );
}
