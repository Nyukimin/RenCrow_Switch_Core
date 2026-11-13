// Modified by RenCrow Switch Core, 2026-11-13: generation-scoped sampling timing.
use super::ChildMonitor;
use super::ChildMonitorPhase;
use super::ChildMonitorSnapshot;

pub(crate) enum SamplingTimingEvent {
    SeriesStarted,
    RequestStarted { stream_idle_timeout_ms: i64 },
    RequestFinished,
    RetryDecided,
    SeriesFinished,
}

impl ChildMonitor {
    pub(crate) fn observe_timing(&mut self, turn_id: &str, event: SamplingTimingEvent, at_ms: i64) {
        if self.snapshot.turn_id.as_deref() != Some(turn_id) {
            return;
        }
        match event {
            SamplingTimingEvent::SeriesStarted => self.request_timing.begin_series(at_ms),
            SamplingTimingEvent::RequestStarted {
                stream_idle_timeout_ms,
            } => {
                self.request_timing
                    .request_started(at_ms, Some(stream_idle_timeout_ms));
                if self.active_tool_calls.is_empty()
                    && self.pending_inputs.is_empty()
                    && !self.compaction_active
                    && !self.tracking_overflowed
                    && !matches!(
                        self.snapshot.phase,
                        ChildMonitorPhase::Completed | ChildMonitorPhase::Failed
                    )
                {
                    self.set_phase(ChildMonitorPhase::Pending, None);
                }
            }
            SamplingTimingEvent::RequestFinished => self.request_timing.request_finished(at_ms),
            SamplingTimingEvent::RetryDecided => self.request_timing.retry_decided(at_ms),
            SamplingTimingEvent::SeriesFinished => self.request_timing.finish_series(at_ms),
        }
    }

    pub(crate) fn snapshot_at(&self, now_ms: i64) -> ChildMonitorSnapshot {
        let mut snapshot = self.snapshot();
        snapshot.timing = self.request_timing.snapshot(now_ms);
        snapshot
    }
}
