use crate::agent::child_dispatch::ChildDispatchKey;
use crate::agent::child_dispatch::ChildDispatchReservation;
use codex_features::Feature;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChildDispatchGuardResult {
    Disabled,
    StaleTurn,
    Reserved,
    Duplicate,
    CapacityReached,
}

impl ChildDispatchGuardResult {
    pub(crate) fn refusal_message(self) -> Option<&'static str> {
        match self {
            Self::Disabled | Self::Reserved => None,
            Self::StaleTurn => {
                Some("child dispatch belongs to a stale parent turn and was not sent")
            }
            Self::Duplicate => Some(
                "unchanged child dispatch already attempted for this parent turn; inspect the child outcome or change the instructions/conditions before dispatching again",
            ),
            Self::CapacityReached => Some(
                "child dispatch guard reached its per-turn limit; inspect child outcomes before dispatching again",
            ),
        }
    }
}

impl super::session::Session {
    /// Reserves one parent-originated child turn-start action for the current parent turn.
    pub(crate) async fn reserve_child_dispatch(
        &self,
        turn_id: &str,
        key: ChildDispatchKey,
    ) -> ChildDispatchGuardResult {
        if !self.enabled(Feature::RencrowChildMonitor) {
            return ChildDispatchGuardResult::Disabled;
        }
        let mut state = self.state.lock().await;
        if state.last_started_turn_id.as_deref() != Some(turn_id) {
            return ChildDispatchGuardResult::StaleTurn;
        }
        match state.child_dispatch_book.reserve(key) {
            ChildDispatchReservation::Reserved => ChildDispatchGuardResult::Reserved,
            ChildDispatchReservation::Duplicate => ChildDispatchGuardResult::Duplicate,
            ChildDispatchReservation::CapacityReached => ChildDispatchGuardResult::CapacityReached,
        }
    }
}

#[cfg(test)]
#[path = "child_dispatch_tests.rs"]
mod tests;
