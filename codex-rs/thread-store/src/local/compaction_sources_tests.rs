use std::fs;
use std::path::Path;
use std::time::Duration;

use codex_protocol::ThreadId;
use codex_protocol::items::CommandExecutionItem;
use codex_protocol::items::CommandExecutionStatus;
use codex_protocol::items::TurnItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecCommandSource;
use codex_protocol::protocol::HistoryPosition;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_rollout::RolloutItem;
use codex_rollout::RolloutLine;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

use super::super::LocalThreadStore;
use super::super::test_support::test_config;
use crate::ReadCompactionSourcesParams;

#[tokio::test]
async fn reads_same_thread_source_pairs_respects_cutoffs_and_excludes_foreign_segments() {
    let home = TempDir::new().expect("temp dir");
    let thread_id = ThreadId::default();
    let ancestor_rollout_id = ThreadId::default();
    let foreign_rollout_id = ThreadId::default();
    let foreign_thread_id = ThreadId::default();

    let foreign_path = write_segment(
        home.path(),
        foreign_rollout_id,
        foreign_thread_id,
        None,
        pair("foreign-call", foreign_thread_id),
    );
    let foreign_end = history_position(foreign_path.as_path(), foreign_rollout_id, 4);

    let same_thread_pairs = (0..14)
        .map(|index| pair(&format!("ancestor-{index}"), thread_id))
        .collect::<Vec<_>>();
    let mut ancestor_items = same_thread_pairs.concat();
    ancestor_items.extend(pair("after-cutoff-call", thread_id));
    let ancestor_path = write_segment(
        home.path(),
        ancestor_rollout_id,
        thread_id,
        Some(foreign_end),
        ancestor_items,
    );
    let ancestor_end = history_position(ancestor_path.as_path(), ancestor_rollout_id, 47);
    let ancestor_bytes = fs::read(ancestor_path.as_path()).expect("read ancestor rollout");
    let cutoff = usize::try_from(ancestor_end.end_byte_offset).expect("cutoff fits usize");
    let mut corrupt_after_cutoff = ancestor_bytes[..cutoff].to_vec();
    corrupt_after_cutoff.extend_from_slice(b"{invalid json}\n");
    corrupt_after_cutoff.extend_from_slice(&ancestor_bytes[cutoff..]);
    fs::write(ancestor_path.as_path(), corrupt_after_cutoff).expect("corrupt after cutoff");

    write_segment(
        home.path(),
        thread_id,
        thread_id,
        Some(ancestor_end),
        pair("current-call", thread_id),
    );
    let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
    let call_ids = (0..14)
        .map(|index| format!("ancestor-{index}"))
        .chain([
            "foreign-call".to_owned(),
            "after-cutoff-call".to_owned(),
            "current-call".to_owned(),
        ])
        .collect();

    let history = super::read(
        &store,
        ReadCompactionSourcesParams {
            thread_id,
            call_ids,
        },
    )
    .await
    .expect("read compaction source lineage");

    let returned_call_ids = history
        .items
        .iter()
        .filter_map(item_call_id)
        .collect::<Vec<_>>();
    assert_eq!(history.thread_id, thread_id);
    assert_eq!(history.items.len(), 45);
    assert_eq!(returned_call_ids.len(), 45);
    for index in 0..14 {
        assert_eq!(
            returned_call_ids
                .iter()
                .filter(|call_id| call_id.as_str() == format!("ancestor-{index}"))
                .count(),
            3
        );
    }
    assert!(
        !returned_call_ids
            .iter()
            .any(|call_id| call_id == "foreign-call" || call_id == "after-cutoff-call")
    );
    assert_eq!(
        returned_call_ids
            .iter()
            .filter(|call_id| call_id.as_str() == "current-call")
            .count(),
        3
    );
}

fn pair(call_id: &str, thread_id: ThreadId) -> Vec<RolloutItem> {
    vec![
        RolloutItem::ResponseItem(codex_rollout::ResponseItemEnvelope::new(
            ResponseItem::FunctionCall {
                id: None,
                name: "exec_command".into(),
                namespace: None,
                arguments: "{}".into(),
                encrypted_function_args: None,
                call_id: call_id.into(),
                internal_chat_message_metadata_passthrough: None,
            },
        )),
        RolloutItem::ResponseItem(codex_rollout::ResponseItemEnvelope::new(
            ResponseItem::FunctionCallOutput {
                id: None,
                call_id: Some(call_id.into()),
                name: Some("exec_command".into()),
                namespace: None,
                output: FunctionCallOutputPayload::from_text("complete output".into()),
                internal_chat_message_metadata_passthrough: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
            thread_id,
            turn_id: "turn-1".into(),
            item: TurnItem::CommandExecution(CommandExecutionItem {
                model_context: None,
                id: call_id.into(),
                plugin_id: None,
                script_path: None,
                process_id: Some("process-1".into()),
                command: vec!["exec_command".into()],
                cwd: serde_json::from_value(serde_json::json!("file:///tmp")).unwrap(),
                parsed_cmd: vec![],
                source: ExecCommandSource::UnifiedExecStartup,
                interaction_input: None,
                status: CommandExecutionStatus::Completed,
                stdout: Some("complete output".into()),
                stderr: None,
                aggregated_output: Some("complete output".into()),
                exit_code: Some(0),
                duration: Some(Duration::from_millis(1)),
                formatted_output: Some("complete output".into()),
            }),
            started_at_ms: Some(1),
            completed_at_ms: 2,
        })),
    ]
}

fn write_segment(
    home: &Path,
    rollout_id: ThreadId,
    thread_id: ThreadId,
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
                history_mode: ThreadHistoryMode::Paginated,
                history_base,
                ..SessionMeta::default()
            },
            git: None,
        }),
    )];
    for (index, item) in items.into_iter().enumerate() {
        let ordinal = initial_ordinal + u64::try_from(index).unwrap() + 1;
        lines.push(rollout_line(Some(ordinal), item));
    }
    fs::write(path.as_path(), format!("{}\n", lines.join("\n"))).expect("write rollout");
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
        end_byte_offset: u64::try_from(end_byte_offset).expect("byte offset"),
    }
}

fn item_call_id(item: &RolloutItem) -> Option<String> {
    match item {
        RolloutItem::ResponseItem(envelope) => match &envelope.item {
            ResponseItem::FunctionCall { call_id, .. }
            | ResponseItem::CustomToolCall { call_id, .. }
            | ResponseItem::CustomToolCallOutput { call_id, .. } => Some(call_id.clone()),
            ResponseItem::FunctionCallOutput {
                call_id: Some(call_id),
                ..
            } => Some(call_id.clone()),
            _ => None,
        },
        RolloutItem::EventMsg(EventMsg::ItemCompleted(event)) => match &event.item {
            TurnItem::CommandExecution(command) => Some(command.id.clone()),
            _ => None,
        },
        _ => None,
    }
}
