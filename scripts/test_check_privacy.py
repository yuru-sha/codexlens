"""Synthetic privacy gate regressions; no production credentials or history."""

from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

from check_privacy import findings, scan


class PrivacyChecks(unittest.TestCase):
    def test_artifacts_and_synthetic_fixture_boundary(self):
        for name in (".codexlens.sqlite", ".codexlens.sqlite-wal", "data.db", ".env.local", "auth.json"):
            self.assertTrue(findings(name, b""), name)
        self.assertEqual(findings("tests/fixtures/state/sample.sqlite", b"SQLite format 3\0"), [])
        self.assertEqual(findings(".env.example", b"TOKEN=replace-me"), [])
        self.assertIn("fixture-home-path", findings("tests/fixtures/sample.jsonl", b"/" + b"Users/example"))

    def test_secret_shapes_without_committing_secret_like_values(self):
        samples = {
            "github-token": b"ghp_" + b"x" * 36,
            "openai-token": b"sk-proj-" + b"x" * 48,
            "aws-access-key": b"AKIA" + b"X" * 16,
            "private-key": b"-----BEGIN " + b"PRIVATE KEY-----\n" + b"A" * 64,
        }
        for rule, value in samples.items():
            self.assertIn(rule, findings("sample.txt", value))
            self.assertIn(rule, findings("tests/fixtures/sample.txt", value))
        self.assertEqual(findings("sample.txt", b"token=synthetic-placeholder"), [])

    def test_index_is_checked_even_when_working_copy_is_safe(self):
        root = Path(__file__).resolve().parents[1]
        scratch = root / ".codexlens"
        scratch.mkdir(exist_ok=True)
        with tempfile.TemporaryDirectory(dir=scratch) as directory:
            repo = Path(directory)
            subprocess.run(["git", "init", "-q", str(repo)], check=True)
            value = b"ghp_" + b"x" * 36
            (repo / "sample.txt").write_bytes(value)
            subprocess.run(["git", "-C", str(repo), "add", "sample.txt"], check=True)
            (repo / "sample.txt").write_text("safe\n")
            self.assertEqual(scan(repo), [])
            self.assertIn(("sample.txt", "github-token"), scan(repo, staged=True))
            result = subprocess.run(
                [sys.executable, "-B", str(root / "scripts/check_privacy.py"), "--staged"],
                cwd=repo, capture_output=True, check=False,
            )
            self.assertEqual(result.returncode, 1)
            self.assertNotIn(value, result.stdout + result.stderr)
            (repo / "scripts").mkdir()
            (repo / "scripts/check_privacy.py").write_bytes(
                (root / "scripts/check_privacy.py").read_bytes())
            hook = subprocess.run(
                ["git", "-C", str(repo), "-c", "core.hooksPath=" + str(root / ".githooks"),
                 "hook", "run", "pre-commit"], capture_output=True, check=False,
            )
            self.assertNotEqual(hook.returncode, 0)
            self.assertIn(b"github-token", hook.stderr)
            self.assertNotIn(value, hook.stdout + hook.stderr)
            (repo / ".codexlens.sqlite").write_bytes(b"synthetic database")
            self.assertIn((".codexlens.sqlite", "database-outside-fixtures"), scan(repo))


if __name__ == "__main__":
    unittest.main()
