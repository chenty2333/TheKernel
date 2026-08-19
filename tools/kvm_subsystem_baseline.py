#!/usr/bin/env python3
"""KVM subsystem performance harness for the shared TKPERF guest protocol.

The guest helpers are the authority for the performance evidence format.  This
module parses exactly their TKPERF_RUN/CORRECTNESS/WINDOW/LATENCY/DONE records;
it does not invent a second serial protocol.  A cell is admitted to formal
statistics only when its correctness and latency records are both successful,
the run completed, and the lane policy accepts its topology/depth.
"""

from __future__ import annotations

import argparse
import csv
import json
import os
import shlex
import shutil
import subprocess
import sys
import time
from collections import defaultdict
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable, Mapping, Sequence

# ``python tools/kvm_subsystem_baseline.py`` puts ``tools/`` (rather than the
# repository root) on ``sys.path``.  Keep the module imports identical for
# direct-script and ``python -m tools...`` execution without duplicating the
# runner package or relying on the caller's working directory.
if __package__ in {None, ""}:
    _repo_root = str(Path(__file__).resolve().parents[1])
    if _repo_root not in sys.path:
        sys.path.insert(0, _repo_root)

from tools.qemu_runner.model import Interaction, RunLimits
from tools.qemu_runner.runner import RunConfig, RunnerError, run
from tools.kvm_scheduler_pinner import (
    BackendIdentityUnavailable,
    parse_external_backend_identities,
)


TKPERF_SCHEMA = "thekernel-perf-v1"
SCHEMA = TKPERF_SCHEMA
RUN_MARKER = "TKPERF_RUN"
CORRECTNESS_MARKER = "TKPERF_CORRECTNESS"
WINDOW_MARKER = "TKPERF_WINDOW"
LATENCY_MARKER = "TKPERF_LATENCY"
DONE_MARKER = "TKPERF_DONE"
ERROR_MARKER = "TKPERF_ERROR"
EXIT_MARKER = "TKPERF_EXIT"
DATA_MARKER = "TKPERF_DATA"
PEER_SCHEMA = "thekernel-tkpfnet1-peer-v1"
PEER_READY_MARKER = "TKPFNET1_PEER_READY"
PEER_DONE_MARKER = "TKPFNET1_PEER_DONE"
PIN_EXTERNAL_IDENTITY_KEYS = frozenset({"pid", "tgid", "exe", "starttime"})
PIN_EXTERNAL_PROCESS_V3_KEYS = frozenset(
    {
        "pid",
        "main_tid",
        "name",
        "affinity",
        "tgid",
        "exe",
        "starttime",
        "backend_authorized",
    }
)

RAW_COLUMNS = (
    "schema",
    "target",
    "repeat",
    "workload",
    "run_id",
    "cell",
    "op",
    "size",
    "qd",
    "window_warmup",
    "window_samples",
    "latency_samples",
    "wall_p50_ns",
    "wall_p99_ns",
    "cpu_p50_ns",
    "cpu_p99_ns",
    # Optional path/oracle and cost evidence.  Empty is the only valid
    # representation for an unavailable PMU counter; zero is a measurement.
    "path",
    "oracle",
    "cycles",
    "instructions",
    "cache_misses",
    "branch_misses",
    "llc_hitm",
    "cpu_cost_ns",
    "throughput_ops_per_sec",
)
SUMMARY_COLUMNS = (
    "schema",
    "target",
    "repeat",
    "workload",
    "cell",
    "op",
    "size",
    "qd",
    "status",
    "window_warmup",
    "window_samples",
    "latency_samples",
    "wall_p50_ns",
    "wall_p99_ns",
    "cpu_p50_ns",
    "cpu_p99_ns",
    # Optional evidence extensions.  Empty means unavailable; never encode a
    # missing PMU counter as a fabricated zero.
    "path",
    "oracle",
    "cycles",
    "instructions",
    "cache_misses",
    "branch_misses",
    "llc_hitm",
    "cpu_cost_ns",
    "throughput_ops_per_sec",
)

# The image builder installs these exact helper names.  A caller can override
# one path explicitly, but a formal subsystem lane always chooses from this
# map rather than silently running the scheduler helper or a smoke test.
PERF_HELPERS = {
    "io-uring-physical": "/opt/thekernel-tests/bin/thekernel-io-uring-physical-perf",
    "seccomp": "/opt/thekernel-tests/bin/thekernel-seccomp-perf",
    "packet": "/opt/thekernel-tests/bin/thekernel-packet-perf",
    "network": "/opt/thekernel-tests/bin/thekernel-packet-perf",
}
EXPECTED_WORKLOADS = {
    "io-uring-physical": "io-uring-physical",
    "seccomp": "seccomp",
    "packet": "packet",
    "network": "packet",
}
FORMAL_SUBSYSTEMS = tuple(PERF_HELPERS)
NETWORK_FORBIDDEN_TOPOLOGIES = frozenset({"selftest", "user", "slirp", "loopback"})
FORMAL_NETWORK_MODES = frozenset({"passt", "tap-vhost"})
# QEMU's root virtio disk is attached first (``/dev/vda``) and the formal
# performance data image is the runner's ``id=extra`` drive, attached second
# (``/dev/vdb``).  This is intentionally a contract, not a user-selectable
# convenience: accepting /dev/vda or an arbitrary string would make a guest
# prove only that it measured its root filesystem.
FORMAL_DATA_DEVICE = "/dev/vdb"
FORMAL_DATA_SOURCE = "qemu-drive=extra"
FORMAL_DATA_MAPPING = "unique-rootfs-extra"
PHYSICAL_PROOF_KEYS = frozenset({
    "physical_submitted", "physical_child_submitted",
    "physical_completed", "physical_child_completed", "physical_qd_highwater",
    "physical_extent_highwater", "physical_direct_bytes", "physical_quarantine",
    "direct_hit_delta",
    "direct_fallback_delta",
})
PHYSICAL_COUNTER_KEYS = (
    "physical_submitted", "physical_child_submitted",
    "physical_completed", "physical_child_completed", "physical_qd_highwater",
    "physical_extent_highwater", "physical_direct_bytes", "physical_quarantine",
    "direct_hit_delta",
    "direct_fallback_delta",
)
OPTIONAL_EVIDENCE_KEYS = frozenset({
    "path", "oracle", "cycles", "instructions", "cache_misses",
    "branch_misses", "llc_hitm", "cpu_cost_ns", "throughput_ops_per_sec",
})
PMU_EVIDENCE_KEYS = (
    "cycles", "instructions", "cache_misses", "branch_misses", "llc_hitm",
    "cpu_cost_ns",
)
OPTIONAL_EVIDENCE_COLUMNS = (
    "path", "oracle", "cycles", "instructions", "cache_misses",
    "branch_misses", "llc_hitm", "cpu_cost_ns", "throughput_ops_per_sec",
)
DELTA_KEYS = (
    "published_delta", "native_executed_delta", "interpreter_executed_delta",
    "fallback_policy_interpreter_delta", "fallback_translation_delta",
    "fallback_publication_delta", "fallback_owner_delta",
    "fallback_unavailable_delta", "jit_rejected_delta", "fallback_delta",
)
EXECUTOR_VALUES = frozenset({"auto", "interpreter", "jit"})
TOPOLOGY_VALUES = frozenset({"formal", "selftest", "user", "slirp", "loopback"})
CELL_PROOF_VALUES = frozenset({
    "no-filter", "linux-active/unsupported-ablation", "unsupported-ablation",
    "verified", "auto-active", "jit-rejected", "correctness-fail",
    "invalid-delta", "executor-proof-fail", "physical-dma",
})
DONE_PROOF_VALUES = frozenset({"verified", "unsupported", "fail"})
PHYSICAL_ORACLE_VALUES = frozenset({
    "thekernel-physical-counters", "linux-kernel-no-thekernel-counters",
})


def _physical_highwater_valid(
    queue_depth: int, highwater: int, submitted: int
) -> bool:
    """Validate achieved live-owner depth independently of requested qd.

    ``qd`` is the requested/submitted SQ batch size.  Admission and immediate
    completion can keep the achieved highwater below that request, so qd>1
    only proves that more than one request was live at some point.  The
    counter must still be a positive value bounded by the exact submitted
    count for its reset window; qd=1 remains an exact serial-depth check.
    """

    if queue_depth == 1:
        return highwater == 1
    return queue_depth > 1 and highwater >= 2 and highwater <= submitted


def _physical_extent_highwater_expected(size: int, queue_depth: int) -> int:
    """Return the lower publication depth required by this fixed workload."""

    return 16 if size == 256 * 1024 else 1


def _read_int(path: Path) -> int | None:
    try:
        value = path.read_text(encoding="ascii").strip()
    except (OSError, UnicodeDecodeError):
        return None
    if value.startswith("-"):
        digits = value[1:]
        if not digits.isdecimal():
            return None
        return -int(digits, 10)
    if not value.isdecimal():
        return None
    return int(value, 10)


def host_cost_capabilities() -> dict[str, object]:
    """Report host PMU authority without claiming an unmeasured counter.

    The KVM helper protocol already carries optional per-cell cost fields.  A
    formal run must also say whether those fields could have been populated on
    this host.  This is a capability receipt only: it never writes zero for a
    missing event and it does not turn a host-side ``perf`` probe into guest
    measurements.
    """

    perf_path = shutil.which("perf")
    pmu_root = Path("/sys/bus/event_source/devices/cpu")
    pmu_present = pmu_root.is_dir()
    paranoid_path = Path("/proc/sys/kernel/perf_event_paranoid")
    paranoid = _read_int(paranoid_path)
    if perf_path is None:
        host_reason = "perf-not-installed"
        host_status = "unavailable"
    elif not pmu_present:
        host_reason = "cpu-pmu-not-exposed"
        host_status = "unavailable"
    elif paranoid is None:
        host_reason = "perf-permission-policy-unreadable"
        host_status = "permission-unknown"
    elif paranoid >= 2:
        host_reason = f"perf_event_paranoid={paranoid}"
        host_status = "permission-restricted"
    else:
        host_reason = "event-open-not-probed"
        host_status = "available-unprobed"

    event_dir = pmu_root / "events"
    llc_aliases = (
        "mem_load_l3_hit_retired.xsnp_hitm",
        "mem_load_l3_hit_retired.xsnp_miss",
        "offcore_response.demand_data_rd.l3_hit.snoop_hitm",
    )
    exposed_llc_aliases = [name for name in llc_aliases if (event_dir / name).is_file()]
    metrics: dict[str, dict[str, object]] = {}
    for name in ("cycles", "instructions", "cache_misses", "branch_misses"):
        metrics[name] = {
            "status": host_status,
            "reason": host_reason,
            "measured": False,
        }
    if host_status == "unavailable":
        llc_status = "unavailable"
        llc_reason = host_reason
    elif not exposed_llc_aliases:
        llc_status = "event-not-exposed"
        llc_reason = "no-known-llc-hitm-alias-in-cpu-pmu"
    else:
        llc_status = host_status
        llc_reason = host_reason
    metrics["llc_hitm"] = {
        "status": llc_status,
        "reason": llc_reason,
        "event_aliases": exposed_llc_aliases,
        "measured": False,
    }
    metrics["cpu_cost_ns"] = {
        "status": "helper-supported",
        "reason": "process-cpu-clock-in-TKPERF_LATENCY",
        "measured": False,
    }
    return {
        "schema": "thekernel-perf-cost-capabilities-v1",
        "perf": {"path": perf_path, "status": "available" if perf_path else "unavailable"},
        "cpu_pmu_sysfs": {"path": str(pmu_root), "present": pmu_present},
        "perf_event_paranoid": paranoid,
        "permission": {"status": host_status, "reason": host_reason},
        "metrics": metrics,
        "throughput": {
            "status": "derived-for-seccomp",
            "field": "throughput_ops_per_sec",
            "definition": "floor(1e9 / wall_p50_ns) for serial syscall cells",
            "concurrent": False,
        },
        "measurement_policy": "PMU values are recorded only when helper evidence is present; unavailable stays empty",
    }


class BaselineError(ValueError):
    """Raised for malformed TKPERF evidence or an unsafe lane setup."""


class TopologyUnavailable(BaselineError):
    """Raised when the host cannot provide a trustworthy CPU topology."""


@dataclass(frozen=True)
class Sample:
    target: str
    repeat: int
    workload: str
    run_id: str
    cell: str
    op: str | None
    size: int | None
    qd: int | None
    window_warmup: int
    window_samples: int
    latency_samples: int
    wall_p50_ns: int
    wall_p99_ns: int
    cpu_p50_ns: int
    cpu_p99_ns: int
    attributes: tuple[tuple[str, str], ...] = ()

    @property
    def topology(self) -> str | None:
        attrs = dict(self.attributes)
        for key in ("topology", "mode", "network", "path", "transport"):
            value = attrs.get(key)
            if value:
                return value.lower()
        return None


@dataclass(frozen=True)
class PerfCell:
    target: str
    repeat: int
    workload: str
    run_id: str
    cell: str
    op: str | None
    size: int | None
    qd: int | None
    attributes: tuple[tuple[str, str], ...]
    correctness_status: str | None = None
    window_status: str | None = None
    latency_status: str | None = None
    window_warmup: int | None = None
    window_samples: int | None = None
    latency_samples: int | None = None
    wall_p50_ns: int | None = None
    wall_p99_ns: int | None = None
    cpu_p50_ns: int | None = None
    cpu_p99_ns: int | None = None

    @property
    def formal_policy_reason(self) -> str | None:
        if self.workload == "io-uring-physical":
            if self.target == "thekernel":
                if self.correctness_status == "unsupported":
                    return "thekernel-physical-cell-unsupported"
                if self.correctness_status == "ok" and not self.physical_proof:
                    return "missing-thekernel-physical-proof"
            elif self.target == "linux":
                if self.correctness_status == "ok" and not self.linux_path_proof:
                    return "missing-linux-path-boundary"
        topology = dict(self.attributes)
        values = {
            str(topology.get(key, "")).lower()
            for key in ("topology", "mode", "network", "path", "transport")
        }
        if values & NETWORK_FORBIDDEN_TOPOLOGIES:
            return "non-formal-network-topology"
        if self.proof_policy_reason is not None:
            return self.proof_policy_reason
        return None

    @property
    def physical_proof(self) -> bool:
        """Return whether a TheKernel cell proves its physical DMA path.

        The correctness marker proves one batch.  WINDOW/LATENCY carry the
        independent counter delta for the complete timed interval (all
        warmup plus latency batches), so their submitted/completed/hit/byte
        counts scale with that interval while ``physical_qd_highwater`` stays
        an absolute value within its reset window.
        """

        if self.workload != "io-uring-physical" or self.target != "thekernel":
            return False
        attrs = dict(self.attributes)
        if attrs.get("proof") != "physical-dma":
            return False
        if attrs.get("path") != "thekernel-physical-dma":
            return False
        if attrs.get("oracle") != "thekernel-physical-counters":
            return False
        if self.qd is None or self.size is None:
            return False
        values: dict[str, int] = {}
        for key in PHYSICAL_COUNTER_KEYS:
            value = attrs.get(key)
            if value is None or not value.isdecimal():
                return False
            values[key] = int(value, 10)
        if self.window_warmup is None or self.window_samples is None:
            return False
        if self.latency_samples is None or self.latency_samples != self.window_samples:
            return False
        measurement_batches = self.window_warmup + self.window_samples
        if measurement_batches <= 0:
            return False
        expected_requests = self.qd * measurement_batches
        expected_bytes = expected_requests * self.size
        expected_children = expected_requests * _physical_extent_highwater_expected(
            self.size, self.qd
        )
        return (
            values["physical_submitted"] == expected_requests
            and values["physical_child_submitted"] == expected_children
            and values["physical_completed"] == expected_requests
            and values["physical_child_completed"] == expected_children
            and _physical_highwater_valid(
                self.qd, values["physical_qd_highwater"], expected_requests
            )
            and values["physical_extent_highwater"]
            == _physical_extent_highwater_expected(self.size, self.qd)
            and values["physical_direct_bytes"] == expected_bytes
            and values["physical_quarantine"] == 0
            and values["direct_hit_delta"] == expected_requests
            and values["direct_fallback_delta"] == 0
        )

    @property
    def linux_path_proof(self) -> bool:
        """Return whether Linux evidence states its non-TheKernel oracle."""

        if self.workload != "io-uring-physical" or self.target != "linux":
            return False
        attrs = dict(self.attributes)
        return (
            attrs.get("path") == "linux-io-uring"
            and attrs.get("oracle") == "linux-kernel-no-thekernel-counters"
            and attrs.get("proof") == "linux-active/unsupported-ablation"
            and not any(key in attrs for key in PHYSICAL_COUNTER_KEYS)
        )

    @property
    def executor(self) -> str | None:
        return dict(self.attributes).get("executor")

    @property
    def proof(self) -> str | None:
        return dict(self.attributes).get("proof")

    @property
    def claim_degraded(self) -> bool:
        return (
            self.executor == "auto"
            and self.proof in {
                "linux-active/unsupported-ablation",
                "unsupported-ablation",
            }
        )

    def _proof_deltas(self) -> dict[str, int | None]:
        attrs = dict(self.attributes)
        result: dict[str, int | None] = {}
        for key in DELTA_KEYS:
            value = attrs.get(key)
            if value is None or value == "unsupported":
                result[key] = None
            elif value.isdecimal():
                result[key] = int(value, 10)
            else:
                # Parser validation normally catches this. Keep hand-built
                # cells fail-closed as well.
                result[key] = None
        return result

    @property
    def proof_policy_reason(self) -> str | None:
        executor = self.executor
        proof = self.proof
        if executor is None:
            return None
        if self.correctness_status == "unsupported":
            return None if proof in {
                "unsupported-ablation", "linux-active/unsupported-ablation"
            } else "unsupported-proof-mismatch"
        if self.correctness_status != "ok":
            return "executor-proof-not-successful"
        # A no-filter control does not execute an executor and has no executor
        # delta to prove; it remains a valid control baseline.
        if proof == "no-filter":
            return None
        deltas = self._proof_deltas()
        if executor == "auto":
            if proof == "linux-active/unsupported-ablation":
                return None
            if proof != "auto-active":
                return "auto-proof-mismatch"
            if (
                deltas["published_delta"] is None
                or (
                    (deltas["native_executed_delta"] or 0) <= 0
                    and (deltas["interpreter_executed_delta"] or 0) <= 0
                )
            ):
                return "auto-proof-delta-invalid"
            return None
        if executor not in {"jit", "interpreter"} or proof != "verified":
            return "explicit-executor-proof-mismatch"
        if any(value is None for value in deltas.values()):
            return "explicit-executor-delta-unsupported"
        if deltas["fallback_delta"] != sum(
            deltas[key] or 0
            for key in (
                "fallback_policy_interpreter_delta",
                "fallback_translation_delta",
                "fallback_publication_delta",
                "fallback_owner_delta",
                "fallback_unavailable_delta",
            )
        ):
            return "explicit-executor-fallback-delta-mismatch"
        if (deltas["published_delta"] or 0) <= 0:
            return "explicit-executor-published-delta-invalid"
        if executor == "jit":
            valid = (
                (deltas["native_executed_delta"] or 0) > 0
                and (deltas["interpreter_executed_delta"] or 0) == 0
                and all(
                    (deltas[key] or 0) == 0
                    for key in (
                        "fallback_policy_interpreter_delta",
                        "fallback_translation_delta",
                        "fallback_publication_delta",
                        "fallback_owner_delta",
                        "fallback_unavailable_delta",
                        "jit_rejected_delta",
                        "fallback_delta",
                    )
                )
            )
        else:
            valid = (
                (deltas["native_executed_delta"] or 0) == 0
                and (deltas["interpreter_executed_delta"] or 0) > 0
                and (deltas["fallback_policy_interpreter_delta"] or 0) > 0
                and all(
                    (deltas[key] or 0) == 0
                    for key in (
                        "fallback_translation_delta",
                        "fallback_publication_delta",
                        "fallback_owner_delta",
                        "fallback_unavailable_delta",
                        "jit_rejected_delta",
                    )
                )
            )
        return None if valid else "explicit-executor-delta-proof-failed"

    @property
    def complete_ok(self) -> bool:
        return (
            self.correctness_status == "ok"
            and self.window_status == "ok"
            and self.latency_status == "ok"
            and self.window_warmup is not None
            and self.window_samples is not None
            and self.latency_samples is not None
            and self.wall_p50_ns is not None
            and self.wall_p99_ns is not None
            and self.cpu_p50_ns is not None
            and self.cpu_p99_ns is not None
        )

    @property
    def formal_ok(self) -> bool:
        return self.complete_ok and self.formal_policy_reason is None

    def sample(self) -> Sample:
        if not self.formal_ok:
            raise BaselineError(f"cell {self.cell} is not a formal latency measurement")
        assert self.window_warmup is not None
        assert self.window_samples is not None
        assert self.latency_samples is not None
        assert self.wall_p50_ns is not None
        assert self.wall_p99_ns is not None
        assert self.cpu_p50_ns is not None
        assert self.cpu_p99_ns is not None
        return Sample(
            self.target, self.repeat, self.workload, self.run_id, self.cell,
            self.op, self.size, self.qd, self.window_warmup,
            self.window_samples, self.latency_samples, self.wall_p50_ns,
            self.wall_p99_ns, self.cpu_p50_ns, self.cpu_p99_ns,
            self.attributes,
        )


@dataclass(frozen=True)
class SubsystemRun:
    target: str
    repeat: int
    workload: str
    run_id: str
    expected_cells: int
    cells: tuple[PerfCell, ...]
    done_status: str | None
    done: bool
    error: str | None = None
    runner_status: int | None = None
    exit_seen: bool = False
    data_proof: tuple[tuple[str, str], ...] | None = None

    @property
    def status(self) -> str:
        if self.error is not None:
            return "error"
        if not self.done:
            return "incomplete"
        if self.done_status != "ok":
            return "unsupported"
        if len(self.cells) != self.expected_cells:
            return "incomplete"
        if (
            self.workload == "io-uring-physical"
            and self.target == "thekernel"
            and any(cell.correctness_status == "unsupported" for cell in self.cells)
        ):
            return "unsupported"
        return "ok"

    @property
    def samples(self) -> tuple[Sample, ...]:
        return tuple(cell.sample() for cell in self.cells if cell.formal_ok)

    @property
    def correctness(self) -> str:
        if not self.cells:
            return "missing"
        if all(cell.correctness_status == "ok" for cell in self.cells):
            return "ok"
        if any(cell.correctness_status == "fail" for cell in self.cells):
            return "fail"
        return "unsupported"

    @property
    def claim_degraded(self) -> bool:
        """Whether any admitted auto-executor cell is Linux fallback evidence."""

        return any(cell.claim_degraded for cell in self.cells)


@dataclass(frozen=True)
class TargetImages:
    """Explicit artifacts for one comparison target and its boot policy."""

    kernel: Path
    rootfs: Path
    esp: Path | None
    extra_block: Path | None
    direct_kernel: bool
    initrd: Path | None = None
    cmdline: str | None = None


def _fields(line: str, marker: str) -> dict[str, str] | None:
    stripped = line.strip()
    if not stripped.startswith(marker + " "):
        return None
    fields: dict[str, str] = {}
    for token in stripped.split()[1:]:
        key, equal, value = token.partition("=")
        # The guest helpers append the libc strerror in parentheses to
        # TKPERF_ERROR, e.g. ``errno=EINVAL (Invalid argument)``.  It is
        # human context, not a second protocol field; keep the machine fields
        # strict while accepting that exact helper output shape.
        if not equal and marker == ERROR_MARKER and token.startswith("("):
            break
        if not equal or not key or not value or key in fields:
            raise BaselineError(f"invalid {marker} record: {stripped!r}")
        fields[key] = value
    return fields


def _value(fields: Mapping[str, str], key: str, marker: str) -> str:
    value = fields.get(key)
    if value is None or value == "":
        raise BaselineError(f"{marker} is missing {key}")
    return value


def _integer(
    fields: Mapping[str, str],
    key: str,
    marker: str,
    *,
    allow_unsupported: bool = False,
    positive: bool = False,
) -> int | None:
    value = fields.get(key)
    if value is None:
        raise BaselineError(f"{marker} is missing {key}")
    if allow_unsupported and value == "unsupported":
        return None
    if not value.isdecimal():
        raise BaselineError(f"{marker} has invalid {key}: {value!r}")
    result = int(value, 10)
    if positive and result <= 0:
        raise BaselineError(f"{marker} requires positive {key}")
    return result


def _schema(fields: Mapping[str, str], marker: str) -> None:
    if fields.get("schema") != TKPERF_SCHEMA:
        raise BaselineError(
            f"unsupported {marker} schema: {fields.get('schema')!r}, expected {TKPERF_SCHEMA!r}"
        )


def _check_allowed(
    fields: Mapping[str, str],
    marker: str,
    allowed: frozenset[str],
) -> None:
    unknown = sorted(set(fields) - allowed)
    if unknown:
        raise BaselineError(f"{marker} has unknown fields: {unknown}")


def _run_id(value: str, marker: str) -> str:
    if len(value) != 16 or any(char not in "0123456789abcdefABCDEF" for char in value):
        raise BaselineError(f"{marker} has invalid run_id: {value!r}")
    return value.lower()


def _identity(
    fields: Mapping[str, str],
    *,
    marker: str,
    run_id: str,
    workload: str,
    cell_required: bool = True,
) -> tuple[str, str | None, int | None, int | None, dict[str, str]]:
    if _value(fields, "run_id", marker).lower() != run_id:
        raise BaselineError(f"{marker} run_id mismatch")
    if _value(fields, "workload", marker) != workload:
        raise BaselineError(f"{marker} workload mismatch")
    cell = _value(fields, "cell", marker) if cell_required else ""
    op = fields.get("op")
    size = _integer(fields, "size", marker, positive=True) if "size" in fields else None
    qd = _integer(fields, "qd", marker, positive=True) if "qd" in fields else None
    if workload == "io-uring-physical" and (op is None or size is None or qd is None):
        raise BaselineError(f"{marker} io-uring cell requires op, size, and qd")
    return cell, op, size, qd, dict(fields)


def _cell_from(
    fields: Mapping[str, str],
    *,
    target: str,
    repeat: int,
    run_id: str,
    workload: str,
    marker: str,
    cells: dict[str, PerfCell],
    matrix: tuple[frozenset[str], frozenset[int], frozenset[int]] | None = None,
) -> PerfCell:
    cell, op, size, qd, attrs = _identity(
        fields, marker=marker, run_id=run_id, workload=workload
    )
    if matrix is not None:
        ops, sizes, qds = matrix
        if op not in ops or size not in sizes or qd not in qds:
            raise BaselineError(
                f"{marker} cell {cell} is outside the TKPERF_RUN op/size/qd matrix"
            )
    key = cell
    current = cells.get(key)
    if current is None:
        current = PerfCell(
            target, repeat, workload, run_id, cell, op, size, qd,
            tuple(sorted((key, value) for key, value in attrs.items()
                         if key not in {"schema", "workload", "run_id", "cell",
                                        "op", "size", "qd", "status"})),
        )
        cells[key] = current
    else:
        if (current.op, current.size, current.qd) != (op, size, qd):
            raise BaselineError(f"{marker} identity mismatch for cell {cell}")
        existing = dict(current.attributes)
        incoming = {
            key: value
            for key, value in attrs.items()
            if key not in {"schema", "workload", "run_id", "cell", "op", "size", "qd",
                           "status"}
        }
        merged = tuple(sorted({**existing, **incoming}.items()))
        if merged != current.attributes:
            _replace_cell(cells, current, attributes=merged)
            current = cells[key]
    return current


def _replace_cell(cells: dict[str, PerfCell], current: PerfCell, **changes: object) -> None:
    cells[current.cell] = PerfCell(**{**current.__dict__, **changes})


def _parse_status(fields: Mapping[str, str], marker: str) -> str:
    status = _value(fields, "status", marker)
    if status not in {"ok", "unsupported", "fail"}:
        raise BaselineError(f"{marker} has invalid status: {status!r}")
    return status


def _validate_run_context(fields: Mapping[str, str], marker: str, workload: str) -> None:
    executor = fields.get("executor")
    if executor is not None and executor not in EXECUTOR_VALUES:
        raise BaselineError(f"{marker} has invalid executor: {executor!r}")
    domain = fields.get("domain")
    if domain is not None and domain != workload:
        raise BaselineError(f"{marker} domain/workload mismatch")
    topology = fields.get("topology")
    if topology is not None:
        if workload != "packet" or topology not in TOPOLOGY_VALUES:
            raise BaselineError(f"{marker} has invalid topology: {topology!r}")


def _validate_marker_context(
    fields: Mapping[str, str],
    marker: str,
    run_fields: Mapping[str, str],
) -> None:
    for key in ("executor", "domain", "topology"):
        run_value = run_fields.get(key)
        marker_value = fields.get(key)
        if run_value is None:
            if marker_value is not None:
                raise BaselineError(f"{marker} introduces unannounced {key}")
        elif marker_value != run_value:
            raise BaselineError(f"{marker} {key} mismatch")


def _validate_proof_fields(
    fields: Mapping[str, str], marker: str, status: str
) -> None:
    proof = fields.get("proof")
    if fields.get("executor") is not None and proof is None:
        raise BaselineError(f"{marker} is missing proof for executor evidence")
    if proof is not None and proof not in CELL_PROOF_VALUES:
        raise BaselineError(f"{marker} has invalid proof: {proof!r}")
    present = {key for key in DELTA_KEYS if key in fields}
    if present and present != set(DELTA_KEYS):
        missing = sorted(set(DELTA_KEYS) - present)
        raise BaselineError(f"{marker} has incomplete proof deltas: {missing}")
    for key in present:
        value = fields[key]
        if value != "unsupported" and not value.isdecimal():
            raise BaselineError(f"{marker} has invalid {key}: {value!r}")
    if "oracle" in fields and fields["oracle"] not in {
        "accept-half", *PHYSICAL_ORACLE_VALUES,
    }:
        raise BaselineError(f"{marker} has invalid oracle: {fields['oracle']!r}")
    if proof is not None and status == "ok" and proof in {
        "unsupported-ablation", "correctness-fail", "jit-rejected",
        "invalid-delta", "executor-proof-fail",
    }:
        raise BaselineError(f"{marker} successful status has non-success proof: {proof!r}")


def _validate_optional_evidence(fields: Mapping[str, str], marker: str) -> None:
    """Validate optional path/oracle/PMU evidence without inventing values.

    Helpers may grow a path oracle or perf-event counters independently of
    this parser.  When present, counters are non-negative decimal
    observations; when unavailable they are omitted (and later serialized as
    an empty TSV field), never represented as a synthetic zero.
    """

    if "path" in fields and not fields["path"]:
        raise BaselineError(f"{marker} has an empty path")
    if "oracle" in fields and not fields["oracle"]:
        raise BaselineError(f"{marker} has an empty oracle")
    for key in (*PMU_EVIDENCE_KEYS, "throughput_ops_per_sec"):
        if key in fields and (
            not fields[key].isdecimal() or int(fields[key], 10) < 0
        ):
            raise BaselineError(f"{marker} has invalid optional evidence {key}: {fields[key]!r}")


def _validate_physical_counter_fields(
    fields: Mapping[str, str], marker: str, *, target: str,
    workload: str, qd: int | None, size: int | None, status: str,
    batches: int | None = None,
) -> None:
    """Validate physical DMA evidence for one marker's reset window."""

    if workload != "io-uring-physical" or status != "ok":
        return
    present = set(fields) & set(PHYSICAL_COUNTER_KEYS)
    if target == "linux":
        if present:
            raise BaselineError(
                f"{marker} Linux path must not claim TheKernel physical counters"
            )
        return
    if target != "thekernel":
        raise BaselineError(f"{marker} has unsupported physical target {target!r}")
    missing = [key for key in PHYSICAL_COUNTER_KEYS if key not in fields]
    if missing:
        raise BaselineError(
            f"{marker} TheKernel success is missing physical proof: {missing}"
        )
    if qd is None or size is None:
        raise BaselineError(f"{marker} physical proof has incomplete cell identity")
    if batches is None or batches <= 0:
        raise BaselineError(f"{marker} physical proof has invalid batch count")
    values: dict[str, int] = {}
    for key in PHYSICAL_COUNTER_KEYS:
        value = fields[key]
        if not value.isdecimal():
            raise BaselineError(f"{marker} has invalid physical counter {key}: {value!r}")
        values[key] = int(value, 10)
    expected_requests = qd * batches
    expected_children = expected_requests * _physical_extent_highwater_expected(size, qd)
    if (
        values["physical_submitted"] != expected_requests
        or values["physical_child_submitted"] != expected_children
        or values["physical_completed"] != expected_requests
        or values["physical_child_completed"] != expected_children
        or values["physical_completed"] > values["physical_submitted"]
        or not _physical_highwater_valid(
            qd, values["physical_qd_highwater"], expected_requests
        )
        or values["physical_extent_highwater"]
        != _physical_extent_highwater_expected(size, qd)
        or values["physical_direct_bytes"] != expected_requests * size
        or values["physical_quarantine"] != 0
        or values["direct_hit_delta"] != expected_requests
        or values["direct_fallback_delta"] != 0
    ):
        raise BaselineError(f"{marker} has an invalid TheKernel physical proof")


def parse_tkperf_log(
    path: Path,
    *,
    target: str,
    repeat: int,
    expected_workload: str | None = None,
    expected_topology: str | None = None,
) -> SubsystemRun:
    """Parse one exact TKPERF helper log, preserving each cell/window/latency."""

    try:
        lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
    except OSError as error:
        return SubsystemRun(target, repeat, "unknown", "", 0, (), None, False, str(error))

    run_fields: dict[str, str] | None = None
    run_id = ""
    workload = ""
    expected_cells = 0
    cells: dict[str, PerfCell] = {}
    # IO cell coverage is keyed by the declared matrix tuple, never by the
    # helper-provided cell label.  A label can be changed or duplicated by a
    # malformed stream; the (op,size,qd) identity is the formal contract.
    matrix_cell_names: dict[tuple[str, int, int], str] = {}
    done_status: str | None = None
    done_unsupported: int | None = None
    saw_done = False
    exit_seen = False
    exit_status: int | None = None
    error_message: str | None = None
    runner_exit: int | None = None
    run_matrix: tuple[frozenset[str], frozenset[int], frozenset[int]] | None = None
    data_proof: tuple[tuple[str, str], ...] | None = None
    known = {
        RUN_MARKER, CORRECTNESS_MARKER, WINDOW_MARKER, LATENCY_MARKER,
        DONE_MARKER, ERROR_MARKER, EXIT_MARKER, DATA_MARKER,
    }

    def observe_matrix_cell(current: PerfCell, marker: str) -> None:
        if workload != "io-uring-physical":
            return
        if current.op is None or current.size is None or current.qd is None:
            raise BaselineError(f"{marker} io-uring cell has incomplete matrix identity")
        identity = (current.op, current.size, current.qd)
        previous = matrix_cell_names.get(identity)
        if previous is not None and previous != current.cell:
            raise BaselineError(
                f"{marker} duplicates TKPERF matrix tuple {identity!r} "
                f"under cells {previous!r} and {current.cell!r}"
            )
        matrix_cell_names[identity] = current.cell

    for line_number, line in enumerate(lines, 1):
        stripped = line.strip()
        if not stripped:
            continue
        name = stripped.split(" ", 1)[0]
        if not name.startswith("TKPERF_"):
            continue
        if name in known and " " not in stripped:
            raise BaselineError(
                f"malformed {name} marker at line {line_number}: marker requires fields"
            )
        if name not in known:
            raise BaselineError(f"unknown TKPERF marker {name!r} at line {line_number}")
        if saw_done and name != EXIT_MARKER:
            raise BaselineError(f"TKPERF marker appears after DONE at line {line_number}")
        fields = _fields(stripped, name)
        assert fields is not None
        _schema(fields, name)
        _validate_optional_evidence(fields, name)

        if name == RUN_MARKER:
            if run_fields is not None:
                raise BaselineError(f"duplicate {RUN_MARKER}")
            _check_allowed(
                fields, name,
                frozenset({
                    "schema", "workload", "run_id", "cells", "sizes", "qd", "ops",
                    "clocks", "executor", "domain", "topology", "oracle", "proof",
                    *OPTIONAL_EVIDENCE_KEYS,
                    *PHYSICAL_PROOF_KEYS,
                }),
            )
            workload = _value(fields, "workload", name)
            if expected_workload is not None and workload != expected_workload:
                raise BaselineError(f"{name} workload mismatch: {workload!r}")
            _validate_run_context(fields, name, workload)
            if expected_topology is not None and fields.get("topology") != expected_topology:
                raise BaselineError(
                    f"{name} topology mismatch: expected {expected_topology!r}, got {fields.get('topology')!r}"
                )
            run_id = _run_id(_value(fields, "run_id", name), name)
            expected_cells = _integer(fields, "cells", name, positive=True) or 0
            if workload == "io-uring-physical":
                matrix_values: list[object] = []
                for key, positive in (("ops", False), ("sizes", True), ("qd", True)):
                    raw = _value(fields, key, name)
                    parts = raw.split(",")
                    if not parts or any(not part for part in parts):
                        raise BaselineError(f"{name} has an empty {key} matrix")
                    if key == "ops":
                        values = frozenset(parts)
                        if len(values) != len(parts):
                            raise BaselineError(f"{name} has duplicate ops")
                    else:
                        parsed = []
                        for part in parts:
                            if not part.isdecimal() or (positive and int(part, 10) <= 0):
                                raise BaselineError(f"{name} has invalid {key} value: {part!r}")
                            parsed.append(int(part, 10))
                        values = frozenset(parsed)
                        if len(values) != len(parsed):
                            raise BaselineError(f"{name} has duplicate {key}")
                    matrix_values.append(values)
                run_matrix = (matrix_values[0], matrix_values[1], matrix_values[2])  # type: ignore[assignment]
                expected_matrix_cells = (
                    len(run_matrix[0]) * len(run_matrix[1]) * len(run_matrix[2])
                )
                if expected_cells != expected_matrix_cells:
                    raise BaselineError(
                        f"{name} cell count {expected_cells} does not match op/size/qd matrix {expected_matrix_cells}"
                    )
            run_fields = fields
            continue

        if name == EXIT_MARKER:
            if not saw_done:
                raise BaselineError(f"{name} appears before {DONE_MARKER}")
            if exit_seen:
                raise BaselineError(f"duplicate {EXIT_MARKER}")
            _check_allowed(fields, name, frozenset({"schema", "status"}))
            exit_status = _integer(fields, "status", name)
            runner_exit = exit_status
            exit_seen = True
            continue

        if name == ERROR_MARKER:
            _check_allowed(fields, name, frozenset({"schema", "workload", "stage", "errno", "reason"}))
            if error_message is not None:
                raise BaselineError("duplicate TKPERF_ERROR")
            error_message = fields.get("stage", "guest_error")
            continue

        if name == DATA_MARKER:
            if run_fields is None:
                raise BaselineError(f"{name} appears before {RUN_MARKER}")
            if workload != "io-uring-physical":
                raise BaselineError(f"{name} is only valid for io-uring-physical")
            if data_proof is not None:
                raise BaselineError(f"duplicate {name}")
            _check_allowed(
                fields,
                name,
                frozenset({
                    "schema", "workload", "run_id", "device", "mount", "fs",
                    "major", "minor", "identity", "mapping",
                }),
            )
            if _value(fields, "workload", name) != workload or _run_id(_value(fields, "run_id", name), name) != run_id:
                raise BaselineError(f"{name} identity mismatch")
            if (
                fields.get("fs") != "ext4"
                or fields.get("identity") != "verified"
                or fields.get("mapping") != FORMAL_DATA_MAPPING
            ):
                raise BaselineError(f"{name} does not prove an ext4 data disk")
            device = _value(fields, "device", name)
            mount = _value(fields, "mount", name)
            major_number = _value(fields, "major", name)
            minor_number = _value(fields, "minor", name)
            if (
                device != FORMAL_DATA_DEVICE
                or not mount.startswith("/")
                or mount in {"", "/"}
                or mount == "/tmp"
                or mount.startswith("/tmp/")
            ):
                raise BaselineError(
                    f"{name} has an invalid device or mount; must prove the runner extra disk "
                    f"at {FORMAL_DATA_DEVICE} and a non-root mount"
                )
            for key in ("major", "minor"):
                value = major_number if key == "major" else minor_number
                if not value.isdecimal():
                    raise BaselineError(f"{name} has invalid {key}")
            data_proof = tuple(sorted(fields.items()))
            continue

        if run_fields is None:
            raise BaselineError(f"{name} appears before {RUN_MARKER}")

        if name == DONE_MARKER:
            if saw_done:
                raise BaselineError(f"duplicate {DONE_MARKER}")
            _check_allowed(
                fields,
                name,
                frozenset({
                    "schema", "workload", "run_id", "status", "cells", "unsupported",
                    "executor", "domain", "topology", "proof",
                }),
            )
            _validate_marker_context(fields, name, run_fields)
            if _value(fields, "workload", name) != workload or _run_id(_value(fields, "run_id", name), name) != run_id:
                raise BaselineError(f"{name} identity mismatch")
            done_status = _parse_status(fields, name)
            done_proof = fields.get("proof")
            if fields.get("executor") is not None and done_proof is None:
                raise BaselineError(f"{name} is missing proof for executor evidence")
            if done_proof is not None and done_proof not in DONE_PROOF_VALUES:
                raise BaselineError(f"{name} has invalid proof: {done_proof!r}")
            if done_proof is not None and done_proof != {
                "ok": "verified", "unsupported": "unsupported", "fail": "fail"
            }[done_status]:
                raise BaselineError(f"{name} status/proof mismatch")
            done_cells = _integer(fields, "cells", name, positive=True) or 0
            if done_cells != expected_cells:
                raise BaselineError(f"{name} cell count mismatch")
            unsupported = _integer(fields, "unsupported", name) if "unsupported" in fields else 0
            if unsupported is None:
                raise BaselineError(f"{name} unsupported count is invalid")
            # Packet helper capability failures are explicit at CORRECTNESS
            # and intentionally emit no WINDOW/LATENCY records. Treat that
            # cell as an explicit unsupported capability boundary; a missing
            # marker on an otherwise attempted cell remains invalid later.
            for key, cell in tuple(cells.items()):
                if cell.correctness_status == "unsupported":
                    changes: dict[str, object] = {}
                    if cell.window_status is None:
                        changes.update(window_status="unsupported", window_warmup=None, window_samples=None)
                    if cell.latency_status is None:
                        changes.update(
                            latency_status="unsupported",
                            latency_samples=None,
                            wall_p50_ns=None,
                            wall_p99_ns=None,
                            cpu_p50_ns=None,
                            cpu_p99_ns=None,
                        )
                    if changes:
                        cells[key] = PerfCell(**{**cell.__dict__, **changes})
            done_unsupported = unsupported
            saw_done = True
            continue

        if name == CORRECTNESS_MARKER:
            allowed = frozenset({
                "schema", "workload", "run_id", "cell", "op", "size", "qd",
                "status", "reason", "cqe", "missing", "duplicate", "digest",
                "user_data", "calls", "checksum", "topology", "mode", "network",
                "path", "transport", "case", "kind", "packet_size", "proof",
                "executor", "domain",
                "oracle", "sent", "accepted", "required", "invalid", "rejected",
                *OPTIONAL_EVIDENCE_KEYS,
                *DELTA_KEYS,
                *PHYSICAL_PROOF_KEYS,
            })
            _check_allowed(fields, name, allowed)
            _validate_marker_context(fields, name, run_fields)
            current = _cell_from(fields, target=target, repeat=repeat, run_id=run_id,
                                  workload=workload, marker=name, cells=cells,
                                  matrix=run_matrix)
            observe_matrix_cell(current, name)
            if current.correctness_status is not None:
                raise BaselineError(f"duplicate correctness for cell {current.cell}")
            status = _parse_status(fields, name)
            _validate_proof_fields(fields, name, status)
            _validate_physical_counter_fields(
                fields,
                name,
                target=target,
                workload=workload,
                qd=current.qd,
                size=current.size,
                status=status,
                batches=1,
            )
            for numeric in ("cqe", "missing", "duplicate", "calls"):
                if numeric in fields and status == "ok":
                    _integer(fields, numeric, name, positive=False)
            if status == "ok" and fields.get("missing") not in (None, "0"):
                raise BaselineError(f"{name} reports missing CQEs")
            if status == "ok" and fields.get("duplicate") not in (None, "0"):
                raise BaselineError(f"{name} reports duplicate CQEs")
            _replace_cell(cells, current, correctness_status=status)
            continue

        if name == WINDOW_MARKER:
            allowed = frozenset({
                "schema", "workload", "run_id", "cell", "op", "size", "qd",
                "status", "reason", "warmup", "samples", "clocks", "topology",
                "mode", "network", "path", "transport", "case", "kind", "packet_size", "proof",
                *OPTIONAL_EVIDENCE_KEYS,
                "executor", "domain",
                *PHYSICAL_PROOF_KEYS,
            })
            _check_allowed(fields, name, allowed)
            _validate_marker_context(fields, name, run_fields)
            current = _cell_from(fields, target=target, repeat=repeat, run_id=run_id,
                                  workload=workload, marker=name, cells=cells,
                                  matrix=run_matrix)
            observe_matrix_cell(current, name)
            if current.window_status is not None:
                raise BaselineError(f"duplicate window for cell {current.cell}")
            status = _parse_status(fields, name)
            warmup = _integer(fields, "warmup", name, allow_unsupported=status == "unsupported")
            sample_count = _integer(fields, "samples", name, allow_unsupported=status == "unsupported")
            clocks = _value(fields, "clocks", name)
            if clocks != "monotonic,process-cpu":
                raise BaselineError(f"{name} has unexpected clocks: {clocks!r}")
            if status == "ok" and (warmup is None or sample_count is None):
                raise BaselineError(f"{name} successful window has unsupported counts")
            _validate_physical_counter_fields(
                fields,
                name,
                target=target,
                workload=workload,
                qd=current.qd,
                size=current.size,
                status=status,
                batches=(warmup or 0) + (sample_count or 0)
                if status == "ok" else None,
            )
            _replace_cell(cells, current, window_status=status,
                          window_warmup=warmup, window_samples=sample_count)
            continue

        if name == LATENCY_MARKER:
            allowed = frozenset({
                "schema", "workload", "run_id", "cell", "op", "size", "qd",
                "status", "reason", "samples", "wall_p50_ns", "wall_p99_ns",
                "cpu_p50_ns", "cpu_p99_ns", "sink", "topology", "mode", "network",
                "path", "transport", "case", "kind", "packet_size", "proof",
                *OPTIONAL_EVIDENCE_KEYS,
                "executor", "domain",
                *PHYSICAL_PROOF_KEYS,
            })
            _check_allowed(fields, name, allowed)
            _validate_marker_context(fields, name, run_fields)
            current = _cell_from(fields, target=target, repeat=repeat, run_id=run_id,
                                  workload=workload, marker=name, cells=cells,
                                  matrix=run_matrix)
            observe_matrix_cell(current, name)
            if current.latency_status is not None:
                raise BaselineError(f"duplicate latency for cell {current.cell}")
            status = _parse_status(fields, name)
            sample_count = _integer(fields, "samples", name, allow_unsupported=status == "unsupported")
            metrics: dict[str, int | None] = {}
            for metric in ("wall_p50_ns", "wall_p99_ns", "cpu_p50_ns", "cpu_p99_ns"):
                metrics[metric] = _integer(fields, metric, name, allow_unsupported=status == "unsupported")
            if status == "ok":
                if sample_count is None or sample_count <= 0 or any(value is None for value in metrics.values()):
                    raise BaselineError(f"{name} successful latency has unsupported values")
                if metrics["wall_p99_ns"] < metrics["wall_p50_ns"] or metrics["cpu_p99_ns"] < metrics["cpu_p50_ns"]:
                    raise BaselineError(f"{name} p99 is below p50")
                if (
                    workload == "io-uring-physical"
                    and target == "thekernel"
                    and current.window_samples is not None
                    and sample_count != current.window_samples
                ):
                    raise BaselineError(
                        f"{name} sample count does not match the physical measurement window"
                    )
            _validate_physical_counter_fields(
                fields,
                name,
                target=target,
                workload=workload,
                qd=current.qd,
                size=current.size,
                status=status,
                batches=(current.window_warmup or 0) + (current.window_samples or 0)
                if status == "ok" else None,
            )
            _replace_cell(cells, current, latency_status=status,
                          latency_samples=sample_count, **metrics)
            continue

    # A helper may publish path/PMU evidence once on TKPERF_RUN instead of
    # repeating it on every cell.  Carry that optional evidence into the raw
    # sample rows while letting a cell-specific value take precedence.
    if run_fields is not None:
        run_optional = {
            key: run_fields[key]
            for key in OPTIONAL_EVIDENCE_COLUMNS
            if key in run_fields
        }
        if run_optional:
            for key, cell in tuple(cells.items()):
                merged = tuple(sorted({**run_optional, **dict(cell.attributes)}.items()))
                if merged != cell.attributes:
                    _replace_cell(cells, cell, attributes=merged)

    if run_fields is None:
        return SubsystemRun(target, repeat, "unknown", "", 0, (), done_status, False, error_message or "missing TKPERF_RUN")
    if error_message is not None:
        return SubsystemRun(target, repeat, workload, run_id, expected_cells, tuple(cells.values()),
                            done_status, saw_done, error_message, runner_exit, exit_seen, data_proof)
    if not saw_done:
        return SubsystemRun(target, repeat, workload, run_id, expected_cells, tuple(cells.values()),
                            done_status, False, "missing TKPERF_DONE", runner_exit, exit_seen, data_proof)
    if not exit_seen:
        return SubsystemRun(target, repeat, workload, run_id, expected_cells, tuple(cells.values()),
                            done_status, True, "missing TKPERF_EXIT", runner_exit, False, data_proof)
    if exit_status != 0:
        return SubsystemRun(target, repeat, workload, run_id, expected_cells, tuple(cells.values()),
                            done_status, True, "nonzero TKPERF_EXIT", runner_exit, True, data_proof)
    if workload == "io-uring-physical":
        assert run_matrix is not None
        expected_matrix = {
            (op, size, qd)
            for op in run_matrix[0]
            for size in run_matrix[1]
            for qd in run_matrix[2]
        }
        observed_matrix = set(matrix_cell_names)
        if observed_matrix != expected_matrix:
            return SubsystemRun(
                target,
                repeat,
                workload,
                run_id,
                expected_cells,
                tuple(cells.values()),
                done_status,
                True,
                "matrix coverage mismatch",
                runner_exit,
                exit_seen,
                data_proof,
            )
    if len(cells) != expected_cells:
        return SubsystemRun(target, repeat, workload, run_id, expected_cells, tuple(cells.values()),
                            done_status, True, "cell count mismatch", runner_exit, exit_seen, data_proof)
    if done_unsupported is not None:
        actual_unsupported = sum(
            1
            for cell in cells.values()
            if cell.correctness_status == "unsupported"
            and cell.window_status == "unsupported"
            and cell.latency_status == "unsupported"
        )
        if done_unsupported != actual_unsupported:
            raise BaselineError(
                f"{DONE_MARKER} unsupported count mismatch: "
                f"declared {done_unsupported}, observed {actual_unsupported}"
            )
    if workload == "io-uring-physical" and data_proof is None:
        return SubsystemRun(target, repeat, workload, run_id, expected_cells, tuple(cells.values()),
                            done_status, True, "missing TKPERF_DATA disk proof", runner_exit, exit_seen, None)
    return SubsystemRun(target, repeat, workload, run_id, expected_cells, tuple(cells.values()),
                        done_status, True, None, runner_exit, exit_seen, data_proof)


parse_guest_log = parse_tkperf_log
parse_log = parse_tkperf_log
parse_performance_log = parse_tkperf_log


def nearest_rank(values: Iterable[int], permille: int) -> int:
    ordered = sorted(values)
    if not ordered:
        raise BaselineError("cannot calculate a quantile for zero samples")
    if permille < 0 or permille > 1000:
        raise BaselineError("quantile must be between zero and one thousand permille")
    rank = (len(ordered) * permille + 999) // 1000
    return ordered[min(rank, len(ordered)) - 1]


def stats(values: Iterable[int]) -> dict[str, int]:
    values = tuple(values)
    return {
        "count": len(values),
        "p50_ns": nearest_rank(values, 500),
        "p99_ns": nearest_rank(values, 990),
        "p999_ns": nearest_rank(values, 999),
    }


def eligible_for_stats(
    guest: SubsystemRun, *, runner_returncode: int, pin_valid: bool
) -> bool:
    """Admit a run only after DONE, correctness, runner zero, and valid pin."""

    if (
        runner_returncode != 0
        or guest.runner_status != 0
        or not guest.exit_seen
        or not pin_valid
        or guest.status != "ok"
    ):
        return False
    if guest.done_status != "ok" or not guest.done:
        return False
    if guest.workload == "io-uring-physical" and guest.data_proof is None:
        return False
    formal = guest.samples
    if not formal:
        return False
    # Every declared cell must have a complete correctness/window/latency
    # outcome.  In particular, do not let a failed or partially-emitted cell
    # disappear just because another cell produced a usable sample.
    for cell in guest.cells:
        statuses = (
            cell.correctness_status,
            cell.window_status,
            cell.latency_status,
        )
        # A cell is either a complete measured cell or an explicit capability
        # boundary. Mixed outcomes must not disappear from the formal result.
        if statuses not in {
            ("ok", "ok", "ok"),
            ("unsupported", "unsupported", "unsupported"),
        }:
            return False
        if statuses == ("unsupported", "unsupported", "unsupported"):
            # Linux may report an explicit capability boundary when the
            # host/kernel cannot execute the cell.  A TheKernel physical lane
            # cannot be formal with any missing physical-DMA cell, including
            # QD8/32; generic virtio capability text is not an oracle.
            if guest.workload == "io-uring-physical" and guest.target == "thekernel":
                return False
            if cell.proof_policy_reason is not None:
                return False
        elif cell.formal_policy_reason is not None:
            return False
    return True


def _optional_row_fields(samples: Iterable[Sample]) -> dict[str, object]:
    """Keep optional evidence only when a group has one unambiguous value."""

    values_by_key: dict[str, set[str]] = {key: set() for key in OPTIONAL_EVIDENCE_COLUMNS}
    for sample in samples:
        attrs = dict(sample.attributes)
        for key in OPTIONAL_EVIDENCE_COLUMNS:
            value = attrs.get(key)
            if value is not None and value != "":
                values_by_key[key].add(value)
    return {
        key: next(iter(values)) if len(values) == 1 else ""
        for key, values in values_by_key.items()
    }


def _derived_throughput(values: Sequence[Sample]) -> int | str:
    """Expose a conservative serial seccomp throughput companion metric.

    ``TKPERF_LATENCY`` measures one syscall per timing sample.  Its p50 can
    therefore provide a useful serial ops/s view for the seccomp RMW/JIT cost,
    but it is not a concurrent throughput claim; other workloads keep this
    field empty until their helper emits a real throughput oracle.
    """

    if not values or values[0].workload != "seccomp" or values[0].wall_p50_ns <= 0:
        return ""
    return max(1, 1_000_000_000 // values[0].wall_p50_ns)


def summarize_samples(samples: Iterable[Sample]) -> list[dict[str, object]]:
    """Summarize each repeat/cell independently; never pool repeats."""

    grouped: dict[tuple[str, int, str, str, str, str, int | None, int | None], list[Sample]] = defaultdict(list)
    for sample in samples:
        grouped[(
            sample.target, sample.repeat, sample.workload, sample.run_id,
            sample.cell, sample.op or "", sample.size, sample.qd,
        )].append(sample)
    rows: list[dict[str, object]] = []
    for key in sorted(grouped):
        values = grouped[key]
        first = values[0]
        optional = _optional_row_fields(values)
        optional["throughput_ops_per_sec"] = _derived_throughput(values)
        rows.append({
            "schema": SCHEMA, "target": first.target, "repeat": first.repeat,
            "workload": first.workload, "cell": first.cell, "op": first.op or "",
            "size": "" if first.size is None else first.size,
            "qd": "" if first.qd is None else first.qd,
            "status": "ok",
            "window_warmup": first.window_warmup,
            "window_samples": first.window_samples,
            "latency_samples": first.latency_samples,
            "wall_p50_ns": first.wall_p50_ns,
            "wall_p99_ns": first.wall_p99_ns,
            "cpu_p50_ns": first.cpu_p50_ns,
            "cpu_p99_ns": first.cpu_p99_ns,
            **optional,
        })
    return rows


aggregate_samples = summarize_samples


def _read_raw(path: Path) -> tuple[Sample, ...]:
    try:
        with path.open(encoding="utf-8", newline="") as stream:
            reader = csv.DictReader(stream, delimiter="\t")
            if tuple(reader.fieldnames or ()) != RAW_COLUMNS:
                raise BaselineError(f"raw sample header mismatch: {path}")
            samples: list[Sample] = []
            for line_number, row in enumerate(reader, 2):
                try:
                    if row.get("schema") != SCHEMA:
                        raise ValueError("schema")
                    def optional_int(name: str) -> int | None:
                        value = row[name]
                        return None if value == "" else int(value)
                    size = optional_int("size")
                    qd = optional_int("qd")
                    sample = Sample(
                        row["target"], int(row["repeat"]), row["workload"], row["run_id"],
                        row["cell"], row["op"] or None, size, qd,
                        int(row["window_warmup"]), int(row["window_samples"]),
                        int(row["latency_samples"]), int(row["wall_p50_ns"]),
                        int(row["wall_p99_ns"]), int(row["cpu_p50_ns"]),
                        int(row["cpu_p99_ns"]),
                        tuple(
                            (key, row[key])
                            for key in OPTIONAL_EVIDENCE_COLUMNS
                            if row.get(key, "") != ""
                        ),
                    )
                    _validate_optional_evidence(
                        dict(sample.attributes), f"raw sample row {line_number}"
                    )
                    if sample.repeat <= 0 or sample.latency_samples <= 0:
                        raise ValueError("range")
                    samples.append(sample)
                except (KeyError, ValueError) as error:
                    raise BaselineError(f"invalid raw sample row {line_number}: {error}") from error
    except OSError as error:
        raise BaselineError(f"cannot read raw samples: {error}") from error
    return tuple(samples)


def _write_tsv(path: Path, columns: Sequence[str], rows: Iterable[Mapping[str, object]]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.tmp")
    with temporary.open("w", encoding="utf-8", newline="") as stream:
        writer = csv.DictWriter(stream, fieldnames=tuple(columns), delimiter="\t", lineterminator="\n")
        writer.writeheader()
        writer.writerows(rows)
    temporary.replace(path)


def stats_command(input_path: Path, output_path: Path, summary_tsv: Path | None = None) -> int:
    samples = _read_raw(input_path)
    runs = summarize_samples(samples)
    output_path.parent.mkdir(parents=True, exist_ok=True)
    temporary = output_path.with_name(f".{output_path.name}.tmp")
    temporary.write_text(
        json.dumps({"schema": SCHEMA, "raw_sample_count": len(samples), "runs": runs},
                   indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    temporary.replace(output_path)
    if summary_tsv is not None:
        _write_tsv(summary_tsv, SUMMARY_COLUMNS, runs)
    return 0


# ---- host CPU topology -------------------------------------------------


def _parse_cpu_list(value: str) -> tuple[int, ...]:
    if not value:
        return ()
    cpus: list[int] = []
    for item in value.split(","):
        bounds = item.split("-")
        if len(bounds) == 1:
            bounds.append(bounds[0])
        if len(bounds) != 2 or any(not bound.isdecimal() for bound in bounds):
            raise BaselineError(f"invalid CPU list: {value!r}")
        first, last = (int(bound, 10) for bound in bounds)
        if first > last:
            raise BaselineError(f"invalid CPU list: {value!r}")
        cpus.extend(range(first, last + 1))
    if len(set(cpus)) != len(cpus):
        raise BaselineError(f"CPU list contains duplicates: {value!r}")
    return tuple(sorted(cpus))


def _read_text(path: Path, default: str) -> str:
    try:
        return path.read_text(encoding="ascii").strip() or default
    except OSError:
        return default


def _read_required_text(path: Path, label: str) -> str:
    """Read topology facts that cannot safely fall back to a singleton."""

    try:
        value = path.read_text(encoding="ascii").strip()
    except (OSError, UnicodeDecodeError) as error:
        raise TopologyUnavailable(f"{label} is unavailable: {path}: {error}") from error
    if not value:
        raise TopologyUnavailable(f"{label} is empty: {path}")
    return value


@dataclass(frozen=True)
class HostCpu:
    cpu: int
    siblings: frozenset[int]
    package: str
    core: str
    performance_class: str
    # These fields make automatic selection conservative on hybrid hosts.
    # They default to unknown so synthetic/test topologies built with the
    # original five-field constructor remain fail-closed and comparable.
    core_type: str = "unknown"
    cache_class: str = "unknown"
    max_freq_khz: int | None = None


@dataclass(frozen=True)
class HostTopology:
    cpus: tuple[HostCpu, ...]

    @property
    def by_cpu(self) -> dict[int, HostCpu]:
        return {cpu.cpu: cpu for cpu in self.cpus}

    @property
    def online(self) -> tuple[int, ...]:
        return tuple(cpu.cpu for cpu in self.cpus)


def _validate_sibling_equivalence(topology: HostTopology) -> None:
    """Require reciprocal, transitive SMT sibling classes for online CPUs."""

    if not topology.cpus:
        raise TopologyUnavailable("host CPU topology has no online CPUs")
    if len({record.cpu for record in topology.cpus}) != len(topology.cpus):
        raise TopologyUnavailable("host CPU topology contains duplicate CPU records")
    by_cpu = topology.by_cpu
    online = set(by_cpu)
    for record in topology.cpus:
        siblings = set(record.siblings)
        if not siblings or record.cpu not in siblings:
            raise TopologyUnavailable(
                f"SMT sibling class for CPU {record.cpu} is empty or omits the CPU"
            )
        missing = siblings - online
        if missing:
            raise TopologyUnavailable(
                f"SMT sibling class for CPU {record.cpu} references offline/unknown CPUs "
                f"{sorted(missing)}"
            )
        for sibling in siblings:
            if record.cpu not in by_cpu[sibling].siblings:
                raise TopologyUnavailable(
                    f"SMT sibling topology is not reciprocal between CPUs "
                    f"{record.cpu} and {sibling}"
                )
        closure = set().union(*(set(by_cpu[sibling].siblings) for sibling in siblings))
        if closure != siblings:
            raise TopologyUnavailable(
                f"SMT sibling topology is not an equivalence class for CPU {record.cpu}"
            )


def _cache_class(cpu_root: Path, cpu: int) -> str:
    """Return a cache-shape signature, excluding cache instance identity."""

    entries: list[str] = []
    for cache in sorted((cpu_root / f"cpu{cpu}" / "cache").glob("index*")):
        level = _read_text(cache / "level", "")
        cache_type = _read_text(cache / "type", "")
        if level not in {"1", "2"}:
            continue
        if level == "1" and cache_type not in {"Data", "Instruction"}:
            continue
        if level == "2" and cache_type != "Unified":
            continue
        size = _read_text(cache / "size", "unknown")
        shared = _read_text(cache / "shared_cpu_list", "")
        try:
            shared_count = len(_parse_cpu_list(shared))
        except BaselineError:
            shared_count = 0
        entries.append(f"L{level}:{cache_type}:{size}:shared={shared_count}")
    return ";".join(entries) or "unknown"


def _max_freq_khz(cpu_root: Path, cpu: int) -> int | None:
    for name in ("cpuinfo_max_freq", "scaling_max_freq"):
        value = _read_text(cpu_root / f"cpu{cpu}" / "cpufreq" / name, "")
        if value.isdecimal() and int(value) > 0:
            return int(value, 10)
    return None


def _selection_class_map(topology: HostTopology) -> dict[int, str]:
    """Group CPUs by core/cache family and near-equal max-frequency class.

    Firmware often reports small per-core turbo differences (for example
    4.8GHz on CPU0 and 4.7GHz on CPUs1-3) even though those cores are the same
    P-core/cache class.  A five-percent band groups that case while keeping a
    3.7GHz E-core and 3.3GHz LP-E class separate.  If no cache/frequency
    evidence exists, retain the older capacity class and fail closed.
    """

    families: dict[tuple[str, str, str], list[HostCpu]] = defaultdict(list)
    for record in topology.cpus:
        if record.core_type == "unknown" and record.cache_class == "unknown":
            family = ("legacy", record.performance_class, "")
        else:
            family = (record.core_type, record.cache_class, "")
        families[family].append(record)

    result: dict[int, str] = {}
    for family, records in families.items():
        representatives: list[int | None] = []
        for record in sorted(records, key=lambda item: item.cpu):
            frequency = record.max_freq_khz
            group_index: int | None = None
            if frequency is not None:
                for index, representative in enumerate(representatives):
                    if representative is None:
                        continue
                    high = max(frequency, representative)
                    low = min(frequency, representative)
                    if high and (high - low) / high <= 0.05:
                        group_index = index
                        break
            if group_index is None:
                if frequency is None:
                    # Missing frequency is not proof that every member of the
                    # family is equal.  Use the reported capacity string.
                    key = f"{family[0]}|{family[1]}|capacity={record.performance_class}"
                    result[record.cpu] = key
                    continue
                representatives.append(frequency)
                group_index = len(representatives) - 1
            result[record.cpu] = (
                f"{family[0]}|{family[1]}|freq-class={group_index}"
            )
    return result


def host_topology_manifest(topology: HostTopology) -> list[dict[str, object]]:
    """Serialize the evidence used for automatic CPU-class selection."""

    classes = _selection_class_map(topology)
    return [
        {
            "cpu": record.cpu,
            "siblings": sorted(record.siblings),
            "package": record.package,
            "core": record.core,
            "performance_class": record.performance_class,
            "core_type": record.core_type,
            "cache_class": record.cache_class,
            "max_freq_khz": record.max_freq_khz,
            "selection_class": classes[record.cpu],
        }
        for record in topology.cpus
    ]


def read_host_topology(cpu_root: Path = Path("/sys/devices/system/cpu")) -> HostTopology:
    online_text = _read_text(cpu_root / "online", "")
    if online_text:
        try:
            online = _parse_cpu_list(online_text)
        except BaselineError as error:
            raise TopologyUnavailable(f"online CPU topology is invalid: {error}") from error
    else:
        online = tuple(
            int(path.name.removeprefix("cpu"))
            for path in cpu_root.glob("cpu[0-9]*")
            if path.name.removeprefix("cpu").isdecimal()
        )
    records: list[HostCpu] = []
    for cpu in sorted(set(online)):
        root = cpu_root / f"cpu{cpu}"
        topology = root / "topology"
        siblings_text = _read_required_text(
            topology / "thread_siblings_list",
            f"thread_siblings_list for CPU {cpu}",
        )
        try:
            siblings = _parse_cpu_list(siblings_text)
        except BaselineError as error:
            raise TopologyUnavailable(
                f"thread_siblings_list for CPU {cpu} is invalid: {error}"
            ) from error
        if not siblings or cpu not in siblings:
            raise TopologyUnavailable(
                f"thread_siblings_list for CPU {cpu} omits the CPU or is invalid"
            )
        siblings_set = frozenset(siblings)
        package = _read_text(topology / "physical_package_id", "unknown")
        core = _read_text(topology / "core_id", str(cpu))
        # cpu_capacity is the best heterogeneous-CPU discriminator.  On
        # machines without it, max frequency is a conservative fallback; a
        # missing value is deliberately not treated as proof of homogeneity.
        performance = _read_text(root / "cpu_capacity", "")
        if not performance:
            performance = _read_text(root / "cpufreq" / "cpuinfo_max_freq", "")
        if not performance:
            performance = "unknown"
        core_type = _read_text(topology / "core_type", "")
        cache_class = _cache_class(cpu_root, cpu)
        if not core_type:
            # Cache shape is a stable proxy for P/E-family identity on x86
            # hybrid systems when Linux does not expose topology/core_type.
            core_type = cache_class
        records.append(
            HostCpu(
                cpu,
                siblings_set,
                package,
                core,
                performance,
                core_type,
                cache_class,
                _max_freq_khz(cpu_root, cpu),
            )
        )
    if not records:
        raise TopologyUnavailable(f"host CPU topology is unavailable below {cpu_root}")
    topology = HostTopology(tuple(records))
    _validate_sibling_equivalence(topology)
    return topology


def validate_cpu_selection(
    cpus: Iterable[int],
    topology: HostTopology,
    *,
    allow_heterogeneous: bool = False,
) -> tuple[int, ...]:
    _validate_sibling_equivalence(topology)
    selected = tuple(cpus)
    if not selected or len(set(selected)) != len(selected):
        raise BaselineError("CPU selection must be non-empty and contain no duplicates")
    by_cpu = topology.by_cpu
    if not set(selected).issubset(by_cpu):
        raise BaselineError(f"CPU selection is outside online topology: {sorted(set(selected) - set(by_cpu))}")
    for index, cpu in enumerate(selected):
        if set(selected[index + 1 :]) & set(by_cpu[cpu].siblings):
            raise BaselineError(f"CPU selection contains SMT siblings around CPU {cpu}")
    classes_by_cpu = _selection_class_map(topology)
    classes = {classes_by_cpu[cpu] for cpu in selected}
    if len(classes) > 1 and not allow_heterogeneous:
        raise BaselineError(
            "CPU selection mixes heterogeneous performance/core-cache/frequency classes; "
            "pass explicit CPUs to override"
        )
    return tuple(sorted(selected))


def validate_cpu_roles(
    roles: Mapping[str, Iterable[int]], topology: HostTopology
) -> dict[str, tuple[int, ...]]:
    """Validate role-local and physical-core disjointness.

    A logical CPU list can look disjoint while still selecting both SMT
    siblings of one physical core.  That is especially easy to do when the
    vCPU and iothread/backend lists are supplied independently.  Validate
    each role using the normal homogeneous-class policy, then compare the
    complete sibling closure across roles.  The returned normalized lists
    make it convenient for callers to retain the exact declaration in a
    manifest.
    """

    normalized: dict[str, tuple[int, ...]] = {}
    physical: dict[str, set[int]] = {}
    for role, cpus in roles.items():
        selected = validate_cpu_selection(cpus, topology)
        normalized[role] = selected
        physical[role] = _exclude_siblings(selected, topology)
    role_names = tuple(normalized)
    for index, left in enumerate(role_names):
        for right in role_names[index + 1 :]:
            overlap = physical[left] & physical[right]
            if overlap:
                raise BaselineError(
                    "CPU roles share a physical core/SMT sibling: "
                    f"{left} and {right} (logical CPUs {sorted(overlap)})"
                )
    return normalized


def select_host_cpus(
    count: int,
    *,
    topology: HostTopology | None = None,
    cpu_root: Path = Path("/sys/devices/system/cpu"),
    explicit: str | Iterable[int] | None = None,
    allowed: Iterable[int] | None = None,
) -> tuple[int, ...]:
    """Select one thread per core and one homogeneous class by default.

    An explicit selection may mix performance classes (the caller has stated
    that intent), but SMT siblings are always rejected.  Automatic selection
    never mixes either class or sibling threads.
    """

    if count <= 0:
        raise BaselineError("CPU count must be positive")
    topology = topology or read_host_topology(cpu_root)
    _validate_sibling_equivalence(topology)
    allowed_set = set(topology.online if allowed is None else allowed)
    if explicit is not None:
        chosen = _parse_cpu_list(explicit) if isinstance(explicit, str) else tuple(explicit)
        if len(chosen) != count:
            raise BaselineError(f"explicit CPU selection requires exactly {count} CPUs")
        if not set(chosen).issubset(allowed_set):
            raise BaselineError(
                f"explicit CPU selection is outside allowed affinity: {sorted(set(chosen) - allowed_set)}"
            )
        return validate_cpu_selection(chosen, topology, allow_heterogeneous=True)
    candidates = [cpu for cpu in topology.cpus if cpu.cpu in allowed_set]
    class_keys = _selection_class_map(topology)
    by_class: dict[str, list[HostCpu]] = defaultdict(list)
    for cpu in candidates:
        by_class[class_keys[cpu.cpu]].append(cpu)
    def class_score(value: str) -> tuple[int, int, str]:
        records = by_class[value]
        frequencies = [record.max_freq_khz for record in records if record.max_freq_khz]
        capacities = [int(record.performance_class) for record in records
                      if record.performance_class.isdecimal()]
        # Prefer the fastest evidenced homogeneous class, then the highest
        # capacity class.  This prevents a hybrid host from silently choosing
        # LP-E CPUs merely because their numeric class sorts first.
        return (
            max(frequencies, default=-1),
            max(capacities, default=-1),
            value,
        )
    for selection_class in sorted(by_class, key=class_score, reverse=True):
        if selection_class.startswith("legacy|unknown|"):
            # Missing capacity/frequency evidence is not a homogeneous-host
            # proof.  The caller can still provide an explicit CPU list.
            continue
        chosen: list[int] = []
        sibling_seen: set[int] = set()
        for record in by_class[selection_class]:
            if record.siblings & sibling_seen:
                continue
            chosen.append(record.cpu)
            sibling_seen.update(record.siblings)
            if len(chosen) == count:
                return validate_cpu_selection(chosen, topology)
    raise BaselineError(
        f"cannot select {count} homogeneous core/cache/frequency CPUs; pass an explicit CPU list"
    )


def _exclude_siblings(cpus: Iterable[int], topology: HostTopology) -> set[int]:
    by_cpu = topology.by_cpu
    excluded: set[int] = set()
    for cpu in cpus:
        record = by_cpu.get(cpu)
        if record is not None:
            excluded.update(record.siblings)
    return excluded


def _measurement_class(cpus: Iterable[int], topology: HostTopology) -> str:
    class_keys = _selection_class_map(topology)
    classes = {class_keys[cpu] for cpu in cpus}
    if len(classes) != 1:
        raise BaselineError(
            "measurement CPU selection mixes heterogeneous core/cache/frequency classes"
        )
    # An explicit CPU list is still the caller's declaration when firmware
    # does not expose capacity/frequency evidence; the class is recorded in
    # the manifest so that comparison consumers can make that choice visible.
    return next(iter(classes))


def _housekeeping_selection(
    explicit: str | None,
    *,
    allowed: set[int],
    measurement: set[int],
    topology: HostTopology,
) -> tuple[int, ...]:
    """Choose housekeeping CPUs outside every measurement physical core."""

    _validate_sibling_equivalence(topology)

    # A housekeeping thread on the other SMT sibling still competes for the
    # same execution resources and cache state as a measurement vCPU.  The
    # formal lane therefore excludes the full sibling closure, including for
    # explicit housekeeping requests.
    measurement_physical = _exclude_siblings(measurement, topology)

    if explicit is not None:
        chosen = _parse_cpu_list(explicit)
        if not chosen:
            raise BaselineError("housekeeping CPU selection must not be empty")
    else:
        # Housekeeping is disjoint by physical core.  Prefer the slowest
        # evidenced homogeneous class, keeping QEMU main/unknown work away
        # from the measured P/E class.  On the current hybrid host this is
        # CPUs 12-15; an explicit --housekeeping-cpus remains authoritative.
        candidates = set(allowed) - measurement_physical
        if candidates:
            classes = _selection_class_map(topology)
            by_class: dict[str, list[int]] = defaultdict(list)
            for cpu in sorted(candidates):
                by_class[classes[cpu]].append(cpu)
            def housekeeping_score(selection_class: str) -> tuple[int, int, str]:
                records = [topology.by_cpu[cpu] for cpu in by_class[selection_class]]
                frequencies = [record.max_freq_khz for record in records if record.max_freq_khz]
                capacities = [int(record.performance_class) for record in records
                              if record.performance_class.isdecimal()]
                return (
                    min(frequencies, default=10**12),
                    min(capacities, default=10**12),
                    selection_class,
                )
            chosen = tuple(sorted(by_class[min(by_class, key=housekeeping_score)]))
        else:
            chosen = ()
    if not chosen:
        raise BaselineError(
            "no housekeeping CPU remains outside measurement CPUs or their physical-core siblings"
        )
    if not set(chosen).issubset(allowed):
        raise BaselineError(
            f"housekeeping CPU selection is outside allowed affinity: {sorted(set(chosen) - allowed)}"
        )
    if set(chosen) & measurement_physical:
        raise BaselineError(
            "housekeeping CPUs overlap measurement CPUs or their physical-core siblings"
        )
    return tuple(chosen)


# Friendly aliases for callers that call the policy a topology resolver.
choose_host_cpus = select_host_cpus
select_cpus = select_host_cpus
get_host_topology = read_host_topology
parse_cpu_list = _parse_cpu_list
choose_housekeeping_cpus = _housekeeping_selection
validate_cpu_role_selection = validate_cpu_roles


def _pin_external_identity(
    value: object,
) -> tuple[int, int, str, int] | None:
    if not isinstance(value, Mapping) or set(value) != PIN_EXTERNAL_IDENTITY_KEYS:
        return None
    pid = value.get("pid")
    tgid = value.get("tgid")
    executable = value.get("exe")
    starttime = value.get("starttime")
    if not isinstance(executable, str) or not executable:
        return None
    try:
        absolute_executable = Path(executable).is_absolute()
    except (OSError, RuntimeError, ValueError, TypeError):
        return None
    if (
        isinstance(pid, bool)
        or not isinstance(pid, int)
        or pid <= 0
        or isinstance(tgid, bool)
        or not isinstance(tgid, int)
        or tgid <= 0
        or not absolute_executable
        or isinstance(starttime, bool)
        or not isinstance(starttime, int)
        or starttime <= 0
    ):
        return None
    try:
        canonical_executable = str(Path(executable).resolve())
    except (OSError, RuntimeError, ValueError, TypeError):
        return None
    return pid, tgid, canonical_executable, starttime


def _pin_affinity(value: object) -> bool:
    return (
        isinstance(value, list)
        and bool(value)
        and all(
            isinstance(cpu, int) and not isinstance(cpu, bool) and cpu >= 0
            for cpu in value
        )
    )


def _pin_report_valid(
    path: Path,
    *,
    expected_vcpu_count: int,
    vcpu_cpus: tuple[int, ...],
    io_cpus: tuple[int, ...],
    backend_cpus: tuple[int, ...],
    expected_external_backends: tuple[Mapping[str, object], ...] = (),
) -> bool:
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError):
        return False
    if not isinstance(payload, dict) or payload.get("schema") != "thekernel-kvm-thread-pinning-v4":
        return False
    declared_external_backends = payload.get("declared_external_backends")
    if not isinstance(declared_external_backends, list):
        return False
    expected_identity_tuples: set[tuple[int, int, str, int]] = set()
    for expected in expected_external_backends:
        identity = _pin_external_identity(expected)
        if identity is None or identity[0] in {item[0] for item in expected_identity_tuples}:
            return False
        expected_identity_tuples.add(identity)
    declared_identity_tuples: set[tuple[int, int, str, int]] = set()
    for declaration in declared_external_backends:
        identity = _pin_external_identity(declaration)
        if identity is None or identity[0] in {item[0] for item in declared_identity_tuples}:
            return False
        declared_identity_tuples.add(identity)
    if declared_identity_tuples != expected_identity_tuples:
        return False
    external_processes = payload.get("external_processes")
    if not isinstance(external_processes, list):
        return False
    measurement = set(vcpu_cpus) | set(io_cpus) | set(backend_cpus)
    if payload.get("expected_vcpu_count") != expected_vcpu_count:
        return False
    if payload.get("requested_vcpu_cpus") != list(vcpu_cpus):
        return False
    if payload.get("requested_io_cpus") != list(io_cpus):
        return False
    if payload.get("requested_backend_cpus") != list(backend_cpus):
        return False
    if payload.get("measurement_cpus") != sorted(measurement):
        return False
    report_smt_siblings = payload.get("measurement_smt_siblings")
    if (
        not isinstance(report_smt_siblings, list)
        or any(isinstance(cpu, bool) or not isinstance(cpu, int) for cpu in report_smt_siblings)
        or not measurement.issubset(set(report_smt_siblings))
    ):
        return False
    housekeeping = payload.get("housekeeping_cpus")
    if (
        not isinstance(housekeeping, list)
        or not housekeeping
        or any(not isinstance(cpu, int) for cpu in housekeeping)
        or set(housekeeping) & set(report_smt_siblings)
    ):
        return False
    if payload.get("unknown_off_measurement") is not True or payload.get("unknown_status") != "ok":
        return False
    if payload.get("proof_failures") != []:
        return False
    if (
        payload.get("ptrace_clone_events") is not True
        or payload.get("unknown_thread_proof") != "ptrace-clone-event"
        or isinstance(payload.get("clone_event_count"), bool)
        or not isinstance(payload.get("clone_event_count"), int)
        or payload.get("clone_event_count", 0) <= 0
        or payload.get("exit_readback_proof") is not True
    ):
        return False
    exit_readback_tids = payload.get("exit_readback_tids")
    if (
        not isinstance(exit_readback_tids, list)
        or not exit_readback_tids
        or any(
            isinstance(tid, bool) or not isinstance(tid, int) or tid <= 0
            for tid in exit_readback_tids
        )
        or exit_readback_tids != sorted(set(exit_readback_tids))
    ):
        return False
    if (
        payload.get("launcher_affinity") != housekeeping
        or payload.get("process_inherited_housekeeping") is not True
        or payload.get("new_threads_inherit_housekeeping") is not True
    ):
        return False
    if payload.get("qemu_main_status") != "ok" or not isinstance(payload.get("qemu_main"), dict):
        return False
    if payload.get("housekeeping_status") != "ok":
        return False
    if payload.get("vcpu_status") != "ok" or payload.get("io_status") != "ok":
        return False
    if payload.get("backend_status") != ("ok" if backend_cpus else "not_requested"):
        return False
    vcpu_threads = payload.get("vcpu_threads")
    if not isinstance(vcpu_threads, dict):
        return False
    if set(vcpu_threads) != {str(index) for index in range(expected_vcpu_count)}:
        return False
    used_tids: set[int] = set()
    # External process leaders cannot alias a QEMU vCPU/IO/unknown role or an
    # internal QEMU backend.  Only an exact declared external backend leader
    # may occupy an external backend record.
    external_collision_tids: set[int] = set()
    external_backend_tids: set[int] = set()
    affinity_by_tid: dict[int, tuple[int, ...]] = {}
    for index in range(expected_vcpu_count):
        record = vcpu_threads[str(index)]
        if not isinstance(record, dict) or set(record) != {
            "tid", "name", "affinity", "tgid"
        }:
            return False
        tid = record.get("tid")
        affinity = record.get("affinity")
        if (
            isinstance(tid, bool)
            or not isinstance(tid, int)
            or tid <= 0
            or tid in used_tids
            or affinity != [vcpu_cpus[index % len(vcpu_cpus)]]
            or record.get("tgid") != payload.get("pid")
        ):
            return False
        used_tids.add(tid)
        external_collision_tids.add(tid)
        affinity_by_tid[tid] = tuple(affinity)
    for field, requested in (("io_threads", io_cpus), ("backend_threads", backend_cpus)):
        records = payload.get(field)
        if not isinstance(records, list) or (requested and not records):
            return False
        for record in records:
            if not isinstance(record, dict):
                return False
            if field in {"io_threads", "backend_threads"} and set(record) != {
                "tid", "name", "affinity", "tgid"
            }:
                return False
            tid = record.get("tid")
            affinity = record.get("affinity")
            if (
                isinstance(tid, bool)
                or not isinstance(tid, int)
                or tid <= 0
                or tid in used_tids
                or not isinstance(affinity, list)
                or not _pin_affinity(affinity)
                or not set(affinity).issubset(
                    set(requested) if requested else set(housekeeping)
                )
                or (
                    field == "io_threads"
                    and record.get("tgid") != payload.get("pid")
                )
                or (
                    field == "backend_threads"
                    and (
                        isinstance(record.get("tgid"), bool)
                        or not isinstance(record.get("tgid"), int)
                        or record.get("tgid", 0) <= 0
                    )
                )
            ):
                return False
            if field == "backend_threads" and record.get("tgid") != payload.get("pid"):
                if tid != record.get("tgid"):
                    return False
                process = next(
                    (
                        item
                        for item in external_processes
                        if isinstance(item, dict) and item.get("pid") == record.get("tgid")
                    ),
                    None,
                )
                identity = (
                    _pin_external_identity(
                        {
                            "pid": process.get("pid"),
                            "tgid": process.get("tgid"),
                            "exe": process.get("exe"),
                            "starttime": process.get("starttime"),
                        }
                    )
                    if isinstance(process, dict)
                    else None
                )
                if (
                    identity is None
                    or identity not in declared_identity_tuples
                    or process.get("backend_authorized") is not True
                ):
                    return False
            used_tids.add(tid)
            if field == "io_threads":
                external_collision_tids.add(tid)
            elif record.get("tgid") == payload.get("pid"):
                # An internal QEMU backend is still QEMU-owned; an external
                # leader may not alias its TID.
                external_collision_tids.add(tid)
            elif tid == record.get("tgid"):
                # Only a separate external process leader can satisfy an
                # authorized backend identity, subject to the declaration
                # and process-record checks below.
                external_backend_tids.add(tid)
            affinity_by_tid[tid] = tuple(affinity)
    qemu_main = payload.get("qemu_main")
    if not isinstance(qemu_main, dict):
        return False
    main_tid = qemu_main.get("tid")
    main_affinity = qemu_main.get("affinity")
    if (
        isinstance(main_tid, bool)
        or not isinstance(main_tid, int)
        or main_tid <= 0
        or main_tid != payload.get("pid")
        or main_tid in used_tids
            or not _pin_affinity(main_affinity)
            or not set(main_affinity).issubset(set(housekeeping))
    ):
        return False
    used_tids.add(main_tid)
    external_collision_tids.add(main_tid)
    affinity_by_tid[main_tid] = tuple(main_affinity)
    unknown_threads = payload.get("unknown_threads")
    if not isinstance(unknown_threads, list):
        return False
    for record in unknown_threads:
        if not isinstance(record, dict):
            return False
        tid = record.get("tid")
        affinity = record.get("affinity")
        if (
            isinstance(tid, bool)
            or not isinstance(tid, int)
            or tid <= 0
            or tid in used_tids
            or not _pin_affinity(affinity)
            or set(affinity) & measurement
        ):
            return False
        used_tids.add(tid)
        external_collision_tids.add(tid)
        affinity_by_tid[tid] = tuple(affinity)
    external_process_identities: set[tuple[int, int, str, int]] = set()
    external_identity_by_pid: dict[int, tuple[int, int, str, int]] = {}
    for process in external_processes:
        if not isinstance(process, dict) or set(process) != PIN_EXTERNAL_PROCESS_V3_KEYS:
            return False
        pid = process.get("pid")
        identity = _pin_external_identity(
            {
                "pid": pid,
                "tgid": process.get("tgid"),
                "exe": process.get("exe"),
                "starttime": process.get("starttime"),
            }
        )
        if (
            isinstance(pid, bool)
            or not isinstance(pid, int)
            or pid <= 0
            or process.get("main_tid") != pid
            or not isinstance(process.get("name"), str)
            or not process["name"]
            or identity is None
            or identity[0] != pid
            or identity[1] != process.get("tgid")
            or identity[1] != pid
            or pid == payload.get("pid")
            or pid in external_collision_tids
            or not isinstance(process.get("backend_authorized"), bool)
            or not _pin_affinity(process.get("affinity"))
            or tuple(process["affinity"]) != affinity_by_tid.get(pid)
        ):
            return False
        if identity in external_process_identities:
            return False
        if pid in external_identity_by_pid:
            return False
        external_identity_by_pid[pid] = identity
        external_process_identities.add(identity)
        if identity in declared_identity_tuples:
            if process["backend_authorized"] is not True:
                return False
            if pid not in external_backend_tids:
                return False
        elif process["backend_authorized"] is not False:
            return False
    if not expected_identity_tuples.issubset(external_process_identities):
        return False
    if not used_tids.issubset(set(exit_readback_tids)):
        return False
    return True


def pin_report_failure_status(path: Path, pin_valid: bool) -> str:
    """Classify missing placement capability separately from bad evidence."""

    if pin_valid:
        return "ok"
    try:
        exists = path.exists()
    except OSError:
        exists = False
    if not exists:
        return "unsupported"
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except OSError:
        return "unsupported"
    except (UnicodeDecodeError, json.JSONDecodeError):
        return "pinning-error"
    if not isinstance(payload, dict) or payload.get("schema") != "thekernel-kvm-thread-pinning-v4":
        return "pinning-error"
    required = {
        "schema", "pid", "expected_vcpu_count", "requested_vcpu_cpus",
        "requested_io_cpus", "requested_backend_cpus", "housekeeping_cpus",
        "measurement_cpus", "measurement_smt_siblings", "vcpu_threads",
        "io_threads", "backend_threads", "external_processes",
        "declared_external_backends", "qemu_main", "unknown_threads",
        "vcpu_status", "io_status", "backend_status", "qemu_main_status",
        "housekeeping_status", "unknown_status", "unknown_off_measurement",
        "launcher_affinity", "process_inherited_housekeeping",
        "new_threads_inherit_housekeeping", "ptrace_clone_events",
        "clone_event_count", "unknown_thread_proof", "exit_readback_tids",
        "exit_readback_proof", "proof_failures",
    }
    if set(payload) != required:
        return "pinning-error"
    def affinity_shape(record: Mapping[str, object]) -> bool:
        value = record.get("affinity")
        return isinstance(value, list) and all(
            isinstance(cpu, int) and not isinstance(cpu, bool) and cpu >= 0
            for cpu in value
        )
    if not isinstance(payload.get("vcpu_threads"), dict):
        return "pinning-error"
    for field in ("io_threads", "backend_threads", "unknown_threads", "external_processes", "declared_external_backends"):
        if not isinstance(payload.get(field), list):
            return "pinning-error"
    proof_failures = payload.get("proof_failures")
    if not isinstance(proof_failures, list):
        return "pinning-error"
    for failure in proof_failures:
        if (
            not isinstance(failure, dict)
            or not isinstance(failure.get("reason"), str)
            or not failure["reason"]
            or isinstance(failure.get("tid"), bool)
            or not isinstance(failure.get("tid"), int)
            or failure["tid"] <= 0
        ):
            return "pinning-error"
    if proof_failures:
        return "pinning-error"
    for record in payload["vcpu_threads"].values():
        if (
            not isinstance(record, dict)
            or set(record) != {"tid", "name", "affinity", "tgid"}
            or not affinity_shape(record)
        ):
            return "pinning-error"
    for field in ("io_threads", "backend_threads"):
        for record in payload[field]:
            if (
                not isinstance(record, dict)
                or set(record) != {"tid", "name", "affinity", "tgid"}
                or not affinity_shape(record)
            ):
                return "pinning-error"
    for record in payload["unknown_threads"]:
        if (
            not isinstance(record, dict)
            or set(record) != {"tid", "name", "affinity"}
            or not affinity_shape(record)
        ):
            return "pinning-error"
    external_pids: set[int] = set()
    for process in payload["external_processes"]:
        if not isinstance(process, dict) or set(process) != PIN_EXTERNAL_PROCESS_V3_KEYS:
            return "pinning-error"
        identity = _pin_external_identity({
            "pid": process.get("pid"),
            "tgid": process.get("tgid"),
            "exe": process.get("exe"),
            "starttime": process.get("starttime"),
        })
        if (
            identity is None
            or identity[0] in external_pids
            or not affinity_shape(process)
        ):
            return "pinning-error"
        external_pids.add(identity[0])
    declared_pids: set[int] = set()
    for declaration in payload["declared_external_backends"]:
        identity = _pin_external_identity(declaration)
        if identity is None or identity[0] in declared_pids:
            return "pinning-error"
        declared_pids.add(identity[0])
    qemu_main_status = payload.get("qemu_main_status")
    qemu_main = payload.get("qemu_main")
    if qemu_main_status == "not_observed":
        if qemu_main is not None:
            return "pinning-error"
    elif (
        not isinstance(qemu_main, dict)
        or set(qemu_main) != {"tid", "name", "affinity"}
        or not affinity_shape(qemu_main)
    ):
        return "pinning-error"
    exit_readback_tids = payload.get("exit_readback_tids")
    if not isinstance(exit_readback_tids, list):
        return "pinning-error"
    if not exit_readback_tids:
        return "unsupported"
    if any(
        isinstance(tid, bool) or not isinstance(tid, int) or tid <= 0
        for tid in exit_readback_tids
    ) or exit_readback_tids != sorted(set(exit_readback_tids)):
        return "pinning-error"
    if not isinstance(payload.get("measurement_cpus"), list) or not isinstance(payload.get("measurement_smt_siblings"), list):
        return "pinning-error"
    if not isinstance(payload.get("housekeeping_cpus"), list):
        return "pinning-error"
    for field in ("process_inherited_housekeeping", "new_threads_inherit_housekeeping"):
        if not isinstance(payload.get(field), bool):
            return "pinning-error"
    if not isinstance(payload.get("housekeeping_status"), str):
        return "pinning-error"
    if payload.get("ptrace_clone_events") is not True:
        return "unsupported"
    if payload.get("process_inherited_housekeeping") is not True or payload.get("new_threads_inherit_housekeeping") is not True:
        return "unsupported"
    clone_count = payload.get("clone_event_count")
    if isinstance(clone_count, bool) or not isinstance(clone_count, int) or clone_count <= 0:
        # ptrace=true is an affirmative proof claim.  A zero/invalid event
        # count contradicts that claim and is malformed evidence, not an
        # unobservable host capability.
        return "pinning-error"
    if payload.get("unknown_thread_proof") != "ptrace-clone-event":
        return "unsupported"
    if payload.get("exit_readback_proof") is not True:
        return "unsupported"
    if payload.get("qemu_main_status") == "not_observed":
        return "unsupported"
    if payload.get("housekeeping_status") == "not_reported":
        return "unsupported"
    if payload.get("unknown_status") == "unsupported":
        return "unsupported"
    if any(
        isinstance(payload.get(field), str)
        and payload.get(field) in {"not_observed", "unsupported"}
        for field in ("vcpu_status", "io_status", "backend_status")
    ):
        return "unsupported"
    return "pinning-error"


def _build_guest_command(
    args: argparse.Namespace, helper: str, *, guest_args: Sequence[str] | None = None
) -> bytes:
    # The TKPERF helpers are self-describing and intentionally take no
    # workload/iteration flags.  Passing scheduler-style arguments here would
    # run the wrong binary contract and invalidate the run.
    configured_args = list(getattr(args, "guest_args", []) if guest_args is None else guest_args)
    if getattr(args, "subsystem", None) == "io-uring-physical":
        if any(value in {"--data-dir", "--data-device"} for value in configured_args):
            raise BaselineError(
                "io-uring-physical data directory/device are runner-owned and cannot be overridden"
            )
        data_device = str(getattr(args, "data_device", FORMAL_DATA_DEVICE))
        if data_device != FORMAL_DATA_DEVICE:
            raise BaselineError(
                "io-uring-physical formal extra-block data device is fixed to /dev/vdb"
            )
        configured_args.extend(
            (
                "--data-dir", str(getattr(args, "data_dir", "/mnt/thekernel-perf-data")),
                "--data-device", data_device,
            )
        )
    command = [helper, *configured_args]
    shutdown = getattr(args, "shutdown_command", "/bin/busybox poweroff -f")
    lines = [
        "if [ -w /proc/sys/kernel/printk ]; then echo 1 > /proc/sys/kernel/printk; fi",
        # Let already-formatted early IRQ diagnostics drain before the first
        # machine-readable marker. Serial writes from that backlog may
        # otherwise split TKPERF_RUN even though the helper itself succeeds.
        "/bin/busybox sleep 3",
    ]
    if getattr(args, "subsystem", None) == "io-uring-physical":
        data_dir = getattr(args, "data_dir", "/mnt/thekernel-perf-data")
        data_device = str(getattr(args, "data_device", FORMAL_DATA_DEVICE))
        if not data_dir or not data_device:
            raise BaselineError("io-uring-physical requires an explicit data directory and device")
        if data_device != FORMAL_DATA_DEVICE:
            # Keep this check next to the shell construction as well as at
            # argument assembly: callers that pass pre-built guest_args must
            # not be able to redirect the formal helper onto the root disk.
            raise BaselineError(
                "io-uring-physical formal extra-block data device is fixed to /dev/vdb"
            )
        qdir = shlex.quote(str(data_dir))
        qdev = shlex.quote(str(data_device))
        # The helper verifies ext4 and st_dev==st_rdev again.  This setup line
        # makes the independent extra disk explicit in the run artifact.
        lines.extend([
            f"/bin/busybox mkdir -p {qdir}",
            "if ! /bin/busybox mount -t ext4 " + qdev + " " + qdir + "; then echo TKPERF_\"\"ERROR schema="
            + TKPERF_SCHEMA
            + " workload=io-uring-physical stage=data-mount errno=19 reason=extra-disk-mount; "
            # There is no RUN/DONE protocol to close when the required disk
            # could not be mounted.  Leave the error record as the strict
            # parser's terminal evidence instead of emitting an EXIT before
            # the mandatory DONE marker and turning a setup failure into a
            # parser exception.
            + "/bin/busybox sleep 1; "
            + shutdown
            + "; exit 1; fi",
        ])
    lines.extend([
        " ".join(shlex.quote(value) for value in command),
        "rc=$?",
        # The interactive shell echoes input. Split the marker token in the
        # command text so strict parsing sees only the command's output, not
        # a second pre-execution marker embedded in the prompt line.
        "echo TKPERF_\"\"EXIT schema=" + TKPERF_SCHEMA + " status=$rc",
        # Bound the serial drain before a poweroff banner can splice into the
        # final latency marker.  The parser remains strict about completeness.
        "/bin/busybox sleep 1",
        shutdown,
    ])
    return ("\n".join(lines) + "\n").encode("utf-8")


def _packet_guest_args(
    args: argparse.Namespace, *, peer_mac: str, run_id: str | None = None
) -> list[str]:
    """Build the packet helper's explicit host-peer invocation.

    The packet helper's default is an AF_PACKET loopback selftest.  A formal
    lane must never fall back to that mode, so the harness supplies ``--formal``
    and the tap-facing guest interface/peer MAC itself.
    """

    configured = list(getattr(args, "guest_args", []))
    if any(value == "--selftest" or value.startswith("--selftest=") for value in configured):
        raise BaselineError("formal packet lane rejects --selftest")
    for option in ("--interface", "--interface=", "--peer-mac", "--peer-mac="):
        if any(value == option or value.startswith(option) for value in configured):
            raise BaselineError(f"formal packet lane owns {option.rstrip('=')}")
    interface = getattr(args, "packet_interface", "eth0")
    if not interface or any(char.isspace() for char in interface):
        raise BaselineError("formal packet lane requires a valid --packet-interface")
    result = [
        *configured,
        "--formal",
        "--interface",
        interface,
        "--peer-mac",
        peer_mac,
    ]
    if run_id is not None:
        result.extend(("--run-id", run_id))
    return result


def _validate_mac(value: str) -> str:
    parts = value.split(":")
    if len(parts) != 6 or any(len(part) != 2 for part in parts):
        raise BaselineError(f"invalid packet peer MAC: {value!r}")
    try:
        raw = bytes(int(part, 16) for part in parts)
    except ValueError as error:
        raise BaselineError(f"invalid packet peer MAC: {value!r}") from error
    if raw == b"\x00" * 6 or raw[0] & 1:
        raise BaselineError(f"packet peer MAC must be a unicast non-zero address: {value!r}")
    return value.lower()


def _new_run_id(repeat: int) -> str:
    value = time.monotonic_ns() ^ (os.getpid() << 32) ^ repeat
    return f"{value & ((1 << 64) - 1):016x}"


def _data_proof_matches(
    guest: SubsystemRun, *, data_device: str, data_dir: str
) -> bool:
    """Bind the guest's mount proof to this run's requested extra disk.

    The helper proves that the directory is an ext4 mount backed by a block
    device, but a formal harness must also reject a marker for a different
    device or mount point than the command/manifest requested.
    """

    if guest.workload != "io-uring-physical":
        return True
    if data_device != FORMAL_DATA_DEVICE:
        return False
    if guest.data_proof is None:
        return False
    proof = dict(guest.data_proof)
    return (
        proof.get("device") == data_device
        and proof.get("mount") == data_dir
        and proof.get("fs") == "ext4"
        and proof.get("identity") == "verified"
        and proof.get("mapping") == FORMAL_DATA_MAPPING
    )


def _qemu_keyval(value: str) -> tuple[str, dict[str, str]] | None:
    """Parse one QEMU comma-escaped keyval argument structurally.

    QEMU represents a literal comma as ``,,`` (two adjacent commas).  A
    substring check such as ``"id=extra" in command`` can be spliced by a
    different drive, so formal admission uses the same key/value boundaries
    emitted by ``tools.qemu_runner.command``.
    """

    fields: dict[str, str] = {}
    bare: list[str] = []
    item: list[str] = []
    items: list[str] = []
    index = 0
    while index < len(value):
        character = value[index]
        if character == ",":
            if index + 1 < len(value) and value[index + 1] == ",":
                item.append(",")
                index += 2
                continue
            items.append("".join(item))
            item = []
            index += 1
            continue
        item.append(character)
        index += 1
    items.append("".join(item))
    if any(not entry for entry in items):
        return None
    for entry in items:
        key, separator, field_value = entry.partition("=")
        if not separator:
            if not entry or entry in bare:
                return None
            bare.append(entry)
            continue
        if not key or not field_value or key in fields:
            return None
        fields[key] = field_value
    if len(bare) > 1:
        return None
    return (bare[0] if bare else "", fields)


def _qemu_drive_identity(command: Sequence[str]) -> tuple[dict[str, str], ...] | None:
    """Extract QEMU ``-drive``/``-device`` identities without substring tests."""

    drives: list[dict[str, str]] = []
    devices: list[dict[str, str]] = []
    index = 0
    while index < len(command):
        option = command[index]
        if option not in {"-drive", "-device"}:
            index += 1
            continue
        if index + 1 >= len(command) or not isinstance(command[index + 1], str):
            return None
        parsed = _qemu_keyval(command[index + 1])
        if parsed is None:
            return None
        kind, fields = parsed
        if option == "-drive":
            drives.append({"kind": kind, **fields})
        else:
            devices.append({"kind": kind, **fields})
        index += 2
    # The guest's /dev/vda + /dev/vdb proof depends on virtio enumeration, not
    # on the relative order of unrelated ``-drive`` options (pflash, ESP,
    # etc.).  There must be exactly two virtio-blk-pci devices, and their
    # ordered drive identities must be rootfs then extra.  Any preceding,
    # duplicate, missing-drive, or additional virtio block device is
    # ambiguous and therefore rejected.
    virtio_devices = [
        device for device in devices if device.get("kind") == "virtio-blk-pci"
    ]
    if len(virtio_devices) != 2 or [
        device.get("drive") for device in virtio_devices
    ] != ["rootfs", "extra"]:
        return None

    selected: list[dict[str, str]] = []
    for drive_id in ("rootfs", "extra"):
        matches = [drive for drive in drives if drive.get("id") == drive_id]
        if len(matches) != 1:
            return None
        selected.append(matches[0])
    return tuple(selected)


def _receipt_evidence_path(value: object) -> str | None:
    if not isinstance(value, dict):
        return None
    path = value.get("path")
    size = value.get("size_bytes")
    digest = value.get("sha256")
    if (
        not isinstance(path, str)
        or not path
        or path != str(Path(path).expanduser().resolve())
        or type(size) is not int
        or size < 0
        or not isinstance(digest, str)
        or len(digest) != 64
        or any(character not in "0123456789abcdef" for character in digest)
    ):
        return None
    return path


def _extra_block_receipt_matches(
    receipt_path: Path, *, extra_block: Path | None, data_device: str
) -> bool:
    """Verify the exact runner-owned rootfs/extra-drive topology.

    The guest's ``TKPERF_DATA`` record proves the mounted block device with
    ``stat``/``statfs``; it does not, by itself, prove which QEMU image was
    attached.  The receipt therefore binds the source evidence to the
    canonical *runtime* image path in the parsed ``-drive id=extra`` option,
    requires the matching ``virtio-blk-pci,drive=extra`` device, and checks
    the unique rootfs-before-extra enumeration used for /dev/vda + /dev/vdb.
    """

    if data_device != FORMAL_DATA_DEVICE or extra_block is None:
        return False
    try:
        payload = json.loads(receipt_path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError):
        return False
    if not isinstance(payload, dict):
        return False
    source_path = _receipt_evidence_path(payload.get("extra_block_source"))
    if source_path != str(extra_block.expanduser().resolve()):
        return False
    rootfs_runtime = _receipt_evidence_path(payload.get("rootfs_runtime_before"))
    extra_runtime = _receipt_evidence_path(payload.get("extra_block_runtime_before"))
    if rootfs_runtime is None or extra_runtime is None:
        return False
    for key in ("rootfs_runtime_after", "extra_block_runtime_after"):
        after = payload.get(key)
        if after is not None and _receipt_evidence_path(after) not in {
            rootfs_runtime if key.startswith("rootfs") else extra_runtime
        }:
            return False
    command = payload.get("command")
    if not isinstance(command, list) or any(not isinstance(item, str) for item in command):
        return False
    identities = _qemu_drive_identity(command)
    if identities is None:
        return False
    rootfs_drive, extra_drive = identities
    try:
        rootfs_file = rootfs_drive["file"]
        extra_file = extra_drive["file"]
    except KeyError:
        return False
    if (
        rootfs_file != rootfs_runtime
        or extra_file != extra_runtime
        or rootfs_file == extra_file
    ):
        return False
    return (
        rootfs_drive.get("if") == "none"
        and rootfs_drive.get("format") == "raw"
        and extra_drive.get("if") == "none"
        and extra_drive.get("format") == "raw"
    )


@dataclass
class PacketPeerProcess:
    process: subprocess.Popen[bytes]
    stream: object
    log_path: Path
    affinity_path: Path
    run_id: str
    tap_name: str
    peer_mac: str
    requested_cpus: tuple[int, ...]
    readback_cpus: tuple[int, ...]
    ready: dict[str, str] | None = None
    done: dict[str, str] | None = None
    done_error: str | None = None
    returncode: int | None = None
    terminated_by_harness: bool = False


def _peer_fields(line: str, marker: str) -> dict[str, str] | None:
    stripped = line.strip()
    if not stripped.startswith(marker + " "):
        return None
    fields: dict[str, str] = {}
    for token in stripped.split()[1:]:
        key, equal, value = token.partition("=")
        if not equal or not key or not value or key in fields:
            raise BaselineError(f"invalid {marker} record: {stripped!r}")
        fields[key] = value
    return fields


def _peer_records(path: Path) -> tuple[dict[str, str] | None, dict[str, str] | None]:
    try:
        lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
    except OSError:
        return None, None
    ready: dict[str, str] | None = None
    done: dict[str, str] | None = None
    for line in lines:
        stripped = line.strip()
        if stripped.startswith("TKPFNET1_") and not (
            stripped.startswith(PEER_READY_MARKER + " ")
            or stripped.startswith(PEER_DONE_MARKER + " ")
        ):
            raise BaselineError(f"unknown host-peer marker: {stripped!r}")
        if stripped.startswith(PEER_READY_MARKER + " "):
            if ready is not None:
                raise BaselineError("duplicate host-peer READY")
            ready = _peer_fields(stripped, PEER_READY_MARKER)
        elif stripped.startswith(PEER_DONE_MARKER + " "):
            if done is not None:
                raise BaselineError("duplicate host-peer DONE")
            done = _peer_fields(stripped, PEER_DONE_MARKER)
    return ready, done


def _validate_peer_ready(
    fields: Mapping[str, str], *, run_id: str, tap_name: str, peer_mac: str
) -> None:
    if set(fields) != {"schema", "run_id", "interface", "mac", "status"}:
        raise BaselineError("host-peer READY fields are not exact")
    if fields["schema"] != PEER_SCHEMA or fields["run_id"].lower() != run_id:
        raise BaselineError("host-peer READY identity mismatch")
    if fields["interface"] != tap_name or _validate_mac(fields["mac"]) != peer_mac:
        raise BaselineError("host-peer READY interface/MAC mismatch")
    if fields["status"] != "ok":
        raise BaselineError("host-peer READY is not successful")


def _validate_peer_done(fields: Mapping[str, str], *, run_id: str) -> None:
    # The peer's READY record carries the schema identity.  Its completion
    # record is deliberately a compact run result and has no schema/frames
    # aliases: packet_perf_peer.c emits exactly these six fields.
    if set(fields) != {"run_id", "status", "sent", "echoed", "checksum", "errors"}:
        raise BaselineError("host-peer DONE fields are not exact")
    if fields["run_id"].lower() != run_id:
        raise BaselineError("host-peer DONE run identity mismatch")
    if fields["status"] != "ok":
        raise BaselineError("host-peer DONE is not successful")
    for key in ("sent", "echoed", "errors"):
        if not fields[key].isdecimal():
            raise BaselineError(f"host-peer DONE {key} is invalid")
    sent = int(fields["sent"], 10)
    echoed = int(fields["echoed"], 10)
    errors = int(fields["errors"], 10)
    if sent <= 0 or echoed <= 0 or echoed > sent:
        raise BaselineError("host-peer DONE sent/echoed counts are invalid")
    if errors != 0:
        raise BaselineError("host-peer DONE reports errors")
    checksum = fields["checksum"].lower()
    if len(checksum) != 8 or any(char not in "0123456789abcdef" for char in checksum):
        raise BaselineError("host-peer DONE checksum is invalid")


def _wait_packet_peer_ready(peer: PacketPeerProcess, timeout: float) -> None:
    deadline = time.monotonic() + max(0.1, min(timeout, 30.0))
    while time.monotonic() < deadline:
        ready, done = _peer_records(peer.log_path)
        if done is not None:
            raise BaselineError("host peer emitted DONE before READY")
        if ready is not None:
            _validate_peer_ready(
                ready,
                run_id=peer.run_id,
                tap_name=peer.tap_name,
                peer_mac=peer.peer_mac,
            )
            peer.ready = ready
            return
        if peer.process.poll() is not None:
            raise BaselineError("packet host peer exited before READY")
        time.sleep(0.01)
    raise BaselineError("timed out waiting for packet host-peer READY")


def _wait_packet_peer_done(peer: PacketPeerProcess, timeout: float) -> None:
    deadline = time.monotonic() + max(0.1, min(timeout, 30.0))
    while time.monotonic() < deadline:
        ready, done = _peer_records(peer.log_path)
        if ready is None:
            peer.done_error = "host-peer READY disappeared"
            return
        if done is not None:
            _validate_peer_ready(
                ready,
                run_id=peer.run_id,
                tap_name=peer.tap_name,
                peer_mac=peer.peer_mac,
            )
            _validate_peer_done(done, run_id=peer.run_id)
            try:
                peer.readback_cpus = tuple(sorted(os.sched_getaffinity(peer.process.pid)))
            except OSError as error:
                peer.done_error = f"packet host peer affinity readback failed: {error}"
                return
            if peer.readback_cpus != peer.requested_cpus:
                peer.done_error = "packet host peer affinity changed before DONE"
                return
            peer.affinity_path.write_text(
                json.dumps(
                    {
                        "schema": PEER_SCHEMA,
                        "run_id": peer.run_id,
                        "pid": peer.process.pid,
                        "requested_cpus": list(peer.requested_cpus),
                        "readback_cpus": list(peer.readback_cpus),
                        "status": "ok",
                    },
                    sort_keys=True,
                )
                + "\n",
                encoding="utf-8",
            )
            peer.done = done
            return
        if peer.process.poll() is not None:
            peer.done_error = "packet host peer exited before DONE"
            return
        time.sleep(0.01)
    peer.done_error = "timed out waiting for packet host-peer DONE"


def _start_packet_peer(
    command: Sequence[str],
    *,
    run_dir: Path,
    tap_name: str,
    peer_mac: str,
    run_id: str,
    backend_cpus: tuple[int, ...],
    ready_timeout: float = 5.0,
) -> PacketPeerProcess:
    if not command or any(not value for value in command):
        raise BaselineError("packet formal lane requires a non-empty host-peer command")
    if not backend_cpus:
        raise BaselineError("packet formal lane requires a dedicated backend CPU")
    owned_options = ("--interface", "--peer-mac", "--backend-cpu", "--run-id")
    if any(
        value in owned_options or any(value.startswith(option + "=") for option in owned_options)
        for value in command
    ):
        raise BaselineError("packet host-peer command must not pre-bind per-run identity options")
    # The checked-in peer consumes these as real argv values.  Environment
    # variables remain diagnostic context only and cannot accidentally leave a
    # stale interface/MAC/run identity in a repeated lane.
    peer_args = [
        *command,
        "--interface", tap_name,
        "--peer-mac", peer_mac,
        "--backend-cpu", str(backend_cpus[0]),
        "--run-id", run_id,
    ]
    peer_log_path = run_dir / "packet-peer.log"
    peer_log = peer_log_path.open("wb")
    environment = os.environ.copy()
    environment.update(
        {
            "THEKERNEL_PACKET_PEER_TAP": tap_name,
            "THEKERNEL_PACKET_PEER_INTERFACE": tap_name,
            "THEKERNEL_PACKET_PEER_MAC": peer_mac,
            "THEKERNEL_PACKET_PEER_RUN_ID": run_id,
            "THEKERNEL_PACKET_PEER_PROTOCOL": "0x88b7",
            "THEKERNEL_PACKET_PEER_READY_MARKER": PEER_READY_MARKER,
            "THEKERNEL_PACKET_PEER_DONE_MARKER": PEER_DONE_MARKER,
        }
    )
    try:
        process = subprocess.Popen(
            peer_args,
            cwd=run_dir,
            env=environment,
            stdin=subprocess.DEVNULL,
            stdout=peer_log,
            stderr=subprocess.STDOUT,
        )
    except OSError:
        peer_log.close()
        raise
    if process.poll() is not None:
        peer_log.close()
        raise BaselineError("packet host peer exited before the QEMU run")
    try:
        os.sched_setaffinity(process.pid, set(backend_cpus))
        readback_cpus = tuple(sorted(os.sched_getaffinity(process.pid)))
    except OSError as error:
        process.terminate()
        process.wait(timeout=2.0)
        peer_log.close()
        raise BaselineError(f"cannot pin packet host peer: {error}") from error
    if readback_cpus != tuple(sorted(backend_cpus)):
        process.terminate()
        process.wait(timeout=2.0)
        peer_log.close()
        raise BaselineError(
            f"packet host peer affinity readback mismatch: {readback_cpus} != {backend_cpus}"
        )
    affinity_path = run_dir / "packet-peer-affinity.json"
    affinity_path.write_text(
        json.dumps(
            {
                "schema": PEER_SCHEMA,
                "run_id": run_id,
                "pid": process.pid,
                "requested_cpus": list(backend_cpus),
                "readback_cpus": list(readback_cpus),
                "status": "ok",
            },
            sort_keys=True,
        )
        + "\n",
        encoding="utf-8",
    )
    peer = PacketPeerProcess(
        process,
        peer_log,
        peer_log_path,
        affinity_path,
        run_id,
        tap_name,
        peer_mac,
        tuple(backend_cpus),
        readback_cpus,
    )
    try:
        _wait_packet_peer_ready(peer, ready_timeout)
    except BaseException:
        _stop_packet_peer(peer)
        raise
    return peer


def _stop_packet_peer(peer: PacketPeerProcess | None) -> int | None:
    if peer is None:
        return None
    if peer.process.poll() is None:
        peer.terminated_by_harness = True
        peer.process.terminate()
    try:
        peer.returncode = peer.process.wait(timeout=2.0)
    except subprocess.TimeoutExpired:
        peer.process.kill()
        peer.returncode = peer.process.wait(timeout=2.0)
    finally:
        peer.stream.close()
    return peer.returncode


def _helper_for(args: argparse.Namespace) -> str:
    configured = getattr(args, "guest_program", None)
    if configured:
        return configured
    try:
        return PERF_HELPERS[args.subsystem]
    except KeyError as error:
        raise BaselineError(f"no formal TKPERF helper for subsystem {args.subsystem!r}") from error


def _require_file(path: Path | None, label: str) -> Path:
    if path is None:
        raise BaselineError(f"{label} is required")
    resolved = path.expanduser().resolve()
    if not resolved.is_file() or resolved.stat().st_size == 0:
        raise BaselineError(f"{label} is missing or empty: {resolved}")
    return resolved


def _require_ext4_image(path: Path, label: str) -> Path:
    """Check the ext4 superblock before attaching a formal data disk."""

    path = _require_file(path, label)
    try:
        with path.open("rb") as stream:
            stream.seek(1024 + 56)
            magic = stream.read(2)
    except OSError as error:
        raise BaselineError(f"cannot inspect {label}: {error}") from error
    if magic != b"\x53\xef":
        raise BaselineError(f"{label} is not an ext4 image")
    return path


def _target_images(args: argparse.Namespace, target: str) -> TargetImages:
    prefix = "linux_" if target == "linux" else ""
    kernel = _require_file(getattr(args, prefix + "kernel", None), f"{target} kernel")
    rootfs = _require_file(getattr(args, prefix + "rootfs", None), f"{target} rootfs")
    extra: Path | None = getattr(args, prefix + "extra_block", None)
    if args.subsystem == "io-uring-physical":
        if extra is None:
            raise BaselineError(
                f"{target} io-uring-physical formal lane requires --{prefix}extra-block"
            )
        extra = _require_ext4_image(extra, f"{target} extra-block")
        if extra == rootfs or extra.samefile(rootfs):
            raise BaselineError(f"{target} extra-block must be independent from rootfs")
    elif extra is not None:
        extra = _require_file(extra, f"{target} extra-block")
        if extra == rootfs or extra.samefile(rootfs):
            raise BaselineError(f"{target} extra-block must be independent from rootfs")
    if target == "linux":
        return TargetImages(
            kernel=kernel,
            rootfs=rootfs,
            esp=None,
            extra_block=extra,
            direct_kernel=True,
            initrd=(
                _require_file(args.linux_initrd, "linux initrd")
                if getattr(args, "linux_initrd", None) is not None
                else None
            ),
            cmdline=getattr(args, "linux_cmdline", "root=/dev/vda rw console=ttyS0 init=/etc/thekernel/shell-init.sh panic=-1 reboot=t"),
        )
    esp = _require_file(getattr(args, "esp", None), "thekernel ESP")
    return TargetImages(
        kernel=kernel,
        rootfs=rootfs,
        esp=esp,
        extra_block=extra,
        direct_kernel=False,
    )


def run_command(args: argparse.Namespace) -> int:
    if args.subsystem not in PERF_HELPERS:
        raise BaselineError(f"unsupported formal subsystem: {args.subsystem!r}")
    if args.subsystem == "io-uring-physical":
        data_device = str(getattr(args, "data_device", FORMAL_DATA_DEVICE))
        if data_device != FORMAL_DATA_DEVICE:
            raise BaselineError(
                "io-uring-physical formal extra-block data device is fixed to /dev/vdb"
            )
        configured_helper = getattr(args, "guest_program", None)
        if configured_helper is not None and configured_helper != PERF_HELPERS[args.subsystem]:
            raise BaselineError(
                "io-uring-physical formal lane requires the checked-in helper for data proof"
            )
    prearm_kvm_nx_worker = bool(getattr(args, "prearm_kvm_nx_worker", False))
    prearm_kvm_nx_timeout = float(
        getattr(args, "prearm_kvm_nx_timeout", 2.0)
    )
    if prearm_kvm_nx_worker and not (0.0 < prearm_kvm_nx_timeout <= 10.0):
        raise BaselineError(
            "--prearm-kvm-nx-timeout must be in (0, 10] seconds"
        )
    network = getattr(args, "network", "passt")
    if network not in FORMAL_NETWORK_MODES:
        raise BaselineError(
            f"formal subsystem lanes require passt or tap-vhost networking, got {network!r}"
        )
    if prearm_kvm_nx_worker and not (
        args.subsystem == "io-uring-physical" and network == "passt"
    ):
        raise BaselineError(
            "--prearm-kvm-nx-worker is restricted to io-uring-physical passt lanes"
        )
    if network == "tap-vhost":
        # QEMU's kernel vhost workers are host tasks outside the QEMU tgid.
        # The current pinner cannot establish their ownership and read-back
        # affinity for any formal workload, not only packet cells.  Refuse
        # every tap-vhost lane before creating a manifest or starting a guest
        # rather than claiming that QEMU-tgid evidence covers them.
        print(
            "kvm-subsystem-baseline: UNSUPPORTED: tap-vhost vhost worker ownership is not tracked",
            file=sys.stderr,
        )
        return 78
    packet_lane = args.subsystem in {"packet", "network"}
    packet_tap = getattr(args, "tap_name", None)
    packet_peer_mac: str | None = None
    packet_peer_command: Sequence[str] = ()
    if packet_lane:
        if network != "tap-vhost":
            raise BaselineError(
                "formal packet lanes require tap-vhost with an AF_PACKET host peer; passt cannot carry TKPFNET1"
            )
        if not packet_tap:
            raise BaselineError("formal packet lanes require --tap-name")
        configured_mac = getattr(args, "packet_peer_mac", None)
        if not configured_mac:
            raise BaselineError("formal packet lanes require --packet-peer-mac")
        packet_peer_mac = _validate_mac(configured_mac)
        packet_peer_command = tuple(getattr(args, "packet_peer_command", ()) or ())
        if not packet_peer_command:
            raise BaselineError(
                "formal packet lanes require --packet-peer-command; no host peer will be faked"
            )
        if not getattr(args, "backend_cpus", None):
            raise BaselineError(
                "formal packet lanes require an explicit dedicated --backend-cpus peer CPU"
            )
    target_choice = getattr(args, "target", "thekernel")
    if target_choice not in {"thekernel", "linux", "both"}:
        raise BaselineError(f"unsupported subsystem target: {target_choice!r}")
    targets = ("thekernel", "linux") if target_choice == "both" else (target_choice,)
    images_by_target: dict[str, TargetImages] = {}
    unavailable: list[dict[str, object]] = []
    for target in targets:
        try:
            images_by_target[target] = _target_images(args, target)
        except BaselineError as error:
            if target == "linux" and target_choice == "both":
                unavailable.append({"target": target, "status": "unavailable", "reason": str(error)})
                continue
            raise

    if not Path("/dev/kvm").exists():
        print(
            "kvm-subsystem-baseline: UNSUPPORTED: /dev/kvm is unavailable",
            file=sys.stderr,
        )
        return 78
    qemu = shutil.which(args.qemu_binary or "qemu-system-x86_64")
    if qemu is None:
        raise BaselineError("qemu-system-x86_64 is unavailable")
    helper = _helper_for(args)
    output = args.output.expanduser().resolve()
    output.mkdir(parents=True, exist_ok=True)

    topology = read_host_topology()
    allowed = set(os.sched_getaffinity(0)) & set(topology.online)
    vcpu_cpus = select_host_cpus(
        args.cpus, topology=topology, explicit=args.vcpu_cpus, allowed=allowed
    )
    measurement_class = _measurement_class(vcpu_cpus, topology)
    io_allowed = allowed - _exclude_siblings(vcpu_cpus, topology)
    selection_classes = _selection_class_map(topology)
    # I/O and backend workers are intentionally allowed to use a separate
    # homogeneous class from vCPUs.  On the hybrid host this selects P-core
    # vCPUs (0-3) and dedicated E-core workers (4/8), while each role remains
    # class-consistent and excluded from the vCPU sibling set.
    alternate_io = {
        cpu for cpu in io_allowed if selection_classes[cpu] != measurement_class
    }
    if args.io_cpus is None and alternate_io:
        io_allowed = alternate_io
    io_cpus = select_host_cpus(1, topology=topology, explicit=args.io_cpus, allowed=io_allowed)
    io_class = _measurement_class(io_cpus, topology)
    backend_cpus = (
        select_host_cpus(
            1,
            topology=topology,
            explicit=args.backend_cpus,
            allowed={
                cpu for cpu in (
                    allowed - _exclude_siblings(vcpu_cpus, topology)
                    - _exclude_siblings(io_cpus, topology)
                )
                if args.backend_cpus is not None
                or selection_classes[cpu] == measurement_class
            },
        )
        if args.backend_cpus
        else ()
    )
    backend_class = _measurement_class(backend_cpus, topology) if backend_cpus else None
    validate_cpu_roles(
        {
            "vCPU": vcpu_cpus,
            "IO": io_cpus,
            **({"backend": backend_cpus} if backend_cpus else {}),
        },
        topology,
    )
    measurement = set(vcpu_cpus) | set(io_cpus) | set(backend_cpus)
    housekeeping = _housekeeping_selection(
        args.housekeeping_cpus, allowed=allowed, measurement=measurement, topology=topology
    )
    housekeeping_classes = {selection_classes[cpu] for cpu in housekeeping}

    expected_workload = EXPECTED_WORKLOADS[args.subsystem]
    cost_capabilities = host_cost_capabilities()
    raw_samples: list[Sample] = []
    runs: list[dict[str, object]] = [*unavailable]
    for repeat in range(1, args.repeat + 1):
        # Target is intentionally inside the repeat loop to alternate Linux
        # and TheKernel and reduce thermal/time drift in ``both`` runs.
        for target in targets:
            images = images_by_target.get(target)
            if images is None:
                continue
            run_dir = output / target / args.subsystem / f"repeat-{repeat:03d}"
            run_dir.mkdir(parents=True, exist_ok=True)
            commands = run_dir / "commands"
            packet_run_id = _new_run_id(repeat) if packet_lane else None
            guest_args = (
                _packet_guest_args(args, peer_mac=packet_peer_mac, run_id=packet_run_id)
                if packet_lane and packet_peer_mac is not None and packet_run_id is not None
                else None
            )
            commands.write_bytes(_build_guest_command(args, helper, guest_args=guest_args))
            pin_report = run_dir / "thread-pinning.json"
            env_keys = {
                "THEKERNEL_KVM_QEMU": qemu,
                "THEKERNEL_KVM_VCPU_CPUS": ",".join(map(str, vcpu_cpus)),
                "THEKERNEL_KVM_IO_CPUS": ",".join(map(str, io_cpus)),
                "THEKERNEL_KVM_BACKEND_CPUS": ",".join(map(str, backend_cpus)),
                "THEKERNEL_KVM_HOUSEKEEPING_CPUS": ",".join(map(str, housekeeping)),
                "THEKERNEL_KVM_VCPU_COUNT": str(args.cpus),
                "THEKERNEL_KVM_PIN_REPORT": str(pin_report),
                "THEKERNEL_KVM_PREARM_KVM_NX_WORKER": (
                    "1" if prearm_kvm_nx_worker else "0"
                ),
                "THEKERNEL_KVM_PREARM_KVM_NX_TIMEOUT": str(
                    prearm_kvm_nx_timeout
                ),
            }
            previous = {key: os.environ.get(key) for key in env_keys}
            os.environ.update(env_keys)
            peer: PacketPeerProcess | None = None
            peer_returncode: int | None = None
            result = None
            try:
                if packet_lane and packet_peer_mac is not None and packet_tap is not None:
                    peer = _start_packet_peer(
                        packet_peer_command,
                        run_dir=run_dir,
                        tap_name=packet_tap,
                        peer_mac=packet_peer_mac,
                        run_id=packet_run_id or "",
                        backend_cpus=backend_cpus,
                        ready_timeout=getattr(args, "peer_ready_timeout", 5.0),
                    )
                extra_args: list[str] = ["-name", "guest=thekernel-perf,debug-threads=on"]
                if images.direct_kernel:
                    extra_args[0:0] = ["-append", images.cmdline or ""]
                    if images.initrd is not None:
                        extra_args[0:0] = ["-initrd", str(images.initrd)]
                with commands.open("rb") as input_stream:
                    result = run(
                        RunConfig(
                            arch="x86_64",
                            kernel=images.kernel,
                            rootfs=images.rootfs,
                            esp=images.esp,
                            direct_kernel=images.direct_kernel,
                            workdir=run_dir,
                            log_path=run_dir / "console.log",
                            cache_dir=output / "image-cache",
                            extra_block=images.extra_block,
                            memory=args.memory,
                            cpus=args.cpus,
                            qemu_binary=qemu,
                            qemu_launcher=(
                                sys.executable,
                                str(Path(__file__).with_name("kvm_scheduler_pinner.py")),
                            ),
                            accel="kvm",
                            cpu="host",
                            iothread_id="subsystem-io",
                            network=network,
                            tap_name=packet_tap,
                            extra_args=tuple(extra_args),
                            ovmf_code=args.ovmf_code,
                            ovmf_vars=args.ovmf_vars,
                            receipt_path=run_dir / "qemu-receipt.json",
                            limits=RunLimits(total_timeout_secs=args.timeout, ready_timeout_secs=args.ready_timeout),
                            interaction=Interaction(interactive=True, input_after_marker=args.ready_marker),
                        ),
                        input_stream=input_stream,
                    )
            finally:
                if peer is not None and result is not None:
                    try:
                        _wait_packet_peer_done(peer, getattr(args, "peer_done_timeout", 5.0))
                    except BaselineError as error:
                        peer.done_error = str(error)
                elif peer is not None:
                    peer.done_error = "guest runner did not complete"
                peer_returncode = _stop_packet_peer(peer)
                for key, previous_value in previous.items():
                    if previous_value is None:
                        os.environ.pop(key, None)
                    else:
                        os.environ[key] = previous_value

            guest = parse_tkperf_log(
                run_dir / "console.log",
                target=target,
                repeat=repeat,
                expected_workload=expected_workload,
                expected_topology="formal" if packet_lane else None,
            )
            try:
                expected_external_backends = parse_external_backend_identities()
            except BackendIdentityUnavailable:
                pin_valid = False
            else:
                pin_valid = _pin_report_valid(
                    pin_report,
                    expected_vcpu_count=args.cpus,
                    vcpu_cpus=vcpu_cpus,
                    io_cpus=io_cpus,
                    backend_cpus=backend_cpus,
                    expected_external_backends=expected_external_backends,
                )
            peer_ok = (
                peer is None
                or (
                    peer.done is not None
                    and peer.done_error is None
                    and (peer.returncode == 0 or (peer.returncode == -15 and peer.terminated_by_harness))
                    and peer.readback_cpus == peer.requested_cpus
                )
            )
            runner_returncode = result.returncode if result is not None else 1
            run_identity_ok = packet_run_id is None or guest.run_id == packet_run_id
            data_proof_ok = _data_proof_matches(
                guest,
                data_device=str(getattr(args, "data_device", FORMAL_DATA_DEVICE)),
                data_dir=str(getattr(args, "data_dir", "/mnt/thekernel-perf-data")),
            )
            extra_block_proof_ok = (
                _extra_block_receipt_matches(
                    run_dir / "qemu-receipt.json",
                    extra_block=images.extra_block,
                    data_device=str(getattr(args, "data_device", FORMAL_DATA_DEVICE)),
                )
                if args.subsystem == "io-uring-physical"
                else True
            )
            data_proof_ok = data_proof_ok and extra_block_proof_ok
            eligible = peer_ok and run_identity_ok and data_proof_ok and eligible_for_stats(
                guest, runner_returncode=runner_returncode, pin_valid=pin_valid
            )
            pin_status = pin_report_failure_status(pin_report, pin_valid)
            if eligible:
                raw_samples.extend(guest.samples)
            status = (
                "ok" if eligible
                else "host-peer-error" if not peer_ok
                else "protocol-error" if not run_identity_ok
                else "data-disk-error" if not data_proof_ok
                else "pinning-error" if pin_status == "pinning-error"
                else "unsupported" if pin_status == "unsupported"
                else "unsupported" if runner_returncode == 78
                else guest.status if runner_returncode == 0
                else "runner-error"
            )
            runs.append(
                {
                    "target": target,
                    "repeat": repeat,
                    "subsystem": args.subsystem,
                    "helper": helper,
                    "workload": guest.workload,
                    "run_id": guest.run_id,
                    "status": status,
                    "runner_returncode": runner_returncode,
                    "boot": "direct-kernel" if images.direct_kernel else "uefi",
                    "host_peer_returncode": peer_returncode,
                    "host_peer_ready": peer.ready is not None if peer is not None else None,
                    "host_peer_done": peer.done is not None if peer is not None else None,
                    "host_peer_done_error": peer.done_error if peer is not None else None,
                    "host_peer_affinity_requested": list(peer.requested_cpus) if peer is not None else None,
                    "host_peer_affinity_readback": list(peer.readback_cpus) if peer is not None else None,
                    "host_peer_pid": peer.process.pid if peer is not None else None,
                    "host_peer_terminated_by_harness": peer.terminated_by_harness if peer is not None else None,
                    "packet_config": (
                        {
                            "topology": "formal/tap-vhost",
                            "guest_interface": getattr(args, "packet_interface", "eth0"),
                            "tap_name": packet_tap,
                            "peer_mac": packet_peer_mac,
                            "peer_backend_cpu": backend_cpus[0] if backend_cpus else None,
                            "run_id": packet_run_id,
                        }
                        if packet_lane else None
                    ),
                    "done": guest.done,
                    "done_status": guest.done_status,
                    "correctness": guest.correctness,
                    "claim": "degraded" if guest.claim_degraded else "formal",
                    "claim_degraded": guest.claim_degraded,
                    "pin_valid": pin_valid,
                    "data_proof": dict(guest.data_proof) if guest.data_proof is not None else None,
                    "data_proof_matches_request": data_proof_ok,
                    "extra_block_proof_matches_runner": extra_block_proof_ok,
                    "extra_block_device": FORMAL_DATA_DEVICE if images.extra_block is not None else None,
                    "extra_block_source": FORMAL_DATA_SOURCE if images.extra_block is not None else None,
                    "reason": (
                        "thread-pinning-unsupported"
                        if pin_status == "unsupported"
                        else guest.error
                    ),
                    "formal_cells": len(guest.samples),
                }
            )

    _write_tsv(
        output / "raw-samples.tsv",
        RAW_COLUMNS,
        (
            {
                "schema": SCHEMA,
                "target": sample.target,
                "repeat": sample.repeat,
                "workload": sample.workload,
                "run_id": sample.run_id,
                "cell": sample.cell,
                "op": sample.op or "",
                "size": "" if sample.size is None else sample.size,
                "qd": "" if sample.qd is None else sample.qd,
                "window_warmup": sample.window_warmup,
                "window_samples": sample.window_samples,
                "latency_samples": sample.latency_samples,
                "wall_p50_ns": sample.wall_p50_ns,
                "wall_p99_ns": sample.wall_p99_ns,
                "cpu_p50_ns": sample.cpu_p50_ns,
                "cpu_p99_ns": sample.cpu_p99_ns,
                **_optional_row_fields((sample,)),
            }
            for sample in raw_samples
        ),
    )
    summary = summarize_samples(raw_samples)
    (output / "summary.json").write_text(
        json.dumps(
            {
                "schema": SCHEMA,
                "helper": helper,
                "subsystem": args.subsystem,
                "targets": list(targets),
                "raw_sample_count": len(raw_samples),
                "optional_cost_metrics": list(PMU_EVIDENCE_KEYS),
                "optional_cost_policy": "record-when-available; missing-is-empty-not-zero",
                "cost_capabilities": cost_capabilities,
                "runs": summary,
            },
            indent=2,
            sort_keys=True,
        )
        + "\n",
        encoding="utf-8",
    )
    _write_tsv(output / "summary.tsv", SUMMARY_COLUMNS, summary)
    target_manifest = {
        target: {
            "kernel": str(images.kernel),
            "rootfs": str(images.rootfs),
            "esp": str(images.esp) if images.esp is not None else None,
            "extra_block": str(images.extra_block) if images.extra_block is not None else None,
            "extra_block_device": FORMAL_DATA_DEVICE if images.extra_block is not None else None,
            "extra_block_source": FORMAL_DATA_SOURCE if images.extra_block is not None else None,
            "extra_block_guest_mapping": (
                FORMAL_DATA_MAPPING if images.extra_block is not None else None
            ),
            "rootfs_device": "/dev/vda" if images.extra_block is not None else None,
            "initrd": str(images.initrd) if images.initrd is not None else None,
            "boot": "direct-kernel" if images.direct_kernel else "uefi",
            "firmware": None if images.direct_kernel else "OVMF",
            "cmdline": images.cmdline,
        }
        for target, images in images_by_target.items()
    }
    for record in unavailable:
        target_manifest.setdefault(
            str(record["target"]),
            {
                "status": "unavailable",
                "reason": record.get("reason"),
                "kernel": None,
                "rootfs": None,
                "esp": None,
                "extra_block": None,
                "extra_block_device": None,
                "extra_block_source": None,
                "extra_block_guest_mapping": None,
                "rootfs_device": None,
                "boot": None,
                "firmware": None,
                "initrd": None,
                "cmdline": None,
            },
        )
    data_disk = {
        "required": args.subsystem == "io-uring-physical",
        "device": FORMAL_DATA_DEVICE if args.subsystem == "io-uring-physical" else None,
        "mount": getattr(args, "data_dir", "/mnt/thekernel-perf-data"),
        "source": FORMAL_DATA_SOURCE if args.subsystem == "io-uring-physical" else None,
        "qemu_drive_id": "extra" if args.subsystem == "io-uring-physical" else None,
        "rootfs_device": "/dev/vda" if args.subsystem == "io-uring-physical" else None,
        "guest_mapping": FORMAL_DATA_MAPPING if args.subsystem == "io-uring-physical" else None,
        "enumeration": (
            "unique virtio-blk order: id=rootfs -> /dev/vda; id=extra -> /dev/vdb"
            if args.subsystem == "io-uring-physical" else None
        ),
        "filesystem": "ext4" if args.subsystem == "io-uring-physical" else None,
        "marker": DATA_MARKER if args.subsystem == "io-uring-physical" else None,
        "identity": "st_dev==st_rdev" if args.subsystem == "io-uring-physical" else None,
        "proof": all(
            record.get("data_proof_matches_request") is True
            for record in runs
            if record.get("target") in images_by_target
        ) if args.subsystem == "io-uring-physical" else False,
        "runner_receipt_binding": (
            "structured rootfs-before-extra command: -drive id=extra file=runtime + virtio-blk-pci,drive=extra"
            if args.subsystem == "io-uring-physical" else None
        ),
    }
    (output / "manifest.json").write_text(
        json.dumps(
            {
                "schema": SCHEMA,
                "subsystem": args.subsystem,
                "target": target_choice,
                "targets": target_manifest,
                "boot_comparison": "TheKernel uses UEFI/OVMF; Linux uses direct bzImage when selected; boot is outside measurement windows",
                "helper": helper,
                "network": network,
                "packet_topology": "formal/tap-vhost" if packet_lane else None,
                "tap_name": packet_tap,
                "packet_peer_mac": packet_peer_mac,
                "packet_peer_command": list(packet_peer_command),
                "packet_peer_schema": PEER_SCHEMA if packet_lane else None,
                "data_disk": data_disk,
                "cost_capabilities": cost_capabilities,
                "vcpu_cpus": list(vcpu_cpus),
                "io_cpus": list(io_cpus),
                "backend_cpus": list(backend_cpus),
                "housekeeping_cpus": list(housekeeping),
                "cpu_selection": {
                    "policy": "core-cache-maxfreq-class",
                    "vcpu_class": measurement_class,
                    "vcpu_count": len(vcpu_cpus),
                    "io_class": io_class,
                    "backend_class": backend_class,
                    "housekeeping_class": (
                        next(iter(housekeeping_classes))
                        if len(housekeeping_classes) == 1 else "mixed"
                    ) if housekeeping else None,
                },
                "host_cpu_topology": host_topology_manifest(topology),
                "runs": runs,
            },
            indent=2,
            sort_keys=True,
        )
        + "\n",
        encoding="utf-8",
    )
    return 0 if runs and all(run_record["status"] == "ok" for run_record in runs) else 1


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="kvm-subsystem-baseline")
    subparsers = parser.add_subparsers(dest="command", required=True)
    run_parser = subparsers.add_parser("run", help="run an explicit KVM subsystem lane")
    run_parser.add_argument("--subsystem", choices=FORMAL_SUBSYSTEMS, required=True)
    run_parser.add_argument("--target", choices=("thekernel", "linux", "both"), default="thekernel")
    run_parser.add_argument("--kernel", type=Path)
    run_parser.add_argument("--rootfs", type=Path)
    run_parser.add_argument("--esp", type=Path)
    run_parser.add_argument("--extra-block", type=Path)
    run_parser.add_argument("--linux-kernel", type=Path)
    run_parser.add_argument("--linux-rootfs", type=Path)
    run_parser.add_argument("--linux-extra-block", type=Path)
    run_parser.add_argument("--linux-initrd", type=Path)
    run_parser.add_argument(
        "--linux-cmdline",
        default="root=/dev/vda rw console=ttyS0 init=/etc/thekernel/shell-init.sh panic=-1 reboot=t",
    )
    run_parser.add_argument("--ovmf-code", type=Path)
    run_parser.add_argument("--ovmf-vars", type=Path)
    run_parser.add_argument("--output", type=Path, required=True)
    run_parser.add_argument(
        "--guest-program",
        help="override the image helper path (formal lanes otherwise use the fixed map)",
    )
    run_parser.add_argument("--guest-args", nargs="*", default=[])
    run_parser.add_argument("--tap-name", help="existing host TAP interface for packet formal")
    run_parser.add_argument("--packet-interface", default="eth0")
    run_parser.add_argument("--packet-peer-mac")
    run_parser.add_argument(
        "--packet-peer-command",
        nargs="+",
        help="one-shot TKPFNET1 AF_PACKET host peer; receives tap/MAC via environment",
    )
    run_parser.add_argument("--peer-ready-timeout", type=float, default=5.0)
    run_parser.add_argument("--peer-done-timeout", type=float, default=5.0)
    run_parser.add_argument("--data-dir", default="/mnt/thekernel-perf-data")
    run_parser.add_argument("--data-device", default="/dev/vdb")
    # Kept as a suppressed compatibility spelling for callers that still pass
    # the old scheduler option; helper selection is now determined by
    # --subsystem and never loops over a second workload protocol.
    run_parser.add_argument("--workloads", nargs="+", default=None, help=argparse.SUPPRESS)
    run_parser.add_argument("--iterations", type=int, default=1000)
    run_parser.add_argument("--warmup", type=int, default=100)
    run_parser.add_argument("--repeat", type=int, default=3)
    run_parser.add_argument("--cpus", type=int, default=1)
    run_parser.add_argument("--vcpu-cpus", "--measurement-cpus", dest="vcpu_cpus")
    run_parser.add_argument("--io-cpus")
    run_parser.add_argument("--backend-cpus")
    run_parser.add_argument("--housekeeping-cpus")
    run_parser.add_argument(
        "--prearm-kvm-nx-worker",
        action="store_true",
        help="hold vCPUs on housekeeping until KVM's untraced NX worker is armed",
    )
    run_parser.add_argument(
        "--prearm-kvm-nx-timeout", type=float, default=2.0
    )
    run_parser.add_argument(
        "--network",
        "--network-mode",
        "--network-topology",
        dest="network",
        choices=("user", "passt", "tap-vhost"),
        default="passt",
    )
    run_parser.add_argument("--memory", default="128M")
    run_parser.add_argument("--timeout", type=float, default=120.0)
    run_parser.add_argument("--ready-timeout", type=float, default=60.0)
    run_parser.add_argument("--ready-marker", default="THEKERNEL_SHELL_READY")
    run_parser.add_argument("--shutdown-command", default="/bin/busybox poweroff -f")
    run_parser.add_argument("--qemu-binary")
    run_parser.set_defaults(func=run_command)
    stats_parser = subparsers.add_parser("stats", help="recompute statistics from raw samples")
    stats_parser.add_argument("--input", type=Path, required=True)
    stats_parser.add_argument("--output", type=Path, required=True)
    stats_parser.add_argument("--summary-tsv", type=Path)
    stats_parser.set_defaults(func=lambda args: stats_command(args.input, args.output, args.summary_tsv))
    return parser


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    try:
        args = parser.parse_args(argv)
        if args.command == "run" and (
            args.iterations <= 0 or args.warmup < 0 or args.repeat <= 0 or args.cpus <= 0
        ):
            parser.error("iterations/repeat/cpus must be positive and warmup non-negative")
        return int(args.func(args))
    except TopologyUnavailable as error:
        print(f"kvm-subsystem-baseline: UNSUPPORTED: {error}", file=sys.stderr)
        return 78
    except (BaselineError, RunnerError, OSError) as error:
        print(f"kvm-subsystem-baseline: ERROR: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
