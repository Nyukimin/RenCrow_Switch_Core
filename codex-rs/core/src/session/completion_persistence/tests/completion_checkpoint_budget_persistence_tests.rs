// Modified by RenCrow Switch Core, 2026-11-20: separate caller and host checkpoint budgets.
use super::*;
use codex_history::task_context::PlanReviewRecord;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn completion_snapshot_envelope_keeps_shared_checkpoint_budget() {
    let (session, _) = make_session_and_context().await;
    let mut state = open_state(/*paused*/ false);
    state.next_action = None;
    let checkpoint_limit = codex_history::task_context::MAX_COMPLETION_CHECKPOINT_BYTES;

    while serde_json::to_vec(&state).unwrap().len() + 4_096 < checkpoint_limit {
        let state_size = serde_json::to_vec(&state).unwrap().len();
        let remaining = checkpoint_limit - state_size - 512;
        let field_size = (remaining / 3).min(4_096);
        state.plan_reviews.push(PlanReviewRecord {
            at: state.plan_reviews.len() as i64 + 1,
            previous_plan: "p".repeat(field_size),
            revised_plan: "r".repeat(field_size),
            reason: "e".repeat(field_size),
        });
    }
    loop {
        let state_size = serde_json::to_vec(&state).unwrap().len();
        if state_size + 1 >= checkpoint_limit {
            break;
        }
        let previous_action = state.next_action.clone();
        let action = state.next_action.get_or_insert_with(String::new);
        if action.len() == 4_096 {
            break;
        }
        action.push('x');
        if serde_json::to_vec(&state).unwrap().len() >= checkpoint_limit {
            state.next_action = previous_action;
            break;
        }
    }
    let completion_state_size = serde_json::to_vec(&state).unwrap().len();
    assert!(completion_state_size < checkpoint_limit);
    assert!(completion_state_size + 4_096 >= checkpoint_limit);

    session.state.lock().await.rencrow_completion_state = Some(("root".into(), state));
    let error = session
        .persist_autonomous_completion_snapshot()
        .await
        .expect_err("snapshot envelope includes host metadata beyond the state budget");
    assert_eq!(error, "completion snapshot exceeds its size bound");
}
