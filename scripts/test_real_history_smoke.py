"""Synthetic contract checks for the aggregate-only real-history smoke runner."""

import os
from contextlib import redirect_stderr
import io
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

from real_history_smoke import build_report, main, run_command, run_json


class RealHistorySmokeTests(unittest.TestCase):
    def run_rejected_smoke(self, codex_home: Path, store: Path, report: Path) -> str:
        with (
            patch("real_history_smoke.run_command") as run_command_mock,
            patch("real_history_smoke.run_json") as run_json_mock,
            redirect_stderr(io.StringIO()) as stderr,
        ):
            run_command_mock.return_value = 0.0
            run_json_mock.side_effect = lambda _binary, _arguments, _timeout, command: (
                0.0,
                {"command": command, "data": {}},
            )
            result = main(
                [
                    "--codex-home",
                    str(codex_home),
                    "--store",
                    str(store),
                    "--report",
                    str(report),
                ]
            )

        self.assertEqual(result, 1, stderr.getvalue())
        run_command_mock.assert_not_called()
        run_json_mock.assert_not_called()
        return stderr.getvalue()

    def test_rejects_canonical_output_collisions_before_refresh(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            codex_home = root / "codex-home"
            codex_home.mkdir()
            store = root / "store.sqlite"
            store.write_text("store sentinel", encoding="utf-8")
            hard_link = root / "store-hard-link.sqlite"
            os.link(store, hard_link)
            alias_parent = root / "alias-parent"
            alias_parent.mkdir()
            canonical_alias = alias_parent / ".." / store.name
            before = store.read_bytes()

            for report in (store, canonical_alias, hard_link):
                error = self.run_rejected_smoke(codex_home, store, report)
                self.assertIn("different files", error)
                self.assertEqual(store.read_bytes(), before)

    def test_rejects_repository_local_outputs_before_refresh(self):
        repository_root = Path(__file__).resolve().parents[1]
        with tempfile.TemporaryDirectory() as temporary, tempfile.TemporaryDirectory(
            dir=repository_root
        ) as repository_output:
            root = Path(temporary)
            codex_home = root / "codex-home"
            codex_home.mkdir()
            outside_store = root / "store.sqlite"
            outside_report = root / "report.json"
            local_target = Path(repository_output) / "output"
            local_target.write_text("output sentinel", encoding="utf-8")
            before = local_target.read_bytes()

            for store, report in (
                (local_target, outside_report),
                (outside_store, local_target),
            ):
                error = self.run_rejected_smoke(codex_home, store, report)
                self.assertIn("outside the repository", error)
                self.assertEqual(local_target.read_bytes(), before)

    def test_unresolvable_output_paths_fail_with_bounded_error(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            codex_home = root / "codex-home"
            codex_home.mkdir()
            store = root / "store.sqlite"
            report = root / "report.json"
            store.write_text("store sentinel", encoding="utf-8")
            report.write_text("report sentinel", encoding="utf-8")
            before = (store.read_bytes(), report.read_bytes())
            original_resolve = Path.resolve

            for unresolvable_path, description in (
                (store, "--store path"),
                (report, "--report path"),
            ):
                def fail_resolution(path, *args, **kwargs):
                    if path == unresolvable_path:
                        raise RuntimeError("symlink loop: /private/path")
                    return original_resolve(path, *args, **kwargs)

                with patch.object(
                    Path, "resolve", autospec=True, side_effect=fail_resolution
                ):
                    error = self.run_rejected_smoke(codex_home, store, report)

                self.assertIn(f"could not resolve {description}", error)
                self.assertNotIn("/private/path", error)
                self.assertEqual((store.read_bytes(), report.read_bytes()), before)

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
