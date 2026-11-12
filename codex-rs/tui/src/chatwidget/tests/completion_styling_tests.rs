//! Exercises completion metadata through live notifications and restored turn history.

use super::*;
use crate::clock_format::ClockFormat;
use chrono::Local;
use chrono::TimeZone;
use pretty_assertions::assert_eq;

const COMPLETED_AT: i64 = 1_700_000_000;

fn completed_turn(duration_ms: Option<i64>, completed_at: Option<i64>) -> AppServerTurn {
    AppServerTurn {
        completed_at,
        items: vec![AppServerThreadItem::AgentMessage {
            id: "answer-1".to_string(),
            text: "The change is ready.".to_string(),
            phase: Some(MessagePhase::FinalAnswer),
            memory_citation: None,
            delivery: None,
            questions: None,
        }],
        ..app_server_turn(
            "turn-1",
            AppServerTurnStatus::Completed,
            duration_ms,
            /*error*/ None,
        )
    }
}

fn complete_turn(chat: &mut ChatWidget, turn: AppServerTurn) {
    chat.handle_server_notification(
        ServerNotification::TurnCompleted(TurnCompletedNotification {
            thread_id: chat.thread_id.map(|id| id.to_string()).unwrap_or_default(),
            turn,
        }),
        /*replay_kind*/ None,
    );
}

fn completion_labels(rx: &mut tokio::sync::mpsc::UnboundedReceiver<AppEvent>) -> String {
    std::iter::from_fn(|| rx.try_recv().ok())
        .filter_map(|event| match event {
            AppEvent::InsertHistoryCell(cell)
                if cell.as_any().is::<history_cell::FinalMessageSeparator>() =>
            {
                let label = lines_to_single_string(&cell.raw_lines()).trim().to_string();
                (!label.is_empty()).then_some(label)
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn saved_completion_label() -> String {
    Local
        .timestamp_opt(COMPLETED_AT, /*nsecs*/ 0)
        .unwrap()
        .format(&format!(
            "%b %-d, %Y at {}",
            ClockFormat::system().time_format()
        ))
        .to_string()
}

#[tokio::test]
async fn completion_follows_plain_and_streamed_tool_answers() {
    for streamed in [false, true] {
        let (mut chat, mut rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
        handle_turn_started(&mut chat, "turn-1");
        if streamed {
            let command = begin_exec(&mut chat, "command-1", "echo ready");
            end_exec(&mut chat, command, "ready", "", /*exit_code*/ 0);
            handle_agent_message_delta(&mut chat, "The change is ready.\n");
            chat.run_commit_tick();
        }
        complete_turn(&mut chat, completed_turn(Some(125_000), Some(COMPLETED_AT)));

        let text = drain_insert_history_normalized(&mut rx)
            .iter()
            .map(|lines| lines_to_single_string(lines))
            .collect::<String>();
        assert_chatwidget_snapshot!(
            if streamed {
                "completion_after_streamed_tool_answer"
            } else {
                "completion_after_plain_answer"
            },
            text,
        );
    }
}

#[tokio::test]
async fn completion_live_applies_duration_threshold_and_preserves_timestamp_fallback() {
    for (duration_ms, completed_at, prefix) in [
        (598, Some(COMPLETED_AT), ""),
        (1_000, Some(COMPLETED_AT), ""),
        (60_000, Some(COMPLETED_AT), ""),
        (60_999, Some(COMPLETED_AT), ""),
        (61_000, Some(COMPLETED_AT), "Worked for 1m 1s · "),
        (125_000, Some(COMPLETED_AT), "Worked for 2m 5s · "),
        (1_000, None, ""),
    ] {
        let (mut chat, mut rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
        handle_turn_started(&mut chat, "turn-1");
        let turn = completed_turn(Some(duration_ms), completed_at);
        let before = Local::now();
        complete_turn(&mut chat, turn.clone());
        let after = Local::now();
        let possible = [before, after].map(|time| {
            let time = if completed_at.is_some() {
                saved_completion_label()
            } else {
                time.format(ClockFormat::system().time_format()).to_string()
            };
            format!("{prefix}{time}")
        });
        let label = completion_labels(&mut rx);
        assert!(possible.contains(&label), "{label:?}");
        complete_turn(&mut chat, turn);
        assert_eq!(completion_labels(&mut rx), "");
    }
}

#[tokio::test]
async fn completion_replay_preserves_metadata_and_input_without_live_side_effects() {
    let done = saved_completion_label();
    for (duration_ms, completed_at, expected) in [
        (
            Some(125_000),
            Some(COMPLETED_AT),
            format!("Worked for 2m 5s · {done}"),
        ),
        (None, None, String::new()),
        (Some(598), Some(COMPLETED_AT), done.clone()),
        (Some(60_000), Some(COMPLETED_AT), done.clone()),
        (
            Some(61_000),
            Some(COMPLETED_AT),
            format!("Worked for 1m 1s · {done}"),
        ),
        (Some(60_000), None, String::new()),
        (Some(125_000), None, "Worked for 2m 5s".to_string()),
    ] {
        for replay_kind in [
            ReplayKind::ResumeInitialMessages,
            ReplayKind::ThreadSnapshot,
        ] {
            let (mut chat, mut rx, mut op_rx) =
                make_chatwidget_manual(/*model_override*/ None).await;
            chat.thread_id = Some(ThreadId::new());
            handle_turn_started(&mut chat, "turn-1");
            if matches!(replay_kind, ReplayKind::ThreadSnapshot) {
                chat.queue_user_message("Continue".into());
            }
            let queued_input = chat.input_queue.queued_user_messages.clone();
            while op_rx.try_recv().is_ok() {}
            let mut turn = completed_turn(duration_ms, completed_at);
            chat.replay_thread_turns(vec![turn.clone()], replay_kind);
            assert_eq!(completion_labels(&mut rx), expected);
            assert_matches!(chat.pending_notification, None);
            assert_eq!(chat.input_queue.queued_user_messages, queued_input);
            assert_matches!(op_rx.try_recv(), Err(TryRecvError::Empty));

            // An empty replay label must not suppress later authoritative live metadata.
            chat.input_queue.queued_user_messages.clear();
            turn.completed_at = Some(COMPLETED_AT);
            complete_turn(&mut chat, turn.clone());
            assert_eq!(
                completion_labels(&mut rx),
                if expected.is_empty() { &done } else { "" }
            );
            complete_turn(&mut chat, turn);
            assert_eq!(completion_labels(&mut rx), "");
        }
    }
}

#[tokio::test]
async fn completion_replay_seeds_last_activity_from_saved_completion_time() {
    let (mut chat, _rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.local_settings.tui.rencrow_tui_jst_timestamps = true;
    let completed_at = COMPLETED_AT;
    let mut newer = completed_turn(Some(125_000), Some(completed_at));
    newer.id = "turn-newer".to_string();

    chat.replay_thread_turns(vec![newer], ReplayKind::ResumeInitialMessages);

    assert_eq!(chat.last_activity_epoch_seconds, Some(completed_at));

    let mut older = completed_turn(Some(125_000), Some(completed_at - 60));
    older.id = "turn-older".to_string();
    chat.replay_thread_turns(vec![older], ReplayKind::ThreadSnapshot);
    assert_eq!(chat.last_activity_epoch_seconds, Some(completed_at));

    let (mut unknown_chat, _rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
    unknown_chat.local_settings.tui.rencrow_tui_jst_timestamps = true;
    unknown_chat.replay_thread_turns(
        vec![completed_turn(Some(125_000), /*completed_at*/ None)],
        ReplayKind::ResumeInitialMessages,
    );
    assert_eq!(unknown_chat.last_activity_epoch_seconds, None);
}

#[tokio::test]
async fn assistant_history_keeps_jst_timestamp_for_live_and_replayed_answers() {
    let completed_at = chrono::DateTime::parse_from_rfc3339("2026-11-08T18:16:00+09:00")
        .unwrap()
        .timestamp();
    let expected_timestamp = "[18:16:00 JST]";

    let (mut live_chat, mut live_rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
    live_chat.local_settings.tui.rencrow_tui_jst_timestamps = true;
    handle_turn_started(&mut live_chat, "turn-live");
    handle_agent_message_delta(&mut live_chat, "# The live answer is ready.");
    live_chat.run_commit_tick();
    live_chat.handle_server_notification(
        ServerNotification::ItemCompleted(ItemCompletedNotification {
            item: AppServerThreadItem::AgentMessage {
                id: "answer-live".to_string(),
                text: "# The live answer is ready.".to_string(),
                phase: Some(MessagePhase::FinalAnswer),
                memory_citation: None,
                delivery: None,
                questions: None,
            },
            thread_id: "thread-live".to_string(),
            turn_id: "turn-live".to_string(),
            completed_at_ms: completed_at * 1_000,
        }),
        /*replay_kind*/ None,
    );
    let live_source = std::iter::from_fn(|| live_rx.try_recv().ok())
        .find_map(|event| match event {
            AppEvent::ConsolidateAgentMessage { source, .. } => Some(source),
            _ => None,
        })
        .expect("live final answer should be consolidated into history");
    assert!(
        live_source.starts_with(expected_timestamp),
        "{live_source:?}"
    );
    assert!(
        live_source.contains("\n\n# The live answer is ready."),
        "{live_source:?}"
    );
    assert_eq!(live_source.matches(expected_timestamp).count(), 1);

    let (mut replay_chat, mut replay_rx, _ops) =
        make_chatwidget_manual(/*model_override*/ None).await;
    replay_chat.local_settings.tui.rencrow_tui_jst_timestamps = true;
    let mut turn = completed_turn(None, Some(completed_at));
    turn.items[0] = AppServerThreadItem::AgentMessage {
        id: "answer-replay".to_string(),
        text: "# The replayed answer is ready.".to_string(),
        phase: Some(MessagePhase::FinalAnswer),
        memory_citation: None,
        delivery: None,
        questions: None,
    };
    replay_chat.replay_thread_turns(vec![turn], ReplayKind::ResumeInitialMessages);
    let replay_text = drain_insert_history(&mut replay_rx)
        .into_iter()
        .flatten()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(replay_text.contains(expected_timestamp), "{replay_text:?}");
    assert!(replay_text.contains("# The replayed answer is ready."));
}

#[tokio::test]
async fn replay_actual_phase_null_turn_keeps_jst_timestamp_on_assistant_history() {
    let (mut chat, mut rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.local_settings.tui.rencrow_tui_jst_timestamps = true;
    let turn: AppServerTurn =
        serde_json::from_str(include_str!("fixtures/jst_actual_phase_null_turn.json"))
            .expect("captured app-server turn should deserialize");
    assert_eq!(turn.completed_at, Some(1_794_132_708));
    assert!(matches!(
        &turn.items[0],
        AppServerThreadItem::AgentMessage { phase: None, .. }
    ));

    chat.replay_thread_turns(vec![turn], ReplayKind::ThreadSnapshot);

    let history = drain_insert_history(&mut rx)
        .into_iter()
        .flatten()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(history.contains("[19:11:48 JST]"), "{history:?}");
    assert_eq!(history.matches("[19:11:48 JST]").count(), 1, "{history:?}");
    assert!(
        history.contains("凍結schemaが確定している数値下限"),
        "{history:?}"
    );
}

#[tokio::test]
async fn live_stream_stamps_first_display_and_reuses_it_at_completion() {
    let (mut chat, mut rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.local_settings.tui.rencrow_tui_jst_timestamps = true;
    let turn: AppServerTurn =
        serde_json::from_str(include_str!("fixtures/jst_actual_phase_null_turn.json"))
            .expect("captured app-server turn should deserialize");
    let turn_id = turn.id.clone();
    let thread_id = chat.thread_id.map(|id| id.to_string()).unwrap_or_default();
    let item = turn.items[0].clone();
    let (item_id, text) = match &item {
        AppServerThreadItem::AgentMessage {
            id,
            text,
            phase: None,
            ..
        } => (id.clone(), text.clone()),
        other => panic!("expected captured phase:null AgentMessage, got {other:?}"),
    };
    let split_at = text
        .char_indices()
        .nth(40)
        .map(|(index, _)| index)
        .unwrap_or(text.len());
    let (first_delta, remaining_delta) = text.split_at(split_at);

    handle_turn_started(&mut chat, &turn_id);
    chat.handle_server_notification(
        ServerNotification::ItemStarted(ItemStartedNotification {
            thread_id: thread_id.clone(),
            turn_id: turn_id.clone(),
            started_at_ms: 0,
            item: item.clone(),
        }),
        /*replay_kind*/ None,
    );
    chat.handle_server_notification(
        ServerNotification::AgentMessageDelta(
            codex_app_server_protocol::AgentMessageDeltaNotification {
                thread_id: thread_id.clone(),
                turn_id: turn_id.clone(),
                item_id: item_id.clone(),
                delta: first_delta.to_string(),
            },
        ),
        /*replay_kind*/ None,
    );
    let display_epoch = chat
        .last_activity_epoch_seconds
        .expect("live delta records event time");
    let expected_prefix = super::super::activity_clock::history_timestamp_label(display_epoch)
        .expect("event epoch should format");
    chat.run_commit_tick();

    let first_display = std::iter::from_fn(|| rx.try_recv().ok())
        .filter_map(|event| match event {
            AppEvent::InsertHistoryCell(cell)
                if cell.as_any().is::<history_cell::AgentMessageCell>() =>
            {
                Some(
                    cell.display_lines(120)
                        .into_iter()
                        .map(|line| line.to_string())
                        .collect::<Vec<_>>()
                        .join("\n"),
                )
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        first_display.contains(&expected_prefix),
        "{first_display:?}"
    );
    assert_eq!(first_display.matches(&expected_prefix).count(), 1);

    chat.handle_server_notification(
        ServerNotification::AgentMessageDelta(
            codex_app_server_protocol::AgentMessageDeltaNotification {
                thread_id: thread_id.clone(),
                turn_id: turn_id.clone(),
                item_id,
                delta: remaining_delta.to_string(),
            },
        ),
        /*replay_kind*/ None,
    );
    chat.run_commit_tick();
    let later_display = std::iter::from_fn(|| rx.try_recv().ok())
        .filter_map(|event| match event {
            AppEvent::InsertHistoryCell(cell)
                if cell.as_any().is::<history_cell::AgentMessageCell>() =>
            {
                Some(
                    cell.display_lines(120)
                        .into_iter()
                        .map(|line| line.to_string())
                        .collect::<Vec<_>>()
                        .join("\n"),
                )
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !later_display.contains(&expected_prefix),
        "{later_display:?}"
    );

    chat.handle_server_notification(
        ServerNotification::ItemCompleted(ItemCompletedNotification {
            thread_id: thread_id.clone(),
            turn_id: turn_id.clone(),
            item,
            completed_at_ms: 1_794_132_708_393,
        }),
        /*replay_kind*/ None,
    );
    chat.handle_server_notification(
        ServerNotification::TurnCompleted(TurnCompletedNotification { thread_id, turn }),
        /*replay_kind*/ None,
    );
    let consolidated = std::iter::from_fn(|| rx.try_recv().ok())
        .filter_map(|event| match event {
            AppEvent::ConsolidateAgentMessage { source, .. } => Some(source),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(consolidated.len(), 1, "{consolidated:?}");
    assert!(
        consolidated[0].starts_with(&expected_prefix),
        "{consolidated:?}"
    );
    assert_eq!(consolidated[0].matches(&expected_prefix).count(), 1);
    assert!(consolidated[0].contains("凍結schemaが確定している数値下限"));
}

#[tokio::test]
async fn live_agent_stream_keeps_default_jst_timestamp_opt_out() {
    let (mut chat, mut rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
    assert!(!chat.activity_timestamps_enabled());
    handle_turn_started(&mut chat, "turn-opt-out");
    chat.handle_server_notification(
        ServerNotification::AgentMessageDelta(
            codex_app_server_protocol::AgentMessageDeltaNotification {
                thread_id: "thread-opt-out".to_string(),
                turn_id: "turn-opt-out".to_string(),
                item_id: "message-opt-out".to_string(),
                delta: "# The timestamp stays disabled.\n".to_string(),
            },
        ),
        /*replay_kind*/ None,
    );
    chat.run_commit_tick();
    let displayed = std::iter::from_fn(|| rx.try_recv().ok())
        .filter_map(|event| match event {
            AppEvent::InsertHistoryCell(cell)
                if cell.as_any().is::<history_cell::AgentMessageCell>() =>
            {
                Some(
                    cell.display_lines(80)
                        .into_iter()
                        .map(|line| line.to_string())
                        .collect::<Vec<_>>()
                        .join("\n"),
                )
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(displayed.contains("The timestamp stays disabled."));
    assert!(!displayed.contains("JST]"), "{displayed:?}");
}

#[tokio::test]
async fn replay_delta_does_not_use_live_clock_for_jst_timestamp() {
    let (mut chat, _rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.local_settings.tui.rencrow_tui_jst_timestamps = true;
    chat.last_activity_epoch_seconds = Some(1_794_132_708);
    chat.handle_server_notification(
        ServerNotification::AgentMessageDelta(
            codex_app_server_protocol::AgentMessageDeltaNotification {
                thread_id: "thread-replay-delta".to_string(),
                turn_id: "turn-replay-delta".to_string(),
                item_id: "message-replay-delta".to_string(),
                delta: "Historical assistant text without recorded item time.".to_string(),
            },
        ),
        Some(ReplayKind::ThreadSnapshot),
    );
    assert_eq!(
        chat.transcript
            .agent_message_stream_item
            .as_ref()
            .map(|(turn_id, item_id)| (turn_id.as_str(), item_id.as_str())),
        Some(("turn-replay-delta", "message-replay-delta"))
    );
    assert_eq!(chat.transcript.agent_message_display_timestamp, None);
}

#[tokio::test]
async fn terminal_error_history_uses_recorded_time_without_filling_unknown_replay_time() {
    let completed_at = chrono::DateTime::parse_from_rfc3339("2026-11-08T18:17:00+09:00")
        .unwrap()
        .timestamp();
    let message = "RENCROW_EXECUTION_CONTRACT_INCOMPLETE: missing receipt";
    let expected_timestamp = "[18:17:00 JST]";

    let (mut replay_chat, mut replay_rx, _ops) =
        make_chatwidget_manual(/*model_override*/ None).await;
    replay_chat.local_settings.tui.rencrow_tui_jst_timestamps = true;
    let mut failed = completed_turn(None, Some(completed_at));
    failed.status = AppServerTurnStatus::Failed;
    failed.error = Some(AppServerTurnError {
        misalignment: None,
        message: message.to_string(),
        codex_error_info: None,
        additional_details: None,
    });
    replay_chat.handle_server_notification(
        ServerNotification::TurnCompleted(TurnCompletedNotification {
            thread_id: "thread-1".to_string(),
            turn: failed,
        }),
        Some(ReplayKind::ThreadSnapshot),
    );
    let replay_error = drain_insert_history(&mut replay_rx)
        .into_iter()
        .flatten()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        replay_error.contains(&format!("{expected_timestamp} ■ {message}")),
        "{replay_error:?}"
    );

    let (mut live_chat, mut live_rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
    live_chat.local_settings.tui.rencrow_tui_jst_timestamps = true;
    let before = super::super::activity_clock::now_epoch_seconds();
    live_chat.handle_server_notification(
        ServerNotification::Error(ErrorNotification {
            error: AppServerTurnError {
                misalignment: None,
                message: message.to_string(),
                codex_error_info: None,
                additional_details: None,
            },
            will_retry: false,
            thread_id: "thread-1".to_string(),
            turn_id: "turn-live-error".to_string(),
        }),
        /*replay_kind*/ None,
    );
    let after = super::super::activity_clock::now_epoch_seconds();
    let live_error = drain_insert_history(&mut live_rx)
        .into_iter()
        .flatten()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    let possible_timestamps = [before, after]
        .map(super::super::activity_clock::history_timestamp_label)
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    assert!(
        possible_timestamps
            .iter()
            .any(|timestamp| live_error.contains(&format!("{timestamp} ■ {message}"))),
        "{live_error:?}"
    );

    let (mut unknown_chat, mut unknown_rx, _ops) =
        make_chatwidget_manual(/*model_override*/ None).await;
    unknown_chat.local_settings.tui.rencrow_tui_jst_timestamps = true;
    let mut unknown_failed = completed_turn(None, None);
    unknown_failed.status = AppServerTurnStatus::Failed;
    unknown_failed.error = Some(AppServerTurnError {
        misalignment: None,
        message: message.to_string(),
        codex_error_info: None,
        additional_details: None,
    });
    unknown_chat.handle_server_notification(
        ServerNotification::TurnCompleted(TurnCompletedNotification {
            thread_id: "thread-1".to_string(),
            turn: unknown_failed,
        }),
        Some(ReplayKind::ThreadSnapshot),
    );
    let unknown_error = drain_insert_history(&mut unknown_rx)
        .into_iter()
        .flatten()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!unknown_error.contains("JST]"), "{unknown_error:?}");
}

#[tokio::test]
async fn completion_replay_waits_for_older_turn_items_to_load() {
    let (mut chat, mut rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
    let older_completed_at = COMPLETED_AT - 86_400;
    let mut older = completed_turn(Some(125_000), Some(older_completed_at));
    older.id = "older-turn".to_string();
    let mut unloaded = older.clone();
    unloaded.items_view = codex_app_server_protocol::TurnItemsView::NotLoaded;
    unloaded.items.clear();
    let newer = completed_turn(Some(1_000), Some(COMPLETED_AT));

    chat.replay_thread_turns(vec![unloaded, newer], ReplayKind::ResumeInitialMessages);
    assert_eq!(completion_labels(&mut rx), saved_completion_label());

    chat.replay_thread_turns(vec![older], ReplayKind::ThreadSnapshot);
    let older_time = Local
        .timestamp_opt(older_completed_at, /*nsecs*/ 0)
        .unwrap()
        .format(&format!(
            "%b %-d, %Y at {}",
            ClockFormat::system().time_format()
        ))
        .to_string();
    assert_eq!(
        completion_labels(&mut rx),
        format!("Worked for 2m 5s · {older_time}"),
    );
}

#[tokio::test]
async fn completion_failed_and_interrupted_turns_do_not_report_success() {
    for status in [
        AppServerTurnStatus::Failed,
        AppServerTurnStatus::Interrupted,
    ] {
        for replay_kind in [None, Some(ReplayKind::ResumeInitialMessages)] {
            let (mut chat, mut rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
            let mut turn = completed_turn(Some(125_000), Some(COMPLETED_AT));
            turn.status = status.clone();
            turn.error = (status == AppServerTurnStatus::Failed).then(|| AppServerTurnError {
                misalignment: None,
                message: "The task failed.".to_string(),
                codex_error_info: None,
                additional_details: None,
            });
            if let Some(replay_kind) = replay_kind {
                chat.replay_thread_turns(vec![turn], replay_kind);
            } else {
                handle_turn_started(&mut chat, "turn-1");
                complete_turn(&mut chat, turn);
            }
            assert_eq!(completion_labels(&mut rx), "");
            assert!(!chat.bottom_pane.is_task_running());
        }
    }
}

#[test]
fn completion_snapshot_normalization_preserves_clock_only_message_lines() {
    let timestamp = "Sep 6, 2000 at 2:32 PM";
    let transcript = format!(
        "› {timestamp}\n• {timestamp}\n  └ {timestamp}\n  The job was done {timestamp}\n  {timestamp} is the expected text\n  Worked for 2m 5s · {timestamp} is quoted\n\n  3:24 PM\n  Worked for 1h 2m 3s · {timestamp}\n"
    );
    let message = history_cell::PlainHistoryCell::new(
        transcript
            .lines()
            .map(|line| Line::from(line.to_owned()))
            .collect(),
    );
    assert_chatwidget_snapshot!("completion_like_transcript_text", &transcript);
    assert_eq!(
        normalize_completion_timestamps(&message, &transcript),
        transcript
    );

    for clock_format in [ClockFormat::TwelveHour, ClockFormat::TwentyFourHour] {
        let footer =
            history_cell::FinalMessageSeparator::new(Some(3_723), /*runtime_metrics*/ None)
                .with_completed_at(
                    Local.timestamp_opt(COMPLETED_AT, /*nsecs*/ 0).unwrap(),
                    clock_format,
                );
        assert_eq!(
            normalize_completion_timestamps(
                &footer,
                lines_to_single_string(&footer.display_lines(/*width*/ 80))
            ),
            "  Worked for [duration] · [completion time]\n"
        );
    }
}
