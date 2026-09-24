//! Prepare, Normal, and Emergency stages of V2 compaction (Annex A F01, Part 2 §3–7, §24–55).

use super::super::*;
use super::candidate;
use super::emergency;
use super::emergency::EmergencyPlan;
use super::history;
use super::model_request;
use super::native;
use super::observation;
use super::select_obsolete_instructions;
use super::selection_required;
use super::summary;
use super::summary::AdoptedV2Checkpoint;
use super::summary::SemanticContext;
use crate::context_manager::ContextManager;
use crate::session::context_window::ContextWindowTokenStatus;
use codex_history::ObservationReference;
use codex_history::RenCrowCompactionMetadataV2;
use codex_history::RolloutItem;
use codex_history::compaction_candidate::CandidateInput;
use codex_history::compaction_candidate::digest;
use codex_history::compaction_preprocess::InstructionObservationLink;
use codex_history::compaction_preprocess::InstructionPruning;
use codex_history::compaction_preprocess::collect_instruction_candidates;
use codex_history::compaction_preprocess::prune_known_obsolete;
use codex_history::compaction_selection::InstructionSelectionApplication;
use codex_history::compaction_selection::validate_and_apply_selection;
use codex_history::observation_projection::ObservationCoverage;
use codex_history::observation_projection::ObservationProjection;
use codex_protocol::ThreadId;
use codex_protocol::config_types::AutoCompactTokenLimitScope;
use codex_protocol::models::BaseInstructions;
use codex_protocol::protocol::ThreadSettingsSnapshot;
use codex_rollout::PreparedCompactionSources;
use serde_json::json;
use std::collections::HashSet;
use tokio_util::sync::CancellationToken;

/// Why a stage produced no committable candidate (Part 2 §3–7, §24).
pub(super) enum StageFailure {
    /// Model, transport, response schema, or semantic rejection: continue with Emergency.
    Semantic(CodexErr),
    /// The minimum retained context does not fit: CapacityBlocked.
    Capacity(String),
    /// A deterministic stored-data or identity conflict: IntegrityBlocked.
    Integrity(String),
    /// Cancellation, a stale snapshot, persistence, or I/O: returned unchanged.
    Abort(CodexErr),
}

impl StageFailure {
    /// Classify a model request failure: cancellation aborts; anything else routes to Emergency.
    fn model(error: CodexErr) -> Self {
        if matches!(
            error.details(),
            CodexErrorDetails::Interrupted | CodexErrorDetails::TurnAborted
        ) {
            Self::Abort(error)
        } else {
            Self::Semantic(error)
        }
    }

    /// The user-visible error for a failure that ends compaction (Part 2 §66).
    pub(super) fn into_error(self) -> CodexErr {
        match self {
            Self::Semantic(error) | Self::Abort(error) => error,
            Self::Capacity(reason) => CodexErr::InvalidRequest(format!(
                "RenCrow compaction is capacity-blocked: the authoritative retained context exceeds the currently available context capacity. No source data was discarded. ({reason})"
            )),
            Self::Integrity(reason) => CodexErr::InvalidRequest(format!(
                "RenCrow detected a deterministic integrity conflict in stored compaction data. The current state was not replaced. ({reason})"
            )),
        }
    }
}

/// Deterministic preparation shared by Normal and Emergency.
pub(super) struct PreparedV2<'a> {
    originals: &'a [ResponseItemEnvelope],
    thread_id: String,
    adopted: Option<AdoptedV2Checkpoint>,
    semantic: SemanticContext,
    sources: PreparedCompactionSources<'a>,
    input: CandidateInput,
    /// Previously accepted removals that still apply; no new semantic decision.
    pruning: InstructionPruning,
    /// Observations not yet presented to an accepted Normal summary.
    covered: Vec<ObservationReference>,
    projections: Vec<(usize, usize, ObservationProjection)>,
    inventory: Vec<ObservationCoverage>,
    links: Vec<InstructionObservationLink>,
    base: BaseInstructions,
    initial_context: Vec<ResponseItemEnvelope>,
    scope: AutoCompactTokenLimitScope,
    limits: ContextWindowTokenStatus,
}

/// A validated candidate ready for the session-owned commit.
pub(super) struct StageCandidate {
    pub(super) items: Vec<ResponseItemEnvelope>,
    pub(super) summary_text: String,
    pub(super) response_id: Option<String>,
    pub(super) removals: usize,
    pub(super) results: usize,
    pub(super) observations: usize,
    pub(super) requests: usize,
    pub(super) model: Option<String>,
}

/// Prepare the deterministic state shared by Normal and Emergency (Annex A F01 Prepare).
///
/// Every failure here happens before any model output, so it is an integrity conflict.
#[allow(clippy::too_many_arguments)]
pub(super) async fn prepare_v2<'a>(
    sess: &Session,
    ctx: &TurnContext,
    snapshot: &'a ContextManager,
    canonical: &'a [RolloutItem],
    source_thread: &ThreadId,
    active_call_ids: &HashSet<String>,
    history_hash: &str,
    settings: &ThreadSettingsSnapshot,
    initial_context: Vec<ResponseItemEnvelope>,
) -> Result<PreparedV2<'a>, StageFailure> {
    let integrity = StageFailure::Integrity;
    let originals = snapshot.annotated_items();
    let thread_id = sess.thread_id().to_string();
    let adopted = summary::find_adopted_v2_checkpoint(originals, &thread_id).map_err(integrity)?;
    let sources = codex_rollout::prepare_compaction_sources(
        originals,
        canonical,
        source_thread,
        active_call_ids,
    )
    .map_err(integrity)?;
    let prompt_base = sess.get_prompt_base_instructions().await;
    let base = sess.get_base_instructions().await;
    let binding = digest(&json!({"thread":thread_id,"turn":ctx.sub_id,"history":history_hash,"settings":settings,"base":base.text,"world":snapshot.world_state_checkpoint()})).map_err(integrity)?;
    let input = history::capture(
        originals,
        binding,
        &thread_id,
        vec![json!({"base":prompt_base,"settings":settings,"injection":initial_context.iter().map(|i| &i.item).collect::<Vec<_>>()})],
        &history::verified_pair_projections(&sources),
    )
    .map_err(integrity)?;
    let previous = adopted.as_ref().map(|adopted| &adopted.metadata);
    let pruning = prune_known_obsolete(
        &input,
        previous.map_or(&[][..], |previous| previous.applied_refs.as_slice()),
    )
    .map_err(integrity)?;
    // Handled means presented to an accepted Normal summary, not merely stored (Part 2 §21).
    let covered = previous
        .map(|previous| previous.summary_covered_observations.clone())
        .unwrap_or_default();
    let projections = observation::project_unhandled_observations(originals, &sources, &covered)
        .map_err(integrity)?;
    let inventory = observation::cumulative_observation_coverage(
        previous.map_or(&[][..], |previous| previous.observations.as_slice()),
        &projections
            .iter()
            .map(|(_, _, projection)| projection.coverage.clone())
            .collect::<Vec<_>>(),
    )
    .map_err(integrity)?;
    let links = observation::build_completion_links(originals, &input, &pruning, &sources)
        .map_err(integrity)?;
    let semantic = summary::semantic_context(originals, adopted.as_ref());
    Ok(PreparedV2 {
        originals,
        thread_id,
        adopted,
        semantic,
        sources,
        input,
        pruning,
        covered,
        projections,
        inventory,
        links,
        base,
        initial_context,
        scope: ctx.config.model_auto_compact_token_limit_scope,
        limits: crate::session::context_window::context_window_token_status(sess, ctx).await,
    })
}

/// Build and validate a Normal candidate with at most two model requests (Part 2 §8–18).
pub(super) async fn normal_candidate(
    sess: &Session,
    ctx: &TurnContext,
    metadata: CompactionTurnMetadata,
    cancellation: &CancellationToken,
    snapshot: &ContextManager,
    prepared: &PreparedV2<'_>,
) -> Result<StageCandidate, StageFailure> {
    let p = prepared;
    let mut receipts = Vec::new();
    let (selection, presentation_hash) =
        if selection_required(&p.input, p.semantic.selection_boundary, &p.links) {
            let payload = collect_instruction_candidates(&p.input, &p.pruning, &p.links)
                .map_err(StageFailure::Integrity)?;
            let selection = select_obsolete_instructions(
                sess,
                ctx,
                metadata,
                payload,
                &mut receipts,
                cancellation,
            )
            .await
            .map_err(StageFailure::model)?;
            let presentation_hash = selection
                .as_ref()
                .map(|selection| selection.presentation_hash().to_owned());
            (selection, presentation_hash)
        } else {
            (None, None)
        };
    // Emergency never uses a model selection, so a conflict it caused routes to Emergency.
    let selected = selection.is_some();
    let after_selection = move |error: String| {
        if selected {
            StageFailure::Semantic(CodexErr::InvalidRequest(format!(
                "RenCrow instruction selection rejected: {error}"
            )))
        } else {
            StageFailure::Integrity(error)
        }
    };
    let application = validate_and_apply_selection(&p.input, &p.pruning, &p.links, selection)
        .map_err(after_selection)?;
    let native = native::filter_retained_instructions(p.originals, &p.input, application.clone())
        .map_err(after_selection)?;
    // Emergency retains more unsummarized Work than this floor, so it cannot fit either (§15).
    candidate::preflight_compaction_floor(
        snapshot,
        &p.base,
        &native,
        &p.initial_context,
        p.scope,
        &p.limits,
    )
    .map_err(StageFailure::Capacity)?;
    let excerpts = p
        .projections
        .iter()
        .map(|(call_index, output_index, projection)| {
            (*call_index, *output_index, projection.summary.clone())
        })
        .collect::<Vec<_>>();
    let summary_history = summary::build_summary_history(
        p.originals,
        &p.input,
        &native,
        &excerpts,
        p.semantic.previous_summary,
        p.semantic.work_boundary,
    )
    .map_err(after_selection)?;
    let (summary_suffix, response_id) = model_request::request_compaction_summary(
        sess,
        ctx,
        metadata,
        summary_history,
        &application.results,
        &mut receipts,
        cancellation,
    )
    .await
    .map_err(StageFailure::model)?;

    // Everything below depends on the model summary; a rejection routes to Emergency.
    let rejected = |error: String| {
        StageFailure::Semantic(CodexErr::InvalidRequest(format!(
            "RenCrow Normal candidate rejected: {error}"
        )))
    };
    let summary_text = format!("{SUMMARY_PREFIX}\n{summary_suffix}");
    let inventory_refs = p
        .inventory
        .iter()
        .map(|coverage| coverage.reference.clone())
        .collect::<Vec<_>>();
    let important = summary::important_refs_from_summary(&summary_suffix, &inventory_refs);
    let requests = receipts.len();
    let model = ctx.model_info().slug.clone();
    let effort = sess
        .reasoning_effort_for_request(&ctx.initial_settings, RequestEffortUsage::Compaction)
        .await;
    let checkpoint_metadata = candidate::fresh_checkpoint_metadata(
        &summary_text,
        p.input
            .snapshot()
            .map_err(StageFailure::Integrity)?
            .hash()
            .to_owned(),
        presentation_hash,
        &application,
        p.inventory.clone(),
        observation::summary_covered_after_normal(&p.covered, &p.projections),
        important.refs,
        receipts,
        model.clone(),
        effort,
    );
    let mut items =
        native::build_native_replacement(&native, &summary_text, p.initial_context.clone())
            .map_err(rejected)?;
    attach_summary_metadata(&mut items, &ctx.sub_id, &checkpoint_metadata).map_err(rejected)?;
    candidate::validate_compaction_candidate(
        snapshot,
        &p.base,
        &native,
        &p.initial_context,
        &items,
        &p.thread_id,
        p.scope,
        &p.limits,
    )
    .map_err(rejected)?;
    Ok(StageCandidate {
        items,
        summary_text,
        response_id: Some(response_id),
        removals: application
            .pruning
            .applied
            .len()
            .saturating_sub(p.pruning.applied.len()),
        results: application.results.len(),
        observations: p.projections.len(),
        requests,
        model: Some(model),
    })
}

/// Build and validate a deterministic Emergency candidate without any model request (§25–55).
///
/// A structural or metadata conflict is an integrity failure; a candidate that does not shrink
/// or fit the configured limits is CapacityBlocked.
pub(super) fn emergency_candidate(
    ctx: &TurnContext,
    snapshot: &ContextManager,
    prepared: &PreparedV2<'_>,
) -> Result<StageCandidate, StageFailure> {
    let p = prepared;
    let integrity = StageFailure::Integrity;
    let native = native::filter_retained_instructions(
        p.originals,
        &p.input,
        InstructionSelectionApplication {
            pruning: p.pruning.clone(),
            plan_hash: None,
            results: Vec::new(),
        },
    )
    .map_err(integrity)?;
    let (summary_text, semantic) =
        emergency::emergency_summary_text(p.originals, p.semantic.previous_summary)
            .map_err(integrity)?;
    let plan = EmergencyPlan {
        originals: p.originals,
        input: &p.input,
        native: &native,
        unhandled_pairs: p
            .projections
            .iter()
            .map(|(call_index, output_index, _)| (*call_index, *output_index))
            .collect(),
        markers: emergency::select_observation_markers(p.originals, &p.sources, &p.projections),
        work_boundary: p.semantic.work_boundary,
        summary_text,
    };
    let metadata = emergency::emergency_checkpoint_metadata(
        &plan.summary_text,
        semantic,
        p.input.snapshot().map_err(integrity)?.hash().to_owned(),
        &p.pruning,
        p.adopted.as_ref().map(|adopted| &adopted.metadata),
        &plan.markers,
    )
    .map_err(integrity)?;
    let mut items = plan.build(&p.initial_context).map_err(integrity)?;
    attach_summary_metadata(&mut items, &ctx.sub_id, &metadata).map_err(integrity)?;
    plan.validate(&p.initial_context, &items)
        .map_err(integrity)?;
    let summary = items
        .last()
        .ok_or_else(|| StageFailure::Integrity("emergency candidate is empty".into()))?;
    emergency::validate_emergency_summary(summary, &plan.summary_text, &p.thread_id, &metadata)
        .map_err(integrity)?;
    candidate::check_context_budget(snapshot, &p.base, items.clone(), p.scope, &p.limits)
        .map_err(StageFailure::Capacity)?;
    Ok(StageCandidate {
        items,
        summary_text: plan.summary_text,
        response_id: None,
        removals: 0,
        results: 0,
        observations: plan.markers.len(),
        requests: 0,
        model: None,
    })
}

/// Bind the checkpoint metadata and turn to the typed summary at the end of a candidate.
fn attach_summary_metadata(
    items: &mut [ResponseItemEnvelope],
    turn_id: &str,
    metadata: &RenCrowCompactionMetadataV2,
) -> Result<(), String> {
    let summary = items
        .last_mut()
        .ok_or_else(|| "compaction replacement has no summary".to_owned())?;
    summary.set_turn_id_if_missing(turn_id);
    summary.metadata.get_or_insert_default().rencrow_compaction = Some(
        serde_json::to_value(metadata)
            .map_err(|error| format!("failed to encode compaction metadata: {error}"))?,
    );
    Ok(())
}
