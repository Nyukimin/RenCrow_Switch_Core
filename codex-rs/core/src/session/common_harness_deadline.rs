// Modified by RenCrow Switch Core, 2026-11-27: bound inference without aborting tool settlement.
use super::session::Session;
use super::turn_context::TurnContext;
use codex_history::task_context::CommonHarnessStateV1;
use codex_history::task_context::HostEventV1;
use codex_history::task_context::TaskRunStatusV1;
use codex_history::task_context::WorkWindowV1;
use std::future::Future;
use std::time::Duration;
use tokio::time::Instant;

// New initial S1 policy, independent of the existing reviewer SLA and provider idle timeout.
const S1_WORK_WINDOW_MS: u64 = 600_000;
pub(crate) const WORK_WINDOW_EXPIRED: &str = "common-harness absolute work window expired";

pub(super) fn work_window_now_ms() -> Result<i64, String> {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| format!("read common-harness work clock: {error}"))?;
    i64::try_from(elapsed.as_millis()).map_err(|_| "work clock exceeds supported range".into())
}

/// The same absolute window covers stream creation, deltas, and compaction requests.
pub(crate) async fn within_work_window<T>(
    deadline: Option<Instant>,
    future: impl Future<Output = T>,
) -> Result<T, String> {
    let Some(deadline) = deadline else {
        return Ok(future.await);
    };
    tokio::select! {
        biased;
        _ = tokio::time::sleep_until(deadline) => Err(WORK_WINDOW_EXPIRED.into()),
        result = future => Ok(result),
    }
}

impl Session {
    pub(crate) async fn common_harness_inference_deadline(
        &self,
        turn: &TurnContext,
    ) -> Result<Option<Instant>, String> {
        let Some((root, state)) = self.common_harness_turn_state(turn).await? else {
            return Ok(None);
        };
        if let Some(reason) = super::common_harness_completion::common_harness_stop_reason(&state) {
            return Err(reason);
        }
        if state.run_state.status == TaskRunStatusV1::Completed {
            return Ok(None);
        }
        if !self.services.rollout_thread_trace.is_enabled() {
            // Preserve an already-expired durable deadline as the primary stop
            // reason; otherwise block before starting a queued attempt/window.
            if let Some(window) = state.run_state.work_window.as_ref() {
                let remaining_ms = window.deadline_at_ms - work_window_now_ms()?;
                if remaining_ms <= 0 {
                    self.apply_common_harness_host_event(
                        &root,
                        super::common_harness::common_harness_fence(&state),
                        HostEventV1::Block {
                            reason_code: "work_window_expired".into(),
                            resume_condition: "Reconcile any started operations and authorize a new work budget before resuming inference.".into(),
                        },
                    )
                    .await?;
                    return Err(WORK_WINDOW_EXPIRED.into());
                }
            }
            // common_harness_turn_state has already validated/recovered the
            // current local-writer owner. Apply this block under that fence.
            self.apply_common_harness_host_event(
                &root,
                super::common_harness::common_harness_fence(&state),
                HostEventV1::Block {
                    reason_code: "trace_unavailable".into(),
                    resume_condition: "Restart with CODEX_ROLLOUT_TRACE_ROOT set to a writable trace directory, then explicitly recover the blocked task.".into(),
                },
            )
            .await?;
            return Err("common-harness inference blocked: rollout trace is unavailable".into());
        }
        let state = self.ensure_common_harness_attempt(&root).await?;
        let state = self.bind_common_harness_work_window(&root, state).await?;
        let window = state.run_state.work_window.as_ref().ok_or_else(|| {
            "active common-harness inference has no durable work window".to_string()
        })?;
        let remaining_ms = window.deadline_at_ms - work_window_now_ms()?;
        if remaining_ms <= 0 {
            self.apply_common_harness_host_event(
                &root,
                super::common_harness::common_harness_fence(&state),
                HostEventV1::Block {
                    reason_code: "work_window_expired".into(),
                    resume_condition: "Reconcile any started operations and authorize a new work budget before resuming inference.".into(),
                },
            )
            .await?;
            return Err(WORK_WINDOW_EXPIRED.into());
        }
        Ok(Some(
            Instant::now() + Duration::from_millis(remaining_ms as u64),
        ))
    }

    pub(super) async fn bind_common_harness_work_window(
        &self,
        root: &str,
        state: CommonHarnessStateV1,
    ) -> Result<CommonHarnessStateV1, String> {
        if state.run_state.work_window.is_some()
            || !matches!(
                state.run_state.status,
                TaskRunStatusV1::Running | TaskRunStatusV1::Verifying
            )
        {
            return Ok(state);
        }
        let started_at_ms = work_window_now_ms()?;
        let deadline_at_ms = started_at_ms
            .checked_add(S1_WORK_WINDOW_MS as i64)
            .ok_or_else(|| "common-harness work deadline exceeds supported range".to_string())?;
        self.apply_common_harness_host_event(
            root,
            super::common_harness::common_harness_fence(&state),
            HostEventV1::BindWorkWindow {
                window: WorkWindowV1 {
                    window_id: format!("work-window:{}", uuid::Uuid::now_v7()),
                    attempt_no: state.run_state.attempt_no,
                    started_at_ms,
                    deadline_at_ms,
                    timeout_ms: S1_WORK_WINDOW_MS,
                    policy_ref: "common-harness/s1-parent-window/1".into(),
                },
            },
        )
        .await
    }
}

#[cfg(test)]
#[path = "common_harness_deadline_unit_tests.rs"]
mod tests;
