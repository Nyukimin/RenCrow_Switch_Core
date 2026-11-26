// Modified by RenCrow Switch Core, 2026-11-26: bounded common-harness IO evidence.
//! Bounded filesystem and Go test-result observations for the common harness.
//!
//! This module records evidence only. It never mutates task-check receipts.

use anyhow::Result;
use codex_apply_patch::ApplyPatchAction;
use serde::Deserialize;
use serde::Serialize;
use std::path::Path;
use std::path::PathBuf;

mod artifact;
mod dependency_subset;
mod go_json;
mod patch;

pub(crate) use dependency_subset::DependencySubsetComparisonV1;
pub(crate) use dependency_subset::compare_dependency_subset;

const MAX_ARTIFACT_FILES: usize = 16;
const MAX_ARTIFACT_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ArtifactSetV1 {
    pub root: PathBuf,
    pub binding: ArtifactBindingV1,
    pub entries: Vec<ArtifactEntryV1>,
    pub digest: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ArtifactBindingV1 {
    pub workspace_binding_ref: String,
    pub scope_revision: u64,
    pub git_commit_ref: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ArtifactEntryV1 {
    pub path: PathBuf,
    pub present: bool,
    pub sha256: Option<String>,
    pub executable: bool,
}

impl ArtifactSetV1 {
    /// Captures explicitly named files under the owner-provided workspace binding and scope.
    pub(crate) fn capture_bound(
        root: &Path,
        paths: &[PathBuf],
        binding: &ArtifactBindingV1,
    ) -> Result<Self> {
        artifact::capture_bound(root, paths, binding)
    }

    /// Returns whether a prior artifact snapshot still matches the workspace.
    pub(crate) fn matches_workspace(&self) -> Result<bool> {
        artifact::matches_workspace(self)
    }

    /// Returns the exact canonical JSON bytes used as the v1 artifact fingerprint input.
    #[cfg(test)]
    pub(crate) fn canonical_fingerprint_json(
        binding: &ArtifactBindingV1,
        entries: &[ArtifactEntryV1],
    ) -> Result<String> {
        artifact::canonical_fingerprint_json(binding, entries)
    }
}

impl ArtifactBindingV1 {
    /// Uses a canonical workspace root as the local binding for standalone callers.
    pub(crate) fn for_canonical_root(root: &Path) -> Result<Self> {
        artifact::for_canonical_root(root)
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PatchJournalBindingV1 {
    pub task_id: String,
    pub request_revision: u64,
    pub attempt_no: u32,
    pub owner_epoch: u64,
    pub expected_state_version: u64,
    pub operation_id: String,
    pub workspace_root: PathBuf,
    pub artifact_binding: ArtifactBindingV1,
    pub artifact_paths: Vec<PathBuf>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PatchJournalIntentV1 {
    pub binding: PatchJournalBindingV1,
    pub before: ArtifactSetV1,
    pub predicted_after: ArtifactSetV1,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PatchReconciliationV1 {
    AllPre,
    AllPost,
    PartialOrOther,
}

/// Builds bounded preimages and predicted postimages without applying the patch.
pub(crate) fn prepare_patch_journal(
    binding: &PatchJournalBindingV1,
    action: &ApplyPatchAction,
) -> Result<PatchJournalIntentV1> {
    patch::prepare_patch_journal(binding, action)
}

/// Reconciles a durable intent against the current workspace without writing files.
pub(crate) fn reconcile_patch_journal(
    intent: &PatchJournalIntentV1,
) -> Result<PatchReconciliationV1> {
    patch::reconcile_patch_journal(intent)
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GoTestExpectation {
    pub selected_cases: Vec<String>,
    pub expected_case_count: usize,
    pub expected_baseline_failure: Option<ExpectedBaselineFailure>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExpectedBaselineFailure {
    pub case_name: String,
    pub reason: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GoTestVerdict {
    Pass,
    Fail,
    Skip,
    Error,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GoTestObservation {
    pub case_count: usize,
    pub selected_names: Vec<String>,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub launch_succeeded: bool,
    pub framework_error: bool,
    pub log_complete: bool,
    pub raw_exit_code: Option<i32>,
    pub verdict: GoTestVerdict,
    pub matched_expected_baseline_failure: bool,
}

/// Interprets a complete `go test -json` event stream without changing raw exit facts.
pub(crate) fn observe_go_test_json(
    stream: &[u8],
    launch_succeeded: bool,
    raw_exit_code: Option<i32>,
    expectation: &GoTestExpectation,
) -> Result<GoTestObservation> {
    go_json::observe_go_test_json(stream, launch_succeeded, raw_exit_code, expectation)
}

#[cfg(test)]
#[path = "common_harness_io_tests.rs"]
mod tests;
