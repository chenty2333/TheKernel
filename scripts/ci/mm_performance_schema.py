"""Shared, versioned contracts for TheKernel MM performance evidence."""

from __future__ import annotations


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
    "mode",
    "arch",
    "cpus",
    "online_cpus",
    "metrics",
    "receipt",
    "host_pre",
    "host_post",
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
