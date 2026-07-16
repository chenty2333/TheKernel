#!/usr/bin/env python3
"""Validate and compare portable TheKernel MM performance evidence bundles."""

from __future__ import annotations

import argparse
import csv
import hashlib
import json
import os
import re
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Any, Iterable

from mm_performance_schema import (
    BUNDLE_SCHEMA,
    EXPECTED_METRICS,
    MANIFEST_COLUMNS,
    METRIC_COLUMNS,
    PIN_METRICS,
    POLICY_SCHEMA,
    REPORT_COLUMNS,
)

HEX40_RE = re.compile(r"^[0-9a-f]{40}$")
HEX64_RE = re.compile(r"^[0-9a-f]{64}$")
FINGERPRINT_RE = re.compile(r"^(?:auto|declared)-sha256:[0-9a-f]{64}$")


class EvidenceError(ValueError):
    """Raised when a bundle, policy, or comparison contract is invalid."""


@dataclass(frozen=True)
class ManifestRecord:
    key: tuple[str, int]
    values: dict[str, str]


@dataclass(frozen=True)
class MetricRecord:
    arch: str
    requested_cpus: int
    online_cpus: int
    metric: str
    count: int
    p50_ns: int
    p99_ns: int
    p999_ns: int
    throughput_bytes_per_sec: int | None

    @property
    def key(self) -> tuple[str, int, str]:
        return (self.arch, self.requested_cpus, self.metric)


@dataclass(frozen=True)
class Bundle:
    root: Path
    manifest: dict[tuple[str, int], ManifestRecord]
    metrics: dict[tuple[str, int, str], MetricRecord]


@dataclass(frozen=True)
class MetricPolicy:
    p99_max_regression_percent: int
    p999_max_regression_percent: int | None
    throughput_min_retained_percent: int | None


@dataclass(frozen=True)
class ReportRow:
    arch: str
    requested_cpus: int
    metric: str
    statistic: str
    mode: str
    baseline: int
    candidate: int
    threshold_percent: int
    comparator: str
    result: str
    candidate_ratio_ppm: str


def parse_nonnegative_int(text: str, field: str, context: str) -> int:
    if not text.isascii() or not text.isdecimal():
        raise EvidenceError(f"{context} has invalid {field}: {text!r}")
    try:
        return int(text, 10)
    except ValueError as error:
        raise EvidenceError(f"{context} has invalid {field}: {text!r}") from error


def parse_positive_int(text: str, field: str, context: str) -> int:
    value = parse_nonnegative_int(text, field, context)
    if value == 0:
        raise EvidenceError(f"{context} requires positive {field}")
    return value


def read_tsv(
    path: Path, expected_columns: tuple[str, ...], context: str
) -> list[dict[str, str]]:
    try:
        with path.open("r", encoding="utf-8", newline="") as source:
            reader = csv.reader(source, delimiter="\t", strict=True)
            try:
                header = tuple(next(reader))
            except StopIteration as error:
                raise EvidenceError(f"{context} is empty: {path}") from error
            if header != expected_columns:
                raise EvidenceError(
                    f"{context} header mismatch: expected={expected_columns!r} "
                    f"actual={header!r}"
                )
            rows: list[dict[str, str]] = []
            for line_number, cells in enumerate(reader, start=2):
                if len(cells) != len(expected_columns):
                    raise EvidenceError(
                        f"{context} row {line_number} has {len(cells)} fields; "
                        f"expected {len(expected_columns)}"
                    )
                rows.append(dict(zip(expected_columns, cells, strict=True)))
    except (OSError, csv.Error) as error:
        raise EvidenceError(f"cannot read {context} {path}: {error}") from error
    if not rows:
        raise EvidenceError(f"{context} has no data rows: {path}")
    return rows


def safe_bundle_file(root: Path, relative: str, field: str) -> Path:
    if not relative or "\\" in relative:
        raise EvidenceError(f"{field} is not a portable POSIX path: {relative!r}")
    pure = PurePosixPath(relative)
    parts = relative.split("/")
    if pure.is_absolute() or any(part in {"", ".", ".."} for part in parts):
        raise EvidenceError(f"{field} must be a normalized relative path: {relative!r}")
    candidate = root.joinpath(*parts)
    try:
        resolved = candidate.resolve(strict=True)
    except (OSError, ValueError) as error:
        raise EvidenceError(f"{field} is missing or inaccessible: {relative!r}: {error}") from error
    try:
        resolved.relative_to(root)
    except ValueError as error:
        raise EvidenceError(f"{field} escapes the evidence bundle: {relative!r}") from error
    if not resolved.is_file():
        raise EvidenceError(f"{field} is not a regular file: {relative!r}")
    return resolved


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    try:
        with path.open("rb") as source:
            while chunk := source.read(1024 * 1024):
                digest.update(chunk)
    except OSError as error:
        raise EvidenceError(f"cannot hash artifact {path}: {error}") from error
    return digest.hexdigest()


def validate_artifact(
    root: Path,
    values: dict[str, str],
    path_field: str,
    sha_field: str,
    size_field: str,
    context: str,
) -> Path:
    path = safe_bundle_file(root, values[path_field], f"{context} {path_field}")
    expected_sha = values[sha_field]
    if not HEX64_RE.fullmatch(expected_sha):
        raise EvidenceError(f"{context} has invalid {sha_field}: {expected_sha!r}")
    expected_size = parse_positive_int(values[size_field], size_field, context)
    actual_size = path.stat().st_size
    if actual_size != expected_size:
        raise EvidenceError(
            f"{context} {path_field} size mismatch: "
            f"expected={expected_size} actual={actual_size}"
        )
    actual_sha = sha256_file(path)
    if actual_sha != expected_sha:
        raise EvidenceError(
            f"{context} {path_field} SHA-256 mismatch: "
            f"expected={expected_sha} actual={actual_sha}"
        )
    return path


def validate_manifest_row(
    root: Path, row: dict[str, str], line_number: int
) -> tuple[ManifestRecord, Path]:
    context = f"manifest row {line_number}"
    if row["bundle_schema"] != BUNDLE_SCHEMA:
        raise EvidenceError(
            f"{context} has unsupported bundle_schema: {row['bundle_schema']!r}; "
            f"expected {BUNDLE_SCHEMA!r}"
        )
    for field in (
        "thekernel_commit",
        "thekernel_ax_commit",
        "thekernel_linux_abi_commit",
    ):
        if not HEX40_RE.fullmatch(row[field]):
            raise EvidenceError(f"{context} has invalid {field}: {row[field]!r}")
    arch = row["arch"]
    if arch not in {"rv", "la"}:
        raise EvidenceError(f"{context} has invalid arch: {arch!r}")
    requested_cpus = parse_positive_int(row["requested_cpus"], "requested_cpus", context)
    online_cpus = parse_positive_int(row["online_cpus"], "online_cpus", context)
    if requested_cpus > 64 or online_cpus != requested_cpus:
        raise EvidenceError(
            f"{context} has invalid CPU topology: "
            f"requested={requested_cpus} online={online_cpus}"
        )
    iterations = parse_positive_int(row["iterations"], "iterations", context)
    live_vmas = parse_positive_int(row["live_vmas"], "live_vmas", context)
    pin_iterations = parse_positive_int(
        row["pin_iterations"], "pin_iterations", context
    )
    pin_workers = parse_positive_int(row["pin_workers"], "pin_workers", context)
    if iterations > 100000 or live_vmas > 16384 or pin_iterations > 10000:
        raise EvidenceError(f"{context} workload exceeds the bundle-v2 limits")
    if pin_workers != requested_cpus:
        raise EvidenceError(
            f"{context} pin worker topology mismatch: "
            f"requested={requested_cpus} workers={pin_workers}"
        )
    for field in (
        "kernel_sha256",
        "rootfs_sha256",
        "qemu_sha256",
        "runner_contract_sha256",
    ):
        if not HEX64_RE.fullmatch(row[field]):
            raise EvidenceError(f"{context} has invalid {field}: {row[field]!r}")
    expected_qemu = {
        "rv": "qemu-system-riscv64",
        "la": "qemu-system-loongarch64",
    }[arch]
    if row["qemu_binary"] != expected_qemu:
        raise EvidenceError(
            f"{context} has invalid qemu_binary for {arch}: {row['qemu_binary']!r}"
        )
    if not row["qemu_version"] or any(
        character in row["qemu_version"] for character in "\r\n"
    ):
        raise EvidenceError(f"{context} has invalid qemu_version")
    if not FINGERPRINT_RE.fullmatch(row["runner_fingerprint"]):
        raise EvidenceError(
            f"{context} has invalid runner_fingerprint: {row['runner_fingerprint']!r}"
        )

    kernel_path = safe_bundle_file(root, row["kernel_artifact"], f"{context} kernel_artifact")
    kernel_size = parse_positive_int(
        row["kernel_size_bytes"], "kernel_size_bytes", context
    )
    if kernel_path.stat().st_size != kernel_size:
        raise EvidenceError(
            f"{context} kernel_artifact size mismatch: "
            f"expected={kernel_size} actual={kernel_path.stat().st_size}"
        )
    kernel_sha = sha256_file(kernel_path)
    if kernel_sha != row["kernel_sha256"]:
        raise EvidenceError(
            f"{context} kernel_artifact SHA-256 mismatch: "
            f"expected={row['kernel_sha256']} actual={kernel_sha}"
        )
    metrics_path = validate_artifact(
        root,
        row,
        "metrics_artifact",
        "metrics_sha256",
        "metrics_size_bytes",
        context,
    )
    commands_path = validate_artifact(
        root,
        row,
        "commands",
        "commands_sha256",
        "commands_size_bytes",
        context,
    )
    validate_artifact(
        root,
        row,
        "qemu_log",
        "qemu_log_sha256",
        "qemu_log_size_bytes",
        context,
    )
    expected_command = (
        "/opt/thekernel-tests/bin/thekernel-mm-performance "
        f"--iterations {row['iterations']} --vmas {row['live_vmas']} "
        f"--pin-iterations {row['pin_iterations']} "
        f"--pin-workers {row['pin_workers']}; exit\n"
    )
    try:
        command_text = commands_path.read_text(encoding="utf-8")
    except OSError as error:
        raise EvidenceError(f"cannot read {context} command artifact: {error}") from error
    if command_text != expected_command:
        raise EvidenceError(f"{context} command artifact does not match its workload fields")
    return ManifestRecord((arch, requested_cpus), row), metrics_path


def expected_metric_count(manifest: ManifestRecord, metric: str) -> int:
    values = manifest.values
    iterations = int(values["iterations"], 10)
    pin_iterations = int(values["pin_iterations"], 10)
    pin_workers = int(values["pin_workers"], 10)
    return {
        "vma_scale": iterations,
        "mremap_latency": iterations * 2,
        "protect_touch_latency": iterations,
        "pin_throughput": pin_iterations,
        "pin_contention": pin_iterations * pin_workers,
    }[metric]


def parse_metric_row(
    row: dict[str, str], context: str, manifest: ManifestRecord
) -> MetricRecord:
    arch = row["arch"]
    requested_cpus = parse_positive_int(row["requested_cpus"], "requested_cpus", context)
    online_cpus = parse_positive_int(row["online_cpus"], "online_cpus", context)
    if (arch, requested_cpus) != manifest.key or online_cpus != int(
        manifest.values["online_cpus"], 10
    ):
        raise EvidenceError(
            f"{context} topology does not match manifest: "
            f"metric={(arch, requested_cpus, online_cpus)!r} manifest={manifest.key!r}"
        )
    metric = row["metric"]
    if metric not in EXPECTED_METRICS:
        raise EvidenceError(f"{context} has unexpected metric: {metric!r}")
    if row["status"] != "ok":
        raise EvidenceError(
            f"{context} metric={metric} is not regression evidence: "
            f"status={row['status']!r}"
        )
    count = parse_positive_int(row["count"], "count", context)
    expected_count = expected_metric_count(manifest, metric)
    if count != expected_count:
        raise EvidenceError(
            f"{context} metric={metric} count mismatch: "
            f"expected={expected_count} actual={count}"
        )
    p50 = parse_nonnegative_int(row["p50_ns"], "p50_ns", context)
    p99 = parse_nonnegative_int(row["p99_ns"], "p99_ns", context)
    p999 = parse_nonnegative_int(row["p999_ns"], "p999_ns", context)
    if not p50 <= p99 <= p999:
        raise EvidenceError(
            f"{context} metric={metric} has non-monotonic quantiles: "
            f"{p50}, {p99}, {p999}"
        )
    throughput: int | None
    if metric in PIN_METRICS:
        throughput = parse_positive_int(
            row["throughput_bytes_per_sec"], "throughput_bytes_per_sec", context
        )
    else:
        if row["throughput_bytes_per_sec"] != "-":
            raise EvidenceError(
                f"{context} metric={metric} must use '-' for throughput"
            )
        throughput = None
    if row["reason"] != "-" or row["errno"] != "-":
        raise EvidenceError(f"{context} metric={metric} ok row has reason or errno")
    return MetricRecord(
        arch,
        requested_cpus,
        online_cpus,
        metric,
        count,
        p50,
        p99,
        p999,
        throughput,
    )


def parse_run_metrics(path: Path, manifest: ManifestRecord) -> dict[str, MetricRecord]:
    rows = read_tsv(path, METRIC_COLUMNS, f"metrics artifact for {manifest.key}")
    metrics: dict[str, MetricRecord] = {}
    for line_number, row in enumerate(rows, start=2):
        metric = parse_metric_row(
            row, f"metrics artifact {manifest.key} row {line_number}", manifest
        )
        if metric.metric in metrics:
            raise EvidenceError(
                f"metrics artifact {manifest.key} has duplicate metric: {metric.metric}"
            )
        metrics[metric.metric] = metric
    if set(metrics) != set(EXPECTED_METRICS):
        missing = sorted(set(EXPECTED_METRICS) - set(metrics))
        extra = sorted(set(metrics) - set(EXPECTED_METRICS))
        raise EvidenceError(
            f"metrics artifact {manifest.key} metric set mismatch: "
            f"missing={missing!r} extra={extra!r}"
        )
    return metrics


def load_bundle(path: Path) -> Bundle:
    try:
        root = path.expanduser().resolve(strict=True)
    except OSError as error:
        raise EvidenceError(f"evidence bundle is missing or inaccessible: {path}: {error}") from error
    if not root.is_dir():
        raise EvidenceError(f"evidence bundle is not a directory: {root}")
    manifest_path = safe_bundle_file(
        root, "mm-performance-manifest.tsv", "bundle manifest"
    )
    rows = read_tsv(manifest_path, MANIFEST_COLUMNS, "bundle manifest")
    manifest: dict[tuple[str, int], ManifestRecord] = {}
    metrics: dict[tuple[str, int, str], MetricRecord] = {}
    for line_number, row in enumerate(rows, start=2):
        record, metrics_path = validate_manifest_row(root, row, line_number)
        if record.key in manifest:
            raise EvidenceError(f"bundle manifest has duplicate run key: {record.key!r}")
        manifest[record.key] = record
        for metric in parse_run_metrics(metrics_path, record).values():
            if metric.key in metrics:
                raise EvidenceError(f"bundle has duplicate metric key: {metric.key!r}")
            metrics[metric.key] = metric

    uniform_fields = (
        "bundle_schema",
        "thekernel_commit",
        "thekernel_ax_commit",
        "thekernel_linux_abi_commit",
        "iterations",
        "live_vmas",
        "pin_iterations",
        "runner_fingerprint",
        "runner_contract_sha256",
    )
    first = next(iter(manifest.values())).values
    for key, record in manifest.items():
        for field in uniform_fields:
            if record.values[field] != first[field]:
                raise EvidenceError(
                    f"bundle manifest field {field} is not uniform at run {key!r}"
                )
    per_arch_fields = (
        "rootfs_sha256",
        "qemu_binary",
        "qemu_version",
        "qemu_sha256",
    )
    arch_reference: dict[str, dict[str, str]] = {}
    for key, record in manifest.items():
        reference = arch_reference.setdefault(key[0], record.values)
        for field in per_arch_fields:
            if record.values[field] != reference[field]:
                raise EvidenceError(
                    f"bundle manifest field {field} is not uniform for arch={key[0]}"
                )

    matrix_path = safe_bundle_file(root, "mm-performance.tsv", "bundle metric matrix")
    matrix_rows = read_tsv(matrix_path, METRIC_COLUMNS, "bundle metric matrix")
    matrix: dict[tuple[str, int, str], MetricRecord] = {}
    for line_number, row in enumerate(matrix_rows, start=2):
        arch = row["arch"]
        try:
            cpus = int(row["requested_cpus"], 10)
        except ValueError as error:
            raise EvidenceError(
                f"bundle metric matrix row {line_number} has invalid requested_cpus"
            ) from error
        run = manifest.get((arch, cpus))
        if run is None:
            raise EvidenceError(
                f"bundle metric matrix row {line_number} has no manifest run: "
                f"{(arch, cpus)!r}"
            )
        metric = parse_metric_row(
            row, f"bundle metric matrix row {line_number}", run
        )
        if metric.key in matrix:
            raise EvidenceError(
                f"bundle metric matrix has duplicate metric key: {metric.key!r}"
            )
        matrix[metric.key] = metric
    if matrix != metrics:
        missing = sorted(set(metrics) - set(matrix))
        extra = sorted(set(matrix) - set(metrics))
        changed = sorted(key for key in set(matrix) & set(metrics) if matrix[key] != metrics[key])
        raise EvidenceError(
            "bundle metric matrix does not match per-run artifacts: "
            f"missing={missing!r} extra={extra!r} changed={changed!r}"
        )
    return Bundle(root, manifest, metrics)


def policy_percent(value: Any, field: str, metric: str, *, nullable: bool) -> int | None:
    if value is None and nullable:
        return None
    if isinstance(value, bool) or not isinstance(value, int):
        raise EvidenceError(f"policy metric={metric} has invalid {field}: {value!r}")
    if value < 0 or value > 10000:
        raise EvidenceError(
            f"policy metric={metric} requires {field} in the range 0..10000"
        )
    return value


def load_policy(path: Path) -> dict[str, MetricPolicy]:
    try:
        with path.open("r", encoding="utf-8") as source:
            payload = json.load(source)
    except (OSError, json.JSONDecodeError) as error:
        raise EvidenceError(f"cannot read regression policy {path}: {error}") from error
    if not isinstance(payload, dict) or set(payload) != {"schema", "metrics"}:
        raise EvidenceError("regression policy must contain exactly schema and metrics")
    if payload["schema"] != POLICY_SCHEMA:
        raise EvidenceError(
            f"unsupported regression policy schema: {payload['schema']!r}"
        )
    metrics = payload["metrics"]
    if not isinstance(metrics, dict) or set(metrics) != set(EXPECTED_METRICS):
        raise EvidenceError(
            "regression policy metric set mismatch: "
            f"expected={sorted(EXPECTED_METRICS)!r} "
            f"actual={sorted(metrics) if isinstance(metrics, dict) else metrics!r}"
        )
    result: dict[str, MetricPolicy] = {}
    expected_fields = {
        "p99_max_regression_percent",
        "p999_max_regression_percent",
        "throughput_min_retained_percent",
    }
    for metric in EXPECTED_METRICS:
        entry = metrics[metric]
        if not isinstance(entry, dict) or set(entry) != expected_fields:
            raise EvidenceError(
                f"policy metric={metric} must contain exactly {sorted(expected_fields)!r}"
            )
        p99 = policy_percent(
            entry["p99_max_regression_percent"],
            "p99_max_regression_percent",
            metric,
            nullable=False,
        )
        p999 = policy_percent(
            entry["p999_max_regression_percent"],
            "p999_max_regression_percent",
            metric,
            nullable=True,
        )
        throughput = policy_percent(
            entry["throughput_min_retained_percent"],
            "throughput_min_retained_percent",
            metric,
            nullable=True,
        )
        if metric in PIN_METRICS and throughput is None:
            raise EvidenceError(
                f"policy metric={metric} requires throughput_min_retained_percent"
            )
        if metric in PIN_METRICS and throughput == 0:
            raise EvidenceError(
                f"policy metric={metric} requires positive throughput retention"
            )
        if metric not in PIN_METRICS and throughput is not None:
            raise EvidenceError(
                f"policy metric={metric} must not gate unavailable throughput"
            )
        assert p99 is not None
        result[metric] = MetricPolicy(p99, p999, throughput)
    return result


def compare_provenance(baseline: Bundle, candidate: Bundle) -> None:
    baseline_keys = set(baseline.manifest)
    candidate_keys = set(candidate.manifest)
    if baseline_keys != candidate_keys:
        raise EvidenceError(
            "bundle run-key set mismatch: "
            f"baseline_only={sorted(baseline_keys - candidate_keys)!r} "
            f"candidate_only={sorted(candidate_keys - baseline_keys)!r}"
        )
    comparable_fields = (
        "bundle_schema",
        "thekernel_ax_commit",
        "thekernel_linux_abi_commit",
        "arch",
        "requested_cpus",
        "online_cpus",
        "iterations",
        "live_vmas",
        "pin_iterations",
        "pin_workers",
        "rootfs_sha256",
        "qemu_binary",
        "qemu_version",
        "qemu_sha256",
        "runner_fingerprint",
        "runner_contract_sha256",
        "commands_sha256",
    )
    for key in sorted(baseline_keys):
        before = baseline.manifest[key].values
        after = candidate.manifest[key].values
        for field in comparable_fields:
            if before[field] != after[field]:
                raise EvidenceError(
                    f"run {key!r} is not comparable: {field} differs: "
                    f"baseline={before[field]!r} candidate={after[field]!r}"
                )
    if set(baseline.metrics) != set(candidate.metrics):
        raise EvidenceError("bundle metric-key set mismatch")
    for key in sorted(baseline.metrics):
        before = baseline.metrics[key]
        after = candidate.metrics[key]
        if before.online_cpus != after.online_cpus or before.count != after.count:
            raise EvidenceError(
                f"metric {key!r} topology or sample count differs: "
                f"baseline={(before.online_cpus, before.count)!r} "
                f"candidate={(after.online_cpus, after.count)!r}"
            )


def ratio_ppm(baseline: int, candidate: int) -> str:
    if baseline == 0:
        return "-"
    return str(candidate * 1_000_000 // baseline)


def latency_row(
    metric: MetricRecord,
    baseline_value: int,
    candidate_value: int,
    statistic: str,
    max_regression_percent: int | None,
) -> ReportRow:
    if max_regression_percent is None:
        return ReportRow(
            metric.arch,
            metric.requested_cpus,
            metric.metric,
            statistic,
            "report_only",
            baseline_value,
            candidate_value,
            0,
            "-",
            "REPORT_ONLY",
            ratio_ppm(baseline_value, candidate_value),
        )
    threshold_percent = 100 + max_regression_percent
    passed = candidate_value * 100 <= baseline_value * threshold_percent
    return ReportRow(
        metric.arch,
        metric.requested_cpus,
        metric.metric,
        statistic,
        "gate",
        baseline_value,
        candidate_value,
        threshold_percent,
        "<=",
        "PASS" if passed else "FAIL",
        ratio_ppm(baseline_value, candidate_value),
    )


def throughput_row(
    metric: MetricRecord,
    baseline_value: int,
    candidate_value: int,
    retained_percent: int,
) -> ReportRow:
    passed = candidate_value * 100 >= baseline_value * retained_percent
    return ReportRow(
        metric.arch,
        metric.requested_cpus,
        metric.metric,
        "throughput_bytes_per_sec",
        "gate",
        baseline_value,
        candidate_value,
        retained_percent,
        ">=",
        "PASS" if passed else "FAIL",
        ratio_ppm(baseline_value, candidate_value),
    )


def compare_bundles(
    baseline: Bundle,
    candidate: Bundle,
    policy: dict[str, MetricPolicy],
) -> list[ReportRow]:
    compare_provenance(baseline, candidate)
    rows: list[ReportRow] = []
    arch_order = {"rv": 0, "la": 1}
    metric_order = {metric: index for index, metric in enumerate(EXPECTED_METRICS)}
    keys = sorted(
        baseline.metrics,
        key=lambda key: (arch_order[key[0]], key[1], metric_order[key[2]]),
    )
    for key in keys:
        before = baseline.metrics[key]
        after = candidate.metrics[key]
        metric_policy = policy[before.metric]
        rows.append(
            latency_row(
                before,
                before.p99_ns,
                after.p99_ns,
                "p99_ns",
                metric_policy.p99_max_regression_percent,
            )
        )
        rows.append(
            latency_row(
                before,
                before.p999_ns,
                after.p999_ns,
                "p999_ns",
                metric_policy.p999_max_regression_percent,
            )
        )
        if before.metric in PIN_METRICS:
            assert before.throughput_bytes_per_sec is not None
            assert after.throughput_bytes_per_sec is not None
            assert metric_policy.throughput_min_retained_percent is not None
            rows.append(
                throughput_row(
                    before,
                    before.throughput_bytes_per_sec,
                    after.throughput_bytes_per_sec,
                    metric_policy.throughput_min_retained_percent,
                )
            )
    return rows


def write_report(path: Path, rows: Iterable[ReportRow]) -> None:
    destination = path.expanduser().resolve()
    parent = destination.parent
    if not parent.is_dir():
        raise EvidenceError(f"regression report parent is not a directory: {parent}")
    temporary: Path | None = None
    try:
        with tempfile.NamedTemporaryFile(
            "w", encoding="utf-8", newline="", dir=parent, delete=False
        ) as output:
            temporary = Path(output.name)
            writer = csv.writer(output, delimiter="\t", lineterminator="\n")
            writer.writerow(REPORT_COLUMNS)
            for row in rows:
                writer.writerow(
                    (
                        row.arch,
                        row.requested_cpus,
                        row.metric,
                        row.statistic,
                        row.mode,
                        row.baseline,
                        row.candidate,
                        row.threshold_percent if row.mode == "gate" else "-",
                        row.comparator,
                        row.result,
                        row.candidate_ratio_ppm,
                    )
                )
        os.replace(temporary, destination)
    except OSError as error:
        try:
            if temporary is not None:
                temporary.unlink(missing_ok=True)
        except OSError:
            pass
        raise EvidenceError(f"cannot write regression report {path}: {error}") from error


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="compare two portable TheKernel MM performance evidence bundles"
    )
    parser.add_argument("--baseline", type=Path, required=True)
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--policy", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    return parser


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        baseline = load_bundle(args.baseline)
        candidate = load_bundle(args.candidate)
        policy = load_policy(args.policy)
        rows = compare_bundles(baseline, candidate, policy)
        write_report(args.output, rows)
    except EvidenceError as error:
        try:
            args.output.expanduser().unlink(missing_ok=True)
        except OSError:
            pass
        print(f"compare-mm-performance: INVALID: {error}", file=sys.stderr)
        return 2
    failures = sum(row.result == "FAIL" for row in rows)
    report_only = sum(row.result == "REPORT_ONLY" for row in rows)
    print(
        "compare-mm-performance: "
        f"{'REGRESSION' if failures else 'PASS'} "
        f"gates={len(rows) - report_only} failures={failures} "
        f"report_only={report_only} report={args.output}"
    )
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
