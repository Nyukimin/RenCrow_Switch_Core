use super::*;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context;
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

fn begin(root: &str, task: &str, revision: u64) -> serde_json::Value {
    json!({"action":"begin", "root_task_id":root, "task_id":task, "task_revision":revision})
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
    for autonomous_field in ["target_task_id", "unit_id"] {
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
    for accepted_field in [
        "root_task_id",
        "task_id",
        "task_revision",
        "result",
        "target_task_id",
        "unit_id",
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
    let expected: [(&str, &[&str], &[&str]); 7] = [
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
            &["action", "target_task_id"],
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
        json!({"action":"status"}),
        json!({"action":"plan_review","root_task_id":"root","previous_plan":"a","revised_plan":"b","reason":"new evidence"}),
        json!({"action":"accept","result":"done","satisfied_conditions":["AC-1"]}),
    ];
    for input in valid {
        serde_json::from_value::<TaskContextBoundaryArgs>(input)
            .expect("valid action shape parses");
    }

    for invalid in [
        json!({"action":"begin","root_task_id":"root","task_id":"unit","task_revision":1,"goal":"extra"}),
        json!({"action":"accept","goal":"goal","tasks":[],"result":"done","completion_state":{}}),
        json!({"action":"update","root_task_id":"root","completion_state":{},"goal":"extra"}),
    ] {
        assert!(serde_json::from_value::<TaskContextBoundaryArgs>(invalid).is_err());
    }
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
        .finish_task_check(call_id, 0, true, evidence.hash_artifacts().unwrap())
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

    let current_receipt = session
        .latest_rencrow_task_state()
        .await
        .expect("current task state parses")
        .expect("current task state exists")
        .completed
        .get("sp-newAC1")
        .cloned()
        .expect("current accepted receipt exists");
    assert_ne!(current_receipt, old_receipt);
    assert_eq!(current_receipt.revision, old_receipt.revision);
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
        current_receipt.artifact_ref
    );
    assert_eq!(completion.units[1].status, UnitStatus::Done);
    assert_eq!(
        completion.units[1].evidence[0].artifact_ref,
        current_receipt.artifact_ref
    );
    assert_eq!(completion.units[2].status, UnitStatus::Planned);
    assert_eq!(completion.current_unit, None);
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
        .finish_task_check("verify-call", 0, true, after)
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
            0,
            true,
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
