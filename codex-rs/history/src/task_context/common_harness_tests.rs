use super::*;

fn request(max_attempts: u32) -> TaskRequestV1 {
    TaskRequestV1 {
        task_id: "task-root".into(),
        request_revision: 1,
        authorized_request_ref: "request:sha256:abc".into(),
        goal: "Fix the specified regression".into(),
        workspace_ref: "workspace:root".into(),
        start_target_version: "target-1".into(),
        permission_snapshot_ref: "permission:sha256:def".into(),
        constraints: vec!["Keep changes in scope".into()],
        conditions: vec![ConditionSpecV1 {
            condition_id: "AC-1".into(),
            condition_revision: 1,
            kind: ConditionKindV1::TestSuite,
            comparison_scope: "named package and cases".into(),
            expected: "all selected tests pass".into(),
            required: true,
            verifier_ref: Some("verifier:go-test-json".into()),
            verifier_config_revision: Some(1),
            check: Some(CheckSpecV1 {
                argv: vec![
                    "go".into(),
                    "test".into(),
                    "-json".into(),
                    "-count=1".into(),
                    "-run".into(),
                    "^TestRegression$".into(),
                    "./pkg".into(),
                ],
                parser: CheckOutputParserV1::GoTestJson,
                expected_outcome: ExpectedCheckOutcomeV1::AllSelectedTestsPass,
                go_test_selection: Some(GoTestSelectionV1 {
                    selected_cases: vec!["TestRegression".into()],
                    expected_case_count: 1,
                }),
                cwd_binding_refs: vec!["workspace:root/pkg".into()],
                allowed_environment_refs: vec![],
                toolchain_ref: Some("toolchain:go".into()),
                timeout_ms: 30_000,
            }),
            dependency_refs: vec!["pkg/regression_test.go".into(), "pkg/source.go".into()],
        }],
        condition_set_revision: 1,
        max_attempts,
        operation_limits_ref: "operation-limits:sha256:ghi".into(),
        skill_refs: vec![],
        execution_owner: ExecutionOwnerV1::StandaloneSwitch,
    }
}

fn queued(max_attempts: u32) -> CommonHarnessStateV1 {
    CommonHarnessStateV1::queued(request(max_attempts), "owner-1".into(), 1).unwrap()
}

fn apply(
    state: &CommonHarnessStateV1,
    event: HostEventV1,
) -> Result<CommonHarnessStateV1, TransitionErrorV1> {
    state.apply_host_event(
        StateFenceV1 {
            request_revision: state.request.request_revision,
            expected_state_version: state.run_state.state_version,
            owner_epoch: state.run_state.owner_epoch,
        },
        event,
    )
}

fn start(state: &CommonHarnessStateV1) -> CommonHarnessStateV1 {
    apply(
        state,
        HostEventV1::StartAttempt {
            target_version: state.run_state.target_version.clone(),
        },
    )
    .unwrap()
}

fn work_window(attempt_no: u32) -> WorkWindowV1 {
    WorkWindowV1 {
        window_id: format!("window:{attempt_no}:1"),
        attempt_no,
        started_at_ms: 1_000,
        deadline_at_ms: 601_000,
        timeout_ms: 600_000,
        policy_ref: "common-harness/s1-parent-window/1".into(),
    }
}

fn running_reproduce_failure() -> CommonHarnessStateV1 {
    let mut task_request = request(3);
    task_request.conditions[0].kind = ConditionKindV1::ReproduceFailure;
    task_request.conditions[0].expected =
        "TestRegression fails on baseline and passes after the fix".into();
    let queued = CommonHarnessStateV1::queued(task_request, "owner-1".into(), 1).unwrap();
    start(&queued)
}

fn initial_claim(intent: &OperationIntentV1, claim_id: &str) -> OperationClaimV1 {
    OperationClaimV1 {
        claim_id: claim_id.into(),
        operation_id: intent.operation_id.clone(),
        previous_claim_id: None,
        owner_epoch: intent.owner_epoch,
        expected_state_version: intent.expected_state_version,
        intent_digest: intent.content_digest().unwrap(),
        claimed_at_ms: 999_000,
        deadline_at_ms: 1_000_000,
        kind: OperationClaimKindV1::Execution,
        supersedes_execution_claim_id: None,
        native_executor_dead_proof_ref: None,
    }
}

#[test]
fn initial_claim_binds_all_intent_fields_and_bounded_absolute_deadline() {
    let state = start(&queued(3));
    let intent = OperationIntentV1 {
        operation_id: "claim-contract".into(),
        task_id: state.request.task_id.clone(),
        request_revision: state.request.request_revision,
        attempt_no: state.run_state.attempt_no,
        owner_epoch: state.run_state.owner_epoch,
        expected_state_version: state.run_state.state_version,
        base_target_version: state.run_state.target_version.clone(),
        operation_kind: OperationKindV1::ApplyPatch,
        normalized_arguments_ref: "args:claim-contract".into(),
        normalized_arguments_digest: "sha256:args".into(),
        permission_decision_ref: "permission:allow".into(),
        approval_ref: None,
        executor_ref: "executor:patch".into(),
        timeout_ms: 30_000,
        status: OperationIntentStatusV1::Reserved,
    };
    let claim = initial_claim(&intent, "claim-contract-1");
    let mut changed_intent = intent.clone();
    changed_intent.permission_decision_ref = "permission:ask".into();
    assert_eq!(
        apply(
            &state,
            HostEventV1::ReserveOperation {
                intent: changed_intent,
                claim: claim.clone(),
            }
        ),
        Err(TransitionErrorV1::InvalidOperation("intent_digest"))
    );

    let mut extended_deadline = claim;
    extended_deadline.deadline_at_ms = 1_030_001;
    assert_eq!(
        apply(
            &state,
            HostEventV1::ReserveOperation {
                intent,
                claim: extended_deadline,
            }
        ),
        Err(TransitionErrorV1::InvalidOperation("claim_deadline"))
    );
}

fn record_operation(
    state: &CommonHarnessStateV1,
    operation_id: &str,
    outcome: OperationOutcomeV1,
) -> CommonHarnessStateV1 {
    let intent = OperationIntentV1 {
        operation_id: operation_id.into(),
        task_id: state.request.task_id.clone(),
        request_revision: state.request.request_revision,
        attempt_no: state.run_state.attempt_no,
        owner_epoch: state.run_state.owner_epoch,
        expected_state_version: state.run_state.state_version,
        base_target_version: state.run_state.target_version.clone(),
        operation_kind: OperationKindV1::RunVerifier,
        normalized_arguments_ref: format!("args:{operation_id}"),
        normalized_arguments_digest: format!("sha256:{operation_id}"),
        permission_decision_ref: "permission:allow".into(),
        approval_ref: None,
        executor_ref: "executor:go".into(),
        timeout_ms: 30_000,
        status: OperationIntentStatusV1::Reserved,
    };
    let reserved = apply(
        state,
        HostEventV1::ReserveOperation {
            intent: intent.clone(),
            claim: initial_claim(&intent, &format!("claim-{operation_id}")),
        },
    )
    .unwrap();
    let target_version = intent.base_target_version.clone();
    apply(
        &reserved,
        HostEventV1::FinalizeOperation {
            receipt: OperationReceiptV1 {
                operation_id: intent.operation_id,
                task_id: intent.task_id,
                request_revision: intent.request_revision,
                attempt_no: intent.attempt_no,
                owner_epoch: intent.owner_epoch,
                execution_id: format!("claim-{operation_id}"),
                outcome,
                actual_before_version: target_version.clone(),
                actual_after_version: Some(target_version),
                exit_status: Some(if outcome == OperationOutcomeV1::Failed {
                    1
                } else {
                    0
                }),
                result_ref: Some(format!("receipt:{operation_id}")),
                external_receipt_ref: None,
                reconciliation_ref: None,
            },
        },
    )
    .unwrap()
}

fn verifying(state: &CommonHarnessStateV1) -> CommonHarnessStateV1 {
    apply(
        state,
        HostEventV1::BeginVerification {
            target_version: state.run_state.target_version.clone(),
        },
    )
    .unwrap()
}

fn evidence(
    state: &CommonHarnessStateV1,
    evidence_id: &str,
    operation_id: &str,
    verdict: ConditionVerdictV1,
) -> ConditionEvidenceV1 {
    ConditionEvidenceV1 {
        evidence_id: evidence_id.into(),
        condition_id: "AC-1".into(),
        condition_revision: 1,
        condition_set_revision: 1,
        request_revision: 1,
        target_version: state.run_state.target_version.clone(),
        comparison_scope_digest: "sha256:scope".into(),
        execution_id: format!("claim-{operation_id}"),
        operation_id: operation_id.into(),
        verifier_ref: "verifier:go-test-json".into(),
        verifier_config_revision: 1,
        observed: format!("observed:{verdict:?}"),
        verdict,
        log_refs: vec![format!("log:{operation_id}")],
        artifact_refs: vec![],
        source_evidence_ref: None,
        comparison_proof_ref: None,
        host_negative_proof: None,
        lifecycle: EvidenceLifecycleV1::Current,
    }
}

fn verifier_error_evidence(
    state: &CommonHarnessStateV1,
    evidence_id: &str,
    operation_id: &str,
) -> ConditionEvidenceV1 {
    let mut error = evidence(state, evidence_id, operation_id, ConditionVerdictV1::Error);
    let receipt = state
        .operation_refs
        .iter()
        .find(|record| record.intent.operation_id == operation_id)
        .and_then(|record| record.receipt.as_ref())
        .expect("verifier error requires its raw operation receipt");
    error.log_refs = vec![
        receipt
            .result_ref
            .clone()
            .expect("verifier error receipt retains the raw output artifact"),
    ];
    error
}

fn negative_evidence(
    state: &CommonHarnessStateV1,
    evidence_id: &str,
    operation_id: &str,
    verdict: ConditionVerdictV1,
    baseline_operation_id: &str,
    resolution_operation_id: Option<&str>,
) -> ConditionEvidenceV1 {
    let mut evidence = evidence(state, evidence_id, operation_id, verdict);
    evidence.host_negative_proof = Some(HostNegativeProofV1 {
        baseline_receipt_ref: baseline_operation_id.into(),
        expected_failure_identity: "TestRegression: assertion mismatch".into(),
        observed_failure_identity: "TestRegression: assertion mismatch".into(),
        resolution_receipt_ref: resolution_operation_id.map(str::to_owned),
    });
    evidence
}

fn prepared_evidence_rebind() -> (CommonHarnessStateV1, ConditionEvidenceV1) {
    let mut task_request = request(3);
    let mut other_condition = task_request.conditions[0].clone();
    other_condition.condition_id = "AC-2".into();
    task_request.conditions.push(other_condition);
    let queued = CommonHarnessStateV1::queued(task_request, "owner-1".into(), 1).unwrap();
    let state = start(&queued);
    let state = record_operation(
        &state,
        "verify-before-rebind",
        OperationOutcomeV1::Succeeded,
    );
    let state = verifying(&state);
    let original = evidence(
        &state,
        "evidence-before-rebind",
        "verify-before-rebind",
        ConditionVerdictV1::Pass,
    );
    let state = apply(&state, HostEventV1::RecordEvidence { evidence: original }).unwrap();
    let state = apply(
        &state,
        HostEventV1::InvalidateEvidence {
            evidence_ids: vec!["evidence-before-rebind".into()],
            new_target_version: "target-2".into(),
        },
    )
    .unwrap();
    let mut rebound = evidence(
        &state,
        "evidence-after-rebind",
        "verify-before-rebind",
        ConditionVerdictV1::Pass,
    );
    rebound.source_evidence_ref = Some("evidence-before-rebind".into());
    rebound.comparison_proof_ref = Some("host-comparison:dependencies-unchanged".into());
    (state, rebound)
}

#[test]
fn rebind_evidence_preserves_old_receipt_and_requires_host_comparison() {
    let (state, mut rebound) = prepared_evidence_rebind();
    let operation_count = state.operation_refs.len();
    let old_receipt = state
        .operation_refs
        .iter()
        .find(|record| record.intent.operation_id == "verify-before-rebind")
        .unwrap()
        .receipt
        .clone();
    assert_eq!(
        old_receipt.as_ref().unwrap().actual_before_version,
        "target-1"
    );
    let rebound_state = apply(
        &state,
        HostEventV1::RebindEvidence {
            evidence: rebound.clone(),
        },
    )
    .expect("same condition can be rebound with host comparison proof");
    assert_eq!(
        rebound_state
            .operation_refs
            .iter()
            .find(|record| record.intent.operation_id == "verify-before-rebind")
            .unwrap()
            .receipt,
        old_receipt,
        "the original old-target receipt remains immutable"
    );
    assert_eq!(rebound_state.operation_refs.len(), operation_count);
    let mut expected_rebound = state.evidence_refs[0].clone();
    expected_rebound.evidence_id = "evidence-after-rebind".into();
    expected_rebound.target_version = "target-2".into();
    expected_rebound.source_evidence_ref = Some("evidence-before-rebind".into());
    expected_rebound.comparison_proof_ref = Some("host-comparison:dependencies-unchanged".into());
    expected_rebound.lifecycle = EvidenceLifecycleV1::Current;
    assert_eq!(rebound_state.evidence_refs[1], expected_rebound);
    assert_eq!(
        rebound_state.evidence_refs[0].lifecycle,
        EvidenceLifecycleV1::Stale
    );
    assert_eq!(
        rebound_state.evidence_refs[1]
            .source_evidence_ref
            .as_deref(),
        Some("evidence-before-rebind")
    );
    assert_eq!(rebound_state.evidence_refs[1].condition_id, "AC-1");
    assert_eq!(rebound_state.evidence_refs[1].condition_revision, 1);
    assert_eq!(rebound_state.evidence_refs[1].target_version, "target-2");
    assert_eq!(
        rebound_state.evidence_refs[1].operation_id,
        "verify-before-rebind"
    );
    assert_eq!(
        rebound_state.evidence_refs[1].execution_id,
        "claim-verify-before-rebind"
    );
    assert_eq!(
        rebound_state.evidence_refs[1].comparison_scope_digest,
        rebound_state.evidence_refs[0].comparison_scope_digest
    );
    assert_eq!(
        rebound_state.evidence_refs[1]
            .comparison_proof_ref
            .as_deref(),
        Some("host-comparison:dependencies-unchanged")
    );
    let mut tampered_snapshot = rebound_state;
    tampered_snapshot.evidence_refs[1].verifier_config_revision += 1;
    assert!(tampered_snapshot.validate(None).is_err());

    rebound.comparison_proof_ref = None;
    assert!(
        apply(
            &state,
            HostEventV1::RebindEvidence {
                evidence: rebound.clone(),
            }
        )
        .is_err()
    );
    rebound.comparison_proof_ref = Some(" ".into());
    assert!(apply(&state, HostEventV1::RebindEvidence { evidence: rebound }).is_err());
}

#[test]
fn rebind_evidence_can_follow_prior_rebind_without_new_operation() {
    let (state, rebound) = prepared_evidence_rebind();
    let once = apply(&state, HostEventV1::RebindEvidence { evidence: rebound }).unwrap();
    let twice_target = apply(
        &once,
        HostEventV1::InvalidateEvidence {
            evidence_ids: vec!["evidence-after-rebind".into()],
            new_target_version: "target-3".into(),
        },
    )
    .unwrap();
    let mut rebound_again = evidence(
        &twice_target,
        "evidence-after-second-rebind",
        "verify-before-rebind",
        ConditionVerdictV1::Pass,
    );
    rebound_again.source_evidence_ref = Some("evidence-after-rebind".into());
    rebound_again.comparison_proof_ref =
        Some("host-comparison:dependencies-unchanged-again".into());
    let operation_count = twice_target.operation_refs.len();
    let twice = apply(
        &twice_target,
        HostEventV1::RebindEvidence {
            evidence: rebound_again,
        },
    )
    .unwrap();
    assert_eq!(twice.operation_refs.len(), operation_count);
    assert_eq!(twice.run_state.target_version, "target-3");
    assert_eq!(twice.evidence_refs.len(), 3);
    assert_eq!(
        twice.evidence_refs[2].source_evidence_ref.as_deref(),
        Some("evidence-after-rebind")
    );
}

#[test]
fn rebind_evidence_rejects_changed_condition_source_or_dependency_scope() {
    let (state, rebound) = prepared_evidence_rebind();

    let mut different_condition = rebound.clone();
    different_condition.condition_id = "AC-2".into();
    assert!(
        apply(
            &state,
            HostEventV1::RebindEvidence {
                evidence: different_condition,
            }
        )
        .is_err()
    );

    let mut different_source = rebound.clone();
    different_source.source_evidence_ref = Some("evidence-from-other-condition".into());
    assert!(
        apply(
            &state,
            HostEventV1::RebindEvidence {
                evidence: different_source,
            }
        )
        .is_err()
    );

    let mut changed_scope = rebound.clone();
    changed_scope.comparison_scope_digest = "sha256:changed-dependencies".into();
    assert!(
        apply(
            &state,
            HostEventV1::RebindEvidence {
                evidence: changed_scope,
            }
        )
        .is_err()
    );

    let mut changed_verifier_configuration = rebound;
    changed_verifier_configuration.verifier_config_revision += 1;
    assert!(
        apply(
            &state,
            HostEventV1::RebindEvidence {
                evidence: changed_verifier_configuration,
            }
        )
        .is_err()
    );
}

#[test]
fn reproduce_failure_completion_accepts_rebound_candidate_without_rerun() {
    let state = running_reproduce_failure();
    let state = record_operation(&state, "baseline", OperationOutcomeV1::Failed);
    let baseline = negative_evidence(
        &state,
        "baseline-evidence",
        "baseline",
        ConditionVerdictV1::Fail,
        "baseline",
        None,
    );
    let state = apply(&state, HostEventV1::RecordEvidence { evidence: baseline }).unwrap();

    let state = reserve_operation(&state, "apply-fix", OperationKindV1::ApplyPatch);
    let state = finalize_operation_with_target(&state, "apply-fix", "target-fixed");
    let state = apply(
        &state,
        HostEventV1::InvalidateEvidence {
            evidence_ids: vec!["baseline-evidence".into()],
            new_target_version: "target-fixed".into(),
        },
    )
    .unwrap();
    let state = record_operation(&state, "candidate-verifier", OperationOutcomeV1::Succeeded);
    let state = verifying(&state);
    let candidate = negative_evidence(
        &state,
        "candidate-evidence",
        "candidate-verifier",
        ConditionVerdictV1::Pass,
        "baseline",
        Some("candidate-verifier"),
    );
    let state = apply(
        &state,
        HostEventV1::RecordEvidence {
            evidence: candidate,
        },
    )
    .unwrap();

    let state = reserve_operation(&state, "unrelated-change", OperationKindV1::ApplyPatch);
    let state = finalize_operation_with_target(&state, "unrelated-change", "target-unrelated");
    let state = apply(
        &state,
        HostEventV1::InvalidateEvidence {
            evidence_ids: vec!["candidate-evidence".into()],
            new_target_version: "target-unrelated".into(),
        },
    )
    .unwrap();
    let mut rebound = state
        .evidence_refs
        .iter()
        .find(|evidence| evidence.evidence_id == "candidate-evidence")
        .unwrap()
        .clone();
    rebound.evidence_id = "candidate-evidence-rebound".into();
    rebound.target_version = "target-unrelated".into();
    rebound.source_evidence_ref = Some("candidate-evidence".into());
    rebound.comparison_proof_ref = Some("host-comparison:candidate-dependencies-unchanged".into());
    rebound.lifecycle = EvidenceLifecycleV1::Current;

    let verifier_count = state
        .operation_refs
        .iter()
        .filter(|record| record.intent.operation_kind == OperationKindV1::RunVerifier)
        .count();
    let state = apply(&state, HostEventV1::RebindEvidence { evidence: rebound }).unwrap();
    assert_eq!(
        state
            .operation_refs
            .iter()
            .filter(|record| record.intent.operation_kind == OperationKindV1::RunVerifier)
            .count(),
        verifier_count
    );
    let completed = apply(
        &state,
        HostEventV1::Complete {
            target_version: "target-unrelated".into(),
        },
    )
    .expect("unchanged candidate dependencies remain proven after host rebind");
    assert_eq!(completed.run_state.status, TaskRunStatusV1::Completed);
    assert_eq!(completed.operation_refs.len(), state.operation_refs.len());
}

fn reserve_operation(
    state: &CommonHarnessStateV1,
    operation_id: &str,
    kind: OperationKindV1,
) -> CommonHarnessStateV1 {
    let intent = OperationIntentV1 {
        operation_id: operation_id.into(),
        task_id: state.request.task_id.clone(),
        request_revision: state.request.request_revision,
        attempt_no: state.run_state.attempt_no,
        owner_epoch: state.run_state.owner_epoch,
        expected_state_version: state.run_state.state_version,
        base_target_version: state.run_state.target_version.clone(),
        operation_kind: kind,
        normalized_arguments_ref: format!("args:{operation_id}"),
        normalized_arguments_digest: format!("sha256:{operation_id}"),
        permission_decision_ref: "permission:allow".into(),
        approval_ref: None,
        executor_ref: "executor:host".into(),
        timeout_ms: 30_000,
        status: OperationIntentStatusV1::Running,
    };
    apply(
        state,
        HostEventV1::ReserveOperation {
            intent: intent.clone(),
            claim: initial_claim(&intent, &format!("execution:{operation_id}")),
        },
    )
    .expect("reserve the fixture's bound host operation")
}

fn unknown_receipt(state: &CommonHarnessStateV1, operation_id: &str) -> CommonHarnessStateV1 {
    let record = state
        .operation_refs
        .iter()
        .find(|record| record.intent.operation_id == operation_id)
        .unwrap();
    let intent = &record.intent;
    apply(
        state,
        HostEventV1::FinalizeOperation {
            receipt: OperationReceiptV1 {
                operation_id: operation_id.into(),
                task_id: intent.task_id.clone(),
                request_revision: intent.request_revision,
                attempt_no: intent.attempt_no,
                owner_epoch: intent.owner_epoch,
                execution_id: record.claims[0].claim_id.clone(),
                outcome: OperationOutcomeV1::Unknown,
                actual_before_version: intent.base_target_version.clone(),
                actual_after_version: None,
                exit_status: None,
                result_ref: None,
                external_receipt_ref: None,
                reconciliation_ref: None,
            },
        },
    )
    .expect("record the fixture's unknown raw receipt")
}

fn blocked_unsettled_operation(operation_id: &str, kind: OperationKindV1) -> CommonHarnessStateV1 {
    let state = start(&queued(3));
    let state = reserve_operation(&state, operation_id, kind);
    unknown_receipt(&state, operation_id)
}

fn blocked_reconciled_patch(with_other_unsettled: bool) -> CommonHarnessStateV1 {
    let state = start(&queued(3));
    let state = record_operation(&state, "verify-prior", OperationOutcomeV1::Succeeded);
    let state = verifying(&state);
    let state = observe(
        &state,
        "evidence-prior",
        "verify-prior",
        ConditionVerdictV1::Pass,
    );
    let state = reserve_operation(&state, "patch-recovery", OperationKindV1::ApplyPatch);
    let state = if with_other_unsettled {
        reserve_operation(
            &state,
            "external-still-pending",
            OperationKindV1::ExternalAction,
        )
    } else {
        state
    };
    let state = unknown_receipt(&state, "patch-recovery");
    let state = apply(
        &state,
        HostEventV1::TakeOwnership {
            new_owner_id: "owner-2".into(),
            new_owner_epoch: 2,
            takeover_proof_ref: "takeover:proof-2".into(),
        },
    )
    .unwrap();
    let patch = state
        .operation_refs
        .iter()
        .find(|record| record.intent.operation_id == "patch-recovery")
        .unwrap();
    let state = apply(
        &state,
        HostEventV1::ClaimExistingOperation {
            operation_id: "patch-recovery".into(),
            claim: OperationClaimV1 {
                claim_id: "finalizer-patch-recovery".into(),
                operation_id: "patch-recovery".into(),
                previous_claim_id: Some(patch.claims[0].claim_id.clone()),
                owner_epoch: 2,
                expected_state_version: state.run_state.state_version,
                intent_digest: patch.intent.content_digest().unwrap(),
                claimed_at_ms: 2_000_000,
                deadline_at_ms: patch.claims[0].deadline_at_ms,
                kind: OperationClaimKindV1::Finalizer,
                supersedes_execution_claim_id: None,
                native_executor_dead_proof_ref: None,
            },
        },
    )
    .unwrap();
    let patch = state
        .operation_refs
        .iter()
        .find(|record| record.intent.operation_id == "patch-recovery")
        .unwrap();
    let target = state.run_state.target_version.clone();
    let finalized = apply(
        &state,
        HostEventV1::FinalizeReconciledOperation {
            finalization: OperationFinalizationV1 {
                operation_id: "patch-recovery".into(),
                finalizer_claim_id: "finalizer-patch-recovery".into(),
                execution_claim_id: patch.claims[0].claim_id.clone(),
                receipt: OperationReceiptV1 {
                    operation_id: "patch-recovery".into(),
                    task_id: patch.intent.task_id.clone(),
                    request_revision: patch.intent.request_revision,
                    attempt_no: patch.intent.attempt_no,
                    owner_epoch: patch.intent.owner_epoch,
                    execution_id: patch.claims[0].claim_id.clone(),
                    outcome: OperationOutcomeV1::Succeeded,
                    actual_before_version: target,
                    actual_after_version: Some("target-after-patch".into()),
                    exit_status: Some(0),
                    result_ref: Some("journal:patch-observation".into()),
                    external_receipt_ref: None,
                    reconciliation_ref: Some("journal:patch-resolution".into()),
                },
                reconciliation_ref: Some("journal:patch-resolution".into()),
                source_observation_refs: vec!["journal:patch-observation".into()],
            },
        },
    )
    .unwrap();
    let restored = finalized;
    if with_other_unsettled {
        assert_eq!(restored.run_state.status, TaskRunStatusV1::Blocked);
        assert_eq!(restored.run_state.phase, TaskRunPhaseV1::Reconcile);
        assert_eq!(
            restored.run_state.reason_code.as_deref(),
            Some("operation_outcome_unknown")
        );
        assert_eq!(
            restored.run_state.resume_condition.as_deref(),
            Some("patch-recovery")
        );
    } else {
        assert_eq!(restored.run_state.status, TaskRunStatusV1::Running);
        assert_eq!(restored.run_state.phase, TaskRunPhaseV1::Apply);
        assert_eq!(restored.run_state.target_version, "target-1");
        assert_eq!(restored.run_state.reason_code, None);
        assert_eq!(restored.run_state.resume_condition, None);
    }
    restored.validate(None).unwrap();
    restored
}

fn observe(
    state: &CommonHarnessStateV1,
    evidence_id: &str,
    operation_id: &str,
    verdict: ConditionVerdictV1,
) -> CommonHarnessStateV1 {
    apply(
        state,
        HostEventV1::RecordEvidence {
            evidence: evidence(state, evidence_id, operation_id, verdict),
        },
    )
    .unwrap()
}

fn completed(max_attempts: u32) -> CommonHarnessStateV1 {
    let state = start(&queued(max_attempts));
    let state = record_operation(&state, "verify-1", OperationOutcomeV1::Succeeded);
    let state = verifying(&state);
    let state = observe(&state, "evidence-1", "verify-1", ConditionVerdictV1::Pass);
    apply(
        &state,
        HostEventV1::Complete {
            target_version: state.run_state.target_version.clone(),
        },
    )
    .unwrap()
}

#[test]
fn stale_state_version_and_owner_epoch_fences_reject_host_updates() {
    let state = start(&queued(3));
    let stale_version = StateFenceV1 {
        expected_state_version: state.run_state.state_version - 1,
        ..fence(&state)
    };
    assert_eq!(
        state.apply_host_event(
            stale_version,
            HostEventV1::BeginVerification {
                target_version: state.run_state.target_version.clone(),
            }
        ),
        Err(TransitionErrorV1::FenceMismatch)
    );
    let stale_owner = StateFenceV1 {
        owner_epoch: state.run_state.owner_epoch - 1,
        ..fence(&state)
    };
    assert_eq!(
        state.apply_host_event(stale_owner, HostEventV1::RequestCancel),
        Err(TransitionErrorV1::FenceMismatch)
    );
}

fn fence(state: &CommonHarnessStateV1) -> StateFenceV1 {
    StateFenceV1 {
        request_revision: state.request.request_revision,
        expected_state_version: state.run_state.state_version,
        owner_epoch: state.run_state.owner_epoch,
    }
}

#[test]
fn attempt_cannot_start_with_unsettled_operation_or_after_cancel_request() {
    let state = start(&queued(3));
    let state = record_operation(&state, "failed-check", OperationOutcomeV1::Failed);
    let state = verifying(&state);
    let state = observe(
        &state,
        "failed-evidence",
        "failed-check",
        ConditionVerdictV1::Fail,
    );
    let pending_intent = OperationIntentV1 {
        operation_id: "unsettled-patch".into(),
        task_id: state.request.task_id.clone(),
        request_revision: state.request.request_revision,
        attempt_no: state.run_state.attempt_no,
        owner_epoch: state.run_state.owner_epoch,
        expected_state_version: state.run_state.state_version,
        base_target_version: state.run_state.target_version.clone(),
        operation_kind: OperationKindV1::ApplyPatch,
        normalized_arguments_ref: "args:unsettled-patch".into(),
        normalized_arguments_digest: "sha256:patch-args".into(),
        permission_decision_ref: "permission:allow".into(),
        approval_ref: None,
        executor_ref: "executor:patch".into(),
        timeout_ms: 30_000,
        status: OperationIntentStatusV1::Reserved,
    };
    let state = apply(
        &state,
        HostEventV1::ReserveOperation {
            intent: pending_intent.clone(),
            claim: initial_claim(&pending_intent, "claim-unsettled-patch"),
        },
    )
    .unwrap();
    let event = HostEventV1::StartAttempt {
        target_version: "target-2".into(),
    };
    assert_eq!(
        apply(&state, event.clone()),
        Err(TransitionErrorV1::UnsettledOperations)
    );
    let cancelled = apply(&state, HostEventV1::RequestCancel).unwrap();
    assert!(cancelled.run_state.cancel_requested);
    assert_eq!(cancelled.run_state.status, TaskRunStatusV1::Blocked);
    assert!(matches!(
        apply(&cancelled, event),
        Err(TransitionErrorV1::InvalidState(_))
    ));
    assert_eq!(cancelled.run_state.attempt_no, 1);
}

#[test]
fn latest_pass_supersedes_old_failure_for_retry_eligibility() {
    let state = start(&queued(3));
    let state = record_operation(&state, "fail-1", OperationOutcomeV1::Failed);
    let state = verifying(&state);
    let state = observe(&state, "evidence-fail", "fail-1", ConditionVerdictV1::Fail);
    let state = record_operation(&state, "pass-1", OperationOutcomeV1::Succeeded);
    let state = observe(&state, "evidence-pass", "pass-1", ConditionVerdictV1::Pass);

    assert!(matches!(
        apply(
            &state,
            HostEventV1::StartAttempt {
                target_version: state.run_state.target_version.clone(),
            }
        ),
        Err(TransitionErrorV1::InvalidState(_))
    ));
    assert_eq!(state.run_state.attempt_no, 1);
}

#[test]
fn third_failed_attempt_exhausts_without_allowing_a_fourth_start() {
    let mut state = start(&queued(3));
    for attempt in 1..=3 {
        let operation_id = format!("attempt-{attempt}-check");
        let evidence_id = format!("attempt-{attempt}-evidence");
        state = record_operation(&state, &operation_id, OperationOutcomeV1::Failed);
        state = verifying(&state);
        state = observe(
            &state,
            &evidence_id,
            &operation_id,
            ConditionVerdictV1::Fail,
        );
        if attempt < 3 {
            state = apply(
                &state,
                HostEventV1::StartAttempt {
                    target_version: state.run_state.target_version.clone(),
                },
            )
            .unwrap();
        }
    }
    assert_eq!(state.run_state.attempt_no, 3);
    let exhausted = apply(
        &state,
        HostEventV1::EndAttempt {
            reason_code: "attempt_budget_exhausted".into(),
        },
    )
    .unwrap();
    assert_eq!(exhausted.run_state.status, TaskRunStatusV1::Exhausted);
    assert_eq!(exhausted.run_state.attempt_no, 3);
    assert!(
        apply(
            &exhausted,
            HostEventV1::StartAttempt {
                target_version: exhausted.run_state.target_version.clone(),
            },
        )
        .is_err()
    );
    assert_eq!(exhausted.run_state.attempt_no, 3);
}

#[test]
fn active_evidence_invalidation_preserves_the_current_attempt() {
    let no_evidence_running = start(&queued(3));
    let advanced_target = apply(
        &no_evidence_running,
        HostEventV1::InvalidateEvidence {
            evidence_ids: vec![],
            new_target_version: "target-2".into(),
        },
    )
    .unwrap();
    assert_eq!(advanced_target.run_state.status, TaskRunStatusV1::Running);
    assert_eq!(advanced_target.run_state.attempt_no, 1);
    assert_eq!(advanced_target.run_state.target_version, "target-2");

    let complete_once = completed(3);
    let queued_for_retry = apply(
        &complete_once,
        HostEventV1::InvalidateEvidence {
            evidence_ids: vec!["evidence-1".into()],
            new_target_version: "target-2".into(),
        },
    )
    .unwrap();
    let running = start(&queued_for_retry);
    let invalidated_running = apply(
        &running,
        HostEventV1::InvalidateEvidence {
            evidence_ids: vec!["evidence-1".into()],
            new_target_version: "target-3".into(),
        },
    )
    .unwrap();
    assert_eq!(
        invalidated_running.run_state.status,
        TaskRunStatusV1::Running
    );
    assert_eq!(invalidated_running.run_state.attempt_no, 2);

    let with_operation = record_operation(
        &invalidated_running,
        "verify-2",
        OperationOutcomeV1::Succeeded,
    );
    let verifying = verifying(&with_operation);
    let with_evidence = observe(
        &verifying,
        "evidence-2",
        "verify-2",
        ConditionVerdictV1::Pass,
    );
    let invalidated_verifying = apply(
        &with_evidence,
        HostEventV1::InvalidateEvidence {
            evidence_ids: vec!["evidence-2".into()],
            new_target_version: "target-4".into(),
        },
    )
    .unwrap();
    assert_eq!(
        invalidated_verifying.run_state.status,
        TaskRunStatusV1::Verifying
    );
    assert_eq!(invalidated_verifying.run_state.attempt_no, 2);
}

#[test]
fn completed_invalidation_queues_or_exhausts_without_resetting_attempts() {
    let completed_with_budget = completed(2);
    let queued = apply(
        &completed_with_budget,
        HostEventV1::InvalidateEvidence {
            evidence_ids: vec!["evidence-1".into()],
            new_target_version: "target-2".into(),
        },
    )
    .unwrap();
    assert_eq!(queued.run_state.status, TaskRunStatusV1::Queued);
    assert_eq!(queued.run_state.attempt_no, 1);

    let second_attempt = start(&queued);
    let second_attempt =
        record_operation(&second_attempt, "verify-2", OperationOutcomeV1::Succeeded);
    let second_attempt = verifying(&second_attempt);
    let second_attempt = observe(
        &second_attempt,
        "evidence-2",
        "verify-2",
        ConditionVerdictV1::Pass,
    );
    let completed = apply(
        &second_attempt,
        HostEventV1::Complete {
            target_version: second_attempt.run_state.target_version.clone(),
        },
    )
    .unwrap();
    let exhausted = apply(
        &completed,
        HostEventV1::InvalidateEvidence {
            evidence_ids: vec!["evidence-2".into()],
            new_target_version: "target-3".into(),
        },
    )
    .unwrap();
    assert_eq!(exhausted.run_state.status, TaskRunStatusV1::Exhausted);
    assert_eq!(exhausted.run_state.attempt_no, 2);
}

#[test]
fn unknown_receipt_stays_unsettled_and_blocks_success_or_cancellation() {
    let state = start(&queued(3));
    let intent = OperationIntentV1 {
        operation_id: "external-1".into(),
        task_id: state.request.task_id.clone(),
        request_revision: state.request.request_revision,
        attempt_no: state.run_state.attempt_no,
        owner_epoch: state.run_state.owner_epoch,
        expected_state_version: state.run_state.state_version,
        base_target_version: state.run_state.target_version.clone(),
        operation_kind: OperationKindV1::ExternalAction,
        normalized_arguments_ref: "args:external-1".into(),
        normalized_arguments_digest: "sha256:external-args".into(),
        permission_decision_ref: "permission:allow".into(),
        approval_ref: None,
        executor_ref: "executor:external".into(),
        timeout_ms: 30_000,
        status: OperationIntentStatusV1::Running,
    };
    let state = apply(
        &state,
        HostEventV1::ReserveOperation {
            intent: intent.clone(),
            claim: initial_claim(&intent, "claim-external-1"),
        },
    )
    .unwrap();
    let state = apply(
        &state,
        HostEventV1::FinalizeOperation {
            receipt: OperationReceiptV1 {
                operation_id: intent.operation_id.clone(),
                task_id: intent.task_id,
                request_revision: intent.request_revision,
                attempt_no: intent.attempt_no,
                owner_epoch: intent.owner_epoch,
                execution_id: "claim-external-1".into(),
                outcome: OperationOutcomeV1::Unknown,
                actual_before_version: intent.base_target_version,
                actual_after_version: None,
                exit_status: None,
                result_ref: None,
                external_receipt_ref: None,
                reconciliation_ref: None,
            },
        },
    )
    .unwrap();

    assert_eq!(state.run_state.status, TaskRunStatusV1::Blocked);
    assert_eq!(state.run_state.unsettled_operation_refs, vec!["external-1"]);
    assert_eq!(
        state.operation_refs[0].receipt.as_ref().unwrap().outcome,
        OperationOutcomeV1::Unknown
    );
    assert_eq!(
        apply(
            &state,
            HostEventV1::StartAttempt {
                target_version: state.run_state.target_version.clone(),
            }
        ),
        Err(TransitionErrorV1::UnsettledOperations)
    );
    let state = apply(&state, HostEventV1::RequestCancel).unwrap();
    assert!(matches!(
        apply(&state, HostEventV1::ConfirmCancelled),
        Err(TransitionErrorV1::UnsettledOperations)
    ));
    assert!(matches!(
        apply(
            &state,
            HostEventV1::Complete {
                target_version: state.run_state.target_version.clone(),
            }
        ),
        Err(TransitionErrorV1::InvalidState(_))
    ));
    let operation = &state.operation_refs[0];
    let claim = OperationClaimV1 {
        claim_id: "external-execution-claim-2".into(),
        operation_id: operation.intent.operation_id.clone(),
        previous_claim_id: Some(operation.claims[0].claim_id.clone()),
        owner_epoch: state.run_state.owner_epoch,
        expected_state_version: state.run_state.state_version,
        intent_digest: operation.intent.content_digest().unwrap(),
        claimed_at_ms: 1_000_001,
        deadline_at_ms: operation.claims[0].deadline_at_ms,
        kind: OperationClaimKindV1::Execution,
        supersedes_execution_claim_id: Some(operation.claims[0].claim_id.clone()),
        native_executor_dead_proof_ref: Some("native-dead:claim-1".into()),
    };
    assert_eq!(
        apply(
            &state,
            HostEventV1::ClaimExistingOperation {
                operation_id: operation.intent.operation_id.clone(),
                claim,
            }
        ),
        Err(TransitionErrorV1::InvalidOperation(
            "external_reexecution_unsupported"
        ))
    );
}

#[test]
fn legacy_common_harness_snapshot_omits_and_defaults_the_work_window() {
    let state = queued(3);
    let serialized = serde_json::to_value(&state).unwrap();
    assert!(serialized["run_state"].get("work_window").is_none());
    let mut legacy_json = serialized;
    legacy_json["run_state"]
        .as_object_mut()
        .unwrap()
        .remove("work_window");
    let restored: CommonHarnessStateV1 = serde_json::from_value(legacy_json).unwrap();
    assert_eq!(restored.run_state.work_window, None);
    assert!(restored.validate(None).is_ok());
}

#[test]
fn work_window_binds_once_and_identical_replay_is_idempotent() {
    let state = start(&queued(3));
    let window = work_window(state.run_state.attempt_no);
    let bound = apply(
        &state,
        HostEventV1::BindWorkWindow {
            window: window.clone(),
        },
    )
    .unwrap();
    assert_eq!(bound.run_state.work_window.as_ref(), Some(&window));
    assert_eq!(bound.run_state.attempt_no, state.run_state.attempt_no);
    assert_eq!(bound.run_state.status, state.run_state.status);
    assert_eq!(bound.run_state.phase, state.run_state.phase);
    assert_eq!(
        bound.run_state.target_version,
        state.run_state.target_version
    );
    assert_eq!(
        bound.run_state.state_version,
        state.run_state.state_version + 1
    );

    let replay = apply(&bound, HostEventV1::BindWorkWindow { window }).unwrap();
    assert_eq!(replay, bound);

    let mut changed = bound.run_state.work_window.clone().unwrap();
    changed.deadline_at_ms += 1;
    assert!(apply(&bound, HostEventV1::BindWorkWindow { window: changed }).is_err());
}

#[test]
fn restored_work_window_requires_exact_attempt_and_absolute_interval() {
    let valid = work_window(1);
    let mut valid_state = start(&queued(3));
    valid_state.run_state.work_window = Some(valid.clone());
    assert!(valid_state.validate(None).is_ok());

    let mut wrong_attempt = valid.clone();
    wrong_attempt.attempt_no = 2;
    let mut missing_start = valid.clone();
    missing_start.started_at_ms = 0;
    let mut changed_interval = valid.clone();
    changed_interval.deadline_at_ms += 1;
    let mut zero_timeout = valid.clone();
    zero_timeout.timeout_ms = 0;
    let mut overflowed_interval = valid.clone();
    overflowed_interval.started_at_ms = i64::MAX;
    overflowed_interval.deadline_at_ms = i64::MAX;
    overflowed_interval.timeout_ms = 1;
    let mut empty_window_id = valid.clone();
    empty_window_id.window_id.clear();
    let mut empty_policy = valid;
    empty_policy.policy_ref.clear();

    for invalid in [
        wrong_attempt,
        missing_start,
        changed_interval,
        zero_timeout,
        overflowed_interval,
        empty_window_id,
        empty_policy,
    ] {
        let mut state = start(&queued(3));
        state.run_state.work_window = Some(invalid);
        let snapshot = serde_json::to_value(&state).unwrap();
        let restored: CommonHarnessStateV1 = serde_json::from_value(snapshot).unwrap();
        assert!(restored.validate(None).is_err());
    }
}

#[test]
fn work_window_deadline_survives_waiting_restore_and_owner_takeover() {
    let mut state = start(&queued(3));
    let window = work_window(state.run_state.attempt_no);
    state.run_state.work_window = Some(window.clone());
    let waiting = apply(
        &state,
        HostEventV1::EnterWaiting {
            question_ref: "question:work-window".into(),
            operation_digest: "sha256:approved-operation".into(),
            resume_phase: TaskRunPhaseV1::Apply,
        },
    )
    .unwrap();
    let restored: CommonHarnessStateV1 =
        serde_json::from_value(serde_json::to_value(&waiting).unwrap()).unwrap();
    assert_eq!(restored.run_state.work_window.as_ref(), Some(&window));

    let taken_over = apply(
        &restored,
        HostEventV1::TakeOwnership {
            new_owner_id: "owner-2".into(),
            new_owner_epoch: 2,
            takeover_proof_ref: "takeover:proof-2".into(),
        },
    )
    .unwrap();
    assert_eq!(taken_over.run_state.work_window.as_ref(), Some(&window));
    let resumed = apply(
        &taken_over,
        HostEventV1::ResumeWaitingBound {
            question_ref: "question:work-window".into(),
            operation_digest: "sha256:approved-operation".into(),
            target_version: taken_over.run_state.target_version.clone(),
            answer_ref: "answer:approved".into(),
            answer_digest: "sha256:answer".into(),
        },
    )
    .unwrap();
    assert_eq!(resumed.run_state.work_window.as_ref(), Some(&window));
    assert_eq!(resumed.run_state.attempt_no, state.run_state.attempt_no);
}

#[test]
fn starting_the_next_task_attempt_clears_the_previous_work_window() {
    let mut completed = completed(3);
    completed.run_state.work_window = Some(work_window(completed.run_state.attempt_no));
    let queued = apply(
        &completed,
        HostEventV1::InvalidateEvidence {
            evidence_ids: vec!["evidence-1".into()],
            new_target_version: "target-2".into(),
        },
    )
    .unwrap();
    assert_eq!(
        queued.run_state.work_window,
        completed.run_state.work_window
    );

    let next = apply(
        &queued,
        HostEventV1::StartAttempt {
            target_version: queued.run_state.target_version.clone(),
        },
    )
    .unwrap();
    assert_eq!(
        next.run_state.attempt_no,
        completed.run_state.attempt_no + 1
    );
    assert_eq!(next.run_state.work_window, None);
}

#[test]
fn waiting_snapshot_restores_its_binding_without_consuming_an_attempt() {
    let state = start(&queued(3));
    let waiting = apply(
        &state,
        HostEventV1::EnterWaiting {
            question_ref: "question:approval".into(),
            operation_digest: "sha256:approved-operation".into(),
            resume_phase: TaskRunPhaseV1::Apply,
        },
    )
    .unwrap();
    let restored: CommonHarnessStateV1 =
        serde_json::from_value(serde_json::to_value(&waiting).unwrap()).unwrap();
    assert_eq!(
        restored
            .run_state
            .waiting
            .as_ref()
            .unwrap()
            .operation_digest,
        "sha256:approved-operation"
    );
    assert_eq!(restored.run_state.attempt_no, 1);

    let resumed = apply(
        &restored,
        HostEventV1::ResumeWaitingBound {
            question_ref: "question:approval".into(),
            operation_digest: "sha256:approved-operation".into(),
            target_version: restored.run_state.target_version.clone(),
            answer_ref: "answer:approved".into(),
            answer_digest: "sha256:answer".into(),
        },
    )
    .unwrap();
    assert_eq!(resumed.run_state.status, TaskRunStatusV1::Running);
    assert_eq!(resumed.run_state.phase, TaskRunPhaseV1::Apply);
    assert_eq!(resumed.run_state.attempt_no, 1);
}

#[test]
fn finalization_is_idempotent_and_conflicting_receipt_is_rejected() {
    let state = start(&queued(3));
    let intent = OperationIntentV1 {
        operation_id: "verify-idempotent".into(),
        task_id: state.request.task_id.clone(),
        request_revision: state.request.request_revision,
        attempt_no: state.run_state.attempt_no,
        owner_epoch: state.run_state.owner_epoch,
        expected_state_version: state.run_state.state_version,
        base_target_version: state.run_state.target_version.clone(),
        operation_kind: OperationKindV1::RunVerifier,
        normalized_arguments_ref: "args:verify-idempotent".into(),
        normalized_arguments_digest: "sha256:args".into(),
        permission_decision_ref: "permission:allow".into(),
        approval_ref: None,
        executor_ref: "executor:go".into(),
        timeout_ms: 30_000,
        status: OperationIntentStatusV1::Running,
    };
    let state = apply(
        &state,
        HostEventV1::ReserveOperation {
            intent: intent.clone(),
            claim: initial_claim(&intent, "claim-verify-idempotent"),
        },
    )
    .unwrap();
    let receipt = OperationReceiptV1 {
        operation_id: intent.operation_id.clone(),
        task_id: intent.task_id,
        request_revision: intent.request_revision,
        attempt_no: intent.attempt_no,
        owner_epoch: intent.owner_epoch,
        execution_id: "claim-verify-idempotent".into(),
        outcome: OperationOutcomeV1::Succeeded,
        actual_before_version: intent.base_target_version,
        actual_after_version: Some("target-after".into()),
        exit_status: Some(0),
        result_ref: Some("receipt:verify-idempotent".into()),
        external_receipt_ref: None,
        reconciliation_ref: None,
    };
    let finalized = apply(
        &state,
        HostEventV1::FinalizeOperation {
            receipt: receipt.clone(),
        },
    )
    .unwrap();
    let repeated = apply(
        &finalized,
        HostEventV1::FinalizeOperation {
            receipt: receipt.clone(),
        },
    )
    .unwrap();
    assert_eq!(repeated, finalized);

    let mut conflicting = receipt;
    conflicting.exit_status = Some(1);
    assert_eq!(
        apply(
            &finalized,
            HostEventV1::FinalizeOperation {
                receipt: conflicting
            }
        ),
        Err(TransitionErrorV1::OperationAlreadyFinalized)
    );
}

#[test]
fn takeover_uses_state_cas_and_fences_the_previous_owner_epoch() {
    let state = start(&queued(3));
    let stale_fence = StateFenceV1 {
        expected_state_version: state.run_state.state_version - 1,
        ..fence(&state)
    };
    assert_eq!(
        state.apply_host_event(
            stale_fence,
            HostEventV1::TakeOwnership {
                new_owner_id: "owner-2".into(),
                new_owner_epoch: 2,
                takeover_proof_ref: "takeover:proof-1".into(),
            }
        ),
        Err(TransitionErrorV1::FenceMismatch)
    );

    let taken_over = apply(
        &state,
        HostEventV1::TakeOwnership {
            new_owner_id: "owner-2".into(),
            new_owner_epoch: 2,
            takeover_proof_ref: "takeover:proof-1".into(),
        },
    )
    .unwrap();
    assert_eq!(taken_over.run_state.owner_id, "owner-2");
    assert_eq!(taken_over.run_state.owner_epoch, 2);
    assert_eq!(taken_over.owner_takeovers.len(), 1);
    assert_eq!(
        taken_over.apply_host_event(fence(&state), HostEventV1::RequestCancel),
        Err(TransitionErrorV1::FenceMismatch)
    );
}

#[test]
fn waiting_resume_requires_the_saved_question_operation_and_target_binding() {
    let running = start(&queued(3));
    let waiting = apply(
        &running,
        HostEventV1::EnterWaiting {
            question_ref: "question:review".into(),
            operation_digest: "sha256:approved-operation".into(),
            resume_phase: TaskRunPhaseV1::Apply,
        },
    )
    .unwrap();
    let resumed = apply(
        &waiting,
        HostEventV1::ResumeWaitingBound {
            question_ref: "question:review".into(),
            operation_digest: "sha256:approved-operation".into(),
            target_version: waiting.run_state.target_version.clone(),
            answer_ref: "answer:yes".into(),
            answer_digest: "sha256:answer-content".into(),
        },
    )
    .unwrap();
    assert_eq!(resumed.run_state.status, TaskRunStatusV1::Running);
    assert_eq!(resumed.run_state.attempt_no, waiting.run_state.attempt_no);
}

#[test]
fn unbound_waiting_resume_cannot_authorize_the_saved_operation() {
    let running = start(&queued(3));
    let waiting = apply(
        &running,
        HostEventV1::EnterWaiting {
            question_ref: "question:review".into(),
            operation_digest: "sha256:approved-operation".into(),
            resume_phase: TaskRunPhaseV1::Apply,
        },
    )
    .unwrap();
    assert_eq!(
        apply(
            &waiting,
            HostEventV1::ResumeWaiting {
                answer_ref: "answer:yes".into(),
                answer_digest: "sha256:answer-content".into(),
            }
        ),
        Err(TransitionErrorV1::InvalidState(
            "resume_requires_saved_binding"
        ))
    );
}

fn reserved_native_operation() -> CommonHarnessStateV1 {
    let state = start(&queued(3));
    let intent = OperationIntentV1 {
        operation_id: "patch-recovery".into(),
        task_id: state.request.task_id.clone(),
        request_revision: state.request.request_revision,
        attempt_no: state.run_state.attempt_no,
        owner_epoch: state.run_state.owner_epoch,
        expected_state_version: state.run_state.state_version,
        base_target_version: state.run_state.target_version.clone(),
        operation_kind: OperationKindV1::ApplyPatch,
        normalized_arguments_ref: "args:patch-recovery".into(),
        normalized_arguments_digest: "sha256:patch-recovery-args".into(),
        permission_decision_ref: "permission:allow".into(),
        approval_ref: None,
        executor_ref: "executor:patch".into(),
        timeout_ms: 30_000,
        status: OperationIntentStatusV1::Running,
    };
    apply(
        &state,
        HostEventV1::ReserveOperation {
            claim: initial_claim(&intent, "execution-claim-1"),
            intent,
        },
    )
    .unwrap()
}

fn finalize_operation_with_target(
    state: &CommonHarnessStateV1,
    operation_id: &str,
    target_after: &str,
) -> CommonHarnessStateV1 {
    let record = state
        .operation_refs
        .iter()
        .find(|record| record.intent.operation_id == operation_id)
        .unwrap();
    let intent = &record.intent;
    let execution_claim = record
        .claims
        .iter()
        .rev()
        .find(|claim| claim.kind == OperationClaimKindV1::Execution)
        .unwrap();
    apply(
        state,
        HostEventV1::FinalizeOperation {
            receipt: OperationReceiptV1 {
                operation_id: operation_id.into(),
                task_id: intent.task_id.clone(),
                request_revision: intent.request_revision,
                attempt_no: intent.attempt_no,
                owner_epoch: execution_claim.owner_epoch,
                execution_id: execution_claim.claim_id.clone(),
                outcome: OperationOutcomeV1::Succeeded,
                actual_before_version: intent.base_target_version.clone(),
                actual_after_version: Some(target_after.into()),
                exit_status: Some(0),
                result_ref: Some(format!("artifact:receipt:{operation_id}")),
                external_receipt_ref: None,
                reconciliation_ref: None,
            },
        },
    )
    .expect("finalize the fixture's successful operation")
}

#[test]
fn reexecution_claim_requires_positive_dead_proof_for_last_execution_claim() {
    let state = reserved_native_operation();
    let record = &state.operation_refs[0];
    let claim = OperationClaimV1 {
        claim_id: "execution-claim-2".into(),
        operation_id: record.intent.operation_id.clone(),
        previous_claim_id: Some(record.claims[0].claim_id.clone()),
        owner_epoch: state.run_state.owner_epoch,
        expected_state_version: state.run_state.state_version,
        intent_digest: record.intent.content_digest().unwrap(),
        claimed_at_ms: 999_000,
        deadline_at_ms: record.claims[0].deadline_at_ms,
        kind: OperationClaimKindV1::Execution,
        supersedes_execution_claim_id: Some(record.claims[0].claim_id.clone()),
        native_executor_dead_proof_ref: None,
    };
    assert_eq!(
        apply(
            &state,
            HostEventV1::ClaimExistingOperation {
                operation_id: record.intent.operation_id.clone(),
                claim,
            }
        ),
        Err(TransitionErrorV1::InvalidOperation(
            "executor_not_proven_dead"
        ))
    );
}

#[test]
fn reexecution_claim_cannot_extend_the_original_absolute_deadline() {
    let state = reserved_native_operation();
    let record = &state.operation_refs[0];
    let claim = OperationClaimV1 {
        claim_id: "execution-claim-2".into(),
        operation_id: record.intent.operation_id.clone(),
        previous_claim_id: Some(record.claims[0].claim_id.clone()),
        owner_epoch: state.run_state.owner_epoch,
        expected_state_version: state.run_state.state_version,
        intent_digest: record.intent.content_digest().unwrap(),
        claimed_at_ms: 999_000,
        deadline_at_ms: record.claims[0].deadline_at_ms + 1,
        kind: OperationClaimKindV1::Execution,
        supersedes_execution_claim_id: Some(record.claims[0].claim_id.clone()),
        native_executor_dead_proof_ref: Some("executor-dead:claim-1".into()),
    };
    assert_eq!(
        apply(
            &state,
            HostEventV1::ClaimExistingOperation {
                operation_id: record.intent.operation_id.clone(),
                claim,
            }
        ),
        Err(TransitionErrorV1::InvalidOperation("deadline_changed"))
    );
}

#[test]
fn takeover_execution_receipt_is_bound_to_the_new_execution_claim_epoch() {
    let state = reserved_native_operation();
    let taken_over = apply(
        &state,
        HostEventV1::TakeOwnership {
            new_owner_id: "owner-2".into(),
            new_owner_epoch: 2,
            takeover_proof_ref: "takeover:proof-2".into(),
        },
    )
    .unwrap();
    let record = &taken_over.operation_refs[0];
    let reclaimed = apply(
        &taken_over,
        HostEventV1::ClaimExistingOperation {
            operation_id: record.intent.operation_id.clone(),
            claim: OperationClaimV1 {
                claim_id: "execution-claim-2".into(),
                operation_id: record.intent.operation_id.clone(),
                previous_claim_id: Some(record.claims[0].claim_id.clone()),
                owner_epoch: 2,
                expected_state_version: taken_over.run_state.state_version,
                intent_digest: record.intent.content_digest().unwrap(),
                claimed_at_ms: 999_000,
                deadline_at_ms: record.claims[0].deadline_at_ms,
                kind: OperationClaimKindV1::Execution,
                supersedes_execution_claim_id: Some(record.claims[0].claim_id.clone()),
                native_executor_dead_proof_ref: Some("executor-dead:claim-1".into()),
            },
        },
    )
    .unwrap();
    let receipt = OperationReceiptV1 {
        operation_id: "patch-recovery".into(),
        task_id: reclaimed.request.task_id.clone(),
        request_revision: reclaimed.request.request_revision,
        attempt_no: reclaimed.run_state.attempt_no,
        owner_epoch: 2,
        execution_id: "execution-claim-2".into(),
        outcome: OperationOutcomeV1::Succeeded,
        actual_before_version: reclaimed.run_state.target_version.clone(),
        actual_after_version: Some("target-after-recovery".into()),
        exit_status: None,
        result_ref: Some("journal:recovered-execution".into()),
        external_receipt_ref: None,
        reconciliation_ref: None,
    };
    let mut wrong_claim_receipt = receipt.clone();
    wrong_claim_receipt.execution_id = "forged-execution-id".into();
    assert!(matches!(
        apply(
            &reclaimed,
            HostEventV1::FinalizeOperation {
                receipt: wrong_claim_receipt,
            }
        ),
        Err(TransitionErrorV1::InvalidOperation(_))
    ));
    let settled = apply(&reclaimed, HostEventV1::FinalizeOperation { receipt }).unwrap();
    assert!(settled.run_state.unsettled_operation_refs.is_empty());
    assert_eq!(
        settled.operation_refs[0]
            .effective_receipt()
            .unwrap()
            .owner_epoch,
        2
    );
}

#[test]
fn finalizer_claim_requires_an_unknown_raw_receipt() {
    let state = reserved_native_operation();
    let record = &state.operation_refs[0];
    let claim = OperationClaimV1 {
        claim_id: "finalizer-before-observation".into(),
        operation_id: record.intent.operation_id.clone(),
        previous_claim_id: Some(record.claims[0].claim_id.clone()),
        owner_epoch: state.run_state.owner_epoch,
        expected_state_version: state.run_state.state_version,
        intent_digest: record.intent.content_digest().unwrap(),
        claimed_at_ms: 999_500,
        deadline_at_ms: record.claims[0].deadline_at_ms,
        kind: OperationClaimKindV1::Finalizer,
        supersedes_execution_claim_id: None,
        native_executor_dead_proof_ref: None,
    };
    let transition = apply(
        &state,
        HostEventV1::ClaimExistingOperation {
            operation_id: record.intent.operation_id.clone(),
            claim,
        },
    );
    assert!(matches!(
        transition,
        Err(TransitionErrorV1::InvalidOperation(_))
    ));
    assert_eq!(state.operation_refs[0].claims.len(), 1);
    assert_eq!(state.operation_refs[0].receipt, None);
}

#[test]
fn retry_after_unknown_keeps_raw_receipt_and_finalizes_against_new_claim() {
    let state = reserved_native_operation();
    let state = apply(
        &state,
        HostEventV1::FinalizeOperation {
            receipt: OperationReceiptV1 {
                operation_id: "patch-recovery".into(),
                task_id: state.request.task_id.clone(),
                request_revision: state.request.request_revision,
                attempt_no: state.run_state.attempt_no,
                owner_epoch: state.run_state.owner_epoch,
                execution_id: "execution-claim-1".into(),
                outcome: OperationOutcomeV1::Unknown,
                actual_before_version: state.run_state.target_version.clone(),
                actual_after_version: None,
                exit_status: None,
                result_ref: None,
                external_receipt_ref: None,
                reconciliation_ref: None,
            },
        },
    )
    .unwrap();
    let raw_unknown = state.operation_refs[0].receipt.clone().unwrap();
    let taken_over = apply(
        &state,
        HostEventV1::TakeOwnership {
            new_owner_id: "owner-2".into(),
            new_owner_epoch: 2,
            takeover_proof_ref: "takeover:proof-2".into(),
        },
    )
    .unwrap();
    let record = &taken_over.operation_refs[0];
    let reclaimed = apply(
        &taken_over,
        HostEventV1::ClaimExistingOperation {
            operation_id: record.intent.operation_id.clone(),
            claim: OperationClaimV1 {
                claim_id: "execution-claim-2".into(),
                operation_id: record.intent.operation_id.clone(),
                previous_claim_id: Some(record.claims[0].claim_id.clone()),
                owner_epoch: 2,
                expected_state_version: taken_over.run_state.state_version,
                intent_digest: record.intent.content_digest().unwrap(),
                claimed_at_ms: 999_000,
                deadline_at_ms: record.claims[0].deadline_at_ms,
                kind: OperationClaimKindV1::Execution,
                supersedes_execution_claim_id: Some(record.claims[0].claim_id.clone()),
                native_executor_dead_proof_ref: Some("executor-dead:claim-1".into()),
            },
        },
    )
    .unwrap();
    let resolved = apply(
        &reclaimed,
        HostEventV1::FinalizeReconciledOperation {
            finalization: OperationFinalizationV1 {
                operation_id: "patch-recovery".into(),
                finalizer_claim_id: "execution-claim-2".into(),
                execution_claim_id: "execution-claim-2".into(),
                receipt: OperationReceiptV1 {
                    operation_id: "patch-recovery".into(),
                    task_id: reclaimed.request.task_id.clone(),
                    request_revision: reclaimed.request.request_revision,
                    attempt_no: reclaimed.run_state.attempt_no,
                    owner_epoch: 2,
                    execution_id: "execution-claim-2".into(),
                    outcome: OperationOutcomeV1::Succeeded,
                    actual_before_version: raw_unknown.actual_before_version.clone(),
                    actual_after_version: Some("target-after-retry".into()),
                    exit_status: None,
                    result_ref: Some("journal:retry-result".into()),
                    external_receipt_ref: None,
                    reconciliation_ref: Some("journal:retry-resolution".into()),
                },
                reconciliation_ref: Some("journal:retry-resolution".into()),
                source_observation_refs: vec!["journal:retry-result".into()],
            },
        },
    )
    .unwrap();
    let record = &resolved.operation_refs[0];
    assert_eq!(record.receipt.as_ref(), Some(&raw_unknown));
    assert_eq!(
        record.effective_receipt().unwrap().execution_id,
        "execution-claim-2"
    );
    assert!(resolved.run_state.unsettled_operation_refs.is_empty());
}

#[test]
fn missing_receipt_observation_after_takeover_recovers_original_execution_allpost() {
    let state = reserved_native_operation();
    let original_claim = state.operation_refs[0].claims[0].clone();
    let original_target = state.run_state.target_version.clone();
    let owner_two = apply(
        &state,
        HostEventV1::TakeOwnership {
            new_owner_id: "owner-2".into(),
            new_owner_epoch: 2,
            takeover_proof_ref: "takeover:proof-2".into(),
        },
    )
    .unwrap();
    let observation_ref = "artifact:missing-execution-observation";
    let observed = apply(
        &owner_two,
        HostEventV1::ObserveMissingOperation {
            operation_id: "patch-recovery".into(),
            observation_ref: observation_ref.into(),
        },
    )
    .unwrap();
    let record = &observed.operation_refs[0];
    let raw_unknown = record.receipt.as_ref().unwrap();
    assert_eq!(record.claims.len(), 1);
    assert_eq!(record.claims[0], original_claim);
    assert_eq!(raw_unknown.outcome, OperationOutcomeV1::Unknown);
    assert_eq!(raw_unknown.execution_id, original_claim.claim_id);
    assert_eq!(raw_unknown.owner_epoch, original_claim.owner_epoch);
    assert_eq!(raw_unknown.actual_before_version, original_target);
    assert_eq!(raw_unknown.actual_after_version, None);
    assert_eq!(raw_unknown.exit_status, None);
    assert_eq!(raw_unknown.result_ref.as_deref(), Some(observation_ref));
    assert_eq!(raw_unknown.reconciliation_ref, None);
    assert_eq!(record.effective_receipt(), None);
    assert_eq!(
        observed.run_state.unsettled_operation_refs,
        vec!["patch-recovery"]
    );
    assert_eq!(observed.run_state.status, TaskRunStatusV1::Blocked);
    assert_eq!(observed.run_state.phase, TaskRunPhaseV1::Reconcile);
    assert_eq!(
        apply(
            &observed,
            HostEventV1::ObserveMissingOperation {
                operation_id: "patch-recovery".into(),
                observation_ref: observation_ref.into(),
            }
        )
        .unwrap(),
        observed,
        "replaying the exact missing-operation observation is idempotent"
    );
    assert_eq!(
        apply(
            &observed,
            HostEventV1::ObserveMissingOperation {
                operation_id: "patch-recovery".into(),
                observation_ref: "artifact:conflicting-observation".into(),
            }
        ),
        Err(TransitionErrorV1::OperationAlreadyFinalized)
    );

    let with_finalizer = apply(
        &observed,
        HostEventV1::ClaimExistingOperation {
            operation_id: "patch-recovery".into(),
            claim: OperationClaimV1 {
                claim_id: "finalizer-after-crash".into(),
                operation_id: "patch-recovery".into(),
                previous_claim_id: Some(original_claim.claim_id.clone()),
                owner_epoch: 2,
                expected_state_version: observed.run_state.state_version,
                intent_digest: observed.operation_refs[0].intent.content_digest().unwrap(),
                claimed_at_ms: 2_000_000,
                deadline_at_ms: original_claim.deadline_at_ms,
                kind: OperationClaimKindV1::Finalizer,
                supersedes_execution_claim_id: None,
                native_executor_dead_proof_ref: None,
            },
        },
    )
    .unwrap();
    let settled = apply(
        &with_finalizer,
        HostEventV1::FinalizeReconciledOperation {
            finalization: OperationFinalizationV1 {
                operation_id: "patch-recovery".into(),
                finalizer_claim_id: "finalizer-after-crash".into(),
                execution_claim_id: original_claim.claim_id.clone(),
                receipt: OperationReceiptV1 {
                    operation_id: "patch-recovery".into(),
                    task_id: with_finalizer.request.task_id.clone(),
                    request_revision: with_finalizer.request.request_revision,
                    attempt_no: with_finalizer.run_state.attempt_no,
                    owner_epoch: original_claim.owner_epoch,
                    execution_id: original_claim.claim_id,
                    outcome: OperationOutcomeV1::Succeeded,
                    actual_before_version: original_target,
                    actual_after_version: Some("target-after-crash-patch".into()),
                    exit_status: None,
                    result_ref: Some("artifact:allpost-result".into()),
                    external_receipt_ref: None,
                    reconciliation_ref: Some("artifact:allpost-reconciliation".into()),
                },
                reconciliation_ref: Some("artifact:allpost-reconciliation".into()),
                source_observation_refs: vec![
                    observation_ref.into(),
                    "artifact:allpost-result".into(),
                ],
            },
        },
    )
    .unwrap();
    assert_eq!(
        settled.operation_refs[0]
            .receipt
            .as_ref()
            .unwrap()
            .result_ref
            .as_deref(),
        Some(observation_ref)
    );
    assert_eq!(
        settled.operation_refs[0]
            .effective_receipt()
            .unwrap()
            .outcome,
        OperationOutcomeV1::Succeeded
    );
    let attempt_no = settled.run_state.attempt_no;
    let resumed = apply(
        &settled,
        HostEventV1::ResumeReconciled {
            operation_id: "patch-recovery".into(),
            target_version: "target-after-crash-patch".into(),
        },
    )
    .unwrap();
    assert_eq!(resumed.run_state.target_version, "target-after-crash-patch");
    assert_eq!(resumed.run_state.attempt_no, attempt_no);
}

#[test]
fn ordinary_finalize_cannot_record_a_known_receipt_for_a_stale_execution_claim() {
    let state = reserved_native_operation();
    let taken_over = apply(
        &state,
        HostEventV1::TakeOwnership {
            new_owner_id: "owner-2".into(),
            new_owner_epoch: 2,
            takeover_proof_ref: "takeover:proof-2".into(),
        },
    )
    .unwrap();
    let intent = &taken_over.operation_refs[0].intent;
    assert_eq!(
        apply(
            &taken_over,
            HostEventV1::FinalizeOperation {
                receipt: OperationReceiptV1 {
                    operation_id: intent.operation_id.clone(),
                    task_id: intent.task_id.clone(),
                    request_revision: intent.request_revision,
                    attempt_no: intent.attempt_no,
                    owner_epoch: 1,
                    execution_id: "execution-claim-1".into(),
                    outcome: OperationOutcomeV1::Succeeded,
                    actual_before_version: intent.base_target_version.clone(),
                    actual_after_version: Some("forged-known-result".into()),
                    exit_status: Some(0),
                    result_ref: Some("artifact:forged-known-result".into()),
                    external_receipt_ref: None,
                    reconciliation_ref: None,
                },
            }
        ),
        Err(TransitionErrorV1::InvalidOperation("stale_execution_claim"))
    );
    assert_eq!(taken_over.operation_refs[0].receipt, None);

    let known = finalize_operation_with_target(&state, "patch-recovery", "target-known");
    assert_eq!(
        apply(
            &known,
            HostEventV1::ObserveMissingOperation {
                operation_id: "patch-recovery".into(),
                observation_ref: "artifact:missing-observation-after-known".into(),
            }
        ),
        Err(TransitionErrorV1::OperationAlreadyFinalized)
    );
    assert_eq!(
        known.operation_refs[0].receipt.as_ref().unwrap().outcome,
        OperationOutcomeV1::Succeeded
    );
}

#[test]
fn reconciled_finalization_keeps_raw_unknown_receipt_and_separates_owner_provenance() {
    let state = reserved_native_operation();
    let state = apply(
        &state,
        HostEventV1::FinalizeOperation {
            receipt: OperationReceiptV1 {
                operation_id: "patch-recovery".into(),
                task_id: state.request.task_id.clone(),
                request_revision: state.request.request_revision,
                attempt_no: state.run_state.attempt_no,
                owner_epoch: state.run_state.owner_epoch,
                execution_id: "execution-claim-1".into(),
                outcome: OperationOutcomeV1::Unknown,
                actual_before_version: state.run_state.target_version.clone(),
                actual_after_version: None,
                exit_status: None,
                result_ref: None,
                external_receipt_ref: None,
                reconciliation_ref: None,
            },
        },
    )
    .unwrap();
    let raw_unknown = state.operation_refs[0].receipt.clone().unwrap();
    assert_eq!(state.operation_refs[0].effective_receipt(), None);

    let mut handoff = state;
    let prior_owner_id = handoff.run_state.owner_id.clone();
    let prior_owner_epoch = handoff.run_state.owner_epoch;
    let prior_state_version = handoff.run_state.state_version;
    handoff.run_state.owner_id = "owner-2".into();
    handoff.run_state.owner_epoch = 2;
    handoff.run_state.state_version += 1;
    handoff.owner_takeovers.push(OwnerTakeoverRecordV1 {
        previous_owner_id: prior_owner_id,
        new_owner_id: "owner-2".into(),
        previous_owner_epoch: prior_owner_epoch,
        new_owner_epoch: 2,
        expected_state_version: prior_state_version,
        takeover_proof_ref: "takeover:proof-2".into(),
    });
    let intent = &handoff.operation_refs[0].intent;
    let intent_operation_id = intent.operation_id.clone();
    let intent_digest = intent.content_digest().unwrap();
    let intent_task_id = intent.task_id.clone();
    let intent_request_revision = intent.request_revision;
    let intent_attempt_no = intent.attempt_no;
    let original_deadline = handoff.operation_refs[0].claims[0].deadline_at_ms;
    let finalizer_state_version = handoff.run_state.state_version;
    handoff.operation_refs[0].claims.push(OperationClaimV1 {
        claim_id: "finalizer-claim-2".into(),
        operation_id: intent_operation_id.clone(),
        previous_claim_id: Some("execution-claim-1".into()),
        owner_epoch: 2,
        expected_state_version: finalizer_state_version,
        intent_digest,
        claimed_at_ms: 2_000_000,
        deadline_at_ms: original_deadline,
        kind: OperationClaimKindV1::Finalizer,
        supersedes_execution_claim_id: None,
        native_executor_dead_proof_ref: None,
    });
    handoff.run_state.state_version += 1;
    let final_receipt = OperationReceiptV1 {
        operation_id: intent_operation_id.clone(),
        task_id: intent_task_id,
        request_revision: intent_request_revision,
        attempt_no: intent_attempt_no,
        owner_epoch: 1,
        execution_id: "execution-claim-1".into(),
        outcome: OperationOutcomeV1::Succeeded,
        actual_before_version: raw_unknown.actual_before_version.clone(),
        actual_after_version: Some("target-after-reconciliation".into()),
        exit_status: None,
        result_ref: Some("journal:local-observation".into()),
        external_receipt_ref: None,
        reconciliation_ref: Some("journal:reconciliation".into()),
    };
    let finalized = apply(
        &handoff,
        HostEventV1::FinalizeReconciledOperation {
            finalization: OperationFinalizationV1 {
                operation_id: intent_operation_id,
                finalizer_claim_id: "finalizer-claim-2".into(),
                execution_claim_id: "execution-claim-1".into(),
                receipt: final_receipt.clone(),
                reconciliation_ref: Some("journal:reconciliation".into()),
                source_observation_refs: vec!["journal:local-observation".into()],
            },
        },
    )
    .unwrap();
    let record = &finalized.operation_refs[0];
    assert_eq!(record.receipt.as_ref(), Some(&raw_unknown));
    assert_eq!(record.effective_receipt(), Some(&final_receipt));
    assert_eq!(
        record.finalizations[0].execution_claim_id,
        "execution-claim-1"
    );
    assert_eq!(
        record.finalizations[0].finalizer_claim_id,
        "finalizer-claim-2"
    );
    assert_ne!(record.claims[0].owner_epoch, record.claims[1].owner_epoch);
}

#[test]
fn settled_recovery_history_survives_a_later_owner_takeover() {
    let state = reserved_native_operation();
    let state = apply(
        &state,
        HostEventV1::FinalizeOperation {
            receipt: OperationReceiptV1 {
                operation_id: "patch-recovery".into(),
                task_id: state.request.task_id.clone(),
                request_revision: state.request.request_revision,
                attempt_no: state.run_state.attempt_no,
                owner_epoch: 1,
                execution_id: "execution-claim-1".into(),
                outcome: OperationOutcomeV1::Unknown,
                actual_before_version: state.run_state.target_version.clone(),
                actual_after_version: None,
                exit_status: None,
                result_ref: None,
                external_receipt_ref: None,
                reconciliation_ref: None,
            },
        },
    )
    .unwrap();
    let taken_over = apply(
        &state,
        HostEventV1::TakeOwnership {
            new_owner_id: "owner-2".into(),
            new_owner_epoch: 2,
            takeover_proof_ref: "takeover:proof-2".into(),
        },
    )
    .unwrap();
    let record = &taken_over.operation_refs[0];
    let with_finalizer = apply(
        &taken_over,
        HostEventV1::ClaimExistingOperation {
            operation_id: record.intent.operation_id.clone(),
            claim: OperationClaimV1 {
                claim_id: "finalizer-claim-2".into(),
                operation_id: record.intent.operation_id.clone(),
                previous_claim_id: Some(record.claims[0].claim_id.clone()),
                owner_epoch: 2,
                expected_state_version: taken_over.run_state.state_version,
                intent_digest: record.intent.content_digest().unwrap(),
                claimed_at_ms: 2_000_000,
                deadline_at_ms: record.claims[0].deadline_at_ms,
                kind: OperationClaimKindV1::Finalizer,
                supersedes_execution_claim_id: None,
                native_executor_dead_proof_ref: None,
            },
        },
    )
    .unwrap();
    let finalized = apply(
        &with_finalizer,
        HostEventV1::FinalizeReconciledOperation {
            finalization: OperationFinalizationV1 {
                operation_id: "patch-recovery".into(),
                finalizer_claim_id: "finalizer-claim-2".into(),
                execution_claim_id: "execution-claim-1".into(),
                receipt: OperationReceiptV1 {
                    operation_id: "patch-recovery".into(),
                    task_id: with_finalizer.request.task_id.clone(),
                    request_revision: with_finalizer.request.request_revision,
                    attempt_no: with_finalizer.run_state.attempt_no,
                    owner_epoch: 1,
                    execution_id: "execution-claim-1".into(),
                    outcome: OperationOutcomeV1::Succeeded,
                    actual_before_version: with_finalizer.run_state.target_version.clone(),
                    actual_after_version: Some("target-after-reconciliation".into()),
                    exit_status: None,
                    result_ref: Some("journal:local-observation".into()),
                    external_receipt_ref: None,
                    reconciliation_ref: Some("journal:reconciliation".into()),
                },
                reconciliation_ref: Some("journal:reconciliation".into()),
                source_observation_refs: vec!["journal:local-observation".into()],
            },
        },
    )
    .unwrap();
    let handed_off_again = apply(
        &finalized,
        HostEventV1::TakeOwnership {
            new_owner_id: "owner-3".into(),
            new_owner_epoch: 3,
            takeover_proof_ref: "takeover:proof-3".into(),
        },
    )
    .unwrap();
    assert_eq!(handed_off_again.run_state.owner_epoch, 3);
    assert_eq!(handed_off_again.operation_refs[0].finalizations.len(), 1);
}

#[test]
fn stale_finalizer_claim_cannot_settle_after_a_later_takeover() {
    let state = reserved_native_operation();
    let state = apply(
        &state,
        HostEventV1::FinalizeOperation {
            receipt: OperationReceiptV1 {
                operation_id: "patch-recovery".into(),
                task_id: state.request.task_id.clone(),
                request_revision: state.request.request_revision,
                attempt_no: state.run_state.attempt_no,
                owner_epoch: 1,
                execution_id: "execution-claim-1".into(),
                outcome: OperationOutcomeV1::Unknown,
                actual_before_version: state.run_state.target_version.clone(),
                actual_after_version: None,
                exit_status: None,
                result_ref: None,
                external_receipt_ref: None,
                reconciliation_ref: None,
            },
        },
    )
    .unwrap();
    let raw_unknown = state.operation_refs[0].receipt.clone().unwrap();
    let owner_two = apply(
        &state,
        HostEventV1::TakeOwnership {
            new_owner_id: "owner-2".into(),
            new_owner_epoch: 2,
            takeover_proof_ref: "takeover:proof-2".into(),
        },
    )
    .unwrap();
    let record = &owner_two.operation_refs[0];
    let with_old_finalizer = apply(
        &owner_two,
        HostEventV1::ClaimExistingOperation {
            operation_id: record.intent.operation_id.clone(),
            claim: OperationClaimV1 {
                claim_id: "finalizer-claim-2".into(),
                operation_id: record.intent.operation_id.clone(),
                previous_claim_id: Some(record.claims[0].claim_id.clone()),
                owner_epoch: 2,
                expected_state_version: owner_two.run_state.state_version,
                intent_digest: record.intent.content_digest().unwrap(),
                claimed_at_ms: 2_000_000,
                deadline_at_ms: record.claims[0].deadline_at_ms,
                kind: OperationClaimKindV1::Finalizer,
                supersedes_execution_claim_id: None,
                native_executor_dead_proof_ref: None,
            },
        },
    )
    .unwrap();
    let owner_three = apply(
        &with_old_finalizer,
        HostEventV1::TakeOwnership {
            new_owner_id: "owner-3".into(),
            new_owner_epoch: 3,
            takeover_proof_ref: "takeover:proof-3".into(),
        },
    )
    .unwrap();
    let result = apply(
        &owner_three,
        HostEventV1::FinalizeReconciledOperation {
            finalization: OperationFinalizationV1 {
                operation_id: "patch-recovery".into(),
                finalizer_claim_id: "finalizer-claim-2".into(),
                execution_claim_id: "execution-claim-1".into(),
                receipt: OperationReceiptV1 {
                    operation_id: "patch-recovery".into(),
                    task_id: owner_three.request.task_id.clone(),
                    request_revision: owner_three.request.request_revision,
                    attempt_no: owner_three.run_state.attempt_no,
                    owner_epoch: 1,
                    execution_id: "execution-claim-1".into(),
                    outcome: OperationOutcomeV1::Succeeded,
                    actual_before_version: raw_unknown.actual_before_version.clone(),
                    actual_after_version: Some("target-after-reconciliation".into()),
                    exit_status: None,
                    result_ref: Some("journal:local-observation".into()),
                    external_receipt_ref: None,
                    reconciliation_ref: Some("journal:reconciliation".into()),
                },
                reconciliation_ref: Some("journal:reconciliation".into()),
                source_observation_refs: vec!["journal:local-observation".into()],
            },
        },
    );
    assert!(matches!(
        result,
        Err(TransitionErrorV1::InvalidOperation(_))
    ));
    assert_eq!(
        owner_three.operation_refs[0].receipt.as_ref(),
        Some(&raw_unknown)
    );
    assert!(owner_three.operation_refs[0].finalizations.is_empty());
}

#[test]
fn owner_takeover_history_requires_monotonic_state_versions() {
    let state = start(&queued(3));
    let first = apply(
        &state,
        HostEventV1::TakeOwnership {
            new_owner_id: "owner-2".into(),
            new_owner_epoch: 2,
            takeover_proof_ref: "takeover:proof-2".into(),
        },
    )
    .unwrap();
    let mut second = apply(
        &first,
        HostEventV1::TakeOwnership {
            new_owner_id: "owner-3".into(),
            new_owner_epoch: 3,
            takeover_proof_ref: "takeover:proof-3".into(),
        },
    )
    .unwrap();
    second.owner_takeovers[1].expected_state_version =
        second.owner_takeovers[0].expected_state_version;
    assert!(second.validate(None).is_err());
}

#[test]
fn running_baseline_reproduction_evidence_is_partial_and_does_not_satisfy_condition() {
    let state = record_operation(
        &running_reproduce_failure(),
        "baseline",
        OperationOutcomeV1::Failed,
    );
    let evidence = negative_evidence(
        &state,
        "baseline-evidence",
        "baseline",
        ConditionVerdictV1::Fail,
        "baseline",
        None,
    );
    let state = apply(&state, HostEventV1::RecordEvidence { evidence }).unwrap();
    assert_eq!(state.run_state.status, TaskRunStatusV1::Running);
    assert!(
        state
            .run_state
            .unmet_conditions
            .contains(&"AC-1".to_owned())
    );
    assert_eq!(state.run_state.attempt_no, 1);
}

#[test]
fn running_baseline_verifier_error_is_recorded_without_negative_proof() {
    let state = record_operation(
        &running_reproduce_failure(),
        "baseline-compile-error",
        OperationOutcomeV1::Failed,
    );
    let error = verifier_error_evidence(
        &state,
        "baseline-compile-error-evidence",
        "baseline-compile-error",
    );
    let state = apply(&state, HostEventV1::RecordEvidence { evidence: error }).unwrap();
    let recorded = state.evidence_refs.last().expect("recorded verifier error");
    assert_eq!(recorded.verdict, ConditionVerdictV1::Error);
    assert!(recorded.host_negative_proof.is_none());
    assert_eq!(state.run_state.status, TaskRunStatusV1::Running);
    assert_eq!(state.run_state.attempt_no, 1);
    assert!(state.run_state.unmet_conditions.contains(&"AC-1".into()));
    let verifying = apply(
        &state,
        HostEventV1::BeginVerification {
            target_version: state.run_state.target_version.clone(),
        },
    )
    .unwrap();
    assert!(
        apply(
            &verifying,
            HostEventV1::Complete {
                target_version: verifying.run_state.target_version.clone(),
            }
        )
        .is_err()
    );
}

#[test]
fn running_baseline_verifier_error_rejects_unbound_receipt_or_negative_proof() {
    let baseline = running_reproduce_failure();

    let missing_operation =
        record_operation(&baseline, "different-operation", OperationOutcomeV1::Failed);
    let wrong_operation = evidence(
        &missing_operation,
        "wrong-operation-error",
        "unregistered-operation",
        ConditionVerdictV1::Error,
    );
    assert!(
        apply(
            &missing_operation,
            HostEventV1::RecordEvidence {
                evidence: wrong_operation,
            }
        )
        .is_err()
    );

    let external = reserve_operation(
        &baseline,
        "external-action-error",
        OperationKindV1::ExternalAction,
    );
    let intent = external
        .operation_refs
        .iter()
        .find(|record| record.intent.operation_id == "external-action-error")
        .unwrap()
        .intent
        .clone();
    let external = apply(
        &external,
        HostEventV1::FinalizeOperation {
            receipt: OperationReceiptV1 {
                operation_id: intent.operation_id.clone(),
                task_id: intent.task_id.clone(),
                request_revision: intent.request_revision,
                attempt_no: intent.attempt_no,
                owner_epoch: intent.owner_epoch,
                execution_id: "execution:external-action-error".into(),
                outcome: OperationOutcomeV1::Failed,
                actual_before_version: intent.base_target_version.clone(),
                actual_after_version: Some(intent.base_target_version),
                exit_status: Some(1),
                result_ref: Some("artifact:external-action-error".into()),
                external_receipt_ref: None,
                reconciliation_ref: None,
            },
        },
    )
    .unwrap();
    let external_error = verifier_error_evidence(
        &external,
        "external-action-error-evidence",
        "external-action-error",
    );
    assert!(
        apply(
            &external,
            HostEventV1::RecordEvidence {
                evidence: external_error,
            }
        )
        .is_err()
    );

    let succeeded = record_operation(
        &baseline,
        "succeeded-operation",
        OperationOutcomeV1::Succeeded,
    );
    let wrong_outcome =
        verifier_error_evidence(&succeeded, "wrong-outcome-error", "succeeded-operation");
    assert!(
        apply(
            &succeeded,
            HostEventV1::RecordEvidence {
                evidence: wrong_outcome,
            }
        )
        .is_err()
    );

    let failed = record_operation(&baseline, "proof-bearing-error", OperationOutcomeV1::Failed);
    let mut proof_bearing_error = verifier_error_evidence(
        &failed,
        "proof-bearing-error-evidence",
        "proof-bearing-error",
    );
    proof_bearing_error.host_negative_proof = Some(HostNegativeProofV1 {
        baseline_receipt_ref: "proof-bearing-error".into(),
        expected_failure_identity: "TestRegression: assertion mismatch".into(),
        observed_failure_identity: "TestRegression: assertion mismatch".into(),
        resolution_receipt_ref: None,
    });
    assert!(
        apply(
            &failed,
            HostEventV1::RecordEvidence {
                evidence: proof_bearing_error,
            }
        )
        .is_err()
    );

    let shifted = apply(
        &baseline,
        HostEventV1::InvalidateEvidence {
            evidence_ids: vec![],
            new_target_version: "target-2".into(),
        },
    )
    .unwrap();
    let shifted = record_operation(
        &shifted,
        "wrong-target-operation",
        OperationOutcomeV1::Failed,
    );
    let wrong_target =
        verifier_error_evidence(&shifted, "wrong-target-error", "wrong-target-operation");
    assert!(
        apply(
            &shifted,
            HostEventV1::RecordEvidence {
                evidence: wrong_target,
            }
        )
        .is_err()
    );
}

#[test]
fn completion_requires_resolution_receipt_for_the_current_candidate_evidence() {
    let mut state = record_operation(
        &running_reproduce_failure(),
        "baseline",
        OperationOutcomeV1::Failed,
    );
    let baseline_evidence = negative_evidence(
        &state,
        "baseline-evidence",
        "baseline",
        ConditionVerdictV1::Fail,
        "baseline",
        None,
    );
    state.evidence_refs.push(baseline_evidence);
    state.validate(None).unwrap();
    let state = apply(
        &state,
        HostEventV1::InvalidateEvidence {
            evidence_ids: vec!["baseline-evidence".into()],
            new_target_version: "target-2".into(),
        },
    )
    .unwrap();
    let state = record_operation(&state, "old-candidate", OperationOutcomeV1::Succeeded);
    let state = record_operation(&state, "candidate-current", OperationOutcomeV1::Succeeded);
    let state = verifying(&state);
    let evidence = negative_evidence(
        &state,
        "candidate-evidence",
        "candidate-current",
        ConditionVerdictV1::Pass,
        "baseline",
        Some("old-candidate"),
    );
    let state = apply(&state, HostEventV1::RecordEvidence { evidence }).unwrap();
    assert!(
        apply(
            &state,
            HostEventV1::Complete {
                target_version: state.run_state.target_version.clone(),
            }
        )
        .is_err()
    );
}

#[test]
fn completion_requires_baseline_failure_at_the_request_start_target() {
    let state = apply(
        &running_reproduce_failure(),
        HostEventV1::InvalidateEvidence {
            evidence_ids: vec![],
            new_target_version: "target-2".into(),
        },
    )
    .unwrap();
    let state = record_operation(&state, "wrong-baseline", OperationOutcomeV1::Failed);
    let state = record_operation(&state, "candidate-current", OperationOutcomeV1::Succeeded);
    let state = verifying(&state);
    let evidence = negative_evidence(
        &state,
        "candidate-evidence",
        "candidate-current",
        ConditionVerdictV1::Pass,
        "wrong-baseline",
        Some("candidate-current"),
    );
    let state = apply(&state, HostEventV1::RecordEvidence { evidence }).unwrap();
    assert!(
        apply(
            &state,
            HostEventV1::Complete {
                target_version: state.run_state.target_version.clone(),
            }
        )
        .is_err()
    );
}

#[test]
fn reconciled_resume_advances_target_after_real_finalization_without_new_attempt() {
    let state = blocked_reconciled_patch(false);
    let attempt_no = state.run_state.attempt_no;
    let resumed = apply(
        &state,
        HostEventV1::ResumeReconciled {
            operation_id: "patch-recovery".into(),
            target_version: "target-after-patch".into(),
        },
    )
    .unwrap();
    assert_eq!(resumed.run_state.status, TaskRunStatusV1::Running);
    assert_eq!(resumed.run_state.phase, TaskRunPhaseV1::Apply);
    assert_eq!(resumed.run_state.target_version, "target-after-patch");
    assert_eq!(resumed.run_state.attempt_no, attempt_no);
    assert!(
        resumed
            .evidence_refs
            .iter()
            .all(|evidence| evidence.lifecycle == EvidenceLifecycleV1::Stale)
    );
    assert!(
        resumed
            .run_state
            .unmet_conditions
            .contains(&"AC-1".to_owned())
    );
}

#[test]
fn reconciled_resume_rejects_unproven_external_verifier_and_unsettled_work() {
    for (operation_id, kind) in [
        ("unknown-external", OperationKindV1::ExternalAction),
        ("unknown-verifier", OperationKindV1::RunVerifier),
    ] {
        let state = blocked_unsettled_operation(operation_id, kind);
        assert!(
            apply(
                &state,
                HostEventV1::ResumeReconciled {
                    operation_id: operation_id.into(),
                    target_version: "target-after-patch".into(),
                },
            )
            .is_err()
        );
    }

    let state = blocked_reconciled_patch(true);
    assert!(
        apply(
            &state,
            HostEventV1::ResumeReconciled {
                operation_id: "patch-recovery".into(),
                target_version: "target-after-patch".into(),
            },
        )
        .is_err()
    );
}

#[test]
fn reconciled_resume_rejects_wrong_target_and_stale_fence() {
    let state = blocked_reconciled_patch(false);
    assert!(
        apply(
            &state,
            HostEventV1::ResumeReconciled {
                operation_id: "different-operation".into(),
                target_version: "target-after-patch".into(),
            },
        )
        .is_err()
    );
    assert!(
        apply(
            &state,
            HostEventV1::ResumeReconciled {
                operation_id: "patch-recovery".into(),
                target_version: "forged-target".into(),
            },
        )
        .is_err()
    );

    let stale_fence = StateFenceV1 {
        expected_state_version: state.run_state.state_version - 1,
        request_revision: state.request.request_revision,
        owner_epoch: state.run_state.owner_epoch,
    };
    assert_eq!(
        state.apply_host_event(
            stale_fence,
            HostEventV1::ResumeReconciled {
                operation_id: "patch-recovery".into(),
                target_version: "target-after-patch".into(),
            }
        ),
        Err(TransitionErrorV1::FenceMismatch)
    );
}

#[test]
fn completion_requires_current_host_pass_evidence() {
    let state = verifying(&record_operation(
        &start(&queued(3)),
        "verify-incomplete",
        OperationOutcomeV1::Succeeded,
    ));
    assert_eq!(
        apply(
            &state,
            HostEventV1::Complete {
                target_version: state.run_state.target_version.clone(),
            }
        ),
        Err(TransitionErrorV1::CannotComplete(
            "required_evidence_missing"
        ))
    );
}
