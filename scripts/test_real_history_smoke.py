"""Synthetic contract checks for the aggregate-only real-history smoke runner."""

import json
import subprocess
import unittest
from unittest.mock import patch

from real_history_smoke import build_report, run_command, run_json


class RealHistorySmokeTests(unittest.TestCase):
    def test_runner_discards_non_json_output_but_keeps_json_stdout(self):
        completed = subprocess.CompletedProcess(
            [], 0, stdout=b'{"command":"analyze"}', stderr=b"raw"
        )
        with patch("real_history_smoke.subprocess.run", return_value=completed) as run:
            run_command("codexlens", ["refresh"], 10)
            run_json("codexlens", ["analyze"], 10, "analyze")

        command_call, json_call = run.call_args_list
        self.assertIs(command_call.kwargs["stdout"], subprocess.DEVNULL)
        self.assertIs(command_call.kwargs["stderr"], subprocess.DEVNULL)
        self.assertIs(json_call.kwargs["stdout"], subprocess.PIPE)
        self.assertIs(json_call.kwargs["stderr"], subprocess.DEVNULL)

    def test_report_records_scope_counts_runtime_and_raw_immutability(self):
        report = build_report(
            scope="project:/synthetic/project",
            before={"file_count": 2, "byte_count": 10, "tree_sha256": "same"},
            after={"file_count": 2, "byte_count": 10, "tree_sha256": "same"},
            durations={"refresh": 0.1, "analyze": 0.2, "doctor": 0.3, "optimize": 0.4},
            analyze={
                "data": {
                    "coverage": {
                        "status": "partial",
                        "limitations": [
                            {
                                "kind": "missing_activity_timestamp",
                                "message": "private source path must not be copied",
                                "selected_sessions": 1,
                                "selected_records": 2,
                                "affected_lenses": ["usage"],
                            }
                        ],
                        "limitations_omitted": 1,
                    },
                    "freshness": {
                        "state": "recorded",
                        "source_count": 3,
                        "latest_ingested_at": "2026-09-19T13:00:00Z",
                    },
                    "finding_counts": {"failure": 2, "stuck": 1},
                }
            },
            doctor={"data": {"top_fixes": [{}]}, "coverage": {"status": "partial"}},
            optimize={"data": {"proposals": {"rendered": [{}], "skipped": [{}]}}},
        )

        self.assertEqual(report["scope"], "project:/synthetic/project")
        self.assertEqual(report["finding_count"], 3)
        self.assertEqual(report["proposal_count"], 2)
        self.assertEqual(report["reviewable_proposal_count"], 1)
        self.assertTrue(report["actionable_output"])
        self.assertTrue(report["raw_input_immutable"])
        self.assertTrue(report["coverage"]["partial_or_unknown"])
        self.assertEqual(
            report["coverage"]["limitations"],
            [
                {
                    "kind": "missing_activity_timestamp",
                    "summary": "missing activity timestamp",
                    "selected_sessions": 1,
                    "selected_records": 2,
                    "affected_lenses": ["usage"],
                }
            ],
        )
        self.assertEqual(report["coverage"]["limitations_omitted"], 1)
        self.assertEqual(
            report["freshness"],
            {
                "state": "recorded",
                "source_count": 3,
                "latest_ingested_at": "2026-09-19T13:00:00Z",
            },
        )
        self.assertNotIn("private source path", json.dumps(report))
        self.assertEqual(report["runtime_seconds"], 1.0)


if __name__ == "__main__":
    unittest.main()
