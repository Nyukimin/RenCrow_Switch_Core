// Modified by RenCrow Switch Core, 2026-11-27: test immutable dependency comparisons.

use std::fs;
use std::path::Path;
use std::path::PathBuf;

use super::DependencySubsetComparisonV1;
use super::compare_dependency_subset;
use crate::common_harness_io::ArtifactBindingV1;
use crate::common_harness_io::ArtifactSetV1;

fn capture(root: &Path, paths: &[&str]) -> ArtifactSetV1 {
    let binding = ArtifactBindingV1::for_canonical_root(root).expect("artifact binding");
    let paths = paths.iter().map(PathBuf::from).collect::<Vec<_>>();
    ArtifactSetV1::capture_bound(root, &paths, &binding).expect("artifact snapshot")
}

#[test]
fn unrelated_target_change_preserves_declared_dependency_subset() {
    let directory = tempfile::tempdir().expect("workspace");
    fs::write(directory.path().join("dependency.rs"), "stable input\n").expect("dependency");
    fs::write(directory.path().join("unrelated.rs"), "before\n").expect("unrelated input");
    let before = capture(directory.path(), &["dependency.rs", "unrelated.rs"]);

    fs::write(
        directory.path().join("unrelated.rs"),
        "changed unrelated input\n",
    )
    .expect("modify unrelated input");
    let after = capture(directory.path(), &["dependency.rs", "unrelated.rs"]);

    assert_ne!(before.digest, after.digest);
    assert_eq!(
        compare_dependency_subset(&before, &after, &[PathBuf::from("dependency.rs")])
            .expect("compare declared subset"),
        DependencySubsetComparisonV1::Unchanged
    );
}

#[test]
fn dependency_content_change_requires_rerun() {
    let directory = tempfile::tempdir().expect("workspace");
    fs::write(directory.path().join("dependency.rs"), "before\n").expect("dependency");
    let before = capture(directory.path(), &["dependency.rs"]);

    fs::write(directory.path().join("dependency.rs"), "after\n").expect("modify dependency");
    let after = capture(directory.path(), &["dependency.rs"]);

    assert_eq!(
        compare_dependency_subset(&before, &after, &[PathBuf::from("dependency.rs")])
            .expect("compare changed dependency"),
        DependencySubsetComparisonV1::Changed
    );
}

#[test]
#[cfg(unix)]
fn dependency_executable_mode_change_requires_rerun() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().expect("workspace");
    let dependency = directory.path().join("dependency.sh");
    fs::write(&dependency, "#!/bin/sh\nexit 0\n").expect("dependency");
    fs::set_permissions(&dependency, fs::Permissions::from_mode(0o644)).expect("non-executable");
    let before = capture(directory.path(), &["dependency.sh"]);

    fs::set_permissions(&dependency, fs::Permissions::from_mode(0o755)).expect("executable");
    let after = capture(directory.path(), &["dependency.sh"]);

    assert_eq!(
        compare_dependency_subset(&before, &after, &[PathBuf::from("dependency.sh")])
            .expect("compare changed mode"),
        DependencySubsetComparisonV1::Changed
    );
}

#[test]
fn dependency_presence_change_requires_rerun() {
    let directory = tempfile::tempdir().expect("workspace");
    let dependency = directory.path().join("dependency.rs");
    fs::write(&dependency, "input\n").expect("dependency");
    let before = capture(directory.path(), &["dependency.rs"]);

    fs::remove_file(&dependency).expect("remove dependency");
    let after = capture(directory.path(), &["dependency.rs"]);

    assert_eq!(
        compare_dependency_subset(&before, &after, &[PathBuf::from("dependency.rs")])
            .expect("compare changed presence"),
        DependencySubsetComparisonV1::Changed
    );
}

#[test]
fn explicit_absent_dependency_is_reusable_when_unchanged() {
    let directory = tempfile::tempdir().expect("workspace");
    let before = capture(directory.path(), &["dependency.rs"]);
    let after = capture(directory.path(), &["dependency.rs"]);

    assert_eq!(
        compare_dependency_subset(&before, &after, &[PathBuf::from("dependency.rs")])
            .expect("compare explicit absent dependency"),
        DependencySubsetComparisonV1::Unchanged
    );
}

#[test]
fn missing_declared_path_in_either_snapshot_is_not_reusable() {
    let directory = tempfile::tempdir().expect("workspace");
    fs::write(directory.path().join("dependency.rs"), "input\n").expect("dependency");
    fs::write(directory.path().join("other.rs"), "input\n").expect("other input");
    let before = capture(directory.path(), &["dependency.rs", "other.rs"]);
    let after = capture(directory.path(), &["other.rs"]);

    assert!(compare_dependency_subset(&before, &after, &[PathBuf::from("dependency.rs")]).is_err());
}

#[test]
fn different_binding_or_workspace_root_is_not_reusable() {
    let first = tempfile::tempdir().expect("first workspace");
    let second = tempfile::tempdir().expect("second workspace");
    fs::write(first.path().join("dependency.rs"), "input\n").expect("first dependency");
    fs::write(second.path().join("dependency.rs"), "input\n").expect("second dependency");
    let before = capture(first.path(), &["dependency.rs"]);
    let after_other_root = ArtifactSetV1::capture_bound(
        second.path(),
        &[PathBuf::from("dependency.rs")],
        &before.binding,
    )
    .expect("snapshot with same binding at another root");

    assert!(
        compare_dependency_subset(
            &before,
            &after_other_root,
            &[PathBuf::from("dependency.rs")]
        )
        .is_err()
    );

    let mut different_binding = before.binding.clone();
    different_binding.scope_revision += 1;
    let after_other_binding = ArtifactSetV1::capture_bound(
        first.path(),
        &[PathBuf::from("dependency.rs")],
        &different_binding,
    )
    .expect("snapshot with distinct scope revision");
    assert!(
        compare_dependency_subset(
            &before,
            &after_other_binding,
            &[PathBuf::from("dependency.rs")]
        )
        .is_err()
    );
}

#[test]
fn tampered_snapshot_digest_is_rejected() {
    let directory = tempfile::tempdir().expect("workspace");
    fs::write(directory.path().join("dependency.rs"), "input\n").expect("dependency");
    let before = capture(directory.path(), &["dependency.rs"]);
    let mut tampered = before.clone();
    tampered.entries[0].sha256 = Some("0".repeat(64));

    assert!(
        compare_dependency_subset(&before, &tampered, &[PathBuf::from("dependency.rs")]).is_err()
    );

    let mut noncanonical_root = before.clone();
    noncanonical_root.root = before.root.join(".");
    assert!(
        compare_dependency_subset(
            &before,
            &noncanonical_root,
            &[PathBuf::from("dependency.rs")]
        )
        .is_err()
    );
}
