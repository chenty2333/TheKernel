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


def complete_log(
    *,
    topology: int = 4,
    missing_pin: bool = False,
    iterations: int = 100,
    vmas: int = 512,
    pin_iterations: int = 25,
    pin_workers: int = 4,
) -> str:
    records = [f"MM_PERF_TOPOLOGY status=ok online_cpus={topology}"]
    cpu_ids = ",".join(str(cpu) for cpu in range(topology))
    records.append(
        f"MM_PERF_AFFINITY status=ok bytes=8 allowed_cpus={topology} "
        f"cpu_ids={cpu_ids} cpu_ids_complete=1"
    )
    records.append(
        "MM_PERF_RUN schema=thekernel-mm-performance-run-v2 arch=rv "
        f"iterations={iterations} vmas={vmas} "
        f"pin_iterations={pin_iterations} pin_workers={pin_workers} "
        "page_size=4096"
    )
    records.append("MM_PERF_SEMANTICS status=ok")
    for metric in (
        "vma_scale",
        "mremap_latency",
        "mremap_file_duplicate_latency",
        "mremap_shared_anon_resize_latency",
        "protect_touch_latency",
    ):
        count = iterations * (
            2 if metric in {"mremap_latency", "mremap_shared_anon_resize_latency"} else 1
        )
        records.append(
            f"MM_PERF metric={metric} status=ok count={count} "
            "p50_ns=10 p99_ns=20 p999_ns=30"
            + (
                f" requested_vmas={vmas} fixture_vmas={vmas}"
                if metric == "vma_scale"
                else ""
            )
        )
    records.append(
        f"MM_PERF metric=mremap_fixed_replace_latency status=ok count={iterations} "
        "p50_ns=10 p99_ns=20 p999_ns=30 "
        f"requested_vmas={vmas} fixture_vmas={vmas}"
    )
    records.extend(
        (
            "MM_PERF_MREMAP_WORKER status=ok worker=0 cpu=0 "
            f"completed={iterations} slot_a=1048576 slot_b=1060864 bytes=8192 "
            f"start_ns=100 end_ns=300 p99_ns=25 fixture_before_vmas={vmas} "
            f"fixture_after_vmas={vmas}",
            "MM_PERF_MREMAP_WORKER status=ok worker=1 cpu=1 "
            f"completed={iterations} slot_a=1073152 slot_b=1085440 bytes=8192 "
            f"start_ns=150 end_ns=350 p99_ns=26 fixture_before_vmas={vmas} "
            f"fixture_after_vmas={vmas}",
            "MM_PERF metric=mremap_disjoint_same_as_contention status=ok "
            f"count={iterations * 2} p50_ns=11 p99_ns=25 p999_ns=30 "
            f"requested_vmas={vmas} fixture_vmas={vmas}",
        )
    )
    for metric in (
        "direct_io_pin_proxy_throughput",
        "direct_io_pin_proxy_same_as_contention",
        "direct_io_pin_proxy_cross_as_contention",
    ):
        if missing_pin:
            records.append(
                f"MM_PERF metric={metric} status=missing count=0 "
                "p50_ns=missing p99_ns=missing p999_ns=missing "
                "throughput_bytes_per_sec=missing "
                "requested_vmas=512 fixture_vmas=512 "
                "reason=direct_io_unavailable errno=22"
            )
        else:
            count = (
                pin_iterations
                if metric == "direct_io_pin_proxy_throughput"
                else pin_iterations * pin_workers
            )
            records.append(
                f"MM_PERF metric={metric} status=ok count={count} "
                "p50_ns=40 p99_ns=50 p999_ns=60 "
                "throughput_bytes_per_sec=1048576 "
                f"requested_vmas={vmas} fixture_vmas={vmas}"
            )
    if not missing_pin:
        records.append(
            "MM_PERF_PIN_WORKER mode=single status=ok worker=0 cpu=0 "
            f"completed={pin_iterations} p99_ns=50 over_10ms=0 over_50ms=0 "
            f"fixture_before_vmas={vmas} fixture_after_vmas={vmas}"
        )
        records.extend(
            "MM_PERF_PIN_WORKER mode=contention status=ok "
            f"worker={worker} cpu={worker} completed={pin_iterations} p99_ns=50 "
            f"over_10ms=0 over_50ms=0 fixture_before_vmas={vmas} "
            f"fixture_after_vmas={vmas}"
            for worker in range(pin_workers)
        )
        records.extend(
            "MM_PERF_PIN_CROSS_AS_WORKER status=ok "
            f"worker={worker} pid={1000 + worker} cpu={worker} "
            f"completed={pin_iterations} p99_ns=50 fixture_before_vmas={vmas} "
            f"fixture_after_vmas={vmas} cow_isolated=1"
            for worker in range(pin_workers)
        )
    records.append("MM_PERF_DONE status=ok")
    return "boot noise\n" + "\n".join(records) + "\nshutdown noise\n"


class MmPerformanceParserTests(unittest.TestCase):
    def run_parser(
        self,
        log_text: str,
        *extra_args: str,
        cpus: int = 4,
        workload_args: bool = True,
    ) -> subprocess.CompletedProcess[str]:
        with tempfile.TemporaryDirectory() as temporary:
            log = Path(temporary) / "qemu.log"
            log.write_text(log_text, encoding="utf-8")
            arguments = [
                    sys.executable,
                    str(PARSER),
                    str(log),
                    "--arch",
                    "rv",
                    "--cpus",
                    str(cpus),
                ]
            if workload_args:
                arguments.extend(
                    (
                        "--iterations", "100",
                        "--vmas", "512",
                        "--pin-iterations", "25",
                        "--pin-workers", "4",
                    )
                )
            arguments.extend(extra_args)
            return subprocess.run(
                arguments,
                cwd=REPO_ROOT,
                check=False,
                capture_output=True,
                text=True,
            )

    def test_normalizes_complete_evidence_to_tsv(self) -> None:
        result = self.run_parser(complete_log())

        self.assertEqual(result.returncode, 0, result.stderr)
        lines = result.stdout.splitlines()
        self.assertEqual(len(lines), 11)
        self.assertEqual(
            lines[0].split("\t")[:5],
            ["arch", "requested_cpus", "online_cpus", "metric", "status"],
        )
        self.assertIn("rv\t4\t4\tvma_scale\tok\t100\t10\t20\t30", lines[1])
        self.assertIn(
            "\tmremap_fixed_replace_latency\tok\t100\t10\t20\t30\t-\t512\t512",
            lines[3],
        )
        self.assertIn(
            "\tdirect_io_pin_proxy_cross_as_contention\tok\t100\t40\t50\t60\t1048576",
            lines[10],
        )

    def test_structured_missing_pin_metrics_are_evidence(self) -> None:
        result = self.run_parser(complete_log(missing_pin=True), "--format", "json")

        self.assertEqual(result.returncode, 0, result.stderr)
        payload = json.loads(result.stdout)
        pin = {item["metric"]: item for item in payload["metrics"]}
        self.assertEqual(pin["direct_io_pin_proxy_throughput"]["status"], "missing")
        self.assertEqual(pin["direct_io_pin_proxy_throughput"]["reason"], "direct_io_unavailable")
        self.assertEqual(pin["direct_io_pin_proxy_throughput"]["errno"], 22)
        self.assertEqual(pin["direct_io_pin_proxy_throughput"]["requested_vmas"], 512)
        self.assertEqual(pin["direct_io_pin_proxy_throughput"]["fixture_vmas"], 512)
        self.assertEqual(pin["direct_io_pin_proxy_cross_as_contention"]["status"], "missing")

    def test_structured_missing_non_pin_metric_is_evidence(self) -> None:
        log = complete_log().replace(
            "MM_PERF metric=mremap_latency status=ok count=200 "
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
            "p50_ns=1 p99_ns=1 p999_ns=1 "
            "requested_vmas=512 fixture_vmas=512\nMM_PERF_DONE status=ok",
        )
        result = self.run_parser(log)

        self.assertEqual(result.returncode, 1)
        self.assertIn("duplicate metric record: vma_scale", result.stderr)

    def test_rejects_missing_required_metric(self) -> None:
        log = complete_log().replace(
            "MM_PERF metric=mremap_latency status=ok count=200 "
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
            "MM_PERF_AFFINITY status=ok bytes=8 allowed_cpus=4 "
            "cpu_ids=0,1,2,3 cpu_ids_complete=1",
            "MM_PERF_AFFINITY status=ok bytes=1 allowed_cpus=4 "
            "cpu_ids=0,1,2,3 cpu_ids_complete=1",
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
            complete_log().replace(
                "metric=mremap_latency status=ok count=200",
                "metric=mremap_latency status=ok count=100",
            ),
        )

        self.assertEqual(result.returncode, 1)
        self.assertIn(
            "mremap_latency count mismatch: expected=200 actual=100",
            result.stderr,
        )

    def test_rejects_cross_as_count_drift_from_worker_topology(self) -> None:
        valid = complete_log()
        result = self.run_parser(valid)

        self.assertEqual(result.returncode, 0, result.stderr)
        drifted = valid.replace(
            "metric=direct_io_pin_proxy_cross_as_contention status=ok count=100",
            "metric=direct_io_pin_proxy_cross_as_contention status=ok count=99",
        )
        rejected = self.run_parser(drifted)
        self.assertEqual(rejected.returncode, 1)
        self.assertIn(
            "direct_io_pin_proxy_cross_as_contention count mismatch: expected=100 actual=99",
            rejected.stderr,
        )

    def test_rejects_requested_and_verified_fixture_drift(self) -> None:
        requested = self.run_parser(
            complete_log().replace(
                "metric=mremap_fixed_replace_latency status=ok count=100 "
                "p50_ns=10 p99_ns=20 p999_ns=30 requested_vmas=512 "
                "fixture_vmas=512",
                "metric=mremap_fixed_replace_latency status=ok count=100 "
                "p50_ns=10 p99_ns=20 p999_ns=30 requested_vmas=511 "
                "fixture_vmas=511",
            ),
        )
        self.assertEqual(requested.returncode, 1)
        self.assertIn(
            "mremap_fixed_replace_latency requested_vmas mismatch: "
            "expected=512 actual=511",
            requested.stderr,
        )

        result = self.run_parser(
            complete_log().replace(
                "metric=mremap_fixed_replace_latency status=ok count=100 "
                "p50_ns=10 p99_ns=20 p999_ns=30 requested_vmas=512 "
                "fixture_vmas=512",
                "metric=mremap_fixed_replace_latency status=ok count=100 "
                "p50_ns=10 p99_ns=20 p999_ns=30 requested_vmas=512 "
                "fixture_vmas=511",
            ),
        )

        self.assertEqual(result.returncode, 1)
        self.assertIn(
            "mremap_fixed_replace_latency verified fixture_vmas mismatch: "
            "requested=512 verified=511",
            result.stderr,
        )

    def test_rejects_missing_or_duplicate_run_record(self) -> None:
        record = (
            "MM_PERF_RUN schema=thekernel-mm-performance-run-v2 arch=rv "
            "iterations=100 vmas=512 pin_iterations=25 pin_workers=4 "
            "page_size=4096"
        )
        missing = self.run_parser(complete_log().replace(record + "\n", ""))
        self.assertEqual(missing.returncode, 1)
        self.assertIn("missing MM_PERF_RUN record", missing.stderr)

        duplicate = self.run_parser(complete_log().replace(record, record + "\n" + record))
        self.assertEqual(duplicate.returncode, 1)
        self.assertIn("duplicate MM_PERF_RUN record", duplicate.stderr)

    def test_rejects_run_record_workload_or_arch_drift(self) -> None:
        arch = self.run_parser(complete_log().replace("arch=rv", "arch=la", 1))
        self.assertEqual(arch.returncode, 1)
        self.assertIn("MM_PERF_RUN arch mismatch", arch.stderr)

        workload = self.run_parser(
            complete_log().replace("pin_iterations=25", "pin_iterations=24", 1)
        )
        self.assertEqual(workload.returncode, 1)
        self.assertIn("MM_PERF_RUN pin_iterations mismatch", workload.stderr)

    def test_rejects_missing_and_duplicate_cpu_worker_witnesses(self) -> None:
        worker = (
            "MM_PERF_PIN_WORKER mode=contention status=ok worker=3 cpu=3 "
            "completed=25 p99_ns=50 over_10ms=0 over_50ms=0 "
            "fixture_before_vmas=512 fixture_after_vmas=512\n"
        )
        missing = self.run_parser(complete_log().replace(worker, ""))
        self.assertEqual(missing.returncode, 1)
        self.assertIn("worker evidence count mismatch", missing.stderr)

        duplicate_cpu = self.run_parser(
            complete_log().replace("worker=3 cpu=3 completed=25", "worker=3 cpu=2 completed=25", 1)
        )
        self.assertEqual(duplicate_cpu.returncode, 1)
        self.assertIn("duplicate CPU witnesses", duplicate_cpu.stderr)

    def test_rejects_cross_as_cow_pid_and_fixture_witness_drift(self) -> None:
        cow = self.run_parser(complete_log().replace("cow_isolated=1", "cow_isolated=0", 1))
        self.assertEqual(cow.returncode, 1)
        self.assertIn("lacks COW isolation witness", cow.stderr)

        duplicate_pid = self.run_parser(
            complete_log().replace("worker=3 pid=1003", "worker=3 pid=1002", 1)
        )
        self.assertEqual(duplicate_pid.returncode, 1)
        self.assertIn("duplicate PID witnesses", duplicate_pid.stderr)

        fixture = self.run_parser(
            complete_log().replace(
                "worker=0 pid=1000 cpu=0 completed=25 p99_ns=50 fixture_before_vmas=512",
                "worker=0 pid=1000 cpu=0 completed=25 p99_ns=50 fixture_before_vmas=511",
                1,
            )
        )
        self.assertEqual(fixture.returncode, 1)
        self.assertIn("fixture mismatch", fixture.stderr)

    def test_rejects_mremap_worker_window_and_slot_witness_drift(self) -> None:
        no_overlap = self.run_parser(
            complete_log().replace(
                "worker=1 cpu=1 completed=100 slot_a=1073152 slot_b=1085440 "
                "bytes=8192 start_ns=150 end_ns=350",
                "worker=1 cpu=1 completed=100 slot_a=1073152 slot_b=1085440 "
                "bytes=8192 start_ns=300 end_ns=350",
            )
        )
        self.assertEqual(no_overlap.returncode, 1)
        self.assertIn("execution windows do not overlap", no_overlap.stderr)

        overlapping_slots = self.run_parser(
            complete_log().replace("slot_a=1073152", "slot_a=1052672")
        )
        self.assertEqual(overlapping_slots.returncode, 1)
        self.assertIn("slot ranges overlap", overlapping_slots.stderr)

        missing_worker = self.run_parser(
            complete_log().replace(
                "MM_PERF_MREMAP_WORKER status=ok worker=1 cpu=1 "
                "completed=100 slot_a=1073152 slot_b=1085440 bytes=8192 "
                "start_ns=150 end_ns=350 p99_ns=26 "
                "fixture_before_vmas=512 fixture_after_vmas=512\n",
                "",
            )
        )
        self.assertEqual(missing_worker.returncode, 1)
        self.assertIn("mremap worker evidence count mismatch", missing_worker.stderr)

    def test_rejects_mremap_geometry_or_affinity_forgery(self) -> None:
        outside_affinity = self.run_parser(
            complete_log().replace("worker=1 cpu=1", "worker=1 cpu=100", 1)
        )
        self.assertEqual(outside_affinity.returncode, 1)
        self.assertIn("outside the affinity witness", outside_affinity.stderr)

        wrong_size = self.run_parser(
            complete_log().replace("bytes=8192", "bytes=1", 1)
        )
        self.assertEqual(wrong_size.returncode, 1)
        self.assertIn("slot size mismatch", wrong_size.stderr)

        unaligned = self.run_parser(
            complete_log().replace("slot_a=1048576", "slot_a=1048577", 1)
        )
        self.assertEqual(unaligned.returncode, 1)
        self.assertIn("unaligned slot address", unaligned.stderr)

        invalid_page_size = self.run_parser(
            complete_log().replace("page_size=4096", "page_size=1", 1)
        )
        self.assertEqual(invalid_page_size.returncode, 1)
        self.assertIn("invalid page_size", invalid_page_size.stderr)

    def test_missing_mremap_metric_cannot_retain_success_witnesses(self) -> None:
        log = complete_log().replace(
            "MM_PERF metric=mremap_disjoint_same_as_contention status=ok "
            "count=200 p50_ns=11 p99_ns=25 p999_ns=30 "
            "requested_vmas=512 fixture_vmas=512",
            "MM_PERF metric=mremap_disjoint_same_as_contention status=missing "
            "count=0 p50_ns=missing p99_ns=missing p999_ns=missing "
            "requested_vmas=512 fixture_vmas=512 "
            "reason=contention_unavailable errno=11",
        )
        rejected = self.run_parser(log)
        self.assertEqual(rejected.returncode, 1)
        self.assertIn("must be absent", rejected.stderr)

        without_workers = "\n".join(
            line
            for line in log.splitlines()
            if not line.startswith("MM_PERF_MREMAP_WORKER ")
        ) + "\n"
        accepted = self.run_parser(without_workers)
        self.assertEqual(accepted.returncode, 0, accepted.stderr)


if __name__ == "__main__":
    unittest.main()
