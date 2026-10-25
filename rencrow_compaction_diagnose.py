#!/usr/bin/env python3
"""Read-only, bounded RenCrow Compaction V2 checkpoint and warning report."""

import argparse
from contextlib import closing
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
from pathlib import PurePosixPath, PureWindowsPath
import re
import sqlite3
import sys


MAX_EVENTS = 200
MAX_TRACE_EVENTS = 100_000
MAX_TRACE_BYTES = 64 * 1024 * 1024
MAX_RESPONSE_BYTES = 16 * 1024 * 1024
MAX_RESPONSE_TOTAL_BYTES = 64 * 1024 * 1024
MAX_USAGE_BRIDGE_EVENT_MSGS = 1
ROLLOUT_TIME = re.compile(r"\A\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z\Z")
SAFE_ID = re.compile(r"\A[A-Za-z0-9_:\-]{1,128}\Z")
AUTO_COMPACT = re.compile(r"run_auto_compact\{reason=([A-Za-z]+) phase=([A-Za-z]+)\}")


def _safe_id(value):
    return value if isinstance(value, str) and SAFE_ID.fullmatch(value) else None


def _timestamp(value):
    return value if isinstance(value, str) and ROLLOUT_TIME.fullmatch(value) else None


def _preceding_usage_response(row, thread_id):
    if row is None:
        return None, "no_preceding_token_usage_record"
    if _safe_id(row.get("thread_id")) != thread_id:
        return None, "preceding_token_usage_thread_mismatch"
    response_id = _safe_id(row.get("response_id"))
    return (response_id, "candidate_available") if response_id else (
        None, "invalid_preceding_response_id"
    )


def _v2_metadata(payload):
    metadata = payload.get("replacement_history_metadata") or []
    if not isinstance(metadata, list):
        return None
    for item in metadata:
        if not isinstance(item, dict):
            continue
        value = item.get("rencrow_compaction")
        if isinstance(value, dict) and value.get("version") == 2:
            return value
    return None


def rollout_events(path):
    checkpoints = []
    thread_id = None
    pending = None
    usage = None
    bridge_events = 0
    with path.open(encoding="utf-8") as stream:
        for number, raw in enumerate(stream, 1):
            try:
                row = json.loads(raw)
            except json.JSONDecodeError as exc:
                raise ValueError(f"{path}:{number}: invalid JSON: {exc.msg}") from exc
            if not isinstance(row, dict):
                raise ValueError(f"{path}:{number}: expected object")
            kind = row.get("type")
            payload = row.get("payload") or {}
            if not isinstance(payload, dict):
                raise ValueError(f"{path}:{number}: invalid payload")
            previous_usage = usage if kind == "compacted" else None
            if kind == "token_usage_record":
                usage, bridge_events = payload, 0
            elif kind == "event_msg" and usage is not None:
                bridge_events += 1
                if bridge_events > MAX_USAGE_BRIDGE_EVENT_MSGS:
                    usage = None
            elif kind == "compacted":
                usage, bridge_events = None, 0
            else:
                usage, bridge_events = None, 0
            if kind == "session_meta":
                thread_id = _safe_id(payload.get("id"))
            elif kind == "compacted":
                pending = None
                metadata = _v2_metadata(payload)
                if metadata is None:
                    continue
                mode = metadata.get("selection_mode")
                if mode not in (
                    "model_selection",
                    "no_candidates",
                    "deterministic_emergency",
                ):
                    raise ValueError(f"{path}:{number}: unknown V2 selection mode")
                responses = metadata.get("responses") or []
                if not isinstance(responses, list):
                    raise ValueError(f"{path}:{number}: invalid V2 responses")
                checkpoint = {
                    "line": number,
                    "timestamp": _timestamp(row.get("timestamp")),
                    "selection_mode": mode,
                    "retained_items": len(payload.get("replacement_history") or []),
                    "semantic_summary": bool(metadata.get("semantic_summary_hash")),
                    "model_requests": [
                        {
                            "stage": _safe_id(item.get("stage")),
                            "response_id": _safe_id(item.get("response_id")),
                        }
                        for item in responses
                        if isinstance(item, dict)
                    ],
                    "commit_line": None,
                }
                if mode == "deterministic_emergency":
                    response_id, status = _preceding_usage_response(
                        previous_usage, thread_id
                    )
                    checkpoint["_response_id"] = response_id
                    checkpoint["_response_status"] = status
                checkpoints.append(checkpoint)
                pending = checkpoint
            elif kind == "rencrow_compaction_commit" and pending is not None:
                pending["commit_line"] = number
                pending = None
            if len(checkpoints) > MAX_EVENTS:
                raise ValueError(f"{path}: more than {MAX_EVENTS} V2 checkpoints")
    if thread_id is None:
        raise ValueError(f"{path}: no valid session thread ID")
    return thread_id, checkpoints


def _reason(body):
    if isinstance(body, bytes):
        body = body.decode("utf-8", errors="replace")
    if "compaction summary response contains a non-assistant item" in body:
        return "summary_non_assistant_item"
    if "schema rejected" in body:
        return "schema_rejected"
    if "model unavailable" in body or "connection" in body or "disconnected" in body:
        return "model_or_transport_failure"
    if "RenCrow Normal compaction failed; using Emergency" in body:
        return "other_normal_failure"
    return "other_v2_warning"


def v2_log_events(path, thread_id, checkpoints, capture_bodies=False):
    uri = f"{path.resolve().as_uri()}?mode=ro"
    with closing(sqlite3.connect(uri, uri=True)) as db:
        names = {
            row[0]
            for row in db.execute("SELECT name FROM sqlite_master WHERE type='table'")
        }
        if "logs" not in names:
            raise ValueError(f"{path}: logs table missing")
        rows = db.execute(
            "SELECT id, ts, level, feedback_log_body, file, line FROM logs "
            "WHERE thread_id=? AND target='codex_core::compact::rencrow' "
            "AND level IN ('WARN','ERROR') ORDER BY ts,ts_nanos LIMIT ?",
            (thread_id, MAX_EVENTS + 1),
        ).fetchall()
        if len(rows) > MAX_EVENTS:
            raise ValueError(f"{path}: more than {MAX_EVENTS} V2 warnings")
        warnings = []
        for row in rows:
            body = row[3] or ""
            body_text = (
                body.decode("utf-8", errors="replace")
                if isinstance(body, bytes)
                else body
            )
            auto = AUTO_COMPACT.search(body_text)
            warning = {
                "log_id": row[0],
                "timestamp": datetime.fromtimestamp(
                    row[1], timezone.utc
                ).isoformat(),
                "level": row[2],
                "reason": _reason(body_text),
                "trigger": _safe_id(auto.group(1)) if auto else None,
                "phase": _safe_id(auto.group(2)) if auto else None,
                "source_file": row[4],
                "source_line": row[5],
            }
            if capture_bodies:
                warning["_body"] = (
                    body if isinstance(body, bytes) else body.encode("utf-8")
                )
            warnings.append(warning)
        for checkpoint in checkpoints:
            stamp = checkpoint["timestamp"]
            if stamp is None:
                checkpoint["nearby_log_rows"] = None
                continue
            seconds = int(
                datetime.fromisoformat(stamp.replace("Z", "+00:00")).timestamp()
            )
            checkpoint["nearby_log_rows"] = db.execute(
                "SELECT count(*) FROM logs WHERE thread_id=? AND ts BETWEEN ? AND ?",
                (thread_id, seconds - 120, seconds + 30),
            ).fetchone()[0]
    return warnings


def _safe_bundle_file(root, relative):
    if (
        not isinstance(relative, str)
        or not relative
        or len(relative) > 512
        or "\\" in relative
        or "\x00" in relative
    ):
        return None, "unsafe_payload_path"
    parts = relative.split("/")
    if (
        PurePosixPath(relative).is_absolute()
        or PureWindowsPath(relative).is_absolute()
        or PureWindowsPath(relative).drive
        or any(p in ("", ".", "..") for p in parts)
    ):
        return None, "unsafe_payload_path"
    candidate = root.joinpath(*parts)
    if any(
        root.joinpath(*parts[:i]).is_symlink() for i in range(1, len(parts) + 1)
    ):
        return None, "payload_path_symlink"
    try:
        resolved = candidate.resolve(strict=True)
        resolved.relative_to(root)
    except FileNotFoundError:
        return None, "payload_missing"
    except (OSError, RuntimeError, ValueError):
        return None, "payload_path_escape"
    return (resolved, None) if resolved.is_file() else (None, "payload_not_file")


def _trace_index(bundle, thread_id, response_ids):
    try:
        root = bundle.resolve(strict=True)
    except FileNotFoundError:
        return "trace_bundle_missing", {}, {}
    except (OSError, RuntimeError):
        return "trace_bundle_unavailable", {}, {}
    if not root.is_dir():
        return "trace_bundle_not_directory", {}, {}
    trace, error = _safe_bundle_file(root, "trace.jsonl")
    if error:
        status = "trace_missing" if error == "payload_missing" else "trace_" + error
        return status, {}, {}
    found, path_uses = {}, {}
    try:
        with trace.open("rb") as stream:
            raw_events = stream.read(MAX_TRACE_BYTES + 1)
        if len(raw_events) > MAX_TRACE_BYTES:
            return "trace_too_large", {}, {}
        lines = raw_events.splitlines()
        if len(lines) > MAX_TRACE_EVENTS:
            return "trace_event_limit_exceeded", {}, {}
        for raw in lines:
            event = json.loads(raw.decode("utf-8"))
            if not isinstance(event, dict) or not isinstance(
                event.get("payload"), dict
            ):
                return "trace_corrupt", {}, {}
            payload = event["payload"]
            if payload.get("type") != "inference_completed":
                continue
            ref = payload.get("response_payload")
            path = ref.get("path") if isinstance(ref, dict) else None
            if isinstance(path, str) and len(path) <= 512:
                path_uses[path] = path_uses.get(path, 0) + 1
            key = (
                _safe_id(event.get("thread_id")),
                _safe_id(payload.get("response_id")),
            )
            if key[0] == thread_id and key[1] in response_ids:
                found.setdefault(key, []).append(path)
    except (OSError, UnicodeDecodeError, json.JSONDecodeError):
        return "trace_corrupt", {}, {}
    return None, found, path_uses


def _response_result(root, key, found, path_uses, budget):
    matches = found.get(key, [])
    if not matches:
        return {"status": "response_not_found"}, budget
    if len(matches) != 1:
        return {"status": "duplicate_response_match"}, budget
    relative = matches[0]
    if not isinstance(relative, str):
        return {"status": "response_payload_reference_missing"}, budget
    if path_uses.get(relative, 0) > 1:
        return {"status": "payload_path_reused"}, budget
    path, error = _safe_bundle_file(root, relative)
    if error:
        return {"status": error}, budget
    try:
        size = path.stat().st_size
        if size > MAX_RESPONSE_BYTES:
            return {"status": "response_payload_too_large"}, budget
        if budget + size > MAX_RESPONSE_TOTAL_BYTES:
            return {"status": "response_payload_budget_exceeded"}, budget
        with path.open("rb") as stream:
            data = stream.read(MAX_RESPONSE_BYTES + 1)
    except OSError:
        return {"status": "response_payload_read_error"}, budget
    if len(data) > MAX_RESPONSE_BYTES:
        return {"status": "response_payload_too_large"}, budget
    budget += len(data)
    try:
        response = json.loads(data.decode("utf-8"))
        if not isinstance(response, dict):
            return {"status": "response_payload_corrupt"}, budget
    except (UnicodeDecodeError, json.JSONDecodeError):
        return {"status": "response_payload_corrupt"}, budget
    if response.get("response_id") != key[1]:
        return {"status": "response_payload_response_id_mismatch"}, budget
    return {
        "status": "matched",
        "path": relative,
        "size_bytes": len(data),
        "sha256": hashlib.sha256(data).hexdigest(),
    }, budget


def _write_warning_output(path, warnings):
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    descriptor = os.open(path, flags, 0o600)
    try:
        if os.name == "posix":
            os.fchmod(descriptor, 0o600)
        with os.fdopen(descriptor, "wb") as stream:
            for warning in warnings:
                body = warning["_body"]
                stream.write(len(body).to_bytes(8, "big"))
                stream.write(body)
    except Exception:
        try:
            os.close(descriptor)
        except OSError:
            pass
        try:
            Path(path).unlink()
        except OSError:
            pass
        raise


def diagnose(rollout, logs_db=None, trace_bundle=None, warning_output=None):
    thread_id, checkpoints = rollout_events(rollout)
    explicit_logs_db = logs_db is not None
    if logs_db is None:
        sessions = next(
            (p for p in rollout.resolve().parents if p.name == "sessions"), None
        )
        logs_db = sessions.parent / "logs_2.sqlite" if sessions else None
    if explicit_logs_db and not logs_db.is_file():
        raise ValueError(f"{logs_db}: logs database missing")
    if logs_db is not None and logs_db.is_file():
        warnings = v2_log_events(
            logs_db, thread_id, checkpoints, warning_output is not None
        )
        log_status = "read"
    else:
        warnings = []
        log_status = "unavailable"
    if warning_output is not None:
        if log_status != "read":
            raise ValueError("warning output requires an available logs database")
        _write_warning_output(warning_output, warnings)
    report = {
        "schema_version": 2,
        "thread_id": thread_id,
        "v2_checkpoint_count": len(checkpoints),
        "normal_count": sum(
            c["commit_line"] is not None
            and c["selection_mode"] != "deterministic_emergency"
            for c in checkpoints
        ),
        "emergency_count": sum(
            c["commit_line"] is not None
            and c["selection_mode"] == "deterministic_emergency"
            for c in checkpoints
        ),
        "uncommitted_checkpoint_count": sum(
            c["commit_line"] is None for c in checkpoints
        ),
        "checkpoints": [
            {key: value for key, value in c.items() if not key.startswith("_")}
            for c in checkpoints
        ],
        "v2_warnings": [
            {key: value for key, value in w.items() if not key.startswith("_")}
            for w in warnings
        ],
        "log_status": log_status,
        "limits": [
            "Only RenCrow V2 metadata and same-thread V2 warnings are reported",
            "No conversation or warning body is emitted",
            "Missing warnings or log coverage do not prove Normal succeeded",
            "Model requests list only accepted checkpoint receipts, not failed Normal attempts",
        ],
    }
    if trace_bundle is not None:
        emergency = [
            c for c in checkpoints if c["selection_mode"] == "deterministic_emergency"
        ]
        targets = {c.get("_response_id") for c in emergency if c.get("_response_id")}
        trace_status, found, path_uses = (
            _trace_index(trace_bundle, thread_id, targets)
            if targets else (None, {}, {})
        )
        try:
            root = (
                trace_bundle.resolve(strict=True)
                if trace_status is None and targets
                else None
            )
        except (OSError, RuntimeError):
            trace_status, root = "trace_bundle_unavailable", None
        budget = 0
        matches = []
        for checkpoint in emergency:
            response_id = checkpoint.get("_response_id")
            if response_id is None:
                result = {"status": checkpoint.get("_response_status")}
            elif trace_status is not None:
                result = {"status": trace_status}
            else:
                result, budget = _response_result(
                    root, (thread_id, response_id), found, path_uses, budget
                )
            matches.append(
                {
                    "checkpoint_line": checkpoint["line"],
                    "candidate_response_id": response_id,
                    **result,
                }
            )
        report["emergency_response_matches"] = matches
        report["response_match_basis"] = (
            "exact same-thread response_id; causal link not established"
        )
    return report


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--rollout", type=Path, required=True, help="one Codex JSONL rollout"
    )
    parser.add_argument(
        "--logs-db", type=Path, help="optional CODEX_HOME/logs_2.sqlite"
    )
    parser.add_argument(
        "--trace-bundle",
        type=Path,
        help="optional trace bundle for exact completed-response ID matching",
    )
    parser.add_argument(
        "--warning-output",
        type=Path,
        help="new private file for verbatim same-thread V2 warning bodies",
    )
    args = parser.parse_args(argv)
    try:
        report = diagnose(
            args.rollout, args.logs_db, args.trace_bundle, args.warning_output
        )
    except (OSError, ValueError, sqlite3.DatabaseError) as exc:
        print(
            json.dumps({"status": "error", "reason": str(exc)}, ensure_ascii=False),
            file=sys.stderr,
        )
        return 2
    print(json.dumps(report, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    sys.exit(main())
