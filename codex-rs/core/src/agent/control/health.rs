use super::LocalAgentControl;
use crate::TurnStartOptions;
use crate::agent::AgentStatus;
use crate::agent::child_monitor::ChildMonitorSnapshot;
use crate::agent::child_monitor::health::ChildHealthNotice;
use crate::agent::child_monitor::health::ChildHealthNoticeKind;
use crate::agent::child_monitor::health::ChildHealthNotifications;
use crate::agent::child_monitor::health::evaluate_child_health;
use crate::agent_communication::AgentCommunicationContext;
use crate::agent_communication::AgentCommunicationKind;
use crate::codex_thread::CodexThread;
use crate::config::Config;
use crate::context::ChildHealthNotification;
use crate::context::ContextualUserFragment;
use codex_features::Feature;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::MultiAgentVersion;
use std::collections::HashMap;
use std::collections::VecDeque;

const MAX_FAILED_DELIVERIES: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct NoticeKey {
    kind: ChildHealthNoticeKind,
    turn_id: Option<String>,
    progress_sequence: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum StatusKind {
    PendingInit,
    Running,
    Interrupted,
    Completed,
    Errored,
    Shutdown,
    NotFound,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ParentGeneration {
    turn_id: Option<String>,
    status: StatusKind,
}

impl ParentGeneration {
    fn new(turn_id: Option<String>, status: &AgentStatus) -> Self {
        let status = match status {
            AgentStatus::PendingInit => StatusKind::PendingInit,
            AgentStatus::Running => StatusKind::Running,
            AgentStatus::Interrupted => StatusKind::Interrupted,
            AgentStatus::Completed(_) => StatusKind::Completed,
            AgentStatus::Errored(_) => StatusKind::Errored,
            AgentStatus::Shutdown => StatusKind::Shutdown,
            AgentStatus::NotFound => StatusKind::NotFound,
        };
        Self { turn_id, status }
    }
}

/// Delivery memory is local to one child watcher and bounded across generations.
#[derive(Debug, Default)]
pub(crate) struct HealthDeliveryState {
    sent: ChildHealthNotifications,
    failed: HashMap<NoticeKey, ParentGeneration>,
    failed_order: VecDeque<NoticeKey>,
}

impl HealthDeliveryState {
    fn should_attempt(&self, notice: &ChildHealthNotice, parent: &ParentGeneration) -> bool {
        let key = NoticeKey::from(notice);
        self.sent.should_send(notice) && self.failed.get(&key) != Some(parent)
    }

    fn failed(&mut self, notice: &ChildHealthNotice, parent: ParentGeneration) {
        let key = NoticeKey::from(notice);
        if !self.failed.contains_key(&key) {
            self.failed_order.push_back(key.clone());
        }
        self.failed.insert(key, parent);
        while self.failed.len() > MAX_FAILED_DELIVERIES {
            if let Some(oldest) = self.failed_order.pop_front() {
                self.failed.remove(&oldest);
            }
        }
    }

    fn accepted(&mut self, notice: &ChildHealthNotice) {
        self.sent.sent(notice);
    }
}

impl From<&ChildHealthNotice> for NoticeKey {
    fn from(notice: &ChildHealthNotice) -> Self {
        Self {
            kind: notice.kind,
            turn_id: notice.turn_id.clone(),
            progress_sequence: notice.progress_sequence,
        }
    }
}

pub(crate) async fn enabled_for_parent_and_child(
    control: &LocalAgentControl,
    parent_thread_id: ThreadId,
    child_thread_id: ThreadId,
) -> bool {
    let Ok(manager) = control.upgrade() else {
        return false;
    };
    let Ok(parent) = manager.get_thread(parent_thread_id).await else {
        return false;
    };
    if !config_enabled(parent.config().await.as_ref()) {
        return false;
    }
    match manager.get_thread(child_thread_id).await {
        Ok(child) => config_enabled(child.config().await.as_ref()),
        Err(_) => true,
    }
}

fn config_enabled(config: &Config) -> bool {
    config.features.enabled(Feature::RencrowChildMonitor)
}

pub(crate) fn notices_for_status(
    snapshot: &ChildMonitorSnapshot,
    status: &AgentStatus,
    now_ms: i64,
) -> Vec<ChildHealthNotice> {
    let notices = evaluate_child_health(snapshot, now_ms);
    match status {
        AgentStatus::PendingInit | AgentStatus::Running => notices
            .into_iter()
            .filter(|notice| notice.kind != ChildHealthNoticeKind::CollectionUnconfirmed)
            .collect(),
        AgentStatus::Completed(_) | AgentStatus::Errored(_) => notices
            .into_iter()
            .filter(|notice| notice.kind == ChildHealthNoticeKind::CollectionUnconfirmed)
            .collect(),
        AgentStatus::Interrupted | AgentStatus::Shutdown | AgentStatus::NotFound => Vec::new(),
    }
}

pub(crate) fn observation_unavailable(
    turn_id: Option<String>,
    progress_sequence: u64,
    now_ms: i64,
    reason_code: &'static str,
) -> ChildHealthNotice {
    ChildHealthNotice {
        kind: ChildHealthNoticeKind::ObservationUnavailable,
        turn_id,
        phase: crate::agent::child_monitor::ChildMonitorPhase::Unknown,
        reason_code: reason_code.to_string(),
        observed_at_ms: now_ms,
        age_ms: None,
        progress_sequence,
    }
}

pub(crate) async fn observe(
    control: &LocalAgentControl,
    parent_thread_id: ThreadId,
    child_thread_id: ThreadId,
    child_thread: &CodexThread,
    status: &AgentStatus,
    delivery: &mut HealthDeliveryState,
) {
    let snapshot = child_thread.session.child_monitor_snapshot().await;
    let notices = match (status, snapshot) {
        (AgentStatus::PendingInit | AgentStatus::Running, Some(mut snapshot)) => {
            snapshot.timing.child_work_deadline_at_ms = None;
            if let Some(child_turn_id) = snapshot.turn_id.as_deref()
                && let Ok(manager) = control.upgrade()
                && let Ok(parent_thread) = manager.get_thread(parent_thread_id).await
            {
                snapshot.timing.child_work_deadline_at_ms = parent_thread
                    .session
                    .completion_review_deadline_for_turn(child_thread_id, child_turn_id)
                    .await;
            }
            notices_for_status(
                &snapshot,
                status,
                crate::turn_timing::now_unix_timestamp_ms(),
            )
        }
        (AgentStatus::PendingInit | AgentStatus::Running, None) => vec![observation_unavailable(
            None,
            0,
            crate::turn_timing::now_unix_timestamp_ms(),
            "child_monitor_snapshot_unavailable",
        )],
        (AgentStatus::Completed(_) | AgentStatus::Errored(_), Some(snapshot)) => {
            notices_for_status(
                &snapshot,
                status,
                crate::turn_timing::now_unix_timestamp_ms(),
            )
        }
        (AgentStatus::Completed(_) | AgentStatus::Errored(_), None)
        | (AgentStatus::Interrupted | AgentStatus::Shutdown | AgentStatus::NotFound, _) => {
            Vec::new()
        }
    };
    for notice in notices {
        deliver(control, parent_thread_id, child_thread_id, notice, delivery).await;
    }
}

pub(crate) async fn deliver(
    control: &LocalAgentControl,
    parent_thread_id: ThreadId,
    child_thread_id: ThreadId,
    notice: ChildHealthNotice,
    delivery: &mut HealthDeliveryState,
) {
    let Ok(manager) = control.upgrade() else {
        return;
    };
    let Ok(parent_thread) = manager.get_thread(parent_thread_id).await else {
        return;
    };
    let (parent_turn_id, parent_status) = parent_thread.session.agent_status_generation().await;
    let parent_generation = ParentGeneration::new(parent_turn_id, &parent_status);
    if !delivery.should_attempt(&notice, &parent_generation) {
        return;
    }
    let notification = ChildHealthNotification::new(child_thread_id, notice.clone());
    if parent_thread.multi_agent_version() == Some(MultiAgentVersion::V2) {
        let parent_path = control
            .get_agent_metadata(parent_thread_id)
            .and_then(|metadata| metadata.agent_path)
            .unwrap_or_else(AgentPath::root);
        let communication = InterAgentCommunication::new(
            parent_path.clone(),
            parent_path,
            Vec::new(),
            notification.render(),
            /*trigger_turn*/ false,
        );
        let context =
            AgentCommunicationContext::new(AgentCommunicationKind::Message, parent_thread_id);
        if control
            .send_inter_agent_communication(
                parent_thread_id,
                communication,
                context,
                TurnStartOptions::default(),
            )
            .await
            .is_ok()
        {
            delivery.accepted(&notice);
        } else {
            delivery.failed(&notice, parent_generation);
        }
    } else {
        parent_thread
            .inject_fragment_without_turn(notification)
            .await;
        delivery.accepted(&notice);
    }
}

#[cfg(test)]
#[path = "health_tests.rs"]
mod tests;
