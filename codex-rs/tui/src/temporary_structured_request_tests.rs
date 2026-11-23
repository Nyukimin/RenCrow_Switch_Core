use super::STRUCTURED_RESPONSE_MAX_BYTES;
use super::TemporaryStructuredThreadOptions;
use super::TemporaryStructuredTurnRequest;
use super::collect_structured_response;
use super::run_temporary_structured_turn_with_timeout;
use super::start_structured_turn;
use super::start_temporary_thread;
use crate::legacy_core::config::ConfigBuilder;
use crate::test_support::PathBufExt;
use codex_app_server_client::AppServerClient;
use codex_app_server_client::AppServerEvent;
use codex_app_server_client::RemoteAppServerClient;
use codex_app_server_client::RemoteAppServerConnectArgs;
use codex_app_server_client::RemoteAppServerEndpoint;
use codex_app_server_protocol::AskForApproval;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ItemCompletedNotification;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::JSONRPCNotification;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::SandboxPolicy;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadSource;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::ThreadUnsubscribeResponse;
use codex_app_server_protocol::ThreadUnsubscribeStatus;
use codex_app_server_protocol::Turn;
use codex_app_server_protocol::TurnCompletedNotification;
use codex_app_server_protocol::TurnInterruptResponse;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserInput;
use codex_config::LoaderOverrides;
use codex_exec_server::EnvironmentManager;
use core_test_support::responses;
use futures::SinkExt;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tempfile::tempdir;
use tokio::net::TcpListener;
use tokio::sync::mpsc::unbounded_channel;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

async fn wait_for_turn_completion(
    app_server: &mut AppServerClient,
    turn_id: &str,
) -> color_eyre::Result<()> {
    let mut received_assistant_message = false;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match app_server.next_event().await {
                Some(AppServerEvent::ServerNotification(notification)) => {
                    match notification.as_ref() {
                        ServerNotification::ItemCompleted(completed)
                            if completed.turn_id == turn_id
                                && matches!(&completed.item, ThreadItem::AgentMessage { .. }) =>
                        {
                            received_assistant_message = true;
                        }
                        ServerNotification::TurnCompleted(completed)
                            if completed.turn.id == turn_id =>
                        {
                            if completed.turn.status != TurnStatus::Completed {
                                return Err(color_eyre::eyre::eyre!(
                                    "turn {turn_id} completed with status {:?}",
                                    completed.turn.status
                                ));
                            }
                            if !received_assistant_message {
                                return Err(color_eyre::eyre::eyre!(
                                    "turn {turn_id} completed without an assistant response"
                                ));
                            }
                            return Ok(());
                        }
                        _ => {}
                    }
                }
                Some(_) => {}
                None => return Err(color_eyre::eyre::eyre!("app-server event stream closed")),
            }
        }
    })
    .await?
}

fn has_model_tool(tools: &[serde_json::Value], name: &str) -> bool {
    tools.iter().any(|tool| {
        tool["name"] == name
            || tool["tools"]
                .as_array()
                .is_some_and(|nested| nested.iter().any(|nested_tool| nested_tool["name"] == name))
    })
}

fn agent_message_notification(turn_id: &str, text: &str) -> ServerNotification {
    ServerNotification::ItemCompleted(ItemCompletedNotification {
        item: ThreadItem::AgentMessage {
            id: "message-1".to_string(),
            text: text.to_string(),
            phase: None,
            memory_citation: None,
            delivery: None,
            questions: None,
        },
        thread_id: "thread-1".to_string(),
        turn_id: turn_id.to_string(),
        completed_at_ms: 0,
    })
}

fn turn_completed_notification(turn_id: &str, status: TurnStatus) -> ServerNotification {
    ServerNotification::TurnCompleted(TurnCompletedNotification {
        thread_id: "thread-1".to_string(),
        turn: Turn {
            id: turn_id.to_string(),
            items: Vec::new(),
            items_view: Default::default(),
            status,
            error: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
        },
    })
}

#[derive(Clone, Copy)]
enum TurnLifecycleMockMode {
    Complete,
    HoldUntilInterrupt,
}

async fn start_turn_lifecycle_mock(
    mode: TurnLifecycleMockMode,
) -> color_eyre::Result<(
    AppServerClient,
    std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    tokio::sync::oneshot::Receiver<()>,
    tokio::task::JoinHandle<()>,
)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("ws://{}", listener.local_addr()?);
    let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let request_log = std::sync::Arc::clone(&requests);
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("client connects");
        let mut websocket = accept_async(stream).await.expect("websocket accepts");
        let mut started_tx = Some(started_tx);
        while let Some(Ok(Message::Text(text))) = websocket.next().await {
            let message: JSONRPCMessage = serde_json::from_str(&text).expect("valid request");
            let JSONRPCMessage::Request(request) = message else {
                continue;
            };
            match request.method.as_str() {
                "initialize" => {
                    let result = serde_json::json!({
                        "userAgent": "codex-tui-test",
                        "codexHome": "/tmp/codex-tui-test"
                    });
                    websocket
                        .send(Message::Text(
                            serde_json::to_string(&JSONRPCMessage::Response(JSONRPCResponse {
                                id: request.id,
                                result,
                            }))
                            .expect("serialize initialize response")
                            .into(),
                        ))
                        .await
                        .expect("send initialize response");
                }
                "turn/start" => {
                    request_log.lock().expect("request log lock").push(
                        serde_json::json!({"method": request.method, "params": request.params}),
                    );
                    let response = TurnStartResponse {
                        turn: Turn {
                            id: "mock-turn-1".to_string(),
                            items: Vec::new(),
                            items_view: Default::default(),
                            status: TurnStatus::InProgress,
                            error: None,
                            started_at: None,
                            completed_at: None,
                            duration_ms: None,
                        },
                    };
                    websocket
                        .send(Message::Text(
                            serde_json::to_string(&JSONRPCMessage::Response(JSONRPCResponse {
                                id: request.id,
                                result: serde_json::to_value(response)
                                    .expect("serialize start response"),
                            }))
                            .expect("serialize turn/start response")
                            .into(),
                        ))
                        .await
                        .expect("send turn/start response");
                    if let Some(started_tx) = started_tx.take() {
                        let _ = started_tx.send(());
                    }
                    if matches!(mode, TurnLifecycleMockMode::Complete) {
                        for notification in [
                            JSONRPCNotification {
                                method: "item/completed".to_string(),
                                params: Some(
                                    serde_json::to_value(ItemCompletedNotification {
                                        item: ThreadItem::AgentMessage {
                                            id: "mock-message-1".to_string(),
                                            text: r#"{"title":"ok"}"#.to_string(),
                                            phase: None,
                                            memory_citation: None,
                                            delivery: None,
                                            questions: None,
                                        },
                                        thread_id: "mock-thread-1".to_string(),
                                        turn_id: "mock-turn-1".to_string(),
                                        completed_at_ms: 1,
                                    })
                                    .expect("serialize item notification"),
                                ),
                            },
                            JSONRPCNotification {
                                method: "turn/completed".to_string(),
                                params: Some(
                                    serde_json::to_value(TurnCompletedNotification {
                                        thread_id: "mock-thread-1".to_string(),
                                        turn: Turn {
                                            id: "mock-turn-1".to_string(),
                                            items: Vec::new(),
                                            items_view: Default::default(),
                                            status: TurnStatus::Completed,
                                            error: None,
                                            started_at: None,
                                            completed_at: None,
                                            duration_ms: None,
                                        },
                                    })
                                    .expect("serialize turn notification"),
                                ),
                            },
                        ] {
                            websocket
                                .send(Message::Text(
                                    serde_json::to_string(&JSONRPCMessage::Notification(
                                        notification,
                                    ))
                                    .expect("serialize notification")
                                    .into(),
                                ))
                                .await
                                .expect("send notification");
                        }
                    }
                }
                "turn/interrupt" => {
                    request_log.lock().expect("request log lock").push(
                        serde_json::json!({"method": request.method, "params": request.params}),
                    );
                    websocket
                        .send(Message::Text(
                            serde_json::to_string(&JSONRPCMessage::Response(JSONRPCResponse {
                                id: request.id,
                                result: serde_json::to_value(TurnInterruptResponse {})
                                    .expect("serialize interrupt response"),
                            }))
                            .expect("serialize interrupt response")
                            .into(),
                        ))
                        .await
                        .expect("send interrupt response");
                    let interrupted = JSONRPCNotification {
                        method: "turn/completed".to_string(),
                        params: Some(
                            serde_json::to_value(TurnCompletedNotification {
                                thread_id: "mock-thread-1".to_string(),
                                turn: Turn {
                                    id: "mock-turn-1".to_string(),
                                    items: Vec::new(),
                                    items_view: Default::default(),
                                    status: TurnStatus::Interrupted,
                                    error: None,
                                    started_at: None,
                                    completed_at: None,
                                    duration_ms: None,
                                },
                            })
                            .expect("serialize interrupted turn"),
                        ),
                    };
                    websocket
                        .send(Message::Text(
                            serde_json::to_string(&JSONRPCMessage::Notification(interrupted))
                                .expect("serialize interrupted notification")
                                .into(),
                        ))
                        .await
                        .expect("send interrupted notification");
                }
                "thread/unsubscribe" => {
                    request_log.lock().expect("request log lock").push(
                        serde_json::json!({"method": request.method, "params": request.params}),
                    );
                    websocket
                        .send(Message::Text(
                            serde_json::to_string(&JSONRPCMessage::Response(JSONRPCResponse {
                                id: request.id,
                                result: serde_json::to_value(ThreadUnsubscribeResponse {
                                    status: ThreadUnsubscribeStatus::Unsubscribed,
                                })
                                .expect("serialize unsubscribe response"),
                            }))
                            .expect("serialize unsubscribe response")
                            .into(),
                        ))
                        .await
                        .expect("send unsubscribe response");
                    break;
                }
                _ => {}
            }
        }
    });
    let client = RemoteAppServerClient::connect(RemoteAppServerConnectArgs {
        endpoint: RemoteAppServerEndpoint::WebSocket {
            websocket_url: url,
            auth_token: None,
        },
        client_name: "codex-tui-test".to_string(),
        client_version: "0.0.0-test".to_string(),
        experimental_api: true,
        mcp_server_openai_form_elicitation: false,
        opt_out_notification_methods: Vec::new(),
        channel_capacity: 16,
    })
    .await?;
    Ok((
        AppServerClient::Remote(client),
        requests,
        started_rx,
        server,
    ))
}

#[tokio::test]
async fn managed_workspace_default_respects_read_only_availability() -> color_eyre::Result<()> {
    for read_only_allowed in [true, false] {
        let codex_home = tempdir()?;
        let requirements_path = codex_home.path().join("requirements.toml");
        std::fs::write(
            &requirements_path,
            format!(
                "default_permissions = \":workspace\"\n\n\
                 [allowed_permission_profiles]\n\
                 \":read-only\" = {read_only_allowed}\n\
                 \":workspace\" = true\n"
            ),
        )?;
        let loader_overrides = LoaderOverrides {
            system_requirements_path: Some(requirements_path),
            ignore_project_config: true,
            ..LoaderOverrides::without_managed_config_for_tests()
        };
        let config = ConfigBuilder::default()
            .codex_home(codex_home.path().to_path_buf())
            .fallback_cwd(Some(codex_home.path().to_path_buf()))
            .loader_overrides(loader_overrides.clone())
            .build()
            .await?;
        let options = TemporaryStructuredThreadOptions {
            thread_source: ThreadSource::Feature("thread_title".to_string()),
            model: "gpt-5.2".to_string(),
            model_provider: config.model_provider_id.clone(),
            cwd: config.cwd.to_string_lossy().into_owned(),
            active_permission_profile: config
                .permissions
                .active_permission_profile()
                .map(|profile| profile.id),
            mcp_server_names: Vec::new(),
        };
        let client = crate::start_embedded_app_server(
            Default::default(),
            config,
            Vec::new(),
            loader_overrides,
            /*strict_config*/ false,
            Default::default(),
            codex_feedback::CodexFeedback::new(),
            /*log_db*/ None,
            /*state_db*/ None,
            Arc::new(EnvironmentManager::default_for_tests()),
        )
        .await?;
        let app_server = AppServerClient::InProcess(client);
        let result = start_temporary_thread(&app_server.request_handle(), options).await;
        app_server.shutdown().await?;

        if read_only_allowed {
            let response = result?;
            assert_eq!(
                (
                    response.active_permission_profile.map(|profile| profile.id),
                    response.approval_policy,
                    response.sandbox,
                ),
                (
                    Some(":read-only".to_string()),
                    AskForApproval::Never,
                    SandboxPolicy::ReadOnly {
                        network_access: false,
                    },
                ),
            );
        } else {
            assert_eq!(
                result.expect_err("read-only must be allowed").to_string(),
                "temporary structured thread did not start with read-only permissions",
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn preserves_custom_permissions_and_disables_required_mcp_servers() -> color_eyre::Result<()>
{
    let (chat_widget, _, _, _) =
        crate::chatwidget::tests::make_chatwidget_manual_with_sender().await;
    let mut config = chat_widget.config_ref().clone();
    let codex_home = tempdir()?;
    let denied_path = codex_home.path().join("denied");
    let denied_key = toml::Value::String(denied_path.display().to_string());
    std::fs::write(
        codex_home.path().join("config.toml"),
        format!(
            "default_permissions = \"title-restricted\"\n\n\
             [permissions.title-restricted.filesystem]\n\
             \":root\" = \"read\"\n\
             {denied_key} = \"deny\"\n\n\
             [mcp_servers.forbidden]\n\
             command = \"codex-auto-title-missing-mcp\"\n\
             required = true\n"
        ),
    )?;
    config.codex_home = codex_home.path().to_path_buf().abs();
    config.sqlite =
        codex_state::SqliteConfig::new_for_testing(codex_home.path().to_path_buf().abs());

    let app_server = crate::start_embedded_app_server_for_picker(&config).await?;
    let response = start_temporary_thread(
        &app_server.request_handle(),
        TemporaryStructuredThreadOptions {
            thread_source: ThreadSource::Feature("thread_title".to_string()),
            model: "gpt-5.2".to_string(),
            model_provider: config.model_provider_id.clone(),
            cwd: config.cwd.display().to_string(),
            active_permission_profile: Some("title-restricted".to_string()),
            mcp_server_names: vec!["forbidden".to_string()],
        },
    )
    .await?;

    assert_eq!(
        (
            response.active_permission_profile.map(|profile| profile.id),
            response.model_provider,
            response.thread.ephemeral,
            response.thread.thread_source,
        ),
        (
            Some("title-restricted".to_string()),
            config.model_provider_id,
            true,
            Some(ThreadSource::Feature("thread_title".to_string())),
        )
    );

    app_server.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn temporary_structured_thread_omits_task_boundary_while_normal_thread_keeps_it()
-> color_eyre::Result<()> {
    let mock_server = responses::start_mock_server().await;
    let base_url = mock_server.uri();
    let response_log = responses::mount_sse_sequence(
        &mock_server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("normal-response"),
                responses::ev_assistant_message("normal-message", "normal reply"),
                responses::ev_completed("normal-response"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("structured-response"),
                responses::ev_assistant_message(
                    "structured-message",
                    r#"{"title":"temporary title"}"#,
                ),
                responses::ev_completed("structured-response"),
            ]),
        ],
    )
    .await;
    let codex_home = tempdir()?;
    std::fs::write(
        codex_home.path().join("config.toml"),
        format!(
            r#"
model = "gpt-5.5"
model_provider = "fix006-mock"
rencrow_compaction = true
rencrow_autonomous_completion = true

[features]
send_message_to_user_async = true

[features.sleep_tool]
mode = "always_on"

[model_providers.fix006-mock]
name = "FIX-006 mock"
base_url = "{base_url}/v1"
wire_api = "responses"
request_max_retries = 0
stream_max_retries = 0
"#
        ),
    )?;
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await?;
    assert!(config.rencrow_compaction);
    assert!(config.rencrow_autonomous_completion);
    // The bundled gpt-5.5 catalog entry has no experimental-supported tools,
    // matching the current Qwen route's empty capability list.

    let cli_config_overrides = vec![
        ("rencrow_compaction".to_string(), true.into()),
        ("rencrow_autonomous_completion".to_string(), true.into()),
    ];
    let app_server_client = crate::start_embedded_app_server(
        Default::default(),
        config.clone(),
        cli_config_overrides,
        LoaderOverrides::without_managed_config_for_tests(),
        /*strict_config*/ false,
        Default::default(),
        codex_feedback::CodexFeedback::new(),
        /*log_db*/ None,
        /*state_db*/ None,
        Arc::new(EnvironmentManager::default_for_tests()),
    )
    .await?;
    let mut app_server = AppServerClient::InProcess(app_server_client);
    let request_handle = app_server.request_handle();
    let normal_thread: ThreadStartResponse = request_handle
        .request_typed(ClientRequest::ThreadStart {
            request_id: RequestId::String("fix006-normal-thread".to_string()),
            params: ThreadStartParams {
                model: Some("gpt-5.5".to_string()),
                model_provider: Some("fix006-mock".to_string()),
                thread_source: Some(ThreadSource::User),
                ..ThreadStartParams::default()
            },
        })
        .await?;
    let normal_turn: TurnStartResponse = request_handle
        .request_typed(ClientRequest::TurnStart {
            request_id: RequestId::String("fix006-normal-turn".to_string()),
            params: TurnStartParams {
                thread_id: normal_thread.thread.id,
                input: vec![UserInput::Text {
                    text: "Return a short reply.".to_string(),
                    text_elements: Vec::new(),
                }],
                ..TurnStartParams::default()
            },
        })
        .await?;
    wait_for_turn_completion(&mut app_server, &normal_turn.turn.id).await?;

    let structured_schema = json!({
        "type": "object",
        "properties": {"title": {"type": "string"}},
        "required": ["title"],
        "additionalProperties": false
    });
    let structured_thread = start_temporary_thread(
        &request_handle,
        TemporaryStructuredThreadOptions {
            thread_source: ThreadSource::Feature("thread_title".to_string()),
            model: "gpt-5.5".to_string(),
            model_provider: "fix006-mock".to_string(),
            cwd: config.cwd.display().to_string(),
            active_permission_profile: config
                .permissions
                .active_permission_profile()
                .map(|profile| profile.id),
            mcp_server_names: Vec::new(),
        },
    )
    .await?;
    let structured_turn = start_structured_turn(
        &request_handle,
        structured_thread.thread.id,
        "Return a title as JSON.".to_string(),
        structured_schema.clone(),
        None,
    )
    .await?;
    wait_for_turn_completion(&mut app_server, &structured_turn.turn.id).await?;

    let requests = response_log.requests();
    assert_eq!(requests.len(), 2);
    let normal_body = requests[0].body_json();
    let normal_tools = normal_body["tools"].as_array().expect("normal tools array");
    assert!(
        has_model_tool(normal_tools, "task_context_boundary"),
        "normal turn should keep task_context_boundary"
    );
    for expected_tool in ["send_message_to_user_async", "sleep"] {
        assert!(
            has_model_tool(normal_tools, expected_tool),
            "normal turn should keep `{expected_tool}`"
        );
    }

    let structured_body = requests[1].body_json();
    assert_eq!(structured_body["text"]["format"]["type"], "json_schema");
    assert_eq!(
        structured_body["text"]["format"]["schema"],
        structured_schema
    );
    let structured_tools = structured_body["tools"]
        .as_array()
        .expect("structured tools array");
    assert!(
        structured_tools.is_empty(),
        "temporary structured turn must have no model-visible tools: {structured_tools:?}"
    );
    app_server.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn temporary_turn_timeout_and_cancellation_interrupt_before_unsubscribe()
-> color_eyre::Result<()> {
    for (mode, should_cancel, timeout) in [
        (
            TurnLifecycleMockMode::Complete,
            false,
            Duration::from_secs(2),
        ),
        (
            TurnLifecycleMockMode::HoldUntilInterrupt,
            true,
            Duration::from_secs(2),
        ),
        (
            TurnLifecycleMockMode::HoldUntilInterrupt,
            false,
            Duration::from_secs(1),
        ),
    ] {
        let (mut client, requests, started, server) = start_turn_lifecycle_mock(mode).await?;
        let request_handle = client.request_handle();
        let (notification_tx, notification_rx) = unbounded_channel();
        let event_forwarder = tokio::spawn(async move {
            let mut forwarded = Vec::new();
            while let Some(event) = client.next_event().await {
                if let AppServerEvent::ServerNotification(notification) = event {
                    forwarded.push((*notification).clone());
                    if notification_tx.send(*notification).is_err() {
                        break;
                    }
                }
            }
            let _ = client.shutdown().await;
            forwarded
        });
        let cancellation = CancellationToken::new();
        let run_cancellation = cancellation.clone();
        let run = tokio::spawn(run_temporary_structured_turn_with_timeout(
            TemporaryStructuredTurnRequest {
                request_handle,
                thread_id: "mock-thread-1".to_string(),
                prompt: "return json".to_string(),
                output_schema: json!({"type": "object"}),
                effort: None,
                notifications: notification_rx,
                cancellation: run_cancellation,
            },
            timeout,
        ));

        if matches!(mode, TurnLifecycleMockMode::HoldUntilInterrupt) {
            tokio::time::timeout(Duration::from_secs(2), started).await??;
            if should_cancel {
                cancellation.cancel();
            }
            let error = run.await?.expect_err("held turn should time out or cancel");
            assert_eq!(
                error.to_string(),
                if should_cancel {
                    "temporary structured turn cancelled"
                } else {
                    "temporary structured turn timed out"
                }
            );
        } else {
            assert_eq!(run.await??, r#"{"title":"ok"}"#);
        }

        let forwarded = event_forwarder.await?;
        server.await?;
        let requests = requests.lock().expect("request log lock").clone();
        let methods = requests
            .iter()
            .filter_map(|request| request["method"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(methods.first(), Some(&"turn/start"));
        assert_eq!(methods.last(), Some(&"thread/unsubscribe"));
        if matches!(mode, TurnLifecycleMockMode::HoldUntilInterrupt) {
            assert_eq!(
                methods,
                ["turn/start", "turn/interrupt", "thread/unsubscribe"]
            );
            assert_eq!(requests[1]["params"]["turnId"], "mock-turn-1");
            assert!(forwarded.iter().any(|notification| matches!(
                notification,
                ServerNotification::TurnCompleted(completed)
                    if completed.turn.id == "mock-turn-1"
                        && completed.turn.status == TurnStatus::Interrupted
            )));
        } else {
            assert_eq!(methods, ["turn/start", "thread/unsubscribe"]);
        }
    }
    Ok(())
}

#[tokio::test]
async fn returns_latest_matching_assistant_message() {
    let (sender, receiver) = unbounded_channel();
    sender
        .send(agent_message_notification("other-turn", "ignore me"))
        .expect("send unrelated assistant message");
    sender
        .send(turn_completed_notification(
            "other-turn",
            TurnStatus::Completed,
        ))
        .expect("send unrelated completion");
    sender
        .send(agent_message_notification("turn-1", "first"))
        .expect("send first assistant message");
    sender
        .send(agent_message_notification("turn-1", "final"))
        .expect("send final assistant message");
    sender
        .send(turn_completed_notification("turn-1", TurnStatus::Completed))
        .expect("send matching completion");

    let response = collect_structured_response(receiver, "turn-1")
        .await
        .expect("collect structured response");

    assert_eq!(response, "final");
}

#[tokio::test]
async fn rejects_failed_turn() {
    let (sender, receiver) = unbounded_channel();
    sender
        .send(turn_completed_notification("turn-1", TurnStatus::Failed))
        .expect("send failed completion");

    let error = collect_structured_response(receiver, "turn-1")
        .await
        .expect_err("failed turn should not produce a response");

    assert_eq!(
        error.to_string(),
        "temporary structured turn ended with status Failed"
    );
}

#[tokio::test]
async fn rejects_completion_without_assistant_message() {
    let (sender, receiver) = unbounded_channel();
    sender
        .send(turn_completed_notification("turn-1", TurnStatus::Completed))
        .expect("send completion");

    let error = collect_structured_response(receiver, "turn-1")
        .await
        .expect_err("completion without assistant message should fail");

    assert_eq!(
        error.to_string(),
        "temporary structured turn completed without a response"
    );
}

#[tokio::test]
async fn rejects_closed_notification_channel() {
    let (sender, receiver) = unbounded_channel();
    drop(sender);

    let error = collect_structured_response(receiver, "turn-1")
        .await
        .expect_err("closed notification channel should fail");

    assert_eq!(
        error.to_string(),
        "temporary structured turn notification channel closed"
    );
}

#[tokio::test]
async fn rejects_oversized_assistant_message() {
    let (sender, receiver) = unbounded_channel();
    let oversized = "x".repeat(STRUCTURED_RESPONSE_MAX_BYTES + 1);
    sender
        .send(agent_message_notification("turn-1", &oversized))
        .expect("send oversized assistant message");
    sender
        .send(turn_completed_notification("turn-1", TurnStatus::Completed))
        .expect("send matching completion");

    let error = collect_structured_response(receiver, "turn-1")
        .await
        .expect_err("oversized assistant message should be rejected");

    assert_eq!(
        error.to_string(),
        "temporary structured response exceeds 8192 bytes"
    );
}
