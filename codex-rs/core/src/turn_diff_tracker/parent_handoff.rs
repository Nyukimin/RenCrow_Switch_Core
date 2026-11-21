// Modified by RenCrow Switch Core, 2026-11-21.
use super::*;
use std::collections::BTreeMap;

/// Preserve the actual patch baseline, current contents and rename origins. Cached
/// rendered diffs are recomputed, never substituted for the mutation ledger.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ParentHandoffDiffTracker {
    display_roots: BTreeMap<String, PathUri>,
    baseline: Vec<(TrackedPath, TrackedContent)>,
    current: Vec<(TrackedPath, TrackedContent)>,
    origins: Vec<(TrackedPath, TrackedPath)>,
    next_revision: u64,
}

fn sorted_entries<T: Clone>(map: &HashMap<TrackedPath, T>) -> Vec<(TrackedPath, T)> {
    let mut entries = map
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<Vec<_>>();
    entries.sort_by(|(left, _), (right, _)| {
        (&left.environment_id, left.path.to_string())
            .cmp(&(&right.environment_id, right.path.to_string()))
    });
    entries
}

impl TurnDiffTracker {
    pub(crate) fn parent_handoff_snapshot(&self) -> Result<ParentHandoffDiffTracker, String> {
        if !self.valid {
            return Err("parent handoff cannot transfer an invalid patch tracker".into());
        }
        Ok(ParentHandoffDiffTracker {
            display_roots: self
                .display_roots_by_environment
                .clone()
                .into_iter()
                .collect(),
            baseline: sorted_entries(&self.baseline_by_path),
            current: sorted_entries(&self.current_by_path),
            origins: sorted_entries(&self.origin_by_current_path),
            next_revision: self.next_revision,
        })
    }
}

impl ParentHandoffDiffTracker {
    pub(crate) fn restore(&self) -> Result<TurnDiffTracker, String> {
        let baseline = self.baseline.iter().cloned().collect::<HashMap<_, _>>();
        let current = self.current.iter().cloned().collect::<HashMap<_, _>>();
        let origins = self.origins.iter().cloned().collect::<HashMap<_, _>>();
        if baseline.len() != self.baseline.len()
            || current.len() != self.current.len()
            || origins.len() != self.origins.len()
        {
            return Err("parent handoff patch tracker contains duplicate paths".into());
        }
        let mut tracker = TurnDiffTracker {
            display_roots_by_environment: self.display_roots.clone().into_iter().collect(),
            baseline_by_path: baseline,
            current_by_path: current,
            origin_by_current_path: origins,
            next_revision: self.next_revision,
            ..TurnDiffTracker::default()
        };
        tracker.refresh_unified_diff();
        Ok(tracker)
    }
}
