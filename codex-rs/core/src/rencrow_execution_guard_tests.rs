// Modified by RenCrow: native progress/receipt regression tests.
use super::*;
use pretty_assertions::assert_eq;
use std::num::NonZeroU16;

#[test]
fn repeated_read_is_blocked_until_real_progress() {
    let policy = RencrowExecutionGuard {
        max_repeated_reads: NonZeroU16::new(2).unwrap(),
        ..Default::default()
    };
    let mut state = GuardState::default();
    assert!(state.charge_read(&policy, "cat a").is_ok());
    assert!(state.charge_read(&policy, "cat a").is_ok());
    assert!(state.charge_read(&policy, "cat a").is_err());
    assert!(state.charge_read(&policy, "ls").is_ok());
    assert!(state.charge_read(&policy, "cat b").is_ok());
    state.observe_diff(Some("real patch"));
    state.finish_check("cargo test", state.diff_stamp, 0);
    assert!(state.charge_read(&policy, "cat a").is_ok());
}

#[test]
fn distinct_reads_can_exceed_the_legacy_total_without_progress_fabrication() {
    let policy = RencrowExecutionGuard {
        max_read_only_calls: NonZeroU16::new(1).unwrap(),
        max_repeated_reads: NonZeroU16::new(1).unwrap(),
        task_kind: Some(RencrowTaskKind::ReadOnly),
        ..Default::default()
    };
    let mut state = GuardState::default();
    for index in 0..32 {
        assert!(
            state
                .charge_read(&policy, &format!("inspect source {index}"))
                .is_ok()
        );
    }
    assert!(state.charge_read(&policy, "inspect source 0").is_err());
}

#[test]
fn only_a_successful_check_of_a_new_diff_resets_read_budget() {
    let policy = RencrowExecutionGuard {
        max_repeated_reads: NonZeroU16::new(1).unwrap(),
        ..Default::default()
    };
    let mut state = GuardState::default();
    state.observe_diff(Some("small first patch"));
    state.finish_check("cargo test", state.diff_stamp, 0);
    assert!(state.charge_read(&policy, "cat a").is_ok());
    assert!(state.charge_read(&policy, "cat a").is_err());

    // Repeating the same passing check cannot keep reopening the read budget.
    state.finish_check("cargo test", state.diff_stamp, 0);
    assert!(state.charge_read(&policy, "cat a").is_err());

    // A changed patch must itself pass a check before it counts as progress.
    state.observe_diff(Some("small second patch"));
    assert!(state.charge_read(&policy, "cat a").is_err());
    state.finish_check("cargo test", state.diff_stamp, 0);
    assert!(state.charge_read(&policy, "cat a").is_ok());
}

#[test]
fn failed_current_check_recovers_diagnostic_reads_only_once_per_stamp_and_twice_per_turn() {
    let policy = RencrowExecutionGuard {
        max_read_only_calls: NonZeroU16::new(1).unwrap(),
        ..Default::default()
    };
    let mut state = GuardState::default();
    state.observe_diff(Some("patch A"));
    state.charge_read(&policy, "rg before failure").unwrap();
    let stamp_a = state.diff_stamp;
    state.finish_check("test failed", stamp_a, 1);
    assert!(state.charge_read(&policy, "inspect failure output").is_ok());
    assert!(
        state
            .charge_read(&policy, "same failure output renamed")
            .is_err()
    );

    // Re-running the failure and changing the command identity cannot replenish reads.
    state.finish_check("renamed check", stamp_a, 1);
    assert!(state.charge_read(&policy, "new command name").is_err());

    state.observe_diff(Some("patch B"));
    let stamp_b = state.diff_stamp;
    state.finish_check("B failed", stamp_b, 1);
    assert!(state.charge_read(&policy, "inspect B").is_ok());

    state.observe_diff(Some("patch A"));
    state.finish_check("A failed again", stamp_a, 1);
    assert!(
        state
            .charge_read(&policy, "A cannot recover twice")
            .is_err()
    );
}

#[test]
fn failed_unmodified_baseline_checks_use_the_normal_read_budget() {
    for task_kind in [RencrowTaskKind::Verify, RencrowTaskKind::ReadOnly] {
        let policy = RencrowExecutionGuard {
            max_read_only_calls: NonZeroU16::new(2).unwrap(),
            task_kind: Some(task_kind),
            ..Default::default()
        };
        let mut state = GuardState::default();
        state.observe_diff(None);
        let baseline_stamp = state.diff_stamp;

        state.finish_check("baseline check A", baseline_stamp, 1);
        state.finish_check("baseline check B", baseline_stamp, 1);

        assert!(!state.diff_nonempty);
        assert_eq!(state.diagnostic_reads_stamp, None);
        assert_eq!(state.baseline_failure_reads_stamp, Some(baseline_stamp));
        assert!(state.failed_checks.contains("baseline check A"));
        assert!(state.failed_checks.contains("baseline check B"));
        assert!(!state.checked);
        assert!(
            state
                .charge_read(&policy, "inspect baseline failure A")
                .is_ok()
        );
        assert!(
            state
                .charge_read(&policy, "inspect baseline failure B")
                .is_ok()
        );
        let error = state
            .charge_read(&policy, "inspect baseline failure C")
            .expect_err("ordinary read limit still applies after baseline failures");
        assert!(error.contains("bounded read budget after failed checks"));
        assert!(!error.contains("failed current diff"));
    }
}

#[test]
fn newly_verified_diff_reopens_bounded_diagnostics_but_repeated_or_failed_checks_do_not() {
    let policy = RencrowExecutionGuard {
        max_read_only_calls: NonZeroU16::new(1).unwrap(),
        ..Default::default()
    };
    let mut state = GuardState::default();

    state.observe_diff(Some("patch A"));
    state.finish_check("test A failed", state.diff_stamp, 1);
    assert!(state.charge_read(&policy, "diagnose A").is_ok());

    state.observe_diff(Some("patch B"));
    state.finish_check("test B failed", state.diff_stamp, 1);
    assert!(state.charge_read(&policy, "diagnose B").is_ok());

    state.observe_diff(Some("patch C"));
    state.finish_check("test C failed", state.diff_stamp, 1);
    assert!(
        state
            .charge_read(&policy, "diagnose C before verified progress")
            .is_err()
    );

    state.observe_diff(Some("verified patch D"));
    state.finish_check("test D passed", state.diff_stamp, 0);
    // Repeating a successful check on the same revision is not new progress.
    state.finish_check("test D passed again", state.diff_stamp, 0);

    state.observe_diff(Some("patch E"));
    state.finish_check("test E failed", state.diff_stamp, 1);
    assert!(
        state
            .charge_read(&policy, "diagnose E after verified progress")
            .is_ok()
    );

    // Re-running the failed check on the same revision cannot replenish diagnostics.
    state.finish_check("test E failed again", state.diff_stamp, 1);
    assert!(state.charge_read(&policy, "diagnose E again").is_err());
}

#[test]
fn failed_check_opens_bounded_diagnostics_after_unlimited_normal_reads() {
    let policy = RencrowExecutionGuard {
        max_read_only_calls: NonZeroU16::new(1).unwrap(),
        ..Default::default()
    };
    let mut state = GuardState::default();
    state.observe_diff(Some("real patch"));
    for index in 0..32 {
        state
            .charge_read(&policy, &format!("inspect source {index}"))
            .unwrap();
    }
    let stamp = state.diff_stamp;
    state.finish_check("failed test", stamp, 1);
    assert!(state.charge_read(&policy, "inspect failure output").is_ok());
    assert!(
        state
            .charge_read(&policy, "inspect another diagnostic")
            .is_err()
    );
}

#[test]
fn repeated_budget_refusals_have_a_finite_host_stop_threshold() {
    let mut state = GuardState::default();
    assert!(!state.record_budget_rejection());
    assert!(!state.record_budget_rejection());
    assert!(state.record_budget_rejection());
}

#[test]
fn patch_verification_failures_reset_only_after_a_changed_diff() {
    let mut state = GuardState::default();
    assert!(!state.record_patch_verification_failure(None));
    assert!(!state.record_patch_verification_failure(None));
    state.observe_diff(None);
    state.finish_check("cargo test", state.diff_stamp, 0);
    assert!(state.record_patch_verification_failure(None));

    let mut state = GuardState::default();
    assert!(!state.record_patch_verification_failure(Some("first diff")));
    assert!(!state.record_patch_verification_failure(Some("first diff")));
    state.observe_diff(Some("accepted changed diff"));
    assert!(!state.record_patch_verification_failure(Some("accepted changed diff")));
}

#[tokio::test]
async fn internal_patch_failures_do_not_receive_recovery_hints() {
    let (session, mut turn) = crate::session::tests::make_session_and_context().await;
    std::sync::Arc::make_mut(&mut turn.config).rencrow_execution_guard =
        Some(RencrowExecutionGuard::default());
    turn.session_source = codex_protocol::protocol::SessionSource::Internal(
        codex_protocol::protocol::InternalSessionSource::Guardian,
    );
    *session.active_turn.lock().await = Some(crate::state::ActiveTurn::default());
    let turn = std::sync::Arc::new(turn);
    let invocation = ToolInvocation {
        session: session.into(),
        step_context: crate::session::step_context::StepContext::for_test(std::sync::Arc::clone(
            &turn,
        )),
        turn,
        cancellation_token: tokio_util::sync::CancellationToken::new(),
        tracker: std::sync::Arc::new(tokio::sync::Mutex::new(
            crate::turn_diff_tracker::TurnDiffTracker::new(),
        )),
        call_id: "internal-patch".into(),
        tool_name: codex_tools::ToolName::plain("apply_patch"),
        source: crate::tools::context::ToolCallSource::Direct,
        payload: ToolPayload::Custom {
            input: "invalid patch".into(),
        },
    };
    let error = FunctionCallError::RespondToModel(format!(
        "{PATCH_RETRYABLE_ERROR_MARKER}{PATCH_CONTEXT_FAILURE_KIND}apply_patch verification failed: missing context"
    ));

    let FunctionCallError::RespondToModel(message) =
        patch_verification_failure(&invocation, error).await
    else {
        panic!("expected a model-visible patch error");
    };
    assert_eq!(message, "apply_patch verification failed: missing context");
    assert!(!message.contains("RENCROW_PATCH_CONTEXT_HINT"));

    let (exec_session, mut exec_turn) = crate::session::tests::make_session_and_context().await;
    std::sync::Arc::make_mut(&mut exec_turn.config).rencrow_execution_guard =
        Some(RencrowExecutionGuard::default());
    *exec_session.active_turn.lock().await = Some(crate::state::ActiveTurn::default());
    let exec_turn = std::sync::Arc::new(exec_turn);
    let exec_invocation = ToolInvocation {
        session: exec_session.into(),
        step_context: crate::session::step_context::StepContext::for_test(std::sync::Arc::clone(
            &exec_turn,
        )),
        turn: exec_turn,
        cancellation_token: tokio_util::sync::CancellationToken::new(),
        tracker: std::sync::Arc::new(tokio::sync::Mutex::new(
            crate::turn_diff_tracker::TurnDiffTracker::new(),
        )),
        call_id: "external-patch".into(),
        tool_name: codex_tools::ToolName::plain("apply_patch"),
        source: crate::tools::context::ToolCallSource::Direct,
        payload: ToolPayload::Custom {
            input: "invalid patch".into(),
        },
    };
    let error = FunctionCallError::RespondToModel(format!(
        "{PATCH_RETRYABLE_ERROR_MARKER}{PATCH_CONTEXT_FAILURE_KIND}apply_patch verification failed: missing context"
    ));

    let FunctionCallError::RespondToModel(message) =
        patch_verification_failure(&exec_invocation, error).await
    else {
        panic!("expected a model-visible patch error");
    };
    assert!(message.contains("RENCROW_PATCH_CONTEXT_HINT"));
    assert!(message.contains("Markdown line prefixes such as `- `"));
}

#[test]
fn patch_format_hint_example_parses_add_and_update_in_one_envelope() {
    let hint = patch_failure_hint(PATCH_SYNTAX_FAILURE_KIND);
    let (_, example) = hint.split_once("Example with two files:\n").unwrap();
    let parsed = codex_apply_patch::parse_patch(example).unwrap();
    assert_eq!(parsed.hunks.len(), 2);
    assert!(matches!(
        parsed.hunks[0],
        codex_apply_patch::Hunk::AddFile { .. }
    ));
    assert!(matches!(
        parsed.hunks[1],
        codex_apply_patch::Hunk::UpdateFile { .. }
    ));
}

#[test]
fn completion_requires_host_diff_and_checks_after_latest_edit() {
    let policy = RencrowExecutionGuard {
        require_edit: true,
        require_check: true,
        ..Default::default()
    };
    let mut state = GuardState::default();
    state.observe_diff(None);
    assert_eq!(state.missing(&policy, false).len(), 2);
    state.observe_diff(Some("first edit"));
    state.failed_checks.insert("go test".into());
    state.finish_check("go test", state.diff_stamp, 1);
    assert_eq!(state.missing(&policy, true).len(), 1);
    state.finish_check("unrelated check", state.diff_stamp, 0);
    assert_eq!(state.missing(&policy, true).len(), 1);
    state.finish_check("go test", state.diff_stamp, 0);
    assert!(state.missing(&policy, true).is_empty());
    let before_edit = state.diff_stamp;
    state.observe_diff(Some("second edit"));
    state.finish_check("go test", before_edit, 0);
    assert_eq!(state.missing(&policy, true).len(), 1);
    state.finish_check("go test", state.diff_stamp, 0);
    assert!(state.missing(&policy, true).is_empty());
}

#[test]
fn unrelated_success_keeps_failed_check_named_in_bounded_diagnostic() {
    let policy = RencrowExecutionGuard {
        require_edit: true,
        require_check: true,
        ..Default::default()
    };
    let mut state = GuardState::default();
    state.observe_diff(Some("patch"));
    state.finish_check("go build ./...", state.check_stamp(), 1);
    state.finish_check("go build ./internal/portal", state.check_stamp(), 0);

    assert_eq!(
        state.missing(&policy, true),
        vec!["successful check after the last edit (failed checks must be rerun)"]
    );
    assert!(
        state
            .failed_check_diagnostic()
            .contains("unresolved failed checks: go build ./...")
    );
    assert!(
        !state
            .failed_check_diagnostic()
            .contains("go build ./internal/portal")
    );
}

#[test]
fn failed_check_diagnostic_redacts_and_bounds_commands() {
    for secret_command in [
        "TOKEN='alpha beta' go test",
        "go test --ToKeN 'alpha beta'",
        "curl -H 'Authorization: Bearer alpha beta' https://example.test",
        "curl https://user:password@example.test/path",
    ] {
        let display = safe_check_command_display(secret_command);
        assert!(display.starts_with("[redacted command "));
        assert!(!display.contains("alpha"));
        assert!(!display.contains("password"));
        assert!(!display.contains("user"));
    }
    assert_eq!(
        safe_check_command_display("cd RenCrow_PORTAL && go build ./..."),
        "cd RenCrow_PORTAL && go build ./..."
    );
    let long_command = format!("go test {}", "x".repeat(300));
    assert!(
        safe_check_command_display(&long_command).chars().count()
            <= MAX_FAILED_CHECK_COMMAND_CHARS + 1
    );
}

#[test]
fn collecting_child_results_is_a_separate_completion_requirement() {
    let policy = RencrowExecutionGuard {
        require_child_results: true,
        ..Default::default()
    };
    let mut state = GuardState::default();
    state.children.insert("child".into());
    assert_eq!(
        state.missing(&policy, false),
        vec!["collected or explicitly closed delegated children"]
    );
    state.children.remove("child");
    assert!(state.missing(&policy, false).is_empty());
}

#[test]
fn typed_implement_and_verify_keep_an_explicit_child_collection_requirement() {
    use codex_config::RencrowTaskKind;
    for task_kind in [RencrowTaskKind::Implement, RencrowTaskKind::Verify] {
        let policy = RencrowExecutionGuard {
            task_kind: Some(task_kind),
            require_child_results: true,
            ..Default::default()
        };
        let mut state = GuardState::default();
        state.children.insert("pending child".into());
        state.observe_diff(Some("current patch"));
        let stamp = state.check_stamp();
        state.finish_check("passing check", stamp, 0);
        assert_eq!(
            state.missing(&policy, true),
            vec!["collected or explicitly closed delegated children"]
        );
    }
}

#[test]
fn orchestrator_requires_successful_child_evidence_and_a_parent_check() {
    use codex_config::RencrowTaskKind;
    use serde_json::json;
    let policy = RencrowExecutionGuard {
        task_kind: Some(RencrowTaskKind::Orchestrate),
        ..Default::default()
    };
    let mut state = GuardState::default();
    assert!(state.missing(&policy, false).is_empty());

    // An implementation child creates an obligation that survives failed/closed
    // worker status and cannot be erased by collecting the terminal result.
    state.implementation_child_delegated = true;
    state.children.insert("worker".into());
    state.observe_child_result("close_agent", &json!({"target":"worker"}), &json!({}));
    assert_eq!(state.missing(&policy, false).len(), 2);
    state
        .child_results
        .insert((codex_protocol::ThreadId::new(), "child-turn".into()));
    assert_eq!(state.missing(&policy, false).len(), 1);
    state.finish_check("parent check", state.diff_stamp, 0);
    assert!(state.missing(&policy, false).is_empty());
    state.checked = true;
    state
        .child_results
        .insert((codex_protocol::ThreadId::new(), "new-child-turn".into()));
    state.checked = false;
    assert_eq!(state.missing(&policy, false).len(), 1);

    let mut self_implementing = GuardState::default();
    self_implementing.observe_diff(Some("parent implementation"));
    assert_eq!(self_implementing.missing(&policy, true).len(), 1);
    self_implementing.finish_check("parent check", self_implementing.diff_stamp, 0);
    assert!(self_implementing.missing(&policy, true).is_empty());
}

#[test]
fn orchestrator_read_only_children_do_not_create_implementation_obligations() {
    use codex_config::RencrowTaskKind;
    use serde_json::json;
    let policy = RencrowExecutionGuard {
        task_kind: Some(RencrowTaskKind::Orchestrate),
        ..Default::default()
    };
    let mut state = GuardState::default();
    state.observe_child_result("spawn_agent", &json!({}), &json!({"agent_id":"reader"}));
    state.observe_child_result(
        "wait_agent",
        &json!({}),
        &json!({"status":{"reader":AgentStatus::Completed(Some("done".into()))}}),
    );
    assert!(state.missing(&policy, false).is_empty());
}

#[test]
fn unknown_child_contract_fails_closed_as_implementation_delegation() {
    assert!(!implementation_child_requires_edit(Some(false)));
    assert!(implementation_child_requires_edit(Some(true)));
    assert!(implementation_child_requires_edit(None));
}

#[test]
fn child_evidence_requires_a_diff_and_passing_check_for_implementation_turns() {
    use codex_config::RencrowTaskKind;
    let orchestrate = RencrowExecutionGuard {
        task_kind: Some(RencrowTaskKind::Orchestrate),
        ..Default::default()
    };
    let implement = RencrowExecutionGuard {
        task_kind: Some(RencrowTaskKind::Implement),
        ..Default::default()
    };
    let verify = RencrowExecutionGuard {
        task_kind: Some(RencrowTaskKind::Verify),
        ..Default::default()
    };
    let mut state = GuardState::default();
    state.checked = true;
    assert!(child_execution_evidence_eligible(
        &orchestrate,
        &state,
        true
    ));
    assert!(child_execution_evidence_eligible(&implement, &state, true));
    assert!(!child_execution_evidence_eligible(
        &orchestrate,
        &state,
        false
    ));
    assert!(!child_execution_evidence_eligible(&verify, &state, true));

    state.checked = false;
    assert!(!child_execution_evidence_eligible(
        &orchestrate,
        &state,
        true
    ));
}

#[test]
fn orchestrator_noop_turn_still_rejects_failed_or_incomplete_checks() {
    use codex_config::RencrowTaskKind;
    let policy = RencrowExecutionGuard {
        task_kind: Some(RencrowTaskKind::Orchestrate),
        ..Default::default()
    };
    let mut state = GuardState::default();
    state.failed_checks.insert("failed check".into());
    assert_eq!(state.missing(&policy, false).len(), 1);
    state.failed_checks.clear();
    state.pending_checks.insert(1, ("pending".into(), 0));
    assert_eq!(state.missing(&policy, false).len(), 1);
    state.pending_checks.clear();
    state
        .started_checks
        .insert("call".into(), ("started".into(), 0));
    assert_eq!(state.missing(&policy, false).len(), 1);

    let readonly = RencrowExecutionGuard {
        task_kind: Some(RencrowTaskKind::ReadOnly),
        ..Default::default()
    };
    assert!(state.missing(&readonly, false).is_empty());
}

#[test]
fn only_completed_results_are_candidates_for_host_child_evidence() {
    use serde_json::json;
    let completed = serde_json::to_value(AgentStatus::Completed(Some("done".into()))).unwrap();
    let errored = serde_json::to_value(AgentStatus::Errored("failed".into())).unwrap();
    let shutdown = serde_json::to_value(AgentStatus::Shutdown).unwrap();
    assert_eq!(
        completed_child_ids(
            "wait_agent",
            &json!({}),
            &json!({"status":{"a":completed,"b":errored,"c":shutdown},"timed_out":false})
        ),
        vec!["a".to_string()]
    );
    assert!(
        completed_child_ids(
            "wait_agent",
            &json!({}),
            &json!({"status":{},"timed_out":true})
        )
        .is_empty()
    );
    assert!(
        completed_child_ids(
            "resume_agent",
            &json!({"id":"a"}),
            &json!({"status":errored})
        )
        .is_empty()
    );
    assert_eq!(
        completed_child_ids(
            "resume_agent",
            &json!({"id":"a"}),
            &json!({"status":completed})
        ),
        vec!["a".to_string()]
    );
}

#[test]
fn child_receipt_invalidates_parent_checks_started_before_acceptance() {
    use codex_config::RencrowTaskKind;
    let policy = RencrowExecutionGuard {
        task_kind: Some(RencrowTaskKind::Orchestrate),
        ..Default::default()
    };
    let mut state = GuardState::default();
    state.observe_diff(Some("parent diff"));
    let old_check_stamp = state.check_stamp();
    state
        .child_results
        .insert((codex_protocol::ThreadId::new(), "child-turn".into()));
    state.child_revision += 1;
    state.checked = false;
    state.finish_check("old async check", old_check_stamp, 0);
    assert_eq!(
        state.missing(&policy, true),
        vec!["successful check after the last edit (failed checks must be rerun)"]
    );

    let current_check_stamp = state.check_stamp();
    state.finish_check("fresh parent check", current_check_stamp, 0);
    assert!(state.missing(&policy, true).is_empty());
}

#[test]
fn a_new_child_receipt_applies_budget_progress_once() {
    let mut state = GuardState::default();
    state.budget_rejections = 2;
    state.repeated.insert("same read".into(), 2);
    state.record_child_progress();
    assert!(state.repeated.is_empty());
    assert!(!state.record_budget_rejection());
    assert!(!state.record_budget_rejection());
    assert!(state.record_budget_rejection());
}

#[test]
fn shell_classification_preserves_checks_and_excludes_masked_failures() {
    for command in [
        "cat source.rs",
        "strings source.rs",
        "ls -l",
        "pwd",
        "rg target src",
        "cd /workspace && cat src.rs",
    ] {
        assert!(read_only(command), "{command}");
    }
    for command in [
        "python3 edit.py",
        "echo hello > source.rs",
        "go test ./...",
        "cat a; rm b",
    ] {
        assert!(!read_only(command), "{command}");
    }
    for command in [
        "go test ./...",
        "go test -tags realworld -count=1 -v -run TestRealWorldStoryPlayerRead ./cmd/rencrow",
        "cd /workspace && GOCACHE=/workspace/cache go test ./pkg",
        "cargo nextest run -p codex-core",
        "node --check app.js",
        "node --test internal/example.test.mjs",
        "/usr/local/bin/go test ./internal/example",
        "python3 -m py_compile src/example.py",
        "python3 -B -m py_compile src/example.py",
        "/opt/homebrew/bin/python3 -m py_compile src/example.py",
        "/Library/Frameworks/Python.framework/Versions/3.14/bin/Python -m unittest",
        "/Users/yukimi/.local/bin/rencrow-resource-check -- src/a.rs src/b.rs",
        "cd /Users/yukimi/RenCrow/Tmp/ren2-supervision/full-l1-20261029 && python3 -B -m unittest -v test_collect_films",
        "python -B -m pytest",
        "python3 -B -m compileall src",
        "python3 -m unittest tests",
        "python3 -B -m unittest -v tests",
        "bash scripts/e2e-static.sh",
        "bash scripts/e2e-static.sh 2>&1",
        "bash ./scripts/test-unit_3.sh",
        "bash scripts/test-unit.v1.sh",
        "scripts/e2e-static.sh",
        "./scripts/test-unit_3.sh",
        "RUN_E2E=1 bash scripts/e2e-static.sh",
        "RUN_E2E=1 ./scripts/e2e-static.sh",
        "go test -run \"TestOne|TestTwo\" ./...",
        "go test ./... 2>&1",
        "go test -run 'TestFetchRetries429|TestThrottleCooldown' ./...",
        "go test -run 'TestDollar$Literal|TestQuoted&&Name' ./...",
        "cd '/workspace with spaces' && GOCACHE='/cache|literal' go test -run 'TestOne|TestTwo' ./...",
    ] {
        assert!(check_command(command), "{command}");
    }
    for command in [
        "echo 'go test'",
        "go test || true",
        "go test & true",
        "go test $(echo ./...)",
        "go test; true",
        "go test && echo done",
        "cat tests.log",
        "go test > saved.log",
        "go test ./... 2>&1 | head",
        "go test ./... 2>&1; echo done",
        "go test ./... 2>errors.log",
        "go test -run 'TestOne|TestTwo' ./... | cat",
        "go test -run 'TestOne|TestTwo' ./... || true",
        "go test -run \"$(echo Test)\" ./...",
        "go test -run \"`echo Test`\" ./...",
        "go test -run 'TestOne|TestTwo ./...",
        "1INVALID=value go test ./...",
        "python3 -B test_collect_films.py",
        "python3 tests.py",
        "python3 tests.py && echo done",
        "python3 -B -c 'print(1)'",
        "/opt/homebrew/bin/python3.14 -c 'print(1)'",
        "python3 -X dev -m unittest",
        "python3 -B -m unittest && echo done",
        "python3 -B -m unittest | cat",
        "python3 -B -m py_compile file.py && echo done",
        "python3 -B -m unittest > saved.log",
        "node -- test.js",
        "echo go test ./...",
        "python3 -m py_compile file.py && echo done",
        "bash scripts/e2e-static.sh extra",
        "bash scripts/../scripts/e2e-static.sh",
        "bash /workspace/scripts/e2e-static.sh",
        "bash scripts/unknown.sh",
        "bash scripts/test-.sh",
        "bash scripts/test-é.sh",
        "bash scripts/nested/test-unit.sh",
        "scripts/e2e-static.sh extra",
        "./scripts/test-unit_3.sh extra",
        "bash -c 'scripts/e2e-static.sh'",
        "bash scripts/e2e-static.sh | cat",
        "bash scripts/e2e-static.sh; echo done",
        "bash scripts/e2e-static.sh && echo done",
        "bash scripts/e2e-static.sh || true",
        "bash scripts/e2e-static.sh > result.log",
    ] {
        assert!(!check_command(command), "{command}");
    }
}

#[test]
fn embedded_direct_check_detection_is_narrow() {
    assert_eq!(
        direct_check_in_compound_command("cd /tmp && cat value.py && python3 -B -m unittest -q"),
        Some("python3 -B -m unittest -q".into())
    );
    for command in [
        "echo 'python3 -m unittest'",
        "cd /tmp && python3 -B -m unittest -q",
        "python3 tests.py && echo done",
        "cat value.py && env TEST=1 python3 -B -m unittest -q",
        "unsupported-shell-syntax <(python3 -m unittest)",
    ] {
        assert_eq!(direct_check_in_compound_command(command), None, "{command}");
    }
}

#[test]
fn direct_python_scripts_are_not_checks_but_the_unittest_runner_is() {
    use codex_config::RencrowTaskKind;
    let policy = RencrowExecutionGuard {
        task_kind: Some(RencrowTaskKind::Orchestrate),
        ..Default::default()
    };
    let mut state = GuardState::default();
    state.observe_diff(Some("current patch"));
    assert!(!check_command("python3 tests.py"));
    assert!(unrecognized_python_test_script(
        "python3 tests.py && echo done"
    ));
    assert!(!unrecognized_python_test_script("python3 app.py"));
    assert!(!unrecognized_python_test_script(
        "python3 -m unittest tests"
    ));
    assert_eq!(
        state.missing(&policy, true),
        vec!["successful check after the last edit (failed checks must be rerun)"]
    );

    let command = "python3 -m unittest -v tests";
    assert!(check_command(command));
    state.finish_check(command, state.check_stamp(), 0);
    assert!(state.missing(&policy, true).is_empty());
}

#[test]
fn successful_unrecognized_test_script_gets_actionable_completion_guidance() {
    use codex_config::RencrowTaskKind;
    let policy = RencrowExecutionGuard {
        task_kind: Some(RencrowTaskKind::Orchestrate),
        ..Default::default()
    };
    let mut state = GuardState::default();
    state.observe_diff(Some("current patch"));
    state.record_uncredited_python_test("python3 tests.py && echo ALL_TESTS_PASSED");
    assert!(!state.checked);

    let mut attempts = 0;
    let prompt = state
        .completion_recovery_prompt(&policy, true, &mut attempts)
        .expect("missing recognized check should request recovery");
    assert!(prompt.contains("successful exec of `python3 tests.py"));
    assert!(prompt.contains("python3 -m unittest -v tests"));
    assert!(prompt.contains("pytest -q <test_path>"));

    state.finish_check("python3 -m unittest -v tests", state.check_stamp(), 0);
    assert!(state.uncredited_python_test().is_none());
    assert!(state.missing(&policy, true).is_empty());
}

#[test]
fn e2e_script_checks_require_a_successful_exit() {
    use codex_config::RencrowTaskKind;
    let policy = RencrowExecutionGuard {
        task_kind: Some(RencrowTaskKind::Orchestrate),
        ..Default::default()
    };
    let command = "./scripts/e2e-static.sh";
    assert!(check_command(command));

    let mut state = GuardState::default();
    state.observe_diff(Some("current patch"));
    state.finish_check(command, state.check_stamp(), 1);
    assert_eq!(
        state.missing(&policy, true),
        vec!["successful check after the last edit (failed checks must be rerun)"]
    );
    state.finish_check(command, state.check_stamp(), 0);
    assert!(state.missing(&policy, true).is_empty());
}

#[test]
fn config_limits_cannot_be_zero_or_weakened_by_a_role() {
    assert!(toml::from_str::<RencrowExecutionGuard>("max_read_only_calls = 0").is_err());
    let parent = RencrowExecutionGuard {
        require_check: true,
        ..Default::default()
    };
    let role = RencrowExecutionGuard {
        max_read_only_calls: NonZeroU16::new(100).unwrap(),
        require_edit: true,
        ..Default::default()
    };
    let merged = RencrowExecutionGuard::narrow(Some(parent.clone()), Some(role)).unwrap();
    assert_eq!(merged.max_read_only_calls, parent.max_read_only_calls);
    assert!(merged.require_edit && merged.require_check);
}

#[test]
fn executor_cannot_redelegate_through_a_direct_agent_cli() {
    assert!(nested_agent_cli("codex exec implement"));
    assert!(nested_agent_cli("/home/user/bin/codex exec implement"));
    assert!(nested_agent_cli("claude implement"));
    assert!(!nested_agent_cli("codex --version"));
    assert!(!nested_agent_cli("cat codex.rs"));
    assert!(!nested_agent_cli("echo 'codex exec implement'"));
}

#[test]
fn child_receipts_require_terminal_wait_or_explicit_close() {
    use serde_json::json;
    let mut state = GuardState::default();
    state.observe_child_result("spawn_agent", &json!({}), &json!({"agent_id":"child"}));
    for status in [
        AgentStatus::Running,
        AgentStatus::NotFound,
        AgentStatus::Interrupted,
    ] {
        state.observe_child_result(
            "wait_agent",
            &json!({}),
            &json!({"status":{"child":status},"timed_out":true}),
        );
        assert!(state.children.contains("child"));
    }
    state.observe_child_result("wait_agent", &json!({}), &json!({"status":{"child":AgentStatus::Errored("missing evidence".into())},"timed_out":false}));
    assert!(state.children.is_empty());
    state.observe_child_result(
        "resume_agent",
        &json!({"id":"child"}),
        &json!({"status":AgentStatus::Running}),
    );
    assert!(state.children.contains("child"));
    state.observe_child_result(
        "resume_agent",
        &json!({"id":"child"}),
        &json!({"status":AgentStatus::Errored("failed".into())}),
    );
    assert!(!state.children.contains("child"));
    state.observe_child_result(
        "close_agent",
        &json!({"target":"child"}),
        &json!({"previous_status":AgentStatus::Running}),
    );
    assert!(state.children.is_empty());
    state.observe_child_result(
        "send_input",
        &json!({"target":"child"}),
        &json!({"submission_id":"turn"}),
    );
    state.observe_child_result(
        "wait_agent",
        &json!({}),
        &json!({"status":{"child":AgentStatus::Completed(Some("done".into()))}}),
    );
    assert!(state.children.is_empty());
}

#[test]
fn child_recovery_is_bounded_and_requires_uncollected_children() {
    let mut state = GuardState::default();
    let mut attempts = 0;
    assert!(state.child_recovery_prompt(&mut attempts).is_none());
    state.children.insert("child-b".into());
    state.children.insert("child-a".into());
    let prompt = state.child_recovery_prompt(&mut attempts).unwrap();
    assert!(prompt.contains("child-a, child-b"));
    assert!(state.child_recovery_prompt(&mut attempts).is_some());
    assert!(state.child_recovery_prompt(&mut attempts).is_none());
    assert_eq!(attempts, 2);
    assert_eq!(state.children.len(), 2);
}

#[test]
fn completion_recovery_is_one_shot_and_uses_current_missing_evidence() {
    let policy = RencrowExecutionGuard {
        require_edit: true,
        require_check: true,
        ..Default::default()
    };
    let mut state = GuardState::default();
    state.observe_diff(None);
    let mut attempts = 0;
    let prompt = state
        .completion_recovery_prompt(&policy, false, &mut attempts)
        .unwrap();
    assert!(prompt.contains("accepted current-turn patch diff"));
    assert!(prompt.contains("successful check after the last edit"));
    assert_eq!(attempts, 1);
    assert!(
        state
            .completion_recovery_prompt(&policy, false, &mut attempts)
            .is_none()
    );

    state.observe_diff(Some("patch"));
    state.finish_check("check", state.diff_stamp, 0);
    assert!(state.missing(&policy, true).is_empty());
}

#[test]
fn typed_recovery_prompts_follow_verify_readonly_and_orchestrate_contracts() {
    use codex_config::RencrowTaskKind;
    let verify = RencrowExecutionGuard {
        task_kind: Some(RencrowTaskKind::Verify),
        require_edit: true,
        ..Default::default()
    };
    let mut state = GuardState::default();
    state.observe_diff(None);
    let mut attempts = 0;
    let prompt = state
        .completion_recovery_prompt(&verify, false, &mut attempts)
        .unwrap();
    assert!(prompt.contains("run and pass the required check"));
    assert!(!prompt.contains("make the required edit"));

    let readonly = RencrowExecutionGuard {
        task_kind: Some(RencrowTaskKind::ReadOnly),
        require_edit: true,
        require_check: true,
        ..Default::default()
    };
    assert!(
        state
            .completion_recovery_prompt(&readonly, false, &mut attempts)
            .is_none()
    );

    let orchestrate = RencrowExecutionGuard {
        task_kind: Some(RencrowTaskKind::Orchestrate),
        ..Default::default()
    };
    state.children.insert("child".into());
    let mut child_attempt = 0;
    assert!(state.child_recovery_prompt(&mut child_attempt).is_some());
    assert!(orchestrate.requires_child_results());
}

#[test]
fn timeout_does_not_collect_children_but_terminal_wait_does() {
    let mut state = GuardState::default();
    state.children.insert("child".into());
    state.observe_child_result(
        "wait_agent",
        &serde_json::json!({}),
        &serde_json::json!({"status": {}, "timed_out": true}),
    );
    assert!(state.children.contains("child"));
    state.observe_child_result("wait_agent", &serde_json::json!({}), &serde_json::json!({"status": {"child": serde_json::to_value(AgentStatus::Completed(Some("done".into()))).unwrap()}, "timed_out": false}));
    assert!(state.children.is_empty());
}

#[test]
fn bounded_read_pipelines_cannot_escape_the_read_budget() {
    for command in [
        "cd /workspace && rg target src | head -40",
        "cd /workspace && cat source.rs | tail -20",
        "cd /workspace; cat source.rs; echo separator; sed -n '1,20p' other.rs",
        "printf '%s' heading; rg target src | cut -c1-100",
    ] {
        assert!(read_only(command), "unmetered read-only command: {command}");
    }
}

#[tokio::test]
async fn host_child_recovery_uses_active_turn_and_stops_after_receipt() {
    let (session, mut turn) = crate::session::tests::make_session_and_context().await;
    std::sync::Arc::make_mut(&mut turn.config).rencrow_execution_guard =
        Some(RencrowExecutionGuard {
            require_child_results: true,
            ..Default::default()
        });
    let active = crate::state::ActiveTurn::default();
    let shared = active.turn_state.clone();
    shared
        .lock()
        .await
        .rencrow_execution_guard
        .children
        .insert("pending-child".into());
    *session.active_turn.lock().await = Some(active);
    let mut attempts = 0;
    assert!(
        child_recovery_prompt(&session, &turn, &mut attempts)
            .await
            .unwrap()
            .contains("pending-child")
    );
    shared
        .lock()
        .await
        .rencrow_execution_guard
        .observe_child_result(
            "close_agent",
            &serde_json::json!({"target":"pending-child"}),
            &serde_json::json!({"previous_status":"shutdown"}),
        );
    assert!(
        child_recovery_prompt(&session, &turn, &mut attempts)
            .await
            .is_none()
    );
    assert_eq!(attempts, 1);
}

#[tokio::test]
async fn session_evidence_is_in_memory_and_cleared_for_a_new_turn() {
    let (session, _) = crate::session::tests::make_session_and_context().await;
    let turn_id = "guarded-child-turn".to_string();
    session.record_started_turn(&turn_id).await;
    session
        .store_rencrow_execution_evidence(GuardExecutionEvidence {
            thread_id: session.thread_id(),
            turn_id: turn_id.clone(),
            parent_thread_id: codex_protocol::ThreadId::new(),
            diff_stamp: 7,
            has_diff: true,
            checked: true,
        })
        .await;
    assert!(
        session
            .rencrow_execution_evidence_for_test()
            .await
            .is_some()
    );

    assert!(
        session
            .rencrow_execution_evidence_for_test()
            .await
            .is_some()
    );

    session.record_started_turn("new-turn-after-resume").await;
    assert!(
        session
            .rencrow_execution_evidence_for_test()
            .await
            .is_none()
    );
}

#[tokio::test]
async fn parent_session_consumes_each_child_turn_receipt_once_across_parent_turns() {
    let (session, _) = crate::session::tests::make_session_and_context().await;
    let child_thread_id = codex_protocol::ThreadId::new();
    let evidence = GuardExecutionEvidence {
        thread_id: child_thread_id,
        turn_id: "child-turn-1".into(),
        parent_thread_id: session.thread_id(),
        diff_stamp: 17,
        has_diff: true,
        checked: true,
    };
    assert!(
        session
            .consume_rencrow_child_execution_evidence(&evidence)
            .await
    );

    // A new parent turn does not make the cached child turn a new receipt.
    session.record_started_turn("parent-turn-2").await;
    assert!(
        !session
            .consume_rencrow_child_execution_evidence(&evidence)
            .await
    );

    // The same child thread's genuinely new turn has independent evidence.
    let new_child_turn = GuardExecutionEvidence {
        turn_id: "child-turn-2".into(),
        ..evidence
    };
    assert!(
        session
            .consume_rencrow_child_execution_evidence(&new_child_turn)
            .await
    );
}
