// Modified by RenCrow Switch Core, 2026-11-21: initialize accepted revision history fixtures.
// Modified by RenCrow Switch Core, 2026-11-21: exercise paginated handoff recovery.
use super::make_session_and_context;
use crate::agent::completion_recovery::SUPPORTED_RECHECK_CONDITION;
use crate::agent::completion_recovery::due_plan_review_event;
use crate::context::ContextualUserFragment;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::state::TaskKind;
use crate::task_check::TaskArtifactStamp;
use crate::task_check::TaskCheckKind;
use crate::task_check::TaskEvidenceRequest;
use codex_history::CodexHarnessMetadata;
use codex_history::ResponseItemEnvelope;
use codex_history::RolloutItem;
use codex_history::task_checkpoint_metadata::RenCrowTaskCheckpointMetadataV3;
use codex_history::task_checkpoint_metadata::TaskBoundarySourceFence;
use codex_history::task_context::AcceptanceCondition;
use codex_history::task_context::AcceptanceStatus;
use codex_history::task_context::AcceptedTaskReceipt;
use codex_history::task_context::BlockerCategory;
use codex_history::task_context::BoundaryCandidate;
use codex_history::task_context::BoundaryMode;
use codex_history::task_context::CompletionBlocker;
use codex_history::task_context::CompletionEvidenceRef;
use codex_history::task_context::CompletionState;
use codex_history::task_context::CompletionUnit;
use codex_history::task_context::CompletionUnitKind;
use codex_history::task_context::StoredTaskState;
use codex_history::task_context::TaskState;
use codex_history::task_context::TaskStatus;
use codex_history::task_context::UnitStatus;
use codex_utils_absolute_path::test_support::PathExt as AbsolutePathExt;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;

struct ParentHandoffRecoveryTask;

impl crate::tasks::SessionTask for ParentHandoffRecoveryTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }

    fn span_name(&self) -> &'static str {
        "completion_recovery.parent_handoff_fixture"
    }

    async fn run(
        self: Arc<Self>,
        _session: Arc<Session>,
        _ctx: Arc<TurnContext>,
        _input: Vec<crate::session::input_queue::TurnInput>,
        cancellation_token: CancellationToken,
    ) -> crate::tasks::SessionTaskResult {
        cancellation_token.cancelled().await;
        Err(codex_protocol::error::CodexErr::TurnAborted)
    }
}

async fn attach_reserved_cli_root_persistence(session: &mut Session) -> anyhow::Result<PathBuf> {
    let config = session.get_config().await;
    let thread_id = session.thread_id();
    let live_thread = codex_thread_store::LiveThread::create(
        Arc::clone(&session.services.thread_store),
        codex_thread_store::CreateThreadParams {
            creator_user_id: None,
            creator_account_id: None,
            session_id: thread_id.into(),
            thread_id,
            extra_config: None,
            forked_from_id: None,
            parent_thread_id: None,
            source: codex_protocol::protocol::SessionSource::Cli,
            thread_source: None,
            originator: "test_originator".into(),
            base_instructions: codex_protocol::models::BaseInstructions::default(),
            dynamic_tools: Vec::new(),
            selected_capability_roots: Vec::new(),
            multi_agent_version: None,
            history_mode: codex_protocol::protocol::ThreadHistoryMode::Paginated,
            subagent_history_start_ordinal: None,
            history_base: None,
            initial_window_id: uuid::Uuid::now_v7().to_string(),
            runtime_workspace_roots: None,
            metadata: codex_thread_store::ThreadPersistenceMetadata {
                cwd: Some(config.cwd.to_path_buf()),
                model_provider: config.model_provider_id.clone(),
                memory_mode: codex_protocol::protocol::ThreadMemoryMode::Disabled,
            },
        },
    )
    .await?;
    session.services.live_thread = Some(live_thread);
    session
        .ensure_rollout_materialized(codex_thread_store::PersistContext::Standard)
        .await;
    session.flush_rollout().await?;
    Ok(session
        .current_rollout_path()
        .await
        .expect("load reserved root rollout path")
        .expect("reserved root thread has a rollout path"))
}

async fn prepare_authenticated_handoff(
    mut fixture: LaunchRestoreFixture,
    handoff_directory: &std::path::Path,
) -> anyhow::Result<(
    Arc<Session>,
    codex_protocol::ThreadId,
    Vec<RolloutItem>,
    tempfile::TempDir,
)> {
    let config = Arc::make_mut(&mut fixture.turn_context.config);
    config.rencrow_compaction = true;
    config.rencrow_parent_handoff_dir =
        Some(codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(handoff_directory)?);
    let _source_rollout_path = attach_reserved_cli_root_persistence(&mut fixture.session).await?;
    let session = Arc::new(fixture.session);
    let turn = Arc::new(fixture.turn_context);
    session
        .spawn_task(Arc::clone(&turn), Vec::new(), ParentHandoffRecoveryTask)
        .await;
    session
        .persist_autonomous_completion_snapshot()
        .await
        .map_err(anyhow::Error::msg)?;
    let prepared = crate::session::completion_persistence::parent_handoff::runtime::prepare(
        &session,
        &turn,
        codex_analytics::CompactionPhase::MidTurn,
    )
    .await
    .map_err(anyhow::Error::msg)?
    .ok_or_else(|| anyhow::anyhow!("parent handoff was not prepared"))?;
    let record_path = std::fs::read_dir(handoff_directory)?
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("handoff.json"))
        .find(|path| path.is_file())
        .ok_or_else(|| anyhow::anyhow!("authenticated handoff record was not written"))?;
    crate::session::completion_persistence::parent_handoff::runtime::finish(
        &session,
        &turn,
        &prepared,
        "failed",
        Some("typed capacity fixture".into()),
    )
    .await
    .map_err(anyhow::Error::msg)?;
    let (_, successor, _, history) =
        crate::session::completion_persistence::parent_handoff::runtime::initial_history(
            &record_path,
            &turn.config,
        )
        .map_err(anyhow::Error::msg)?;
    Ok((session, successor, history, fixture._workspace))
}

async fn cold_restore_authenticated_handoff(
    successor_id: codex_protocol::ThreadId,
    history: Vec<RolloutItem>,
    handoff_directory: &std::path::Path,
) -> anyhow::Result<(Arc<Session>, Arc<TurnContext>, PathBuf)> {
    let (mut session, mut turn) = make_session_and_context().await;
    session.thread_id = successor_id;
    let config = Arc::make_mut(&mut turn.config);
    config.rencrow_compaction = true;
    config.rencrow_parent_handoff_dir =
        Some(codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(handoff_directory)?);
    let rollout_path = attach_reserved_cli_root_persistence(&mut session).await?;
    session.persist_rollout_items(&history).await;
    session
        .flush_rollout()
        .await
        .expect("authenticated import history persists before restore");
    session
        .record_initial_history(codex_history::InitialHistory::Resumed(
            codex_history::ResumedHistory {
                conversation_id: successor_id,
                history: Arc::new(history),
                rollout_path: Some(rollout_path.clone()),
            },
        ))
        .await;
    Ok((Arc::new(session), Arc::new(turn), rollout_path))
}

async fn prepare_successor_handoff(
    session: Arc<Session>,
    handoff_directory: &std::path::Path,
) -> anyhow::Result<(Arc<Session>, codex_protocol::ThreadId, Vec<RolloutItem>)> {
    let mut turn = session.new_default_turn().await;
    let turn_context = Arc::get_mut(&mut turn).expect("new default turn is uniquely owned");
    let config = Arc::make_mut(&mut turn_context.config);
    config.rencrow_compaction = true;
    config.rencrow_parent_handoff_dir =
        Some(codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(handoff_directory)?);
    session
        .spawn_task(Arc::clone(&turn), Vec::new(), ParentHandoffRecoveryTask)
        .await;
    session
        .persist_autonomous_completion_snapshot()
        .await
        .map_err(anyhow::Error::msg)?;
    let prepared = crate::session::completion_persistence::parent_handoff::runtime::prepare(
        &session,
        &turn,
        codex_analytics::CompactionPhase::MidTurn,
    )
    .await
    .map_err(anyhow::Error::msg)?
    .ok_or_else(|| anyhow::anyhow!("successor parent handoff was not prepared"))?;
    let record_path = std::fs::read_dir(handoff_directory)?
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("handoff.json"))
        .find(|path| path.is_file())
        .ok_or_else(|| anyhow::anyhow!("successor handoff record was not written"))?;
    crate::session::completion_persistence::parent_handoff::runtime::finish(
        &session,
        &turn,
        &prepared,
        "failed",
        Some("typed capacity fixture".into()),
    )
    .await
    .map_err(anyhow::Error::msg)?;
    let (_, successor, _, history) =
        crate::session::completion_persistence::parent_handoff::runtime::initial_history(
            &record_path,
            &turn.config,
        )
        .map_err(anyhow::Error::msg)?;
    Ok((session, successor, history))
}

#[tokio::test]
async fn changed_artifact_invalidates_satisfied_condition_during_reconcile() {
    let (session, _) = make_session_and_context().await;
    let workspace = tempfile::tempdir().expect("workspace");
    let artifact = workspace.path().join("artifact.txt");
    std::fs::write(&artifact, "verified bytes\n").unwrap();
    let paths = vec![PathBuf::from("artifact.txt")];
    let before = crate::task_check::hash_workspace_artifacts(workspace.path(), &paths).unwrap();
    let fingerprint = codex_history::compaction_candidate::digest(&json!({
        "task_id": "unit-1",
        "task_revision": 1,
        "workspace_root": workspace.path().to_string_lossy(),
        "artifacts": before.iter().map(|stamp| json!({
            "path": stamp.path.to_string_lossy(),
            "sha256": stamp.sha256,
        })).collect::<Vec<_>>(),
    }))
    .unwrap();
    session
        .begin_task_check(
            "accepted-check".into(),
            TaskEvidenceRequest {
                task_id: "unit-1".into(),
                task_revision: 1,
                artifact_paths: paths,
                check_kind: TaskCheckKind::Verification,
            },
            workspace.path().to_path_buf(),
            before.clone(),
        )
        .await;
    session
        .finish_task_check("accepted-check", 0, true, before)
        .await
        .expect("successful receipt");
    let evidence = CompletionEvidenceRef {
        task_id: "unit-1".into(),
        task_revision: 1,
        artifact_ref: format!("task-check:accepted-check:{fingerprint}"),
        verification_ref: format!("task-check:accepted-check:{fingerprint}"),
    };
    session.state.lock().await.rencrow_completion_state = Some((
        "root".into(),
        CompletionState {
            contract_revision: 1,
            units_completed_since_review: 1,
            plan_review_at: None,
            plan_reviews: Vec::new(),
            paused: false,
            consumed_host_events: Vec::new(),
            conditions: vec![
                AcceptanceCondition {
                    id: "AC-1".into(),
                    revision: 1,
                    result: "Artifact remains valid".into(),
                    scope: "workspace artifact".into(),
                    verification: "Run verification".into(),
                    acceptance_criteria: "Verification succeeds against current bytes".into(),
                    status: AcceptanceStatus::Satisfied,
                    evidence: vec![evidence.clone()],
                    unmet_reason: None,
                },
                AcceptanceCondition {
                    id: "AC-2".into(),
                    revision: 1,
                    result: "Another condition remains open".into(),
                    scope: "unrelated scope".into(),
                    verification: "Run another verification".into(),
                    acceptance_criteria: "Another result is accepted".into(),
                    status: AcceptanceStatus::Open,
                    evidence: Vec::new(),
                    unmet_reason: Some("unrelated reason stays intact".into()),
                },
            ],
            units: vec![CompletionUnit {
                id: "unit-1".into(),
                purpose: "Verify artifact".into(),
                status: UnitStatus::Done,
                kind: CompletionUnitKind::Execution,
                condition_refs: vec!["AC-1".into()],
                depends_on: Vec::new(),
                evidence: vec![evidence],
                task_refs: Vec::new(),
                review_target: None,
                review_evidence: None,
                review_launch: None,
                review_decision_reason: None,
                review_state: None,
                review_revalidation_of: None,
            }],
            current_unit: None,
            failed_approaches: Vec::new(),
            blocker: None,
            next_action: None,
            product_progress_at: Some(100),
            runtime_recovery_at: Some(110),
            activity_at: Some(120),
            common_harness: None,
        },
    ));

    session
        .persist_autonomous_completion_snapshot()
        .await
        .expect("persist satisfied snapshot with its host receipt");
    let snapshot = session
        .state
        .lock()
        .await
        .history
        .annotated_items()
        .iter()
        .rev()
        .find_map(|item| {
            item.metadata
                .as_ref()
                .and_then(|metadata| metadata.rencrow_completion.clone())
        })
        .expect("host snapshot persisted");
    let task_boundary = accepted_task_boundary_item(
        &session.thread_id().to_string(),
        &format!("task-check:accepted-check:{fingerprint}"),
    );
    {
        let mut state = session.state.lock().await;
        state.rencrow_completion_state = None;
        state.task_check_ledger = crate::task_check::TaskCheckLedger::default();
    }
    session
        .restore_autonomous_completion_snapshot(&[task_boundary, snapshot_item(snapshot)])
        .await;
    assert_eq!(
        session
            .autonomous_completion_current_state()
            .await
            .expect("restored state")
            .1
            .conditions[0]
            .status,
        AcceptanceStatus::Satisfied,
        "cold restore retains the completed contract before artifact reconciliation"
    );

    let original = session
        .autonomous_completion_current_state()
        .await
        .unwrap()
        .1;
    assert!(
        session
            .completion_evidence_matches_current_artifact(
                &session
                    .autonomous_completion_current_state()
                    .await
                    .unwrap()
                    .1
                    .conditions[0]
                    .evidence[0]
            )
            .await
    );
    let history_count = session.state.lock().await.history.annotated_items().len();
    assert!(
        !session
            .reconcile_rencrow_completion_evidence("root")
            .await
            .expect("unchanged artifact reconcile")
    );
    assert_eq!(
        session
            .autonomous_completion_current_state()
            .await
            .unwrap()
            .1,
        original,
        "a current completed contract stays idle"
    );
    assert_eq!(
        original.conditions[1].unmet_reason.as_deref(),
        Some("unrelated reason stays intact")
    );
    assert_eq!(
        session.state.lock().await.history.annotated_items().len(),
        history_count
    );

    // Model a cold restored contract, then change the real artifact after its saved receipt.
    std::fs::write(&artifact, "changed bytes\n").unwrap();
    assert!(
        session
            .reconcile_rencrow_completion_evidence("root")
            .await
            .expect("host reconcile")
    );
    let state = session
        .autonomous_completion_current_state()
        .await
        .unwrap()
        .1;
    assert_eq!(state.conditions[0].status, AcceptanceStatus::Open);
    assert_eq!(
        state.conditions[0].evidence,
        original.conditions[0].evidence
    );
    assert!(state.conditions[0].unmet_reason.is_some());
    assert!(
        state
            .next_action
            .as_deref()
            .is_some_and(|action| action.contains("AC-1"))
    );
    assert!(
        state
            .next_action
            .as_deref()
            .is_some_and(|action| action.contains("unit-1@1"))
    );
    assert!(
        state
            .next_action
            .as_deref()
            .is_some_and(|action| action.contains("reverify-AC-1-"))
    );
    assert_eq!(state.units[0].status, UnitStatus::Done);
    assert_eq!(state.product_progress_at, Some(100));
    assert_eq!(state.runtime_recovery_at, Some(110));
    assert_eq!(state.activity_at, Some(120));
    assert_eq!(state.conditions[1], original.conditions[1]);
    assert!(session.autonomous_completion_pending().await);
    assert!(session.state.lock().await.history.annotated_items().len() > history_count);

    let blocker = CompletionBlocker {
        category: BlockerCategory::Authority,
        reason: "Existing authority blocker".into(),
        required_input: None,
        authority: Some("project owner".into()),
        evidence: Vec::new(),
        recheck_condition: None,
        review_owner: None,
    };
    let mut blocked_contract = original.clone();
    blocked_contract.blocker = Some(blocker.clone());
    session.state.lock().await.rencrow_completion_state = Some(("root".into(), blocked_contract));
    std::fs::write(&artifact, "changed again\n").unwrap();
    assert!(
        session
            .reconcile_rencrow_completion_evidence("root")
            .await
            .expect("reconcile preserves an existing blocker")
    );
    let blocked = session
        .autonomous_completion_current_state()
        .await
        .unwrap()
        .1;
    assert_eq!(blocked.conditions[0].status, AcceptanceStatus::Open);
    assert_eq!(blocked.blocker, Some(blocker));
    assert_eq!(blocked.product_progress_at, Some(100));
    assert_eq!(blocked.runtime_recovery_at, Some(110));
    assert_eq!(blocked.activity_at, Some(120));
    assert!(!session.autonomous_completion_pending().await);

    session.pause_autonomous_completion().await;
    assert!(!session.autonomous_completion_pending().await);
    let paused = session
        .autonomous_completion_current_state()
        .await
        .unwrap()
        .1;
    assert!(paused.paused);
    assert_eq!(paused.blocker, blocked.blocker);
    assert_eq!(paused.conditions[0].status, AcceptanceStatus::Open);
    assert_eq!(paused.units[0].status, UnitStatus::Done);
    assert_eq!(paused.product_progress_at, Some(100));
}

fn accepted_task_boundary_item(thread_id: &str, evidence_ref: &str) -> RolloutItem {
    let boundary = BoundaryCandidate {
        version: 1,
        mode: BoundaryMode::Task,
        thread_id: thread_id.into(),
        root_task_id: "root".into(),
        closed_task_id: "unit-1".into(),
        revision: 2,
        closed_task_revision: 1,
        goal: "verify the workspace artifact".into(),
        tasks: vec![TaskState {
            id: "unit-1".into(),
            purpose: "verify artifact".into(),
            status: TaskStatus::Completed,
            depends_on: Vec::new(),
        }],
        current_task_id: None,
        artifact_ref: evidence_ref.into(),
        verification_ref: evidence_ref.into(),
        decisions: Vec::new(),
        constraints: Vec::new(),
        completion_state: None,
    };
    let candidate_hash = codex_history::archive_reference::content_sha256(
        &serde_json::to_string(&boundary).unwrap(),
    );
    let task_state = StoredTaskState {
        boundary,
        completed: BTreeMap::from([(
            "unit-1".into(),
            AcceptedTaskReceipt {
                revision: 1,
                result: "artifact verification completed".into(),
                artifact_ref: evidence_ref.into(),
                verification_ref: evidence_ref.into(),
            },
        )]),
        accepted_history: vec![],
        parent_review_ref: "review:unit-1".into(),
        previous_revision: None,
        previous_state_hash: None,
        candidate_hash,
        boundary_nonce: "nonce-unit-1".into(),
    };
    task_state
        .validate(thread_id)
        .expect("valid accepted task state");

    let summary_text = "accepted task boundary";
    let mut envelope = ResponseItemEnvelope::new(codex_protocol::models::ResponseItem::Message {
        id: None,
        role: "user".into(),
        content: vec![codex_protocol::models::ContentItem::InputText {
            text: summary_text.into(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: Some(
            codex_protocol::models::InternalChatMessageMetadataPassthrough {
                content_item_kinds: Some(vec![codex_protocol::models::ContentItemKind(
                    "compaction.summary".into(),
                )]),
                ..Default::default()
            },
        ),
    });
    let checkpoint = RenCrowTaskCheckpointMetadataV3::task_boundary(
        summary_text,
        serde_json::to_value(task_state).unwrap(),
        TaskBoundarySourceFence {
            history_hash: "a".repeat(64),
            removed_refs: Vec::new(),
        },
    );
    envelope.metadata.get_or_insert_default().rencrow_compaction =
        Some(serde_json::to_value(checkpoint).unwrap());
    RolloutItem::ResponseItem(envelope)
}

fn snapshot_item(value: serde_json::Value) -> RolloutItem {
    let item = Box::new(crate::context::AutonomousCompletionSnapshot).into_boxed_response_item();
    RolloutItem::ResponseItem(ResponseItemEnvelope {
        item,
        metadata: Some(CodexHarnessMetadata {
            rencrow_completion: Some(value),
            ..Default::default()
        }),
    })
}

struct LaunchRestoreFixture {
    session: Session,
    turn_context: crate::session::turn_context::TurnContext,
    _workspace: tempfile::TempDir,
    history: Vec<RolloutItem>,
}

async fn latest_completion_snapshot(session: &Session) -> serde_json::Value {
    session
        .state
        .lock()
        .await
        .history
        .annotated_items()
        .iter()
        .rev()
        .find_map(|item| {
            item.metadata
                .as_ref()
                .and_then(|metadata| metadata.rencrow_completion.clone())
        })
        .expect("host completion snapshot is persisted")
}

fn go_command() -> Vec<String> {
    vec![
        "/bin/zsh".into(),
        "-lc".into(),
        "go vet ./internal/portal".into(),
    ]
}

fn go_completion_event(
    thread_id: codex_protocol::ThreadId,
    call_id: &str,
    cwd: &std::path::Path,
    success: bool,
    output: &str,
) -> RolloutItem {
    RolloutItem::EventMsg(codex_protocol::protocol::EventMsg::ItemCompleted(
        codex_protocol::protocol::ItemCompletedEvent {
            thread_id,
            turn_id: "restore-launch-test-turn".into(),
            item: codex_protocol::items::TurnItem::CommandExecution(
                codex_protocol::items::CommandExecutionItem {
                    model_context: None,
                    id: call_id.into(),
                    plugin_id: None,
                    script_path: None,
                    process_id: Some(format!("process-{call_id}")),
                    command: go_command(),
                    cwd: codex_utils_path_uri::PathUri::from_abs_path(&cwd.abs()),
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
                },
            ),
            started_at_ms: Some(1),
            completed_at_ms: 2,
        },
    ))
}

async fn record_go_check(
    session: &Session,
    call_id: &str,
    workspace_root: &std::path::Path,
    artifact_paths: Vec<PathBuf>,
    scope: &crate::state::ActiveTaskScope,
    success: bool,
) -> RolloutItem {
    let canonical_workspace_root = workspace_root
        .canonicalize()
        .expect("canonical verification workspace");
    let workspace_root = canonical_workspace_root.as_path();
    let before = crate::task_check::hash_workspace_artifacts(workspace_root, &artifact_paths)
        .expect("verification fixture artifacts can be hashed");
    let obligation = crate::task_check::TaskCheckObligation::new(
        scope.clone(),
        workspace_root.to_path_buf(),
        "go vet ./internal/portal",
        artifact_paths.clone(),
    )
    .expect("verification obligation is valid");
    session
        .begin_task_check_with_obligation(
            call_id.into(),
            TaskEvidenceRequest {
                task_id: scope.task_id.clone(),
                task_revision: scope.task_revision,
                artifact_paths,
                check_kind: TaskCheckKind::Verification,
            },
            workspace_root.to_path_buf(),
            before.clone(),
            Some(obligation),
        )
        .await;
    let diagnostic = if success {
        ""
    } else {
        "go: go.mod file not found in current directory or any parent directory; see 'go help modules'\n"
    };
    let observation = (!success).then(|| {
        crate::task_check::TaskCheckObservation::from_execution(
            call_id,
            &go_command(),
            workspace_root,
            diagnostic,
            1,
            true,
            false,
        )
    });
    session
        .finish_task_check_observed(
            call_id,
            if success { 0 } else { 1 },
            true,
            before,
            observation,
        )
        .await
        .expect("host verification receipt is recorded");
    go_completion_event(
        session.thread_id(),
        call_id,
        workspace_root,
        success,
        diagnostic,
    )
}

async fn record_live_go_success(
    session: &Session,
    turn_context: &crate::session::turn_context::TurnContext,
    workspace_root: &std::path::Path,
    call_id: &str,
) {
    let canonical_workspace_root = workspace_root
        .canonicalize()
        .expect("canonical live verification workspace");
    let workspace_root = canonical_workspace_root.as_path();
    let artifact_paths = ["internal/portal/action.go", "internal/portal/server.go"]
        .map(PathBuf::from)
        .to_vec();
    let before = crate::task_check::hash_workspace_artifacts(workspace_root, &artifact_paths)
        .expect("corrected target artifacts remain available");
    let scope = crate::state::ActiveTaskScope {
        root_task_id: "root".into(),
        task_id: "task-a".into(),
        task_revision: 1,
        begin_call_id: "begin-task-a".into(),
        start_history_cursor: 0,
    };
    let obligation = crate::task_check::TaskCheckObligation::new(
        scope,
        workspace_root.to_path_buf(),
        "go vet ./internal/portal",
        artifact_paths.clone(),
    )
    .expect("corrected target uses the original execution scope");
    session
        .begin_task_check_with_obligation(
            call_id.into(),
            TaskEvidenceRequest {
                task_id: "task-a".into(),
                task_revision: 1,
                artifact_paths,
                check_kind: TaskCheckKind::Verification,
            },
            workspace_root.to_path_buf(),
            before.clone(),
            Some(obligation),
        )
        .await;
    let target_item = codex_protocol::items::TurnItem::CommandExecution(
        codex_protocol::items::CommandExecutionItem {
            model_context: None,
            id: call_id.into(),
            plugin_id: None,
            script_path: None,
            process_id: Some(format!("process-{call_id}")),
            command: go_command(),
            cwd: codex_utils_path_uri::PathUri::from_abs_path(&workspace_root.abs()),
            parsed_cmd: Vec::new(),
            source: codex_protocol::protocol::ExecCommandSource::UnifiedExecStartup,
            interaction_input: None,
            status: codex_protocol::items::CommandExecutionStatus::Completed,
            stdout: Some(String::new()),
            stderr: Some(String::new()),
            aggregated_output: Some(String::new()),
            exit_code: Some(0),
            duration: Some(std::time::Duration::from_millis(1)),
            formatted_output: Some(String::new()),
        },
    );
    session
        .emit_turn_item_completed(turn_context, target_item)
        .await;
    session.observe_task_check_terminal_event(call_id).await;
    let target_observation = crate::task_check::TaskCheckObservation::from_execution(
        call_id,
        &go_command(),
        workspace_root,
        "",
        0,
        true,
        false,
    );
    session
        .finish_task_check_observed(call_id, 0, true, before, Some(target_observation))
        .await
        .expect("newly emitted target completion is recorded");
}

fn snapshot_check_status(snapshot_item: &RolloutItem, call_id: &str) -> serde_json::Value {
    let RolloutItem::ResponseItem(envelope) = snapshot_item else {
        panic!("expected host snapshot item");
    };
    envelope
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.rencrow_completion.as_ref())
        .and_then(|snapshot| snapshot["task_checks"].as_array())
        .and_then(|checks| {
            checks
                .iter()
                .find(|entry| entry["attempt"]["call_id"] == call_id)
        })
        .map(|entry| entry["attempt"]["status"].clone())
        .expect("snapshot retains the expected verification receipt")
}

fn snapshot_has_check(snapshot_item: &RolloutItem, call_id: &str) -> bool {
    let RolloutItem::ResponseItem(envelope) = snapshot_item else {
        return false;
    };
    envelope
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.rencrow_completion.as_ref())
        .and_then(|snapshot| snapshot["task_checks"].as_array())
        .is_some_and(|checks| {
            checks
                .iter()
                .any(|entry| entry["attempt"]["call_id"] == call_id)
        })
}

async fn launch_restore_fixture() -> LaunchRestoreFixture {
    let (session, turn_context) = make_session_and_context().await;
    session
        .set_rencrow_completion_state("root".into(), external_blocked_state())
        .await
        .expect("install a valid completion contract for durable snapshots");

    let workspace = tempfile::tempdir().expect("workspace");
    let source_root = workspace.path();
    let target_root = source_root.join("RenCrow_PORTAL");
    let early_root = source_root.join("unrelated_early");
    let latest_root = source_root.join("unrelated_latest");
    let artifact_names = ["internal/portal/action.go", "internal/portal/server.go"];
    for root in [&target_root, &early_root, &latest_root] {
        for name in artifact_names {
            let path = root.join(name);
            std::fs::create_dir_all(path.parent().expect("artifact parent")).unwrap();
            std::fs::write(path, "unchanged verification fixture\n").unwrap();
        }
    }
    std::fs::write(target_root.join("go.mod"), "module example.test/portal\n").unwrap();

    let scope = crate::state::ActiveTaskScope {
        root_task_id: "root".into(),
        task_id: "task-a".into(),
        task_revision: 1,
        begin_call_id: "begin-task-a".into(),
        start_history_cursor: 0,
    };
    let source_paths = artifact_names
        .map(|name| PathBuf::from("RenCrow_PORTAL").join(name))
        .to_vec();
    let scoped_paths = artifact_names.map(PathBuf::from).to_vec();
    let source_event = record_go_check(
        &session,
        "failed-go-launch",
        source_root,
        source_paths,
        &scope,
        false,
    )
    .await;
    let source_snapshot = latest_completion_snapshot(&session).await;

    let early_event = record_go_check(
        &session,
        "earliest-unrelated-success",
        &early_root,
        scoped_paths.clone(),
        &scope,
        true,
    )
    .await;
    let early_receipt = session
        .state
        .lock()
        .await
        .task_check_ledger
        .get("earliest-unrelated-success")
        .cloned()
        .expect("the unrelated successful receipt is recorded");
    let early_fingerprint = codex_history::compaction_candidate::digest(&json!({
        "task_id": early_receipt.task_id,
        "task_revision": early_receipt.task_revision,
        "workspace_root": early_receipt.workspace_root.to_string_lossy(),
        "artifacts": early_receipt.artifact_after.iter().map(|stamp| json!({
            "path": stamp.path.to_string_lossy(),
            "sha256": stamp.sha256,
        })).collect::<Vec<_>>(),
    }))
    .expect("accepted evidence fingerprint is canonical");
    {
        let mut state = session.state.lock().await;
        let (_, completion) = state
            .rencrow_completion_state
            .as_mut()
            .expect("completion blocker remains installed");
        completion
            .blocker
            .as_mut()
            .expect("external blocker remains installed")
            .evidence
            .push(CompletionEvidenceRef {
                task_id: "task-a".into(),
                task_revision: 1,
                artifact_ref: format!("task-check:earliest-unrelated-success:{early_fingerprint}"),
                verification_ref: format!(
                    "task-check:earliest-unrelated-success:{early_fingerprint}"
                ),
            });
    }
    let early_snapshot = latest_completion_snapshot(&session).await;

    let target_event = record_go_check(
        &session,
        "corrected-go-success",
        &target_root,
        scoped_paths.clone(),
        &scope,
        true,
    )
    .await;
    let target_snapshot = latest_completion_snapshot(&session).await;

    let latest_event = record_go_check(
        &session,
        "latest-unrelated-success",
        &latest_root,
        scoped_paths,
        &scope,
        true,
    )
    .await;
    let latest_snapshot = latest_completion_snapshot(&session).await;

    LaunchRestoreFixture {
        session,
        turn_context,
        _workspace: workspace,
        history: vec![
            source_event,
            snapshot_item(source_snapshot),
            early_event,
            snapshot_item(early_snapshot),
            target_event,
            snapshot_item(target_snapshot),
            latest_event,
            snapshot_item(latest_snapshot),
        ],
    }
}

async fn failed_launch_handoff_fixture() -> LaunchRestoreFixture {
    let (session, turn_context) = make_session_and_context().await;
    session
        .set_rencrow_completion_state("root".into(), external_blocked_state())
        .await
        .expect("install a valid completion contract for parent handoff");
    let workspace = tempfile::tempdir().expect("workspace");
    let source_root = workspace.path();
    let target_root = source_root.join("RenCrow_PORTAL");
    let artifact_names = ["internal/portal/action.go", "internal/portal/server.go"];
    for name in artifact_names {
        let path = target_root.join(name);
        std::fs::create_dir_all(path.parent().expect("artifact parent")).unwrap();
        std::fs::write(path, "unchanged handoff fixture\n").unwrap();
    }
    std::fs::write(target_root.join("go.mod"), "module example.test/portal\n").unwrap();
    let scope = crate::state::ActiveTaskScope {
        root_task_id: "root".into(),
        task_id: "task-a".into(),
        task_revision: 1,
        begin_call_id: "begin-task-a".into(),
        start_history_cursor: 0,
    };
    let source_paths = artifact_names
        .map(|name| PathBuf::from("RenCrow_PORTAL").join(name))
        .to_vec();
    let source_event = record_go_check(
        &session,
        "failed-go-launch",
        source_root,
        source_paths,
        &scope,
        false,
    )
    .await;
    let source_snapshot = latest_completion_snapshot(&session).await;
    LaunchRestoreFixture {
        session,
        turn_context,
        _workspace: workspace,
        history: vec![source_event, snapshot_item(source_snapshot)],
    }
}

#[tokio::test]
async fn cold_restore_selects_matching_go_correction_from_an_older_snapshot() {
    let fixture = launch_restore_fixture().await;
    let expected_target_status = snapshot_check_status(&fixture.history[5], "corrected-go-success");
    let expected_latest_status =
        snapshot_check_status(&fixture.history[7], "latest-unrelated-success");
    let expected_accepted_nonlatest_status =
        snapshot_check_status(&fixture.history[7], "earliest-unrelated-success");
    let expected_source_status = snapshot_check_status(&fixture.history[7], "failed-go-launch");
    assert_eq!(expected_source_status["Finished"]["succeeded"], false);
    assert_eq!(expected_latest_status["Finished"]["succeeded"], true);
    assert!(snapshot_has_check(
        &fixture.history[5],
        "corrected-go-success"
    ));
    assert!(!snapshot_has_check(
        &fixture.history[7],
        "corrected-go-success"
    ));
    let session = fixture.session;
    {
        let mut state = session.state.lock().await;
        state.rencrow_completion_state = None;
        state.task_check_ledger = crate::task_check::TaskCheckLedger::default();
    }

    session
        .restore_autonomous_completion_snapshot(&fixture.history)
        .await;

    let restored_rows = session.state.lock().await.task_check_ledger.snapshot();
    let source = restored_rows
        .iter()
        .find(|entry| entry.attempt.call_id == "failed-go-launch")
        .expect("the original failed verification remains retained");
    assert_eq!(
        source.attempt.resolved_by.as_deref(),
        Some("corrected-go-success"),
        "restore skips an earlier same-task success whose artifact root does not match"
    );
    let correction = source
        .attempt
        .launch_correction
        .as_ref()
        .expect("the failed launch retains its correction proof");
    assert_eq!(correction.proof_ref, "failed-go-launch");
    assert_eq!(correction.target_call_id, "corrected-go-success");
    assert_eq!(correction.target_output_ref, "corrected-go-success");
    assert_eq!(correction.target_output_sha256.len(), 64);

    let target = restored_rows
        .iter()
        .find(|entry| entry.attempt.call_id == "corrected-go-success")
        .expect("the success receipt referenced by the proof remains in the ledger");
    let crate::task_check::TaskCheckAttemptStatus::Finished(target_receipt) =
        &target.attempt.status
    else {
        panic!("the corrected target remains a finished receipt");
    };
    assert!(target_receipt.succeeded);
    assert_eq!(target_receipt.call_id, correction.target_call_id);
    assert_eq!(target_receipt.workspace_root, correction.target_cwd);
    assert!(!target_receipt.artifact_before.is_empty());
    assert_eq!(
        target_receipt.artifact_before,
        target_receipt.artifact_after
    );
    assert_eq!(
        serde_json::to_value(target_receipt).expect("serialize retained target receipt"),
        expected_target_status["Finished"],
        "the older successful target's receipt and digest stamps remain byte-for-byte equivalent"
    );

    let latest = restored_rows
        .iter()
        .find(|entry| entry.attempt.call_id == "latest-unrelated-success")
        .expect("the newest unrelated success receipt is preserved");
    assert!(matches!(
        &latest.attempt.status,
        crate::task_check::TaskCheckAttemptStatus::Finished(receipt)
            if receipt.succeeded && receipt.call_id == "latest-unrelated-success"
    ));
    let crate::task_check::TaskCheckAttemptStatus::Finished(latest_receipt) =
        &latest.attempt.status
    else {
        panic!("the latest unrelated success remains finished");
    };
    assert_eq!(
        serde_json::to_value(latest_receipt).expect("serialize unrelated receipt"),
        expected_latest_status["Finished"],
        "the latest unrelated successful receipt remains byte-for-byte equivalent"
    );
    let accepted_nonlatest_receipt = session
        .state
        .lock()
        .await
        .task_check_ledger
        .get("earliest-unrelated-success")
        .cloned()
        .expect("the accepted nonlatest receipt remains referenced after correction");
    assert!(accepted_nonlatest_receipt.succeeded);
    assert_eq!(
        serde_json::to_value(accepted_nonlatest_receipt)
            .expect("serialize accepted nonlatest receipt"),
        expected_accepted_nonlatest_status["Finished"],
        "reconciliation must retain accepted nonlatest evidence byte-for-byte"
    );

    session
        .persist_autonomous_completion_snapshot()
        .await
        .expect("resnapshot retains the resolved proof closure");
    let resnapshot = latest_completion_snapshot(&session).await;
    let serialized_checks = resnapshot["task_checks"]
        .as_array()
        .expect("resnapshot contains verification receipts");
    assert!(serialized_checks.iter().any(|entry| {
        entry["attempt"]["call_id"] == "failed-go-launch"
            && entry["attempt"]["resolved_by"] == "corrected-go-success"
    }));
    assert!(
        serialized_checks
            .iter()
            .any(|entry| entry["attempt"]["call_id"] == "corrected-go-success")
    );
    assert!(
        serialized_checks
            .iter()
            .any(|entry| entry["attempt"]["call_id"] == "latest-unrelated-success")
    );
    assert!(serialized_checks.iter().any(|entry| {
        entry["attempt"]["call_id"] == "earliest-unrelated-success"
            && entry["attempt"]["status"]["Finished"]["succeeded"] == true
    }));

    let mut second_restore_history = fixture.history;
    second_restore_history.push(snapshot_item(resnapshot));
    {
        let mut state = session.state.lock().await;
        state.rencrow_completion_state = None;
        state.task_check_ledger = crate::task_check::TaskCheckLedger::default();
    }
    session
        .restore_autonomous_completion_snapshot(&second_restore_history)
        .await;
    let second_rows = session.state.lock().await.task_check_ledger.snapshot();
    let source = second_rows
        .iter()
        .find(|entry| entry.attempt.call_id == "failed-go-launch")
        .expect("the source failure survives another cold restore");
    assert_eq!(
        source.attempt.resolved_by.as_deref(),
        Some("corrected-go-success")
    );
    assert!(
        second_rows
            .iter()
            .any(|entry| entry.attempt.call_id == "corrected-go-success")
    );
    assert!(
        second_rows
            .iter()
            .any(|entry| entry.attempt.call_id == "latest-unrelated-success")
    );
    assert!(
        session
            .state
            .lock()
            .await
            .task_check_ledger
            .get("earliest-unrelated-success")
            .is_some()
    );
}

#[tokio::test]
async fn cold_restore_requires_target_event_then_recovers_on_later_live_success() {
    let fixture = launch_restore_fixture().await;
    let session = fixture.session;
    let turn_context = fixture.turn_context;
    let workspace_root = fixture._workspace.path().join("RenCrow_PORTAL");
    let history = fixture
        .history
        .into_iter()
        .filter(|item| {
            !matches!(item,
                RolloutItem::EventMsg(codex_protocol::protocol::EventMsg::ItemCompleted(event))
                    if matches!(&event.item,
                        codex_protocol::items::TurnItem::CommandExecution(command)
                            if command.id == "corrected-go-success"))
        })
        .collect::<Vec<_>>();
    {
        let mut state = session.state.lock().await;
        state.rencrow_completion_state = None;
        state.task_check_ledger = crate::task_check::TaskCheckLedger::default();
    }

    session
        .restore_autonomous_completion_snapshot(&history)
        .await;

    {
        let state = session.state.lock().await;
        let source = state
            .task_check_ledger
            .attempt_for_call("failed-go-launch")
            .expect("the source failure remains available for review");
        assert!(source.resolved_by.is_none());
        assert!(source.launch_correction.is_none());
    }

    record_live_go_success(
        &session,
        &turn_context,
        &workspace_root,
        "live-corrected-go-success",
    )
    .await;
    let state = session.state.lock().await;
    let source = state
        .task_check_ledger
        .attempt_for_call("failed-go-launch")
        .expect("restored source failure remains in the ledger");
    assert_eq!(
        source.resolved_by.as_deref(),
        Some("live-corrected-go-success"),
        "restored source event order allows a later live success to correct the launch"
    );
}

#[tokio::test]
async fn cold_restore_does_not_seed_ambiguous_source_event_order() {
    let fixture = launch_restore_fixture().await;
    let session = fixture.session;
    let turn_context = fixture.turn_context;
    let workspace_root = fixture._workspace.path().join("RenCrow_PORTAL");
    let mut history = fixture.history;
    let duplicate_source_event = history[0].clone();
    history.insert(1, duplicate_source_event);
    {
        let mut state = session.state.lock().await;
        state.rencrow_completion_state = None;
        state.task_check_ledger = crate::task_check::TaskCheckLedger::default();
    }

    session
        .restore_autonomous_completion_snapshot(&history)
        .await;
    record_live_go_success(
        &session,
        &turn_context,
        &workspace_root,
        "live-after-ambiguous-source",
    )
    .await;

    let state = session.state.lock().await;
    let source = state
        .task_check_ledger
        .attempt_for_call("failed-go-launch")
        .expect("ambiguous source failure remains reviewable");
    assert!(source.resolved_by.is_none());
    assert!(source.launch_correction.is_none());
}

#[tokio::test]
async fn cold_restore_keeps_launch_unresolved_when_target_event_precedes_failure() {
    let fixture = launch_restore_fixture().await;
    let session = fixture.session;
    let mut history = fixture.history;
    let target_event = history.remove(4);
    history.insert(0, target_event);
    {
        let mut state = session.state.lock().await;
        state.rencrow_completion_state = None;
        state.task_check_ledger = crate::task_check::TaskCheckLedger::default();
    }

    session
        .restore_autonomous_completion_snapshot(&history)
        .await;

    let state = session.state.lock().await;
    let source = state
        .task_check_ledger
        .attempt_for_call("failed-go-launch")
        .expect("the source failure remains reviewable");
    assert!(source.resolved_by.is_none());
    assert!(source.launch_correction.is_none());
}

#[tokio::test]
async fn authenticated_parent_handoff_preserves_and_revalidates_imported_launch_lineage()
-> anyhow::Result<()> {
    let fixture = failed_launch_handoff_fixture().await;
    let workspace_root = fixture._workspace.path().join("RenCrow_PORTAL");
    let first_handoff_directory = tempfile::tempdir().expect("first handoff directory");
    let (source_session, successor_id, imported_history, _workspace) =
        prepare_authenticated_handoff(fixture, first_handoff_directory.path()).await?;
    let imported_projection =
        crate::session::completion_persistence::parent_handoff::runtime::restore_seed(
            &imported_history,
            successor_id,
        )
        .map_err(anyhow::Error::msg)?
        .expect("successor history has an authenticated parent import");
    let first_projection_sha256 =
        crate::session::completion_persistence::maintenance_snapshot_digest(
            &imported_projection.snapshot,
        )
        .map_err(anyhow::Error::msg)?;
    assert!(imported_history.iter().all(|item| !matches!(
        item,
        RolloutItem::EventMsg(codex_protocol::protocol::EventMsg::ItemCompleted(event))
            if matches!(&event.item,
                codex_protocol::items::TurnItem::CommandExecution(command)
                    if command.id == "failed-go-launch" || command.id == "handoff-corrected-go-success")
    )));

    let (successor, successor_turn, successor_rollout_path) = cold_restore_authenticated_handoff(
        successor_id,
        imported_history.clone(),
        first_handoff_directory.path(),
    )
    .await?;
    {
        let state = successor.state.lock().await;
        let source = state
            .task_check_ledger
            .attempt_for_call("failed-go-launch")
            .expect("authenticated imported launch failure is retained");
        assert!(source.resolved_by.is_none());
        assert!(source.launch_correction.is_none());
    }

    record_live_go_success(
        &successor,
        &successor_turn,
        &workspace_root,
        "handoff-corrected-go-success",
    )
    .await;
    let expected_correction = {
        let state = successor.state.lock().await;
        let source = state
            .task_check_ledger
            .attempt_for_call("failed-go-launch")
            .expect("imported failure remains in the ledger");
        assert_eq!(
            source.resolved_by.as_deref(),
            Some("handoff-corrected-go-success")
        );
        let correction = source
            .launch_correction
            .as_ref()
            .expect("successor-local event supplies a correction proof");
        assert_eq!(
            correction.source_handoff_projection_sha256.as_deref(),
            Some(first_projection_sha256.as_str())
        );
        assert_eq!(correction.source_event_ordinal, None);
        assert_eq!(correction.target_event_ordinal, None);
        serde_json::to_value(correction).expect("serialize successor correction")
    };
    successor
        .persist_autonomous_completion_snapshot()
        .await
        .map_err(anyhow::Error::msg)?;
    successor
        .flush_rollout()
        .await
        .expect("successor correction and native event persist");
    let codex_history::InitialHistory::Resumed(persisted_successor) =
        crate::rollout::recorder::RolloutRecorder::get_rollout_history(&successor_rollout_path)
            .await
            .expect("read persisted successor rollout")
    else {
        panic!("expected persisted successor rollout");
    };
    assert!(persisted_successor.history.iter().any(|item| matches!(
        item,
        RolloutItem::EventMsg(codex_protocol::protocol::EventMsg::ItemCompleted(event))
            if matches!(&event.item,
                codex_protocol::items::TurnItem::CommandExecution(command)
                    if command.id == "handoff-corrected-go-success")
    )));
    {
        let mut state = successor.state.lock().await;
        state.rencrow_completion_state = None;
        state.task_check_ledger = crate::task_check::TaskCheckLedger::default();
    }
    successor
        .record_initial_history(codex_history::InitialHistory::Resumed(
            codex_history::ResumedHistory {
                conversation_id: successor_id,
                history: persisted_successor.history.clone(),
                rollout_path: Some(successor_rollout_path.clone()),
            },
        ))
        .await;
    {
        let state = successor.state.lock().await;
        let source = state
            .task_check_ledger
            .attempt_for_call("failed-go-launch")
            .expect("cold replay retains the imported source failure");
        assert_eq!(
            source.resolved_by.as_deref(),
            Some("handoff-corrected-go-success")
        );
        assert_eq!(
            serde_json::to_value(source.launch_correction.as_ref().unwrap())
                .expect("serialize cold-restored correction"),
            expected_correction,
            "the exact projection hash and missing native ordinals survive persisted cold replay"
        );
    }

    let second_handoff_directory = tempfile::tempdir().expect("second handoff directory");
    let (successor_source_session, second_successor_id, second_import_history) =
        prepare_successor_handoff(Arc::clone(&successor), second_handoff_directory.path()).await?;
    assert!(second_import_history.iter().all(|item| !matches!(
        item,
        RolloutItem::EventMsg(codex_protocol::protocol::EventMsg::ItemCompleted(event))
            if matches!(&event.item,
                codex_protocol::items::TurnItem::CommandExecution(command)
                    if command.id == "failed-go-launch" || command.id == "handoff-corrected-go-success")
    )));
    let second_projection =
        crate::session::completion_persistence::parent_handoff::runtime::restore_seed(
            &second_import_history,
            second_successor_id,
        )
        .map_err(anyhow::Error::msg)?
        .expect("second verified handoff remains authenticated");
    let second_projection_sha256 =
        crate::session::completion_persistence::maintenance_snapshot_digest(
            &second_projection.snapshot,
        )
        .map_err(anyhow::Error::msg)?;
    assert_ne!(second_projection_sha256, first_projection_sha256);
    let (second_successor, _second_turn, _second_rollout_path) =
        cold_restore_authenticated_handoff(
            second_successor_id,
            second_import_history.clone(),
            second_handoff_directory.path(),
        )
        .await?;
    {
        let state = second_successor.state.lock().await;
        let source = state
            .task_check_ledger
            .attempt_for_call("failed-go-launch")
            .expect("second successor preserves the original failed call");
        assert_eq!(
            source.resolved_by.as_deref(),
            Some("handoff-corrected-go-success")
        );
        let correction = source
            .launch_correction
            .as_ref()
            .expect("second successor preserves the imported correction pair");
        assert_eq!(
            correction.source_handoff_projection_sha256.as_deref(),
            Some(first_projection_sha256.as_str()),
            "a later verified handoff retains the original projection lineage"
        );
        assert_eq!(correction.source_event_ordinal, None);
        assert_eq!(correction.target_event_ordinal, None);
    }

    let mut mutated_import = second_import_history.clone();
    let mutated = mutated_import.iter_mut().any(|item| {
        let RolloutItem::ResponseItem(envelope) = item else {
            return false;
        };
        let Some(snapshot) = envelope
            .metadata
            .as_mut()
            .and_then(|metadata| metadata.rencrow_completion.as_mut())
        else {
            return false;
        };
        let Some(checks) = snapshot["task_checks"].as_array_mut() else {
            return false;
        };
        let Some(source) = checks
            .iter_mut()
            .find(|entry| entry["attempt"]["call_id"] == "failed-go-launch")
        else {
            return false;
        };
        source["attempt"]["launch_correction"]["target_call_id"] =
            serde_json::Value::String("unknown-imported-target".into());
        true
    });
    assert!(
        mutated,
        "the projected snapshot contains the source correction"
    );
    let (mutated_successor, _, _) = cold_restore_authenticated_handoff(
        second_successor_id,
        mutated_import,
        second_handoff_directory.path(),
    )
    .await?;
    {
        let state = mutated_successor.state.lock().await;
        assert!(
            state
                .task_check_ledger
                .attempt_for_call("failed-go-launch")
                .is_none_or(|source| source.resolved_by.is_none())
        );
    }

    let mut ambiguous_import = second_import_history;
    let forged_target_event = go_completion_event(
        second_successor_id,
        "handoff-corrected-go-success",
        &workspace_root,
        true,
        "",
    );
    let latest_snapshot_index = ambiguous_import
        .iter()
        .rposition(|item| {
            matches!(
                item,
                RolloutItem::ResponseItem(envelope)
                    if envelope.metadata.as_ref()
                        .and_then(|metadata| metadata.rencrow_completion.as_ref())
                        .is_some()
            )
        })
        .expect("authenticated handoff includes a projected completion snapshot");
    ambiguous_import.splice(
        latest_snapshot_index..latest_snapshot_index,
        [forged_target_event.clone(), forged_target_event],
    );
    let (ambiguous_successor, _, _) = cold_restore_authenticated_handoff(
        second_successor_id,
        ambiguous_import,
        second_handoff_directory.path(),
    )
    .await?;
    {
        let state = ambiguous_successor.state.lock().await;
        assert!(
            state
                .task_check_ledger
                .attempt_for_call("failed-go-launch")
                .is_none_or(|source| source.resolved_by.is_none())
        );
    }
    source_session
        .abort_all_tasks(codex_protocol::protocol::TurnAbortReason::Interrupted)
        .await;
    successor_source_session
        .abort_all_tasks(codex_protocol::protocol::TurnAbortReason::Interrupted)
        .await;
    Ok(())
}

fn external_blocked_state() -> serde_json::Value {
    json!({
        "contract_revision": 1,
        "conditions": [{
            "id": "AC-1",
            "revision": 1,
            "result": "The service can serve the requested model",
            "scope": "configured provider",
            "verification": "Check the configured model catalog",
            "acceptance_criteria": "The requested model is alive with a fresh check",
            "status": "open"
        }],
        "units": [{
            "id": "unit-1",
            "purpose": "Continue the blocked feature",
            "status": "ready",
            "condition_refs": ["AC-1"]
        }],
        "blocker": {
            "category": "external_service",
            "reason": "The configured model service is unavailable",
            "recheck_condition": SUPPORTED_RECHECK_CONDITION
        },
        "next_action": "Recheck the configured service",
        "product_progress_at": null,
        "runtime_recovery_at": null,
        "activity_at": null
    })
}

async fn register_blocker(session: &Session) -> CompletionState {
    session
        .set_rencrow_completion_state("root".into(), external_blocked_state())
        .await
        .expect("valid external blocker state");
    session
        .autonomous_completion_current_state()
        .await
        .expect("registered state")
        .1
}

#[tokio::test]
async fn matching_external_blocker_clears_without_changing_product_clocks() {
    let (session, _) = make_session_and_context().await;
    register_blocker(&session).await;
    {
        let mut state = session.state.lock().await;
        let completion = &mut state.rencrow_completion_state.as_mut().unwrap().1;
        completion.product_progress_at = Some(100);
        completion.activity_at = Some(110);
    }
    let expected = session
        .autonomous_completion_current_state()
        .await
        .unwrap()
        .1;

    let before_recovery = chrono::Utc::now().timestamp();
    assert!(
        session
            .clear_recovered_external_blocker("root", &expected)
            .await
            .expect("persist recovered blocker")
    );

    let saved = session
        .autonomous_completion_current_state()
        .await
        .unwrap()
        .1;
    assert_eq!(saved.blocker, None);
    assert!(
        saved
            .runtime_recovery_at
            .is_some_and(|at| at >= before_recovery && at <= chrono::Utc::now().timestamp())
    );
    assert_eq!(saved.product_progress_at, Some(100));
    assert_eq!(saved.activity_at, Some(110));
}

#[tokio::test]
async fn changed_state_user_pause_and_pending_host_check_reject_recovery() {
    let (session, _) = make_session_and_context().await;
    let stale = register_blocker(&session).await;
    let mut changed = external_blocked_state();
    changed["next_action"] = "New plan while probe was running".into();
    session
        .set_rencrow_completion_state("root".into(), changed)
        .await
        .unwrap();
    assert!(
        !session
            .clear_recovered_external_blocker("root", &stale)
            .await
            .unwrap()
    );
    assert!(
        session
            .autonomous_completion_current_state()
            .await
            .unwrap()
            .1
            .blocker
            .is_some()
    );

    session.pause_autonomous_completion().await;
    let paused = session
        .autonomous_completion_current_state()
        .await
        .unwrap()
        .1;
    assert!(
        !session
            .clear_recovered_external_blocker("root", &paused)
            .await
            .unwrap()
    );

    let (session, _) = make_session_and_context().await;
    let expected = register_blocker(&session).await;
    session
        .begin_task_check(
            "pending-check".into(),
            TaskEvidenceRequest {
                task_id: "unit-1".into(),
                task_revision: 1,
                artifact_paths: vec![PathBuf::from("artifact.txt")],
                check_kind: TaskCheckKind::Verification,
            },
            PathBuf::from("/tmp"),
            vec![TaskArtifactStamp {
                path: PathBuf::from("artifact.txt"),
                sha256: "hash".into(),
            }],
        )
        .await;
    assert!(
        !session
            .clear_recovered_external_blocker("root", &expected)
            .await
            .unwrap()
    );
    assert!(
        session
            .autonomous_completion_current_state()
            .await
            .unwrap()
            .1
            .blocker
            .is_some()
    );
}

#[tokio::test]
async fn unsupported_external_recheck_sentinels_are_never_candidates() {
    let (session, _) = make_session_and_context().await;
    let mut state = external_blocked_state();
    state["blocker"]["recheck_condition"] = "model_supplied_custom_probe".into();
    session
        .set_rencrow_completion_state("root".into(), state)
        .await
        .unwrap();

    assert!(
        session
            .autonomous_completion_recovery_candidate()
            .await
            .is_none()
    );
}

#[tokio::test]
async fn one_observer_is_owned_and_rejected_duplicate_is_aborted() {
    let (session, _) = make_session_and_context().await;
    assert!(
        session
            .install_completion_recovery_task(AbortOnDropHandle::new(tokio::spawn(async {
                std::future::pending::<()>().await;
            })))
            .await
    );

    let (started_tx, started_rx) = oneshot::channel();
    let (dropped_tx, dropped_rx) = oneshot::channel();
    let duplicate = tokio::spawn(async move {
        struct DropSignal(Option<oneshot::Sender<()>>);
        impl Drop for DropSignal {
            fn drop(&mut self) {
                if let Some(signal) = self.0.take() {
                    let _ = signal.send(());
                }
            }
        }
        let _signal = DropSignal(Some(dropped_tx));
        let _ = started_tx.send(());
        std::future::pending::<()>().await;
    });
    started_rx.await.unwrap();
    assert!(
        !session
            .install_completion_recovery_task(AbortOnDropHandle::new(duplicate))
            .await
    );
    tokio::time::timeout(std::time::Duration::from_secs(1), dropped_rx)
        .await
        .expect("rejected observer gets aborted")
        .unwrap();
}

#[tokio::test]
async fn idle_review_deadline_is_observed_once_and_respects_stop_and_uncollected_child() {
    let (session, _) = make_session_and_context().await;
    session
        .set_rencrow_completion_state(
            "root".into(),
            serde_json::json!({
                "contract_revision":1,
                "conditions":[{"id":"AC-1","revision":1,"result":"CLI works",
                    "scope":"CLI","verification":"Run CLI","acceptance_criteria":"exit 0","status":"open"}],
                "units":[],"next_action":"Implement CLI"
            }),
        )
        .await
        .unwrap();
    assert!(due_plan_review_event(&session).await.is_none());
    {
        let mut state = session.state.lock().await;
        let contract = &mut state.rencrow_completion_state.as_mut().unwrap().1;
        contract.plan_review_at = Some(chrono::Utc::now().timestamp() - 1801);
        contract.product_progress_at = None;
    }
    let key = due_plan_review_event(&session)
        .await
        .expect("idle deadline is due without a new model turn");
    {
        let mut state = session.state.lock().await;
        state
            .rencrow_completion_uncollected_children
            .insert("01a1c606-48b8-7cf3-bb78-77b47f6b8c2f".into());
    }
    assert!(due_plan_review_event(&session).await.is_none());
    {
        let mut state = session.state.lock().await;
        state.rencrow_completion_uncollected_children.clear();
        state
            .rencrow_completion_state
            .as_mut()
            .unwrap()
            .1
            .consumed_host_events
            .push(key);
    }
    assert!(due_plan_review_event(&session).await.is_none());
    {
        let mut state = session.state.lock().await;
        state
            .rencrow_completion_state
            .as_mut()
            .unwrap()
            .1
            .consumed_host_events
            .clear();
        state
            .task_check_ledger
            .restore_snapshot(
                serde_json::from_value(serde_json::json!([{
                    "task_id":"unit-a", "task_revision":1,
                    "attempt":{"call_id":"unfinished-check", "status":"Pending"}
                }]))
                .unwrap(),
            )
            .unwrap();
    }
    assert!(due_plan_review_event(&session).await.is_none());
    {
        let mut state = session.state.lock().await;
        state
            .task_check_ledger
            .restore_snapshot(Vec::new())
            .unwrap();
        state.rencrow_completion_paused = true;
    }
    assert!(due_plan_review_event(&session).await.is_none());
}
