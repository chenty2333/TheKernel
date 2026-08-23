from __future__ import annotations

import argparse
import hashlib
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import tools.kvm_scheduler_baseline as scheduler_baseline
import tools.kvm_subsystem_baseline as subsystem_baseline

from tools.kvm_subsystem_baseline import (
    BaselineError,
    HostCpu,
    HostTopology,
    TopologyUnavailable,
    _build_guest_command,
    _data_proof_matches,
    _extra_block_receipt_matches,
    _housekeeping_selection,
    _packet_guest_args,
    _pin_report_valid,
    _start_packet_peer,
    _stop_packet_peer,
    _validate_peer_done,
    _wait_packet_peer_done,
    _helper_for,
    host_cost_capabilities,
    eligible_for_stats,
    parse_tkperf_log,
    pin_report_failure_status,
    read_host_topology,
    run_command,
    select_host_cpus,
    stats,
    summarize_samples,
    validate_cpu_roles,
)
from tools.kvm_scheduler_pinner import write_report


SCHEMA = "thekernel-perf-v1"
RUN_ID = "0123456789abcdef"


def seccomp_log(
    *,
    latency: int = 9,
    correctness: str = "ok",
    done: str = "ok",
    include_done: bool = True,
    extra_latency: str = "",
) -> str:
    records = [
        f"TKPERF_RUN schema={SCHEMA} workload=seccomp run_id={RUN_ID} cells=1 clocks=monotonic,process-cpu executor=auto domain=seccomp",
        f"TKPERF_CORRECTNESS schema={SCHEMA} workload=seccomp run_id={RUN_ID} cell=no_filter status={correctness} calls=16 checksum=0123 executor=auto domain=seccomp reason=none proof=no-filter published_delta=unsupported native_executed_delta=unsupported interpreter_executed_delta=unsupported fallback_policy_interpreter_delta=unsupported fallback_translation_delta=unsupported fallback_publication_delta=unsupported fallback_owner_delta=unsupported fallback_unavailable_delta=unsupported jit_rejected_delta=unsupported fallback_delta=unsupported",
        f"TKPERF_WINDOW schema={SCHEMA} workload=seccomp run_id={RUN_ID} cell=no_filter status=ok warmup=2 samples=4 clocks=monotonic,process-cpu executor=auto domain=seccomp",
        f"TKPERF_LATENCY schema={SCHEMA} workload=seccomp run_id={RUN_ID} cell=no_filter status=ok samples=4 wall_p50_ns={latency} wall_p99_ns={latency + 2} cpu_p50_ns=3 cpu_p99_ns=4 sink=0123 executor=auto domain=seccomp",
    ]
    if extra_latency:
        records.append(extra_latency)
    if include_done:
        records.append(
            f"TKPERF_DONE schema={SCHEMA} workload=seccomp run_id={RUN_ID} status={done} cells=1 executor=auto domain=seccomp proof={'verified' if done == 'ok' else done}"
        )
        records.append(f"TKPERF_EXIT schema={SCHEMA} status=0")
    return "\n".join(records) + "\n"


def io_log(
    *, direct_proof: bool = True, unsupported_qd: bool = True,
    target: str = "thekernel", qd8_highwater: int = 3,
) -> str:
    if target == "linux":
        path = "linux-io-uring"
        oracle = "linux-kernel-no-thekernel-counters"
        proof = "linux-active/unsupported-ablation"
    else:
        path = "thekernel-physical-dma"
        oracle = "thekernel-physical-counters"
        proof = "physical-dma"
    records = [
        f"TKPERF_RUN schema={SCHEMA} workload=io-uring-physical run_id={RUN_ID} cells=2 sizes=4096 qd=1,8 ops=read_fixed clocks=monotonic,process-cpu path={path} oracle={oracle}",
        f"TKPERF_DATA schema={SCHEMA} workload=io-uring-physical run_id={RUN_ID} device=/dev/vdb mount=/mnt/thekernel-perf-data fs=ext4 major=8 minor=16 identity=verified mapping=unique-rootfs-extra",
    ]
    qd1_fields = (
        "physical_submitted=1 physical_child_submitted=1 "
        "physical_completed=1 physical_child_completed=1 physical_qd_highwater=1 "
        "physical_extent_highwater=1 "
        "physical_direct_bytes=4096 physical_quarantine=0 direct_hit_delta=1 "
        "direct_fallback_delta=0"
        if target == "thekernel" and direct_proof else ""
    )
    qd1_measurement_fields = (
        "physical_submitted=6 physical_child_submitted=6 "
        "physical_completed=6 physical_child_completed=6 physical_qd_highwater=1 "
        "physical_extent_highwater=1 "
        "physical_direct_bytes=24576 physical_quarantine=0 direct_hit_delta=6 "
        "direct_fallback_delta=0"
        if target == "thekernel" and direct_proof else ""
    )
    evidence = f"path={path} oracle={oracle} proof={proof}"
    records.extend([
        f"TKPERF_CORRECTNESS schema={SCHEMA} workload=io-uring-physical run_id={RUN_ID} cell=read_fixed_size4096_qd1 op=read_fixed size=4096 qd=1 status=ok cqe=1 missing=0 duplicate=0 digest=0123 user_data=verified {evidence} {qd1_fields}",
        f"TKPERF_WINDOW schema={SCHEMA} workload=io-uring-physical run_id={RUN_ID} cell=read_fixed_size4096_qd1 op=read_fixed size=4096 qd=1 status=ok warmup=2 samples=4 clocks=monotonic,process-cpu {evidence} {qd1_measurement_fields}",
        f"TKPERF_LATENCY schema={SCHEMA} workload=io-uring-physical run_id={RUN_ID} cell=read_fixed_size4096_qd1 op=read_fixed size=4096 qd=1 status=ok samples=4 wall_p50_ns=9 wall_p99_ns=11 cpu_p50_ns=3 cpu_p99_ns=4 {evidence} {qd1_measurement_fields}",
    ])
    if unsupported_qd:
        records.extend([
            f"TKPERF_CORRECTNESS schema={SCHEMA} workload=io-uring-physical run_id={RUN_ID} cell=read_fixed_size4096_qd8 op=read_fixed size=4096 qd=8 status=unsupported reason=physical-path-unavailable cqe=0 missing=unsupported duplicate=unsupported digest=unsupported {evidence}",
            f"TKPERF_WINDOW schema={SCHEMA} workload=io-uring-physical run_id={RUN_ID} cell=read_fixed_size4096_qd8 op=read_fixed size=4096 qd=8 status=unsupported warmup=0 samples=0 clocks=monotonic,process-cpu reason=physical-path-unavailable {evidence}",
            f"TKPERF_LATENCY schema={SCHEMA} workload=io-uring-physical run_id={RUN_ID} cell=read_fixed_size4096_qd8 op=read_fixed size=4096 qd=8 status=unsupported samples=0 wall_p50_ns=unsupported wall_p99_ns=unsupported cpu_p50_ns=unsupported cpu_p99_ns=unsupported reason=physical-path-unavailable {evidence}",
        ])
    else:
        qd_fields = (
            f"physical_submitted=8 physical_child_submitted=8 "
            f"physical_completed=8 physical_child_completed=8 physical_qd_highwater={qd8_highwater} "
            "physical_extent_highwater=1 "
            "physical_direct_bytes=32768 physical_quarantine=0 direct_hit_delta=8 "
            "direct_fallback_delta=0"
            if target == "thekernel" and direct_proof else ""
        )
        qd_measurement_fields = (
            f"physical_submitted=48 physical_child_submitted=48 "
            f"physical_completed=48 physical_child_completed=48 physical_qd_highwater={qd8_highwater} "
            "physical_extent_highwater=1 "
            "physical_direct_bytes=196608 physical_quarantine=0 direct_hit_delta=48 "
            "direct_fallback_delta=0"
            if target == "thekernel" and direct_proof else ""
        )
        records.extend([
            f"TKPERF_CORRECTNESS schema={SCHEMA} workload=io-uring-physical run_id={RUN_ID} cell=read_fixed_size4096_qd8 op=read_fixed size=4096 qd=8 status=ok cqe=8 missing=0 duplicate=0 digest=0123 user_data=verified {evidence} {qd_fields}",
            f"TKPERF_WINDOW schema={SCHEMA} workload=io-uring-physical run_id={RUN_ID} cell=read_fixed_size4096_qd8 op=read_fixed size=4096 qd=8 status=ok warmup=2 samples=4 clocks=monotonic,process-cpu {evidence} {qd_measurement_fields}",
            f"TKPERF_LATENCY schema={SCHEMA} workload=io-uring-physical run_id={RUN_ID} cell=read_fixed_size4096_qd8 op=read_fixed size=4096 qd=8 status=ok samples=4 wall_p50_ns=19 wall_p99_ns=21 cpu_p50_ns=5 cpu_p99_ns=6 {evidence} {qd_measurement_fields}",
        ])
    records.append(
        f"TKPERF_DONE schema={SCHEMA} workload=io-uring-physical run_id={RUN_ID} status=ok cells=2 unsupported={1 if unsupported_qd else 0}"
    )
    records.append(f"TKPERF_EXIT schema={SCHEMA} status=0")
    return "\n".join(records) + "\n"


def io_multiextent_qd32_log() -> str:
    evidence = (
        "path=thekernel-physical-dma oracle=thekernel-physical-counters "
        "proof=physical-dma"
    )
    correctness = (
        "physical_submitted=32 physical_child_submitted=512 "
        "physical_completed=32 physical_child_completed=512 "
        "physical_qd_highwater=7 physical_extent_highwater=16 "
        "physical_direct_bytes=8388608 physical_quarantine=0 "
        "direct_hit_delta=32 direct_fallback_delta=0"
    )
    measurement = (
        "physical_submitted=192 physical_child_submitted=3072 "
        "physical_completed=192 physical_child_completed=3072 "
        "physical_qd_highwater=7 physical_extent_highwater=16 "
        "physical_direct_bytes=50331648 physical_quarantine=0 "
        "direct_hit_delta=192 direct_fallback_delta=0"
    )
    records = [
        f"TKPERF_RUN schema={SCHEMA} workload=io-uring-physical run_id={RUN_ID} cells=1 sizes=262144 qd=32 ops=read_fixed clocks=monotonic,process-cpu path=thekernel-physical-dma oracle=thekernel-physical-counters",
        f"TKPERF_DATA schema={SCHEMA} workload=io-uring-physical run_id={RUN_ID} device=/dev/vdb mount=/mnt/thekernel-perf-data fs=ext4 major=8 minor=16 identity=verified mapping=unique-rootfs-extra",
        f"TKPERF_CORRECTNESS schema={SCHEMA} workload=io-uring-physical run_id={RUN_ID} cell=read_fixed_size262144_qd32 op=read_fixed size=262144 qd=32 status=ok cqe=32 missing=0 duplicate=0 digest=0123 user_data=verified {evidence} {correctness}",
        f"TKPERF_WINDOW schema={SCHEMA} workload=io-uring-physical run_id={RUN_ID} cell=read_fixed_size262144_qd32 op=read_fixed size=262144 qd=32 status=ok warmup=2 samples=4 clocks=monotonic,process-cpu {evidence} {measurement}",
        f"TKPERF_LATENCY schema={SCHEMA} workload=io-uring-physical run_id={RUN_ID} cell=read_fixed_size262144_qd32 op=read_fixed size=262144 qd=32 status=ok samples=4 wall_p50_ns=19 wall_p99_ns=21 cpu_p50_ns=5 cpu_p99_ns=6 {evidence} {measurement}",
        f"TKPERF_DONE schema={SCHEMA} workload=io-uring-physical run_id={RUN_ID} status=ok cells=1 unsupported=0",
        f"TKPERF_EXIT schema={SCHEMA} status=0",
    ]
    return "\n".join(records) + "\n"


def network_log(topology: str) -> str:
    records = [
        f"TKPERF_RUN schema={SCHEMA} workload=packet run_id={RUN_ID} cells=1 sizes=64 qd=1 ops=stream-echo clocks=monotonic,process-cpu executor=auto domain=packet topology={topology}",
        f"TKPERF_CORRECTNESS schema={SCHEMA} workload=packet run_id={RUN_ID} cell=packet-filter-off op=stream-echo size=64 qd=1 status=ok reason=none calls=1 missing=0 duplicate=0 checksum=0 executor=auto domain=packet proof=no-filter oracle=accept-half published_delta=unsupported native_executed_delta=unsupported interpreter_executed_delta=unsupported fallback_policy_interpreter_delta=unsupported fallback_translation_delta=unsupported fallback_publication_delta=unsupported fallback_owner_delta=unsupported fallback_unavailable_delta=unsupported jit_rejected_delta=unsupported fallback_delta=unsupported topology={topology} mode=filter-off",
        f"TKPERF_WINDOW schema={SCHEMA} workload=packet run_id={RUN_ID} cell=packet-filter-off op=stream-echo size=64 qd=1 status=ok warmup=2 samples=4 clocks=monotonic,process-cpu topology={topology} mode=filter-off executor=auto domain=packet",
        f"TKPERF_LATENCY schema={SCHEMA} workload=packet run_id={RUN_ID} cell=packet-filter-off op=stream-echo size=64 qd=1 status=ok samples=4 wall_p50_ns=9 wall_p99_ns=11 cpu_p50_ns=3 cpu_p99_ns=4 sink=selftest-echo topology={topology} mode=filter-off executor=auto domain=packet",
        f"TKPERF_DONE schema={SCHEMA} workload=packet run_id={RUN_ID} status=ok cells=1 executor=auto domain=packet topology={topology} proof=verified",
        f"TKPERF_EXIT schema={SCHEMA} status=0",
    ]
    return "\n".join(records) + "\n"


def parse_text(
    text: str, *, workload: str = "seccomp", repeat: int = 1,
    target: str = "thekernel",
):
    with tempfile.TemporaryDirectory() as directory:
        path = Path(directory) / "console.log"
        path.write_text(text, encoding="utf-8")
        return parse_tkperf_log(
            path,
            target=target,
            repeat=repeat,
            expected_workload=workload,
        )


def explicit_seccomp_log(executor: str, *, valid: bool = True) -> str:
    if executor == "jit":
        deltas = {
            "published_delta": 2,
            "native_executed_delta": 4 if valid else 0,
            "interpreter_executed_delta": 0,
            "fallback_policy_interpreter_delta": 0,
            "fallback_translation_delta": 0,
            "fallback_publication_delta": 0,
            "fallback_owner_delta": 0,
            "fallback_unavailable_delta": 0,
            "jit_rejected_delta": 0,
        }
    else:
        deltas = {
            "published_delta": 2,
            "native_executed_delta": 0,
            "interpreter_executed_delta": 4,
            "fallback_policy_interpreter_delta": 4 if valid else 0,
            "fallback_translation_delta": 0,
            "fallback_publication_delta": 0,
            "fallback_owner_delta": 0,
            "fallback_unavailable_delta": 0,
            "jit_rejected_delta": 0,
        }
    deltas["fallback_delta"] = sum(
        deltas[key]
        for key in (
            "fallback_policy_interpreter_delta", "fallback_translation_delta",
            "fallback_publication_delta", "fallback_owner_delta",
            "fallback_unavailable_delta",
        )
    )
    delta_text = " ".join(f"{key}={value}" for key, value in deltas.items())
    return (
        f"TKPERF_RUN schema={SCHEMA} workload=seccomp run_id={RUN_ID} cells=1 "
        f"clocks=monotonic,process-cpu executor={executor} domain=seccomp\n"
        f"TKPERF_CORRECTNESS schema={SCHEMA} workload=seccomp run_id={RUN_ID} "
        f"cell=short status=ok calls=16 checksum=0123 executor={executor} "
        f"domain=seccomp reason=none proof=verified {delta_text}\n"
        f"TKPERF_WINDOW schema={SCHEMA} workload=seccomp run_id={RUN_ID} "
        f"cell=short status=ok warmup=2 samples=4 clocks=monotonic,process-cpu "
        f"executor={executor} domain=seccomp\n"
        f"TKPERF_LATENCY schema={SCHEMA} workload=seccomp run_id={RUN_ID} "
        f"cell=short status=ok samples=4 wall_p50_ns=9 wall_p99_ns=11 "
        f"cpu_p50_ns=3 cpu_p99_ns=4 sink=0123 executor={executor} domain=seccomp\n"
        f"TKPERF_DONE schema={SCHEMA} workload=seccomp run_id={RUN_ID} "
        f"status=ok cells=1 executor={executor} domain=seccomp proof=verified\n"
        f"TKPERF_EXIT schema={SCHEMA} status=0\n"
    )


class SubsystemBaselineTests(unittest.TestCase):
    def test_actual_helper_fixtures_parse_with_exit_appended(self) -> None:
        seccomp = parse_text(
            (Path(__file__).parent / "fixtures" / "seccomp-perf-auto.txt").read_text()
            + f"TKPERF_EXIT schema={SCHEMA} status=0\n"
        )
        self.assertEqual(seccomp.status, "ok")
        self.assertTrue(seccomp.claim_degraded)
        self.assertTrue(eligible_for_stats(seccomp, runner_returncode=0, pin_valid=True))

        packet = parse_text(
            (Path(__file__).parent / "fixtures" / "packet-perf-auto.txt").read_text()
            + f"TKPERF_EXIT schema={SCHEMA} status=0\n",
            workload="packet",
        )
        self.assertEqual(packet.done_status, "unsupported")
        self.assertFalse(eligible_for_stats(packet, runner_returncode=0, pin_valid=True))

    def test_explicit_executor_proof_deltas_are_required(self) -> None:
        for executor in ("jit", "interpreter"):
            with self.subTest(executor=executor):
                self.assertTrue(
                    eligible_for_stats(
                        parse_text(explicit_seccomp_log(executor)),
                        runner_returncode=0,
                        pin_valid=True,
                    )
                )
                self.assertFalse(
                    eligible_for_stats(
                        parse_text(explicit_seccomp_log(executor, valid=False)),
                        runner_returncode=0,
                        pin_valid=True,
                    )
                )
        with self.assertRaisesRegex(BaselineError, "missing proof"):
            parse_text(explicit_seccomp_log("jit").replace(" proof=verified", ""))

    def test_real_helper_protocol_requires_all_completion_gates(self) -> None:
        guest = parse_text(seccomp_log(), repeat=2)
        self.assertEqual(guest.status, "ok")
        self.assertEqual(guest.correctness, "ok")
        self.assertEqual(guest.samples[0].latency_samples, 4)
        self.assertTrue(eligible_for_stats(guest, runner_returncode=0, pin_valid=True))
        self.assertFalse(eligible_for_stats(guest, runner_returncode=1, pin_valid=True))
        self.assertFalse(eligible_for_stats(guest, runner_returncode=0, pin_valid=False))
        summary = summarize_samples(guest.samples)[0]
        self.assertEqual(summary["repeat"], 2)
        self.assertEqual(summary["throughput_ops_per_sec"], 1_000_000_000 // 9)

    def test_done_or_cell_correctness_failure_is_not_a_measurement(self) -> None:
        missing_done = parse_text(seccomp_log(include_done=False))
        self.assertFalse(missing_done.done)
        self.assertFalse(eligible_for_stats(missing_done, runner_returncode=0, pin_valid=True))
        failed_done = parse_text(seccomp_log(done="fail"))
        self.assertFalse(eligible_for_stats(failed_done, runner_returncode=0, pin_valid=True))
        failed_correctness = parse_text(seccomp_log(correctness="fail"))
        self.assertFalse(eligible_for_stats(failed_correctness, runner_returncode=0, pin_valid=True))
        mixed_cell = parse_text(
            seccomp_log().replace("cell=no_filter status=ok warmup=2", "cell=no_filter status=unsupported warmup=2")
        )
        self.assertFalse(eligible_for_stats(mixed_cell, runner_returncode=0, pin_valid=True))

    def test_exit_marker_is_mandatory_and_must_be_zero(self) -> None:
        missing = parse_text(seccomp_log().replace(f"TKPERF_EXIT schema={SCHEMA} status=0\n", ""))
        self.assertEqual(missing.error, "missing TKPERF_EXIT")
        self.assertFalse(eligible_for_stats(missing, runner_returncode=0, pin_valid=True))
        nonzero = parse_text(seccomp_log().replace("TKPERF_EXIT schema=thekernel-perf-v1 status=0", "TKPERF_EXIT schema=thekernel-perf-v1 status=7"))
        self.assertEqual(nonzero.error, "nonzero TKPERF_EXIT")
        self.assertFalse(eligible_for_stats(nonzero, runner_returncode=0, pin_valid=True))
        with self.assertRaisesRegex(BaselineError, "appears before TKPERF_DONE"):
            parse_text(
                seccomp_log(include_done=False)
                + f"TKPERF_EXIT schema={SCHEMA} status=0\n"
            )

    def test_schema_and_duplicate_latency_are_strict(self) -> None:
        with self.assertRaisesRegex(BaselineError, "unsupported TKPERF_RUN schema"):
            parse_text(seccomp_log().replace(SCHEMA, "thekernel-perf-v0"))
        duplicate = seccomp_log(
            extra_latency=(
                f"TKPERF_LATENCY schema={SCHEMA} workload=seccomp run_id={RUN_ID} "
                "cell=no_filter status=ok samples=4 wall_p50_ns=9 wall_p99_ns=11 "
                "cpu_p50_ns=3 cpu_p99_ns=4 sink=0123 executor=auto domain=seccomp"
            )
        )
        with self.assertRaisesRegex(BaselineError, "duplicate latency"):
            parse_text(duplicate)

    def test_helper_error_keeps_parenthesized_errno_context_parseable(self) -> None:
        guest = parse_text(
            f"TKPERF_RUN schema={SCHEMA} workload=seccomp run_id={RUN_ID} cells=1 clocks=monotonic,process-cpu executor=auto domain=seccomp\n"
            f"TKPERF_ERROR schema={SCHEMA} workload=seccomp stage=install-filter errno=95 (Operation not supported)\n"
        )
        self.assertEqual(guest.status, "error")
        self.assertEqual(guest.error, "install-filter")
        self.assertFalse(eligible_for_stats(guest, runner_returncode=0, pin_valid=True))

    def test_thekernel_qd8_unsupported_is_not_formal_and_physical_proof_is_strict(self) -> None:
        self.assertEqual(
            subsystem_baseline._physical_extent_highwater_expected(262144, 1), 16
        )
        self.assertEqual(
            subsystem_baseline._physical_extent_highwater_expected(262144, 8), 16
        )
        synchronous = parse_text(io_log(), workload="io-uring-physical")
        self.assertEqual(synchronous.status, "unsupported")
        self.assertEqual(len(synchronous.samples), 1)
        self.assertFalse(eligible_for_stats(synchronous, runner_returncode=0, pin_valid=True))

        with self.assertRaisesRegex(BaselineError, "missing physical proof"):
            parse_text(io_log(direct_proof=False, unsupported_qd=False), workload="io-uring-physical")

        physical = parse_text(
            io_log(direct_proof=True, unsupported_qd=False), workload="io-uring-physical"
        )
        self.assertEqual(len(physical.samples), 2)
        self.assertTrue(eligible_for_stats(physical, runner_returncode=0, pin_valid=True))
        self.assertEqual(
            dict(physical.samples[0].attributes)["physical_submitted"], "6"
        )
        self.assertEqual(
            dict(physical.samples[1].attributes)["physical_submitted"], "48"
        )
        self.assertEqual(
            dict(physical.samples[1].attributes)["physical_qd_highwater"], "3"
        )

        # Requested QD is distinct from achieved live-owner depth, but a
        # QD>1 performance cell must prove actual overlap rather than present
        # a fully serial path as asynchronous queue depth.
        with self.assertRaisesRegex(BaselineError, "invalid TheKernel physical proof"):
            parse_text(
                io_log(direct_proof=True, unsupported_qd=False, qd8_highwater=1),
                workload="io-uring-physical",
            )
        with self.assertRaisesRegex(BaselineError, "invalid TheKernel physical proof"):
            parse_text(
                io_log(direct_proof=True, unsupported_qd=False, qd8_highwater=0),
                workload="io-uring-physical",
            )
        with self.assertRaisesRegex(BaselineError, "invalid TheKernel physical proof"):
            parse_text(
                io_log(direct_proof=True, unsupported_qd=False, qd8_highwater=9),
                workload="io-uring-physical",
            )
        with self.assertRaisesRegex(BaselineError, "invalid TheKernel physical proof"):
            parse_text(
                io_log(direct_proof=True, unsupported_qd=False).replace(
                    "physical_qd_highwater=1", "physical_qd_highwater=2"
                ),
                workload="io-uring-physical",
            )
        with self.assertRaisesRegex(BaselineError, "invalid TheKernel physical proof"):
            parse_text(
                io_log(direct_proof=True, unsupported_qd=False).replace(
                    "physical_extent_highwater=1", "physical_extent_highwater=2", 1
                ),
                workload="io-uring-physical",
            )

        stale_measurement_proof = io_log(
            direct_proof=True, unsupported_qd=False
        ).replace(
            "physical_submitted=6 physical_child_submitted=6 "
            "physical_completed=6 physical_child_completed=6 physical_qd_highwater=1 "
            "physical_extent_highwater=1 "
            "physical_direct_bytes=24576 physical_quarantine=0 direct_hit_delta=6 "
            "direct_fallback_delta=0",
            "physical_submitted=1 physical_child_submitted=1 "
            "physical_completed=1 physical_child_completed=1 physical_qd_highwater=1 "
            "physical_extent_highwater=1 "
            "physical_direct_bytes=4096 physical_quarantine=0 direct_hit_delta=1 "
            "direct_fallback_delta=0",
        )
        with self.assertRaisesRegex(BaselineError, "invalid TheKernel physical proof"):
            parse_text(stale_measurement_proof, workload="io-uring-physical")

        mismatched_window = io_log(
            direct_proof=True, unsupported_qd=False
        ).replace(
            "cell=read_fixed_size4096_qd1 op=read_fixed size=4096 qd=1 status=ok "
            "samples=4 wall_p50_ns=9",
            "cell=read_fixed_size4096_qd1 op=read_fixed size=4096 qd=1 status=ok "
            "samples=5 wall_p50_ns=9",
        )
        with self.assertRaisesRegex(BaselineError, "does not match the physical measurement window"):
            parse_text(mismatched_window, workload="io-uring-physical")

        for replacement in (
            ("physical_direct_bytes=32768", "physical_direct_bytes=32767"),
            ("direct_fallback_delta=0", "direct_fallback_delta=1"),
        ):
            with self.subTest(replacement=replacement):
                with self.assertRaisesRegex(BaselineError, "invalid TheKernel physical proof"):
                    parse_text(
                        io_log(direct_proof=True, unsupported_qd=False).replace(*replacement),
                        workload="io-uring-physical",
                    )

    def test_thekernel_qd32_multiextent_child_oracle_scales_with_batches(self) -> None:
        guest = parse_text(
            io_multiextent_qd32_log(), workload="io-uring-physical"
        )
        self.assertEqual(guest.status, "ok")
        self.assertTrue(eligible_for_stats(guest, runner_returncode=0, pin_valid=True))
        sample = guest.samples[0]
        attributes = dict(sample.attributes)
        self.assertEqual(attributes["physical_submitted"], "192")
        self.assertEqual(attributes["physical_child_submitted"], "3072")
        self.assertEqual(attributes["physical_completed"], "192")
        self.assertEqual(attributes["physical_child_completed"], "3072")
        self.assertEqual(attributes["physical_extent_highwater"], "16")

        forged = io_multiextent_qd32_log().replace(
            "physical_child_completed=512", "physical_child_completed=511", 1
        )
        with self.assertRaisesRegex(BaselineError, "invalid TheKernel physical proof"):
            parse_text(forged, workload="io-uring-physical")

    def test_linux_physical_cells_remain_runnable_without_thekernel_counters(self) -> None:
        linux = parse_text(
            io_log(direct_proof=False, unsupported_qd=False, target="linux"),
            workload="io-uring-physical",
            target="linux",
        )
        self.assertEqual(linux.status, "ok")
        self.assertEqual(len(linux.samples), 2)
        self.assertTrue(eligible_for_stats(linux, runner_returncode=0, pin_valid=True))
        forged = io_log(direct_proof=True, unsupported_qd=False, target="linux").replace(
            "path=linux-io-uring oracle=linux-kernel-no-thekernel-counters proof=linux-active/unsupported-ablation ",
            "path=linux-io-uring oracle=linux-kernel-no-thekernel-counters proof=linux-active/unsupported-ablation physical_submitted=1 ",
            1,
        )
        with self.assertRaisesRegex(BaselineError, "must not claim TheKernel physical counters"):
            parse_text(forged, workload="io-uring-physical", target="linux")

    def test_io_matrix_coverage_is_exact_and_label_independent(self) -> None:
        renamed = io_log().replace(
            "cell=read_fixed_size4096_qd1 op=read_fixed",
            "cell=alias_for_same_tuple op=read_fixed",
            1,
        )
        with self.assertRaisesRegex(BaselineError, "duplicates TKPERF matrix tuple"):
            parse_text(renamed, workload="io-uring-physical")

        missing = "\n".join(
            line for line in io_log().splitlines() if "qd8" not in line
        ) + "\n"
        parsed = parse_text(missing, workload="io-uring-physical")
        self.assertEqual(parsed.error, "matrix coverage mismatch")

        extra = io_log().replace("qd=8", "qd=16")
        with self.assertRaisesRegex(BaselineError, "outside the TKPERF_RUN"):
            parse_text(extra, workload="io-uring-physical")

    def test_io_data_marker_rejects_rootfs_tmp_mounts(self) -> None:
        with self.assertRaisesRegex(BaselineError, "invalid device or mount"):
            parse_text(
                io_log().replace("mount=/mnt/thekernel-perf-data", "mount=/tmp/perf-data"),
                workload="io-uring-physical",
            )

    def test_io_data_marker_is_bound_to_requested_device_and_mount(self) -> None:
        guest = parse_text(io_log(), workload="io-uring-physical")
        self.assertTrue(
            _data_proof_matches(
                guest,
                data_device="/dev/vdb",
                data_dir="/mnt/thekernel-perf-data",
            )
        )
        self.assertFalse(
            _data_proof_matches(
                guest,
                data_device="/dev/vdc",
                data_dir="/mnt/thekernel-perf-data",
            )
        )

    def test_io_formal_data_device_is_fixed_to_runner_extra_disk(self) -> None:
        args = argparse.Namespace(
            subsystem="io-uring-physical",
            guest_args=[],
            data_dir="/mnt/thekernel-perf-data",
            data_device="/dev/vda",
            shutdown_command="poweroff",
        )
        with self.assertRaisesRegex(BaselineError, "fixed to /dev/vdb"):
            _build_guest_command(args, "/helper")
        args.data_device = "/dev/vdb"
        args.guest_args = ["--data-device", "/dev/vda"]
        with self.assertRaisesRegex(BaselineError, "runner-owned"):
            _build_guest_command(args, "/helper")
        with self.assertRaisesRegex(BaselineError, "invalid device or mount"):
            parse_text(
                io_log().replace("device=/dev/vdb", "device=/dev/vda"),
                workload="io-uring-physical",
            )
        with self.assertRaisesRegex(BaselineError, "does not prove an ext4 data disk"):
            parse_text(
                io_log().replace("mapping=unique-rootfs-extra", "mapping=serial-other"),
                workload="io-uring-physical",
            )

    def test_runner_receipt_binds_guest_data_proof_to_extra_drive(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            rootfs = root / "rootfs.img"
            extra = root / "extra.img"
            rootfs.write_bytes(b"rootfs")
            extra.write_bytes(b"ext4")

            def evidence(path: Path) -> dict[str, object]:
                data = path.read_bytes()
                return {
                    "path": str(path.resolve()),
                    "size_bytes": len(data),
                    "sha256": hashlib.sha256(data).hexdigest(),
                }

            receipt = root / "qemu-receipt.json"
            receipt.write_text(
                json.dumps(
                    {
                        "extra_block_source": evidence(extra),
                        "rootfs_runtime_before": evidence(rootfs),
                        "extra_block_runtime_before": evidence(extra),
                        "rootfs_runtime_after": evidence(rootfs),
                        "extra_block_runtime_after": evidence(extra),
                        "command": [
                            "qemu-system-x86_64",
                            "-drive",
                            f"file={rootfs.resolve()},if=none,format=raw,id=rootfs",
                            "-device",
                            "virtio-blk-pci,drive=rootfs",
                            "-drive",
                            f"file={extra.resolve()},if=none,format=raw,id=extra",
                            "-device",
                            "virtio-blk-pci,drive=extra",
                        ],
                    }
                ),
                encoding="utf-8",
            )
            self.assertTrue(
                _extra_block_receipt_matches(
                    receipt, extra_block=extra, data_device="/dev/vdb"
                )
            )
            valid_payload = json.loads(receipt.read_text(encoding="utf-8"))
            for adversarial in (
                lambda command: command[:3] + [
                    "-device", "virtio-blk-pci,drive=other"
                ] + command[3:],
                lambda command: command + [
                    "-device", "virtio-blk-pci,drive=extra"
                ],
                lambda command: command[:4] + [
                    command[8]
                ] + command[5:8] + [command[4]],
            ):
                candidate = json.loads(json.dumps(valid_payload))
                candidate["command"] = adversarial(candidate["command"])
                receipt.write_text(json.dumps(candidate), encoding="utf-8")
                self.assertFalse(
                    _extra_block_receipt_matches(
                        receipt, extra_block=extra, data_device="/dev/vdb"
                    )
                )
            drive_reordered = json.loads(json.dumps(valid_payload))
            drive_reordered["command"] = (
                drive_reordered["command"][:1]
                + drive_reordered["command"][5:7]
                + drive_reordered["command"][1:5]
                + drive_reordered["command"][7:]
            )
            receipt.write_text(json.dumps(drive_reordered), encoding="utf-8")
            self.assertTrue(
                _extra_block_receipt_matches(
                    receipt, extra_block=extra, data_device="/dev/vdb"
                )
            )
            receipt.write_text(json.dumps(valid_payload), encoding="utf-8")
            receipt.write_text(
                receipt.read_text(encoding="utf-8").replace("id=extra", "id=rootfs"),
                encoding="utf-8",
            )
            self.assertFalse(
                _extra_block_receipt_matches(
                    receipt, extra_block=extra, data_device="/dev/vdb"
                )
            )
            payload = json.loads(receipt.read_text(encoding="utf-8"))
            payload["command"] = [
                item.replace(str(extra.resolve()), str(rootfs.resolve()))
                if item.startswith("file=") else item
                for item in payload["command"]
            ]
            payload["command"].insert(0, "--opaque=id=extra")
            receipt.write_text(json.dumps(payload), encoding="utf-8")
            self.assertFalse(
                _extra_block_receipt_matches(
                    receipt, extra_block=extra, data_device="/dev/vdb"
                )
            )

    def test_topology_missing_sibling_file_is_not_a_singleton(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "online").write_text("0\n", encoding="ascii")
            (root / "cpu0" / "topology").mkdir(parents=True)
            with self.assertRaisesRegex(BaselineError, "thread_siblings_list"):
                read_host_topology(root)

    def test_topology_sibling_sets_must_be_reciprocal_equivalence_classes(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "online").write_text("0-2\n", encoding="ascii")
            sibling_lists = {0: "0-1", 1: "0-1,2", 2: "1-2"}
            for cpu, siblings in sibling_lists.items():
                topology = root / f"cpu{cpu}" / "topology"
                topology.mkdir(parents=True)
                (topology / "thread_siblings_list").write_text(
                    siblings + "\n", encoding="ascii"
                )
            with self.assertRaisesRegex(TopologyUnavailable, "equivalence|reciprocal"):
                read_host_topology(root)

    def test_main_maps_missing_topology_to_explicit_unsupported(self) -> None:
        class FakeParser:
            @staticmethod
            def parse_args(_argv):
                return argparse.Namespace(
                    command="run", iterations=1, warmup=0, repeat=1, cpus=1,
                    func=lambda _args: (_ for _ in ()).throw(
                        TopologyUnavailable("missing sibling capability")
                    ),
                )

        with patch.object(subsystem_baseline, "build_parser", return_value=FakeParser()):
            self.assertEqual(subsystem_baseline.main(["run"]), 78)
        with patch.object(scheduler_baseline, "build_parser", return_value=FakeParser()):
            self.assertEqual(scheduler_baseline.main(["run"]), 78)

    def test_tkperf_bare_known_markers_fail_closed(self) -> None:
        for marker in (
            "TKPERF_RUN", "TKPERF_CORRECTNESS", "TKPERF_WINDOW", "TKPERF_LATENCY",
            "TKPERF_DONE", "TKPERF_ERROR", "TKPERF_EXIT", "TKPERF_DATA",
        ):
            with self.subTest(marker=marker):
                with self.assertRaisesRegex(BaselineError, "malformed"):
                    parse_text(seccomp_log() + marker + "\n")

    def test_cost_capabilities_are_explicit_and_never_fake_measurements(self) -> None:
        capabilities = host_cost_capabilities()
        self.assertEqual(capabilities["schema"], "thekernel-perf-cost-capabilities-v1")
        metrics = capabilities["metrics"]
        for name in ("cycles", "instructions", "cache_misses", "branch_misses", "llc_hitm"):
            self.assertIn(name, metrics)
            self.assertIn("status", metrics[name])
            self.assertFalse(metrics[name]["measured"])
            self.assertEqual(metrics[name]["measurement_status"], "not-measured")
        self.assertEqual(capabilities["measurement_status"], "not-measured")
        self.assertEqual(capabilities["throughput"]["concurrent"], False)

    def test_subsystem_evidence_class_requires_a_sample_not_capability_probe(self) -> None:
        self.assertEqual(subsystem_baseline.subsystem_evidence_class(()), "not-measured")
        sample = subsystem_baseline.Sample(
            "thekernel", 1, "seccomp", RUN_ID, "no_filter", None, None, None,
            1, 1, 1, 10, 20, 3, 4,
        )
        self.assertEqual(
            subsystem_baseline.subsystem_evidence_class((sample,)),
            "cpu-cost-evidenced",
        )

    def test_pin_validator_rejects_null_external_container_before_iteration(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "thread-pinning.json"
            path.write_text(json.dumps({
                "schema": "thekernel-kvm-thread-pinning-v4",
                "declared_external_backends": [],
                "external_processes": None,
            }), encoding="utf-8")
            self.assertFalse(_pin_report_valid(
                path,
                expected_vcpu_count=1,
                vcpu_cpus=(0,),
                io_cpus=(1,),
                backend_cpus=(),
            ))

    def test_zero_runner_with_missing_pin_proof_is_unsupported(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "thread-pinning.json"
            write_report(
                path,
                pid=10,
                expected_vcpu_count=1,
                vcpus={"0": {"tid": 11, "name": "CPU 0/KVM", "affinity": [2]}},
                io_threads=[{"tid": 12, "name": "IO thread", "affinity": [3]}],
                requested_vcpu=(2,),
                requested_io=(3,),
                housekeeping=(0, 1),
                measurement_smt_siblings=(2, 3),
                qemu_main={"tid": 10, "name": "qemu", "affinity": [0, 1]},
                unknown_threads=[{"tid": 14, "name": "mystery", "affinity": [1]}],
                ptrace_clone_events=False,
                clone_event_count=0,
            )
            self.assertEqual(pin_report_failure_status(path, False), "unsupported")
            path.write_text("{not-json\n", encoding="utf-8")
            self.assertEqual(pin_report_failure_status(path, False), "pinning-error")

    def test_housekeeping_capability_gaps_are_unsupported(self) -> None:
        def write_valid(path: Path) -> None:
            write_report(
                path,
                pid=10,
                expected_vcpu_count=1,
                vcpus={"0": {"tid": 11, "name": "CPU 0/KVM", "affinity": [2]}},
                io_threads=[{"tid": 12, "name": "IO thread", "affinity": [3]}],
                requested_vcpu=(2,),
                requested_io=(3,),
                housekeeping=(0, 1),
                measurement_smt_siblings=(2, 3),
                qemu_main={"tid": 10, "name": "qemu", "affinity": [0, 1]},
                unknown_threads=[{"tid": 14, "name": "mystery", "affinity": [1]}],
                ptrace_clone_events=True,
                clone_event_count=1,
                exit_readback_tids=(10, 11, 12, 14),
                exit_readback_proof=True,
            )

        for field in (
            "process_inherited_housekeeping",
            "new_threads_inherit_housekeeping",
        ):
            with self.subTest(field=field):
                with tempfile.TemporaryDirectory() as directory:
                    path = Path(directory) / "thread-pinning.json"
                    write_valid(path)
                    payload = json.loads(path.read_text(encoding="utf-8"))
                    payload[field] = False
                    path.write_text(json.dumps(payload), encoding="utf-8")
                    self.assertFalse(_pin_report_valid(
                        path,
                        expected_vcpu_count=1,
                        vcpu_cpus=(2,),
                        io_cpus=(3,),
                        backend_cpus=(),
                    ))
                    self.assertEqual(pin_report_failure_status(path, False), "unsupported")

        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "thread-pinning.json"
            write_valid(path)
            payload = json.loads(path.read_text(encoding="utf-8"))
            payload["housekeeping_status"] = "not_reported"
            path.write_text(json.dumps(payload), encoding="utf-8")
            self.assertFalse(_pin_report_valid(
                path,
                expected_vcpu_count=1,
                vcpu_cpus=(2,),
                io_cpus=(3,),
                backend_cpus=(),
            ))
            self.assertEqual(pin_report_failure_status(path, False), "unsupported")

        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "thread-pinning.json"
            write_valid(path)
            payload = json.loads(path.read_text(encoding="utf-8"))
            payload["process_inherited_housekeeping"] = None
            path.write_text(json.dumps(payload), encoding="utf-8")
            self.assertEqual(pin_report_failure_status(path, False), "pinning-error")

    def test_contradictory_clone_proof_is_pinning_error(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "thread-pinning.json"
            write_report(
                path,
                pid=10,
                expected_vcpu_count=1,
                vcpus={"0": {"tid": 11, "name": "CPU 0/KVM", "affinity": [2]}},
                io_threads=[{"tid": 12, "name": "IO thread", "affinity": [3]}],
                requested_vcpu=(2,),
                requested_io=(3,),
                housekeeping=(0, 1),
                measurement_smt_siblings=(2, 3),
                qemu_main={"tid": 10, "name": "qemu", "affinity": [0, 1]},
                unknown_threads=[{"tid": 14, "name": "mystery", "affinity": [1]}],
                ptrace_clone_events=True,
                clone_event_count=1,
                exit_readback_tids=(10, 11, 12, 14),
                exit_readback_proof=True,
            )
            payload = json.loads(path.read_text(encoding="utf-8"))
            payload["clone_event_count"] = 0
            path.write_text(json.dumps(payload), encoding="utf-8")
            self.assertFalse(_pin_report_valid(
                path,
                expected_vcpu_count=1,
                vcpu_cpus=(2,),
                io_cpus=(3,),
                backend_cpus=(),
            ))
            self.assertEqual(pin_report_failure_status(path, False), "pinning-error")

    def test_external_process_pid_cannot_alias_vcpu_io_or_unknown_role(self) -> None:
        cases = (
            ("vcpu", 11, [2]),
            ("io", 12, [3]),
            ("unknown", 14, [1]),
        )
        for role, pid, affinity in cases:
            with self.subTest(role=role):
                with tempfile.TemporaryDirectory() as directory:
                    path = Path(directory) / "thread-pinning.json"
                    process = {
                        "pid": pid,
                        "tgid": pid,
                        "exe": "/bin/true",
                        "starttime": 300 + pid,
                        "main_tid": pid,
                        "name": "renamed-helper",
                        "affinity": affinity,
                        "backend_authorized": False,
                    }
                    unknown_threads = (
                        [{"tid": 14, "name": "mystery", "affinity": [1]}]
                        if role != "unknown"
                        else [{"tid": 14, "name": "renamed-helper", "affinity": [1]}]
                    )
                    write_report(
                        path,
                        pid=10,
                        expected_vcpu_count=1,
                        vcpus={"0": {"tid": 11, "name": "CPU 0/KVM", "affinity": [2]}},
                        io_threads=[{"tid": 12, "name": "IO thread", "affinity": [3]}],
                        requested_vcpu=(2,),
                        requested_io=(3,),
                        housekeeping=(0, 1),
                        measurement_smt_siblings=(2, 3),
                        external_processes=[process],
                        qemu_main={"tid": 10, "name": "qemu", "affinity": [0, 1]},
                        unknown_threads=unknown_threads,
                        ptrace_clone_events=True,
                        clone_event_count=1,
                        exit_readback_tids=(10, 11, 12, 14),
                        exit_readback_proof=True,
                    )
                    self.assertFalse(_pin_report_valid(
                        path,
                        expected_vcpu_count=1,
                        vcpu_cpus=(2,),
                        io_cpus=(3,),
                        backend_cpus=(),
                    ))
                    self.assertEqual(pin_report_failure_status(path, False), "pinning-error")

    def test_external_backend_and_qemu_main_tids_cannot_be_reused(self) -> None:
        identity = {
            "pid": 5000,
            "tgid": 5000,
            "exe": "/usr/bin/passt",
            "starttime": 199,
        }
        for authorized in (False, True):
            with self.subTest(alias="internal-backend", authorized=authorized):
                with tempfile.TemporaryDirectory() as directory:
                    path = Path(directory) / "thread-pinning.json"
                    process = {
                        **identity,
                        "main_tid": 5000,
                        "name": "renamed-helper",
                        "affinity": [1],
                        "backend_authorized": authorized,
                    }
                    write_report(
                        path,
                        pid=10,
                        expected_vcpu_count=1,
                        vcpus={"0": {"tid": 11, "name": "CPU 0/KVM", "affinity": [2]}},
                        io_threads=[{"tid": 12, "name": "IO thread", "affinity": [3]}],
                        requested_vcpu=(2,),
                        requested_io=(3,),
                        housekeeping=(0, 1),
                        measurement_smt_siblings=(2, 3),
                        backend_threads=[{
                            "tid": 5000,
                            "name": "internal-backend",
                            "affinity": [1],
                            "tgid": 10,
                        }],
                        external_processes=[process],
                        declared_external_backends=(identity,) if authorized else (),
                        qemu_main={"tid": 10, "name": "qemu", "affinity": [0, 1]},
                        unknown_threads=[{"tid": 14, "name": "mystery", "affinity": [1]}],
                        ptrace_clone_events=True,
                        clone_event_count=1,
                        exit_readback_tids=(10, 11, 12, 14, 5000),
                        exit_readback_proof=True,
                    )
                    self.assertFalse(_pin_report_valid(
                        path,
                        expected_vcpu_count=1,
                        vcpu_cpus=(2,),
                        io_cpus=(3,),
                        backend_cpus=(),
                        expected_external_backends=(identity,) if authorized else (),
                    ))
                    self.assertEqual(pin_report_failure_status(path, False), "pinning-error")

        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "thread-pinning.json"
            write_report(
                path,
                pid=10,
                expected_vcpu_count=1,
                vcpus={"0": {"tid": 11, "name": "CPU 0/KVM", "affinity": [2]}},
                io_threads=[{"tid": 12, "name": "IO thread", "affinity": [3]}],
                requested_vcpu=(2,),
                requested_io=(3,),
                housekeeping=(0, 1),
                measurement_smt_siblings=(2, 3),
                qemu_main={"tid": 99, "name": "qemu", "affinity": [0, 1]},
                unknown_threads=[{"tid": 14, "name": "mystery", "affinity": [1]}],
                ptrace_clone_events=True,
                clone_event_count=1,
                exit_readback_tids=(11, 12, 14, 99),
                exit_readback_proof=True,
            )
            self.assertFalse(_pin_report_valid(
                path,
                expected_vcpu_count=1,
                vcpu_cpus=(2,),
                io_cpus=(3,),
                backend_cpus=(),
            ))
            self.assertEqual(pin_report_failure_status(path, False), "pinning-error")

        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "thread-pinning.json"
            write_report(
                path,
                pid=10,
                expected_vcpu_count=1,
                vcpus={"0": {"tid": 11, "name": "CPU 0/KVM", "affinity": [2]}},
                io_threads=[{"tid": 12, "name": "IO thread", "affinity": [3]}],
                requested_vcpu=(2,),
                requested_io=(3,),
                housekeeping=(0, 1),
                measurement_smt_siblings=(2, 3),
                external_processes=[{
                    "pid": 99,
                    "tgid": 99,
                    "exe": "/bin/true",
                    "starttime": 200,
                    "main_tid": 99,
                    "name": "renamed-helper",
                    "affinity": [1],
                    "backend_authorized": False,
                }],
                qemu_main={"tid": 99, "name": "qemu", "affinity": [0, 1]},
                unknown_threads=[{"tid": 14, "name": "mystery", "affinity": [1]}],
                ptrace_clone_events=True,
                clone_event_count=1,
                exit_readback_tids=(11, 12, 14, 99),
                exit_readback_proof=True,
            )
            self.assertFalse(_pin_report_valid(
                path,
                expected_vcpu_count=1,
                vcpu_cpus=(2,),
                io_cpus=(3,),
                backend_cpus=(),
            ))
            self.assertEqual(pin_report_failure_status(path, False), "pinning-error")

    def test_pin_validator_rejects_conflicting_external_pid_records(self) -> None:
        identity = {
            "pid": 5002,
            "tgid": 5002,
            "exe": "/usr/bin/passt",
            "starttime": 104,
        }
        process = {
            **identity,
            "main_tid": 5002,
            "name": "helper",
            "affinity": [1],
            "backend_authorized": True,
        }
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "thread-pinning.json"
            write_report(
                path,
                pid=10,
                expected_vcpu_count=1,
                vcpus={"0": {"tid": 11, "name": "CPU 0/KVM", "affinity": [2]}},
                io_threads=[{"tid": 12, "name": "IO thread", "affinity": [3]}],
                requested_vcpu=(2,),
                requested_io=(3,),
                housekeeping=(0, 1),
                measurement_smt_siblings=(2, 3),
                backend_threads=[{
                    "tid": 5002,
                    "name": "helper",
                    "affinity": [1],
                    "tgid": 5002,
                }],
                external_processes=[process],
                declared_external_backends=(identity,),
                qemu_main={"tid": 10, "name": "qemu", "affinity": [0, 1]},
                unknown_threads=[{"tid": 14, "name": "mystery", "affinity": [1]}],
                ptrace_clone_events=True,
                clone_event_count=1,
                exit_readback_tids=(10, 11, 12, 14, 5002),
                exit_readback_proof=True,
            )
            payload = json.loads(path.read_text(encoding="utf-8"))
            payload["external_processes"].append({
                **process,
                "name": "helper-reused",
            })
            path.write_text(json.dumps(payload), encoding="utf-8")
            self.assertFalse(_pin_report_valid(
                path,
                expected_vcpu_count=1,
                vcpu_cpus=(2,),
                io_cpus=(3,),
                backend_cpus=(),
                expected_external_backends=(identity,),
            ))
            self.assertEqual(pin_report_failure_status(path, False), "pinning-error")

    def test_network_selftest_user_loopback_are_not_formal_cells(self) -> None:
        for topology in ("selftest", "user", "slirp", "loopback"):
            guest = parse_text(network_log(topology), workload="packet")
            self.assertFalse(eligible_for_stats(guest, runner_returncode=0, pin_valid=True))

    def test_repeat_is_not_pooled(self) -> None:
        first = parse_text(seccomp_log(latency=11), repeat=1)
        second = parse_text(seccomp_log(latency=22), repeat=2)
        rows = summarize_samples((*first.samples, *second.samples))
        self.assertEqual([row["repeat"] for row in rows], [1, 2])
        self.assertEqual([row["wall_p50_ns"] for row in rows], [11, 22])

    def test_formal_subsystem_maps_to_the_real_helper(self) -> None:
        args = argparse.Namespace(
            subsystem="seccomp",
            guest_program=None,
            guest_args=[],
            shutdown_command="poweroff",
        )
        self.assertEqual(_helper_for(args), "/opt/thekernel-tests/bin/thekernel-seccomp-perf")
        command = _build_guest_command(args, _helper_for(args)).decode()
        self.assertIn("/opt/thekernel-tests/bin/thekernel-seccomp-perf\n", command)
        self.assertNotIn("--workload", command)
        self.assertNotIn("echo TKPERF_EXIT", command)
        self.assertLess(command.index('TKPERF_""EXIT'), command.index("poweroff"))

    def test_formal_setup_rejects_user_network_before_kvm(self) -> None:
        args = argparse.Namespace(subsystem="seccomp", network="user")
        with self.assertRaisesRegex(BaselineError, "require passt or tap-vhost"):
            run_command(args)

    def test_packet_formal_requires_tap_peer_and_never_selftest(self) -> None:
        args = argparse.Namespace(subsystem="packet", network="passt")
        with self.assertRaisesRegex(BaselineError, "require tap-vhost"):
            run_command(args)
        args = argparse.Namespace(guest_args=["--selftest"], packet_interface="eth0")
        with self.assertRaisesRegex(BaselineError, "rejects --selftest"):
            _packet_guest_args(args, peer_mac="02:00:00:00:00:01")
        args = argparse.Namespace(guest_args=[], packet_interface="eth0")
        guest_args = _packet_guest_args(args, peer_mac="02:00:00:00:00:01")
        self.assertEqual(
            guest_args,
            ["--formal", "--interface", "eth0", "--peer-mac", "02:00:00:00:00:01"],
        )

    def test_tap_vhost_formal_fails_closed_without_vhost_worker_proof(self) -> None:
        for subsystem in ("packet", "seccomp", "io-uring-physical"):
            with self.subTest(subsystem=subsystem):
                args = argparse.Namespace(
                    subsystem=subsystem,
                    network="tap-vhost",
                    tap_name="tap-net0",
                    packet_peer_mac="02:00:00:00:00:01",
                    packet_peer_command=("peer",),
                    backend_cpus="4",
                )
                self.assertEqual(run_command(args), 78)

    def test_packet_peer_ready_done_and_backend_affinity_are_proven(self) -> None:
        backend_cpu = min(os.sched_getaffinity(0))
        run_id = "0123456789abcdef"
        code = (
            "import time\n"
            "print('TKPFNET1_PEER_READY schema=thekernel-tkpfnet1-peer-v1 "
            f"run_id={run_id} interface=tap-net0 mac=02:00:00:00:00:01 status=ok', flush=True)\n"
            "time.sleep(0.05)\n"
            "print('TKPFNET1_PEER_DONE "
            f"run_id={run_id} status=ok sent=12 echoed=12 checksum=deadbeef errors=0', flush=True)\n"
            "time.sleep(1)\n"
        )
        with tempfile.TemporaryDirectory() as directory:
            peer = _start_packet_peer(
                (sys.executable, "-c", code),
                run_dir=Path(directory),
                tap_name="tap-net0",
                peer_mac="02:00:00:00:00:01",
                run_id=run_id,
                backend_cpus=(backend_cpu,),
                ready_timeout=1.0,
            )
            _wait_packet_peer_done(peer, 1.0)
            returncode = _stop_packet_peer(peer)
            affinity = json.loads(peer.affinity_path.read_text(encoding="utf-8"))
        self.assertEqual(peer.ready["run_id"], run_id)
        self.assertEqual(peer.done["sent"], "12")
        self.assertEqual(peer.done["echoed"], "12")
        self.assertEqual(peer.done["errors"], "0")
        self.assertIsNone(peer.done_error)
        self.assertIn(returncode, (0, -15))
        self.assertEqual(affinity["requested_cpus"], [backend_cpu])
        self.assertEqual(affinity["readback_cpus"], [backend_cpu])

    def test_packet_peer_old_done_aliases_are_rejected(self) -> None:
        with self.assertRaisesRegex(BaselineError, "fields are not exact"):
            _validate_peer_done(
                {
                    "schema": "thekernel-tkpfnet1-peer-v1",
                    "run_id": RUN_ID,
                    "frames": "12",
                    "checksum": "deadbeef",
                    "status": "ok",
                },
                run_id=RUN_ID,
            )

    def test_packet_peer_early_exit_is_rejected_before_guest(self) -> None:
        backend_cpu = min(os.sched_getaffinity(0))
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(BaselineError, "before READY"):
                _start_packet_peer(
                    (sys.executable, "-c", "print('not a peer', flush=True)"),
                    run_dir=Path(directory),
                    tap_name="tap-net0",
                    peer_mac="02:00:00:00:00:01",
                    run_id="0123456789abcdef",
                    backend_cpus=(backend_cpu,),
                    ready_timeout=0.5,
                )

    def test_packet_peer_without_done_is_not_formal(self) -> None:
        backend_cpu = min(os.sched_getaffinity(0))
        code = (
            "import time\n"
            "print('TKPFNET1_PEER_READY schema=thekernel-tkpfnet1-peer-v1 "
            "run_id=0123456789abcdef interface=tap-net0 "
            "mac=02:00:00:00:00:01 status=ok', flush=True)\n"
            "time.sleep(1)\n"
        )
        with tempfile.TemporaryDirectory() as directory:
            peer = _start_packet_peer(
                (sys.executable, "-c", code),
                run_dir=Path(directory),
                tap_name="tap-net0",
                peer_mac="02:00:00:00:00:01",
                run_id="0123456789abcdef",
                backend_cpus=(backend_cpu,),
                ready_timeout=0.5,
            )
            _wait_packet_peer_done(peer, 0.1)
            _stop_packet_peer(peer)
        self.assertEqual(peer.done, None)
        self.assertIn("DONE", peer.done_error or "")

    def test_topology_rejects_smt_and_heterogeneous_auto_selection(self) -> None:
        topology = HostTopology(
            (
                HostCpu(0, frozenset({0, 1}), "0", "0", "p"),
                HostCpu(1, frozenset({0, 1}), "0", "0", "p"),
                HostCpu(2, frozenset({2}), "0", "1", "e"),
                HostCpu(3, frozenset({3}), "0", "2", "e"),
            )
        )
        with self.assertRaisesRegex(BaselineError, "SMT siblings"):
            select_host_cpus(2, topology=topology, explicit="0,1")
        with self.assertRaisesRegex(BaselineError, "homogeneous"):
            select_host_cpus(3, topology=topology)
        self.assertEqual(select_host_cpus(2, topology=topology, explicit="2,3"), (2, 3))

    def test_measurement_roles_reject_physical_core_overlap(self) -> None:
        topology = HostTopology(
            (
                HostCpu(0, frozenset({0, 1}), "0", "0", "p"),
                HostCpu(1, frozenset({0, 1}), "0", "0", "p"),
                HostCpu(2, frozenset({2}), "0", "1", "p"),
            )
        )
        with self.assertRaisesRegex(BaselineError, "share a physical core"):
            validate_cpu_roles({"vCPU": (0,), "IO": (1,)}, topology)

    def test_hybrid_auto_selection_prefers_p_class_and_slow_housekeeping(self) -> None:
        records = [
            HostCpu(
                cpu,
                frozenset({cpu}),
                "0",
                str(cpu),
                "1024" if cpu == 0 else "1005",
                "p-core",
                "p-cache",
                4800000 if cpu == 0 else 4700000,
            )
            for cpu in range(4)
        ]
        records.extend(
            HostCpu(cpu, frozenset({cpu}), "0", str(cpu), "701", "e-core", "e-cache", 3700000)
            for cpu in range(4, 12)
        )
        records.extend(
            HostCpu(cpu, frozenset({cpu}), "0", str(cpu), "625", "e-core", "e-cache", 3300000)
            for cpu in range(12, 16)
        )
        topology = HostTopology(tuple(records))
        vcpus = select_host_cpus(4, topology=topology)
        io = select_host_cpus(1, topology=topology, allowed=set(range(4, 16)))
        housekeeping = _housekeeping_selection(
            None,
            allowed=set(range(16)),
            measurement=set(vcpus) | set(io),
            topology=topology,
        )
        self.assertEqual(vcpus, (0, 1, 2, 3))
        self.assertEqual(io, (4,))
        self.assertEqual(housekeeping, (12, 13, 14, 15))

    def test_housekeeping_rejects_measurement_smt_sibling(self) -> None:
        topology = HostTopology(
            (
                HostCpu(0, frozenset({0, 1}), "0", "0", "p"),
                HostCpu(1, frozenset({0, 1}), "0", "0", "p"),
            )
        )
        with self.assertRaisesRegex(BaselineError, "physical-core siblings"):
            _housekeeping_selection(None, allowed={0, 1}, measurement={0}, topology=topology)
        with self.assertRaisesRegex(BaselineError, "physical-core siblings"):
            _housekeeping_selection("1", allowed={0, 1}, measurement={0}, topology=topology)

    def test_stats_quantiles(self) -> None:
        self.assertEqual(stats([9, 1, 4, 2]), {"count": 4, "p50_ns": 2, "p99_ns": 9, "p999_ns": 9})


if __name__ == "__main__":
    unittest.main()
