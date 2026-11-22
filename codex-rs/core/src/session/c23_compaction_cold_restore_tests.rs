use super::*;
use crate::compact::InitialContextInjection;
use crate::session::step_context::StepContext;
use crate::state::ActiveTurn;
use crate::state::RunningTask;
use crate::state::TaskKind;
use crate::task_check::TaskCheckExecution;
use crate::task_check::TaskCheckKind;
use crate::task_check::TaskCheckObservation;
use crate::task_check::TaskEvidenceRequest;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::task_context_boundary::TaskContextBoundaryHandler;
use crate::tools::handlers::task_context_boundary_spec::TASK_CONTEXT_BOUNDARY_TOOL_NAME;
use crate::turn_diff_tracker::TurnDiffTracker;
use codex_history::ResponseItemEnvelope;
use codex_history::task_context::AcceptedTaskEvidenceReceipt;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ReasoningEffort as ReasoningEffortConfig;
use codex_tools::ToolName;
use core_test_support::responses;
use pretty_assertions::assert_eq;
use std::process::Command;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

static ACTIVE_TURNS: codex_diagnostics::Gauge =
    codex_diagnostics::Gauge::new("c23_compaction_cold_restore_active_turns");

fn invocation(
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    call_id: &str,
    args: serde_json::Value,
) -> ToolInvocation {
    ToolInvocation {
        session,
        step_context: StepContext::for_test(Arc::clone(&turn)),
        turn,
        cancellation_token: CancellationToken::new(),
        tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
        call_id: call_id.to_string(),
        tool_name: ToolName::plain(TASK_CONTEXT_BOUNDARY_TOOL_NAME),
        source: ToolCallSource::Direct,
        payload: ToolPayload::Function {
            arguments: serde_json::to_string(&args).expect("serialize boundary args"),
        },
    }
}

async fn record_named_call(
    session: &Session,
    turn: &TurnContext,
    call_id: &str,
    name: &str,
    args: &str,
) {
    session
        .record_annotated_conversation_items(
            turn,
            turn.model_info().as_ref(),
            vec![ResponseItemEnvelope::new(ResponseItem::FunctionCall {
                id: None,
                name: name.to_string(),
                namespace: None,
                arguments: args.to_string(),
                encrypted_function_args: None,
                call_id: call_id.to_string(),
                internal_chat_message_metadata_passthrough: None,
            })],
        )
        .await;
}

async fn record_named_output(
    session: &Session,
    turn: &TurnContext,
    call_id: &str,
    name: &str,
    output: &str,
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
                    output: FunctionCallOutputPayload::from_text(output.to_string()),
                    internal_chat_message_metadata_passthrough: None,
                },
            )],
        )
        .await;
}

async fn record_boundary_call(session: &Session, turn: &TurnContext, call_id: &str, args: &str) {
    record_named_call(
        session,
        turn,
        call_id,
        TASK_CONTEXT_BOUNDARY_TOOL_NAME,
        args,
    )
    .await;
}

async fn record_boundary_output(session: &Session, turn: &TurnContext, call_id: &str) {
    record_named_output(
        session,
        turn,
        call_id,
        TASK_CONTEXT_BOUNDARY_TOOL_NAME,
        "ok",
    )
    .await;
}

async fn accept_revision(
    session: &Arc<Session>,
    turn: &Arc<TurnContext>,
    workspace: &std::path::Path,
    revision: u64,
) {
    let handler = TaskContextBoundaryHandler::new(true);
    let workspace_cwd = workspace.canonicalize().expect("canonical isolated cwd");
    let begin_id = format!("begin-{revision}");
    let begin_args = if revision == 1 {
        serde_json::json!({
            "action":"begin", "root_task_id":"root", "task_id":"task-a",
            "task_revision":revision,
            "completion_state":{
                "contract_revision":1,
                "conditions":[{
                    "id":"AC-1", "revision":1, "result":"Deliver task-a", "scope":"task-a",
                    "verification":"Run task-a check", "acceptance_criteria":"Check passes", "status":"open"
                }],
                "units":[{
                    "id":"task-a", "purpose":"Complete task-a", "status":"running",
                    "kind":"execution", "condition_refs":["AC-1"], "depends_on":[], "task_refs":[]
                }],
                "current_unit":"task-a"
            }
        })
    } else {
        serde_json::json!({"action":"begin", "root_task_id":"root", "task_id":"task-a", "task_revision":revision})
    };
    let begin_args_text = begin_args.to_string();
    handler
        .handle(invocation(
            Arc::clone(session),
            Arc::clone(turn),
            &begin_id,
            begin_args,
        ))
        .await
        .expect("begin task revision");
    record_boundary_call(session, turn, &begin_id, &begin_args_text).await;
    record_boundary_output(session, turn, &begin_id).await;

    let request = TaskEvidenceRequest {
        task_id: "task-a".into(),
        task_revision: revision,
        artifact_paths: vec![std::path::PathBuf::from("artifact.txt")],
        check_kind: TaskCheckKind::Verification,
    };
    let execution = TaskCheckExecution::new(request.clone(), workspace).expect("hash artifact");
    let verify_id = format!("verify-{revision}");
    session
        .begin_task_check(
            verify_id.clone(),
            request,
            execution.workspace_root.clone(),
            execution.before.clone(),
        )
        .await;
    let command = vec![
        "python3".to_string(),
        "-B".to_string(),
        "-c".to_string(),
        "from pathlib import Path; assert Path(\"artifact.txt\").is_file()".to_string(),
    ];
    let output = Command::new(&command[0])
        .args(&command[1..])
        .current_dir(&workspace_cwd)
        .output()
        .expect("run isolated artifact verification command");
    let exit_code = output.status.code().unwrap_or(-1);
    let completed = output.status.code().is_some();
    let diagnostic = format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let observation = TaskCheckObservation::from_execution(
        &verify_id,
        &command,
        &workspace_cwd,
        &diagnostic,
        exit_code,
        completed,
        false,
    );
    session
        .finish_task_check_observed(
            &verify_id,
            exit_code,
            completed,
            execution.hash_artifacts().expect("hash verified artifact"),
            Some(observation),
        )
        .await
        .expect("verification receipt succeeds");
    assert!(
        output.status.success(),
        "isolated verification command failed"
    );
    let command_text = format!(
        "{} {} {} '{}'",
        command[0], command[1], command[2], command[3]
    );
    // Unified exec accepts absolute native workdir values and resolves them against the URI cwd.
    let workdir = workspace_cwd.to_string_lossy().into_owned();
    let command_args = serde_json::json!({
        "cmd":command_text,
        "workdir":workdir,
        "task_evidence": {
            "task_id":"task-a",
            "task_revision":revision,
            "artifact_paths":["artifact.txt"],
            "check_kind":"verification"
        }
    });
    let command_args = command_args.to_string();
    let _: crate::tools::handlers::unified_exec::ExecCommandArgs =
        serde_json::from_str(&command_args)
            .expect("exec call uses the declared command and task-evidence schema");
    record_named_call(session, turn, &verify_id, "exec_command", &command_args).await;
    record_named_output(session, turn, &verify_id, "exec_command", &diagnostic).await;

    let accept_id = format!("accept-{revision}");
    let accept_args = serde_json::json!({
        "action":"accept", "result":format!("accepted revision {revision}"),
        "satisfied_conditions":["AC-1"]
    });
    let args_text = accept_args.to_string();
    record_boundary_call(session, turn, &accept_id, &args_text).await;
    handler
        .handle(invocation(
            Arc::clone(session),
            Arc::clone(turn),
            &accept_id,
            accept_args,
        ))
        .await
        .expect("accept verified revision");
    record_boundary_output(session, turn, &accept_id).await;
    assert!(
        session
            .commit_prepared_task_boundary(turn, &CancellationToken::new())
            .await
            .expect("commit native task boundary")
    );
}

async fn install_active_turn(session: &Session, turn: Arc<TurnContext>) {
    *session.active_turn.lock().await = Some(ActiveTurn {
        task: Some(RunningTask {
            done: Arc::new(tokio::sync::Notify::new()),
            kind: TaskKind::Regular,
            task: Arc::new(crate::tasks::RegularTask::new()),
            cancellation_token: CancellationToken::new(),
            handle: tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async {
                std::future::pending::<()>().await;
            })),
            turn_context: turn,
            _agent_execution_guard: None,
            _diagnostics_guard: ACTIVE_TURNS.track(),
            _timer: None,
        }),
        ..Default::default()
    });
}

fn accepted_identities(
    state: &codex_history::task_context::StoredTaskState,
) -> Vec<AcceptedTaskEvidenceReceipt> {
    let mut identities = state.accepted_history.clone();
    identities.push(AcceptedTaskEvidenceReceipt {
        task_id: state.boundary.closed_task_id.clone(),
        task_revision: state.completed[&state.boundary.closed_task_id].revision,
        artifact_ref: state.completed[&state.boundary.closed_task_id]
            .artifact_ref
            .clone(),
        verification_ref: state.completed[&state.boundary.closed_task_id]
            .verification_ref
            .clone(),
    });
    identities.sort_by_key(|receipt| receipt.task_revision);
    identities
}

async fn verification_snapshots(session: &Session) -> Vec<serde_json::Value> {
    let mut snapshots = session
        .task_check_snapshot()
        .await
        .into_iter()
        .filter(|snapshot| snapshot.attempt.call_id.starts_with("verify-"))
        .map(|snapshot| serde_json::to_value(snapshot).expect("serialize host task check"))
        .collect::<Vec<_>>();
    snapshots.sort_by(|left, right| {
        left["attempt"]["call_id"]
            .as_str()
            .cmp(&right["attempt"]["call_id"].as_str())
    });
    snapshots
}

#[tokio::test]
async fn accepted_revision_receipts_survive_real_compaction_and_cold_restore() {
    let server = responses::start_mock_server().await;
    let provider = codex_model_provider_info::ModelProviderInfo::create_openai_provider(Some(
        format!("{}/v1", server.uri()),
    ));
    let cold_provider = provider.clone();
    let (mut session, mut turn, _) = make_session_and_context_with_auth_and_config_and_rx(
        codex_login::CodexAuth::create_dummy_chatgpt_auth_for_testing(),
        Vec::new(),
        move |config| {
            config.model = Some("gpt-5.2".to_string());
            config.model_provider = provider;
            config.model_provider.supports_websockets = false;
            config.model_reasoning_effort = Some(ReasoningEffortConfig::High);
            config.rencrow_compaction = true;
            config.rencrow_autonomous_completion = true;
        },
    )
    .await;
    Arc::get_mut(&mut turn)
        .expect("unique turn before fixture setup")
        .session_source = codex_protocol::protocol::SessionSource::Cli;
    let rollout_path =
        attach_thread_persistence(Arc::get_mut(&mut session).expect("unique session")).await;
    install_active_turn(&session, Arc::clone(&turn)).await;

    let workspace = tempfile::tempdir().expect("isolated OS temporary workspace");
    let mut expected_evidence = Vec::new();
    let mut prior_receipts = Vec::new();
    for revision in 1..=3 {
        let artifact = format!("verified artifact revision {revision}\n");
        std::fs::write(workspace.path().join("artifact.txt"), &artifact)
            .expect("write isolated artifact");
        accept_revision(&session, &turn, workspace.path(), revision).await;

        let state = session
            .latest_rencrow_task_state()
            .await
            .expect("read task checkpoint")
            .expect("task checkpoint exists");
        let receipt = state.completed.get("task-a").expect("latest receipt");
        assert_eq!(receipt.revision, revision);
        assert_eq!(receipt.result, format!("accepted revision {revision}"));
        let evidence = session
            .rencrow_completion_state("root")
            .await
            .expect("completion state")
            .units[0]
            .evidence
            .last()
            .expect("accepted revision evidence")
            .clone();
        assert_eq!(evidence.task_id, "task-a");
        assert_eq!(evidence.task_revision, revision);
        let bytes = session
            .task_evidence_artifact_bytes(&evidence)
            .await
            .expect("host verification ledger binds artifact path and hash");
        let artifact_stamps = bytes
            .iter()
            .map(|(path, sha256)| serde_json::json!({"path":path,"sha256":sha256}))
            .collect::<Vec<_>>();
        let fingerprint = codex_history::compaction_candidate::digest(&serde_json::json!({
                "task_id":evidence.task_id.as_str(),
                "task_revision":evidence.task_revision,
                "workspace_root":workspace.path().canonicalize().expect("canonical workspace").to_string_lossy(),
                "artifacts":artifact_stamps,
            }))
        .expect("recompute path-bound artifact fingerprint");
        assert_eq!(
            codex_history::task_context::task_check_artifact_fingerprint(&evidence.artifact_ref),
            Some(fingerprint.as_str())
        );
        expected_evidence.push((evidence, bytes));
        prior_receipts.push(receipt.clone());
    }

    let before = session
        .latest_rencrow_task_state()
        .await
        .expect("read final pre-compaction state")
        .expect("final task state");
    let expected_identities = accepted_identities(&before);
    assert_eq!(expected_identities.len(), 3);
    assert_eq!(prior_receipts.len(), 3);
    for (index, receipt) in prior_receipts.iter().enumerate() {
        assert_eq!(receipt.revision, (index + 1) as u64);
        assert_eq!(receipt.result, format!("accepted revision {}", index + 1));
        let identity = expected_identities
            .iter()
            .find(|identity| identity.task_revision == receipt.revision)
            .expect("accepted identity for each revision");
        assert_eq!(identity.task_id, "task-a");
        assert_eq!(identity.artifact_ref, receipt.artifact_ref);
        assert_eq!(identity.verification_ref, receipt.verification_ref);
        assert_eq!(
            expected_evidence[index].0.artifact_ref,
            receipt.artifact_ref
        );
        assert_eq!(
            expected_evidence[index].0.verification_ref,
            receipt.verification_ref
        );
    }
    assert_eq!(before.completed["task-a"].result, "accepted revision 3");
    let expected_task_checks = verification_snapshots(&session).await;
    assert_eq!(expected_task_checks.len(), 3);

    session
        .record_annotated_conversation_items(
            &turn,
            turn.model_info().as_ref(),
            vec![
                ResponseItemEnvelope::new(ResponseItem::Message {
                    id: None,
                    role: "user".into(),
                    content: vec![codex_protocol::models::ContentItem::InputText {
                        text: "Continue the verified task and preserve its accepted evidence."
                            .into(),
                    }],
                    phase: None,
                    internal_chat_message_metadata_passthrough: None,
                }),
                ResponseItemEnvelope::new(ResponseItem::Message {
                    id: None,
                    role: "assistant".into(),
                    content: vec![codex_protocol::models::ContentItem::OutputText {
                        text: "completed work ".repeat(4_000),
                    }],
                    phase: None,
                    internal_chat_message_metadata_passthrough: None,
                }),
            ],
        )
        .await;

    let summary_mock = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_assistant_message("c23-summary", "Retain the verified task state."),
            responses::ev_completed("c23-summary-response"),
        ]),
    )
    .await;
    crate::compact::run_inline_auto_compact_task(
        Arc::clone(&session),
        Arc::clone(&turn),
        InitialContextInjection::DoNotInject,
        CompactionReason::ContextLimit,
        CompactionPhase::MidTurn,
    )
    .await
    .expect("real compaction through the Responses endpoint");
    assert_eq!(summary_mock.requests().len(), 1);
    session
        .flush_rollout()
        .await
        .expect("flush compacted rollout");
    assert_eq!(verification_snapshots(&session).await, expected_task_checks);

    let compacted = session
        .latest_rencrow_task_state()
        .await
        .expect("read compacted checkpoint")
        .expect("compacted task state");
    assert_eq!(accepted_identities(&compacted), expected_identities);
    assert_eq!(compacted.completed["task-a"].result, "accepted revision 3");

    let InitialHistory::Resumed(resumed) = RolloutRecorder::get_rollout_history(&rollout_path)
        .await
        .expect("load canonical post-compaction rollout")
    else {
        panic!("compacted rollout should be resumable");
    };
    let source_thread_id = session.thread_id();
    assert_eq!(resumed.conversation_id, source_thread_id);
    let (mut fresh, _, _) = make_session_and_context_with_auth_and_config_and_rx(
        codex_login::CodexAuth::create_dummy_chatgpt_auth_for_testing(),
        Vec::new(),
        move |config| {
            config.model = Some("gpt-5.2".to_string());
            config.model_provider = cold_provider;
            config.model_provider.supports_websockets = false;
            config.model_reasoning_effort = Some(ReasoningEffortConfig::High);
            config.rencrow_compaction = true;
            config.rencrow_autonomous_completion = true;
        },
    )
    .await;
    Arc::get_mut(&mut fresh)
        .expect("unique fresh session before resume")
        .thread_id = source_thread_id;
    assert_eq!(fresh.thread_id(), source_thread_id);
    fresh
        .record_initial_history(InitialHistory::Resumed(resumed))
        .await;
    assert_eq!(verification_snapshots(&fresh).await, expected_task_checks);
    let restored = fresh
        .latest_rencrow_task_state()
        .await
        .expect("read restored task checkpoint")
        .expect("restored task state");
    assert_eq!(accepted_identities(&restored), expected_identities);
    assert_eq!(restored.completed["task-a"].result, "accepted revision 3");

    let restored_completion = fresh
        .rencrow_completion_state("root")
        .await
        .expect("restored completion state");
    for (index, (evidence, expected_bytes)) in expected_evidence.iter().enumerate() {
        assert_eq!(evidence.task_revision, (index + 1) as u64);
        let restored_bytes = fresh.task_evidence_artifact_bytes(evidence).await;
        assert_eq!(
            restored_bytes.as_ref(),
            Some(expected_bytes),
            "cold restore must retain the host path/hash evidence for each accepted revision"
        );
        assert!(
            restored_completion.units[0]
                .evidence
                .iter()
                .any(|actual| actual == evidence)
        );
    }
}
