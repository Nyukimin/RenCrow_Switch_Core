//! Deterministic preprocessing of host-captured compaction candidates.

use crate::archive_reference::ARCHIVE_REFERENCE_VERSION;
use crate::archive_reference::ArchiveReference;
use crate::archive_reference::ArchiveTerminalStatus;
use crate::archive_reference::content_sha256;
use crate::compaction_candidate::CandidateInput;
use crate::compaction_candidate::Origin;
use crate::compaction_plan::ByteRange;
use crate::compaction_plan::SourceRef;
use serde::Serialize;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::ops::Range;

/// Deterministic instruction exclusions tied to the original host snapshot.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InstructionPruning {
    pub applied: Vec<SourceRef>,
    pub retained_human_text: BTreeMap<String, String>,
}

/// Host-verified, single-pair link. This type is not deserialized from model output.
///
/// It identifies evidence that may be presented for semantic selection; it does not prove
/// that the instruction completed or that the terminal result means success.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct InstructionObservationLink {
    pub instruction: SourceRef,
    pub call: SourceRef,
    pub output: SourceRef,
    pub terminal: ArchiveReference,
}

/// Apply adopted source references to their exact current Human fragments.
///
/// This is intentionally a projection: the caller combines the retained text map with
/// the original native records, while the source refs keep ranges bound to the original
/// snapshot for later selection stages.
pub fn prune_known_obsolete(
    input: &CandidateInput,
    known: &[SourceRef],
) -> Result<InstructionPruning, String> {
    let snapshot = input.snapshot()?;
    for reference in known {
        if reference.range.start >= reference.range.end {
            return Err("known instruction reference has an empty or reversed range".into());
        }
    }

    let records = input
        .records
        .iter()
        .map(|record| (record.id.as_str(), record))
        .collect::<HashMap<_, _>>();
    let mut result = InstructionPruning::default();
    let mut removals = BTreeMap::<String, Vec<Range<usize>>>::new();

    for reference in known {
        let Some(record) = records.get(reference.id.as_str()).copied() else {
            continue;
        };
        if record.text.is_empty() {
            continue;
        }

        // Compare the whole fragment before validating its old range. If the record changed,
        // its old offset can be out of bounds or inside a UTF-8 code point and is simply stale.
        let current_fragment = snapshot
            .reference(
                &record.id,
                ByteRange {
                    start: 0,
                    end: record.text.len(),
                },
            )
            .map_err(|error| format!("invalid candidate snapshot: {error:?}"))?;
        if current_fragment.hash != reference.hash {
            continue;
        }
        let current_reference = snapshot
            .reference(&record.id, reference.range.clone())
            .map_err(|error| format!("invalid known instruction range: {error:?}"))?;
        if current_reference != *reference {
            continue;
        }

        let overlaps_protection = record.protected.iter().any(|protected| {
            reference.range.start < protected.end && protected.start < reference.range.end
        });
        if overlaps_protection || record.opaque.is_some() || record.origin == Origin::Unknown {
            return Err("known instruction reference overlaps protected source content".into());
        }
        if record.origin != Origin::Human || record.role != "user" {
            return Err("known instruction reference does not target a Human user source".into());
        }

        result.applied.push(reference.clone());
        removals
            .entry(record.id.clone())
            .or_default()
            .push(reference.range.start..reference.range.end);
    }

    for ranges in removals.values_mut() {
        ranges.sort_by_key(|range| (range.start, range.end));
        if ranges.windows(2).any(|pair| pair[1].start < pair[0].end) {
            return Err(
                "current instruction references have duplicate or overlapping ranges".into(),
            );
        }
    }

    for (id, mut ranges) in removals {
        let record = records
            .get(id.as_str())
            .expect("applied source IDs came from the input record index");
        ranges.sort_by_key(|range| std::cmp::Reverse(range.start));
        let mut retained = record.text.clone();
        for range in ranges {
            retained.replace_range(range, "");
        }
        result.retained_human_text.insert(id, retained);
    }

    Ok(result)
}

/// Deterministically bind the host decisions that shaped an instruction-selection presentation.
/// Callers must use this with the original snapshot hash, exact applied source refs, and all
/// host-verified links used by [`collect_instruction_candidates`].
pub fn instruction_presentation_hash(
    snapshot_hash: &str,
    applied: &[SourceRef],
    links: &[InstructionObservationLink],
) -> Result<String, String> {
    #[derive(Serialize)]
    struct PresentationBinding<'a> {
        contract: &'static str,
        snapshot_hash: &'a str,
        applied: &'a [SourceRef],
        links: &'a [InstructionObservationLink],
    }

    crate::compaction_candidate::digest(&PresentationBinding {
        contract: "rencrow-instruction-presentation-v1",
        snapshot_hash,
        applied,
        links,
    })
}

/// Collect Human instructions and explicitly linked, host-verified completion candidates.
pub fn collect_instruction_candidates(
    input: &CandidateInput,
    pruning: &InstructionPruning,
    links: &[InstructionObservationLink],
) -> Result<Option<serde_json::Value>, String> {
    let snapshot = input.snapshot()?;
    let checked_pruning = prune_known_obsolete(input, &pruning.applied)?;
    if checked_pruning != *pruning {
        return Err("instruction pruning projection does not match its source references".into());
    }
    let record_indices = input
        .records
        .iter()
        .enumerate()
        .map(|(index, record)| (record.id.as_str(), index))
        .collect::<HashMap<_, _>>();
    let mut human_sources = Vec::new();
    let mut candidate_humans = HashMap::<String, bool>::new();

    for record in input
        .records
        .iter()
        .filter(|record| record.origin == Origin::Human)
    {
        let text = pruning
            .retained_human_text
            .get(&record.id)
            .unwrap_or(&record.text);
        let deletions = pruning
            .applied
            .iter()
            .filter(|reference| reference.id == record.id)
            .map(|reference| reference.range.start..reference.range.end)
            .collect::<Vec<_>>();
        let rebased_ranges = record
            .protected
            .iter()
            .map(|range| ByteRange {
                start: rebase_offset(range.start, &deletions),
                end: rebase_offset(range.end, &deletions),
            })
            .collect::<Vec<_>>();
        let protected = rebased_ranges
            .iter()
            .map(|range| serde_json::json!({"start": range.start, "end": range.end}))
            .collect::<Vec<_>>();
        let is_candidate = !fully_protected(text, &rebased_ranges, record.opaque.is_some());
        candidate_humans.insert(record.id.clone(), is_candidate);
        human_sources.push(serde_json::json!({
            "id": record.id,
            "origin": "human",
            "role": record.role,
            "text": text,
            "protected": protected,
            "has_opaque": record.opaque.is_some(),
            "candidate": is_candidate,
        }));
    }

    let mut completion_links = Vec::new();
    let mut linked_instructions = HashSet::new();
    let mut linked_calls = HashSet::new();
    let mut linked_outputs = HashSet::new();
    let mut linked_terminal_pairs = HashSet::new();
    for link in links {
        if !linked_instructions.insert(link.instruction.id.as_str())
            || !linked_calls.insert(link.call.id.as_str())
            || !linked_outputs.insert(link.output.id.as_str())
            || !linked_terminal_pairs.insert((
                link.terminal.thread_id.as_str(),
                link.terminal.call_id.as_str(),
            ))
        {
            return Err("linked instruction or terminal pair is reused".into());
        }
        let instruction_index =
            validate_link_reference(&snapshot, &link.instruction, &record_indices)?;
        let call_index = validate_link_reference(&snapshot, &link.call, &record_indices)?;
        let output_index = validate_link_reference(&snapshot, &link.output, &record_indices)?;
        if !(instruction_index < call_index && call_index < output_index) {
            return Err("linked instruction and observation sources are out of order".into());
        }
        let instruction = &input.records[instruction_index];
        let call = &input.records[call_index];
        let output = &input.records[output_index];

        if instruction.origin != Origin::Human
            || instruction.role != "user"
            || instruction.opaque.is_some()
            || pruning
                .applied
                .iter()
                .any(|reference| reference.id == instruction.id)
            || !candidate_humans
                .get(&instruction.id)
                .copied()
                .unwrap_or(false)
        {
            return Err("linked instruction is not an unpruned Human candidate".into());
        }
        if overlaps_protected(&link.instruction.range, &instruction.protected) {
            return Err("linked instruction range is protected".into());
        }
        if call.origin != Origin::Work
            || call.execution_evidence
            || call.opaque.is_some()
            || !call.protected.is_empty()
            || output.origin != Origin::Work
            || !output.execution_evidence
            || output.opaque.is_some()
            || !output.protected.is_empty()
        {
            return Err("linked call/output do not have ordinary work provenance".into());
        }
        if !is_full_bounded_text(&link.call, &call.text)
            || !is_full_bounded_text(&link.output, &output.text)
        {
            return Err("linked call/output must be full text between 1 and 2048 bytes".into());
        }
        let terminal = &link.terminal;
        if terminal.version != ARCHIVE_REFERENCE_VERSION
            || terminal.thread_id.trim().is_empty()
            || terminal.call_id.trim().is_empty()
            || !terminal.validates_terminal()
            || terminal.original_content_sha256 != content_sha256(&output.text)
        {
            return Err(
                "linked terminal reference is invalid or does not match output text".into(),
            );
        }

        let Some(call_text) = call.text.get(link.call.range.start..link.call.range.end) else {
            return Err("linked call range is not valid UTF-8 text".into());
        };
        let Some(output_text) = output
            .text
            .get(link.output.range.start..link.output.range.end)
        else {
            return Err("linked output range is not valid UTF-8 text".into());
        };
        let terminal_status = match terminal.status {
            ArchiveTerminalStatus::Completed => "completed",
            ArchiveTerminalStatus::Failed => "failed",
        };
        completion_links.push(serde_json::json!({
            "instruction_id": instruction.id,
            "instruction_range": link.instruction.range,
            "call_source_id": call.id,
            "output_source_id": output.id,
            "tool_call_id": terminal.call_id,
            "call_text": call_text,
            "output_text": output_text,
            "terminal_status": terminal_status,
            "terminal_exit_code": terminal.exit_code,
            "candidate_only": true,
        }));
    }

    if !candidate_humans.values().any(|is_candidate| *is_candidate) {
        return Ok(None);
    }

    let snapshot_hash = snapshot.hash();
    let presentation_hash = instruction_presentation_hash(&snapshot_hash, &pruning.applied, links)?;
    Ok(Some(serde_json::json!({
        "schema_version": 1,
        "snapshot_hash": snapshot_hash,
        "presentation_hash": presentation_hash,
        "sources": human_sources,
        "completion_links": completion_links,
    })))
}

fn fully_protected(text: &str, protected: &[ByteRange], has_opaque: bool) -> bool {
    let text_len = text.len();
    if text_len == 0 || has_opaque {
        return true;
    }
    let mut protected = protected.to_vec();
    protected.sort_by_key(|range| (range.start, range.end));

    let mut covered_until = 0;
    for range in protected {
        if range.start > covered_until {
            return false;
        }
        covered_until = covered_until.max(range.end);
        if covered_until >= text_len {
            return true;
        }
    }
    false
}

fn validate_link_reference(
    snapshot: &crate::compaction_plan::CompactionSnapshot,
    reference: &SourceRef,
    record_indices: &HashMap<&str, usize>,
) -> Result<usize, String> {
    let index = record_indices
        .get(reference.id.as_str())
        .copied()
        .ok_or_else(|| "linked source ID is absent from the snapshot".to_owned())?;
    let checked = snapshot
        .reference(&reference.id, reference.range.clone())
        .map_err(|error| format!("invalid linked source reference: {error:?}"))?;
    if checked != *reference {
        return Err("linked source reference is stale".into());
    }
    Ok(index)
}

fn is_full_bounded_text(reference: &SourceRef, text: &str) -> bool {
    (1..=2_048).contains(&text.len())
        && reference.range.start == 0
        && reference.range.end == text.len()
}

fn overlaps_protected(range: &ByteRange, protected: &[ByteRange]) -> bool {
    protected
        .iter()
        .any(|span| range.start < span.end && span.start < range.end)
}

fn rebase_offset(offset: usize, deletions: &[Range<usize>]) -> usize {
    offset
        - deletions
            .iter()
            .filter(|range| range.end <= offset)
            .map(|range| range.end - range.start)
            .sum::<usize>()
}

#[cfg(test)]
#[path = "compaction_preprocess_tests.rs"]
mod tests;
