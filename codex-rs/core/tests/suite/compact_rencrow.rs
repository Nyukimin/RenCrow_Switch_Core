// Modified by RenCrow Switch Core, 2026-09-22.
use super::compact::non_openai_model_provider;
use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_core::config::ThreadStoreConfig;
use codex_features::Feature;
use codex_history::InitialHistory;
use codex_history::ResumedHistory;
use codex_history::RolloutItem;
use codex_history::input_intake::InputAuthor;
use codex_history::input_intake::OriginalInput;
use codex_history::input_intake::SubmissionIntake;
use codex_protocol::items::CommandExecutionStatus;
use codex_protocol::items::TurnItem;
use codex_protocol::mcp::ClientMcpExtensions;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecCommandSource;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::turn_input::TurnInput;
use codex_protocol::user_input::UserInput;
use codex_thread_store::LoadThreadHistoryParams;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_completed_with_tokens;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use serde_json::Value;
use serde_json::json;
use std::fs;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use wiremock::Mock;
use wiremock::Request;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

#[test_case::test_case(false; "manual")]
#[test_case::test_case(true; "automatic")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rencrow_compaction_keeps_one_selected_history_through_resume_and_recompaction(
    automatic: bool,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let seen = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured = Arc::clone(&seen);
    Mock::given(method("POST")).and(path("/v1/responses")).respond_with(move |request: &Request| {
        let body: Value = request.body_json().unwrap();
        captured.lock().unwrap().push(body.clone());
        let joined = body["input"].as_array().unwrap().iter().filter(|item| item["role"] == "user").filter_map(|item| item["content"][0]["text"].as_str()).collect::<String>();
        let data = serde_json::from_str::<Value>(&joined).ok();
        let trigger_auto = automatic && data.is_none() && body["input"].as_array().unwrap().iter().rev()
            .find(|item| item["role"] == "user").is_some_and(|item| item["content"][0]["text"] == "Use current-label instead.");
        let completed = if trigger_auto { ev_completed_with_tokens("response-fixture", 110_000) } else { ev_completed("response-fixture") };
        let reply = if let Some(data) = data {
            if data.get("plan").is_some() {
                json!({"accepted_operations": (0..data["plan"]["operations"].as_array().unwrap().len()).collect::<Vec<_>>()})
            } else if data.get("view_hash").is_some() { json!({"text":"Continue using the current label; preserve Japanese output."})
            } else {
                let sources = data["sources"].as_array().unwrap();
                let old = sources.iter().find(|source| source["text"].as_str().is_some_and(|text| text.contains("Use obsolete-label.")));
                let new = sources.iter().find(|source| source["text"] == "Use current-label instead.");
                match (old,new) {
                    (Some(old),Some(new)) => json!({"operations":[{"action":"drop_superseded","source":old["id"],"source_text":"Use obsolete-label.","correction":new["id"]}]}),
                    _ => json!({"operations":[]}),
                }
            }.to_string()
        } else { "acknowledged".into() };
        ResponseTemplate::new(200).insert_header("content-type","text/event-stream").set_body_string(sse(vec![ev_assistant_message("answer", &reply), completed]))
    }).mount(&server).await;
    let provider = non_openai_model_provider(&server);
    let mut builder = test_codex().with_config(move |config| {
        config.model_provider = provider;
        config.rencrow_compaction = true;
        if automatic {
            config.model_auto_compact_token_limit = Some(100_000);
        }
    });
    let test = builder.build(&server).await?;
    for (client, text) in [
        ("first", "Keep Japanese. Use obsolete-label."),
        ("second", "Use current-label instead."),
    ] {
        let thread = test.session_configured.session_id.to_string();
        let directory = test
            .codex_home_path()
            .join("rencrow/input-intake")
            .join(&thread);
        fs::create_dir_all(&directory)?;
        let receipt = SubmissionIntake::new(
            thread,
            client.into(),
            OriginalInput {
                author: InputAuthor::Human,
                text: text.into(),
                attachments: vec![],
            },
            text,
        )
        .map_err(anyhow::Error::msg)?;
        fs::write(
            directory.join(format!("{client}.json")),
            serde_json::to_vec(&receipt)?,
        )?;
        test.codex
            .start_or_steer_turn(TurnInputRequest::new(TurnInput::UserInput {
                content: vec![UserInput::Text {
                    text: text.into(),
                    text_elements: vec![],
                }],
                client_id: Some(client.into()),
            }))
            .await?;
        wait_for_event(&test.codex, |event| {
            matches!(event, EventMsg::TurnComplete(_))
        })
        .await;
    }
    if !automatic {
        test.codex.submit(Op::Compact).await?;
        wait_for_event(&test.codex, |event| {
            if let EventMsg::Error(error) = event {
                panic!("compaction error: {error:?}");
            }
            matches!(event, EventMsg::TurnComplete(_))
        })
        .await;
    }
    test.submit_text_turn("continue before restart").await?;
    let path = test.codex.rollout_path().unwrap();
    let persisted = fs::read_to_string(&path)?;
    let checkpoints: Vec<Value> = persisted
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|row| row["type"] == "compacted")
        .collect();
    assert_eq!(checkpoints.len(), 1);
    let replacement_metadata =
        checkpoints[0]["payload"]["replacement_history_metadata"].to_string();
    assert!(replacement_metadata.contains("\"summary_review\":null"));
    // Original text remains in the append-only audit log, not replacement content.
    let codex_history::RolloutItem::Compacted(checkpoint) =
        serde_json::from_value(checkpoints[0].clone())?
    else {
        panic!("wrong checkpoint type")
    };
    let actual = checkpoint
        .replacement_history
        .unwrap()
        .iter()
        .map(|envelope| serde_json::to_string(&envelope.item).unwrap())
        .collect::<String>();
    assert!(!actual.contains("Use obsolete-label."));
    assert!(actual.contains("Keep Japanese."));
    assert!(replacement_metadata.contains("rencrow_compaction"));
    let provider = non_openai_model_provider(&server);
    let mut restarted = test_codex().with_config(move |config| {
        config.model_provider = provider;
        config.rencrow_compaction = true;
    });
    let resumed = restarted.restart(&server, &test).await?;
    resumed.submit_text_turn("continue after restart").await?;
    resumed.codex.submit(Op::Compact).await?;
    wait_for_event(&resumed.codex, |event| {
        if let EventMsg::Error(error) = event {
            panic!("second compaction error: {error:?}");
        }
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    resumed
        .submit_text_turn("continue after second compact")
        .await?;
    let requests = seen.lock().unwrap();
    let last = requests.last().unwrap()["input"].to_string();
    assert!(!last.contains("Use obsolete-label."));
    assert!(!last.contains("accepted_operations"));
    assert!(last.contains("Keep Japanese."));
    assert!(last.contains("Continue using the current label"));
    assert_eq!(requests.len(), 11); // Five normal turns and two three-request compactions.
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rencrow_non_human_history_skips_only_selection_inference() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let seen = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured = Arc::clone(&seen);
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(move |request: &Request| {
            let body: Value = request.body_json().expect("request JSON");
            captured.lock().expect("requests lock").push(body.clone());
            let joined = body["input"].as_array().expect("input array").iter()
                .filter(|item| item["role"] == "user")
                .filter_map(|item| item["content"][0]["text"].as_str())
                .collect::<String>();
            let reply = match serde_json::from_str::<Value>(&joined) {
                Ok(data) if data.get("view_hash").is_some() => json!({
                    "text":"Prior response was acknowledged; preserve unattributed input separately."
                }).to_string(),
                Ok(_) => panic!("selection inference must not run without human provenance"),
                Err(_) => "acknowledged".into(),
            };
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse(vec![ev_assistant_message("answer", &reply), ev_completed("response-fixture")]))
        })
        .mount(&server)
        .await;
    let provider = non_openai_model_provider(&server);
    let mut builder = test_codex().with_config(move |config| {
        config.model_provider = provider;
        config.rencrow_compaction = true;
    });
    let test = builder.build(&server).await?;
    test.submit_text_turn("Keep unknown-origin-marker intact.")
        .await?;
    test.codex.submit(Op::Compact).await?;
    wait_for_event(&test.codex, |event| {
        if let EventMsg::Error(error) = event {
            panic!("compaction error: {error:?}");
        }
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    test.submit_text_turn("continue").await?;
    let requests = seen.lock().expect("requests lock");
    assert_eq!(requests.len(), 3); // Two ordinary requests and one summary request.
    assert!(
        requests.last().expect("last request")["input"]
            .to_string()
            .contains("unknown-origin-marker")
    );
    let persisted = fs::read_to_string(test.codex.rollout_path().expect("rollout path"))?;
    assert!(persisted.contains("deterministic_no_human_input"));
    Ok(())
}

fn replacement_history_from_rollout(path: &std::path::Path) -> Result<Vec<Value>> {
    let rollout_text = fs::read_to_string(path)?;
    let mut replacement_history = None;
    for line in rollout_text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let entry = codex_rollout::parse_rollout_line(line)?;
        if let RolloutItem::Compacted(compacted) = entry.item
            && let Some(items) = compacted.replacement_history
        {
            replacement_history = Some(
                items
                    .into_iter()
                    .map(|envelope| serde_json::to_value(envelope.item))
                    .collect::<std::result::Result<Vec<_>, _>>()?,
            );
        }
    }
    replacement_history.ok_or_else(|| anyhow::anyhow!("expected rollout replacement history"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rencrow_completed_unified_exec_pair_is_summarized_after_terminal_receipt() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let seen = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured = Arc::clone(&seen);
    let tool_call_issued = Arc::new(AtomicBool::new(false));
    let summary_count = Arc::new(AtomicUsize::new(0));
    let tool_call_state = Arc::clone(&tool_call_issued);
    let summary_state = Arc::clone(&summary_count);
    let call_id = "completed-work-fixture-call";
    let tool_result = "completed-work-fixture-7f23";
    let call_arguments =
        json!({"cmd":format!("echo {tool_result}"),"yield_time_ms":5_000}).to_string();
    let cold_resume_call_id = "completed-work-cold-resume-call";
    let cold_resume_result = "completed-work-cold-resume-91b4";
    let cold_resume_prompt = "COLD_RESUME_TOOL_PROMPT";
    let cold_resume_arguments = json!({
        "cmd": format!("echo {cold_resume_result}"),
        "yield_time_ms": 5_000
    })
    .to_string();
    let cold_call_issued = Arc::new(AtomicBool::new(false));
    let cold_call_state = Arc::clone(&cold_call_issued);

    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(move |request: &Request| {
            let body: Value = request.body_json().expect("request JSON");
            captured.lock().expect("requests lock").push(body.clone());
            let summary = body["input"].as_array().into_iter().flatten().flat_map(|item| {
                item["content"].as_array().into_iter().flatten()
                    .filter_map(|part| part["text"].as_str())
            }).find_map(|text| {
                serde_json::from_str::<Value>(text).ok()
                    .filter(|value| value.get("view_hash").is_some())
            });
            if let Some(summary) = summary {
                let count = summary_state.fetch_add(1, Ordering::SeqCst);
                if count == 0 {
                    let retained = summary["view"]["retained"].as_array().expect("retained array");
                    let records = retained
                        .iter()
                        .filter_map(|record| record["text"].as_str())
                        .filter_map(|text| serde_json::from_str::<Value>(text).ok())
                        .collect::<Vec<_>>();
                    let call = records
                        .iter()
                        .find(|record| record["call_id"] == call_id && record.get("arguments").is_some())
                        .expect("summary-visible verified call");
                    assert_eq!(call["arguments"].as_str(), Some(call_arguments.as_str()));
                    assert_eq!(call["status"], "completed");
                    assert_eq!(call["exit_code"], 0);
                    let result = records
                        .iter()
                        .find(|record| record["call_id"] == call_id && record.get("result").is_some())
                        .expect("summary-visible verified result");
                    assert_eq!(result["tool"], "exec_command");
                    assert_eq!(result["status"], "completed");
                    assert_eq!(result["exit_code"], 0);
                    assert!(result["result"].as_str().is_some_and(|text| text.contains(tool_result)));
                } else {
                    let retained = summary["view"]["retained"].as_array().expect("retained array");
                    let retained_text =
                        serde_json::to_string(retained).expect("retained view JSON");
                    assert!(!retained_text.contains(call_id), "cold resume revived the previous call");
                    let records = retained
                        .iter()
                        .filter_map(|record| record["text"].as_str())
                        .filter_map(|text| serde_json::from_str::<Value>(text).ok())
                        .collect::<Vec<_>>();
                    let call = records
                        .iter()
                        .find(|record| record["call_id"] == cold_resume_call_id && record.get("arguments").is_some())
                        .expect("cold-resume call should be summary-visible");
                    assert_eq!(call["arguments"].as_str(), Some(cold_resume_arguments.as_str()));
                    assert_eq!(call["status"], "completed");
                    assert_eq!(call["exit_code"], 0);
                    let result = records
                        .iter()
                        .find(|record| record["call_id"] == cold_resume_call_id && record.get("result").is_some())
                        .expect("cold-resume result should be summary-visible");
                    assert_eq!(result["status"], "completed");
                    assert_eq!(result["exit_code"], 0);
                    assert!(result["result"].as_str().is_some_and(|text| text.contains(cold_resume_result)));
                }
                let reply = json!({"text":"A verified command completed; continue from its recorded result."}).to_string();
                return ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse(vec![ev_assistant_message("summary", &reply), ev_completed("summary-response")]));
            }

            let input_text = body["input"].to_string();
            if input_text.contains(cold_resume_prompt)
                && !cold_call_state.swap(true, Ordering::SeqCst)
            {
                assert!(
                    !input_text.contains(call_id),
                    "ordinary cold-resume input revived the compacted command"
                );
                return ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse(vec![
                        ev_response_created("cold-tool-response"),
                        ev_function_call(cold_resume_call_id, "exec_command", &cold_resume_arguments),
                        ev_completed("cold-tool-response"),
                    ]));
            }

            if body["input"].as_array().is_some_and(|items| {
                items.iter().any(|item| item["type"] == "function_call_output")
            }) || tool_call_state.load(Ordering::SeqCst) {
                return ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse(vec![ev_assistant_message("ack", "command completed"), ev_completed("normal-response")]));
            }

            tool_call_state.store(true, Ordering::SeqCst);
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse(vec![
                    ev_response_created("tool-response"),
                    ev_function_call(call_id, "exec_command", &call_arguments),
                    ev_completed("tool-response"),
                ]))
        })
        .mount(&server)
        .await;

    let provider = non_openai_model_provider(&server);
    let mut builder = test_codex()
        .with_history_mode(ThreadHistoryMode::Paginated)
        .with_config(move |config| {
            config.model_provider = provider;
            config.rencrow_compaction = true;
            config.experimental_thread_store = ThreadStoreConfig::Local;
            config
                .features
                .enable(Feature::UnifiedExec)
                .expect("completed-work fixture must enable UnifiedExec");
        });
    let test = builder.build(&server).await?;
    test.submit_text_turn("Run the completed-work fixture command.")
        .await?;

    test.codex.flush_rollout().await?;
    let rollout_path = test.codex.rollout_path().expect("rollout path");
    let prefix = fs::read(&rollout_path)?;
    let mut terminal_count = 0;
    let mut observed_call_items = Vec::new();
    for line in std::str::from_utf8(&prefix)?
        .lines()
        .filter(|line| !line.is_empty())
    {
        let item = codex_rollout::parse_rollout_line(line)?.item;
        match item {
            RolloutItem::ResponseItem(envelope) => match &envelope.item {
                ResponseItem::FunctionCall {
                    call_id: observed_id,
                    ..
                }
                | ResponseItem::FunctionCallOutput {
                    call_id: Some(observed_id),
                    ..
                } if observed_id == call_id => {
                    observed_call_items.push(serde_json::to_value(&envelope.item)?.to_string());
                }
                _ => {}
            },
            RolloutItem::EventMsg(EventMsg::ItemCompleted(event)) => {
                if let TurnItem::CommandExecution(command) = event.item
                    && command.id == call_id
                {
                    terminal_count += 1;
                    observed_call_items.push(format!(
                        "ItemCompleted source={:?} status={:?} exit_code={:?}",
                        command.source, command.status, command.exit_code
                    ));
                    assert_eq!(command.source, ExecCommandSource::UnifiedExecStartup);
                    assert_eq!(command.status, CommandExecutionStatus::Completed);
                    assert_eq!(command.exit_code, Some(0));
                }
            }
            _ => {}
        }
    }
    assert_eq!(
        terminal_count, 1,
        "expected a canonical ItemCompleted receipt; observed call records: {observed_call_items:?}"
    );

    test.codex.submit(Op::Compact).await?;
    wait_for_event(&test.codex, |event| {
        if let EventMsg::Error(error) = event {
            panic!("compaction error: {error:?}");
        }
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let compacted = fs::read(&rollout_path)?;
    assert!(
        compacted.starts_with(&prefix),
        "compaction rewrote the original rollout prefix"
    );
    let replacement = replacement_history_from_rollout(&rollout_path)?;
    assert!(!replacement.iter().any(|item| item["call_id"] == call_id));
    assert!(
        !replacement
            .iter()
            .any(|item| item["type"] == "function_call_output" && item["call_id"] == call_id)
    );

    test.codex.shutdown_and_wait().await?;
    let context = test
        .thread_store
        .load_latest_model_context(LoadThreadHistoryParams {
            thread_id: test.session_configured.thread_id,
            include_archived: false,
        })
        .await?;
    let resumed = test
        .thread_manager
        .resume_thread_with_history(
            test.config.clone(),
            InitialHistory::Resumed(ResumedHistory {
                conversation_id: context.thread_id,
                history: Arc::new(context.items),
                rollout_path: Some(rollout_path.clone()),
            }),
            test.thread_manager.auth_manager(),
            /*parent_trace*/ None,
            ClientMcpExtensions::default(),
        )
        .await?;
    let resumed_thread = resumed.thread;
    resumed_thread
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: cold_resume_prompt.to_owned(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(&resumed_thread, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    resumed_thread.flush_rollout().await?;
    let cold_resume_rollout = fs::read(&rollout_path)?;
    assert!(cold_resume_rollout.starts_with(&compacted));
    let mut cold_terminal_count = 0;
    for line in std::str::from_utf8(&cold_resume_rollout)?
        .lines()
        .filter(|line| !line.is_empty())
    {
        if let RolloutItem::EventMsg(EventMsg::ItemCompleted(event)) =
            codex_rollout::parse_rollout_line(line)?.item
            && let TurnItem::CommandExecution(command) = event.item
            && command.id == cold_resume_call_id
        {
            cold_terminal_count += 1;
            assert_eq!(command.source, ExecCommandSource::UnifiedExecStartup);
            assert_eq!(command.status, CommandExecutionStatus::Completed);
            assert_eq!(command.exit_code, Some(0));
        }
    }
    assert_eq!(
        cold_terminal_count, 1,
        "cold-resume command must have a canonical terminal receipt"
    );

    resumed_thread.submit(Op::Compact).await?;
    wait_for_event(&resumed_thread, |event| {
        if let EventMsg::Error(error) = event {
            panic!("second compaction error: {error:?}");
        }
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let recompacted = fs::read(&rollout_path)?;
    assert!(recompacted.starts_with(&cold_resume_rollout));
    let replacement = replacement_history_from_rollout(&rollout_path)?;
    assert!(
        !replacement
            .iter()
            .any(|item| { item["call_id"] == call_id || item["call_id"] == cold_resume_call_id })
    );
    assert!(!replacement.iter().any(|item| {
        item["type"] == "function_call_output"
            && (item["call_id"] == call_id || item["call_id"] == cold_resume_call_id)
    }));
    assert_eq!(summary_count.load(Ordering::SeqCst), 2);
    assert_eq!(seen.lock().expect("requests lock").len(), 6);
    Ok(())
}
