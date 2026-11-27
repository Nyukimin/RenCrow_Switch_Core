// Modified by RenCrow Switch Core, 2026-11-27: configure native patch test filesystem helpers.
// Modified by RenCrow Switch Core, 2026-11-26: exercise common-harness gates through normal Begin.
use super::*;
use crate::session::common_harness_completion::CommonHarnessTurnEndDecision;
use codex_apply_patch::CODEX_CORE_APPLY_PATCH_ARG1;
#[cfg(unix)]
use codex_exec_server::CODEX_ARG0_EXEC_HELPER_ARG1;
use codex_exec_server::CODEX_FS_HELPER_ARG1;
use codex_exec_server::ExecServerRuntimePaths;
use codex_history::task_context::HostEventV1;
use codex_history::task_context::StateFenceV1;
use codex_history::task_context::TaskRunPhaseV1;
use codex_history::task_context::TaskRunStatusV1;
use codex_protocol::models::ContentItem;
use codex_sandboxing::landlock::CODEX_LINUX_SANDBOX_ARG0;
use codex_test_binary_support::TestBinaryDispatchGuard;
use codex_test_binary_support::TestBinaryDispatchMode;
use codex_test_binary_support::configure_test_binary_dispatch;
#[cfg(target_os = "windows")]
use codex_windows_sandbox::CODEX_WINDOWS_SANDBOX_ARG1;
use pretty_assertions::assert_eq;

#[ctor::ctor]
static COMMON_HARNESS_TEST_BINARY_DISPATCH: Option<TestBinaryDispatchGuard> = {
    configure_test_binary_dispatch("codex-core-common-harness-tests", |exe_name, argv1| {
        if argv1 == Some(CODEX_CORE_APPLY_PATCH_ARG1) {
            return TestBinaryDispatchMode::DispatchArg0Only;
        }
        #[cfg(unix)]
        if argv1 == Some(CODEX_ARG0_EXEC_HELPER_ARG1) {
            return TestBinaryDispatchMode::DispatchArg0Only;
        }
        if argv1 == Some(CODEX_FS_HELPER_ARG1) {
            return TestBinaryDispatchMode::DispatchArg0Only;
        }
        if exe_name == CODEX_LINUX_SANDBOX_ARG0 {
            return TestBinaryDispatchMode::DispatchArg0Only;
        }
        #[cfg(target_os = "windows")]
        if argv1 == Some(CODEX_WINDOWS_SANDBOX_ARG1) {
            return TestBinaryDispatchMode::DispatchArg0Only;
        }
        TestBinaryDispatchMode::InstallAliases
    })
};

fn common_harness_native_patch_test_runtime_paths() -> ExecServerRuntimePaths {
    #[cfg(target_os = "linux")]
    let linux_sandbox_exe = Some(
        COMMON_HARNESS_TEST_BINARY_DISPATCH
            .as_ref()
            .and_then(|dispatch| dispatch.paths().codex_linux_sandbox_exe.clone())
            .expect("Linux sandbox alias from the test binary dispatch helper"),
    );
    #[cfg(not(target_os = "linux"))]
    let linux_sandbox_exe = None;

    ExecServerRuntimePaths::new(
        std::env::current_exe().expect("current test binary path"),
        linux_sandbox_exe,
    )
    .expect("native patch test filesystem helper runtime paths")
}

#[path = "native_patch_recovery_tests.rs"]
mod native_patch_recovery_tests;
#[path = "common_harness_prompt_at03_tests.rs"]
mod prompt_at03_tests;
#[path = "common_harness_prompt_overflow_tests.rs"]
mod prompt_overflow_tests;
#[path = "common_harness_prompt_tests.rs"]
mod prompt_tests;

async fn common_task() -> (tempfile::TempDir, Arc<Session>, Arc<TurnContext>) {
    common_task_with_reproduction(true).await
}

async fn common_task_with_reproduction(
    reproduce_failure: bool,
) -> (tempfile::TempDir, Arc<Session>, Arc<TurnContext>) {
    common_task_in_store(reproduce_failure, false).await
}

async fn common_task_in_store(
    reproduce_failure: bool,
    local_writer: bool,
) -> (tempfile::TempDir, Arc<Session>, Arc<TurnContext>) {
    common_task_in_store_variant(
        reproduce_failure,
        local_writer,
        CommonTaskFixtureVariant::Default,
    )
    .await
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommonTaskFixtureVariant {
    Default,
    LoginShellEnabled,
    SuiteExcludesBaselineFailure,
    InconsistentSuiteExpectedCounts,
    FrameworkErrorAtBaseline,
    FreeformApplyPatchTool,
    At03PreBeginNotes,
    OversizedPromptContext,
    InvalidAdmission(InvalidAdmissionCase),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InvalidAdmissionCase {
    TraceUnavailable,
    GoTestExecHook,
    OutsidePackage,
    GoRun,
    FailedExpectedCount,
    SuiteFailureOutcome,
    ReproductionPassOutcome,
    OptionalOnlyConditions,
    MixedOptionalCondition,
    RootEqualsTask,
}

async fn assert_rejected_admission(case: InvalidAdmissionCase) {
    use InvalidAdmissionCase as Case;
    let (_workspace, session, _turn) = common_task_in_store_variant(
        true,
        false,
        CommonTaskFixtureVariant::InvalidAdmission(case),
    )
    .await;
    assert!(
        session.active_task_scope().await.is_none(),
        "{case:?} must be rejected before a task scope is activated"
    );
    assert!(
        session
            .current_common_harness_state(if case == Case::RootEqualsTask {
                "harness-work"
            } else {
                "harness-root"
            })
            .await
            .is_err(),
        "{case:?} must not persist an admitted common-harness state"
    );
}

macro_rules! admission_rejection_case {
    ($name:ident, $case:ident) => {
        #[tokio::test]
        async fn $name() {
            assert_rejected_admission(InvalidAdmissionCase::$case).await;
        }
    };
}

admission_rejection_case!(begin_rejects_go_exec_hook, GoTestExecHook);
admission_rejection_case!(
    begin_rejects_when_receipt_trace_is_disabled,
    TraceUnavailable
);
admission_rejection_case!(begin_rejects_outside_go_package, OutsidePackage);
admission_rejection_case!(begin_rejects_go_run_as_verifier, GoRun);
admission_rejection_case!(
    begin_rejects_failed_expected_suite_count,
    FailedExpectedCount
);
admission_rejection_case!(begin_rejects_failure_outcome_on_suite, SuiteFailureOutcome);
admission_rejection_case!(
    begin_rejects_all_pass_outcome_on_reproduction,
    ReproductionPassOutcome
);
admission_rejection_case!(begin_rejects_reference_only_profile, OptionalOnlyConditions);
admission_rejection_case!(
    begin_rejects_mixed_optional_reference_condition,
    MixedOptionalCondition
);
admission_rejection_case!(begin_rejects_root_equal_to_execution_task, RootEqualsTask);

async fn common_task_with_freeform_apply_patch_tool()
-> (tempfile::TempDir, Arc<Session>, Arc<TurnContext>) {
    common_task_in_store_variant(
        true,
        false,
        CommonTaskFixtureVariant::FreeformApplyPatchTool,
    )
    .await
}

async fn common_task_with_baseline_framework_error()
-> (tempfile::TempDir, Arc<Session>, Arc<TurnContext>) {
    common_task_in_store_variant(
        true,
        false,
        CommonTaskFixtureVariant::FrameworkErrorAtBaseline,
    )
    .await
}

async fn common_task_with_suite_excluding_baseline_failure()
-> (tempfile::TempDir, Arc<Session>, Arc<TurnContext>) {
    common_task_in_store_variant(
        true,
        false,
        CommonTaskFixtureVariant::SuiteExcludesBaselineFailure,
    )
    .await
}

async fn common_task_with_inconsistent_suite_expected_counts()
-> (tempfile::TempDir, Arc<Session>, Arc<TurnContext>) {
    common_task_in_store_variant(
        false,
        false,
        CommonTaskFixtureVariant::InconsistentSuiteExpectedCounts,
    )
    .await
}

async fn common_task_in_store_variant(
    reproduce_failure: bool,
    local_writer: bool,
    variant: CommonTaskFixtureVariant,
) -> (tempfile::TempDir, Arc<Session>, Arc<TurnContext>) {
    common_task_in_store_variant_with_provider(reproduce_failure, local_writer, variant, None).await
}

async fn common_task_in_store_variant_with_provider(
    reproduce_failure: bool,
    local_writer: bool,
    variant: CommonTaskFixtureVariant,
    provider_override: Option<codex_model_provider_info::ModelProviderInfo>,
) -> (tempfile::TempDir, Arc<Session>, Arc<TurnContext>) {
    let workspace = tempfile::tempdir().expect("task workspace");
    let fixture_home = workspace.path().join(".host");
    std::fs::create_dir(&fixture_home).expect("fixture host storage");
    let go_cache = std::env::var_os("GOCACHE")
        .map(std::path::PathBuf::from)
        .filter(|path| path.is_dir())
        .unwrap_or_else(|| fixture_home.join("go-cache"));
    std::fs::create_dir_all(&go_cache).expect("fixture Go cache");
    std::fs::write(
        workspace.path().join("go.mod"),
        "module example.test/harness\n\ngo 1.24\n",
    )
    .unwrap();
    std::fs::write(
        workspace.path().join("value.go"),
        if variant == CommonTaskFixtureVariant::FrameworkErrorAtBaseline {
            "package harness\nfunc Value( {\n"
        } else if variant == CommonTaskFixtureVariant::SuiteExcludesBaselineFailure {
            if reproduce_failure {
                "package harness\nfunc Value() int { return 1 }\nfunc Other() int { return 1 }\n"
            } else {
                "package harness\nfunc Value() int { return 2 }\nfunc Other() int { return 1 }\n"
            }
        } else if reproduce_failure {
            "package harness\nfunc Value() int { return 1 }\n"
        } else {
            "package harness\nfunc Value() int { return 2 }\n"
        },
    )
    .unwrap();
    let test_source = if variant == CommonTaskFixtureVariant::SuiteExcludesBaselineFailure {
        "package harness\nimport \"testing\"\nfunc TestHarness(t *testing.T) { if Value() != 2 { t.Fatal(\"value expected 2, got 1\") } }\nfunc TestOther(t *testing.T) { if Other() != 1 { t.Fatalf(\"other value expected 1, got %d\", Other()) } }\n"
    } else {
        "package harness\nimport \"testing\"\nfunc TestHarness(t *testing.T) { if Value() != 2 { t.Fatal(\"value expected 2, got 1\") } }\n"
    };
    std::fs::write(workspace.path().join("value_test.go"), test_source).unwrap();
    let configure_config = |config: &mut crate::config::Config| {
        if let Some(provider) = &provider_override {
            config.model_provider = provider.clone();
            config.model_provider.supports_websockets = false;
        }
        config
            .permissions
            .set_permission_profile(codex_protocol::models::PermissionProfile::workspace_write())
            .expect("fixture Go compilation requires the standard workspace-write profile");
        if variant == CommonTaskFixtureVariant::LoginShellEnabled {
            config.permissions.allow_login_shell = true;
        }
        config
            .permissions
            .shell_environment_policy
            .r#set
            .insert("GOCACHE".into(), go_cache.to_string_lossy().into_owned());
        config.codex_home = fixture_home
            .clone()
            .try_into()
            .expect("absolute fixture host storage");
        config.cwd = workspace
            .path()
            .to_path_buf()
            .try_into()
            .expect("absolute fixture workspace");
    };
    let (session, turn, _events) = if matches!(
        variant,
        CommonTaskFixtureVariant::FreeformApplyPatchTool
            | CommonTaskFixtureVariant::At03PreBeginNotes
    ) {
        crate::session::tests::make_session_and_context_with_auth_and_config_and_rx_and_runtime_paths(
            codex_login::CodexAuth::from_api_key("Test API Key"),
            Vec::new(),
            configure_config,
            common_harness_native_patch_test_runtime_paths(),
        )
        .await
    } else {
        make_session_and_context_with_auth_and_config_and_rx(
            codex_login::CodexAuth::from_api_key("Test API Key"),
            Vec::new(),
            configure_config,
        )
        .await
    };
    let mut session = Arc::try_unwrap(session)
        .ok()
        .expect("unique fixture session");
    let mut turn = Arc::try_unwrap(turn).expect("unique fixture turn");
    turn.session_source = SessionSource::Cli;
    Arc::make_mut(&mut turn.config).rencrow_compaction = true;
    Arc::make_mut(&mut turn.config).rencrow_autonomous_completion = true;
    if matches!(
        variant,
        CommonTaskFixtureVariant::FreeformApplyPatchTool
            | CommonTaskFixtureVariant::At03PreBeginNotes
    ) {
        crate::session::tests::update_turn_settings_for_test(&mut turn, |settings| {
            Arc::make_mut(&mut settings.model_info).apply_patch_tool_type =
                Some(codex_protocol::openai_models::ApplyPatchToolType::Freeform);
        });
    }
    let turn = Arc::new(turn);
    let local_store = Arc::clone(&session.services.thread_store);
    let _store = attach_boundary_owner(&mut session, Arc::clone(&turn)).await;
    if local_writer {
        session.services.live_thread = None;
        session.services.thread_store = local_store;
        crate::session::tests::attach_thread_persistence(&mut session).await;
    }
    // Common Harness tests need a real receipt sink; use its explicit root seam
    // instead of mutating CODEX_ROLLOUT_TRACE_ROOT for the whole process.
    if variant != CommonTaskFixtureVariant::InvalidAdmission(InvalidAdmissionCase::TraceUnavailable)
    {
        let trace_root = fixture_home.join("rollout-traces");
        std::fs::create_dir_all(&trace_root).expect("fixture trace root");
        session.services.rollout_thread_trace =
            codex_rollout_trace::ThreadTraceContext::start_root_in_root_for_test(
                &trace_root,
                codex_rollout_trace::ThreadStartedTraceMetadata {
                    thread_id: session.thread_id.to_string(),
                    agent_path: "/root".into(),
                    task_name: None,
                    nickname: None,
                    agent_role: None,
                    session_source: SessionSource::Cli,
                    cwd: turn.config.cwd.as_path().to_path_buf(),
                    rollout_path: None,
                    model: turn.model_info().slug.clone(),
                    provider_name: turn.config.model_provider_id.clone(),
                    approval_policy: turn.approval_policy().to_string(),
                    sandbox_policy: format!("{:?}", turn.sandbox_policy()),
                },
            )
            .expect("enabled fixture trace");
        session
            .services
            .rollout_thread_trace
            .record_codex_turn_started(turn.sub_id.as_str());
    }
    let session = Arc::new(session);
    if variant == CommonTaskFixtureVariant::At03PreBeginNotes {
        session
            .record_annotated_conversation_items(
                &turn,
                turn.model_info().as_ref(),
                vec![
                    ResponseItemEnvelope::new(ResponseItem::Message {
                        id: None,
                        role: "user".into(),
                        content: vec![ContentItem::InputText {
                            text: "Continue the existing workspace maintenance.".into(),
                        }],
                        phase: None,
                        internal_chat_message_metadata_passthrough: None,
                    }),
                    ResponseItemEnvelope::new(ResponseItem::Message {
                        id: None,
                        role: "assistant".into(),
                        content: vec![ContentItem::OutputText {
                            text: "ordinary historical maintenance notes ".repeat(4_000),
                        }],
                        phase: None,
                        internal_chat_message_metadata_passthrough: None,
                    }),
                ],
            )
            .await;
    }
    let paths = json!(["go.mod", "value.go", "value_test.go"]);
    let check = |expected_outcome: serde_json::Value| {
        json!({
            "argv": ["go", "test", "-json", "-count=1", "-run", "^TestHarness$", "."],
            "parser": "go_test_json", "expected_outcome": expected_outcome,
            "go_test_selection": {"selected_cases": ["TestHarness"], "expected_case_count": 1},
            "cwd_binding_refs": paths.clone(), "allowed_environment_refs": [],
            "toolchain_ref": "go", "timeout_ms": 30000
        })
    };
    let mut request = json!({
        "contract_version": "common-harness/1.0", "task_id": "harness-work", "request_revision": 1,
        "goal": "Fix Value and pass the specified regression", "constraints": ["Keep the test unchanged"],
        "conditions": [
            {"condition_id":"AC-reproduce", "condition_revision":1, "kind":"reproduce_failure",
             "comparison_scope":"TestHarness", "expected":"baseline fails and candidate passes", "required":true,
             "verifier_ref":"codex-common-harness/go-test-json/1", "verifier_config_revision":1,
             "check":check(json!({"specified_test_failure":{"test_name":"TestHarness","reason":"value expected 2, got 1"}})),
             "dependency_refs":paths.clone()},
            {"condition_id":"AC-suite", "condition_revision":1, "kind":"test_suite",
             "comparison_scope":"TestHarness", "expected":"one regression passes with zero failures", "required":true,
             "verifier_ref":"codex-common-harness/go-test-json/1", "verifier_config_revision":1,
             "check":check(json!({"test_counts":{"passed":1,"failed":0,"skipped":0}})), "dependency_refs":paths}
        ],
        "condition_set_revision":1, "max_attempts":3, "skill_refs":[], "execution_owner":"standalone_switch"
    });
    if variant == CommonTaskFixtureVariant::OversizedPromptContext {
        request["constraints"] = json!(vec!["x".repeat(4096); 9]);
    }
    match variant {
        CommonTaskFixtureVariant::Default | CommonTaskFixtureVariant::LoginShellEnabled => {}
        CommonTaskFixtureVariant::SuiteExcludesBaselineFailure => {
            request["conditions"][1]["comparison_scope"] = json!("TestOther");
            request["conditions"][1]["expected"] = json!("one unrelated regression passes");
            request["conditions"][1]["check"]["argv"][5] = json!("^TestOther$");
            request["conditions"][1]["check"]["go_test_selection"]["selected_cases"] =
                json!(["TestOther"]);
        }
        CommonTaskFixtureVariant::InconsistentSuiteExpectedCounts => {
            request["conditions"][1]["check"]["expected_outcome"] =
                json!({"test_counts":{"passed":2,"failed":0,"skipped":0}});
        }
        CommonTaskFixtureVariant::FrameworkErrorAtBaseline => {}
        CommonTaskFixtureVariant::FreeformApplyPatchTool
        | CommonTaskFixtureVariant::At03PreBeginNotes => {}
        CommonTaskFixtureVariant::OversizedPromptContext => {}
        CommonTaskFixtureVariant::InvalidAdmission(case) => match case {
            InvalidAdmissionCase::TraceUnavailable => {}
            InvalidAdmissionCase::GoTestExecHook => {
                request["conditions"][1]["check"]["argv"] = json!([
                    "go",
                    "test",
                    "-json",
                    "-count=1",
                    "-exec",
                    "/bin/sh",
                    "-run",
                    "^TestHarness$",
                    "."
                ]);
            }
            InvalidAdmissionCase::OutsidePackage => {
                request["conditions"][1]["check"]["argv"][6] = json!("../outside");
            }
            InvalidAdmissionCase::GoRun => {
                request["conditions"][1]["check"]["argv"] = json!(["go", "run", "./value.go"]);
            }
            InvalidAdmissionCase::FailedExpectedCount => {
                request["conditions"][1]["check"]["expected_outcome"] =
                    json!({"test_counts":{"passed":0,"failed":1,"skipped":0}});
            }
            InvalidAdmissionCase::SuiteFailureOutcome => {
                request["conditions"][1]["check"]["expected_outcome"] = serde_json::to_value(
                    codex_history::task_context::ExpectedCheckOutcomeV1::SpecifiedTestFailure {
                        test_name: "TestHarness".into(),
                        reason: "value expected 2, got 1".into(),
                    },
                )
                .expect("serialize the declared expected-outcome variant");
            }
            InvalidAdmissionCase::ReproductionPassOutcome => {
                request["conditions"][0]["check"]["expected_outcome"] = serde_json::to_value(
                    codex_history::task_context::ExpectedCheckOutcomeV1::AllSelectedTestsPass,
                )
                .expect("serialize the declared expected-outcome variant");
            }
            InvalidAdmissionCase::OptionalOnlyConditions => {
                request["conditions"][0]["required"] = json!(false);
                request["conditions"][1]["required"] = json!(false);
            }
            InvalidAdmissionCase::MixedOptionalCondition => {
                request["conditions"][0]["required"] = json!(false);
            }
            InvalidAdmissionCase::RootEqualsTask => {}
        },
    }
    if !reproduce_failure {
        request["conditions"].as_array_mut().unwrap().remove(0);
    }
    let root_task_id = if variant
        == CommonTaskFixtureVariant::InvalidAdmission(InvalidAdmissionCase::RootEqualsTask)
    {
        "harness-work"
    } else {
        "harness-root"
    };
    let args = json!({"action":"begin", "root_task_id":root_task_id, "task_id":"harness-work",
        "task_revision":1, "common_harness_request":request});
    let _: super::TaskContextBoundaryArgs = serde_json::from_value(args.clone())
        .expect("fixture is a valid TaskContextBoundaryArgs request before admission");
    let call_id = "begin-harness-task";
    let arguments = args.to_string();
    record_call_with_arguments(
        &session,
        &turn,
        call_id,
        crate::tools::handlers::task_context_boundary_spec::TASK_CONTEXT_BOUNDARY_TOOL_NAME,
        &arguments,
    )
    .await;
    let result = TaskContextBoundaryHandler::new(true)
        .handle(invocation_in_session(
            Arc::clone(&session),
            Arc::clone(&turn),
            call_id,
            args,
        ))
        .await;
    if variant == CommonTaskFixtureVariant::InvalidAdmission(InvalidAdmissionCase::TraceUnavailable)
    {
        match result {
            Err(error) => assert!(
                error.to_string().contains("rollout tracing"),
                "trace-disabled Begin must identify the missing receipt sink"
            ),
            Ok(_) => panic!("trace-disabled Begin must be rejected before admission"),
        }
    } else if matches!(
        variant,
        CommonTaskFixtureVariant::InconsistentSuiteExpectedCounts
            | CommonTaskFixtureVariant::InvalidAdmission(_)
    ) {
        assert!(
            result.is_err(),
            "Begin must reject unsupported admission case {variant:?}"
        );
    } else {
        assert!(
            result.is_ok(),
            "normal Begin must register the task: {:?}",
            result.err()
        );
        record_output(
            &session,
            &turn,
            call_id,
            crate::tools::handlers::task_context_boundary_spec::TASK_CONTEXT_BOUNDARY_TOOL_NAME,
        )
        .await;
    }
    (workspace, session, turn)
}

#[tokio::test]
async fn common_harness_owner_recovery_blocks_before_inference_without_trace() {
    let (_workspace, session_arc, turn) = common_task_in_store(true, true).await;
    let mut session = Arc::try_unwrap(session_arc)
        .ok()
        .expect("unique fixture session");
    let running = session
        .ensure_common_harness_attempt("harness-root")
        .await
        .unwrap();
    let previous_owner = session
        .apply_common_harness_host_event(
            "harness-root",
            StateFenceV1 {
                request_revision: running.request.request_revision,
                expected_state_version: running.run_state.state_version,
                owner_epoch: running.run_state.owner_epoch,
            },
            HostEventV1::TakeOwnership {
                new_owner_id: "fixture-previous-runtime".into(),
                new_owner_epoch: running.run_state.owner_epoch + 1,
                takeover_proof_ref: "fixture:previous-runtime".into(),
            },
        )
        .await
        .unwrap();

    // Force the persisted old-owner recovery branch, then simulate that the
    // restored Session could not open its new root trace. Writer proof remains real.
    session.services.rollout_thread_trace = codex_rollout_trace::ThreadTraceContext::disabled();
    let error = session
        .common_harness_inference_deadline(&turn)
        .await
        .unwrap_err();
    assert!(error.contains("trace is unavailable"));

    let blocked = session
        .current_common_harness_state("harness-root")
        .await
        .unwrap();
    assert_eq!(
        blocked.run_state.owner_id,
        crate::session::common_harness::common_harness_runtime_owner_id(
            &session.thread_id().to_string()
        )
    );
    assert_eq!(
        blocked.run_state.owner_epoch,
        previous_owner.run_state.owner_epoch + 1
    );
    assert_eq!(
        blocked.run_state.attempt_no, previous_owner.run_state.attempt_no,
        "missing trace must not consume a new task attempt"
    );
    assert_eq!(blocked.run_state.status, TaskRunStatusV1::Blocked);
    assert_eq!(
        blocked.run_state.reason_code.as_deref(),
        Some("trace_unavailable")
    );
}

#[tokio::test]
async fn common_harness_turn_end_leaves_normal_chat_on_the_legacy_path() {
    let (session, turn) = make_session_and_context().await;
    let before = session.clone_history().await.raw_items().count();
    assert_eq!(
        session.common_harness_turn_end_gate(&turn).await.unwrap(),
        CommonHarnessTurnEndDecision::Legacy
    );
    assert_eq!(session.clone_history().await.raw_items().count(), before);
    assert!(session.active_task_scope().await.is_none());
}

#[tokio::test]
async fn common_harness_turn_end_cannot_complete_from_an_assistant_report() {
    let (_workspace, session, turn) = common_task().await;
    assert!(matches!(
        session.common_harness_turn_end_gate(&turn).await.unwrap(),
        CommonHarnessTurnEndDecision::Continue { .. }
    ));
    let current = session
        .current_common_harness_state("harness-root")
        .await
        .unwrap();
    assert_eq!(current.run_state.status, TaskRunStatusV1::Running);
    assert_eq!(current.run_state.attempt_no, 1);
    assert_eq!(current.run_state.unmet_conditions.len(), 2);
    assert!(current.evidence_refs.is_empty());
}

#[tokio::test]
async fn common_harness_accept_rejects_model_supplied_satisfied_conditions() {
    let (_workspace, session, turn) = common_task().await;
    let result = TaskContextBoundaryHandler::new(true)
        .handle(invocation_in_session(
            Arc::clone(&session),
            Arc::clone(&turn),
            "model-selected-common-harness-accept",
            json!({
                "action":"accept",
                "result":"The task is done.",
                "satisfied_conditions":["AC-reproduce", "AC-suite"]
            }),
        ))
        .await;
    let error = result
        .err()
        .expect("the model cannot choose host-satisfied conditions");
    assert!(
        format!("{error:?}").contains("host-owned"),
        "unexpected common-harness Accept rejection: {error:?}"
    );
}

#[tokio::test]
async fn common_harness_turn_end_waiting_does_not_consume_an_attempt_or_restart_inference() {
    let (_workspace, session, turn) = common_task().await;
    let started = session
        .ensure_common_harness_attempt("harness-root")
        .await
        .unwrap();
    let waiting = session
        .apply_common_harness_host_event(
            "harness-root",
            StateFenceV1 {
                request_revision: started.request.request_revision,
                expected_state_version: started.run_state.state_version,
                owner_epoch: started.run_state.owner_epoch,
            },
            HostEventV1::EnterWaiting {
                question_ref: "question-1".into(),
                operation_digest: "bound-operation-1".into(),
                resume_phase: TaskRunPhaseV1::Plan,
            },
        )
        .await
        .unwrap();
    // Exercise the actual pre-turn compaction seam, before the later inference gate.
    // Cancellation makes an attempted model compaction return immediately instead of
    // contacting a provider; a waiting task must skip that request entirely.
    session.set_total_tokens_full(&turn).await;
    assert!(
        crate::session::context_window::context_window_token_status(&session, &turn)
            .await
            .token_limit_reached,
        "the fixture must actually require compaction"
    );
    let cancellation = tokio_util::sync::CancellationToken::new();
    cancellation.cancel();
    let mut client = session.services.model_client.new_session();
    assert!(
        crate::session::turn::run_pre_sampling_compact(
            &session,
            &turn,
            &mut client,
            &cancellation,
        )
        .await
        .is_ok(),
        "waiting must skip model compaction even when the context is full"
    );
    let decision = session.common_harness_turn_end_gate(&turn).await.unwrap();
    assert!(matches!(
        decision,
        CommonHarnessTurnEndDecision::Stopped {
            status: TaskRunStatusV1::Waiting,
            ..
        }
    ));
    assert_eq!(
        session
            .current_common_harness_state("harness-root")
            .await
            .unwrap(),
        waiting
    );
}

#[tokio::test]
async fn common_harness_turn_end_preserves_a_blocked_task_and_its_resume_condition() {
    let (_workspace, session, turn) = common_task().await;
    let started = session
        .ensure_common_harness_attempt("harness-root")
        .await
        .unwrap();
    let blocked = session
        .apply_common_harness_host_event(
            "harness-root",
            StateFenceV1 {
                request_revision: started.request.request_revision,
                expected_state_version: started.run_state.state_version,
                owner_epoch: started.run_state.owner_epoch,
            },
            HostEventV1::Block {
                reason_code: "verifier-unavailable".into(),
                resume_condition: "restore-registered-verifier".into(),
            },
        )
        .await
        .unwrap();
    let decision = session.common_harness_turn_end_gate(&turn).await.unwrap();
    assert!(matches!(
        decision,
        CommonHarnessTurnEndDecision::Stopped {
            status: TaskRunStatusV1::Blocked,
            ..
        }
    ));
    assert_eq!(
        session
            .current_common_harness_state("harness-root")
            .await
            .unwrap(),
        blocked
    );
}

#[cfg(test)]
#[path = "common_harness_producer_tests.rs"]
mod producer_tests;

#[path = "common_harness_deadline_tests.rs"]
mod deadline_tests;

#[path = "common_harness_review_actor_tests.rs"]
mod review_actor_tests;

#[cfg(test)]
#[path = "common_harness_acceptance_tests.rs"]
mod acceptance_tests;

#[cfg(test)]
#[path = "common_harness_owner_recovery_tests.rs"]
mod owner_recovery_tests;

#[tokio::test]
async fn common_harness_turn_end_observes_changed_files_before_continuing() {
    let (workspace, session, turn) = common_task().await;
    let started = session
        .ensure_common_harness_attempt("harness-root")
        .await
        .unwrap();
    std::fs::write(
        workspace.path().join("value.go"),
        "package harness\nfunc Value() int { return 2 }\n",
    )
    .unwrap();
    assert!(matches!(
        session.common_harness_turn_end_gate(&turn).await.unwrap(),
        CommonHarnessTurnEndDecision::Continue { .. }
    ));
    let current = session
        .current_common_harness_state("harness-root")
        .await
        .unwrap();
    assert_ne!(
        current.run_state.target_version,
        started.run_state.target_version
    );
    assert_eq!(current.run_state.attempt_no, started.run_state.attempt_no);
    assert!(current.run_state.state_version > started.run_state.state_version);
}

async fn fresh_legacy_boundary_session() -> (Arc<Session>, Arc<TurnContext>) {
    let (mut session, mut turn) = make_session_and_context().await;
    turn.session_source = SessionSource::Cli;
    Arc::make_mut(&mut turn.config).rencrow_compaction = true;
    Arc::make_mut(&mut turn.config).rencrow_autonomous_completion = true;
    let turn = Arc::new(turn);
    attach_boundary_owner(&mut session, Arc::clone(&turn)).await;
    (Arc::new(session), turn)
}

#[tokio::test]
async fn common_harness_raw_state_cannot_be_injected_through_legacy_begin() {
    let (_workspace, source, _source_turn) = common_task().await;
    let contract = source
        .rencrow_completion_state("harness-root")
        .await
        .unwrap();
    let (session, turn) = fresh_legacy_boundary_session().await;
    let result = TaskContextBoundaryHandler::new(true)
        .handle(invocation_in_session(
            Arc::clone(&session),
            turn,
            "injected-host-state-begin",
            json!({"action":"begin", "root_task_id":"harness-root", "task_id":"harness-work",
                "task_revision":1, "completion_state":contract}),
        ))
        .await;
    assert!(
        result.is_err(),
        "model input cannot install host-only v1 state"
    );
    assert!(
        session
            .rencrow_completion_state("harness-root")
            .await
            .is_none()
    );
    assert!(session.active_task_scope().await.is_none());
}

#[tokio::test]
async fn common_harness_raw_state_cannot_be_injected_through_legacy_update() {
    let (_workspace, source, _source_turn) = common_task().await;
    let contract = source
        .rencrow_completion_state("harness-root")
        .await
        .unwrap();
    let mut legacy = serde_json::to_value(&contract).unwrap();
    legacy["common_harness"] = serde_json::Value::Null;
    let (session, turn) = fresh_legacy_boundary_session().await;
    TaskContextBoundaryHandler::new(true)
        .handle(invocation_in_session(
            Arc::clone(&session),
            Arc::clone(&turn),
            "begin-real-legacy",
            json!({"action":"begin", "root_task_id":"harness-root", "task_id":"harness-work",
                "task_revision":1, "completion_state":legacy}),
        ))
        .await
        .expect("legacy contract remains supported");
    let mut legacy_update = serde_json::to_value(
        session
            .rencrow_completion_state("harness-root")
            .await
            .unwrap(),
    )
    .unwrap();
    legacy_update["common_harness"] = serde_json::Value::Null;
    TaskContextBoundaryHandler::new(true)
        .handle(invocation_in_session(
            Arc::clone(&session),
            Arc::clone(&turn),
            "update-real-legacy-null-extension",
            json!({"action":"update", "root_task_id":"harness-root", "completion_state":legacy_update}),
        ))
        .await
        .expect("an explicit null extension is valid legacy input");
    let before = session
        .rencrow_completion_state("harness-root")
        .await
        .unwrap();
    let result = TaskContextBoundaryHandler::new(true)
        .handle(invocation_in_session(
            Arc::clone(&session),
            turn,
            "injected-host-state-update",
            json!({"action":"update", "root_task_id":"harness-root", "completion_state":contract}),
        ))
        .await;
    assert!(
        result.is_err(),
        "model Update cannot install host-only v1 state"
    );
    assert_eq!(
        session
            .rencrow_completion_state("harness-root")
            .await
            .unwrap(),
        before
    );
}
