// Modified by RenCrow Switch Core, 2026-11-21.
//! Native preparation and import. The existing supervisor owns quiescent activation.
use super::*;
use crate::config::Config;
use crate::context::AutonomousCompletionContinue;
use crate::context::CompactionSummary;
use crate::context::ParentHandoffNotice;
use crate::session::turn_context::TurnContext;
use codex_features::Feature;
use codex_history::compaction_candidate::history_digest;
use codex_history::task_checkpoint_metadata::RenCrowTaskCheckpointMetadataV3;
use codex_history::task_checkpoint_metadata::TaskBoundarySourceFence;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::ContentItem;
use std::path::Path;
#[path = "parent_handoff_io.rs"]
mod io;
use io::audit;
pub(crate) use io::claim;
use io::host_value;
pub(crate) use io::load_verified;
use io::notice;
use io::read_bounded;
use io::write_private;
#[path = "parent_handoff_input.rs"]
mod input;
pub(crate) use input::PreservedHandoffInput;

impl Session {
    pub(crate) async fn take_parent_handoff_guard(&self) -> Option<GuardState> {
        self.state.lock().await.rencrow_inherited_guard.take()
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HandoffProof {
    pub(crate) offset: u64,
    pub(crate) length: u64,
    pub(crate) sha256: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HandoffRecord {
    pub(crate) version: u32,
    pub(crate) capsule_file: PathBuf,
    pub(crate) capsule_sha256: String,
    pub(crate) capsule_file_sha256: String,
    pub(crate) source_thread_id: ThreadId,
    pub(crate) successor_thread_id: ThreadId,
    pub(crate) source_turn_id: String,
    pub(crate) phase: String,
    pub(crate) reason: Option<String>,
    pub(crate) prepared_proof: HandoffProof,
    pub(crate) failed_proof: Option<HandoffProof>,
    pub(crate) input_file: Option<PathBuf>,
    pub(crate) input_file_sha256: Option<String>,
}

pub(crate) struct PreparedHandoff {
    path: PathBuf,
    record: HandoffRecord,
    history_hash: String,
}

pub(crate) fn runtime_contract(config: &Config) -> Result<String, String> {
    let mut roles = BTreeMap::new();
    for (name, role) in &config.agent_roles {
        let file_hash = role
            .config_file
            .as_ref()
            .map(|path| {
                let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
                codex_history::compaction_candidate::digest(&bytes)
            })
            .transpose()?;
        roles.insert(name, (format!("{role:?}"), file_hash));
    }
    maintenance_snapshot_digest(&serde_json::json!({
        "guard": config.rencrow_execution_guard,
        "tools": config.rencrow_tool_allowlist,
        "completion": config.rencrow_autonomous_completion,
        "compaction": config.rencrow_compaction,
        "handoff_directory": config.rencrow_parent_handoff_dir,
        "multi_agent": config.features.enabled(Feature::Collab),
        "multi_agent_v2": config.features.enabled(Feature::MultiAgentV2),
        "base": config.base_instructions,
        "developer": config.developer_instructions,
        "provider": config.model_provider,
        "roles": roles,
    }))
}

/// Save the finite context and original host state BEFORE invoking PreCompact hooks.
pub(crate) async fn prepare(
    sess: &Session,
    turn: &TurnContext,
    phase: codex_analytics::CompactionPhase,
) -> Result<Option<PreparedHandoff>, String> {
    let Some(directory) = &turn.config.rencrow_parent_handoff_dir else {
        return Ok(None);
    };
    if !turn.config.rencrow_compaction {
        return Err("parent handoff requires local RenCrow compaction".into());
    }
    if turn.session_source.is_non_root_agent() {
        return Err("parent handoff is restricted to root threads".into());
    }
    if !sess.list_background_terminals().await.is_empty() || sess.has_outstanding_durable_sleep() {
        return Err("parent handoff has live background terminals or durable sleep".into());
    }
    let turn_state = {
        let active = sess.active_turn.lock().await;
        let turn = active.as_ref().ok_or("parent handoff has no active turn")?;
        std::sync::Arc::clone(&turn.turn_state)
    };
    let (guard_state, tracker) = {
        let state = turn_state.lock().await;
        (
            state.rencrow_execution_guard.clone(),
            state.rencrow_handoff_diff_tracker.clone(),
        )
    };
    let tracker = match tracker {
        Some(tracker) => Some(tracker.lock().await.clone()),
        None => None,
    };
    let guard = guard_state.snapshot_with_diff_tracker(tracker.as_ref())?;
    sess.check_rencrow_checkpoint()
        .await
        .map_err(|e| e.to_string())?;
    let history = sess.clone_history().await;
    let pending_prompt = if matches!(phase, codex_analytics::CompactionPhase::PreTurn) {
        Some(
            sess.state
                .lock()
                .await
                .rencrow_handoff_pending_input
                .as_ref()
                .filter(|(id, _)| id == &turn.sub_id)
                .map(|(_, text)| text.clone())
                .ok_or("pre-turn handoff requires a finite plain-text pending input")?,
        )
    } else {
        None
    };
    let accepted_task_state = crate::compact::latest_task_state_in_history(
        history.annotated_items(),
        &sess.thread_id().to_string(),
    )?;
    let (original_snapshot, prior_snapshots) = {
        let state = sess.state.lock().await;
        if state.pending_task_boundary.is_some() || state.rencrow_completion_paused {
            return Err("parent handoff has a pending boundary or paused work".into());
        }
        let hash = &state
            .rencrow_completion_last_persisted_snapshot
            .as_ref()
            .ok_or("parent handoff requires a registered completion contract")?
            .sha256;
        let value = state
            .rencrow_completion_handoff_proofs
            .get(hash)
            .ok_or("parent handoff latest host snapshot is unavailable")?
            .clone();
        let prior = proof_closure(
            &value,
            &state.rencrow_completion_handoff_proofs,
            accepted_task_state.as_ref(),
        )?;
        (value, prior)
    };
    let snapshot: Snapshot =
        serde_json::from_value(original_snapshot.clone()).map_err(|e| e.to_string())?;
    let goal = accepted_task_state
        .as_ref()
        .map(|state| state.boundary.goal.as_str())
        .unwrap_or(&snapshot.root_task_id);
    let mut summary = format!("Goal: {goal}\nRoot: {}\n", snapshot.root_task_id);
    let mut next = Vec::new();
    for unit in &snapshot.state.units {
        summary.push_str(&format!(
            "{}: {:?}: {}\n",
            unit.id, unit.status, unit.purpose
        ));
        if !matches!(
            unit.status,
            codex_history::task_context::UnitStatus::Done
                | codex_history::task_context::UnitStatus::Cancelled
        ) {
            next.push(format!("{}: {}", unit.id, unit.purpose));
        }
    }
    if let Some(blocker) = &snapshot.state.blocker {
        summary.push_str(&format!(
            "Unresolved blocker (not cleared by transfer): {}\n",
            serde_json::to_string(blocker).map_err(|e| e.to_string())?
        ));
    }
    let mut retained = Vec::new();
    let latest_autonomous_completion_continue = history
        .annotated_items()
        .iter()
        .enumerate()
        .filter_map(|(index, envelope)| {
            AutonomousCompletionContinue::is_host_generated(envelope).then_some(index)
        })
        .next_back();
    for (history_index, envelope) in history.annotated_items().iter().enumerate() {
        if let Some(scope) = &snapshot.active_scope
            && match &envelope.item {
                ResponseItem::FunctionCall { call_id, .. } => call_id == &scope.begin_call_id,
                ResponseItem::FunctionCallOutput { call_id, .. } => {
                    call_id.as_deref() == Some(scope.begin_call_id.as_str())
                }
                _ => false,
            }
        {
            retained.push(RolloutItem::ResponseItem(envelope.clone()));
            continue;
        }
        // Only this typed host runtime value is last-write-wins. User and opaque messages stay intact.
        if AutonomousCompletionContinue::is_host_generated(envelope)
            && latest_autonomous_completion_continue != Some(history_index)
        {
            continue;
        }
        if let ResponseItem::Message {
            role,
            content,
            internal_chat_message_metadata_passthrough: kind,
            ..
        } = &envelope.item
        {
            if !matches!(role.as_str(), "user" | "developer") {
                continue;
            }
            let kinds = kind
                .as_ref()
                .and_then(|kind| kind.content_item_kinds.as_ref());
            if kinds.is_some_and(|kinds| {
                kinds.iter().any(|kind| {
                    matches!(
                        kind.0.as_str(),
                        "rencrow.completion_snapshot" | "rencrow.parent_handoff"
                    )
                })
            }) {
                continue;
            }
            if kinds.is_some_and(|kinds| kinds.iter().any(|kind| kind.0 == "compaction.summary")) {
                for text in content.iter().filter_map(|item| match item {
                    ContentItem::InputText { text } => Some(text),
                    _ => None,
                }) {
                    summary.push_str(text);
                    summary.push('\n');
                }
                continue;
            }
            // Preserve messages and provenance exactly, including withdrawals already selected
            // by the owner's current history. No summaries become new user authorization.
            let mut instruction = envelope.clone();
            if let Some(metadata) = instruction.metadata.as_mut() {
                metadata.rencrow_compaction = None;
                metadata.rencrow_completion = None;
            }
            retained.push(RolloutItem::ResponseItem(instruction));
        }
    }
    if next.is_empty() {
        return Err("parent handoff has no unfinished units".into());
    }
    if summary.len() > 32 * 1024
        || serde_json::to_vec(&retained)
            .map_err(|e| e.to_string())?
            .len()
            > 128 * 1024
    {
        return Err("parent handoff retained instructions exceed the finite context limit".into());
    }
    let successor = sess.services.agent_control.generate_thread_id();
    let capsule = ParentHandoffCapsule {
        version: HANDOFF_VERSION,
        source_thread_id: sess.thread_id(),
        successor_thread_id: successor,
        source_turn_id: turn.sub_id.clone(),
        source_rollout: sess
            .current_rollout_path()
            .await
            .map_err(|e| e.to_string())?
            .ok_or("parent handoff rollout absent")?,
        source_history_hash: history_digest(history.annotated_items())?,
        source_history_len: history.annotated_items().len(),
        pending_prompt,
        runtime_contract_sha256: runtime_contract(&turn.config)?,
        settings: sess.thread_settings_snapshot().await,
        cwd: turn.config.cwd.to_path_buf(),
        summary,
        next_work: next.join("\n"),
        original_snapshot,
        prior_snapshots,
        accepted_task_state,
        retained_instructions: retained,
        guard,
    };
    verify_live_source(sess, &capsule).await?;
    capsule.project()?;
    // Reanchor the existing validated host fact in the physical rollout only. Growing
    // model history here would invalidate unchanged-history failure suppression.
    if !sess
        .persist_rollout_items(&[RolloutItem::ResponseItem(ResponseItemEnvelope {
            item: Box::new(AutonomousCompletionSnapshot).into_boxed_response_item(),
            metadata: Some(CodexHarnessMetadata {
                rencrow_completion: Some(capsule.original_snapshot.clone()),
                ..Default::default()
            }),
        })])
        .await
    {
        return Err("parent handoff source snapshot copy failed".into());
    }
    let folder = directory.join(successor.to_string());
    let capsule_file = folder.join("capsule.json").to_path_buf();
    let path = folder.join("handoff.json").to_path_buf();
    write_private(&capsule_file, &capsule)?;
    let capsule_file_sha256 = codex_history::archive_reference::content_sha256(
        &std::fs::read_to_string(&capsule_file).map_err(|e| e.to_string())?,
    );
    let mut record = HandoffRecord {
        version: HANDOFF_VERSION,
        capsule_file,
        capsule_sha256: capsule.sha256()?,
        capsule_file_sha256,
        source_thread_id: capsule.source_thread_id,
        successor_thread_id: successor,
        source_turn_id: turn.sub_id.clone(),
        phase: "prepared".into(),
        reason: None,
        prepared_proof: HandoffProof {
            offset: 0,
            length: 0,
            sha256: String::new(),
        },
        failed_proof: None,
        input_file: None,
        input_file_sha256: None,
    };
    record.prepared_proof = audit(sess, &record, "prepared").await?;
    write_private(&path, &record)?;
    Ok(Some(PreparedHandoff {
        path,
        record,
        history_hash: capsule.source_history_hash,
    }))
}

/// Only a typed capacity/model failure after both compaction paths qualifies.
/// Interruption, integrity errors, uncertain persistence and stale snapshots never do.
pub(crate) async fn finish(
    sess: &Session,
    turn: &TurnContext,
    prepared: &PreparedHandoff,
    outcome: &str,
    reason: Option<String>,
) -> Result<(), String> {
    let mut record = prepared.record.clone();
    record.phase = outcome.into();
    record.reason = reason;
    if outcome == "failed" {
        let capsule = read_bounded::<ParentHandoffCapsule>(&record.capsule_file)?;
        if capsule.pending_prompt.is_some() {
            record.phase = "awaiting_input".into();
            write_private(&prepared.path, &record)?;
            sess.state.lock().await.rencrow_handoff_pending_record = Some(prepared.path.clone());
            return Ok(());
        }
        sess.check_rencrow_checkpoint()
            .await
            .map_err(|e| e.to_string())?;
        if history_digest(sess.clone_history().await.annotated_items())? != prepared.history_hash
            || !sess.list_background_terminals().await.is_empty()
            || sess.thread_settings_snapshot().await
                != read_bounded::<ParentHandoffCapsule>(&record.capsule_file)?.settings
        {
            return Err(
                "parent handoff source changed during compaction; no automatic migration".into(),
            );
        }
        let cancelled = {
            let active = sess.active_turn.lock().await;
            active
                .as_ref()
                .and_then(|active| active.task.as_ref())
                .filter(|task| task.turn_context.sub_id == turn.sub_id)
                .ok_or("parent handoff source task ended")?
                .cancellation_token
                .is_cancelled()
        };
        if cancelled {
            return Err("parent handoff source was cancelled".into());
        }
        verify_live_source(
            sess,
            &read_bounded::<ParentHandoffCapsule>(&record.capsule_file)?,
        )
        .await?;
        record.failed_proof = Some(audit(sess, &record, "failed").await?);
    }
    write_private(&prepared.path, &record)
}

/// Capture and seal both use the same native state comparison; no audit-only event can
/// conceal a concurrent check result, pause, task boundary or guard-budget mutation.
async fn verify_live_source(sess: &Session, capsule: &ParentHandoffCapsule) -> Result<(), String> {
    if sess.has_outstanding_durable_sleep() {
        return Err("parent handoff source has outstanding durable sleep".into());
    }
    let original: Snapshot =
        serde_json::from_value(capsule.original_snapshot.clone()).map_err(|e| e.to_string())?;
    let turn_state = {
        let active = sess.active_turn.lock().await;
        let active = active.as_ref().ok_or("handoff source has no active turn")?;
        let task = active
            .task
            .as_ref()
            .filter(|task| task.turn_context.sub_id == capsule.source_turn_id)
            .ok_or("handoff source task changed")?;
        if task.cancellation_token.is_cancelled() {
            return Err("handoff source was cancelled".into());
        }
        std::sync::Arc::clone(&active.turn_state)
    };
    let (guard_state, tracker) = {
        let state = turn_state.lock().await;
        (
            state.rencrow_execution_guard.clone(),
            state.rencrow_handoff_diff_tracker.clone(),
        )
    };
    let tracker = match tracker {
        Some(tracker) => Some(tracker.lock().await.clone()),
        None => None,
    };
    let guard = guard_state.snapshot_with_diff_tracker(tracker.as_ref())?;
    if guard != capsule.guard {
        return Err("handoff source guard changed".into());
    }
    let state = sess.state.lock().await;
    let task = crate::compact::latest_task_state_in_history(
        state.history.annotated_items(),
        &sess.thread_id().to_string(),
    )?;
    let references = completion_task_check_references(&original.state);
    let mut checks = state
        .task_check_ledger
        .snapshot_with_references(&references);
    let mut original_checks = original.task_checks;
    checks.sort_by(|a, b| a.attempt.call_id.cmp(&b.attempt.call_id));
    original_checks.sort_by(|a, b| a.attempt.call_id.cmp(&b.attempt.call_id));
    let mut patches = state
        .task_patch_attempts
        .iter()
        .filter(|(_, attempt)| {
            state
                .active_task_scope
                .as_ref()
                .is_some_and(|scope| scope.same_execution(&attempt.scope))
        })
        .map(|(call, attempt)| (call.clone(), attempt.clone()))
        .collect::<Vec<_>>();
    patches.sort_by(|a, b| a.0.cmp(&b.0));
    let mut original_patches = original.patch_attempts;
    for (_, attempt) in &mut patches {
        attempt.scope.start_history_cursor = 0;
    }
    for (_, attempt) in &mut original_patches {
        attempt.scope.start_history_cursor = 0;
    }
    original_patches.sort_by(|a, b| a.0.cmp(&b.0));
    if state.rencrow_completion_state.as_ref() != Some(&(original.root_task_id, original.state))
        || state.rencrow_completion_paused
        || state.pending_task_boundary.is_some()
        || !state.rencrow_completion_uncollected_children.is_empty()
        || state.rencrow_completion_resume_generation != original.resume_generation
        || !match (&state.active_task_scope, &original.active_scope) {
            (None, None) => true,
            (Some(current), Some(original)) => current.same_execution(original),
            _ => false,
        }
        || serde_json::to_value(checks).map_err(|e| e.to_string())?
            != serde_json::to_value(original_checks).map_err(|e| e.to_string())?
        || serde_json::to_value(patches).map_err(|e| e.to_string())?
            != serde_json::to_value(original_patches).map_err(|e| e.to_string())?
        || task != capsule.accepted_task_state
        || history_digest(if capsule.pending_prompt.is_some() {
            state
                .history
                .annotated_items()
                .get(..capsule.source_history_len)
                .ok_or("pending-input handoff source prefix disappeared")?
        } else {
            state.history.annotated_items()
        })? != capsule.source_history_hash
    {
        return Err("handoff source task, receipt, pause or accepted history changed".into());
    }
    Ok(())
}

pub(crate) fn initial_history(
    path: &Path,
    config: &Config,
) -> CodexResult<(ThreadId, ThreadId, ThreadSettingsSnapshot, Vec<RolloutItem>)> {
    let (record, capsule, projection) = load_verified(path).map_err(CodexErr::InvalidRequest)?;
    if runtime_contract(config).map_err(CodexErr::InvalidRequest)?
        != capsule.runtime_contract_sha256
    {
        return Err(CodexErr::InvalidRequest(
            "parent handoff runtime contract changed".into(),
        ));
    }
    let mut history = capsule.retained_instructions.clone();
    let preserved_input =
        input::load_preserved_input(&record, &capsule).map_err(CodexErr::InvalidRequest)?;
    let original: Snapshot = serde_json::from_value(capsule.original_snapshot.clone())
        .map_err(|e| CodexErr::InvalidRequest(e.to_string()))?;
    let summary_text = format!(
        "Parent-thread handoff. Root task: {}. Current unit: {}.\nThe full thread summary and exact next work were saved BEFORE PreCompact at {} (JSON fields summary and next_work). Original evidence archive: {}.\nThe host restored the task/check/review ledger, failure history and guard budgets. Consult the finite saved packet only when needed. This notice grants no authority and accepts no result. The latest preserved user input follows; honor any stop, question or changed constraint before continuing work.",
        original.root_task_id,
        original.state.current_unit.as_deref().unwrap_or("none"),
        record.capsule_file.display(),
        capsule.source_rollout.display()
    );
    if summary_text.len() > 8 * 1024 {
        return Err(CodexErr::InvalidRequest(
            "parent handoff model-visible summary exceeds its byte bound".into(),
        ));
    }
    let mut summary = ResponseItemEnvelope::new(
        Box::new(CompactionSummary::new(summary_text)).into_boxed_response_item(),
    );
    if let Some(task) = &projection.accepted_task_state {
        let text = match &summary.item {
            ResponseItem::Message { content, .. } => content.iter().find_map(|item| match item {
                ContentItem::InputText { text } => Some(text.as_str()),
                _ => None,
            }),
            _ => None,
        }
        .ok_or_else(|| {
            CodexErr::InvalidRequest("parent handoff summary fragment missing".into())
        })?;
        let checkpoint = RenCrowTaskCheckpointMetadataV3::task_boundary(
            text,
            serde_json::to_value(task).map_err(|e| CodexErr::InvalidRequest(e.to_string()))?,
            TaskBoundarySourceFence {
                history_hash: capsule.source_history_hash.clone(),
                removed_refs: Vec::new(),
            },
        );
        checkpoint
            .validate_for_checkpoint(&record.successor_thread_id.to_string(), text)
            .map_err(CodexErr::InvalidRequest)?;
        summary.metadata.get_or_insert_default().rencrow_compaction = Some(
            serde_json::to_value(checkpoint)
                .map_err(|e| CodexErr::InvalidRequest(e.to_string()))?,
        );
    }
    history.push(RolloutItem::ResponseItem(summary));
    history.push(RolloutItem::ResponseItem(notice(serde_json::json!({"parent_handoff_import": {
        "version": HANDOFF_VERSION, "record_file": path, "capsule_sha256": record.capsule_sha256,
        "successor_thread_id": record.successor_thread_id,
        "projection_mapping_sha256": maintenance_snapshot_digest(&serde_json::to_value(&projection.original_to_projected)
            .map_err(|e| CodexErr::InvalidRequest(e.to_string()))?).map_err(CodexErr::InvalidRequest)?,
    }}))));
    history.push(RolloutItem::ResponseItem(ResponseItemEnvelope {
        item: Box::new(AutonomousCompletionSnapshot).into_boxed_response_item(),
        metadata: Some(CodexHarnessMetadata {
            rencrow_completion: Some(projection.snapshot),
            ..Default::default()
        }),
    }));
    history.extend(preserved_input);
    Ok((
        record.source_thread_id,
        record.successor_thread_id,
        capsule.settings,
        history,
    ))
}

/// Ancestor snapshots seed reference resolution only; they never overwrite the latest state.
pub(crate) fn restore_seed(
    items: &[RolloutItem],
    target: ThreadId,
) -> Result<Option<ParentHandoffProjection>, String> {
    let mut result = None;
    for item in items {
        let Some(import) = host_value(item).and_then(|value| value.get("parent_handoff_import"))
        else {
            continue;
        };
        if result.is_some() {
            return Err("duplicate parent handoff import".into());
        }
        let path = import
            .get("record_file")
            .and_then(Value::as_str)
            .ok_or("parent handoff import path missing")?;
        let (record, _, projection) = load_verified(Path::new(path))?;
        if record.successor_thread_id != target
            || import.get("capsule_sha256").and_then(Value::as_str)
                != Some(record.capsule_sha256.as_str())
        {
            return Err("parent handoff import identity mismatch".into());
        }
        if import
            .get("projection_mapping_sha256")
            .and_then(Value::as_str)
            != Some(
                maintenance_snapshot_digest(
                    &serde_json::to_value(&projection.original_to_projected)
                        .map_err(|e| e.to_string())?,
                )?
                .as_str(),
            )
        {
            return Err("parent handoff projected proof mapping changed".into());
        }
        result = Some(projection);
    }
    Ok(result)
}
