//! Borrowed rollout observation indexing and exact response-item matching.

use super::ArchiveEvidenceError;
use super::ArchiveEvidenceIneligibility;
use super::MAX_ARCHIVE_EVIDENCE_BYTES;
use codex_history::ArchiveReference;
use codex_history::ArchiveTerminalStatus;
use codex_history::ObservationReference;
use codex_history::ResponseItemEnvelope;
use codex_history::archive_reference::content_sha256;
use codex_history::archive_reference::is_ordinary_harness_metadata;
use codex_history::archive_reference::is_ordinary_passthrough;
use codex_history::observation_projection::OBSERVATION_PART_FULL_LIMIT_BYTES;
use codex_protocol::ThreadId;
use codex_protocol::items::CommandExecutionStatus;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolCallKind {
    Function,
    Custom,
}

#[derive(Debug, Clone, Copy)]
struct IndexedCall<'a> {
    index: usize,
    envelope: &'a ResponseItemEnvelope,
    kind: ToolCallKind,
    name: &'a str,
    namespace: Option<&'a str>,
    status: Option<&'a str>,
}

#[derive(Debug, Clone, Copy)]
struct IndexedOutput<'a> {
    index: usize,
    envelope: &'a ResponseItemEnvelope,
    kind: ToolCallKind,
    name: Option<&'a str>,
    namespace: Option<&'a str>,
}

#[derive(Debug, Default)]
struct IndexedObservation<'a> {
    calls: Vec<IndexedCall<'a>>,
    outputs: Vec<IndexedOutput<'a>>,
    terminals: Vec<&'a codex_protocol::items::CommandExecutionItem>,
}

/// Borrowed call/output index for batch projection and rollout retrieval.
///
/// Construction visits the rollout once and never copies response bodies. Callers can retain
/// the index while validating multiple references or retrieving raw call/output envelopes.
/// `ObservationReference.sha256` hashes only the output text, not call arguments.
pub struct ObservationIndex<'a> {
    by_call_id: HashMap<String, IndexedObservation<'a>>,
    thread_id: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct IndexedObservationRef<'a> {
    pub call_index: usize,
    pub output_index: usize,
    pub call: &'a ResponseItemEnvelope,
    pub output: &'a ResponseItemEnvelope,
    pub tool_name: &'a str,
    pub body: &'a str,
    kind: ToolCallKind,
    terminal: Option<&'a codex_protocol::items::CommandExecutionItem>,
}

/// Which original text part to retrieve from a referenced observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationPart {
    Call,
    Output,
}

impl FromStr for ObservationPart {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "call" => Ok(Self::Call),
            "output" => Ok(Self::Output),
            _ => Err("part must be call or output"),
        }
    }
}

impl fmt::Display for ObservationPart {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Call => "call",
            Self::Output => "output",
        })
    }
}

/// A verified, bounded half-open byte range from one original text part.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ObservationRange {
    pub reference: ObservationReference,
    pub part: ObservationPart,
    pub start: usize,
    pub end: usize,
    pub total_bytes: usize,
    pub part_sha256: String,
    pub text: String,
}

impl<'a> ObservationIndex<'a> {
    pub fn new(items: &'a [codex_history::RolloutItem], expected_thread: &ThreadId) -> Self {
        let mut index = Self {
            by_call_id: HashMap::new(),
            thread_id: Some(expected_thread.to_string()),
        };
        for (item_index, item) in items.iter().enumerate() {
            match item {
                codex_history::RolloutItem::ResponseItem(envelope) => {
                    index.insert_envelope(item_index, envelope);
                }
                codex_history::RolloutItem::EventMsg(EventMsg::ItemCompleted(event))
                    if event.thread_id == *expected_thread =>
                {
                    if let TurnItem::CommandExecution(command) = &event.item {
                        index
                            .by_call_id
                            .entry(command.id.clone())
                            .or_default()
                            .terminals
                            .push(command);
                    }
                }
                _ => {}
            }
        }
        index
    }

    pub(super) fn from_envelopes(items: &'a [ResponseItemEnvelope]) -> Self {
        let mut index = Self {
            by_call_id: HashMap::new(),
            thread_id: None,
        };
        for (item_index, envelope) in items.iter().enumerate() {
            index.insert_envelope(item_index, envelope);
        }
        index
    }

    fn insert_envelope(&mut self, index: usize, envelope: &'a ResponseItemEnvelope) {
        let (call_id, call, output) = match &envelope.item {
            ResponseItem::FunctionCall {
                call_id,
                name,
                namespace,
                ..
            } => (
                call_id.as_str(),
                Some(IndexedCall {
                    index,
                    envelope,
                    kind: ToolCallKind::Function,
                    name,
                    namespace: namespace.as_deref(),
                    status: None,
                }),
                None,
            ),
            ResponseItem::CustomToolCall {
                call_id,
                name,
                namespace,
                status,
                ..
            } => (
                call_id.as_str(),
                Some(IndexedCall {
                    index,
                    envelope,
                    kind: ToolCallKind::Custom,
                    name,
                    namespace: namespace.as_deref(),
                    status: status.as_deref(),
                }),
                None,
            ),
            ResponseItem::FunctionCallOutput {
                call_id: Some(call_id),
                name,
                namespace,
                ..
            } => (
                call_id.as_str(),
                None,
                Some(IndexedOutput {
                    index,
                    envelope,
                    kind: ToolCallKind::Function,
                    name: name.as_deref(),
                    namespace: namespace.as_deref(),
                }),
            ),
            ResponseItem::CustomToolCallOutput { call_id, name, .. } => (
                call_id.as_str(),
                None,
                Some(IndexedOutput {
                    index,
                    envelope,
                    kind: ToolCallKind::Custom,
                    name: name.as_deref(),
                    namespace: None,
                }),
            ),
            _ => return,
        };
        let entry = self.by_call_id.entry(call_id.to_owned()).or_default();
        if let Some(call) = call {
            entry.calls.push(call);
        }
        if let Some(output) = output {
            entry.outputs.push(output);
        }
    }

    /// Resolve one unique, ordinary text observation without copying its body.
    pub(super) fn get(
        &self,
        call_id: &str,
    ) -> Result<IndexedObservationRef<'a>, ArchiveEvidenceError> {
        let entry = self
            .by_call_id
            .get(call_id)
            .ok_or(ArchiveEvidenceError::Ineligible(
                ArchiveEvidenceIneligibility::AmbiguousCall,
            ))?;
        if entry.calls.len() != 1 {
            return Err(ArchiveEvidenceError::Ineligible(
                ArchiveEvidenceIneligibility::AmbiguousCall,
            ));
        }
        if entry.outputs.len() != 1 {
            return Err(ArchiveEvidenceError::Ineligible(
                ArchiveEvidenceIneligibility::AmbiguousOutput,
            ));
        }
        let call = entry.calls[0];
        let output = entry.outputs[0];
        if call.index >= output.index {
            return Err(ArchiveEvidenceError::Ineligible(
                ArchiveEvidenceIneligibility::OutputMismatch,
            ));
        }
        if call.kind != output.kind {
            return Err(ArchiveEvidenceError::Ineligible(
                ArchiveEvidenceIneligibility::OutputMismatch,
            ));
        }
        if output.name.is_some_and(|name| name != call.name)
            || output
                .namespace
                .is_some_and(|namespace| Some(namespace) != call.namespace)
        {
            return Err(ArchiveEvidenceError::Ineligible(
                ArchiveEvidenceIneligibility::UnsupportedTool,
            ));
        }
        if !is_ordinary_passthrough(passthrough(&call.envelope.item))
            || !is_ordinary_harness_metadata(call.envelope.metadata.as_ref())
            || !is_ordinary_passthrough(passthrough(&output.envelope.item))
            || !is_ordinary_harness_metadata(output.envelope.metadata.as_ref())
        {
            return Err(ArchiveEvidenceError::Ineligible(
                ArchiveEvidenceIneligibility::NonOrdinaryProvenance,
            ));
        }
        if call.kind == ToolCallKind::Custom && call.status != Some("completed") {
            return Err(ArchiveEvidenceError::Ineligible(
                ArchiveEvidenceIneligibility::NonFinalTerminal,
            ));
        }
        let body = text_output(&output.envelope.item).ok_or(ArchiveEvidenceError::Ineligible(
            ArchiveEvidenceIneligibility::NonTextOutput,
        ))?;
        if call.name == "exec_command" {
            if call.kind != ToolCallKind::Function {
                return Err(ArchiveEvidenceError::Ineligible(
                    ArchiveEvidenceIneligibility::UnsupportedTool,
                ));
            }
            Ok(IndexedObservationRef {
                call_index: call.index,
                output_index: output.index,
                call: call.envelope,
                output: output.envelope,
                tool_name: call.name,
                body,
                kind: call.kind,
                terminal: completed_command_terminal(&entry.terminals).ok(),
            })
        } else {
            Ok(IndexedObservationRef {
                call_index: call.index,
                output_index: output.index,
                call: call.envelope,
                output: output.envelope,
                tool_name: call.name,
                body,
                kind: call.kind,
                terminal: None,
            })
        }
    }

    pub(super) fn pair_indices(&self, call_id: &str) -> Option<(usize, usize)> {
        let entry = self.by_call_id.get(call_id)?;
        if entry.calls.len() != 1 || entry.outputs.len() != 1 {
            return None;
        }
        let call = entry.calls[0];
        let output = entry.outputs[0];
        (call.index < output.index
            && call.kind == output.kind
            && output.name.is_none_or(|name| name == call.name)
            && output
                .namespace
                .is_none_or(|namespace| Some(namespace) == call.namespace))
        .then_some((call.index, output.index))
    }

    pub fn reference(
        &self,
        call_id: &str,
        thread_id: &ThreadId,
        active_call_ids: &HashSet<String>,
    ) -> Result<(ObservationReference, IndexedObservationRef<'a>), ArchiveEvidenceError> {
        let expected_thread_id = thread_id.to_string();
        if self.thread_id.as_deref() != Some(expected_thread_id.as_str()) {
            return Err(ArchiveEvidenceError::Invalid(
                "observation index thread does not match requested reference thread",
            ));
        }
        if call_id.is_empty() {
            return Err(ArchiveEvidenceError::Ineligible(
                ArchiveEvidenceIneligibility::OutputMismatch,
            ));
        }
        if active_call_ids.contains(call_id) {
            return Err(ArchiveEvidenceError::Ineligible(
                ArchiveEvidenceIneligibility::ActiveCall,
            ));
        }
        let observation = self.get(call_id)?;
        Ok((
            ObservationReference::new(
                thread_id.to_string(),
                call_id.to_owned(),
                content_sha256(observation.body),
            ),
            observation,
        ))
    }

    /// Resolve a persisted observation pointer to the borrowed original call/output pair.
    pub fn resolve_reference(
        &self,
        reference: &ObservationReference,
        active_call_ids: &HashSet<String>,
    ) -> Result<IndexedObservationRef<'a>, ArchiveEvidenceError> {
        if self.thread_id.as_deref() != Some(reference.thread_id.as_str())
            || reference.call_id.is_empty()
        {
            return Err(ArchiveEvidenceError::Invalid(
                "observation reference thread or call identity mismatch",
            ));
        }
        if active_call_ids.contains(&reference.call_id) {
            return Err(ArchiveEvidenceError::Ineligible(
                ArchiveEvidenceIneligibility::ActiveCall,
            ));
        }
        let observation = self.get(&reference.call_id)?;
        if content_sha256(observation.body) != reference.sha256 {
            return Err(ArchiveEvidenceError::Invalid(
                "observation output hash does not match persisted reference",
            ));
        }
        Ok(observation)
    }

    /// Resolve an exact UTF-8 byte range after binding the full observation reference.
    ///
    /// The output reference is verified even when retrieving call arguments. `part_sha256`
    /// independently binds the selected whole call or output text before slicing it.
    pub fn resolve_range(
        &self,
        reference: &ObservationReference,
        active_call_ids: &HashSet<String>,
        part: ObservationPart,
        start: usize,
        end: usize,
        part_sha256: &str,
    ) -> Result<ObservationRange, ArchiveEvidenceError> {
        let observation = self.resolve_reference(reference, active_call_ids)?;
        let text = match part {
            ObservationPart::Call => match &observation.call.item {
                ResponseItem::FunctionCall {
                    encrypted_function_args: Some(_),
                    ..
                } => {
                    return Err(ArchiveEvidenceError::Invalid(
                        "encrypted function arguments are not retrievable as text",
                    ));
                }
                ResponseItem::FunctionCall { arguments, .. } => arguments.as_str(),
                ResponseItem::CustomToolCall { input, .. } => input.as_str(),
                _ => {
                    return Err(ArchiveEvidenceError::Invalid(
                        "observation call does not contain text input",
                    ));
                }
            },
            ObservationPart::Output => observation.body,
        };
        if content_sha256(text) != part_sha256 {
            return Err(ArchiveEvidenceError::Invalid(
                "observation part digest does not match canonical text",
            ));
        }
        if start >= end
            || end.saturating_sub(start) > OBSERVATION_PART_FULL_LIMIT_BYTES
            || end > text.len()
        {
            return Err(ArchiveEvidenceError::Invalid(
                "observation range must be nonempty, within bounds, and at most 2048 bytes",
            ));
        }
        let excerpt = text.get(start..end).ok_or(ArchiveEvidenceError::Invalid(
            "observation range is not on UTF-8 boundaries",
        ))?;
        Ok(ObservationRange {
            reference: reference.clone(),
            part,
            start,
            end,
            total_bytes: text.len(),
            part_sha256: part_sha256.to_owned(),
            text: excerpt.to_owned(),
        })
    }

    pub(super) fn archive_reference(
        &self,
        call_id: &str,
        expected_thread: &ThreadId,
        active_call_ids: &HashSet<String>,
    ) -> Result<(ArchiveReference, IndexedObservationRef<'a>), ArchiveEvidenceError> {
        let (observation_reference, observation) =
            self.reference(call_id, expected_thread, active_call_ids)?;
        if observation.body.len() > MAX_ARCHIVE_EVIDENCE_BYTES {
            return Err(ArchiveEvidenceError::Ineligible(
                ArchiveEvidenceIneligibility::OversizedOutput,
            ));
        }
        let reference =
            self.terminal_reference_for_observation(&observation_reference, observation)?;
        Ok((reference, observation))
    }

    /// Validate an exec terminal against a borrowed, already-indexed pair without applying the
    /// legacy full-result materialization limit.
    pub(super) fn terminal_reference_for_observation(
        &self,
        observation_reference: &ObservationReference,
        observation: IndexedObservationRef<'a>,
    ) -> Result<ArchiveReference, ArchiveEvidenceError> {
        let indexed_thread = self.thread_id.as_deref();
        let call_id = match &observation.call.item {
            ResponseItem::FunctionCall { call_id, .. }
            | ResponseItem::CustomToolCall { call_id, .. } => call_id,
            _ => {
                return Err(ArchiveEvidenceError::Invalid(
                    "indexed observation call identity is invalid",
                ));
            }
        };
        if indexed_thread != Some(observation_reference.thread_id.as_str())
            || call_id != &observation_reference.call_id
            || observation_reference.call_id.is_empty()
            || content_sha256(observation.body) != observation_reference.sha256
        {
            return Err(ArchiveEvidenceError::Invalid(
                "observation reference does not match indexed exec pair",
            ));
        }
        if observation.kind != ToolCallKind::Function || observation.tool_name != "exec_command" {
            return Err(ArchiveEvidenceError::Ineligible(
                ArchiveEvidenceIneligibility::UnsupportedTool,
            ));
        }
        let terminal = observation
            .terminal
            .ok_or(ArchiveEvidenceError::Ineligible(
                ArchiveEvidenceIneligibility::MissingTerminal,
            ))?;
        let status = match terminal.status {
            CommandExecutionStatus::Completed => ArchiveTerminalStatus::Completed,
            CommandExecutionStatus::Failed => ArchiveTerminalStatus::Failed,
            CommandExecutionStatus::InProgress | CommandExecutionStatus::Declined => {
                return Err(ArchiveEvidenceError::Ineligible(
                    ArchiveEvidenceIneligibility::NonFinalTerminal,
                ));
            }
        };
        let exit_code = terminal.exit_code.ok_or(ArchiveEvidenceError::Ineligible(
            ArchiveEvidenceIneligibility::MissingExitCode,
        ))?;
        if (status == ArchiveTerminalStatus::Completed && exit_code != 0)
            || (status == ArchiveTerminalStatus::Failed && exit_code == 0)
        {
            return Err(ArchiveEvidenceError::Ineligible(
                ArchiveEvidenceIneligibility::ContradictoryTerminal,
            ));
        }
        Ok(ArchiveReference::new(
            observation_reference.thread_id.clone(),
            observation_reference.call_id.clone(),
            observation_reference.sha256.clone(),
            status,
            exit_code,
            terminal.process_id.clone(),
        ))
    }
}

pub(super) fn passthrough(
    item: &ResponseItem,
) -> Option<&codex_protocol::models::InternalChatMessageMetadataPassthrough> {
    match item {
        ResponseItem::FunctionCall {
            internal_chat_message_metadata_passthrough,
            ..
        }
        | ResponseItem::FunctionCallOutput {
            internal_chat_message_metadata_passthrough,
            ..
        }
        | ResponseItem::CustomToolCall {
            internal_chat_message_metadata_passthrough,
            ..
        }
        | ResponseItem::CustomToolCallOutput {
            internal_chat_message_metadata_passthrough,
            ..
        } => internal_chat_message_metadata_passthrough.as_ref(),
        _ => None,
    }
}

fn text_output(item: &ResponseItem) -> Option<&str> {
    match item {
        ResponseItem::FunctionCallOutput { output, .. }
        | ResponseItem::CustomToolCallOutput { output, .. } => output.text_content(),
        _ => None,
    }
}

fn completed_command_terminal<'a>(
    terminals: &[&'a codex_protocol::items::CommandExecutionItem],
) -> Result<&'a codex_protocol::items::CommandExecutionItem, ArchiveEvidenceError> {
    let terminals = terminals
        .iter()
        .copied()
        .filter(|command| {
            command.source == codex_protocol::protocol::ExecCommandSource::UnifiedExecStartup
                && matches!(
                    command.status,
                    CommandExecutionStatus::Completed | CommandExecutionStatus::Failed
                )
                && command.exit_code.is_some()
        })
        .collect::<Vec<_>>();
    if terminals.len() != 1 {
        return Err(ArchiveEvidenceError::Ineligible(
            ArchiveEvidenceIneligibility::MissingTerminal,
        ));
    }
    Ok(terminals[0])
}

/// Compare two items as persisted in rollout.
///
/// `FunctionCallOutputPayload::success` is internal metadata that rollout never serializes, so a
/// live output and its canonical copy may differ only there. Every persisted field stays bound; the
/// output fields are listed exhaustively so a new field must be classified here.
pub(super) fn same_persisted_item(actual: &ResponseItem, expected: &ResponseItem) -> bool {
    match (actual, expected) {
        (
            ResponseItem::FunctionCallOutput {
                id: actual_id,
                call_id: actual_call_id,
                name: actual_name,
                namespace: actual_namespace,
                output: actual_output,
                internal_chat_message_metadata_passthrough: actual_passthrough,
            },
            ResponseItem::FunctionCallOutput {
                id: expected_id,
                call_id: expected_call_id,
                name: expected_name,
                namespace: expected_namespace,
                output: expected_output,
                internal_chat_message_metadata_passthrough: expected_passthrough,
            },
        ) => {
            actual_id == expected_id
                && actual_call_id == expected_call_id
                && actual_name == expected_name
                && actual_namespace == expected_namespace
                && actual_output.body == expected_output.body
                && actual_passthrough == expected_passthrough
        }
        (
            ResponseItem::CustomToolCallOutput {
                id: actual_id,
                call_id: actual_call_id,
                name: actual_name,
                output: actual_output,
                internal_chat_message_metadata_passthrough: actual_passthrough,
            },
            ResponseItem::CustomToolCallOutput {
                id: expected_id,
                call_id: expected_call_id,
                name: expected_name,
                output: expected_output,
                internal_chat_message_metadata_passthrough: expected_passthrough,
            },
        ) => {
            actual_id == expected_id
                && actual_call_id == expected_call_id
                && actual_name == expected_name
                && actual_output.body == expected_output.body
                && actual_passthrough == expected_passthrough
        }
        _ => actual == expected,
    }
}

pub(super) fn same_output_identity_ignoring_body(
    actual: &ResponseItem,
    expected: &ResponseItem,
) -> bool {
    match (actual, expected) {
        (
            ResponseItem::FunctionCallOutput {
                id: actual_id,
                call_id: actual_call_id,
                name: actual_name,
                namespace: actual_namespace,
                internal_chat_message_metadata_passthrough: actual_passthrough,
                ..
            },
            ResponseItem::FunctionCallOutput {
                id: expected_id,
                call_id: expected_call_id,
                name: expected_name,
                namespace: expected_namespace,
                internal_chat_message_metadata_passthrough: expected_passthrough,
                ..
            },
        ) => {
            actual_id == expected_id
                && actual_call_id == expected_call_id
                && actual_name == expected_name
                && actual_namespace == expected_namespace
                && actual_passthrough == expected_passthrough
        }
        _ => false,
    }
}

/// Compare a fresh text output after allowing only its text body to differ. Identity,
/// passthrough, and every other persisted typed field remain bound to the canonical rollout item;
/// the unpersisted `success` flag is not compared (see `same_persisted_item`).
pub(super) fn same_fresh_output_except_text_body(
    actual: &ResponseItem,
    expected: &ResponseItem,
) -> bool {
    match (actual, expected) {
        (
            ResponseItem::FunctionCallOutput {
                id: actual_id,
                call_id: actual_call_id,
                name: actual_name,
                namespace: actual_namespace,
                output: actual_output,
                internal_chat_message_metadata_passthrough: actual_passthrough,
            },
            ResponseItem::FunctionCallOutput {
                id: expected_id,
                call_id: expected_call_id,
                name: expected_name,
                namespace: expected_namespace,
                output: expected_output,
                internal_chat_message_metadata_passthrough: expected_passthrough,
            },
        ) => {
            matches!(
                &actual_output.body,
                codex_protocol::models::FunctionCallOutputBody::Text(_)
            ) && matches!(
                &expected_output.body,
                codex_protocol::models::FunctionCallOutputBody::Text(_)
            ) && actual_id == expected_id
                && actual_call_id == expected_call_id
                && actual_name == expected_name
                && actual_namespace == expected_namespace
                && actual_passthrough == expected_passthrough
        }
        (
            ResponseItem::CustomToolCallOutput {
                id: actual_id,
                call_id: actual_call_id,
                name: actual_name,
                output: actual_output,
                internal_chat_message_metadata_passthrough: actual_passthrough,
            },
            ResponseItem::CustomToolCallOutput {
                id: expected_id,
                call_id: expected_call_id,
                name: expected_name,
                output: expected_output,
                internal_chat_message_metadata_passthrough: expected_passthrough,
            },
        ) => {
            matches!(
                &actual_output.body,
                codex_protocol::models::FunctionCallOutputBody::Text(_)
            ) && matches!(
                &expected_output.body,
                codex_protocol::models::FunctionCallOutputBody::Text(_)
            ) && actual_id == expected_id
                && actual_call_id == expected_call_id
                && actual_name == expected_name
                && actual_passthrough == expected_passthrough
        }
        _ => false,
    }
}
