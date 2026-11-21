// Modified by RenCrow Switch Core, 2026-11-21: verify durable blocked-call recovery.
use super::*;
use crate::context::GuardianContextMode;

use super::tests::build_world_state_from_turn_context;
use super::tests::make_session_and_context;
use super::tests::raw_history_items;
use crate::context::CompactionSummary;
use crate::context::ContextualUserFragment;
use codex_history::CompactedItem;
use codex_history::InitialHistory;
use codex_history::ResponseItemEnvelope;
use codex_history::ResumedHistory;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::SessionContextWindow;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::ThreadRolledBackEvent;
use codex_protocol::protocol::WorldStateItem;
use codex_protocol::security_risk::SecurityRiskScore;
use codex_rollout::ModelContextScan;
use codex_rollout::ModelContextScanProgress;
use core_test_support::responses::strip_metadata_from_items;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::PathBuf;
use test_case::test_case;
use uuid::Uuid;

fn legacy_blocked_exec_call(call_id: &str, turn_id: &str) -> RolloutItem {
    RolloutItem::ResponseItem(ResponseItemEnvelope {
        item: serde_json::from_value(json!({
            "type": "function_call",
            "call_id": call_id,
            "name": "exec_command",
            "arguments": "{\"cmd\":\"git status --porcelain\"}",
            "internal_chat_message_metadata_passthrough": {"turn_id": turn_id}
        }))
        .expect("exec call fixture"),
        metadata: None,
    })
}

fn legacy_blocked_turn_complete(turn_id: &str, message: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::TurnComplete(
        serde_json::from_value(json!({
            "turn_id": turn_id,
            "last_agent_message": null,
            "error": {"message": message, "codex_error_info": "other"},
            "started_at": null,
            "completed_at": null,
            "duration_ms": null,
            "time_to_first_token_ms": null
        }))
        .expect("fatal turn-complete fixture"),
    ))
}

fn legacy_blocked_rollout() -> Vec<RolloutItem> {
    let calls = [
        (
            "call_4086ff8b87ed4ac899346458",
            "01a1f776-0f5b-7db3-98bd-bff717fc5e3a",
            "Fatal error: RENCROW_READ_BUDGET_EXCEEDED: repeated host refusals stopped the turn; RENCROW_READ_BUDGET_EXCEEDED: same read command repeated without accepted progress; command not executed. Do not make further reads. If this contract permits completion without a patch or check, report evidence-backed findings or a plan now; if more investigation is essential, use allowed delegation and collect its result. State any unknowns. Do not invent a patch or run an unrelated check.",
        ),
        (
            "call_6efcf002dc8b488eb2636681",
            "01a1f91e-9680-7532-ac6e-6083b332c48b",
            "Fatal error: RENCROW_READ_BUDGET_EXCEEDED: repeated host refusals stopped the turn; RENCROW_READ_BUDGET_EXCEEDED: same read command repeated without accepted progress; command not executed. Do not make further reads. If this contract permits completion without a patch or check, report evidence-backed findings or a plan now; if more investigation is essential, use allowed delegation and collect its result. State any unknowns. Do not invent a patch or run an unrelated check.",
        ),
        (
            "call_4515721a156c404aa1978bb0",
            "01a1fcfa-b360-7ee3-bd06-ee7cc17a58b9",
            "Fatal error: RENCROW_READ_BUDGET_EXCEEDED: repeated host refusals stopped the turn; RENCROW_READ_BUDGET_EXCEEDED: bounded diagnostics for the failed current diff are exhausted; command not executed. Do not make further reads. Run and pass the required check directly, then complete with evidence-backed results. If investigation is still essential, use allowed delegation and collect its result.",
        ),
    ];
    let mut items = Vec::new();
    for (call_id, turn_id, error) in calls {
        items.push(legacy_blocked_exec_call(call_id, turn_id));
        items.push(legacy_blocked_turn_complete(turn_id, error));
    }
    items
}

fn legacy_exact_blocked_fatal_output(
    call_id: &str,
    thread_id: &str,
    fatal_message: &str,
) -> RolloutItem {
    let body = fatal_message
        .strip_prefix("Fatal error: ")
        .expect("reserved Fatal message prefix");
    let reference = codex_history::ObservationReference::new(
        thread_id,
        call_id,
        codex_history::archive_reference::content_sha256(body),
    );
    RolloutItem::ResponseItem(ResponseItemEnvelope {
        item: ResponseItem::FunctionCallOutput {
            id: None,
            call_id: Some(call_id.to_owned()),
            name: Some("exec_command".to_owned()),
            namespace: None,
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::Text(body.to_owned()),
                success: Some(false),
            },
            internal_chat_message_metadata_passthrough: None,
        },
        metadata: Some(codex_history::CodexHarnessMetadata {
            rencrow_blocked_tool_call: Some(Box::new(
                codex_history::BlockedToolCallReference::new(reference),
            )),
            ..Default::default()
        }),
    })
}

fn legacy_command_started(call_id: &str, turn_id: &str, thread_id: &ThreadId) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::ItemStarted(
        codex_protocol::protocol::ItemStartedEvent {
            thread_id: *thread_id,
            turn_id: turn_id.to_owned(),
            item: codex_protocol::items::TurnItem::CommandExecution(
                codex_protocol::items::CommandExecutionItem {
                    model_context: None,
                    id: call_id.to_owned(),
                    plugin_id: None,
                    script_path: None,
                    process_id: None,
                    command: vec!["cat".to_owned(), "README.md".to_owned()],
                    cwd: codex_utils_path_uri::PathUri::from_host_native_path(std::env::temp_dir())
                        .expect("temporary directory is a valid local path"),
                    parsed_cmd: Vec::new(),
                    source: codex_protocol::protocol::ExecCommandSource::Agent,
                    interaction_input: None,
                    status: codex_protocol::items::CommandExecutionStatus::InProgress,
                    stdout: None,
                    stderr: None,
                    aggregated_output: None,
                    exit_code: None,
                    duration: None,
                    formatted_output: None,
                },
            ),
            started_at_ms: 1,
        },
    ))
}

#[tokio::test]
async fn recorded_questions_share_queued_input_order_across_resume() {
    let (mut session, turn) = make_session_and_context().await;
    session.guardian_context_mode = GuardianContextMode::ThreadOwned;
    session.state.lock().await.history = ContextManager::with_guardian_context_mode(
        GuardianContextMode::ThreadOwned,
        &SessionSource::default(),
    );
    let question = |call_id: &str| {
        serde_json::from_value::<ResponseItem>(json!({
            "type": "function_call", "call_id": call_id,
            "namespace": "mcp__codex_apps", "name": "user_messaging_send_message",
            "arguments": "{\"text\":\"Continue?\"}"
        }))
        .unwrap()
    };
    session
        .record_conversation_items(&turn, turn.model_info(), &[question("first")])
        .await;
    let reply_order = session.reserve_user_input_order().await;
    session
        .record_conversation_items(&turn, turn.model_info(), &[question("second")])
        .await;
    // The accepted reply is recorded after a newer question, and the first send's
    // result arrives last. Neither delay should move the first question's position.
    session
        .record_annotated_conversation_items(
            &turn,
            turn.model_info(),
            vec![
                ResponseItemEnvelope {
                    item: user_message("Yes."),
                    metadata: Some(codex_history::CodexHarnessMetadata {
                        user_input_order: reply_order,
                        ..Default::default()
                    }),
                },
                ResponseItemEnvelope {
                    item: serde_json::from_value(json!({
                        "type": "function_call_output", "call_id": "first", "output": "Sent."
                    }))
                    .unwrap(),
                    metadata: Some(codex_history::CodexHarnessMetadata {
                        delivered_assistant_message: Some("Continue?".to_owned()),
                        ..Default::default()
                    }),
                },
            ],
        )
        .await;
    let saved = session
        .clone_history()
        .await
        .into_annotated_items()
        .into_iter()
        .map(RolloutItem::ResponseItem)
        .collect::<Vec<_>>();
    let saved = serde_json::from_value(serde_json::to_value(saved).unwrap()).unwrap();
    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: session.thread_id,
            history: Arc::new(saved),
            rollout_path: None,
        }))
        .await;
    let history = session.clone_history().await;
    assert_eq!(
        history
            .annotated_items()
            .iter()
            .map(|item| {
                item.metadata
                    .as_ref()
                    .and_then(|metadata| metadata.user_input_order)
            })
            .collect::<Vec<_>>(),
        vec![Some(0), Some(2), Some(1), None]
    );
    assert_eq!(session.reserve_user_input_order().await, Some(3));
}

#[tokio::test]
async fn sender_context_follows_its_delivery_through_checkpoint_and_rollback() {
    let (mut session, turn_context) = make_session_and_context().await;
    session.guardian_context_mode = GuardianContextMode::ThreadOwned;
    let mut live = ContextManager::with_guardian_context_mode(
        GuardianContextMode::ThreadOwned,
        &SessionSource::default(),
    );
    let mut items = Vec::new();
    let mut snapshots = Vec::new();
    for index in 0..2 {
        // A and then B are accepted before either is consumed.
        let acceptance_order = Some(live.reserve_input_order());
        let steer_order = Some(live.reserve_input_order());
        let snapshot = codex_history::SenderUserMessages {
            receiver_turn_id: format!("turn-{index}"),
            receiver_message_id: format!("delivery-{index}"),
            text: format!("Sender context {index}"),
        };
        let input = [
            ResponseItemEnvelope {
                item: serde_json::from_value::<ResponseItem>(json!({
                    "type": "function_call_output", "id": snapshot.receiver_message_id,
                    "name": "send_message_to_thread", "namespace": "codex_app", "output": "Inspect."
                }))
                .unwrap(),
                metadata: Some(codex_history::CodexHarnessMetadata {
                    user_input_order: acceptance_order,
                    sender_user_messages: Some(Box::new(snapshot.clone())),
                    ..Default::default()
                }),
            },
            ResponseItemEnvelope {
                item: user_message("Later user steer B."),
                metadata: Some(codex_history::CodexHarnessMetadata {
                    user_input_order: steer_order,
                    ..Default::default()
                }),
            },
        ];
        live.record_annotated_items(&input, turn_context.model_info().truncation_policy.into());
        items.extend(input.into_iter().map(RolloutItem::ResponseItem));
        snapshots.push(snapshot);
    }
    let mut checkpoint: CompactedItem =
        serde_json::from_value(json!({"message": "compacted"})).unwrap();
    checkpoint.replacement_history = Some(live.annotated_items().to_vec());
    checkpoint.retained_context = Some(live.retained_context().clone());
    let mut checkpointed = items.clone();
    checkpointed.push(RolloutItem::Compacted(checkpoint));
    for (num_turns, expected) in [(1, &snapshots[1]), (2, &snapshots[0])] {
        let mut rolled_back = live.clone();
        rolled_back.drop_last_n_user_turns(num_turns);
        assert_eq!(
            rolled_back.retained_context().sender_user_messages(),
            Some(expected)
        );
        for history in [items.clone(), checkpointed.clone()] {
            let mut history: Vec<RolloutItem> =
                serde_json::from_value(serde_json::to_value(history).unwrap()).unwrap();
            history.push(RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
                ThreadRolledBackEvent { num_turns },
            )));
            let replayed = session
                .reconstruct_history_from_rollout(&turn_context, &history)
                .await;
            assert_eq!(replayed.retained_context, *rolled_back.retained_context());
            assert_eq!(replayed.history, rolled_back.annotated_items());
        }
    }
}

macro_rules! object {
    ($value:tt) => {
        serde_json::from_value(json!($value)).unwrap()
    };
}

fn user_message(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn assistant_message(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn annotated(items: Vec<ResponseItem>) -> Vec<ResponseItemEnvelope> {
    items.into_iter().map(ResponseItemEnvelope::new).collect()
}

fn inter_agent_assistant_message(text: &str) -> ResponseItem {
    let communication = InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::root().join("worker").unwrap(),
        Vec::new(),
        text.to_string(),
        /*trigger_turn*/ true,
    );
    ResponseItem::Message {
        id: None,
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: serde_json::to_string(&communication).unwrap(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn completed_user_turn_rollout(
    turn_context_item: TurnContextItem,
    items: Vec<RolloutItem>,
) -> Vec<RolloutItem> {
    let turn_id = turn_context_item
        .turn_id
        .clone()
        .expect("turn context should have turn_id");
    let mut rollout_items = vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: turn_id.clone(),
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "seed".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::TurnContext(turn_context_item),
    ];
    rollout_items.extend(items);
    rollout_items.push(RolloutItem::EventMsg(EventMsg::TurnComplete(
        codex_protocol::protocol::TurnCompleteEvent {
            turn_id,
            last_agent_message: None,
            error: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
        },
    )));
    rollout_items
}

#[tokio::test]
async fn record_initial_history_reconstructs_typed_inter_agent_message() {
    let (session, _turn_context) = make_session_and_context().await;
    let communication = InterAgentCommunication::new(
        AgentPath::root().join("worker").expect("worker path"),
        AgentPath::root(),
        Vec::new(),
        "child done".to_string(),
        /*trigger_turn*/ false,
    );

    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: ThreadId::default(),
            history: Arc::new(vec![RolloutItem::InterAgentCommunication(
                communication.clone(),
            )]),
            rollout_path: Some(PathBuf::from("/tmp/resume.jsonl")),
        }))
        .await;

    assert_eq!(
        raw_history_items(&session.state.lock().await.clone_history()),
        vec![communication.to_model_input_item()]
    );
}

#[tokio::test]
async fn record_initial_history_ignores_security_risk_scores() {
    let (session, _turn_context) = make_session_and_context().await;
    let user_item = user_message("visible user input");
    let security_risk = SecurityRiskScore {
        scores: BTreeMap::from([("credential_access".to_string(), 0.92)]),
        call_id: None,
        action: None,
        sampled_at: None,
    };

    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: ThreadId::default(),
            history: Arc::new(vec![
                RolloutItem::ResponseItem(ResponseItemEnvelope::new(user_item.clone())),
                RolloutItem::SecurityRiskScore(security_risk),
            ]),
            rollout_path: Some(PathBuf::from("/tmp/resume.jsonl")),
        }))
        .await;

    assert_eq!(
        strip_metadata_from_items(&raw_history_items(
            &session.state.lock().await.clone_history()
        )),
        vec![user_item]
    );
}

#[derive(Clone, Copy)]
enum BaselineTurnInput {
    UserMessage,
    RemovedByFork,
}

#[test_case(BaselineTurnInput::UserMessage; "user turn")]
#[test_case(BaselineTurnInput::RemovedByFork; "fork removed the task message")]
#[tokio::test]
async fn record_initial_history_restores_world_state_baseline(input: BaselineTurnInput) {
    let (session, turn_context) = make_session_and_context().await;
    let turn_context = Arc::new(turn_context);
    let world_state = build_world_state_from_turn_context(&session, &turn_context).await;
    let expected_history = world_state
        .render_full()
        .into_iter()
        .map(ContextualUserFragment::into_boxed_response_item)
        .collect::<Vec<_>>();
    let mut world_state_items = expected_history
        .iter()
        .cloned()
        .map(ResponseItemEnvelope::new)
        .map(RolloutItem::ResponseItem)
        .collect::<Vec<_>>();
    world_state_items.push(RolloutItem::WorldState(WorldStateItem::full(
        world_state.snapshot().into_object(),
    )));
    let context_item = turn_context.to_turn_context_item();
    let rollout_items = match input {
        BaselineTurnInput::UserMessage => {
            completed_user_turn_rollout(context_item.clone(), world_state_items)
        }
        BaselineTurnInput::RemovedByFork => {
            world_state_items.push(RolloutItem::TurnContext(context_item.clone()));
            world_state_items
        }
    };
    // Exercise the persisted representation, not just an in-memory fork.
    let rollout_items =
        serde_json::from_value(serde_json::to_value(rollout_items).unwrap()).unwrap();

    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: ThreadId::default(),
            history: Arc::new(rollout_items),
            rollout_path: Some(PathBuf::from("/tmp/resume.jsonl")),
        }))
        .await;
    assert_eq!(
        (
            session.previous_turn_settings().await,
            serde_json::to_value(session.reference_context_item().await).unwrap(),
        ),
        (
            Some(PreviousTurnSettings {
                model: context_item.model.clone(),
                comp_hash: context_item.comp_hash.clone(),
                realtime_active: context_item.realtime_active,
            }),
            serde_json::to_value(Some(context_item)).unwrap(),
        )
    );
    let step_context = StepContext::for_test(Arc::clone(&turn_context));
    session
        .record_context_updates_and_set_reference_context_item(&step_context)
        .await
        .expect("world state should build");

    assert_eq!(
        raw_history_items(&session.clone_history().await),
        expected_history,
    );
}

#[tokio::test]
async fn record_initial_history_resumed_bare_turn_context_does_not_hydrate_previous_turn_settings()
{
    let (session, turn_context) = make_session_and_context().await;
    let previous_model = "previous-rollout-model";
    let previous_context_item = TurnContextItem {
        turn_id: Some(turn_context.sub_id.clone()),
        root_turn_id: None,
        disabled_plugin_ids: None,
        #[allow(deprecated)]
        cwd: turn_context.cwd.clone(),
        workspace_roots: None,
        current_date: turn_context.current_date.clone(),
        timezone: turn_context.timezone.clone(),
        approval_policy: turn_context.approval_policy(),
        approvals_reviewer: None,
        sandbox_policy: turn_context.sandbox_policy(),
        permission_profile: None,
        active_permission_profile: None,
        network: None,
        file_system_sandbox_policy: None,
        model: previous_model.to_string(),
        comp_hash: None,
        personality: turn_context.personality(),
        collaboration_mode: Some(turn_context.collaboration_mode()),
        multi_agent_version: None,
        multi_agent_mode: None,
        realtime_active: Some(turn_context.realtime_active),
        cyber_access_program: None,
        effort: turn_context.reasoning_effort().cloned(),
        summary: codex_protocol::config_types::ReasoningSummary::Auto,
    };
    let rollout_items = vec![RolloutItem::TurnContext(previous_context_item)];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;
    assert_eq!(reconstructed.world_state_baseline, None);

    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: ThreadId::default(),
            history: Arc::new(rollout_items),
            rollout_path: Some(PathBuf::from("/tmp/resume.jsonl")),
        }))
        .await;

    assert_eq!(session.previous_turn_settings().await, None);
    assert!(session.reference_context_item().await.is_none());
}

#[test]
fn legacy_blocked_tool_call_responses_require_one_exact_call_per_closed_turn() {
    let event_thread_id = ThreadId::default();
    let thread_id = event_thread_id.to_string();
    let current_turn_id = "01a1fcfa-b360-7ee3-bd06-ee7cc17a58b9";
    let valid = legacy_blocked_rollout();
    let recovered =
        super::rollout_reconstruction::legacy_blocked_tool_call_responses(&valid, &thread_id);
    assert_eq!(recovered.len(), 3);

    let recovered_ids = recovered
        .iter()
        .map(|envelope| match &envelope.item {
            ResponseItem::FunctionCallOutput {
                call_id: Some(call_id),
                output,
                ..
            } => {
                assert_eq!(output.success, Some(false));
                let FunctionCallOutputBody::Text(body) = &output.body else {
                    panic!("recovery output must retain the blocked-call proof body");
                };
                let proof: serde_json::Value =
                    serde_json::from_str(body).expect("recovery proof JSON");
                assert_eq!(proof["kind"], "blocked_before_handler");
                assert_eq!(proof["source_call_id"].as_str(), Some(call_id.as_str()));
                assert!(proof["source_payload_sha256"].as_str().is_some());
                assert!(proof["source_rollout_ordinal"].as_u64().is_some());
                assert!(
                    envelope
                        .metadata
                        .as_ref()
                        .and_then(|metadata| metadata.rencrow_blocked_tool_call.as_ref())
                        .is_some()
                );
                call_id.clone()
            }
            _ => panic!("expected a blocked function-call output"),
        })
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(recovered_ids.len(), 3);

    let mut ambiguous = valid.clone();
    let second_call = legacy_blocked_exec_call(
        "call_ambiguous_same_turn",
        "01a1fcfa-b360-7ee3-bd06-ee7cc17a58b9",
    );
    let terminal = ambiguous
        .iter()
        .position(|item| matches!(item, RolloutItem::EventMsg(EventMsg::TurnComplete(event)) if event.turn_id == "01a1fcfa-b360-7ee3-bd06-ee7cc17a58b9"))
        .expect("current-turn terminal");
    ambiguous.insert(terminal, second_call);
    let recovered =
        super::rollout_reconstruction::legacy_blocked_tool_call_responses(&ambiguous, &thread_id);
    assert_eq!(recovered.len(), 2);
    assert!(!recovered.iter().any(|envelope| matches!(
        &envelope.item,
        ResponseItem::FunctionCallOutput { call_id: Some(call_id), .. }
            if call_id == "call_4515721a156c404aa1978bb0"
    )));

    // An ordinary earlier refusal is not proof of the reserved pre-handler Fatal.
    // The final unique unmatched call remains eligible for compatibility recovery.
    let mut ordinary_refusal_compatibility = valid.clone();
    let current_call = ordinary_refusal_compatibility
        .iter()
        .position(|item| {
            matches!(
                item,
                RolloutItem::ResponseItem(envelope)
                    if matches!(&envelope.item,
                        ResponseItem::FunctionCall { call_id, .. }
                            if call_id == "call_4515721a156c404aa1978bb0")
            )
        })
        .expect("current call exists");
    let ordinary_call_id = "call_ordinary_refusal_same_turn";
    ordinary_refusal_compatibility.splice(
        current_call..current_call,
        [
            legacy_blocked_exec_call(ordinary_call_id, current_turn_id),
            RolloutItem::ResponseItem(ResponseItemEnvelope::new(
                ResponseItem::FunctionCallOutput {
                    id: None,
                    call_id: Some(ordinary_call_id.to_owned()),
                    name: Some("exec_command".to_owned()),
                    namespace: None,
                    output: FunctionCallOutputPayload::from_text(
                        "ordinary read refusal; command not executed".to_owned(),
                    ),
                    internal_chat_message_metadata_passthrough: None,
                },
            )),
        ],
    );
    let recovered = super::rollout_reconstruction::legacy_blocked_tool_call_responses(
        &ordinary_refusal_compatibility,
        &thread_id,
    );
    assert_eq!(recovered.len(), 3);
    assert!(recovered.iter().any(|envelope| matches!(
        &envelope.item,
        ResponseItem::FunctionCallOutput { call_id: Some(call_id), .. }
            if call_id == "call_4515721a156c404aa1978bb0"
    )));

    // An exact existing false Fatal output paired with call A does not prove that
    // a later unmatched call B was blocked before handler execution.
    let mut exact_fatal_already_paired = valid.clone();
    let fatal_message = exact_fatal_already_paired
        .iter()
        .find_map(|item| match item {
            RolloutItem::EventMsg(EventMsg::TurnComplete(event))
                if event.turn_id == current_turn_id =>
            {
                event.error.as_ref().map(|error| error.message.clone())
            }
            _ => None,
        })
        .expect("current turn has the reserved Fatal message");
    let candidate_call_id = "call_unmatched_after_exact_fatal";
    let current_terminal = exact_fatal_already_paired
        .iter()
        .position(|item| {
            matches!(
                item,
                RolloutItem::EventMsg(EventMsg::TurnComplete(event))
                    if event.turn_id == current_turn_id
            )
        })
        .expect("current-turn terminal");
    exact_fatal_already_paired.splice(
        current_terminal..current_terminal,
        [
            legacy_exact_blocked_fatal_output(
                "call_4515721a156c404aa1978bb0",
                &thread_id,
                &fatal_message,
            ),
            legacy_blocked_exec_call(candidate_call_id, current_turn_id),
        ],
    );
    let exact_fatal_already_paired: Vec<RolloutItem> = serde_json::from_value(
        serde_json::to_value(&exact_fatal_already_paired)
            .expect("serialize canonical fatal output"),
    )
    .expect("deserialize canonical fatal output");
    assert!(exact_fatal_already_paired.iter().any(|item| matches!(
        item,
        RolloutItem::ResponseItem(ResponseItemEnvelope {
            item: ResponseItem::FunctionCallOutput {
                call_id: Some(call_id), output, ..
            },
            ..
        }) if call_id == "call_4515721a156c404aa1978bb0" && output.success.is_none()
    )));
    let recovered = super::rollout_reconstruction::legacy_blocked_tool_call_responses(
        &exact_fatal_already_paired,
        &thread_id,
    );
    assert_eq!(recovered.len(), 2);
    assert!(!recovered.iter().any(|envelope| matches!(
        &envelope.item,
        ResponseItem::FunctionCallOutput { call_id: Some(call_id), .. }
            if call_id == candidate_call_id
    )));

    let explicit_success_fatal = exact_fatal_already_paired
        .iter()
        .cloned()
        .map(|mut item| {
            if let RolloutItem::ResponseItem(ResponseItemEnvelope {
                item:
                    ResponseItem::FunctionCallOutput {
                        call_id: Some(call_id),
                        output,
                        ..
                    },
                ..
            }) = &mut item
                && call_id == "call_4515721a156c404aa1978bb0"
            {
                output.success = Some(true);
            }
            item
        })
        .collect::<Vec<_>>();
    let recovered = super::rollout_reconstruction::legacy_blocked_tool_call_responses(
        &explicit_success_fatal,
        &thread_id,
    );
    assert_eq!(recovered.len(), 2);
    assert!(!recovered.iter().any(|envelope| matches!(
        &envelope.item,
        ResponseItem::FunctionCallOutput { call_id: Some(call_id), .. }
            if call_id == candidate_call_id
    )));

    // Duplicate call IDs invalidate the candidate before unmatched-call counting.
    let mut duplicate_call_id = valid.clone();
    let duplicate_terminal = duplicate_call_id
        .iter()
        .position(|item| {
            matches!(
                item,
                RolloutItem::EventMsg(EventMsg::TurnComplete(event))
                    if event.turn_id == current_turn_id
            )
        })
        .expect("current-turn terminal");
    duplicate_call_id.splice(
        duplicate_terminal..duplicate_terminal,
        [
            legacy_blocked_exec_call("call_4515721a156c404aa1978bb0", current_turn_id),
            legacy_blocked_exec_call("call_unique_after_duplicate", current_turn_id),
        ],
    );
    let recovered = super::rollout_reconstruction::legacy_blocked_tool_call_responses(
        &duplicate_call_id,
        &thread_id,
    );
    assert_eq!(recovered.len(), 2);
    assert!(!recovered.iter().any(|envelope| matches!(
        &envelope.item,
        ResponseItem::FunctionCallOutput { call_id: Some(call_id), .. }
            if call_id == "call_4515721a156c404aa1978bb0"
    )));

    // A matching ItemStarted proves execution began, so do not synthesize a
    // blocked-before-handler response for that candidate.
    let mut command_started = valid.clone();
    let started_call_id = "call_started_candidate";
    let started_terminal = command_started
        .iter()
        .position(|item| {
            matches!(
                item,
                RolloutItem::EventMsg(EventMsg::TurnComplete(event))
                    if event.turn_id == current_turn_id
            )
        })
        .expect("current-turn terminal");
    command_started.splice(
        started_terminal..started_terminal,
        [
            RolloutItem::ResponseItem(ResponseItemEnvelope::new(
                ResponseItem::FunctionCallOutput {
                    id: None,
                    call_id: Some("call_4515721a156c404aa1978bb0".to_owned()),
                    name: Some("exec_command".to_owned()),
                    namespace: None,
                    output: FunctionCallOutputPayload::from_text(
                        "ordinary prior refusal".to_owned(),
                    ),
                    internal_chat_message_metadata_passthrough: None,
                },
            )),
            legacy_blocked_exec_call(started_call_id, current_turn_id),
            legacy_command_started(started_call_id, current_turn_id, &event_thread_id),
        ],
    );
    let recovered = super::rollout_reconstruction::legacy_blocked_tool_call_responses(
        &command_started,
        &thread_id,
    );
    assert_eq!(recovered.len(), 2);
    assert!(!recovered.iter().any(|envelope| matches!(
        &envelope.item,
        ResponseItem::FunctionCallOutput { call_id: Some(call_id), .. }
            if call_id == started_call_id
    )));

    let no_exact_marker = valid
        .iter()
        .map(|item| match item {
            RolloutItem::EventMsg(EventMsg::TurnComplete(event))
                if event.turn_id == current_turn_id =>
            {
                let mut event = event.clone();
                event.error.as_mut().expect("fatal error").message =
                    "Fatal error: another reason; command not executed".to_owned();
                RolloutItem::EventMsg(EventMsg::TurnComplete(event))
            }
            _ => item.clone(),
        })
        .collect::<Vec<_>>();
    let recovered = super::rollout_reconstruction::legacy_blocked_tool_call_responses(
        &no_exact_marker,
        &thread_id,
    );
    assert_eq!(recovered.len(), 2);

    let unknown_fatal = valid
        .iter()
        .map(|item| match item {
            RolloutItem::EventMsg(EventMsg::TurnComplete(event))
                if event.turn_id == current_turn_id =>
            {
                let mut event = event.clone();
                event.error.as_mut().expect("fatal error").codex_error_info = None;
                RolloutItem::EventMsg(EventMsg::TurnComplete(event))
            }
            _ => item.clone(),
        })
        .collect::<Vec<_>>();
    let recovered = super::rollout_reconstruction::legacy_blocked_tool_call_responses(
        &unknown_fatal,
        &thread_id,
    );
    assert_eq!(recovered.len(), 2);

    let already_paired = valid
        .iter()
        .cloned()
        .chain(std::iter::once(RolloutItem::ResponseItem(
            ResponseItemEnvelope::new(ResponseItem::FunctionCallOutput {
                id: None,
                call_id: Some("call_4515721a156c404aa1978bb0".to_owned()),
                name: Some("exec_command".to_owned()),
                namespace: None,
                output: FunctionCallOutputPayload::from_text("blocked before handler".to_owned()),
                internal_chat_message_metadata_passthrough: None,
            }),
        )))
        .collect::<Vec<_>>();
    let recovered = super::rollout_reconstruction::legacy_blocked_tool_call_responses(
        &already_paired,
        &thread_id,
    );
    assert_eq!(recovered.len(), 2);

    let missing_terminal = valid
        .into_iter()
        .filter(|item| {
            !matches!(
                item,
                RolloutItem::EventMsg(EventMsg::TurnComplete(event))
                    if event.turn_id == current_turn_id
            )
        })
        .collect::<Vec<_>>();
    let recovered = super::rollout_reconstruction::legacy_blocked_tool_call_responses(
        &missing_terminal,
        &thread_id,
    );
    assert_eq!(recovered.len(), 2);
}

#[tokio::test]
async fn resumed_legacy_blocked_tool_calls_are_persisted_once() {
    let (mut session, turn_context) = make_session_and_context().await;
    let rollout_path = super::tests::attach_thread_persistence(&mut session).await;
    let source_rollout = legacy_blocked_rollout();
    for item in &source_rollout {
        match item {
            RolloutItem::ResponseItem(envelope) => {
                session
                    .record_annotated_conversation_items(
                        &turn_context,
                        turn_context.model_info().as_ref(),
                        vec![envelope.clone()],
                    )
                    .await;
            }
            RolloutItem::EventMsg(event) => {
                session.send_event(&turn_context, event.clone()).await;
            }
            _ => panic!("legacy fixture contains only calls and terminal events"),
        }
    }
    session
        .flush_rollout()
        .await
        .expect("source calls and terminal events are persisted");
    let InitialHistory::Resumed(source) =
        crate::rollout::recorder::RolloutRecorder::get_rollout_history(&rollout_path)
            .await
            .expect("read persisted source rollout")
    else {
        panic!("expected persisted source rollout");
    };
    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: source.conversation_id,
            history: source.history,
            rollout_path: Some(rollout_path.clone()),
        }))
        .await;
    session
        .flush_rollout()
        .await
        .expect("recovered false outputs are persisted");

    let InitialHistory::Resumed(persisted) =
        crate::rollout::recorder::RolloutRecorder::get_rollout_history(&rollout_path)
            .await
            .expect("read rollout after recovery")
    else {
        panic!("expected persisted recovered rollout");
    };
    let persisted_outputs = persisted
        .history
        .iter()
        .filter(|item| {
            matches!(
                item,
                RolloutItem::ResponseItem(ResponseItemEnvelope {
                    item: ResponseItem::FunctionCallOutput {
                        call_id: Some(call_id), ..
                    },
                    ..
                }) if call_id.starts_with("call_")
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(persisted_outputs.len(), 3);
    for item in persisted_outputs {
        let RolloutItem::ResponseItem(envelope) = item else {
            unreachable!();
        };
        assert!(
            envelope
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.rencrow_blocked_tool_call.as_ref())
                .is_some()
        );
    }

    // Reopening the actual persisted rollout must not append another refusal.
    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: persisted.conversation_id,
            history: persisted.history,
            rollout_path: Some(rollout_path.clone()),
        }))
        .await;
    session
        .flush_rollout()
        .await
        .expect("idempotent resume leaves persisted output count stable");
    let InitialHistory::Resumed(after_second_resume) =
        crate::rollout::recorder::RolloutRecorder::get_rollout_history(&rollout_path)
            .await
            .expect("reread rollout after idempotent resume")
    else {
        panic!("expected persisted rollout after second resume");
    };
    let output_count = after_second_resume
        .history
        .iter()
        .filter(|envelope| {
            matches!(
                envelope,
                RolloutItem::ResponseItem(ResponseItemEnvelope {
                    item: ResponseItem::FunctionCallOutput {
                        call_id: Some(call_id), ..
                    },
                    ..
                }) if call_id.starts_with("call_")
            )
        })
        .count();
    assert_eq!(output_count, 3);
}

#[tokio::test]
async fn record_initial_history_resumed_hydrates_previous_turn_settings_from_lifecycle_turn_with_missing_turn_context_id()
 {
    let (session, turn_context) = make_session_and_context().await;
    let previous_model = "previous-rollout-model";
    let mut previous_context_item = TurnContextItem {
        turn_id: Some(turn_context.sub_id.clone()),
        root_turn_id: None,
        disabled_plugin_ids: None,
        #[allow(deprecated)]
        cwd: turn_context.cwd.clone(),
        workspace_roots: None,
        current_date: turn_context.current_date.clone(),
        timezone: turn_context.timezone.clone(),
        approval_policy: turn_context.approval_policy(),
        approvals_reviewer: None,
        sandbox_policy: turn_context.sandbox_policy(),
        permission_profile: None,
        active_permission_profile: None,
        network: None,
        file_system_sandbox_policy: None,
        model: previous_model.to_string(),
        comp_hash: Some("comp-hash-a".to_string()),
        personality: turn_context.personality(),
        collaboration_mode: Some(turn_context.collaboration_mode()),
        multi_agent_version: None,
        multi_agent_mode: None,
        realtime_active: Some(turn_context.realtime_active),
        cyber_access_program: None,
        effort: turn_context.reasoning_effort().cloned(),
        summary: codex_protocol::config_types::ReasoningSummary::Auto,
    };
    let turn_id = previous_context_item
        .turn_id
        .clone()
        .expect("turn context should have turn_id");
    previous_context_item.turn_id = None;

    let rollout_items = vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: turn_id.clone(),
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "seed".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::TurnContext(previous_context_item),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id,
                last_agent_message: None,
                error: None,
                started_at: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
    ];

    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: ThreadId::default(),
            history: Arc::new(rollout_items),
            rollout_path: Some(PathBuf::from("/tmp/resume.jsonl")),
        }))
        .await;

    assert_eq!(
        session.previous_turn_settings().await,
        Some(PreviousTurnSettings {
            model: previous_model.to_string(),
            comp_hash: Some("comp-hash-a".to_string()),
            realtime_active: Some(turn_context.realtime_active),
        })
    );
}

#[tokio::test]
async fn reconstruct_history_rollback_keeps_history_and_metadata_in_sync_for_completed_turns() {
    let (session, turn_context) = make_session_and_context().await;
    let first_context_item = turn_context.to_turn_context_item();
    let first_turn_id = first_context_item
        .turn_id
        .clone()
        .expect("turn context should have turn_id");
    let mut rolled_back_context_item = first_context_item.clone();
    rolled_back_context_item.turn_id = Some("rolled-back-turn".to_string());
    rolled_back_context_item.model = "rolled-back-model".to_string();
    let rolled_back_turn_id = rolled_back_context_item
        .turn_id
        .clone()
        .expect("turn context should have turn_id");
    let turn_one_user = user_message("turn 1 user");
    let turn_one_assistant = assistant_message("turn 1 assistant");
    let turn_two_user = user_message("turn 2 user");
    let turn_two_assistant = assistant_message("turn 2 assistant");

    let rollout_items = vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: first_turn_id.clone(),
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "turn 1 user".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::TurnContext(first_context_item.clone()),
        RolloutItem::WorldState(WorldStateItem::full(object!({
            "test": {"environment": "first"}
        }))),
        RolloutItem::ResponseItem(turn_one_user.clone().into()),
        RolloutItem::ResponseItem(turn_one_assistant.clone().into()),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: first_turn_id,
                started_at: None,
                last_agent_message: None,
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: rolled_back_turn_id.clone(),
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "turn 2 user".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::TurnContext(rolled_back_context_item),
        RolloutItem::WorldState(WorldStateItem::patch(object!({
            "test": {"environment": "rolled-back"}
        }))),
        RolloutItem::ResponseItem(turn_two_user.into()),
        RolloutItem::ResponseItem(turn_two_assistant.into()),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: rolled_back_turn_id,
                started_at: None,
                last_agent_message: None,
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
            codex_protocol::protocol::ThreadRolledBackEvent { num_turns: 1 },
        )),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert_eq!(
        reconstructed.history,
        annotated(vec![turn_one_user, turn_one_assistant])
    );
    assert_eq!(
        reconstructed.previous_turn_settings,
        Some(PreviousTurnSettings {
            model: turn_context.model_info().slug.clone(),
            comp_hash: None,
            realtime_active: Some(turn_context.realtime_active),
        })
    );
    assert_eq!(
        serde_json::to_value(reconstructed.reference_context_item)
            .expect("serialize reconstructed reference context item"),
        serde_json::to_value(Some(first_context_item))
            .expect("serialize expected reference context item")
    );
    assert_eq!(
        serde_json::to_value(reconstructed.world_state_baseline)
            .expect("serialize reconstructed world state"),
        json!({"test": {"environment": "first"}})
    );
}

#[tokio::test]
async fn reconstruct_history_rollback_keeps_history_and_metadata_in_sync_for_incomplete_turn() {
    let (session, turn_context) = make_session_and_context().await;
    let first_context_item = turn_context.to_turn_context_item();
    let first_turn_id = first_context_item
        .turn_id
        .clone()
        .expect("turn context should have turn_id");
    let incomplete_turn_id = "incomplete-rolled-back-turn".to_string();
    let turn_one_user = user_message("turn 1 user");
    let turn_one_assistant = assistant_message("turn 1 assistant");
    let turn_two_user = user_message("turn 2 user");

    let rollout_items = vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: first_turn_id.clone(),
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "turn 1 user".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::TurnContext(first_context_item.clone()),
        RolloutItem::ResponseItem(turn_one_user.clone().into()),
        RolloutItem::ResponseItem(turn_one_assistant.clone().into()),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: first_turn_id,
                started_at: None,
                last_agent_message: None,
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: incomplete_turn_id,
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "turn 2 user".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::ResponseItem(turn_two_user.into()),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
            codex_protocol::protocol::ThreadRolledBackEvent { num_turns: 1 },
        )),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert_eq!(
        reconstructed.history,
        annotated(vec![turn_one_user, turn_one_assistant])
    );
    assert_eq!(
        reconstructed.previous_turn_settings,
        Some(PreviousTurnSettings {
            model: turn_context.model_info().slug.clone(),
            comp_hash: None,
            realtime_active: Some(turn_context.realtime_active),
        })
    );
    assert_eq!(
        serde_json::to_value(reconstructed.reference_context_item)
            .expect("serialize reconstructed reference context item"),
        serde_json::to_value(Some(first_context_item))
            .expect("serialize expected reference context item")
    );
}

#[tokio::test]
async fn reconstruct_history_rollback_skips_non_user_turns_for_history_and_metadata() {
    let (session, turn_context) = make_session_and_context().await;
    let first_context_item = turn_context.to_turn_context_item();
    let first_turn_id = first_context_item
        .turn_id
        .clone()
        .expect("turn context should have turn_id");
    let second_turn_id = "rolled-back-user-turn".to_string();
    let standalone_turn_id = "standalone-turn".to_string();
    let turn_one_user = user_message("turn 1 user");
    let turn_one_assistant = assistant_message("turn 1 assistant");
    let turn_two_user = user_message("turn 2 user");
    let turn_two_assistant = assistant_message("turn 2 assistant");
    let standalone_assistant = assistant_message("standalone assistant");

    let rollout_items = vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: first_turn_id.clone(),
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "turn 1 user".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::TurnContext(first_context_item.clone()),
        RolloutItem::ResponseItem(turn_one_user.clone().into()),
        RolloutItem::ResponseItem(turn_one_assistant.clone().into()),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: first_turn_id,
                started_at: None,
                last_agent_message: None,
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: second_turn_id.clone(),
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "turn 2 user".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::ResponseItem(turn_two_user.into()),
        RolloutItem::ResponseItem(turn_two_assistant.into()),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: second_turn_id,
                started_at: None,
                last_agent_message: None,
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: standalone_turn_id.clone(),
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
        RolloutItem::ResponseItem(standalone_assistant.into()),
        RolloutItem::WorldState(WorldStateItem::full(object!({}))),
        RolloutItem::TurnContext(TurnContextItem {
            turn_id: Some(standalone_turn_id.clone()),
            ..first_context_item.clone()
        }),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: standalone_turn_id,
                started_at: None,
                last_agent_message: None,
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
            codex_protocol::protocol::ThreadRolledBackEvent { num_turns: 1 },
        )),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert_eq!(
        reconstructed.history,
        annotated(vec![turn_one_user, turn_one_assistant])
    );
    assert_eq!(
        reconstructed.previous_turn_settings,
        Some(PreviousTurnSettings {
            model: turn_context.model_info().slug.clone(),
            comp_hash: None,
            realtime_active: Some(turn_context.realtime_active),
        })
    );
    assert_eq!(
        serde_json::to_value(reconstructed.reference_context_item)
            .expect("serialize reconstructed reference context item"),
        serde_json::to_value(Some(first_context_item))
            .expect("serialize expected reference context item")
    );
}

#[tokio::test]
async fn reconstruct_history_rollback_counts_inter_agent_assistant_turns() {
    let (session, turn_context) = make_session_and_context().await;
    let first_context_item = turn_context.to_turn_context_item();
    let first_turn_id = first_context_item
        .turn_id
        .clone()
        .expect("turn context should have turn_id");
    let assistant_turn_id = "assistant-instruction-turn".to_string();
    let assistant_turn_context = TurnContextItem {
        turn_id: Some(assistant_turn_id.clone()),
        ..first_context_item.clone()
    };
    let assistant_instruction = inter_agent_assistant_message("continue");
    let assistant_reply = assistant_message("worker reply");

    let rollout_items = vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: first_turn_id.clone(),
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "turn 1 user".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::TurnContext(first_context_item.clone()),
        RolloutItem::ResponseItem(user_message("turn 1 user").into()),
        RolloutItem::ResponseItem(assistant_message("turn 1 assistant").into()),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: first_turn_id,
                started_at: None,
                last_agent_message: None,
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: assistant_turn_id.clone(),
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
        RolloutItem::TurnContext(assistant_turn_context),
        RolloutItem::ResponseItem(assistant_instruction.into()),
        RolloutItem::ResponseItem(assistant_reply.into()),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: assistant_turn_id,
                started_at: None,
                last_agent_message: None,
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
            codex_protocol::protocol::ThreadRolledBackEvent { num_turns: 1 },
        )),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert_eq!(
        reconstructed.history,
        annotated(vec![
            user_message("turn 1 user"),
            assistant_message("turn 1 assistant")
        ])
    );
    assert_eq!(
        reconstructed.previous_turn_settings,
        Some(PreviousTurnSettings {
            model: turn_context.model_info().slug.clone(),
            comp_hash: None,
            realtime_active: Some(turn_context.realtime_active),
        })
    );
    assert_eq!(
        serde_json::to_value(reconstructed.reference_context_item)
            .expect("serialize reconstructed reference context item"),
        serde_json::to_value(Some(first_context_item))
            .expect("serialize expected reference context item")
    );
}

#[tokio::test]
async fn reconstruct_history_rollback_clears_history_and_metadata_when_exceeding_user_turns() {
    let (session, turn_context) = make_session_and_context().await;
    let only_context_item = turn_context.to_turn_context_item();
    let only_turn_id = only_context_item
        .turn_id
        .clone()
        .expect("turn context should have turn_id");
    let rollout_items = vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: only_turn_id.clone(),
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "only user".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::TurnContext(only_context_item),
        RolloutItem::ResponseItem(user_message("only user").into()),
        RolloutItem::ResponseItem(assistant_message("only assistant").into()),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: only_turn_id,
                started_at: None,
                last_agent_message: None,
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
            codex_protocol::protocol::ThreadRolledBackEvent { num_turns: 99 },
        )),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert_eq!(reconstructed.history, Vec::new());
    assert_eq!(reconstructed.previous_turn_settings, None);
    assert!(reconstructed.reference_context_item.is_none());
}

#[tokio::test]
async fn record_initial_history_resumed_rollback_skips_only_user_turns() {
    let (session, turn_context) = make_session_and_context().await;
    let previous_context_item = turn_context.to_turn_context_item();
    let user_turn_id = previous_context_item
        .turn_id
        .clone()
        .expect("turn context should have turn_id");
    let standalone_turn_id = "standalone-task-turn".to_string();
    let rollout_items = vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: user_turn_id.clone(),
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "seed".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::TurnContext(previous_context_item),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: user_turn_id,
                started_at: None,
                last_agent_message: None,
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
        // Standalone task turn (no UserMessage) should not consume rollback skips.
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: standalone_turn_id.clone(),
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: standalone_turn_id,
                started_at: None,
                last_agent_message: None,
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
            codex_protocol::protocol::ThreadRolledBackEvent { num_turns: 1 },
        )),
    ];

    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: ThreadId::default(),
            history: Arc::new(rollout_items),
            rollout_path: Some(PathBuf::from("/tmp/resume.jsonl")),
        }))
        .await;

    assert_eq!(session.previous_turn_settings().await, None);
    assert!(session.reference_context_item().await.is_none());
}

#[tokio::test]
async fn record_initial_history_resumed_rollback_drops_incomplete_user_turn_compaction_metadata() {
    let (session, turn_context) = make_session_and_context().await;
    let previous_context_item = turn_context.to_turn_context_item();
    let previous_turn_id = previous_context_item
        .turn_id
        .clone()
        .expect("turn context should have turn_id");
    let incomplete_turn_id = "incomplete-compacted-user-turn".to_string();

    let rollout_items = vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: previous_turn_id.clone(),
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "seed".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::TurnContext(previous_context_item.clone()),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: previous_turn_id,
                started_at: None,
                last_agent_message: None,
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: incomplete_turn_id,
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "rolled back".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::Compacted(CompactedItem {
            message: String::new(),
            replacement_history: Some(Vec::new()),
            retained_context: None,
            guardian_history: None,
            mcp_resource_origins: None,
            window_number: None,
            first_window_id: None,
            previous_window_id: None,
            window_id: None,
            compaction_response_id: None,
            latest_token_usage_record: None,
        }),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
            codex_protocol::protocol::ThreadRolledBackEvent { num_turns: 1 },
        )),
    ];

    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: ThreadId::default(),
            history: Arc::new(rollout_items),
            rollout_path: Some(PathBuf::from("/tmp/resume.jsonl")),
        }))
        .await;

    assert_eq!(
        session.previous_turn_settings().await,
        Some(PreviousTurnSettings {
            model: turn_context.model_info().slug.clone(),
            comp_hash: None,
            realtime_active: Some(turn_context.realtime_active),
        })
    );
    assert_eq!(
        serde_json::to_value(session.reference_context_item().await)
            .expect("serialize seeded reference context item"),
        serde_json::to_value(Some(previous_context_item))
            .expect("serialize expected reference context item")
    );
}

#[derive(Clone, Copy)]
enum MissingContextBaseline {
    BareTurnContext,
    WorldStatePatch,
    CompactedSnapshot,
}

#[test_case(MissingContextBaseline::BareTurnContext; "bare turn context")]
#[test_case(MissingContextBaseline::WorldStatePatch; "patch without a full snapshot")]
#[test_case(MissingContextBaseline::CompactedSnapshot; "full snapshot before compaction")]
#[tokio::test]
async fn record_initial_history_requires_surviving_full_snapshot_without_user_turn(
    baseline: MissingContextBaseline,
) {
    let (session, turn_context) = make_session_and_context().await;
    let mut rollout_items = match baseline {
        MissingContextBaseline::BareTurnContext => Vec::new(),
        MissingContextBaseline::WorldStatePatch => {
            vec![RolloutItem::WorldState(WorldStateItem::patch(object!({})))]
        }
        MissingContextBaseline::CompactedSnapshot => vec![
            RolloutItem::WorldState(WorldStateItem::full(object!({}))),
            RolloutItem::Compacted(CompactedItem {
                message: String::new(),
                replacement_history: Some(Vec::new()),
                retained_context: None,
                guardian_history: None,
                mcp_resource_origins: None,
                window_number: None,
                first_window_id: None,
                previous_window_id: None,
                window_id: None,
                compaction_response_id: None,
                latest_token_usage_record: None,
            }),
        ],
    };
    rollout_items.push(RolloutItem::TurnContext(
        turn_context.to_turn_context_item(),
    ));

    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: ThreadId::default(),
            history: Arc::new(rollout_items),
            rollout_path: Some(PathBuf::from("/tmp/resume.jsonl")),
        }))
        .await;

    assert!(session.reference_context_item().await.is_none());
}

#[tokio::test]
async fn record_initial_history_resumed_does_not_seed_reference_context_item_after_compaction() {
    let (session, turn_context) = make_session_and_context().await;
    let previous_context_item = turn_context.to_turn_context_item();
    let rollout_items = vec![
        RolloutItem::TurnContext(previous_context_item),
        RolloutItem::Compacted(CompactedItem {
            message: String::new(),
            replacement_history: Some(Vec::new()),
            retained_context: None,
            guardian_history: None,
            mcp_resource_origins: None,
            window_number: None,
            first_window_id: None,
            previous_window_id: None,
            window_id: None,
            compaction_response_id: None,
            latest_token_usage_record: None,
        }),
    ];

    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: ThreadId::default(),
            history: Arc::new(rollout_items),
            rollout_path: Some(PathBuf::from("/tmp/resume.jsonl")),
        }))
        .await;

    assert_eq!(session.previous_turn_settings().await, None);
    assert!(session.reference_context_item().await.is_none());
}

#[tokio::test]
async fn reconstruct_history_restores_initial_window_from_session_meta() {
    let (session, turn_context) = make_session_and_context().await;
    let thread_id = ThreadId::default();
    let initial_window_id = Uuid::now_v7();
    let rollout_items = vec![RolloutItem::SessionMeta(SessionMetaLine {
        meta: SessionMeta {
            session_id: thread_id.into(),
            id: thread_id,
            context_window: Some(SessionContextWindow {
                window_id: initial_window_id.to_string(),
            }),
            ..SessionMeta::default()
        },
        git: None,
    })];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert_eq!(reconstructed.window_number, 0);
    assert_eq!(reconstructed.first_window_id, Some(initial_window_id));
    assert_eq!(reconstructed.previous_window_id, None);
    assert_eq!(reconstructed.window_id, Some(initial_window_id));
}

#[tokio::test]
async fn reconstruct_history_prefers_compacted_window_over_session_meta() {
    let (session, turn_context) = make_session_and_context().await;
    let thread_id = ThreadId::default();
    let initial_window_id = Uuid::now_v7();
    let compacted_first_window_id = Uuid::now_v7();
    let compacted_previous_window_id = Uuid::now_v7();
    let compacted_window_id = Uuid::now_v7();
    let rollout_items = vec![
        RolloutItem::SessionMeta(SessionMetaLine {
            meta: SessionMeta {
                session_id: thread_id.into(),
                id: thread_id,
                context_window: Some(SessionContextWindow {
                    window_id: initial_window_id.to_string(),
                }),
                ..SessionMeta::default()
            },
            git: None,
        }),
        RolloutItem::Compacted(CompactedItem {
            message: String::new(),
            replacement_history: Some(Vec::new()),
            retained_context: None,
            guardian_history: None,
            mcp_resource_origins: None,
            window_number: Some(2),
            first_window_id: Some(compacted_first_window_id.to_string()),
            previous_window_id: Some(compacted_previous_window_id.to_string()),
            window_id: Some(compacted_window_id.to_string()),
            compaction_response_id: None,
            latest_token_usage_record: None,
        }),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert_eq!(reconstructed.window_number, 2);
    assert_eq!(
        reconstructed.first_window_id,
        Some(compacted_first_window_id)
    );
    assert_eq!(
        reconstructed.previous_window_id,
        Some(compacted_previous_window_id)
    );
    assert_eq!(reconstructed.window_id, Some(compacted_window_id));
}

#[tokio::test]
async fn reconstruct_history_replays_world_state_from_latest_compaction_window() {
    let (session, turn_context) = make_session_and_context().await;
    let rollout_items = completed_user_turn_rollout(
        turn_context.to_turn_context_item(),
        vec![
            RolloutItem::WorldState(WorldStateItem::full(object!({
                "environment": {"status": "old"}
            }))),
            RolloutItem::Compacted(CompactedItem {
                message: String::new(),
                replacement_history: Some(Vec::new()),
                retained_context: None,
                guardian_history: None,
                mcp_resource_origins: None,
                window_number: Some(1),
                first_window_id: None,
                previous_window_id: None,
                window_id: None,
                compaction_response_id: None,
                latest_token_usage_record: None,
            }),
            RolloutItem::WorldState(WorldStateItem::full(object!({
                "environment": {"status": "starting", "cwd": "/workspace"}
            }))),
            RolloutItem::WorldState(WorldStateItem::patch(object!({
                "environment": {"status": "ready"}
            }))),
        ],
    );

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert_eq!(
        serde_json::to_value(reconstructed.world_state_baseline)
            .expect("serialize reconstructed world state"),
        json!({
            "environment": {"status": "ready", "cwd": "/workspace"}
        })
    );
}

#[tokio::test]
async fn bounded_replay_matches_full_replay_after_empty_turn_compactions() {
    let (session, turn_context) = make_session_and_context().await;
    let session_meta = SessionMetaLine {
        meta: SessionMeta {
            history_mode: ThreadHistoryMode::Paginated,
            ..Default::default()
        },
        git: None,
    };
    let initial_context = turn_context.to_turn_context_item();
    let mut rollout_items = vec![RolloutItem::SessionMeta(session_meta.clone())];
    rollout_items.extend(completed_user_turn_rollout(
        initial_context.clone(),
        vec![RolloutItem::ResponseItem(
            user_message("original task").into(),
        )],
    ));
    let window_ids = [Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7()];
    let mut latest_wake_start = 0;
    for window_number in 1..=2 {
        let mut context = initial_context.clone();
        context.turn_id = Some(format!("wake-{window_number}"));
        context.model = format!("model-{window_number}");
        context.comp_hash = Some(format!("hash-{window_number}"));
        context.realtime_active = Some(window_number == 2);
        let mut wake_items = completed_user_turn_rollout(
            context.clone(),
            vec![
                RolloutItem::Compacted(CompactedItem {
                    message: String::new(),
                    replacement_history: Some(annotated(vec![assistant_message(&format!(
                        "summary-{window_number}"
                    ))])),
                    retained_context: None,
                    guardian_history: Some(codex_history::GuardianHistoryCheckpoint(vec![
                        user_message("original task"),
                    ])),
                    mcp_resource_origins: None,
                    window_number: Some(window_number as u64),
                    first_window_id: Some(window_ids[0].to_string()),
                    previous_window_id: Some(window_ids[window_number - 1].to_string()),
                    window_id: Some(window_ids[window_number].to_string()),
                    compaction_response_id: None,
                    latest_token_usage_record: None,
                }),
                RolloutItem::WorldState(WorldStateItem::full(object!({
                    "environment": {"window": window_number, "status": "starting"}
                }))),
                RolloutItem::TurnContext(context),
                RolloutItem::ResponseItem(assistant_message("continued working").into()),
                RolloutItem::WorldState(WorldStateItem::patch(object!({
                    "environment": {"status": "ready"}
                }))),
            ],
        );
        // Clock wakes have no user-message boundary, and the next wait interrupts the turn.
        wake_items.retain(|item| {
            !matches!(
                item,
                RolloutItem::EventMsg(EventMsg::UserMessage(_) | EventMsg::TurnComplete(_))
            )
        });
        wake_items.push(RolloutItem::EventMsg(EventMsg::TurnAborted(
            codex_protocol::protocol::TurnAbortedEvent {
                turn_id: Some(format!("wake-{window_number}")),
                reason: TurnAbortReason::Interrupted,
                started_at: None,
                completed_at: None,
                duration_ms: None,
            },
        )));
        latest_wake_start = rollout_items.len();
        rollout_items.extend(wake_items);
    }

    let mut scan = ModelContextScan::default();
    let cutoff = rollout_items
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, item)| {
            matches!(scan.push(item.clone()), ModelContextScanProgress::Complete).then_some(index)
        });
    assert_eq!(cutoff, Some(latest_wake_start));
    let bounded_items = scan.finish(session_meta);
    let full = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;
    let bounded = session
        .reconstruct_history_from_rollout(&turn_context, &bounded_items)
        .await;
    assert_eq!(
        bounded.guardian_history.as_ref(),
        Some(&codex_history::GuardianHistoryCheckpoint(vec![
            user_message("original task"),
            assistant_message("continued working"),
        ])),
    );
    assert_eq!(
        (
            bounded.guardian_history,
            bounded.history,
            bounded.previous_turn_settings,
            bounded.reference_context_item,
            bounded.world_state_baseline,
            bounded.window_number,
            bounded.first_window_id,
            bounded.previous_window_id,
            bounded.window_id,
        ),
        (
            full.guardian_history,
            full.history,
            full.previous_turn_settings,
            full.reference_context_item,
            full.world_state_baseline,
            full.window_number,
            full.first_window_id,
            full.previous_window_id,
            full.window_id,
        ),
    );
}

#[tokio::test]
async fn reconstruct_history_preserves_legacy_compaction_count_with_session_meta_window() {
    let (session, turn_context) = make_session_and_context().await;
    let thread_id = ThreadId::default();
    let initial_window_id = Uuid::now_v7();
    let rollout_items = vec![
        RolloutItem::SessionMeta(SessionMetaLine {
            meta: SessionMeta {
                session_id: thread_id.into(),
                id: thread_id,
                context_window: Some(SessionContextWindow {
                    window_id: initial_window_id.to_string(),
                }),
                ..SessionMeta::default()
            },
            git: None,
        }),
        RolloutItem::Compacted(CompactedItem {
            message: "legacy summary".to_string(),
            replacement_history: None,
            retained_context: None,
            guardian_history: None,
            mcp_resource_origins: None,
            window_number: None,
            first_window_id: None,
            previous_window_id: None,
            window_id: None,
            compaction_response_id: None,
            latest_token_usage_record: None,
        }),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert_eq!(reconstructed.window_number, 1);
    assert_eq!(reconstructed.first_window_id, None);
    assert_eq!(reconstructed.previous_window_id, None);
    assert_eq!(reconstructed.window_id, None);
}

#[tokio::test]
async fn reconstruct_history_legacy_compaction_without_replacement_history_does_not_inject_current_initial_context()
 {
    let (session, turn_context) = make_session_and_context().await;
    let answer = codex_history::RetainedContextEvent::VerifiedAnswer {
        answer: codex_history::VerifiedAnswer {
            turn_id: "legacy-turn".to_owned(),
            call_id: "ask-1".to_owned(),
            questions: vec![codex_history::VerifiedQuestionAnswer {
                question: "Upload?".to_owned(),
                answer: "Only privately.".to_owned(),
            }],
        },
        acceptance_order: None,
    };
    let mut retained = codex_history::RetainedContext::default();
    retained.mark_user_messages_incomplete();
    retained.record(&answer);
    let rollout_items = vec![
        RolloutItem::ResponseItem(user_message("before compact").into()),
        RolloutItem::ResponseItem(assistant_message("assistant reply").into()),
        RolloutItem::RetainedContext(answer),
        RolloutItem::Compacted(CompactedItem {
            message: "legacy summary".to_string(),
            replacement_history: None,
            retained_context: None,
            guardian_history: None,
            mcp_resource_origins: None,
            window_number: None,
            first_window_id: None,
            previous_window_id: None,
            window_id: None,
            compaction_response_id: None,
            latest_token_usage_record: None,
        }),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert_eq!(
        reconstructed.history,
        annotated(vec![
            user_message("before compact"),
            ContextualUserFragment::into(CompactionSummary::new("legacy summary")),
        ])
    );
    assert!(reconstructed.reference_context_item.is_none());
    assert_eq!(reconstructed.retained_context, retained);
}

#[tokio::test]
async fn reconstruct_history_legacy_compaction_without_replacement_history_clears_later_reference_context_item()
 {
    let (session, turn_context) = make_session_and_context().await;
    let current_context_item = turn_context.to_turn_context_item();
    let current_turn_id = current_context_item
        .turn_id
        .clone()
        .expect("turn context should have turn_id");
    let rollout_items = vec![
        RolloutItem::ResponseItem(user_message("before compact").into()),
        RolloutItem::Compacted(CompactedItem {
            message: "legacy summary".to_string(),
            replacement_history: None,
            retained_context: None,
            guardian_history: None,
            mcp_resource_origins: None,
            window_number: None,
            first_window_id: None,
            previous_window_id: None,
            window_id: None,
            compaction_response_id: None,
            latest_token_usage_record: None,
        }),
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: current_turn_id.clone(),
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "after legacy compact".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::TurnContext(current_context_item),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: current_turn_id,
                started_at: None,
                last_agent_message: None,
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert!(reconstructed.reference_context_item.is_none());
}

#[tokio::test]
async fn record_initial_history_resumed_turn_context_after_compaction_reestablishes_reference_context_item()
 {
    let (session, turn_context) = make_session_and_context().await;
    let previous_model = "previous-rollout-model";
    let previous_context_item = TurnContextItem {
        turn_id: Some(turn_context.sub_id.clone()),
        root_turn_id: Some("root-turn".to_string()),
        disabled_plugin_ids: None,
        #[allow(deprecated)]
        cwd: turn_context.cwd.clone(),
        workspace_roots: None,
        current_date: turn_context.current_date.clone(),
        timezone: turn_context.timezone.clone(),
        approval_policy: turn_context.approval_policy(),
        approvals_reviewer: None,
        sandbox_policy: turn_context.sandbox_policy(),
        permission_profile: None,
        active_permission_profile: None,
        network: None,
        file_system_sandbox_policy: None,
        model: previous_model.to_string(),
        comp_hash: None,
        personality: turn_context.personality(),
        collaboration_mode: Some(turn_context.collaboration_mode()),
        multi_agent_version: None,
        multi_agent_mode: None,
        realtime_active: Some(turn_context.realtime_active),
        cyber_access_program: None,
        effort: turn_context.reasoning_effort().cloned(),
        summary: codex_protocol::config_types::ReasoningSummary::Auto,
    };
    let previous_turn_id = previous_context_item
        .turn_id
        .clone()
        .expect("turn context should have turn_id");
    let rollout_items = vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: previous_turn_id.clone(),
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "seed".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        // Compaction clears baseline until a later TurnContextItem re-establishes it.
        RolloutItem::Compacted(CompactedItem {
            message: String::new(),
            replacement_history: Some(Vec::new()),
            retained_context: None,
            guardian_history: None,
            mcp_resource_origins: None,
            window_number: None,
            first_window_id: None,
            previous_window_id: None,
            window_id: None,
            compaction_response_id: None,
            latest_token_usage_record: None,
        }),
        RolloutItem::TurnContext(previous_context_item),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: previous_turn_id,
                started_at: None,
                last_agent_message: None,
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
    ];

    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: ThreadId::default(),
            history: Arc::new(rollout_items),
            rollout_path: Some(PathBuf::from("/tmp/resume.jsonl")),
        }))
        .await;

    assert_eq!(
        session.previous_turn_settings().await,
        Some(PreviousTurnSettings {
            model: previous_model.to_string(),
            comp_hash: None,
            realtime_active: Some(turn_context.realtime_active),
        })
    );
    assert_eq!(
        serde_json::to_value(session.reference_context_item().await)
            .expect("serialize seeded reference context item"),
        serde_json::to_value(Some(TurnContextItem {
            turn_id: Some(turn_context.sub_id.clone()),
            root_turn_id: Some("root-turn".to_string()),
            disabled_plugin_ids: None,
            #[allow(deprecated)]
            cwd: turn_context.cwd.clone(),
            workspace_roots: None,
            current_date: turn_context.current_date.clone(),
            timezone: turn_context.timezone.clone(),
            approval_policy: turn_context.approval_policy(),
            approvals_reviewer: None,
            sandbox_policy: turn_context.sandbox_policy(),
            permission_profile: None,
            active_permission_profile: None,
            network: None,
            file_system_sandbox_policy: None,
            model: previous_model.to_string(),
            comp_hash: None,
            personality: turn_context.personality(),
            collaboration_mode: Some(turn_context.collaboration_mode()),
            multi_agent_version: None,
            multi_agent_mode: None,
            realtime_active: Some(turn_context.realtime_active),
            cyber_access_program: None,
            effort: turn_context.reasoning_effort().cloned(),
            summary: codex_protocol::config_types::ReasoningSummary::Auto,
        }))
        .expect("serialize expected reference context item")
    );
}

#[tokio::test]
async fn record_initial_history_resumed_aborted_turn_without_id_clears_active_turn_for_compaction_accounting()
 {
    let (session, turn_context) = make_session_and_context().await;
    let previous_model = "previous-rollout-model";
    let previous_context_item = TurnContextItem {
        turn_id: Some(turn_context.sub_id.clone()),
        root_turn_id: None,
        disabled_plugin_ids: None,
        #[allow(deprecated)]
        cwd: turn_context.cwd.clone(),
        workspace_roots: None,
        current_date: turn_context.current_date.clone(),
        timezone: turn_context.timezone.clone(),
        approval_policy: turn_context.approval_policy(),
        approvals_reviewer: None,
        sandbox_policy: turn_context.sandbox_policy(),
        permission_profile: None,
        active_permission_profile: None,
        network: None,
        file_system_sandbox_policy: None,
        model: previous_model.to_string(),
        comp_hash: None,
        personality: turn_context.personality(),
        collaboration_mode: Some(turn_context.collaboration_mode()),
        multi_agent_version: None,
        multi_agent_mode: None,
        realtime_active: Some(turn_context.realtime_active),
        cyber_access_program: None,
        effort: turn_context.reasoning_effort().cloned(),
        summary: codex_protocol::config_types::ReasoningSummary::Auto,
    };
    let previous_turn_id = previous_context_item
        .turn_id
        .clone()
        .expect("turn context should have turn_id");
    let aborted_turn_id = "aborted-turn-without-id".to_string();

    let rollout_items = vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: previous_turn_id.clone(),
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "seed".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::TurnContext(previous_context_item),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: previous_turn_id,
                started_at: None,
                last_agent_message: None,
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: aborted_turn_id,
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "aborted".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::EventMsg(EventMsg::TurnAborted(
            codex_protocol::protocol::TurnAbortedEvent {
                turn_id: None,
                started_at: None,
                reason: TurnAbortReason::Interrupted,
                completed_at: None,
                duration_ms: None,
            },
        )),
        RolloutItem::Compacted(CompactedItem {
            message: String::new(),
            replacement_history: Some(Vec::new()),
            retained_context: None,
            guardian_history: None,
            mcp_resource_origins: None,
            window_number: None,
            first_window_id: None,
            previous_window_id: None,
            window_id: None,
            compaction_response_id: None,
            latest_token_usage_record: None,
        }),
    ];

    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: ThreadId::default(),
            history: Arc::new(rollout_items),
            rollout_path: Some(PathBuf::from("/tmp/resume.jsonl")),
        }))
        .await;

    assert_eq!(
        session.previous_turn_settings().await,
        Some(PreviousTurnSettings {
            model: previous_model.to_string(),
            comp_hash: None,
            realtime_active: Some(turn_context.realtime_active),
        })
    );
    assert!(session.reference_context_item().await.is_none());
}

#[tokio::test]
async fn record_initial_history_resumed_unmatched_abort_preserves_active_turn_for_later_turn_context()
 {
    let (session, turn_context) = make_session_and_context().await;
    let previous_context_item = turn_context.to_turn_context_item();
    let previous_turn_id = previous_context_item
        .turn_id
        .clone()
        .expect("turn context should have turn_id");
    let current_model = "current-rollout-model";
    let current_turn_id = "current-turn".to_string();
    let unmatched_abort_turn_id = "other-turn".to_string();
    let current_context_item = TurnContextItem {
        turn_id: Some(current_turn_id.clone()),
        root_turn_id: None,
        disabled_plugin_ids: None,
        #[allow(deprecated)]
        cwd: turn_context.cwd.clone(),
        workspace_roots: None,
        current_date: turn_context.current_date.clone(),
        timezone: turn_context.timezone.clone(),
        approval_policy: turn_context.approval_policy(),
        approvals_reviewer: None,
        sandbox_policy: turn_context.sandbox_policy(),
        permission_profile: None,
        active_permission_profile: None,
        network: None,
        file_system_sandbox_policy: None,
        model: current_model.to_string(),
        comp_hash: None,
        personality: turn_context.personality(),
        collaboration_mode: Some(turn_context.collaboration_mode()),
        multi_agent_version: None,
        multi_agent_mode: None,
        realtime_active: Some(turn_context.realtime_active),
        cyber_access_program: None,
        effort: turn_context.reasoning_effort().cloned(),
        summary: codex_protocol::config_types::ReasoningSummary::Auto,
    };

    let rollout_items = vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: previous_turn_id.clone(),
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "seed".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::TurnContext(previous_context_item),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: previous_turn_id,
                started_at: None,
                last_agent_message: None,
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: current_turn_id.clone(),
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "current".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::EventMsg(EventMsg::TurnAborted(
            codex_protocol::protocol::TurnAbortedEvent {
                turn_id: Some(unmatched_abort_turn_id),
                started_at: None,
                reason: TurnAbortReason::Interrupted,
                completed_at: None,
                duration_ms: None,
            },
        )),
        RolloutItem::TurnContext(current_context_item.clone()),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: current_turn_id,
                started_at: None,
                last_agent_message: None,
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
    ];

    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: ThreadId::default(),
            history: Arc::new(rollout_items),
            rollout_path: Some(PathBuf::from("/tmp/resume.jsonl")),
        }))
        .await;

    assert_eq!(
        session.previous_turn_settings().await,
        Some(PreviousTurnSettings {
            model: current_model.to_string(),
            comp_hash: None,
            realtime_active: Some(turn_context.realtime_active),
        })
    );
    assert_eq!(
        serde_json::to_value(session.reference_context_item().await)
            .expect("serialize seeded reference context item"),
        serde_json::to_value(Some(current_context_item))
            .expect("serialize expected reference context item")
    );
}

#[tokio::test]
async fn record_initial_history_resumed_trailing_incomplete_turn_compaction_clears_reference_context_item()
 {
    let (session, turn_context) = make_session_and_context().await;
    let previous_model = "previous-rollout-model";
    let previous_context_item = TurnContextItem {
        turn_id: Some(turn_context.sub_id.clone()),
        root_turn_id: None,
        disabled_plugin_ids: None,
        #[allow(deprecated)]
        cwd: turn_context.cwd.clone(),
        workspace_roots: None,
        current_date: turn_context.current_date.clone(),
        timezone: turn_context.timezone.clone(),
        approval_policy: turn_context.approval_policy(),
        approvals_reviewer: None,
        sandbox_policy: turn_context.sandbox_policy(),
        permission_profile: None,
        active_permission_profile: None,
        network: None,
        file_system_sandbox_policy: None,
        model: previous_model.to_string(),
        comp_hash: None,
        personality: turn_context.personality(),
        collaboration_mode: Some(turn_context.collaboration_mode()),
        multi_agent_version: None,
        multi_agent_mode: None,
        realtime_active: Some(turn_context.realtime_active),
        cyber_access_program: None,
        effort: turn_context.reasoning_effort().cloned(),
        summary: codex_protocol::config_types::ReasoningSummary::Auto,
    };
    let previous_turn_id = previous_context_item
        .turn_id
        .clone()
        .expect("turn context should have turn_id");
    let incomplete_turn_id = "trailing-incomplete-turn".to_string();

    let rollout_items = vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: previous_turn_id.clone(),
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "seed".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::TurnContext(previous_context_item),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: previous_turn_id,
                started_at: None,
                last_agent_message: None,
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: incomplete_turn_id,
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "incomplete".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::Compacted(CompactedItem {
            message: String::new(),
            replacement_history: Some(Vec::new()),
            retained_context: None,
            guardian_history: None,
            mcp_resource_origins: None,
            window_number: None,
            first_window_id: None,
            previous_window_id: None,
            window_id: None,
            compaction_response_id: None,
            latest_token_usage_record: None,
        }),
    ];

    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: ThreadId::default(),
            history: Arc::new(rollout_items),
            rollout_path: Some(PathBuf::from("/tmp/resume.jsonl")),
        }))
        .await;

    assert_eq!(
        session.previous_turn_settings().await,
        Some(PreviousTurnSettings {
            model: previous_model.to_string(),
            comp_hash: None,
            realtime_active: Some(turn_context.realtime_active),
        })
    );
    assert!(session.reference_context_item().await.is_none());
}

#[tokio::test]
async fn record_initial_history_resumed_trailing_incomplete_turn_preserves_turn_context_item() {
    let (session, turn_context) = make_session_and_context().await;
    let current_context_item = turn_context.to_turn_context_item();
    let current_turn_id = current_context_item
        .turn_id
        .clone()
        .expect("turn context should have turn_id");

    let rollout_items = vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: current_turn_id,
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "incomplete".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::TurnContext(current_context_item.clone()),
    ];

    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: ThreadId::default(),
            history: Arc::new(rollout_items),
            rollout_path: Some(PathBuf::from("/tmp/resume.jsonl")),
        }))
        .await;

    assert_eq!(
        session.previous_turn_settings().await,
        Some(PreviousTurnSettings {
            model: turn_context.model_info().slug.clone(),
            comp_hash: None,
            realtime_active: Some(turn_context.realtime_active),
        })
    );
    assert_eq!(
        serde_json::to_value(session.reference_context_item().await)
            .expect("serialize seeded reference context item"),
        serde_json::to_value(Some(current_context_item))
            .expect("serialize expected reference context item")
    );
}

#[tokio::test]
async fn record_initial_history_resumed_replaced_incomplete_compacted_turn_clears_reference_context_item()
 {
    let (session, turn_context) = make_session_and_context().await;
    let previous_model = "previous-rollout-model";
    let previous_context_item = TurnContextItem {
        turn_id: Some(turn_context.sub_id.clone()),
        root_turn_id: None,
        disabled_plugin_ids: None,
        #[allow(deprecated)]
        cwd: turn_context.cwd.clone(),
        workspace_roots: None,
        current_date: turn_context.current_date.clone(),
        timezone: turn_context.timezone.clone(),
        approval_policy: turn_context.approval_policy(),
        approvals_reviewer: None,
        sandbox_policy: turn_context.sandbox_policy(),
        permission_profile: None,
        active_permission_profile: None,
        network: None,
        file_system_sandbox_policy: None,
        model: previous_model.to_string(),
        comp_hash: None,
        personality: turn_context.personality(),
        collaboration_mode: Some(turn_context.collaboration_mode()),
        multi_agent_version: None,
        multi_agent_mode: None,
        realtime_active: Some(turn_context.realtime_active),
        cyber_access_program: None,
        effort: turn_context.reasoning_effort().cloned(),
        summary: codex_protocol::config_types::ReasoningSummary::Auto,
    };
    let previous_turn_id = previous_context_item
        .turn_id
        .clone()
        .expect("turn context should have turn_id");
    let compacted_incomplete_turn_id = "compacted-incomplete-turn".to_string();
    let replacing_turn_id = "replacing-turn".to_string();

    let rollout_items = vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: previous_turn_id.clone(),
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "seed".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::TurnContext(previous_context_item),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: previous_turn_id,
                started_at: None,
                last_agent_message: None,
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: compacted_incomplete_turn_id,
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "compacted".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::Compacted(CompactedItem {
            message: String::new(),
            replacement_history: Some(Vec::new()),
            retained_context: None,
            guardian_history: None,
            mcp_resource_origins: None,
            window_number: None,
            first_window_id: None,
            previous_window_id: None,
            window_id: None,
            compaction_response_id: None,
            latest_token_usage_record: None,
        }),
        // A newer TurnStarted replaces the incomplete compacted turn without a matching
        // completion/abort for the old one.
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: replacing_turn_id,
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
            },
        )),
    ];

    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: ThreadId::default(),
            history: Arc::new(rollout_items),
            rollout_path: Some(PathBuf::from("/tmp/resume.jsonl")),
        }))
        .await;

    assert_eq!(
        session.previous_turn_settings().await,
        Some(PreviousTurnSettings {
            model: previous_model.to_string(),
            comp_hash: None,
            realtime_active: Some(turn_context.realtime_active),
        })
    );
    assert!(session.reference_context_item().await.is_none());
}
