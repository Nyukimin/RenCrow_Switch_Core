use super::session::Session;
use crate::context::ContextualUserFragment;
use crate::context::HookAdditionalContext;
use crate::session::turn_context::TurnContext;
use codex_history::RenCrowTaskCheckpointMetadataV3;
use codex_history::ResponseItemEnvelope;
use codex_history::TaskBoundarySourceFence;
use codex_history::TaskBoundarySourceRef;
use codex_history::TaskCheckpointMode;
use codex_history::compaction_candidate::digest;
use codex_history::compaction_candidate::history_digest;
use codex_history::task_context::StoredTaskState;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::InternalChatMessageMetadataPassthrough;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::SessionSource;
use std::collections::HashMap;
use std::collections::HashSet;
use tokio_util::sync::CancellationToken;

impl Session {
    /// Commit a prepared task boundary after the current response's tool results have been
    /// recorded, before another model request can observe the old context.
    pub(crate) async fn commit_prepared_task_boundary(
        &self,
        turn: &TurnContext,
        cancellation_token: &CancellationToken,
    ) -> Result<bool> {
        let Some(pending) = self.pending_task_boundary().await else {
            return Ok(false);
        };
        let Some(active) = self.active_task_scope().await else {
            return self
                .discard_prepared_task_boundary(
                    turn,
                    &pending,
                    "prepared task boundary has no active task scope",
                )
                .await;
        };
        if cancellation_token.is_cancelled() {
            return Err(CodexErr::TurnAborted);
        }
        if pending.expected_turn_id != turn.sub_id
            || pending.state.boundary.thread_id != self.thread_id().to_string()
            || pending.state.boundary.root_task_id != active.root_task_id
            || pending.state.boundary.closed_task_id != active.task_id
            || pending.state.boundary.closed_task_revision != active.task_revision
            || !matches!(
                &turn.session_source,
                SessionSource::Cli | SessionSource::VSCode | SessionSource::Exec
            )
            || !turn.config.rencrow_compaction
        {
            return self
                .discard_prepared_task_boundary(
                    turn,
                    &pending,
                    "prepared task boundary identity or authorization changed before commit",
                )
                .await;
        }

        if !self
            .services
            .unified_exec_manager
            .list_processes()
            .await
            .is_empty()
        {
            return self
                .discard_prepared_task_boundary(
                    turn,
                    &pending,
                    "task boundary commit was held while a unified exec process was running",
                )
                .await;
        }
        if self.services.agent_control.has_registered_agents() {
            let agents = self
                .services
                .agent_control
                .list_agents(&turn.session_source, None)
                .await?;
            if agents.iter().any(|agent| {
                matches!(
                    agent.status,
                    AgentStatus::PendingInit | AgentStatus::Running | AgentStatus::Interrupted
                )
            }) {
                return self
                    .discard_prepared_task_boundary(
                        turn,
                        &pending,
                        "task boundary commit was held while a child agent was unresolved",
                    )
                    .await;
            }
        }

        let history = self.clone_history().await;
        let originals = history.annotated_items();
        let suffix = originals
            .get(pending.prepared_history_len..)
            .unwrap_or_default();
        let exact_accept_call = matches!(
            suffix.first().map(|row| &row.item),
            Some(ResponseItem::FunctionCall {
                name,
                call_id,
                arguments,
                ..
            }) if name == "task_context_boundary"
                && call_id == &pending.accept_call_id
                && arguments == &pending.accept_call_arguments
        );
        let exact_accept_output = matches!(
            suffix.get(1).map(|row| &row.item),
            Some(
                ResponseItem::FunctionCallOutput {
                    call_id: Some(call_id),
                    ..
                }
                | ResponseItem::CustomToolCallOutput { call_id, .. }
            ) if call_id == &pending.accept_call_id
        );
        if suffix.len() != 2
            || !exact_accept_call
            || !exact_accept_output
            || history_digest(&originals[..pending.prepared_history_len])
                .map_err(CodexErr::InvalidRequest)?
                != pending.prepared_history_hash
        {
            return self
                .discard_prepared_task_boundary(
                    turn,
                    &pending,
                    "history changed after task boundary preparation",
                )
                .await;
        }
        if active.start_history_cursor > originals.len() {
            return self
                .discard_prepared_task_boundary(
                    turn,
                    &pending,
                    "task scope history cursor is outside the current history",
                )
                .await;
        }
        if validate_call_pair(originals, &active.begin_call_id).is_err()
            || validate_call_pair(originals, &pending.accept_call_id).is_err()
        {
            return self
                .discard_prepared_task_boundary(
                    turn,
                    &pending,
                    "task boundary begin or accept call/result pair is incomplete",
                )
                .await;
        }
        if !unresolved_calls_since(originals, active.start_history_cursor).is_empty() {
            return self
                .discard_prepared_task_boundary(
                    turn,
                    &pending,
                    "task boundary commit was held because a tool call in this scope has no result",
                )
                .await;
        }

        let verified_paths = pending
            .verified_artifacts
            .iter()
            .map(|artifact| artifact.path.clone())
            .collect::<Vec<_>>();
        let Some(current_artifacts) = crate::task_check::hash_workspace_artifacts(
            &pending.verification_workspace_root,
            &verified_paths,
        ) else {
            return self
                .discard_prepared_task_boundary(
                    turn,
                    &pending,
                    "verified task artifacts are no longer available at commit time",
                )
                .await;
        };
        if current_artifacts != pending.verified_artifacts {
            return self
                .discard_prepared_task_boundary(
                    turn,
                    &pending,
                    "verified task artifacts changed after acceptance; checkpoint was not committed",
                )
                .await;
        }

        // Reuse the ordinary host history projection. Only verified patch pairs and assistant
        // work classified as non-opaque, unprotected Work can be removed.
        let verified_patch_call_ids = self
            .verified_task_patch_call_ids(
                &active,
                &pending.verification_workspace_root,
                &pending.verified_artifacts,
            )
            .await;
        let mut call_indices = HashMap::<String, (usize, String)>::new();
        let mut output_indices = HashMap::<String, usize>::new();
        for (index, envelope) in originals
            .iter()
            .enumerate()
            .skip(active.start_history_cursor)
        {
            match &envelope.item {
                ResponseItem::FunctionCall { call_id, name, .. }
                    if name != "task_context_boundary" =>
                {
                    call_indices.insert(call_id.clone(), (index, name.clone()));
                }
                ResponseItem::CustomToolCall { call_id, name, .. }
                    if name != "task_context_boundary" =>
                {
                    call_indices.insert(call_id.clone(), (index, name.clone()));
                }
                ResponseItem::FunctionCallOutput {
                    call_id: Some(call_id),
                    ..
                }
                | ResponseItem::CustomToolCallOutput { call_id, .. } => {
                    output_indices.insert(call_id.clone(), index);
                }
                _ => {}
            }
        }
        let mut verified_patch_pairs = Vec::new();
        for call_id in &verified_patch_call_ids {
            let Some((call_index, name)) = call_indices.get(call_id) else {
                continue;
            };
            let Some(output_index) = output_indices.get(call_id) else {
                continue;
            };
            if name == "apply_patch" {
                verified_patch_pairs.push((*call_index, *output_index));
            }
        }
        let work_indices = crate::compact::task_boundary_work_indices(
            originals,
            &self.thread_id().to_string(),
            &verified_patch_pairs,
        )
        .map_err(CodexErr::InvalidRequest)?;
        let mut removed = HashSet::<usize>::new();
        for (index, envelope) in originals.iter().enumerate() {
            if is_previous_task_boundary_summary(
                envelope,
                &self.thread_id().to_string(),
                &pending.state.boundary.root_task_id,
            ) {
                removed.insert(index);
            }
        }
        for (index, envelope) in originals
            .iter()
            .enumerate()
            .skip(active.start_history_cursor)
        {
            if work_indices.contains(&index)
                && matches!(
                    &envelope.item,
                    ResponseItem::Message { role, .. } if role == "assistant"
                )
            {
                removed.insert(index);
            }
        }
        for (call_index, output_index) in verified_patch_pairs {
            if work_indices.contains(&call_index) && work_indices.contains(&output_index) {
                removed.insert(call_index);
                removed.insert(output_index);
            }
        }

        let history_hash = history_digest(originals).map_err(CodexErr::InvalidRequest)?;
        let mut removed_refs = removed.iter().copied().collect::<Vec<_>>();
        removed_refs.sort_unstable();
        let removed_refs = removed_refs
            .into_iter()
            .map(|index| {
                digest(&(&originals[index].item, &originals[index].metadata))
                    .map(|hash| TaskBoundarySourceRef {
                        id: format!("item:{index}"),
                        hash,
                    })
                    .map_err(CodexErr::InvalidRequest)
            })
            .collect::<Result<Vec<_>>>()?;
        let source_fence = TaskBoundarySourceFence {
            history_hash: history_hash.clone(),
            removed_refs,
        };
        let mut replacement = originals
            .iter()
            .enumerate()
            .filter(|(index, _)| !removed.contains(index))
            .map(|(_, envelope)| envelope.clone())
            .collect::<Vec<_>>();
        replacement.push(ResponseItemEnvelope::new(ResponseItem::Message {
            id: None,
            role: "user".into(),
            content: vec![ContentItem::InputText {
                text: pending.summary_text.clone(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: Some(
                InternalChatMessageMetadataPassthrough {
                    content_item_kinds: Some(vec![ContentItemKind("compaction.summary".into())]),
                    ..Default::default()
                },
            ),
        }));

        let is_subtask =
            pending.state.boundary.closed_task_id != pending.state.boundary.root_task_id;
        let accepted_completion_state = pending.state.boundary.completion_state.clone();
        let accepted_root_task_id = pending.state.boundary.root_task_id.clone();
        self.commit_task_boundary_checkpoint(
            history_hash,
            pending.expected_turn_id.clone(),
            replacement,
            pending.summary_text,
            pending.state,
            source_fence,
        )
        .await?;
        if let Some(completion_state) = accepted_completion_state
            && let Err(error) = self
                .set_rencrow_completion_state(
                    accepted_root_task_id,
                    serde_json::to_value(completion_state)
                        .map_err(|error| CodexErr::InvalidRequest(error.to_string()))?,
                )
                .await
        {
            self.pause_autonomous_completion().await;
            tracing::error!(%error, "accepted task boundary committed but completion ledger refresh failed; autonomous completion paused");
        }
        if is_subtask {
            self.note_autonomous_completion_unit_accepted().await;
        }

        let scope_closed = {
            let mut state = self.state.lock().await;
            if state
                .pending_task_boundary
                .as_ref()
                .is_some_and(|current| current.accept_call_id == pending.accept_call_id)
            {
                state.pending_task_boundary = None;
                state.active_task_scope = None;
                state
                    .task_patch_attempts
                    .retain(|_, attempt| attempt.scope != active);
                state.task_boundary_committed_turn = Some(turn.sub_id.clone());
                true
            } else {
                false
            }
        };
        if !scope_closed {
            return Err(CodexErr::InvalidRequest(
                "prepared task boundary changed after checkpoint commit".into(),
            ));
        }
        if turn.config.rencrow_autonomous_completion
            && let Err(error) = self.persist_autonomous_completion_snapshot().await
        {
            self.pause_autonomous_completion().await;
            return Err(CodexErr::InvalidRequest(format!(
                "checkpoint committed, scope-closure snapshot persistence failed: {error}"
            )));
        }
        Ok(true)
    }

    async fn discard_prepared_task_boundary(
        &self,
        turn: &TurnContext,
        pending: &crate::state::PendingTaskBoundary,
        reason: &str,
    ) -> Result<bool> {
        {
            let mut state = self.state.lock().await;
            if state
                .pending_task_boundary
                .as_ref()
                .is_some_and(|current| current.accept_call_id == pending.accept_call_id)
            {
                state.pending_task_boundary = None;
            }
        }
        let notice: ResponseItem = ContextualUserFragment::into(HookAdditionalContext::new(
            format!(
                "Task boundary was not committed. The active task remains open and can be retried. Reason: {reason}."
            ),
        ));
        self.record_conversation_items(turn, turn.model_info(), &[notice])
            .await;
        tracing::warn!(
            reason,
            "discarded invalid prepared task boundary; active task scope remains available for retry"
        );
        Ok(false)
    }

    pub(crate) async fn take_task_boundary_committed_turn(&self, turn_id: &str) -> bool {
        let mut state = self.state.lock().await;
        if state.task_boundary_committed_turn.as_deref() == Some(turn_id) {
            state.task_boundary_committed_turn = None;
            true
        } else {
            false
        }
    }
}

fn is_previous_task_boundary_summary(
    envelope: &ResponseItemEnvelope,
    thread_id: &str,
    root_task_id: &str,
) -> bool {
    let ResponseItem::Message {
        role,
        content,
        internal_chat_message_metadata_passthrough,
        ..
    } = &envelope.item
    else {
        return false;
    };
    if role != "user"
        || !internal_chat_message_metadata_passthrough
            .as_ref()
            .and_then(|metadata| metadata.content_item_kinds.as_ref())
            .is_some_and(|kinds| {
                !kinds.is_empty() && kinds.iter().all(|kind| kind.0 == "compaction.summary")
            })
    {
        return false;
    }
    let Some(value) = envelope
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.rencrow_compaction.as_ref())
    else {
        return false;
    };
    let Some(summary_text) = content
        .iter()
        .map(|item| match item {
            ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                Some(text.as_str())
            }
            _ => None,
        })
        .collect::<Option<Vec<_>>>()
        .map(|parts| parts.concat())
    else {
        return false;
    };
    RenCrowTaskCheckpointMetadataV3::parse_and_validate(value.clone(), thread_id, &summary_text)
        .is_ok_and(|checkpoint| {
            checkpoint.mode == TaskCheckpointMode::TaskBoundary
                && checkpoint
                    .task_state
                    .and_then(|value| serde_json::from_value::<StoredTaskState>(value).ok())
                    .is_some_and(|state| state.boundary.root_task_id == root_task_id)
        })
}

fn validate_call_pair(items: &[ResponseItemEnvelope], call_id: &str) -> Result<()> {
    let call_exists = items.iter().any(|envelope| match &envelope.item {
        ResponseItem::FunctionCall { call_id: id, .. }
        | ResponseItem::CustomToolCall { call_id: id, .. } => id == call_id,
        _ => false,
    });
    let output_exists = items.iter().any(|envelope| match &envelope.item {
        ResponseItem::FunctionCallOutput {
            call_id: Some(id), ..
        }
        | ResponseItem::CustomToolCallOutput { call_id: id, .. } => id == call_id,
        _ => false,
    });
    if call_exists && output_exists {
        Ok(())
    } else {
        Err(CodexErr::InvalidRequest(
            "task boundary call/result pair is incomplete".into(),
        ))
    }
}

fn unresolved_calls_since(items: &[ResponseItemEnvelope], start: usize) -> Vec<String> {
    let mut calls = HashMap::<String, String>::new();
    let mut outputs = HashSet::<String>::new();
    for envelope in items.iter().skip(start) {
        match &envelope.item {
            ResponseItem::FunctionCall { call_id, name, .. }
            | ResponseItem::CustomToolCall { call_id, name, .. }
                if name != "task_context_boundary" =>
            {
                calls.insert(call_id.clone(), name.clone());
            }
            ResponseItem::FunctionCallOutput {
                call_id: Some(call_id),
                ..
            }
            | ResponseItem::CustomToolCallOutput { call_id, .. } => {
                outputs.insert(call_id.clone());
            }
            _ => {}
        }
    }
    calls
        .into_iter()
        .filter(|(call_id, _)| !outputs.contains(call_id))
        .map(|(call_id, name)| format!("{name}:{call_id}"))
        .collect()
}
