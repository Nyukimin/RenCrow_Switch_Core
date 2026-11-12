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
use codex_history::compaction_candidate::Origin;
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

/// Extend the ordinary turn prompt for the summary when it still leaves room for the response.
fn turn_prefix<'a>(
    snapshot: &ContextManager,
    p: &PreparedV2<'_>,
    step: &'a crate::session::step_context::StepContext,
) -> Option<model_request::TurnPrefix<'a>> {
    let mut citations = Vec::new();
    let mut hidden_observations = Vec::new();
    for (_, output_index, projection) in &p.projections {
        let excerpt = &projection.summary;
        let call = excerpt
            .call
            .excerpts
            .first()
            .map(|text| text.split_whitespace().collect::<Vec<_>>().join(" "))
            .unwrap_or_default();
        let call = call.chars().take(CITATION_CALL_CHARS).collect::<String>();
        citations.push(format!(
            "- {}: {} `{call}`",
            excerpt.reference.call_id, excerpt.tool_name
        ));
        let shown_as_marker = p
            .originals
            .get(*output_index)
            .and_then(|output| output.metadata.as_ref())
            .is_some_and(|metadata| {
                metadata.rencrow_observation_projection.is_some()
                    || metadata.rencrow_archive_reference.is_some()
            });
        if shown_as_marker {
            let body = serde_json::to_string(excerpt).ok()?;
            hidden_observations.push(format!("{{\"observation\":{body}}}"));
        }
    }
    // The instruction and citations are far smaller than the history; count bytes as tokens.
    let added = citations
        .iter()
        .chain(&hidden_observations)
        .map(String::len)
        .sum::<usize>()
        + 2_048;
    let needed = p
        .limits
        .active_context_tokens
        .saturating_add(i64::try_from(added).ok()?)
        .saturating_add(SUMMARY_OUTPUT_RESERVE_TOKENS);
    if p.limits
        .full_context_window_limit
        .is_some_and(|limit| needed > limit)
    {
        return None;
    }
    Some(model_request::TurnPrefix {
        step_context: step,
        input: snapshot
            .clone()
            .for_prompt(&step.settings.model_info.input_modalities),
        citations,
        hidden_observations,
    })
}

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
            Self::Integrity(reason) => CodexErr::InvalidRequest(
                codex_history::compaction_transaction::integrity_blocked_message(&reason),
            ),
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
    protected_task_boundary_call_ids: HashSet<String>,
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

pub(super) fn partition_task_boundary_pairs(
    sources: &mut PreparedCompactionSources<'_>,
    protected_call_ids: &HashSet<String>,
) {
    let mut ordinary_pairs = Vec::with_capacity(sources.pairs.len());
    for pair in sources.pairs.drain(..) {
        if protected_call_ids.contains(&pair.reference.call_id) {
            sources
                .protected_indices
                .extend([pair.call_index, pair.output_index]);
        } else {
            ordinary_pairs.push(pair);
        }
    }
    sources.pairs = ordinary_pairs;
    sources.protected_indices.sort_unstable();
    sources.protected_indices.dedup();
}

pub(super) async fn protected_task_boundary_call_ids(sess: &Session) -> HashSet<String> {
    let mut call_ids = HashSet::new();
    if let Some(scope) = sess.active_task_scope().await {
        call_ids.insert(scope.begin_call_id.clone());
        if let Some(attempt) = sess
            .latest_task_check_for_scope(&scope.task_id, scope.task_revision)
            .await
        {
            call_ids.insert(attempt.call_id);
        }
    }
    call_ids
}

const WORKING_TAIL_PAIR_MAX_BYTES: usize = 12 * 1_024;

pub(super) fn protected_working_tail_call_ids(
    originals: &[ResponseItemEnvelope],
    input: &CandidateInput,
    sources: &PreparedCompactionSources<'_>,
    active_root_task_id: Option<&str>,
) -> HashSet<String> {
    if input.records.len() != originals.len() {
        return HashSet::new();
    }
    let mut protected = HashSet::new();
    let pair_fits = |pair: &codex_rollout::PreparedCompactionSourcePair<'_>| {
        pair.canonical_call_input.is_some()
            && pair.canonical_output_text.is_some()
            && pair
                .canonical_call_input
                .map_or(usize::MAX, str::len)
                .checked_add(pair.output_total_bytes)
                .is_some_and(|size| size <= WORKING_TAIL_PAIR_MAX_BYTES)
    };

    // Preserve only the latest current-root status/update pair. The task ID binds this tail
    // protection; it does not make the output authoritative or change its acceptance rules.
    if let Some(root_task_id) = active_root_task_id
        && let Some(pair) = sources.pairs.iter().rev().find(|pair| {
            pair.tool_name == "task_context_boundary"
                && pair.reference_kind == codex_rollout::PreparedCompactionReferenceKind::Fresh
                && pair
                    .canonical_call_input
                    .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
                    .is_some_and(|args| {
                        matches!(args["action"].as_str(), Some("status" | "update"))
                            && args["root_task_id"].as_str() == Some(root_task_id)
                    })
        })
        .filter(|pair| pair_fits(pair))
    {
        protected.insert(pair.reference.call_id.clone());
    }

    // The latest direct `cat` after the latest declared user input is the only generic read
    // retained. Select it before checking the byte bound so an oversized newest read does not
    // silently fall back to a stale smaller result.
    let latest_declared_input = input
        .records
        .iter()
        .rposition(|record| {
            record.origin == Origin::Human || record.origin == Origin::Automation
        });
    if let Some(boundary) = latest_declared_input {
        let latest_read = sources
            .pairs
            .iter()
            .filter(|pair| {
                pair.call_index > boundary
                    && pair.tool_name == "exec_command"
                    && pair.reference_kind == codex_rollout::PreparedCompactionReferenceKind::Fresh
                    && pair.canonical_output_text.is_some()
                    && pair
                        .canonical_call_input
                        .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
                        .and_then(|args| args["cmd"].as_str().map(str::to_owned))
                        .is_some_and(|command| is_direct_cat_read(&command))
            })
            .max_by_key(|pair| pair.call_index);
        if let Some(pair) = latest_read.filter(|pair| pair_fits(pair)) {
            protected.insert(pair.reference.call_id.clone());
        }
    }
    protected
}

fn is_direct_cat_read(command: &str) -> bool {
    let Some(path) = command.trim().strip_prefix("cat ") else {
        return false;
    };
    !path.trim().is_empty()
        && !command
            .chars()
            .any(|ch| matches!(ch, ';' | '|' | '&' | '>' | '<' | '`' | '$' | '\\' | '\n' | '\r'))
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
    let mut sources = codex_rollout::prepare_compaction_sources(
        originals,
        canonical,
        source_thread,
        active_call_ids,
    )
    .map_err(integrity)?;
    let mut protected_task_boundary_call_ids = protected_task_boundary_call_ids(sess).await;
    // Canonical source validation above must see every pair. After it succeeds, keep active task
    // boundary evidence out of ordinary observation capture and summary projection: the native
    // projection retains these exact call/result items independently.
    partition_task_boundary_pairs(&mut sources, &protected_task_boundary_call_ids);
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
    let active_root_task_id = sess
        .active_task_scope()
        .await
        .map(|scope| scope.root_task_id);
    let working_tail_call_ids = protected_working_tail_call_ids(
        originals,
        &input,
        &sources,
        active_root_task_id.as_deref(),
    );
    partition_task_boundary_pairs(&mut sources, &working_tail_call_ids);
    protected_task_boundary_call_ids.extend(working_tail_call_ids);
    let previous = adopted.as_ref().and_then(|adopted| {
        adopted
            .metadata
            .as_ref()
            .or(adopted.inherited_capacity_metadata.as_ref())
    });
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
    if adopted.as_ref().is_some_and(|adopted| {
        adopted.task_boundary
            && adopted
                .inherited_capacity_metadata
                .as_ref()
                .and_then(|metadata| metadata.semantic_summary_hash.as_ref())
                .is_some()
            && semantic.previous_summary.is_none()
    }) {
        return Err(StageFailure::Integrity(
            "task boundary cannot locate its accepted semantic summary by hash".into(),
        ));
    }
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
        protected_task_boundary_call_ids,
        links,
        base,
        initial_context,
        scope: ctx.config.model_auto_compact_token_limit_scope,
        limits: crate::session::context_window::context_window_token_status(sess, ctx).await,
    })
}

#[cfg(test)]
pub(super) fn build_emergency_history_for_test(
    prepared: &PreparedV2<'_>,
) -> Result<Vec<ResponseItemEnvelope>, String> {
    let native = native::filter_retained_instructions_with_task_context(
        prepared.originals,
        &prepared.input,
        InstructionSelectionApplication {
            pruning: prepared.pruning.clone(),
            plan_hash: None,
            results: Vec::new(),
        },
        prepared.semantic.task_context,
        &prepared.protected_task_boundary_call_ids,
    )?;
    let markers = emergency::select_observation_markers(
        prepared.originals,
        &prepared.sources,
        &prepared.projections,
        prepared.adopted.as_ref().map(|adopted| adopted.index),
    )?;
    let (summary_text, _) =
        emergency::emergency_summary_text(prepared.originals, prepared.semantic.previous_summary)?;
    let plan = emergency::EmergencyPlan {
        originals: prepared.originals,
        input: &prepared.input,
        native: &native,
        unhandled_pairs: prepared
            .projections
            .iter()
            .map(|(call, output, _)| (*call, *output))
            .collect(),
        markers: markers
            .iter()
            .map(|(index, projection)| (*index, projection))
            .collect(),
        work_boundary: prepared.semantic.work_boundary,
        summary_text,
    };
    let items = plan.build(&[])?;
    plan.validate(&[], &items)?;
    Ok(items)
}

#[cfg(test)]
pub(super) fn validate_normal_history_for_test(
    prepared: &PreparedV2<'_>,
) -> Result<Vec<ResponseItemEnvelope>, String> {
    let application =
        validate_and_apply_selection(&prepared.input, &prepared.pruning, &prepared.links, None)?;
    let native = native::filter_retained_instructions_with_task_context(
        prepared.originals,
        &prepared.input,
        application,
        prepared.semantic.task_context,
        &prepared.protected_task_boundary_call_ids,
    )?;
    let excerpts = prepared
        .projections
        .iter()
        .map(|(call, output, projection)| (*call, *output, projection.summary.clone()))
        .collect::<Vec<_>>();
    summary::build_summary_history_with_task_context(
        prepared.originals,
        &prepared.input,
        &native,
        &excerpts,
        prepared.semantic.previous_summary,
        prepared.semantic.work_boundary,
        prepared.semantic.task_context,
    )
}

/// Output the Gateway reserves for a summary response; the extended prompt must leave room for it.
const SUMMARY_OUTPUT_RESERVE_TOKENS: i64 = 16_384;
/// Characters of each tool call quoted in a summary citation line.
const CITATION_CALL_CHARS: usize = 100;

/// Build and validate a Normal candidate with at most two model requests (Part 2 §8–18).
///
/// `step_context` is the step of the ordinary request before a mid-turn compaction. When this
/// compaction removes nothing, the summary request extends that prompt (Part 3 proposal A).
pub(super) async fn normal_candidate(
    sess: &Session,
    ctx: &TurnContext,
    metadata: CompactionTurnMetadata,
    cancellation: &CancellationToken,
    snapshot: &ContextManager,
    prepared: &PreparedV2<'_>,
    step_context: Option<&crate::session::step_context::StepContext>,
) -> Result<StageCandidate, StageFailure> {
    let p = prepared;
    let mut receipts = Vec::new();
    // Selection is an optional optimization. Reject bad proposals, retain their instructions,
    // and still summarize ordinary Work. Cancellation and deterministic integrity failures abort.
    let mut application = validate_and_apply_selection(&p.input, &p.pruning, &p.links, None)
        .map_err(StageFailure::Integrity)?;
    let mut conservative = false;
    let mut presentation_hash = None;
    if selection_required(&p.input, p.semantic.selection_boundary, &p.links)
        && let Some(request) = super::selection_request(
            &p.input,
            collect_instruction_candidates(&p.input, &p.pruning, &p.links)
                .map_err(StageFailure::Integrity)?,
        )
        .map_err(StageFailure::Integrity)?
    {
        presentation_hash = Some(request.presentation_hash().to_owned());
        match select_obsolete_instructions(
            sess,
            ctx,
            metadata,
            &request,
            &mut receipts,
            cancellation,
        )
        .await
        {
            Ok(text) => match request.apply(&p.input, &p.pruning, &p.links, &text) {
                Ok(report) => {
                    conservative = report.rejected > 0;
                    application = report.application;
                    if report.normalized_fence || conservative {
                        sess.send_event(ctx, EventMsg::Warning(WarningEvent { message: format!(
                            "RenCrow selection: normalized_fence={}, rejected_operations={}; rejected instructions remain retained. Work summary continues.", report.normalized_fence, report.rejected
                        ) })).await;
                    }
                }
                Err(error) => {
                    conservative = true;
                    tracing::warn!(%error, "RenCrow selection rejected; retaining instructions and continuing Work summary");
                }
            },
            Err(error) => match StageFailure::model(error) {
                StageFailure::Semantic(error) => {
                    conservative = true;
                    tracing::warn!(%error, "RenCrow selection unavailable; retaining instructions and continuing Work summary");
                }
                other => return Err(other),
            },
        }
    }
    if conservative {
        sess.send_event(ctx, EventMsg::Warning(WarningEvent { message:
            "RenCrow instruction selection was incomplete or rejected. Unaccepted instructions remain intact; only Work is summarized and selection will be reconsidered.".into()
        })).await;
    }
    let after_selection = StageFailure::Integrity;
    let native = native::filter_retained_instructions_with_task_context(
        p.originals,
        &p.input,
        application.clone(),
        p.semantic.task_context,
        &p.protected_task_boundary_call_ids,
    )
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
    let summary_history = summary::build_summary_history_with_task_context(
        p.originals,
        &p.input,
        &native,
        &excerpts,
        p.semantic.previous_summary,
        p.semantic.work_boundary,
        p.semantic.task_context,
    )
    .map_err(after_selection)?;
    // The extended prompt shows removed passages verbatim, so it is used only when none were
    // removed or replaced by this compaction.
    let turn_prefix = step_context
        .filter(|_| {
            application.pruning.applied.len() == p.pruning.applied.len()
                && application.results.is_empty()
        })
        .and_then(|step| turn_prefix(snapshot, p, step));
    let (summary_suffix, response_id) = model_request::request_compaction_summary(
        sess,
        ctx,
        metadata,
        summary_history,
        &application.results,
        turn_prefix,
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
    let mut checkpoint_metadata = candidate::fresh_checkpoint_metadata(
        &summary_text,
        p.input
            .snapshot()
            .map_err(StageFailure::Integrity)?
            .hash()
            .to_owned(),
        presentation_hash.clone(),
        &application,
        p.inventory.clone(),
        observation::summary_covered_after_normal(&p.covered, &p.projections),
        important.refs,
        receipts,
        model.clone(),
        effort,
    );
    if conservative {
        checkpoint_metadata.selection_mode =
            codex_history::CompactionSelectionMode::ConservativeSelection;
        // A completed but rejected response is an attempt receipt, never an accepted decision.
        checkpoint_metadata.presentation_hash = presentation_hash.clone();
    }
    let mut items =
        native::build_native_replacement(&native, &summary_text, p.initial_context.clone())
            .map_err(rejected)?;
    attach_summary_metadata(
        &mut items,
        &ctx.sub_id,
        &summary_text,
        &checkpoint_metadata,
        p.adopted
            .as_ref()
            .and_then(|adopted| adopted.task_state.as_ref()),
    )
    .map_err(rejected)?;
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
    let native = native::filter_retained_instructions_with_task_context(
        p.originals,
        &p.input,
        InstructionSelectionApplication {
            pruning: p.pruning.clone(),
            plan_hash: None,
            results: Vec::new(),
        },
        p.semantic.task_context,
        &p.protected_task_boundary_call_ids,
    )
    .map_err(integrity)?;
    let (summary_text, semantic) =
        emergency::emergency_summary_text(p.originals, p.semantic.previous_summary)
            .map_err(integrity)?;
    // The previous checkpoint's summary is last in its replacement, so every item before it was
    // kept by that checkpoint, whether it was Normal or Emergency.
    let durable_boundary = p.adopted.as_ref().map(|adopted| adopted.index);
    let markers = emergency::select_observation_markers(
        p.originals,
        &p.sources,
        &p.projections,
        durable_boundary,
    )
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
        markers: markers
            .iter()
            .map(|(index, projection)| (*index, projection))
            .collect(),
        work_boundary: p.semantic.work_boundary,
        summary_text,
    };
    let metadata = emergency::emergency_checkpoint_metadata(
        &plan.summary_text,
        semantic,
        p.input.snapshot().map_err(integrity)?.hash().to_owned(),
        &p.pruning,
        p.adopted.as_ref().and_then(|adopted| {
            adopted
                .metadata
                .as_ref()
                .or(adopted.inherited_capacity_metadata.as_ref())
        }),
        &plan.markers,
    )
    .map_err(integrity)?;
    let mut items = plan.build(&p.initial_context).map_err(integrity)?;
    attach_summary_metadata(
        &mut items,
        &ctx.sub_id,
        &plan.summary_text,
        &metadata,
        p.adopted
            .as_ref()
            .and_then(|adopted| adopted.task_state.as_ref()),
    )
    .map_err(integrity)?;
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
pub(super) fn attach_summary_metadata(
    items: &mut [ResponseItemEnvelope],
    turn_id: &str,
    summary_text: &str,
    metadata: &RenCrowCompactionMetadataV2,
    task_state: Option<&serde_json::Value>,
) -> Result<(), String> {
    let summary = items
        .last_mut()
        .ok_or_else(|| "compaction replacement has no summary".to_owned())?;
    summary.set_turn_id_if_missing(turn_id);
    let value = if let Some(task_state) = task_state {
        let wrapped = codex_history::RenCrowTaskCheckpointMetadataV3::capacity(
            summary_text,
            Some(task_state.clone()),
            metadata.clone(),
        );
        serde_json::to_value(wrapped)
    } else {
        serde_json::to_value(metadata)
    }
    .map_err(|error| format!("failed to encode compaction metadata: {error}"))?;
    summary.metadata.get_or_insert_default().rencrow_compaction = Some(value);
    Ok(())
}

/// Attach a deterministic task-boundary checkpoint without any fabricated model response data.
pub(crate) fn attach_task_boundary_metadata(
    items: &mut [ResponseItemEnvelope],
    turn_id: &str,
    summary_text: &str,
    task_state: &codex_history::task_context::StoredTaskState,
    source_fence: codex_history::TaskBoundarySourceFence,
) -> Result<(), String> {
    let summary = items
        .last_mut()
        .ok_or_else(|| "task boundary replacement has no summary".to_owned())?;
    let typed = matches!(
        &summary.item,
        codex_protocol::models::ResponseItem::Message {
            role,
            internal_chat_message_metadata_passthrough: Some(passthrough),
            content,
            ..
        } if role == "user"
            && matches!(passthrough.content_item_kinds.as_deref(), Some([kind]) if kind.0 == "compaction.summary")
            && matches!(content.as_slice(), [codex_protocol::models::ContentItem::InputText { text }] if text == summary_text)
    );
    if !typed {
        return Err("task boundary output is not one typed compaction summary".into());
    }
    task_state.validate(&task_state.boundary.thread_id)?;
    let checkpoint = codex_history::RenCrowTaskCheckpointMetadataV3::task_boundary(
        summary_text,
        serde_json::to_value(task_state).map_err(|error| error.to_string())?,
        source_fence,
    );
    checkpoint.validate_for_checkpoint(&task_state.boundary.thread_id, summary_text)?;
    summary.set_turn_id_if_missing(turn_id);
    summary.metadata.get_or_insert_default().rencrow_compaction =
        Some(serde_json::to_value(checkpoint).map_err(|error| error.to_string())?);
    Ok(())
}
