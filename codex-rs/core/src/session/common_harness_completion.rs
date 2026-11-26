// Modified by RenCrow Switch Core, 2026-11-26: gate common-harness inference and turn completion.
// Modified by RenCrow Switch Core, 2026-11-27: verify native acceptance and restore the local writer fence.
use super::session::Session;
use super::turn_context::TurnContext;
use crate::common_harness_io::ArtifactBindingV1;
use crate::common_harness_io::ArtifactSetV1;
use codex_history::task_context::CommonHarnessStateV1;
use codex_history::task_context::ConditionVerdictV1;
use codex_history::task_context::EvidenceLifecycleV1;
use codex_history::task_context::HostEventV1;
use codex_history::task_context::TaskRunStatusV1;
use codex_protocol::protocol::SessionSource;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CommonHarnessTurnEndDecision {
    Legacy,
    Continue {
        next_action: String,
    },
    Stopped {
        status: TaskRunStatusV1,
        reason: String,
    },
    Completed {
        target_version: String,
    },
}

pub(crate) fn common_harness_stop_reason(state: &CommonHarnessStateV1) -> Option<String> {
    matches!(
        state.run_state.status,
        TaskRunStatusV1::Waiting
            | TaskRunStatusV1::Blocked
            | TaskRunStatusV1::Exhausted
            | TaskRunStatusV1::Cancelled
    )
    .then(|| {
        format!(
            "common-harness task {} is {:?}: {}",
            state.request.task_id,
            state.run_state.status,
            state
                .run_state
                .reason_code
                .as_deref()
                .unwrap_or("task suspended")
        )
    })
}

fn verified_condition_ids(common: &CommonHarnessStateV1) -> Result<Vec<String>, String> {
    common
        .apply_host_event(
            super::common_harness::common_harness_fence(common),
            HostEventV1::Complete {
                target_version: common.run_state.target_version.clone(),
            },
        )
        .map_err(|error| {
            format!("common-harness native conditions are not satisfied: {error:?}")
        })?;
    Ok(common
        .request
        .conditions
        .iter()
        .filter(|condition| condition.required)
        .map(|condition| condition.condition_id.clone())
        .collect())
}

fn needs_native_patch_reconciliation(common: &CommonHarnessStateV1) -> bool {
    if !common.run_state.unsettled_operation_refs.is_empty() {
        return common.operation_refs.iter().any(|record| {
            common
                .run_state
                .unsettled_operation_refs
                .contains(&record.intent.operation_id)
                && record.intent.operation_kind
                    == codex_history::task_context::OperationKindV1::ApplyPatch
        });
    }
    common.run_state.unsettled_operation_refs.is_empty()
        && common.operation_refs.iter().any(|record| {
            super::common_harness_native_patch_recovery::is_current_pending_resume(common, record)
        })
}

impl Session {
    /// Explicit status may re-observe an accepted artifact; ordinary chat does not.
    pub(crate) async fn refresh_common_harness_status(
        &self,
        turn: &TurnContext,
        root: &str,
    ) -> Result<codex_history::task_context::CompletionState, String> {
        let completion = self
            .rencrow_completion_state(root)
            .await
            .ok_or_else(|| "no host-registered completion contract is available".to_string())?;
        let Some(common) = completion.common_harness.as_ref() else {
            return Ok(completion);
        };
        if self.is_read_only_review_actor_for_turn(turn).await
            || common.run_state.status != TaskRunStatusV1::Completed
            || self.active_task_scope().await.is_some()
        {
            return Ok(completion);
        }
        if common.request.execution_owner
            != codex_history::task_context::ExecutionOwnerV1::StandaloneSwitch
        {
            return Ok(completion);
        }
        let workspace = turn
            .config
            .cwd
            .as_path()
            .canonicalize()
            .map_err(|error| format!("resolve status workspace: {error}"))?;
        let binding = ArtifactBindingV1::for_canonical_root(&workspace)
            .map_err(|error| format!("bind status workspace: {error}"))?;
        if binding.workspace_binding_ref != common.request.workspace_ref {
            return Err("common-harness status workspace binding changed".into());
        }
        let paths = super::common_harness::request_dependency_paths(&common.request)?;
        let target = ArtifactSetV1::capture_bound(&workspace, &paths, &binding)
            .map_err(|error| format!("observe accepted common-harness target: {error}"))?;
        if !target
            .matches_workspace()
            .map_err(|error| format!("recheck status target: {error}"))?
        {
            return Err("common-harness target changed while status observed it".into());
        }
        if target.digest != common.run_state.target_version {
            let runtime_owner = super::common_harness::common_harness_runtime_owner_id(
                &self.thread_id().to_string(),
            );
            let current = if common.run_state.owner_id != runtime_owner {
                let proof = super::common_harness_writer::verify_local_writer(self)
                    .await
                    .map_err(|error| {
                        format!("verify status writer before revalidation: {error}")
                    })?;
                self.take_over_common_harness_local_writer(root, turn, proof)
                    .await?
            } else {
                common.clone()
            };
            self.reserve_common_harness_iteration_after_target_change(root, &current, target)
                .await?;
            return self
                .rencrow_completion_state(root)
                .await
                .ok_or_else(|| "completion status disappeared after re-observation".to_string());
        }
        Ok(completion)
    }

    pub(crate) async fn common_harness_acceptance_conditions(
        &self,
        turn: &TurnContext,
    ) -> Result<Option<Vec<String>>, String> {
        let Some((root, common)) = self.common_harness_turn_state(turn).await? else {
            return Ok(None);
        };
        let common = self
            .refresh_common_harness_target(&root, common, turn)
            .await?;
        self.validate_common_harness_evidence_artifacts(turn, &common)?;
        verified_condition_ids(&common).map(Some)
    }

    /// Re-observe the host-bound artifact scope before using persisted evidence.
    /// A changed target invalidates current proofs; it never changes the request baseline.
    async fn refresh_common_harness_target(
        &self,
        root: &str,
        common: CommonHarnessStateV1,
        turn: &TurnContext,
    ) -> Result<CommonHarnessStateV1, String> {
        let workspace = turn
            .config
            .cwd
            .as_path()
            .canonicalize()
            .map_err(|error| format!("resolve common-harness completion workspace: {error}"))?;
        let binding = ArtifactBindingV1::for_canonical_root(&workspace)
            .map_err(|error| format!("bind common-harness completion workspace: {error}"))?;
        if binding.workspace_binding_ref != common.request.workspace_ref {
            return Err("common-harness completion workspace binding changed".into());
        }
        let paths = super::common_harness::request_dependency_paths(&common.request)?;
        let target = ArtifactSetV1::capture_bound(&workspace, &paths, &binding)
            .map_err(|error| format!("capture common-harness completion target: {error}"))?;
        if !target
            .matches_workspace()
            .map_err(|error| format!("recheck common-harness completion target: {error}"))?
        {
            return Err("common-harness completion target changed while being observed".into());
        }
        if target.digest == common.run_state.target_version {
            return Ok(common);
        }
        if !common.run_state.unsettled_operation_refs.is_empty() {
            return Err("common-harness target changed while an operation is unsettled".into());
        }
        self.rebind_common_harness_target(root, turn, common, target)
            .await
    }

    /// Read the persisted root contract; malformed v1 state cannot become a legacy fallback.
    pub(crate) async fn common_harness_turn_state(
        &self,
        turn: &TurnContext,
    ) -> Result<Option<(String, CommonHarnessStateV1)>, String> {
        if !matches!(
            turn.session_source,
            SessionSource::Cli | SessionSource::VSCode | SessionSource::Exec
        ) {
            return Ok(None);
        }
        if self.is_read_only_review_actor_for_turn(turn).await
            || self
                .common_harness_tool_state_for_turn(turn)
                .await
                .is_none()
        {
            return Ok(None);
        }
        let (root, canonical) = {
            let state = self.state.lock().await;
            let Some((root, canonical)) = state.rencrow_completion_state.as_ref() else {
                return Ok(None);
            };
            if canonical.common_harness.is_none() {
                return Ok(None);
            }
            (root.clone(), canonical.clone())
        };
        let completion = super::completion_review::project_completion_state(&canonical)
            .map_err(|error| format!("read canonical common-harness contract: {error}"))?;
        let Some(common) = completion.common_harness else {
            return Ok(None);
        };
        if common.request.execution_owner
            != codex_history::task_context::ExecutionOwnerV1::StandaloneSwitch
        {
            return Err(
                "S1 common-harness cannot locally execute or recover an external runner-owned task"
                    .into(),
            );
        }
        if common.run_state.status == TaskRunStatusV1::Completed
            && self.active_task_scope().await.is_none()
        {
            return Ok(None);
        }
        let runtime_owner =
            super::common_harness::common_harness_runtime_owner_id(&self.thread_id().to_string());
        let common = if common.run_state.owner_id != runtime_owner {
            let proof = super::common_harness_writer::verify_local_writer(self)
                .await
                .map_err(|error| format!("verify resumed common-harness writer: {error}"))?;
            self.take_over_common_harness_local_writer(&root, turn, proof)
                .await?
        } else {
            common
        };
        if !needs_native_patch_reconciliation(&common) {
            return Ok(Some((root, common)));
        }
        if matches!(
            common.run_state.status,
            TaskRunStatusV1::Waiting | TaskRunStatusV1::Cancelled
        ) || common.run_state.cancel_requested
        {
            return Ok(Some((root, common)));
        }

        // Takeover acquired and released its own guard. Reacquire once for
        // recovery, then reread the canonical root and recheck the candidate.
        let owner_guard = self.completion_owner_guard().await;
        let current = self.current_common_harness_state(&root).await?;
        if current.run_state.owner_id != runtime_owner {
            return Err("common-harness owner changed before native patch recovery".into());
        }
        if !needs_native_patch_reconciliation(&current) {
            return Ok(Some((root, current)));
        }
        if matches!(
            current.run_state.status,
            TaskRunStatusV1::Waiting | TaskRunStatusV1::Cancelled
        ) || current.run_state.cancel_requested
        {
            return Ok(Some((root, current)));
        }
        let recovered = self
            .reconcile_unsettled_common_harness_native_patch(&root, turn, current, &owner_guard)
            .await;
        let recovered = match recovered {
            Ok(state) => state,
            Err(error) => {
                let state = self.current_common_harness_state(&root).await?;
                if matches!(
                    state.run_state.status,
                    TaskRunStatusV1::Queued | TaskRunStatusV1::Running | TaskRunStatusV1::Verifying
                ) {
                    self.apply_common_harness_host_event_guarded(
                        &root,
                        super::common_harness::common_harness_fence(&state),
                        HostEventV1::Block {
                            reason_code: "native_patch_reconciliation_unavailable".into(),
                            resume_condition: error,
                        },
                        &owner_guard,
                    )
                    .await?
                } else {
                    state
                }
            }
        };
        Ok(Some((root, recovered)))
    }

    pub(crate) async fn common_harness_turn_end_gate(
        &self,
        turn: &TurnContext,
    ) -> Result<CommonHarnessTurnEndDecision, String> {
        if self.is_read_only_review_actor_for_turn(turn).await {
            return Ok(match self.common_harness_tool_state_for_turn(turn).await {
                Some((_, common)) => CommonHarnessTurnEndDecision::Stopped {
                    status: common.run_state.status,
                    reason: "read-only review output does not advance the parent workflow".into(),
                },
                None => CommonHarnessTurnEndDecision::Legacy,
            });
        }
        let Some((root, common)) = self.common_harness_turn_state(turn).await? else {
            return Ok(CommonHarnessTurnEndDecision::Legacy);
        };
        if let Some(reason) = common_harness_stop_reason(&common) {
            return Ok(CommonHarnessTurnEndDecision::Stopped {
                status: common.run_state.status,
                reason,
            });
        }
        if self
            .reserved_common_harness_execution_for_turn(turn)
            .await
            .is_some()
        {
            let next_action = self
                .rencrow_completion_state(&root)
                .await
                .and_then(|completion| completion.next_action)
                .ok_or_else(|| {
                    "reserved common-harness execution has no host next action".to_string()
                })?;
            return Ok(CommonHarnessTurnEndDecision::Continue { next_action });
        }
        let common = if common.run_state.status == TaskRunStatusV1::Queued {
            self.ensure_common_harness_attempt(&root).await?
        } else {
            common
        };
        let common = self
            .refresh_common_harness_target(&root, common, turn)
            .await?;
        if let Some(reason) = common_harness_stop_reason(&common) {
            return Ok(CommonHarnessTurnEndDecision::Stopped {
                status: common.run_state.status,
                reason,
            });
        }
        if common.run_state.status == TaskRunStatusV1::Completed {
            return Ok(CommonHarnessTurnEndDecision::Completed {
                target_version: common.run_state.target_version,
            });
        }
        if matches!(
            common.run_state.status,
            TaskRunStatusV1::Running | TaskRunStatusV1::Verifying
        ) && common.run_state.unsettled_operation_refs.is_empty()
        {
            let mut all_terminal = true;
            let mut failed_this_attempt = Vec::new();
            let mut verifier_error = None;
            for condition in common.request.conditions.iter().filter(|c| c.required) {
                let latest = common.evidence_refs.iter().rev().find(|evidence| {
                    evidence.condition_id == condition.condition_id
                        && evidence.condition_revision == condition.condition_revision
                        && evidence.condition_set_revision == common.request.condition_set_revision
                        && evidence.request_revision == common.request.request_revision
                        && evidence.target_version == common.run_state.target_version
                        && evidence.lifecycle == EvidenceLifecycleV1::Current
                });
                let Some(evidence) = latest else {
                    all_terminal = false;
                    continue;
                };
                let observed_this_attempt = common.operation_refs.iter().any(|operation| {
                    operation.intent.operation_id == evidence.operation_id
                        && operation.intent.attempt_no == common.run_state.attempt_no
                });
                match evidence.verdict {
                    ConditionVerdictV1::Pass => {}
                    ConditionVerdictV1::Fail => {
                        if observed_this_attempt {
                            failed_this_attempt.push(format!(
                                "{}: {} (evidence {})",
                                condition.condition_id, evidence.observed, evidence.evidence_id
                            ));
                        }
                    }
                    ConditionVerdictV1::Indeterminate | ConditionVerdictV1::Error => {
                        all_terminal = false;
                        if observed_this_attempt {
                            verifier_error = Some(format!(
                                "{}: {} (evidence {})",
                                condition.condition_id, evidence.observed, evidence.evidence_id
                            ));
                        }
                    }
                }
            }
            if let Some(reason) = verifier_error {
                let stopped = self
                    .apply_common_harness_host_event(
                        &root,
                        super::common_harness::common_harness_fence(&common),
                        HostEventV1::Block {
                            reason_code: "required_verifier_unavailable".into(),
                            resume_condition: reason.clone(),
                        },
                    )
                    .await?;
                return Ok(CommonHarnessTurnEndDecision::Stopped {
                    status: stopped.run_state.status,
                    reason,
                });
            }
            if common.run_state.status == TaskRunStatusV1::Verifying
                && all_terminal
                && !failed_this_attempt.is_empty()
            {
                let reason = failed_this_attempt.join("; ");
                let event = if common.run_state.attempt_no < common.request.max_attempts {
                    HostEventV1::StartAttempt {
                        target_version: common.run_state.target_version.clone(),
                    }
                } else {
                    HostEventV1::EndAttempt {
                        reason_code: "required_conditions_failed_at_attempt_limit".into(),
                    }
                };
                let next = self
                    .apply_common_harness_host_event(
                        &root,
                        super::common_harness::common_harness_fence(&common),
                        event,
                    )
                    .await?;
                if next.run_state.status == TaskRunStatusV1::Exhausted {
                    return Ok(CommonHarnessTurnEndDecision::Stopped {
                        status: next.run_state.status,
                        reason,
                    });
                }
                return Ok(CommonHarnessTurnEndDecision::Continue {
                    next_action: format!(
                        "Attempt {} of {}. Repair the unmet conditions: {reason}",
                        next.run_state.attempt_no, next.request.max_attempts
                    ),
                });
            }
        }
        if common.run_state.status == TaskRunStatusV1::Verifying
            && verified_condition_ids(&common).is_ok()
        {
            if self.active_task_scope().await.is_none() {
                self.reconcile_rencrow_completion_evidence(&root).await?;
                if self.autonomous_completion_is_complete().await {
                    let current = self
                        .refresh_common_harness_target(&root, common, turn)
                        .await?;
                    if verified_condition_ids(&current).is_ok() {
                        self.validate_common_harness_evidence_artifacts(turn, &current)?;
                        let target_version = current.run_state.target_version.clone();
                        self.apply_common_harness_host_event(
                            &root,
                            super::common_harness::common_harness_fence(&current),
                            HostEventV1::Complete {
                                target_version: target_version.clone(),
                            },
                        )
                        .await?;
                        return Ok(CommonHarnessTurnEndDecision::Completed { target_version });
                    }
                    return Ok(CommonHarnessTurnEndDecision::Continue {
                        next_action: current.run_state.next_operation,
                    });
                }
                let next_action = self
                    .rencrow_completion_state(&root)
                    .await
                    .and_then(|state| state.next_action)
                    .unwrap_or_else(|| "Resolve the existing native completion obligations before final completion.".into());
                return Ok(CommonHarnessTurnEndDecision::Continue { next_action });
            }
            return Ok(CommonHarnessTurnEndDecision::Continue {
                next_action: "Call task_context_boundary accept with a result summary. The host derives satisfied conditions from current native evidence.".into(),
            });
        }
        Ok(CommonHarnessTurnEndDecision::Continue {
            next_action: common.run_state.next_operation,
        })
    }
}
