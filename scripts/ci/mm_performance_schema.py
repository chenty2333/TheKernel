"""Shared, versioned contracts for TheKernel MM performance evidence."""

from __future__ import annotations


BUNDLE_SCHEMA = "thekernel-mm-performance-bundle-v2"
POLICY_SCHEMA = "thekernel-mm-performance-regression-policy-v1"
EXPECTED_METRICS = (
    "vma_scale",
    "mremap_latency",
    "protect_touch_latency",
    "pin_throughput",
    "pin_contention",
)
PIN_METRICS = frozenset({"pin_throughput", "pin_contention"})
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
REPORT_COLUMNS = (
    "arch",
    "requested_cpus",
    "metric",
    "statistic",
    "mode",
    "baseline",
    "candidate",
    "threshold_percent",
    "comparator",
    "result",
    "candidate_ratio_ppm",
)
