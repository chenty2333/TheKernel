#!/usr/bin/env python3
"""Host tests for portable MM evidence and relative regression policy."""

from __future__ import annotations

import csv
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
        "bundle_schema": "thekernel-mm-performance-bundle-v2",
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
        "kernel_artifact": "rv-4cpu/kernel",
        "metrics_artifact": "rv-4cpu/mm-performance.tsv",
        "commands": "rv-4cpu.commands",
        "qemu_log": "rv-4cpu/qemu.log",
    }
    if manifest_overrides:
        values.update(manifest_overrides)
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

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def bundle(self, name: str, **kwargs: Any) -> Path:
        path = self.root / name
        make_bundle(path, **kwargs)
        return path

    def compare(
        self, baseline: Path, candidate: Path, *, policy: Path | None = None
    ) -> tuple[subprocess.CompletedProcess[str], Path]:
        report = self.root / "report.tsv"
        report.unlink(missing_ok=True)
        result = subprocess.run(
            [
                sys.executable,
                str(COMPARATOR),
                "--baseline",
                str(baseline),
                "--candidate",
                str(candidate),
                "--policy",
                str(policy or self.policy),
                "--output",
                str(report),
            ],
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

    def test_policy_can_enable_p999_hard_gate(self) -> None:
        payload = json.loads(self.policy.read_text(encoding="utf-8"))
        payload["metrics"]["vma_scale"]["p999_max_regression_percent"] = 20
        self.policy.write_text(json.dumps(payload), encoding="utf-8")
        baseline = self.bundle("baseline")
        candidate = self.bundle("candidate")
        set_metric(candidate, "vma_scale", p999_ns=240)

        result, _ = self.compare(baseline, candidate)
        self.assertEqual(result.returncode, 0, result.stderr)

        set_metric(candidate, "vma_scale", p999_ns=241)
        result, _ = self.compare(baseline, candidate)
        self.assertEqual(result.returncode, 1, result.stderr)

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

    def test_v1_absolute_path_manifest_is_not_silently_upgraded(self) -> None:
        baseline = self.bundle("baseline")
        candidate = self.bundle("candidate")
        mutate_manifest(
            candidate,
            lambda row: row.update(
                {
                    "bundle_schema": "thekernel-mm-performance-bundle-v1",
                    "kernel_artifact": "/workspace/rv-4cpu/kernel",
                }
            ),
        )

        result, report = self.compare(baseline, candidate)

        self.assertEqual(result.returncode, 2)
        self.assertIn("unsupported bundle_schema", result.stderr)
        self.assertFalse(report.exists())


if __name__ == "__main__":
    unittest.main()
