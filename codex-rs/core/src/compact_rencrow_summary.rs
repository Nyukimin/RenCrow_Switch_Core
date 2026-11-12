//! Pure summary-history projection and staged-output validation for V2 compaction.

use super::native::NativeProjection;
use super::native::is_user_summary;
use crate::context::CompactionObservation;
use crate::context::ContextualUserFragment;
use codex_history::ResponseItemEnvelope;
use codex_history::archive_reference::ObservationReference;
use codex_history::compaction_candidate::CandidateInput;
use codex_history::compaction_candidate::Origin;
use codex_history::compaction_checkpoint_metadata::CompactionSelectionMode;
use codex_history::compaction_checkpoint_metadata::RenCrowCompactionMetadataV2;
use codex_history::observation_projection::ObservationSummaryExcerpt;
use codex_history::task_checkpoint_metadata::RenCrowTaskCheckpointMetadataV3;
use codex_history::task_checkpoint_metadata::TaskCheckpointMode;
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
    pub(super) metadata: Option<RenCrowCompactionMetadataV2>,
    /// Prior capacity provenance inherited through a task boundary. It is separate from the
    /// active V3 envelope and is used only for existing V2 observation/pruning carry-forward.
    pub(super) inherited_capacity_metadata: Option<RenCrowCompactionMetadataV2>,
    /// Host-accepted task state carried by V3, including across capacity checkpoints.
    pub(super) task_state: Option<serde_json::Value>,
    /// Latest validated task-boundary envelope that owns the carried state, if any.
    pub(super) task_context_index: Option<usize>,
    /// A task boundary is a durable summary, but does not assert that all prior work or humans
    /// were semantically processed.
    pub(super) task_boundary: bool,
    /// Exact summary message body bound by `metadata.summary_hash`.
    pub(super) summary_text: String,
}

/// Find the latest adopted V2 or V3 summary in session-owned history.
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
        let version = metadata.get("version").and_then(serde_json::Value::as_u64);
        if version == Some(1) {
            continue;
        }
        if version.is_none() {
            if metadata.get("version").is_some() {
                return Err("RenCrow compaction checkpoint version is malformed".into());
            }
            continue;
        }
        if !matches!(version, Some(2 | 3)) {
            return Err("unsupported RenCrow compaction checkpoint version".into());
        }
        let text = checkpoint_summary_body(envelope)?;
        let (metadata, inherited_capacity_metadata, task_state, task_boundary) = match version {
            Some(2) => (
                Some(RenCrowCompactionMetadataV2::parse_and_validate(
                    metadata.clone(),
                    thread_id,
                    text,
                )?),
                None,
                None,
                false,
            ),
            Some(3) => {
                let checkpoint = RenCrowTaskCheckpointMetadataV3::parse_and_validate(
                    metadata.clone(),
                    thread_id,
                    text,
                )?;
                let task_boundary = checkpoint.mode == TaskCheckpointMode::TaskBoundary;
                let task_state = checkpoint.task_state.clone();
                let (capacity, inherited) = if task_boundary {
                    (None, previous_capacity_metadata(items, index, thread_id)?)
                } else {
                    (Some(checkpoint.into_capacity_metadata()?), None)
                };
                (capacity, inherited, task_state, task_boundary)
            }
            _ => unreachable!("version checked above"),
        };
        let task_context_index = if task_boundary {
            Some(index)
        } else if let Some(task_state) = task_state.as_ref() {
            let context = latest_task_context_index(items, index, thread_id, task_state)?;
            if context.is_none() {
                return Err(
                    "V3 capacity checkpoint carries task state without its typed task context envelope"
                        .into(),
                );
            }
            context
        } else {
            None
        };
        return Ok(Some(AdoptedV2Checkpoint {
            index,
            metadata,
            inherited_capacity_metadata,
            task_state,
            task_context_index,
            task_boundary,
            summary_text: text.to_owned(),
        }));
    }
    Ok(None)
}

fn latest_task_context_index(
    items: &[ResponseItemEnvelope],
    before_index: usize,
    thread_id: &str,
    task_state: &serde_json::Value,
) -> Result<Option<usize>, String> {
    for (index, envelope) in items[..before_index].iter().enumerate().rev() {
        let Some(value) = envelope
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.rencrow_compaction.as_ref())
        else {
            continue;
        };
        if value.get("version").and_then(serde_json::Value::as_u64) != Some(3) {
            continue;
        }
        let text = checkpoint_summary_body(envelope)?;
        let checkpoint =
            RenCrowTaskCheckpointMetadataV3::parse_and_validate(value.clone(), thread_id, text)?;
        if checkpoint.mode == TaskCheckpointMode::TaskBoundary
            && checkpoint.task_state.as_ref() == Some(task_state)
        {
            return Ok(Some(index));
        }
    }
    Ok(None)
}

/// Carry the previous accepted V2 projection through a task-boundary V3 without copying it into
/// the task snapshot. Observation coverage and prior pruning receipts still belong to the existing
/// capacity-compression owner.
fn previous_capacity_metadata(
    items: &[ResponseItemEnvelope],
    before_index: usize,
    thread_id: &str,
) -> Result<Option<RenCrowCompactionMetadataV2>, String> {
    for envelope in items[..before_index].iter().rev() {
        let Some(value) = envelope
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.rencrow_compaction.as_ref())
        else {
            continue;
        };
        let version = value.get("version").and_then(serde_json::Value::as_u64);
        if version == Some(1) {
            continue;
        }
        if version.is_none() {
            if value.get("version").is_some() {
                return Err("RenCrow compaction checkpoint version is malformed".into());
            }
            continue;
        }
        if !matches!(version, Some(2 | 3)) {
            return Err("unsupported RenCrow compaction checkpoint version".into());
        }
        let text = checkpoint_summary_body(envelope)?;
        match version {
            Some(2) => {
                return RenCrowCompactionMetadataV2::parse_and_validate(
                    value.clone(),
                    thread_id,
                    text,
                )
                .map(Some);
            }
            Some(3) => {
                let checkpoint = RenCrowTaskCheckpointMetadataV3::parse_and_validate(
                    value.clone(),
                    thread_id,
                    text,
                )?;
                if checkpoint.mode == TaskCheckpointMode::Capacity {
                    return checkpoint.into_capacity_metadata().map(Some);
                }
            }
            _ => unreachable!("version checked above"),
        }
    }
    Ok(None)
}

pub(super) fn checkpoint_summary_body(envelope: &ResponseItemEnvelope) -> Result<&str, String> {
    let ResponseItem::Message {
        role,
        content,
        internal_chat_message_metadata_passthrough,
        ..
    } = &envelope.item
    else {
        return Err("compaction metadata is not attached to a typed summary".into());
    };
    let typed = role == "user"
        && internal_chat_message_metadata_passthrough
            .as_ref()
            .and_then(|passthrough| passthrough.content_item_kinds.as_deref())
            .is_some_and(|kinds| matches!(kinds, [kind] if kind.0 == "compaction.summary"));
    let (true, [ContentItem::InputText { text }]) = (typed, content.as_slice()) else {
        return Err("compaction metadata is not attached to a typed summary".into());
    };
    Ok(text)
}

/// Positions that separate already summarized state from unsummarized state (Part 2 §18, §29–32).
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct SemanticContext {
    /// The previous accepted semantic summary, shown once to Summary and carried by Emergency.
    pub(super) previous_summary: Option<usize>,
    /// Latest accepted prefixless task JSON; it is distinct from semantic summary text.
    pub(super) task_context: Option<usize>,
    /// Ordinary Work before this index is already represented by the previous summary.
    pub(super) work_boundary: Option<usize>,
    /// Humans before this index were already considered by an accepted selection.
    pub(super) selection_boundary: Option<usize>,
}

/// Locate the semantic boundary, which differs from the durable checkpoint boundary.
///
/// A Normal V2 checkpoint is all three positions. An Emergency checkpoint is durable but not
/// semantic: every ordinary Work item it retained is unsummarized, its Humans need a new
/// selection, and only a carried semantic summary is shown again; a host placeholder is not.
/// Before the first V2 checkpoint, the latest owner-adopted legacy summary is the only record of
/// earlier summarized work, so it is shown once and bounds ordinary Work, but not selection.
pub(super) fn semantic_context(
    items: &[ResponseItemEnvelope],
    adopted: Option<&AdoptedV2Checkpoint>,
) -> SemanticContext {
    match adopted {
        Some(adopted) if adopted.task_boundary => SemanticContext {
            previous_summary: adopted
                .inherited_capacity_metadata
                .as_ref()
                .and_then(|metadata| metadata.semantic_summary_hash.as_deref())
                .and_then(|hash| matching_semantic_summary(items, adopted.index, hash)),
            task_context: adopted.task_context_index,
            // The source fence records only the exact refs removed at the boundary. Unrelated
            // historical work and instructions remain semantically unprocessed.
            work_boundary: None,
            selection_boundary: None,
        },
        Some(adopted)
            if adopted.metadata.as_ref().is_some_and(|metadata| {
                metadata.selection_mode == CompactionSelectionMode::DeterministicEmergency
            }) =>
        {
            SemanticContext {
                previous_summary: adopted
                    .metadata
                    .as_ref()
                    .or(adopted.inherited_capacity_metadata.as_ref())
                    .and_then(|metadata| metadata.semantic_summary_hash.as_ref())
                    .is_some()
                    .then_some(adopted.index),
                task_context: adopted.task_context_index,
                work_boundary: None,
                selection_boundary: None,
            }
        }
        Some(adopted) => SemanticContext {
            previous_summary: Some(adopted.index),
            task_context: adopted.task_context_index,
            work_boundary: Some(adopted.index),
            selection_boundary: (adopted.metadata.as_ref().is_some_and(|metadata| {
                metadata.selection_mode != CompactionSelectionMode::ConservativeSelection
            }))
            .then_some(adopted.index),
        },
        None => {
            let legacy = items.iter().rposition(is_user_summary);
            SemanticContext {
                previous_summary: legacy,
                task_context: None,
                work_boundary: legacy,
                selection_boundary: None,
            }
        }
    }
}

fn matching_semantic_summary(
    items: &[ResponseItemEnvelope],
    before_index: usize,
    semantic_hash: &str,
) -> Option<usize> {
    items[..before_index]
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, envelope)| {
            let text = checkpoint_summary_body(envelope).ok()?;
            (is_user_summary(envelope)
                && codex_history::archive_reference::content_sha256(text) == semantic_hash)
                .then_some(index)
        })
}

/// Build the model-visible history for the single V2 summary request.
///
/// Humans and protected items come from the native projection, so the summary sees exactly what
/// the replacement retains. The previous semantic summary is shown once; ordinary Work before the
/// work boundary is already represented by it and is omitted. Other summaries, including an
/// emergency placeholder, are not shown. Each verified observation pair becomes one bounded host
/// item.
#[cfg(test)]
pub(super) fn build_summary_history(
    originals: &[ResponseItemEnvelope],
    input: &CandidateInput,
    native: &NativeProjection,
    observations: &[(usize, usize, ObservationSummaryExcerpt)],
    previous_summary: Option<usize>,
    work_boundary: Option<usize>,
) -> Result<Vec<ResponseItemEnvelope>, String> {
    build_summary_history_with_task_context(
        originals,
        input,
        native,
        observations,
        previous_summary,
        work_boundary,
        None,
    )
}

pub(super) fn build_summary_history_with_task_context(
    originals: &[ResponseItemEnvelope],
    input: &CandidateInput,
    native: &NativeProjection,
    observations: &[(usize, usize, ObservationSummaryExcerpt)],
    previous_summary: Option<usize>,
    work_boundary: Option<usize>,
    task_context: Option<usize>,
) -> Result<Vec<ResponseItemEnvelope>, String> {
    if input.records.len() != originals.len() {
        return Err("summary history and candidate record counts differ".into());
    }
    if let Some(index) = previous_summary
        && originals
            .get(index)
            .is_none_or(|item| !is_user_summary(item))
    {
        return Err("previous summary index does not identify a compaction summary".into());
    }
    if let Some(index) = task_context
        && originals
            .get(index)
            .is_none_or(|item| !is_user_summary(item))
    {
        return Err("task context index does not identify a typed task summary".into());
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
        if previous_summary == Some(index) || task_context == Some(index) {
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
            && work_boundary.is_none_or(|boundary| index > boundary)
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
/// A marker may list more IDs directly after its string, as in `observation:"a", "b"`.
pub(super) fn important_refs_from_summary(
    summary_text: &str,
    inventory: &[ObservationReference],
) -> ImportantRefResolution {
    let mut resolution = ImportantRefResolution::default();
    let mut rest = summary_text;
    while let Some(position) = rest.find(IMPORTANT_REF_MARKER) {
        let literal = &rest[position + IMPORTANT_REF_MARKER.len() - 1..];
        let Some(end) = json_string_end(literal) else {
            resolution.ignored_markers += 1;
            break;
        };
        rest = &literal[end + 1..];
        let Ok(call_id) = serde_json::from_str::<String>(&literal[..=end]) else {
            resolution.ignored_markers += 1;
            continue;
        };
        resolution.record(&call_id, inventory);
        rest = resolution.record_listed_ids(rest, inventory);
    }
    resolution
}

impl ImportantRefResolution {
    fn record(&mut self, call_id: &str, inventory: &[ObservationReference]) {
        let mut matches = inventory
            .iter()
            .filter(|reference| reference.call_id == call_id);
        match (matches.next(), matches.next()) {
            (Some(reference), None) => {
                if !self.refs.contains(reference) {
                    self.refs.push(reference.clone());
                }
            }
            (None, _) | (Some(_), Some(_)) => self.ignored_markers += 1,
        }
    }

    /// Record `, "<call_id>"` items that directly follow a marker and return the text after them.
    fn record_listed_ids<'a>(
        &mut self,
        mut rest: &'a str,
        inventory: &[ObservationReference],
    ) -> &'a str {
        loop {
            let Some(item) = rest.trim_start().strip_prefix(',').map(str::trim_start) else {
                return rest;
            };
            let Some(end) = json_string_end(item) else {
                return rest;
            };
            let literal = &item[..=end];
            // An unclosed quote in prose would otherwise swallow the next marker's opening quote.
            if literal.contains(IMPORTANT_REF_MARKER) {
                return rest;
            }
            let Ok(call_id) = serde_json::from_str::<String>(literal) else {
                return rest;
            };
            self.record(&call_id, inventory);
            rest = &item[end + 1..];
        }
    }
}

/// Byte index of the closing quote of the JSON string that starts `text`.
fn json_string_end(text: &str) -> Option<usize> {
    if !text.starts_with('"') {
        return None;
    }
    let mut escaped = false;
    text.char_indices().skip(1).find_map(|(index, ch)| {
        let closes = !escaped && ch == '"';
        escaped = !escaped && ch == '\\';
        closes.then_some(index)
    })
}

/// Keep compaction-only server reasoning metadata out of the live context policy.
pub(crate) fn should_apply_server_reasoning_included(rencrow_compaction: bool) -> bool {
    !rencrow_compaction
}
