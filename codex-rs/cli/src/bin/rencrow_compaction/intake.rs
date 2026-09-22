// Modified by RenCrow Switch Core, 2026-09-22.
//! Bind separately recorded original input to accepted linear rollout messages.
use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use codex_history::compaction_candidate::CandidateInput;
use codex_history::compaction_candidate::CandidateRecord;
use codex_history::compaction_candidate::Origin;
use codex_history::compaction_candidate::digest;
use codex_history::input_intake::InputAuthor;
use codex_history::input_intake::SubmissionIntake;
use codex_history::input_intake::valid_intake_id;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;

pub(super) fn directory(rollout: &Path, explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = explicit {
        return Some(path.to_owned());
    }
    rollout
        .ancestors()
        .find(|p| {
            p.file_name()
                .is_some_and(|n| n == "sessions" || n == "archived_sessions")
        })
        .and_then(Path::parent)
        .map(|home| home.join("rencrow").join("input-intake"))
}

pub(super) fn apply(data: &[u8], input: &mut CandidateInput, directory: &Path) -> Result<()> {
    let lines = data
        .split(|b| *b == b'\n')
        .enumerate()
        .filter(|(_, line)| !line.is_empty())
        .map(|(i, line)| Ok((i, serde_json::from_slice::<Value>(line)?)))
        .collect::<Result<Vec<_>>>()?;
    let ids = lines
        .iter()
        .filter(|(_, v)| v["type"] == "session_meta")
        .filter_map(|(_, v)| v["payload"]["id"].as_str())
        .collect::<BTreeSet<_>>();
    if ids.is_empty() {
        return Ok(());
    }
    ensure!(ids.len() == 1, "ambiguous intake thread identity");
    let thread_id = ids.first().context("missing thread identity")?;
    ensure!(valid_intake_id(thread_id), "invalid intake thread identity");
    let mut pending = None;
    let mut processed = BTreeSet::new();
    let mut replacements = BTreeMap::new();
    for (index, value) in &lines {
        if value["type"] == "response_item"
            && value["payload"]["type"] == "message"
            && value["payload"]["role"] == "user"
        {
            pending = Some((*index, value));
        }
        if value["type"] != "event_msg" {
            continue;
        }
        let (client_id, accepted) = match value["payload"]["type"].as_str() {
            Some("user_message") => {
                let Some(client_id) = value["payload"]["client_id"].as_str() else {
                    continue;
                };
                (
                    client_id.to_owned(),
                    value["payload"]["message"]
                        .as_str()
                        .context("missing accepted message text")?
                        .to_owned(),
                )
            }
            Some("item_completed") if value["payload"]["item"]["type"] == "UserMessage" => {
                ensure!(
                    value["payload"]["thread_id"].as_str() == Some(*thread_id),
                    "accepted item belongs to a different or missing thread"
                );
                let item: codex_protocol::items::UserMessageItem =
                    serde_json::from_value(value["payload"]["item"].clone())?;
                let Some(client_id) = item.client_id.clone() else {
                    continue;
                };
                (client_id, item.message())
            }
            _ => continue,
        };
        let Some((source_index, source)) = pending.take() else {
            continue;
        };
        ensure!(
            valid_intake_id(&client_id),
            "invalid intake client identity"
        );
        if !processed.insert(client_id.to_owned()) {
            continue;
        }
        let path = directory.join(thread_id).join(format!("{client_id}.json"));
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        ensure!(
            bytes.len() <= 1_048_576,
            "intake receipt exceeds recording limit"
        );
        let receipt: SubmissionIntake =
            serde_json::from_slice(&bytes).context("invalid intake receipt")?;
        ensure!(
            receipt.matches_accepted(thread_id, &client_id, &accepted),
            "intake does not match server acceptance"
        );
        if source["metadata"]["inherited_user_message"] == true
            || !source["metadata"]["sender_user_messages"].is_null()
        {
            continue;
        }
        let id = format!("line-{source_index}");
        let original = input
            .records
            .iter()
            .find(|r| r.id == id)
            .context("missing accepted source record")?;
        let content = source["payload"]["content"]
            .as_array()
            .context("missing source message content")?;
        let text = content
            .iter()
            .filter(|c| matches!(c["type"].as_str(), Some("input_text" | "output_text")))
            .filter_map(|c| c["text"].as_str())
            .collect::<String>();
        let raw = &receipt.original.text;
        let start = if raw.is_empty() {
            0
        } else {
            let first = text
                .find(raw)
                .context("accepted original text is absent from model history")?;
            ensure!(
                text.rfind(raw) == Some(first),
                "original text is ambiguous in model history"
            );
            first
        };
        let end = start + raw.len();
        let mut residual = source.clone();
        let parts = residual["payload"]["content"]
            .as_array_mut()
            .context("missing source content")?;
        let mut offset = 0;
        for part in parts.iter_mut() {
            if !matches!(part["type"].as_str(), Some("input_text" | "output_text")) {
                continue;
            }
            if let Some(part_text) = part["text"].as_str() {
                let len = part_text.len();
                let lo = start.saturating_sub(offset).min(len);
                let hi = end.saturating_sub(offset).min(len);
                let mut kept = part_text.to_owned();
                if lo < hi {
                    ensure!(
                        kept.is_char_boundary(lo) && kept.is_char_boundary(hi),
                        "invalid intake UTF-8 boundary"
                    );
                    kept.replace_range(lo..hi, "");
                }
                part["text"] = Value::String(kept);
                offset += len;
            }
        }
        let human = CandidateRecord {
            id: format!("{id}-input"),
            origin: match receipt.original.author {
                InputAuthor::Human => Origin::Human,
                InputAuthor::Automation => Origin::Work,
            },
            intake_ref: Some(format!(
                "{thread_id}/{client_id}:{}",
                digest(&receipt).map_err(anyhow::Error::msg)?
            )),
            scope: (*thread_id).to_owned(),
            role: "user".into(),
            text: raw.clone(),
            protected: vec![],
            execution_evidence: false,
            opaque: (!receipt.original.attachments.is_empty())
                .then(|| json!({"attachments":receipt.original.attachments})),
        };
        // The source message's remaining context, prepared images and host metadata
        // are retained separately, without a second copy of the original text.
        let mut rest = original.clone();
        rest.text = serde_json::to_string(&residual["payload"])?;
        rest.opaque = Some(residual);
        replacements.insert(id, vec![human, rest]);
    }
    input.records = std::mem::take(&mut input.records)
        .into_iter()
        .flat_map(|r| replacements.remove(&r.id).unwrap_or_else(|| vec![r]))
        .collect();
    input.snapshot().map_err(anyhow::Error::msg)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_history::input_intake::OriginalInput;

    fn fixture(directory: &Path, author: InputAuthor, accepted: bool, inherited: bool) -> Vec<u8> {
        let original = OriginalInput {
            author,
            text: "本人本文".into(),
            attachments: vec![json!({"kind":"local_image","path":"添付.png"})],
        };
        let receipt = SubmissionIntake::new(
            "thread-1".into(),
            "client-1".into(),
            original,
            "本人本文\nIDE context",
        )
        .unwrap();
        std::fs::create_dir_all(directory.join("thread-1")).unwrap();
        std::fs::write(
            directory.join("thread-1/client-1.json"),
            serde_json::to_vec(&receipt).unwrap(),
        )
        .unwrap();
        let mut lines = vec![
            json!({"type":"session_meta","payload":{"id":"thread-1"}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"本人本文\nIDE context"},{"type":"input_image","image_url":"data:image/png;base64,fixture"}]},"metadata":{"user_input_order":1,"inherited_user_message":inherited}}),
        ];
        if accepted {
            lines.push(json!({"type":"event_msg","payload":{"type":"user_message","client_id":"client-1","message":"本人本文\nIDE context"}}));
        }
        lines
            .into_iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("\n")
            .into_bytes()
    }

    #[test]
    fn separates_accepted_original_and_attachment_from_host_context_without_duplicates() {
        let directory = tempfile::tempdir().unwrap();
        let data = fixture(directory.path(), InputAuthor::Human, true, false);
        let mut input = super::super::capture::capture(&data).unwrap();
        apply(&data, &mut input, directory.path()).unwrap();
        assert_eq!(input.records.len(), 2);
        assert_eq!(input.records[0].origin, Origin::Human);
        assert_eq!(input.records[0].text, "本人本文");
        assert_eq!(
            input.records[0].opaque,
            Some(json!({"attachments":[{"kind":"local_image","path":"添付.png"}]}))
        );
        assert_eq!(input.records[1].origin, Origin::Unknown);
        let rest = input.records[1].opaque.as_ref().unwrap();
        assert_eq!(rest["payload"]["content"][0]["text"], "\nIDE context");
        assert_eq!(
            rest["payload"]["content"][1]["image_url"],
            "data:image/png;base64,fixture"
        );
        assert_eq!(rest["metadata"]["user_input_order"], 1);
        assert!(!serde_json::to_string(rest).unwrap().contains("本人本文"));
    }

    #[test]
    fn binds_persisted_user_item_completion_without_a_legacy_event() {
        let directory = tempfile::tempdir().unwrap();
        let mut data = fixture(directory.path(), InputAuthor::Human, false, false);
        let event = json!({"type":"event_msg","payload":{"type":"item_completed","thread_id":"thread-1","item":{"type":"UserMessage","id":"item-1","client_id":"client-1","content":[{"type":"text","text":"本人本文\nIDE context","text_elements":[]}]}}});
        data.extend_from_slice(format!("\n{event}").as_bytes());
        let mut input = super::super::capture::capture(&data).unwrap();
        apply(&data, &mut input, directory.path()).unwrap();
        assert_eq!(input.records[0].origin, Origin::Human);
        assert_eq!(input.records[0].text, "本人本文");
        let wrong_thread = String::from_utf8(data).unwrap().replace(
            "\"thread_id\":\"thread-1\"",
            "\"thread_id\":\"thread-other\"",
        );
        let mut input = super::super::capture::capture(wrong_thread.as_bytes()).unwrap();
        assert!(apply(wrong_thread.as_bytes(), &mut input, directory.path()).is_err());
    }

    #[test]
    fn pending_automation_and_inherited_input_never_become_human() {
        for (author, accepted, inherited, expected) in [
            (InputAuthor::Human, false, false, Origin::Unknown),
            (InputAuthor::Automation, true, false, Origin::Work),
            (InputAuthor::Human, true, true, Origin::Unknown),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let data = fixture(directory.path(), author, accepted, inherited);
            let mut input = super::super::capture::capture(&data).unwrap();
            apply(&data, &mut input, directory.path()).unwrap();
            assert_eq!(input.records[0].origin, expected);
            assert!(input.records.iter().all(|r| r.origin != Origin::Human));
        }
    }

    #[test]
    fn changed_receipt_is_rejected_and_missing_receipt_preserves_unknown() {
        let directory = tempfile::tempdir().unwrap();
        let data = fixture(directory.path(), InputAuthor::Human, true, false);
        let mut input = super::super::capture::capture(&data).unwrap();
        let path = directory.path().join("thread-1/client-1.json");
        let mut receipt: SubmissionIntake =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        receipt.submitted_text_hash = "stale".into();
        std::fs::write(&path, serde_json::to_vec(&receipt).unwrap()).unwrap();
        assert!(apply(&data, &mut input, directory.path()).is_err());
        std::fs::remove_file(path).unwrap();
        apply(&data, &mut input, directory.path()).unwrap();
        assert_eq!(input.records[0].origin, Origin::Unknown);
    }
}
