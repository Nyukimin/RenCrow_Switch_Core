// Modified by RenCrow Switch Core, 2026-09-22.
//! Pure validation for derived compaction views. This module never edits a rollout.
//!
//! The host supplies provenance and protected ranges. Models propose operations;
//! a separate semantic review must agree on the exact plan before application.
//! Structural validation is not proof that a semantic judgment is correct.

use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeSet;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    UserInstruction,
    ExecutionEvidence,
    Context,
}

/// A host-authored classification, never accepted from a model's proposal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SourceFragment {
    pub id: String,
    pub scope: String,
    pub kind: SourceKind,
    pub text: String,
    /// Protected spans use UTF-8 byte offsets within this fragment.
    pub protected: Vec<ByteRange>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ByteRange {
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceRef {
    pub id: String,
    pub hash: String,
    pub range: ByteRange,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    Keep {
        source: SourceRef,
    },
    DropSuperseded {
        source: SourceRef,
        correction: SourceRef,
    },
    ReplaceCompleted {
        source: SourceRef,
        evidence: SourceRef,
        result: String,
    },
}

impl Operation {
    fn source(&self) -> &SourceRef {
        match self {
            Self::Keep { source }
            | Self::DropSuperseded { source, .. }
            | Self::ReplaceCompleted { source, .. } => source,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionPlan {
    pub schema_version: u32,
    pub snapshot_hash: String,
    pub operations: Vec<Operation>,
}

/// A model review is evidence, not authorization to override host protections.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticReview {
    pub plan_hash: String,
    pub accepted_operations: Vec<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlanError {
    InvalidSnapshot,
    InvalidSchema,
    StaleSnapshot,
    StaleReview,
    InvalidReview,
    UnknownReference,
    StaleReference,
    InvalidRange,
    OverlappingOperations,
    ProtectedSource,
    InvalidEvidence,
    RemovedEvidence,
    InvalidResult,
    Serialization,
}

/// Snapshot binding includes the host's session, turn, config and checkpoint identity.
/// Equality of this hash does not lock a live session; the committing host must
/// compare its current revision again under its own persistence boundary.
#[derive(Clone, Debug, Serialize)]
pub struct CompactionSnapshot {
    binding: String,
    fragments: Vec<SourceFragment>,
    hash: String,
}

/// Original text remaining after selection; generated results stay separate so
/// callers cannot accidentally present a model-written result as user testimony.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RetainedFragment {
    pub id: String,
    pub kind: SourceKind,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DerivedResult {
    pub source: SourceRef,
    pub evidence: SourceRef,
    pub text: String,
}

/// One view is to be consumed by both summary input and replacement construction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CompactionView {
    pub snapshot_hash: String,
    pub plan_hash: String,
    pub retained: Vec<RetainedFragment>,
    pub results: Vec<DerivedResult>,
    pub applied_operations: Vec<usize>,
    pub unresolved_operations: Vec<usize>,
}

fn hash(value: &impl Serialize) -> Result<String, PlanError> {
    let bytes = serde_json::to_vec(value).map_err(|_| PlanError::Serialization)?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn valid_range(text: &str, range: &ByteRange) -> bool {
    range.start < range.end && text.get(range.start..range.end).is_some()
}

fn overlaps(a: &ByteRange, b: &ByteRange) -> bool {
    a.start < b.end && b.start < a.end
}

impl CompactionPlan {
    pub fn hash(&self) -> Result<String, PlanError> {
        hash(self)
    }
}

impl CompactionSnapshot {
    pub fn capture(binding: String, fragments: Vec<SourceFragment>) -> Result<Self, PlanError> {
        let mut ids = BTreeSet::new();
        if binding.trim().is_empty()
            || fragments.iter().any(|fragment| {
                fragment.id.trim().is_empty()
                    || fragment.scope.trim().is_empty()
                    || !ids.insert(&fragment.id)
                    || fragment
                        .protected
                        .iter()
                        .any(|range| !valid_range(&fragment.text, range))
            })
        {
            return Err(PlanError::InvalidSnapshot);
        }
        let hash = hash(&(&binding, &fragments))?;
        Ok(Self {
            binding,
            fragments,
            hash,
        })
    }

    pub fn hash(&self) -> &str {
        &self.hash
    }

    pub fn reference(&self, id: &str, range: ByteRange) -> Result<SourceRef, PlanError> {
        let fragment = self
            .fragments
            .iter()
            .find(|f| f.id == id)
            .ok_or(PlanError::UnknownReference)?;
        if !valid_range(&fragment.text, &range) {
            return Err(PlanError::InvalidRange);
        }
        Ok(SourceRef {
            id: id.to_owned(),
            hash: hash(fragment)?,
            range,
        })
    }

    fn resolve(&self, reference: &SourceRef) -> Result<(usize, &SourceFragment), PlanError> {
        let (index, fragment) = self
            .fragments
            .iter()
            .enumerate()
            .find(|(_, f)| f.id == reference.id)
            .ok_or(PlanError::UnknownReference)?;
        if hash(fragment)? != reference.hash {
            return Err(PlanError::StaleReference);
        }
        if !valid_range(&fragment.text, &reference.range) {
            return Err(PlanError::InvalidRange);
        }
        Ok((index, fragment))
    }

    pub fn apply(
        &self,
        plan: &CompactionPlan,
        review: &SemanticReview,
    ) -> Result<CompactionView, PlanError> {
        if plan.schema_version != 1 {
            return Err(PlanError::InvalidSchema);
        }
        if self.hash != plan.snapshot_hash {
            return Err(PlanError::StaleSnapshot);
        }
        let plan_hash = plan.hash()?;
        if review.plan_hash != plan_hash {
            return Err(PlanError::StaleReview);
        }
        let accepted: BTreeSet<_> = review.accepted_operations.iter().copied().collect();
        if accepted.len() != review.accepted_operations.len()
            || accepted.iter().any(|i| *i >= plan.operations.len())
        {
            return Err(PlanError::InvalidReview);
        }
        let mut sources: Vec<&SourceRef> = Vec::new();
        let mut removed: Vec<&SourceRef> = Vec::new();
        let mut witnesses: Vec<&SourceRef> = Vec::new();
        let mut results = Vec::new();
        let mut applied = Vec::new();
        let mut unresolved = Vec::new();
        for (i, operation) in plan.operations.iter().enumerate() {
            let source = operation.source();
            let (source_index, fragment) = self.resolve(source)?;
            if sources
                .iter()
                .any(|other| other.id == source.id && overlaps(&other.range, &source.range))
            {
                return Err(PlanError::OverlappingOperations);
            }
            sources.push(source);
            if matches!(operation, Operation::Keep { .. }) {
                continue;
            }
            if fragment.kind != SourceKind::UserInstruction
                || fragment
                    .protected
                    .iter()
                    .any(|p| overlaps(p, &source.range))
            {
                return Err(PlanError::ProtectedSource);
            }
            let evidence = match operation {
                Operation::DropSuperseded { correction, .. } => {
                    let (index, correcting) = self.resolve(correction)?;
                    if index < source_index
                        || (index == source_index && correction.range.start < source.range.end)
                        || correcting.kind != SourceKind::UserInstruction
                        || correcting.scope != fragment.scope
                    {
                        return Err(PlanError::InvalidEvidence);
                    }
                    correction
                }
                Operation::ReplaceCompleted {
                    evidence, result, ..
                } => {
                    let (index, proving) = self.resolve(evidence)?;
                    if index <= source_index
                        || proving.kind != SourceKind::ExecutionEvidence
                        || proving.scope != fragment.scope
                    {
                        return Err(PlanError::InvalidEvidence);
                    }
                    if result.trim().is_empty() {
                        return Err(PlanError::InvalidResult);
                    }
                    evidence
                }
                Operation::Keep { .. } => unreachable!(),
            };
            if !accepted.contains(&i) {
                unresolved.push(i);
                continue;
            }
            removed.push(source);
            witnesses.push(evidence);
            applied.push(i);
            if let Operation::ReplaceCompleted {
                evidence, result, ..
            } = operation
            {
                results.push(DerivedResult {
                    source: source.clone(),
                    evidence: evidence.clone(),
                    text: result.clone(),
                });
            }
        }
        if witnesses.iter().any(|w| {
            removed
                .iter()
                .any(|r| w.id == r.id && overlaps(&w.range, &r.range))
        }) {
            return Err(PlanError::RemovedEvidence);
        }
        let retained = self
            .fragments
            .iter()
            .map(|fragment| {
                let mut text = fragment.text.clone();
                let mut ranges: Vec<_> = removed
                    .iter()
                    .filter(|r| r.id == fragment.id)
                    .map(|r| &r.range)
                    .collect();
                ranges.sort_by_key(|r| std::cmp::Reverse(r.start));
                for range in ranges {
                    text.replace_range(range.start..range.end, "");
                }
                RetainedFragment {
                    id: fragment.id.clone(),
                    kind: fragment.kind.clone(),
                    text,
                }
            })
            .collect();
        Ok(CompactionView {
            snapshot_hash: self.hash.clone(),
            plan_hash,
            retained,
            results,
            applied_operations: applied,
            unresolved_operations: unresolved,
        })
    }
}

#[cfg(test)]
#[path = "compaction_plan_tests.rs"]
mod tests;
