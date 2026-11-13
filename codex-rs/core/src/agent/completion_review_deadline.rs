use super::control::LocalAgentControl;
use crate::agent::status::is_final;
use crate::codex_thread::CodexThread;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::TurnAbortReason;

pub(crate) enum CompletionReviewAbort {
    AbortedAndObserved,
    AlreadyTerminal,
    Unsafe { reason: String, fingerprint: String },
}

fn review_safety_fingerprint(value: serde_json::Value) -> String {
    codex_history::compaction_candidate::digest(&value).unwrap_or_else(|_| "0".repeat(64))
}

async fn review_ended_event_is_durable(thread: &CodexThread, turn_id: &str) -> bool {
    if thread.session.flush_rollout().await.is_err() {
        return false;
    }
    let Ok(stored) = thread
        .read_thread(
            /*include_archived*/ true, /*include_history*/ true,
        )
        .await
    else {
        return false;
    };
    stored.history.is_some_and(|history| {
        history.items.iter().any(|item| {
            matches!(
                item,
                codex_history::RolloutItem::EventMsg(EventMsg::TurnAborted(event))
                    if event.turn_id.as_deref() == Some(turn_id)
                        && event.reason == TurnAbortReason::ReviewEnded
            )
        })
    })
}

impl LocalAgentControl {
    /// Reject generic follow-up input only for the exact generation of an expired host reviewer.
    pub(crate) async fn ensure_completion_review_input_allowed(
        &self,
        reviewer_thread: &CodexThread,
    ) -> CodexResult<()> {
        let SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id, ..
        }) = &reviewer_thread.session_source
        else {
            return Ok(());
        };
        let Ok(manager) = self.upgrade() else {
            return Ok(());
        };
        let Ok(parent_thread) = manager.get_thread(*parent_thread_id).await else {
            return Ok(());
        };
        parent_thread
            .session
            .enforce_due_completion_review_deadlines()
            .await;
        let escalations = parent_thread
            .session
            .completion_review_deadline_escalations(&[reviewer_thread.session.thread_id])
            .await;
        let expired_generations = parent_thread
            .session
            .expired_completion_review_generations(reviewer_thread.session.thread_id)
            .await;
        if expired_generations.is_empty() {
            return Ok(());
        }
        let (current_turn_id, _) = reviewer_thread.session.agent_status_generation().await;
        if let Some(expected_turn_id) = expired_generations.iter().find(|expected_turn_id| {
            current_turn_id
                .as_deref()
                .is_none_or(|current| current == expected_turn_id.as_str())
                || expected_turn_id.starts_with("pending-generation:")
        }) {
            let reason = escalations
                .get(&reviewer_thread.session.thread_id)
                .filter(|(turn_id, _)| turn_id == expected_turn_id)
                .map_or_else(
                    || "the expired reviewer result has not been collected".to_string(),
                    |(_, reason)| reason.clone(),
                );
            return Err(CodexErr::InvalidRequest(format!(
                "completion reviewer deadline expired for this generation; input was withheld: {reason}"
            )));
        }
        Ok(())
    }

    /// Stop only the host-bound, read-only reviewer generation when no tool runtime has started.
    pub(crate) async fn abort_completion_review_turn_if_model_only(
        &self,
        parent_thread_id: ThreadId,
        reviewer_thread_id: ThreadId,
        reviewer_turn_id: &str,
        reviewer_role: &str,
        previous_safety_fingerprint: Option<&str>,
    ) -> CompletionReviewAbort {
        let Ok(state) = self.upgrade() else {
            return CompletionReviewAbort::Unsafe {
                reason: "thread manager is unavailable".into(),
                fingerprint: review_safety_fingerprint(
                    serde_json::json!({"manager": "unavailable"}),
                ),
            };
        };
        let Ok(thread) = state.get_thread(reviewer_thread_id).await else {
            return CompletionReviewAbort::Unsafe {
                reason: "reviewer thread is unavailable".into(),
                fingerprint: review_safety_fingerprint(
                    serde_json::json!({"reviewer": "unavailable"}),
                ),
            };
        };
        let expected_source = matches!(
            &thread.session_source,
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: source_parent,
                agent_role: Some(source_role),
                ..
            }) if *source_parent == parent_thread_id && source_role == reviewer_role
        );
        let metadata_matches = self
            .get_agent_metadata(reviewer_thread_id)
            .and_then(|metadata| metadata.agent_role)
            .as_deref()
            == Some(reviewer_role);
        let readonly_config = thread
            .config()
            .await
            .rencrow_execution_guard
            .as_ref()
            .is_some_and(|guard| !guard.requires_edit());
        if !expected_source || !metadata_matches || !readonly_config {
            return CompletionReviewAbort::Unsafe {
                reason: "reviewer identity, parent lineage, or read-only guard did not match"
                    .into(),
                fingerprint: review_safety_fingerprint(serde_json::json!({
                    "identity_matches": expected_source,
                    "metadata_matches": metadata_matches,
                    "readonly_config": readonly_config,
                })),
            };
        }

        let (turn_id, status) = thread.session.agent_status_generation().await;
        if turn_id.as_deref() != Some(reviewer_turn_id) {
            return CompletionReviewAbort::Unsafe {
                reason: "reviewer generation changed before deadline handling".into(),
                fingerprint: review_safety_fingerprint(serde_json::json!({
                    "turn_id": turn_id,
                    "status": format!("{status:?}"),
                })),
            };
        }
        if matches!(&status, AgentStatus::Errored(reason) if reason == "ReviewEnded") {
            return if review_ended_event_is_durable(&thread, reviewer_turn_id).await {
                CompletionReviewAbort::AbortedAndObserved
            } else {
                CompletionReviewAbort::Unsafe {
                    reason: "ReviewEnded status was observed but its rollout event was not durable"
                        .into(),
                    fingerprint: review_safety_fingerprint(serde_json::json!({
                        "turn_id": reviewer_turn_id,
                        "terminal_event": "ReviewEnded not durable",
                    })),
                }
            };
        }
        if is_final(&status) {
            return CompletionReviewAbort::AlreadyTerminal;
        }

        // Capture the per-turn dispatch epoch before reading the external inventories. If a
        // tool starts and finishes between those reads, the epoch change still invalidates the
        // safety observation at the abort linearization point.
        let dispatch_snapshot = thread
            .session
            .reviewer_tool_dispatch_snapshot(reviewer_turn_id)
            .await;
        let background_terminals = thread.list_background_terminals().await;
        let active_code_mode_cell = thread.session.services.code_mode_service.has_active_cells();
        let (tool_dispatch_epoch, in_flight_tools) = dispatch_snapshot
            .map(|(epoch, in_flight)| (Some(epoch), in_flight))
            .unwrap_or((None, 0));
        let background_ids = background_terminals
            .iter()
            .map(|terminal| format!("{}:{}", terminal.process_id, terminal.item_id))
            .collect::<Vec<_>>()
            .join(",");
        let fingerprint = review_safety_fingerprint(serde_json::json!({
            "generation": reviewer_turn_id,
            "status": format!("{status:?}"),
            "tool_dispatch_epoch": tool_dispatch_epoch,
            "in_flight_tools": in_flight_tools,
            "background_terminals": background_ids,
            "active_code_mode_cell": active_code_mode_cell,
        }));
        let safe_model_only = dispatch_snapshot.is_some()
            && in_flight_tools == 0
            && background_terminals.is_empty()
            && !active_code_mode_cell;
        if !safe_model_only {
            let reason = if in_flight_tools > 0 {
                "reviewer has an in-flight tool future"
            } else if !background_terminals.is_empty() {
                "reviewer has a background terminal"
            } else if active_code_mode_cell {
                "reviewer has an active code-mode cell"
            } else {
                "exact reviewer generation is not an active regular turn"
            };
            return CompletionReviewAbort::Unsafe {
                reason: reason.into(),
                fingerprint,
            };
        }
        let abort_noop_fingerprint = review_safety_fingerprint(serde_json::json!({
            "state": fingerprint.clone(),
            "result": "abort-noop",
        }));
        if previous_safety_fingerprint == Some(fingerprint.as_str()) {
            return CompletionReviewAbort::Unsafe {
                reason: "reviewer safety state has not changed since the prior escalation".into(),
                fingerprint,
            };
        }
        if previous_safety_fingerprint == Some(abort_noop_fingerprint.as_str()) {
            return CompletionReviewAbort::Unsafe {
                reason: "reviewer safety state has not changed since the prior escalation".into(),
                fingerprint: abort_noop_fingerprint,
            };
        }

        match thread
            .session
            .abort_model_only_turn_if_active(
                reviewer_turn_id,
                tool_dispatch_epoch.expect("safe model-only state has a dispatch epoch"),
                TurnAbortReason::ReviewEnded,
            )
            .await
        {
            Ok(true) => {}
            Ok(false) => {
                return CompletionReviewAbort::Unsafe {
                    reason: "exact model-only reviewer turn was no longer active".into(),
                    fingerprint: abort_noop_fingerprint,
                };
            }
            Err(error) => {
                return CompletionReviewAbort::Unsafe {
                    reason: error,
                    fingerprint: review_safety_fingerprint(serde_json::json!({
                        "state": fingerprint.clone(),
                        "result": "abort-flush-failed",
                    })),
                };
            }
        }

        let terminal = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let (turn_id, status) = thread.session.agent_status_generation().await;
                if turn_id.as_deref() == Some(reviewer_turn_id) && is_final(&status) {
                    return Some(status);
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .ok()
        .flatten();
        match terminal {
            Some(AgentStatus::Errored(reason)) if reason == "ReviewEnded" => {
                if review_ended_event_is_durable(&thread, reviewer_turn_id).await {
                    CompletionReviewAbort::AbortedAndObserved
                } else {
                    CompletionReviewAbort::Unsafe {
                        reason:
                            "ReviewEnded status was observed but its rollout event was not durable"
                                .into(),
                        fingerprint: review_safety_fingerprint(serde_json::json!({
                            "turn_id": reviewer_turn_id,
                            "terminal_event": "ReviewEnded not durable",
                        })),
                    }
                }
            }
            Some(_) => CompletionReviewAbort::AlreadyTerminal,
            None => CompletionReviewAbort::Unsafe {
                reason:
                    "reviewer abort was requested but its exact terminal state was not observed"
                        .into(),
                fingerprint: review_safety_fingerprint(serde_json::json!({
                    "turn_id": reviewer_turn_id,
                    "abort": "terminal unobserved",
                })),
            },
        }
    }
}
