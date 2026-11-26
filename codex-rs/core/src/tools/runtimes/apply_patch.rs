// Modified by RenCrow Switch Core, 2026-11-27: trace controlled native patch interruption boundaries.
//! Apply Patch runtime: executes verified patches under the orchestrator.
//!
//! Assumes `apply_patch` verification/approval happened upstream. Reuses the
//! selected turn environment filesystem for both local and remote turns, with
//! sandboxing enforced by the explicit filesystem sandbox context.
use crate::common_harness_artifacts::load_operation_artifact;
use crate::common_harness_artifacts::write_operation_artifact;
use crate::common_harness_io::ArtifactBindingV1;
use crate::common_harness_io::PatchJournalBindingV1;
use crate::common_harness_io::PatchJournalIntentV1;
use crate::common_harness_io::PatchReconciliationV1;
use crate::common_harness_io::prepare_patch_journal;
use crate::common_harness_io::reconcile_patch_journal;
use crate::common_harness_native_patch::CommonHarnessPatchArtifactV1;
use crate::common_harness_native_patch::capture_before_effect;
use crate::exec::is_likely_sandbox_denied;
use crate::session::turn_context::TurnEnvironment;
use crate::tools::sandboxing::Approvable;
use crate::tools::sandboxing::ApprovalAction;
use crate::tools::sandboxing::ExecApprovalRequirement;
use crate::tools::sandboxing::SandboxAttempt;
use crate::tools::sandboxing::Sandboxable;
use crate::tools::sandboxing::ToolCtx;
use crate::tools::sandboxing::ToolError;
use crate::tools::sandboxing::ToolRuntime;
use crate::tools::sandboxing::executor_windows_sandbox_selection;
use codex_apply_patch::AppliedPatchDelta;
use codex_apply_patch::ApplyPatchAction;
use codex_apply_patch::ApplyPatchOptions;
use codex_exec_server::FileSystemSandboxContext;
use codex_history::task_context::HostEventV1;
use codex_history::task_context::OperationKindV1;
use codex_history::task_context::OperationOutcomeV1;
use codex_history::task_context::OperationReceiptV1;
use codex_protocol::error::CodexErr;
use codex_protocol::error::SandboxErr;
use codex_protocol::exec_output::ExecToolCallOutput;
use codex_protocol::exec_output::StreamOutput;
use codex_protocol::models::AdditionalPermissionProfile;
use codex_protocol::models::PermissionProfile;
use codex_protocol::permissions::FileSystemAccessMode;
use codex_protocol::permissions::FileSystemPath;
use codex_protocol::permissions::FileSystemSpecialPath;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::FileChange;
use codex_sandboxing::SandboxType;
use codex_sandboxing::SandboxablePreference;
use codex_sandboxing::is_likely_executor_managed_sandbox_denied;
use codex_sandboxing::policy_transforms::effective_permission_profile;
use codex_sandboxing::record_filesystem_sandbox_violation;
use codex_utils_path_uri::PathUri;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct CommonHarnessPatchResultArtifactV1 {
    operation_id: String,
    normalized_patch_sha256: String,
    stdout: String,
    stderr: String,
    timed_out: bool,
    failed: bool,
    reconciliation: Option<PatchReconciliationV1>,
    actual_after_version: Option<String>,
}

/// Explicitly attached test telemetry for native patch crash/reentrancy cases.
#[cfg(test)]
#[derive(Clone, Debug)]
pub(crate) struct PatchTestObserver {
    pub(crate) counter_path: PathBuf,
    pub(crate) stage_trace_path: PathBuf,
    pub(crate) crash: Option<PatchTestCrashConfig>,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PatchTestCrashPhase {
    BeforeCompletionMarker,
    AfterCompletionMarker,
}

#[cfg(test)]
#[derive(Clone, Debug)]
pub(crate) struct PatchTestCrashConfig {
    pub(crate) manifest_path: PathBuf,
    pub(crate) phase: PatchTestCrashPhase,
}

#[cfg(test)]
impl PatchTestObserver {
    pub(crate) fn record_stage(&self, stage: &str) {
        use std::io::Write;
        let timestamp_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("test stage timestamp is after the Unix epoch")
            .as_millis();
        let mut trace = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.stage_trace_path)
            .expect("open native patch stage trace");
        writeln!(trace, "{timestamp_ms}:{stage}").expect("append native patch stage trace");
        trace.sync_all().expect("sync native patch stage trace");
        tracing::debug!(
            timestamp_ms,
            stage,
            trace = %self.stage_trace_path.display(),
            "native-patch-stage"
        );
    }

    fn record_before_effect(&self) {
        use std::io::Write;
        let previous = std::fs::read_to_string(&self.counter_path).unwrap_or_default();
        let next = previous.lines().count() + 1;
        let mut counter = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.counter_path)
            .expect("open native patch effect-attempt counter");
        writeln!(counter, "native-effect-count:{next}")
            .expect("append native patch effect-attempt counter");
        counter
            .sync_all()
            .expect("sync native patch effect-attempt counter");
    }

    fn crash_after_effect(
        &self,
        operation_id: &str,
        operation_artifact_ref: &str,
        executor_owner_id: &str,
        process_id: u32,
    ) -> ! {
        let path = &self
            .crash
            .as_ref()
            .expect("crash phase is configured")
            .manifest_path;
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).expect("read private AT04 setup manifest"))
                .expect("decode private AT04 setup manifest");
        let object = manifest.as_object_mut().expect("AT04 manifest object");
        object.insert("operation_id".into(), serde_json::json!(operation_id));
        object.insert(
            "operation_artifact_ref".into(),
            serde_json::json!(operation_artifact_ref),
        );
        object.insert(
            "operation_executor_owner_id".into(),
            serde_json::json!(executor_owner_id),
        );
        object.insert(
            "operation_executor_pid".into(),
            serde_json::json!(process_id),
        );

        let temporary = path.with_extension("tmp");
        std::fs::write(
            &temporary,
            serde_json::to_vec(&manifest).expect("encode AT04 manifest"),
        )
        .expect("write AT04 manifest temp");
        std::fs::File::open(&temporary)
            .and_then(|file| file.sync_all())
            .expect("sync AT04 manifest temp");
        std::fs::rename(&temporary, path).expect("publish AT04 operation provenance");
        if let Some(parent) = path.parent() {
            std::fs::File::open(parent)
                .and_then(|directory| directory.sync_all())
                .expect("sync AT04 manifest directory");
        }
        std::process::exit(86);
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, serde::Serialize)]
pub(crate) struct ApplyPatchApprovalKey {
    pub(crate) environment_id: String,
    pub(crate) path: PathUri,
}

#[derive(Debug)]
pub struct ApplyPatchRequest {
    pub turn_environment: TurnEnvironment,
    pub action: ApplyPatchAction,
    pub file_paths: Vec<PathUri>,
    pub changes: Arc<std::collections::HashMap<PathBuf, FileChange>>,
    pub exec_approval_requirement: ExecApprovalRequirement,
    pub additional_permissions: Option<AdditionalPermissionProfile>,
    pub permissions_preapproved: bool,
}

#[derive(Default)]
pub struct ApplyPatchRuntime {
    committed_delta: AppliedPatchDelta,
}

#[derive(Debug)]
pub struct ApplyPatchRuntimeOutput {
    pub exec_output: ExecToolCallOutput,
    pub delta: AppliedPatchDelta,
}

impl ApplyPatchRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn committed_delta(&self) -> &AppliedPatchDelta {
        &self.committed_delta
    }

    fn build_approval_action(req: &ApplyPatchRequest, call_id: &str) -> ApprovalAction {
        ApprovalAction::ApplyPatch {
            id: call_id.to_string(),
            environment_id: req.turn_environment.selection.environment_id.clone(),
            cwd: req.action.cwd.clone(),
            files: req.file_paths.clone(),
            patch: req.action.patch.clone(),
            changes: Arc::clone(&req.changes),
            permissions_preapproved: req.permissions_preapproved,
        }
    }

    fn file_system_sandbox_context_for_attempt(
        req: &ApplyPatchRequest,
        attempt: &SandboxAttempt<'_>,
    ) -> Option<FileSystemSandboxContext> {
        if !attempt.sandbox_requested {
            return None;
        }

        let permissions = effective_permission_profile(
            attempt.exec_server_permissions,
            req.additional_permissions.as_ref(),
        );
        Some(FileSystemSandboxContext {
            permissions,
            cwd: attempt.sandbox_cwd.clone(),
            workspace_roots: attempt.workspace_roots.to_vec(),
            user_home_dir: req.turn_environment.user_home_dir.clone(),
            temporary_directories: None,
            windows_sandbox_selection: executor_windows_sandbox_selection(
                attempt.windows_sandbox_type,
                attempt.windows_sandbox_level,
                attempt.sandbox_cwd,
            ),
            windows_sandbox_proxy_settings_mode: None,
            use_legacy_landlock: attempt.use_legacy_landlock,
        })
    }

    fn align_sandbox_paths_with_effect_root(
        mut sandbox: FileSystemSandboxContext,
        action_cwd: &PathUri,
        effect_cwd: &PathUri,
    ) -> std::io::Result<FileSystemSandboxContext> {
        // ApplyPatch's no-follow filesystem walk uses the canonical action root.
        // Project only policy paths intersecting that root, preserving the
        // allow/deny modes. Glob and unresolved special-path denies fail closed.
        let action_path = action_cwd.to_abs_path()?;
        let effect_path = effect_cwd.to_abs_path()?;
        if action_path.as_path().canonicalize()? != effect_path.as_path() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "sandbox alias projection requires the verified canonical action root",
            ));
        }
        if action_path.as_path() == effect_path.as_path() {
            return Ok(sandbox);
        }

        let canonicalize_policy_path = |path: &PathUri| -> std::io::Result<PathUri> {
            let absolute = path.to_abs_path()?;
            if !absolute.as_path().starts_with(action_path.as_path())
                && !action_path.as_path().starts_with(absolute.as_path())
            {
                return Ok(path.clone());
            }
            for ancestor in absolute.as_path().ancestors() {
                if std::fs::symlink_metadata(ancestor).is_err() {
                    continue;
                }
                let canonical = ancestor.canonicalize()?;
                let suffix = absolute.as_path().strip_prefix(ancestor).map_err(|error| {
                    std::io::Error::new(std::io::ErrorKind::InvalidInput, error)
                })?;
                return PathUri::from_host_native_path(canonical.join(suffix));
            }
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "sandbox policy path has no existing prefix to canonicalize",
            ))
        };

        let original_profile = sandbox.permissions.clone();
        let mut policy = original_profile.file_system_sandbox_policy();
        for entry in &mut policy.entries {
            match &mut entry.path {
                FileSystemPath::Path { path } => *path = canonicalize_policy_path(path)?,
                FileSystemPath::GlobPattern { .. } => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Unsupported,
                        "cannot prove glob policy equivalence across a symlinked action root",
                    ));
                }
                FileSystemPath::Special { value }
                    if entry.access == FileSystemAccessMode::Deny
                        && !matches!(
                            value,
                            FileSystemSpecialPath::Root
                                | FileSystemSpecialPath::ProjectRoots { .. }
                        ) =>
                {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Unsupported,
                        "cannot prove special-path denial equivalence across a symlinked action root",
                    ));
                }
                FileSystemPath::Special { .. } => {}
            }
        }
        sandbox.permissions = PermissionProfile::from_runtime_permissions_with_enforcement(
            original_profile.enforcement(),
            &policy,
            original_profile.network_sandbox_policy(),
        );
        sandbox.cwd = canonicalize_policy_path(&sandbox.cwd)?;
        sandbox.workspace_roots = sandbox
            .workspace_roots
            .iter()
            .map(&canonicalize_policy_path)
            .collect::<std::io::Result<Vec<_>>>()?;
        sandbox.user_home_dir = sandbox
            .user_home_dir
            .as_ref()
            .map(&canonicalize_policy_path)
            .transpose()?;
        sandbox.temporary_directories = sandbox
            .temporary_directories
            .as_ref()
            .map(|directories| {
                directories
                    .iter()
                    .map(&canonicalize_policy_path)
                    .collect::<std::io::Result<Vec<_>>>()
            })
            .transpose()?;
        Ok(sandbox)
    }
}

impl Sandboxable for ApplyPatchRuntime {
    fn sandbox_preference(&self) -> SandboxablePreference {
        SandboxablePreference::Auto
    }
    fn escalate_on_failure(&self) -> bool {
        true
    }
}

impl Approvable<ApplyPatchRequest> for ApplyPatchRuntime {
    fn approval_action(
        &self,
        req: &ApplyPatchRequest,
        call_id: &str,
    ) -> std::io::Result<ApprovalAction> {
        Ok(ApplyPatchRuntime::build_approval_action(req, call_id))
    }

    fn wants_no_sandbox_approval(&self, policy: AskForApproval) -> bool {
        match policy {
            AskForApproval::Never => false,
            AskForApproval::Granular(granular_config) => granular_config.allows_sandbox_approval(),
            AskForApproval::OnRequest => true,
            AskForApproval::UnlessTrusted => true,
        }
    }

    // apply_patch approvals are decided upstream by assess_patch_safety.
    //
    // This override ensures the orchestrator runs the patch approval flow when required instead
    // of falling back to the global exec approval policy.
    fn exec_approval_requirement(
        &self,
        req: &ApplyPatchRequest,
    ) -> Option<ExecApprovalRequirement> {
        Some(req.exec_approval_requirement.clone())
    }
}

impl ToolRuntime<ApplyPatchRequest, ApplyPatchRuntimeOutput> for ApplyPatchRuntime {
    fn turn_environment<'a>(&self, req: &'a ApplyPatchRequest) -> &'a TurnEnvironment {
        &req.turn_environment
    }

    fn uses_executor_managed_process_sandbox(&self, req: &ApplyPatchRequest) -> bool {
        req.turn_environment.environment.is_remote()
    }

    fn sandbox_cwd<'a>(&self, req: &'a ApplyPatchRequest) -> Option<&'a PathUri> {
        Some(&req.action.cwd)
    }

    async fn run(
        &mut self,
        req: &ApplyPatchRequest,
        attempt: &SandboxAttempt<'_>,
        ctx: &ToolCtx,
    ) -> Result<ApplyPatchRuntimeOutput, ToolError> {
        let started_at = Instant::now();
        #[cfg(test)]
        let patch_test_observer = ctx
            .step_context
            .turn
            .extension_data
            .get::<PatchTestObserver>();
        #[cfg(test)]
        if let Some(observer) = patch_test_observer.as_ref() {
            observer.record_stage("prepare:start");
        }
        preflight_common_harness_sandbox(req, attempt, ctx).await?;
        let common_harness = prepare_common_harness_patch(req, ctx).await?;
        #[cfg(test)]
        if let Some(observer) = patch_test_observer.as_ref() {
            observer.record_stage("prepare:complete");
        }
        let mut remaining_operation_time = None;
        let mut completion_marker = None;
        if let Some((root_task_id, journal, operation, _execution_claim_id, artifact_ref, _guard)) =
            common_harness.as_ref()
        {
            let persisted: CommonHarnessPatchArtifactV1 = load_operation_artifact(
                ctx.step_context.turn.config.codex_home.as_path(),
                ctx.session.thread_id,
                artifact_ref,
            )
            .map_err(|error| {
                ToolError::Rejected(format!("load durable v1 patch intent: {error}"))
            })?;
            if persisted.operation_id != operation.operation_id
                || persisted.journal != *journal
                || persisted.normalized_patch != req.action.patch
            {
                return Err(ToolError::Rejected(
                    "durable common-harness patch intent does not match the native action".into(),
                ));
            }
            let current = ctx
                .session
                .current_common_harness_state(root_task_id)
                .await
                .map_err(ToolError::Rejected)?;
            if !persisted.executor.is_bound_to(
                &operation.operation_id,
                &ctx.session.thread_id().to_string(),
                &current.run_state.owner_id,
                current.run_state.owner_epoch,
            ) || persisted.executor.process_id() != std::process::id()
            {
                return Err(ToolError::Rejected(
                    "durable common-harness patch executor provenance does not match this runtime"
                        .into(),
                ));
            }
            let marker = persisted.native_patch_completion_marker();
            let predicted_marker_ref =
                crate::common_harness_artifacts::predict_operation_artifact_ref(&marker)
                    .map_err(ToolError::Rejected)?;
            if predicted_marker_ref != persisted.completion_marker_ref {
                return Err(ToolError::Rejected(
                    "durable common-harness patch completion marker binding changed".into(),
                ));
            }
            completion_marker = Some(marker);
            let claim = current
                .operation_refs
                .iter()
                .find(|record| record.intent.operation_id == operation.operation_id)
                .and_then(|record| record.claims.last())
                .ok_or_else(|| {
                    ToolError::Rejected("v1 patch execution claim disappeared".into())
                })?;
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|error| ToolError::Rejected(format!("read v1 patch deadline: {error}")))?
                .as_millis();
            if claim.owner_epoch != current.run_state.owner_epoch
                || claim.expected_state_version != operation.expected_state_version
                || current.run_state.target_version != operation.base_target_version
                || i128::from(claim.deadline_at_ms) <= now_ms as i128
                || journal.before.digest != current.run_state.target_version
                || !journal.before.matches_workspace().map_err(|error| {
                    ToolError::Rejected(format!("recheck v1 patch preimage: {error}"))
                })?
            {
                return Err(ToolError::Rejected(
                    "common-harness v1 patch claim, target, preimage, or deadline changed before execution".into(),
                ));
            }
            remaining_operation_time = Some(Duration::from_millis(
                u64::try_from(claim.deadline_at_ms as i128 - now_ms as i128).map_err(|_| {
                    ToolError::Rejected("common-harness v1 patch deadline has expired".into())
                })?,
            ));
        }
        let fs = req.turn_environment.environment.get_filesystem();
        let sandbox = Self::file_system_sandbox_context_for_attempt(req, attempt);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        #[cfg(test)]
        if let Some(observer) = patch_test_observer.as_ref() {
            observer.record_stage("effect:start");
            observer.record_before_effect();
        }
        // Use the already verified canonical root for the effect as well as the
        // journal. System aliases such as /var must not be followed by the
        // descendant no-symlink filesystem operations.
        let effect_cwd = if let Some((_, journal, _, _, _, _)) = common_harness.as_ref() {
            PathUri::from_host_native_path(&journal.binding.workspace_root).map_err(|error| {
                ToolError::Rejected(format!("bind canonical v1 patch effect root: {error}"))
            })?
        } else {
            req.action.cwd.clone()
        };
        let sandbox = match (sandbox, common_harness.as_ref()) {
            (Some(sandbox), Some(_)) => Some(
                Self::align_sandbox_paths_with_effect_root(sandbox, &req.action.cwd, &effect_cwd)
                    .map_err(|error| {
                    ToolError::Rejected(format!("align v1 patch sandbox paths: {error}"))
                })?,
            ),
            (sandbox, _) => sandbox,
        };
        #[cfg(test)]
        if let Some(observer) = patch_test_observer.as_ref() {
            observer.record_stage(&format!(
                "effect:sandbox_context:{sandbox:?};cwd:{effect_cwd:?};sandbox_type:{:?};sandbox_requested:{}",
                attempt.sandbox, attempt.sandbox_requested
            ));
        }
        let effect = codex_apply_patch::apply_patch_with_options(
            &req.action.patch,
            ApplyPatchOptions {
                update_file_mode: req.action.update_file_mode(),
                // Only reject links when an otherwise-required sandbox was bypassed.
                // Executor-managed sandboxes can have SandboxType::None.
                follow_symlinks: common_harness.is_none()
                    && (attempt.sandbox_requested
                        || !attempt.manager.should_sandbox(
                            attempt.permissions,
                            self.sandbox_preference(),
                            attempt.enforce_managed_network,
                        )),
            },
            &effect_cwd,
            &mut stdout,
            &mut stderr,
            fs.as_ref(),
            sandbox.as_ref(),
        );
        let (result, timed_out) = if let Some(remaining) = remaining_operation_time {
            match tokio::time::timeout(remaining, effect).await {
                Ok(result) => (Some(result), false),
                Err(_) => (None, true),
            }
        } else {
            (Some(effect.await), false)
        };
        #[cfg(test)]
        if let Some(observer) = patch_test_observer.as_ref() {
            observer.record_stage("effect:complete");
            if let Some(Err(failure)) = result.as_ref() {
                observer.record_stage(&format!("effect:failure:{failure:?}"));
            }
        }
        if !timed_out
            && result.as_ref().is_some_and(Result::is_ok)
            && let (Some((_, _, _operation, _, artifact_ref, _)), Some(marker)) =
                (common_harness.as_ref(), completion_marker.as_ref())
        {
            #[cfg(test)]
            if let Some(observer) = patch_test_observer.as_ref()
                && observer.crash.as_ref().is_some_and(|config| {
                    config.phase == PatchTestCrashPhase::BeforeCompletionMarker
                })
            {
                observer.crash_after_effect(
                    &_operation.operation_id,
                    artifact_ref,
                    marker.executor.runtime_owner_id(),
                    std::process::id(),
                );
            }
            let marker_ref = write_operation_artifact(
                ctx.step_context.turn.config.codex_home.as_path(),
                ctx.session.thread_id,
                marker,
            )
            .map_err(ToolError::Rejected)?;
            let persisted: CommonHarnessPatchArtifactV1 = load_operation_artifact(
                ctx.step_context.turn.config.codex_home.as_path(),
                ctx.session.thread_id,
                artifact_ref,
            )
            .map_err(ToolError::Rejected)?;
            if marker_ref != persisted.completion_marker_ref {
                return Err(ToolError::Rejected(
                    "native patch completion marker ref did not match its reserved ref".into(),
                ));
            }
            #[cfg(test)]
            if let Some(observer) = patch_test_observer.as_ref()
                && observer.crash.as_ref().is_some_and(|config| {
                    config.phase == PatchTestCrashPhase::AfterCompletionMarker
                })
            {
                observer.crash_after_effect(
                    &_operation.operation_id,
                    artifact_ref,
                    marker.executor.runtime_owner_id(),
                    std::process::id(),
                );
            }
        }
        let stdout = String::from_utf8_lossy(&stdout).into_owned();
        let stderr = String::from_utf8_lossy(&stderr).into_owned();
        let failed = timed_out || result.as_ref().is_some_and(Result::is_err);
        let exit_code = if failed { 1 } else { 0 };
        let delta = match result {
            Some(Ok(delta)) => delta,
            Some(Err(failure)) => failure.into_parts().1,
            None => AppliedPatchDelta::default(),
        };
        self.committed_delta.append(delta);
        if let Some((root_task_id, intent, operation, execution_claim_id, _artifact_ref, guard)) =
            common_harness.as_ref()
        {
            if timed_out {
                let result_ref = write_operation_artifact(
                    ctx.step_context.turn.config.codex_home.as_path(),
                    ctx.session.thread_id,
                    &CommonHarnessPatchResultArtifactV1 {
                        operation_id: operation.operation_id.clone(),
                        normalized_patch_sha256: format!(
                            "{:x}",
                            sha2::Sha256::digest(req.action.patch.as_bytes())
                        ),
                        stdout: stdout.clone(),
                        stderr: stderr.clone(),
                        timed_out: true,
                        failed: true,
                        reconciliation: None,
                        actual_after_version: None,
                    },
                )
                .ok();
                let current = ctx
                    .session
                    .current_common_harness_state(root_task_id)
                    .await
                    .map_err(ToolError::Rejected)?;
                let receipt = OperationReceiptV1 {
                    operation_id: operation.operation_id.clone(),
                    task_id: operation.task_id.clone(),
                    request_revision: operation.request_revision,
                    attempt_no: operation.attempt_no,
                    owner_epoch: operation.owner_epoch,
                    execution_id: execution_claim_id.clone(),
                    outcome: OperationOutcomeV1::Unknown,
                    actual_before_version: intent.before.digest.clone(),
                    actual_after_version: None,
                    exit_status: None,
                    result_ref,
                    external_receipt_ref: None,
                    reconciliation_ref: None,
                };
                ctx.session
                    .apply_common_harness_host_event_guarded(
                        root_task_id,
                        codex_history::task_context::StateFenceV1 {
                            request_revision: current.request.request_revision,
                            expected_state_version: current.run_state.state_version,
                            owner_epoch: current.run_state.owner_epoch,
                        },
                        HostEventV1::FinalizeOperation { receipt },
                        guard,
                    )
                    .await
                    .map_err(ToolError::Rejected)?;
                return Err(ToolError::Rejected(
                    "common-harness v1 native patch exceeded its absolute operation deadline; outcome is blocked for reconciliation".into(),
                ));
            }
            let reconciliation = match reconcile_patch_journal(intent) {
                Ok(reconciliation) => reconciliation,
                Err(error) => {
                    let current = ctx
                        .session
                        .current_common_harness_state(root_task_id)
                        .await
                        .map_err(ToolError::Rejected)?;
                    let receipt = OperationReceiptV1 {
                        operation_id: operation.operation_id.clone(),
                        task_id: operation.task_id.clone(),
                        request_revision: operation.request_revision,
                        attempt_no: operation.attempt_no,
                        owner_epoch: operation.owner_epoch,
                        execution_id: execution_claim_id.clone(),
                        outcome: OperationOutcomeV1::Unknown,
                        actual_before_version: intent.before.digest.clone(),
                        actual_after_version: None,
                        exit_status: None,
                        result_ref: None,
                        external_receipt_ref: None,
                        reconciliation_ref: None,
                    };
                    ctx.session
                        .apply_common_harness_host_event_guarded(
                            root_task_id,
                            codex_history::task_context::StateFenceV1 {
                                request_revision: current.request.request_revision,
                                expected_state_version: current.run_state.state_version,
                                owner_epoch: current.run_state.owner_epoch,
                            },
                            HostEventV1::FinalizeOperation { receipt },
                            guard,
                        )
                        .await
                        .map_err(ToolError::Rejected)?;
                    return Err(ToolError::Rejected(format!(
                        "v1 patch outcome could not be reconciled: {error}"
                    )));
                }
            };
            let after_version = match reconciliation {
                PatchReconciliationV1::AllPre => Some(intent.before.digest.clone()),
                PatchReconciliationV1::AllPost => Some(intent.predicted_after.digest.clone()),
                PatchReconciliationV1::PartialOrOther => None,
            };
            let outcome = match (reconciliation, failed) {
                (PatchReconciliationV1::AllPost, false) => OperationOutcomeV1::Succeeded,
                (PatchReconciliationV1::AllPre, true) => OperationOutcomeV1::Failed,
                _ => OperationOutcomeV1::Unknown,
            };
            let result_ref = write_operation_artifact(
                ctx.step_context.turn.config.codex_home.as_path(),
                ctx.session.thread_id,
                &CommonHarnessPatchResultArtifactV1 {
                    operation_id: operation.operation_id.clone(),
                    normalized_patch_sha256: format!(
                        "{:x}",
                        sha2::Sha256::digest(req.action.patch.as_bytes())
                    ),
                    stdout: stdout.clone(),
                    stderr: stderr.clone(),
                    timed_out: false,
                    failed,
                    reconciliation: Some(reconciliation),
                    actual_after_version: after_version.clone(),
                },
            )
            .ok();
            let outcome = if result_ref.is_some() {
                outcome
            } else {
                OperationOutcomeV1::Unknown
            };
            let receipt = OperationReceiptV1 {
                operation_id: operation.operation_id.clone(),
                task_id: operation.task_id.clone(),
                request_revision: operation.request_revision,
                attempt_no: operation.attempt_no,
                owner_epoch: operation.owner_epoch,
                execution_id: execution_claim_id.clone(),
                outcome,
                actual_before_version: intent.before.digest.clone(),
                actual_after_version: after_version.clone(),
                exit_status: Some(if failed { 1 } else { 0 }),
                result_ref: result_ref.clone(),
                external_receipt_ref: None,
                reconciliation_ref: if outcome == OperationOutcomeV1::Unknown {
                    None
                } else {
                    result_ref.clone()
                },
            };
            let finalized = ctx
                .session
                .current_common_harness_state(root_task_id)
                .await
                .map_err(|error| ToolError::Rejected(format!("read v1 patch state: {error}")))?;
            let fence = codex_history::task_context::StateFenceV1 {
                request_revision: finalized.request.request_revision,
                expected_state_version: finalized.run_state.state_version,
                owner_epoch: finalized.run_state.owner_epoch,
            };
            let finalized = ctx
                .session
                .apply_common_harness_host_event_guarded(
                    root_task_id,
                    fence,
                    HostEventV1::FinalizeOperation { receipt },
                    guard,
                )
                .await
                .map_err(|error| {
                    ToolError::Rejected(format!("persist v1 patch receipt: {error}"))
                })?;
            #[cfg(test)]
            if let Some(observer) = patch_test_observer.as_ref() {
                observer.record_stage("receipt:complete");
            }
            if reconciliation == PatchReconciliationV1::AllPost {
                let evidence_ids = finalized
                    .evidence_refs
                    .iter()
                    .filter(|evidence| {
                        evidence.lifecycle
                            == codex_history::task_context::EvidenceLifecycleV1::Current
                    })
                    .map(|evidence| evidence.evidence_id.clone())
                    .collect();
                let fence = codex_history::task_context::StateFenceV1 {
                    request_revision: finalized.request.request_revision,
                    expected_state_version: finalized.run_state.state_version,
                    owner_epoch: finalized.run_state.owner_epoch,
                };
                ctx.session
                    .apply_common_harness_host_event_guarded(
                        root_task_id,
                        fence,
                        HostEventV1::InvalidateEvidence {
                            evidence_ids,
                            new_target_version: intent.predicted_after.digest.clone(),
                        },
                        guard,
                    )
                    .await
                    .map_err(|error| {
                        ToolError::Rejected(format!("persist v1 patch target: {error}"))
                    })?;
                #[cfg(test)]
                if let Some(observer) = patch_test_observer.as_ref() {
                    observer.record_stage("invalidate:complete");
                }
            }
        }
        let output = ExecToolCallOutput {
            exit_code,
            stdout: StreamOutput::new(stdout.clone()),
            stderr: StreamOutput::new(stderr.clone()),
            aggregated_output: StreamOutput::new(format!("{stdout}{stderr}")),
            duration: started_at.elapsed(),
            timed_out: false,
        };
        let sandbox_denied = failed
            && if attempt.sandbox == SandboxType::None {
                attempt.sandbox_requested && is_likely_executor_managed_sandbox_denied(&output)
            } else {
                is_likely_sandbox_denied(attempt.sandbox, &output)
            };
        if sandbox_denied {
            if common_harness.is_some() {
                return Err(ToolError::Rejected(format!(
                    "common-harness v1 patch was denied by its selected sandbox: {}",
                    output.aggregated_output.text
                )));
            }
            // TODO(iceweasel): Report executor filesystem sandbox backends like process/start so
            // executor-managed apply_patch denials can emit backend-specific violation telemetry.
            if attempt.sandbox != SandboxType::None {
                record_filesystem_sandbox_violation(attempt.sandbox, &output);
            }
            return Err(ToolError::Codex(CodexErr::Sandbox(SandboxErr::Denied {
                output: Box::new(output),
                network_policy_decision: None,
            })));
        }
        #[cfg(test)]
        if let Some(observer) = patch_test_observer.as_ref() {
            observer.record_stage("runtime:return");
        }
        Ok(ApplyPatchRuntimeOutput {
            exec_output: output,
            delta: self.committed_delta.clone(),
        })
    }
}

async fn prepare_common_harness_patch(
    req: &ApplyPatchRequest,
    ctx: &ToolCtx,
) -> Result<
    Option<(
        String,
        PatchJournalIntentV1,
        codex_history::task_context::OperationIntentV1,
        String,
        String,
        tokio::sync::OwnedMutexGuard<()>,
    )>,
    ToolError,
> {
    let Some(scope) = ctx.session.active_task_scope().await else {
        return Ok(None);
    };
    let is_common_harness = ctx
        .session
        .rencrow_completion_state(&scope.root_task_id)
        .await
        .is_some_and(|completion| completion.common_harness.is_some());
    if !is_common_harness {
        return Ok(None);
    }
    if req.turn_environment.environment.is_remote() {
        return Err(ToolError::Rejected(
            "common-harness v1 patch execution requires a local workspace".into(),
        ));
    }
    #[cfg(test)]
    let patch_test_observer = ctx
        .step_context
        .turn
        .extension_data
        .get::<PatchTestObserver>();
    #[cfg(test)]
    if let Some(observer) = patch_test_observer.as_ref() {
        observer.record_stage("attempt:ensure:start");
    }
    let _initial = ctx
        .session
        .ensure_common_harness_attempt(&scope.root_task_id)
        .await
        .map_err(ToolError::Rejected)?;
    #[cfg(test)]
    if let Some(observer) = patch_test_observer.as_ref() {
        observer.record_stage("attempt:ensure:complete");
        observer.record_stage("guard:acquire:start");
    }
    let guard = ctx.session.completion_owner_guard().await;
    #[cfg(test)]
    if let Some(observer) = patch_test_observer.as_ref() {
        observer.record_stage("guard:acquire:complete");
    }
    let state = ctx
        .session
        .current_common_harness_state(&scope.root_task_id)
        .await
        .map_err(ToolError::Rejected)?;
    let workspace_root = req
        .turn_environment
        .cwd()
        .to_abs_path()
        .map_err(|error| ToolError::Rejected(format!("resolve v1 patch workspace: {error}")))?
        .canonicalize()
        .map_err(|error| {
            ToolError::Rejected(format!("canonicalize v1 patch workspace: {error}"))
        })?;
    let action_root_path =
        req.action.cwd.to_abs_path().map_err(|error| {
            ToolError::Rejected(format!("resolve v1 patch action cwd: {error}"))
        })?;
    let action_root = action_root_path.canonicalize().map_err(|error| {
        ToolError::Rejected(format!("canonicalize v1 patch action cwd: {error}"))
    })?;
    if workspace_root != action_root {
        return Err(ToolError::Rejected(
            "common-harness v1 patches must target the bound workspace root".into(),
        ));
    }
    let artifact_binding = ArtifactBindingV1::for_canonical_root(&workspace_root)
        .map_err(|error| ToolError::Rejected(format!("bind v1 patch workspace: {error}")))?;
    if artifact_binding.workspace_binding_ref != state.request.workspace_ref {
        return Err(ToolError::Rejected(
            "common-harness v1 patch workspace binding changed".into(),
        ));
    }
    let changed_paths = req
        .file_paths
        .iter()
        .map(|uri| {
            let absolute = uri
                .to_abs_path()
                .map_err(|error| ToolError::Rejected(format!("resolve v1 patch path: {error}")))?;
            absolute
                .strip_prefix(&action_root_path)
                .map(std::path::Path::to_path_buf)
                .map_err(|_| ToolError::Rejected("v1 patch path escapes the workspace".into()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let allowed_artifact_paths = state
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
        .collect::<std::collections::BTreeSet<_>>();
    if changed_paths
        .iter()
        .any(|path| !allowed_artifact_paths.contains(path))
    {
        return Err(ToolError::Rejected(
            "common-harness v1 patch targets a path outside the registered artifact set".into(),
        ));
    }
    let artifact_paths = allowed_artifact_paths.into_iter().collect::<Vec<_>>();
    let operation_id = format!("patch:{}", ctx.call_id);
    let binding = PatchJournalBindingV1 {
        task_id: state.request.task_id.clone(),
        request_revision: state.request.request_revision,
        attempt_no: state.run_state.attempt_no,
        owner_epoch: state.run_state.owner_epoch,
        expected_state_version: state.run_state.state_version,
        operation_id: operation_id.clone(),
        workspace_root: workspace_root.to_path_buf(),
        artifact_binding,
        artifact_paths,
    };
    let journal = prepare_patch_journal(&binding, &req.action)
        .map_err(|error| ToolError::Rejected(format!("prepare v1 patch journal: {error}")))?;
    if journal.before.digest != state.run_state.target_version
        || !journal
            .before
            .matches_workspace()
            .map_err(|error| ToolError::Rejected(format!("verify v1 patch preimage: {error}")))?
    {
        return Err(ToolError::Rejected(
            "common-harness v1 patch preimage no longer matches the host target".into(),
        ));
    }
    let executor = capture_before_effect(
        &operation_id,
        &ctx.session.thread_id().to_string(),
        state.run_state.owner_epoch,
    )
    .map_err(ToolError::Rejected)?;
    if executor.runtime_owner_id() != state.run_state.owner_id {
        return Err(ToolError::Rejected(
            "common-harness patch runtime owner changed before operation reservation".into(),
        ));
    }
    let mut artifact = CommonHarnessPatchArtifactV1 {
        operation_id: operation_id.clone(),
        journal: journal.clone(),
        normalized_patch: req.action.patch.clone(),
        executor,
        completion_marker_ref: String::new(),
    };
    let marker_ref = crate::common_harness_artifacts::predict_operation_artifact_ref(
        &artifact.native_patch_completion_marker(),
    )
    .map_err(ToolError::Rejected)?;
    artifact.completion_marker_ref = marker_ref;
    #[cfg(test)]
    if let Some(observer) = patch_test_observer.as_ref() {
        observer.record_stage("artifact:write:start");
    }
    let artifact_ref = write_operation_artifact(
        ctx.step_context.turn.config.codex_home.as_path(),
        ctx.session.thread_id,
        &artifact,
    )
    .map_err(ToolError::Rejected)?;
    #[cfg(test)]
    if let Some(observer) = patch_test_observer.as_ref() {
        observer.record_stage("artifact:write:complete");
        observer.record_stage("reserve:start");
    }
    let (operation, execution_claim_id) = ctx
        .session
        .reserve_common_harness_operation_guarded(
            &scope.root_task_id,
            crate::session::common_harness::NativeOperationDefinition {
                operation_id,
                operation_kind: OperationKindV1::ApplyPatch,
                normalized_arguments: req.action.patch.as_bytes(),
                normalized_arguments_ref: artifact_ref.clone(),
                permission_decision_ref: state.request.permission_snapshot_ref.clone(),
                executor_ref: "codex-apply-patch/local".into(),
                timeout_ms: 30_000,
            },
            &guard,
        )
        .await
        .map_err(ToolError::Rejected)?;
    #[cfg(test)]
    if let Some(observer) = patch_test_observer.as_ref() {
        observer.record_stage("reserve:complete");
    }
    if operation.base_target_version != journal.before.digest {
        return Err(ToolError::Rejected(
            "common-harness v1 patch target changed after reservation".into(),
        ));
    }
    let current = ctx
        .session
        .current_common_harness_state(&scope.root_task_id)
        .await
        .map_err(ToolError::Rejected)?;
    let claim = current
        .operation_refs
        .iter()
        .find(|record| record.intent.operation_id == operation.operation_id)
        .and_then(|record| {
            record
                .claims
                .iter()
                .find(|claim| claim.claim_id == execution_claim_id)
        })
        .ok_or_else(|| ToolError::Rejected("v1 patch execution claim disappeared".into()))?;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| ToolError::Rejected(format!("read v1 patch deadline: {error}")))?
        .as_millis();
    if claim.owner_epoch != current.run_state.owner_epoch
        || claim.expected_state_version != operation.expected_state_version
        || current.run_state.target_version != operation.base_target_version
        || i128::from(claim.deadline_at_ms) <= now_ms as i128
    {
        return Err(ToolError::Rejected(
            "common-harness v1 patch claim, target, or deadline changed before execution".into(),
        ));
    }
    Ok(Some((
        scope.root_task_id,
        journal,
        operation,
        execution_claim_id,
        artifact_ref,
        guard,
    )))
}

async fn preflight_common_harness_sandbox(
    req: &ApplyPatchRequest,
    attempt: &SandboxAttempt<'_>,
    ctx: &ToolCtx,
) -> Result<(), ToolError> {
    let Some(scope) = ctx.session.active_task_scope().await else {
        return Ok(());
    };
    let is_common_harness = ctx
        .session
        .rencrow_completion_state(&scope.root_task_id)
        .await
        .is_some_and(|completion| completion.common_harness.is_some());
    if !is_common_harness {
        return Ok(());
    }
    let Some(sandbox) = ApplyPatchRuntime::file_system_sandbox_context_for_attempt(req, attempt)
    else {
        return Ok(());
    };
    let canonical_action_root = req
        .action
        .cwd
        .to_abs_path()
        .and_then(|path| path.as_path().canonicalize())
        .map_err(|error| {
            ToolError::Rejected(format!("preflight v1 patch sandbox root: {error}"))
        })?;
    let effect_cwd = PathUri::from_host_native_path(canonical_action_root)
        .map_err(|error| ToolError::Rejected(format!("preflight v1 patch effect root: {error}")))?;
    let projection = ApplyPatchRuntime::align_sandbox_paths_with_effect_root(
        sandbox,
        &req.action.cwd,
        &effect_cwd,
    );
    if let Err(error) = projection {
        let state = ctx
            .session
            .current_common_harness_state(&scope.root_task_id)
            .await
            .map_err(ToolError::Rejected)?;
        if state.request.execution_owner
            == codex_history::task_context::ExecutionOwnerV1::StandaloneSwitch
        {
            ctx.session.apply_common_harness_host_event(
                &scope.root_task_id,
                codex_history::task_context::StateFenceV1 {
                    request_revision: state.request.request_revision,
                    expected_state_version: state.run_state.state_version,
                    owner_epoch: state.run_state.owner_epoch,
                },
                HostEventV1::Block {
                    reason_code: "permission_normalization_unavailable".into(),
                    resume_condition: "Reconcile the workspace binding and permission policy before requesting another native operation.".into(),
                },
            ).await.map_err(ToolError::Rejected)?;
        }
        return Err(ToolError::Rejected(format!(
            "preflight v1 patch sandbox paths: {error}"
        )));
    }
    Ok(())
}

#[cfg(test)]
#[path = "apply_patch_tests.rs"]
mod tests;
