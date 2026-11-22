// Modified by RenCrow Switch Core, 2026-11-21: retain bounded accepted task revision identities.
use super::AcceptedTaskReceipt;
use super::CompletionEvidenceRef;
use super::CompletionState;
use super::MAX_TEXT_BYTES;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

const MAX_ACCEPTED_TASK_EVIDENCE_HISTORY: usize = 1024;

/// Exact identity of an older host-accepted task receipt referenced by completion history.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptedTaskEvidenceReceipt {
    pub task_id: String,
    pub task_revision: u64,
    pub artifact_ref: String,
    pub verification_ref: String,
}

/// Identifies whether evidence names the current receipt, exact history, or a refreshable revision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AcceptedTaskEvidenceResolution<'a> {
    Current(&'a AcceptedTaskReceipt),
    Historical(&'a AcceptedTaskEvidenceReceipt),
    CurrentRevision(&'a AcceptedTaskReceipt),
}

/// Resolve an evidence identity against the latest host receipt or exact retained history.
pub fn resolve_accepted_task_evidence<'a>(
    evidence: &CompletionEvidenceRef,
    current: &'a BTreeMap<String, AcceptedTaskReceipt>,
    history: &'a [AcceptedTaskEvidenceReceipt],
) -> Option<AcceptedTaskEvidenceResolution<'a>> {
    if let Some(receipt) = history.iter().find(|receipt| {
        receipt.task_id == evidence.task_id
            && receipt.task_revision == evidence.task_revision
            && receipt.artifact_ref == evidence.artifact_ref
            && receipt.verification_ref == evidence.verification_ref
    }) {
        return Some(AcceptedTaskEvidenceResolution::Historical(receipt));
    }
    let receipt = current.get(&evidence.task_id)?;
    if receipt.revision != evidence.task_revision {
        return None;
    }
    Some(
        if receipt.artifact_ref == evidence.artifact_ref
            && receipt.verification_ref == evidence.verification_ref
        {
            AcceptedTaskEvidenceResolution::Current(receipt)
        } else {
            AcceptedTaskEvidenceResolution::CurrentRevision(receipt)
        },
    )
}

pub(super) fn validate_accepted_history(
    state: Option<&CompletionState>,
    current: &BTreeMap<String, AcceptedTaskReceipt>,
    history: &[AcceptedTaskEvidenceReceipt],
) -> Result<(), String> {
    if history.len() > MAX_ACCEPTED_TASK_EVIDENCE_HISTORY {
        return Err("stored accepted task evidence history exceeds its limit".into());
    }
    let evidence_refs = completion_evidence_refs(state).collect::<Vec<_>>();
    let mut identities = BTreeSet::new();
    for receipt in history {
        if receipt.task_id.trim().is_empty()
            || receipt.task_id.len() > MAX_TEXT_BYTES
            || receipt.task_revision == 0
            || receipt.artifact_ref.trim().is_empty()
            || receipt.artifact_ref.len() > MAX_TEXT_BYTES
            || receipt.verification_ref.trim().is_empty()
            || receipt.verification_ref.len() > MAX_TEXT_BYTES
            || !identities.insert((
                receipt.task_id.as_str(),
                receipt.task_revision,
                receipt.artifact_ref.as_str(),
                receipt.verification_ref.as_str(),
            ))
        {
            return Err(
                "stored accepted task evidence history has an invalid or duplicate identity".into(),
            );
        }
        if current.get(&receipt.task_id).is_some_and(|latest| {
            latest.revision == receipt.task_revision
                && latest.artifact_ref == receipt.artifact_ref
                && latest.verification_ref == receipt.verification_ref
        }) {
            return Err(
                "stored accepted task evidence history duplicates a current receipt".into(),
            );
        }
        if !evidence_refs
            .iter()
            .any(|evidence| receipt_identity_matches_evidence(receipt, evidence))
        {
            return Err(
                "stored accepted task evidence history is not referenced by completion state"
                    .into(),
            );
        }
    }
    Ok(())
}

pub(super) fn retain_referenced_task_evidence(
    state: Option<&CompletionState>,
    current: &BTreeMap<String, AcceptedTaskReceipt>,
    sources: Vec<AcceptedTaskEvidenceReceipt>,
) -> Result<Vec<AcceptedTaskEvidenceReceipt>, String> {
    let evidence_refs = completion_evidence_refs(state).collect::<Vec<_>>();
    let mut identities = BTreeSet::new();
    let mut retained = Vec::new();
    for receipt in sources {
        if current.get(&receipt.task_id).is_some_and(|latest| {
            latest.revision == receipt.task_revision
                && latest.artifact_ref == receipt.artifact_ref
                && latest.verification_ref == receipt.verification_ref
        }) || !evidence_refs
            .iter()
            .any(|evidence| receipt_identity_matches_evidence(&receipt, evidence))
        {
            continue;
        }
        let identity = (
            receipt.task_id.clone(),
            receipt.task_revision,
            receipt.artifact_ref.clone(),
            receipt.verification_ref.clone(),
        );
        if identities.insert(identity) {
            retained.push(receipt);
        }
    }
    if retained.len() > MAX_ACCEPTED_TASK_EVIDENCE_HISTORY {
        return Err("accepted task evidence history exceeds its limit".into());
    }
    retained.sort_by(|left, right| {
        (
            &left.task_id,
            left.task_revision,
            &left.artifact_ref,
            &left.verification_ref,
        )
            .cmp(&(
                &right.task_id,
                right.task_revision,
                &right.artifact_ref,
                &right.verification_ref,
            ))
    });
    Ok(retained)
}

fn completion_evidence_refs(
    state: Option<&CompletionState>,
) -> impl Iterator<Item = &CompletionEvidenceRef> {
    state.into_iter().flat_map(|state| {
        state
            .conditions
            .iter()
            .flat_map(|condition| condition.evidence.iter())
            .chain(
                state
                    .units
                    .iter()
                    .flat_map(|unit| unit.evidence.iter().chain(unit.task_refs.iter())),
            )
            .chain(
                state
                    .blocker
                    .iter()
                    .flat_map(|blocker| blocker.evidence.iter()),
            )
    })
}

fn receipt_identity_matches_evidence(
    receipt: &AcceptedTaskEvidenceReceipt,
    evidence: &CompletionEvidenceRef,
) -> bool {
    receipt.task_id == evidence.task_id
        && receipt.task_revision == evidence.task_revision
        && receipt.artifact_ref == evidence.artifact_ref
        && receipt.verification_ref == evidence.verification_ref
}
