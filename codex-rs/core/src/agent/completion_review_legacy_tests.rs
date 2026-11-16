// Added by RenCrow Switch Core, 2026-11-16: v1 failed-review migration regression.
use super::*;
use crate::agent::CompletionReviewAbort;
use crate::session::CompletionReviewRetryRequest;
use codex_history::task_context::CompletionReviewAttemptStatus;
use codex_history::task_context::CompletionReviewRecordLocator;
use codex_history::task_context::CompletionReviewTerminalStatus;
use codex_history::task_context::UnitStatus;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::TurnAbortReason;
use codex_thread_store::PersistedTurnAbortLocator;
use pretty_assertions::assert_eq;
use std::time::Duration;
use wiremock::Mock;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

const LEGACY_ROOT_TASK_ID: &str = "root";
const LEGACY_REVIEW_UNIT_ID: &str = "review-unit-legacy";
const LEGACY_TASK_ID: &str = "task-a";
const REVIEWER_ROLE: &str = "completion_reviewer";
const REVIEWER_MODEL: &str = "gpt-5.4";
const LEGACY_SPAWN_CALL_ID: &str = "legacy-review-spawn-call";
const PLAN_REVIEW_CALL_ID: &str = "authorize-legacy-review-retry";
const PREVIOUS_PLAN: &str = "Wait for the reviewer";
const REVISED_PLAN: &str = "Retry the failed review once with the same accepted artifact";
const RETRY_REASON: &str =
    "The first read-only reviewer generation ended before returning a result";

#[tokio::test]
async fn v1_failed_review_requires_typed_import_then_reserves_one_explicit_retry() {
    let server = start_mock_server().await;
    let (request_tx, mut request_rx) = tokio::sync::mpsc::unbounded_channel::<serde_json::Value>();
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(move |request: &wiremock::Request| {
            let body = request
                .body_json::<serde_json::Value>()
                .expect("Responses request should be JSON");
            request_tx
                .send(body)
                .expect("test keeps request receiver open");
            ResponseTemplate::new(200).set_delay(Duration::from_secs(60))
        })
        .expect(1)
        .mount(&server)
        .await;

    let (home, mut config) = test_config().await;
    config.model_provider.base_url = Some(format!("{}/v1", server.uri()));
    config.model_provider.supports_websockets = false;
    config.model = Some(REVIEWER_MODEL.into());
    config.rencrow_execution_guard = Some(codex_config::RencrowExecutionGuard {
        task_kind: Some(codex_config::RencrowTaskKind::ReadOnly),
        ..Default::default()
    });
    config.agent_roles.insert(
        REVIEWER_ROLE.into(),
        AgentRoleConfig {
            enabled: Some(true),
            description: Some("Read-only completion reviewer".into()),
            ..Default::default()
        },
    );
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let (parent_thread_id, parent_thread) = harness.start_thread().await;
    // The thread manager gives each started thread its own LocalAgentControl registry. Use the
    // parent Session's registry for spawn, abort, metadata checks, and cleanup so the retry
    // importer observes the exact child identity registered by this fixture.
    let control = parent_thread.session.services.agent_control.clone();
    let (_accepted_workspace, target_artifact_ref) =
        record_accepted_review_target(&parent_thread.session).await;

    let reviewer_thread_id = control
        .spawn_agent_with_metadata(
            harness.config.clone(),
            text_input("Review the accepted artifact. Stay read-only."),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: Some(
                    AgentPath::root()
                        .join(REVIEWER_ROLE)
                        .expect("reviewer role path is valid"),
                ),
                agent_nickname: None,
                agent_role: Some(REVIEWER_ROLE.into()),
            })),
            SpawnAgentOptions {
                parent_thread_id: Some(parent_thread_id),
                ..Default::default()
            },
        )
        .await
        .expect("registered reviewer child starts")
        .thread_id;
    let reviewer_thread = harness
        .manager
        .get_thread(reviewer_thread_id)
        .await
        .expect("reviewer child stays registered");
    assert_eq!(
        control
            .get_agent_metadata(reviewer_thread_id)
            .and_then(|metadata| metadata.agent_role),
        Some(REVIEWER_ROLE.into())
    );

    let request = request_rx
        .recv()
        .await
        .expect("reviewer request reaches mock model");
    let metadata = request["client_metadata"]
        .as_object()
        .expect("model request has client metadata");
    let reviewer_thread_id_text = reviewer_thread_id.to_string();
    assert_eq!(
        metadata["thread_id"].as_str(),
        Some(reviewer_thread_id_text.as_str())
    );
    let reviewer_turn_id = metadata["turn_id"]
        .as_str()
        .expect("model request identifies its exact generation")
        .to_string();
    let (observed_turn_id, running_status) =
        reviewer_thread.session.agent_status_generation().await;
    assert_eq!(observed_turn_id.as_deref(), Some(reviewer_turn_id.as_str()));
    assert!(!crate::agent::status::is_final(&running_status));

    // Stop that exact model-only generation through the real host control path. Its returned
    // typed evidence, rather than a hand-built status, supplies the legacy failure proof.
    let abort = control
        .abort_completion_review_turn_if_model_only(
            parent_thread_id,
            reviewer_thread_id,
            &reviewer_turn_id,
            REVIEWER_ROLE,
            /*previous_safety_fingerprint*/ None,
        )
        .await;
    let proof = match abort {
        CompletionReviewAbort::AbortedAndObserved(proof) => proof,
        CompletionReviewAbort::AlreadyTerminal => panic!("reviewer must be running before abort"),
        CompletionReviewAbort::Unsafe { reason, .. } => {
            panic!("exact reviewer abort was unsafe: {reason}")
        }
    };
    let (terminal_turn_id, terminal_status) =
        reviewer_thread.session.agent_status_generation().await;
    assert_eq!(terminal_turn_id.as_deref(), Some(reviewer_turn_id.as_str()));
    assert!(matches!(terminal_status, AgentStatus::Errored(ref reason) if reason == "ReviewEnded"));
    assert_eq!(
        proof.event.turn_id.as_deref(),
        Some(reviewer_turn_id.as_str())
    );
    assert_eq!(proof.event.reason, TurnAbortReason::ReviewEnded);
    assert_eq!(proof.owner.thread_id, reviewer_thread_id);
    assert_eq!(proof.owner.parent_thread_id, Some(parent_thread_id));
    assert_eq!(proof.owner.agent_role.as_deref(), Some(REVIEWER_ROLE));

    let legacy_contract: codex_history::task_context::CompletionState =
        serde_json::from_value(serde_json::json!({
            "contract_revision": 1,
            "conditions": [{
                "id": "AC-1",
                "revision": 1,
                "result": "The accepted artifact is correct",
                "scope": "Task A",
                "verification": "Inspect the accepted artifact",
                "acceptance_criteria": "The reviewer confirms the result",
                "status": "open",
                "evidence": [],
                "unmet_reason": null
            }],
            "units": [{
                "id": LEGACY_REVIEW_UNIT_ID,
                "purpose": "Review the accepted task artifact",
                "status": "failed",
                "kind": "review",
                "condition_refs": ["AC-1"],
                "depends_on": [],
                "evidence": [],
                "task_refs": [],
                "review_target": {"task_id": LEGACY_TASK_ID, "task_revision": 1},
                "review_evidence": {
                    "reviewer_thread_id": reviewer_thread_id.to_string(),
                    "reviewer_turn_id": reviewer_turn_id,
                    "reviewer_role": REVIEWER_ROLE,
                    "spawn_call_id": LEGACY_SPAWN_CALL_ID,
                    "deadline_unix_ms": null,
                    "deadline_escalation": null,
                    "deadline_safety_fingerprint": null,
                    "wait_result_ref": null,
                    "terminal_status": "failed",
                    "wait_call_id": null,
                    "target_task_id": LEGACY_TASK_ID,
                    "target_task_revision": 1,
                    "target_artifact_ref": target_artifact_ref
                },
                "review_launch": null,
                "review_decision_reason": null,
                "review_state": null
            }],
            "current_unit": LEGACY_REVIEW_UNIT_ID,
            "failed_approaches": [],
            "blocker": null,
            "next_action": PREVIOUS_PLAN,
            "paused": false,
            "consumed_host_events": [],
            "units_completed_since_review": 0,
            "plan_review_at": null,
            "plan_reviews": [],
            "product_progress_at": null,
            "runtime_recovery_at": null,
            "activity_at": null
        }))
        .expect("legacy v1 completion state deserializes");
    crate::session::tests::install_host_completion_fixture(
        &parent_thread.session,
        LEGACY_ROOT_TASK_ID,
        legacy_contract,
    )
    .await
    .expect("persist the legacy v1 host completion snapshot");

    let retry = CompletionReviewRetryRequest {
        unit_id: LEGACY_REVIEW_UNIT_ID.into(),
        expected_attempt_id: LEGACY_SPAWN_CALL_ID.into(),
        invocation_call_id: PLAN_REVIEW_CALL_ID.into(),
        previous_plan: PREVIOUS_PLAN.into(),
        revised_plan: REVISED_PLAN.into(),
        reason: RETRY_REASON.into(),
    };
    assert!(matches!(
        &reviewer_thread.session_source,
        codex_protocol::protocol::SessionSource::SubAgent(
            codex_protocol::protocol::SubAgentSource::ThreadSpawn {
                parent_thread_id: source_parent,
                agent_role: Some(source_role),
                ..
            }
        ) if *source_parent == parent_thread_id && source_role == REVIEWER_ROLE
    ));
    assert_eq!(
        control
            .get_agent_metadata(reviewer_thread_id)
            .and_then(|metadata| metadata.agent_role)
            .as_deref(),
        Some(REVIEWER_ROLE)
    );
    let reviewer_config = reviewer_thread.config().await;
    let reviewer_guard = reviewer_config
        .rencrow_execution_guard
        .as_ref()
        .expect("native reviewer child keeps its read-only execution guard");
    assert!(
        !reviewer_guard.requires_edit(),
        "legacy retry fixture requires a real read-only child guard"
    );
    parent_thread
        .session
        .record_autonomous_plan_review(
            LEGACY_ROOT_TASK_ID,
            PLAN_REVIEW_CALL_ID,
            PREVIOUS_PLAN.into(),
            REVISED_PLAN.into(),
            RETRY_REASON.into(),
            Some(retry),
        )
        .await
        .expect("explicit plan review imports only the exact failed v1 attempt");

    let imported = parent_thread
        .session
        .rencrow_completion_state(LEGACY_ROOT_TASK_ID)
        .await
        .expect("imported failed attempt is available");
    let review = imported.units[0]
        .review_state
        .as_ref()
        .expect("legacy failure is represented canonically after explicit authorization");
    let attempt = review
        .active_attempt
        .as_ref()
        .expect("imported attempt retained");
    assert_eq!(imported.units[0].status, UnitStatus::Failed);
    assert_eq!(attempt.attempt_id, LEGACY_SPAWN_CALL_ID);
    assert_eq!(attempt.status, CompletionReviewAttemptStatus::Failed);
    assert_eq!(
        attempt.reviewer_model, None,
        "legacy snapshots do not inherit current config"
    );
    assert_eq!(
        attempt
            .imported_from
            .as_ref()
            .map(|reference| reference.version),
        Some(1)
    );
    let imported_snapshot_ref = attempt
        .imported_from
        .clone()
        .expect("v1 source snapshot is recorded");
    assert_eq!(
        review.previous_attempt_ref.as_ref(),
        Some(&imported_snapshot_ref)
    );
    assert_eq!(
        review
            .retry_authorization
            .as_ref()
            .map(|grant| grant.authorization_id.as_str()),
        Some(PLAN_REVIEW_CALL_ID)
    );
    assert_eq!(
        attempt.terminal_source.as_ref().map(|source| source.status),
        Some(CompletionReviewTerminalStatus::Failed)
    );
    let terminal_event = attempt
        .terminal_source
        .as_ref()
        .and_then(|source| source.event.as_ref())
        .expect("typed terminal source is imported from durable reviewer history");
    assert_eq!(terminal_event.turn_id, reviewer_turn_id);
    assert_eq!(terminal_event.owner_thread_id, reviewer_thread_id_text);
    let parent_thread_id_text = parent_thread_id.to_string();
    assert_eq!(
        terminal_event.owner_parent_thread_id.as_deref(),
        Some(parent_thread_id_text.as_str())
    );
    assert_eq!(
        terminal_event.owner_agent_role.as_deref(),
        Some(REVIEWER_ROLE)
    );
    assert_eq!(terminal_event.reason, "ReviewEnded");
    assert_eq!(
        terminal_event.locator,
        match proof.source.locator {
            PersistedTurnAbortLocator::Ordinal(value) =>
                CompletionReviewRecordLocator::Ordinal(value),
            PersistedTurnAbortLocator::LegacyLineIndex(value) =>
                CompletionReviewRecordLocator::LegacyLineIndex(value),
        }
    );
    let grant_bearing_snapshot_ref = parent_thread
        .session
        .persisted_review_attempt_snapshot_ref(LEGACY_ROOT_TASK_ID, LEGACY_REVIEW_UNIT_ID, attempt)
        .await
        .expect("grant-bearing failed attempt has a durable snapshot receipt")
        .expect("snapshot reader finds the exact failed attempt in the persisted snapshot");

    let launch = parent_thread
        .session
        .prepare_completion_review_launch(
            LEGACY_TASK_ID,
            Some(LEGACY_REVIEW_UNIT_ID),
            "reserve-new-review-generation",
            Some(REVIEWER_ROLE),
            harness.config.model.as_deref(),
        )
        .await
        .expect("typed v1 failure authorizes exactly one explicit retry reservation");
    assert!(launch.should_spawn);
    assert_eq!(launch.target_artifact_ref, target_artifact_ref);
    let reserved = parent_thread
        .session
        .rencrow_completion_state(LEGACY_ROOT_TASK_ID)
        .await
        .expect("new retry reservation is durable");
    let review = reserved.units[0]
        .review_state
        .as_ref()
        .expect("canonical review state remains");
    let attempt = review
        .active_attempt
        .as_ref()
        .expect("new retry reservation is active");
    assert_eq!(attempt.attempt_id, "reserve-new-review-generation");
    assert_eq!(attempt.status, CompletionReviewAttemptStatus::Reserved);
    assert_eq!(
        attempt.reviewer_model.as_deref(),
        harness.config.model.as_deref()
    );
    assert!(attempt.imported_from.is_none());
    assert_eq!(
        review.previous_attempt_ref.as_ref(),
        Some(&grant_bearing_snapshot_ref),
        "the retry reservation points to the exact immediately preceding grant-bearing snapshot"
    );
    assert_ne!(
        review.previous_attempt_ref.as_ref(),
        Some(&imported_snapshot_ref),
        "the original v1 import remains on the historical attempt, not as the retry predecessor"
    );
    assert!(
        review.retry_authorization.is_none(),
        "authorization is consumed once into reservation"
    );

    control
        .shutdown_live_agent(reviewer_thread_id)
        .await
        .expect("clean up terminal reviewer thread");
    control
        .shutdown_agent_tree(parent_thread_id)
        .await
        .expect("clean up parent thread");
}
