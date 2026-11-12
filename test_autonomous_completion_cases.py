"""Deterministic local checks for the C03 host-side apparatus."""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest
import uuid

from autonomous_completion_cases import (
    GATE_NAME,
    GATE_STARTED_NAME,
    RELEASE_NAME,
    _write_new,
    child_gate_is_running,
    committed_checkpoint,
    host_spawn_results,
    next_parent_action_after,
    prepare_c03_case,
    wait_result_for_child,
)


class AutonomousCompletionCasesTests(unittest.TestCase):
    def test_host_spawn_id_is_read_from_paired_host_output_only(self):
        child_id = str(uuid.uuid4())
        rows = [
            {
                "ordinal": 10,
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"text": json.dumps({"agent_id": child_id})}],
                },
            },
            {
                "ordinal": 11,
                "type": "response_item",
                "payload": {
                    "type": "function_call",
                    "call_id": "spawn-call",
                    "name": "multi_agent_v1::spawn_agent",
                    "arguments": "{}",
                },
            },
            {
                "ordinal": 12,
                "type": "response_item",
                "payload": {
                    "type": "function_call_output",
                    "call_id": "spawn-call",
                    "output": json.dumps({"agent_id": child_id}),
                },
            },
        ]
        self.assertEqual(
            host_spawn_results(rows, after_ordinal=10),
            [{"child_id": child_id, "call_id": "spawn-call", "ordinal": 12}],
        )

    def test_gate_must_be_in_progress_for_exact_child_and_workspace(self):
        with tempfile.TemporaryDirectory() as tmp:
            workspace = Path(tmp).resolve()
            gate = workspace / GATE_NAME
            child_id = str(uuid.uuid4())
            parent_id = str(uuid.uuid4())
            response = {
                "thread": {
                    "id": child_id,
                    "parentThreadId": parent_id,
                    "turns": [
                        {
                            "items": [
                                {
                                    "type": "commandExecution",
                                    "status": "inProgress",
                                    "command": f"python3 {gate}",
                                    "cwd": str(workspace),
                                }
                            ]
                        }
                    ],
                }
            }
            self.assertTrue(
                child_gate_is_running(response, child_id, parent_id, workspace, gate)
            )
            response["thread"]["turns"][0]["items"][0]["status"] = "completed"
            self.assertFalse(
                child_gate_is_running(response, child_id, parent_id, workspace, gate)
            )
            self.assertFalse(
                child_gate_is_running(
                    response, str(uuid.uuid4()), parent_id, workspace, gate
                )
            )

    def test_checkpoint_requires_new_compacted_item_and_host_commit_marker(self):
        rows = [
            {"ordinal": 1, "type": "compacted", "payload": {"window_id": "old"}},
            {
                "ordinal": 2,
                "type": "rencrow_compaction_commit",
                "payload": {"checkpoint_hash": "old-hash"},
            },
            {"ordinal": 4, "type": "compacted", "payload": {"window_id": "new"}},
            {
                "ordinal": 5,
                "type": "rencrow_compaction_commit",
                "payload": {"checkpoint_hash": "new-hash"},
            },
        ]
        self.assertEqual(
            committed_checkpoint(rows, after_ordinal=2),
            {"ordinal": 5, "checkpoint_hash": "new-hash"},
        )
        self.assertIsNone(committed_checkpoint(rows[:3], after_ordinal=2))

    def test_wait_result_and_next_action_must_follow_same_child(self):
        child_id = str(uuid.uuid4())
        rows = [
            {
                "ordinal": 20,
                "type": "response_item",
                "payload": {
                    "type": "function_call",
                    "call_id": "wait-call",
                    "name": "multi_agent_v1::wait_agent",
                    "arguments": json.dumps({"targets": [child_id]}),
                },
            },
            {
                "ordinal": 21,
                "type": "response_item",
                "payload": {
                    "type": "function_call_output",
                    "call_id": "wait-call",
                    "output": json.dumps({"status": {child_id: {"completed": "done"}}}),
                },
            },
            {
                "ordinal": 22,
                "type": "response_item",
                "payload": {
                    "type": "function_call",
                    "call_id": "next-call",
                    "name": "task_context_boundary",
                    "arguments": "{}",
                },
            },
        ]
        result = wait_result_for_child(rows, child_id, after_ordinal=10)
        self.assertEqual(result["ordinal"], 21)
        self.assertEqual(
            next_parent_action_after(rows, result["ordinal"])["tool"],
            "task_context_boundary",
        )
        self.assertIsNone(
            wait_result_for_child(rows, str(uuid.uuid4()), after_ordinal=10)
        )

    def test_prepared_gate_waits_for_host_release_and_closes_process_pipes(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            workspace = root / "workspace"
            home = root / "home"
            case_dir = root / "case"
            workspace.mkdir()
            home.mkdir()
            (home / "config.toml").write_text(
                'model_provider = "local"\n[model_providers.local]\nbase_url = "http://127.0.0.1:1/v1"\n',
                encoding="utf-8",
            )
            prompt = root / "prompt.txt"
            prompt.write_text("fixed C03 prompt", encoding="utf-8")
            manifest = prepare_c03_case(
                case_dir=case_dir,
                workspace=workspace,
                binary=sys.executable,
                home=home,
                prompt_file=prompt,
                model="test-model",
                timeout_seconds=8,
                gate_timeout_seconds=6,
            )
            self.assertFalse(manifest["trial_started"])
            self.assertEqual(manifest["transport"], "native_app_server_stdio")
            gate = Path(manifest["gate"])
            self.assertEqual(gate.parent, workspace.resolve())
            proc = None
            try:
                with subprocess.Popen(
                    [sys.executable, str(gate)],
                    cwd=workspace,
                    stdin=subprocess.PIPE,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    text=True,
                ) as proc:
                    started = workspace / GATE_STARTED_NAME
                    deadline = time.monotonic() + 2
                    while time.monotonic() < deadline and not started.exists():
                        time.sleep(0.01)
                    self.assertTrue(started.is_file())
                    self.assertIsNone(proc.poll(), "gate must block until release")
                    _write_new(workspace / RELEASE_NAME, b"test release\n")
                    stdout, stderr = proc.communicate(timeout=2)
                    self.assertEqual(proc.returncode, 0)
                    self.assertEqual(stdout, "C03_GATE_RELEASED\n")
                    self.assertEqual(stderr, "")
            finally:
                if proc is not None and proc.poll() is None:
                    _write_new(workspace / RELEASE_NAME, b"test cleanup release\n")
                    proc.communicate(timeout=2)

    def test_prepare_rejects_existing_gate_and_unbounded_timeouts(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            workspace = root / "workspace"
            home = root / "home"
            workspace.mkdir()
            home.mkdir()
            (home / "config.toml").write_text(
                'model_provider = "local"\n[model_providers.local]\nbase_url = "http://127.0.0.1:1/v1"\n',
                encoding="utf-8",
            )
            (workspace / GATE_NAME).touch()
            prompt = root / "prompt.txt"
            prompt.write_text("prompt", encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "gate path already exists"):
                prepare_c03_case(
                    case_dir=root / "case-a",
                    workspace=workspace,
                    binary=sys.executable,
                    home=home,
                    prompt_file=prompt,
                    model="test-model",
                    timeout_seconds=8,
                    gate_timeout_seconds=6,
                )
            (workspace / GATE_NAME).unlink()
            with self.assertRaisesRegex(ValueError, "timeout"):
                prepare_c03_case(
                    case_dir=root / "case-b",
                    workspace=workspace,
                    binary=sys.executable,
                    home=home,
                    prompt_file=prompt,
                    model="test-model",
                    timeout_seconds=MAX_TIMEOUT_PLUS_ONE,
                    gate_timeout_seconds=6,
                )


MAX_TIMEOUT_PLUS_ONE = 1801


if __name__ == "__main__":
    unittest.main()
