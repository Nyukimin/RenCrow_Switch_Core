// Modified by RenCrow Switch Core, 2026-11-26: bind expected failures to host verification facts.
//! Host-only expected-negative obligations. Raw failing receipts remain immutable.

use super::TaskCheckExecution;
use super::TaskCheckLedger;
use super::TaskCheckObligation;
use serde::Deserialize;
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExpectedNegativeBindingV1 {
    pub(crate) source_call_id: String,
    pub(crate) obligation: TaskCheckObligation,
    pub(crate) condition_id: String,
    pub(crate) condition_revision: u64,
    pub(crate) condition_set_revision: u64,
    pub(crate) baseline_version: String,
    pub(crate) expected_outcome_digest: String,
    pub(crate) expected_failure_identity: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExpectedNegativeResolutionV1 {
    pub(crate) observed_failure_identity: String,
    pub(crate) evidence_ref: String,
    pub(crate) raw_output_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExpectedNegativeRecordV1 {
    binding: ExpectedNegativeBindingV1,
    resolution: Option<ExpectedNegativeResolutionV1>,
}

impl TaskCheckLedger {
    /// Projects exact raw-check call keys after host-proven failure settlement.
    pub(crate) fn resolved_call_guard_keys(&self) -> Vec<String> {
        let mut keys = self
            .attempts
            .values()
            .filter_map(|attempt| {
                let super::TaskCheckAttemptStatus::Finished(receipt) = &attempt.status else {
                    return None;
                };
                if receipt.succeeded
                    || !receipt.completed
                    || receipt.exit_code == 0
                    || receipt.exit_code == -1
                    || self.attempt_is_unresolved(attempt)
                {
                    return None;
                }
                let obligation = self
                    .attempt_obligation
                    .get(&attempt.call_id)
                    .and_then(Option::as_ref)?;
                Some(format!("{}:call:{}", obligation.key(), attempt.call_id))
            })
            .collect::<Vec<_>>();
        keys.sort();
        keys.dedup();
        keys
    }

    /// Check that the pending row binds this exact pre-launch execution before persistence.
    pub(crate) fn pending_execution_matches(
        &self,
        call_id: &str,
        execution: &TaskCheckExecution,
    ) -> bool {
        self.attempts.get(call_id).is_some_and(|attempt| {
            matches!(&attempt.status, super::TaskCheckAttemptStatus::Pending)
        }) && self
            .pending_data
            .get(call_id)
            .is_some_and(|(request, workspace, before)| {
                request == &execution.request
                    && workspace == &execution.workspace_root
                    && before == &execution.before
                    && self.attempt_obligation.get(call_id) == Some(&execution.obligation)
            })
    }

    /// Reserve the expected outcome while the matching raw check is still pending.
    pub(crate) fn reserve_expected_negative(
        &mut self,
        binding: ExpectedNegativeBindingV1,
    ) -> Result<(), String> {
        self.validate_expected_negative_binding(&binding)?;
        if !self
            .attempts
            .get(&binding.source_call_id)
            .is_some_and(|attempt| {
                matches!(&attempt.status, super::TaskCheckAttemptStatus::Pending)
            })
            || !self.pending_data.contains_key(&binding.source_call_id)
        {
            return Err(
                "expected-negative reservation requires the pending pre-launch check".into(),
            );
        }
        if let Some(existing) = self.expected_negative.get(&binding.source_call_id) {
            return if existing.binding == binding {
                Ok(())
            } else {
                Err("conflicting expected-negative reservation".into())
            };
        }
        self.expected_negative.insert(
            binding.source_call_id.clone(),
            ExpectedNegativeRecordV1 {
                binding,
                resolution: None,
            },
        );
        Ok(())
    }

    /// Resolve only the reserved reproduction obligation from host-observed failure evidence.
    pub(crate) fn resolve_expected_negative(
        &mut self,
        binding: &ExpectedNegativeBindingV1,
        resolution: ExpectedNegativeResolutionV1,
    ) -> Result<(), String> {
        let record = self
            .expected_negative
            .get(&binding.source_call_id)
            .ok_or_else(|| {
                "expected-negative check was not reserved before execution".to_string()
            })?;
        if &record.binding != binding {
            return Err("expected-negative resolution changed its reserved binding".into());
        }
        self.validate_expected_negative_resolution(binding, &resolution)?;
        if let Some(existing) = &record.resolution {
            return if existing == &resolution {
                Ok(())
            } else {
                Err("conflicting expected-negative resolution".into())
            };
        }
        let record = self
            .expected_negative
            .get_mut(&binding.source_call_id)
            .ok_or_else(|| {
                "expected-negative check was not reserved before execution".to_string()
            })?;
        record.resolution = Some(resolution);
        Ok(())
    }

    fn validate_expected_negative_binding(
        &self,
        binding: &ExpectedNegativeBindingV1,
    ) -> Result<(), String> {
        let sha256 =
            |value: &str| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit());
        if binding.source_call_id.trim().is_empty()
            || binding.condition_id.trim().is_empty()
            || binding.condition_revision == 0
            || binding.condition_set_revision == 0
            || binding.expected_failure_identity.trim().is_empty()
            || !sha256(&binding.baseline_version)
            || !sha256(&binding.expected_outcome_digest)
            || self
                .attempt_obligation
                .get(&binding.source_call_id)
                .and_then(Option::as_ref)
                != Some(&binding.obligation)
            || self.attempt_scope.get(&binding.source_call_id)
                != Some(&(
                    binding.obligation.scope.task_id.clone(),
                    binding.obligation.scope.task_revision,
                ))
        {
            return Err("invalid expected-negative host binding".into());
        }
        Ok(())
    }

    fn validate_expected_negative_resolution(
        &self,
        binding: &ExpectedNegativeBindingV1,
        resolution: &ExpectedNegativeResolutionV1,
    ) -> Result<(), String> {
        self.validate_expected_negative_binding(binding)?;
        let Some(super::TaskCheckAttempt {
            status: super::TaskCheckAttemptStatus::Finished(receipt),
            ..
        }) = self.attempts.get(&binding.source_call_id)
        else {
            return Err("expected-negative source has no completed raw receipt".into());
        };
        if receipt.exit_code != 1
            || !receipt.completed
            || receipt.succeeded
            || receipt.workspace_root != binding.obligation.workspace_root
            || receipt.artifact_before != receipt.artifact_after
            || receipt
                .artifact_before
                .iter()
                .map(|stamp| &stamp.path)
                .ne(binding.obligation.artifact_paths.iter())
            || resolution.observed_failure_identity != binding.expected_failure_identity
            || resolution.evidence_ref.trim().is_empty()
            || resolution.raw_output_sha256.len() != 64
            || !resolution
                .raw_output_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
            || receipt.observation.as_ref().is_none_or(|observation| {
                !observation.diagnostic_complete
                    || observation.diagnostic_truncated
                    || observation.launch_failure.is_some()
                    || observation.output_ref != binding.source_call_id
                    || observation.diagnostic_sha256 != resolution.raw_output_sha256
            })
        {
            return Err(
                "expected-negative resolution does not match the complete raw failure".into(),
            );
        }
        Ok(())
    }

    pub(super) fn validate_expected_negative_snapshot(&self) -> Result<(), String> {
        for (call, record) in &self.expected_negative {
            if call != &record.binding.source_call_id {
                return Err("expected-negative snapshot call mismatch".into());
            }
            self.validate_expected_negative_binding(&record.binding)?;
            if let Some(resolution) = &record.resolution {
                self.validate_expected_negative_resolution(&record.binding, resolution)?;
            }
        }
        Ok(())
    }

    pub(super) fn attempt_is_unresolved(&self, attempt: &super::TaskCheckAttempt) -> bool {
        super::attempt_is_unresolved(attempt)
            && self
                .expected_negative
                .get(&attempt.call_id)
                .is_none_or(|record| record.resolution.is_none())
    }
}

#[cfg(test)]
#[path = "task_check_expected_negative_tests.rs"]
mod tests;
