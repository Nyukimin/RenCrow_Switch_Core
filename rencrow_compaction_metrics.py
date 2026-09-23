#!/usr/bin/env python3
"""RenCrow: read-only rollout metrics; no model calls or transcript output."""

import argparse
import json
import math
import os
from pathlib import Path
import tempfile
import time


def _number(value):
    return (
        isinstance(value, (int, float))
        and not isinstance(value, bool)
        and math.isfinite(value)
    )


def _usage_value(record, field):
    usage = record.get("usage")
    if not isinstance(usage, dict):
        return None
    value = usage.get(field)
    return value if _number(value) else None


def _sum_usage(records, field, *, absent_is_zero=False):
    if not records:
        return 0
    values = []
    for record in records:
        usage = record.get("usage")
        if not isinstance(usage, dict):
            return None
        if field not in usage and absent_is_zero:
            values.append(0)
            continue
        value = usage.get(field)
        if not _number(value):
            return None
        values.append(value)
    return sum(values)


def _turn_output_tokens(records, turn_id):
    matching = [record for record in records if record.get("turn_id") == turn_id]
    if not matching:
        return None
    values = [_usage_value(record, "output_tokens") for record in matching]
    if any(value is None for value in values):
        return None
    return sum(values)


def _stage_rate(stage):
    seconds = stage.get("seconds")
    output_tokens = _usage_value(stage, "output_tokens")
    if not _number(seconds) or seconds <= 0 or output_tokens is None:
        return None
    return round(output_tokens / seconds, 2)


def _stage_total_tokens(stages):
    if not stages:
        return 0
    values = [_usage_value(stage, "total_tokens") for stage in stages]
    if any(value is None for value in values):
        return None
    return sum(values)


def _stage_seconds(stages):
    values = [stage.get("seconds") for stage in stages]
    if any(not _number(value) or value < 0 for value in values):
        return None
    return round(sum(values), 3)


class Metrics:
    def __init__(self):
        self.index = 0
        self.usage = {}
        self.checkpoints = []
        self.pending = None
        self.tool_results = []
        self.errors = 0
        self.last_turn_rate = None
        self.thread_id = None
        self.active_turn = None

    def consume(self, row):
        self.index += 1
        kind, p = row["type"], row.get("payload", {})
        if kind == "session_meta":
            self.thread_id = p.get("id")
        elif kind == "token_usage_record":
            key = p.get("response_id")
            if not key:
                raise ValueError(
                    "usage record without response_id cannot be deduplicated"
                )
            if key in self.usage:
                if self.usage[key]["usage"] != p.get("usage"):
                    raise ValueError("conflicting usage for the same response_id")
            else:
                self.usage[key] = {
                    "index": self.index,
                    "turn_id": p.get("turn_id"),
                    "usage": p.get("usage"),
                }
        elif kind == "compacted":
            self.pending = None
            for metadata in p.get("replacement_history_metadata") or []:
                if metadata and metadata.get("rencrow_compaction"):
                    value = metadata["rencrow_compaction"]
                    if value.get("version") == 2:
                        applied_refs = value.get("applied_refs")
                        results = value.get("results")
                        self.pending = {
                            "timestamp": row["timestamp"],
                            "selection_mode": value.get("selection_mode"),
                            "stages": value.get("responses") or [],
                            "model": value.get("model"),
                            "effort": value.get("effort"),
                            "applied_ref_count": len(applied_refs)
                            if isinstance(applied_refs, list)
                            else None,
                            "result_count": len(results)
                            if isinstance(results, list)
                            else None,
                        }
                    else:
                        bundle = value.get("bundle")
                        if not isinstance(bundle, dict):
                            continue
                        self.pending = {
                            "timestamp": row["timestamp"],
                            "selection_mode": value.get("selection_mode"),
                            "stages": bundle.get("responses") or [],
                            "model": bundle.get("model"),
                            "effort": bundle.get("effort"),
                            "approved_operations": len(
                                (bundle.get("plan_review") or {}).get(
                                    "accepted_operations"
                                )
                                or []
                            ),
                        }
        elif kind == "rencrow_compaction_commit" and self.pending:
            self.pending["index"] = self.index
            self.pending["commit_marker_observed"] = True
            self.checkpoints.append(self.pending)
            self.pending = None
        elif kind == "response_item" and p.get("type") in (
            "function_call_output",
            "custom_tool_call_output",
        ):
            self.tool_results.append(self.index)
        elif kind == "event_msg" and p.get("type") == "task_started":
            self.active_turn = (p["turn_id"], p["started_at"])
        elif kind == "event_msg" and p.get("type") == "task_complete":
            self.active_turn = None
            self.errors += bool(p.get("error"))
            elapsed = p.get("duration_ms", 0)
            if elapsed:
                output = _turn_output_tokens(self.usage.values(), p.get("turn_id"))
                self.last_turn_rate = (
                    round(output / (elapsed / 1000), 2)
                    if output is not None and _number(elapsed) and elapsed > 0
                    else None
                )

    def report(self):
        active_rate = None
        if self.active_turn:
            turn_id, started = self.active_turn
            elapsed = time.time() - started
            if elapsed > 0:
                output = _turn_output_tokens(self.usage.values(), turn_id)
                if output is not None:
                    active_rate = round(output / elapsed, 2)
        stage_ids = {
            stage.get("response_id")
            for checkpoint in self.checkpoints
            for stage in checkpoint["stages"]
            if isinstance(stage, dict) and stage.get("response_id")
        }
        if self.pending:
            stage_ids.update(
                stage.get("response_id")
                for stage in self.pending["stages"]
                if isinstance(stage, dict) and stage.get("response_id")
            )
        others = sorted(
            (v for k, v in self.usage.items() if k not in stage_ids),
            key=lambda v: v["index"],
        )
        checkpoints = []
        for i, c in enumerate(self.checkpoints):
            stages = [
                {**stage, "output_tok_per_wall_second": _stage_rate(stage)}
                for stage in c["stages"]
                if isinstance(stage, dict)
            ]
            first_stage = min(
                (
                    self.usage[stage["response_id"]]["index"]
                    for stage in stages
                    if stage.get("response_id") in self.usage
                ),
                default=c["index"],
            )
            before = [v for v in others if v["index"] < first_stage]
            boundary = (
                self.checkpoints[i + 1]["index"]
                if i + 1 < len(self.checkpoints)
                else self.index + 1
            )
            after = [v for v in others if c["index"] < v["index"] < boundary]
            pre = _usage_value(before[-1], "input_tokens") if before else None
            post = _usage_value(after[0], "input_tokens") if after else None
            checkpoints.append(
                {
                    **c,
                    "stages": stages,
                    "seconds": _stage_seconds(stages),
                    "stage_total_tokens": _stage_total_tokens(stages),
                    "before_observed_input_tokens": pre,
                    "after_observed_input_tokens": post,
                    "observed_input_delta": post - pre
                    if pre is not None and post is not None
                    else None,
                    "post_checkpoint_tool_results": sum(
                        c["index"] < n < boundary for n in self.tool_results
                    ),
                    "semantic_continuity": "requires_independent_review",
                }
            )
        return {
            "thread_id": self.thread_id,
            "observed_rows": self.index,
            "unique_usage_records": len(self.usage),
            "total_recorded_tokens": _sum_usage(self.usage.values(), "total_tokens"),
            "recorded_input_tokens": _sum_usage(self.usage.values(), "input_tokens"),
            "recorded_cached_input_tokens": _sum_usage(
                self.usage.values(), "cached_input_tokens", absent_is_zero=True
            ),
            "recorded_output_tokens": _sum_usage(self.usage.values(), "output_tokens"),
            "last_observed_input_tokens": _usage_value(others[-1], "input_tokens")
            if others
            else None,
            "last_completed_turn_output_tok_per_wall_second": self.last_turn_rate,
            "active_turn_recorded_output_tok_per_wall_second": active_rate,
            "task_errors": self.errors,
            "prepared_checkpoint_without_marker": self.pending is not None,
            "compactions": checkpoints,
            "comparison": "Adjacent requests differ. Delta is descriptive, not controlled savings. Stage usage is already included in total_recorded_tokens; do not add it again. Unmatched failed compaction usage may be among other observed requests. Commit observation is not independent hash validation.",
        }


def read_available(stream, metrics):
    while True:
        offset = stream.tell()
        line = stream.readline()
        if not line:
            return
        if not line.endswith(b"\n"):
            stream.seek(offset)
            return
        metrics.consume(json.loads(line))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("rollout", type=Path)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--watch", action="store_true")
    args = parser.parse_args()
    if args.output and args.output.resolve() == args.rollout.resolve():
        parser.error("output cannot overwrite rollout")
    metrics = Metrics()
    previous = None
    with args.rollout.open("rb") as stream:
        while True:
            read_available(stream, metrics)
            report = metrics.report()
            if report != previous:
                if args.output:
                    # Derived statistics only; no conversation text is written.
                    temp = None
                    try:
                        with tempfile.NamedTemporaryFile(
                            mode="w",
                            encoding="utf-8",
                            dir=args.output.parent,
                            delete=False,
                        ) as output:
                            temp = Path(output.name)
                            json.dump(report, output, ensure_ascii=False, indent=2)
                        os.replace(temp, args.output)
                    finally:
                        if temp is not None:
                            temp.unlink(missing_ok=True)
                if args.watch:
                    print("\033[2J\033[H", end="")
                    print("Qwen / Compaction metrics (read-only)")
                    print("thread:", report["thread_id"])
                    print(
                        "requests:",
                        report["unique_usage_records"],
                        "total tokens:",
                        report["total_recorded_tokens"],
                    )
                    print(
                        "input / cached input / output:",
                        report["recorded_input_tokens"],
                        "/",
                        report["recorded_cached_input_tokens"],
                        "/",
                        report["recorded_output_tokens"],
                    )
                    print(
                        "last input:",
                        report["last_observed_input_tokens"],
                        "turn output tok/sec (wall incl tools):",
                        report["last_completed_turn_output_tok_per_wall_second"],
                    )
                    print(
                        "active turn recorded output tok/sec (wall incl tools/wait):",
                        report["active_turn_recorded_output_tok_per_wall_second"],
                    )
                    print(
                        "committed compactions:",
                        len(report["compactions"]),
                        "task errors:",
                        report["task_errors"],
                    )
                    for c in report["compactions"][-2:]:
                        print(
                            c["timestamp"],
                            "pre/post:",
                            c["before_observed_input_tokens"],
                            "/",
                            c["after_observed_input_tokens"],
                        )
                        print(
                            "compaction tokens/seconds:",
                            c["stage_total_tokens"],
                            "/",
                            c["seconds"],
                        )
                        print(
                            "stages tok/sec:",
                            [
                                s["output_tok_per_wall_second"]
                                for s in c["stages"]
                            ],
                        )
                    print(
                        "Adjacent requests are not a controlled comparison. Quality: independent review.",
                        flush=True,
                    )
                else:
                    print(json.dumps(report, ensure_ascii=False))
                previous = report
            if not args.watch:
                break
            time.sleep(10)


if __name__ == "__main__":
    main()
