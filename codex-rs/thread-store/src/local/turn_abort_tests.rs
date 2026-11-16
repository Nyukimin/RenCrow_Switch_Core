// Added by RenCrow Switch Core, 2026-11-16: cover persisted turn-abort ownership and bounds.
use std::fs;
use std::io::Write;
use std::path::Path;

use codex_protocol::ThreadId;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::HistoryPosition;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnAbortedEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_rollout::RolloutItem;
use codex_rollout::RolloutLine;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

use super::super::LocalThreadStore;
use super::super::test_support::test_config;

#[tokio::test]
async fn reads_exact_abort_event_from_legacy_rollout() {
    let home = TempDir::new().expect("temp dir");
    let thread_id = ThreadId::default();
    write_rollout(
        home.path(),
        thread_id,
        thread_id,
        ThreadHistoryMode::Legacy,
        /*history_base*/ None,
        vec![turn_aborted("review-turn", TurnAbortReason::ReviewEnded)],
    );
    let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);

    let evidence = super::read(&store, thread_id, "review-turn")
        .await
        .expect("read legacy abort event");
    assert_eq!(
        evidence
            .as_ref()
            .map(|evidence| evidence.event.turn_id.as_deref()),
        Some(Some("review-turn"))
    );
    let evidence = evidence.expect("abort evidence exists");
    assert_eq!(evidence.owner.thread_id, thread_id);
    assert_eq!(
        evidence.source.locator,
        super::super::super::PersistedTurnAbortLocator::Ordinal(1)
    );
    assert!(
        super::read(&store, thread_id, "other-turn")
            .await
            .expect("wrong turn is absent")
            .is_none()
    );
}

#[tokio::test]
async fn reads_only_current_paginated_segment_and_ignores_parent_and_cutoff_evidence() {
    let home = TempDir::new().expect("temp dir");
    let parent_id = ThreadId::default();
    let thread_id = ThreadId::default();
    let parent_path = write_rollout(
        home.path(),
        parent_id,
        parent_id,
        ThreadHistoryMode::Paginated,
        /*history_base*/ None,
        vec![turn_aborted("review-turn", TurnAbortReason::ReviewEnded)],
    );
    let cutoff = history_position(
        parent_path.as_path(),
        parent_id,
        /*end_ordinal_exclusive*/ 1,
    );
    write_rollout(
        home.path(),
        thread_id,
        thread_id,
        ThreadHistoryMode::Paginated,
        Some(cutoff),
        vec![turn_aborted("review-turn", TurnAbortReason::Interrupted)],
    );
    let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);

    let event = super::read(&store, thread_id, "review-turn")
        .await
        .expect("read current paginated segment")
        .expect("current segment event exists");
    assert_eq!(event.event.reason, TurnAbortReason::Interrupted);
}

#[tokio::test]
async fn historical_abort_remains_readable_after_a_later_turn() {
    let home = TempDir::new().expect("temp dir");
    let thread_id = ThreadId::default();
    write_rollout(
        home.path(),
        thread_id,
        thread_id,
        ThreadHistoryMode::Legacy,
        /*history_base*/ None,
        vec![
            turn_aborted("review-turn", TurnAbortReason::ReviewEnded),
            turn_complete("later-turn"),
        ],
    );
    let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);

    let evidence = super::read(&store, thread_id, "review-turn")
        .await
        .expect("read historical abort despite later turn")
        .expect("historical abort exists");
    assert_eq!(evidence.event.turn_id.as_deref(), Some("review-turn"));
    assert_eq!(
        evidence.source.locator,
        super::super::super::PersistedTurnAbortLocator::Ordinal(1)
    );
}

#[tokio::test]
async fn legacy_record_without_ordinal_uses_distinct_line_index_locator() {
    let home = TempDir::new().expect("temp dir");
    let thread_id = ThreadId::default();
    write_legacy_rollout_without_ordinals(
        home.path(),
        thread_id,
        vec![turn_aborted("review-turn", TurnAbortReason::ReviewEnded)],
    );
    let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);

    let evidence = super::read(&store, thread_id, "review-turn")
        .await
        .expect("read legacy event without ordinal")
        .expect("legacy abort exists");
    assert_eq!(
        evidence.source.locator,
        super::super::super::PersistedTurnAbortLocator::LegacyLineIndex(1)
    );
}

#[tokio::test]
async fn conflicting_terminal_event_for_exact_turn_fails_closed() {
    let home = TempDir::new().expect("temp dir");
    let thread_id = ThreadId::default();
    write_rollout(
        home.path(),
        thread_id,
        thread_id,
        ThreadHistoryMode::Legacy,
        /*history_base*/ None,
        vec![
            turn_aborted("review-turn", TurnAbortReason::ReviewEnded),
            turn_complete("review-turn"),
        ],
    );
    let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);

    assert!(super::read(&store, thread_id, "review-turn").await.is_err());
}

#[tokio::test]
async fn duplicate_abort_records_for_exact_turn_fail_closed() {
    let home = TempDir::new().expect("temp dir");
    let thread_id = ThreadId::default();
    write_rollout(
        home.path(),
        thread_id,
        thread_id,
        ThreadHistoryMode::Legacy,
        /*history_base*/ None,
        vec![
            turn_aborted("review-turn", TurnAbortReason::ReviewEnded),
            turn_aborted("review-turn", TurnAbortReason::ReviewEnded),
        ],
    );
    let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);

    assert!(super::read(&store, thread_id, "review-turn").await.is_err());
}

#[tokio::test]
async fn reads_bounded_segment_owned_by_same_thread() {
    let home = TempDir::new().expect("temp dir");
    let thread_id = ThreadId::default();
    let prior_rollout_id = ThreadId::default();
    let prior_path = write_rollout(
        home.path(),
        prior_rollout_id,
        thread_id,
        ThreadHistoryMode::Paginated,
        /*history_base*/ None,
        vec![turn_aborted("review-turn", TurnAbortReason::ReviewEnded)],
    );
    let history_base = history_position(
        prior_path.as_path(),
        prior_rollout_id,
        /*end_ordinal_exclusive*/ 2,
    );
    write_rollout(
        home.path(),
        thread_id,
        thread_id,
        ThreadHistoryMode::Paginated,
        Some(history_base),
        vec![],
    );
    let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);

    let event = super::read(&store, thread_id, "review-turn")
        .await
        .expect("read bounded same-owner segment")
        .expect("abort event is within the bounded segment");
    assert_eq!(event.event.reason, TurnAbortReason::ReviewEnded);
}

#[tokio::test]
async fn excludes_event_at_or_after_same_owner_segment_cutoff() {
    let home = TempDir::new().expect("temp dir");
    let thread_id = ThreadId::default();
    let prior_rollout_id = ThreadId::default();
    let prior_path = write_rollout(
        home.path(),
        prior_rollout_id,
        thread_id,
        ThreadHistoryMode::Paginated,
        /*history_base*/ None,
        vec![turn_aborted("review-turn", TurnAbortReason::ReviewEnded)],
    );
    let history_base = history_position(
        prior_path.as_path(),
        prior_rollout_id,
        /*end_ordinal_exclusive*/ 1,
    );
    write_rollout(
        home.path(),
        thread_id,
        thread_id,
        ThreadHistoryMode::Paginated,
        Some(history_base),
        vec![],
    );
    let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);

    assert!(
        super::read(&store, thread_id, "review-turn")
            .await
            .expect("read bounded history before excluded event")
            .is_none()
    );
}

#[tokio::test]
async fn absent_persisted_abort_event_is_not_inferred() {
    let home = TempDir::new().expect("temp dir");
    let thread_id = ThreadId::default();
    write_rollout(
        home.path(),
        thread_id,
        thread_id,
        ThreadHistoryMode::Legacy,
        /*history_base*/ None,
        vec![],
    );
    let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);

    assert!(
        super::read(&store, thread_id, "review-turn")
            .await
            .expect("read with no persisted abort")
            .is_none()
    );
}

#[tokio::test]
async fn malformed_record_in_current_persisted_segment_fails_closed() {
    let home = TempDir::new().expect("temp dir");
    let thread_id = ThreadId::default();
    let path = write_rollout(
        home.path(),
        thread_id,
        thread_id,
        ThreadHistoryMode::Legacy,
        /*history_base*/ None,
        vec![turn_aborted("review-turn", TurnAbortReason::ReviewEnded)],
    );
    fs::OpenOptions::new()
        .append(true)
        .open(path)
        .expect("open rollout")
        .write_all(b"{invalid json}\n")
        .expect("append malformed record");
    let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);

    assert!(super::read(&store, thread_id, "review-turn").await.is_err());
}

fn turn_aborted(turn_id: &str, reason: TurnAbortReason) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::TurnAborted(TurnAbortedEvent {
        turn_id: Some(turn_id.to_owned()),
        reason,
        started_at: None,
        completed_at: None,
        duration_ms: None,
    }))
}

fn turn_complete(turn_id: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
        turn_id: turn_id.to_owned(),
        last_agent_message: None,
        error: None,
        started_at: None,
        completed_at: None,
        duration_ms: None,
        time_to_first_token_ms: None,
    }))
}

fn write_rollout(
    home: &Path,
    rollout_id: ThreadId,
    thread_id: ThreadId,
    history_mode: ThreadHistoryMode,
    history_base: Option<HistoryPosition>,
    items: Vec<RolloutItem>,
) -> std::path::PathBuf {
    let directory = home.join("sessions/2026/07/16");
    fs::create_dir_all(directory.as_path()).expect("create rollout directory");
    let path = directory.join(format!("rollout-2026-07-16T00-00-00-{rollout_id}.jsonl"));
    let initial_ordinal = history_base.map_or(0, |base| base.end_ordinal_exclusive);
    let mut lines = vec![rollout_line(
        Some(initial_ordinal),
        RolloutItem::SessionMeta(SessionMetaLine {
            meta: SessionMeta {
                session_id: thread_id.into(),
                id: thread_id,
                history_mode,
                history_base,
                ..SessionMeta::default()
            },
            git: None,
        }),
    )];
    for (index, item) in items.into_iter().enumerate() {
        let ordinal = initial_ordinal + u64::try_from(index).expect("index fits u64") + 1;
        lines.push(rollout_line(Some(ordinal), item));
    }
    fs::write(path.as_path(), format!("{}\n", lines.join("\n"))).expect("write rollout");
    path
}

fn write_legacy_rollout_without_ordinals(
    home: &Path,
    thread_id: ThreadId,
    items: Vec<RolloutItem>,
) -> std::path::PathBuf {
    let directory = home.join("sessions/2026/07/16");
    fs::create_dir_all(directory.as_path()).expect("create rollout directory");
    let path = directory.join(format!("rollout-2026-07-16T00-00-00-{thread_id}.jsonl"));
    let mut lines = vec![rollout_line(
        None,
        RolloutItem::SessionMeta(SessionMetaLine {
            meta: SessionMeta {
                session_id: thread_id.into(),
                id: thread_id,
                history_mode: ThreadHistoryMode::Legacy,
                ..SessionMeta::default()
            },
            git: None,
        }),
    )];
    lines.extend(items.into_iter().map(|item| rollout_line(None, item)));
    fs::write(path.as_path(), format!("{}\n", lines.join("\n")))
        .expect("write legacy rollout without ordinals");
    path
}

fn rollout_line(ordinal: Option<u64>, item: RolloutItem) -> String {
    serde_json::to_string(&RolloutLine {
        timestamp: "2026-07-16T00:00:00.000Z".to_string(),
        ordinal,
        item,
    })
    .expect("serialize rollout line")
}

fn history_position(
    path: &Path,
    rollout_id: ThreadId,
    end_ordinal_exclusive: u64,
) -> HistoryPosition {
    let bytes = fs::read(path).expect("read rollout");
    let end_byte_offset = bytes
        .split_inclusive(|byte| *byte == b'\n')
        .take_while(|line| {
            codex_rollout::parse_rollout_line_bytes(line)
                .expect("parse rollout line")
                .ordinal
                .is_none_or(|ordinal| ordinal < end_ordinal_exclusive)
        })
        .map(<[u8]>::len)
        .sum::<usize>();
    HistoryPosition {
        thread_id: rollout_id,
        end_ordinal_exclusive,
        end_byte_offset: u64::try_from(end_byte_offset).expect("offset fits u64"),
    }
}
