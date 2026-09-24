//! Pure summary-history projection and staged-output validation for V2 compaction.

use super::native::NativeProjection;
use super::native::is_user_summary;
use crate::context::CompactionObservation;
use crate::context::ContextualUserFragment;
use codex_history::ResponseItemEnvelope;
use codex_history::archive_reference::ObservationReference;
use codex_history::compaction_candidate::CandidateInput;
use codex_history::compaction_candidate::Origin;
use codex_history::observation_projection::ObservationSummaryExcerpt;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use std::collections::HashMap;
use std::collections::HashSet;

/// Only this exact prefix starts an important-observation marker; the JSON string follows it.
const IMPORTANT_REF_MARKER: &str = "observation:\"";

/// Build the model-visible history for the single V2 summary request.
///
/// Humans and protected items come from the native projection, so the summary sees exactly what
/// the replacement retains. Ordinary Work before the adopted summary is already represented by
/// that summary and is omitted. Each verified observation pair becomes one bounded host item.
#[allow(dead_code)]
pub(super) fn build_summary_history(
    originals: &[ResponseItemEnvelope],
    input: &CandidateInput,
    native: &NativeProjection,
    observations: &[(usize, usize, ObservationSummaryExcerpt)],
    adopted_summary_index: Option<usize>,
) -> Result<Vec<ResponseItemEnvelope>, String> {
    if input.records.len() != originals.len() {
        return Err("summary history and candidate record counts differ".into());
    }
    if let Some(index) = adopted_summary_index
        && originals
            .get(index)
            .is_none_or(|item| !is_user_summary(item))
    {
        return Err("adopted summary index does not identify a compaction summary".into());
    }
    let retained = native
        .retained_by_original_index()
        .collect::<HashMap<_, _>>();
    if retained.iter().any(|(index, envelope)| {
        originals
            .get(*index)
            .is_none_or(|original| original.item.id() != envelope.item.id())
    }) {
        return Err("native projection does not match the summary history identity".into());
    }

    let mut observation_calls = HashMap::new();
    let mut consumed = HashSet::new();
    for (call_index, output_index, excerpt) in observations {
        if call_index >= output_index {
            return Err("observation pair is reversed or collapsed".into());
        }
        if *output_index >= originals.len() {
            return Err("observation pair index is out of range".into());
        }
        if !consumed.insert(*call_index) || !consumed.insert(*output_index) {
            return Err("observation pairs overlap or repeat an item".into());
        }
        for index in [*call_index, *output_index] {
            let record = &input.records[index];
            if record.origin != Origin::Work
                || record.opaque.is_some()
                || !record.protected.is_empty()
                || retained.contains_key(&index)
                || is_user_summary(&originals[index])
            {
                return Err("observation pair slot is not verified ordinary work".into());
            }
        }
        let call_id = match (&originals[*call_index].item, &originals[*output_index].item) {
            (
                ResponseItem::FunctionCall { call_id, .. },
                ResponseItem::FunctionCallOutput {
                    call_id: Some(output_call_id),
                    ..
                },
            )
            | (
                ResponseItem::CustomToolCall { call_id, .. },
                ResponseItem::CustomToolCallOutput {
                    call_id: output_call_id,
                    ..
                },
            ) if call_id == output_call_id => call_id,
            _ => return Err("observation pair is not one native call and output".into()),
        };
        if *call_id != excerpt.reference.call_id {
            return Err("observation pair does not match its reference call".into());
        }
        observation_calls.insert(*call_index, excerpt);
    }

    let mut projected = Vec::new();
    for (index, (record, original)) in input.records.iter().zip(originals).enumerate() {
        if adopted_summary_index == Some(index) {
            projected.push(original.clone());
        } else if let Some(excerpt) = observation_calls.get(&index) {
            let body = serde_json::to_string(excerpt)
                .map(|excerpt| format!("{{\"observation\":{excerpt}}}"))
                .map_err(|error| format!("failed to encode bounded observation: {error}"))?;
            projected.push(ResponseItemEnvelope::new(ContextualUserFragment::into(
                CompactionObservation::new(body),
            )));
        } else if consumed.contains(&index) {
            continue;
        } else if let Some(envelope) = retained.get(&index) {
            projected.push((*envelope).clone());
        } else if record.origin == Origin::Work
            && adopted_summary_index.is_none_or(|boundary| index > boundary)
            && !is_user_summary(original)
        {
            projected.push(original.clone());
        }
    }
    Ok(projected)
}

/// Accept only reasoning plus exactly one assistant text message from a staged summary response.
#[allow(dead_code)]
pub(super) fn summary_suffix_from_staged_output(output: &[ResponseItem]) -> Result<String, String> {
    let mut summary = None;
    for item in output {
        match item {
            ResponseItem::Reasoning { .. } => {}
            ResponseItem::Message { role, content, .. } if role == "assistant" => {
                if summary.is_some() {
                    return Err(
                        "compaction summary response has multiple assistant messages".into(),
                    );
                }
                let mut text = String::new();
                for part in content {
                    match part {
                        ContentItem::OutputText { text: part } => text.push_str(part),
                        ContentItem::InputText { .. }
                        | ContentItem::InputImage { .. }
                        | ContentItem::InputAudio { .. } => {
                            return Err("compaction summary response has non-text content".into());
                        }
                    }
                }
                summary = Some(text);
            }
            _ => {
                return Err("compaction summary response contains a non-assistant item".into());
            }
        }
    }
    let summary =
        summary.ok_or_else(|| "compaction summary response has no assistant message".to_owned())?;
    if summary.trim().is_empty() {
        return Err("compaction summary is empty".into());
    }
    Ok(summary)
}

/// Important references resolved from explicit summary markers, plus the ignored marker count.
#[allow(dead_code)]
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct ImportantRefResolution {
    pub(super) refs: Vec<ObservationReference>,
    pub(super) ignored_markers: u32,
}

/// Resolve `observation:"<call_id>"` markers against the host inventory without failing.
///
/// Malformed, unknown, or ambiguous markers are ignored and counted; ordinary prose such as
/// `key observation: ...` is not a marker. Resolved references are deduplicated in first order.
#[allow(dead_code)]
pub(super) fn important_refs_from_summary(
    summary_text: &str,
    inventory: &[ObservationReference],
) -> ImportantRefResolution {
    let mut resolution = ImportantRefResolution::default();
    let mut rest = summary_text;
    while let Some(position) = rest.find(IMPORTANT_REF_MARKER) {
        let literal = &rest[position + IMPORTANT_REF_MARKER.len() - 1..];
        let mut escaped = false;
        let closing = literal.char_indices().skip(1).find(|(_, ch)| {
            let closes = !escaped && *ch == '"';
            escaped = !escaped && *ch == '\\';
            closes
        });
        let Some((end, _)) = closing else {
            resolution.ignored_markers += 1;
            break;
        };
        rest = &literal[end + 1..];
        let Ok(call_id) = serde_json::from_str::<String>(&literal[..=end]) else {
            resolution.ignored_markers += 1;
            continue;
        };
        let mut matches = inventory
            .iter()
            .filter(|reference| reference.call_id == call_id);
        match (matches.next(), matches.next()) {
            (Some(reference), None) => {
                if !resolution.refs.contains(reference) {
                    resolution.refs.push(reference.clone());
                }
            }
            (None, _) | (Some(_), Some(_)) => resolution.ignored_markers += 1,
        }
    }
    resolution
}

/// Keep compaction-only server reasoning metadata out of the live context policy.
pub(crate) fn should_apply_server_reasoning_included(rencrow_compaction: bool) -> bool {
    !rencrow_compaction
}
