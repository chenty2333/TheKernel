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
sys.path.insert(0, str(REPO_ROOT))
COMPARATOR = REPO_ROOT / "scripts" / "ci" / "compare-mm-performance.py"
LOCK_PARSER = REPO_ROOT / "scripts" / "ci" / "parse-mm-lock-diagnostics.py"
ASID_PARSER = REPO_ROOT / "scripts" / "ci" / "parse-asid-switch-diagnostics.py"
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
    "measurement_mode",
    "kernel_profile",
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
    "platform_class",
    "pmu_source",
    "cpu_model",
    "firmware_version",
    "cpu_freq_policy",
    "kernel_artifact",
    "metrics_artifact",
    "metrics_sha256",
    "metrics_size_bytes",
    "mm_lock_diagnostics_artifact",
    "mm_lock_diagnostics_sha256",
    "mm_lock_diagnostics_size_bytes",
    "asid_switch_diagnostics_artifact",
    "asid_switch_diagnostics_sha256",
    "asid_switch_diagnostics_size_bytes",
    "commands",
    "commands_sha256",
    "commands_size_bytes",
    "guest_inputs",
    "guest_inputs_sha256",
    "guest_inputs_size_bytes",
    "qemu_receipt",
    "qemu_receipt_sha256",
    "qemu_receipt_size_bytes",
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
    "requested_vmas",
    "fixture_vmas",
    "reason",
    "errno",
)
EXPECTED_METRICS = (
    "vma_scale",
    "mremap_latency",
    "mremap_fixed_replace_latency",
    "mremap_disjoint_same_as_contention",
    "mremap_file_duplicate_latency",
    "mremap_shared_anon_resize_latency",
    "protect_touch_latency",
    "address_space_switch_ping_pong_latency",
    "direct_io_pin_proxy_throughput",
    "direct_io_pin_proxy_same_as_contention",
    "direct_io_pin_proxy_cross_as_contention",
)
PIN_METRICS = frozenset(
    {"direct_io_pin_proxy_throughput", "direct_io_pin_proxy_same_as_contention", "direct_io_pin_proxy_cross_as_contention"}
)
VMA_FIXTURE_METRICS = frozenset(
    {
        "vma_scale",
        "mremap_fixed_replace_latency",
        "mremap_disjoint_same_as_contention",
        "direct_io_pin_proxy_throughput",
        "direct_io_pin_proxy_same_as_contention",
        "direct_io_pin_proxy_cross_as_contention",
    }
)
MM_LOCK_STAGES = (
    "user_pin_admission",
    "user_pin_expectation",
    "user_pin_collect_owners",
    "user_pin_revalidate",
    "user_pin_commit",
    "user_pin_release",
    "mremap_optimistic_plan",
    "mremap_optimistic_commit",
    "mremap_serialized",
    "phys_pin_registry_shard",
    "phys_pin_publish_shard",
    "phys_pin_release_shard",
    "phys_pin_dealloc_probe_shard",
)


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


def metric_log_record(row: dict[str, str]) -> str:
    fields = [
        f"metric={row['metric']}",
        f"status={row['status']}",
        f"count={row['count']}",
        f"p50_ns={row['p50_ns']}",
        f"p99_ns={row['p99_ns']}",
        f"p999_ns={row['p999_ns']}",
    ]
    if row["throughput_bytes_per_sec"] != "-":
        fields.append(
            f"throughput_bytes_per_sec={row['throughput_bytes_per_sec']}"
        )
    if row["requested_vmas"] != "-":
        fields.extend(
            (
                f"requested_vmas={row['requested_vmas']}",
                f"fixture_vmas={row['fixture_vmas']}",
            )
        )
    if row["status"] == "missing":
        fields.extend((f"reason={row['reason']}", f"errno={row['errno']}"))
    return "MM_PERF " + " ".join(fields)


def lock_diagnostic_log_records() -> list[str]:
    buckets = ["0"] * 64
    buckets[1] = "1"
    histogram = ",".join(buckets)
    records = [
        "MM_LOCK_DIAGNOSTICS schema=thekernel-mm-lock-diagnostics-v1 "
        "enabled=0 resetting=0 active_samples=0 epoch=7 sequence=101 "
        "sequence_exhausted=0 histogram=log2_ns_v1"
    ]
    records.extend(
        "MM_LOCK_STAGE "
        f"stage={stage} epoch=7 samples=1 wait_sum_ns=1 wait_max_ns=1 "
        "hold_sum_ns=1 hold_max_ns=1 saturated=0 "
        f"wait_buckets={histogram} hold_buckets={histogram}"
        for stage in MM_LOCK_STAGES
    )
    records.append(
        "MM_LOCK_DIAGNOSTICS_END enabled=0 resetting=0 active_samples=0 "
        "epoch=7 sequence=101 sequence_exhausted=0"
    )
    return records


def render_qemu_log(
    rows: list[dict[str, str]], measurement_mode: str, values: dict[str, str]
) -> str:
    online_cpus = rows[0]["online_cpus"]
    pin_workers = int(values["pin_workers"])
    pin_iterations = values["pin_iterations"]
    live_vmas = values["live_vmas"]
    records = [
        "fixture guest log",
        f"MM_PERF_TOPOLOGY status=ok online_cpus={online_cpus}",
        f"MM_PERF_AFFINITY status=ok bytes=8 allowed_cpus={online_cpus} "
        f"cpu_ids={','.join(str(cpu) for cpu in range(int(online_cpus)))} "
        "cpu_ids_complete=1",
        "MM_PERF_RUN schema=thekernel-mm-performance-run-v3 "
        f"arch={values['arch']} iterations={values['iterations']} "
        f"vmas={live_vmas} pin_iterations={pin_iterations} "
        f"pin_workers={pin_workers} page_size=4096",
        "MM_PERF_SEMANTICS status=ok",
        "MM_PERF_MREMAP_WORKER status=ok worker=0 cpu=0 "
        f"completed={values['iterations']} slot_a=1048576 slot_b=1060864 "
        "bytes=8192 start_ns=100 end_ns=300 p99_ns=100 "
        f"fixture_before_vmas={live_vmas} fixture_after_vmas={live_vmas}",
        "MM_PERF_MREMAP_WORKER status=ok worker=1 cpu=1 "
        f"completed={values['iterations']} slot_a=1073152 slot_b=1085440 "
        "bytes=8192 start_ns=150 end_ns=350 p99_ns=100 "
        f"fixture_before_vmas={live_vmas} fixture_after_vmas={live_vmas}",
        "MM_PERF_PIN_WORKER mode=single status=ok worker=0 cpu=0 "
        f"completed={pin_iterations} p99_ns=100 over_10ms=0 over_50ms=0 "
        f"fixture_before_vmas={live_vmas} fixture_after_vmas={live_vmas}",
        *(
            "MM_PERF_PIN_WORKER mode=contention status=ok "
            f"worker={worker} cpu={worker} completed={pin_iterations} "
            "p99_ns=100 over_10ms=0 over_50ms=0 "
            f"fixture_before_vmas={live_vmas} fixture_after_vmas={live_vmas}"
            for worker in range(pin_workers)
        ),
        *(
            "MM_PERF_PIN_CROSS_AS_WORKER status=ok "
            f"worker={worker} pid={1000 + worker} cpu={worker} "
            f"completed={pin_iterations} p99_ns=100 "
            f"fixture_before_vmas={live_vmas} fixture_after_vmas={live_vmas} "
            "cow_isolated=1"
            for worker in range(pin_workers)
        ),
        *(metric_log_record(row) for row in rows),
        "MM_PERF_DONE status=ok",
    ]
    if measurement_mode == "diagnostic":
        records.extend(lock_diagnostic_log_records())
        records.extend(
            (
                "ASID_SWITCH_DIAGNOSTICS "
                "schema=thekernel-asid-switch-diagnostics-v1 enabled=0 "
                "fast_path_avoided=100 fallback_asid_zero=0 "
                "fallback_invalid_width=0 fallback_exhausted=0 "
                "fallback_generation_mismatch=0 "
                "fallback_same_id_different_root=0 saturated=0",
                "PMU_CAPABILITIES schema=thekernel-pmu-capabilities-v1 "
                "source=sbi-pmu counter_count=2 consistent_snapshot=0 "
                "samples_collected=0",
                "PMU_EVENT event=cpu_cycles requestable=1 sampled=0",
                "PMU_EVENT event=instructions requestable=1 sampled=0",
                "PMU_EVENT event=dtlb_read_misses requestable=1 sampled=0",
                "PMU_EVENT event=dtlb_write_misses requestable=1 sampled=0",
                "PMU_EVENT event=itlb_read_misses requestable=1 sampled=0",
            )
        )
    records.append("System is shutting down")
    return "\n".join(records) + "\n"


def derive_lock_diagnostics(qemu_log: Path) -> bytes:
    completed = subprocess.run(
        [sys.executable, str(LOCK_PARSER), str(qemu_log)],
        cwd=REPO_ROOT,
        check=False,
        capture_output=True,
    )
    if completed.returncode != 0:
        raise AssertionError(completed.stderr.decode("utf-8", errors="replace"))
    return completed.stdout


def derive_asid_diagnostics(qemu_log: Path) -> bytes:
    completed = subprocess.run(
        [sys.executable, str(ASID_PARSER), str(qemu_log)],
        cwd=REPO_ROOT,
        check=False,
        capture_output=True,
    )
    if completed.returncode != 0:
        raise AssertionError(completed.stderr.decode("utf-8", errors="replace"))
    return completed.stdout


def make_bundle(
    root: Path,
    *,
    manifest_overrides: dict[str, str] | None = None,
    kernel_content: bytes = b"fixture-kernel\n",
    measurement_mode: str = "product",
    arch: str = "rv",
    requested_cpus: int = 4,
    capture_offset_secs: int = 0,
    capture_day: int = 1,
) -> None:
    run_name = f"{arch}-{requested_cpus}cpu"
    qemu_binary = {
        "rv": "qemu-system-riscv64",
        "la": "qemu-system-loongarch64",
    }[arch]
    host_cpu_set = "0" if requested_cpus == 1 else f"0-{requested_cpus - 1}"
    root.mkdir(parents=True)
    run = root / run_name
    run.mkdir()
    values = {
        "bundle_schema": "thekernel-mm-performance-bundle-v10",
        "thekernel_commit": "1" * 40,
        "thekernel_ax_commit": "2" * 40,
        "thekernel_linux_abi_commit": "3" * 40,
        "measurement_mode": measurement_mode,
        "kernel_profile": (
            "mm-performance" if measurement_mode == "diagnostic" else "shell"
        ),
        "arch": arch,
        "requested_cpus": str(requested_cpus),
        "online_cpus": str(requested_cpus),
        "iterations": "100",
        "live_vmas": "512",
        "pin_iterations": "25",
        "pin_workers": str(requested_cpus),
        "rootfs_sha256": "4" * 64,
        "qemu_binary": qemu_binary,
        "qemu_version": "QEMU emulator version fixture",
        "qemu_sha256": "5" * 64,
        "runner_fingerprint": f"auto-sha256:{'6' * 64}",
        "runner_contract_sha256": "7" * 64,
        "host_cpu_set": host_cpu_set,
        "host_cpu_selection": "auto-homogeneous-v1",
        "host_cpu_class": "package:0,max_freq_khz:3700000",
        "platform_class": "qemu-tcg",
        "pmu_source": "none",
        "cpu_model": "not-applicable",
        "firmware_version": "not-applicable",
        "cpu_freq_policy": "not-applicable",
        "kernel_artifact": f"{run_name}/kernel",
        "metrics_artifact": f"{run_name}/mm-performance.tsv",
        "commands": f"{run_name}/commands",
        "guest_inputs": f"{run_name}/guest-inputs.tsv",
        "qemu_receipt": f"{run_name}/qemu-runner-receipt.json",
        "qemu_log": f"{run_name}/qemu.log",
        "host_diagnostics_pre": f"{run_name}/host-pre.tsv",
        "host_diagnostics_post": f"{run_name}/host-post.tsv",
    }
    if manifest_overrides:
        values.update(manifest_overrides)
    timestamp_fraction = int(
        hashlib.sha256(str(root).encode("utf-8")).hexdigest()[:12], 16
    ) % 1_000_000
    timestamps = {
        "pre": (
            f"2026-01-{capture_day:02d}T00:00:{capture_offset_secs:02d}."
            f"{timestamp_fraction:06d}+00:00"
        ),
        "post": (
            f"2026-01-{capture_day:02d}T00:00:{capture_offset_secs + 1:02d}."
            f"{timestamp_fraction:06d}+00:00"
        ),
    }
    iterations = int(values["iterations"])
    pin_iterations = int(values["pin_iterations"])
    pin_workers = int(values["pin_workers"])

    kernel = run / "kernel"
    kernel.write_bytes(kernel_content)
    values["kernel_sha256"] = sha256(kernel)
    values["kernel_size_bytes"] = str(kernel.stat().st_size)

    commands = run / "commands"
    workload_command = (
        "/opt/thekernel-tests/bin/thekernel-mm-performance "
        f"--iterations {values['iterations']} --vmas {values['live_vmas']} "
        f"--pin-iterations {values['pin_iterations']} "
        f"--pin-workers {values['pin_workers']}"
    )
    if values["measurement_mode"] == "diagnostic":
        command_text = "\n".join(
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
    else:
        command_text = workload_command + " || exit 1\nexit\n"
    commands.write_text(command_text, encoding="utf-8")
    values["commands_sha256"] = sha256(commands)
    values["commands_size_bytes"] = str(commands.stat().st_size)

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
                {"key": "online_cpu_set", "value": values["host_cpu_set"]},
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
                "requested_vmas": (
                    values["live_vmas"] if metric in VMA_FIXTURE_METRICS else "-"
                ),
                "fixture_vmas": (
                    values["live_vmas"] if metric in VMA_FIXTURE_METRICS else "-"
                ),
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

    qemu_log = run / "qemu.log"
    qemu_log.write_text(
        render_qemu_log(metric_rows, values["measurement_mode"], values),
        encoding="utf-8",
    )
    values["qemu_log_sha256"] = sha256(qemu_log)
    values["qemu_log_size_bytes"] = str(qemu_log.stat().st_size)

    command_line_count = len(commands.read_text(encoding="utf-8").splitlines())
    guest_inputs = run / "guest-inputs.tsv"
    guest_inputs.write_text(
        "".join(
            f"{key}\t{value}\n"
            for key, value in (
                ("schema_version", "1"),
                ("arch", values["arch"]),
                ("requested_cpus", values["requested_cpus"]),
                ("kernel_profile", values["kernel_profile"]),
                ("kernel_path", str(kernel.resolve())),
                ("kernel_size_bytes", values["kernel_size_bytes"]),
                ("kernel_sha256", values["kernel_sha256"]),
                ("commands_path", str(commands.resolve())),
                ("commands_size_bytes", values["commands_size_bytes"]),
                ("commands_line_count", str(command_line_count)),
                ("commands_sha256", values["commands_sha256"]),
                ("rootfs_source", "/fixture/rootfs.img"),
                ("rootfs_size_bytes", "1"),
                ("rootfs_sha256", values["rootfs_sha256"]),
                ("qemu_binary", f"/usr/bin/{values['qemu_binary']}"),
                ("qemu_sha256", values["qemu_sha256"]),
                ("qemu_version", values["qemu_version"]),
            )
        ),
        encoding="utf-8",
    )
    values["guest_inputs_sha256"] = sha256(guest_inputs)
    values["guest_inputs_size_bytes"] = str(guest_inputs.stat().st_size)

    from tools.qemu_runner.command import build_qemu_command
    from tools.qemu_runner.model import Drive

    fixture_rootfs = Path("/fixture/rootfs.img")
    fixture_qemu = Path(f"/usr/bin/{values['qemu_binary']}")
    recorded_command = list(
        build_qemu_command(
            arch=values["arch"],
            kernel=kernel.resolve(),
            rootfs=Drive(fixture_rootfs, "snapshot"),
            extra_block=None,
            memory="1G",
            cpus=int(values["requested_cpus"]),
            qemu_binary=str(fixture_qemu),
        )
    )
    qemu_receipt = run / "qemu-runner-receipt.json"
    qemu_receipt.write_text(
        json.dumps(
            {
                "schema_version": 2,
                "state": "complete",
                "arch": values["arch"],
                "cpus": int(values["requested_cpus"]),
                "memory": "1G",
                "rootfs_mode": "snapshot",
                "extra_block_mode": "rw",
                "command": recorded_command,
                "returncode": 0,
                "error_message": None,
                "timed_out": False,
                "interrupted": False,
                "intentionally_stopped": False,
                "interaction": {
                    "interactive": True,
                    "input_after_marker": "THEKERNEL_SHELL_READY",
                    "stop_after_marker": None,
                    "external_input_producer": True,
                },
                "log_path": str(qemu_log.resolve()),
                "kernel": {
                    "path": str(kernel.resolve()),
                    "sha256": values["kernel_sha256"],
                    "size_bytes": int(values["kernel_size_bytes"]),
                },
                "rootfs_source": {
                    "path": str(fixture_rootfs),
                    "sha256": values["rootfs_sha256"],
                    "size_bytes": 1,
                },
                "rootfs_runtime_before": {
                    "path": str(fixture_rootfs),
                    "sha256": values["rootfs_sha256"],
                    "size_bytes": 1,
                },
                "rootfs_runtime_after": {
                    "path": str(fixture_rootfs),
                    "sha256": values["rootfs_sha256"],
                    "size_bytes": 1,
                },
                "qemu": {
                    "requested": values["qemu_binary"],
                    "path": str(fixture_qemu),
                    "sha256": values["qemu_sha256"],
                    "size_bytes": 1,
                },
                "log": {
                    "path": str(qemu_log.resolve()),
                    "sha256": values["qemu_log_sha256"],
                    "size_bytes": int(values["qemu_log_size_bytes"]),
                },
                "stdin": {
                    "state": "complete",
                    "sha256": values["commands_sha256"],
                    "bytes": int(values["commands_size_bytes"]),
                    "line_count": command_line_count,
                    "observed_bytes": int(values["commands_size_bytes"]),
                    "source_eof": True,
                    "broken_pipe": False,
                    "relay_complete": True,
                    "source_sha256": values["commands_sha256"],
                    "source_bytes": int(values["commands_size_bytes"]),
                    "source_line_count": command_line_count,
                    "source_unchanged": True,
                    "producer_status": 0,
                    "producer_status_kind": "exit:0",
                    "source_fully_relayed": True,
                    "producer_status_accepted": True,
                },
            },
            indent=2,
            sort_keys=True,
        )
        + "\n",
        encoding="utf-8",
    )
    values["qemu_receipt_sha256"] = sha256(qemu_receipt)
    values["qemu_receipt_size_bytes"] = str(qemu_receipt.stat().st_size)
    if values["measurement_mode"] == "diagnostic":
        lock_diagnostics = run / "mm-lock-diagnostics.tsv"
        lock_diagnostics.write_bytes(derive_lock_diagnostics(qemu_log))
        values["mm_lock_diagnostics_artifact"] = (
            f"{run_name}/mm-lock-diagnostics.tsv"
        )
        values["mm_lock_diagnostics_sha256"] = sha256(lock_diagnostics)
        values["mm_lock_diagnostics_size_bytes"] = str(
            lock_diagnostics.stat().st_size
        )
        asid_diagnostics = run / "asid-switch-diagnostics.tsv"
        asid_diagnostics.write_bytes(derive_asid_diagnostics(qemu_log))
        values["asid_switch_diagnostics_artifact"] = (
            f"{run_name}/asid-switch-diagnostics.tsv"
        )
        values["asid_switch_diagnostics_sha256"] = sha256(asid_diagnostics)
        values["asid_switch_diagnostics_size_bytes"] = str(
            asid_diagnostics.stat().st_size
        )
    else:
        values["mm_lock_diagnostics_artifact"] = "not-collected"
        values["mm_lock_diagnostics_sha256"] = "not-collected"
        values["mm_lock_diagnostics_size_bytes"] = "not-collected"
        values["asid_switch_diagnostics_artifact"] = "not-collected"
        values["asid_switch_diagnostics_sha256"] = "not-collected"
        values["asid_switch_diagnostics_size_bytes"] = "not-collected"
    write_tsv(root / "mm-performance-manifest.tsv", MANIFEST_COLUMNS, [values])


def make_release_bundle(
    root: Path,
    *,
    manifest_overrides: dict[str, str] | None = None,
    kernel_content: bytes = b"fixture-kernel\n",
    capture_day: int = 1,
) -> None:
    root.mkdir(parents=True)
    manifest_rows: list[dict[str, str]] = []
    metric_rows: list[dict[str, str]] = []
    artifact_fields = (
        "kernel_artifact",
        "metrics_artifact",
        "commands",
        "guest_inputs",
        "qemu_receipt",
        "qemu_log",
        "host_diagnostics_pre",
        "host_diagnostics_post",
    )
    for index, (arch, cpus) in enumerate(
        (("rv", 4), ("rv", 8), ("la", 4), ("la", 8))
    ):
        source = root / f"source-{arch}-{cpus}"
        make_bundle(
            source,
            manifest_overrides=manifest_overrides,
            kernel_content=kernel_content,
            arch=arch,
            requested_cpus=cpus,
            capture_offset_secs=index * 2,
            capture_day=capture_day,
        )
        _, rows = read_tsv(source / "mm-performance-manifest.tsv")
        row = rows[0]
        for field in artifact_fields:
            row[field] = f"{source.name}/{row[field]}"
        manifest_rows.append(row)
        _, source_metrics = read_tsv(source / "mm-performance.tsv")
        metric_rows.extend(source_metrics)
    write_tsv(
        root / "mm-performance-manifest.tsv",
        MANIFEST_COLUMNS,
        manifest_rows,
    )
    write_tsv(root / "mm-performance.tsv", METRIC_COLUMNS, metric_rows)


def mutate_manifest(root: Path, mutator: Callable[[dict[str, str]], None]) -> None:
    path = root / "mm-performance-manifest.tsv"
    columns, rows = read_tsv(path)
    mutator(rows[0])
    write_tsv(path, columns, rows)


def refresh_qemu_receipt_log(root: Path) -> dict[str, str]:
    qemu_log = root / "rv-4cpu" / "qemu.log"
    qemu_receipt = root / "rv-4cpu" / "qemu-runner-receipt.json"
    receipt = json.loads(qemu_receipt.read_text(encoding="utf-8"))
    receipt["log"]["sha256"] = sha256(qemu_log)
    receipt["log"]["size_bytes"] = qemu_log.stat().st_size
    qemu_receipt.write_text(
        json.dumps(receipt, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    return {
        "qemu_receipt_sha256": sha256(qemu_receipt),
        "qemu_receipt_size_bytes": str(qemu_receipt.stat().st_size),
    }


def receipt_artifact_fields(root: Path, name: str) -> dict[str, str]:
    artifact = root / "rv-4cpu" / name
    prefix = "guest_inputs" if name == "guest-inputs.tsv" else "qemu_receipt"
    return {
        f"{prefix}_sha256": sha256(artifact),
        f"{prefix}_size_bytes": str(artifact.stat().st_size),
    }


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
    update_log: bool = True,
) -> None:
    metrics = root / "rv-4cpu" / "mm-performance.tsv"
    columns, rows = read_tsv(metrics)
    mutator(rows)
    write_tsv(metrics, columns, rows)
    if update_matrix:
        write_tsv(root / "mm-performance.tsv", columns, rows)
    updates = {
        "metrics_sha256": sha256(metrics),
        "metrics_size_bytes": str(metrics.stat().st_size),
    }
    if update_log:
        _, manifest_rows = read_tsv(root / "mm-performance-manifest.tsv")
        qemu_log = root / "rv-4cpu" / "qemu.log"
        qemu_log.write_text(
            render_qemu_log(
                rows,
                manifest_rows[0]["measurement_mode"],
                manifest_rows[0],
            ),
            encoding="utf-8",
        )
        updates.update(
            {
                "qemu_log_sha256": sha256(qemu_log),
                "qemu_log_size_bytes": str(qemu_log.stat().st_size),
            }
        )
        updates.update(refresh_qemu_receipt_log(root))
    mutate_manifest(
        root,
        lambda row: row.update(updates),
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
        output: Path | None = None,
        allow_partial: bool = True,
    ) -> tuple[subprocess.CompletedProcess[str], Path]:
        self.series_counter += 1
        if normalize_timestamps:
            base = dt.datetime(2026, 1, 1, tzinfo=dt.UTC) + dt.timedelta(
                days=self.series_counter
            )
            for index, (baseline, candidate) in enumerate(
                zip(baselines, candidates)
            ):
                pair_start = base + dt.timedelta(seconds=index * 4)
                first, second = (
                    (baseline, candidate)
                    if index % 2 == 0
                    else (candidate, baseline)
                )
                set_capture_interval(
                    first, pair_start, pair_start + dt.timedelta(seconds=1)
                )
                set_capture_interval(
                    second,
                    pair_start + dt.timedelta(seconds=2),
                    pair_start + dt.timedelta(seconds=3),
                )
        report = output or self.root / "report.tsv"
        if output is None:
            report.unlink(missing_ok=True)
        arguments = [sys.executable, str(COMPARATOR)]
        for baseline in baselines:
            arguments.extend(("--baseline", str(baseline)))
        for candidate in candidates:
            arguments.extend(("--candidate", str(candidate)))
        if allow_partial:
            arguments.append("--allow-partial")
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
        self.assertIn("PARTIAL PASS", result.stdout)
        self.assertIn("release_gate=false", result.stdout)
        rows = self.report_rows(report)
        self.assertEqual(len(rows), 25)
        self.assertEqual({row["evidence_scope"] for row in rows}, {"partial_triage"})
        self.assertEqual({row["release_gate"] for row in rows}, {"false"})
        self.assertEqual(sum(row["result"] == "PASS" for row in rows), 14)
        self.assertEqual(sum(row["result"] == "REPORT_ONLY" for row in rows), 11)

    def test_default_rejects_incomplete_release_matrix(self) -> None:
        baseline = self.bundle("baseline")
        candidate = self.bundle("candidate")

        result, _ = self.compare_series(
            [baseline], [candidate], allow_partial=False
        )

        self.assertEqual(result.returncode, 2)
        self.assertIn("release bundle run-key set mismatch", result.stderr)
        self.assertIn("--allow-partial only for triage", result.stderr)

    def test_default_accepts_complete_release_matrix_and_marks_report(self) -> None:
        baselines: list[Path] = []
        candidates: list[Path] = []
        for index in range(3):
            baseline = self.root / f"release-baseline-{index}"
            candidate = self.root / f"release-candidate-{index}"
            baseline_day = index * 2 + (1 if index % 2 == 0 else 2)
            candidate_day = index * 2 + (2 if index % 2 == 0 else 1)
            make_release_bundle(baseline, capture_day=baseline_day)
            make_release_bundle(
                candidate,
                manifest_overrides={"thekernel_commit": "8" * 40},
                kernel_content=b"candidate-kernel\n",
                capture_day=candidate_day,
            )
            baselines.append(baseline)
            candidates.append(candidate)

        result, report = self.compare_series(
            baselines,
            candidates,
            allow_partial=False,
            normalize_timestamps=False,
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("PASS", result.stdout)
        self.assertNotIn("PARTIAL", result.stdout)
        self.assertIn("release_gate=true", result.stdout)
        rows = self.report_rows(report)
        self.assertEqual(len(rows), 100)
        self.assertEqual({row["evidence_scope"] for row in rows}, {"release"})
        self.assertEqual({row["release_gate"] for row in rows}, {"true"})

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

    def test_direct_io_pin_proxy_throughput_relative_boundary_is_gated(self) -> None:
        baseline = self.bundle("baseline")
        candidate = self.bundle("candidate")
        set_metric(candidate, "direct_io_pin_proxy_throughput", throughput_bytes_per_sec=900)

        result, _ = self.compare(baseline, candidate)
        self.assertEqual(result.returncode, 0, result.stderr)

        set_metric(candidate, "direct_io_pin_proxy_throughput", throughput_bytes_per_sec=899)
        result, report = self.compare(baseline, candidate)
        self.assertEqual(result.returncode, 1, result.stderr)
        row = next(
            row
            for row in self.report_rows(report)
            if row["metric"] == "direct_io_pin_proxy_throughput"
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

    def test_invalid_topology_metric_count_and_fixture_are_rejected(self) -> None:
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

        fixture = self.bundle("fixture")
        set_metric(fixture, "mremap_fixed_replace_latency", fixture_vmas=511)
        result, _ = self.compare(baseline, fixture)
        self.assertEqual(result.returncode, 2)
        self.assertIn("fixture_vmas mismatch", result.stderr)

        requested = self.bundle("requested")
        set_metric(
            requested,
            "mremap_fixed_replace_latency",
            requested_vmas=511,
            fixture_vmas=511,
        )
        result, _ = self.compare(baseline, requested)
        self.assertEqual(result.returncode, 2)
        self.assertIn("requested_vmas mismatch", result.stderr)

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
        self.assertIn("missing required metric records", result.stderr)

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

    def test_diagnostic_bundles_are_not_product_regression_evidence(self) -> None:
        baseline = self.bundle("baseline-diagnostic", measurement_mode="diagnostic")
        candidate = self.bundle("candidate-diagnostic", measurement_mode="diagnostic")

        result, report = self.compare(baseline, candidate)

        self.assertEqual(result.returncode, 2)
        self.assertIn("not product regression evidence", result.stderr)
        self.assertFalse(report.exists())

    def test_product_sentinel_and_diagnostic_artifact_are_fail_closed(self) -> None:
        baseline = self.bundle("baseline")
        product = self.bundle("product-bad-sentinel")
        mutate_manifest(
            product,
            lambda row: row.update(
                {"mm_lock_diagnostics_size_bytes": "0"}
            ),
        )
        result, report = self.compare(baseline, product)
        self.assertEqual(result.returncode, 2)
        self.assertIn("product evidence must use", result.stderr)
        self.assertFalse(report.exists())

        diagnostic_baseline = self.bundle(
            "diagnostic-baseline", measurement_mode="diagnostic"
        )
        diagnostic = self.bundle("diagnostic-corrupt", measurement_mode="diagnostic")
        lock_artifact = diagnostic / "rv-4cpu" / "mm-lock-diagnostics.tsv"
        lock_artifact.write_text("corrupt\n", encoding="utf-8")
        result, report = self.compare(diagnostic_baseline, diagnostic)
        self.assertEqual(result.returncode, 2)
        self.assertRegex(result.stderr, r"mm_lock_diagnostics_artifact (size|SHA-256) mismatch")
        self.assertFalse(report.exists())

    def test_metrics_artifact_must_be_derived_from_raw_qemu_log(self) -> None:
        baseline = self.bundle("baseline")
        candidate = self.bundle("candidate-spliced-metrics")

        def change_metric(rows: list[dict[str, str]]) -> None:
            next(row for row in rows if row["metric"] == "vma_scale")[
                "p99_ns"
            ] = "119"

        mutate_metrics(candidate, change_metric, update_log=False)
        result, report = self.compare(baseline, candidate)

        self.assertEqual(result.returncode, 2)
        self.assertIn("metrics artifact does not match the raw QEMU log", result.stderr)
        self.assertFalse(report.exists())

    def test_product_raw_log_rejects_diagnostics(self) -> None:
        baseline = self.bundle("baseline")
        candidate = self.bundle("candidate-product-lock-log")
        qemu_log = candidate / "rv-4cpu" / "qemu.log"
        with qemu_log.open("a", encoding="utf-8") as output:
            output.write("MM_LOCK_FORGED status=present\n")
        receipt_updates = refresh_qemu_receipt_log(candidate)
        mutate_manifest(
            candidate,
            lambda row: row.update(
                {
                    "qemu_log_sha256": sha256(qemu_log),
                    "qemu_log_size_bytes": str(qemu_log.stat().st_size),
                    **receipt_updates,
                }
            ),
        )

        result, report = self.compare(baseline, candidate)

        self.assertEqual(result.returncode, 2)
        self.assertIn("product raw QEMU log contains diagnostics", result.stderr)
        self.assertFalse(report.exists())

    def test_diagnostic_artifact_must_be_derived_from_raw_qemu_log(self) -> None:
        baseline = self.bundle("diagnostic-baseline", measurement_mode="diagnostic")
        candidate = self.bundle("diagnostic-spliced", measurement_mode="diagnostic")
        diagnostics = candidate / "rv-4cpu" / "mm-lock-diagnostics.tsv"
        with diagnostics.open("a", encoding="utf-8") as output:
            output.write("forged\trow\n")
        mutate_manifest(
            candidate,
            lambda row: row.update(
                {
                    "mm_lock_diagnostics_sha256": sha256(diagnostics),
                    "mm_lock_diagnostics_size_bytes": str(
                        diagnostics.stat().st_size
                    ),
                }
            ),
        )

        result, report = self.compare(baseline, candidate)

        self.assertEqual(result.returncode, 2)
        self.assertIn(
            "MM lock diagnostics artifact does not match the raw QEMU log",
            result.stderr,
        )
        self.assertFalse(report.exists())

    def test_diagnostic_bundle_rejects_pmu_sample_claims(self) -> None:
        baseline = self.bundle("diagnostic-pmu-baseline", measurement_mode="diagnostic")
        candidate = self.bundle("diagnostic-pmu-sampled", measurement_mode="diagnostic")
        qemu_log = candidate / "rv-4cpu" / "qemu.log"
        qemu_log.write_text(
            qemu_log.read_text(encoding="utf-8").replace(
                "samples_collected=0", "samples_collected=1", 1
            ),
            encoding="utf-8",
        )
        receipt_updates = refresh_qemu_receipt_log(candidate)
        mutate_manifest(
            candidate,
            lambda row: row.update(
                {
                    "qemu_log_sha256": sha256(qemu_log),
                    "qemu_log_size_bytes": str(qemu_log.stat().st_size),
                    **receipt_updates,
                }
            ),
        )

        result, report = self.compare(baseline, candidate)

        self.assertEqual(result.returncode, 2)
        self.assertIn("samples_collected=0", result.stderr)
        self.assertFalse(report.exists())

    def test_guest_input_receipt_cannot_be_spliced_from_another_run(self) -> None:
        baseline = self.bundle("baseline")
        candidate = self.bundle("candidate")
        donor = self.bundle(
            "donor-guest-inputs", manifest_overrides={"iterations": "101"}
        )
        shutil.copy2(
            donor / "rv-4cpu" / "guest-inputs.tsv",
            candidate / "rv-4cpu" / "guest-inputs.tsv",
        )
        updates = receipt_artifact_fields(candidate, "guest-inputs.tsv")
        mutate_manifest(candidate, lambda row: row.update(updates))

        result, report = self.compare(baseline, candidate)

        self.assertEqual(result.returncode, 2)
        self.assertIn("guest input receipt", result.stderr)
        self.assertFalse(report.exists())

    def test_qemu_receipt_cannot_be_spliced_from_another_run(self) -> None:
        baseline = self.bundle("baseline")
        candidate = self.bundle("candidate")
        donor = self.bundle(
            "donor-qemu-receipt", manifest_overrides={"iterations": "101"}
        )
        shutil.copy2(
            donor / "rv-4cpu" / "qemu-runner-receipt.json",
            candidate / "rv-4cpu" / "qemu-runner-receipt.json",
        )
        updates = receipt_artifact_fields(candidate, "qemu-runner-receipt.json")
        mutate_manifest(candidate, lambda row: row.update(updates))

        result, report = self.compare(baseline, candidate)

        self.assertEqual(result.returncode, 2)
        self.assertIn("QEMU stdin receipt is invalid", result.stderr)
        self.assertFalse(report.exists())

    def test_qemu_receipt_requires_complete_file_evidence_records(self) -> None:
        baseline = self.bundle("baseline")
        for key in ("qemu", "rootfs_source"):
            with self.subTest(key=key):
                candidate = self.bundle(f"candidate-missing-{key}-size")
                receipt_path = (
                    candidate / "rv-4cpu" / "qemu-runner-receipt.json"
                )
                receipt = json.loads(receipt_path.read_text(encoding="utf-8"))
                del receipt[key]["size_bytes"]
                receipt_path.write_text(
                    json.dumps(receipt, indent=2, sort_keys=True) + "\n",
                    encoding="utf-8",
                )
                updates = receipt_artifact_fields(
                    candidate, "qemu-runner-receipt.json"
                )
                mutate_manifest(candidate, lambda row: row.update(updates))

                result, report = self.compare(baseline, candidate)

                self.assertEqual(result.returncode, 2)
                self.assertIn(f"invalid {key} size", result.stderr)
                self.assertFalse(report.exists())

    def test_truncated_forwarding_receipt_is_rejected(self) -> None:
        baseline = self.bundle("baseline")
        candidate = self.bundle("candidate-truncated-stdin")
        commands = candidate / "rv-4cpu" / "commands"
        prefix = commands.read_bytes().splitlines(keepends=True)[0]
        receipt_path = candidate / "rv-4cpu" / "qemu-runner-receipt.json"
        receipt = json.loads(receipt_path.read_text(encoding="utf-8"))
        receipt["stdin"].update(
            {
                "sha256": hashlib.sha256(prefix).hexdigest(),
                "bytes": len(prefix),
                "line_count": 1,
                "observed_bytes": len(prefix),
                "source_eof": False,
                "relay_complete": False,
                "source_fully_relayed": False,
                "producer_status_accepted": False,
            }
        )
        receipt_path.write_text(
            json.dumps(receipt, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        updates = receipt_artifact_fields(candidate, "qemu-runner-receipt.json")
        mutate_manifest(candidate, lambda row: row.update(updates))

        result, report = self.compare(baseline, candidate)

        self.assertEqual(result.returncode, 2)
        self.assertIn("QEMU stdin receipt is invalid", result.stderr)
        self.assertFalse(report.exists())

    def test_exact_stream_with_producer_141_is_not_exact_evidence(self) -> None:
        baseline = self.bundle("baseline")
        candidate = self.bundle("candidate-producer-141")
        receipt_path = candidate / "rv-4cpu" / "qemu-runner-receipt.json"
        receipt = json.loads(receipt_path.read_text(encoding="utf-8"))
        receipt["stdin"].update(
            {
                "producer_status": 141,
                "producer_status_kind": "signal:13",
                "producer_status_accepted": False,
            }
        )
        receipt_path.write_text(
            json.dumps(receipt, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        updates = receipt_artifact_fields(candidate, "qemu-runner-receipt.json")
        mutate_manifest(candidate, lambda row: row.update(updates))

        result, report = self.compare(baseline, candidate)

        self.assertEqual(result.returncode, 2)
        self.assertIn("producer_status_accepted is not true", result.stderr)
        self.assertFalse(report.exists())

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

    def test_hashed_capture_intervals_prove_counterbalanced_pair_order(self) -> None:
        baselines = [self.bundle(f"baseline-{index}") for index in range(3)]
        candidates = [self.bundle(f"candidate-{index}") for index in range(3)]
        base = dt.datetime(2026, 3, 1, tzinfo=dt.UTC)
        for index, (baseline, candidate) in enumerate(
            zip(baselines, candidates, strict=True)
        ):
            pair_start = base + dt.timedelta(seconds=index * 4)
            first, second = (
                (baseline, candidate)
                if index % 2 == 0
                else (candidate, baseline)
            )
            set_capture_interval(
                first, pair_start, pair_start + dt.timedelta(seconds=1)
            )
            set_capture_interval(
                second,
                pair_start + dt.timedelta(seconds=2),
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
        self.assertIn("adjacent-pair order", result.stderr)
        self.assertFalse(report.exists())

        set_capture_interval(
            baselines[1],
            base + dt.timedelta(seconds=4),
            base + dt.timedelta(seconds=5),
        )
        set_capture_interval(
            candidates[1],
            base + dt.timedelta(seconds=6),
            base + dt.timedelta(seconds=7),
        )
        result, report = self.compare_series(
            baselines, candidates, normalize_timestamps=False
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn("counterbalanced order", result.stderr)
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
        payload["metrics"]["direct_io_pin_proxy_throughput"][
            "throughput_min_retained_percent"
        ] = 89
        self.policy.write_text(json.dumps(payload), encoding="utf-8")
        result, _ = self.compare(baseline, candidate)
        self.assertEqual(result.returncode, 2)
        self.assertIn("90 percent throughput retention", result.stderr)

    def test_stability_policy_cannot_weaken_pair_ratio_spread_limit(self) -> None:
        baseline = self.bundle("baseline")
        candidate = self.bundle("candidate")
        payload = json.loads(self.stability_policy.read_text(encoding="utf-8"))
        payload["maximum_pair_ratio_spread_percent"] = 21
        self.stability_policy.write_text(json.dumps(payload), encoding="utf-8")

        result, report = self.compare(baseline, candidate)

        self.assertEqual(result.returncode, 2)
        self.assertIn("20 percent pair-ratio spread ceiling", result.stderr)
        self.assertFalse(report.exists())

    def test_validation_failure_preserves_preexisting_output(self) -> None:
        baselines = [self.bundle(f"baseline-{index}") for index in range(3)]
        candidates = [self.bundle(f"candidate-{index}") for index in range(3)]
        mutate_manifest(
            candidates[0],
            lambda row: row.update({"bundle_schema": "invalid-bundle"}),
        )
        report = self.root / "preexisting-report.tsv"
        sentinel = b"operator-owned evidence\n"
        report.write_bytes(sentinel)

        result, _ = self.compare_series(baselines, candidates, output=report)

        self.assertEqual(result.returncode, 2)
        self.assertIn("unsupported bundle_schema", result.stderr)
        self.assertEqual(report.read_bytes(), sentinel)

    def test_physical_platform_class_is_explicitly_unimplemented(self) -> None:
        baselines = [self.bundle(f"baseline-{index}") for index in range(3)]
        candidates = [self.bundle(f"candidate-{index}") for index in range(3)]
        mutate_manifest(
            candidates[0],
            lambda row: row.update(
                {
                    "platform_class": "physical",
                    "pmu_source": "sbi-pmu",
                    "cpu_model": "sifive-u74",
                    "firmware_version": "opensbi-1.4",
                    "cpu_freq_policy": "fixed-frequency",
                }
            ),
        )

        result, _ = self.compare_series(baselines, candidates)

        self.assertEqual(result.returncode, 2)
        self.assertIn("physical evidence authority is not implemented", result.stderr)

    def test_unknown_platform_class_is_rejected(self) -> None:
        baselines = [self.bundle(f"baseline-{index}") for index in range(3)]
        candidates = [self.bundle(f"candidate-{index}") for index in range(3)]
        mutate_manifest(
            candidates[0],
            lambda row: row.update({"platform_class": "qemu-kvm"}),
        )

        result, _ = self.compare_series(baselines, candidates)

        self.assertEqual(result.returncode, 2)
        self.assertIn("invalid platform_class", result.stderr)

    def test_tcg_pmu_source_is_rejected(self) -> None:
        for pmu_source, expected_error in (
            ("loongarch-pmcfg", "pmu_source does not match arch"),
            ("sbi-pmu", "qemu-tcg evidence must use pmu_source='none'"),
        ):
            with self.subTest(pmu_source=pmu_source):
                baselines = [
                    self.bundle(f"baseline-{pmu_source}-{index}")
                    for index in range(3)
                ]
                candidates = [
                    self.bundle(f"candidate-{pmu_source}-{index}")
                    for index in range(3)
                ]
                mutate_manifest(
                    candidates[0],
                    lambda row: row.update({"pmu_source": pmu_source}),
                )

                result, _ = self.compare_series(baselines, candidates)

                self.assertEqual(result.returncode, 2)
                self.assertIn(expected_error, result.stderr)

    def test_tcg_rows_must_not_claim_platform_identity(self) -> None:
        # A TCG receipt carrying a CPU model would read as physical evidence.
        baselines = [self.bundle(f"baseline-{index}") for index in range(3)]
        candidates = [self.bundle(f"candidate-{index}") for index in range(3)]
        mutate_manifest(
            candidates[0],
            lambda row: row.update({"cpu_model": "sifive-u74"}),
        )

        result, _ = self.compare_series(baselines, candidates)

        self.assertEqual(result.returncode, 2)
        self.assertIn("qemu-tcg evidence must use cpu_model", result.stderr)

    def test_output_inside_input_bundle_is_rejected_without_mutation(self) -> None:
        baselines = [self.bundle(f"baseline-{index}") for index in range(3)]
        candidates = [self.bundle(f"candidate-{index}") for index in range(3)]
        report = baselines[0] / "operator-receipt.tsv"
        sentinel = b"preserve this bundle receipt\n"
        report.write_bytes(sentinel)

        result, _ = self.compare_series(baselines, candidates, output=report)

        self.assertEqual(result.returncode, 2)
        self.assertIn("outside every input evidence bundle", result.stderr)
        self.assertEqual(report.read_bytes(), sentinel)

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

    def test_v1_through_v6_manifests_are_not_silently_upgraded(self) -> None:
        baseline = self.bundle("baseline")
        for version in ("v1", "v2", "v3", "v4", "v5", "v6", "v8"):
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
