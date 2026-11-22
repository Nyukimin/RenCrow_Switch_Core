use super::tests::make_session_and_context;
use super::*;
use codex_history::ResumedHistory;
use codex_protocol::config_types::ModeKind;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnAbortedEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnStartedEvent;
use codex_rollout::RolloutItem;
use std::sync::Arc;

fn turn_started(turn_id: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
        turn_id: turn_id.into(),
        root_turn_id: None,
        trace_id: None,
        started_at: None,
        model_context_window: None,
        collaboration_mode_kind: ModeKind::Default,
    }))
}

fn turn_aborted(turn_id: Option<&str>, reason: TurnAbortReason) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::TurnAborted(TurnAbortedEvent {
        turn_id: turn_id.map(str::to_owned),
        started_at: None,
        reason,
        completed_at: None,
        duration_ms: None,
    }))
}

fn turn_completed(turn_id: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
        turn_id: turn_id.into(),
        last_agent_message: Some("done".into()),
        error: None,
        started_at: None,
        completed_at: None,
        duration_ms: None,
        time_to_first_token_ms: None,
    }))
}

async fn resumed_status(items: Vec<RolloutItem>, subagent: bool) -> AgentStatus {
    resumed_status_with_conversation(items, subagent, true).await
}

async fn resumed_status_with_conversation(
    items: Vec<RolloutItem>,
    subagent: bool,
    same_thread: bool,
) -> AgentStatus {
    let (session, _) = make_session_and_context().await;
    let thread_id = session.thread_id();
    let conversation_id = if same_thread {
        thread_id
    } else {
        let other_thread_id = ThreadId::new();
        assert_ne!(other_thread_id, thread_id);
        other_thread_id
    };
    if subagent {
        let parent_thread_id = ThreadId::new();
        session
            .state
            .lock()
            .await
            .session_configuration
            .session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id,
            depth: 1,
            agent_path: None,
            agent_nickname: None,
            agent_role: Some("reviewer".into()),
        });
    }
    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id,
            history: Arc::new(items),
            rollout_path: None,
        }))
        .await;
    session.agent_status_generation().await.1
}

async fn forked_status(items: Vec<RolloutItem>) -> AgentStatus {
    let (session, _) = make_session_and_context().await;
    session
        .record_initial_history(InitialHistory::Forked(items))
        .await;
    session.agent_status_generation().await.1
}

#[tokio::test]
async fn same_thread_subagent_resume_restores_review_ended_for_latest_turn() {
    let status = resumed_status(
        vec![
            turn_started("review-turn"),
            turn_aborted(Some("review-turn"), TurnAbortReason::ReviewEnded),
        ],
        true,
    )
    .await;

    assert_eq!(status, AgentStatus::Errored("ReviewEnded".into()));
}

#[tokio::test]
async fn root_resume_keeps_review_ended_status_unrestored() {
    let status = resumed_status(
        vec![
            turn_started("root-turn"),
            turn_aborted(Some("root-turn"), TurnAbortReason::ReviewEnded),
        ],
        false,
    )
    .await;

    assert_eq!(status, AgentStatus::PendingInit);
}

#[tokio::test]
async fn different_conversation_resume_keeps_review_ended_status_unrestored() {
    let status = resumed_status_with_conversation(
        vec![
            turn_started("review-turn"),
            turn_aborted(Some("review-turn"), TurnAbortReason::ReviewEnded),
        ],
        true,
        false,
    )
    .await;

    assert_eq!(status, AgentStatus::PendingInit);
}

#[tokio::test]
async fn resume_status_preserves_interrupt_and_completed_turn_contracts() {
    let interrupted = resumed_status(
        vec![
            turn_started("interrupted-turn"),
            turn_aborted(Some("interrupted-turn"), TurnAbortReason::Interrupted),
        ],
        true,
    )
    .await;
    assert_eq!(interrupted, AgentStatus::Interrupted);

    let completed = resumed_status(
        vec![
            turn_started("completed-turn"),
            turn_completed("completed-turn"),
        ],
        true,
    )
    .await;
    assert_eq!(completed, AgentStatus::PendingInit);
}

#[tokio::test]
async fn review_ended_resume_does_not_restore_unbound_or_stale_turns() {
    let missing_turn_id = resumed_status(
        vec![
            turn_started("review-turn"),
            turn_aborted(None, TurnAbortReason::ReviewEnded),
        ],
        true,
    )
    .await;
    assert_eq!(missing_turn_id, AgentStatus::PendingInit);

    let missing_generation_and_turn_id =
        resumed_status(vec![turn_aborted(None, TurnAbortReason::ReviewEnded)], true).await;
    assert_eq!(missing_generation_and_turn_id, AgentStatus::PendingInit);

    let delayed_old_abort = resumed_status(
        vec![
            turn_started("old-turn"),
            turn_started("current-turn"),
            turn_aborted(Some("old-turn"), TurnAbortReason::ReviewEnded),
        ],
        true,
    )
    .await;
    assert_eq!(delayed_old_abort, AgentStatus::PendingInit);

    let later_turn = resumed_status(
        vec![
            turn_started("review-turn"),
            turn_aborted(Some("review-turn"), TurnAbortReason::ReviewEnded),
            turn_started("follow-up-turn"),
        ],
        true,
    )
    .await;
    assert_eq!(later_turn, AgentStatus::PendingInit);
}

#[tokio::test]
async fn fork_does_not_restore_review_ended_status() {
    let status = forked_status(vec![
        turn_started("review-turn"),
        turn_aborted(Some("review-turn"), TurnAbortReason::ReviewEnded),
    ])
    .await;

    assert_eq!(status, AgentStatus::PendingInit);
}
