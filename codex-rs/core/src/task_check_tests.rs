// Modified by RenCrow Switch Core, 2026-11-16.
use super::*;

#[test]
fn cold_ledger_retains_latest_unknown_attempt_instead_of_older_success() {
    let stamp = TaskArtifactStamp {
        path: "result.txt".into(),
        sha256: "abc".into(),
    };
    let mut original = TaskCheckLedger::default();
    original.begin(
        "pass".into(),
        request_for(1, &stamp.path),
        "/w".into(),
        vec![stamp.clone()],
    );
    original
        .finish("pass", 0, true, vec![stamp.clone()])
        .unwrap();
    original.begin(
        "unknown".into(),
        request_for(1, &stamp.path),
        "/w".into(),
        vec![stamp],
    );
    let wire = serde_json::to_string(&original.snapshot()).unwrap();
    let mut restored = TaskCheckLedger::default();
    restored
        .restore_snapshot(serde_json::from_str(&wire).unwrap())
        .unwrap();
    let latest = restored.latest_for_task("task-a", 1).unwrap();
    assert_eq!(latest.call_id, "unknown");
    assert!(matches!(latest.status, TaskCheckAttemptStatus::Pending));
    assert!(restored.get("pass").is_none());
    assert!(restored.finish("unknown", 0, true, vec![]).is_none());
}

#[test]
fn cold_ledger_preserves_failed_and_successful_receipts_and_rejects_mismatch() {
    let stamp = TaskArtifactStamp {
        path: "result.txt".into(),
        sha256: "abc".into(),
    };
    let mut original = TaskCheckLedger::default();
    for (revision, exit) in [(1, 0), (2, 1)] {
        let call = format!("check-{revision}");
        original.begin(
            call.clone(),
            request_for(revision, &stamp.path),
            "/w".into(),
            vec![stamp.clone()],
        );
        original
            .finish(&call, exit, true, vec![stamp.clone()])
            .unwrap();
    }
    let wire = serde_json::to_string(&original.snapshot()).unwrap();
    let mut restored = TaskCheckLedger::default();
    restored
        .restore_snapshot(serde_json::from_str(&wire).unwrap())
        .unwrap();
    assert!(restored.get("check-1").unwrap().succeeded);
    assert!(!restored.get("check-2").unwrap().succeeded);
    let mut invalid = original.snapshot();
    invalid[0].task_id = "another-task".into();
    assert!(restored.restore_snapshot(invalid).is_err());
    assert!(
        restored.get("check-1").unwrap().succeeded,
        "failed restore must be atomic"
    );
}

#[test]
fn ledger_only_accepts_unchanged_artifacts_and_success_status() {
    let stamp = TaskArtifactStamp {
        path: "src/lib.rs".into(),
        sha256: "abc".into(),
    };
    let mut ledger = TaskCheckLedger::default();
    ledger.begin(
        "call-1".into(),
        TaskEvidenceRequest {
            task_id: "task-a".into(),
            task_revision: 3,
            artifact_paths: vec![stamp.path.clone()],
            check_kind: TaskCheckKind::Verification,
        },
        "/workspace".into(),
        vec![stamp.clone()],
    );
    assert!(matches!(
        &ledger.latest_for_task("task-a", 3).unwrap().status,
        TaskCheckAttemptStatus::Pending
    ));
    let succeeded = ledger
        .finish("call-1", 0, true, vec![stamp.clone()])
        .unwrap();
    assert!(succeeded.succeeded);
    assert_eq!(succeeded.workspace_root, PathBuf::from("/workspace"));
    ledger.begin(
        "call-2".into(),
        TaskEvidenceRequest {
            task_id: "task-a".into(),
            task_revision: 3,
            artifact_paths: vec![stamp.path.clone()],
            check_kind: TaskCheckKind::Verification,
        },
        "/workspace".into(),
        vec![stamp.clone()],
    );
    let failed = ledger.finish("call-2", 1, true, vec![stamp]).unwrap();
    assert!(!failed.succeeded);
}

#[test]
fn host_observation_classifies_only_complete_exact_go_module_launch_failure() {
    let command = vec![
        "/bin/zsh".to_string(),
        "-lc".to_string(),
        "go vet ./internal/portal".to_string(),
    ];
    let diagnostic = "go: go.mod file not found in current directory or any parent directory; see 'go help modules'\n";
    let cwd = std::env::current_dir().unwrap();
    let observation = TaskCheckObservation::from_execution(
        "call-failed",
        &command,
        &cwd,
        diagnostic,
        1,
        true,
        false,
    );
    assert_eq!(
        observation.launch_failure,
        Some(TaskCheckLaunchFailure::GoModuleDiscovery)
    );
    assert!(observation.diagnostic_complete);
    assert!(!observation.diagnostic_truncated);

    let truncated = TaskCheckObservation::from_execution(
        "call-truncated",
        &command,
        &cwd,
        diagnostic,
        1,
        true,
        true,
    );
    assert_eq!(truncated.launch_failure, None);
    assert!(!truncated.diagnostic_complete);
    assert!(truncated.diagnostic_truncated);

    let ordinary_failure = TaskCheckObservation::from_execution(
        "call-code-failure",
        &command,
        &cwd,
        "internal/portal: vet: found code issue\n",
        1,
        true,
        false,
    );
    assert_eq!(ordinary_failure.launch_failure, None);
}

#[test]
fn old_task_check_receipt_serialization_omits_new_observation_field() {
    let stamp = TaskArtifactStamp {
        path: "result.txt".into(),
        sha256: "abc".into(),
    };
    let mut ledger = TaskCheckLedger::default();
    ledger.begin(
        "legacy".into(),
        request_for(1, &stamp.path),
        "/w".into(),
        vec![stamp.clone()],
    );
    ledger.finish("legacy", 0, true, vec![stamp]).unwrap();
    let wire = serde_json::to_value(ledger.snapshot()).unwrap();
    assert!(
        wire[0]["attempt"]["status"]["Finished"]
            .get("observation")
            .is_none()
    );
}

#[test]
fn live_launch_correction_retains_failure_and_imports_only_matching_success() {
    let temp = tempfile::tempdir().expect("workspace");
    let source_root = temp.path().canonicalize().expect("canonical workspace");
    let target_root = source_root.join("RenCrow_PORTAL");
    let package_root = target_root.join("internal/portal");
    std::fs::create_dir_all(&package_root).unwrap();
    for name in [
        "dev_story_player_action.go",
        "dev_story_player_test.go",
        "server.go",
    ] {
        std::fs::write(package_root.join(name), "fixture").unwrap();
    }
    let source_paths = [
        "RenCrow_PORTAL/internal/portal/dev_story_player_action.go",
        "RenCrow_PORTAL/internal/portal/dev_story_player_test.go",
        "RenCrow_PORTAL/internal/portal/server.go",
    ]
    .map(PathBuf::from);
    let target_paths = [
        "internal/portal/dev_story_player_action.go",
        "internal/portal/dev_story_player_test.go",
        "internal/portal/server.go",
    ]
    .map(PathBuf::from);
    let stamps = |paths: &[PathBuf]| {
        paths
            .iter()
            .map(|path| TaskArtifactStamp {
                path: path.clone(),
                sha256: "same-artifact-hash".into(),
            })
            .collect::<Vec<_>>()
    };
    let scope = crate::state::ActiveTaskScope {
        root_task_id: "root".into(),
        task_id: "task-a".into(),
        task_revision: 1,
        begin_call_id: "begin-a".into(),
        start_history_cursor: 5,
    };
    let command = vec![
        "/bin/zsh".to_string(),
        "-lc".to_string(),
        "go vet ./internal/portal".to_string(),
    ];
    let source_obligation = TaskCheckObligation::new(
        scope.clone(),
        source_root.clone(),
        "go vet ./internal/portal",
        source_paths.to_vec(),
    )
    .unwrap();
    let source_obligation_key = source_obligation.key();
    let source_stamps = stamps(&source_paths);
    let mut ledger = TaskCheckLedger::default();
    ledger.begin_with_obligation(
        "failed-launch".into(),
        TaskEvidenceRequest {
            task_id: "task-a".into(),
            task_revision: 1,
            artifact_paths: source_paths.to_vec(),
            check_kind: TaskCheckKind::Verification,
        },
        source_root.clone(),
        source_stamps.clone(),
        Some(source_obligation),
    );
    ledger.observe_terminal_event("failed-launch");
    let diagnostic = "go: go.mod file not found in current directory or any parent directory; see 'go help modules'\n";
    ledger
        .finish_observed(
            "failed-launch",
            1,
            true,
            source_stamps,
            Some(TaskCheckObservation::from_execution(
                "failed-launch",
                &command,
                &source_root,
                diagnostic,
                1,
                true,
                false,
            )),
        )
        .unwrap();

    let target_obligation = TaskCheckObligation::new(
        scope.clone(),
        target_root.clone(),
        "go vet ./internal/portal",
        target_paths.to_vec(),
    )
    .unwrap();
    let target_stamps = stamps(&target_paths);
    ledger.begin_with_obligation(
        "corrected-success".into(),
        TaskEvidenceRequest {
            task_id: "task-a".into(),
            task_revision: 1,
            artifact_paths: target_paths.to_vec(),
            check_kind: TaskCheckKind::Verification,
        },
        target_root.clone(),
        target_stamps.clone(),
        Some(target_obligation),
    );
    ledger.observe_terminal_event("corrected-success");
    let target_observation = TaskCheckObservation::from_execution(
        "corrected-success",
        &command,
        &target_root,
        "",
        0,
        true,
        false,
    );
    ledger
        .finish_observed(
            "corrected-success",
            0,
            true,
            target_stamps,
            Some(target_observation),
        )
        .unwrap();
    assert_eq!(
        ledger.resolved_launch_obligation_keys(),
        vec![source_obligation_key.clone()]
    );
    assert!(
        ledger
            .get("corrected-success")
            .unwrap()
            .observation
            .is_none()
    );
    let snapshot = ledger.snapshot();
    assert_eq!(snapshot.len(), 2);
    assert_eq!(
        ledger
            .latest_for_task("task-a", 1)
            .map(|attempt| attempt.call_id.as_str()),
        Some("corrected-success")
    );
    let mut restored = TaskCheckLedger::default();
    restored.restore_snapshot(snapshot.clone()).unwrap();
    assert_eq!(
        restored.resolved_launch_obligation_keys(),
        vec![source_obligation_key]
    );
    assert!(
        restored
            .unresolved_obligations(&crate::state::ActiveTaskScope {
                root_task_id: "root".into(),
                task_id: "task-a".into(),
                task_revision: 1,
                begin_call_id: "begin-a".into(),
                start_history_cursor: 99,
            })
            .is_empty()
    );
    assert_eq!(restored.snapshot().len(), 2);

    for newer_state in ["pending", "ordinary_failure", "changed_artifacts"] {
        let mut conflicted = TaskCheckLedger::default();
        conflicted.restore_snapshot(snapshot.clone()).unwrap();
        let newer_obligation = TaskCheckObligation::new(
            scope.clone(),
            source_root.clone(),
            "go vet ./internal/portal",
            source_paths.to_vec(),
        )
        .unwrap();
        let newer_stamps = stamps(&source_paths);
        conflicted.begin_with_obligation(
            format!("newer-{newer_state}"),
            TaskEvidenceRequest {
                task_id: "task-a".into(),
                task_revision: 1,
                artifact_paths: source_paths.to_vec(),
                check_kind: TaskCheckKind::Verification,
            },
            source_root.clone(),
            newer_stamps.clone(),
            Some(newer_obligation),
        );
        if newer_state == "ordinary_failure" {
            conflicted
                .finish_observed(&format!("newer-{newer_state}"), 1, true, newer_stamps, None)
                .unwrap();
        } else if newer_state == "changed_artifacts" {
            assert!(
                conflicted
                    .finish_observed(&format!("newer-{newer_state}"), 1, true, Vec::new(), None,)
                    .is_none()
            );
        }
        assert!(
            conflicted.resolved_launch_obligation_keys().is_empty(),
            "an older correction cannot settle the newer same-key {newer_state} attempt"
        );
    }
}

#[test]
fn diagnostics_prioritize_unresolved_failure_and_bound_total_status_payload() {
    let root = std::env::current_dir().unwrap();
    let scope = crate::state::ActiveTaskScope {
        root_task_id: "root".into(),
        task_id: "critical-task".into(),
        task_revision: 1,
        begin_call_id: "begin-critical".into(),
        start_history_cursor: 0,
    };
    let path = PathBuf::from(format!("{}.go", "p".repeat(3000)));
    let command = format!("go vet {}", "x".repeat(3000));
    let obligation =
        TaskCheckObligation::new(scope, root.clone(), &command, vec![path.clone()]).unwrap();
    let stamp = TaskArtifactStamp {
        path: path.clone(),
        sha256: "artifact-hash".into(),
    };
    let mut ledger = TaskCheckLedger::default();
    ledger.begin_with_obligation(
        "call-critical".into(),
        TaskEvidenceRequest {
            task_id: "critical-task".into(),
            task_revision: 1,
            artifact_paths: vec![path],
            check_kind: TaskCheckKind::Verification,
        },
        root.clone(),
        vec![stamp.clone()],
        Some(obligation),
    );
    ledger
        .finish_observed(
            "call-critical",
            1,
            true,
            vec![stamp],
            Some(TaskCheckObservation::from_execution(
                "call-critical",
                &["/bin/zsh".into(), "-lc".into(), command],
                &root,
                &"diagnostic".repeat(4000),
                1,
                true,
                false,
            )),
        )
        .unwrap();
    let diagnostics = ledger.diagnostics_for_root("root");
    let serialized = serde_json::to_vec(&diagnostics).unwrap();
    assert!(serialized.len() <= 8 * 1024);
    assert_eq!(diagnostics["count"], 1);
    assert_eq!(diagnostics["unresolved_count"], 1);
    assert_eq!(diagnostics["attempts"][0]["call_id"], "call-critical");
    assert_eq!(diagnostics["attempts"][0]["command_identity_omitted"], true);
    assert_eq!(diagnostics["attempts"][0]["artifact_paths_omitted"], true);
    assert_eq!(diagnostics["attempts"][0]["diagnostic"]["truncated"], true);
}

fn request_for(task_revision: u64, path: &Path) -> TaskEvidenceRequest {
    TaskEvidenceRequest {
        task_id: "task-a".into(),
        task_revision,
        artifact_paths: vec![path.to_path_buf()],
        check_kind: TaskCheckKind::Verification,
    }
}

#[test]
fn task_check_identity_normalizes_only_unittest_verbosity_options() {
    assert_eq!(
        normalized_check_identity("python3  -m unittest -v tests"),
        normalized_check_identity("python3 -m unittest --verbose tests")
    );
    assert_ne!(
        normalized_check_identity("python3 -m unittest -k -v tests"),
        normalized_check_identity("python3 -m unittest -k tests")
    );
    assert_ne!(
        normalized_check_identity("python3 -m unittest tests -- -v"),
        normalized_check_identity("python3 -m unittest tests")
    );
    // A verbosity-looking token consumed as an option value is not verbosity.
    assert_ne!(
        normalized_check_identity("python3 -m unittest --pattern -v tests"),
        normalized_check_identity("python3 -m unittest --pattern tests")
    );
    // Unknown options force raw identity, and tokens after `--` stay significant.
    assert_ne!(
        normalized_check_identity("python3 -m unittest --future-option -v tests"),
        normalized_check_identity("python3 -m unittest --future-option tests")
    );
    assert_ne!(
        normalized_check_identity("python3 -m unittest tests -- --verbose"),
        normalized_check_identity("python3 -m unittest tests --")
    );
}

#[test]
fn task_check_obligations_resolve_only_matching_identity_and_keep_pending_calls() {
    let stamp = TaskArtifactStamp {
        path: "artifact.txt".into(),
        sha256: "same".into(),
    };
    let scope = crate::state::ActiveTaskScope {
        root_task_id: "root".into(),
        task_id: "task-a".into(),
        task_revision: 1,
        begin_call_id: "begin-a".into(),
        start_history_cursor: 10,
    };
    let workspace_root = std::env::current_dir().unwrap();
    let make_obligation = |command: &str| {
        TaskCheckObligation::new(
            scope.clone(),
            workspace_root.clone(),
            command,
            vec![stamp.path.clone()],
        )
        .unwrap()
    };
    let mut ledger = TaskCheckLedger::default();
    let failed = make_obligation("python3 -m unittest -v tests");
    ledger.begin_with_obligation(
        "failed".into(),
        request_for(1, &stamp.path),
        workspace_root.clone(),
        vec![stamp.clone()],
        Some(failed),
    );
    ledger
        .finish("failed", 1, true, vec![stamp.clone()])
        .unwrap();
    let matching = make_obligation("python3 -m unittest --verbose tests");
    ledger.begin_with_obligation(
        "recovered".into(),
        request_for(1, &stamp.path),
        workspace_root.clone(),
        vec![stamp.clone()],
        Some(matching),
    );
    ledger
        .finish("recovered", 0, true, vec![stamp.clone()])
        .unwrap();
    assert!(ledger.unresolved_obligations(&scope).is_empty());

    let other = make_obligation("python3 -m unittest tests.other");
    ledger.begin_with_obligation(
        "unrelated".into(),
        request_for(1, &stamp.path),
        workspace_root.clone(),
        vec![stamp.clone()],
        Some(other),
    );
    ledger
        .finish("unrelated", 0, true, vec![stamp.clone()])
        .unwrap();
    assert!(
        ledger.unresolved_obligations(&scope).is_empty(),
        "a successful unrelated check must not resurrect a resolved failure"
    );
    assert!(ledger.unresolved_obligations_for_root("root").is_empty());
    assert!(
        ledger
            .unresolved_obligations_for_root("another-root")
            .is_empty()
    );

    let different_target_failure = make_obligation("python3 -m unittest tests.other");
    ledger.begin_with_obligation(
        "different-target-failure".into(),
        request_for(1, &stamp.path),
        workspace_root.clone(),
        vec![stamp.clone()],
        Some(different_target_failure.clone()),
    );
    ledger
        .finish("different-target-failure", 1, true, vec![stamp.clone()])
        .unwrap();
    let same_old_target_success = make_obligation("python3 -m unittest -v tests");
    ledger.begin_with_obligation(
        "same-old-target-success".into(),
        request_for(1, &stamp.path),
        workspace_root.clone(),
        vec![stamp.clone()],
        Some(same_old_target_success),
    );
    ledger
        .finish("same-old-target-success", 0, true, vec![stamp.clone()])
        .unwrap();
    assert!(
        ledger
            .unresolved_obligations(&scope)
            .iter()
            .any(|item| { item.command_identity == different_target_failure.command_identity })
    );

    let pending = make_obligation("python3 -m unittest pending_test");
    ledger.begin_with_obligation(
        "pending".into(),
        request_for(1, &stamp.path),
        workspace_root.clone(),
        vec![stamp.clone()],
        Some(pending.clone()),
    );
    let later_success = make_obligation("python3 -m unittest pending_test");
    ledger.begin_with_obligation(
        "pending-later-success".into(),
        request_for(1, &stamp.path),
        workspace_root.clone(),
        vec![stamp.clone()],
        Some(later_success),
    );
    ledger
        .finish("pending-later-success", 0, true, vec![stamp.clone()])
        .unwrap();
    assert!(
        ledger
            .unresolved_obligations(&scope)
            .iter()
            .any(|item| { item.command_identity == pending.command_identity })
    );
    assert_eq!(ledger.unresolved_obligations_for_root("root").len(), 2);

    let wire = serde_json::to_string(&ledger.snapshot()).unwrap();
    let mut restored = TaskCheckLedger::default();
    restored
        .restore_snapshot(serde_json::from_str(&wire).unwrap())
        .unwrap();
    let mut restored_scope = restored.unresolved_obligations(&scope);
    let mut ledger_scope = ledger.unresolved_obligations(&scope);
    restored_scope.sort_by(|left, right| left.command_identity.cmp(&right.command_identity));
    ledger_scope.sort_by(|left, right| left.command_identity.cmp(&right.command_identity));
    assert_eq!(restored_scope, ledger_scope);
    let mut restored_root = restored.unresolved_obligations_for_root("root");
    let mut ledger_root = ledger.unresolved_obligations_for_root("root");
    restored_root.sort_by(|left, right| left.command_identity.cmp(&right.command_identity));
    ledger_root.sort_by(|left, right| left.command_identity.cmp(&right.command_identity));
    assert_eq!(restored_root, ledger_root);
}

#[test]
fn task_check_obligation_identity_ignores_history_cursor_but_keeps_execution_scope() {
    let cwd = std::env::current_dir().unwrap();
    let scope = crate::state::ActiveTaskScope {
        root_task_id: "root".into(),
        task_id: "task-a".into(),
        task_revision: 1,
        begin_call_id: "begin-a".into(),
        start_history_cursor: 10,
    };
    let first = TaskCheckObligation::new(
        scope.clone(),
        cwd.clone(),
        "python3 -m unittest -v tests",
        vec!["result.txt".into(), "other.txt".into()],
    )
    .unwrap();
    let mut rebased_scope = scope.clone();
    rebased_scope.start_history_cursor = 99;
    let rebased = TaskCheckObligation::new(
        rebased_scope,
        cwd.clone(),
        "python3 -m unittest --verbose tests",
        vec!["other.txt".into(), "result.txt".into()],
    )
    .unwrap();
    assert_eq!(first.key(), rebased.key());

    let another_revision = TaskCheckObligation {
        scope: crate::state::ActiveTaskScope {
            task_revision: 2,
            ..scope.clone()
        },
        ..first.clone()
    };
    assert_ne!(first.key(), another_revision.key());
    let another_task = TaskCheckObligation {
        scope: crate::state::ActiveTaskScope {
            task_id: "task-b".into(),
            ..scope.clone()
        },
        ..first.clone()
    };
    assert_ne!(first.key(), another_task.key());
    let another_root = TaskCheckObligation {
        scope: crate::state::ActiveTaskScope {
            root_task_id: "another-root".into(),
            ..scope.clone()
        },
        ..first.clone()
    };
    assert_ne!(first.key(), another_root.key());
    let another_begin = TaskCheckObligation {
        scope: crate::state::ActiveTaskScope {
            begin_call_id: "begin-b".into(),
            ..scope.clone()
        },
        ..first.clone()
    };
    assert_ne!(first.key(), another_begin.key());
    let another_artifact_set = TaskCheckObligation {
        artifact_paths: vec!["different.txt".into()],
        ..first.clone()
    };
    assert_ne!(first.key(), another_artifact_set.key());
    let another_target = TaskCheckObligation::new(
        scope,
        cwd,
        "python3 -m unittest -v tests.other",
        vec!["result.txt".into(), "other.txt".into()],
    )
    .unwrap();
    assert_ne!(first.key(), another_target.key());
    let another_cwd = TaskCheckObligation {
        workspace_root: "/another-workspace".into(),
        ..first.clone()
    };
    assert_ne!(first.key(), another_cwd.key());
}

#[test]
fn changed_artifacts_remain_unresolved_after_later_success() {
    let stamp = TaskArtifactStamp {
        path: "artifact.txt".into(),
        sha256: "same".into(),
    };
    let mut ledger = TaskCheckLedger::default();
    let cwd = std::env::current_dir().unwrap();
    let scope = crate::state::ActiveTaskScope {
        root_task_id: "root".into(),
        task_id: "task-a".into(),
        task_revision: 4,
        begin_call_id: "begin-a".into(),
        start_history_cursor: 10,
    };
    let obligation = TaskCheckObligation::new(
        scope.clone(),
        cwd.clone(),
        "python3 -m unittest -v tests",
        vec![stamp.path.clone()],
    )
    .unwrap();
    ledger.begin_with_obligation(
        "pass".into(),
        request_for(4, &stamp.path),
        cwd.clone(),
        vec![stamp.clone()],
        Some(obligation.clone()),
    );
    ledger.finish("pass", 0, true, vec![stamp.clone()]).unwrap();
    ledger.begin_with_obligation(
        "changed".into(),
        request_for(4, &stamp.path),
        cwd.clone(),
        vec![stamp.clone()],
        Some(obligation),
    );
    assert!(
        ledger
            .finish(
                "changed",
                0,
                true,
                vec![TaskArtifactStamp {
                    path: stamp.path.clone(),
                    sha256: "changed".into(),
                }]
            )
            .is_none()
    );
    let latest = ledger.latest_for_task("task-a", 4).unwrap();
    assert_eq!(latest.call_id, "changed");
    assert!(matches!(
        latest.status,
        TaskCheckAttemptStatus::ChangedArtifacts
    ));
    ledger.begin_with_obligation(
        "later-pass".into(),
        request_for(4, &stamp.path),
        cwd.clone(),
        vec![TaskArtifactStamp {
            path: stamp.path.clone(),
            sha256: "changed".into(),
        }],
        Some(
            TaskCheckObligation::new(
                scope.clone(),
                cwd,
                "python3 -m unittest -v tests",
                vec![stamp.path.clone()],
            )
            .unwrap(),
        ),
    );
    ledger
        .finish(
            "later-pass",
            0,
            true,
            vec![TaskArtifactStamp {
                path: stamp.path,
                sha256: "changed".into(),
            }],
        )
        .unwrap();
    assert_eq!(ledger.unresolved_obligations(&scope).len(), 1);
    assert!(matches!(
        ledger
            .attempt_for_call("changed")
            .map(|attempt| &attempt.status),
        Some(TaskCheckAttemptStatus::ChangedArtifacts)
    ));
}

#[test]
fn latest_failed_or_pending_attempt_blocks_an_older_success() {
    let stamp = TaskArtifactStamp {
        path: "artifact.txt".into(),
        sha256: "same".into(),
    };
    let mut ledger = TaskCheckLedger::default();
    ledger.begin(
        "pass".into(),
        request_for(5, &stamp.path),
        "/w".into(),
        vec![stamp.clone()],
    );
    ledger.finish("pass", 0, true, vec![stamp.clone()]).unwrap();
    ledger.begin(
        "failure".into(),
        request_for(5, &stamp.path),
        "/w".into(),
        vec![stamp.clone()],
    );
    ledger
        .finish("failure", 1, true, vec![stamp.clone()])
        .unwrap();
    let latest = ledger.latest_for_task("task-a", 5).unwrap();
    assert_eq!(latest.call_id, "failure");
    assert!(matches!(
        &latest.status,
        TaskCheckAttemptStatus::Finished(receipt) if !receipt.succeeded
    ));

    ledger.begin(
        "pending".into(),
        request_for(5, &stamp.path),
        "/w".into(),
        vec![stamp],
    );
    let latest = ledger.latest_for_task("task-a", 5).unwrap();
    assert_eq!(latest.call_id, "pending");
    assert!(matches!(latest.status, TaskCheckAttemptStatus::Pending));
}

#[test]
fn snapshot_retains_an_earlier_failed_attempt_after_later_success() {
    let stamp = TaskArtifactStamp {
        path: "artifact.txt".into(),
        sha256: "same".into(),
    };
    let mut ledger = TaskCheckLedger::default();
    let scope = crate::state::ActiveTaskScope {
        root_task_id: "root".into(),
        task_id: "task-a".into(),
        task_revision: 7,
        begin_call_id: "begin-a".into(),
        start_history_cursor: 10,
    };
    for (call, exit) in [("failed", 1), ("different-success", 0)] {
        ledger.begin(
            call.into(),
            request_for(7, &stamp.path),
            "/w".into(),
            vec![stamp.clone()],
        );
        ledger
            .finish(call, exit, true, vec![stamp.clone()])
            .unwrap();
    }
    assert_eq!(ledger.unresolved_count_for_scope(&scope), 1);

    let snapshot = ledger.snapshot();
    assert_eq!(
        snapshot.len(),
        2,
        "all verification obligations must persist"
    );
    let mut restored = TaskCheckLedger::default();
    restored.restore_snapshot(snapshot).unwrap();
    assert!(matches!(
        restored.attempt_for_call("failed").map(|attempt| &attempt.status),
        Some(TaskCheckAttemptStatus::Finished(receipt)) if !receipt.succeeded
    ));
    assert_eq!(restored.unresolved_count_for_scope(&scope), 1);
}

#[test]
fn repeated_resolved_checks_keep_snapshot_bounded() {
    let stamp = TaskArtifactStamp {
        path: "artifact.txt".into(),
        sha256: "same".into(),
    };
    let scope = crate::state::ActiveTaskScope {
        root_task_id: "root".into(),
        task_id: "task-a".into(),
        task_revision: 8,
        begin_call_id: "begin-a".into(),
        start_history_cursor: 10,
    };
    let cwd = std::env::current_dir().unwrap();
    let obligation = TaskCheckObligation::new(
        scope.clone(),
        cwd.clone(),
        "python3 -m unittest -v tests",
        vec![stamp.path.clone()],
    )
    .unwrap();
    let mut ledger = TaskCheckLedger::default();
    for index in 0..300 {
        let failed_call = format!("failed-{index}");
        ledger.begin_with_obligation(
            failed_call.clone(),
            request_for(8, &stamp.path),
            cwd.clone(),
            vec![stamp.clone()],
            Some(obligation.clone()),
        );
        ledger
            .finish(&failed_call, 1, true, vec![stamp.clone()])
            .unwrap();

        let success_call = format!("success-{index}");
        ledger.begin_with_obligation(
            success_call.clone(),
            request_for(8, &stamp.path),
            cwd.clone(),
            vec![stamp.clone()],
            Some(obligation.clone()),
        );
        ledger
            .finish(&success_call, 0, true, vec![stamp.clone()])
            .unwrap();
        if index < 299 {
            let wire = serde_json::to_string(&ledger.snapshot()).unwrap();
            let mut restored = TaskCheckLedger::default();
            restored
                .restore_snapshot(serde_json::from_str(&wire).unwrap())
                .unwrap();
            ledger = restored;
        }
    }

    assert!(ledger.unresolved_obligations(&scope).is_empty());
    assert_eq!(
        ledger.snapshot().len(),
        1,
        "resolved checks must not bloat snapshots"
    );

    let referenced = std::collections::HashSet::from(["failed-299".to_string()]);
    let referenced_snapshot = ledger.snapshot_with_references(&referenced);
    assert_eq!(
        referenced_snapshot.len(),
        2,
        "a referenced failed attempt retains its resolution target"
    );
    assert!(referenced_snapshot.iter().any(|entry| {
        entry.attempt.call_id == "failed-299"
            && entry.attempt.resolved_by.as_deref() == Some("success-299")
    }));
}

#[test]
fn latest_attempt_follows_start_order_when_checks_finish_out_of_order() {
    let stamp = TaskArtifactStamp {
        path: "artifact.txt".into(),
        sha256: "same".into(),
    };
    let mut ledger = TaskCheckLedger::default();
    let cwd = std::env::current_dir().unwrap();
    let scope = crate::state::ActiveTaskScope {
        root_task_id: "root".into(),
        task_id: "task-a".into(),
        task_revision: 6,
        begin_call_id: "begin-a".into(),
        start_history_cursor: 10,
    };
    let obligation = TaskCheckObligation::new(
        scope.clone(),
        cwd.clone(),
        "python3 -m unittest -v tests",
        vec![stamp.path.clone()],
    )
    .unwrap();
    ledger.begin_with_obligation(
        "older".into(),
        request_for(6, &stamp.path),
        cwd.clone(),
        vec![stamp.clone()],
        Some(obligation.clone()),
    );
    ledger.begin_with_obligation(
        "newer".into(),
        request_for(6, &stamp.path),
        cwd,
        vec![stamp.clone()],
        Some(obligation),
    );
    ledger
        .finish("newer", 0, true, vec![stamp.clone()])
        .unwrap();
    ledger.finish("older", 1, true, vec![stamp]).unwrap();
    assert_eq!(
        ledger.latest_for_task("task-a", 6).unwrap().call_id,
        "newer"
    );
    assert_eq!(
        ledger.unresolved_obligations(&scope).len(),
        1,
        "a later failure from an already-running attempt remains unresolved"
    );
}

#[test]
fn initial_yield_does_not_complete_pending_evidence() {
    let workspace_root = std::env::current_dir().unwrap();
    let stamp = TaskArtifactStamp {
        path: "artifact.txt".into(),
        sha256: "before".into(),
    };
    let mut ledger = TaskCheckLedger::default();
    ledger.begin(
        "long-running-call".into(),
        TaskEvidenceRequest {
            task_id: "task-a".into(),
            task_revision: 1,
            artifact_paths: vec![stamp.path.clone()],
            check_kind: TaskCheckKind::Verification,
        },
        workspace_root,
        vec![stamp],
    );
    assert!(matches!(
        &ledger.latest_for_task("task-a", 1).unwrap().status,
        TaskCheckAttemptStatus::Pending
    ));
}

#[test]
fn exec_command_without_evidence_keeps_metadata_optional() {
    let args = serde_json::from_value::<crate::tools::handlers::unified_exec::ExecCommandArgs>(
        serde_json::json!({ "cmd": "true" }),
    )
    .expect("normal exec args remain valid");
    assert!(args.task_evidence.is_none());
}

#[test]
fn refuses_changed_artifact_and_workspace_escape() {
    let before = TaskArtifactStamp {
        path: "a".into(),
        sha256: "before".into(),
    };
    let after = TaskArtifactStamp {
        path: "a".into(),
        sha256: "after".into(),
    };
    let mut ledger = TaskCheckLedger::default();
    ledger.begin(
        "c".into(),
        TaskEvidenceRequest {
            task_id: "t".into(),
            task_revision: 1,
            artifact_paths: vec![before.path.clone()],
            check_kind: TaskCheckKind::Verification,
        },
        "/workspace".into(),
        vec![before],
    );
    assert!(ledger.finish("c", 0, true, vec![after]).is_none());
    let root = std::env::temp_dir();
    assert!(hash_workspace_artifacts(&root, &[PathBuf::from("../outside")]).is_none());
}

#[test]
fn evidence_request_rejects_model_supplied_receipt_fields() {
    let valid = serde_json::json!({
        "task_id": "task-a",
        "task_revision": 2,
        "artifact_paths": ["src/lib.rs"],
        "check_kind": "verification"
    });
    assert!(serde_json::from_value::<TaskEvidenceRequest>(valid.clone()).is_ok());
    let mut forged = valid;
    forged["accepted"] = serde_json::Value::Bool(true);
    assert!(serde_json::from_value::<TaskEvidenceRequest>(forged).is_err());
}

#[test]
fn execution_hashes_only_existing_regular_files_under_workspace() {
    let temp = tempfile::tempdir().expect("tempdir");
    let artifact = temp.path().join("artifact.txt");
    std::fs::write(&artifact, "before").expect("write artifact");
    let execution = TaskCheckExecution::new(
        TaskEvidenceRequest {
            task_id: "task-a".into(),
            task_revision: 1,
            artifact_paths: vec!["artifact.txt".into()],
            check_kind: TaskCheckKind::Verification,
        },
        temp.path(),
    )
    .expect("valid task evidence");
    let before = execution.before.clone();
    std::fs::write(artifact, "after").expect("rewrite artifact");
    let after = execution.hash_artifacts().expect("hash after");
    assert_ne!(before, after);
    assert!(
        TaskCheckExecution::new(
            TaskEvidenceRequest {
                task_id: "task-a".into(),
                task_revision: 1,
                artifact_paths: vec!["../outside".into()],
                check_kind: TaskCheckKind::Verification,
            },
            temp.path(),
        )
        .is_none()
    );
}

#[cfg(unix)]
#[test]
fn refuses_fifo_without_blocking_on_open() {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let temp = tempfile::tempdir().expect("tempdir");
    let fifo_path = temp.path().join("artifact.pipe");
    let fifo_cstr = CString::new(fifo_path.as_os_str().as_bytes()).expect("fifo path");
    // SAFETY: the path is a valid NUL-terminated string and mode is a valid permission mask.
    let result = unsafe { libc::mkfifo(fifo_cstr.as_ptr(), 0o600) };
    assert_eq!(result, 0, "mkfifo should create a FIFO");
    assert!(hash_workspace_artifacts(temp.path(), &["artifact.pipe".into()]).is_none());
}

// Modified by RenCrow Switch Core, 2026-11-26: retain host expected-negative evidence across restore.
#[test]
fn expected_negative_check_survives_cold_restore_without_rewriting_raw_failure() {
    let workspace = tempfile::tempdir().expect("workspace");
    let artifact = std::path::PathBuf::from("regression_test.go");
    std::fs::write(workspace.path().join(&artifact), "package regression\n")
        .expect("baseline artifact");
    let scope = crate::state::ActiveTaskScope {
        root_task_id: "root-v1".into(),
        task_id: "task-a".into(),
        task_revision: 1,
        begin_call_id: "begin-v1".into(),
        start_history_cursor: 0,
    };
    let request = request_for(1, &artifact);
    let execution =
        TaskCheckExecution::new(request.clone(), workspace.path()).expect("bounded baseline");
    let obligation = TaskCheckObligation::new(
        scope.clone(),
        execution.workspace_root.clone(),
        "go test -json -count=1 -run '^TestRegression$' .",
        vec![artifact],
    )
    .expect("bound obligation");
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
    let output = concat!(
        "{\"Action\":\"run\",\"Package\":\"example.test/regression\",\"Test\":\"TestRegression\"}\n",
        "{\"Action\":\"output\",\"Package\":\"example.test/regression\",\"Test\":\"TestRegression\",\"Output\":\"expected 4, got 3\\n\"}\n",
        "{\"Action\":\"fail\",\"Package\":\"example.test/regression\",\"Test\":\"TestRegression\"}\n",
        "{\"Action\":\"fail\",\"Package\":\"example.test/regression\"}\n",
    );
    let observation = TaskCheckObservation::from_execution(
        "baseline-call",
        &command,
        workspace.path(),
        output,
        /*exit_code*/ 1,
        /*completed*/ true,
        /*producer_truncated*/ false,
    );
    let output_sha = observation.diagnostic_sha256.clone();
    let mut ledger = TaskCheckLedger::default();
    ledger.begin_with_obligation(
        "baseline-call".into(),
        request,
        execution.workspace_root,
        execution.before.clone(),
        Some(obligation.clone()),
    );
    ledger
        .finish_observed(
            "baseline-call",
            /*exit_code*/ 1,
            /*completed*/ true,
            execution.before,
            Some(observation),
        )
        .expect("raw failing receipt");
    assert_eq!(ledger.unresolved_count_for_scope(&scope), 1);
    let mut wire = serde_json::to_value(ledger.snapshot()).expect("snapshot");
    wire[0]["expected_negative"] = serde_json::json!({
        "binding": {
            "source_call_id": "baseline-call",
            "obligation": obligation,
            "condition_id": "AC-reproduce",
            "condition_revision": 1,
            "condition_set_revision": 1,
            "baseline_version": "a".repeat(64),
            "expected_outcome_digest": "b".repeat(64),
            "expected_failure_identity": "TestRegression: expected 4, got 3"
        },
        "resolution": {
            "observed_failure_identity": "TestRegression: expected 4, got 3",
            "evidence_ref": "host-evidence:baseline-call",
            "raw_output_sha256": output_sha
        }
    });
    let restored_snapshot = serde_json::from_value(wire)
        .expect("host expected-negative record is part of the typed snapshot");
    let mut restored = TaskCheckLedger::default();
    restored
        .restore_snapshot(restored_snapshot)
        .expect("restore bound negative evidence");
    assert_eq!(restored.unresolved_count_for_scope(&scope), 0);
    let receipt = restored.get("baseline-call").expect("raw failure retained");
    assert_eq!((receipt.exit_code, receipt.succeeded), (1, false));
    assert!(restored.latest_for_task("task-a", 1).is_some());
}
