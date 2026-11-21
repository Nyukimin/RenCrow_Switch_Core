// Modified by RenCrow Switch Core, 2026-11-20: separate caller and host checkpoint budgets.
use super::*;
use crate::context::AutonomousCompletionSnapshot;
use crate::context::ContextualUserFragment;
use codex_history::CodexHarnessMetadata;
use codex_history::ResponseItemEnvelope;
use codex_history::RolloutItem;
use codex_history::task_context::CompletionState;
use codex_history::task_context::PlanReviewRecord;
use codex_thread_store::LoadThreadHistoryParams;
use codex_thread_store::ThreadStore;
use pretty_assertions::assert_eq;

fn completion_snapshot_item(value: serde_json::Value) -> RolloutItem {
    let item = Box::new(AutonomousCompletionSnapshot).into_boxed_response_item();
    RolloutItem::ResponseItem(ResponseItemEnvelope {
        item,
        metadata: Some(CodexHarnessMetadata {
            rencrow_completion: Some(value),
            ..Default::default()
        }),
    })
}

#[tokio::test]
async fn host_completion_history_survives_small_update_accept_and_cold_history_read() {
    let (mut session, mut turn_context) = make_session_and_context().await;
    turn_context.session_source = SessionSource::Cli;
    Arc::make_mut(&mut turn_context.config).rencrow_compaction = true;
    Arc::make_mut(&mut turn_context.config).rencrow_autonomous_completion = true;
    let turn = Arc::new(turn_context);
    let store = attach_boundary_owner(&mut session, Arc::clone(&turn)).await;
    let session = Arc::new(session);

    let plan_reviews = (0..20)
        .map(|index| {
            let suffix = "x".repeat(3_000);
            PlanReviewRecord {
                at: i64::from(index + 1),
                previous_plan: format!("previous-{index}-{suffix}"),
                revised_plan: format!("revised-{index}-{suffix}"),
                reason: format!("evidence-{index}-{suffix}"),
            }
        })
        .collect::<Vec<_>>();
    let initial_value = json!({
        "contract_revision": 4,
        "consumed_host_events": ["host-event-a", "host-event-b"],
        "plan_review_at": 1,
        "plan_reviews": plan_reviews,
        "conditions": (1..=4).map(|index| json!({
            "id": format!("AC-{index}"), "revision": 1,
            "result": format!("Deliver condition {index}"),
            "scope": format!("Condition {index}"),
            "verification": format!("Verify condition {index}"),
            "acceptance_criteria": format!("Condition {index} passes"),
            "status": "open"
        })).collect::<Vec<_>>(),
        "units": [
            {"id":"task-a", "purpose":"Implement condition one", "status":"ready",
             "kind":"execution", "condition_refs":["AC-1"], "depends_on":[]},
            {"id":"review-old", "purpose":"Review task A", "status":"planned",
             "kind":"review", "condition_refs":["AC-1"], "depends_on":["task-a"],
             "review_target":{"task_id":"task-a", "task_revision":1}}
        ],
        "current_unit":"task-a",
        "next_action":"Implement task A"
    });
    let initial_state: CompletionState = serde_json::from_value(initial_value).unwrap();
    let original_plan_reviews = initial_state.plan_reviews.clone();
    let initial_snapshot = json!({
        "version": 1,
        "thread_id": session.thread_id().to_string(),
        "root_task_id": "root",
        "state": initial_state,
        "resume_generation": 0,
        "task_checks": [],
        "active_scope": null,
        "patch_attempts": [],
        "uncollected_children": []
    });
    session
        .restore_autonomous_completion_snapshot(&[completion_snapshot_item(initial_snapshot)])
        .await;

    let begin_a = json!({"action":"begin", "root_task_id":"root",
        "task_id":"task-a", "task_revision":1});
    record_call_with_arguments(
        &session,
        &turn,
        "begin-a",
        TASK_CONTEXT_BOUNDARY_TOOL_NAME,
        &begin_a.to_string(),
    )
    .await;
    TaskContextBoundaryHandler::new(true)
        .handle(invocation_in_session(
            Arc::clone(&session),
            Arc::clone(&turn),
            "begin-a",
            begin_a,
        ))
        .await
        .expect("begin execution with host-owned plan history");
    record_output(&session, &turn, "begin-a", TASK_CONTEXT_BOUNDARY_TOOL_NAME).await;

    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("artifact.txt"), "task A artifact").unwrap();
    record_successful_verification(&session, &turn, workspace.path(), "task-a", 1, "verify-a")
        .await;
    let accept_a = json!({"action":"accept", "result":"Task A verified",
        "satisfied_conditions":["AC-1"]});
    record_call_with_arguments(
        &session,
        &turn,
        "accept-a",
        TASK_CONTEXT_BOUNDARY_TOOL_NAME,
        &accept_a.to_string(),
    )
    .await;
    TaskContextBoundaryHandler::new(true)
        .handle(invocation_in_session(
            Arc::clone(&session),
            Arc::clone(&turn),
            "accept-a",
            accept_a,
        ))
        .await
        .expect("accept task A while retaining host history");
    record_output(&session, &turn, "accept-a", TASK_CONTEXT_BOUNDARY_TOOL_NAME).await;
    assert!(
        session
            .commit_prepared_task_boundary(&turn, &tokio_util::sync::CancellationToken::new())
            .await
            .expect("commit task A boundary")
    );

    let before_update = session.rencrow_completion_state("root").await.unwrap();
    let old_review = before_update
        .units
        .iter()
        .find(|unit| unit.id == "review-old")
        .unwrap();
    let mut update_state = serde_json::to_value(&before_update).unwrap();
    update_state.as_object_mut().unwrap().remove("plan_reviews");
    update_state["contract_revision"] = json!(before_update.contract_revision + 1);
    update_state["current_unit"] = json!("task-b");
    update_state["units"].as_array_mut().unwrap().push(json!({
        "id":"task-b", "purpose":"Implement condition two", "status":"ready",
        "kind":"execution", "condition_refs":["AC-2"], "depends_on":[]
    }));

    let mut oversized_update = update_state.clone();
    // `plan_reviews` is a recognized host-owned field that the installer would
    // otherwise restore from the canonical state. Its raw caller payload must
    // still be rejected before that merge can hide the oversized input.
    oversized_update["plan_reviews"] = json!(original_plan_reviews);
    let rejected = TaskContextBoundaryHandler::new(true)
        .handle(invocation_in_session(
            Arc::clone(&session),
            Arc::clone(&turn),
            "oversized-update",
            json!({"action":"update", "root_task_id":"root",
                "completion_state":oversized_update}),
        ))
        .await;
    let rejected = rejected
        .err()
        .expect("oversized caller host field must be rejected");
    assert!(
        rejected
            .to_string()
            .contains("completion state input exceeds caller size limit"),
        "reject at the caller input size gate, got: {rejected}"
    );
    assert_eq!(
        session.rencrow_completion_state("root").await.unwrap(),
        before_update
    );

    let update = json!({"action":"update", "root_task_id":"root",
        "completion_state":update_state});
    record_call_with_arguments(
        &session,
        &turn,
        "update-small",
        TASK_CONTEXT_BOUNDARY_TOOL_NAME,
        &update.to_string(),
    )
    .await;
    TaskContextBoundaryHandler::new(true)
        .handle(invocation_in_session(
            Arc::clone(&session),
            Arc::clone(&turn),
            "update-small",
            update,
        ))
        .await
        .expect("small caller update merges retained host history");
    record_output(
        &session,
        &turn,
        "update-small",
        TASK_CONTEXT_BOUNDARY_TOOL_NAME,
    )
    .await;
    let after_update = session.rencrow_completion_state("root").await.unwrap();
    let retained_review = after_update
        .units
        .iter()
        .find(|unit| unit.id == "review-old")
        .unwrap();
    assert_eq!(retained_review, old_review);
    assert_eq!(after_update.conditions, before_update.conditions);
    assert_eq!(after_update.plan_reviews, original_plan_reviews);
    assert_eq!(
        after_update.consumed_host_events,
        before_update.consumed_host_events
    );

    let begin_b = json!({"action":"begin", "root_task_id":"root",
        "task_id":"task-b", "task_revision":1});
    record_call_with_arguments(
        &session,
        &turn,
        "begin-b",
        TASK_CONTEXT_BOUNDARY_TOOL_NAME,
        &begin_b.to_string(),
    )
    .await;
    TaskContextBoundaryHandler::new(true)
        .handle(invocation_in_session(
            Arc::clone(&session),
            Arc::clone(&turn),
            "begin-b",
            begin_b,
        ))
        .await
        .expect("begin added execution unit");
    record_output(&session, &turn, "begin-b", TASK_CONTEXT_BOUNDARY_TOOL_NAME).await;
    let workspace_b = tempfile::tempdir().expect("task B workspace");
    std::fs::write(workspace_b.path().join("artifact.txt"), "task B artifact").unwrap();
    record_successful_verification(&session, &turn, workspace_b.path(), "task-b", 1, "verify-b")
        .await;
    let accept_b = json!({"action":"accept", "result":"Task B verified",
        "satisfied_conditions":["AC-2"]});
    record_call_with_arguments(
        &session,
        &turn,
        "accept-b",
        TASK_CONTEXT_BOUNDARY_TOOL_NAME,
        &accept_b.to_string(),
    )
    .await;
    TaskContextBoundaryHandler::new(true)
        .handle(invocation_in_session(
            Arc::clone(&session),
            Arc::clone(&turn),
            "accept-b",
            accept_b,
        ))
        .await
        .expect("accept added execution unit");
    record_output(&session, &turn, "accept-b", TASK_CONTEXT_BOUNDARY_TOOL_NAME).await;
    assert!(
        session
            .commit_prepared_task_boundary(&turn, &tokio_util::sync::CancellationToken::new())
            .await
            .expect("commit task B boundary")
    );
    let expected_completion = session.rencrow_completion_state("root").await.unwrap();

    let persisted = ThreadStore::load_history(
        store.as_ref(),
        LoadThreadHistoryParams {
            thread_id: session.thread_id(),
            include_archived: false,
        },
    )
    .await
    .expect("load persisted thread history");
    let replacement = persisted
        .items
        .iter()
        .rev()
        .find_map(|item| match item {
            codex_history::RolloutItem::Compacted(compacted) => {
                compacted.replacement_history.clone()
            }
            _ => None,
        })
        .expect("accepted completion checkpoint has replacement history");
    let restored_task_state = crate::compact::latest_task_state_in_history(
        &replacement,
        &session.thread_id().to_string(),
    )
    .expect("parse cold task checkpoint")
    .expect("task checkpoint restored");
    let restored_completion = restored_task_state.boundary.completion_state.unwrap();
    assert_eq!(
        restored_completion.conditions,
        expected_completion.conditions
    );
    assert_eq!(restored_completion.plan_reviews, original_plan_reviews);
    assert!(restored_task_state.completed.contains_key("task-a"));
    assert!(restored_task_state.completed.contains_key("task-b"));
    assert_eq!(
        restored_completion
            .units
            .iter()
            .find(|unit| unit.id == "review-old"),
        Some(old_review)
    );
    let (mut cold_session, _) = make_session_and_context().await;
    cold_session.thread_id = session.thread_id();
    assert_eq!(cold_session.thread_id(), session.thread_id());
    assert_eq!(cold_session.rencrow_completion_state("root").await, None);
    cold_session
        .restore_autonomous_completion_snapshot(&persisted.items)
        .await;
    let cold_completion = cold_session.rencrow_completion_state("root").await.unwrap();
    assert!(!cold_completion.paused);
    assert!(!cold_session.autonomous_completion_paused().await);
    assert_eq!(cold_completion.conditions, expected_completion.conditions);
    assert_eq!(cold_completion.plan_reviews, original_plan_reviews);
    assert_eq!(
        cold_completion.consumed_host_events,
        expected_completion.consumed_host_events
    );
    assert_eq!(
        cold_completion
            .units
            .iter()
            .find(|unit| unit.id == "review-old"),
        expected_completion
            .units
            .iter()
            .find(|unit| unit.id == "review-old")
    );
}
