"""Bounded host-side apparatus for autonomous-completion lifecycle cases.

C03 runs the parent turn through native app-server stdio, watches the host rollout for
the real V1 child identity, and compacts the same parent thread while the child's
workspace gate command is host-visible as in progress. It never edits rollout/SQLite
state or sends a second model input.
"""

from contextlib import ExitStack
import hashlib
import json
import os
from pathlib import Path
import select
import shlex
import signal
import subprocess
import sys
import time
import tomllib
import uuid

from autonomous_completion_capture import capture_requests
from autonomous_completion_probe import (
    _app_server_read_message,
    _app_server_request,
    _app_server_send,
    _sha256_file,
    _terminate_group,
    parse_events,
)

MAX_C03_TIMEOUT_SECONDS = 1800
DEFAULT_C03_GATE_TIMEOUT_SECONDS = 1200
GATE_NAME = "c03_child_gate.py"
RELEASE_NAME = ".c03-release"
GATE_STARTED_NAME = ".c03-gate-started"


def _write_new(path: Path, content: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "wb") as stream:
        stream.write(content)
        stream.flush()
        os.fsync(stream.fileno())


def _gate_source(timeout_seconds: int) -> str:
    return f'''#!/usr/bin/env python3
"""Workspace-local C03 gate; release is controlled by the host apparatus."""
from pathlib import Path
import sys
import time

workspace = Path(__file__).resolve().parent
started = workspace / {GATE_STARTED_NAME!r}
release = workspace / {RELEASE_NAME!r}
started.write_text("started\\n", encoding="utf-8")
deadline = time.monotonic() + {timeout_seconds}
while time.monotonic() < deadline:
    if release.is_file():
        print("C03_GATE_RELEASED")
        raise SystemExit(0)
    time.sleep(0.05)
print("C03_GATE_TIMEOUT", file=sys.stderr)
raise SystemExit(124)
'''


def prepare_c03_case(
    *,
    case_dir,
    workspace,
    binary,
    home,
    prompt_file,
    model,
    timeout_seconds=MAX_C03_TIMEOUT_SECONDS,
    gate_timeout_seconds=DEFAULT_C03_GATE_TIMEOUT_SECONDS,
):
    """Create the predeclared workspace gate and immutable C03 operation plan."""
    case_dir = Path(case_dir).resolve()
    workspace = Path(workspace).resolve()
    binary = Path(binary).resolve()
    home = Path(home).resolve()
    prompt_file = Path(prompt_file).resolve()
    if not 1 <= timeout_seconds <= MAX_C03_TIMEOUT_SECONDS:
        raise ValueError(f"C03 timeout must be 1..{MAX_C03_TIMEOUT_SECONDS} seconds")
    if not 1 <= gate_timeout_seconds <= timeout_seconds:
        raise ValueError("C03 gate timeout must be within the fixed case timeout")
    if case_dir.exists():
        raise ValueError("C03 evidence directory already exists")
    if not workspace.is_dir() or not home.is_dir():
        raise ValueError("C03 requires an existing isolated workspace and CODEX_HOME")
    if not binary.is_file() or not os.access(binary, os.X_OK):
        raise ValueError("C03 native binary is missing or not executable")
    if not prompt_file.is_file():
        raise ValueError("C03 initial prompt file is missing")
    if workspace in case_dir.parents or home in case_dir.parents:
        raise ValueError("C03 evidence must be outside the workspace and CODEX_HOME")
    if case_dir in workspace.parents or case_dir in home.parents:
        raise ValueError(
            "C03 evidence directory cannot contain the workspace or CODEX_HOME"
        )
    for name in (GATE_NAME, RELEASE_NAME, GATE_STARTED_NAME):
        if (workspace / name).exists():
            raise ValueError(f"C03 workspace gate path already exists: {name}")

    config_path = home / "config.toml"
    if not config_path.is_file():
        raise ValueError("C03 CODEX_HOME is missing config.toml")
    config = tomllib.loads(config_path.read_text(encoding="utf-8"))
    provider = config.get("model_provider")
    providers = config.get("model_providers", {})
    provider_config = providers.get(provider) if isinstance(provider, str) else None
    if not isinstance(provider_config, dict) or not isinstance(
        provider_config.get("base_url"), str
    ):
        raise ValueError("C03 requires the configured model provider base URL")

    gate_path = workspace / GATE_NAME
    _write_new(gate_path, _gate_source(gate_timeout_seconds).encode())
    case_dir.mkdir(parents=True)
    prompt = prompt_file.read_text(encoding="utf-8")
    manifest = {
        "case": "C03",
        "transport": "native_app_server_stdio",
        "workspace": str(workspace),
        "home": str(home),
        "binary": str(binary),
        "binary_sha256": _sha256_file(binary),
        "config_sha256": _sha256_file(config_path),
        "provider": provider,
        "model": model,
        "prompt_file": str(prompt_file),
        "prompt_sha256": hashlib.sha256(prompt.encode()).hexdigest(),
        "gate": str(gate_path),
        "gate_sha256": hashlib.sha256(gate_path.read_bytes()).hexdigest(),
        "release_path": str(workspace / RELEASE_NAME),
        "gate_started_path": str(workspace / GATE_STARTED_NAME),
        "timeout_seconds": timeout_seconds,
        "gate_timeout_seconds": gate_timeout_seconds,
        "created_unix": time.time(),
        "fixed_operations": [
            "initialize native app-server stdio",
            "thread/start in the isolated workspace",
            "turn/start once with the fixed initial prompt",
            "identify one child from its host spawn_agent result",
            "thread/read until that child's gate CLI is InProgress in the workspace",
            "thread/compact/start once for the same parent thread",
            "wait for a new rencrow_compaction_commit rollout marker",
            "confirm the same child gate remains InProgress, then release its workspace sentinel",
            "observe wait_agent collecting that same child's terminal result and a later parent action",
            "wait for the initial parent turn to complete without adding model input",
        ],
        "external_steer": False,
        "rollout_or_database_edit": False,
        "trial_started": False,
    }
    _write_new(
        case_dir / "c03-manifest.json", json.dumps(manifest, indent=2).encode() + b"\n"
    )
    return manifest


def _ordinal(row, fallback):
    value = row.get("ordinal")
    return value if isinstance(value, int) else fallback


def _function_item(row):
    if row.get("type") != "response_item":
        return None
    payload = row.get("payload")
    if not isinstance(payload, dict) or payload.get("type") not in {
        "function_call",
        "function_call_output",
        "custom_tool_call",
        "custom_tool_call_output",
    }:
        return None
    return payload


def _tool_name(item):
    return item.get("name") or item.get("tool_name")


def _parse_json_field(value):
    if isinstance(value, dict):
        return value
    if isinstance(value, str):
        try:
            parsed = json.loads(value)
        except json.JSONDecodeError:
            return None
        return parsed if isinstance(parsed, dict) else None
    return None


def host_spawn_results(rows, after_ordinal=-1):
    """Return child IDs only from host output paired with an actual spawn_agent call."""
    calls = {}
    results = []
    for index, row in enumerate(rows):
        item = _function_item(row)
        if item is None:
            continue
        ordinal = _ordinal(row, index)
        if ordinal <= after_ordinal:
            continue
        call_id = item.get("call_id")
        if not isinstance(call_id, str) or not call_id:
            continue
        if item.get("type") in {"function_call", "custom_tool_call"}:
            name = _tool_name(item)
            if isinstance(name, str) and name.rsplit("::", 1)[-1] == "spawn_agent":
                calls[call_id] = name
            continue
        if call_id not in calls:
            continue
        metadata = row.get("metadata")
        if isinstance(metadata, dict) and metadata.get("client_authored") is True:
            continue
        output = _parse_json_field(item.get("output"))
        child_id = output.get("agent_id") if output else None
        if not isinstance(child_id, str):
            continue
        try:
            child_id = str(uuid.UUID(child_id))
        except (ValueError, AttributeError):
            continue
        results.append({"child_id": child_id, "call_id": call_id, "ordinal": ordinal})
    return results


def child_gate_is_running(thread_response, child_id, parent_id, workspace, gate_path):
    """Require thread/read host state for the exact child and exact workspace gate CLI."""
    if not isinstance(thread_response, dict):
        return False
    thread = thread_response.get("thread")
    if not isinstance(thread, dict) or thread.get("id") != child_id:
        return False
    if thread.get("parentThreadId") not in (None, parent_id):
        return False
    workspace = Path(workspace).resolve()
    gate_path = Path(gate_path).resolve()
    turns = thread.get("turns")
    if not isinstance(turns, list):
        return False
    for turn in turns:
        items = turn.get("items") if isinstance(turn, dict) else None
        if not isinstance(items, list):
            continue
        for item in items:
            if not isinstance(item, dict) or item.get("type") != "commandExecution":
                continue
            if item.get("status") != "inProgress":
                continue
            cwd = item.get("cwd")
            if not isinstance(cwd, str) or Path(cwd).resolve() != workspace:
                continue
            command = item.get("command")
            if not isinstance(command, str):
                continue
            try:
                tokens = shlex.split(command)
            except ValueError:
                continue
            for token in tokens:
                candidate = Path(token)
                if not candidate.is_absolute():
                    candidate = workspace / candidate
                if candidate.resolve() == gate_path:
                    return True
    return False


def committed_checkpoint(rows, after_ordinal=-1):
    """Find a newly appended durable RenCrow compaction commit marker."""
    compacted_ordinals = set()
    for index, row in enumerate(rows):
        ordinal = _ordinal(row, index)
        if ordinal <= after_ordinal:
            continue
        if row.get("type") == "compacted":
            compacted_ordinals.add(ordinal)
        if row.get("type") != "rencrow_compaction_commit":
            continue
        payload = row.get("payload")
        if not isinstance(payload, dict):
            continue
        checkpoint_hash = payload.get("checkpoint_hash")
        if (
            isinstance(checkpoint_hash, str)
            and checkpoint_hash
            and any(
                compacted_ordinal < ordinal for compacted_ordinal in compacted_ordinals
            )
        ):
            return {"ordinal": ordinal, "checkpoint_hash": checkpoint_hash}
    return None


def wait_result_for_child(rows, child_id, after_ordinal=-1):
    """Return the host wait_agent result for the exact child ID, if terminal."""
    calls = {}
    for index, row in enumerate(rows):
        item = _function_item(row)
        if item is None:
            continue
        ordinal = _ordinal(row, index)
        if ordinal <= after_ordinal:
            continue
        call_id = item.get("call_id")
        if not isinstance(call_id, str) or not call_id:
            continue
        if item.get("type") in {"function_call", "custom_tool_call"}:
            name = _tool_name(item)
            if not isinstance(name, str) or name.rsplit("::", 1)[-1] != "wait_agent":
                continue
            args = _parse_json_field(item.get("arguments"))
            targets = args.get("targets") if args else None
            if isinstance(targets, list) and child_id in targets:
                calls[call_id] = True
            continue
        if call_id not in calls:
            continue
        output = _parse_json_field(item.get("output"))
        status_map = output.get("status") if output else None
        status = status_map.get(child_id) if isinstance(status_map, dict) else None
        if isinstance(status, dict) and any(
            key in status for key in ("completed", "errored", "shutdown")
        ):
            return {"call_id": call_id, "ordinal": ordinal, "status": status}
    return None


def next_parent_action_after(rows, ordinal):
    for index, row in enumerate(rows):
        item = _function_item(row)
        if item is None or item.get("type") not in {
            "function_call",
            "custom_tool_call",
        }:
            continue
        if _ordinal(row, index) <= ordinal:
            continue
        name = _tool_name(item)
        if isinstance(name, str) and name.rsplit("::", 1)[-1] not in {
            "spawn_agent",
            "wait_agent",
        }:
            return {"ordinal": _ordinal(row, index), "tool": name}
    return None


class _AppServerClient:
    def __init__(self, proc):
        self.proc = proc
        self.pending = b""
        self.request_id = 0
        self.notifications = []

    def request(self, method, params, timeout):
        self.request_id += 1
        result, self.pending = _app_server_request(
            self.proc,
            self.request_id,
            method,
            params,
            time.monotonic() + timeout,
            self.pending,
            self.notifications,
        )
        return result

    def notify(self, message):
        _app_server_send(self.proc, message)

    def pump(self, timeout):
        deadline = time.monotonic() + max(0.001, timeout)
        ready, _, _ = select.select([self.proc.stdout], [], [], timeout)
        if not ready:
            if self.proc.poll() is not None:
                raise RuntimeError("native app-server exited before C03 completed")
            return
        message, self.pending = _app_server_read_message(
            self.proc, deadline, self.pending
        )
        self.notifications.append(message)


def _rows(path):
    return [row for _, row in parse_events(path)]


def _read_rollout_thread_id(rollout):
    first = next(iter(_rows(rollout)), None)
    if not isinstance(first, dict) or first.get("type") != "session_meta":
        return None
    payload = first.get("payload")
    if not isinstance(payload, dict):
        return None
    value = payload.get("session_id") or payload.get("id")
    return value if isinstance(value, str) else None


def _turn_completed(client, turn_id):
    return any(
        message.get("method") == "turn/completed"
        and isinstance(message.get("params"), dict)
        and message["params"].get("turn", {}).get("id") == turn_id
        for message in client.notifications
    )


def run_c03_case(*, case_dir, manifest_path=None):
    """Run the fixed C03 sequence; callers supply only the prepared manifest."""
    case_dir = Path(case_dir).resolve()
    manifest_path = Path(manifest_path or case_dir / "c03-manifest.json").resolve()
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    if (
        manifest.get("case") != "C03"
        or manifest.get("transport") != "native_app_server_stdio"
    ):
        raise ValueError("unsupported C03 manifest")
    timeout = manifest.get("timeout_seconds")
    if not isinstance(timeout, int) or not 1 <= timeout <= MAX_C03_TIMEOUT_SECONDS:
        raise ValueError("C03 manifest timeout is invalid")
    workspace = Path(manifest["workspace"]).resolve()
    home = Path(manifest["home"]).resolve()
    binary = Path(manifest["binary"]).resolve()
    prompt_file = Path(manifest["prompt_file"]).resolve()
    gate = Path(manifest["gate"]).resolve()
    release = Path(manifest["release_path"]).resolve()
    if _sha256_file(binary) != manifest.get("binary_sha256"):
        raise ValueError("C03 binary changed after manifest preparation")
    if _sha256_file(home / "config.toml") != manifest.get("config_sha256"):
        raise ValueError("C03 config changed after manifest preparation")
    if _sha256_file(gate) != manifest.get("gate_sha256"):
        raise ValueError("C03 gate changed after manifest preparation")
    if _sha256_file(prompt_file) != manifest.get("prompt_sha256"):
        raise ValueError("C03 initial prompt changed after manifest preparation")
    if release.exists() or Path(manifest["gate_started_path"]).exists():
        raise ValueError("C03 gate sentinels must be absent before the trial")
    config = tomllib.loads((home / "config.toml").read_text(encoding="utf-8"))
    provider = manifest["provider"]
    upstream = config["model_providers"][provider]["base_url"]
    output_dir = case_dir / "provider-inputs"
    output_dir.mkdir(parents=True, exist_ok=True)
    stderr_path = case_dir / "app-server-stderr.log"
    prompt = prompt_file.read_text(encoding="utf-8")
    command = [
        str(binary),
        "--no-daemon",
        "-c",
        'model_reasoning_effort="xhigh"',
        "app-server",
        "--stdio",
    ]
    env = os.environ.copy()
    env["CODEX_HOME"] = str(home)
    env["PATH"] = str(binary.parent) + os.pathsep + env.get("PATH", "")
    result = {
        "case": "C03",
        "started": False,
        "success": False,
        "external_steer": False,
        "release_cleanup_used": False,
        "errors": [],
    }
    proc = None
    stderr = None
    released = False
    deadline = time.monotonic() + timeout
    try:
        with ExitStack() as stack:
            proxy = stack.enter_context(capture_requests(output_dir, upstream))
            command[4:4] = [
                "-c",
                f"model_providers.{provider}.base_url={json.dumps(proxy)}",
            ]
            stderr = stack.enter_context(stderr_path.open("wb"))
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
            client = _AppServerClient(proc)
            client.request(
                "initialize",
                {
                    "clientInfo": {"name": "autonomous-completion-c03", "version": "1"},
                    "capabilities": {"experimentalApi": True},
                },
                min(60, max(1, deadline - time.monotonic())),
            )
            client.notify({"jsonrpc": "2.0", "method": "initialized"})
            thread = client.request(
                "thread/start",
                {
                    "cwd": str(workspace),
                    "model": manifest["model"],
                    "modelProvider": provider,
                },
                min(60, max(1, deadline - time.monotonic())),
            )
            thread_data = thread.get("thread", {})
            parent_id = thread_data.get("id")
            rollout = thread_data.get("path")
            if not isinstance(parent_id, str) or not isinstance(rollout, str):
                raise RuntimeError(
                    "thread/start did not return parent ID and rollout path"
                )
            rollout = Path(rollout).resolve()
            if not rollout.is_file() or _read_rollout_thread_id(rollout) != parent_id:
                raise RuntimeError(
                    "thread/start rollout does not belong to returned parent ID"
                )
            baseline_rows = _rows(rollout)
            baseline_ordinal = max(
                (_ordinal(row, index) for index, row in enumerate(baseline_rows)),
                default=-1,
            )
            turn = client.request(
                "turn/start",
                {
                    "threadId": parent_id,
                    "input": [{"type": "text", "text": prompt}],
                },
                min(60, max(1, deadline - time.monotonic())),
            )
            turn_id = turn.get("turn", {}).get("id")
            if not isinstance(turn_id, str):
                turn_id = next(
                    (
                        message.get("params", {}).get("turn", {}).get("id")
                        for message in client.notifications
                        if message.get("method") == "turn/started"
                        and isinstance(message.get("params"), dict)
                    ),
                    None,
                )
            if not isinstance(turn_id, str):
                raise RuntimeError("turn/start did not expose the live parent turn ID")
            result.update(
                {
                    "started": True,
                    "parent_thread_id": parent_id,
                    "parent_turn_id": turn_id,
                    "rollout": str(rollout),
                    "baseline_ordinal": baseline_ordinal,
                }
            )

            child = None
            while time.monotonic() < deadline:
                rows = _rows(rollout)
                spawned = host_spawn_results(rows, baseline_ordinal)
                if len(spawned) > 1:
                    raise RuntimeError("C03 observed more than one host-spawned child")
                if spawned:
                    child = spawned[0]
                    break
                client.pump(min(0.2, deadline - time.monotonic()))
            if child is None:
                raise TimeoutError("C03 timed out waiting for host spawn_agent result")
            result["child"] = child

            gate_running = False
            while time.monotonic() < deadline:
                child_view = client.request(
                    "thread/read",
                    {"threadId": child["child_id"], "includeTurns": True},
                    min(10, max(1, deadline - time.monotonic())),
                )
                if child_gate_is_running(
                    child_view, child["child_id"], parent_id, workspace, gate
                ):
                    gate_running = True
                    break
                client.pump(min(0.1, deadline - time.monotonic()))
            if not gate_running:
                raise TimeoutError(
                    "C03 timed out waiting for the child gate CLI to run"
                )
            rows = _rows(rollout)
            compact_baseline = max(
                (_ordinal(row, index) for index, row in enumerate(rows)), default=-1
            )
            client.request(
                "thread/compact/start",
                {"threadId": parent_id},
                min(30, max(1, deadline - time.monotonic())),
            )
            checkpoint = None
            while time.monotonic() < deadline:
                rows = _rows(rollout)
                checkpoint = committed_checkpoint(rows, compact_baseline)
                if checkpoint:
                    break
                client.pump(min(0.2, deadline - time.monotonic()))
            if checkpoint is None:
                raise TimeoutError(
                    "C03 timed out waiting for durable compaction commit"
                )
            child_view = client.request(
                "thread/read",
                {"threadId": child["child_id"], "includeTurns": True},
                min(10, max(1, deadline - time.monotonic())),
            )
            if not child_gate_is_running(
                child_view, child["child_id"], parent_id, workspace, gate
            ):
                raise RuntimeError("C03 child gate was not still InProgress at commit")
            result["checkpoint"] = checkpoint
            _write_new(release, b"released after committed checkpoint\n")
            released = True

            wait_result = None
            wait_baseline = checkpoint["ordinal"]
            while time.monotonic() < deadline:
                rows = _rows(rollout)
                wait_result = wait_result_for_child(
                    rows, child["child_id"], wait_baseline
                )
                if wait_result:
                    break
                client.pump(min(0.2, deadline - time.monotonic()))
            if wait_result is None:
                raise TimeoutError(
                    "C03 parent did not collect the same child's terminal result"
                )
            result["wait_result"] = wait_result

            next_action = None
            while time.monotonic() < deadline:
                rows = _rows(rollout)
                next_action = next_parent_action_after(rows, wait_result["ordinal"])
                if next_action:
                    break
                client.pump(min(0.2, deadline - time.monotonic()))
            if next_action is None:
                raise TimeoutError(
                    "C03 parent did not perform a next action after child result"
                )
            result["next_action"] = next_action

            while time.monotonic() < deadline and not _turn_completed(client, turn_id):
                client.pump(min(0.2, deadline - time.monotonic()))
            if not _turn_completed(client, turn_id):
                raise TimeoutError(
                    "C03 parent turn did not complete within the fixed timeout"
                )
            result["success"] = True
            result["completed_turn"] = True
    except Exception as error:  # evidence records the precise failure; cleanup still releases the child.
        result["errors"].append(f"{type(error).__name__}: {error}")
    finally:
        if not released:
            try:
                if not release.exists():
                    _write_new(release, b"released during C03 cleanup\n")
                    result["release_cleanup_used"] = True
            except OSError as error:
                result["errors"].append(f"gate cleanup failed: {error}")
        if proc is not None:
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
    result["finished_unix"] = time.time()
    result["provider_input_count"] = len(list(output_dir.glob("*.json")))
    (case_dir / "c03-result.json").write_text(
        json.dumps(result, indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
    )
    return result
