import json
import io
import tempfile
import unittest
from unittest.mock import patch
from pathlib import Path
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import threading
from urllib.request import Request, urlopen
from autonomous_completion_capture import capture_requests

from autonomous_completion_probe import (
    CASES,
    parent_rollout,
    parse_events,
    build_evidence_index,
    index_existing_run,
    set_config_flags,
    summarize_events,
    resume_command,
    completed_idle_resume_command,
    run_resume_probe,
    _resume_terminal_classification,
    _app_server_send,
    _run_completed_idle_resume,
)


class ProbeParserTests(unittest.TestCase):
    def test_all_predefined_case_ids_are_accepted(self):
        self.assertEqual(CASES, {f"C{i:02d}" for i in range(1, 11)})

    def test_app_server_send_writes_jsonl_bytes_to_binary_pipe(self):
        proc = type("Process", (), {"stdin": io.BytesIO()})()
        _app_server_send(proc, {"jsonrpc": "2.0", "method": "initialized"})
        self.assertEqual(
            proc.stdin.getvalue(), b'{"jsonrpc":"2.0","method":"initialized"}\n'
        )

    def _resume_source_fixture(self, root, process_status="completed", completed=False):
        source = root / "source-run"
        home = source / "home"
        sessions = home / "sessions" / "2026" / "11" / "11"
        workspace = source / "workspace"
        sessions.mkdir(parents=True)
        workspace.mkdir(parents=True)
        (workspace / "scenario.txt").write_text("isolated source workspace\n")
        config = home / "config.toml"
        config.write_text(
            'model = "qwen38-flash--rtx5060"\n'
            'model_provider = "test-provider"\n'
            "[model_providers.test-provider]\n"
            'base_url = "http://127.0.0.1:9999/v1"\n'
        )
        binary = source / "candidate"
        binary.write_text("#!/bin/sh\nexit 0\n")
        binary.chmod(0o755)
        rollout = sessions / "rollout-test.jsonl"
        rollout.write_text(
            json.dumps(
                {
                    "type": "session_meta",
                    "payload": {
                        "source": "exec",
                        "session_id": "thread-123",
                    },
                }
            )
            + "\n"
            + json.dumps(
                {
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "developer",
                        "internal_chat_message_metadata_passthrough": {
                            "content_item_kinds": ["rencrow.completion_snapshot"]
                        },
                    },
                    "metadata": {
                        "rencrow_completion": {
                            "thread_id": "thread-123",
                            "root_task_id": "root-a",
                            "uncollected_children": [],
                            "task_checks": [],
                            "patch_attempts": [],
                            "state": (
                                {
                                    "contract_revision": 1,
                                    "conditions": [
                                        {"id": "AC-1", "status": "satisfied"}
                                    ],
                                    "units": [{"id": "U-1", "status": "done"}],
                                    "paused": False,
                                    "blocker": None,
                                }
                                if completed
                                else {"contract_revision": 1}
                            ),
                            "active_scope": None,
                        }
                    },
                }
            )
            + "\n"
            + json.dumps(
                {
                    "type": "event_msg",
                    "payload": {"type": "task_complete", "turn_id": "turn-before"},
                }
            )
            + "\n"
        )
        import hashlib

        manifest = {
            "model": "qwen38-flash--rtx5060",
            "effort": "xhigh",
            "binary": str(binary),
            "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
            "config_sha256": hashlib.sha256(config.read_bytes()).hexdigest(),
            "workspace": str(workspace),
        }
        (source / "manifest.json").write_text(json.dumps(manifest))
        (source / "result.json").write_text(
            json.dumps(
                {
                    "process_status": process_status,
                    "exit_code": 0 if process_status == "completed" else -15,
                    "session": {
                        "rollout": str(rollout.relative_to(source)),
                    },
                }
            )
        )
        return source, rollout, manifest

    def test_resume_command_uses_native_thread_id_and_distinguishes_prompt_mode(self):
        with tempfile.TemporaryDirectory() as tmp:
            source, _, manifest = self._resume_source_fixture(Path(tmp))
            info = {
                "binary": Path(manifest["binary"]),
                "thread_id": "thread-123",
                "manifest": manifest,
            }
            quiet = resume_command(
                info, source / "workspace", "quiet_autonomous_resume", None
            )
            prompted = resume_command(
                info,
                source / "workspace",
                "initial_prompt_race",
                "please continue the queued work",
            )
        self.assertEqual(quiet[2:4], ["resume", "thread-123"])
        self.assertNotIn("--json", quiet)
        self.assertEqual(quiet[-1], "automation")
        self.assertEqual(
            prompted[2:5], ["resume", "thread-123", "please continue the queued work"]
        )
        self.assertEqual(prompted[-1], "human")

    def test_resume_reuses_same_thread_and_records_only_native_append(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            source, source_rollout, manifest = self._resume_source_fixture(root)
            original_hash = (
                __import__("hashlib").sha256(source_rollout.read_bytes()).hexdigest()
            )
            prompt_file = root / "resume-prompt.txt"
            prompt_file.write_text("Resume and inspect the next unfinished unit.\n")
            output = source / "resume-evidence" / "run-1"
            with patch(
                "autonomous_completion_probe._run_resume_tui",
                return_value=(0, False, "completed_conditions"),
            ) as run_tui:
                result, status = run_resume_probe(
                    source,
                    output,
                    "initial_prompt_race",
                    timeout=30,
                    prompt_file=prompt_file,
                )
            self.assertEqual(status, 0)
            self.assertEqual(result["thread_id_from_rollout"], "thread-123")
            self.assertTrue(result["source_process_exit_observed"]["basis"])
            self.assertTrue(result["test_apparatus_restart"])
            self.assertTrue(result["source_rollout_prefix_preserved"])
            self.assertEqual(
                __import__("hashlib").sha256(source_rollout.read_bytes()).hexdigest(),
                original_hash,
            )
            self.assertFalse((output / "home").exists())
            self.assertEqual(result["resume_issued_without_prompt"], False)
            self.assertEqual(result["external_steer"], True)
            self.assertEqual(
                result["manual_or_prompt_resume_is_not_autonomous_acceptance"], True
            )
            self.assertEqual(
                result["safe_terminal_classification"], "completed_conditions"
            )
            self.assertTrue(
                run_tui.call_args.args[1]["PATH"].startswith(
                    str(Path(manifest["binary"]).resolve().parent)
                )
            )
            self.assertEqual(
                run_tui.call_args.args[0][2:5],
                [
                    "resume",
                    "thread-123",
                    "Resume and inspect the next unfinished unit.",
                ],
            )

    def test_resume_terminal_classification_requires_completed_turn_and_host_acceptance(
        self,
    ):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _, rollout, _ = self._resume_source_fixture(root)
            initial_lines = len(rollout.read_text().splitlines())
            self.assertIsNone(
                _resume_terminal_classification(rollout, "thread-123", initial_lines)
            )
            with rollout.open("a") as stream:
                stream.write(
                    json.dumps(
                        {
                            "type": "event_msg",
                            "payload": {"type": "task_started", "turn_id": "resume-1"},
                        }
                    )
                    + "\n"
                    + json.dumps(
                        {
                            "type": "event_msg",
                            "payload": {"type": "task_complete", "turn_id": "resume-1"},
                        }
                    )
                    + "\n"
                )
            self.assertIsNone(
                _resume_terminal_classification(rollout, "thread-123", initial_lines)
            )
            rows = [json.loads(line) for line in rollout.read_text().splitlines()]
            rows[1]["metadata"]["rencrow_completion"]["state"]["conditions"] = [
                {"id": "AC-1", "status": "satisfied"}
            ]
            rollout.write_text("\n".join(json.dumps(row) for row in rows) + "\n")
            self.assertEqual(
                _resume_terminal_classification(rollout, "thread-123", initial_lines),
                "completed_conditions",
            )

    def test_resume_requires_terminal_source_process_evidence(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            source, _, _ = self._resume_source_fixture(root, process_status="running")
            with self.assertRaisesRegex(ValueError, "terminal process status"):
                run_resume_probe(
                    source,
                    source / "resume-evidence" / "run-1",
                    "quiet_autonomous_resume",
                    timeout=30,
                )

    def test_completed_idle_resume_requires_complete_host_state_and_http_capture(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            source, _, _ = self._resume_source_fixture(root, completed=False)
            with self.assertRaisesRegex(
                ValueError, "every acceptance condition satisfied"
            ):
                run_resume_probe(
                    source,
                    source / "resume-evidence" / "run-1",
                    "completed_idle_resume",
                    timeout=120,
                    capture_http=True,
                )
            source, _, _ = self._resume_source_fixture(
                root / "complete", completed=True
            )
            with self.assertRaisesRegex(ValueError, "requires --capture-http"):
                run_resume_probe(
                    source,
                    source / "resume-evidence" / "run-1",
                    "completed_idle_resume",
                    timeout=120,
                    capture_http=False,
                )

    def test_completed_idle_resume_rejects_running_unit_and_active_scope(self):
        for field, value, expected in [
            ("unit_status", "running", "every completion unit done"),
            ("active_scope", {"task_id": "task-a"}, "no active task scope"),
        ]:
            with self.subTest(field=field), tempfile.TemporaryDirectory() as tmp:
                source, rollout, _ = self._resume_source_fixture(
                    Path(tmp), completed=True
                )
                rows = [json.loads(line) for line in rollout.read_text().splitlines()]
                snapshot = rows[1]["metadata"]["rencrow_completion"]
                if field == "unit_status":
                    snapshot["state"]["units"][0]["status"] = value
                else:
                    snapshot["active_scope"] = value
                rollout.write_text("\n".join(json.dumps(row) for row in rows) + "\n")
                with self.assertRaisesRegex(ValueError, expected):
                    run_resume_probe(
                        source,
                        source / "resume-evidence" / "run-1",
                        "completed_idle_resume",
                        timeout=120,
                        capture_http=True,
                    )

    def test_completed_idle_resume_uses_app_server_and_requires_ready_plus_idle_evidence(
        self,
    ):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            source, rollout, manifest = self._resume_source_fixture(
                root, completed=True
            )
            output = source / "resume-evidence" / "completed-idle"
            with patch(
                "autonomous_completion_probe._run_completed_idle_resume",
                return_value={
                    "configured": True,
                    "ready_signal": "successful thread/resume response after native SessionConfiguredEvent",
                    "idle_observed": True,
                    "observation_seconds": 60,
                    "child_started": False,
                    "protocol_error": False,
                    "host_snapshot_unchanged": True,
                    "new_tool_call": False,
                    "provider_input_count": 0,
                    "new_turn_counts": {
                        "started_turn_ids": [],
                        "completed_turn_ids": [],
                    },
                    "same_thread_id": True,
                    "same_workspace": True,
                    "same_rollout_path": True,
                },
            ) as run_app_server:
                result, status = run_resume_probe(
                    source,
                    output,
                    "completed_idle_resume",
                    timeout=120,
                    capture_http=True,
                )
            self.assertEqual(status, 0)
            self.assertEqual(result["transport"], "app_server_stdio")
            self.assertEqual(
                result["safe_terminal_classification"], "completed_idle_restored"
            )
            self.assertTrue(result["resume_issued_without_prompt"])
            self.assertEqual(result["new_turns_observed"]["started_turn_ids"], [])
            self.assertTrue(result["source_rollout_prefix_preserved"])
            self.assertEqual(result["status"], "needs_review")
            command = run_app_server.call_args.args[0]
            self.assertIn("app-server", command)
            self.assertIn("--stdio", command)
            self.assertNotIn("resume", command)
            self.assertTrue(
                run_app_server.call_args.kwargs["observation_seconds"] == 60
            )
            provider = json.loads((output / "resume-command.json").read_text())
            self.assertIn("127.0.0.1", provider["provider_capture_override"])

    def test_completed_idle_resume_remains_needs_review_without_ready_or_full_idle_window(
        self,
    ):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            source, _, _ = self._resume_source_fixture(root, completed=True)
            with patch(
                "autonomous_completion_probe._run_completed_idle_resume",
                return_value={
                    "configured": False,
                    "idle_observed": False,
                    "reason": "resume response mismatch",
                },
            ):
                result, status = run_resume_probe(
                    source,
                    source / "resume-evidence" / "run-1",
                    "completed_idle_resume",
                    timeout=120,
                    capture_http=True,
                )
            self.assertEqual(status, 1)
            self.assertIsNone(result["safe_terminal_classification"])
            self.assertEqual(result["status"], "needs_review")
            self.assertEqual(
                result["completed_idle_evidence"]["reason"], "resume response mismatch"
            )

    def test_completed_idle_resume_closes_popen_stdout_on_exception(self):
        class FakeProcess:
            def __init__(self):
                self.stdin = io.StringIO()
                self.stdout = io.StringIO()

            def poll(self):
                return 0

        with tempfile.TemporaryDirectory() as tmp:
            proc = FakeProcess()
            with (
                patch(
                    "autonomous_completion_probe.subprocess.Popen", return_value=proc
                ),
                patch(
                    "autonomous_completion_probe._app_server_request",
                    side_effect=RuntimeError("initialize failed"),
                ),
            ):
                with self.assertRaisesRegex(RuntimeError, "initialize failed"):
                    _run_completed_idle_resume(
                        ["codex", "app-server", "--stdio"],
                        {},
                        Path(tmp) / "stderr.log",
                        Path(tmp) / "rollout.jsonl",
                        "thread-123",
                        Path(tmp),
                        {},
                        0,
                        Path(tmp) / "provider-inputs",
                    )
            self.assertTrue(proc.stdin.closed)
            self.assertTrue(proc.stdout.closed)

    def test_resume_refuses_uncollected_children_and_pending_checks(self):
        for pending_field, value, message in [
            ("uncollected_children", ["child-1"], "uncollected child"),
            (
                "task_checks",
                [{"attempt": {"status": "Pending"}}],
                "pending task check",
            ),
            (
                "patch_attempts",
                [["patch-1", {"completed_successfully": False}]],
                "unfinished patch attempt",
            ),
        ]:
            with (
                self.subTest(pending_field=pending_field),
                tempfile.TemporaryDirectory() as tmp,
            ):
                root = Path(tmp)
                source, rollout, _ = self._resume_source_fixture(root)
                rows = [json.loads(line) for line in rollout.read_text().splitlines()]
                rows[1]["metadata"]["rencrow_completion"][pending_field] = value
                rollout.write_text("\n".join(json.dumps(row) for row in rows) + "\n")
                with self.assertRaisesRegex(ValueError, message):
                    run_resume_probe(
                        source,
                        source / "resume-evidence" / "run-1",
                        "quiet_autonomous_resume",
                        timeout=30,
                    )

    def test_resume_refuses_unresolved_native_tool_call(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            source, rollout, _ = self._resume_source_fixture(root)
            with rollout.open("a") as stream:
                stream.write(
                    json.dumps(
                        {
                            "type": "response_item",
                            "payload": {
                                "type": "function_call",
                                "call_id": "exec-pending",
                                "name": "exec_command",
                            },
                        }
                    )
                    + "\n"
                )
            with self.assertRaisesRegex(
                ValueError, "without terminal outputs: exec-pending"
            ):
                run_resume_probe(
                    source,
                    source / "resume-evidence" / "run-1",
                    "quiet_autonomous_resume",
                    timeout=30,
                )

    def test_capture_records_actual_request_and_forwards_response(self):
        class Upstream(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def do_POST(self):
                body = self.rfile.read(int(self.headers["Content-Length"]))
                self.send_response(200)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

        server = ThreadingHTTPServer(("127.0.0.1", 0), Upstream)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            with tempfile.TemporaryDirectory() as tmp:
                destination = Path(tmp) / "requests"
                with capture_requests(
                    destination, f"http://127.0.0.1:{server.server_port}/v1"
                ) as proxy:
                    body = b'{"input":"fixture"}'
                    with urlopen(
                        Request(
                            proxy + "/responses",
                            data=body,
                            headers={"Content-Type": "application/json"},
                        ),
                        timeout=5,
                    ) as response:
                        self.assertEqual(response.read(), body)
                record = json.loads(next(destination.glob("*.json")).read_text())
                self.assertEqual(record["body"], {"input": "fixture"})
                self.assertEqual(record["path"], "/v1/responses")
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=5)

    def test_usage_uses_reported_cumulative_tokens_not_text_length(self):
        result = summarize_events(
            [
                (
                    1,
                    {
                        "type": "event_msg",
                        "payload": {
                            "type": "token_count",
                            "info": {
                                "total_token_usage": {
                                    "input_tokens": 120,
                                    "cached_input_tokens": 90,
                                    "output_tokens": 10,
                                }
                            },
                        },
                    },
                )
            ],
            "C01",
        )
        self.assertEqual(result["metrics"]["input_tokens"], 120)
        self.assertEqual(result["metrics"]["cache_tokens"], 90)
        self.assertIsNone(result["metrics"]["cost"])

    def test_newer_child_is_never_used_as_parent(self):
        with tempfile.TemporaryDirectory() as tmp:
            parent, child = Path(tmp) / "parent.jsonl", Path(tmp) / "child.jsonl"
            parent.write_text(
                json.dumps({"type": "session_meta", "payload": {"source": "exec"}})
                + "\n"
            )
            child.write_text(
                json.dumps(
                    {
                        "type": "session_meta",
                        "payload": {"source": {"subagent": {"thread_spawn": {}}}},
                    }
                )
                + "\n"
            )
            self.assertEqual(parent_rollout([parent, child]), parent)
            self.assertIsNone(parent_rollout([child]))

    def test_prose_and_tool_mention_are_not_actions_or_commit(self):
        rows = [
            {
                "type": "item.completed",
                "item": {
                    "type": "message",
                    "text": "I will call task_context_boundary and compact.",
                },
            },
            {
                "type": "event_msg",
                "payload": {"type": "task_context_boundary_candidate"},
            },
        ]
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "events.jsonl"
            path.write_text("\n".join(json.dumps(x) for x in rows))
            result = summarize_events(parse_events(path), "C01")
        self.assertEqual(result["tool_call_count"], 0)
        self.assertEqual(result["explicit_compaction_commit_count"], 0)
        self.assertEqual(result["status"], "not_run")

    def test_tool_call_is_counted_but_candidate_is_not_commit(self):
        events = [
            (
                1,
                {
                    "type": "response_item",
                    "payload": {
                        "type": "function_call",
                        "name": "task_context_boundary",
                        "arguments": '{"action":"begin","task_id":"B"}',
                    },
                },
            ),
            (2, {"type": "event_msg", "payload": {"type": "compaction_candidate"}}),
        ]
        result = summarize_events(events, "C10")
        self.assertEqual(result["tool_call_count"], 1)
        self.assertIsNone(result["first_tool_call"])
        self.assertEqual(result["explicit_compaction_commit_count"], 0)
        self.assertEqual(result["status"], "not_run")

    def test_only_exact_native_commit_counts_and_first_call_is_after_it(self):
        events = [
            (
                1,
                {
                    "type": "response_item",
                    "payload": {
                        "type": "function_call",
                        "name": "exec_command",
                        "arguments": "before",
                    },
                },
            ),
            (
                2,
                {
                    "type": "rencrow_compaction_candidate",
                    "payload": {"checkpoint_hash": "candidate"},
                },
            ),
            (
                3,
                {
                    "type": "rencrow_compaction_commit",
                    "payload": {"checkpoint_hash": "hash-1"},
                },
            ),
            (
                4,
                {
                    "type": "response_item",
                    "payload": {
                        "type": "function_call",
                        "name": "task_context_boundary",
                        "arguments": "after",
                    },
                },
            ),
            (
                5,
                {
                    "type": "rencrow_compaction_commit",
                    "payload": {"checkpoint_hash": ""},
                },
            ),
            (
                6,
                {
                    "type": "response_item",
                    "payload": {
                        "type": "function_call",
                        "name": "task_context_boundary",
                        "arguments": '{"action":"begin","task_id":"B"}',
                    },
                },
            ),
            (
                7,
                {
                    "type": "response_item",
                    "payload": {
                        "type": "function_call",
                        "name": "task_context_boundary",
                        "arguments": '{"action":"begin","task_id":"B"}',
                    },
                },
            ),
        ]
        result = summarize_events(events, "C10")
        self.assertEqual(result["explicit_compaction_commit_count"], 1)
        self.assertEqual(result["first_tool_call"]["arguments"], "after")
        self.assertEqual(result["first_tool_call_position"], "after_first_commit")
        self.assertEqual(result["duplicate_begin_target_count"], 1)
        self.assertEqual(result["duplicate_begin_targets"], ["B"])

    def test_config_flags_are_inserted_at_toml_root(self):
        with tempfile.TemporaryDirectory() as tmp:
            source, target = Path(tmp) / "source.toml", Path(tmp) / "fresh.toml"
            source.write_text(
                'model = "sample"\n[model_providers.rencrow]\nbase_url = "http://127.0.0.1:8090/v1"\n'
            )
            set_config_flags(source, target)
            loaded = __import__("tomllib").loads(target.read_text())
        self.assertIs(loaded["rencrow_compaction"], True)
        self.assertIs(loaded["rencrow_autonomous_completion"], True)
        self.assertEqual(
            loaded["model_providers"]["rencrow"]["base_url"], "http://127.0.0.1:8090/v1"
        )

    def test_explicit_commit_requires_review_never_automatic_pass(self):
        events = [
            (
                1,
                {
                    "type": "rencrow_compaction_commit",
                    "payload": {"checkpoint_hash": "hash"},
                },
            )
        ]
        result = summarize_events(events, "C04")
        self.assertEqual(result["explicit_compaction_commit_count"], 1)
        self.assertEqual(result["status"], "needs_review")

    def test_evidence_index_links_actual_stages_without_copying_content(self):
        with tempfile.TemporaryDirectory() as tmp:
            run = Path(tmp) / "run"
            rollout = run / "home" / "sessions" / "rollout-test.jsonl"
            captures = run / "provider-inputs"
            rollout.parent.mkdir(parents=True)
            captures.mkdir(parents=True)
            rows = [
                {
                    "type": "response_item",
                    "timestamp": "2026-11-11T00:00:00Z",
                    "payload": {"type": "message", "role": "developer"},
                    "metadata": {
                        "rencrow_completion": {
                            "root_task_id": "root",
                            "state": {"contract_revision": 3, "paused": False},
                        }
                    },
                },
                {
                    "type": "compacted",
                    "timestamp": "2026-11-11T00:00:02Z",
                    "payload": {"replacement_history": []},
                    "metadata": {
                        "rencrow_compaction": {
                            "version": 3,
                            "mode": "capacity",
                            "summary_hash": "summary-hash",
                            "snapshot_hash": "snapshot-hash",
                            "task_state": {"root_task_id": "root"},
                        }
                    },
                },
                {
                    "type": "rencrow_compaction_commit",
                    "timestamp": "2026-11-11T00:00:03Z",
                    "payload": {"checkpoint_hash": "checkpoint-1"},
                },
                {
                    "type": "response_item",
                    "payload": {
                        "type": "function_call",
                        "name": "exec_command",
                        "arguments": "{}",
                    },
                },
                {
                    "type": "response_item",
                    "payload": {
                        "type": "function_call",
                        "name": "task_context_boundary",
                        "arguments": '{"action":"begin","task_id":"B"}',
                    },
                },
                {
                    "type": "response_item",
                    "payload": {
                        "type": "function_call",
                        "name": "exec_command",
                        "arguments": "{}",
                    },
                },
                {
                    "type": "response_item",
                    "payload": {
                        "type": "function_call",
                        "name": "exec_command",
                        "arguments": "{}",
                    },
                },
                {
                    "type": "response_item",
                    "payload": {
                        "type": "function_call",
                        "name": "exec_command",
                        "arguments": "{}",
                    },
                },
                {
                    "type": "response_item",
                    "payload": {
                        "type": "function_call",
                        "name": "task_context_boundary",
                        "arguments": '{"action":"accept","goal":"root"}',
                    },
                },
                {
                    "type": "rencrow_compaction_commit",
                    "timestamp": "2026-11-11T00:00:06Z",
                    "payload": {"checkpoint_hash": "checkpoint-2"},
                },
            ]
            rollout.write_text("\n".join(json.dumps(row) for row in rows) + "\n")
            for ordinal, time_ns in [
                (1, 1794355201000000000),
                (2, 1794355204000000000),
            ]:
                record = {
                    "ordinal": ordinal,
                    "time_ns": time_ns,
                    "method": "POST",
                    "path": "/v1/responses",
                    "body": {
                        "model": "qwen",
                        "input": [{"type": "message", "content": "private prompt"}],
                    },
                }
                (captures / f"{ordinal:05d}.json").write_text(json.dumps(record))

            index = build_evidence_index(run, parse_events(rollout), rollout)

        self.assertEqual(index["commit_count"], 2)
        self.assertTrue(index["multiple_commits"])
        self.assertEqual(index["cold_resume"], "unknown")
        self.assertEqual(
            index["host_completion_snapshot_refs"][0]["root_task_id"], "root"
        )
        first_commit = index["E1"][0]
        self.assertEqual(len(first_commit["first_five_tool_calls"]), 5)
        self.assertEqual(first_commit["next_accept"]["action"], "accept")
        self.assertEqual(index["commits"][0]["I0"]["status"], "unknown")
        self.assertEqual(
            index["commits"][0]["I1"]["status"], "first_postcommit_request_candidate"
        )
        self.assertEqual(
            index["commits"][0]["summary_output"]["status"], "identified_by_hash"
        )
        self.assertEqual(
            index["commits"][0]["summary_output_refs"][0]["summary_hash"],
            "summary-hash",
        )
        serialized = json.dumps(index)
        self.assertNotIn("private prompt", serialized)

    def test_failed_trial_index_keeps_missing_stages_unknown_and_writes_elsewhere(self):
        with tempfile.TemporaryDirectory() as tmp:
            run = Path(tmp) / "failed-trial"
            rollout_dir = run / "home" / "sessions"
            rollout_dir.mkdir(parents=True)
            (run / "events.jsonl").write_text("{}\n")
            (run / "result.json").write_text(
                json.dumps({"case": "C04", "process_status": "failed", "exit_code": 1})
            )
            output = Path(tmp) / "indexes" / "failed.json"
            index = index_existing_run(run, output)
            self.assertEqual(index["commit_count"], 0)
            self.assertEqual(index["status"], "not_run")
            self.assertEqual(index["process_status"], "failed")
            self.assertEqual(index["case"], "C04")
            self.assertEqual(index["S0"]["status"], "unknown")
            self.assertEqual(index["S1"]["status"], "unknown")
            self.assertEqual(index["summary_input"]["status"], "unknown")
            self.assertEqual(json.loads(output.read_text()), index)


if __name__ == "__main__":
    unittest.main()
