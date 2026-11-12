// Modified by RenCrow Switch Core, 2026-09-22.
//! Input provenance and durable replacement stay inside the session owner.
use super::session::Session;
use super::thread_settings;
use crate::context::world_state::WorldState;
use crate::context_manager::HistoryReplacement;
use crate::session::turn_context::TurnContext;
use crate::state::AutoCompactWindowIds;
use crate::state::ReasoningEffortPin;
use codex_history::CompactedItem;
use codex_history::ResponseItemEnvelope;
use codex_history::RolloutItem;
use codex_history::compaction_candidate::digest;
use codex_history::compaction_candidate::history_digest;
use codex_history::input_intake::SubmissionIntake;
use codex_history::input_intake::valid_intake_id;
use codex_history::task_checkpoint_metadata::RenCrowTaskCheckpointMetadataV3;
use codex_history::task_checkpoint_metadata::TaskBoundarySourceFence;
use codex_history::task_checkpoint_metadata::TaskCheckpointMode;
use codex_history::task_context::StoredTaskState;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadSettingsAppliedEvent;
use codex_protocol::protocol::ThreadSettingsSnapshot;
use codex_protocol::protocol::TurnContextItem;
use codex_protocol::protocol::WorldStateItem;
use serde_json::json;
use std::sync::Arc;

pub(crate) struct RenCrowCheckpoint {
    pub items: Vec<ResponseItemEnvelope>,
    pub expected_history_hash: String,
    pub expected_settings: ThreadSettingsSnapshot,
    pub reference_context: Option<TurnContextItem>,
    pub world_state: Option<Arc<WorldState>>,
    pub summary: String,
    /// Summary response of a Normal checkpoint; a deterministic emergency sends none.
    pub response_id: Option<String>,
    pub expected_turn: String,
    pub expected_base_text: String,
    pub expected_world: Option<WorldStateItem>,
}

impl Session {
    pub(crate) async fn latest_rencrow_task_state(&self) -> Result<Option<StoredTaskState>> {
        let history = self.clone_history().await;
        crate::compact::latest_task_state_in_history(
            history.annotated_items(),
            &self.thread_id().to_string(),
        )
        .map_err(CodexErr::InvalidRequest)
    }

    /// Commit a host-validated task boundary at the session-owned persistence barrier.
    pub(crate) async fn commit_task_boundary_checkpoint(
        &self,
        expected_history_hash: String,
        expected_turn_id: String,
        mut items: Vec<ResponseItemEnvelope>,
        summary_text: String,
        task_state: StoredTaskState,
        source_fence: TaskBoundarySourceFence,
    ) -> Result<()> {
        let snapshot = self.clone_history().await;
        let actual_history_hash =
            history_digest(snapshot.annotated_items()).map_err(CodexErr::InvalidRequest)?;
        if actual_history_hash != expected_history_hash
            || source_fence.history_hash != expected_history_hash
        {
            return Err(CodexErr::InvalidRequest(
                "task boundary snapshot changed before commit".into(),
            ));
        }
        crate::compact::attach_task_boundary_metadata(
            &mut items,
            &expected_turn_id,
            &summary_text,
            &task_state,
            source_fence,
        )
        .map_err(CodexErr::InvalidRequest)?;
        let settings = self.thread_settings_snapshot().await;
        let base = self.get_base_instructions().await;
        let expected_world = snapshot.world_state_checkpoint();
        self.commit_rencrow_checkpoint(RenCrowCheckpoint {
            items,
            expected_history_hash,
            expected_settings: settings,
            reference_context: None,
            world_state: None,
            summary: summary_text,
            response_id: None,
            expected_turn: expected_turn_id,
            expected_base_text: base.text,
            expected_world,
        })
        .await
    }

    pub(crate) async fn check_rencrow_checkpoint(&self) -> Result<()> {
        if self.state.lock().await.rencrow_checkpoint_failed {
            return Err(CodexErr::Fatal("Compaction checkpoint persistence was uncertain; restart to recover the last readable owner checkpoint before continuing".into()));
        }
        Ok(())
    }

    /// History digest whose automatic compaction already failed in this session (Annex A F03).
    pub(crate) async fn rencrow_auto_compaction_failed_hash(&self) -> Option<String> {
        self.state
            .lock()
            .await
            .rencrow_auto_compaction_failed_hash
            .clone()
    }

    pub(crate) async fn set_rencrow_auto_compaction_failed_hash(&self, hash: Option<String>) {
        self.state.lock().await.rencrow_auto_compaction_failed_hash = hash;
    }

    /// History digest whose Normal stage already failed semantically (Part 2 §56).
    pub(crate) async fn rencrow_normal_failed_hash(&self) -> Option<String> {
        self.state.lock().await.rencrow_normal_failed_hash.clone()
    }

    pub(crate) async fn set_rencrow_normal_failed_hash(&self, hash: Option<String>) {
        self.state.lock().await.rencrow_normal_failed_hash = hash;
    }

    pub(super) async fn bind_rencrow_intake(
        &self,
        ctx: &TurnContext,
        client_id: Option<&str>,
        text: &str,
        items: &mut [ResponseItemEnvelope],
    ) {
        let Some(client_id) = client_id.filter(|id| valid_intake_id(id)) else {
            return;
        };
        let thread_id = self.thread_id().to_string();
        let path = ctx
            .config
            .codex_home
            .join("rencrow/input-intake")
            .join(&thread_id)
            .join(format!("{client_id}.json"));
        let result = async {
            let metadata = tokio::fs::metadata(&path).await?;
            if metadata.len() > 1024 * 1024 {
                return Err(std::io::Error::other("intake exceeds size limit"));
            }
            let bytes = tokio::fs::read(path).await?;
            let receipt: SubmissionIntake =
                serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
            if !receipt.matches_accepted(&thread_id, client_id, text) {
                return Err(std::io::Error::other(
                    "intake does not match accepted input",
                ));
            }
            Ok(receipt)
        }
        .await;
        match result {
            Ok(receipt) => {
                if items.len() == 1 {
                    // Only original text is eligible. Mixed host additions stay unknown in
                    // the adapter unless its complete selected text matches the envelope.
                    items[0].metadata.get_or_insert_default().rencrow_input = Some(json!({
                        "version": 1, "author": receipt.original.author, "thread_id": thread_id,
                        "receipt_hash": digest(&receipt).ok(), "client_id": client_id,
                        "selected_text": receipt.original.text, "attachments": receipt.original.attachments
                    }));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(%error, "RenCrow intake was not bound; input remains unknown")
            }
        }
    }

    /// Queued input that has not entered history does not make the candidate stale; the turn
    /// records it after the checkpoint.
    pub(crate) async fn commit_rencrow_checkpoint(
        &self,
        mut candidate: RenCrowCheckpoint,
    ) -> Result<()> {
        let cancellation = {
            let active = self.active_turn.lock().await;
            active
                .as_ref()
                .and_then(|active| active.task.as_ref())
                .filter(|task| task.turn_context.sub_id == candidate.expected_turn)
                .map(|task| task.cancellation_token.clone())
        };
        let _input_guard = self
            .input_queue
            .compaction_gate
            .acquire()
            .await
            .map_err(|error| CodexErr::Fatal(error.to_string()))?;
        let _settings_guard = thread_settings::acquire_persistence_lock(self).await;
        let (batch, ids, number, mcp_revision, cancellation, live_thread) = {
            let mut state = self.state.lock().await;
            let current_settings = state
                .session_configuration
                .thread_settings_snapshot(&state.session_configuration.environments);
            if state.rencrow_checkpoint_failed
                || history_digest(state.history.annotated_items())
                    .map_err(CodexErr::InvalidRequest)?
                    != candidate.expected_history_hash
                || current_settings != candidate.expected_settings
                || state.session_configuration.base_instructions != candidate.expected_base_text
                || state.history.world_state_checkpoint() != candidate.expected_world
            {
                return Err(CodexErr::InvalidRequest(
                    "RenCrow compaction candidate is stale; history was not replaced".into(),
                ));
            }
            validate_task_boundary_source_fence(
                state.history.annotated_items(),
                &candidate.items,
                &candidate.expected_history_hash,
                &self.thread_id().to_string(),
                &candidate.summary,
            )
            .map_err(CodexErr::InvalidRequest)?;
            let cancellation = cancellation.ok_or(CodexErr::TurnAborted)?;
            if cancellation.is_cancelled() {
                return Err(CodexErr::TurnAborted);
            }
            let live_thread = self.live_thread().ok_or_else(|| {
                CodexErr::InvalidRequest("RenCrow compaction requires owner persistence".into())
            })?;
            for envelope in &mut candidate.items {
                Self::assign_missing_response_item_id(&mut envelope.item);
            }
            let mcp_revision = self
                .services
                .executed_tool_calls
                .mcp_attribution_checkpoint(/*force*/ true)
                .and_then(|(attribution, revision)| {
                    candidate.items.last_mut().map(|item| {
                        item.metadata.get_or_insert_default().mcp_attribution = Some(attribution);
                        revision
                    })
                });
            let following = 1
                + usize::from(candidate.world_state.is_some())
                + usize::from(candidate.reference_context.is_some());
            let summary_metadata = candidate
                .items
                .last_mut()
                .and_then(|item| item.metadata.as_mut())
                .and_then(|metadata| metadata.rencrow_compaction.as_mut())
                .ok_or_else(|| {
                    CodexErr::InvalidRequest("missing validated compaction metadata".into())
                })?;
            summary_metadata["transaction_following_items"] = json!(following);
            if summary_metadata
                .get("version")
                .and_then(serde_json::Value::as_u64)
                == Some(3)
                && summary_metadata
                    .get("mode")
                    .and_then(serde_json::Value::as_str)
                    == Some("capacity")
            {
                summary_metadata["capacity_metadata"]["transaction_following_items"] =
                    json!(following);
                summary_metadata["capacity_metadata"]
                    .as_object_mut()
                    .ok_or_else(|| {
                        CodexErr::InvalidRequest(
                            "validated V3 capacity metadata is not an object".into(),
                        )
                    })?
                    .remove("committed_transaction_hash");
            }
            summary_metadata
                .as_object_mut()
                .ok_or_else(|| {
                    CodexErr::InvalidRequest(
                        "validated compaction metadata is not an object".into(),
                    )
                })?
                .remove("committed_transaction_hash");
            let mut next_history = state.history.clone();
            next_history.replace_compacted(candidate.items.clone(), None);
            let previous_ids = state.auto_compact_window_ids();
            let ids = AutoCompactWindowIds {
                first_window_id: previous_ids.first_window_id,
                previous_window_id: Some(previous_ids.window_id),
                window_id: uuid::Uuid::now_v7(),
            };
            let number = state.auto_compact_window_number().saturating_add(1);
            let checkpoint = CompactedItem {
                message: candidate.summary,
                replacement_history: Some(candidate.items.clone()),
                retained_context: Some(next_history.retained_context().clone()),
                guardian_history: next_history.guardian_history_checkpoint(),
                mcp_resource_origins: self.services.mcp_runtime.resource_origin_checkpoint(),
                window_number: Some(number),
                first_window_id: Some(ids.first_window_id.to_string()),
                previous_window_id: ids.previous_window_id.map(|id| id.to_string()),
                window_id: Some(ids.window_id.to_string()),
                compaction_response_id: candidate.response_id,
                latest_token_usage_record: state.latest_token_usage_record.clone(),
            };
            let mut batch = vec![RolloutItem::Compacted(checkpoint)];
            if let Some(world) = &candidate.world_state {
                batch.push(RolloutItem::WorldState(WorldStateItem::full(
                    world.snapshot().into_object(),
                )));
            }
            if let Some(context) = &candidate.reference_context {
                batch.push(RolloutItem::TurnContext(context.clone()));
            }
            batch.push(RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(
                ThreadSettingsAppliedEvent {
                    thread_id: Some(self.thread_id()),
                    thread_settings: current_settings,
                },
            )));
            let checkpoint_hash = codex_history::compaction_transaction::transaction_hash(&batch)
                .map_err(CodexErr::InvalidRequest)?;
            batch.push(RolloutItem::RenCrowCompactionCommit { checkpoint_hash });
            // Input/settings permits fence admission through I/O. The state flag blocks
            // subsequent sampling, while releasing state and active_turn permits abort.
            // Recheck the snapshot after persistence before publishing the replacement.
            state.rencrow_checkpoint_failed = true;
            (batch, ids, number, mcp_revision, cancellation, live_thread)
        };
        let persisted = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(CodexErr::Fatal(
                "Compaction canceled during persistence; restart required".into())),
            result = async {
                live_thread.append_items(&batch).await?;
                live_thread.flush().await
            } => result,
        };
        if let Err(error) = persisted {
            return Err(CodexErr::Fatal(format!(
                "RenCrow checkpoint persistence uncertain: {error}; restart required"
            )));
        }
        let mut state = self.state.lock().await;
        if cancellation.is_cancelled()
            || history_digest(state.history.annotated_items()).map_err(CodexErr::Fatal)?
                != candidate.expected_history_hash
            || state
                .session_configuration
                .thread_settings_snapshot(&state.session_configuration.environments)
                != candidate.expected_settings
            || state.session_configuration.base_instructions != candidate.expected_base_text
            || state.history.world_state_checkpoint() != candidate.expected_world
        {
            return Err(CodexErr::Fatal(
                "Compaction snapshot changed at persistence barrier; restart required".into(),
            ));
        }
        state.rencrow_checkpoint_failed = false;
        state.replace_annotated_history(
            candidate.items,
            candidate.reference_context,
            HistoryReplacement::Compaction {
                reviewer_compaction_hash: None,
            },
        );
        state.adopt_rencrow_compaction_window(number, ids);
        state.reasoning_effort_pin = ReasoningEffortPin::Compacted;
        if let Some(world) = candidate.world_state {
            state.history.set_world_state_baseline(world.snapshot());
        }
        state.queue_pending_session_start_source(codex_hooks::SessionStartSource::Compact);
        if let Some(revision) = mcp_revision {
            self.services
                .executed_tool_calls
                .mark_mcp_attribution_persisted(revision);
        }
        Ok(())
    }
}

/// Verify that a task boundary is bound to this exact input snapshot and the exact removed items.
/// V2 and V3 capacity checkpoints continue through the existing compaction contract.
fn validate_task_boundary_source_fence(
    originals: &[ResponseItemEnvelope],
    replacement: &[ResponseItemEnvelope],
    expected_history_hash: &str,
    thread_id: &str,
    summary_text: &str,
) -> std::result::Result<(), String> {
    let Some(value) = replacement
        .last()
        .and_then(|envelope| envelope.metadata.as_ref())
        .and_then(|metadata| metadata.rencrow_compaction.as_ref())
    else {
        return Ok(());
    };
    if value.get("version").and_then(serde_json::Value::as_u64) != Some(3) {
        return Ok(());
    }
    let checkpoint = RenCrowTaskCheckpointMetadataV3::parse_and_validate(
        value.clone(),
        thread_id,
        summary_text,
    )?;
    if checkpoint.mode != TaskCheckpointMode::TaskBoundary {
        return Ok(());
    }
    let fence = checkpoint
        .source_fence
        .as_ref()
        .ok_or_else(|| "task boundary checkpoint has no source fence".to_owned())?;
    let new_root_task_id = checkpoint
        .task_state
        .as_ref()
        .and_then(|value| StoredTaskState::parse_and_validate(value.clone(), thread_id).ok())
        .map(|state| state.boundary.root_task_id)
        .ok_or_else(|| "task boundary checkpoint has no valid root task state".to_owned())?;
    if fence.history_hash != expected_history_hash {
        return Err(
            "task boundary source fence does not match the checkpoint input snapshot".into(),
        );
    }

    let mut removed_indices = std::collections::BTreeSet::new();
    let mut removed_item_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for reference in &fence.removed_refs {
        let Some(index_text) = reference.id.strip_prefix("item:") else {
            return Err("task boundary source reference is not a host item index".into());
        };
        let index = index_text
            .parse::<usize>()
            .map_err(|_| "task boundary source reference index is invalid".to_owned())?;
        if index_text != index.to_string() || !removed_indices.insert(index) {
            return Err(
                "task boundary source reference index is duplicated or noncanonical".into(),
            );
        }
        let original = originals
            .get(index)
            .ok_or_else(|| "task boundary source reference is outside its snapshot".to_owned())?;
        if digest(&(&original.item, &original.metadata))? != reference.hash {
            return Err("task boundary source reference hash changed".into());
        }
        let item_hash = digest(&original.item)?;
        *removed_item_counts.entry(item_hash).or_default() += 1;
    }

    let mut original_item_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for envelope in originals {
        *original_item_counts
            .entry(digest(&envelope.item)?)
            .or_default() += 1;
    }
    let mut replacement_item_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for envelope in replacement {
        *replacement_item_counts
            .entry(digest(&envelope.item)?)
            .or_default() += 1;
    }
    for (item_hash, original_count) in &original_item_counts {
        let removed_count = original_count.saturating_sub(
            replacement_item_counts
                .get(item_hash)
                .copied()
                .unwrap_or_default(),
        );
        if removed_item_counts
            .get(item_hash)
            .copied()
            .unwrap_or_default()
            != removed_count
        {
            return Err(
                "task boundary source fence does not match the exact removed item set".into(),
            );
        }
    }
    if removed_item_counts
        .keys()
        .any(|hash| !original_item_counts.contains_key(hash))
    {
        return Err("task boundary source fence references a non-source item".into());
    }

    for &index in &removed_indices {
        if let Some((call_id, is_call)) = tool_call_pair(&originals[index].item) {
            let matches = originals
                .iter()
                .enumerate()
                .filter_map(|(other_index, envelope)| {
                    (other_index != index
                        && tool_call_pair(&envelope.item).is_some_and(
                            |(other_id, other_is_call)| {
                                other_id == call_id && other_is_call != is_call
                            },
                        ))
                    .then_some(other_index)
                })
                .collect::<Vec<_>>();
            if matches.len() != 1 || !removed_indices.contains(&matches[0]) {
                return Err("task boundary must remove tool calls and results as a pair".into());
            }
        } else if !matches!(
            &originals[index].item,
            ResponseItem::Message { role, .. } if role == "assistant"
        ) && is_replaceable_task_boundary_summary(&originals[index], thread_id)
            .is_none_or(|root_task_id| root_task_id != new_root_task_id)
        {
            return Err(
                "task boundary source fence includes a protected or unknown history item".into(),
            );
        }
    }
    Ok(())
}

fn is_replaceable_task_boundary_summary(
    envelope: &ResponseItemEnvelope,
    thread_id: &str,
) -> Option<String> {
    let ResponseItem::Message {
        role,
        content,
        internal_chat_message_metadata_passthrough,
        ..
    } = &envelope.item
    else {
        return None;
    };
    if role != "user"
        || !internal_chat_message_metadata_passthrough
            .as_ref()
            .and_then(|metadata| metadata.content_item_kinds.as_ref())
            .is_some_and(|kinds| {
                !kinds.is_empty()
                    && kinds.len() == content.len()
                    && kinds.iter().all(|kind| kind.0 == "compaction.summary")
            })
    {
        return None;
    }
    let value = envelope
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.rencrow_compaction.as_ref())?;
    let summary_text = content
        .iter()
        .map(|item| match item {
            ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                Some(text.as_str())
            }
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?
        .concat();
    RenCrowTaskCheckpointMetadataV3::parse_and_validate(value.clone(), thread_id, &summary_text)
        .ok()
        .filter(|checkpoint| checkpoint.mode == TaskCheckpointMode::TaskBoundary)
        .and_then(|checkpoint| checkpoint.task_state)
        .and_then(|value| StoredTaskState::parse_and_validate(value, thread_id).ok())
        .map(|state| state.boundary.root_task_id)
}

fn tool_call_pair(item: &ResponseItem) -> Option<(&str, bool)> {
    match item {
        ResponseItem::FunctionCall { call_id, .. }
        | ResponseItem::CustomToolCall { call_id, .. } => Some((call_id, true)),
        ResponseItem::FunctionCallOutput {
            call_id: Some(call_id),
            ..
        }
        | ResponseItem::CustomToolCallOutput { call_id, .. } => Some((call_id, false)),
        _ => None,
    }
}

#[cfg(test)]
#[path = "rencrow_compaction_tests.rs"]
mod tests;
