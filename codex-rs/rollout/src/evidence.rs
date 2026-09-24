//! Read-only proof and retrieval for host archive references.

use codex_history::ArchiveReference;
use codex_history::ObservationReference;
use codex_history::ResponseItemEnvelope;
use codex_history::archive_reference::is_ordinary_harness_metadata;
use codex_history::archive_reference::is_ordinary_passthrough;
use codex_protocol::ThreadId;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::TruncationPolicy;
use std::collections::HashSet;
use std::fmt;
use std::path::Path;

#[path = "evidence/compaction_inventory.rs"]
mod compaction_inventory;
#[path = "evidence/observation_index.rs"]
mod observation_index;
pub use compaction_inventory::InventoryQuery;
pub use compaction_inventory::ObservationInventoryEntry;
pub use compaction_inventory::ObservationInventoryPage;
pub use compaction_inventory::inventory_compaction_from_items;
pub use observation_index::IndexedObservationRef;
pub use observation_index::ObservationIndex;
pub use observation_index::ObservationPart;
pub use observation_index::ObservationRange;
use observation_index::passthrough;
use observation_index::same_fresh_output_except_text_body;
use observation_index::same_output_identity_ignoring_body;
use observation_index::same_persisted_item;

pub const MAX_ARCHIVE_EVIDENCE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveEvidence {
    pub reference: ArchiveReference,
    pub tool_name: String,
    pub result: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveEvidenceIneligibility {
    ActiveCall,
    AmbiguousCall,
    UnsupportedTool,
    NonOrdinaryProvenance,
    AmbiguousOutput,
    NonTextOutput,
    OversizedOutput,
    OutputMismatch,
    MissingTerminal,
    NonFinalTerminal,
    MissingExitCode,
    ContradictoryTerminal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArchiveEvidenceError {
    Ineligible(ArchiveEvidenceIneligibility),
    Invalid(&'static str),
}

impl fmt::Display for ArchiveEvidenceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ineligible(reason) => {
                write!(formatter, "archive candidate is ineligible: {reason:?}")
            }
            Self::Invalid(reason) => formatter.write_str(reason),
        }
    }
}

impl std::error::Error for ArchiveEvidenceError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArchiveEvidenceSelection {
    Eligible(ArchiveEvidence),
    Ineligible,
}

#[derive(Clone, Copy)]
enum ArchiveEvidenceTarget<'a> {
    Candidate {
        expected_output: &'a ResponseItemEnvelope,
    },
    Existing {
        expected_reference: &'a ArchiveReference,
    },
    Lookup,
}

impl ArchiveEvidenceTarget<'_> {
    fn ineligible(self, reason: ArchiveEvidenceIneligibility) -> ArchiveEvidenceError {
        match self {
            Self::Candidate { .. } => ArchiveEvidenceError::Ineligible(reason),
            Self::Existing { .. } | Self::Lookup => {
                ArchiveEvidenceError::Invalid("archive evidence is not valid")
            }
        }
    }
}

/// Resolves one output against already-loaded canonical rollout items.
///
/// expected_output is required for a new reference and proves that the exact
/// persisted response item is being replaced. Existing references pass
/// expected_reference instead and are matched to the immutable raw output.
pub fn resolve_archive_evidence_from_items(
    items: &[codex_history::RolloutItem],
    expected_thread: &ThreadId,
    call_id: &str,
    expected_output: Option<&ResponseItemEnvelope>,
    expected_reference: Option<&ArchiveReference>,
    active_call_ids: &HashSet<String>,
) -> Result<ArchiveEvidence, ArchiveEvidenceError> {
    let target = match (expected_output, expected_reference) {
        (Some(expected_output), None) => ArchiveEvidenceTarget::Candidate { expected_output },
        (None, Some(expected_reference)) => ArchiveEvidenceTarget::Existing { expected_reference },
        (None, None) => ArchiveEvidenceTarget::Lookup,
        (Some(_), Some(_)) => {
            return Err(ArchiveEvidenceError::Invalid(
                "archive evidence cannot validate a new output and existing reference together",
            ));
        }
    };
    resolve_archive_evidence_from_items_inner(
        items,
        expected_thread,
        call_id,
        target,
        active_call_ids,
    )
}

pub fn select_new_archive_evidence_from_items(
    items: &[codex_history::RolloutItem],
    expected_thread: &ThreadId,
    call_id: &str,
    expected_output: &ResponseItemEnvelope,
    active_call_ids: &HashSet<String>,
) -> Result<ArchiveEvidenceSelection, ArchiveEvidenceError> {
    match resolve_archive_evidence_from_items(
        items,
        expected_thread,
        call_id,
        Some(expected_output),
        None,
        active_call_ids,
    ) {
        Ok(evidence) => Ok(ArchiveEvidenceSelection::Eligible(evidence)),
        Err(ArchiveEvidenceError::Ineligible(_)) => Ok(ArchiveEvidenceSelection::Ineligible),
        Err(error) => Err(error),
    }
}

/// Resolves terminal command evidence only when the selected history still
/// contains the exact ordinary call persisted in the canonical rollout.
///
/// New outputs pass `expected_output`; v1 archive markers pass
/// `expected_reference`. The output resolver remains the owner of terminal,
/// result, thread, and reference validation.
pub fn resolve_completed_work_evidence_from_items(
    items: &[codex_history::RolloutItem],
    expected_thread: &ThreadId,
    call_id: &str,
    expected_call: &ResponseItemEnvelope,
    expected_output: Option<&ResponseItemEnvelope>,
    expected_reference: Option<&ArchiveReference>,
    active_call_ids: &HashSet<String>,
) -> Result<ArchiveEvidence, ArchiveEvidenceError> {
    let target = match (expected_output, expected_reference) {
        (Some(expected_output), None) => ArchiveEvidenceTarget::Candidate { expected_output },
        (None, Some(expected_reference)) => ArchiveEvidenceTarget::Existing { expected_reference },
        (None, None) => ArchiveEvidenceTarget::Lookup,
        (Some(_), Some(_)) => {
            return Err(ArchiveEvidenceError::Invalid(
                "completed work requires exactly one output or archive reference",
            ));
        }
    };
    let index = ObservationIndex::new(items, expected_thread);
    let evidence = resolve_archive_evidence_with_index(
        &index,
        expected_thread,
        call_id,
        target,
        active_call_ids,
    )?;

    let ResponseItem::FunctionCall {
        call_id: selected_call_id,
        name: selected_name,
        internal_chat_message_metadata_passthrough: selected_passthrough,
        ..
    } = &expected_call.item
    else {
        return Err(ArchiveEvidenceError::Ineligible(
            ArchiveEvidenceIneligibility::UnsupportedTool,
        ));
    };
    if selected_call_id != call_id {
        return Err(ArchiveEvidenceError::Ineligible(
            ArchiveEvidenceIneligibility::OutputMismatch,
        ));
    }
    if selected_name != "exec_command" {
        return Err(ArchiveEvidenceError::Ineligible(
            ArchiveEvidenceIneligibility::UnsupportedTool,
        ));
    }
    if !is_ordinary_passthrough(selected_passthrough.as_ref())
        || !is_ordinary_harness_metadata(expected_call.metadata.as_ref())
    {
        return Err(ArchiveEvidenceError::Ineligible(
            ArchiveEvidenceIneligibility::NonOrdinaryProvenance,
        ));
    }

    let observation = index.get(call_id)?;
    if !same_persisted_item(&observation.call.item, &expected_call.item) {
        return Err(ArchiveEvidenceError::Ineligible(
            ArchiveEvidenceIneligibility::OutputMismatch,
        ));
    }

    Ok(evidence)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreparedCompactionReferenceKind {
    Fresh,
    Existing,
    /// A V2 observation marker verified against its canonical raw observation. The canonical
    /// texts are borrowed like a fresh pair; the live body is the host marker, never raw data.
    ObservationMarker,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedCompactionSourcePair<'a> {
    pub call_index: usize,
    pub output_index: usize,
    pub reference: ObservationReference,
    pub reference_kind: PreparedCompactionReferenceKind,
    /// Original canonical output byte length, borrowed from the same verified index entry.
    /// This scalar lets history project an existing marker without materializing its raw body.
    pub output_total_bytes: usize,
    /// Canonical text fields borrowed from the rollout only for fresh pairs. Existing markers
    /// deliberately carry no raw body and must use the existing-reference projection path.
    pub tool_name: &'a str,
    pub canonical_call_input: Option<&'a str>,
    pub canonical_output_text: Option<&'a str>,
    /// Present only for verified `exec_command` observations. This is transient validation data;
    /// V2 persists the generic output reference and must not add a legacy V1 marker.
    pub terminal_reference: Option<ArchiveReference>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedCompactionSources<'a> {
    pub pairs: Vec<PreparedCompactionSourcePair<'a>>,
    /// Selected-history indices that this function did not verify as removable pairs.
    /// This is only protection from tool-pair removal; downstream history policy still owns
    /// whether each item is summarized or retained in another form.
    pub protected_indices: Vec<usize>,
}

/// Inventory verified returned call/output observations from a selected history snapshot.
///
/// Existing v1 archive markers are validated against their canonical raw command output even if
/// the selected snapshot lacks an unambiguous preceding call. Fresh pairs require a unique
/// same-kind call/output with ordinary provenance and matching call/output metadata. Output text
/// may match the canonical body exactly or match deterministic history truncation using the
/// persisted output token budget. Only text-only output is eligible.
/// A non-exec output proves only that a tool returned text; it does not prove success or semantic
/// coverage. All other selected indices stay protected from tool-pair removal. Inputs are
/// read-only and remain unchanged.
pub fn prepare_compaction_sources<'a>(
    selected: &[ResponseItemEnvelope],
    canonical: &'a [codex_history::RolloutItem],
    expected_thread: &ThreadId,
    active_call_ids: &HashSet<String>,
) -> Result<PreparedCompactionSources<'a>, String> {
    let canonical_index = ObservationIndex::new(canonical, expected_thread);
    let selected_index = ObservationIndex::from_envelopes(selected);
    let mut existing_references: Vec<Option<ArchiveReference>> = vec![None; selected.len()];
    for (index, envelope) in selected.iter().enumerate() {
        let Some(reference) = envelope
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.rencrow_archive_reference.as_ref())
        else {
            continue;
        };
        let ResponseItem::FunctionCallOutput {
            call_id: Some(call_id),
            ..
        } = &envelope.item
        else {
            return Err("existing archive reference is attached to a non-output item".into());
        };
        codex_history::archive_reference::validate_marker(
            envelope,
            &expected_thread.to_string(),
            reference,
        )
        .map_err(|error| format!("existing archive marker is invalid: {error}"))?;
        let (canonical_reference, canonical_observation) = canonical_index
            .archive_reference(call_id, expected_thread, active_call_ids)
            .map_err(|error| format!("existing archive evidence is invalid: {error}"))?;
        if &canonical_reference != reference {
            return Err("existing archive evidence does not match its persisted reference".into());
        }
        if !same_output_identity_ignoring_body(&envelope.item, &canonical_observation.output.item) {
            return Err(
                "existing archive marker output identity differs from persisted output".into(),
            );
        }
        let mut marker_metadata = envelope.metadata.clone().unwrap_or_default();
        marker_metadata.rencrow_archive_reference = None;
        let canonical_metadata = canonical_observation
            .output
            .metadata
            .clone()
            .unwrap_or_default();
        if marker_metadata != canonical_metadata {
            return Err("existing archive marker metadata differs from persisted output".into());
        }
        existing_references[index] = Some(reference.clone());
    }

    let mut pairs = Vec::new();
    let mut paired_indices = vec![false; selected.len()];
    for output_index in 0..selected.len() {
        let call_id = match &selected[output_index].item {
            ResponseItem::FunctionCallOutput {
                call_id: Some(call_id),
                ..
            }
            | ResponseItem::CustomToolCallOutput { call_id, .. } => call_id,
            _ => continue,
        };
        if active_call_ids.contains(call_id) {
            continue;
        }
        let (call_index, indexed_output_index) = match selected_index.pair_indices(call_id) {
            Some(indices) => indices,
            None => continue,
        };
        if indexed_output_index != output_index {
            continue;
        }
        if call_index >= output_index || paired_indices[call_index] || paired_indices[output_index]
        {
            continue;
        }
        let selected_call = &selected[call_index];
        if !is_ordinary_passthrough(passthrough(&selected_call.item))
            || !is_ordinary_harness_metadata(selected_call.metadata.as_ref())
        {
            continue;
        }
        if matches!(
            &selected_call.item,
            ResponseItem::FunctionCall {
                encrypted_function_args: Some(_),
                ..
            }
        ) {
            continue;
        }
        if is_observation_marker(&selected[output_index]) {
            pairs.push(prepare_observation_marker(
                &canonical_index,
                selected,
                call_index,
                output_index,
                call_id,
                expected_thread,
                active_call_ids,
            )?);
            paired_indices[call_index] = true;
            paired_indices[output_index] = true;
            continue;
        }
        let reference_kind = if existing_references[output_index].is_some() {
            PreparedCompactionReferenceKind::Existing
        } else {
            PreparedCompactionReferenceKind::Fresh
        };
        let (reference, terminal_reference, canonical_observation, fresh_source) =
            if let Some(reference) = &existing_references[output_index] {
                let observation = match canonical_index.get(call_id) {
                    Ok(observation) => observation,
                    Err(ArchiveEvidenceError::Invalid(reason)) => return Err(reason.into()),
                    Err(ArchiveEvidenceError::Ineligible(_)) => continue,
                };
                (
                    ObservationReference::new(
                        reference.thread_id.clone(),
                        reference.call_id.clone(),
                        reference.original_content_sha256.clone(),
                    ),
                    Some(reference.clone()),
                    observation,
                    None,
                )
            } else {
                let selected_observation = match selected_index.get(call_id) {
                    Ok(observation) => observation,
                    Err(ArchiveEvidenceError::Invalid(reason)) => return Err(reason.into()),
                    Err(ArchiveEvidenceError::Ineligible(_)) => continue,
                };
                let (canonical_reference, canonical_observation) =
                    match canonical_index.reference(call_id, expected_thread, active_call_ids) {
                        Ok(result) => result,
                        Err(ArchiveEvidenceError::Invalid(reason)) => return Err(reason.into()),
                        Err(ArchiveEvidenceError::Ineligible(_)) => continue,
                    };
                let terminal_reference = if canonical_observation.tool_name == "exec_command" {
                    match canonical_index.terminal_reference_for_observation(
                        &canonical_reference,
                        canonical_observation,
                    ) {
                        Ok(reference) => Some(reference),
                        Err(ArchiveEvidenceError::Invalid(reason)) => return Err(reason.into()),
                        Err(ArchiveEvidenceError::Ineligible(_)) => continue,
                    }
                } else {
                    None
                };
                if canonical_observation.call.metadata != selected_call.metadata
                    || canonical_observation.output.metadata != selected[output_index].metadata
                    || !fresh_output_matches(
                        &canonical_observation.output.item,
                        &selected_observation.output.item,
                        canonical_observation
                            .output
                            .metadata
                            .as_ref()
                            .and_then(|metadata| metadata.history_truncation_token_limit),
                    )
                {
                    continue;
                }
                let Some(canonical_call_input) = call_input(&canonical_observation.call.item)
                else {
                    continue;
                };
                (
                    canonical_reference,
                    terminal_reference,
                    canonical_observation,
                    Some((canonical_call_input, canonical_observation.body)),
                )
            };
        if !same_persisted_item(&canonical_observation.call.item, &selected_call.item) {
            continue;
        }
        paired_indices[call_index] = true;
        paired_indices[output_index] = true;
        pairs.push(PreparedCompactionSourcePair {
            call_index,
            output_index,
            reference,
            reference_kind,
            output_total_bytes: canonical_observation.body.len(),
            tool_name: canonical_observation.tool_name,
            canonical_call_input: fresh_source.map(|(input, _)| input),
            canonical_output_text: fresh_source.map(|(_, output)| output),
            terminal_reference,
        });
    }

    // A marker replaced raw data, so it must never fall back to protected raw output (§39).
    if selected
        .iter()
        .enumerate()
        .any(|(index, envelope)| is_observation_marker(envelope) && !paired_indices[index])
    {
        return Err(
            "V2 observation marker could not be verified as a unique inactive call and output"
                .into(),
        );
    }
    let protected_indices = paired_indices
        .iter()
        .enumerate()
        .filter(|(_, paired)| !**paired)
        .map(|(index, _)| index)
        .collect();
    Ok(PreparedCompactionSources {
        pairs,
        protected_indices,
    })
}

fn is_observation_marker(envelope: &ResponseItemEnvelope) -> bool {
    envelope
        .metadata
        .as_ref()
        .is_some_and(|metadata| metadata.rencrow_observation_projection.is_some())
}

/// Verify an existing V2 observation marker against its canonical raw observation (§39–40).
///
/// The marker body and metadata must be exactly regenerated from the canonical call and output,
/// and the call and output identity must match. Any mismatch is a deterministic integrity
/// conflict; the marker is never reinterpreted as raw output.
fn prepare_observation_marker<'a>(
    canonical_index: &ObservationIndex<'a>,
    selected: &[ResponseItemEnvelope],
    call_index: usize,
    output_index: usize,
    call_id: &str,
    expected_thread: &ThreadId,
    active_call_ids: &HashSet<String>,
) -> Result<PreparedCompactionSourcePair<'a>, String> {
    let (reference, observation) = canonical_index
        .reference(call_id, expected_thread, active_call_ids)
        .map_err(|error| {
            format!("V2 observation marker has no verifiable canonical source: {error}")
        })?;
    let call_text = call_input(&observation.call.item)
        .ok_or_else(|| "V2 observation marker call has no text input".to_owned())?;
    let (call, output) = (&selected[call_index], &selected[output_index]);
    let marker_body = match &output.item {
        ResponseItem::FunctionCallOutput { output, .. }
        | ResponseItem::CustomToolCallOutput { output, .. } => output.text_content(),
        _ => None,
    }
    .ok_or_else(|| "V2 observation marker body is not text".to_owned())?;
    if !same_persisted_item(&observation.call.item, &call.item)
        || observation.call.metadata != call.metadata
        || !same_fresh_output_except_text_body(&output.item, &observation.output.item)
    {
        return Err("V2 observation marker identity differs from its canonical source".into());
    }
    let regenerated = codex_history::project_observation(
        &reference,
        observation.tool_name,
        call_text,
        observation.body,
    )?;
    codex_history::observation_marker::verify_observation_marker(
        &regenerated,
        observation.output.metadata.as_ref(),
        output.metadata.as_ref(),
        marker_body,
    )?;
    let terminal_reference = if observation.tool_name == "exec_command" {
        match canonical_index.terminal_reference_for_observation(&reference, observation) {
            Ok(reference) => Some(reference),
            Err(ArchiveEvidenceError::Invalid(reason)) => return Err(reason.into()),
            Err(ArchiveEvidenceError::Ineligible(_)) => None,
        }
    } else {
        None
    };
    Ok(PreparedCompactionSourcePair {
        call_index,
        output_index,
        reference,
        reference_kind: PreparedCompactionReferenceKind::ObservationMarker,
        output_total_bytes: observation.body.len(),
        tool_name: observation.tool_name,
        canonical_call_input: Some(call_text),
        canonical_output_text: Some(observation.body),
        terminal_reference,
    })
}

fn call_input(item: &ResponseItem) -> Option<&str> {
    match item {
        ResponseItem::FunctionCall { arguments, .. } => Some(arguments),
        ResponseItem::CustomToolCall { input, .. } => Some(input),
        _ => None,
    }
}

fn fresh_output_matches(
    canonical: &ResponseItem,
    selected: &ResponseItem,
    saved_token_limit: Option<usize>,
) -> bool {
    if same_persisted_item(canonical, selected) {
        return true;
    }
    if !same_fresh_output_except_text_body(canonical, selected) {
        return false;
    }
    let Some(token_limit) = saved_token_limit else {
        return false;
    };
    let (Some(canonical_text), Some(selected_text)) = (
        canonical_text_output(canonical),
        canonical_text_output(selected),
    ) else {
        return false;
    };
    // truncate_text returns the original string when it fits within this threshold. Avoid asking
    // it to clone a large raw observation on a mismatch branch that cannot have been truncated.
    if token_limit > 0
        && canonical_text.len()
            <= codex_utils_output_truncation::approx_bytes_for_tokens(token_limit)
    {
        return false;
    }
    codex_utils_output_truncation::truncate_text(
        canonical_text,
        TruncationPolicy::Tokens(token_limit),
    ) == selected_text
}

fn canonical_text_output(item: &ResponseItem) -> Option<&str> {
    match item {
        ResponseItem::FunctionCallOutput { output, .. }
        | ResponseItem::CustomToolCallOutput { output, .. } => output.text_content(),
        _ => None,
    }
}

fn resolve_archive_evidence_from_items_inner(
    items: &[codex_history::RolloutItem],
    expected_thread: &ThreadId,
    call_id: &str,
    target: ArchiveEvidenceTarget<'_>,
    active_call_ids: &HashSet<String>,
) -> Result<ArchiveEvidence, ArchiveEvidenceError> {
    let index = ObservationIndex::new(items, expected_thread);
    resolve_archive_evidence_with_index(&index, expected_thread, call_id, target, active_call_ids)
}

fn resolve_archive_evidence_with_index(
    index: &ObservationIndex<'_>,
    expected_thread: &ThreadId,
    call_id: &str,
    target: ArchiveEvidenceTarget<'_>,
    active_call_ids: &HashSet<String>,
) -> Result<ArchiveEvidence, ArchiveEvidenceError> {
    if active_call_ids.contains(call_id) {
        return Err(target.ineligible(ArchiveEvidenceIneligibility::ActiveCall));
    }
    let thread = expected_thread.to_string();
    if let ArchiveEvidenceTarget::Existing { expected_reference } = target
        && !expected_reference.validates_identity(&thread, call_id)
    {
        return Err(ArchiveEvidenceError::Invalid(
            "archive reference thread or call identity mismatch",
        ));
    }
    let (reference, observation) = index
        .archive_reference(call_id, expected_thread, active_call_ids)
        .map_err(|error| match error {
            ArchiveEvidenceError::Ineligible(reason) => target.ineligible(reason),
            error => error,
        })?;

    if let ArchiveEvidenceTarget::Candidate { expected_output } = target {
        if !same_persisted_item(&observation.output.item, &expected_output.item) {
            return Err(target.ineligible(ArchiveEvidenceIneligibility::OutputMismatch));
        }
    }
    if let ArchiveEvidenceTarget::Existing { expected_reference } = target
        && &reference != expected_reference
    {
        return Err(ArchiveEvidenceError::Invalid(
            "archive reference metadata does not match persisted receipt",
        ));
    }

    Ok(ArchiveEvidence {
        reference,
        tool_name: observation.tool_name.to_owned(),
        result: observation.body.to_owned(),
    })
}

/// Loads a canonical rollout path and resolves one bounded archive result.
pub async fn resolve_archive_evidence(
    path: &Path,
    expected_thread: &ThreadId,
    call_id: &str,
    sha256: &str,
) -> std::io::Result<ArchiveEvidence> {
    let (items, thread_id, parse_errors) = crate::RolloutRecorder::load_rollout_items(path).await?;
    if parse_errors != 0 {
        return Err(std::io::Error::other(format!(
            "rollout contains {parse_errors} parse errors"
        )));
    }
    if thread_id != Some(*expected_thread) {
        return Err(std::io::Error::other(
            "rollout header thread does not match requested thread",
        ));
    }
    let active_call_ids = HashSet::new();
    let evidence = resolve_archive_evidence_from_items(
        &items,
        expected_thread,
        call_id,
        None,
        None,
        &active_call_ids,
    )
    .map_err(|error| std::io::Error::other(error.to_string()))?;
    if evidence.reference.original_content_sha256 != sha256 {
        return Err(std::io::Error::other("archive evidence hash mismatch"));
    }
    Ok(evidence)
}

#[cfg(test)]
#[path = "evidence_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "evidence_truncation_tests.rs"]
mod truncation_tests;

#[cfg(test)]
#[path = "evidence/f01_encrypted_call_tests.rs"]
mod f01_encrypted_call_tests;

#[cfg(test)]
#[path = "evidence/f08_range_tests.rs"]
mod f08_range_tests;

#[cfg(test)]
#[path = "evidence/f08_inventory_tests.rs"]
mod f08_inventory_tests;

#[cfg(test)]
#[path = "evidence/v2_marker_tests.rs"]
mod v2_marker_tests;
