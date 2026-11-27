// Modified by RenCrow Switch Core, 2026-11-16.
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::common_harness_artifacts::load_operation_artifact;
use crate::common_harness_artifacts::write_operation_artifact;
use crate::common_harness_io::ExpectedBaselineFailure;
use crate::common_harness_io::GoTestExpectation;
use crate::common_harness_io::GoTestObservation;
use crate::common_harness_io::observe_go_test_json;
use crate::exec::DEFAULT_EXEC_COMMAND_TIMEOUT_MS;
use crate::exec_policy::prompt_is_rejected_by_policy;
use crate::function_tool::FunctionCallError;
use crate::maybe_emit_implicit_skill_invocation;
use crate::task_check::TaskCheckExecution;
use crate::tools::context::ExecCommandToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::apply_granted_turn_permissions;
use crate::tools::handlers::apply_patch::intercept_apply_patch;
use crate::tools::handlers::file_system_sandbox_policy_context_for_cwd;
use crate::tools::handlers::implicit_granted_permissions;
use crate::tools::handlers::normalize_and_validate_additional_permissions;
use crate::tools::handlers::parse_arguments;
use crate::tools::handlers::parse_arguments_with_base_path;
use crate::tools::handlers::resolve_sandbox_permissions;
use crate::tools::handlers::resolve_tool_environment;
use crate::tools::handlers::rewrite_function_string_argument;
use crate::tools::handlers::updated_hook_command;
use crate::tools::hook_names::HookToolName;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::PostToolUsePayload;
use crate::tools::registry::PreToolUsePayload;
use crate::tools::registry::ToolExecutor;
use crate::unified_exec::ExecCommandRequest;
use crate::unified_exec::UnifiedExecContext;
use crate::unified_exec::UnifiedExecError;
use crate::unified_exec::UnifiedExecProcessManager;
use crate::unified_exec::generate_chunk_id;
use codex_features::Feature;
use codex_history::task_context::CheckOutputParserV1;
use codex_history::task_context::CommonHarnessStateV1;
use codex_history::task_context::ConditionEvidenceV1;
use codex_history::task_context::ConditionKindV1;
use codex_history::task_context::ConditionSpecV1;
use codex_history::task_context::ConditionVerdictV1;
use codex_history::task_context::EvidenceLifecycleV1;
use codex_history::task_context::ExpectedCheckOutcomeV1;
use codex_history::task_context::HostEventV1;
use codex_history::task_context::HostNegativeProofV1;
use codex_history::task_context::OperationKindV1;
use codex_history::task_context::OperationOutcomeV1;
use codex_history::task_context::OperationReceiptV1;
use codex_history::task_context::TaskRunStatusV1;
use codex_otel::SessionTelemetry;
use codex_otel::TOOL_CALL_UNIFIED_EXEC_METRIC;
use codex_sandboxing::SandboxManager;
use codex_sandboxing::SandboxType;
use codex_sandboxing::SandboxablePreference;
use codex_shell_command::shell_detect::detect_shell_type;
use codex_tools::JsonSchema;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use codex_utils_output_truncation::approx_token_count;
use codex_utils_path_uri::PathConvention;
use codex_utils_string::truncate_middle_chars;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;

use super::super::shell_spec::CommandToolOptions;
use super::super::shell_spec::create_exec_command_tool_with_environment_id;
use super::ExecCommandArgs;
use super::ExecCommandEnvironmentArgs;
use super::get_command;
use super::post_unified_exec_tool_use_payload;
use super::shell_mode_for_environment;

// A byte limit is a conservative hard token bound even for byte-fallback tokenizers.
const EXEC_COMMAND_REJECTION_MAX_BYTES: usize = 900;

#[derive(Clone, Copy)]
pub(crate) struct ExecCommandHandlerOptions {
    pub(crate) allow_login_shell: bool,
    pub(crate) allow_tty: bool,
    pub(crate) exec_permission_approvals_enabled: bool,
    pub(crate) include_environment_id: bool,
    pub(crate) include_task_evidence: bool,
    pub(crate) include_shell_parameter: bool,
    pub(crate) include_windows_shell_guidance: bool,
}

#[derive(Clone, Copy)]
enum ExecCommandLifetime {
    Interactive,
    OneShot,
}

pub struct ExecCommandHandler {
    options: ExecCommandHandlerOptions,
    lifetime: ExecCommandLifetime,
}

impl Default for ExecCommandHandler {
    fn default() -> Self {
        Self {
            lifetime: ExecCommandLifetime::Interactive,
            options: ExecCommandHandlerOptions {
                allow_login_shell: false,
                allow_tty: true,
                exec_permission_approvals_enabled: false,
                include_environment_id: false,
                include_task_evidence: false,
                include_shell_parameter: true,
                include_windows_shell_guidance: cfg!(windows),
            },
        }
    }
}

impl ExecCommandHandler {
    pub(crate) fn new(options: ExecCommandHandlerOptions) -> Self {
        Self {
            options,
            lifetime: ExecCommandLifetime::Interactive,
        }
    }

    pub(crate) fn one_shot(options: ExecCommandHandlerOptions) -> Self {
        Self {
            options,
            lifetime: ExecCommandLifetime::OneShot,
        }
    }
}

impl ToolExecutor<ToolInvocation> for ExecCommandHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("exec_command")
    }

    fn spec(&self) -> ToolSpec {
        let spec = create_exec_command_tool_with_environment_id(
            CommandToolOptions {
                allow_login_shell: self.options.allow_login_shell,
                exec_permission_approvals_enabled: self.options.exec_permission_approvals_enabled,
            },
            self.options.include_environment_id,
            self.options.include_shell_parameter,
            self.options.include_windows_shell_guidance,
            self.options.include_task_evidence,
        );
        let mut spec = match self.lifetime {
            ExecCommandLifetime::Interactive => spec,
            ExecCommandLifetime::OneShot => one_shot_exec_command_spec(spec),
        };
        if !self.options.allow_tty
            && let ToolSpec::Function(spec) = &mut spec
        {
            spec.parameters
                .properties
                .get_or_insert_default()
                .remove("tty");
        }
        spec
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(self.handle_call(invocation))
    }
}

impl ExecCommandHandler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            step_context,
            cancellation_token,
            tracker,
            call_id,
            payload,
            ..
        } = invocation;

        let arguments = match payload {
            ToolPayload::Function { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "exec_command handler received unsupported payload".to_string(),
                ));
            }
        };

        let manager: &UnifiedExecProcessManager = &session.services.unified_exec_manager;
        let context = UnifiedExecContext::new(
            session.clone(),
            step_context.clone(),
            cancellation_token,
            call_id.clone(),
        );
        let environment_args: ExecCommandEnvironmentArgs = parse_arguments(&arguments)?;
        let Some(turn_environment) = resolve_tool_environment(
            &step_context.environments,
            environment_args.environment_id.as_deref(),
        )?
        else {
            return Err(FunctionCallError::RespondToModel(
                "unified exec is unavailable in this session".to_string(),
            ));
        };
        let native_environment_cwd = turn_environment.cwd().clone();
        let cwd = environment_args
            .workdir
            .as_deref()
            .filter(|workdir| !workdir.is_empty())
            .map_or_else(
                || Ok(native_environment_cwd.clone()),
                |workdir| native_environment_cwd.join(workdir),
            )
            .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;
        let environment = Arc::clone(&turn_environment.environment);
        let fs = environment.get_filesystem();

        // Remote executors enforce URI-native sandbox policy themselves. Only a host-local
        // sandbox needs a native cwd for resolving paths nested in the permissions config.
        let requires_host_native_cwd = !environment.is_remote()
            && SandboxManager::new().select_initial(
                turn_environment.permission_profile(),
                SandboxablePreference::Auto,
                codex_protocol::sandbox::effective_windows_sandbox_type(
                    turn_environment.config().windows_sandbox_type,
                    turn_environment.config().windows_sandbox_level,
                ),
                turn.network.is_some(),
            ) != SandboxType::None;
        // `to_abs_path()` alone cannot identify foreign drive paths: `file:///C:/repo` is
        // representable as `/C:/repo` on POSIX. Require the inferred convention to match too.
        let cwd_uses_native_convention =
            cwd.infer_path_convention() == Some(PathConvention::native());
        let native_cwd = match cwd.to_abs_path() {
            Ok(cwd) if cwd_uses_native_convention => Some(cwd),
            _ if !requires_host_native_cwd => None,
            Err(err) => return Err(FunctionCallError::RespondToModel(err.to_string())),
            Ok(_) => {
                return Err(FunctionCallError::RespondToModel(format!(
                    "path URI `{cwd}` does not use the host's native {} path convention",
                    PathConvention::native()
                )));
            }
        };
        let mut args: ExecCommandArgs = match native_cwd.as_ref() {
            Some(native_cwd) => {
                // The base path only resolves paths nested in the permissions config types.
                parse_arguments_with_base_path(&arguments, native_cwd)?
            }
            None => {
                // Foreign executor cwd values cannot seed this host's AbsolutePathBufGuard.
                // Sandbox intent and URI-native roots are still sent to the executor.
                parse_arguments(&arguments)?
            }
        };
        if args.tty && !session.features().enabled(Feature::UnifiedExecTty) {
            return Err(FunctionCallError::RespondToModel(
                "TTY execution is disabled by config; omit `tty` or set it to false.".to_string(),
            ));
        }
        let sandbox_permissions =
            resolve_sandbox_permissions(args.sandbox_permissions, args.justification.as_deref())?;
        let hook_command = args.cmd.clone();
        maybe_emit_implicit_skill_invocation(
            session.as_ref(),
            context.step_context.turn.as_ref(),
            &hook_command,
            &cwd,
            native_cwd.as_ref(),
            &turn_environment.selection.environment_id,
        )
        .await;
        let shell_mode =
            shell_mode_for_environment(&turn.unified_exec_shell_mode, environment.as_ref());
        // Remote environments may use a different OS and must build commands with their native
        // shell; fall back to the session shell when the environment did not report one.
        let shell = turn_environment
            .shell
            .clone()
            .map(Arc::new)
            .unwrap_or_else(|| session.user_shell());
        // TODO(anp): Resolve requested shells in remote environments instead of restricting
        // commands to the reported default shell.
        if environment.is_remote()
            && let Some(requested_shell) = args.shell.take()
        {
            let Some(remote_shell) = turn_environment.shell.as_ref() else {
                return Err(FunctionCallError::RespondToModel(format!(
                    "environment `{}` does not report a shell",
                    turn_environment.selection.environment_id
                )));
            };
            if detect_shell_type(Path::new(&requested_shell)) != Some(remote_shell.shell_type) {
                return Err(FunctionCallError::RespondToModel(format!(
                    "environment `{}` only supports `{}`",
                    turn_environment.selection.environment_id,
                    remote_shell.name()
                )));
            }
        }
        if args.login.is_none()
            && let Some(scope) = session.active_task_scope().await
            && session
                .rencrow_completion_state(&scope.root_task_id)
                .await
                .is_some_and(|completion| completion.common_harness.is_some())
        {
            args.login = Some(false);
        }
        let resolved_command = get_command(
            &args,
            shell,
            &shell_mode,
            turn_environment.config().allow_login_shell,
        )
        .map_err(FunctionCallError::RespondToModel)?;
        let command = resolved_command.command;
        let shell_type = resolved_command.shell_type;
        let verifier_direct_shell =
            matches!(&shell_mode, codex_tools::UnifiedExecShellMode::Direct)
                && args.shell.is_none()
                && !args
                    .login
                    .unwrap_or(turn_environment.config().allow_login_shell);
        let ExecCommandArgs {
            mut tty,
            yield_time_ms,
            timeout_ms,
            max_output_tokens,
            sandbox_permissions: _,
            additional_permissions,
            justification,
            prefix_rule,
            task_evidence,
            ..
        } = args;
        let mut completion_timeout = match self.lifetime {
            ExecCommandLifetime::Interactive => None,
            ExecCommandLifetime::OneShot => {
                tty = false;
                Some(Duration::from_millis(
                    timeout_ms.unwrap_or(DEFAULT_EXEC_COMMAND_TIMEOUT_MS),
                ))
            }
        };

        let exec_permission_approvals_enabled =
            session.features().enabled(Feature::ExecPermissionApprovals);
        let requested_additional_permissions = additional_permissions.clone();
        let sandbox_context =
            turn_environment.sandbox_context(/*additional_permissions*/ None);
        let permission_context = file_system_sandbox_policy_context_for_cwd(&sandbox_context, &cwd);
        let effective_additional_permissions = apply_granted_turn_permissions(
            context.session.as_ref(),
            turn_environment,
            &cwd,
            sandbox_permissions,
            additional_permissions,
        )
        .await;
        let additional_permissions_allowed = exec_permission_approvals_enabled
            || (session.features().enabled(Feature::RequestPermissionsTool)
                && effective_additional_permissions.permissions_preapproved);

        // Sticky turn permissions have already been approved, so they should
        // continue through the normal exec approval flow for the command.
        let approval_policy = context.step_context.settings.approval_policy();
        if effective_additional_permissions
            .sandbox_permissions
            .requests_sandbox_override()
            && !effective_additional_permissions.permissions_preapproved
            && prompt_is_rejected_by_policy(approval_policy, /*prompt_is_rule*/ false).is_some()
        {
            return Err(FunctionCallError::RespondToModel(format!(
                "approval policy is {approval_policy:?}; reject command — you cannot ask for escalated permissions if the approval policy is {approval_policy:?}"
            )));
        }

        let normalized_additional_permissions = match implicit_granted_permissions(
            sandbox_permissions,
            requested_additional_permissions.as_ref(),
            &effective_additional_permissions,
        )
        .map_or_else(
            || {
                normalize_and_validate_additional_permissions(
                    additional_permissions_allowed,
                    approval_policy,
                    effective_additional_permissions.sandbox_permissions,
                    effective_additional_permissions.additional_permissions,
                    effective_additional_permissions.permissions_preapproved,
                    &permission_context,
                )
            },
            |permissions| Ok(Some(permissions)),
        ) {
            Ok(normalized) => normalized,
            Err(err) => {
                return Err(FunctionCallError::RespondToModel(err));
            }
        };

        let intercepted_patch = intercept_apply_patch(
            &command,
            &cwd,
            fs.as_ref(),
            turn_environment.clone(),
            context.session.clone(),
            Arc::clone(&context.step_context),
            context.cancellation_token.clone(),
            Some(&tracker),
            &context.call_id,
            "exec_command",
        )
        .await;
        if let Some(output) = intercepted_patch? {
            return Ok(boxed_tool_output(ExecCommandToolOutput {
                event_call_id: String::new(),
                chunk_id: String::new(),
                wall_time: std::time::Duration::ZERO,
                raw_output: output.into_text().into_bytes(),
                truncation_policy: step_context.settings.model_info.truncation_policy.into(),
                max_output_tokens,
                process_id: None,
                exit_code: None,
                original_token_count: None,
                output_omitted_bytes: None,
                hook_command: None,
            }));
        }

        emit_unified_exec_tty_metric(&step_context.session_telemetry, tty);
        crate::tools::lifecycle::notify_command_start(
            context.session.as_ref(),
            context.step_context.turn.as_ref(),
            &context.call_id,
            &command,
            &cwd,
            fs.as_ref(),
        )
        .await;
        let has_task_evidence = task_evidence.is_some();
        let task_evidence_command = command.last().map(String::as_str).unwrap_or_default();
        let task_evidence_command_error =
            validate_task_evidence_check_command(task_evidence_command).err();
        let task_evidence_result = match task_evidence {
            Some(_request) if let Some(message) = task_evidence_command_error => {
                Err(FunctionCallError::RespondToModel(format!(
                    "{message}; remove shell wrappers such as `;`, pipes, or exit-status masking and run the check directly"
                )))
            }
            Some(_request) if environment.is_remote() => Err(FunctionCallError::RespondToModel(
                "task_evidence is supported only for local exec environments".to_string(),
            )),
            Some(request) => {
                let active_scope = session.active_task_scope().await;
                if !active_scope.as_ref().is_some_and(|scope| {
                    scope.task_id == request.task_id && scope.task_revision == request.task_revision
                }) {
                    Err(FunctionCallError::RespondToModel(
                        "task_evidence task ID and revision must match the active task scope"
                            .into(),
                    ))
                } else {
                    native_cwd
                        .as_ref()
                        .ok_or_else(|| {
                            FunctionCallError::RespondToModel(
                                "task_evidence requires a host-native local working directory"
                                    .to_string(),
                            )
                        })
                        .and_then(|workspace_root| {
                            let read_policy =
                                sandbox_context.permissions.file_system_sandbox_policy();
                            let read_deny =
                                codex_protocol::permissions::ReadDenyMatcher::try_new_for_local_paths(
                                    &read_policy,
                                    workspace_root.as_path(),
                                )
                                .map_err(FunctionCallError::RespondToModel)?;
                            if request.artifact_paths.iter().any(|path| {
                                read_deny.as_ref().is_some_and(|matcher| {
                                    matcher.is_local_path_read_denied(&workspace_root.join(path))
                                })
                            }) {
                                return Err(FunctionCallError::RespondToModel(
                                    "task_evidence artifact is denied by the active filesystem read policy"
                                        .to_string(),
                                ));
                            }
                            TaskCheckExecution::new(request, workspace_root.as_path())
                                .map(|mut execution| {
                                    execution.execution_environment_id = Some(
                                        turn_environment.selection.environment_id.clone(),
                                    );
                                    if turn.config.rencrow_autonomous_completion
                                        && let Some(scope) = active_scope.clone()
                                    {
                                        execution.bind_obligation(
                                            scope,
                                            task_evidence_command,
                                        );
                                    }
                                    Some(execution)
                                })
                                .ok_or_else(|| {
                                    FunctionCallError::RespondToModel(
                                        "task_evidence artifacts must be existing regular files inside the local workspace".to_string(),
                                    )
                                })
                        })
                }
            }
            None => Ok(None),
        };
        if has_task_evidence && task_evidence_result.is_err() {
            crate::rencrow_execution_guard::cancel_unexecuted_task_evidence_check(
                session.as_ref(),
                &call_id,
            )
            .await;
        }
        let task_evidence = task_evidence_result?;
        let common_harness_verifier = prepare_common_harness_verifier(
            session.as_ref(),
            turn.config.codex_home.as_path(),
            CommonHarnessVerifierRequest {
                call_id: &call_id,
                command: &hook_command,
                task_check_command: &command,
                task_evidence: task_evidence.as_ref(),
                is_remote: environment.is_remote(),
                workspace_root: native_cwd.as_deref(),
                verifier_direct_shell,
            },
        )
        .await?;
        if common_harness_verifier.is_some() {
            tty = false;
        }
        if let Some(verifier) = common_harness_verifier.as_ref() {
            let current = session
                .current_common_harness_state(&verifier.root_task_id)
                .await
                .map_err(FunctionCallError::RespondToModel)?;
            let claim = current
                .operation_refs
                .iter()
                .find(|record| record.intent.operation_id == verifier.intent.operation_id)
                .and_then(|record| record.claims.last())
                .ok_or_else(|| {
                    FunctionCallError::RespondToModel(
                        "common-harness verifier execution claim disappeared".into(),
                    )
                })?;
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|error| {
                    FunctionCallError::RespondToModel(format!("read verifier deadline: {error}"))
                })?
                .as_millis();
            if claim.owner_epoch != current.run_state.owner_epoch
                || claim.expected_state_version != verifier.intent.expected_state_version
                || current.run_state.target_version != verifier.intent.base_target_version
                || i128::from(claim.deadline_at_ms) <= now_ms as i128
            {
                return Err(FunctionCallError::RespondToModel(
                    "common-harness verifier claim, target, or deadline changed before execution"
                        .into(),
                ));
            }
            let remaining_ms = u64::try_from(claim.deadline_at_ms as i128 - now_ms as i128)
                .map_err(|_| {
                    FunctionCallError::RespondToModel(
                        "common-harness verifier deadline has expired".into(),
                    )
                })?;
            completion_timeout = Some(
                Duration::from_millis(remaining_ms)
                    .min(Duration::from_millis(verifier.check.timeout_ms)),
            );
        }
        // Preparation can be cancelled. Reserve a process only once it is done.
        let process_id = manager.allocate_process_id().await;
        let request = ExecCommandRequest {
            command,
            shell_type,
            hook_command: hook_command.clone(),
            process_id,
            yield_time_ms,
            max_output_tokens,
            cwd,
            sandbox_cwd: native_environment_cwd,
            turn_environment: turn_environment.clone(),
            shell_mode,
            network: context.step_context.turn.network.clone(),
            tty,
            sandbox_permissions: effective_additional_permissions.sandbox_permissions,
            additional_permissions: normalized_additional_permissions,
            additional_permissions_preapproved: effective_additional_permissions
                .permissions_preapproved,
            justification,
            prefix_rule,
            task_evidence,
        };
        let result = match completion_timeout {
            Some(timeout) => {
                UnifiedExecProcessManager::exec_command_to_completion(request, &context, timeout)
                    .await
            }
            None => manager.exec_command(request, &context).await,
        };
        match result {
            Ok(response) => {
                if let Some(verifier) = common_harness_verifier.as_ref() {
                    persist_common_harness_verifier_result(
                        session.as_ref(),
                        turn.config.codex_home.as_path(),
                        verifier,
                        &verifier.owner_guard,
                        &response,
                    )
                    .await
                    .map_err(FunctionCallError::RespondToModel)?;
                }
                Ok(boxed_tool_output(response))
            }
            Err(UnifiedExecError::SandboxDenied {
                output,
                original_token_count,
                output_omitted_bytes,
                ..
            }) => {
                let output_text = output.aggregated_output.text;
                let original_token_count =
                    original_token_count.unwrap_or_else(|| approx_token_count(&output_text));
                let output = ExecCommandToolOutput {
                    event_call_id: context.call_id.clone(),
                    chunk_id: generate_chunk_id(),
                    wall_time: output.duration,
                    raw_output: output_text.into_bytes(),
                    truncation_policy: step_context.settings.model_info.truncation_policy.into(),
                    max_output_tokens,
                    // Sandbox denial is terminal, so there is no live
                    // process for write_stdin to resume.
                    process_id: None,
                    exit_code: Some(output.exit_code),
                    original_token_count: Some(original_token_count),
                    output_omitted_bytes,
                    hook_command: Some(hook_command),
                };
                if let Some(verifier) = common_harness_verifier.as_ref() {
                    persist_common_harness_verifier_result(
                        session.as_ref(),
                        turn.config.codex_home.as_path(),
                        verifier,
                        &verifier.owner_guard,
                        &output,
                    )
                    .await
                    .map_err(FunctionCallError::RespondToModel)?;
                }
                Ok(boxed_tool_output(output))
            }
            Err(err) => {
                let message = format!("exec_command failed: {err:?}");
                if let Some(verifier) = common_harness_verifier.as_ref() {
                    persist_common_harness_verifier_unknown(
                        session.as_ref(),
                        turn.config.codex_home.as_path(),
                        verifier,
                        &verifier.owner_guard,
                        &message,
                    )
                    .await
                    .map_err(FunctionCallError::RespondToModel)?;
                }
                Err(FunctionCallError::RespondToModel(truncate_middle_chars(
                    &message,
                    EXEC_COMMAND_REJECTION_MAX_BYTES,
                )))
            }
        }
    }
}

struct CommonHarnessVerifierOperation {
    root_task_id: String,
    source_call_id: String,
    task_check_command: Vec<String>,
    condition: codex_history::task_context::ConditionSpecV1,
    check: codex_history::task_context::CheckSpecV1,
    task_check_execution: TaskCheckExecution,
    intent: codex_history::task_context::OperationIntentV1,
    execution_claim_id: String,
    expected_negative: Option<crate::task_check::ExpectedNegativeBindingV1>,
    workspace_root: std::path::PathBuf,
    target_paths: Vec<std::path::PathBuf>,
    artifact_binding: crate::common_harness_io::ArtifactBindingV1,
    owner_guard: tokio::sync::OwnedMutexGuard<()>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct CommonHarnessVerifierArtifactV1 {
    operation_id: String,
    task_id: String,
    request_revision: u64,
    condition: codex_history::task_context::ConditionSpecV1,
    normalized_argv: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct CommonHarnessVerifierResultArtifactV1 {
    operation_id: String,
    task_id: String,
    request_revision: u64,
    raw_output: Vec<u8>,
    output_omitted_bytes: Option<usize>,
    parsed_observation: Option<String>,
    parser_error: Option<String>,
    actual_after_version: Option<String>,
    after_artifact_set_ref: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct CommonHarnessVerifierUnknownArtifactV1 {
    operation_id: String,
    reason: String,
}

async fn persist_common_harness_verifier_unknown(
    session: &crate::session::session::Session,
    codex_home: &Path,
    verifier: &CommonHarnessVerifierOperation,
    guard: &tokio::sync::OwnedMutexGuard<()>,
    error: &str,
) -> Result<(), String> {
    let after_artifacts = crate::common_harness_io::ArtifactSetV1::capture_bound(
        &verifier.workspace_root,
        &verifier.target_paths,
        &verifier.artifact_binding,
    )
    .ok();
    let actual_after_version = after_artifacts
        .as_ref()
        .map(|artifacts| artifacts.digest.clone());
    let result_ref = write_operation_artifact(
        codex_home,
        session.thread_id,
        &CommonHarnessVerifierUnknownArtifactV1 {
            operation_id: verifier.intent.operation_id.clone(),
            reason: error.chars().take(4096).collect(),
        },
    )
    .ok();
    let current = session
        .current_common_harness_state(&verifier.root_task_id)
        .await?;
    let receipt = OperationReceiptV1 {
        operation_id: verifier.intent.operation_id.clone(),
        task_id: verifier.intent.task_id.clone(),
        request_revision: verifier.intent.request_revision,
        attempt_no: verifier.intent.attempt_no,
        owner_epoch: verifier.intent.owner_epoch,
        execution_id: verifier.execution_claim_id.clone(),
        outcome: OperationOutcomeV1::Unknown,
        actual_before_version: verifier.intent.base_target_version.clone(),
        actual_after_version,
        exit_status: None,
        result_ref,
        external_receipt_ref: None,
        reconciliation_ref: None,
    };
    session
        .apply_common_harness_host_event_guarded(
            &verifier.root_task_id,
            codex_history::task_context::StateFenceV1 {
                request_revision: current.request.request_revision,
                expected_state_version: current.run_state.state_version,
                owner_epoch: current.run_state.owner_epoch,
            },
            HostEventV1::FinalizeOperation { receipt },
            guard,
        )
        .await?;
    Ok(())
}

async fn persist_common_harness_verifier_result(
    session: &crate::session::session::Session,
    codex_home: &Path,
    verifier: &CommonHarnessVerifierOperation,
    guard: &tokio::sync::OwnedMutexGuard<()>,
    output: &ExecCommandToolOutput,
) -> Result<(), String> {
    let selection = verifier
        .check
        .go_test_selection
        .as_ref()
        .ok_or_else(|| "registered Go verifier has no selected case set".to_string())?;
    let expected_baseline_failure = match &verifier.check.expected_outcome {
        ExpectedCheckOutcomeV1::SpecifiedTestFailure { test_name, reason }
        | ExpectedCheckOutcomeV1::FailureIdentity { test_name, reason } => {
            Some(ExpectedBaselineFailure {
                case_name: test_name.clone(),
                reason: reason.clone(),
            })
        }
        ExpectedCheckOutcomeV1::TestCounts { .. }
        | ExpectedCheckOutcomeV1::AllSelectedTestsPass => None,
        _ => return Err("registered Go verifier selected an unsupported expected outcome".into()),
    };
    let expectation = GoTestExpectation {
        selected_cases: selection.selected_cases.clone(),
        expected_case_count: selection.expected_case_count,
        expected_baseline_failure,
    };
    let observation = if output.output_omitted_bytes.is_some() {
        Err(anyhow::anyhow!(
            "Go JSON output collection was capped before the complete event stream"
        ))
    } else {
        observe_go_test_json(
            &output.raw_output,
            output.exit_code.is_some(),
            output.exit_code,
            &expectation,
        )
    };
    let parsed_observation = observation
        .as_ref()
        .ok()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|error| format!("encode Go verifier observation: {error}"))?;
    let parser_error = observation.as_ref().err().map(ToString::to_string);
    let after_artifacts = crate::common_harness_io::ArtifactSetV1::capture_bound(
        &verifier.workspace_root,
        &verifier.target_paths,
        &verifier.artifact_binding,
    )
    .ok();
    let task_check_after = verifier
        .task_check_execution
        .hash_artifacts()
        .unwrap_or_default();
    let actual_after_version = after_artifacts
        .as_ref()
        .map(|artifacts| artifacts.digest.clone());
    let after_artifact_set_ref = after_artifacts
        .as_ref()
        .map(|artifacts| write_operation_artifact(codex_home, session.thread_id, artifacts))
        .transpose();
    let persisted_after_artifact_ref = after_artifact_set_ref
        .as_ref()
        .ok()
        .and_then(Option::as_ref)
        .cloned();
    let after_artifact_error = after_artifact_set_ref
        .as_ref()
        .err()
        .map(ToString::to_string);
    let target_stable = after_artifacts.as_ref().is_some_and(|artifacts| {
        artifacts.matches_workspace().unwrap_or(false)
            && artifacts.digest == verifier.intent.base_target_version
    }) && task_check_after == verifier.task_check_execution.before;
    let result_artifact = write_operation_artifact(
        codex_home,
        session.thread_id,
        &CommonHarnessVerifierResultArtifactV1 {
            operation_id: verifier.intent.operation_id.clone(),
            task_id: verifier.intent.task_id.clone(),
            request_revision: verifier.intent.request_revision,
            raw_output: output.raw_output.clone(),
            output_omitted_bytes: output.output_omitted_bytes.map(std::num::NonZeroUsize::get),
            parsed_observation,
            parser_error,
            actual_after_version: actual_after_version.clone(),
            after_artifact_set_ref: persisted_after_artifact_ref.clone(),
        },
    );
    let result_ref = match result_artifact {
        Ok(result_ref) => result_ref,
        Err(error) => {
            let current = session
                .current_common_harness_state(&verifier.root_task_id)
                .await?;
            let receipt = OperationReceiptV1 {
                operation_id: verifier.intent.operation_id.clone(),
                task_id: verifier.intent.task_id.clone(),
                request_revision: verifier.intent.request_revision,
                attempt_no: verifier.intent.attempt_no,
                owner_epoch: verifier.intent.owner_epoch,
                execution_id: verifier.execution_claim_id.clone(),
                outcome: OperationOutcomeV1::Unknown,
                actual_before_version: verifier.intent.base_target_version.clone(),
                actual_after_version: actual_after_version.clone(),
                exit_status: output.exit_code,
                result_ref: None,
                external_receipt_ref: None,
                reconciliation_ref: None,
            };
            session
                .apply_common_harness_host_event_guarded(
                    &verifier.root_task_id,
                    codex_history::task_context::StateFenceV1 {
                        request_revision: current.request.request_revision,
                        expected_state_version: current.run_state.state_version,
                        owner_epoch: current.run_state.owner_epoch,
                    },
                    HostEventV1::FinalizeOperation { receipt },
                    guard,
                )
                .await?;
            return Err(format!(
                "persist completed Go verifier output artifact; operation was finalized unknown: {error}"
            ));
        }
    };
    let current = session
        .current_common_harness_state(&verifier.root_task_id)
        .await?;
    let outcome = match (target_stable, output.exit_code) {
        (false, _) | (true, None) => OperationOutcomeV1::Unknown,
        (true, Some(0)) => OperationOutcomeV1::Succeeded,
        (true, Some(_)) => OperationOutcomeV1::Failed,
    };
    let receipt = OperationReceiptV1 {
        operation_id: verifier.intent.operation_id.clone(),
        task_id: verifier.intent.task_id.clone(),
        request_revision: verifier.intent.request_revision,
        attempt_no: verifier.intent.attempt_no,
        owner_epoch: verifier.intent.owner_epoch,
        execution_id: verifier.execution_claim_id.clone(),
        outcome,
        actual_before_version: verifier.intent.base_target_version.clone(),
        actual_after_version: actual_after_version.clone(),
        exit_status: output.exit_code,
        result_ref: Some(result_ref.clone()),
        external_receipt_ref: None,
        reconciliation_ref: None,
    };
    let finalized = session
        .apply_common_harness_host_event_guarded(
            &verifier.root_task_id,
            codex_history::task_context::StateFenceV1 {
                request_revision: current.request.request_revision,
                expected_state_version: current.run_state.state_version,
                owner_epoch: current.run_state.owner_epoch,
            },
            HostEventV1::FinalizeOperation { receipt },
            guard,
        )
        .await?;
    let task_check_output = String::from_utf8_lossy(&output.raw_output);
    let completed_task_check = output.exit_code.is_some_and(|exit_code| exit_code != -1);
    session
        .finish_common_harness_task_check_guarded(
            guard,
            &verifier.source_call_id,
            &verifier.task_check_execution,
            crate::task_check::TaskCheckCompletion {
                command: &verifier.task_check_command,
                cwd: &verifier.workspace_root,
                output: &task_check_output,
                exit_code: output.exit_code.unwrap_or(-1),
                completed: completed_task_check,
                output_truncated: output.output_omitted_bytes.is_some(),
                after: task_check_after,
            },
        )
        .await?;
    if outcome == OperationOutcomeV1::Unknown {
        return Err("registered Go verifier ended without a complete raw exit receipt".into());
    }
    if !target_stable {
        return Err(
            "workspace target changed during common-harness verification; result is blocked".into(),
        );
    }
    let observation =
        observation.map_err(|error| format!("observe registered Go verifier output: {error}"))?;
    let mut task_check_evidence_ref = None;
    let host_negative_proof = if after_artifact_error.is_some() {
        // A missing durable artifact snapshot is a verifier error. In particular, it
        // cannot resolve the raw expected-negative reservation for a baseline run.
        None
    } else if let Some(binding) = verifier.expected_negative.as_ref() {
        if observation.verdict == crate::common_harness_io::GoTestVerdict::Error {
            // A complete Go framework/compiler failure is a known ERROR observation,
            // not the expected failed test case and never a negative proof.
            None
        } else {
            if output.exit_code != Some(1) || !observation.matched_expected_baseline_failure {
                return Err(
                    "expected-negative Go verifier did not match the registered baseline failure"
                        .into(),
                );
            }
            let identity = binding.expected_failure_identity.clone();
            let raw_output_sha256 = format!("{:x}", Sha256::digest(&output.raw_output));
            let (_receipt, evidence_ref) = session
                .resolve_common_harness_expected_negative_guarded(
                    &verifier.owner_guard,
                    binding,
                    identity.clone(),
                    raw_output_sha256,
                )
                .await?;
            task_check_evidence_ref = Some(evidence_ref);
            Some(HostNegativeProofV1 {
                baseline_receipt_ref: verifier.intent.operation_id.clone(),
                expected_failure_identity: identity.clone(),
                observed_failure_identity: identity,
                resolution_receipt_ref: None,
            })
        }
    } else {
        None
    };
    let verdict = if after_artifact_error.is_some() {
        ConditionVerdictV1::Error
    } else if host_negative_proof.is_some() {
        // The adapter accepts this raw failure only because it matches the
        // registered baseline identity. The reproduction condition remains
        // unmet until a passing candidate observation is paired with it.
        ConditionVerdictV1::Fail
    } else {
        match observation.verdict {
            crate::common_harness_io::GoTestVerdict::Pass => ConditionVerdictV1::Pass,
            crate::common_harness_io::GoTestVerdict::Fail => ConditionVerdictV1::Fail,
            crate::common_harness_io::GoTestVerdict::Skip
            | crate::common_harness_io::GoTestVerdict::Error => ConditionVerdictV1::Error,
        }
    };
    let observed = if let Some(error) = after_artifact_error.as_ref() {
        serde_json::json!({
            "go_observation": &observation,
            "artifact_snapshot_error": error,
        })
        .to_string()
    } else {
        serde_json::to_string(&observation)
            .map_err(|error| format!("encode Go verifier observation: {error}"))?
    };
    let evidence = ConditionEvidenceV1 {
        evidence_id: format!(
            "evidence:{}:{}",
            verifier.intent.operation_id, verifier.condition.condition_id
        ),
        condition_id: verifier.condition.condition_id.clone(),
        condition_revision: verifier.condition.condition_revision,
        condition_set_revision: finalized.request.condition_set_revision,
        request_revision: verifier.intent.request_revision,
        target_version: verifier.intent.base_target_version.clone(),
        comparison_scope_digest: format!(
            "sha256:{:x}",
            Sha256::digest(verifier.condition.comparison_scope.as_bytes())
        ),
        execution_id: verifier.execution_claim_id.clone(),
        operation_id: verifier.intent.operation_id.clone(),
        verifier_ref: verifier
            .condition
            .verifier_ref
            .clone()
            .unwrap_or_else(|| "codex-common-harness/go-test-json/1".into()),
        verifier_config_revision: verifier.condition.verifier_config_revision.unwrap_or(1),
        observed,
        verdict,
        log_refs: std::iter::once(result_ref.clone())
            .chain(task_check_evidence_ref)
            .collect(),
        artifact_refs: persisted_after_artifact_ref.into_iter().collect(),
        source_evidence_ref: None,
        comparison_proof_ref: None,
        host_negative_proof,
        lifecycle: EvidenceLifecycleV1::Current,
    };
    let evidence_state = session
        .apply_common_harness_host_event_guarded(
            &verifier.root_task_id,
            codex_history::task_context::StateFenceV1 {
                request_revision: finalized.request.request_revision,
                expected_state_version: finalized.run_state.state_version,
                owner_epoch: finalized.run_state.owner_epoch,
            },
            HostEventV1::RecordEvidence {
                evidence: evidence.clone(),
            },
            guard,
        )
        .await?;
    if verdict == ConditionVerdictV1::Pass
        && verifier.condition.kind == codex_history::task_context::ConditionKindV1::TestSuite
    {
        let reproduction = evidence_state.request.conditions.iter().find(|candidate| {
            candidate.required
                && candidate.kind == codex_history::task_context::ConditionKindV1::ReproduceFailure
        });
        if let Some(reproduction) = reproduction {
            let completed_reproduction_already_exists =
                evidence_state.evidence_refs.iter().any(|prior| {
                    prior.condition_id == reproduction.condition_id
                        && prior.condition_revision == reproduction.condition_revision
                        && prior.target_version == evidence_state.run_state.target_version
                        && prior.lifecycle == EvidenceLifecycleV1::Current
                        && prior
                            .host_negative_proof
                            .as_ref()
                            .is_some_and(|proof| proof.resolution_receipt_ref.is_some())
                });
            if !completed_reproduction_already_exists {
                let baseline = evidence_state.evidence_refs.iter().rev().find_map(|prior| {
                    let proof = prior.host_negative_proof.as_ref()?;
                    (prior.condition_id == reproduction.condition_id
                        && prior.condition_revision == reproduction.condition_revision
                        && prior.target_version == evidence_state.request.start_target_version
                        && proof.resolution_receipt_ref.is_none())
                    .then_some((prior, proof))
                });
                if let Some((baseline, baseline_proof)) = baseline {
                    let baseline_is_failed = evidence_state.operation_refs.iter().any(|record| {
                        record.intent.operation_id == baseline_proof.baseline_receipt_ref
                            && record.receipt.as_ref().is_some_and(|receipt| {
                                receipt.outcome == OperationOutcomeV1::Failed
                            })
                    });
                    if baseline_is_failed
                        && suite_candidate_matches_reproduction(
                            reproduction,
                            &verifier.condition,
                            &observation,
                            baseline,
                            &evidence,
                            &evidence_state.request.start_target_version,
                        )
                    {
                        let resolved_proof = HostNegativeProofV1 {
                            baseline_receipt_ref: baseline_proof.baseline_receipt_ref.clone(),
                            expected_failure_identity: baseline_proof
                                .expected_failure_identity
                                .clone(),
                            observed_failure_identity: baseline_proof
                                .observed_failure_identity
                                .clone(),
                            resolution_receipt_ref: Some(verifier.intent.operation_id.clone()),
                        };
                        let candidate_evidence = ConditionEvidenceV1 {
                            evidence_id: format!(
                                "evidence:{}:reproduction:{}",
                                verifier.intent.operation_id, reproduction.condition_id
                            ),
                            condition_id: reproduction.condition_id.clone(),
                            condition_revision: reproduction.condition_revision,
                            condition_set_revision: evidence_state.request.condition_set_revision,
                            request_revision: verifier.intent.request_revision,
                            target_version: evidence_state.run_state.target_version.clone(),
                            comparison_scope_digest: format!(
                                "sha256:{:x}",
                                Sha256::digest(reproduction.comparison_scope.as_bytes())
                            ),
                            execution_id: verifier.execution_claim_id.clone(),
                            operation_id: verifier.intent.operation_id.clone(),
                            verifier_ref: reproduction
                                .verifier_ref
                                .clone()
                                .unwrap_or_else(|| "codex-common-harness/go-test-json/1".into()),
                            verifier_config_revision: reproduction
                                .verifier_config_revision
                                .unwrap_or(1),
                            observed: serde_json::json!({
                                "baseline_evidence_id": baseline.evidence_id,
                                "candidate_suite_evidence_id": evidence.evidence_id,
                                "candidate_observation": observation,
                            })
                            .to_string(),
                            verdict: ConditionVerdictV1::Pass,
                            log_refs: baseline
                                .log_refs
                                .iter()
                                .cloned()
                                .chain(evidence.log_refs.iter().cloned())
                                .collect(),
                            artifact_refs: evidence.artifact_refs.clone(),
                            source_evidence_ref: None,
                            comparison_proof_ref: None,
                            host_negative_proof: Some(resolved_proof),
                            lifecycle: EvidenceLifecycleV1::Current,
                        };
                        session
                            .apply_common_harness_host_event_guarded(
                                &verifier.root_task_id,
                                codex_history::task_context::StateFenceV1 {
                                    request_revision: evidence_state.request.request_revision,
                                    expected_state_version: evidence_state.run_state.state_version,
                                    owner_epoch: evidence_state.run_state.owner_epoch,
                                },
                                HostEventV1::RecordEvidence {
                                    evidence: candidate_evidence,
                                },
                                guard,
                            )
                            .await?;
                    }
                }
            }
        }
    }
    Ok(())
}

struct CommonHarnessVerifierRequest<'a> {
    call_id: &'a str,
    command: &'a str,
    task_check_command: &'a [String],
    task_evidence: Option<&'a TaskCheckExecution>,
    is_remote: bool,
    workspace_root: Option<&'a Path>,
    verifier_direct_shell: bool,
}

async fn prepare_common_harness_verifier(
    session: &crate::session::session::Session,
    codex_home: &Path,
    request: CommonHarnessVerifierRequest<'_>,
) -> Result<Option<CommonHarnessVerifierOperation>, FunctionCallError> {
    let CommonHarnessVerifierRequest {
        call_id,
        command,
        task_check_command,
        task_evidence,
        is_remote,
        workspace_root,
        verifier_direct_shell,
    } = request;
    let Some(scope) = session.active_task_scope().await else {
        return Ok(None);
    };
    let Some(mut state) = session
        .rencrow_completion_state(&scope.root_task_id)
        .await
        .and_then(|completion| completion.common_harness)
    else {
        return Ok(None);
    };
    if is_remote {
        return Err(FunctionCallError::RespondToModel(
            "common-harness v1 verification requires a local workspace".into(),
        ));
    }
    if !verifier_direct_shell {
        return Err(FunctionCallError::RespondToModel(
            "common-harness v1 verification requires the direct host shell mode without login startup".into(),
        ));
    }
    let evidence = task_evidence.ok_or_else(|| {
        FunctionCallError::RespondToModel(
            "common-harness v1 verification requires host task_evidence binding".into(),
        )
    })?;
    if evidence.request.task_id != scope.task_id
        || evidence.request.task_revision != scope.task_revision
    {
        return Err(FunctionCallError::RespondToModel(
            "common-harness v1 verification request does not match the active task".into(),
        ));
    }
    let workspace_root = workspace_root.ok_or_else(|| {
        FunctionCallError::RespondToModel(
            "common-harness v1 verification requires a host-native cwd".into(),
        )
    })?;
    let workspace_root = workspace_root.canonicalize().map_err(|error| {
        FunctionCallError::RespondToModel(format!("resolve verifier cwd: {error}"))
    })?;
    let dependency_paths = state
        .request
        .conditions
        .iter()
        .flat_map(|condition| {
            condition.dependency_refs.iter().chain(
                condition
                    .check
                    .iter()
                    .flat_map(|check| check.cwd_binding_refs.iter()),
            )
        })
        .map(std::path::PathBuf::from)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let binding = crate::common_harness_io::ArtifactBindingV1::for_canonical_root(&workspace_root)
        .map_err(|error| {
            FunctionCallError::RespondToModel(format!("bind verifier workspace: {error}"))
        })?;
    if binding.workspace_binding_ref != state.request.workspace_ref {
        return Err(FunctionCallError::RespondToModel(
            "common-harness v1 workspace binding changed".into(),
        ));
    }
    let artifacts = crate::common_harness_io::ArtifactSetV1::capture_bound(
        &workspace_root,
        &dependency_paths,
        &binding,
    )
    .map_err(|error| {
        FunctionCallError::RespondToModel(format!("capture verifier target: {error}"))
    })?;
    if !artifacts.matches_workspace().map_err(|error| {
        FunctionCallError::RespondToModel(format!("recheck verifier target: {error}"))
    })? || artifacts.digest != state.run_state.target_version
    {
        return Err(FunctionCallError::RespondToModel(
            "common-harness v1 verifier target changed since the host checkpoint".into(),
        ));
    }
    let check_args = shlex::split(command).ok_or_else(|| {
        FunctionCallError::RespondToModel(
            "common-harness v1 verifier command is not direct argv".into(),
        )
    })?;
    let path_variable = std::env::var_os("PATH")
        .ok_or_else(|| FunctionCallError::RespondToModel("host Go PATH is unavailable".into()))?;
    let executable = std::env::split_paths(&path_variable)
        .map(|directory| directory.join(if cfg!(windows) { "go.exe" } else { "go" }))
        .find(|path| path.is_file())
        .and_then(|path| path.canonicalize().ok())
        .ok_or_else(|| {
            FunctionCallError::RespondToModel("host Go executable is unavailable".into())
        })?;
    let mut actual_argv = check_args;
    if let Some(first) = actual_argv.first_mut() {
        if first == "go" {
            *first = executable.to_string_lossy().into_owned();
        } else if Path::new(first).canonicalize().ok().as_deref() != Some(executable.as_path()) {
            return Err(FunctionCallError::RespondToModel(
                "common-harness v1 verifier must use the host Go executable".into(),
            ));
        }
    }
    let condition = select_common_harness_condition(&state, &actual_argv)
        .map_err(FunctionCallError::RespondToModel)?;
    if condition.verifier_ref.as_deref() != Some("codex-common-harness/go-test-json/1")
        || condition.verifier_config_revision != Some(1)
    {
        return Err(FunctionCallError::RespondToModel(
            "registered common-harness verifier adapter is unsupported".into(),
        ));
    }
    let check = condition.check.clone().ok_or_else(|| {
        FunctionCallError::RespondToModel("registered common-harness verifier is incomplete".into())
    })?;
    if check.parser != CheckOutputParserV1::GoTestJson
        || !check.allowed_environment_refs.is_empty()
        || check.go_test_selection.is_none()
    {
        return Err(FunctionCallError::RespondToModel(
            "registered common-harness Go verifier has unsupported bindings".into(),
        ));
    }
    let expected_negative =
        if condition.kind == codex_history::task_context::ConditionKindV1::ReproduceFailure {
            let expected_failure_identity = match &check.expected_outcome {
                ExpectedCheckOutcomeV1::SpecifiedTestFailure { test_name, reason }
                | ExpectedCheckOutcomeV1::FailureIdentity { test_name, reason } => {
                    format!("{test_name}: {reason}")
                }
                _ => {
                    return Err(FunctionCallError::RespondToModel(
                        "reproduce_failure requires a specified Go test failure identity".into(),
                    ));
                }
            };
            let obligation = evidence.obligation.clone().ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "expected-negative verification has no host task-check obligation".into(),
                )
            })?;
            let expected = serde_json::to_vec(&check.expected_outcome)
                .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
            Some(crate::task_check::ExpectedNegativeBindingV1 {
                source_call_id: call_id.to_string(),
                obligation,
                condition_id: condition.condition_id.clone(),
                condition_revision: condition.condition_revision,
                condition_set_revision: state.request.condition_set_revision,
                baseline_version: state.run_state.target_version.clone(),
                expected_outcome_digest: format!("{:x}", Sha256::digest(expected)),
                expected_failure_identity,
            })
        } else {
            None
        };
    let mut artifact_paths = evidence
        .request
        .artifact_paths
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    artifact_paths.sort();
    let mut cwd_binding_refs = check.cwd_binding_refs.clone();
    cwd_binding_refs.sort();
    if artifact_paths != cwd_binding_refs {
        return Err(FunctionCallError::RespondToModel(
            "task_evidence paths do not match the registered verifier inputs".into(),
        ));
    }
    if state.run_state.status == TaskRunStatusV1::Queued {
        state = session
            .ensure_common_harness_attempt(&scope.root_task_id)
            .await
            .map_err(FunctionCallError::RespondToModel)?;
    }
    let is_baseline_reproduction = condition.kind
        == codex_history::task_context::ConditionKindV1::ReproduceFailure
        && state.run_state.target_version == state.request.start_target_version;
    if state.run_state.status == TaskRunStatusV1::Running && !is_baseline_reproduction {
        state = session
            .apply_common_harness_host_event(
                &scope.root_task_id,
                codex_history::task_context::StateFenceV1 {
                    request_revision: state.request.request_revision,
                    expected_state_version: state.run_state.state_version,
                    owner_epoch: state.run_state.owner_epoch,
                },
                HostEventV1::BeginVerification {
                    target_version: state.run_state.target_version.clone(),
                },
            )
            .await
            .map_err(FunctionCallError::RespondToModel)?;
    }
    if state.run_state.status != TaskRunStatusV1::Verifying
        && !(is_baseline_reproduction && state.run_state.status == TaskRunStatusV1::Running)
    {
        return Err(FunctionCallError::RespondToModel(
            "common-harness v1 task is not ready for verification".into(),
        ));
    }
    let owner_guard = session.completion_owner_guard().await;
    state = session
        .current_common_harness_state(&scope.root_task_id)
        .await
        .map_err(FunctionCallError::RespondToModel)?;
    if (state.run_state.status != TaskRunStatusV1::Verifying
        && !(is_baseline_reproduction && state.run_state.status == TaskRunStatusV1::Running))
        || state.run_state.target_version != artifacts.digest
    {
        return Err(FunctionCallError::RespondToModel(
            "common-harness verifier state changed before reservation".into(),
        ));
    }
    session
        .begin_common_harness_task_check_guarded(
            &owner_guard,
            call_id,
            evidence,
            expected_negative.clone(),
        )
        .await
        .map_err(FunctionCallError::RespondToModel)?;
    let operation_id = format!("verify:{call_id}");
    let artifact_ref = write_operation_artifact(
        codex_home,
        session.thread_id,
        &CommonHarnessVerifierArtifactV1 {
            operation_id: operation_id.clone(),
            task_id: state.request.task_id.clone(),
            request_revision: state.request.request_revision,
            condition: condition.clone(),
            normalized_argv: actual_argv,
        },
    )
    .map_err(FunctionCallError::RespondToModel)?;
    let (intent, execution_claim_id) = session
        .reserve_common_harness_operation_guarded(
            &scope.root_task_id,
            crate::session::common_harness::NativeOperationDefinition {
                operation_id,
                operation_kind: OperationKindV1::RunVerifier,
                normalized_arguments: command.as_bytes(),
                normalized_arguments_ref: artifact_ref,
                permission_decision_ref: state.request.permission_snapshot_ref.clone(),
                executor_ref: check.toolchain_ref.clone().unwrap_or_default(),
                timeout_ms: check.timeout_ms,
            },
            &owner_guard,
        )
        .await
        .map_err(FunctionCallError::RespondToModel)?;
    let persisted: CommonHarnessVerifierArtifactV1 = load_operation_artifact(
        codex_home,
        session.thread_id,
        &intent.normalized_arguments_ref,
    )
    .map_err(FunctionCallError::RespondToModel)?;
    if persisted.operation_id != intent.operation_id
        || persisted.task_id != intent.task_id
        || persisted.request_revision != intent.request_revision
        || persisted.condition != condition
        || persisted.normalized_argv != check.argv
    {
        return Err(FunctionCallError::RespondToModel(
            "durable common-harness verifier intent does not match the registered check".into(),
        ));
    }
    let reserved_state = session
        .current_common_harness_state(&scope.root_task_id)
        .await
        .map_err(FunctionCallError::RespondToModel)?;
    let has_persisted_execution_claim = reserved_state
        .operation_refs
        .iter()
        .find(|record| record.intent.operation_id == intent.operation_id)
        .is_some_and(|record| {
            record
                .claims
                .iter()
                .any(|claim| claim.claim_id == execution_claim_id)
        });
    if !has_persisted_execution_claim {
        return Err(FunctionCallError::RespondToModel(
            "common-harness verifier execution claim disappeared after reservation".into(),
        ));
    }
    Ok(Some(CommonHarnessVerifierOperation {
        root_task_id: scope.root_task_id,
        source_call_id: call_id.to_string(),
        task_check_command: task_check_command.to_vec(),
        condition,
        check,
        task_check_execution: evidence.clone(),
        intent,
        execution_claim_id,
        expected_negative,
        workspace_root,
        target_paths: dependency_paths,
        artifact_binding: binding,
        owner_guard,
    }))
}

fn select_common_harness_condition(
    state: &CommonHarnessStateV1,
    actual_argv: &[String],
) -> Result<ConditionSpecV1, String> {
    let matching = state
        .request
        .conditions
        .iter()
        .filter(|condition| {
            condition
                .check
                .as_ref()
                .is_some_and(|check| check.argv.as_slice() == actual_argv)
        })
        .collect::<Vec<_>>();
    match matching.as_slice() {
        [condition] => return Ok((**condition).clone()),
        [] => {
            return Err("command does not match a registered common-harness verifier".into());
        }
        _ => {}
    }

    let reproductions = matching
        .iter()
        .filter(|condition| condition.kind == ConditionKindV1::ReproduceFailure)
        .copied()
        .collect::<Vec<_>>();
    let suites = matching
        .iter()
        .filter(|condition| condition.kind == ConditionKindV1::TestSuite)
        .copied()
        .collect::<Vec<_>>();
    let [reproduction] = reproductions.as_slice() else {
        return Err(
            "argv matches multiple common-harness conditions without a unique reproduction phase"
                .into(),
        );
    };
    let [suite] = suites.as_slice() else {
        return Err(
            "argv matches multiple common-harness conditions without a unique suite phase".into(),
        );
    };
    if matching.len() != 2 {
        return Err("argv matches additional common-harness conditions and is ambiguous".into());
    }

    let confirmed_baseline_failure = state.evidence_refs.iter().any(|evidence| {
        evidence.condition_id == reproduction.condition_id
            && evidence.condition_revision == reproduction.condition_revision
            && evidence.condition_set_revision == state.request.condition_set_revision
            && evidence.request_revision == state.request.request_revision
            && evidence.target_version == state.request.start_target_version
            && evidence.host_negative_proof.as_ref().is_some_and(|proof| {
                proof.resolution_receipt_ref.is_none()
                    && state.operation_refs.iter().any(|operation| {
                        operation.intent.operation_id == proof.baseline_receipt_ref
                            && operation.receipt.as_ref().is_some_and(|receipt| {
                                receipt.outcome == OperationOutcomeV1::Failed
                            })
                    })
            })
    });

    if state.run_state.target_version == state.request.start_target_version
        && !confirmed_baseline_failure
    {
        Ok((**reproduction).clone())
    } else {
        Ok((**suite).clone())
    }
}

fn suite_candidate_matches_reproduction(
    reproduction: &ConditionSpecV1,
    suite: &ConditionSpecV1,
    observation: &GoTestObservation,
    baseline: &ConditionEvidenceV1,
    candidate: &ConditionEvidenceV1,
    start_target_version: &str,
) -> bool {
    let Some(reproduction_check) = reproduction.check.as_ref() else {
        return false;
    };
    let Some(suite_check) = suite.check.as_ref() else {
        return false;
    };
    let (failure_test, failure_reason) = match &reproduction_check.expected_outcome {
        ExpectedCheckOutcomeV1::SpecifiedTestFailure { test_name, reason }
        | ExpectedCheckOutcomeV1::FailureIdentity { test_name, reason } => (test_name, reason),
        _ => return false,
    };
    let Some(reproduction_selection) = reproduction_check.go_test_selection.as_ref() else {
        return false;
    };
    let Some(suite_selection) = suite_check.go_test_selection.as_ref() else {
        return false;
    };
    let expected_failure_identity = format!("{failure_test}: {failure_reason}");

    reproduction.kind == ConditionKindV1::ReproduceFailure
        && suite.kind == ConditionKindV1::TestSuite
        && reproduction.verifier_ref == suite.verifier_ref
        && reproduction.verifier_config_revision == suite.verifier_config_revision
        && reproduction_check.parser == suite_check.parser
        && reproduction_check.toolchain_ref == suite_check.toolchain_ref
        && reproduction_selection.selected_cases.contains(failure_test)
        && suite_selection.selected_cases.contains(failure_test)
        && observation.selected_names.contains(failure_test)
        && matches!(
            &suite_check.expected_outcome,
            ExpectedCheckOutcomeV1::TestCounts { .. }
                | ExpectedCheckOutcomeV1::AllSelectedTestsPass
        )
        && baseline.condition_id == reproduction.condition_id
        && baseline.condition_revision == reproduction.condition_revision
        && baseline.request_revision == candidate.request_revision
        && baseline.target_version == start_target_version
        && baseline.host_negative_proof.as_ref().is_some_and(|proof| {
            proof.resolution_receipt_ref.is_none()
                && proof.expected_failure_identity == expected_failure_identity
                && proof.observed_failure_identity == expected_failure_identity
        })
        && candidate.condition_id == suite.condition_id
        && candidate.condition_revision == suite.condition_revision
        && candidate.verdict == ConditionVerdictV1::Pass
        && candidate.lifecycle == EvidenceLifecycleV1::Current
        && candidate.target_version != start_target_version
        && candidate.operation_id != baseline.operation_id
}

pub(crate) fn validate_task_evidence_check_command(command: &str) -> Result<(), &'static str> {
    if crate::rencrow_execution_guard::is_task_evidence_check_command(command) {
        Ok(())
    } else {
        Err("task_evidence requires a direct recognized check command")
    }
}

fn one_shot_exec_command_spec(spec: ToolSpec) -> ToolSpec {
    let ToolSpec::Function(mut spec) = spec else {
        unreachable!("exec_command has a function schema");
    };
    spec.description = spec.description.replacen(
        "Runs a command in a PTY, returning output or a session ID for ongoing interaction.",
        "Runs a command to completion and returns its output. The process is terminated on timeout or cancellation and cannot be resumed.",
        1,
    );
    let properties = spec.parameters.properties.get_or_insert_default();
    properties.remove("tty");
    properties.remove("yield_time_ms");
    properties.insert(
        "timeout_ms".to_string(),
        JsonSchema::number(Some(
            "Maximum command runtime. Defaults to 10000 ms.".to_string(),
        )),
    );
    spec.output_schema = spec.output_schema.map(|schema| {
        let mut schema = schema.into_value();
        if let Some(output_properties) = schema
            .get_mut("properties")
            .and_then(serde_json::Value::as_object_mut)
        {
            output_properties.remove("session_id");
        }
        schema.into()
    });
    ToolSpec::Function(spec)
}

impl CoreToolRuntime for ExecCommandHandler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }

    fn pre_tool_use_payload(&self, invocation: &ToolInvocation) -> Option<PreToolUsePayload> {
        let ToolPayload::Function { arguments } = &invocation.payload else {
            return None;
        };

        parse_arguments::<ExecCommandArgs>(arguments)
            .ok()
            .map(|args| PreToolUsePayload {
                tool_name: HookToolName::bash(),
                tool_input: serde_json::json!({ "command": args.cmd }),
            })
    }

    fn with_updated_hook_input(
        &self,
        mut invocation: ToolInvocation,
        updated_input: serde_json::Value,
    ) -> Result<ToolInvocation, FunctionCallError> {
        let ToolPayload::Function { arguments } = invocation.payload else {
            return Err(FunctionCallError::RespondToModel(
                "hook input rewrite received unsupported exec_command payload".to_string(),
            ));
        };
        invocation.payload = ToolPayload::Function {
            arguments: rewrite_function_string_argument(
                &arguments,
                "exec_command",
                "cmd",
                updated_hook_command(&updated_input)?,
            )?,
        };
        Ok(invocation)
    }

    fn post_tool_use_payload(
        &self,
        invocation: &ToolInvocation,
        result: &dyn crate::tools::context::ToolOutput,
    ) -> Option<PostToolUsePayload> {
        post_unified_exec_tool_use_payload(invocation, result)
    }
}

fn emit_unified_exec_tty_metric(session_telemetry: &SessionTelemetry, tty: bool) {
    session_telemetry.counter(
        TOOL_CALL_UNIFIED_EXEC_METRIC,
        /*inc*/ 1,
        &[("tty", if tty { "true" } else { "false" })],
    );
}
