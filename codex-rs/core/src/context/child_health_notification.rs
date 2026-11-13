// Modified by RenCrow Switch Core, 2026-11-13: bounded host child observations.
use super::ContextualUserFragment;
use crate::agent::child_monitor::health::ChildHealthNotice;
use codex_protocol::ThreadId;
use codex_protocol::models::ContentItemKind;

#[derive(Debug, Clone)]
pub(crate) struct ChildHealthNotification {
    child: ThreadId,
    notice: ChildHealthNotice,
}

impl ChildHealthNotification {
    pub(crate) fn new(child: ThreadId, mut notice: ChildHealthNotice) -> Self {
        notice.turn_id = notice.turn_id.map(|id| id.chars().take(160).collect());
        notice.reason_code = notice.reason_code.chars().take(160).collect();
        Self { child, notice }
    }
}

impl ContextualUserFragment for ChildHealthNotification {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("multi_agent.child_health".to_string())
    }

    fn role(&self) -> &'static str {
        "user"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        (
            "<child_health_notification>",
            "</child_health_notification>",
        )
    }

    fn body(&self) -> String {
        serde_json::json!({
            "origin": "host_child_monitor",
            "child_thread_id": self.child,
            "observation": self.notice,
            "guidance": "Inspect status and available results. Elapsed time does not prove termination. Collection unconfirmed means no explicit collection receipt was observed. The parent decides whether changed instructions, resumption, or redelegation are needed."
        }).to_string()
    }
}

#[cfg(test)]
#[path = "child_health_notification_tests.rs"]
mod tests;
