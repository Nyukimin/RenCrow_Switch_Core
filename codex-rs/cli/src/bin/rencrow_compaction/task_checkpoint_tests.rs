use super::CanonicalRow;
use super::latest_raw_task_checkpoint_candidate;
use super::task_checkpoint_following_items;
use codex_history::CompactedItem;
use codex_history::RolloutItem;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

fn compacted_row(ordinal: u64, metadata: Value) -> CanonicalRow {
    CanonicalRow {
        value: json!({
            "ordinal": ordinal,
            "type": "compacted",
            "payload": {"replacement_history_metadata": metadata},
        }),
        item: RolloutItem::Compacted(CompactedItem {
            message: String::new(),
            replacement_history: None,
            guardian_history: None,
            retained_context: None,
            mcp_resource_origins: None,
            window_number: None,
            first_window_id: None,
            previous_window_id: None,
            window_id: None,
            compaction_response_id: None,
            latest_token_usage_record: None,
        }),
        ordinal,
        sha256: "0".repeat(64),
    }
}

#[test]
fn latest_capacity_row_with_embedded_task_boundary_is_selected_and_malformed_target_fails() {
    let earlier = compacted_row(
        4,
        json!([{"rencrow_compaction":{"version":3,"mode":"task_boundary"}}]),
    );
    let latest = compacted_row(
        9,
        json!([
            {"rencrow_compaction":{"version":3,"mode":"task_boundary"}},
            {"rencrow_compaction":{"version":3,"mode":"capacity","task_state":{}}}
        ]),
    );
    let rows = vec![earlier, latest];
    let selected = latest_raw_task_checkpoint_candidate(&rows)
        .expect("latest task checkpoint candidate should be selected");
    assert_eq!(selected, 1);
    assert!(task_checkpoint_following_items(&rows[selected].item).is_err());
}
