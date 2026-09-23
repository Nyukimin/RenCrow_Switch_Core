// Modified by RenCrow Switch Core, 2026-09-22.
//! Read-only candidate production. No session or checkpoint mutation lives here.
use crate::compaction_plan::*;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeSet;

pub const SUMMARY_TEXT_MAX_BYTES: usize = 32_000;

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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prior_invalidations: Vec<String>,
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
    /// Host digest of the exact summary body; required for version 2 bundles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_hash: Option<String>,
    #[serde(default)]
    pub summary_review: Option<SummaryReview>,
    pub model: String,
    pub effort: String,
    pub responses: Vec<Value>,
}

pub fn digest(value: &impl Serialize) -> Result<String, String> {
    let data = serde_json::to_vec(value).map_err(|_| "serialization failed")?;
    Ok(format!("{:x}", Sha256::digest(data)))
}

/// Bind model items together with their non-model provenance using the history owner types.
pub fn history_digest(items: &[crate::ResponseItemEnvelope]) -> Result<String, String> {
    digest(
        &items
            .iter()
            .map(|item| (&item.item, &item.metadata))
            .collect::<Vec<_>>(),
    )
}

impl CandidateInput {
    /// Return the deterministic inventory of work records owned by this snapshot.
    pub fn work_source_ids(&self) -> Vec<String> {
        self.records
            .iter()
            .filter(|record| record.origin == Origin::Work)
            .map(|record| record.id.clone())
            .collect()
    }

    /// Bind summary text to the selected view and complete host-owned work inventory.
    pub fn bind_summary(&self, view: &CompactionView, text: String) -> Result<WorkSummary, String> {
        let snapshot = self.snapshot()?;
        if view.snapshot_hash != snapshot.hash() {
            return Err("stale compaction view".into());
        }
        if text.trim().is_empty() {
            return Err("empty summary".into());
        }
        if text.len() > SUMMARY_TEXT_MAX_BYTES {
            return Err("compaction summary exceeds the bounded checkpoint text limit".into());
        }
        Ok(WorkSummary {
            view_hash: digest(view)?,
            text,
            source_ids: self.work_source_ids(),
        })
    }

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

    /// Returns exact source passages that the current or an earlier accepted plan invalidated.
    ///
    /// The source range is resolved against the host-owned snapshot here rather than copied from
    /// model output. Prior passages are carried forward so a later summary cannot revive an older
    /// instruction.
    pub fn invalidations(
        &self,
        plan: &CompactionPlan,
        view: &CompactionView,
    ) -> Result<Vec<String>, String> {
        let snapshot = self.snapshot()?;
        if view.snapshot_hash != snapshot.hash() {
            return Err("stale compaction view".into());
        }
        if view.plan_hash != plan.hash().map_err(|error| format!("{error:?}"))? {
            return Err("compaction view does not match plan".into());
        }

        let mut seen = BTreeSet::new();
        let mut invalidations = Vec::new();
        let mut add = |passage: String| -> Result<(), String> {
            if passage.is_empty() {
                return Err("empty invalidation is not allowed".into());
            }
            if seen.insert(passage.clone()) {
                invalidations.push(passage);
            }
            Ok(())
        };

        for passage in &self.prior_invalidations {
            add(passage.clone())?;
        }
        for operation_index in &view.applied_operations {
            let operation = plan
                .operations
                .get(*operation_index)
                .ok_or_else(|| "compaction view references an unknown operation".to_owned())?;
            let source = match operation {
                Operation::DropSuperseded { source, .. }
                | Operation::ReplaceCompleted { source, .. } => source,
                Operation::Keep { .. } => {
                    return Err("compaction view marks a keep operation as applied".into());
                }
            };
            let checked = snapshot
                .reference(&source.id, source.range.clone())
                .map_err(|error| format!("{error:?}"))?;
            if &checked != source {
                return Err("stale invalidation reference".into());
            }
            let fragment = self
                .records
                .iter()
                .find(|record| record.id == source.id)
                .ok_or_else(|| "unknown invalidation source".to_owned())?;
            let passage = fragment
                .text
                .get(source.range.start..source.range.end)
                .ok_or_else(|| "compaction invalidation range is not valid UTF-8".to_owned())?;
            add(passage.to_owned())?;
        }
        Ok(invalidations)
    }

    /// One selected view provides both the summary request and retained human text.
    pub fn summary_input(
        &self,
        view: &CompactionView,
        plan: &CompactionPlan,
    ) -> Result<Value, String> {
        let invalidations = self.invalidations(plan, view)?;
        let mut summary_view = view.clone();
        let mut separately_retained_work = Vec::new();
        for retained in &mut summary_view.retained {
            let Some(record) = self.records.iter().find(|record| record.id == retained.id) else {
                return Err("summary view references an unknown record".into());
            };
            if record.origin == Origin::Work {
                // These records remain verbatim in the assembled context. Do not
                // ask the model to summarize a second copy of protected tool data.
                // Evidence used to replace a human request still needs full review.
                if record.opaque.is_some()
                    && !view
                        .results
                        .iter()
                        .any(|result| result.evidence.id == record.id)
                {
                    separately_retained_work.push(record.id.clone());
                    retained.text.clear();
                    continue;
                }
                for passage in &invalidations {
                    retained.text = retained.text.replace(passage, "");
                }
            }
        }
        Ok(serde_json::json!({
            "view_hash": digest(view)?,
            "view": summary_view,
            "separately_retained_work": separately_retained_work
        }))
    }

    /// Deleted passages belong only to independent validation, never generation.
    pub fn summary_review_input(
        &self,
        view: &CompactionView,
        plan: &CompactionPlan,
        summary: &WorkSummary,
    ) -> Result<Value, String> {
        // Generation must not see withdrawn passages again. The independent
        // reviewer still receives exact evidence for any completion replacement.
        let completion_evidence: Vec<_> = self
            .records
            .iter()
            .filter(|record| {
                view.results
                    .iter()
                    .any(|result| result.evidence.id == record.id)
            })
            .map(|record| serde_json::json!({"id":record.id,"text":record.text}))
            .collect();
        Ok(serde_json::json!({
            "input": self.summary_input(view, plan)?,
            "summary": {
                "view_hash": summary.view_hash,
                "text": summary.text
            },
            "completion_evidence": completion_evidence,
            "negative_validation": {
                "kind": "invalidated_source_passages",
                "passages": self.invalidations(plan, view)?,
                "instruction": "Audit only. Reject unnecessary repetition of withdrawn instructions or obsolete values, including historical annotations; completed factual results may remain."
            }
        }))
    }

    pub fn assemble(&self, bundle: &CandidateBundle) -> Result<Value, String> {
        if !matches!(bundle.version, 1 | 2) || bundle.input_hash != digest(self)? {
            return Err("stale candidate".into());
        }
        let view = self.view(&bundle.plan, &bundle.plan_review)?;
        let invalidations = self.invalidations(&bundle.plan, &view)?;
        if bundle.summary.view_hash != digest(&view)? {
            return Err("summary validation failed".into());
        }
        if bundle.summary.text.trim().is_empty() {
            return Err("empty summary".into());
        }
        if bundle.summary.text.len() > SUMMARY_TEXT_MAX_BYTES {
            return Err("compaction summary exceeds the bounded checkpoint text limit".into());
        }
        let summary_hash = digest(&bundle.summary)?;
        let summary_hash_valid = match bundle.version {
            1 => bundle
                .summary_hash
                .as_ref()
                .is_none_or(|bound| bound == &summary_hash),
            2 => bundle.summary_hash.as_deref() == Some(summary_hash.as_str()),
            _ => false,
        };
        let review_valid = bundle
            .summary_review
            .as_ref()
            .is_some_and(|review| review.summary_hash == summary_hash && review.accepted);
        if !summary_hash_valid {
            return Err("summary validation failed".into());
        }
        match (bundle.version, bundle.summary_review.as_ref()) {
            (1, Some(_)) if review_valid => {}
            (2, None) => {}
            (2, Some(_)) if review_valid => {}
            _ => return Err("summary validation failed".into()),
        }
        if invalidations
            .iter()
            .any(|passage| bundle.summary.text.contains(passage))
        {
            return Err("summary reintroduces an invalidated source passage".into());
        }
        let work_ids = self.work_source_ids();
        let expected: BTreeSet<_> = work_ids.iter().collect();
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
