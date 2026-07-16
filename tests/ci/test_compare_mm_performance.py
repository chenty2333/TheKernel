#!/usr/bin/env python3
"""Host tests for portable MM evidence and relative regression policy."""

from __future__ import annotations

import csv
import datetime as dt
import hashlib
import json
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from typing import Any, Callable


REPO_ROOT = Path(__file__).resolve().parents[2]
COMPARATOR = REPO_ROOT / "scripts" / "ci" / "compare-mm-performance.py"
DEFAULT_POLICY = (
    REPO_ROOT
    / "scripts"
    / "ci"
    / "nightly"
    / "mm-performance-regression-policy.json"
)
DEFAULT_STABILITY_POLICY = (
    REPO_ROOT
    / "scripts"
    / "ci"
    / "nightly"
    / "mm-performance-stability-policy.json"
)
MANIFEST_COLUMNS = (
    "bundle_schema",
    "thekernel_commit",
    "thekernel_ax_commit",
    "thekernel_linux_abi_commit",
    "arch",
    "requested_cpus",
    "online_cpus",
    "iterations",
    "live_vmas",
    "pin_iterations",
    "pin_workers",
    "kernel_sha256",
    "kernel_size_bytes",
    "rootfs_sha256",
    "qemu_binary",
    "qemu_version",
    "qemu_sha256",
    "runner_fingerprint",
    "runner_contract_sha256",
    "host_cpu_set",
    "host_cpu_selection",
    "host_cpu_class",
    "kernel_artifact",
    "metrics_artifact",
    "metrics_sha256",
    "metrics_size_bytes",
    "commands",
    "commands_sha256",
    "commands_size_bytes",
    "qemu_log",
    "qemu_log_sha256",
    "qemu_log_size_bytes",
    "host_diagnostics_pre",
    "host_diagnostics_pre_sha256",
    "host_diagnostics_pre_size_bytes",
    "host_diagnostics_post",
    "host_diagnostics_post_sha256",
    "host_diagnostics_post_size_bytes",
)
METRIC_COLUMNS = (
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
EXPECTED_METRICS = (
    "vma_scale",
    "mremap_latency",
    "protect_touch_latency",
    "pin_throughput",
    "pin_contention",
)
PIN_METRICS = frozenset({"pin_throughput", "pin_contention"})


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def write_tsv(path: Path, columns: tuple[str, ...], rows: list[dict[str, str]]) -> None:
    with path.open("w", encoding="utf-8", newline="") as output:
        writer = csv.DictWriter(
            output, fieldnames=columns, delimiter="\t", lineterminator="\n"
        )
        writer.writeheader()
        writer.writerows(rows)


def read_tsv(path: Path) -> tuple[tuple[str, ...], list[dict[str, str]]]:
    with path.open("r", encoding="utf-8", newline="") as source:
        reader = csv.DictReader(source, delimiter="\t")
        assert reader.fieldnames is not None
        return tuple(reader.fieldnames), list(reader)


def expected_count(metric: str, iterations: int, pin_iterations: int, pin_workers: int) -> int:
    return {
        "vma_scale": iterations,
        "mremap_latency": iterations * 2,
        "protect_touch_latency": iterations,
        "pin_throughput": pin_iterations,
        "pin_contention": pin_iterations * pin_workers,
    }[metric]


def make_bundle(
    root: Path,
    *,
    manifest_overrides: dict[str, str] | None = None,
    kernel_content: bytes = b"fixture-kernel\n",
) -> None:
    root.mkdir(parents=True)
    run = root / "rv-4cpu"
    run.mkdir()
    values = {
        "bundle_schema": "thekernel-mm-performance-bundle-v3",
        "thekernel_commit": "1" * 40,
        "thekernel_ax_commit": "2" * 40,
        "thekernel_linux_abi_commit": "3" * 40,
        "arch": "rv",
        "requested_cpus": "4",
        "online_cpus": "4",
        "iterations": "100",
        "live_vmas": "512",
        "pin_iterations": "25",
        "pin_workers": "4",
        "rootfs_sha256": "4" * 64,
        "qemu_binary": "qemu-system-riscv64",
        "qemu_version": "QEMU emulator version fixture",
        "qemu_sha256": "5" * 64,
        "runner_fingerprint": f"auto-sha256:{'6' * 64}",
        "runner_contract_sha256": "7" * 64,
        "host_cpu_set": "0-3",
        "host_cpu_selection": "auto-homogeneous-v1",
        "host_cpu_class": "package:0,max_freq_khz:3700000",
        "kernel_artifact": "rv-4cpu/kernel",
        "metrics_artifact": "rv-4cpu/mm-performance.tsv",
        "commands": "rv-4cpu.commands",
        "qemu_log": "rv-4cpu/qemu.log",
        "host_diagnostics_pre": "rv-4cpu/host-pre.tsv",
        "host_diagnostics_post": "rv-4cpu/host-post.tsv",
    }
    if manifest_overrides:
        values.update(manifest_overrides)
    timestamp_fraction = int(
        hashlib.sha256(str(root).encode("utf-8")).hexdigest()[:12], 16
    ) % 1_000_000
    timestamps = {
        "pre": f"2026-01-01T00:00:00.{timestamp_fraction:06d}+00:00",
        "post": f"2026-01-01T00:00:01.{timestamp_fraction:06d}+00:00",
    }
    iterations = int(values["iterations"])
    pin_iterations = int(values["pin_iterations"])
    pin_workers = int(values["pin_workers"])

    kernel = run / "kernel"
    kernel.write_bytes(kernel_content)
    values["kernel_sha256"] = sha256(kernel)
    values["kernel_size_bytes"] = str(kernel.stat().st_size)

    commands = root / "rv-4cpu.commands"
    commands.write_text(
        "/opt/thekernel-tests/bin/thekernel-mm-performance "
        f"--iterations {values['iterations']} --vmas {values['live_vmas']} "
        f"--pin-iterations {values['pin_iterations']} "
        f"--pin-workers {values['pin_workers']}; exit\n",
        encoding="utf-8",
    )
    values["commands_sha256"] = sha256(commands)
    values["commands_size_bytes"] = str(commands.stat().st_size)

    qemu_log = run / "qemu.log"
    qemu_log.write_text("fixture guest log\nSystem is shutting down\n", encoding="utf-8")
    values["qemu_log_sha256"] = sha256(qemu_log)
    values["qemu_log_size_bytes"] = str(qemu_log.stat().st_size)

    for phase in ("pre", "post"):
        diagnostics = run / f"host-{phase}.tsv"
        write_tsv(
            diagnostics,
            ("key", "value"),
            [
                {"key": "schema", "value": "thekernel-mm-performance-host-diagnostics-v1"},
                {"key": "phase", "value": phase},
                {"key": "timestamp_utc", "value": timestamps[phase]},
                {"key": "selected_cpu_set", "value": values["host_cpu_set"]},
                {
                    "key": "host_cpu_selection",
                    "value": values["host_cpu_selection"],
                },
                {"key": "host_cpu_class", "value": values["host_cpu_class"]},
                {"key": "online_cpu_set", "value": "0-3"},
                {"key": "loadavg", "value": "0.00 0.00 0.00 1/1 1"},
                {"key": "psi.cpu", "value": "missing"},
                {"key": "cgroup.cpu_stat", "value": "missing"},
                *[
                    {"key": f"cpu.{cpu}.{field}", "value": value}
                    for cpu in range(int(values["requested_cpus"]))
                    for field, value in (
                        ("online", "1"),
                        ("package", "0"),
                        ("max_freq_khz", "3700000"),
                        ("current_freq_khz", "missing"),
                    )
                ],
            ],
        )
        values[f"host_diagnostics_{phase}_sha256"] = sha256(diagnostics)
        values[f"host_diagnostics_{phase}_size_bytes"] = str(
            diagnostics.stat().st_size
        )

    metric_rows: list[dict[str, str]] = []
    for metric in EXPECTED_METRICS:
        metric_rows.append(
            {
                "arch": values["arch"],
                "requested_cpus": values["requested_cpus"],
                "online_cpus": values["online_cpus"],
                "metric": metric,
                "status": "ok",
                "count": str(
                    expected_count(metric, iterations, pin_iterations, pin_workers)
                ),
                "p50_ns": "50",
                "p99_ns": "100",
                "p999_ns": "200",
                "throughput_bytes_per_sec": "1000" if metric in PIN_METRICS else "-",
                "reason": "-",
                "errno": "-",
            }
        )
    metrics = run / "mm-performance.tsv"
    matrix = root / "mm-performance.tsv"
    write_tsv(metrics, METRIC_COLUMNS, metric_rows)
    write_tsv(matrix, METRIC_COLUMNS, metric_rows)
    values["metrics_sha256"] = sha256(metrics)
    values["metrics_size_bytes"] = str(metrics.stat().st_size)
    write_tsv(root / "mm-performance-manifest.tsv", MANIFEST_COLUMNS, [values])


def mutate_manifest(root: Path, mutator: Callable[[dict[str, str]], None]) -> None:
    path = root / "mm-performance-manifest.tsv"
    columns, rows = read_tsv(path)
    mutator(rows[0])
    write_tsv(path, columns, rows)


def clone_bundle(source: Path, destination: Path, marker: str) -> None:
    shutil.copytree(source, destination, symlinks=True)
    fraction = int(
        hashlib.sha256(marker.encode("utf-8")).hexdigest()[:12], 16
    ) % 1_000_000
    start = dt.datetime(2026, 1, 1, tzinfo=dt.UTC).replace(microsecond=fraction)
    set_capture_interval(destination, start, start + dt.timedelta(seconds=1))


def set_capture_interval(root: Path, start: dt.datetime, end: dt.datetime) -> None:
    updates: dict[str, str] = {}
    for phase, timestamp in (("pre", start), ("post", end)):
        diagnostics = root / "rv-4cpu" / f"host-{phase}.tsv"
        columns, rows = read_tsv(diagnostics)
        next(row for row in rows if row["key"] == "timestamp_utc")[
            "value"
        ] = timestamp.isoformat(timespec="microseconds")
        write_tsv(diagnostics, columns, rows)
        updates[f"host_diagnostics_{phase}_sha256"] = sha256(diagnostics)
        updates[f"host_diagnostics_{phase}_size_bytes"] = str(
            diagnostics.stat().st_size
        )
    mutate_manifest(root, lambda row: row.update(updates))


def set_raw_capture_timestamp(root: Path, phase: str, value: str) -> None:
    diagnostics = root / "rv-4cpu" / f"host-{phase}.tsv"
    columns, rows = read_tsv(diagnostics)
    next(row for row in rows if row["key"] == "timestamp_utc")["value"] = value
    write_tsv(diagnostics, columns, rows)
    mutate_manifest(
        root,
        lambda row: row.update(
            {
                f"host_diagnostics_{phase}_sha256": sha256(diagnostics),
                f"host_diagnostics_{phase}_size_bytes": str(
                    diagnostics.stat().st_size
                ),
            }
        ),
    )


def mutate_metrics(
    root: Path,
    mutator: Callable[[list[dict[str, str]]], None],
    *,
    update_matrix: bool = True,
) -> None:
    metrics = root / "rv-4cpu" / "mm-performance.tsv"
    columns, rows = read_tsv(metrics)
    mutator(rows)
    write_tsv(metrics, columns, rows)
    if update_matrix:
        write_tsv(root / "mm-performance.tsv", columns, rows)
    mutate_manifest(
        root,
        lambda row: row.update(
            {
                "metrics_sha256": sha256(metrics),
                "metrics_size_bytes": str(metrics.stat().st_size),
            }
        ),
    )


def set_metric(root: Path, metric_name: str, **changes: str | int) -> None:
    def apply(rows: list[dict[str, str]]) -> None:
        row = next(row for row in rows if row["metric"] == metric_name)
        row.update({key: str(value) for key, value in changes.items()})

    mutate_metrics(root, apply)


class CompareMmPerformanceTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.policy = self.root / "policy.json"
        shutil.copy2(DEFAULT_POLICY, self.policy)
        self.stability_policy = self.root / "stability-policy.json"
        shutil.copy2(DEFAULT_STABILITY_POLICY, self.stability_policy)
        self.comparison_counter = 0
        self.series_counter = 0

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def bundle(self, name: str, **kwargs: Any) -> Path:
        path = self.root / name
        make_bundle(path, **kwargs)
        return path

    def compare(
        self,
        baseline: Path,
        candidate: Path,
        *,
        policy: Path | None = None,
        repetitions: int = 3,
    ) -> tuple[subprocess.CompletedProcess[str], Path]:
        self.comparison_counter += 1
        series_root = self.root / f"comparison-{self.comparison_counter}"
        series_root.mkdir()
        baselines: list[Path] = []
        candidates: list[Path] = []
        for index in range(repetitions):
            baseline_copy = series_root / f"baseline-{index}"
            candidate_copy = series_root / f"candidate-{index}"
            clone_bundle(
                baseline,
                baseline_copy,
                f"comparison-{self.comparison_counter}-baseline-{index}",
            )
            clone_bundle(
                candidate,
                candidate_copy,
                f"comparison-{self.comparison_counter}-candidate-{index}",
            )
            baselines.append(baseline_copy)
            candidates.append(candidate_copy)
        return self.compare_series(
            baselines,
            candidates,
            policy=policy,
        )

    def compare_series(
        self,
        baselines: list[Path],
        candidates: list[Path],
        *,
        policy: Path | None = None,
        stability_policy: Path | None = None,
        normalize_timestamps: bool = True,
    ) -> tuple[subprocess.CompletedProcess[str], Path]:
        self.series_counter += 1
        if normalize_timestamps:
            base = dt.datetime(2026, 1, 1, tzinfo=dt.UTC) + dt.timedelta(
                days=self.series_counter
            )
            for index, baseline in enumerate(baselines):
                start = base + dt.timedelta(seconds=index * 4)
                set_capture_interval(
                    baseline, start, start + dt.timedelta(seconds=1)
                )
            for index, candidate in enumerate(candidates):
                start = base + dt.timedelta(seconds=index * 4 + 2)
                set_capture_interval(
                    candidate, start, start + dt.timedelta(seconds=1)
                )
        report = self.root / "report.tsv"
        report.unlink(missing_ok=True)
        arguments = [sys.executable, str(COMPARATOR)]
        for baseline in baselines:
            arguments.extend(("--baseline", str(baseline)))
        for candidate in candidates:
            arguments.extend(("--candidate", str(candidate)))
        arguments.extend(
            (
                "--policy",
                str(policy or self.policy),
                "--stability-policy",
                str(stability_policy or self.stability_policy),
                "--output",
                str(report),
            )
        )
        result = subprocess.run(
            arguments,
            cwd=REPO_ROOT,
            check=False,
            capture_output=True,
            text=True,
        )
        return result, report

    def report_rows(self, report: Path) -> list[dict[str, str]]:
        _, rows = read_tsv(report)
        return rows

    def test_equal_and_changed_kernel_bundles_are_comparable(self) -> None:
        baseline = self.bundle("baseline")
        candidate = self.bundle(
            "candidate",
            manifest_overrides={"thekernel_commit": "8" * 40},
            kernel_content=b"candidate-kernel\n",
        )

        result, report = self.compare(baseline, candidate)

        self.assertEqual(result.returncode, 0, result.stderr)
        rows = self.report_rows(report)
        self.assertEqual(len(rows), 12)
        self.assertEqual(sum(row["result"] == "PASS" for row in rows), 7)
        self.assertEqual(sum(row["result"] == "REPORT_ONLY" for row in rows), 5)

    def test_p99_relative_boundary_uses_exact_integer_arithmetic(self) -> None:
        baseline = self.bundle("baseline")
        candidate = self.bundle("candidate")
        set_metric(candidate, "vma_scale", p99_ns=120)

        result, _ = self.compare(baseline, candidate)
        self.assertEqual(result.returncode, 0, result.stderr)

        set_metric(candidate, "vma_scale", p99_ns=121)
        result, report = self.compare(baseline, candidate)
        self.assertEqual(result.returncode, 1, result.stderr)
        row = next(
            row
            for row in self.report_rows(report)
            if row["metric"] == "vma_scale" and row["statistic"] == "p99_ns"
        )
        self.assertEqual(row["result"], "FAIL")
        self.assertEqual(row["threshold_percent"], "120")

    def test_pin_throughput_relative_boundary_is_gated(self) -> None:
        baseline = self.bundle("baseline")
        candidate = self.bundle("candidate")
        set_metric(candidate, "pin_throughput", throughput_bytes_per_sec=900)

        result, _ = self.compare(baseline, candidate)
        self.assertEqual(result.returncode, 0, result.stderr)

        set_metric(candidate, "pin_throughput", throughput_bytes_per_sec=899)
        result, report = self.compare(baseline, candidate)
        self.assertEqual(result.returncode, 1, result.stderr)
        row = next(
            row
            for row in self.report_rows(report)
            if row["metric"] == "pin_throughput"
            and row["statistic"] == "throughput_bytes_per_sec"
        )
        self.assertEqual(row["result"], "FAIL")
        self.assertEqual(row["threshold_percent"], "90")

    def test_p999_is_report_only_until_policy_enables_it(self) -> None:
        baseline = self.bundle("baseline")
        candidate = self.bundle("candidate")
        set_metric(candidate, "vma_scale", p999_ns=1000000)

        result, report = self.compare(baseline, candidate)

        self.assertEqual(result.returncode, 0, result.stderr)
        row = next(
            row
            for row in self.report_rows(report)
            if row["metric"] == "vma_scale" and row["statistic"] == "p999_ns"
        )
        self.assertEqual(row["mode"], "report_only")
        self.assertEqual(row["result"], "REPORT_ONLY")

    def test_policy_cannot_enable_p999_hard_gate(self) -> None:
        payload = json.loads(self.policy.read_text(encoding="utf-8"))
        payload["metrics"]["vma_scale"]["p999_max_regression_percent"] = 20
        self.policy.write_text(json.dumps(payload), encoding="utf-8")
        baseline = self.bundle("baseline")
        candidate = self.bundle("candidate")
        result, report = self.compare(baseline, candidate)
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn("must contain exactly", result.stderr)
        self.assertFalse(report.exists())

    def test_large_integer_boundary_does_not_round_through_float(self) -> None:
        baseline = self.bundle("baseline")
        candidate = self.bundle("candidate")
        large = 9_007_199_254_740_995
        set_metric(
            baseline,
            "vma_scale",
            p50_ns=large,
            p99_ns=large,
            p999_ns=large * 2,
        )
        exact_boundary = large * 120 // 100
        set_metric(
            candidate,
            "vma_scale",
            p50_ns=large,
            p99_ns=exact_boundary,
            p999_ns=large * 2,
        )

        result, _ = self.compare(baseline, candidate)
        self.assertEqual(result.returncode, 0, result.stderr)

        set_metric(candidate, "vma_scale", p99_ns=exact_boundary + 1)
        result, _ = self.compare(baseline, candidate)
        self.assertEqual(result.returncode, 1, result.stderr)

    def test_dependency_rootfs_qemu_and_runner_drift_are_rejected(self) -> None:
        baseline = self.bundle("baseline")
        cases = {
            "thekernel_ax_commit": "9" * 40,
            "thekernel_linux_abi_commit": "a" * 40,
            "rootfs_sha256": "b" * 64,
            "qemu_version": "QEMU emulator version changed",
            "qemu_sha256": "c" * 64,
            "runner_fingerprint": f"auto-sha256:{'d' * 64}",
            "runner_contract_sha256": "e" * 64,
        }
        for index, (field, value) in enumerate(cases.items()):
            with self.subTest(field=field):
                candidate = self.bundle(
                    f"candidate-{index}", manifest_overrides={field: value}
                )
                result, report = self.compare(baseline, candidate)
                self.assertEqual(result.returncode, 2)
                self.assertIn(f"{field} differs", result.stderr)
                self.assertFalse(report.exists())

    def test_workload_and_run_key_drift_are_rejected(self) -> None:
        baseline = self.bundle("baseline")
        workload = self.bundle(
            "workload", manifest_overrides={"iterations": "101"}
        )
        result, _ = self.compare(baseline, workload)
        self.assertEqual(result.returncode, 2)
        self.assertIn("iterations differs", result.stderr)

        topology = self.bundle(
            "topology",
            manifest_overrides={
                "requested_cpus": "8",
                "online_cpus": "8",
                "pin_workers": "8",
                "host_cpu_set": "0-7",
            },
        )
        result, _ = self.compare(baseline, topology)
        self.assertEqual(result.returncode, 2)
        self.assertIn("run-key set mismatch", result.stderr)

    def test_invalid_topology_and_metric_count_are_rejected(self) -> None:
        baseline = self.bundle("baseline")
        topology = self.bundle(
            "topology", manifest_overrides={"online_cpus": "3"}
        )
        result, _ = self.compare(baseline, topology)
        self.assertEqual(result.returncode, 2)
        self.assertIn("invalid CPU topology", result.stderr)

        count = self.bundle("count")
        set_metric(count, "mremap_latency", count=199)
        result, _ = self.compare(baseline, count)
        self.assertEqual(result.returncode, 2)
        self.assertIn("count mismatch", result.stderr)

    def test_duplicate_and_missing_metric_keys_are_rejected(self) -> None:
        baseline = self.bundle("baseline")
        duplicate = self.bundle("duplicate")
        mutate_metrics(duplicate, lambda rows: rows.append(dict(rows[0])))
        result, _ = self.compare(baseline, duplicate)
        self.assertEqual(result.returncode, 2)
        self.assertIn("duplicate metric", result.stderr)

        missing = self.bundle("missing")
        mutate_metrics(
            missing,
            lambda rows: rows.__setitem__(slice(None), rows[1:]),
        )
        result, _ = self.compare(baseline, missing)
        self.assertEqual(result.returncode, 2)
        self.assertIn("metric set mismatch", result.stderr)

    def test_absolute_parent_and_symlink_escape_paths_are_rejected(self) -> None:
        baseline = self.bundle("baseline")

        absolute = self.bundle("absolute")
        absolute_metrics = absolute / "rv-4cpu" / "mm-performance.tsv"
        mutate_manifest(
            absolute,
            lambda row: row.update({"metrics_artifact": str(absolute_metrics)}),
        )
        result, _ = self.compare(baseline, absolute)
        self.assertEqual(result.returncode, 2)
        self.assertIn("normalized relative path", result.stderr)

        parent = self.bundle("parent")
        mutate_manifest(
            parent,
            lambda row: row.update(
                {"metrics_artifact": "rv-4cpu/../rv-4cpu/mm-performance.tsv"}
            ),
        )
        result, _ = self.compare(baseline, parent)
        self.assertEqual(result.returncode, 2)
        self.assertIn("normalized relative path", result.stderr)

        symlink = self.bundle("symlink")
        outside = self.root / "outside-metrics.tsv"
        shutil.copy2(symlink / "rv-4cpu" / "mm-performance.tsv", outside)
        linked = symlink / "rv-4cpu" / "mm-performance.tsv"
        linked.unlink()
        linked.symlink_to(outside)
        result, _ = self.compare(baseline, symlink)
        self.assertEqual(result.returncode, 2)
        self.assertIn("escapes the evidence bundle", result.stderr)

    def test_missing_and_hash_mismatched_artifacts_are_rejected(self) -> None:
        baseline = self.bundle("baseline")
        missing = self.bundle("missing")
        (missing / "rv-4cpu" / "qemu.log").unlink()
        result, _ = self.compare(baseline, missing)
        self.assertEqual(result.returncode, 2)
        self.assertIn("missing or inaccessible", result.stderr)

        corrupt = self.bundle("corrupt")
        (corrupt / "rv-4cpu" / "kernel").write_bytes(b"corrupt\n")
        result, _ = self.compare(baseline, corrupt)
        self.assertEqual(result.returncode, 2)
        self.assertRegex(result.stderr, r"kernel_artifact (size|SHA-256) mismatch")

    def test_bundle_remains_valid_after_copying_to_a_new_directory(self) -> None:
        baseline = self.bundle("baseline")
        copied = self.root / "relocated" / "copied-bundle"
        shutil.copytree(baseline, copied)

        result, _ = self.compare(baseline, copied)

        self.assertEqual(result.returncode, 0, result.stderr)

    def test_metric_row_order_is_not_comparison_identity(self) -> None:
        baseline = self.bundle("baseline")
        candidate = self.bundle("candidate")
        mutate_metrics(candidate, lambda rows: rows.reverse())

        result, _ = self.compare(baseline, candidate)

        self.assertEqual(result.returncode, 0, result.stderr)

    def test_series_requires_at_least_three_and_an_odd_pair_count(self) -> None:
        baseline = self.bundle("baseline")
        candidate = self.bundle("candidate")

        result, _ = self.compare(baseline, candidate, repetitions=1)
        self.assertEqual(result.returncode, 2)
        self.assertIn("outside stability policy", result.stderr)

        result, _ = self.compare(baseline, candidate, repetitions=2)
        self.assertEqual(result.returncode, 2)
        self.assertIn("odd pair count", result.stderr)

    def test_exact_ratio_median_uses_three_distinct_pairs(self) -> None:
        baselines = [self.bundle(f"baseline-{index}") for index in range(3)]
        candidates = [self.bundle(f"candidate-{index}") for index in range(3)]
        for candidate, value in zip(candidates, (100, 119, 120), strict=True):
            set_metric(candidate, "vma_scale", p99_ns=value)

        result, report = self.compare_series(baselines, candidates)

        self.assertEqual(result.returncode, 0, result.stderr)
        row = next(
            row
            for row in self.report_rows(report)
            if row["metric"] == "vma_scale" and row["statistic"] == "p99_ns"
        )
        self.assertEqual(row["pair_count"], "3")
        self.assertEqual(row["median_pair"], "2")
        self.assertEqual(row["candidate_ratio_ppm"], "1190000")
        self.assertEqual(row["pair_ratio_min_ppm"], "1000000")
        self.assertEqual(row["pair_ratio_max_ppm"], "1200000")

        result, report = self.compare_series(
            baselines, [candidates[1], candidates[0], candidates[2]]
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        row = next(
            row
            for row in self.report_rows(report)
            if row["metric"] == "vma_scale" and row["statistic"] == "p99_ns"
        )
        self.assertEqual(row["median_pair"], "1")

    def test_stable_median_regression_returns_one(self) -> None:
        baselines = [self.bundle(f"baseline-{index}") for index in range(3)]
        candidates = [self.bundle(f"candidate-{index}") for index in range(3)]
        for candidate, value in zip(candidates, (120, 121, 121), strict=True):
            set_metric(candidate, "vma_scale", p99_ns=value)

        result, report = self.compare_series(baselines, candidates)

        self.assertEqual(result.returncode, 1, result.stderr)
        row = next(
            row
            for row in self.report_rows(report)
            if row["metric"] == "vma_scale" and row["statistic"] == "p99_ns"
        )
        self.assertEqual(row["result"], "FAIL")
        self.assertEqual(row["candidate_ratio_ppm"], "1210000")

    def test_noisy_pair_ratios_return_two_without_a_report(self) -> None:
        baselines = [self.bundle(f"baseline-{index}") for index in range(3)]
        candidates = [self.bundle(f"candidate-{index}") for index in range(3)]
        for candidate, value in zip(candidates, (90, 100, 120), strict=True):
            set_metric(candidate, "vma_scale", p99_ns=value)

        result, report = self.compare_series(baselines, candidates)

        self.assertEqual(result.returncode, 2)
        self.assertIn("unstable paired series", result.stderr)
        self.assertFalse(report.exists())

    def test_missing_and_duplicate_pair_receipts_are_rejected(self) -> None:
        baselines = [self.bundle(f"baseline-{index}") for index in range(3)]
        candidates = [self.bundle(f"candidate-{index}") for index in range(3)]

        result, report = self.compare_series(baselines, candidates[:2])
        self.assertEqual(result.returncode, 2)
        self.assertIn("length mismatch", result.stderr)
        self.assertFalse(report.exists())

        result, report = self.compare_series(
            [baselines[0], baselines[0], baselines[2]], candidates
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn("reuses one bundle receipt", result.stderr)
        self.assertFalse(report.exists())

    def test_hashed_capture_intervals_prove_actual_alternating_order(self) -> None:
        baselines = [self.bundle(f"baseline-{index}") for index in range(3)]
        candidates = [self.bundle(f"candidate-{index}") for index in range(3)]
        base = dt.datetime(2026, 3, 1, tzinfo=dt.UTC)
        for index, (baseline, candidate) in enumerate(
            zip(baselines, candidates, strict=True)
        ):
            pair_start = base + dt.timedelta(seconds=index * 4)
            baseline_end = pair_start + dt.timedelta(seconds=1)
            set_capture_interval(baseline, pair_start, baseline_end)
            set_capture_interval(
                candidate,
                baseline_end + dt.timedelta(microseconds=1),
                pair_start + dt.timedelta(seconds=3),
            )

        result, _ = self.compare_series(
            baselines, candidates, normalize_timestamps=False
        )
        self.assertEqual(result.returncode, 0, result.stderr)

        result, report = self.compare_series(
            baselines,
            [candidates[1], candidates[0], candidates[2]],
            normalize_timestamps=False,
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn("strict alternating order", result.stderr)
        self.assertFalse(report.exists())

        set_capture_interval(
            candidates[0],
            base + dt.timedelta(seconds=1),
            base + dt.timedelta(seconds=3),
        )
        result, report = self.compare_series(
            baselines, candidates, normalize_timestamps=False
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn("strict alternating order", result.stderr)
        self.assertFalse(report.exists())

    def test_capture_timestamp_and_interval_are_strictly_validated(self) -> None:
        baselines = [self.bundle(f"baseline-{index}") for index in range(3)]
        candidates = [self.bundle(f"candidate-{index}") for index in range(3)]
        self.compare_series(baselines, candidates)

        set_raw_capture_timestamp(
            baselines[0], "pre", "2026-01-01 00:00:00+00:00"
        )
        result, report = self.compare_series(
            baselines, candidates, normalize_timestamps=False
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn("not strict RFC3339 UTC", result.stderr)
        self.assertFalse(report.exists())

        base = dt.datetime(2026, 4, 1, tzinfo=dt.UTC)
        set_capture_interval(
            baselines[0], base + dt.timedelta(seconds=1), base
        )
        result, report = self.compare_series(
            baselines, candidates, normalize_timestamps=False
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn("reversed or empty capture interval", result.stderr)
        self.assertFalse(report.exists())

    def test_each_side_requires_one_commit_and_kernel_hash(self) -> None:
        baselines = [
            self.bundle("baseline-0"),
            self.bundle(
                "baseline-1", manifest_overrides={"thekernel_commit": "8" * 40}
            ),
            self.bundle("baseline-2"),
        ]
        candidates = [self.bundle(f"candidate-{index}") for index in range(3)]

        result, _ = self.compare_series(baselines, candidates)
        self.assertEqual(result.returncode, 2)
        self.assertIn("baseline series changes thekernel_commit", result.stderr)

        baselines = [
            self.bundle("kernel-baseline-0"),
            self.bundle("kernel-baseline-1", kernel_content=b"other-kernel\n"),
            self.bundle("kernel-baseline-2"),
        ]
        result, _ = self.compare_series(baselines, candidates)
        self.assertEqual(result.returncode, 2)
        self.assertIn("baseline series changes kernel_sha256", result.stderr)

    def test_policy_cannot_weaken_hard_regression_limits(self) -> None:
        baseline = self.bundle("baseline")
        candidate = self.bundle("candidate")
        payload = json.loads(self.policy.read_text(encoding="utf-8"))
        payload["metrics"]["vma_scale"]["p99_max_regression_percent"] = 21
        self.policy.write_text(json.dumps(payload), encoding="utf-8")
        result, _ = self.compare(baseline, candidate)
        self.assertEqual(result.returncode, 2)
        self.assertIn("20 percent P99 ceiling", result.stderr)

        shutil.copy2(DEFAULT_POLICY, self.policy)
        payload = json.loads(self.policy.read_text(encoding="utf-8"))
        payload["metrics"]["pin_throughput"][
            "throughput_min_retained_percent"
        ] = 89
        self.policy.write_text(json.dumps(payload), encoding="utf-8")
        result, _ = self.compare(baseline, candidate)
        self.assertEqual(result.returncode, 2)
        self.assertIn("90 percent throughput retention", result.stderr)

    def test_host_diagnostics_reject_unsafe_fields(self) -> None:
        baseline = self.bundle("baseline")
        candidate = self.bundle("candidate")
        diagnostics = candidate / "rv-4cpu" / "host-pre.tsv"
        columns, rows = read_tsv(diagnostics)
        rows.append({"key": "hostname", "value": "private-host"})
        write_tsv(diagnostics, columns, rows)
        mutate_manifest(
            candidate,
            lambda row: row.update(
                {
                    "host_diagnostics_pre_sha256": sha256(diagnostics),
                    "host_diagnostics_pre_size_bytes": str(diagnostics.stat().st_size),
                }
            ),
        )

        result, report = self.compare(baseline, candidate)

        self.assertEqual(result.returncode, 2)
        self.assertIn("contains unsafe key", result.stderr)
        self.assertFalse(report.exists())

    def test_v1_and_v2_manifests_are_not_silently_upgraded(self) -> None:
        baseline = self.bundle("baseline")
        for version in ("v1", "v2"):
            with self.subTest(version=version):
                candidate = self.bundle(f"candidate-{version}")
                mutate_manifest(
                    candidate,
                    lambda row: row.update(
                        {
                            "bundle_schema": f"thekernel-mm-performance-bundle-{version}",
                            "kernel_artifact": (
                                "/workspace/rv-4cpu/kernel"
                                if version == "v1"
                                else row["kernel_artifact"]
                            ),
                        }
                    ),
                )

                result, report = self.compare(baseline, candidate)

                self.assertEqual(result.returncode, 2)
                self.assertIn("unsupported bundle_schema", result.stderr)
                self.assertFalse(report.exists())


if __name__ == "__main__":
    unittest.main()
