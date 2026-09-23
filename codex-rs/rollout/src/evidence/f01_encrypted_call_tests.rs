//! Unwired F01 regression: opaque function-call arguments must prevent tool-pair removal.

use super::*;
use codex_history::ResponseItemEnvelope;
use codex_history::RolloutItem;
use codex_protocol::ThreadId;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use std::collections::HashSet;

#[test]
fn fresh_function_pairs_with_encrypted_arguments_remain_protected() {
    let thread = ThreadId::from_u128(81_001);
    for encrypted_function_args in [Some(Vec::new()), Some(vec!["opaque-arg".into()])] {
        let call = ResponseItemEnvelope::new(ResponseItem::FunctionCall {
            id: None,
            name: "read_document".into(),
            namespace: None,
            arguments: "{\"path\":\"a\"}".into(),
            encrypted_function_args,
            call_id: "opaque-call".into(),
            internal_chat_message_metadata_passthrough: None,
        });
        let output = ResponseItemEnvelope::new(ResponseItem::FunctionCallOutput {
            id: None,
            call_id: Some("opaque-call".into()),
            name: Some("read_document".into()),
            namespace: None,
            output: FunctionCallOutputPayload::from_text("returned text".into()),
            internal_chat_message_metadata_passthrough: None,
        });
        // The selected and canonical snapshots carry exactly the same opaque call bytes.
        let selected = vec![call.clone(), output.clone()];
        let canonical = vec![
            RolloutItem::ResponseItem(call),
            RolloutItem::ResponseItem(output),
        ];

        let prepared = prepare_compaction_sources(&selected, &canonical, &thread, &HashSet::new())
            .expect("ordinary text output can still be inventoried safely");

        std::assert!(prepared.pairs.is_empty());
        std::assert_eq!(prepared.protected_indices, vec![0, 1]);
    }
}
