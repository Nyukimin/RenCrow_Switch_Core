// Modified by RenCrow Switch Core, 2026-11-21.
use super::super::parent_handoff::ParentHandoffCapsule;
use super::super::parent_handoff::runtime;
use super::*;
use pretty_assertions::assert_eq;

async fn capsule() -> anyhow::Result<ParentHandoffCapsule> {
    let (session, turn) = make_session_and_context().await;
    let source = session.thread_id();
    Ok(ParentHandoffCapsule {
        version: 1,
        source_thread_id: source,
        successor_thread_id: codex_protocol::ThreadId::new(),
        source_turn_id: "source-turn".into(),
        source_rollout: turn.config.cwd.join("source.jsonl").to_path_buf(),
        source_history_hash: "a".repeat(64),
        source_history_len: 0,
        pending_prompt: None,
        runtime_contract_sha256: runtime::runtime_contract(&turn.config)
            .map_err(anyhow::Error::msg)?,
        settings: session.thread_settings_snapshot().await,
        cwd: turn.config.cwd.to_path_buf(),
        summary: "Goal, constraints, finite original evidence and failures".into(),
        next_work: "Complete unit-1 using the existing acceptance criteria".into(),
        original_snapshot: serde_json::json!({
            "version":1, "thread_id":source.to_string(), "root_task_id":"root",
            "resume_generation":3, "state":open_state(false),
        }),
        prior_snapshots: BTreeMap::new(),
        accepted_task_state: None,
        retained_instructions: Vec::new(),
        guard: crate::rencrow_execution_guard::GuardState::default()
            .parent_handoff_snapshot()
            .map_err(anyhow::Error::msg)?,
    })
}

#[tokio::test]
async fn parent_handoff_projects_owner_but_preserves_conditions_failures_and_original() {
    let mut capsule = capsule().await.unwrap();
    capsule.guard["reads"] = serde_json::json!(17);
    capsule.guard["budget_rejections"] = serde_json::json!(2);
    capsule.guard["repeated"] = serde_json::json!({"exact read call":3});
    let original = capsule.original_snapshot.clone();
    let projection = capsule.project().unwrap();
    assert_eq!(capsule.original_snapshot, original);
    assert_eq!(
        projection.snapshot["thread_id"],
        capsule.successor_thread_id.to_string()
    );
    assert_eq!(projection.snapshot["resume_generation"], 4);
    assert_eq!(projection.snapshot["state"], original["state"]);
    assert_eq!(projection.guard, capsule.guard);
    assert!(
        projection
            .original_to_projected
            .contains_key(&maintenance_snapshot_digest(&original).unwrap())
    );
}

#[tokio::test]
async fn parent_handoff_rejects_pause_unknown_work_foreign_or_missing_proofs() {
    let mut capsule = capsule().await.unwrap();
    capsule.original_snapshot["state"]["paused"] = Value::Bool(true);
    assert!(matches!(capsule.project(), Err(error) if error.contains("paused")));
    capsule.original_snapshot["state"]["paused"] = Value::Bool(false);
    capsule.original_snapshot["uncollected_children"] =
        serde_json::json!([codex_protocol::ThreadId::new().to_string()]);
    assert!(capsule.project().is_err());
    capsule.original_snapshot["uncollected_children"] = serde_json::json!([]);
    capsule.original_snapshot["thread_id"] =
        serde_json::json!(codex_protocol::ThreadId::new().to_string());
    assert!(capsule.project().is_err());
}

#[tokio::test]
async fn parent_handoff_cold_restore_keeps_open_task_and_does_not_accept_it() {
    let mut capsule = capsule().await.unwrap();
    let (session, _) = make_session_and_context().await;
    capsule.successor_thread_id = session.thread_id();
    let projection = capsule.project().unwrap();
    session
        .restore_autonomous_completion_snapshot(&[snapshot_item(projection.snapshot)])
        .await;
    let state = session.state.lock().await;
    let (root, restored) = state.rencrow_completion_state.as_ref().unwrap();
    assert_eq!(root, "root");
    assert_eq!(*restored, open_state(false));
    assert_eq!(state.rencrow_completion_resume_generation, 4);
    assert!(!state.rencrow_completion_paused);
}

#[test]
fn parent_handoff_exclusive_start_does_not_resend_unknown_result() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("handoff.json");
    let target = codex_protocol::ThreadId::new();
    runtime::claim(&path, target).unwrap();
    assert!(runtime::claim(&path, target).is_err());
    assert!(runtime::claim(&path, codex_protocol::ThreadId::new()).is_err());
}

#[test]
fn parent_handoff_unsealed_record_cannot_import() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("handoff.json");
    std::fs::write(&path, "{}").unwrap();
    assert!(runtime::load_verified(&path).is_err());
}

struct ActiveHandoffFixture {
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    directory: tempfile::TempDir,
}

async fn active_handoff() -> anyhow::Result<ActiveHandoffFixture> {
    let directory = tempfile::tempdir()?;
    let (mut session, mut turn) = make_session_and_context().await;
    let config = session.get_config().await;
    let live_thread = codex_thread_store::LiveThread::create(
        Arc::clone(&session.services.thread_store),
        codex_thread_store::CreateThreadParams {
            creator_user_id: None,
            creator_account_id: None,
            session_id: session.thread_id().into(),
            thread_id: session.thread_id(),
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
            history_mode: Default::default(),
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
    let config = Arc::make_mut(&mut turn.config);
    config.rencrow_compaction = true;
    config.rencrow_parent_handoff_dir =
        Some(codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(directory.path())?);
    session.state.lock().await.rencrow_completion_state = Some(("root".into(), open_state(false)));
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    session
        .spawn_task(Arc::clone(&turn), Vec::new(), PendingBoundaryTask)
        .await;
    session
        .persist_autonomous_completion_snapshot()
        .await
        .map_err(anyhow::Error::msg)?;
    Ok(ActiveHandoffFixture {
        session,
        turn,
        directory,
    })
}

fn handoff_record(directory: &std::path::Path) -> anyhow::Result<std::path::PathBuf> {
    let folders = std::fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
    anyhow::ensure!(folders.len() == 1, "exactly one successor must be reserved");
    Ok(folders[0].path().join("handoff.json"))
}

#[tokio::test]
async fn parent_handoff_keeps_only_latest_host_completion_continue() -> anyhow::Result<()> {
    let fixture = active_handoff().await?;
    let continue_item = |next_action: &str, client_authored: bool| {
        let item = crate::context::ContextualUserFragment::into(
            crate::context::AutonomousCompletionContinue {
                next_action: next_action.into(),
            },
        );
        let mut envelope = codex_history::ResponseItemEnvelope::new(item);
        envelope.metadata = Some(codex_history::CodexHarnessMetadata {
            client_authored,
            ..Default::default()
        });
        envelope
    };
    let user_constraint = "Keep the original user constraint exactly as written.";
    let stale_action = "stale-handoff-next-action";
    let client_action = "client-authored-lookalike";
    let latest_action = "latest-handoff-next-action";
    fixture
        .session
        .record_annotated_conversation_items(
            &fixture.turn,
            fixture.turn.model_info().as_ref(),
            vec![
                codex_history::ResponseItemEnvelope::new(
                    codex_protocol::models::ResponseItem::Message {
                        id: None,
                        role: "user".into(),
                        content: vec![codex_protocol::models::ContentItem::InputText {
                            text: user_constraint.into(),
                        }],
                        phase: None,
                        internal_chat_message_metadata_passthrough: None,
                    },
                ),
                continue_item(stale_action, false),
                continue_item(client_action, true),
                continue_item(latest_action, false),
            ],
        )
        .await;
    let source_history = fixture.session.clone_history().await;
    let source_history_hash =
        codex_history::compaction_candidate::history_digest(source_history.annotated_items())
            .map_err(anyhow::Error::msg)?;
    let source_history_len = source_history.annotated_items().len();

    runtime::prepare(
        &fixture.session,
        &fixture.turn,
        codex_analytics::CompactionPhase::MidTurn,
    )
    .await
    .map_err(anyhow::Error::msg)?
    .ok_or_else(|| anyhow::anyhow!("prepared packet missing"))?;

    let record_path = handoff_record(fixture.directory.path())?;
    let record: runtime::HandoffRecord = serde_json::from_slice(&std::fs::read(record_path)?)?;
    let capsule: ParentHandoffCapsule =
        serde_json::from_slice(&std::fs::read(record.capsule_file)?)?;
    let retained_texts = capsule
        .retained_instructions
        .iter()
        .filter_map(|item| match item {
            codex_history::RolloutItem::ResponseItem(envelope) => match &envelope.item {
                codex_protocol::models::ResponseItem::Message { content, .. } => Some(
                    content
                        .iter()
                        .filter_map(|item| match item {
                            codex_protocol::models::ContentItem::InputText { text } => {
                                Some(text.as_str())
                            }
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join(""),
                ),
                _ => None,
            },
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        !retained_texts
            .iter()
            .any(|text| text.contains(stale_action))
    );
    assert!(
        retained_texts
            .iter()
            .any(|text| text.contains(latest_action))
    );
    assert!(
        retained_texts
            .iter()
            .any(|text| text.contains(client_action))
    );
    assert!(retained_texts.iter().any(|text| text == user_constraint));
    assert_eq!(capsule.source_history_hash, source_history_hash);
    assert_eq!(capsule.source_history_len, source_history_len);

    fixture
        .session
        .abort_all_tasks(TurnAbortReason::Interrupted)
        .await;
    Ok(())
}

#[tokio::test]
async fn parent_handoff_failure_seal_rejects_live_contract_drift() -> anyhow::Result<()> {
    let fixture = active_handoff().await?;
    let before_hash = codex_history::compaction_candidate::history_digest(
        fixture.session.clone_history().await.annotated_items(),
    )
    .map_err(anyhow::Error::msg)?;
    let prepared = runtime::prepare(
        &fixture.session,
        &fixture.turn,
        codex_analytics::CompactionPhase::MidTurn,
    )
    .await
    .map_err(anyhow::Error::msg)?
    .ok_or_else(|| anyhow::anyhow!("prepared packet missing"))?;
    let path = handoff_record(fixture.directory.path())?;
    let after_hash = codex_history::compaction_candidate::history_digest(
        fixture.session.clone_history().await.annotated_items(),
    )
    .map_err(anyhow::Error::msg)?;
    assert_eq!(before_hash, after_hash);
    let before: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    assert_eq!(before["phase"], "prepared");
    fixture
        .session
        .state
        .lock()
        .await
        .rencrow_completion_state
        .as_mut()
        .unwrap()
        .1
        .next_action = Some("changed after preparation".into());
    assert!(
        runtime::finish(
            &fixture.session,
            &fixture.turn,
            &prepared,
            "failed",
            Some("typed capacity fixture".into())
        )
        .await
        .is_err()
    );
    let after: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    assert_eq!(before, after);
    fixture
        .session
        .abort_all_tasks(TurnAbortReason::Interrupted)
        .await;
    Ok(())
}

#[tokio::test]
async fn parent_handoff_pre_turn_seals_only_after_preserving_exact_input() -> anyhow::Result<()> {
    let fixture = active_handoff().await?;
    let text = "Stop the current work and report the unresolved acceptance criteria.";
    let input = vec![crate::session::input_queue::TurnInput::UserInput {
        content: vec![codex_protocol::user_input::UserInput::Text {
            text: text.into(),
            text_elements: Vec::new(),
        }],
        client_id: None,
        acceptance_order: None,
    }];
    fixture
        .session
        .remember_parent_handoff_input(&fixture.turn, &input)
        .await;
    let prepared = runtime::prepare(
        &fixture.session,
        &fixture.turn,
        codex_analytics::CompactionPhase::PreTurn,
    )
    .await
    .map_err(anyhow::Error::msg)?
    .ok_or_else(|| anyhow::anyhow!("prepared packet missing"))?;
    let path = handoff_record(fixture.directory.path())?;
    runtime::finish(
        &fixture.session,
        &fixture.turn,
        &prepared,
        "failed",
        Some("typed capacity fixture".into()),
    )
    .await
    .map_err(anyhow::Error::msg)?;
    assert!(runtime::load_verified(&path).is_err());
    let stopped = crate::session::turn::run_hooks_and_record_inputs(
        &fixture.session,
        &fixture.turn,
        &fixture.turn.capture_current_model_info(),
        &input,
        codex_thread_store::PersistContext::Standard,
    )
    .await;
    assert!(!stopped);
    fixture
        .session
        .finalize_parent_handoff_input(&fixture.turn, runtime::PreservedHandoffInput::Accepted)
        .await
        .map_err(anyhow::Error::msg)?;
    let (_, capsule, _) = runtime::load_verified(&path).map_err(anyhow::Error::msg)?;
    assert_eq!(capsule.pending_prompt.as_deref(), Some(text));
    let (_, _, _, history) = runtime::initial_history(&path, &fixture.turn.config)?;
    let Some(RolloutItem::ResponseItem(last)) = history.last() else {
        panic!("preserved input absent")
    };
    assert!(
        matches!(&last.item, ResponseItem::Message { role, content, .. }
        if role == "user" && matches!(content.as_slice(),
            [codex_protocol::models::ContentItem::InputText { text: actual }] if actual == text))
    );
    fixture
        .session
        .abort_all_tasks(TurnAbortReason::Interrupted)
        .await;
    Ok(())
}
