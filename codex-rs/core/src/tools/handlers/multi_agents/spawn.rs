use super::*;
use crate::agent::child_config::PreparedSpawnConfig;
use crate::agent::child_config::SpawnConfigOptions;
use crate::agent::child_config::SpawnConfigVersion;
use crate::agent::child_config::prepare_agent_spawn_config;
use crate::agent::control::render_input_preview;
use crate::agent::exceeds_thread_spawn_depth_limit;
use crate::agent::next_thread_spawn_depth;
use crate::agent::role::DEFAULT_ROLE_NAME;
use crate::agent::types::SpawnAgentForkMode;
use crate::agent::types::SpawnAgentOptions;
use crate::tools::handlers::multi_agents_spec::SpawnAgentToolOptions;
use crate::tools::handlers::multi_agents_spec::create_spawn_agent_tool_v1;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErrorDetails;
use codex_tools::ToolSpec;

#[derive(Debug)]
pub(crate) struct CompletionReviewerSpawnResult {
    pub(crate) agent_id: String,
    pub(crate) reviewer_role: String,
    pub(crate) model_route: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReviewSpawnFailureDisposition {
    RejectedBeforeCreation,
    ChildCreated(ThreadId),
    CreationUnknown,
}

fn review_spawn_failure_disposition(
    capacity_rejected: bool,
    created_thread_id: Option<ThreadId>,
) -> ReviewSpawnFailureDisposition {
    match created_thread_id {
        Some(thread_id) => ReviewSpawnFailureDisposition::ChildCreated(thread_id),
        None if capacity_rejected => ReviewSpawnFailureDisposition::RejectedBeforeCreation,
        None => ReviewSpawnFailureDisposition::CreationUnknown,
    }
}

pub(crate) fn configured_completion_reviewer_role(
    config: &crate::config::Config,
) -> Option<String> {
    let preferred = ["reviewer", "analyst", "verifier"];
    preferred
        .iter()
        .find(|name| configured_reviewer_role_declared(config, name))
        .map(|name| (*name).to_string())
        .or_else(|| {
            config
                .agent_roles
                .keys()
                .find(|name| configured_reviewer_role_declared(config, name))
                .cloned()
        })
}

pub(crate) fn completion_reviewer_config_is_read_only(
    config: &crate::config::Config,
    role_name: &str,
) -> bool {
    configured_read_only_reviewer_role(config, role_name)
}

fn configured_reviewer_role_declared(config: &crate::config::Config, role_name: &str) -> bool {
    config.agent_roles.get(role_name).is_some_and(|role| {
        reviewer_role_declaration_is_safe(role_name, role.description.as_deref(), true, true)
    })
}

pub(crate) fn validate_completion_reviewer_depth(
    session_source: &codex_protocol::protocol::SessionSource,
    max_depth: i32,
) -> Result<(), String> {
    let child_depth = next_thread_spawn_depth(session_source);
    if exceeds_thread_spawn_depth_limit(child_depth, max_depth) {
        Err("agent depth limit reached before completion review reservation".into())
    } else {
        Ok(())
    }
}

/// Spawn a review only through a configured read-only role. The call is internal to
/// task_context_boundary; callers cannot select a role or model fallback.
pub(crate) async fn spawn_configured_completion_review(
    invocation: &ToolInvocation,
    prepared: PreparedSpawnConfig,
    unit_id: &str,
    target_task_id: &str,
    target_task_revision: u64,
    target_ref: &str,
    review_brief: &str,
) -> Result<CompletionReviewerSpawnResult, FunctionCallError> {
    let session = &invocation.session;
    let step_context = &invocation.step_context;
    let turn = &invocation.turn;
    let role = prepared.role_name.clone().ok_or_else(|| {
        FunctionCallError::RespondToModel("configured reviewer role is missing".into())
    })?;
    if !configured_read_only_reviewer_role(&prepared.config, &role) {
        return Err(FunctionCallError::RespondToModel(
            "configured reviewer role is not read-only under the active execution guard".into(),
        ));
    }
    let configured_model = prepared
        .config
        .model
        .clone()
        .filter(|model| !model.trim().is_empty())
        .ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "configured reviewer role has no explicit model route".into(),
            )
        })?;
    let reasoning_effort = prepared.config.model_reasoning_effort.clone();
    let child_depth = next_thread_spawn_depth(&turn.session_source);
    validate_completion_reviewer_depth(&turn.session_source, turn.config.agent_max_depth)
        .map_err(FunctionCallError::RespondToModel)?;
    let prompt = format!(
        "Host-assigned read-only review. Inspect task `{target_task_id}` revision {target_task_revision} against accepted artifact reference `{target_ref}`.\n{review_brief}\nUse the listed repository-relative artifact paths and stated acceptance criteria as your review basis. Use code and existing host evidence only; do not edit files or rerun tests. Return findings to the parent for an explicit adoption decision."
    );
    let input_items = vec![UserInput::Text {
        text: prompt.clone(),
        text_elements: Vec::new(),
    }];
    let call_id = invocation.call_id.clone();
    session
        .emit_turn_item_started(
            turn,
            &TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                id: call_id.clone(),
                tool: CollabAgentTool::SpawnAgent,
                status: CollabAgentToolCallStatus::InProgress,
                sender_thread_id: session.thread_id,
                receiver_thread_ids: Vec::new(),
                receiver_agents: Vec::new(),
                prompt: Some(prompt.clone()),
                model: Some(configured_model.clone()),
                reasoning_effort: reasoning_effort.clone(),
                agents_states: Default::default(),
            }),
        )
        .await;
    let (spawned, created_thread_rx) = session
        .services
        .agent_control
        .spawn_agent_with_metadata_tracking_creation(
            prepared.config,
            input_items,
            Some(thread_spawn_source(
                session.thread_id,
                &turn.session_source,
                child_depth,
                Some(&role),
                /*task_name*/ None,
            )?),
            SpawnAgentOptions {
                parent_thread_id: Some(session.thread_id),
                parent_turn_id: Some(turn.sub_id.clone()),
                root_turn_id: turn.turn_metadata_state.root_turn_id(),
                turn_trigger: turn.turn_metadata_state.current_turn_trigger(),
                environments: Some(step_context.environments.to_selections()),
                ..Default::default()
            },
        )
        .await;
    let created_after_error = if spawned.is_err() {
        created_thread_rx.await.ok()
    } else {
        None
    };
    let (thread_id, status, agent_role) = match &spawned {
        Ok(child) => (
            Some(child.thread_id),
            child.status.clone(),
            child.metadata.agent_role.clone(),
        ),
        Err(_) => match created_after_error {
            Some(thread_id) => {
                let status = session
                    .services
                    .agent_control
                    .get_agent_status_generation(thread_id)
                    .await
                    .map(|(_, status)| status)
                    .unwrap_or(AgentStatus::PendingInit);
                let role = session
                    .services
                    .agent_control
                    .get_agent_metadata(thread_id)
                    .and_then(|metadata| metadata.agent_role);
                (Some(thread_id), status, role)
            }
            None => (None, AgentStatus::NotFound, None),
        },
    };
    let receiver_agents = thread_id
        .and_then(|id| {
            agent_role.clone().map(|agent_role| {
                vec![CollabAgentRef {
                    thread_id: id,
                    agent_nickname: None,
                    agent_role: Some(agent_role),
                }]
            })
        })
        .unwrap_or_default();
    session
        .emit_turn_item_completed(
            turn,
            TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                id: call_id.clone(),
                tool: CollabAgentTool::SpawnAgent,
                status: collab_tool_call_status(&status, thread_id),
                sender_thread_id: session.thread_id,
                receiver_thread_ids: thread_id.into_iter().collect(),
                receiver_agents,
                prompt: Some(prompt),
                model: Some(configured_model.clone()),
                reasoning_effort: reasoning_effort.clone(),
                agents_states: thread_id
                    .map(|id| [(id, status.clone())].into_iter().collect())
                    .unwrap_or_default(),
            }),
        )
        .await;
    let child = match spawned {
        Ok(child) => child,
        Err(error) => {
            match review_spawn_failure_disposition(
                matches!(error.details(), CodexErrorDetails::AgentLimitReached { .. }),
                created_after_error,
            ) {
                ReviewSpawnFailureDisposition::RejectedBeforeCreation => {
                    session
                        .rollback_unstarted_completion_review(unit_id)
                        .await
                        .map_err(FunctionCallError::RespondToModel)?;
                }
                ReviewSpawnFailureDisposition::ChildCreated(child_id) => {
                    if let Err(persist_error) = session
                        .record_completion_review_child_pending(
                            unit_id,
                            target_task_id,
                            target_task_revision,
                            target_ref.to_string(),
                            child_id,
                            role.clone(),
                            call_id.clone(),
                        )
                        .await
                    {
                        session.pause_autonomous_completion().await;
                        return Err(FunctionCallError::RespondToModel(format!(
                            "review spawn call {call_id} created child {child_id}, but its pending binding could not be persisted: {persist_error}; do not spawn again"
                        )));
                    }
                    return Err(FunctionCallError::RespondToModel(format!(
                        "review spawn call {call_id} created child {child_id}, but initial input delivery failed: {error}; the child is bound as pending. Reconcile and wait for this same child; do not spawn another reviewer"
                    )));
                }
                ReviewSpawnFailureDisposition::CreationUnknown => {
                    session
                        .mark_completion_review_launch_unknown(
                            unit_id,
                            &call_id,
                            format!("spawn outcome is ambiguous: {error}"),
                        )
                        .await
                        .map_err(FunctionCallError::RespondToModel)?;
                    return Err(FunctionCallError::RespondToModel(format!(
                        "review spawn outcome is unknown for launch call {call_id}: {error}. Do not spawn again. Inspect the parent rollout's spawn_agent call and host child list; wait only if an exact child can be established"
                    )));
                }
            }
            return Err(collab_spawn_error(error));
        }
    };
    if let Err(error) = session
        .record_completion_review_child_pending(
            unit_id,
            target_task_id,
            target_task_revision,
            target_ref.to_string(),
            child.thread_id,
            role.clone(),
            call_id.clone(),
        )
        .await
    {
        session.pause_autonomous_completion().await;
        return Err(FunctionCallError::RespondToModel(format!(
            "reviewer child {} started but its launch binding could not be persisted: {error}",
            child.thread_id
        )));
    }
    session
        .observe_completion_child_tool(
            "spawn_agent",
            &serde_json::json!({"completion_review": unit_id}),
            &serde_json::json!({"agent_id": child.thread_id.to_string()}),
        )
        .await;
    let reviewer_turn_id = session
        .services
        .agent_control
        .get_agent_status_generation(child.thread_id)
        .await
        .ok()
        .and_then(|(turn_id, _)| turn_id);
    let snapshot = session
        .services
        .agent_control
        .get_agent_config_snapshot(child.thread_id)
        .await;
    let Some(reviewer_turn_id) = reviewer_turn_id else {
        return Err(FunctionCallError::RespondToModel(
            "reviewer spawned but its turn generation is unresolved; review unit remains pending"
                .into(),
        ));
    };
    if child.metadata.agent_role.as_deref() != Some(role.as_str())
        || snapshot.as_ref().map(|snapshot| snapshot.model.as_str())
            != Some(configured_model.as_str())
    {
        return Err(FunctionCallError::RespondToModel(
            "reviewer spawned but its configured role/model route could not be verified; review unit remains pending".into(),
        ));
    }
    session
        .record_completion_review_spawn(
            unit_id,
            target_task_id,
            target_task_revision,
            target_ref.to_string(),
            child.thread_id,
            reviewer_turn_id,
            role.clone(),
            call_id.clone(),
        )
        .await
        .map_err(FunctionCallError::RespondToModel)?;
    Ok(CompletionReviewerSpawnResult {
        agent_id: child.thread_id.to_string(),
        reviewer_role: role,
        model_route: configured_model,
    })
}

#[derive(Default)]
pub(crate) struct Handler {
    options: SpawnAgentToolOptions,
}

impl Handler {
    pub(crate) fn new(options: SpawnAgentToolOptions) -> Self {
        Self { options }
    }
}

impl ToolExecutor<ToolInvocation> for Handler {
    fn tool_name(&self) -> ToolName {
        ToolName::namespaced(MULTI_AGENT_V1_NAMESPACE, "spawn_agent")
    }

    fn spec(&self) -> ToolSpec {
        create_spawn_agent_tool_v1(self.options.clone())
    }

    fn search_info(&self) -> Option<ToolSearchInfo> {
        multi_agent_tool_search_info(
            "spawn_agent spawn agent subagent sub-agent delegate delegation parallel work worker explorer no-apps fork model reasoning",
            self.spec(),
        )
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move { handle_spawn_agent(invocation).await.map(boxed_tool_output) })
    }
}

async fn handle_spawn_agent(
    invocation: ToolInvocation,
) -> Result<SpawnAgentResult, FunctionCallError> {
    let ToolInvocation {
        session,
        step_context,
        payload,
        call_id,
        ..
    } = invocation;
    let turn = &step_context.turn;
    let arguments = function_arguments(payload)?;
    let args: SpawnAgentArgs = parse_arguments(&arguments)?;
    let role_name = args
        .agent_type
        .as_deref()
        .map(str::trim)
        .filter(|role| !role.is_empty());
    let mut input_items = parse_collab_input(args.message, args.items)?;
    let prompt = render_input_preview(&input_items);
    let session_source = turn.session_source.clone();
    let child_depth = next_thread_spawn_depth(&session_source);
    let max_depth = turn.config.agent_max_depth;
    if exceeds_thread_spawn_depth_limit(child_depth, max_depth) {
        return Err(FunctionCallError::RespondToModel(
            "Agent depth limit reached. Solve the task yourself.".to_string(),
        ));
    }
    session
        .emit_turn_item_started(
            turn,
            &TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                id: call_id.clone(),
                tool: CollabAgentTool::SpawnAgent,
                status: CollabAgentToolCallStatus::InProgress,
                sender_thread_id: session.thread_id,
                receiver_thread_ids: Vec::new(),
                receiver_agents: Vec::new(),
                prompt: Some(prompt.clone()),
                model: Some(args.model.clone().unwrap_or_default()),
                reasoning_effort: Some(args.reasoning_effort.clone().unwrap_or_default()),
                agents_states: Default::default(),
            }),
        )
        .await;
    let prepared = prepare_agent_spawn_config(
        &session,
        step_context.as_ref(),
        SpawnConfigOptions {
            version: SpawnConfigVersion::V1,
            full_history_fork: args.fork_context,
            role_name,
            model: args.model.as_deref(),
            reasoning_effort: args.reasoning_effort.clone(),
        },
    )
    .await
    .map_err(FunctionCallError::RespondToModel)?;
    let config = prepared.config;
    let completion_review = if let Some(review) = args.completion_review.as_ref() {
        let role = prepared.role_name.as_deref().ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "completion review requires a configured reviewer role".to_string(),
            )
        })?;
        if !configured_read_only_reviewer_role(&config, role) {
            return Err(FunctionCallError::RespondToModel(
                "completion review requires a configured reviewer/analyst/verifier role with read-only permissions".to_string(),
            ));
        }
        let target_ref = session
            .reserve_completion_review_launch(
                &review.unit_id,
                &review.target_task_id,
                review.target_task_revision,
                &call_id,
            )
            .await
            .map_err(FunctionCallError::RespondToModel)?;
        input_items.push(UserInput::Text {
            text: format!(
                "Host-assigned read-only review. Inspect task `{}` revision {} against accepted artifact reference `{target_ref}`. Use code and already available host evidence only; do not edit files or rerun tests. Return findings to the parent for an explicit adoption decision.",
                review.target_task_id, review.target_task_revision
            ),
            text_elements: Vec::new(),
        });
        Some((review, role.to_string(), target_ref))
    } else {
        None
    };
    let spawn_source = Some(thread_spawn_source(
        session.thread_id,
        &turn.session_source,
        child_depth,
        prepared.role_name.as_deref(),
        /*task_name*/ None,
    )?);
    let spawn_options = SpawnAgentOptions {
        fork_parent_spawn_call_id: args.fork_context.then(|| call_id.clone()),
        fork_mode: args.fork_context.then_some(SpawnAgentForkMode::FullHistory),
        parent_thread_id: Some(session.thread_id),
        parent_turn_id: Some(turn.sub_id.clone()),
        root_turn_id: turn.turn_metadata_state.root_turn_id(),
        turn_trigger: turn.turn_metadata_state.current_turn_trigger(),
        environments: Some(step_context.environments.to_selections()),
        multi_agent_v2_usage_hints: None,
        cyber_access_program: turn.cyber_access_program,
    };
    let (result, created_thread_rx) = if completion_review.is_some() {
        let (result, receiver) = session
            .services
            .agent_control
            .spawn_agent_with_metadata_tracking_creation(
                config,
                input_items,
                spawn_source,
                spawn_options,
            )
            .await;
        (result, Some(receiver))
    } else {
        (
            session
                .services
                .agent_control
                .spawn_agent_with_metadata(config, input_items, spawn_source, spawn_options)
                .await,
            None,
        )
    };
    let created_after_error = if result.is_err() {
        match created_thread_rx {
            Some(receiver) => receiver.await.ok(),
            None => None,
        }
    } else {
        None
    };
    if let (Some((review, role, target_ref)), Err(error)) = (&completion_review, &result) {
        match review_spawn_failure_disposition(
            matches!(error.details(), CodexErrorDetails::AgentLimitReached { .. }),
            created_after_error,
        ) {
            ReviewSpawnFailureDisposition::RejectedBeforeCreation => {
                session
                    .rollback_unstarted_completion_review(&review.unit_id)
                    .await
                    .map_err(FunctionCallError::RespondToModel)?;
            }
            ReviewSpawnFailureDisposition::ChildCreated(child_id) => {
                session
                    .record_completion_review_child_pending(
                        &review.unit_id,
                        &review.target_task_id,
                        review.target_task_revision,
                        target_ref.clone(),
                        child_id,
                        role.clone(),
                        call_id.clone(),
                    )
                    .await
                    .map_err(FunctionCallError::RespondToModel)?;
            }
            ReviewSpawnFailureDisposition::CreationUnknown => {
                session
                    .mark_completion_review_launch_unknown(
                        &review.unit_id,
                        &call_id,
                        format!("spawn outcome is ambiguous: {error}"),
                    )
                    .await
                    .map_err(FunctionCallError::RespondToModel)?;
            }
        }
    }
    let (new_thread_id, new_agent_metadata, status) = match &result {
        Ok(spawned_agent) => (
            Some(spawned_agent.thread_id),
            Some(spawned_agent.metadata.clone()),
            spawned_agent.status.clone(),
        ),
        Err(_) => match created_after_error {
            Some(thread_id) => (
                Some(thread_id),
                session.services.agent_control.get_agent_metadata(thread_id),
                session
                    .services
                    .agent_control
                    .get_agent_status_generation(thread_id)
                    .await
                    .map(|(_, status)| status)
                    .unwrap_or(AgentStatus::PendingInit),
            ),
            None => (None, None, AgentStatus::NotFound),
        },
    };
    let agent_snapshot = match new_thread_id {
        Some(thread_id) => {
            session
                .services
                .agent_control
                .get_agent_config_snapshot(thread_id)
                .await
        }
        None => None,
    };
    if let (Some((review, role, target_ref)), Ok(spawned_agent)) = (completion_review, &result) {
        let reviewer_turn_id = session
            .services
            .agent_control
            .get_agent_status_generation(spawned_agent.thread_id)
            .await
            .ok()
            .and_then(|(turn_id, _)| turn_id);
        if spawned_agent.metadata.agent_role.as_deref() != Some(role.as_str())
            || reviewer_turn_id.is_none()
        {
            session.pause_autonomous_completion().await;
            tracing::error!(
                reviewer_thread_id = %spawned_agent.thread_id,
                "completion reviewer role or initial turn generation could not be verified"
            );
        } else if let Some(reviewer_turn_id) = reviewer_turn_id
            && let Err(error) = session
                .record_completion_review_spawn(
                    &review.unit_id,
                    &review.target_task_id,
                    review.target_task_revision,
                    target_ref,
                    spawned_agent.thread_id,
                    reviewer_turn_id,
                    role,
                    call_id.clone(),
                )
                .await
        {
            tracing::error!(%error, "failed to persist completion reviewer spawn evidence");
            session.pause_autonomous_completion().await;
        }
    }
    let (_new_agent_path, new_agent_nickname, new_agent_role) =
        match (&agent_snapshot, new_agent_metadata) {
            (Some(snapshot), _) => (
                snapshot.session_source.get_agent_path().map(String::from),
                snapshot.session_source.get_nickname(),
                snapshot.session_source.get_agent_role(),
            ),
            (None, Some(metadata)) => (
                metadata.agent_path.map(String::from),
                metadata.agent_nickname,
                metadata.agent_role,
            ),
            (None, None) => (None, None, None),
        };
    let effective_model = agent_snapshot
        .as_ref()
        .map(|snapshot| snapshot.model.clone())
        .unwrap_or_else(|| args.model.clone().unwrap_or_default());
    let effective_reasoning_effort = agent_snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.reasoning_effort.clone())
        .unwrap_or(args.reasoning_effort.unwrap_or_default());
    let nickname = new_agent_nickname.clone();
    let receiver_thread_ids = new_thread_id.into_iter().collect();
    let receiver_agents = new_thread_id
        .map(|thread_id| CollabAgentRef {
            thread_id,
            agent_nickname: new_agent_nickname,
            agent_role: new_agent_role,
        })
        .into_iter()
        .collect();
    let agents_states = new_thread_id
        .map(|thread_id| [(thread_id, status.clone())].into_iter().collect())
        .unwrap_or_default();
    session
        .emit_turn_item_completed(
            turn,
            TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                id: call_id,
                tool: CollabAgentTool::SpawnAgent,
                status: collab_tool_call_status(&status, new_thread_id),
                sender_thread_id: session.thread_id,
                receiver_thread_ids,
                receiver_agents,
                prompt: Some(prompt),
                model: Some(effective_model),
                reasoning_effort: Some(effective_reasoning_effort),
                agents_states,
            }),
        )
        .await;
    let new_thread_id = result.map_err(collab_spawn_error)?.thread_id;
    let role_tag = role_name.unwrap_or(DEFAULT_ROLE_NAME);
    turn.session_telemetry.counter(
        "codex.multi_agent.spawn",
        /*inc*/ 1,
        &[("role", role_tag), ("version", "v1")],
    );

    Ok(SpawnAgentResult {
        agent_id: new_thread_id.to_string(),
        nickname,
    })
}

fn configured_read_only_reviewer_role(config: &crate::config::Config, role_name: &str) -> bool {
    let Some(role) = config.agent_roles.get(role_name) else {
        return false;
    };
    reviewer_role_declaration_is_safe(
        role_name,
        role.description.as_deref(),
        config.rencrow_read_only,
        config
            .rencrow_execution_guard
            .as_ref()
            .is_some_and(|guard| {
                guard.task_kind == Some(codex_config::RencrowTaskKind::ReadOnly)
                    && !guard.require_edit
            }),
    )
}

fn reviewer_role_declaration_is_safe(
    role_name: &str,
    role_description: Option<&str>,
    read_only: bool,
    read_only_guard: bool,
) -> bool {
    let name_is_reviewer = role_name
        .split(|character: char| !character.is_ascii_alphanumeric())
        .any(|part| {
            matches!(
                part.to_ascii_lowercase().as_str(),
                "reviewer" | "analyst" | "verifier"
            )
        });
    let description_is_reviewer = role_description.is_some_and(|description| {
        let description = description.to_ascii_lowercase();
        description.contains("review")
            || description.contains("analyst")
            || description.contains("verif")
    });
    read_only && read_only_guard && (name_is_reviewer || description_is_reviewer)
}

impl CoreToolRuntime for Handler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}

#[derive(Debug, Deserialize)]
struct SpawnAgentArgs {
    message: Option<String>,
    items: Option<Vec<UserInput>>,
    agent_type: Option<String>,
    model: Option<String>,
    reasoning_effort: Option<ReasoningEffort>,
    #[serde(default)]
    fork_context: bool,
    completion_review: Option<CompletionReviewRequest>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CompletionReviewRequest {
    unit_id: String,
    target_task_id: String,
    target_task_revision: u64,
}

#[derive(Debug, Serialize)]
pub(crate) struct SpawnAgentResult {
    agent_id: String,
    nickname: Option<String>,
}

impl ToolOutput for SpawnAgentResult {
    fn log_output(&self) -> String {
        tool_output_json_text(self, "spawn_agent")
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        tool_output_response_item(call_id, payload, self, Some(true), "spawn_agent")
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        tool_output_code_mode_result(self, "spawn_agent")
    }
}

#[cfg(test)]
mod completion_review_tests {
    use super::ReviewSpawnFailureDisposition;
    use super::review_spawn_failure_disposition;
    use super::reviewer_role_declaration_is_safe;
    use codex_protocol::ThreadId;

    #[test]
    fn completion_review_requires_configured_reviewer_identity_and_read_only_contract() {
        assert!(reviewer_role_declaration_is_safe(
            "analyst",
            Some("Review code and host evidence."),
            true,
            true,
        ));
        assert!(!reviewer_role_declaration_is_safe(
            "worker",
            Some("Implement a change."),
            true,
            true,
        ));
        assert!(!reviewer_role_declaration_is_safe(
            "analyst",
            Some("Review code."),
            false,
            true,
        ));
        assert!(!reviewer_role_declaration_is_safe(
            "analyst",
            Some("Review code."),
            true,
            false,
        ));
        assert!(!reviewer_role_declaration_is_safe(
            "custom",
            Some("ordinary role"),
            true,
            true,
        ));
    }

    #[test]
    fn review_spawn_failure_classification_distinguishes_pre_and_post_creation() {
        let child = ThreadId::try_from("01a1c606-48b8-7cf3-bb78-77b47f6b8c2f")
            .expect("valid child thread ID");
        assert_eq!(
            review_spawn_failure_disposition(true, None),
            ReviewSpawnFailureDisposition::RejectedBeforeCreation
        );
        assert_eq!(
            review_spawn_failure_disposition(false, Some(child)),
            ReviewSpawnFailureDisposition::ChildCreated(child)
        );
        assert_eq!(
            review_spawn_failure_disposition(false, None),
            ReviewSpawnFailureDisposition::CreationUnknown
        );
        assert_eq!(
            review_spawn_failure_disposition(true, Some(child)),
            ReviewSpawnFailureDisposition::ChildCreated(child),
            "an actual child ID must never be rolled back as a capacity rejection"
        );
    }
}
