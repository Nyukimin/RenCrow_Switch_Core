// Modified by RenCrow Switch Core, 2026-11-16.
use super::*;
use codex_history::task_context::AcceptanceCondition;
use codex_history::task_context::AcceptanceStatus;
use codex_history::task_context::AcceptedTaskReceipt;
use codex_history::task_context::CompletionEvidenceRef;
use codex_history::task_context::CompletionReviewAttempt;
use codex_history::task_context::CompletionReviewAttemptStatus;
use codex_history::task_context::CompletionReviewHistoricalFailure;
use codex_history::task_context::CompletionReviewRetryAuthorization;
use codex_history::task_context::CompletionReviewState;
use codex_history::task_context::CompletionReviewTarget;
use codex_history::task_context::CompletionReviewTerminalSource;
use codex_history::task_context::CompletionReviewTerminalStatus;
use codex_history::task_context::CompletionState;
use codex_history::task_context::CompletionUnit;
use codex_history::task_context::CompletionUnitKind;
use codex_history::task_context::UnitStatus;
use codex_history::task_context::project_completion_review_state;
use codex_history::task_context::validate_completion_state;
use std::collections::BTreeMap;

const TARGET_TASK_ID: &str = "task-a";
const TARGET_ARTIFACT: &str = "artifact-a";
const ATTEMPT_ID: &str = "spawn-call-a";

fn accepted_receipts() -> BTreeMap<String, AcceptedTaskReceipt> {
    BTreeMap::from([(
        TARGET_TASK_ID.to_string(),
        AcceptedTaskReceipt {
            revision: 1,
            result: "Task A was accepted".into(),
            artifact_ref: TARGET_ARTIFACT.into(),
            verification_ref: "verify-a".into(),
        },
    )])
}

fn base_unit(id: &str, status: UnitStatus) -> CompletionUnit {
    CompletionUnit {
        id: id.into(),
        purpose: format!("Review obligation {id}"),
        status,
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
        review_state: None,
        review_revalidation_of: None,
    }
}

fn attempt(
    status: CompletionReviewAttemptStatus,
    terminal: Option<CompletionReviewTerminalStatus>,
    collected: bool,
    adopted: bool,
) -> CompletionReviewAttempt {
    let has_native_wait = collected || terminal == Some(CompletionReviewTerminalStatus::Failed);
    CompletionReviewAttempt {
        attempt_id: ATTEMPT_ID.into(),
        contract_revision: 1,
        condition_refs: vec!["AC-1".into()],
        conditions_sha256: "a".repeat(64),
        status,
        launch_unknown_reason: None,
        consumed_retry_authorization: None,
        imported_from: None,
        reviewer_thread_id: Some("reviewer-thread-a".into()),
        reviewer_turn_id: Some("reviewer-turn-a".into()),
        reviewer_role: "reviewer".into(),
        reviewer_model: Some("review-model".into()),
        target_task_id: TARGET_TASK_ID.into(),
        target_task_revision: 1,
        target_artifact_ref: TARGET_ARTIFACT.into(),
        deadline_unix_ms: Some(1_800_000_000_000),
        deadline_escalation: None,
        deadline_safety_fingerprint: None,
        terminal_source: terminal.map(|status| CompletionReviewTerminalSource {
            status,
            event: None,
        }),
        wait_result_ref: has_native_wait.then(|| "wait-result-a".into()),
        wait_call_id: has_native_wait.then(|| "wait-call-a".into()),
        adoption_reason: adopted.then(|| "Parent accepted the review".into()),
    }
}

fn projected_unit(
    unit_id: &str,
    active_attempt: CompletionReviewAttempt,
    retry_authorization: Option<CompletionReviewRetryAuthorization>,
    historical_failure: Option<CompletionReviewHistoricalFailure>,
) -> CompletionUnit {
    let mut unit = base_unit(unit_id, UnitStatus::Planned);
    unit.review_state = Some(CompletionReviewState {
        version: 1,
        obligation_unit_id: unit_id.into(),
        active_attempt: Some(active_attempt),
        historical_failure,
        retry_reservation_prior_historical_failure: None,
        previous_attempt_ref: None,
        retry_authorization,
        withdrawal: None,
        revalidation: None,
    });
    project_completion_review_state(&unit).expect("fixture projects from canonical attempt state")
}

fn make_contract(units: Vec<CompletionUnit>) -> CompletionState {
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
            verification: "Check the accepted task receipt".into(),
            acceptance_criteria: "The review obligation uses the accepted revision".into(),
            status: AcceptanceStatus::Open,
            evidence: Vec::new(),
            unmet_reason: None,
        }],
        units,
        current_unit: None,
        failed_approaches: Vec::new(),
        blocker: None,
        next_action: Some("Select an available review obligation".into()),
        product_progress_at: None,
        runtime_recovery_at: None,
        activity_at: None,
    }
}

fn assert_valid(contract: &CompletionState) {
    validate_completion_state(Some(contract), &accepted_receipts())
        .expect("all test state satisfies the history validator");
}

fn historical_failure() -> CompletionReviewHistoricalFailure {
    CompletionReviewHistoricalFailure {
        attempt_id: "old-failed-attempt".into(),
        contract_revision: Some(1),
        imported_from: None,
        consumed_retry_authorization: None,
        reviewer_thread_id: "old-reviewer-thread".into(),
        reviewer_turn_id: "old-reviewer-turn".into(),
        target_task_id: TARGET_TASK_ID.into(),
        target_task_revision: 1,
        target_artifact_ref: TARGET_ARTIFACT.into(),
        reviewer_role: "reviewer".into(),
        reviewer_model: Some("review-model".into()),
        deadline_unix_ms: Some(1_800_000_000_000),
        wait_result_ref: Some("historical-failure-wait-result".into()),
        wait_call_id: Some("historical-failure-wait-call".into()),
        deadline_escalation: None,
        deadline_safety_fingerprint: None,
        terminal_source: CompletionReviewTerminalSource {
            status: CompletionReviewTerminalStatus::Failed,
            event: None,
        },
    }
}

fn retry_authorization(expected_attempt_id: &str) -> CompletionReviewRetryAuthorization {
    CompletionReviewRetryAuthorization {
        authorization_id: "plan-review-call-a".into(),
        expected_attempt_id: expected_attempt_id.into(),
        plan_review_call_id: "plan-review-call-a".into(),
        reason: "The previous reviewer failed and a safe new attempt is authorized".into(),
        previous_plan: "Review the accepted task once".into(),
        revised_plan: "Repeat the review using a newly reserved attempt".into(),
    }
}

#[test]
fn review_condition_digest_survives_receipt_refresh_but_tracks_definition_changes() {
    let contract = make_contract(Vec::new());
    let condition_refs = vec!["AC-1".to_string()];
    let original = completion_review_conditions_sha256(&contract, &condition_refs)
        .expect("original condition definition hashes");

    let mut refreshed_receipt = contract;
    let condition = &mut refreshed_receipt.conditions[0];
    condition.status = AcceptanceStatus::Satisfied;
    condition.evidence = vec![CompletionEvidenceRef {
        task_id: TARGET_TASK_ID.into(),
        task_revision: 1,
        artifact_ref: "task-check:refreshed-receipt".into(),
        verification_ref: "task-check:refreshed-receipt".into(),
    }];
    assert_eq!(
        completion_review_conditions_sha256(&refreshed_receipt, &condition_refs)
            .expect("same definition with refreshed receipt hashes"),
        original
    );

    refreshed_receipt.conditions[0].acceptance_criteria =
        "The updated review criteria require a fresh distinction".into();
    assert_ne!(
        completion_review_conditions_sha256(&refreshed_receipt, &condition_refs)
            .expect("changed condition definition hashes"),
        original
    );
}

#[test]
fn independent_obligations_for_one_target_require_an_exact_unit_selector() {
    let adopted = projected_unit(
        "review-a",
        attempt(
            CompletionReviewAttemptStatus::Completed,
            Some(CompletionReviewTerminalStatus::Completed),
            /*collected*/ true,
            /*adopted*/ true,
        ),
        /*retry_authorization*/ None,
        /*historical_failure*/ None,
    );
    let planned = base_unit("review-a-second-obligation", UnitStatus::Planned);
    let contract = make_contract(vec![adopted, planned]);
    assert_valid(&contract);

    let ambiguous = select_completion_review(
        &contract,
        TARGET_TASK_ID,
        /*target_task_revision*/ 1,
        /*unit_id*/ None,
    )
    .expect_err("one completed obligation cannot hide an independent one");
    assert!(ambiguous.contains("multiple review obligations"));

    let selected_done = select_completion_review(
        &contract,
        TARGET_TASK_ID,
        /*target_task_revision*/ 1,
        Some("review-a"),
    )
    .expect("explicitly selected completed obligation is returned idempotently");
    assert_eq!(selected_done.obligation_unit_id, "review-a");
    assert_eq!(
        selected_done.outcome,
        CompletionReviewSelectionOutcome::AlreadyAdopted
    );

    let selected_open = select_completion_review(
        &contract,
        TARGET_TASK_ID,
        /*target_task_revision*/ 1,
        Some("review-a-second-obligation"),
    )
    .expect("explicitly selected independent obligation stays launchable");
    assert_eq!(
        selected_open.obligation_unit_id,
        "review-a-second-obligation"
    );
    assert!(matches!(
        selected_open.outcome,
        CompletionReviewSelectionOutcome::NeedsReservation {
            expected_attempt_id: None,
            retry_authorization_id: None,
        }
    ));
}

#[test]
fn an_existing_collected_attempt_is_a_lookup_not_a_new_reservation() {
    let unit = projected_unit(
        "review-a",
        attempt(
            CompletionReviewAttemptStatus::Completed,
            Some(CompletionReviewTerminalStatus::Completed),
            /*collected*/ true,
            /*adopted*/ false,
        ),
        /*retry_authorization*/ None,
        /*historical_failure*/ None,
    );
    let contract = make_contract(vec![unit]);
    assert_valid(&contract);

    let selected = select_completion_review(
        &contract,
        TARGET_TASK_ID,
        /*target_task_revision*/ 1,
        Some("review-a"),
    )
    .expect("collected result can be retrieved");
    assert_eq!(selected.obligation_unit_id, "review-a");
    assert_eq!(
        selected.outcome,
        CompletionReviewSelectionOutcome::ExistingAttempt {
            attempt_id: Some(ATTEMPT_ID.into()),
        }
    );
}

#[test]
fn cancelling_an_attempt_does_not_cancel_or_complete_its_obligation() {
    let unit = projected_unit(
        "review-a",
        attempt(
            CompletionReviewAttemptStatus::Cancelled,
            Some(CompletionReviewTerminalStatus::Cancelled),
            /*collected*/ false,
            /*adopted*/ false,
        ),
        /*retry_authorization*/ None,
        /*historical_failure*/ None,
    );
    assert_eq!(unit.status, UnitStatus::Blocked);
    let contract = make_contract(vec![unit]);
    assert_valid(&contract);

    let selected = select_completion_review(
        &contract,
        TARGET_TASK_ID,
        /*target_task_revision*/ 1,
        Some("review-a"),
    )
    .expect("cancelled attempt remains an existing unresolved attempt");
    assert!(matches!(
        selected.outcome,
        CompletionReviewSelectionOutcome::ExistingAttempt {
            attempt_id: Some(_),
        }
    ));
}

#[test]
fn historical_failure_with_active_unknown_attempt_does_not_grant_retry() {
    let unit = projected_unit(
        "review-a",
        attempt(
            CompletionReviewAttemptStatus::Unknown,
            /*terminal*/ None,
            /*collected*/ false,
            /*adopted*/ false,
        ),
        /*retry_authorization*/ None,
        Some(historical_failure()),
    );
    let contract = make_contract(vec![unit]);
    assert_valid(&contract);

    let selected = select_completion_review(
        &contract,
        TARGET_TASK_ID,
        /*target_task_revision*/ 1,
        Some("review-a"),
    )
    .expect("active unknown attempt remains unresolved");
    assert_eq!(
        selected.outcome,
        CompletionReviewSelectionOutcome::ExistingAttempt {
            attempt_id: Some(ATTEMPT_ID.into()),
        }
    );
}

#[test]
fn retry_grant_authorizes_only_its_exact_failed_attempt() {
    let matching_grant = retry_authorization(ATTEMPT_ID);
    let matching = projected_unit(
        "review-a",
        attempt(
            CompletionReviewAttemptStatus::Failed,
            Some(CompletionReviewTerminalStatus::Failed),
            /*collected*/ false,
            /*adopted*/ false,
        ),
        Some(matching_grant.clone()),
        None,
    );
    let mut contract = make_contract(vec![matching]);
    contract
        .plan_reviews
        .push(codex_history::task_context::PlanReviewRecord {
            at: 1_800_000_000,
            previous_plan: matching_grant.previous_plan,
            revised_plan: matching_grant.revised_plan,
            reason: matching_grant.reason,
        });
    assert_valid(&contract);

    let selected = select_completion_review(
        &contract,
        TARGET_TASK_ID,
        /*target_task_revision*/ 1,
        Some("review-a"),
    )
    .expect("matching grant authorizes one exact failed attempt");
    assert!(matches!(
        selected.outcome,
        CompletionReviewSelectionOutcome::NeedsReservation {
            expected_attempt_id: Some(expected),
            retry_authorization_id: Some(authorization),
        } if expected == ATTEMPT_ID && authorization == "plan-review-call-a"
    ));

    let mismatched = projected_unit(
        "review-a",
        attempt(
            CompletionReviewAttemptStatus::Failed,
            Some(CompletionReviewTerminalStatus::Failed),
            /*collected*/ false,
            /*adopted*/ false,
        ),
        Some(retry_authorization("a-different-attempt")),
        None,
    );
    let mut mismatched_contract = make_contract(vec![mismatched]);
    mismatched_contract.plan_reviews = contract.plan_reviews.clone();
    assert!(validate_completion_state(Some(&mismatched_contract), &accepted_receipts()).is_err());
    assert!(
        select_completion_review(
            &mismatched_contract,
            TARGET_TASK_ID,
            /*target_task_revision*/ 1,
            Some("review-a"),
        )
        .is_err()
    );
}
