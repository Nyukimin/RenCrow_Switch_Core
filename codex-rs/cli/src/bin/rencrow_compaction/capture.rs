// Modified by RenCrow Switch Core, 2026-09-22.
//! Conservative import of linear response history, not a live checkpoint replayer.
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use anyhow::ensure;
use codex_history::compaction_candidate::CandidateInput;
use codex_history::compaction_candidate::CandidateRecord;
use codex_history::compaction_candidate::Origin;
use codex_history::compaction_candidate::digest;
use serde_json::Value;

pub(super) fn capture(data: &[u8]) -> Result<CandidateInput> {
    let mut records = Vec::new();
    for (index, line) in data.split(|b| *b == b'\n').enumerate() {
        if line.is_empty() {
            continue;
        }
        let value: Value =
            serde_json::from_slice(line).context("incomplete or invalid rollout line")?;
        match value["type"].as_str() {
            Some("response_item") => {}
            Some(
                "session_meta" | "turn_context" | "token_usage_record" | "security_risk_score",
            ) => continue,
            Some("event_msg") if value["payload"]["type"] != "thread_rolled_back" => continue,
            _ => bail!(
                "unsupported rollout replay item at line {}; use an owner-reconstructed snapshot",
                index + 1
            ),
        }
        let payload = &value["payload"];
        ensure!(
            payload.is_object(),
            "response_item payload must be an object"
        );
        let assistant_text = payload["type"] == "message"
            && payload["role"] == "assistant"
            && payload["content"].as_array().is_some_and(|content| {
                !content.is_empty()
                    && content.iter().all(|part| {
                        matches!(part["type"].as_str(), Some("output_text" | "input_text"))
                            && part["text"].is_string()
                    })
            });
        // Sibling metadata holds acceptance order, inherited context and other host facts.
        // Never discard it or infer a human author from one of these fields.
        let has_metadata = value
            .get("metadata")
            .is_some_and(|metadata| !metadata.is_null());
        records.push(CandidateRecord {
            id: format!("line-{index}"),
            origin: if assistant_text {
                Origin::Work
            } else {
                Origin::Unknown
            },
            intake_ref: None,
            scope: "legacy".into(),
            role: payload["role"].as_str().unwrap_or("unknown").into(),
            text: serde_json::to_string(payload)?,
            protected: vec![],
            execution_evidence: false,
            opaque: if !assistant_text || has_metadata {
                Some(value.clone())
            } else {
                None
            },
        });
    }
    ensure!(!records.is_empty(), "rollout has no response items");
    Ok(CandidateInput {
        version: 1,
        binding: digest(&data).map_err(anyhow::Error::msg)?,
        records,
        current_context: vec![],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn preserves_sibling_metadata_and_never_promotes_user_role() {
        let items = [
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"instruction"}]},"metadata":{"user_input_order":1,"inherited_user_message":true}}),
            json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"work"}]},"metadata":{"fallback_token_limit_override":100}}),
            json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"more work"}]}}),
        ];
        let data = items
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        let result = capture(data.as_bytes()).unwrap();
        assert_eq!(result.records[0].origin, Origin::Unknown);
        assert_eq!(result.records[0].opaque, Some(items[0].clone()));
        assert_eq!(result.records[1].origin, Origin::Work);
        assert_eq!(result.records[1].opaque, Some(items[1].clone()));
        assert!(result.records[2].opaque.is_none());
    }

    #[test]
    fn rejects_replay_boundaries_instead_of_reviving_old_history() {
        for value in [
            json!({"type":"compacted","payload":{"message":"summary","replacement_history":[]}}),
            json!({"type":"event_msg","payload":{"type":"thread_rolled_back","num_turns":1}}),
            json!({"type":"retained_context","payload":{}}),
            json!({"type":"inter_agent_communication","payload":{}}),
            json!({"type":"future_history_item","payload":{}}),
        ] {
            assert!(
                capture(value.to_string().as_bytes())
                    .unwrap_err()
                    .to_string()
                    .contains("unsupported rollout replay")
            );
        }
        assert!(capture(b"{incomplete").is_err());
    }
}
