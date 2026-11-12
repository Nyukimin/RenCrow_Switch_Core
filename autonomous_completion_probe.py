#!/usr/bin/env python3
"""Run one isolated real-Qwen autonomous-completion probe and retain evidence."""

import argparse
from datetime import datetime
import errno
import hashlib
from contextlib import ExitStack
import json
import os
from pathlib import Path
import re
import select
import signal
import shutil
import pty
import struct
import subprocess
import sys
import termios
import time
import tomllib
from autonomous_completion_capture import capture_requests

MODEL = "qwen38-flash--rtx5060"
CASES = {f"C{i:02d}" for i in range(1, 11)}
MAX_RESUME_TIMEOUT_SECONDS = 1800


def _load_resume_source(source_run):
    """Resolve an existing probe thread only from its own recorded evidence."""
    source_run = Path(source_run).resolve()
    manifest = json.loads((source_run / "manifest.json").read_text())
    result = json.loads((source_run / "result.json").read_text())
    if result.get("process_status") not in {
        "completed",
        "failed",
        "timeout",
        "interrupted",
    }:
        raise ValueError("source probe has no terminal process status")
    if not isinstance(result.get("exit_code"), int):
        raise ValueError("source probe did not record the codex process exit code")
    if result.get("process_status") == "timeout" and result.get("exit_code") is None:
        raise ValueError("timed-out source process exit was not observed")

    recorded_rollout = result.get("session", {}).get("rollout")
    if not isinstance(recorded_rollout, str) or not recorded_rollout:
        raise ValueError("source evidence does not identify its parent rollout")
    rollout = (source_run / recorded_rollout).resolve()
    if source_run not in rollout.parents or not rollout.is_file():
        raise ValueError("recorded parent rollout is missing or outside the source run")
    first = next((row for _, row in parse_events(rollout)), None)
    if not isinstance(first, dict) or first.get("type") != "session_meta":
        raise ValueError("recorded rollout has no initial session metadata")
    payload = first.get("payload", {})
    if not isinstance(payload, dict) or payload.get("source") != "exec":
        raise ValueError("recorded rollout is not the original parent exec thread")
    thread_id = payload.get("session_id") or payload.get("id")
    if not isinstance(thread_id, str) or not thread_id:
        raise ValueError("parent rollout does not identify its thread")
    latest_snapshot = _latest_host_completion_snapshot(rollout, thread_id)
    if latest_snapshot is None:
        raise ValueError("source rollout has no matching host completion snapshot")
    if latest_snapshot.get("uncollected_children"):
        raise ValueError("source thread still has uncollected child results")
    if any(
        not isinstance(attempt, dict) or not attempt.get("completed_successfully")
        for _, attempt in latest_snapshot.get("patch_attempts", [])
    ):
        raise ValueError("source thread has an unfinished patch attempt")
    for check in latest_snapshot.get("task_checks", []):
        attempt = check.get("attempt", {}) if isinstance(check, dict) else {}
        status = attempt.get("status") if isinstance(attempt, dict) else None
        if status == "Pending" or (isinstance(status, dict) and "Pending" in status):
            raise ValueError("source thread has a pending task check")
    unresolved_calls = _unresolved_tool_call_ids(rollout)
    if unresolved_calls:
        raise ValueError(
            "source rollout has tool calls without terminal outputs: "
            + ", ".join(unresolved_calls[:8])
        )

    home = source_run / "home"
    config = home / "config.toml"
    workspace = Path(manifest.get("workspace", "")).resolve()
    binary = Path(manifest.get("binary", "")).resolve()
    if not config.is_file() or not workspace.is_dir():
        raise ValueError("source run is missing its original config or workspace")
    if not binary.is_file() or not os.access(binary, os.X_OK):
        raise ValueError("recorded native binary is not executable")
    if _sha256_file(binary) != manifest.get("binary_sha256"):
        raise ValueError("recorded native binary changed since the source trial")
    if _sha256_file(config) != manifest.get("config_sha256"):
        raise ValueError("source config changed since the source trial")
    if manifest.get("effort") != "xhigh" or manifest.get("model") != MODEL:
        raise ValueError(
            "source run does not use the recorded autonomous probe model/effort"
        )
    return {
        "source_run": source_run,
        "manifest": manifest,
        "result": result,
        "rollout": rollout,
        "thread_id": thread_id,
        "completion_snapshot": latest_snapshot,
        "home": home,
        "config": config,
        "workspace": workspace,
        "binary": binary,
    }


def _latest_host_completion_snapshot(rollout, thread_id):
    latest_snapshot = None
    for _, row in parse_events(rollout):
        row_payload = row.get("payload", {})
        row_metadata = row.get("metadata", {})
        kinds = (
            row_payload.get("internal_chat_message_metadata_passthrough", {}).get(
                "content_item_kinds", []
            )
            if isinstance(row_payload, dict)
            else []
        )
        is_host_snapshot = (
            row.get("type") == "response_item"
            and isinstance(row_payload, dict)
            and row_payload.get("type") == "message"
            and row_payload.get("role") == "developer"
            and kinds == ["rencrow.completion_snapshot"]
            and isinstance(row_metadata, dict)
            and not row_metadata.get("client_authored", False)
        )
        if not is_host_snapshot:
            continue
        for _, snapshot in _walk_key(row_metadata, "rencrow_completion"):
            if isinstance(snapshot, dict) and snapshot.get("thread_id") == thread_id:
                latest_snapshot = snapshot
    return latest_snapshot


def _sha256_file(path):
    digest = hashlib.sha256()
    with Path(path).open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def _unresolved_tool_call_ids(rollout):
    calls = set()
    outputs = set()
    for _, row in parse_events(rollout):
        payload = row.get("payload", {})
        if row.get("type") != "response_item" or not isinstance(payload, dict):
            continue
        item_type = payload.get("type")
        call_id = payload.get("call_id")
        if not isinstance(call_id, str) or not call_id:
            continue
        if item_type in {"function_call", "custom_tool_call"}:
            calls.add(call_id)
        elif item_type in {"function_call_output", "custom_tool_call_output"}:
            outputs.add(call_id)
    return sorted(calls - outputs)


def resume_command(source, workspace, mode, prompt):
    """Build argv using only options confirmed by native `resume --help`."""
    command = [
        str(source["binary"]),
        "--no-daemon",
        "resume",
        source["thread_id"],
    ]
    if prompt is not None:
        command.append(prompt)
    command.extend(
        [
            "--cd",
            str(workspace),
            "--model",
            source["manifest"]["model"],
            "-c",
            'model_reasoning_effort="xhigh"',
            "--rencrow-input-author",
            "human" if mode == "initial_prompt_race" else "automation",
        ]
    )
    return command


def completed_idle_resume_command(source, workspace, provider_proxy):
    """Build the native app-server command used to prove a quiet cold restore."""
    return [
        str(source["binary"]),
        "--no-daemon",
        "-c",
        'model_reasoning_effort="xhigh"',
        "-c",
        f"model_providers.{source['manifest']['provider']}.base_url={json.dumps(provider_proxy)}",
        "app-server",
        "--stdio",
    ]


def _validate_completed_idle_source(source):
    result = source["result"]
    if result.get("process_status") != "completed" or result.get("exit_code") != 0:
        raise ValueError(
            "completed-idle resume requires a successfully exited source process"
        )
    snapshot = source["completion_snapshot"]
    state = snapshot.get("state")
    if not isinstance(state, dict):
        raise ValueError("completed-idle resume requires a typed completion state")
    conditions = state.get("conditions")
    if (
        not isinstance(conditions, list)
        or not conditions
        or any(
            not isinstance(condition, dict) or condition.get("status") != "satisfied"
            for condition in conditions
        )
    ):
        raise ValueError(
            "completed-idle resume requires every acceptance condition satisfied"
        )
    units = state.get("units", [])
    if not isinstance(units, list) or any(
        not isinstance(unit, dict) or unit.get("status") != "done" for unit in units
    ):
        raise ValueError("completed-idle resume requires every completion unit done")
    if state.get("paused") is True or state.get("blocker") is not None:
        raise ValueError(
            "completed-idle resume cannot start from paused or blocked state"
        )
    if snapshot.get("active_scope") is not None:
        raise ValueError("completed-idle resume requires no active task scope")
    config = tomllib.loads(source["config"].read_text())
    provider = config.get("model_provider")
    providers = config.get("model_providers", {})
    if not isinstance(provider, str) or not isinstance(providers, dict):
        raise ValueError(
            "completed-idle resume requires the configured source provider"
        )
    provider_config = providers.get(provider)
    if not isinstance(provider_config, dict) or not isinstance(
        provider_config.get("base_url"), str
    ):
        raise ValueError(
            "completed-idle resume requires a configured provider base URL"
        )
    source["provider"] = provider
    source["provider_base_url"] = provider_config["base_url"]


def _app_server_send(proc, request):
    message = json.dumps(request, separators=(",", ":")) + "\n"
    proc.stdin.write(message.encode("utf-8"))
    proc.stdin.flush()


def _app_server_read_message(proc, deadline, pending_bytes=b""):
    """Read one JSONL app-server message without treating EOF as readiness."""
    while time.monotonic() < deadline:
        if b"\n" in pending_bytes:
            line, pending_bytes = pending_bytes.split(b"\n", 1)
            if line:
                return json.loads(line), pending_bytes
        remaining = max(0.0, deadline - time.monotonic())
        ready, _, _ = select.select([proc.stdout], [], [], min(remaining, 0.25))
        if not ready:
            if proc.poll() is not None:
                raise RuntimeError("app-server exited before the required response")
            continue
        chunk = os.read(proc.stdout.fileno(), 65536)
        if not chunk:
            raise RuntimeError("app-server stdout closed before the required response")
        pending_bytes += chunk
    raise TimeoutError("app-server response timed out")


def _app_server_request(
    proc, request_id, method, params, deadline, pending=b"", notifications=None
):
    _app_server_send(
        proc,
        {"jsonrpc": "2.0", "id": request_id, "method": method, "params": params},
    )
    while time.monotonic() < deadline:
        message, pending = _app_server_read_message(proc, deadline, pending)
        if message.get("id") != request_id:
            if notifications is not None:
                notifications.append(message)
            continue
        if "error" in message:
            raise RuntimeError(f"app-server {method} failed: {message['error']}")
        if not isinstance(message.get("result"), dict):
            raise RuntimeError(f"app-server {method} returned no result object")
        return message["result"], pending
    raise TimeoutError(f"app-server {method} timed out")


def _completed_idle_evidence_is_sufficient(evidence):
    if not isinstance(evidence, dict):
        return False
    counts = evidence.get("new_turn_counts")
    return bool(
        evidence.get("configured") is True
        and evidence.get("ready_signal")
        == "successful thread/resume response after native SessionConfiguredEvent"
        and evidence.get("idle_observed") is True
        and evidence.get("observation_seconds", 0) >= 60
        and evidence.get("same_thread_id") is True
        and evidence.get("same_workspace") is True
        and evidence.get("same_rollout_path") is True
        and evidence.get("host_snapshot_unchanged") is True
        and evidence.get("child_started") is False
        and evidence.get("protocol_error") is False
        and evidence.get("new_tool_call") is False
        and evidence.get("provider_input_count") == 0
        and isinstance(counts, dict)
        and counts.get("started_turn_ids") == []
        and counts.get("completed_turn_ids") == []
    )


def _run_completed_idle_resume(
    command,
    env,
    stderr_path,
    rollout,
    thread_id,
    workspace,
    source_snapshot,
    old_line_count,
    provider_input_dir,
    observation_seconds=60,
    startup_timeout=60,
):
    """Cold-resume over native app-server stdio and observe the idle thread."""
    deadline = time.monotonic() + startup_timeout
    with Path(stderr_path).open("wb") as stderr:
        proc = subprocess.Popen(
            command,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=stderr,
            env=env,
            cwd=workspace,
            shell=False,
            start_new_session=True,
        )
        pending = b""
        notifications = []
        configured = False
        child_started = False
        protocol_error = False
        try:
            _, pending = _app_server_request(
                proc,
                1,
                "initialize",
                {
                    "clientInfo": {
                        "name": "autonomous-completion-probe",
                        "version": "1",
                    },
                    "capabilities": {"experimentalApi": True},
                },
                deadline,
                notifications=notifications,
            )
            _app_server_send(proc, {"jsonrpc": "2.0", "method": "initialized"})
            response, pending = _app_server_request(
                proc,
                2,
                "thread/resume",
                {
                    "threadId": thread_id,
                    "cwd": str(workspace),
                    "model": MODEL,
                    "deferAutonomousCompletion": False,
                    "excludeTurns": True,
                },
                deadline,
                pending,
                notifications,
            )
            thread = response.get("thread", {})
            returned_path = thread.get("path")
            same_rollout_path = (
                isinstance(returned_path, str)
                and Path(returned_path).resolve() == Path(rollout).resolve()
            )
            configured = thread.get("id") == thread_id and str(
                Path(response.get("cwd", "")).resolve()
            ) == str(Path(workspace).resolve())
            if not configured or not same_rollout_path:
                raise RuntimeError(
                    "thread/resume response did not confirm the requested thread, workspace, and source rollout path"
                )

            child_started = any(
                message.get("method") == "thread/started"
                and isinstance(message.get("params"), dict)
                and message["params"].get("thread", {}).get("parentThreadId")
                == thread_id
                for message in notifications
            )
            protocol_error = any(
                message.get("error") is not None
                or message.get("method")
                in {"error", "server/error", "turn/failed", "thread/failed"}
                for message in notifications
            )
            idle_deadline = time.monotonic() + observation_seconds
            while (
                time.monotonic() < idle_deadline
                and not child_started
                and not protocol_error
            ):
                # Keep consuming protocol notifications; a child start is never a quiet resume.
                ready, _, _ = select.select(
                    [proc.stdout], [], [], min(0.25, idle_deadline - time.monotonic())
                )
                if ready:
                    message, pending = _app_server_read_message(
                        proc, min(idle_deadline, time.monotonic() + 0.5), pending
                    )
                    method = message.get("method", "")
                    params = message.get("params", {})
                    child = params.get("thread", {}) if isinstance(params, dict) else {}
                    if message.get("error") is not None or method in {
                        "error",
                        "server/error",
                        "turn/failed",
                        "thread/failed",
                    }:
                        protocol_error = True
                        break
                    if (
                        method == "thread/started"
                        and child.get("parentThreadId") == thread_id
                    ):
                        child_started = True
                        break
                if proc.poll() is not None:
                    break
                counts = _new_turn_counts(rollout, old_line_count)
                snapshot = _latest_host_completion_snapshot(rollout, thread_id)
                appended = parse_events(rollout)[old_line_count:]
                new_tool_calls = any(
                    row.get("type") == "response_item"
                    and isinstance(row.get("payload"), dict)
                    and row["payload"].get("type")
                    in {"function_call", "custom_tool_call"}
                    for _, row in appended
                )
                provider_calls = list(Path(provider_input_dir).glob("*.json"))
                if (
                    counts["started_turn_ids"]
                    or counts["completed_turn_ids"]
                    or new_tool_calls
                    or snapshot != source_snapshot
                    or provider_calls
                ):
                    break
            counts = _new_turn_counts(rollout, old_line_count)
            snapshot = _latest_host_completion_snapshot(rollout, thread_id)
            provider_calls = list(Path(provider_input_dir).glob("*.json"))
            new_tool_call = any(
                row.get("type") == "response_item"
                and isinstance(row.get("payload"), dict)
                and row["payload"].get("type") in {"function_call", "custom_tool_call"}
                for _, row in parse_events(rollout)[old_line_count:]
            )
            host_snapshot_unchanged = snapshot == source_snapshot
            idle_observed = (
                configured
                and not child_started
                and not protocol_error
                and proc.poll() is None
                and host_snapshot_unchanged
                and not counts["started_turn_ids"]
                and not counts["completed_turn_ids"]
                and not _unresolved_tool_call_ids(rollout)
                and not new_tool_call
                and not provider_calls
                and time.monotonic() >= idle_deadline
            )
            return {
                "configured": configured,
                "ready_signal": "successful thread/resume response after native SessionConfiguredEvent",
                "idle_observed": idle_observed,
                "observation_seconds": observation_seconds,
                "child_started": child_started,
                "protocol_error": protocol_error,
                "host_snapshot_unchanged": host_snapshot_unchanged,
                "new_tool_call": new_tool_call,
                "provider_input_count": len(provider_calls),
                "new_turn_counts": counts,
                "same_thread_id": thread.get("id") == thread_id,
                "same_workspace": str(Path(response.get("cwd", "")).resolve())
                == str(Path(workspace).resolve()),
                "same_rollout_path": same_rollout_path,
                "returned_rollout_path": returned_path,
            }
        finally:
            try:
                if proc.stdin and not proc.stdin.closed:
                    proc.stdin.close()
                if proc.poll() is None:
                    try:
                        proc.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        _terminate_group(proc, signal.SIGTERM)
                        try:
                            proc.wait(timeout=5)
                        except subprocess.TimeoutExpired:
                            _terminate_group(proc, signal.SIGKILL)
                            proc.wait()
            finally:
                if proc.stdout and not proc.stdout.closed:
                    proc.stdout.close()


def _new_turn_counts(rollout_path, old_line_count):
    rows = parse_events(rollout_path)
    appended = [row for _, row in rows[old_line_count:]]
    started = [
        (index, row.get("payload", {}).get("turn_id"))
        for index, row in enumerate(appended)
        if row.get("type") == "event_msg"
        and row.get("payload", {}).get("type") == "task_started"
    ]
    completed = [
        (index, row.get("payload", {}).get("turn_id"))
        for index, row in enumerate(appended)
        if row.get("type") == "event_msg"
        and row.get("payload", {}).get("type") == "task_complete"
    ]
    user_messages = []
    for index, row in enumerate(appended):
        payload = row.get("payload", {})
        if row.get("type") != "response_item" or not isinstance(payload, dict):
            continue
        if payload.get("type") != "message" or payload.get("role") != "user":
            continue
        content = payload.get("content", [])
        text = "".join(
            item.get("text", "")
            for item in content
            if isinstance(item, dict) and item.get("type") in {"input_text", "text"}
        )
        user_messages.append(
            {
                "row_index": index,
                "text_sha256": hashlib.sha256(text.encode()).hexdigest(),
            }
        )
    turn_contexts = [
        index for index, row in enumerate(appended) if row.get("type") == "turn_context"
    ]
    return {
        "started_turn_ids": [turn_id for _, turn_id in started],
        "completed_turn_ids": [turn_id for _, turn_id in completed],
        "first_started_row_index": started[0][0] if started else None,
        "first_user_message": user_messages[0] if user_messages else None,
        "new_user_message_count": len(user_messages),
        "first_turn_context_row_index": turn_contexts[0] if turn_contexts else None,
    }


def _resume_terminal_classification(rollout, thread_id, old_line_count):
    """Return a safe idle terminal state, never infer completion from process exit."""
    counts = _new_turn_counts(rollout, old_line_count)
    completed = set(counts["completed_turn_ids"])
    if (
        not counts["started_turn_ids"]
        or any(turn_id is None for turn_id in counts["started_turn_ids"])
        or not set(counts["started_turn_ids"]).issubset(completed)
    ):
        return None
    snapshot = _latest_host_completion_snapshot(rollout, thread_id)
    if not isinstance(snapshot, dict):
        return None
    if snapshot.get("uncollected_children"):
        return None
    if any(
        not isinstance(attempt, dict) or not attempt.get("completed_successfully")
        for _, attempt in snapshot.get("patch_attempts", [])
    ):
        return None
    for check in snapshot.get("task_checks", []):
        attempt = check.get("attempt", {}) if isinstance(check, dict) else {}
        status = attempt.get("status") if isinstance(attempt, dict) else None
        if status == "Pending" or (isinstance(status, dict) and "Pending" in status):
            return None
    if _unresolved_tool_call_ids(rollout):
        return None

    contract = snapshot.get("state", {})
    conditions = contract.get("conditions", []) if isinstance(contract, dict) else []
    all_satisfied = bool(conditions) and all(
        isinstance(condition, dict) and condition.get("status") == "satisfied"
        for condition in conditions
    )
    if all_satisfied:
        return "completed_conditions"
    if isinstance(contract, dict) and contract.get("paused") is True:
        return "paused_wait"
    blocker = contract.get("blocker") if isinstance(contract, dict) else None
    if isinstance(blocker, dict):
        category = blocker.get("category")
        reason = blocker.get("reason")
        has_concrete_detail = (
            (category == "user_input" and bool(blocker.get("required_input")))
            or (category == "authority" and bool(blocker.get("authority")))
            or (
                category == "external_service"
                and bool(blocker.get("recheck_condition"))
            )
        )
        if (
            category in {"user_input", "authority", "external_service"}
            and reason
            and has_concrete_detail
        ):
            return "blocked_wait"
    return None


def _run_resume_tui(command, env, terminal_log, timeout, terminal_state=None):
    """Run the native resume TUI in a private PTY and bound the test restart."""
    master_fd, slave_fd = pty.openpty()
    try:
        import fcntl

        fcntl.ioctl(slave_fd, termios.TIOCSWINSZ, struct.pack("HHHH", 32, 120, 0, 0))
        with Path(terminal_log).open("wb") as log:
            process = subprocess.Popen(
                command,
                stdin=slave_fd,
                stdout=slave_fd,
                stderr=slave_fd,
                env=env,
                shell=False,
                start_new_session=True,
                close_fds=True,
            )
            os.close(slave_fd)
            slave_fd = -1
            deadline = time.monotonic() + timeout
            terminal_classification = None
            last_state_check = 0.0
            while process.poll() is None and time.monotonic() < deadline:
                ready, _, _ = select.select([master_fd], [], [], 0.25)
                if ready:
                    try:
                        chunk = os.read(master_fd, 65536)
                    except OSError as error:
                        if error.errno == errno.EIO:
                            break
                        raise
                    if chunk:
                        log.write(chunk)
                        log.flush()
                if (
                    terminal_state is not None
                    and time.monotonic() - last_state_check >= 1.0
                ):
                    last_state_check = time.monotonic()
                    terminal_classification = terminal_state()
                    if terminal_classification is not None:
                        # TUI is idle only after a completed turn and host-verified terminal
                        # contract/blocker state. EOF is test-apparatus shutdown, not a user
                        # continuation or a completion claim.
                        try:
                            os.write(master_fd, b"\x04")
                        except OSError:
                            pass
                        try:
                            process.wait(timeout=5)
                        except subprocess.TimeoutExpired:
                            _terminate_group(process, signal.SIGTERM)
                            try:
                                process.wait(timeout=5)
                            except subprocess.TimeoutExpired:
                                _terminate_group(process, signal.SIGKILL)
                                process.wait()
                        break
            if terminal_state is not None and terminal_classification is None:
                terminal_classification = terminal_state()
            timed_out = process.poll() is None and terminal_classification is None
            if timed_out:
                _terminate_group(process, signal.SIGTERM)
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    _terminate_group(process, signal.SIGKILL)
                    process.wait()
            else:
                process.wait()
            # Drain final terminal output after process exit.
            while True:
                ready, _, _ = select.select([master_fd], [], [], 0.1)
                if not ready:
                    break
                try:
                    chunk = os.read(master_fd, 65536)
                except OSError as error:
                    if error.errno == errno.EIO:
                        break
                    raise
                if not chunk:
                    break
                log.write(chunk)
            return process.returncode, timed_out, terminal_classification
    finally:
        if slave_fd >= 0:
            os.close(slave_fd)
        os.close(master_fd)


def run_resume_probe(
    source_run, output, mode, timeout=1800, prompt_file=None, capture_http=False
):
    """Resume the source thread in place; only native rollout append is allowed."""
    if mode not in {
        "quiet_autonomous_resume",
        "initial_prompt_race",
        "completed_idle_resume",
    }:
        raise ValueError("mode must distinguish quiet resume from initial-prompt race")
    if timeout < 1 or timeout > MAX_RESUME_TIMEOUT_SECONDS:
        raise ValueError(
            f"timeout must be between 1 and {MAX_RESUME_TIMEOUT_SECONDS} seconds"
        )
    prompt = None
    if mode == "initial_prompt_race":
        if prompt_file is None:
            raise ValueError("initial-prompt-race mode requires --prompt-file")
        prompt = Path(prompt_file).read_text().strip()
        if not prompt:
            raise ValueError("initial prompt file must contain non-empty text")
    elif prompt_file is not None:
        raise ValueError("quiet autonomous resume cannot include an initial prompt")
    if mode == "completed_idle_resume" and not capture_http:
        raise ValueError(
            "completed-idle resume requires --capture-http to observe actual provider input"
        )
    if mode == "completed_idle_resume" and timeout < 120:
        raise ValueError(
            "completed-idle resume needs at least 120 seconds for restore and idle observation"
        )

    source = _load_resume_source(source_run)
    if mode == "completed_idle_resume":
        _validate_completed_idle_source(source)
    output = Path(output).resolve()
    if source["source_run"] not in output.parents:
        raise ValueError(
            "resume evidence must be in a separate subdirectory of the source run"
        )
    if source["home"] in output.parents or source["workspace"] in output.parents:
        raise ValueError(
            "resume evidence cannot be written inside CODEX_HOME or the workspace"
        )
    if output.exists():
        raise ValueError("resume output already exists")

    source_rollout_bytes = source["rollout"].read_bytes()
    source_rollout_hash = hashlib.sha256(source_rollout_bytes).hexdigest()
    source_rollout_line_count = len(parse_events(source["rollout"]))
    config_hash_before = _sha256_file(source["config"])
    binary_hash_before = _sha256_file(source["binary"])
    output.mkdir(parents=True)
    metadata = {
        "kind": "test_apparatus_same_thread_restart",
        "mode": mode,
        "transport": "app_server_stdio"
        if mode == "completed_idle_resume"
        else "tui_pty",
        "source_run": str(source["source_run"]),
        "thread_id_from_rollout": source["thread_id"],
        "completion_snapshot_root_task_id": source["completion_snapshot"].get(
            "root_task_id"
        ),
        "source_process_exit_observed": {
            "process_status": source["result"]["process_status"],
            "exit_code": source["result"]["exit_code"],
            "basis": "source result is written only after subprocess.communicate returns",
        },
        "source_rollout_sha256_before": source_rollout_hash,
        "source_rollout_line_count_before": source_rollout_line_count,
        "binary_sha256_before": binary_hash_before,
        "config_sha256_before": config_hash_before,
        "initial_prompt_sha256": hashlib.sha256(prompt.encode()).hexdigest()
        if prompt is not None
        else None,
        "timeout_seconds": timeout,
        "test_apparatus_restart": True,
        "resume_issued_without_prompt": mode
        in {"quiet_autonomous_resume", "completed_idle_resume"},
        "external_steer": prompt is not None,
        "initial_prompt_race_is_diagnostic_only": mode == "initial_prompt_race",
    }
    (output / "resume-metadata.json").write_text(json.dumps(metadata, indent=2) + "\n")
    env = os.environ.copy()
    env["CODEX_HOME"] = str(source["home"])
    env["PATH"] = str(source["binary"].parent) + os.pathsep + env.get("PATH", "")
    command = resume_command(source, source["workspace"], mode, prompt)
    capture = ExitStack()
    provider_override = None
    if capture_http:
        parsed_config = tomllib.loads(source["config"].read_text())
        provider = parsed_config["model_provider"]
        upstream = parsed_config["model_providers"][provider]["base_url"]
        proxy = capture.enter_context(
            capture_requests(output / "provider-inputs", upstream)
        )
        provider_override = f"model_providers.{provider}.base_url={json.dumps(proxy)}"
        if mode == "completed_idle_resume":
            source["manifest"]["provider"] = provider
            command = completed_idle_resume_command(source, source["workspace"], proxy)
        else:
            command[-2:-2] = ["-c", provider_override]
    (output / "resume-command.json").write_text(
        json.dumps(
            {
                "argv": [
                    "<initial prompt omitted>"
                    if prompt is not None and part == prompt
                    else part
                    for part in command
                ],
                "binary": str(source["binary"]),
                "provider_capture_override": provider_override,
            },
            indent=2,
        )
        + "\n"
    )
    completed_idle_evidence = None
    try:
        if mode == "completed_idle_resume":
            try:
                completed_idle_evidence = _run_completed_idle_resume(
                    command,
                    env,
                    output / "stderr.log",
                    source["rollout"],
                    source["thread_id"],
                    source["workspace"],
                    source["completion_snapshot"],
                    source_rollout_line_count,
                    output / "provider-inputs",
                    observation_seconds=60,
                    startup_timeout=min(timeout - 60, 120),
                )
            except (
                OSError,
                RuntimeError,
                TimeoutError,
                ValueError,
                TypeError,
                AttributeError,
                KeyError,
            ) as error:
                completed_idle_evidence = {
                    "configured": False,
                    "idle_observed": False,
                    "observation_seconds": 0,
                    "reason": str(error),
                }
            completed_idle_ok = _completed_idle_evidence_is_sufficient(
                completed_idle_evidence
            )
            exit_code = 0 if completed_idle_ok else 1
            timed_out = False
            terminal_classification = (
                "completed_idle_restored" if completed_idle_ok else None
            )
        else:
            exit_code, timed_out, terminal_classification = _run_resume_tui(
                command,
                env,
                output / "terminal.log",
                timeout,
                terminal_state=lambda: _resume_terminal_classification(
                    source["rollout"], source["thread_id"], source_rollout_line_count
                ),
            )
    finally:
        capture.close()
    source_rollout_after = source["rollout"].read_bytes()
    source_rollout_hash_after = hashlib.sha256(source_rollout_after).hexdigest()
    source_config_hash_after = _sha256_file(source["config"])
    source_binary_hash_after = _sha256_file(source["binary"])
    counts = _new_turn_counts(source["rollout"], source_rollout_line_count)
    prompt_match = (
        counts["first_user_message"]["text_sha256"] == metadata["initial_prompt_sha256"]
        if prompt is not None and counts["first_user_message"]
        else False
    )
    first_context_index = counts["first_turn_context_row_index"]
    prompt_index = (
        counts["first_user_message"].get("row_index")
        if counts["first_user_message"] and prompt_match
        else None
    )
    prompt_was_before_first_model_context = (
        prompt_index is not None
        and first_context_index is not None
        and prompt_index < first_context_index
    )
    result = {
        **metadata,
        "process_status": (
            "safe_terminal_state_reached"
            if terminal_classification is not None
            else (
                "timeout"
                if timed_out
                else ("completed" if exit_code == 0 else "failed")
            )
        ),
        "exit_code": exit_code,
        "safe_terminal_classification": terminal_classification,
        "completed_idle_evidence": completed_idle_evidence,
        "test_apparatus_exit_after_safe_terminal_state": terminal_classification
        is not None,
        "source_rollout_prefix_preserved": source_rollout_after.startswith(
            source_rollout_bytes
        ),
        "source_rollout_sha256_after": source_rollout_hash_after,
        "source_rollout_append_bytes": max(
            0, len(source_rollout_after) - len(source_rollout_bytes)
        ),
        "source_config_unchanged": source_config_hash_after == config_hash_before,
        "source_binary_unchanged": source_binary_hash_after == binary_hash_before,
        "initial_prompt_message_hash_matches": prompt_match
        if prompt is not None
        else None,
        "initial_prompt_before_first_model_context": (
            prompt_was_before_first_model_context if prompt is not None else None
        ),
        "resume_issued_without_prompt": mode
        in {"quiet_autonomous_resume", "completed_idle_resume"},
        "manual_or_prompt_resume_is_not_autonomous_acceptance": mode
        == "initial_prompt_race",
        "resumed_rollout": str(source["rollout"].relative_to(source["source_run"])),
        "new_turns_observed": counts,
        "provider_input_count": len(list((output / "provider-inputs").glob("*.json")))
        if capture_http
        else None,
        "status": "needs_review",
        "note": (
            "Native app-server stdio same-thread restore was observed idle for 60 seconds; this is not a TUI trial and completion acceptance remains needs_review."
            if mode == "completed_idle_resume"
            else "Native same-thread resume evidence only; acceptance remains needs_review and requires host/model-action review."
        ),
    }
    (output / "resume-result.json").write_text(json.dumps(result, indent=2) + "\n")
    if not (
        result["source_rollout_prefix_preserved"]
        and result["source_config_unchanged"]
        and result["source_binary_unchanged"]
    ):
        return result, 1
    return result, 0 if terminal_classification is not None else 1


def parent_rollout(paths):
    """Select the actual CLI parent; a newer child is never a substitute."""
    matches = []
    for path in paths:
        with path.open() as stream:
            row = json.loads(stream.readline())
        metadata = row.get("payload", {})
        if row.get("type") == "session_meta" and metadata.get("source") == "exec":
            matches.append(path)
    if len(matches) != 1:
        return None
    return matches[0]


def parse_events(path):
    """Read CLI JSON events or rollout JSONL without interpreting prose as actions."""
    events = []
    for line_no, line in enumerate(
        Path(path).read_text(errors="replace").splitlines(), 1
    ):
        try:
            row = json.loads(line)
        except json.JSONDecodeError:
            continue
        events.append((line_no, row))
    return events


def _walk_key(value, key, path="$", found=None):
    if found is None:
        found = []
    if isinstance(value, dict):
        for name, child in value.items():
            child_path = f"{path}.{name}"
            if name == key:
                found.append((child_path, child))
            _walk_key(child, key, child_path, found)
    elif isinstance(value, list):
        for index, child in enumerate(value):
            _walk_key(child, key, f"{path}[{index}]", found)
    return found


def _time_ns(value):
    if not isinstance(value, str) or not value:
        return None
    try:
        parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
        return int(parsed.timestamp()) * 1_000_000_000 + parsed.microsecond * 1_000
    except ValueError:
        return None


def _provider_inputs(run_dir):
    refs = []
    directory = Path(run_dir) / "provider-inputs"
    if not directory.is_dir():
        return refs
    for path in sorted(directory.glob("*.json")):
        try:
            raw = path.read_bytes()
            record = json.loads(raw)
        except (OSError, json.JSONDecodeError):
            continue
        body = record.get("body") if isinstance(record, dict) else None
        body = body if isinstance(body, dict) else {}
        items = body.get("input")
        refs.append(
            {
                "path": str(path.relative_to(run_dir)),
                "sha256": hashlib.sha256(raw).hexdigest(),
                "ordinal": record.get("ordinal"),
                "method": record.get("method"),
                "endpoint": record.get("path"),
                "time_ns": record.get("time_ns"),
                "model": body.get("model"),
                "input_item_count": len(items) if isinstance(items, list) else None,
                "input_item_types": [
                    item.get("type") for item in items if isinstance(item, dict)
                ]
                if isinstance(items, list)
                else None,
            }
        )
    return refs


def _call_action(call):
    if call.get("name") != "task_context_boundary":
        return None, None
    try:
        args = json.loads(call.get("arguments") or "{}")
    except (json.JSONDecodeError, TypeError):
        return None, None
    action = args.get("action") if isinstance(args, dict) else None
    task_id = args.get("task_id") if isinstance(args, dict) else None
    return action if isinstance(action, str) else None, task_id if isinstance(
        task_id, str
    ) else None


def build_evidence_index(run_dir, events, rollout_path=None):
    """Index evidence by stable references; never copy prompt or summary bodies."""
    run_dir = Path(run_dir).resolve()

    def read_json(name):
        try:
            return json.loads((run_dir / name).read_text())
        except (OSError, json.JSONDecodeError):
            return {}

    manifest = read_json("manifest.json")
    result_file = read_json("result.json")
    commits, completion_states, checkpoint_metadata, calls = [], [], [], []
    for line, row in events:
        payload = row.get("payload", {}) if isinstance(row.get("payload"), dict) else {}
        if row.get("type") == "rencrow_compaction_commit":
            checkpoint_hash = payload.get("checkpoint_hash")
            if isinstance(checkpoint_hash, str) and checkpoint_hash:
                commits.append(
                    {
                        "line": line,
                        "timestamp": row.get("timestamp"),
                        "time_ns": _time_ns(row.get("timestamp")),
                        "checkpoint_hash": checkpoint_hash,
                    }
                )
        for key, target, rows in (
            ("rencrow_completion", "completion", completion_states),
            ("rencrow_compaction", "compaction", checkpoint_metadata),
        ):
            for json_path, value in _walk_key(row, key):
                if not isinstance(value, dict):
                    continue
                ref = {"line": line, "json_path": json_path}
                if target == "completion":
                    state = value.get("state")
                    ref.update(
                        {
                            "root_task_id": value.get("root_task_id"),
                            "contract_revision": state.get("contract_revision")
                            if isinstance(state, dict)
                            else None,
                            "paused": state.get("paused")
                            if isinstance(state, dict)
                            else None,
                        }
                    )
                else:
                    ref.update(
                        {
                            "version": value.get("version"),
                            "mode": value.get("mode"),
                            "snapshot_hash": value.get("snapshot_hash"),
                            "summary_hash": value.get("summary_hash"),
                            "task_state_present": isinstance(
                                value.get("task_state"), dict
                            ),
                        }
                    )
                rows.append(ref)
        if row.get("type") == "response_item" and payload.get("type") in {
            "function_call",
            "custom_tool_call",
        }:
            calls.append(
                {
                    "line": line,
                    "name": payload.get("name"),
                    "arguments": payload.get("arguments", ""),
                }
            )
        item = row.get("item", {}) if isinstance(row.get("item"), dict) else {}
        if row.get("type") == "item.completed" and item.get("type") in {
            "tool_call",
            "function_call",
        }:
            calls.append(
                {
                    "line": line,
                    "name": item.get("name"),
                    "arguments": item.get("arguments", ""),
                }
            )

    provider_refs = _provider_inputs(run_dir)
    for request in provider_refs:
        request["_time_ns"] = request.get("time_ns")
    host_refs = list(
        {(ref["line"], ref["json_path"]): ref for ref in completion_states}.values()
    )

    commit_evidence = []
    e1_sections = []
    for index, commit in enumerate(commits):
        next_commit = commits[index + 1] if index + 1 < len(commits) else None
        scoped_metadata = [
            ref
            for ref in checkpoint_metadata
            if (index == 0 or ref["line"] > commits[index - 1]["line"])
            and ref["line"] < commit["line"]
        ]
        previous_commit_line = commits[index - 1]["line"] if index else 0
        s0_candidates = [
            ref
            for ref in host_refs
            if previous_commit_line < ref["line"] < commit["line"]
        ]
        calls_after = [
            call
            for call in calls
            if call["line"] > commit["line"]
            and (next_commit is None or call["line"] < next_commit["line"])
        ]
        actions = []
        for call in calls_after:
            action, task_id = _call_action(call)
            actions.append(
                {
                    "line": call["line"],
                    "name": call.get("name"),
                    "action": action,
                    "task_id": task_id,
                }
            )
        first_accept = next(
            (item for item in actions if item["action"] == "accept"), None
        )
        e1_sections.append(
            {
                "commit_ref": {
                    "line": commit["line"],
                    "checkpoint_hash": commit["checkpoint_hash"],
                },
                "first_five_tool_calls": actions[:5],
                "next_accept": first_accept,
            }
        )
        commit_time = commit.get("time_ns")
        next_time = next_commit.get("time_ns") if next_commit else None
        post_requests = [
            request
            for request in provider_refs
            if isinstance(commit_time, int)
            and isinstance(request.get("_time_ns"), int)
            and request["_time_ns"] > commit_time
            and (next_time is None or request["_time_ns"] < next_time)
        ]
        pre_requests = [
            request
            for request in provider_refs
            if isinstance(commit_time, int)
            and isinstance(request.get("_time_ns"), int)
            and request["_time_ns"] < commit_time
            and (
                index == 0
                or request["_time_ns"] > (commits[index - 1].get("time_ns") or 0)
            )
        ]
        output_refs = [
            ref
            for ref in scoped_metadata
            if isinstance(ref.get("summary_hash"), str) and ref["summary_hash"]
        ]
        commit_evidence.append(
            {
                **{key: value for key, value in commit.items() if key != "time_ns"},
                "S0": {
                    "status": "unknown" if not s0_candidates else "stage_unverified",
                    "refs": s0_candidates,
                },
                "S1": {
                    "status": "unknown" if not scoped_metadata else "stage_unverified",
                    "refs": scoped_metadata,
                },
                "S0_host_snapshot_candidates": s0_candidates,
                "S1_checkpoint_metadata_refs": scoped_metadata,
                "summary_output": {
                    "status": "identified_by_hash" if output_refs else "unknown",
                    "refs": output_refs,
                },
                "summary_output_refs": output_refs,
                "I0": {"status": "unknown", "candidate_provider_inputs": pre_requests},
                "I1": {
                    "status": "unknown"
                    if not post_requests
                    else "first_postcommit_request_candidate",
                    "ref": post_requests[0] if post_requests else None,
                    "ordinary_input_role": "unknown",
                },
            }
        )

    cold_resume_refs = []
    for line, row in events:
        if row.get("type") != "session_meta":
            continue
        payload = row.get("payload", {})
        if isinstance(payload, dict):
            cold_resume_refs.extend(
                {"line": line, "json_path": path}
                for path, value in _walk_key(payload, "cold_resume")
                if value is True
            )
    for request in provider_refs:
        request.pop("_time_ns", None)
    return {
        "case": result_file.get("case", manifest.get("case")),
        "process_status": result_file.get("process_status"),
        "exit_code": result_file.get("exit_code"),
        "status": "not_run" if not commits else "needs_review",
        "rollout": str(Path(rollout_path).resolve().relative_to(run_dir))
        if rollout_path
        else None,
        "commit_count": len(commits),
        "multiple_commits": len(commits) > 1,
        "cold_resume": "identified" if cold_resume_refs else "unknown",
        "cold_resume_refs": cold_resume_refs,
        "host_completion_snapshot_refs": completion_states,
        "S0": {
            "status": "unknown" if not host_refs else "stage_unverified",
            "refs": host_refs,
        },
        "S1": {
            "status": "unknown" if not checkpoint_metadata else "stage_unverified",
            "refs": checkpoint_metadata,
        },
        "summary_input": {
            "status": "unknown",
            "reason": "no explicit structured summary-input marker",
        },
        "commits": commit_evidence,
        "provider_input_refs": provider_refs,
        "E1": e1_sections,
        "note": "References and structural metadata only; semantic stage matching requires review. Missing evidence remains unknown.",
    }


def index_existing_run(run_dir, output_path):
    run_dir = Path(run_dir).resolve()
    output_path = Path(output_path).resolve()
    if not run_dir.is_dir():
        raise ValueError("run directory must exist")
    if output_path == run_dir or run_dir in output_path.parents:
        raise ValueError("index output must be outside the original trial directory")
    rollout_files = (
        sorted(
            (run_dir / "home" / "sessions").rglob("rollout-*.jsonl"),
            key=lambda path: path.stat().st_mtime,
        )
        if (run_dir / "home" / "sessions").exists()
        else []
    )
    rollout = parent_rollout(rollout_files)
    events = parse_events(rollout) if rollout else []
    if not events and (run_dir / "events.jsonl").is_file():
        events = parse_events(run_dir / "events.jsonl")
    index = build_evidence_index(run_dir, events, rollout)
    output_path.parent.mkdir(parents=True, exist_ok=True)
    if output_path.exists():
        raise ValueError("index output already exists")
    output_path.write_text(json.dumps(index, indent=2) + "\n")
    return index


def summarize_events(events, case_id):
    calls, compactions = [], []
    usage = {}
    for n, row in events:
        typ = str(row.get("type", ""))
        payload = row.get("payload", {}) if isinstance(row.get("payload"), dict) else {}
        item = row.get("item", {}) if isinstance(row.get("item"), dict) else {}
        if typ == "event_msg" and payload.get("type") == "token_count":
            info = payload.get("info") or {}
            usage = info.get("total_token_usage") or usage
        # Keep tool-call evidence only. Assistant text mentioning a tool is not a call.
        if typ == "item.completed" and item.get("type") in {
            "tool_call",
            "function_call",
        }:
            calls.append(
                {
                    "line": n,
                    "name": item.get("name"),
                    "arguments": item.get("arguments", ""),
                }
            )
        if typ == "response_item" and payload.get("type") in {
            "function_call",
            "custom_tool_call",
        }:
            calls.append(
                {
                    "line": n,
                    "name": payload.get("name"),
                    "arguments": payload.get("arguments", ""),
                }
            )
        # This is the native rollout record; candidates and similarly named events do not count.
        if (
            typ == "rencrow_compaction_commit"
            and isinstance(payload.get("checkpoint_hash"), str)
            and payload["checkpoint_hash"]
        ):
            compactions.append(
                {"line": n, "type": typ, "checkpoint_hash": payload["checkpoint_hash"]}
            )

    first_commit_line = compactions[0]["line"] if compactions else None
    post_commit_calls = [
        c
        for c in calls
        if first_commit_line is not None and c["line"] > first_commit_line
    ]
    starts = []
    for call in post_commit_calls:
        if call.get("name") != "task_context_boundary":
            continue
        try:
            args = json.loads(call.get("arguments") or "{}")
        except (json.JSONDecodeError, TypeError):
            continue
        if args.get("action") == "begin":
            target = args.get("task_id") or args.get("target_task_id")
            if isinstance(target, str) and target:
                starts.append({"line": call["line"], "task_id": target})
    start_ids = [x["task_id"] for x in starts]
    duplicate_ids = sorted({x for x in start_ids if start_ids.count(x) > 1})
    # Preserve evidence, do not infer whether an opaque argument semantically targets A/B.
    return {
        "case": case_id,
        "tool_call_count": len(calls),
        "first_tool_call": post_commit_calls[0] if post_commit_calls else None,
        "first_tool_call_position": "after_first_commit" if post_commit_calls else None,
        "post_commit_tool_call_count": len(post_commit_calls),
        "post_commit_begin_targets": starts,
        "duplicate_begin_target_count": sum(
            start_ids.count(x) - 1 for x in duplicate_ids
        ),
        "duplicate_begin_targets": duplicate_ids,
        "tool_calls": calls,
        "explicit_compaction_commit_count": len(compactions),
        "compaction_commits": compactions,
        "status": "not_run" if not compactions else "needs_review",
        "metrics": {
            "input_tokens": usage.get("input_tokens"),
            "output_tokens": usage.get("output_tokens"),
            "cache_tokens": usage.get("cached_input_tokens"),
            "scope": "parent cumulative usage; child usage must be reported separately",
            "cost": None,
            "baseline": "unknown",
        },
        "note": "No pass is assigned automatically; verify commit, first action, target IDs, acceptance, and prohibited repeats against raw evidence.",
    }


def set_config_flags(source, destination):
    text = Path(source).read_text()
    # TOML root settings must precede all tables. Remove only root-level assignments.
    lines = text.splitlines()
    first_table = next(
        (i for i, line in enumerate(lines) if re.match(r"^\s*\[", line)), len(lines)
    )
    root, rest = lines[:first_table], lines[first_table:]
    for key in ("rencrow_compaction", "rencrow_autonomous_completion"):
        root = [line for line in root if not re.match(rf"^\s*{key}\s*=", line)]
    text = (
        "\n".join(
            root
            + ["rencrow_compaction = true", "rencrow_autonomous_completion = true"]
            + rest
        )
        + "\n"
    )
    parsed = tomllib.loads(text)
    if (
        parsed.get("rencrow_compaction") is not True
        or parsed.get("rencrow_autonomous_completion") is not True
    ):
        raise ValueError("failed to set root-level compaction flags")
    Path(destination).write_text(text)


def main(argv=None):
    argv = sys.argv[1:] if argv is None else argv
    if argv and argv[0] == "resume":
        resume_parser = argparse.ArgumentParser(
            description="Restart one completed isolated probe thread in its existing home/workspace."
        )
        resume_parser.add_argument("source_run", type=Path)
        resume_parser.add_argument("--output", required=True, type=Path)
        resume_parser.add_argument(
            "--mode",
            required=True,
            choices=[
                "quiet_autonomous_resume",
                "initial_prompt_race",
                "completed_idle_resume",
            ],
            help="Select TUI resume, prompted TUI resume, or completed-state app-server idle restore.",
        )
        resume_parser.add_argument(
            "--prompt-file",
            type=Path,
            help="Required for initial_prompt_race; the native resume PROMPT positional argument.",
        )
        resume_parser.add_argument(
            "--timeout",
            type=int,
            default=MAX_RESUME_TIMEOUT_SECONDS,
            help=f"Bounded test-run timeout (max {MAX_RESUME_TIMEOUT_SECONDS}s).",
        )
        resume_parser.add_argument(
            "--capture-http",
            action="store_true",
            help="Capture actual provider inputs through the existing loopback proxy.",
        )
        resume_args = resume_parser.parse_args(argv[1:])
        try:
            result, status = run_resume_probe(
                resume_args.source_run,
                resume_args.output,
                resume_args.mode,
                resume_args.timeout,
                resume_args.prompt_file,
                resume_args.capture_http,
            )
        except (OSError, ValueError, json.JSONDecodeError) as error:
            resume_parser.error(str(error))
        print(json.dumps(result, indent=2))
        return status
    if argv and argv[0] == "index":
        index_parser = argparse.ArgumentParser(
            description="Build a reference-only evidence index for a completed probe run."
        )
        index_parser.add_argument("run_dir", type=Path)
        index_parser.add_argument("--output", required=True, type=Path)
        index_args = index_parser.parse_args(argv[1:])
        try:
            result = index_existing_run(index_args.run_dir, index_args.output)
        except (OSError, ValueError) as error:
            index_parser.error(str(error))
        print(
            json.dumps(
                {
                    "index_output": str(index_args.output.resolve()),
                    "case": result["case"],
                    "process_status": result["process_status"],
                    "exit_code": result["exit_code"],
                    "status": result["status"],
                    "commit_count": result["commit_count"],
                    "S0": result["S0"]["status"],
                    "S1": result["S1"]["status"],
                    "summary_input": result["summary_input"]["status"],
                    "cold_resume": result["cold_resume"],
                },
                indent=2,
            )
        )
        return 0
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--binary", required=True, type=Path)
    p.add_argument("--output", required=True, type=Path)
    p.add_argument(
        "--config-template",
        required=True,
        type=Path,
        help="Caller-supplied config preserving role routes; secrets are not copied into manifest",
    )
    p.add_argument(
        "--scenario",
        required=True,
        type=Path,
        help="Predefined case prompt/fixture; never production state",
    )
    p.add_argument("--case", required=True, choices=sorted(CASES))
    p.add_argument("--model", required=True, choices=[MODEL])
    p.add_argument("--timeout", type=int, default=900)
    p.add_argument(
        "--capture-http",
        action="store_true",
        help="Record actual loopback provider inputs in this isolated test",
    )
    a = p.parse_args(argv)
    binary, config, scenario = (
        x.resolve() for x in (a.binary, a.config_template, a.scenario)
    )
    out = a.output.resolve()
    if not binary.is_file() or not os.access(binary, os.X_OK):
        p.error("--binary must be an executable file")
    if not config.is_file() or not scenario.is_file():
        p.error("config template and scenario must exist")
    if a.timeout < 1:
        p.error("--timeout must be positive")
    out.mkdir(parents=True, exist_ok=False)
    home, workspace = out / "home", out / "workspace"
    home.mkdir()
    workspace.mkdir()
    shutil.copy2(scenario, workspace / "scenario.txt")
    set_config_flags(config, home / "config.toml")
    started = time.time()
    manifest = {
        "case": a.case,
        "model": a.model,
        "effort": "xhigh",
        "binary": str(binary),
        "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        "config_sha256": hashlib.sha256(
            (home / "config.toml").read_bytes()
        ).hexdigest(),
        "scenario_sha256": hashlib.sha256(scenario.read_bytes()).hexdigest(),
        "config_template": str(config),
        "workspace": str(workspace),
        "timeout_seconds": a.timeout,
        "created_unix": started,
        "isolation": "fresh CODEX_HOME and workspace; scenario only",
        "route": "caller-configured; credentials excluded from manifest",
        "baseline": "unknown",
    }
    (out / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    env = os.environ.copy()
    env["CODEX_HOME"] = str(home)
    env["PATH"] = str(binary.parent) + os.pathsep + env.get("PATH", "")
    argv_cmd = [
        str(binary),
        "--no-daemon",
        "exec",
        "--skip-git-repo-check",
        "--cd",
        str(workspace),
        "--json",
        "-m",
        a.model,
        "-c",
        'model_reasoning_effort="xhigh"',
        "-",
    ]
    status, rc = "failed", None
    capture = ExitStack()
    try:
        if a.capture_http:
            parsed = tomllib.loads((home / "config.toml").read_text())
            provider = parsed["model_provider"]
            upstream = parsed["model_providers"][provider]["base_url"]
            proxy = capture.enter_context(
                capture_requests(out / "provider-inputs", upstream)
            )
            argv_cmd[-1:-1] = [
                "-c",
                f"model_providers.{provider}.base_url={json.dumps(proxy)}",
            ]
        with (
            (out / "events.jsonl").open("wb") as log,
            (workspace / "scenario.txt").open("rb") as prompt,
        ):
            proc = subprocess.Popen(
                argv_cmd,
                stdin=prompt,
                stdout=log,
                stderr=subprocess.PIPE,
                env=env,
                shell=False,
                start_new_session=True,
            )
            try:
                _, stderr = proc.communicate(timeout=a.timeout)
                rc = proc.returncode
            except subprocess.TimeoutExpired:
                _terminate_group(proc, signal.SIGTERM)
                try:
                    _, stderr = proc.communicate(timeout=10)
                except subprocess.TimeoutExpired:
                    _terminate_group(proc, signal.SIGKILL)
                    _, stderr = proc.communicate()
                status = "timeout"
                rc = proc.returncode
            (out / "stderr.log").write_bytes(stderr or b"")
            if rc == 0:
                status = "completed"
    except KeyboardInterrupt:
        if "proc" in locals():
            _terminate_group(proc, signal.SIGTERM)
            try:
                _, stderr = proc.communicate(timeout=10)
            except subprocess.TimeoutExpired:
                _terminate_group(proc, signal.SIGKILL)
                _, stderr = proc.communicate()
            (out / "stderr.log").write_bytes(stderr or b"")
            rc = proc.returncode
        status = "interrupted"
    except (OSError, ValueError, tomllib.TOMLDecodeError) as e:
        (out / "stderr.log").write_text(str(e) + "\n")
    finally:
        capture.close()
    events = (
        parse_events(out / "events.jsonl") if (out / "events.jsonl").exists() else []
    )
    rollouts = sorted(
        (home / "sessions").rglob("rollout-*.jsonl"), key=lambda f: f.stat().st_mtime
    )
    rollout = parent_rollout(rollouts)
    rollout_events = parse_events(rollout) if rollout else []
    result = summarize_events(rollout_events or events, a.case)
    turn_contexts = [
        row.get("payload", {})
        for _, row in rollout_events
        if row.get("type") == "turn_context"
    ]
    actual_model = next((x.get("model") for x in turn_contexts if x.get("model")), None)
    actual_effort = next(
        (x.get("effort") for x in turn_contexts if x.get("effort")), None
    )
    result["session"] = {
        "rollout": str(rollout.relative_to(out)) if rollout else None,
        "model": actual_model,
        "effort": actual_effort,
        "matches_requested": actual_model == a.model and actual_effort == "xhigh",
    }
    index_events = rollout_events or events
    evidence_index = build_evidence_index(out, index_events, rollout)
    evidence_index.update({"case": a.case, "process_status": status, "exit_code": rc})
    (out / "evidence-index.json").write_text(
        json.dumps(evidence_index, indent=2) + "\n"
    )
    if rollout and not result["session"]["matches_requested"]:
        result["status"] = "failed_session_mismatch"
    result.update(
        {
            "process_status": status,
            "exit_code": rc,
            "duration_seconds": round(time.time() - started, 3),
            "raw_events": "events.jsonl",
            "stderr": "stderr.log",
            "cli_events": "events.jsonl",
            "evidence_index": "evidence-index.json",
        }
    )
    if result["explicit_compaction_commit_count"] == 0:
        result["status"] = "not_run"
    (out / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result, indent=2))
    return (
        0 if status == "completed" and result["explicit_compaction_commit_count"] else 1
    )


def _terminate_group(proc, sig):
    try:
        os.killpg(proc.pid, sig)
    except ProcessLookupError:
        pass


if __name__ == "__main__":
    sys.exit(main())
