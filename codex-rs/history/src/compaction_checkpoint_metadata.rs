//! Strict V2 provenance stored on a compacted checkpoint.
//!
//! This metadata carries host-owned selection and observation references only. It does not
//! embed model excerpts, raw observation bodies, or semantic-review claims.

use crate::ObservationReference;
use crate::archive_reference::content_sha256;
use crate::compaction_plan::DerivedResult;
use crate::compaction_plan::SourceRef;
use crate::observation_projection::ObservationCoverage;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::TokenUsage;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionSelectionMode {
    NoCandidates,
    ModelSelection,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointResponseStage {
    InstructionSelection,
    Summary,
}

/// The response receipt shape already produced by the compaction owner.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionModelResponseReceipt {
    pub stage: CheckpointResponseStage,
    pub response_id: String,
    pub seconds: f64,
    pub usage: Option<TokenUsage>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenCrowCompactionMetadataV2 {
    pub version: u32,
    pub snapshot_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presentation_hash: Option<String>,
    /// SHA-256 of the exact final summary message text, including its format prefix.
    /// Checkpoint validation must compare this value with the separately available message body.
    pub summary_hash: String,
    pub selection_mode: CompactionSelectionMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_hash: Option<String>,
    pub applied_refs: Vec<SourceRef>,
    pub results: Vec<DerivedResult>,
    pub observations: Vec<ObservationCoverage>,
    pub important_refs: Vec<ObservationReference>,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<ReasoningEffort>,
    pub responses: Vec<CompactionModelResponseReceipt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transaction_following_items: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub committed_transaction_hash: Option<String>,
}

impl RenCrowCompactionMetadataV2 {
    /// Parse and validate V2 checkpoint metadata against its owning thread and exact summary body.
    pub fn parse_and_validate(
        value: serde_json::Value,
        current_thread_id: &str,
        summary_text: &str,
    ) -> Result<Self, String> {
        let metadata: Self = serde_json::from_value(value)
            .map_err(|error| format!("invalid V2 compaction metadata: {error}"))?;
        metadata.validate_for_checkpoint(current_thread_id, summary_text)?;
        Ok(metadata)
    }

    /// Validate fields and bind the summary digest to the exact checkpoint message body.
    pub fn validate_for_checkpoint(
        &self,
        current_thread_id: &str,
        summary_text: &str,
    ) -> Result<(), String> {
        if self.version != 2 {
            return Err("unsupported RenCrow compaction metadata version".into());
        }
        if current_thread_id.trim().is_empty() {
            return Err("checkpoint thread ID must be nonempty".into());
        }
        if !is_sha256(&self.snapshot_hash) || !is_sha256(&self.summary_hash) {
            return Err("checkpoint snapshot or summary hash is invalid".into());
        }
        if content_sha256(summary_text) != self.summary_hash {
            return Err("checkpoint summary hash does not match its message body".into());
        }
        if self.model.trim().is_empty()
            || self
                .effort
                .as_ref()
                .is_some_and(|effort| effort.as_str().trim().is_empty())
        {
            return Err("checkpoint model and any specified effort must be nonempty".into());
        }

        let expected_stages = match self.selection_mode {
            CompactionSelectionMode::NoCandidates => {
                if self.presentation_hash.is_some()
                    || self.plan_hash.is_some()
                    || !self.results.is_empty()
                {
                    return Err(
                        "no-candidate checkpoint cannot carry selection presentation, plan, or results"
                            .into(),
                    );
                }
                vec![CheckpointResponseStage::Summary]
            }
            CompactionSelectionMode::ModelSelection => {
                let Some(presentation_hash) = self.presentation_hash.as_deref() else {
                    return Err("model selection checkpoint has no presentation hash".into());
                };
                let Some(plan_hash) = self.plan_hash.as_deref() else {
                    return Err("model selection checkpoint has no plan hash".into());
                };
                if !is_sha256(presentation_hash) || !is_sha256(plan_hash) {
                    return Err("checkpoint presentation or plan hash is invalid".into());
                }
                vec![
                    CheckpointResponseStage::InstructionSelection,
                    CheckpointResponseStage::Summary,
                ]
            }
        };
        if self.responses.len() != expected_stages.len()
            || self
                .responses
                .iter()
                .zip(expected_stages)
                .any(|(receipt, expected)| receipt.stage != expected)
        {
            return Err("checkpoint response receipts do not match its selection mode".into());
        }
        let mut response_ids = HashSet::with_capacity(self.responses.len());
        for receipt in &self.responses {
            if receipt.response_id.trim().is_empty()
                || !receipt.seconds.is_finite()
                || receipt.seconds < 0.0
                || !response_ids.insert(receipt.response_id.as_str())
            {
                return Err("checkpoint response receipt is invalid or duplicated".into());
            }
        }

        let mut applied_refs = HashSet::with_capacity(self.applied_refs.len());
        for reference in &self.applied_refs {
            validate_source_ref(reference)?;
            if !applied_refs.insert(source_ref_key(reference)) {
                return Err("checkpoint applied source references are duplicated".into());
            }
        }
        let mut result_sources = HashSet::with_capacity(self.results.len());
        for result in &self.results {
            validate_source_ref(&result.source)?;
            validate_source_ref(&result.evidence)?;
            if result.text.trim().is_empty()
                || !self.applied_refs.contains(&result.source)
                || !result_sources.insert(source_ref_key(&result.source))
            {
                return Err("checkpoint derived result is invalid or unbound".into());
            }
        }

        let mut observation_ids = HashSet::with_capacity(self.observations.len());
        let mut inventory_refs = HashSet::with_capacity(self.observations.len());
        for coverage in &self.observations {
            coverage.validate()?;
            let reference = &coverage.reference;
            if reference.thread_id != current_thread_id
                || !observation_ids.insert(reference.call_id.as_str())
            {
                return Err("checkpoint observation is cross-thread or duplicated".into());
            }
            inventory_refs.insert(observation_ref_key(reference));
        }
        let mut important_refs = HashSet::with_capacity(self.important_refs.len());
        for reference in &self.important_refs {
            let key = observation_ref_key(reference);
            if reference.thread_id != current_thread_id
                || !inventory_refs.contains(&key)
                || !important_refs.insert(key)
            {
                return Err("checkpoint important observation is absent or duplicated".into());
            }
        }

        if self.transaction_following_items.is_some() && self.committed_transaction_hash.is_some() {
            return Err("checkpoint cannot be both prepared and committed".into());
        }
        if self
            .committed_transaction_hash
            .as_deref()
            .is_some_and(|hash| !is_sha256(hash))
        {
            return Err("checkpoint committed transaction hash is invalid".into());
        }
        Ok(())
    }
}

fn validate_source_ref(reference: &SourceRef) -> Result<(), String> {
    if reference.id.trim().is_empty()
        || !is_sha256(&reference.hash)
        || reference.range.start >= reference.range.end
    {
        return Err("checkpoint source reference is invalid".into());
    }
    Ok(())
}

fn source_ref_key(reference: &SourceRef) -> (&str, &str, usize, usize) {
    (
        reference.id.as_str(),
        reference.hash.as_str(),
        reference.range.start,
        reference.range.end,
    )
}

fn observation_ref_key(reference: &ObservationReference) -> (&str, &str, &str) {
    (
        reference.thread_id.as_str(),
        reference.call_id.as_str(),
        reference.sha256.as_str(),
    )
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
#[path = "compaction_checkpoint_metadata_tests.rs"]
mod tests;
