use crate::agent::child_config::SpawnConfigOptions;
use crate::agent::child_config::SpawnConfigVersion;
use crate::agent::child_config::prepare_agent_spawn_config;
use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::handlers::task_context_boundary_spec::TASK_CONTEXT_BOUNDARY_TOOL_NAME;
use crate::tools::handlers::task_context_boundary_spec::create_task_context_boundary_tool;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_history::ResponseItemEnvelope;
use codex_history::compaction_candidate::digest;
use codex_history::compaction_candidate::history_digest;
use codex_history::task_context::AcceptedTaskReceipt;
use codex_history::task_context::BoundaryCandidate;
use codex_history::task_context::BoundaryMode;
use codex_history::task_context::CompletionEvidenceRef;
use codex_history::task_context::CompletionUnitKind;
use codex_history::task_context::HostBoundaryFacts;
use codex_history::task_context::StoredTaskState;
use codex_history::task_context::TaskState;
use codex_history::task_context::TaskStatus;
use codex_history::task_context::UnitStatus;
use codex_history::task_context::validate_boundary;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::SessionSource;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use codex_utils_output_truncation::approx_token_count;
use serde::Deserialize;
use std::collections::HashSet;

pub(crate) struct TaskContextBoundaryHandler {
    autonomous_completion: bool,
}

impl TaskContextBoundaryHandler {
    pub(crate) fn new(autonomous_completion: bool) -> Self {
        Self {
            autonomous_completion,
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum TaskContextBoundaryArgs {
    Begin {
        root_task_id: String,
        task_id: String,
        task_revision: u64,
        #[serde(default)]
        completion_state: Option<serde_json::Value>,
    },
    Update {
        root_task_id: String,
        completion_state: serde_json::Value,
    },
    Status {
        #[serde(default)]
        root_task_id: Option<String>,
    },
    PlanReview {
        root_task_id: String,
        previous_plan: String,
        revised_plan: String,
        reason: String,
    },
    Review {
        target_task_id: String,
    },
    AdoptReview {
        unit_id: String,
        reason: String,
    },
    Accept {
        #[serde(default)]
        goal: Option<String>,
        #[serde(default)]
        tasks: Option<Vec<TaskState>>,
        #[serde(default)]
        current_task_id: Option<String>,
        #[serde(default)]
        decisions: Option<Vec<String>>,
        #[serde(default)]
        constraints: Option<Vec<String>>,
        result: String,
        #[serde(default)]
        satisfied_conditions: Option<Vec<String>>,
    },
}

impl ToolExecutor<ToolInvocation> for TaskContextBoundaryHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(TASK_CONTEXT_BOUNDARY_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        create_task_context_boundary_tool(self.autonomous_completion)
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            if !matches!(&invocation.payload, ToolPayload::Function { .. }) {
                return Err(FunctionCallError::RespondToModel(
                    "task_context_boundary received unsupported payload".into(),
                ));
            }
            if !matches!(
                &invocation.turn.session_source,
                SessionSource::Cli | SessionSource::VSCode | SessionSource::Exec
            ) {
                return Err(FunctionCallError::RespondToModel(
                    "task_context_boundary is available only to an authorized parent session"
                        .into(),
                ));
            }
            if !invocation.turn.config.rencrow_compaction {
                return Err(FunctionCallError::RespondToModel(
                    "task_context_boundary requires RenCrow compaction to be enabled".into(),
                ));
            }

            let ToolPayload::Function { arguments } = &invocation.payload else {
                unreachable!("payload was checked above")
            };
            let raw_arguments = arguments.clone();
            let args: TaskContextBoundaryArgs = parse_arguments(arguments)?;
            match args {
                TaskContextBoundaryArgs::Begin {
                    root_task_id,
                    task_id,
                    task_revision,
                    completion_state,
                } => {
                    if root_task_id.trim().is_empty()
                        || task_id.trim().is_empty()
                        || task_revision == 0
                    {
                        return Err(FunctionCallError::RespondToModel(
                            "root_task_id, task_id, and positive task_revision are required".into(),
                        ));
                    }
                    let active_scope = invocation.session.active_task_scope().await;
                    if let Some(active) = active_scope.as_ref()
                        && !(active.root_task_id == root_task_id
                            && active.task_id == task_id
                            && active.task_revision == task_revision)
                        && !(invocation.turn.config.rencrow_autonomous_completion
                            && active.root_task_id == root_task_id
                            && active.task_id == active.root_task_id)
                    {
                        return Err(FunctionCallError::RespondToModel(format!(
                            "task scope `{}` revision {} is already active; finish or accept it before beginning `{}` revision {}",
                            active.task_id, active.task_revision, task_id, task_revision
                        )));
                    }
                    if let Some(active) = active_scope
                        && active.root_task_id == root_task_id
                        && (active.task_id != task_id || active.task_revision != task_revision)
                    {
                        if invocation.session.pending_task_boundary().await.is_some() {
                            return Err(FunctionCallError::RespondToModel(
                                "cannot switch execution scope while a task boundary is pending"
                                    .into(),
                            ));
                        }
                        if !invocation
                            .session
                            .services
                            .unified_exec_manager
                            .list_processes()
                            .await
                            .is_empty()
                        {
                            return Err(FunctionCallError::RespondToModel(
                                "cannot switch execution scope while a unified exec process is running".into(),
                            ));
                        }
                        if invocation
                            .session
                            .services
                            .agent_control
                            .has_registered_agents()
                        {
                            let agents = invocation
                                .session
                                .services
                                .agent_control
                                .list_agents(&invocation.turn.session_source, None)
                                .await
                                .map_err(|error| {
                                    FunctionCallError::RespondToModel(error.to_string())
                                })?;
                            if agents.iter().any(|agent| {
                                matches!(
                                    agent.status,
                                    AgentStatus::PendingInit
                                        | AgentStatus::Running
                                        | AgentStatus::Interrupted
                                )
                            }) {
                                return Err(FunctionCallError::RespondToModel(
                                    "cannot switch execution scope while a child agent is unresolved".into(),
                                ));
                            }
                        }
                        let history = invocation.session.clone_history().await;
                        let items = history.annotated_items();
                        let unresolved = unresolved_calls_since(
                            items,
                            active.start_history_cursor,
                            &invocation.call_id,
                        );
                        if !unresolved.is_empty() {
                            return Err(FunctionCallError::RespondToModel(format!(
                                "cannot switch execution scope while tool effects are unresolved: {}",
                                unresolved.join("; "),
                            )));
                        }
                        let exec_calls = items.iter().enumerate().filter_map(|(index, envelope)| {
                            (index >= active.start_history_cursor).then_some((index, &envelope.item)).and_then(|(index, item)| {
                                matches!(item, ResponseItem::FunctionCall { name, .. } if name == "exec_command")
                                    .then_some((index, item))
                            })
                        }).collect::<Vec<_>>();
                        if !exec_calls.is_empty() {
                            let check = invocation
                                .session
                                .latest_task_check_for_scope(&active.task_id, active.task_revision)
                                .await;
                            let Some(crate::task_check::TaskCheckAttemptStatus::Finished(receipt)) =
                                check.map(|attempt| attempt.status)
                            else {
                                return Err(FunctionCallError::RespondToModel(
                                    "cannot switch execution scope while root edits lack a completed verification receipt".into(),
                                ));
                            };
                            let check_position = items.iter().position(|envelope| {
                                matches!(
                                &envelope.item,
                                    ResponseItem::FunctionCall { name, call_id, .. }
                                        if name == "exec_command" && call_id == &receipt.call_id
                                )
                            });
                            if !receipt.succeeded
                                || receipt.exit_code != 0
                                || check_position.is_none_or(|position| {
                                    exec_calls.iter().any(|(index, _)| *index > position)
                                })
                            {
                                return Err(FunctionCallError::RespondToModel(
                                    "cannot switch execution scope while root edits are unverified or verification is stale".into(),
                                ));
                            }
                            let paths = receipt
                                .artifact_after
                                .iter()
                                .map(|stamp| stamp.path.clone())
                                .collect::<Vec<_>>();
                            let current = crate::task_check::hash_workspace_artifacts(
                                &receipt.workspace_root,
                                &paths,
                            )
                            .ok_or_else(|| {
                                FunctionCallError::RespondToModel(
                                    "root verification artifacts are unavailable".into(),
                                )
                            })?;
                            if current != receipt.artifact_after
                                || receipt.artifact_before != receipt.artifact_after
                            {
                                return Err(FunctionCallError::RespondToModel(
                                    "cannot switch execution scope while root verification artifacts have changed".into(),
                                ));
                            }
                        }
                    }
                    invocation
                        .session
                        .validate_task_scope_begin(
                            &root_task_id,
                            &task_id,
                            task_revision,
                            invocation.turn.config.rencrow_autonomous_completion,
                        )
                        .await
                        .map_err(FunctionCallError::RespondToModel)?;
                    if invocation.turn.config.rencrow_autonomous_completion
                        && let Some(completion_state) = completion_state
                    {
                        invocation
                            .session
                            .set_rencrow_completion_state(root_task_id.clone(), completion_state)
                            .await
                            .map_err(FunctionCallError::RespondToModel)?;
                    }
                    invocation
                        .session
                        .begin_task_scope(
                            root_task_id,
                            task_id,
                            task_revision,
                            invocation.call_id,
                            invocation.turn.config.rencrow_autonomous_completion,
                        )
                        .await
                        .map_err(FunctionCallError::RespondToModel)?;
                    if invocation.turn.config.rencrow_autonomous_completion {
                        invocation
                            .session
                            .persist_autonomous_completion_snapshot()
                            .await
                            .map_err(FunctionCallError::RespondToModel)?;
                    }
                    Ok(boxed_tool_output(FunctionToolOutput::from_text(
                        "Task scope begun. This records a host start boundary and does not accept or complete the task."
                            .into(),
                        Some(true),
                    )))
                }
                TaskContextBoundaryArgs::Update {
                    root_task_id,
                    completion_state,
                } => {
                    if !invocation.turn.config.rencrow_autonomous_completion {
                        return Err(FunctionCallError::RespondToModel(
                            "autonomous completion is not enabled for this session".into(),
                        ));
                    }
                    invocation
                        .session
                        .set_rencrow_completion_state(root_task_id.clone(), completion_state)
                        .await
                        .map_err(FunctionCallError::RespondToModel)?;
                    let state = invocation
                        .session
                        .rencrow_completion_state(&root_task_id)
                        .await
                        .ok_or_else(|| {
                            FunctionCallError::RespondToModel(
                                "host completion state disappeared after update".into(),
                            )
                        })?;
                    crate::autonomous_progress::report(
                        &invocation.session,
                        &invocation.turn,
                        &state,
                    )
                    .await;
                    let rendered = serde_json::to_string(&state)
                        .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
                    Ok(boxed_tool_output(FunctionToolOutput::from_text(
                        rendered,
                        Some(true),
                    )))
                }
                TaskContextBoundaryArgs::Status { root_task_id } => {
                    if !invocation.turn.config.rencrow_autonomous_completion {
                        return Err(FunctionCallError::RespondToModel(
                            "autonomous completion is not enabled for this session".into(),
                        ));
                    }
                    let root_task_id = match root_task_id {
                        Some(root_task_id) => root_task_id,
                        None => invocation
                            .session
                            .latest_rencrow_task_state()
                            .await
                            .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?
                            .map(|state| state.boundary.root_task_id)
                            .ok_or_else(|| {
                                FunctionCallError::RespondToModel(
                                    "no host-registered completion contract is available".into(),
                                )
                            })?,
                    };
                    let state = invocation
                        .session
                        .rencrow_completion_state(&root_task_id)
                        .await
                        .ok_or_else(|| {
                            FunctionCallError::RespondToModel(
                                "no host-registered completion contract is available".into(),
                            )
                        })?;
                    let rendered = serde_json::to_string(&state)
                        .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
                    Ok(boxed_tool_output(FunctionToolOutput::from_text(
                        rendered,
                        Some(true),
                    )))
                }
                TaskContextBoundaryArgs::Review { target_task_id } => {
                    if !invocation.turn.config.rencrow_autonomous_completion {
                        return Err(FunctionCallError::RespondToModel(
                            "autonomous completion is not enabled for this session".into(),
                        ));
                    }
                    let role_name = super::multi_agents::spawn::configured_completion_reviewer_role(
                        &invocation.turn.config,
                    )
                    .ok_or_else(|| {
                        FunctionCallError::RespondToModel(
                            "no configured read-only reviewer/analyst/verifier role is available; no reviewer was spawned".into(),
                        )
                    })?;
                    let prepared = prepare_agent_spawn_config(
                        &invocation.session,
                        &invocation.step_context,
                        SpawnConfigOptions {
                            version: SpawnConfigVersion::V1,
                            full_history_fork: false,
                            role_name: Some(&role_name),
                            model: None,
                            reasoning_effort: None,
                        },
                    )
                    .await
                    .map_err(FunctionCallError::RespondToModel)?;
                    if prepared.config.model.as_deref().is_none_or(str::is_empty) {
                        return Err(FunctionCallError::RespondToModel(
                            "configured reviewer role did not resolve an explicit model route; no reviewer was spawned".into(),
                        ));
                    }
                    let prepared_role = prepared.role_name.as_deref().ok_or_else(|| {
                        FunctionCallError::RespondToModel(
                            "configured reviewer role was not resolved; no reviewer was spawned"
                                .into(),
                        )
                    })?;
                    if !super::multi_agents::spawn::completion_reviewer_config_is_read_only(
                        &prepared.config,
                        prepared_role,
                    ) {
                        return Err(FunctionCallError::RespondToModel(
                            "configured reviewer role must declare read-only identity and guard; no reviewer was spawned".into(),
                        ));
                    }
                    super::multi_agents::spawn::validate_completion_reviewer_depth(
                        &invocation.turn.session_source,
                        invocation.turn.config.agent_max_depth,
                    )
                    .map_err(FunctionCallError::RespondToModel)?;
                    let launch = invocation
                        .session
                        .prepare_completion_review_launch(&target_task_id, &invocation.call_id)
                        .await
                        .map_err(FunctionCallError::RespondToModel)?;
                    if !launch.should_spawn
                        && launch
                            .reviewer_turn_id
                            .as_deref()
                            .is_some_and(|turn_id| turn_id.starts_with("pending-generation:"))
                        && let (Some(child_id), Some(role), Some(spawn_call_id)) = (
                            launch.reviewer_thread_id.as_deref(),
                            launch.reviewer_role.as_deref(),
                            launch.spawn_call_id.as_deref(),
                        )
                        && let Ok(child_id) = codex_protocol::ThreadId::try_from(child_id)
                    {
                        let child_generation = invocation
                            .session
                            .services
                            .agent_control
                            .get_agent_status_generation(child_id)
                            .await
                            .ok()
                            .and_then(|(generation, _)| generation);
                        let child_metadata = invocation
                            .session
                            .services
                            .agent_control
                            .get_agent_metadata(child_id);
                        let child_config = invocation
                            .session
                            .services
                            .agent_control
                            .get_agent_config_snapshot(child_id)
                            .await;
                        if let (Some(generation), Some(metadata), Some(child_config)) =
                            (child_generation, child_metadata, child_config)
                            && metadata.agent_role.as_deref() == Some(role)
                            && prepared.config.model.as_deref() == Some(child_config.model.as_str())
                        {
                            invocation
                                .session
                                .record_completion_review_spawn(
                                    &launch.unit_id,
                                    &launch.target_task_id,
                                    launch.target_task_revision,
                                    launch.target_artifact_ref.clone(),
                                    child_id,
                                    generation,
                                    role.to_string(),
                                    spawn_call_id.to_string(),
                                )
                                .await
                                .map_err(FunctionCallError::RespondToModel)?;
                        }
                    }
                    let (reviewer_thread_id, reviewer_role, model_route, launch_outcome) = if launch
                        .should_spawn
                    {
                        let spawned =
                            super::multi_agents::spawn::spawn_configured_completion_review(
                                &invocation,
                                prepared,
                                &launch.unit_id,
                                &launch.target_task_id,
                                launch.target_task_revision,
                                &launch.target_artifact_ref,
                                &launch.review_brief,
                            )
                            .await?;
                        (
                            Some(spawned.agent_id),
                            Some(spawned.reviewer_role),
                            Some(spawned.model_route),
                            "spawned".to_string(),
                        )
                    } else {
                        let model_route = if let Some(id) = launch
                            .reviewer_thread_id
                            .as_deref()
                            .and_then(|id| codex_protocol::ThreadId::try_from(id).ok())
                        {
                            invocation
                                .session
                                .services
                                .agent_control
                                .get_agent_config_snapshot(id)
                                .await
                                .map(|snapshot| snapshot.model)
                        } else {
                            None
                        };
                        let current = invocation
                            .session
                            .rencrow_completion_state(&launch.root_task_id)
                            .await;
                        let collected = current.as_ref().is_some_and(|state| {
                            state.units.iter().any(|unit| {
                                unit.id == launch.unit_id
                                    && unit
                                        .review_evidence
                                        .as_ref()
                                        .is_some_and(|evidence| evidence.wait_result_ref.is_some())
                            })
                        });
                        let outcome = if let Some(escalation) =
                            launch.deadline_escalation.as_deref()
                        {
                            format!(
                                "review deadline escalated: {escalation}; do not repeatedly wait for or resubmit this reviewer generation"
                            )
                        } else if launch.status == UnitStatus::Blocked
                            || launch.status == UnitStatus::Failed
                        {
                            "review is blocked or failed; inspect host completion state before authorizing another reviewer".to_string()
                        } else if launch.status == UnitStatus::Done {
                            "already_adopted".to_string()
                        } else if collected {
                            "collected; call adopt_review(unit_id, reason)".to_string()
                        } else if let Some(unresolved) = launch.unresolved_launch.as_ref() {
                            return Ok(boxed_tool_output(FunctionToolOutput::from_text(
                                serde_json::to_string(&serde_json::json!({
                                    "unit_id": launch.unit_id,
                                    "target_task_id": launch.target_task_id,
                                    "target_task_revision": launch.target_task_revision,
                                    "reviewer_thread_id": serde_json::Value::Null,
                                    "reviewer_role": serde_json::Value::Null,
                                    "model_route": serde_json::Value::Null,
                                    "status": "running",
                                    "outcome": "unknown review spawn; do not spawn again",
                                    "launch_call_id": unresolved.spawn_call_id,
                                    "launch_status": unresolved.status,
                                    "reason": unresolved.reason,
                                    "reconciliation": "Inspect the parent rollout's spawn_agent call with this launch_call_id and the host child list. If an exact reviewer child can be established, wait for that same child. If not, retain this unknown state; do not resubmit the spawn."
                                }))
                                .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?,
                                Some(true),
                            )));
                        } else {
                            "pending or unresolved; wait for the same reviewer child, then call adopt_review(unit_id, reason)".to_string()
                        };
                        (
                            launch.reviewer_thread_id.clone(),
                            launch.reviewer_role.clone(),
                            model_route,
                            outcome,
                        )
                    };
                    if launch.should_spawn
                        && let Some(state) = invocation
                            .session
                            .rencrow_completion_state(&launch.root_task_id)
                            .await
                    {
                        crate::autonomous_progress::report(
                            &invocation.session,
                            &invocation.turn,
                            &state,
                        )
                        .await;
                    }
                    let rendered = serde_json::to_string(&serde_json::json!({
                        "unit_id": launch.unit_id,
                        "target_task_id": launch.target_task_id,
                        "target_task_revision": launch.target_task_revision,
                        "reviewer_thread_id": reviewer_thread_id,
                        "reviewer_role": reviewer_role,
                        "model_route": model_route,
                        "status": format!("{:?}", launch.status).to_ascii_lowercase(),
                        "outcome": launch_outcome,
                        "deadline_escalation": launch.deadline_escalation,
                    }))
                    .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
                    Ok(boxed_tool_output(FunctionToolOutput::from_text(
                        rendered,
                        Some(true),
                    )))
                }
                TaskContextBoundaryArgs::AdoptReview { unit_id, reason } => {
                    if !invocation.turn.config.rencrow_autonomous_completion {
                        return Err(FunctionCallError::RespondToModel(
                            "autonomous completion is not enabled for this session".into(),
                        ));
                    }
                    let adopted = invocation
                        .session
                        .adopt_completion_review(&unit_id, reason)
                        .await
                        .map_err(FunctionCallError::RespondToModel)?;
                    let state = invocation
                        .session
                        .rencrow_completion_state(&adopted.root_task_id)
                        .await;
                    if let Some(state) = state.as_ref() {
                        crate::autonomous_progress::report(
                            &invocation.session,
                            &invocation.turn,
                            state,
                        )
                        .await;
                    }
                    let rendered = serde_json::to_string(&serde_json::json!({
                        "unit_id": adopted.unit_id,
                        "target_task_id": adopted.target_task_id,
                        "target_task_revision": adopted.target_task_revision,
                        "reviewer_thread_id": adopted.reviewer_thread_id,
                        "status": "done",
                        "next_action": "Continue with the next planned unit. No root accept is required.",
                    }))
                    .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
                    Ok(boxed_tool_output(FunctionToolOutput::from_text(
                        rendered,
                        Some(true),
                    )))
                }
                TaskContextBoundaryArgs::PlanReview {
                    root_task_id,
                    previous_plan,
                    revised_plan,
                    reason,
                } => {
                    if !invocation.turn.config.rencrow_autonomous_completion {
                        return Err(FunctionCallError::RespondToModel(
                            "autonomous completion is not enabled for this session".into(),
                        ));
                    }
                    invocation
                        .session
                        .record_autonomous_plan_review(
                            &root_task_id,
                            previous_plan,
                            revised_plan,
                            reason,
                        )
                        .await
                        .map_err(FunctionCallError::RespondToModel)?;
                    let state = invocation
                        .session
                        .rencrow_completion_state(&root_task_id)
                        .await
                        .ok_or_else(|| {
                            FunctionCallError::RespondToModel(
                                "host completion state disappeared after plan review".into(),
                            )
                        })?;
                    let rendered = serde_json::to_string(&state)
                        .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
                    Ok(boxed_tool_output(FunctionToolOutput::from_text(
                        rendered,
                        Some(true),
                    )))
                }
                TaskContextBoundaryArgs::Accept {
                    goal,
                    tasks,
                    current_task_id,
                    decisions,
                    constraints,
                    result,
                    satisfied_conditions,
                } => {
                    let maybe_latest = invocation
                        .session
                        .latest_rencrow_task_state()
                        .await
                        .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
                    let active = invocation.session.active_task_scope().await;
                    if active.as_ref().is_some_and(|active| {
                        maybe_latest
                            .as_ref()
                            .is_some_and(|state| state.boundary.root_task_id != active.root_task_id)
                    }) {
                        return Err(FunctionCallError::RespondToModel(
                            "active task root does not match the last accepted task state".into(),
                        ));
                    }
                    let root_task_id = active
                        .as_ref()
                        .map(|active| active.root_task_id.as_str())
                        .or_else(|| {
                            maybe_latest
                                .as_ref()
                                .map(|state| state.boundary.root_task_id.as_str())
                        });
                    let autonomous = invocation.turn.config.rencrow_autonomous_completion;
                    let (goal, tasks, current_task_id, decisions, constraints) = if autonomous {
                        if goal.is_some()
                            || tasks.is_some()
                            || current_task_id.is_some()
                            || decisions.is_some()
                            || constraints.is_some()
                        {
                            return Err(FunctionCallError::RespondToModel(
                                "autonomous accept derives the task board, goal, decisions, and constraints from host state; submit only result and optional satisfied_conditions".into(),
                            ));
                        }
                        let root_task_id = root_task_id.ok_or_else(|| {
                            FunctionCallError::RespondToModel(
                                "autonomous accept requires an active registered root task".into(),
                            )
                        })?;
                        let active_task_id = active
                            .as_ref()
                            .map(|scope| scope.task_id.as_str())
                            .ok_or_else(|| {
                                FunctionCallError::RespondToModel(
                                    "begin an active task scope before autonomous acceptance"
                                        .into(),
                                )
                            })?;
                        let contract = invocation.session
                            .rencrow_completion_state(root_task_id)
                            .await
                            .ok_or_else(|| FunctionCallError::RespondToModel(
                                "register a finite completion contract before accepting a boundary".into(),
                            ))?;
                        let goal = completion_goal(&contract);
                        let task_purpose = contract
                            .units
                            .iter()
                            .find(|unit| unit.id == active_task_id)
                            .map(|unit| unit.purpose.as_str())
                            .unwrap_or(active_task_id);
                        let (tasks, current_task_id) = host_task_board(
                            root_task_id,
                            active_task_id,
                            &goal,
                            task_purpose,
                            maybe_latest.as_ref(),
                        );
                        let decisions = maybe_latest
                            .as_ref()
                            .map(|state| state.boundary.decisions.clone())
                            .unwrap_or_default();
                        let constraints = maybe_latest
                            .as_ref()
                            .map(|state| state.boundary.constraints.clone())
                            .unwrap_or_default();
                        (goal, tasks, current_task_id, decisions, constraints)
                    } else {
                        let goal = goal.ok_or_else(|| {
                            FunctionCallError::RespondToModel(
                                "goal is required when autonomous completion is disabled".into(),
                            )
                        })?;
                        let tasks = tasks.ok_or_else(|| {
                            FunctionCallError::RespondToModel(
                                "tasks are required when autonomous completion is disabled".into(),
                            )
                        })?;
                        if satisfied_conditions.is_some() {
                            return Err(FunctionCallError::RespondToModel(
                                "satisfied_conditions requires autonomous completion".into(),
                            ));
                        }
                        let tasks = normalize_accepted_tasks(
                            tasks,
                            maybe_latest.as_ref(),
                            &invocation.session.thread_id().to_string(),
                            root_task_id,
                        )
                        .map_err(FunctionCallError::RespondToModel)?;
                        (
                            goal,
                            tasks,
                            current_task_id,
                            decisions.unwrap_or_default(),
                            constraints.unwrap_or_default(),
                        )
                    };
                    let Some(active) = active else {
                        let already_committed = maybe_latest.as_ref().is_some_and(|state| {
                            let boundary = &state.boundary;
                            boundary.thread_id == invocation.session.thread_id().to_string()
                                && boundary.goal == goal
                                && boundary.tasks == tasks
                                && boundary.current_task_id == current_task_id
                                && boundary.decisions == decisions
                                && boundary.constraints == constraints
                                && state
                                    .completed
                                    .get(&boundary.closed_task_id)
                                    .is_some_and(|receipt| receipt.result == result)
                        });
                        if already_committed {
                            return Ok(boxed_tool_output(FunctionToolOutput::from_text(
                                "Task boundary was already committed with this content.".into(),
                                Some(true),
                            )));
                        }
                        return Err(FunctionCallError::RespondToModel(
                            "begin a task scope before accepting a boundary; begin is required for changed content".into(),
                        ));
                    };
                    if autonomous && active.task_id == active.root_task_id {
                        if invocation
                            .session
                            .has_uncollected_completion_children()
                            .await
                        {
                            return Err(FunctionCallError::RespondToModel(
                                "collect terminal child results before accepting the root task; terminal status alone is not collection".into(),
                            ));
                        }
                        let contract = invocation.session
                            .rencrow_completion_state(&active.root_task_id)
                            .await
                            .ok_or_else(|| FunctionCallError::RespondToModel(
                                "register a finite completion contract before accepting the root task".into(),
                            ))?;
                        invocation
                            .session
                            .set_rencrow_completion_state(
                                active.root_task_id.clone(),
                                serde_json::to_value(&contract).map_err(|error| {
                                    FunctionCallError::RespondToModel(error.to_string())
                                })?,
                            )
                            .await
                            .map_err(FunctionCallError::RespondToModel)?;
                    }
                    let history = invocation.session.clone_history().await;
                    let originals = history.annotated_items();
                    let Some(begin_index) = originals.iter().position(|envelope| {
                        matches!(
                            &envelope.item,
                            ResponseItem::FunctionCall { name, call_id, .. }
                                if name == TASK_CONTEXT_BOUNDARY_TOOL_NAME
                                    && call_id == &active.begin_call_id
                        )
                    }) else {
                        return Err(FunctionCallError::RespondToModel(
                            "task begin call is not present in the current host history".into(),
                        ));
                    };
                    let begin_call_exists = originals.iter().any(|envelope| {
                        matches!(
                            &envelope.item,
                            ResponseItem::FunctionCall { call_id, .. }
                                | ResponseItem::CustomToolCall { call_id, .. }
                                if call_id == &active.begin_call_id
                        )
                    });
                    let begin_output_exists = originals.iter().any(|envelope| {
                        matches!(
                            &envelope.item,
                            ResponseItem::FunctionCallOutput {
                                call_id: Some(call_id),
                                ..
                            }
                                | ResponseItem::CustomToolCallOutput { call_id, .. }
                                if call_id == &active.begin_call_id
                        )
                    });
                    if !begin_call_exists || !begin_output_exists {
                        return Err(FunctionCallError::RespondToModel(
                            "task begin call/result pair is incomplete".into(),
                        ));
                    }
                    invocation
                        .session
                        .rebase_active_task_scope_cursor(&active.begin_call_id, begin_index)
                        .await;
                    let latest_state = maybe_latest;
                    let current_revision = latest_state
                        .as_ref()
                        .map(|state| state.boundary.revision.checked_add(1))
                        .unwrap_or(Some(1))
                        .ok_or_else(|| {
                            FunctionCallError::RespondToModel("task state revision overflow".into())
                        })?;
                    let attempt = invocation
                        .session
                        .latest_task_check_for_scope(&active.task_id, active.task_revision)
                        .await
                        .ok_or_else(|| {
                            FunctionCallError::RespondToModel(
                                "no completed verification receipt exists for this task revision"
                                    .into(),
                            )
                        })?;
                    let receipt = match attempt.status {
                        crate::task_check::TaskCheckAttemptStatus::Pending => {
                            return Err(FunctionCallError::RespondToModel(format!(
                                "latest verification attempt {} is still running",
                                attempt.call_id
                            )));
                        }
                        crate::task_check::TaskCheckAttemptStatus::ChangedArtifacts => {
                            return Err(FunctionCallError::RespondToModel(format!(
                                "latest verification attempt {} changed its artifacts",
                                attempt.call_id
                            )));
                        }
                        crate::task_check::TaskCheckAttemptStatus::Finished(receipt) => receipt,
                    };
                    if !receipt.succeeded || receipt.exit_code != 0 {
                        return Err(FunctionCallError::RespondToModel(
                            "the latest verification for this task did not succeed".into(),
                        ));
                    }
                    if invocation.turn.config.rencrow_autonomous_completion {
                        let unresolved = invocation
                            .session
                            .unresolved_task_check_obligations(&active)
                            .await;
                        if unresolved > 0 {
                            return Err(FunctionCallError::RespondToModel(format!(
                                "task has {unresolved} unresolved verification obligation(s); rerun each failed check with the same scope, working directory, command, and artifact set"
                            )));
                        }
                    }
                    if receipt.artifact_before != receipt.artifact_after {
                        return Err(FunctionCallError::RespondToModel(
                            "verification changed its declared artifacts".into(),
                        ));
                    }
                    let artifact_paths = receipt
                        .artifact_after
                        .iter()
                        .map(|stamp| stamp.path.clone())
                        .collect::<Vec<_>>();
                    let current_stamps = crate::task_check::hash_workspace_artifacts(
                        &receipt.workspace_root,
                        &artifact_paths,
                    )
                    .ok_or_else(|| {
                        FunctionCallError::RespondToModel(
                            "verification artifacts are no longer available as local regular files"
                                .into(),
                        )
                    })?;
                    if current_stamps != receipt.artifact_after {
                        return Err(FunctionCallError::RespondToModel(
                            "verification artifacts changed after the check".into(),
                        ));
                    }

                    let check_call_is_in_scope =
                        originals.iter().skip(begin_index).any(|envelope| {
                            matches!(
                                &envelope.item,
                                ResponseItem::FunctionCall { name, call_id, .. }
                                    if name == "exec_command" && call_id == &receipt.call_id
                            )
                        });
                    if !check_call_is_in_scope {
                        return Err(FunctionCallError::RespondToModel(
                            "verification receipt is not tied to an exec call after this task began".into(),
                        ));
                    }

                    let mut unresolved_effects = Vec::new();
                    if !invocation
                        .session
                        .services
                        .unified_exec_manager
                        .list_processes()
                        .await
                        .is_empty()
                    {
                        unresolved_effects.push("a unified exec process is still running".into());
                    }
                    if invocation
                        .session
                        .services
                        .agent_control
                        .has_registered_agents()
                    {
                        let agents = invocation
                            .session
                            .services
                            .agent_control
                            .list_agents(&invocation.turn.session_source, None)
                            .await
                            .map_err(|error| {
                                FunctionCallError::RespondToModel(error.to_string())
                            })?;
                        if agents.iter().any(|agent| {
                            matches!(
                                agent.status,
                                AgentStatus::PendingInit
                                    | AgentStatus::Running
                                    | AgentStatus::Interrupted
                            )
                        }) {
                            unresolved_effects.push("a child agent is unresolved".into());
                        }
                    }
                    unresolved_effects.extend(unresolved_calls_since(
                        originals,
                        begin_index,
                        &invocation.call_id,
                    ));
                    if !unresolved_effects.is_empty() {
                        return Err(FunctionCallError::RespondToModel(format!(
                            "task boundary is held because unresolved work remains: {}",
                            unresolved_effects.join("; ")
                        )));
                    }

                    if goal.trim().is_empty() || result.trim().is_empty() {
                        return Err(FunctionCallError::RespondToModel(
                            "goal and result must be non-empty".into(),
                        ));
                    }
                    let artifact_stamp_set = receipt
                        .artifact_after
                        .iter()
                        .map(|stamp| {
                            serde_json::json!({
                                "path": stamp.path.to_string_lossy(),
                                "sha256": stamp.sha256,
                            })
                        })
                        .collect::<Vec<_>>();
                    let artifact_fingerprint = digest(&serde_json::json!({
                        "task_id": receipt.task_id,
                        "task_revision": receipt.task_revision,
                        "workspace_root": receipt.workspace_root.to_string_lossy(),
                        "artifacts": artifact_stamp_set,
                    }))
                    .map_err(FunctionCallError::RespondToModel)?;
                    let evidence_ref =
                        format!("task-check:{}:{}", receipt.call_id, artifact_fingerprint);
                    let mut completion_state = invocation
                        .session
                        .rencrow_completion_state(&active.root_task_id)
                        .await
                        .map(|mut state| {
                            apply_host_acceptance(
                                &mut state,
                                &active.task_id,
                                active.task_revision,
                                &evidence_ref,
                                satisfied_conditions.as_deref().unwrap_or_default(),
                                active.task_id != active.root_task_id,
                            )
                            .map_err(FunctionCallError::RespondToModel)?;
                            Ok::<_, FunctionCallError>(state)
                        })
                        .transpose()?;
                    if invocation.turn.config.rencrow_autonomous_completion
                        && active.task_id != active.root_task_id
                        && let Some(state) = completion_state.as_mut()
                    {
                        let mut receipts = latest_state
                            .as_ref()
                            .map(|latest| latest.completed.clone())
                            .unwrap_or_default();
                        receipts.insert(
                            active.task_id.clone(),
                            AcceptedTaskReceipt {
                                revision: active.task_revision,
                                result: result.clone(),
                                artifact_ref: evidence_ref.clone(),
                                verification_ref: evidence_ref.clone(),
                            },
                        );
                        let mut state_value = serde_json::to_value(&*state).map_err(|error| {
                            FunctionCallError::RespondToModel(error.to_string())
                        })?;
                        crate::session::session::canonicalize_completion_state_evidence(
                            &mut state_value,
                            &receipts,
                        )
                        .map_err(FunctionCallError::RespondToModel)?;
                        *state = serde_json::from_value(state_value).map_err(|error| {
                            FunctionCallError::RespondToModel(error.to_string())
                        })?;
                    }
                    if autonomous && active.task_id == active.root_task_id {
                        let contract = completion_state.as_ref().ok_or_else(|| {
                            FunctionCallError::RespondToModel(
                                "root task cannot be accepted without a registered completion contract".into(),
                            )
                        })?;
                        if contract.conditions.iter().any(|condition| {
                            condition.status
                                != codex_history::task_context::AcceptanceStatus::Satisfied
                        }) {
                            return Err(FunctionCallError::RespondToModel(
                                "root task cannot be accepted while required completion conditions remain open".into(),
                            ));
                        }
                        if contract
                            .units
                            .iter()
                            .any(|unit| unit.status != UnitStatus::Done)
                        {
                            return Err(FunctionCallError::RespondToModel(
                                "root task cannot be accepted while planned, blocked, failed, or running units remain".into(),
                            ));
                        }
                    }
                    let candidate = BoundaryCandidate {
                        version: 1,
                        mode: BoundaryMode::Task,
                        thread_id: invocation.session.thread_id().to_string(),
                        root_task_id: active.root_task_id.clone(),
                        closed_task_id: active.task_id.clone(),
                        revision: current_revision,
                        closed_task_revision: active.task_revision,
                        goal,
                        tasks,
                        current_task_id,
                        artifact_ref: evidence_ref.clone(),
                        verification_ref: evidence_ref.clone(),
                        decisions,
                        constraints,
                        completion_state,
                    };
                    let previous_state_hash = latest_state
                        .as_ref()
                        .map(codex_history::task_context::StoredTaskState::state_hash)
                        .transpose()
                        .map_err(FunctionCallError::RespondToModel)?;
                    let parent_review_ref = format!(
                        "parent-review:{}:{}",
                        invocation.call_id,
                        digest(&candidate).map_err(FunctionCallError::RespondToModel)?
                    );
                    let facts = HostBoundaryFacts {
                        thread_id: invocation.session.thread_id().to_string(),
                        authorized_parent: true,
                        root_task_id: active.root_task_id.clone(),
                        current_revision,
                        closed_task_id: active.task_id.clone(),
                        closed_task_revision: active.task_revision,
                        artifact_ref: Some(evidence_ref.clone()),
                        artifact_revision: Some(active.task_revision),
                        artifact_result: Some(result),
                        verification_ref: Some(evidence_ref.clone()),
                        verification_artifact_ref: Some(evidence_ref),
                        verification_artifact_revision: Some(active.task_revision),
                        verification_succeeded: true,
                        parent_review_ref: Some(parent_review_ref),
                        parent_review_matches: true,
                        unresolved_effects: Vec::new(),
                        accepted_completed: latest_state
                            .as_ref()
                            .map(|state| state.completed.clone())
                            .unwrap_or_default(),
                    };
                    let validated = validate_boundary(candidate, &facts).map_err(|error| {
                        FunctionCallError::RespondToModel(format!(
                            "task boundary candidate was rejected: {error:?}"
                        ))
                    })?;
                    let stored = validated
                        .into_stored_state(
                            latest_state.as_ref().map(|state| state.boundary.revision),
                            previous_state_hash,
                            format!("boundary:{}", invocation.call_id),
                        )
                        .map_err(FunctionCallError::RespondToModel)?;
                    let summary_text = stored
                        .render_context()
                        .map_err(FunctionCallError::RespondToModel)?;
                    if approx_token_count(&summary_text) > 10_000 {
                        return Err(FunctionCallError::RespondToModel(
                            "rendered task boundary context exceeds the 10000-token limit".into(),
                        ));
                    }
                    let prepared_history_len = match originals.last().map(|row| &row.item) {
                        Some(ResponseItem::FunctionCall {
                            name,
                            call_id,
                            arguments: recorded_arguments,
                            ..
                        }) if name == TASK_CONTEXT_BOUNDARY_TOOL_NAME
                            && call_id == &invocation.call_id
                            && recorded_arguments == &raw_arguments =>
                        {
                            originals.len() - 1
                        }
                        _ => originals.len(),
                    };
                    let prepared_history_hash = history_digest(&originals[..prepared_history_len])
                        .map_err(FunctionCallError::RespondToModel)?;
                    invocation
                        .session
                        .set_pending_task_boundary(crate::state::PendingTaskBoundary {
                            state: stored,
                            accept_call_id: invocation.call_id,
                            accept_call_arguments: raw_arguments,
                            expected_turn_id: invocation.turn.sub_id.clone(),
                            summary_text,
                            prepared_history_hash,
                            prepared_history_len,
                            verification_workspace_root: receipt.workspace_root.clone(),
                            verified_artifacts: receipt.artifact_after.clone(),
                        })
                        .await
                        .map_err(FunctionCallError::RespondToModel)?;
                    invocation.session.request_new_context_window().await;
                    Ok(boxed_tool_output(FunctionToolOutput::from_text(
                        "Task boundary prepared. Host verification and review bindings are recorded; checkpoint commit is still pending."
                            .into(),
                        Some(true),
                    )))
                }
            }
        })
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        false
    }
}

fn unresolved_calls_since(
    items: &[ResponseItemEnvelope],
    start: usize,
    active_call_id: &str,
) -> Vec<String> {
    use std::collections::HashMap;
    let mut calls = HashMap::<String, String>::new();
    let mut outputs = std::collections::HashSet::<String>::new();
    for envelope in items.iter().skip(start) {
        match &envelope.item {
            ResponseItem::FunctionCall { name, call_id, .. }
                if name != TASK_CONTEXT_BOUNDARY_TOOL_NAME && call_id != active_call_id =>
            {
                calls.insert(call_id.clone(), name.clone());
            }
            ResponseItem::CustomToolCall { name, call_id, .. }
                if name != TASK_CONTEXT_BOUNDARY_TOOL_NAME && call_id != active_call_id =>
            {
                calls.insert(call_id.clone(), name.clone());
            }
            ResponseItem::FunctionCallOutput {
                call_id: Some(call_id),
                ..
            }
            | ResponseItem::CustomToolCallOutput { call_id, .. } => {
                outputs.insert(call_id.clone());
            }
            _ => {}
        }
    }
    calls
        .into_iter()
        .filter(|(call_id, _)| !outputs.contains(call_id))
        .map(|(call_id, name)| format!("tool call `{name}` ({call_id}) has no result"))
        .collect()
}

fn normalize_accepted_tasks(
    mut tasks: Vec<TaskState>,
    latest: Option<&StoredTaskState>,
    thread_id: &str,
    root_task_id: Option<&str>,
) -> Result<Vec<TaskState>, String> {
    let Some(latest) = latest else {
        return Ok(tasks);
    };
    if latest.boundary.thread_id != thread_id
        || root_task_id.is_none_or(|root| root != latest.boundary.root_task_id)
    {
        return Err("last accepted task state belongs to another thread or root".into());
    }

    let mut seen = HashSet::new();
    if tasks.iter().any(|task| !seen.insert(task.id.clone())) {
        return Err("task board contains duplicate task IDs".into());
    }

    for (previous_index, previous) in latest.boundary.tasks.iter().enumerate() {
        if previous.status != TaskStatus::Completed || !latest.completed.contains_key(&previous.id)
        {
            continue;
        }
        if let Some(candidate) = tasks.iter().find(|task| task.id == previous.id) {
            if candidate.status != TaskStatus::Completed {
                return Err(format!(
                    "accepted completed task `{}` cannot return to {:?}",
                    previous.id, candidate.status
                ));
            }
            continue;
        }

        let insertion_index = latest.boundary.tasks[previous_index + 1..]
            .iter()
            .find_map(|later| tasks.iter().position(|task| task.id == later.id))
            .unwrap_or(tasks.len());
        tasks.insert(insertion_index, previous.clone());
    }
    Ok(tasks)
}

fn completion_goal(state: &codex_history::task_context::CompletionState) -> String {
    state
        .conditions
        .iter()
        .map(|condition| format!("{}: {}", condition.id, condition.result.trim()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn host_task_board(
    root_task_id: &str,
    closed_task_id: &str,
    goal: &str,
    closed_task_purpose: &str,
    latest: Option<&StoredTaskState>,
) -> (Vec<TaskState>, Option<String>) {
    let closing_root = root_task_id == closed_task_id;
    let mut tasks = latest
        .map(|state| {
            state
                .boundary
                .tasks
                .iter()
                .filter(|task| {
                    task.id != root_task_id
                        && task.status == TaskStatus::Completed
                        && state.completed.contains_key(&task.id)
                })
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if let Some(root) = tasks.iter_mut().find(|task| task.id == root_task_id) {
        root.purpose = goal.to_string();
        root.status = if closing_root {
            TaskStatus::Completed
        } else {
            TaskStatus::InProgress
        };
    } else {
        tasks.insert(
            0,
            TaskState {
                id: root_task_id.to_string(),
                purpose: goal.to_string(),
                status: if closing_root {
                    TaskStatus::Completed
                } else {
                    TaskStatus::InProgress
                },
                depends_on: Vec::new(),
            },
        );
    }
    if !closing_root {
        if let Some(task) = tasks.iter_mut().find(|task| task.id == closed_task_id) {
            task.status = TaskStatus::Completed;
        } else {
            tasks.push(TaskState {
                id: closed_task_id.to_string(),
                purpose: closed_task_purpose.to_string(),
                status: TaskStatus::Completed,
                depends_on: Vec::new(),
            });
        }
    }
    (tasks, (!closing_root).then(|| root_task_id.to_string()))
}

fn apply_host_acceptance(
    state: &mut codex_history::task_context::CompletionState,
    task_id: &str,
    task_revision: u64,
    receipt_ref: &str,
    selected_conditions: &[String],
    require_execution_unit: bool,
) -> Result<(), String> {
    let mut unit_index = state
        .units
        .iter()
        .position(|unit| unit.kind == CompletionUnitKind::Execution && unit.id == task_id)
        .or_else(|| {
            state.current_unit.as_ref().and_then(|current| {
                state.units.iter().position(|unit| {
                    unit.kind == CompletionUnitKind::Execution && &unit.id == current
                })
            })
        })
        .or_else(|| {
            state.units.iter().position(|unit| {
                unit.kind == CompletionUnitKind::Execution
                    && unit.task_refs.iter().any(|reference| {
                        reference.task_id == task_id && reference.task_revision == task_revision
                    })
            })
        });
    let mut review_plan_mapping = false;
    if unit_index.is_none() && require_execution_unit {
        let review_index = if let Some(current) = state.current_unit.as_deref() {
            let Some(index) = state.units.iter().position(|unit| unit.id == current) else {
                return Err(format!(
                    "current completion unit {current} does not identify the planned review for {task_id}@{task_revision}"
                ));
            };
            let review = &state.units[index];
            if review.kind != CompletionUnitKind::Review
                || review.status != UnitStatus::Planned
                || review.review_target.as_ref().is_none_or(|target| {
                    target.task_id != task_id || target.task_revision != task_revision
                })
            {
                return Err(format!(
                    "current completion unit {current} is not a planned review for {task_id}@{task_revision}"
                ));
            }
            Some(index)
        } else {
            let matches = state
                .units
                .iter()
                .enumerate()
                .filter(|(_, unit)| {
                    unit.kind == CompletionUnitKind::Review
                        && unit.status == UnitStatus::Planned
                        && unit.review_target.as_ref().is_some_and(|target| {
                            target.task_id == task_id && target.task_revision == task_revision
                        })
                })
                .map(|(index, _)| index)
                .collect::<Vec<_>>();
            match matches.as_slice() {
                [] => None,
                [index] => Some(*index),
                _ => {
                    return Err(format!(
                        "task {task_id}@{task_revision} has multiple matching planned review units"
                    ));
                }
            }
        };
        if let Some(review_index) = review_index {
            let review = &state.units[review_index];
            if review.depends_on.len() != 1 {
                return Err(format!(
                    "planned review unit {} must depend on exactly one execution unit",
                    review.id
                ));
            }
            let dependency_id = &review.depends_on[0];
            let dependencies = state
                .units
                .iter()
                .enumerate()
                .filter(|(_, unit)| unit.id == *dependency_id)
                .map(|(index, _)| index)
                .collect::<Vec<_>>();
            let [execution_index] = dependencies.as_slice() else {
                return Err(format!(
                    "planned review unit {} must identify one unique execution dependency",
                    review.id
                ));
            };
            let execution = &state.units[*execution_index];
            if execution.kind != CompletionUnitKind::Execution
                || !matches!(execution.status, UnitStatus::Planned | UnitStatus::Running)
                || execution.task_refs.iter().any(|reference| {
                    reference.task_id != task_id || reference.task_revision != task_revision
                })
            {
                return Err(format!(
                    "planned review unit {} does not depend on an available execution unit for {task_id}@{task_revision}",
                    review.id
                ));
            }
            if selected_conditions
                .iter()
                .any(|condition| !execution.condition_refs.contains(condition))
            {
                return Err(format!(
                    "selected conditions are not declared by execution unit {}",
                    execution.id
                ));
            }
            unit_index = Some(*execution_index);
            review_plan_mapping = true;
        }
    }
    if require_execution_unit && !state.units.is_empty() && unit_index.is_none() {
        return Err(format!(
            "task {task_id}@{task_revision} has no matching execution unit"
        ));
    }
    if !selected_conditions.is_empty() && unit_index.is_none() {
        return Err(format!(
            "task {task_id}@{task_revision} has no matching execution unit for condition acceptance"
        ));
    }
    if let Some(index) = unit_index {
        let unit = &mut state.units[index];
        if unit.status != UnitStatus::Done {
            unit.status = UnitStatus::Done;
        }
        let receipt = CompletionEvidenceRef {
            task_id: task_id.to_string(),
            task_revision,
            artifact_ref: receipt_ref.to_string(),
            verification_ref: receipt_ref.to_string(),
        };
        if !unit.evidence.contains(&receipt) {
            unit.evidence.push(receipt.clone());
        }
        for condition_id in selected_conditions {
            let condition = state
                .conditions
                .iter_mut()
                .find(|condition| condition.id == *condition_id)
                .ok_or_else(|| format!("unknown acceptance condition {condition_id}"))?;
            if !review_plan_mapping && !unit.condition_refs.iter().any(|id| id == condition_id) {
                unit.condition_refs.push(condition_id.clone());
            }
            condition.status = codex_history::task_context::AcceptanceStatus::Satisfied;
            condition.unmet_reason = None;
            // Condition evidence is the current proof. Older accepted unit receipts
            // remain in unit history, while a stale condition proof is replaced.
            condition.evidence.clear();
            condition.evidence.push(receipt.clone());
        }
    } else if !selected_conditions.is_empty() {
        return Err("selected conditions require a matching execution unit".into());
    }
    Ok(())
}

impl CoreToolRuntime for TaskContextBoundaryHandler {
    fn is_builtin_control_tool(&self) -> bool {
        true
    }
}

#[cfg(test)]
#[path = "task_context_boundary_tests.rs"]
mod tests;
