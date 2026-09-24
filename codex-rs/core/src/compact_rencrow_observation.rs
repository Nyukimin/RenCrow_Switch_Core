//! Observation handling for V2 compaction: new verified pairs, the cumulative inventory, and
//! host completion-candidate links.

use codex_history::ResponseItemEnvelope;
use codex_history::archive_reference::ObservationReference;
use codex_history::compaction_candidate::CandidateInput;
use codex_history::compaction_candidate::Origin;
use codex_history::compaction_plan::ByteRange;
use codex_history::compaction_preprocess::InstructionObservationLink;
use codex_history::compaction_preprocess::InstructionPruning;
use codex_history::observation_projection::OBSERVATION_PART_FULL_LIMIT_BYTES;
use codex_history::observation_projection::ObservationCoverage;
use codex_history::observation_projection::ObservationProjection;
use codex_history::observation_projection::project_existing_observation;
use codex_history::observation_projection::project_observation;
use codex_protocol::models::ResponseItem;
use codex_rollout::PreparedCompactionReferenceKind;
use codex_rollout::PreparedCompactionSources;
use std::collections::BTreeMap;

/// Project verified pairs whose observation identity is not already covered.
///
/// Handling is decided by exact `thread_id + call_id + sha256` identity, never by position, so a
/// call that was still active at the previous checkpoint is projected once it completes. A covered
/// call ID with a different output digest is a stored-data conflict and fails closed. Existing
/// references project the current call text and the canonical output length without rehydrating
/// the archived output body.
pub(super) fn project_unhandled_observations(
    originals: &[ResponseItemEnvelope],
    prepared: &PreparedCompactionSources<'_>,
    covered: &[ObservationReference],
) -> Result<Vec<(usize, usize, ObservationProjection)>, String> {
    let mut projections = Vec::new();
    for pair in &prepared.pairs {
        let reference = &pair.reference;
        if covered.iter().any(|known| {
            known.validates_identity(&reference.thread_id, &reference.call_id)
                && known.sha256 != reference.sha256
        }) {
            return Err("observation call ID was recorded with a different output digest".into());
        }
        if covered.contains(reference) {
            continue;
        }
        let projection = match pair.reference_kind {
            // A verified V2 marker is re-projected from its canonical raw text, like a fresh pair.
            PreparedCompactionReferenceKind::Fresh
            | PreparedCompactionReferenceKind::ObservationMarker => {
                let (Some(call_text), Some(output_text)) =
                    (pair.canonical_call_input, pair.canonical_output_text)
                else {
                    return Err("fresh observation has no canonical call or output text".into());
                };
                project_observation(reference, pair.tool_name, call_text, output_text)?
            }
            PreparedCompactionReferenceKind::Existing => {
                let call_text = match originals.get(pair.call_index).map(|call| &call.item) {
                    Some(ResponseItem::FunctionCall { arguments, .. }) => arguments,
                    Some(ResponseItem::CustomToolCall { input, .. }) => input,
                    _ => return Err("existing observation has no current tool call".into()),
                };
                project_existing_observation(
                    reference,
                    pair.tool_name,
                    call_text,
                    pair.output_total_bytes,
                )?
            }
        };
        projections.push((pair.call_index, pair.output_index, projection));
    }
    Ok(projections)
}

/// Merge adopted coverage with new coverage by exact observation identity.
///
/// The same identity is kept once in first order. The same thread and call ID with a different
/// output digest fails closed. The result is host-side inventory and is never sent to the model.
pub(super) fn cumulative_observation_coverage(
    previous: &[ObservationCoverage],
    current: &[ObservationCoverage],
) -> Result<Vec<ObservationCoverage>, String> {
    let mut merged: Vec<ObservationCoverage> = Vec::with_capacity(previous.len() + current.len());
    for coverage in previous.iter().chain(current) {
        let reference = &coverage.reference;
        match merged.iter().find(|known| {
            known
                .reference
                .validates_identity(&reference.thread_id, &reference.call_id)
        }) {
            Some(known) if known.reference.sha256 == reference.sha256 => {}
            Some(_) => {
                return Err(
                    "observation call ID was recorded with a different output digest".into(),
                );
            }
            None => merged.push(coverage.clone()),
        }
    }
    Ok(merged)
}

/// Extend the handled set with the observations presented to an accepted Normal summary.
///
/// Only an accepted Normal summary adds references (Part 2 §21–22); the caller passes exactly the
/// projections it presented. A deterministic emergency checkpoint carries the previous set.
pub(super) fn summary_covered_after_normal(
    previous: &[ObservationReference],
    presented: &[(usize, usize, ObservationProjection)],
) -> Vec<ObservationReference> {
    let mut covered = previous.to_vec();
    for (_, _, projection) in presented {
        let reference = &projection.coverage.reference;
        if !covered.contains(reference) {
            covered.push(reference.clone());
        }
    }
    covered
}

/// Build host completion-candidate links, at most one per unambiguous turn.
///
/// A link only allows semantic review to consider completion; it never proves success. A turn
/// yields a link only when its single user message is a verified Human with a stable ID and no
/// protected or already pruned text, and its only tool activity is one verified terminal
/// `exec_command` pair whose call and output are fully present in CandidateInput. Any tool item
/// without a turn ID cannot be attributed, so it disables linking for the whole history.
pub(super) fn build_completion_links(
    originals: &[ResponseItemEnvelope],
    input: &CandidateInput,
    pruning: &InstructionPruning,
    prepared: &PreparedCompactionSources<'_>,
) -> Result<Vec<InstructionObservationLink>, String> {
    if input.records.len() != originals.len() {
        return Err("completion link history and candidate record counts differ".into());
    }
    let mut turns = BTreeMap::<&str, (Vec<usize>, Vec<usize>)>::new();
    for (index, (record, envelope)) in input.records.iter().zip(originals).enumerate() {
        let tool = match &envelope.item {
            ResponseItem::LocalShellCall { .. }
            | ResponseItem::FunctionCall { .. }
            | ResponseItem::ToolSearchCall { .. }
            | ResponseItem::FunctionCallOutput { .. }
            | ResponseItem::CustomToolCall { .. }
            | ResponseItem::CustomToolCallOutput { .. }
            | ResponseItem::ToolSearchOutput { .. }
            | ResponseItem::WebSearchCall { .. }
            | ResponseItem::ImageGenerationCall { .. }
            | ResponseItem::Other => true,
            ResponseItem::AdditionalTools { .. }
            | ResponseItem::Message { .. }
            | ResponseItem::AgentMessage { .. }
            | ResponseItem::Reasoning { .. }
            | ResponseItem::Compaction { .. }
            | ResponseItem::ConfigurationUpdate { .. }
            | ResponseItem::CompactionTrigger { .. }
            | ResponseItem::ContextCompaction { .. } => false,
        };
        let user =
            record.role == "user" && matches!(record.origin, Origin::Human | Origin::Unknown);
        if !tool && !user {
            continue;
        }
        let Some(turn_id) = envelope.item.turn_id() else {
            if tool {
                return Ok(Vec::new());
            }
            continue;
        };
        let (users, tools) = turns.entry(turn_id).or_default();
        if tool {
            tools.push(index);
        } else {
            users.push(index);
        }
    }

    let snapshot = input.snapshot()?;
    let whole = |index: usize| {
        let record = &input.records[index];
        snapshot
            .reference(
                &record.id,
                ByteRange {
                    start: 0,
                    end: record.text.len(),
                },
            )
            .map_err(|error| format!("invalid completion link source: {error:?}"))
    };
    let bounded = |index: usize| {
        let text = &input.records[index].text;
        !text.is_empty() && text.len() <= OBSERVATION_PART_FULL_LIMIT_BYTES
    };
    let mut links = Vec::new();
    for (users, tools) in turns.values() {
        let ([human], [call, output]) = (users.as_slice(), tools.as_slice()) else {
            continue;
        };
        let Some(terminal) = prepared
            .pairs
            .iter()
            .find(|pair| pair.call_index == *call && pair.output_index == *output)
            .and_then(|pair| pair.terminal_reference.as_ref())
        else {
            continue;
        };
        let record = &input.records[*human];
        if record.origin != Origin::Human
            || record.opaque.is_some()
            || !record.protected.is_empty()
            || record.text.is_empty()
            || human >= call
            || pruning
                .applied
                .iter()
                .any(|reference| reference.id == record.id)
            || [*human, *call, *output]
                .iter()
                .any(|index| originals[*index].item.id().is_none())
            || !bounded(*call)
            || !bounded(*output)
        {
            continue;
        }
        links.push(InstructionObservationLink {
            instruction: whole(*human)?,
            call: whole(*call)?,
            output: whole(*output)?,
            terminal: terminal.clone(),
        });
    }
    Ok(links)
}
