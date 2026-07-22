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

from mm_performance_schema import (
    EXPECTED_METRICS,
    PIN_METRICS,
    VMA_FIXTURE_METRICS,
)

REASON_RE = re.compile(r"^[A-Za-z0-9_.:-]+$")
RUN_SCHEMA = "thekernel-mm-performance-run-v2"
SINGLE_PROXY_METRIC = "direct_io_pin_proxy_throughput"
SAME_AS_PROXY_METRIC = "direct_io_pin_proxy_same_as_contention"
CROSS_AS_PROXY_METRIC = "direct_io_pin_proxy_cross_as_contention"
MREMAP_CONTENTION_METRIC = "mremap_disjoint_same_as_contention"
MREMAP_CONTENTION_WORKERS = 2
MREMAP_CONTENTION_SLOT_PAGES = 2
MAX_AFFINITY_CPU_IDS = 64
MAX_U64 = (1 << 64) - 1


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
    requested_vmas: int | None
    fixture_vmas: int | None
    reason: str | None
    error_number: int | None


@dataclass(frozen=True)
class Evidence:
    arch: str
    requested_cpus: int
    online_cpus: int
    metrics: tuple[Metric, ...]


@dataclass(frozen=True)
class PinWorker:
    mode: str
    worker: int
    cpu: int
    completed: int
    p99_ns: int
    over_10ms: int
    over_50ms: int
    fixture_before_vmas: int
    fixture_after_vmas: int


@dataclass(frozen=True)
class CrossAsPinWorker:
    worker: int
    pid: int
    cpu: int
    completed: int
    p99_ns: int
    fixture_before_vmas: int
    fixture_after_vmas: int
    cow_isolated: int


@dataclass(frozen=True)
class MremapWorker:
    worker: int
    cpu: int
    completed: int
    slot_a: int
    slot_b: int
    bytes: int
    start_ns: int
    end_ns: int
    p99_ns: int
    fixture_before_vmas: int
    fixture_after_vmas: int


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


def parse_run(
    fields: dict[str, str],
    *,
    arch: str,
    iterations: int,
    vmas: int,
    pin_iterations: int,
    pin_workers: int,
) -> int:
    context = "MM_PERF_RUN"
    keys = {
        "schema",
        "arch",
        "iterations",
        "vmas",
        "pin_iterations",
        "pin_workers",
        "page_size",
    }
    require_keys(fields, keys, keys, context)
    expected = {
        "schema": RUN_SCHEMA,
        "arch": arch,
        "iterations": str(iterations),
        "vmas": str(vmas),
        "pin_iterations": str(pin_iterations),
        "pin_workers": str(pin_workers),
    }
    for key, expected_value in expected.items():
        actual = fields[key]
        if key not in {"schema", "arch"}:
            parse_positive_int(actual, key, context)
        if actual != expected_value:
            raise EvidenceError(
                f"{context} {key} mismatch: "
                f"expected={expected_value!r} actual={actual!r}"
            )
    page_size = parse_positive_int(fields["page_size"], "page_size", context)
    if page_size < 1024 or page_size > 1024 * 1024 or page_size & (page_size - 1):
        raise EvidenceError(f"{context} has invalid page_size: {page_size}")
    return page_size


def parse_pin_worker(fields: dict[str, str]) -> PinWorker:
    context = "MM_PERF_PIN_WORKER"
    keys = {
        "mode",
        "status",
        "worker",
        "cpu",
        "completed",
        "p99_ns",
        "over_10ms",
        "over_50ms",
        "fixture_before_vmas",
        "fixture_after_vmas",
    }
    require_keys(fields, keys, keys, context)
    mode = fields["mode"]
    if mode not in {"single", "contention"}:
        raise EvidenceError(f"{context} has invalid mode: {mode!r}")
    if fields["status"] != "ok":
        raise EvidenceError(f"{context} has invalid status: {fields['status']!r}")
    completed = parse_positive_int(fields["completed"], "completed", context)
    over_10ms = parse_nonnegative_int(fields["over_10ms"], "over_10ms", context)
    over_50ms = parse_nonnegative_int(fields["over_50ms"], "over_50ms", context)
    if over_50ms > over_10ms or over_10ms > completed:
        raise EvidenceError(
            f"{context} has inconsistent tail counters: "
            f"completed={completed} over_10ms={over_10ms} over_50ms={over_50ms}"
        )
    return PinWorker(
        mode=mode,
        worker=parse_nonnegative_int(fields["worker"], "worker", context),
        cpu=parse_nonnegative_int(fields["cpu"], "cpu", context),
        completed=completed,
        p99_ns=parse_positive_int(fields["p99_ns"], "p99_ns", context),
        over_10ms=over_10ms,
        over_50ms=over_50ms,
        fixture_before_vmas=parse_positive_int(
            fields["fixture_before_vmas"], "fixture_before_vmas", context
        ),
        fixture_after_vmas=parse_positive_int(
            fields["fixture_after_vmas"], "fixture_after_vmas", context
        ),
    )


def parse_cross_as_pin_worker(fields: dict[str, str]) -> CrossAsPinWorker:
    context = "MM_PERF_PIN_CROSS_AS_WORKER"
    keys = {
        "status",
        "worker",
        "pid",
        "cpu",
        "completed",
        "p99_ns",
        "fixture_before_vmas",
        "fixture_after_vmas",
        "cow_isolated",
    }
    require_keys(fields, keys, keys, context)
    if fields["status"] != "ok":
        raise EvidenceError(f"{context} has invalid status: {fields['status']!r}")
    return CrossAsPinWorker(
        worker=parse_nonnegative_int(fields["worker"], "worker", context),
        pid=parse_positive_int(fields["pid"], "pid", context),
        cpu=parse_nonnegative_int(fields["cpu"], "cpu", context),
        completed=parse_positive_int(fields["completed"], "completed", context),
        p99_ns=parse_positive_int(fields["p99_ns"], "p99_ns", context),
        fixture_before_vmas=parse_positive_int(
            fields["fixture_before_vmas"], "fixture_before_vmas", context
        ),
        fixture_after_vmas=parse_positive_int(
            fields["fixture_after_vmas"], "fixture_after_vmas", context
        ),
        cow_isolated=parse_nonnegative_int(
            fields["cow_isolated"], "cow_isolated", context
        ),
    )


def parse_mremap_worker(fields: dict[str, str]) -> MremapWorker:
    context = "MM_PERF_MREMAP_WORKER"
    keys = {
        "status",
        "worker",
        "cpu",
        "completed",
        "slot_a",
        "slot_b",
        "bytes",
        "start_ns",
        "end_ns",
        "p99_ns",
        "fixture_before_vmas",
        "fixture_after_vmas",
    }
    require_keys(fields, keys, keys, context)
    if fields["status"] != "ok":
        raise EvidenceError(f"{context} has invalid status: {fields['status']!r}")
    record = MremapWorker(
        worker=parse_nonnegative_int(fields["worker"], "worker", context),
        cpu=parse_nonnegative_int(fields["cpu"], "cpu", context),
        completed=parse_positive_int(fields["completed"], "completed", context),
        slot_a=parse_positive_int(fields["slot_a"], "slot_a", context),
        slot_b=parse_positive_int(fields["slot_b"], "slot_b", context),
        bytes=parse_positive_int(fields["bytes"], "bytes", context),
        start_ns=parse_positive_int(fields["start_ns"], "start_ns", context),
        end_ns=parse_positive_int(fields["end_ns"], "end_ns", context),
        p99_ns=parse_positive_int(fields["p99_ns"], "p99_ns", context),
        fixture_before_vmas=parse_positive_int(
            fields["fixture_before_vmas"], "fixture_before_vmas", context
        ),
        fixture_after_vmas=parse_positive_int(
            fields["fixture_after_vmas"], "fixture_after_vmas", context
        ),
    )
    if record.start_ns >= record.end_ns:
        raise EvidenceError(
            f"{context} worker={record.worker} has an invalid execution window"
        )
    for name, address in (("slot_a", record.slot_a), ("slot_b", record.slot_b)):
        if address > MAX_U64 - record.bytes:
            raise EvidenceError(
                f"{context} worker={record.worker} {name} range overflows"
            )
    return record


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


def parse_affinity(
    fields: dict[str, str], online_cpus: int
) -> tuple[frozenset[int], bool]:
    context = "MM_PERF_AFFINITY"
    require_keys(
        fields,
        {"status", "bytes", "allowed_cpus", "cpu_ids", "cpu_ids_complete"},
        {"status", "bytes", "allowed_cpus", "cpu_ids", "cpu_ids_complete"},
        context,
    )
    if fields["status"] != "ok":
        raise EvidenceError(f"affinity has invalid status: {fields['status']!r}")
    returned_bytes = parse_positive_int(fields["bytes"], "bytes", context)
    allowed_cpus = parse_positive_int(
        fields["allowed_cpus"], "allowed_cpus", context
    )
    if returned_bytes % 8 != 0:
        raise EvidenceError(
            f"affinity byte count is not 64-bit word aligned: {returned_bytes}"
        )
    if returned_bytes * 8 < online_cpus:
        raise EvidenceError(
            f"affinity mask is too short: bytes={returned_bytes} cpus={online_cpus}"
        )
    if allowed_cpus != online_cpus:
        raise EvidenceError(
            "affinity/topology mismatch: "
            f"allowed={allowed_cpus} online={online_cpus}"
        )
    complete_text = fields["cpu_ids_complete"]
    if complete_text not in {"0", "1"}:
        raise EvidenceError(
            f"affinity has invalid cpu_ids_complete: {complete_text!r}"
        )
    raw_ids = fields["cpu_ids"].split(",") if fields["cpu_ids"] else []
    if not raw_ids or len(raw_ids) > MAX_AFFINITY_CPU_IDS:
        raise EvidenceError("affinity cpu_ids list is empty or too long")
    cpu_ids: list[int] = []
    for raw_id in raw_ids:
        cpu_id = parse_nonnegative_int(raw_id, "cpu_ids", context)
        if cpu_ids and cpu_id <= cpu_ids[-1]:
            raise EvidenceError("affinity cpu_ids must be strictly increasing")
        cpu_ids.append(cpu_id)
    complete = complete_text == "1"
    if complete and len(cpu_ids) != allowed_cpus:
        raise EvidenceError(
            "affinity complete cpu_ids count mismatch: "
            f"allowed={allowed_cpus} ids={len(cpu_ids)}"
        )
    if len(cpu_ids) < min(online_cpus, MAX_AFFINITY_CPU_IDS):
        raise EvidenceError(
            "affinity cpu_ids prefix is shorter than the advertised topology"
        )
    return frozenset(cpu_ids), complete


def parse_metric(fields: dict[str, str]) -> Metric:
    common = {"metric", "status", "count", "p50_ns", "p99_ns", "p999_ns"}
    name = fields.get("metric", "")
    context = f"MM_PERF metric={name or '<missing>'}"
    if name not in EXPECTED_METRICS:
        raise EvidenceError(f"unexpected metric name: {name!r}")
    throughput_key = {"throughput_bytes_per_sec"} if name in PIN_METRICS else set()
    fixture_key = (
        {"requested_vmas", "fixture_vmas"}
        if name in VMA_FIXTURE_METRICS
        else set()
    )
    status = fields.get("status")

    if status == "ok":
        record_keys = common | throughput_key | fixture_key
        require_keys(fields, record_keys, record_keys, context)
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
        requested_vmas = None
        fixture_vmas = None
        if name in VMA_FIXTURE_METRICS:
            requested_vmas = parse_positive_int(
                fields["requested_vmas"], "requested_vmas", context
            )
            fixture_vmas = parse_positive_int(
                fields["fixture_vmas"], "fixture_vmas", context
            )
            if fixture_vmas != requested_vmas:
                raise EvidenceError(
                    f"{context} verified fixture_vmas mismatch: "
                    f"requested={requested_vmas} verified={fixture_vmas}"
                )
        return Metric(
            name,
            status,
            count,
            p50,
            p99,
            p999,
            throughput,
            requested_vmas,
            fixture_vmas,
            None,
            None,
        )

    if status == "missing":
        missing_keys = common | throughput_key | fixture_key | {"reason", "errno"}
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
        requested_vmas = None
        fixture_vmas = None
        if name in VMA_FIXTURE_METRICS:
            requested_vmas = parse_positive_int(
                fields["requested_vmas"], "requested_vmas", context
            )
            if fields["fixture_vmas"] != "missing":
                fixture_vmas = parse_nonnegative_int(
                    fields["fixture_vmas"], "fixture_vmas", context
                )
        return Metric(
            name,
            status,
            0,
            None,
            None,
            None,
            None,
            requested_vmas,
            fixture_vmas,
            reason,
            error_number,
        )

    raise EvidenceError(f"{context} has invalid status: {status!r}")


def validate_pin_workers(
    records: list[PinWorker],
    *,
    mode: str,
    metric: Metric,
    expected_workers: int,
    pin_iterations: int,
    vmas: int,
    affinity_cpu_ids: frozenset[int],
) -> None:
    selected = [record for record in records if record.mode == mode]
    context = f"{mode} direct-I/O proxy worker evidence"
    if metric.status == "missing":
        if selected:
            raise EvidenceError(
                f"{context} must be absent when {metric.name} is missing"
            )
        return
    if len(selected) != expected_workers:
        raise EvidenceError(
            f"{context} count mismatch: "
            f"expected={expected_workers} actual={len(selected)}"
        )
    indexes = [record.worker for record in selected]
    if sorted(indexes) != list(range(expected_workers)):
        raise EvidenceError(
            f"{context} worker indexes mismatch: "
            f"expected={list(range(expected_workers))!r} actual={sorted(indexes)!r}"
        )
    cpus = [record.cpu for record in selected]
    if len(set(cpus)) != len(cpus):
        raise EvidenceError(f"{context} contains duplicate CPU witnesses")
    for record in selected:
        if record.cpu not in affinity_cpu_ids:
            raise EvidenceError(
                f"{context} worker={record.worker} CPU {record.cpu} "
                "is outside the affinity witness"
            )
        if record.completed != pin_iterations:
            raise EvidenceError(
                f"{context} worker={record.worker} completed mismatch: "
                f"expected={pin_iterations} actual={record.completed}"
            )
        if (
            record.fixture_before_vmas != vmas
            or record.fixture_after_vmas != vmas
        ):
            raise EvidenceError(
                f"{context} worker={record.worker} fixture mismatch: "
                f"expected={vmas} before={record.fixture_before_vmas} "
                f"after={record.fixture_after_vmas}"
            )


def validate_cross_as_pin_workers(
    records: list[CrossAsPinWorker],
    *,
    metric: Metric,
    expected_workers: int,
    pin_iterations: int,
    vmas: int,
    affinity_cpu_ids: frozenset[int],
) -> None:
    context = "cross-address-space direct-I/O proxy worker evidence"
    if metric.status == "missing":
        if records:
            raise EvidenceError(
                f"{context} must be absent when {metric.name} is missing"
            )
        return
    if len(records) != expected_workers:
        raise EvidenceError(
            f"{context} count mismatch: "
            f"expected={expected_workers} actual={len(records)}"
        )
    indexes = [record.worker for record in records]
    if sorted(indexes) != list(range(expected_workers)):
        raise EvidenceError(
            f"{context} worker indexes mismatch: "
            f"expected={list(range(expected_workers))!r} actual={sorted(indexes)!r}"
        )
    cpus = [record.cpu for record in records]
    if len(set(cpus)) != len(cpus):
        raise EvidenceError(f"{context} contains duplicate CPU witnesses")
    pids = [record.pid for record in records]
    if len(set(pids)) != len(pids):
        raise EvidenceError(f"{context} contains duplicate PID witnesses")
    for record in records:
        if record.cpu not in affinity_cpu_ids:
            raise EvidenceError(
                f"{context} worker={record.worker} CPU {record.cpu} "
                "is outside the affinity witness"
            )
        if record.completed != pin_iterations:
            raise EvidenceError(
                f"{context} worker={record.worker} completed mismatch: "
                f"expected={pin_iterations} actual={record.completed}"
            )
        if (
            record.fixture_before_vmas != vmas
            or record.fixture_after_vmas != vmas
        ):
            raise EvidenceError(
                f"{context} worker={record.worker} fixture mismatch: "
                f"expected={vmas} before={record.fixture_before_vmas} "
                f"after={record.fixture_after_vmas}"
            )
        if record.cow_isolated != 1:
            raise EvidenceError(
                f"{context} worker={record.worker} lacks COW isolation witness"
            )


def validate_mremap_workers(
    records: list[MremapWorker],
    *,
    metric: Metric,
    iterations: int,
    vmas: int,
    page_size: int,
    affinity_cpu_ids: frozenset[int],
) -> None:
    context = "disjoint same-address-space mremap worker evidence"
    if metric.status == "missing":
        if records:
            raise EvidenceError(
                f"{context} must be absent when {metric.name} is missing"
            )
        return
    if len(records) != MREMAP_CONTENTION_WORKERS:
        raise EvidenceError(
            f"{context} count mismatch: "
            f"expected={MREMAP_CONTENTION_WORKERS} actual={len(records)}"
        )
    indexes = sorted(record.worker for record in records)
    if indexes != list(range(MREMAP_CONTENTION_WORKERS)):
        raise EvidenceError(
            f"{context} worker indexes mismatch: "
            f"expected={list(range(MREMAP_CONTENTION_WORKERS))!r} "
            f"actual={indexes!r}"
        )
    if len({record.cpu for record in records}) != MREMAP_CONTENTION_WORKERS:
        raise EvidenceError(f"{context} contains duplicate CPU witnesses")
    expected_bytes = page_size * MREMAP_CONTENTION_SLOT_PAGES

    ranges: list[tuple[int, int, int, str]] = []
    for record in records:
        if record.cpu not in affinity_cpu_ids:
            raise EvidenceError(
                f"{context} worker={record.worker} CPU {record.cpu} "
                "is outside the affinity witness"
            )
        if record.bytes != expected_bytes:
            raise EvidenceError(
                f"{context} worker={record.worker} slot size mismatch: "
                f"expected={expected_bytes} actual={record.bytes}"
            )
        if record.slot_a % page_size != 0 or record.slot_b % page_size != 0:
            raise EvidenceError(
                f"{context} worker={record.worker} has an unaligned slot address"
            )
        if record.completed != iterations:
            raise EvidenceError(
                f"{context} worker={record.worker} completed mismatch: "
                f"expected={iterations} actual={record.completed}"
            )
        if (
            record.fixture_before_vmas != vmas
            or record.fixture_after_vmas != vmas
        ):
            raise EvidenceError(
                f"{context} worker={record.worker} fixture mismatch: "
                f"expected={vmas} before={record.fixture_before_vmas} "
                f"after={record.fixture_after_vmas}"
            )
        if abs(record.slot_a - record.slot_b) <= record.bytes:
            raise EvidenceError(
                f"{context} worker={record.worker} slots lack a separating gap"
            )
        ranges.extend(
            (
                (record.slot_a, record.slot_a + record.bytes, record.worker, "a"),
                (record.slot_b, record.slot_b + record.bytes, record.worker, "b"),
            )
        )
    for index, left in enumerate(ranges):
        for right in ranges[index + 1 :]:
            if left[0] < right[1] and right[0] < left[1]:
                raise EvidenceError(
                    f"{context} slot ranges overlap: "
                    f"worker={left[2]} slot={left[3]} and "
                    f"worker={right[2]} slot={right[3]}"
                )
    if max(record.start_ns for record in records) >= min(
        record.end_ns for record in records
    ):
        raise EvidenceError(f"{context} execution windows do not overlap")


def parse_evidence(
    log: Path,
    arch: str,
    requested_cpus: int,
    *,
    iterations: int,
    vmas: int,
    pin_iterations: int,
    pin_workers: int,
) -> Evidence:
    run_fields: dict[str, str] | None = None
    topology_fields: dict[str, str] | None = None
    affinity_fields: dict[str, str] | None = None
    metrics: dict[str, Metric] = {}
    pin_worker_records: list[PinWorker] = []
    cross_as_pin_worker_records: list[CrossAsPinWorker] = []
    mremap_worker_records: list[MremapWorker] = []
    done_count = 0
    semantics_count = 0

    try:
        lines = log.read_text(encoding="utf-8", errors="replace").splitlines()
    except OSError as error:
        raise EvidenceError(f"cannot read log {log}: {error}") from error

    for line in lines:
        if line.startswith("MM_PERF_RUN "):
            if run_fields is not None:
                raise EvidenceError("duplicate MM_PERF_RUN record")
            run_fields = parse_fields(line, "MM_PERF_RUN ")
        elif line.startswith("MM_PERF_TOPOLOGY "):
            if topology_fields is not None:
                raise EvidenceError("duplicate MM_PERF_TOPOLOGY record")
            topology_fields = parse_fields(line, "MM_PERF_TOPOLOGY ")
        elif line.startswith("MM_PERF_AFFINITY "):
            if affinity_fields is not None:
                raise EvidenceError("duplicate MM_PERF_AFFINITY record")
            affinity_fields = parse_fields(line, "MM_PERF_AFFINITY ")
        elif line.startswith("MM_PERF_PIN_WORKER "):
            pin_worker_records.append(
                parse_pin_worker(parse_fields(line, "MM_PERF_PIN_WORKER "))
            )
        elif line.startswith("MM_PERF_PIN_CROSS_AS_WORKER "):
            cross_as_pin_worker_records.append(
                parse_cross_as_pin_worker(
                    parse_fields(line, "MM_PERF_PIN_CROSS_AS_WORKER ")
                )
            )
        elif line.startswith("MM_PERF_MREMAP_WORKER "):
            mremap_worker_records.append(
                parse_mremap_worker(
                    parse_fields(line, "MM_PERF_MREMAP_WORKER ")
                )
            )
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

    if run_fields is None:
        raise EvidenceError("missing MM_PERF_RUN record")
    page_size = parse_run(
        run_fields,
        arch=arch,
        iterations=iterations,
        vmas=vmas,
        pin_iterations=pin_iterations,
        pin_workers=pin_workers,
    )
    if topology_fields is None:
        raise EvidenceError("missing MM_PERF_TOPOLOGY record")
    online_cpus = parse_topology(topology_fields, requested_cpus)
    if affinity_fields is None:
        raise EvidenceError("missing MM_PERF_AFFINITY record")
    affinity_cpu_ids, affinity_complete = parse_affinity(
        affinity_fields, online_cpus
    )
    if online_cpus <= MAX_AFFINITY_CPU_IDS and not affinity_complete:
        raise EvidenceError("affinity CPU ID witness is unexpectedly incomplete")
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
    expected_counts = {
        "vma_scale": iterations,
        "mremap_latency": iterations * 2,
        "mremap_fixed_replace_latency": iterations,
        MREMAP_CONTENTION_METRIC: iterations * MREMAP_CONTENTION_WORKERS,
        "mremap_file_duplicate_latency": iterations,
        "mremap_shared_anon_resize_latency": iterations * 2,
        "protect_touch_latency": iterations,
        SINGLE_PROXY_METRIC: pin_iterations,
        SAME_AS_PROXY_METRIC: pin_iterations * pin_workers,
        CROSS_AS_PROXY_METRIC: pin_iterations * pin_workers,
    }
    for name in EXPECTED_METRICS:
        metric = metrics[name]
        if metric.status == "ok" and metric.count != expected_counts[name]:
            raise EvidenceError(
                f"{name} count mismatch: "
                f"expected={expected_counts[name]} actual={metric.count}"
            )
    for name in VMA_FIXTURE_METRICS:
        metric = metrics[name]
        if metric.requested_vmas != vmas:
            raise EvidenceError(
                f"{name} requested_vmas mismatch: "
                f"expected={vmas} actual={metric.requested_vmas}"
            )
        if metric.status == "ok" and metric.fixture_vmas != vmas:
            raise EvidenceError(
                f"{name} verified fixture_vmas mismatch: "
                f"requested={vmas} verified={metric.fixture_vmas}"
            )
    validate_pin_workers(
        pin_worker_records,
        mode="single",
        metric=metrics[SINGLE_PROXY_METRIC],
        expected_workers=1,
        pin_iterations=pin_iterations,
        vmas=vmas,
        affinity_cpu_ids=affinity_cpu_ids,
    )
    validate_pin_workers(
        pin_worker_records,
        mode="contention",
        metric=metrics[SAME_AS_PROXY_METRIC],
        expected_workers=pin_workers,
        pin_iterations=pin_iterations,
        vmas=vmas,
        affinity_cpu_ids=affinity_cpu_ids,
    )
    validate_cross_as_pin_workers(
        cross_as_pin_worker_records,
        metric=metrics[CROSS_AS_PROXY_METRIC],
        expected_workers=pin_workers,
        pin_iterations=pin_iterations,
        vmas=vmas,
        affinity_cpu_ids=affinity_cpu_ids,
    )
    validate_mremap_workers(
        mremap_worker_records,
        metric=metrics[MREMAP_CONTENTION_METRIC],
        iterations=iterations,
        vmas=vmas,
        page_size=page_size,
        affinity_cpu_ids=affinity_cpu_ids,
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
            "requested_vmas",
            "fixture_vmas",
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
                (
                    metric.requested_vmas
                    if metric.requested_vmas is not None
                    else "-"
                ),
                (
                    metric.fixture_vmas
                    if metric.fixture_vmas is not None
                    else (
                        "missing"
                        if metric.name in VMA_FIXTURE_METRICS
                        else "-"
                    )
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
                "requested_vmas": metric.requested_vmas,
                "fixture_vmas": metric.fixture_vmas,
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
    parser.add_argument("--iterations", required=True, type=int)
    parser.add_argument("--vmas", required=True, type=int)
    parser.add_argument("--pin-iterations", required=True, type=int)
    parser.add_argument("--pin-workers", required=True, type=int)
    parser.add_argument("--format", choices=("tsv", "json"), default="tsv")
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    if args.cpus <= 0:
        parser.error("--cpus must be positive")
    workload_arguments = (
        args.iterations,
        args.vmas,
        args.pin_iterations,
        args.pin_workers,
    )
    if not all(value > 0 for value in workload_arguments):
        parser.error(
            "--iterations, --vmas, --pin-iterations, and --pin-workers must be positive"
        )

    try:
        evidence = parse_evidence(
            args.log,
            args.arch,
            args.cpus,
            iterations=args.iterations,
            vmas=args.vmas,
            pin_iterations=args.pin_iterations,
            pin_workers=args.pin_workers,
        )
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
