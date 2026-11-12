//! V3 checkpoint envelope for deterministic task boundaries and capacity compaction.
//!
//! V2 remains the strict payload for model generated capacity summaries. V3 adds only the
//! task snapshot and source fence needed to resume a task boundary without inventing a model
//! response receipt.

use crate::archive_reference::content_sha256;
use crate::compaction_checkpoint_metadata::RenCrowCompactionMetadataV2;
use crate::task_context::StoredTaskState;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskCheckpointMode {
    TaskBoundary,
    Capacity,
}

/// Exact identity of one original history item selected for removal by a task boundary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskBoundarySourceRef {
    pub id: String,
    pub hash: String,
}

/// Digest of the source history and the exact item set removed at a task boundary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskBoundarySourceFence {
    pub history_hash: String,
    pub removed_refs: Vec<TaskBoundarySourceRef>,
}

/// Thin V3 envelope. `task_state` is the last host-validated task snapshot and is carried
/// unchanged by subsequent capacity checkpoints. The V2 payload exists only for capacity mode.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenCrowTaskCheckpointMetadataV3 {
    pub version: u32,
    pub mode: TaskCheckpointMode,
    pub summary_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_state: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_fence: Option<TaskBoundarySourceFence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity_metadata: Option<RenCrowCompactionMetadataV2>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transaction_following_items: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub committed_transaction_hash: Option<String>,
}

impl RenCrowTaskCheckpointMetadataV3 {
    pub fn task_boundary(
        summary_text: &str,
        task_state: Value,
        source_fence: TaskBoundarySourceFence,
    ) -> Self {
        Self {
            version: 3,
            mode: TaskCheckpointMode::TaskBoundary,
            summary_hash: content_sha256(summary_text),
            task_state: Some(task_state),
            source_fence: Some(source_fence),
            capacity_metadata: None,
            transaction_following_items: None,
            committed_transaction_hash: None,
        }
    }

    /// Wrap a validated V2 capacity checkpoint while carrying the last accepted task snapshot.
    pub fn capacity(
        summary_text: &str,
        task_state: Option<Value>,
        metadata: RenCrowCompactionMetadataV2,
    ) -> Self {
        Self {
            version: 3,
            mode: TaskCheckpointMode::Capacity,
            summary_hash: content_sha256(summary_text),
            task_state,
            source_fence: None,
            transaction_following_items: metadata.transaction_following_items,
            committed_transaction_hash: metadata.committed_transaction_hash.clone(),
            capacity_metadata: Some(metadata),
        }
    }

    pub fn parse_and_validate(
        value: Value,
        current_thread_id: &str,
        summary_text: &str,
    ) -> Result<Self, String> {
        let metadata: Self = serde_json::from_value(value)
            .map_err(|error| format!("invalid V3 task checkpoint metadata: {error}"))?;
        metadata.validate_for_checkpoint(current_thread_id, summary_text)?;
        Ok(metadata)
    }

    pub fn validate_for_checkpoint(
        &self,
        current_thread_id: &str,
        summary_text: &str,
    ) -> Result<(), String> {
        if self.version != 3 {
            return Err("unsupported RenCrow task checkpoint metadata version".into());
        }
        if current_thread_id.trim().is_empty() {
            return Err("checkpoint thread ID must be nonempty".into());
        }
        if !is_sha256(&self.summary_hash) || content_sha256(summary_text) != self.summary_hash {
            return Err("checkpoint summary hash does not match its message body".into());
        }
        if let Some(state) = &self.task_state {
            StoredTaskState::parse_and_validate(state.clone(), current_thread_id)?;
        }
        match self.mode {
            TaskCheckpointMode::TaskBoundary => {
                if self.task_state.is_none() {
                    return Err("task boundary checkpoint has no task state".into());
                }
                if self.capacity_metadata.is_some() {
                    return Err(
                        "task boundary checkpoint cannot contain V2 capacity metadata".into(),
                    );
                }
                let fence = self
                    .source_fence
                    .as_ref()
                    .ok_or_else(|| "task boundary checkpoint has no source fence".to_owned())?;
                validate_source_fence(fence)?;
            }
            TaskCheckpointMode::Capacity => {
                if self.source_fence.is_some() {
                    return Err(
                        "capacity checkpoint cannot carry a task-boundary source fence".into(),
                    );
                }
                let capacity = self
                    .capacity_metadata
                    .as_ref()
                    .ok_or_else(|| "capacity checkpoint has no V2 metadata".to_owned())?;
                capacity.validate_for_checkpoint(current_thread_id, summary_text)?;
                if capacity.transaction_following_items != self.transaction_following_items
                    || capacity.committed_transaction_hash != self.committed_transaction_hash
                {
                    return Err("V3 and V2 transaction fields do not agree".into());
                }
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

    /// Return only the embedded capacity payload. Task-boundary checkpoints are not V2 summaries.
    pub fn into_capacity_metadata(self) -> Result<RenCrowCompactionMetadataV2, String> {
        if self.mode != TaskCheckpointMode::Capacity {
            return Err("task boundary checkpoint has no capacity metadata".into());
        }
        self.capacity_metadata
            .ok_or_else(|| "capacity checkpoint has no V2 metadata".into())
    }
}

fn validate_source_fence(fence: &TaskBoundarySourceFence) -> Result<(), String> {
    if !is_sha256(&fence.history_hash) {
        return Err("task boundary source fence is incomplete".into());
    }
    let mut ids = HashSet::with_capacity(fence.removed_refs.len());
    for reference in &fence.removed_refs {
        if reference.id.trim().is_empty()
            || !is_sha256(&reference.hash)
            || !ids.insert(reference.id.as_str())
        {
            return Err("task boundary source references are invalid or duplicated".into());
        }
    }
    Ok(())
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
#[path = "task_checkpoint_metadata_tests.rs"]
mod tests;
