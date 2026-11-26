// Modified by RenCrow Switch Core, 2026-11-27: absolute request deadline tests.
use super::within_work_window;
use std::time::Duration;
use tokio::time::Instant;

#[tokio::test]
async fn common_harness_deadline_interrupts_an_unfinished_request() {
    let result = tokio::time::timeout(
        Duration::from_millis(100),
        within_work_window(
            Some(Instant::now() + Duration::from_millis(5)),
            std::future::pending::<()>(),
        ),
    )
    .await;
    assert!(
        matches!(result, Ok(Err(_))),
        "absolute deadline must end the request"
    );
}

#[tokio::test]
async fn common_harness_deadline_is_not_extended_by_stream_progress() {
    let deadline = Instant::now() + Duration::from_millis(5);
    let result = tokio::time::timeout(Duration::from_millis(100), async {
        loop {
            if within_work_window(Some(deadline), async {
                tokio::time::sleep(Duration::from_millis(1)).await;
            })
            .await
            .is_err()
            {
                break;
            }
        }
    })
    .await;
    assert!(result.is_ok(), "deltas must not renew the absolute window");
}

#[tokio::test]
async fn common_harness_deadline_leaves_legacy_requests_unchanged() {
    assert_eq!(within_work_window(None, async { 42 }).await.unwrap(), 42);
}
