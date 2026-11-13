//! Bounded identities for parent-originated child turn-start dispatches.

use codex_protocol::ThreadId;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use std::collections::HashSet;

pub(crate) const MAX_CHILD_DISPATCHES_PER_TURN: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ChildDispatchKind {
    V1SendInput { interrupt: bool },
    V2FollowupTask,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ChildDispatchKey {
    target: ThreadId,
    kind: ChildDispatchKind,
    fingerprint: [u8; 32],
}

pub(crate) fn fingerprint<T: Serialize>(
    target: ThreadId,
    kind: ChildDispatchKind,
    payload: &T,
) -> Result<ChildDispatchKey, serde_json::Error> {
    let payload = serde_json::to_vec(payload)?;
    let mut digest = Sha256::new();
    let kind_tag: &[u8] = match kind {
        ChildDispatchKind::V1SendInput { interrupt: false } => b"v1-send-input\0".as_slice(),
        ChildDispatchKind::V1SendInput { interrupt: true } => b"v1-send-input-interrupt\0",
        ChildDispatchKind::V2FollowupTask => b"v2-followup-task\0",
    };
    digest.update(kind_tag);
    digest.update(payload);
    Ok(ChildDispatchKey {
        target,
        kind,
        fingerprint: digest.finalize().into(),
    })
}

#[derive(Debug, Default)]
pub(crate) struct ChildDispatchBook {
    reservations: HashSet<ChildDispatchKey>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChildDispatchReservation {
    Reserved,
    Duplicate,
    CapacityReached,
}

impl ChildDispatchBook {
    pub(crate) fn reserve(&mut self, key: ChildDispatchKey) -> ChildDispatchReservation {
        if self.reservations.contains(&key) {
            return ChildDispatchReservation::Duplicate;
        }
        if self.reservations.len() >= MAX_CHILD_DISPATCHES_PER_TURN {
            return ChildDispatchReservation::CapacityReached;
        }
        self.reservations.insert(key);
        ChildDispatchReservation::Reserved
    }

    pub(crate) fn clear(&mut self) {
        self.reservations.clear();
    }
}

#[cfg(test)]
#[path = "child_dispatch_tests.rs"]
mod tests;
