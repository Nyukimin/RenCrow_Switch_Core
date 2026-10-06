import json
from pathlib import Path
import sqlite3
import tempfile
import unittest

from rencrow_compaction_diagnose import diagnose, main


class DiagnoseTests(unittest.TestCase):
    def test_v2_only_correlates_warning_without_emitting_private_text(self):
        with tempfile.TemporaryDirectory() as directory:
            rollout = Path(directory) / "rollout.jsonl"
            logs_db = Path(directory) / "logs.sqlite"
            rows = [
                {
                    "timestamp": "2026-10-04T10:00:00Z",
                    "type": "session_meta",
                    "payload": {"id": "thread-1"},
                },
                {
                    "timestamp": "2026-10-04T10:00:01Z",
                    "type": "compacted",
                    "payload": {
                        "message": "private conversation",
                        "replacement_history": [{"private": "text"}],
                        "replacement_history_metadata": [{"client_authored": True}],
                    },
                },
                {
                    "timestamp": "2026-10-04T10:00:02Z",
                    "type": "compacted",
                    "payload": {
                        "message": "private conversation",
                        "replacement_history": [{"private": "text"}],
                        "replacement_history_metadata": [
                            {
                                "rencrow_compaction": {
                                    "version": 2,
                                    "selection_mode": "deterministic_emergency",
                                    "semantic_summary_hash": None,
                                    "responses": [],
                                }
                            }
                        ],
                    },
                },
                {
                    "timestamp": "2026-10-04T10:00:03Z",
                    "type": "rencrow_compaction_commit",
                    "payload": {},
                },
            ]
            rollout.write_text(
                "".join(json.dumps(row) + "\n" for row in rows), encoding="utf-8"
            )
            with sqlite3.connect(logs_db) as db:
                db.execute(
                    "CREATE TABLE logs (id INTEGER PRIMARY KEY, ts INTEGER, ts_nanos INTEGER, level TEXT, target TEXT, feedback_log_body TEXT, file TEXT, line INTEGER, thread_id TEXT)"
                )
                db.execute(
                    "INSERT INTO logs VALUES (1,1791108002,0,'WARN','codex_core::compact::rencrow',?, 'compact_rencrow.rs',357,'thread-1')",
                    (
                        "RenCrow Normal compaction failed; using Emergency error=compaction summary response contains a non-assistant item private-token",
                    ),
                )
                db.execute(
                    "INSERT INTO logs VALUES (2,1791108002,0,'WARN','codex_core::compact::rencrow',?, 'compact_rencrow.rs',357,'other-thread')",
                    ("private other-thread",),
                )
            report = diagnose(rollout, logs_db)
            self.assertEqual(report["v2_checkpoint_count"], 1)
            self.assertEqual(report["emergency_count"], 1)
            self.assertEqual(report["checkpoints"][0]["commit_line"], 4)
            self.assertEqual(
                report["v2_warnings"][0]["reason"], "summary_non_assistant_item"
            )
            self.assertEqual(report["uncommitted_checkpoint_count"], 0)
            self.assertEqual(len(report["v2_warnings"]), 1)
            self.assertNotIn("private", json.dumps(report))

    def test_malformed_rollout_fails_closed(self):
        with tempfile.TemporaryDirectory() as directory:
            rollout = Path(directory) / "rollout.jsonl"
            rollout.write_text(
                '{"type":"session_meta","payload":{"id":"thread"}}\n{bad}\n',
                encoding="utf-8",
            )
            self.assertEqual(main(["--rollout", str(rollout)]), 2)


if __name__ == "__main__":
    unittest.main()
