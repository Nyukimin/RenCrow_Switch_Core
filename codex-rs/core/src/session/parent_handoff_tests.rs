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
    _codex_home: Option<tempfile::TempDir>,
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
        _codex_home: None,
    })
}

async fn active_handoff_with_cli_config(
    provider_url: String,
) -> anyhow::Result<ActiveHandoffFixture> {
    use codex_config::config_toml::ConfigToml;
    use codex_config::config_toml::ProjectConfig;
    use codex_protocol::config_types::ReasoningSummary;
    use codex_protocol::config_types::SandboxMode;
    use codex_protocol::openai_models::ReasoningEffort;
    use std::collections::HashMap;

    let directory = tempfile::tempdir()?;
    let workspace = tempfile::tempdir()?;
    let codex_home = tempfile::tempdir()?;
    let workspace_path =
        codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(workspace.path())?;
    let handoff_directory =
        codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(directory.path())?;
    let trusted_workspace = codex_config::loader::project_trust_key(workspace.path());
    let config_toml = ConfigToml {
        model: Some("gpt-5.5".into()),
        model_provider: Some("openai".into()),
        openai_base_url: Some(provider_url),
        cli_auth_credentials_store: Some(codex_config::types::AuthCredentialsStoreMode::File),
        projects: Some(HashMap::from([(
            trusted_workspace.clone(),
            ProjectConfig {
                trust_level: Some(codex_protocol::config_types::TrustLevel::Trusted),
            },
        )])),
        model_reasoning_effort: Some(ReasoningEffort::High),
        model_reasoning_summary: Some(ReasoningSummary::None),
        model_auto_compact_token_limit: Some(1),
        model_auto_compact_token_limit_scope: Some(
            codex_protocol::config_types::AutoCompactTokenLimitScope::Total,
        ),
        sandbox_mode: Some(SandboxMode::WorkspaceWrite),
        rencrow_compaction: Some(true),
        rencrow_autonomous_completion: Some(true),
        rencrow_parent_handoff_dir: Some(handoff_directory),
        rencrow_execution_guard: Some(codex_config::RencrowExecutionGuard {
            task_kind: Some(codex_config::RencrowTaskKind::Orchestrate),
            ..Default::default()
        }),
        analytics: Some(codex_config::types::AnalyticsConfigToml {
            enabled: Some(false),
        }),
        check_for_update_on_startup: Some(false),
        ..Default::default()
    };
    std::fs::write(
        codex_home.path().join(codex_config::CONFIG_TOML_FILE),
        toml::to_string(&config_toml)?,
    )?;
    let config = crate::config::ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(workspace.path().to_path_buf()))
        .build()
        .await?;
    anyhow::ensure!(
        config.cwd == workspace_path,
        "source Config did not load the isolated cwd"
    );
    anyhow::ensure!(
        config.model_provider_id == "openai"
            && config.model_provider.base_url.as_deref()
                == Some(config_toml.openai_base_url.as_deref().unwrap()),
        "source Config did not load the loopback OpenAI provider"
    );
    anyhow::ensure!(
        config.active_project.is_trusted(),
        "source Config did not load trust for the isolated workspace"
    );
    anyhow::ensure!(
        config.cli_auth_credentials_store_mode
            == codex_config::types::AuthCredentialsStoreMode::File,
        "source Config did not select the isolated file auth store"
    );
    anyhow::ensure!(
        config.model.as_deref() == Some("gpt-5.5")
            && config.model_reasoning_effort == Some(ReasoningEffort::High)
            && config.model_reasoning_summary == Some(ReasoningSummary::None)
            && config.model_auto_compact_token_limit == Some(1)
            && config.rencrow_compaction
            && config.rencrow_autonomous_completion
            && config.rencrow_parent_handoff_dir.as_ref()
                == config_toml.rencrow_parent_handoff_dir.as_ref()
            && config
                .rencrow_execution_guard
                .as_ref()
                .and_then(|guard| guard.task_kind.as_ref())
                == Some(&codex_config::RencrowTaskKind::Orchestrate),
        "source Config does not match the CLI handoff model and runtime settings"
    );
    anyhow::ensure!(
        runtime::runtime_contract(&config).is_ok(),
        "loader-built C29 source Config has no valid runtime contract"
    );
    codex_login::login_with_api_key(
        codex_home.path(),
        "c29-test-only-dummy-key",
        codex_config::types::AuthCredentialsStoreMode::File,
        codex_login::AuthKeyringBackendKind::default(),
    )?;
    let (session, turn, _) = crate::session::tests::make_session_and_context_with_config_and_rx(
        codex_login::CodexAuth::from_api_key("test"),
        Vec::new(),
        config,
    )
    .await;
    let mut session = Arc::try_unwrap(session)
        .unwrap_or_else(|_| panic!("loader-built source session fixture has a unique owner"));
    let turn = Arc::try_unwrap(turn)
        .unwrap_or_else(|_| panic!("loader-built source turn fixture has a unique owner"));
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
        _workspace: Some(workspace),
        _codex_home: Some(codex_home),
    })
}

#[tokio::test]
async fn actual_capacity_compaction_failure_seals_and_cold_restores_parent_handoff()
-> anyhow::Result<()> {
    let server = wiremock::MockServer::start().await;
    let bridge_enabled = std::env::var_os("RENCROW_C29_HARNESS").is_some();
    let provider_url = format!("{}/v1", server.uri());
    let fixture = if bridge_enabled {
        active_handoff_with_cli_config(provider_url).await?
    } else {
        active_handoff_with_loopback_provider(Some(1), provider_url).await?
    };
    if bridge_enabled {
        let response_sse = concat!(
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"c29-resp\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"c29-resp\"}}\n\n",
        );
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/responses"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(response_sse),
            )
            .mount(&server)
            .await;
    }
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
    if bridge_enabled {
        assert_eq!(
            capsule.settings.reasoning_summary,
            Some(codex_protocol::config_types::ReasoningSummary::None),
            "C29 CLI source Config and sealed capsule must retain TUI's explicit summary default"
        );
    }
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

    if std::env::var_os("RENCROW_C29_HARNESS").is_some() {
        run_c29_python_owner_harness(&fixture, &path, &record, &capsule).await?;
        let server_requests = server.received_requests().await.unwrap_or_default();
        let continuation_requests = server_requests
            .iter()
            .filter(|request| {
                request.method == wiremock::http::Method::POST
                    && request.url.path().ends_with("/responses")
                    && serde_json::from_slice::<Value>(&request.body).is_ok_and(|body| {
                        body.to_string().contains("Hostの自動引継ぎが完了しました")
                    })
            })
            .count();
        anyhow::ensure!(
            continuation_requests == 1,
            "expected exactly one continuation request at the isolated loopback Responses endpoint; observed {continuation_requests}"
        );
        fixture
            .session
            .abort_all_tasks(TurnAbortReason::Interrupted)
            .await;
        return Ok(());
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

async fn run_c29_python_owner_harness(
    fixture: &ActiveHandoffFixture,
    record_path: &std::path::Path,
    record: &runtime::HandoffRecord,
    capsule: &ParentHandoffCapsule,
) -> anyhow::Result<()> {
    use sha2::Digest;
    use std::io::Write;
    let test_process_group_id = c29_test_process_group_id()?;

    let harness = std::env::var_os("RENCROW_C29_HARNESS")
        .ok_or_else(|| anyhow::anyhow!("RENCROW_C29_HARNESS is required"))?;
    let tools_root = std::env::var_os("RENCROW_TOOLS_ROOT")
        .ok_or_else(|| anyhow::anyhow!("RENCROW_TOOLS_ROOT archive is required"))?;
    let tools_root_path = std::path::PathBuf::from(&tools_root);
    let tools_commit = std::env::var("RENCROW_TOOLS_COMMIT")?;
    let parent_handoff_sha256 = std::env::var("RENCROW_PARENT_HANDOFF_SHA256")?;
    let cli_binary =
        std::path::PathBuf::from(std::env::var_os("RENCROW_C29_CLI_BIN").ok_or_else(|| {
            anyhow::anyhow!("RENCROW_C29_CLI_BIN is required for the real CLI bridge")
        })?);
    let cli_source_commit = std::env::var("RENCROW_C29_CLI_COMMIT")?;
    let expected_cli_sha256 = std::env::var("RENCROW_C29_CLI_SHA256")?;
    let cli_manifest_path =
        std::path::PathBuf::from(std::env::var_os("RENCROW_C29_CLI_MANIFEST").ok_or_else(
            || anyhow::anyhow!("RENCROW_C29_CLI_MANIFEST is required for paired binary evidence"),
        )?);
    let cli_manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&cli_manifest_path)?)?;
    anyhow::ensure!(
        cli_manifest["source_commit"] == cli_source_commit
            && cli_manifest["source_clean_after_build"] == true
            && cli_manifest["artifacts"]["codex"]["path"] == cli_binary.to_string_lossy().as_ref()
            && cli_manifest["artifacts"]["codex"]["sha256"] == expected_cli_sha256,
        "paired CLI manifest does not bind the provided path, source commit, and SHA-256"
    );
    let cli_binary_bytes = std::fs::read(&cli_binary)?;
    let actual_cli_sha256 = format!("{:x}", sha2::Sha256::digest(&cli_binary_bytes));
    anyhow::ensure!(
        actual_cli_sha256 == expected_cli_sha256,
        "test-owned CLI binary hash mismatch at {}",
        cli_binary.display()
    );
    let test_source_head = std::process::Command::new("git")
        .arg("-C")
        .arg(env!("CARGO_MANIFEST_DIR"))
        .args(["rev-parse", "HEAD"])
        .output()?;
    anyhow::ensure!(
        test_source_head.status.success(),
        "could not read test source HEAD"
    );
    let test_source_head = String::from_utf8(test_source_head.stdout)?
        .trim()
        .to_owned();
    let cli_source_commit_matches_test_head = cli_source_commit == test_source_head;
    let source_diff = std::process::Command::new("git")
        .arg("-C")
        .arg(env!("CARGO_MANIFEST_DIR"))
        .args([
            "diff",
            "--name-only",
            &format!("{cli_source_commit}..{test_source_head}"),
        ])
        .output()?;
    anyhow::ensure!(
        source_diff.status.success(),
        "could not compare paired CLI and test source"
    );
    let paired_to_test_changes = String::from_utf8(source_diff.stdout)?
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    anyhow::ensure!(
        paired_to_test_changes.iter().all(|path| {
            path.starts_with("codex-rs/core/src/session/") && path.ends_with("_tests.rs")
                || path.starts_with("codex-rs/core/tests/fixtures/")
                || path.starts_with("codex-rs/core/tests/suite/")
        }),
        "paired CLI source differs from test HEAD outside test/fixture paths: {paired_to_test_changes:?}"
    );
    let workspace = fixture.turn.config.cwd.to_path_buf();
    let trusted_workspace_key = codex_config::loader::project_trust_key(&workspace);
    let codex_home = fixture.turn.config.codex_home.to_path_buf();
    let source_rollout = capsule.source_rollout.clone();
    anyhow::ensure!(
        source_rollout.starts_with(codex_home.join("sessions")),
        "native source rollout is outside its isolated Codex HOME sessions directory: {}",
        source_rollout.display()
    );
    let provider_url = fixture
        .turn
        .config
        .model_provider
        .base_url
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("capacity fixture must use its local mock provider"))?;
    let base = fixture.directory.path().join("c29-python-owner");
    std::fs::create_dir_all(&base)?;
    let record_bytes = std::fs::read(record_path)?;
    let record_sha256 =
        codex_history::archive_reference::content_sha256(std::str::from_utf8(&record_bytes)?);
    let config_path = base.join("harness-input.json");
    let target_effort = capsule
        .settings
        .reasoning_effort
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("sealed capsule omits reasoning effort"))?;
    let config = serde_json::json!({
        "base": base,
        "home": codex_home,
        "cwd": workspace,
        "record_file": record_path,
        "record_sha256": record_sha256,
        "record_id": record.capsule_sha256,
        "source_thread": record.source_thread_id.to_string(),
        "successor_thread": record.successor_thread_id.to_string(),
        "source_turn": record.source_turn_id,
        "root_task_id": capsule.original_snapshot["root_task_id"],
        "source_rollout": source_rollout,
        "rollout_mtime": std::fs::metadata(&source_rollout)?.modified()?.elapsed()?.as_secs(),
        "handoff_directory": record_path.parent().and_then(std::path::Path::parent)
            .ok_or_else(|| anyhow::anyhow!("handoff directory is missing"))?,
        "provider_url": provider_url,
        "tools_root": tools_root_path,
        "test_binary": std::env::current_exe()?,
        "cli_binary": cli_binary,
        "cli_binary_sha256": actual_cli_sha256,
        "cli_binary_manifest_source_clean": cli_manifest["source_clean_after_build"],
        "cli_source_commit": cli_source_commit,
        "cli_manifest_path": cli_manifest_path,
        "paired_to_test_source_changes": paired_to_test_changes,
        "test_source_head": test_source_head,
        "cli_source_commit_matches_test_head": cli_source_commit_matches_test_head,
        "source_runtime_contract_sha256": runtime::runtime_contract(&fixture.turn.config)
            .map_err(anyhow::Error::msg)?,
        "target_model": capsule.settings.model,
        "target_effort": serde_json::to_value(target_effort)?,
        "cli_config_file": codex_home.join(codex_config::CONFIG_TOML_FILE),
        "trusted_workspace_key": trusted_workspace_key,
        "test_process_group_id": test_process_group_id,
    });
    let mut config_file = std::fs::File::create(&config_path)?;
    config_file.write_all(&serde_json::to_vec_pretty(&config)?)?;
    config_file.sync_all()?;

    let python = std::env::var_os("PYTHON").unwrap_or_else(|| "python3".into());
    let mut command = std::process::Command::new(python);
    command
        .arg(harness)
        .arg(&config_path)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &codex_home)
        .env("TMPDIR", base.join("tmp"))
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env("RENCROW_TOOLS_ROOT", tools_root)
        .env("RENCROW_TOOLS_COMMIT", &tools_commit)
        .env("RENCROW_PARENT_HANDOFF_SHA256", &parent_handoff_sha256)
        .env("RENCROW_C29_CLI_BIN", &cli_binary)
        .env("RENCROW_C29_CLI_SHA256", &expected_cli_sha256)
        .env("RENCROW_C29_CLI_COMMIT", &cli_source_commit);
    command.env("RENCROW_C29_CLI_MANIFEST", &cli_manifest_path);
    if let Some(value) = std::env::var_os("RUST_MIN_STACK") {
        command.env("RUST_MIN_STACK", value);
    }
    std::fs::create_dir_all(base.join("tmp"))?;
    if let Some(value) = std::env::var_os("DYLD_LIBRARY_PATH") {
        command.env("DYLD_LIBRARY_PATH", value);
    }
    let output = command.output()?;
    let child_log_path = base.join("successor-cli-pty.log");
    let child_log_diagnostic = match std::fs::read_to_string(&child_log_path) {
        Ok(contents) => format!(
            "successor-cli-pty.log ({}):\n{contents}",
            child_log_path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            format!(
                "successor-cli-pty.log is absent: {}",
                child_log_path.display()
            )
        }
        Err(error) => format!(
            "successor-cli-pty.log could not be read at {}: {error}",
            child_log_path.display()
        ),
    };
    let failure_evidence = if output.status.success() {
        None
    } else if let Some(evidence_root) = std::env::var_os("RENCROW_C29_EVIDENCE_DIR") {
        match preserve_c29_failure_evidence(
            std::path::Path::new(&evidence_root),
            &base,
            record_path,
            &record.capsule_file,
            &output,
            &child_log_path,
            record,
        ) {
            Ok(path) => Some(format!("failure evidence saved at {}", path.display())),
            Err(error) => Some(format!("failure evidence save failed: {error:#}")),
        }
    } else {
        None
    };
    anyhow::ensure!(
        output.status.success(),
        "test-owned Python Supervisor bridge failed (status {}): {}\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
        child_log_diagnostic,
        failure_evidence
            .as_deref()
            .unwrap_or("failure evidence was not requested")
    );
    let result_path = base.join("result.json");
    let result: serde_json::Value = serde_json::from_slice(&std::fs::read(&result_path)?)?;
    let paired_to_test_changes_json = serde_json::to_value(&paired_to_test_changes)?;
    anyhow::ensure!(
        result["record_file"] == record_path.to_string_lossy().as_ref()
            && result["record_id"] == record.capsule_sha256
            && result["record_sha256"] == record_sha256
            && result["source_thread"] == record.source_thread_id.to_string()
            && result["successor_thread"] == record.successor_thread_id.to_string()
            && result["launch_count"] == 1
            && result["continuation_count"] == 1
            && result["continuation_submit_count"] == 1
            && result["input_delivery_sequence"]
                == serde_json::json!([
                    "bracketed-paste",
                    "cold-owner-readonly-resume",
                    "same-process-continuing-reobserve",
                    "enter"
                ])
            && result["source_exit_keys"]
                == serde_json::json!(["Escape", "C-u", "C-c", "C-c", "C-u"])
            && result["child_returncode_after_cleanup"] == 0
            && result["successor_rollout_copy_bytes_equal"] == true
            && result["successor_rollout_sha256"].as_str().is_some()
            && result["cold_resume_tested"] == false
            && result["cold_owner_status"] == "parent_handoff_continuing"
            && result["cold_owner_returncode"] == 0
            && result["cold_owner_pid"].as_u64().is_some()
            && result["cold_owner_pid"] != result["child_pid"]
            && result["cold_owner_process_pid"].as_u64().is_some()
            && result["cold_owner_process_pid"] == result["cold_owner_pid"]
            && result["cold_owner_process_group_id"] == test_process_group_id
            && result["cold_owner_process_reaped"] == true
            && result["cold_owner_send_count"] == 0
            && result["cold_owner_ledger_sha256"].as_str().is_some()
            && result["real_supervisor_restart_tested"] == false
            && result["successor_cli_started"] == true
            && result["cli_auth_file_store"] == true
            && result["cli_auth_dummy_key_only"] == true
            && result["cli_api_key_environment_absent"] == true
            && result["cli_workspace_trusted"] == true
            && result["successor_cli_returncode_after_cleanup"] == 0
            && result["test_process_group_id"] == test_process_group_id
            && result["test_process_id"] == test_process_group_id
            && result["python_process_id"]
                .as_i64()
                .is_some_and(|pid| pid != test_process_group_id as i64)
            && result["python_process_group_id"] == test_process_group_id
            && result["source_process_group_id"] == test_process_group_id
            && result["duplicate_start_process_group_id"] == test_process_group_id
            && result["successor_cli_process_group_id"] == test_process_group_id
            && result["successor_cli_inherited_test_group"] == true
            && result["successor_cli_reaped"] == true
            && result["duplicate_start_reaped"] == true
            && result["source_process_reaped"] == true
            && result["cleanup_signal_scope"] == "owned-popen-pids-only"
            && result["process_group_isolated_by_nextest"] == true
            && result["cleanup_signal_targets"]
                .as_array()
                .is_some_and(|targets| {
                    targets.contains(&result["source_process_pid"])
                        && targets.iter().all(|pid| {
                            pid == &result["source_process_pid"]
                                || pid == &result["child_pid"]
                                || pid == &result["duplicate_start_process_pid"]
                        })
                })
            && result["duplicate_native_start_rejected"] == true
            && result["duplicate_start_child_pid"].as_u64().is_some()
            && result["duplicate_start_child_pid"] != result["child_pid"]
            && result["duplicate_start_returncode"] == 0
            && result["owner_final_status"] == "parent_handoff_adopted"
            && result["cli_binary_sha256"] == actual_cli_sha256
            && result["cli_binary_manifest_source_clean"] == true
            && result["cli_source_commit"] == cli_source_commit
            && result["paired_to_test_source_changes"] == paired_to_test_changes_json
            && result["cli_argv_observed"] == true
            && result["task_started_delta"] == 1
            && result["cli_manifest_path"] == cli_manifest_path.to_string_lossy().as_ref()
            && result["test_source_head"] == test_source_head
            && result["cli_source_commit_matches_test_head"] == cli_source_commit_matches_test_head
            && result["parent_handoff_source_sha256"] == parent_handoff_sha256
            && result["tools_root"] == tools_root_path.to_string_lossy().as_ref()
            && result["parent_handoff_source"]
                == tools_root_path
                    .join("tools/observability/ren2_supervisor/parent_handoff.py")
                    .to_string_lossy()
                    .as_ref()
            && result["tools_commit"] == tools_commit,
        "C29 bridge result did not preserve the one-record/one-child contract: {result}"
    );
    if let Some(evidence_root) = std::env::var_os("RENCROW_C29_EVIDENCE_DIR") {
        let cli_config_path = codex_home.join(codex_config::CONFIG_TOML_FILE);
        let cli_evidence_sources = C29CliEvidenceSources {
            config: &cli_config_path,
            manifest: &cli_manifest_path,
            tools_root: &tools_root_path,
        };
        preserve_c29_evidence(
            std::path::Path::new(&evidence_root),
            &base,
            record_path,
            &record.capsule_file,
            &capsule.source_rollout,
            &cli_evidence_sources,
            record,
        )?;
    }
    Ok(())
}

#[cfg(unix)]
fn c29_test_process_group_id() -> anyhow::Result<i32> {
    let process_id = unsafe { libc::getpid() };
    let process_group_id = unsafe { libc::getpgrp() };
    anyhow::ensure!(
        process_id == process_group_id,
        "C29 requires the nextest-owned test process to lead its isolated process group; pid={process_id}, pgid={process_group_id}"
    );
    Ok(process_id)
}

#[cfg(not(unix))]
fn c29_test_process_group_id() -> anyhow::Result<i32> {
    anyhow::bail!("C29 process-group acceptance requires Unix nextest semantics")
}

struct C29CliEvidenceSources<'a> {
    config: &'a std::path::Path,
    manifest: &'a std::path::Path,
    tools_root: &'a std::path::Path,
}

fn preserve_c29_evidence(
    evidence_root: &std::path::Path,
    run_directory: &std::path::Path,
    record_path: &std::path::Path,
    capsule_path: &std::path::Path,
    source_rollout: &std::path::Path,
    cli_sources: &C29CliEvidenceSources<'_>,
    record: &runtime::HandoffRecord,
) -> anyhow::Result<()> {
    let destination = evidence_root.join(format!("c29-{}", record.successor_thread_id));
    anyhow::ensure!(
        !destination.exists(),
        "refusing to overwrite existing C29 evidence directory: {}",
        destination.display()
    );
    std::fs::create_dir_all(&destination)?;
    let artifacts = [
        ("sealed-handoff.json", record_path.to_path_buf()),
        ("handoff-capsule.json", capsule_path.to_path_buf()),
        ("source-rollout.jsonl", source_rollout.to_path_buf()),
        ("cli-config.toml", cli_sources.config.to_path_buf()),
        (
            "cli-build-manifest.json",
            cli_sources.manifest.to_path_buf(),
        ),
        (
            "parent-handoff-source.py",
            cli_sources
                .tools_root
                .join("tools/observability/ren2_supervisor/parent_handoff.py"),
        ),
        (
            "supervise-source.py",
            cli_sources
                .tools_root
                .join("tools/observability/ren2_supervisor/supervise.py"),
        ),
        (
            "native-successor-rollout.jsonl",
            run_directory.join("native-successor-rollout.jsonl"),
        ),
        (
            "harness-input.json",
            run_directory.join("harness-input.json"),
        ),
        (
            "python-owner-result.json",
            run_directory.join("result.json"),
        ),
        (
            "cold-owner-result.json",
            run_directory.join("cold-owner-result.json"),
        ),
        (
            "duplicate-start-proof.json",
            run_directory.join("duplicate-start-proof.json"),
        ),
        (
            "cli-process-proof.json",
            run_directory.join("cli-process-proof.json"),
        ),
        (
            "successor-cli-pty.log",
            run_directory.join("successor-cli-pty.log"),
        ),
        (
            "duplicate-start.log",
            run_directory.join("duplicate-start.log"),
        ),
        (
            "supervisor-ledger.json",
            run_directory.join("escalation-ledger.json"),
        ),
        ("settings.json", run_directory.join("settings.json")),
        (
            "completion-contract.json",
            run_directory.join("completion-contract.json"),
        ),
        (
            "executor-policy.json",
            run_directory.join("executor-policy.json"),
        ),
    ];
    let hashes = copy_c29_evidence_artifacts(&destination, &artifacts)?;
    std::fs::write(
        destination.join("artifact-hashes.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "record_id": record.capsule_sha256,
            "source_thread": record.source_thread_id.to_string(),
            "successor_thread": record.successor_thread_id.to_string(),
            "tools_scope": "git archive HEAD; commit and source hash pinned in python-owner-result.json",
            "artifacts": hashes,
        }))?,
    )?;
    Ok(())
}

fn preserve_c29_failure_evidence(
    evidence_root: &std::path::Path,
    run_directory: &std::path::Path,
    record_path: &std::path::Path,
    capsule_path: &std::path::Path,
    output: &std::process::Output,
    child_log_path: &std::path::Path,
    record: &runtime::HandoffRecord,
) -> anyhow::Result<std::path::PathBuf> {
    std::fs::create_dir_all(evidence_root)?;
    let destination = evidence_root.join(format!(
        "c29-failure-{}-{}",
        record.successor_thread_id, record.capsule_sha256
    ));
    std::fs::create_dir(&destination).map_err(|error| {
        anyhow::anyhow!(
            "create new C29 failure evidence directory {} (existing evidence is never overwritten): {error}",
            destination.display()
        )
    })?;

    let stdout_path = run_directory.join("python-bridge.stdout.bin");
    let stderr_path = run_directory.join("python-bridge.stderr.bin");
    std::fs::write(&stdout_path, &output.stdout)?;
    std::fs::write(&stderr_path, &output.stderr)?;
    let absent_log_path = run_directory.join("successor-cli-pty.log.absent.txt");
    let preserved_log_path = if child_log_path.exists() {
        child_log_path.to_path_buf()
    } else {
        std::fs::write(
            &absent_log_path,
            format!("CLI PTY log was absent at {}\n", child_log_path.display()),
        )?;
        absent_log_path
    };
    let mut artifacts = vec![
        ("sealed-handoff.json", record_path.to_path_buf()),
        ("handoff-capsule.json", capsule_path.to_path_buf()),
        (
            "harness-input.json",
            run_directory.join("harness-input.json"),
        ),
        ("python-bridge.stdout.bin", stdout_path),
        ("python-bridge.stderr.bin", stderr_path),
        ("successor-cli-diagnostic.log", preserved_log_path),
    ];
    let python_diagnostic = run_directory.join("failure-diagnostic.json");
    if python_diagnostic.exists() {
        artifacts.push(("python-failure-diagnostic.json", python_diagnostic));
    }
    let hashes = copy_c29_evidence_artifacts(&destination, &artifacts)?;
    std::fs::write(
        destination.join("failure-artifact-manifest.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "bridge_exit_status": output.status.to_string(),
            "record_id": record.capsule_sha256,
            "source_thread": record.source_thread_id.to_string(),
            "successor_thread": record.successor_thread_id.to_string(),
            "auth_material_saved": false,
            "artifacts": hashes,
        }))?,
    )?;
    Ok(destination)
}

fn copy_c29_evidence_artifacts(
    destination: &std::path::Path,
    artifacts: &[(&str, std::path::PathBuf)],
) -> anyhow::Result<serde_json::Map<String, serde_json::Value>> {
    use sha2::Digest;

    let mut hashes = serde_json::Map::new();
    for (name, source) in artifacts {
        let bytes = std::fs::read(source)
            .map_err(|error| anyhow::anyhow!("read C29 evidence {}: {error}", source.display()))?;
        let copied_path = destination.join(name);
        std::fs::copy(source, &copied_path)?;
        let copied = std::fs::read(&copied_path)?;
        anyhow::ensure!(
            bytes == copied,
            "C29 evidence copy changed bytes for {name}"
        );
        hashes.insert(
            (*name).to_string(),
            serde_json::Value::String(format!("{:x}", sha2::Sha256::digest(&copied))),
        );
    }
    Ok(hashes)
}

/// Explicit subprocess-only native import helper. The parent test supplies a real sealed record.
#[tokio::test]
#[ignore = "spawned only by the isolated C29 Python owner harness"]
async fn parent_handoff_os_child_imports_same_record() -> anyhow::Result<()> {
    let record_path = std::path::PathBuf::from(std::env::var("C29_HANDOFF_PATH")?);
    let workspace = std::path::PathBuf::from(std::env::var("C29_WORKSPACE")?);
    let provider_url = std::env::var("C29_PROVIDER_URL")?;
    let codex_home = std::path::PathBuf::from(std::env::var("C29_CODEX_HOME")?);
    let ready_file = std::path::PathBuf::from(std::env::var("C29_READY_FILE")?);
    let release_file = std::path::PathBuf::from(std::env::var("C29_RELEASE_FILE")?);
    let done_file = std::path::PathBuf::from(std::env::var("C29_DONE_FILE")?);
    let (record, capsule, _) = runtime::load_verified(&record_path).map_err(anyhow::Error::msg)?;
    let source_generation = capsule.original_snapshot["resume_generation"]
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("sealed source snapshot omits resume_generation"))?;
    let expected_successor_generation = source_generation
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("sealed source resume_generation overflowed"))?;
    let handoff_directory = record_path
        .parent()
        .and_then(std::path::Path::parent)
        .ok_or_else(|| anyhow::anyhow!("handoff directory is missing"))?;
    let workspace_path =
        codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(&workspace)?;
    let handoff_path =
        codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(handoff_directory)?;
    let (_source_session, source_turn, _) =
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
                config.rencrow_parent_handoff_dir = Some(handoff_path);
                config.model_auto_compact_token_limit = Some(1);
                config.model_auto_compact_token_limit_scope =
                    codex_protocol::config_types::AutoCompactTokenLimitScope::Total;
                config.model_provider.base_url = Some(provider_url);
                config.model_providers.insert(
                    config.model_provider_id.clone(),
                    config.model_provider.clone(),
                );
            },
        )
        .await;
    let mut owner_config = source_turn.config.as_ref().clone();
    owner_config.codex_home =
        codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(&codex_home)?;
    owner_config.rencrow_parent_handoff_file =
        Some(codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(&record_path)?);
    let manager = crate::ThreadManager::with_models_provider_and_home_for_tests(
        codex_login::CodexAuth::from_api_key("test"),
        owner_config.model_provider.clone(),
        owner_config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
    );
    let mut successor_options = crate::StartThreadOptions::new(owner_config.clone());
    successor_options.session_source = Some(codex_protocol::protocol::SessionSource::Cli);
    let successor = manager.start_thread(successor_options).await?;
    anyhow::ensure!(successor.thread_id == record.successor_thread_id);
    runtime::claim(&record_path, record.successor_thread_id)
        .expect_err("successor process must not claim the imported record twice");
    let mut duplicate_options = crate::StartThreadOptions::new(owner_config);
    duplicate_options.session_source = Some(codex_protocol::protocol::SessionSource::Cli);
    anyhow::ensure!(
        manager.start_thread(duplicate_options).await.is_err(),
        "successor process started the same handoff record twice"
    );
    {
        let state = successor.thread.session.state.lock().await;
        let (root, restored) = state
            .rencrow_completion_state
            .as_ref()
            .expect("native successor restores the task state");
        anyhow::ensure!(root == "root" && *restored == open_state(false));
        anyhow::ensure!(
            state.rencrow_completion_resume_generation == expected_successor_generation
        );
        anyhow::ensure!(!state.rencrow_completion_paused);
    }
    let imported = successor.thread.conversation_history_snapshot().await;
    anyhow::ensure!(imported.items().any(|item| match item {
        codex_protocol::models::ResponseItem::Message { role, content, .. } => {
            role == "user"
                && content.iter().any(|item| {
                    matches!(item, codex_protocol::models::ContentItem::InputText { text }
                        if text == "Preserve this user constraint.")
                })
        }
        _ => false,
    }));
    let proof = serde_json::json!({
        "record_file": record_path,
        "record_id": record.capsule_sha256,
        "source_thread": record.source_thread_id.to_string(),
        "successor_thread": record.successor_thread_id.to_string(),
        "generation": expected_successor_generation,
        "duplicate_claim_rejected": true,
        "duplicate_start_rejected": true,
    });
    std::fs::write(&ready_file, serde_json::to_vec(&proof)?)?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(25);
    while !release_file.exists() && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    anyhow::ensure!(
        release_file.exists(),
        "native child release signal was not received"
    );
    successor.thread.shutdown_and_wait().await?;
    std::fs::write(&done_file, serde_json::to_vec(&proof)?)?;
    Ok(())
}

/// Explicit subprocess-only duplicate-start proof while the real CLI owns the record.
#[tokio::test]
#[ignore = "spawned only by the isolated C29 Python owner harness"]
async fn parent_handoff_os_child_rejects_claimed_record() -> anyhow::Result<()> {
    use crate::config::ConfigBuilder;
    let record_path = std::path::PathBuf::from(std::env::var("C29_HANDOFF_PATH")?);
    let workspace = std::path::PathBuf::from(std::env::var("C29_WORKSPACE")?);
    let codex_home = std::path::PathBuf::from(std::env::var("C29_CODEX_HOME")?);
    let proof_path = std::path::PathBuf::from(std::env::var("C29_DUPLICATE_PROOF")?);
    let duplicate_config = ConfigBuilder::default()
        .codex_home(codex_home)
        .fallback_cwd(Some(workspace))
        .cli_overrides(vec![(
            "rencrow_parent_handoff_file".to_string(),
            toml::Value::String(record_path.to_string_lossy().into_owned()),
        )])
        .build()
        .await?;
    let manager = crate::ThreadManager::with_models_provider_and_home_for_tests(
        codex_login::CodexAuth::from_api_key("test"),
        duplicate_config.model_provider.clone(),
        duplicate_config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
    );
    let mut options = crate::StartThreadOptions::new(duplicate_config);
    options.session_source = Some(codex_protocol::protocol::SessionSource::Cli);
    let error = match manager.start_thread(options).await {
        Ok(started) => {
            started.thread.shutdown_and_wait().await?;
            anyhow::bail!("a second process started the CLI-owned handoff");
        }
        Err(error) => error,
    };
    anyhow::ensure!(
        error.to_string().contains("claimed or unavailable"),
        "duplicate start failed for an unexpected reason: {error}"
    );
    std::fs::write(
        &proof_path,
        serde_json::to_vec(&serde_json::json!({
            "record_file": record_path,
            "duplicate_start_child_pid": std::process::id(),
            "duplicate_start_rejected": true,
            "reason": error.to_string(),
        }))?,
    )?;
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
