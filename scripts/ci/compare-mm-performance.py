#!/usr/bin/env python3
"""Validate and compare explicit MM performance receipts."""

from __future__ import annotations

import argparse
import csv
import datetime as dt
import hashlib
import io
import json
import os
import re
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from functools import cmp_to_key
from pathlib import Path, PurePosixPath
from typing import Any, Iterable

from mm_performance_host import CpuSelectionError, parse_cpu_list
from mm_performance_schema import (
    EXPECTED_METRICS, HOST_DIAGNOSTIC_SCHEMA, MANIFEST_COLUMNS,
    MEASUREMENT_MODES, METRIC_COLUMNS, PIN_METRICS, POLICY_SCHEMA,
    REPORT_COLUMNS, STABILITY_POLICY_SCHEMA, VMA_FIXTURE_METRICS,
)
from source_combination import SourceCombinationError, load as load_source_combination

HEX64_RE = re.compile(r"^[0-9a-f]{64}$")
GIT_OBJECT_RE = re.compile(r"^[0-9a-f]{40}$")
CPU_CLASS_RE = re.compile(r"^package:[0-9]+,max_freq_khz:[1-9][0-9]*$")
RFC3339_UTC_RE = re.compile(
    r"^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}"
    r"(?:\.[0-9]{1,6})?(?:Z|\+00:00)$"
)
HOST_DIAGNOSTIC_MAX_BYTES = 64 * 1024
MAX_PAIR_RATIO_SPREAD_PERCENT = 20
REQUIRED_RELEASE_RUN_KEYS = frozenset({("x86_64", 4), ("x86_64", 8)})
PARSER = Path(__file__).with_name("parse-mm-performance.py")


class EvidenceError(ValueError):
    """Raised when an explicit receipt or comparison contract is invalid."""


@dataclass(frozen=True)
class HostSnapshot:
    timestamp: dt.datetime
    cpu_set: str
    selection: str
    cpu_class: str


@dataclass(frozen=True)
class Run:
    key: tuple[str, int]
    mode: str
    metrics_path: Path
    receipt_sha256: str
    identity: tuple[Any, ...]
    kernel_identity: tuple[int, str]
    host: HostSnapshot
    capture_start: dt.datetime
    capture_end: dt.datetime


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
    requested_vmas: int | None
    fixture_vmas: int | None

    @property
    def key(self) -> tuple[str, int, str]:
        return (self.arch, self.requested_cpus, self.metric)


@dataclass(frozen=True)
class Bundle:
    root: Path
    runs: dict[tuple[str, int], Run]
    metrics: dict[tuple[str, int, str], MetricRecord]
    capture_start: dt.datetime
    capture_end: dt.datetime


@dataclass(frozen=True)
class MetricPolicy:
    p99_max_regression_percent: int
    throughput_min_retained_percent: int | None


@dataclass(frozen=True)
class StabilityPolicy:
    minimum_pairs: int
    maximum_pairs: int
    maximum_pair_ratio_spread_percent: int


@dataclass(frozen=True)
class RatioSample:
    pair: int
    baseline: int
    candidate: int


@dataclass(frozen=True)
class ReportRow:
    arch: str
    requested_cpus: int
    metric: str
    statistic: str
    mode: str
    pair_count: int
    median_pair: int
    baseline: int
    candidate: int
    threshold_percent: int
    comparator: str
    result: str
    candidate_ratio_ppm: str
    pair_ratio_min_ppm: str
    pair_ratio_max_ppm: str


def parse_nonnegative_int(value: str, field: str, context: str) -> int:
    if not value.isascii() or not value.isdecimal():
        raise EvidenceError(f"{context} has invalid {field}: {value!r}")
    return int(value, 10)


def parse_positive_int(value: str, field: str, context: str) -> int:
    result = parse_nonnegative_int(value, field, context)
    if result == 0:
        raise EvidenceError(f"{context} requires positive {field}")
    return result


def read_tsv(path: Path, columns: tuple[str, ...], context: str) -> list[dict[str, str]]:
    try:
        with path.open("r", encoding="utf-8", newline="") as source:
            reader = csv.reader(source, delimiter="\t", strict=True)
            header = tuple(next(reader))
            if header != columns:
                raise EvidenceError(
                    f"{context} header mismatch: expected={columns!r} actual={header!r}"
                )
            rows = []
            for line, cells in enumerate(reader, start=2):
                if len(cells) != len(columns):
                    raise EvidenceError(f"{context} row {line} has wrong field count")
                rows.append(dict(zip(columns, cells, strict=True)))
    except StopIteration as error:
        raise EvidenceError(f"{context} is empty") from error
    except (OSError, csv.Error) as error:
        raise EvidenceError(f"cannot read {context} {path}: {error}") from error
    if not rows:
        raise EvidenceError(f"{context} has no data rows")
    return rows


def read_tsv_bytes(payload: bytes, columns: tuple[str, ...], context: str) -> list[dict[str, str]]:
    try:
        source = io.StringIO(payload.decode("utf-8"), newline="")
        reader = csv.reader(source, delimiter="\t", strict=True)
        header = tuple(next(reader))
        if header != columns:
            raise EvidenceError(
                f"{context} header mismatch: expected={columns!r} actual={header!r}"
            )
        rows = []
        for line, cells in enumerate(reader, start=2):
            if len(cells) != len(columns):
                raise EvidenceError(f"{context} row {line} has wrong field count")
            rows.append(dict(zip(columns, cells, strict=True)))
        return rows
    except (UnicodeDecodeError, StopIteration, csv.Error) as error:
        raise EvidenceError(f"cannot read {context}: {error}") from error


def safe_file(root: Path, relative: str, field: str) -> Path:
    if not relative or "\\" in relative:
        raise EvidenceError(f"{field} is not a portable POSIX path: {relative!r}")
    pure, parts = PurePosixPath(relative), relative.split("/")
    if pure.is_absolute() or any(part in {"", ".", ".."} for part in parts):
        raise EvidenceError(f"{field} must be a normalized relative path: {relative!r}")
    try:
        result = root.joinpath(*parts).resolve(strict=True)
        result.relative_to(root)
    except (OSError, ValueError) as error:
        raise EvidenceError(f"{field} is missing, inaccessible, or escapes the bundle") from error
    if not result.is_file():
        raise EvidenceError(f"{field} is not a regular file: {relative!r}")
    return result


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    try:
        with path.open("rb") as source:
            while chunk := source.read(1024 * 1024):
                digest.update(chunk)
    except OSError as error:
        raise EvidenceError(f"cannot hash {path}: {error}") from error
    return digest.hexdigest()


def parse_rfc3339_utc(value: str, context: str) -> dt.datetime:
    if not RFC3339_UTC_RE.fullmatch(value):
        raise EvidenceError(f"{context} is not strict RFC3339 UTC: {value!r}")
    try:
        return dt.datetime.fromisoformat(
            value[:-1] + "+00:00" if value.endswith("Z") else value
        )
    except ValueError as error:
        raise EvidenceError(f"{context} has an invalid timestamp") from error


def validate_host_snapshot(path: Path, phase: str, cpus: int, context: str) -> HostSnapshot:
    if path.stat().st_size > HOST_DIAGNOSTIC_MAX_BYTES:
        raise EvidenceError(f"{context} host {phase} snapshot exceeds size limit")
    values: dict[str, str] = {}
    for line, row in enumerate(read_tsv(path, ("key", "value"), f"{context} host {phase}"), start=2):
        key, value = row["key"], row["value"]
        if key in values or not key or not value or any(char in value for char in "\t\r\n"):
            raise EvidenceError(f"{context} host {phase} has invalid row {line}")
        values[key] = value
    required = {
        "schema", "phase", "timestamp_utc", "selected_cpu_set",
        "host_cpu_selection", "host_cpu_class", "online_cpu_set", "loadavg",
    }
    if missing := sorted(required - values.keys()):
        raise EvidenceError(f"{context} host {phase} is missing keys: {missing!r}")
    if values["schema"] != HOST_DIAGNOSTIC_SCHEMA or values["phase"] != phase:
        raise EvidenceError(f"{context} host {phase} has invalid identity")
    try:
        selected = parse_cpu_list(values["selected_cpu_set"])
    except CpuSelectionError as error:
        raise EvidenceError(f"{context} host {phase} has invalid CPU set") from error
    if len(selected) != cpus or not CPU_CLASS_RE.fullmatch(values["host_cpu_class"]):
        raise EvidenceError(f"{context} host {phase} does not match guest CPU topology")
    if values["host_cpu_selection"] not in {"auto-homogeneous-v1", "explicit-homogeneous-v1"}:
        raise EvidenceError(f"{context} host {phase} has invalid selection")
    fields = dict(item.split(":", 1) for item in values["host_cpu_class"].split(","))
    for cpu in selected:
        expected = {
            f"cpu.{cpu}.online": "1",
            f"cpu.{cpu}.package": fields["package"],
            f"cpu.{cpu}.max_freq_khz": fields["max_freq_khz"],
        }
        if any(values.get(key) != value for key, value in expected.items()) or f"cpu.{cpu}.current_freq_khz" not in values:
            raise EvidenceError(f"{context} host {phase} lacks valid CPU witness for {cpu}")
    if not any(key == "psi.cpu" or key.startswith("psi.cpu.") for key in values) or not any(key.startswith("cgroup.cpu_stat") for key in values):
        raise EvidenceError(f"{context} host {phase} lacks pressure or cgroup evidence")
    return HostSnapshot(
        parse_rfc3339_utc(values["timestamp_utc"], f"{context} host {phase}"),
        values["selected_cpu_set"], values["host_cpu_selection"], values["host_cpu_class"],
    )


def receipt_file_identity(value: Any, field: str, context: str) -> tuple[int, str]:
    if not isinstance(value, dict) or set(value) - {"path", "size_bytes", "sha256", "requested"}:
        raise EvidenceError(f"{context} receipt {field} has invalid file evidence")
    path, size, digest = value.get("path"), value.get("size_bytes"), value.get("sha256")
    if not isinstance(path, str) or not path or type(size) is not int or size <= 0 or not isinstance(digest, str) or not HEX64_RE.fullmatch(digest):
        raise EvidenceError(f"{context} receipt {field} has invalid file evidence")
    if "requested" in value and (not isinstance(value["requested"], str) or not value["requested"]):
        raise EvidenceError(f"{context} receipt {field} has invalid requested name")
    return size, digest


def receipt_source_identity(value: Any, context: str) -> tuple[str, ...]:
    if not isinstance(value, dict) or set(value) != {
        "schema", "combination_id", "sources"
    } or value["schema"] != 1:
        raise EvidenceError(f"{context} receipt has invalid source identity")
    combination_id = value["combination_id"]
    sources = value["sources"]
    try:
        declared_sources = load_source_combination(
            Path(__file__).resolve().parents[2] / "config" / "source-combination.toml"
        )
    except SourceCombinationError as error:
        raise EvidenceError(f"cannot validate source combination: {error}") from error
    expected_sources = {"thekernel", *declared_sources}
    if (
        not isinstance(combination_id, str)
        or not re.fullmatch(r"source-combination-v1-[0-9a-f]{64}", combination_id)
        or not isinstance(sources, dict)
        or set(sources) != expected_sources
    ):
        raise EvidenceError(f"{context} receipt has invalid source identity")
    identity: list[str] = [combination_id]
    for name in sorted(sources):
        source = sources[name]
        if not isinstance(source, dict) or set(source) != {
            "repository_root", "commit", "tree", "worktree_dirty", "match_declared"
        }:
            raise EvidenceError(f"{context} receipt has invalid source identity")
        root = source["repository_root"]
        commit = source["commit"]
        tree = source["tree"]
        dirty = source["worktree_dirty"]
        matches = source["match_declared"]
        if (
            not isinstance(root, str)
            or not Path(root).is_absolute()
            or not isinstance(commit, str)
            or not GIT_OBJECT_RE.fullmatch(commit)
            or not isinstance(tree, str)
            or not GIT_OBJECT_RE.fullmatch(tree)
            or type(dirty) is not bool
            or type(matches) is not bool
        ):
            raise EvidenceError(f"{context} receipt has invalid source identity")
        if dirty:
            raise EvidenceError(f"{context} receipt source identity is dirty: {name}")
        if not matches:
            raise EvidenceError(f"{context} receipt source identity does not match declared combination: {name}")
        identity.extend((name, commit, tree))
    return tuple(identity)


def validate_receipt(receipt_path: Path, log_path: Path, arch: str, cpus: int, context: str) -> tuple[tuple[Any, ...], tuple[int, str]]:
    try:
        payload = json.loads(receipt_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise EvidenceError(f"cannot read {context} performance receipt: {error}") from error
    if not isinstance(payload, dict) or payload.get("schema_version") != 4:
        raise EvidenceError(f"{context} has unsupported performance receipt")
    expected = {
        "state": "recorded", "arch": arch, "cpus": cpus, "returncode": 0,
        "error_message": None, "timed_out": False, "interrupted": False,
        "intentionally_stopped": False, "marker_success": False,
        "guest_clean_shutdown": True, "runner_terminated": False,
        "runner_termination_reason": None, "direct_kernel": False,
        "physical_retirement_proven": False,
        "rootfs_mode": "snapshot",
    }
    if any(payload.get(key) != value for key, value in expected.items()):
        raise EvidenceError(f"{context} receipt final state mismatch")
    if type(payload.get("duration_ms")) is not int or payload["duration_ms"] < 0:
        raise EvidenceError(f"{context} receipt has invalid duration")
    source_identity = receipt_source_identity(payload.get("source_identity"), context)
    if payload.get("interaction") != {
        "interactive": True, "input_after_marker": "THEKERNEL_SHELL_READY",
        "stop_after_marker": None,
    }:
        raise EvidenceError(f"{context} receipt interaction contract mismatch")
    stdin = payload.get("stdin")
    if not isinstance(stdin, dict) or set(stdin) != {
        "source", "forwarded", "source_unchanged", "source_eof",
        "broken_pipe", "relay_complete",
    }:
        raise EvidenceError(f"{context} receipt lacks stdin evidence")
    source, forwarded = stdin.get("source"), stdin.get("forwarded")
    if not isinstance(source, dict) or set(source) != {"path", "sha256", "bytes", "line_count"}:
        raise EvidenceError(f"{context} receipt has invalid stdin source")
    if not isinstance(forwarded, dict) or set(forwarded) != {"sha256", "bytes", "line_count"}:
        raise EvidenceError(f"{context} receipt has invalid forwarded stdin")
    if not isinstance(source["path"], str) or not source["path"]:
        raise EvidenceError(f"{context} receipt has invalid stdin source path")
    for record, label in ((source, "source"), (forwarded, "forwarded")):
        if not isinstance(record["sha256"], str) or not HEX64_RE.fullmatch(record["sha256"]):
            raise EvidenceError(f"{context} receipt has invalid stdin {label} hash")
        for field in ("bytes", "line_count"):
            if type(record[field]) is not int or record[field] < 0:
                raise EvidenceError(f"{context} receipt has invalid stdin {label} {field}")
    if source["sha256"] != forwarded["sha256"] or source["bytes"] != forwarded["bytes"] or source["line_count"] != forwarded["line_count"]:
        raise EvidenceError(f"{context} receipt stdin forwarding is incomplete")
    if any(stdin.get(field) is not True for field in ("source_eof", "relay_complete", "source_unchanged")) or stdin.get("broken_pipe") is not False:
        raise EvidenceError(f"{context} receipt stdin forwarding is incomplete")
    files = (
        "kernel", "rootfs_source", "rootfs_runtime_before", "rootfs_runtime_after",
        "qemu", "esp_source", "esp_runtime", "ovmf_code", "ovmf_vars_source",
        "ovmf_vars_runtime", "log",
    )
    identities = {field: receipt_file_identity(payload.get(field), field, context) for field in files}
    immutable_snapshot_pairs = [
        ("rootfs_runtime_before", "rootfs_runtime_after"),
        ("ovmf_vars_source", "ovmf_vars_runtime"),
    ]
    if not payload["rootfs_source"]["path"].endswith((".gz", ".xz")):
        immutable_snapshot_pairs.append(("rootfs_source", "rootfs_runtime_before"))
    if not payload["esp_source"]["path"].endswith((".gz", ".xz")):
        immutable_snapshot_pairs.append(("esp_source", "esp_runtime"))
    if any(identities[source] != identities[runtime] for source, runtime in immutable_snapshot_pairs):
        raise EvidenceError(f"{context} receipt changes a snapshot input")
    log = payload["log"]
    if (
        payload.get("log_path") != log["path"]
        or Path(log["path"]).name != "console.log"
        or log["size_bytes"] != log_path.stat().st_size
        or log["sha256"] != sha256_file(log_path)
    ):
        raise EvidenceError(f"{context} receipt log evidence does not match console.log")
    if (
        not isinstance(payload.get("memory"), str)
        or not payload["memory"]
        or not isinstance(payload.get("extra_args"), list)
        or not isinstance(payload.get("network"), str)
        or not payload["network"]
    ):
        raise EvidenceError(f"{context} receipt has invalid run configuration")
    identity = (
        payload.get("memory"), payload.get("accel"), payload.get("cpu"),
        payload.get("iothread_id"), payload.get("network"), payload.get("tap_name"),
        payload["extra_args"], payload.get("qemu_launcher"), payload["rootfs_mode"],
        payload["direct_kernel"], payload["qemu"].get("requested"),
        *source_identity,
        source["sha256"], source["bytes"], source["line_count"],
        *(identities[field] for field in (
            "rootfs_source", "qemu", "esp_source", "ovmf_code", "ovmf_vars_source"
        )),
    )
    return identity, identities["kernel"]


def run_parser(log: Path, arch: str, cpus: int, context: str) -> list[dict[str, str]]:
    completed = subprocess.run(
        [sys.executable, str(PARSER), str(log), "--arch", arch, "--cpus", str(cpus)],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False,
    )
    if completed.returncode:
        raise EvidenceError(
            f"{context} parser rejected console log: {completed.stderr.decode(errors='replace').strip()}"
        )
    return read_tsv_bytes(completed.stdout, METRIC_COLUMNS, f"{context} derived metrics")


def parse_metric(row: dict[str, str], run: Run, context: str) -> MetricRecord:
    arch = row["arch"]
    cpus = parse_positive_int(row["requested_cpus"], "requested_cpus", context)
    online = parse_positive_int(row["online_cpus"], "online_cpus", context)
    if (arch, cpus) != run.key or online != cpus:
        raise EvidenceError(f"{context} topology does not match receipt")
    metric = row["metric"]
    if metric not in EXPECTED_METRICS or row["status"] != "ok":
        raise EvidenceError(f"{context} is not complete regression evidence")
    count = parse_positive_int(row["count"], "count", context)
    p50, p99, p999 = (
        parse_nonnegative_int(row[key], key, context)
        for key in ("p50_ns", "p99_ns", "p999_ns")
    )
    if not p50 <= p99 <= p999:
        raise EvidenceError(f"{context} has non-monotonic quantiles")
    throughput = (
        parse_positive_int(row["throughput_bytes_per_sec"], "throughput_bytes_per_sec", context)
        if metric in PIN_METRICS else None
    )
    if metric not in PIN_METRICS and row["throughput_bytes_per_sec"] != "-":
        raise EvidenceError(f"{context} has unexpected throughput")
    if metric in VMA_FIXTURE_METRICS:
        requested = parse_positive_int(row["requested_vmas"], "requested_vmas", context)
        fixture = parse_positive_int(row["fixture_vmas"], "fixture_vmas", context)
        if requested != fixture:
            raise EvidenceError(f"{context} has inconsistent VMA fixture")
    else:
        requested = fixture = None
        if row["requested_vmas"] != "-" or row["fixture_vmas"] != "-":
            raise EvidenceError(f"{context} has unexpected VMA fixture")
    if row["reason"] != "-" or row["errno"] != "-":
        raise EvidenceError(f"{context} has unexpected error fields")
    return MetricRecord(arch, cpus, online, metric, count, p50, p99, p999, throughput, requested, fixture)


def load_bundle(path: Path, *, allow_partial: bool = False) -> Bundle:
    try:
        root = path.expanduser().resolve(strict=True)
    except OSError as error:
        raise EvidenceError(f"evidence directory is missing: {path}") from error
    rows = read_tsv(safe_file(root, "mm-performance-manifest.tsv", "manifest"), MANIFEST_COLUMNS, "manifest")
    runs: dict[tuple[str, int], Run] = {}
    metrics: dict[tuple[str, int, str], MetricRecord] = {}
    for line, row in enumerate(rows, start=2):
        context = f"manifest row {line}"
        if row["mode"] not in MEASUREMENT_MODES or row["arch"] != "x86_64":
            raise EvidenceError(f"{context} has unsupported mode or architecture")
        cpus = parse_positive_int(row["cpus"], "cpus", context)
        if cpus > 64 or parse_positive_int(row["online_cpus"], "online_cpus", context) != cpus:
            raise EvidenceError(f"{context} has invalid CPU topology")
        key = (row["arch"], cpus)
        if key in runs:
            raise EvidenceError(f"manifest has duplicate run key: {key!r}")
        metrics_path = safe_file(root, row["metrics"], f"{context} metrics")
        receipt_path = safe_file(root, row["receipt"], f"{context} receipt")
        pre = safe_file(root, row["host_pre"], f"{context} host_pre")
        post = safe_file(root, row["host_post"], f"{context} host_post")
        log_path = receipt_path.with_name("console.log")
        if not log_path.is_file():
            raise EvidenceError(f"{context} receipt has no run-local console.log")
        identity, kernel = validate_receipt(receipt_path, log_path, key[0], cpus, context)
        before = validate_host_snapshot(pre, "pre", cpus, context)
        after = validate_host_snapshot(post, "post", cpus, context)
        if before.cpu_set != after.cpu_set or before.selection != after.selection or before.cpu_class != after.cpu_class or before.timestamp >= after.timestamp:
            raise EvidenceError(f"{context} host snapshots do not bound one run")
        run = Run(key, row["mode"], metrics_path, sha256_file(receipt_path), identity, kernel, before, before.timestamp, after.timestamp)
        derived = run_parser(log_path, key[0], cpus, context)
        actual = read_tsv(metrics_path, METRIC_COLUMNS, f"{context} metrics")
        canonical = lambda items: sorted(tuple(item[column] for column in METRIC_COLUMNS) for item in items)
        if canonical(actual) != canonical(derived):
            raise EvidenceError(f"{context} metrics do not match receipt console log")
        run_metrics: dict[str, MetricRecord] = {}
        for number, item in enumerate(actual, start=2):
            record = parse_metric(item, run, f"{context} metrics row {number}")
            if record.metric in run_metrics:
                raise EvidenceError(f"{context} has duplicate metric {record.metric}")
            run_metrics[record.metric] = record
        if set(run_metrics) != set(EXPECTED_METRICS):
            raise EvidenceError(f"{context} metric set mismatch")
        runs[key] = run
        for record in run_metrics.values():
            if record.key in metrics:
                raise EvidenceError(f"bundle has duplicate metric key: {record.key!r}")
            metrics[record.key] = record
    keys = set(runs)
    if allow_partial:
        if not keys or not keys.issubset(REQUIRED_RELEASE_RUN_KEYS):
            raise EvidenceError("partial bundle has invalid run-key set")
    elif keys != REQUIRED_RELEASE_RUN_KEYS:
        raise EvidenceError(f"release bundle run-key set mismatch: expected={sorted(REQUIRED_RELEASE_RUN_KEYS)!r} actual={sorted(keys)!r}")
    ordered = list(runs.values())
    for previous, current in zip(ordered, ordered[1:]):
        if previous.capture_end >= current.capture_start:
            raise EvidenceError("bundle run capture intervals overlap or are out of order")
    host_sets: dict[int, set[int]] = {}
    for run in ordered:
        if run.mode != "product":
            raise EvidenceError("diagnostic evidence is not product regression evidence")
        selected = set(parse_cpu_list(run.host.cpu_set))
        prior = host_sets.setdefault(run.key[1], selected)
        if prior != selected:
            raise EvidenceError("bundle host CPU set is not uniform")
    previous: set[int] = set()
    for count in sorted(host_sets):
        if not previous.issubset(host_sets[count]):
            raise EvidenceError("bundle host CPU sets are not nested")
        previous = host_sets[count]
    return Bundle(root, runs, metrics, ordered[0].capture_start, ordered[-1].capture_end)


def policy_percent(value: Any, field: str, metric: str, nullable: bool) -> int | None:
    if value is None and nullable:
        return None
    if isinstance(value, bool) or not isinstance(value, int) or not 0 <= value <= 10000:
        raise EvidenceError(f"policy metric={metric} has invalid {field}")
    return value


def load_policy(path: Path) -> dict[str, MetricPolicy]:
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise EvidenceError(f"cannot read regression policy {path}: {error}") from error
    if not isinstance(payload, dict) or set(payload) != {"schema", "metrics"} or payload["schema"] != POLICY_SCHEMA or not isinstance(payload["metrics"], dict) or set(payload["metrics"]) != set(EXPECTED_METRICS):
        raise EvidenceError("unsupported regression policy")
    result = {}
    for metric in EXPECTED_METRICS:
        entry = payload["metrics"][metric]
        if not isinstance(entry, dict) or set(entry) != {"p99_max_regression_percent", "throughput_min_retained_percent"}:
            raise EvidenceError(f"policy metric={metric} has invalid fields")
        p99 = policy_percent(entry["p99_max_regression_percent"], "p99_max_regression_percent", metric, False)
        throughput = policy_percent(entry["throughput_min_retained_percent"], "throughput_min_retained_percent", metric, True)
        if p99 is None or p99 > 20 or (metric in PIN_METRICS and (throughput is None or throughput < 90)) or (metric not in PIN_METRICS and throughput is not None):
            raise EvidenceError(f"policy metric={metric} weakens the regression contract")
        result[metric] = MetricPolicy(p99, throughput)
    return result


def load_stability_policy(path: Path) -> StabilityPolicy:
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise EvidenceError(f"cannot read stability policy {path}: {error}") from error
    expected = {"schema", "minimum_pairs", "maximum_pairs", "maximum_pair_ratio_spread_percent"}
    if not isinstance(payload, dict) or set(payload) != expected or payload["schema"] != STABILITY_POLICY_SCHEMA:
        raise EvidenceError("unsupported stability policy")
    values = [payload[name] for name in ("minimum_pairs", "maximum_pairs", "maximum_pair_ratio_spread_percent")]
    if any(isinstance(value, bool) or not isinstance(value, int) for value in values):
        raise EvidenceError("stability policy has invalid values")
    minimum, maximum, spread = values
    if minimum < 3 or minimum % 2 == 0 or maximum < minimum or maximum > 101 or maximum % 2 == 0 or not 0 <= spread <= MAX_PAIR_RATIO_SPREAD_PERCENT:
        raise EvidenceError("stability policy weakens the regression contract")
    return StabilityPolicy(minimum, maximum, spread)


def compare_provenance(before: Bundle, after: Bundle) -> None:
    if set(before.runs) != set(after.runs):
        raise EvidenceError("bundle run-key set mismatch")
    for key in sorted(before.runs):
        left, right = before.runs[key], after.runs[key]
        if (
            left.mode != right.mode
            or left.identity != right.identity
            or (left.host.cpu_set, left.host.selection, left.host.cpu_class)
            != (right.host.cpu_set, right.host.selection, right.host.cpu_class)
        ):
            raise EvidenceError(f"run {key!r} is not comparable")
    if set(before.metrics) != set(after.metrics):
        raise EvidenceError("bundle metric-key set mismatch")
    for key in before.metrics:
        left, right = before.metrics[key], after.metrics[key]
        if (left.online_cpus, left.count) != (right.online_cpus, right.count):
            raise EvidenceError(f"metric {key!r} topology or sample count differs")


def validate_side_identity(label: str, bundles: list[Bundle]) -> None:
    reference = bundles[0]
    for index, bundle in enumerate(bundles[1:], start=2):
        compare_provenance(reference, bundle)
        for key in reference.runs:
            if reference.runs[key].kernel_identity != bundle.runs[key].kernel_identity:
                raise EvidenceError(f"{label} series changes kernel identity at pair {index} run={key!r}")


def validate_counterbalanced_capture_order(baselines: list[Bundle], candidates: list[Bundle]) -> None:
    previous_end: dt.datetime | None = None
    previous_orientation: str | None = None
    for index, (baseline, candidate) in enumerate(zip(baselines, candidates, strict=True), start=1):
        if baseline.capture_end < candidate.capture_start:
            orientation, start, end = "baseline-first", baseline.capture_start, candidate.capture_end
        elif candidate.capture_end < baseline.capture_start:
            orientation, start, end = "candidate-first", candidate.capture_start, baseline.capture_end
        else:
            raise EvidenceError(f"pair {index} was not captured as a disjoint pair")
        if (previous_end is not None and previous_end >= start) or previous_orientation == orientation:
            raise EvidenceError("paired series is not chronological counterbalanced evidence")
        previous_end, previous_orientation = end, orientation


def ratio_compare(left: RatioSample, right: RatioSample) -> int:
    before, after = left.candidate * right.baseline, right.candidate * left.baseline
    return (before > after) - (before < after)


def ratio_ppm(sample: RatioSample) -> str:
    return str(sample.candidate * 1_000_000 // sample.baseline)


def paired_row(metric: MetricRecord, statistic: str, samples: list[RatioSample], threshold: int | None, comparator: str, stability: StabilityPolicy) -> ReportRow:
    if any(sample.baseline <= 0 for sample in samples):
        raise EvidenceError(f"{metric.key!r} {statistic} has a zero baseline")
    ordered = sorted(samples, key=cmp_to_key(ratio_compare))
    low, median, high = ordered[0], ordered[len(ordered) // 2], ordered[-1]
    if threshold is None:
        result, mode = "REPORT_ONLY", "report_only"
    else:
        if high.candidate * low.baseline * 100 > low.candidate * high.baseline * (100 + stability.maximum_pair_ratio_spread_percent):
            raise EvidenceError(f"unstable paired series for {metric.key!r} {statistic}")
        passed = median.candidate * 100 <= median.baseline * threshold if comparator == "<=" else median.candidate * 100 >= median.baseline * threshold
        result, mode = ("PASS" if passed else "FAIL"), "gate"
    return ReportRow(metric.arch, metric.requested_cpus, metric.metric, statistic, mode, len(samples), median.pair, median.baseline, median.candidate, 0 if threshold is None else threshold, "-" if threshold is None else comparator, result, ratio_ppm(median), ratio_ppm(low), ratio_ppm(high))


def compare_series(baselines: list[Bundle], candidates: list[Bundle], policy: dict[str, MetricPolicy], stability: StabilityPolicy) -> list[ReportRow]:
    if len(baselines) != len(candidates) or len(baselines) % 2 == 0 or not stability.minimum_pairs <= len(baselines) <= stability.maximum_pairs:
        raise EvidenceError("paired series length is outside the stability policy")
    receipts: set[str] = set()
    for bundle in (*baselines, *candidates):
        for run in bundle.runs.values():
            if run.receipt_sha256 in receipts:
                raise EvidenceError("paired series reuses one performance receipt")
            receipts.add(run.receipt_sha256)
    validate_counterbalanced_capture_order(baselines, candidates)
    validate_side_identity("baseline", baselines)
    validate_side_identity("candidate", candidates)
    for baseline, candidate in zip(baselines, candidates, strict=True):
        compare_provenance(baseline, candidate)
    rows = []
    for key in sorted(baselines[0].metrics, key=lambda item: (item[1], EXPECTED_METRICS.index(item[2]))):
        metric, entry = baselines[0].metrics[key], policy[key[2]]
        pairs = list(enumerate(zip(baselines, candidates, strict=True), start=1))
        rows.append(paired_row(metric, "p99_ns", [RatioSample(index, before.metrics[key].p99_ns, after.metrics[key].p99_ns) for index, (before, after) in pairs], 100 + entry.p99_max_regression_percent, "<=", stability))
        rows.append(paired_row(metric, "p999_ns", [RatioSample(index, before.metrics[key].p999_ns, after.metrics[key].p999_ns) for index, (before, after) in pairs], None, "-", stability))
        if key[2] in PIN_METRICS:
            rows.append(paired_row(metric, "throughput_bytes_per_sec", [RatioSample(index, before.metrics[key].throughput_bytes_per_sec or 0, after.metrics[key].throughput_bytes_per_sec or 0) for index, (before, after) in pairs], entry.throughput_min_retained_percent, ">=", stability))
    return rows


def validate_output_destination(path: Path, bundles: Iterable[Bundle]) -> Path:
    destination = path.expanduser().resolve()
    if any(destination.is_relative_to(bundle.root) for bundle in bundles):
        raise EvidenceError("regression report destination must be outside every input evidence bundle")
    return destination


def write_report(path: Path, rows: Iterable[ReportRow], release_gate: bool) -> None:
    if not path.parent.is_dir():
        raise EvidenceError(f"regression report parent is not a directory: {path.parent}")
    with tempfile.NamedTemporaryFile("w", encoding="utf-8", newline="", dir=path.parent, delete=False) as output:
        temporary = Path(output.name)
        writer = csv.writer(output, delimiter="\t", lineterminator="\n")
        writer.writerow(REPORT_COLUMNS)
        for row in rows:
            writer.writerow(("release" if release_gate else "partial_triage", "true" if release_gate else "false", row.arch, row.requested_cpus, row.metric, row.statistic, row.mode, row.pair_count, row.median_pair, row.baseline, row.candidate, row.threshold_percent if row.mode == "gate" else "-", row.comparator, row.result, row.candidate_ratio_ppm, row.pair_ratio_min_ppm, row.pair_ratio_max_ppm))
    os.replace(temporary, path)


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description="compare an odd paired series of MM performance receipts")
    parser.add_argument("--baseline", type=Path, action="append", required=True)
    parser.add_argument("--candidate", type=Path, action="append", required=True)
    parser.add_argument("--policy", type=Path, required=True)
    parser.add_argument("--stability-policy", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--allow-partial", action="store_true")
    return parser


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        baselines = [load_bundle(path, allow_partial=args.allow_partial) for path in args.baseline]
        candidates = [load_bundle(path, allow_partial=args.allow_partial) for path in args.candidate]
        output = validate_output_destination(args.output, (*baselines, *candidates))
        rows = compare_series(baselines, candidates, load_policy(args.policy), load_stability_policy(args.stability_policy))
        write_report(output, rows, not args.allow_partial)
    except EvidenceError as error:
        print(f"compare-mm-performance: INVALID: {error}", file=sys.stderr)
        return 2
    failures = sum(row.result == "FAIL" for row in rows)
    print(f"compare-mm-performance: {'REGRESSION' if failures else 'PASS'} pairs={len(args.baseline)} report={args.output}")
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
