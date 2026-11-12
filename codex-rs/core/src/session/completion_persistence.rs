//! Durable host snapshots reuse the rollout envelope, outside model-written content.
// Modified by RenCrow Switch Core, 2026-11-11.

use super::session::Session;
use crate::context::AutonomousCompletionSnapshot;
use crate::context::ContextualUserFragment;
use crate::task_check::TaskCheckLedger;
use crate::task_check::TaskCheckSnapshot;
use codex_history::CodexHarnessMetadata;
use codex_history::ResponseItemEnvelope;
use codex_history::RolloutItem;
use codex_history::task_context::CompletionState;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TurnAbortReason;
use codex_utils_output_truncation::TruncationPolicy;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashSet;

const MAX_COMPLETION_SNAPSHOT_BYTES: usize = 256 * 1024;
const MAX_COMPLETION_TASK_CHECKS: usize = 256;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MaintenanceRepairEvidenceDigest {
    pub kind: String,
    pub path: String,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MaintenanceRepairReceipt {
    pub version: u32,
    pub thread_id: String,
    pub root_task_id: String,
    pub request_sha256: String,
    pub snapshot_before_sha256: String,
    pub shutdown_before_snapshot_sha256: String,
    pub shutdown_paused_snapshot_sha256: String,
    pub legacy_scope_mismatch_snapshot_sha256: Vec<String>,
    pub evidence: Vec<MaintenanceRepairEvidenceDigest>,
    pub resume_generation_before: u64,
    pub resume_generation_after: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MaintenanceRepairSnapshotInfo {
    pub value: Value,
    pub snapshot_sha256: String,
    pub root_task_id: String,
    pub resume_generation: u64,
    pub state: CompletionState,
    pub active_scope: Option<Value>,
    pub uncollected_children: Vec<String>,
}

fn validate_snapshot_sequence_transition(
    previous: Option<(&str, u64, u64, bool)>,
    next: &Snapshot,
) -> Result<(), &'static str> {
    let Some((previous_root, previous_revision, previous_generation, previous_paused)) = previous
    else {
        return Ok(());
    };
    if previous_root != next.root_task_id
        || next.state.contract_revision < previous_revision
        || next.resume_generation < previous_generation
        || (next.resume_generation == previous_generation && previous_paused && !next.state.paused)
    {
        return Err("completion snapshot sequence violates restore monotonicity");
    }
    Ok(())
}

fn collect_task_check_call_id(reference: &str, calls: &mut HashSet<String>) {
    if let Some(call_id) = reference
        .strip_prefix("task-check:")
        .and_then(|value| value.rsplit_once(':').map(|(call_id, _)| call_id))
        .filter(|call_id| !call_id.is_empty())
    {
        calls.insert(call_id.to_owned());
    }
}

fn completion_task_check_references(state: &CompletionState) -> HashSet<String> {
    let mut calls = HashSet::new();
    let evidence = state
        .conditions
        .iter()
        .flat_map(|condition| condition.evidence.iter())
        .chain(
            state
                .units
                .iter()
                .flat_map(|unit| unit.evidence.iter().chain(unit.task_refs.iter())),
        )
        .chain(
            state
                .blocker
                .iter()
                .flat_map(|blocker| blocker.evidence.iter()),
        );
    for reference in evidence {
        collect_task_check_call_id(&reference.artifact_ref, &mut calls);
        collect_task_check_call_id(&reference.verification_ref, &mut calls);
    }
    for reference in state
        .units
        .iter()
        .filter_map(|unit| unit.review_evidence.as_ref())
    {
        collect_task_check_call_id(&reference.target_artifact_ref, &mut calls);
    }
    calls
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn validate_repair_receipt(
    receipt: &MaintenanceRepairReceipt,
    snapshot: &Snapshot,
    expected_thread_id: &str,
) -> Result<(), String> {
    let allowed_kinds = [
        "deployment_result",
        "deployment_preflight",
        "maintenance_ticket",
        "pre_deploy_snapshot",
        "pre_deploy_facts",
        "deployer_source",
    ];
    let unique_hashes = receipt
        .legacy_scope_mismatch_snapshot_sha256
        .iter()
        .collect::<BTreeSet<_>>();
    let unique_kinds = receipt
        .evidence
        .iter()
        .map(|evidence| evidence.kind.as_str())
        .collect::<BTreeSet<_>>();
    if receipt.version != 1
        || receipt.thread_id != expected_thread_id
        || receipt.thread_id != snapshot.thread_id
        || receipt.root_task_id != snapshot.root_task_id
        || receipt.request_sha256.is_empty()
        || !is_sha256(&receipt.request_sha256)
        || !is_sha256(&receipt.snapshot_before_sha256)
        || !is_sha256(&receipt.shutdown_before_snapshot_sha256)
        || !is_sha256(&receipt.shutdown_paused_snapshot_sha256)
        || receipt.legacy_scope_mismatch_snapshot_sha256.is_empty()
        || receipt.legacy_scope_mismatch_snapshot_sha256.len() > 32
        || unique_hashes.len() != receipt.legacy_scope_mismatch_snapshot_sha256.len()
        || receipt
            .legacy_scope_mismatch_snapshot_sha256
            .iter()
            .any(|hash| !is_sha256(hash))
        || receipt.evidence.len() != allowed_kinds.len()
        || unique_kinds.len() != allowed_kinds.len()
        || receipt.evidence.iter().any(|evidence| {
            !allowed_kinds.contains(&evidence.kind.as_str())
                || evidence.path.trim().is_empty()
                || evidence.sha256.is_empty()
                || !is_sha256(&evidence.sha256)
        })
        || receipt.resume_generation_before.checked_add(1) != Some(receipt.resume_generation_after)
        || receipt.resume_generation_after != snapshot.resume_generation
        || snapshot.state.paused
    {
        return Err("maintenance repair receipt is invalid".into());
    }
    Ok(())
}

fn validate_snapshot_payload(
    snapshot: &Snapshot,
    expected_thread_id: &str,
    receipts: &BTreeMap<String, codex_history::task_context::AcceptedTaskReceipt>,
) -> Result<(), String> {
    if snapshot.version != 1
        || snapshot.thread_id != expected_thread_id
        || snapshot.root_task_id.trim().is_empty()
        || snapshot.root_task_id.len() > 4096
        || snapshot.state.contract_revision == 0
        || snapshot.task_checks.len() > MAX_COMPLETION_TASK_CHECKS
        || snapshot.uncollected_children.len() > 256
        || snapshot
            .uncollected_children
            .iter()
            .any(|id| codex_protocol::ThreadId::try_from(id.as_str()).is_err())
        || !serde_json::to_vec(snapshot)
            .is_ok_and(|bytes| bytes.len() <= MAX_COMPLETION_SNAPSHOT_BYTES)
        || snapshot.active_scope.as_ref().is_some_and(|scope| {
            scope.task_id.trim().is_empty()
                || scope.task_revision == 0
                || scope.begin_call_id.trim().is_empty()
        })
        || snapshot.patch_attempts.len() > 256
        || snapshot.patch_attempts.iter().any(|(call, attempt)| {
            call.trim().is_empty()
                || !snapshot
                    .active_scope
                    .as_ref()
                    .is_some_and(|scope| scope.same_execution(&attempt.scope))
                || attempt.paths.len() > 16
        })
        || codex_history::task_context::validate_completion_state(Some(&snapshot.state), receipts)
            .is_err()
    {
        return Err("completion snapshot payload is invalid".into());
    }
    let mut ledger = TaskCheckLedger::default();
    ledger.restore_snapshot(snapshot.task_checks.clone())?;
    let referenced_task_checks = completion_task_check_references(&snapshot.state);
    let retained_call_ids = snapshot
        .task_checks
        .iter()
        .map(|entry| entry.attempt.call_id.as_str())
        .collect::<HashSet<_>>();
    if referenced_task_checks
        .iter()
        .any(|call_id| !retained_call_ids.contains(call_id.as_str()))
    {
        return Err("completion snapshot references a missing task-check receipt".into());
    }
    Ok(())
}

/// Hash a completion snapshot using the canonical JSON digest used by compaction history.
pub fn maintenance_snapshot_digest(value: &Value) -> Result<String, String> {
    codex_history::compaction_candidate::digest(value)
}

/// Validate one snapshot in canonical history with the restore payload validator.
pub fn inspect_maintenance_repair_snapshot(
    items: &[RolloutItem],
    target_snapshot_sha256: &str,
    expected_thread_id: &str,
    expected_root_task_id: &str,
    allow_legacy_scope_root_mismatch: bool,
) -> Result<MaintenanceRepairSnapshotInfo, String> {
    if !is_sha256(target_snapshot_sha256) {
        return Err("target snapshot digest is invalid".into());
    }
    let committed_history = codex_history::compaction_transaction::committed_items(items)?;
    let mut response_items = Vec::new();
    let mut accepted_receipts = None;
    let mut previous_snapshot: Option<(String, u64, u64, bool)> = None;
    let mut target_info = None;
    for item in &committed_history {
        let checkpoint_changed = match item {
            RolloutItem::ResponseItem(envelope) => {
                response_items.push(envelope.clone());
                envelope
                    .metadata
                    .as_ref()
                    .is_some_and(|metadata| metadata.rencrow_compaction.is_some())
            }
            RolloutItem::Compacted(compacted) => {
                if let Some(replacement) = &compacted.replacement_history {
                    response_items = replacement.clone();
                }
                true
            }
            _ => false,
        };
        if checkpoint_changed {
            accepted_receipts =
                crate::compact::latest_task_state_in_history(&response_items, expected_thread_id)?;
        }
        let RolloutItem::ResponseItem(envelope) = item else {
            continue;
        };
        let Some(metadata) = envelope.metadata.as_ref() else {
            continue;
        };
        let Some(value) = metadata.rencrow_completion.as_ref() else {
            continue;
        };
        let is_host = matches!(&envelope.item, ResponseItem::Message {
            role, internal_chat_message_metadata_passthrough: Some(kind), ..
        } if role == "developer" && kind.content_item_kinds.as_ref().is_some_and(|kinds| {
            kinds.len() == 1 && kinds[0].0 == "rencrow.completion_snapshot"
        })) && !metadata.client_authored;
        if !is_host {
            if maintenance_snapshot_digest(value)? == target_snapshot_sha256 {
                return Err("target completion snapshot is not host-authored".into());
            }
            continue;
        }
        let snapshot = serde_json::from_value::<Snapshot>(value.clone())
            .map_err(|error| format!("completion snapshot is malformed: {error}"))?;
        let previous = previous_snapshot
            .as_ref()
            .map(|(root, revision, generation, paused)| {
                (root.as_str(), *revision, *generation, *paused)
            });
        validate_snapshot_sequence_transition(previous, &snapshot)?;
        previous_snapshot = Some((
            snapshot.root_task_id.clone(),
            snapshot.state.contract_revision,
            snapshot.resume_generation,
            snapshot.state.paused,
        ));
        if maintenance_snapshot_digest(value)? != target_snapshot_sha256 {
            continue;
        }
        if snapshot.thread_id != expected_thread_id
            || snapshot.root_task_id != expected_root_task_id
        {
            return Err("target completion snapshot belongs to another thread or root".into());
        }
        let scope_root_mismatch = snapshot
            .active_scope
            .as_ref()
            .is_some_and(|scope| scope.root_task_id != snapshot.root_task_id);
        if scope_root_mismatch != allow_legacy_scope_root_mismatch {
            return Err(
                "target completion snapshot has an unexpected scope/root relationship".into(),
            );
        }
        if accepted_receipts
            .as_ref()
            .is_some_and(|state| state.boundary.root_task_id != snapshot.root_task_id)
        {
            return Err(
                "target completion snapshot disagrees with the accepted task boundary root".into(),
            );
        }
        let receipts = accepted_receipts
            .as_ref()
            .map(|state| state.completed.clone())
            .unwrap_or_default();
        validate_snapshot_payload(&snapshot, expected_thread_id, &receipts)?;
        if target_info.is_none() {
            target_info = Some(MaintenanceRepairSnapshotInfo {
                value: value.clone(),
                snapshot_sha256: target_snapshot_sha256.to_owned(),
                root_task_id: snapshot.root_task_id,
                resume_generation: snapshot.resume_generation,
                state: snapshot.state,
                active_scope: snapshot
                    .active_scope
                    .map(|scope| serde_json::to_value(scope).map_err(|error| error.to_string()))
                    .transpose()?,
                uncollected_children: snapshot.uncollected_children,
            });
        }
    }
    target_info
        .ok_or_else(|| "target completion snapshot was not found in canonical history".into())
}

/// Build a generation-bumped pause repair without changing contract, scope, checks or children.
pub fn build_maintenance_repair_snapshot(
    current_value: &Value,
    receipt: MaintenanceRepairReceipt,
) -> Result<Value, String> {
    let mut snapshot = serde_json::from_value::<Snapshot>(current_value.clone())
        .map_err(|error| format!("current completion snapshot is malformed: {error}"))?;
    let current_sha256 = maintenance_snapshot_digest(current_value)?;
    if !snapshot.state.paused
        || receipt.snapshot_before_sha256 != current_sha256
        || receipt.resume_generation_before != snapshot.resume_generation
    {
        return Err("repair receipt does not match the paused snapshot".into());
    }
    snapshot.state.paused = false;
    snapshot.resume_generation = snapshot
        .resume_generation
        .checked_add(1)
        .ok_or_else(|| "completion resume generation overflow".to_string())?;
    snapshot.maintenance_repair_receipt = Some(receipt);
    let receipt = snapshot
        .maintenance_repair_receipt
        .as_ref()
        .ok_or_else(|| "repair receipt was not attached".to_string())?;
    validate_repair_receipt(receipt, &snapshot, &snapshot.thread_id)?;
    let value = serde_json::to_value(snapshot).map_err(|error| error.to_string())?;
    if serde_json::to_vec(&value)
        .map_err(|error| error.to_string())?
        .len()
        > MAX_COMPLETION_SNAPSHOT_BYTES
    {
        return Err("repaired completion snapshot exceeds its size bound".into());
    }
    Ok(value)
}

fn maintenance_receipt_matches_history(
    receipt: &MaintenanceRepairReceipt,
    repair_value: &Value,
    host_snapshots: &BTreeMap<String, Value>,
) -> bool {
    let Some(shutdown_before) = host_snapshots.get(&receipt.shutdown_before_snapshot_sha256) else {
        return false;
    };
    let Some(shutdown_paused) = host_snapshots.get(&receipt.shutdown_paused_snapshot_sha256) else {
        return false;
    };
    let Some(paused_current) = host_snapshots.get(&receipt.snapshot_before_sha256) else {
        return false;
    };
    let Ok(before_snapshot) = serde_json::from_value::<Snapshot>(shutdown_before.clone()) else {
        return false;
    };
    let Ok(paused_snapshot) = serde_json::from_value::<Snapshot>(shutdown_paused.clone()) else {
        return false;
    };
    let Ok(repair_snapshot) = serde_json::from_value::<Snapshot>(repair_value.clone()) else {
        return false;
    };
    if before_snapshot.state.paused
        || !paused_snapshot.state.paused
        || before_snapshot.thread_id != paused_snapshot.thread_id
        || before_snapshot.root_task_id != paused_snapshot.root_task_id
        || before_snapshot.state.contract_revision != paused_snapshot.state.contract_revision
        || before_snapshot.resume_generation != paused_snapshot.resume_generation
        || receipt.thread_id != before_snapshot.thread_id
        || receipt.root_task_id != before_snapshot.root_task_id
    {
        return false;
    }
    let mut expected_paused = shutdown_before.clone();
    let Some(paused_flag) = expected_paused
        .get_mut("state")
        .and_then(Value::as_object_mut)
        .and_then(|state| state.get_mut("paused"))
    else {
        return false;
    };
    *paused_flag = Value::Bool(true);
    if expected_paused != *shutdown_paused {
        return false;
    }
    let Ok(expected_repair) = build_maintenance_repair_snapshot(paused_current, receipt.clone())
    else {
        return false;
    };
    expected_repair == *repair_value
        && repair_snapshot.root_task_id == paused_snapshot.root_task_id
        && repair_snapshot.resume_generation == paused_snapshot.resume_generation.saturating_add(1)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    version: u32,
    thread_id: String,
    root_task_id: String,
    state: CompletionState,
    #[serde(default)]
    resume_generation: u64,
    #[serde(default)]
    task_checks: Vec<TaskCheckSnapshot>,
    #[serde(default)]
    active_scope: Option<crate::state::ActiveTaskScope>,
    #[serde(default)]
    patch_attempts: Vec<(String, crate::state::TaskPatchAttempt)>,
    #[serde(default)]
    uncollected_children: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    maintenance_repair_receipt: Option<MaintenanceRepairReceipt>,
}

impl Session {
    pub(crate) async fn autonomous_completion_event_seen(&self, event_key: &str) -> bool {
        self.state
            .lock()
            .await
            .rencrow_completion_state
            .as_ref()
            .is_some_and(|(_, state)| {
                state
                    .consumed_host_events
                    .iter()
                    .any(|key| key == event_key)
            })
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "dedicated append lock serializes capture and asynchronous rollout persistence; SessionState locks are scoped separately"
    )]
    pub(crate) async fn persist_autonomous_completion_snapshot(&self) -> Result<(), String> {
        let persist_lock = self
            .state
            .lock()
            .await
            .rencrow_completion_persist_lock
            .clone();
        // Serialize capture and the async append together; this dedicated lock never
        // protects SessionState and prevents older snapshots overtaking newer ones.
        let _persist_guard = persist_lock.lock().await;
        let (
            completion_state,
            task_checks,
            referenced_task_checks,
            resume_generation,
            uncollected_children,
            active_scope,
            patch_attempts,
        ) = {
            let state = self.state.lock().await;
            let referenced_task_checks = state
                .rencrow_completion_state
                .as_ref()
                .map(|(_, contract)| completion_task_check_references(contract))
                .unwrap_or_default();
            (
                state.rencrow_completion_state.clone(),
                state
                    .task_check_ledger
                    .snapshot_with_references(&referenced_task_checks),
                referenced_task_checks,
                state.rencrow_completion_resume_generation,
                state
                    .rencrow_completion_uncollected_children
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>(),
                state.active_task_scope.clone(),
                state
                    .task_patch_attempts
                    .iter()
                    .filter(|(_, attempt)| {
                        state
                            .active_task_scope
                            .as_ref()
                            .is_some_and(|scope| scope.same_execution(&attempt.scope))
                    })
                    .map(|(call, attempt)| (call.clone(), attempt.clone()))
                    .collect::<Vec<_>>(),
            )
        };
        let Some((root_task_id, contract)) = completion_state else {
            return Ok(());
        };
        let retained_call_ids = task_checks
            .iter()
            .map(|snapshot| snapshot.attempt.call_id.as_str())
            .collect::<HashSet<_>>();
        if referenced_task_checks
            .iter()
            .any(|call_id| !retained_call_ids.contains(call_id.as_str()))
        {
            self.mark_completion_paused_in_memory().await;
            return Err("completion snapshot references a missing task-check receipt".to_string());
        }
        if task_checks.len() > MAX_COMPLETION_TASK_CHECKS {
            self.mark_completion_paused_in_memory().await;
            return Err("completion snapshot exceeds the task-check bound".to_string());
        }
        let snapshot = Snapshot {
            version: 1,
            thread_id: self.thread_id().to_string(),
            root_task_id,
            state: contract,
            resume_generation,
            task_checks,
            active_scope,
            patch_attempts,
            uncollected_children,
            maintenance_repair_receipt: None,
        };
        let value = serde_json::to_value(snapshot).map_err(|error| error.to_string())?;
        if serde_json::to_vec(&value)
            .map_err(|error| error.to_string())?
            .len()
            > MAX_COMPLETION_SNAPSHOT_BYTES
        {
            self.mark_completion_paused_in_memory().await;
            return Err("completion snapshot exceeds its size bound".to_string());
        }
        let item = ResponseItemEnvelope {
            item: Box::new(AutonomousCompletionSnapshot).into_boxed_response_item(),
            metadata: Some(CodexHarnessMetadata {
                rencrow_completion: Some(value),
                ..Default::default()
            }),
        };
        if !self
            .persist_rollout_items(&[RolloutItem::ResponseItem(item.clone())])
            .await
        {
            let mut state = self.state.lock().await;
            state.rencrow_completion_paused = true;
            if let Some((_, contract)) = state.rencrow_completion_state.as_mut() {
                contract.paused = true;
            }
            return Err(
                "completion snapshot persistence failed; autonomous work paused".to_string(),
            );
        }
        self.state
            .lock()
            .await
            .history
            .record_annotated_items(&[item], TruncationPolicy::Tokens(1024));
        Ok(())
    }

    pub(crate) async fn restore_autonomous_completion_snapshot(&self, items: &[RolloutItem]) {
        let mut restored: Option<(String, CompletionState)> = None;
        let mut restored_checks = None;
        let mut restored_scope = None;
        let mut restored_patches = Vec::new();
        let mut restored_children = Vec::new();
        let mut restored_revision = None;
        let mut restored_generation = None;
        let mut restored_paused = false;
        let mut fail_closed = false;
        let mut response_items = Vec::new();
        let mut accepted_receipts = None;
        let authorized_legacy_scope_mismatch_hashes = items
            .iter()
            .filter_map(|item| {
                let RolloutItem::ResponseItem(envelope) = item else {
                    return None;
                };
                let metadata = envelope.metadata.as_ref()?;
                let value = metadata.rencrow_completion.as_ref()?;
                let is_host = matches!(&envelope.item, ResponseItem::Message {
                    role, internal_chat_message_metadata_passthrough: Some(kind), ..
                } if role == "developer" && kind.content_item_kinds.as_ref().is_some_and(|kinds| {
                    kinds.len() == 1 && kinds[0].0 == "rencrow.completion_snapshot"
                })) && !metadata.client_authored;
                if !is_host {
                    return None;
                }
                let snapshot = serde_json::from_value::<Snapshot>(value.clone()).ok()?;
                let receipt = snapshot.maintenance_repair_receipt.as_ref()?;
                validate_repair_receipt(receipt, &snapshot, &self.thread_id().to_string()).ok()?;
                Some(receipt.legacy_scope_mismatch_snapshot_sha256.clone())
            })
            .flatten()
            .collect::<HashSet<_>>();
        let declared_shutdown_paused_hashes = items
            .iter()
            .filter_map(|item| {
                let RolloutItem::ResponseItem(envelope) = item else {
                    return None;
                };
                let metadata = envelope.metadata.as_ref()?;
                let value = metadata.rencrow_completion.as_ref()?;
                let is_host = matches!(&envelope.item, ResponseItem::Message {
                    role, internal_chat_message_metadata_passthrough: Some(kind), ..
                } if role == "developer" && kind.content_item_kinds.as_ref().is_some_and(|kinds| {
                    kinds.len() == 1 && kinds[0].0 == "rencrow.completion_snapshot"
                })) && !metadata.client_authored;
                if !is_host {
                    return None;
                }
                let snapshot = serde_json::from_value::<Snapshot>(value.clone()).ok()?;
                let receipt = snapshot.maintenance_repair_receipt?;
                Some(receipt.shutdown_paused_snapshot_sha256)
            })
            .collect::<HashSet<_>>();
        let mut observed_legacy_scope_mismatch_hashes = HashSet::new();
        let mut last_valid_snapshot_sha256 = None;
        let mut host_snapshot_values = BTreeMap::<String, Value>::new();
        let mut shutdown_pair_seen = false;
        let mut interrupted_after_shutdown_pair = false;
        for item in items {
            // Validate each snapshot against receipts already committed at that position,
            // never against a future revision which may legitimately supersede its evidence.
            let checkpoint_changed = match item {
                RolloutItem::ResponseItem(envelope) => {
                    response_items.push(envelope.clone());
                    envelope
                        .metadata
                        .as_ref()
                        .is_some_and(|metadata| metadata.rencrow_compaction.is_some())
                }
                RolloutItem::Compacted(compacted) => {
                    if let Some(replacement) = &compacted.replacement_history {
                        response_items = replacement.clone();
                    }
                    true
                }
                _ => false,
            };
            if checkpoint_changed {
                match crate::compact::latest_task_state_in_history(
                    &response_items,
                    &self.thread_id().to_string(),
                ) {
                    Ok(task_state) => accepted_receipts = task_state,
                    Err(_) => fail_closed = true,
                }
            }
            match item {
                RolloutItem::ResponseItem(envelope) => {
                    let Some(metadata) = envelope.metadata.as_ref() else {
                        continue;
                    };
                    let Some(value) = metadata.rencrow_completion.as_ref() else {
                        continue;
                    };
                    // Model/user text, even with matching markers, is not a host snapshot.
                    let is_host = matches!(&envelope.item, ResponseItem::Message {
                        role, internal_chat_message_metadata_passthrough: Some(kind), ..
                    } if role == "developer" && kind.content_item_kinds.as_ref().is_some_and(|kinds| {
                        kinds.len() == 1 && kinds[0].0 == "rencrow.completion_snapshot"
                    })) && !metadata.client_authored;
                    if !is_host {
                        continue;
                    }
                    let parsed = serde_json::from_value::<Snapshot>(value.clone());
                    let Ok(snapshot) = parsed else {
                        fail_closed = true;
                        continue;
                    };
                    let snapshot_sha256 =
                        codex_history::compaction_candidate::digest(value).unwrap_or_default();
                    host_snapshot_values.insert(snapshot_sha256.clone(), value.clone());
                    if declared_shutdown_paused_hashes.contains(&snapshot_sha256) {
                        shutdown_pair_seen = true;
                    }
                    if snapshot.version != 1
                        || snapshot.thread_id != self.thread_id().to_string()
                        || snapshot.root_task_id.trim().is_empty()
                        || snapshot.root_task_id.len() > 4096
                        || snapshot.state.contract_revision == 0
                        || snapshot.task_checks.len() > MAX_COMPLETION_TASK_CHECKS
                        || snapshot.uncollected_children.len() > 256
                        || snapshot
                            .uncollected_children
                            .iter()
                            .any(|id| codex_protocol::ThreadId::try_from(id.as_str()).is_err())
                        || !serde_json::to_vec(&snapshot)
                            .is_ok_and(|bytes| bytes.len() <= MAX_COMPLETION_SNAPSHOT_BYTES)
                    {
                        fail_closed = true;
                        continue;
                    }
                    if snapshot.active_scope.as_ref().is_some_and(|scope| {
                        scope.task_id.trim().is_empty()
                            || scope.task_revision == 0
                            || scope.begin_call_id.trim().is_empty()
                    }) || snapshot.patch_attempts.len() > 256
                        || snapshot.patch_attempts.iter().any(|(call, attempt)| {
                            call.trim().is_empty()
                                || !snapshot
                                    .active_scope
                                    .as_ref()
                                    .is_some_and(|scope| scope.same_execution(&attempt.scope))
                                || attempt.paths.len() > 16
                        })
                    {
                        fail_closed = true;
                        continue;
                    }
                    let previous_snapshot = restored.as_ref().map(|(root, previous)| {
                        (
                            root.as_str(),
                            restored_revision.unwrap_or_default(),
                            restored_generation.unwrap_or_default(),
                            previous.paused,
                        )
                    });
                    if validate_snapshot_sequence_transition(previous_snapshot, &snapshot).is_err()
                    {
                        fail_closed = true;
                        continue;
                    }
                    if accepted_receipts.as_ref().is_some_and(|task_state| {
                        task_state.boundary.root_task_id != snapshot.root_task_id
                    }) {
                        fail_closed = true;
                        continue;
                    }
                    let receipts = accepted_receipts
                        .as_ref()
                        .map(|state| &state.completed)
                        .cloned()
                        .unwrap_or_default();
                    let scope_root_mismatch = snapshot
                        .active_scope
                        .as_ref()
                        .is_some_and(|scope| scope.root_task_id != snapshot.root_task_id);
                    if scope_root_mismatch {
                        observed_legacy_scope_mismatch_hashes.insert(snapshot_sha256.clone());
                        if authorized_legacy_scope_mismatch_hashes.contains(&snapshot_sha256)
                            && validate_snapshot_payload(
                                &snapshot,
                                &self.thread_id().to_string(),
                                &receipts,
                            )
                            .is_ok()
                        {
                            continue;
                        }
                        fail_closed = true;
                        continue;
                    }
                    if validate_snapshot_payload(
                        &snapshot,
                        &self.thread_id().to_string(),
                        &receipts,
                    )
                    .is_err()
                    {
                        fail_closed = true;
                        continue;
                    }
                    if let Some(receipt) = &snapshot.maintenance_repair_receipt
                        && (validate_repair_receipt(
                            receipt,
                            &snapshot,
                            &self.thread_id().to_string(),
                        )
                        .is_err()
                            || last_valid_snapshot_sha256.as_deref()
                                != Some(receipt.snapshot_before_sha256.as_str())
                            || receipt.resume_generation_before
                                != restored_generation.unwrap_or_default()
                            || receipt
                                .legacy_scope_mismatch_snapshot_sha256
                                .iter()
                                .cloned()
                                .collect::<HashSet<_>>()
                                != observed_legacy_scope_mismatch_hashes
                            || interrupted_after_shutdown_pair
                            || !maintenance_receipt_matches_history(
                                receipt,
                                value,
                                &host_snapshot_values,
                            ))
                    {
                        fail_closed = true;
                        continue;
                    }
                    if restored_generation != Some(snapshot.resume_generation) {
                        restored_paused = snapshot.state.paused;
                    } else {
                        restored_paused |= snapshot.state.paused;
                    }
                    restored_revision = Some(snapshot.state.contract_revision);
                    restored_generation = Some(snapshot.resume_generation);
                    restored_checks = Some(snapshot.task_checks);
                    restored_scope = snapshot.active_scope;
                    restored_patches = snapshot.patch_attempts;
                    restored_children = snapshot.uncollected_children;
                    restored = Some((snapshot.root_task_id, snapshot.state));
                    last_valid_snapshot_sha256 = Some(snapshot_sha256);
                    if snapshot.maintenance_repair_receipt.is_some() {
                        shutdown_pair_seen = false;
                    }
                }
                RolloutItem::EventMsg(EventMsg::TurnAborted(event))
                    if event.reason == TurnAbortReason::Interrupted =>
                {
                    if shutdown_pair_seen {
                        interrupted_after_shutdown_pair = true;
                    }
                    if restored.is_some() {
                        restored_paused = true;
                    }
                }
                _ => {}
            }
        }
        if let Some((root, mut contract)) = restored {
            let paused = {
                let mut current = self.state.lock().await;
                let current_generation = current.rencrow_completion_resume_generation;
                let snapshot_generation = restored_generation.unwrap_or_default();
                if current_generation > snapshot_generation {
                    if fail_closed {
                        current.rencrow_completion_paused = true;
                        if let Some((_, existing)) = current.rencrow_completion_state.as_mut() {
                            existing.paused = true;
                        }
                    }
                    return;
                }
                let current_paused = current.rencrow_completion_paused
                    || current
                        .rencrow_completion_state
                        .as_ref()
                        .is_some_and(|(_, existing)| existing.paused);
                let current_is_newer_or_same =
                    restored_generation.is_some_and(|generation| current_generation >= generation);
                fail_closed || restored_paused || (current_is_newer_or_same && current_paused)
            };
            contract.paused = paused;
            {
                let mut state = self.state.lock().await;
                if let Some(task_checks) = restored_checks
                    && state
                        .task_check_ledger
                        .restore_snapshot(task_checks)
                        .is_err()
                {
                    let _ = state.task_check_ledger.restore_snapshot(Vec::new());
                    contract.paused = true;
                }
                state.rencrow_completion_paused = contract.paused;
                state.rencrow_completion_resume_generation =
                    restored_generation.unwrap_or_default();
                state.rencrow_completion_uncollected_children =
                    restored_children.into_iter().collect();
                // The old cursor belongs to the original history, not its compacted projection.
                // active_task_scope() rebinds it to the retained host begin call after hydration.
                if let Some(scope) = restored_scope.as_mut() {
                    scope.start_history_cursor = 0;
                }
                state.rencrow_scope_needs_rebase = restored_scope.is_some();
                state.active_task_scope = restored_scope;
                state.task_patch_attempts = restored_patches
                    .into_iter()
                    .map(|(call, mut attempt)| {
                        attempt.scope.start_history_cursor = 0;
                        (call, attempt)
                    })
                    .collect();
                state.rencrow_completion_state = Some((root, contract));
            }
            self.revalidate_collected_completion_reviews().await;
        } else if fail_closed {
            self.state.lock().await.rencrow_completion_paused = true;
        }
    }

    async fn mark_completion_paused_in_memory(&self) {
        let mut state = self.state.lock().await;
        state.rencrow_completion_paused = true;
        if let Some((_, contract)) = state.rencrow_completion_state.as_mut() {
            contract.paused = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::tests::make_session_and_context;
    use codex_history::task_context::AcceptanceCondition;
    use codex_history::task_context::AcceptanceStatus;
    use codex_history::task_context::CompletionUnit;
    use codex_history::task_context::UnitStatus;
    use pretty_assertions::assert_eq;

    fn open_state(paused: bool) -> CompletionState {
        CompletionState {
            contract_revision: 1,
            units_completed_since_review: 0,
            plan_review_at: None,
            plan_reviews: Vec::new(),
            paused,
            consumed_host_events: Vec::new(),
            conditions: vec![AcceptanceCondition {
                id: "AC-1".into(),
                revision: 1,
                result: "Result".into(),
                scope: "Scope".into(),
                verification: "Check".into(),
                acceptance_criteria: "Pass".into(),
                status: AcceptanceStatus::Open,
                evidence: Vec::new(),
                unmet_reason: None,
            }],
            units: vec![CompletionUnit {
                id: "unit-1".into(),
                purpose: "Complete the result".into(),
                status: UnitStatus::Ready,
                kind: codex_history::task_context::CompletionUnitKind::Execution,
                condition_refs: vec!["AC-1".into()],
                depends_on: Vec::new(),
                evidence: Vec::new(),
                task_refs: Vec::new(),
                review_target: None,
                review_evidence: None,
                review_launch: None,
                review_decision_reason: None,
            }],
            current_unit: Some("unit-1".into()),
            failed_approaches: Vec::new(),
            blocker: None,
            next_action: Some("Start unit-1".into()),
            product_progress_at: None,
            runtime_recovery_at: None,
            activity_at: None,
        }
    }

    fn snapshot_item(value: serde_json::Value) -> RolloutItem {
        let item = Box::new(AutonomousCompletionSnapshot).into_boxed_response_item();
        RolloutItem::ResponseItem(ResponseItemEnvelope {
            item,
            metadata: Some(CodexHarnessMetadata {
                rencrow_completion: Some(value),
                ..Default::default()
            }),
        })
    }

    fn maintenance_repair_history(
        thread_id: &str,
        include_interrupted: bool,
        fake_shutdown_pair: bool,
    ) -> Vec<RolloutItem> {
        let shutdown_before = serde_json::json!({
            "version": 1, "thread_id": thread_id, "root_task_id": "root",
            "resume_generation": 0, "state": open_state(false),
            "active_scope": {"root_task_id":"old-root", "task_id":"unit-1", "task_revision":1,
                "begin_call_id":"begin-1", "start_history_cursor":0}
        });
        let shutdown_before_hash = maintenance_snapshot_digest(&shutdown_before).unwrap();
        let paused = serde_json::json!({
            "version": 1, "thread_id": thread_id, "root_task_id": "root",
            "resume_generation": 0, "state": open_state(true),
            "active_scope": {"root_task_id":"old-root", "task_id":"unit-1", "task_revision":1,
                "begin_call_id":"begin-1", "start_history_cursor":0}
        });
        let paused_hash = maintenance_snapshot_digest(&paused).unwrap();
        let current_paused = serde_json::json!({
            "version": 1, "thread_id": thread_id, "root_task_id": "root",
            "resume_generation": 0, "state": open_state(true), "active_scope": null
        });
        let current_paused_hash = maintenance_snapshot_digest(&current_paused).unwrap();
        let receipt = MaintenanceRepairReceipt {
            version: 1,
            thread_id: thread_id.into(),
            root_task_id: "root".into(),
            request_sha256: "1111111111111111111111111111111111111111111111111111111111111111"
                .into(),
            snapshot_before_sha256: current_paused_hash,
            shutdown_before_snapshot_sha256: if fake_shutdown_pair {
                "2222222222222222222222222222222222222222222222222222222222222222".into()
            } else {
                shutdown_before_hash.clone()
            },
            shutdown_paused_snapshot_sha256: paused_hash.clone(),
            legacy_scope_mismatch_snapshot_sha256: vec![shutdown_before_hash, paused_hash],
            evidence: [
                "deployment_result",
                "deployment_preflight",
                "maintenance_ticket",
                "pre_deploy_snapshot",
                "pre_deploy_facts",
                "deployer_source",
            ]
            .into_iter()
            .enumerate()
            .map(|(index, kind)| MaintenanceRepairEvidenceDigest {
                kind: kind.into(),
                path: format!("evidence-{index}.json"),
                sha256: format!("{:064x}", index + 1),
            })
            .collect(),
            resume_generation_before: 0,
            resume_generation_after: 1,
        };
        let repair = build_maintenance_repair_snapshot(&current_paused, receipt).unwrap();
        let mut items = vec![snapshot_item(shutdown_before), snapshot_item(paused)];
        if include_interrupted {
            items.push(RolloutItem::EventMsg(EventMsg::TurnAborted(
                codex_protocol::protocol::TurnAbortedEvent {
                    turn_id: Some("turn-between-pause-and-repair".into()),
                    reason: TurnAbortReason::Interrupted,
                    started_at: None,
                    completed_at: None,
                    duration_ms: None,
                },
            )));
        }
        items.push(snapshot_item(current_paused));
        items.push(snapshot_item(repair));
        items
    }

    #[test]
    fn maintenance_repair_inspection_rejects_same_generation_pause_clear() {
        let thread_id = "01a1c0b9-582b-71c0-a6d6-552f2d581a8e";
        let paused = serde_json::json!({
            "version": 1,
            "thread_id": thread_id,
            "root_task_id": "root",
            "resume_generation": 4,
            "state": open_state(true),
        });
        let unpaused_same_generation = serde_json::json!({
            "version": 1,
            "thread_id": thread_id,
            "root_task_id": "root",
            "resume_generation": 4,
            "state": open_state(false),
        });
        let target = maintenance_snapshot_digest(&unpaused_same_generation).unwrap();
        let items = vec![
            snapshot_item(paused),
            snapshot_item(unpaused_same_generation),
        ];

        let error = inspect_maintenance_repair_snapshot(&items, &target, thread_id, "root", false)
            .expect_err("same-generation resume must remain fail-closed");
        assert!(error.contains("restore monotonicity"));
    }

    #[test]
    fn maintenance_repair_inspection_rejects_checkpoint_failure_after_target_snapshot() {
        let thread_id = "01a1c0b9-582b-71c0-a6d6-552f2d581a8e";
        let target = serde_json::json!({
            "version": 1,
            "thread_id": thread_id,
            "root_task_id": "root",
            "resume_generation": 0,
            "state": open_state(false),
        });
        let target_hash = maintenance_snapshot_digest(&target).unwrap();
        let mut invalid_checkpoint = snapshot_item(serde_json::json!({
            "version": 1,
            "thread_id": thread_id,
            "root_task_id": "root",
            "resume_generation": 1,
            "state": open_state(false),
        }));
        let RolloutItem::ResponseItem(envelope) = &mut invalid_checkpoint else {
            panic!("repair item must be a response item");
        };
        envelope
            .metadata
            .as_mut()
            .expect("completion metadata")
            .rencrow_compaction = Some(serde_json::json!({"version":99}));

        let items = vec![snapshot_item(target), invalid_checkpoint];
        let error =
            inspect_maintenance_repair_snapshot(&items, &target_hash, thread_id, "root", false)
                .expect_err("invalid later compaction checkpoint must fail closed");
        assert!(error.contains("unsupported RenCrow compaction checkpoint version"));
    }

    #[test]
    fn maintenance_repair_inspection_ignores_an_uncommitted_task_checkpoint() {
        let thread_id = "01a1c0b9-582b-71c0-a6d6-552f2d581a8e";
        let mut checkpoint = ResponseItemEnvelope::new(ResponseItem::Message {
            id: None,
            role: "user".into(),
            content: vec![codex_protocol::models::ContentItem::InputText {
                text: "pending checkpoint".into(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: Some(
                codex_protocol::models::InternalChatMessageMetadataPassthrough {
                    content_item_kinds: Some(vec![codex_protocol::models::ContentItemKind(
                        "compaction.summary".into(),
                    )]),
                    ..Default::default()
                },
            ),
        });
        checkpoint
            .metadata
            .get_or_insert_default()
            .rencrow_compaction = Some(serde_json::json!({
            "version": 3,
            "mode": "capacity",
            "summary_hash": "a".repeat(64),
            "task_state": {"invalid": true},
            "transaction_following_items": 0
        }));
        let compacted = RolloutItem::Compacted(codex_history::CompactedItem {
            message: "pending checkpoint".into(),
            replacement_history: Some(vec![checkpoint]),
            retained_context: None,
            guardian_history: None,
            mcp_resource_origins: None,
            window_number: Some(1),
            first_window_id: Some("window-1".into()),
            previous_window_id: None,
            window_id: Some("window-1".into()),
            compaction_response_id: Some("pending-response".into()),
            latest_token_usage_record: None,
        });
        let target = serde_json::json!({
            "version": 1,
            "thread_id": thread_id,
            "root_task_id": "root",
            "resume_generation": 0,
            "state": open_state(false),
        });
        let target_hash = maintenance_snapshot_digest(&target).unwrap();
        let items = vec![compacted, snapshot_item(target)];

        assert!(
            inspect_maintenance_repair_snapshot(&items, &target_hash, thread_id, "root", false)
                .is_ok()
        );
    }

    #[tokio::test]
    async fn patch_proof_survives_snapshot_when_history_cursors_differ() {
        let (session, _) = make_session_and_context().await;
        let scope = crate::state::ActiveTaskScope {
            root_task_id: "root".into(),
            task_id: "unit-a".into(),
            task_revision: 1,
            begin_call_id: "begin-a".into(),
            start_history_cursor: 3,
        };
        let mut patch_scope = scope.clone();
        patch_scope.start_history_cursor = 4;
        {
            let mut state = session.state.lock().await;
            state.rencrow_completion_state = Some(("root".into(), open_state(false)));
            state.active_task_scope = Some(scope.clone());
            state.task_patch_attempts.insert(
                "patch-a".into(),
                crate::state::TaskPatchAttempt {
                    scope: patch_scope,
                    local_environment: true,
                    paths: vec![std::path::PathBuf::from("/tmp/verified-artifact.txt")],
                    completed_successfully: true,
                },
            );
        }
        session
            .persist_autonomous_completion_snapshot()
            .await
            .unwrap();
        let snapshot = {
            let state = session.state.lock().await;
            state
                .history
                .annotated_items()
                .iter()
                .find_map(|item| {
                    item.metadata
                        .as_ref()
                        .and_then(|metadata| metadata.rencrow_completion.clone())
                })
                .expect("captured host snapshot")
        };
        assert_eq!(snapshot["patch_attempts"].as_array().unwrap().len(), 1);
        {
            let mut state = session.state.lock().await;
            state.active_task_scope = None;
            state.task_patch_attempts.clear();
            state.history.replace(vec![serde_json::from_value(serde_json::json!({
                "type":"function_call", "call_id":"begin-a", "name":"task_context_boundary",
                "arguments":serde_json::json!({"action":"begin", "root_task_id":"root", "task_id":"unit-a", "task_revision":1}).to_string()
            })).unwrap()]);
        }
        session
            .restore_autonomous_completion_snapshot(&[snapshot_item(snapshot)])
            .await;
        let restored = session
            .active_task_scope()
            .await
            .expect("scope rebound to retained begin");
        assert!(restored.same_execution(&scope));
        assert!(!session.autonomous_completion_paused().await);
        let state = session.state.lock().await;
        let patch = state
            .task_patch_attempts
            .get("patch-a")
            .expect("host patch proof retained");
        assert!(restored.same_execution(&patch.scope));
        assert_eq!(
            restored.start_history_cursor,
            patch.scope.start_history_cursor
        );
        assert!(patch.completed_successfully);
    }

    #[tokio::test]
    async fn cold_restore_preserves_review_unit_target_without_fabricated_evidence() {
        let (session, _) = make_session_and_context().await;
        let mut contract = open_state(false);
        contract.units[0].kind = codex_history::task_context::CompletionUnitKind::Review;
        contract.units[0].review_target =
            Some(codex_history::task_context::CompletionReviewTarget {
                task_id: "task-a".into(),
                task_revision: 2,
            });
        contract.units[0].status = UnitStatus::Running;
        contract.units[0].review_launch =
            Some(codex_history::task_context::CompletionReviewLaunchState {
                spawn_call_id: "review-launch-call".into(),
                status: codex_history::task_context::CompletionReviewLaunchStatus::Unknown,
                reason: Some("spawn outcome could not be confirmed".into()),
            });
        {
            session.state.lock().await.rencrow_completion_state = Some(("root".into(), contract));
        }
        session
            .persist_autonomous_completion_snapshot()
            .await
            .expect("persist typed review target");
        let snapshot = {
            let mut state = session.state.lock().await;
            let snapshot = state
                .history
                .annotated_items()
                .iter()
                .find_map(|item| {
                    item.metadata
                        .as_ref()
                        .and_then(|metadata| metadata.rencrow_completion.clone())
                })
                .expect("captured host snapshot");
            state.rencrow_completion_state = None;
            snapshot
        };

        session
            .restore_autonomous_completion_snapshot(&[snapshot_item(snapshot)])
            .await;
        let restored = session.rencrow_completion_state("root").await.unwrap();
        assert_eq!(
            restored.units[0].kind,
            codex_history::task_context::CompletionUnitKind::Review
        );
        assert_eq!(
            restored.units[0].review_target,
            Some(codex_history::task_context::CompletionReviewTarget {
                task_id: "task-a".into(),
                task_revision: 2,
            })
        );
        assert!(restored.units[0].review_evidence.is_none());
        assert_eq!(
            restored.units[0].review_launch,
            Some(codex_history::task_context::CompletionReviewLaunchState {
                spawn_call_id: "review-launch-call".into(),
                status: codex_history::task_context::CompletionReviewLaunchStatus::Unknown,
                reason: Some("spawn outcome could not be confirmed".into()),
            })
        );
        assert!(!session.autonomous_completion_paused().await);
    }

    #[tokio::test]
    async fn cold_restore_preserves_a_persisted_stop() {
        let (session, _) = make_session_and_context().await;
        let snapshot = serde_json::json!({
            "version": 1,
            "thread_id": session.thread_id().to_string(),
            "root_task_id": "root",
            "state": open_state(true),
        });
        session
            .restore_autonomous_completion_snapshot(&[snapshot_item(snapshot)])
            .await;

        assert!(session.autonomous_completion_paused().await);
        assert_eq!(
            session.rencrow_completion_state("root").await,
            Some(open_state(true))
        );
    }

    #[tokio::test]
    async fn invalid_first_snapshot_fails_closed_without_prior_valid_state() {
        let (session, _) = make_session_and_context().await;
        let snapshot = serde_json::json!({
            "version": 99,
            "thread_id": session.thread_id().to_string(),
            "root_task_id": "root",
            "state": open_state(false),
        });
        session
            .restore_autonomous_completion_snapshot(&[snapshot_item(snapshot)])
            .await;

        assert!(session.autonomous_completion_paused().await);
        assert_eq!(session.rencrow_completion_state("root").await, None);
    }

    #[tokio::test]
    async fn older_or_invalid_later_snapshots_cannot_rewind_state_or_clear_stop() {
        let (session, _) = make_session_and_context().await;
        let mut newer = open_state(false);
        newer.contract_revision = 2;
        let snapshots = [
            snapshot_item(serde_json::json!({
                "version": 1,
                "thread_id": session.thread_id().to_string(),
                "root_task_id": "root",
                "state": newer,
            })),
            snapshot_item(serde_json::json!({
                "version": 1,
                "thread_id": session.thread_id().to_string(),
                "root_task_id": "root",
                "state": open_state(false),
            })),
        ];
        session
            .restore_autonomous_completion_snapshot(&snapshots)
            .await;

        assert!(session.autonomous_completion_paused().await);
        let restored = session.rencrow_completion_state("root").await.unwrap();
        assert_eq!(restored.contract_revision, 2);
        assert!(restored.paused);
    }

    #[tokio::test]
    async fn cold_restore_keeps_an_unfinished_task_check_pending() {
        let (session, _) = make_session_and_context().await;
        let snapshot = serde_json::json!({
            "version": 1,
            "thread_id": session.thread_id().to_string(),
            "root_task_id": "root",
            "state": open_state(false),
            "task_checks": [{
                "task_id": "task-a",
                "task_revision": 1,
                "attempt": {"call_id": "call-a", "status": "Pending"}
            }]
        });
        session
            .restore_autonomous_completion_snapshot(&[snapshot_item(snapshot)])
            .await;

        let state = session.state.lock().await;
        let attempt = state
            .task_check_ledger
            .latest_for_task("task-a", 1)
            .expect("pending host check is restored");
        assert!(matches!(
            &attempt.status,
            crate::task_check::TaskCheckAttemptStatus::Pending
        ));
    }

    #[tokio::test]
    async fn higher_host_resume_generation_can_clear_a_persisted_stop() {
        let (session, _) = make_session_and_context().await;
        let thread_id = session.thread_id().to_string();
        let paused = serde_json::json!({
            "version": 1,
            "thread_id": thread_id,
            "root_task_id": "root",
            "resume_generation": 0,
            "state": open_state(true),
        });
        let resumed = serde_json::json!({
            "version": 1,
            "thread_id": session.thread_id().to_string(),
            "root_task_id": "root",
            "resume_generation": 1,
            "state": open_state(false),
        });
        session
            .restore_autonomous_completion_snapshot(&[
                snapshot_item(paused),
                snapshot_item(resumed),
            ])
            .await;

        assert!(!session.autonomous_completion_paused().await);
        assert!(
            session
                .rencrow_completion_state("root")
                .await
                .is_some_and(|state| !state.paused)
        );
        assert_eq!(
            session
                .state
                .lock()
                .await
                .rencrow_completion_resume_generation,
            1
        );
    }

    #[tokio::test]
    async fn host_repair_receipt_clears_only_declared_legacy_scope_mismatch() {
        let (session, _) = make_session_and_context().await;
        session
            .restore_autonomous_completion_snapshot(&maintenance_repair_history(
                &session.thread_id().to_string(),
                false,
                false,
            ))
            .await;

        assert!(
            !session.autonomous_completion_paused().await,
            "a valid host repair receipt should exempt only its exact legacy scope mismatch"
        );
        assert_eq!(
            session
                .state
                .lock()
                .await
                .rencrow_completion_resume_generation,
            1
        );
    }

    #[tokio::test]
    async fn repair_receipt_cannot_clear_with_unresolved_shutdown_pair_hashes() {
        let (session, _) = make_session_and_context().await;
        session
            .restore_autonomous_completion_snapshot(&maintenance_repair_history(
                &session.thread_id().to_string(),
                false,
                true,
            ))
            .await;
        assert!(session.autonomous_completion_paused().await);
    }

    #[tokio::test]
    async fn repair_receipt_cannot_clear_after_interrupted_turn() {
        let (session, _) = make_session_and_context().await;
        session
            .restore_autonomous_completion_snapshot(&maintenance_repair_history(
                &session.thread_id().to_string(),
                true,
                false,
            ))
            .await;
        assert!(session.autonomous_completion_paused().await);
    }

    #[tokio::test]
    async fn same_generation_snapshot_cannot_clear_a_persisted_stop() {
        let (session, _) = make_session_and_context().await;
        let thread_id = session.thread_id().to_string();
        let paused = serde_json::json!({
            "version": 1,
            "thread_id": thread_id,
            "root_task_id": "root",
            "resume_generation": 3,
            "state": open_state(true),
        });
        let invalid_resume = serde_json::json!({
            "version": 1,
            "thread_id": session.thread_id().to_string(),
            "root_task_id": "root",
            "resume_generation": 3,
            "state": open_state(false),
        });
        session
            .restore_autonomous_completion_snapshot(&[
                snapshot_item(paused),
                snapshot_item(invalid_resume),
            ])
            .await;

        assert!(session.autonomous_completion_paused().await);
        assert!(
            session
                .rencrow_completion_state("root")
                .await
                .is_some_and(|state| state.paused)
        );
        assert_eq!(
            session
                .state
                .lock()
                .await
                .rencrow_completion_resume_generation,
            3
        );
    }
}
