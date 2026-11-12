use super::InitialHistory;
use super::ResumedHistory;
use super::RolloutItem;
use super::RolloutRecorder;
use super::attach_thread_persistence;
use super::make_session_and_context;
use crate::session::session::history_ends_with_completed_turn;
use codex_history::task_context::AcceptanceCondition;
use codex_history::task_context::AcceptanceStatus;
use codex_history::task_context::CompletionEvidenceRef;
use codex_history::task_context::CompletionReviewEvidence;
use codex_history::task_context::CompletionReviewTarget;
use codex_history::task_context::CompletionState;
use codex_history::task_context::CompletionUnit;
use codex_history::task_context::UnitStatus;
use pretty_assertions::assert_eq;

fn open_contract() -> CompletionState {
    CompletionState {
        contract_revision: 1,
        units_completed_since_review: 0,
        plan_review_at: None,
        plan_reviews: vec![],
        paused: false,
        consumed_host_events: vec![],
        conditions: vec![AcceptanceCondition {
            id: "AC-1".into(),
            revision: 1,
            result: "The requested feature works".into(),
            scope: "CLI".into(),
            verification: "Run the end-to-end command".into(),
            acceptance_criteria: "The command produces the expected output".into(),
            status: AcceptanceStatus::Open,
            evidence: vec![],
            unmet_reason: None,
        }],
        units: vec![CompletionUnit {
            id: "unit-1".into(),
            purpose: "Implement the CLI behavior".into(),
            status: UnitStatus::Ready,
            kind: codex_history::task_context::CompletionUnitKind::Execution,
            condition_refs: vec!["AC-1".into()],
            depends_on: vec![],
            evidence: vec![],
            task_refs: vec![],
            review_target: None,
            review_evidence: None,
            review_launch: None,
            review_decision_reason: None,
        }],
        current_unit: Some("unit-1".into()),
        failed_approaches: vec![],
        blocker: None,
        next_action: Some("Start unit-1".into()),
        product_progress_at: None,
        runtime_recovery_at: None,
        activity_at: None,
    }
}

async fn record_test_artifact_receipt(
    session: &super::Session,
    workspace: &std::path::Path,
    task_id: &str,
    task_revision: u64,
    call_id: &str,
) -> CompletionEvidenceRef {
    let artifact_paths = vec![std::path::PathBuf::from("artifact.txt")];
    let before = crate::task_check::hash_workspace_artifacts(workspace, &artifact_paths).unwrap();
    let request = crate::task_check::TaskEvidenceRequest {
        task_id: task_id.to_string(),
        task_revision,
        artifact_paths,
        check_kind: crate::task_check::TaskCheckKind::Verification,
    };
    session
        .begin_task_check(
            call_id.to_string(),
            request,
            workspace.to_path_buf(),
            before.clone(),
        )
        .await;
    session
        .finish_task_check(call_id, 0, true, before)
        .await
        .expect("successful receipt");
    CompletionEvidenceRef {
        task_id: task_id.to_string(),
        task_revision,
        artifact_ref: format!("task-check:{call_id}:test-fingerprint"),
        verification_ref: format!("task-check:{call_id}:test-fingerprint"),
    }
}

#[tokio::test]
async fn product_progress_requires_changed_artifact_bytes_not_new_receipt_identity() {
    let (session, _) = make_session_and_context().await;
    let workspace = tempfile::tempdir().expect("workspace");
    let artifact = workspace.path().join("artifact.txt");
    std::fs::write(&artifact, "same bytes\n").unwrap();
    let old = record_test_artifact_receipt(&session, workspace.path(), "task-a", 1, "call-a").await;
    let same_bytes_new_identity =
        record_test_artifact_receipt(&session, workspace.path(), "task-b", 4, "call-b").await;
    assert_ne!(old.task_id, same_bytes_new_identity.task_id);
    assert_ne!(old.artifact_ref, same_bytes_new_identity.artifact_ref);
    let old_bytes = session.task_evidence_artifact_bytes(&old).await.unwrap();
    let same_bytes = session
        .task_evidence_artifact_bytes(&same_bytes_new_identity)
        .await
        .unwrap();
    assert_eq!(old_bytes, same_bytes);
    assert!(
        !crate::session::session::condition_progress_is_product_progress(
            true,
            true,
            true,
            Some(&old_bytes),
            Some(&same_bytes),
        ),
        "a new call/task identity over identical artifact bytes cannot reset product progress"
    );

    std::fs::write(&artifact, "changed bytes\n").unwrap();
    let changed =
        record_test_artifact_receipt(&session, workspace.path(), "task-b", 5, "call-b-changed")
            .await;
    let changed_bytes = session
        .task_evidence_artifact_bytes(&changed)
        .await
        .unwrap();
    assert!(
        crate::session::session::condition_progress_is_product_progress(
            true,
            true,
            true,
            Some(&old_bytes),
            Some(&changed_bytes),
        ),
        "a host-observed artifact byte change followed by fresh verification is product progress"
    );
    assert!(
        !crate::session::session::condition_progress_is_product_progress(
            true,
            true,
            true,
            None,
            Some(&changed_bytes),
        ),
        "missing historical receipt bytes cannot authorize a progress clock reset"
    );
}

#[tokio::test]
async fn initial_open_contract_is_available_to_the_live_session_before_any_boundary() {
    let (session, _) = make_session_and_context().await;
    session
        .set_rencrow_completion_state(
            "root".into(),
            serde_json::to_value(open_contract()).unwrap(),
        )
        .await
        .expect("initial contract can be registered before task acceptance");

    let saved = session.rencrow_completion_state("root").await.unwrap();
    assert_eq!(saved.conditions, open_contract().conditions);
    assert_eq!(saved.units, open_contract().units);
    assert!(
        saved.plan_review_at.is_some(),
        "host establishes initial review clock"
    );
    assert!(session.autonomous_completion_pending().await);
}

#[tokio::test]
async fn planned_review_can_reference_future_task_but_evidence_and_spawn_need_receipts() {
    let (session, _) = make_session_and_context().await;
    let mut contract = open_contract();
    contract.units.push(CompletionUnit {
        id: "review-a".into(),
        purpose: "Review unit-1 after its accepted task revision exists".into(),
        status: UnitStatus::Planned,
        kind: codex_history::task_context::CompletionUnitKind::Review,
        condition_refs: vec!["AC-1".into()],
        depends_on: vec!["unit-1".into()],
        evidence: vec![],
        task_refs: vec![],
        review_target: Some(codex_history::task_context::CompletionReviewTarget {
            task_id: "task-a".into(),
            task_revision: 1,
        }),
        review_evidence: None,
        review_launch: None,
        review_decision_reason: None,
    });
    session
        .set_rencrow_completion_state("root".into(), serde_json::to_value(&contract).unwrap())
        .await
        .expect("a planned review may point to an execution task not accepted yet");
    assert_eq!(
        session
            .rencrow_completion_state("root")
            .await
            .unwrap()
            .units[1]
            .review_target,
        contract.units[1].review_target
    );

    let mut fabricated = serde_json::to_value(&contract).unwrap();
    fabricated["conditions"][0]["evidence"] = serde_json::json!([{
        "task_id": "task-a", "task_revision": 1
    }]);
    let error = session
        .set_rencrow_completion_state("root".into(), fabricated)
        .await
        .expect_err("unaccepted task evidence must not be canonicalized as a receipt");
    assert!(error.contains("no accepted host receipt matches completion evidence task-a@1"));
    assert!(
        session
            .rencrow_completion_state("root")
            .await
            .unwrap()
            .conditions[0]
            .evidence
            .is_empty()
    );

    {
        let mut state = session.state.lock().await;
        state.rencrow_completion_state.as_mut().unwrap().1.units[1].status = UnitStatus::Ready;
    }
    let error = session
        .validate_completion_review_spawn("review-a", "task-a", 1)
        .await
        .expect_err("review spawn still requires an accepted target receipt");
    assert!(error.contains("review target has no accepted task receipt"));
}

#[tokio::test]
async fn model_completion_update_cannot_clear_or_replace_unknown_review_launch() {
    let (session, _) = make_session_and_context().await;
    let mut contract = open_contract();
    let unit = &mut contract.units[0];
    unit.kind = codex_history::task_context::CompletionUnitKind::Review;
    unit.status = UnitStatus::Running;
    unit.review_target = Some(codex_history::task_context::CompletionReviewTarget {
        task_id: "task-a".into(),
        task_revision: 1,
    });
    let launch = codex_history::task_context::CompletionReviewLaunchState {
        spawn_call_id: "host-review-call".into(),
        status: codex_history::task_context::CompletionReviewLaunchStatus::Unknown,
        reason: Some("ambiguous post-spawn failure".into()),
    };
    unit.review_launch = Some(launch.clone());
    session.state.lock().await.rencrow_completion_state = Some(("root".into(), contract.clone()));

    let mut omitted = serde_json::to_value(&contract).unwrap();
    omitted["units"][0]
        .as_object_mut()
        .unwrap()
        .remove("review_launch");
    session
        .set_rencrow_completion_state("root".into(), omitted)
        .await
        .expect("omitted host field is restored by the setter");
    assert_eq!(
        session
            .rencrow_completion_state("root")
            .await
            .unwrap()
            .units[0]
            .review_launch,
        Some(launch.clone())
    );

    let mut forged = serde_json::to_value(&contract).unwrap();
    forged["units"][0]["review_launch"]["spawn_call_id"] = serde_json::json!("model-call");
    forged["units"][0]["review_launch"]["status"] = serde_json::json!("reserved");
    forged["units"][0]["review_launch"]["reason"] = serde_json::Value::Null;
    session
        .set_rencrow_completion_state("root".into(), forged)
        .await
        .expect("model-supplied host field is ignored");
    assert_eq!(
        session
            .rencrow_completion_state("root")
            .await
            .unwrap()
            .units[0]
            .review_launch,
        Some(launch)
    );

    let mut removed = serde_json::to_value(&contract).unwrap();
    removed["units"] = serde_json::json!([]);
    let error = session
        .set_rencrow_completion_state("root".into(), removed)
        .await
        .expect_err("unresolved host launch cannot be removed from the plan");
    assert!(error.contains("cannot remove unresolved review unit"));

    let mut retargeted = serde_json::to_value(&contract).unwrap();
    retargeted["units"][0]["review_target"]["task_revision"] = serde_json::json!(2);
    let error = session
        .set_rencrow_completion_state("root".into(), retargeted)
        .await
        .expect_err("unresolved host launch cannot be retargeted");
    assert!(error.contains("cannot change the identity or target"));
}

#[tokio::test]
async fn pending_review_evidence_cannot_be_removed_or_retargeted() {
    let (session, _) = make_session_and_context().await;
    let mut contract = open_contract();
    let unit = &mut contract.units[0];
    unit.kind = codex_history::task_context::CompletionUnitKind::Review;
    unit.status = UnitStatus::Running;
    unit.review_target = Some(CompletionReviewTarget {
        task_id: "task-a".into(),
        task_revision: 1,
    });
    unit.review_evidence = Some(CompletionReviewEvidence {
        reviewer_thread_id: "01a1c77f-15cf-7491-9372-cea9ad0c408a".into(),
        reviewer_turn_id: "pending-generation:review-call".into(),
        reviewer_role: "analyst".into(),
        spawn_call_id: "review-call".into(),
        wait_result_ref: None,
        terminal_status: None,
        wait_call_id: None,
        target_task_id: "task-a".into(),
        target_task_revision: 1,
        target_artifact_ref: "task-check:accepted-artifact".into(),
    });
    session.state.lock().await.rencrow_completion_state = Some(("root".into(), contract.clone()));

    let mut omitted = serde_json::to_value(&contract).unwrap();
    omitted["units"] = serde_json::json!([]);
    let error = session
        .set_rencrow_completion_state("root".into(), omitted)
        .await
        .expect_err("unresolved host-bound review cannot be removed");
    assert!(error.contains("cannot remove unresolved review unit"));

    let mut renamed = serde_json::to_value(&contract).unwrap();
    renamed["units"][0]["id"] = serde_json::json!("replacement-review");
    let error = session
        .set_rencrow_completion_state("root".into(), renamed)
        .await
        .expect_err("unresolved host-bound review cannot be renamed");
    assert!(error.contains("cannot remove unresolved review unit"));

    let mut retargeted = serde_json::to_value(&contract).unwrap();
    retargeted["units"][0]["review_target"]["task_revision"] = serde_json::json!(2);
    let error = session
        .set_rencrow_completion_state("root".into(), retargeted)
        .await
        .expect_err("unresolved host-bound review cannot be retargeted");
    assert!(error.contains("cannot change the identity or target"));
}

#[tokio::test]
async fn initial_contract_survives_rollout_resume_before_any_accepted_task_boundary() {
    let (mut session, _) = make_session_and_context().await;
    let rollout_path = attach_thread_persistence(&mut session).await;
    session
        .set_rencrow_completion_state(
            "root".into(),
            serde_json::to_value(open_contract()).unwrap(),
        )
        .await
        .expect("initial contract can be registered");
    session
        .flush_rollout()
        .await
        .expect("contract rollout flush");
    let expected = session.rencrow_completion_state("root").await.unwrap();

    let InitialHistory::Resumed(resumed) = RolloutRecorder::get_rollout_history(&rollout_path)
        .await
        .expect("read persisted rollout")
    else {
        panic!("expected resumed rollout");
    };
    {
        let mut state = session.state.lock().await;
        state.rencrow_completion_state = None;
        state.rencrow_completion_paused = false;
    }
    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: resumed.conversation_id,
            history: resumed.history,
            rollout_path: Some(rollout_path),
        }))
        .await;

    assert_eq!(
        session.rencrow_completion_state("root").await,
        Some(expected),
        "cold resume must restore the initial contract before the first task boundary"
    );
}

#[tokio::test]
async fn wrong_thread_host_snapshot_is_rejected_and_fails_paused() {
    let (mut session, _) = make_session_and_context().await;
    let rollout_path = attach_thread_persistence(&mut session).await;
    session
        .set_rencrow_completion_state(
            "root".into(),
            serde_json::to_value(open_contract()).unwrap(),
        )
        .await
        .unwrap();
    session.flush_rollout().await.unwrap();
    let InitialHistory::Resumed(resumed) = RolloutRecorder::get_rollout_history(&rollout_path)
        .await
        .unwrap()
    else {
        panic!("expected persisted rollout");
    };
    let mut items = (*resumed.history).clone();
    let snapshot = items
        .iter_mut()
        .rev()
        .find_map(|item| match item {
            RolloutItem::ResponseItem(envelope) => envelope
                .metadata
                .as_mut()
                .and_then(|metadata| metadata.rencrow_completion.as_mut()),
            _ => None,
        })
        .expect("host completion snapshot");
    snapshot["thread_id"] = "another-thread".into();

    {
        let mut state = session.state.lock().await;
        state.rencrow_completion_state = None;
        state.rencrow_completion_paused = false;
    }
    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: resumed.conversation_id,
            history: std::sync::Arc::new(items),
            rollout_path: Some(rollout_path),
        }))
        .await;

    assert_eq!(session.rencrow_completion_state("root").await, None);
    assert!(session.autonomous_completion_paused().await);
}

#[tokio::test]
async fn fresh_plan_text_and_model_clock_claims_do_not_count_as_product_progress() {
    let (session, _) = make_session_and_context().await;
    session
        .set_rencrow_completion_state(
            "root".into(),
            serde_json::to_value(open_contract()).unwrap(),
        )
        .await
        .unwrap();

    let mut revised_text = open_contract();
    revised_text.next_action = Some("Try a different implementation detail".into());
    revised_text.product_progress_at = Some(i64::MAX);
    revised_text.activity_at = Some(i64::MAX);
    session
        .set_rencrow_completion_state("root".into(), serde_json::to_value(revised_text).unwrap())
        .await
        .unwrap();

    let saved = session.rencrow_completion_state("root").await.unwrap();
    assert_eq!(saved.product_progress_at, None);
    assert_eq!(saved.activity_at, None);

    let mut changed_criteria_without_revision = saved.clone();
    changed_criteria_without_revision.conditions[0].acceptance_criteria =
        "Different criteria with the same revision".into();
    assert!(
        session
            .set_rencrow_completion_state(
                "root".into(),
                serde_json::to_value(changed_criteria_without_revision).unwrap()
            )
            .await
            .is_err()
    );

    let mut changed_criteria_under_new_revision = saved;
    changed_criteria_under_new_revision.contract_revision = 2;
    changed_criteria_under_new_revision.conditions[0].revision = 2;
    changed_criteria_under_new_revision.conditions[0].acceptance_criteria =
        "Weakened criteria under a model-created revision".into();
    assert!(
        session
            .set_rencrow_completion_state(
                "root".into(),
                serde_json::to_value(changed_criteria_under_new_revision).unwrap()
            )
            .await
            .is_err(),
        "a model revision cannot silently change acceptance criteria"
    );
}

#[tokio::test]
async fn pause_blocks_new_autonomous_child_turns() {
    let (session, _) = make_session_and_context().await;
    session
        .set_rencrow_completion_state(
            "root".into(),
            serde_json::to_value(open_contract()).unwrap(),
        )
        .await
        .unwrap();
    assert!(
        session
            .mark_autonomous_completion_turn("turn-before-pause".into())
            .await
    );
    assert!(
        session
            .mark_autonomous_completion_continued("turn-before-pause", "terminal-child-7")
            .await
    );
    session
        .set_rencrow_completion_state(
            "root".into(),
            serde_json::to_value(open_contract()).unwrap(),
        )
        .await
        .expect("model snapshot cannot clear host-consumed event keys");
    assert_eq!(
        session
            .rencrow_completion_state("root")
            .await
            .unwrap()
            .consumed_host_events,
        vec!["terminal-child-7"]
    );

    session.pause_autonomous_completion().await;

    assert!(session.autonomous_completion_paused().await);
    assert!(
        session
            .rencrow_completion_state("root")
            .await
            .is_some_and(|state| state.paused),
        "pause must be represented in durable contract state"
    );
    assert!(!session.autonomous_completion_pending().await);
    assert!(
        !session
            .mark_autonomous_completion_turn("turn-after-pause".into())
            .await
    );
    assert!(
        !session
            .mark_autonomous_completion_continued("turn-after-pause", "terminal-child-8")
            .await
    );
}

#[tokio::test]
async fn unresolved_running_unit_still_allows_terminal_child_result_reconciliation() {
    let (session, _) = make_session_and_context().await;
    let mut contract = open_contract();
    contract.units[0].status = UnitStatus::Running;
    session
        .set_rencrow_completion_state("root".into(), serde_json::to_value(contract).unwrap())
        .await
        .expect("running unit state is valid while awaiting its terminal receipt");

    assert!(
        session.autonomous_completion_pending().await,
        "an unresolved child result must be collected even if no new unit is Ready"
    );
}

#[test]
fn cold_resume_only_qualifies_after_matching_completed_turn() {
    use codex_protocol::config_types::ModeKind;
    use codex_protocol::protocol::EventMsg;
    use codex_protocol::protocol::TurnCompleteEvent;
    use codex_protocol::protocol::TurnStartedEvent;

    let start = |turn_id: &str| {
        RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: turn_id.to_string(),
            root_turn_id: None,
            trace_id: None,
            started_at: None,
            model_context_window: Some(128_000),
            collaboration_mode_kind: ModeKind::Default,
        }))
    };
    let complete = |turn_id: &str| {
        RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: turn_id.to_string(),
            last_agent_message: None,
            error: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
        }))
    };

    assert!(history_ends_with_completed_turn(&[
        start("turn-1"),
        complete("turn-1")
    ]));
    assert!(!history_ends_with_completed_turn(&[
        start("turn-1"),
        complete("turn-1"),
        start("turn-2"),
    ]));
    assert!(!history_ends_with_completed_turn(&[
        start("turn-1"),
        complete("turn-2")
    ]));
}

#[test]
fn completion_continuation_key_tracks_actions_but_ignores_bookkeeping_clocks() {
    let mut contract = open_contract();
    let initial = crate::session::session::Session::autonomous_completion_action_digest(&contract);

    contract.activity_at = Some(10);
    contract.product_progress_at = Some(11);
    contract.runtime_recovery_at = Some(12);
    contract.units_completed_since_review = 1;
    contract.plan_review_at = Some(13);
    contract
        .consumed_host_events
        .push("turn-end:old".to_string());
    let bookkeeping_only =
        crate::session::session::Session::autonomous_completion_action_digest(&contract);
    assert_eq!(initial, bookkeeping_only);

    contract.next_action = Some("Review AC-1 with a changed approach".to_string());
    let changed_action =
        crate::session::session::Session::autonomous_completion_action_digest(&contract);
    assert_ne!(initial, changed_action);
}
