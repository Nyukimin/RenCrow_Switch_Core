// Modified by RenCrow Switch Core, 2026-09-22.
//! Read-only candidate production. No session or checkpoint mutation lives here.
use crate::compaction_plan::*;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeSet;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    Human,
    Host,
    Work,
    Unknown,
}

/// Input supplied by a trusted collector, not by the compaction model.
/// `human` requires intake evidence; missing provenance remains unknown.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateRecord {
    pub id: String,
    pub origin: Origin,
    pub intake_ref: Option<String>,
    pub scope: String,
    pub role: String,
    pub text: String,
    pub protected: Vec<ByteRange>,
    pub execution_evidence: bool,
    pub opaque: Option<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateInput {
    pub version: u32,
    pub binding: String,
    pub records: Vec<CandidateRecord>,
    pub current_context: Vec<Value>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkSummary {
    pub view_hash: String,
    pub text: String,
    pub source_ids: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SummaryReview {
    pub summary_hash: String,
    pub accepted: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateBundle {
    pub version: u32,
    pub input_hash: String,
    pub plan: CompactionPlan,
    pub plan_review: SemanticReview,
    pub summary: WorkSummary,
    pub summary_review: SummaryReview,
    pub model: String,
    pub effort: String,
    pub responses: Vec<Value>,
}

pub fn digest(value: &impl Serialize) -> Result<String, String> {
    let data = serde_json::to_vec(value).map_err(|_| "serialization failed")?;
    Ok(format!("{:x}", Sha256::digest(data)))
}

impl CandidateInput {
    pub fn snapshot(&self) -> Result<CompactionSnapshot, String> {
        if self.version != 1 {
            return Err("unsupported input version".into());
        }
        if self.records.iter().any(|r| r.origin == Origin::Host) && self.current_context.is_empty()
        {
            return Err("host context requires a current owner context".into());
        }
        let mut fragments = Vec::new();
        for record in &self.records {
            if record.origin == Origin::Human
                && (record.role != "user"
                    || record
                        .intake_ref
                        .as_ref()
                        .is_none_or(|r| r.trim().is_empty()))
            {
                return Err("human origin requires user role and trusted intake reference".into());
            }
            if record.execution_evidence && record.origin != Origin::Work {
                return Err("execution evidence must have work provenance".into());
            }
            let kind = if record.origin == Origin::Human {
                SourceKind::UserInstruction
            } else if record.execution_evidence {
                SourceKind::ExecutionEvidence
            } else {
                SourceKind::Context
            };
            let mut protected = record.protected.clone();
            // Attachments and unknown origin must survive unchanged, including associated text.
            if (record.opaque.is_some() || record.origin == Origin::Unknown)
                && !record.text.is_empty()
            {
                protected.push(ByteRange {
                    start: 0,
                    end: record.text.len(),
                });
            }
            fragments.push(SourceFragment {
                id: record.id.clone(),
                scope: record.scope.clone(),
                kind,
                text: record.text.clone(),
                protected,
            });
        }
        CompactionSnapshot::capture(digest(self)?, fragments).map_err(|e| format!("{e:?}"))
    }

    pub fn proposal_input(&self) -> Result<Value, String> {
        let snapshot = self.snapshot()?;
        let mut sources = Vec::new();
        for record in &self.records {
            if record.text.is_empty() {
                continue;
            }
            let reference = snapshot
                .reference(
                    &record.id,
                    ByteRange {
                        start: 0,
                        end: record.text.len(),
                    },
                )
                .map_err(|e| format!("{e:?}"))?;
            sources.push(serde_json::json!({"source":reference,"origin":record.origin,"scope":record.scope,"text":record.text,"protected":record.protected,"has_opaque":record.opaque.is_some(),"execution_evidence":record.execution_evidence}));
        }
        Ok(
            serde_json::json!({"schema_version":1,"snapshot_hash":snapshot.hash(),"sources":sources}),
        )
    }

    pub fn view(
        &self,
        plan: &CompactionPlan,
        review: &SemanticReview,
    ) -> Result<CompactionView, String> {
        self.snapshot()?
            .apply(plan, review)
            .map_err(|e| format!("{e:?}"))
    }

    /// One selected view provides both the summary request and retained human text.
    pub fn summary_input(&self, view: &CompactionView) -> Result<Value, String> {
        let work_ids: Vec<_> = self
            .records
            .iter()
            .filter(|r| r.origin == Origin::Work)
            .map(|r| r.id.clone())
            .collect();
        Ok(serde_json::json!({"view_hash":digest(view)?,"view":view,"work_source_ids":work_ids}))
    }

    pub fn assemble(&self, bundle: &CandidateBundle) -> Result<Value, String> {
        if bundle.version != 1 || bundle.input_hash != digest(self)? {
            return Err("stale candidate".into());
        }
        let view = self.view(&bundle.plan, &bundle.plan_review)?;
        if bundle.summary.view_hash != digest(&view)?
            || bundle.summary_review.summary_hash != digest(&bundle.summary)?
            || !bundle.summary_review.accepted
        {
            return Err("summary validation failed".into());
        }
        if bundle.summary.text.trim().is_empty() {
            return Err("empty summary".into());
        }
        let expected: BTreeSet<_> = self
            .records
            .iter()
            .filter(|r| r.origin == Origin::Work)
            .map(|r| &r.id)
            .collect();
        let actual: BTreeSet<_> = bundle.summary.source_ids.iter().collect();
        if actual != expected || actual.len() != bundle.summary.source_ids.len() {
            return Err("summary source coverage mismatch".into());
        }
        let mut human = Vec::new();
        let mut protected = Vec::new();
        for (original, selected) in self.records.iter().zip(&view.retained) {
            match original.origin {
                Origin::Human => {
                    if !selected.text.is_empty() || original.opaque.is_some() {
                        human.push(serde_json::json!({"id":original.id,"intake_ref":original.intake_ref,"role":original.role,"text":selected.text,"opaque":original.opaque}));
                    }
                }
                Origin::Unknown => protected
                    .push(serde_json::to_value(original).map_err(|_| "serialization failed")?),
                Origin::Host | Origin::Work
                    if original.opaque.is_some() || !original.protected.is_empty() =>
                {
                    protected
                        .push(serde_json::to_value(original).map_err(|_| "serialization failed")?)
                }
                Origin::Host | Origin::Work => {}
            }
        }
        Ok(
            serde_json::json!({"version":1,"input_hash":bundle.input_hash,"view_hash":digest(&view)?,"current_context":self.current_context,"human_input":human,"work_summary":bundle.summary,"completed_results":view.results,"protected":protected,"selection":view.applied_operations,"unresolved":view.unresolved_operations}),
        )
    }
}

#[cfg(test)]
#[path = "compaction_candidate_tests.rs"]
mod tests;
