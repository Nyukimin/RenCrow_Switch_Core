use super::*;
use crate::agent::control::monitor::ChildMonitorReport;
use crate::agent::status::is_final;
use crate::session::session::Session;
use crate::tools::handlers::multi_agents_spec::WaitAgentTimeoutOptions;
use crate::tools::handlers::multi_agents_spec::create_wait_agent_tool_v1;
use codex_features::Feature;
use codex_protocol::error::CodexErrorDetails;
use codex_tools::ToolSpec;
use futures::FutureExt;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch::Receiver;
use tokio::time::Instant;

use crate::session::InputQueueActivity;
use crate::state::TurnState;
use tokio::sync::Mutex;
use tokio::time::sleep_until;

#[derive(Default)]
pub(crate) struct Handler {
    options: WaitAgentTimeoutOptions,
}

impl Handler {
    pub(crate) fn new(options: WaitAgentTimeoutOptions) -> Self {
        Self { options }
    }
}

impl ToolExecutor<ToolInvocation> for Handler {
    fn tool_name(&self) -> ToolName {
        ToolName::namespaced(MULTI_AGENT_V1_NAMESPACE, "wait_agent")
    }

    fn spec(&self) -> ToolSpec {
        create_wait_agent_tool_v1(self.options)
    }

    fn search_info(&self) -> Option<ToolSearchInfo> {
        multi_agent_tool_search_info(
            "wait_agent wait agent subagent status final result complete timeout targets",
            self.spec(),
        )
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(self.handle_call(invocation))
    }
}

impl Handler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            payload,
            call_id,
            ..
        } = invocation;
        let arguments = function_arguments(payload)?;
        let args: WaitArgs = parse_arguments(&arguments)?;
        let receiver_thread_ids = parse_agent_id_targets(args.targets)?;
        session.enforce_due_completion_review_deadlines().await;
        let mut review_deadline_escalations = session
            .completion_review_deadline_escalations(&receiver_thread_ids)
            .await;
        let mut receiver_agents = Vec::with_capacity(receiver_thread_ids.len());
        let mut target_by_thread_id = HashMap::with_capacity(receiver_thread_ids.len());
        for receiver_thread_id in &receiver_thread_ids {
            let agent_metadata = session
                .services
                .agent_control
                .get_agent_metadata(*receiver_thread_id)
                .unwrap_or_default();
            target_by_thread_id.insert(
                *receiver_thread_id,
                agent_metadata
                    .agent_path
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| receiver_thread_id.to_string()),
            );
            receiver_agents.push(CollabAgentRef {
                thread_id: *receiver_thread_id,
                agent_nickname: agent_metadata.agent_nickname,
                agent_role: agent_metadata.agent_role,
            });
        }

        let timeout_ms = args.timeout_ms.unwrap_or(DEFAULT_WAIT_TIMEOUT_MS);
        let timeout_ms = match timeout_ms {
            ms if ms <= 0 => {
                return Err(FunctionCallError::RespondToModel(
                    "timeout_ms must be greater than zero".to_owned(),
                ));
            }
            ms => ms.clamp(MIN_WAIT_TIMEOUT_MS, MAX_WAIT_TIMEOUT_MS),
        };

        session
            .emit_turn_item_started(
                &turn,
                &TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                    id: call_id.clone(),
                    tool: CollabAgentTool::Wait,
                    status: CollabAgentToolCallStatus::InProgress,
                    sender_thread_id: session.thread_id,
                    receiver_thread_ids: receiver_thread_ids.clone(),
                    receiver_agents: receiver_agents.clone(),
                    prompt: None,
                    model: None,
                    reasoning_effort: None,
                    agents_states: Default::default(),
                }),
            )
            .await;

        let turn_state = session
            .input_queue
            .turn_state_for_sub_id(&session.active_turn, &turn.sub_id)
            .await;
        let (activity_rx, _) = session
            .input_queue
            .subscribe_activity(turn_state.as_deref())
            .await;

        let mut status_rxs = Vec::with_capacity(receiver_thread_ids.len());
        let mut initial_final_statuses: Vec<(ThreadId, Option<String>, AgentStatus)> = Vec::new();
        let mut escalated_running_generation = false;
        for id in &receiver_thread_ids {
            match session.services.agent_control.subscribe_status(*id).await {
                Ok(rx) => {
                    let (turn_id, status) = session
                        .services
                        .agent_control
                        .get_agent_status_generation(*id)
                        .await
                        .unwrap_or_else(|_| (None, rx.borrow().clone()));
                    if is_final(&status) {
                        initial_final_statuses.push((*id, turn_id, status));
                    } else if review_deadline_escalations.get(id).is_some_and(
                        |(reviewer_turn_id, _)| {
                            turn_id.as_deref() == Some(reviewer_turn_id.as_str())
                        },
                    ) {
                        // The host persisted an escalation for this same running generation.
                        // Return it to the caller instead of entering another unbounded wait.
                        escalated_running_generation = true;
                        continue;
                    } else {
                        status_rxs.push((*id, rx));
                    }
                }
                Err(err) if matches!(err.details(), CodexErrorDetails::ThreadNotFound(_)) => {
                    initial_final_statuses.push((*id, None, AgentStatus::NotFound));
                }
                Err(err) => {
                    let mut statuses = HashMap::with_capacity(1);
                    statuses.insert(*id, session.services.agent_control.get_status(*id).await);
                    session
                        .emit_turn_item_completed(
                            &turn,
                            TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                                id: call_id.clone(),
                                tool: CollabAgentTool::Wait,
                                status: wait_tool_call_status(&statuses),
                                sender_thread_id: session.thread_id,
                                receiver_thread_ids: statuses.keys().copied().collect(),
                                receiver_agents: wait_receiver_agents(&statuses, &receiver_agents),
                                prompt: None,
                                model: None,
                                reasoning_effort: None,
                                agents_states: statuses,
                            }),
                        )
                        .await;
                    return Err(collab_agent_error(*id, err));
                }
            }
        }

        let pending_activity = session
            .input_queue
            .subscribe_activity(turn_state.as_deref())
            .await
            .1;

        let (statuses, steered) = if pending_activity == Some(InputQueueActivity::Steer) {
            (Vec::new(), true)
        } else if !initial_final_statuses.is_empty() {
            (initial_final_statuses, false)
        } else {
            let futures = FuturesUnordered::new();
            for (id, rx) in status_rxs.into_iter() {
                let session = session.clone();
                futures.push(wait_for_final_status(session, id, rx));
            }
            let requested_deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);
            let deadline = session
                .next_completion_review_deadline(&receiver_thread_ids)
                .await
                .map(|deadline_ms| {
                    let remaining_ms = deadline_ms
                        .saturating_sub(chrono::Utc::now().timestamp_millis())
                        .max(0) as u64;
                    Instant::now() + Duration::from_millis(remaining_ms)
                })
                .map_or(requested_deadline, |review_deadline| {
                    requested_deadline.min(review_deadline)
                });
            match wait_for_statuses_or_steer(
                Arc::clone(&session),
                futures,
                activity_rx,
                turn_state,
                pending_activity,
                deadline,
            )
            .await
            {
                WaitStatusesOutcome::Completed(results) => (results, false),
                WaitStatusesOutcome::Steered => (Vec::new(), true),
                WaitStatusesOutcome::TimedOut => {
                    session.enforce_due_completion_review_deadlines().await;
                    review_deadline_escalations = session
                        .completion_review_deadline_escalations(&receiver_thread_ids)
                        .await;
                    let mut final_statuses = Vec::new();
                    escalated_running_generation = false;
                    for id in &receiver_thread_ids {
                        if let Ok((turn_id, status)) = session
                            .services
                            .agent_control
                            .get_agent_status_generation(*id)
                            .await
                        {
                            if is_final(&status) {
                                final_statuses.push((*id, turn_id, status));
                            } else if review_deadline_escalations.get(id).is_some_and(
                                |(reviewer_turn_id, _)| {
                                    turn_id.as_deref() == Some(reviewer_turn_id.as_str())
                                },
                            ) {
                                escalated_running_generation = true;
                            }
                        }
                    }
                    (final_statuses, false)
                }
            }
        };

        session
            .collect_completion_review_results(&call_id, &statuses)
            .await;
        if turn.config.features.enabled(Feature::RencrowChildMonitor) {
            for (child, child_turn, status) in &statuses {
                if let Some(child_turn) = child_turn
                    && matches!(status, AgentStatus::Completed(_) | AgentStatus::Errored(_))
                {
                    session
                        .services
                        .agent_control
                        .mark_child_result_collected(
                            *child,
                            child_turn,
                            crate::turn_timing::now_unix_timestamp_ms(),
                        )
                        .await;
                }
            }
        }

        let timed_out = statuses.is_empty() && !steered;
        let statuses_by_id = statuses
            .iter()
            .map(|(thread_id, _, status)| (*thread_id, status.clone()))
            .collect::<HashMap<_, _>>();
        let child_monitor = if turn.config.features.enabled(Feature::RencrowChildMonitor) {
            let children = receiver_thread_ids
                .iter()
                .map(|id| (*id, target_by_thread_id[id].clone()))
                .collect::<Vec<_>>();
            let mut report = session
                .services
                .agent_control
                .child_monitor_report(&children)
                .await;
            session
                .project_child_work_deadlines(&mut report, &children)
                .await;
            Some(report)
        } else {
            None
        };
        let result = WaitAgentResult {
            child_monitor,
            status: statuses
                .into_iter()
                .filter_map(|(thread_id, _, status)| {
                    target_by_thread_id
                        .get(&thread_id)
                        .cloned()
                        .map(|target| (target, status))
                })
                .collect(),
            timed_out,
            message: if steered {
                Some("Wait interrupted by pending parent input. No child completion was observed; child execution is unchanged.".to_string())
            } else if statuses_by_id.is_empty() && escalated_running_generation {
                Some("A host-owned completion reviewer deadline expired. Cancellation was withheld because active tool or job state could not be proven safe; the host escalation is persisted. This reviewer generation will not be waited on or resubmitted again automatically.".to_string())
            } else {
                None
            },
        };

        session
            .emit_turn_item_completed(
                &turn,
                TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                    id: call_id,
                    tool: CollabAgentTool::Wait,
                    status: wait_tool_call_status(&statuses_by_id),
                    sender_thread_id: session.thread_id,
                    receiver_thread_ids: statuses_by_id.keys().copied().collect(),
                    receiver_agents: wait_receiver_agents(&statuses_by_id, &receiver_agents),
                    prompt: None,
                    model: None,
                    reasoning_effort: None,
                    agents_states: statuses_by_id,
                }),
            )
            .await;

        Ok(boxed_tool_output(result))
    }
}

fn wait_tool_call_status(statuses: &HashMap<ThreadId, AgentStatus>) -> CollabAgentToolCallStatus {
    if statuses
        .values()
        .any(|status| matches!(status, AgentStatus::Errored(_) | AgentStatus::NotFound))
    {
        CollabAgentToolCallStatus::Failed
    } else {
        CollabAgentToolCallStatus::Completed
    }
}

fn wait_receiver_agents(
    statuses: &HashMap<ThreadId, AgentStatus>,
    receiver_agents: &[CollabAgentRef],
) -> Vec<CollabAgentRef> {
    if statuses.is_empty() {
        return Vec::new();
    }

    let mut agents = Vec::with_capacity(statuses.len());
    let mut seen = HashMap::with_capacity(receiver_agents.len());
    for receiver_agent in receiver_agents {
        seen.insert(receiver_agent.thread_id, ());
        if statuses.contains_key(&receiver_agent.thread_id) {
            agents.push(receiver_agent.clone());
        }
    }

    let mut extras = statuses
        .keys()
        .filter(|thread_id| !seen.contains_key(thread_id))
        .map(|thread_id| CollabAgentRef {
            thread_id: *thread_id,
            agent_nickname: None,
            agent_role: None,
        })
        .collect::<Vec<_>>();
    extras.sort_by_key(|agent| agent.thread_id.to_string());
    agents.extend(extras);
    agents
}

impl CoreToolRuntime for Handler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}

#[derive(Debug, Deserialize)]
struct WaitArgs {
    #[serde(default)]
    targets: Vec<String>,
    timeout_ms: Option<i64>,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct WaitAgentResult {
    pub(crate) status: HashMap<String, AgentStatus>,
    pub(crate) timed_out: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) child_monitor: Option<ChildMonitorReport>,
}

impl ToolOutput for WaitAgentResult {
    fn log_output(&self) -> String {
        tool_output_json_text(self, "wait_agent")
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        tool_output_response_item(call_id, payload, self, /*success*/ None, "wait_agent")
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        tool_output_code_mode_result(self, "wait_agent")
    }
}

async fn wait_for_final_status(
    session: Arc<Session>,
    thread_id: ThreadId,
    mut status_rx: Receiver<AgentStatus>,
) -> Option<(ThreadId, Option<String>, AgentStatus)> {
    let mut sample = session
        .services
        .agent_control
        .get_agent_status_generation(thread_id)
        .await
        .ok();
    if let Some((turn_id, status)) = sample.take()
        && is_final(&status)
    {
        return Some((thread_id, turn_id, status));
    }

    loop {
        if status_rx.changed().await.is_err() {
            let (turn_id, status) = session
                .services
                .agent_control
                .get_agent_status_generation(thread_id)
                .await
                .ok()?;
            return is_final(&status).then_some((thread_id, turn_id, status));
        }
        if let Ok((turn_id, status)) = session
            .services
            .agent_control
            .get_agent_status_generation(thread_id)
            .await
            && is_final(&status)
        {
            return Some((thread_id, turn_id, status));
        }
    }
}

enum WaitStatusesOutcome {
    Completed(Vec<(ThreadId, Option<String>, AgentStatus)>),
    Steered,
    TimedOut,
}

async fn wait_for_statuses_or_steer<F>(
    session: Arc<Session>,
    mut futures: FuturesUnordered<F>,
    mut activity_rx: Receiver<InputQueueActivity>,
    turn_state: Option<Arc<Mutex<TurnState>>>,
    pending_activity: Option<InputQueueActivity>,
    deadline: Instant,
) -> WaitStatusesOutcome
where
    F: Future<Output = Option<(ThreadId, Option<String>, AgentStatus)>>,
{
    if pending_activity == Some(InputQueueActivity::Steer) {
        return WaitStatusesOutcome::Steered;
    }

    let mut results = Vec::new();
    let timer = sleep_until(deadline);
    tokio::pin!(timer);
    loop {
        tokio::select! {
            changed = activity_rx.changed() => {
                if changed.is_err() {
                    break;
                }
                activity_rx.borrow_and_update();
                let pending_activity = session
                    .input_queue
                    .subscribe_activity(turn_state.as_deref())
                    .await
                    .1;
                if pending_activity == Some(InputQueueActivity::Steer) {
                    return WaitStatusesOutcome::Steered;
                }
            }
            status = futures.next() => match status {
                Some(Some(status)) => {
                    results.push(status);
                    break;
                }
                Some(None) => continue,
                None => break,
            },
            () = &mut timer => break,
        }
    }

    if !results.is_empty() {
        loop {
            match futures.next().now_or_never() {
                Some(Some(Some(result))) => results.push(result),
                Some(Some(None)) => continue,
                Some(None) | None => break,
            }
        }
    }

    let pending_activity = session
        .input_queue
        .subscribe_activity(turn_state.as_deref())
        .await
        .1;
    if pending_activity == Some(InputQueueActivity::Steer) {
        return WaitStatusesOutcome::Steered;
    }
    if !results.is_empty() {
        return WaitStatusesOutcome::Completed(results);
    }
    WaitStatusesOutcome::TimedOut
}
