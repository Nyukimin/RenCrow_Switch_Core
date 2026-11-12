//! End to end parent task boundary through the regular model loop.
use anyhow::Result;
use codex_protocol::models::PermissionProfile;
use codex_protocol::models::ResponseItem;
use codex_rollout::RolloutItem;
use core_test_support::responses::ev_apply_patch_custom_tool_call;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use serde_json::json;
use std::fs;

fn function_response(id: &str, name: &str, args: &serde_json::Value) -> String {
    sse(vec![
        ev_response_created(&format!("response-{id}")),
        ev_function_call(id, name, &args.to_string()),
        ev_completed(&format!("response-{id}")),
    ])
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parent_task_boundary_runs_in_model_loop_and_survives_cold_resume() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let fixture = test_codex()
        .with_config(|config| {
            config.rencrow_compaction = true;
            config.rencrow_tool_allowlist = Some(vec![
                "exec_command".into(),
                "apply_patch".into(),
                "task_context_boundary".into(),
            ]);
        })
        .build(&server)
        .await?;
    let artifact = fixture.cwd_path().join("test_task_boundary_artifact.py");
    fs::write(
        &artifact,
        "import unittest\n\nARTIFACT_CONTENT = \"unverified artifact\"\n\nclass TestTaskBoundaryArtifact(unittest.TestCase):\n    def test_artifact_content(self):\n        self.assertEqual(ARTIFACT_CONTENT, \"verified artifact after patch\")\n",
    )?;
    let relative_artifact = artifact.strip_prefix(fixture.cwd_path())?.to_string_lossy();

    let begin = json!({
        "action":"begin", "root_task_id":"root-a", "task_id":"task-a", "task_revision":1
    });
    let patch = "*** Begin Patch\n*** Update File: test_task_boundary_artifact.py\n@@\n-ARTIFACT_CONTENT = \"unverified artifact\"\n+ARTIFACT_CONTENT = \"verified artifact after patch\"\n*** End Patch";
    let unverified_patch = "*** Begin Patch\n*** Add File: unverified-extra.txt\n+not included in the verification receipt\n*** End Patch";
    let evidence = json!({
        "cmd":"python3 -B -m unittest -v test_task_boundary_artifact",
        "task_evidence": {
            "task_id":"task-a", "task_revision":1,
            "artifact_paths":[relative_artifact], "check_kind":"verification"
        }
    });
    let accept = json!({
        "action":"accept", "goal":"Produce verified result A and continue with B",
        "tasks":[
            {"id":"task-a","purpose":"Verify the local artifact","status":"completed"},
            {"id":"task-b","purpose":"Continue the next task","status":"in_progress"}
        ],
        "current_task_id":"task-b", "decisions":["Keep the original user constraint"],
        "constraints":["Retain the user's formatting constraint"],
        "result":"A completed after artifact verification; continue B."
    });
    let begin_b = json!({
        "action":"begin", "root_task_id":"root-a", "task_id":"task-b", "task_revision":1
    });
    let evidence_b = json!({
        "cmd":"python3 -B -m unittest -v test_task_boundary_artifact",
        "task_evidence": {
            "task_id":"task-b", "task_revision":1,
            "artifact_paths":[relative_artifact], "check_kind":"verification"
        }
    });
    let accept_b = json!({
        "action":"accept", "goal":"Produce verified result A and B, then continue C",
        "tasks":[
            {"id":"task-a","purpose":"Verify the local artifact","status":"completed"},
            {"id":"task-b","purpose":"Finish the next task","status":"completed"},
            {"id":"task-c","purpose":"Continue the next task","status":"in_progress"}
        ],
        "current_task_id":"task-c", "decisions":["Keep the original user constraint"],
        "constraints":["Retain the user's formatting constraint"],
        "result":"B completed after artifact verification; continue C."
    });
    let responses = mount_sse_sequence(
        &server,
        vec![
            function_response("begin-call", "task_context_boundary", &begin),
            sse(vec![
                ev_response_created("patch-response"),
                ev_assistant_message(
                    "transient-progress",
                    "I inspected the source and assistantProgressMarker while implementing task A.",
                ),
                ev_apply_patch_custom_tool_call("patch-call", patch),
                ev_completed("patch-response"),
            ]),
            sse(vec![
                ev_response_created("extra-patch-response"),
                ev_apply_patch_custom_tool_call("patch-extra", unverified_patch),
                ev_completed("extra-patch-response"),
            ]),
            function_response(
                "unknown-effect-call",
                "exec_command",
                &json!({"cmd":"touch unknown-exec-effect.txt"}),
            ),
            function_response("evidence-call", "exec_command", &evidence),
            function_response("accept-call", "task_context_boundary", &accept),
            sse(vec![
                ev_response_created("normal-after-commit"),
                ev_assistant_message(
                    "normal-message",
                    "A is verified; continuing B while preserving the user constraint.",
                ),
                ev_completed("normal-after-commit"),
            ]),
            function_response("begin-b-call", "task_context_boundary", &begin_b),
            function_response("evidence-b-call", "exec_command", &evidence_b),
            function_response("accept-b-call", "task_context_boundary", &accept_b),
            sse(vec![
                ev_response_created("normal-after-second-commit"),
                ev_assistant_message("normal-second-message", "B is complete; continue C."),
                ev_completed("normal-after-second-commit"),
            ]),
            sse(vec![
                ev_response_created("cold-resume-normal"),
                core_test_support::responses::ev_assistant_message(
                    "cold-resume-message",
                    "Resumed with the saved task state.",
                ),
                ev_completed("cold-resume-normal"),
            ]),
        ],
    )
    .await;

    fixture
        .submit_turn_with_permission_profile(
            "Goal: finish A and continue B. Constraint: retain the user's formatting constraint.",
            PermissionProfile::Disabled,
        )
        .await?;
    let requests = responses.requests();
    let artifact_contents = fs::read_to_string(&artifact)?;
    assert_eq!(
        artifact_contents,
        "import unittest\n\nARTIFACT_CONTENT = \"verified artifact after patch\"\n\nclass TestTaskBoundaryArtifact(unittest.TestCase):\n    def test_artifact_content(self):\n        self.assertEqual(ARTIFACT_CONTENT, \"verified artifact after patch\")\n",
        "patch artifact mismatch; requests={:?}; rollout={}",
        requests
            .iter()
            .map(|request| request.body_json()["tools"].clone())
            .collect::<Vec<_>>(),
        fs::read_to_string(fixture.codex.rollout_path().expect("rollout path"))?
    );

    assert_eq!(
        requests.len(),
        7,
        "task boundary must not add a compaction LLM call; request bodies={:?}; rollout={}",
        requests
            .iter()
            .map(core_test_support::responses::ResponsesRequest::body_json)
            .collect::<Vec<_>>(),
        fs::read_to_string(fixture.codex.rollout_path().expect("rollout path"))?
    );
    assert!(requests.iter().all(|request| {
        let body = request.body_json();
        !body.to_string().contains("compaction_trigger")
            && !body["instructions"]
                .as_str()
                .unwrap_or_default()
                .contains("read-only compaction")
    }));
    let post_commit = requests.last().expect("post commit normal request");
    let request_text = post_commit.body_json().to_string();
    assert!(request_text.contains("Goal"));
    assert!(request_text.contains("A completed after artifact verification"));
    assert!(request_text.contains("B"));
    assert!(request_text.contains("formatting constraint"));
    assert!(request_text.contains("python3 -B -m unittest -v test_task_boundary_artifact"));
    let rollout_path = fixture.codex.rollout_path().expect("rollout path");
    let raw_source = fs::read_to_string(&rollout_path)?;
    assert!(raw_source.contains("begin-call"));
    assert!(raw_source.contains("accept-call"));
    assert!(raw_source.contains("patch-call"));
    assert!(raw_source.contains("patch-extra"));
    assert!(raw_source.contains("unknown-effect-call"));
    assert!(raw_source.contains("evidence-call"));
    assert!(raw_source.contains("assistantProgressMarker"));
    assert!(raw_source.contains("retain the user's formatting constraint"));
    let checkpoint = raw_source
        .lines()
        .filter_map(|line| codex_rollout::parse_rollout_line(line).ok())
        .filter_map(|entry| match entry.item {
            RolloutItem::Compacted(checkpoint) => Some(checkpoint),
            _ => None,
        })
        .next_back()
        .expect("persisted task boundary checkpoint");
    let replacement = checkpoint
        .replacement_history
        .as_ref()
        .expect("replacement history")
        .iter()
        .map(|row| serde_json::to_string(&(&row.item, &row.metadata)))
        .collect::<std::result::Result<Vec<_>, _>>()?
        .join("\n");
    assert!(replacement.contains("task_boundary"));
    assert!(
        !replacement.contains("assistantProgressMarker"),
        "special retained progress row: {}",
        replacement
            .lines()
            .find(|line| line.contains("assistantProgressMarker"))
            .unwrap_or("missing row")
    );
    let retained_patch_pair = checkpoint
        .replacement_history
        .as_ref()
        .unwrap()
        .iter()
        .filter_map(|row| match &row.item {
            ResponseItem::CustomToolCall { call_id, .. }
            | ResponseItem::CustomToolCallOutput { call_id, .. }
                if call_id == "patch-call" =>
            {
                Some(call_id.as_str())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        retained_patch_pair.is_empty(),
        "verified patch call/output pair was retained: {retained_patch_pair:?}"
    );
    assert!(replacement.contains("patch-extra"));
    assert!(replacement.contains("unknown-effect-call"));
    assert!(replacement.contains("unknown-exec-effect.txt"));
    assert!(replacement.contains("evidence-call"));
    assert!(replacement.contains("task_evidence"));
    if let Some(index) = request_text.find("assistantProgressMarker") {
        let start = index.saturating_sub(120);
        let end = (index + 160).min(request_text.len());
        let body = post_commit.body_json();
        let tool_outputs = body["input"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|item| item.get("output"))
            .filter_map(serde_json::Value::as_str)
            .collect::<Vec<_>>();
        panic!(
            "transient marker retained in post-commit request near: {}; tool outputs: {tool_outputs:?}",
            &request_text[start..end],
        );
    }
    assert!(
        requests[0].body_json()["tools"]
            .to_string()
            .contains("task_context_boundary")
    );

    fixture
        .submit_turn_with_permission_profile(
            "Continue task B and then C; preserve the original formatting constraint.",
            PermissionProfile::Disabled,
        )
        .await?;
    let after_second = responses.requests();
    assert_eq!(after_second.len(), 11);
    let second_normal_request = after_second.last().unwrap().body_json().to_string();
    assert!(second_normal_request.contains("B completed after artifact verification"));
    assert!(second_normal_request.contains("Continue the next task"));
    assert_eq!(second_normal_request.matches("task_context_v1").count(), 1);
    let raw_source = fs::read_to_string(&rollout_path)?;
    let second_checkpoint = raw_source
        .lines()
        .filter_map(|line| codex_rollout::parse_rollout_line(line).ok())
        .filter_map(|entry| match entry.item {
            RolloutItem::Compacted(checkpoint) => Some(checkpoint),
            _ => None,
        })
        .next_back()
        .expect("second persisted task boundary checkpoint");
    let second_replacement = second_checkpoint
        .replacement_history
        .as_ref()
        .expect("second replacement history")
        .iter()
        .map(|row| serde_json::to_string(&(&row.item, &row.metadata)))
        .collect::<std::result::Result<Vec<_>, _>>()?
        .join("\n");
    assert_eq!(second_replacement.matches("compaction.summary").count(), 1);

    let resumed = test_codex()
        .with_config(|config| {
            config.rencrow_compaction = true;
            config.rencrow_tool_allowlist = Some(vec![
                "exec_command".into(),
                "apply_patch".into(),
                "task_context_boundary".into(),
            ]);
        })
        .restart(&server, &fixture)
        .await?;
    resumed
        .submit_text_turn("Continue B after restart.")
        .await?;
    let requests = responses.requests();
    assert_eq!(requests.len(), 12);
    assert!(requests.iter().all(|request| {
        let body = request.body_json();
        !body.to_string().contains("compaction_trigger")
            && !body["instructions"]
                .as_str()
                .unwrap_or_default()
                .contains("read-only compaction")
    }));
    let resumed_text = requests.last().unwrap().body_json().to_string();
    assert!(resumed_text.contains("B completed after artifact verification"));
    assert!(resumed_text.contains("formatting constraint"));
    assert!(!resumed_text.contains("transientWork"));

    assert!(raw_source.contains("test_task_boundary_artifact.py"));
    Ok(())
}
