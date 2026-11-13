use super::ChildTimingSnapshot;
use super::RequestTiming;

#[test]
fn request_output_and_snapshot_reads_preserve_observed_times() {
    let mut timing = RequestTiming::default();
    timing.begin_series(100);
    timing.request_started(110, Some(50));

    let waiting = timing.snapshot(160);
    assert_eq!(waiting.first_output_wait_ms, Some(50));
    assert_eq!(waiting.current_request_first_output_at_ms, None);
    assert_eq!(timing.snapshot(170).first_output_wait_ms, Some(60));

    timing.output_observed(145);
    assert_eq!(
        timing.snapshot(200),
        ChildTimingSnapshot {
            current_request_started_at_ms: Some(110),
            current_request_first_output_at_ms: Some(145),
            first_output_wait_ms: Some(35),
            stream_idle_timeout_ms: Some(50),
            ..ChildTimingSnapshot::default()
        }
    );
}

#[test]
fn retry_elapsed_is_cumulative_and_finish_is_idempotent() {
    let mut timing = RequestTiming::default();
    timing.begin_series(100);
    timing.request_started(110, None);
    timing.retry_decided(180);
    assert_eq!(timing.snapshot(250).retry_cumulative_ms, 150);

    timing.finish_series(300);
    timing.finish_series(400);
    assert_eq!(timing.snapshot(500).retry_cumulative_ms, 200);
    assert_eq!(timing.snapshot(500).sampling_retry_count, 1);
    assert_eq!(timing.snapshot(500).first_output_wait_ms, Some(190));

    // A normal post-tool model series contributes no retry time.
    timing.begin_series(600);
    timing.request_started(610, None);
    timing.finish_series(700);
    assert_eq!(timing.snapshot(900).retry_cumulative_ms, 200);
}
