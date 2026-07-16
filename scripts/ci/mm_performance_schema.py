"""Shared, versioned contracts for TheKernel MM performance evidence."""

from __future__ import annotations


BUNDLE_SCHEMA = "thekernel-mm-performance-bundle-v3"
POLICY_SCHEMA = "thekernel-mm-performance-regression-policy-v2"
STABILITY_POLICY_SCHEMA = "thekernel-mm-performance-stability-policy-v1"
HOST_DIAGNOSTIC_SCHEMA = "thekernel-mm-performance-host-diagnostics-v1"
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
REPORT_COLUMNS = (
    "arch",
    "requested_cpus",
    "metric",
    "statistic",
    "mode",
    "pair_count",
    "median_pair",
    "baseline",
    "candidate",
    "threshold_percent",
    "comparator",
    "result",
    "candidate_ratio_ppm",
    "pair_ratio_min_ppm",
    "pair_ratio_max_ppm",
)
