// Modified by RenCrow Switch Core, 2026-11-21.
//! Explicit, host-owned successor-parent projection. Original evidence remains immutable.

use super::*;
use crate::rencrow_execution_guard::GuardState;
use codex_history::task_context::StoredTaskState;
use codex_protocol::ThreadId;
use codex_protocol::protocol::ThreadSettingsSnapshot;
use std::path::PathBuf;

const HANDOFF_VERSION: u32 = 1;
const MAX_HANDOFF_BYTES: usize = 8 * 1024 * 1024;
const MAX_PRIOR_SNAPSHOTS: usize = 256;

#[path = "parent_handoff_runtime.rs"]
pub(crate) mod runtime;

pub(crate) fn proof_closure(
    latest: &Value,
    available: &BTreeMap<String, Value>,
    task: Option<&StoredTaskState>,
) -> Result<BTreeMap<String, Value>, String> {
    let typed: Snapshot = serde_json::from_value(latest.clone()).map_err(|e| e.to_string())?;
    let mut pending = snapshot_dependencies(&typed)?;
    if let Some(completion) = task.and_then(|task| task.boundary.completion_state.as_ref()) {
        for unit in &completion.units {
            if let Some(review) = &unit.review_state {
                review_reference_hashes(
                    &serde_json::to_value(review).map_err(|e| e.to_string())?,
                    &mut pending,
                )?;
            }
        }
    }
    let mut result = BTreeMap::new();
    while let Some(hash) = pending.pop_first() {
        if result.contains_key(&hash) {
            continue;
        }
        if result.len() >= MAX_PRIOR_SNAPSHOTS {
            return Err("parent handoff proof limit exceeded".into());
        }
        let value = available
            .get(&hash)
            .ok_or("parent handoff proof unavailable")?;
        let snapshot: Snapshot =
            serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
        pending.extend(snapshot_dependencies(&snapshot)?);
        result.insert(hash, value.clone());
    }
    result.remove(&maintenance_snapshot_digest(latest)?);
    Ok(result)
}

/// A context capsule produced by the running source owner before PreCompact hooks.
/// It carries original host facts, not model assertions of completion or new authority.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ParentHandoffCapsule {
    pub(crate) version: u32,
    pub(crate) source_thread_id: ThreadId,
    pub(crate) successor_thread_id: ThreadId,
    pub(crate) source_turn_id: String,
    pub(crate) source_rollout: PathBuf,
    pub(crate) source_history_hash: String,
    pub(crate) source_history_len: usize,
    pub(crate) pending_prompt: Option<String>,
    pub(crate) runtime_contract_sha256: String,
    pub(crate) settings: ThreadSettingsSnapshot,
    pub(crate) cwd: PathBuf,
    pub(crate) summary: String,
    pub(crate) next_work: String,
    pub(crate) original_snapshot: Value,
    pub(crate) prior_snapshots: BTreeMap<String, Value>,
    pub(crate) accepted_task_state: Option<StoredTaskState>,
    pub(crate) retained_instructions: Vec<RolloutItem>,
    pub(crate) guard: Value,
}

/// A new owner's derived view, with complete original-to-projected audit correspondence.
/// Terminal sources in the original capsule are never edited in the predecessor's history.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ParentHandoffProjection {
    pub(crate) version: u32,
    pub(crate) source_thread_id: ThreadId,
    pub(crate) successor_thread_id: ThreadId,
    pub(crate) capsule_sha256: String,
    pub(crate) prior_snapshots: BTreeMap<String, Value>,
    pub(crate) original_to_projected: BTreeMap<String, String>,
    pub(crate) snapshot: Value,
    pub(crate) accepted_task_state: Option<StoredTaskState>,
    pub(crate) guard: Value,
}

fn review_reference_hashes(value: &Value, hashes: &mut BTreeSet<String>) -> Result<(), String> {
    match value {
        Value::Object(fields)
            if fields.len() == 2
                && fields.contains_key("version")
                && fields.contains_key("sha256") =>
        {
            if !matches!(fields["version"].as_u64(), Some(1 | 2)) {
                return Err("unsupported review snapshot reference in parent handoff".into());
            }
            let hash = fields["sha256"]
                .as_str()
                .filter(|hash| is_sha256(hash))
                .ok_or("invalid review snapshot reference in parent handoff")?;
            hashes.insert(hash.to_owned());
        }
        Value::Object(fields) => {
            for child in fields.values() {
                review_reference_hashes(child, hashes)?;
            }
        }
        Value::Array(items) => {
            for child in items {
                review_reference_hashes(child, hashes)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn snapshot_dependencies(snapshot: &Snapshot) -> Result<BTreeSet<String>, String> {
    let mut hashes = BTreeSet::new();
    for unit in &snapshot.state.units {
        if let Some(review) = &unit.review_state {
            review_reference_hashes(
                &serde_json::to_value(review).map_err(|e| e.to_string())?,
                &mut hashes,
            )?;
        }
    }
    Ok(hashes)
}

fn project_review_references(
    value: &mut Value,
    mapping: &BTreeMap<String, String>,
) -> Result<(), String> {
    match value {
        Value::Object(fields)
            if fields.len() == 2
                && fields.contains_key("version")
                && fields.contains_key("sha256") =>
        {
            let source = fields["sha256"]
                .as_str()
                .ok_or("malformed review snapshot reference")?;
            let target = mapping
                .get(source)
                .ok_or("parent handoff omits a referenced host snapshot")?;
            fields.insert("sha256".into(), Value::String(target.clone()));
        }
        Value::Object(fields) => {
            for child in fields.values_mut() {
                project_review_references(child, mapping)?;
            }
        }
        Value::Array(items) => {
            for child in items {
                project_review_references(child, mapping)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Rebind only the logical parent ownership of the declared terminal-source fields.
/// Reviewer identities, attempts, commands, receipts, artifacts, decisions, and caps stay intact.
fn project_completion_state(
    state: &mut CompletionState,
    source: &str,
    target: &str,
    mapping: &BTreeMap<String, String>,
) -> Result<(), String> {
    for unit in &mut state.units {
        let Some(review) = &mut unit.review_state else {
            continue;
        };
        let active = review
            .active_attempt
            .as_mut()
            .and_then(|attempt| attempt.terminal_source.as_mut())
            .and_then(|source| source.event.as_mut());
        let historical = review
            .historical_failure
            .as_mut()
            .and_then(|failure| failure.terminal_source.event.as_mut());
        let rollback = review
            .retry_reservation_prior_historical_failure
            .as_mut()
            .and_then(|failure| failure.terminal_source.event.as_mut());
        for event in [active, historical, rollback].into_iter().flatten() {
            if event.owner_parent_thread_id.as_deref() != Some(source) {
                return Err("parent handoff terminal source belongs to another parent".into());
            }
            event.owner_parent_thread_id = Some(target.to_owned());
        }
        let mut value = serde_json::to_value(&*review).map_err(|e| e.to_string())?;
        project_review_references(&mut value, mapping)?;
        *review = serde_json::from_value(value).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn project_snapshot(
    original: &Snapshot,
    source: &str,
    target: &str,
    mapping: &BTreeMap<String, String>,
) -> Result<Snapshot, String> {
    if original.thread_id != source || !completion_review_event_parents_match_snapshot(original) {
        return Err("parent handoff contains a foreign owner snapshot".into());
    }
    let mut derived = original.clone();
    derived.thread_id = target.to_owned();
    project_completion_state(&mut derived.state, source, target, mapping)?;
    // A predecessor maintenance repair was not performed on the successor. Its complete
    // original receipt remains in the immutable capsule and audit mapping instead.
    derived.maintenance_repair_receipt = None;
    Ok(derived)
}

impl ParentHandoffCapsule {
    pub(crate) fn sha256(&self) -> Result<String, String> {
        maintenance_snapshot_digest(&serde_json::to_value(self).map_err(|e| e.to_string())?)
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.version != HANDOFF_VERSION
            || self.source_thread_id == self.successor_thread_id
            || self.source_turn_id.is_empty()
            || !self.source_rollout.is_absolute()
            || !self.cwd.is_absolute()
            || !is_sha256(&self.source_history_hash)
            || !is_sha256(&self.runtime_contract_sha256)
            || self.summary.trim().is_empty()
            || self.next_work.trim().is_empty()
            || self.prior_snapshots.len() > MAX_PRIOR_SNAPSHOTS
            || serde_json::to_vec(self).map_err(|e| e.to_string())?.len() > MAX_HANDOFF_BYTES
            || !GuardState::from_parent_handoff(&self.guard)?.parent_handoff_is_quiescent()
        {
            return Err("parent handoff capsule is incomplete, busy, or out of bounds".into());
        }
        let source = self.source_thread_id.to_string();
        let latest: Snapshot =
            serde_json::from_value(self.original_snapshot.clone()).map_err(|e| e.to_string())?;
        if latest.state.paused || !latest.uncollected_children.is_empty() {
            return Err("paused or uncollected source work cannot migrate automatically".into());
        }
        if latest.task_checks.iter().any(|check| {
            matches!(
                check.attempt.status,
                crate::task_check::TaskCheckAttemptStatus::Pending
            )
        }) || latest.state.units.iter().any(|unit| {
            unit.review_launch.is_some()
                || unit
                    .review_state
                    .as_ref()
                    .and_then(|review| review.active_attempt.as_ref())
                    .is_some_and(|attempt| {
                        !matches!(attempt.status,
                        codex_history::task_context::CompletionReviewAttemptStatus::Failed
                        | codex_history::task_context::CompletionReviewAttemptStatus::Completed
                        | codex_history::task_context::CompletionReviewAttemptStatus::Cancelled)
                    })
        }) {
            return Err(
                "in-flight or unknown check/review outcomes cannot migrate automatically".into(),
            );
        }
        let receipts = match &self.accepted_task_state {
            Some(state) => {
                state.validate(&source)?;
                if state.boundary.root_task_id != latest.root_task_id {
                    return Err("parent handoff task root differs from its completion root".into());
                }
                state.completed.clone()
            }
            None => BTreeMap::new(),
        };
        validate_snapshot_payload(&latest, &source, &receipts)?;
        let mut originals = self.prior_snapshots.clone();
        for (hash, value) in &originals {
            let snapshot: Snapshot =
                serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
            if maintenance_snapshot_digest(value)? != *hash
                || snapshot.thread_id != source
                || snapshot.root_task_id != latest.root_task_id
                || !completion_review_event_parents_match_snapshot(&snapshot)
            {
                return Err("parent handoff prior-snapshot proof is invalid".into());
            }
        }
        let latest_hash = maintenance_snapshot_digest(&self.original_snapshot)?;
        originals.insert(latest_hash.clone(), self.original_snapshot.clone());
        if !review_snapshot_references_resolve(&latest, &latest_hash, &originals) {
            return Err(
                "parent handoff has unresolved review authorization or retry history".into(),
            );
        }
        Ok(())
    }

    pub(crate) fn project(&self) -> Result<ParentHandoffProjection, String> {
        self.validate()?;
        let source = self.source_thread_id.to_string();
        let target = self.successor_thread_id.to_string();
        let mut pending = self.prior_snapshots.clone();
        let mut mapping = BTreeMap::new();
        let mut projected = BTreeMap::new();
        while !pending.is_empty() {
            let mut ready = Vec::new();
            for (hash, value) in &pending {
                let snapshot: Snapshot =
                    serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
                if snapshot_dependencies(&snapshot)?
                    .iter()
                    .all(|dependency| mapping.contains_key(dependency))
                {
                    ready.push(hash.clone());
                }
            }
            if ready.is_empty() {
                return Err("parent handoff reference closure is missing or cyclic".into());
            }
            for hash in ready {
                let value = pending
                    .remove(&hash)
                    .ok_or("parent handoff reference disappeared")?;
                let original: Snapshot =
                    serde_json::from_value(value).map_err(|e| e.to_string())?;
                let derived = project_snapshot(&original, &source, &target, &mapping)?;
                let value = serde_json::to_value(derived).map_err(|e| e.to_string())?;
                let derived_hash = maintenance_snapshot_digest(&value)?;
                mapping.insert(hash, derived_hash.clone());
                projected.insert(derived_hash, value);
            }
        }
        let original: Snapshot =
            serde_json::from_value(self.original_snapshot.clone()).map_err(|e| e.to_string())?;
        let mut latest = project_snapshot(&original, &source, &target, &mapping)?;
        latest.resume_generation = latest
            .resume_generation
            .checked_add(1)
            .ok_or("parent handoff resume generation overflow")?;
        let mut task_state = self.accepted_task_state.clone();
        if let Some(state) = &mut task_state {
            state.boundary.thread_id = target.clone();
            if let Some(completion) = state.boundary.completion_state.as_mut() {
                project_completion_state(completion, &source, &target, &mapping)?;
            }
            state.candidate_hash = codex_history::archive_reference::content_sha256(
                &serde_json::to_string(&state.boundary).map_err(|e| e.to_string())?,
            );
            state.validate(&target)?;
        }
        let receipts = task_state
            .as_ref()
            .map(|state| state.completed.clone())
            .unwrap_or_default();
        validate_snapshot_payload(&latest, &target, &receipts)?;
        let snapshot = serde_json::to_value(latest).map_err(|e| e.to_string())?;
        let latest_hash = maintenance_snapshot_digest(&snapshot)?;
        let mut proof_values = projected.clone();
        proof_values.insert(latest_hash.clone(), snapshot.clone());
        let typed: Snapshot =
            serde_json::from_value(snapshot.clone()).map_err(|e| e.to_string())?;
        if !review_snapshot_references_resolve(&typed, &latest_hash, &proof_values) {
            return Err("projected parent review history does not validate".into());
        }
        mapping.insert(
            maintenance_snapshot_digest(&self.original_snapshot)?,
            latest_hash,
        );
        Ok(ParentHandoffProjection {
            version: HANDOFF_VERSION,
            source_thread_id: self.source_thread_id,
            successor_thread_id: self.successor_thread_id,
            capsule_sha256: self.sha256()?,
            prior_snapshots: projected,
            original_to_projected: mapping,
            snapshot,
            accepted_task_state: task_state,
            guard: self.guard.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::review_reference_hashes;
    use serde_json::json;
    use std::collections::BTreeSet;

    #[test]
    fn parent_handoff_review_reference_walker_accepts_only_valid_v1_v2_refs() {
        // Captured from the actual v2 previous_attempt_ref that caused the runtime warning.
        let captured_v2_sha = "20b9cbe1774e14df8626033a215f72e6b6303dad056138c9734b2f07c7574622";
        for (version, sha) in [(1, "a".repeat(64)), (2, captured_v2_sha.to_owned())] {
            let value = json!({"previous_attempt_ref":{"version":version,"sha256":sha.clone()}});
            let mut hashes = BTreeSet::new();
            review_reference_hashes(&value, &mut hashes).expect("supported review ref");
            assert_eq!(hashes, [sha].into_iter().collect());
        }

        for value in [
            json!({"previous_attempt_ref":{"version":3,"sha256":"a".repeat(64)}}),
            json!({"previous_attempt_ref":{"version":2,"sha256":"not-a-sha256"}}),
        ] {
            let mut hashes = BTreeSet::new();
            assert!(review_reference_hashes(&value, &mut hashes).is_err());
            assert!(hashes.is_empty());
        }
    }
}
