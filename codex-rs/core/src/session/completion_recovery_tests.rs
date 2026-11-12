use super::make_session_and_context;
use crate::agent::completion_recovery::SUPPORTED_RECHECK_CONDITION;
use crate::agent::completion_recovery::due_plan_review_event;
use crate::context::ContextualUserFragment;
use crate::session::session::Session;
use crate::task_check::TaskArtifactStamp;
use crate::task_check::TaskCheckKind;
use crate::task_check::TaskEvidenceRequest;
use codex_history::CodexHarnessMetadata;
use codex_history::ResponseItemEnvelope;
use codex_history::RolloutItem;
use codex_history::task_checkpoint_metadata::RenCrowTaskCheckpointMetadataV3;
use codex_history::task_checkpoint_metadata::TaskBoundarySourceFence;
use codex_history::task_context::AcceptanceCondition;
use codex_history::task_context::AcceptanceStatus;
use codex_history::task_context::AcceptedTaskReceipt;
use codex_history::task_context::BlockerCategory;
use codex_history::task_context::BoundaryCandidate;
use codex_history::task_context::BoundaryMode;
use codex_history::task_context::CompletionBlocker;
use codex_history::task_context::CompletionEvidenceRef;
use codex_history::task_context::CompletionState;
use codex_history::task_context::CompletionUnit;
use codex_history::task_context::CompletionUnitKind;
use codex_history::task_context::StoredTaskState;
use codex_history::task_context::TaskState;
use codex_history::task_context::TaskStatus;
use codex_history::task_context::UnitStatus;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::PathBuf;
use tokio::sync::oneshot;
use tokio_util::task::AbortOnDropHandle;

#[tokio::test]
async fn changed_artifact_invalidates_satisfied_condition_during_reconcile() {
    let (session, _) = make_session_and_context().await;
    let workspace = tempfile::tempdir().expect("workspace");
    let artifact = workspace.path().join("artifact.txt");
    std::fs::write(&artifact, "verified bytes\n").unwrap();
    let paths = vec![PathBuf::from("artifact.txt")];
    let before = crate::task_check::hash_workspace_artifacts(workspace.path(), &paths).unwrap();
    let fingerprint = codex_history::compaction_candidate::digest(&json!({
        "task_id": "unit-1",
        "task_revision": 1,
        "workspace_root": workspace.path().to_string_lossy(),
        "artifacts": before.iter().map(|stamp| json!({
            "path": stamp.path.to_string_lossy(),
            "sha256": stamp.sha256,
        })).collect::<Vec<_>>(),
    }))
    .unwrap();
    session
        .begin_task_check(
            "accepted-check".into(),
            TaskEvidenceRequest {
                task_id: "unit-1".into(),
                task_revision: 1,
                artifact_paths: paths,
                check_kind: TaskCheckKind::Verification,
            },
            workspace.path().to_path_buf(),
            before.clone(),
        )
        .await;
    session
        .finish_task_check("accepted-check", 0, true, before)
        .await
        .expect("successful receipt");
    let evidence = CompletionEvidenceRef {
        task_id: "unit-1".into(),
        task_revision: 1,
        artifact_ref: format!("task-check:accepted-check:{fingerprint}"),
        verification_ref: format!("task-check:accepted-check:{fingerprint}"),
    };
    session.state.lock().await.rencrow_completion_state = Some((
        "root".into(),
        CompletionState {
            contract_revision: 1,
            units_completed_since_review: 1,
            plan_review_at: None,
            plan_reviews: Vec::new(),
            paused: false,
            consumed_host_events: Vec::new(),
            conditions: vec![
                AcceptanceCondition {
                    id: "AC-1".into(),
                    revision: 1,
                    result: "Artifact remains valid".into(),
                    scope: "workspace artifact".into(),
                    verification: "Run verification".into(),
                    acceptance_criteria: "Verification succeeds against current bytes".into(),
                    status: AcceptanceStatus::Satisfied,
                    evidence: vec![evidence.clone()],
                    unmet_reason: None,
                },
                AcceptanceCondition {
                    id: "AC-2".into(),
                    revision: 1,
                    result: "Another condition remains open".into(),
                    scope: "unrelated scope".into(),
                    verification: "Run another verification".into(),
                    acceptance_criteria: "Another result is accepted".into(),
                    status: AcceptanceStatus::Open,
                    evidence: Vec::new(),
                    unmet_reason: Some("unrelated reason stays intact".into()),
                },
            ],
            units: vec![CompletionUnit {
                id: "unit-1".into(),
                purpose: "Verify artifact".into(),
                status: UnitStatus::Done,
                kind: CompletionUnitKind::Execution,
                condition_refs: vec!["AC-1".into()],
                depends_on: Vec::new(),
                evidence: vec![evidence],
                task_refs: Vec::new(),
                review_target: None,
                review_evidence: None,
                review_launch: None,
                review_decision_reason: None,
            }],
            current_unit: None,
            failed_approaches: Vec::new(),
            blocker: None,
            next_action: None,
            product_progress_at: Some(100),
            runtime_recovery_at: Some(110),
            activity_at: Some(120),
        },
    ));

    session
        .persist_autonomous_completion_snapshot()
        .await
        .expect("persist satisfied snapshot with its host receipt");
    let snapshot = session
        .state
        .lock()
        .await
        .history
        .annotated_items()
        .iter()
        .rev()
        .find_map(|item| {
            item.metadata
                .as_ref()
                .and_then(|metadata| metadata.rencrow_completion.clone())
        })
        .expect("host snapshot persisted");
    let task_boundary = accepted_task_boundary_item(
        &session.thread_id().to_string(),
        &format!("task-check:accepted-check:{fingerprint}"),
    );
    {
        let mut state = session.state.lock().await;
        state.rencrow_completion_state = None;
        state.task_check_ledger = crate::task_check::TaskCheckLedger::default();
    }
    session
        .restore_autonomous_completion_snapshot(&[task_boundary, snapshot_item(snapshot)])
        .await;
    assert_eq!(
        session
            .autonomous_completion_current_state()
            .await
            .expect("restored state")
            .1
            .conditions[0]
            .status,
        AcceptanceStatus::Satisfied,
        "cold restore retains the completed contract before artifact reconciliation"
    );

    let original = session
        .autonomous_completion_current_state()
        .await
        .unwrap()
        .1;
    assert!(
        session
            .completion_evidence_matches_current_artifact(
                &session
                    .autonomous_completion_current_state()
                    .await
                    .unwrap()
                    .1
                    .conditions[0]
                    .evidence[0]
            )
            .await
    );
    let history_count = session.state.lock().await.history.annotated_items().len();
    assert!(
        !session
            .reconcile_rencrow_completion_evidence("root")
            .await
            .expect("unchanged artifact reconcile")
    );
    assert_eq!(
        session
            .autonomous_completion_current_state()
            .await
            .unwrap()
            .1,
        original,
        "a current completed contract stays idle"
    );
    assert_eq!(
        original.conditions[1].unmet_reason.as_deref(),
        Some("unrelated reason stays intact")
    );
    assert_eq!(
        session.state.lock().await.history.annotated_items().len(),
        history_count
    );

    // Model a cold restored contract, then change the real artifact after its saved receipt.
    std::fs::write(&artifact, "changed bytes\n").unwrap();
    assert!(
        session
            .reconcile_rencrow_completion_evidence("root")
            .await
            .expect("host reconcile")
    );
    let state = session
        .autonomous_completion_current_state()
        .await
        .unwrap()
        .1;
    assert_eq!(state.conditions[0].status, AcceptanceStatus::Open);
    assert_eq!(
        state.conditions[0].evidence,
        original.conditions[0].evidence
    );
    assert!(state.conditions[0].unmet_reason.is_some());
    assert!(
        state
            .next_action
            .as_deref()
            .is_some_and(|action| action.contains("AC-1"))
    );
    assert!(
        state
            .next_action
            .as_deref()
            .is_some_and(|action| action.contains("unit-1@1"))
    );
    assert!(
        state
            .next_action
            .as_deref()
            .is_some_and(|action| action.contains("reverify-AC-1-"))
    );
    assert_eq!(state.units[0].status, UnitStatus::Done);
    assert_eq!(state.product_progress_at, Some(100));
    assert_eq!(state.runtime_recovery_at, Some(110));
    assert_eq!(state.activity_at, Some(120));
    assert_eq!(state.conditions[1], original.conditions[1]);
    assert!(session.autonomous_completion_pending().await);
    assert!(session.state.lock().await.history.annotated_items().len() > history_count);

    let blocker = CompletionBlocker {
        category: BlockerCategory::Authority,
        reason: "Existing authority blocker".into(),
        required_input: None,
        authority: Some("project owner".into()),
        evidence: Vec::new(),
        recheck_condition: None,
    };
    let mut blocked_contract = original.clone();
    blocked_contract.blocker = Some(blocker.clone());
    session.state.lock().await.rencrow_completion_state = Some(("root".into(), blocked_contract));
    std::fs::write(&artifact, "changed again\n").unwrap();
    assert!(
        session
            .reconcile_rencrow_completion_evidence("root")
            .await
            .expect("reconcile preserves an existing blocker")
    );
    let blocked = session
        .autonomous_completion_current_state()
        .await
        .unwrap()
        .1;
    assert_eq!(blocked.conditions[0].status, AcceptanceStatus::Open);
    assert_eq!(blocked.blocker, Some(blocker));
    assert_eq!(blocked.product_progress_at, Some(100));
    assert_eq!(blocked.runtime_recovery_at, Some(110));
    assert_eq!(blocked.activity_at, Some(120));
    assert!(!session.autonomous_completion_pending().await);

    session.pause_autonomous_completion().await;
    assert!(!session.autonomous_completion_pending().await);
    let paused = session
        .autonomous_completion_current_state()
        .await
        .unwrap()
        .1;
    assert!(paused.paused);
    assert_eq!(paused.blocker, blocked.blocker);
    assert_eq!(paused.conditions[0].status, AcceptanceStatus::Open);
    assert_eq!(paused.units[0].status, UnitStatus::Done);
    assert_eq!(paused.product_progress_at, Some(100));
}

fn accepted_task_boundary_item(thread_id: &str, evidence_ref: &str) -> RolloutItem {
    let boundary = BoundaryCandidate {
        version: 1,
        mode: BoundaryMode::Task,
        thread_id: thread_id.into(),
        root_task_id: "root".into(),
        closed_task_id: "unit-1".into(),
        revision: 2,
        closed_task_revision: 1,
        goal: "verify the workspace artifact".into(),
        tasks: vec![TaskState {
            id: "unit-1".into(),
            purpose: "verify artifact".into(),
            status: TaskStatus::Completed,
            depends_on: Vec::new(),
        }],
        current_task_id: None,
        artifact_ref: evidence_ref.into(),
        verification_ref: evidence_ref.into(),
        decisions: Vec::new(),
        constraints: Vec::new(),
        completion_state: None,
    };
    let candidate_hash = codex_history::archive_reference::content_sha256(
        &serde_json::to_string(&boundary).unwrap(),
    );
    let task_state = StoredTaskState {
        boundary,
        completed: BTreeMap::from([(
            "unit-1".into(),
            AcceptedTaskReceipt {
                revision: 1,
                result: "artifact verification completed".into(),
                artifact_ref: evidence_ref.into(),
                verification_ref: evidence_ref.into(),
            },
        )]),
        parent_review_ref: "review:unit-1".into(),
        previous_revision: None,
        previous_state_hash: None,
        candidate_hash,
        boundary_nonce: "nonce-unit-1".into(),
    };
    task_state
        .validate(thread_id)
        .expect("valid accepted task state");

    let summary_text = "accepted task boundary";
    let mut envelope = ResponseItemEnvelope::new(codex_protocol::models::ResponseItem::Message {
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
    let checkpoint = RenCrowTaskCheckpointMetadataV3::task_boundary(
        summary_text,
        serde_json::to_value(task_state).unwrap(),
        TaskBoundarySourceFence {
            history_hash: "a".repeat(64),
            removed_refs: Vec::new(),
        },
    );
    envelope.metadata.get_or_insert_default().rencrow_compaction =
        Some(serde_json::to_value(checkpoint).unwrap());
    RolloutItem::ResponseItem(envelope)
}

fn snapshot_item(value: serde_json::Value) -> RolloutItem {
    let item = Box::new(crate::context::AutonomousCompletionSnapshot).into_boxed_response_item();
    RolloutItem::ResponseItem(ResponseItemEnvelope {
        item,
        metadata: Some(CodexHarnessMetadata {
            rencrow_completion: Some(value),
            ..Default::default()
        }),
    })
}

fn external_blocked_state() -> serde_json::Value {
    json!({
        "contract_revision": 1,
        "conditions": [{
            "id": "AC-1",
            "revision": 1,
            "result": "The service can serve the requested model",
            "scope": "configured provider",
            "verification": "Check the configured model catalog",
            "acceptance_criteria": "The requested model is alive with a fresh check",
            "status": "open"
        }],
        "units": [{
            "id": "unit-1",
            "purpose": "Continue the blocked feature",
            "status": "ready",
            "condition_refs": ["AC-1"]
        }],
        "blocker": {
            "category": "external_service",
            "reason": "The configured model service is unavailable",
            "recheck_condition": SUPPORTED_RECHECK_CONDITION
        },
        "next_action": "Recheck the configured service",
        "product_progress_at": null,
        "runtime_recovery_at": null,
        "activity_at": null
    })
}

async fn register_blocker(session: &Session) -> CompletionState {
    session
        .set_rencrow_completion_state("root".into(), external_blocked_state())
        .await
        .expect("valid external blocker state");
    session
        .autonomous_completion_current_state()
        .await
        .expect("registered state")
        .1
}

#[tokio::test]
async fn matching_external_blocker_clears_without_changing_product_clocks() {
    let (session, _) = make_session_and_context().await;
    register_blocker(&session).await;
    {
        let mut state = session.state.lock().await;
        let completion = &mut state.rencrow_completion_state.as_mut().unwrap().1;
        completion.product_progress_at = Some(100);
        completion.activity_at = Some(110);
    }
    let expected = session
        .autonomous_completion_current_state()
        .await
        .unwrap()
        .1;

    let before_recovery = chrono::Utc::now().timestamp();
    assert!(
        session
            .clear_recovered_external_blocker("root", &expected)
            .await
            .expect("persist recovered blocker")
    );

    let saved = session
        .autonomous_completion_current_state()
        .await
        .unwrap()
        .1;
    assert_eq!(saved.blocker, None);
    assert!(
        saved
            .runtime_recovery_at
            .is_some_and(|at| at >= before_recovery && at <= chrono::Utc::now().timestamp())
    );
    assert_eq!(saved.product_progress_at, Some(100));
    assert_eq!(saved.activity_at, Some(110));
}

#[tokio::test]
async fn changed_state_user_pause_and_pending_host_check_reject_recovery() {
    let (session, _) = make_session_and_context().await;
    let stale = register_blocker(&session).await;
    let mut changed = external_blocked_state();
    changed["next_action"] = "New plan while probe was running".into();
    session
        .set_rencrow_completion_state("root".into(), changed)
        .await
        .unwrap();
    assert!(
        !session
            .clear_recovered_external_blocker("root", &stale)
            .await
            .unwrap()
    );
    assert!(
        session
            .autonomous_completion_current_state()
            .await
            .unwrap()
            .1
            .blocker
            .is_some()
    );

    session.pause_autonomous_completion().await;
    let paused = session
        .autonomous_completion_current_state()
        .await
        .unwrap()
        .1;
    assert!(
        !session
            .clear_recovered_external_blocker("root", &paused)
            .await
            .unwrap()
    );

    let (session, _) = make_session_and_context().await;
    let expected = register_blocker(&session).await;
    session
        .begin_task_check(
            "pending-check".into(),
            TaskEvidenceRequest {
                task_id: "unit-1".into(),
                task_revision: 1,
                artifact_paths: vec![PathBuf::from("artifact.txt")],
                check_kind: TaskCheckKind::Verification,
            },
            PathBuf::from("/tmp"),
            vec![TaskArtifactStamp {
                path: PathBuf::from("artifact.txt"),
                sha256: "hash".into(),
            }],
        )
        .await;
    assert!(
        !session
            .clear_recovered_external_blocker("root", &expected)
            .await
            .unwrap()
    );
    assert!(
        session
            .autonomous_completion_current_state()
            .await
            .unwrap()
            .1
            .blocker
            .is_some()
    );
}

#[tokio::test]
async fn unsupported_external_recheck_sentinels_are_never_candidates() {
    let (session, _) = make_session_and_context().await;
    let mut state = external_blocked_state();
    state["blocker"]["recheck_condition"] = "model_supplied_custom_probe".into();
    session
        .set_rencrow_completion_state("root".into(), state)
        .await
        .unwrap();

    assert!(
        session
            .autonomous_completion_recovery_candidate()
            .await
            .is_none()
    );
}

#[tokio::test]
async fn one_observer_is_owned_and_rejected_duplicate_is_aborted() {
    let (session, _) = make_session_and_context().await;
    assert!(
        session
            .install_completion_recovery_task(AbortOnDropHandle::new(tokio::spawn(async {
                std::future::pending::<()>().await;
            })))
            .await
    );

    let (started_tx, started_rx) = oneshot::channel();
    let (dropped_tx, dropped_rx) = oneshot::channel();
    let duplicate = tokio::spawn(async move {
        struct DropSignal(Option<oneshot::Sender<()>>);
        impl Drop for DropSignal {
            fn drop(&mut self) {
                if let Some(signal) = self.0.take() {
                    let _ = signal.send(());
                }
            }
        }
        let _signal = DropSignal(Some(dropped_tx));
        let _ = started_tx.send(());
        std::future::pending::<()>().await;
    });
    started_rx.await.unwrap();
    assert!(
        !session
            .install_completion_recovery_task(AbortOnDropHandle::new(duplicate))
            .await
    );
    tokio::time::timeout(std::time::Duration::from_secs(1), dropped_rx)
        .await
        .expect("rejected observer gets aborted")
        .unwrap();
}

#[tokio::test]
async fn idle_review_deadline_is_observed_once_and_respects_stop_and_uncollected_child() {
    let (session, _) = make_session_and_context().await;
    session
        .set_rencrow_completion_state(
            "root".into(),
            serde_json::json!({
                "contract_revision":1,
                "conditions":[{"id":"AC-1","revision":1,"result":"CLI works",
                    "scope":"CLI","verification":"Run CLI","acceptance_criteria":"exit 0","status":"open"}],
                "units":[],"next_action":"Implement CLI"
            }),
        )
        .await
        .unwrap();
    assert!(due_plan_review_event(&session).await.is_none());
    {
        let mut state = session.state.lock().await;
        let contract = &mut state.rencrow_completion_state.as_mut().unwrap().1;
        contract.plan_review_at = Some(chrono::Utc::now().timestamp() - 1801);
        contract.product_progress_at = None;
    }
    let key = due_plan_review_event(&session)
        .await
        .expect("idle deadline is due without a new model turn");
    {
        let mut state = session.state.lock().await;
        state
            .rencrow_completion_uncollected_children
            .insert("01a1c606-48b8-7cf3-bb78-77b47f6b8c2f".into());
    }
    assert!(due_plan_review_event(&session).await.is_none());
    {
        let mut state = session.state.lock().await;
        state.rencrow_completion_uncollected_children.clear();
        state
            .rencrow_completion_state
            .as_mut()
            .unwrap()
            .1
            .consumed_host_events
            .push(key);
    }
    assert!(due_plan_review_event(&session).await.is_none());
    {
        let mut state = session.state.lock().await;
        state
            .rencrow_completion_state
            .as_mut()
            .unwrap()
            .1
            .consumed_host_events
            .clear();
        state
            .task_check_ledger
            .restore_snapshot(
                serde_json::from_value(serde_json::json!([{
                    "task_id":"unit-a", "task_revision":1,
                    "attempt":{"call_id":"unfinished-check", "status":"Pending"}
                }]))
                .unwrap(),
            )
            .unwrap();
    }
    assert!(due_plan_review_event(&session).await.is_none());
    {
        let mut state = session.state.lock().await;
        state
            .task_check_ledger
            .restore_snapshot(Vec::new())
            .unwrap();
        state.rencrow_completion_paused = true;
    }
    assert!(due_plan_review_event(&session).await.is_none());
}
