// Modified by RenCrow Switch Core, 2026-11-27: verify durable Common context at runtime boundaries.
use super::CommonTaskFixtureVariant;
use super::common_task_in_store_variant;
use codex_history::task_context::TaskRunStatusV1;
use codex_protocol::models::ResponseItem;
use codex_utils_string::approx_bytes_for_tokens;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn oversized_prompt_context_durably_blocks_append_without_truncating_contract() {
    let (_workspace, session, turn) = common_task_in_store_variant(
        true,
        false,
        CommonTaskFixtureVariant::OversizedPromptContext,
    )
    .await;
    let before = session
        .current_common_harness_state("harness-root")
        .await
        .expect("ordinary typed Begin admits the oversized-prompt fixture");
    assert_eq!(before.run_state.status, TaskRunStatusV1::Queued);
    assert_eq!(before.run_state.attempt_no, 0);

    // Exercise the production attempt/deadline boundary and exact prompt appender.
    session
        .common_harness_inference_deadline(&turn)
        .await
        .expect("the single bounded attempt starts before prompt construction");
    let mut input = Vec::<ResponseItem>::new();
    let append_error = session
        .append_common_harness_prompt_context(&turn, &mut input)
        .await
        .expect_err("oversized complete host projection must stop before model input");
    assert!(
        append_error.contains("prompt_context_too_large"),
        "{append_error}"
    );
    assert!(
        input.is_empty(),
        "no truncated Common fragment reaches the model"
    );

    let blocked = session
        .current_common_harness_state("harness-root")
        .await
        .expect("the appender durably records a host Block transition");
    assert_eq!(blocked.run_state.status, TaskRunStatusV1::Blocked);
    assert_eq!(
        blocked.run_state.reason_code.as_deref(),
        Some("prompt_context_too_large")
    );
    assert_eq!(blocked.run_state.attempt_no, 1);
    assert_eq!(blocked.request.goal, before.request.goal);
    assert!(
        blocked.request.goal.len() <= 4096,
        "goal respects its field bound"
    );
    assert_eq!(blocked.request.constraints.len(), 9);
    assert!(
        blocked
            .request
            .constraints
            .iter()
            .all(|constraint| constraint.len() <= 4096),
        "each constraint respects its field bound"
    );
    assert!(
        blocked
            .request
            .constraints
            .iter()
            .map(String::len)
            .sum::<usize>()
            > approx_bytes_for_tokens(8_000),
        "the complete multi-field host projection exceeds the appender budget"
    );
    assert_eq!(blocked.request.constraints, before.request.constraints);
    assert_eq!(blocked.request.conditions, before.request.conditions);
    assert_eq!(
        serde_json::to_value(
            &blocked.request.conditions[1]
                .check
                .as_ref()
                .expect("complete registered Go check")
                .argv,
        )
        .unwrap(),
        serde_json::to_value(
            &before.request.conditions[1]
                .check
                .as_ref()
                .expect("complete registered Go check")
                .argv,
        )
        .unwrap(),
        "oversize handling leaves the verifier contract intact"
    );

    // Reentering the existing deadline/appender path cannot consume another
    // attempt, clear the block, or append a partial substitute projection.
    assert!(
        session
            .common_harness_inference_deadline(&turn)
            .await
            .is_err()
    );
    let retry_error = session
        .append_common_harness_prompt_context(&turn, &mut input)
        .await
        .expect_err("blocked oversized tasks do not retry model inference");
    assert!(
        retry_error.contains("prompt_context_too_large"),
        "{retry_error}"
    );
    assert!(input.is_empty());
    let after_retry = session
        .current_common_harness_state("harness-root")
        .await
        .expect("blocked state remains canonical");
    assert_eq!(
        after_retry, blocked,
        "rechecking the stopped task must not append a transition or consume an attempt"
    );
}
