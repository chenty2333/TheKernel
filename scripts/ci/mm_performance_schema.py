"""Shared, versioned contracts for TheKernel MM performance evidence."""

from __future__ import annotations


BUNDLE_SCHEMA = "thekernel-mm-performance-bundle-v10"
POLICY_SCHEMA = "thekernel-mm-performance-regression-policy-v6"
STABILITY_POLICY_SCHEMA = "thekernel-mm-performance-stability-policy-v1"
HOST_DIAGNOSTIC_SCHEMA = "thekernel-mm-performance-host-diagnostics-v1"
MEASUREMENT_MODES = frozenset({"product", "diagnostic"})
# Where the measurement physically ran. QEMU TCG establishes correctness and
# relative regression evidence only; absolute performance and architectural
# event claims require `physical` evidence, which carries its own receipt
# authority once the hardware bring-up lands. The vocabulary is
# reserved now so a TCG receipt can never masquerade as a physical one.
PLATFORM_CLASSES = frozenset({"qemu-tcg", "physical"})
# Which counter mechanism produced any PMU-derived numbers in the bundle.
PMU_SOURCES = frozenset({"none", "platform"})
PMU_SOURCE_BY_ARCH = {"x86_64": "platform"}
# Sentinel for platform fields that do not apply to the row's platform class.
PLATFORM_NOT_APPLICABLE = "not-applicable"
# The only frequency policy under which physical latency numbers are
# comparable: DVFS/boost must be pinned for cycle counts to mean anything.
PHYSICAL_FREQ_POLICY = "fixed-frequency"
KERNEL_PROFILE_BY_MODE = {
    "product": "shell",
    "diagnostic": "mm-performance",
}
MM_LOCK_DIAGNOSTIC_SENTINEL = "not-collected"
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
REPORT_COLUMNS = (
    "evidence_scope",
    "release_gate",
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
