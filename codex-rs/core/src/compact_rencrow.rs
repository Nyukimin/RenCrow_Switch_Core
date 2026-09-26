// Modified by RenCrow Switch Core, 2026-09-22; V2 orchestration 2026-09-24.
//! V2 compaction: Normal with optional selection and one summary, deterministic Emergency
//! fallback, and explicit blocked states.
use super::*;
use codex_history::compaction_candidate::CandidateInput;
use codex_history::compaction_candidate::Origin;
use codex_history::compaction_candidate::history_digest;
use codex_history::compaction_checkpoint_metadata::CheckpointResponseStage;
use codex_history::compaction_checkpoint_metadata::CompactionModelResponseReceipt;
use codex_history::compaction_pipeline::InstructionSelection;
use codex_history::compaction_pipeline::ProposedOperation;
use codex_history::compaction_pipeline::ProposedPlan;
use codex_history::compaction_preprocess::InstructionObservationLink;
use codex_rollout::RolloutRecorder;
use serde_json::Value;
use serde_json::json;
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

pub(super) use summary::should_apply_server_reasoning_included;

#[cfg(test)]
#[path = "compact_rencrow_orchestration_tests.rs"]
mod orchestration_tests;

const INSTRUCTION_SELECTION_PROMPT: &str = "Select only explicit obsolete Human instructions from candidate:true Human sources. Candidate:false Human sources are context only; never remove them. Use completion_links only for the Human identified by instruction_id; a link is a candidate and does not prove success. Preserve failed, pending, ambiguous, protected, opaque, and continuing work. Do not infer completion from unrelated sources. For drop_superseded, correction_text is required and must be the exact unique correction passage that remains current; corrections must remain. source_text is optional only when the entire source is safe to remove; for a mixed instruction provide the exact unique source passage. Never remove protected portions or an entire partly protected source. For replace_completed, evidence must be the linked output_source_id; use it as factual grounding and do not claim more than the output and terminal status/exit establish. A successful command that inspects another job (such as ls or tail) does not prove that job succeeded. Keep unresolved conditions and uncertainty. Return only {\"operations\":[{\"action\":\"drop_superseded\",\"source\":\"<Human id>\",\"source_text\":\"<optional exact unique passage>\",\"correction\":\"<later Human id>\",\"correction_text\":\"<exact unique passage>\"},{\"action\":\"replace_completed\",\"source\":\"<Human id>\",\"source_text\":\"<optional exact unique passage>\",\"evidence\":\"<linked output_source_id>\",\"result\":\"<brief factual result supported by the output>\"}]}. Use only the fields shown for the chosen action. Omit source_text only when the whole source is safe to remove. Do not return a snapshot hash or extra keys. JSON only.";

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
            record.origin == Origin::Human
                && semantic_boundary.is_none_or(|boundary| index > boundary)
        })
}

pub(super) async fn select_obsolete_instructions(
    sess: &Session,
    ctx: &TurnContext,
    metadata: CompactionTurnMetadata,
    candidate_payload: Option<Value>,
    receipts: &mut Vec<CompactionModelResponseReceipt>,
    cancellation: &tokio_util::sync::CancellationToken,
) -> CodexResult<Option<InstructionSelection>> {
    let Some(payload) = candidate_payload else {
        return Ok(None);
    };
    let invalid =
        || CodexErr::InvalidRequest("RenCrow instruction selection payload is invalid".into());
    let snapshot_hash = payload
        .get("snapshot_hash")
        .and_then(Value::as_str)
        .filter(|hash| hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or_else(invalid)?
        .to_owned();
    let presentation_hash = payload
        .get("presentation_hash")
        .and_then(Value::as_str)
        .filter(|hash| hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or_else(invalid)?
        .to_owned();
    let sources = payload
        .get("sources")
        .and_then(Value::as_array)
        .ok_or_else(invalid)?;
    let completion_links = payload
        .get("completion_links")
        .and_then(Value::as_array)
        .ok_or_else(invalid)?;
    if !sources.iter().all(Value::is_object) || !completion_links.iter().all(Value::is_object) {
        return Err(invalid());
    }
    let has_candidate = sources.iter().any(|source| {
        source.get("origin").and_then(Value::as_str) == Some("human")
            && source.get("candidate").and_then(Value::as_bool) == Some(true)
    });
    if !has_candidate {
        return Ok(None);
    }

    let input = json!({"sources": sources, "completion_links": completion_links});
    let (response, seconds) = model_request::drain_compaction_stage(
        sess,
        ctx,
        metadata,
        "instruction_selection",
        model_request::json_stage_input(INSTRUCTION_SELECTION_PROMPT, &input),
        cancellation,
    )
    .await?;
    let text = model_request::json_stage_text(response.output)?;
    receipts.push(CompactionModelResponseReceipt {
        stage: CheckpointResponseStage::InstructionSelection,
        response_id: response.response_id,
        seconds,
        usage: response.token_usage,
    });
    let proposed: ProposedPlan = model_request::parse_json_stage("instruction_selection", &text)?;
    if proposed.operations.iter().any(|operation| {
        matches!(operation, ProposedOperation::DropSuperseded { correction_text, .. }
            if correction_text
                .as_deref()
                .is_none_or(|text| text.trim().is_empty()))
    }) {
        return Err(CodexErr::InvalidRequest(
            "RenCrow instruction selection requires exact correction_text".into(),
        ));
    }
    Ok(Some(InstructionSelection::from_host(
        snapshot_hash,
        presentation_hash,
        proposed,
    )))
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
    let (canonical, source_thread, active_call_ids) = load_canonical_rollout(sess)
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
        match stages::normal_candidate(sess, ctx, metadata, cancellation, snapshot, &prepared).await
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

#[cfg(test)]
#[path = "compact_rencrow_tests.rs"]
mod tests;
