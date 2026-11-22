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

fn review_attempt_for_handoff() -> codex_history::task_context::CompletionReviewAttempt {
    use codex_history::task_context::CompletionReviewAttempt;
    use codex_history::task_context::CompletionReviewAttemptStatus;

    CompletionReviewAttempt {
        attempt_id: "handoff-review-attempt".into(),
        contract_revision: 1,
        condition_refs: vec!["AC-1".into()],
        conditions_sha256: "0".repeat(64),
        status: CompletionReviewAttemptStatus::Completed,
        launch_unknown_reason: None,
        consumed_retry_authorization: None,
        imported_from: None,
        reviewer_thread_id: Some(codex_protocol::ThreadId::new().to_string()),
        reviewer_turn_id: Some("handoff-review-turn".into()),
        reviewer_role: "analyst".into(),
        reviewer_model: Some("test-review-model".into()),
        target_task_id: "task-a".into(),
        target_task_revision: 1,
        target_artifact_ref: "task-check:accepted-artifact".into(),
        deadline_unix_ms: Some(1_800_000_000_000),
        deadline_escalation: None,
        deadline_safety_fingerprint: None,
        terminal_source: Some(
            codex_history::task_context::CompletionReviewTerminalSource {
                status: codex_history::task_context::CompletionReviewTerminalStatus::Completed,
                event: None,
            },
        ),
        wait_result_ref: Some("handoff-review-wait-result".into()),
        wait_call_id: Some("handoff-review-wait-call".into()),
        adoption_reason: Some("Host accepted the completed review".into()),
    }
}

fn review_snapshot_with_attempt(
    thread_id: &str,
    resume_generation: u64,
    attempt: &codex_history::task_context::CompletionReviewAttempt,
    previous_attempt_ref: Option<codex_history::task_context::CompletionReviewSnapshotRef>,
) -> serde_json::Value {
    use codex_history::task_context::CompletionReviewState;
    use codex_history::task_context::CompletionReviewTarget;
    use codex_history::task_context::CompletionUnit;
    use codex_history::task_context::CompletionUnitKind;
    use codex_history::task_context::UnitStatus;

    let review_unit = CompletionUnit {
        id: "unit-1".into(),
        purpose: "Review the accepted result".into(),
        status: UnitStatus::Running,
        kind: CompletionUnitKind::Review,
        condition_refs: vec!["AC-1".into()],
        depends_on: Vec::new(),
        evidence: Vec::new(),
        task_refs: Vec::new(),
        review_target: Some(CompletionReviewTarget {
            task_id: "task-a".into(),
            task_revision: 1,
        }),
        review_evidence: None,
        review_launch: None,
        review_decision_reason: None,
        review_state: Some(CompletionReviewState {
            version: 1,
            obligation_unit_id: "unit-1".into(),
            active_attempt: Some(attempt.clone()),
            historical_failure: None,
            retry_reservation_prior_historical_failure: None,
            previous_attempt_ref,
            retry_authorization: None,
            withdrawal: None,
            revalidation: None,
        }),
        review_revalidation_of: None,
    };
    let review_unit = codex_history::task_context::project_completion_review_state(&review_unit)
        .expect("derive review unit compatibility fields");
    let mut state = open_state(false);
    state.units = vec![review_unit];
    state.current_unit = None;
    state.next_action = Some("Continue the exact review obligation".into());

    serde_json::json!({
        "version": 2,
        "thread_id": thread_id,
        "root_task_id": "root",
        "resume_generation": resume_generation,
        "state": state,
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
async fn parent_handoff_projects_valid_v2_review_snapshot_reference() {
    let mut capsule = capsule().await.unwrap();
    let source_thread_id = capsule.source_thread_id.to_string();
    let attempt = review_attempt_for_handoff();
    let prior = review_snapshot_with_attempt(
        &source_thread_id,
        1,
        &attempt,
        /*previous_attempt_ref*/ None,
    );
    let prior_sha256 = maintenance_snapshot_digest(&prior).unwrap();
    capsule.prior_snapshots.insert(prior_sha256.clone(), prior);
    capsule.original_snapshot = review_snapshot_with_attempt(
        &source_thread_id,
        2,
        &attempt,
        Some(codex_history::task_context::CompletionReviewSnapshotRef {
            version: 2,
            sha256: prior_sha256.clone(),
        }),
    );

    let projection = capsule
        .project()
        .expect("supported v2 reference and its exact proof project together");
    let projected_ref =
        &projection.snapshot["state"]["units"][0]["review_state"]["previous_attempt_ref"];
    let projected_prior_sha256 = projection.original_to_projected[&prior_sha256].clone();
    assert_eq!(projected_ref["version"], 2);
    assert_eq!(projected_ref["sha256"], projected_prior_sha256);
    assert!(
        projection
            .prior_snapshots
            .contains_key(&projected_prior_sha256)
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
    _workspace: Option<tempfile::TempDir>,
}

async fn active_handoff() -> anyhow::Result<ActiveHandoffFixture> {
    active_handoff_with_auto_compact_limit(None).await
}

async fn active_handoff_with_auto_compact_limit(
    auto_compact_limit: Option<i64>,
) -> anyhow::Result<ActiveHandoffFixture> {
    active_handoff_with_provider(auto_compact_limit, None).await
}

async fn active_handoff_with_loopback_provider(
    auto_compact_limit: Option<i64>,
    provider_url: String,
) -> anyhow::Result<ActiveHandoffFixture> {
    active_handoff_with_provider(auto_compact_limit, Some(provider_url)).await
}

async fn active_handoff_with_provider(
    auto_compact_limit: Option<i64>,
    provider_url: Option<String>,
) -> anyhow::Result<ActiveHandoffFixture> {
    let directory = tempfile::tempdir()?;
    let loopback_configured = provider_url.is_some();
    let (mut session, mut turn, workspace) = if let Some(provider_url) = provider_url {
        let workspace = tempfile::tempdir()?;
        let workspace_path =
            codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(workspace.path())?;
        let handoff_directory =
            codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(directory.path())?;
        let (session, turn, _events) =
            crate::session::tests::make_session_and_context_with_auth_and_config_and_rx(
                codex_login::CodexAuth::from_api_key("test"),
                Vec::new(),
                move |config| {
                    config.cwd = workspace_path;
                    let sandbox_policy = config.legacy_sandbox_policy();
                    config
                        .set_legacy_sandbox_policy(sandbox_policy)
                        .expect("sync workspace roots with the isolated cwd");
                    config.rencrow_compaction = true;
                    config.rencrow_autonomous_completion = true;
                    config.rencrow_parent_handoff_dir = Some(handoff_directory);
                    if let Some(limit) = auto_compact_limit {
                        config.model_auto_compact_token_limit = Some(limit);
                        config.model_auto_compact_token_limit_scope =
                            codex_protocol::config_types::AutoCompactTokenLimitScope::Total;
                    }
                    config.model_provider.base_url = Some(provider_url);
                    config.model_providers.insert(
                        config.model_provider_id.clone(),
                        config.model_provider.clone(),
                    );
                },
            )
            .await;
        (
            Arc::try_unwrap(session)
                .unwrap_or_else(|_| panic!("source session fixture has a unique owner")),
            Arc::try_unwrap(turn)
                .unwrap_or_else(|_| panic!("source turn fixture has a unique owner")),
            Some(workspace),
        )
    } else {
        let (session, turn) = make_session_and_context().await;
        (session, turn, None)
    };
    if let Some(limit) = auto_compact_limit {
        if !loopback_configured {
            let config = Arc::make_mut(&mut turn.config);
            config.model_auto_compact_token_limit = Some(limit);
            config.model_auto_compact_token_limit_scope =
                codex_protocol::config_types::AutoCompactTokenLimitScope::Total;
        }
        let settings = Arc::make_mut(&mut turn.initial_settings);
        Arc::make_mut(&mut settings.model_info).auto_compact_token_limit = Some(limit);
        assert_eq!(turn.model_info().auto_compact_token_limit(), Some(limit));
    }
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
    if !loopback_configured {
        let config = Arc::make_mut(&mut turn.config);
        config.rencrow_compaction = true;
        config.rencrow_parent_handoff_dir =
            Some(codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(directory.path())?);
    }
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
        _workspace: workspace,
    })
}

#[tokio::test]
async fn actual_capacity_compaction_failure_seals_and_cold_restores_parent_handoff()
-> anyhow::Result<()> {
    let server = wiremock::MockServer::start().await;
    let fixture =
        active_handoff_with_loopback_provider(Some(1), format!("{}/v1", server.uri())).await?;
    assert_eq!(fixture.turn.config.model_auto_compact_token_limit, Some(1));
    assert_eq!(
        fixture.turn.model_info().auto_compact_token_limit(),
        Some(1)
    );

    let generated_work = "completed assistant work output ".repeat(4096);
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
                            text: "Preserve this user constraint.".into(),
                        }],
                        phase: None,
                        internal_chat_message_metadata_passthrough: None,
                    },
                ),
                codex_history::ResponseItemEnvelope::new(
                    codex_protocol::models::ResponseItem::Message {
                        id: None,
                        role: "assistant".into(),
                        content: vec![codex_protocol::models::ContentItem::OutputText {
                            text: generated_work,
                        }],
                        phase: None,
                        internal_chat_message_metadata_passthrough: None,
                    },
                ),
            ],
        )
        .await;
    let before_history = fixture.session.clone_history().await;
    let before_hash =
        codex_history::compaction_candidate::history_digest(before_history.annotated_items())
            .map_err(anyhow::Error::msg)?;
    let before_settings = fixture.session.thread_settings_snapshot().await;
    let before_state = fixture
        .session
        .state
        .lock()
        .await
        .rencrow_completion_state
        .clone();

    let error = crate::compact::run_inline_auto_compact_task(
        Arc::clone(&fixture.session),
        Arc::clone(&fixture.turn),
        crate::compact::InitialContextInjection::DoNotInject,
        codex_analytics::CompactionReason::ContextLimit,
        codex_analytics::CompactionPhase::MidTurn,
    )
    .await
    .expect_err("the one-token scope must reject the retained compaction floor");
    let error = error.to_string();
    assert!(error.contains("capacity-blocked"), "{error}");
    assert!(error.contains("minimum compaction floor"), "{error}");
    assert!(error.contains(">= 1 tokens"), "{error}");

    let after_hash = codex_history::compaction_candidate::history_digest(
        fixture.session.clone_history().await.annotated_items(),
    )
    .map_err(anyhow::Error::msg)?;
    assert_eq!(before_hash, after_hash);
    assert_eq!(
        before_settings,
        fixture.session.thread_settings_snapshot().await
    );
    assert_eq!(
        before_state,
        fixture
            .session
            .state
            .lock()
            .await
            .rencrow_completion_state
            .clone()
    );

    let path = handoff_record(fixture.directory.path())?;
    let raw: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    assert_eq!(raw["phase"], "failed");
    assert!(raw["failed_proof"].is_object());
    assert!(raw["reason"].as_str().is_some());
    let (record, capsule, _) = runtime::load_verified(&path).map_err(anyhow::Error::msg)?;
    assert_eq!(record.phase, "failed");
    assert!(record.failed_proof.is_some());
    let source_generation = capsule.original_snapshot["resume_generation"]
        .as_u64()
        .expect("sealed source snapshot has a resume generation");
    let expected_successor_generation = source_generation
        .checked_add(1)
        .expect("successor projection increments the source generation");
    assert!(!capsule.summary.is_empty());
    assert!(capsule.next_work.contains("unit-1"));

    let (_, _, _, imported_history) =
        runtime::initial_history(&path, &fixture.turn.config).map_err(anyhow::Error::msg)?;
    let (mut cold_session, _) = make_session_and_context().await;
    cold_session.thread_id = record.successor_thread_id;
    cold_session
        .restore_autonomous_completion_snapshot(&imported_history)
        .await;
    {
        let restored = cold_session.state.lock().await;
        let (root, state) = restored.rencrow_completion_state.as_ref().unwrap();
        assert_eq!(root, "root");
        assert_eq!(*state, open_state(false));
        assert_eq!(
            restored.rencrow_completion_resume_generation,
            expected_successor_generation
        );
        assert!(!restored.rencrow_completion_paused);
    }

    let manager_home = tempfile::tempdir()?;
    let mut owner_config = fixture.turn.config.as_ref().clone();
    owner_config.codex_home =
        codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(manager_home.path())?;
    owner_config.rencrow_parent_handoff_file =
        Some(codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(&path)?);
    let thread_manager = crate::ThreadManager::with_models_provider_and_home_for_tests(
        codex_login::CodexAuth::from_api_key("test"),
        owner_config.model_provider.clone(),
        owner_config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
    );
    let mut baseline_config = owner_config.clone();
    baseline_config.rencrow_parent_handoff_file = None;
    let baseline = thread_manager
        .start_thread(crate::StartThreadOptions::new(baseline_config))
        .await?;
    let baseline_settings = baseline.thread.thread_settings_snapshot().await;
    let settings_field_matches = serde_json::json!({
        "model": baseline_settings.model == capsule.settings.model,
        "model_provider_id": baseline_settings.model_provider_id == capsule.settings.model_provider_id,
        "service_tier": baseline_settings.service_tier == capsule.settings.service_tier,
        "approval_policy": baseline_settings.approval_policy == capsule.settings.approval_policy,
        "approvals_reviewer": baseline_settings.approvals_reviewer == capsule.settings.approvals_reviewer,
        "permission_profile": baseline_settings.permission_profile == capsule.settings.permission_profile,
        "active_permission_profile": baseline_settings.active_permission_profile == capsule.settings.active_permission_profile,
        "cwd": baseline_settings.cwd == capsule.settings.cwd,
        "runtime_workspace_roots": baseline_settings.runtime_workspace_roots == capsule.settings.runtime_workspace_roots,
        "reasoning_effort": baseline_settings.reasoning_effort == capsule.settings.reasoning_effort,
        "reasoning_summary": baseline_settings.reasoning_summary == capsule.settings.reasoning_summary,
        "personality": baseline_settings.personality == capsule.settings.personality,
        "collaboration_mode": baseline_settings.collaboration_mode == capsule.settings.collaboration_mode,
        "disabled_plugin_ids": baseline_settings.disabled_plugin_ids == capsule.settings.disabled_plugin_ids,
    });
    assert!(
        baseline_settings == capsule.settings,
        "baseline settings mismatch; per-field equality: {settings_field_matches}"
    );
    baseline.thread.shutdown_and_wait().await?;

    let successor = thread_manager
        .start_thread(crate::StartThreadOptions::new(owner_config.clone()))
        .await?;
    assert_eq!(successor.thread_id, record.successor_thread_id);
    runtime::claim(&path, record.successor_thread_id).expect_err("owner already claimed handoff");
    let duplicate_start = thread_manager
        .start_thread(crate::StartThreadOptions::new(owner_config))
        .await;
    assert!(duplicate_start.is_err());

    {
        let restored = successor.thread.session.state.lock().await;
        let (root, state) = restored.rencrow_completion_state.as_ref().unwrap();
        assert_eq!(root, "root");
        assert_eq!(*state, open_state(false));
        assert_eq!(
            restored.rencrow_completion_resume_generation,
            expected_successor_generation
        );
        assert!(!restored.rencrow_completion_paused);
    }
    let successor_history = successor.thread.conversation_history_snapshot().await;
    assert!(successor_history.items().any(|item| match item {
        codex_protocol::models::ResponseItem::Message { role, content, .. } => {
            role == "user"
                && content.iter().any(|item| {
                    matches!(item, codex_protocol::models::ContentItem::InputText { text }
                        if text == "Preserve this user constraint.")
                })
        }
        _ => false,
    }));
    let server_requests = server.received_requests().await.unwrap_or_default();
    assert_eq!(
        server_requests
            .iter()
            .filter(|request| {
                request.method == wiremock::http::Method::POST
                    && request.url.path().ends_with("/responses")
            })
            .count(),
        0,
        "thread-manager handoff start must not issue a model request"
    );
    successor.thread.shutdown_and_wait().await?;

    fixture
        .session
        .abort_all_tasks(TurnAbortReason::Interrupted)
        .await;
    Ok(())
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
