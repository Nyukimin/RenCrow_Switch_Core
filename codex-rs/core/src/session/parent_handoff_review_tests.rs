// Modified by RenCrow Switch Core, 2026-11-21.
use super::*;
use crate::session::completion_persistence::parent_handoff::ParentHandoffCapsule;
use crate::session::completion_persistence::parent_handoff::proof_closure;
use crate::session::completion_persistence::parent_handoff::runtime;
use codex_history::CodexHarnessMetadata;
use codex_history::ResponseItemEnvelope;

fn snapshot_item(value: Value) -> RolloutItem {
    RolloutItem::ResponseItem(ResponseItemEnvelope {
        item: Box::new(crate::context::AutonomousCompletionSnapshot).into_boxed_response_item(),
        metadata: Some(CodexHarnessMetadata {
            rencrow_completion: Some(value),
            ..Default::default()
        }),
    })
}

#[tokio::test]
async fn parent_handoff_failed_review_preserves_exact_attempt_and_acceptance_on_cold_restore()
-> anyhow::Result<()> {
    let fixture = setup_failed_review().await?;
    let source = &fixture.session;
    let original_state = source.rencrow_completion_state(ROOT_TASK_ID).await.unwrap();
    let config = source.get_config().await;
    let task_state = crate::compact::latest_task_state_in_history(
        source.clone_history().await.annotated_items(),
        &source.thread_id().to_string(),
    )
    .map_err(anyhow::Error::msg)?;
    let (latest, proofs) = {
        let state = source.state.lock().await;
        let hash = &state
            .rencrow_completion_last_persisted_snapshot
            .as_ref()
            .unwrap()
            .sha256;
        let latest = state
            .rencrow_completion_handoff_proofs
            .get(hash)
            .unwrap()
            .clone();
        let proofs = proof_closure(
            &latest,
            &state.rencrow_completion_handoff_proofs,
            task_state.as_ref(),
        )
        .map_err(anyhow::Error::msg)?;
        (latest, proofs)
    };
    let (target, _) = make_session_and_context().await;
    let capsule = ParentHandoffCapsule {
        version: 1,
        source_thread_id: source.thread_id(),
        successor_thread_id: target.thread_id(),
        source_turn_id: "failed-review-handoff".into(),
        source_rollout: fixture.rollout_path,
        source_history_hash: "b".repeat(64),
        source_history_len: 0,
        pending_prompt: None,
        runtime_contract_sha256: runtime::runtime_contract(&config).map_err(anyhow::Error::msg)?,
        settings: source.thread_settings_snapshot().await,
        cwd: config.cwd.to_path_buf(),
        summary: "Failed reviewer; exact obligation remains open".into(),
        next_work: "Diagnose the existing failure before the authorized retry".into(),
        original_snapshot: latest,
        prior_snapshots: proofs,
        accepted_task_state: task_state,
        retained_instructions: Vec::new(),
        guard: crate::rencrow_execution_guard::GuardState::default()
            .parent_handoff_snapshot()
            .map_err(anyhow::Error::msg)?,
    };
    let projection = capsule.project().map_err(anyhow::Error::msg)?;
    let (record, history) = make_import_fixture(&capsule, &projection)?;
    target
        .restore_autonomous_completion_snapshot(&history)
        .await;
    let transferred = target.rencrow_completion_state(ROOT_TASK_ID).await.unwrap();
    assert_eq!(transferred.conditions, original_state.conditions);
    assert_eq!(
        transferred.failed_approaches,
        original_state.failed_approaches
    );
    let review = transferred
        .units
        .iter()
        .find(|unit| unit.id == REVIEW_UNIT_ID)
        .unwrap()
        .review_state
        .as_ref()
        .unwrap();
    let attempt = review.active_attempt.as_ref().unwrap();
    assert_eq!(attempt.attempt_id, fixture.first_attempt_id);
    assert_eq!(
        attempt.reviewer_thread_id.as_deref(),
        Some(fixture.first_child_id.to_string().as_str())
    );
    assert_eq!(
        attempt.status,
        codex_history::task_context::CompletionReviewAttemptStatus::Failed
    );
    assert!(!transferred.paused);
    drop(record);
    Ok(())
}

// Materialize the immutable host capsule and genuine wire-shaped audit records. This is
// local persistence fixture data, not a remote reviewer, model call or fabricated result.
fn make_import_fixture(
    capsule: &ParentHandoffCapsule,
    projection: &crate::session::completion_persistence::parent_handoff::ParentHandoffProjection,
) -> anyhow::Result<(tempfile::TempDir, Vec<RolloutItem>)> {
    let directory = tempfile::tempdir()?;
    let mut capsule = capsule.clone();
    capsule.source_rollout = directory.path().join("source.jsonl");
    let capsule_file = directory.path().join("capsule.json");
    let hash = capsule.sha256().map_err(anyhow::Error::msg)?;
    let capsule_bytes = serde_json::to_vec(&capsule)?;
    let file_hash =
        codex_history::archive_reference::content_sha256(std::str::from_utf8(&capsule_bytes)?);
    let mut bytes = serde_json::to_vec(&serde_json::json!({"type":"session_meta","payload":{
        "id":capsule.source_thread_id, "session_id":capsule.source_thread_id,
        "source":"cli", "cwd":capsule.cwd
    }}))?;
    bytes.push(b'\n');
    bytes.extend(serde_json::to_vec(&snapshot_item(
        capsule.original_snapshot.clone(),
    ))?);
    bytes.push(b'\n');
    let mut proof = Vec::new();
    for phase in ["prepared", "failed"] {
        let mut item = snapshot_item(serde_json::json!({"parent_handoff_audit":{
            "version":1, "source_thread_id":capsule.source_thread_id, "successor_thread_id":capsule.successor_thread_id,
            "source_turn_id":capsule.source_turn_id, "capsule_sha256":hash,
            "capsule_file_sha256":file_hash, "input_file_sha256":null, "phase":phase,
        }}));
        let RolloutItem::ResponseItem(envelope) = &mut item else {
            unreachable!()
        };
        envelope.item = Box::new(crate::context::ParentHandoffNotice).into_boxed_response_item();
        let mut line = serde_json::to_vec(&item)?;
        line.push(b'\n');
        proof.push(runtime::HandoffProof {
            offset: bytes.len() as u64,
            length: line.len() as u64,
            sha256: codex_history::archive_reference::content_sha256(std::str::from_utf8(&line)?),
        });
        bytes.extend(line);
    }
    std::fs::write(&capsule.source_rollout, bytes)?;
    std::fs::write(&capsule_file, capsule_bytes)?;
    let path = directory.path().join("handoff.json");
    let record = runtime::HandoffRecord {
        version: 1,
        capsule_file,
        capsule_sha256: hash.clone(),
        capsule_file_sha256: file_hash,
        source_thread_id: capsule.source_thread_id,
        successor_thread_id: capsule.successor_thread_id,
        source_turn_id: capsule.source_turn_id.clone(),
        phase: "failed".into(),
        reason: Some("capacity".into()),
        prepared_proof: proof.remove(0),
        failed_proof: Some(proof.remove(0)),
        input_file: None,
        input_file_sha256: None,
    };
    std::fs::write(&path, serde_json::to_vec(&record)?)?;
    // initial_history also exercises native source audit proof parsing and the ordinary
    // typed V3 task checkpoint. The config identity was captured before changing only
    // the archive location for this bounded fixture.
    let mut history = Vec::new();
    if let Some(task) = &projection.accepted_task_state {
        let text = "Inherited accepted task state";
        let mut envelope = ResponseItemEnvelope::new(
            Box::new(crate::context::CompactionSummary::new(text)).into_boxed_response_item(),
        );
        envelope.metadata.get_or_insert_default().rencrow_compaction = Some(serde_json::to_value(
            codex_history::task_checkpoint_metadata::RenCrowTaskCheckpointMetadataV3::task_boundary(
                text,
                serde_json::to_value(task)?,
                codex_history::task_checkpoint_metadata::TaskBoundarySourceFence {
                    history_hash: "b".repeat(64),
                    removed_refs: Vec::new(),
                },
            ),
        )?);
        history.push(RolloutItem::ResponseItem(envelope));
    }
    let mut import = snapshot_item(serde_json::json!({"parent_handoff_import":{
        "version":1, "record_file":path, "capsule_sha256":hash,
        "successor_thread_id":capsule.successor_thread_id,
        "projection_mapping_sha256":codex_history::compaction_candidate::digest(&serde_json::to_value(&projection.original_to_projected)?).map_err(anyhow::Error::msg)?,
    }}));
    let RolloutItem::ResponseItem(envelope) = &mut import else {
        unreachable!()
    };
    envelope.item = Box::new(crate::context::ParentHandoffNotice).into_boxed_response_item();
    history.push(import);
    history.push(snapshot_item(projection.snapshot.clone()));
    Ok((directory, history))
}
