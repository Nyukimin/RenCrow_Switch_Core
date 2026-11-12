// Modified by RenCrow Switch Core, 2026-09-22: handle durable compaction markers; 2026-09-28: role tool allowlist.
use super::input_queue::InputQueue;
use super::mcp_refresh::McpRefresh;
use super::step_context::StepContext;
use super::step_settings::ModelInfoOverrides;
use super::step_settings::StepSettings;
use super::step_settings::StepSettingsConstraints;
use super::step_settings::StepSettingsUpdate;
use super::*;
use crate::agents_md_manager::AgentsMdManager;
use crate::agents_md_manager::SessionInstructions;
use crate::config::ConstraintError;
use crate::context::GuardianContextMode;
use crate::environment_selection::ThreadEnvironments;
use crate::environment_selection::TurnEnvironmentSnapshot;
use crate::hook_mcp_executor::CoreHookMcpExecutor;
use crate::mcp_tool_call::McpToolApprovalMetadata;
use crate::responses_metadata::CodexResponsesMetadata;
use crate::responses_metadata::CodexResponsesRequestKind;
use crate::responses_metadata::CompactionTurnMetadata;
use crate::shell_snapshot::ShellSnapshot;
use crate::shell_snapshot::SnapshotCredentialBrokerState;
use crate::state::ActiveTurn;
use crate::turn_metadata::ExecutionMetadata;
use codex_attachment_store::AttachmentStore;
use codex_extension_api::ExtensionDataInit;
use codex_http_client::ClientRouteClass;
use codex_http_client::RouteAwareClientPool;
use codex_login::auth::AgentIdentityAuthPolicy;
use codex_model_provider::SharedModelProvider;
use codex_prompts::render_model_instructions;
use codex_protocol::SessionId;
use codex_protocol::capabilities::SelectedCapabilityRoot;
use codex_protocol::config_types::ShellEnvironmentPolicy;
use codex_protocol::mcp::ClientMcpExtensions;
use codex_protocol::models::ProfileWorkspaceRoot;
use codex_protocol::permissions::FileSystemPath;
use codex_protocol::permissions::FileSystemSpecialPath;
use codex_protocol::protocol::EnvironmentConfig;
use codex_protocol::protocol::HookCompletedEvent;
use codex_protocol::protocol::McpInvocation;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadSource;
use codex_protocol::protocol::TurnEnvironmentSelections;
use codex_sandboxing::SandboxType;
use codex_skills::SkillError;
use codex_utils_git_discovery::GitRootDiscovery;
use codex_utils_path::replace_path_and_deduplicate;
use std::sync::OnceLock;
use tokio::sync::Semaphore;

type McpToolApprovalMetadataMap =
    HashMap<(String, String), std::sync::Weak<(Option<McpInvocation>, McpToolApprovalMetadata)>>;

/// Context for an initialized model agent
///
/// A session has at most 1 running task at a time, and can be interrupted by user input.
pub(crate) struct Session {
    pub(crate) thread_id: ThreadId,
    pub(crate) installation_id: String,
    pub(super) tx_event: Sender<Event>,
    pub(super) agent_status: watch::Sender<AgentStatus>,
    pub(super) state: Mutex<SessionState>,
    /// Orders accepted settings commits and their persisted events with compaction checkpoints.
    /// Keep this separate from `state` so storage I/O does not block runtime state access.
    pub(super) thread_settings_persistence: Semaphore,
    /// Serializes rebuild/apply cycles for the running proxy; each cycle
    /// rebuilds from the current SessionState while holding this lock.
    pub(super) managed_network_proxy_refresh_lock: Semaphore,
    /// The set of enabled features should be invariant for the lifetime of the
    /// session.
    pub(super) features: ManagedFeatures,
    pub(crate) guardian_context_mode: GuardianContextMode,
    pub(super) isolation: codex_extension_api::SessionIsolation,
    pub(crate) tool_policy: Arc<codex_extension_api::ToolPolicy>,
    pub(crate) windows_sandbox_proxy_settings_mode:
        codex_sandboxing::WindowsSandboxProxySettingsMode,
    pub(super) multi_agent_version: OnceLock<MultiAgentVersion>,
    /// Owns invalidation and serializes refreshes without blocking captured calls.
    pub(super) mcp_refresh: McpRefresh,
    /// Non-owning lookup for approval data retained by running MCP invocations.
    pub(crate) mcp_tool_approval_metadata: std::sync::Mutex<McpToolApprovalMetadataMap>,
    pub(super) mcp_elicitation_reviewer_handle: OnceLock<codex_mcp::ElicitationReviewerHandle>,
    pub(super) mcp_elicitation_lifecycle_handle: OnceLock<codex_mcp::ElicitationLifecycle>,
    pub(super) mcp_prewarm_tx: async_channel::Sender<()>,
    pub(super) mcp_prewarm_shutdown: CancellationToken,
    pub(super) mcp_prewarm_task: std::sync::Mutex<Option<JoinHandle<()>>>,
    pub(crate) conversation: Arc<RealtimeConversationManager>,
    pub(crate) realtime_history: Option<Mutex<crate::realtime_history::RealtimeHistoryState>>,
    pub(crate) active_turn: Mutex<Option<ActiveTurn>>,
    pub(crate) async_hook_results: async_channel::Receiver<HookCompletedEvent>,
    pub(crate) input_queue: InputQueue,
    pub(crate) services: SessionServices,
    pub(super) git_enrichment_policy: GitEnrichmentPolicy,
    pub(super) fork_persistence: ForkPersistence,
    pub(super) forked_from_ordinal_exclusive: Option<u64>,
    pub(super) next_internal_sub_id: AtomicU64,
}

#[derive(Clone)]
pub(crate) struct SessionConfiguration {
    /// Runtime provider and its provider-specific execution policy.
    pub(super) provider: SharedModelProvider,

    /// Desired configured inputs inherited by future turns.
    pub(super) step_settings: Arc<StepSettings>,
    /// Ordered environments inherited by future turns.
    pub(super) environments: Vec<TurnEnvironmentSelection>,
    /// Explicit startup overrides used when resolving effective model metadata.
    pub(super) model_info_overrides: ModelInfoOverrides,

    /// Developer instructions that supplement the base instructions.
    pub(super) developer_instructions: Option<String>,

    /// Base instructions for the session.
    pub(super) base_instructions: String,

    /// Permission profile state for the session. Keep the constrained profile,
    /// active profile id, and profile-defined workspace roots in sync by using
    /// the methods below instead of mutating the fields independently.
    pub(super) permission_profile_state: PermissionProfileState,
    pub(super) allow_login_shell: bool,
    pub(super) shell_environment_policy: ShellEnvironmentPolicy,
    // TODO(anp): Reconcile these legacy thread defaults with TurnEnvironment::sandbox_context;
    // internal sandbox decisions should use the selected environment's configuration.
    pub(super) windows_sandbox_level: WindowsSandboxLevel,
    pub(super) windows_sandbox_type: SandboxType,
    pub(super) use_legacy_landlock: bool,

    /// Legacy thread cwd used when a turn does not select an environment.
    pub(super) legacy_fallback_cwd: AbsolutePathBuf,
    /// Top-level runtime workspace roots, independent of explicit environment selections.
    pub(super) runtime_workspace_roots: Vec<AbsolutePathBuf>,
    /// Directory containing all Codex state for this session.
    pub(super) codex_home: AbsolutePathBuf,
    /// Optional user-facing name for the thread, updated during the session.
    pub(super) thread_name: Option<String>,
    /// Thread-owned plugin selection inherited by future turns.
    pub(super) disabled_plugin_ids: Vec<String>,

    // TODO(pakrym): Remove config from here
    pub(super) original_config_do_not_use: Arc<Config>,
    /// Optional service name tag for session metrics.
    pub(super) metrics_service_name: Option<String>,
    pub(super) app_server_client_name: Option<String>,
    pub(super) app_server_client_version: Option<String>,
    /// Guardian reviewer identity is trusted only when established during an in-memory spawn.
    pub(super) trusted_guardian_reviewer: bool,
    /// Source of the session (cli, vscode, exec, mcp, ...)
    pub(super) session_source: SessionSource,
    /// Persisted thread history contract selected when this thread was created.
    pub(super) history_mode: ThreadHistoryMode,
    /// Immediate history source copied into this thread, when this thread was forked.
    pub(super) forked_from_thread_id: Option<ThreadId>,
    /// Immediate control/spawn parent for this thread, when it has one.
    pub(super) parent_thread_id: Option<ThreadId>,
    /// Optional analytics source classification for this thread.
    pub(super) thread_source: Option<ThreadSource>,
    /// Effective originator used for this thread's Responses requests and analytics events.
    pub(super) originator: String,
    pub(super) dynamic_tools: Vec<DynamicToolSpec>,
    pub(super) user_shell_override: Option<shell::Shell>,
}

impl SessionConfiguration {
    pub(super) fn cwd(&self) -> &AbsolutePathBuf {
        &self.legacy_fallback_cwd
    }

    pub(crate) fn codex_home(&self) -> &AbsolutePathBuf {
        &self.codex_home
    }

    pub(super) fn inferred_environment_config(&self) -> EnvironmentConfig {
        EnvironmentConfig {
            allow_login_shell: self.allow_login_shell,
            workspace_roots: Vec::new(),
            permission_profile: self.permission_profile_state.snapshot(),
            shell_environment_policy: self.shell_environment_policy.clone(),
            windows_sandbox_level: self.windows_sandbox_level,
            windows_sandbox_type: self.windows_sandbox_type,
            use_legacy_landlock: self.use_legacy_landlock,
            exec_policy: None,
            mcp_policy: None,
            network_policy: None,
            selected_capability_roots: Vec::new(),
        }
    }

    pub(super) fn permission_profile(&self) -> PermissionProfile {
        self.permission_profile_state.permission_profile().clone()
    }

    fn materialized_permission_profile(
        &self,
        environments: &[TurnEnvironmentSelection],
    ) -> PermissionProfile {
        let workspace_roots = ThreadEnvironments::primary_workspace_roots_for(environments);
        self.permission_profile()
            .materialize_project_roots_with_workspace_roots(&workspace_roots)
    }

    fn effective_permission_profile(
        &self,
        environments: &[TurnEnvironmentSelection],
    ) -> PermissionProfile {
        ThreadEnvironments::primary_config_for(environments)
            .map(|config| {
                config
                    .permission_profile
                    .permission_profile()
                    .clone()
                    .materialize_project_roots_with_path_uris(&config.workspace_roots)
            })
            .unwrap_or_else(|| self.materialized_permission_profile(environments))
    }

    pub(super) fn active_permission_profile(&self) -> Option<ActivePermissionProfile> {
        self.permission_profile_state.active_permission_profile()
    }

    pub(super) fn apply_permission_profile_to_permissions(
        &self,
        permissions: &mut crate::config::Permissions,
    ) {
        permissions.set_permission_profile_state(self.permission_profile_state.clone());
    }

    #[cfg(test)]
    pub(super) fn set_permission_profile_for_tests(
        &mut self,
        permission_profile: PermissionProfile,
    ) -> ConstraintResult<()> {
        self.permission_profile_state
            .set_legacy_permission_profile(permission_profile)
    }

    pub(super) fn sandbox_policy(
        &self,
        environments: &[TurnEnvironmentSelection],
    ) -> SandboxPolicy {
        let permission_profile = self.materialized_permission_profile(environments);
        codex_sandboxing::compatibility_sandbox_policy_for_permission_profile(
            &permission_profile,
            self.cwd(),
        )
    }

    pub(super) fn file_system_sandbox_policy(
        &self,
        environments: &[TurnEnvironmentSelection],
    ) -> FileSystemSandboxPolicy {
        self.materialized_permission_profile(environments)
            .file_system_sandbox_policy()
    }

    pub(super) fn network_sandbox_policy(&self) -> NetworkSandboxPolicy {
        self.permission_profile_state
            .permission_profile()
            .network_sandbox_policy()
    }

    pub(super) fn thread_config_snapshot(
        &self,
        environment_selections: Vec<TurnEnvironmentSelection>,
    ) -> ThreadConfigSnapshot {
        let workspace_roots =
            ThreadEnvironments::primary_workspace_roots_for(&environment_selections);
        let permission_profile = ThreadEnvironments::primary_config_for(&environment_selections)
            .map(|config| config.permission_profile.clone())
            .unwrap_or_else(|| self.permission_profile_state.snapshot());
        ThreadConfigSnapshot {
            model: self.step_settings.collaboration_mode.model().to_string(),
            model_provider_id: self.original_config_do_not_use.model_provider_id.clone(),
            service_tier: self.step_settings.service_tier.clone(),
            approval_policy: self.step_settings.approval_policy.value(),
            approvals_reviewer: self.step_settings.approvals_reviewer,
            permission_profile: self.effective_permission_profile(&environment_selections),
            full_access: codex_protocol::protocol::has_full_access(
                self.step_settings.approval_policy.value(),
                &self.permission_profile(),
                environment_selections
                    .iter()
                    .map(|environment| &environment.config),
            ),
            active_permission_profile: permission_profile.active_permission_profile(),
            environments: TurnEnvironmentSelections::new(
                self.legacy_fallback_cwd.clone(),
                environment_selections,
            ),
            workspace_roots,
            profile_workspace_roots: permission_profile.profile_workspace_roots().to_vec(),
            ephemeral: self.original_config_do_not_use.ephemeral,
            reasoning_effort: self.step_settings.collaboration_mode.reasoning_effort(),
            reasoning_summary: self.step_settings.reasoning_summary,
            personality: self.step_settings.personality,
            collaboration_mode: self.step_settings.collaboration_mode.clone(),
            session_source: self.session_source.clone(),
            history_mode: self.history_mode,
            forked_from_thread_id: self.forked_from_thread_id,
            parent_thread_id: self.parent_thread_id,
            thread_source: self.thread_source.clone(),
            originator: self.originator.clone(),
            disabled_plugin_ids: self.disabled_plugin_ids.clone(),
        }
    }

    /// Captures thread-owned settings for persistence and resume.
    pub(super) fn thread_settings_snapshot(
        &self,
        environment_selections: &[TurnEnvironmentSelection],
    ) -> ThreadSettingsSnapshot {
        ThreadSettingsSnapshot {
            model: self.step_settings.collaboration_mode.model().to_string(),
            model_provider_id: self.original_config_do_not_use.model_provider_id.clone(),
            service_tier: self.step_settings.service_tier.clone(),
            approval_policy: self.step_settings.approval_policy.value(),
            approvals_reviewer: self.step_settings.approvals_reviewer,
            permission_profile: self.materialized_permission_profile(environment_selections),
            active_permission_profile: self.active_permission_profile(),
            cwd: self.legacy_fallback_cwd.clone(),
            runtime_workspace_roots: Some(self.runtime_workspace_roots.clone()),
            reasoning_effort: self.step_settings.collaboration_mode.reasoning_effort(),
            reasoning_summary: self.step_settings.reasoning_summary,
            personality: self.step_settings.personality,
            collaboration_mode: self.step_settings.collaboration_mode.clone(),
            disabled_plugin_ids: self.disabled_plugin_ids.clone(),
        }
    }

    /// Captures thread-owned settings and their separately owned environments.
    pub(super) fn restorable_thread_settings(
        &self,
        environment_selections: Vec<TurnEnvironmentSelection>,
    ) -> CodexThreadSettingsOverrides {
        CodexThreadSettingsOverrides {
            environments: Some(TurnEnvironmentSelections::new(
                self.legacy_fallback_cwd.clone(),
                environment_selections,
            )),
            runtime_workspace_roots: Some(self.runtime_workspace_roots.clone()),
            profile_workspace_roots: Some(
                self.permission_profile_state
                    .profile_workspace_roots()
                    .to_vec(),
            ),
            approval_policy: Some(self.step_settings.approval_policy.value()),
            approvals_reviewer: Some(self.step_settings.approvals_reviewer),
            permission_profile: Some(self.permission_profile()),
            active_permission_profile: self.active_permission_profile(),
            windows_sandbox_level: Some(self.windows_sandbox_level),
            summary: self.step_settings.reasoning_summary,
            service_tier: Some(self.step_settings.service_tier.clone()),
            collaboration_mode: Some(self.step_settings.collaboration_mode.clone()),
            personality: self.step_settings.personality,
            disabled_plugin_ids: Some(self.disabled_plugin_ids.clone()),
            ..Default::default()
        }
    }

    pub(super) fn validate(
        &self,
        environments: &[TurnEnvironmentSelection],
    ) -> ConstraintResult<()> {
        self.step_settings
            .validate(&self.step_settings_constraints(environments))?;
        super::environment::validate_environment_selections(environments)
    }

    pub(super) fn step_settings_constraints(
        &self,
        environments: &[TurnEnvironmentSelection],
    ) -> StepSettingsConstraints<'_> {
        let permission_profile = self.effective_permission_profile(environments);
        StepSettingsConstraints {
            requirements: self
                .original_config_do_not_use
                .config_layer_stack
                .requirements(),
            guardian_approval_enabled: self
                .original_config_do_not_use
                .features
                .enabled(Feature::GuardianApproval),
            trusted_guardian_reviewer: self.trusted_guardian_reviewer,
            has_full_disk_write_access: permission_profile
                .file_system_sandbox_policy()
                .has_full_disk_write_access(),
        }
    }

    pub(super) fn apply(
        &self,
        updates: &SessionSettingsUpdate,
        current_environments: &[TurnEnvironmentSelection],
    ) -> ConstraintResult<Self> {
        let mut next_configuration = self.clone();
        if let Some(disabled_plugin_ids) = &updates.disabled_plugin_ids {
            next_configuration.disabled_plugin_ids = disabled_plugin_ids.clone();
        }
        let current_file_system_sandbox_policy =
            self.file_system_sandbox_policy(current_environments);
        let file_system_policy_has_rebindable_project_root_write =
            current_file_system_sandbox_policy
                .entries
                .iter()
                .any(|entry| {
                    entry.access.can_write()
                        && matches!(
                            &entry.path,
                            FileSystemPath::Special {
                                value: FileSystemSpecialPath::ProjectRoots { subpath: None },
                            }
                        )
                });
        if let Some(windows_sandbox_level) = updates.windows_sandbox_level {
            next_configuration.windows_sandbox_level = windows_sandbox_level;
        }

        let current_cwd = self.cwd().clone();
        if let Some(environments) = &updates.environments {
            next_configuration.legacy_fallback_cwd = environments.legacy_fallback_cwd.clone();
            next_configuration.environments = environments.environments.clone();
        }
        let cwd_changed = next_configuration.legacy_fallback_cwd != current_cwd;
        if let Some(runtime_workspace_roots) = &updates.runtime_workspace_roots {
            next_configuration.runtime_workspace_roots = runtime_workspace_roots.clone();
        } else if cwd_changed {
            next_configuration.runtime_workspace_roots = replace_path_and_deduplicate(
                next_configuration.runtime_workspace_roots,
                current_cwd.as_path(),
                next_configuration.legacy_fallback_cwd.clone(),
            );
        }

        if let Some(permission_profile) = updates.permission_profile.clone() {
            let active_permission_profile =
                updates.active_permission_profile.clone().or_else(|| {
                    if permission_profile == self.permission_profile() {
                        self.active_permission_profile()
                    } else {
                        None
                    }
                });
            next_configuration.set_permission_profile_projection(
                permission_profile,
                active_permission_profile,
                updates.profile_workspace_roots.clone().unwrap_or_default(),
                Some(&current_file_system_sandbox_policy),
            )?;
            if let Some(active_permission_profile) = next_configuration.active_permission_profile()
            {
                let mut config = (*next_configuration.original_config_do_not_use).clone();
                let permission_profile = next_configuration.permission_profile();
                config.permissions.network = config
                    .network_proxy_spec_for_active_permission_profile(
                        &active_permission_profile,
                        &permission_profile,
                    )
                    .map_err(|err| ConstraintError::InvalidValue {
                        field_name: "default_permissions",
                        candidate: active_permission_profile.id.clone(),
                        allowed: format!(
                            "configured permission profile with valid network policy ({err})"
                        ),
                        requirement_source: codex_config::RequirementSource::Unknown,
                    })?;
                config
                    .permissions
                    .set_permission_profile_from_session_snapshot(
                        PermissionProfileSnapshot::active(
                            permission_profile,
                            active_permission_profile,
                        ),
                    )?;
                next_configuration.original_config_do_not_use = Arc::new(config);
            }
        } else if let Some(sandbox_policy) = updates.sandbox_policy.clone() {
            let file_system_sandbox_policy =
                FileSystemSandboxPolicy::from_legacy_sandbox_policy_preserving_deny_entries(
                    &sandbox_policy,
                    next_configuration.cwd(),
                    &current_file_system_sandbox_policy,
                );
            let network_sandbox_policy = NetworkSandboxPolicy::from(&sandbox_policy);
            next_configuration
                .permission_profile_state
                .set_legacy_permission_profile(
                    PermissionProfile::from_runtime_permissions_with_enforcement(
                        SandboxEnforcement::from_legacy_sandbox_policy(&sandbox_policy),
                        &file_system_sandbox_policy,
                        network_sandbox_policy,
                    ),
                )?;
        } else if cwd_changed && file_system_policy_has_rebindable_project_root_write {
            // Compatibility projection can resolve filesystem paths. Only compute it
            // when a cwd-bound legacy policy might need rebinding.
            let current_sandbox_policy = self.sandbox_policy(current_environments);
            if current_file_system_sandbox_policy.is_semantically_equivalent_to(
                &FileSystemSandboxPolicy::from_legacy_sandbox_policy_preserving_deny_entries(
                    &current_sandbox_policy,
                    self.cwd(),
                    &current_file_system_sandbox_policy,
                ),
                self.cwd(),
            ) {
                // Preserve richer split policies across cwd-only updates; only
                // rederive when the session is already using a structurally
                // cwd-bound legacy bridge.
                let file_system_sandbox_policy =
                    FileSystemSandboxPolicy::from_legacy_sandbox_policy_preserving_deny_entries(
                        &current_sandbox_policy,
                        next_configuration.cwd(),
                        &current_file_system_sandbox_policy,
                    );
                next_configuration
                    .permission_profile_state
                    .set_legacy_permission_profile(
                        PermissionProfile::from_runtime_permissions_with_enforcement(
                            SandboxEnforcement::from_legacy_sandbox_policy(&current_sandbox_policy),
                            &file_system_sandbox_policy,
                            self.network_sandbox_policy(),
                        ),
                    )?;
            }
        }
        if let Some(app_server_client_name) = updates.app_server_client_name.clone() {
            next_configuration.app_server_client_name = Some(app_server_client_name);
        }
        if let Some(app_server_client_version) = updates.app_server_client_version.clone() {
            next_configuration.app_server_client_version = Some(app_server_client_version);
        }
        let next_environments = updates
            .environments
            .as_ref()
            .map_or(current_environments, |environments| {
                environments.environments.as_slice()
            });
        super::environment::validate_environment_selections(next_environments)?;
        // Apply step settings last: the proposed permissions and environment
        // selections must be complete before deriving their validation constraints.
        next_configuration.step_settings = Arc::new(self.step_settings.apply(
            &updates.step_settings,
            &next_configuration.step_settings_constraints(next_environments),
        )?);
        Ok(next_configuration)
    }

    fn set_permission_profile_projection(
        &mut self,
        permission_profile: PermissionProfile,
        active_permission_profile: Option<ActivePermissionProfile>,
        profile_workspace_roots: Vec<ProfileWorkspaceRoot>,
        preserve_deny_reads_from: Option<&FileSystemSandboxPolicy>,
    ) -> ConstraintResult<()> {
        let enforcement = permission_profile.enforcement();
        let (mut file_system_sandbox_policy, network_sandbox_policy) =
            permission_profile.to_runtime_permissions();
        if let Some(existing_file_system_policy) = preserve_deny_reads_from {
            file_system_sandbox_policy
                .preserve_deny_read_restrictions_from(existing_file_system_policy);
        }
        let effective_permission_profile =
            PermissionProfile::from_runtime_permissions_with_enforcement(
                enforcement,
                &file_system_sandbox_policy,
                network_sandbox_policy,
            );

        let permission_snapshot = match active_permission_profile {
            Some(active_permission_profile) => {
                PermissionProfileSnapshot::active_with_profile_workspace_roots(
                    effective_permission_profile,
                    active_permission_profile,
                    profile_workspace_roots,
                )
            }
            None => PermissionProfileSnapshot::legacy(effective_permission_profile),
        };

        self.permission_profile_state
            .set_permission_profile_snapshot(permission_snapshot)
    }
}

/// The configuration and public snapshot published by one settings commit.
pub(crate) struct SessionSettingsCommit {
    pub(crate) configuration: SessionConfiguration,
    pub(crate) snapshot: ThreadSettingsSnapshot,
}

#[derive(Default, Clone)]
pub(crate) struct SessionSettingsUpdate {
    pub(crate) step_settings: StepSettingsUpdate,
    pub(crate) environments: Option<TurnEnvironmentSelections>,
    pub(crate) runtime_workspace_roots: Option<Vec<AbsolutePathBuf>>,
    pub(crate) profile_workspace_roots: Option<Vec<ProfileWorkspaceRoot>>,
    pub(crate) sandbox_policy: Option<SandboxPolicy>,
    pub(crate) permission_profile: Option<PermissionProfile>,
    pub(crate) active_permission_profile: Option<ActivePermissionProfile>,
    pub(crate) windows_sandbox_level: Option<WindowsSandboxLevel>,
    pub(crate) service_tier_for_turn: Option<String>,
    pub(crate) app_server_client_name: Option<String>,
    pub(crate) app_server_client_version: Option<String>,
    pub(crate) disabled_plugin_ids: Option<Vec<String>>,
}

pub(crate) struct AppServerClientMetadata {
    pub(crate) client_name: Option<String>,
    pub(crate) client_version: Option<String>,
}

async fn warm_plugins_and_skills_for_session_init(
    config: Arc<Config>,
    plugins_manager: Arc<PluginsManager>,
    skills_service: Arc<HostSkillsService>,
    turn_environments: &TurnEnvironmentSnapshot,
    extensions: &codex_extension_api::ExtensionRegistry<Config>,
) -> Vec<SkillError> {
    let plugins_input = config.plugins_config_input();
    let plugin_outcome = plugins_manager.plugins_for_config(&plugins_input).await;
    if config.features.enabled(Feature::SkipHostSkillDiscovery)
        && !extensions.requires_host_skill_discovery()
    {
        return Vec::new();
    }

    let fs = turn_environments.primary_filesystem();
    let effective_skill_roots = plugin_outcome.effective_plugin_skill_roots();
    let plugin_skill_snapshots = plugins_manager.plugin_skill_snapshots_for_config(&plugins_input);
    let skills_input = skills_load_input_from_config(config.as_ref(), effective_skill_roots)
        .with_plugin_skill_snapshots(plugin_skill_snapshots);
    skills_service
        .snapshot_for_config(&skills_input, fs)
        .await
        .outcome()
        .errors
        .clone()
}

fn canonicalize_completion_evidence(
    value: &mut serde_json::Value,
    completed: &std::collections::BTreeMap<
        String,
        codex_history::task_context::AcceptedTaskReceipt,
    >,
) -> Result<(), String> {
    match value {
        serde_json::Value::Object(object) => {
            if object.contains_key("task_id") && object.contains_key("task_revision") {
                let task_id = object
                    .get("task_id")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| "completion evidence task_id must be a string".to_string())?;
                let revision = object
                    .get("task_revision")
                    .and_then(serde_json::Value::as_u64)
                    .ok_or_else(|| {
                        "completion evidence task_revision must be a positive integer".to_string()
                    })?;
                let receipt = completed.get(task_id)
                    .filter(|receipt| receipt.revision == revision)
                    .ok_or_else(|| format!("no accepted host receipt matches completion evidence {task_id}@{revision}"))?;
                object.insert(
                    "artifact_ref".into(),
                    serde_json::Value::String(receipt.artifact_ref.clone()),
                );
                object.insert(
                    "verification_ref".into(),
                    serde_json::Value::String(receipt.verification_ref.clone()),
                );
            }
            for child in object.values_mut() {
                canonicalize_completion_evidence(child, completed)?;
            }
        }
        serde_json::Value::Array(values) => {
            for child in values {
                canonicalize_completion_evidence(child, completed)?;
            }
        }
        _ => {}
    }
    Ok(())
}

pub(crate) fn canonicalize_completion_state_evidence(
    value: &mut serde_json::Value,
    completed: &std::collections::BTreeMap<
        String,
        codex_history::task_context::AcceptedTaskReceipt,
    >,
) -> Result<(), String> {
    let object = value
        .as_object_mut()
        .ok_or_else(|| "completion state must be an object".to_string())?;
    if let Some(conditions) = object
        .get_mut("conditions")
        .and_then(serde_json::Value::as_array_mut)
    {
        for condition in conditions {
            if let Some(evidence) = condition.get_mut("evidence") {
                canonicalize_completion_evidence(evidence, completed)?;
            }
        }
    }
    if let Some(units) = object
        .get_mut("units")
        .and_then(serde_json::Value::as_array_mut)
    {
        for unit in units {
            for field in ["evidence", "task_refs"] {
                if let Some(evidence) = unit.get_mut(field) {
                    canonicalize_completion_evidence(evidence, completed)?;
                }
            }
        }
    }
    if let Some(blocker) = object.get_mut("blocker")
        && let Some(evidence) = blocker.get_mut("evidence")
    {
        canonicalize_completion_evidence(evidence, completed)?;
    }
    Ok(())
}

fn collect_completion_evidence(
    value: &serde_json::Value,
    output: &mut Vec<(String, u64, String, String)>,
) {
    match value {
        serde_json::Value::Object(object) => {
            if let (
                Some(task_id),
                Some(task_revision),
                Some(artifact_ref),
                Some(verification_ref),
            ) = (
                object.get("task_id").and_then(serde_json::Value::as_str),
                object
                    .get("task_revision")
                    .and_then(serde_json::Value::as_u64),
                object
                    .get("artifact_ref")
                    .and_then(serde_json::Value::as_str),
                object
                    .get("verification_ref")
                    .and_then(serde_json::Value::as_str),
            ) {
                output.push((
                    task_id.to_owned(),
                    task_revision,
                    artifact_ref.to_owned(),
                    verification_ref.to_owned(),
                ));
            }
            for child in object.values() {
                collect_completion_evidence(child, output);
            }
        }
        serde_json::Value::Array(values) => {
            for child in values {
                collect_completion_evidence(child, output);
            }
        }
        _ => {}
    }
}

fn collect_fresh_completion_evidence(
    proposal: &serde_json::Value,
    previous: Option<&codex_history::task_context::CompletionState>,
    output: &mut Vec<(String, u64, String, String)>,
) {
    let previous_value = previous.and_then(|state| serde_json::to_value(state).ok());
    let previous_units = previous_value
        .as_ref()
        .and_then(|value| value.get("units"))
        .and_then(serde_json::Value::as_array);

    if let Some(conditions) = proposal
        .get("conditions")
        .and_then(serde_json::Value::as_array)
    {
        for condition in conditions {
            if condition.get("status").and_then(serde_json::Value::as_str) == Some("satisfied")
                && let Some(evidence) = condition.get("evidence")
            {
                collect_completion_evidence(evidence, output);
            }
        }
    }

    if let Some(units) = proposal.get("units").and_then(serde_json::Value::as_array) {
        for unit in units {
            if unit.get("status").and_then(serde_json::Value::as_str) != Some("done") {
                continue;
            }
            let unit_id = unit.get("id").and_then(serde_json::Value::as_str);
            let previous_unit = previous_units.and_then(|previous_units| {
                previous_units.iter().find(|previous_unit| {
                    previous_unit.get("id").and_then(serde_json::Value::as_str) == unit_id
                })
            });
            let was_done = previous_unit
                .and_then(|previous_unit| previous_unit.get("status"))
                .and_then(serde_json::Value::as_str)
                == Some("done");
            let mut current_refs = Vec::new();
            for field in ["evidence", "task_refs"] {
                if let Some(refs) = unit.get(field) {
                    collect_completion_evidence(refs, &mut current_refs);
                }
            }
            if !was_done {
                output.extend(current_refs);
                continue;
            }
            let mut previous_refs = Vec::new();
            for field in ["evidence", "task_refs"] {
                if let Some(refs) = previous_unit.and_then(|previous_unit| previous_unit.get(field))
                {
                    collect_completion_evidence(refs, &mut previous_refs);
                }
            }
            let previous_refs = previous_refs
                .into_iter()
                .collect::<std::collections::HashSet<_>>();
            output.extend(
                current_refs
                    .into_iter()
                    .filter(|reference| !previous_refs.contains(reference)),
            );
        }
    }
}

pub(super) fn condition_progress_is_product_progress(
    had_previous_evidence: bool,
    previous_was_satisfied: bool,
    previous_proof_invalidated: bool,
    previous_artifacts: Option<&std::collections::HashSet<(String, String)>>,
    current_artifacts: Option<&std::collections::HashSet<(String, String)>>,
) -> bool {
    if previous_was_satisfied && !previous_proof_invalidated {
        return false;
    }
    if !had_previous_evidence {
        return true;
    }
    matches!((previous_artifacts, current_artifacts), (Some(previous), Some(current)) if previous != current)
}

pub(super) fn history_ends_with_completed_turn(items: &[RolloutItem]) -> bool {
    let mut current_turn_id: Option<&str> = None;
    let mut completed = false;
    for item in items {
        match item {
            RolloutItem::EventMsg(EventMsg::TurnStarted(event)) => {
                current_turn_id = Some(event.turn_id.as_str());
                completed = false;
            }
            RolloutItem::EventMsg(EventMsg::TurnComplete(event)) => {
                completed = current_turn_id == Some(event.turn_id.as_str());
            }
            RolloutItem::EventMsg(EventMsg::TurnAborted(_)) => completed = false,
            _ => {}
        }
    }
    completed
}

impl Session {
    pub(crate) async fn set_rencrow_completion_state(
        &self,
        root_task_id: String,
        mut completion_state: serde_json::Value,
    ) -> Result<(), String> {
        if root_task_id.trim().is_empty() {
            return Err("completion state requires a root ID".into());
        }
        // Invalidation is a host fact, not a model-controlled progress reset.
        // A stale condition may be reopened after its evidence is removed from the proposal.
        let previous_contract = self.rencrow_completion_state(&root_task_id).await;
        // Review evidence is host-collected from the reviewer child lifecycle. Never
        // accept a model-supplied replacement or deletion of that binding.
        if let Some(previous) = previous_contract.as_ref() {
            let proposed_units = completion_state
                .get("units")
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| {
                    "completion update must retain host-bound review units".to_string()
                })?;
            for old_unit in previous.units.iter().filter(|unit| {
                unit.review_launch.is_some()
                    || (unit.kind == codex_history::task_context::CompletionUnitKind::Review
                        && unit.review_evidence.is_some()
                        && unit.status != codex_history::task_context::UnitStatus::Done)
            }) {
                let proposed = proposed_units.iter().find(|unit| {
                    unit.get("id").and_then(serde_json::Value::as_str) == Some(old_unit.id.as_str())
                });
                let Some(proposed) = proposed else {
                    return Err(format!(
                        "completion update cannot remove unresolved review unit {}",
                        old_unit.id
                    ));
                };
                let target = proposed.get("review_target");
                if proposed.get("kind").and_then(serde_json::Value::as_str) != Some("review")
                    || target
                        .and_then(|target| target.get("task_id"))
                        .and_then(serde_json::Value::as_str)
                        != old_unit
                            .review_target
                            .as_ref()
                            .map(|target| target.task_id.as_str())
                    || target
                        .and_then(|target| target.get("task_revision"))
                        .and_then(serde_json::Value::as_u64)
                        != old_unit
                            .review_target
                            .as_ref()
                            .map(|target| target.task_revision)
                {
                    return Err(format!(
                        "completion update cannot change the identity or target of unresolved review unit {}",
                        old_unit.id
                    ));
                }
            }
        }
        if let Some(proposal_units) = completion_state
            .get_mut("units")
            .and_then(serde_json::Value::as_array_mut)
        {
            for proposed in proposal_units {
                let Some(object) = proposed.as_object_mut() else {
                    continue;
                };
                object.remove("review_evidence");
                object.remove("review_launch");
                let Some(old_unit) = previous_contract.as_ref().and_then(|old| {
                    old.units.iter().find(|unit| {
                        object.get("id").and_then(serde_json::Value::as_str)
                            == Some(unit.id.as_str())
                    })
                }) else {
                    continue;
                };
                if let Some(evidence) = old_unit.review_evidence.as_ref() {
                    object.insert(
                        "review_evidence".into(),
                        serde_json::to_value(evidence).map_err(|error| error.to_string())?,
                    );
                }
                if let Some(launch) = old_unit.review_launch.as_ref() {
                    object.insert(
                        "review_launch".into(),
                        serde_json::to_value(launch).map_err(|error| error.to_string())?,
                    );
                }
            }
        }
        let mut invalidated_conditions = std::collections::HashSet::new();
        if let Some(previous) = previous_contract.as_ref() {
            for condition in previous.conditions.iter().filter(|condition| {
                condition.status == codex_history::task_context::AcceptanceStatus::Satisfied
            }) {
                for evidence in &condition.evidence {
                    if !self
                        .completion_evidence_matches_current_artifact(evidence)
                        .await
                    {
                        invalidated_conditions.insert(condition.id.clone());
                        break;
                    }
                }
            }
        }
        if !invalidated_conditions.is_empty()
            && let Some(conditions) = completion_state
                .get_mut("conditions")
                .and_then(serde_json::Value::as_array_mut)
        {
            for condition in conditions {
                let Some(id) = condition
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
                else {
                    continue;
                };
                let has_new_host_evidence = previous_contract
                    .as_ref()
                    .and_then(|previous| previous.conditions.iter().find(|old| old.id == id))
                    .is_some_and(|old| {
                        let prior = old
                            .evidence
                            .iter()
                            .map(|evidence| {
                                (evidence.task_id.as_str(), evidence.task_revision)
                            })
                            .collect::<std::collections::HashSet<_>>();
                        condition
                            .get("evidence")
                            .and_then(serde_json::Value::as_array)
                            .is_some_and(|evidence| {
                                evidence.iter().any(|item| {
                                    let task_id = item
                                        .get("task_id")
                                        .and_then(serde_json::Value::as_str);
                                    let revision = item
                                        .get("task_revision")
                                        .and_then(serde_json::Value::as_u64);
                                    matches!((task_id, revision), (Some(task_id), Some(revision)) if !prior.contains(&(task_id, revision)))
                                })
                            })
                    });
                if invalidated_conditions.contains(&id)
                    && !has_new_host_evidence
                    && let Some(condition) = condition.as_object_mut()
                {
                    condition.insert("status".into(), serde_json::Value::String("open".into()));
                    if let Some(previous_evidence) = previous_contract
                        .as_ref()
                        .and_then(|previous| previous.conditions.iter().find(|old| old.id == id))
                        .map(|previous| serde_json::to_value(&previous.evidence))
                        .transpose()
                        .map_err(|error| error.to_string())?
                    {
                        condition.insert("evidence".into(), previous_evidence);
                    }
                    condition.insert("unmet_reason".into(), serde_json::Value::String(
                        "Previously accepted evidence no longer matches the current artifact revision.".into(),
                    ));
                }
            }
        }
        let latest = self
            .latest_rencrow_task_state()
            .await
            .map_err(|error| error.to_string())?;
        if latest
            .as_ref()
            .is_some_and(|state| state.boundary.root_task_id != root_task_id)
        {
            return Err("completion root does not match the latest accepted task state".into());
        }
        let completed = latest
            .as_ref()
            .map(|state| state.completed.clone())
            .unwrap_or_default();
        if let Some(accepted) = latest.as_ref()
            && accepted.boundary.root_task_id != root_task_id
        {
            return Err("completion root does not match the latest accepted task state".into());
        }
        canonicalize_completion_state_evidence(&mut completion_state, &completed)?;
        let mut evidence_refs = Vec::new();
        collect_fresh_completion_evidence(
            &completion_state,
            previous_contract.as_ref(),
            &mut evidence_refs,
        );
        for (task_id, task_revision, artifact_ref, verification_ref) in evidence_refs {
            let attempt = self.latest_task_check_for_scope(&task_id, task_revision).await
                .ok_or_else(|| format!("no current host verification exists for completion evidence {task_id}@{task_revision}"))?;
            let receipt = match attempt.status {
                crate::task_check::TaskCheckAttemptStatus::Finished(receipt) => receipt,
                crate::task_check::TaskCheckAttemptStatus::Pending => {
                    return Err("completion evidence verification is still running".into());
                }
                crate::task_check::TaskCheckAttemptStatus::ChangedArtifacts => {
                    return Err("completion evidence verification is stale".into());
                }
            };
            if !receipt.succeeded
                || receipt.exit_code != 0
                || receipt.artifact_before != receipt.artifact_after
            {
                return Err(
                    "completion evidence does not have a successful stable verification receipt"
                        .into(),
                );
            }
            let paths = receipt
                .artifact_after
                .iter()
                .map(|stamp| stamp.path.clone())
                .collect::<Vec<_>>();
            let current =
                crate::task_check::hash_workspace_artifacts(&receipt.workspace_root, &paths)
                    .ok_or_else(|| {
                        "completion evidence artifacts are no longer available".to_string()
                    })?;
            if current != receipt.artifact_after {
                return Err("completion evidence artifacts changed after verification".into());
            }
            let artifact_stamp_set = receipt
                .artifact_after
                .iter()
                .map(|stamp| {
                    serde_json::json!({
                        "path": stamp.path.to_string_lossy(), "sha256": stamp.sha256,
                    })
                })
                .collect::<Vec<_>>();
            let fingerprint = codex_history::compaction_candidate::digest(&serde_json::json!({
                "task_id": receipt.task_id,
                "task_revision": receipt.task_revision,
                "workspace_root": receipt.workspace_root.to_string_lossy(),
                "artifacts": artifact_stamp_set,
            }))?;
            let host_ref = format!("task-check:{}:{fingerprint}", receipt.call_id);
            if artifact_ref != host_ref || verification_ref != host_ref {
                return Err("completion evidence does not match its current host receipt".into());
            }
        }
        let mut completion_state: codex_history::task_context::CompletionState =
            serde_json::from_value(completion_state).map_err(|error| error.to_string())?;
        if completion_state.contract_revision == 0 {
            return Err("completion state requires a positive contract revision".into());
        }
        codex_history::task_context::validate_completion_state(
            Some(&completion_state),
            &completed,
        )?;
        if completion_state.conditions.iter().any(|condition| {
            condition.status == codex_history::task_context::AcceptanceStatus::Waived
        }) {
            return Err(
                "waiving an acceptance condition is not authorized by this host action".into(),
            );
        }
        let mut product_progress_conditions = std::collections::HashSet::new();
        if let Some(previous) = previous_contract.as_ref() {
            for condition in completion_state.conditions.iter().filter(|condition| {
                condition.status == codex_history::task_context::AcceptanceStatus::Satisfied
            }) {
                let Some(old) = previous
                    .conditions
                    .iter()
                    .find(|old| old.id == condition.id)
                else {
                    product_progress_conditions.insert(condition.id.clone());
                    continue;
                };
                let prior_proof_invalidated = invalidated_conditions.contains(&condition.id);
                let mut old_bytes = std::collections::HashSet::new();
                let mut new_bytes = std::collections::HashSet::new();
                let mut comparable = true;
                for evidence in &old.evidence {
                    if let Some(bytes) = self.task_evidence_artifact_bytes(evidence).await {
                        old_bytes.extend(bytes);
                    } else {
                        comparable = false;
                    }
                }
                for evidence in &condition.evidence {
                    if let Some(bytes) = self.task_evidence_artifact_bytes(evidence).await {
                        new_bytes.extend(bytes);
                    } else {
                        comparable = false;
                    }
                }
                let old_bytes = (comparable && !old.evidence.is_empty()).then_some(old_bytes);
                let new_bytes = comparable.then_some(new_bytes);
                if condition_progress_is_product_progress(
                    !old.evidence.is_empty(),
                    old.status == codex_history::task_context::AcceptanceStatus::Satisfied,
                    prior_proof_invalidated,
                    old_bytes.as_ref(),
                    new_bytes.as_ref(),
                ) {
                    product_progress_conditions.insert(condition.id.clone());
                }
            }
        } else {
            product_progress_conditions.extend(
                completion_state
                    .conditions
                    .iter()
                    .filter(|condition| {
                        condition.status == codex_history::task_context::AcceptanceStatus::Satisfied
                    })
                    .map(|condition| condition.id.clone()),
            );
        }
        {
            let mut state = self.state.lock().await;
            if let Some((active_root, previous)) = &state.rencrow_completion_state {
                if active_root != &root_task_id {
                    return Err("completion state root does not match the active contract".into());
                }
                if completion_state.contract_revision < previous.contract_revision {
                    return Err("completion contract revision cannot move backwards".into());
                }
                completion_state.paused = previous.paused;
                completion_state.consumed_host_events = previous.consumed_host_events.clone();
                let prior = previous
                    .conditions
                    .iter()
                    .map(|item| (item.id.as_str(), item))
                    .collect::<std::collections::HashMap<_, _>>();
                if completion_state.conditions.iter().any(|item| {
                    prior.get(item.id.as_str()).is_some_and(|old| {
                        item.revision < old.revision
                            || (item.result != old.result
                                || item.scope != old.scope
                                || item.verification != old.verification
                                || item.acceptance_criteria != old.acceptance_criteria)
                            || (old.status
                                == codex_history::task_context::AcceptanceStatus::Satisfied
                                && item.status
                                    != codex_history::task_context::AcceptanceStatus::Satisfied
                                && (!invalidated_conditions.contains(&item.id)
                                    || item
                                        .unmet_reason
                                        .as_ref()
                                        .is_none_or(|reason| reason.trim().is_empty())))
                            || (item.status
                                == codex_history::task_context::AcceptanceStatus::Satisfied
                                && old.status
                                    != codex_history::task_context::AcceptanceStatus::Satisfied
                                && item.evidence == old.evidence)
                    })
                }) || previous.conditions.iter().any(|old| {
                    !completion_state
                        .conditions
                        .iter()
                        .any(|item| item.id == old.id)
                }) {
                    return Err(
                    "acceptance conditions cannot be removed, weakened, or redefined by a model update; a revision increment is not owner authorization".into(),
                );
                }
            }
            if let Some((_, previous)) = &state.rencrow_completion_state {
                completion_state.paused = previous.paused;
                completion_state.consumed_host_events = previous.consumed_host_events.clone();
            } else {
                completion_state.paused = state.rencrow_completion_paused;
                completion_state.consumed_host_events.clear();
            }
            let now = chrono::Utc::now().timestamp();
            if let Some((_, previous)) = &state.rencrow_completion_state {
                for approach in &mut completion_state.failed_approaches {
                    if !previous.failed_approaches.iter().any(|old| old == approach) {
                        approach.recorded_at = now;
                    }
                }
            } else {
                for approach in &mut completion_state.failed_approaches {
                    approach.recorded_at = now;
                }
            }
            let prior_progress = state
                .rencrow_completion_state
                .as_ref()
                .and_then(|(_, old)| old.product_progress_at);
            completion_state.units_completed_since_review = state
                .rencrow_completion_state
                .as_ref()
                .map(|(_, old)| old.units_completed_since_review)
                .unwrap_or(0);
            completion_state.plan_review_at = state
                .rencrow_completion_state
                .as_ref()
                .and_then(|(_, old)| old.plan_review_at)
                .or(Some(now));
            completion_state.plan_reviews = state
                .rencrow_completion_state
                .as_ref()
                .map(|(_, old)| old.plan_reviews.clone())
                .unwrap_or_default();
            completion_state.product_progress_at = prior_progress;
            completion_state.runtime_recovery_at = state
                .rencrow_completion_state
                .as_ref()
                .and_then(|(_, old)| old.runtime_recovery_at);
            completion_state.activity_at = state
                .rencrow_completion_state
                .as_ref()
                .and_then(|(_, old)| old.activity_at);
            if !product_progress_conditions.is_empty() {
                completion_state.product_progress_at = Some(now);
                completion_state.activity_at = Some(now);
                completion_state.units_completed_since_review = 0;
                completion_state.plan_review_at = Some(now);
            }
            state.rencrow_completion_state = Some((root_task_id, completion_state));
        }
        self.persist_autonomous_completion_snapshot().await?;
        Ok(())
    }

    pub(crate) async fn rencrow_completion_state(
        &self,
        root_task_id: &str,
    ) -> Option<codex_history::task_context::CompletionState> {
        self.state
            .lock()
            .await
            .rencrow_completion_state
            .as_ref()
            .filter(|(root, _)| root == root_task_id)
            .map(|(_, completion_state)| completion_state.clone())
    }

    pub(crate) async fn autonomous_completion_pending(&self) -> bool {
        let state = self.state.lock().await;
        if state.rencrow_completion_paused
            || state
                .rencrow_completion_state
                .as_ref()
                .is_none_or(|(_, contract)| contract.paused)
        {
            return false;
        }
        let Some((_, contract)) = state.rencrow_completion_state.as_ref() else {
            return false;
        };
        let open_conditions = contract.conditions.iter().any(|condition| {
            condition.status != codex_history::task_context::AcceptanceStatus::Satisfied
                && condition.status != codex_history::task_context::AcceptanceStatus::Waived
        });
        let unfinished_units = contract.units.iter().any(|unit| {
            !matches!(
                unit.status,
                codex_history::task_context::UnitStatus::Done
                    | codex_history::task_context::UnitStatus::Cancelled
            )
        });
        (open_conditions
            || unfinished_units
            || !state.rencrow_completion_uncollected_children.is_empty())
            && contract.blocker.is_none()
    }

    pub(crate) async fn autonomous_completion_plan_review_due(&self) -> bool {
        let state = self.state.lock().await;
        let Some((_, contract)) = state.rencrow_completion_state.as_ref() else {
            return false;
        };
        if contract.paused || contract.blocker.is_some() {
            return false;
        }
        let two_units = contract.units_completed_since_review >= 2;
        let thirty_minutes = contract
            .plan_review_at
            .is_some_and(|at| chrono::Utc::now().timestamp().saturating_sub(at) >= 30 * 60);
        two_units || thirty_minutes
    }

    pub(crate) async fn note_autonomous_completion_unit_accepted(&self) {
        {
            let mut state = self.state.lock().await;
            let Some((_, contract)) = state.rencrow_completion_state.as_mut() else {
                return;
            };
            contract.units_completed_since_review =
                contract.units_completed_since_review.saturating_add(1);
            contract.activity_at = Some(chrono::Utc::now().timestamp());
        }
        if let Err(error) = self.persist_autonomous_completion_snapshot().await {
            tracing::error!(%error, "failed to persist accepted completion unit count");
            self.pause_autonomous_completion().await;
        }
    }

    pub(crate) async fn record_autonomous_plan_review(
        &self,
        root_task_id: &str,
        previous_plan: String,
        revised_plan: String,
        reason: String,
    ) -> Result<(), String> {
        if previous_plan.trim().is_empty()
            || revised_plan.trim().is_empty()
            || reason.trim().is_empty()
            || previous_plan == revised_plan
        {
            return Err(
                "plan review requires distinct non-empty old and new plans with a reason".into(),
            );
        }
        let now = chrono::Utc::now().timestamp();
        {
            let mut state = self.state.lock().await;
            let (active_root, contract) = state
                .rencrow_completion_state
                .as_mut()
                .ok_or_else(|| "no host completion contract is registered".to_string())?;
            if active_root != root_task_id {
                return Err("plan review root does not match the registered contract".into());
            }
            if contract.paused || contract.blocker.is_some() {
                return Err(
                    "cannot review a paused or externally blocked completion contract".into(),
                );
            }
            if contract.units_completed_since_review < 2
                && contract
                    .plan_review_at
                    .is_none_or(|at| now.saturating_sub(at) < 30 * 60)
            {
                return Err("plan review threshold has not been reached".into());
            }
            let host_previous_plan = contract
                .next_action
                .as_deref()
                .unwrap_or("No plan recorded");
            if host_previous_plan != previous_plan {
                return Err("previous plan does not match the host-registered next action".into());
            }
            contract
                .plan_reviews
                .push(codex_history::task_context::PlanReviewRecord {
                    at: now,
                    previous_plan,
                    revised_plan: revised_plan.clone(),
                    reason,
                });
            contract.next_action = Some(revised_plan);
            contract.units_completed_since_review = 0;
            contract.plan_review_at = Some(now);
            contract.activity_at = Some(now);
        }
        self.persist_autonomous_completion_snapshot().await
    }

    pub(crate) async fn autonomous_completion_has_unresolved_work(
        &self,
        source: &codex_protocol::protocol::SessionSource,
    ) -> bool {
        if !self
            .services
            .unified_exec_manager
            .list_processes()
            .await
            .is_empty()
        {
            return true;
        }
        if !self.services.agent_control.has_registered_agents() {
            return false;
        }
        self.services
            .agent_control
            .list_agents(source, None)
            .await
            .map(|agents| {
                agents.iter().any(|agent| {
                    matches!(
                        agent.status,
                        codex_protocol::protocol::AgentStatus::PendingInit
                            | codex_protocol::protocol::AgentStatus::Running
                            | codex_protocol::protocol::AgentStatus::Interrupted
                    )
                })
            })
            .unwrap_or(true)
    }

    pub(crate) async fn autonomous_completion_paused(&self) -> bool {
        let state = self.state.lock().await;
        state.rencrow_completion_paused
            || state
                .rencrow_completion_state
                .as_ref()
                .is_some_and(|(_, contract)| contract.paused)
    }

    pub(crate) async fn autonomous_completion_previous_turn_id(&self) -> Option<String> {
        self.state.lock().await.last_started_turn_id.clone()
    }

    pub(crate) async fn autonomous_completion_cold_resume_turn_id(&self) -> Option<String> {
        let state = self.state.lock().await;
        state
            .rencrow_last_history_turn_completed
            .then(|| state.last_started_turn_id.clone())
            .flatten()
    }

    pub(crate) async fn autonomous_completion_current_state(
        &self,
    ) -> Option<(String, codex_history::task_context::CompletionState)> {
        self.state.lock().await.rencrow_completion_state.clone()
    }

    pub(crate) fn autonomous_completion_action_digest(
        contract: &codex_history::task_context::CompletionState,
    ) -> String {
        let failed_approaches = contract
            .failed_approaches
            .iter()
            .map(|approach| {
                serde_json::json!({
                    "cause": &approach.cause,
                    "next_action": &approach.next_action,
                })
            })
            .collect::<Vec<_>>();
        codex_history::compaction_candidate::digest(&serde_json::json!({
            "contract_revision": contract.contract_revision,
            "conditions": &contract.conditions,
            "units": &contract.units,
            "current_unit": &contract.current_unit,
            "failed_approaches": failed_approaches,
            "blocker": &contract.blocker,
            "next_action": &contract.next_action,
        }))
        .unwrap_or_else(|_| "invalid-action-state".to_string())
    }

    pub(crate) async fn autonomous_completion_has_unresolved_runtime_work(&self) -> bool {
        let source = self
            .state
            .lock()
            .await
            .session_configuration
            .session_source
            .clone();
        self.autonomous_completion_has_unresolved_work(&source)
            .await
    }

    pub(crate) async fn stop_completion_recovery_task_for_teardown(&self) {
        let observer = self
            .state
            .lock()
            .await
            .rencrow_completion_recovery_task
            .take();
        if let Some(observer) = observer {
            observer.abort();
            let _ = observer.await;
        }
    }

    pub(crate) async fn autonomous_completion_is_parent_session(&self) -> bool {
        !self
            .state
            .lock()
            .await
            .session_configuration
            .session_source
            .is_non_root_agent()
    }

    pub(crate) async fn mark_autonomous_completion_bootstrap(
        &self,
        turn_id: &str,
    ) -> Option<String> {
        if self.state.lock().await.rencrow_completion_state.is_some() {
            return None;
        }
        let root_task_id = if let Some(scope) = self.active_task_scope().await {
            Some(scope.root_task_id)
        } else {
            self.latest_rencrow_task_state()
                .await
                .ok()
                .flatten()
                .filter(|state| state.boundary.current_task_id.is_some())
                .map(|state| state.boundary.root_task_id)
        }?;
        let mut state = self.state.lock().await;
        if state.rencrow_completion_state.is_some()
            || !state
                .rencrow_completion_bootstrap_turn_ids
                .insert(turn_id.to_owned())
        {
            return None;
        }
        Some(root_task_id)
    }

    pub(crate) async fn mark_autonomous_completion_turn(&self, turn_id: String) -> bool {
        let mut state = self.state.lock().await;
        if state.rencrow_completion_paused
            || !state
                .rencrow_completion_state
                .as_ref()
                .is_some_and(|(_, contract)| {
                    !contract.paused
                        && contract.blocker.is_none()
                        && contract.conditions.iter().any(|condition| {
                            condition.status
                                != codex_history::task_context::AcceptanceStatus::Satisfied
                                && condition.status
                                    != codex_history::task_context::AcceptanceStatus::Waived
                        })
                })
        {
            return false;
        }
        state.rencrow_autonomous_turn_ids.insert(turn_id)
    }

    pub(crate) async fn is_autonomous_completion_turn(&self, turn_id: &str) -> bool {
        self.state
            .lock()
            .await
            .rencrow_autonomous_turn_ids
            .contains(turn_id)
    }

    pub(crate) async fn mark_autonomous_completion_continued(
        &self,
        turn_id: &str,
        host_event_key: &str,
    ) -> bool {
        {
            let mut state = self.state.lock().await;
            if state.rencrow_completion_paused
                || state.rencrow_completion_state.is_none()
                || state
                    .rencrow_completion_state
                    .as_ref()
                    .is_some_and(|(_, contract)| {
                        contract.paused
                            || contract
                                .consumed_host_events
                                .iter()
                                .any(|event| event == host_event_key)
                    })
            {
                return false;
            }
            state.rencrow_completion_continued_for_turn = Some(turn_id.to_owned());
            if let Some((_, contract)) = state.rencrow_completion_state.as_mut() {
                contract
                    .consumed_host_events
                    .push(host_event_key.to_owned());
            }
        }
        if let Err(error) = self.persist_autonomous_completion_snapshot().await {
            tracing::error!(%error, "failed to persist consumed autonomous completion event");
            self.pause_autonomous_completion().await;
            return false;
        }
        true
    }

    pub(crate) async fn pause_autonomous_completion(&self) {
        {
            let mut state = self.state.lock().await;
            state.rencrow_completion_paused = true;
            if let Some((_, contract)) = state.rencrow_completion_state.as_mut() {
                contract.paused = true;
            }
        }
        if let Err(error) = self.persist_autonomous_completion_snapshot().await {
            tracing::error!(%error, "failed to persist autonomous completion pause");
        }
    }

    pub(crate) async fn resume_autonomous_completion_from_user_turn(&self) -> Result<bool, String> {
        {
            let mut state = self.state.lock().await;
            let Some((_, contract)) = state.rencrow_completion_state.as_mut() else {
                return Ok(false);
            };
            contract.paused = false;
            state.rencrow_completion_paused = false;
            state.rencrow_completion_resume_generation =
                state.rencrow_completion_resume_generation.saturating_add(1);
        }
        if let Err(error) = self.persist_autonomous_completion_snapshot().await {
            self.pause_autonomous_completion().await;
            return Err(error);
        }
        Ok(true)
    }

    pub(crate) async fn begin_task_scope(
        &self,
        root_task_id: String,
        task_id: String,
        task_revision: u64,
        begin_call_id: String,
        allow_unfinished_root_rebind: bool,
    ) -> Result<crate::state::ActiveTaskScope, String> {
        let history_cursor = self.clone_history().await.raw_items().count();
        let mut state = self.state.lock().await;
        let requested = crate::state::ActiveTaskScope {
            root_task_id,
            task_id,
            task_revision,
            begin_call_id,
            start_history_cursor: history_cursor,
        };
        match state.active_task_scope.as_ref() {
            Some(active)
                if active.root_task_id == requested.root_task_id
                    && active.task_id == requested.task_id
                    && active.task_revision == requested.task_revision =>
            {
                // An exact begin replay is idempotent and keeps the original host cursor/call ID.
                Ok(active.clone())
            }
            Some(active)
                if allow_unfinished_root_rebind
                    && active.root_task_id == requested.root_task_id
                    && active.task_id == active.root_task_id =>
            {
                if state.pending_task_boundary.is_some() {
                    return Err(
                        "cannot start another unit while a task boundary commit is pending".into(),
                    );
                }
                // Root plan lifetime does not own the active execution scope. Preserve its
                // history and receipts, and move only the cursor for future work.
                state.active_task_scope = Some(requested.clone());
                state.rencrow_scope_needs_rebase = false;
                Ok(requested)
            }
            Some(active) => Err(format!(
                "task scope `{}` revision {} is already active; finish it before beginning `{}` revision {}",
                active.task_id, active.task_revision, requested.task_id, requested.task_revision
            )),
            None => {
                state.active_task_scope = Some(requested.clone());
                state.rencrow_scope_needs_rebase = false;
                Ok(requested)
            }
        }
    }

    pub(crate) async fn validate_task_scope_begin(
        &self,
        root_task_id: &str,
        task_id: &str,
        task_revision: u64,
        allow_unfinished_root_rebind: bool,
    ) -> Result<(), String> {
        let state = self.state.lock().await;
        match state.active_task_scope.as_ref() {
            Some(active)
                if active.root_task_id == root_task_id
                    && active.task_id == task_id
                    && active.task_revision == task_revision =>
            {
                Ok(())
            }
            Some(active)
                if allow_unfinished_root_rebind
                    && active.root_task_id == root_task_id
                    && active.task_id == active.root_task_id =>
            {
                if state.pending_task_boundary.is_some() {
                    Err("cannot start another unit while a task boundary commit is pending".into())
                } else {
                    Ok(())
                }
            }
            Some(active) => Err(format!(
                "task scope `{}` revision {} is already active; finish it before beginning `{}` revision {}",
                active.task_id, active.task_revision, task_id, task_revision
            )),
            None => Ok(()),
        }
    }

    pub(crate) async fn active_task_scope(&self) -> Option<crate::state::ActiveTaskScope> {
        let (scope, needs_rebase) = {
            let state = self.state.lock().await;
            (
                state.active_task_scope.clone()?,
                state.rencrow_scope_needs_rebase,
            )
        };
        if !needs_rebase {
            return Some(scope);
        }
        let history = self.clone_history().await;
        let begin_index = history.annotated_items().iter().position(|envelope| {
            matches!(
                &envelope.item,
                ResponseItem::FunctionCall { name, call_id, .. }
                    if name == "task_context_boundary" && call_id == &scope.begin_call_id
            )
        })?;
        let mut state = self.state.lock().await;
        if let Some(active) = state
            .active_task_scope
            .as_mut()
            .filter(|active| active.begin_call_id == scope.begin_call_id)
        {
            active.start_history_cursor = begin_index;
        } else {
            return None;
        }
        state.rencrow_scope_needs_rebase = false;
        let scope = state.active_task_scope.clone()?;
        for attempt in state
            .task_patch_attempts
            .values_mut()
            .filter(|attempt| attempt.scope.begin_call_id == scope.begin_call_id)
        {
            attempt.scope.start_history_cursor = begin_index;
        }
        Some(scope)
    }

    pub(crate) async fn task_check_completion_root_id(&self) -> Option<String> {
        let state = self.state.lock().await;
        state
            .active_task_scope
            .as_ref()
            .map(|scope| scope.root_task_id.clone())
            .or_else(|| {
                state
                    .rencrow_completion_state
                    .as_ref()
                    .map(|(root_task_id, _)| root_task_id.clone())
            })
    }

    pub(crate) async fn rebase_active_task_scope_cursor(
        &self,
        begin_call_id: &str,
        start_history_cursor: usize,
    ) {
        let mut state = self.state.lock().await;
        if let Some(scope) = state
            .active_task_scope
            .as_mut()
            .filter(|scope| scope.begin_call_id == begin_call_id)
        {
            scope.start_history_cursor = start_history_cursor;
        }
    }

    pub(crate) async fn begin_task_patch_attempt(&self, call_id: &str, local_environment: bool) {
        let mut state = self.state.lock().await;
        let Some(scope) = state.active_task_scope.clone() else {
            return;
        };
        state.task_patch_attempts.insert(
            call_id.to_string(),
            crate::state::TaskPatchAttempt {
                scope,
                local_environment,
                paths: Vec::new(),
                completed_successfully: false,
            },
        );
    }

    pub(crate) async fn finish_task_patch_attempt(
        &self,
        call_id: &str,
        paths: Vec<std::path::PathBuf>,
        completed_successfully: bool,
    ) {
        let mut state = self.state.lock().await;
        let Some(attempt) = state.task_patch_attempts.get_mut(call_id) else {
            return;
        };
        attempt.paths = paths;
        attempt.completed_successfully = completed_successfully;
    }

    pub(crate) async fn verified_task_patch_call_ids(
        &self,
        scope: &crate::state::ActiveTaskScope,
        workspace_root: &std::path::Path,
        artifacts: &[crate::task_check::TaskArtifactStamp],
    ) -> Vec<String> {
        let root = match std::fs::canonicalize(workspace_root) {
            Ok(root) => root,
            Err(_) => return Vec::new(),
        };
        let artifact_paths = artifacts
            .iter()
            .map(|artifact| artifact.path.clone())
            .collect::<HashSet<_>>();
        let state = self.state.lock().await;
        state
            .task_patch_attempts
            .iter()
            .filter_map(|(call_id, attempt)| {
                // The history cursor is a mutable projection index that can be rebased
                // after a checkpoint. Scope identity is the host begin call and task
                // revision; requiring cursor equality would discard valid patch receipts
                // recorded before that rebase.
                if !attempt.scope.same_execution(scope)
                    || !attempt.local_environment
                    || !attempt.completed_successfully
                    || attempt.paths.is_empty()
                {
                    return None;
                }
                let paths = attempt
                    .paths
                    .iter()
                    .map(|path| {
                        let canonical = if path.is_absolute() {
                            std::fs::canonicalize(path).ok()?
                        } else {
                            std::fs::canonicalize(root.join(path)).ok()?
                        };
                        canonical
                            .strip_prefix(&root)
                            .ok()
                            .map(std::path::Path::to_path_buf)
                    })
                    .collect::<Option<Vec<_>>>()?;
                if paths.iter().all(|path| {
                    !path.as_os_str().is_empty()
                        && path
                            .components()
                            .all(|component| matches!(component, std::path::Component::Normal(_)))
                        && artifact_paths.contains(path)
                }) {
                    Some(call_id.clone())
                } else {
                    None
                }
            })
            .collect()
    }

    pub(crate) async fn latest_task_check_for_scope(
        &self,
        task_id: &str,
        task_revision: u64,
    ) -> Option<crate::task_check::TaskCheckAttempt> {
        self.state
            .lock()
            .await
            .task_check_ledger
            .latest_for_task(task_id, task_revision)
            .cloned()
    }

    pub(crate) async fn unresolved_task_check_obligations(
        &self,
        scope: &crate::state::ActiveTaskScope,
    ) -> usize {
        self.state
            .lock()
            .await
            .task_check_ledger
            .unresolved_count_for_scope(scope)
    }

    pub(crate) async fn unresolved_task_check_obligations_for_root(
        &self,
        root_task_id: &str,
    ) -> usize {
        self.state
            .lock()
            .await
            .task_check_ledger
            .unresolved_count_for_root(root_task_id)
    }

    pub(super) async fn task_check_attempt_by_call(
        &self,
        call_id: &str,
    ) -> Option<crate::task_check::TaskCheckAttempt> {
        self.state
            .lock()
            .await
            .task_check_ledger
            .attempt_for_call(call_id)
            .cloned()
    }

    pub(super) async fn task_evidence_artifact_bytes(
        &self,
        evidence: &codex_history::task_context::CompletionEvidenceRef,
    ) -> Option<std::collections::HashSet<(String, String)>> {
        let call_id = evidence
            .artifact_ref
            .strip_prefix("task-check:")?
            .rsplit_once(':')?
            .0;
        let attempt = self.task_check_attempt_by_call(call_id).await?;
        let crate::task_check::TaskCheckAttemptStatus::Finished(receipt) = attempt.status else {
            return None;
        };
        if !receipt.succeeded
            || receipt.exit_code != 0
            || receipt.task_id != evidence.task_id
            || receipt.task_revision != evidence.task_revision
            || receipt.artifact_before != receipt.artifact_after
        {
            return None;
        }
        Some(
            receipt
                .artifact_after
                .into_iter()
                .map(|stamp| (stamp.path.to_string_lossy().into_owned(), stamp.sha256))
                .collect(),
        )
    }

    pub(crate) async fn set_pending_task_boundary(
        &self,
        pending: crate::state::PendingTaskBoundary,
    ) -> Result<(), String> {
        let mut state = self.state.lock().await;
        if state.pending_task_boundary.is_some() {
            return Err("a task boundary is already prepared".into());
        }
        state.pending_task_boundary = Some(pending);
        Ok(())
    }

    pub(crate) async fn pending_task_boundary(&self) -> Option<crate::state::PendingTaskBoundary> {
        self.state.lock().await.pending_task_boundary.clone()
    }

    pub(crate) async fn begin_task_check(
        &self,
        call_id: String,
        request: crate::task_check::TaskEvidenceRequest,
        workspace_root: std::path::PathBuf,
        before: Vec<crate::task_check::TaskArtifactStamp>,
    ) {
        self.begin_task_check_with_obligation(call_id, request, workspace_root, before, None)
            .await;
    }

    pub(crate) async fn begin_task_check_with_obligation(
        &self,
        call_id: String,
        request: crate::task_check::TaskEvidenceRequest,
        workspace_root: std::path::PathBuf,
        before: Vec<crate::task_check::TaskArtifactStamp>,
        obligation: Option<crate::task_check::TaskCheckObligation>,
    ) {
        self.state
            .lock()
            .await
            .task_check_ledger
            .begin_with_obligation(call_id, request, workspace_root, before, obligation);
        if let Err(error) = self.persist_autonomous_completion_snapshot().await {
            let mut state = self.state.lock().await;
            state.rencrow_completion_paused = true;
            if let Some((_, contract)) = state.rencrow_completion_state.as_mut() {
                contract.paused = true;
            }
            tracing::error!(%error, "failed to persist pending task verification; autonomous completion paused");
        }
    }

    pub(crate) async fn finish_task_check(
        &self,
        call_id: &str,
        exit_code: i32,
        completed: bool,
        after: Vec<crate::task_check::TaskArtifactStamp>,
    ) -> Option<crate::task_check::TaskCheckReceipt> {
        let receipt = self
            .state
            .lock()
            .await
            .task_check_ledger
            .finish(call_id, exit_code, completed, after);
        if let Err(error) = self.persist_autonomous_completion_snapshot().await {
            let mut state = self.state.lock().await;
            state.rencrow_completion_paused = true;
            if let Some((_, contract)) = state.rencrow_completion_state.as_mut() {
                contract.paused = true;
            }
            tracing::error!(%error, "failed to persist task verification result; autonomous completion paused");
        }
        receipt
    }

    #[cfg(test)]
    pub(crate) async fn task_check_receipt(
        &self,
        call_id: &str,
    ) -> Option<crate::task_check::TaskCheckReceipt> {
        self.state
            .lock()
            .await
            .task_check_ledger
            .get(call_id)
            .cloned()
    }

    #[cfg(test)]
    pub(crate) async fn task_check_snapshot(&self) -> Vec<crate::task_check::TaskCheckSnapshot> {
        self.state.lock().await.task_check_ledger.snapshot()
    }

    /// Returns the concrete identity for this thread.
    pub(crate) fn thread_id(&self) -> ThreadId {
        self.thread_id
    }

    /// Returns the identity shared by the root thread and all descendant threads.
    pub(crate) fn session_id(&self) -> SessionId {
        self.services.agent_control.session_id()
    }

    pub(crate) async fn store_rencrow_execution_evidence(
        &self,
        evidence: crate::rencrow_execution_guard::GuardExecutionEvidence,
    ) {
        let mut state = self.state.lock().await;
        if state.last_started_turn_id.as_deref() == Some(evidence.turn_id.as_str()) {
            state.rencrow_execution_evidence = Some(evidence);
        }
    }

    pub(crate) async fn rencrow_execution_evidence_for_parent(
        &self,
        parent_thread_id: ThreadId,
    ) -> Option<crate::rencrow_execution_guard::GuardExecutionEvidence> {
        let state = self.state.lock().await;
        let evidence = state.rencrow_execution_evidence.as_ref()?;
        (state.last_started_turn_id.as_deref() == Some(evidence.turn_id.as_str())
            && state.session_configuration.parent_thread_id == Some(parent_thread_id)
            && evidence.parent_thread_id == parent_thread_id
            && evidence.thread_id == self.thread_id
            && evidence.has_diff
            && evidence.checked)
            .then(|| evidence.clone())
    }

    pub(crate) async fn consume_rencrow_child_execution_evidence(
        &self,
        evidence: &crate::rencrow_execution_guard::GuardExecutionEvidence,
    ) -> bool {
        if evidence.parent_thread_id != self.thread_id || !evidence.has_diff || !evidence.checked {
            return false;
        }
        self.state
            .lock()
            .await
            .rencrow_consumed_child_receipts
            .insert((evidence.thread_id, evidence.turn_id.clone()))
    }

    #[cfg(test)]
    pub(crate) async fn rencrow_execution_evidence_for_test(
        &self,
    ) -> Option<crate::rencrow_execution_guard::GuardExecutionEvidence> {
        self.state.lock().await.rencrow_execution_evidence.clone()
    }

    pub(crate) async fn originator(&self) -> String {
        let state = self.state.lock().await;
        state.session_configuration.originator.clone()
    }

    pub(crate) async fn responses_metadata(
        &self,
        step_context: &StepContext,
        request_kind: CodexResponsesRequestKind,
    ) -> CodexResponsesMetadata {
        let (window_id, window_number, context_window_id) = self.current_window().await;
        let mut responses_metadata = step_context.turn.turn_metadata_state.to_responses_metadata(
            self.installation_id.clone(),
            window_id,
            request_kind,
        );
        ExecutionMetadata::from_settings(&step_context.settings).apply_to(&mut responses_metadata);
        responses_metadata.tool_namespaces_info = if step_context
            .turn
            .config
            .tool_registry
            .turn_metadata_includes_tool_info
            && step_context.settings.model_info.use_responses_lite
        {
            step_context.tool_router.tool_namespaces_info().cloned()
        } else {
            None
        };
        self.with_window_and_fork_metadata(
            &step_context.turn,
            responses_metadata,
            window_number,
            context_window_id,
        )
    }

    // TODO(CDXENT-454): Build the compaction request and metadata from the captured execution.
    // Remote compaction currently attaches only finalized tool inventory because the rest of the
    // request remains turn-backed; local compaction does not have a finalized request inventory.
    pub(crate) async fn compaction_responses_metadata(
        &self,
        turn_context: &TurnContext,
        compaction_metadata: CompactionTurnMetadata,
    ) -> CodexResponsesMetadata {
        let (window_id, window_number, context_window_id) = self.current_window().await;
        let responses_metadata = turn_context.turn_metadata_state.to_responses_metadata(
            self.installation_id.clone(),
            window_id,
            CodexResponsesRequestKind::Compaction(compaction_metadata),
        );
        self.with_window_and_fork_metadata(
            turn_context,
            responses_metadata,
            window_number,
            context_window_id,
        )
    }

    fn with_window_and_fork_metadata(
        &self,
        turn_context: &TurnContext,
        responses_metadata: CodexResponsesMetadata,
        window_number: u64,
        context_window_id: uuid::Uuid,
    ) -> CodexResponsesMetadata {
        CodexResponsesMetadata {
            window_number: Some(window_number),
            context_window_id: Some(context_window_id),
            mcp_attribution: Some(self.services.executed_tool_calls.mcp_attribution_snapshot()),
            analytics_enabled: Some(self.services.analytics_events_client.is_enabled()),
            history_ingest_requested: turn_context
                .config
                .token_budget
                .as_ref()
                .is_some_and(|config| config.use_history_notes_extension)
                .then_some(true),
            forked_from_ordinal_exclusive: self
                .forked_from_ordinal_exclusive
                .filter(|_| responses_metadata.forked_from_thread_id.is_some()),
            ..responses_metadata
        }
    }

    #[instrument(name = "session_init", level = "info", skip_all)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn new(
        startup: Option<Arc<super::startup::SessionStartup>>,
        mut session_configuration: SessionConfiguration,
        environment_selections: &[TurnEnvironmentSelection],
        config: Arc<Config>,
        mut instructions: SessionInstructions,
        installation_id: String,
        auth_manager: Arc<AuthManager>,
        models_manager: SharedModelsManager,
        git_root_discovery: Arc<GitRootDiscovery>,
        model_info: ModelInfo,
        exec_policy: Arc<ExecPolicyManager>,
        tx_event: Sender<Event>,
        agent_status: watch::Sender<AgentStatus>,
        mut initial_history: InitialHistory,
        fork_persistence: ForkPersistence,
        session_source: SessionSource,
        skills_service: Arc<HostSkillsService>,
        plugins_manager: Arc<PluginsManager>,
        mcp_manager: Arc<McpManager>,
        code_mode_session_provider: Arc<dyn codex_code_mode::CodeModeSessionProvider>,
        extensions: Arc<codex_extension_api::ExtensionRegistry<crate::config::Config>>,
        mut thread_extension_init: ExtensionDataInit,
        client_mcp_extensions: ClientMcpExtensions,
        agent_control: LocalAgentControl,
        reserved_thread_id: Option<ThreadId>,
        environment_manager: Arc<EnvironmentManager>,
        inherited_environments: Option<TurnEnvironmentSnapshot>,
        analytics_events_client: Option<AnalyticsEventsClient>,
        image_store: Arc<dyn AttachmentStore>,
        thread_store: Arc<dyn ThreadStore>,
        parent_rollout_thread_trace: ThreadTraceContext,
        attestation_provider: Option<Arc<dyn AttestationProvider>>,
        external_time_provider: Option<Arc<dyn TimeProvider>>,
        multi_agent_version: Option<MultiAgentVersion>,
        git_enrichment_policy: GitEnrichmentPolicy,
        windows_sandbox_proxy_settings_mode: codex_sandboxing::WindowsSandboxProxySettingsMode,
    ) -> anyhow::Result<Arc<Self>> {
        debug!(
            "Configuring session: model={}; provider={:?}",
            session_configuration
                .step_settings
                .collaboration_mode
                .model(),
            session_configuration.provider
        );
        let base_instructions_provenance = if config.base_instructions.is_some() {
            Some(
                config
                    .base_instructions_provenance
                    .clone()
                    .unwrap_or(BaseInstructionsProvenance::Custom),
            )
        } else if let Some(inherited_base_instructions) = initial_history.get_base_instructions() {
            let BaseInstructions { text, provenance } = inherited_base_instructions;
            provenance.or_else(|| {
                (text == render_model_instructions(&model_info)).then(|| {
                    BaseInstructionsProvenance::Model {
                        model: model_info.slug.clone(),
                    }
                })
            })
        } else {
            Some(BaseInstructionsProvenance::Model {
                model: model_info.slug.clone(),
            })
        };
        let forked_from_id = session_configuration
            .forked_from_thread_id
            .or_else(|| initial_history.forked_from_id());
        session_configuration.forked_from_thread_id = forked_from_id;
        let forked_from_ordinal_exclusive = match &fork_persistence {
            ForkPersistence::Referenced { history_base, .. } => {
                history_base.map(|position| position.end_ordinal_exclusive)
            }
            ForkPersistence::Copied => match &initial_history {
                InitialHistory::Resumed(resumed) => {
                    // Both local and CCA thread stores place the resumed thread's
                    // canonical SessionMeta first. Never inspect inherited metadata:
                    // an ancestor's history_base describes a different fork boundary.
                    resumed.history.first().and_then(|item| match item {
                        RolloutItem::SessionMeta(meta)
                            if meta.meta.id == resumed.conversation_id =>
                        {
                            codex_rollout::forked_from_ordinal_exclusive(
                                &meta.meta,
                                resumed.rollout_path.as_deref(),
                            )
                        }
                        _ => None,
                    })
                }
                InitialHistory::New | InitialHistory::Cleared | InitialHistory::Forked(_) => None,
            },
        }
        .filter(|_| forked_from_id.is_some());
        let parent_thread_id = session_configuration
            .parent_thread_id
            .or_else(|| initial_history.get_resumed_parent_thread_id());
        session_configuration.parent_thread_id = parent_thread_id;
        if parent_thread_id.is_none() {
            agent_control.set_root_service_tier(
                session_configuration
                    .step_settings
                    .service_tier
                    .clone()
                    .or_else(|| config.service_tier.clone()),
            );
        }
        let is_paginated_subagent = matches!(
            session_configuration.history_mode,
            ThreadHistoryMode::Paginated
        ) && matches!(
            session_configuration.thread_source.as_ref(),
            Some(ThreadSource::Subagent | ThreadSource::GuardianReview)
        );
        if let InitialHistory::Forked(items) = &mut initial_history {
            Self::assign_missing_rollout_response_item_ids(items);
        }
        let multi_agent_version = multi_agent_version.map(OnceLock::from).unwrap_or_default();
        let initial_multi_agent_version = multi_agent_version.get().copied();

        let thread_id = match (&initial_history, reserved_thread_id) {
            (
                InitialHistory::New | InitialHistory::Cleared | InitialHistory::Forked(_),
                Some(thread_id),
            ) => thread_id,
            (InitialHistory::New | InitialHistory::Cleared | InitialHistory::Forked(_), None) => {
                agent_control.generate_thread_id()
            }
            (InitialHistory::Resumed(resumed_history), None) => resumed_history.conversation_id,
            (InitialHistory::Resumed(_), Some(_)) => {
                return Err(anyhow::anyhow!(
                    "reserved thread ID cannot be used when resuming a thread"
                ));
            }
        };
        let isolation = thread_extension_init
            .get::<codex_extension_api::SessionIsolation>()
            .map(|policy| *policy)
            .unwrap_or_default();
        if !session_configuration.session_source.is_non_root_agent()
            && isolation != codex_extension_api::SessionIsolation::Isolated
        {
            instructions.thread_provider = agent_control
                .root_thread_instructions_provider(thread_id, instructions.thread_provider);
        }
        // Ephemeral forks reuse cache routing, without sharing storage or lifecycle identity.
        let fork_cache_key = match &initial_history {
            InitialHistory::Forked(items)
                if config.ephemeral
                    && !session_configuration.session_source.is_non_root_agent() =>
            {
                items.iter().find_map(|item| match item {
                    RolloutItem::SessionMeta(meta) => Some(meta.meta.session_id.to_string()),
                    _ => None,
                })
            }
            InitialHistory::New
            | InitialHistory::Cleared
            | InitialHistory::Resumed(_)
            | InitialHistory::Forked(_) => None,
        };
        let resumed_session_id = match &initial_history {
            InitialHistory::Resumed(resumed) => {
                resumed.history.iter().find_map(|item| match item {
                    RolloutItem::SessionMeta(meta_line) => Some(meta_line.meta.session_id),
                    _ => None,
                })
            }
            InitialHistory::New | InitialHistory::Cleared | InitialHistory::Forked(_) => None,
        };
        // Legacy subagent rollouts synthesized session_id from their own thread ID.
        let resumed_session_id = resumed_session_id.filter(|session_id| {
            !session_configuration.session_source.is_non_root_agent()
                || *session_id != SessionId::from(thread_id)
        });
        // session_id is equal to the root thread's ID.
        let session_id = resumed_session_id.unwrap_or_else(|| {
            if session_configuration.session_source.is_non_root_agent() {
                agent_control.session_id()
            } else {
                SessionId::from(thread_id)
            }
        });
        let initial_auto_compact_window_ids = AutoCompactWindowIds::new_initial();
        let restore_child_window = matches!(&initial_history, InitialHistory::Forked(_))
            && session_configuration.session_source.is_non_root_agent()
            && config.features.enabled(Feature::TokenBudget);
        if restore_child_window && let InitialHistory::Forked(items) = &mut initial_history {
            let child_window_id = initial_auto_compact_window_ids.window_id.to_string();
            for item in items {
                if let RolloutItem::Compacted(checkpoint) = item {
                    checkpoint.window_number = Some(0);
                    checkpoint.first_window_id = Some(child_window_id.clone());
                    checkpoint.previous_window_id = None;
                    checkpoint.window_id = Some(child_window_id.clone());
                }
            }
        }
        let agent_control = agent_control.with_session_id(
            session_id,
            config
                .effective_agent_max_threads(MultiAgentVersion::V2)
                .unwrap_or(usize::MAX),
        );
        let time_provider = crate::current_time::resolve_time_provider(
            config.current_time_reminder.as_ref(),
            external_time_provider,
        )?;
        let selected_capability_roots =
            match thread_extension_init.get::<Vec<SelectedCapabilityRoot>>() {
                Some(roots) => roots.as_ref().clone(),
                None => {
                    let roots = initial_history.get_selected_capability_roots();
                    if !roots.is_empty() {
                        thread_extension_init.insert(roots.clone());
                    }
                    roots
                }
            };
        thread_extension_init.insert(codex_extension_api::ThreadOriginator(
            session_configuration.originator.clone(),
        ));
        // Publish the already resolved model before extensions make startup decisions.
        // Turn construction refreshes this attachment when the selected model changes.
        thread_extension_init.insert(model_info);
        let tool_policy = thread_extension_init
            .get::<codex_extension_api::ToolPolicy>()
            .unwrap_or_else(|| {
                // Older reviewer rollouts predate the explicit startup policy.
                if crate::guardian::is_basic_session_source(&session_configuration.session_source) {
                    Arc::new(codex_guardian_reviewer::reviewer_tool_policy())
                } else {
                    Arc::default()
                }
            });
        // Narrow the policy to the configured allowlist, so an unlisted tool is neither shown to
        // the model nor executed (ROLE_TOOL_POLICY.md).
        let tool_policy = match config.rencrow_tool_allowlist.as_deref() {
            Some(allowlist) => Arc::new(crate::tools::rencrow_tool_allowlist::narrow_policy(
                &tool_policy,
                allowlist,
            )),
            None => tool_policy,
        };
        let mcp_thread_init = thread_extension_init.clone();
        let thread_extension_data = codex_extension_api::ExtensionData::new_with_init(
            thread_id.to_string(),
            thread_extension_init,
        );
        // Capture follows the flag; replay selects reviewer policy from the saved checkpoint.
        let guardian_context_mode = GuardianContextMode::from_features(&config.features);
        thread_extension_data.insert(crate::context::GuardianReviewEvidence::default());
        // Kick off independent async setup tasks in parallel to reduce startup latency.
        //
        // - initialize thread persistence with new or resumed session info
        // - perform default shell discovery
        // - load history metadata (skipped for subagents)
        let persistence_auth = futures::FutureExt::shared(auth_manager.auth());
        let mcp_auth = persistence_auth.clone();
        let thread_persistence_fut = async {
            if config.ephemeral {
                Ok::<_, anyhow::Error>((None, LiveThreadInitGuard::new(/*live_thread*/ None)))
            } else {
                let mut local_guard = LiveThreadInitGuard::default();
                let mut managed_guard = match &startup {
                    Some(startup) => Some(startup.persistence.lock().await),
                    None => None,
                };
                let guard = managed_guard.as_deref_mut().unwrap_or(&mut local_guard);
                let live_thread = match &initial_history {
                    InitialHistory::New | InitialHistory::Cleared | InitialHistory::Forked(_) => {
                        let auth = persistence_auth.await;
                        let params = CreateThreadParams {
                            creator_user_id: auth.as_ref().and_then(CodexAuth::get_chatgpt_user_id),
                            creator_account_id: auth.as_ref().and_then(CodexAuth::get_account_id),
                            session_id,
                            thread_id,
                            extra_config: config.extra_config.clone(),
                            forked_from_id,
                            parent_thread_id,
                            source: session_source,
                            thread_source: session_configuration.thread_source.clone(),
                            originator: session_configuration.originator.clone(),
                            base_instructions: BaseInstructions {
                                text: session_configuration.base_instructions.clone(),
                                provenance: base_instructions_provenance.clone(),
                            },
                            dynamic_tools: session_configuration.dynamic_tools.clone(),
                            selected_capability_roots: selected_capability_roots.clone(),
                            multi_agent_version: initial_multi_agent_version,
                            history_mode: session_configuration.history_mode,
                            history_base: match &fork_persistence {
                                ForkPersistence::Copied => None,
                                ForkPersistence::Referenced { history_base, .. } => *history_base,
                            },
                            subagent_history_start_ordinal: None,
                            initial_window_id: initial_auto_compact_window_ids
                                .window_id
                                .to_string(),
                            runtime_workspace_roots: Some(config.workspace_roots.clone()),
                            metadata: ThreadPersistenceMetadata {
                                cwd: Some(config.cwd.to_path_buf()),
                                model_provider: config.model_provider_id.clone(),
                                memory_mode: if config.memories.generate_memories {
                                    ThreadMemoryMode::Enabled
                                } else {
                                    ThreadMemoryMode::Disabled
                                },
                            },
                        };
                        if is_paginated_subagent
                            && matches!(&fork_persistence, ForkPersistence::Copied)
                            && let InitialHistory::Forked(items) = &initial_history
                        {
                            LiveThread::create_with_inherited_model_context(
                                Arc::clone(&thread_store),
                                params,
                                items,
                                guard,
                            )
                            .await?
                        } else {
                            guard
                                .acquire(LiveThread::create(Arc::clone(&thread_store), params))
                                .await?
                        }
                    }
                    InitialHistory::Resumed(resumed_history) => {
                        let params = ResumeThreadParams {
                            thread_id: resumed_history.conversation_id,
                            rollout_path: resumed_history.rollout_path.clone(),
                            history: Some(resumed_history.history.clone()),
                            include_archived: true,
                            metadata: ThreadPersistenceMetadata {
                                cwd: Some(config.cwd.to_path_buf()),
                                model_provider: config.model_provider_id.clone(),
                                memory_mode: if config.memories.generate_memories {
                                    ThreadMemoryMode::Enabled
                                } else {
                                    ThreadMemoryMode::Disabled
                                },
                            },
                        };
                        guard
                            .acquire(LiveThread::resume(
                                Arc::clone(&thread_store),
                                session_configuration.history_mode,
                                params,
                            ))
                            .await?
                    }
                };
                Ok((Some(live_thread), local_guard))
            }
        }
        .instrument(info_span!(
            "session_init.thread_persistence",
            otel.name = "session_init.thread_persistence",
            session_init.ephemeral = config.ephemeral,
        ));
        let state_db_fut = async {
            if config.ephemeral {
                None
            } else if let Some(local_store) =
                thread_store.as_any().downcast_ref::<LocalThreadStore>()
            {
                local_store.state_db().await
            } else {
                None
            }
        }
        .instrument(info_span!(
            "session_init.state_db",
            otel.name = "session_init.state_db",
            session_init.ephemeral = config.ephemeral,
        ));

        let mut mcp_auth_changes = auth_manager.auth_change_receiver();
        let plugins_manager_for_prewarm = Arc::clone(&plugins_manager);
        let config_for_mcp = Arc::clone(&config);
        let mcp_manager_for_mcp = Arc::clone(&mcp_manager);
        let mcp_thread_init_for_startup = &mcp_thread_init;
        let thread_extension_data_for_mcp = &thread_extension_data;
        let mcp_originator = session_configuration.originator.clone();
        let mcp_session_source = session_configuration.session_source.clone();
        let mcp_disabled_plugin_ids = session_configuration.disabled_plugin_ids.clone();
        let mcp_runtime_cwd = environment_selections
            .first()
            .and_then(|environment| environment.cwd.to_abs_path().ok())
            .map(|cwd| cwd.to_path_buf())
            .unwrap_or_else(|| session_configuration.cwd().to_path_buf());
        let auth_and_mcp_fut = async move {
            let auth = mcp_auth.await;
            if config_for_mcp.features.plugin_recommendations_enabled() {
                let plugins_config = config_for_mcp.plugins_config_input();
                let auth_for_prewarm = auth.clone();
                // Fetch the catalog while MCP and plugin/skill initialization continue.
                // Context construction still handles filtering and prompt insertion.
                tokio::spawn(async move {
                    plugins_manager_for_prewarm
                        .recommended_plugins_mode_for_config(
                            &plugins_config,
                            auth_for_prewarm.as_ref(),
                        )
                        .await;
                });
            }
            let mcp_projection = mcp_manager_for_mcp
                .runtime_config_for_step(
                    &config_for_mcp,
                    mcp_thread_init_for_startup,
                    thread_extension_data_for_mcp,
                    McpThreadIdentity {
                        auth_changed: false,
                        session_source: &mcp_session_source,
                        originator: &mcp_originator,
                        disabled_plugin_ids: &mcp_disabled_plugin_ids,
                        environments: McpEnvironmentScope::Selected(environment_selections),
                    },
                    /*ready_selected_capability_roots*/ &[],
                    /*executor_capability_discovery*/ None,
                )
                .await;
            (auth, mcp_projection)
        }
        .instrument(info_span!(
            "session_init.auth_mcp",
            otel.name = "session_init.auth_mcp",
        ));

        // Join all independent futures.
        let (thread_persistence_result, state_db_ctx, (auth, mcp_projection)) =
            tokio::join!(thread_persistence_fut, state_db_fut, auth_and_mcp_fut);

        let (live_thread, mut live_thread_init) = thread_persistence_result.map_err(|e| {
            error!("failed to initialize thread persistence: {e:#}");
            e
        })?;
        let session_result: anyhow::Result<Arc<Self>> = async {
            let rollout_path = if let Some(live_thread) = live_thread.as_ref() {
                live_thread.local_rollout_path().await?
            } else {
                None
            };
            let trace_agent_path = session_configuration
                .session_source
                .get_agent_path()
                .unwrap_or_else(codex_protocol::AgentPath::root);
            let trace_task_name =
                (!trace_agent_path.is_root()).then(|| trace_agent_path.name().to_string());
            let trace_metadata = ThreadStartedTraceMetadata {
                thread_id: thread_id.to_string(),
                agent_path: trace_agent_path.to_string(),
                task_name: trace_task_name,
                nickname: session_configuration.session_source.get_nickname(),
                agent_role: session_configuration.session_source.get_agent_role(),
                session_source: session_configuration.session_source.clone(),
                cwd: session_configuration.cwd().to_path_buf(),
                rollout_path: rollout_path.clone(),
                model: session_configuration.step_settings.collaboration_mode.model().to_string(),
                provider_name: config.model_provider_id.clone(),
                approval_policy: session_configuration.step_settings.approval_policy.value().to_string(),
                sandbox_policy: format!(
                    "{:?}",
                    session_configuration.sandbox_policy(environment_selections)
                ),
            };
            let rollout_thread_trace = if matches!(
                session_configuration.session_source,
                SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. })
            ) {
                // Spawned child threads are part of their root rollout tree. If the
                // parent had no trace bundle, do not create an orphan child bundle
                // that looks like an independent rollout.
                parent_rollout_thread_trace.start_child_thread_trace_or_disabled(trace_metadata)
            } else {
                ThreadTraceContext::start_root_or_disabled(trace_metadata)
            };

            let mut post_session_configured_events = Vec::<Event>::new();

            for usage in config.features.legacy_feature_usages() {
                post_session_configured_events.push(Event {
                    id: INITIAL_SUBMIT_ID.to_owned(),
                    msg: EventMsg::DeprecationNotice(DeprecationNoticeEvent {
                        summary: usage.summary.clone(),
                        details: usage.details.clone(),
                    }),
                });
            }
            for message in &config.startup_warnings {
                post_session_configured_events.push(Event {
                    id: "".to_owned(),
                    msg: EventMsg::Warning(WarningEvent {
                        message: message.clone(),
                    }),
                });
            }
            let effective_config = config.config_layer_stack.effective_config();
            let config_path = config.codex_home.join(CONFIG_TOML_FILE);
            if let Some(event) = unstable_features_warning_event(
                effective_config.get("features").and_then(TomlValue::as_table),
                config.suppress_unstable_features_warning,
                &config.features,
                &config_path.display().to_string(),
            ) {
                post_session_configured_events.push(event);
            }
            let telemetry_auth = auth.as_ref();
            let auth_mode = telemetry_auth
                .map(CodexAuth::auth_mode)
                .map(TelemetryAuthMode::from);
            let account_id = telemetry_auth.and_then(CodexAuth::get_account_id);
            let account_email = telemetry_auth.and_then(CodexAuth::get_account_email);
            let originator = session_configuration.originator.clone();
            let terminal_type = user_agent();
            let session_model = session_configuration.step_settings.collaboration_mode.model().to_string();
            let auth_env_telemetry = collect_auth_env_telemetry(
                session_configuration.provider.info(),
                auth_manager.codex_api_key_env_enabled(),
            );
            let mut session_telemetry = SessionTelemetry::new(
                thread_id,
                session_model.as_str(),
                session_model.as_str(),
                account_id.clone(),
                account_email.clone(),
                auth_mode,
                originator.clone(),
                config.otel.log_user_prompt,
                terminal_type.clone(),
                session_configuration.session_source.clone(),
            )
            .with_auth_env(auth_env_telemetry.to_otel_metadata())
            .with_tool_result_log_config(config.otel.tool_result);
            if let Some(metrics) = thread_extension_data.get::<codex_otel::MetricsClient>() {
                session_telemetry = session_telemetry.with_metrics(metrics.as_ref().clone());
            }
            if let Some(service_name) = session_configuration.metrics_service_name.as_deref() {
                session_telemetry = session_telemetry.with_metrics_service_name(service_name);
            }
            let network_proxy_audit_metadata = NetworkProxyAuditMetadata {
                conversation_id: Some(thread_id.to_string()),
                app_version: Some(env!("CARGO_PKG_VERSION").to_string()),
                user_account_id: account_id,
                auth_mode: auth_mode.map(|mode| mode.to_string()),
                originator: Some(originator),
                user_email: account_email,
                terminal_type: Some(terminal_type),
                model: Some(session_model.clone()),
                slug: Some(session_model),
            };
            crate::config::emit_session_start_metrics(config.as_ref(), &session_telemetry);
            let is_worktree = session_configuration.cwd().canonicalize().ok().and_then(|cwd| {
                codex_git_utils::repository_identity(&cwd).and_then(|_| {
                    get_git_repo_root(&cwd).map(|root| root.join(".git").is_file())
                })
            });
            let is_worktree_tag = match is_worktree {
                Some(true) => "true",
                Some(false) => "false",
                None => "unknown",
            };
            let is_git_tag = if get_git_repo_root(session_configuration.cwd()).is_some() {
                "true"
            } else {
                "false"
            };
            session_telemetry.counter(
                THREAD_STARTED_METRIC,
                /*inc*/ 1,
                &[("is_git", is_git_tag), ("is_worktree", is_worktree_tag)],
            );

            let mcp_server_names =
                codex_mcp::effective_mcp_servers(
                    &mcp_projection.config,
                    auth.as_ref(),
                )
                    .into_iter()
                    .filter_map(|(name, server)| server.enabled().then_some(name))
                    .collect::<Vec<_>>();
            session_telemetry.conversation_starts(
                config.model_provider.name.as_str(),
                session_configuration.step_settings.collaboration_mode.reasoning_effort(),
                config
                    .model_reasoning_summary
                    .unwrap_or(ReasoningSummaryConfig::Auto),
                config.model_context_window,
                config.model_auto_compact_token_limit,
                config.permissions.approval_policy.value(),
                config
                    .permissions
                    .legacy_sandbox_policy(session_configuration.cwd().as_path()),
                mcp_server_names.iter().map(String::as_str).collect(),
            );

            let use_zsh_fork_shell = config.features.enabled(Feature::ShellZshFork);
            let default_shell = if let Some(user_shell_override) =
                session_configuration.user_shell_override.clone()
            {
                user_shell_override
            } else if use_zsh_fork_shell {
                let zsh_path = config.zsh_path.as_ref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "zsh fork feature enabled, but no packaged zsh fork is available for this install"
                    )
                })?;
                if zsh_path.is_file() {
                    shell::Shell {
                        shell_type: shell::ShellType::Zsh,
                        shell_path: zsh_path.clone(),
                    }
                } else {
                    shell::get_shell(shell::ShellType::Zsh).ok_or_else(|| {
                        anyhow::anyhow!(
                            "zsh fork feature enabled, but packaged zsh fork `{}` is not usable",
                            zsh_path.display()
                        )
                    })?
                }
            } else {
                shell::default_user_shell()
            };
            let credential_broker_available = config.features.enabled(Feature::NetworkProxy)
                && config
                    .config_layer_stack
                    .requirements()
                    .network
                    .as_ref()
                    .is_none_or(|network| network.value.enabled != Some(false));
            let credential_broker_configured = credential_broker_available
                && effective_config
                    .get("features")
                    .and_then(|features| features.get("network_proxy"))
                    .and_then(|network_proxy| network_proxy.get("credential_broker"))
                    .and_then(TomlValue::as_bool)
                    .unwrap_or(false);
            let credential_broker_active = credential_broker_configured
                && config
                    .permissions
                    .network
                    .as_ref()
                    .is_some_and(crate::config::NetworkProxySpec::credential_broker_enabled);
            let prefer_executor_shell_snapshots = config.features.enabled(Feature::ShellSnapshotV2)
                && config.features.enabled(Feature::ShellTool)
                && config.features.enabled(Feature::UnifiedExec)
                && matches!(
                    codex_tools::UnifiedExecShellMode::for_session(
                        config.features.get(),
                        crate::tools::tool_user_shell_type(&default_shell),
                        config.zsh_path.as_ref(),
                        config.main_execve_wrapper_exe.as_ref(),
                    ),
                    codex_tools::UnifiedExecShellMode::Direct
                );
            let use_executor_shell_snapshots =
                prefer_executor_shell_snapshots && !credential_broker_active;
            let shell_snapshot = if config.features.enabled(Feature::ShellSnapshot)
                && (!use_executor_shell_snapshots || credential_broker_available)
            {
                let snapshot_credential_broker = credential_broker_available.then(|| {
                    let state = if credential_broker_active {
                        SnapshotCredentialBrokerState::Starting
                    } else {
                        SnapshotCredentialBrokerState::Inactive
                    };
                    watch::channel(state).0
                });
                ShellSnapshot::new(
                    config.codex_home.clone(),
                    thread_id,
                    session_telemetry.clone(),
                    state_db_ctx.clone(),
                    snapshot_credential_broker,
                    prefer_executor_shell_snapshots,
                )
            } else {
                ShellSnapshot::disabled()
            };
            let turn_environments = Arc::new(ThreadEnvironments::new(
                environment_manager,
                default_shell.clone(),
                session_configuration.inferred_environment_config(),
                shell_snapshot,
                inherited_environments.unwrap_or_default(),
                config.features.enabled(Feature::DeferredExecutor),
            ));
            turn_environments.update_selections(
                environment_selections,
                &session_configuration.inferred_environment_config(),
            );
            session_configuration.environments = turn_environments.selections();
            let resolved_environments = turn_environments.snapshot().await;
            let agents_md_manager = Arc::new(AgentsMdManager::new(instructions));
            let plugin_skill_warmup = warm_plugins_and_skills_for_session_init(
                Arc::clone(&config),
                Arc::clone(&plugins_manager),
                Arc::clone(&skills_service),
                &resolved_environments,
                extensions.as_ref(),
            )
            .instrument(info_span!(
                "session_init.plugin_skill_warmup",
                otel.name = "session_init.plugin_skill_warmup",
            ));
            let thread_name_lookup =
                thread_title_from_thread_store(live_thread.as_ref(), &thread_store, thread_id)
                    .instrument(info_span!(
                        "session_init.thread_name_lookup",
                        otel.name = "session_init.thread_name_lookup",
                    ));
            let (instruction_refresh, plugin_skill_errors, thread_name) = tokio::join!(
                agents_md_manager.refresh(config.as_ref(), &resolved_environments),
                plugin_skill_warmup,
                thread_name_lookup,
            );
            let (agents_md_result, instruction_warnings) = instruction_refresh;
            // TODO(anp): Present AGENTS.md discovery errors more clearly to the user.
            agents_md_result?;
            post_session_configured_events.extend(
                instruction_warnings.into_iter().map(|message| Event {
                    id: INITIAL_SUBMIT_ID.to_owned(),
                    msg: EventMsg::Warning(WarningEvent { message }),
                }),
            );
            for err in &plugin_skill_errors {
                error!(
                    "failed to load skill {}: {}",
                    err.path.display(),
                    err.message
                );
            }
            session_configuration.thread_name = thread_name.clone();
            let mut state = SessionState::new_with_auto_compact_window_ids(
                session_configuration.clone(),
                initial_auto_compact_window_ids,
                ContextManager::with_guardian_context_mode(
                    guardian_context_mode,
                    &session_configuration.session_source,
                ),
            );
            state.last_started_turn_id = initial_history.get_rollout_items().iter().rev().find_map(|item| {
                match item {
                    RolloutItem::EventMsg(EventMsg::TurnStarted(event)) => Some(event.turn_id.clone()),
                    _ => None,
                }
            });
            state.rencrow_last_history_turn_completed =
                history_ends_with_completed_turn(initial_history.get_rollout_items());
            state.base_instructions_provenance = base_instructions_provenance.clone();
            state.active_disabled_plugin_ids = session_configuration.disabled_plugin_ids.clone();
            let managed_network_requirements_configured = config
                .config_layer_stack
                .requirements_toml()
                .network
                .is_some();
            let managed_network_requirements_enabled = config.managed_network_requirements_enabled();
            let network_approval = Arc::new(NetworkApprovalService::default());
            // The managed proxy can call back into core for allowlist-miss decisions.
            let network_policy_decider_session = if managed_network_requirements_configured {
                config
                    .permissions
                    .network
                    .as_ref()
                    .map(|_| Arc::new(RwLock::new(std::sync::Weak::<Session>::new())))
            } else {
                None
            };
            let blocked_request_observer = config
                .permissions
                .network
                .as_ref()
                .map(|_| build_blocked_request_observer(Arc::clone(&network_approval)));
            let network_policy_decider =
                network_policy_decider_session
                    .as_ref()
                    .map(|network_policy_decider_session| {
                        build_network_policy_decider(
                            Arc::clone(&network_approval),
                            Arc::clone(network_policy_decider_session),
                        )
                    });
            let (network_proxy, session_network_proxy) =
                if let Some(spec) = config
                    .permissions
                    .network
                    .as_ref()
                    .filter(|spec| spec.enabled())
                {
                    let current_exec_policy = exec_policy.current();
                    let (network_proxy, session_network_proxy) = Self::start_managed_network_proxy(
                        spec,
                        current_exec_policy.as_ref(),
                        config.permissions.permission_profile(),
                        config.permissions.windows_sandbox_type,
                        network_policy_decider.as_ref().map(Arc::clone),
                        blocked_request_observer.as_ref().map(Arc::clone),
                        managed_network_requirements_configured,
                        network_proxy_audit_metadata.clone(),
                    )
                    .instrument(info_span!(
                        "session_init.network_proxy",
                        otel.name = "session_init.network_proxy",
                        session_init.managed_network_requirements_enabled =
                            managed_network_requirements_enabled,
                    ))
                    .await?;
                    (Some(network_proxy), Some(session_network_proxy))
                } else {
                    (None, None)
                };
            if let Some(network_proxy) = network_proxy.as_ref()
                && config
                    .permissions
                    .network
                    .as_ref()
                    .is_some_and(crate::config::NetworkProxySpec::credential_broker_enabled)
            {
                turn_environments.set_snapshot_credential_broker(
                    SnapshotCredentialBrokerState::Ready(network_proxy.proxy()),
                );
            }

            // Hooks and extensions share one stable thread-owned MCP runtime handle.
            let mcp_runtime = Arc::new(McpRuntime::empty(
                mcp_projection.config.prefix_mcp_tool_names,
            ));
            let hooks_config = build_hooks_config(
                &config,
                plugins_manager.as_ref(),
                resolved_environments.single_local_environment(),
                &session_configuration.disabled_plugin_ids,
            )
            .await;
            let (hooks, async_hook_results) = Hooks::new(
                hooks_config,
                thread_id,
                Arc::new(CoreHookMcpExecutor {
                    runtime: Arc::clone(&mcp_runtime),
                    thread_id,
                }),
            )?;
            for warning in hooks.startup_warnings() {
                post_session_configured_events.push(Event {
                    id: INITIAL_SUBMIT_ID.to_owned(),
                    msg: EventMsg::Warning(WarningEvent {
                        message: warning.clone(),
                    }),
                });
            }

            let analytics_events_client = if config.analytics_enabled == Some(false) {
                AnalyticsEventsClient::disabled()
            } else {
                analytics_events_client.unwrap_or_else(|| {
                    AnalyticsEventsClient::new(
                        Arc::clone(&auth_manager),
                        config.chatgpt_base_url.trim_end_matches('/').to_string(),
                        config.analytics_enabled,
                    )
                })
            };
            for item in initial_history.get_rollout_items() {
                match item {
                    RolloutItem::Compacted(compacted) => {
                        if let Some(checkpoint) = &compacted.mcp_resource_origins {
                            mcp_runtime.restore_resource_origin_checkpoint(checkpoint);
                        }
                    }
                    RolloutItem::EventMsg(event) => mcp_runtime.observe_event(event),
                    RolloutItem::SessionMeta(_)
                    | RolloutItem::ResponseItem(_)
                    | RolloutItem::InterAgentCommunication(_)
                    | RolloutItem::InterAgentCommunicationMetadata { .. }
                    | RolloutItem::TurnContext(_)
                    | RolloutItem::WorldState(_)
                    | RolloutItem::RealtimeItem(_)
                    | RolloutItem::TokenUsageRecord(_)
                    | RolloutItem::RetainedContext(_)
                    | RolloutItem::RenCrowCompactionCommit { .. }
                | RolloutItem::SecurityRiskScore(_) => {}
                }
            }
            let session_extension_data =
                codex_extension_api::ExtensionData::new(session_id.to_string());
            session_extension_data.insert(analytics_events_client.clone());
            let mcp_resource_client = Arc::new(McpResourceClient::new(Arc::clone(&mcp_runtime)));
            let extension_metrics =
                extension_metrics::from_session_telemetry(session_telemetry.clone());
            let workspace_routing = thread_extension_data
                .get_or_init(|| config.workspace_routing_context());
            for contributor in extensions.thread_lifecycle_contributors() {
                contributor.on_thread_start(codex_extension_api::ThreadStartInput {
                    config: config.as_ref(),
                    session_source: &session_configuration.session_source,
                    persistent_thread_state_available: state_db_ctx.is_some(),
                    environments: environment_selections,
                    mcp_resource_client: Some(Arc::clone(&mcp_resource_client)),
                    extension_metrics: Some(Arc::clone(&extension_metrics)),
                    session_store: &session_extension_data,
                    thread_store: &thread_extension_data,
                }).await;
            }

            let executed_tool_calls = crate::state::ExecutedToolCalls::new(
                &config.features,
                &initial_history,
            );
            let codex_responses_headers = thread_extension_data.get::<crate::CodexResponsesHeaders>();
            // Ephemeral title requests use request-level effort even when managed settings
            // enable overrides. The client-supplied tag selects cache behavior, not permissions.
            let title_request = config.ephemeral && matches!(
                session_configuration.thread_source.as_ref(),
                Some(ThreadSource::Feature(feature)) if feature == "thread_title"
            );
            let reasoning_effort_override_enabled =
                config.features.enabled(Feature::ReasoningEffortOverride) && !title_request;
            let services = SessionServices {
                // Start with an empty connection set. The initialized set is
                // published after SessionConfigured so MCP events follow it.
                mcp_runtime,
                mcp_handler_cache: Default::default(),
                unified_exec_manager: UnifiedExecProcessManager::new(
                    config.background_terminal_max_timeout,
                ),
                elicitations: crate::elicitation::ElicitationService::new(),
                shell_zsh_path: config.zsh_path.clone(),
                main_execve_wrapper_exe: config.main_execve_wrapper_exe.clone(),
                analytics_events_client,
                hooks: arc_swap::ArcSwap::from_pointee(hooks),
                rollout_thread_trace,
                user_shell: Arc::new(default_shell),
                show_raw_agent_reasoning: config.show_raw_agent_reasoning,
                exec_policy,
                auth_manager: Arc::clone(&auth_manager),
                openai_file_upload_client_pool: RouteAwareClientPool::new_without_request_logging(
                    config.http_client_factory(),
                    ClientRouteClass::Api,
                )
                .with_legacy_custom_ca_fallback(),
                session_telemetry,
                models_manager: Arc::clone(&models_manager),
                git_root_discovery,
                tool_approvals: Mutex::new(ApprovalStore::default()),
                runtime_handle: tokio::runtime::Handle::current(),
                skills_service,
                agents_md_manager,
                plugins_manager: Arc::clone(&plugins_manager),
                mcp_manager: Arc::clone(&mcp_manager),
                extensions,
                // TODO(jif): extract session to share between sub-agents
                session_extension_data,
                thread_extension_data,
                selected_capability_roots,
                mcp_thread_init,
                client_mcp_extensions,
                agent_control,
                network_proxy: arc_swap::ArcSwapOption::from(network_proxy.map(Arc::new)),
                network_proxy_audit_metadata,
                managed_network_requirements_configured,
                network_approval: Arc::clone(&network_approval),
                state_db: state_db_ctx.clone(),
                live_thread: live_thread.clone(),
                image_store,
                thread_store: Arc::clone(&thread_store),
                attestation_provider: attestation_provider.clone(),
                time_provider,
                model_client: ModelClient::new(
                    Some(Arc::clone(&auth_manager)),
                    if config.features.enabled(Feature::UseAgentIdentity) {
                        AgentIdentityAuthPolicy::ChatGptAuth
                    } else {
                        AgentIdentityAuthPolicy::JwtOnly
                    },
                    thread_id,
                    session_configuration.provider.info().clone(),
                    session_configuration.session_source.clone(),
                    session_configuration.originator.clone(),
                    config.model_verbosity,
                    config.features.enabled(Feature::ContentItemKinds),
                    reasoning_effort_override_enabled,
                    config.features.enabled(Feature::EnableRequestCompression),
                    config.features.enabled(Feature::RuntimeMetrics),
                    Self::build_model_client_beta_features_header(config.as_ref()),
                    /*concurrent_reasoning_summaries_enabled*/ config
                        .features
                        .enabled(Feature::ConcurrentReasoningSummaries),
                    attestation_provider,
                    config.http_client_factory(),
                    workspace_routing.as_ref().clone(),
                )
                .with_restored_history(matches!(
                    &initial_history,
                    InitialHistory::Resumed(_) | InitialHistory::Forked(_)
                ))
                .with_session_context(
                    crate::guardian::prompt_cache_key_override_for_review_session(
                        &session_configuration.session_source,
                        session_configuration.parent_thread_id,
                    )
                    .or(fork_cache_key),
                    tx_event.clone(),
                    codex_responses_headers,
                ),
                executed_tool_calls: executed_tool_calls.clone(),
                code_mode_service: crate::tools::code_mode::CodeModeService::new(
                    thread_id,
                    Arc::clone(&code_mode_session_provider),
                    &config.code_mode,
                    executed_tool_calls,
                ),
                tool_search_handler_cache: Default::default(),
                turn_environments: Arc::clone(&turn_environments),
            };
            let (mcp_prewarm_tx, mcp_prewarm_rx) = async_channel::bounded(1);
            let sess = Arc::new(Session {
                thread_id,
                installation_id,
                tx_event: tx_event.clone(),
                agent_status,
                state: Mutex::new(state),
                thread_settings_persistence: Semaphore::new(/*permits*/ 1),
                managed_network_proxy_refresh_lock: Semaphore::new(/*permits*/ 1),
                features: config.features.clone(),
                guardian_context_mode,
                isolation,
                tool_policy,
                windows_sandbox_proxy_settings_mode,
                multi_agent_version,
                mcp_refresh: McpRefresh::new(),
                mcp_tool_approval_metadata: Default::default(),
        mcp_elicitation_reviewer_handle: OnceLock::new(),
                mcp_elicitation_lifecycle_handle: OnceLock::new(),
                mcp_prewarm_tx,
                mcp_prewarm_shutdown: CancellationToken::new(),
                mcp_prewarm_task: std::sync::Mutex::new(None),
                conversation: Arc::new(RealtimeConversationManager::new()),
                realtime_history: (session_configuration.history_mode == ThreadHistoryMode::Paginated
                    && services.live_thread.is_some())
                .then(|| Mutex::new(Default::default())),
                active_turn: Mutex::new(None),
                async_hook_results,
                input_queue: InputQueue::new(),
                services,
                git_enrichment_policy,
                fork_persistence,
                forked_from_ordinal_exclusive,
                next_internal_sub_id: AtomicU64::new(0),
            });
            if let Some(startup) = &startup {
                let _ = startup.session.set(Arc::clone(&sess));
            }
            if let Some(network_policy_decider_session) = network_policy_decider_session {
                let mut guard = network_policy_decider_session.write().await;
                *guard = Arc::downgrade(&sess);
            }
            // Dispatch the SessionConfiguredEvent first and then report any errors.
            // If resuming, include converted initial messages in the payload so UIs can render them immediately.
            let initial_messages = initial_history.get_event_msgs();
            let thread_config =
                session_configuration.thread_config_snapshot(turn_environments.selections());
            let events = std::iter::once(Event {
                id: INITIAL_SUBMIT_ID.to_owned(),
                msg: EventMsg::SessionConfigured(SessionConfiguredEvent {
                    session_id,
                    thread_id,
                    cwd: thread_config.cwd().clone(),
                    forked_from_id: thread_config.forked_from_thread_id,
                    parent_thread_id: thread_config.parent_thread_id,
                    thread_source: thread_config.thread_source,
                    thread_name: session_configuration.thread_name.clone(),
                    model: thread_config.model,
                    model_provider_id: thread_config.model_provider_id,
                    service_tier: thread_config.service_tier,
                    approval_policy: thread_config.approval_policy,
                    approvals_reviewer: thread_config.approvals_reviewer,
                    network_proxy: session_network_proxy.filter(|_| {
                        Self::managed_network_proxy_active_for_permission_profile(
                            &thread_config.permission_profile,
                        )
                    }),
                    permission_profile: thread_config.permission_profile,
                    active_permission_profile: thread_config.active_permission_profile,
                    reasoning_effort: thread_config.reasoning_effort,
                    initial_messages,
                    rollout_path,
                }),
            })
            .chain(post_session_configured_events.into_iter());
            for event in events {
                sess.send_event_raw(event).await;
            }
            turn_environments.start_connection_event_forwarding(tx_event.clone());

            let startup_auth_changed = mcp_auth_changes.has_changed().unwrap_or(false);
            if startup_auth_changed {
                mcp_auth_changes.mark_unchanged();
            }
            let latest_auth = sess.services.auth_manager.auth().await;
            let resolved_environment_selections = resolved_environments.all_selections();
            let mcp_projection = if startup_auth_changed
                || mcp_auth_changes.has_changed().unwrap_or(false)
                || resolved_environment_selections.as_slice() != environment_selections
            {
                sess.services
                    .mcp_manager
                    .runtime_config_for_step(
                        config.as_ref(),
                        &sess.services.mcp_thread_init,
                        &sess.services.thread_extension_data,
                        McpThreadIdentity {
                            auth_changed: !sess
                                .services
                                .mcp_runtime
                                .current_auth_matches(latest_auth.as_ref()),
                            session_source: &session_configuration.session_source,
                            originator: &session_configuration.originator,
                            disabled_plugin_ids: &session_configuration.disabled_plugin_ids,
                            environments: McpEnvironmentScope::Selected(&resolved_environment_selections),
                        },
                        /*ready_selected_capability_roots*/ &[],
                        /*executor_capability_discovery*/ None,
                    )
                    .await
            } else {
                mcp_projection
            };
            sess.install_initial_mcp_runtime(
                &session_configuration,
                latest_auth,
                mcp_projection,
                &resolved_environments,
                mcp_runtime_cwd,
            )
            .await?;
            sess.start_mcp_prewarm_worker(mcp_prewarm_rx, mcp_auth_changes);
            sess.schedule_startup_prewarm(sess.get_prompt_base_instructions().await.text)
                .await;
            let session_start_source = match &initial_history {
                InitialHistory::Forked(_) if forked_from_id.is_some() => {
                    codex_hooks::SessionStartSource::Fork
                }
                // `thread/resume` with supplied history uses `Forked` internally
                // without a fork parent, so it should still report `resume`.
                InitialHistory::Resumed(_) | InitialHistory::Forked(_) => {
                    codex_hooks::SessionStartSource::Resume
                }
                InitialHistory::New => codex_hooks::SessionStartSource::Startup,
                InitialHistory::Cleared => codex_hooks::SessionStartSource::Clear,
            };

            // record_initial_history can emit events. We record only after the SessionConfiguredEvent is emitted.
            Box::pin(sess.record_initial_history(initial_history)).await;
            if restore_child_window {
                sess.state.lock().await.restore_auto_compact_window(
                    /*window_number*/ 0,
                    initial_auto_compact_window_ids,
                );
            }
            if matches!(&sess.fork_persistence, ForkPersistence::Referenced { .. }) {
                // Keep the source reserved until the child's history reference is durable.
                sess.try_ensure_rollout_materialized(PersistContext::Standard)
                    .await?;
            }
            {
                let mut state = sess.state.lock().await;
                state.queue_pending_session_start_source(session_start_source);
            }
            Ok(sess)
        }
        .await;
        match session_result {
            Ok(sess) => {
                live_thread_init.commit();
                Ok(sess)
            }
            Err(err) => {
                live_thread_init.discard().await;
                Err(err)
            }
        }
    }
}
