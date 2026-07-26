#!/usr/bin/env python3
"""Validate capability-only PMU records without accepting measurements."""

from __future__ import annotations

import argparse
import csv
import sys
from pathlib import Path


SCHEMA = "thekernel-pmu-capabilities-v1"
EVENTS = (
    "cpu_cycles",
    "instructions",
    "dtlb_read_misses",
    "dtlb_write_misses",
    "itlb_read_misses",
)
SOURCE_BY_ARCH = {"rv": "sbi-pmu", "la": "loongarch-pmcfg"}


class EvidenceError(ValueError):
    """Raised when capability-only records are missing or claim samples."""


def parse_fields(line: str, prefix: str) -> dict[str, str]:
    fields: dict[str, str] = {}
    for token in line.removeprefix(prefix).split(" "):
        if not token or "=" not in token:
            raise EvidenceError(f"malformed {prefix.strip()} field: {token!r}")
        key, value = token.split("=", 1)
        if not key or not value or key in fields:
            raise EvidenceError(f"invalid {prefix.strip()} field: {token!r}")
        fields[key] = value
    return fields


def require_fields(fields: dict[str, str], expected: set[str], context: str) -> None:
    if set(fields) != expected:
        raise EvidenceError(
            f"{context} fields mismatch: "
            f"expected={sorted(expected)!r} actual={sorted(fields)!r}"
        )


def parse_nonnegative(value: str, field: str) -> int:
    if not value.isascii() or not value.isdecimal():
        raise EvidenceError(f"invalid {field}: {value!r}")
    return int(value, 10)


def validate(path: Path, arch: str) -> tuple[dict[str, str], dict[str, dict[str, str]]]:
    try:
        lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
    except OSError as error:
        raise EvidenceError(f"cannot read log {path}: {error}") from error
    headers = [
        parse_fields(line, "PMU_CAPABILITIES ")
        for line in lines
        if line.startswith("PMU_CAPABILITIES ")
    ]
    if len(headers) != 1:
        raise EvidenceError(f"expected one PMU_CAPABILITIES record, found {len(headers)}")
    header = headers[0]
    require_fields(
        header,
        {
            "schema",
            "source",
            "counter_count",
            "consistent_snapshot",
            "samples_collected",
        },
        "PMU_CAPABILITIES",
    )
    if header["schema"] != SCHEMA:
        raise EvidenceError(f"unsupported PMU capability schema: {header['schema']!r}")
    expected_source = SOURCE_BY_ARCH[arch]
    if header["source"] != expected_source:
        raise EvidenceError(
            f"PMU capability source mismatch: arch={arch!r} "
            f"expected={expected_source!r} actual={header['source']!r}"
        )
    parse_nonnegative(header["counter_count"], "counter_count")
    if header["consistent_snapshot"] not in {"0", "1"}:
        raise EvidenceError(
            f"invalid consistent_snapshot: {header['consistent_snapshot']!r}"
        )
    if header["samples_collected"] != "0":
        raise EvidenceError(
            "capability-only evidence must use samples_collected=0"
        )

    events: dict[str, dict[str, str]] = {}
    for line in lines:
        if not line.startswith("PMU_EVENT "):
            continue
        fields = parse_fields(line, "PMU_EVENT ")
        require_fields(fields, {"event", "requestable", "sampled"}, "PMU_EVENT")
        name = fields["event"]
        if name not in EVENTS:
            raise EvidenceError(f"unexpected PMU event: {name!r}")
        if name in events:
            raise EvidenceError(f"duplicate PMU event: {name}")
        if fields["requestable"] not in {"0", "1"}:
            raise EvidenceError(
                f"PMU event {name} has invalid requestable flag: "
                f"{fields['requestable']!r}"
            )
        if fields["sampled"] != "0":
            raise EvidenceError(f"capability-only PMU event {name} must use sampled=0")
        events[name] = fields
    if set(events) != set(EVENTS):
        raise EvidenceError(
            "PMU event set mismatch: "
            f"expected={sorted(EVENTS)!r} actual={sorted(events)!r}"
        )
    return header, events


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("log", type=Path)
    parser.add_argument("--arch", choices=tuple(SOURCE_BY_ARCH), required=True)
    arguments = parser.parse_args()
    try:
        header, events = validate(arguments.log, arguments.arch)
        writer = csv.writer(sys.stdout, delimiter="\t", lineterminator="\n")
        writer.writerow(("schema", "source", "counter_count", "consistent_snapshot"))
        writer.writerow(
            (
                header["schema"],
                header["source"],
                header["counter_count"],
                header["consistent_snapshot"],
            )
        )
        writer.writerow(("event", "requestable", "sampled"))
        for name in EVENTS:
            writer.writerow((name, events[name]["requestable"], "0"))
    except EvidenceError as error:
        print(f"parse-pmu-capabilities: FAIL: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
