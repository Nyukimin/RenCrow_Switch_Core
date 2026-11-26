// Modified by RenCrow Switch Core, 2026-11-27: enable real receipt tracing in Common Begin fixtures.
// Modified by RenCrow Switch Core, 2026-11-21: cover accepted task revision history through accept, update, and reload.
// Modified by RenCrow Switch Core, 2026-11-21: cover host verification diagnostics in status and accept.
// Modified by RenCrow Switch Core, 2026-11-20: separate caller and host checkpoint budgets.
// Modified by RenCrow Switch Core, 2026-11-16.
use super::*;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context;
use crate::session::tests::make_session_and_context_with_auth_and_config_and_rx;
use crate::session::turn_context::TurnContext;
use crate::state::ActiveTurn;
use crate::state::RunningTask;
use crate::state::TaskKind;
use crate::task_check::TaskCheckExecution;
use crate::task_check::TaskCheckKind;
use crate::task_check::TaskEvidenceRequest;
use crate::tasks::RegularTask;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolInvocation;
use crate::tools::registry::ToolExecutor;
use crate::turn_diff_tracker::TurnDiffTracker;
use codex_history::ResponseItemEnvelope;
use codex_history::task_context::TaskStatus;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::protocol::SessionSource;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::Mutex;

#[path = "completion_checkpoint_budget_tests.rs"]
mod completion_checkpoint_budget_tests;

#[path = "common_harness_lifecycle_tests.rs"]
mod common_harness_lifecycle_tests;

static TEST_ACTIVE_TURNS: codex_diagnostics::Gauge =
    codex_diagnostics::Gauge::new("task_context_boundary_test_active_turns");

async fn invocation(
    source: SessionSource,
    call_id: &str,
    args: serde_json::Value,
) -> ToolInvocation {
    invocation_with_autonomous_completion(source, call_id, args, false).await
}

async fn invocation_with_autonomous_completion(
    source: SessionSource,
    call_id: &str,
    args: serde_json::Value,
    autonomous_completion: bool,
) -> ToolInvocation {
    let (session, mut turn) = make_session_and_context().await;
    turn.session_source = source;
    Arc::make_mut(&mut turn.config).rencrow_compaction = true;
    Arc::make_mut(&mut turn.config).rencrow_autonomous_completion = autonomous_completion;
    let turn = Arc::new(turn);
    ToolInvocation {
        session: session.into(),
        step_context: StepContext::for_test(Arc::clone(&turn)),
        turn,
        cancellation_token: tokio_util::sync::CancellationToken::new(),
        tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
        call_id: call_id.to_string(),
        tool_name: ToolName::plain(TASK_CONTEXT_BOUNDARY_TOOL_NAME),
        source: ToolCallSource::Direct,
        payload: ToolPayload::Function {
            arguments: serde_json::to_string(&args).unwrap(),
        },
    }
}

fn invocation_in_session(
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    call_id: &str,
    args: serde_json::Value,
) -> ToolInvocation {
    ToolInvocation {
        session,
        step_context: StepContext::for_test(Arc::clone(&turn)),
        turn,
        cancellation_token: tokio_util::sync::CancellationToken::new(),
        tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
        call_id: call_id.to_string(),
        tool_name: ToolName::plain(TASK_CONTEXT_BOUNDARY_TOOL_NAME),
        source: ToolCallSource::Direct,
        payload: ToolPayload::Function {
            arguments: serde_json::to_string(&args).unwrap(),
        },
    }
}

async fn attach_boundary_owner(
    session: &mut Session,
    turn: Arc<TurnContext>,
) -> Arc<codex_thread_store::InMemoryThreadStore> {
    use codex_thread_store::CreateThreadParams;
    use codex_thread_store::InMemoryThreadStore;
    use codex_thread_store::LiveThread;
    use codex_thread_store::ThreadPersistenceMetadata;

    let store = Arc::new(InMemoryThreadStore::default());
    let thread_store: Arc<dyn codex_thread_store::ThreadStore> = store.clone();
    let config = session.get_config().await;
    let live_thread = LiveThread::create(
        Arc::clone(&thread_store),
        CreateThreadParams {
            creator_user_id: None,
            creator_account_id: None,
            session_id: session.session_id(),
            thread_id: session.thread_id(),
            extra_config: None,
            forked_from_id: None,
            parent_thread_id: None,
            source: SessionSource::Cli,
            thread_source: None,
            originator: "test_originator".into(),
            base_instructions: codex_protocol::models::BaseInstructions::default(),
            dynamic_tools: Vec::new(),
            selected_capability_roots: Vec::new(),
            multi_agent_version: None,
            history_mode: Default::default(),
            subagent_history_start_ordinal: None,
            history_base: None,
            initial_window_id: uuid::Uuid::now_v7().to_string(),
            runtime_workspace_roots: None,
            metadata: ThreadPersistenceMetadata {
                cwd: Some(config.cwd.to_path_buf()),
                model_provider: config.model_provider_id.clone(),
                memory_mode: if config.memories.generate_memories {
                    codex_protocol::protocol::ThreadMemoryMode::Enabled
                } else {
                    codex_protocol::protocol::ThreadMemoryMode::Disabled
                },
            },
        },
    )
    .await
    .expect("create owner persistence");
    session.services.thread_store = thread_store;
    session.services.live_thread = Some(live_thread);

    let cancellation_token = tokio_util::sync::CancellationToken::new();
    let task = RunningTask {
        done: Arc::new(tokio::sync::Notify::new()),
        kind: TaskKind::Regular,
        task: Arc::new(RegularTask::new()),
        cancellation_token,
        handle: tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async {
            std::future::pending::<()>().await;
        })),
        turn_context: turn,
        _agent_execution_guard: None,
        _diagnostics_guard: TEST_ACTIVE_TURNS.track(),
        _timer: None,
    };
    let active = ActiveTurn {
        task: Some(task),
        ..Default::default()
    };
    *session.active_turn.lock().await = Some(active);
    store
}

fn enable_common_harness_fixture_trace(session: &mut Session, turn: &TurnContext) {
    let trace_root = turn.config.codex_home.as_path().join("rollout-traces");
    std::fs::create_dir_all(&trace_root).expect("common Begin trace fixture root");
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
        .expect("enabled Common Begin fixture trace");
    session
        .services
        .rollout_thread_trace
        .record_codex_turn_started(turn.sub_id.as_str());
}

fn begin(root: &str, task: &str, revision: u64) -> serde_json::Value {
    json!({"action":"begin", "root_task_id":root, "task_id":task, "task_revision":revision})
}

#[tokio::test]
async fn begin_accepts_and_persists_common_harness_request_while_legacy_begin_remains_accepted() {
    let workspace = tempfile::tempdir().expect("bounded Go fixture workspace");
    std::fs::write(
        workspace.path().join("go.mod"),
        "module example.test/common-harness\n\ngo 1.24\n",
    )
    .expect("write Go fixture module");
    std::fs::write(
        workspace.path().join("task_test.go"),
        "package commonharness\nimport \"testing\"\nfunc TestBoundedFixture(t *testing.T) {}\n",
    )
    .expect("write Go fixture test");
    let (session, turn_context, _events) = make_session_and_context_with_auth_and_config_and_rx(
        codex_login::CodexAuth::from_api_key("Test API Key"),
        Vec::new(),
        |config| {
            config.cwd = workspace
                .path()
                .to_path_buf()
                .try_into()
                .expect("absolute Go fixture workspace");
        },
    )
    .await;
    let mut session = Arc::try_unwrap(session)
        .ok()
        .expect("unique configured session for persistence setup");
    let mut turn_context = Arc::try_unwrap(turn_context)
        .expect("unique configured turn context for persistence setup");
    let skill_dir = session
        .get_config()
        .await
        .codex_home
        .join("skills")
        .join("semantic-coding");
    std::fs::create_dir_all(&skill_dir).expect("create host skill fixture directory");
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: semantic-coding\ndescription: fixture skill\n---\n\n# Fixture\n",
    )
    .expect("write host skill fixture");
    turn_context.session_source = SessionSource::Cli;
    Arc::make_mut(&mut turn_context.config).rencrow_compaction = true;
    Arc::make_mut(&mut turn_context.config).rencrow_autonomous_completion = true;
    let turn = Arc::new(turn_context);
    let _store = attach_boundary_owner(&mut session, Arc::clone(&turn)).await;
    enable_common_harness_fixture_trace(&mut session, &turn);
    let session = Arc::new(session);

    let request = json!({
        "contract_version": "common-harness/1.0",
        "task_id": "task-v1",
        "request_revision": 1,
        "authorized_request_ref": "user-request-1",
        "goal": "Verify a bounded task request is registered by the host",
        "workspace_ref": "workspace-current",
        "start_target_version": "baseline-version",
        "permission_snapshot_ref": "permission-snapshot-current",
        "constraints": ["Do not expand the authorized workspace"],
        "conditions": [{
            "condition_id": "AC-1",
            "condition_revision": 1,
            "kind": "test_suite",
            "comparison_scope": "TestBoundedFixture",
            "expected": "the single selected fixture test passes",
            "required": true,
            "verifier_ref": "codex-common-harness/go-test-json/1",
            "verifier_config_revision": 1,
            "check": {
                "argv": ["go", "test", "-json", "-count=1", "-run", "^TestBoundedFixture$", "."],
                "parser": "go_test_json",
                "expected_outcome": {
                    "test_counts": {"passed": 1, "failed": 0, "skipped": 0}
                },
                "go_test_selection": {
                    "selected_cases": ["TestBoundedFixture"],
                    "expected_case_count": 1
                },
                "cwd_binding_refs": ["go.mod", "task_test.go"],
                "allowed_environment_refs": [],
                "toolchain_ref": "go",
                "timeout_ms": 30000
            },
            "dependency_refs": ["go.mod", "task_test.go"]
        }],
        "condition_set_revision": 1,
        "max_attempts": 3,
        "operation_limits_ref": "operation-limits-1",
        "skill_refs": [{
            "id": "semantic-coding",
            "version_ref": "local-current",
            "content_digest": "sha256:skill-content",
            "source_ref": "skills/semantic-coding/SKILL.md"
        }],
        "execution_owner": "standalone_switch"
    });
    let mut omitted_host_metadata_request = request.clone();
    for host_owned in [
        "authorized_request_ref",
        "workspace_ref",
        "start_target_version",
        "permission_snapshot_ref",
        "operation_limits_ref",
    ] {
        omitted_host_metadata_request
            .as_object_mut()
            .expect("request object")
            .remove(host_owned);
    }
    omitted_host_metadata_request["skill_refs"] = json!([{"id": "semantic-coding"}]);
    let begin_args = json!({
        "action": "begin",
        "root_task_id": "root-v1",
        "task_id": "task-v1",
        "task_revision": 1,
        "common_harness_request": request
    });
    let result = TaskContextBoundaryHandler::new(true)
        .handle(invocation_in_session(
            Arc::clone(&session),
            Arc::clone(&turn),
            "begin-common-harness-v1",
            begin_args,
        ))
        .await;
    assert!(
        result.is_ok(),
        "Begin should accept a common_harness_request: {:?}",
        result.err()
    );

    assert_eq!(
        session
            .active_task_scope()
            .await
            .map(|scope| { (scope.root_task_id, scope.task_id, scope.task_revision) }),
        Some(("root-v1".into(), "task-v1".into(), 1))
    );
    let stored = session
        .rencrow_completion_state("root-v1")
        .await
        .expect("v1 completion state remains available after Begin persistence");
    let stored = serde_json::to_value(stored).expect("serialize persisted completion state");
    assert_eq!(stored["common_harness"]["contract_version"], 1);
    assert_eq!(stored["common_harness"]["request"]["task_id"], "task-v1");
    assert_eq!(stored["common_harness"]["request"]["request_revision"], 1);
    assert_ne!(
        stored["common_harness"]["request"]["authorized_request_ref"], "user-request-1",
        "authorization provenance comes from the host session"
    );
    assert_ne!(
        stored["common_harness"]["request"]["workspace_ref"], "workspace-current",
        "workspace binding comes from the session cwd"
    );
    assert_ne!(
        stored["common_harness"]["request"]["start_target_version"], "baseline-version",
        "starting target version is captured by the host"
    );
    assert_eq!(
        stored["common_harness"]["run_state"]["target_version"],
        stored["common_harness"]["request"]["start_target_version"]
    );
    assert_ne!(
        stored["common_harness"]["request"]["permission_snapshot_ref"],
        "permission-snapshot-current",
        "permissions are bound from the host session"
    );
    assert!(
        !stored["common_harness"]["request"]["skill_refs"]
            .to_string()
            .contains("sha256:skill-content"),
        "skill references are bound to host-resolved skill content"
    );
    assert_ne!(
        stored["common_harness"]["request"]["conditions"][0]["check"]["toolchain_ref"], "go",
        "the Go toolchain is resolved by the host"
    );

    let downgraded_begin = TaskContextBoundaryHandler::new(true)
        .handle(invocation_in_session(
            Arc::clone(&session),
            Arc::clone(&turn),
            "legacy-begin-cannot-downgrade-v1",
            begin("root-v1", "task-v1", 1),
        ))
        .await;
    assert!(
        matches!(
            downgraded_begin,
            Err(FunctionCallError::RespondToModel(message))
                if message.contains("common-harness v1")
        ),
        "a legacy Begin must fail explicitly while the active scope is v1"
    );
    let legacy_update = TaskContextBoundaryHandler::new(true)
        .handle(invocation_in_session(
            Arc::clone(&session),
            Arc::clone(&turn),
            "legacy-update-cannot-replace-v1",
            json!({
                "action": "update",
                "root_task_id": "root-v1",
                "completion_state": {
                    "contract_revision": 9,
                    "conditions": [],
                    "units": [],
                }
            }),
        ))
        .await;
    assert!(
        matches!(
            legacy_update,
            Err(FunctionCallError::RespondToModel(message))
                if message.contains("common-harness v1")
        ),
        "legacy Update must not replace or bypass host-owned v1 state"
    );
    let legacy_accept = TaskContextBoundaryHandler::new(true)
        .handle(invocation_in_session(
            Arc::clone(&session),
            Arc::clone(&turn),
            "legacy-accept-cannot-bypass-v1",
            json!({"action": "accept", "result": "claimed complete"}),
        ))
        .await;
    assert!(
        matches!(
            legacy_accept,
            Err(FunctionCallError::RespondToModel(message))
                if message.contains("common-harness v1")
        ),
        "legacy Accept must use the v1 host completion gate"
    );
    let after_legacy_actions = session
        .rencrow_completion_state("root-v1")
        .await
        .expect("v1 state remains installed after rejected legacy actions");
    assert_eq!(
        serde_json::to_value(after_legacy_actions).expect("serialize unchanged completion state")["common_harness"],
        stored["common_harness"],
        "legacy actions cannot remove or mutate the persisted v1 state"
    );

    let (omitted_session, omitted_turn_context, _omitted_events) =
        make_session_and_context_with_auth_and_config_and_rx(
            codex_login::CodexAuth::from_api_key("Test API Key"),
            Vec::new(),
            |config| {
                config.cwd = workspace
                    .path()
                    .to_path_buf()
                    .try_into()
                    .expect("absolute Go fixture workspace");
            },
        )
        .await;
    let mut omitted_session = Arc::try_unwrap(omitted_session)
        .ok()
        .expect("unique configured session for omitted-metadata persistence setup");
    let mut omitted_turn_context = Arc::try_unwrap(omitted_turn_context)
        .expect("unique configured turn context for omitted-metadata setup");
    omitted_turn_context.session_source = SessionSource::Cli;
    Arc::make_mut(&mut omitted_turn_context.config).rencrow_compaction = true;
    Arc::make_mut(&mut omitted_turn_context.config).rencrow_autonomous_completion = true;
    let omitted_skill_dir = omitted_session
        .get_config()
        .await
        .codex_home
        .join("skills")
        .join("semantic-coding");
    std::fs::create_dir_all(&omitted_skill_dir).expect("create host skill fixture directory");
    std::fs::write(
        omitted_skill_dir.join("SKILL.md"),
        "---\nname: semantic-coding\ndescription: fixture skill\n---\n\n# Fixture\n",
    )
    .expect("write host skill fixture");
    let omitted_turn = Arc::new(omitted_turn_context);
    let _omitted_store =
        attach_boundary_owner(&mut omitted_session, Arc::clone(&omitted_turn)).await;
    enable_common_harness_fixture_trace(&mut omitted_session, &omitted_turn);
    let omitted_session = Arc::new(omitted_session);
    TaskContextBoundaryHandler::new(true)
        .handle(invocation_in_session(
            Arc::clone(&omitted_session),
            Arc::clone(&omitted_turn),
            "begin-common-harness-with-host-defaults",
            json!({
                "action": "begin",
                "root_task_id": "root-host-defaults",
                "task_id": "task-v1",
                "task_revision": 1,
                "common_harness_request": omitted_host_metadata_request,
            }),
        ))
        .await
        .expect("ordinary Begin may omit host-owned reference metadata");
    let omitted_stored = omitted_session
        .rencrow_completion_state("root-host-defaults")
        .await
        .expect("omitted-metadata Begin persisted host state");
    assert!(
        !omitted_stored
            .common_harness
            .unwrap()
            .request
            .workspace_ref
            .is_empty()
    );

    let (mut legacy_session, mut legacy_turn_context) = make_session_and_context().await;
    legacy_turn_context.session_source = SessionSource::Cli;
    Arc::make_mut(&mut legacy_turn_context.config).rencrow_compaction = true;
    let legacy_turn = Arc::new(legacy_turn_context);
    let _legacy_store = attach_boundary_owner(&mut legacy_session, Arc::clone(&legacy_turn)).await;
    let legacy_session = Arc::new(legacy_session);
    TaskContextBoundaryHandler::new(false)
        .handle(invocation_in_session(
            Arc::clone(&legacy_session),
            Arc::clone(&legacy_turn),
            "begin-legacy-control",
            begin("legacy-root", "legacy-task", 1),
        ))
        .await
        .expect("legacy Begin without common_harness_request remains accepted");
    assert_eq!(
        legacy_session.active_task_scope().await.map(|scope| (
            scope.root_task_id,
            scope.task_id,
            scope.task_revision
        )),
        Some(("legacy-root".into(), "legacy-task".into(), 1))
    );
}

#[test]
fn status_presents_current_review_reason_without_changing_canonical_next_action() {
    let state: codex_history::task_context::CompletionState = serde_json::from_value(json!({
        "contract_revision": 2,
        "conditions": [{
            "id": "AC-1", "revision": 1, "result": "Output", "scope": "CLI",
            "verification": "Check output", "acceptance_criteria": "Check passes",
            "status": "satisfied", "evidence": []
        }],
        "units": [],
        "next_action": "Keep the canonical plan action unchanged"
    }))
    .expect("minimal completion state deserializes");

    let diagnostics = json!({
        "count": 1,
        "omitted": 0,
        "attempts": [{
            "call_id": "verify-call-17",
            "launch_phase": "go_module_discovery",
            "observed_cwd": "/repo",
            "diagnostic": {"excerpt": "go.mod was not found", "truncated": false}
        }]
    });
    let rendered = super::render_completion_status(
        &state,
        Some(
            "review-a is stale for the current accepted artifact; add an explicitly linked review",
        ),
        Some(&diagnostics),
    )
    .expect("status output serializes");
    let status_value: serde_json::Value = serde_json::from_str(&rendered)
        .expect("status output remains a single parseable JSON object");
    assert_eq!(
        status_value["current_review_obligation"],
        "review-a is stale for the current accepted artifact; add an explicitly linked review"
    );
    assert_eq!(status_value["verification_obligations"], diagnostics);
    assert_eq!(
        status_value["next_action"],
        "Keep the canonical plan action unchanged"
    );
    assert!(
        serde_json::from_value::<codex_history::task_context::CompletionState>(
            status_value.clone()
        )
        .is_err(),
        "status-only annotation is not accepted as model-authored completion state"
    );
    let mut rendered_state = status_value;
    assert_eq!(
        rendered_state
            .as_object_mut()
            .unwrap()
            .remove("current_review_obligation"),
        Some(json!(
            "review-a is stale for the current accepted artifact; add an explicitly linked review"
        ))
    );
    assert!(
        rendered_state
            .as_object_mut()
            .unwrap()
            .remove("verification_obligations")
            .is_some()
    );
    assert_eq!(rendered_state, serde_json::to_value(&state).unwrap());
    assert_eq!(
        rendered_state["next_action"],
        "Keep the canonical plan action unchanged"
    );

    let empty_diagnostics = json!({"count":0,"omitted":0,"attempts":[]});
    assert_eq!(
        super::render_completion_status(&state, /*reason*/ None, Some(&empty_diagnostics))
            .expect("empty diagnostics do not add a wire annotation"),
        serde_json::to_string(&state).expect("canonical state serializes")
    );
    assert_eq!(
        super::render_completion_status(
            &state, /*reason*/ None, /*verification_obligations*/ None
        )
        .expect("no-reason output serializes"),
        serde_json::to_string(&state).expect("canonical state serializes")
    );
}

#[tokio::test]
async fn status_reports_host_verification_attempts_and_accept_points_to_status() {
    let (mut session, mut turn_context) = make_session_and_context().await;
    turn_context.session_source = SessionSource::Cli;
    Arc::make_mut(&mut turn_context.config).rencrow_compaction = true;
    Arc::make_mut(&mut turn_context.config).rencrow_autonomous_completion = true;
    let turn = Arc::new(turn_context);
    let _store = attach_boundary_owner(&mut session, Arc::clone(&turn)).await;
    let session = Arc::new(session);

    let initial_contract = json!({
        "contract_revision": 1,
        "conditions": [{
            "id": "AC-1", "revision": 1, "result": "Deliver AC-1", "scope": "AC-1",
            "verification": "Run AC-1 checks", "acceptance_criteria": "Checks pass",
            "status": "open"
        }],
        "units": [{
            "id": "task-a", "purpose": "Verify task A", "status": "running",
            "kind": "execution", "condition_refs": ["AC-1"], "depends_on": [], "task_refs": []
        }],
        "current_unit": "task-a",
        "next_action": "Keep the host verification decision visible"
    });
    let begin_args = json!({
        "action": "begin", "root_task_id": "root", "task_id": "task-a",
        "task_revision": 1, "completion_state": initial_contract
    });
    TaskContextBoundaryHandler::new(true)
        .handle(invocation_in_session(
            Arc::clone(&session),
            Arc::clone(&turn),
            "begin-diagnostics",
            begin_args,
        ))
        .await
        .expect("register completion contract and active task scope");
    record_call(
        &session,
        &turn,
        "begin-diagnostics",
        TASK_CONTEXT_BOUNDARY_TOOL_NAME,
    )
    .await;
    record_output(
        &session,
        &turn,
        "begin-diagnostics",
        TASK_CONTEXT_BOUNDARY_TOOL_NAME,
    )
    .await;

    let failed_workspace = tempfile::tempdir().expect("failed verification workspace");
    std::fs::write(
        failed_workspace.path().join("artifact.txt"),
        "failed attempt",
    )
    .unwrap();
    let failed_request = TaskEvidenceRequest {
        task_id: "task-a".into(),
        task_revision: 1,
        artifact_paths: vec![std::path::PathBuf::from("artifact.txt")],
        check_kind: TaskCheckKind::Verification,
    };
    let mut failed_evidence =
        TaskCheckExecution::new(failed_request.clone(), failed_workspace.path())
            .expect("capture failed attempt artifacts");
    failed_evidence.bind_obligation(
        session
            .active_task_scope()
            .await
            .expect("active task scope"),
        "go vet ./internal/portal",
    );
    session
        .begin_task_check_with_obligation(
            "failed-vet-call".into(),
            failed_request,
            failed_evidence.workspace_root.clone(),
            failed_evidence.before.clone(),
            failed_evidence.obligation.clone(),
        )
        .await;
    session
        .finish_task_check(
            "failed-vet-call",
            /*exit_code*/ 1,
            /*completed*/ true,
            failed_evidence.hash_artifacts().unwrap(),
        )
        .await
        .expect("record failed verification attempt");
    record_call(&session, &turn, "failed-vet-call", "exec_command").await;
    record_output(&session, &turn, "failed-vet-call", "exec_command").await;

    let latest_failure = TaskContextBoundaryHandler::new(true)
        .handle(invocation_in_session(
            Arc::clone(&session),
            Arc::clone(&turn),
            "accept-with-latest-failure",
            json!({"action": "accept", "result": "Task A completed"}),
        ))
        .await;
    let latest_failure = match latest_failure {
        Err(FunctionCallError::RespondToModel(message)) => message,
        Err(error) => panic!("expected actionable latest-check failure, got {error}"),
        Ok(_) => panic!("latest failed verification must block acceptance"),
    };
    assert!(
        latest_failure.contains("failed-vet-call"),
        "{latest_failure}"
    );
    assert!(latest_failure.contains("exit_code=1"), "{latest_failure}");
    assert!(
        latest_failure.contains("task_context_boundary status"),
        "{latest_failure}"
    );
    assert!(latest_failure.contains("observed_cwd"), "{latest_failure}");
    assert!(
        !latest_failure.contains("rerun each failed check"),
        "{latest_failure}"
    );

    let successful_workspace =
        tempfile::tempdir().expect("later successful verification workspace");
    std::fs::write(
        successful_workspace.path().join("artifact.txt"),
        "later successful attempt",
    )
    .unwrap();
    let successful_request = TaskEvidenceRequest {
        task_id: "task-a".into(),
        task_revision: 1,
        artifact_paths: vec![std::path::PathBuf::from("artifact.txt")],
        check_kind: TaskCheckKind::Verification,
    };
    let mut successful_evidence =
        TaskCheckExecution::new(successful_request.clone(), successful_workspace.path())
            .expect("capture later successful attempt artifacts");
    successful_evidence.bind_obligation(
        session
            .active_task_scope()
            .await
            .expect("active task scope"),
        "go vet ./internal/portal",
    );
    session
        .begin_task_check_with_obligation(
            "later-success-call".into(),
            successful_request,
            successful_evidence.workspace_root.clone(),
            successful_evidence.before.clone(),
            successful_evidence.obligation.clone(),
        )
        .await;
    session
        .finish_task_check(
            "later-success-call",
            /*exit_code*/ 0,
            /*completed*/ true,
            successful_evidence.hash_artifacts().unwrap(),
        )
        .await
        .expect("record later successful verification");
    record_call(&session, &turn, "later-success-call", "exec_command").await;
    record_output(&session, &turn, "later-success-call", "exec_command").await;

    let status_output = TaskContextBoundaryHandler::new(true)
        .handle(invocation_in_session(
            Arc::clone(&session),
            Arc::clone(&turn),
            "status-diagnostics",
            json!({"action": "status", "root_task_id": "root"}),
        ))
        .await
        .expect("status remains available with verification diagnostics");
    let status_value: serde_json::Value = serde_json::from_str(&status_output.log_output())
        .expect("status output remains parseable completion JSON");
    assert_eq!(status_value["verification_obligations"]["count"], 2);
    assert_eq!(status_value["verification_obligations"]["omitted"], 0);
    assert_eq!(
        status_value["verification_obligations"]["unresolved_count"],
        1
    );
    assert!(
        status_value["verification_obligations"]["attempts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|attempt| attempt["call_id"] == "failed-vet-call" && attempt["exit_code"] == 1)
    );
    assert!(
        status_value["verification_obligations"]["attempts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|attempt| attempt["call_id"] == "later-success-call"
                && attempt["succeeded"] == true)
    );
    let canonical_state = session
        .rencrow_completion_state("root")
        .await
        .expect("registered completion state remains available");
    assert_eq!(
        status_value["next_action"],
        serde_json::to_value(canonical_state.next_action).unwrap()
    );
    assert!(
        serde_json::from_value::<codex_history::task_context::CompletionState>(
            status_value.clone()
        )
        .is_err(),
        "host verification annotation must not become model-authored state"
    );

    let accept_error = TaskContextBoundaryHandler::new(true)
        .handle(invocation_in_session(
            Arc::clone(&session),
            Arc::clone(&turn),
            "accept-with-unresolved-check",
            json!({"action": "accept", "result": "Task A completed"}),
        ))
        .await;
    let accept_error = match accept_error {
        Err(error) => error,
        Ok(_) => panic!("unresolved prior verification must block acceptance"),
    };
    let FunctionCallError::RespondToModel(message) = accept_error else {
        panic!("accept should return an actionable model-facing verification error")
    };
    assert!(
        message.contains("task_context_boundary status"),
        "{message}"
    );
    assert!(message.contains("root_task_id root"), "{message}");
    assert!(
        message.contains("verification_obligations.attempts"),
        "{message}"
    );
    assert!(message.contains("observed_cwd"), "{message}");
    assert!(
        message.contains("diagnostic excerpt or omission/hash markers"),
        "{message}"
    );
    assert!(!message.contains("rerun each failed check"), "{message}");
}

#[tokio::test]
async fn accept_closes_a_blocked_tool_call_only_after_its_false_output_is_recorded() {
    let (mut session, mut turn_context) = make_session_and_context().await;
    turn_context.session_source = SessionSource::Cli;
    Arc::make_mut(&mut turn_context.config).rencrow_compaction = true;
    let turn = Arc::new(turn_context);
    attach_boundary_owner(&mut session, Arc::clone(&turn)).await;
    let session = Arc::new(session);
    begin_scope(&session, &turn, "task-a", 1, "begin-call").await;

    let workspace = tempfile::tempdir().expect("verification workspace");
    std::fs::write(workspace.path().join("artifact.txt"), "verified").unwrap();
    record_call(&session, &turn, "legacy-blocked-call", "exec_command").await;
    record_successful_verification(
        &session,
        &turn,
        workspace.path(),
        "task-a",
        1,
        "verification-call",
    )
    .await;

    let accept_args = json!({
        "action":"accept",
        "goal":"Finish task A",
        "tasks":[{"id":"task-a", "purpose":"Verify task A", "status":"completed"}],
        "current_task_id":null,
        "result":"Task A verification passed"
    });
    record_call_with_arguments(
        &session,
        &turn,
        "accept-with-unpaired-blocked-call",
        TASK_CONTEXT_BOUNDARY_TOOL_NAME,
        &accept_args.to_string(),
    )
    .await;
    let error = match TaskContextBoundaryHandler::new(false)
        .handle(invocation_in_session(
            Arc::clone(&session),
            Arc::clone(&turn),
            "accept-with-unpaired-blocked-call",
            accept_args.clone(),
        ))
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("accept must remain held while the blocked call has no result"),
    };
    assert!(
        error.to_string().contains("legacy-blocked-call"),
        "unexpected accept blocker: {error}"
    );
    record_output(
        &session,
        &turn,
        "accept-with-unpaired-blocked-call",
        TASK_CONTEXT_BOUNDARY_TOOL_NAME,
    )
    .await;

    session
        .record_annotated_conversation_items(
            &turn,
            turn.model_info().as_ref(),
            vec![ResponseItemEnvelope::new(
                ResponseItem::FunctionCallOutput {
                    id: None,
                    call_id: Some("legacy-blocked-call".to_owned()),
                    name: Some("exec_command".to_owned()),
                    namespace: None,
                    output: FunctionCallOutputPayload {
                        body: codex_protocol::models::FunctionCallOutputBody::Text(
                            "blocked before handler; command not executed".to_owned(),
                        ),
                        success: Some(false),
                    },
                    internal_chat_message_metadata_passthrough: None,
                },
            )],
        )
        .await;

    record_call_with_arguments(
        &session,
        &turn,
        "accept-after-blocked-result",
        TASK_CONTEXT_BOUNDARY_TOOL_NAME,
        &accept_args.to_string(),
    )
    .await;
    TaskContextBoundaryHandler::new(false)
        .handle(invocation_in_session(
            Arc::clone(&session),
            Arc::clone(&turn),
            "accept-after-blocked-result",
            accept_args,
        ))
        .await
        .expect("the exact false output closes the unresolved-call gate");
}

#[tokio::test]
async fn schema_parses_and_parent_begin_records_host_scope() {
    let handler = TaskContextBoundaryHandler::new(false);
    let schema = serde_json::to_value(handler.spec()).expect("schema serializes");
    assert_eq!(schema["name"], TASK_CONTEXT_BOUNDARY_TOOL_NAME);
    assert_eq!(
        schema["parameters"]["properties"]["action"]["enum"],
        json!(["begin", "accept"])
    );
    for legacy_field in [
        "goal",
        "tasks",
        "current_task_id",
        "decisions",
        "constraints",
    ] {
        assert!(
            schema["parameters"]["properties"]
                .get(legacy_field)
                .is_some(),
            "disabled schema preserves legacy field {legacy_field}"
        );
    }
    assert!(
        schema["parameters"]["properties"]
            .get("completion_state")
            .is_none()
    );
    assert!(
        schema["parameters"]["properties"]
            .get("previous_plan")
            .is_none()
    );
    for autonomous_field in ["target_task_id", "unit_id", "review_retry"] {
        assert!(
            schema["parameters"]["properties"]
                .get(autonomous_field)
                .is_none(),
            "disabled schema omits autonomous review field {autonomous_field}"
        );
    }
    let enabled_schema = serde_json::to_value(TaskContextBoundaryHandler::new(true).spec())
        .expect("enabled schema serializes");
    assert_eq!(
        enabled_schema["parameters"]["properties"]["action"]["enum"],
        json!([
            "begin",
            "update",
            "status",
            "plan_review",
            "accept",
            "review",
            "adopt_review"
        ])
    );
    assert!(
        enabled_schema["parameters"]["properties"]
            .get("completion_state")
            .is_some()
    );
    assert!(
        enabled_schema["parameters"]["properties"]
            .get("previous_plan")
            .is_some()
    );
    assert_eq!(
        enabled_schema["parameters"]["properties"]["completion_state"]["properties"]["units"]["items"]
            ["properties"]["review_revalidation_of"]["type"],
        "string",
        "a planned review may explicitly name the exact adopted review it revalidates"
    );
    for accepted_field in [
        "root_task_id",
        "task_id",
        "task_revision",
        "result",
        "target_task_id",
        "unit_id",
        "review_retry",
    ] {
        assert!(
            enabled_schema["parameters"]["properties"]
                .get(accepted_field)
                .is_some(),
            "enabled schema retains accepted field {accepted_field}"
        );
    }
    for rejected_legacy_field in [
        "goal",
        "tasks",
        "current_task_id",
        "decisions",
        "constraints",
    ] {
        assert!(
            enabled_schema["parameters"]["properties"]
                .get(rejected_legacy_field)
                .is_none(),
            "enabled schema must not advertise rejected legacy field {rejected_legacy_field}"
        );
    }
    let conditions = &enabled_schema["parameters"]["properties"]["completion_state"]["properties"]
        ["conditions"]["items"]["properties"];
    assert!(
        conditions["result"]["description"]
            .as_str()
            .is_some_and(|description| description.contains("user's requested result"))
    );
    assert!(conditions["scope"]["description"].as_str().is_some());
    assert!(conditions["verification"]["description"].as_str().is_some());
    assert!(
        conditions["acceptance_criteria"]["description"]
            .as_str()
            .is_some()
    );
    let invocation = invocation(SessionSource::Cli, "begin-1", begin("root", "task-a", 1)).await;
    let session = Arc::clone(&invocation.session);
    handler
        .handle(invocation)
        .await
        .expect("parent begin succeeds");
    let scope = session.active_task_scope().await.expect("scope is stored");
    assert_eq!(scope.root_task_id, "root");
    assert_eq!(scope.task_id, "task-a");
    assert_eq!(scope.task_revision, 1);
    assert_eq!(scope.begin_call_id, "begin-1");
}

#[test]
fn enabled_schema_action_shapes_match_argument_parser() {
    let schema = serde_json::to_value(TaskContextBoundaryHandler::new(true).spec())
        .expect("enabled schema serializes");
    let one_of = schema["parameters"]["oneOf"]
        .as_array()
        .expect("enabled schema has action-specific oneOf");
    let expected: [(&str, &[&str], &[&str]); 8] = [
        (
            "begin",
            &[
                "action",
                "root_task_id",
                "task_id",
                "task_revision",
                "common_harness_request",
            ],
            &[
                "action",
                "root_task_id",
                "task_id",
                "task_revision",
                "common_harness_request",
            ],
        ),
        (
            "begin",
            &[
                "action",
                "root_task_id",
                "task_id",
                "task_revision",
                "completion_state",
            ],
            &["action", "root_task_id", "task_id", "task_revision"],
        ),
        (
            "update",
            &["action", "root_task_id", "completion_state"],
            &["action", "root_task_id", "completion_state"],
        ),
        ("status", &["action", "root_task_id"], &["action"]),
        (
            "plan_review",
            &[
                "action",
                "root_task_id",
                "previous_plan",
                "revised_plan",
                "reason",
                "review_retry",
            ],
            &[
                "action",
                "root_task_id",
                "previous_plan",
                "revised_plan",
                "reason",
            ],
        ),
        (
            "accept",
            &["action", "result", "satisfied_conditions"],
            &["action", "result"],
        ),
        (
            "review",
            &["action", "target_task_id", "unit_id"],
            &["action", "target_task_id"],
        ),
        (
            "adopt_review",
            &["action", "unit_id", "reason"],
            &["action", "unit_id", "reason"],
        ),
    ];
    assert_eq!(one_of.len(), expected.len());
    for (branch, (action, allowed, required)) in one_of.iter().zip(expected) {
        assert_eq!(branch["properties"]["action"]["enum"], json!([action]));
        let mut actual_allowed = branch["properties"]
            .as_object()
            .expect("branch properties")
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();
        actual_allowed.sort_unstable();
        let mut expected_allowed = allowed.to_vec();
        expected_allowed.sort_unstable();
        assert_eq!(actual_allowed, expected_allowed);
        let mut actual_required = branch["required"]
            .as_array()
            .expect("branch required")
            .iter()
            .map(|name| name.as_str().expect("required name"))
            .collect::<Vec<_>>();
        actual_required.sort_unstable();
        let mut expected_required = required.to_vec();
        expected_required.sort_unstable();
        assert_eq!(actual_required, expected_required);
        assert_eq!(branch["additionalProperties"], json!(false));
    }

    let valid = [
        json!({"action":"begin","root_task_id":"root","task_id":"unit","task_revision":1,"completion_state":{}}),
        json!({"action":"update","root_task_id":"root","completion_state":{}}),
        json!({"action":"update","root_task_id":"root","completion_state":{"contract_revision":3,"conditions":[],"units":[{"id":"review-a-current","purpose":"Revalidate accepted task bytes","status":"planned","kind":"review","condition_refs":["AC-1"],"depends_on":["task-a"],"review_target":{"task_id":"task-a","task_revision":1},"review_revalidation_of":"review-a"}]}}),
        json!({"action":"status"}),
        json!({"action":"plan_review","root_task_id":"root","previous_plan":"a","revised_plan":"b","reason":"new evidence"}),
        json!({"action":"plan_review","root_task_id":"root","previous_plan":"a","revised_plan":"b","reason":"failed attempt has a new authorized mitigation","review_retry":{"unit_id":"review-a","expected_attempt_id":"spawn-call-a"}}),
        json!({"action":"accept","result":"done","satisfied_conditions":["AC-1"]}),
        json!({"action":"review","target_task_id":"task-a","unit_id":"review-a"}),
    ];
    for input in valid {
        serde_json::from_value::<TaskContextBoundaryArgs>(input)
            .expect("valid action shape parses");
    }
    let TaskContextBoundaryArgs::Review {
        target_task_id,
        unit_id,
    } = serde_json::from_value(json!({
        "action":"review",
        "target_task_id":"task-a",
        "unit_id":"review-a"
    }))
    .expect("review parses an explicit obligation selector")
    else {
        panic!("review arguments parse as review action");
    };
    assert_eq!(target_task_id, "task-a");
    assert_eq!(unit_id.as_deref(), Some("review-a"));

    for invalid in [
        json!({"action":"begin","root_task_id":"root","task_id":"unit","task_revision":1,"goal":"extra"}),
        json!({"action":"accept","goal":"goal","tasks":[],"result":"done","completion_state":{}}),
        json!({"action":"update","root_task_id":"root","completion_state":{},"goal":"extra"}),
    ] {
        assert!(serde_json::from_value::<TaskContextBoundaryArgs>(invalid).is_err());
    }
}

#[test]
fn common_harness_begin_schema_advertises_host_bound_go_verifier_inputs() {
    let schema = serde_json::to_value(TaskContextBoundaryHandler::new(true).spec())
        .expect("enabled schema serializes");
    let request = &schema["parameters"]["properties"]["common_harness_request"];
    assert_eq!(
        request["properties"]["contract_version"]["enum"],
        json!(["common-harness/1.0"])
    );
    let request_required = request["required"]
        .as_array()
        .expect("common harness required fields")
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect::<Vec<_>>();
    for host_owned in [
        "authorized_request_ref",
        "workspace_ref",
        "start_target_version",
        "permission_snapshot_ref",
        "operation_limits_ref",
    ] {
        assert!(
            !request_required.contains(&host_owned),
            "Begin must not require model-invented host reference {host_owned}"
        );
    }
    let condition = &request["properties"]["conditions"]["items"];
    assert_eq!(
        condition["properties"]["verifier_ref"]["enum"],
        json!(["codex-common-harness/go-test-json/1"])
    );
    assert_eq!(
        condition["properties"]["verifier_config_revision"]["type"],
        "number"
    );
    assert!(
        condition["properties"]["verifier_config_revision"]["description"]
            .as_str()
            .is_some_and(|description| description.contains('1'))
    );
    let check = &condition["properties"]["check"];
    assert_eq!(
        check["properties"]["parser"]["enum"],
        json!(["go_test_json"])
    );
    let selection = &check["properties"]["go_test_selection"];
    assert_eq!(selection["type"], "object");
    assert_eq!(
        selection["required"],
        json!(["selected_cases", "expected_case_count"])
    );
    assert_eq!(selection["properties"]["selected_cases"]["type"], "array");
    assert_eq!(
        selection["properties"]["expected_case_count"]["type"],
        "number"
    );
    let outcomes = check["properties"]["expected_outcome"]["oneOf"]
        .as_array()
        .expect("supported typed Go outcomes are advertised");
    let outcome_tags = outcomes
        .iter()
        .flat_map(|variant| variant["properties"].as_object().into_iter())
        .flat_map(|properties| properties.keys().cloned())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        outcome_tags,
        [
            "all_selected_tests_pass",
            "failure_identity",
            "specified_test_failure",
            "test_counts",
        ]
        .into_iter()
        .map(str::to_string)
        .collect()
    );
    let skills = &request["properties"]["skill_refs"]["items"];
    assert_eq!(skills["required"], json!(["id"]));
    assert_eq!(skills["properties"]["id"]["type"], "string");
}

#[test]
fn autonomous_accept_builds_host_task_board_and_only_adopts_selected_conditions() {
    let (tasks, current) = host_task_board("root", "unit-a", "AC-1: delivered", "Complete A", None);
    assert_eq!(current.as_deref(), Some("root"));
    assert_eq!(tasks[0].id, "root");
    assert_eq!(tasks[0].status, TaskStatus::InProgress);
    assert_eq!(tasks[1].id, "unit-a");
    assert_eq!(tasks[1].purpose, "Complete A");
    assert_eq!(tasks[1].status, TaskStatus::Completed);

    let mut state: codex_history::task_context::CompletionState = serde_json::from_value(json!({
        "contract_revision":1,
        "conditions":[
            {"id":"AC-1","revision":1,"result":"A is complete","scope":"A","verification":"check A","acceptance_criteria":"A passes","status":"satisfied","evidence":[{"task_id":"task-a","task_revision":1,"artifact_ref":"old-a","verification_ref":"old-a"}]},
            {"id":"AC-2","revision":1,"result":"B is complete","scope":"B","verification":"check B","acceptance_criteria":"B passes","status":"open"}
        ],
        "units":[
            {"id":"unit-a","purpose":"Complete A","status":"done","kind":"execution","condition_refs":["AC-1"],"evidence":[{"task_id":"task-a","task_revision":1,"artifact_ref":"old-a","verification_ref":"old-a"}]},
            {"id":"unit-b","purpose":"Complete B","status":"ready","kind":"execution","condition_refs":["AC-2"]}
        ],
        "current_unit":"unit-b"
    })).expect("typed completion contract");
    apply_host_acceptance(
        &mut state,
        "task-b",
        2,
        "task-check:check-b:sha256",
        &["AC-1".to_string(), "AC-2".to_string()],
        true,
    )
    .expect("parent may adopt an existing condition from a refreshed independent unit receipt");
    assert_eq!(state.units[1].status, UnitStatus::Done);
    assert!(state.units[1].condition_refs.contains(&"AC-1".to_string()));
    assert_eq!(
        state.conditions[0].status,
        codex_history::task_context::AcceptanceStatus::Satisfied
    );
    assert_eq!(
        state.conditions[1].status,
        codex_history::task_context::AcceptanceStatus::Satisfied
    );
    assert_eq!(state.conditions[0].evidence[0].task_revision, 2);
    assert_eq!(state.units[0].evidence[0].task_revision, 1);

    let error = apply_host_acceptance(
        &mut state,
        "task-b",
        2,
        "task-check:check-b:sha256",
        &["AC-3".to_string()],
        true,
    )
    .expect_err("unknown condition IDs are rejected");
    assert!(error.contains("unknown acceptance condition"));
}

#[test]
fn autonomous_accept_resolves_execution_from_exact_planned_review_dependency() {
    let mut state: codex_history::task_context::CompletionState = serde_json::from_value(json!({
        "contract_revision": 2,
        "conditions": [{
            "id": "AC-1", "revision": 1, "result": "Deliver AC-1", "scope": "AC-1",
            "verification": "Run the AC-1 checks", "acceptance_criteria": "Checks pass",
            "status": "open"
        }],
        "units": [
            {"id":"execAC1", "purpose":"Implement AC-1", "status":"running",
             "kind":"execution", "condition_refs":["AC-1"], "depends_on":[], "task_refs":[]},
            {"id":"reviewAC1", "purpose":"Review AC-1", "status":"planned",
             "kind":"review", "condition_refs":[], "depends_on":["execAC1"],
             "review_target":{"task_id":"sp-newAC1", "task_revision":1}}
        ],
        "current_unit": null
    }))
    .expect("actual native AC1 projection shape is a valid completion contract");
    let planned_review = state.units[1].clone();
    let original_conditions = state.conditions.clone();
    let initial_state = state.clone();

    apply_host_acceptance(
        &mut state,
        "sp-newAC1",
        1,
        "task-check:call-8b:artifact-fingerprint",
        &["AC-1".to_string()],
        true,
    )
    .expect("exact planned review dependency identifies the execution unit");

    assert_eq!(state.units[0].status, UnitStatus::Done);
    assert_eq!(state.units[0].condition_refs, vec!["AC-1".to_string()]);
    assert_eq!(state.units[0].evidence[0].task_id, "sp-newAC1");
    assert_eq!(
        state.conditions[0].status,
        codex_history::task_context::AcceptanceStatus::Satisfied
    );
    assert_eq!(state.conditions[0].evidence[0].task_revision, 1);
    assert_eq!(state.units[1], planned_review);
    assert_eq!(state.current_unit, None);

    let mut planned_execution = initial_state.clone();
    planned_execution.units[0].status = UnitStatus::Planned;
    apply_host_acceptance(
        &mut planned_execution,
        "sp-newAC1",
        1,
        "task-check:call-8b:artifact-fingerprint",
        &["AC-1".to_string()],
        true,
    )
    .expect("planned execution dependency can be accepted");
    assert_eq!(planned_execution.units[0].status, UnitStatus::Done);

    let mut no_condition_selection = initial_state.clone();
    apply_host_acceptance(
        &mut no_condition_selection,
        "sp-newAC1",
        1,
        "task-check:call-8b:artifact-fingerprint",
        &[],
        true,
    )
    .expect("accept without selected conditions still records the unit receipt");
    assert_eq!(no_condition_selection.conditions, original_conditions);

    let mut selected_review = initial_state;
    selected_review.current_unit = Some("reviewAC1".to_string());
    apply_host_acceptance(
        &mut selected_review,
        "sp-newAC1",
        1,
        "task-check:call-8b:artifact-fingerprint",
        &["AC-1".to_string()],
        true,
    )
    .expect("current unit may select the exact planned review");
    assert_eq!(selected_review.current_unit.as_deref(), Some("reviewAC1"));
}

#[test]
fn autonomous_accept_rejects_ambiguous_or_invalid_review_dependencies() {
    use codex_history::task_context::CompletionState;

    let base_state = || -> CompletionState {
        serde_json::from_value(json!({
            "contract_revision": 2,
            "conditions": [
                {"id":"AC-1", "revision":1, "result":"Deliver AC-1", "scope":"AC-1",
                 "verification":"Run checks", "acceptance_criteria":"Checks pass", "status":"open"},
                {"id":"AC-2", "revision":1, "result":"Deliver AC-2", "scope":"AC-2",
                 "verification":"Run checks", "acceptance_criteria":"Checks pass", "status":"open"}
            ],
            "units": [
                {"id":"execAC1", "purpose":"Implement AC-1", "status":"running",
                 "kind":"execution", "condition_refs":["AC-1"], "depends_on":[], "task_refs":[]},
                {"id":"reviewAC1", "purpose":"Review AC-1", "status":"planned",
                 "kind":"review", "condition_refs":[], "depends_on":["execAC1"],
                 "review_target":{"task_id":"sp-newAC1", "task_revision":1}}
            ],
            "current_unit": null
        }))
        .expect("typed review dependency fixture")
    };
    let mut cases: Vec<(&str, CompletionState, Vec<String>)> = Vec::new();

    let mut foreign_target = base_state();
    foreign_target.units[1]
        .review_target
        .as_mut()
        .unwrap()
        .task_id = "other-task".into();
    cases.push(("foreign target", foreign_target, vec!["AC-1".into()]));

    let mut wrong_revision = base_state();
    wrong_revision.units[1]
        .review_target
        .as_mut()
        .unwrap()
        .task_revision = 2;
    cases.push(("wrong revision", wrong_revision, vec!["AC-1".into()]));

    let mut ambiguous = base_state();
    let mut second_review = ambiguous.units[1].clone();
    second_review.id = "reviewAC1-also".into();
    ambiguous.units.push(second_review);
    cases.push(("ambiguous review", ambiguous, vec!["AC-1".into()]));

    let mut missing_dependency = base_state();
    missing_dependency.units[1].depends_on.clear();
    cases.push((
        "missing dependency",
        missing_dependency,
        vec!["AC-1".into()],
    ));

    let mut multiple_dependencies = base_state();
    multiple_dependencies.units[1].depends_on = vec!["execAC1".into(), "execAC1".into()];
    cases.push((
        "multiple dependencies",
        multiple_dependencies,
        vec!["AC-1".into()],
    ));

    let mut review_dependency = base_state();
    review_dependency.units[1].depends_on = vec!["reviewAC1".into()];
    cases.push((
        "non-execution dependency",
        review_dependency,
        vec!["AC-1".into()],
    ));

    for status in [UnitStatus::Done, UnitStatus::Failed, UnitStatus::Blocked] {
        let mut invalid_status = base_state();
        invalid_status.units[0].status = status;
        cases.push((
            "unavailable execution status",
            invalid_status,
            vec!["AC-1".into()],
        ));
    }

    let mut nonplanned_review = base_state();
    nonplanned_review.units[1].status = UnitStatus::Ready;
    cases.push(("non-planned review", nonplanned_review, vec!["AC-1".into()]));

    cases.push((
        "condition outside execution unit",
        base_state(),
        vec!["AC-2".into()],
    ));

    let mut other_current_unit = base_state();
    let mut other_review = other_current_unit.units[1].clone();
    other_review.id = "review-other".into();
    other_review.review_target.as_mut().unwrap().task_id = "other-task".into();
    other_current_unit.units.push(other_review);
    other_current_unit.current_unit = Some("review-other".into());
    cases.push((
        "other current review",
        other_current_unit,
        vec!["AC-1".into()],
    ));

    for (name, mut state, selected_conditions) in cases {
        let error = apply_host_acceptance(
            &mut state,
            "sp-newAC1",
            1,
            "task-check:call-8b:artifact-fingerprint",
            &selected_conditions,
            true,
        )
        .expect_err(name);
        assert!(!error.is_empty(), "{name} must be rejected with a reason");
    }

    let mut root_scope = base_state();
    assert!(
        apply_host_acceptance(
            &mut root_scope,
            "sp-newAC1",
            1,
            "task-check:call-8b:artifact-fingerprint",
            &["AC-1".to_string()],
            false,
        )
        .is_err(),
        "root acceptance cannot infer its execution unit from a review plan"
    );
}

#[tokio::test]
async fn tool_is_registered_only_for_parent_sources_when_compaction_is_enabled() {
    for (source, expected) in [
        (SessionSource::Cli, true),
        (
            SessionSource::SubAgent(codex_protocol::protocol::SubAgentSource::Review),
            false,
        ),
    ] {
        let (session, mut turn) = make_session_and_context().await;
        turn.session_source = source;
        Arc::make_mut(&mut turn.config).rencrow_compaction = true;
        let turn = Arc::new(turn);
        let step = StepContext::for_test(Arc::clone(&turn));
        let router = crate::tools::spec_plan::build_tool_router(
            &session,
            step.turn.as_ref(),
            &step.settings.model_info,
            &step.environments,
            &step.mcp,
            /*apps_enabled*/ false,
            &turn.extension_data,
            /*tool_suggest_candidates*/ None,
            /*common_harness_tool_profile*/ None,
        )
        .expect("build tool router");
        assert_eq!(
            router
                .model_visible_specs()
                .iter()
                .any(|spec| spec.name() == TASK_CONTEXT_BOUNDARY_TOOL_NAME),
            expected
        );
    }
}

#[tokio::test]
async fn subagent_begin_is_rejected() {
    let invocation = invocation(
        SessionSource::SubAgent(codex_protocol::protocol::SubAgentSource::Review),
        "begin-child",
        begin("root", "task-a", 1),
    )
    .await;
    let session = Arc::clone(&invocation.session);
    let error = match TaskContextBoundaryHandler::new(false)
        .handle(invocation)
        .await
    {
        Ok(_) => panic!("subagent is rejected"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("authorized parent"));
    assert!(session.active_task_scope().await.is_none());
}

#[tokio::test]
async fn unfinished_root_execution_scope_can_be_rebound_to_child_and_replay_is_idempotent() {
    let handler = TaskContextBoundaryHandler::new(false);
    let first = invocation_with_autonomous_completion(
        SessionSource::VSCode,
        "begin-1",
        begin("root", "root", 1),
        true,
    )
    .await;
    let session = Arc::clone(&first.session);
    handler.handle(first).await.expect("first begin succeeds");

    let mut replay = invocation(
        SessionSource::Exec,
        "begin-replay",
        begin("root", "root", 1),
    )
    .await;
    replay.session = Arc::clone(&session);
    handler
        .handle(replay)
        .await
        .expect("same task replay is idempotent");
    assert_eq!(
        session.active_task_scope().await.unwrap().begin_call_id,
        "begin-1"
    );

    let mut other = invocation_with_autonomous_completion(
        SessionSource::Cli,
        "begin-2",
        begin("root", "task-b", 1),
        true,
    )
    .await;
    other.session = Arc::clone(&session);
    handler
        .handle(other)
        .await
        .expect("same-root child scope can begin while root remains unfinished");
    let active = session.active_task_scope().await.unwrap();
    assert_eq!(active.root_task_id, "root");
    assert_eq!(active.task_id, "task-b");
    assert_eq!(active.begin_call_id, "begin-2");
}

#[tokio::test]
async fn rejected_completion_contract_begin_keeps_scope_contract_and_check_ledger_unchanged() {
    let handler = TaskContextBoundaryHandler::new(true);
    let contract = json!({
        "contract_revision":1,
        "conditions":[{
            "id":"AC-1", "revision":1, "result":"Output X", "scope":"CLI",
            "verification":"Run check X", "acceptance_criteria":"exit 0",
            "status":"open"
        }],
        "units":[], "current_unit":null, "next_action":"Work on AC-1"
    });
    let first = invocation_with_autonomous_completion(
        SessionSource::Cli,
        "begin-root",
        json!({
            "action":"begin", "root_task_id":"root", "task_id":"root",
            "task_revision":1, "completion_state":contract
        }),
        true,
    )
    .await;
    let session = Arc::clone(&first.session);
    handler
        .handle(first)
        .await
        .expect("initial contract begins");
    let scope_before = session.active_task_scope().await.expect("initial scope");
    let contract_before = session
        .rencrow_completion_state("root")
        .await
        .expect("initial contract");
    let checks_before = session.task_check_snapshot().await;

    let mut invalid = invocation_with_autonomous_completion(
        SessionSource::Cli,
        "begin-child-invalid-contract",
        json!({
            "action":"begin", "root_task_id":"root", "task_id":"task-a",
            "task_revision":1,
            "completion_state":{
                "contract_revision":2,
                "conditions":[{
                    "id":"AC-1", "revision":2, "result":"Weakened output", "scope":"CLI",
                    "verification":"Run check X", "acceptance_criteria":"exit 0",
                    "status":"open"
                }],
                "units":[], "current_unit":null, "next_action":"Work on AC-1"
            }
        }),
        true,
    )
    .await;
    invalid.session = Arc::clone(&session);
    let error = handler
        .handle(invalid)
        .await
        .err()
        .expect("condition definition change is rejected");
    assert!(error.to_string().contains("redefined"));
    assert_eq!(session.active_task_scope().await, Some(scope_before));
    assert_eq!(
        session.rencrow_completion_state("root").await,
        Some(contract_before)
    );
    assert_eq!(
        serde_json::to_value(session.task_check_snapshot().await).expect("serialize checks after"),
        serde_json::to_value(checks_before).expect("serialize checks before")
    );
}

async fn record_call(
    session: &crate::session::session::Session,
    turn: &crate::session::turn_context::TurnContext,
    call_id: &str,
    name: &str,
) {
    record_call_with_arguments(session, turn, call_id, name, "{}").await;
}

async fn record_call_with_arguments(
    session: &crate::session::session::Session,
    turn: &crate::session::turn_context::TurnContext,
    call_id: &str,
    name: &str,
    arguments: &str,
) {
    session
        .record_annotated_conversation_items(
            turn,
            turn.model_info().as_ref(),
            vec![ResponseItemEnvelope::new(ResponseItem::FunctionCall {
                id: None,
                name: name.to_string(),
                namespace: None,
                arguments: arguments.to_string(),
                encrypted_function_args: None,
                call_id: call_id.to_string(),
                internal_chat_message_metadata_passthrough: None,
            })],
        )
        .await;
}

async fn record_output(
    session: &crate::session::session::Session,
    turn: &crate::session::turn_context::TurnContext,
    call_id: &str,
    name: &str,
) {
    session
        .record_annotated_conversation_items(
            turn,
            turn.model_info().as_ref(),
            vec![ResponseItemEnvelope::new(
                ResponseItem::FunctionCallOutput {
                    id: None,
                    call_id: Some(call_id.to_string()),
                    name: Some(name.to_string()),
                    namespace: None,
                    output: FunctionCallOutputPayload::from_text("ok".into()),
                    internal_chat_message_metadata_passthrough: None,
                },
            )],
        )
        .await;
}

async fn begin_scope(
    session: &Arc<Session>,
    turn: &Arc<TurnContext>,
    task_id: &str,
    task_revision: u64,
    call_id: &str,
) {
    TaskContextBoundaryHandler::new(false)
        .handle(invocation_in_session(
            Arc::clone(session),
            Arc::clone(turn),
            call_id,
            begin("root", task_id, task_revision),
        ))
        .await
        .expect("begin task scope");
    record_call(session, turn, call_id, TASK_CONTEXT_BOUNDARY_TOOL_NAME).await;
    record_output(session, turn, call_id, TASK_CONTEXT_BOUNDARY_TOOL_NAME).await;
}

async fn record_successful_verification(
    session: &Arc<Session>,
    turn: &Arc<TurnContext>,
    workspace: &std::path::Path,
    task_id: &str,
    task_revision: u64,
    call_id: &str,
) {
    let request = TaskEvidenceRequest {
        task_id: task_id.into(),
        task_revision,
        artifact_paths: vec![std::path::PathBuf::from("artifact.txt")],
        check_kind: TaskCheckKind::Verification,
    };
    let evidence = TaskCheckExecution::new(request.clone(), workspace).unwrap();
    session
        .begin_task_check(
            call_id.into(),
            request,
            evidence.workspace_root.clone(),
            evidence.before.clone(),
        )
        .await;
    session
        .finish_task_check(
            call_id,
            /*exit_code*/ 0,
            /*completed*/ true,
            evidence.hash_artifacts().unwrap(),
        )
        .await
        .expect("verification succeeds");
    record_call(session, turn, call_id, "exec_command").await;
    record_output(session, turn, call_id, "exec_command").await;
}

#[tokio::test]
async fn autonomous_accept_with_prior_same_revision_receipt_persists_fresh_host_evidence() {
    let (mut session, mut turn_context) = make_session_and_context().await;
    turn_context.session_source = SessionSource::Cli;
    Arc::make_mut(&mut turn_context.config).rencrow_compaction = true;
    Arc::make_mut(&mut turn_context.config).rencrow_autonomous_completion = true;
    let turn = Arc::new(turn_context);
    let store = attach_boundary_owner(&mut session, Arc::clone(&turn)).await;
    let session = Arc::new(session);
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("artifact.txt"), "unchanged artifact").unwrap();

    let initial_contract = json!({
        "contract_revision":1,
        "conditions":[
            {"id":"AC-1", "revision":1, "result":"Deliver AC-1", "scope":"AC-1",
             "verification":"Run AC-1 checks", "acceptance_criteria":"Checks pass", "status":"open"},
            {"id":"AC-2", "revision":1, "result":"Deliver AC-2", "scope":"AC-2",
             "verification":"Run AC-2 checks", "acceptance_criteria":"Checks pass", "status":"open"}
        ],
        "units":[
            {"id":"oldExecAC3", "purpose":"Supplemental AC-1 work", "status":"running",
             "kind":"execution", "condition_refs":["AC-1"], "depends_on":[], "task_refs":[]},
            {"id":"newExecAC1", "purpose":"Current AC-1 implementation", "status":"running",
             "kind":"execution", "condition_refs":["AC-1"], "depends_on":[], "task_refs":[]},
            {"id":"reviewAC1", "purpose":"Review AC-1", "status":"planned",
             "kind":"review", "condition_refs":["AC-1"], "depends_on":["newExecAC1"],
             "review_target":{"task_id":"sp-newAC1", "task_revision":1}}
        ],
        "current_unit":"oldExecAC3"
    });
    TaskContextBoundaryHandler::new(true)
        .handle(invocation_in_session(
            Arc::clone(&session),
            Arc::clone(&turn),
            "begin-old-scope",
            json!({
                "action":"begin", "root_task_id":"root", "task_id":"sp-newAC1",
                "task_revision":1, "completion_state":initial_contract
            }),
        ))
        .await
        .expect("begin the current task scope with prior supplemental execution selected");
    record_call(
        &session,
        &turn,
        "begin-old-scope",
        TASK_CONTEXT_BOUNDARY_TOOL_NAME,
    )
    .await;
    record_output(
        &session,
        &turn,
        "begin-old-scope",
        TASK_CONTEXT_BOUNDARY_TOOL_NAME,
    )
    .await;
    record_successful_verification(
        &session,
        &turn,
        workspace.path(),
        "sp-newAC1",
        1,
        "verify-old",
    )
    .await;

    let first_accept = json!({
        "action":"accept", "result":"Supplemental AC-1 evidence accepted",
        "satisfied_conditions":["AC-1"]
    });
    record_call_with_arguments(
        &session,
        &turn,
        "accept-old",
        TASK_CONTEXT_BOUNDARY_TOOL_NAME,
        &first_accept.to_string(),
    )
    .await;
    TaskContextBoundaryHandler::new(true)
        .handle(invocation_in_session(
            Arc::clone(&session),
            Arc::clone(&turn),
            "accept-old",
            first_accept,
        ))
        .await
        .expect("first acceptance records AC-1 on the selected old execution unit");
    record_output(
        &session,
        &turn,
        "accept-old",
        TASK_CONTEXT_BOUNDARY_TOOL_NAME,
    )
    .await;
    assert!(
        session
            .commit_prepared_task_boundary(&turn, &tokio_util::sync::CancellationToken::new())
            .await
            .expect("commit the prior accepted receipt")
    );

    let old_receipt = session
        .latest_rencrow_task_state()
        .await
        .expect("prior task state parses")
        .expect("prior task state exists")
        .completed
        .get("sp-newAC1")
        .cloned()
        .expect("prior same-scope accepted receipt exists");
    assert_eq!(old_receipt.revision, 1);
    let after_first = session
        .rencrow_completion_state("root")
        .await
        .expect("completion state after first accept");
    assert_eq!(after_first.units[0].status, UnitStatus::Done);
    assert_eq!(after_first.units[1].status, UnitStatus::Running);
    assert_eq!(after_first.units[2].status, UnitStatus::Planned);
    assert_eq!(after_first.current_unit.as_deref(), Some("oldExecAC3"));

    let mut cleared_unit = serde_json::to_value(after_first).expect("serialize completion state");
    cleared_unit["current_unit"] = serde_json::Value::Null;
    let clear_args =
        json!({"action":"update", "root_task_id":"root", "completion_state":cleared_unit});
    record_call_with_arguments(
        &session,
        &turn,
        "clear-current-unit",
        TASK_CONTEXT_BOUNDARY_TOOL_NAME,
        &clear_args.to_string(),
    )
    .await;
    TaskContextBoundaryHandler::new(true)
        .handle(invocation_in_session(
            Arc::clone(&session),
            Arc::clone(&turn),
            "clear-current-unit",
            clear_args,
        ))
        .await
        .expect("clear current unit while retaining the accepted receipt");
    record_output(
        &session,
        &turn,
        "clear-current-unit",
        TASK_CONTEXT_BOUNDARY_TOOL_NAME,
    )
    .await;
    let before_fresh = session
        .rencrow_completion_state("root")
        .await
        .expect("updated completion state");
    assert_eq!(before_fresh.current_unit, None);
    assert_eq!(
        before_fresh.conditions[0].evidence[0].artifact_ref,
        old_receipt.artifact_ref
    );

    TaskContextBoundaryHandler::new(true)
        .handle(invocation_in_session(
            Arc::clone(&session),
            Arc::clone(&turn),
            "begin-current-scope",
            begin("root", "sp-newAC1", 1),
        ))
        .await
        .expect("reopen the same task scope and revision");
    record_call(
        &session,
        &turn,
        "begin-current-scope",
        TASK_CONTEXT_BOUNDARY_TOOL_NAME,
    )
    .await;
    record_output(
        &session,
        &turn,
        "begin-current-scope",
        TASK_CONTEXT_BOUNDARY_TOOL_NAME,
    )
    .await;
    record_successful_verification(
        &session,
        &turn,
        workspace.path(),
        "sp-newAC1",
        1,
        "verify-current",
    )
    .await;

    let second_accept = json!({
        "action":"accept", "result":"Current AC-1 verification accepted",
        "satisfied_conditions":["AC-1"]
    });
    record_call_with_arguments(
        &session,
        &turn,
        "accept-current",
        TASK_CONTEXT_BOUNDARY_TOOL_NAME,
        &second_accept.to_string(),
    )
    .await;
    TaskContextBoundaryHandler::new(true)
        .handle(invocation_in_session(
            Arc::clone(&session),
            Arc::clone(&turn),
            "accept-current",
            second_accept,
        ))
        .await
        .expect("planned review dependency maps the fresh acceptance to new execution unit");
    record_output(
        &session,
        &turn,
        "accept-current",
        TASK_CONTEXT_BOUNDARY_TOOL_NAME,
    )
    .await;
    assert!(
        session
            .commit_prepared_task_boundary(&turn, &tokio_util::sync::CancellationToken::new())
            .await
            .expect("commit fresh same-scope receipt and completion state")
    );

    let current_state = session
        .latest_rencrow_task_state()
        .await
        .expect("current task state parses")
        .expect("current task state exists");
    let current_receipt = current_state
        .completed
        .get("sp-newAC1")
        .cloned()
        .expect("current accepted receipt exists");
    assert_ne!(current_receipt, old_receipt);
    assert_eq!(current_receipt.revision, old_receipt.revision);
    assert_eq!(
        current_state.accepted_history,
        vec![codex_history::task_context::AcceptedTaskEvidenceReceipt {
            task_id: "sp-newAC1".into(),
            task_revision: old_receipt.revision,
            artifact_ref: old_receipt.artifact_ref.clone(),
            verification_ref: old_receipt.verification_ref.clone(),
        }]
    );
    let reloaded = codex_history::task_context::StoredTaskState::parse_and_validate(
        serde_json::to_value(&current_state).expect("serialize revision history checkpoint"),
        &session.thread_id().to_string(),
    )
    .expect("exact accepted revision history survives checkpoint reload");
    assert_eq!(reloaded.accepted_history, current_state.accepted_history);
    let completion = session
        .rencrow_completion_state("root")
        .await
        .expect("completion state remains registered");
    let condition = completion
        .conditions
        .iter()
        .find(|condition| condition.id == "AC-1")
        .expect("accepted condition remains present");
    assert_eq!(condition.evidence.len(), 1);
    assert_eq!(condition.evidence[0].task_id, "sp-newAC1");
    assert_eq!(condition.evidence[0].task_revision, 1);
    assert_eq!(
        condition.evidence[0].artifact_ref,
        current_receipt.artifact_ref
    );
    assert_eq!(
        condition.evidence[0].verification_ref,
        current_receipt.verification_ref
    );
    assert_ne!(condition.evidence[0].artifact_ref, old_receipt.artifact_ref);
    assert_eq!(completion.units[0].status, UnitStatus::Done);
    assert_eq!(completion.units[0].evidence[0].task_id, "sp-newAC1");
    assert_eq!(completion.units[0].evidence[0].task_revision, 1);
    assert_eq!(
        completion.units[0].evidence[0].artifact_ref,
        old_receipt.artifact_ref
    );
    assert_eq!(completion.units[1].status, UnitStatus::Done);
    assert_eq!(
        completion.units[1].evidence[0].artifact_ref,
        current_receipt.artifact_ref
    );
    assert_eq!(completion.units[2].status, UnitStatus::Planned);
    assert_eq!(completion.current_unit, None);

    let update = json!({
        "action":"update",
        "root_task_id":"root",
        "completion_state":serde_json::to_value(&completion).expect("serialize current completion state")
    });
    TaskContextBoundaryHandler::new(true)
        .handle(invocation_in_session(
            Arc::clone(&session),
            Arc::clone(&turn),
            "update-after-history",
            update,
        ))
        .await
        .expect("completion update resolves exact historical unit evidence");
    let _ = store;
}

#[tokio::test]
async fn autonomous_accept_retains_three_accepted_revisions_across_update_and_checkpoint_reload() {
    let (mut session, mut turn_context) = make_session_and_context().await;
    turn_context.session_source = SessionSource::Cli;
    Arc::make_mut(&mut turn_context.config).rencrow_compaction = true;
    Arc::make_mut(&mut turn_context.config).rencrow_autonomous_completion = true;
    let turn = Arc::new(turn_context);
    let store = attach_boundary_owner(&mut session, Arc::clone(&turn)).await;
    let session = Arc::new(session);
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(
        workspace.path().join("artifact.txt"),
        "same verified artifact",
    )
    .expect("create artifact");

    let completion_contract = json!({
        "contract_revision": 1,
        "conditions": [{
            "id": "AC-1", "revision": 1, "result": "Deliver task-a", "scope": "task-a",
            "verification": "Run task-a check", "acceptance_criteria": "Check passes", "status": "open"
        }],
        "units": [{
            "id": "task-a", "purpose": "Complete task-a", "status": "running",
            "kind": "execution", "condition_refs": ["AC-1"], "depends_on": [], "task_refs": []
        }],
        "current_unit": "task-a"
    });
    let handler = TaskContextBoundaryHandler::new(true);
    handler
        .handle(invocation_in_session(
            Arc::clone(&session),
            Arc::clone(&turn),
            "begin-revision-1",
            json!({
                "action":"begin", "root_task_id":"root", "task_id":"task-a",
                "task_revision":1, "completion_state":completion_contract
            }),
        ))
        .await
        .expect("begin revision one with completion contract");
    record_call(
        &session,
        &turn,
        "begin-revision-1",
        TASK_CONTEXT_BOUNDARY_TOOL_NAME,
    )
    .await;
    record_output(
        &session,
        &turn,
        "begin-revision-1",
        TASK_CONTEXT_BOUNDARY_TOOL_NAME,
    )
    .await;

    for revision in 1..=3 {
        if revision > 1 {
            let call_id = format!("begin-revision-{revision}");
            handler
                .handle(invocation_in_session(
                    Arc::clone(&session),
                    Arc::clone(&turn),
                    &call_id,
                    begin("root", "task-a", revision),
                ))
                .await
                .expect("begin next revision for the same task");
            record_call(&session, &turn, &call_id, TASK_CONTEXT_BOUNDARY_TOOL_NAME).await;
            record_output(&session, &turn, &call_id, TASK_CONTEXT_BOUNDARY_TOOL_NAME).await;
        }

        let verify_call_id = format!("verify-revision-{revision}");
        record_successful_verification(
            &session,
            &turn,
            workspace.path(),
            "task-a",
            revision,
            &verify_call_id,
        )
        .await;

        let accept_call_id = format!("accept-revision-{revision}");
        let accept_args = json!({
            "action":"accept",
            "result":format!("accepted revision {revision}"),
            "satisfied_conditions":["AC-1"]
        });
        record_call_with_arguments(
            &session,
            &turn,
            &accept_call_id,
            TASK_CONTEXT_BOUNDARY_TOOL_NAME,
            &accept_args.to_string(),
        )
        .await;
        handler
            .handle(invocation_in_session(
                Arc::clone(&session),
                Arc::clone(&turn),
                &accept_call_id,
                accept_args,
            ))
            .await
            .expect("accept current revision while retaining older unit evidence");
        record_output(
            &session,
            &turn,
            &accept_call_id,
            TASK_CONTEXT_BOUNDARY_TOOL_NAME,
        )
        .await;
        assert!(
            session
                .commit_prepared_task_boundary(&turn, &tokio_util::sync::CancellationToken::new())
                .await
                .expect("commit accepted revision")
        );

        let stored = session
            .latest_rencrow_task_state()
            .await
            .expect("latest boundary is valid")
            .expect("latest boundary exists");
        assert_eq!(stored.completed["task-a"].revision, revision);
        assert_eq!(stored.accepted_history.len(), (revision - 1) as usize);
        let completion = session
            .rencrow_completion_state("root")
            .await
            .expect("completion state remains registered");
        assert_eq!(completion.units[0].evidence.len(), revision as usize);
        assert_eq!(completion.conditions[0].evidence[0].task_revision, revision);
        for (index, evidence) in completion.units[0].evidence.iter().enumerate() {
            assert_eq!(evidence.task_revision, (index + 1) as u64);
        }
        let restored = codex_history::task_context::StoredTaskState::parse_and_validate(
            serde_json::to_value(&stored).expect("serialize accepted revision checkpoint"),
            &session.thread_id().to_string(),
        )
        .expect("persisted revision history survives cold-style parse and validation");
        assert_eq!(restored.accepted_history, stored.accepted_history);

        if revision == 2 {
            let status_id = "status-after-revision-2";
            handler
                .handle(invocation_in_session(
                    Arc::clone(&session),
                    Arc::clone(&turn),
                    status_id,
                    json!({"action":"status", "root_task_id":"root"}),
                ))
                .await
                .expect("status reads completion state with retained history");
            record_call(&session, &turn, status_id, TASK_CONTEXT_BOUNDARY_TOOL_NAME).await;
            record_output(&session, &turn, status_id, TASK_CONTEXT_BOUNDARY_TOOL_NAME).await;

            let update_id = "update-after-revision-2";
            let update_args = json!({
                "action":"update", "root_task_id":"root",
                "completion_state":serde_json::to_value(&completion).expect("serialize update")
            });
            record_call_with_arguments(
                &session,
                &turn,
                update_id,
                TASK_CONTEXT_BOUNDARY_TOOL_NAME,
                &update_args.to_string(),
            )
            .await;
            handler
                .handle(invocation_in_session(
                    Arc::clone(&session),
                    Arc::clone(&turn),
                    update_id,
                    update_args,
                ))
                .await
                .expect("update validates current condition and exact older unit evidence");
            record_output(&session, &turn, update_id, TASK_CONTEXT_BOUNDARY_TOOL_NAME).await;
        }
    }
    let _ = store;
}

#[tokio::test]
async fn fresh_receipt_after_history_replacement_survives_exact_begin_replay() {
    let (mut session, mut turn_context) = make_session_and_context().await;
    turn_context.session_source = SessionSource::Cli;
    Arc::make_mut(&mut turn_context.config).rencrow_compaction = true;
    let turn = Arc::new(turn_context);
    attach_boundary_owner(&mut session, Arc::clone(&turn)).await;
    let session = Arc::new(session);
    for index in 0..16 {
        let call_id = format!("old-call-{index}");
        record_call(&session, &turn, &call_id, "read_file").await;
        record_output(&session, &turn, &call_id, "read_file").await;
    }

    begin_scope(&session, &turn, "task-a", 1, "begin-call").await;
    let old_scope = session.active_task_scope().await.unwrap();
    let history = session.clone_history().await;
    let begin_pair = history
        .raw_items()
        .filter(|item| match item {
            ResponseItem::FunctionCall { call_id, .. } => call_id == "begin-call",
            ResponseItem::FunctionCallOutput {
                call_id: Some(call_id),
                ..
            } => call_id == "begin-call",
            _ => false,
        })
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(begin_pair.len(), 2);
    session.replace_history(begin_pair, None).await;

    TaskContextBoundaryHandler::new(false)
        .handle(invocation_in_session(
            Arc::clone(&session),
            Arc::clone(&turn),
            "begin-call",
            begin("root", "task-a", 1),
        ))
        .await
        .expect("exact begin replay remains idempotent after replacement");
    let replayed_scope = session.active_task_scope().await.unwrap();
    assert_eq!(replayed_scope.root_task_id, old_scope.root_task_id);
    assert_eq!(replayed_scope.task_id, old_scope.task_id);
    assert_eq!(replayed_scope.task_revision, old_scope.task_revision);
    assert_eq!(replayed_scope.begin_call_id, old_scope.begin_call_id);

    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("artifact.txt"), "verified").unwrap();
    record_successful_verification(
        &session,
        &turn,
        workspace.path(),
        "task-a",
        1,
        "verify-call",
    )
    .await;
    let history = session.clone_history().await;
    let verification_index = history
        .raw_items()
        .position(|item| matches!(item, ResponseItem::FunctionCall { call_id, .. } if call_id == "verify-call"))
        .unwrap();
    assert_eq!(old_scope.start_history_cursor, 32);
    assert_eq!(verification_index, 2);

    let accept_args = json!({
        "action":"accept",
        "goal":"Finish task A",
        "tasks":[{"id":"task-a", "purpose":"Verify task A", "status":"completed"}],
        "current_task_id":null,
        "result":"Task A verification passed"
    });
    record_call_with_arguments(
        &session,
        &turn,
        "accept-call",
        TASK_CONTEXT_BOUNDARY_TOOL_NAME,
        &accept_args.to_string(),
    )
    .await;
    TaskContextBoundaryHandler::new(false)
        .handle(invocation_in_session(
            Arc::clone(&session),
            Arc::clone(&turn),
            "accept-call",
            accept_args,
        ))
        .await
        .expect("fresh same-scope verification receipt remains eligible after replacement");
    record_output(
        &session,
        &turn,
        "accept-call",
        TASK_CONTEXT_BOUNDARY_TOOL_NAME,
    )
    .await;
    assert!(
        session
            .commit_prepared_task_boundary(&turn, &tokio_util::sync::CancellationToken::new(),)
            .await
            .expect("history replacement preserves task boundary provenance")
    );
}

async fn assert_boundary_rejected(
    session: &Arc<Session>,
    turn: &Arc<TurnContext>,
    call_id: &str,
    args: serde_json::Value,
    expected: &str,
) {
    let error = match TaskContextBoundaryHandler::new(false)
        .handle(invocation_in_session(
            Arc::clone(session),
            Arc::clone(turn),
            call_id,
            args,
        ))
        .await
    {
        Ok(_) => panic!("invalid task boundary should be rejected"),
        Err(error) => error,
    };
    assert!(error.to_string().contains(expected), "{error}");
}

#[tokio::test]
async fn accept_preserves_omitted_host_accepted_completions_and_replay_is_idempotent() {
    let (mut session, mut turn_context) = make_session_and_context().await;
    turn_context.session_source = SessionSource::Cli;
    Arc::make_mut(&mut turn_context.config).rencrow_compaction = true;
    let turn = Arc::new(turn_context);
    let store = attach_boundary_owner(&mut session, Arc::clone(&turn)).await;
    let session = Arc::new(session);
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("artifact.txt"), "runtime verified").unwrap();

    begin_scope(
        &session,
        &turn,
        "verify-installed-task-runtime",
        1,
        "begin-runtime",
    )
    .await;
    record_successful_verification(
        &session,
        &turn,
        workspace.path(),
        "verify-installed-task-runtime",
        1,
        "verify-runtime",
    )
    .await;
    let first_accept = json!({
        "action":"accept",
        "goal":"Complete the Ren2 recovery",
        "tasks":[
            {"id":"verify-installed-task-runtime", "purpose":"Verify installed runtime", "status":"completed"},
            {"id":"product-owner-answer-pending", "purpose":"Resolve the product owner answer", "status":"blocked"}
        ],
        "current_task_id":"product-owner-answer-pending",
        "result":"Installed runtime verification passed"
    });
    record_call_with_arguments(
        &session,
        &turn,
        "accept-runtime",
        TASK_CONTEXT_BOUNDARY_TOOL_NAME,
        &first_accept.to_string(),
    )
    .await;
    TaskContextBoundaryHandler::new(false)
        .handle(invocation_in_session(
            Arc::clone(&session),
            Arc::clone(&turn),
            "accept-runtime",
            first_accept,
        ))
        .await
        .expect("first completion is accepted");
    record_output(
        &session,
        &turn,
        "accept-runtime",
        TASK_CONTEXT_BOUNDARY_TOOL_NAME,
    )
    .await;
    assert!(
        session
            .commit_prepared_task_boundary(&turn, &tokio_util::sync::CancellationToken::new())
            .await
            .expect("first boundary commits")
    );
    let previous = session
        .latest_rencrow_task_state()
        .await
        .unwrap()
        .expect("first task state is persisted");
    assert_eq!(previous.boundary.revision, 1);
    assert_eq!(
        previous.completed.keys().cloned().collect::<Vec<_>>(),
        ["verify-installed-task-runtime"]
    );
    assert_eq!(previous.boundary.tasks[0].status, TaskStatus::Completed);
    assert_eq!(previous.boundary.tasks[1].status, TaskStatus::Blocked);
    let inherited = normalize_accepted_tasks(
        Vec::new(),
        Some(&previous),
        &session.thread_id().to_string(),
        Some("root"),
    )
    .expect("only host-accepted completed tasks are inherited");
    assert_eq!(
        inherited
            .iter()
            .map(|task| task.id.as_str())
            .collect::<Vec<_>>(),
        ["verify-installed-task-runtime"]
    );
    assert!(
        normalize_accepted_tasks(
            Vec::new(),
            Some(&previous),
            &session.thread_id().to_string(),
            Some("different-root"),
        )
        .unwrap_err()
        .contains("another thread or root")
    );
    assert!(
        normalize_accepted_tasks(
            Vec::new(),
            Some(&previous),
            "different-thread",
            Some("root"),
        )
        .unwrap_err()
        .contains("another thread or root")
    );
    assert_boundary_rejected(
        &session,
        &turn,
        "accept-downgrade",
        json!({
            "action":"accept",
            "goal":"Complete the Ren2 recovery",
            "tasks":[
                {"id":"verify-installed-task-runtime", "purpose":"Verify installed runtime", "status":"blocked"},
                {"id":"product-owner-answer-pending", "purpose":"Resolve the product owner answer", "status":"blocked"}
            ],
            "current_task_id":"product-owner-answer-pending",
            "result":"Downgrade must be refused"
        }),
        "cannot return to Blocked",
    )
    .await;
    assert_boundary_rejected(
        &session,
        &turn,
        "accept-duplicate",
        json!({
            "action":"accept",
            "goal":"Complete the Ren2 recovery",
            "tasks":[
                {"id":"verify-installed-task-runtime", "purpose":"Verify installed runtime", "status":"completed"},
                {"id":"verify-installed-task-runtime", "purpose":"Duplicate", "status":"completed"}
            ],
            "current_task_id":null,
            "result":"Duplicate must be refused"
        }),
        "duplicate task IDs",
    )
    .await;

    std::fs::write(
        workspace.path().join("artifact.txt"),
        "answer task verified",
    )
    .unwrap();
    begin_scope(
        &session,
        &turn,
        "product-owner-answer-pending",
        2,
        "begin-answer",
    )
    .await;
    record_successful_verification(
        &session,
        &turn,
        workspace.path(),
        "product-owner-answer-pending",
        2,
        "verify-answer",
    )
    .await;
    assert_boundary_rejected(
        &session,
        &turn,
        "accept-unverified-completion",
        json!({
            "action":"accept",
            "goal":"Complete the Ren2 recovery",
            "tasks":[
                {"id":"product-owner-answer-pending", "purpose":"Resolve the product owner answer", "status":"completed"},
                {"id":"unverified-new-task", "purpose":"No host receipt exists", "status":"completed"},
                {"id":"continue-work", "purpose":"Continue recovery work", "status":"in_progress"}
            ],
            "current_task_id":"continue-work",
            "result":"Unverified completion must be refused"
        }),
        "UnverifiedCompletedTask",
    )
    .await;
    assert_boundary_rejected(
        &session,
        &turn,
        "accept-dependency-error",
        json!({
            "action":"accept",
            "goal":"Complete the Ren2 recovery",
            "tasks":[
                {"id":"product-owner-answer-pending", "purpose":"Resolve the product owner answer", "status":"completed"},
                {"id":"continue-work", "purpose":"Continue recovery work", "status":"in_progress", "depends_on":["missing-task"]}
            ],
            "current_task_id":"continue-work",
            "result":"Invalid dependency must be refused"
        }),
        "DependencyMissing",
    )
    .await;
    assert_boundary_rejected(
        &session,
        &turn,
        "accept-missing-closed-task",
        json!({
            "action":"accept",
            "goal":"Complete the Ren2 recovery",
            "tasks":[
                {"id":"continue-work", "purpose":"Continue recovery work", "status":"in_progress"}
            ],
            "current_task_id":"continue-work",
            "result":"The verified closed task must be explicit"
        }),
        "ClosedTaskNotCompleted",
    )
    .await;
    let large_purpose = "x".repeat(4096);
    let oversized_tasks = std::iter::once(json!({
        "id":"product-owner-answer-pending", "purpose":"Resolve the product owner answer", "status":"completed"
    }))
    .chain(std::iter::once(json!({
        "id":"continue-work", "purpose":"Continue recovery work", "status":"in_progress"
    })))
    .chain((0..8).map(|index| json!({
        "id":format!("large-task-{index}"), "purpose":large_purpose, "status":"pending"
    })))
    .collect::<Vec<_>>();
    assert_boundary_rejected(
        &session,
        &turn,
        "accept-oversized",
        json!({
            "action":"accept",
            "goal":"Complete the Ren2 recovery",
            "tasks":oversized_tasks,
            "current_task_id":"continue-work",
            "result":"Oversized state must be refused"
        }),
        "Oversized",
    )
    .await;
    let second_accept = json!({
        "action":"accept",
        "goal":"Complete the Ren2 recovery",
        "tasks":[
            {"id":"product-owner-answer-pending", "purpose":"Resolve the product owner answer", "status":"completed"},
            {"id":"continue-work", "purpose":"Continue recovery work", "status":"in_progress"}
        ],
        "current_task_id":"continue-work",
        "result":"Product owner answer task verification passed"
    });
    record_call_with_arguments(
        &session,
        &turn,
        "accept-answer",
        TASK_CONTEXT_BOUNDARY_TOOL_NAME,
        &second_accept.to_string(),
    )
    .await;
    TaskContextBoundaryHandler::new(false)
        .handle(invocation_in_session(
            Arc::clone(&session),
            Arc::clone(&turn),
            "accept-answer",
            second_accept.clone(),
        ))
        .await
        .expect("accept inherits the prior host-accepted completion");
    assert!(session.pending_task_boundary().await.is_some());
    record_output(
        &session,
        &turn,
        "accept-answer",
        TASK_CONTEXT_BOUNDARY_TOOL_NAME,
    )
    .await;
    assert!(
        session
            .commit_prepared_task_boundary(&turn, &tokio_util::sync::CancellationToken::new())
            .await
            .expect("second boundary commits")
    );
    let committed = session
        .latest_rencrow_task_state()
        .await
        .unwrap()
        .expect("second task state is persisted");
    assert_eq!(committed.boundary.revision, 2);
    assert!(
        committed
            .completed
            .contains_key("verify-installed-task-runtime")
    );
    assert!(
        committed
            .completed
            .contains_key("product-owner-answer-pending")
    );
    let persisted = codex_thread_store::ThreadStore::load_history(
        store.as_ref(),
        codex_thread_store::LoadThreadHistoryParams {
            thread_id: session.thread_id(),
            include_archived: false,
        },
    )
    .await
    .expect("read committed task state from persisted history");
    let persisted_replacement = persisted
        .items
        .iter()
        .rev()
        .find_map(|item| match item {
            codex_history::RolloutItem::Compacted(compacted) => {
                compacted.replacement_history.clone()
            }
            _ => None,
        })
        .expect("persisted task boundary replacement");
    let reloaded = crate::compact::latest_task_state_in_history(
        &persisted_replacement,
        &session.thread_id().to_string(),
    )
    .expect("parse reloaded task state")
    .expect("reloaded task state exists");
    assert!(
        reloaded
            .completed
            .contains_key("verify-installed-task-runtime")
    );

    TaskContextBoundaryHandler::new(false)
        .handle(invocation_in_session(
            Arc::clone(&session),
            Arc::clone(&turn),
            "accept-answer-replay",
            second_accept,
        ))
        .await
        .expect("same abbreviated acceptance is idempotent after commit");
    assert_eq!(
        session
            .latest_rencrow_task_state()
            .await
            .unwrap()
            .unwrap()
            .boundary
            .revision,
        2
    );
}

#[tokio::test]
async fn accept_uses_host_receipt_and_stays_pending_until_safe_point() {
    let (mut session, mut turn_context) = make_session_and_context().await;
    turn_context.session_source = SessionSource::Cli;
    Arc::make_mut(&mut turn_context.config).rencrow_compaction = true;
    let turn = Arc::new(turn_context);
    let store = attach_boundary_owner(&mut session, Arc::clone(&turn)).await;
    let session = Arc::new(session);
    let begin_invocation = invocation_in_session(
        Arc::clone(&session),
        Arc::clone(&turn),
        "begin-call",
        begin("root", "task-a", 7),
    );
    TaskContextBoundaryHandler::new(false)
        .handle(begin_invocation)
        .await
        .expect("begin task scope");
    record_call(
        &session,
        &turn,
        "begin-call",
        TASK_CONTEXT_BOUNDARY_TOOL_NAME,
    )
    .await;
    record_output(
        &session,
        &turn,
        "begin-call",
        TASK_CONTEXT_BOUNDARY_TOOL_NAME,
    )
    .await;

    let directory = tempfile::tempdir().expect("workspace");
    std::fs::write(directory.path().join("artifact.txt"), "verified").unwrap();
    let request = TaskEvidenceRequest {
        task_id: "task-a".into(),
        task_revision: 7,
        artifact_paths: vec![std::path::PathBuf::from("artifact.txt")],
        check_kind: TaskCheckKind::Verification,
    };
    let evidence = TaskCheckExecution::new(request.clone(), directory.path()).unwrap();
    let before = evidence.before.clone();
    let after = evidence.hash_artifacts().unwrap();
    session
        .begin_task_check(
            "verify-call".into(),
            request,
            evidence.workspace_root.clone(),
            before,
        )
        .await;
    session
        .finish_task_check(
            "verify-call",
            /*exit_code*/ 0,
            /*completed*/ true,
            after,
        )
        .await
        .expect("successful receipt");
    record_call(&session, &turn, "verify-call", "exec_command").await;
    record_output(&session, &turn, "verify-call", "exec_command").await;
    record_call(&session, &turn, "read-call", "read_file").await;
    record_output(&session, &turn, "read-call", "read_file").await;

    let accept_args = json!({
        "action":"accept",
        "goal":"Finish the parent task",
        "tasks":[
            {"id":"task-a", "purpose":"Verify task A", "status":"completed"},
            {"id":"task-b", "purpose":"Implement task B", "status":"in_progress"}
        ],
        "current_task_id":"task-b",
        "decisions":["Keep the host receipt"],
        "constraints":["Preserve user instructions"],
        "result":"Task A verification passed"
    });
    record_call_with_arguments(
        &session,
        &turn,
        "accept-call",
        TASK_CONTEXT_BOUNDARY_TOOL_NAME,
        &accept_args.to_string(),
    )
    .await;
    let accept = invocation_in_session(
        Arc::clone(&session),
        Arc::clone(&turn),
        "accept-call",
        accept_args.clone(),
    );
    TaskContextBoundaryHandler::new(false)
        .handle(accept)
        .await
        .expect("accept prepares boundary");
    assert!(session.pending_task_boundary().await.is_some());
    assert!(session.latest_rencrow_task_state().await.unwrap().is_none());

    record_output(
        &session,
        &turn,
        "accept-call",
        TASK_CONTEXT_BOUNDARY_TOOL_NAME,
    )
    .await;
    assert!(
        session
            .commit_prepared_task_boundary(&turn, &tokio_util::sync::CancellationToken::new())
            .await
            .expect("safe-point commit")
    );
    assert!(session.pending_task_boundary().await.is_none());
    assert!(session.active_task_scope().await.is_none());
    let stored = session.latest_rencrow_task_state().await.unwrap().unwrap();
    assert_eq!(stored.boundary.closed_task_id, "task-a");
    assert_eq!(stored.boundary.revision, 1);
    TaskContextBoundaryHandler::new(false)
        .handle(invocation_in_session(
            Arc::clone(&session),
            Arc::clone(&turn),
            "accept-retry",
            accept_args.clone(),
        ))
        .await
        .expect("identical completed acceptance is idempotent");
    let mut conflicting_retry = accept_args;
    conflicting_retry["result"] = json!("different content");
    assert!(
        TaskContextBoundaryHandler::new(false)
            .handle(invocation_in_session(
                Arc::clone(&session),
                Arc::clone(&turn),
                "accept-conflict",
                conflicting_retry,
            ))
            .await
            .is_err()
    );
    let history = session.clone_history().await;
    let next_input = history
        .clone()
        .for_prompt(&turn.model_info().input_modalities);
    let prompt_text = next_input
        .iter()
        .filter_map(|item| match item {
            ResponseItem::Message { content, .. } => Some(
                content
                    .iter()
                    .filter_map(|part| match part {
                        codex_protocol::models::ContentItem::InputText { text } => {
                            Some(text.as_str())
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(prompt_text.contains("Task A verification passed"));
    assert!(prompt_text.contains("Implement task B"));
    assert!(history.annotated_items().iter().any(|envelope| {
        matches!(
            &envelope.item,
            ResponseItem::FunctionCall { call_id, .. }
                | ResponseItem::FunctionCallOutput {
                    call_id: Some(call_id),
                    ..
                } if call_id == "read-call"
        )
    }));
    let persisted = codex_thread_store::ThreadStore::load_history(
        store.as_ref(),
        codex_thread_store::LoadThreadHistoryParams {
            thread_id: session.thread_id(),
            include_archived: false,
        },
    )
    .await
    .expect("cold history read");
    let persisted_replacement = persisted
        .items
        .iter()
        .rev()
        .find_map(|item| match item {
            codex_history::RolloutItem::Compacted(compacted) => {
                compacted.replacement_history.clone()
            }
            _ => None,
        })
        .expect("persisted compacted replacement");
    let resumed_task_state = crate::compact::latest_task_state_in_history(
        &persisted_replacement,
        &session.thread_id().to_string(),
    )
    .expect("parse task state from persisted boundary")
    .expect("task state survives cold history read");
    assert_eq!(resumed_task_state.boundary.closed_task_id, "task-a");

    let begin_b = invocation_in_session(
        Arc::clone(&session),
        Arc::clone(&turn),
        "begin-b-call",
        begin("root", "task-b", 8),
    );
    TaskContextBoundaryHandler::new(false)
        .handle(begin_b)
        .await
        .expect("begin second task");
    record_call(
        &session,
        &turn,
        "begin-b-call",
        TASK_CONTEXT_BOUNDARY_TOOL_NAME,
    )
    .await;
    record_output(
        &session,
        &turn,
        "begin-b-call",
        TASK_CONTEXT_BOUNDARY_TOOL_NAME,
    )
    .await;
    std::fs::write(directory.path().join("artifact.txt"), "verified again").unwrap();
    let request_b = TaskEvidenceRequest {
        task_id: "task-b".into(),
        task_revision: 8,
        artifact_paths: vec![std::path::PathBuf::from("artifact.txt")],
        check_kind: TaskCheckKind::Verification,
    };
    let evidence_b = TaskCheckExecution::new(request_b.clone(), directory.path()).unwrap();
    session
        .begin_task_check(
            "verify-b-call".into(),
            request_b,
            evidence_b.workspace_root.clone(),
            evidence_b.before.clone(),
        )
        .await;
    session
        .finish_task_check(
            "verify-b-call",
            /*exit_code*/ 0,
            /*completed*/ true,
            evidence_b.hash_artifacts().unwrap(),
        )
        .await
        .expect("second successful receipt");
    record_call(&session, &turn, "verify-b-call", "exec_command").await;
    record_output(&session, &turn, "verify-b-call", "exec_command").await;
    let accept_b_args = json!({
        "action":"accept", "goal":"Finish the parent task",
        "tasks":[
            {"id":"task-a", "purpose":"Verify task A", "status":"completed"},
            {"id":"task-b", "purpose":"Verify task B", "status":"completed"},
            {"id":"task-c", "purpose":"Continue task C", "status":"in_progress"}
        ],
        "current_task_id":"task-c",
        "decisions":["Keep the host receipt"],
        "constraints":["Preserve user instructions"],
        "result":"Task B verification passed"
    });
    record_call_with_arguments(
        &session,
        &turn,
        "accept-b-call",
        TASK_CONTEXT_BOUNDARY_TOOL_NAME,
        &accept_b_args.to_string(),
    )
    .await;
    TaskContextBoundaryHandler::new(false)
        .handle(invocation_in_session(
            Arc::clone(&session),
            Arc::clone(&turn),
            "accept-b-call",
            accept_b_args,
        ))
        .await
        .expect("second accept prepares boundary");
    record_output(
        &session,
        &turn,
        "accept-b-call",
        TASK_CONTEXT_BOUNDARY_TOOL_NAME,
    )
    .await;
    std::fs::write(
        directory.path().join("artifact.txt"),
        "changed after acceptance",
    )
    .unwrap();
    assert!(
        !session
            .commit_prepared_task_boundary(&turn, &tokio_util::sync::CancellationToken::new())
            .await
            .expect("changed artifact is rejected without storage")
    );
    assert!(session.pending_task_boundary().await.is_none());
    assert_eq!(
        session.active_task_scope().await.unwrap().task_id,
        "task-b",
        "nonpersistent validation rejection leaves the active task retryable"
    );
    assert_eq!(
        session
            .latest_rencrow_task_state()
            .await
            .unwrap()
            .unwrap()
            .boundary
            .revision,
        1,
        "changed artifact must not persist a new boundary"
    );
    let history = session.clone_history().await;
    assert!(history.annotated_items().iter().any(|envelope| {
        matches!(
            &envelope.item,
            ResponseItem::Message { role, content, .. }
                if role == "developer"
                    && content.iter().any(|item| matches!(
                        item,
                        codex_protocol::models::ContentItem::InputText { text }
                            if text.contains("Task boundary was not committed")
                                && text.contains("changed after acceptance")
                    ))
        )
    }));
}
