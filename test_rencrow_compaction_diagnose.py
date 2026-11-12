import hashlib
import json
import os
from pathlib import Path
import sqlite3
import stat
import tempfile
import unittest

from rencrow_compaction_diagnose import _reason, diagnose, main


class DiagnoseTests(unittest.TestCase):
    def _inputs(self, root):
        rollout = root / "rollout.jsonl"
        logs = root / "logs.sqlite"
        rows = [
            {"type": "session_meta", "payload": {"id": "thread-1"}},
            {
                "type": "token_usage_record",
                "payload": {"thread_id": "thread-1", "response_id": "response-1"},
            },
            {"type": "event_msg", "payload": {"type": "token_count"}},
            {
                "type": "compacted",
                "payload": {
                    "replacement_history": [],
                    "replacement_history_metadata": [
                        {
                            "rencrow_compaction": {
                                "version": 2,
                                "selection_mode": "deterministic_emergency",
                                "responses": [],
                            }
                        }
                    ],
                },
            },
            {"type": "rencrow_compaction_commit", "payload": {}},
        ]
        rollout.write_text(
            "".join(json.dumps(row) + "\n" for row in rows), encoding="utf-8"
        )
        warning = "RenCrow Normal compaction failed; using Emergency error=schema rejected private-token"
        with sqlite3.connect(logs) as db:
            db.execute(
                "CREATE TABLE logs (id INTEGER, ts INTEGER, ts_nanos INTEGER, "
                "level TEXT, target TEXT, feedback_log_body TEXT, file TEXT, "
                "line INTEGER, thread_id TEXT)"
            )
            db.executemany(
                "INSERT INTO logs VALUES (?,1791108002,0,'WARN',"
                "'codex_core::compact::rencrow',?,'compact_rencrow.rs',357,?)",
                [(1, warning, "thread-1"), (2, "private other thread", "other-thread")],
            )
        return rollout, logs, warning

    def _trace(self, bundle, events, payloads):
        bundle.mkdir()
        for path, body in payloads.items():
            target = bundle / path
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(body)
        (bundle / "trace.jsonl").write_text(
            "".join(json.dumps(event) + "\n" for event in events),
            encoding="utf-8",
        )

    def _event(self, thread, response, path):
        return {
            "thread_id": thread,
            "payload": {
                "type": "inference_completed",
                "response_id": response,
                "response_payload": {"path": path},
            },
        }

    def test_conservative_summary_is_not_counted_as_fully_accepted_selection(self):
        with tempfile.TemporaryDirectory() as temp:
            rollout, logs, _ = self._inputs(Path(temp))
            rows = [json.loads(line) for line in rollout.read_text().splitlines()]
            metadata = rows[3]["payload"]["replacement_history_metadata"][0][
                "rencrow_compaction"
            ]
            metadata["selection_mode"] = "conservative_selection"
            metadata["semantic_summary_hash"] = "1" * 64
            rollout.write_text("".join(json.dumps(row) + "\n" for row in rows))
            report = diagnose(rollout, logs)
            self.assertEqual(
                (
                    report["normal_count"],
                    report["conservative_count"],
                    report["emergency_count"],
                ),
                (0, 1, 0),
            )

    def test_default_report_keeps_bodies_private_and_classifies_known_reason(self):
        with tempfile.TemporaryDirectory() as temp:
            rollout, logs, _ = self._inputs(Path(temp))
            report = diagnose(rollout, logs)
            self.assertEqual(report["v2_warnings"][0]["reason"], "schema_rejected")
            self.assertNotIn("emergency_response_matches", report)
            self.assertNotIn("private", json.dumps(report))
            self.assertNotIn("_response_id", json.dumps(report))
            self.assertEqual(
                _reason("RenCrow Normal compaction failed; using Emergency"),
                "other_normal_failure",
            )
            self.assertEqual(_reason("unrecognized V2 warning"), "other_v2_warning")

    def test_bridge_and_trace_match_require_exact_thread_and_response_id(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            rollout, logs, _ = self._inputs(root)
            bundle = root / "trace"
            body = b'{"response_id":"response-1","output":[]}'
            events = [
                self._event("thread-1", "other-response", "payloads/other.json"),
                self._event("other-thread", "response-1", "payloads/wrong.json"),
                self._event("thread-1", "response-1", "payloads/response.json"),
            ]
            self._trace(
                bundle,
                events,
                {
                    "payloads/other.json": b"{}",
                    "payloads/wrong.json": b"{}",
                    "payloads/response.json": body,
                },
            )
            report = diagnose(rollout, logs, bundle)
            match = report["emergency_response_matches"][0]
            self.assertEqual(
                report["response_match_basis"],
                "exact same-thread response_id; causal link not established",
            )
            self.assertEqual(match["status"], "matched")
            self.assertEqual(match["path"], "payloads/response.json")
            self.assertEqual(match["size_bytes"], len(body))
            self.assertEqual(match["sha256"], hashlib.sha256(body).hexdigest())
            self.assertNotIn(body.decode(), json.dumps(report))
            unmatched = root / "unmatched"
            self._trace(
                unmatched,
                events[:2],
                {"payloads/other.json": b"{}", "payloads/wrong.json": b"{}"},
            )
            self.assertEqual(
                diagnose(rollout, logs, unmatched)["emergency_response_matches"][0][
                    "status"
                ],
                "response_not_found",
            )

    def test_unsafe_reused_duplicate_and_corrupt_payloads_fail_closed(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            rollout, logs, _ = self._inputs(root)
            cases = [
                (
                    "traversal",
                    [self._event("thread-1", "response-1", "../outside")],
                    {},
                    "unsafe_payload_path",
                ),
                (
                    "reused",
                    [
                        self._event("thread-1", "response-1", "p.json"),
                        self._event("other-thread", "other", "p.json"),
                    ],
                    {"p.json": b"{}"},
                    "payload_path_reused",
                ),
                (
                    "duplicate",
                    [
                        self._event("thread-1", "response-1", "a.json"),
                        self._event("thread-1", "response-1", "b.json"),
                    ],
                    {"a.json": b"{}", "b.json": b"{}"},
                    "duplicate_response_match",
                ),
                (
                    "missing",
                    [self._event("thread-1", "response-1", "missing.json")],
                    {},
                    "payload_missing",
                ),
                (
                    "corrupt",
                    [self._event("thread-1", "response-1", "bad.json")],
                    {"bad.json": b"{malformed"},
                    "response_payload_corrupt",
                ),
                (
                    "response_id_mismatch",
                    [self._event("thread-1", "response-1", "mismatch.json")],
                    {"mismatch.json": b'{"response_id":"another-response"}'},
                    "response_payload_response_id_mismatch",
                ),
            ]
            for name, events, payloads, expected in cases:
                bundle = root / name
                self._trace(bundle, events, payloads)
                report = diagnose(rollout, logs, bundle)
                self.assertEqual(
                    report["emergency_response_matches"][0]["status"], expected
                )
            self.assertEqual(
                (root / "corrupt" / "bad.json").read_bytes(), b"{malformed"
            )
            if os.name == "posix":
                outside = root / "outside.json"
                outside.write_bytes(b"{}")
                bundle = root / "symlink"
                self._trace(
                    bundle,
                    [self._event("thread-1", "response-1", "link.json")],
                    {"link.json": b"{}"},
                )
                (bundle / "link.json").unlink()
                (bundle / "link.json").symlink_to(outside)
                self.assertEqual(
                    diagnose(rollout, logs, bundle)["emergency_response_matches"][0][
                        "status"
                    ],
                    "payload_path_symlink",
                )

    def test_warning_output_is_new_private_file_with_exact_same_thread_body(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            rollout, logs, warning = self._inputs(root)
            output = root / "warnings.bin"
            report = diagnose(rollout, logs, warning_output=output)
            framed = output.read_bytes()
            length = int.from_bytes(framed[:8], "big")
            self.assertEqual(framed[8 : 8 + length], warning.encode("utf-8"))
            self.assertEqual(len(framed), 8 + length)
            self.assertEqual(report["v2_warnings"][0]["reason"], "schema_rejected")
            self.assertNotIn(warning, json.dumps(report))
            if os.name == "posix":
                self.assertEqual(stat.S_IMODE(output.stat().st_mode), 0o600)
            with self.assertRaises(FileExistsError):
                diagnose(rollout, logs, warning_output=output)

    def test_malformed_rollout_fails_closed(self):
        with tempfile.TemporaryDirectory() as temp:
            rollout = Path(temp) / "rollout.jsonl"
            rollout.write_text(
                '{"type":"session_meta","payload":{"id":"thread"}}\n{bad}\n',
                encoding="utf-8",
            )
            self.assertEqual(main(["--rollout", str(rollout)]), 2)


if __name__ == "__main__":
    unittest.main()
