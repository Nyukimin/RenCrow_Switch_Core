use super::ChildMonitor;
use super::ChildMonitorPhase;
use codex_protocol::models::WebSearchAction;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::protocol::WebSearchBeginEvent;
use codex_protocol::protocol::WebSearchEndEvent;

fn started_event(turn_id: &str) -> EventMsg {
    EventMsg::TurnStarted(TurnStartedEvent {
        turn_id: turn_id.to_string(),
        root_turn_id: None,
        trace_id: None,
        started_at: None,
        model_context_window: None,
        collaboration_mode_kind: Default::default(),
    })
}

#[test]
fn child_monitor_tracks_parallel_tool_lifecycle_without_guessing_waiting() {
    let mut monitor = ChildMonitor::default();
    monitor.observe("turn-1", &started_event("turn-1"), 101);
    assert_eq!(monitor.snapshot().phase, ChildMonitorPhase::Pending);

    monitor.observe(
        "turn-1",
        &EventMsg::WebSearchBegin(WebSearchBeginEvent {
            call_id: "call-1".to_string(),
        }),
        /*observed_at_ms*/ 102,
    );
    assert_eq!(monitor.snapshot().phase, ChildMonitorPhase::ToolExecution);
    assert_eq!(monitor.snapshot().active_tool_count, 1);

    monitor.observe(
        "turn-1",
        &EventMsg::WebSearchEnd(WebSearchEndEvent {
            call_id: "call-1".to_string(),
            query: "query".to_string(),
            action: WebSearchAction::Search {
                query: None,
                queries: None,
            },
            results: None,
        }),
        /*observed_at_ms*/ 103,
    );
    let ended = monitor.snapshot();
    assert_eq!(ended.phase, ChildMonitorPhase::Unknown);
    assert_eq!(ended.active_tool_count, 0);

    monitor.observe(
        "turn-1",
        &EventMsg::WebSearchEnd(WebSearchEndEvent {
            call_id: "call-1".to_string(),
            query: "query".to_string(),
            action: WebSearchAction::Search {
                query: None,
                queries: None,
            },
            results: None,
        }),
        /*observed_at_ms*/ 104,
    );
    assert_eq!(monitor.snapshot(), ended);
}

#[test]
fn child_monitor_ignores_events_from_an_older_turn() {
    let mut monitor = ChildMonitor::default();
    monitor.observe("turn-2", &started_event("turn-2"), 105);
    let current = monitor.snapshot();

    monitor.observe(
        "turn-1",
        &EventMsg::WebSearchBegin(WebSearchBeginEvent {
            call_id: "stale-call".to_string(),
        }),
        /*observed_at_ms*/ 106,
    );

    assert_eq!(monitor.snapshot(), current);
}

#[test]
fn child_monitor_distinguishes_generation_input_compaction_and_terminal_events() {
    use codex_protocol::ThreadId;
    use codex_protocol::items::ContextCompactionItem;
    use codex_protocol::items::TurnItem;
    use codex_protocol::protocol::ErrorEvent;
    use codex_protocol::protocol::ItemCompletedEvent;
    use codex_protocol::protocol::ItemStartedEvent;
    use codex_protocol::protocol::ReasoningContentDeltaEvent;
    use codex_protocol::protocol::TurnCompleteEvent;
    use codex_protocol::request_user_input::RequestUserInputEvent;
    let mut monitor = ChildMonitor::default();
    monitor.observe("turn-1", &started_event("turn-1"), 107);
    monitor.observe(
        "turn-1",
        &EventMsg::ReasoningContentDelta(ReasoningContentDeltaEvent {
            thread_id: "child".to_string(),
            turn_id: "turn-1".to_string(),
            item_id: "reasoning".to_string(),
            delta: "thinking".to_string(),
            summary_index: 0,
        }),
        /*observed_at_ms*/ 108,
    );
    assert_eq!(monitor.snapshot().phase, ChildMonitorPhase::Generating);
    monitor.observe(
        "turn-1",
        &EventMsg::RequestUserInput(RequestUserInputEvent {
            call_id: "question".to_string(),
            turn_id: "turn-1".to_string(),
            questions: Vec::new(),
            is_blocking: true,
            auto_resolution_ms: None,
        }),
        /*observed_at_ms*/ 109,
    );
    assert_eq!(monitor.snapshot().phase, ChildMonitorPhase::InputWaiting);
    monitor.input_received("wrong-id");
    assert_eq!(monitor.snapshot().phase, ChildMonitorPhase::InputWaiting);
    monitor.input_received("question");
    assert_eq!(monitor.snapshot().phase, ChildMonitorPhase::Pending);
    let item = TurnItem::ContextCompaction(ContextCompactionItem::new());
    let child = ThreadId::new();
    monitor.observe(
        "turn-1",
        &EventMsg::ItemStarted(ItemStartedEvent {
            thread_id: child,
            turn_id: "turn-1".to_string(),
            item: item.clone(),
            started_at_ms: 1,
        }),
        /*observed_at_ms*/ 110,
    );
    assert_eq!(monitor.snapshot().phase, ChildMonitorPhase::Compacting);
    monitor.observe(
        "turn-1",
        &EventMsg::ItemCompleted(ItemCompletedEvent {
            thread_id: child,
            turn_id: "turn-1".to_string(),
            item,
            started_at_ms: Some(1),
            completed_at_ms: 2,
        }),
        /*observed_at_ms*/ 111,
    );
    assert_eq!(monitor.snapshot().phase, ChildMonitorPhase::Pending);
    let mut end = TurnCompleteEvent {
        turn_id: "turn-1".to_string(),
        last_agent_message: None,
        error: None,
        started_at: None,
        completed_at: None,
        duration_ms: None,
        time_to_first_token_ms: None,
    };
    monitor.observe("turn-1", &EventMsg::TurnComplete(end.clone()), 112);
    assert_eq!(monitor.snapshot().phase, ChildMonitorPhase::Completed);
    monitor.observe("turn-1", &started_event("turn-1"), 113);
    end.error = Some(ErrorEvent {
        message: "terminal failure".to_string(),
        codex_error_info: None,
        misalignment: None,
    });
    monitor.observe("turn-1", &EventMsg::TurnComplete(end), 114);
    assert_eq!(monitor.snapshot().phase, ChildMonitorPhase::Failed);
    monitor.observe("", &EventMsg::ShutdownComplete, 115);
    assert_eq!(monitor.snapshot().phase, ChildMonitorPhase::Unknown);
}

#[test]
fn child_monitor_timestamps_are_event_driven_and_collection_is_generation_scoped() {
    use codex_protocol::protocol::EventMsg;
    use codex_protocol::protocol::TurnCompleteEvent;
    use codex_protocol::protocol::WebSearchBeginEvent;
    use codex_protocol::protocol::WebSearchEndEvent;

    let mut monitor = ChildMonitor::default();
    monitor.observe("turn-1", &started_event("turn-1"), 10);
    let started = monitor.snapshot();
    assert_eq!(started.turn_started_at_ms, Some(10));
    assert_eq!(started.last_activity_at_ms, Some(10));
    assert_eq!(started.progress_sequence, 1);

    monitor.model_output("stale-turn", 11);
    assert_eq!(monitor.snapshot(), started);
    monitor.model_output("turn-1", 20);
    let generated = monitor.snapshot();
    assert_eq!(generated.first_output_at_ms, Some(20));
    assert_eq!(generated.last_output_at_ms, Some(20));
    assert_eq!(generated.progress_sequence, 2);

    monitor.observe(
        "turn-1",
        &EventMsg::WebSearchBegin(WebSearchBeginEvent {
            call_id: "call-1".to_string(),
        }),
        30,
    );
    monitor.observe(
        "turn-1",
        &EventMsg::WebSearchBegin(WebSearchBeginEvent {
            call_id: "call-1".to_string(),
        }),
        31,
    );
    let tool_started = monitor.snapshot();
    assert_eq!(tool_started.tool_started_at_ms, Some(30));
    assert_eq!(tool_started.progress_sequence, 3);

    monitor.observe(
        "turn-1",
        &EventMsg::WebSearchEnd(WebSearchEndEvent {
            call_id: "call-1".to_string(),
            query: "query".to_string(),
            action: WebSearchAction::Search {
                query: None,
                queries: None,
            },
            results: None,
        }),
        40,
    );
    monitor.observe(
        "turn-1",
        &EventMsg::WebSearchEnd(WebSearchEndEvent {
            call_id: "call-1".to_string(),
            query: "query".to_string(),
            action: WebSearchAction::Search {
                query: None,
                queries: None,
            },
            results: None,
        }),
        41,
    );
    let tool_ended = monitor.snapshot();
    assert_eq!(tool_ended.tool_ended_at_ms, Some(40));
    assert_eq!(tool_ended.progress_sequence, 4);
    assert!(!monitor.collected("turn-1", 50));

    monitor.observe(
        "turn-1",
        &EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: "turn-1".to_string(),
            last_agent_message: None,
            error: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
        }),
        60,
    );
    let terminal = monitor.snapshot();
    assert_eq!(terminal.turn_completed_at_ms, Some(60));
    let terminal_sequence = terminal.progress_sequence;
    assert!(monitor.collected("turn-1", 70));
    assert!(monitor.collected("turn-1", 80));
    let collected = monitor.snapshot();
    assert_eq!(collected.result_collected_at_ms, Some(70));
    assert_eq!(collected.progress_sequence, terminal_sequence);
    assert_eq!(collected.last_activity_at_ms, Some(60));
}

#[test]
fn child_monitor_request_wait_budget_does_not_mark_a_child_stopped() {
    use super::SamplingTimingEvent;
    let mut monitor = ChildMonitor::default();
    monitor.observe(
        "turn-1",
        &started_event("turn-1"),
        /*observed_at_ms*/ 100,
    );
    monitor.observe_timing(
        "turn-1",
        SamplingTimingEvent::SeriesStarted,
        /*at_ms*/ 100,
    );
    monitor.observe_timing(
        "turn-1",
        SamplingTimingEvent::RequestStarted {
            stream_idle_timeout_ms: 50,
        },
        /*at_ms*/ 110,
    );
    let before_read = monitor.snapshot();
    let waiting = monitor.snapshot_at(200);
    assert_eq!(waiting.timing.first_output_wait_ms, Some(90));
    assert_eq!(waiting.phase, ChildMonitorPhase::Pending);
    assert_eq!(monitor.snapshot(), before_read);
    monitor.observe(
        "stale-turn",
        &EventMsg::TurnAborted(codex_protocol::protocol::TurnAbortedEvent {
            turn_id: Some("stale-turn".to_string()),
            reason: codex_protocol::protocol::TurnAbortReason::Interrupted,
            started_at: None,
            completed_at: None,
            duration_ms: None,
        }),
        /*observed_at_ms*/ 205,
    );
    assert_eq!(
        monitor
            .snapshot_at(206)
            .timing
            .current_request_finished_at_ms,
        None
    );
    monitor.observe_timing(
        "turn-1",
        SamplingTimingEvent::RequestFinished,
        /*at_ms*/ 210,
    );
    assert_eq!(
        monitor.snapshot_at(900).timing.first_output_wait_ms,
        Some(100)
    );
    monitor.observe_timing(
        "stale-turn",
        SamplingTimingEvent::SeriesFinished,
        /*at_ms*/ 950,
    );
    assert_eq!(
        monitor
            .snapshot_at(1000)
            .timing
            .current_request_finished_at_ms,
        Some(210)
    );
}
