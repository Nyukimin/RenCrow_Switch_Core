//! Level 2 deterministic emergency compaction (Part 2 §25–33, §41–55).
//!
//! Emergency sends no model request and makes no new semantic decision. It keeps exact Humans with
//! already accepted removals, protected and opaque items, every unhandled observation pair, and
//! all ordinary Work after the semantic boundary. It may only drop Work already represented by the
//! previous semantic summary and replace eligible large outputs with verified V2 markers.

use super::native::NativeProjection;
use super::native::content_text;
use super::native::host_insertion_index;
use super::native::is_user_summary;
use super::observation::cumulative_observation_coverage;
use crate::compact::SUMMARY_PREFIX;
use crate::context::CompactionSummary;
use crate::context::ContextualUserFragment;
use codex_history::RenCrowCompactionMetadataV2;
use codex_history::ResponseItemEnvelope;
use codex_history::archive_reference::content_sha256;
use codex_history::compaction_candidate::CandidateInput;
use codex_history::compaction_candidate::Origin;
use codex_history::compaction_checkpoint_metadata::CompactionSelectionMode;
use codex_history::compaction_preprocess::InstructionPruning;
use codex_history::observation_marker::observation_marker_body;
use codex_history::observation_marker::observation_marker_metadata;
use codex_history::observation_marker::verify_observation_marker;
use codex_history::observation_projection::ObservationCoverage;
use codex_history::observation_projection::ObservationProjection;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::ResponseItem;
use codex_rollout::PreparedCompactionReferenceKind;
use codex_rollout::PreparedCompactionSources;
use std::collections::HashMap;
use std::collections::HashSet;

/// Host text used only when no semantic summary has been accepted (Part 2 §32). It is not a
/// semantic summary and is never shown to a later summary request as one.
pub(super) const EMERGENCY_PLACEHOLDER: &str =
    "No semantic summary has been accepted.\nUnsummarized work remains explicitly retained.";

/// Final summary body of an emergency checkpoint and whether it is a carried semantic summary.
pub(super) fn emergency_summary_text(
    originals: &[ResponseItemEnvelope],
    previous_summary: Option<usize>,
) -> Result<(String, bool), String> {
    let Some(index) = previous_summary else {
        return Ok((format!("{SUMMARY_PREFIX}\n{EMERGENCY_PLACEHOLDER}"), false));
    };
    let carried = match originals.get(index).map(|summary| &summary.item) {
        Some(ResponseItem::Message { role, content, .. })
            if role == "user" && matches!(content.as_slice(), [ContentItem::InputText { .. }]) =>
        {
            content_text(content)
        }
        _ => return Err("previous semantic summary is not one user text item".into()),
    };
    if !carried.starts_with(&format!("{SUMMARY_PREFIX}\n")) {
        return Err("previous semantic summary cannot be carried with its exact body".into());
    }
    Ok((carried, true))
}

/// Choose unhandled fresh outputs whose V2 marker is smaller than the live output (§36).
///
/// Only verified fresh pairs qualify: V1 archive references and existing V2 markers are already
/// compact, and protected, active, ambiguous, or non-text outputs never reach the prepared pairs.
pub(super) fn select_observation_markers<'a>(
    originals: &[ResponseItemEnvelope],
    prepared: &PreparedCompactionSources<'_>,
    projections: &'a [(usize, usize, ObservationProjection)],
) -> Vec<(usize, &'a ObservationProjection)> {
    projections
        .iter()
        .filter(|(_, output_index, _)| {
            prepared.pairs.iter().any(|pair| {
                pair.output_index == *output_index
                    && pair.reference_kind == PreparedCompactionReferenceKind::Fresh
            })
        })
        .filter(|(_, output_index, projection)| {
            live_output_text(&originals[*output_index])
                .is_some_and(|text| observation_marker_body(projection).len() < text.len())
        })
        .map(|(_, output_index, projection)| (*output_index, projection))
        .collect()
}

fn live_output_text(envelope: &ResponseItemEnvelope) -> Option<&str> {
    match &envelope.item {
        ResponseItem::FunctionCallOutput { output, .. }
        | ResponseItem::CustomToolCallOutput { output, .. } => output.text_content(),
        _ => None,
    }
}

/// Replace one output body with its V2 marker, keeping the item type and identity (§33–35).
fn marker_output(
    original: &ResponseItemEnvelope,
    projection: &ObservationProjection,
) -> Result<ResponseItemEnvelope, String> {
    let mut marker = original.clone();
    match &mut marker.item {
        ResponseItem::FunctionCallOutput { output, .. }
        | ResponseItem::CustomToolCallOutput { output, .. } => {
            output.body = FunctionCallOutputBody::Text(observation_marker_body(projection));
        }
        _ => return Err("observation marker target is not a tool output".into()),
    }
    marker.metadata = Some(observation_marker_metadata(
        original.metadata.as_ref(),
        &projection.coverage,
    )?);
    Ok(marker)
}

/// Deterministic emergency decisions shared by the builder and its validator.
pub(super) struct EmergencyPlan<'a> {
    pub(super) originals: &'a [ResponseItemEnvelope],
    pub(super) input: &'a CandidateInput,
    /// Humans with only already accepted removals, plus protected and opaque items.
    pub(super) native: &'a NativeProjection,
    /// Call and output slots of every observation not yet covered by an accepted summary.
    pub(super) unhandled_pairs: Vec<(usize, usize)>,
    pub(super) markers: Vec<(usize, &'a ObservationProjection)>,
    pub(super) work_boundary: Option<usize>,
    pub(super) summary_text: String,
}

/// What the plan requires at one original history slot.
#[derive(Clone, Copy)]
enum Slot<'a> {
    Retained(&'a ResponseItemEnvelope),
    Marker(&'a ObservationProjection),
    Original,
    Dropped,
}

impl<'a> EmergencyPlan<'a> {
    fn slots(&self) -> Result<Vec<Slot<'_>>, String> {
        if self.input.records.len() != self.originals.len() {
            return Err("emergency history and candidate record counts differ".into());
        }
        let retained = self
            .native
            .retained_by_original_index()
            .collect::<HashMap<_, _>>();
        let pair_slots = self
            .unhandled_pairs
            .iter()
            .flat_map(|(call, output)| [*call, *output])
            .collect::<HashSet<_>>();
        let markers = self.markers.iter().copied().collect::<HashMap<_, _>>();
        if markers.keys().any(|index| !pair_slots.contains(index)) {
            return Err("emergency marker targets a handled or unpaired output".into());
        }
        Ok(self
            .input
            .records
            .iter()
            .zip(self.originals)
            .enumerate()
            .map(|(index, (record, original))| {
                if let Some(envelope) = retained.get(&index) {
                    Slot::Retained(envelope)
                } else if let Some(projection) = markers.get(&index) {
                    Slot::Marker(projection)
                } else if pair_slots.contains(&index)
                    || (record.origin == Origin::Work
                        && self.work_boundary.is_none_or(|boundary| index > boundary)
                        && !is_user_summary(original))
                {
                    Slot::Original
                } else {
                    Slot::Dropped
                }
            })
            .collect())
    }

    fn summary_item(&self) -> ResponseItemEnvelope {
        ResponseItemEnvelope::new(ContextualUserFragment::into(CompactionSummary::new(
            &self.summary_text,
        )))
    }

    /// Build the replacement history with a typed summary last and initial context in its slot.
    pub(super) fn build(
        &self,
        initial_context: &[ResponseItemEnvelope],
    ) -> Result<Vec<ResponseItemEnvelope>, String> {
        let mut body = Vec::new();
        for (slot, original) in self.slots()?.into_iter().zip(self.originals) {
            match slot {
                Slot::Retained(envelope) => body.push(envelope.clone()),
                Slot::Marker(projection) => body.push(marker_output(original, projection)?),
                Slot::Original => body.push(original.clone()),
                Slot::Dropped => {}
            }
        }
        let summary = self.summary_item();
        let insertion = insertion_index(&body, &summary);
        body.splice(insertion..insertion, initial_context.iter().cloned());
        body.push(summary);
        Ok(body)
    }

    /// Check a candidate against the plan without trusting the builder (§54).
    ///
    /// Exact Humans, protected items, unhandled pairs, and unsummarized Work must appear in order;
    /// each marker must verify against its projection; the initial context must sit in the host
    /// slot; and the typed summary must carry exactly the planned body.
    pub(super) fn validate(
        &self,
        initial_context: &[ResponseItemEnvelope],
        candidate: &[ResponseItemEnvelope],
    ) -> Result<(), String> {
        let (summary, rest) = candidate
            .split_last()
            .ok_or_else(|| "emergency candidate is empty".to_owned())?;
        match &summary.item {
            ResponseItem::Message { role, content, .. }
                if role == "user"
                    && matches!(content.as_slice(), [ContentItem::InputText { text }] if *text == self.summary_text) =>
                {}
            _ => return Err("emergency candidate does not end with its planned summary".into()),
        }
        let body = remove_initial_context(rest, initial_context, summary)?;
        let mut body = body.iter();
        for (slot, original) in self.slots()?.into_iter().zip(self.originals) {
            let expected = match slot {
                Slot::Dropped => continue,
                Slot::Retained(envelope) => Some(envelope),
                Slot::Original => Some(original),
                Slot::Marker(_) => None,
            };
            let actual = body
                .next()
                .ok_or_else(|| "emergency candidate omits a retained item".to_owned())?;
            match (slot, expected) {
                (Slot::Marker(projection), _) => {
                    verify_marker_output(original, actual, projection)?;
                }
                (_, Some(expected)) if actual == expected => {}
                _ => {
                    return Err(
                        "emergency candidate changed, reordered, or dropped a retained item".into(),
                    );
                }
            }
        }
        if body.next().is_some() {
            return Err("emergency candidate adds items outside the plan".into());
        }
        Ok(())
    }
}

fn insertion_index(body: &[ResponseItemEnvelope], summary: &ResponseItemEnvelope) -> usize {
    host_insertion_index(body.iter().chain(std::iter::once(summary)))
        .unwrap_or(body.len())
        .min(body.len())
}

/// Recover the body by removing the initial context from the slot the host would choose.
fn remove_initial_context(
    items: &[ResponseItemEnvelope],
    initial_context: &[ResponseItemEnvelope],
    summary: &ResponseItemEnvelope,
) -> Result<Vec<ResponseItemEnvelope>, String> {
    if initial_context.is_empty() {
        return Ok(items.to_vec());
    }
    let body_len = items
        .len()
        .checked_sub(initial_context.len())
        .ok_or_else(|| "emergency candidate omits the initial context".to_owned())?;
    for start in 0..=body_len {
        if items[start..start + initial_context.len()] != *initial_context {
            continue;
        }
        let mut body = items[..start].to_vec();
        body.extend_from_slice(&items[start + initial_context.len()..]);
        if insertion_index(&body, summary) == start {
            return Ok(body);
        }
    }
    Err("emergency candidate places the initial context outside the host slot".into())
}

fn verify_marker_output(
    original: &ResponseItemEnvelope,
    actual: &ResponseItemEnvelope,
    projection: &ObservationProjection,
) -> Result<(), String> {
    let body =
        live_output_text(actual).ok_or_else(|| "emergency marker output is not text".to_owned())?;
    verify_observation_marker(
        projection,
        original.metadata.as_ref(),
        actual.metadata.as_ref(),
        body,
    )?;
    // Only the body and metadata may change; the item type and identity stay the original's.
    let mut restored = actual.clone();
    restored.metadata = original.metadata.clone();
    match (&mut restored.item, &original.item) {
        (
            ResponseItem::FunctionCallOutput { output, .. },
            ResponseItem::FunctionCallOutput {
                output: original_output,
                ..
            },
        )
        | (
            ResponseItem::CustomToolCallOutput { output, .. },
            ResponseItem::CustomToolCallOutput {
                output: original_output,
                ..
            },
        ) => output.body = original_output.body.clone(),
        _ => return Err("emergency marker changed the output item type".into()),
    }
    if restored != *original {
        return Err("emergency marker changed the output identity".into());
    }
    Ok(())
}

/// Metadata for an emergency checkpoint (§41–49).
///
/// No semantic state advances: the previous summary coverage and important refs are carried,
/// only still-applicable accepted removals remain, and new markers join the inventory without
/// becoming summary-covered.
pub(super) fn emergency_checkpoint_metadata(
    summary_text: &str,
    semantic: bool,
    snapshot_hash: String,
    pruning: &InstructionPruning,
    previous: Option<&RenCrowCompactionMetadataV2>,
    markers: &[(usize, &ObservationProjection)],
) -> Result<RenCrowCompactionMetadataV2, String> {
    let marker_coverage = markers
        .iter()
        .map(|(_, projection)| projection.coverage.clone())
        .collect::<Vec<ObservationCoverage>>();
    let observations = cumulative_observation_coverage(
        previous.map_or(&[][..], |previous| previous.observations.as_slice()),
        &marker_coverage,
    )?;
    let summary_hash = content_sha256(summary_text);
    Ok(RenCrowCompactionMetadataV2 {
        version: 2,
        snapshot_hash,
        presentation_hash: None,
        semantic_summary_hash: semantic.then(|| summary_hash.clone()),
        summary_hash,
        selection_mode: CompactionSelectionMode::DeterministicEmergency,
        plan_hash: None,
        applied_refs: pruning.applied.clone(),
        results: Vec::new(),
        observations,
        summary_covered_observations: previous
            .map(|previous| previous.summary_covered_observations.clone())
            .unwrap_or_default(),
        important_refs: previous
            .map(|previous| previous.important_refs.clone())
            .unwrap_or_default(),
        model: None,
        effort: None,
        responses: Vec::new(),
        transaction_following_items: None,
        committed_transaction_hash: None,
    })
}

/// Require the final item to be the typed summary carrying exactly the expected metadata.
pub(super) fn validate_emergency_summary(
    summary: &ResponseItemEnvelope,
    summary_text: &str,
    thread_id: &str,
    expected: &RenCrowCompactionMetadataV2,
) -> Result<(), String> {
    let typed = matches!(
        &summary.item,
        ResponseItem::Message {
            internal_chat_message_metadata_passthrough: Some(passthrough),
            ..
        } if matches!(passthrough.content_item_kinds.as_deref(), Some([kind]) if kind.0 == "compaction.summary")
    );
    if !typed {
        return Err("emergency summary is not marked compaction.summary".into());
    }
    let metadata = summary
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.rencrow_compaction.clone())
        .ok_or_else(|| "emergency summary has no V2 checkpoint metadata".to_owned())?;
    let metadata =
        RenCrowCompactionMetadataV2::parse_and_validate(metadata, thread_id, summary_text)?;
    if metadata != *expected {
        return Err("emergency checkpoint metadata differs from the deterministic plan".into());
    }
    Ok(())
}

#[cfg(test)]
#[path = "compact_rencrow_emergency_tests.rs"]
mod tests;
