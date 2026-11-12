// Modified by RenCrow: persistent, bounded subagent activity in the main TUI.
//! Read cached thread state without blocking the UI or issuing model requests.

use super::App;
use super::ThreadBufferedEvent;
use super::agent_status_feed::activity_summary;
use super::agent_status_feed::bounded_summary;
use crate::chatwidget::ChatWidget;
use crate::text_formatting::truncate_text;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadItem;
use ratatui::buffer::Buffer;
use ratatui::buffer::CellDiffOption;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::widgets::Clear;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;

impl App {
    pub(super) fn agent_panel_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut rows = Vec::new();
        for (thread_id, entry) in self.agent_navigation.ordered_threads() {
            if Some(thread_id) == self.primary_thread_id {
                continue;
            }
            let label = entry
                .agent_path
                .clone()
                .or_else(|| entry.agent_nickname.clone())
                .unwrap_or_else(|| thread_id.to_string());
            let mut status = if entry.is_closed {
                "closed"
            } else if entry.is_running {
                "running"
            } else {
                "idle"
            };
            let mut task = None;
            let mut activity = None;
            if let Some(channel) = self.thread_event_channels.get(&thread_id)
                && let Ok(store) = channel.store.try_lock()
            {
                if !entry.is_closed {
                    if store.active_turn_id().is_some() {
                        status = "running";
                    } else if let Some(turn) = store.turns.last() {
                        status = match turn.status {
                            codex_app_server_protocol::TurnStatus::Completed => "completed",
                            codex_app_server_protocol::TurnStatus::Failed => "failed",
                            codex_app_server_protocol::TurnStatus::Interrupted => "interrupted",
                            codex_app_server_protocol::TurnStatus::InProgress => "running",
                        };
                    }
                }
                let buffered_items = store.buffer.iter().rev().filter_map(|event| match event {
                    ThreadBufferedEvent::Notification(notification) => {
                        match notification.as_ref() {
                            ServerNotification::ItemStarted(event) => Some(&event.item),
                            ServerNotification::ItemCompleted(event) => Some(&event.item),
                            _ => None,
                        }
                    }
                    _ => None,
                });
                for item in buffered_items.chain(
                    store
                        .turns
                        .iter()
                        .rev()
                        .flat_map(|turn| turn.items.iter().rev()),
                ) {
                    if task.is_none()
                        && let ThreadItem::UserMessage { content, .. } = item
                    {
                        task = bounded_summary(
                            &ChatWidget::user_message_display_from_inputs(content).message,
                        );
                    }
                    if activity.is_none() {
                        activity = activity_summary(item);
                    }
                    if task.is_some() && activity.is_some() {
                        break;
                    }
                }
            }
            rows.push(AgentPanelRow {
                label,
                status,
                task,
                activity,
            });
        }
        panel_lines(&rows, width)
    }
}

struct AgentPanelRow {
    label: String,
    status: &'static str,
    task: Option<String>,
    activity: Option<String>,
}

fn panel_lines(rows: &[AgentPanelRow], width: u16) -> Vec<Line<'static>> {
    let running = rows.iter().filter(|row| row.status == "running").count();
    let summary = if rows.is_empty() {
        format!("Subagents: {running} running / 0 total · /subagents details")
    } else {
        let idle = rows.iter().filter(|row| row.status == "idle").count();
        let failed = rows.iter().filter(|row| row.status == "failed").count();
        if width < 60 {
            format!("Subagents: R{running} I{idle} F{failed} · /subagents")
        } else {
            format!("Subagents: {running} running / {idle} idle / {failed} failed · /subagents")
        }
    };
    let mut lines = vec![truncate_text(&summary, usize::from(width)).dim().into()];
    // Completed agents remain available through /subagents, but do not occupy the persistent
    // panel. Keep active work ahead of idle agents when details fit.
    let ordered = rows
        .iter()
        .filter(|row| row.status == "running")
        .chain(rows.iter().filter(|row| row.status == "idle"))
        .collect::<Vec<_>>();
    for row in ordered.iter().take(4) {
        let label = truncate_text(&format!("{} [{}]", row.label, row.status), width as usize);
        lines.push(if row.status == "running" {
            label.green().into()
        } else {
            label.dim().into()
        });
        let detail_width = usize::from(width).saturating_sub(2);
        let task = row.task.as_deref().unwrap_or("Task not received yet");
        let activity = row.activity.as_deref().unwrap_or("No recent activity yet");
        lines.push(
            format!(
                "  {}",
                truncate_text(&format!("Task: {task}"), detail_width)
            )
            .dim()
            .into(),
        );
        lines.push(
            format!(
                "  {}",
                truncate_text(&format!("Latest: {activity}"), detail_width)
            )
            .dim()
            .into(),
        );
    }
    if ordered.len() > 4 {
        lines.push(
            format!("  +{} more · /subagents", ordered.len() - 4)
                .dim()
                .into(),
        );
    }
    lines
}

pub(super) fn fit_panel_lines(lines: &[Line<'static>], max_height: usize) -> Vec<Line<'static>> {
    let Some(summary) = lines.first() else {
        return Vec::new();
    };
    if max_height == 0 {
        return Vec::new();
    }
    if max_height == 1 {
        return vec![summary.clone()];
    }

    let more_count = lines.last().and_then(|line| {
        line.to_string()
            .strip_prefix("  +")
            .and_then(|text| text.strip_suffix(" more · /subagents"))
            .and_then(|count| count.parse::<usize>().ok())
    });
    let detail_end = lines.len() - usize::from(more_count.is_some());
    let available_details = max_height - 1 - usize::from(more_count.is_some());
    let complete_details = (available_details / 3) * 3;
    let mut fitted = Vec::with_capacity(max_height);
    fitted.push(summary.clone());
    fitted.extend(lines[1..detail_end].iter().take(complete_details).cloned());
    if let Some(more_count) = more_count {
        let initially_shown = (detail_end - 1) / 3;
        let shown = (fitted.len() - 1) / 3;
        let remaining = initially_shown + more_count - shown;
        if remaining > 0 && fitted.len() < max_height {
            fitted.push(format!("  +{remaining} more · /subagents").dim().into());
        }
    }
    fitted
}

#[cfg(test)]
#[path = "agent_panel_tests.rs"]
mod tests;

// Raw history insertion and terminal redraws can erase unchanged cells outside the
// diff cache. Repaint the reserved panel on every frame, including its padding.
pub(super) fn render_panel(lines: &[Line<'static>], area: Rect, buffer: &mut Buffer) {
    Clear.render(area, buffer);
    Paragraph::new(lines.to_vec()).render(area, buffer);
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            let cell = &mut buffer[(x, y)];
            if cell.diff_option == CellDiffOption::None {
                cell.set_diff_option(CellDiffOption::AlwaysUpdate);
            }
        }
    }
}
