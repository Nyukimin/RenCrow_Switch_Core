// Modified by RenCrow Switch Core, 2026-11-26: host-side patch intent prediction and reconciliation.
use super::ArtifactEntryV1;
use super::ArtifactSetV1;
use super::PatchJournalBindingV1;
use super::PatchJournalIntentV1;
use super::PatchReconciliationV1;
use anyhow::Context;
use anyhow::Result;
use codex_apply_patch::ApplyPatchAction;
use codex_apply_patch::ApplyPatchFileChange;
use codex_utils_path_uri::PathUri;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

pub(super) fn prepare_patch_journal(
    binding: &PatchJournalBindingV1,
    action: &ApplyPatchAction,
) -> Result<PatchJournalIntentV1> {
    validate_patch_binding(binding)?;
    let workspace_root = fs::canonicalize(&binding.workspace_root)
        .context("canonicalize patch journal workspace")?;
    anyhow::ensure!(
        workspace_root == binding.workspace_root,
        "patch journal workspace root is not canonical"
    );
    let action_cwd = action
        .cwd
        .to_abs_path()
        .context("resolve verified patch action cwd")?;
    let action_cwd_path = action_cwd.as_path().to_path_buf();
    let canonical_action_cwd =
        fs::canonicalize(&action_cwd_path).context("canonicalize verified patch action cwd")?;
    anyhow::ensure!(
        canonical_action_cwd == workspace_root,
        "patch action cwd does not match the bound workspace path"
    );

    let artifact_paths = normalize_scope_paths(&binding.artifact_paths)?;
    let before = ArtifactSetV1::capture_bound(
        &workspace_root,
        &binding.artifact_paths,
        &binding.artifact_binding,
    )?;
    let mut after_by_path = before
        .entries
        .iter()
        .cloned()
        .map(|entry| {
            Ok((
                super::artifact::normalize_relative_path(&entry.path)?,
                entry,
            ))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    let mut touched_paths = BTreeSet::new();
    anyhow::ensure!(
        !action.changes().is_empty(),
        "patch action contains no file changes"
    );

    for (source_uri, change) in action.changes() {
        let source_path = uri_relative_path(source_uri, &action_cwd_path)?;
        let source_key = super::artifact::normalize_relative_path(&source_path)?;
        validate_scoped_target(&source_key, &artifact_paths)?;
        anyhow::ensure!(
            touched_paths.insert(source_key.clone()),
            "patch action contains duplicate source paths"
        );
        match change {
            ApplyPatchFileChange::Add { content } => {
                let prior = after_by_path
                    .get(&source_key)
                    .context("added patch path is absent from the artifact preimage")?;
                let prior_executable = prior.present.then_some(prior.executable);
                after_by_path.insert(
                    source_key,
                    file_entry(&source_path, content.as_bytes(), prior_executable)?,
                );
            }
            ApplyPatchFileChange::Delete { content } => {
                let prior = after_by_path
                    .get(&source_key)
                    .context("deleted patch path is absent from the artifact preimage")?;
                anyhow::ensure!(prior.present, "patch delete source is not a regular file");
                anyhow::ensure!(
                    prior.sha256.as_deref()
                        == Some(super::artifact::content_sha256(content.as_bytes()).as_str()),
                    "patch delete source changed after verification"
                );
                after_by_path.insert(source_key, absent_entry(&source_path));
            }
            ApplyPatchFileChange::Update {
                new_content,
                move_path,
                ..
            } => {
                let prior = after_by_path
                    .get(&source_key)
                    .context("updated patch path is absent from the artifact preimage")?;
                anyhow::ensure!(prior.present, "patch update source is not a regular file");
                let prior_executable = prior.executable;
                if let Some(move_uri) = move_path {
                    let move_path = uri_relative_path(move_uri, &action_cwd_path)?;
                    let move_key = super::artifact::normalize_relative_path(&move_path)?;
                    validate_scoped_target(&move_key, &artifact_paths)?;
                    anyhow::ensure!(
                        move_key != source_key && touched_paths.insert(move_key.clone()),
                        "patch move target collides with another changed path"
                    );
                    let destination = after_by_path
                        .get(&move_key)
                        .context("patch move target is absent from the artifact preimage")?;
                    let destination_executable =
                        destination.present.then_some(destination.executable);
                    after_by_path.insert(source_key, absent_entry(&source_path));
                    after_by_path.insert(
                        move_key,
                        file_entry(&move_path, new_content.as_bytes(), destination_executable)?,
                    );
                } else {
                    after_by_path.insert(
                        source_key,
                        file_entry(&source_path, new_content.as_bytes(), Some(prior_executable))?,
                    );
                }
            }
        }
    }

    let predicted_entries = after_by_path.into_values().collect::<Vec<_>>();
    let predicted_digest =
        super::artifact::fingerprint_digest(&binding.artifact_binding, &predicted_entries)?;
    let predicted_after = ArtifactSetV1 {
        root: workspace_root,
        binding: binding.artifact_binding.clone(),
        entries: predicted_entries,
        digest: predicted_digest,
    };
    Ok(PatchJournalIntentV1 {
        binding: binding.clone(),
        before,
        predicted_after,
    })
}

pub(super) fn reconcile_patch_journal(
    intent: &PatchJournalIntentV1,
) -> Result<PatchReconciliationV1> {
    validate_patch_binding(&intent.binding)?;
    validate_snapshot(&intent.before, &intent.binding)?;
    validate_snapshot(&intent.predicted_after, &intent.binding)?;
    anyhow::ensure!(
        intent
            .before
            .entries
            .iter()
            .map(|entry| &entry.path)
            .eq(intent
                .predicted_after
                .entries
                .iter()
                .map(|entry| &entry.path)),
        "patch journal preimage and postimage scopes differ"
    );
    let current = ArtifactSetV1::capture_bound(
        &intent.binding.workspace_root,
        &intent.binding.artifact_paths,
        &intent.binding.artifact_binding,
    )?;
    if same_snapshot(&current, &intent.before) {
        Ok(PatchReconciliationV1::AllPre)
    } else if same_snapshot(&current, &intent.predicted_after) {
        Ok(PatchReconciliationV1::AllPost)
    } else {
        Ok(PatchReconciliationV1::PartialOrOther)
    }
}

fn validate_patch_binding(binding: &PatchJournalBindingV1) -> Result<()> {
    anyhow::ensure!(
        !binding.task_id.trim().is_empty()
            && binding.request_revision > 0
            && binding.attempt_no > 0
            && binding.owner_epoch > 0
            && binding.expected_state_version > 0
            && !binding.operation_id.trim().is_empty(),
        "patch journal binding identity is incomplete"
    );
    anyhow::ensure!(
        !binding.artifact_paths.is_empty()
            && binding.artifact_paths.len() <= super::MAX_ARTIFACT_FILES,
        "patch journal artifact scope is empty or exceeds the bounded file limit"
    );
    let canonical_root = fs::canonicalize(&binding.workspace_root)
        .context("canonicalize patch journal binding root")?;
    anyhow::ensure!(
        canonical_root == binding.workspace_root,
        "patch journal workspace root is not canonical"
    );
    Ok(())
}

fn normalize_scope_paths(paths: &[PathBuf]) -> Result<BTreeSet<String>> {
    let mut normalized = BTreeSet::new();
    for path in paths {
        let path = super::artifact::normalize_relative_path(path)?;
        anyhow::ensure!(
            normalized.insert(path),
            "patch journal artifact scope has duplicate paths"
        );
    }
    Ok(normalized)
}

fn validate_scoped_target(path: &str, scope: &BTreeSet<String>) -> Result<()> {
    anyhow::ensure!(
        scope.contains(path),
        "patch target path is outside the registered artifact scope"
    );
    Ok(())
}

fn uri_relative_path(uri: &PathUri, action_cwd: &Path) -> Result<PathBuf> {
    let absolute = uri.to_abs_path().context("resolve patch action path")?;
    let relative = absolute
        .as_path()
        .strip_prefix(action_cwd)
        .context("patch action path is outside the workspace root")?;
    let normalized = super::artifact::normalize_relative_path(relative)?;
    Ok(PathBuf::from(normalized))
}

fn file_entry(path: &Path, contents: &[u8], executable: Option<bool>) -> Result<ArtifactEntryV1> {
    Ok(ArtifactEntryV1 {
        path: PathBuf::from(super::artifact::normalize_relative_path(path)?),
        present: true,
        sha256: Some(super::artifact::content_sha256(contents)),
        executable: executable.unwrap_or(false),
    })
}

fn absent_entry(path: &Path) -> ArtifactEntryV1 {
    ArtifactEntryV1 {
        path: path.to_path_buf(),
        present: false,
        sha256: None,
        executable: false,
    }
}

fn validate_snapshot(snapshot: &ArtifactSetV1, binding: &PatchJournalBindingV1) -> Result<()> {
    let canonical_root = fs::canonicalize(&binding.workspace_root)
        .context("canonicalize patch journal workspace while reconciling")?;
    anyhow::ensure!(
        snapshot.root == canonical_root,
        "patch journal snapshot root changed"
    );
    anyhow::ensure!(
        snapshot.binding == binding.artifact_binding,
        "patch journal snapshot binding changed"
    );
    let expected_paths = normalize_scope_paths(&binding.artifact_paths)?;
    let snapshot_paths = snapshot
        .entries
        .iter()
        .map(|entry| super::artifact::normalize_relative_path(&entry.path))
        .collect::<Result<BTreeSet<_>>>()?;
    anyhow::ensure!(
        snapshot_paths == expected_paths && snapshot.entries.len() == expected_paths.len(),
        "patch journal snapshot does not cover its bound artifact scope"
    );
    let expected_digest =
        super::artifact::fingerprint_digest(&snapshot.binding, &snapshot.entries)?;
    anyhow::ensure!(
        expected_digest == snapshot.digest,
        "patch journal snapshot digest does not match its entries"
    );
    Ok(())
}

fn same_snapshot(left: &ArtifactSetV1, right: &ArtifactSetV1) -> bool {
    left.binding == right.binding && left.digest == right.digest && left.entries == right.entries
}
