// Modified by RenCrow Switch Core, 2026-11-13: read-only child execution visibility.
use super::LocalAgentControl;
use crate::agent::child_monitor::ChildMonitorSnapshot;
use codex_protocol::ThreadId;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;

const MAX_REPORTED_CHILDREN: usize = 16;
// Keep added monitoring context below the existing tool-output token budget.
const MAX_REPORT_BYTES: usize = 4096;

/// Bounded observations, separate from lifecycle status and completion receipts.
#[derive(Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct ChildMonitorReport {
    pub(crate) children: BTreeMap<String, ChildMonitorSnapshot>,
    pub(crate) omitted_children: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) observation_error: Option<String>,
}

impl LocalAgentControl {
    pub(crate) async fn mark_child_result_collected(
        &self,
        child: ThreadId,
        turn_id: &str,
        at_ms: i64,
    ) {
        if let Ok(manager) = self.upgrade()
            && let Ok(thread) = manager.get_thread(child).await
        {
            thread
                .session
                .mark_child_result_collected(turn_id, at_ms)
                .await;
        }
    }

    pub(crate) async fn get_child_monitor_snapshot(&self, child: ThreadId) -> ChildMonitorSnapshot {
        if let Ok(manager) = self.upgrade()
            && let Ok(thread) = manager.get_thread(child).await
            && let Some(snapshot) = thread.session.child_monitor_snapshot().await
        {
            return snapshot;
        }
        ChildMonitorSnapshot {
            reason: Some("child session unavailable or monitoring disabled".to_string()),
            ..Default::default()
        }
    }

    pub(crate) async fn child_monitor_report(
        &self,
        children: &[(ThreadId, String)],
    ) -> ChildMonitorReport {
        let mut report = ChildMonitorReport {
            omitted_children: children.len().saturating_sub(MAX_REPORTED_CHILDREN),
            ..Default::default()
        };
        let mut reported_bytes = 128;
        for (child, reference) in children.iter().take(MAX_REPORTED_CHILDREN) {
            let snapshot = self.get_child_monitor_snapshot(*child).await;
            let entry_bytes = serde_json::to_vec(&(reference, &snapshot))
                .map_or(MAX_REPORT_BYTES, |entry| entry.len());
            if reported_bytes + entry_bytes > MAX_REPORT_BYTES {
                report.omitted_children += 1;
                continue;
            }
            reported_bytes += entry_bytes;
            report.children.insert(reference.clone(), snapshot);
        }
        report
    }

    pub(crate) async fn open_child_monitor_report(&self, parent: ThreadId) -> ChildMonitorReport {
        let children = match self.open_thread_spawn_children(parent).await {
            Ok(children) => children,
            Err(_) => {
                return ChildMonitorReport {
                    observation_error: Some("child registry unavailable".to_string()),
                    ..Default::default()
                };
            }
        }
        .into_iter()
        .map(|(id, _)| (id, id.to_string()))
        .collect::<Vec<_>>();
        self.child_monitor_report(&children).await
    }
}
