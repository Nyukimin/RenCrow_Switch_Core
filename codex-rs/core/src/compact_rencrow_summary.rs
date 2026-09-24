//! Pure summary-history projection and staged-output validation for V2 compaction.

use super::native::NativeProjection;
use super::native::is_user_summary;
use crate::context::CompactionObservation;
use crate::context::ContextualUserFragment;
use codex_history::ResponseItemEnvelope;
use codex_history::archive_reference::ObservationReference;
use codex_history::compaction_candidate::CandidateInput;
use codex_history::compaction_candidate::Origin;
use codex_history::compaction_checkpoint_metadata::RenCrowCompactionMetadataV2;
use codex_history::observation_projection::ObservationSummaryExcerpt;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use std::collections::HashMap;
use std::collections::HashSet;

/// Only this exact prefix starts an important-observation marker; the JSON string follows it.
const IMPORTANT_REF_MARKER: &str = "observation:\"";

/// The latest adopted V2 compaction summary in the current selected history.
#[derive(Debug, PartialEq)]
pub(super) struct AdoptedV2Checkpoint {
    pub(super) index: usize,
    pub(super) metadata: RenCrowCompactionMetadataV2,
    /// Exact summary message body bound by `metadata.summary_hash`.
    pub(super) summary_text: String,
}

/// Find the latest adopted V2 summary in session-owned history.
///
/// The owner replaces live history only after a durable commit and replays only committed
/// transactions, so presence here means adoption. The lifecycle fields may therefore carry either
/// the live prepared count or the replayed committed hash; the fresh-candidate rule that both are
/// absent does not apply. Summaries without V2 metadata are not boundaries. V2 metadata on an item
/// that is not a valid typed summary fails closed instead of falling back to an older checkpoint.
pub(super) fn find_adopted_v2_checkpoint(
    items: &[ResponseItemEnvelope],
    thread_id: &str,
) -> Result<Option<AdoptedV2Checkpoint>, String> {
    for (index, envelope) in items.iter().enumerate().rev() {
        let Some(metadata) = envelope
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.rencrow_compaction.as_ref())
        else {
            continue;
        };
        if metadata.get("version").and_then(serde_json::Value::as_u64) != Some(2) {
            continue;
        }
        let ResponseItem::Message {
            role,
            content,
            internal_chat_message_metadata_passthrough,
            ..
        } = &envelope.item
        else {
            return Err("V2 compaction metadata is not attached to a typed summary".into());
        };
        let typed = role == "user"
            && internal_chat_message_metadata_passthrough
                .as_ref()
                .and_then(|passthrough| passthrough.content_item_kinds.as_deref())
                .is_some_and(|kinds| matches!(kinds, [kind] if kind.0 == "compaction.summary"));
        let (true, [ContentItem::InputText { text }]) = (typed, content.as_slice()) else {
            return Err("V2 compaction metadata is not attached to a typed summary".into());
        };
        let metadata =
            RenCrowCompactionMetadataV2::parse_and_validate(metadata.clone(), thread_id, text)?;
        return Ok(Some(AdoptedV2Checkpoint {
            index,
            metadata,
            summary_text: text.clone(),
        }));
    }
    Ok(None)
}

/// Choose the one previous summary shown to the V2 summary request (Part 2 §18, Annex A F20).
///
/// The adopted V2 summary is used when present. Before the first V2 checkpoint, the latest
/// owner-adopted legacy summary is the only record of earlier summarized work, so it is shown once
/// instead of being dropped. It is not a selection or observation boundary.
pub(super) fn previous_summary_index(
    items: &[ResponseItemEnvelope],
    adopted: Option<&AdoptedV2Checkpoint>,
) -> Option<usize> {
    adopted
        .map(|adopted| adopted.index)
        .or_else(|| items.iter().rposition(is_user_summary))
}

/// Build the model-visible history for the single V2 summary request.
///
/// Humans and protected items come from the native projection, so the summary sees exactly what
/// the replacement retains. Ordinary Work before the adopted summary is already represented by
/// that summary and is omitted. Each verified observation pair becomes one bounded host item.
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
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct ImportantRefResolution {
    pub(super) refs: Vec<ObservationReference>,
    pub(super) ignored_markers: u32,
}

/// Resolve `observation:"<call_id>"` markers against the host inventory without failing.
///
/// Malformed, unknown, or ambiguous markers are ignored and counted; ordinary prose such as
/// `key observation: ...` is not a marker. Resolved references are deduplicated in first order.
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
