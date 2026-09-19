"""Synthetic contract checks for the aggregate-only real-history smoke runner."""

import unittest

from real_history_smoke import build_report


class RealHistorySmokeTests(unittest.TestCase):
    def test_report_records_scope_counts_runtime_and_raw_immutability(self):
        report = build_report(
            scope="project:/synthetic/project",
            before={"file_count": 2, "byte_count": 10, "tree_sha256": "same"},
            after={"file_count": 2, "byte_count": 10, "tree_sha256": "same"},
            durations={"refresh": 0.1, "analyze": 0.2, "doctor": 0.3, "optimize": 0.4},
            analyze={
                "data": {
                    "coverage": {"status": "partial", "limitations": ["synthetic gap"]},
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
        self.assertEqual(report["runtime_seconds"], 1.0)


if __name__ == "__main__":
    unittest.main()
