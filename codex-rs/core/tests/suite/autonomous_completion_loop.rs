//! Modified by RenCrow on 2026-11-16: exercise native reviewer recovery against real tool flows.
//! Autonomous completion claims bind to real accepted task receipts and reopen when stale.
use anyhow::Result;
use codex_config::RencrowExecutionGuard;
use codex_core::TurnInputRequest;
use codex_core::config::AgentRoleConfig;
use codex_core::config::Config;
use codex_features::Feature;
use codex_protocol::models::PermissionProfile;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::turn_input::TurnInputSubmission;
use codex_protocol::user_input::UserInput;
use codex_rollout::RolloutItem;
use core_test_support::responses::ev_apply_patch_custom_tool_call;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once_match;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::mount_sse_sequence_match;
use core_test_support::responses::sse;
use core_test_support::responses::sse_failed;
use core_test_support::responses::sse_response;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use serde_json::Value;
use serde_json::json;
use std::fs;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

const REVIEW_MODEL: &str = "gpt-5.6-sol";
const REVIEW_ROLE: &str = "reviewer";

fn function_response(call_id: &str, name: &str, args: &Value) -> String {
    sse(vec![
        ev_response_created(&format!("response-{call_id}")),
        ev_function_call(call_id, name, &args.to_string()),
        ev_completed(&format!("response-{call_id}")),
    ])
}

async fn mount_function_turn(
    server: &wiremock::MockServer,
    parent_thread_id: &str,
    call_id: &str,
    name: &str,
    args: &Value,
    message: &str,
) {
    mount_parent_sse_sequence(
        server,
        parent_thread_id,
        vec![
            function_response(call_id, name, args),
            sse(vec![
                ev_response_created(&format!("{call_id}-finished")),
                ev_assistant_message(&format!("{call_id}-message"), message),
                ev_completed(&format!("{call_id}-finished")),
            ]),
        ],
    )
    .await;
}

async fn mount_multi_agent_turn(
    server: &wiremock::MockServer,
    parent_thread_id: &str,
    call_id: &str,
    tool: &str,
    args: &Value,
    message: &str,
) {
    mount_parent_sse_sequence(
        server,
        parent_thread_id,
        vec![
            sse(vec![
                ev_response_created(&format!("{call_id}-parent")),
                ev_function_call_with_namespace(call_id, "multi_agent_v1", tool, &args.to_string()),
                ev_completed(&format!("{call_id}-parent")),
            ]),
            sse(vec![
                ev_response_created(&format!("{call_id}-finished")),
                ev_assistant_message(&format!("{call_id}-message"), message),
                ev_completed(&format!("{call_id}-finished")),
            ]),
        ],
    )
    .await;
}

struct ParentThreadRequest(String);

impl wiremock::Match for ParentThreadRequest {
    fn matches(&self, request: &wiremock::Request) -> bool {
        request
            .body_json::<Value>()
            .ok()
            .and_then(|body| {
                body["client_metadata"]["thread_id"]
                    .as_str()
                    .map(str::to_string)
            })
            .as_deref()
            == Some(self.0.as_str())
    }
}

async fn mount_parent_sse_sequence(
    server: &wiremock::MockServer,
    parent_thread_id: &str,
    bodies: Vec<String>,
) {
    mount_sse_sequence_match(
        server,
        ParentThreadRequest(parent_thread_id.to_string()),
        bodies,
    )
    .await;
}

fn is_completion_reviewer_request(request: &wiremock::Request, parent_thread_id: &str) -> bool {
    let Ok(body) = request.body_json::<Value>() else {
        return false;
    };
    let has_reviewer_prompt = body["input"].as_array().is_some_and(|items| {
        items.iter().any(|item| {
            item["type"] == "message"
                && item["role"] == "user"
                && item["content"].as_array().is_some_and(|content| {
                    content.iter().any(|part| {
                        part["type"] == "input_text"
                            && part["text"]
                                .as_str()
                                .is_some_and(|text| text.contains("Host-assigned read-only review"))
                    })
                })
        })
    });
    has_reviewer_prompt
        && body["model"] == REVIEW_MODEL
        && body["client_metadata"]["thread_id"].as_str() != Some(parent_thread_id)
        && body["client_metadata"]["x-codex-parent-thread-id"].as_str() == Some(parent_thread_id)
}

async fn completion_reviewer_request_diagnostics(
    server: &wiremock::MockServer,
    parent_thread_id: &str,
    child_thread_id: &str,
) -> String {
    let requests = server.received_requests().await.unwrap_or_default();
    let mut summaries = Vec::new();
    for (index, request) in requests.iter().enumerate() {
        if request.method != wiremock::http::Method::POST
            || !request.url.path().ends_with("/responses")
        {
            continue;
        }
        let body = if request
            .headers
            .get("content-encoding")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value
                    .split(',')
                    .any(|encoding| encoding.trim().eq_ignore_ascii_case("zstd"))
            }) {
            zstd::stream::decode_all(std::io::Cursor::new(&request.body)).ok()
        } else {
            Some(request.body.clone())
        };
        let parsed = body
            .as_deref()
            .and_then(|body| serde_json::from_slice::<Value>(body).ok());
        let Some(body) = parsed else {
            summaries.push(format!("request[{index}] json_decoded=false"));
            continue;
        };
        let has_review_prompt = body["input"].as_array().is_some_and(|items| {
            items.iter().any(|item| {
                item["type"] == "message"
                    && item["role"] == "user"
                    && item["content"].as_array().is_some_and(|content| {
                        content.iter().any(|part| {
                            part["type"] == "input_text"
                                && part["text"].as_str().is_some_and(|text| {
                                    text.contains("Host-assigned read-only review")
                                })
                        })
                    })
            })
        });
        summaries.push(format!(
            "request[{index}] model_matches={} child_thread_matches={} parent_thread_matches={} review_prompt_present={}",
            body["model"] == REVIEW_MODEL,
            body["client_metadata"]["thread_id"].as_str() == Some(child_thread_id),
            body["client_metadata"]["x-codex-parent-thread-id"].as_str()
                == Some(parent_thread_id),
            has_review_prompt,
        ));
    }
    format!(
        "server_response_requests={} [{}]",
        summaries.len(),
        summaries.join("; ")
    )
}

fn open_contract() -> Value {
    json!({
        "contract_revision": 1,
        "conditions": [{
            "id":"AC-1", "revision":1,
            "result":"A verified feature is available",
            "scope":"CLI feature",
            "verification":"Verify the output artifact",
            "acceptance_criteria":"The artifact contains the expected result",
            "status":"open", "evidence":[], "unmet_reason":null
        }],
        "units": [{
            "id":"unit-a", "purpose":"Implement and verify the feature",
            "status":"ready", "condition_refs":["AC-1"], "depends_on":[],
            "evidence":[], "task_refs":[]
        }],
        "current_unit":"unit-a", "failed_approaches":[], "blocker":null,
        "next_action":"Start the implementation task",
        "product_progress_at":null, "runtime_recovery_at":null, "activity_at":null
    })
}

fn claim(status: &str, evidence_refs: Vec<Value>, unit_status: &str) -> Value {
    let unit_task_refs = if unit_status == "done" {
        evidence_refs.clone()
    } else {
        Vec::new()
    };
    json!({
        "contract_revision":1,
        "conditions":[{
            "id":"AC-1", "revision":1,
            "result":"A verified feature is available", "scope":"CLI feature",
            "verification":"Verify the output artifact",
            "acceptance_criteria":"The artifact contains the expected result",
            "status":status, "evidence":evidence_refs, "unmet_reason":null
        }],
        "units":[
            {
                "id":"unit-a", "purpose":"Implement and verify the feature",
                "status":unit_status, "condition_refs":["AC-1"], "depends_on":[],
                "evidence":[], "task_refs":unit_task_refs
            },
            {
                "id":"task-c", "purpose":"Complete the next accepted execution task",
                "status":"ready", "condition_refs":["AC-1"], "depends_on":[],
                "evidence":[], "task_refs":[]
            },
            {
                "id":"task-d", "purpose":"Complete the following accepted execution task",
                "status":"planned", "condition_refs":["AC-1"], "depends_on":["task-c"],
                "evidence":[], "task_refs":[]
            }
        ],
        "current_unit": if unit_status == "ready" { Some("unit-a") } else { None },
        "failed_approaches":[], "blocker":null, "next_action":"Continue with verified evidence",
        "product_progress_at":0, "runtime_recovery_at":null, "activity_at":0
    })
}

fn evidence(task_id: &str) -> Value {
    json!({"task_id":task_id, "task_revision":1})
}

fn write_artifact_content_test(
    cwd: &std::path::Path,
    test_filename: &str,
    artifact_filename: &str,
) -> Result<()> {
    let tests_dir = cwd.join("tests");
    fs::create_dir_all(&tests_dir)?;
    fs::write(
        tests_dir.join(test_filename),
        format!(
            "import unittest\nfrom pathlib import Path\n\nclass ArtifactContent(unittest.TestCase):\n    def test_accepted_artifact_content(self):\n        self.assertEqual(Path({artifact_filename:?}).read_text(), \"accepted artifact\\n\")\n"
        ),
    )?;
    Ok(())
}

fn configure_completion_review_test(config: &mut Config, provider_url: &str) {
    config.rencrow_compaction = true;
    config.rencrow_autonomous_completion = true;
    config.rencrow_tool_allowlist = Some(vec![
        "exec_command".into(),
        "task_context_boundary".into(),
        "multi_agent_v1::wait_agent".into(),
        "multi_agent_v1::resume_agent".into(),
    ]);
    config
        .features
        .enable(Feature::Collab)
        .expect("enable collaboration for the isolated reviewer test");
    let model_provider_id = config.model_provider_id.clone();
    config.model_provider.stream_idle_timeout_ms = Some(12 * 60 * 1000);
    config
        .model_providers
        .insert(model_provider_id.clone(), config.model_provider.clone());
    let role_path = config.codex_home.join("completion-reviewer.toml");
    // The role selects the test's injected `openai` route above; role TOML may only add a
    // separate custom provider because built-in provider IDs are reserved in config files.
    fs::write(
        &role_path,
        format!(
            "model = \"{REVIEW_MODEL}\"\nrencrow_read_only = true\nmodel_provider = \"{model_provider_id}\"\n\n[rencrow_execution_guard]\ntask_kind = \"read_only\"\n\n[model_providers.mock]\nname = \"mock\"\nbase_url = \"{provider_url}\"\nenv_key = \"PATH\"\nwire_api = \"responses\"\n"
        ),
    )
    .expect("write isolated read-only completion reviewer role");
    config.agent_roles.insert(
        REVIEW_ROLE.into(),
        AgentRoleConfig {
            enabled: None,
            description: Some("Read-only completion reviewer".into()),
            config_file: Some(role_path.as_path().to_path_buf()),
            nickname_candidates: None,
        },
    );
}

async fn mount_delayed_completion_reviewer_response(
    server: &wiremock::MockServer,
    parent_thread_id: String,
    delay: Duration,
) {
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path_regex(".*/responses$"))
        .and(move |request: &wiremock::Request| {
            is_completion_reviewer_request(request, &parent_thread_id)
        })
        .respond_with(
            sse_response(sse(vec![
                ev_response_created("delayed-completion-reviewer-response"),
                ev_assistant_message(
                    "delayed-completion-reviewer-message",
                    "This mock response must arrive only after the host review deadline.",
                ),
                ev_completed("delayed-completion-reviewer-response"),
            ]))
            .set_delay(delay),
        )
        .up_to_n_times(1)
        .mount(server)
        .await;
}

async fn completion_reviewer_requests(
    server: &wiremock::MockServer,
    parent_thread_id: &str,
) -> Result<Vec<wiremock::Request>> {
    let requests = server
        .received_requests()
        .await
        .ok_or_else(|| anyhow::anyhow!("loopback mock server did not return captured requests"))?;
    Ok(requests
        .into_iter()
        .filter(|request| is_completion_reviewer_request(request, parent_thread_id))
        .collect())
}

async fn wait_for_completion_reviewer_requests(
    server: &wiremock::MockServer,
    parent_thread_id: &str,
) -> Vec<wiremock::Request> {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let requests = completion_reviewer_requests(server, parent_thread_id)
                .await
                .expect("read loopback completion reviewer requests");
            if !requests.is_empty() {
                return requests;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("timed out waiting for the real completion reviewer request")
}

async fn persisted_review_ended_event_count(
    codex_home: &std::path::Path,
    reviewer_thread_id: &str,
    reviewer_turn_id: &str,
) -> Result<usize> {
    let path = codex_core::find_thread_path_by_id_str(
        codex_home,
        reviewer_thread_id,
        /*state_db_ctx*/ None,
    )
    .await?
    .ok_or_else(|| anyhow::anyhow!("reviewer rollout is missing for {reviewer_thread_id}"))?;
    Ok(fs::read_to_string(path)?
        .lines()
        .filter_map(|line| codex_rollout::parse_rollout_line(line).ok())
        .filter(|entry| {
            matches!(
                &entry.item,
                RolloutItem::EventMsg(codex_protocol::protocol::EventMsg::TurnAborted(event))
                    if event.turn_id.as_deref() == Some(reviewer_turn_id)
                        && event.reason == TurnAbortReason::ReviewEnded
            )
        })
        .count())
}

fn unix_time_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time should be after the Unix epoch")
        .as_millis()
        .try_into()
        .expect("current Unix timestamp should fit in i64 milliseconds")
}

fn host_completion_snapshots(path: &std::path::Path) -> Result<Vec<Value>> {
    let rollout = fs::read_to_string(path)?;
    Ok(rollout
        .lines()
        .filter_map(|line| codex_rollout::parse_rollout_line(line).ok())
        .filter_map(|entry| match entry.item {
            RolloutItem::ResponseItem(envelope) => envelope
                .metadata
                .and_then(|metadata| metadata.rencrow_completion),
            _ => None,
        })
        .collect())
}

fn latest_host_completion_snapshot(path: &std::path::Path) -> Result<Value> {
    host_completion_snapshots(path)?
        .into_iter()
        .rev()
        .find(|snapshot| snapshot["state"]["units"].is_array())
        .ok_or_else(|| anyhow::anyhow!("host completion snapshot with units is missing"))
}

fn completion_unit<'a>(snapshot: &'a Value, id: &str) -> &'a Value {
    snapshot["state"]["units"]
        .as_array()
        .and_then(|units| units.iter().find(|unit| unit["id"] == id))
        .unwrap_or_else(|| panic!("completion unit {id} is missing"))
}

async fn assert_persisted_reviewer_identity(
    codex_home: &std::path::Path,
    thread_id: &str,
    turn_id: &str,
) -> Result<()> {
    let path =
        codex_core::find_thread_path_by_id_str(codex_home, thread_id, /*state_db_ctx*/ None)
            .await?
            .ok_or_else(|| anyhow::anyhow!("reviewer rollout is missing for thread {thread_id}"))?;
    let rollout = fs::read_to_string(path)?;
    let mut agent_role = None;
    let mut context = None;
    for line in rollout.lines() {
        let Ok(entry) = codex_rollout::parse_rollout_line(line) else {
            continue;
        };
        match entry.item {
            RolloutItem::SessionMeta(meta) => agent_role = meta.meta.agent_role,
            RolloutItem::TurnContext(item) if item.turn_id.as_deref() == Some(turn_id) => {
                context = Some(item);
            }
            _ => {}
        }
    }
    assert_eq!(agent_role.as_deref(), Some(REVIEW_ROLE));
    assert_eq!(
        context.expect("reviewer turn context is persisted").model,
        REVIEW_MODEL
    );
    Ok(())
}

fn tool_output_debug(path: &std::path::Path, target_call_id: &str) -> Result<Option<String>> {
    Ok(fs::read_to_string(path)?
        .lines()
        .filter_map(|line| codex_rollout::parse_rollout_line(line).ok())
        .find_map(|entry| match entry.item {
            RolloutItem::ResponseItem(envelope) => match envelope.item {
                ResponseItem::FunctionCallOutput {
                    call_id: Some(call_id),
                    output,
                    ..
                } if call_id == target_call_id => Some(format!("{output:?}")),
                ResponseItem::CustomToolCallOutput {
                    call_id, output, ..
                } if call_id == target_call_id => Some(format!("{output:?}")),
                _ => None,
            },
            _ => None,
        }))
}

fn tool_output_text(path: &std::path::Path, target_call_id: &str) -> Result<Option<String>> {
    Ok(fs::read_to_string(path)?
        .lines()
        .filter_map(|line| codex_rollout::parse_rollout_line(line).ok())
        .find_map(|entry| match entry.item {
            RolloutItem::ResponseItem(envelope) => match envelope.item {
                ResponseItem::FunctionCallOutput {
                    call_id: Some(call_id),
                    output,
                    ..
                } if call_id == target_call_id => output.body.to_text(),
                _ => None,
            },
            _ => None,
        }))
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_task_receipts_satisfy_then_stale_artifacts_reopen_without_clock_reset_on_reordering()
-> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let fixture = test_codex()
        .with_config(move |config| {
            config.rencrow_compaction = true;
            config.rencrow_autonomous_completion = true;
            config.rencrow_tool_allowlist = Some(vec![
                "exec_command".into(),
                "apply_patch".into(),
                "task_context_boundary".into(),
            ]);
        })
        .build(&server)
        .await?;
    let artifact_a = fixture.cwd_path().join("artifact-a.txt");
    let artifact_b = fixture.cwd_path().join("artifact-b.txt");
    let artifact_c = fixture.cwd_path().join("artifact-c.txt");
    let artifact_d = fixture.cwd_path().join("artifact-d.txt");
    let test_fixture = fixture.cwd_path().join("tests/test_fixture.py");
    fs::create_dir_all(test_fixture.parent().expect("test fixture parent"))?;
    fs::write(
        &test_fixture,
        "import unittest\nclass Fixture(unittest.TestCase):\n def test_fixture(self): self.assertEqual(1, 2)\n",
    )?;
    let unrelated_test = fixture.cwd_path().join("tests/test_unrelated.py");
    fs::write(
        &unrelated_test,
        "import unittest\nclass Unrelated(unittest.TestCase):\n def test_unrelated(self): self.assertTrue(True)\n",
    )?;
    fs::write(&artifact_a, "before A\n")?;
    fs::write(&artifact_b, "verified B\n")?;
    fs::write(&artifact_c, "verified C\n")?;
    fs::write(&artifact_d, "verified D\n")?;
    let relative_a = artifact_a
        .strip_prefix(fixture.cwd_path())?
        .to_string_lossy();
    let relative_b = artifact_b
        .strip_prefix(fixture.cwd_path())?
        .to_string_lossy();
    let relative_c = artifact_c
        .strip_prefix(fixture.cwd_path())?
        .to_string_lossy();
    let relative_d = artifact_d
        .strip_prefix(fixture.cwd_path())?
        .to_string_lossy();
    let relative_test_fixture = test_fixture
        .strip_prefix(fixture.cwd_path())?
        .to_string_lossy();
    let relative_unrelated_test = unrelated_test
        .strip_prefix(fixture.cwd_path())?
        .to_string_lossy();

    let begin_root = json!({
        "action":"begin", "root_task_id":"root", "task_id":"root", "task_revision":1,
        "completion_state":open_contract()
    });
    let begin_a =
        json!({"action":"begin", "root_task_id":"root", "task_id":"task-a", "task_revision":1});
    let patch_a = "*** Begin Patch\n*** Update File: artifact-a.txt\n@@\n-before A\n+verified A\n*** End Patch";
    let check_a_failed = json!({"cmd":"python3 -B -m unittest discover -s tests -p test_fixture.py -v", "task_evidence":{
        "task_id":"task-a", "task_revision":1,
        "artifact_paths":[relative_a, relative_test_fixture, relative_unrelated_test], "check_kind":"verification"
    }});
    let check_a_unrelated_success = json!({"cmd":"python3 -B -m unittest discover -s tests -p test_unrelated.py -v", "task_evidence":{
        "task_id":"task-a", "task_revision":1,
        "artifact_paths":[relative_a, relative_test_fixture, relative_unrelated_test], "check_kind":"verification"
    }});
    let patch_test_fixture = "*** Begin Patch\n*** Update File: tests/test_fixture.py\n@@\n- def test_fixture(self): self.assertEqual(1, 2)\n+ def test_fixture(self): self.assertEqual(1, 1)\n*** End Patch";
    let check_a_recovery = json!({"cmd":"python3 -B -m unittest discover -s tests -p test_fixture.py --verbose", "task_evidence":{
        "task_id":"task-a", "task_revision":1,
        "artifact_paths":[relative_a, relative_test_fixture, relative_unrelated_test], "check_kind":"verification"
    }});
    let accept_a = json!({"action":"accept", "result":"Task A artifact verified"});
    let begin_b =
        json!({"action":"begin", "root_task_id":"root", "task_id":"task-b", "task_revision":1});
    let check_b = json!({"cmd":"python3 -B -m unittest discover -s tests -p test_unrelated.py -v", "task_evidence":{
        "task_id":"task-b", "task_revision":1,
        "artifact_paths":[relative_b, relative_unrelated_test], "check_kind":"verification"
    }});
    let accept_b = json!({"action":"accept", "result":"Task B artifact verified"});
    let begin_c =
        json!({"action":"begin", "root_task_id":"root", "task_id":"task-c", "task_revision":1});
    let check_c = json!({"cmd":"python3 -B -m unittest discover -s tests -p test_unrelated.py -v", "task_evidence":{
        "task_id":"task-c", "task_revision":1,
        "artifact_paths":[relative_c, relative_unrelated_test], "check_kind":"verification"
    }});
    let accept_c = json!({"action":"accept", "result":"Task C artifact verified"});
    let begin_d =
        json!({"action":"begin", "root_task_id":"root", "task_id":"task-d", "task_revision":1});
    let check_d = json!({"cmd":"python3 -B -m unittest discover -s tests -p test_unrelated.py -v", "task_evidence":{
        "task_id":"task-d", "task_revision":1,
        "artifact_paths":[relative_d, relative_unrelated_test], "check_kind":"verification"
    }});
    let accept_d = json!({"action":"accept", "result":"Task D artifact verified"});
    let plan_review = json!({
        "action":"plan_review", "root_task_id":"root",
        "previous_plan":"Continue with verified evidence",
        "revised_plan":"Reverify A after artifact changes",
        "reason":"Two accepted task results arrived without new acceptance-condition progress."
    });
    let evidence_a = evidence("task-a");
    let evidence_b = evidence("task-b");
    let satisfied = claim(
        "satisfied",
        vec![evidence_a.clone(), evidence_b.clone()],
        "done",
    );
    let reordered = claim(
        "satisfied",
        vec![evidence_b.clone(), evidence_a.clone()],
        "done",
    );
    let mut stale_claim = claim("satisfied", vec![evidence_a, evidence_b], "ready");
    stale_claim["next_action"] = "Reverify A after artifact changes".into();
    let mutate_a = json!({"cmd":"printf 'changed after verification\\n' > artifact-a.txt"});
    let begin_a_reverify =
        json!({"action":"begin", "root_task_id":"root", "task_id":"task-a", "task_revision":1});
    let check_a_reverify = json!({"cmd":"python3 -B -m unittest discover -s tests -p test_fixture.py --verbose", "task_evidence":{
        "task_id":"task-a", "task_revision":1,
        "artifact_paths":[relative_a, relative_test_fixture, relative_unrelated_test], "check_kind":"verification"
    }});
    let accept_a_reverify = json!({
        "action":"accept", "result":"Task A reverified after its accepted bytes changed",
        "satisfied_conditions":["AC-1"]
    });
    let mutate_a_again = json!({"cmd":"printf 'changed again after reaccept\\n' > artifact-a.txt"});

    let responses = mount_sse_sequence(
        &server,
        vec![
            function_response("begin-root", "task_context_boundary", &begin_root),
            function_response("begin-a", "task_context_boundary", &begin_a),
            sse(vec![
                ev_response_created("patch-a"),
                ev_apply_patch_custom_tool_call("patch-a-call", patch_a),
                ev_completed("patch-a"),
            ]),
            function_response("check-a-failed", "exec_command", &check_a_failed),
            function_response(
                "check-a-unrelated-success",
                "exec_command",
                &check_a_unrelated_success,
            ),
            function_response("accept-a-rejected", "task_context_boundary", &accept_a),
            sse(vec![
                ev_response_created("repair-test-fixture"),
                ev_apply_patch_custom_tool_call("repair-test-fixture-call", patch_test_fixture),
                ev_completed("repair-test-fixture"),
            ]),
            function_response("check-a-recovery", "exec_command", &check_a_recovery),
            function_response("accept-a", "task_context_boundary", &accept_a),
            function_response("begin-b", "task_context_boundary", &begin_b),
            function_response("check-b", "exec_command", &check_b),
            function_response("accept-b", "task_context_boundary", &accept_b),
            function_response(
                "satisfy",
                "task_context_boundary",
                &json!({"action":"update", "root_task_id":"root", "completion_state":satisfied}),
            ),
            function_response("clock-gap", "exec_command", &json!({"cmd":"sleep 2"})),
            function_response(
                "reorder",
                "task_context_boundary",
                &json!({"action":"update", "root_task_id":"root", "completion_state":reordered}),
            ),
            function_response("begin-c", "task_context_boundary", &begin_c),
            function_response("check-c", "exec_command", &check_c),
            function_response("accept-c", "task_context_boundary", &accept_c),
            function_response("begin-d", "task_context_boundary", &begin_d),
            function_response("check-d", "exec_command", &check_d),
            function_response("accept-d", "task_context_boundary", &accept_d),
            function_response("plan-review", "task_context_boundary", &plan_review),
            function_response("mutate-a", "exec_command", &mutate_a),
            function_response(
                "begin-a-reverify",
                "task_context_boundary",
                &begin_a_reverify,
            ),
            function_response("check-a-reverify", "exec_command", &check_a_reverify),
            function_response(
                "accept-a-reverify",
                "task_context_boundary",
                &accept_a_reverify,
            ),
            function_response("mutate-a-again", "exec_command", &mutate_a_again),
            function_response(
                "stale-update",
                "task_context_boundary",
                &json!({"action":"update", "root_task_id":"root", "completion_state":stale_claim}),
            ),
            sse(vec![
                ev_response_created("done"),
                ev_assistant_message("done-message", "Stale evidence reopened AC-1."),
                ev_completed("done"),
            ]),
        ],
    )
    .await;

    fixture
        .submit_turn_with_permission_profile(
            "Implement and verify A and B, then preserve the acceptance evidence.",
            PermissionProfile::Disabled,
        )
        .await?;
    assert_eq!(
        fs::read_to_string(&artifact_a)?,
        "changed again after reaccept\n"
    );
    let rollout_path = fixture.codex.rollout_path().expect("rollout path");
    let reverify_begin_output =
        tool_output_text(&rollout_path, "begin-a-reverify")?.expect("same-task reverify begin");
    let reverify_check_output =
        tool_output_text(&rollout_path, "check-a-reverify")?.expect("same-task reverify check");
    let reverify_accept_output = tool_output_text(&rollout_path, "accept-a-reverify")?
        .expect("same-task public accept output");
    assert!(reverify_begin_output.contains("Task scope begun"));
    assert!(
        reverify_check_output.contains("Process exited with code 0")
            && reverify_check_output.contains("OK"),
        "same-task verification did not return a successful unified-exec result: {reverify_check_output}"
    );
    assert!(
        reverify_accept_output.contains("Task boundary prepared"),
        "same-task accept did not prepare its host boundary: {reverify_accept_output}"
    );
    let rejected_accept = tool_output_debug(&rollout_path, "accept-a-rejected")?
        .expect("first acceptance attempt output");
    assert!(
        rejected_accept.contains("unresolved verification obligation"),
        "unrelated success must not resolve the first failure: {rejected_accept}"
    );
    let snapshots = host_completion_snapshots(&rollout_path)?;
    let evidence_ids = |snapshot: &Value| {
        let mut ids = snapshot["state"]["conditions"][0]["evidence"]
            .as_array()
            .expect("condition evidence array")
            .iter()
            .map(|evidence| serde_json::to_string(evidence).unwrap())
            .collect::<Vec<_>>();
        ids.sort();
        ids
    };
    let has_initial_receipts = |snapshot: &Value| {
        snapshot["state"]["conditions"][0]["evidence"]
            .as_array()
            .is_some_and(|evidence| {
                evidence.len() == 2
                    && evidence.iter().any(|item| {
                        item["artifact_ref"].as_str().is_some_and(|reference| {
                            reference.starts_with("task-check:check-a-recovery:")
                        })
                    })
                    && evidence.iter().any(|item| {
                        item["artifact_ref"]
                            .as_str()
                            .is_some_and(|reference| reference.starts_with("task-check:check-b:"))
                    })
            })
    };
    let satisfied_snapshots = snapshots
        .iter()
        .filter(|snapshot| {
            snapshot["state"]["conditions"][0]["status"] == "satisfied"
                && has_initial_receipts(snapshot)
        })
        .collect::<Vec<_>>();
    assert!(
        satisfied_snapshots.len() >= 2,
        "both satisfied updates must persist host snapshots"
    );
    let first_progress = &satisfied_snapshots[0]["state"]["product_progress_at"];
    let reordered_progress =
        &satisfied_snapshots[satisfied_snapshots.len() - 1]["state"]["product_progress_at"];
    assert_eq!(
        first_progress, reordered_progress,
        "reordering identical accepted receipt refs cannot reset product progress"
    );
    assert_eq!(
        satisfied_snapshots[0]["state"]["units_completed_since_review"], 0,
        "new acceptance progress resets the no-progress review counter"
    );
    let plan_review_snapshot = snapshots
        .iter()
        .find(|snapshot| {
            snapshot["state"]["plan_reviews"]
                .as_array()
                .is_some_and(|reviews| {
                    reviews
                        .iter()
                        .any(|review| review["revised_plan"] == "Reverify A after artifact changes")
                })
        })
        .expect("host snapshot records the exact plan-review event");
    assert_eq!(
        plan_review_snapshot["state"]["units_completed_since_review"], 0,
        "plan review resets the counter in its own persisted event"
    );
    let latest_reaccepted = snapshots
        .iter()
        .rev()
        .find(|snapshot| {
            snapshot["state"]["conditions"][0]["status"] == "satisfied"
                && snapshot["state"]["conditions"][0]["evidence"]
                    .as_array()
                    .is_some_and(|evidence| {
                        evidence.iter().any(|item| {
                            item["artifact_ref"].as_str().is_some_and(|reference| {
                                reference.starts_with("task-check:check-a-reverify:")
                            })
                        })
                    })
        })
        .expect("public same-task reaccept persists a fresh satisfied condition");
    let reaccepted_evidence = &latest_reaccepted["state"]["conditions"][0]["evidence"];
    let reaccepted_a_evidence = reaccepted_evidence
        .as_array()
        .and_then(|evidence| {
            evidence
                .iter()
                .find(|item| item["task_id"] == "task-a" && item["task_revision"] == 1)
        })
        .expect("fresh acceptance retains task-a at the same revision");
    let reaccepted_a_ref = reaccepted_a_evidence["artifact_ref"]
        .as_str()
        .expect("reaccepted task-a artifact ref");
    assert!(reaccepted_a_ref.starts_with("task-check:check-a-reverify:"));
    let initial_a_ref = satisfied_snapshots
        .last()
        .and_then(|snapshot| snapshot["state"]["conditions"][0]["evidence"].as_array())
        .and_then(|evidence| evidence.iter().find(|item| item["task_id"] == "task-a"))
        .and_then(|item| item["artifact_ref"].as_str())
        .expect("prior satisfied state contains the original task-a proof");
    assert_ne!(
        reaccepted_a_ref, initial_a_ref,
        "same-task same-revision accept binds a fresh receipt after the artifact bytes changed"
    );
    let initial_a_fingerprint =
        codex_history::task_context::task_check_artifact_fingerprint(initial_a_ref)
            .expect("prior accepted task-a receipt has an artifact fingerprint");
    let reaccepted_a_fingerprint =
        codex_history::task_context::task_check_artifact_fingerprint(reaccepted_a_ref)
            .expect("reaccepted task-a receipt has an artifact fingerprint");
    assert_ne!(
        initial_a_fingerprint, reaccepted_a_fingerprint,
        "same task ID and revision have different artifact bytes after reverification"
    );
    assert_eq!(
        latest_reaccepted["state"]["units_completed_since_review"], 1,
        "the public subtask reaccept increments the post-review counter"
    );
    let latest = snapshots.last().expect("stale update persisted");
    assert_eq!(latest["state"]["conditions"][0]["status"], "open");
    assert_eq!(
        latest["state"]["units_completed_since_review"], 1,
        "stale reopening after reaccept preserves the post-review accepted-unit count"
    );
    assert_eq!(
        evidence_ids(latest),
        evidence_ids(latest_reaccepted),
        "stale reopening retains the previous proof set independent of evidence ordering"
    );
    assert!(
        latest["state"]["conditions"][0]["unmet_reason"]
            .as_str()
            .unwrap()
            .contains("artifact")
    );
    let tool_results = fs::read_to_string(&rollout_path)?
        .lines()
        .filter_map(|line| codex_rollout::parse_rollout_line(line).ok())
        .filter_map(|entry| match entry.item {
            RolloutItem::ResponseItem(envelope) => match envelope.item {
                ResponseItem::FunctionCallOutput {
                    call_id: Some(call_id),
                    output,
                    ..
                } if ["accept-c", "accept-d", "plan-review", "stale-update"]
                    .contains(&call_id.as_str()) =>
                {
                    Some((call_id, format!("{output:?}")))
                }
                ResponseItem::CustomToolCallOutput {
                    call_id, output, ..
                } if ["accept-c", "accept-d", "plan-review", "stale-update"]
                    .contains(&call_id.as_str()) =>
                {
                    Some((call_id, format!("{output:?}")))
                }
                _ => None,
            },
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        latest["state"]["plan_reviews"].as_array().unwrap().len(),
        1,
        "plan review was not recorded; relevant tool outputs={tool_results:?}"
    );

    let requests = responses.requests();
    assert!(requests.len() >= 19);
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_evidence_recovery_accept_completes_final_guard() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let fixture = test_codex()
        .with_config(|config| {
            config.rencrow_compaction = true;
            config.rencrow_autonomous_completion = true;
            config.rencrow_execution_guard = Some(RencrowExecutionGuard {
                require_check: true,
                ..Default::default()
            });
            config.rencrow_tool_allowlist = Some(vec![
                "exec_command".into(),
                "apply_patch".into(),
                "task_context_boundary".into(),
            ]);
        })
        .build(&server)
        .await?;
    let test_fixture = fixture.cwd_path().join("tests/test_fixture.py");
    fs::create_dir_all(test_fixture.parent().expect("test fixture parent"))?;
    fs::write(
        &test_fixture,
        "import unittest\nclass Fixture(unittest.TestCase):\n def test_fixture(self): self.assertEqual(1, 2)\n",
    )?;
    let relative_test_fixture = test_fixture
        .strip_prefix(fixture.cwd_path())?
        .to_string_lossy();

    let begin_root = json!({
        "action":"begin", "root_task_id":"root", "task_id":"root", "task_revision":1,
        "completion_state":open_contract()
    });
    let begin_unit =
        json!({"action":"begin", "root_task_id":"root", "task_id":"unit-a", "task_revision":1});
    let check_failed = json!({"cmd":"python3 -B -m unittest discover -s tests -p test_fixture.py -v", "task_evidence":{
        "task_id":"unit-a", "task_revision":1,
        "artifact_paths":[relative_test_fixture], "check_kind":"verification"
    }});
    let patch_fixture = "*** Begin Patch\n*** Update File: tests/test_fixture.py\n@@\n- def test_fixture(self): self.assertEqual(1, 2)\n+ def test_fixture(self): self.assertEqual(1, 1)\n*** End Patch";
    let check_recovered = json!({"cmd":"python3 -B -m unittest discover -s tests -p test_fixture.py --verbose", "task_evidence":{
        "task_id":"unit-a", "task_revision":1,
        "artifact_paths":[relative_test_fixture], "check_kind":"verification"
    }});
    let accept = json!({
        "action":"accept", "result":"Unit A is verified", "satisfied_conditions":["AC-1"]
    });

    mount_sse_sequence(
        &server,
        vec![
            function_response("begin-root", "task_context_boundary", &begin_root),
            function_response("begin-unit-a", "task_context_boundary", &begin_unit),
            function_response("check-failed", "exec_command", &check_failed),
            sse(vec![
                ev_response_created("repair-fixture"),
                ev_apply_patch_custom_tool_call("repair-fixture-call", patch_fixture),
                ev_completed("repair-fixture"),
            ]),
            function_response("check-recovered", "exec_command", &check_recovered),
            function_response("accept-unit-a", "task_context_boundary", &accept),
            sse(vec![
                ev_response_created("completed"),
                ev_assistant_message("completed-message", "Unit A is verified and complete."),
                ev_completed("completed"),
            ]),
        ],
    )
    .await;

    fixture
        .submit_turn_with_permission_profile(
            "Verify and accept Unit A, then finish the task.",
            PermissionProfile::Disabled,
        )
        .await?;
    let final_status = fixture.codex.agent_status().await;
    assert!(
        matches!(&final_status, AgentStatus::Completed(Some(_))),
        "the final host completion guard did not complete the integrated turn: {final_status:?}"
    );
    let rollout_path = fixture.codex.rollout_path().expect("rollout path");
    let accepted = tool_output_debug(&rollout_path, "accept-unit-a")?.expect("acceptance output");
    assert!(
        accepted.contains("Task boundary prepared"),
        "the recovered same-identity receipt was not accepted: {accepted}"
    );
    let committed_snapshots = host_completion_snapshots(&rollout_path)?;
    let latest_committed_unit = committed_snapshots
        .iter()
        .rev()
        .find(|snapshot| {
            snapshot["state"]["units"].as_array().is_some_and(|units| {
                units
                    .iter()
                    .any(|unit| unit["id"] == "unit-a" && unit["status"] == "done")
            })
        })
        .expect("host snapshot after accepted task boundary");
    assert!(
        latest_committed_unit.get("active_scope").is_some(),
        "completion snapshot must explicitly retain the active_scope field"
    );
    assert_eq!(
        latest_committed_unit["active_scope"],
        Value::Null,
        "the latest committed boundary snapshot must persist the cleared task scope"
    );

    // Change a verified artifact after completion. The ordinary turn-end path
    // must reopen the affected condition before the autonomous continuation
    // can add and accept a new-ID verification unit.
    fs::write(
        &test_fixture,
        "# changed after completion\nimport unittest\nclass Fixture(unittest.TestCase):\n def test_fixture(self): self.assertEqual(1, 1)\n",
    )?;
    let completed_snapshots = host_completion_snapshots(&rollout_path)?;
    let completed_state = completed_snapshots
        .iter()
        .rev()
        .find(|snapshot| snapshot["state"]["conditions"][0]["status"] == "satisfied")
        .expect("completed host snapshot")
        .get("state")
        .expect("completion state")
        .clone();
    let original_evidence = completed_state["conditions"][0]["evidence"].clone();
    let original_revision = completed_state["contract_revision"].clone();

    let mut revalidation_state = completed_state;
    revalidation_state["conditions"][0]["status"] = "open".into();
    revalidation_state["conditions"][0]["unmet_reason"] =
        "The previously verified artifact changed after completion.".into();
    revalidation_state["next_action"] =
        "Revalidate AC-1/task unit-a revision 1 using a new execution unit ID: update, begin, verify, and accept."
            .into();
    revalidation_state["units"]
        .as_array_mut()
        .unwrap()
        .push(json!({
            "id":"unit-b", "purpose":"Revalidate AC-1 after the artifact changed",
            "status":"ready", "condition_refs":["AC-1"], "depends_on":["unit-a"],
            "evidence":[], "task_refs":[]
        }));
    revalidation_state["current_unit"] = "unit-b".into();

    let begin_revalidation = json!({
        "action":"begin", "root_task_id":"root", "task_id":"unit-b", "task_revision":1
    });
    let recheck = json!({
        "cmd":"python3 -B -m unittest discover -s tests -p test_fixture.py --verbose",
        "task_evidence":{
            "task_id":"unit-b", "task_revision":1,
            "artifact_paths":[relative_test_fixture], "check_kind":"verification"
        }
    });
    let accept_revalidation = json!({
        "action":"accept", "result":"Unit B reverified the changed artifact", "satisfied_conditions":["AC-1"]
    });
    mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("artifact-changed-turn"),
                ev_assistant_message(
                    "artifact-changed-message",
                    "The workspace artifact changed; I will revalidate it.",
                ),
                ev_completed("artifact-changed-turn"),
            ]),
            function_response(
                "revalidation-update",
                "task_context_boundary",
                &json!({
                    "action":"update", "root_task_id":"root",
                    "completion_state":revalidation_state
                }),
            ),
            function_response("begin-unit-b", "task_context_boundary", &begin_revalidation),
            function_response("check-unit-b", "exec_command", &recheck),
            function_response(
                "accept-unit-b",
                "task_context_boundary",
                &accept_revalidation,
            ),
            sse(vec![
                ev_response_created("reverified-completed"),
                ev_assistant_message(
                    "reverified-completed-message",
                    "AC-1 is reverified and complete.",
                ),
                ev_completed("reverified-completed"),
            ]),
        ],
    )
    .await;
    fixture
        .submit_turn_with_permission_profile(
            "The accepted artifact changed after completion. Revalidate it using a new unit ID and complete the condition.",
            PermissionProfile::Disabled,
        )
        .await?;

    let snapshots = host_completion_snapshots(&rollout_path)?;
    let reopened = snapshots
        .iter()
        .find(|snapshot| {
            snapshot["state"]["conditions"][0]["status"] == "open"
                && snapshot["state"]["conditions"][0]["unmet_reason"]
                    .as_str()
                    .is_some_and(|reason| reason.contains("artifact"))
        })
        .expect("turn-end reconciliation persisted an open condition");
    assert_eq!(
        reopened["state"]["conditions"][0]["evidence"], original_evidence,
        "reopening retains the original accepted proof"
    );
    assert_eq!(reopened["state"]["contract_revision"], original_revision);
    assert_eq!(reopened["state"]["units"][0]["status"], "done");
    assert!(
        reopened["state"]["next_action"]
            .as_str()
            .is_some_and(|action| action.contains("AC-1") && action.contains("unit-a")),
        "reconciliation should name the affected condition and original task"
    );
    let reaccepted = snapshots
        .iter()
        .rev()
        .find(|snapshot| snapshot["state"]["conditions"][0]["status"] == "satisfied")
        .expect("new verification re-satisfied AC-1");
    assert_eq!(reaccepted["state"]["contract_revision"], original_revision);
    assert_eq!(reaccepted["state"]["units"][0]["status"], "done");
    assert_eq!(reaccepted["state"]["units"][1]["id"], "unit-b");
    assert_eq!(reaccepted["state"]["units"][1]["status"], "done");
    let check_output =
        tool_output_debug(&rollout_path, "check-unit-b")?.expect("new-ID verification output");
    assert!(
        check_output.contains("OK"),
        "reverification failed: {check_output}"
    );
    let accepted_output =
        tool_output_debug(&rollout_path, "accept-unit-b")?.expect("new-ID acceptance output");
    assert!(
        accepted_output.contains("Task boundary prepared"),
        "new-ID receipt was not accepted: {accepted_output}"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn planned_review_runs_through_host_spawn_wait_same_child_followup_and_adoption() -> Result<()>
{
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let provider_url = format!("{}/v1", server.uri());
    let fixture = test_codex()
        .with_config(move |config| {
            config.rencrow_compaction = true;
            config.rencrow_autonomous_completion = true;
            config.rencrow_tool_allowlist = Some(vec![
                "exec_command".into(),
                "task_context_boundary".into(),
                "multi_agent_v1::send_input".into(),
                "multi_agent_v1::wait_agent".into(),
                "multi_agent_v1::close_agent".into(),
            ]);
            config.features.enable(Feature::Collab).unwrap();
            let model_provider_id = config.model_provider_id.clone();
            config.model_providers.insert(
                model_provider_id.clone(),
                config.model_provider.clone(),
            );
            let role_path = config.codex_home.join("completion-reviewer.toml");
            fs::write(
                &role_path,
                format!(
                    "model = \"{REVIEW_MODEL}\"\nrencrow_read_only = true\nmodel_provider = \"{model_provider_id}\"\n\n[rencrow_execution_guard]\ntask_kind = \"read_only\"\n\n[model_providers.mock]\nname = \"mock\"\nbase_url = \"{provider_url}\"\nenv_key = \"PATH\"\nwire_api = \"responses\"\n"
                ),
            )
            .unwrap();
            config.agent_roles.insert(
                REVIEW_ROLE.into(),
                AgentRoleConfig {
                    enabled: None,
                    description: Some("Read-only completion reviewer".into()),
                    config_file: Some(role_path.to_path_buf()),
                    nickname_candidates: None,
                },
            );
        })
        .build(&server)
        .await?;
    let parent_thread_id = fixture.session_configured.thread_id.to_string();
    let artifact = fixture.cwd_path().join("review-artifact.txt");
    fs::write(&artifact, "accepted artifact\n")?;
    write_artifact_content_test(
        fixture.cwd_path(),
        "test_review_artifact.py",
        "review-artifact.txt",
    )?;
    let relative_artifact = artifact
        .strip_prefix(fixture.cwd_path())?
        .to_string_lossy()
        .to_string();
    let test_artifact = fixture.cwd_path().join("tests/test_review_artifact.py");
    let relative_test_artifact = test_artifact
        .strip_prefix(fixture.cwd_path())?
        .to_string_lossy()
        .to_string();

    let review_contract = json!({
        "contract_revision":1,
        "conditions":[{
            "id":"AC-1", "revision":1, "result":"Task A is correct", "scope":"CLI",
            "verification":"Inspect the accepted artifact", "acceptance_criteria":"Expected content is present",
            "status":"open", "evidence":[], "unmet_reason":null
        }],
        "units":[
            {"id":"task-a", "purpose":"Create the reviewed artifact", "status":"ready", "condition_refs":["AC-1"], "depends_on":[], "evidence":[], "task_refs":[]},
            {"id":"review-a", "purpose":"Review task A against AC-1", "status":"planned", "kind":"review", "condition_refs":["AC-1"], "depends_on":["task-a"], "evidence":[], "task_refs":[], "review_target":{"task_id":"task-a","task_revision":1}},
            {"id":"task-b", "purpose":"Continue after review A", "status":"planned", "condition_refs":["AC-1"], "depends_on":["review-a"], "evidence":[], "task_refs":[]},
            {"id":"review-b", "purpose":"Review task B against AC-1", "status":"planned", "kind":"review", "condition_refs":["AC-1"], "depends_on":["task-b"], "evidence":[], "task_refs":[], "review_target":{"task_id":"task-b","task_revision":1}}
        ],
        "current_unit":"task-a", "failed_approaches":[], "blocker":null, "next_action":"Begin task A",
        "product_progress_at":null, "runtime_recovery_at":null, "activity_at":null
    });
    mount_parent_sse_sequence(
        &server,
        &parent_thread_id,
        vec![
            function_response(
                "review-begin-root",
                "task_context_boundary",
                &json!({"action":"begin","root_task_id":"root","task_id":"root","task_revision":1,"completion_state":review_contract}),
            ),
            sse(vec![ev_response_created("review-root-begun"), ev_assistant_message("review-root-begun-message", "Plan registered."), ev_completed("review-root-begun")]),
        ],
    )
    .await;
    fixture
        .submit_turn("Register the finite A, review A, B, review B plan.")
        .await?;

    mount_parent_sse_sequence(
        &server,
        &parent_thread_id,
        vec![
            function_response("review-begin-a", "task_context_boundary", &json!({"action":"begin","root_task_id":"root","task_id":"task-a","task_revision":1})),
            function_response("review-check-a", "exec_command", &json!({"cmd":"python3 -B -m unittest discover -s tests -p test_review_artifact.py -v","task_evidence":{"task_id":"task-a","task_revision":1,"artifact_paths":[relative_artifact],"check_kind":"verification"}})),
            function_response("review-accept-a", "task_context_boundary", &json!({"action":"accept","result":"Task A artifact verified","satisfied_conditions":["AC-1"]})),
            sse(vec![ev_response_created("review-a-accepted"), ev_assistant_message("review-a-accepted-message", "Task A is accepted; required review A remains."), ev_completed("review-a-accepted")]),
        ],
    )
    .await;
    fixture.submit_turn("Verify and accept task A.").await?;
    let rollout_path = fixture.codex.rollout_path().expect("rollout path");
    let begin_output =
        tool_output_debug(&rollout_path, "review-begin-a")?.expect("task A begin output");
    let check_output =
        tool_output_debug(&rollout_path, "review-check-a")?.expect("task A check output");
    let accept_output =
        tool_output_debug(&rollout_path, "review-accept-a")?.expect("task A accept output");
    assert!(!begin_output.is_empty());
    assert!(
        check_output.contains("OK"),
        "task A verification failed: {check_output}"
    );
    assert!(
        accept_output.contains("Task boundary prepared"),
        "task A acceptance failed: {accept_output}"
    );

    let child_parent_thread_id = parent_thread_id.clone();
    let child_response = mount_sse_sequence_match(
        &server,
        move |request: &wiremock::Request| {
            is_completion_reviewer_request(request, &child_parent_thread_id)
        },
        vec![
            sse(vec![
                ev_response_created("reviewer-turn-1"),
                ev_assistant_message("reviewer-result-1", "The accepted artifact matches AC-1."),
                ev_completed("reviewer-turn-1"),
            ]),
            sse(vec![
                ev_response_created("reviewer-followup-response"),
                ev_assistant_message(
                    "reviewer-followup-result",
                    "The accepted artifact still matches AC-1 after the follow-up review.",
                ),
                ev_completed("reviewer-followup-response"),
            ]),
        ],
    )
    .await;
    mount_parent_sse_sequence(
        &server,
        &parent_thread_id,
        vec![
            function_response(
                "review-action-a",
                "task_context_boundary",
                &json!({"action":"review","target_task_id":"task-a"}),
            ),
            sse(vec![
                ev_response_created("review-action-finished"),
                ev_assistant_message(
                    "review-action-finished-message",
                    "Review launched; collect its result next.",
                ),
                ev_completed("review-action-finished"),
            ]),
        ],
    )
    .await;
    fixture
        .submit_turn("Run the planned read-only review for task A.")
        .await?;
    let review_output =
        tool_output_debug(&rollout_path, "review-action-a")?.expect("review action tool output");
    let launched = host_completion_snapshots(&rollout_path)?
        .into_iter()
        .rev()
        .find(|snapshot| snapshot["state"]["units"].is_array())
        .expect("review launch host snapshot");
    let review_unit = launched["state"]["units"]
        .as_array()
        .unwrap()
        .iter()
        .find(|unit| unit["id"] == "review-a")
        .expect("review unit");
    let reviewer_thread_id = review_unit["review_evidence"]["reviewer_thread_id"]
        .as_str()
        .expect("host-bound reviewer thread id")
        .to_string();
    let reviewer_requests = child_response
        .requests()
        .iter()
        .filter(|request| request.body_json()["client_metadata"]["thread_id"] == reviewer_thread_id)
        .map(core_test_support::responses::ResponsesRequest::body_json)
        .collect::<Vec<_>>();
    let dispatch_diagnostics = if reviewer_requests.len() != 1 {
        Some(
            completion_reviewer_request_diagnostics(
                &server,
                &parent_thread_id,
                &reviewer_thread_id,
            )
            .await,
        )
    } else {
        None
    };
    assert_eq!(
        reviewer_requests.len(),
        1,
        "review launches exactly the host-returned child thread {reviewer_thread_id}; tool_output_present={}; request diagnostics: {dispatch_diagnostics:?}",
        !review_output.is_empty()
    );
    assert_eq!(reviewer_requests[0]["model"], REVIEW_MODEL);
    let reviewer_turn_id = reviewer_requests
        .first()
        .and_then(|body| {
            body["client_metadata"]["turn_id"]
                .as_str()
                .map(str::to_string)
        })
        .expect("reviewer request has a turn generation");
    assert!(!review_output.is_empty(), "review action returned output");
    assert_eq!(review_unit["id"], "review-a");
    assert_eq!(review_unit["review_evidence"]["reviewer_role"], REVIEW_ROLE);
    assert_eq!(
        review_unit["review_evidence"]["reviewer_thread_id"],
        reviewer_thread_id
    );

    mount_parent_sse_sequence(
        &server,
        &parent_thread_id,
        vec![
            sse(vec![
                ev_response_created("review-wait-parent"),
                ev_function_call_with_namespace(
                    "review-wait-call",
                    "multi_agent_v1",
                    "wait_agent",
                    &json!({"targets":[reviewer_thread_id],"timeout_ms":30000}).to_string(),
                ),
                ev_completed("review-wait-parent"),
            ]),
            sse(vec![
                ev_response_created("review-wait-finished"),
                ev_assistant_message(
                    "review-wait-finished-message",
                    "Review result collected; adoption decision is next.",
                ),
                ev_completed("review-wait-finished"),
            ]),
        ],
    )
    .await;
    fixture
        .submit_turn("Collect the result from the returned review child.")
        .await?;

    let collected =
        host_completion_snapshots(&fixture.codex.rollout_path().expect("rollout path"))?
            .into_iter()
            .rev()
            .find(|snapshot| snapshot["state"]["units"].is_array())
            .expect("host snapshot after wait_agent collection");
    let collected_review = collected["state"]["units"]
        .as_array()
        .unwrap()
        .iter()
        .find(|unit| unit["id"] == "review-a")
        .unwrap();
    assert_eq!(collected_review["status"], "running");
    assert_eq!(
        collected_review["review_evidence"]["reviewer_turn_id"],
        reviewer_turn_id
    );
    assert!(collected_review["review_evidence"]["wait_result_ref"].is_string());
    assert_eq!(
        collected_review["review_evidence"]["wait_call_id"],
        "review-wait-call"
    );
    let wait_output =
        tool_output_debug(&rollout_path, "review-wait-call")?.expect("native V1 wait_agent output");
    assert!(wait_output.contains(&reviewer_thread_id));
    assert!(
        wait_output.contains("The accepted artifact matches AC-1."),
        "the parent wait must collect the child review reply: {wait_output}"
    );
    assert_persisted_reviewer_identity(
        fixture.codex_home_path(),
        &reviewer_thread_id,
        &reviewer_turn_id,
    )
    .await?;

    mount_multi_agent_turn(
        &server,
        &parent_thread_id,
        "review-send-input-followup",
        "send_input",
        &json!({
            "target":reviewer_thread_id,
            "message":"Please recheck the same accepted artifact against AC-1 and return a fresh review result."
        }),
        "A fresh reviewer turn was submitted to the same completed child.",
    )
    .await;
    fixture
        .submit_turn("Send the completed reviewer child one follow-up request.")
        .await?;
    let send_input_json = tool_output_text(&rollout_path, "review-send-input-followup")?
        .expect("native send_input follow-up output");
    let send_input_result: Value = serde_json::from_str(&send_input_json)?;
    let submission_id = send_input_result["submission_id"]
        .as_str()
        .expect("started send_input exposes its actual submission id");
    let followup_requests = child_response.requests();
    let reviewer_requests = followup_requests
        .iter()
        .filter(|request| {
            let body = request.body_json();
            body["client_metadata"]["thread_id"] == reviewer_thread_id
                && body["model"] == REVIEW_MODEL
        })
        .collect::<Vec<_>>();
    assert_eq!(
        reviewer_requests.len(),
        2,
        "one initial and one follow-up request use the same reviewer child and model"
    );
    let followup_request = reviewer_requests
        .iter()
        .find(|request| {
            request.body_json()["client_metadata"]["turn_id"].as_str()
                != Some(reviewer_turn_id.as_str())
        })
        .expect("same reviewer child receives the follow-up model request");
    let followup_request_body = followup_request.body_json();
    let followup_turn_id = followup_request_body["client_metadata"]["turn_id"]
        .as_str()
        .expect("follow-up model request identifies its new turn");
    assert_eq!(followup_request_body["model"], REVIEW_MODEL);
    assert!(followup_request.body_contains_text(
        "Please recheck the same accepted artifact against AC-1 and return a fresh review result."
    ));
    assert_eq!(submission_id, followup_turn_id);
    assert_ne!(followup_turn_id, reviewer_turn_id);

    let followup_started = latest_host_completion_snapshot(&rollout_path)?;
    let started_review = completion_unit(&followup_started, "review-a");
    assert_eq!(started_review["status"], "running");
    assert_eq!(
        started_review["review_evidence"]["reviewer_thread_id"],
        reviewer_thread_id
    );
    assert_eq!(
        started_review["review_evidence"]["reviewer_turn_id"],
        followup_turn_id
    );
    assert_eq!(
        started_review["review_state"]["active_attempt"]["attempt_id"],
        "review-send-input-followup"
    );
    assert_ne!(
        started_review["review_state"]["active_attempt"]["attempt_id"],
        reviewer_turn_id
    );
    assert_eq!(
        started_review["review_state"]["active_attempt"]["status"],
        "bound"
    );
    assert!(started_review["review_evidence"]["wait_result_ref"].is_null());
    assert!(started_review["review_evidence"]["wait_call_id"].is_null());

    mount_function_turn(
        &server,
        &parent_thread_id,
        "review-reject-old-adoption",
        "task_context_boundary",
        &json!({
            "action":"adopt_review",
            "unit_id":"review-a",
            "reason":"The first collected review result was superseded by a follow-up."
        }),
        "The prior generation cannot be adopted while follow-up work remains uncollected.",
    )
    .await;
    fixture
        .submit_turn("Try to adopt the prior review while its follow-up child is still active.")
        .await?;
    let old_adoption_output = tool_output_debug(&rollout_path, "review-reject-old-adoption")?
        .expect("old adoption response");
    assert!(
        !old_adoption_output.is_empty(),
        "the prior-generation adoption action must return a host response: {old_adoption_output}"
    );
    let still_pending = latest_host_completion_snapshot(&rollout_path)?;
    assert_eq!(
        completion_unit(&still_pending, "review-a")["status"],
        "running"
    );
    assert!(completion_unit(&still_pending, "review-a")["review_state"]["active_attempt"]["adoption_reason"].is_null());
    assert!(
        still_pending["uncollected_children"]
            .as_array()
            .is_some_and(|children| children
                .iter()
                .any(|child| child.as_str() == Some(reviewer_thread_id.as_str()))),
        "the follow-up child remains on the host uncollected-work ledger until native wait"
    );

    mount_multi_agent_turn(
        &server,
        &parent_thread_id,
        "review-wait-followup",
        "wait_agent",
        &json!({"targets":[reviewer_thread_id],"timeout_ms":30000}),
        "The new follow-up reviewer result was collected.",
    )
    .await;
    fixture
        .submit_turn("Collect the follow-up review turn from the same child.")
        .await?;
    let followup_wait_output = tool_output_debug(&rollout_path, "review-wait-followup")?
        .expect("native follow-up wait output");
    assert!(followup_wait_output.contains("The accepted artifact still matches AC-1"));
    let followup_collected = latest_host_completion_snapshot(&rollout_path)?;
    let collected_followup_review = completion_unit(&followup_collected, "review-a");
    assert_eq!(
        collected_followup_review["review_evidence"]["reviewer_turn_id"],
        followup_turn_id
    );
    assert!(collected_followup_review["review_evidence"]["wait_result_ref"].is_string());
    assert_eq!(
        collected_followup_review["review_evidence"]["wait_call_id"],
        "review-wait-followup"
    );
    assert_persisted_reviewer_identity(
        fixture.codex_home_path(),
        &reviewer_thread_id,
        followup_turn_id,
    )
    .await?;

    mount_parent_sse_sequence(
        &server,
        &parent_thread_id,
        vec![
            sse(vec![
                ev_response_created("review-close-a-parent"),
                ev_function_call_with_namespace(
                    "review-close-a",
                    "multi_agent_v1",
                    "close_agent",
                    &json!({"target":reviewer_thread_id}).to_string(),
                ),
                ev_completed("review-close-a-parent"),
            ]),
            function_response("review-adopt-a", "task_context_boundary", &json!({"action":"adopt_review","unit_id":"review-a","reason":"The follow-up reviewer confirmed the accepted artifact still meets AC-1."})),
            sse(vec![ev_response_created("review-adopted"), ev_assistant_message("review-adopted-message", "Review A adopted; continue with task B."), ev_completed("review-adopted")]),
        ],
    )
    .await;
    fixture
        .submit_turn("Adopt the collected review A result.")
        .await?;
    let adoption_output =
        tool_output_debug(&rollout_path, "review-adopt-a")?.expect("adopt_review tool output");
    assert!(!adoption_output.is_empty(), "adopt action returned output");
    let close_output =
        tool_output_debug(&rollout_path, "review-close-a")?.expect("close_agent tool output");
    assert!(
        !close_output.is_empty(),
        "terminal reviewer child was closed"
    );
    let after_review_adoption = latest_host_completion_snapshot(&rollout_path)?;
    assert_eq!(after_review_adoption["uncollected_children"], json!([]));
    let adopted_review_a = completion_unit(&after_review_adoption, "review-a");
    assert_eq!(adopted_review_a["status"], "done");
    assert_eq!(
        adopted_review_a["review_evidence"]["reviewer_thread_id"],
        reviewer_thread_id
    );
    assert_eq!(
        adopted_review_a["review_evidence"]["reviewer_turn_id"],
        followup_turn_id
    );
    assert!(adopted_review_a["review_evidence"]["wait_result_ref"].is_string());

    let parent_thread_id_for_task_b = parent_thread_id.clone();
    let task_b_responses = mount_sse_sequence_match(
        &server,
        move |request: &wiremock::Request| {
            request
                .body_json::<Value>()
                .ok()
                .and_then(|body| {
                    body["client_metadata"]["thread_id"]
                        .as_str()
                        .map(str::to_string)
                })
                .as_deref()
                == Some(parent_thread_id_for_task_b.as_str())
        },
        vec![
            function_response(
                "guidance-begin-task-b",
                "task_context_boundary",
                &json!({"action":"begin","root_task_id":"root","task_id":"task-b","task_revision":1}),
            ),
            function_response(
                "guidance-update-shared-artifact",
                "exec_command",
                &json!({
                    "cmd":"python3 -B - <<'PY'\nfrom pathlib import Path\nPath(\"review-artifact.txt\").write_text(\"updated by task B\\n\")\nPath(\"tests/test_review_artifact.py\").write_text('''import unittest\nfrom pathlib import Path\n\nclass ArtifactContent(unittest.TestCase):\n    def test_accepted_artifact_content(self):\n        self.assertEqual(Path(\"review-artifact.txt\").read_text(), \"updated by task B\\\\n\")\n''')\nPY"
                }),
            ),
            function_response(
                "guidance-check-task-b",
                "exec_command",
                &json!({
                    "cmd":"python3 -B -m unittest discover -s tests -p test_review_artifact.py -v",
                    "task_evidence":{"task_id":"task-b","task_revision":1,"artifact_paths":[relative_artifact, relative_test_artifact],"check_kind":"verification"}
                }),
            ),
            function_response(
                "guidance-accept-task-b",
                "task_context_boundary",
                &json!({"action":"accept","result":"Task B verified the updated shared artifact","satisfied_conditions":["AC-1"]}),
            ),
            function_response(
                "guidance-status-task-b",
                "task_context_boundary",
                &json!({"action":"status","root_task_id":"root"}),
            ),
            sse(vec![
                ev_response_created("guidance-task-b-finished"),
                ev_assistant_message(
                    "guidance-task-b-finished-message",
                    "Task B is accepted; the remaining reviews stay on the plan.",
                ),
                ev_completed("guidance-task-b-finished"),
            ]),
            sse(vec![
                ev_response_created("guidance-after-continuation"),
                ev_assistant_message(
                    "guidance-after-continuation-message",
                    "I reconciled the host review status and saved action.",
                ),
                ev_completed("guidance-after-continuation"),
            ]),
        ],
    )
    .await;
    fixture
        .submit_turn("Verify and accept task B after updating the shared reviewed artifact.")
        .await?;
    let task_b_begin_output =
        tool_output_debug(&rollout_path, "guidance-begin-task-b")?.expect("task B begin output");
    let task_b_update_output = tool_output_debug(&rollout_path, "guidance-update-shared-artifact")?
        .expect("shared artifact update output");
    let task_b_check_output =
        tool_output_debug(&rollout_path, "guidance-check-task-b")?.expect("task B check output");
    let task_b_accept_output =
        tool_output_debug(&rollout_path, "guidance-accept-task-b")?.expect("task B accept output");
    let task_b_status_output =
        tool_output_text(&rollout_path, "guidance-status-task-b")?.expect("task B host status");
    assert!(task_b_begin_output.contains("Task scope begun"));
    assert!(!task_b_update_output.is_empty());
    assert!(
        task_b_check_output.contains("Process exited with code 0")
            && task_b_check_output.contains("Ran 1 test")
            && task_b_check_output.contains("OK"),
        "task B check failed: {task_b_check_output}"
    );
    assert!(
        task_b_accept_output.contains("Task boundary prepared"),
        "task B host receipt was not accepted: {task_b_accept_output}"
    );
    let task_b_status: Value = serde_json::from_str(&task_b_status_output)?;
    let task_b_verification = task_b_status["verification_obligations"]["attempts"]
        .as_array()
        .and_then(|attempts| {
            attempts
                .iter()
                .find(|attempt| attempt["call_id"] == "guidance-check-task-b")
        })
        .expect("host status retains the task B check receipt");
    assert_eq!(task_b_verification["status"], "finished");
    assert_eq!(task_b_verification["exit_code"], 0);
    assert_eq!(task_b_verification["succeeded"], true);
    assert_eq!(
        task_b_verification["artifact_paths"],
        json!([relative_artifact, relative_test_artifact])
    );
    assert_eq!(fs::read_to_string(&artifact)?, "updated by task B\n");
    assert!(fs::read_to_string(&test_artifact)?.contains("updated by task B\\n"));
    let task_b_requests = task_b_responses.requests();
    assert_eq!(
        task_b_requests.len(),
        7,
        "turn-end continuation is followed by a new model sampling request"
    );
    let continuation_request = &task_b_requests[6];
    assert!(
        continuation_request
            .body_contains_text("Continue unfinished conditions, units, or child collection")
    );
    assert!(continuation_request.body_contains_text("Current host review status: Review review-a remains historical for the current accepted artifact or condition definitions"));
    assert!(continuation_request.body_contains_text("Saved next action: Begin task A."));
    assert!(continuation_request.body_contains_text("review dependency readiness"));
    assert!(
        continuation_request.body_contains_text(
            "carry forward existing accepted task execution evidence unchanged"
        )
    );
    let latest_after_task_b = latest_host_completion_snapshot(&rollout_path)?;
    assert_eq!(latest_after_task_b["state"]["next_action"], "Begin task A");
    assert_eq!(
        completion_unit(&latest_after_task_b, "review-a")["status"],
        "done"
    );
    assert_eq!(
        completion_unit(&latest_after_task_b, "task-b")["status"],
        "done"
    );
    let task_b_unit = completion_unit(&latest_after_task_b, "task-b");
    assert!(["evidence", "task_refs"].iter().any(|field| {
        task_b_unit[*field].as_array().is_some_and(|refs| {
            refs.iter().any(|reference| {
                reference["task_id"] == "task-b" && reference["task_revision"] == 1
            })
        })
    }));
    assert_eq!(
        completion_unit(&latest_after_task_b, "review-b")["status"],
        "planned"
    );
    assert_eq!(
        completion_unit(&latest_after_task_b, "review-b")["review_target"],
        json!({"task_id":"task-b","task_revision":1})
    );

    let snapshots =
        host_completion_snapshots(&fixture.codex.rollout_path().expect("rollout path"))?;
    let adopted = snapshots
        .iter()
        .rev()
        .find(|snapshot| snapshot["state"]["units"].is_array())
        .expect("host completion snapshot after adoption");
    let units = adopted["state"]["units"].as_array().unwrap();
    let review_a = units.iter().find(|unit| unit["id"] == "review-a").unwrap();
    let review_b = units.iter().find(|unit| unit["id"] == "review-b").unwrap();
    let rollout = fs::read_to_string(fixture.codex.rollout_path().expect("rollout path"))?;
    let review_tool_outputs = rollout
        .lines()
        .filter_map(|line| codex_rollout::parse_rollout_line(line).ok())
        .filter_map(|entry| match entry.item {
            RolloutItem::ResponseItem(envelope) => match envelope.item {
                ResponseItem::FunctionCallOutput {
                    call_id: Some(call_id),
                    output,
                    ..
                } if ["review-action-a", "review-wait-call", "review-adopt-a"]
                    .contains(&call_id.as_str()) =>
                {
                    Some((call_id, format!("{output:?}")))
                }
                ResponseItem::CustomToolCallOutput {
                    call_id, output, ..
                } if ["review-action-a", "review-wait-call", "review-adopt-a"]
                    .contains(&call_id.as_str()) =>
                {
                    Some((call_id, format!("{output:?}")))
                }
                _ => None,
            },
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        review_a["status"], "done",
        "review action outputs: {review_tool_outputs:?}; evidence: {}",
        review_a["review_evidence"]
    );
    assert_eq!(review_a["review_evidence"]["reviewer_role"], REVIEW_ROLE);
    assert_eq!(
        review_b["status"], "planned",
        "required review B remains on the host board"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cold_resume_reuses_running_uncollected_typed_review_launch() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let provider_url = format!("{}/v1", server.uri());
    let provider_url_for_config = provider_url.clone();
    let fixture = test_codex()
        .with_config(move |config| {
            configure_completion_review_test(config, &provider_url_for_config);
        })
        .build(&server)
        .await?;
    let parent_thread_id = fixture.session_configured.thread_id.to_string();
    let artifact = fixture.cwd_path().join("cold-resume-review-artifact.txt");
    fs::write(&artifact, "accepted artifact\n")?;
    write_artifact_content_test(
        fixture.cwd_path(),
        "test_cold_resume_review_artifact.py",
        "cold-resume-review-artifact.txt",
    )?;
    let relative_artifact = artifact
        .strip_prefix(fixture.cwd_path())?
        .to_string_lossy()
        .to_string();
    let review_contract = json!({
        "contract_revision": 1,
        "conditions": [{
            "id": "AC-1", "revision": 1, "result": "Task A is correct", "scope": "CLI",
            "verification": "Inspect the accepted artifact", "acceptance_criteria": "Expected content is present",
            "status": "open", "evidence": [], "unmet_reason": null
        }],
        "units": [
            {"id":"task-a", "purpose":"Create the reviewed artifact", "status":"ready", "condition_refs":["AC-1"], "depends_on":[], "evidence":[], "task_refs":[]},
            {"id":"review-a", "purpose":"Review task A against AC-1", "status":"planned", "kind":"review", "condition_refs":["AC-1"], "depends_on":["task-a"], "evidence":[], "task_refs":[], "review_target":{"task_id":"task-a","task_revision":1}}
        ],
        "current_unit": "task-a",
        "failed_approaches": [],
        "blocker": null,
        "next_action": "Spawn the reviewer for task-a and wait for its result.",
        "product_progress_at": null,
        "runtime_recovery_at": null,
        "activity_at": null
    });

    mount_parent_sse_sequence(
        &server,
        &parent_thread_id,
        vec![
            function_response(
                "cold-review-begin-root",
                "task_context_boundary",
                &json!({"action":"begin","root_task_id":"root","task_id":"root","task_revision":1,"completion_state":review_contract}),
            ),
            sse(vec![
                ev_response_created("cold-review-root-finished"),
                ev_assistant_message("cold-review-root-message", "Review plan registered."),
                ev_completed("cold-review-root-finished"),
            ]),
        ],
    )
    .await;
    fixture
        .submit_turn("Register task A and its required review.")
        .await?;
    mount_parent_sse_sequence(
        &server,
        &parent_thread_id,
        vec![
            function_response(
                "cold-review-begin-task-a",
                "task_context_boundary",
                &json!({"action":"begin","root_task_id":"root","task_id":"task-a","task_revision":1}),
            ),
            function_response(
                "cold-review-verify-task-a",
                "exec_command",
                &json!({"cmd":"python3 -B -m unittest discover -s tests -p test_cold_resume_review_artifact.py -v","task_evidence":{"task_id":"task-a","task_revision":1,"artifact_paths":[relative_artifact],"check_kind":"verification"}}),
            ),
            function_response(
                "cold-review-accept-task-a",
                "task_context_boundary",
                &json!({"action":"accept","result":"Task A artifact verified","satisfied_conditions":["AC-1"]}),
            ),
            sse(vec![
                ev_response_created("cold-review-task-a-finished"),
                ev_assistant_message("cold-review-task-a-message", "Task A is accepted; review A remains required."),
                ev_completed("cold-review-task-a-finished"),
            ]),
        ],
    )
    .await;
    fixture
        .submit_turn("Verify and accept task A before its required review.")
        .await?;
    let rollout_path = fixture.codex.rollout_path().expect("parent rollout path");
    let accepted = latest_host_completion_snapshot(&rollout_path)?;
    assert_eq!(completion_unit(&accepted, "task-a")["status"], "done");
    assert_eq!(completion_unit(&accepted, "review-a")["status"], "planned");

    mount_delayed_completion_reviewer_response(
        &server,
        parent_thread_id.clone(),
        Duration::from_secs(12 * 60),
    )
    .await;
    mount_parent_sse_sequence(
        &server,
        &parent_thread_id,
        vec![
            sse(vec![
                ev_response_created("cold-review-launch-parent"),
                ev_function_call(
                    "cold-review-launch-call",
                    "task_context_boundary",
                    &json!({"action":"review","target_task_id":"task-a","unit_id":"review-a"})
                        .to_string(),
                ),
                ev_completed("cold-review-launch-parent"),
            ]),
            sse(vec![
                ev_response_created("cold-review-launch-finished"),
                ev_assistant_message("cold-review-launch-message", "Review launched."),
                ev_completed("cold-review-launch-finished"),
            ]),
        ],
    )
    .await;
    fixture
        .submit_turn("Launch the planned read-only review for task A.")
        .await?;

    let before_resume = latest_host_completion_snapshot(&rollout_path)?;
    let launched_unit = completion_unit(&before_resume, "review-a");
    let reviewer_thread_id = launched_unit["review_evidence"]["reviewer_thread_id"]
        .as_str()
        .expect("host-bound reviewer ID")
        .to_string();
    let reviewer_turn_id = launched_unit["review_evidence"]["reviewer_turn_id"]
        .as_str()
        .expect("host-bound reviewer generation")
        .to_string();
    let attempt_id = launched_unit["review_state"]["active_attempt"]["attempt_id"]
        .as_str()
        .expect("canonical review attempt")
        .to_string();
    assert_eq!(launched_unit["status"], "running");
    assert!(launched_unit["review_evidence"]["wait_result_ref"].is_null());
    assert!(launched_unit["review_evidence"]["terminal_status"].is_null());
    assert_eq!(
        before_resume["state"]["next_action"],
        "Spawn the reviewer for task-a and wait for its result."
    );
    assert_eq!(
        before_resume["uncollected_children"],
        json!([reviewer_thread_id])
    );
    assert_eq!(
        wait_for_completion_reviewer_requests(&server, &parent_thread_id)
            .await
            .len(),
        1
    );

    mount_function_turn(
        &server,
        &parent_thread_id,
        "cold-review-premature-adopt-call",
        "task_context_boundary",
        &json!({"action":"adopt_review","unit_id":"review-a","reason":"Attempt adoption before collecting the reviewer."}),
        "The host rejected premature adoption.",
    )
    .await;
    fixture
        .submit_turn("Try the host adoption action before collecting this review.")
        .await?;
    let premature_adopt_output =
        tool_output_debug(&rollout_path, "cold-review-premature-adopt-call")?
            .expect("premature adoption output");
    assert!(
        premature_adopt_output.contains("uncollected"),
        "host did not reject adoption while the reviewer child was uncollected: {premature_adopt_output}"
    );
    let after_rejected_adoption = latest_host_completion_snapshot(&rollout_path)?;
    let uncollected_unit = completion_unit(&after_rejected_adoption, "review-a");
    assert_eq!(uncollected_unit["status"], "running");
    assert!(uncollected_unit["review_decision_reason"].is_null());
    assert!(uncollected_unit["review_state"]["active_attempt"]["adoption_reason"].is_null());
    assert_eq!(
        after_rejected_adoption["uncollected_children"],
        json!([reviewer_thread_id])
    );

    fixture.codex.flush_rollout().await?;
    let codex_home = Arc::clone(&fixture.home);
    let cwd = Arc::clone(&fixture.cwd);
    fixture.codex.shutdown_and_wait().await?;
    drop(fixture);

    let resume_provider_url = provider_url.clone();
    let mut resume_builder = test_codex().with_config(move |config| {
        configure_completion_review_test(config, &resume_provider_url);
    });
    let resumed = resume_builder
        .resume(&server, codex_home, rollout_path.clone())
        .await?;
    assert_eq!(
        resumed.session_configured.thread_id.to_string(),
        parent_thread_id
    );
    assert!(cwd.path().join("cold-resume-review-artifact.txt").is_file());
    let hydrated = latest_host_completion_snapshot(&rollout_path)?;
    assert!(hydrated["state"]["paused"].as_bool().unwrap_or(false));
    let hydrated_unit = completion_unit(&hydrated, "review-a");
    assert_eq!(hydrated_unit["status"], "running");
    assert_eq!(
        hydrated_unit["review_state"]["active_attempt"]["attempt_id"],
        attempt_id
    );
    assert_eq!(
        hydrated_unit["review_evidence"]["reviewer_thread_id"],
        reviewer_thread_id
    );
    assert_eq!(
        hydrated_unit["review_evidence"]["reviewer_turn_id"],
        reviewer_turn_id
    );
    assert!(hydrated_unit["review_evidence"]["wait_result_ref"].is_null());
    assert!(hydrated_unit["review_evidence"]["terminal_status"].is_null());
    assert!(hydrated_unit["review_decision_reason"].is_null());
    assert_eq!(
        hydrated["uncollected_children"],
        json!([reviewer_thread_id])
    );

    mount_function_turn(
        &server,
        &parent_thread_id,
        "cold-review-repeat-call",
        "task_context_boundary",
        &json!({"action":"review","target_task_id":"task-a","unit_id":"review-a"}),
        "The existing typed review remains unresolved.",
    )
    .await;
    resumed
        .submit_turn("Re-request the typed review for task-a using the current host state.")
        .await?;
    let repeated_output = tool_output_debug(&rollout_path, "cold-review-repeat-call")?
        .expect("repeated typed review action output");
    assert!(
        repeated_output.contains(&reviewer_thread_id),
        "existing reviewer was not returned: {repeated_output}"
    );
    assert_eq!(
        completion_reviewer_requests(&server, &parent_thread_id)
            .await?
            .len(),
        1
    );
    let after_repeat = latest_host_completion_snapshot(&rollout_path)?;
    let repeated_unit = completion_unit(&after_repeat, "review-a");
    assert_eq!(repeated_unit["status"], "running");
    assert_eq!(
        repeated_unit["review_state"]["active_attempt"]["attempt_id"],
        attempt_id
    );
    assert!(repeated_unit["review_evidence"]["wait_result_ref"].is_null());
    assert!(repeated_unit["review_evidence"]["terminal_status"].is_null());
    assert!(repeated_unit["review_decision_reason"].is_null());
    assert!(repeated_unit["review_state"]["active_attempt"]["adoption_reason"].is_null());
    assert_eq!(
        after_repeat["uncollected_children"],
        json!([reviewer_thread_id])
    );

    resumed.codex.shutdown_and_wait().await?;
    drop(cwd);
    Ok(())
}

/// Requires a dedicated nextest timeout greater than the existing 10-minute reviewer SLA.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_reviewer_deadline_stops_collects_and_cold_resumes_the_same_generation() -> Result<()>
{
    skip_if_no_network!(Ok(()));
    const REVIEWER_SLA_MS: i64 = 10 * 60 * 1000;
    const PARENT_WAIT_TIMEOUT_MS: i64 = 11 * 60 * 1000;

    let server = start_mock_server().await;
    let provider_url = format!("{}/v1", server.uri());
    let provider_url_for_config = provider_url.clone();
    let fixture = test_codex()
        .with_config(move |config| {
            configure_completion_review_test(config, &provider_url_for_config);
        })
        .build(&server)
        .await?;
    let parent_thread_id = fixture.session_configured.thread_id.to_string();
    let artifact = fixture.cwd_path().join("deadline-review-artifact.txt");
    fs::write(&artifact, "accepted artifact\n")?;
    write_artifact_content_test(
        fixture.cwd_path(),
        "test_deadline_review_artifact.py",
        "deadline-review-artifact.txt",
    )?;
    let relative_artifact = artifact
        .strip_prefix(fixture.cwd_path())?
        .to_string_lossy()
        .to_string();

    let review_contract = json!({
        "contract_revision":1,
        "conditions":[{
            "id":"AC-1", "revision":1, "result":"Task A is correct", "scope":"CLI",
            "verification":"Inspect the accepted artifact", "acceptance_criteria":"Expected content is present",
            "status":"open", "evidence":[], "unmet_reason":null
        }],
        "units":[
            {"id":"task-a", "purpose":"Create the reviewed artifact", "status":"ready", "condition_refs":["AC-1"], "depends_on":[], "evidence":[], "task_refs":[]},
            {"id":"review-a", "purpose":"Review task A against AC-1", "status":"planned", "kind":"review", "condition_refs":["AC-1"], "depends_on":["task-a"], "evidence":[], "task_refs":[], "review_target":{"task_id":"task-a","task_revision":1}},
            {"id":"task-b", "purpose":"Continue after review A", "status":"planned", "condition_refs":["AC-1"], "depends_on":["review-a"], "evidence":[], "task_refs":[]}
        ],
        "current_unit":"task-a", "failed_approaches":[], "blocker":null,
        "next_action":"Begin task A", "product_progress_at":null,
        "runtime_recovery_at":null, "activity_at":null
    });
    mount_parent_sse_sequence(
        &server,
        &parent_thread_id,
        vec![
            function_response(
                "deadline-review-begin-root",
                "task_context_boundary",
                &json!({"action":"begin","root_task_id":"root","task_id":"root","task_revision":1,"completion_state":review_contract}),
            ),
            sse(vec![
                ev_response_created("deadline-review-root-begun"),
                ev_assistant_message("deadline-review-root-begun-message", "Review plan registered."),
                ev_completed("deadline-review-root-begun"),
            ]),
        ],
    )
    .await;
    fixture
        .submit_turn("Register task A, its required review, and the following task.")
        .await?;

    mount_parent_sse_sequence(
        &server,
        &parent_thread_id,
        vec![
            function_response(
                "deadline-review-begin-task-a",
                "task_context_boundary",
                &json!({"action":"begin","root_task_id":"root","task_id":"task-a","task_revision":1}),
            ),
            function_response(
                "deadline-review-verify-task-a",
                "exec_command",
                &json!({"cmd":"python3 -B -m unittest discover -s tests -p test_deadline_review_artifact.py -v","task_evidence":{"task_id":"task-a","task_revision":1,"artifact_paths":[relative_artifact],"check_kind":"verification"}}),
            ),
            function_response(
                "deadline-review-accept-task-a",
                "task_context_boundary",
                &json!({"action":"accept","result":"Task A artifact verified","satisfied_conditions":["AC-1"]}),
            ),
            sse(vec![
                ev_response_created("deadline-review-task-a-accepted"),
                ev_assistant_message("deadline-review-task-a-message", "Task A is accepted; review A remains required."),
                ev_completed("deadline-review-task-a-accepted"),
            ]),
        ],
    )
    .await;
    fixture
        .submit_turn("Verify and accept task A before its required review.")
        .await?;
    let rollout_path = fixture.codex.rollout_path().expect("parent rollout path");
    let task_a_begin_output = tool_output_debug(&rollout_path, "deadline-review-begin-task-a")?
        .expect("task A begin output");
    let task_a_verification_output =
        tool_output_debug(&rollout_path, "deadline-review-verify-task-a")?
            .expect("task A verification output");
    let task_a_accept_output = tool_output_debug(&rollout_path, "deadline-review-accept-task-a")?
        .expect("task A acceptance output");
    assert!(!task_a_begin_output.is_empty());
    assert!(
        task_a_verification_output.contains("OK"),
        "task A verification failed before reviewer launch: {task_a_verification_output}"
    );
    assert!(
        task_a_accept_output.contains("Task boundary prepared"),
        "task A acceptance failed before reviewer launch: {task_a_accept_output}"
    );
    let accepted = latest_host_completion_snapshot(&rollout_path)?;
    let accepted_task_a = completion_unit(&accepted, "task-a");
    let accepted_review_a = completion_unit(&accepted, "review-a");
    assert_eq!(accepted["state"]["conditions"][0]["status"], "satisfied");
    assert_eq!(accepted_task_a["status"], "done");
    assert_eq!(accepted_review_a["status"], "planned");

    mount_delayed_completion_reviewer_response(
        &server,
        parent_thread_id.clone(),
        Duration::from_secs(12 * 60),
    )
    .await;
    mount_parent_sse_sequence(
        &server,
        &parent_thread_id,
        vec![
            sse(vec![
                ev_response_created("deadline-review-action-parent"),
                ev_function_call(
                    "deadline-review-action",
                    "task_context_boundary",
                    &json!({"action":"review","target_task_id":"task-a"}).to_string(),
                ),
                ev_completed("deadline-review-action-parent"),
            ]),
            sse(vec![
                ev_response_created("deadline-review-action-finished"),
                ev_assistant_message(
                    "deadline-review-action-message",
                    "Review launched; wait on that reviewer.",
                ),
                ev_completed("deadline-review-action-finished"),
            ]),
        ],
    )
    .await;
    fixture
        .submit_turn("Launch the planned read-only review for task A.")
        .await?;

    let review_action_output =
        tool_output_debug(&rollout_path, "deadline-review-action")?.expect("review action output");
    assert!(
        !review_action_output.is_empty(),
        "review action returned no host output; accepted snapshot: {accepted}"
    );
    let launched = latest_host_completion_snapshot(&rollout_path)?;
    let launched_review = completion_unit(&launched, "review-a");
    let reviewer_thread_id = launched_review["review_evidence"]["reviewer_thread_id"]
        .as_str()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "review action did not bind a reviewer thread; output={review_action_output}; task-a-status={}; review-a-status={}; review-a-evidence={}; review-a-state={}",
                accepted_task_a["status"],
                launched_review["status"],
                launched_review["review_evidence"],
                launched_review["review_state"]
            )
        })?
        .to_string();
    let reviewer_turn_id = launched_review["review_evidence"]["reviewer_turn_id"]
        .as_str()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "review action bound no reviewer generation; output={review_action_output}; review-a-state={}",
                launched_review["review_state"]
            )
        })?
        .to_string();
    let deadline_unix_ms = launched_review["review_evidence"]["deadline_unix_ms"]
        .as_i64()
        .expect("host-assigned reviewer deadline");
    let reviewer_requests = wait_for_completion_reviewer_requests(&server, &parent_thread_id).await;
    assert_eq!(
        reviewer_requests.len(),
        1,
        "one host-bound reviewer request must be dispatched"
    );
    let reviewer_request_body: Value = reviewer_requests[0].body_json()?;
    assert_eq!(
        reviewer_request_body["client_metadata"]["thread_id"],
        reviewer_thread_id
    );
    assert_eq!(
        reviewer_request_body["client_metadata"]["turn_id"],
        reviewer_turn_id
    );
    assert_eq!(reviewer_request_body["model"], REVIEW_MODEL);
    let remaining_before_wait_ms = deadline_unix_ms.saturating_sub(unix_time_ms());
    assert!(
        (REVIEWER_SLA_MS - 60_000..=REVIEWER_SLA_MS).contains(&remaining_before_wait_ms),
        "reviewer request reached the loopback gateway with about the full host SLA remaining: {remaining_before_wait_ms}ms"
    );
    assert!(matches!(
        fixture
            .thread_manager
            .get_thread(codex_protocol::ThreadId::try_from(
                reviewer_thread_id.as_str()
            )?)
            .await?
            .agent_status()
            .await,
        AgentStatus::Running
    ));

    mount_parent_sse_sequence(
        &server,
        &parent_thread_id,
        vec![
            sse(vec![
                ev_response_created("deadline-wait-parent"),
                ev_function_call_with_namespace(
                    "deadline-wait-call",
                    "multi_agent_v1",
                    "wait_agent",
                    &json!({
                        "targets":[reviewer_thread_id],
                        "timeout_ms":PARENT_WAIT_TIMEOUT_MS
                    })
                    .to_string(),
                ),
                ev_completed("deadline-wait-parent"),
            ]),
            sse(vec![
                ev_response_created("deadline-wait-finished"),
                ev_assistant_message(
                    "deadline-wait-finished-message",
                    "The host deadline result was collected.",
                ),
                ev_completed("deadline-wait-finished"),
            ]),
        ],
    )
    .await;
    let wait_submission = fixture
        .codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Wait for the exact reviewer generation and collect its host deadline result."
                .into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let wait_turn_id = match wait_submission {
        TurnInputSubmission::Started { turn_id } => turn_id,
        submission => anyhow::bail!("deadline wait turn did not start: {submission:?}"),
    };
    let wait_started = tokio::time::Instant::now();
    let wait_event_deadline = wait_started
        + Duration::from_millis(remaining_before_wait_ms as u64)
        + Duration::from_secs(120);
    loop {
        let event =
            tokio::time::timeout_at(wait_event_deadline, fixture.codex.next_event()).await??;
        if matches!(
            event.msg,
            EventMsg::TurnComplete(turn_complete) if turn_complete.turn_id == wait_turn_id
        ) {
            break;
        }
    }
    let wait_elapsed = wait_started.elapsed();
    assert!(
        wait_elapsed + Duration::from_secs(2)
            >= Duration::from_millis(remaining_before_wait_ms as u64),
        "native wait returned before the saved reviewer deadline: elapsed={wait_elapsed:?}, remaining={remaining_before_wait_ms}ms"
    );
    assert!(
        wait_elapsed
            <= Duration::from_millis(remaining_before_wait_ms as u64) + Duration::from_secs(120),
        "native wait exceeded the saved deadline by more than its bounded recovery margin: elapsed={wait_elapsed:?}, remaining={remaining_before_wait_ms}ms"
    );

    let wait_output = tool_output_debug(&rollout_path, "deadline-wait-call")?
        .expect("native deadline wait output");
    assert!(
        wait_output.contains("ReviewEnded"),
        "native wait collected the exact ReviewEnded status: {wait_output}"
    );
    let after_wait = latest_host_completion_snapshot(&rollout_path)?;
    let failed_review = completion_unit(&after_wait, "review-a");
    let failed_evidence = &failed_review["review_evidence"];
    assert_eq!(failed_review["status"], "failed");
    assert_eq!(failed_evidence["reviewer_thread_id"], reviewer_thread_id);
    assert_eq!(failed_evidence["reviewer_turn_id"], reviewer_turn_id);
    assert_eq!(failed_evidence["deadline_unix_ms"], deadline_unix_ms);
    assert_eq!(failed_evidence["terminal_status"], "failed");
    assert!(
        failed_evidence["deadline_escalation"]
            .as_str()
            .is_some_and(|reason| reason.contains("ReviewEnded terminal status"))
    );
    let original_wait_result_ref = failed_evidence["wait_result_ref"]
        .as_str()
        .expect("deadline generation native wait receipt")
        .to_string();
    assert_eq!(failed_evidence["wait_call_id"], "deadline-wait-call");
    assert_eq!(
        persisted_review_ended_event_count(
            fixture.codex_home_path(),
            &reviewer_thread_id,
            &reviewer_turn_id,
        )
        .await?,
        1,
        "one durable ReviewEnded event belongs to the exact reviewer generation"
    );
    let reviewer_thread = fixture
        .thread_manager
        .get_thread(codex_protocol::ThreadId::try_from(
            reviewer_thread_id.as_str(),
        )?)
        .await?;
    assert_eq!(
        reviewer_thread.agent_status().await,
        AgentStatus::Errored("ReviewEnded".into())
    );
    assert_eq!(
        completion_reviewer_requests(&server, &parent_thread_id)
            .await?
            .len(),
        1,
        "deadline handling must not spawn or resubmit another reviewer request"
    );

    reviewer_thread.flush_rollout().await?;
    fixture.codex.flush_rollout().await?;
    let codex_home = Arc::clone(&fixture.home);
    let cwd = Arc::clone(&fixture.cwd);
    reviewer_thread.shutdown_and_wait().await?;
    fixture.codex.shutdown_and_wait().await?;
    drop(reviewer_thread);
    drop(fixture);
    assert!(cwd.path().join("deadline-review-artifact.txt").is_file());

    let resume_provider_url = provider_url.clone();
    let mut resume_builder = test_codex().with_config(move |config| {
        configure_completion_review_test(config, &resume_provider_url);
    });
    let resumed = resume_builder
        .resume(&server, codex_home, rollout_path.clone())
        .await?;
    assert_eq!(
        resumed.session_configured.thread_id.to_string(),
        parent_thread_id
    );

    mount_parent_sse_sequence(
        &server,
        &parent_thread_id,
        vec![
            sse(vec![
                ev_response_created("deadline-cold-resume-parent"),
                ev_function_call_with_namespace(
                    "deadline-resume-agent-call",
                    "multi_agent_v1",
                    "resume_agent",
                    &json!({"id":reviewer_thread_id}).to_string(),
                ),
                ev_completed("deadline-cold-resume-parent"),
            ]),
            sse(vec![
                ev_response_created("deadline-cold-resume-wait-parent"),
                ev_function_call_with_namespace(
                    "deadline-cold-resume-wait-call",
                    "multi_agent_v1",
                    "wait_agent",
                    &json!({"targets":[reviewer_thread_id],"timeout_ms":30_000}).to_string(),
                ),
                ev_completed("deadline-cold-resume-wait-parent"),
            ]),
            function_response(
                "deadline-cold-resume-status-call",
                "task_context_boundary",
                &json!({"action":"status","root_task_id":"root"}),
            ),
            sse(vec![
                ev_response_created("deadline-cold-resume-finished"),
                ev_assistant_message(
                    "deadline-cold-resume-message",
                    "Saved review state remains failed and collected.",
                ),
                ev_completed("deadline-cold-resume-finished"),
            ]),
        ],
    )
    .await;
    resumed
        .submit_turn("Restore the saved reviewer, wait for its terminal generation, and inspect the saved completion state.")
        .await?;

    let resume_output = tool_output_debug(&rollout_path, "deadline-resume-agent-call")?
        .expect("cold resume must restore the same reviewer thread");
    assert!(
        resume_output.contains("ReviewEnded"),
        "resumed status: {resume_output}"
    );
    let cold_wait_output = tool_output_debug(&rollout_path, "deadline-cold-resume-wait-call")?
        .expect("cold resume wait must observe the existing terminal reviewer");
    assert!(
        cold_wait_output.contains("ReviewEnded"),
        "cold wait must return the original terminal generation: {cold_wait_output}"
    );
    let cold_status_output = tool_output_debug(&rollout_path, "deadline-cold-resume-status-call")?
        .expect("cold resume status must expose the restored completion state");
    for saved_value in [
        reviewer_thread_id.as_str(),
        reviewer_turn_id.as_str(),
        original_wait_result_ref.as_str(),
        "deadline-wait-call",
        "ReviewEnded",
        "failed",
    ] {
        assert!(
            cold_status_output.contains(saved_value),
            "cold-resumed status output omitted saved value {saved_value}: {cold_status_output}"
        );
    }
    let resumed_reviewer = resumed
        .thread_manager
        .get_thread(codex_protocol::ThreadId::try_from(
            reviewer_thread_id.as_str(),
        )?)
        .await?;
    assert_eq!(
        resumed_reviewer.agent_status().await,
        AgentStatus::Errored("ReviewEnded".into())
    );
    assert_eq!(
        persisted_review_ended_event_count(
            resumed.codex_home_path(),
            &reviewer_thread_id,
            &reviewer_turn_id,
        )
        .await?,
        1,
        "cold resume must retain one terminal event for the same reviewer generation"
    );
    assert_eq!(
        completion_reviewer_requests(&server, &parent_thread_id)
            .await?
            .len(),
        1,
        "cold resume and terminal wait must not dispatch a second reviewer model request"
    );
    resumed.codex.shutdown_and_wait().await?;
    drop(cwd);
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_review_close_preserves_history_and_allows_one_new_authorized_review() -> Result<()>
{
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let provider_url = format!("{}/v1", server.uri());
    let fixture = test_codex()
        .with_config(move |config| {
            config.rencrow_compaction = true;
            config.rencrow_autonomous_completion = true;
            config.rencrow_tool_allowlist = Some(vec![
                "exec_command".into(),
                "task_context_boundary".into(),
                "multi_agent_v1::wait_agent".into(),
                "multi_agent_v1::close_agent".into(),
            ]);
            config.features.enable(Feature::Collab).unwrap();
            let model_provider_id = config.model_provider_id.clone();
            config.model_providers.insert(
                model_provider_id.clone(),
                config.model_provider.clone(),
            );
            let role_path = config.codex_home.join("completion-reviewer.toml");
            fs::write(
                &role_path,
                format!(
                    "model = \"{REVIEW_MODEL}\"\nrencrow_read_only = true\nmodel_provider = \"{model_provider_id}\"\n\n[rencrow_execution_guard]\ntask_kind = \"read_only\"\n\n[model_providers.mock]\nname = \"mock\"\nbase_url = \"{provider_url}\"\nenv_key = \"PATH\"\nwire_api = \"responses\"\n"
                ),
            )
            .unwrap();
            config.agent_roles.insert(
                REVIEW_ROLE.into(),
                AgentRoleConfig {
                    enabled: None,
                    description: Some("Read-only completion reviewer".into()),
                    config_file: Some(role_path.to_path_buf()),
                    nickname_candidates: None,
                },
            );
        })
        .build(&server)
        .await?;
    let parent_thread_id = fixture.session_configured.thread_id.to_string();

    let artifact = fixture.cwd_path().join("review-recovery-artifact.txt");
    fs::write(&artifact, "accepted artifact\n")?;
    let relative_artifact = artifact
        .strip_prefix(fixture.cwd_path())?
        .to_string_lossy()
        .to_string();
    write_artifact_content_test(
        fixture.cwd_path(),
        "test_review_recovery_artifact.py",
        "review-recovery-artifact.txt",
    )?;
    let state = json!({
        "contract_revision":1,
        "conditions":[{"id":"AC-1","revision":1,"result":"Artifact is correct","scope":"CLI","verification":"Inspect the accepted artifact","acceptance_criteria":"Expected content is present","status":"open","evidence":[],"unmet_reason":null}],
        "units":[
            {"id":"task-a","purpose":"Create the reviewed artifact","status":"ready","condition_refs":["AC-1"],"depends_on":[],"evidence":[],"task_refs":[]},
            {"id":"review-a","purpose":"Review task A against AC-1","status":"planned","kind":"review","condition_refs":["AC-1"],"depends_on":["task-a"],"evidence":[],"task_refs":[],"review_target":{"task_id":"task-a","task_revision":1}}
        ],
        "current_unit":"task-a","failed_approaches":[],"blocker":null,"next_action":"Begin task A",
        "product_progress_at":null,"runtime_recovery_at":null,"activity_at":null
    });
    mount_parent_sse_sequence(
        &server,
        &parent_thread_id,
        vec![
            function_response("recovery-begin-root", "task_context_boundary", &json!({"action":"begin","root_task_id":"root","task_id":"root","task_revision":1,"completion_state":state})),
            sse(vec![ev_response_created("recovery-root-ready"), ev_assistant_message("recovery-root-ready-msg", "Plan registered."), ev_completed("recovery-root-ready")]),
        ],
    )
    .await;
    fixture
        .submit_turn("Register the artifact task and its required review.")
        .await?;

    mount_parent_sse_sequence(
        &server,
        &parent_thread_id,
        vec![
            function_response("recovery-begin-task", "task_context_boundary", &json!({"action":"begin","root_task_id":"root","task_id":"task-a","task_revision":1})),
            function_response("recovery-check-task", "exec_command", &json!({"cmd":"python3 -B -m unittest discover -s tests -p test_review_recovery_artifact.py -v","task_evidence":{"task_id":"task-a","task_revision":1,"artifact_paths":[relative_artifact],"check_kind":"verification"}})),
            function_response("recovery-accept-task", "task_context_boundary", &json!({"action":"accept","result":"Task A artifact verified","satisfied_conditions":["AC-1"]})),
            sse(vec![ev_response_created("recovery-task-accepted"), ev_assistant_message("recovery-task-accepted-msg", "Task A is accepted; its required review remains."), ev_completed("recovery-task-accepted")]),
        ],
    )
    .await;
    fixture.submit_turn("Verify and accept task A.").await?;
    let rollout_path = fixture.codex.rollout_path().expect("rollout path");
    let begin_output = tool_output_debug(&rollout_path, "recovery-begin-task")?
        .expect("recovery task begin output");
    let check_output = tool_output_debug(&rollout_path, "recovery-check-task")?
        .expect("recovery task check output");
    let accept_output = tool_output_debug(&rollout_path, "recovery-accept-task")?
        .expect("recovery task accept output");
    assert!(!begin_output.is_empty());
    assert!(
        check_output.contains("OK"),
        "recovery task verification failed: {check_output}"
    );
    assert!(
        accept_output.contains("Task boundary prepared"),
        "recovery task acceptance failed: {accept_output}"
    );

    let child_parent_thread_id = parent_thread_id.clone();
    let matches_review_request = move |request: &wiremock::Request| {
        is_completion_reviewer_request(request, &child_parent_thread_id)
    };
    let failed_child_response = mount_sse_once_match(
        &server,
        matches_review_request,
        sse_failed(
            "reviewer-provider-error",
            "insufficient_quota",
            "simulated reviewer failure",
        ),
    )
    .await;
    mount_function_turn(
        &server,
        &parent_thread_id,
        "recovery-review-first",
        "task_context_boundary",
        &json!({"action":"review","target_task_id":"task-a"}),
        "The reviewer ended with a provider error.",
    )
    .await;
    fixture
        .submit_turn("Run the planned read-only review for task A.")
        .await?;
    let first_launch = latest_host_completion_snapshot(&rollout_path)?;
    let first_launch_output = tool_output_debug(&rollout_path, "recovery-review-first")?
        .unwrap_or_else(|| "<missing review tool output>".into());
    let first_unit = completion_unit(&first_launch, "review-a");
    let first_thread_id = first_unit["review_evidence"]["reviewer_thread_id"]
        .as_str()
        .expect("first host-bound reviewer thread")
        .to_string();
    let first_turn_id = first_unit["review_evidence"]["reviewer_turn_id"]
        .as_str()
        .expect("first reviewer turn")
        .to_string();
    assert!(first_launch_output.contains(REVIEW_MODEL));
    assert!(first_launch_output.contains(REVIEW_ROLE));
    let first_child_requests = failed_child_response
        .requests()
        .into_iter()
        .filter(|request| {
            request.body_json()["client_metadata"]["thread_id"].as_str()
                == Some(first_thread_id.as_str())
        })
        .collect::<Vec<_>>();
    let dispatch_diagnostics = if first_child_requests.len() != 1 {
        Some(
            completion_reviewer_request_diagnostics(&server, &parent_thread_id, &first_thread_id)
                .await,
        )
    } else {
        None
    };
    assert_eq!(
        first_child_requests.len(),
        1,
        "one dispatch for the host-bound child; request diagnostics: {dispatch_diagnostics:?}"
    );
    assert_eq!(first_child_requests[0].body_json()["model"], REVIEW_MODEL);
    assert_eq!(
        first_child_requests[0].body_json()["client_metadata"]["turn_id"],
        first_turn_id
    );

    mount_multi_agent_turn(
        &server,
        &parent_thread_id,
        "recovery-wait-first",
        "wait_agent",
        &json!({"targets":[first_thread_id],"timeout_ms":30000}),
        "The failed reviewer result was collected.",
    )
    .await;
    fixture
        .submit_turn("Collect the result from the failed reviewer.")
        .await?;
    assert_persisted_reviewer_identity(fixture.codex_home_path(), &first_thread_id, &first_turn_id)
        .await?;
    let after_wait = latest_host_completion_snapshot(&rollout_path)?;
    let first_wait_output = tool_output_debug(&rollout_path, "recovery-wait-first")?
        .unwrap_or_else(|| "<missing wait_agent output>".into());
    assert!(
        first_wait_output.contains("Quota exceeded")
            || first_wait_output.contains("simulated reviewer failure"),
        "wait must collect the provider failure served to the child: {first_wait_output}"
    );
    let first_wait_unit = completion_unit(&after_wait, "review-a");
    let preceding_outputs = [
        "recovery-begin-root",
        "recovery-begin-task",
        "recovery-check-task",
        "recovery-accept-task",
        "recovery-review-first",
    ]
    .into_iter()
    .map(|call_id| {
        (
            call_id,
            tool_output_debug(&rollout_path, call_id)
                .ok()
                .flatten()
                .unwrap_or_else(|| "<missing tool output>".into()),
        )
    })
    .collect::<Vec<_>>();
    assert_eq!(
        first_wait_unit["status"], "failed",
        "wait output={first_wait_output}; review evidence={}; blocker={}; preceding tool outputs={preceding_outputs:?}",
        first_wait_unit["review_evidence"], after_wait["state"]["blocker"],
    );
    assert_eq!(
        after_wait["state"]["units"][1]["review_evidence"]["reviewer_turn_id"],
        first_turn_id
    );
    assert_eq!(
        after_wait["state"]["failed_approaches"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    mount_multi_agent_turn(
        &server,
        &parent_thread_id,
        "recovery-close-first",
        "close_agent",
        &json!({"target":first_thread_id}),
        "The failed reviewer child was closed.",
    )
    .await;
    fixture
        .submit_turn("Close the terminal failed reviewer child.")
        .await?;
    let after_close = latest_host_completion_snapshot(&rollout_path)?;
    assert_eq!(after_close["state"]["units"][1]["status"], "failed");
    assert_eq!(
        after_close["state"]["units"][1]["review_evidence"]["reviewer_turn_id"],
        first_turn_id
    );

    mount_function_turn(
        &server,
        &parent_thread_id,
        "recovery-reject-failed-adoption",
        "task_context_boundary",
        &json!({"action":"adopt_review","unit_id":"review-a","reason":"Attempt to adopt the failed reviewer result."}),
        "The host refused to adopt the failed review.",
    )
    .await;
    fixture
        .submit_turn("Try adopting the failed review to verify the host rejects it.")
        .await?;
    let failed_adoption = tool_output_debug(&rollout_path, "recovery-reject-failed-adoption")?
        .expect("failed review adoption output");
    assert!(
        failed_adoption.contains(
            "review result has not been collected from the same completed child generation"
        ),
        "the failed result must remain unadoptable: {failed_adoption}"
    );

    let failed_attempt_id =
        completion_unit(&after_close, "review-a")["review_evidence"]["spawn_call_id"]
            .as_str()
            .expect("host spawn call ID identifies the failed attempt")
            .to_string();
    let retry_previous_plan = after_close["state"]["next_action"]
        .as_str()
        .expect("host completion snapshot retains the current next action")
        .to_string();
    let review_state_before_unknown_retry =
        completion_unit(&after_close, "review-a")["review_state"].clone();
    mount_function_turn(
        &server,
        &parent_thread_id,
        "recovery-reject-unknown-retry",
        "task_context_boundary",
        &json!({
            "action":"plan_review",
            "root_task_id":"root",
            "previous_plan":retry_previous_plan.clone(),
            "revised_plan":"Try a new review attempt without a known host failed generation.",
            "reason":"The submitted attempt identifier is not the exact failed generation.",
            "review_retry":{"unit_id":"review-a","expected_attempt_id":"unseen-attempt"}
        }),
        "The host refused a retry grant for an unknown attempt.",
    )
    .await;
    fixture
        .submit_turn("Try to authorize a retry using an unknown attempt identifier.")
        .await?;
    let unknown_retry = tool_output_debug(&rollout_path, "recovery-reject-unknown-retry")?
        .expect("unknown retry tool output");
    assert!(
        unknown_retry
            .contains("retry request does not match the active or importable legacy attempt"),
        "unknown attempt must reach the exact-attempt validation branch: {unknown_retry}"
    );
    let after_unknown_retry = latest_host_completion_snapshot(&rollout_path)?;
    assert_eq!(
        after_unknown_retry["state"]["next_action"], retry_previous_plan,
        "a rejected retry leaves the host-registered next action unchanged"
    );
    assert_eq!(
        completion_unit(&after_unknown_retry, "review-a")["review_state"],
        review_state_before_unknown_retry,
        "an unknown attempt must not mutate review state or consume retry authorization"
    );

    let authorized_previous_plan = after_unknown_retry["state"]["next_action"]
        .as_str()
        .expect("rejected retry leaves a host-owned next action")
        .to_string();
    mount_function_turn(
        &server,
        &parent_thread_id,
        "recovery-authorize-retry",
        "task_context_boundary",
        &json!({
            "action":"plan_review",
            "root_task_id":"root",
            "previous_plan":authorized_previous_plan,
            "revised_plan":"Review task A against AC-1 again after replacing the failed reviewer result with a new host-authorized attempt.",
            "reason":"The first reviewer attempt failed; the exact same review obligation remains required and needs a fresh reviewer attempt.",
            "review_retry":{"unit_id":"review-a","expected_attempt_id":failed_attempt_id}
        }),
        "The parent authorized one new attempt for the same review obligation; the failed attempt remains recorded.",
    )
    .await;
    fixture
        .submit_turn("Record the changed plan and explicitly authorize one retry for the exact failed review attempt.")
        .await?;
    let authorized_tool_output = tool_output_text(&rollout_path, "recovery-authorize-retry")?
        .expect("plan-review returns the host completion state");
    let authorized_tool_state: Value = serde_json::from_str(&authorized_tool_output)?;
    let authorized_tool_unit = authorized_tool_state["units"]
        .as_array()
        .and_then(|units| units.iter().find(|unit| unit["id"] == "review-a"))
        .expect("plan-review tool output contains the review obligation");
    assert_eq!(
        authorized_tool_unit["review_state"]["active_attempt"]["attempt_id"], failed_attempt_id,
        "successful plan-review output retains the exact failed attempt"
    );
    assert_eq!(
        authorized_tool_unit["review_state"]["active_attempt"]["status"],
        "failed"
    );
    assert_eq!(
        authorized_tool_unit["review_state"]["retry_authorization"]["expected_attempt_id"],
        failed_attempt_id,
        "successful plan-review output grants exactly the currently failed attempt"
    );
    assert_eq!(
        authorized_tool_unit["review_state"]["retry_authorization"]["authorization_id"],
        "recovery-authorize-retry"
    );
    let authorized_snapshot = latest_host_completion_snapshot(&rollout_path)?;
    let authorized_retry_state =
        completion_unit(&authorized_snapshot, "review-a")["review_state"].clone();
    assert_eq!(
        authorized_retry_state["active_attempt"]["attempt_id"], failed_attempt_id,
        "the recorded retry grant must still be attached to the failed attempt"
    );
    assert_eq!(
        authorized_retry_state["retry_authorization"]["expected_attempt_id"], failed_attempt_id,
        "the grant must authorize exactly the currently recorded failed attempt"
    );

    let retry_parent_thread_id = parent_thread_id.clone();
    let retry_child_response = mount_sse_once_match(
        &server,
        move |request: &wiremock::Request| {
            is_completion_reviewer_request(request, &retry_parent_thread_id)
        },
        sse(vec![
            ev_response_created("recovery-reviewer-success"),
            ev_assistant_message(
                "recovery-reviewer-success-msg",
                "The accepted artifact matches AC-1.",
            ),
            ev_completed("recovery-reviewer-success"),
        ]),
    )
    .await;
    mount_function_turn(
        &server,
        &parent_thread_id,
        "recovery-review-retry",
        "task_context_boundary",
        &json!({"action":"review","target_task_id":"task-a","unit_id":"review-a"}),
        "The authorized new reviewer confirmed the artifact.",
    )
    .await;
    fixture
        .submit_turn("Run the authorized new attempt for review-a.")
        .await?;
    let retry_launch_output =
        tool_output_debug(&rollout_path, "recovery-review-retry")?.expect("retry launch output");
    let retry_launch = latest_host_completion_snapshot(&rollout_path)?;
    let retry_unit = completion_unit(&retry_launch, "review-a");
    let active_attempt = &retry_unit["review_state"]["active_attempt"];
    let active_attempt_id = active_attempt["attempt_id"]
        .as_str()
        .expect("retry must record a new attempt id");
    let active_attempt_status = active_attempt["status"].as_str().unwrap_or("<missing>");
    assert_ne!(
        active_attempt_id,
        failed_attempt_id.as_str(),
        "an authorized retry must create a new attempt instead of reusing the failed attempt"
    );
    let retry_thread_id = retry_unit["review_evidence"]["reviewer_thread_id"]
        .as_str()
        .unwrap_or("<missing>")
        .to_string();
    let retry_turn_id_snapshot = retry_unit["review_evidence"]["reviewer_turn_id"]
        .as_str()
        .unwrap_or("<missing>");
    let provider_requests = retry_child_response.requests();
    let provider_request_metadata = provider_requests
        .iter()
        .map(|request| {
            let body = request.body_json();
            serde_json::json!({
                "model": body.get("model"),
                "client_metadata": body.get("client_metadata"),
            })
        })
        .collect::<Vec<_>>();
    assert_ne!(
        retry_thread_id, "<missing>",
        "retry response={retry_launch_output:?}; active attempt id={active_attempt_id:?} status={active_attempt_status:?}; snapshot turn={retry_turn_id_snapshot:?}; provider requests={provider_request_metadata:?}"
    );
    assert_ne!(
        retry_thread_id, first_thread_id,
        "retry response={retry_launch_output:?}; active attempt id={active_attempt_id:?} status={active_attempt_status:?}; snapshot child={retry_thread_id:?} turn={retry_turn_id_snapshot:?}; provider requests={provider_request_metadata:?}"
    );
    assert_eq!(retry_unit["review_evidence"]["reviewer_role"], REVIEW_ROLE);
    assert!(retry_launch_output.contains(REVIEW_MODEL));
    assert!(retry_launch_output.contains(REVIEW_ROLE));
    let retry_requests = provider_requests
        .into_iter()
        .filter(|request| request.body_json()["client_metadata"]["thread_id"] == retry_thread_id)
        .collect::<Vec<_>>();
    assert_eq!(retry_requests.len(), 1, "one new reviewer dispatch only");
    assert_eq!(retry_requests[0].body_json()["model"], REVIEW_MODEL);
    assert_eq!(
        retry_requests[0].body_json()["client_metadata"]["turn_id"],
        retry_unit["review_evidence"]["reviewer_turn_id"]
    );
    assert_ne!(
        retry_requests[0].body_json()["client_metadata"]["turn_id"],
        first_turn_id
    );

    mount_multi_agent_turn(
        &server,
        &parent_thread_id,
        "recovery-wait-retry",
        "wait_agent",
        &json!({"targets":[retry_thread_id],"timeout_ms":30000}),
        "The successful retry result was collected.",
    )
    .await;
    fixture
        .submit_turn("Collect the successful retry review result.")
        .await?;
    let retry_wait_output = tool_output_debug(&rollout_path, "recovery-wait-retry")?
        .expect("successful retry wait output");
    assert!(
        retry_wait_output.contains("The accepted artifact matches AC-1."),
        "wait must collect the reply served to the retry child: {retry_wait_output}"
    );
    let retry_turn_id = retry_unit["review_evidence"]["reviewer_turn_id"]
        .as_str()
        .expect("retry reviewer turn")
        .to_string();
    assert_persisted_reviewer_identity(fixture.codex_home_path(), &retry_thread_id, &retry_turn_id)
        .await?;
    mount_function_turn(
        &server,
        &parent_thread_id,
        "recovery-adopt-retry",
        "task_context_boundary",
        &json!({"action":"adopt_review","unit_id":"review-a","reason":"The authorized reviewer confirmed the accepted artifact meets AC-1."}),
        "The new review is adopted.",
    )
    .await;
    fixture
        .submit_turn("Adopt the collected result from review-a.")
        .await?;

    let final_state = latest_host_completion_snapshot(&rollout_path)?;
    let adopted = completion_unit(&final_state, "review-a");
    assert_eq!(adopted["status"], "done");
    assert_eq!(
        adopted["review_state"]["active_attempt"]["attempt_id"], active_attempt_id,
        "adoption must retain the new successful attempt identity"
    );
    assert_eq!(adopted["review_evidence"]["reviewer_role"], REVIEW_ROLE);
    assert_eq!(
        adopted["review_evidence"]["reviewer_thread_id"],
        retry_thread_id
    );
    assert!(adopted["review_evidence"]["wait_result_ref"].is_string());
    let failed_wait =
        tool_output_debug(&rollout_path, "recovery-wait-first")?.expect("failed wait output");
    assert!(failed_wait.contains("Errored") || failed_wait.contains("error"));
    assert!(tool_output_debug(&rollout_path, "recovery-adopt-retry")?.is_some());
    Ok(())
}
