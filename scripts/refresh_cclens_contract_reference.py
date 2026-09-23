#!/usr/bin/env python3
"""Regenerate the bounded cclens output oracle from its pinned source build."""

import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile


ROOT = Path(__file__).resolve().parents[1]
FIXTURE = ROOT / "tests/fixtures/analysis/cclens-command-contract"
REVISION = "3df5f76eb14a53c4cb03d975fd4dbd4eb2f7cc70"
VIEWS = ("usage", "inventory", "waste", "overhead", "prompts", "failures", "stuck", "doctor")


def run(binary: Path, args: list[str], env: dict[str, str]) -> dict:
    result = subprocess.run(
        [str(binary), *args], check=True, capture_output=True, text=True, env=env
    )
    return json.loads(result.stdout)


def compact(command: str, report: dict) -> dict:
    if command == "analyze":
        return {
            key: report[key]
            for key in (
                "sessions",
                "skill_invocations",
                "surfaces",
                "subagent_tokens",
                "subagents",
                "permission_denials",
            )
        }
    if command == "usage":
        return {"skills": report["skills"], "tokens": report["tokens"]}
    if command == "inventory":
        fields = ("kind", "id", "scope", "uses", "status")
        return {"surfaces": [{key: row[key] for key in fields} for row in report["surfaces"]]}
    if command == "waste":
        fields = ("kind", "id", "scope", "uses", "wedge")
        return {"wedges": [{key: row[key] for key in fields} for row in report["wedges"]]}
    if command == "overhead":
        return {key: report[key] for key in ("config_tokens", "floor", "per_project", "residual")}
    if command in ("prompts", "failures"):
        return report
    if command == "stuck":
        fields = ("project", "session_id", "file", "edits", "span_secs")
        return {"episodes": [{key: row[key] for key in fields} for row in report["episodes"]]}
    return {
        "global": {key: report["global"][key] for key in ("friction_global", "hotspots", "unused")},
        "projects": [
            {key: row[key] for key in ("project", "thrash")} for row in report["projects"]
        ],
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cclens", required=True, type=Path, help="binary built from pinned source")
    parser.add_argument("--source", required=True, type=Path, help="cclens source checkout")
    parser.add_argument(
        "--output",
        type=Path,
        default=FIXTURE / "reference-output.json",
        help="reference JSON destination",
    )
    args = parser.parse_args()

    source = subprocess.run(
        ["git", "-C", str(args.source), "rev-parse", "HEAD"],
        check=True,
        capture_output=True,
        text=True,
    ).stdout.strip()
    if source != REVISION:
        parser.error(f"cclens source must be {REVISION}, got {source}")

    env = os.environ.copy()
    env["CLAUDE_CONFIG_DIR"] = str(FIXTURE / "config")
    with tempfile.TemporaryDirectory(prefix="cclens-contract-") as temporary:
        database = Path(temporary) / "cclens.db"
        analyzed = run(
            args.cclens,
            [
                "analyze",
                "--projects",
                str(FIXTURE / "projects"),
                "--format",
                "json",
                "--db",
                str(database),
            ],
            env,
        )
        reports = {"analyze": compact("analyze", analyzed)}
        reports["sql"] = {
            "sessions": run(
                args.cclens,
                [
                    "sql",
                    "SELECT COUNT(*) AS sessions FROM sessions",
                    "--format",
                    "json",
                    "--db",
                    str(database),
                ],
                env,
            )[0]["sessions"]
        }
        for command in VIEWS:
            report = run(
                args.cclens,
                [command, "--frozen", "--format", "json", "--db", str(database)],
                env,
            )
            reports[command] = compact(command, report)
        prompt = subprocess.run(
            [str(args.cclens), "optimize", "--frozen", "--print", "--db", str(database)],
            check=True,
            capture_output=True,
            text=True,
            env=env,
        ).stdout.lower()
        reports["optimize"] = {
            "signals": {
                "command-not-found": "command-not-found" in prompt,
                "test-failure": "test-failure" in prompt,
                "stuck-file": "lib.rs" in prompt,
                "unused-skill": "unused-skill" in prompt,
            }
        }

    reference = {
        "oracle": f"cclens@{REVISION}",
        "fixture": "tests/fixtures/analysis/cclens-command-contract",
        "reports": reports,
    }
    args.output.write_text(json.dumps(reference, indent=2, sort_keys=True) + "\n")


if __name__ == "__main__":
    main()
