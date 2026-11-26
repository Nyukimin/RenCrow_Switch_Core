use super::*;
use crate::config::PermissionProfileSnapshot;
use crate::environment_selection::EnvironmentConfigOrigin;
use crate::tools::sandboxing::SandboxAttempt;
use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::models::AdditionalPermissionProfile;
use codex_protocol::models::FileSystemPermissions;
use codex_protocol::models::PermissionProfile;
use codex_protocol::permissions::FileSystemAccessMode;
use codex_protocol::permissions::FileSystemPath;
use codex_protocol::permissions::FileSystemSandboxEntry;
use codex_protocol::permissions::FileSystemSandboxPolicy;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_protocol::protocol::EnvironmentConfig;
use codex_protocol::protocol::EnvironmentConfigState;
use codex_protocol::protocol::GranularApprovalConfig;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_sandboxing::SandboxManager;
use codex_sandboxing::SandboxType;
use codex_sandboxing::policy_transforms::effective_file_system_sandbox_policy;
use codex_sandboxing::policy_transforms::effective_network_sandbox_policy;
use codex_utils_path_uri::PathUri;
use core_test_support::PathBufExt;
use pretty_assertions::assert_eq;
use std::collections::HashMap;
fn test_turn_environment(environment_id: &str) -> crate::session::turn_context::TurnEnvironment {
    crate::session::turn_context::TurnEnvironment::new(
        TurnEnvironmentSelection {
            environment_id: environment_id.to_string(),
            cwd: PathUri::from_abs_path(&std::env::temp_dir().abs()),
            workspace_roots: Vec::new(),
            config: EnvironmentConfigState::Ready(EnvironmentConfig {
                allow_login_shell: true,
                workspace_roots: Vec::new(),
                windows_sandbox_level: WindowsSandboxLevel::Disabled,
                windows_sandbox_type: SandboxType::None,
                use_legacy_landlock: false,
                permission_profile: PermissionProfileSnapshot::legacy(
                    PermissionProfile::read_only(),
                ),
                shell_environment_policy: Default::default(),
                exec_policy: None,
                mcp_policy: None,
                network_policy: None,
                selected_capability_roots: Vec::new(),
            }),
        },
        EnvironmentConfigOrigin::Thread,
        std::sync::Arc::new(codex_exec_server::Environment::default_for_tests()),
        /*shell*/ None,
    )
}

#[test]
fn wants_no_sandbox_approval_granular_respects_sandbox_flag() {
    let runtime = ApplyPatchRuntime::new();
    assert!(runtime.wants_no_sandbox_approval(AskForApproval::OnRequest));
    assert!(
        !runtime.wants_no_sandbox_approval(AskForApproval::Granular(GranularApprovalConfig {
            sandbox_approval: false,
            rules: true,
            skill_approval: true,
            request_permissions: true,
            mcp_elicitations: true,
        }))
    );
    assert!(
        runtime.wants_no_sandbox_approval(AskForApproval::Granular(GranularApprovalConfig {
            sandbox_approval: true,
            rules: true,
            skill_approval: true,
            request_permissions: true,
            mcp_elicitations: true,
        }))
    );
}

#[tokio::test]
async fn approval_action_preserves_patch_path_uris() {
    let path = PathUri::parse("file:///C:/workspace/guardian-apply-patch-test.txt")
        .expect("valid foreign path URI");
    let action = ApplyPatchAction::new_add_for_test(&path, "hello".to_string());
    let expected_cwd = action.cwd.clone();
    let expected_patch = action.patch.clone();
    let request = ApplyPatchRequest {
        turn_environment: test_turn_environment(codex_exec_server::LOCAL_ENVIRONMENT_ID),
        action,
        file_paths: vec![path.clone()],
        changes: Arc::new(HashMap::new()),
        exec_approval_requirement: ExecApprovalRequirement::NeedsApproval {
            reason: None,
            proposed_execpolicy_amendment: None,
        },
        additional_permissions: None,
        permissions_preapproved: false,
    };

    let approval_action = ApplyPatchRuntime::build_approval_action(&request, "call-1");

    assert_eq!(
        approval_action,
        ApprovalAction::ApplyPatch {
            id: "call-1".to_string(),
            environment_id: codex_exec_server::LOCAL_ENVIRONMENT_ID.to_string(),
            cwd: expected_cwd,
            files: vec![path],
            patch: expected_patch,
            changes: Arc::new(HashMap::new()),
            permissions_preapproved: false,
        }
    );
}

#[tokio::test]
async fn permission_request_payload_uses_apply_patch_hook_name_and_aliases() {
    let path = std::env::temp_dir()
        .join("apply-patch-permission-request-payload.txt")
        .abs();
    let action =
        ApplyPatchAction::new_add_for_test(&PathUri::from_abs_path(&path), "hello".to_string());
    let expected_patch = action.patch.clone();
    let req = ApplyPatchRequest {
        turn_environment: test_turn_environment(codex_exec_server::LOCAL_ENVIRONMENT_ID),
        action,
        file_paths: vec![PathUri::from_abs_path(&path)],
        changes: Arc::new(HashMap::new()),
        exec_approval_requirement: ExecApprovalRequirement::NeedsApproval {
            reason: None,
            proposed_execpolicy_amendment: None,
        },
        additional_permissions: None,
        permissions_preapproved: false,
    };

    let payload =
        ApplyPatchRuntime::build_approval_action(&req, "call-1").permission_request_payload();

    assert_eq!(payload.tool_name.name(), "apply_patch");
    assert_eq!(
        payload.tool_name.matcher_aliases(),
        &["Write".to_string(), "Edit".to_string()]
    );
    assert_eq!(
        payload.tool_input,
        serde_json::json!({ "command": expected_patch })
    );
}

#[tokio::test]
async fn approval_keys_include_environment_id() {
    let runtime = ApplyPatchRuntime::new();
    let path = std::env::temp_dir()
        .join("apply-patch-approval-key.txt")
        .abs();
    let path_uri = PathUri::from_abs_path(&path);
    let req = ApplyPatchRequest {
        turn_environment: test_turn_environment("remote"),
        action: ApplyPatchAction::new_add_for_test(&path_uri, "hello".to_string()),
        file_paths: vec![path_uri.clone()],
        changes: Arc::new(HashMap::new()),
        exec_approval_requirement: ExecApprovalRequirement::Skip {
            bypass_sandbox: false,
            proposed_execpolicy_amendment: None,
        },
        additional_permissions: None,
        permissions_preapproved: false,
    };

    let keys = runtime
        .approval_action(&req, "call-1")
        .expect("build approval action")
        .cache_keys();

    assert_eq!(
        serde_json::to_value(&keys).expect("serialize approval keys"),
        serde_json::json!([
            {
                "environment_id": "remote",
                "path": path_uri,
            }
        ])
    );
}

#[tokio::test]
async fn sandbox_cwd_uses_patch_action_cwd() {
    let runtime = ApplyPatchRuntime::new();
    let path = std::env::temp_dir()
        .join("apply-patch-runtime-sandbox-cwd.txt")
        .abs();
    let req = ApplyPatchRequest {
        turn_environment: test_turn_environment(codex_exec_server::LOCAL_ENVIRONMENT_ID),
        action: ApplyPatchAction::new_add_for_test(
            &PathUri::from_abs_path(&path),
            "hello".to_string(),
        ),
        file_paths: vec![PathUri::from_abs_path(&path)],
        changes: Arc::new(HashMap::new()),
        exec_approval_requirement: ExecApprovalRequirement::Skip {
            bypass_sandbox: false,
            proposed_execpolicy_amendment: None,
        },
        additional_permissions: None,
        permissions_preapproved: false,
    };

    assert_eq!(runtime.sandbox_cwd(&req), Some(&req.action.cwd));
}

#[tokio::test]
async fn file_system_sandbox_context_preserves_executor_workspace_permissions() {
    let path = std::env::temp_dir()
        .join("apply-patch-runtime-attempt.txt")
        .abs();
    let additional_permissions = AdditionalPermissionProfile {
        network: None,
        file_system: Some(FileSystemPermissions::from_read_write_roots(
            Some(vec![path.clone()]),
            Some(Vec::new()),
        )),
    };
    let req = ApplyPatchRequest {
        turn_environment: test_turn_environment(codex_exec_server::LOCAL_ENVIRONMENT_ID),
        action: ApplyPatchAction::new_add_for_test(
            &PathUri::from_abs_path(&path),
            "hello".to_string(),
        ),
        file_paths: vec![PathUri::from_abs_path(&path)],
        changes: Arc::new(HashMap::new()),
        exec_approval_requirement: ExecApprovalRequirement::Skip {
            bypass_sandbox: false,
            proposed_execpolicy_amendment: None,
        },
        additional_permissions: Some(additional_permissions.clone()),
        permissions_preapproved: false,
    };
    let exec_server_permissions = PermissionProfile::workspace_write();
    let file_system_policy = exec_server_permissions.file_system_sandbox_policy();
    let permissions = exec_server_permissions
        .clone()
        .materialize_project_roots_with_workspace_roots(std::slice::from_ref(&path));
    let manager = SandboxManager::new();
    let sandbox_policy_cwd = PathUri::from_abs_path(&path);
    let attempt = SandboxAttempt {
        sandbox: SandboxType::MacosSeatbelt,
        sandbox_requested: true,
        permissions: &permissions,
        exec_server_permissions: &exec_server_permissions,
        enforce_managed_network: false,
        manager: &manager,
        sandbox_cwd: &sandbox_policy_cwd,
        workspace_roots: std::slice::from_ref(&sandbox_policy_cwd),
        sandbox_exe: None,
        use_legacy_landlock: true,
        windows_sandbox_type: SandboxType::WindowsRestrictedToken,
        windows_sandbox_level: WindowsSandboxLevel::RestrictedToken,
        network_denial_cancellation_token: None,
        network_proxy: None,
    };

    let sandbox = ApplyPatchRuntime::file_system_sandbox_context_for_attempt(&req, &attempt)
        .expect("sandbox context");

    let file_system_policy =
        effective_file_system_sandbox_policy(&file_system_policy, Some(&additional_permissions));
    let network_policy = effective_network_sandbox_policy(
        NetworkSandboxPolicy::Restricted,
        Some(&additional_permissions),
    );
    let expected_permissions =
        PermissionProfile::from_runtime_permissions(&file_system_policy, network_policy);
    assert_eq!(sandbox.permissions, expected_permissions);
    assert_eq!(
        sandbox.cwd,
        codex_utils_path_uri::PathUri::from_abs_path(&path)
    );
    assert_eq!(
        sandbox.windows_sandbox_selection,
        if cfg!(windows) {
            codex_file_system::WindowsSandboxSelection::RestrictedToken
        } else {
            codex_file_system::WindowsSandboxSelection::Disabled
        }
    );
    assert_eq!(sandbox.use_legacy_landlock, true);
}

#[tokio::test]
async fn file_system_sandbox_context_respects_sandbox_request() {
    let path = std::env::temp_dir()
        .join("apply-patch-runtime-none.txt")
        .abs();
    let mut req = ApplyPatchRequest {
        turn_environment: test_turn_environment(codex_exec_server::LOCAL_ENVIRONMENT_ID),
        action: ApplyPatchAction::new_add_for_test(
            &PathUri::from_abs_path(&path),
            "hello".to_string(),
        ),
        file_paths: vec![PathUri::from_abs_path(&path)],
        changes: Arc::new(HashMap::new()),
        exec_approval_requirement: ExecApprovalRequirement::Skip {
            bypass_sandbox: false,
            proposed_execpolicy_amendment: None,
        },
        additional_permissions: None,
        permissions_preapproved: false,
    };
    let permissions = PermissionProfile::Disabled;
    let manager = SandboxManager::new();
    let sandbox_policy_cwd = PathUri::from_abs_path(&path);
    let attempt = SandboxAttempt {
        sandbox: SandboxType::None,
        sandbox_requested: false,
        permissions: &permissions,
        exec_server_permissions: &permissions,
        enforce_managed_network: false,
        manager: &manager,
        sandbox_cwd: &sandbox_policy_cwd,
        workspace_roots: std::slice::from_ref(&sandbox_policy_cwd),
        sandbox_exe: None,
        use_legacy_landlock: false,
        windows_sandbox_type: SandboxType::None,
        windows_sandbox_level: WindowsSandboxLevel::Disabled,
        network_denial_cancellation_token: None,
        network_proxy: None,
    };

    assert_eq!(
        ApplyPatchRuntime::file_system_sandbox_context_for_attempt(&req, &attempt),
        None
    );

    let cwd = PathUri::parse("file:///C:/workspace").expect("Windows workspace URI");
    let user_home_dir = PathUri::parse("file:///C:/Users/remote").expect("Windows home URI");
    req.turn_environment.user_home_dir = Some(user_home_dir.clone());
    let permissions = PermissionProfile::workspace_write();
    let attempt = SandboxAttempt {
        sandbox_requested: true,
        permissions: &permissions,
        exec_server_permissions: &permissions,
        sandbox_cwd: &cwd,
        workspace_roots: std::slice::from_ref(&cwd),
        ..attempt
    };

    assert_eq!(
        ApplyPatchRuntime::file_system_sandbox_context_for_attempt(&req, &attempt),
        Some(FileSystemSandboxContext {
            permissions,
            cwd: cwd.clone(),
            workspace_roots: vec![cwd],
            user_home_dir: Some(user_home_dir),
            temporary_directories: None,
            windows_sandbox_selection: codex_file_system::WindowsSandboxSelection::RestrictedToken,
            windows_sandbox_proxy_settings_mode: None,
            use_legacy_landlock: false,
        })
    );
}

#[cfg(unix)]
#[test]
fn common_harness_effect_alias_projection_preserves_explicit_denials() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let physical_root = temp.path().join("physical");
    let physical_nested = temp.path().join("physical-nested");
    std::fs::create_dir_all(&physical_root).expect("physical root");
    std::fs::create_dir_all(&physical_nested).expect("physical nested root");
    let outer_alias = temp.path().join("outer-alias");
    std::os::unix::fs::symlink(&physical_root, &outer_alias).expect("outer symlink alias");
    let inner_alias = physical_root.join("inner-alias");
    std::os::unix::fs::symlink(&physical_nested, &inner_alias).expect("inner symlink alias");
    let action_path = outer_alias.join("inner-alias");
    let effect_path = action_path.canonicalize().expect("canonical workspace");
    let action_cwd = PathUri::from_host_native_path(&action_path).expect("action cwd URI");
    let effect_cwd = PathUri::from_host_native_path(&effect_path).expect("effect cwd URI");
    let denied_path = action_path.join("blocked.txt");
    let denied_effect_path = effect_path.join("blocked.txt");
    let policy = FileSystemSandboxPolicy::restricted(vec![
        FileSystemSandboxEntry::new(
            FileSystemPath::Path {
                path: action_cwd.clone(),
            },
            FileSystemAccessMode::Write,
        ),
        FileSystemSandboxEntry::new(
            FileSystemPath::Path {
                path: PathUri::from_host_native_path(&denied_path).expect("denied path URI"),
            },
            FileSystemAccessMode::Deny,
        ),
    ]);
    let permissions =
        PermissionProfile::from_runtime_permissions(&policy, NetworkSandboxPolicy::Restricted);
    let sandbox =
        FileSystemSandboxContext::from_permission_profile(permissions, action_cwd.clone());

    let projected =
        ApplyPatchRuntime::align_sandbox_paths_with_effect_root(sandbox, &action_cwd, &effect_cwd)
            .expect("project sandbox policy paths");
    let projected_policy = projected.permissions.file_system_sandbox_policy();
    assert!(projected_policy.can_write_local_path_with_cwd(&effect_path, &effect_path,));
    assert!(!projected_policy.can_write_local_path_with_cwd(&denied_effect_path, &effect_path,));
    assert_eq!(projected.cwd, effect_cwd);
    assert_eq!(projected.workspace_roots, vec![effect_cwd]);
}

#[cfg(unix)]
#[test]
fn common_harness_effect_alias_projection_rejects_unprojectable_glob_denials() {
    let temp = tempfile::tempdir().expect("temporary workspace");
    let physical_root = temp.path().join("physical");
    std::fs::create_dir_all(&physical_root).expect("physical root");
    let alias_root = temp.path().join("alias");
    std::os::unix::fs::symlink(&physical_root, &alias_root).expect("root symlink alias");
    let effect_root = alias_root.canonicalize().expect("canonical workspace");
    let action_cwd = PathUri::from_host_native_path(&alias_root).expect("action cwd URI");
    let effect_cwd = PathUri::from_host_native_path(&effect_root).expect("effect cwd URI");
    let policy = FileSystemSandboxPolicy::restricted(vec![FileSystemSandboxEntry::new(
        FileSystemPath::GlobPattern {
            pattern: format!("{}/**/secret", alias_root.display()),
        },
        FileSystemAccessMode::Deny,
    )]);
    let sandbox = FileSystemSandboxContext::from_permission_profile(
        PermissionProfile::from_runtime_permissions(&policy, NetworkSandboxPolicy::Restricted),
        action_cwd.clone(),
    );

    let error =
        ApplyPatchRuntime::align_sandbox_paths_with_effect_root(sandbox, &action_cwd, &effect_cwd)
            .expect_err("glob denial must fail closed across an alias");
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
}
