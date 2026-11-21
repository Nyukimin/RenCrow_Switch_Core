// Modified by RenCrow Switch Core, 2026-11-21.
//! Pre-turn failures seal only after prompt hooks accept and persist the pending input.
use super::*;
use crate::session::input_queue::TurnInput;
use codex_protocol::user_input::UserInput;

pub(crate) enum PreservedHandoffInput {
    Accepted,
    Stopped,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreservedInput {
    source_thread_id: ThreadId,
    source_turn_id: String,
    source_history_hash_after: String,
    items: Vec<RolloutItem>,
}

impl Session {
    pub(crate) async fn remember_parent_handoff_input(
        &self,
        turn: &TurnContext,
        input: &[TurnInput],
    ) {
        let text = match input {
            [TurnInput::UserInput { content, .. }] => match content.as_slice() {
                [
                    UserInput::Text {
                        text,
                        text_elements,
                    },
                ] if text_elements.is_empty()
                    && text.len() <= 32 * 1024
                    && !text.contains('\0') =>
                {
                    Some(text.clone())
                }
                _ => None,
            },
            _ => None,
        };
        self.state.lock().await.rencrow_handoff_pending_input =
            text.map(|text| (turn.sub_id.clone(), text));
    }

    pub(crate) async fn finalize_parent_handoff_input(
        &self,
        turn: &TurnContext,
        status: PreservedHandoffInput,
    ) -> Result<(), String> {
        let Some(path) = self
            .state
            .lock()
            .await
            .rencrow_handoff_pending_record
            .take()
        else {
            return Ok(());
        };
        let mut record: HandoffRecord = read_bounded(&path)?;
        if record.source_thread_id != self.thread_id()
            || record.source_turn_id != turn.sub_id
            || record.phase != "awaiting_input"
        {
            return Err("pre-turn handoff is not bound to the current failed attempt".into());
        }
        if matches!(status, PreservedHandoffInput::Stopped) {
            record.phase = "blocked".into();
            record.reason = Some("pending input was stopped by its hook".into());
            return write_private(&path, &record);
        }
        let result = async {
            let capsule: ParentHandoffCapsule = read_bounded(&record.capsule_file)?;
            self.check_rencrow_checkpoint()
                .await
                .map_err(|e| e.to_string())?;
            verify_live_source(self, &capsule).await?;
            let history = self.clone_history().await;
            let suffix = history
                .annotated_items()
                .get(capsule.source_history_len..)
                .ok_or("pre-turn handoff source prefix disappeared")?;
            let input = PreservedInput {
                source_thread_id: self.thread_id(),
                source_turn_id: turn.sub_id.clone(),
                source_history_hash_after: history_digest(history.annotated_items())?,
                items: suffix
                    .iter()
                    .cloned()
                    .map(RolloutItem::ResponseItem)
                    .collect(),
            };
            validate_input(&input, &capsule)?;
            let input_path = path.with_file_name("preserved-input.json");
            write_private(&input_path, &input)?;
            record.input_file_sha256 = Some(codex_history::archive_reference::content_sha256(
                &std::fs::read_to_string(&input_path).map_err(|e| e.to_string())?,
            ));
            record.input_file = Some(input_path);
            record.failed_proof = Some(audit(self, &record, "failed").await?);
            record.phase = "failed".into();
            Ok::<(), String>(())
        }
        .await;
        if let Err(error) = result {
            record.phase = "blocked".into();
            record.reason = Some(error.clone());
            write_private(&path, &record)?;
            return Err(error);
        }
        write_private(&path, &record)
    }
}

fn validate_input(input: &PreservedInput, capsule: &ParentHandoffCapsule) -> Result<(), String> {
    if input.source_thread_id != capsule.source_thread_id
        || input.source_turn_id != capsule.source_turn_id
        || !is_sha256(&input.source_history_hash_after)
        || input.items.is_empty()
        || serde_json::to_vec(input).map_err(|e| e.to_string())?.len() > 128 * 1024
    {
        return Err("preserved pending input is incomplete, foreign or oversized".into());
    }
    let mut user_text = Vec::new();
    for item in &input.items {
        match item {
            RolloutItem::ResponseItem(envelope) => match &envelope.item {
                ResponseItem::Message { role, content, .. }
                    if role == "user" || role == "developer" =>
                {
                    if role == "user" {
                        let [ContentItem::InputText { text }] = content.as_slice() else {
                            return Err(
                                "pre-turn handoff supports exactly one plain-text input".into()
                            );
                        };
                        user_text.push(text.as_str());
                    }
                }
                _ => return Err("pending input suffix contains work or an unknown effect".into()),
            },
            _ => return Err("pending input suffix contains a non-input record".into()),
        }
    }
    if user_text.as_slice()
        != [capsule
            .pending_prompt
            .as_deref()
            .ok_or("pending prompt absent")?]
    {
        return Err("recorded pending input differs from the pre-hook prompt".into());
    }
    Ok(())
}

pub(super) fn load_preserved_input(
    record: &HandoffRecord,
    capsule: &ParentHandoffCapsule,
) -> Result<Vec<RolloutItem>, String> {
    match (
        &capsule.pending_prompt,
        &record.input_file,
        &record.input_file_sha256,
    ) {
        (None, None, None) => Ok(Vec::new()),
        (Some(_), Some(path), Some(hash)) => {
            let input: PreservedInput = read_bounded(path)?;
            let bytes = serde_json::to_vec(&input).map_err(|e| e.to_string())?;
            if codex_history::archive_reference::content_sha256(
                std::str::from_utf8(&bytes).map_err(|e| e.to_string())?,
            ) != *hash
            {
                return Err("preserved pending input bytes changed".into());
            }
            validate_input(&input, capsule)?;
            Ok(input.items)
        }
        _ => Err("pending prompt does not have its accepted native input proof".into()),
    }
}
