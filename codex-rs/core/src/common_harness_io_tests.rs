use super::ArtifactBindingV1;
use super::ArtifactEntryV1;
use super::ArtifactSetV1;
use super::ExpectedBaselineFailure;
use super::GoTestExpectation;
use super::GoTestVerdict;
use super::PatchJournalBindingV1;
use super::PatchReconciliationV1;
use super::observe_go_test_json;
use super::prepare_patch_journal;
use super::reconcile_patch_journal;
#[cfg(unix)]
use codex_apply_patch::ApplyPatchAction;
#[cfg(unix)]
use codex_apply_patch::ApplyPatchArgs;
#[cfg(unix)]
use codex_apply_patch::MaybeApplyPatchVerified;
#[cfg(unix)]
use codex_exec_server::LocalFileSystem;
#[cfg(unix)]
use codex_utils_absolute_path::AbsolutePathBuf;
#[cfg(unix)]
use codex_utils_path_uri::PathUri;
use sha2::Digest;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

fn expectation(names: &[&str]) -> GoTestExpectation {
    GoTestExpectation {
        selected_cases: names.iter().map(|name| (*name).to_owned()).collect(),
        expected_case_count: names.len(),
        expected_baseline_failure: None,
    }
}

fn event(name: &str, action: &str) -> String {
    format!(
        "{{\"Time\":\"2026-11-26T00:00:00Z\",\"Action\":\"{action}\",\"Package\":\"example.test/pkg\",\"Test\":\"{name}\"}}\n"
    )
}

fn package_event(package: &str, action: &str) -> String {
    format!(
        "{{\"Time\":\"2026-11-26T00:00:00Z\",\"Action\":\"{action}\",\"Package\":\"{package}\"}}\n"
    )
}

fn package_start_event(package: &str) -> String {
    format!(
        "{{\"Time\":\"2026-11-26T00:00:00Z\",\"Action\":\"start\",\"Package\":\"{package}\"}}\n"
    )
}

fn capture_local_fixture(
    root: &std::path::Path,
    paths: &[PathBuf],
) -> anyhow::Result<ArtifactSetV1> {
    let binding = ArtifactBindingV1::for_canonical_root(root)?;
    ArtifactSetV1::capture_bound(root, paths, &binding)
}

#[test]
#[cfg(unix)]
fn artifact_set_sorts_files_and_records_absence_and_executable_mode() {
    let directory = tempfile::tempdir().expect("tempdir");
    fs::write(directory.path().join("run.sh"), "#!/bin/sh\n").expect("write executable");
    fs::set_permissions(
        directory.path().join("run.sh"),
        fs::Permissions::from_mode(0o755),
    )
    .expect("set executable mode");

    let captured = capture_local_fixture(
        directory.path(),
        &[PathBuf::from("missing.txt"), PathBuf::from("run.sh")],
    )
    .expect("capture artifact set");

    assert_eq!(captured.entries[0].path, PathBuf::from("missing.txt"));
    assert!(!captured.entries[0].present);
    assert!(captured.entries[1].present);
    assert!(captured.entries[1].executable);
    assert!(captured.matches_workspace().expect("compare workspace"));
}

#[cfg(unix)]
fn patch_binding(root: &std::path::Path, paths: &[&str]) -> PatchJournalBindingV1 {
    PatchJournalBindingV1 {
        task_id: "task-vector".to_owned(),
        request_revision: 3,
        attempt_no: 1,
        owner_epoch: 2,
        expected_state_version: 5,
        operation_id: "operation-vector".to_owned(),
        workspace_root: std::fs::canonicalize(root).expect("canonical workspace root"),
        artifact_binding: ArtifactBindingV1 {
            workspace_binding_ref: "workspace://vector".to_owned(),
            scope_revision: 7,
            git_commit_ref: None,
        },
        artifact_paths: paths.iter().map(PathBuf::from).collect(),
    }
}

#[cfg(unix)]
async fn verified_action(root: &std::path::Path, patch: &str) -> ApplyPatchAction {
    let mut args: ApplyPatchArgs = codex_apply_patch::parse_patch(patch).expect("parse patch");
    args.workdir = Some(root.to_string_lossy().into_owned());
    let cwd = PathUri::from_host_native_path(root).expect("workspace URI");
    match codex_apply_patch::verify_apply_patch_args(
        args,
        &cwd,
        &LocalFileSystem::unsandboxed(),
        None,
    )
    .await
    {
        MaybeApplyPatchVerified::Body(action) => action,
        other => panic!("patch did not produce a verified native action: {other:?}"),
    }
}

#[test]
#[cfg(unix)]
fn patch_journal_add_captures_preimage_and_predicted_postimage_without_applying() {
    let directory = tempfile::tempdir().expect("tempdir");
    let target =
        AbsolutePathBuf::try_from(directory.path().join("new.txt")).expect("absolute target");
    let action = ApplyPatchAction::new_add_for_test(
        &PathUri::from_abs_path(&target),
        "new content\n".to_owned(),
    );
    let binding = patch_binding(directory.path(), &["new.txt"]);

    let intent = prepare_patch_journal(&binding, &action).expect("prepare add journal");

    assert!(!intent.before.entries[0].present);
    assert!(intent.predicted_after.entries[0].present);
    assert!(!directory.path().join("new.txt").exists());
}

#[tokio::test]
#[cfg(unix)]
async fn patch_journal_projects_native_add_delete_update_and_move_actions() {
    let directory = tempfile::tempdir().expect("tempdir");
    fs::write(directory.path().join("delete.txt"), "old\n").expect("delete source");
    fs::write(directory.path().join("update.txt"), "old\n").expect("update source");
    fs::write(directory.path().join("move.txt"), "old\n").expect("move source");
    let cases = [
        (
            "add.txt",
            format!(
                "*** Begin Patch\n*** Add File: {}\n+added\n*** End Patch",
                directory.path().join("add.txt").display()
            ),
            false,
        ),
        (
            "delete.txt",
            format!(
                "*** Begin Patch\n*** Delete File: {}\n*** End Patch",
                directory.path().join("delete.txt").display()
            ),
            true,
        ),
        (
            "update.txt",
            format!(
                "*** Begin Patch\n*** Update File: {}\n@@\n-old\n+new\n*** End Patch",
                directory.path().join("update.txt").display()
            ),
            true,
        ),
        (
            "move.txt",
            format!(
                "*** Begin Patch\n*** Update File: {}\n*** Move to: {}\n@@\n-old\n+moved\n*** End Patch",
                directory.path().join("move.txt").display(),
                directory.path().join("moved.txt").display()
            ),
            true,
        ),
    ];

    for (source, patch, source_exists_before) in cases {
        let action = verified_action(directory.path(), &patch).await;
        let paths = if source == "move.txt" {
            vec!["move.txt", "moved.txt"]
        } else {
            vec![source]
        };
        let binding = patch_binding(directory.path(), &paths);
        let journal = prepare_patch_journal(&binding, &action)
            .unwrap_or_else(|error| panic!("journal projection for {source}: {error}"));
        assert_eq!(journal.before.entries[0].present, source_exists_before);
        assert_eq!(
            fs::read(directory.path().join(source)).is_ok(),
            source_exists_before,
            "preparing a journal must not apply {source} patch"
        );
    }
}

#[test]
#[cfg(unix)]
fn patch_journal_rejects_outside_and_symlink_targets_with_path_errors() {
    let directory = tempfile::tempdir().expect("tempdir");
    let outside = tempfile::tempdir().expect("outside tempdir");
    let outside_target =
        AbsolutePathBuf::try_from(outside.path().join("new.txt")).expect("absolute outside target");
    let outside_action = ApplyPatchAction::new_add_for_test(
        &PathUri::from_abs_path(&outside_target),
        "outside\n".to_owned(),
    );
    let outside_binding = patch_binding(directory.path(), &["new.txt"]);
    let outside_error = prepare_patch_journal(&outside_binding, &outside_action)
        .expect_err("reject target outside artifact scope");
    assert!(
        outside_error
            .to_string()
            .to_ascii_lowercase()
            .contains("path")
    );

    std::os::unix::fs::symlink(outside.path(), directory.path().join("link"))
        .expect("create symlink");
    let linked_target = AbsolutePathBuf::try_from(directory.path().join("link/new.txt"))
        .expect("absolute symlink target");
    let linked_action = ApplyPatchAction::new_add_for_test(
        &PathUri::from_abs_path(&linked_target),
        "outside\n".to_owned(),
    );
    let linked_binding = patch_binding(directory.path(), &["link/new.txt"]);
    let linked_error = prepare_patch_journal(&linked_binding, &linked_action)
        .expect_err("reject symlink component");
    assert!(
        linked_error
            .to_string()
            .to_ascii_lowercase()
            .contains("path")
    );
}

#[tokio::test]
#[cfg(unix)]
async fn patch_journal_reconciliation_handles_partial_crash_after_native_move_intent() {
    let directory = tempfile::tempdir().expect("tempdir");
    let source_path = directory.path().join("source.txt");
    fs::write(&source_path, "old\n").expect("write move source");
    let patch = format!(
        "*** Begin Patch\n*** Update File: {}\n*** Move to: {}\n@@\n-old\n+moved\n*** End Patch",
        source_path.display(),
        directory.path().join("target.txt").display()
    );
    let action = verified_action(directory.path(), &patch).await;
    let binding = patch_binding(directory.path(), &["source.txt", "target.txt"]);
    let intent = prepare_patch_journal(&binding, &action).expect("prepare verified move intent");

    fs::write(directory.path().join("target.txt"), "moved\n")
        .expect("write partial move destination");

    assert_eq!(
        reconcile_patch_journal(&intent).expect("reconcile partial crash"),
        PatchReconciliationV1::PartialOrOther,
    );
}

#[test]
fn artifact_fingerprint_matches_the_independent_canonical_json_and_digest_vector() {
    let binding = ArtifactBindingV1 {
        workspace_binding_ref: "workspace://vector".to_owned(),
        scope_revision: 7,
        git_commit_ref: None,
    };
    let entries = vec![
        ArtifactEntryV1 {
            path: PathBuf::from("a.txt"),
            present: true,
            sha256: Some(
                "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824".to_owned(),
            ),
            executable: false,
        },
        ArtifactEntryV1 {
            path: PathBuf::from("z.txt"),
            present: false,
            sha256: None,
            executable: false,
        },
        ArtifactEntryV1 {
            path: PathBuf::from("α.go"),
            present: false,
            sha256: None,
            executable: false,
        },
    ];

    let json = ArtifactSetV1::canonical_fingerprint_json(&binding, &entries)
        .expect("serialize canonical fingerprint input");
    assert_eq!(
        json,
        r#"{"format":"common-harness-artifact-set/1","workspace_binding_ref":"workspace://vector","scope_revision":7,"git_commit_ref":null,"entries":[{"path":"a.txt","kind":"file","executable":false,"sha256":"2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"},{"path":"z.txt","kind":"absent","executable":null,"sha256":null},{"path":"α.go","kind":"absent","executable":null,"sha256":null}]}"#
    );
    assert_eq!(
        format!("{:x}", sha2::Sha256::digest(json.as_bytes())),
        "f2def22190e27c0f0c6aac55908b492685ebeec7327392ec7716fa0b52c99060",
    );
}

#[test]
fn artifact_set_rejects_symlink_components_and_paths_outside_root() {
    let directory = tempfile::tempdir().expect("tempdir");
    let outside = tempfile::tempdir().expect("outside tempdir");
    fs::write(outside.path().join("secret.txt"), "secret").expect("write outside file");
    #[cfg(unix)]
    std::os::unix::fs::symlink(outside.path(), directory.path().join("link"))
        .expect("create symlink");

    assert!(capture_local_fixture(directory.path(), &[PathBuf::from("../secret.txt")]).is_err());
    #[cfg(unix)]
    assert!(capture_local_fixture(directory.path(), &[PathBuf::from("link/secret.txt")]).is_err());
}

#[test]
fn artifact_set_detects_modified_input_files() {
    let directory = tempfile::tempdir().expect("tempdir");
    let artifact = directory.path().join("input.go");
    fs::write(&artifact, "package sample\n").expect("write initial file");
    let captured = capture_local_fixture(directory.path(), &[PathBuf::from("input.go")])
        .expect("capture artifact set");
    fs::write(&artifact, "package changed\n").expect("modify file");

    assert!(
        !captured
            .matches_workspace()
            .expect("compare modified workspace")
    );
}

#[test]
fn artifact_set_rejects_persisted_digest_mismatch_before_filesystem_recapture() {
    let directory = tempfile::tempdir().expect("tempdir");
    let artifact_set = ArtifactSetV1 {
        root: directory.path().to_path_buf(),
        binding: ArtifactBindingV1 {
            workspace_binding_ref: "workspace://fixture".to_owned(),
            scope_revision: 1,
            git_commit_ref: None,
        },
        entries: vec![ArtifactEntryV1 {
            path: PathBuf::from("input.go"),
            present: true,
            sha256: Some(
                "4c50c9bb85f4d5ce933ff172b9d5a2ae33d76c7e22b9d7dfe773511762a09ea2".to_owned(),
            ),
            executable: false,
        }],
        digest: "tampered-digest".to_owned(),
    };

    let error = artifact_set
        .matches_workspace()
        .expect_err("reject mismatched persisted fingerprint");
    assert!(error.to_string().contains("binding and entries"));
}

#[test]
fn go_json_observation_requires_selected_cases_to_have_run() {
    let mut stream = package_start_event("example.test/pkg");
    stream.push_str(&event("TestA", "run"));
    stream.push_str(&event("TestA", "pass"));
    stream.push_str(&package_event("example.test/pkg", "pass"));
    let observed = observe_go_test_json(
        stream.as_bytes(),
        true,
        Some(0),
        &expectation(&["TestA", "TestCachedButNotRun"]),
    )
    .expect("observe Go events");

    assert_eq!(observed.case_count, 1);
    assert_eq!(observed.selected_names, vec!["TestA"]);
    assert_eq!(observed.verdict, GoTestVerdict::Fail);
}

#[test]
fn go_json_selected_cases_pass_only_after_package_pass() {
    let mut stream = package_start_event("example.test/pkg");
    stream.push_str(&event("TestA", "run"));
    stream.push_str(&event("TestA", "pass"));
    stream.push_str(&package_event("example.test/pkg", "pass"));
    let observed = observe_go_test_json(stream.as_bytes(), true, Some(0), &expectation(&["TestA"]))
        .expect("observe successful complete test stream");

    assert_eq!(observed.verdict, GoTestVerdict::Pass);
    assert_eq!(observed.case_count, 1);
    assert_eq!(observed.passed, 1);
    assert!(observed.log_complete);
}

#[test]
fn go_json_zero_cases_and_all_skipped_cases_are_not_pass() {
    let zero_stream = concat!(
        "{\"Action\":\"start\",\"Package\":\"example.test/pkg\"}\n",
        "{\"Action\":\"pass\",\"Package\":\"example.test/pkg\"}\n",
    );
    let zero = observe_go_test_json(
        zero_stream.as_bytes(),
        true,
        Some(0),
        &expectation(&["TestA"]),
    )
    .expect("observe completed zero-case package");
    assert_eq!(zero.verdict, GoTestVerdict::Fail);
    assert!(zero.log_complete);

    let mut skipped_stream = package_start_event("example.test/pkg");
    skipped_stream.push_str(&event("TestA", "run"));
    skipped_stream.push_str(&event("TestA", "skip"));
    skipped_stream.push_str(&package_event("example.test/pkg", "pass"));
    let skipped = observe_go_test_json(
        skipped_stream.as_bytes(),
        true,
        Some(0),
        &expectation(&["TestA"]),
    )
    .expect("observe skipped stream");
    assert_eq!(skipped.verdict, GoTestVerdict::Skip);
    assert_eq!(skipped.skipped, 1);
}

#[test]
fn go_json_distinguishes_launch_failure_from_a_test_failure() {
    let mut failed_stream = package_start_event("example.test/pkg");
    failed_stream.push_str(&event("TestA", "run"));
    failed_stream.push_str(&event("TestA", "fail"));
    failed_stream.push_str(&package_event("example.test/pkg", "fail"));
    let failed = observe_go_test_json(
        failed_stream.as_bytes(),
        true,
        Some(1),
        &expectation(&["TestA"]),
    )
    .expect("observe test failure");
    assert_eq!(failed.verdict, GoTestVerdict::Fail);
    assert_eq!(failed.raw_exit_code, Some(1));

    let launch = observe_go_test_json(b"", false, None, &expectation(&["TestA"]))
        .expect("observe launch failure");
    assert_eq!(launch.verdict, GoTestVerdict::Error);
    assert!(!launch.launch_succeeded);
}

#[test]
fn go_json_package_failure_without_cases_is_a_framework_error() {
    let stream = format!(
        "{}{{\"Action\":\"output\",\"Package\":\"example.test/pkg\",\"Output\":\"# compile failure\\n\"}}\n{}",
        package_start_event("example.test/pkg"),
        package_event("example.test/pkg", "fail")
    );
    let observed = observe_go_test_json(stream.as_bytes(), true, Some(1), &expectation(&["TestA"]))
        .expect("observe framework failure");

    assert_eq!(observed.verdict, GoTestVerdict::Error);
    assert!(observed.framework_error);
    assert_eq!(observed.case_count, 0);
    assert_eq!(observed.raw_exit_code, Some(1));
}

#[test]
fn expected_baseline_failure_matches_reason_without_rewriting_raw_exit() {
    let stream = concat!(
        r#"{"Time":"2026-11-26T20:21:22.692258+09:00","Action":"start","Package":"example.test/common-harness-fixture"}"#,
        "\n",
        r#"{"Time":"2026-11-26T20:21:22.831482+09:00","Action":"run","Package":"example.test/common-harness-fixture","Test":"TestRegression"}"#,
        "\n",
        r#"{"Time":"2026-11-26T20:21:22.831531+09:00","Action":"output","Package":"example.test/common-harness-fixture","Test":"TestRegression","Output":"=== RUN   TestRegression\n"}"#,
        "\n",
        r#"{"Time":"2026-11-26T20:21:22.831545+09:00","Action":"output","Package":"example.test/common-harness-fixture","Test":"TestRegression","Output":"    sum_test.go:7: expected 4, got 3\n"}"#,
        "\n",
        r#"{"Time":"2026-11-26T20:21:22.831556+09:00","Action":"output","Package":"example.test/common-harness-fixture","Test":"TestRegression","Output":"--- FAIL: TestRegression (0.00s)\n"}"#,
        "\n",
        r#"{"Time":"2026-11-26T20:21:22.83156+09:00","Action":"fail","Package":"example.test/common-harness-fixture","Test":"TestRegression","Elapsed":0}"#,
        "\n",
        r#"{"Time":"2026-11-26T20:21:22.831564+09:00","Action":"output","Package":"example.test/common-harness-fixture","Output":"FAIL\n"}"#,
        "\n",
        r#"{"Time":"2026-11-26T20:21:22.831849+09:00","Action":"output","Package":"example.test/common-harness-fixture","Output":"FAIL\texample.test/common-harness-fixture\t0.139s\n"}"#,
        "\n",
        r#"{"Time":"2026-11-26T20:21:22.831863+09:00","Action":"fail","Package":"example.test/common-harness-fixture","Elapsed":0.14}"#,
        "\n",
    );
    let mut expected = expectation(&["TestRegression"]);
    expected.expected_baseline_failure = Some(ExpectedBaselineFailure {
        case_name: "TestRegression".to_owned(),
        reason: "expected 4, got 3".to_owned(),
    });

    let observed = observe_go_test_json(stream.as_bytes(), true, Some(1), &expected)
        .expect("observe expected baseline failure");
    assert_eq!(observed.verdict, GoTestVerdict::Pass);
    assert!(observed.matched_expected_baseline_failure);
    assert_eq!(observed.raw_exit_code, Some(1));
}

#[test]
fn malformed_or_truncated_go_json_is_an_error() {
    let malformed = observe_go_test_json(b"{bad json}\n", true, Some(1), &expectation(&["TestA"]))
        .expect("return typed malformed-stream observation");
    assert_eq!(malformed.verdict, GoTestVerdict::Error);
    assert!(!malformed.log_complete);

    let truncated = observe_go_test_json(
        b"{\"Action\":\"run\"",
        true,
        Some(1),
        &expectation(&["TestA"]),
    )
    .expect("return typed truncated-stream observation");
    assert_eq!(truncated.verdict, GoTestVerdict::Error);
    assert!(!truncated.log_complete);
}

#[test]
fn go_json_requires_a_package_terminal_event_after_case_terminals() {
    let mut stream = package_start_event("example.test/pkg");
    stream.push_str(&event("TestA", "run"));
    stream.push_str(&event("TestA", "pass"));
    let observed = observe_go_test_json(stream.as_bytes(), true, Some(0), &expectation(&["TestA"]))
        .expect("observe stream without package terminal");

    assert_eq!(observed.verdict, GoTestVerdict::Error);
    assert!(!observed.log_complete);
}

#[test]
fn go_json_rejects_duplicate_selected_case_names_across_packages() {
    let mut stream = package_start_event("example.test/pkg");
    stream.push_str(&event("TestA", "run"));
    stream.push_str(&event("TestA", "pass"));
    stream.push_str(&package_event("example.test/pkg", "pass"));
    stream.push_str(&package_start_event("example.test/other"));
    stream.push_str("{\"Action\":\"run\",\"Package\":\"example.test/other\",\"Test\":\"TestA\"}\n");
    stream
        .push_str("{\"Action\":\"pass\",\"Package\":\"example.test/other\",\"Test\":\"TestA\"}\n");
    stream.push_str(&package_event("example.test/other", "pass"));
    let observed = observe_go_test_json(stream.as_bytes(), true, Some(0), &expectation(&["TestA"]))
        .expect("observe duplicate case names");

    assert_eq!(observed.verdict, GoTestVerdict::Error);
}
