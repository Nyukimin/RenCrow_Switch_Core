// Modified by RenCrow Switch Core, 2026-11-27: compare immutable condition dependencies.
use super::ArtifactSetV1;
use anyhow::Result;
use std::path::Path;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DependencySubsetComparisonV1 {
    Unchanged,
    Changed,
}

pub(crate) fn compare_dependency_subset(
    before: &ArtifactSetV1,
    after: &ArtifactSetV1,
    declared_paths: &[PathBuf],
) -> Result<DependencySubsetComparisonV1> {
    validate_snapshot(before)?;
    validate_snapshot(after)?;
    anyhow::ensure!(
        before.root == after.root && before.binding == after.binding,
        "dependency snapshots have different workspace bindings"
    );
    anyhow::ensure!(
        !declared_paths.is_empty() && declared_paths.len() <= super::MAX_ARTIFACT_FILES,
        "dependency subset is empty or exceeds its bound"
    );
    let paths = declared_paths
        .iter()
        .map(|path| super::artifact::normalize_relative_path(path))
        .collect::<Result<std::collections::BTreeSet<_>>>()?;
    let mut changed = false;
    for path in paths {
        let path = std::path::Path::new(&path);
        let before_entry = before
            .entries
            .iter()
            .find(|entry| entry.path.as_path() == path)
            .ok_or_else(|| anyhow::anyhow!("dependency missing from before snapshot"))?;
        let after_entry = after
            .entries
            .iter()
            .find(|entry| entry.path.as_path() == path)
            .ok_or_else(|| anyhow::anyhow!("dependency missing from after snapshot"))?;
        changed |= before_entry != after_entry;
    }
    Ok(if changed {
        DependencySubsetComparisonV1::Changed
    } else {
        DependencySubsetComparisonV1::Unchanged
    })
}

fn validate_snapshot(snapshot: &ArtifactSetV1) -> Result<()> {
    let canonical_root = snapshot.root.canonicalize()?;
    anyhow::ensure!(
        snapshot.root.is_absolute()
            && snapshot.root.is_dir()
            && canonical_root.as_os_str() == snapshot.root.as_os_str(),
        "dependency snapshot workspace is not canonical"
    );
    anyhow::ensure!(
        !snapshot.entries.is_empty() && snapshot.entries.len() <= super::MAX_ARTIFACT_FILES,
        "dependency snapshot entry count is invalid"
    );
    anyhow::ensure!(
        super::artifact::fingerprint_digest(&snapshot.binding, &snapshot.entries)?
            == snapshot.digest,
        "dependency snapshot digest is invalid"
    );
    let mut previous: Option<String> = None;
    for entry in &snapshot.entries {
        let path = super::artifact::normalize_relative_path(&entry.path)?;
        anyhow::ensure!(
            entry.path.as_path() == Path::new(&path),
            "dependency path is not canonical"
        );
        anyhow::ensure!(
            previous
                .as_ref()
                .is_none_or(|previous| previous.as_bytes() < path.as_bytes()),
            "dependency entries are unordered or duplicated"
        );
        anyhow::ensure!(
            entry.present || (!entry.executable && entry.sha256.is_none()),
            "absent dependency has file metadata"
        );
        previous = Some(path);
    }
    Ok(())
}

#[cfg(test)]
#[path = "dependency_subset_tests.rs"]
mod tests;
