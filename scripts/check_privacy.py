#!/usr/bin/env python3
"""Offline repository checks. Print paths/rule IDs, never matching content."""

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import sys


RULES = {
    "private-key": rb"-----BEGIN (?:[A-Z0-9]+ )?PRIVATE KEY-----\s+[A-Za-z0-9+/=]{32,}",
    "github-token": rb"\b(?:gh[pousr]_[A-Za-z0-9]{36,}|github_pat_[A-Za-z0-9_]{60,})\b",
    "openai-token": rb"\bsk-(?:(?:proj|svcacct)-)?[A-Za-z0-9_-]{40,}\b",
    "aws-access-key": rb"\b(?:AKIA|ASIA)[A-Z0-9]{16}\b",
}
LIMIT = 8 * 1024 * 1024


def findings(name, data):
    path = Path(name)
    fixture = path.parts[:2] == ("tests", "fixtures")
    rules = []
    if (path.name == ".env" or path.name.startswith(".env.")) and path.name != ".env.example":
        rules.append("environment-file")
    if ".codexlens" in path.parts or path.name in ("auth.json", "credentials.json"):
        rules.append("private-artifact")
    if not fixture and re.search(r"\.(?:sqlite(?:3)?|db)(?:-(?:wal|shm|journal))?$", path.name):
        rules.append("database-outside-fixtures")
    for rule, pattern in RULES.items():
        if re.search(pattern, data):
            rules.append(rule)
    if fixture and re.search(rb"/Users/|/home/|[A-Za-z]:[\\/]Users[\\/]", data):
        rules.append("fixture-home-path")
    return rules


def git(root, *args):
    return subprocess.check_output(["git", "-C", str(root), *args], stderr=subprocess.DEVNULL)


def scan(root, staged=False):
    entries = git(root, "ls-files", "--stage", "-z").split(b"\0") if staged else git(
        root, "ls-files", "--cached", "--others", "--exclude-standard", "-z"
    ).split(b"\0")
    failures = []
    for entry in sorted(set(entries) - {b""}):
        mode = None
        if staged:
            metadata, entry = entry.split(b"\t", 1)
            mode, _, stage = metadata.split()
            if stage != b"0":
                failures.append((os.fsdecode(entry), "unmerged-index"))
                continue
        name = os.fsdecode(entry)
        path = root / name
        try:
            if mode in (b"120000", b"160000") or any(
                parent.is_symlink() for parent in (path, *path.parents) if parent != root
            ):
                failures.append((name, "unscanned-link"))
                continue
            if not staged and not path.exists():
                continue  # A tracked deletion has no working-tree content.
            if not staged and not path.is_file():
                failures.append((name, "unscanned-special-file"))
                continue
            if staged:
                size = int(git(root, "cat-file", "-s", ":" + name))
            else:
                size = path.stat().st_size
            if size > LIMIT:
                failures.append((name, "oversized-file"))
                continue
            data = git(root, "show", ":" + name) if staged else path.read_bytes()
            failures.extend((name, rule) for rule in findings(name, data))
        except (OSError, ValueError, subprocess.CalledProcessError):
            failures.append((name, "unreadable-file"))
    return failures


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--staged", action="store_true", help="scan index blobs, not working files")
    args = parser.parse_args()
    try:
        root = Path(git(Path.cwd(), "rev-parse", "--show-toplevel").decode().strip())
        failures = scan(root, args.staged)
    except (OSError, subprocess.CalledProcessError):
        print("privacy: repository scan unavailable", file=sys.stderr)
        return 1
    for name, rule in failures:
        print("privacy: {}: {}".format(json.dumps(name), rule), file=sys.stderr)
    if not failures:
        print("privacy: PASS ({})".format("index" if args.staged else "working tree"))
    return int(bool(failures))


if __name__ == "__main__":
    sys.exit(main())
