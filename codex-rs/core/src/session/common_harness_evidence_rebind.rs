// Modified by RenCrow Switch Core, 2026-11-27: preserve proofs for unchanged declared dependencies.
use super::session::Session;
use super::turn_context::TurnContext;
use crate::common_harness_artifacts::load_operation_artifact;
use crate::common_harness_artifacts::write_operation_artifact;
use crate::common_harness_io::ArtifactSetV1;
use crate::common_harness_io::DependencySubsetComparisonV1;
use crate::common_harness_io::compare_dependency_subset;
use codex_history::task_context::CommonHarnessStateV1;
use codex_history::task_context::ConditionEvidenceV1;
use codex_history::task_context::ConditionSpecV1;
use codex_history::task_context::ConditionVerdictV1;
use codex_history::task_context::EvidenceLifecycleV1;
use codex_history::task_context::HostEventV1;
use serde::Deserialize;
use serde::Serialize;
use std::path::PathBuf;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EvidenceComparisonProofV1 {
    format: String,
    condition_id: String,
    condition_revision: u64,
    condition_set_revision: u64,
    request_revision: u64,
    source_evidence_id: String,
    operation_id: String,
    execution_id: String,
    old_target_version: String,
    new_target_version: String,
    source_snapshot_ref: String,
    target_snapshot_ref: String,
    dependency_paths: Vec<PathBuf>,
}

fn dependencies(condition: &ConditionSpecV1) -> Vec<PathBuf> {
    condition
        .dependency_refs
        .iter()
        .chain(
            condition
                .check
                .iter()
                .flat_map(|check| check.cwd_binding_refs.iter()),
        )
        .map(PathBuf::from)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

impl Session {
    fn common_harness_evidence_snapshot(
        &self,
        turn: &TurnContext,
        common: &CommonHarnessStateV1,
        condition: &ConditionSpecV1,
        evidence: &ConditionEvidenceV1,
    ) -> Result<(ArtifactSetV1, String), String> {
        let home = turn.config.codex_home.as_path();
        if let Some(reference) = evidence.comparison_proof_ref.as_ref() {
            let proof: EvidenceComparisonProofV1 =
                load_operation_artifact(home, self.thread_id(), reference)?;
            if proof.format != "common-harness-evidence-comparison/1"
                || proof.condition_id != condition.condition_id
                || proof.condition_revision != condition.condition_revision
                || proof.condition_set_revision != common.request.condition_set_revision
                || proof.request_revision != common.request.request_revision
                || Some(proof.source_evidence_id.as_str())
                    != evidence.source_evidence_ref.as_deref()
                || proof.operation_id != evidence.operation_id
                || proof.execution_id != evidence.execution_id
                || proof.new_target_version != evidence.target_version
                || proof.dependency_paths != dependencies(condition)
            {
                return Err("common-harness dependency proof binding is invalid".into());
            }
            let before: ArtifactSetV1 =
                load_operation_artifact(home, self.thread_id(), &proof.source_snapshot_ref)?;
            let after: ArtifactSetV1 =
                load_operation_artifact(home, self.thread_id(), &proof.target_snapshot_ref)?;
            if before.digest != proof.old_target_version
                || after.digest != proof.new_target_version
                || compare_dependency_subset(&before, &after, &proof.dependency_paths)
                    .map_err(|error| format!("validate dependency comparison: {error}"))?
                    != DependencySubsetComparisonV1::Unchanged
            {
                return Err("common-harness dependency proof does not establish reuse".into());
            }
            return Ok((after, proof.target_snapshot_ref));
        }
        for reference in &evidence.artifact_refs {
            let Ok(snapshot) =
                load_operation_artifact::<ArtifactSetV1>(home, self.thread_id(), reference)
            else {
                continue;
            };
            if snapshot.digest == evidence.target_version {
                // Self-comparison also validates the immutable snapshot's shape and digest.
                compare_dependency_subset(&snapshot, &snapshot, &dependencies(condition))
                    .map_err(|error| format!("validate native evidence snapshot: {error}"))?;
                return Ok((snapshot, reference.clone()));
            }
        }
        Err("common-harness current evidence has no resolvable artifact snapshot".into())
    }

    pub(super) fn validate_common_harness_evidence_artifacts(
        &self,
        turn: &TurnContext,
        common: &CommonHarnessStateV1,
    ) -> Result<(), String> {
        for condition in common
            .request
            .conditions
            .iter()
            .filter(|condition| condition.required)
        {
            let evidence = common
                .evidence_refs
                .iter()
                .rev()
                .find(|evidence| {
                    evidence.condition_id == condition.condition_id
                        && evidence.condition_revision == condition.condition_revision
                        && evidence.condition_set_revision == common.request.condition_set_revision
                        && evidence.request_revision == common.request.request_revision
                        && evidence.target_version == common.run_state.target_version
                        && evidence.lifecycle == EvidenceLifecycleV1::Current
                })
                .ok_or_else(|| "common-harness required evidence is missing".to_string())?;
            let (snapshot, _) =
                self.common_harness_evidence_snapshot(turn, common, condition, evidence)?;
            if !snapshot
                .matches_workspace()
                .map_err(|error| format!("recheck evidence snapshot: {error}"))?
            {
                return Err(
                    "common-harness evidence snapshot no longer matches the workspace".into(),
                );
            }
        }
        Ok(())
    }

    pub(super) async fn rebind_common_harness_target(
        &self,
        root: &str,
        turn: &TurnContext,
        common: CommonHarnessStateV1,
        target: ArtifactSetV1,
    ) -> Result<CommonHarnessStateV1, String> {
        let mut reusable = Vec::new();
        for condition in &common.request.conditions {
            let Some(evidence) = common.evidence_refs.iter().rev().find(|evidence| {
                evidence.condition_id == condition.condition_id
                    && evidence.condition_revision == condition.condition_revision
                    && evidence.condition_set_revision == common.request.condition_set_revision
                    && evidence.request_revision == common.request.request_revision
                    && evidence.target_version == common.run_state.target_version
                    && evidence.lifecycle == EvidenceLifecycleV1::Current
            }) else {
                continue;
            };
            if evidence.verdict != ConditionVerdictV1::Pass {
                continue;
            }
            let Ok((before, before_ref)) =
                self.common_harness_evidence_snapshot(turn, &common, condition, evidence)
            else {
                // Missing or legacy snapshots require fresh verification; they never permit reuse.
                continue;
            };
            if compare_dependency_subset(&before, &target, &dependencies(condition))
                .map_err(|error| format!("compare condition dependencies: {error}"))?
                == DependencySubsetComparisonV1::Unchanged
            {
                reusable.push((condition.clone(), evidence.clone(), before_ref));
            }
        }
        let home = turn.config.codex_home.as_path();
        let target_ref = if reusable.is_empty() {
            None
        } else {
            Some(write_operation_artifact(home, self.thread_id(), &target)?)
        };
        let evidence_ids = common
            .evidence_refs
            .iter()
            .filter(|evidence| evidence.lifecycle == EvidenceLifecycleV1::Current)
            .map(|evidence| evidence.evidence_id.clone())
            .collect();
        let mut current = self
            .apply_common_harness_host_event(
                root,
                super::common_harness::common_harness_fence(&common),
                HostEventV1::InvalidateEvidence {
                    evidence_ids,
                    new_target_version: target.digest.clone(),
                },
            )
            .await?;
        for (condition, source, source_snapshot_ref) in reusable {
            let proof = EvidenceComparisonProofV1 {
                format: "common-harness-evidence-comparison/1".into(),
                condition_id: condition.condition_id.clone(),
                condition_revision: condition.condition_revision,
                condition_set_revision: common.request.condition_set_revision,
                request_revision: common.request.request_revision,
                source_evidence_id: source.evidence_id.clone(),
                operation_id: source.operation_id.clone(),
                execution_id: source.execution_id.clone(),
                old_target_version: source.target_version.clone(),
                new_target_version: target.digest.clone(),
                source_snapshot_ref,
                target_snapshot_ref: target_ref
                    .as_ref()
                    .ok_or("missing host target snapshot")?
                    .clone(),
                dependency_paths: dependencies(&condition),
            };
            let proof_ref = write_operation_artifact(home, self.thread_id(), &proof)?;
            let mut evidence = source;
            evidence.source_evidence_ref = Some(evidence.evidence_id.clone());
            evidence.evidence_id = format!("rebound-evidence:{}", uuid::Uuid::now_v7());
            evidence.target_version = target.digest.clone();
            evidence.comparison_proof_ref = Some(proof_ref);
            evidence.lifecycle = EvidenceLifecycleV1::Current;
            current = self
                .apply_common_harness_host_event(
                    root,
                    super::common_harness::common_harness_fence(&current),
                    HostEventV1::RebindEvidence { evidence },
                )
                .await?;
        }
        Ok(current)
    }
}
