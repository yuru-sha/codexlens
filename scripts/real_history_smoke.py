#!/usr/bin/env python3
"""Run an aggregate-only smoke check against an explicitly selected Codex home."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
from typing import Any


class SmokeError(RuntimeError):
    """A bounded, user-actionable smoke-run failure."""


LIMITATION_SUMMARIES = {
    "missing_activity_timestamp": "missing activity timestamp",
    "missing_lifecycle_timestamp": "missing lifecycle timestamp",
    "invalid_timestamp": "invalid timestamp",
    "oversized_line": "oversized input line",
    "unreadable": "unreadable input",
    "malformed_json": "malformed JSON",
    "state_schema_mismatch": "state schema mismatch",
    "state_query": "state query failure",
    "metadata_conflict": "conflicting metadata",
    "opaque_tool_input": "opaque tool input",
    "unsupported_reader": "unsupported input reader",
}


def nonnegative_count(value: Any) -> int:
    return value if isinstance(value, int) and value >= 0 else 0


def limitation_summary(limitation: Any) -> dict[str, Any]:
    if not isinstance(limitation, dict):
        return {
            "kind": "unknown",
            "summary": "coverage limitation",
            "selected_sessions": 0,
            "selected_records": 0,
            "affected_lenses": [],
        }
    kind = str(limitation.get("kind", "unknown"))[:64]
    lenses = limitation.get("affected_lenses")
    return {
        "kind": kind,
        "summary": LIMITATION_SUMMARIES.get(kind, "coverage limitation"),
        "selected_sessions": nonnegative_count(limitation.get("selected_sessions")),
        "selected_records": nonnegative_count(limitation.get("selected_records")),
        "affected_lenses": [str(lens)[:64] for lens in lenses[:8]]
        if isinstance(lenses, list)
        else [],
    }


def freshness_summary(analyze_data: dict[str, Any], doctor: dict[str, Any]) -> dict[str, Any]:
    freshness = analyze_data.get("freshness") or doctor.get("freshness") or {}
    latest = freshness.get("latest_ingested_at")
    return {
        "state": str(freshness.get("state", "unknown"))[:32],
        "source_count": nonnegative_count(freshness.get("source_count")),
        "latest_ingested_at": None if latest is None else str(latest)[:64],
    }


def snapshot_inputs(
    root: Path, output_paths: tuple[Path, ...] = ()
) -> dict[str, int | str]:
    """Hash inputs without exposing paths, rejecting output aliases."""

    entries: list[bytes] = []
    byte_count = 0

    def raise_walk_error(error: OSError) -> None:
        raise SmokeError("could not read the selected Codex input tree") from error

    for directory, directories, files in os.walk(root, onerror=raise_walk_error, followlinks=False):
        directories[:] = sorted(
            name
            for name in directories
            if not (Path(directory) / name).is_symlink()
        )
        for name in sorted(files):
            path = Path(directory) / name
            if path.is_symlink() or not path.is_file():
                continue
            for output in output_paths:
                try:
                    aliases_input = path.samefile(output)
                except FileNotFoundError:
                    continue
                except OSError as error:
                    raise SmokeError("could not compare smoke outputs with Codex inputs") from error
                if aliases_input:
                    raise SmokeError("smoke output aliases a selected Codex input")
            digest = hashlib.sha256()
            try:
                with path.open("rb") as stream:
                    for chunk in iter(lambda: stream.read(1024 * 1024), b""):
                        digest.update(chunk)
                size = path.stat().st_size
            except OSError as error:
                raise SmokeError("could not read the selected Codex input tree") from error
            relative = path.relative_to(root).as_posix().encode("utf-8")
            entries.append(relative + b"\0" + digest.digest() + b"\n")
            byte_count += size

    return {
        "file_count": len(entries),
        "byte_count": byte_count,
        "tree_sha256": hashlib.sha256(b"".join(entries)).hexdigest(),
    }


def build_report(
    *,
    scope: str,
    before: dict[str, int | str],
    after: dict[str, int | str],
    durations: dict[str, float],
    analyze: dict[str, Any],
    doctor: dict[str, Any],
    optimize: dict[str, Any],
) -> dict[str, Any]:
    analyze_data = analyze.get("data", {})
    doctor_data = doctor.get("data", {})
    coverage = analyze_data.get("coverage") or doctor.get("coverage") or {}
    limitations = coverage.get("limitations") or []
    finding_counts = {
        str(kind): int(count)
        for kind, count in (analyze_data.get("finding_counts") or {}).items()
    }
    proposals = optimize.get("data", {}).get("proposals") or {}
    rendered = proposals.get("rendered") or []
    skipped = proposals.get("skipped") or []
    top_fixes = doctor_data.get("top_fixes") or []

    return {
        "scope": scope,
        "coverage": {
            "status": coverage.get("status", "unknown"),
            "partial_or_unknown": coverage.get("status") in {"partial", "unknown"}
            or bool(limitations),
            "limitations_count": len(limitations),
            "limitations": [limitation_summary(limitation) for limitation in limitations],
            "limitations_omitted": nonnegative_count(coverage.get("limitations_omitted")),
            "session_count": coverage.get("session_count", 0),
            "record_count": coverage.get("record_count", 0),
        },
        "freshness": freshness_summary(analyze_data, doctor),
        "finding_count": sum(finding_counts.values()),
        "finding_counts": finding_counts,
        "proposal_count": len(rendered) + len(skipped),
        "reviewable_proposal_count": len(rendered),
        "skipped_proposal_count": len(skipped),
        "actionable_output": bool(top_fixes or rendered),
        "runtime_seconds": round(sum(durations.values()), 3),
        "command_runtime_seconds": {
            name: round(duration, 3) for name, duration in durations.items()
        },
        "raw_input_immutable": before == after,
        "raw_input_before": before,
        "raw_input_after": after,
    }


def run_process(
    binary: str,
    arguments: list[str],
    timeout: float,
    *,
    capture_stdout: bool = False,
) -> tuple[float, bytes]:
    started = time.monotonic()
    try:
        result = subprocess.run(
            [binary, *arguments],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE if capture_stdout else subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
            timeout=timeout,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise SmokeError("smoke command could not complete") from error
    if result.returncode:
        raise SmokeError(f"smoke command failed with exit code {result.returncode}")
    return time.monotonic() - started, result.stdout or b""


def run_command(binary: str, arguments: list[str], timeout: float) -> float:
    duration, _ = run_process(binary, arguments, timeout)
    return duration


def run_json(
    binary: str,
    arguments: list[str],
    timeout: float,
    expected_command: str,
) -> tuple[float, dict[str, Any]]:
    duration, stdout = run_process(
        binary,
        arguments,
        timeout,
        capture_stdout=True,
    )
    try:
        document = json.loads(stdout)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise SmokeError("smoke command did not return one JSON document") from error
    if not isinstance(document, dict):
        raise SmokeError("smoke command returned an invalid JSON document")
    if document.get("command") != expected_command:
        raise SmokeError("smoke command returned an unexpected command document")
    return duration, document


def outside(path: Path, root: Path) -> bool:
    try:
        path.relative_to(root)
    except ValueError:
        return True
    return False


def resolve_path(path: Path, description: str) -> Path:
    try:
        return path.resolve()
    except (OSError, RuntimeError) as error:
        raise SmokeError(f"could not resolve {description}") from error


def same_output_target(store: Path, report: Path) -> bool:
    if store == report:
        return True
    try:
        if store.samefile(report):
            return True
    except FileNotFoundError:
        pass
    except OSError as error:
        raise SmokeError("could not compare --store and --report paths") from error
    if store.as_posix().casefold() != report.as_posix().casefold():
        return False
    return not case_sensitive_filesystem(store.parent)


def case_sensitive_filesystem(directory: Path) -> bool:
    while True:
        try:
            directory.stat()
        except FileNotFoundError:
            pass
        except OSError as error:
            raise SmokeError("could not compare --store and --report paths") from error
        else:
            if directory.is_dir():
                break
        if directory.parent == directory:
            raise SmokeError("could not compare --store and --report paths")
        directory = directory.parent

    # Probe the volume directly; the host OS does not determine its case rules.
    try:
        with tempfile.TemporaryDirectory(prefix="CodexLensCaseProbe-", dir=directory) as name:
            probe = Path(name)
            try:
                return not probe.samefile(probe.with_name(probe.name.swapcase()))
            except FileNotFoundError:
                return True
    except OSError as error:
        raise SmokeError("could not compare --store and --report paths") from error


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--codex-home", required=True, help="explicit absolute Codex input directory")
    parser.add_argument("--store", required=True, help="absolute derived-store path outside Codex home")
    parser.add_argument("--report", required=True, help="absolute aggregate report path outside Codex home")
    parser.add_argument("--scope", default="all", help="all, global, project, or project:PATH")
    parser.add_argument("--binary", default="codexlens", help="codexlens executable")
    parser.add_argument("--include-archived", action="store_true")
    parser.add_argument("--include-subagents", action="store_true")
    parser.add_argument("--timeout", type=float, default=300.0, help="per-command timeout in seconds")
    parser.add_argument(
        "--require-actionable",
        action="store_true",
        help="fail when the selected history produces no doctor fix or reviewable proposal",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    codex_home = Path(args.codex_home)
    store = Path(args.store)
    report_path = Path(args.report)
    try:
        if not codex_home.is_absolute() or not codex_home.is_dir():
            raise SmokeError("--codex-home must be an existing absolute directory")
        if not store.is_absolute() or not report_path.is_absolute():
            raise SmokeError("--store and --report must be absolute paths")
        codex_home = resolve_path(codex_home, "--codex-home path")
        store = resolve_path(store, "--store path")
        report_path = resolve_path(report_path, "--report path")
        repository_root = resolve_path(Path(__file__), "repository root").parent.parent
        if not outside(store, repository_root) or not outside(report_path, repository_root):
            raise SmokeError("--store and --report must stay outside the repository")
        if not outside(store, codex_home) or not outside(report_path, codex_home):
            raise SmokeError("--store and --report must stay outside --codex-home")
        if same_output_target(store, report_path):
            raise SmokeError("--store and --report must be different files")
        if args.timeout <= 0:
            raise SmokeError("--timeout must be positive")
        selection = ["--store", str(store), "--scope", args.scope]
        if args.include_archived:
            selection.append("--include-archived")
        if args.include_subagents:
            selection.append("--include-subagents")

        before = snapshot_inputs(codex_home, (store, report_path))
        store.parent.mkdir(parents=True, exist_ok=True)
        durations: dict[str, float] = {}
        refresh = ["refresh", "--codex-home", str(codex_home), "--store", str(store)]
        if args.include_archived:
            refresh.append("--include-archived")
        if args.include_subagents:
            refresh.append("--include-subagents")
        durations["refresh"] = run_command(args.binary, refresh, args.timeout)
        durations["analyze"], analyze = run_json(
            args.binary,
            ["analyze", "--frozen", "--format", "json", *selection],
            args.timeout,
            "analyze",
        )
        durations["doctor"], doctor = run_json(
            args.binary,
            ["doctor", "--frozen", "--format", "json", *selection],
            args.timeout,
            "doctor",
        )
        durations["optimize"], optimize = run_json(
            args.binary,
            ["optimize", "--print", "--frozen", "--format", "json", *selection],
            args.timeout,
            "optimize",
        )
        after = snapshot_inputs(codex_home)
        report = build_report(
            scope=args.scope,
            before=before,
            after=after,
            durations=durations,
            analyze=analyze,
            doctor=doctor,
            optimize=optimize,
        )
        report_path.parent.mkdir(parents=True, exist_ok=True)
        report_path.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    except (OSError, SmokeError) as error:
        print(str(error), file=sys.stderr)
        return 1

    if args.require_actionable and not report["actionable_output"]:
        print("real-history smoke found no actionable output", file=sys.stderr)
        return 1
    print(
        "real-history smoke: "
        f"{report['finding_count']} findings, {report['proposal_count']} proposals, "
        f"coverage={report['coverage']['status']}, "
        f"actionable={str(report['actionable_output']).lower()}, "
        f"raw_input_immutable={str(report['raw_input_immutable']).lower()}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
