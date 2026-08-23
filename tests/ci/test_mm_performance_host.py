#!/usr/bin/env python3
"""Tests for homogeneous MM runner CPU selection and safe diagnostics."""

from __future__ import annotations

import importlib.util
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock


REPO_ROOT = Path(__file__).resolve().parents[2]
CI_DIR = REPO_ROOT / "scripts" / "ci"


def load_module(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


host = load_module("mm_performance_host", CI_DIR / "mm_performance_host.py")
schema = load_module("mm_performance_schema", CI_DIR / "mm_performance_schema.py")


def add_cpu(root: Path, cpu: int, package: int, max_frequency: int) -> None:
    topology = root / f"cpu{cpu}" / "topology"
    frequency = root / f"cpu{cpu}" / "cpufreq"
    topology.mkdir(parents=True)
    frequency.mkdir()
    (topology / "physical_package_id").write_text(
        f"{package}\n", encoding="ascii"
    )
    (frequency / "cpuinfo_max_freq").write_text(
        f"{max_frequency}\n", encoding="ascii"
    )


class CpuSelectionTests(unittest.TestCase):
    def test_cpu_list_round_trip_and_duplicate_rejection(self) -> None:
        self.assertEqual(host.parse_cpu_list("0-2,5,7-8"), (0, 1, 2, 5, 7, 8))
        self.assertEqual(host.format_cpu_list((8, 2, 1, 0, 7, 5)), "0-2,5,7-8")
        with self.assertRaises(host.CpuSelectionError):
            host.parse_cpu_list("0-2,2")

    def test_auto_selects_one_large_homogeneous_class(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            sysfs = Path(temporary)
            for cpu in range(8):
                add_cpu(sysfs, cpu, 0, 3_700_000)
            for cpu in range(8, 12):
                add_cpu(sysfs, cpu, 0, 4_800_000)

            result = host.select_cpu_sets(
                (4, 8), allowed=set(range(12)), sysfs=sysfs
            )

            self.assertEqual([item.host_cpu_set for item in result], ["0-3", "0-7"])
            self.assertTrue(
                all(item.selection == "auto-homogeneous-v1" for item in result)
            )
            self.assertTrue(
                all(
                    item.cpu_class == "package:0,max_freq_khz:3700000"
                    for item in result
                )
            )

    def test_auto_reports_unsupported_instead_of_mixing_classes(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            sysfs = Path(temporary)
            for cpu in range(4):
                add_cpu(sysfs, cpu, 0, 3_700_000)
            for cpu in range(4, 8):
                add_cpu(sysfs, cpu, 0, 4_800_000)

            with self.assertRaises(host.CpuSelectionUnsupported):
                host.select_cpu_sets((8,), allowed=set(range(8)), sysfs=sysfs)

    def test_explicit_selection_is_still_checked_for_mixed_classes(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            sysfs = Path(temporary)
            add_cpu(sysfs, 0, 0, 4_800_000)
            add_cpu(sysfs, 1, 0, 4_800_000)
            add_cpu(sysfs, 2, 0, 3_700_000)
            with self.assertRaises(host.CpuSelectionError):
                host.select_cpu_sets(
                    (2,), explicit="1-2", allowed={0, 1, 2}, sysfs=sysfs
                )

            result = host.select_cpu_sets(
                (2,), explicit="0-1", allowed={0, 1, 2}, sysfs=sysfs
            )
            self.assertEqual(result[0].host_cpu_set, "0-1")
            self.assertEqual(result[0].selection, "explicit-homogeneous-v1")
            self.assertEqual(
                result[0].cpu_class, "package:0,max_freq_khz:4800000"
            )

    def test_missing_topology_is_unsupported(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            with self.assertRaises(host.CpuSelectionUnsupported):
                host.select_cpu_sets((1,), allowed={0}, sysfs=Path(temporary))


class HostDiagnosticTests(unittest.TestCase):
    def test_capture_contains_only_bounded_safe_categories(self) -> None:
        capture = load_module(
            "capture_mm_performance_host",
            CI_DIR / "capture-mm-performance-host.py",
        )
        original = os.sched_getaffinity(0)
        selected = min(original)
        try:
            os.sched_setaffinity(0, {selected})
            rows = capture.collect(
                "pre",
                (selected,),
                "explicit-homogeneous-v1",
                "package:0,max_freq_khz:1",
            )
        finally:
            os.sched_setaffinity(0, original)

        values = dict(rows)
        self.assertEqual(values["schema"], capture.SCHEMA)
        self.assertEqual(values["phase"], "pre")
        self.assertEqual(values["selected_cpu_set"], str(selected))
        self.assertFalse(any(key.startswith("command_count.") for key, _ in rows))
        self.assertFalse(any("hostname" in key or "cmdline" in key for key, _ in rows))
        encoded = sum(
            len("\t".join(row).encode("utf-8")) + 1
            for row in [("key", "value"), *rows]
        )
        self.assertLess(encoded, capture.MAX_DIAGNOSTIC_BYTES)


class RunnerContractTests(unittest.TestCase):
    def test_shell_manifest_header_tracks_current_schema(self) -> None:
        source = (CI_DIR / "nightly" / "mm-performance.sh").read_text(
            encoding="utf-8"
        )
        self.assertIn(
            "mode\\tarch\\tcpus\\tonline_cpus\\tmetrics\\treceipt\\thost_pre\\thost_post",
            source,
        )
        self.assertEqual(
            schema.MANIFEST_COLUMNS,
            ("mode", "arch", "cpus", "online_cpus", "metrics", "receipt", "host_pre", "host_post"),
        )

    def test_runner_materializes_one_explicit_receipt_per_run(self) -> None:
        source = (CI_DIR / "nightly" / "mm-performance.sh").read_text(
            encoding="utf-8"
        )
        self.assertIn("mm_perf_capture_prepared_run", source)
        self.assertIn('"$run_name/performance-receipt.json"', source)
        self.assertIn('"$run_name/host-pre.tsv"', source)
        self.assertIn('"$run_name/host-post.tsv"', source)

    def test_mm_boundary_prepares_before_host_pre_and_only_runs_between_snapshots(self) -> None:
        source = (CI_DIR / "nightly" / "mm-performance-boundary.sh").read_text(
            encoding="utf-8"
        )
        prepare = source.index("nightly_prepare_guest_run")
        host_pre = source.index("--phase pre")
        execute = source.index("nightly_run_prepared_guest")
        host_post = source.index("--phase post")
        self.assertLess(prepare, host_pre)
        self.assertLess(host_pre, execute)
        self.assertLess(execute, host_post)

        nightly_lib = (CI_DIR / "nightly" / "lib.sh").read_text(encoding="utf-8")
        prepare_body = nightly_lib.split("nightly_prepare_guest_run() {", 1)[1].split(
            "nightly_run_prepared_guest() {", 1
        )[0]
        execute_body = nightly_lib.split("nightly_run_prepared_guest() {", 1)[1].split(
            "nightly_run_guest() {", 1
        )[0]
        self.assertIn("ci_prepare_run_dir", prepare_body)
        self.assertIn("thekernel.py build", prepare_body)
        self.assertIn("thekernel.py rootfs", prepare_body)
        self.assertIn("run --no-build", execute_body)
        self.assertNotIn("ci_prepare_run_dir", execute_body)
        self.assertNotIn("thekernel.py build", execute_body)
        self.assertNotIn("thekernel.py rootfs", execute_body)

    def test_product_run_no_build_skips_artifact_builders(self) -> None:
        product = load_module(
            "thekernel_product_cli", REPO_ROOT / "tools" / "thekernel.py"
        )
        args = product.build_parser().parse_args(
            ["run", "--no-build", "--profile", "mm-performance", "--smp", "4"]
        )
        with (
            mock.patch.object(product, "build_kernel") as build_kernel,
            mock.patch.object(product, "build_rootfs") as build_rootfs,
            mock.patch.object(product, "run_product", return_value=0) as run_product,
        ):
            self.assertEqual(product.run_cmd(args), 0)
        build_kernel.assert_not_called()
        build_rootfs.assert_not_called()
        run_product.assert_called_once()

    def test_product_run_no_build_fails_closed_when_artifacts_are_missing(self) -> None:
        product = load_module(
            "thekernel_product_cli_missing", REPO_ROOT / "tools" / "thekernel.py"
        )
        args = product.build_parser().parse_args(["run", "--no-build"])
        with tempfile.TemporaryDirectory() as temporary:
            with mock.patch.dict(
                os.environ, {"THEKERNEL_STATE_DIR": temporary}, clear=False
            ):
                with self.assertRaisesRegex(product.ProductError, "kernel and ESP"):
                    product.run_cmd(args)


if __name__ == "__main__":
    unittest.main()
