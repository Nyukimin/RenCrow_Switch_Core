"""Contract tests for read-only compaction measurements."""

import io
import json
from contextlib import redirect_stdout
import sys
import tempfile
import unittest
from unittest.mock import patch
from rencrow_compaction_metrics import Metrics, read_available
from rencrow_compaction_metrics import main as metrics_main


def usage(key, tokens):
    return {
        "type": "token_usage_record",
        "payload": {
            "response_id": key,
            "turn_id": "t",
            "usage": {
                "input_tokens": tokens,
                "output_tokens": 0,
                "total_tokens": tokens,
            },
        },
    }


def prepared():
    stage = {
        "stage": "summary",
        "response_id": "s",
        "seconds": 2,
        "output_tok_per_wall_second": 5,
        "usage": {"total_tokens": 200},
    }
    return {
        "type": "compacted",
        "timestamp": "2026-09-22",
        "payload": {
            "replacement_history_metadata": [
                {
                    "rencrow_compaction": {
                        "selection_mode": "deterministic_no_human_input",
                        "bundle": {
                            "model": "worker",
                            "effort": "high",
                            "responses": [stage],
                            "plan_review": {"accepted_operations": []},
                        },
                    }
                }
            ]
        },
    }


def v2_prepared(selection_mode="model_selection", responses=None):
    metadata = {
        "version": 2,
        "snapshot_hash": "a" * 64,
        "summary_hash": "b" * 64,
        "selection_mode": selection_mode,
        "applied_refs": [{"id": "human-1"}, {"id": "human-2"}],
        "results": [{"kind": "drop_superseded"}],
        "observations": [],
        "important_refs": [],
        "model": "worker-v2",
        "effort": "high",
        "responses": responses
        if responses is not None
        else [
            {
                "stage": "instruction_selection",
                "response_id": "select-1",
                "seconds": 2.0,
                "usage": {
                    "input_tokens": 30,
                    "output_tokens": 6,
                    "total_tokens": 36,
                },
            },
            {
                "stage": "summary",
                "response_id": "summary-1",
                "seconds": 3.0,
                "usage": {
                    "input_tokens": 70,
                    "output_tokens": 15,
                    "total_tokens": 85,
                },
            },
        ],
    }
    if selection_mode == "no_candidates":
        metadata["applied_refs"] = []
        metadata["results"] = []
    return {
        "type": "compacted",
        "timestamp": "2026-09-23",
        "payload": {"replacement_history_metadata": [{"rencrow_compaction": metadata}]},
    }


def v3_capacity_prepared(
    capacity_metadata=None, mode="capacity", include_capacity_metadata=True
):
    inner = capacity_metadata
    if inner is None:
        inner = v2_prepared()["payload"]["replacement_history_metadata"][0][
            "rencrow_compaction"
        ]
    metadata = {
        "version": 3,
        "mode": mode,
        "task_state": {"boundary": {"revision": 2}},
    }
    if include_capacity_metadata:
        metadata["capacity_metadata"] = inner
    return {
        "type": "compacted",
        "timestamp": "2026-09-25",
        "payload": {"replacement_history_metadata": [{"rencrow_compaction": metadata}]},
    }


class MeasurementTests(unittest.TestCase):
    def test_deduplication_and_stage_cost_not_added_twice(self):
        m = Metrics()
        for row in [
            usage("before", 1000),
            usage("s", 200),
            prepared(),
            {"type": "rencrow_compaction_commit"},
            usage("after", 600),
            usage("before", 1000),
        ]:
            m.consume(row)
        r = m.report()
        self.assertEqual(r["total_recorded_tokens"], 1800)
        c = r["compactions"][0]
        self.assertEqual(
            (
                c["before_observed_input_tokens"],
                c["after_observed_input_tokens"],
                c["stage_total_tokens"],
            ),
            (1000, 600, 200),
        )
        self.assertEqual(c["semantic_continuity"], "requires_independent_review")

    def test_uncommitted_checkpoint_not_counted(self):
        m = Metrics()
        m.consume(prepared())
        self.assertEqual(m.report()["compactions"], [])
        self.assertTrue(m.report()["prepared_checkpoint_without_marker"])

    def test_partial_live_line_waits_for_completion(self):
        m = Metrics()
        text = json.dumps(usage("a", 5)).encode()
        stream = io.BytesIO(text)
        read_available(stream, m)
        self.assertEqual(m.index, 0)
        stream.seek(0, 2)
        stream.write(b"\n")
        stream.seek(0)
        read_available(stream, m)
        self.assertEqual(m.report()["total_recorded_tokens"], 5)

    def test_bad_complete_line_is_not_silently_skipped(self):
        with self.assertRaises(json.JSONDecodeError):
            read_available(io.BytesIO(b"not-json\n"), Metrics())

    def test_conflicting_response_identity_rejected(self):
        m = Metrics()
        m.consume(usage("a", 5))
        with self.assertRaises(ValueError):
            m.consume(usage("a", 9))

    def test_missing_comparison_stays_unknown(self):
        m = Metrics()
        m.consume(prepared())
        m.consume({"type": "rencrow_compaction_commit"})
        self.assertIsNone(m.report()["compactions"][0]["observed_input_delta"])

    def test_v2_counts_real_applied_ranges_and_results(self):
        m = Metrics()
        m.consume(v2_prepared())
        m.consume({"type": "rencrow_compaction_commit"})

        checkpoint = m.report()["compactions"][0]
        self.assertEqual(checkpoint["selection_mode"], "model_selection")
        self.assertEqual(checkpoint["applied_ref_count"], 2)
        self.assertEqual(checkpoint["result_count"], 1)
        self.assertNotIn("approved_operations", checkpoint)
        self.assertEqual(checkpoint["stage_total_tokens"], 121)
        self.assertEqual(
            [stage["output_tok_per_wall_second"] for stage in checkpoint["stages"]],
            [3.0, 5.0],
        )

    def test_v2_no_candidates_and_pending_checkpoint(self):
        m = Metrics()
        m.consume(
            v2_prepared(
                "no_candidates",
                responses=[
                    {
                        "stage": "summary",
                        "response_id": "summary-no-candidates",
                        "seconds": 1.0,
                        "usage": None,
                    }
                ],
            )
        )
        report = m.report()
        self.assertEqual(report["compactions"], [])
        self.assertTrue(report["prepared_checkpoint_without_marker"])

        m.consume({"type": "rencrow_compaction_commit"})
        checkpoint = m.report()["compactions"][0]
        self.assertEqual(checkpoint["selection_mode"], "no_candidates")
        self.assertEqual(checkpoint["applied_ref_count"], 0)
        self.assertEqual(checkpoint["result_count"], 0)
        self.assertIsNone(checkpoint["stage_total_tokens"])

    def test_v3_task_boundary_with_inherited_v2_is_not_helper_compaction(self):
        mixed = v2_prepared()
        mixed["timestamp"] = "2026-09-24"
        mixed["payload"]["replacement_history_metadata"].append(
            {
                "rencrow_compaction": {
                    "version": 3,
                    "mode": "task_boundary",
                    "task_state": {"boundary": {"revision": 1}},
                }
            }
        )
        rows = [
            usage("normal", 100),
            v2_prepared(),
            {"type": "rencrow_compaction_commit"},
            mixed,
            {"type": "rencrow_compaction_commit"},
        ]
        metrics = Metrics()
        for row in rows:
            metrics.consume(row)
        report = metrics.report()
        self.assertEqual(metrics.index, len(rows))
        self.assertEqual(len(report["compactions"]), 1)
        self.assertEqual(report["compactions"][0]["index"], 3)
        self.assertFalse(report["prepared_checkpoint_without_marker"])

    def test_v3_capacity_reads_nested_responses_only_after_commit_marker(self):
        compacted = v3_capacity_prepared()
        metrics = Metrics()
        metrics.consume(compacted)

        self.assertEqual(metrics.report()["compactions"], [])
        self.assertTrue(metrics.report()["prepared_checkpoint_without_marker"])

        metrics.consume({"type": "rencrow_compaction_commit"})
        checkpoint = metrics.report()["compactions"][0]
        self.assertEqual(checkpoint["selection_mode"], "model_selection")
        self.assertEqual(checkpoint["model"], "worker-v2")
        self.assertEqual(checkpoint["effort"], "high")
        self.assertEqual(checkpoint["applied_ref_count"], 2)
        self.assertEqual(checkpoint["result_count"], 1)
        self.assertEqual(checkpoint["stage_total_tokens"], 121)
        self.assertEqual(checkpoint["seconds"], 5.0)

    def test_v3_unknown_mode_or_invalid_capacity_metadata_is_skipped(self):
        for compacted in (
            v3_capacity_prepared(mode="future_mode"),
            v3_capacity_prepared(include_capacity_metadata=False),
            v3_capacity_prepared(capacity_metadata={"version": 3, "responses": []}),
        ):
            metrics = Metrics()
            metrics.consume(compacted)
            metrics.consume({"type": "rencrow_compaction_commit"})
            self.assertEqual(metrics.report()["compactions"], [])
            self.assertFalse(metrics.report()["prepared_checkpoint_without_marker"])

    def test_v3_capacity_unknown_stage_units_remain_unknown(self):
        inner = v2_prepared(
            responses=[
                {
                    "stage": "summary",
                    "response_id": "summary-unknown-seconds",
                    "usage": {
                        "input_tokens": 10,
                        "output_tokens": 2,
                        "total_tokens": 12,
                    },
                }
            ]
        )["payload"]["replacement_history_metadata"][0]["rencrow_compaction"]
        metrics = Metrics()
        metrics.consume(v3_capacity_prepared(capacity_metadata=inner))
        metrics.consume({"type": "rencrow_compaction_commit"})

        checkpoint = metrics.report()["compactions"][0]
        self.assertIsNone(checkpoint["seconds"])
        self.assertEqual(checkpoint["stage_total_tokens"], 12)

    def test_missing_v2_usage_is_unknown_and_rate_uses_real_output(self):
        m = Metrics()
        m.consume(
            v2_prepared(
                responses=[
                    {
                        "stage": "instruction_selection",
                        "response_id": "select-1",
                        "seconds": 2.0,
                        "usage": None,
                    },
                    {
                        "stage": "summary",
                        "response_id": "summary-1",
                        "seconds": 3.0,
                        "usage": {
                            "input_tokens": 70,
                            "output_tokens": 15,
                            "total_tokens": 85,
                        },
                    },
                ]
            )
        )
        m.consume({"type": "rencrow_compaction_commit"})

        checkpoint = m.report()["compactions"][0]
        self.assertIsNone(checkpoint["stage_total_tokens"])
        self.assertEqual(
            [stage["output_tok_per_wall_second"] for stage in checkpoint["stages"]],
            [None, 5.0],
        )

    def test_incomplete_global_usage_is_not_reported_as_complete(self):
        m = Metrics()
        m.consume(
            {
                "type": "token_usage_record",
                "payload": {
                    "response_id": "partial",
                    "turn_id": "t",
                    "usage": {"input_tokens": 7, "total_tokens": 9},
                },
            }
        )
        report = m.report()
        self.assertEqual(report["recorded_input_tokens"], 7)
        self.assertIsNone(report["recorded_output_tokens"])

    def test_incomplete_usage_does_not_crash_turn_rate(self):
        m = Metrics()
        m.consume(
            {
                "type": "token_usage_record",
                "payload": {
                    "response_id": "unknown-output",
                    "turn_id": "t",
                    "usage": {"input_tokens": 4, "total_tokens": 4},
                },
            }
        )
        m.consume(
            {
                "type": "event_msg",
                "payload": {
                    "type": "task_complete",
                    "turn_id": "t",
                    "duration_ms": 1000,
                },
            }
        )
        self.assertIsNone(m.report()["last_completed_turn_output_tok_per_wall_second"])

    def test_watch_terminal_prints_unknown_stage_rate(self):
        class StopWatch(Exception):
            pass

        compacted = v2_prepared(
            "no_candidates",
            responses=[
                {
                    "stage": "summary",
                    "response_id": "summary-no-usage",
                    "seconds": 1.0,
                    "usage": None,
                }
            ],
        )
        rows = [compacted, {"type": "rencrow_compaction_commit"}]
        with tempfile.TemporaryDirectory() as directory:
            rollout = f"{directory}/rollout.jsonl"
            with open(rollout, "w", encoding="utf-8") as stream:
                for row in rows:
                    stream.write(json.dumps(row) + "\n")
            output = io.StringIO()
            with patch.object(sys, "argv", ["metrics", rollout, "--watch"]):
                with patch(
                    "rencrow_compaction_metrics.time.sleep", side_effect=StopWatch
                ):
                    with redirect_stdout(output), self.assertRaises(StopWatch):
                        metrics_main()
        self.assertIn("stages tok/sec: [None]", output.getvalue())


if __name__ == "__main__":
    unittest.main()
