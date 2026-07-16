#!/usr/bin/env python3
"""Validate and normalize the guest MM performance evidence stream."""

from __future__ import annotations

import argparse
import csv
import json
import re
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import TextIO


EXPECTED_METRICS = (
    "vma_scale",
    "mremap_latency",
    "protect_touch_latency",
    "pin_throughput",
    "pin_contention",
)
PIN_METRICS = frozenset({"pin_throughput", "pin_contention"})
REASON_RE = re.compile(r"^[A-Za-z0-9_.:-]+$")


class EvidenceError(ValueError):
    """Raised when a guest log does not contain a complete evidence contract."""


@dataclass(frozen=True)
class Metric:
    name: str
    status: str
    count: int
    p50_ns: int | None
    p99_ns: int | None
    p999_ns: int | None
    throughput_bytes_per_sec: int | None
    reason: str | None
    error_number: int | None


@dataclass(frozen=True)
class Evidence:
    arch: str
    requested_cpus: int
    online_cpus: int
    metrics: tuple[Metric, ...]


def parse_fields(line: str, prefix: str) -> dict[str, str]:
    fields: dict[str, str] = {}
    payload = line.removeprefix(prefix)
    if not payload or payload.startswith(" "):
        raise EvidenceError(f"malformed {prefix.strip()} record: {line!r}")
    for token in payload.split(" "):
        if not token or "=" not in token:
            raise EvidenceError(f"malformed field in record: {line!r}")
        key, value = token.split("=", 1)
        if not key or not value:
            raise EvidenceError(f"empty key or value in record: {line!r}")
        if key in fields:
            raise EvidenceError(f"duplicate field {key!r} in record: {line!r}")
        fields[key] = value
    return fields


def require_keys(
    fields: dict[str, str], required: set[str], allowed: set[str], context: str
) -> None:
    missing = sorted(required - fields.keys())
    unknown = sorted(fields.keys() - allowed)
    if missing:
        raise EvidenceError(f"{context} is missing fields: {', '.join(missing)}")
    if unknown:
        raise EvidenceError(f"{context} has unknown fields: {', '.join(unknown)}")


def parse_nonnegative_int(text: str, field: str, context: str) -> int:
    if not text.isascii() or not text.isdecimal():
        raise EvidenceError(f"{context} has invalid {field}: {text!r}")
    return int(text, 10)


def parse_positive_int(text: str, field: str, context: str) -> int:
    value = parse_nonnegative_int(text, field, context)
    if value == 0:
        raise EvidenceError(f"{context} requires positive {field}")
    return value


def parse_topology(fields: dict[str, str], requested_cpus: int) -> int:
    context = "MM_PERF_TOPOLOGY"
    status = fields.get("status")
    if status == "missing":
        require_keys(
            fields,
            {"status", "online_cpus", "reason", "errno"},
            {"status", "online_cpus", "reason", "errno"},
            context,
        )
        if fields["online_cpus"] != "missing":
            raise EvidenceError("missing topology must use online_cpus=missing")
        reason = fields["reason"]
        if not REASON_RE.fullmatch(reason):
            raise EvidenceError(f"topology has invalid reason: {reason!r}")
        error_number = parse_nonnegative_int(fields["errno"], "errno", context)
        raise EvidenceError(
            f"guest CPU topology unavailable: reason={reason} errno={error_number}"
        )
    if status != "ok":
        raise EvidenceError(f"topology has invalid status: {status!r}")
    require_keys(
        fields,
        {"status", "online_cpus"},
        {"status", "online_cpus"},
        context,
    )
    online_cpus = parse_positive_int(fields["online_cpus"], "online_cpus", context)
    if online_cpus != requested_cpus:
        raise EvidenceError(
            "guest CPU topology mismatch: "
            f"requested={requested_cpus} online={online_cpus}"
        )
    return online_cpus


def parse_metric(fields: dict[str, str]) -> Metric:
    common = {"metric", "status", "count", "p50_ns", "p99_ns", "p999_ns"}
    name = fields.get("metric", "")
    context = f"MM_PERF metric={name or '<missing>'}"
    if name not in EXPECTED_METRICS:
        raise EvidenceError(f"unexpected metric name: {name!r}")
    throughput_key = {"throughput_bytes_per_sec"} if name in PIN_METRICS else set()
    status = fields.get("status")

    if status == "ok":
        require_keys(fields, common | throughput_key, common | throughput_key, context)
        count = parse_positive_int(fields["count"], "count", context)
        p50 = parse_nonnegative_int(fields["p50_ns"], "p50_ns", context)
        p99 = parse_nonnegative_int(fields["p99_ns"], "p99_ns", context)
        p999 = parse_nonnegative_int(fields["p999_ns"], "p999_ns", context)
        if not p50 <= p99 <= p999:
            raise EvidenceError(
                f"{context} has non-monotonic quantiles: {p50}, {p99}, {p999}"
            )
        throughput = None
        if name in PIN_METRICS:
            throughput = parse_positive_int(
                fields["throughput_bytes_per_sec"],
                "throughput_bytes_per_sec",
                context,
            )
        return Metric(name, status, count, p50, p99, p999, throughput, None, None)

    if status == "missing":
        missing_keys = common | throughput_key | {"reason", "errno"}
        require_keys(fields, missing_keys, missing_keys, context)
        if fields["count"] != "0":
            raise EvidenceError(f"{context} missing record must use count=0")
        for percentile in ("p50_ns", "p99_ns", "p999_ns"):
            if fields[percentile] != "missing":
                raise EvidenceError(
                    f"{context} missing record must use {percentile}=missing"
                )
        if name in PIN_METRICS and fields["throughput_bytes_per_sec"] != "missing":
            raise EvidenceError(
                f"{context} missing record must use throughput_bytes_per_sec=missing"
            )
        reason = fields["reason"]
        if not REASON_RE.fullmatch(reason):
            raise EvidenceError(f"{context} has invalid reason: {reason!r}")
        error_number = parse_nonnegative_int(fields["errno"], "errno", context)
        return Metric(name, status, 0, None, None, None, None, reason, error_number)

    raise EvidenceError(f"{context} has invalid status: {status!r}")


def parse_evidence(log: Path, arch: str, requested_cpus: int) -> Evidence:
    topology_fields: dict[str, str] | None = None
    metrics: dict[str, Metric] = {}
    done_count = 0
    semantics_count = 0

    try:
        lines = log.read_text(encoding="utf-8", errors="replace").splitlines()
    except OSError as error:
        raise EvidenceError(f"cannot read log {log}: {error}") from error

    for line in lines:
        if line.startswith("MM_PERF_TOPOLOGY "):
            if topology_fields is not None:
                raise EvidenceError("duplicate MM_PERF_TOPOLOGY record")
            topology_fields = parse_fields(line, "MM_PERF_TOPOLOGY ")
        elif line.startswith("MM_PERF "):
            metric = parse_metric(parse_fields(line, "MM_PERF "))
            if metric.name in metrics:
                raise EvidenceError(f"duplicate metric record: {metric.name}")
            metrics[metric.name] = metric
        elif line.startswith("MM_PERF_SEMANTICS "):
            fields = parse_fields(line, "MM_PERF_SEMANTICS ")
            require_keys(fields, {"status"}, {"status"}, "MM_PERF_SEMANTICS")
            if fields["status"] != "ok":
                raise EvidenceError(
                    f"MM_PERF_SEMANTICS has invalid status: {fields['status']!r}"
                )
            semantics_count += 1
        elif line.startswith("MM_PERF_DONE "):
            fields = parse_fields(line, "MM_PERF_DONE ")
            require_keys(fields, {"status"}, {"status"}, "MM_PERF_DONE")
            if fields["status"] != "ok":
                raise EvidenceError(
                    f"MM_PERF_DONE has invalid status: {fields['status']!r}"
                )
            done_count += 1

    if topology_fields is None:
        raise EvidenceError("missing MM_PERF_TOPOLOGY record")
    online_cpus = parse_topology(topology_fields, requested_cpus)
    if semantics_count != 1:
        raise EvidenceError(
            f"expected one MM_PERF_SEMANTICS record, found {semantics_count}"
        )
    if done_count != 1:
        raise EvidenceError(f"expected one MM_PERF_DONE record, found {done_count}")
    missing_metrics = [name for name in EXPECTED_METRICS if name not in metrics]
    if missing_metrics:
        raise EvidenceError(
            "missing required metric records: " + ", ".join(missing_metrics)
        )
    return Evidence(
        arch=arch,
        requested_cpus=requested_cpus,
        online_cpus=online_cpus,
        metrics=tuple(metrics[name] for name in EXPECTED_METRICS),
    )


def write_tsv(evidence: Evidence, output: TextIO) -> None:
    writer = csv.writer(output, delimiter="\t", lineterminator="\n")
    writer.writerow(
        (
            "arch",
            "requested_cpus",
            "online_cpus",
            "metric",
            "status",
            "count",
            "p50_ns",
            "p99_ns",
            "p999_ns",
            "throughput_bytes_per_sec",
            "reason",
            "errno",
        )
    )
    for metric in evidence.metrics:
        writer.writerow(
            (
                evidence.arch,
                evidence.requested_cpus,
                evidence.online_cpus,
                metric.name,
                metric.status,
                metric.count,
                metric.p50_ns if metric.p50_ns is not None else "missing",
                metric.p99_ns if metric.p99_ns is not None else "missing",
                metric.p999_ns if metric.p999_ns is not None else "missing",
                (
                    metric.throughput_bytes_per_sec
                    if metric.throughput_bytes_per_sec is not None
                    else ("missing" if metric.name in PIN_METRICS else "-")
                ),
                metric.reason or "-",
                metric.error_number if metric.error_number is not None else "-",
            )
        )


def write_json(evidence: Evidence, output: TextIO) -> None:
    payload = {
        "arch": evidence.arch,
        "requested_cpus": evidence.requested_cpus,
        "online_cpus": evidence.online_cpus,
        "metrics": [
            {
                "metric": metric.name,
                "status": metric.status,
                "count": metric.count,
                "p50_ns": metric.p50_ns,
                "p99_ns": metric.p99_ns,
                "p999_ns": metric.p999_ns,
                "throughput_bytes_per_sec": metric.throughput_bytes_per_sec,
                "reason": metric.reason,
                "errno": metric.error_number,
            }
            for metric in evidence.metrics
        ],
    }
    json.dump(payload, output, indent=2, sort_keys=True)
    output.write("\n")


def main() -> int:
    parser = argparse.ArgumentParser(
        description="validate and normalize TheKernel MM performance evidence"
    )
    parser.add_argument("log", type=Path)
    parser.add_argument("--arch", required=True, choices=("rv", "la", "host"))
    parser.add_argument("--cpus", required=True, type=int)
    parser.add_argument("--format", choices=("tsv", "json"), default="tsv")
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    if args.cpus <= 0:
        parser.error("--cpus must be positive")

    try:
        evidence = parse_evidence(args.log, args.arch, args.cpus)
        if args.output is None:
            output = sys.stdout
            close_output = False
        else:
            args.output.parent.mkdir(parents=True, exist_ok=True)
            output = args.output.open("w", encoding="utf-8", newline="")
            close_output = True
        try:
            if args.format == "json":
                write_json(evidence, output)
            else:
                write_tsv(evidence, output)
        finally:
            if close_output:
                output.close()
    except (EvidenceError, OSError) as error:
        print(f"parse-mm-performance: FAIL: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
