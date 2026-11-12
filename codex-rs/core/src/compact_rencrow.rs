// Modified by RenCrow Switch Core, 2026-09-22; V2 orchestration 2026-09-24.
//! V2 compaction: Normal with optional selection and one summary, deterministic Emergency
//! fallback, and explicit blocked states.
use super::*;
use codex_history::compaction_candidate::CandidateInput;
use codex_history::compaction_candidate::history_digest;
use codex_history::compaction_checkpoint_metadata::CheckpointResponseStage;
use codex_history::compaction_checkpoint_metadata::CompactionModelResponseReceipt;
use codex_history::compaction_preprocess::InstructionObservationLink;
use codex_history::task_context::StoredTaskState;
use codex_rollout::RolloutRecorder;
use codex_thread_store::ReadCompactionSourcesParams;
use codex_thread_store::ThreadStoreError;
use serde_json::Value;
use std::collections::HashSet;

#[path = "compact_rencrow_candidate.rs"]
mod candidate;
#[path = "compact_rencrow_emergency.rs"]
mod emergency;
#[path = "compact_rencrow_history.rs"]
mod history;
#[path = "compact_rencrow_request.rs"]
mod model_request;
#[path = "compact_rencrow_native.rs"]
mod native;
#[path = "compact_rencrow_observation.rs"]
mod observation;
#[path = "compact_rencrow_stages.rs"]
mod stages;
#[path = "compact_rencrow_summary.rs"]
mod summary;
#[path = "compact_rencrow_timeline.rs"]
mod timeline;

use stages::StageFailure;

pub(crate) use stages::attach_task_boundary_metadata;
pub(super) use summary::should_apply_server_reasoning_included;

pub(crate) fn latest_task_state_in_history(
    items: &[codex_history::ResponseItemEnvelope],
    thread_id: &str,
) -> Result<Option<StoredTaskState>, String> {
    let Some(adopted) = summary::find_adopted_v2_checkpoint(items, thread_id)? else {
        return Ok(None);
    };
    adopted
        .task_state
        .map(|state| StoredTaskState::parse_and_validate(state, thread_id))
        .transpose()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommittedTaskCheckpointInfo {
    pub committed_transaction_hash: String,
    pub task_boundary_index: usize,
    pub task_boundary_summary_hash: String,
    pub root_task_id: String,
    pub closed_task_id: String,
    pub task_revision: u64,
    pub closed_task_revision: u64,
}

pub fn inspect_committed_task_checkpoint_window(
    transaction_window: &[codex_history::RolloutItem],
    expected_thread_id: &str,
    expected_root_task_id: &str,
) -> Result<CommittedTaskCheckpointInfo, String> {
    let Some(codex_history::RolloutItem::Compacted(raw_target)) = transaction_window.first() else {
        return Err("task checkpoint transaction does not begin with a compacted row".into());
    };
    let raw_replacement = raw_target
        .replacement_history
        .as_ref()
        .ok_or_else(|| "task checkpoint target has no replacement history".to_owned())?;
    let raw_capacity = raw_replacement
        .last()
        .and_then(|envelope| envelope.metadata.as_ref())
        .and_then(|metadata| metadata.rencrow_compaction.as_ref())
        .ok_or_else(|| "task checkpoint target has no capacity metadata".to_owned())?;
    if raw_capacity.get("version").and_then(Value::as_u64) != Some(3)
        || raw_capacity.get("mode").and_then(Value::as_str) != Some("capacity")
        || raw_capacity.get("task_state").is_none_or(Value::is_null)
        || raw_capacity
            .get("transaction_following_items")
            .and_then(Value::as_u64)
            .is_none()
        || raw_capacity
            .get("committed_transaction_hash")
            .is_some_and(|hash| !hash.is_null())
    {
        return Err(
            "latest task checkpoint is not an owner-prepared V3 capacity transaction".into(),
        );
    }
    let following_items = raw_capacity["transaction_following_items"]
        .as_u64()
        .and_then(|count| usize::try_from(count).ok())
        .ok_or_else(|| "task checkpoint transaction window is invalid".to_owned())?;
    let expected_window_len = following_items
        .checked_add(2)
        .ok_or_else(|| "task checkpoint transaction window is too large".to_owned())?;
    if transaction_window.len() != expected_window_len {
        return Err("task checkpoint transaction window is not exact".into());
    }
    let committed = codex_history::compaction_transaction::committed_items(transaction_window)?;
    let Some(codex_history::RolloutItem::Compacted(target)) = committed.first() else {
        return Err(
            "latest task checkpoint transaction was not committed by the history owner".into(),
        );
    };
    let replacement_history = target
        .replacement_history
        .as_deref()
        .ok_or_else(|| "committed task checkpoint has no replacement history".to_owned())?;
    let committed_transaction_hash = replacement_history
        .last()
        .and_then(|envelope| envelope.metadata.as_ref())
        .and_then(|metadata| metadata.rencrow_compaction.as_ref())
        .and_then(|metadata| metadata.get("committed_transaction_hash"))
        .and_then(Value::as_str)
        .filter(|hash| hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or_else(|| "history owner did not attach a committed transaction hash".to_owned())?
        .to_owned();
    let adopted = summary::find_adopted_v2_checkpoint(replacement_history, expected_thread_id)?
        .ok_or_else(|| "committed task checkpoint has no adopted task context".to_owned())?;
    let task_boundary_index = adopted.task_context_index.ok_or_else(|| {
        "committed task checkpoint has no selected task-boundary envelope".to_owned()
    })?;
    let boundary_envelope = replacement_history
        .get(task_boundary_index)
        .ok_or_else(|| "selected task-boundary index is outside replacement history".to_owned())?;
    let boundary_value = boundary_envelope
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.rencrow_compaction.clone())
        .ok_or_else(|| "selected task boundary has no V3 metadata".to_owned())?;
    let boundary = codex_history::task_checkpoint_metadata::RenCrowTaskCheckpointMetadataV3::parse_and_validate(
        boundary_value,
        expected_thread_id,
        summary::checkpoint_summary_body(boundary_envelope)?,
    )?;
    if boundary.mode != codex_history::task_checkpoint_metadata::TaskCheckpointMode::TaskBoundary {
        return Err("selected task context is not a V3 task-boundary checkpoint".into());
    }
    let task_state = latest_task_state_in_history(replacement_history, expected_thread_id)?
        .ok_or_else(|| "committed task checkpoint has no validated task state".to_owned())?;
    if task_state.boundary.root_task_id != expected_root_task_id {
        return Err("committed task checkpoint belongs to another root task".into());
    }
    Ok(CommittedTaskCheckpointInfo {
        committed_transaction_hash,
        task_boundary_index,
        task_boundary_summary_hash: boundary.summary_hash,
        root_task_id: task_state.boundary.root_task_id,
        closed_task_id: task_state.boundary.closed_task_id,
        task_revision: task_state.boundary.revision,
        closed_task_revision: task_state.boundary.closed_task_revision,
    })
}

/// Classify task-boundary source items through the same host-owned history projection used by
/// ordinary compaction. Only explicitly supplied terminal work pairs are upgraded from Unknown.
pub(crate) fn task_boundary_work_indices(
    items: &[codex_history::ResponseItemEnvelope],
    thread_id: &str,
    completed_pairs: &[(usize, usize)],
) -> Result<HashSet<usize>, String> {
    let completed_work = completed_pairs
        .iter()
        .map(
            |(call_index, output_index)| history::CompletedWorkProjection {
                call_index: *call_index,
                output_index: *output_index,
                call_text: String::new(),
                output_text: String::new(),
            },
        )
        .collect::<Vec<_>>();
    let input = history::capture(
        items,
        format!("task-boundary:{thread_id}"),
        thread_id,
        vec![serde_json::json!({"task_boundary_thread_id": thread_id})],
        &completed_work,
    )?;
    Ok(input
        .records
        .iter()
        .enumerate()
        .filter(|(_, record)| {
            record.origin == codex_history::compaction_candidate::Origin::Work
                && record.protected.is_empty()
                && record.opaque.is_none()
        })
        .map(|(index, _)| index)
        .collect())
}

#[cfg(test)]
#[path = "compact_rencrow_orchestration_tests.rs"]
mod orchestration_tests;

/// Decide whether instruction selection is needed at all (Annex A F13).
///
/// Selection is needed only when a host completion link exists or a verified Human appears after
/// the semantic boundary. Humans before the boundary were already considered by an accepted
/// selection; when a new Human arrives, the candidate payload still presents every active Human.
/// Without a boundary every verified Human is new.
pub(super) fn selection_required(
    input: &CandidateInput,
    semantic_boundary: Option<usize>,
    links: &[InstructionObservationLink],
) -> bool {
    !links.is_empty()
        || input.records.iter().enumerate().any(|(index, record)| {
            record.origin.is_declared_input()
                && semantic_boundary.is_none_or(|boundary| index > boundary)
        })
}

/// Deterministically skip inference when no source can be selected.
pub(super) fn selection_request(
    input: &CandidateInput,
    payload: Option<Value>,
) -> Result<Option<codex_history::compaction_selection_wire::SelectionRequest>, String> {
    let Some(payload) = payload else {
        return Ok(None);
    };
    if !payload["sources"]
        .as_array()
        .ok_or("missing sources")?
        .iter()
        .any(|source| source["candidate"] == true)
    {
        return Ok(None);
    }
    codex_history::compaction_selection_wire::SelectionRequest::new(input, &payload).map(Some)
}

// Modified by RenCrow: only the host-owned ID contract is sent to the model.
pub(super) async fn select_obsolete_instructions(
    sess: &Session,
    ctx: &TurnContext,
    metadata: CompactionTurnMetadata,
    request: &codex_history::compaction_selection_wire::SelectionRequest,
    receipts: &mut Vec<CompactionModelResponseReceipt>,
    cancellation: &tokio_util::sync::CancellationToken,
) -> CodexResult<String> {
    let (response, seconds) =
        model_request::drain_instruction_selection(sess, ctx, metadata, request, cancellation)
            .await?;
    receipts.push(CompactionModelResponseReceipt {
        stage: CheckpointResponseStage::InstructionSelection,
        response_id: response.response_id,
        seconds,
        usage: response.token_usage,
    });
    model_request::json_stage_text(response.output)
}

/// Error returned when an automatic compaction would repeat a failure on an unchanged history.
/// A blocked state is not offered manual retry as its recovery (Part 2 §65).
const AUTO_RETRY_SUPPRESSED: &str = "RenCrow automatic compaction was not retried because the same unchanged history already failed compaction. The history was not changed.";

/// Shown when Normal failed for a semantic or model reason and Emergency committed (Part 2 §66).
const EMERGENCY_TRANSITION: &str = "Normal semantic compaction was unavailable. RenCrow continued with deterministic compaction. No source data was discarded.";

/// Decide whether an automatic compaction stops before any model request (Annex A F03).
///
/// Manual compaction is always allowed, even for a history whose automatic compaction failed.
pub(super) fn auto_retry_suppressed(
    trigger: CompactionTrigger,
    failed_hash: Option<&str>,
    history_hash: &str,
) -> bool {
    matches!(trigger, CompactionTrigger::Auto) && failed_hash == Some(history_hash)
}

/// Decide whether Normal is skipped because it already failed on this history (Part 2 §56).
///
/// An automatic compaction goes straight to Emergency; a manual `/compact` retries Normal, which
/// is how a temporary model failure is retried (§65).
pub(super) fn normal_skipped(
    trigger: CompactionTrigger,
    normal_failed_hash: Option<&str>,
    history_hash: &str,
) -> bool {
    matches!(trigger, CompactionTrigger::Auto) && normal_failed_hash == Some(history_hash)
}

/// Decide whether a failed compaction records the automatic-retry fingerprint (Annex A F03).
///
/// Cancellation and races where the history or settings changed before the failure do not
/// describe the unchanged snapshot, so they never suppress the next automatic attempt.
pub(super) fn records_auto_failure(
    trigger: CompactionTrigger,
    error: &CodexErr,
    cancelled: bool,
    snapshot_unchanged: bool,
) -> bool {
    matches!(trigger, CompactionTrigger::Auto)
        && !cancelled
        && snapshot_unchanged
        && !matches!(
            error.details(),
            CodexErrorDetails::Interrupted | CodexErrorDetails::TurnAborted
        )
}

/// Run one V2 compaction and commit it through the session owner (Part 2 §3).
///
/// Normal is tried first. A semantic or model failure continues with deterministic Emergency;
/// capacity and integrity failures end in an explicit blocked error without changing history.
pub(super) async fn run(
    sess: Arc<Session>,
    ctx: Arc<TurnContext>,
    injection: InitialContextInjection,
    metadata: CompactionTurnMetadata,
) -> CodexResult<String> {
    sess.check_rencrow_checkpoint().await?;
    let cancellation = sess
        .active_turn
        .lock()
        .await
        .as_ref()
        .and_then(|active| active.task.as_ref())
        .filter(|task| task.turn_context.sub_id == ctx.sub_id)
        .map(|task| task.cancellation_token.clone())
        .ok_or(CodexErr::TurnAborted)?;
    let item = TurnItem::ContextCompaction(ContextCompactionItem::new());
    sess.emit_turn_item_started(&ctx, &item).await;
    // Queued input is not in the snapshot; the turn records it after the checkpoint.
    let snapshot = sess.clone_history().await;
    let history_hash =
        history_digest(snapshot.annotated_items()).map_err(CodexErr::InvalidRequest)?;
    let settings = sess.thread_settings_snapshot().await;
    let trigger = metadata.trigger();
    if auto_retry_suppressed(
        trigger,
        sess.rencrow_auto_compaction_failed_hash().await.as_deref(),
        &history_hash,
    ) {
        return Err(CodexErr::InvalidRequest(AUTO_RETRY_SUPPRESSED.into()));
    }
    let skip_normal = normal_skipped(
        trigger,
        sess.rencrow_normal_failed_hash().await.as_deref(),
        &history_hash,
    );

    let result = compact_v2(
        &sess,
        &ctx,
        injection,
        metadata,
        &cancellation,
        &snapshot,
        history_hash.clone(),
        settings.clone(),
        skip_normal,
    )
    .await;
    let outcome = match result {
        Ok(outcome) => outcome,
        Err(error) => {
            let snapshot_unchanged = history_digest(sess.clone_history().await.annotated_items())
                .is_ok_and(|current| current == history_hash)
                && sess.thread_settings_snapshot().await == settings;
            if records_auto_failure(
                trigger,
                &error,
                cancellation.is_cancelled(),
                snapshot_unchanged,
            ) {
                sess.set_rencrow_auto_compaction_failed_hash(Some(history_hash))
                    .await;
            }
            return Err(error);
        }
    };
    sess.set_rencrow_auto_compaction_failed_hash(/*hash*/ None)
        .await;
    sess.set_rencrow_normal_failed_hash(/*hash*/ None).await;
    sess.recompute_token_usage(&ctx).await;
    sess.emit_turn_item_completed(&ctx, item).await;
    let message = match &outcome.model {
        Some(model) => format!(
            "RenCrow compaction applied: {} instruction removals, {} completed results, {} new observations, {} model requests. Model {model}.",
            outcome.removals, outcome.results, outcome.observations, outcome.requests
        ),
        None => format!(
            "{EMERGENCY_TRANSITION} {} observation markers.",
            outcome.observations
        ),
    };
    sess.send_event(&ctx, EventMsg::Warning(WarningEvent { message }))
        .await;
    Ok(outcome
        .summary_text
        .strip_prefix(&format!("{SUMMARY_PREFIX}\n"))
        .unwrap_or(&outcome.summary_text)
        .to_owned())
}

/// Facts about one committed checkpoint, reported after the durable commit.
struct CommittedOutcome {
    summary_text: String,
    removals: usize,
    results: usize,
    observations: usize,
    requests: usize,
    /// `None` for a deterministic emergency checkpoint.
    model: Option<String>,
}

/// Prepare once, try Normal unless it is skipped, fall back to Emergency, and commit.
#[allow(clippy::too_many_arguments)]
async fn compact_v2(
    sess: &Session,
    ctx: &TurnContext,
    injection: InitialContextInjection,
    metadata: CompactionTurnMetadata,
    cancellation: &tokio_util::sync::CancellationToken,
    snapshot: &crate::context_manager::ContextManager,
    history_hash: String,
    settings: codex_protocol::protocol::ThreadSettingsSnapshot,
    skip_normal: bool,
) -> CodexResult<CommittedOutcome> {
    // Only a mid-turn compaction has the step of the ordinary request its summary can extend.
    let step_context = match &injection {
        InitialContextInjection::BeforeLastUserMessage { step_context, .. } => {
            Some(Arc::clone(step_context))
        }
        InitialContextInjection::DoNotInject => None,
    };
    let (canonical, source_thread, active_call_ids) =
        load_canonical_rollout(sess, snapshot.annotated_items())
            .await
            .map_err(StageFailure::into_error)?;
    let expected_world = snapshot.world_state_checkpoint();
    let (initial_context, world_state) = build_compaction_initial_context(sess, &injection).await;
    let prepared = stages::prepare_v2(
        sess,
        ctx,
        snapshot,
        &canonical,
        &source_thread,
        &active_call_ids,
        &history_hash,
        &settings,
        initial_context,
    )
    .await
    .map_err(StageFailure::into_error)?;

    let normal = if skip_normal {
        None
    } else {
        match stages::normal_candidate(
            sess,
            ctx,
            metadata,
            cancellation,
            snapshot,
            &prepared,
            step_context.as_deref(),
        )
        .await
        {
            Ok(candidate) => Some(candidate),
            Err(StageFailure::Semantic(error)) => {
                tracing::warn!(%error, "RenCrow Normal compaction failed; using Emergency");
                sess.set_rencrow_normal_failed_hash(Some(history_hash.clone()))
                    .await;
                None
            }
            Err(failure) => return Err(failure.into_error()),
        }
    };
    let candidate = match normal {
        Some(candidate) => candidate,
        None => stages::emergency_candidate(ctx, snapshot, &prepared)
            .map_err(StageFailure::into_error)?,
    };
    let reference_context = match injection {
        InitialContextInjection::DoNotInject => None,
        InitialContextInjection::BeforeLastUserMessage { step_context, .. } => {
            Some(step_context.to_turn_context_item())
        }
    };
    let base = sess.get_base_instructions().await;
    sess.commit_rencrow_checkpoint(crate::session::RenCrowCheckpoint {
        items: candidate.items,
        expected_history_hash: history_hash,
        expected_settings: settings,
        reference_context,
        world_state,
        summary: candidate.summary_text.clone(),
        response_id: candidate.response_id,
        expected_turn: ctx.sub_id.clone(),
        expected_base_text: base.text,
        expected_world,
    })
    .await?;
    Ok(CommittedOutcome {
        summary_text: candidate.summary_text,
        removals: candidate.removals,
        results: candidate.results,
        observations: candidate.observations,
        requests: candidate.requests,
        model: candidate.model,
    })
}

/// Load the canonical rollout once for this compaction (Annex A F04).
///
/// Without persisted rollout storage no pair can be verified, so every tool item stays protected;
/// existing archive references and markers then fail closed in `prepare_compaction_sources`.
/// Unreadable storage aborts; a malformed or foreign rollout is an integrity conflict.
async fn load_canonical_rollout(
    sess: &Session,
    selected: &[codex_history::ResponseItemEnvelope],
) -> Result<
    (
        Vec<codex_history::RolloutItem>,
        codex_protocol::ThreadId,
        HashSet<String>,
    ),
    StageFailure,
> {
    let unavailable = |error: String| StageFailure::Abort(CodexErr::InvalidRequest(error));
    let active_call_ids = sess
        .list_background_terminals()
        .await
        .into_iter()
        .map(|terminal| terminal.item_id)
        .collect();
    let Some(path) = sess.current_rollout_path().await.map_err(|error| {
        unavailable(format!(
            "RenCrow compaction blocked: rollout unavailable: {error}"
        ))
    })?
    else {
        return Ok((Vec::new(), sess.thread_id(), active_call_ids));
    };
    sess.flush_rollout().await.map_err(|error| {
        unavailable(format!(
            "RenCrow compaction blocked: rollout flush failed: {error}"
        ))
    })?;
    let call_ids = compaction_source_call_ids(selected);
    match sess
        .services
        .thread_store
        .read_compaction_sources(ReadCompactionSourcesParams {
            thread_id: sess.thread_id(),
            call_ids,
        })
        .await
    {
        Ok(history) => {
            if history.thread_id != sess.thread_id() {
                return Err(StageFailure::Integrity(
                    "compaction source history belongs to another thread".into(),
                ));
            }
            return Ok((history.items, history.thread_id, active_call_ids));
        }
        Err(ThreadStoreError::Unsupported {
            operation: "read_compaction_sources",
        }) => {}
        Err(ThreadStoreError::InvalidRequest { message }) => {
            return Err(StageFailure::Integrity(message));
        }
        Err(error) => {
            return Err(unavailable(format!(
                "RenCrow compaction blocked: canonical source history read failed: {error}"
            )));
        }
    }
    let (items, thread_id, parse_errors) = RolloutRecorder::load_rollout_items(&path)
        .await
        .map_err(|error| {
            unavailable(format!(
                "RenCrow compaction blocked: rollout read failed: {error}"
            ))
        })?;
    if parse_errors != 0 {
        return Err(StageFailure::Integrity(format!(
            "rollout contains {parse_errors} parse errors"
        )));
    }
    let thread_id = thread_id
        .ok_or_else(|| StageFailure::Integrity("rollout has no canonical thread header".into()))?;
    if thread_id != sess.thread_id() {
        return Err(StageFailure::Integrity(
            "rollout thread does not match live session".into(),
        ));
    }
    Ok((items, thread_id, active_call_ids))
}

fn compaction_source_call_ids(items: &[codex_history::ResponseItemEnvelope]) -> Vec<String> {
    let mut call_ids = items
        .iter()
        .filter_map(|envelope| match &envelope.item {
            codex_protocol::models::ResponseItem::FunctionCall { call_id, .. }
            | codex_protocol::models::ResponseItem::CustomToolCall { call_id, .. }
            | codex_protocol::models::ResponseItem::CustomToolCallOutput { call_id, .. } => {
                Some(call_id.clone())
            }
            codex_protocol::models::ResponseItem::FunctionCallOutput {
                call_id: Some(call_id),
                ..
            } => Some(call_id.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    call_ids.sort_unstable();
    call_ids.dedup();
    call_ids
}

#[cfg(test)]
#[path = "compact_rencrow_tests.rs"]
mod tests;
