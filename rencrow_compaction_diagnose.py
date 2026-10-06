#!/usr/bin/env python3
"""Read-only, bounded RenCrow Compaction V2 checkpoint and warning report."""

import argparse
from contextlib import closing
from datetime import datetime, timezone
import json
from pathlib import Path
import re
import sqlite3
import sys


MAX_EVENTS = 200
ROLLOUT_TIME = re.compile(r"\A\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z\Z")
SAFE_ID = re.compile(r"\A[A-Za-z0-9_:\-]{1,128}\Z")
AUTO_COMPACT = re.compile(r"run_auto_compact\{reason=([A-Za-z]+) phase=([A-Za-z]+)\}")


def _safe_id(value):
    return value if isinstance(value, str) and SAFE_ID.fullmatch(value) else None


def _timestamp(value):
    return value if isinstance(value, str) and ROLLOUT_TIME.fullmatch(value) else None


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
    if "compaction summary response contains a non-assistant item" in body:
        return "summary_non_assistant_item"
    if "schema rejected" in body:
        return "schema_rejected"
    if "model unavailable" in body or "connection" in body or "disconnected" in body:
        return "model_or_transport_failure"
    if "RenCrow Normal compaction failed; using Emergency" in body:
        return "other_normal_failure"
    return "other_v2_warning"


def v2_log_events(path, thread_id, checkpoints):
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
            auto = AUTO_COMPACT.search(body)
            warnings.append(
                {
                    "log_id": row[0],
                    "timestamp": datetime.fromtimestamp(
                        row[1], timezone.utc
                    ).isoformat(),
                    "level": row[2],
                    "reason": _reason(body),
                    "trigger": _safe_id(auto.group(1)) if auto else None,
                    "phase": _safe_id(auto.group(2)) if auto else None,
                    "source_file": row[4],
                    "source_line": row[5],
                }
            )
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


def diagnose(rollout, logs_db=None):
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
        warnings = v2_log_events(logs_db, thread_id, checkpoints)
        log_status = "read"
    else:
        warnings = []
        log_status = "unavailable"
    return {
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
        "checkpoints": checkpoints,
        "v2_warnings": warnings,
        "log_status": log_status,
        "limits": [
            "Only RenCrow V2 metadata and same-thread V2 warnings are reported",
            "No conversation or warning body is emitted",
            "Missing warnings or log coverage do not prove Normal succeeded",
            "Model requests list only accepted checkpoint receipts, not failed Normal attempts",
        ],
    }


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--rollout", type=Path, required=True, help="one Codex JSONL rollout"
    )
    parser.add_argument(
        "--logs-db", type=Path, help="optional CODEX_HOME/logs_2.sqlite"
    )
    args = parser.parse_args(argv)
    try:
        report = diagnose(args.rollout, args.logs_db)
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
