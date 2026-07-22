#!/usr/bin/env python3
"""Validate and normalize opt-in MM lock diagnostic records."""

from __future__ import annotations

import argparse
import csv
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import TextIO


SCHEMA = "thekernel-mm-lock-diagnostics-v1"
HISTOGRAM = "log2_ns_v1"
BUCKET_COUNT = 64
U64_MAX = (1 << 64) - 1
EXPECTED_STAGES = (
    "user_pin_admission",
    "user_pin_expectation",
    "user_pin_collect_owners",
    "user_pin_revalidate",
    "user_pin_commit",
    "user_pin_release",
    "mremap_optimistic_plan",
    "mremap_optimistic_commit",
    "mremap_serialized",
    "phys_pin_registry_shard",
    "phys_pin_publish_shard",
    "phys_pin_release_shard",
    "phys_pin_dealloc_probe_shard",
)
REQUIRED_EXERCISED_STAGES = (
    "user_pin_admission",
    "user_pin_expectation",
    "user_pin_collect_owners",
    "user_pin_revalidate",
    "user_pin_commit",
    "user_pin_release",
    "phys_pin_publish_shard",
    "phys_pin_release_shard",
)
MREMAP_STAGES = (
    "mremap_optimistic_plan",
    "mremap_optimistic_commit",
    "mremap_serialized",
)


class EvidenceError(ValueError):
    """Raised when a diagnostic log violates the evidence contract."""


@dataclass(frozen=True)
class StageRecord:
    stage: str
    epoch: int
    samples: int
    wait_sum_ns: int
    wait_max_ns: int
    hold_sum_ns: int
    hold_max_ns: int
    wait_buckets: tuple[int, ...]
    hold_buckets: tuple[int, ...]


def parse_fields(line: str, prefix: str) -> dict[str, str]:
    payload = line.removeprefix(prefix)
    if not payload or payload.startswith(" "):
        raise EvidenceError(f"malformed {prefix.strip()} record")
    fields: dict[str, str] = {}
    for token in payload.split(" "):
        if not token or "=" not in token:
            raise EvidenceError(f"malformed diagnostic field: {token!r}")
        key, value = token.split("=", 1)
        if not key or not value or key in fields:
            raise EvidenceError(f"invalid or duplicate diagnostic field: {key!r}")
        fields[key] = value
    return fields


def require_fields(
    fields: dict[str, str], expected: frozenset[str], context: str
) -> None:
    actual = frozenset(fields)
    if actual != expected:
        missing = sorted(expected - actual)
        unknown = sorted(actual - expected)
        raise EvidenceError(
            f"{context} field mismatch: missing={missing!r} unknown={unknown!r}"
        )


def parse_uint(value: str, field: str, context: str) -> int:
    if not value.isascii() or not value.isdecimal():
        raise EvidenceError(f"{context} has invalid {field}: {value!r}")
    parsed = int(value, 10)
    if parsed > U64_MAX:
        raise EvidenceError(f"{context} has out-of-range {field}: {value!r}")
    return parsed


def parse_histogram(value: str, field: str, context: str) -> tuple[int, ...]:
    cells = value.split(",")
    if len(cells) != BUCKET_COUNT:
        raise EvidenceError(
            f"{context} {field} has {len(cells)} buckets; expected {BUCKET_COUNT}"
        )
    return tuple(parse_uint(cell, field, context) for cell in cells)


def bucket_lower_ns(index: int) -> int:
    if index == 0:
        return 0
    return 1 << (index - 1)


def bucket_upper_ns(index: int) -> int:
    if index == 0:
        return 0
    if index == BUCKET_COUNT - 1:
        return U64_MAX
    return (1 << index) - 1


def validate_timing_tuple(
    total: int,
    maximum: int,
    buckets: tuple[int, ...],
    field: str,
    context: str,
) -> None:
    populated = [index for index, count in enumerate(buckets) if count]
    if not populated:
        raise EvidenceError(f"{context} nonempty stage has empty {field}_buckets")

    highest = populated[-1]
    highest_lower = bucket_lower_ns(highest)
    highest_upper = bucket_upper_ns(highest)
    if maximum < highest_lower or maximum > highest_upper:
        raise EvidenceError(
            f"{context} {field}_max_ns is incompatible with highest populated "
            f"{field}_buckets bucket: bucket={highest} "
            f"range={highest_lower}..{highest_upper} actual={maximum}"
        )

    # One sample in the highest bucket must equal maximum. The remaining
    # samples in that bucket cannot exceed it, which makes these bounds tighter
    # than the bucket ranges alone while remaining exact for integer nanoseconds.
    minimum_total = sum(
        count * bucket_lower_ns(index) for index, count in enumerate(buckets)
    ) + (maximum - highest_lower)
    maximum_total = sum(
        count * bucket_upper_ns(index)
        for index, count in enumerate(buckets[:highest])
    ) + buckets[highest] * maximum

    # The producer marks the stage saturated when a cumulative u64 addition
    # overflows. A saturated=0 record therefore cannot represent a histogram
    # whose minimum possible total is already outside u64.
    if minimum_total > U64_MAX:
        raise EvidenceError(
            f"{context} {field}_buckets require a saturated cumulative total"
        )
    maximum_total = min(maximum_total, U64_MAX)
    if total < minimum_total or total > maximum_total:
        raise EvidenceError(
            f"{context} {field}_sum_ns is outside histogram-derived bounds: "
            f"minimum={minimum_total} maximum={maximum_total} actual={total}"
        )


def parse_stage(fields: dict[str, str], header_epoch: int) -> StageRecord:
    expected = frozenset(
        {
            "stage",
            "epoch",
            "samples",
            "wait_sum_ns",
            "wait_max_ns",
            "hold_sum_ns",
            "hold_max_ns",
            "saturated",
            "wait_buckets",
            "hold_buckets",
        }
    )
    stage = fields.get("stage", "<missing>")
    context = f"MM_LOCK_STAGE stage={stage}"
    require_fields(fields, expected, context)
    if stage not in EXPECTED_STAGES:
        raise EvidenceError(f"unexpected MM lock stage: {stage!r}")
    epoch = parse_uint(fields["epoch"], "epoch", context)
    if epoch != header_epoch:
        raise EvidenceError(
            f"{context} epoch mismatch: header={header_epoch} stage={epoch}"
        )
    if fields["saturated"] != "0":
        raise EvidenceError(f"{context} is saturated")
    samples = parse_uint(fields["samples"], "samples", context)
    wait_sum = parse_uint(fields["wait_sum_ns"], "wait_sum_ns", context)
    wait_max = parse_uint(fields["wait_max_ns"], "wait_max_ns", context)
    hold_sum = parse_uint(fields["hold_sum_ns"], "hold_sum_ns", context)
    hold_max = parse_uint(fields["hold_max_ns"], "hold_max_ns", context)
    wait_buckets = parse_histogram(fields["wait_buckets"], "wait_buckets", context)
    hold_buckets = parse_histogram(fields["hold_buckets"], "hold_buckets", context)
    if sum(wait_buckets) != samples or sum(hold_buckets) != samples:
        raise EvidenceError(f"{context} histogram/sample count mismatch")
    if samples == 0:
        if any((wait_sum, wait_max, hold_sum, hold_max)):
            raise EvidenceError(f"{context} empty stage has nonzero timing")
    else:
        if wait_max > wait_sum or hold_max > hold_sum:
            raise EvidenceError(f"{context} maximum exceeds cumulative timing")
        validate_timing_tuple(wait_sum, wait_max, wait_buckets, "wait", context)
        validate_timing_tuple(hold_sum, hold_max, hold_buckets, "hold", context)
    return StageRecord(
        stage,
        epoch,
        samples,
        wait_sum,
        wait_max,
        hold_sum,
        hold_max,
        wait_buckets,
        hold_buckets,
    )


def parse_log(path: Path) -> tuple[int, tuple[StageRecord, ...]]:
    try:
        lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
    except OSError as error:
        raise EvidenceError(f"cannot read diagnostic log {path}: {error}") from error

    header: dict[str, str] | None = None
    end: dict[str, str] | None = None
    stages: dict[str, StageRecord] = {}
    for line in lines:
        if line.startswith("MM_LOCK_DIAGNOSTICS_END "):
            if header is None:
                raise EvidenceError("MM_LOCK_DIAGNOSTICS_END precedes diagnostics header")
            if end is not None:
                raise EvidenceError("duplicate MM_LOCK_DIAGNOSTICS_END record")
            end = parse_fields(line, "MM_LOCK_DIAGNOSTICS_END ")
        elif line.startswith("MM_LOCK_DIAGNOSTICS "):
            if header is not None:
                raise EvidenceError("duplicate MM_LOCK_DIAGNOSTICS record")
            header = parse_fields(line, "MM_LOCK_DIAGNOSTICS ")
        elif line.startswith("MM_LOCK_STAGE "):
            if header is None:
                raise EvidenceError("MM_LOCK_STAGE precedes diagnostics header")
            if end is not None:
                raise EvidenceError("MM_LOCK_STAGE follows diagnostics end record")
            epoch = parse_uint(header.get("epoch", ""), "epoch", "diagnostics header")
            record = parse_stage(parse_fields(line, "MM_LOCK_STAGE "), epoch)
            if record.stage in stages:
                raise EvidenceError(f"duplicate MM lock stage: {record.stage}")
            stages[record.stage] = record

    if header is None:
        raise EvidenceError("missing MM_LOCK_DIAGNOSTICS record")
    require_fields(
        header,
        frozenset(
            {
                "schema",
                "enabled",
                "resetting",
                "active_samples",
                "epoch",
                "sequence",
                "sequence_exhausted",
                "histogram",
            }
        ),
        "MM_LOCK_DIAGNOSTICS",
    )
    if end is None:
        raise EvidenceError("missing MM_LOCK_DIAGNOSTICS_END record")
    control_fields = frozenset(
        {
            "enabled",
            "resetting",
            "active_samples",
            "epoch",
            "sequence",
            "sequence_exhausted",
        }
    )
    require_fields(end, control_fields, "MM_LOCK_DIAGNOSTICS_END")
    if header["schema"] != SCHEMA:
        raise EvidenceError(f"unsupported MM lock schema: {header['schema']!r}")
    if header["histogram"] != HISTOGRAM:
        raise EvidenceError(f"unsupported MM lock histogram: {header['histogram']!r}")
    if header["enabled"] != "0":
        raise EvidenceError("MM lock snapshot must be captured after collection is disabled")
    if header["resetting"] != "0":
        raise EvidenceError("MM lock snapshot was captured during reset")
    if header["sequence_exhausted"] != "0":
        raise EvidenceError("MM lock diagnostic sequence is exhausted")
    if parse_uint(header["active_samples"], "active_samples", "MM_LOCK_DIAGNOSTICS") != 0:
        raise EvidenceError("MM lock snapshot has active samples")
    epoch = parse_uint(header["epoch"], "epoch", "MM_LOCK_DIAGNOSTICS")
    sequence = parse_uint(header["sequence"], "sequence", "MM_LOCK_DIAGNOSTICS")
    if sequence == U64_MAX:
        raise EvidenceError("MM lock diagnostic sequence is exhausted")
    for field in control_fields:
        if end[field] != header[field]:
            raise EvidenceError(
                f"MM lock snapshot control state changed during read: field={field} "
                f"header={header[field]!r} end={end[field]!r}"
            )
    missing = [stage for stage in EXPECTED_STAGES if stage not in stages]
    if missing:
        raise EvidenceError(f"missing MM lock stages: {missing!r}")
    unexercised = [
        stage for stage in REQUIRED_EXERCISED_STAGES if stages[stage].samples == 0
    ]
    if unexercised:
        raise EvidenceError(
            f"required MM lock stages have no samples: {unexercised!r}"
        )
    if sum(stages[stage].samples for stage in MREMAP_STAGES) == 0:
        raise EvidenceError("mremap MM lock stages have no samples")
    return epoch, tuple(stages[stage] for stage in EXPECTED_STAGES)


def percentile_upper_ns(buckets: tuple[int, ...], thousandths: int) -> int:
    samples = sum(buckets)
    if samples == 0:
        return 0
    rank = max(1, (samples * thousandths + 999) // 1000)
    cumulative = 0
    for index, count in enumerate(buckets):
        cumulative += count
        if cumulative >= rank:
            return bucket_upper_ns(index)
    raise AssertionError("validated histogram lost its samples")


def write_tsv(epoch: int, records: tuple[StageRecord, ...], output: TextIO) -> None:
    writer = csv.writer(output, delimiter="\t", lineterminator="\n")
    writer.writerow(
        (
            "schema",
            "epoch",
            "stage",
            "samples",
            "wait_sum_ns",
            "wait_avg_ns",
            "wait_p50_upper_ns",
            "wait_p99_upper_ns",
            "wait_p999_upper_ns",
            "wait_max_ns",
            "hold_sum_ns",
            "hold_avg_ns",
            "hold_p50_upper_ns",
            "hold_p99_upper_ns",
            "hold_p999_upper_ns",
            "hold_max_ns",
            "wait_buckets",
            "hold_buckets",
        )
    )
    for record in records:
        divisor = record.samples or 1
        writer.writerow(
            (
                SCHEMA,
                epoch,
                record.stage,
                record.samples,
                record.wait_sum_ns,
                record.wait_sum_ns // divisor,
                percentile_upper_ns(record.wait_buckets, 500),
                percentile_upper_ns(record.wait_buckets, 990),
                percentile_upper_ns(record.wait_buckets, 999),
                record.wait_max_ns,
                record.hold_sum_ns,
                record.hold_sum_ns // divisor,
                percentile_upper_ns(record.hold_buckets, 500),
                percentile_upper_ns(record.hold_buckets, 990),
                percentile_upper_ns(record.hold_buckets, 999),
                record.hold_max_ns,
                ",".join(str(value) for value in record.wait_buckets),
                ",".join(str(value) for value in record.hold_buckets),
            )
        )


def main() -> int:
    parser = argparse.ArgumentParser(
        description="validate TheKernel MM lock diagnostics"
    )
    parser.add_argument("log", type=Path)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    try:
        epoch, records = parse_log(args.log)
        if args.output is None:
            write_tsv(epoch, records, sys.stdout)
        else:
            args.output.parent.mkdir(parents=True, exist_ok=True)
            with args.output.open("w", encoding="utf-8", newline="") as output:
                write_tsv(epoch, records, output)
    except EvidenceError as error:
        parser.exit(2, f"error: {error}\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
