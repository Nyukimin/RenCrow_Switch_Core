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
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result;
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
    pub response_id: String,
    pub expected_turn: String,
    pub expected_base_text: String,
    pub expected_world: Option<WorldStateItem>,
}

impl Session {
    pub(crate) async fn check_rencrow_checkpoint(&self) -> Result<()> {
        if self.state.lock().await.rencrow_checkpoint_failed {
            return Err(CodexErr::Fatal("Compaction checkpoint persistence was uncertain; restart to recover the last readable owner checkpoint before continuing".into()));
        }
        Ok(())
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

    pub(crate) async fn commit_rencrow_checkpoint(
        &self,
        mut candidate: RenCrowCheckpoint,
        activity: &tokio::sync::watch::Receiver<super::input_queue::InputQueueActivity>,
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
                || activity.has_changed().unwrap_or(true)
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
                compaction_response_id: Some(candidate.response_id),
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

#[cfg(test)]
#[path = "rencrow_compaction_tests.rs"]
mod tests;
