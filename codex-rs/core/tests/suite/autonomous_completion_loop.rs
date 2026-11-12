//! Autonomous completion claims bind to real accepted task receipts and reopen when stale.
use anyhow::Result;
use codex_config::RencrowExecutionGuard;
use codex_core::config::AgentRoleConfig;
use codex_features::Feature;
use codex_protocol::models::PermissionProfile;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AgentStatus;
use codex_rollout::RolloutItem;
use core_test_support::responses::ev_apply_patch_custom_tool_call;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once_match;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use serde_json::Value;
use serde_json::json;
use std::fs;

const REVIEW_MODEL: &str = "gpt-5.6-sol";
const REVIEW_ROLE: &str = "reviewer";

fn function_response(call_id: &str, name: &str, args: &Value) -> String {
    sse(vec![
        ev_response_created(&format!("response-{call_id}")),
        ev_function_call(call_id, name, &args.to_string()),
        ev_completed(&format!("response-{call_id}")),
    ])
}

fn request_contains_text(request: &wiremock::Request, text: &str) -> bool {
    String::from_utf8_lossy(&request.body).contains(text)
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
        "changed after verification\n"
    );
    let rollout_path = fixture.codex.rollout_path().expect("rollout path");
    let rejected_accept = tool_output_debug(&rollout_path, "accept-a-rejected")?
        .expect("first acceptance attempt output");
    assert!(
        rejected_accept.contains("unresolved verification obligation"),
        "unrelated success must not resolve the first failure: {rejected_accept}"
    );
    let snapshots = host_completion_snapshots(&rollout_path)?;
    let satisfied_snapshots = snapshots
        .iter()
        .filter(|snapshot| snapshot["state"]["conditions"][0]["status"] == "satisfied")
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
    let latest = snapshots.last().expect("stale update persisted");
    assert_eq!(latest["state"]["conditions"][0]["status"], "open");
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
    assert_eq!(
        evidence_ids(latest),
        evidence_ids(satisfied_snapshots.last().unwrap()),
        "stale reopening retains the previous proof set independent of evidence ordering"
    );
    assert!(
        latest["state"]["conditions"][0]["unmet_reason"]
            .as_str()
            .unwrap()
            .contains("artifact")
    );
    let task_unit_completions = latest["state"]["units_completed_since_review"]
        .as_u64()
        .unwrap();
    assert_eq!(
        task_unit_completions, 0,
        "plan review resets its own counter"
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
async fn planned_review_runs_through_host_spawn_wait_and_adoption() -> Result<()> {
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
            ]);
            config.features.enable(Feature::Collab).unwrap();
            let role_path = config.codex_home.join("completion-reviewer.toml");
            fs::write(
                &role_path,
                format!(
                    "model = \"{REVIEW_MODEL}\"\nrencrow_read_only = true\nmodel_provider = \"mock\"\n\n[rencrow_execution_guard]\ntask_kind = \"read_only\"\n\n[model_providers.mock]\nname = \"mock\"\nbase_url = \"{provider_url}\"\nenv_key = \"PATH\"\nwire_api = \"responses\"\n"
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
    let artifact = fixture.cwd_path().join("review-artifact.txt");
    fs::write(&artifact, "accepted artifact\n")?;
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
            {"id":"task-b", "purpose":"Continue after review A", "status":"planned", "condition_refs":["AC-1"], "depends_on":["review-a"], "evidence":[], "task_refs":[]},
            {"id":"review-b", "purpose":"Review task B against AC-1", "status":"planned", "kind":"review", "condition_refs":["AC-1"], "depends_on":["task-b"], "evidence":[], "task_refs":[], "review_target":{"task_id":"task-b","task_revision":1}}
        ],
        "current_unit":"task-a", "failed_approaches":[], "blocker":null, "next_action":"Begin task A",
        "product_progress_at":null, "runtime_recovery_at":null, "activity_at":null
    });
    mount_sse_sequence(
        &server,
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

    mount_sse_sequence(
        &server,
        vec![
            function_response("review-begin-a", "task_context_boundary", &json!({"action":"begin","root_task_id":"root","task_id":"task-a","task_revision":1})),
            function_response("review-check-a", "exec_command", &json!({"cmd":"cat review-artifact.txt","task_evidence":{"task_id":"task-a","task_revision":1,"artifact_paths":[relative_artifact],"check_kind":"verification"}})),
            function_response("review-accept-a", "task_context_boundary", &json!({"action":"accept","result":"Task A artifact verified","satisfied_conditions":["AC-1"]})),
            sse(vec![ev_response_created("review-a-accepted"), ev_assistant_message("review-a-accepted-message", "Task A is accepted; required review A remains."), ev_completed("review-a-accepted")]),
        ],
    )
    .await;
    fixture.submit_turn("Verify and accept task A.").await?;

    let parent_thread_id = fixture.session_configured.thread_id.to_string();
    let child_response = mount_sse_once_match(
        &server,
        move |request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
            let is_agent_message = body["input"]
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item["type"] == "agent_message"));
            request_contains_text(request, "Host-assigned read-only review")
                && request_contains_text(request, "Expected content is present")
                && request_contains_text(request, "review-artifact.txt")
                && is_agent_message
                && body["client_metadata"]["thread_id"].as_str() != Some(parent_thread_id.as_str())
        },
        sse(vec![
            ev_response_created("reviewer-turn-1"),
            ev_assistant_message("reviewer-result-1", "The accepted artifact matches AC-1."),
            ev_completed("reviewer-turn-1"),
        ]),
    )
    .await;
    mount_sse_sequence(
        &server,
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
    let rollout_path = fixture.codex.rollout_path().expect("rollout path");
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
    assert_eq!(
        reviewer_requests.len(),
        1,
        "review launches exactly the host-returned child thread {reviewer_thread_id}; tool output: {review_output}"
    );
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

    mount_sse_sequence(
        &server,
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

    mount_sse_sequence(
        &server,
        vec![
            function_response("review-adopt-a", "task_context_boundary", &json!({"action":"adopt_review","unit_id":"review-a","reason":"The reviewer confirmed the accepted artifact meets AC-1."})),
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
