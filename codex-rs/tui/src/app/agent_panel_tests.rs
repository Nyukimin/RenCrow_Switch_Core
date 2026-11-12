// Modified by RenCrow: snapshot coverage for persistent subagent activity.
use super::*;

#[test]
fn panel_shows_running_work_before_idle_agents() {
    let rows = vec![
        AgentPanelRow {
            label: "/root/review".into(),
            status: "idle",
            task: Some("Review changes".into()),
            activity: Some("Review complete".into()),
        },
        AgentPanelRow {
            label: "/root/tests".into(),
            status: "running",
            task: Some("Check TUI".into()),
            activity: Some("$ just test -p codex-tui".into()),
        },
    ];
    let text = panel_lines(&rows, 80)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(text, @r###"
    Subagents: 1 running / 1 idle / 0 failed · /subagents
    /root/tests [running]
      Task: Check TUI
      Latest: $ just test -p codex-tui
    /root/review [idle]
      Task: Review changes
      Latest: Review complete
    "###);
}

#[test]
fn empty_panel_explicitly_shows_no_running_agents() {
    let text = panel_lines(&[], 80)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(text, @"Subagents: 0 running / 0 total · /subagents details");
}

#[test]
fn narrow_summary_keeps_all_counts_visible() {
    let rows = vec![
        AgentPanelRow {
            label: "/root/a".into(),
            status: "running",
            task: None,
            activity: None,
        },
        AgentPanelRow {
            label: "/root/b".into(),
            status: "idle",
            task: None,
            activity: None,
        },
        AgentPanelRow {
            label: "/root/c".into(),
            status: "failed",
            task: None,
            activity: None,
        },
    ];
    insta::assert_snapshot!(
        panel_lines(&rows, 40)[0].to_string(),
        @"Subagents: R1 I1 F1 · /subagents"
    );
}

#[test]
fn panel_budget_keeps_summary_and_complete_running_rows() {
    let rows = (0..6)
        .map(|index| AgentPanelRow {
            label: format!("/root/agent-{index}"),
            status: "running",
            task: Some("Check narrow layout".into()),
            activity: Some("Working".into()),
        })
        .chain([AgentPanelRow {
            label: "/root/done".into(),
            status: "completed",
            task: Some("Finished task".into()),
            activity: Some("Done".into()),
        }])
        .collect::<Vec<_>>();
    let lines = panel_lines(&rows, 80);
    let narrow = fit_panel_lines(&lines, 1);
    assert_eq!(narrow.len(), 1);
    let desktop = fit_panel_lines(&lines, 10);
    assert!(desktop.len() <= 10);
    assert!(
        desktop
            .iter()
            .any(|line| line.to_string() == "  +4 more · /subagents")
    );
    assert!(
        desktop
            .iter()
            .all(|line| !line.to_string().contains("[completed]"))
    );
}

#[tokio::test]
async fn panel_tracks_cached_work_and_terminal_state_without_switching_threads() {
    use codex_app_server_protocol::ItemCompletedNotification;
    use codex_app_server_protocol::Turn;
    use codex_app_server_protocol::TurnStatus;
    use codex_app_server_protocol::UserInput;
    use codex_protocol::ThreadId;
    let mut app = super::super::test_support::make_test_app().await;
    let parent = ThreadId::new();
    let child = ThreadId::new();
    app.primary_thread_id = Some(parent);
    app.agent_navigation.upsert(
        parent, /*agent_nickname*/ None, /*agent_role*/ None, /*is_closed*/ false,
    );
    app.agent_navigation
        .record_sub_agent_activity(crate::multi_agents::SubAgentActivityDisplay {
            thread_id: child,
            agent_path: "/root/check".into(),
            is_running_hint: true,
        });
    let channel = app.ensure_thread_channel(child);
    {
        let mut store = channel.store.lock().await;
        store.active_turn_id = Some("turn-1".into());
        for item in [
            ThreadItem::UserMessage {
                id: "task".into(),
                client_id: None,
                content: vec![UserInput::Text {
                    text: "Check the activity panel".into(),
                    text_elements: vec![],
                }],
            },
            ThreadItem::AgentMessage {
                id: "activity".into(),
                text: "Checking narrow terminals".into(),
                phase: None,
                memory_citation: None,
                delivery: None,
                questions: None,
            },
        ] {
            store.push_notification(ServerNotification::ItemCompleted(
                ItemCompletedNotification {
                    thread_id: child.to_string(),
                    turn_id: "turn-1".into(),
                    item,
                    completed_at_ms: 1,
                },
            ));
        }
    }
    let text = app
        .agent_panel_lines(100)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(text, @r###"
    Subagents: 1 running / 0 idle / 0 failed · /subagents
    /root/check [running]
      Task: Check the activity panel
      Latest: Checking narrow terminals
    "###);
    {
        let mut store = app
            .thread_event_channels
            .get(&child)
            .unwrap()
            .store
            .lock()
            .await;
        store.active_turn_id = None;
        store.turns.push(Turn {
            id: "turn-1".into(),
            items: vec![],
            items_view: codex_app_server_protocol::TurnItemsView::Full,
            status: TurnStatus::Completed,
            error: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
        });
    }
    let text = app
        .agent_panel_lines(100)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("0 running / 0 idle / 0 failed"));
    assert!(!text.contains("[completed]"));
    app.agent_navigation.mark_closed(child);
    let text = app
        .agent_panel_lines(100)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!text.contains("[closed]"));
}

#[tokio::test]
async fn owned_screen_keeps_agent_panel_above_the_composer() -> color_eyre::eyre::Result<()> {
    use codex_protocol::ThreadId;
    use ratatui::layout::Size;
    let mut app = super::super::test_support::make_test_app().await;
    app.local_settings.tui.animations = false;
    app.primary_thread_id = Some(ThreadId::new());
    app.agent_navigation
        .record_sub_agent_activity(crate::multi_agents::SubAgentActivityDisplay {
            thread_id: ThreadId::new(),
            agent_path: "/root/check".into(),
            is_running_hint: true,
        });
    app.transcript_cells.push(std::sync::Arc::new(
        crate::history_cell::PlainHistoryCell::new(vec!["TRANSCRIPT_BODY_VISIBLE".into()]),
    ));
    let mut tui = crate::tui::test_support::make_test_tui()?;
    tui.set_owned_screen(/*owned*/ true)?;
    let mut narrow_panel = String::new();
    let mut desktop_panel = String::new();
    for size in [
        Size::new(80, 24),
        Size::new(40, 20),
        Size::new(40, 12),
        Size::new(80, 40),
        Size::new(80, 24),
    ] {
        tui.screen_size_for_event(&crate::tui::TuiEvent::Resize(size))?;
        let bottom = app.render_owned_transcript(&mut tui, size)?;
        let buffer = crate::custom_terminal::test_support::last_rendered_buffer(&tui.terminal);
        let rows = buffer
            .content
            .chunks(usize::from(buffer.area.width))
            .map(|row| {
                row.iter()
                    .map(ratatui::buffer::Cell::symbol)
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        let header = rows
            .iter()
            .position(|line| line.contains("Subagents:"))
            .expect("panel remains visible");
        assert!(
            header >= 1,
            "transcript body row must remain above the panel"
        );
        assert!(
            rows.iter()
                .any(|line| line.contains("TRANSCRIPT_BODY_VISIBLE"))
        );
        assert!(header < usize::from(bottom.y));
        assert!(tui.terminal.last_known_cursor_pos.y >= bottom.y);
        if size.width == 40 {
            assert_eq!(bottom.y.saturating_sub(header as u16), 1);
            narrow_panel = rows[header..usize::from(bottom.y)]
                .iter()
                .map(|line| line.trim_end())
                .collect::<Vec<_>>()
                .join("\n");
        } else if size.height == 40 {
            assert!(usize::from(bottom.y).saturating_sub(header) <= 10);
            let panel = rows[header..usize::from(bottom.y)]
                .iter()
                .map(|line| line.trim_end())
                .collect::<Vec<_>>()
                .join("\n");
            desktop_panel = panel;
        }
    }
    insta::assert_snapshot!(narrow_panel, @"Subagents: R1 I0 F0 · /subagents");
    insta::assert_snapshot!(desktop_panel, @r###"
            Subagents: 1 running / 0 idle / 0 failed · /subagents
            /root/check [running]
              Task: Task not received yet
              Latest: No recent activity yet
            "###);
    Ok(())
}

#[test]
fn unchanged_panel_reappears_after_terminal_content_is_erased() {
    use ratatui::layout::Rect;
    use std::io::Write;
    let area = Rect::new(0, 0, 80, 5);
    let mut terminal = crate::custom_terminal::Terminal::with_options(
        crate::test_backend::VT100Backend::new(80, 5),
    )
    .expect("terminal");
    terminal.set_viewport_area(area);
    let lines = panel_lines(
        &[AgentPanelRow {
            label: "CORE explorer".into(),
            status: "running",
            task: Some("Inspect Image tab".into()),
            activity: Some("Read registrar.go".into()),
        }],
        80,
    );
    terminal
        .draw(|frame| render_panel(&lines, area, frame.buffer_mut()))
        .expect("initial draw");
    let initial = terminal.backend().vt100().screen().contents();
    for _ in 0..3 {
        write!(terminal.backend_mut(), "\x1b[2J").expect("simulate external erase");
        assert!(
            !terminal
                .backend()
                .vt100()
                .screen()
                .contents()
                .contains("CORE explorer")
        );
        terminal
            .draw(|frame| render_panel(&lines, area, frame.buffer_mut()))
            .expect("repeat draw");
        pretty_assertions::assert_eq!(terminal.backend().vt100().screen().contents(), initial);
    }
}
