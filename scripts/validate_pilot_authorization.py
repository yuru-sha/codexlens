"""Validate a private finding-pilot authorization record."""

from __future__ import annotations

import json
import re
import sys
from datetime import datetime, timezone
from typing import Any

REQUIRED_FIELDS = (
    "source_scope",
    "project_scope",
    "observation_period",
    "period_since",
    "period_until",
    "archive_inclusion",
    "storage_location",
    "retention_deletion_policy",
    "owner_authorization",
)
RFC3339_BOUND = re.compile(
    r"^(?P<prefix>\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2})"
    r"(?:\.(?P<fraction>\d{1,9}))?"
    r"(?P<zone>Z|[+-]\d{2}:\d{2})$"
)


def _parse_bound(record: dict[str, Any], field: str) -> tuple[datetime, int]:
    value = record[field]
    match = RFC3339_BOUND.fullmatch(value)
    if match is None:
        raise ValueError("authorization period invalid")
    fraction = match.group("fraction") or ""
    fraction_nanos = (fraction + "000000000")[:9]
    normalized = match.group("prefix")
    if fraction:
        normalized += "." + fraction_nanos[:6]
    normalized += match.group("zone")
    if normalized.endswith("Z"):
        normalized = normalized[:-1] + "+00:00"
    try:
        parsed = datetime.fromisoformat(normalized)
    except ValueError as error:
        raise ValueError("authorization period invalid") from error
    if parsed.tzinfo is None:
        raise ValueError("authorization period invalid")
    return parsed.astimezone(timezone.utc), int(fraction_nanos[6:])


def read_authorization(path: str) -> tuple[str, str]:
    try:
        with open(path, encoding="utf-8") as stream:
            record = json.load(stream)
    except (OSError, ValueError) as error:
        raise ValueError("authorization gate incomplete") from error
    if not isinstance(record, dict) or any(
        not isinstance(record.get(field), str) or not record[field].strip()
        for field in REQUIRED_FIELDS
    ):
        raise ValueError("authorization gate incomplete")

    period_since = _parse_bound(record, "period_since")
    period_until = _parse_bound(record, "period_until")
    if period_since >= period_until:
        raise ValueError("authorization period invalid")
    return record["period_since"], record["period_until"]


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print("usage: validate_pilot_authorization.py AUTHORIZATION_RECORD", file=sys.stderr)
        return 2
    try:
        period_since, period_until = read_authorization(argv[1])
    except ValueError as error:
        print(str(error), file=sys.stderr)
        return 1
    print(period_since, period_until, sep="\t")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
