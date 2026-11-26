// Modified by RenCrow Switch Core, 2026-11-27: reserve and activate host-selected Common S1 iterations.
use super::session::Session;
use super::turn_context::TurnContext;
use codex_history::task_context::AcceptanceStatus;
use codex_history::task_context::CompletionState;
use codex_history::task_context::CompletionUnit;
use codex_history::task_context::CompletionUnitKind;
use codex_history::task_context::ExecutionOwnerV1;
use codex_history::task_context::TaskRunStatusV1;
use codex_history::task_context::UnitStatus;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::SessionSource;
use std::collections::BTreeSet;

/// Return the current host-reserved execution unit for a target-rebound Common task.
/// The outer task board is host-owned while Common v1 is active, so this predicate
/// accepts only the exact finite unit shape produced by the host status refresh.
pub(crate) fn reserved_common_harness_execution(
    completion: &CompletionState,
) -> Option<&CompletionUnit> {
    let common = completion.common_harness.as_ref()?;
    if common.request.execution_owner != ExecutionOwnerV1::StandaloneSwitch
        || !matches!(
            common.run_state.status,
            TaskRunStatusV1::Queued | TaskRunStatusV1::Running
        )
    {
        return None;
    }
    let unit_id = completion.current_unit.as_deref()?;
    let suffix = unit_id.strip_prefix(&format!("{}-iteration-", common.request.task_id))?;
    let counter = suffix.parse::<u32>().ok().filter(|counter| *counter > 0)?;
    if !(2..=256).contains(&counter) || counter.to_string() != suffix {
        return None;
    }

    let unit = completion.units.iter().find(|unit| unit.id == unit_id)?;
    if unit.kind != CompletionUnitKind::Execution || unit.status != UnitStatus::Ready {
        return None;
    }
    let required_conditions = common
        .request
        .conditions
        .iter()
        .filter(|condition| condition.required)
        .map(|condition| condition.condition_id.as_str())
        .collect::<BTreeSet<_>>();
    let unit_conditions = unit
        .condition_refs
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    if required_conditions.is_empty()
        || unit_conditions.len() != unit.condition_refs.len()
        || unit_conditions != required_conditions
        || unit.depends_on.len() != 1
        || !unit.evidence.is_empty()
        || !unit.task_refs.is_empty()
    {
        return None;
    }
    // The existing stale-proof path retains historical receipts on Open conditions.
    // Only fresh Common evidence can satisfy them; history does not block a new unit.
    if required_conditions.iter().any(|condition_id| {
        completion
            .conditions
            .iter()
            .find(|condition| condition.id == *condition_id)
            .is_none_or(|condition| condition.status != AcceptanceStatus::Open)
    }) {
        return None;
    }

    let predecessor = completion.units.iter().find(|candidate| {
        candidate.id == unit.depends_on[0]
            && candidate.kind == CompletionUnitKind::Execution
            && candidate.status == UnitStatus::Done
            && candidate.id != unit.id
    })?;
    debug_assert_ne!(predecessor.id, unit.id);
    Some(unit)
}

impl Session {
    pub(crate) async fn reserved_common_harness_execution_for_turn(
        &self,
        turn: &TurnContext,
    ) -> Option<(String, String, u64)> {
        if !turn.config.rencrow_compaction
            || !turn.config.rencrow_autonomous_completion
            || !matches!(
                turn.session_source,
                SessionSource::Cli | SessionSource::VSCode | SessionSource::Exec
            )
            || self.is_read_only_review_actor_for_turn(turn).await
            || self.active_task_scope().await.is_some()
        {
            return None;
        }
        let (root, common) = self.common_harness_tool_state_for_turn(turn).await?;
        let runtime_owner =
            super::common_harness::common_harness_runtime_owner_id(&self.thread_id.to_string());
        if common.run_state.owner_id != runtime_owner {
            return None;
        }
        let completion = self.rencrow_completion_state(&root).await?;
        let unit = reserved_common_harness_execution(&completion)?;
        Some((root, unit.id.clone(), 1))
    }

    pub(crate) async fn common_harness_reserved_begin_matches(
        &self,
        turn: &TurnContext,
        root_task_id: &str,
        task_id: &str,
        task_revision: u64,
    ) -> bool {
        self.reserved_common_harness_execution_for_turn(turn)
            .await
            .is_some_and(|(root, reserved_task, reserved_revision)| {
                root == root_task_id
                    && reserved_task == task_id
                    && reserved_revision == task_revision
            })
    }

    /// Activate only the exact real Begin call that the host reserved in the canonical plan.
    pub(crate) async fn begin_reserved_common_harness_execution(
        &self,
        root_task_id: &str,
        task_id: &str,
        task_revision: u64,
        begin_call_id: &str,
    ) -> Result<(), String> {
        let _owner_guard = self.completion_owner_guard().await;
        if self.active_task_scope().await.is_some() {
            return Err("a common-harness execution scope is already active".into());
        }
        let (canonical_root, canonical) = {
            let state = self.state.lock().await;
            if state.pending_task_boundary.is_some() {
                return Err(
                    "cannot begin a reserved common-harness unit while a task boundary is pending"
                        .into(),
                );
            }
            let (root, completion) = state.rencrow_completion_state.as_ref().ok_or_else(|| {
                "reserved common-harness completion state is unavailable".to_string()
            })?;
            (root.clone(), completion.clone())
        };
        if canonical_root != root_task_id {
            return Err("reserved common-harness root task does not match canonical state".into());
        }
        let completion = super::completion_review::project_completion_state(&canonical)
            .map_err(|error| format!("read reserved common-harness unit: {error}"))?;
        let common = completion
            .common_harness
            .as_ref()
            .ok_or_else(|| "reserved common-harness v1 state is unavailable".to_string())?;
        let expected_owner =
            super::common_harness::common_harness_runtime_owner_id(&self.thread_id.to_string());
        if common.run_state.owner_id != expected_owner
            || common.request.execution_owner != ExecutionOwnerV1::StandaloneSwitch
        {
            return Err("current runtime does not own the common-harness writer epoch".into());
        }
        let reserved = reserved_common_harness_execution(&completion)
            .ok_or_else(|| "no host-reserved common-harness execution unit is ready".to_string())?;
        if reserved.id != task_id || task_revision != 1 {
            return Err(
                "Begin does not match the exact host-reserved common-harness execution unit".into(),
            );
        }

        let history = self.clone_history().await;
        let matching_calls = history
            .raw_items()
            .filter_map(|item| match item {
                ResponseItem::FunctionCall {
                    name,
                    namespace,
                    arguments,
                    call_id,
                    ..
                } if call_id == begin_call_id => Some((name, namespace, arguments)),
                _ => None,
            })
            .collect::<Vec<_>>();
        if matching_calls.len() != 1 {
            return Err("reserved Begin must identify one exact recorded model tool call".into());
        }
        let (name, namespace, arguments) = matching_calls[0];
        if name != "task_context_boundary" || namespace.is_some() {
            return Err("reserved Begin call is not the task-context boundary tool".into());
        }
        let args: serde_json::Value = serde_json::from_str(arguments)
            .map_err(|error| format!("parse reserved Begin call arguments: {error}"))?;
        if args.get("action").and_then(serde_json::Value::as_str) != Some("begin")
            || args.get("root_task_id").and_then(serde_json::Value::as_str) != Some(root_task_id)
            || args.get("task_id").and_then(serde_json::Value::as_str) != Some(task_id)
            || args
                .get("task_revision")
                .and_then(serde_json::Value::as_u64)
                != Some(task_revision)
            || args
                .get("common_harness_request")
                .is_some_and(|value| !value.is_null())
            || args
                .get("completion_state")
                .is_some_and(|value| !value.is_null())
        {
            return Err("recorded Begin arguments do not match the host reservation".into());
        }

        self.begin_task_scope(
            root_task_id.to_string(),
            task_id.to_string(),
            task_revision,
            begin_call_id.to_string(),
            false,
        )
        .await?;
        if let Err(error) = self.persist_autonomous_completion_snapshot().await {
            self.mark_completion_paused_in_memory().await;
            return Err(error);
        }
        Ok(())
    }
}
