// Modified by RenCrow Switch Core, 2026-11-24: preserve review scope on rejected cross-task revalidation.
// Modified by RenCrow Switch Core, 2026-11-16.
#[path = "parent_handoff_review_tests.rs"]
mod parent_handoff_review_tests;
use super::InitialHistory;
use super::RolloutItem;
use super::RolloutRecorder;
use super::accepted_task_boundary_item;
use super::attach_thread_persistence;
use super::install_host_completion_fixture;
use super::make_session_and_context;
use crate::session::CompletionReviewChildGeneration;
use crate::session::CompletionReviewChildIdentity;
use crate::session::CompletionReviewRetryRequest;
use crate::session::session::Session;
use codex_context_fragments::ContextualUserFragment;
use codex_history::compaction_candidate::digest;
use codex_history::task_context::AcceptanceCondition;
use codex_history::task_context::AcceptanceStatus;
use codex_history::task_context::CompletionEvidenceRef;
use codex_history::task_context::CompletionReviewAttemptStatus;
use codex_history::task_context::CompletionReviewRecordLocator;
use codex_history::task_context::CompletionReviewSnapshotRef;
use codex_history::task_context::CompletionReviewTarget;
use codex_history::task_context::CompletionReviewTerminalEvent;
use codex_history::task_context::CompletionReviewTerminalSource;
use codex_history::task_context::CompletionReviewTerminalStatus;
use codex_history::task_context::CompletionReviewWithdrawalSource;
use codex_history::task_context::CompletionState;
use codex_history::task_context::CompletionUnit;
use codex_history::task_context::CompletionUnitKind;
use codex_history::task_context::UnitStatus;
use codex_protocol::ThreadId;
use codex_protocol::models::BaseInstructions;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadMemoryMode;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnAbortedEvent;
use codex_thread_store::CreateThreadParams;
use codex_thread_store::LiveThread;
use codex_thread_store::PersistedTurnAbortLocator;
use codex_thread_store::ThreadPersistenceMetadata;
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;

const ROOT_TASK_ID: &str = "root";
const TARGET_TASK_ID: &str = "task-a";
const REVIEW_UNIT_ID: &str = "review-a";
const REVIEWER_ROLE: &str = "reviewer";
const REVIEWER_MODEL: &str = "test-review-model";
const PREVIOUS_PLAN: &str = "Review the accepted task against AC-1";
const REVISED_PLAN: &str = "Repeat the same review after replacing the failed attempt";
const RETRY_REASON: &str = "The previous reviewer failed; the exact obligation remains required";

struct FailedReviewFixture {
    session: Session,
    rollout_path: std::path::PathBuf,
    task_workspace: tempfile::TempDir,
    target_artifact_ref: String,
    first_child_id: ThreadId,
    first_turn_id: String,
    first_attempt_id: String,
}

fn task_artifact_fingerprint(receipt: &crate::task_check::TaskCheckReceipt) -> String {
    let artifact_stamp_set = receipt
        .artifact_after
        .iter()
        .map(|stamp| {
            serde_json::json!({
                "path": stamp.path.to_string_lossy(),
                "sha256": stamp.sha256,
            })
        })
        .collect::<Vec<_>>();
    digest(&serde_json::json!({
        "task_id": receipt.task_id,
        "task_revision": receipt.task_revision,
        "workspace_root": receipt.workspace_root.to_string_lossy(),
        "artifacts": artifact_stamp_set,
    }))
    .expect("accepted task artifact fingerprint is serializable")
}

async fn install_accepted_task_receipt(
    session: &Session,
    call_id: &str,
) -> anyhow::Result<(String, tempfile::TempDir, RolloutItem)> {
    let workspace = tempfile::tempdir()?;
    let (artifact_ref, accepted_boundary) = install_accepted_task_receipt_with_content(
        session,
        call_id,
        "accepted artifact\n",
        workspace.path(),
    )
    .await?;
    Ok((artifact_ref, workspace, accepted_boundary))
}

async fn install_accepted_task_receipt_with_content(
    session: &Session,
    call_id: &str,
    artifact_contents: &str,
    workspace: &Path,
) -> anyhow::Result<(String, RolloutItem)> {
    let artifact_path = std::path::PathBuf::from("review-target.txt");
    std::fs::write(workspace.join(&artifact_path), artifact_contents)?;
    install_accepted_task_receipt_for_paths(session, call_id, workspace, vec![artifact_path]).await
}

async fn install_accepted_task_receipt_for_paths(
    session: &Session,
    call_id: &str,
    workspace: &Path,
    artifact_paths: Vec<std::path::PathBuf>,
) -> anyhow::Result<(String, RolloutItem)> {
    let artifact_before =
        crate::task_check::hash_workspace_artifacts(workspace, &artifact_paths)
            .ok_or_else(|| anyhow::anyhow!("accepted task artifact fixture could not be hashed"))?;
    session
        .begin_task_check(
            call_id.to_string(),
            crate::task_check::TaskEvidenceRequest {
                task_id: TARGET_TASK_ID.to_string(),
                task_revision: 1,
                artifact_paths,
                check_kind: crate::task_check::TaskCheckKind::Verification,
            },
            workspace.to_path_buf(),
            artifact_before.clone(),
        )
        .await;
    let receipt = session
        .finish_task_check(
            call_id,
            /*exit_code*/ 0,
            /*completed*/ true,
            artifact_before,
        )
        .await
        .ok_or_else(|| anyhow::anyhow!("accepted task fixture check did not produce a receipt"))?;
    let artifact_ref = format!(
        "task-check:{call_id}:{}",
        task_artifact_fingerprint(&receipt)
    );
    let boundary_item =
        accepted_task_boundary_item(&session.thread_id().to_string(), &artifact_ref);
    let accepted_boundary =
        record_accepted_boundary_in_history_and_store(session, boundary_item).await?;
    let accepted = session
        .latest_rencrow_task_state()
        .await?
        .ok_or_else(|| anyhow::anyhow!("accepted task boundary is unavailable in history"))?;
    anyhow::ensure!(
        accepted
            .completed
            .get(TARGET_TASK_ID)
            .is_some_and(|accepted| accepted.revision == 1 && accepted.artifact_ref == artifact_ref),
        "accepted task receipt must bind the review target to its host task-check artifact"
    );
    Ok((artifact_ref, accepted_boundary))
}

/// Mirrors the post-collection observer invoked for a native wait tool receipt.
async fn observe_native_wait_result(session: &Session, child_id: ThreadId, status: AgentStatus) {
    let target = child_id.to_string();
    session
        .observe_completion_child_tool(
            "wait_agent",
            &serde_json::json!({"targets": [target.clone()]}),
            &serde_json::json!({"status": {target: status}}),
        )
        .await;
}

/// This Session/store fixture adopts the same typed boundary in the two owners used by the
/// reader: ContextManager history and the durable rollout. It exercises the shared reader seam,
/// not the public task-boundary tool action.
async fn record_accepted_boundary_in_history_and_store(
    session: &Session,
    boundary_item: RolloutItem,
) -> anyhow::Result<RolloutItem> {
    let RolloutItem::ResponseItem(mut envelope) = boundary_item else {
        anyhow::bail!("accepted task boundary fixture must be a typed response item");
    };
    let history_hash = codex_history::compaction_candidate::history_digest(
        session.clone_history().await.annotated_items(),
    )
    .map_err(anyhow::Error::msg)?;
    let metadata = envelope
        .metadata
        .as_mut()
        .and_then(|metadata| metadata.rencrow_compaction.as_mut())
        .ok_or_else(|| anyhow::anyhow!("accepted task boundary has no V3 checkpoint metadata"))?;
    let mut checkpoint: codex_history::task_checkpoint_metadata::RenCrowTaskCheckpointMetadataV3 =
        serde_json::from_value(metadata.clone())?;
    checkpoint
        .source_fence
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("accepted task boundary has no source fence"))?
        .history_hash = history_hash;
    let summary_text = match &envelope.item {
        codex_protocol::models::ResponseItem::Message { content, .. } => content
            .iter()
            .find_map(|item| match item {
                codex_protocol::models::ContentItem::InputText { text } => Some(text.as_str()),
                _ => None,
            })
            .ok_or_else(|| anyhow::anyhow!("accepted task boundary summary text is missing"))?,
        _ => anyhow::bail!("accepted task boundary body must be a message"),
    };
    checkpoint
        .validate_for_checkpoint(&session.thread_id().to_string(), summary_text)
        .map_err(anyhow::Error::msg)?;
    *metadata = serde_json::to_value(checkpoint)?;
    let rollout_item = RolloutItem::ResponseItem(envelope.clone());
    if !session
        .persist_rollout_items(std::slice::from_ref(&rollout_item))
        .await
    {
        anyhow::bail!("accepted task boundary did not persist to the live thread writer");
    }
    session.flush_rollout().await?;
    session.state.lock().await.history.record_annotated_items(
        std::slice::from_ref(&envelope),
        codex_utils_output_truncation::TruncationPolicy::Tokens(1024),
    );
    Ok(rollout_item)
}

/// Record a real task-check receipt for a new execution ID, then extend the accepted task
/// boundary while retaining its earlier accepted receipts. This exercises the session/store
/// acceptance seam; it does not emulate the public task-boundary tool action.
async fn install_accepted_task_receipt_for_new_target(
    session: &Session,
    call_id: &str,
    task_id: &str,
    task_revision: u64,
    artifact_contents: &str,
    workspace: &Path,
) -> anyhow::Result<(String, RolloutItem)> {
    let artifact_path = std::path::PathBuf::from("review-target.txt");
    std::fs::write(workspace.join(&artifact_path), artifact_contents)?;
    let artifact_paths = vec![artifact_path];
    let artifact_before =
        crate::task_check::hash_workspace_artifacts(workspace, &artifact_paths)
            .ok_or_else(|| anyhow::anyhow!("new-target artifact fixture could not be hashed"))?;
    session
        .begin_task_check(
            call_id.to_string(),
            crate::task_check::TaskEvidenceRequest {
                task_id: task_id.to_string(),
                task_revision,
                artifact_paths,
                check_kind: crate::task_check::TaskCheckKind::Verification,
            },
            workspace.to_path_buf(),
            artifact_before.clone(),
        )
        .await;
    let receipt = session
        .finish_task_check(
            call_id,
            /*exit_code*/ 0,
            /*completed*/ true,
            artifact_before,
        )
        .await
        .ok_or_else(|| anyhow::anyhow!("new-target task check did not produce a receipt"))?;
    let artifact_ref = format!(
        "task-check:{call_id}:{}",
        task_artifact_fingerprint(&receipt)
    );

    let mut accepted = session
        .latest_rencrow_task_state()
        .await?
        .ok_or_else(|| anyhow::anyhow!("the prior accepted task state is unavailable"))?;
    let prior_revision = accepted.boundary.revision;
    let prior_state_hash = accepted.state_hash().map_err(anyhow::Error::msg)?;
    anyhow::ensure!(
        !accepted.completed.contains_key(task_id),
        "new execution ID must not replace an earlier accepted task"
    );
    accepted.previous_revision = Some(prior_revision);
    accepted.previous_state_hash = Some(prior_state_hash);
    accepted.boundary.revision += 1;
    accepted.boundary.closed_task_id = task_id.to_string();
    accepted.boundary.closed_task_revision = task_revision;
    accepted.boundary.artifact_ref = artifact_ref.clone();
    accepted.boundary.verification_ref = artifact_ref.clone();
    accepted
        .boundary
        .tasks
        .push(codex_history::task_context::TaskState {
            id: task_id.to_string(),
            purpose: "execute the newly accepted task target".into(),
            status: codex_history::task_context::TaskStatus::Completed,
            depends_on: vec![TARGET_TASK_ID.to_string()],
        });
    accepted.completed.insert(
        task_id.to_string(),
        codex_history::task_context::AcceptedTaskReceipt {
            revision: task_revision,
            result: "new execution task completed and verified".into(),
            artifact_ref: artifact_ref.clone(),
            verification_ref: artifact_ref.clone(),
        },
    );
    accepted.parent_review_ref = format!("review:{task_id}:{task_revision}");
    accepted.boundary_nonce = format!("accepted-{call_id}");
    accepted.candidate_hash = codex_history::archive_reference::content_sha256(
        &serde_json::to_string(&accepted.boundary)?,
    );
    let thread_id = session.thread_id().to_string();
    accepted.validate(&thread_id).map_err(anyhow::Error::msg)?;

    let summary_text = "Accepted the new execution task with its task-check receipt.";
    let mut envelope =
        codex_history::ResponseItemEnvelope::new(codex_protocol::models::ResponseItem::Message {
            id: None,
            role: "user".into(),
            content: vec![codex_protocol::models::ContentItem::InputText {
                text: summary_text.into(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: Some(
                codex_protocol::models::InternalChatMessageMetadataPassthrough {
                    content_item_kinds: Some(vec![codex_protocol::models::ContentItemKind(
                        "compaction.summary".into(),
                    )]),
                    ..Default::default()
                },
            ),
        });
    let checkpoint =
        codex_history::task_checkpoint_metadata::RenCrowTaskCheckpointMetadataV3::task_boundary(
            summary_text,
            serde_json::to_value(accepted)?,
            codex_history::task_checkpoint_metadata::TaskBoundarySourceFence {
                history_hash: "b".repeat(64),
                removed_refs: Vec::new(),
            },
        );
    envelope.metadata.get_or_insert_default().rencrow_compaction =
        Some(serde_json::to_value(checkpoint)?);
    let boundary_item =
        record_accepted_boundary_in_history_and_store(session, RolloutItem::ResponseItem(envelope))
            .await?;
    let latest = session
        .latest_rencrow_task_state()
        .await?
        .ok_or_else(|| anyhow::anyhow!("new-target accepted boundary is unavailable"))?;
    anyhow::ensure!(
        latest.completed.contains_key(TARGET_TASK_ID)
            && latest.completed.get(task_id).is_some_and(|accepted| {
                accepted.revision == task_revision && accepted.artifact_ref == artifact_ref
            }),
        "new-target boundary must retain Alpha and bind Beta to its task-check receipt"
    );
    Ok((artifact_ref, boundary_item))
}

fn completion_state(target_artifact_ref: &str) -> CompletionState {
    let target_evidence = CompletionEvidenceRef {
        task_id: TARGET_TASK_ID.into(),
        task_revision: 1,
        artifact_ref: target_artifact_ref.into(),
        verification_ref: target_artifact_ref.into(),
    };
    CompletionState {
        contract_revision: 1,
        units_completed_since_review: 0,
        plan_review_at: Some(1_800_000_000),
        plan_reviews: Vec::new(),
        paused: false,
        consumed_host_events: Vec::new(),
        conditions: vec![AcceptanceCondition {
            id: "AC-1".into(),
            revision: 1,
            result: "Task A is complete".into(),
            scope: "Task A".into(),
            verification: "Verify the accepted artifact".into(),
            acceptance_criteria: "The reviewer confirms the accepted artifact".into(),
            status: AcceptanceStatus::Satisfied,
            evidence: vec![target_evidence.clone()],
            unmet_reason: None,
        }],
        units: vec![
            CompletionUnit {
                id: TARGET_TASK_ID.into(),
                purpose: "Execute task A".into(),
                status: UnitStatus::Done,
                kind: CompletionUnitKind::Execution,
                condition_refs: vec!["AC-1".into()],
                depends_on: Vec::new(),
                evidence: Vec::new(),
                task_refs: vec![target_evidence],
                review_target: None,
                review_evidence: None,
                review_launch: None,
                review_decision_reason: None,
                review_state: None,
                review_revalidation_of: None,
            },
            CompletionUnit {
                id: REVIEW_UNIT_ID.into(),
                purpose: "Review the accepted task A artifact".into(),
                status: UnitStatus::Ready,
                kind: CompletionUnitKind::Review,
                condition_refs: vec!["AC-1".into()],
                depends_on: vec![TARGET_TASK_ID.into()],
                evidence: Vec::new(),
                task_refs: Vec::new(),
                review_target: Some(CompletionReviewTarget {
                    task_id: TARGET_TASK_ID.into(),
                    task_revision: 1,
                }),
                review_evidence: None,
                review_launch: None,
                review_decision_reason: None,
                review_state: None,
                review_revalidation_of: None,
            },
            CompletionUnit {
                id: "review-a-legacy-planned".into(),
                purpose: "Explicitly retractable planned duplicate for task A".into(),
                status: UnitStatus::Planned,
                kind: CompletionUnitKind::Review,
                condition_refs: vec!["AC-1".into()],
                depends_on: vec![TARGET_TASK_ID.into()],
                evidence: Vec::new(),
                task_refs: Vec::new(),
                review_target: Some(CompletionReviewTarget {
                    task_id: TARGET_TASK_ID.into(),
                    task_revision: 1,
                }),
                review_evidence: None,
                review_launch: None,
                review_decision_reason: None,
                review_state: None,
                review_revalidation_of: None,
            },
        ],
        current_unit: Some(REVIEW_UNIT_ID.into()),
        failed_approaches: Vec::new(),
        blocker: None,
        next_action: Some(PREVIOUS_PLAN.into()),
        product_progress_at: None,
        runtime_recovery_at: None,
        activity_at: None,
    }
}

async fn withdraw_unlaunched_review(
    session: &Session,
    unit_id: &str,
    update_call_id: &str,
) -> anyhow::Result<()> {
    let mut completion = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("completion plan remains registered");
    let unit = completion
        .units
        .iter_mut()
        .find(|unit| unit.id == unit_id)
        .expect("planned review obligation remains registered");
    anyhow::ensure!(unit.status == UnitStatus::Planned);
    anyhow::ensure!(unit.review_evidence.is_none() && unit.review_launch.is_none());
    unit.status = UnitStatus::Cancelled;
    session
        .set_rencrow_completion_state_with_call_id(
            ROOT_TASK_ID.into(),
            serde_json::to_value(completion)?,
            Some(update_call_id.into()),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    let withdrawn = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("withdrawn completion plan remains registered");
    anyhow::ensure!(withdrawn.units.iter().any(|unit| {
        unit.id == unit_id
            && unit.status == UnitStatus::Cancelled
            && unit
                .review_state
                .as_ref()
                .and_then(|review| review.withdrawal.as_ref())
                .is_some_and(|withdrawal| matches!(
                    &withdrawal.source,
                    CompletionReviewWithdrawalSource::HostUpdate { update_call_id: recorded, .. }
                        if recorded == update_call_id
                ))
    }));
    Ok(())
}

#[tokio::test]
async fn completion_review_brief_preserves_receipt_workspace_and_artifact_paths()
-> anyhow::Result<()> {
    let (mut session, _) = make_session_and_context().await;
    attach_thread_persistence(&mut session).await;
    let (artifact_ref, workspace, _) =
        install_accepted_task_receipt(&session, "accepted-task-check-path-packet").await?;
    install_host_completion_fixture(&session, ROOT_TASK_ID, completion_state(&artifact_ref))
        .await
        .map_err(anyhow::Error::msg)?;
    {
        let mut state = session.state.lock().await;
        let (_, contract) = state
            .rencrow_completion_state
            .as_mut()
            .expect("completion fixture is installed");
        contract
            .units
            .iter_mut()
            .find(|unit| unit.id == REVIEW_UNIT_ID)
            .expect("review obligation is present")
            .purpose = "p".repeat(4_096);
        let condition = contract
            .conditions
            .iter_mut()
            .find(|condition| condition.id == "AC-1")
            .expect("review condition is present");
        condition.result = "r".repeat(4_096);
        condition.scope = "s".repeat(4_096);
        condition.verification = "v".repeat(4_096);
        condition.acceptance_criteria = "c".repeat(4_096);
    }

    let launch = session
        .prepare_completion_review_launch(
            TARGET_TASK_ID,
            Some(REVIEW_UNIT_ID),
            "review-spawn-path-packet",
            Some(REVIEWER_ROLE),
            Some(REVIEWER_MODEL),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    let workspace_root = workspace.path();
    let location = launch
        .review_brief
        .lines()
        .find_map(|line| line.strip_prefix("Verified artifact location: "))
        .map(serde_json::from_str::<Value>)
        .transpose()?
        .expect("review brief contains a complete structured artifact location");
    let artifact_path = std::path::PathBuf::from(
        location["absolute_path"]
            .as_str()
            .expect("artifact path is a string"),
    );

    assert!(launch.should_spawn);
    assert!(launch.review_brief.chars().count() <= 7_000);
    assert!(!launch.review_brief.contains(&artifact_ref));
    assert_eq!(launch.target_artifact_ref, artifact_ref);
    assert_eq!(
        location["workspace_root"].as_str(),
        Some(workspace_root.to_string_lossy().as_ref())
    );
    assert!(artifact_path.is_absolute());
    assert_eq!(
        std::fs::read_to_string(&artifact_path)?,
        "accepted artifact\n"
    );
    Ok(())
}

#[tokio::test]
async fn oversized_verified_artifact_locations_do_not_reserve_a_review() -> anyhow::Result<()> {
    let (session, _) = make_session_and_context().await;
    let workspace = tempfile::tempdir()?;
    let mut artifact_paths = Vec::new();
    for index in 0..12 {
        let relative_path = std::path::PathBuf::from(format!("dir-{index}-{}", "d".repeat(180)))
            .join(format!("subdir-{}", "s".repeat(180)))
            .join(format!("artifact-{}", "a".repeat(180)));
        let absolute_path = workspace.path().join(&relative_path);
        std::fs::create_dir_all(
            absolute_path
                .parent()
                .expect("artifact path has a parent directory"),
        )?;
        std::fs::write(&absolute_path, "verified artifact")?;
        artifact_paths.push(relative_path);
    }
    let (artifact_ref, _) = install_accepted_task_receipt_for_paths(
        &session,
        "accepted-task-check-long-paths",
        workspace.path(),
        artifact_paths,
    )
    .await?;
    install_host_completion_fixture(&session, ROOT_TASK_ID, completion_state(&artifact_ref))
        .await
        .map_err(anyhow::Error::msg)?;

    let result = session
        .prepare_completion_review_launch(
            TARGET_TASK_ID,
            Some(REVIEW_UNIT_ID),
            "review-spawn-long-paths",
            Some(REVIEWER_ROLE),
            Some(REVIEWER_MODEL),
        )
        .await;
    assert!(matches!(
        result,
        Err(ref error) if error.contains("verified artifact locations exceed the completion review brief limit")
    ));

    let contract = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("completion state remains available");
    let review_unit = contract
        .units
        .iter()
        .find(|unit| unit.id == REVIEW_UNIT_ID)
        .expect("review obligation remains present");
    assert_eq!(review_unit.status, UnitStatus::Ready);
    assert!(review_unit.review_launch.is_none());
    assert!(review_unit.review_state.is_none());
    Ok(())
}

#[tokio::test]
async fn planned_review_without_accepted_task_state_has_no_current_scope_reason() {
    let (session, _) = make_session_and_context().await;
    let contract = completion_state("task-check:planned-only");
    session.state.lock().await.rencrow_completion_state =
        Some((ROOT_TASK_ID.to_string(), contract));

    assert!(
        session
            .latest_rencrow_task_state()
            .await
            .expect("read task state")
            .is_none()
    );
    assert!(
        session
            .autonomous_completion_current_review_scope_reason()
            .await
            .is_none()
    );
}

fn planned_revalidation_unit(unit_id: &str, source_unit_id: &str) -> CompletionUnit {
    CompletionUnit {
        id: unit_id.into(),
        purpose: "Revalidate task A after its accepted bytes changed".into(),
        status: UnitStatus::Planned,
        kind: CompletionUnitKind::Review,
        condition_refs: vec!["AC-1".into()],
        depends_on: vec![TARGET_TASK_ID.into()],
        evidence: Vec::new(),
        task_refs: Vec::new(),
        review_target: Some(CompletionReviewTarget {
            task_id: TARGET_TASK_ID.into(),
            task_revision: 1,
        }),
        review_evidence: None,
        review_launch: None,
        review_decision_reason: None,
        review_state: None,
        review_revalidation_of: Some(source_unit_id.into()),
    }
}

fn planned_independent_review_unit(unit_id: &str) -> CompletionUnit {
    let mut unit = planned_revalidation_unit(unit_id, "unused-source-for-independent-review");
    unit.purpose = "Independent review obligation for task A".into();
    unit.review_revalidation_of = None;
    unit
}

async fn spawn_and_bind_attempt(
    session: &Session,
    unit_id: &str,
    target_artifact_ref: &str,
    spawn_call_id: &str,
    thread_id: ThreadId,
    turn_id: &str,
) -> anyhow::Result<()> {
    spawn_and_bind_attempt_for_target(
        session,
        unit_id,
        CompletionReviewTarget {
            task_id: TARGET_TASK_ID.into(),
            task_revision: 1,
        },
        target_artifact_ref,
        spawn_call_id,
        thread_id,
        turn_id,
    )
    .await
}

async fn spawn_and_bind_attempt_for_target(
    session: &Session,
    unit_id: &str,
    target: CompletionReviewTarget,
    target_artifact_ref: &str,
    spawn_call_id: &str,
    thread_id: ThreadId,
    turn_id: &str,
) -> anyhow::Result<()> {
    session
        .record_completion_review_child_pending(
            unit_id,
            CompletionReviewChildIdentity {
                target: target.clone(),
                target_artifact_ref: target_artifact_ref.to_string(),
                reviewer_thread_id: thread_id,
                reviewer_role: REVIEWER_ROLE.to_string(),
                spawn_call_id: spawn_call_id.to_string(),
            },
        )
        .await
        .map_err(anyhow::Error::msg)?;
    session
        .record_completion_review_spawn(
            unit_id,
            CompletionReviewChildGeneration {
                identity: CompletionReviewChildIdentity {
                    target,
                    target_artifact_ref: target_artifact_ref.to_string(),
                    reviewer_thread_id: thread_id,
                    reviewer_role: REVIEWER_ROLE.to_string(),
                    spawn_call_id: spawn_call_id.to_string(),
                },
                reviewer_turn_id: turn_id.to_string(),
            },
        )
        .await
        .map_err(anyhow::Error::msg)
}

async fn setup_failed_review() -> anyhow::Result<FailedReviewFixture> {
    let (mut session, _) = make_session_and_context().await;
    let rollout_path = attach_thread_persistence(&mut session).await;
    let (target_artifact_ref, task_workspace, _) =
        install_accepted_task_receipt(&session, "accepted-task-check").await?;
    install_host_completion_fixture(
        &session,
        ROOT_TASK_ID,
        completion_state(&target_artifact_ref),
    )
    .await
    .map_err(anyhow::Error::msg)?;

    let first_attempt_id = "review-spawn-first";
    let first_launch = session
        .prepare_completion_review_launch(
            TARGET_TASK_ID,
            Some(REVIEW_UNIT_ID),
            first_attempt_id,
            Some(REVIEWER_ROLE),
            Some(REVIEWER_MODEL),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    anyhow::ensure!(
        first_launch.should_spawn,
        "first planned attempt must reserve a spawn"
    );
    let first_child_id =
        ThreadId::try_from("01a1c606-48b8-7cf3-bb78-77b47f6b8c2f").expect("valid child ID");
    let first_turn_id = "review-turn-first";
    spawn_and_bind_attempt(
        &session,
        REVIEW_UNIT_ID,
        &target_artifact_ref,
        first_attempt_id,
        first_child_id,
        first_turn_id,
    )
    .await?;
    session
        .collect_completion_review_results(
            "wait-first-failed",
            &[(
                first_child_id,
                Some(first_turn_id.to_string()),
                AgentStatus::Errored("simulated exact reviewer failure".into()),
            )],
        )
        .await;
    observe_native_wait_result(
        &session,
        first_child_id,
        AgentStatus::Errored("simulated exact reviewer failure".into()),
    )
    .await;
    session.flush_rollout().await?;

    let failed = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .ok_or_else(|| anyhow::anyhow!("failed review contract is missing"))?;
    anyhow::ensure!(
        failed.units.iter().any(|unit| {
            unit.id == REVIEW_UNIT_ID
                && unit.status == UnitStatus::Failed
                && unit
                    .review_state
                    .as_ref()
                    .and_then(|review| review.active_attempt.as_ref())
                    .is_some_and(|attempt| {
                        attempt.attempt_id == first_attempt_id
                            && attempt.status
                                == codex_history::task_context::CompletionReviewAttemptStatus::Failed
                            && attempt.terminal_source.as_ref().is_some_and(|source| {
                                source.status == codex_history::task_context::CompletionReviewTerminalStatus::Failed
                            })
                    })
        }),
        "native wait collection must persist the exact first failed attempt"
    );
    {
        let mut state = session.state.lock().await;
        let (_, contract) = state
            .rencrow_completion_state
            .as_mut()
            .expect("registered completion contract");
        contract.contract_revision = 2;
    }
    session
        .persist_autonomous_completion_snapshot()
        .await
        .map_err(anyhow::Error::msg)?;
    let revised = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("revised contract remains registered");
    anyhow::ensure!(
        revised
            .units
            .iter()
            .find(|unit| unit.id == REVIEW_UNIT_ID)
            .and_then(|unit| unit.review_state.as_ref())
            .and_then(|review| review.active_attempt.as_ref())
            .is_some_and(|attempt| attempt.contract_revision == 1),
        "a same-target contract revision preserves the exact historical failure revision"
    );
    Ok(FailedReviewFixture {
        session,
        rollout_path,
        task_workspace,
        target_artifact_ref,
        first_child_id,
        first_turn_id: first_turn_id.to_string(),
        first_attempt_id: first_attempt_id.to_string(),
    })
}

async fn setup_completed_unadopted_retry() -> anyhow::Result<(FailedReviewFixture, ThreadId, String)>
{
    let fixture = setup_failed_review().await?;
    let session = &fixture.session;
    let retry = CompletionReviewRetryRequest {
        unit_id: REVIEW_UNIT_ID.into(),
        expected_attempt_id: fixture.first_attempt_id.clone(),
        invocation_call_id: "followup-retry-plan-review".into(),
        previous_plan: PREVIOUS_PLAN.into(),
        revised_plan: REVISED_PLAN.into(),
        reason: RETRY_REASON.into(),
    };
    let retry_call_id = retry.invocation_call_id.clone();
    session
        .record_autonomous_plan_review(
            ROOT_TASK_ID,
            &retry_call_id,
            retry.previous_plan.clone(),
            retry.revised_plan.clone(),
            retry.reason.clone(),
            Some(retry),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    let launch = session
        .prepare_completion_review_launch(
            TARGET_TASK_ID,
            Some(REVIEW_UNIT_ID),
            "followup-retry-spawn",
            Some(REVIEWER_ROLE),
            Some(REVIEWER_MODEL),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    anyhow::ensure!(
        launch.should_spawn,
        "exact retry grant reserves the second attempt"
    );
    let child_id =
        ThreadId::try_from("01a1c606-48b8-7cf3-bb78-77b47f6b8c33").expect("valid retry child ID");
    let turn_id = "followup-retry-turn".to_string();
    spawn_and_bind_attempt(
        session,
        REVIEW_UNIT_ID,
        &fixture.target_artifact_ref,
        "followup-retry-spawn",
        child_id,
        &turn_id,
    )
    .await?;
    session
        .collect_completion_review_results(
            "followup-retry-wait",
            &[(
                child_id,
                Some(turn_id.clone()),
                AgentStatus::Completed(Some("The accepted artifact meets AC-1".into())),
            )],
        )
        .await;
    observe_native_wait_result(
        session,
        child_id,
        AgentStatus::Completed(Some("The accepted artifact meets AC-1".into())),
    )
    .await;
    session.flush_rollout().await?;
    let completed = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("successful retry remains unadopted in the completion contract");
    anyhow::ensure!(completed.units.iter().any(|unit| {
        unit.id == REVIEW_UNIT_ID
            && unit.status == UnitStatus::Running
            && unit.review_state.as_ref().is_some_and(|review| {
                review.previous_attempt_ref.is_some()
                    && review.active_attempt.as_ref().is_some_and(|attempt| {
                        attempt.attempt_id == "followup-retry-spawn"
                            && attempt.status
                                == codex_history::task_context::CompletionReviewAttemptStatus::Completed
                            && attempt.reviewer_turn_id.as_deref() == Some(turn_id.as_str())
                            && attempt.adoption_reason.is_none()
                            && attempt.wait_result_ref.is_some()
                            && attempt.wait_call_id.as_deref() == Some("followup-retry-wait")
                    })
            })
    }), "fixture must contain one exact native-wait completed, unadopted attempt");
    Ok((fixture, child_id, turn_id))
}

async fn create_persisted_review_thread(session: &Session, child_id: ThreadId) -> LiveThread {
    let config = session.get_config().await;
    LiveThread::create(
        Arc::clone(&session.services.thread_store),
        CreateThreadParams {
            creator_user_id: None,
            creator_account_id: None,
            session_id: session.session_id(),
            thread_id: child_id,
            extra_config: None,
            forked_from_id: None,
            parent_thread_id: Some(session.thread_id()),
            source: SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: session.thread_id(),
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some(REVIEWER_ROLE.into()),
            }),
            thread_source: None,
            originator: "test_originator".into(),
            base_instructions: BaseInstructions::default(),
            dynamic_tools: Vec::new(),
            selected_capability_roots: Vec::new(),
            multi_agent_version: None,
            history_mode: ThreadHistoryMode::Legacy,
            history_base: None,
            subagent_history_start_ordinal: None,
            initial_window_id: uuid::Uuid::now_v7().to_string(),
            runtime_workspace_roots: None,
            metadata: ThreadPersistenceMetadata {
                cwd: Some(config.cwd.to_path_buf()),
                model_provider: config.model_provider_id.clone(),
                memory_mode: ThreadMemoryMode::Disabled,
            },
        },
    )
    .await
    .expect("create persisted reviewer thread for cold-restore fixture")
}

#[tokio::test]
async fn reviewer_followup_started_attempt_waits_for_its_own_native_result_and_restores()
-> anyhow::Result<()> {
    let (fixture, child_id, old_turn_id) = setup_completed_unadopted_retry().await?;
    let session = &fixture.session;
    let child_id_string = child_id.to_string();
    let prior_state = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("completed review state before follow-up");
    let prior_unit = prior_state
        .units
        .iter()
        .find(|unit| unit.id == REVIEW_UNIT_ID)
        .expect("review obligation remains registered");
    let prior_attempt = prior_unit
        .review_state
        .as_ref()
        .and_then(|review| review.active_attempt.clone())
        .expect("exact completed attempt is retained");
    let failed_attempt_ref = prior_unit
        .review_state
        .as_ref()
        .and_then(|review| review.previous_attempt_ref.clone())
        .expect("successful retry retains its prior failed snapshot reference");
    assert!(prior_attempt.consumed_retry_authorization.is_some());
    assert!(
        !session
            .state
            .lock()
            .await
            .rencrow_completion_uncollected_children
            .contains(&child_id_string),
        "native wait must have collected the original completed reviewer child"
    );

    let unsubmitted_call_id = "followup-send-input-not-submitted";
    let preparation = session
        .prepare_completion_review_followup(child_id, unsubmitted_call_id)
        .await
        .map_err(anyhow::Error::msg)?
        .expect("exact completed unadopted child is eligible for follow-up");
    assert_eq!(preparation.root_task_id, ROOT_TASK_ID);
    assert_eq!(preparation.unit_id, REVIEW_UNIT_ID);
    assert_eq!(preparation.child_thread_id, child_id);
    assert_eq!(preparation.call_id, unsubmitted_call_id);
    assert_eq!(preparation.prior_unit, *prior_unit);
    assert_eq!(
        preparation
            .prior_unit
            .review_state
            .as_ref()
            .and_then(|review| review.active_attempt.as_ref()),
        Some(&prior_attempt)
    );
    let prior_snapshot_ref = preparation
        .reserved_unit
        .review_state
        .as_ref()
        .and_then(|review| review.previous_attempt_ref.clone())
        .expect("reserved unit references the exact completed-attempt snapshot");
    assert_eq!(prior_snapshot_ref.version, 2);
    assert_eq!(prior_snapshot_ref.sha256.len(), 64);
    let persisted_before_followup = durable_completion_snapshots(&fixture.rollout_path).await?;
    let prior_attempt_json = serde_json::to_value(&prior_attempt)?;
    let prior_source_snapshot = persisted_before_followup
        .iter()
        .find(|snapshot| {
            durable_unit(snapshot, REVIEW_UNIT_ID)["review_state"]["active_attempt"]
                == prior_attempt_json
        })
        .ok_or_else(|| anyhow::anyhow!("the exact completed source snapshot is persisted"))?;
    assert_eq!(
        durable_unit(prior_source_snapshot, REVIEW_UNIT_ID)["review_state"]["previous_attempt_ref"],
        serde_json::to_value(&failed_attempt_ref)?,
        "the exact completed source snapshot retains the prior failed-attempt reference"
    );
    let prior_adoption = session
        .adopt_completion_review(
            REVIEW_UNIT_ID,
            "The prior completed result must not adopt while follow-up is reserved".into(),
        )
        .await;
    assert!(
        prior_adoption.is_err(),
        "the prior completed result cannot be adopted after a follow-up reservation: {prior_adoption:?}"
    );
    let preflight_state = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("preflight records the outgoing follow-up reservation");
    let preflight_attempt = preflight_state
        .units
        .iter()
        .find(|unit| unit.id == REVIEW_UNIT_ID)
        .and_then(|unit| unit.review_state.as_ref())
        .and_then(|review| review.active_attempt.as_ref())
        .expect("preflight reservation is canonical");
    assert_eq!(preflight_attempt.attempt_id, unsubmitted_call_id);
    assert_eq!(
        preflight_attempt.status,
        codex_history::task_context::CompletionReviewAttemptStatus::Reserved
    );
    assert!(preflight_attempt.reviewer_turn_id.is_none());
    assert!(preflight_attempt.terminal_source.is_none());
    assert!(preflight_attempt.wait_result_ref.is_none());
    assert!(preflight_attempt.wait_call_id.is_none());
    assert!(preflight_attempt.adoption_reason.is_none());
    assert_eq!(
        preflight_state
            .units
            .iter()
            .find(|unit| unit.id == REVIEW_UNIT_ID)
            .and_then(|unit| unit.review_state.as_ref())
            .and_then(|review| review.previous_attempt_ref.as_ref()),
        Some(&prior_snapshot_ref),
        "preflight links the prior exact attempt snapshot"
    );
    assert_eq!(
        preflight_state
            .units
            .iter()
            .find(|unit| unit.id == REVIEW_UNIT_ID),
        Some(&preparation.reserved_unit),
        "the host snapshot exposes the same reservation prepared before the side effect"
    );
    assert!(
        session
            .state
            .lock()
            .await
            .rencrow_completion_uncollected_children
            .contains(&child_id.to_string())
    );
    let preflight_snapshot = latest_durable_completion_snapshot(&fixture.rollout_path).await?;
    assert!(
        preflight_snapshot["uncollected_children"]
            .as_array()
            .is_some_and(|children| children
                .iter()
                .any(|child| child.as_str() == Some(child_id_string.as_str())))
    );
    assert_eq!(
        durable_unit(&preflight_snapshot, REVIEW_UNIT_ID)["review_state"]["previous_attempt_ref"],
        serde_json::to_value(&prior_snapshot_ref)?,
        "preflight points to the immediately preceding completed-attempt snapshot"
    );

    session
        .cancel_completion_review_followup_not_submitted(preparation)
        .await
        .map_err(anyhow::Error::msg)?;
    let after_not_submitted = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("NotSubmitted rollback keeps the completion contract");
    assert_eq!(
        after_not_submitted
            .units
            .iter()
            .find(|unit| unit.id == REVIEW_UNIT_ID),
        Some(prior_unit),
        "known NotSubmitted restores the prior review unit exactly"
    );
    assert!(
        !session
            .state
            .lock()
            .await
            .rencrow_completion_uncollected_children
            .contains(&child_id.to_string())
    );
    let cancelled_snapshot = latest_durable_completion_snapshot(&fixture.rollout_path).await?;
    assert!(
        !cancelled_snapshot["uncollected_children"]
            .as_array()
            .is_some_and(|children| children
                .iter()
                .any(|child| child.as_str() == Some(child_id_string.as_str())))
    );

    let preparation = session
        .prepare_completion_review_followup(child_id, "followup-send-input-started")
        .await
        .map_err(anyhow::Error::msg)?
        .expect("follow-up can be prepared again after proven NotSubmitted");
    let prior_snapshot_ref = preparation
        .reserved_unit
        .review_state
        .as_ref()
        .and_then(|review| review.previous_attempt_ref.clone())
        .expect("Reserved follow-up unit retains its exact predecessor snapshot reference");
    let followup_turn_id = "followup-review-new-turn".to_string();
    session
        .record_completion_review_followup_started(preparation, followup_turn_id.clone())
        .await
        .map_err(anyhow::Error::msg)?;
    let started = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("started generation is recorded");
    let started_review = started
        .units
        .iter()
        .find(|unit| unit.id == REVIEW_UNIT_ID)
        .expect("same review obligation remains registered");
    let started_attempt = started_review
        .review_state
        .as_ref()
        .and_then(|review| review.active_attempt.as_ref())
        .expect("follow-up is the canonical active attempt");
    assert_eq!(started_attempt.attempt_id, "followup-send-input-started");
    assert_eq!(
        started_attempt.reviewer_turn_id.as_deref(),
        Some(followup_turn_id.as_str())
    );
    assert_eq!(
        started_attempt.reviewer_thread_id.as_deref(),
        Some(child_id_string.as_str())
    );
    assert_eq!(
        started_attempt.status,
        codex_history::task_context::CompletionReviewAttemptStatus::Bound
    );
    assert!(started_attempt.terminal_source.is_none());
    assert!(started_attempt.wait_result_ref.is_none());
    assert!(started_attempt.wait_call_id.is_none());
    assert!(started_attempt.deadline_escalation.is_none());
    assert_eq!(
        started_review
            .review_state
            .as_ref()
            .and_then(|review| review.previous_attempt_ref.as_ref()),
        Some(&prior_snapshot_ref),
        "the new generation references the exact unadopted prior snapshot"
    );
    assert!(started_attempt.consumed_retry_authorization.is_none());
    assert_ne!(started_attempt.attempt_id, prior_attempt.attempt_id);
    assert_ne!(
        started_attempt.reviewer_turn_id.as_deref(),
        Some(old_turn_id.as_str())
    );

    session
        .collect_completion_review_results(
            "wait-followup-old-generation",
            &[(
                child_id,
                Some(old_turn_id.clone()),
                AgentStatus::Completed(Some("stale result from the prior turn".into())),
            )],
        )
        .await;
    // This is a collector-only stale receipt check: it is not a successful native wait
    // observation and must not clear the follow-up child's uncollected ledger entry.
    assert_eq!(
        session.rencrow_completion_state(ROOT_TASK_ID).await,
        Some(started.clone()),
        "the old turn's wait result cannot complete or replace the follow-up attempt"
    );

    session
        .collect_completion_review_results(
            "wait-followup-new-generation",
            &[(
                child_id,
                Some(followup_turn_id.clone()),
                AgentStatus::Completed(Some("The follow-up confirms AC-1".into())),
            )],
        )
        .await;
    observe_native_wait_result(
        session,
        child_id,
        AgentStatus::Completed(Some("The follow-up confirms AC-1".into())),
    )
    .await;
    session.flush_rollout().await?;
    let collected = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("new native wait result is collected");
    let collected_review = collected
        .units
        .iter()
        .find(|unit| unit.id == REVIEW_UNIT_ID)
        .expect("same review obligation remains registered after wait");
    assert!(
        collected_review
            .review_state
            .as_ref()
            .and_then(|review| review.active_attempt.as_ref())
            .is_some_and(|attempt| {
                attempt.attempt_id == "followup-send-input-started"
                    && attempt.status
                        == codex_history::task_context::CompletionReviewAttemptStatus::Completed
                    && attempt.wait_call_id.as_deref() == Some("wait-followup-new-generation")
                    && attempt.adoption_reason.is_none()
            })
    );
    session
        .adopt_completion_review(
            REVIEW_UNIT_ID,
            "The follow-up reviewer confirmed AC-1".into(),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    assert!(session.autonomous_completion_pending().await);
    withdraw_unlaunched_review(
        session,
        "review-a-legacy-planned",
        "withdraw-followup-independent-planned-review",
    )
    .await?;
    assert!(!session.autonomous_completion_pending().await);
    assert!(session.autonomous_completion_is_complete().await);

    let snapshots = durable_completion_snapshots(&fixture.rollout_path).await?;
    anyhow::ensure!(snapshots.iter().any(|snapshot| {
        durable_unit(snapshot, REVIEW_UNIT_ID)["review_state"]["active_attempt"]["attempt_id"]
            == fixture.first_attempt_id
            && durable_unit(snapshot, REVIEW_UNIT_ID)["review_state"]["active_attempt"]["status"]
                == "failed"
    }), "the failed retry source remains in the persisted snapshot chain");
    cold_restore_completion_history(session, &fixture.rollout_path).await?;
    assert!(
        session
            .latest_rencrow_task_state()
            .await?
            .is_some_and(|accepted| accepted.completed.contains_key(TARGET_TASK_ID))
    );
    assert!(!session.autonomous_completion_pending().await);
    assert!(session.autonomous_completion_is_complete().await);
    let restored = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("restored follow-up adoption remains registered");
    let restored_review = restored
        .units
        .iter()
        .find(|unit| unit.id == REVIEW_UNIT_ID)
        .expect("restored review obligation remains registered")
        .review_state
        .as_ref()
        .expect("restored canonical review state");
    assert_eq!(
        restored_review.previous_attempt_ref.as_ref(),
        Some(&prior_snapshot_ref)
    );
    assert_eq!(
        restored_review
            .active_attempt
            .as_ref()
            .map(|attempt| attempt.attempt_id.as_str()),
        Some("followup-send-input-started")
    );
    Ok(())
}

#[tokio::test]
async fn reserved_followup_cold_restore_rejects_old_turn_then_binds_verified_turn()
-> anyhow::Result<()> {
    let (fixture, child_id, old_turn_id) = setup_completed_unadopted_retry().await?;
    let session = &fixture.session;
    let child_id_string = child_id.to_string();
    let completed_before = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("completed review obligation before send-input reservation");

    // An exact Completed candidate with an already-uncollected child is a duplicate;
    // the owner must reject it without changing the canonical completion state.
    session
        .state
        .lock()
        .await
        .rencrow_completion_uncollected_children
        .insert(child_id_string.clone());
    session
        .persist_autonomous_completion_snapshot()
        .await
        .map_err(anyhow::Error::msg)?;
    assert!(
        session
            .prepare_completion_review_followup(child_id, "duplicate-followup-send-input")
            .await
            .is_err()
    );
    assert_eq!(
        session.rencrow_completion_state(ROOT_TASK_ID).await,
        Some(completed_before.clone())
    );
    session
        .state
        .lock()
        .await
        .rencrow_completion_uncollected_children
        .remove(&child_id_string);
    session
        .persist_autonomous_completion_snapshot()
        .await
        .map_err(anyhow::Error::msg)?;

    let send_input_call_id = "reserved-followup-before-crash";
    let preparation = session
        .prepare_completion_review_followup(child_id, send_input_call_id)
        .await
        .map_err(anyhow::Error::msg)?
        .expect("completed exact child can reserve a follow-up");
    let reserved_snapshot_ref = preparation
        .reserved_unit
        .review_state
        .as_ref()
        .and_then(|review| review.previous_attempt_ref.clone())
        .expect("Reserved unit carries the exact prior snapshot reference");
    let reserved = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("pre-effect Reserved attempt is persisted");
    let reserved_attempt = reserved
        .units
        .iter()
        .find(|unit| unit.id == REVIEW_UNIT_ID)
        .and_then(|unit| unit.review_state.as_ref())
        .and_then(|review| review.active_attempt.as_ref())
        .expect("Reserved attempt is canonical before the send-input effect");
    assert_eq!(reserved_attempt.attempt_id, send_input_call_id);
    assert_eq!(
        reserved_attempt.status,
        codex_history::task_context::CompletionReviewAttemptStatus::Reserved
    );
    assert!(reserved_attempt.reviewer_turn_id.is_none());
    assert_eq!(
        reserved
            .units
            .iter()
            .find(|unit| unit.id == REVIEW_UNIT_ID)
            .and_then(|unit| unit.review_state.as_ref())
            .and_then(|review| review.previous_attempt_ref.as_ref()),
        Some(&reserved_snapshot_ref)
    );

    // Rebuild history and state from the persisted rollout before testing recovery.
    cold_restore_completion_history(session, &fixture.rollout_path).await?;
    let restored_reserved = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("cold restored reservation remains registered");
    assert_eq!(restored_reserved, reserved);
    assert!(
        session
            .state
            .lock()
            .await
            .rencrow_completion_uncollected_children
            .contains(&child_id_string)
    );

    let identity = |turn_id: String| CompletionReviewChildGeneration {
        identity: CompletionReviewChildIdentity {
            target: CompletionReviewTarget {
                task_id: TARGET_TASK_ID.into(),
                task_revision: 1,
            },
            target_artifact_ref: fixture.target_artifact_ref.clone(),
            reviewer_thread_id: child_id,
            reviewer_role: REVIEWER_ROLE.into(),
            spawn_call_id: send_input_call_id.into(),
        },
        reviewer_turn_id: turn_id,
    };
    assert!(
        session
            .record_completion_review_spawn(REVIEW_UNIT_ID, identity(old_turn_id.clone()))
            .await
            .is_err(),
        "the predecessor's exact completed turn cannot bind after Reserved restore"
    );
    assert_eq!(
        session.rencrow_completion_state(ROOT_TASK_ID).await,
        Some(restored_reserved.clone()),
        "old-turn refusal leaves the persisted Reserved attempt unchanged"
    );

    let confirmed_turn_id = "reserved-followup-confirmed-new-turn".to_string();
    session
        .record_completion_review_spawn(REVIEW_UNIT_ID, identity(confirmed_turn_id.clone()))
        .await
        .map_err(anyhow::Error::msg)?;
    let bound = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("the other host's confirmed new turn binds the Reserved attempt");
    let bound_attempt = bound
        .units
        .iter()
        .find(|unit| unit.id == REVIEW_UNIT_ID)
        .and_then(|unit| unit.review_state.as_ref())
        .and_then(|review| review.active_attempt.as_ref())
        .expect("new exact turn becomes the active generation");
    assert_eq!(bound_attempt.attempt_id, send_input_call_id);
    assert_eq!(
        bound_attempt.reviewer_turn_id.as_deref(),
        Some(confirmed_turn_id.as_str())
    );
    assert_eq!(
        bound_attempt.status,
        codex_history::task_context::CompletionReviewAttemptStatus::Bound
    );
    assert!(
        session
            .prepare_completion_review_followup(child_id, "not-a-second-followup")
            .await
            .map_err(anyhow::Error::msg)?
            .is_none(),
        "a Bound child with uncollected work is not an eligible Completed candidate"
    );
    assert_eq!(
        session.rencrow_completion_state(ROOT_TASK_ID).await,
        Some(bound)
    );

    session
        .collect_completion_review_results(
            "reserved-followup-native-wait",
            &[(
                child_id,
                Some(confirmed_turn_id.clone()),
                AgentStatus::Completed(Some("The follow-up confirms AC-1".into())),
            )],
        )
        .await;
    observe_native_wait_result(
        session,
        child_id,
        AgentStatus::Completed(Some("The follow-up confirms AC-1".into())),
    )
    .await;
    session
        .adopt_completion_review(
            REVIEW_UNIT_ID,
            "The recovered follow-up reviewer confirmed AC-1".into(),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    assert!(session.autonomous_completion_pending().await);
    withdraw_unlaunched_review(
        session,
        "review-a-legacy-planned",
        "withdraw-restored-followup-independent-planned-review",
    )
    .await?;
    assert!(!session.autonomous_completion_pending().await);
    assert!(session.autonomous_completion_is_complete().await);
    Ok(())
}

#[tokio::test]
async fn opaque_reviewer_followup_keeps_prior_child_uncollected_without_fake_turn()
-> anyhow::Result<()> {
    let (fixture, child_id, old_turn_id) = setup_completed_unadopted_retry().await?;
    let session = &fixture.session;
    let child_id_string = child_id.to_string();
    let before = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("completed unadopted reviewer generation");
    let prior_attempt = before.units[1]
        .review_state
        .as_ref()
        .and_then(|review| review.active_attempt.clone())
        .expect("prior completed attempt is canonical");
    let preparation = session
        .prepare_completion_review_followup(child_id, "followup-send-input-opaque")
        .await
        .map_err(anyhow::Error::msg)?
        .expect("exact completed child can be prepared");
    let previous_snapshot_ref = preparation
        .reserved_unit
        .review_state
        .as_ref()
        .and_then(|review| review.previous_attempt_ref.clone())
        .expect("opaque reservation carries the exact prior snapshot reference");
    session
        .record_completion_review_followup_opaque_or_ambiguous(
            preparation,
            "native submission steered or had an ambiguous outcome".into(),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    assert!(session.autonomous_completion_paused().await);
    assert!(
        session
            .state
            .lock()
            .await
            .rencrow_completion_uncollected_children
            .contains(&child_id.to_string())
    );
    let opaque_snapshot = latest_durable_completion_snapshot(&fixture.rollout_path).await?;
    assert!(
        opaque_snapshot["uncollected_children"]
            .as_array()
            .is_some_and(|children| {
                children
                    .iter()
                    .any(|child| child.as_str() == Some(child_id_string.as_str()))
            })
    );
    let after = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("opaque follow-up preserves the host completion state");
    let after_review = after
        .units
        .iter()
        .find(|unit| unit.id == REVIEW_UNIT_ID)
        .expect("opaque follow-up preserves the review obligation");
    let opaque_attempt = after_review
        .review_state
        .as_ref()
        .and_then(|review| review.active_attempt.as_ref())
        .expect("opaque follow-up retains its reserved attempt");
    assert_eq!(opaque_attempt.attempt_id, "followup-send-input-opaque");
    assert_eq!(
        opaque_attempt.status,
        codex_history::task_context::CompletionReviewAttemptStatus::Unknown
    );
    assert!(
        opaque_attempt.reviewer_turn_id.is_none(),
        "no unobserved turn id is invented"
    );
    assert!(opaque_attempt.launch_unknown_reason.is_some());
    assert!(opaque_attempt.terminal_source.is_none());
    assert!(opaque_attempt.wait_result_ref.is_none());
    assert!(opaque_attempt.wait_call_id.is_none());
    assert!(opaque_attempt.adoption_reason.is_none());
    assert!(opaque_attempt.deadline_escalation.is_none());
    assert_ne!(opaque_attempt, &prior_attempt);
    let unknown_attempt = opaque_attempt.clone();
    assert_eq!(after_review.status, UnitStatus::Running);
    assert_eq!(
        after_review
            .review_state
            .as_ref()
            .and_then(|review| review.previous_attempt_ref.as_ref()),
        Some(&previous_snapshot_ref),
        "the exact old completed evidence remains reachable through its persisted snapshot"
    );

    // This Session seam explicitly resumes the persisted host state; it does not simulate a
    // public user turn. Reconciliation must still reject reusing the predecessor's completed
    // turn as the Unknown follow-up generation.
    assert!(
        session
            .resume_autonomous_completion_from_user_turn()
            .await
            .map_err(anyhow::Error::msg)?
    );
    assert!(!session.autonomous_completion_paused().await);
    let stale_generation = CompletionReviewChildGeneration {
        identity: CompletionReviewChildIdentity {
            target: CompletionReviewTarget {
                task_id: TARGET_TASK_ID.into(),
                task_revision: 1,
            },
            target_artifact_ref: fixture.target_artifact_ref.clone(),
            reviewer_thread_id: child_id,
            reviewer_role: REVIEWER_ROLE.into(),
            spawn_call_id: "followup-send-input-opaque".into(),
        },
        reviewer_turn_id: old_turn_id,
    };
    assert!(
        session
            .record_completion_review_spawn(REVIEW_UNIT_ID, stale_generation)
            .await
            .is_err(),
        "the old completed turn cannot bind to a new Unknown attempt"
    );
    let after_stale_bind = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("old-turn rebind refusal preserves the Unknown follow-up");
    assert_eq!(
        after_stale_bind
            .units
            .iter()
            .find(|unit| unit.id == REVIEW_UNIT_ID)
            .and_then(|unit| unit.review_state.as_ref())
            .and_then(|review| review.active_attempt.as_ref()),
        Some(&unknown_attempt),
        "the predecessor's completed turn cannot change the active Unknown attempt"
    );
    assert!(
        session
            .state
            .lock()
            .await
            .rencrow_completion_uncollected_children
            .contains(&child_id_string)
    );
    Ok(())
}

async fn latest_durable_completion_snapshot(path: &Path) -> anyhow::Result<Value> {
    durable_completion_snapshots(path)
        .await?
        .into_iter()
        .last()
        .ok_or_else(|| anyhow::anyhow!("durable completion snapshot is missing"))
}

async fn durable_completion_snapshots(path: &Path) -> anyhow::Result<Vec<Value>> {
    let InitialHistory::Resumed(resumed) = RolloutRecorder::get_rollout_history(path).await? else {
        anyhow::bail!("expected the persisted thread rollout to resume");
    };
    Ok(resumed
        .history
        .iter()
        .filter_map(|item| match item {
            RolloutItem::ResponseItem(envelope) => envelope
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.rencrow_completion.clone()),
            _ => None,
        })
        .collect())
}

async fn cold_restore_completion_history(session: &Session, path: &Path) -> anyhow::Result<()> {
    session.flush_rollout().await?;
    let InitialHistory::Resumed(resumed) = RolloutRecorder::get_rollout_history(path).await? else {
        anyhow::bail!("completion fixture rollout must be reloadable");
    };
    cold_restore_completion_items(session, resumed.history.as_ref()).await;
    Ok(())
}

async fn cold_restore_completion_items(session: &Session, items: &[RolloutItem]) {
    {
        let mut state = session.state.lock().await;
        state.rencrow_completion_state = None;
        state.rencrow_completion_paused = false;
        state.task_check_ledger = crate::task_check::TaskCheckLedger::default();
        state.rencrow_completion_last_persisted_snapshot = None;
        let cold_response_history = items
            .iter()
            .filter_map(|item| match item {
                RolloutItem::ResponseItem(envelope) => Some(envelope.clone()),
                _ => None,
            })
            .collect();
        state.history.replace_annotated(cold_response_history);
    }
    session.restore_autonomous_completion_snapshot(items).await;
}

fn durable_unit<'a>(snapshot: &'a Value, unit_id: &str) -> &'a Value {
    snapshot["state"]["units"]
        .as_array()
        .and_then(|units| units.iter().find(|unit| unit["id"] == unit_id))
        .expect("durable completion unit")
}

#[tokio::test]
async fn completion_update_preserves_host_plan_history_when_omitted_or_forged() -> anyhow::Result<()>
{
    let (fixture, _, _) = setup_completed_unadopted_retry().await?;
    let session = &fixture.session;
    let before = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("host retry contract exists");
    anyhow::ensure!(
        !before.plan_reviews.is_empty(),
        "fixture has a host retry decision"
    );

    let mut omitted = serde_json::to_value(&before)?;
    omitted.as_object_mut().unwrap().remove("plan_reviews");
    session
        .set_rencrow_completion_state(ROOT_TASK_ID.into(), omitted)
        .await
        .map_err(anyhow::Error::msg)?;
    let after = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .unwrap();
    assert_eq!(after.plan_reviews, before.plan_reviews);

    let mut forged = serde_json::to_value(&after)?;
    forged["plan_reviews"][0]["reason"] = serde_json::json!("model-authored replacement");
    session
        .set_rencrow_completion_state(ROOT_TASK_ID.into(), forged)
        .await
        .map_err(anyhow::Error::msg)?;
    assert_eq!(
        session
            .rencrow_completion_state(ROOT_TASK_ID)
            .await
            .unwrap()
            .plan_reviews,
        before.plan_reviews
    );
    Ok(())
}

#[tokio::test]
async fn cold_restore_accepts_legacy_review_ended_retry_terminal_and_adjacent_wait_collection()
-> anyhow::Result<()> {
    let fixture = setup_failed_review().await?;
    let session = &fixture.session;

    let second_retry = CompletionReviewRetryRequest {
        unit_id: REVIEW_UNIT_ID.into(),
        expected_attempt_id: fixture.first_attempt_id.clone(),
        invocation_call_id: "cold-restore-second-plan-review".into(),
        previous_plan: session
            .rencrow_completion_state(ROOT_TASK_ID)
            .await
            .and_then(|state| state.next_action)
            .expect("copy the current host plan before the second retry"),
        revised_plan: REVISED_PLAN.into(),
        reason: RETRY_REASON.into(),
    };
    let second_retry_call_id = second_retry.invocation_call_id.clone();
    session
        .record_autonomous_plan_review(
            ROOT_TASK_ID,
            &second_retry_call_id,
            second_retry.previous_plan.clone(),
            second_retry.revised_plan.clone(),
            second_retry.reason.clone(),
            Some(second_retry),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    let second_launch = session
        .prepare_completion_review_launch(
            TARGET_TASK_ID,
            Some(REVIEW_UNIT_ID),
            "cold-restore-second-spawn",
            Some(REVIEWER_ROLE),
            Some(REVIEWER_MODEL),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    anyhow::ensure!(second_launch.should_spawn, "second retry must be reserved");
    let second_child_id =
        ThreadId::try_from("01a1c606-48b8-7cf3-bb78-77b47f6b8c44").expect("valid child ID");
    let second_turn_id = "cold-restore-second-turn";
    spawn_and_bind_attempt(
        session,
        REVIEW_UNIT_ID,
        &fixture.target_artifact_ref,
        "cold-restore-second-spawn",
        second_child_id,
        second_turn_id,
    )
    .await?;
    session
        .collect_completion_review_results(
            "cold-restore-second-wait",
            &[(
                second_child_id,
                Some(second_turn_id.into()),
                AgentStatus::Errored("second exact reviewer failure".into()),
            )],
        )
        .await;
    observe_native_wait_result(
        session,
        second_child_id,
        AgentStatus::Errored("second exact reviewer failure".into()),
    )
    .await;

    let third_retry = CompletionReviewRetryRequest {
        unit_id: REVIEW_UNIT_ID.into(),
        expected_attempt_id: "cold-restore-second-spawn".into(),
        invocation_call_id: "cold-restore-third-plan-review".into(),
        previous_plan: session
            .rencrow_completion_state(ROOT_TASK_ID)
            .await
            .and_then(|state| state.next_action)
            .expect("copy the current host plan before the third retry"),
        revised_plan: "Collect the third exact retry and verify its cold restoration".into(),
        reason: RETRY_REASON.into(),
    };
    let third_retry_call_id = third_retry.invocation_call_id.clone();
    session
        .record_autonomous_plan_review(
            ROOT_TASK_ID,
            &third_retry_call_id,
            third_retry.previous_plan.clone(),
            third_retry.revised_plan.clone(),
            third_retry.reason.clone(),
            Some(third_retry),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    let third_launch = session
        .prepare_completion_review_launch(
            TARGET_TASK_ID,
            Some(REVIEW_UNIT_ID),
            "cold-restore-third-spawn",
            Some(REVIEWER_ROLE),
            Some(REVIEWER_MODEL),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    anyhow::ensure!(third_launch.should_spawn, "third retry must be reserved");
    let third_child_id =
        ThreadId::try_from("01a1c606-48b8-7cf3-bb78-77b47f6b8c45").expect("valid child ID");
    let third_turn_id = "cold-restore-third-turn";
    spawn_and_bind_attempt(
        session,
        REVIEW_UNIT_ID,
        &fixture.target_artifact_ref,
        "cold-restore-third-spawn",
        third_child_id,
        third_turn_id,
    )
    .await?;
    let bound = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("third bound retry is present");
    anyhow::ensure!(
        bound.units.iter().any(|unit| {
            unit.review_state.as_ref().is_some_and(|review| {
                review.retry_reservation_prior_historical_failure.is_some()
                    && review.active_attempt.as_ref().is_some_and(|attempt| {
                        attempt.attempt_id == "cold-restore-third-spawn"
                            && attempt.status == CompletionReviewAttemptStatus::Bound
                    })
            })
        }),
        "third bound retry must retain the rollback history that triggered the old bug"
    );

    let child = create_persisted_review_thread(session, third_child_id).await;
    child
        .append_items(&[RolloutItem::EventMsg(EventMsg::TurnAborted(
            TurnAbortedEvent {
                turn_id: Some(third_turn_id.into()),
                reason: TurnAbortReason::ReviewEnded,
                started_at: None,
                completed_at: None,
                duration_ms: None,
            },
        ))])
        .await
        .expect("persist exact typed ReviewEnded event in the reviewer thread");
    let proof = child
        .read_persisted_turn_abort(third_turn_id.into())
        .await
        .expect("read canonical typed reviewer abort proof")
        .expect("exact reviewer abort event exists");
    let mut terminal = bound.clone();
    let safety_fingerprint =
        crate::session::completion_review_deadline::review_ended_durable_fingerprint(third_turn_id);
    let escalation = "fixture reviewer exceeded the host SLA".to_string();
    let terminal_unit = terminal
        .units
        .iter_mut()
        .find(|unit| unit.id == REVIEW_UNIT_ID)
        .expect("review unit exists");
    terminal_unit.status = UnitStatus::Failed;
    let terminal_attempt = terminal_unit
        .review_state
        .as_mut()
        .and_then(|review| review.active_attempt.as_mut())
        .expect("third attempt exists");
    terminal_attempt.status = CompletionReviewAttemptStatus::Failed;
    terminal_attempt.deadline_escalation = Some(escalation.clone());
    terminal_attempt.deadline_safety_fingerprint = Some(safety_fingerprint.clone());
    terminal_attempt.terminal_source = Some(CompletionReviewTerminalSource {
        status: CompletionReviewTerminalStatus::Failed,
        event: Some(CompletionReviewTerminalEvent {
            rollout_id: proof.source.rollout_id.to_string(),
            locator: match proof.source.locator {
                PersistedTurnAbortLocator::Ordinal(value) => {
                    CompletionReviewRecordLocator::Ordinal(value)
                }
                PersistedTurnAbortLocator::LegacyLineIndex(value) => {
                    CompletionReviewRecordLocator::LegacyLineIndex(value)
                }
            },
            owner_thread_id: proof.owner.thread_id.to_string(),
            turn_id: proof.event.turn_id.clone().expect("exact turn id"),
            owner_parent_thread_id: proof.owner.parent_thread_id.map(|id| id.to_string()),
            owner_agent_role: proof.owner.agent_role,
            reason: format!("{:?}", proof.event.reason),
        }),
    });
    *terminal_unit = codex_history::task_context::project_completion_review_state(terminal_unit)
        .map_err(anyhow::Error::msg)?;
    terminal.blocker = Some(codex_history::task_context::CompletionBlocker {
        category: codex_history::task_context::BlockerCategory::Unknown,
        reason: escalation,
        required_input: None,
        authority: None,
        evidence: Vec::new(),
        recheck_condition: None,
        review_owner: Some(codex_history::task_context::CompletionReviewBlockerOwner {
            unit_id: REVIEW_UNIT_ID.into(),
            attempt_id: "cold-restore-third-spawn".into(),
            safety_fingerprint,
        }),
    });
    session.state.lock().await.rencrow_completion_state = Some((ROOT_TASK_ID.into(), terminal));
    session
        .persist_autonomous_completion_snapshot()
        .await
        .map_err(anyhow::Error::msg)?;

    session
        .collect_completion_review_results(
            "cold-restore-third-wait",
            &[(
                third_child_id,
                Some(third_turn_id.into()),
                AgentStatus::Errored("ReviewEnded".into()),
            )],
        )
        .await;
    observe_native_wait_result(
        session,
        third_child_id,
        AgentStatus::Errored("ReviewEnded".into()),
    )
    .await;
    let collected = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("native wait collection retains the failed review");
    session.flush_rollout().await?;
    cold_restore_completion_history(session, &fixture.rollout_path).await?;

    assert!(!session.autonomous_completion_paused().await);
    let restored = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("legacy terminal-to-wait pair restores completion history");
    assert_eq!(restored, collected);
    assert_eq!(
        restored.conditions, bound.conditions,
        "the acceptance conditions remain attached through the legacy transition"
    );
    assert!(restored.blocker.is_some());
    assert!(restored.units.iter().any(|unit| {
        unit.id == REVIEW_UNIT_ID
            && unit.status == UnitStatus::Failed
            && unit.review_state.as_ref().is_some_and(|review| {
                review.retry_reservation_prior_historical_failure.is_none()
                    && review.active_attempt.as_ref().is_some_and(|attempt| {
                        attempt.attempt_id == "cold-restore-third-spawn"
                            && attempt.status == CompletionReviewAttemptStatus::Failed
                            && attempt.wait_call_id.as_deref() == Some("cold-restore-third-wait")
                    })
            })
    }));

    let InitialHistory::Resumed(resumed_rollout) =
        RolloutRecorder::get_rollout_history(&fixture.rollout_path).await?
    else {
        anyhow::bail!("accepted retry rollout remains reloadable");
    };
    let items = resumed_rollout.history.as_ref().clone();
    let third_terminal_index = items
        .iter()
        .position(|item| match item {
            RolloutItem::ResponseItem(envelope) => envelope
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.rencrow_completion.as_ref())
                .is_some_and(|snapshot| {
                    snapshot["state"]["units"].as_array().is_some_and(|units| {
                        units.iter().any(|unit| {
                            unit["id"] == REVIEW_UNIT_ID
                                && unit["status"] == "failed"
                                && unit["review_state"]["active_attempt"]["attempt_id"]
                                    == "cold-restore-third-spawn"
                                && unit["review_state"]["active_attempt"]["wait_call_id"].is_null()
                        })
                    })
                }),
            _ => false,
        })
        .expect("persisted legacy terminal snapshot is present");
    let collected_index = items
        .iter()
        .position(|item| match item {
            RolloutItem::ResponseItem(envelope) => envelope
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.rencrow_completion.as_ref())
                .is_some_and(|snapshot| {
                    snapshot["state"]["units"].as_array().is_some_and(|units| {
                        units.iter().any(|unit| {
                            unit["id"] == REVIEW_UNIT_ID
                                && unit["review_state"]["active_attempt"]["wait_call_id"]
                                    == "cold-restore-third-wait"
                        })
                    })
                }),
            _ => false,
        })
        .expect("immediately adjacent native wait collection snapshot is present");
    anyhow::ensure!(
        collected_index == third_terminal_index + 1,
        "collected snapshot immediately follows the legacy failed terminal snapshot"
    );

    cold_restore_completion_items(session, &items[..=third_terminal_index]).await;
    assert!(
        session.autonomous_completion_paused().await,
        "a malformed legacy terminal without its immediate wait collection remains fail-closed"
    );

    let mut wrong_generation = items.clone();
    let RolloutItem::ResponseItem(envelope) = &mut wrong_generation[third_terminal_index] else {
        anyhow::bail!("legacy terminal snapshot item is a response envelope");
    };
    let terminal_snapshot = envelope
        .metadata
        .as_mut()
        .and_then(|metadata| metadata.rencrow_completion.as_mut())
        .expect("legacy terminal completion snapshot metadata");
    let terminal_unit = terminal_snapshot["state"]["units"]
        .as_array_mut()
        .and_then(|units| units.iter_mut().find(|unit| unit["id"] == REVIEW_UNIT_ID))
        .expect("terminal review unit");
    terminal_unit["review_state"]["active_attempt"]["attempt_id"] =
        Value::String("different-generation".into());
    cold_restore_completion_items(session, &wrong_generation).await;
    assert!(
        session.autonomous_completion_paused().await,
        "a wait result for a different generation cannot validate the legacy terminal"
    );

    let mut changed_successor = items.clone();
    let RolloutItem::ResponseItem(envelope) = &mut changed_successor[collected_index] else {
        anyhow::bail!("collected snapshot item is a response envelope");
    };
    let successor = envelope
        .metadata
        .as_mut()
        .and_then(|metadata| metadata.rencrow_completion.as_mut())
        .expect("collected completion snapshot metadata");
    successor["state"]["next_action"] = Value::String("unrelated successor mutation".into());
    cold_restore_completion_items(session, &changed_successor).await;
    assert!(
        session.autonomous_completion_paused().await,
        "an extra successor mutation cannot use the legacy transition proof"
    );

    let mut invalid_earlier_history = items.clone();
    let earlier_failure_index = invalid_earlier_history
        .iter()
        .position(|item| match item {
            RolloutItem::ResponseItem(envelope) => envelope
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.rencrow_completion.as_ref())
                .is_some_and(|snapshot| {
                    snapshot["state"]["units"].as_array().is_some_and(|units| {
                        units.iter().any(|unit| {
                            unit["id"] == REVIEW_UNIT_ID
                                && unit["status"] == "failed"
                                && unit["review_state"]["active_attempt"]["attempt_id"]
                                    == fixture.first_attempt_id
                        })
                    })
                }),
            _ => false,
        })
        .expect("first generation's native failure snapshot is present");
    let RolloutItem::ResponseItem(envelope) = &mut invalid_earlier_history[earlier_failure_index]
    else {
        anyhow::bail!("earlier failure snapshot item is a response envelope");
    };
    let earlier = envelope
        .metadata
        .as_mut()
        .and_then(|metadata| metadata.rencrow_completion.as_mut())
        .expect("earlier completion snapshot metadata");
    earlier["state"]["contract_revision"] = serde_json::json!(0);
    cold_restore_completion_items(session, &invalid_earlier_history).await;
    assert!(
        session.autonomous_completion_paused().await,
        "the transition exception cannot repair an independently invalid earlier snapshot"
    );
    Ok(())
}

#[tokio::test]
async fn failed_review_retry_reaches_adopted_completion_through_session_owner() -> anyhow::Result<()>
{
    let fixture = setup_failed_review().await?;
    let session = &fixture.session;
    assert!(session.autonomous_completion_pending().await);
    assert!(!session.autonomous_completion_is_complete().await);

    let before_invalid_retry = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("registered failure state");
    let invalid_retry = CompletionReviewRetryRequest {
        unit_id: REVIEW_UNIT_ID.into(),
        expected_attempt_id: "unknown-attempt".into(),
        invocation_call_id: "invalid-plan-review-call".into(),
        previous_plan: PREVIOUS_PLAN.into(),
        revised_plan: REVISED_PLAN.into(),
        reason: RETRY_REASON.into(),
    };
    let invalid_retry_call_id = invalid_retry.invocation_call_id.clone();
    assert!(
        session
            .record_autonomous_plan_review(
                ROOT_TASK_ID,
                &invalid_retry_call_id,
                invalid_retry.previous_plan.clone(),
                invalid_retry.revised_plan.clone(),
                invalid_retry.reason.clone(),
                Some(invalid_retry),
            )
            .await
            .is_err()
    );
    assert_eq!(
        session.rencrow_completion_state(ROOT_TASK_ID).await,
        Some(before_invalid_retry.clone()),
        "mismatched plan-review attempt must not mutate or consume retry state"
    );
    assert!(session.autonomous_completion_pending().await);
    assert!(!session.autonomous_completion_is_complete().await);

    let unauthorized_launch = session
        .prepare_completion_review_launch(
            TARGET_TASK_ID,
            Some(REVIEW_UNIT_ID),
            "unauthorized-retry-spawn",
            Some(REVIEWER_ROLE),
            Some(REVIEWER_MODEL),
        )
        .await
        .expect("querying an existing failed attempt returns its inert launch facts");
    assert!(
        !unauthorized_launch.should_spawn,
        "failed attempts need an exact plan-review grant"
    );
    assert_eq!(
        session.rencrow_completion_state(ROOT_TASK_ID).await,
        Some(before_invalid_retry.clone()),
        "an unauthorized lookup does not reserve or mutate the failed attempt"
    );

    let mut invalid_cancellation = before_invalid_retry.clone();
    invalid_cancellation
        .units
        .iter_mut()
        .find(|unit| unit.id == REVIEW_UNIT_ID)
        .expect("failed review obligation exists")
        .status = UnitStatus::Cancelled;
    session
        .set_rencrow_completion_state_with_call_id(
            ROOT_TASK_ID.into(),
            serde_json::to_value(invalid_cancellation)?,
            Some("update-cannot-cancel-failed-review".into()),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    let after_invalid_cancellation = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("host-projected failed review remains registered");
    let mut expected_after_invalid_cancellation = before_invalid_retry.clone();
    if expected_after_invalid_cancellation.plan_review_at.is_none() {
        expected_after_invalid_cancellation.plan_review_at =
            after_invalid_cancellation.plan_review_at;
    }
    assert_eq!(
        after_invalid_cancellation, expected_after_invalid_cancellation,
        "host projection preserves the full failed contract when a model proposes cancellation"
    );
    let failed_after_proposal = after_invalid_cancellation
        .units
        .iter()
        .find(|unit| unit.id == REVIEW_UNIT_ID)
        .expect("failed review obligation remains present");
    assert_eq!(failed_after_proposal.status, UnitStatus::Failed);
    let before_failed_review_state = before_invalid_retry
        .units
        .iter()
        .find(|unit| unit.id == REVIEW_UNIT_ID)
        .and_then(|unit| unit.review_state.as_ref())
        .expect("first failed attempt has canonical review state");
    assert!(
        failed_after_proposal
            .review_state
            .as_ref()
            .is_some_and(|review| {
                review.active_attempt.as_ref().is_some_and(|attempt| {
                    attempt.attempt_id == fixture.first_attempt_id
                        && attempt.status
                            == codex_history::task_context::CompletionReviewAttemptStatus::Failed
                }) && review.historical_failure == before_failed_review_state.historical_failure
                    && review.retry_authorization.is_none()
            })
    );
    assert!(after_invalid_cancellation.units.iter().any(|unit| {
        unit.id == "review-a-legacy-planned" && unit.status == UnitStatus::Planned
    }));

    let mut explicit_update = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("failed and planned obligations remain registered");
    explicit_update
        .units
        .iter_mut()
        .find(|unit| unit.id == "review-a-legacy-planned")
        .expect("planned duplicate obligation exists")
        .status = UnitStatus::Cancelled;
    session
        .set_rencrow_completion_state_with_call_id(
            ROOT_TASK_ID.into(),
            serde_json::to_value(explicit_update)?,
            Some("update-retract-planned-review".into()),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    let withdrawn = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("updated review obligations remain registered");
    assert!(withdrawn.units.iter().any(|unit| {
        unit.id == "review-a-legacy-planned"
            && unit.status == UnitStatus::Cancelled
            && unit
                .review_state
                .as_ref()
                .and_then(|review| review.withdrawal.as_ref())
                .is_some_and(|withdrawal| {
                    matches!(
                        &withdrawal.source,
                        CompletionReviewWithdrawalSource::HostUpdate {
                            update_call_id,
                            prior_status: UnitStatus::Planned,
                        } if update_call_id == "update-retract-planned-review"
                    )
                })
    }));
    assert!(withdrawn.units.iter().any(|unit| {
        unit.id == REVIEW_UNIT_ID
            && unit.status == UnitStatus::Failed
            && unit
                .review_state
                .as_ref()
                .and_then(|review| review.active_attempt.as_ref())
                .is_some_and(|attempt| attempt.attempt_id == fixture.first_attempt_id)
    }));

    let retry_request = CompletionReviewRetryRequest {
        unit_id: REVIEW_UNIT_ID.into(),
        expected_attempt_id: fixture.first_attempt_id.clone(),
        invocation_call_id: "authorized-plan-review-call".into(),
        previous_plan: PREVIOUS_PLAN.into(),
        revised_plan: REVISED_PLAN.into(),
        reason: RETRY_REASON.into(),
    };
    let retry_request_call_id = retry_request.invocation_call_id.clone();
    session
        .record_autonomous_plan_review(
            ROOT_TASK_ID,
            &retry_request_call_id,
            retry_request.previous_plan.clone(),
            retry_request.revised_plan.clone(),
            retry_request.reason.clone(),
            Some(retry_request),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    let granted_state = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("explicit retry grant is persisted on the registered contract");
    let failed_attempt_source_ref = granted_state
        .units
        .iter()
        .find(|unit| unit.id == REVIEW_UNIT_ID)
        .and_then(|unit| unit.review_state.as_ref())
        .and_then(|review| review.previous_attempt_ref.clone())
        .expect("grant-bearing state retains its exact prior source reference");
    let grant_bearing_snapshot_ref = {
        let state = session.state.lock().await;
        let receipt = state
            .rencrow_completion_last_persisted_snapshot
            .as_ref()
            .expect("grant-bearing state has a durable snapshot receipt");
        CompletionReviewSnapshotRef {
            version: receipt.version,
            sha256: receipt.sha256.clone(),
        }
    };
    let retry_launch = session
        .prepare_completion_review_launch(
            TARGET_TASK_ID,
            Some(REVIEW_UNIT_ID),
            "review-spawn-retry-unstarted",
            Some(REVIEWER_ROLE),
            Some(REVIEWER_MODEL),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    anyhow::ensure!(
        retry_launch.should_spawn,
        "authorized retry must reserve exactly one spawn"
    );
    anyhow::ensure!(
        retry_launch.target_artifact_ref == fixture.target_artifact_ref,
        "retry must remain tied to the same accepted task artifact"
    );
    let reserved_state = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("reserved retry state remains registered");
    let reserved_review = reserved_state
        .units
        .iter()
        .find(|unit| unit.id == REVIEW_UNIT_ID)
        .and_then(|unit| unit.review_state.as_ref())
        .expect("reserved retry retains canonical review state");
    assert_eq!(
        reserved_review.previous_attempt_ref.as_ref(),
        Some(&grant_bearing_snapshot_ref),
        "reservation B points to grant-bearing source snapshot A"
    );
    anyhow::ensure!(
        reserved_review.retry_authorization.is_none()
            && reserved_review
                .active_attempt
                .as_ref()
                .is_some_and(|attempt| {
                    attempt.attempt_id == "review-spawn-retry-unstarted"
                        && attempt.status
                            == codex_history::task_context::CompletionReviewAttemptStatus::Reserved
                }),
        "retry grant is consumed only into the persisted new attempt reference"
    );
    let (reserved_snapshot_ref, reserved_uncollected_children, reserved_paused) = {
        let state = session.state.lock().await;
        let receipt = state
            .rencrow_completion_last_persisted_snapshot
            .as_ref()
            .expect("reservation B has a durable receipt");
        (
            CompletionReviewSnapshotRef {
                version: receipt.version,
                sha256: receipt.sha256.clone(),
            },
            state.rencrow_completion_uncollected_children.clone(),
            state.rencrow_completion_paused,
        )
    };
    let original_annotated_history = session.clone_history().await.annotated_items().to_vec();
    let durable_reserved_snapshot =
        latest_durable_completion_snapshot(&fixture.rollout_path).await?;
    assert_eq!(
        crate::session::completion_persistence::maintenance_snapshot_digest(
            &durable_reserved_snapshot,
        )
        .map_err(anyhow::Error::msg)?,
        reserved_snapshot_ref.sha256,
        "source-chain negative starts from the exact durable reservation B snapshot"
    );
    let is_host_completion_envelope = |envelope: &codex_history::ResponseItemEnvelope| {
        let is_host_message = matches!(
            &envelope.item,
            codex_protocol::models::ResponseItem::Message {
                role,
                internal_chat_message_metadata_passthrough: Some(passthrough),
                ..
            } if role == "developer"
                && passthrough.content_item_kinds.as_ref().is_some_and(|kinds| {
                    kinds.len() == 1 && kinds[0].0 == "rencrow.completion_snapshot"
                })
        );
        is_host_message
            && envelope.metadata.as_ref().is_some_and(|metadata| {
                !metadata.client_authored && metadata.rencrow_completion.is_some()
            })
    };
    let snapshot_envelope_matches =
        |envelope: &codex_history::ResponseItemEnvelope,
         reference: &CompletionReviewSnapshotRef| {
            is_host_completion_envelope(envelope)
                && envelope
                    .metadata
                    .as_ref()
                    .and_then(|metadata| metadata.rencrow_completion.as_ref())
                    .and_then(|value| {
                        crate::session::completion_persistence::maintenance_snapshot_digest(value)
                            .ok()
                    })
                    .as_deref()
                    == Some(reference.sha256.as_str())
        };
    let failed_source_envelope_count = original_annotated_history
        .iter()
        .filter(|envelope| snapshot_envelope_matches(envelope, &failed_attempt_source_ref))
        .count();
    assert!(
        failed_source_envelope_count > 0,
        "source F is present as a host completion snapshot in the fixture history"
    );
    assert!(
        original_annotated_history
            .iter()
            .any(|envelope| { snapshot_envelope_matches(envelope, &grant_bearing_snapshot_ref) }),
        "grant-bearing source A is retained alongside source F"
    );
    assert!(
        original_annotated_history
            .iter()
            .any(|envelope| snapshot_envelope_matches(envelope, &reserved_snapshot_ref)),
        "reserved source B is retained alongside A and F"
    );
    let corrupted_history = original_annotated_history
        .iter()
        .filter(|envelope| !snapshot_envelope_matches(envelope, &failed_attempt_source_ref))
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        corrupted_history.len() + failed_source_envelope_count,
        original_annotated_history.len(),
        "the temporary history omits every envelope for source F and no other envelope"
    );
    assert_eq!(
        corrupted_history
            .iter()
            .filter(|envelope| snapshot_envelope_matches(envelope, &failed_attempt_source_ref))
            .count(),
        0,
        "only exact source F is removed from the temporary in-memory history"
    );
    assert!(
        corrupted_history
            .iter()
            .any(|envelope| { snapshot_envelope_matches(envelope, &grant_bearing_snapshot_ref) })
    );
    assert!(
        corrupted_history
            .iter()
            .any(|envelope| snapshot_envelope_matches(envelope, &reserved_snapshot_ref))
    );
    session
        .state
        .lock()
        .await
        .history
        .replace_annotated(corrupted_history);
    let task_checks_before_failed_rollback =
        serde_json::to_value(session.task_check_snapshot().await)?;
    let rollback_error = session
        .rollback_unstarted_completion_review(REVIEW_UNIT_ID)
        .await
        .expect_err("rollback refuses an A source whose exact F reference is missing");
    assert_eq!(
        rollback_error,
        "retry predecessor snapshot does not resolve its own persisted source references"
    );
    assert_eq!(
        session.rencrow_completion_state(ROOT_TASK_ID).await,
        Some(reserved_state.clone()),
        "failed source-chain validation leaves reserved B and its consumed grant unchanged"
    );
    let task_checks_after_failed_rollback =
        serde_json::to_value(session.task_check_snapshot().await)?;
    assert_eq!(
        task_checks_after_failed_rollback, task_checks_before_failed_rollback,
        "failed source-chain validation leaves the task-check ledger unchanged"
    );
    let durable_after_failed_rollback =
        latest_durable_completion_snapshot(&fixture.rollout_path).await?;
    assert_eq!(
        crate::session::completion_persistence::maintenance_snapshot_digest(
            &durable_after_failed_rollback,
        )
        .map_err(anyhow::Error::msg)?,
        reserved_snapshot_ref.sha256,
        "failed source-chain rollback does not rewrite the original durable rollout"
    );
    {
        let state = session.state.lock().await;
        let receipt = state
            .rencrow_completion_last_persisted_snapshot
            .as_ref()
            .expect("failed rollback retains reservation B receipt");
        assert_eq!(receipt.version, reserved_snapshot_ref.version);
        assert_eq!(receipt.sha256, reserved_snapshot_ref.sha256);
        assert_eq!(
            state.rencrow_completion_uncollected_children,
            reserved_uncollected_children
        );
        assert_eq!(state.rencrow_completion_paused, reserved_paused);
    }
    session
        .state
        .lock()
        .await
        .history
        .replace_annotated(original_annotated_history);
    session
        .rollback_unstarted_completion_review(REVIEW_UNIT_ID)
        .await
        .map_err(anyhow::Error::msg)?;
    assert_eq!(
        session.rencrow_completion_state(ROOT_TASK_ID).await,
        Some(granted_state.clone()),
        "a proven pre-spawn rejection restores the exact failed attempt and one-shot grant"
    );
    cold_restore_completion_history(session, &fixture.rollout_path).await?;
    let restored_granted_state = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("cold restore reloads the grant-bearing failure state");
    assert_eq!(
        restored_granted_state,
        granted_state.clone(),
        "cold restore retains the exact pre-reservation contract after rollback"
    );
    let restored_grant = restored_granted_state
        .units
        .iter()
        .find(|unit| unit.id == REVIEW_UNIT_ID)
        .and_then(|unit| unit.review_state.as_ref())
        .and_then(|review| review.retry_authorization.as_ref())
        .expect("rollback retry authorization remains one-use and persisted");
    assert_eq!(restored_grant.expected_attempt_id, fixture.first_attempt_id);
    assert_eq!(
        restored_grant.plan_review_call_id,
        "authorized-plan-review-call"
    );
    let retry_launch = session
        .prepare_completion_review_launch(
            TARGET_TASK_ID,
            Some(REVIEW_UNIT_ID),
            "review-spawn-retry",
            Some(REVIEWER_ROLE),
            Some(REVIEWER_MODEL),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    anyhow::ensure!(
        retry_launch.should_spawn,
        "restored retry grant permits one new reservation"
    );

    let retry_child_id =
        ThreadId::try_from("01a1c606-48b8-7cf3-bb78-77b47f6b8c30").expect("valid retry child ID");
    let retry_turn_id = "review-turn-retry";
    spawn_and_bind_attempt(
        session,
        REVIEW_UNIT_ID,
        &fixture.target_artifact_ref,
        "review-spawn-retry",
        retry_child_id,
        retry_turn_id,
    )
    .await?;
    session
        .collect_completion_review_results(
            "wait-retry-completed",
            &[(
                retry_child_id,
                Some(retry_turn_id.to_string()),
                AgentStatus::Completed(Some("The accepted artifact meets AC-1".into())),
            )],
        )
        .await;
    observe_native_wait_result(
        session,
        retry_child_id,
        AgentStatus::Completed(Some("The accepted artifact meets AC-1".into())),
    )
    .await;
    assert!(session.autonomous_completion_pending().await);
    assert!(!session.autonomous_completion_is_complete().await);

    session
        .adopt_completion_review(
            REVIEW_UNIT_ID,
            "The authorized reviewer confirmed the accepted task artifact meets AC-1".into(),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    assert!(!session.autonomous_completion_pending().await);
    assert!(session.autonomous_completion_is_complete().await);
    let adopted = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("completed contract remains registered");
    assert!(
        adopted
            .conditions
            .iter()
            .all(|condition| condition.status == AcceptanceStatus::Satisfied)
    );
    assert!(
        adopted
            .units
            .iter()
            .any(|unit| unit.id == REVIEW_UNIT_ID && unit.status == UnitStatus::Done)
    );

    let workspace = fixture.task_workspace.path();
    let artifact_paths = vec![std::path::PathBuf::from("review-target.txt")];
    let target_bytes_before_refresh =
        crate::task_check::hash_workspace_artifacts(workspace, &artifact_paths)
            .expect("accepted review target bytes are available before receipt refresh");
    let (refreshed_artifact_ref, _) = install_accepted_task_receipt_with_content(
        session,
        "accepted-task-check-refresh",
        "accepted artifact\n",
        fixture.task_workspace.path(),
    )
    .await?;
    let target_bytes_after_refresh =
        crate::task_check::hash_workspace_artifacts(workspace, &artifact_paths)
            .expect("accepted review target bytes remain available after receipt refresh");
    assert_eq!(target_bytes_before_refresh, target_bytes_after_refresh);
    assert_ne!(fixture.target_artifact_ref, refreshed_artifact_ref);
    let latest_task_state = session
        .latest_rencrow_task_state()
        .await?
        .expect("refreshed accepted task state remains available");
    assert_eq!(latest_task_state.completed[TARGET_TASK_ID].revision, 1);
    assert_eq!(
        latest_task_state.completed[TARGET_TASK_ID].artifact_ref,
        refreshed_artifact_ref
    );

    let mut revised = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("adopted review remains available for the updated plan");
    revised.contract_revision += 1;
    let refreshed_evidence = CompletionEvidenceRef {
        task_id: TARGET_TASK_ID.into(),
        task_revision: 1,
        artifact_ref: refreshed_artifact_ref.clone(),
        verification_ref: refreshed_artifact_ref.clone(),
    };
    revised.conditions[0].evidence = vec![refreshed_evidence.clone()];
    revised
        .units
        .iter_mut()
        .find(|unit| unit.id == TARGET_TASK_ID)
        .expect("accepted execution unit remains in the updated plan")
        .task_refs = vec![refreshed_evidence];
    revised.units.push(CompletionUnit {
        id: "review-a-current".into(),
        purpose: "Review the refreshed receipt for task A".into(),
        status: UnitStatus::Planned,
        kind: CompletionUnitKind::Review,
        condition_refs: vec!["AC-1".into()],
        depends_on: vec![TARGET_TASK_ID.into()],
        evidence: Vec::new(),
        task_refs: Vec::new(),
        review_target: Some(CompletionReviewTarget {
            task_id: TARGET_TASK_ID.into(),
            task_revision: 1,
        }),
        review_evidence: None,
        review_launch: None,
        review_decision_reason: None,
        review_state: None,
        review_revalidation_of: None,
    });
    revised.current_unit = Some("review-a-current".into());
    revised.next_action = Some("Review the refreshed accepted task receipt".into());
    session
        .set_rencrow_completion_state_with_call_id(
            ROOT_TASK_ID.into(),
            serde_json::to_value(revised)?,
            Some("update-plan-after-receipt-refresh".into()),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    assert!(session.autonomous_completion_pending().await);
    assert!(!session.autonomous_completion_is_complete().await);

    let adopted_selection = session
        .select_completion_review(TARGET_TASK_ID, Some(REVIEW_UNIT_ID))
        .await
        .map_err(anyhow::Error::msg)?;
    assert_eq!(
        adopted_selection.outcome,
        crate::session::CompletionReviewSelectionOutcome::AlreadyAdopted
    );
    let historical_lookup = session
        .prepare_completion_review_launch(
            TARGET_TASK_ID,
            Some(REVIEW_UNIT_ID),
            "lookup-old-done",
            /*reviewer_role*/ None,
            /*reviewer_model*/ None,
        )
        .await
        .map_err(anyhow::Error::msg)?;
    assert!(!historical_lookup.should_spawn);
    assert_eq!(
        historical_lookup.target_artifact_ref,
        fixture.target_artifact_ref
    );
    let current_selection = session
        .select_completion_review(TARGET_TASK_ID, Some("review-a-current"))
        .await
        .map_err(anyhow::Error::msg)?;
    assert!(matches!(
        current_selection.outcome,
        crate::session::CompletionReviewSelectionOutcome::NeedsReservation {
            expected_attempt_id: None,
            ..
        }
    ));

    let durable = latest_durable_completion_snapshot(&fixture.rollout_path).await?;
    let persisted_unit = durable_unit(&durable, REVIEW_UNIT_ID);
    assert_eq!(persisted_unit["status"], "done");
    assert_eq!(
        persisted_unit["review_state"]["active_attempt"]["attempt_id"],
        "review-spawn-retry"
    );
    assert_eq!(
        persisted_unit["review_state"]["active_attempt"]["reviewer_thread_id"],
        retry_child_id.to_string()
    );
    assert_ne!(fixture.first_child_id, retry_child_id);
    assert_ne!(fixture.first_turn_id, retry_turn_id);

    cold_restore_completion_history(session, &fixture.rollout_path).await?;
    let restored_done = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("updated plan restores the historical Done review");
    assert!(restored_done.units.iter().any(|unit| {
        unit.id == REVIEW_UNIT_ID
            && unit.status == UnitStatus::Done
            && unit
                .review_evidence
                .as_ref()
                .is_some_and(|evidence| evidence.target_artifact_ref == fixture.target_artifact_ref)
    }));
    assert!(
        restored_done
            .units
            .iter()
            .any(|unit| { unit.id == "review-a-current" && unit.status == UnitStatus::Planned })
    );
    let restored_adopted = session
        .select_completion_review(TARGET_TASK_ID, Some(REVIEW_UNIT_ID))
        .await
        .map_err(anyhow::Error::msg)?;
    assert_eq!(
        restored_adopted.outcome,
        crate::session::CompletionReviewSelectionOutcome::AlreadyAdopted
    );
    assert!(session.autonomous_completion_pending().await);
    assert!(!session.autonomous_completion_is_complete().await);
    Ok(())
}

#[tokio::test]
async fn changed_artifact_requires_explicit_review_revalidation_before_completion()
-> anyhow::Result<()> {
    let fixture = setup_failed_review().await?;
    let session = &fixture.session;
    let retry = CompletionReviewRetryRequest {
        unit_id: REVIEW_UNIT_ID.into(),
        expected_attempt_id: fixture.first_attempt_id.clone(),
        invocation_call_id: "revalidation-success-plan-review".into(),
        previous_plan: PREVIOUS_PLAN.into(),
        revised_plan: REVISED_PLAN.into(),
        reason: RETRY_REASON.into(),
    };
    let retry_call_id = retry.invocation_call_id.clone();
    session
        .record_autonomous_plan_review(
            ROOT_TASK_ID,
            &retry_call_id,
            retry.previous_plan.clone(),
            retry.revised_plan.clone(),
            retry.reason.clone(),
            Some(retry),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    let launch = session
        .prepare_completion_review_launch(
            TARGET_TASK_ID,
            Some(REVIEW_UNIT_ID),
            "revalidation-success-spawn",
            Some(REVIEWER_ROLE),
            Some(REVIEWER_MODEL),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    anyhow::ensure!(launch.should_spawn);
    let child_id =
        ThreadId::try_from("01a1c606-48b8-7cf3-bb78-77b47f6b8c32").expect("valid review child ID");
    let turn_id = "revalidation-success-turn";
    spawn_and_bind_attempt(
        session,
        REVIEW_UNIT_ID,
        &fixture.target_artifact_ref,
        "revalidation-success-spawn",
        child_id,
        turn_id,
    )
    .await?;
    session
        .collect_completion_review_results(
            "revalidation-success-wait",
            &[(
                child_id,
                Some(turn_id.to_string()),
                AgentStatus::Completed(Some("The accepted artifact meets AC-1".into())),
            )],
        )
        .await;
    observe_native_wait_result(
        session,
        child_id,
        AgentStatus::Completed(Some("The accepted artifact meets AC-1".into())),
    )
    .await;
    session
        .adopt_completion_review(
            REVIEW_UNIT_ID,
            "The reviewer confirmed the accepted task artifact meets AC-1".into(),
        )
        .await
        .map_err(anyhow::Error::msg)?;

    let mut close_initial_plan = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("adopted review contract is available");
    close_initial_plan
        .units
        .iter_mut()
        .find(|unit| unit.id == "review-a-legacy-planned")
        .expect("independent planned review fixture exists")
        .status = UnitStatus::Cancelled;
    session
        .set_rencrow_completion_state_with_call_id(
            ROOT_TASK_ID.into(),
            serde_json::to_value(close_initial_plan)?,
            Some("revalidation-close-unused-plan".into()),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    anyhow::ensure!(session.autonomous_completion_is_complete().await);

    let workspace = fixture.task_workspace.path();
    std::fs::write(
        workspace.join("review-target.txt"),
        "changed artifact bytes\n",
    )?;
    assert!(
        session
            .reconcile_rencrow_completion_evidence(ROOT_TASK_ID)
            .await
            .map_err(anyhow::Error::msg)?
    );
    let invalidated = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("changed artifact invalidates the prior satisfied condition");
    assert_eq!(invalidated.conditions[0].status, AcceptanceStatus::Open);
    assert!(session.autonomous_completion_pending().await);
    assert!(!session.autonomous_completion_is_complete().await);

    let (mut current_artifact_ref, _) = install_accepted_task_receipt_with_content(
        session,
        "accepted-task-check-changed-bytes",
        "changed artifact bytes\n",
        fixture.task_workspace.path(),
    )
    .await?;
    assert!(session.autonomous_completion_pending().await);
    assert!(!session.autonomous_completion_is_complete().await);

    let mut reaccepted = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("reaccepted contract remains registered");
    let current_evidence = CompletionEvidenceRef {
        task_id: TARGET_TASK_ID.into(),
        task_revision: 1,
        artifact_ref: current_artifact_ref.clone(),
        verification_ref: current_artifact_ref.clone(),
    };
    reaccepted.conditions[0].status = AcceptanceStatus::Satisfied;
    reaccepted.conditions[0].evidence = vec![current_evidence.clone()];
    reaccepted
        .units
        .iter_mut()
        .find(|unit| unit.id == TARGET_TASK_ID)
        .expect("accepted execution unit remains in the plan")
        .task_refs = vec![current_evidence.clone()];
    session
        .set_rencrow_completion_state_with_call_id(
            ROOT_TASK_ID.into(),
            serde_json::to_value(reaccepted.clone())?,
            Some("update-current-accepted-receipt".into()),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    assert!(session.autonomous_completion_pending().await);
    assert!(!session.autonomous_completion_is_complete().await);
    assert!(
        reaccepted
            .units
            .iter()
            .any(|unit| { unit.id == REVIEW_UNIT_ID && unit.status == UnitStatus::Done })
    );

    reaccepted.contract_revision += 1;
    reaccepted.units.push(planned_revalidation_unit(
        "review-a-revalidate-b",
        REVIEW_UNIT_ID,
    ));
    session
        .set_rencrow_completion_state_with_call_id(
            ROOT_TASK_ID.into(),
            serde_json::to_value(reaccepted)?,
            Some("update-review-revalidation-b".into()),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    let bound_b = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("host-bound first revalidation plan remains registered");
    let bound_b_review = bound_b
        .units
        .iter()
        .find(|unit| unit.id == "review-a-revalidate-b")
        .and_then(|unit| unit.review_state.as_ref())
        .expect("revalidation B has host-owned review state");
    assert!(
        bound_b_review.previous_attempt_ref.is_none(),
        "new revalidation stores its source snapshot only in the revalidation edge"
    );
    let source_a_snapshot_ref = bound_b_review
        .revalidation
        .as_ref()
        .expect("revalidation B retains its exact source A")
        .source_snapshot
        .clone();
    let required_fingerprint =
        codex_history::task_context::task_check_artifact_fingerprint(&current_artifact_ref)
            .expect("accepted receipt contains an artifact fingerprint")
            .to_string();
    assert!(bound_b.units.iter().any(|unit| {
        unit.id == "review-a-revalidate-b"
            && unit
                .review_state
                .as_ref()
                .and_then(|state| state.revalidation.as_ref())
                .is_some_and(|source| {
                    source.source_unit_id == REVIEW_UNIT_ID
                        && source.update_call_id == "update-review-revalidation-b"
                        && source.required_artifact_fingerprint == required_fingerprint
                        && source.source_adoption_sha256.len() == 64
                        && source.source_condition_definitions_sha256.len() == 64
                        && source.required_condition_definitions_sha256.len() == 64
                        && source.source_snapshot.sha256.len() == 64
                        && matches!(source.source_snapshot.version, 1 | 2)
                })
    }));
    assert!(session.autonomous_completion_pending().await);
    assert!(!session.autonomous_completion_is_complete().await);

    let mut unauthorized_replacement = bound_b.clone();
    unauthorized_replacement.contract_revision += 1;
    unauthorized_replacement
        .units
        .push(planned_revalidation_unit(
            "review-a-revalidate-c",
            REVIEW_UNIT_ID,
        ));
    assert!(
        session
            .set_rencrow_completion_state_with_call_id(
                ROOT_TASK_ID.into(),
                serde_json::to_value(unauthorized_replacement)?,
                Some("update-replace-active-revalidation".into()),
            )
            .await
            .is_err()
    );
    assert_eq!(
        session.rencrow_completion_state(ROOT_TASK_ID).await,
        Some(bound_b.clone()),
        "a new leaf cannot silently replace a still-planned linked review"
    );

    let mut cancel_b = bound_b;
    cancel_b
        .units
        .iter_mut()
        .find(|unit| unit.id == "review-a-revalidate-b")
        .expect("first linked revalidation exists")
        .status = UnitStatus::Cancelled;
    session
        .set_rencrow_completion_state_with_call_id(
            ROOT_TASK_ID.into(),
            serde_json::to_value(cancel_b)?,
            Some("update-cancel-revalidation-b".into()),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    assert!(session.autonomous_completion_pending().await);
    assert!(!session.autonomous_completion_is_complete().await);

    // Alpha2 bytes remain stale even after withdrawing B; cancellation alone must not
    // complete the root obligation.
    let after_b_cancellation = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("cancelled B remains in the contract");
    let cancelled_b_review = after_b_cancellation
        .units
        .iter()
        .find(|unit| unit.id == "review-a-revalidate-b")
        .expect("B obligation remains historical");
    assert_eq!(cancelled_b_review.status, UnitStatus::Cancelled);
    let cancelled_b_review_state = cancelled_b_review
        .review_state
        .as_ref()
        .expect("cancelled B retains its host review facts");
    assert_eq!(
        cancelled_b_review_state
            .revalidation
            .as_ref()
            .expect("cancelled B retains its revalidation edge")
            .source_snapshot,
        source_a_snapshot_ref,
        "HostUpdate withdrawal preserves the exact source A edge"
    );
    assert!(matches!(
        cancelled_b_review_state
            .withdrawal
            .as_ref()
            .map(|withdrawal| &withdrawal.source),
        Some(CompletionReviewWithdrawalSource::HostUpdate { update_call_id, .. })
            if update_call_id == "update-cancel-revalidation-b"
    ));
    assert!(session.autonomous_completion_pending().await);
    assert!(!session.autonomous_completion_is_complete().await);

    // Old persisted snapshots duplicated the source snapshot into previous_attempt_ref.
    // Record that historical shape using the same host snapshot and its real source history.
    let source_snapshots_before_alias = durable_completion_snapshots(&fixture.rollout_path).await?;
    assert!(
        source_snapshots_before_alias.iter().any(|snapshot| {
            snapshot["version"] == source_a_snapshot_ref.version
                && crate::session::completion_persistence::maintenance_snapshot_digest(snapshot)
                    .is_ok_and(|digest| digest == source_a_snapshot_ref.sha256)
        }),
        "the revalidation source A is present in the real persisted history"
    );
    let mut legacy_alias_snapshot =
        latest_durable_completion_snapshot(&fixture.rollout_path).await?;
    let legacy_b = legacy_alias_snapshot["state"]["units"]
        .as_array_mut()
        .and_then(|units| {
            units
                .iter_mut()
                .find(|unit| unit["id"] == "review-a-revalidate-b")
        })
        .expect("persisted host update contains cancelled B");
    assert_eq!(legacy_b["status"], "cancelled");
    assert_eq!(
        legacy_b["review_state"]["revalidation"]["source_snapshot"],
        serde_json::to_value(&source_a_snapshot_ref)?,
        "old-format alias is copied from the exact host source reference"
    );
    assert!(legacy_b["review_state"]["previous_attempt_ref"].is_null());
    legacy_b["review_state"]["previous_attempt_ref"] =
        serde_json::to_value(&source_a_snapshot_ref)?;
    let legacy_alias_digest =
        crate::session::completion_persistence::maintenance_snapshot_digest(&legacy_alias_snapshot)
            .map_err(anyhow::Error::msg)?;
    let legacy_alias_item = completion_snapshot_item(legacy_alias_snapshot);
    anyhow::ensure!(
        session
            .persist_rollout_items(std::slice::from_ref(&legacy_alias_item))
            .await,
        "old-format source alias must append through the attached rollout writer"
    );
    session.flush_rollout().await?;
    let RolloutItem::ResponseItem(legacy_alias_envelope) = legacy_alias_item else {
        anyhow::bail!("old-format alias fixture is a host response envelope");
    };
    session.state.lock().await.history.record_annotated_items(
        std::slice::from_ref(&legacy_alias_envelope),
        codex_utils_output_truncation::TruncationPolicy::Tokens(/*tokens*/ 1024),
    );
    let persisted_legacy_alias = latest_durable_completion_snapshot(&fixture.rollout_path).await?;
    assert_eq!(
        crate::session::completion_persistence::maintenance_snapshot_digest(
            &persisted_legacy_alias,
        )
        .map_err(anyhow::Error::msg)?,
        legacy_alias_digest,
        "legacy alias snapshot is the exact value appended and reloaded from the real writer"
    );
    assert_eq!(
        durable_unit(&persisted_legacy_alias, "review-a-revalidate-b")["review_state"]["previous_attempt_ref"],
        serde_json::to_value(&source_a_snapshot_ref)?,
        "reloaded old-format snapshot retains the duplicated exact source alias"
    );
    cold_restore_completion_history(session, &fixture.rollout_path).await?;
    let restored_alias_state = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("cold restore accepts the persisted old-format source alias");
    let restored_alias_b = restored_alias_state
        .units
        .iter()
        .find(|unit| unit.id == "review-a-revalidate-b")
        .expect("restored cancelled B remains in the contract");
    assert_eq!(restored_alias_b.status, UnitStatus::Cancelled);
    let restored_alias_review = restored_alias_b
        .review_state
        .as_ref()
        .expect("restored B retains canonical review facts");
    assert_eq!(
        restored_alias_review.previous_attempt_ref.as_ref(),
        Some(&source_a_snapshot_ref),
        "cold restore preserves the historical duplicate source alias"
    );
    assert!(matches!(
        restored_alias_review
            .withdrawal
            .as_ref()
            .map(|withdrawal| &withdrawal.source),
        Some(CompletionReviewWithdrawalSource::HostUpdate { update_call_id, .. })
            if update_call_id == "update-cancel-revalidation-b"
    ));
    assert!(session.autonomous_completion_pending().await);
    assert!(!session.autonomous_completion_is_complete().await);

    let baseline_alpha_fingerprint =
        codex_history::task_context::task_check_artifact_fingerprint(&fixture.target_artifact_ref)
            .expect("source Alpha review has its exact artifact fingerprint")
            .to_string();
    let (baseline_alpha_receipt, _) = install_accepted_task_receipt_with_content(
        session,
        "accepted-task-check-return-to-alpha1",
        "accepted artifact\n",
        fixture.task_workspace.path(),
    )
    .await?;
    assert_eq!(
        codex_history::task_context::task_check_artifact_fingerprint(&baseline_alpha_receipt),
        Some(baseline_alpha_fingerprint.as_str()),
        "a new receipt call for the original bytes has the original artifact fingerprint"
    );
    assert_ne!(baseline_alpha_receipt, fixture.target_artifact_ref);
    let baseline_alpha_evidence = CompletionEvidenceRef {
        task_id: TARGET_TASK_ID.into(),
        task_revision: 1,
        artifact_ref: baseline_alpha_receipt.clone(),
        verification_ref: baseline_alpha_receipt.clone(),
    };
    let mut returned_to_alpha1 = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("Alpha plan remains registered for same-target reacceptance");
    returned_to_alpha1.contract_revision += 1;
    returned_to_alpha1.conditions[0].status = AcceptanceStatus::Satisfied;
    returned_to_alpha1.conditions[0].evidence = vec![baseline_alpha_evidence.clone()];
    returned_to_alpha1.conditions[0].unmet_reason = None;
    let alpha_execution = returned_to_alpha1
        .units
        .iter_mut()
        .find(|unit| unit.id == TARGET_TASK_ID)
        .expect("Alpha execution unit remains present");
    alpha_execution
        .evidence
        .push(baseline_alpha_evidence.clone());
    alpha_execution.task_refs = vec![baseline_alpha_evidence.clone()];
    session
        .set_rencrow_completion_state_with_call_id(
            ROOT_TASK_ID.into(),
            serde_json::to_value(returned_to_alpha1)?,
            Some("host-reaccept-alpha1-after-beta-cancel".into()),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    let completion = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("same-fingerprint Alpha receipt keeps the completion plan registered");
    let latest_task_state = session.latest_rencrow_task_state().await?;
    let current_review_scope_reason = session
        .autonomous_completion_current_review_scope_reason()
        .await;
    let latest_alpha_receipt = latest_task_state
        .as_ref()
        .and_then(|state| state.completed.get(TARGET_TASK_ID))
        .expect("accepted Alpha1 task receipt remains the current target");
    assert_eq!(latest_alpha_receipt.revision, 1);
    assert_eq!(latest_alpha_receipt.artifact_ref, baseline_alpha_receipt);
    assert_eq!(completion.conditions[0].status, AcceptanceStatus::Satisfied);
    assert_eq!(
        completion.conditions[0].evidence,
        vec![baseline_alpha_evidence]
    );
    assert!(current_review_scope_reason.is_none());
    assert!(
        !session.autonomous_completion_pending().await,
        "same-fingerprint Alpha reaccept should restore the adopted review scope"
    );
    assert!(session.autonomous_completion_is_complete().await);
    let restored_alpha1_state = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("same-fingerprint Alpha receipt is persisted");
    assert!(restored_alpha1_state.units.iter().any(|unit| {
        unit.id == REVIEW_UNIT_ID
            && unit.status == UnitStatus::Done
            && unit.condition_refs == vec!["AC-1".to_string()]
            && unit.review_evidence.as_ref().is_some_and(|evidence| {
                evidence.target_task_id == TARGET_TASK_ID && evidence.target_task_revision == 1
            })
    }));
    assert!(restored_alpha1_state.units.iter().any(|unit| {
        unit.id == "review-a-revalidate-b" && unit.status == UnitStatus::Cancelled
    }));

    let workspace = fixture.task_workspace.path();
    std::fs::write(
        workspace.join("review-target.txt"),
        "changed artifact bytes\n",
    )?;
    assert!(
        session
            .reconcile_rencrow_completion_evidence(ROOT_TASK_ID)
            .await
            .map_err(anyhow::Error::msg)?
    );
    assert!(session.autonomous_completion_pending().await);
    assert!(!session.autonomous_completion_is_complete().await);
    current_artifact_ref = install_accepted_task_receipt_with_content(
        session,
        "accepted-task-check-return-to-alpha2",
        "changed artifact bytes\n",
        fixture.task_workspace.path(),
    )
    .await?
    .0;
    assert_eq!(
        codex_history::task_context::task_check_artifact_fingerprint(&current_artifact_ref),
        Some(required_fingerprint.as_str()),
        "the new Alpha2 task-check call retains the exact accepted fingerprint"
    );
    let current_alpha2_evidence = CompletionEvidenceRef {
        task_id: TARGET_TASK_ID.into(),
        task_revision: 1,
        artifact_ref: current_artifact_ref.clone(),
        verification_ref: current_artifact_ref.clone(),
    };
    let mut returned_to_alpha2 = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("completion plan remains registered for Alpha2 reacceptance");
    returned_to_alpha2.contract_revision += 1;
    returned_to_alpha2.conditions[0].status = AcceptanceStatus::Satisfied;
    returned_to_alpha2.conditions[0].evidence = vec![current_alpha2_evidence.clone()];
    returned_to_alpha2.conditions[0].unmet_reason = None;
    let alpha_execution = returned_to_alpha2
        .units
        .iter_mut()
        .find(|unit| unit.id == TARGET_TASK_ID)
        .expect("Alpha execution unit remains present");
    alpha_execution
        .evidence
        .push(current_alpha2_evidence.clone());
    alpha_execution.task_refs = vec![current_alpha2_evidence];
    session
        .set_rencrow_completion_state_with_call_id(
            ROOT_TASK_ID.into(),
            serde_json::to_value(returned_to_alpha2)?,
            Some("host-reaccept-alpha2-before-review-c".into()),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    assert!(session.autonomous_completion_pending().await);
    assert!(!session.autonomous_completion_is_complete().await);

    let mut add_c = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("cancelled predecessor remains in completion history");
    add_c.contract_revision += 1;
    add_c.units.push(planned_revalidation_unit(
        "review-a-revalidate-c",
        REVIEW_UNIT_ID,
    ));
    add_c
        .units
        .push(planned_independent_review_unit("review-a-independent"));
    session
        .set_rencrow_completion_state_with_call_id(
            ROOT_TASK_ID.into(),
            serde_json::to_value(add_c)?,
            Some("update-explicit-review-revalidation".into()),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    let bound_c = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("replacement leaf has host-bound provenance");
    assert!(bound_c.units.iter().any(|unit| {
        unit.id == "review-a-revalidate-c"
            && unit
                .review_state
                .as_ref()
                .and_then(|state| state.revalidation.as_ref())
                .is_some_and(|source| {
                    source.source_unit_id == REVIEW_UNIT_ID
                        && source.update_call_id == "update-explicit-review-revalidation"
                        && source.required_artifact_fingerprint == required_fingerprint
                        && source.source_condition_definitions_sha256.len() == 64
                        && source.required_condition_definitions_sha256.len() == 64
                })
    }));
    assert!(session.autonomous_completion_pending().await);
    assert!(!session.autonomous_completion_is_complete().await);

    let revalidation_launch = session
        .prepare_completion_review_launch(
            TARGET_TASK_ID,
            Some("review-a-revalidate-c"),
            "review-a-revalidate-c-spawn",
            Some(REVIEWER_ROLE),
            Some(REVIEWER_MODEL),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    anyhow::ensure!(revalidation_launch.should_spawn);
    let revalidation_child = ThreadId::try_from("01a1c606-48b8-7cf3-bb78-77b47f6b8c33")
        .expect("valid revalidation child ID");
    let revalidation_turn = "review-a-revalidate-turn";
    spawn_and_bind_attempt(
        session,
        "review-a-revalidate-c",
        &current_artifact_ref,
        "review-a-revalidate-c-spawn",
        revalidation_child,
        revalidation_turn,
    )
    .await?;
    session
        .collect_completion_review_results(
            "review-a-revalidate-c-wait",
            &[(
                revalidation_child,
                Some(revalidation_turn.to_string()),
                AgentStatus::Completed(Some("The changed accepted artifact meets AC-1".into())),
            )],
        )
        .await;
    observe_native_wait_result(
        session,
        revalidation_child,
        AgentStatus::Completed(Some("The changed accepted artifact meets AC-1".into())),
    )
    .await;
    session
        .adopt_completion_review(
            "review-a-revalidate-c",
            "The changed accepted artifact still meets AC-1".into(),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    assert!(session.autonomous_completion_pending().await);
    assert!(!session.autonomous_completion_is_complete().await);

    let mut cancel_independent = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("independent planned review remains outstanding after C adoption");
    cancel_independent
        .units
        .iter_mut()
        .find(|unit| unit.id == "review-a-independent")
        .expect("independent review remains present")
        .status = UnitStatus::Cancelled;
    session
        .set_rencrow_completion_state_with_call_id(
            ROOT_TASK_ID.into(),
            serde_json::to_value(cancel_independent)?,
            Some("update-cancel-independent-review".into()),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    assert!(!session.autonomous_completion_pending().await);
    assert!(session.autonomous_completion_is_complete().await);

    let workspace = fixture.task_workspace.path();
    std::fs::write(
        workspace.join("review-target.txt"),
        "rechanged artifact bytes\n",
    )?;
    assert!(
        session
            .reconcile_rencrow_completion_evidence(ROOT_TASK_ID)
            .await
            .map_err(anyhow::Error::msg)?
    );
    let invalidated_again = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("second artifact change invalidates the current review scope");
    assert_eq!(
        invalidated_again.conditions[0].status,
        AcceptanceStatus::Open
    );
    assert!(session.autonomous_completion_pending().await);
    assert!(!session.autonomous_completion_is_complete().await);

    let (second_artifact_ref, _) = install_accepted_task_receipt_with_content(
        session,
        "accepted-task-check-rechanged-bytes",
        "rechanged artifact bytes\n",
        fixture.task_workspace.path(),
    )
    .await?;
    let mut second_reaccept = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("second accepted receipt keeps the completion plan");
    let second_evidence = CompletionEvidenceRef {
        task_id: TARGET_TASK_ID.into(),
        task_revision: 1,
        artifact_ref: second_artifact_ref.clone(),
        verification_ref: second_artifact_ref.clone(),
    };
    second_reaccept.conditions[0].status = AcceptanceStatus::Satisfied;
    second_reaccept.conditions[0].evidence = vec![second_evidence.clone()];
    second_reaccept
        .units
        .iter_mut()
        .find(|unit| unit.id == TARGET_TASK_ID)
        .expect("execution unit remains current")
        .task_refs = vec![second_evidence];
    session
        .set_rencrow_completion_state_with_call_id(
            ROOT_TASK_ID.into(),
            serde_json::to_value(second_reaccept.clone())?,
            Some("update-second-current-receipt".into()),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    assert!(session.autonomous_completion_pending().await);
    assert!(!session.autonomous_completion_is_complete().await);

    second_reaccept.contract_revision += 1;
    second_reaccept.units.push(planned_revalidation_unit(
        "review-a-revalidate-e",
        "review-a-revalidate-c",
    ));
    session
        .set_rencrow_completion_state_with_call_id(
            ROOT_TASK_ID.into(),
            serde_json::to_value(second_reaccept)?,
            Some("update-chain-review-revalidation-e".into()),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    let chain_state = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("A to C to E revalidation chain is registered");
    assert!(chain_state.units.iter().any(|unit| {
        unit.id == "review-a-revalidate-e"
            && unit
                .review_state
                .as_ref()
                .and_then(|review| review.revalidation.as_ref())
                .is_some_and(|source| {
                    source.source_unit_id == "review-a-revalidate-c"
                        && source.update_call_id == "update-chain-review-revalidation-e"
                        && source.required_artifact_fingerprint
                            == codex_history::task_context::task_check_artifact_fingerprint(
                                &second_artifact_ref,
                            )
                            .unwrap_or_default()
                })
    }));
    assert!(session.autonomous_completion_pending().await);
    assert!(!session.autonomous_completion_is_complete().await);

    let chain_launch = session
        .prepare_completion_review_launch(
            TARGET_TASK_ID,
            Some("review-a-revalidate-e"),
            "review-a-revalidate-e-spawn",
            Some(REVIEWER_ROLE),
            Some(REVIEWER_MODEL),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    anyhow::ensure!(chain_launch.should_spawn);
    let chain_child = ThreadId::try_from("01a1c606-48b8-7cf3-bb78-77b47f6b8c35")
        .expect("valid chained revalidation child ID");
    let chain_turn = "review-a-revalidate-e-turn";
    spawn_and_bind_attempt(
        session,
        "review-a-revalidate-e",
        &second_artifact_ref,
        "review-a-revalidate-e-spawn",
        chain_child,
        chain_turn,
    )
    .await?;
    session
        .collect_completion_review_results(
            "review-a-revalidate-e-wait",
            &[(
                chain_child,
                Some(chain_turn.to_string()),
                AgentStatus::Completed(Some("The rechanged accepted artifact meets AC-1".into())),
            )],
        )
        .await;
    observe_native_wait_result(
        session,
        chain_child,
        AgentStatus::Completed(Some("The rechanged accepted artifact meets AC-1".into())),
    )
    .await;
    session
        .adopt_completion_review(
            "review-a-revalidate-e",
            "The rechanged accepted artifact still meets AC-1".into(),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    assert!(!session.autonomous_completion_pending().await);
    assert!(session.autonomous_completion_is_complete().await);

    cold_restore_completion_history(session, &fixture.rollout_path).await?;
    assert!(!session.autonomous_completion_pending().await);
    assert!(session.autonomous_completion_is_complete().await);
    Ok(())
}

#[tokio::test]
async fn adopted_alpha_can_be_revalidated_against_a_newly_accepted_beta_task() -> anyhow::Result<()>
{
    const BETA_TASK_ID: &str = "task-b";
    const BETA_REVIEW_UNIT_ID: &str = "review-b-revalidate-alpha";

    let (mut session, _) = make_session_and_context().await;
    let rollout_path = attach_thread_persistence(&mut session).await;
    let (alpha_artifact_ref, alpha_workspace, _) =
        install_accepted_task_receipt(&session, "accepted-alpha-task-check").await?;
    install_host_completion_fixture(
        &session,
        ROOT_TASK_ID,
        completion_state(&alpha_artifact_ref),
    )
    .await
    .map_err(anyhow::Error::msg)?;

    let alpha_launch = session
        .prepare_completion_review_launch(
            TARGET_TASK_ID,
            Some(REVIEW_UNIT_ID),
            "review-alpha-spawn",
            Some(REVIEWER_ROLE),
            Some(REVIEWER_MODEL),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    anyhow::ensure!(alpha_launch.should_spawn);
    let alpha_child =
        ThreadId::try_from("01a1c606-48b8-7cf3-bb78-77b47f6b8c36").expect("valid Alpha child ID");
    let alpha_turn = "review-alpha-turn";
    spawn_and_bind_attempt(
        &session,
        REVIEW_UNIT_ID,
        &alpha_artifact_ref,
        "review-alpha-spawn",
        alpha_child,
        alpha_turn,
    )
    .await?;
    session
        .collect_completion_review_results(
            "review-alpha-wait",
            &[(
                alpha_child,
                Some(alpha_turn.to_string()),
                AgentStatus::Completed(Some("Alpha satisfies AC-1".into())),
            )],
        )
        .await;
    observe_native_wait_result(
        &session,
        alpha_child,
        AgentStatus::Completed(Some("Alpha satisfies AC-1".into())),
    )
    .await;
    session
        .adopt_completion_review(
            REVIEW_UNIT_ID,
            "Alpha was reviewed against its accepted artifact".into(),
        )
        .await
        .map_err(anyhow::Error::msg)?;

    let mut close_duplicate = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("Alpha completion plan exists");
    close_duplicate
        .units
        .iter_mut()
        .find(|unit| unit.id == "review-a-legacy-planned")
        .expect("unstarted duplicate review exists")
        .status = UnitStatus::Cancelled;
    session
        .set_rencrow_completion_state_with_call_id(
            ROOT_TASK_ID.into(),
            serde_json::to_value(close_duplicate)?,
            Some("update-cancel-alpha-duplicate".into()),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    anyhow::ensure!(session.autonomous_completion_is_complete().await);

    let workspace = alpha_workspace.path();
    std::fs::write(
        workspace.join("review-target.txt"),
        "Beta replaces the artifact bytes\n",
    )?;
    anyhow::ensure!(
        session
            .reconcile_rencrow_completion_evidence(ROOT_TASK_ID)
            .await
            .map_err(anyhow::Error::msg)?,
        "the real artifact change must invalidate the prior accepted evidence"
    );
    assert!(session.autonomous_completion_pending().await);
    assert!(!session.autonomous_completion_is_complete().await);

    let mut beta_execution_plan = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("Alpha plan remains registered after invalidation");
    beta_execution_plan.contract_revision += 1;
    beta_execution_plan.units.push(CompletionUnit {
        id: BETA_TASK_ID.into(),
        purpose: "Execute and accept the replacement task Beta".into(),
        status: UnitStatus::Planned,
        kind: CompletionUnitKind::Execution,
        condition_refs: vec!["AC-1".into()],
        depends_on: vec![TARGET_TASK_ID.into()],
        evidence: Vec::new(),
        task_refs: Vec::new(),
        review_target: None,
        review_evidence: None,
        review_launch: None,
        review_decision_reason: None,
        review_state: None,
        review_revalidation_of: None,
    });
    session
        .set_rencrow_completion_state_with_call_id(
            ROOT_TASK_ID.into(),
            serde_json::to_value(beta_execution_plan)?,
            Some("update-plan-beta-execution".into()),
        )
        .await
        .map_err(anyhow::Error::msg)?;

    let (beta_artifact_ref, beta_boundary) = install_accepted_task_receipt_for_new_target(
        &session,
        "accepted-beta-task-check",
        BETA_TASK_ID,
        /*task_revision*/ 1,
        "Beta replaces the artifact bytes\n",
        alpha_workspace.path(),
    )
    .await?;
    let beta_evidence = CompletionEvidenceRef {
        task_id: BETA_TASK_ID.into(),
        task_revision: 1,
        artifact_ref: beta_artifact_ref.clone(),
        verification_ref: beta_artifact_ref.clone(),
    };
    let mut beta_plan = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("planned Beta execution remains registered after its accepted receipt");
    beta_plan.contract_revision += 1;
    let beta_execution = beta_plan
        .units
        .iter_mut()
        .find(|unit| unit.id == BETA_TASK_ID)
        .expect("planned Beta execution exists");
    anyhow::ensure!(beta_execution.status == UnitStatus::Planned);
    beta_execution.status = UnitStatus::Done;
    beta_execution.evidence.push(beta_evidence.clone());
    beta_plan.conditions[0].status = AcceptanceStatus::Satisfied;
    beta_plan.conditions[0].evidence = vec![beta_evidence.clone()];
    beta_plan.conditions[0].unmet_reason = None;
    // Mirror apply_host_acceptance's host evidence projection and persist it before the
    // separate Update that introduces the review revalidation relationship.
    session
        .set_rencrow_completion_state_with_call_id(
            ROOT_TASK_ID.into(),
            serde_json::to_value(beta_plan.clone())?,
            Some("host-accept-beta-task-check".into()),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    let accepted_beta_plan = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("host-accepted Beta execution and AC evidence are persisted");
    anyhow::ensure!(
        accepted_beta_plan
            .units
            .iter()
            .any(|unit| unit.id == BETA_TASK_ID
                && unit.status == UnitStatus::Done
                && unit.evidence.contains(&beta_evidence))
            && accepted_beta_plan.conditions[0].evidence == vec![beta_evidence.clone()],
        "host acceptance must persist the Beta execution and AC receipt"
    );

    let mut beta_review_plan = accepted_beta_plan;
    beta_review_plan.contract_revision += 1;
    beta_review_plan.units.push(CompletionUnit {
        id: BETA_REVIEW_UNIT_ID.into(),
        purpose: "Review Beta as an explicit revalidation of adopted Alpha".into(),
        status: UnitStatus::Planned,
        kind: CompletionUnitKind::Review,
        condition_refs: vec!["AC-1".into()],
        depends_on: vec![BETA_TASK_ID.into()],
        evidence: Vec::new(),
        task_refs: Vec::new(),
        review_target: Some(CompletionReviewTarget {
            task_id: BETA_TASK_ID.into(),
            task_revision: 1,
        }),
        review_evidence: None,
        review_launch: None,
        review_decision_reason: None,
        review_state: None,
        review_revalidation_of: Some(REVIEW_UNIT_ID.into()),
    });
    let before_invalid_update = session.rencrow_completion_state(ROOT_TASK_ID).await;
    let mut widened_scope = beta_review_plan.clone();
    let mut additional_condition = widened_scope.conditions[0].clone();
    additional_condition.id = "AC-2".into();
    additional_condition.status = AcceptanceStatus::Open;
    additional_condition.evidence.clear();
    widened_scope
        .units
        .iter_mut()
        .find(|unit| unit.id == BETA_REVIEW_UNIT_ID)
        .expect("new Beta review exists")
        .condition_refs
        .push(additional_condition.id.clone());
    widened_scope.conditions.push(additional_condition);
    let scope_error = session
        .set_rencrow_completion_state_with_call_id(
            ROOT_TASK_ID.into(),
            serde_json::to_value(widened_scope)?,
            Some("update-reject-widened-alpha-review-scope".into()),
        )
        .await
        .expect_err("revalidation cannot widen the adopted source obligation");
    assert!(scope_error.contains("preserve condition_refs exactly"));
    assert_eq!(
        session.rencrow_completion_state(ROOT_TASK_ID).await,
        before_invalid_update,
        "rejected revalidation must not change the accepted plan or its evidence"
    );
    session
        .set_rencrow_completion_state_with_call_id(
            ROOT_TASK_ID.into(),
            serde_json::to_value(beta_review_plan)?,
            Some("update-link-beta-review-to-alpha".into()),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    let linked_plan = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("host-validated Alpha-to-Beta review relationship exists");
    let beta_review = linked_plan
        .units
        .iter()
        .find(|unit| unit.id == BETA_REVIEW_UNIT_ID)
        .expect("Beta revalidation unit exists");
    let source = beta_review
        .review_state
        .as_ref()
        .and_then(|review| review.revalidation.as_ref())
        .expect("the host bound Beta to Alpha's exact adoption");
    anyhow::ensure!(
        source.source_unit_id == REVIEW_UNIT_ID
            && source.required_artifact_fingerprint
                == codex_history::task_context::task_check_artifact_fingerprint(&beta_artifact_ref,)
                    .ok_or_else(|| anyhow::anyhow!("Beta accepted receipt has no fingerprint"))?
            && source.source_snapshot.sha256.len() == 64
            && source.source_adoption_sha256.len() == 64,
        "revalidation proof must bind Alpha adoption and Beta's accepted artifact"
    );
    assert!(session.autonomous_completion_pending().await);
    assert!(!session.autonomous_completion_is_complete().await);

    let beta_launch = session
        .prepare_completion_review_launch(
            BETA_TASK_ID,
            Some(BETA_REVIEW_UNIT_ID),
            "review-beta-spawn",
            Some(REVIEWER_ROLE),
            Some(REVIEWER_MODEL),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    anyhow::ensure!(beta_launch.should_spawn);
    let beta_child =
        ThreadId::try_from("01a1c606-48b8-7cf3-bb78-77b47f6b8c37").expect("valid Beta child ID");
    let beta_turn = "review-beta-turn";
    spawn_and_bind_attempt_for_target(
        &session,
        BETA_REVIEW_UNIT_ID,
        CompletionReviewTarget {
            task_id: BETA_TASK_ID.into(),
            task_revision: 1,
        },
        &beta_artifact_ref,
        "review-beta-spawn",
        beta_child,
        beta_turn,
    )
    .await?;
    session
        .collect_completion_review_results(
            "review-beta-wait",
            &[(
                beta_child,
                Some(beta_turn.to_string()),
                AgentStatus::Completed(Some("Beta satisfies AC-1".into())),
            )],
        )
        .await;
    observe_native_wait_result(
        &session,
        beta_child,
        AgentStatus::Completed(Some("Beta satisfies AC-1".into())),
    )
    .await;
    session
        .adopt_completion_review(
            BETA_REVIEW_UNIT_ID,
            "Beta was reviewed as Alpha's explicit successor".into(),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    assert!(!session.autonomous_completion_pending().await);
    assert!(session.autonomous_completion_is_complete().await);

    let snapshots = durable_completion_snapshots(&rollout_path).await?;
    {
        let mut state = session.state.lock().await;
        state.rencrow_completion_state = None;
        state.rencrow_completion_paused = false;
        state.task_check_ledger = crate::task_check::TaskCheckLedger::default();
        state.rencrow_completion_last_persisted_snapshot = None;
    }
    let mut restore_items = vec![beta_boundary];
    restore_items.extend(snapshots.into_iter().map(completion_snapshot_item));
    session
        .restore_autonomous_completion_snapshot(&restore_items)
        .await;
    assert!(!session.autonomous_completion_pending().await);
    assert!(session.autonomous_completion_is_complete().await);
    Ok(())
}

#[tokio::test]
async fn retry_authorization_is_not_durable_without_the_live_thread_writer() -> anyhow::Result<()> {
    let mut fixture = setup_failed_review().await?;
    let before = latest_durable_completion_snapshot(&fixture.rollout_path).await?;
    let before_review = durable_unit(&before, REVIEW_UNIT_ID);
    assert_eq!(
        before_review["review_state"]["active_attempt"]["status"],
        "failed"
    );
    assert!(before_review["review_state"]["retry_authorization"].is_null());

    fixture.session.services.live_thread = None;
    let retry = CompletionReviewRetryRequest {
        unit_id: REVIEW_UNIT_ID.into(),
        expected_attempt_id: fixture.first_attempt_id.clone(),
        invocation_call_id: "unpersistable-plan-review-call".into(),
        previous_plan: PREVIOUS_PLAN.into(),
        revised_plan: REVISED_PLAN.into(),
        reason: RETRY_REASON.into(),
    };
    let retry_call_id = retry.invocation_call_id.clone();
    assert!(
        fixture
            .session
            .record_autonomous_plan_review(
                ROOT_TASK_ID,
                &retry_call_id,
                retry.previous_plan.clone(),
                retry.revised_plan.clone(),
                retry.reason.clone(),
                Some(retry),
            )
            .await
            .is_err()
    );
    assert!(fixture.session.autonomous_completion_paused().await);
    let after = latest_durable_completion_snapshot(&fixture.rollout_path).await?;
    assert_eq!(
        after, before,
        "without a writer, the durable failure snapshot stays unchanged"
    );
    assert_eq!(
        durable_unit(&after, REVIEW_UNIT_ID)["review_state"]["retry_authorization"],
        Value::Null,
        "failed persistence must not publish a retry grant"
    );
    assert_eq!(
        durable_unit(&after, REVIEW_UNIT_ID)["review_state"]["active_attempt"]["attempt_id"],
        fixture.first_attempt_id
    );
    let writerless_launch = fixture
        .session
        .prepare_completion_review_launch(
            TARGET_TASK_ID,
            Some(REVIEW_UNIT_ID),
            "spawn-must-not-be-authorized",
            Some(REVIEWER_ROLE),
            Some(REVIEWER_MODEL),
        )
        .await
        .expect("existing failed review remains queryable without a writer");
    assert!(!writerless_launch.should_spawn);
    let writerless_state = fixture
        .session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("failed review remains registered after unavailable persistence");
    assert!(writerless_state.units.iter().any(|unit| {
        unit.id == REVIEW_UNIT_ID
            && unit.status == UnitStatus::Failed
            && unit
                .review_state
                .as_ref()
                .is_some_and(|review| {
                    review.retry_authorization.is_none()
                        && review.active_attempt.as_ref().is_some_and(|attempt| {
                            attempt.attempt_id == fixture.first_attempt_id
                                && attempt.status
                                    == codex_history::task_context::CompletionReviewAttemptStatus::Failed
                        })
                })
    }));
    Ok(())
}

fn completion_snapshot_item(snapshot: Value) -> RolloutItem {
    let item = Box::new(crate::context::AutonomousCompletionSnapshot).into_boxed_response_item();
    RolloutItem::ResponseItem(codex_history::ResponseItemEnvelope {
        item,
        metadata: Some(codex_history::CodexHarnessMetadata {
            rencrow_completion: Some(snapshot),
            ..Default::default()
        }),
    })
}

#[tokio::test]
async fn legacy_cancelled_review_survives_v2_opt_in_and_cold_restore_until_other_work_finishes()
-> anyhow::Result<()> {
    let (mut session, _) = make_session_and_context().await;
    let rollout_path = attach_thread_persistence(&mut session).await;
    let (target_artifact_ref, _task_workspace, accepted_boundary) =
        install_accepted_task_receipt(&session, "accepted-task-check").await?;
    let mut legacy_state = completion_state(&target_artifact_ref);
    legacy_state.units[1].status = UnitStatus::Cancelled;
    legacy_state.units.push(CompletionUnit {
        id: "review-c-required".into(),
        purpose: "A separate review obligation that remains required".into(),
        status: UnitStatus::Planned,
        kind: CompletionUnitKind::Review,
        condition_refs: vec!["AC-1".into()],
        depends_on: vec![TARGET_TASK_ID.into()],
        evidence: Vec::new(),
        task_refs: Vec::new(),
        review_target: Some(CompletionReviewTarget {
            task_id: TARGET_TASK_ID.into(),
            task_revision: 1,
        }),
        review_evidence: None,
        review_launch: None,
        review_decision_reason: None,
        review_state: None,
        review_revalidation_of: None,
    });
    session.state.lock().await.rencrow_completion_state = Some((ROOT_TASK_ID.into(), legacy_state));
    session
        .persist_autonomous_completion_snapshot()
        .await
        .map_err(anyhow::Error::msg)?;
    session.flush_rollout().await?;
    let v1_snapshot = latest_durable_completion_snapshot(&rollout_path).await?;
    anyhow::ensure!(
        v1_snapshot["version"] == 1,
        "legacy cancellation fixture must persist as v1"
    );
    assert_eq!(
        durable_unit(&v1_snapshot, REVIEW_UNIT_ID)["status"],
        "cancelled"
    );
    assert!(durable_unit(&v1_snapshot, REVIEW_UNIT_ID)["review_state"].is_null());

    let mut explicit_update = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("legacy cancelled and planned units remain registered");
    explicit_update
        .units
        .iter_mut()
        .find(|unit| unit.id == "review-a-legacy-planned")
        .expect("second planned review exists")
        .status = UnitStatus::Cancelled;
    session
        .set_rencrow_completion_state_with_call_id(
            ROOT_TASK_ID.into(),
            serde_json::to_value(explicit_update)?,
            Some("update-v2-opt-in".into()),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    let v2_snapshot = latest_durable_completion_snapshot(&rollout_path).await?;
    anyhow::ensure!(
        v2_snapshot["version"] == 2,
        "the explicit update opts this state into v2"
    );
    let migrated = durable_unit(&v2_snapshot, REVIEW_UNIT_ID);
    assert_eq!(migrated["status"], "cancelled");
    assert_eq!(
        migrated["review_state"]["withdrawal"]["source"]["kind"],
        "legacy_v1_snapshot"
    );
    assert_eq!(
        migrated["review_state"]["withdrawal"]["source"]["snapshot"]["version"],
        1
    );
    let expected_legacy_source = migrated["review_state"]["withdrawal"]["source"].clone();
    let updated = durable_unit(&v2_snapshot, "review-a-legacy-planned");
    assert_eq!(
        updated["review_state"]["withdrawal"]["source"]["kind"],
        "host_update"
    );
    assert_eq!(
        updated["review_state"]["withdrawal"]["source"]["update_call_id"],
        "update-v2-opt-in"
    );

    {
        let mut state = session.state.lock().await;
        state.rencrow_completion_state = None;
        state.rencrow_completion_paused = false;
        state.task_check_ledger = crate::task_check::TaskCheckLedger::default();
        state.rencrow_completion_last_persisted_snapshot = None;
    }
    session
        .restore_autonomous_completion_snapshot(&[
            accepted_boundary,
            completion_snapshot_item(v1_snapshot),
            completion_snapshot_item(v2_snapshot),
        ])
        .await;
    let restored = session
        .rencrow_completion_state(ROOT_TASK_ID)
        .await
        .expect("cold restore retains the v2 contract");
    let legacy = restored
        .units
        .iter()
        .find(|unit| unit.id == REVIEW_UNIT_ID)
        .expect("legacy review remains present");
    assert_eq!(legacy.status, UnitStatus::Cancelled);
    let restored_legacy_source = legacy
        .review_state
        .as_ref()
        .and_then(|review| review.withdrawal.as_ref())
        .map(|withdrawal| serde_json::to_value(&withdrawal.source))
        .transpose()?
        .expect("exact v1 snapshot source was restored");
    assert_eq!(restored_legacy_source, expected_legacy_source);
    assert!(session.autonomous_completion_pending().await);
    assert!(!session.autonomous_completion_is_complete().await);

    let required_unit = "review-c-required";
    let launch = session
        .prepare_completion_review_launch(
            TARGET_TASK_ID,
            Some(required_unit),
            "review-c-spawn-after-restore",
            Some(REVIEWER_ROLE),
            Some(REVIEWER_MODEL),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    assert!(launch.should_spawn);
    let child_id =
        ThreadId::try_from("01a1c606-48b8-7cf3-bb78-77b47f6b8c31").expect("valid child ID");
    let turn_id = "review-c-turn-after-restore";
    spawn_and_bind_attempt(
        &session,
        required_unit,
        &target_artifact_ref,
        "review-c-spawn-after-restore",
        child_id,
        turn_id,
    )
    .await?;
    session
        .collect_completion_review_results(
            "review-c-wait-after-restore",
            &[(
                child_id,
                Some(turn_id.to_string()),
                AgentStatus::Completed(Some("The accepted artifact meets AC-1".into())),
            )],
        )
        .await;
    observe_native_wait_result(
        &session,
        child_id,
        AgentStatus::Completed(Some("The accepted artifact meets AC-1".into())),
    )
    .await;
    assert!(session.autonomous_completion_pending().await);
    assert!(!session.autonomous_completion_is_complete().await);
    session
        .adopt_completion_review(
            required_unit,
            "The independent post-restore review confirmed AC-1".into(),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    assert!(!session.autonomous_completion_pending().await);
    assert!(session.autonomous_completion_is_complete().await);
    Ok(())
}
