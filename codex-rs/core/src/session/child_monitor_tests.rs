use codex_api::ResponseEvent;
use codex_features::Feature;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TurnStartedEvent;

#[tokio::test]
async fn child_monitor_observes_raw_data_but_not_empty_output_items() {
    let (mut session, turn) = crate::session::tests::make_session_and_context().await;
    session
        .features
        .enable(Feature::RencrowChildMonitor)
        .expect("monitor opt-in");
    session.child_monitor.lock().await.observe(
        &turn.sub_id,
        &EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: turn.sub_id.clone(),
            root_turn_id: None,
            trace_id: None,
            started_at: None,
            model_context_window: None,
            collaboration_mode_kind: Default::default(),
        }),
        /*observed_at_ms*/ 1,
    );
    let before = session
        .child_monitor_snapshot()
        .await
        .expect("monitor enabled");
    session
        .observe_child_model_output(
            &turn.sub_id,
            &ResponseEvent::OutputItemDone(ResponseItem::Message {
                id: None,
                role: "assistant".to_string(),
                content: vec![ContentItem::OutputText {
                    text: String::new(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            }),
        )
        .await;
    assert_eq!(session.child_monitor_snapshot().await, Some(before.clone()));
    session
        .observe_child_model_output(
            &turn.sub_id,
            &ResponseEvent::ToolCallInputDelta {
                item_id: "tool".to_string(),
                call_id: Some("call".to_string()),
                delta: "{".to_string(),
            },
        )
        .await;
    let after = session
        .child_monitor_snapshot()
        .await
        .expect("monitor enabled");
    assert!(after.first_output_at_ms.is_some());
    assert!(after.progress_sequence > before.progress_sequence);
    assert_eq!(after.active_tool_count, 0);
}

#[tokio::test]
async fn child_monitor_nonterminal_error_does_not_report_a_running_child_failed() {
    use codex_protocol::protocol::AgentStatus;
    use codex_protocol::protocol::CodexErrorInfo;
    use codex_protocol::protocol::ErrorEvent;
    use codex_protocol::protocol::Event;
    let (mut session, turn) = crate::session::tests::make_session_and_context().await;
    session
        .features
        .enable(Feature::RencrowChildMonitor)
        .expect("monitor opt-in");
    session.agent_status.send_replace(AgentStatus::Running);
    session
        .send_event_raw(Event {
            id: turn.sub_id.clone(),
            msg: EventMsg::Error(ErrorEvent {
                message: "rollback request failed; current work continues".to_string(),
                codex_error_info: Some(CodexErrorInfo::ThreadRollbackFailed),
                misalignment: None,
            }),
        })
        .await;
    assert_eq!(session.agent_status.borrow().clone(), AgentStatus::Running);
}
