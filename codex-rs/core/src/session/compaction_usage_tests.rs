use super::*;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TokenUsageInfo;
use tokio::sync::Notify;

fn usage(input_tokens: i64, output_tokens: i64, total_tokens: i64) -> TokenUsage {
    TokenUsage {
        input_tokens,
        cached_input_tokens: 0,
        cache_write_input_tokens: 0,
        output_tokens,
        reasoning_output_tokens: 0,
        total_tokens,
        codex_rollout_budget_units: None,
    }
}

#[tokio::test]
async fn compaction_aux_usage_keeps_active_context_safe_during_callback_wait() {
    struct BlockingContributor {
        started: Arc<Notify>,
        observed: Arc<std::sync::Mutex<Option<TokenUsageInfo>>>,
    }

    impl codex_extension_api::TokenUsageContributor for BlockingContributor {
        fn on_token_usage<'a>(
            &'a self,
            _session_store: &'a codex_extension_api::ExtensionData,
            _thread_store: &'a codex_extension_api::ExtensionData,
            _turn_store: &'a codex_extension_api::ExtensionData,
            token_usage: &'a TokenUsageInfo,
        ) -> codex_extension_api::ExtensionFuture<'a, ()> {
            Box::pin(async move {
                *self.observed.lock().expect("observed usage lock") = Some(token_usage.clone());
                self.started.notify_one();
                std::future::pending::<()>().await;
            })
        }
    }

    let (mut session, turn_context) = make_session_and_context().await;
    session
        .record_conversation_items(
            &turn_context,
            turn_context.model_info(),
            &[user_message(
                "current conversation that must remain the active estimate",
            )],
        )
        .await;

    let normal_usage = usage(13, 5, 18);
    session
        .record_token_usage_info(
            &turn_context,
            &turn_context.initial_settings,
            Some(&normal_usage),
        )
        .await
        .expect("normal usage should be recorded");
    {
        let mut state = session.state.lock().await;
        state.start_new_context_window();
        state.ensure_auto_compact_window_server_prefill_from_usage(&usage(29, 0, 29));
    }
    let guard = session.capture_compaction_usage_guard(&turn_context).await;
    let history = session.clone_history().await;
    let base_instructions = session.get_base_instructions().await;
    let expected_active_tokens = history
        .estimate_token_count_with_base_instructions(&base_instructions)
        .expect("current history estimate")
        .max(0);

    let started = Arc::new(Notify::new());
    let observed = Arc::new(std::sync::Mutex::new(None));
    let mut extensions =
        codex_extension_api::ExtensionRegistryBuilder::<crate::config::Config>::new();
    extensions.token_usage_contributor(Arc::new(BlockingContributor {
        started: Arc::clone(&started),
        observed: Arc::clone(&observed),
    }));
    session.services.extensions = Arc::new(extensions.build());

    let session = Arc::new(session);
    let turn_context = Arc::new(turn_context);
    let auxiliary_usage = usage(901, 37, 938);
    let auxiliary_usage_for_task = auxiliary_usage.clone();
    let task_session = Arc::clone(&session);
    let task_turn_context = Arc::clone(&turn_context);
    let task = tokio::spawn(async move {
        task_session
            .update_compaction_aux_token_usage_info(
                &task_turn_context,
                &task_turn_context.initial_settings,
                Some(&auxiliary_usage_for_task),
                &guard,
            )
            .await
    });

    tokio::time::timeout(std::time::Duration::from_secs(2), started.notified())
        .await
        .expect("usage callback is reached");

    let (active_info, prefill) = {
        let state = session.state.lock().await;
        (
            state.token_info().expect("active token info"),
            state.auto_compact_window_snapshot(),
        )
    };
    let mut expected_billing_total = normal_usage.clone();
    expected_billing_total.add_assign(&auxiliary_usage);
    std::assert_eq!(active_info.total_token_usage, expected_billing_total);
    std::assert_eq!(
        active_info.last_token_usage.total_tokens,
        expected_active_tokens
    );
    std::assert_eq!(prefill.prefill_input_tokens, Some(29));

    let callback_info = observed
        .lock()
        .expect("observed usage lock")
        .clone()
        .expect("callback received actual usage");
    std::assert_eq!(callback_info.total_token_usage, expected_billing_total);
    std::assert_eq!(callback_info.last_token_usage, auxiliary_usage);

    task.abort();
    assert!(
        task.await
            .expect_err("blocked callback was aborted")
            .is_cancelled()
    );
    let after_abort = session
        .token_usage_info()
        .await
        .expect("safe token info remains available");
    std::assert_eq!(
        after_abort.last_token_usage.total_tokens,
        expected_active_tokens
    );
}

#[tokio::test]
async fn stale_compaction_usage_guard_preserves_latest_active_state() {
    #[derive(Clone, Copy)]
    enum GuardChange {
        NormalCompletionWithoutUsage,
        NormalCompletionWithUsage,
        Window,
        Turn,
        Settings,
        ActiveUsage,
        RequestTurnMismatch,
        RequestSettingsMismatch,
    }

    for change in [
        GuardChange::NormalCompletionWithoutUsage,
        GuardChange::NormalCompletionWithUsage,
        GuardChange::Window,
        GuardChange::Turn,
        GuardChange::Settings,
        GuardChange::ActiveUsage,
        GuardChange::RequestTurnMismatch,
        GuardChange::RequestSettingsMismatch,
    ] {
        let (session, turn_context) = make_session_and_context().await;
        session
            .record_conversation_items(
                &turn_context,
                turn_context.model_info(),
                &[user_message("conversation state")],
            )
            .await;
        let baseline = usage(21, 8, 29);
        session
            .record_token_usage_info(
                &turn_context,
                &turn_context.initial_settings,
                Some(&baseline),
            )
            .await
            .expect("baseline usage should be recorded");
        {
            let mut state = session.state.lock().await;
            state.start_new_context_window();
            state.set_auto_compact_window_estimated_prefill(31);
        }
        if matches!(
            change,
            GuardChange::RequestTurnMismatch | GuardChange::RequestSettingsMismatch
        ) {
            let mut state = session.state.lock().await;
            if matches!(change, GuardChange::RequestTurnMismatch) {
                state.last_started_turn_id = Some("different-request-turn".into());
            } else {
                state.session_configuration.step_settings =
                    Arc::new(state.session_configuration.step_settings.as_ref().clone());
            }
        }
        let guard = session.capture_compaction_usage_guard(&turn_context).await;

        match change {
            GuardChange::NormalCompletionWithoutUsage => {
                session
                    .record_token_usage_info(&turn_context, &turn_context.initial_settings, None)
                    .await
                    .expect("usage-free normal completion should succeed");
            }
            GuardChange::NormalCompletionWithUsage => {
                session
                    .record_token_usage_info(
                        &turn_context,
                        &turn_context.initial_settings,
                        Some(&usage(34, 9, 43)),
                    )
                    .await
                    .expect("normal completion usage should succeed");
            }
            _ => {
                let mut state = session.state.lock().await;
                match change {
                    GuardChange::Window => {
                        state.start_new_context_window();
                        state.set_auto_compact_window_estimated_prefill(44);
                    }
                    GuardChange::Turn => state.last_started_turn_id = Some("new-turn".into()),
                    GuardChange::Settings => {
                        state.session_configuration.step_settings =
                            Arc::new(state.session_configuration.step_settings.as_ref().clone());
                    }
                    GuardChange::ActiveUsage => {
                        let mut latest = state.token_info().expect("baseline active usage");
                        latest.last_token_usage = usage(55, 6, 61);
                        latest.model_context_window = Some(777);
                        state.set_token_info(Some(latest));
                    }
                    GuardChange::RequestTurnMismatch | GuardChange::RequestSettingsMismatch => {}
                    GuardChange::NormalCompletionWithoutUsage
                    | GuardChange::NormalCompletionWithUsage => unreachable!(),
                }
            }
        }

        let before_aux = session
            .token_usage_info()
            .await
            .expect("active token info before auxiliary completion");
        let before_prefill = session.state.lock().await.auto_compact_window_snapshot();
        let auxiliary_usage = usage(800, 42, 842);
        session
            .update_compaction_aux_token_usage_info(
                &turn_context,
                &turn_context.initial_settings,
                Some(&auxiliary_usage),
                &guard,
            )
            .await
            .expect("auxiliary usage accounting should succeed");

        let after_aux = session
            .token_usage_info()
            .await
            .expect("active token info after auxiliary completion");
        let mut expected_total = before_aux.total_token_usage.clone();
        expected_total.add_assign(&auxiliary_usage);
        std::assert_eq!(after_aux.total_token_usage, expected_total);
        std::assert_eq!(after_aux.last_token_usage, before_aux.last_token_usage);
        std::assert_eq!(
            after_aux.model_context_window,
            before_aux.model_context_window
        );
        std::assert_eq!(
            session.state.lock().await.auto_compact_window_snapshot(),
            before_prefill
        );
    }
}

#[tokio::test]
async fn compaction_aux_usage_preserves_none_estimated_and_server_prefill() {
    #[derive(Clone, Copy)]
    enum Prefill {
        None,
        Estimated,
        ServerObserved,
    }

    for (kind, expected) in [
        (Prefill::None, None),
        (Prefill::Estimated, Some(17)),
        (Prefill::ServerObserved, Some(23)),
    ] {
        let (session, turn_context) = make_session_and_context().await;
        session
            .record_conversation_items(
                &turn_context,
                turn_context.model_info(),
                &[user_message("current context")],
            )
            .await;
        {
            let mut state = session.state.lock().await;
            match kind {
                Prefill::None => {}
                Prefill::Estimated => state.set_auto_compact_window_estimated_prefill(17),
                Prefill::ServerObserved => {
                    state.ensure_auto_compact_window_server_prefill_from_usage(&usage(23, 0, 23));
                }
            }
        }
        let guard = session.capture_compaction_usage_guard(&turn_context).await;
        session
            .update_compaction_aux_token_usage_info(
                &turn_context,
                &turn_context.initial_settings,
                Some(&usage(901, 3, 904)),
                &guard,
            )
            .await
            .expect("auxiliary usage accounting should succeed");
        std::assert_eq!(
            session
                .state
                .lock()
                .await
                .auto_compact_window_snapshot()
                .prefill_input_tokens,
            expected
        );
    }
}

#[tokio::test]
async fn compaction_budget_error_emits_safe_token_count_before_return() {
    let (mut session, turn_context, events) = make_session_and_context_with_rx().await;
    Arc::get_mut(&mut session)
        .expect("test session is uniquely owned")
        .services
        .agent_control = crate::agent::LocalAgentControl::new(
        std::sync::Weak::default(),
        crate::thread_manager::default_thread_id_generator(),
        Some(crate::config::RolloutBudgetConfig {
            limit_tokens: 1,
            reminder_at_remaining_tokens: Vec::new(),
            sampling_token_weight: 1.0,
            prefill_token_weight: 1.0,
        }),
    );
    session
        .record_conversation_items(
            &turn_context,
            turn_context.model_info(),
            &[user_message("current conversation")],
        )
        .await;
    let expected_active_tokens = session
        .clone_history()
        .await
        .estimate_token_count_with_base_instructions(&session.get_base_instructions().await)
        .expect("current history estimate")
        .max(0);
    let guard = session.capture_compaction_usage_guard(&turn_context).await;
    let auxiliary_usage = usage(901, 39, 940);

    let error = session
        .update_compaction_aux_token_usage_info(
            &turn_context,
            &turn_context.initial_settings,
            Some(&auxiliary_usage),
            &guard,
        )
        .await
        .expect_err("the configured rollout budget is exceeded");
    assert!(matches!(
        error.details(),
        codex_protocol::error::CodexErrorDetails::SessionBudgetExceeded
    ));

    let count_event = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let event = events
                .recv()
                .await
                .expect("session event channel remains open");
            if let EventMsg::TokenCount(token_count) = event.msg {
                break token_count;
            }
        }
    })
    .await
    .expect("safe TokenCount is emitted before the budget error returns");
    let safe_info = count_event.info.expect("safe token count info");
    std::assert_eq!(
        safe_info.last_token_usage.total_tokens,
        expected_active_tokens
    );
    std::assert_eq!(safe_info.total_token_usage, auxiliary_usage);
}
