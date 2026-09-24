// Modified by RenCrow Switch Core, 2026-09-22.
//! Adapt owner-reconstructed history without guessing human provenance.
use codex_history::CodexHarnessMetadata;
use codex_history::ResponseItemEnvelope;
use codex_history::compaction_candidate::CandidateBundle;
use codex_history::compaction_candidate::CandidateInput;
use codex_history::compaction_candidate::CandidateRecord;
use codex_history::compaction_candidate::Origin;
use codex_history::compaction_candidate::digest;
use codex_history::observation_projection::OBSERVATION_PART_FULL_LIMIT_BYTES;
use codex_protocol::models::ContentItem;
use codex_protocol::models::InternalChatMessageMetadataPassthrough;
use codex_protocol::models::ReasoningItemContent;
use codex_protocol::models::ReasoningItemReasoningSummary;
use codex_protocol::models::ResponseItem;
use codex_rollout::PreparedCompactionSources;
use serde_json::Value;
use serde_json::json;
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CompletedWorkProjection {
    pub call_index: usize,
    pub output_index: usize,
    pub call_text: String,
    pub output_text: String,
}

/// Build capture pairs from host-verified prepared sources for V2 compaction.
///
/// CandidateInput keeps full text only when both call and output fit the completion-link bound;
/// larger raw bodies stay in the rollout and reach the summary through bounded observations.
#[allow(dead_code)]
pub(super) fn verified_pair_projections(
    prepared: &PreparedCompactionSources<'_>,
) -> Vec<CompletedWorkProjection> {
    prepared
        .pairs
        .iter()
        .map(|pair| {
            let (call_text, output_text) =
                match (pair.canonical_call_input, pair.canonical_output_text) {
                    (Some(call), Some(output))
                        if call.len() <= OBSERVATION_PART_FULL_LIMIT_BYTES
                            && output.len() <= OBSERVATION_PART_FULL_LIMIT_BYTES =>
                    {
                        (call.to_owned(), output.to_owned())
                    }
                    _ => (String::new(), String::new()),
                };
            CompletedWorkProjection {
                call_index: pair.call_index,
                output_index: pair.output_index,
                call_text,
                output_text,
            }
        })
        .collect()
}

pub(super) fn capture(
    items: &[ResponseItemEnvelope],
    binding: String,
    thread_id: &str,
    current_context: Vec<Value>,
    completed_work: &[CompletedWorkProjection],
) -> Result<CandidateInput, String> {
    let mut completed_work_records = HashMap::new();
    for pair in completed_work {
        if pair.call_index == pair.output_index {
            return Err("completed work call and output indexes overlap".into());
        }
        let (Some(call), Some(output)) = (items.get(pair.call_index), items.get(pair.output_index))
        else {
            return Err("completed work index is out of range".into());
        };
        match (&call.item, &output.item) {
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
            ) if call_id == output_call_id => {}
            _ => {
                return Err(
                    "completed work indexes are not one matching tool call and output".into(),
                );
            }
        }
        if completed_work_records
            .insert(pair.call_index, (pair.call_text.as_str(), true))
            .is_some()
            || completed_work_records
                .insert(pair.output_index, (pair.output_text.as_str(), false))
                .is_some()
        {
            return Err("completed work history index is duplicated".into());
        }
    }

    let mut records = Vec::new();
    let mut prior_invalidations = Vec::new();
    for (index, envelope) in items.iter().enumerate() {
        if let Some(value) = envelope
            .metadata
            .as_ref()
            .and_then(|m| m.rencrow_compaction.as_ref())
            .and_then(|m| m.get("invalidated_instructions"))
        {
            let passages: Vec<String> = serde_json::from_value(value.clone())
                .map_err(|_| "invalid checkpoint invalidations".to_owned())?;
            prior_invalidations.extend(passages);
        }
        let mut record = CandidateRecord {
            id: envelope
                .item
                .id()
                .map(|id| id.as_str().to_owned())
                .unwrap_or_else(|| format!("item-{index}")),
            origin: Origin::Unknown,
            intake_ref: None,
            scope: "thread".into(),
            role: "unknown".into(),
            text: String::new(),
            protected: vec![],
            execution_evidence: false,
            opaque: Some(json!({"item":envelope.item,"metadata":envelope.metadata})),
        };
        if let ResponseItem::Message {
            role,
            content,
            internal_chat_message_metadata_passthrough,
            ..
        } = &envelope.item
        {
            record.role = role.clone();
            record.text = content
                .iter()
                .filter_map(|item| match item {
                    ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                        Some(text.as_str())
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            let text_only = content.iter().all(|item| {
                matches!(
                    item,
                    ContentItem::InputText { .. } | ContentItem::OutputText { .. }
                )
            });
            let metadata = envelope.metadata.as_ref();
            let kinds = internal_chat_message_metadata_passthrough
                .as_ref()
                .and_then(|m| m.content_item_kinds.as_ref());
            let host = kinds.is_some_and(|kinds| {
                !kinds.is_empty()
                    && kinds.len() == content.len()
                    && kinds.iter().all(|kind| {
                        matches!(
                            kind.0.as_str(),
                            "agents_md.instructions"
                                | "environments.environment_context"
                                | "host_skills.instructions"
                                | "permissions.instructions"
                                | "multi_agent.usage_hint"
                                | "generic.developer_instructions"
                                | "managed_config.developer_instructions"
                                | "collaboration_mode.instructions"
                                | "persistent_mode.instructions"
                                | "multi_agent.mode_instructions"
                                | "apps.instructions"
                        )
                    })
            });
            let summary = role == "user"
                && (metadata.is_some_and(|m| m.rencrow_compaction.is_some())
                    || kinds.is_some_and(|kinds| {
                        !kinds.is_empty()
                            && kinds.len() == content.len()
                            && kinds.iter().all(|kind| kind.0 == "compaction.summary")
                    }));
            let intake = metadata.and_then(|m| m.rencrow_input.as_ref());
            if role == "user"
                && metadata
                    .is_none_or(|m| !m.inherited_user_message && m.sender_user_messages.is_none())
                && let Some(intake) = intake
                && intake["author"] == "human"
                && intake["thread_id"].as_str() == Some(thread_id)
                && intake["selected_text"].as_str() == Some(record.text.as_str())
                && intake["receipt_hash"].as_str().is_some()
            {
                record.origin = Origin::Human;
                record.intake_ref = intake["receipt_hash"].as_str().map(str::to_owned);
                if text_only {
                    record.opaque = None;
                }
            } else if (role == "assistant" || summary) && text_only {
                record.origin = Origin::Work;
                record.opaque = None;
            } else if host && metadata.is_none_or(|m| !m.client_authored) {
                record.origin = Origin::Host;
                record.text.clear();
                record.opaque = None;
            }
        } else if let ResponseItem::Reasoning {
            summary,
            content,
            encrypted_content,
            internal_chat_message_metadata_passthrough,
            ..
        } = &envelope.item
        {
            let mut text = String::new();
            for item in summary {
                match item {
                    ReasoningItemReasoningSummary::SummaryText { text: part } => {
                        text.push_str(part)
                    }
                }
            }
            if let Some(content) = content {
                for item in content {
                    match item {
                        ReasoningItemContent::ReasoningText { text: part }
                        | ReasoningItemContent::Text { text: part } => text.push_str(part),
                    }
                }
            }
            let ordinary_passthrough = internal_chat_message_metadata_passthrough
                .as_ref()
                .is_none_or(|metadata| {
                    let mut remaining = metadata.clone();
                    remaining.turn_id = None;
                    remaining.create_time = None;
                    // Compare the typed remainder with Default so newly added fields fail closed.
                    remaining == InternalChatMessageMetadataPassthrough::default()
                });
            let ordinary_harness_metadata = envelope
                .metadata
                .as_ref()
                .is_none_or(|metadata| metadata == &CodexHarnessMetadata::default());
            if encrypted_content.is_none()
                && !text.is_empty()
                && ordinary_passthrough
                && ordinary_harness_metadata
            {
                record.origin = Origin::Work;
                record.role = "assistant".into();
                record.text = text;
                record.opaque = None;
            }
        } else if let ResponseItem::FunctionCallOutput { output, .. }
        | ResponseItem::CustomToolCallOutput { output, .. } = &envelope.item
        {
            // Preserve the original structured envelope, including media. Only its
            // textual evidence goes to semantic review; binary media is not JSON text.
            record.origin = Origin::Work;
            record.execution_evidence = envelope
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.rencrow_archive_reference.as_ref())
                .is_none();
            record.text = output.body.to_text().unwrap_or_default();
        }
        if let Some((text, is_call)) = completed_work_records.get(&index) {
            record.origin = Origin::Work;
            record.role = "assistant".into();
            record.text = (*text).to_owned();
            record.opaque = None;
            if *is_call {
                record.execution_evidence = false;
            }
        }
        records.push(record);
    }
    let input = CandidateInput {
        version: 1,
        binding,
        records,
        current_context,
        prior_invalidations,
    };
    input.snapshot()?;
    Ok(input)
}

pub(super) fn replacement(
    input: &CandidateInput,
    bundle: &CandidateBundle,
    originals: &[ResponseItemEnvelope],
) -> Result<Vec<ResponseItemEnvelope>, String> {
    input.assemble(bundle)?;
    if input.records.len() != originals.len() {
        return Err("history mapping mismatch".into());
    }
    let view = input.view(&bundle.plan, &bundle.plan_review)?;
    let mut output = Vec::new();
    for ((record, selected), original) in input
        .records
        .iter()
        .zip(view.retained.iter())
        .zip(originals)
    {
        match record.origin {
            Origin::Human => {
                if selected.text.is_empty() && record.opaque.is_none() {
                    continue;
                }
                let mut envelope = original.clone();
                if selected.text != record.text {
                    let ResponseItem::Message { content, .. } = &mut envelope.item else {
                        return Err("human item changed type".into());
                    };
                    // Preserve item count, annotations and identity. This is original text
                    // selection, never a generated paraphrase presented as user testimony.
                    let mut remaining = selected.text.clone();
                    for item in content {
                        match item {
                            ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                                *text = std::mem::take(&mut remaining)
                            }
                            _ => return Err("attachment-bearing input cannot be rewritten".into()),
                        }
                    }
                    if let Some(intake) = envelope
                        .metadata
                        .get_or_insert_default()
                        .rencrow_input
                        .as_mut()
                    {
                        intake["selected_text"] = json!(selected.text);
                        intake["selection_hash"] = json!(digest(&bundle.plan)?);
                    }
                }
                output.push(envelope);
            }
            Origin::Unknown => output.push(original.clone()),
            Origin::Work | Origin::Host
                if record.opaque.is_some() || !record.protected.is_empty() =>
            {
                output.push(original.clone())
            }
            Origin::Work | Origin::Host => {}
        }
    }
    Ok(output)
}

#[cfg(test)]
#[path = "compact_rencrow_history_tests.rs"]
mod tests;
