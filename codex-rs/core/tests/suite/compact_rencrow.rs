// Modified by RenCrow Switch Core, 2026-09-22; Normal V2 compaction 2026-09-24.
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
use core_test_support::test_codex::TestCodex;
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

/// Developer instruction prefix of the single V2 summary request.
const SUMMARY_REQUEST: &str = "This is a read-only compaction summary request.";
/// Developer instruction prefix of the JSON-dataset instruction selection request.
const SELECTION_STAGE: &str = "This is a read-only compaction stage.";

enum Stage {
    Selection(Value),
    Summary,
    Ordinary,
}

fn item_texts(item: &Value) -> Vec<&str> {
    item["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|part| part["text"].as_str())
        .collect()
}

fn stage(body: &Value) -> Stage {
    let input = body["input"].as_array().expect("input array");
    let developer = input
        .iter()
        .filter(|item| item["role"] == "developer")
        .flat_map(item_texts)
        .collect::<String>();
    if developer.contains(SUMMARY_REQUEST) {
        return Stage::Summary;
    }
    if developer.contains(SELECTION_STAGE) {
        let dataset = input
            .iter()
            .filter(|item| item["role"] == "user")
            .flat_map(item_texts)
            .collect::<String>();
        return Stage::Selection(serde_json::from_str(&dataset).expect("selection dataset"));
    }
    Stage::Ordinary
}

fn assistant_reply(id: &str, text: &str, completed: Value) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(sse(vec![ev_assistant_message(id, text), completed]))
}

fn checkpoint_rows(path: &std::path::Path) -> Result<Vec<Value>> {
    Ok(fs::read_to_string(path)?
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|row| row["type"] == "compacted")
        .collect())
}

/// Submit a turn whose text has an accepted Human intake receipt.
async fn submit_human_turn(test: &TestCodex, client: &str, text: &str) -> Result<()> {
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
    Ok(())
}

/// Run a manual compaction and fail the test on any error event.
async fn compact_without_error(codex: &codex_core::CodexThread) -> Result<()> {
    codex.submit(Op::Compact).await?;
    wait_for_event(codex, |event| {
        if let EventMsg::Error(error) = event {
            panic!("compaction error: {error:?}");
        }
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    Ok(())
}

/// The RenCrow metadata stored on the checkpoint summary of one compacted rollout row.
fn checkpoint_metadata(row: &Value) -> Value {
    row["payload"]["replacement_history_metadata"]
        .as_array()
        .expect("replacement metadata")
        .iter()
        .find_map(|metadata| metadata.get("rencrow_compaction").cloned())
        .expect("checkpoint metadata")
}

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
    let responses = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(move |request: &Request| {
            let body: Value = request.body_json().expect("request JSON");
            captured.lock().expect("requests lock").push(body.clone());
            // Each response item needs its own ID, as in real model output.
            let id = format!("answer-{}", responses.fetch_add(1, Ordering::SeqCst));
            match stage(&body) {
                Stage::Selection(data) => {
                    let sources = data["sources"].as_array().expect("sources");
                    let old = sources.iter().find(|source| {
                        source["text"]
                            .as_str()
                            .is_some_and(|text| text.contains("Use obsolete-label."))
                    });
                    let new = sources
                        .iter()
                        .find(|source| source["text"] == "Use current-label instead.");
                    let operations = match (old, new) {
                        (Some(old), Some(new)) => json!([{
                            "action": "drop_superseded",
                            "source": old["id"],
                            "source_text": "Use obsolete-label.",
                            "correction": new["id"],
                            "correction_text": "Use current-label instead.",
                        }]),
                        _ => json!([]),
                    };
                    assistant_reply(
                        &id,
                        &json!({ "operations": operations }).to_string(),
                        ev_completed("selection-response"),
                    )
                }
                Stage::Summary => assistant_reply(
                    &id,
                    "Continue using the current label; preserve Japanese output.",
                    ev_completed("summary-response"),
                ),
                Stage::Ordinary => {
                    let trigger_auto = automatic
                        && body["input"]
                            .as_array()
                            .expect("input array")
                            .iter()
                            .rev()
                            .find(|item| item["role"] == "user")
                            .is_some_and(|item| {
                                item["content"][0]["text"] == "Use current-label instead."
                            });
                    let completed = if trigger_auto {
                        ev_completed_with_tokens("response-fixture", 110_000)
                    } else {
                        ev_completed("response-fixture")
                    };
                    assistant_reply(&id, "acknowledged", completed)
                }
            }
        })
        .mount(&server)
        .await;
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
        submit_human_turn(&test, client, text).await?;
    }
    if !automatic {
        compact_without_error(&test.codex).await?;
    }
    test.submit_text_turn("continue before restart").await?;
    let path = test.codex.rollout_path().unwrap();
    let checkpoints = checkpoint_rows(&path)?;
    assert_eq!(checkpoints.len(), 1);
    let replacement_metadata =
        checkpoints[0]["payload"]["replacement_history_metadata"].to_string();
    assert!(replacement_metadata.contains("\"version\":2"));
    assert!(replacement_metadata.contains("\"selection_mode\":\"model_selection\""));
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
    let checkpoints = checkpoint_rows(&path)?;
    assert_eq!(checkpoints.len(), 2);
    // The second checkpoint has no new Human, so it sends no selection request.
    assert!(
        checkpoints[1]["payload"]["replacement_history_metadata"]
            .to_string()
            .contains("\"selection_mode\":\"no_candidates\"")
    );
    let requests = seen.lock().unwrap();
    let selections = requests
        .iter()
        .filter(|body| matches!(stage(body), Stage::Selection(_)))
        .count();
    let summaries = requests
        .iter()
        .filter(|body| matches!(stage(body), Stage::Summary))
        .map(|body| body["input"].to_string())
        .collect::<Vec<_>>();
    assert_eq!(selections, 1);
    assert_eq!(summaries.len(), 2);
    for summary in &summaries {
        assert!(!summary.contains("Use obsolete-label."));
        assert!(summary.contains("Keep Japanese."));
    }
    let last = requests.last().unwrap()["input"].to_string();
    assert!(!last.contains("Use obsolete-label."));
    assert!(last.contains("Keep Japanese."));
    assert!(last.contains("Continue using the current label"));
    // Five ordinary turns, one selection, and one summary for each of two compactions.
    assert_eq!(requests.len(), 8);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rencrow_non_human_history_skips_only_selection_inference() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let seen = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured = Arc::clone(&seen);
    let responses = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(move |request: &Request| {
            let body: Value = request.body_json().expect("request JSON");
            captured.lock().expect("requests lock").push(body.clone());
            let id = format!("answer-{}", responses.fetch_add(1, Ordering::SeqCst));
            let reply = match stage(&body) {
                Stage::Summary => {
                    "Prior response was acknowledged; preserve unattributed input separately."
                }
                Stage::Selection(_) | Stage::Ordinary => "acknowledged",
            };
            assistant_reply(&id, reply, ev_completed("response-fixture"))
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
    assert!(
        !requests
            .iter()
            .any(|body| matches!(stage(body), Stage::Selection(_))),
        "selection inference must not run without human provenance"
    );
    assert_eq!(requests.len(), 3); // Two ordinary requests and one summary request.
    assert!(
        requests.last().expect("last request")["input"]
            .to_string()
            .contains("unknown-origin-marker")
    );
    let checkpoints = checkpoint_rows(&test.codex.rollout_path().expect("rollout path"))?;
    assert_eq!(checkpoints.len(), 1);
    assert!(
        checkpoints[0]["payload"]["replacement_history_metadata"]
            .to_string()
            .contains("\"selection_mode\":\"no_candidates\"")
    );
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
            if matches!(stage(&body), Stage::Summary) {
                let count = summary_state.fetch_add(1, Ordering::SeqCst);
                return assistant_reply(
                    &format!("summary-{count}"),
                    "A verified command completed; continue from its recorded result.",
                    ev_completed("summary-response"),
                );
            }

            let input_text = body["input"].to_string();
            if input_text.contains(cold_resume_prompt)
                && !cold_call_state.swap(true, Ordering::SeqCst)
            {
                return ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse(vec![
                        ev_response_created("cold-tool-response"),
                        ev_function_call(
                            cold_resume_call_id,
                            "exec_command",
                            &cold_resume_arguments,
                        ),
                        ev_completed("cold-tool-response"),
                    ]));
            }

            if body["input"].as_array().is_some_and(|items| {
                items
                    .iter()
                    .any(|item| item["type"] == "function_call_output")
            }) || tool_call_state.load(Ordering::SeqCst)
            {
                return ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse(vec![
                        ev_assistant_message("ack", "command completed"),
                        ev_completed("normal-response"),
                    ]));
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
    let checkpoints = checkpoint_rows(&rollout_path)?;
    let covered = checkpoints[0]["payload"]["replacement_history_metadata"]
        .as_array()
        .expect("replacement metadata")
        .iter()
        .find_map(|metadata| {
            metadata["rencrow_compaction"]["summary_covered_observations"].as_array()
        })
        .expect("checkpoint metadata")
        .iter()
        .map(|reference| reference["call_id"].clone())
        .collect::<Vec<_>>();
    assert_eq!(covered, vec![json!(call_id)]);
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
    let requests = seen.lock().expect("requests lock");
    assert_eq!(requests.len(), 6);
    let summaries = requests
        .iter()
        .filter(|body| matches!(stage(body), Stage::Summary))
        .collect::<Vec<_>>();
    assert_eq!(summaries.len(), 2);
    // Each verified pair reaches the summary once as a bounded observation, never as raw items.
    for (summary, (expected_call, expected_result, earlier_call)) in summaries.iter().zip([
        (call_id, tool_result, None),
        (cold_resume_call_id, cold_resume_result, Some(call_id)),
    ]) {
        let input = summary["input"].as_array().expect("summary input");
        assert!(!input.iter().any(|item| {
            matches!(
                item["type"].as_str(),
                Some("function_call" | "function_call_output")
            )
        }));
        let observations = input
            .iter()
            .flat_map(item_texts)
            .filter(|text| text.starts_with("{\"observation\":"))
            .collect::<Vec<_>>();
        assert_eq!(
            observations.len(),
            1,
            "summary observations: {observations:?}"
        );
        let observation: Value = serde_json::from_str(observations[0])?;
        assert_eq!(
            observation["observation"]["reference"]["call_id"],
            expected_call
        );
        assert_eq!(observation["observation"]["tool_name"], "exec_command");
        assert!(observation.to_string().contains(expected_result));
        if let Some(earlier_call) = earlier_call {
            assert!(
                !summary["input"].to_string().contains(earlier_call),
                "cold resume revived the previous call"
            );
        }
    }
    let cold_resume_turn = requests
        .iter()
        .find(|body| body["input"].to_string().contains(cold_resume_prompt))
        .expect("cold-resume request");
    assert!(
        !cold_resume_turn["input"].to_string().contains(call_id),
        "ordinary cold-resume input revived the compacted command"
    );
    Ok(())
}

/// Placeholder body of an emergency checkpoint without a previous semantic summary.
const EMERGENCY_PLACEHOLDER: &str = "No semantic summary has been accepted.";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rencrow_malformed_summary_commits_emergency_and_next_normal_summarizes_its_work()
-> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let seen = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured = Arc::clone(&seen);
    let responses = Arc::new(AtomicUsize::new(0));
    let summaries = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(move |request: &Request| {
            let body: Value = request.body_json().expect("request JSON");
            captured.lock().expect("requests lock").push(body.clone());
            let id = format!("answer-{}", responses.fetch_add(1, Ordering::SeqCst));
            match stage(&body) {
                // An empty summary is malformed; Normal fails and Emergency must not retry it.
                Stage::Summary if summaries.fetch_add(1, Ordering::SeqCst) == 0 => {
                    assistant_reply(&id, " ", ev_completed("bad-summary"))
                }
                Stage::Summary => assistant_reply(
                    &id,
                    "Both tasks progressed; continue with the second task.",
                    ev_completed("summary-response"),
                ),
                Stage::Selection(_) | Stage::Ordinary => {
                    let input = body["input"].to_string();
                    let reply = if input.contains("second task") {
                        "WORK-TWO-RESULT"
                    } else {
                        "WORK-ONE-RESULT"
                    };
                    assistant_reply(&id, reply, ev_completed("response-fixture"))
                }
            }
        })
        .mount(&server)
        .await;
    let provider = non_openai_model_provider(&server);
    let mut builder = test_codex().with_config(move |config| {
        config.model_provider = provider;
        config.rencrow_compaction = true;
    });
    let test = builder.build(&server).await?;
    test.submit_text_turn("first task").await?;
    compact_without_error(&test.codex).await?;

    let path = test.codex.rollout_path().expect("rollout path");
    let checkpoints = checkpoint_rows(&path)?;
    assert_eq!(checkpoints.len(), 1);
    assert!(
        checkpoints[0]["payload"]
            .get("compaction_response_id")
            .is_none_or(Value::is_null)
    );
    let emergency = checkpoint_metadata(&checkpoints[0]);
    assert_eq!(emergency["selection_mode"], "deterministic_emergency");
    assert!(emergency.get("model").is_none());
    assert!(emergency.get("semantic_summary_hash").is_none());
    assert_eq!(emergency["responses"], json!([]));
    let replacement = serde_json::to_string(&replacement_history_from_rollout(&path)?)?;
    assert!(replacement.contains("WORK-ONE-RESULT"));
    assert!(replacement.contains(EMERGENCY_PLACEHOLDER));
    // One ordinary turn and the single failed summary request; Emergency adds none.
    assert_eq!(seen.lock().expect("requests lock").len(), 2);

    test.submit_text_turn("second task").await?;
    compact_without_error(&test.codex).await?;

    let checkpoints = checkpoint_rows(&path)?;
    assert_eq!(checkpoints.len(), 2);
    assert_eq!(
        checkpoint_metadata(&checkpoints[1])["selection_mode"],
        "no_candidates"
    );
    let requests = seen.lock().expect("requests lock");
    assert_eq!(requests.len(), 4);
    let summary = requests
        .iter()
        .filter(|body| matches!(stage(body), Stage::Summary))
        .nth(1)
        .expect("second summary request")["input"]
        .to_string();
    // Work retained by Emergency is still unsummarized, and the placeholder is not a summary.
    assert!(summary.contains("WORK-ONE-RESULT"));
    assert!(summary.contains("WORK-TWO-RESULT"));
    assert!(!summary.contains(EMERGENCY_PLACEHOLDER));
    let replacement = serde_json::to_string(&replacement_history_from_rollout(&path)?)?;
    assert!(!replacement.contains("WORK-ONE-RESULT"));
    assert!(!replacement.contains(EMERGENCY_PLACEHOLDER));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rencrow_malformed_selection_keeps_every_human_exactly_in_emergency() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let seen = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured = Arc::clone(&seen);
    let responses = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(move |request: &Request| {
            let body: Value = request.body_json().expect("request JSON");
            captured.lock().expect("requests lock").push(body.clone());
            let id = format!("answer-{}", responses.fetch_add(1, Ordering::SeqCst));
            let reply = match stage(&body) {
                Stage::Selection(_) => "not a selection",
                Stage::Summary => "unexpected summary",
                Stage::Ordinary => "acknowledged",
            };
            assistant_reply(&id, reply, ev_completed("response-fixture"))
        })
        .mount(&server)
        .await;
    let provider = non_openai_model_provider(&server);
    let mut builder = test_codex().with_config(move |config| {
        config.model_provider = provider;
        config.rencrow_compaction = true;
    });
    let test = builder.build(&server).await?;
    submit_human_turn(&test, "first", "Keep Japanese. Use obsolete-label.").await?;
    submit_human_turn(&test, "second", "Use current-label instead.").await?;
    compact_without_error(&test.codex).await?;

    let requests = seen.lock().expect("requests lock");
    assert_eq!(
        requests
            .iter()
            .filter(|body| matches!(stage(body), Stage::Selection(_)))
            .count(),
        1
    );
    assert!(
        !requests
            .iter()
            .any(|body| matches!(stage(body), Stage::Summary))
    );
    assert_eq!(requests.len(), 3);
    let path = test.codex.rollout_path().expect("rollout path");
    let checkpoints = checkpoint_rows(&path)?;
    assert_eq!(checkpoints.len(), 1);
    assert_eq!(
        checkpoint_metadata(&checkpoints[0])["selection_mode"],
        "deterministic_emergency"
    );
    // Without an accepted selection nothing is removed; both instructions stay verbatim.
    let replacement = serde_json::to_string(&replacement_history_from_rollout(&path)?)?;
    assert!(replacement.contains("Keep Japanese. Use obsolete-label."));
    assert!(replacement.contains("Use current-label instead."));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rencrow_emergency_marker_is_presented_and_covered_by_the_next_normal() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let seen = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured = Arc::clone(&seen);
    let responses = Arc::new(AtomicUsize::new(0));
    let summaries = Arc::new(AtomicUsize::new(0));
    let tool_call_issued = Arc::new(AtomicBool::new(false));
    let call_id = "emergency-marker-call";
    let call_arguments =
        json!({"cmd":"yes emergency-marker-line | head -n 400","yield_time_ms":5_000}).to_string();
    let marker_arguments = call_arguments.clone();
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(move |request: &Request| {
            let body: Value = request.body_json().expect("request JSON");
            captured.lock().expect("requests lock").push(body.clone());
            let id = format!("answer-{}", responses.fetch_add(1, Ordering::SeqCst));
            match stage(&body) {
                Stage::Summary if summaries.fetch_add(1, Ordering::SeqCst) == 0 => {
                    assistant_reply(&id, " ", ev_completed("bad-summary"))
                }
                Stage::Summary => assistant_reply(
                    &id,
                    "The marker command printed repeated lines.",
                    ev_completed("summary-response"),
                ),
                Stage::Selection(_) => {
                    assistant_reply(&id, "{\"operations\":[]}", ev_completed("selection"))
                }
                Stage::Ordinary if !tool_call_issued.swap(true, Ordering::SeqCst) => {
                    ResponseTemplate::new(200)
                        .insert_header("content-type", "text/event-stream")
                        .set_body_string(sse(vec![
                            ev_response_created("tool-response"),
                            ev_function_call(call_id, "exec_command", &marker_arguments),
                            ev_completed("tool-response"),
                        ]))
                }
                Stage::Ordinary => {
                    assistant_reply(&id, "command completed", ev_completed("normal-response"))
                }
            }
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
                .expect("marker fixture must enable UnifiedExec");
        });
    let test = builder.build(&server).await?;
    test.submit_text_turn("Run the marker fixture command.")
        .await?;
    compact_without_error(&test.codex).await?;

    let path = test.codex.rollout_path().expect("rollout path");
    let checkpoints = checkpoint_rows(&path)?;
    assert_eq!(checkpoints.len(), 1);
    let emergency = checkpoint_metadata(&checkpoints[0]);
    assert_eq!(emergency["selection_mode"], "deterministic_emergency");
    assert_eq!(
        emergency["observations"][0]["reference"]["call_id"],
        call_id
    );
    assert_eq!(emergency["summary_covered_observations"], json!([]));
    let replacement = replacement_history_from_rollout(&path)?;
    // The call stays; only the output body became a verified marker.
    assert!(replacement.iter().any(|item| {
        item["type"] == "function_call"
            && item["call_id"] == call_id
            && item["arguments"] == call_arguments.as_str()
    }));
    let marker = replacement
        .iter()
        .find(|item| item["type"] == "function_call_output" && item["call_id"] == call_id)
        .expect("marker output");
    let marker_body: Value = serde_json::from_str(marker["output"].as_str().expect("marker text"))?;
    assert_eq!(marker_body["rencrow_observation"], json!(true));
    assert_eq!(marker_body["call_id"], call_id);
    assert!(marker_body["total_bytes"].as_u64().expect("total bytes") > 4_000);

    test.submit_text_turn("continue").await?;
    compact_without_error(&test.codex).await?;

    let checkpoints = checkpoint_rows(&path)?;
    assert_eq!(checkpoints.len(), 2);
    // The live marker keeps the tool-output shape the model accepts in an ordinary turn.
    let continued = seen
        .lock()
        .expect("requests lock")
        .iter()
        .find(|body| {
            matches!(stage(body), Stage::Ordinary) && body["input"].to_string().contains("continue")
        })
        .cloned()
        .expect("ordinary request after emergency");
    assert!(
        continued["input"]
            .as_array()
            .expect("input")
            .iter()
            .any(|item| {
                item["type"] == "function_call_output"
                    && item["call_id"] == call_id
                    && item["output"]
                        .as_str()
                        .is_some_and(|output| output.contains("\"rencrow_observation\":true"))
            })
    );
    let normal = checkpoint_metadata(&checkpoints[1]);
    assert_eq!(normal["selection_mode"], "no_candidates");
    assert_eq!(
        normal["summary_covered_observations"][0]["call_id"],
        call_id
    );
    let requests = seen.lock().expect("requests lock");
    let summary = requests
        .iter()
        .filter(|body| matches!(stage(body), Stage::Summary))
        .nth(1)
        .expect("second summary request");
    let input = summary["input"].as_array().expect("summary input");
    assert!(!input.iter().any(|item| {
        matches!(
            item["type"].as_str(),
            Some("function_call" | "function_call_output")
        )
    }));
    let observation = input
        .iter()
        .flat_map(item_texts)
        .find(|text| text.starts_with("{\"observation\":"))
        .expect("marker re-presented as an observation");
    let observation: Value = serde_json::from_str(observation)?;
    assert_eq!(observation["observation"]["reference"]["call_id"], call_id);
    let replacement = replacement_history_from_rollout(&path)?;
    assert!(!replacement.iter().any(|item| item["call_id"] == call_id));
    Ok(())
}
