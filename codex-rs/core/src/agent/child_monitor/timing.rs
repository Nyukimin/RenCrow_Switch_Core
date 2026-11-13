// Modified by RenCrow Switch Core, 2026-11-13: independent request/first-output/retry budgets.
use serde::Deserialize;
use serde::Serialize;

/// Timing observations for the current model request and this turn's retries.
///
/// These values describe elapsed time and accepted retry decisions. They do
/// not imply that a child has stopped or should be cancelled.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct ChildTimingSnapshot {
    pub(crate) current_request_started_at_ms: Option<i64>,
    pub(crate) current_request_first_output_at_ms: Option<i64>,
    pub(crate) current_request_finished_at_ms: Option<i64>,
    pub(crate) first_output_wait_ms: Option<i64>,
    pub(crate) stream_idle_timeout_ms: Option<i64>,
    pub(crate) gateway_request_timeout_ms: Option<i64>,
    pub(crate) child_work_deadline_at_ms: Option<i64>,
    pub(crate) sampling_retry_count: u64,
    pub(crate) retry_cumulative_ms: i64,
}

/// Mutable clock state owned by one child turn's monitor.
#[derive(Debug, Clone, Default)]
pub(crate) struct RequestTiming {
    series_started_at_ms: Option<i64>,
    request_started_at_ms: Option<i64>,
    request_first_output_at_ms: Option<i64>,
    request_finished_at_ms: Option<i64>,
    stream_idle_timeout_ms: Option<i64>,
    total_retry_count: u64,
    series_retry_count: u64,
    completed_retry_ms: i64,
    series_finished: bool,
}

impl RequestTiming {
    /// Begin a model sampling series. A normal continuation after tool work is
    /// a new series; only `retry_decided` marks retries within that series.
    pub(crate) fn begin_series(&mut self, at_ms: i64) {
        if self.series_started_at_ms.is_some() && !self.series_finished {
            self.finish_series(at_ms);
        }
        self.series_started_at_ms = Some(at_ms);
        self.series_retry_count = 0;
        self.series_finished = false;
        self.request_started_at_ms = None;
        self.request_first_output_at_ms = None;
        self.request_finished_at_ms = None;
    }

    /// Record the start of an actual provider request, resetting only the
    /// request-scoped first-output measurement.
    pub(crate) fn request_started(&mut self, at_ms: i64, idle_timeout_ms: Option<i64>) {
        self.request_started_at_ms = Some(at_ms);
        self.request_first_output_at_ms = None;
        self.request_finished_at_ms = None;
        self.stream_idle_timeout_ms = idle_timeout_ms;
    }

    /// Record the first real output observed for the current request.
    pub(crate) fn output_observed(&mut self, at_ms: i64) {
        if self.request_started_at_ms.is_some()
            && self.request_first_output_at_ms.is_none()
            && self.request_finished_at_ms.is_none()
        {
            self.request_first_output_at_ms = Some(at_ms);
        }
    }

    pub(crate) fn request_finished(&mut self, at_ms: i64) {
        if self.request_started_at_ms.is_some() {
            self.request_finished_at_ms.get_or_insert(at_ms);
        }
    }

    /// Record one retry decision accepted by the sampling retry policy.
    pub(crate) fn retry_decided(&mut self, _at_ms: i64) {
        self.total_retry_count = self.total_retry_count.saturating_add(1);
        self.series_retry_count = self.series_retry_count.saturating_add(1);
    }

    /// Finish the current sampling series once. Retry-free series add no
    /// elapsed time to the retry cumulative, including ordinary tool followups.
    pub(crate) fn finish_series(&mut self, at_ms: i64) {
        if self.series_finished {
            return;
        }
        self.request_finished(at_ms);
        if self.series_retry_count > 0
            && let Some(started_at_ms) = self.series_started_at_ms
        {
            self.completed_retry_ms = self
                .completed_retry_ms
                .saturating_add(elapsed_ms(started_at_ms, at_ms));
        }
        self.series_finished = true;
    }

    /// Return elapsed measurements without changing any observed timestamps.
    pub(crate) fn snapshot(&self, now_ms: i64) -> ChildTimingSnapshot {
        let first_output_wait_ms = self.request_started_at_ms.map(|started_at_ms| {
            elapsed_ms(
                started_at_ms,
                self.request_first_output_at_ms
                    .or(self.request_finished_at_ms)
                    .unwrap_or(now_ms),
            )
        });
        let retry_cumulative_ms = if self.series_retry_count > 0 && !self.series_finished {
            self.completed_retry_ms.saturating_add(
                self.series_started_at_ms
                    .map_or(0, |started_at_ms| elapsed_ms(started_at_ms, now_ms)),
            )
        } else {
            self.completed_retry_ms
        };

        ChildTimingSnapshot {
            current_request_started_at_ms: self.request_started_at_ms,
            current_request_first_output_at_ms: self.request_first_output_at_ms,
            current_request_finished_at_ms: self.request_finished_at_ms,
            first_output_wait_ms,
            stream_idle_timeout_ms: self.stream_idle_timeout_ms,
            sampling_retry_count: self.total_retry_count,
            retry_cumulative_ms,
            ..ChildTimingSnapshot::default()
        }
    }
}

fn elapsed_ms(started_at_ms: i64, ended_at_ms: i64) -> i64 {
    ended_at_ms.saturating_sub(started_at_ms).max(0)
}

#[cfg(test)]
#[path = "timing_tests.rs"]
mod tests;
