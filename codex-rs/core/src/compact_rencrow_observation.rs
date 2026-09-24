//! Observation handling for V2 compaction: new verified pairs and the cumulative inventory.

use codex_history::ResponseItemEnvelope;
use codex_history::archive_reference::ObservationReference;
use codex_history::observation_projection::ObservationCoverage;
use codex_history::observation_projection::ObservationProjection;
use codex_history::observation_projection::project_existing_observation;
use codex_history::observation_projection::project_observation;
use codex_protocol::models::ResponseItem;
use codex_rollout::PreparedCompactionReferenceKind;
use codex_rollout::PreparedCompactionSources;

/// Project verified pairs whose observation identity is not already covered.
///
/// Handling is decided by exact `thread_id + call_id + sha256` identity, never by position, so a
/// call that was still active at the previous checkpoint is projected once it completes. A covered
/// call ID with a different output digest is a stored-data conflict and fails closed. Existing
/// references project the current call text and the canonical output length without rehydrating
/// the archived output body.
#[allow(dead_code)]
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
            PreparedCompactionReferenceKind::Fresh => {
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
#[allow(dead_code)]
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
