// Modified by RenCrow Switch Core, 2026-11-21.
//! Bounded private storage and native source audit verification.
use super::*;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;

pub(super) fn read_bounded<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, String> {
    serde_json::from_slice(&read_bytes_bounded(path)?).map_err(|e| e.to_string())
}

fn read_bytes_bounded(path: &Path) -> Result<Vec<u8>, String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    file.take(MAX_HANDOFF_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > MAX_HANDOFF_BYTES {
        return Err("parent handoff file exceeds bound".into());
    }
    Ok(bytes)
}

pub(super) fn write_private(path: &Path, value: &impl Serialize) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or("parent handoff path has no directory")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let mut temp = tempfile::NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
    serde_json::to_writer(&mut temp, value).map_err(|e| e.to_string())?;
    temp.as_file().sync_all().map_err(|e| e.to_string())?;
    temp.persist(path).map_err(|e| e.to_string())?;
    std::fs::File::open(parent)
        .and_then(|file| file.sync_all())
        .map_err(|e| e.to_string())
}

pub(super) fn notice(value: Value) -> ResponseItemEnvelope {
    ResponseItemEnvelope {
        item: Box::new(ParentHandoffNotice).into_boxed_response_item(),
        metadata: Some(CodexHarnessMetadata {
            rencrow_completion: Some(value),
            ..Default::default()
        }),
    }
}

pub(super) fn host_value(item: &RolloutItem) -> Option<&Value> {
    let RolloutItem::ResponseItem(envelope) = item else {
        return None;
    };
    let metadata = envelope.metadata.as_ref()?;
    if metadata.client_authored {
        return None;
    }
    match &envelope.item {
        ResponseItem::Message {
            role,
            internal_chat_message_metadata_passthrough: Some(kind),
            ..
        } if role == "developer"
            && matches!(kind.content_item_kinds.as_deref(), Some([kind]) if kind.0 == "rencrow.parent_handoff") =>
        {
            metadata.rencrow_completion.as_ref()
        }
        _ => None,
    }
}

pub(super) async fn audit(
    sess: &Session,
    record: &HandoffRecord,
    phase: &str,
) -> Result<HandoffProof, String> {
    sess.flush_rollout().await.map_err(|e| e.to_string())?;
    let path = sess
        .current_rollout_path()
        .await
        .map_err(|e| e.to_string())?
        .ok_or("parent handoff requires a durable rollout")?;
    let offset = std::fs::metadata(&path).map_err(|e| e.to_string())?.len();
    let value = serde_json::json!({"parent_handoff_audit": {
        "version": HANDOFF_VERSION, "source_thread_id": record.source_thread_id,
        "successor_thread_id": record.successor_thread_id, "source_turn_id": record.source_turn_id,
        "capsule_sha256": record.capsule_sha256, "phase": phase,
        "capsule_file_sha256": record.capsule_file_sha256,
        "input_file_sha256": if phase == "failed" { record.input_file_sha256.as_ref() } else { None },
    }});
    if !sess
        .persist_rollout_items(&[RolloutItem::ResponseItem(notice(value))])
        .await
    {
        return Err("parent handoff audit append failed".into());
    }
    sess.flush_rollout().await.map_err(|e| e.to_string())?;
    let length = std::fs::metadata(&path)
        .map_err(|e| e.to_string())?
        .len()
        .checked_sub(offset)
        .ok_or("parent handoff rollout shrank")?;
    if length == 0 || length > 1024 * 1024 {
        return Err("parent handoff audit range is invalid".into());
    }
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| e.to_string())?;
    let mut bytes = vec![0; length as usize];
    file.read_exact(&mut bytes).map_err(|e| e.to_string())?;
    Ok(HandoffProof {
        offset,
        length,
        sha256: codex_history::archive_reference::content_sha256(
            std::str::from_utf8(&bytes).map_err(|e| e.to_string())?,
        ),
    })
}

pub(super) fn verify_audit(
    capsule: &ParentHandoffCapsule,
    record: &HandoffRecord,
    proof: &HandoffProof,
    phase: &str,
) -> Result<(), String> {
    if proof.length == 0 || proof.length > 1024 * 1024 {
        return Err("parent handoff audit proof range invalid".into());
    }
    let mut file = std::fs::File::open(&capsule.source_rollout).map_err(|e| e.to_string())?;
    file.seek(SeekFrom::Start(proof.offset))
        .map_err(|e| e.to_string())?;
    let mut bytes = vec![0; proof.length as usize];
    file.read_exact(&mut bytes).map_err(|e| e.to_string())?;
    let text = std::str::from_utf8(&bytes).map_err(|e| e.to_string())?;
    if codex_history::archive_reference::content_sha256(text) != proof.sha256 {
        return Err("parent handoff source audit hash mismatch".into());
    }
    let expected = serde_json::json!({"version": HANDOFF_VERSION,
        "source_thread_id": record.source_thread_id, "successor_thread_id": record.successor_thread_id,
        "source_turn_id": record.source_turn_id, "capsule_sha256": record.capsule_sha256,
        "capsule_file_sha256": record.capsule_file_sha256,
        "input_file_sha256": if phase == "failed" { record.input_file_sha256.as_ref() } else { None }, "phase": phase});
    let found = text
        .lines()
        .filter_map(|line| serde_json::from_str::<RolloutItem>(line).ok())
        .any(|item| {
            host_value(&item).and_then(|value| value.get("parent_handoff_audit")) == Some(&expected)
        });
    if !found {
        return Err("parent handoff has no matching native source audit".into());
    }
    Ok(())
}

fn verify_source_fence(
    capsule: &ParentHandoffCapsule,
    record: &HandoffRecord,
) -> Result<(), String> {
    let mut prefix = Vec::new();
    std::fs::File::open(&capsule.source_rollout)
        .map_err(|e| e.to_string())?
        .take(65537)
        .read_to_end(&mut prefix)
        .map_err(|e| e.to_string())?;
    let line = prefix
        .split(|byte| *byte == b'\n')
        .next()
        .ok_or("source metadata missing")?;
    let meta: Value = serde_json::from_slice(line).map_err(|e| e.to_string())?;
    let owner = &meta["payload"];
    if meta["type"] != "session_meta"
        || owner["id"] != capsule.source_thread_id.to_string()
        || owner["session_id"] != capsule.source_thread_id.to_string()
        || owner["source"] != "cli"
        || !owner["parent_thread_id"].is_null()
        || owner["cwd"].as_str() != capsule.cwd.to_str()
    {
        return Err("parent handoff source is not the reserved native root".into());
    }
    // The original snapshot must be the latest native host snapshot at the failure seal.
    // Read only the finite suffix around preparation, never replay the entire source file.
    let prepared_end = record
        .prepared_proof
        .offset
        .checked_add(record.prepared_proof.length)
        .ok_or("prepared source fence overflow")?;
    let failed_offset = record
        .failed_proof
        .as_ref()
        .ok_or("failure proof missing")?
        .offset;
    let begin = record.prepared_proof.offset.saturating_sub(2 * 1024 * 1024);
    let length = failed_offset
        .checked_sub(begin)
        .ok_or("failure predates preparation")?;
    if prepared_end > failed_offset || length > 4 * 1024 * 1024 {
        return Err("parent handoff prepared/failed source fence is out of bounds".into());
    }
    let mut file = std::fs::File::open(&capsule.source_rollout).map_err(|e| e.to_string())?;
    file.seek(SeekFrom::Start(begin))
        .map_err(|e| e.to_string())?;
    let mut bytes = vec![0; length as usize];
    file.read_exact(&mut bytes).map_err(|e| e.to_string())?;
    let latest = bytes.split(|byte| *byte == b'\n')
        .filter_map(|line| serde_json::from_slice::<RolloutItem>(line).ok())
        .filter_map(|item| {
            let RolloutItem::ResponseItem(envelope) = item else { return None };
            let metadata = envelope.metadata?;
            let is_host = matches!(&envelope.item, ResponseItem::Message {
                role, internal_chat_message_metadata_passthrough: Some(kind), ..
            } if role == "developer" && matches!(kind.content_item_kinds.as_deref(), Some([kind]) if kind.0 == "rencrow.completion_snapshot"));
            (!metadata.client_authored && is_host).then_some(metadata.rencrow_completion).flatten()
        }).next_back();
    if latest.as_ref() != Some(&capsule.original_snapshot) {
        return Err(
            "parent handoff original snapshot is not the latest native source snapshot".into(),
        );
    }
    let failed = record
        .failed_proof
        .as_ref()
        .ok_or("failed source fence missing")?;
    let end = failed
        .offset
        .checked_add(failed.length)
        .ok_or("source fence overflow")?;
    let mut file = std::fs::File::open(&capsule.source_rollout).map_err(|e| e.to_string())?;
    file.seek(SeekFrom::Start(end)).map_err(|e| e.to_string())?;
    let mut tail = String::new();
    file.take(2 * 1024 * 1024 + 1)
        .read_to_string(&mut tail)
        .map_err(|e| e.to_string())?;
    if tail.len() > 2 * 1024 * 1024 {
        return Err("source changed beyond the bounded failure fence".into());
    }
    for line in tail.lines() {
        let item: RolloutItem = serde_json::from_str(line).map_err(|e| e.to_string())?;
        let changed = match item {
            RolloutItem::ResponseItem(envelope) => match envelope.item {
                ResponseItem::Message { role, .. } => role == "user",
                ResponseItem::FunctionCall { .. }
                | ResponseItem::LocalShellCall { .. }
                | ResponseItem::CustomToolCall { .. } => true,
                _ => false,
            },
            RolloutItem::EventMsg(EventMsg::TurnStarted(_) | EventMsg::TurnAborted(_)) => true,
            RolloutItem::Compacted(_) | RolloutItem::RenCrowCompactionCommit { .. } => true,
            _ => false,
        };
        if changed {
            return Err(
                "source received input, work, interruption or compaction after its failure fence"
                    .into(),
            );
        }
    }
    Ok(())
}

pub(crate) fn load_verified(
    path: &Path,
) -> Result<(HandoffRecord, ParentHandoffCapsule, ParentHandoffProjection), String> {
    let record: HandoffRecord = read_bounded(path)?;
    if record.version != HANDOFF_VERSION || record.phase != "failed" {
        return Err("parent handoff failure is not confirmed".into());
    }
    let capsule_bytes = read_bytes_bounded(&record.capsule_file)?;
    let capsule: ParentHandoffCapsule =
        serde_json::from_slice(&capsule_bytes).map_err(|e| e.to_string())?;
    if codex_history::archive_reference::content_sha256(
        std::str::from_utf8(&capsule_bytes).map_err(|e| e.to_string())?,
    ) != record.capsule_file_sha256
    {
        return Err("parent handoff immutable capsule bytes changed".into());
    }
    if capsule.sha256()? != record.capsule_sha256
        || capsule.source_thread_id != record.source_thread_id
        || capsule.successor_thread_id != record.successor_thread_id
        || capsule.source_turn_id != record.source_turn_id
    {
        return Err("parent handoff source/capsule binding mismatch".into());
    }
    verify_audit(&capsule, &record, &record.prepared_proof, "prepared")?;
    verify_audit(
        &capsule,
        &record,
        record
            .failed_proof
            .as_ref()
            .ok_or("parent handoff lacks failure proof")?,
        "failed",
    )?;
    verify_source_fence(&capsule, &record)?;
    super::input::load_preserved_input(&record, &capsule)?;
    let projection = capsule.project()?;
    Ok((record, capsule, projection))
}

pub(crate) fn claim(path: &Path, target: ThreadId) -> CodexResult<()> {
    let claim = path.with_extension("claimed");
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&claim)
        .map_err(|e| {
            CodexErr::InvalidRequest(format!(
                "parent handoff already claimed or unavailable: {e}"
            ))
        })?;
    serde_json::to_writer(&file, &serde_json::json!({"successor_thread_id": target}))
        .and_then(|()| file.sync_all().map_err(serde_json::Error::io))
        .map_err(|e| CodexErr::InvalidRequest(e.to_string()))?;
    Ok(())
}
