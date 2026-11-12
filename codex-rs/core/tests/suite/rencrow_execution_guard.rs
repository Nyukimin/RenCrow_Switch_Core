// Modified by RenCrow: exercise native enforcement through the real Responses/tool loop.
use anyhow::Result;
use codex_config::RencrowExecutionGuard;
use codex_config::RencrowTaskKind;
use codex_core::StartThreadOptions;
use codex_core::TurnInputRequest;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_apply_patch_custom_tool_call;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::num::NonZeroU16;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_patch_retries_distinguish_syntax_from_markdown_context_failure() -> Result<()> {
    let server = start_mock_server().await;
    let syntax_patch =
        "*** Begin Patch\n*** Update File: NOTES.md\n@@\n-- current item\n+- updated item";
    // This syntactically valid hunk omits the Markdown `- ` from the source line.
    let context_patch = "*** Begin Patch\n*** Update File: NOTES.md\n@@\n-current item\n+updated item\n*** End Patch";
    let requests = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("patch-syntax"),
                ev_apply_patch_custom_tool_call("patch-syntax-call", syntax_patch),
                ev_completed("patch-syntax"),
            ]),
            sse(vec![
                ev_response_created("patch-context"),
                ev_apply_patch_custom_tool_call("patch-context-call", context_patch),
                ev_completed("patch-context"),
            ]),
            sse(vec![
                ev_response_created("patch-errors-reviewed"),
                ev_assistant_message(
                    "patch-errors-reviewed-message",
                    "I will reread the current Markdown line before retrying.",
                ),
                ev_completed("patch-errors-reviewed"),
            ]),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.rencrow_execution_guard = Some(RencrowExecutionGuard::default());
        })
        .build_with_auto_env(&server)
        .await?;
    let notes = test.cwd_path().join("NOTES.md");
    std::fs::write(&notes, "- current item\n")?;
    test.codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: "Update the Markdown list item".into(),
                text_elements: vec![],
            }])
            .with_thread_settings(
                codex_protocol::protocol::ThreadSettingsOverrides {
                    approval_policy: Some(codex_protocol::protocol::AskForApproval::Never),
                    permission_profile: Some(
                        codex_protocol::models::PermissionProfile::workspace_write(),
                    ),
                    ..Default::default()
                },
            ),
        )
        .await?;

    let event = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let EventMsg::TurnComplete(done) = event else {
        unreachable!()
    };
    assert_eq!(done.error, None);
    let calls = requests.requests();
    assert_eq!(calls.len(), 3);
    assert!(calls[1].body_contains_text("RENCROW_PATCH_FORMAT_HINT"));
    assert!(calls[2].body_contains_text("RENCROW_PATCH_CONTEXT_HINT"));
    assert!(calls[2].body_contains_text("inspect the file"));
    assert!(calls[2].body_contains_text("Markdown line prefixes such as `- `"));
    assert_eq!(std::fs::read_to_string(notes)?, "- current item\n");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn implementation_rejects_obvious_shell_edits_before_execution() -> Result<()> {
    let server = start_mock_server().await;
    let patch = "*** Begin Patch\n*** Add File: value.py\n+VALUE = 7\n*** Add File: test_native_edit_guard.py\n+import unittest\n+from value import VALUE\n+\n+class TestValue(unittest.TestCase):\n+    def test_value(self):\n+        self.assertEqual(VALUE, 7)\n*** End Patch";
    let requests = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("edit-cp"),
                ev_function_call("edit-cp", "exec_command", &json!({"cmd":"cp seed.txt copied.txt"}).to_string()),
                ev_completed("edit-cp"),
            ]),
            sse(vec![
                ev_response_created("edit-python"),
                ev_function_call("edit-python", "exec_command", &json!({"cmd":"python3 -c 'from pathlib import Path; Path(\"python-write.txt\").write_text(\"changed\\n\")'"}).to_string()),
                ev_completed("edit-python"),
            ]),
            sse(vec![
                ev_response_created("edit-cat"),
                ev_function_call("edit-cat", "exec_command", &json!({"cmd":"cat <<'EOF' >> cat-append.txt\nappended\nEOF"}).to_string()),
                ev_completed("edit-cat"),
            ]),
            sse(vec![
                ev_response_created("edit-heredoc-python"),
                ev_function_call("edit-heredoc-python", "exec_command", &json!({"cmd":"python3 - <<'PY'\nfrom pathlib import Path\nPath('heredoc-write.txt').write_text('changed')\nPY"}).to_string()),
                ev_completed("edit-heredoc-python"),
            ]),
            sse(vec![
                ev_response_created("edit-patch"),
                ev_apply_patch_custom_tool_call("edit-patch", patch),
                ev_completed("edit-patch"),
            ]),
            sse(vec![
                ev_response_created("edit-check"),
                ev_function_call("edit-check", "exec_command", &json!({"cmd":"python3 -B -m unittest test_native_edit_guard 2>&1"}).to_string()),
                ev_completed("edit-check"),
            ]),
            sse(vec![
                ev_assistant_message("edit-done", "Edits applied with apply_patch and checked directly."),
                ev_completed("edit-finish"),
            ]),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.rencrow_execution_guard = Some(RencrowExecutionGuard {
                require_edit: true,
                require_check: true,
                ..Default::default()
            });
        })
        .build_with_auto_env(&server)
        .await?;
    std::fs::write(test.cwd_path().join("seed.txt"), "seed\n")?;
    std::fs::write(test.cwd_path().join("cat-append.txt"), "original\n")?;
    test.codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: "Implement and check the requested change".into(),
                text_elements: vec![],
            }])
            .with_thread_settings(
                codex_protocol::protocol::ThreadSettingsOverrides {
                    approval_policy: Some(codex_protocol::protocol::AskForApproval::Never),
                    permission_profile: Some(
                        codex_protocol::models::PermissionProfile::workspace_write(),
                    ),
                    ..Default::default()
                },
            ),
        )
        .await?;
    let event = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let EventMsg::TurnComplete(done) = event else {
        unreachable!()
    };
    let calls = requests.requests();
    assert_eq!(calls.len(), 7);
    for (index, call_id) in [
        (1, "edit-cp"),
        (2, "edit-python"),
        (3, "edit-cat"),
        (4, "edit-heredoc-python"),
    ] {
        let output = calls[index].function_call_output(call_id).to_string();
        assert!(
            output.contains("RENCROW_EDIT_METHOD_REQUIRED"),
            "shell edit {call_id} was not rejected before execution: {output}"
        );
    }
    assert!(!test.cwd_path().join("copied.txt").exists());
    assert!(!test.cwd_path().join("python-write.txt").exists());
    assert!(!test.cwd_path().join("heredoc-write.txt").exists());
    assert_eq!(
        std::fs::read_to_string(test.cwd_path().join("cat-append.txt"))?,
        "original\n"
    );
    assert_eq!(
        std::fs::read_to_string(test.cwd_path().join("value.py"))?,
        "VALUE = 7\n"
    );
    let check_output = calls[6].function_call_output("edit-check").to_string();
    assert!(
        check_output.contains("OK"),
        "direct check did not pass: {check_output}"
    );
    assert_eq!(done.error, None, "guarded implementation should complete");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn embedded_check_must_run_as_a_direct_shell_invocation() -> Result<()> {
    let server = start_mock_server().await;
    let patch = "*** Begin Patch\n*** Add File: value.py\n+VALUE = 7\n*** Add File: test_direct_check_guard.py\n+import unittest\n+from value import VALUE\n+\n+class TestDirectCheck(unittest.TestCase):\n+    def test_value(self):\n+        self.assertEqual(VALUE, 7)\n*** End Patch";
    let requests = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("direct-check-patch"),
                ev_apply_patch_custom_tool_call("direct-check-patch", patch),
                ev_completed("direct-check-patch"),
            ]),
            sse(vec![
                ev_response_created("wrapped-check"),
                ev_function_call(
                    "wrapped-check",
                    "exec_command",
                    &json!({"cmd":"cd . && cat value.py && python3 -B -m unittest -q test_direct_check_guard"}).to_string(),
                ),
                ev_completed("wrapped-check"),
            ]),
            sse(vec![
                ev_response_created("direct-check"),
                ev_function_call(
                    "direct-check",
                    "exec_command",
                    &json!({"cmd":"python3 -B -m unittest -q test_direct_check_guard"}).to_string(),
                ),
                ev_completed("direct-check"),
            ]),
            sse(vec![
                ev_assistant_message("direct-check-done", "Applied the patch and ran the check directly."),
                ev_completed("direct-check-finish"),
            ]),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.rencrow_execution_guard = Some(RencrowExecutionGuard {
                require_edit: true,
                require_check: true,
                ..Default::default()
            });
        })
        .build_with_auto_env(&server)
        .await?;
    test.codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: "Implement and verify the requested change".into(),
                text_elements: vec![],
            }])
            .with_thread_settings(
                codex_protocol::protocol::ThreadSettingsOverrides {
                    approval_policy: Some(codex_protocol::protocol::AskForApproval::Never),
                    permission_profile: Some(
                        codex_protocol::models::PermissionProfile::workspace_write(),
                    ),
                    ..Default::default()
                },
            ),
        )
        .await?;
    let event = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let EventMsg::TurnComplete(done) = event else {
        unreachable!()
    };
    let calls = requests.requests();
    assert_eq!(calls.len(), 4);
    let wrapped_output = calls[2].function_call_output("wrapped-check").to_string();
    assert!(
        wrapped_output.contains("RENCROW_DIRECT_CHECK_REQUIRED"),
        "compound check was not refused before execution: {wrapped_output}"
    );
    let check_output = calls[3].function_call_output("direct-check").to_string();
    assert!(
        check_output.contains("Ran 1 test") && check_output.contains("OK"),
        "direct check did not pass: {check_output}"
    );
    assert!(
        done.error.is_none(),
        "directly verified turn failed: {done:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_read_is_rejected_without_external_hooks() -> Result<()> {
    let server = start_mock_server().await;
    let call = |id: &str| {
        sse(vec![
            ev_response_created(id),
            ev_function_call(id, "exec_command", &json!({"cmd":"pwd"}).to_string()),
            ev_completed(id),
        ])
    };
    let requests = mount_sse_sequence(
        &server,
        vec![
            call("read-1"),
            call("read-2"),
            sse(vec![
                ev_assistant_message("done", "Read budget observed"),
                ev_completed("done"),
            ]),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.rencrow_execution_guard = Some(RencrowExecutionGuard {
                max_repeated_reads: NonZeroU16::new(1).unwrap(),
                ..Default::default()
            });
        })
        .build_with_auto_env(&server)
        .await?;
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Inspect working directory".into(),
            text_elements: vec![],
        }]))
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let calls = requests.requests();
    let rejected = calls[2].function_call_output("read-2").to_string();
    assert!(
        rejected.contains("RENCROW_READ_BUDGET_EXCEEDED"),
        "{rejected}"
    );
    assert!(
        !calls[1]
            .function_call_output("read-1")
            .to_string()
            .contains("RENCROW_READ_BUDGET_EXCEEDED")
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn distinct_orchestrator_reads_can_exceed_legacy_total_limit() -> Result<()> {
    let server = start_mock_server().await;
    let mut responses = Vec::new();
    for index in 0..25 {
        let id = format!("orchestrate-read-{index}");
        let command = format!("echo inspect-{index}");
        responses.push(sse(vec![
            ev_response_created(&id),
            ev_function_call(&id, "exec_command", &json!({"cmd":command}).to_string()),
            ev_completed(&id),
        ]));
    }
    responses.push(sse(vec![
        ev_assistant_message("orchestrate-done", "Evidence-backed investigation plan"),
        ev_completed("orchestrate-done"),
    ]));
    let requests = mount_sse_sequence(&server, responses).await;
    let test = test_codex()
        .with_config(|config| {
            config.rencrow_execution_guard = Some(RencrowExecutionGuard {
                max_read_only_calls: NonZeroU16::new(1).unwrap(),
                task_kind: Some(RencrowTaskKind::Orchestrate),
                ..Default::default()
            });
        })
        .build_with_auto_env(&server)
        .await?;
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Inspect distinct sources and report a plan without implementing it".into(),
            text_elements: vec![],
        }]))
        .await?;
    let event = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let calls = requests.requests();
    assert!(matches!(event, EventMsg::TurnComplete(_)));
    assert_eq!(calls.len(), 26);
    for index in 0..25 {
        let output = calls[index + 1]
            .function_call_output(&format!("orchestrate-read-{index}"))
            .to_string();
        assert!(!output.contains("RENCROW_READ_BUDGET_EXCEEDED"), "{output}");
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_read_budget_refusals_stop_the_turn_without_another_request() -> Result<()> {
    let server = start_mock_server().await;
    let call = |id: &str| {
        sse(vec![
            ev_response_created(id),
            ev_function_call(id, "exec_command", &json!({"cmd":"pwd"}).to_string()),
            ev_completed(id),
        ])
    };
    let requests = mount_sse_sequence(
        &server,
        vec![
            call("read-1"),
            call("read-2"),
            call("read-3"),
            call("read-4"),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.rencrow_execution_guard = Some(RencrowExecutionGuard {
                max_repeated_reads: NonZeroU16::new(1).unwrap(),
                ..Default::default()
            });
        })
        .build_with_auto_env(&server)
        .await?;
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Repeat a read until the host stops the turn".into(),
            text_elements: vec![],
        }]))
        .await?;
    let event = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let EventMsg::TurnComplete(done) = event else {
        unreachable!()
    };
    let error = done
        .error
        .expect("repeated guard refusals must stop the turn");
    assert!(
        error
            .message
            .contains("repeated host refusals stopped the turn")
    );
    assert_eq!(requests.requests().len(), 4);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_apply_patch_verification_failures_stop_the_turn() -> Result<()> {
    let server = start_mock_server().await;
    let invalid_patch = "*** Begin Patch\nnot a patch hunk\n*** End Patch";
    let call = |id: &str| {
        sse(vec![
            ev_response_created(id),
            ev_apply_patch_custom_tool_call(id, invalid_patch),
            ev_completed(id),
        ])
    };
    let requests = mount_sse_sequence(
        &server,
        vec![call("patch-1"), call("patch-2"), call("patch-3")],
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.rencrow_execution_guard = Some(RencrowExecutionGuard::default());
        })
        .build_with_auto_env(&server)
        .await?;
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Apply the requested patch".into(),
            text_elements: vec![],
        }]))
        .await?;
    let event = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let EventMsg::TurnComplete(done) = event else {
        unreachable!()
    };
    let error = done
        .error
        .expect("repeated patch verification failures must stop the turn");
    assert!(error.message.contains("RENCROW_PATCH_RETRY_EXCEEDED"));
    assert_eq!(requests.requests().len(), 3);
    let second_output = requests.requests()[2]
        .custom_tool_call_output("patch-2")
        .to_string();
    assert!(second_output.contains("RENCROW_PATCH_FORMAT_HINT"));
    assert!(second_output.contains("never unified-diff line ranges such as @@ -1 +1 @@"));
    for example_line in [
        "Example:",
        "*** Begin Patch",
        "*** Update File: path",
        "@@",
        "-old",
        "+new",
        "*** End Patch",
    ] {
        assert!(
            second_output.contains(example_line),
            "missing {example_line:?}"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_patch_context_mismatch_guides_exact_source_lines_without_format_hint() -> Result<()>
{
    let server = start_mock_server().await;
    let context_mismatch = "*** Begin Patch\n*** Update File: Cargo.toml\n@@\n-this line is not present in Cargo.toml\n+replacement\n*** End Patch";
    let requests = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("context-mismatch"),
                ev_apply_patch_custom_tool_call("context-mismatch-call", context_mismatch),
                ev_completed("context-mismatch"),
            ]),
            sse(vec![
                ev_assistant_message(
                    "context-mismatch-recovered",
                    "The patch context was rejected.",
                ),
                ev_completed("context-mismatch-recovered"),
            ]),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.rencrow_execution_guard = Some(RencrowExecutionGuard::default());
        })
        .build_with_auto_env(&server)
        .await?;
    std::fs::write(
        test.cwd_path().join("Cargo.toml"),
        "[package]\nname = \"patch-context-fixture\"\n",
    )?;
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Apply the requested patch".into(),
            text_elements: vec![],
        }]))
        .await?;
    let event = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let EventMsg::TurnComplete(done) = event else {
        unreachable!()
    };
    assert!(
        done.error.is_none(),
        "turn should complete after patch rejection: {done:?}"
    );

    let calls = requests.requests();
    assert_eq!(
        calls.len(),
        2,
        "tool rejection should be returned for model review"
    );
    let output = calls[1]
        .custom_tool_call_output("context-mismatch-call")
        .to_string();
    assert!(
        output.contains("RENCROW_PATCH_CONTEXT_HINT"),
        "expected a context mismatch hint for an existing Cargo.toml: {output}"
    );
    assert!(
        !output.contains("RENCROW_PATCH_FORMAT_HINT"),
        "a valid patch with mismatched source should not receive a format hint: {output}"
    );
    assert!(output.contains("exact existing source line"), "{output}");
    assert!(
        output.contains("Markdown line prefixes such as `- `"),
        "{output}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accepted_patch_and_successful_check_reset_patch_failure_budget() -> Result<()> {
    let server = start_mock_server().await;
    let invalid_patch = "*** Begin Patch\nnot a patch hunk\n*** End Patch";
    let valid_patch = "*** Begin Patch\n*** Add File: test_patch_retry.py\n+import unittest\n+\n+class TestPatchRetry(unittest.TestCase):\n+    def test_patch_was_applied(self):\n+        self.assertEqual(2 + 2, 4)\n*** End Patch";
    let requests = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("retry-1"),
                ev_apply_patch_custom_tool_call("invalid-1", invalid_patch),
                ev_apply_patch_custom_tool_call("invalid-2", invalid_patch),
                ev_completed("retry-1"),
            ]),
            sse(vec![
                ev_response_created("retry-2"),
                ev_apply_patch_custom_tool_call("valid-patch", valid_patch),
                ev_completed("retry-2"),
            ]),
            sse(vec![
                ev_response_created("retry-3"),
                ev_function_call(
                    "patch-check",
                    "exec_command",
                    &json!({"cmd":"python3 -B -m unittest 2>&1"}).to_string(),
                ),
                ev_completed("retry-3"),
            ]),
            sse(vec![
                ev_assistant_message("retry-done", "Patch and check passed"),
                ev_completed("retry-4"),
            ]),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.rencrow_execution_guard = Some(RencrowExecutionGuard {
                require_edit: true,
                require_check: true,
                ..Default::default()
            });
        })
        .build_with_auto_env(&server)
        .await?;
    test.codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: "Correct the patch and verify it".into(),
                text_elements: vec![],
            }])
            .with_thread_settings(
                codex_protocol::protocol::ThreadSettingsOverrides {
                    approval_policy: Some(codex_protocol::protocol::AskForApproval::Never),
                    permission_profile: Some(
                        codex_protocol::models::PermissionProfile::workspace_write(),
                    ),
                    ..Default::default()
                },
            ),
        )
        .await?;
    let event = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let EventMsg::TurnComplete(done) = event else {
        unreachable!()
    };
    assert!(
        done.error.is_none(),
        "patch recovery turn should complete: {done:?}"
    );
    let calls = requests.requests();
    assert_eq!(calls.len(), 4);
    for call_id in ["invalid-1", "invalid-2"] {
        assert!(
            calls[1]
                .custom_tool_call_output(call_id)
                .to_string()
                .contains("RENCROW_PATCH_FORMAT_HINT")
        );
    }
    let check_output = calls[3].function_call_output("patch-check").to_string();
    assert!(
        check_output.contains("OK"),
        "successful unittest output was not returned: {check_output}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn orchestrator_allows_handoff_turn_after_implementation_turn_in_same_session() -> Result<()>
{
    use core_test_support::responses::ev_apply_patch_custom_tool_call;
    let server = start_mock_server().await;
    let requests = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_assistant_message("handoff-first", "Prepared the host handoff."),
                ev_completed("handoff-first"),
            ]),
            sse(vec![
                ev_response_created("implementation"),
                ev_apply_patch_custom_tool_call(
                    "implementation-patch",
                    "*** Begin Patch\n*** Add File: guard_orchestrator_turn.py\n+VALUE = 42\n*** End Patch",
                ),
                ev_completed("implementation"),
            ]),
            sse(vec![
                ev_response_created("implementation-check"),
                ev_function_call(
                    "implementation-check-call",
                    "exec_command",
                    &json!({"cmd":"python3 -B -m py_compile guard_orchestrator_turn.py"}).to_string(),
                ),
                ev_completed("implementation-check"),
            ]),
            sse(vec![
                ev_assistant_message("implementation-done", "Implemented and checked."),
                ev_completed("implementation-done"),
            ]),
            sse(vec![
                ev_response_created("verify-only"),
                ev_function_call(
                    "verify-only-check",
                    "exec_command",
                    &json!({"cmd":"python3 -B -m py_compile guard_orchestrator_turn.py"}).to_string(),
                ),
                ev_completed("verify-only"),
            ]),
            sse(vec![
                ev_assistant_message("verify-only-done", "Verified the implementation."),
                ev_completed("verify-only-done"),
            ]),
            sse(vec![
                ev_assistant_message("handoff-second", "Ready for host import."),
                ev_completed("handoff-second"),
            ]),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.rencrow_execution_guard = Some(RencrowExecutionGuard {
                task_kind: Some(codex_config::RencrowTaskKind::Orchestrate),
                ..Default::default()
            });
        })
        .build_with_auto_env(&server)
        .await?;
    let settings = codex_protocol::protocol::ThreadSettingsOverrides {
        approval_policy: Some(codex_protocol::protocol::AskForApproval::Never),
        permission_profile: Some(codex_protocol::models::PermissionProfile::workspace_write()),
        ..Default::default()
    };
    for (text, expected_message) in [
        ("Prepare host handoff", "Prepared the host handoff."),
        ("Implement the requested change", "Implemented and checked."),
        (
            "Verify the implementation without editing",
            "Verified the implementation.",
        ),
        (
            "Prepare host handoff after implementation",
            "Ready for host import.",
        ),
    ] {
        test.codex
            .start_or_steer_turn(
                TurnInputRequest::user_input(vec![UserInput::Text {
                    text: text.into(),
                    text_elements: vec![],
                }])
                .with_thread_settings(settings.clone()),
            )
            .await?;
        let event = wait_for_event(&test.codex, |event| {
            matches!(event, EventMsg::TurnComplete(_))
        })
        .await;
        let EventMsg::TurnComplete(done) = event else {
            unreachable!()
        };
        assert_eq!(done.error, None, "turn {text:?} should complete");
        assert_eq!(done.last_agent_message.as_deref(), Some(expected_message));
    }
    assert_eq!(requests.requests().len(), 7);
    assert_eq!(
        std::fs::read_to_string(test.cwd_path().join("guard_orchestrator_turn.py"))?,
        "VALUE = 42\n"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn redirected_failed_check_keeps_its_nonzero_exit_status() -> Result<()> {
    let server = start_mock_server().await;
    let patch = "*** Begin Patch\n*** Add File: test_failed_check.py\n+import unittest\n+\n+class FailedCheck(unittest.TestCase):\n+    def test_failure(self):\n+        self.assertEqual(1, 2)\n*** End Patch";
    let requests = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("patch"),
                ev_apply_patch_custom_tool_call("patch", patch),
                ev_completed("patch"),
            ]),
            sse(vec![
                ev_response_created("check"),
                ev_function_call(
                    "check",
                    "exec_command",
                    &json!({"cmd":"python3 -B -m unittest test_failed_check 2>&1"}).to_string(),
                ),
                ev_completed("check"),
            ]),
            sse(vec![
                ev_assistant_message("done", "Check ran"),
                ev_completed("done"),
            ]),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.rencrow_execution_guard = Some(RencrowExecutionGuard {
                require_edit: true,
                require_check: true,
                ..Default::default()
            });
        })
        .build_with_auto_env(&server)
        .await?;
    test.codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: "Run the check".into(),
                text_elements: vec![],
            }])
            .with_thread_settings(
                codex_protocol::protocol::ThreadSettingsOverrides {
                    approval_policy: Some(codex_protocol::protocol::AskForApproval::Never),
                    permission_profile: Some(
                        codex_protocol::models::PermissionProfile::workspace_write(),
                    ),
                    ..Default::default()
                },
            ),
        )
        .await?;
    let mut exit_code = None;
    let event = wait_for_event(&test.codex, |event| {
        if let EventMsg::ExecCommandEnd(end) = event {
            exit_code = Some(end.exit_code);
        }
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let EventMsg::TurnComplete(done) = event else {
        unreachable!()
    };
    assert_eq!(exit_code, Some(1));
    assert!(done.error.is_some());
    assert_eq!(requests.requests().len(), 3);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compound_read_with_display_command_is_metered() -> Result<()> {
    let server = start_mock_server().await;
    let call = |id: &str| {
        sse(vec![
            ev_response_created(id),
            ev_function_call(
                id,
                "exec_command",
                &json!({"cmd":"pwd; echo separator"}).to_string(),
            ),
            ev_completed(id),
        ])
    };
    let requests = mount_sse_sequence(
        &server,
        vec![
            call("read-1"),
            call("read-2"),
            sse(vec![
                ev_assistant_message("done", "Read budget observed"),
                ev_completed("done"),
            ]),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.rencrow_execution_guard = Some(RencrowExecutionGuard {
                max_repeated_reads: NonZeroU16::new(1).unwrap(),
                ..Default::default()
            });
        })
        .build_with_auto_env(&server)
        .await?;
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Inspect working directory".into(),
            text_elements: vec![],
        }]))
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let calls = requests.requests();
    let rejected = calls[2].function_call_output("read-2").to_string();
    assert!(
        rejected.contains("RENCROW_READ_BUDGET_EXCEEDED"),
        "{rejected}"
    );
    assert!(
        !calls[1]
            .function_call_output("read-1")
            .to_string()
            .contains("RENCROW_READ_BUDGET_EXCEEDED")
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prose_only_implementation_finishes_as_error_not_completed() -> Result<()> {
    let server = start_mock_server().await;
    mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("empty"),
            ev_assistant_message("message", "I will implement this next."),
            ev_completed("empty"),
        ]),
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.rencrow_execution_guard = Some(RencrowExecutionGuard {
                require_edit: true,
                require_check: true,
                ..Default::default()
            });
        })
        .build_with_auto_env(&server)
        .await?;
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Implement the requested change".into(),
            text_elements: vec![],
        }]))
        .await?;
    let event = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let EventMsg::TurnComplete(done) = event else {
        unreachable!()
    };
    let error = done.error.expect("missing evidence must reject completion");
    assert!(
        error
            .message
            .contains("RENCROW_EXECUTION_CONTRACT_INCOMPLETE")
    );
    assert!(matches!(
        test.codex.agent_status().await,
        AgentStatus::Errored(_)
    ));
    assert_eq!(done.last_agent_message, None);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawned_child_gets_one_completion_recovery_and_can_finish_after_check() -> Result<()> {
    use core_test_support::responses::ev_apply_patch_custom_tool_call;
    let server = start_mock_server().await;
    let requests = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("premature"),
                ev_assistant_message("message", "I will implement this next."),
                ev_completed("premature"),
            ]),
            sse(vec![
                ev_response_created("patch"),
                ev_apply_patch_custom_tool_call(
                    "patch",
                    "*** Begin Patch\n*** Add File: recovered.js\n+const recovered = true;\n*** End Patch",
                ),
                ev_completed("patch"),
            ]),
            sse(vec![
                ev_response_created("check"),
                ev_function_call(
                    "check",
                    "exec_command",
                    &json!({"cmd":"node --check recovered.js", "yield_time_ms":1000}).to_string(),
                ),
                ev_completed("check"),
            ]),
            sse(vec![
                ev_assistant_message("done", "Implemented and checked after recovery."),
                ev_completed("done"),
            ]),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.rencrow_execution_guard = Some(RencrowExecutionGuard {
                require_edit: true,
                require_check: true,
                ..Default::default()
            });
        })
        .build_with_auto_env(&server)
        .await?;
    let child = test
        .thread_manager
        .start_thread(StartThreadOptions {
            session_source: Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: test.session_configured.thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            })),
            environments: Some(test.codex.environment_selections().await),
            ..StartThreadOptions::new(test.config.clone())
        })
        .await?
        .thread;
    child
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: "Implement and check".into(),
                text_elements: vec![],
            }])
            .with_thread_settings(
                codex_protocol::protocol::ThreadSettingsOverrides {
                    approval_policy: Some(codex_protocol::protocol::AskForApproval::Never),
                    permission_profile: Some(
                        codex_protocol::models::PermissionProfile::workspace_write(),
                    ),
                    ..Default::default()
                },
            ),
        )
        .await?;
    let event = wait_for_event(&child, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    let EventMsg::TurnComplete(done) = event else {
        unreachable!()
    };
    assert_eq!(done.error, None);
    assert_eq!(
        done.last_agent_message.as_deref(),
        Some("Implemented and checked after recovery.")
    );
    assert!(requests.requests()[1].body_contains_text("RENCROW_EXECUTION_RECOVERY 1/1"));
    assert_eq!(
        std::fs::read_to_string(test.cwd_path().join("recovered.js"))?,
        "const recovered = true;\n"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawned_child_cannot_finish_after_its_single_recovery_fails() -> Result<()> {
    let server = start_mock_server().await;
    let requests = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("premature"),
                ev_assistant_message("message", "I will implement this next."),
                ev_completed("premature"),
            ]),
            sse(vec![
                ev_assistant_message("repeat", "I will implement this next."),
                ev_completed("repeat"),
            ]),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.rencrow_execution_guard = Some(RencrowExecutionGuard {
                require_edit: true,
                require_check: true,
                ..Default::default()
            });
        })
        .build_with_auto_env(&server)
        .await?;
    let child = test
        .thread_manager
        .start_thread(StartThreadOptions {
            session_source: Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: test.session_configured.thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            })),
            environments: Some(test.codex.environment_selections().await),
            ..StartThreadOptions::new(test.config.clone())
        })
        .await?
        .thread;
    child
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Implement and check".into(),
            text_elements: vec![],
        }]))
        .await?;
    let event = wait_for_event(&child, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    let EventMsg::TurnComplete(done) = event else {
        unreachable!()
    };
    assert!(done.error.is_some());
    assert!(requests.requests()[1].body_contains_text("RENCROW_EXECUTION_RECOVERY 1/1"));
    assert_eq!(requests.requests().len(), 2);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accepted_patch_and_passing_check_allow_completion() -> Result<()> {
    use core_test_support::responses::ev_apply_patch_custom_tool_call;
    let cases = [
        (
            "verified.js",
            "*** Begin Patch\n*** Add File: verified.js\n+const verified = true;\n*** End Patch",
            "node --check --conditions='guard|literal' verified.js",
            "const verified = true;\n",
        ),
        (
            "test_verified.py",
            "*** Begin Patch\n*** Add File: test_verified.py\n+import unittest\n+\n+class VerifiedTest(unittest.TestCase):\n+    def test_verified(self):\n+        self.assertTrue(True)\n*** End Patch",
            "python3 -B -m unittest -v test_verified",
            "import unittest\n\nclass VerifiedTest(unittest.TestCase):\n    def test_verified(self):\n        self.assertTrue(True)\n",
        ),
        (
            "test_verified_compile.py",
            "*** Begin Patch\n*** Add File: test_verified_compile.py\n+value = 42\n*** End Patch",
            "python3 -m py_compile test_verified_compile.py",
            "value = 42\n",
        ),
    ];
    for (file, patch, check, expected_contents) in cases {
        let server = start_mock_server().await;
        mount_sse_sequence(
            &server,
            vec![
                sse(vec![
                    ev_response_created("patch"),
                    ev_apply_patch_custom_tool_call("patch", patch),
                    ev_completed("patch"),
                ]),
                sse(vec![
                    ev_response_created("check"),
                    ev_function_call(
                        "check",
                        "exec_command",
                        &json!({"cmd":check, "yield_time_ms":1000}).to_string(),
                    ),
                    ev_completed("check"),
                ]),
                sse(vec![
                    ev_assistant_message("done", "Implemented and checked."),
                    ev_completed("done"),
                ]),
            ],
        )
        .await;
        let test = test_codex()
            .with_config(|config| {
                config.rencrow_execution_guard = Some(RencrowExecutionGuard {
                    require_edit: true,
                    require_check: true,
                    ..Default::default()
                });
            })
            .build_with_auto_env(&server)
            .await?;
        test.codex
            .start_or_steer_turn(
                TurnInputRequest::user_input(vec![UserInput::Text {
                    text: "Implement and check".into(),
                    text_elements: vec![],
                }])
                .with_thread_settings(
                    codex_protocol::protocol::ThreadSettingsOverrides {
                        approval_policy: Some(codex_protocol::protocol::AskForApproval::Never),
                        permission_profile: Some(
                            codex_protocol::models::PermissionProfile::workspace_write(),
                        ),
                        ..Default::default()
                    },
                ),
            )
            .await?;
        let mut check_exit_code = None;
        let event = wait_for_event(&test.codex, |event| {
            if let EventMsg::ExecCommandEnd(end) = event {
                check_exit_code = Some(end.exit_code);
            }
            matches!(event, EventMsg::TurnComplete(_))
        })
        .await;
        let EventMsg::TurnComplete(done) = event else {
            unreachable!()
        };
        assert_eq!(check_exit_code, Some(0));
        assert_eq!(done.error, None);
        assert_eq!(
            done.last_agent_message.as_deref(),
            Some("Implemented and checked.")
        );
        assert_eq!(
            std::fs::read_to_string(test.cwd_path().join(file))?,
            expected_contents
        );
    }
    Ok(())
}
