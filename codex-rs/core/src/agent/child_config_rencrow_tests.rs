use super::*;
use crate::config::AgentRoleConfig;
use crate::config::ConfigBuilder;
use codex_protocol::permissions::FileSystemAccessMode;
use pretty_assertions::assert_eq;

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
