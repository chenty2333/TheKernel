#!/usr/bin/env python3

from __future__ import annotations

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
PARSER = REPO_ROOT / "scripts" / "ci" / "parse-mm-performance.py"


def complete_log(*, topology: int = 4, missing_pin: bool = False) -> str:
    records = [f"MM_PERF_TOPOLOGY status=ok online_cpus={topology}"]
    records.append(f"MM_PERF_AFFINITY status=ok bytes=8 allowed_cpus={topology}")
    records.append("MM_PERF_SEMANTICS status=ok")
    for metric in ("vma_scale", "mremap_latency", "protect_touch_latency"):
        records.append(
            f"MM_PERF metric={metric} status=ok count=100 "
            "p50_ns=10 p99_ns=20 p999_ns=30"
        )
    for metric in ("pin_throughput", "pin_contention"):
        if missing_pin:
            records.append(
                f"MM_PERF metric={metric} status=missing count=0 "
                "p50_ns=missing p99_ns=missing p999_ns=missing "
                "throughput_bytes_per_sec=missing "
                "reason=direct_io_unavailable errno=22"
            )
        else:
            records.append(
                f"MM_PERF metric={metric} status=ok count=100 "
                "p50_ns=40 p99_ns=50 p999_ns=60 "
                "throughput_bytes_per_sec=1048576"
            )
    records.append("MM_PERF_DONE status=ok")
    return "boot noise\n" + "\n".join(records) + "\nshutdown noise\n"


class MmPerformanceParserTests(unittest.TestCase):
    def run_parser(
        self, log_text: str, *extra_args: str, cpus: int = 4
    ) -> subprocess.CompletedProcess[str]:
        with tempfile.TemporaryDirectory() as temporary:
            log = Path(temporary) / "qemu.log"
            log.write_text(log_text, encoding="utf-8")
            return subprocess.run(
                [
                    sys.executable,
                    str(PARSER),
                    str(log),
                    "--arch",
                    "rv",
                    "--cpus",
                    str(cpus),
                    *extra_args,
                ],
                cwd=REPO_ROOT,
                check=False,
                capture_output=True,
                text=True,
            )

    def test_normalizes_complete_evidence_to_tsv(self) -> None:
        result = self.run_parser(complete_log())

        self.assertEqual(result.returncode, 0, result.stderr)
        lines = result.stdout.splitlines()
        self.assertEqual(len(lines), 6)
        self.assertEqual(
            lines[0].split("\t")[:5],
            ["arch", "requested_cpus", "online_cpus", "metric", "status"],
        )
        self.assertIn("rv\t4\t4\tvma_scale\tok\t100\t10\t20\t30", lines[1])
        self.assertIn("\tpin_contention\tok\t100\t40\t50\t60\t1048576", lines[5])

    def test_structured_missing_pin_metrics_are_evidence(self) -> None:
        result = self.run_parser(complete_log(missing_pin=True), "--format", "json")

        self.assertEqual(result.returncode, 0, result.stderr)
        payload = json.loads(result.stdout)
        pin = {item["metric"]: item for item in payload["metrics"]}
        self.assertEqual(pin["pin_throughput"]["status"], "missing")
        self.assertEqual(pin["pin_throughput"]["reason"], "direct_io_unavailable")
        self.assertEqual(pin["pin_throughput"]["errno"], 22)

    def test_structured_missing_non_pin_metric_is_evidence(self) -> None:
        log = complete_log().replace(
            "MM_PERF metric=mremap_latency status=ok count=100 "
            "p50_ns=10 p99_ns=20 p999_ns=30",
            "MM_PERF metric=mremap_latency status=missing count=0 "
            "p50_ns=missing p99_ns=missing p999_ns=missing "
            "reason=mremap_unavailable errno=38",
        )
        result = self.run_parser(log, "--format", "json")

        self.assertEqual(result.returncode, 0, result.stderr)
        payload = json.loads(result.stdout)
        mremap = next(
            item for item in payload["metrics"] if item["metric"] == "mremap_latency"
        )
        self.assertEqual(mremap["status"], "missing")
        self.assertEqual(mremap["p50_ns"], None)
        self.assertEqual(mremap["reason"], "mremap_unavailable")

    def test_rejects_duplicate_metric(self) -> None:
        log = complete_log().replace(
            "MM_PERF_DONE status=ok",
            "MM_PERF metric=vma_scale status=ok count=1 "
            "p50_ns=1 p99_ns=1 p999_ns=1\nMM_PERF_DONE status=ok",
        )
        result = self.run_parser(log)

        self.assertEqual(result.returncode, 1)
        self.assertIn("duplicate metric record: vma_scale", result.stderr)

    def test_rejects_missing_required_metric(self) -> None:
        log = complete_log().replace(
            "MM_PERF metric=mremap_latency status=ok count=100 "
            "p50_ns=10 p99_ns=20 p999_ns=30\n",
            "",
        )
        result = self.run_parser(log)

        self.assertEqual(result.returncode, 1)
        self.assertIn("missing required metric records: mremap_latency", result.stderr)

    def test_rejects_non_monotonic_quantiles(self) -> None:
        log = complete_log().replace(
            "metric=vma_scale status=ok count=100 p50_ns=10 p99_ns=20 p999_ns=30",
            "metric=vma_scale status=ok count=100 p50_ns=30 p99_ns=20 p999_ns=10",
        )
        result = self.run_parser(log)

        self.assertEqual(result.returncode, 1)
        self.assertIn("non-monotonic quantiles", result.stderr)

    def test_rejects_qemu_and_guest_cpu_mismatch(self) -> None:
        result = self.run_parser(complete_log(topology=1), cpus=8)

        self.assertEqual(result.returncode, 1)
        self.assertIn("requested=8 online=1", result.stderr)

    def test_rejects_missing_topology_even_with_complete_metrics(self) -> None:
        log = complete_log().replace(
            "MM_PERF_TOPOLOGY status=ok online_cpus=4",
            "MM_PERF_TOPOLOGY status=missing online_cpus=missing "
            "reason=sysconf_failed errno=38",
        )
        result = self.run_parser(log)

        self.assertEqual(result.returncode, 1)
        self.assertIn("guest CPU topology unavailable", result.stderr)

    def test_rejects_compact_non_linux_affinity_length(self) -> None:
        log = complete_log().replace(
            "MM_PERF_AFFINITY status=ok bytes=8 allowed_cpus=4",
            "MM_PERF_AFFINITY status=ok bytes=1 allowed_cpus=4",
        )
        result = self.run_parser(log)

        self.assertEqual(result.returncode, 1)
        self.assertIn("not 64-bit word aligned: 1", result.stderr)

    def test_rejects_missing_semantic_preflight(self) -> None:
        result = self.run_parser(
            complete_log().replace("MM_PERF_SEMANTICS status=ok\n", "")
        )

        self.assertEqual(result.returncode, 1)
        self.assertIn("expected one MM_PERF_SEMANTICS record", result.stderr)

    def test_rejects_metric_count_drift_from_requested_workload(self) -> None:
        result = self.run_parser(
            complete_log(),
            "--iterations",
            "100",
            "--pin-iterations",
            "100",
            "--pin-workers",
            "1",
        )

        self.assertEqual(result.returncode, 1)
        self.assertIn(
            "mremap_latency count mismatch: expected=200 actual=100",
            result.stderr,
        )


if __name__ == "__main__":
    unittest.main()
