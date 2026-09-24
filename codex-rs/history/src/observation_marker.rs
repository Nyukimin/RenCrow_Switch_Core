//! Deterministic V2 observation marker for a replaced live tool output (Part 2 §33–39).
//!
//! A marker keeps the original output item type and identity. Only the text body becomes this
//! host-generated JSON, and the output metadata gains the observation coverage. The body is
//! historical tool data, never an instruction. Both the body and the metadata are regenerated from
//! the canonical rollout observation, so a marker is verified by exact comparison.

use crate::CodexHarnessMetadata;
use crate::observation_projection::ObservationCoverage;
use crate::observation_projection::ObservationProjection;

pub const OBSERVATION_MARKER_INSTRUCTION: &str = "Archived historical tool data. Retrieve bounded source ranges before treating unpresented data as evidence.";

/// Render the marker body for one projected observation.
pub fn observation_marker_body(projection: &ObservationProjection) -> String {
    let output = &projection.summary.output;
    serde_json::json!({
        "rencrow_observation": true,
        "version": 2,
        "call_id": projection.coverage.reference.call_id,
        "tool": projection.coverage.tool_name,
        "sha256": output.coverage.sha256,
        "total_bytes": output.coverage.total_bytes,
        "partial": output.coverage.partial,
        "presented_ranges": output.coverage.presented_ranges,
        "excerpts": output.excerpts,
        "instruction": OBSERVATION_MARKER_INSTRUCTION,
    })
    .to_string()
}

/// Derive marker output metadata from the canonical output metadata.
///
/// The truncation budget described the replaced raw body and is dropped; the coverage is added.
/// Unrelated bookkeeping such as MCP attribution is kept unchanged.
pub fn observation_marker_metadata(
    canonical: Option<&CodexHarnessMetadata>,
    coverage: &ObservationCoverage,
) -> Result<CodexHarnessMetadata, String> {
    let mut metadata = canonical.cloned().unwrap_or_default();
    if metadata.rencrow_archive_reference.is_some()
        || metadata.rencrow_observation_projection.is_some()
        || metadata.rencrow_input.is_some()
        || metadata.rencrow_compaction.is_some()
    {
        return Err("observation marker source output carries RenCrow provenance".into());
    }
    metadata.history_truncation_token_limit = None;
    metadata.rencrow_observation_projection = Some(coverage.clone());
    Ok(metadata)
}

/// Check a live marker against the projection regenerated from its canonical source.
pub fn verify_observation_marker(
    regenerated: &ObservationProjection,
    canonical_metadata: Option<&CodexHarnessMetadata>,
    marker_metadata: Option<&CodexHarnessMetadata>,
    marker_body: &str,
) -> Result<(), String> {
    regenerated.validate()?;
    let expected = observation_marker_metadata(canonical_metadata, &regenerated.coverage)?;
    if marker_metadata != Some(&expected) {
        return Err("V2 observation marker metadata does not match its canonical source".into());
    }
    if marker_body != observation_marker_body(regenerated) {
        return Err("V2 observation marker body does not match its canonical source".into());
    }
    Ok(())
}

#[cfg(test)]
#[path = "observation_marker_tests.rs"]
mod tests;
