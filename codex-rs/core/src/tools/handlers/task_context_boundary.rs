// Modified by RenCrow Switch Core, 2026-11-21: preserve exact accepted revision evidence during acceptance.
// Modified by RenCrow Switch Core, 2026-11-21: expose bounded host verification diagnostics in status.
// Modified by RenCrow Switch Core, 2026-11-20: separate caller and host checkpoint budgets.
// Modified by RenCrow Switch Core, 2026-11-16.
use crate::agent::child_config::SpawnConfigOptions;
use crate::agent::child_config::SpawnConfigVersion;
use crate::agent::child_config::prepare_agent_spawn_config;
use crate::function_tool::FunctionCallError;
use crate::session::CompletionReviewChildGeneration;
use crate::session::CompletionReviewChildIdentity;
use crate::session::CompletionReviewRetryRequest;
use crate::session::CompletionReviewSelectionOutcome;
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
use codex_history::task_context::CompletionReviewTarget;
use codex_history::task_context::CompletionUnitKind;
use codex_history::task_context::ConditionSpecV1;
use codex_history::task_context::ExecutionOwnerV1;
use codex_history::task_context::HostBoundaryFacts;
use codex_history::task_context::SkillRefV1;
use codex_history::task_context::StoredTaskState;
use codex_history::task_context::TaskRequestV1;
use codex_history::task_context::TaskState;
use codex_history::task_context::TaskStatus;
use codex_history::task_context::UnitStatus;
use codex_history::task_context::validate_host_boundary_checkpoint;
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
        #[serde(default)]
        common_harness_request: Option<Box<CommonHarnessRequestArgs>>,
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
        #[serde(default)]
        review_retry: Option<ReviewRetryArgs>,
    },
    Review {
        target_task_id: String,
        #[serde(default)]
        unit_id: Option<String>,
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommonHarnessRequestArgs {
    contract_version: String,
    task_id: String,
    request_revision: u64,
    #[serde(default)]
    authorized_request_ref: String,
    goal: String,
    #[serde(default)]
    workspace_ref: String,
    #[serde(default)]
    start_target_version: String,
    #[serde(default)]
    permission_snapshot_ref: String,
    constraints: Vec<String>,
    conditions: Vec<ConditionSpecV1>,
    condition_set_revision: u64,
    max_attempts: u32,
    #[serde(default)]
    operation_limits_ref: String,
    skill_refs: Vec<RequestedSkillRefArgs>,
    execution_owner: ExecutionOwnerV1,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestedSkillRefArgs {
    id: String,
    // Older clients may send model-authored metadata. It is parsed only for
    // compatibility and discarded; Session binds the persistent SkillRefV1.
    #[serde(default)]
    version_ref: Option<String>,
    #[serde(default)]
    content_digest: Option<String>,
    #[serde(default)]
    source_ref: Option<String>,
}

impl CommonHarnessRequestArgs {
    fn into_task_request(self) -> Result<TaskRequestV1, String> {
        if self.contract_version != codex_history::task_context::COMMON_HARNESS_CONTRACT {
            return Err(format!(
                "unsupported common harness contract `{}`",
                self.contract_version
            ));
        }
        Ok(TaskRequestV1 {
            task_id: self.task_id,
            request_revision: self.request_revision,
            authorized_request_ref: self.authorized_request_ref,
            goal: self.goal,
            workspace_ref: self.workspace_ref,
            start_target_version: self.start_target_version,
            permission_snapshot_ref: self.permission_snapshot_ref,
            constraints: self.constraints,
            conditions: self.conditions,
            condition_set_revision: self.condition_set_revision,
            max_attempts: self.max_attempts,
            operation_limits_ref: self.operation_limits_ref,
            skill_refs: self
                .skill_refs
                .into_iter()
                .map(|skill| {
                    let RequestedSkillRefArgs {
                        id,
                        version_ref,
                        content_digest,
                        source_ref,
                    } = skill;
                    let _ignored_legacy_metadata = (version_ref, content_digest, source_ref);
                    SkillRefV1 {
                        id,
                        version_ref: String::new(),
                        content_digest: String::new(),
                        source_ref: String::new(),
                    }
                })
                .collect(),
            execution_owner: self.execution_owner,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewRetryArgs {
    unit_id: String,
    expected_attempt_id: String,
}

fn render_completion_status(
    state: &codex_history::task_context::CompletionState,
    current_review_reason: Option<&str>,
    verification_obligations: Option<&serde_json::Value>,
) -> Result<String, String> {
    if let Some(common) = state.common_harness.as_ref() {
        let condition_summaries = common
            .request
            .conditions
            .iter()
            .take(16)
            .map(|condition| {
                let current_evidence = common.evidence_refs.iter().rev().find(|evidence| {
                    evidence.condition_id == condition.condition_id
                        && evidence.condition_revision == condition.condition_revision
                        && evidence.condition_set_revision == common.request.condition_set_revision
                        && evidence.request_revision == common.request.request_revision
                        && evidence.target_version == common.run_state.target_version
                        && evidence.lifecycle
                            == codex_history::task_context::EvidenceLifecycleV1::Current
                });
                serde_json::json!({
                    "condition_id": condition.condition_id,
                    "revision": condition.condition_revision,
                    "kind": condition.kind,
                    "expected": condition.expected.chars().take(512).collect::<String>(),
                    "required": condition.required,
                    "unmet": common.run_state.unmet_conditions.contains(&condition.condition_id),
                    "latest_verdict": current_evidence.map(|evidence| evidence.verdict),
                    "evidence_id": current_evidence.map(|evidence| evidence.evidence_id.clone()),
                    "observed": current_evidence.map(|evidence| {
                        evidence.observed.chars().take(2048).collect::<String>()
                    }),
                    "log_refs": current_evidence.map(|evidence| evidence.log_refs.iter()
                        .take(8).map(|value| value.chars().take(512).collect::<String>())
                        .collect::<Vec<_>>()),
                    "artifact_refs": current_evidence.map(|evidence| evidence.artifact_refs.iter()
                        .take(16).map(|value| value.chars().take(512).collect::<String>())
                        .collect::<Vec<_>>()),
                })
            })
            .collect::<Vec<_>>();
        let current_operation = common
            .run_state
            .unsettled_operation_refs
            .first()
            .and_then(|operation_id| {
                common
                    .operation_refs
                    .iter()
                    .find(|record| &record.intent.operation_id == operation_id)
            })
            .map(|record| {
                serde_json::json!({
                    "operation_id": record.intent.operation_id,
                    "kind": record.intent.operation_kind,
                    "status": record.intent.status,
                    "claim_count": record.claims.len(),
                    "has_raw_receipt": record.receipt.is_some(),
                    "finalization_count": record.finalizations.len(),
                })
            });
        let recent_evidence = common
            .evidence_refs
            .iter()
            .rev()
            .take(8)
            .map(|evidence| {
                serde_json::json!({
                    "evidence_id": evidence.evidence_id,
                    "condition_id": evidence.condition_id,
                    "verdict": evidence.verdict,
                    "lifecycle": evidence.lifecycle,
                    "target_version": evidence.target_version,
                })
            })
            .collect::<Vec<_>>();
        let verification_obligations = verification_obligations.filter(|diagnostics| {
            diagnostics
                .get("count")
                .and_then(serde_json::Value::as_u64)
                .is_some_and(|count| count > 0)
        });
        let mut rendered = serde_json::json!({
            "contract_version": common.contract_version,
            "request": {
                "task_id": common.request.task_id,
                "request_revision": common.request.request_revision,
                "goal": common.request.goal.chars().take(2048).collect::<String>(),
                "constraints": common.request.constraints.iter().take(16)
                    .map(|value| value.chars().take(512).collect::<String>())
                    .collect::<Vec<_>>(),
                "conditions": condition_summaries,
                "max_attempts": common.request.max_attempts,
            },
            "run_state": {
                "state_version": common.run_state.state_version,
                "owner_id": common.run_state.owner_id,
                "owner_epoch": common.run_state.owner_epoch,
                "status": common.run_state.status,
                "phase": common.run_state.phase,
                "attempt_no": common.run_state.attempt_no,
                "target_version": common.run_state.target_version,
                "unmet_conditions": common.run_state.unmet_conditions.iter().take(32).collect::<Vec<_>>(),
                "next_operation": common.run_state.next_operation,
                "reason_code": common.run_state.reason_code,
                "waiting": common.run_state.waiting,
                "current_operation": current_operation,
            },
            "record_counts": {
                "operations": common.operation_refs.len(),
                "evidence": common.evidence_refs.len(),
            },
            "recent_evidence": recent_evidence,
        });
        if let Some(verification_obligations) = verification_obligations {
            rendered
                .as_object_mut()
                .ok_or_else(|| "common-harness status did not serialize as an object".to_string())?
                .insert(
                    "verification_obligations".into(),
                    verification_obligations.clone(),
                );
        }
        return serde_json::to_string(&rendered).map_err(|error| error.to_string());
    }
    let reason = current_review_reason.filter(|reason| !reason.trim().is_empty());
    let verification_obligations = verification_obligations.filter(|diagnostics| {
        diagnostics
            .get("count")
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|count| count > 0)
    });
    if reason.is_none() && verification_obligations.is_none() {
        return serde_json::to_string(state).map_err(|error| error.to_string());
    }
    let mut rendered = serde_json::to_value(state).map_err(|error| error.to_string())?;
    let object = rendered
        .as_object_mut()
        .ok_or_else(|| "completion state did not serialize as a JSON object".to_string())?;
    if let Some(reason) = reason {
        object.insert(
            "current_review_obligation".into(),
            serde_json::Value::String(reason.to_string()),
        );
    }
    if let Some(verification_obligations) = verification_obligations {
        object.insert(
            "verification_obligations".into(),
            verification_obligations.clone(),
        );
    }
    serde_json::to_string(&rendered).map_err(|error| error.to_string())
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
            let active_common_harness = invocation
                .session
                .common_harness_tool_state_for_turn(&invocation.turn)
                .await;
            if let Some((common_root_task_id, common)) = active_common_harness {
                let scope_matches = invocation
                    .session
                    .active_task_scope()
                    .await
                    .is_some_and(|scope| scope.root_task_id == common_root_task_id);
                let task_is_nonterminal = !matches!(
                    common.run_state.status,
                    codex_history::task_context::TaskRunStatusV1::Completed
                        | codex_history::task_context::TaskRunStatusV1::Cancelled
                        | codex_history::task_context::TaskRunStatusV1::Exhausted
                );
                let review_read_only = invocation
                    .session
                    .is_read_only_review_actor_for_turn(&invocation.turn)
                    .await;
                let reserved_begin = match &args {
                    TaskContextBoundaryArgs::Begin {
                        root_task_id,
                        task_id,
                        task_revision,
                        completion_state: None,
                        common_harness_request: None,
                    } => {
                        invocation
                            .session
                            .common_harness_reserved_begin_matches(
                                &invocation.turn,
                                root_task_id,
                                task_id,
                                *task_revision,
                            )
                            .await
                    }
                    _ => false,
                };
                let supported = matches!(&args, TaskContextBoundaryArgs::Status { .. })
                    || (!review_read_only && reserved_begin)
                    || (scope_matches
                        && task_is_nonterminal
                        && !review_read_only
                        && matches!(&args, TaskContextBoundaryArgs::Accept { .. }));
                if !supported {
                    return Err(FunctionCallError::RespondToModel(if review_read_only {
                        "common-harness reviewer route permits status only".into()
                    } else {
                        "common-harness v1 task actions are host-routed; only status and native-gated accept are supported".into()
                    }));
                }
            }
            match args {
                TaskContextBoundaryArgs::Begin {
                    root_task_id,
                    task_id,
                    task_revision,
                    completion_state,
                    common_harness_request,
                } => {
                    if completion_state.as_ref().is_some_and(|state| {
                        state
                            .get("common_harness")
                            .is_some_and(|extension| !extension.is_null())
                    }) {
                        return Err(FunctionCallError::RespondToModel(
                            "Begin cannot accept host-owned common-harness state; submit common_harness_request instead".into(),
                        ));
                    }
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
                        && active.root_task_id == root_task_id
                        && let Some(state) = invocation
                            .session
                            .rencrow_completion_state(&active.root_task_id)
                            .await
                        && state.common_harness.is_some()
                    {
                        return Err(FunctionCallError::RespondToModel(
                            "an active common-harness v1 task cannot be restarted or downgraded through Begin".into(),
                        ));
                    }
                    if completion_state.is_none()
                        && common_harness_request.is_none()
                        && self.autonomous_completion
                        && invocation.turn.config.rencrow_autonomous_completion
                        && invocation
                            .session
                            .common_harness_reserved_begin_matches(
                                &invocation.turn,
                                &root_task_id,
                                &task_id,
                                task_revision,
                            )
                            .await
                    {
                        invocation
                            .session
                            .begin_reserved_common_harness_execution(
                                &root_task_id,
                                &task_id,
                                task_revision,
                                &invocation.call_id,
                            )
                            .await
                            .map_err(FunctionCallError::RespondToModel)?;
                        return Ok(boxed_tool_output(FunctionToolOutput::from_text(
                            "Host-reserved common-harness execution unit begun. Continue with the current task and host-bound verification obligations.".into(),
                            Some(true),
                        )));
                    }
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
                    if let Some(common_harness_request) = common_harness_request {
                        if completion_state.is_some() {
                            return Err(FunctionCallError::RespondToModel(
                                "Begin cannot combine common_harness_request with completion_state"
                                    .into(),
                            ));
                        }
                        if !self.autonomous_completion
                            || !invocation.turn.config.rencrow_autonomous_completion
                        {
                            return Err(FunctionCallError::RespondToModel(
                                "common_harness/1.0 requires autonomous completion to be enabled"
                                    .into(),
                            ));
                        }
                        let request = (*common_harness_request)
                            .into_task_request()
                            .map_err(FunctionCallError::RespondToModel)?;
                        if request.task_id != task_id || request.request_revision != task_revision {
                            return Err(FunctionCallError::RespondToModel(
                                "common_harness_request task_id and request_revision must match Begin scope"
                                    .into(),
                            ));
                        }
                        invocation
                            .session
                            .begin_common_harness_task(
                                root_task_id,
                                task_id,
                                task_revision,
                                invocation.call_id,
                                request,
                                &invocation.turn,
                            )
                            .await
                            .map_err(FunctionCallError::RespondToModel)?;
                    } else {
                        if invocation.turn.config.rencrow_autonomous_completion
                            && let Some(completion_state) = completion_state
                        {
                            invocation
                                .session
                                .set_rencrow_completion_state(
                                    root_task_id.clone(),
                                    completion_state,
                                )
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
                    if completion_state
                        .get("common_harness")
                        .is_some_and(|extension| !extension.is_null())
                    {
                        return Err(FunctionCallError::RespondToModel(
                            "common-harness v1 state is host-owned and cannot be supplied through Update"
                                .into(),
                        ));
                    }
                    if invocation
                        .session
                        .rencrow_completion_state(&root_task_id)
                        .await
                        .is_some_and(|state| state.common_harness.is_some())
                    {
                        return Err(FunctionCallError::RespondToModel(
                            "common-harness v1 state is host-owned and cannot be changed through Update"
                                .into(),
                        ));
                    }
                    invocation
                        .session
                        .set_rencrow_completion_state_with_call_id(
                            root_task_id.clone(),
                            completion_state,
                            Some(invocation.call_id.clone()),
                        )
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
                        None => match invocation
                            .session
                            .common_harness_root_for_turn(&invocation.turn)
                            .await
                        {
                            Some(root) => root,
                            None => invocation
                                .session
                                .latest_rencrow_task_state()
                                .await
                                .map_err(|error| {
                                    FunctionCallError::RespondToModel(error.to_string())
                                })?
                                .map(|state| state.boundary.root_task_id)
                                .ok_or_else(|| {
                                    FunctionCallError::RespondToModel(
                                        "no host-registered completion contract is available"
                                            .into(),
                                    )
                                })?,
                        },
                    };
                    let state = invocation
                        .session
                        .refresh_common_harness_status(&invocation.turn, &root_task_id)
                        .await
                        .map_err(FunctionCallError::RespondToModel)?;
                    let current_review_reason = invocation
                        .session
                        .autonomous_completion_current_review_scope_reason()
                        .await;
                    let verification_obligations = invocation
                        .session
                        .task_verification_diagnostics(&root_task_id)
                        .await;
                    let rendered = render_completion_status(
                        &state,
                        current_review_reason.as_deref(),
                        Some(&verification_obligations),
                    )
                    .map_err(FunctionCallError::RespondToModel)?;
                    Ok(boxed_tool_output(FunctionToolOutput::from_text(
                        rendered,
                        Some(true),
                    )))
                }
                TaskContextBoundaryArgs::Review {
                    target_task_id,
                    unit_id,
                } => {
                    if !invocation.turn.config.rencrow_autonomous_completion {
                        return Err(FunctionCallError::RespondToModel(
                            "autonomous completion is not enabled for this session".into(),
                        ));
                    }
                    let selection = invocation
                        .session
                        .select_completion_review(&target_task_id, unit_id.as_deref())
                        .await
                        .map_err(FunctionCallError::RespondToModel)?;
                    if matches!(
                        &selection.outcome,
                        CompletionReviewSelectionOutcome::AlreadyAdopted
                    ) {
                        let (_, state) = invocation
                            .session
                            .autonomous_completion_current_state()
                            .await
                            .ok_or_else(|| {
                                FunctionCallError::RespondToModel(
                                    "completion review state is unavailable".into(),
                                )
                            })?;
                        let adopted = state
                            .units
                            .iter()
                            .find(|unit| unit.id == selection.obligation_unit_id);
                        let evidence = adopted.and_then(|unit| unit.review_evidence.as_ref());
                        let target_task_revision = adopted
                            .and_then(|unit| unit.review_target.as_ref())
                            .map(|target| target.task_revision);
                        let model_route = adopted
                            .and_then(|unit| unit.review_state.as_ref())
                            .and_then(|review| review.active_attempt.as_ref())
                            .and_then(|attempt| attempt.reviewer_model.as_deref());
                        let rendered = serde_json::to_string(&serde_json::json!({
                            "unit_id": selection.obligation_unit_id,
                            "target_task_id": target_task_id,
                            "target_task_revision": target_task_revision,
                            "reviewer_thread_id": evidence.map(|item| item.reviewer_thread_id.as_str()),
                            "reviewer_role": evidence.map(|item| item.reviewer_role.as_str()),
                            "model_route": model_route,
                            "status": "done",
                            "outcome": "already_adopted",
                        }))
                        .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
                        return Ok(boxed_tool_output(FunctionToolOutput::from_text(
                            rendered,
                            Some(true),
                        )));
                    }
                    let mut prepared = None;
                    let mut reviewer_role = None;
                    let mut reviewer_model = None;
                    if matches!(
                        &selection.outcome,
                        CompletionReviewSelectionOutcome::NeedsReservation { .. }
                    ) {
                        let role_name = super::multi_agents::spawn::configured_completion_reviewer_role(
                            &invocation.turn.config,
                        )
                        .ok_or_else(|| {
                            FunctionCallError::RespondToModel(
                                "no configured read-only reviewer/analyst/verifier role is available; no reviewer was spawned".into(),
                            )
                        })?;
                        let prepared_config = prepare_agent_spawn_config(
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
                        if prepared_config
                            .config
                            .model
                            .as_deref()
                            .is_none_or(str::is_empty)
                        {
                            return Err(FunctionCallError::RespondToModel(
                                "configured reviewer role did not resolve an explicit model route; no reviewer was spawned".into(),
                            ));
                        }
                        let prepared_role =
                            prepared_config.role_name.as_deref().ok_or_else(|| {
                                FunctionCallError::RespondToModel(
                                "configured reviewer role was not resolved; no reviewer was spawned"
                                    .into(),
                            )
                            })?;
                        if !super::multi_agents::spawn::completion_reviewer_config_is_read_only(
                            &prepared_config.config,
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
                        reviewer_role = Some(prepared_role.to_string());
                        reviewer_model = Some(
                            prepared_config.config.model.as_deref().ok_or_else(|| {
                                FunctionCallError::RespondToModel(
                                    "configured reviewer role did not resolve an explicit model route; no reviewer was spawned".into(),
                                )
                            })?.to_string(),
                        );
                        prepared = Some(prepared_config);
                    }
                    let launch = invocation
                        .session
                        .prepare_completion_review_launch(
                            &target_task_id,
                            unit_id.as_deref(),
                            &invocation.call_id,
                            reviewer_role.as_deref(),
                            reviewer_model.as_deref(),
                        )
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
                            && launch.reviewer_model.as_deref() == Some(child_config.model.as_str())
                        {
                            invocation
                                .session
                                .record_completion_review_spawn(
                                    &launch.unit_id,
                                    CompletionReviewChildGeneration {
                                        identity: CompletionReviewChildIdentity {
                                            target: CompletionReviewTarget {
                                                task_id: launch.target_task_id.clone(),
                                                task_revision: launch.target_task_revision,
                                            },
                                            target_artifact_ref: launch.target_artifact_ref.clone(),
                                            reviewer_thread_id: child_id,
                                            reviewer_role: role.to_string(),
                                            spawn_call_id: spawn_call_id.to_string(),
                                        },
                                        reviewer_turn_id: generation,
                                    },
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
                                prepared.ok_or_else(|| {
                                    FunctionCallError::RespondToModel(
                                        "completion review reservation requires a prepared reviewer route".into(),
                                    )
                                })?,
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
                        let model_route = if let Some(model) = launch.reviewer_model.clone() {
                            Some(model)
                        } else if let Some(id) = launch
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
                    review_retry,
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
                            &invocation.call_id,
                            previous_plan.clone(),
                            revised_plan.clone(),
                            reason.clone(),
                            review_retry.map(|retry| CompletionReviewRetryRequest {
                                unit_id: retry.unit_id,
                                expected_attempt_id: retry.expected_attempt_id,
                                invocation_call_id: invocation.call_id.clone(),
                                previous_plan,
                                revised_plan,
                                reason,
                            }),
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
                    let active = invocation.session.active_task_scope().await;
                    let mut common_harness_satisfied_conditions = None;
                    if let Some(active) = active.as_ref()
                        && let Some(state) = invocation
                            .session
                            .rencrow_completion_state(&active.root_task_id)
                            .await
                        && state.common_harness.is_some()
                    {
                        if satisfied_conditions.is_some() {
                            return Err(FunctionCallError::RespondToModel(
                                "satisfied_conditions is host-owned for common-harness v1; omit the model-supplied field".into(),
                            ));
                        }
                        common_harness_satisfied_conditions = Some(
                            invocation
                                .session
                                .common_harness_acceptance_conditions(&invocation.turn)
                                .await
                                .map_err(|error| {
                                    FunctionCallError::RespondToModel(format!(
                                        "common-harness v1 native acceptance gate rejected Accept: {error}"
                                    ))
                                })?
                                .ok_or_else(|| FunctionCallError::RespondToModel(
                                    "common-harness v1 native acceptance evidence is unavailable".into(),
                                ))?,
                        );
                    }
                    let maybe_latest = invocation
                        .session
                        .latest_rencrow_task_state()
                        .await
                        .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
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
                            .set_accepted_rencrow_completion_state(
                                active.root_task_id.clone(),
                                contract,
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
                        let message = if invocation.turn.config.rencrow_autonomous_completion {
                            format!(
                                "latest verification attempt {} for root_task_id {} did not succeed (exit_code={}); call task_context_boundary status and inspect verification_obligations.attempts for launch_phase, observed_cwd, and diagnostic excerpt or omission/hash markers before choosing a correction",
                                attempt.call_id, active.root_task_id, receipt.exit_code
                            )
                        } else {
                            format!(
                                "latest verification attempt {} did not succeed (exit_code={}); inspect its recorded command and result before choosing a correction",
                                attempt.call_id, receipt.exit_code
                            )
                        };
                        return Err(FunctionCallError::RespondToModel(message));
                    }
                    if invocation.turn.config.rencrow_autonomous_completion {
                        let unresolved = invocation
                            .session
                            .unresolved_task_check_obligations(&active)
                            .await;
                        if unresolved > 0 {
                            return Err(FunctionCallError::RespondToModel(format!(
                                "task has {unresolved} unresolved verification obligation(s); call task_context_boundary status for root_task_id {} and inspect verification_obligations.attempts for call_id, launch_phase, observed_cwd, and diagnostic excerpt or omission/hash markers before choosing a correction. A failure before verification started is not a completed check to rerun blindly",
                                active.root_task_id
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
                                common_harness_satisfied_conditions
                                    .as_deref()
                                    .or(satisfied_conditions.as_deref())
                                    .unwrap_or_default(),
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
                        let mut accepted_history = latest_state
                            .as_ref()
                            .map(|latest| latest.accepted_history.clone())
                            .unwrap_or_default();
                        accepted_history.extend(
                            latest_state
                                .as_ref()
                                .into_iter()
                                .flat_map(|latest| latest.completed.iter())
                                .map(|(task_id, receipt)| {
                                    codex_history::task_context::AcceptedTaskEvidenceReceipt {
                                        task_id: task_id.clone(),
                                        task_revision: receipt.revision,
                                        artifact_ref: receipt.artifact_ref.clone(),
                                        verification_ref: receipt.verification_ref.clone(),
                                    }
                                }),
                        );
                        let mut state_value = serde_json::to_value(&*state).map_err(|error| {
                            FunctionCallError::RespondToModel(error.to_string())
                        })?;
                        crate::session::session::canonicalize_completion_state_evidence(
                            &mut state_value,
                            &receipts,
                            &accepted_history,
                        )
                        .map_err(FunctionCallError::RespondToModel)?;
                        *state = serde_json::from_value(state_value).map_err(|error| {
                            FunctionCallError::RespondToModel(error.to_string())
                        })?;
                    }
                    if autonomous && active.task_id == active.root_task_id {
                        let _contract = completion_state.as_ref().ok_or_else(|| {
                            FunctionCallError::RespondToModel(
                                "root task cannot be accepted without a registered completion contract".into(),
                            )
                        })?;
                        if !invocation.session.autonomous_completion_is_complete().await {
                            return Err(FunctionCallError::RespondToModel(
                                "root task cannot be accepted while projected completion obligations, blockers, or unresolved runtime work remain".into(),
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
                        accepted_history: latest_state
                            .as_ref()
                            .map(|state| state.accepted_history.clone())
                            .unwrap_or_default(),
                    };
                    let validated =
                        validate_host_boundary_checkpoint(candidate, &facts).map_err(|error| {
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
