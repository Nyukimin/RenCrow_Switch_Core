//! Host validation for V2 instruction-selection proposals.

use crate::compaction_candidate::CandidateInput;
use crate::compaction_candidate::Origin;
use crate::compaction_pipeline::InstructionSelection;
use crate::compaction_pipeline::ProposedOperation;
use crate::compaction_pipeline::bind_plan;
use crate::compaction_plan::CompactionPlan;
use crate::compaction_plan::DerivedResult;
use crate::compaction_plan::Operation;
use crate::compaction_plan::SourceRef;
use crate::compaction_plan::ValidatedPlanStructure;
use crate::compaction_preprocess::InstructionObservationLink;
use crate::compaction_preprocess::InstructionPruning;
use crate::compaction_preprocess::collect_instruction_candidates;
use crate::compaction_preprocess::prune_known_obsolete;
use std::collections::BTreeSet;

/// The common result needed by summary projection and native retained-input construction.
/// It contains no copied retained Work records and no semantic-review receipt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstructionSelectionApplication {
    pub pruning: InstructionPruning,
    pub plan_hash: Option<String>,
    pub results: Vec<DerivedResult>,
}

/// Revalidate the exact presentation shown to the selector, bind its proposal to the original
/// snapshot, and return one exclusion map for both summary and retained native input.
pub fn validate_and_apply_selection(
    input: &CandidateInput,
    pruning: &InstructionPruning,
    links: &[InstructionObservationLink],
    selection: Option<InstructionSelection>,
) -> Result<InstructionSelectionApplication, String> {
    let presented = collect_instruction_candidates(input, pruning, links)?;
    let Some(selection) = selection else {
        return Ok(InstructionSelectionApplication {
            pruning: pruning.clone(),
            plan_hash: None,
            results: Vec::new(),
        });
    };
    let presented =
        presented.ok_or_else(|| "selection supplied without eligible candidates".to_owned())?;
    if presented["snapshot_hash"].as_str() != Some(selection.snapshot_hash()) {
        return Err("instruction selection is bound to a stale snapshot".into());
    }
    if presented["presentation_hash"].as_str() != Some(selection.presentation_hash()) {
        return Err("instruction selection is bound to a different presentation".into());
    }

    for operation in &selection.proposed().operations {
        if let ProposedOperation::DropSuperseded {
            correction_text, ..
        } = operation
            && correction_text
                .as_deref()
                .is_none_or(|text| text.trim().is_empty())
        {
            return Err("V2 supersession requires nonblank exact correction_text".into());
        }
    }

    let plan = bind_plan(input, selection.proposed().clone())?;
    let snapshot = input.snapshot()?;
    let plan_hash = snapshot
        .validate_plan_header(&plan)
        .map_err(|error| format!("{error:?}"))?;
    let accepted: BTreeSet<_> = (0..plan.operations.len()).collect();
    let structure = snapshot
        .validate_plan_structure(&plan, &accepted, plan_hash)
        .map_err(|error| format!("{error:?}"))?;

    validate_presented_operations(
        input,
        pruning,
        links,
        selection.proposed().operations.as_slice(),
        &plan,
    )?;
    validate_witnesses_against_prior(pruning, &structure)?;

    let mut unified_refs = pruning.applied.clone();
    unified_refs.extend(structure.removed.iter().cloned());
    let unified = prune_known_obsolete(input, &unified_refs)?;
    Ok(InstructionSelectionApplication {
        pruning: unified,
        plan_hash: Some(structure.plan_hash),
        results: structure.results,
    })
}

fn validate_presented_operations(
    input: &CandidateInput,
    pruning: &InstructionPruning,
    links: &[InstructionObservationLink],
    proposed: &[ProposedOperation],
    plan: &CompactionPlan,
) -> Result<(), String> {
    if proposed.len() != plan.operations.len() {
        return Err("bound plan does not preserve proposal operation order".into());
    }
    for (proposal, operation) in proposed.iter().zip(&plan.operations) {
        match (proposal, operation) {
            (
                ProposedOperation::DropSuperseded {
                    source_text,
                    correction_text: Some(_),
                    ..
                },
                Operation::DropSuperseded { source, correction },
            ) => {
                validate_human_reference(input, pruning, source, source_text.is_none())?;
                validate_human_reference(input, pruning, correction, false)?;
            }
            (
                ProposedOperation::ReplaceCompleted { source_text, .. },
                Operation::ReplaceCompleted {
                    source, evidence, ..
                },
            ) => {
                validate_human_reference(input, pruning, source, source_text.is_none())?;
                let link = links
                    .iter()
                    .find(|link| {
                        link.instruction.id == source.id
                            && link.instruction.hash == source.hash
                            && contains(&link.instruction.range, &source.range)
                    })
                    .ok_or_else(|| {
                        "completion source is not contained in an explicit link".to_owned()
                    })?;
                if evidence != &link.output {
                    return Err(
                        "completion evidence must equal its linked full output reference".into(),
                    );
                }
            }
            _ => return Err("proposal and bound plan operations do not match".into()),
        }
    }
    Ok(())
}

fn validate_human_reference(
    input: &CandidateInput,
    pruning: &InstructionPruning,
    reference: &SourceRef,
    whole_record_requested: bool,
) -> Result<(), String> {
    let record = input
        .records
        .iter()
        .find(|record| record.id == reference.id)
        .ok_or_else(|| "selection references an unknown Human source".to_owned())?;
    if record.origin != Origin::Human || record.role != "user" || record.opaque.is_some() {
        return Err("selection source must be ordinary Human user text".into());
    }
    if whole_record_requested {
        if reference.range.start != 0
            || reference.range.end != record.text.len()
            || !record.protected.is_empty()
            || pruning.applied.iter().any(|known| known.id == reference.id)
        {
            return Err(
                "whole-record selection requires a fully shown, unprotected Human record".into(),
            );
        }
    }
    if pruning
        .applied
        .iter()
        .any(|known| known.id == reference.id && overlaps(&known.range, &reference.range))
    {
        return Err("selection reference crosses a previously removed range".into());
    }
    if record
        .protected
        .iter()
        .any(|protected| overlaps(protected, &reference.range))
    {
        return Err("selection reference overlaps protected Human content".into());
    }
    Ok(())
}

fn validate_witnesses_against_prior(
    pruning: &InstructionPruning,
    structure: &ValidatedPlanStructure,
) -> Result<(), String> {
    if structure.witnesses.iter().any(|witness| {
        pruning
            .applied
            .iter()
            .chain(&structure.removed)
            .any(|removed| removed.id == witness.id && overlaps(&removed.range, &witness.range))
    }) {
        return Err("an applied removal overlaps a correction or completion witness".into());
    }
    Ok(())
}

fn contains(
    outer: &crate::compaction_plan::ByteRange,
    inner: &crate::compaction_plan::ByteRange,
) -> bool {
    outer.start <= inner.start && inner.end <= outer.end
}

fn overlaps(a: &crate::compaction_plan::ByteRange, b: &crate::compaction_plan::ByteRange) -> bool {
    a.start < b.end && b.start < a.end
}

#[cfg(test)]
#[path = "compaction_selection_tests.rs"]
mod tests;
