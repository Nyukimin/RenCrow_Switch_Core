use super::*;
use crate::config::AgentRoleConfig;
use crate::config::ConfigBuilder;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context;
use codex_protocol::permissions::FileSystemAccessMode;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use pretty_assertions::assert_eq;
use std::sync::Arc;

#[test]
fn explicit_spawn_model_overrides_take_precedence_over_configured_defaults() {
    let (model, effort) = resolve_spawn_agent_model_overrides(
        Some("explicit-model"),
        Some(ReasoningEffort::XHigh),
        Some("configured-model"),
        Some(ReasoningEffort::Low),
        /*include_configured_defaults*/ true,
    );

    assert_eq!(model, Some("explicit-model"));
    assert_eq!(effort, Some(ReasoningEffort::XHigh));
}

#[test]
fn post_role_override_pass_does_not_apply_unspecified_configured_defaults() {
    let (model, effort) = resolve_spawn_agent_model_overrides(
        None,
        None,
        Some("configured-model"),
        Some(ReasoningEffort::Low),
        /*include_configured_defaults*/ false,
    );

    assert_eq!(model, None);
    assert_eq!(effort, None);
}

#[tokio::test]
async fn cold_resume_retains_child_model_effort_and_role_without_parent_privilege_growth() {
    let home = tempfile::tempdir().unwrap();
    let role = home.path().join("worker.toml");
    std::fs::write(&role, "model = \"role-model\"\nmodel_reasoning_effort = \"low\"\nrencrow_read_only = true\nrencrow_tool_allowlist = [\"exec_command\"]\n").unwrap();
    let mut config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(home.path().to_path_buf())
        .build()
        .await
        .unwrap();
    config.model = Some("parent-model".into());
    config.model_reasoning_effort = Some(ReasoningEffort::XHigh);
    config.rencrow_tool_allowlist = Some(vec!["exec_command".into(), "apply_patch".into()]);
    config.agent_roles.insert(
        "worker".into(),
        AgentRoleConfig {
            config_file: Some(role),
            ..Default::default()
        },
    );
    let cwd = config.cwd.clone();
    let inherited_network = config
        .permissions
        .permission_profile()
        .to_runtime_permissions()
        .1;
    let provider = config.model_provider_id.clone();
    restore_recorded_agent_config(
        &mut config,
        Some("worker"),
        Some("recorded-child-model"),
        &provider,
        Some(ReasoningEffort::High),
    )
    .await
    .unwrap();
    assert_eq!(config.model.as_deref(), Some("recorded-child-model"));
    assert_eq!(config.model_reasoning_effort, Some(ReasoningEffort::High));
    assert_eq!(config.cwd, cwd);
    assert_eq!(
        config.rencrow_tool_allowlist,
        Some(vec!["exec_command".into()])
    );
    let (files, network) = config
        .permissions
        .permission_profile()
        .to_runtime_permissions();
    assert!(
        files
            .entries
            .iter()
            .all(|entry| entry.access != FileSystemAccessMode::Write)
    );
    assert_eq!(network, inherited_network);
}

#[tokio::test]
async fn cold_resume_fails_instead_of_falling_back_from_missing_provider() {
    let home = tempfile::tempdir().unwrap();
    let mut config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(home.path().to_path_buf())
        .build()
        .await
        .unwrap();
    let error = restore_recorded_agent_config(
        &mut config,
        None,
        Some("child-model"),
        "missing-child-provider",
        Some(ReasoningEffort::High),
    )
    .await
    .unwrap_err();
    assert!(error.contains("missing-child-provider"));
}

#[tokio::test]
async fn v1_full_history_inherits_only_parent_role_model_settings_and_keeps_access_narrow() {
    const ROLE_NAME: &str = "inherited-worker";

    let (session, mut turn) = make_session_and_context().await;
    std::fs::create_dir_all(turn.config.codex_home.as_path())
        .expect("create parent codex_home for inherited role fixture");
    let role_path = turn
        .config
        .codex_home
        .as_path()
        .join("inherited-worker.toml");
    std::fs::write(
        &role_path,
        "model = \"gpt-5.4\"\nmodel_reasoning_effort = \"low\"\nrencrow_read_only = true\nrencrow_tool_allowlist = [\"exec_command\"]\ndeveloper_instructions = \"parent role instructions\"\n",
    )
    .unwrap();

    let mut parent_config = (*turn.config).clone();
    parent_config.agent_default_subagent_model = Some("gpt-5.5".to_string());
    parent_config.agent_default_subagent_reasoning_effort = Some(ReasoningEffort::High);
    parent_config.rencrow_tool_allowlist =
        Some(vec!["exec_command".to_string(), "apply_patch".to_string()]);
    parent_config.agent_roles.insert(
        ROLE_NAME.to_string(),
        AgentRoleConfig {
            config_file: Some(role_path.clone()),
            ..Default::default()
        },
    );
    apply_role_to_config(&mut parent_config, Some(ROLE_NAME))
        .await
        .expect("parent role should be applied before its full-history fork");
    turn.config = Arc::new(parent_config);
    turn.session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: session.thread_id,
        depth: 1,
        agent_path: None,
        agent_nickname: None,
        agent_role: Some(ROLE_NAME.to_string()),
    });
    turn.developer_instructions = Some("live parent turn instructions".to_string());
    let expected_provider_id = turn.config.model_provider_id.clone();
    let step_context = StepContext::for_test(Arc::new(turn));

    // Simulate a role file update after the parent turn started. A full-history child inherits
    // the parent's effective provider and developer instructions; only model and effort are
    // selected again from the role file for C16 precedence.
    std::fs::write(
        &role_path,
        "model = \"gpt-5.4\"\nmodel_reasoning_effort = \"low\"\nrencrow_read_only = false\nrencrow_tool_allowlist = [\"exec_command\", \"apply_patch\"]\nmodel_provider = \"ollama\"\ndeveloper_instructions = \"updated role instructions\"\n",
    )
    .unwrap();

    let inherited_role = prepare_agent_spawn_config(
        &session,
        &step_context,
        SpawnConfigOptions {
            version: SpawnConfigVersion::V1,
            full_history_fork: true,
            role_name: None,
            model: None,
            reasoning_effort: None,
        },
    )
    .await
    .expect("V1 full-history spawn should inherit the parent's recorded role");
    assert_eq!(inherited_role.config.model.as_deref(), Some("gpt-5.4"));
    assert_eq!(
        inherited_role.config.model_reasoning_effort,
        Some(ReasoningEffort::Low)
    );
    assert_eq!(inherited_role.role_name.as_deref(), Some(ROLE_NAME));
    assert_eq!(
        inherited_role.config.model_provider_id,
        expected_provider_id
    );
    assert_eq!(
        inherited_role.config.developer_instructions.as_deref(),
        Some("live parent turn instructions")
    );
    assert!(inherited_role.config.rencrow_read_only);
    assert_eq!(
        inherited_role.config.rencrow_tool_allowlist,
        Some(vec!["exec_command".to_string()])
    );
    assert!(
        inherited_role
            .config
            .permissions
            .permission_profile()
            .to_runtime_permissions()
            .0
            .entries
            .iter()
            .all(|entry| entry.access != FileSystemAccessMode::Write)
    );

    let explicit_model = prepare_agent_spawn_config(
        &session,
        &step_context,
        SpawnConfigOptions {
            version: SpawnConfigVersion::V1,
            full_history_fork: true,
            role_name: None,
            model: Some("gpt-5.5"),
            reasoning_effort: None,
        },
    )
    .await
    .expect("explicit model should override inherited role model");
    assert_eq!(explicit_model.config.model.as_deref(), Some("gpt-5.5"));
    assert_eq!(
        explicit_model.config.model_reasoning_effort,
        Some(ReasoningEffort::Low),
        "explicit model should preserve inherited role effort"
    );
    assert!(explicit_model.config.rencrow_read_only);

    let explicit_effort = prepare_agent_spawn_config(
        &session,
        &step_context,
        SpawnConfigOptions {
            version: SpawnConfigVersion::V1,
            full_history_fork: true,
            role_name: None,
            model: None,
            reasoning_effort: Some(ReasoningEffort::High),
        },
    )
    .await
    .expect("explicit effort should override inherited role effort");
    assert_eq!(explicit_effort.config.model.as_deref(), Some("gpt-5.4"));
    assert_eq!(
        explicit_effort.config.model_reasoning_effort,
        Some(ReasoningEffort::High)
    );
    assert!(explicit_effort.config.rencrow_read_only);

    std::fs::write(&role_path, "model = \"gpt-5.4\"\n")
        .expect("write model-only inherited role config");
    let role_model_only = prepare_agent_spawn_config(
        &session,
        &step_context,
        SpawnConfigOptions {
            version: SpawnConfigVersion::V1,
            full_history_fork: true,
            role_name: None,
            model: None,
            reasoning_effort: None,
        },
    )
    .await
    .expect("role model should combine with the global effort");
    assert_eq!(role_model_only.config.model.as_deref(), Some("gpt-5.4"));
    assert_eq!(
        role_model_only.config.model_reasoning_effort,
        Some(ReasoningEffort::High),
        "model-only role should inherit the global effort"
    );

    std::fs::write(&role_path, "model_reasoning_effort = \"low\"\n")
        .expect("write effort-only inherited role config");
    let role_effort_only = prepare_agent_spawn_config(
        &session,
        &step_context,
        SpawnConfigOptions {
            version: SpawnConfigVersion::V1,
            full_history_fork: true,
            role_name: None,
            model: None,
            reasoning_effort: None,
        },
    )
    .await
    .expect("role effort should combine with the global model");
    assert_eq!(role_effort_only.config.model.as_deref(), Some("gpt-5.5"));
    assert_eq!(
        role_effort_only.config.model_reasoning_effort,
        Some(ReasoningEffort::Low),
        "effort-only role should inherit the global model"
    );

    let (fallback_session, mut fallback_turn) = make_session_and_context().await;
    std::fs::create_dir_all(fallback_turn.config.codex_home.as_path())
        .expect("create fallback codex_home for unselected role fixture");
    let fallback_role_path = fallback_turn
        .config
        .codex_home
        .as_path()
        .join("unselected-worker.toml");
    std::fs::write(
        &fallback_role_path,
        "model = \"gpt-5.4\"\nmodel_reasoning_effort = \"low\"\n",
    )
    .unwrap();
    let mut fallback_config = (*fallback_turn.config).clone();
    fallback_config.agent_default_subagent_model = Some("gpt-5.5".to_string());
    fallback_config.agent_default_subagent_reasoning_effort = Some(ReasoningEffort::High);
    fallback_config.agent_roles.insert(
        "unselected-worker".to_string(),
        AgentRoleConfig {
            config_file: Some(fallback_role_path),
            ..Default::default()
        },
    );
    fallback_turn.config = Arc::new(fallback_config);
    let fallback_step_context = StepContext::for_test(Arc::new(fallback_turn));
    let global_fallback = prepare_agent_spawn_config(
        &fallback_session,
        &fallback_step_context,
        SpawnConfigOptions {
            version: SpawnConfigVersion::V1,
            full_history_fork: true,
            role_name: None,
            model: None,
            reasoning_effort: None,
        },
    )
    .await
    .expect("missing parent role should use configured global defaults");
    assert_eq!(global_fallback.config.model.as_deref(), Some("gpt-5.5"));
    assert_eq!(
        global_fallback.config.model_reasoning_effort,
        Some(ReasoningEffort::High)
    );
    assert_eq!(global_fallback.role_name, None);

    let (disabled_session, mut disabled_turn) = make_session_and_context().await;
    let mut disabled_config = (*disabled_turn.config).clone();
    disabled_config.agent_roles.insert(
        ROLE_NAME.to_string(),
        AgentRoleConfig {
            enabled: Some(false),
            ..Default::default()
        },
    );
    disabled_turn.config = Arc::new(disabled_config);
    disabled_turn.session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: disabled_session.thread_id,
        depth: 1,
        agent_path: None,
        agent_nickname: None,
        agent_role: Some(ROLE_NAME.to_string()),
    });
    let disabled_step_context = StepContext::for_test(Arc::new(disabled_turn));
    let disabled_parent_role = prepare_agent_spawn_config(
        &disabled_session,
        &disabled_step_context,
        SpawnConfigOptions {
            version: SpawnConfigVersion::V1,
            full_history_fork: true,
            role_name: None,
            model: None,
            reasoning_effort: None,
        },
    )
    .await
    .err()
    .expect("a disabled inherited parent role should reject the full-history fork");
    assert!(disabled_parent_role.contains("agent_type 'inherited-worker' is unavailable"));

    let (disabled_default_session, mut disabled_default_turn) = make_session_and_context().await;
    let mut disabled_default_config = (*disabled_default_turn.config).clone();
    disabled_default_config.agent_roles.insert(
        "default".to_string(),
        AgentRoleConfig {
            enabled: Some(false),
            ..Default::default()
        },
    );
    disabled_default_config.agent_roles.insert(
        ROLE_NAME.to_string(),
        AgentRoleConfig {
            enabled: Some(true),
            ..Default::default()
        },
    );
    disabled_default_turn.config = Arc::new(disabled_default_config);
    disabled_default_turn.session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: disabled_default_session.thread_id,
        depth: 1,
        agent_path: None,
        agent_nickname: None,
        agent_role: Some(ROLE_NAME.to_string()),
    });
    let disabled_default_step_context = StepContext::for_test(Arc::new(disabled_default_turn));
    let disabled_default_role = prepare_agent_spawn_config(
        &disabled_default_session,
        &disabled_default_step_context,
        SpawnConfigOptions {
            version: SpawnConfigVersion::V1,
            full_history_fork: true,
            role_name: None,
            model: None,
            reasoning_effort: None,
        },
    )
    .await
    .err()
    .expect("a disabled default role should reject a full-history fork with an inherited role");
    assert!(disabled_default_role.contains("agent_type 'default' is unavailable"));
}
