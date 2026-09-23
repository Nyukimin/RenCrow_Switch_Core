//! Pure summary-history projection and staged-output validation for V2 compaction.

use super::native::NativeProjection;
use codex_history::ResponseItemEnvelope;
use codex_history::archive_reference::ObservationReference;
use codex_history::compaction_candidate::CandidateInput;
use codex_history::observation_projection::ObservationSummaryExcerpt;
use codex_protocol::models::ResponseItem;

/// Intentional TDD stub: RED tests require valid V2 projections to be accepted.
#[allow(dead_code)]
pub(super) fn build_summary_history(
    _originals: &[ResponseItemEnvelope],
    _input: &CandidateInput,
    _native: &NativeProjection,
    _observations: &[(usize, usize, ObservationSummaryExcerpt)],
    _adopted_summary_index: Option<usize>,
) -> Result<Vec<ResponseItemEnvelope>, String> {
    Err("V2 summary history projection is not implemented".into())
}

/// Intentional TDD stub: RED tests require supported assistant output to be accepted.
#[allow(dead_code)]
pub(super) fn summary_suffix_from_staged_output(
    _output: &[ResponseItem],
) -> Result<String, String> {
    Err("V2 staged summary validation is not implemented".into())
}

/// Intentional TDD stub: RED tests require explicit valid reference markers to resolve.
#[allow(dead_code)]
pub(super) fn important_refs_from_summary(
    _summary_text: &str,
    _inventory: &[ObservationReference],
) -> Result<Vec<ObservationReference>, String> {
    Err("V2 important observation reference parsing is not implemented".into())
}

/// Intentional TDD stub for the drain's ServerReasoningIncluded dispatch policy.
#[allow(dead_code)]
pub(crate) fn should_apply_server_reasoning_included(_rencrow_compaction: bool) -> bool {
    true
}
