//! Native-history projection for the validated instruction-removal map.

use super::super::CompactedMessageIdentity;
use super::super::CompactedUserMessage;
use super::super::build_compacted_history_with_limit;
use super::super::collect_annotated_user_messages;
use super::super::insert_initial_context_before_last_real_user_or_summary;
use super::super::is_summary_message;
use codex_history::ResponseItemEnvelope;
use codex_history::compaction_candidate::CandidateInput;
use codex_history::compaction_candidate::Origin;
use codex_history::compaction_candidate::digest;
use codex_history::compaction_plan::ByteRange;
use codex_history::compaction_preprocess::prune_known_obsolete;
use codex_history::compaction_selection::InstructionSelectionApplication;
use codex_protocol::items::TurnItem;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::ResponseItem;
use codex_utils_output_truncation::approx_token_count;
use std::collections::HashMap;
use std::ops::Range;

struct RetainedItem {
    envelope: ResponseItemEnvelope,
}

/// One checked native projection shared by summary input and retained history construction.
pub(super) struct NativeProjection {
    retained: Vec<RetainedItem>,
    user_messages: Vec<CompactedUserMessage>,
    human_positions: Vec<usize>,
}

impl NativeProjection {
    /// Borrow the exact Human envelopes projected into both summary and retained history.
    pub(super) fn summary_human_messages(&self) -> impl Iterator<Item = &ResponseItemEnvelope> {
        self.human_positions
            .iter()
            .map(|index| &self.retained[*index].envelope)
    }

    pub(super) fn retained_native_items(
        &self,
    ) -> impl Iterator<Item = &ResponseItemEnvelope> + DoubleEndedIterator + ExactSizeIterator + '_
    {
        self.retained.iter().map(|item| &item.envelope)
    }

    /// Borrow retained native items with canonical initial context inserted at the host slot.
    pub(super) fn retained_native_items_with_initial_context<'a>(
        &'a self,
        initial_context: &'a [ResponseItemEnvelope],
        summary: &'a ResponseItemEnvelope,
    ) -> impl Iterator<Item = &'a ResponseItemEnvelope> + DoubleEndedIterator + 'a {
        let insertion_index =
            host_insertion_index(self.retained_native_items().chain(std::iter::once(summary)))
                .unwrap_or(self.retained.len())
                .min(self.retained.len());

        self.retained_native_items()
            .take(insertion_index)
            .chain(initial_context.iter())
            .chain(self.retained_native_items().skip(insertion_index))
            .chain(std::iter::once(summary))
    }
}

/// Apply the accepted map to the original native envelopes without copying unrelated Work.
pub(super) fn filter_retained_instructions(
    originals: &[ResponseItemEnvelope],
    input: &CandidateInput,
    application: InstructionSelectionApplication,
) -> Result<NativeProjection, String> {
    let snapshot = input.snapshot()?;
    if input.records.len() != originals.len() {
        return Err("native history and candidate record counts differ".into());
    }

    for (index, (record, envelope)) in input.records.iter().zip(originals).enumerate() {
        let expected_id = envelope
            .item
            .id()
            .map(|id| id.as_str().to_owned())
            .unwrap_or_else(|| format!("item-{index}"));
        if record.id != expected_id {
            return Err(format!(
                "native identity mapping mismatch at candidate index {index}"
            ));
        }
    }

    let checked = prune_known_obsolete(input, &application.pruning.applied)?;
    if checked != application.pruning {
        return Err("accepted native exclusion map does not match its source references".into());
    }

    let records_by_id = input
        .records
        .iter()
        .enumerate()
        .map(|(index, record)| (record.id.as_str(), (index, record)))
        .collect::<HashMap<_, _>>();
    let mut removals = HashMap::<String, Vec<Range<usize>>>::new();
    for reference in &application.pruning.applied {
        let Some((index, record)) = records_by_id.get(reference.id.as_str()).copied() else {
            return Err("native exclusion references an absent source".into());
        };
        if originals[index]
            .item
            .id()
            .is_none_or(|native_id| native_id.as_str() != reference.id)
        {
            return Err("native exclusion targets an item without a stable native identity".into());
        }
        if record.origin != Origin::Human
            || record.role != "user"
            || record.opaque.is_some()
            || !matches!(&originals[index].item, ResponseItem::Message { role, .. } if role == "user")
        {
            return Err("native exclusion does not target a plain Human user message".into());
        }
        let current = snapshot
            .reference(&record.id, reference.range.clone())
            .map_err(|error| format!("invalid native exclusion reference: {error:?}"))?;
        if current != *reference {
            return Err("native exclusion source changed after selection".into());
        }
        if record
            .protected
            .iter()
            .any(|protected| byte_ranges_overlap(protected, &reference.range))
        {
            return Err("native exclusion intersects protected Human text".into());
        }
        removals
            .entry(record.id.clone())
            .or_default()
            .push(reference.range.start..reference.range.end);
    }
    for ranges in removals.values_mut() {
        ranges.sort_by_key(|range| (range.start, range.end));
        if ranges
            .windows(2)
            .any(|pair| ranges_overlap(&pair[0], &pair[1]))
        {
            return Err("native exclusion ranges overlap or repeat".into());
        }
    }

    let selection_hash = match application.plan_hash {
        Some(plan_hash) => plan_hash,
        None => digest(&application.pruning.applied)?,
    };
    let mut retained = Vec::new();
    let mut human_positions = Vec::new();
    for (record, original) in input.records.iter().zip(originals) {
        if is_user_summary(original) && record.origin != Origin::Human {
            continue;
        }
        let envelope = match record.origin {
            Origin::Human => {
                let mut envelope = original.clone();
                let ResponseItem::Message { role, content, .. } = &mut envelope.item else {
                    return Err("Human candidate no longer maps to a native message".into());
                };
                if role != "user" {
                    return Err("Human candidate no longer has user role".into());
                }
                let original_text = content_text(content);
                if original_text != record.text {
                    return Err("native Human text differs from its captured snapshot".into());
                }
                let ranges = removals
                    .get(&record.id)
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                if !ranges.is_empty() {
                    let intake = envelope
                        .metadata
                        .as_mut()
                        .and_then(|metadata| metadata.rencrow_input.as_mut())
                        .ok_or_else(|| {
                            "selected Human source lost its host intake record".to_owned()
                        })?;
                    if intake["selected_text"].as_str() != Some(record.text.as_str()) {
                        return Err(
                            "native Human intake text differs from its captured snapshot".into(),
                        );
                    }
                    apply_ranges_to_content(content, ranges)?;
                    let retained_text = content_text(content);
                    if application.pruning.retained_human_text.get(&record.id)
                        != Some(&retained_text)
                    {
                        return Err(
                            "native Human projection differs from accepted exclusion map".into(),
                        );
                    }
                    intake["selected_text"] = serde_json::json!(retained_text);
                    intake["selection_hash"] = serde_json::json!(selection_hash);
                    if !record.text.is_empty()
                        && retained_text.is_empty()
                        && record.opaque.is_none()
                    {
                        continue;
                    }
                }
                envelope
            }
            Origin::Work | Origin::Host
                if record.opaque.is_none() && record.protected.is_empty() =>
            {
                continue;
            }
            Origin::Work | Origin::Host | Origin::Unknown => original.clone(),
        };
        let retained_index = retained.len();
        if record.origin == Origin::Human {
            human_positions.push(retained_index);
        }
        retained.push(RetainedItem { envelope });
    }

    let user_messages = collect_user_slots(&retained)?;
    Ok(NativeProjection {
        retained,
        user_messages,
        human_positions,
    })
}

/// Reuse the original builder and restore native user envelopes after validating its slots.
pub(super) fn build_native_replacement(
    projection: &NativeProjection,
    summary_text: &str,
    initial_context: Vec<ResponseItemEnvelope>,
) -> Result<Vec<ResponseItemEnvelope>, String> {
    let token_budget = projection
        .user_messages
        .iter()
        .try_fold(0usize, |total, message| {
            total
                .checked_add(approx_token_count(&message.message))
                .ok_or_else(|| "native retained user token budget overflowed".to_owned())
        })?
        .checked_add(1)
        .ok_or_else(|| "native retained user token budget overflowed".to_owned())?;
    let mut built = build_compacted_history_with_limit(
        Vec::new(),
        &projection.user_messages,
        summary_text,
        token_budget,
    );
    let summary = built
        .pop()
        .ok_or_else(|| "original history builder omitted its summary item".to_owned())?;
    if built.len() != projection.user_messages.len() {
        return Err("original history builder changed the retained user slot count".into());
    }
    for (built_item, expected) in built.iter().zip(&projection.user_messages) {
        let ResponseItem::Message {
            id,
            role,
            content,
            internal_chat_message_metadata_passthrough,
            ..
        } = &built_item.item
        else {
            return Err("original history builder changed a retained user item kind".into());
        };
        let mut expected_passthrough = expected.internal_chat_message_metadata_passthrough.clone();
        if let Some(metadata) = expected_passthrough.as_mut()
            && metadata.content_item_kinds.is_some()
        {
            metadata.content_item_kinds = Some(vec![ContentItemKind("user.text".into())]);
        }
        if role != "user"
            || id != &expected.id
            || content_text(content) != expected.message
            || built_item.metadata != expected.harness_metadata
            || internal_chat_message_metadata_passthrough != &expected_passthrough
        {
            return Err("original history builder changed retained user identity or text".into());
        }
    }

    let mut history = projection
        .retained
        .iter()
        .map(|item| item.envelope.clone())
        .collect::<Vec<_>>();
    history.push(summary);

    let history_len = history.len();
    let helper_index = helper_insertion_index(&history);
    let target_index = host_insertion_index(history.iter());
    let with_context =
        insert_initial_context_before_last_real_user_or_summary(history, initial_context.clone());
    if initial_context.is_empty() || helper_index == target_index {
        return Ok(with_context);
    }

    let from = helper_index.unwrap_or(history_len);
    let to = from + initial_context.len();
    if with_context.get(from..to) != Some(initial_context.as_slice()) {
        return Err(
            "initial context placement from the original helper was not identifiable".into(),
        );
    }
    let mut corrected = with_context;
    corrected.drain(from..to);
    let insertion_index = target_index.unwrap_or(corrected.len());
    corrected.splice(insertion_index..insertion_index, initial_context);
    Ok(corrected)
}

fn collect_user_slots(retained: &[RetainedItem]) -> Result<Vec<CompactedUserMessage>, String> {
    let user_items = retained
        .iter()
        .filter(|item| matches!(&item.envelope.item, ResponseItem::Message { role, .. } if role == "user"))
        .collect::<Vec<_>>();
    let stable_user_items = user_items
        .iter()
        .filter(|item| item.envelope.item.id().is_some())
        .map(|item| item.envelope.clone())
        .collect::<Vec<_>>();
    let mut collected =
        collect_annotated_user_messages(&stable_user_items, CompactedMessageIdentity::Preserve)
            .into_iter()
            .filter_map(|message| message.id.clone().map(|id| (id, message)))
            .collect::<HashMap<_, _>>();

    let mut user_messages = Vec::with_capacity(user_items.len());
    for item in user_items {
        let envelope = &item.envelope;
        let ResponseItem::Message {
            id,
            content,
            internal_chat_message_metadata_passthrough,
            ..
        } = &envelope.item
        else {
            unreachable!();
        };
        let message = if let Some(id) = id.as_ref() {
            collected
                .remove(id)
                .unwrap_or_else(|| CompactedUserMessage {
                    id: Some(id.clone()),
                    message: content_text(content),
                    internal_chat_message_metadata_passthrough:
                        internal_chat_message_metadata_passthrough.clone(),
                    harness_metadata: envelope.metadata.clone(),
                })
        } else {
            CompactedUserMessage {
                id: None,
                message: content_text(content),
                internal_chat_message_metadata_passthrough:
                    internal_chat_message_metadata_passthrough.clone(),
                harness_metadata: envelope.metadata.clone(),
            }
        };
        if message.message != content_text(content)
            || message.internal_chat_message_metadata_passthrough
                != *internal_chat_message_metadata_passthrough
            || message.harness_metadata != envelope.metadata
        {
            return Err("collector input does not match the native user item".into());
        }
        user_messages.push(message);
    }
    if !collected.is_empty() {
        return Err("original collector returned an unmatched native user item".into());
    }
    Ok(user_messages)
}

fn content_text(content: &[ContentItem]) -> String {
    content
        .iter()
        .filter_map(|item| match item {
            ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                Some(text.as_str())
            }
            ContentItem::InputImage { .. } | ContentItem::InputAudio { .. } => None,
        })
        .collect()
}

fn apply_ranges_to_content(
    content: &mut [ContentItem],
    ranges: &[Range<usize>],
) -> Result<(), String> {
    let mut offsets = Vec::with_capacity(content.len());
    let mut full_text = String::new();
    for item in content.iter() {
        let start = full_text.len();
        match item {
            ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                full_text.push_str(text)
            }
            ContentItem::InputImage { .. } | ContentItem::InputAudio { .. } => {}
        }
        offsets.push(start..full_text.len());
    }
    let mut local_ranges = vec![Vec::<Range<usize>>::new(); content.len()];
    for range in ranges {
        if range.start >= range.end
            || range.end > full_text.len()
            || !full_text.is_char_boundary(range.start)
            || !full_text.is_char_boundary(range.end)
        {
            return Err("accepted native removal range is outside the Human text".into());
        }
        for (index, span) in offsets.iter().enumerate() {
            let start = range.start.max(span.start);
            let end = range.end.min(span.end);
            if start < end {
                local_ranges[index].push((start - span.start)..(end - span.start));
            }
        }
    }
    for (item, ranges) in content.iter_mut().zip(local_ranges) {
        let text = match item {
            ContentItem::InputText { text } | ContentItem::OutputText { text } => text,
            ContentItem::InputImage { .. } | ContentItem::InputAudio { .. } => continue,
        };
        for range in ranges.into_iter().rev() {
            text.replace_range(range, "");
        }
    }
    Ok(())
}

fn byte_ranges_overlap(left: &ByteRange, right: &ByteRange) -> bool {
    left.start < right.end && right.start < left.end
}

fn ranges_overlap(left: &Range<usize>, right: &Range<usize>) -> bool {
    left.start < right.end && right.start < left.end
}

fn is_user_summary(envelope: &ResponseItemEnvelope) -> bool {
    matches!(&envelope.item, ResponseItem::Message { role, .. } if role == "user")
        && is_provenance_summary(envelope)
}

fn is_provenance_summary(envelope: &ResponseItemEnvelope) -> bool {
    if envelope
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.rencrow_input.as_ref())
        .and_then(|intake| intake.get("author"))
        .and_then(serde_json::Value::as_str)
        == Some("human")
    {
        return false;
    }
    if envelope
        .metadata
        .as_ref()
        .is_some_and(|metadata| metadata.rencrow_compaction.is_some())
    {
        return true;
    }
    let ResponseItem::Message {
        content,
        internal_chat_message_metadata_passthrough: Some(metadata),
        ..
    } = &envelope.item
    else {
        return false;
    };
    metadata.content_item_kinds.as_ref().is_some_and(|kinds| {
        !kinds.is_empty()
            && kinds.len() == content.len()
            && kinds.iter().all(|kind| kind.0 == "compaction.summary")
    })
}

fn helper_insertion_index(history: &[ResponseItemEnvelope]) -> Option<usize> {
    let mut last_user_or_summary = None;
    for (index, envelope) in history.iter().enumerate().rev() {
        if let ResponseItem::AgentMessage { content, .. } = &envelope.item
            && !matches!(content.first(), Some(AgentMessageInputContent::InputText { text }) if text.starts_with("Message Type: FINAL_ANSWER\n"))
        {
            return Some(index);
        }
        let Some(TurnItem::UserMessage(user)) =
            crate::event_mapping::parse_turn_item(&envelope.item)
        else {
            continue;
        };
        last_user_or_summary.get_or_insert(index);
        if !is_summary_message(&user.message()) {
            return Some(index);
        }
    }
    last_user_or_summary.or_else(|| {
        history
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, envelope)| {
                matches!(
                    &envelope.item,
                    ResponseItem::Compaction { .. } | ResponseItem::ContextCompaction { .. }
                )
                .then_some(index)
            })
    })
}

fn host_insertion_index<'a, I>(mut history: I) -> Option<usize>
where
    I: Iterator<Item = &'a ResponseItemEnvelope> + DoubleEndedIterator,
{
    let (exact_len, upper_bound) = history.size_hint();
    if upper_bound != Some(exact_len) {
        return None;
    }
    let mut last_summary = None;
    let mut last_compaction = None;
    let mut index = exact_len;
    while let Some(envelope) = history.next_back() {
        index = index.saturating_sub(1);
        if let ResponseItem::AgentMessage { content, .. } = &envelope.item
            && !matches!(content.first(), Some(AgentMessageInputContent::InputText { text }) if text.starts_with("Message Type: FINAL_ANSWER\n"))
        {
            return Some(index);
        }
        if matches!(&envelope.item, ResponseItem::Message { role, .. } if role == "user") {
            if is_provenance_summary(envelope) {
                last_summary.get_or_insert(index);
            } else {
                // The input is a real user-role item even when its text resembles a summary.
                return Some(index);
            }
        }
        if matches!(
            &envelope.item,
            ResponseItem::Compaction { .. } | ResponseItem::ContextCompaction { .. }
        ) {
            last_compaction.get_or_insert(index);
        }
    }
    last_summary.or(last_compaction)
}

#[cfg(test)]
#[path = "compact_rencrow_native_tests.rs"]
mod tests;
