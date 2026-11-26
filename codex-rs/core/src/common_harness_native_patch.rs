// Modified by RenCrow Switch Core, 2026-11-27: bind native patch executor provenance.
//! Host-owned immutable identity captured before native patch effects.

use crate::common_harness_io::PatchJournalIntentV1;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CommonHarnessPatchArtifactV1 {
    pub(crate) operation_id: String,
    pub(crate) journal: PatchJournalIntentV1,
    pub(crate) normalized_patch: String,
    pub(crate) executor: NativePatchExecutorProvenanceV1,
    pub(crate) completion_marker_ref: String,
}

impl CommonHarnessPatchArtifactV1 {
    pub(crate) fn native_patch_completion_marker(&self) -> NativePatchEffectCompleteV1 {
        NativePatchEffectCompleteV1 {
            format: "common-harness-native-patch-effect-complete/1".into(),
            operation_id: self.operation_id.clone(),
            executor: self.executor.clone(),
            base_target_version: self.journal.before.digest.clone(),
            predicted_target_version: self.journal.predicted_after.digest.clone(),
            normalized_patch_sha256: format!(
                "{:x}",
                sha2::Sha256::digest(self.normalized_patch.as_bytes())
            ),
        }
    }
}

/// Persisted only after the native effect await succeeds and before its receipt.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativePatchEffectCompleteV1 {
    pub(crate) format: String,
    pub(crate) operation_id: String,
    pub(crate) executor: NativePatchExecutorProvenanceV1,
    pub(crate) base_target_version: String,
    pub(crate) predicted_target_version: String,
    pub(crate) normalized_patch_sha256: String,
}
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativePatchExecutorProvenanceV1 {
    format: String,
    operation_id: String,
    thread_id: String,
    runtime_owner_id: String,
    owner_epoch: u64,
    process_id: u32,
}

impl NativePatchExecutorProvenanceV1 {
    pub(crate) fn operation_id(&self) -> &str {
        &self.operation_id
    }

    pub(crate) fn thread_id(&self) -> &str {
        &self.thread_id
    }

    pub(crate) fn runtime_owner_id(&self) -> &str {
        &self.runtime_owner_id
    }

    pub(crate) fn owner_epoch(&self) -> u64 {
        self.owner_epoch
    }

    pub(crate) fn process_id(&self) -> u32 {
        self.process_id
    }

    pub(crate) fn is_bound_to(
        &self,
        operation_id: &str,
        thread_id: &str,
        runtime_owner_id: &str,
        owner_epoch: u64,
    ) -> bool {
        self.format == "common-harness-native-patch-executor/1"
            && self.operation_id() == operation_id
            && self.thread_id() == thread_id
            && self.runtime_owner_id() == runtime_owner_id
            && self.owner_epoch() == owner_epoch
            && self.process_id() > 0
            && self.process_id() <= i32::MAX as u32
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativePatchExecutorDeadProofV1 {
    pub(crate) format: String,
    pub(crate) operation_id: String,
    pub(crate) thread_id: String,
    pub(crate) runtime_owner_id: String,
    pub(crate) owner_epoch: u64,
    pub(crate) process_id: u32,
    pub(crate) errno: i32,
    pub(crate) provenance_ref: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum NativeProcessProbeV1 {
    ProvenDead(NativePatchExecutorDeadProofV1),
    NotProven(NativeProcessNotProvenV1),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NativeProcessNotProvenV1 {
    pub(crate) process_id: u32,
    pub(crate) reason: NativeProcessNotProvenReasonV1,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum NativeProcessNotProvenReasonV1 {
    ProcessExistsOrPidWasReused,
    PermissionDenied,
    #[cfg(not(unix))]
    UnsupportedPlatform,
    ProbeFailed(i32),
}

/// Samples host-owned process/runtime identity before the native effect.
/// PID is never accepted from a caller or a model field.
pub(crate) fn capture_before_effect(
    operation_id: &str,
    thread_id: &str,
    owner_epoch: u64,
) -> Result<NativePatchExecutorProvenanceV1, String> {
    if operation_id.trim().is_empty() || thread_id.trim().is_empty() || owner_epoch == 0 {
        return Err("native patch executor identity is empty".into());
    }
    Ok(NativePatchExecutorProvenanceV1 {
        format: "common-harness-native-patch-executor/1".into(),
        operation_id: operation_id.to_owned(),
        thread_id: thread_id.to_owned(),
        runtime_owner_id: crate::session::common_harness::common_harness_runtime_owner_id(
            thread_id,
        ),
        owner_epoch,
        process_id: std::process::id(),
    })
}

/// Positive proof exists only for `kill(pid, 0) == -1` with ESRCH.
pub(crate) fn probe_executor_dead(
    provenance: &NativePatchExecutorProvenanceV1,
    provenance_ref: &str,
) -> Result<NativeProcessProbeV1, String> {
    if provenance.format != "common-harness-native-patch-executor/1"
        || provenance.operation_id.trim().is_empty()
        || provenance.thread_id.trim().is_empty()
        || provenance.runtime_owner_id.trim().is_empty()
        || provenance.process_id == 0
        || provenance.process_id > i32::MAX as u32
        || !valid_artifact_ref(provenance_ref)
    {
        return Err("malformed native patch executor provenance".into());
    }

    #[cfg(unix)]
    {
        // SAFETY: kill(pid, 0) sends no signal and only probes process existence.
        let status = unsafe { libc::kill(provenance.process_id as libc::pid_t, 0) };
        if status == 0 {
            return Ok(NativeProcessProbeV1::NotProven(NativeProcessNotProvenV1 {
                process_id: provenance.process_id,
                reason: NativeProcessNotProvenReasonV1::ProcessExistsOrPidWasReused,
            }));
        }
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        if errno == libc::ESRCH {
            return Ok(NativeProcessProbeV1::ProvenDead(
                NativePatchExecutorDeadProofV1 {
                    format: "common-harness-native-patch-dead-proof/1".into(),
                    operation_id: provenance.operation_id.clone(),
                    thread_id: provenance.thread_id.clone(),
                    runtime_owner_id: provenance.runtime_owner_id.clone(),
                    owner_epoch: provenance.owner_epoch,
                    process_id: provenance.process_id,
                    errno,
                    provenance_ref: provenance_ref.to_owned(),
                },
            ));
        }
        let reason = if errno == libc::EPERM {
            NativeProcessNotProvenReasonV1::PermissionDenied
        } else {
            NativeProcessNotProvenReasonV1::ProbeFailed(errno)
        };
        Ok(NativeProcessProbeV1::NotProven(NativeProcessNotProvenV1 {
            process_id: provenance.process_id,
            reason,
        }))
    }
    #[cfg(not(unix))]
    {
        Ok(NativeProcessProbeV1::NotProven(NativeProcessNotProvenV1 {
            process_id: provenance.process_id,
            reason: NativeProcessNotProvenReasonV1::UnsupportedPlatform,
        }))
    }
}

fn valid_artifact_ref(reference: &str) -> bool {
    reference
        .strip_prefix("common-harness-artifact/1:sha256:")
        .is_some_and(|digest| {
            digest.len() == 64
                && digest
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        })
}

#[cfg(test)]
#[path = "common_harness_native_patch_tests.rs"]
mod tests;
