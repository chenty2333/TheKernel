#!/usr/bin/env python3
"""Validate and normalize one ASID switch diagnostic snapshot."""

from __future__ import annotations

import argparse
import csv
import sys
from pathlib import Path
from typing import TextIO


SCHEMA = "thekernel-asid-switch-diagnostics-v1"
COLUMNS = (
    "schema",
    "enabled",
    "fast_path_avoided",
    "fallback_asid_zero",
    "fallback_invalid_width",
    "fallback_exhausted",
    "fallback_generation_mismatch",
    "fallback_same_id_different_root",
    "saturated",
)


class EvidenceError(ValueError):
    """Raised when the raw snapshot is missing or malformed."""


def parse_fields(line: str) -> dict[str, str]:
    prefix = "ASID_SWITCH_DIAGNOSTICS "
    fields: dict[str, str] = {}
    for token in line.removeprefix(prefix).split(" "):
        if not token or "=" not in token:
            raise EvidenceError(f"malformed ASID diagnostic field: {token!r}")
        key, value = token.split("=", 1)
        if not key or not value or key in fields:
            raise EvidenceError(f"invalid ASID diagnostic field: {token!r}")
        fields[key] = value
    if set(fields) != set(COLUMNS):
        raise EvidenceError(
            "ASID diagnostic fields mismatch: "
            f"expected={sorted(COLUMNS)!r} actual={sorted(fields)!r}"
        )
    return fields


def parse_uint(value: str, field: str) -> int:
    if not value.isascii() or not value.isdecimal():
        raise EvidenceError(f"invalid {field}: {value!r}")
    return int(value, 10)


def parse_snapshot(path: Path) -> dict[str, str]:
    try:
        lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
    except OSError as error:
        raise EvidenceError(f"cannot read log {path}: {error}") from error
    records = [
        parse_fields(line)
        for line in lines
        if line.startswith("ASID_SWITCH_DIAGNOSTICS ")
    ]
    if len(records) != 1:
        raise EvidenceError(
            f"expected one ASID_SWITCH_DIAGNOSTICS record, found {len(records)}"
        )
    record = records[0]
    if record["schema"] != SCHEMA:
        raise EvidenceError(f"unsupported ASID diagnostic schema: {record['schema']!r}")
    if record["enabled"] != "0":
        raise EvidenceError("captured ASID diagnostics must be disabled")
    if record["saturated"] not in {"0", "1"}:
        raise EvidenceError(f"invalid saturated flag: {record['saturated']!r}")
    counters = [parse_uint(record[field], field) for field in COLUMNS[2:-1]]
    if sum(counters) == 0:
        raise EvidenceError("ASID diagnostic workload recorded no switch decisions")
    return record


def write_tsv(record: dict[str, str], output: TextIO) -> None:
    writer = csv.DictWriter(
        output, fieldnames=COLUMNS, delimiter="\t", lineterminator="\n"
    )
    writer.writeheader()
    writer.writerow(record)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("log", type=Path)
    parser.add_argument("--output", type=Path)
    arguments = parser.parse_args()
    try:
        record = parse_snapshot(arguments.log)
        if arguments.output is None:
            write_tsv(record, sys.stdout)
        else:
            with arguments.output.open("w", encoding="utf-8", newline="") as output:
                write_tsv(record, output)
    except (EvidenceError, OSError) as error:
        print(f"parse-asid-switch-diagnostics: FAIL: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
