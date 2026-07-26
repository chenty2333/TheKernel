#!/usr/bin/env python3
"""Validate and compare portable TheKernel MM performance evidence bundles."""

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

from mm_performance_host import CpuSelectionError, format_cpu_list, parse_cpu_list
from mm_performance_schema import (
    BUNDLE_SCHEMA,
    EXPECTED_METRICS,
    HOST_DIAGNOSTIC_SCHEMA,
    KERNEL_PROFILE_BY_MODE,
    MANIFEST_COLUMNS,
    MEASUREMENT_MODES,
    PHYSICAL_FREQ_POLICY,
    PLATFORM_CLASSES,
    PLATFORM_NOT_APPLICABLE,
    PMU_SOURCES,
    PMU_SOURCE_BY_ARCH,
    METRIC_COLUMNS,
    MM_LOCK_DIAGNOSTIC_SENTINEL,
    PIN_METRICS,
    POLICY_SCHEMA,
    REPORT_COLUMNS,
    STABILITY_POLICY_SCHEMA,
    VMA_FIXTURE_METRICS,
)

REPO_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO_ROOT))

from tools.qemu_runner.receipt import (  # noqa: E402
    ReceiptError,
    command_stream_evidence,
    validate_completed_input_receipt,
    validate_receipt_file_evidence,
)
from tools.qemu_runner.command import build_qemu_command  # noqa: E402
from tools.qemu_runner.model import Drive  # noqa: E402

HEX40_RE = re.compile(r"^[0-9a-f]{40}$")
HEX64_RE = re.compile(r"^[0-9a-f]{64}$")
FINGERPRINT_RE = re.compile(r"^(?:auto|declared)-sha256:[0-9a-f]{64}$")
CPU_CLASS_RE = re.compile(r"^package:[0-9]+,max_freq_khz:[1-9][0-9]*$")
RFC3339_UTC_RE = re.compile(
    r"^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}"
    r"(?:\.[0-9]{1,6})?(?:Z|\+00:00)$"
)
HOST_DIAGNOSTIC_MAX_BYTES = 64 * 1024
MAX_PAIR_RATIO_SPREAD_PERCENT = 20
PARSER_DIR = Path(__file__).resolve().parent
REQUIRED_RELEASE_RUN_KEYS = frozenset(
    {("rv", 4), ("rv", 8), ("la", 4), ("la", 8)}
)


class EvidenceError(ValueError):
    """Raised when a bundle, policy, or comparison contract is invalid."""


@dataclass(frozen=True)
class ManifestRecord:
    key: tuple[str, int]
    values: dict[str, str]
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
    manifest: dict[tuple[str, int], ManifestRecord]
    metrics: dict[tuple[str, int, str], MetricRecord]
    receipt_sha256: str
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


def read_tsv_source(
    source: Any, expected_columns: tuple[str, ...], context: str
) -> list[dict[str, str]]:
    reader = csv.reader(source, delimiter="\t", strict=True)
    try:
        header = tuple(next(reader))
    except StopIteration as error:
        raise EvidenceError(f"{context} is empty") from error
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
    if not rows:
        raise EvidenceError(f"{context} has no data rows")
    return rows


def read_tsv(
    path: Path, expected_columns: tuple[str, ...], context: str
) -> list[dict[str, str]]:
    try:
        with path.open("r", encoding="utf-8", newline="") as source:
            return read_tsv_source(source, expected_columns, context)
    except (OSError, csv.Error) as error:
        raise EvidenceError(f"cannot read {context} {path}: {error}") from error


def read_tsv_bytes(
    payload: bytes, expected_columns: tuple[str, ...], context: str
) -> list[dict[str, str]]:
    try:
        source = io.StringIO(payload.decode("utf-8"), newline="")
        return read_tsv_source(source, expected_columns, context)
    except (UnicodeDecodeError, csv.Error) as error:
        raise EvidenceError(f"cannot read {context}: {error}") from error


def run_evidence_parser(
    script_name: str, arguments: list[str], context: str
) -> bytes:
    script = PARSER_DIR / script_name
    if not script.is_file():
        raise EvidenceError(f"{context} parser is missing: {script}")
    try:
        completed = subprocess.run(
            [sys.executable, str(script), *arguments],
            cwd=PARSER_DIR,
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
    except OSError as error:
        raise EvidenceError(f"cannot run {context} parser: {error}") from error
    if completed.returncode != 0:
        detail = completed.stderr.decode("utf-8", errors="replace").strip()
        raise EvidenceError(
            f"{context} parser rejected the raw QEMU log: {detail or 'no diagnostic'}"
        )
    return completed.stdout


def canonical_metric_rows(rows: list[dict[str, str]]) -> list[tuple[str, ...]]:
    return sorted(tuple(row[column] for column in METRIC_COLUMNS) for row in rows)


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


def parse_rfc3339_utc(value: str, context: str) -> dt.datetime:
    if not RFC3339_UTC_RE.fullmatch(value):
        raise EvidenceError(f"{context} is not strict RFC3339 UTC: {value!r}")
    normalized = value[:-1] + "+00:00" if value.endswith("Z") else value
    try:
        result = dt.datetime.fromisoformat(normalized)
    except ValueError as error:
        raise EvidenceError(f"{context} has an invalid calendar time: {value!r}") from error
    if result.tzinfo is None or result.utcoffset() != dt.timedelta(0):
        raise EvidenceError(f"{context} is not UTC: {value!r}")
    return result


def validate_host_diagnostics(
    root: Path,
    values: dict[str, str],
    phase: str,
    context: str,
) -> dt.datetime:
    path = validate_artifact(
        root,
        values,
        f"host_diagnostics_{phase}",
        f"host_diagnostics_{phase}_sha256",
        f"host_diagnostics_{phase}_size_bytes",
        context,
    )
    if path.stat().st_size > HOST_DIAGNOSTIC_MAX_BYTES:
        raise EvidenceError(
            f"{context} host diagnostics exceed {HOST_DIAGNOSTIC_MAX_BYTES} bytes"
        )
    rows = read_tsv(path, ("key", "value"), f"{context} host diagnostics {phase}")
    diagnostics: dict[str, str] = {}
    allowed_fixed = {
        "schema",
        "phase",
        "timestamp_utc",
        "selected_cpu_set",
        "host_cpu_selection",
        "host_cpu_class",
        "online_cpu_set",
        "loadavg",
        "psi.cpu",
        "cgroup.cpu_stat",
    }
    allowed_dynamic = re.compile(
        r"^(?:psi\.cpu\.[A-Za-z0-9_.-]+|"
        r"cgroup\.cpu_stat\.[A-Za-z0-9_.-]+|"
        r"cpu\.[0-9]+\.(?:online|package|max_freq_khz|current_freq_khz))$"
    )
    for line_number, row in enumerate(rows, start=2):
        key = row["key"]
        value = row["value"]
        if key in diagnostics:
            raise EvidenceError(
                f"{context} host diagnostics {phase} duplicate key at row "
                f"{line_number}: {key!r}"
            )
        if key not in allowed_fixed and not allowed_dynamic.fullmatch(key):
            raise EvidenceError(
                f"{context} host diagnostics {phase} contains unsafe key: {key!r}"
            )
        if not value or any(character in value for character in "\t\r\n"):
            raise EvidenceError(
                f"{context} host diagnostics {phase} has invalid value for {key!r}"
            )
        diagnostics[key] = value
    required = {
        "schema",
        "phase",
        "timestamp_utc",
        "selected_cpu_set",
        "host_cpu_selection",
        "host_cpu_class",
        "online_cpu_set",
        "loadavg",
    }
    missing = sorted(required - diagnostics.keys())
    if missing:
        raise EvidenceError(
            f"{context} host diagnostics {phase} is missing keys: {missing!r}"
        )
    if not any(key == "psi.cpu" or key.startswith("psi.cpu.") for key in diagnostics):
        raise EvidenceError(f"{context} host diagnostics {phase} lacks CPU pressure")
    if not any(key.startswith("cgroup.cpu_stat") for key in diagnostics):
        raise EvidenceError(f"{context} host diagnostics {phase} lacks cgroup CPU stats")
    expected = {
        "schema": HOST_DIAGNOSTIC_SCHEMA,
        "phase": phase,
        "selected_cpu_set": values["host_cpu_set"],
        "host_cpu_selection": values["host_cpu_selection"],
        "host_cpu_class": values["host_cpu_class"],
    }
    for key, value in expected.items():
        if diagnostics.get(key) != value:
            raise EvidenceError(
                f"{context} host diagnostics {phase} {key} mismatch: "
                f"expected={value!r} actual={diagnostics.get(key)!r}"
            )
    class_fields = dict(
        field.split(":", 1) for field in values["host_cpu_class"].split(",")
    )
    for cpu in parse_cpu_list(values["host_cpu_set"]):
        expected_cpu = {
            f"cpu.{cpu}.online": "1",
            f"cpu.{cpu}.package": class_fields["package"],
            f"cpu.{cpu}.max_freq_khz": class_fields["max_freq_khz"],
        }
        for key, value in expected_cpu.items():
            if diagnostics.get(key) != value:
                raise EvidenceError(
                    f"{context} host diagnostics {phase} {key} mismatch: "
                    f"expected={value!r} actual={diagnostics.get(key)!r}"
                )
        current_key = f"cpu.{cpu}.current_freq_khz"
        if current_key not in diagnostics:
            raise EvidenceError(
                f"{context} host diagnostics {phase} lacks {current_key}"
            )
    return parse_rfc3339_utc(
        diagnostics["timestamp_utc"],
        f"{context} host diagnostics {phase} timestamp_utc",
    )


def read_guest_inputs(path: Path, context: str) -> dict[str, str]:
    values: dict[str, str] = {}
    try:
        with path.open("r", encoding="utf-8", newline="") as source:
            reader = csv.reader(source, delimiter="\t", strict=True)
            for line_number, cells in enumerate(reader, start=1):
                if len(cells) != 2 or not cells[0] or not cells[1]:
                    raise EvidenceError(
                        f"{context} guest input receipt row {line_number} is invalid"
                    )
                key, value = cells
                if key in values:
                    raise EvidenceError(
                        f"{context} guest input receipt duplicates {key!r}"
                    )
                values[key] = value
    except (OSError, csv.Error) as error:
        raise EvidenceError(f"cannot read {context} guest input receipt: {error}") from error
    return values


def validate_input_receipts(
    *,
    row: dict[str, str],
    context: str,
    commands_path: Path,
    guest_inputs_path: Path,
    qemu_receipt_path: Path,
) -> None:
    commands = command_stream_evidence(commands_path)
    guest = read_guest_inputs(guest_inputs_path, context)
    expected_guest = {
        "schema_version": "1",
        "arch": row["arch"],
        "requested_cpus": row["requested_cpus"],
        "kernel_profile": row["kernel_profile"],
        "kernel_size_bytes": row["kernel_size_bytes"],
        "kernel_sha256": row["kernel_sha256"],
        "commands_size_bytes": str(commands["bytes"]),
        "commands_line_count": str(commands["line_count"]),
        "commands_sha256": str(commands["sha256"]),
        "rootfs_sha256": row["rootfs_sha256"],
        "qemu_sha256": row["qemu_sha256"],
        "qemu_version": row["qemu_version"],
    }
    for key, expected in expected_guest.items():
        actual = guest.get(key)
        if actual != expected:
            raise EvidenceError(
                f"{context} guest input receipt {key} mismatch: "
                f"expected={expected!r} actual={actual!r}"
            )
    qemu_binary = guest.get("qemu_binary")
    if not qemu_binary or Path(qemu_binary).name != row["qemu_binary"]:
        raise EvidenceError(f"{context} guest input receipt qemu_binary mismatch")

    try:
        with qemu_receipt_path.open(encoding="utf-8") as source:
            loaded: Any = json.load(source)
    except (OSError, json.JSONDecodeError) as error:
        raise EvidenceError(f"cannot read {context} QEMU receipt: {error}") from error
    if not isinstance(loaded, dict):
        raise EvidenceError(f"{context} QEMU receipt must be a JSON object")
    receipt: dict[str, Any] = loaded
    try:
        validate_completed_input_receipt(receipt, commands_path)
    except ReceiptError as error:
        raise EvidenceError(f"{context} QEMU stdin receipt is invalid: {error}") from error
    try:
        evidence_records = validate_receipt_file_evidence(receipt)
    except ReceiptError as error:
        raise EvidenceError(f"{context} QEMU receipt is invalid: {error}") from error

    expected_scalars: dict[str, Any] = {
        "arch": row["arch"],
        "cpus": int(row["requested_cpus"], 10),
        "memory": "1G",
        "rootfs_mode": "snapshot",
        "returncode": 0,
        "error_message": None,
        "timed_out": False,
        "interrupted": False,
        "intentionally_stopped": False,
    }
    for key, expected in expected_scalars.items():
        if receipt.get(key) != expected:
            raise EvidenceError(
                f"{context} QEMU receipt {key} mismatch: "
                f"expected={expected!r} actual={receipt.get(key)!r}"
            )
    expected_interaction = {
        "interactive": True,
        "input_after_marker": "THEKERNEL_SHELL_READY",
        "stop_after_marker": None,
        "external_input_producer": True,
    }
    if receipt.get("interaction") != expected_interaction:
        raise EvidenceError(f"{context} QEMU receipt interaction contract mismatch")
    for key in (
        "extra_block_source",
        "extra_block_runtime_before",
        "extra_block_runtime_after",
    ):
        if key in receipt:
            raise EvidenceError(f"{context} QEMU receipt unexpectedly contains {key}")

    expected_evidence = {
        "kernel": (row["kernel_sha256"], int(row["kernel_size_bytes"], 10)),
        "rootfs_source": (row["rootfs_sha256"], None),
        "qemu": (row["qemu_sha256"], None),
        "log": (row["qemu_log_sha256"], int(row["qemu_log_size_bytes"], 10)),
    }
    for key, (expected_sha256, expected_size) in expected_evidence.items():
        evidence = evidence_records[key]
        if evidence.get("sha256") != expected_sha256:
            raise EvidenceError(f"{context} QEMU receipt {key} SHA-256 mismatch")
        if expected_size is not None and evidence.get("size_bytes") != expected_size:
            raise EvidenceError(f"{context} QEMU receipt {key} size mismatch")

    rootfs_source = evidence_records["rootfs_source"]
    for key in ("rootfs_runtime_before", "rootfs_runtime_after"):
        runtime = evidence_records[key]
        if runtime.get("sha256") != rootfs_source.get("sha256") or runtime.get(
            "size_bytes"
        ) != rootfs_source.get("size_bytes"):
            raise EvidenceError(f"{context} QEMU receipt {key} differs from rootfs source")
    runtime_evidence = evidence_records["rootfs_runtime_before"]
    kernel_evidence = evidence_records["kernel"]
    qemu_evidence = evidence_records["qemu"]
    expected_command = list(
        build_qemu_command(
            arch=row["arch"],
            kernel=Path(kernel_evidence["path"]),
            rootfs=Drive(Path(runtime_evidence["path"]), "snapshot"),
            extra_block=None,
            memory="1G",
            cpus=int(row["requested_cpus"], 10),
            qemu_binary=qemu_evidence["path"],
        )
    )
    if receipt.get("command") != expected_command:
        raise EvidenceError(f"{context} QEMU receipt command does not match its inputs")


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
    measurement_mode = row["measurement_mode"]
    if measurement_mode not in MEASUREMENT_MODES:
        raise EvidenceError(
            f"{context} has invalid measurement_mode: {measurement_mode!r}"
        )
    expected_kernel_profile = KERNEL_PROFILE_BY_MODE[measurement_mode]
    if row["kernel_profile"] != expected_kernel_profile:
        raise EvidenceError(
            f"{context} kernel_profile does not match measurement_mode: "
            f"expected={expected_kernel_profile!r} actual={row['kernel_profile']!r}"
        )
    arch = row["arch"]
    if arch not in {"rv", "la"}:
        raise EvidenceError(f"{context} has invalid arch: {arch!r}")
    platform_class = row["platform_class"]
    if platform_class not in PLATFORM_CLASSES:
        raise EvidenceError(
            f"{context} has invalid platform_class: {platform_class!r}"
        )
    if platform_class == "physical":
        # RFC 0008: physical evidence needs its own receipt authority (PMU
        # receipts, firmware identity, frequency pinning) before it can be
        # validated. Declaring the class now reserves the vocabulary; accepting
        # a half-validated row here would let TCG-grade evidence carry
        # physical-grade claims.
        raise EvidenceError(
            f"{context} declares platform_class=physical, but the physical "
            "evidence authority is not implemented yet; qemu-tcg is the only "
            "accepted platform class"
        )
    pmu_source = row["pmu_source"]
    if pmu_source not in PMU_SOURCES:
        raise EvidenceError(f"{context} has invalid pmu_source: {pmu_source!r}")
    if pmu_source != "none":
        if pmu_source != PMU_SOURCE_BY_ARCH[arch]:
            raise EvidenceError(
                f"{context} pmu_source does not match arch: "
                f"arch={arch!r} pmu_source={pmu_source!r}"
            )
        raise EvidenceError(
            f"{context} qemu-tcg evidence must use pmu_source='none'; "
            "architectural PMU claims require physical evidence authority"
        )
    for field in ("cpu_model", "firmware_version", "cpu_freq_policy"):
        if row[field] != PLATFORM_NOT_APPLICABLE:
            raise EvidenceError(
                f"{context} qemu-tcg evidence must use "
                f"{field}={PLATFORM_NOT_APPLICABLE!r}; TCG has no "
                "authoritative CPU identity or frequency policy "
                f"(physical rows will require cpu_freq_policy="
                f"{PHYSICAL_FREQ_POLICY!r})"
            )
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
        raise EvidenceError(f"{context} workload exceeds the bundle-v10 limits")
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
    try:
        host_cpus = parse_cpu_list(row["host_cpu_set"])
    except CpuSelectionError as error:
        raise EvidenceError(f"{context} has invalid host_cpu_set: {error}") from error
    if len(host_cpus) != requested_cpus:
        raise EvidenceError(
            f"{context} host CPU topology mismatch: "
            f"guest={requested_cpus} host_set={format_cpu_list(host_cpus)}"
        )
    if row["host_cpu_selection"] not in {
        "auto-homogeneous-v1",
        "explicit-homogeneous-v1",
    }:
        raise EvidenceError(
            f"{context} has invalid host_cpu_selection: "
            f"{row['host_cpu_selection']!r}"
        )
    if not CPU_CLASS_RE.fullmatch(row["host_cpu_class"]):
        raise EvidenceError(
            f"{context} has invalid host_cpu_class: {row['host_cpu_class']!r}"
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
    diagnostics_path: Path | None = None
    asid_diagnostics_path: Path | None = None
    if measurement_mode == "product":
        for field in (
            "mm_lock_diagnostics_artifact",
            "mm_lock_diagnostics_sha256",
            "mm_lock_diagnostics_size_bytes",
        ):
            if row[field] != MM_LOCK_DIAGNOSTIC_SENTINEL:
                raise EvidenceError(
                    f"{context} product evidence must use "
                    f"{field}={MM_LOCK_DIAGNOSTIC_SENTINEL!r}"
                )
        for field in (
            "asid_switch_diagnostics_artifact",
            "asid_switch_diagnostics_sha256",
            "asid_switch_diagnostics_size_bytes",
        ):
            if row[field] != MM_LOCK_DIAGNOSTIC_SENTINEL:
                raise EvidenceError(
                    f"{context} product evidence must use "
                    f"{field}={MM_LOCK_DIAGNOSTIC_SENTINEL!r}"
                )
    else:
        diagnostics_path = validate_artifact(
            root,
            row,
            "mm_lock_diagnostics_artifact",
            "mm_lock_diagnostics_sha256",
            "mm_lock_diagnostics_size_bytes",
            context,
        )
        asid_diagnostics_path = validate_artifact(
            root,
            row,
            "asid_switch_diagnostics_artifact",
            "asid_switch_diagnostics_sha256",
            "asid_switch_diagnostics_size_bytes",
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
    guest_inputs_path = validate_artifact(
        root,
        row,
        "guest_inputs",
        "guest_inputs_sha256",
        "guest_inputs_size_bytes",
        context,
    )
    qemu_receipt_path = validate_artifact(
        root,
        row,
        "qemu_receipt",
        "qemu_receipt_sha256",
        "qemu_receipt_size_bytes",
        context,
    )
    qemu_log_path = validate_artifact(
        root,
        row,
        "qemu_log",
        "qemu_log_sha256",
        "qemu_log_size_bytes",
        context,
    )
    validate_input_receipts(
        row=row,
        context=context,
        commands_path=commands_path,
        guest_inputs_path=guest_inputs_path,
        qemu_receipt_path=qemu_receipt_path,
    )
    try:
        qemu_log = qemu_log_path.read_bytes()
    except OSError as error:
        raise EvidenceError(f"cannot read {context} raw QEMU log: {error}") from error
    if measurement_mode == "product" and any(
        line.startswith((b"MM_LOCK_", b"ASID_SWITCH_", b"PMU_"))
        for line in qemu_log.splitlines()
    ):
        raise EvidenceError(f"{context} product raw QEMU log contains diagnostics")

    derived_metrics = run_evidence_parser(
        "parse-mm-performance.py",
        [
            str(qemu_log_path),
            "--arch",
            arch,
            "--cpus",
            str(requested_cpus),
            "--iterations",
            row["iterations"],
            "--vmas",
            row["live_vmas"],
            "--pin-iterations",
            row["pin_iterations"],
            "--pin-workers",
            row["pin_workers"],
        ],
        f"{context} MM performance",
    )
    derived_metric_rows = read_tsv_bytes(
        derived_metrics,
        METRIC_COLUMNS,
        f"{context} metrics derived from raw QEMU log",
    )
    artifact_metric_rows = read_tsv(
        metrics_path,
        METRIC_COLUMNS,
        f"{context} metrics artifact",
    )
    if canonical_metric_rows(derived_metric_rows) != canonical_metric_rows(
        artifact_metric_rows
    ):
        raise EvidenceError(
            f"{context} metrics artifact does not match the raw QEMU log"
        )

    if diagnostics_path is not None:
        derived_diagnostics = run_evidence_parser(
            "parse-mm-lock-diagnostics.py",
            [str(qemu_log_path)],
            f"{context} MM lock diagnostics",
        )
        try:
            artifact_diagnostics = diagnostics_path.read_bytes()
        except OSError as error:
            raise EvidenceError(
                f"cannot read {context} MM lock diagnostics artifact: {error}"
            ) from error
        if artifact_diagnostics != derived_diagnostics:
            raise EvidenceError(
                f"{context} MM lock diagnostics artifact does not match "
                "the raw QEMU log"
            )
    if asid_diagnostics_path is not None:
        derived_asid_diagnostics = run_evidence_parser(
            "parse-asid-switch-diagnostics.py",
            [str(qemu_log_path)],
            f"{context} ASID switch diagnostics",
        )
        try:
            artifact_asid_diagnostics = asid_diagnostics_path.read_bytes()
        except OSError as error:
            raise EvidenceError(
                f"cannot read {context} ASID switch diagnostics artifact: {error}"
            ) from error
        if artifact_asid_diagnostics != derived_asid_diagnostics:
            raise EvidenceError(
                f"{context} ASID switch diagnostics artifact does not match "
                "the raw QEMU log"
            )
        run_evidence_parser(
            "parse-pmu-capabilities.py",
            [str(qemu_log_path), "--arch", arch],
            f"{context} capability-only PMU diagnostics",
        )
    capture_start = validate_host_diagnostics(root, row, "pre", context)
    capture_end = validate_host_diagnostics(root, row, "post", context)
    if capture_start >= capture_end:
        raise EvidenceError(
            f"{context} has a reversed or empty capture interval: "
            f"pre={capture_start.isoformat()} post={capture_end.isoformat()}"
        )
    workload_command = (
        "/opt/thekernel-tests/bin/thekernel-mm-performance "
        f"--iterations {row['iterations']} --vmas {row['live_vmas']} "
        f"--pin-iterations {row['pin_iterations']} "
        f"--pin-workers {row['pin_workers']}"
    )
    if measurement_mode == "product":
        expected_command = workload_command + " || exit 1\nexit\n"
    else:
        expected_command = "\n".join(
            (
                "echo mm_lock_stats=off > /proc/io_test_control || exit 1",
                "echo mm_lock_stats=reset > /proc/io_test_control || exit 1",
                "echo mm_lock_stats=on > /proc/io_test_control || exit 1",
                "echo asid_switch_stats=off > /proc/io_test_control || exit 1",
                "echo asid_switch_stats=reset > /proc/io_test_control || exit 1",
                "echo asid_switch_stats=on > /proc/io_test_control || exit 1",
                workload_command + " || exit 1",
                "mm_lock_off_attempt=0; until echo mm_lock_stats=off > "
                "/proc/io_test_control; do mm_lock_off_attempt="
                "$((mm_lock_off_attempt + 1)); "
                '[ "$mm_lock_off_attempt" -lt 64 ] || exit 1; done',
                "echo asid_switch_stats=off > /proc/io_test_control || exit 1",
                "cat /proc/mm_lock_stats || exit 1",
                "cat /proc/asid_switch_stats || exit 1",
                "cat /proc/pmu_capabilities || exit 1",
                "exit",
                "",
            )
        )
    try:
        command_text = commands_path.read_text(encoding="utf-8")
    except OSError as error:
        raise EvidenceError(f"cannot read {context} command artifact: {error}") from error
    if command_text != expected_command:
        raise EvidenceError(f"{context} command artifact does not match its workload fields")
    return (
        ManifestRecord(
            (arch, requested_cpus), row, capture_start, capture_end
        ),
        metrics_path,
    )


def expected_metric_count(manifest: ManifestRecord, metric: str) -> int:
    values = manifest.values
    iterations = int(values["iterations"], 10)
    pin_iterations = int(values["pin_iterations"], 10)
    pin_workers = int(values["pin_workers"], 10)
    return {
        "vma_scale": iterations,
        "mremap_latency": iterations * 2,
        "mremap_fixed_replace_latency": iterations,
        "mremap_disjoint_same_as_contention": iterations * 2,
        "mremap_file_duplicate_latency": iterations,
        "mremap_shared_anon_resize_latency": iterations * 2,
        "protect_touch_latency": iterations,
        "address_space_switch_ping_pong_latency": iterations,
        "direct_io_pin_proxy_throughput": pin_iterations,
        "direct_io_pin_proxy_same_as_contention": pin_iterations * pin_workers,
        "direct_io_pin_proxy_cross_as_contention": pin_iterations * pin_workers,
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
    requested_vmas: int | None
    fixture_vmas: int | None
    if metric in VMA_FIXTURE_METRICS:
        requested_vmas = parse_positive_int(
            row["requested_vmas"], "requested_vmas", context
        )
        fixture_vmas = parse_positive_int(
            row["fixture_vmas"], "fixture_vmas", context
        )
        manifest_vmas = int(manifest.values["live_vmas"], 10)
        if requested_vmas != manifest_vmas:
            raise EvidenceError(
                f"{context} metric={metric} requested_vmas mismatch: "
                f"manifest={manifest_vmas} actual={requested_vmas}"
            )
        if fixture_vmas != requested_vmas:
            raise EvidenceError(
                f"{context} metric={metric} fixture_vmas mismatch: "
                f"requested={requested_vmas} verified={fixture_vmas}"
            )
    else:
        requested_vmas = None
        if row["requested_vmas"] != "-" or row["fixture_vmas"] != "-":
            raise EvidenceError(
                f"{context} metric={metric} must use '-' for VMA fixture fields"
            )
        fixture_vmas = None
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
        requested_vmas,
        fixture_vmas,
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


def load_bundle(path: Path, *, allow_partial: bool = False) -> Bundle:
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

    run_keys = set(manifest)
    if allow_partial:
        if not run_keys or not run_keys.issubset(REQUIRED_RELEASE_RUN_KEYS):
            raise EvidenceError(
                "partial bundle run-key set must be a nonempty subset of "
                f"{sorted(REQUIRED_RELEASE_RUN_KEYS)!r}: actual={sorted(run_keys)!r}"
            )
    elif run_keys != REQUIRED_RELEASE_RUN_KEYS:
        raise EvidenceError(
            "release bundle run-key set mismatch: "
            f"expected={sorted(REQUIRED_RELEASE_RUN_KEYS)!r} "
            f"actual={sorted(run_keys)!r}; use --allow-partial only for triage"
        )

    capture_records = list(manifest.values())
    for previous, current in zip(capture_records, capture_records[1:]):
        if previous.capture_end >= current.capture_start:
            raise EvidenceError(
                "bundle run capture intervals overlap or are out of order: "
                f"previous={previous.key!r} end={previous.capture_end.isoformat()} "
                f"current={current.key!r} start={current.capture_start.isoformat()}"
            )

    uniform_fields = (
        "bundle_schema",
        "thekernel_commit",
        "thekernel_ax_commit",
        "thekernel_linux_abi_commit",
        "measurement_mode",
        "kernel_profile",
        "iterations",
        "live_vmas",
        "pin_iterations",
        "runner_fingerprint",
        "runner_contract_sha256",
        "host_cpu_selection",
        "host_cpu_class",
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

    host_reference: dict[int, dict[str, str]] = {}
    for key, record in manifest.items():
        reference = host_reference.setdefault(key[1], record.values)
        if record.values["host_cpu_set"] != reference["host_cpu_set"]:
            raise EvidenceError(
                "bundle host CPU set is not uniform for requested_cpus="
                f"{key[1]}"
            )
    previous: set[int] = set()
    for count in sorted(host_reference):
        current = set(parse_cpu_list(host_reference[count]["host_cpu_set"]))
        if not previous.issubset(current):
            raise EvidenceError(
                "bundle host CPU sets are not nested at requested_cpus="
                f"{count}"
            )
        previous = current

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
    return Bundle(
        root,
        manifest,
        metrics,
        sha256_file(manifest_path),
        capture_records[0].capture_start,
        capture_records[-1].capture_end,
    )


def require_product_regression_evidence(
    label: str, bundles: list[Bundle]
) -> None:
    for index, bundle in enumerate(bundles, start=1):
        modes = {
            record.values["measurement_mode"]
            for record in bundle.manifest.values()
        }
        if modes != {"product"}:
            actual = ",".join(sorted(modes))
            raise EvidenceError(
                f"{label} bundle {index} measurement_mode={actual!r} "
                "is not product regression evidence"
            )


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
        if p99 > 20:
            raise EvidenceError(
                f"policy metric={metric} may not weaken the 20 percent P99 ceiling"
            )
        if throughput is not None and throughput < 90:
            raise EvidenceError(
                f"policy metric={metric} may not weaken 90 percent throughput retention"
            )
        result[metric] = MetricPolicy(p99, throughput)
    return result


def load_stability_policy(path: Path) -> StabilityPolicy:
    try:
        with path.open("r", encoding="utf-8") as source:
            payload = json.load(source)
    except (OSError, json.JSONDecodeError) as error:
        raise EvidenceError(f"cannot read stability policy {path}: {error}") from error
    expected = {
        "schema",
        "minimum_pairs",
        "maximum_pairs",
        "maximum_pair_ratio_spread_percent",
    }
    if not isinstance(payload, dict) or set(payload) != expected:
        raise EvidenceError(
            f"stability policy must contain exactly {sorted(expected)!r}"
        )
    if payload["schema"] != STABILITY_POLICY_SCHEMA:
        raise EvidenceError(
            f"unsupported stability policy schema: {payload['schema']!r}"
        )
    values: dict[str, int] = {}
    for field in expected - {"schema"}:
        value = payload[field]
        if isinstance(value, bool) or not isinstance(value, int):
            raise EvidenceError(
                f"stability policy has invalid {field}: {value!r}"
            )
        values[field] = value
    minimum = values["minimum_pairs"]
    maximum = values["maximum_pairs"]
    spread = values["maximum_pair_ratio_spread_percent"]
    if minimum < 3 or minimum % 2 == 0:
        raise EvidenceError("stability policy minimum_pairs must be odd and at least 3")
    if maximum < minimum or maximum > 101 or maximum % 2 == 0:
        raise EvidenceError(
            "stability policy maximum_pairs must be odd, at least minimum_pairs, "
            "and at most 101"
        )
    if spread < 0 or spread > MAX_PAIR_RATIO_SPREAD_PERCENT:
        raise EvidenceError(
            "stability policy may not weaken the 20 percent pair-ratio "
            "spread ceiling"
        )
    return StabilityPolicy(minimum, maximum, spread)


def validate_output_destination(path: Path, bundles: Iterable[Bundle]) -> Path:
    try:
        destination = path.expanduser().resolve()
    except (OSError, RuntimeError) as error:
        raise EvidenceError(
            f"cannot resolve regression report destination {path}: {error}"
        ) from error
    for bundle in bundles:
        try:
            destination.relative_to(bundle.root)
        except ValueError:
            continue
        raise EvidenceError(
            "regression report destination must be outside every input "
            f"evidence bundle: output={destination} bundle={bundle.root}"
        )
    return destination


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
        "measurement_mode",
        "kernel_profile",
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
        "host_cpu_set",
        "host_cpu_selection",
        "host_cpu_class",
        "platform_class",
        "pmu_source",
        "cpu_model",
        "firmware_version",
        "cpu_freq_policy",
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


def validate_side_identity(label: str, bundles: list[Bundle]) -> None:
    reference = bundles[0]
    reference_commit = next(iter(reference.manifest.values())).values[
        "thekernel_commit"
    ]
    for index, bundle in enumerate(bundles[1:], start=2):
        compare_provenance(reference, bundle)
        commit = next(iter(bundle.manifest.values())).values["thekernel_commit"]
        if commit != reference_commit:
            raise EvidenceError(
                f"{label} series changes thekernel_commit at pair {index}: "
                f"expected={reference_commit!r} actual={commit!r}"
            )
        for key in sorted(reference.manifest):
            before = reference.manifest[key].values
            after = bundle.manifest[key].values
            for field in ("kernel_sha256", "kernel_size_bytes"):
                if before[field] != after[field]:
                    raise EvidenceError(
                        f"{label} series changes {field} at pair {index} "
                        f"run={key!r}: expected={before[field]!r} "
                        f"actual={after[field]!r}"
                    )


def validate_counterbalanced_capture_order(
    baselines: list[Bundle], candidates: list[Bundle]
) -> None:
    previous_pair_end: dt.datetime | None = None
    previous_orientation: str | None = None
    for index, (baseline, candidate) in enumerate(
        zip(baselines, candidates, strict=True), start=1
    ):
        if baseline.capture_end < candidate.capture_start:
            orientation = "baseline-first"
            pair_start = baseline.capture_start
            pair_end = candidate.capture_end
        elif candidate.capture_end < baseline.capture_start:
            orientation = "candidate-first"
            pair_start = candidate.capture_start
            pair_end = baseline.capture_end
        else:
            raise EvidenceError(
                "paired series was not captured as disjoint adjacent pairs: "
                f"baseline[{index}]={baseline.capture_start.isoformat()}.."
                f"{baseline.capture_end.isoformat()} candidate[{index}]="
                f"{candidate.capture_start.isoformat()}.."
                f"{candidate.capture_end.isoformat()}"
            )
        if previous_pair_end is not None and previous_pair_end >= pair_start:
            raise EvidenceError(
                "paired series was not captured in chronological adjacent-pair "
                f"order: pair[{index - 1}] end={previous_pair_end.isoformat()} "
                f"must precede pair[{index}] start={pair_start.isoformat()}"
            )
        if previous_orientation == orientation:
            raise EvidenceError(
                "paired series was not captured in counterbalanced order: "
                f"pair[{index - 1}] and pair[{index}] are both {orientation}"
            )
        previous_pair_end = pair_end
        previous_orientation = orientation


def ratio_compare(left: RatioSample, right: RatioSample) -> int:
    before = left.candidate * right.baseline
    after = right.candidate * left.baseline
    if before < after:
        return -1
    if before > after:
        return 1
    return (left.pair > right.pair) - (left.pair < right.pair)


def ratio_ppm(sample: RatioSample) -> str:
    return str(sample.candidate * 1_000_000 // sample.baseline)


def summarize_ratios(
    samples: list[RatioSample],
    *,
    context: str,
    stability: StabilityPolicy,
    check_stability: bool,
) -> tuple[RatioSample, RatioSample, RatioSample]:
    if any(sample.baseline <= 0 for sample in samples):
        raise EvidenceError(f"{context} has a zero baseline and no defined ratio")
    ordered = sorted(samples, key=cmp_to_key(ratio_compare))
    minimum = ordered[0]
    median = ordered[len(ordered) // 2]
    maximum = ordered[-1]
    if check_stability:
        spread_limit = 100 + stability.maximum_pair_ratio_spread_percent
        stable = (
            maximum.candidate * minimum.baseline * 100
            <= minimum.candidate * maximum.baseline * spread_limit
        )
        if not stable:
            raise EvidenceError(
                f"unstable paired series for {context}: "
                f"min_pair={minimum.pair} min_ratio_ppm={ratio_ppm(minimum)} "
                f"max_pair={maximum.pair} max_ratio_ppm={ratio_ppm(maximum)} "
                f"spread_limit_percent={stability.maximum_pair_ratio_spread_percent}"
            )
    return minimum, median, maximum


def paired_row(
    metric: MetricRecord,
    statistic: str,
    samples: list[RatioSample],
    *,
    threshold_percent: int | None,
    comparator: str,
    stability: StabilityPolicy,
) -> ReportRow:
    report_only = threshold_percent is None
    minimum, median, maximum = summarize_ratios(
        samples,
        context=f"{metric.key!r} {statistic}",
        stability=stability,
        check_stability=not report_only,
    )
    if report_only:
        result = "REPORT_ONLY"
    elif comparator == "<=":
        result = (
            "PASS"
            if median.candidate * 100 <= median.baseline * threshold_percent
            else "FAIL"
        )
    elif comparator == ">=":
        result = (
            "PASS"
            if median.candidate * 100 >= median.baseline * threshold_percent
            else "FAIL"
        )
    else:
        raise AssertionError(f"unsupported comparator: {comparator}")
    return ReportRow(
        metric.arch,
        metric.requested_cpus,
        metric.metric,
        statistic,
        "report_only" if report_only else "gate",
        len(samples),
        median.pair,
        median.baseline,
        median.candidate,
        0 if threshold_percent is None else threshold_percent,
        "-" if report_only else comparator,
        result,
        ratio_ppm(median),
        ratio_ppm(minimum),
        ratio_ppm(maximum),
    )


def compare_series(
    baselines: list[Bundle],
    candidates: list[Bundle],
    policy: dict[str, MetricPolicy],
    stability: StabilityPolicy,
) -> list[ReportRow]:
    pair_count = len(baselines)
    if pair_count != len(candidates):
        raise EvidenceError(
            f"paired series length mismatch: baseline={pair_count} "
            f"candidate={len(candidates)}"
        )
    if pair_count % 2 == 0:
        raise EvidenceError(f"paired series requires an odd pair count: {pair_count}")
    if pair_count < stability.minimum_pairs or pair_count > stability.maximum_pairs:
        raise EvidenceError(
            f"paired series count {pair_count} is outside stability policy "
            f"{stability.minimum_pairs}..{stability.maximum_pairs}"
        )
    receipts: dict[str, tuple[str, int]] = {}
    for label, bundles in (("baseline", baselines), ("candidate", candidates)):
        for index, bundle in enumerate(bundles, start=1):
            previous = receipts.get(bundle.receipt_sha256)
            if previous is not None:
                raise EvidenceError(
                    "paired series reuses one bundle receipt: "
                    f"first={previous!r} duplicate={(label, index)!r} "
                    f"sha256={bundle.receipt_sha256}"
                )
            receipts[bundle.receipt_sha256] = (label, index)
    validate_counterbalanced_capture_order(baselines, candidates)
    validate_side_identity("baseline", baselines)
    validate_side_identity("candidate", candidates)
    for index, (baseline, candidate) in enumerate(
        zip(baselines, candidates, strict=True), start=1
    ):
        try:
            compare_provenance(baseline, candidate)
        except EvidenceError as error:
            raise EvidenceError(f"pair {index} provenance mismatch: {error}") from error

    rows: list[ReportRow] = []
    arch_order = {"rv": 0, "la": 1}
    metric_order = {metric: index for index, metric in enumerate(EXPECTED_METRICS)}
    keys = sorted(
        baselines[0].metrics,
        key=lambda key: (arch_order[key[0]], key[1], metric_order[key[2]]),
    )
    for key in keys:
        metric = baselines[0].metrics[key]
        metric_policy = policy[metric.metric]
        p99_samples = [
            RatioSample(index, before.metrics[key].p99_ns, after.metrics[key].p99_ns)
            for index, (before, after) in enumerate(
                zip(baselines, candidates, strict=True), start=1
            )
        ]
        rows.append(
            paired_row(
                metric,
                "p99_ns",
                p99_samples,
                threshold_percent=100 + metric_policy.p99_max_regression_percent,
                comparator="<=",
                stability=stability,
            )
        )
        p999_samples = [
            RatioSample(index, before.metrics[key].p999_ns, after.metrics[key].p999_ns)
            for index, (before, after) in enumerate(
                zip(baselines, candidates, strict=True), start=1
            )
        ]
        rows.append(
            paired_row(
                metric,
                "p999_ns",
                p999_samples,
                threshold_percent=None,
                comparator="-",
                stability=stability,
            )
        )
        if metric.metric in PIN_METRICS:
            retained = metric_policy.throughput_min_retained_percent
            assert retained is not None
            throughput_samples: list[RatioSample] = []
            for index, (before, after) in enumerate(
                zip(baselines, candidates, strict=True), start=1
            ):
                baseline_value = before.metrics[key].throughput_bytes_per_sec
                candidate_value = after.metrics[key].throughput_bytes_per_sec
                assert baseline_value is not None and candidate_value is not None
                throughput_samples.append(
                    RatioSample(index, baseline_value, candidate_value)
                )
            rows.append(
                paired_row(
                    metric,
                    "throughput_bytes_per_sec",
                    throughput_samples,
                    threshold_percent=retained,
                    comparator=">=",
                    stability=stability,
                )
            )
    return rows


def write_report(
    path: Path, rows: Iterable[ReportRow], *, release_gate: bool
) -> None:
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
                        "release" if release_gate else "partial_triage",
                        "true" if release_gate else "false",
                        row.arch,
                        row.requested_cpus,
                        row.metric,
                        row.statistic,
                        row.mode,
                        row.pair_count,
                        row.median_pair,
                        row.baseline,
                        row.candidate,
                        row.threshold_percent if row.mode == "gate" else "-",
                        row.comparator,
                        row.result,
                        row.candidate_ratio_ppm,
                        row.pair_ratio_min_ppm,
                        row.pair_ratio_max_ppm,
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
        description="compare an odd paired series of TheKernel MM evidence bundles"
    )
    parser.add_argument("--baseline", type=Path, action="append", required=True)
    parser.add_argument("--candidate", type=Path, action="append", required=True)
    parser.add_argument("--policy", type=Path, required=True)
    parser.add_argument("--stability-policy", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument(
        "--allow-partial",
        action="store_true",
        help="accept a nonempty subset of the release matrix for triage only",
    )
    return parser


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        baselines = [
            load_bundle(path, allow_partial=args.allow_partial)
            for path in args.baseline
        ]
        candidates = [
            load_bundle(path, allow_partial=args.allow_partial)
            for path in args.candidate
        ]
        require_product_regression_evidence("baseline", baselines)
        require_product_regression_evidence("candidate", candidates)
        output = validate_output_destination(
            args.output, (*baselines, *candidates)
        )
        policy = load_policy(args.policy)
        stability = load_stability_policy(args.stability_policy)
        rows = compare_series(baselines, candidates, policy, stability)
        write_report(output, rows, release_gate=not args.allow_partial)
    except EvidenceError as error:
        print(f"compare-mm-performance: INVALID: {error}", file=sys.stderr)
        return 2
    failures = sum(row.result == "FAIL" for row in rows)
    report_only = sum(row.result == "REPORT_ONLY" for row in rows)
    print(
        "compare-mm-performance: "
        f"{'PARTIAL ' if args.allow_partial else ''}"
        f"{'REGRESSION' if failures else 'PASS'} "
        f"pairs={len(args.baseline)} "
        f"gates={len(rows) - report_only} failures={failures} "
        f"report_only={report_only} "
        f"release_gate={'false' if args.allow_partial else 'true'} "
        f"report={args.output}"
    )
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
