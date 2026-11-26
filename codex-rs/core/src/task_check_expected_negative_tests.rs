// Modified by RenCrow Switch Core, 2026-11-26: verify host-bound expected failure resolution.
use super::*;
use crate::state::ActiveTaskScope;
use crate::task_check::TaskArtifactStamp;
use crate::task_check::TaskCheckExecution;
use crate::task_check::TaskCheckKind;
use crate::task_check::TaskCheckObservation;
use crate::task_check::TaskEvidenceRequest;

struct PendingFixture {
    _workspace: tempfile::TempDir,
    ledger: TaskCheckLedger,
    binding: ExpectedNegativeBindingV1,
    stamps: Vec<TaskArtifactStamp>,
    observation: TaskCheckObservation,
    execution: TaskCheckExecution,
}

impl PendingFixture {
    fn new() -> Self {
        let workspace = tempfile::tempdir().expect("workspace");
        let artifact = std::path::PathBuf::from("regression_test.go");
        std::fs::write(workspace.path().join(&artifact), "package regression\n")
            .expect("baseline artifact");
        let scope = ActiveTaskScope {
            root_task_id: "root-v1".into(),
            task_id: "task-a".into(),
            task_revision: 1,
            begin_call_id: "begin-v1".into(),
            start_history_cursor: 0,
        };
        let request = TaskEvidenceRequest {
            task_id: scope.task_id.clone(),
            task_revision: scope.task_revision,
            artifact_paths: vec![artifact.clone()],
            check_kind: TaskCheckKind::Verification,
        };
        let mut execution =
            TaskCheckExecution::new(request.clone(), workspace.path()).expect("bounded execution");
        let obligation = TaskCheckObligation::new(
            scope,
            execution.workspace_root.clone(),
            "go test -json -count=1 -run '^TestRegression$' .",
            vec![artifact],
        )
        .expect("obligation");
        execution.obligation = Some(obligation.clone());
        let command = [
            "go",
            "test",
            "-json",
            "-count=1",
            "-run",
            "^TestRegression$",
            ".",
        ]
        .map(str::to_owned);
        let observation = TaskCheckObservation::from_execution(
            "baseline-call",
            &command,
            workspace.path(),
            "TestRegression: expected 4, got 3\n",
            /*exit_code*/ 1,
            /*completed*/ true,
            /*producer_truncated*/ false,
        );
        let mut ledger = TaskCheckLedger::default();
        ledger.begin_with_obligation(
            "baseline-call".into(),
            request,
            execution.workspace_root.clone(),
            execution.before.clone(),
            Some(obligation.clone()),
        );
        Self {
            _workspace: workspace,
            ledger,
            binding: ExpectedNegativeBindingV1 {
                source_call_id: "baseline-call".into(),
                obligation,
                condition_id: "AC-reproduce".into(),
                condition_revision: 1,
                condition_set_revision: 1,
                baseline_version: "a".repeat(64),
                expected_outcome_digest: "b".repeat(64),
                expected_failure_identity: "TestRegression: expected 4, got 3".into(),
            },
            stamps: execution.before.clone(),
            observation,
            execution,
        }
    }

    fn finish_failure(&mut self) {
        self.ledger
            .finish_observed(
                "baseline-call",
                /*exit_code*/ 1,
                /*completed*/ true,
                self.stamps.clone(),
                Some(self.observation.clone()),
            )
            .expect("raw failing receipt");
    }

    fn resolution(&self) -> ExpectedNegativeResolutionV1 {
        ExpectedNegativeResolutionV1 {
            observed_failure_identity: self.binding.expected_failure_identity.clone(),
            evidence_ref: "host-evidence:baseline-call".into(),
            raw_output_sha256: self.observation.diagnostic_sha256.clone(),
        }
    }
}

#[test]
fn pending_execution_requires_exact_scope_and_before_images() {
    let mut fixture = PendingFixture::new();
    assert!(
        fixture
            .ledger
            .pending_execution_matches("baseline-call", &fixture.execution)
    );
    assert!(
        !fixture
            .ledger
            .pending_execution_matches("different-call", &fixture.execution)
    );
    let mut different = fixture.execution.clone();
    different.before[0].sha256 = "c".repeat(64);
    assert!(
        !fixture
            .ledger
            .pending_execution_matches("baseline-call", &different)
    );
    let mut different = fixture.execution.clone();
    different.request.task_revision += 1;
    assert!(
        !fixture
            .ledger
            .pending_execution_matches("baseline-call", &different)
    );
    fixture.finish_failure();
    assert!(
        !fixture
            .ledger
            .pending_execution_matches("baseline-call", &fixture.execution)
    );
}

#[test]
fn reservation_requires_pending_source_and_exact_obligation() {
    let mut fixture = PendingFixture::new();
    let mut wrong = fixture.binding.clone();
    wrong.obligation.command_identity = "a different command".into();
    assert!(fixture.ledger.reserve_expected_negative(wrong).is_err());
    fixture
        .ledger
        .reserve_expected_negative(fixture.binding.clone())
        .expect("reserve matching pending check");
    fixture
        .ledger
        .reserve_expected_negative(fixture.binding.clone())
        .expect("identical reservation is idempotent");
    let mut changed = fixture.binding.clone();
    changed.condition_revision += 1;
    assert!(fixture.ledger.reserve_expected_negative(changed).is_err());
    fixture.finish_failure();
    let mut unreserved = PendingFixture::new();
    unreserved.finish_failure();
    assert!(
        unreserved
            .ledger
            .reserve_expected_negative(unreserved.binding)
            .is_err()
    );
}

#[test]
fn resolution_requires_matching_raw_failure_and_keeps_later_failure_unresolved() {
    let mut fixture = PendingFixture::new();
    fixture
        .ledger
        .reserve_expected_negative(fixture.binding.clone())
        .expect("reserve matching pending check");
    let resolution = fixture.resolution();
    assert!(
        fixture
            .ledger
            .resolve_expected_negative(&fixture.binding, resolution)
            .is_err()
    );
    fixture.finish_failure();
    let original = fixture
        .ledger
        .get("baseline-call")
        .expect("raw receipt")
        .clone();
    let mut wrong = fixture.resolution();
    wrong.observed_failure_identity = "another failure".into();
    assert!(
        fixture
            .ledger
            .resolve_expected_negative(&fixture.binding, wrong)
            .is_err()
    );
    let mut wrong = fixture.resolution();
    wrong.raw_output_sha256 = "c".repeat(64);
    assert!(
        fixture
            .ledger
            .resolve_expected_negative(&fixture.binding, wrong)
            .is_err()
    );
    let resolution = fixture.resolution();
    fixture
        .ledger
        .resolve_expected_negative(&fixture.binding, resolution)
        .expect("resolve only the matching reproduction obligation");
    assert_eq!(fixture.ledger.get("baseline-call"), Some(&original));
    assert_eq!(
        fixture
            .ledger
            .unresolved_count_for_scope(&fixture.binding.obligation.scope),
        0
    );
    let scope = fixture.binding.obligation.scope.clone();
    fixture.ledger.begin_with_obligation(
        "later-candidate-failure".into(),
        TaskEvidenceRequest {
            task_id: scope.task_id.clone(),
            task_revision: scope.task_revision,
            artifact_paths: fixture.binding.obligation.artifact_paths.clone(),
            check_kind: TaskCheckKind::Verification,
        },
        fixture.binding.obligation.workspace_root.clone(),
        fixture.stamps.clone(),
        Some(fixture.binding.obligation.clone()),
    );
    fixture
        .ledger
        .finish(
            "later-candidate-failure",
            /*exit_code*/ 1,
            /*completed*/ true,
            fixture.stamps,
        )
        .expect("later raw failure");
    assert_eq!(fixture.ledger.unresolved_count_for_scope(&scope), 1);
    let snapshot = serde_json::to_value(fixture.ledger.snapshot()).expect("snapshot");
    let mut restored = TaskCheckLedger::default();
    restored
        .restore_snapshot(serde_json::from_value(snapshot).expect("typed snapshot"))
        .expect("cold restore");
    assert_eq!(restored.unresolved_count_for_scope(&scope), 1);
    assert_eq!(restored.get("baseline-call"), Some(&original));
}

#[test]
fn expected_negative_call_replay_preserves_the_original_raw_receipt() {
    let mut fixture = PendingFixture::new();
    fixture
        .ledger
        .reserve_expected_negative(fixture.binding.clone())
        .expect("reserve");
    fixture.finish_failure();
    let resolution = fixture.resolution();
    fixture
        .ledger
        .resolve_expected_negative(&fixture.binding, resolution)
        .expect("resolve");
    let original = serde_json::to_value(fixture.ledger.snapshot()).expect("original host snapshot");
    fixture.ledger.begin_with_obligation(
        fixture.binding.source_call_id.clone(),
        fixture.execution.request.clone(),
        fixture.execution.workspace_root.clone(),
        fixture.execution.before.clone(),
        fixture.execution.obligation.clone(),
    );
    assert_eq!(
        serde_json::to_value(fixture.ledger.snapshot()).unwrap(),
        original
    );
}

#[test]
fn expected_negative_guard_projection_names_only_the_resolved_call() {
    let mut fixture = PendingFixture::new();
    fixture
        .ledger
        .reserve_expected_negative(fixture.binding.clone())
        .expect("reserve");
    fixture.finish_failure();
    assert!(fixture.ledger.resolved_call_guard_keys().is_empty());
    let resolution = fixture.resolution();
    fixture
        .ledger
        .resolve_expected_negative(&fixture.binding, resolution)
        .expect("resolve");
    assert_eq!(
        fixture.ledger.resolved_call_guard_keys(),
        vec![format!(
            "{}:call:baseline-call",
            fixture.binding.obligation.key()
        )]
    );
    let mut restored = TaskCheckLedger::default();
    restored
        .restore_snapshot(fixture.ledger.snapshot())
        .expect("restore");
    assert_eq!(
        restored.resolved_call_guard_keys(),
        fixture.ledger.resolved_call_guard_keys()
    );
}

#[test]
fn expected_negative_guard_projection_includes_candidate_failures_resolved_by_later_pass() {
    let mut fixture = PendingFixture::new();
    for (call_id, exit_code) in [("candidate-failed", 1), ("candidate-passed", 0)] {
        fixture.ledger.begin_with_obligation(
            call_id.into(),
            fixture.execution.request.clone(),
            fixture.execution.workspace_root.clone(),
            fixture.stamps.clone(),
            fixture.execution.obligation.clone(),
        );
        fixture
            .ledger
            .finish(
                call_id,
                exit_code,
                /*completed*/ true,
                fixture.stamps.clone(),
            )
            .expect("raw candidate result");
    }
    let expected = vec![format!(
        "{}:call:candidate-failed",
        fixture.binding.obligation.key()
    )];
    assert_eq!(fixture.ledger.resolved_call_guard_keys(), expected);
    let referenced = std::collections::HashSet::from(["candidate-failed".to_string()]);
    let mut restored = TaskCheckLedger::default();
    restored
        .restore_snapshot(fixture.ledger.snapshot_with_references(&referenced))
        .expect("restore host-referenced calls");
    assert_eq!(restored.resolved_call_guard_keys(), expected);
}

#[test]
fn expected_negative_restore_rejects_mismatched_resolution_atomically() {
    let mut fixture = PendingFixture::new();
    fixture
        .ledger
        .reserve_expected_negative(fixture.binding.clone())
        .expect("reserve");
    fixture.finish_failure();
    let resolution = fixture.resolution();
    fixture
        .ledger
        .resolve_expected_negative(&fixture.binding, resolution)
        .expect("resolve");
    let original = serde_json::to_value(fixture.ledger.snapshot()).expect("original host snapshot");
    let mut corrupted = original.clone();
    corrupted[0]["expected_negative"]["resolution"]["raw_output_sha256"] =
        serde_json::json!("c".repeat(64));
    let corrupted = serde_json::from_value(corrupted).expect("typed modified resolution");
    assert!(fixture.ledger.restore_snapshot(corrupted).is_err());
    assert_eq!(
        serde_json::to_value(fixture.ledger.snapshot()).unwrap(),
        original
    );
}
