// Modified by RenCrow Switch Core, 2026-09-22; Normal V2 orchestration 2026-09-24.
//! Normal V2 compaction: optional instruction selection, one summary request, host checks.
use super::*;
use codex_history::compaction_candidate::CandidateInput;
use codex_history::compaction_candidate::Origin;
use codex_history::compaction_candidate::digest;
use codex_history::compaction_candidate::history_digest;
use codex_history::compaction_checkpoint_metadata::CheckpointResponseStage;
use codex_history::compaction_checkpoint_metadata::CompactionModelResponseReceipt;
use codex_history::compaction_pipeline::InstructionSelection;
use codex_history::compaction_pipeline::ProposedOperation;
use codex_history::compaction_pipeline::ProposedPlan;
use codex_history::compaction_preprocess::InstructionObservationLink;
use codex_history::compaction_preprocess::collect_instruction_candidates;
use codex_history::compaction_preprocess::prune_known_obsolete;
use codex_history::compaction_selection::validate_and_apply_selection;
use codex_rollout::RolloutRecorder;
use serde_json::Value;
use serde_json::json;
use std::collections::HashSet;

#[path = "compact_rencrow_candidate.rs"]
mod candidate;
#[path = "compact_rencrow_history.rs"]
mod history;
#[path = "compact_rencrow_request.rs"]
mod model_request;
#[path = "compact_rencrow_native.rs"]
mod native;
#[path = "compact_rencrow_observation.rs"]
mod observation;
#[path = "compact_rencrow_summary.rs"]
mod summary;

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
const AUTO_RETRY_SUPPRESSED: &str = "RenCrow automatic compaction was not retried because the same unchanged history already failed compaction. The history was not changed; run /compact to retry manually.";

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

/// Run one Normal V2 compaction (Annex A F01) and commit it through the session owner.
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
    let turn_state = sess
        .input_queue
        .turn_state_for_sub_id(&sess.active_turn, &ctx.sub_id)
        .await;
    let (activity, pending) = sess
        .input_queue
        .subscribe_activity(turn_state.as_deref())
        .await;
    if pending.is_some() {
        return Err(CodexErr::InvalidRequest(
            "RenCrow compaction deferred: pending input".into(),
        ));
    }
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

    let result = compact_normal(
        &sess,
        &ctx,
        injection,
        metadata,
        &cancellation,
        &activity,
        &snapshot,
        history_hash.clone(),
        settings.clone(),
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
    sess.recompute_token_usage(&ctx).await;
    sess.emit_turn_item_completed(&ctx, item).await;
    sess.send_event(
        &ctx,
        EventMsg::Warning(WarningEvent {
            message: format!(
                "RenCrow compaction applied: {} instruction removals, {} completed results, {} new observations, {} model requests. Model {}.",
                outcome.removals, outcome.results, outcome.observations, outcome.requests, outcome.model
            ),
        }),
    )
    .await;
    Ok(outcome.summary)
}

/// Facts about one committed Normal compaction, reported after the durable commit.
struct NormalOutcome {
    summary: String,
    removals: usize,
    results: usize,
    observations: usize,
    requests: usize,
    model: String,
}

/// Prepare, select, preflight, summarize, validate, and commit one Normal V2 candidate.
///
/// At most two model requests are sent: an optional instruction selection and one summary. Every
/// failure leaves the live history unchanged; only `commit_rencrow_checkpoint` replaces it.
#[allow(clippy::too_many_arguments)]
async fn compact_normal(
    sess: &Session,
    ctx: &TurnContext,
    injection: InitialContextInjection,
    metadata: CompactionTurnMetadata,
    cancellation: &tokio_util::sync::CancellationToken,
    activity: &tokio::sync::watch::Receiver<crate::session::InputQueueActivity>,
    snapshot: &crate::context_manager::ContextManager,
    history_hash: String,
    settings: codex_protocol::protocol::ThreadSettingsSnapshot,
) -> CodexResult<NormalOutcome> {
    let invalid = CodexErr::InvalidRequest;
    let originals = snapshot.annotated_items();
    let thread_id = sess.thread_id().to_string();

    // Prepare.
    let adopted = summary::find_adopted_v2_checkpoint(originals, &thread_id).map_err(invalid)?;
    let (canonical, source_thread, active_call_ids) = load_canonical_rollout(sess).await?;
    let prepared = codex_rollout::prepare_compaction_sources(
        originals,
        &canonical,
        &source_thread,
        &active_call_ids,
    )
    .map_err(|error| invalid(format!("RenCrow compaction blocked: {error}")))?;
    let prompt_base = sess.get_prompt_base_instructions().await;
    let base = sess.get_base_instructions().await;
    let expected_world = snapshot.world_state_checkpoint();
    let (initial_context, world_state) = build_compaction_initial_context(sess, &injection).await;
    let binding = digest(&json!({"thread":thread_id,"turn":ctx.sub_id,"history":history_hash,"settings":settings,"base":base.text,"world":expected_world})).map_err(invalid)?;
    let input = history::capture(
        originals,
        binding,
        &thread_id,
        vec![json!({"base":prompt_base,"settings":settings,"injection":initial_context.iter().map(|i| &i.item).collect::<Vec<_>>()})],
        &history::verified_pair_projections(&prepared),
    )
    .map_err(invalid)?;
    let (known_refs, previous_observations, covered) = adopted
        .as_ref()
        .map(|adopted| {
            (
                adopted.metadata.applied_refs.as_slice(),
                adopted.metadata.observations.as_slice(),
                adopted.metadata.summary_covered_observations.as_slice(),
            )
        })
        .unwrap_or_default();
    let pruning = prune_known_obsolete(&input, known_refs).map_err(invalid)?;
    // Handled means presented to an accepted Normal summary, not merely stored (Part 2 §21).
    let projections = observation::project_unhandled_observations(originals, &prepared, covered)
        .map_err(invalid)?;
    let inventory = observation::cumulative_observation_coverage(
        previous_observations,
        &projections
            .iter()
            .map(|(_, _, projection)| projection.coverage.clone())
            .collect::<Vec<_>>(),
    )
    .map_err(invalid)?;
    let links = observation::build_completion_links(originals, &input, &pruning, &prepared)
        .map_err(invalid)?;

    // Select.
    let mut receipts = Vec::new();
    let (selection, presentation_hash) = if selection_required(
        &input,
        adopted.as_ref().map(|adopted| adopted.index),
        &links,
    ) {
        let payload = collect_instruction_candidates(&input, &pruning, &links).map_err(invalid)?;
        let selection =
            select_obsolete_instructions(sess, ctx, metadata, payload, &mut receipts, cancellation)
                .await?;
        let presentation_hash = selection
            .as_ref()
            .map(|selection| selection.presentation_hash().to_owned());
        (selection, presentation_hash)
    } else {
        (None, None)
    };
    let application =
        validate_and_apply_selection(&input, &pruning, &links, selection).map_err(invalid)?;
    let native = native::filter_retained_instructions(originals, &input, application.clone())
        .map_err(invalid)?;

    // Preflight.
    let scope = ctx.config.model_auto_compact_token_limit_scope;
    let limits = crate::session::context_window::context_window_token_status(sess, ctx).await;
    candidate::preflight_compaction_floor(
        snapshot,
        &base,
        &native,
        &initial_context,
        scope,
        &limits,
    )
    .map_err(|error| invalid(format!("RenCrow compaction preflight failed: {error}")))?;

    // Summarize.
    let excerpts = projections
        .iter()
        .map(|(call_index, output_index, projection)| {
            (*call_index, *output_index, projection.summary.clone())
        })
        .collect::<Vec<_>>();
    let summary_history = summary::build_summary_history(
        originals,
        &input,
        &native,
        &excerpts,
        summary::previous_summary_index(originals, adopted.as_ref()),
    )
    .map_err(invalid)?;
    let (summary_suffix, response_id) = model_request::request_compaction_summary(
        sess,
        ctx,
        metadata,
        summary_history,
        &application.results,
        &mut receipts,
        cancellation,
    )
    .await?;
    let summary_text = format!("{SUMMARY_PREFIX}\n{summary_suffix}");
    let inventory_refs = inventory
        .iter()
        .map(|coverage| coverage.reference.clone())
        .collect::<Vec<_>>();
    let important = summary::important_refs_from_summary(&summary_suffix, &inventory_refs);

    // Validate and commit.
    let requests = receipts.len();
    let model = ctx.model_info().slug.clone();
    let effort = sess
        .reasoning_effort_for_request(&ctx.initial_settings, RequestEffortUsage::Compaction)
        .await;
    let checkpoint_metadata = candidate::fresh_checkpoint_metadata(
        &summary_text,
        input.snapshot().map_err(invalid)?.hash().to_owned(),
        presentation_hash,
        &application,
        inventory,
        observation::summary_covered_after_normal(covered, &projections),
        important.refs,
        receipts,
        model.clone(),
        effort,
    );
    let mut replacement =
        native::build_native_replacement(&native, &summary_text, initial_context.clone())
            .map_err(invalid)?;
    let summary_item = replacement
        .last_mut()
        .ok_or_else(|| invalid("compaction replacement has no summary".into()))?;
    summary_item.set_turn_id_if_missing(&ctx.sub_id);
    summary_item
        .metadata
        .get_or_insert_default()
        .rencrow_compaction = Some(
        serde_json::to_value(&checkpoint_metadata)
            .map_err(|error| invalid(format!("failed to encode compaction metadata: {error}")))?,
    );
    candidate::validate_compaction_candidate(
        snapshot,
        &base,
        &native,
        &initial_context,
        &replacement,
        &thread_id,
        scope,
        &limits,
    )
    .map_err(|error| invalid(format!("RenCrow compaction candidate rejected: {error}")))?;
    let reference_context = match injection {
        InitialContextInjection::DoNotInject => None,
        InitialContextInjection::BeforeLastUserMessage { step_context, .. } => {
            Some(step_context.to_turn_context_item())
        }
    };
    sess.commit_rencrow_checkpoint(
        crate::session::RenCrowCheckpoint {
            items: replacement,
            expected_history_hash: history_hash,
            expected_settings: settings,
            reference_context,
            world_state,
            summary: summary_text,
            response_id: Some(response_id),
            expected_turn: ctx.sub_id.clone(),
            expected_base_text: base.text,
            expected_world,
        },
        activity,
    )
    .await?;
    Ok(NormalOutcome {
        summary: summary_suffix,
        removals: application
            .pruning
            .applied
            .len()
            .saturating_sub(pruning.applied.len()),
        results: application.results.len(),
        observations: projections.len(),
        requests,
        model,
    })
}

/// Load the canonical rollout once for this compaction (Annex A F04).
///
/// Without persisted rollout storage no pair can be verified, so every tool item stays protected;
/// existing archive references then fail closed in `prepare_compaction_sources`.
async fn load_canonical_rollout(
    sess: &Session,
) -> CodexResult<(
    Vec<codex_history::RolloutItem>,
    codex_protocol::ThreadId,
    HashSet<String>,
)> {
    let active_call_ids = sess
        .list_background_terminals()
        .await
        .into_iter()
        .map(|terminal| terminal.item_id)
        .collect();
    let Some(path) = sess.current_rollout_path().await.map_err(|error| {
        CodexErr::InvalidRequest(format!(
            "RenCrow compaction blocked: rollout unavailable: {error}"
        ))
    })?
    else {
        return Ok((Vec::new(), sess.thread_id(), active_call_ids));
    };
    sess.flush_rollout().await.map_err(|error| {
        CodexErr::InvalidRequest(format!(
            "RenCrow compaction blocked: rollout flush failed: {error}"
        ))
    })?;
    let (items, thread_id, parse_errors) = RolloutRecorder::load_rollout_items(&path)
        .await
        .map_err(|error| {
            CodexErr::InvalidRequest(format!(
                "RenCrow compaction blocked: rollout read failed: {error}"
            ))
        })?;
    if parse_errors != 0 {
        return Err(CodexErr::InvalidRequest(format!(
            "RenCrow compaction blocked: rollout contains {parse_errors} parse errors"
        )));
    }
    let thread_id = thread_id.ok_or_else(|| {
        CodexErr::InvalidRequest(
            "RenCrow compaction blocked: rollout has no canonical thread header".into(),
        )
    })?;
    if thread_id != sess.thread_id() {
        return Err(CodexErr::InvalidRequest(
            "RenCrow compaction blocked: rollout thread does not match live session".into(),
        ));
    }
    Ok((items, thread_id, active_call_ids))
}

#[cfg(test)]
#[path = "compact_rencrow_tests.rs"]
mod tests;
