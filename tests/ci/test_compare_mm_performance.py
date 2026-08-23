#!/usr/bin/env python3
"""Focused tests for the explicit MM performance receipt consumer."""

from __future__ import annotations

import hashlib
import importlib.util
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from tests.ci.test_mm_performance_parser import complete_log

REPO_ROOT = Path(__file__).resolve().parents[2]
CI_DIR = REPO_ROOT / "scripts" / "ci"
sys.path.insert(0, str(CI_DIR))
import mm_performance_schema as schema  # noqa: E402
from scripts.ci import source_combination  # noqa: E402


def load_consumer():
    spec = importlib.util.spec_from_file_location(
        "compare_mm_performance", CI_DIR / "compare-mm-performance.py"
    )
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def file_evidence(path: Path, *, actual: bool = False) -> dict[str, object]:
    if actual:
        return {
            "path": str(path.resolve()),
            "size_bytes": path.stat().st_size,
            "sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
        }
    return {"path": str(path), "size_bytes": 1, "sha256": "a" * 64}


def receipt(log: Path) -> dict[str, object]:
    files = {
        name: file_evidence(Path(f"/fixture/{name}"))
        for name in (
            "kernel", "rootfs_source", "rootfs_runtime_before",
            "rootfs_runtime_after", "qemu", "esp_source", "esp_runtime",
            "ovmf_code", "ovmf_vars_source", "ovmf_vars_runtime",
        )
    }
    files["qemu"]["requested"] = "qemu-system-x86_64"
    files["rootfs_runtime_before"] = files["rootfs_source"].copy()
    files["rootfs_runtime_after"] = files["rootfs_source"].copy()
    files["esp_runtime"] = files["esp_source"].copy()
    files["ovmf_vars_runtime"] = files["ovmf_vars_source"].copy()
    return {
        "schema_version": 4,
        "state": "recorded",
        "source_identity": {
            "schema": 1,
            "combination_id": "source-combination-v1-" + "e" * 64,
            "sources": {
                name: {
                    "repository_root": f"/fixture/{name}",
                    "commit": "c" * 40,
                    "tree": "d" * 40,
                    "worktree_dirty": False,
                    "match_declared": True,
                }
                for name in ("thekernel", *source_combination.load(
                    REPO_ROOT / "config" / "source-combination.toml"
                ))
            },
        },
        "arch": "x86_64",
        "cpus": 4,
        "memory": "1G",
        "accel": None,
        "cpu": None,
        "iothread_id": None,
        "network": "user",
        "tap_name": None,
        "extra_args": [],
        "qemu_launcher": None,
        "rootfs_mode": "snapshot",
        "direct_kernel": False,
        "returncode": 0,
        "duration_ms": 1,
        "log_path": str(log.resolve()),
        "error_message": None,
        "timed_out": False,
        "interrupted": False,
        "intentionally_stopped": False,
        "marker_success": False,
        "guest_clean_shutdown": True,
        "runner_terminated": False,
        "runner_termination_reason": None,
        "physical_retirement_proven": False,
        "interaction": {
            "interactive": True,
            "input_after_marker": "THEKERNEL_SHELL_READY",
            "stop_after_marker": None,
        },
        "stdin": {
            "source": {
                "path": "/fixture/commands",
                "sha256": "b" * 64,
                "bytes": 1,
                "line_count": 1,
            },
            "forwarded": {
                "sha256": "b" * 64,
                "bytes": 1,
                "line_count": 1,
            },
            "source_eof": True,
            "relay_complete": True,
            "source_unchanged": True,
            "broken_pipe": False,
        },
        "log": file_evidence(log, actual=True),
        **files,
    }


def write_host_snapshot(path: Path, phase: str, second: int) -> None:
    rows = [
        ("schema", schema.HOST_DIAGNOSTIC_SCHEMA),
        ("phase", phase),
        ("timestamp_utc", f"2026-01-01T00:00:0{second}+00:00"),
        ("selected_cpu_set", "0-3"),
        ("host_cpu_selection", "auto-homogeneous-v1"),
        ("host_cpu_class", "package:0,max_freq_khz:3700000"),
        ("online_cpu_set", "0-3"),
        ("loadavg", "0.00 0.00 0.00 1/1 1"),
        ("psi.cpu", "missing"),
        ("cgroup.cpu_stat", "missing"),
    ]
    rows.extend(
        (f"cpu.{cpu}.{field}", value)
        for cpu in range(4)
        for field, value in (
            ("online", "1"),
            ("package", "0"),
            ("max_freq_khz", "3700000"),
            ("current_freq_khz", "missing"),
        )
    )
    path.write_text(
        "key\tvalue\n" + "".join(f"{key}\t{value}\n" for key, value in rows),
        encoding="utf-8",
    )


class ExplicitReceiptTests(unittest.TestCase):
    def test_manifest_is_the_current_eight_column_contract(self) -> None:
        self.assertEqual(
            schema.MANIFEST_COLUMNS,
            (
                "mode", "arch", "cpus", "online_cpus", "metrics",
                "receipt", "host_pre", "host_post",
            ),
        )
        self.assertFalse(hasattr(schema, "BUNDLE_SCHEMA"))

    def test_receipt_validates_file_evidence_stdin_and_run_local_log(self) -> None:
        consumer = load_consumer()
        with tempfile.TemporaryDirectory() as temporary:
            run = Path(temporary)
            log = run / "console.log"
            log.write_text("guest evidence\n", encoding="utf-8")
            receipt_path = run / "performance-receipt.json"
            receipt_path.write_text(json.dumps(receipt(log)), encoding="utf-8")
            identity, kernel = consumer.validate_receipt(
                receipt_path, log, "x86_64", 4, "fixture"
            )
            self.assertEqual(kernel, (1, "a" * 64))
            self.assertEqual(identity[0], "1G")

    def test_receipt_allows_distinct_paths_for_identical_snapshot_files(self) -> None:
        consumer = load_consumer()
        with tempfile.TemporaryDirectory() as temporary:
            run = Path(temporary)
            log = run / "console.log"
            log.write_text("guest evidence\n", encoding="utf-8")
            payload = receipt(log)
            payload["rootfs_runtime_before"]["path"] = "/runtime/rootfs-before.img"
            payload["rootfs_runtime_after"]["path"] = "/runtime/rootfs-after.img"
            payload["esp_runtime"]["path"] = "/runtime/esp.img"
            payload["ovmf_vars_runtime"]["path"] = "/runtime/OVMF_VARS.fd"
            receipt_path = run / "performance-receipt.json"
            receipt_path.write_text(json.dumps(payload), encoding="utf-8")

            consumer.validate_receipt(receipt_path, log, "x86_64", 4, "fixture")

    def test_receipt_allows_a_stable_decompressed_snapshot_runtime(self) -> None:
        consumer = load_consumer()
        with tempfile.TemporaryDirectory() as temporary:
            run = Path(temporary)
            log = run / "console.log"
            log.write_text("guest evidence\n", encoding="utf-8")
            payload = receipt(log)
            payload["rootfs_source"]["path"] = "/input/rootfs.img.gz"
            payload["rootfs_runtime_before"] = file_evidence(
                Path("/runtime/rootfs.img")
            )
            payload["rootfs_runtime_before"]["size_bytes"] = 2
            payload["rootfs_runtime_before"]["sha256"] = "c" * 64
            payload["rootfs_runtime_after"] = payload[
                "rootfs_runtime_before"
            ].copy()
            receipt_path = run / "performance-receipt.json"
            receipt_path.write_text(json.dumps(payload), encoding="utf-8")

            consumer.validate_receipt(receipt_path, log, "x86_64", 4, "fixture")

    def test_receipt_rejects_changed_snapshot_content_or_size(self) -> None:
        consumer = load_consumer()
        mutations = (
            ("rootfs_runtime_before", "sha256", "c" * 64),
            ("rootfs_runtime_after", "size_bytes", 2),
            ("esp_runtime", "sha256", "d" * 64),
            ("ovmf_vars_runtime", "size_bytes", 2),
        )
        with tempfile.TemporaryDirectory() as temporary:
            run = Path(temporary)
            log = run / "console.log"
            log.write_text("guest evidence\n", encoding="utf-8")
            receipt_path = run / "performance-receipt.json"
            for field, identity_field, value in mutations:
                with self.subTest(field=field, identity_field=identity_field):
                    payload = receipt(log)
                    payload[field]["path"] = f"/runtime/{field}"
                    payload[field][identity_field] = value
                    receipt_path.write_text(json.dumps(payload), encoding="utf-8")
                    with self.assertRaisesRegex(
                        consumer.EvidenceError, "changes a snapshot input"
                    ):
                        consumer.validate_receipt(
                            receipt_path, log, "x86_64", 4, "fixture"
                        )

    def test_receipt_requires_snapshot_rootfs_mode(self) -> None:
        consumer = load_consumer()
        with tempfile.TemporaryDirectory() as temporary:
            run = Path(temporary)
            log = run / "console.log"
            log.write_text("guest evidence\n", encoding="utf-8")
            payload = receipt(log)
            payload["rootfs_mode"] = "readonly"
            receipt_path = run / "performance-receipt.json"
            receipt_path.write_text(json.dumps(payload), encoding="utf-8")

            with self.assertRaisesRegex(consumer.EvidenceError, "final state mismatch"):
                        consumer.validate_receipt(receipt_path, log, "x86_64", 4, "fixture")

    def test_receipt_rejects_dirty_source_or_runner_stopped_marker(self) -> None:
        consumer = load_consumer()
        with tempfile.TemporaryDirectory() as temporary:
            run = Path(temporary)
            log = run / "console.log"
            log.write_text("guest evidence\n", encoding="utf-8")
            receipt_path = run / "performance-receipt.json"
            for field, value, message in (
                (
                    "source_identity",
                    {
                        **receipt(log)["source_identity"],
                        "sources": {
                            **receipt(log)["source_identity"]["sources"],
                            "ax": {
                                **receipt(log)["source_identity"]["sources"]["ax"],
                                "worktree_dirty": True,
                            },
                        },
                    },
                    "source identity is dirty",
                ),
                (
                    "source_identity",
                    {
                        **receipt(log)["source_identity"],
                        "sources": {
                            **receipt(log)["source_identity"]["sources"],
                            "ax": {
                                **receipt(log)["source_identity"]["sources"]["ax"],
                                "match_declared": False,
                            },
                        },
                    },
                    "does not match declared combination",
                ),
                ("marker_success", True, "final state mismatch"),
                ("guest_clean_shutdown", False, "final state mismatch"),
                ("runner_terminated", True, "final state mismatch"),
            ):
                with self.subTest(field=field):
                    payload = receipt(log)
                    payload[field] = value
                    receipt_path.write_text(json.dumps(payload), encoding="utf-8")
                    with self.assertRaisesRegex(consumer.EvidenceError, message):
                        consumer.validate_receipt(
                            receipt_path, log, "x86_64", 4, "fixture"
                        )

    def test_receipt_rejects_log_splicing(self) -> None:
        consumer = load_consumer()
        with tempfile.TemporaryDirectory() as temporary:
            run = Path(temporary)
            log = run / "console.log"
            log.write_text("guest evidence\n", encoding="utf-8")
            payload = receipt(log)
            payload["log"]["sha256"] = "c" * 64
            receipt_path = run / "performance-receipt.json"
            receipt_path.write_text(json.dumps(payload), encoding="utf-8")
            with self.assertRaisesRegex(consumer.EvidenceError, "log evidence"):
                consumer.validate_receipt(receipt_path, log, "x86_64", 4, "fixture")

    def test_receipt_rejects_changed_or_partially_forwarded_input(self) -> None:
        consumer = load_consumer()
        with tempfile.TemporaryDirectory() as temporary:
            run = Path(temporary)
            log = run / "console.log"
            log.write_text("guest evidence\n", encoding="utf-8")
            for field, value in (("source_unchanged", False), ("source_eof", False)):
                payload = receipt(log)
                payload["stdin"][field] = value
                receipt_path = run / "performance-receipt.json"
                receipt_path.write_text(json.dumps(payload), encoding="utf-8")
                with self.assertRaisesRegex(consumer.EvidenceError, "stdin forwarding"):
                    consumer.validate_receipt(receipt_path, log, "x86_64", 4, "fixture")

    def test_loads_metrics_from_the_receipt_bound_console_log(self) -> None:
        consumer = load_consumer()
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            run = root / "x86_64-4cpu"
            run.mkdir()
            log = run / "console.log"
            log.write_text(complete_log(), encoding="utf-8")
            metrics = run / "mm-performance.tsv"
            parsed = subprocess.run(
                [
                    sys.executable, str(CI_DIR / "parse-mm-performance.py"), str(log),
                    "--arch", "x86_64", "--cpus", "4", "--output", str(metrics),
                ],
                text=True, capture_output=True, check=False,
            )
            self.assertEqual(parsed.returncode, 0, parsed.stderr)
            receipt_path = run / "performance-receipt.json"
            receipt_path.write_text(json.dumps(receipt(log)), encoding="utf-8")
            write_host_snapshot(run / "host-pre.tsv", "pre", 0)
            write_host_snapshot(run / "host-post.tsv", "post", 1)
            (root / "mm-performance-manifest.tsv").write_text(
                "\t".join(schema.MANIFEST_COLUMNS) + "\n"
                "product\tx86_64\t4\t4\tx86_64-4cpu/mm-performance.tsv\t"
                "x86_64-4cpu/performance-receipt.json\tx86_64-4cpu/host-pre.tsv\t"
                "x86_64-4cpu/host-post.tsv\n",
                encoding="utf-8",
            )
            bundle = consumer.load_bundle(root, allow_partial=True)
            self.assertEqual(len(bundle.metrics), len(schema.EXPECTED_METRICS))

if __name__ == "__main__":
    unittest.main()
