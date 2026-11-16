// Added by RenCrow Switch Core, 2026-11-16: preserve persisted reviewer failure provenance.
use crate::session::session::Session;
use crate::session::tests::accepted_task_boundary_item;
use crate::session::tests::attach_thread_persistence;
use crate::session::tests::make_session_and_context;
use codex_history::task_context::AcceptanceCondition;
use codex_history::task_context::AcceptanceStatus;
use codex_history::task_context::CompletionReviewAttempt;
use codex_history::task_context::CompletionReviewAttemptStatus;
use codex_history::task_context::CompletionReviewState;
use codex_history::task_context::CompletionReviewTarget;
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
use codex_rollout::RolloutItem;
use codex_thread_store::CreateThreadParams;
use codex_thread_store::LiveThread;
use codex_thread_store::PersistedTurnAbortLocator;
use codex_thread_store::ThreadPersistenceMetadata;
use serde_json::json;
use std::sync::Arc;

const REVIEW_UNIT_ID: &str = "review-a";
const TARGET_TASK_ID: &str = "task-a";
const REVIEWER_ROLE: &str = "reviewer";
const REVIEWER_MODEL: &str = "test-review-model";
const ATTEMPT_ID: &str = "review-spawn-attempt";
const TURN_ID: &str = "review-turn-exact";

fn review_state(reviewer_thread_id: ThreadId) -> CompletionState {
    let artifact_ref = "task-check:accepted-artifact";
    let unit = CompletionUnit {
        id: REVIEW_UNIT_ID.into(),
        purpose: "Review the accepted task artifact".into(),
        status: UnitStatus::Running,
        kind: CompletionUnitKind::Review,
        condition_refs: vec!["AC-1".into()],
        depends_on: Vec::new(),
        evidence: Vec::new(),
        task_refs: Vec::new(),
        review_target: Some(CompletionReviewTarget {
            task_id: TARGET_TASK_ID.into(),
            task_revision: 1,
        }),
        review_evidence: None,
        review_launch: None,
        review_decision_reason: None,
        review_state: Some(CompletionReviewState {
            version: 1,
            obligation_unit_id: REVIEW_UNIT_ID.into(),
            active_attempt: Some(CompletionReviewAttempt {
                attempt_id: ATTEMPT_ID.into(),
                contract_revision: 1,
                condition_refs: vec!["AC-1".into()],
                conditions_sha256: "0".repeat(64),
                status: CompletionReviewAttemptStatus::Bound,
                launch_unknown_reason: None,
                consumed_retry_authorization: None,
                imported_from: None,
                reviewer_thread_id: Some(reviewer_thread_id.to_string()),
                reviewer_turn_id: Some(TURN_ID.into()),
                reviewer_role: REVIEWER_ROLE.into(),
                reviewer_model: Some(REVIEWER_MODEL.into()),
                target_task_id: TARGET_TASK_ID.into(),
                target_task_revision: 1,
                target_artifact_ref: artifact_ref.into(),
                deadline_unix_ms: Some(chrono::Utc::now().timestamp_millis() - 1),
                deadline_escalation: None,
                deadline_safety_fingerprint: None,
                terminal_source: None,
                wait_result_ref: None,
                wait_call_id: None,
                adoption_reason: None,
            }),
            historical_failure: None,
            retry_reservation_prior_historical_failure: None,
            previous_attempt_ref: None,
            retry_authorization: None,
            withdrawal: None,
            revalidation: None,
        }),
        review_revalidation_of: None,
    };
    let unit = codex_history::task_context::project_completion_review_state(&unit)
        .expect("derive compatibility fields from the exact active attempt");
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
            result: "The accepted task artifact is correct".into(),
            scope: "Task A".into(),
            verification: "Inspect the accepted task artifact".into(),
            acceptance_criteria: "The reviewer confirms the result".into(),
            status: AcceptanceStatus::Open,
            evidence: Vec::new(),
            unmet_reason: None,
        }],
        units: vec![unit],
        current_unit: Some(REVIEW_UNIT_ID.into()),
        failed_approaches: Vec::new(),
        blocker: None,
        next_action: Some("Wait for the reviewer".into()),
        product_progress_at: None,
        runtime_recovery_at: None,
        activity_at: None,
    }
}

async fn create_persisted_reviewer_thread(
    session: &Session,
    reviewer_thread_id: ThreadId,
) -> LiveThread {
    let config = session.get_config().await;
    let source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: session.thread_id(),
        depth: 1,
        agent_path: None,
        agent_nickname: None,
        agent_role: Some(REVIEWER_ROLE.into()),
    });
    LiveThread::create(
        Arc::clone(&session.services.thread_store),
        CreateThreadParams {
            creator_user_id: None,
            creator_account_id: None,
            session_id: session.session_id(),
            thread_id: reviewer_thread_id,
            extra_config: None,
            forked_from_id: None,
            parent_thread_id: Some(session.thread_id()),
            source,
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
    .expect("create canonical reviewer thread")
}

#[tokio::test]
async fn failed_native_wait_preserves_store_backed_review_ended_provenance_through_close() {
    // The native wait receipt below is a reducer input, not a native-control E2E. The reviewer
    // identity and terminal event are produced by LocalThreadStore and its typed owner API.
    let (mut session, _) = make_session_and_context().await;
    attach_thread_persistence(&mut session).await;
    let artifact_ref = "task-check:accepted-artifact";
    assert!(
        session
            .persist_rollout_items(&[accepted_task_boundary_item(
                &session.thread_id().to_string(),
                artifact_ref,
            )])
            .await
    );
    session
        .flush_rollout()
        .await
        .expect("flush accepted task receipt");
    let reviewer_thread_id = ThreadId::new();
    let child = create_persisted_reviewer_thread(&session, reviewer_thread_id).await;
    child
        .append_items(&[RolloutItem::EventMsg(EventMsg::TurnAborted(
            TurnAbortedEvent {
                turn_id: Some(TURN_ID.into()),
                reason: TurnAbortReason::ReviewEnded,
                started_at: None,
                completed_at: None,
                duration_ms: None,
            },
        ))])
        .await
        .expect("persist exact reviewer abort event");
    let proof = child
        .read_persisted_turn_abort(TURN_ID.into())
        .await
        .expect("read canonical typed reviewer abort proof")
        .expect("exact reviewer abort event is persisted");
    assert_eq!(proof.event.turn_id.as_deref(), Some(TURN_ID));
    assert_eq!(proof.event.reason, TurnAbortReason::ReviewEnded);
    assert_eq!(proof.owner.thread_id, reviewer_thread_id);
    assert_eq!(proof.owner.parent_thread_id, Some(session.thread_id()));
    assert_eq!(proof.owner.agent_role.as_deref(), Some(REVIEWER_ROLE));
    // This fixture creates a Legacy-mode rollout with no canonical ordinal segment. Its first
    // physical line is SessionMeta, so the persisted TurnAborted event is the exact line index 1.
    assert_eq!(
        proof.source.locator,
        PersistedTurnAbortLocator::LegacyLineIndex(1)
    );

    session.state.lock().await.rencrow_completion_state =
        Some(("root".into(), review_state(reviewer_thread_id)));
    session
        .persist_autonomous_completion_snapshot()
        .await
        .expect("persist active review attempt before recording its terminal fact");
    session
        .observe_completion_child_tool(
            "spawn_agent",
            &json!({}),
            &json!({"agent_id": reviewer_thread_id.to_string()}),
        )
        .await;

    let before = session
        .rencrow_completion_state("root")
        .await
        .expect("review completion state exists")
        .units[0]
        .review_evidence
        .clone()
        .expect("exact review binding is projected");
    session
        .record_completion_review_deadline_outcome(
            REVIEW_UNIT_ID,
            &before,
            super::CompletionReviewDeadlineOutcome {
                status: Some(UnitStatus::Failed),
                escalation: Some("reviewer exceeded the host SLA".into()),
                safety_fingerprint: Some(super::review_ended_durable_fingerprint(TURN_ID)),
                failure_cause: Some("reviewer exceeded the host SLA"),
                terminal_proof: Some(proof.clone()),
            },
        )
        .await;

    let after_deadline = session
        .rencrow_completion_state("root")
        .await
        .expect("deadline result persisted");
    let original_source = after_deadline.units[0]
        .review_state
        .as_ref()
        .and_then(|review| review.active_attempt.as_ref())
        .and_then(|attempt| attempt.terminal_source.clone())
        .expect("deadline path records the typed terminal source");
    assert_eq!(
        original_source.event.as_ref().unwrap().rollout_id,
        proof.source.rollout_id.to_string()
    );
    assert_eq!(original_source.event.as_ref().unwrap().turn_id, TURN_ID);

    session
        .collect_completion_review_results(
            "wait-failed-exact-reviewer-turn",
            &[(
                reviewer_thread_id,
                Some(TURN_ID.into()),
                AgentStatus::Errored("ReviewEnded".into()),
            )],
        )
        .await;
    session
        .observe_completion_child_tool(
            "close_agent",
            &json!({"target": reviewer_thread_id.to_string()}),
            &json!({"previous_status": AgentStatus::Errored("ReviewEnded".into())}),
        )
        .await;

    let final_state = session
        .rencrow_completion_state("root")
        .await
        .expect("review completion state survives wait and close");
    let unit = &final_state.units[0];
    let attempt = unit
        .review_state
        .as_ref()
        .and_then(|review| review.active_attempt.as_ref())
        .expect("same attempt remains active for audit");
    assert_eq!(unit.status, UnitStatus::Failed);
    assert_eq!(attempt.status, CompletionReviewAttemptStatus::Failed);
    let reviewer_id = reviewer_thread_id.to_string();
    assert_eq!(
        attempt.reviewer_thread_id.as_deref(),
        Some(reviewer_id.as_str())
    );
    assert_eq!(attempt.reviewer_turn_id.as_deref(), Some(TURN_ID));
    assert_eq!(attempt.terminal_source.as_ref(), Some(&original_source));
    assert!(
        attempt.wait_result_ref.is_some(),
        "failed native wait receipt is retained"
    );
    assert_eq!(
        attempt.wait_call_id.as_deref(),
        Some("wait-failed-exact-reviewer-turn")
    );
    assert!(
        attempt.adoption_reason.is_none(),
        "failed wait is never adopted as success"
    );
    let expected_rollout_id = proof.source.rollout_id.to_string();
    let expected_parent_id = session.thread_id().to_string();
    let expected_locator = match &proof.source.locator {
        PersistedTurnAbortLocator::Ordinal(value) => {
            codex_history::task_context::CompletionReviewRecordLocator::Ordinal(*value)
        }
        PersistedTurnAbortLocator::LegacyLineIndex(value) => {
            codex_history::task_context::CompletionReviewRecordLocator::LegacyLineIndex(*value)
        }
    };
    assert_eq!(
        attempt
            .terminal_source
            .as_ref()
            .and_then(|source| source.event.as_ref())
            .map(|event| (
                event.rollout_id.as_str(),
                event.owner_thread_id.as_str(),
                event.owner_parent_thread_id.as_deref(),
                event.owner_agent_role.as_deref(),
                event.locator,
                event.turn_id.as_str(),
                event.reason.as_str(),
            )),
        Some((
            expected_rollout_id.as_str(),
            reviewer_id.as_str(),
            Some(expected_parent_id.as_str()),
            Some(REVIEWER_ROLE),
            expected_locator,
            TURN_ID,
            "ReviewEnded",
        )),
        "close observation leaves the exact owner/turn/locator proof unchanged"
    );
    assert!(final_state.blocker.is_some());
}
