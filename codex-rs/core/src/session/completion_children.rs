//! Collection state follows native tool receipts, not the child's completion prose.
// Modified by RenCrow Switch Core, 2026-11-11.
use super::session::Session;
use codex_history::compaction_candidate::digest;
use codex_history::task_context::CompletionReviewEvidence;
use codex_history::task_context::CompletionReviewLaunchState;
use codex_history::task_context::CompletionReviewLaunchStatus;
use codex_history::task_context::CompletionReviewTerminalStatus;
use codex_history::task_context::CompletionUnit;
use codex_history::task_context::CompletionUnitKind;
use codex_history::task_context::UnitStatus;
use codex_protocol::ThreadId;
use codex_protocol::protocol::AgentStatus;
use serde_json::Value;

pub(crate) const COMPLETION_REVIEWER_SLA_MS: i64 = 10 * 60 * 1000;

#[derive(Clone, Debug)]
pub(crate) struct CompletionReviewLaunch {
    pub(crate) root_task_id: String,
    pub(crate) unit_id: String,
    pub(crate) target_task_id: String,
    pub(crate) target_task_revision: u64,
    pub(crate) target_artifact_ref: String,
    pub(crate) review_brief: String,
    pub(crate) status: UnitStatus,
    pub(crate) reviewer_thread_id: Option<String>,
    pub(crate) reviewer_turn_id: Option<String>,
    pub(crate) spawn_call_id: Option<String>,
    pub(crate) reviewer_role: Option<String>,
    pub(crate) deadline_escalation: Option<String>,
    pub(crate) unresolved_launch: Option<CompletionReviewLaunchState>,
    pub(crate) should_spawn: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct CompletionReviewAdoption {
    pub(crate) root_task_id: String,
    pub(crate) unit_id: String,
    pub(crate) target_task_id: String,
    pub(crate) target_task_revision: u64,
    pub(crate) reviewer_thread_id: String,
}

fn terminal(value: &Value) -> bool {
    serde_json::from_value::<AgentStatus>(value.clone()).is_ok_and(|status| {
        matches!(
            status,
            AgentStatus::Completed(_) | AgentStatus::Errored(_) | AgentStatus::Shutdown
        )
    })
}

fn status_matches_review_generation(
    review: &CompletionReviewEvidence,
    child_id: ThreadId,
    turn_id: Option<&str>,
    status: &AgentStatus,
) -> bool {
    ThreadId::try_from(review.reviewer_thread_id.as_str()).is_ok_and(|reviewer_id| {
        reviewer_id == child_id
            && turn_id == Some(review.reviewer_turn_id.as_str())
            && matches!(
                status,
                AgentStatus::Completed(_) | AgentStatus::Errored(_) | AgentStatus::Shutdown
            )
    })
}

fn completion_review_needs_spawn(unit: &CompletionUnit) -> bool {
    matches!(unit.status, UnitStatus::Planned | UnitStatus::Ready) && unit.review_evidence.is_none()
}

fn pending_completion_review_evidence(
    reviewer_thread_id: ThreadId,
    reviewer_role: String,
    spawn_call_id: String,
    target_task_id: String,
    target_task_revision: u64,
    target_artifact_ref: String,
    deadline_unix_ms: Option<i64>,
) -> CompletionReviewEvidence {
    CompletionReviewEvidence {
        reviewer_thread_id: reviewer_thread_id.to_string(),
        reviewer_turn_id: format!("pending-generation:{spawn_call_id}"),
        reviewer_role,
        spawn_call_id,
        deadline_unix_ms,
        deadline_escalation: None,
        deadline_safety_fingerprint: None,
        wait_result_ref: None,
        terminal_status: None,
        wait_call_id: None,
        target_task_id,
        target_task_revision,
        target_artifact_ref,
    }
}

fn bind_completion_review_generation(
    evidence: &mut CompletionReviewEvidence,
    reviewer_thread_id: ThreadId,
    spawn_call_id: &str,
    reviewer_turn_id: String,
) -> bool {
    if evidence.reviewer_thread_id != reviewer_thread_id.to_string()
        || evidence.spawn_call_id != spawn_call_id
        || !evidence.reviewer_turn_id.starts_with("pending-generation:")
    {
        return false;
    }
    evidence.reviewer_turn_id = reviewer_turn_id;
    true
}

impl Session {
    /// Resolve one plan-declared review unit for the latest accepted task revision.
    /// A running unit without evidence is a conservative unresolved-spawn marker: a
    /// repeated review request returns it instead of spawning a duplicate child.
    pub(crate) async fn prepare_completion_review_launch(
        &self,
        target_task_id: &str,
        launch_call_id: &str,
    ) -> Result<CompletionReviewLaunch, String> {
        let (root_task_id, _) = self
            .autonomous_completion_current_state()
            .await
            .ok_or_else(|| "completion review requires a registered completion plan".to_string())?;
        let task_state = self
            .latest_rencrow_task_state()
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "review target has no accepted task receipt".to_string())?;
        if task_state.boundary.root_task_id != root_task_id {
            return Err("review target receipt belongs to a different completion root".into());
        }
        let accepted = task_state
            .completed
            .get(target_task_id)
            .ok_or_else(|| "review target has no accepted task receipt".to_string())?;
        let target_task_revision = accepted.revision;
        let target_artifact_ref = self
            .current_completion_review_target_ref(target_task_id, target_task_revision)
            .await?;

        let launch = {
            let mut state = self.state.lock().await;
            if state.rencrow_completion_paused {
                return Err(
                    "completion review is blocked while autonomous completion is paused".into(),
                );
            }
            let task_check_snapshot = state.task_check_ledger.snapshot();
            let Some((active_root, contract)) = state.rencrow_completion_state.as_mut() else {
                return Err("completion plan disappeared before review launch".into());
            };
            if active_root != &root_task_id || contract.paused {
                return Err("completion review root changed or is paused".into());
            }
            let exact = contract
                .units
                .iter()
                .enumerate()
                .filter(|(_, unit)| {
                    unit.kind == CompletionUnitKind::Review
                        && unit.review_target.as_ref().is_some_and(|target| {
                            target.task_id == target_task_id
                                && target.task_revision == target_task_revision
                        })
                })
                .map(|(index, _)| index)
                .collect::<Vec<_>>();
            if exact.len() != 1 {
                let has_done_review_for_task = contract.units.iter().any(|unit| {
                    unit.kind == CompletionUnitKind::Review
                        && unit.status == UnitStatus::Done
                        && unit
                            .review_target
                            .as_ref()
                            .is_some_and(|target| target.task_id == target_task_id)
                });
                return Err(if has_done_review_for_task {
                    format!(
                        "task {target_task_id} already has an adopted review; a new review requires a new host-authorized plan"
                    )
                } else if exact.is_empty() {
                    format!(
                        "no planned review unit matches accepted task {target_task_id}@{target_task_revision}; add it to the initial completion plan"
                    )
                } else {
                    format!(
                        "multiple planned review units match accepted task {target_task_id}@{target_task_revision}; review plan is ambiguous"
                    )
                });
            }
            let review_index = exact[0];
            let review_unit = &contract.units[review_index];
            let mut brief = format!(
                "Review purpose: {}\nAccepted task result: {}\nAccepted artifact ref: {}",
                review_unit.purpose, accepted.result, target_artifact_ref
            );
            for condition_id in review_unit.condition_refs.iter().take(8) {
                if let Some(condition) = contract.conditions.iter().find(|c| c.id == *condition_id)
                {
                    brief.push_str(&format!(
                        "\nAcceptance condition {}: result={}; scope={}; verification={}; criteria={}",
                        condition.id,
                        condition.result,
                        condition.scope,
                        condition.verification,
                        condition.acceptance_criteria
                    ));
                }
            }
            let check = task_check_snapshot.into_iter().find(|entry| {
                entry.task_id == target_task_id && entry.task_revision == target_task_revision
            });
            if let Some(check) = check
                && let crate::task_check::TaskCheckAttemptStatus::Finished(receipt) =
                    check.attempt.status
            {
                brief.push_str(&format!(
                    "\nHost verification: call_id={}; exit_code={}; succeeded={}",
                    receipt.call_id, receipt.exit_code, receipt.succeeded
                ));
                for artifact in receipt.artifact_after.iter().take(12) {
                    let path = artifact
                        .path
                        .strip_prefix(&receipt.workspace_root)
                        .unwrap_or(&artifact.path)
                        .to_string_lossy();
                    brief.push_str(&format!("\nVerified artifact path: {path}"));
                }
            }
            if brief.chars().count() > 7_000 {
                brief = brief.chars().take(7_000).collect();
            }
            let dependencies_ready =
                contract.units[review_index]
                    .depends_on
                    .iter()
                    .all(|dependency| {
                        contract
                            .units
                            .iter()
                            .find(|candidate| candidate.id == *dependency)
                            .is_some_and(|candidate| candidate.status == UnitStatus::Done)
                    });
            let unit = &mut contract.units[review_index];
            if unit.status == UnitStatus::Planned && dependencies_ready {
                unit.status = UnitStatus::Ready;
            }
            if unit.status == UnitStatus::Planned && !dependencies_ready {
                return Err(format!(
                    "planned review unit {} is waiting for its declared dependencies; accept those units first",
                    unit.id
                ));
            }
            let reviewer_thread_id = unit
                .review_evidence
                .as_ref()
                .map(|evidence| evidence.reviewer_thread_id.clone());
            let reviewer_role = unit
                .review_evidence
                .as_ref()
                .map(|evidence| evidence.reviewer_role.clone());
            let deadline_escalation = unit
                .review_evidence
                .as_ref()
                .and_then(|evidence| evidence.deadline_escalation.clone());
            let unresolved_launch = unit.review_launch.clone();
            let reviewer_turn_id = unit
                .review_evidence
                .as_ref()
                .map(|evidence| evidence.reviewer_turn_id.clone());
            let spawn_call_id = unit
                .review_evidence
                .as_ref()
                .map(|evidence| evidence.spawn_call_id.clone())
                .or_else(|| {
                    unit.review_launch
                        .as_ref()
                        .map(|launch| launch.spawn_call_id.clone())
                });
            let should_spawn = match unit.status {
                UnitStatus::Planned | UnitStatus::Ready if completion_review_needs_spawn(unit) => {
                    unit.status = UnitStatus::Running;
                    unit.review_launch = Some(CompletionReviewLaunchState {
                        spawn_call_id: launch_call_id.to_string(),
                        status: CompletionReviewLaunchStatus::Reserved,
                        deadline_unix_ms: Some(
                            chrono::Utc::now().timestamp_millis() + COMPLETION_REVIEWER_SLA_MS,
                        ),
                        reason: None,
                    });
                    contract.current_unit = Some(unit.id.clone());
                    contract.activity_at = Some(chrono::Utc::now().timestamp());
                    true
                }
                UnitStatus::Running
                | UnitStatus::Ready
                | UnitStatus::Done
                | UnitStatus::Blocked
                | UnitStatus::Failed => false,
                _ => {
                    return Err(format!(
                        "review unit {} is not available for launch ({:?})",
                        unit.id, unit.status
                    ));
                }
            };
            CompletionReviewLaunch {
                root_task_id,
                unit_id: unit.id.clone(),
                target_task_id: target_task_id.to_string(),
                target_task_revision,
                target_artifact_ref: target_artifact_ref.clone(),
                review_brief: brief,
                status: unit.status,
                reviewer_thread_id,
                reviewer_turn_id,
                spawn_call_id,
                reviewer_role,
                deadline_escalation,
                unresolved_launch: unit.review_launch.clone().or(unresolved_launch),
                should_spawn,
            }
        };
        if launch.should_spawn
            && let Err(error) = self.persist_autonomous_completion_snapshot().await
        {
            self.pause_autonomous_completion().await;
            return Err(format!(
                "failed to persist review launch reservation: {error}"
            ));
        }
        Ok(launch)
    }

    /// Undo only a reservation whose spawn was rejected by a typed capacity error.
    /// Other spawn errors may have happened after thread creation and stay unresolved.
    pub(crate) async fn rollback_unstarted_completion_review(
        &self,
        unit_id: &str,
    ) -> Result<(), String> {
        let changed = {
            let mut state = self.state.lock().await;
            let Some((_, contract)) = state.rencrow_completion_state.as_mut() else {
                return Err("completion review contract disappeared before rollback".into());
            };
            let unit = contract
                .units
                .iter_mut()
                .find(|unit| unit.id == unit_id)
                .ok_or_else(|| "completion review unit disappeared before rollback".to_string())?;
            if unit.kind != CompletionUnitKind::Review
                || unit.status != UnitStatus::Running
                || unit.review_evidence.is_some()
            {
                return Err(
                    "completion review reservation is no longer safely rollbackable".into(),
                );
            }
            unit.status = UnitStatus::Ready;
            unit.review_launch = None;
            contract.activity_at = Some(chrono::Utc::now().timestamp());
            true
        };
        if changed {
            self.persist_autonomous_completion_snapshot().await?;
        }
        Ok(())
    }

    /// Persist the real child ID immediately after spawn succeeds, before checking
    /// metadata or generation. A sentinel generation keeps the launch unresolved.
    pub(crate) async fn record_completion_review_child_pending(
        &self,
        unit_id: &str,
        target_task_id: &str,
        target_task_revision: u64,
        target_artifact_ref: String,
        reviewer_thread_id: ThreadId,
        reviewer_role: String,
        spawn_call_id: String,
    ) -> Result<(), String> {
        {
            let mut state = self.state.lock().await;
            let Some((_, contract)) = state.rencrow_completion_state.as_mut() else {
                return Err("completion review contract disappeared after child spawn".into());
            };
            let unit = contract
                .units
                .iter_mut()
                .find(|unit| unit.id == unit_id)
                .ok_or_else(|| {
                    "completion review unit disappeared after child spawn".to_string()
                })?;
            if unit.kind != CompletionUnitKind::Review
                || unit.status != UnitStatus::Running
                || unit.review_evidence.is_some()
                || unit
                    .review_launch
                    .as_ref()
                    .is_none_or(|launch| launch.spawn_call_id != spawn_call_id)
                || unit.review_target.as_ref().is_none_or(|target| {
                    target.task_id != target_task_id || target.task_revision != target_task_revision
                })
            {
                return Err("completion review launch changed after child spawn".into());
            }
            let deadline_unix_ms = unit
                .review_launch
                .as_ref()
                .and_then(|launch| launch.deadline_unix_ms);
            unit.review_evidence = Some(pending_completion_review_evidence(
                reviewer_thread_id,
                reviewer_role,
                spawn_call_id,
                target_task_id.to_string(),
                target_task_revision,
                target_artifact_ref,
                deadline_unix_ms,
            ));
            unit.review_launch = None;
        }
        self.persist_autonomous_completion_snapshot().await
    }

    pub(crate) async fn mark_completion_review_launch_unknown(
        &self,
        unit_id: &str,
        spawn_call_id: &str,
        reason: String,
    ) -> Result<(), String> {
        {
            let mut state = self.state.lock().await;
            let Some((_, contract)) = state.rencrow_completion_state.as_mut() else {
                return Err(
                    "completion review contract disappeared while recording unknown launch".into(),
                );
            };
            let unit = contract
                .units
                .iter_mut()
                .find(|unit| unit.id == unit_id)
                .ok_or_else(|| {
                    "completion review unit disappeared while recording unknown launch".to_string()
                })?;
            if unit.kind != CompletionUnitKind::Review
                || unit.status != UnitStatus::Running
                || unit.review_evidence.is_some()
            {
                return Err(
                    "completion review launch changed before unknown outcome was recorded".into(),
                );
            }
            let launch = unit.review_launch.as_mut().ok_or_else(|| {
                "completion review launch reservation disappeared before reconciliation".to_string()
            })?;
            if launch.spawn_call_id != spawn_call_id {
                return Err(
                    "completion review launch changed before unknown outcome was recorded".into(),
                );
            }
            launch.status = CompletionReviewLaunchStatus::Unknown;
            launch.reason = Some(reason);
            contract.activity_at = Some(chrono::Utc::now().timestamp());
        }
        self.persist_autonomous_completion_snapshot().await
    }

    /// Adopt only the exact terminal review generation collected by native wait_agent.
    pub(crate) async fn adopt_completion_review(
        &self,
        unit_id: &str,
        reason: String,
    ) -> Result<CompletionReviewAdoption, String> {
        if reason.trim().is_empty() {
            return Err("review adoption requires a non-empty parent decision reason".into());
        }
        let (root_task_id, unit) = {
            let state = self.state.lock().await;
            if state.rencrow_completion_paused {
                return Err(
                    "review adoption is blocked while autonomous completion is paused".into(),
                );
            }
            let (root, contract) = state
                .rencrow_completion_state
                .as_ref()
                .ok_or_else(|| "no completion plan is registered".to_string())?;
            if contract.paused {
                return Err(
                    "review adoption is blocked while autonomous completion is paused".into(),
                );
            }
            let unit = contract
                .units
                .iter()
                .find(|unit| unit.id == unit_id)
                .cloned()
                .ok_or_else(|| "review unit does not exist in the completion plan".to_string())?;
            (root.clone(), unit)
        };
        if unit.kind != CompletionUnitKind::Review {
            return Err("only a planned review unit can be adopted".into());
        }
        if unit.status == UnitStatus::Done {
            if unit.review_decision_reason.as_deref() == Some(reason.as_str()) {
                let evidence = unit
                    .review_evidence
                    .as_ref()
                    .ok_or_else(|| "adopted review is missing host evidence".to_string())?;
                return Ok(CompletionReviewAdoption {
                    root_task_id,
                    unit_id: unit.id,
                    target_task_id: evidence.target_task_id.clone(),
                    target_task_revision: evidence.target_task_revision,
                    reviewer_thread_id: evidence.reviewer_thread_id.clone(),
                });
            }
            return Err("an adopted review cannot be changed or repeated without a new host-authorized plan".into());
        }
        let evidence = unit
            .review_evidence
            .clone()
            .ok_or_else(|| "review has not been spawned by the host review action".to_string())?;
        if evidence
            .wait_result_ref
            .as_deref()
            .is_none_or(str::is_empty)
            || evidence.wait_call_id.as_deref().is_none_or(str::is_empty)
            || evidence.terminal_status != Some(CompletionReviewTerminalStatus::Completed)
        {
            return Err(
                "review result has not been collected from the same completed child generation"
                    .into(),
            );
        }
        let target_ref = self
            .current_completion_review_target_ref(
                &evidence.target_task_id,
                evidence.target_task_revision,
            )
            .await?;
        if target_ref != evidence.target_artifact_ref {
            return Err("review target changed after the reviewer completed".into());
        }
        let adopted = {
            let mut state = self.state.lock().await;
            if state.rencrow_completion_paused {
                return Err("review adoption was paused before commit".into());
            }
            let (root, contract) = state
                .rencrow_completion_state
                .as_mut()
                .ok_or_else(|| "completion plan disappeared before review adoption".to_string())?;
            if root != &root_task_id || contract.paused {
                return Err("completion plan changed before review adoption".into());
            }
            let adopted_unit_id = {
                let current = contract
                    .units
                    .iter_mut()
                    .find(|candidate| candidate.id == unit_id)
                    .ok_or_else(|| "review unit disappeared before adoption".to_string())?;
                if current != &unit || current.review_evidence.as_ref() != Some(&evidence) {
                    return Err("review evidence changed before adoption".into());
                }
                current.status = UnitStatus::Done;
                current.review_decision_reason = Some(reason);
                current.id.clone()
            };
            let done = contract
                .units
                .iter()
                .filter(|candidate| candidate.status == UnitStatus::Done)
                .map(|candidate| candidate.id.as_str())
                .collect::<std::collections::HashSet<_>>();
            contract.current_unit = contract
                .units
                .iter()
                .find(|candidate| {
                    matches!(candidate.status, UnitStatus::Planned | UnitStatus::Ready)
                        && candidate
                            .depends_on
                            .iter()
                            .all(|id| done.contains(id.as_str()))
                })
                .map(|candidate| candidate.id.clone());
            contract.activity_at = Some(chrono::Utc::now().timestamp());
            CompletionReviewAdoption {
                root_task_id,
                unit_id: adopted_unit_id,
                target_task_id: evidence.target_task_id.clone(),
                target_task_revision: evidence.target_task_revision,
                reviewer_thread_id: evidence.reviewer_thread_id.clone(),
            }
        };
        if let Err(error) = self.persist_autonomous_completion_snapshot().await {
            self.pause_autonomous_completion().await;
            return Err(format!(
                "review adoption commit failed and autonomous work was paused: {error}"
            ));
        }
        Ok(adopted)
    }

    pub(crate) async fn revalidate_collected_completion_reviews(&self) {
        let collected = {
            let state = self.state.lock().await;
            state
                .rencrow_completion_state
                .as_ref()
                .map(|(_, contract)| {
                    contract
                        .units
                        .iter()
                        .filter_map(|unit| {
                            // Adopted review is historical evidence for its target revision.
                            // Later implementation work may legitimately change the workspace;
                            // current acceptance conditions are rechecked against their own
                            // task-check evidence instead of forcing the old review to repeat.
                            if unit.status == UnitStatus::Done {
                                return None;
                            }
                            let evidence = unit.review_evidence.as_ref()?;
                            evidence.wait_result_ref.as_ref()?;
                            Some((unit.id.clone(), evidence.clone()))
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        };
        for (unit_id, evidence) in collected {
            if self
                .current_completion_review_target_ref(
                    &evidence.target_task_id,
                    evidence.target_task_revision,
                )
                .await
                .is_ok_and(|current| current == evidence.target_artifact_ref)
            {
                continue;
            }
            let changed = {
                let mut state = self.state.lock().await;
                let Some((_, contract)) = state.rencrow_completion_state.as_mut() else {
                    return;
                };
                let Some(unit) = contract.units.iter_mut().find(|unit| unit.id == unit_id) else {
                    continue;
                };
                if unit.review_evidence.as_ref() == Some(&evidence) {
                    unit.status = UnitStatus::Ready;
                    unit.review_evidence = None;
                    unit.review_decision_reason = None;
                    contract.current_unit = Some(unit_id);
                    true
                } else {
                    false
                }
            };
            if changed && self.persist_autonomous_completion_snapshot().await.is_err() {
                self.pause_autonomous_completion().await;
                return;
            }
        }
    }

    pub(crate) async fn validate_completion_review_spawn(
        &self,
        unit_id: &str,
        target_task_id: &str,
        target_task_revision: u64,
    ) -> Result<String, String> {
        let Some((_, contract)) = self.autonomous_completion_current_state().await else {
            return Err("completion review requires an active completion contract".into());
        };
        if contract.paused {
            return Err(
                "completion review is blocked while autonomous completion is paused".into(),
            );
        }
        let unit = contract
            .units
            .iter()
            .find(|unit| unit.id == unit_id)
            .ok_or_else(|| "completion review unit is missing".to_string())?;
        if unit.kind != CompletionUnitKind::Review
            || !matches!(unit.status, UnitStatus::Ready | UnitStatus::Running)
            || unit.review_target.as_ref().is_none_or(|target| {
                target.task_id != target_task_id || target.task_revision != target_task_revision
            })
            || unit.review_evidence.is_some()
        {
            return Err("completion review request does not match an available review unit".into());
        }
        self.current_completion_review_target_ref(target_task_id, target_task_revision)
            .await
    }

    pub(crate) async fn reserve_completion_review_launch(
        &self,
        unit_id: &str,
        target_task_id: &str,
        target_task_revision: u64,
        spawn_call_id: &str,
    ) -> Result<String, String> {
        let target_ref = self
            .validate_completion_review_spawn(unit_id, target_task_id, target_task_revision)
            .await?;
        {
            let mut state = self.state.lock().await;
            if state.rencrow_completion_paused {
                return Err(
                    "completion review is blocked while autonomous completion is paused".into(),
                );
            }
            let (_, contract) = state
                .rencrow_completion_state
                .as_mut()
                .ok_or_else(|| "completion plan disappeared before review launch".to_string())?;
            if contract.paused {
                return Err(
                    "completion review is blocked while autonomous completion is paused".into(),
                );
            }
            let unit = contract
                .units
                .iter_mut()
                .find(|unit| unit.id == unit_id)
                .ok_or_else(|| "completion review unit is missing".to_string())?;
            if unit.kind != CompletionUnitKind::Review
                || unit.status != UnitStatus::Ready
                || unit.review_evidence.is_some()
                || unit.review_launch.is_some()
                || unit.review_target.as_ref().is_none_or(|target| {
                    target.task_id != target_task_id || target.task_revision != target_task_revision
                })
            {
                return Err("completion review launch is already reserved or unresolved".into());
            }
            unit.status = UnitStatus::Running;
            unit.review_launch = Some(CompletionReviewLaunchState {
                spawn_call_id: spawn_call_id.to_string(),
                status: CompletionReviewLaunchStatus::Reserved,
                deadline_unix_ms: Some(
                    chrono::Utc::now().timestamp_millis() + COMPLETION_REVIEWER_SLA_MS,
                ),
                reason: None,
            });
            contract.current_unit = Some(unit_id.to_string());
            contract.activity_at = Some(chrono::Utc::now().timestamp());
        }
        if let Err(error) = self.persist_autonomous_completion_snapshot().await {
            self.pause_autonomous_completion().await;
            return Err(format!(
                "failed to persist completion review launch reservation: {error}"
            ));
        }
        Ok(target_ref)
    }

    pub(crate) async fn record_completion_review_spawn(
        &self,
        unit_id: &str,
        target_task_id: &str,
        target_task_revision: u64,
        target_artifact_ref: String,
        reviewer_thread_id: ThreadId,
        reviewer_turn_id: String,
        reviewer_role: String,
        spawn_call_id: String,
    ) -> Result<(), String> {
        if self
            .current_completion_review_target_ref(target_task_id, target_task_revision)
            .await?
            != target_artifact_ref
        {
            return Err("review target artifact changed while the reviewer was spawning".into());
        }
        {
            let mut state = self.state.lock().await;
            let Some((_, contract)) = state.rencrow_completion_state.as_mut() else {
                return Err("completion review contract disappeared during spawn".into());
            };
            if contract.paused {
                return Err("completion review was paused during spawn".into());
            }
            let unit = contract
                .units
                .iter_mut()
                .find(|unit| unit.id == unit_id)
                .ok_or_else(|| "completion review unit disappeared during spawn".to_string())?;
            if unit.kind != CompletionUnitKind::Review
                || !matches!(unit.status, UnitStatus::Ready | UnitStatus::Running)
                || unit.review_target.as_ref().is_none_or(|target| {
                    target.task_id != target_task_id || target.task_revision != target_task_revision
                })
                || unit.review_evidence.as_ref().is_some_and(|evidence| {
                    evidence.reviewer_thread_id != reviewer_thread_id.to_string()
                        || evidence.spawn_call_id != spawn_call_id
                        || !evidence.reviewer_turn_id.starts_with("pending-generation:")
                })
                || !(unit
                    .review_launch
                    .as_ref()
                    .is_some_and(|launch| launch.spawn_call_id == spawn_call_id)
                    || unit.review_evidence.as_ref().is_some_and(|evidence| {
                        evidence.reviewer_thread_id == reviewer_thread_id.to_string()
                            && evidence.spawn_call_id == spawn_call_id
                            && evidence.reviewer_turn_id.starts_with("pending-generation:")
                    }))
            {
                return Err(
                    "completion review unit changed while the reviewer was spawning".into(),
                );
            }
            let launch_deadline = unit
                .review_launch
                .as_ref()
                .and_then(|launch| launch.deadline_unix_ms);
            let evidence = unit.review_evidence.get_or_insert_with(|| {
                pending_completion_review_evidence(
                    reviewer_thread_id,
                    reviewer_role.clone(),
                    spawn_call_id.clone(),
                    target_task_id.to_string(),
                    target_task_revision,
                    target_artifact_ref.clone(),
                    launch_deadline,
                )
            });
            if !bind_completion_review_generation(
                evidence,
                reviewer_thread_id,
                &spawn_call_id,
                reviewer_turn_id,
            ) {
                return Err("review generation does not match the unresolved child launch".into());
            }
            evidence.reviewer_role = reviewer_role;
            evidence.target_artifact_ref = target_artifact_ref;
            unit.review_launch = None;
        }
        self.persist_autonomous_completion_snapshot().await
    }

    /// Records only terminal results returned by the parent's native wait_agent call.
    pub(crate) async fn collect_completion_review_results(
        &self,
        wait_call_id: &str,
        statuses: &[(ThreadId, Option<String>, AgentStatus)],
    ) {
        let pending = {
            let state = self.state.lock().await;
            let Some((_, contract)) = state.rencrow_completion_state.as_ref() else {
                return;
            };
            contract
                .units
                .iter()
                .filter_map(|unit| {
                    let review = unit.review_evidence.as_ref()?;
                    review
                        .wait_result_ref
                        .is_none()
                        .then(|| (unit.id.clone(), review.clone()))
                })
                .collect::<Vec<_>>()
        };
        for (unit_id, mut review) in pending {
            let launch_pending = review.reviewer_turn_id.starts_with("pending-generation:");
            let pending_after = review.reviewer_turn_id.strip_prefix("pending-after:");
            let reviewer_id = ThreadId::try_from(review.reviewer_thread_id.as_str()).ok();
            let Some(status) = statuses.iter().find(|(thread_id, turn_id, status)| {
                reviewer_id == Some(*thread_id)
                    && if launch_pending {
                        false
                    } else if let Some(previous_turn) = pending_after {
                        matches!(status, AgentStatus::Shutdown)
                            || (turn_id
                                .as_deref()
                                .is_some_and(|current| current != previous_turn)
                                && matches!(
                                    status,
                                    AgentStatus::Completed(_) | AgentStatus::Errored(_)
                                ))
                    } else {
                        status_matches_review_generation(
                            &review,
                            *thread_id,
                            turn_id.as_deref(),
                            status,
                        )
                    }
            }) else {
                continue;
            };
            let expected_binding = review.reviewer_turn_id.clone();
            if let Some(turn_id) = status.1.as_ref()
                && pending_after.is_some()
            {
                review.reviewer_turn_id = turn_id.clone();
            }
            match &status.2 {
                AgentStatus::Completed(Some(_)) => {}
                AgentStatus::Completed(None) | AgentStatus::Errored(_) | AgentStatus::Shutdown => {
                    let (cause, action) = match &status.2 {
                        AgentStatus::Completed(None) => (
                            "reviewer completed without a result summary",
                            "Use a new host-authorized review plan if another review is needed",
                        ),
                        AgentStatus::Errored(_) => (
                            "reviewer turn ended with an error",
                            "Diagnose the reviewer failure before creating a new authorized plan",
                        ),
                        AgentStatus::Shutdown => (
                            "reviewer was shut down before returning a result",
                            "Resolve the shutdown cause before creating a new authorized plan",
                        ),
                        _ => unreachable!(),
                    };
                    let changed = {
                        let mut state = self.state.lock().await;
                        let Some((_, contract)) = state.rencrow_completion_state.as_mut() else {
                            return;
                        };
                        let Some(unit) = contract.units.iter_mut().find(|u| u.id == unit_id) else {
                            continue;
                        };
                        if unit.status != UnitStatus::Running
                            || unit.review_evidence.as_ref().is_none_or(|current| {
                                current.reviewer_thread_id != review.reviewer_thread_id
                                    || current.reviewer_turn_id != expected_binding
                                    || current.spawn_call_id != review.spawn_call_id
                            })
                        {
                            false
                        } else {
                            unit.status = UnitStatus::Failed;
                            contract.failed_approaches.push(
                                codex_history::task_context::FailedApproach {
                                    summary: format!("review unit {} failed", unit.id),
                                    cause: cause.into(),
                                    next_action: action.into(),
                                    recorded_at: chrono::Utc::now().timestamp(),
                                },
                            );
                            true
                        }
                    };
                    if changed && self.persist_autonomous_completion_snapshot().await.is_err() {
                        self.pause_autonomous_completion().await;
                        return;
                    }
                    continue;
                }
                _ => continue,
            }
            let AgentStatus::Completed(Some(summary)) = &status.2 else {
                continue;
            };
            let current = self
                .current_completion_review_target_ref(
                    &review.target_task_id,
                    review.target_task_revision,
                )
                .await;
            if !matches!(
                current.as_deref(),
                Ok(current_ref) if current_ref == review.target_artifact_ref
            ) {
                continue;
            }
            let Ok(result_ref) = digest(&serde_json::json!({
                "wait_call_id": wait_call_id,
                "reviewer_thread_id": review.reviewer_thread_id,
                "reviewer_turn_id": review.reviewer_turn_id,
                "status": "completed",
                "summary": summary,
            })) else {
                continue;
            };
            review.wait_result_ref = Some(result_ref);
            review.terminal_status = Some(CompletionReviewTerminalStatus::Completed);
            review.wait_call_id = Some(wait_call_id.to_string());
            let changed = {
                let mut state = self.state.lock().await;
                let Some((_, contract)) = state.rencrow_completion_state.as_mut() else {
                    return;
                };
                let paused = contract.paused;
                let Some(unit) = contract.units.iter_mut().find(|unit| unit.id == unit_id) else {
                    continue;
                };
                if unit.review_evidence.as_ref().is_some_and(|current| {
                    current.reviewer_thread_id == review.reviewer_thread_id
                        && current.reviewer_turn_id == expected_binding
                        && current.spawn_call_id == review.spawn_call_id
                        && current.wait_result_ref.is_none()
                        && !paused
                }) {
                    unit.review_evidence = Some(review);
                    true
                } else {
                    false
                }
            };
            if changed && self.persist_autonomous_completion_snapshot().await.is_err() {
                self.pause_autonomous_completion().await;
                return;
            }
        }
    }

    async fn current_completion_review_target_ref(
        &self,
        task_id: &str,
        task_revision: u64,
    ) -> Result<String, String> {
        let active_root = self
            .autonomous_completion_current_state()
            .await
            .map(|(root, _)| root)
            .ok_or_else(|| "completion review contract is missing".to_string())?;
        let task_state = self
            .latest_rencrow_task_state()
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "review target has no accepted task receipt".to_string())?;
        if task_state.boundary.root_task_id != active_root {
            return Err("review target receipt belongs to a different completion root".into());
        }
        let accepted = task_state
            .completed
            .get(task_id)
            .cloned()
            .ok_or_else(|| "review target has no accepted task receipt".to_string())?;
        if accepted.revision != task_revision {
            return Err("review target task revision is no longer current".into());
        }
        let attempt = self
            .latest_task_check_for_scope(task_id, task_revision)
            .await
            .ok_or_else(|| "review target has no current host verification".to_string())?;
        let receipt = match attempt.status {
            crate::task_check::TaskCheckAttemptStatus::Finished(receipt) => receipt,
            crate::task_check::TaskCheckAttemptStatus::Pending => {
                return Err("review target verification is still running".into());
            }
            crate::task_check::TaskCheckAttemptStatus::ChangedArtifacts => {
                return Err("review target verification is stale".into());
            }
        };
        let host_ref = format!(
            "task-check:{}:{}",
            receipt.call_id,
            task_check_artifact_fingerprint(&receipt)?
        );
        if !receipt.succeeded
            || receipt.exit_code != 0
            || receipt.artifact_before != receipt.artifact_after
            || accepted.artifact_ref != host_ref
            || accepted.verification_ref != host_ref
        {
            return Err("review target does not match its accepted stable verification".into());
        }
        let paths = receipt
            .artifact_after
            .iter()
            .map(|stamp| stamp.path.clone())
            .collect::<Vec<_>>();
        let current = crate::task_check::hash_workspace_artifacts(&receipt.workspace_root, &paths)
            .ok_or_else(|| "review target artifacts are unavailable".to_string())?;
        if current != receipt.artifact_after {
            return Err("review target artifacts changed after verification".into());
        }
        Ok(accepted.artifact_ref)
    }

    pub(crate) async fn has_uncollected_completion_children(&self) -> bool {
        !self
            .state
            .lock()
            .await
            .rencrow_completion_uncollected_children
            .is_empty()
    }

    pub(crate) async fn observe_completion_child_tool(
        &self,
        tool: &str,
        args: &Value,
        value: &Value,
    ) {
        let mut add = Vec::new();
        let mut remove = Vec::new();
        let mut invalidate_review_children = Vec::new();
        match tool {
            "spawn_agent" => add.extend(value["agent_id"].as_str()),
            "send_input" => {
                add.extend(args["target"].as_str());
                invalidate_review_children.extend(args["target"].as_str());
            }
            "resume_agent" if terminal(&value["status"]) => {
                remove.extend(args["id"].as_str());
                invalidate_review_children.extend(args["id"].as_str());
            }
            "resume_agent" => {
                add.extend(args["id"].as_str());
                invalidate_review_children.extend(args["id"].as_str());
            }
            "close_agent" => {
                remove.extend(args["target"].as_str());
                invalidate_review_children.extend(args["target"].as_str());
            }
            "wait_agent" => {
                if let Some(statuses) = value["status"].as_object() {
                    for (id, status) in statuses {
                        if terminal(status) {
                            remove.push(id.as_str());
                        }
                    }
                }
            }
            _ => return,
        }
        let rebind_generations =
            futures::future::join_all(invalidate_review_children.iter().filter_map(|id| {
                let thread_id = ThreadId::try_from(*id).ok()?;
                Some(async move {
                    let generation = self
                        .services
                        .agent_control
                        .get_agent_status_generation(thread_id)
                        .await
                        .ok()
                        .and_then(|(turn_id, _)| turn_id);
                    (thread_id.to_string(), generation)
                })
            }))
            .await
            .into_iter()
            .collect::<std::collections::HashMap<_, _>>();
        let changed = {
            let mut state = self.state.lock().await;
            let mut changed = false;
            for id in add {
                if codex_protocol::ThreadId::try_from(id).is_ok() {
                    changed |= state
                        .rencrow_completion_uncollected_children
                        .insert(id.to_owned());
                }
            }
            let mut collected = false;
            for id in remove {
                collected |= state.rencrow_completion_uncollected_children.remove(id);
            }
            let rebind_ids = invalidate_review_children
                .into_iter()
                .filter_map(|id| ThreadId::try_from(id).ok())
                .map(|id| id.to_string())
                .collect::<std::collections::HashSet<_>>();
            if !rebind_ids.is_empty()
                && let Some((_, contract)) = state.rencrow_completion_state.as_mut()
            {
                for unit in &mut contract.units {
                    let Some(review_thread_id) = unit
                        .review_evidence
                        .as_ref()
                        .map(|review| review.reviewer_thread_id.clone())
                    else {
                        continue;
                    };
                    if unit.status != UnitStatus::Done && rebind_ids.contains(&review_thread_id) {
                        let already_collected_success =
                            unit.review_evidence.as_ref().is_some_and(|evidence| {
                                evidence.terminal_status
                                    == Some(CompletionReviewTerminalStatus::Completed)
                                    && evidence
                                        .wait_result_ref
                                        .as_ref()
                                        .is_some_and(|result_ref| !result_ref.trim().is_empty())
                                    && evidence
                                        .wait_call_id
                                        .as_ref()
                                        .is_some_and(|call_id| !call_id.trim().is_empty())
                            });
                        if tool == "close_agent" && already_collected_success {
                            continue;
                        }
                        let already_invalidated =
                            unit.review_evidence.as_ref().is_some_and(|evidence| {
                                evidence.reviewer_turn_id.starts_with("pending-after:")
                            });
                        if tool == "close_agent" && already_invalidated {
                            continue;
                        }
                        if let Some(evidence) = unit.review_evidence.as_mut() {
                            let previous_turn = evidence.reviewer_turn_id.clone();
                            evidence.reviewer_turn_id = rebind_generations
                                .get(&review_thread_id)
                                .and_then(Clone::clone)
                                .filter(|turn_id| turn_id != &previous_turn)
                                .unwrap_or_else(|| format!("pending-after:{previous_turn}"));
                            evidence.wait_result_ref = None;
                            evidence.terminal_status = None;
                            evidence.wait_call_id = None;
                        }
                        unit.status = UnitStatus::Running;
                        unit.review_decision_reason = None;
                        contract.current_unit = Some(unit.id.clone());
                        changed = true;
                    }
                }
            }
            if collected && let Some((_, contract)) = state.rencrow_completion_state.as_mut() {
                contract.runtime_recovery_at = Some(chrono::Utc::now().timestamp());
            }
            changed || collected
        };
        if changed && let Err(error) = self.persist_autonomous_completion_snapshot().await {
            tracing::error!(%error, "failed to persist delegated child collection state");
            self.pause_autonomous_completion().await;
        }
    }
}

fn task_check_artifact_fingerprint(
    receipt: &crate::task_check::TaskCheckReceipt,
) -> Result<String, String> {
    let artifact_stamp_set = receipt
        .artifact_after
        .iter()
        .map(|stamp| {
            serde_json::json!({
                "path": stamp.path.to_string_lossy(),
                "sha256": stamp.sha256,
            })
        })
        .collect::<Vec<_>>();
    digest(&serde_json::json!({
        "task_id": receipt.task_id,
        "task_revision": receipt.task_revision,
        "workspace_root": receipt.workspace_root.to_string_lossy(),
        "artifacts": artifact_stamp_set,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::tests::make_session_and_context;
    use codex_history::task_context::AcceptanceCondition;
    use codex_history::task_context::AcceptanceStatus;
    use codex_history::task_context::CompletionState;
    use codex_history::task_context::CompletionUnit;
    use serde_json::json;

    fn pending_review_state(child_id: &str, turn_id: &str) -> CompletionState {
        CompletionState {
            contract_revision: 1,
            units_completed_since_review: 0,
            plan_review_at: None,
            plan_reviews: vec![],
            paused: false,
            consumed_host_events: vec![],
            conditions: vec![AcceptanceCondition {
                id: "AC-1".into(),
                revision: 1,
                result: "The task is correct".into(),
                scope: "CLI".into(),
                verification: "Check the artifact".into(),
                acceptance_criteria: "Expected artifact is present".into(),
                status: AcceptanceStatus::Open,
                evidence: vec![],
                unmet_reason: None,
            }],
            units: vec![CompletionUnit {
                id: "review-1".into(),
                purpose: "Review task-a".into(),
                status: UnitStatus::Running,
                kind: CompletionUnitKind::Review,
                condition_refs: vec!["AC-1".into()],
                depends_on: vec![],
                evidence: vec![],
                task_refs: vec![],
                review_target: Some(codex_history::task_context::CompletionReviewTarget {
                    task_id: "task-a".into(),
                    task_revision: 1,
                }),
                review_evidence: Some(CompletionReviewEvidence {
                    reviewer_thread_id: child_id.into(),
                    reviewer_turn_id: turn_id.into(),
                    reviewer_role: "analyst".into(),
                    spawn_call_id: "spawn-review".into(),
                    deadline_unix_ms: None,
                    deadline_escalation: None,
                    deadline_safety_fingerprint: None,
                    wait_result_ref: Some("sha256:old-result".into()),
                    terminal_status: Some(CompletionReviewTerminalStatus::Completed),
                    wait_call_id: Some("wait-old".into()),
                    target_task_id: "task-a".into(),
                    target_task_revision: 1,
                    target_artifact_ref: "task-check:target".into(),
                }),
                review_launch: None,
                review_decision_reason: None,
            }],
            current_unit: Some("review-1".into()),
            failed_approaches: vec![],
            blocker: None,
            next_action: Some("Review task-a".into()),
            product_progress_at: None,
            runtime_recovery_at: None,
            activity_at: None,
        }
    }

    #[test]
    fn later_completed_generation_cannot_complete_bound_review() {
        let reviewer_id =
            ThreadId::try_from("01a1c606-48b8-7cf3-bb78-77b47f6b8c2f").expect("valid reviewer id");
        let review = CompletionReviewEvidence {
            reviewer_thread_id: reviewer_id.to_string(),
            reviewer_turn_id: "review-turn-original".into(),
            reviewer_role: "analyst".into(),
            spawn_call_id: "spawn-review".into(),
            deadline_unix_ms: None,
            deadline_escalation: None,
            deadline_safety_fingerprint: None,
            wait_result_ref: None,
            terminal_status: None,
            wait_call_id: None,
            target_task_id: "task-a".into(),
            target_task_revision: 1,
            target_artifact_ref: "artifact:sha".into(),
        };
        let unrelated_completion = AgentStatus::Completed(Some("unrelated follow-up".into()));

        assert!(!status_matches_review_generation(
            &review,
            reviewer_id,
            Some("later-unrelated-turn"),
            &unrelated_completion,
        ));
        assert!(status_matches_review_generation(
            &review,
            reviewer_id,
            Some("review-turn-original"),
            &unrelated_completion,
        ));
    }

    #[tokio::test]
    async fn terminal_child_is_not_collected_until_native_result_is_returned() {
        let (session, _) = make_session_and_context().await;
        let child = "01a1c606-48b8-7cf3-bb78-77b47f6b8c2f";
        session
            .observe_completion_child_tool("spawn_agent", &json!({}), &json!({"agent_id":child}))
            .await;
        assert!(session.has_uncollected_completion_children().await);
        session
            .observe_completion_child_tool(
                "wait_agent",
                &json!({}),
                &json!({"status":{child:"running"},"timed_out":true}),
            )
            .await;
        assert!(session.has_uncollected_completion_children().await);
        session
            .observe_completion_child_tool(
                "wait_agent",
                &json!({}),
                &json!({"status":{child:{"completed":"result"}},"timed_out":false}),
            )
            .await;
        assert!(!session.has_uncollected_completion_children().await);
        session
            .observe_completion_child_tool(
                "send_input",
                &json!({"target":child}),
                &json!({"submission_id":"next"}),
            )
            .await;
        assert!(
            session.has_uncollected_completion_children().await,
            "new delegation needs a new result"
        );
    }

    #[tokio::test]
    async fn reviewer_followup_invalidates_old_result_without_releasing_child_reservation() {
        let (session, _) = make_session_and_context().await;
        let child = "01a1c606-48b8-7cf3-bb78-77b47f6b8c2f";
        session.state.lock().await.rencrow_completion_state =
            Some(("root".into(), pending_review_state(child, "review-turn-1")));

        session
            .observe_completion_child_tool(
                "send_input",
                &json!({"target":child}),
                &json!({"submission_id":"followup-1"}),
            )
            .await;

        let state = session
            .rencrow_completion_state("root")
            .await
            .expect("completion plan remains registered");
        let unit = &state.units[0];
        assert_eq!(unit.status, UnitStatus::Running);
        let evidence = unit
            .review_evidence
            .as_ref()
            .expect("child binding remains");
        assert_eq!(evidence.reviewer_thread_id, child);
        assert!(evidence.wait_result_ref.is_none());
        assert!(evidence.wait_call_id.is_none());
        assert_ne!(evidence.reviewer_turn_id, "review-turn-1");
        assert!(session.has_uncollected_completion_children().await);
    }

    #[tokio::test]
    async fn close_preserves_wait_collected_completion_review_result() {
        let (session, _) = make_session_and_context().await;
        let child_id = "01a1c77f-15cf-7491-9372-cea9ad0c408a";
        let before = pending_review_state(child_id, "review-turn-1");
        session.state.lock().await.rencrow_completion_state = Some(("root".into(), before.clone()));

        session
            .observe_completion_child_tool(
                "close_agent",
                &json!({"target": child_id}),
                &json!({"previous_status": {"completed": {"message": "review result"}}}),
            )
            .await;

        let after = session.rencrow_completion_state("root").await.unwrap();
        assert_eq!(
            after.units[0].review_evidence,
            before.units[0].review_evidence
        );
        assert_eq!(after.units[0].status, UnitStatus::Running);
    }

    #[tokio::test]
    async fn close_after_followup_keeps_old_review_result_invalidated() {
        let (session, _) = make_session_and_context().await;
        let child_id = "01a1c77f-15cf-7491-9372-cea9ad0c408a";
        session.state.lock().await.rencrow_completion_state = Some((
            "root".into(),
            pending_review_state(child_id, "review-turn-1"),
        ));
        session
            .observe_completion_child_tool("send_input", &json!({"target": child_id}), &json!({}))
            .await;
        session
            .observe_completion_child_tool(
                "close_agent",
                &json!({"target": child_id}),
                &json!({"previous_status": {"completed": {"message": "review result"}}}),
            )
            .await;

        let state = session.rencrow_completion_state("root").await.unwrap();
        let evidence = state.units[0].review_evidence.as_ref().unwrap();
        assert_eq!(evidence.reviewer_turn_id, "pending-after:review-turn-1");
        assert!(evidence.wait_result_ref.is_none());
        assert!(evidence.terminal_status.is_none());
        assert!(evidence.wait_call_id.is_none());
    }

    #[tokio::test]
    async fn followup_error_generation_is_collected_and_close_needs_no_new_turn() {
        for terminal_status in [
            AgentStatus::Errored("follow-up failed".into()),
            AgentStatus::Shutdown,
        ] {
            let terminal_debug = format!("{terminal_status:?}");
            let (session, _) = make_session_and_context().await;
            let child =
                ThreadId::try_from("01a1c606-48b8-7cf3-bb78-77b47f6b8c2f").expect("valid child id");
            {
                let mut state = session.state.lock().await;
                let mut contract = pending_review_state(&child.to_string(), "pending-after:turn-1");
                contract.units[0]
                    .review_evidence
                    .as_mut()
                    .unwrap()
                    .wait_result_ref = None;
                contract.units[0]
                    .review_evidence
                    .as_mut()
                    .unwrap()
                    .terminal_status = None;
                contract.units[0]
                    .review_evidence
                    .as_mut()
                    .unwrap()
                    .wait_call_id = None;
                state.rencrow_completion_state = Some(("root".into(), contract));
            }
            let generation =
                (!matches!(&terminal_status, AgentStatus::Shutdown)).then(|| "turn-2".to_string());
            session
                .collect_completion_review_results(
                    "wait-followup-terminal",
                    &[(child, generation, terminal_status)],
                )
                .await;
            let state = session
                .rencrow_completion_state("root")
                .await
                .expect("registered review remains available");
            assert_eq!(
                state.units[0].status,
                UnitStatus::Failed,
                "terminal status={terminal_debug}, review evidence={:?}, failures={:?}",
                state.units[0].review_evidence,
                state.failed_approaches
            );
            assert!(
                state
                    .failed_approaches
                    .iter()
                    .any(|failure| { failure.summary.contains("review") })
            );
        }
    }

    #[tokio::test]
    async fn certain_unstarted_capacity_reservation_can_return_to_ready() {
        let (session, _) = make_session_and_context().await;
        let child = "01a1c606-48b8-7cf3-bb78-77b47f6b8c2f";
        {
            let mut state = session.state.lock().await;
            let mut contract = pending_review_state(child, "review-turn-1");
            contract.units[0].status = UnitStatus::Running;
            contract.units[0].review_evidence = None;
            state.rencrow_completion_state = Some(("root".into(), contract));
        }
        session
            .rollback_unstarted_completion_review("review-1")
            .await
            .expect("typed pre-spawn capacity rejection can release reservation");
        let state = session
            .rencrow_completion_state("root")
            .await
            .expect("registered review remains available");
        assert_eq!(state.units[0].status, UnitStatus::Ready);
        assert!(state.units[0].review_evidence.is_none());
    }

    #[test]
    fn unresolved_spawn_reuses_child_and_binds_later_generation() {
        let child =
            ThreadId::try_from("01a1c606-48b8-7cf3-bb78-77b47f6b8c2f").expect("valid child id");
        let mut contract =
            pending_review_state(&child.to_string(), "pending-generation:spawn-review");
        let unit = &mut contract.units[0];
        assert_eq!(unit.status, UnitStatus::Running);
        assert!(!completion_review_needs_spawn(unit));
        let evidence = unit
            .review_evidence
            .as_mut()
            .expect("persisted child binding");
        assert_eq!(evidence.reviewer_thread_id, child.to_string());
        assert_eq!(evidence.spawn_call_id, "spawn-review");
        assert!(bind_completion_review_generation(
            evidence,
            child,
            "spawn-review",
            "review-turn-2".into(),
        ));
        assert_eq!(evidence.reviewer_thread_id, child.to_string());
        assert_eq!(evidence.spawn_call_id, "spawn-review");
        assert_eq!(evidence.reviewer_turn_id, "review-turn-2");
        assert!(!bind_completion_review_generation(
            evidence,
            child,
            "different-spawn",
            "review-turn-3".into(),
        ));
        assert_eq!(evidence.reviewer_turn_id, "review-turn-2");

        // Legacy completion_review spawn calls this binder without the newer
        // pre-persist step. Its initial evidence must use the same sentinel.
        let mut direct = pending_completion_review_evidence(
            child,
            "analyst".into(),
            "legacy-spawn-call".into(),
            "task-a".into(),
            1,
            "task-check:target".into(),
            None,
        );
        assert!(bind_completion_review_generation(
            &mut direct,
            child,
            "legacy-spawn-call",
            "legacy-review-turn".into(),
        ));
        assert_eq!(direct.reviewer_thread_id, child.to_string());
        assert_eq!(direct.spawn_call_id, "legacy-spawn-call");
        assert_eq!(direct.reviewer_turn_id, "legacy-review-turn");
    }

    #[tokio::test]
    async fn failed_review_terminal_is_recorded_and_not_automatically_retried() {
        let (session, _) = make_session_and_context().await;
        let child =
            ThreadId::try_from("01a1c606-48b8-7cf3-bb78-77b47f6b8c2f").expect("valid child id");
        let mut contract = pending_review_state(&child.to_string(), "review-turn-1");
        contract.units[0]
            .review_evidence
            .as_mut()
            .unwrap()
            .wait_result_ref = None;
        contract.units[0]
            .review_evidence
            .as_mut()
            .unwrap()
            .terminal_status = None;
        contract.units[0]
            .review_evidence
            .as_mut()
            .unwrap()
            .wait_call_id = None;
        session.state.lock().await.rencrow_completion_state = Some(("root".into(), contract));

        session
            .collect_completion_review_results(
                "wait-failed",
                &[(
                    child,
                    Some("review-turn-1".into()),
                    AgentStatus::Errored("reviewer failed".into()),
                )],
            )
            .await;

        let state = session
            .rencrow_completion_state("root")
            .await
            .expect("completion plan remains registered");
        assert_eq!(state.units[0].status, UnitStatus::Failed);
        assert_eq!(state.failed_approaches.len(), 1);
        assert!(
            state.failed_approaches[0]
                .cause
                .contains("reviewer turn ended")
        );
    }

    #[tokio::test]
    async fn required_review_unit_keeps_satisfied_execution_plan_pending() {
        let (session, _) = make_session_and_context().await;
        let mut contract = pending_review_state("01a1c606-48b8-7cf3-bb78-77b47f6b8c2f", "turn-1");
        contract.conditions[0].status = AcceptanceStatus::Satisfied;
        contract.conditions[0].evidence =
            vec![codex_history::task_context::CompletionEvidenceRef {
                task_id: "task-a".into(),
                task_revision: 1,
                artifact_ref: "task-check:accepted".into(),
                verification_ref: "task-check:accepted".into(),
            }];
        contract.units[0].status = UnitStatus::Planned;
        contract.units[0].review_evidence = None;
        contract.units[0].depends_on = vec!["task-a".into()];
        contract.units.insert(
            0,
            CompletionUnit {
                id: "task-a".into(),
                purpose: "Execute task A".into(),
                status: UnitStatus::Done,
                kind: CompletionUnitKind::Execution,
                condition_refs: vec!["AC-1".into()],
                depends_on: vec![],
                evidence: vec![],
                task_refs: vec![],
                review_target: None,
                review_evidence: None,
                review_launch: None,
                review_decision_reason: None,
            },
        );
        session.state.lock().await.rencrow_completion_state = Some(("root".into(), contract));

        assert!(
            session.autonomous_completion_pending().await,
            "a required review remains unfinished after execution and AC satisfaction"
        );
    }

    #[tokio::test]
    async fn review_spawn_rejects_unchecked_targets_and_preserves_adopted_review_history() {
        let (session, _) = make_session_and_context().await;
        session.state.lock().await.rencrow_completion_state = Some((
            "root".into(),
            CompletionState {
                contract_revision: 1,
                units_completed_since_review: 0,
                plan_review_at: None,
                plan_reviews: vec![],
                paused: false,
                consumed_host_events: vec![],
                conditions: vec![AcceptanceCondition {
                    id: "AC-1".into(),
                    revision: 1,
                    result: "The requested behavior works".into(),
                    scope: "CLI".into(),
                    verification: "Run the requested check".into(),
                    acceptance_criteria: "Expected behavior is observable".into(),
                    status: AcceptanceStatus::Open,
                    evidence: vec![],
                    unmet_reason: None,
                }],
                units: vec![CompletionUnit {
                    id: "review-1".into(),
                    purpose: "Review task a".into(),
                    status: UnitStatus::Ready,
                    kind: CompletionUnitKind::Review,
                    condition_refs: vec!["AC-1".into()],
                    depends_on: vec![],
                    evidence: vec![],
                    task_refs: vec![],
                    review_target: Some(codex_history::task_context::CompletionReviewTarget {
                        task_id: "task-a".into(),
                        task_revision: 1,
                    }),
                    review_evidence: None,
                    review_launch: None,
                    review_decision_reason: None,
                }],
                current_unit: Some("review-1".into()),
                failed_approaches: vec![],
                blocker: None,
                next_action: Some("Collect a host-verified task before review".into()),
                product_progress_at: None,
                runtime_recovery_at: None,
                activity_at: None,
            },
        ));

        let error = session
            .validate_completion_review_spawn("review-1", "task-a", 1)
            .await
            .expect_err("model target without an accepted host check must be refused");
        assert!(error.contains("accepted task receipt"));
        assert!(
            session
                .rencrow_completion_state("root")
                .await
                .is_some_and(|state| state.units[0].review_evidence.is_none())
        );

        let reviewer_id =
            ThreadId::try_from("01a1c606-48b8-7cf3-bb78-77b47f6b8c2f").expect("valid reviewer id");
        session
            .state
            .lock()
            .await
            .rencrow_completion_state
            .as_mut()
            .unwrap()
            .1
            .units[0]
            .review_evidence = Some(CompletionReviewEvidence {
            reviewer_thread_id: reviewer_id.to_string(),
            reviewer_turn_id: "reviewer-turn-1".into(),
            reviewer_role: "analyst".into(),
            spawn_call_id: "spawn-review".into(),
            deadline_unix_ms: None,
            deadline_escalation: None,
            deadline_safety_fingerprint: None,
            wait_result_ref: None,
            terminal_status: None,
            wait_call_id: None,
            target_task_id: "task-a".into(),
            target_task_revision: 1,
            target_artifact_ref: "task-check:fake".into(),
        });
        session
            .collect_completion_review_results(
                "wait-review",
                &[(
                    reviewer_id,
                    Some("reviewer-turn-1".into()),
                    AgentStatus::Completed(Some("review finished".into())),
                )],
            )
            .await;
        assert!(
            session
                .rencrow_completion_state("root")
                .await
                .is_some_and(|state| state.units[0]
                    .review_evidence
                    .as_ref()
                    .is_some_and(|evidence| evidence.wait_result_ref.is_none()))
        );

        let adopted_evidence = CompletionReviewEvidence {
            reviewer_thread_id: reviewer_id.to_string(),
            reviewer_turn_id: "reviewer-turn-1".into(),
            reviewer_role: "analyst".into(),
            spawn_call_id: "spawn-adopted-review".into(),
            deadline_unix_ms: None,
            deadline_escalation: None,
            deadline_safety_fingerprint: None,
            wait_result_ref: Some("sha256:historical-result".into()),
            terminal_status: Some(CompletionReviewTerminalStatus::Completed),
            wait_call_id: Some("wait-adopted-review".into()),
            target_task_id: "task-a".into(),
            target_task_revision: 1,
            target_artifact_ref: "task-check:historical".into(),
        };
        {
            let mut state = session.state.lock().await;
            let contract = &mut state.rencrow_completion_state.as_mut().unwrap().1;
            contract.units[0].status = UnitStatus::Done;
            contract.units[0].review_decision_reason =
                Some("Adopted the historical review for task revision 1".into());
            contract.units[0].review_evidence = Some(adopted_evidence.clone());
        }
        session.revalidate_collected_completion_reviews().await;
        assert!(
            session
                .rencrow_completion_state("root")
                .await
                .is_some_and(|state| state.units[0].status == UnitStatus::Done
                    && state.units[0].review_decision_reason.is_some()
                    && state.units[0].review_evidence.as_ref() == Some(&adopted_evidence))
        );
    }
}
