//! Display product acceptance separately from process activity using the existing plan UI.
// Modified by RenCrow Switch Core, 2026-11-11.

use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use chrono::DateTime;
use chrono::FixedOffset;
use chrono::Utc;
use codex_history::task_context::AcceptanceStatus;
use codex_history::task_context::CompletionState;
use codex_protocol::plan_tool::PlanItemArg;
use codex_protocol::plan_tool::StepStatus;
use codex_protocol::plan_tool::UpdatePlanArgs;
use codex_protocol::protocol::EventMsg;

fn jst_time(value: Option<i64>) -> String {
    value
        .and_then(|timestamp| DateTime::<Utc>::from_timestamp(timestamp, 0))
        .and_then(|time| {
            FixedOffset::east_opt(9 * 3600).map(|offset| {
                time.with_timezone(&offset)
                    .format("%m-%d %H:%M:%S JST")
                    .to_string()
            })
        })
        .unwrap_or_else(|| "未記録".to_string())
}

pub(crate) async fn report(session: &Session, turn: &TurnContext, state: &CompletionState) {
    let achieved = state
        .conditions
        .iter()
        .filter(|condition| {
            matches!(
                condition.status,
                AcceptanceStatus::Satisfied | AcceptanceStatus::Waived
            )
        })
        .count();
    let blocker = state
        .blocker
        .as_ref()
        .map(|item| item.reason.as_str())
        .unwrap_or("なし");
    let current = state.current_unit.as_deref().unwrap_or("未選択");
    let explanation = format!(
        "[{}] 製品受入 {}/{} · 作業 {} · 停止 {} · 阻害: {}\n最終製品進展: {} · 復旧: {} · 活動: {}\n計画見直し基準: {} · 以後の受入小課題: {}/2（製品進展なし30分でも見直し）",
        jst_time(Some(Utc::now().timestamp())),
        achieved,
        state.conditions.len(),
        current,
        state.paused,
        blocker,
        jst_time(state.product_progress_at),
        jst_time(state.runtime_recovery_at),
        jst_time(state.activity_at),
        jst_time(state.plan_review_at),
        state.units_completed_since_review,
    );
    let plan = state
        .conditions
        .iter()
        .map(|condition| PlanItemArg {
            step: format!("{}: {}", condition.id, condition.result),
            status: match condition.status {
                AcceptanceStatus::Satisfied | AcceptanceStatus::Waived => StepStatus::Completed,
                AcceptanceStatus::Open | AcceptanceStatus::Rejected => StepStatus::Pending,
            },
        })
        .collect();
    session
        .send_event(
            turn,
            EventMsg::PlanUpdate(UpdatePlanArgs {
                explanation: Some(explanation),
                plan,
            }),
        )
        .await;
}
