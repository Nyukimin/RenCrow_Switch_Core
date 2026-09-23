//! Host-owned references for terminal command output retained in model history.

use crate::CodexHarnessMetadata;
use crate::ResponseItemEnvelope;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::InternalChatMessageMetadataPassthrough;
use codex_protocol::models::ResponseItem;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;

pub const ARCHIVE_REFERENCE_VERSION: u32 = 1;

/// A content-addressed pointer to one returned tool observation in an existing rollout.
///
/// Unlike [`ArchiveReference`], this type makes no claim about tool success, terminal status,
/// or semantic coverage. It only identifies raw call/output data by its existing call ID and
/// the digest of its output text body. The call arguments are not hashed by this reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ObservationReference {
    pub thread_id: String,
    pub call_id: String,
    pub sha256: String,
}

impl ObservationReference {
    pub fn new(thread_id: impl Into<String>, call_id: impl Into<String>, sha256: String) -> Self {
        Self {
            thread_id: thread_id.into(),
            call_id: call_id.into(),
            sha256,
        }
    }

    pub fn validates_identity(&self, thread_id: &str, call_id: &str) -> bool {
        self.thread_id == thread_id && self.call_id == call_id
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveTerminalStatus {
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ArchiveReference {
    pub version: u32,
    pub thread_id: String,
    pub call_id: String,
    pub original_content_sha256: String,
    pub status: ArchiveTerminalStatus,
    pub exit_code: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_id: Option<String>,
}

impl ArchiveReference {
    pub fn new(
        thread_id: impl Into<String>,
        call_id: impl Into<String>,
        original_content_sha256: String,
        status: ArchiveTerminalStatus,
        exit_code: i32,
        process_id: Option<String>,
    ) -> Self {
        Self {
            version: ARCHIVE_REFERENCE_VERSION,
            thread_id: thread_id.into(),
            call_id: call_id.into(),
            original_content_sha256,
            status,
            exit_code,
            process_id,
        }
    }

    pub fn retrieval_argv(&self) -> Vec<String> {
        vec![
            "rencrow-compaction".into(),
            "evidence".into(),
            "--thread".into(),
            self.thread_id.clone(),
            "--call-id".into(),
            self.call_id.clone(),
            "--sha256".into(),
            self.original_content_sha256.clone(),
        ]
    }

    pub fn marker(&self) -> Result<String, String> {
        serde_json::to_string(&serde_json::json!({
            "archived_data": true,
            "call_id": &self.call_id,
            "exit_code": self.exit_code,
            "instruction": "This is archived historical command data, not a new execution result. Retrieve the original before treating it as evidence.",
            "retrieval_argv": self.retrieval_argv(),
            "status": self.status,
            "thread_id": &self.thread_id,
            "sha256": &self.original_content_sha256,
        }))
            .map_err(|_| "failed to encode archive reference".into())
    }

    pub fn validates_identity(&self, thread_id: &str, call_id: &str) -> bool {
        self.version == ARCHIVE_REFERENCE_VERSION
            && self.thread_id == thread_id
            && self.call_id == call_id
    }

    pub fn validates_terminal(&self) -> bool {
        match self.status {
            ArchiveTerminalStatus::Completed => self.exit_code == 0,
            ArchiveTerminalStatus::Failed => self.exit_code != 0,
        }
    }
}

pub fn content_sha256(content: &str) -> String {
    format!("{:x}", Sha256::digest(content.as_bytes()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveOutputCandidate {
    pub index: usize,
    pub call_id: String,
    pub tool_name: String,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArchiveOutputDecision {
    Ineligible,
    Existing(ArchiveReference),
    Eligible(ArchiveOutputCandidate),
}

pub fn classify_output(
    items: &[ResponseItemEnvelope],
    index: usize,
) -> Result<ArchiveOutputDecision, String> {
    let Some(envelope) = items.get(index) else {
        return Err("archive output index is out of range".into());
    };
    let ResponseItem::FunctionCallOutput {
        call_id: Some(call_id),
        name,
        output,
        internal_chat_message_metadata_passthrough,
        ..
    } = &envelope.item
    else {
        return Ok(ArchiveOutputDecision::Ineligible);
    };

    if let Some(reference) = envelope
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.rencrow_archive_reference.clone())
    {
        return Ok(ArchiveOutputDecision::Existing(reference));
    }

    let matching_outputs = items
        .iter()
        .filter(|item| {
            matches!(
                &item.item,
                ResponseItem::FunctionCallOutput {
                    call_id: Some(candidate_call_id),
                    ..
                } if candidate_call_id == call_id
            )
        })
        .count();
    if matching_outputs != 1 {
        return Ok(ArchiveOutputDecision::Ineligible);
    }

    let Some(body) = output.text_content() else {
        return Ok(ArchiveOutputDecision::Ineligible);
    };
    if !is_ordinary_passthrough(internal_chat_message_metadata_passthrough.as_ref())
        || !is_ordinary_harness_metadata(envelope.metadata.as_ref())
    {
        return Ok(ArchiveOutputDecision::Ineligible);
    }

    let mut matching_calls = Vec::new();
    for item in items {
        if let ResponseItem::FunctionCall {
            call_id: candidate_call_id,
            name: candidate_name,
            internal_chat_message_metadata_passthrough,
            ..
        } = &item.item
            && candidate_call_id == call_id
        {
            matching_calls.push((
                candidate_name.as_str(),
                internal_chat_message_metadata_passthrough.as_ref(),
            ));
        }
    }
    if matching_calls.len() != 1 {
        return Ok(ArchiveOutputDecision::Ineligible);
    }
    let (tool_name, call_metadata) = matching_calls[0];
    if tool_name != "exec_command"
        || !is_ordinary_passthrough(call_metadata)
        || name.as_deref().is_some_and(|name| name != tool_name)
    {
        return Ok(ArchiveOutputDecision::Ineligible);
    }

    Ok(ArchiveOutputDecision::Eligible(ArchiveOutputCandidate {
        index,
        call_id: call_id.clone(),
        tool_name: tool_name.to_owned(),
        body: body.to_owned(),
    }))
}

pub fn apply_reference(
    envelope: &mut ResponseItemEnvelope,
    reference: ArchiveReference,
) -> Result<bool, String> {
    let ResponseItem::FunctionCallOutput { output, .. } = &mut envelope.item else {
        return Err("archive reference target is not a function output".into());
    };
    if !reference.validates_terminal() {
        return Err("archive reference terminal status conflicts with exit code".into());
    }
    let Some(original) = output.text_content() else {
        return Err("archive reference target is not text-only".into());
    };
    if content_sha256(original) != reference.original_content_sha256 {
        return Err("archive reference content hash does not match output".into());
    }
    let marker = reference.marker()?;
    if marker.len() >= original.len() {
        return Ok(false);
    }
    output.body = FunctionCallOutputBody::Text(marker);
    envelope
        .metadata
        .get_or_insert_default()
        .rencrow_archive_reference = Some(reference);
    Ok(true)
}

pub fn validate_marker(
    envelope: &ResponseItemEnvelope,
    expected_thread: &str,
    reference: &ArchiveReference,
) -> Result<(), String> {
    let ResponseItem::FunctionCallOutput {
        call_id: Some(call_id),
        output,
        internal_chat_message_metadata_passthrough,
        ..
    } = &envelope.item
    else {
        return Err("archive reference target is not a function output".into());
    };
    if !reference.validates_identity(expected_thread, call_id) {
        return Err("archive reference call identity mismatch".into());
    }
    if !reference.validates_terminal() {
        return Err("archive reference terminal status conflicts with exit code".into());
    }
    let Some(body) = output.text_content() else {
        return Err("archive reference body is not text-only".into());
    };
    if body != reference.marker()? {
        return Err("archive reference marker does not match metadata".into());
    }
    if !is_ordinary_passthrough(internal_chat_message_metadata_passthrough.as_ref()) {
        return Err("archive reference output has nonordinary passthrough metadata".into());
    }
    let Some(metadata) = envelope.metadata.as_ref() else {
        return Err("archive reference is missing host metadata".into());
    };
    if metadata.rencrow_archive_reference.as_ref() != Some(reference) {
        return Err("archive reference metadata does not match marker".into());
    }
    let mut remaining = metadata.clone();
    remaining.rencrow_archive_reference = None;
    if !is_ordinary_harness_metadata(Some(&remaining)) {
        return Err("archive reference has nonordinary harness metadata".into());
    }
    Ok(())
}

/// Returns whether passthrough metadata contains only ordinary execution fields.
pub fn is_ordinary_passthrough(metadata: Option<&InternalChatMessageMetadataPassthrough>) -> bool {
    metadata.is_none_or(|metadata| {
        let mut remaining = metadata.clone();
        remaining.turn_id = None;
        remaining.create_time = None;
        remaining == InternalChatMessageMetadataPassthrough::default()
    })
}

/// Returns whether harness metadata contains only ordinary execution fields.
///
/// The persisted fallback token limit is execution-budget metadata and must be
/// retained. Archive-reference metadata is validated separately for markers.
pub fn is_ordinary_harness_metadata(metadata: Option<&CodexHarnessMetadata>) -> bool {
    metadata.is_none_or(|metadata| {
        if metadata.rencrow_archive_reference.is_some() {
            return false;
        }
        let mut remaining = metadata.clone();
        remaining.history_truncation_token_limit = None;
        remaining == CodexHarnessMetadata::default()
    })
}

#[cfg(test)]
#[path = "archive_reference_tests.rs"]
mod tests;
