#!/usr/bin/env python3
"""Reproducible TheKernel/Linux KVM scheduler baseline lane.

This is deliberately a small policy layer over ``tools.qemu_runner``.  It
does not discover or download a Linux image.  The TheKernel lane boots the
explicit x86_64 kernel/rootfs/ESP tuple through q35/UEFI; the Linux lane boots
an explicitly supplied x86_64 bzImage directly with the same rootfs and a
fixed shell-init command line.  Consequently Linux needs only
``--linux-kernel`` and ``--linux-rootfs`` while TheKernel still requires ``--kernel``,
``--rootfs``, and ``--esp``.  Linux may additionally receive an explicit
``--linux-initrd`` when the kernel needs modules to reach its rootfs.
"""

from __future__ import annotations

import argparse
import csv
import json
import os
import shutil
import sys
from collections import defaultdict
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable, Mapping

# ``python tools/kvm_scheduler_baseline.py`` puts ``tools/`` (rather than the
# repository root) on ``sys.path``.  Use the same imports as module execution
# after a minimal repo-root bootstrap; no second runner implementation is
# needed.
if __package__ in {None, ""}:
    _repo_root = str(Path(__file__).resolve().parents[1])
    if _repo_root not in sys.path:
        sys.path.insert(0, _repo_root)

from tools.qemu_runner.model import Interaction, RunLimits
from tools.qemu_runner.runner import RunConfig, RunnerError, run
from tools.qemu_runner.evidence import file_evidence
from tools.qemu_runner.receipt import ReceiptError, source_identity
from tools.kvm_scheduler_pinner import (
    BackendIdentityUnavailable,
    parse_external_backend_identities,
)
from tools.kvm_subsystem_baseline import (
    TopologyUnavailable,
    _measurement_class,
    choose_housekeeping_cpus,
    host_cost_capabilities,
    host_topology_manifest,
    read_host_topology,
    validate_cpu_roles,
    validate_cpu_selection,
)
from tools.host_pmu import PmuUnavailable, UserProcessTreePmu
from scripts.ci import source_combination


# Raw guest observations and their recomputable summaries retain the v1 wire
# schema.  The run manifest is v2 because it records run-scoped PMU raw-data
# collection without changing per-sample evidence semantics.
SCHEMA = "thekernel-kvm-scheduler-baseline-v1"
MANIFEST_SCHEMA = "thekernel-kvm-scheduler-baseline-v2"
RUN_SCHEMA = "thekernel-scheduler-baseline-run-v1"
SAMPLE_SCHEMA = "thekernel-scheduler-baseline-sample-v1"
RESULT_SCHEMA = "thekernel-scheduler-baseline-result-v1"
PIN_REPORT_SCHEMA = "thekernel-kvm-thread-pinning-v5"
PIN_REPORT_PREVIOUS_SCHEMA = "thekernel-kvm-thread-pinning-v4"
PIN_REPORT_LEGACY_SCHEMA = "thekernel-kvm-thread-pinning-v2"
PIN_REPORT_V3_SCHEMA = PIN_REPORT_SCHEMA
PIN_REPORT_KEYS = frozenset(
    {
        "schema",
        "pid",
        "expected_vcpu_count",
        "requested_vcpu_cpus",
        "requested_io_cpus",
        "vcpu_threads",
        "io_threads",
        "vcpu_status",
        "io_status",
    }
)
PIN_THREAD_KEYS = frozenset({"tid", "affinity"})
PIN_THREAD_V3_KEYS = frozenset({"tid", "name", "affinity"})
PIN_ROLE_THREAD_V3_KEYS = frozenset({"tid", "name", "affinity", "tgid"})
PIN_BACKEND_THREAD_V3_KEYS = frozenset({"tid", "name", "affinity", "tgid"})
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
PIN_REPORT_V3_KEYS = frozenset(
    {
        "schema",
        "pid",
        "expected_vcpu_count",
        "requested_vcpu_cpus",
        "requested_io_cpus",
        "requested_backend_cpus",
        "housekeeping_cpus",
        "measurement_cpus",
        "measurement_smt_siblings",
        "vcpu_threads",
        "io_threads",
        "backend_threads",
        "external_processes",
        "declared_external_backends",
        "qemu_main",
        "unknown_threads",
        "vcpu_status",
        "io_status",
        "backend_status",
        "qemu_main_status",
        "housekeeping_status",
        "unknown_status",
        "unknown_off_measurement",
        "launcher_affinity",
        "process_inherited_housekeeping",
        "new_threads_inherit_housekeeping",
        "ptrace_clone_events",
        "clone_event_count",
        "unknown_thread_proof",
        "exit_readback_tids",
        "exit_readback_proof",
        "terminal_proofs",
        "proof_failures",
    }
)
PIN_REPORT_V4_KEYS = PIN_REPORT_V3_KEYS - frozenset({"terminal_proofs"})
PIN_PTRACE_TERMINAL_PROOF_KEYS = frozenset({"tid", "method", "affinity"})
PIN_KVM_TERMINAL_PROOF_KEYS = frozenset(
    {
        "tid", "method", "affinity", "comm", "tgid", "user_worker",
        "prearm_vcpus_housekeeping", "worker_housekeeping", "left_qemu_tgid",
    }
)
WORKLOADS = ("futex", "pipe", "cpu-worker")
PLACEMENTS = ("same", "cross")
DEFAULT_LINUX_CMDLINE = (
    "root=/dev/vda rw console=ttyS0 init=/etc/thekernel/shell-init.sh "
    "panic=-1 reboot=t"
)
RAW_COLUMNS = (
    "schema",
    "target",
    "repeat",
    "workload",
    "placement",
    "worker",
    "sample",
    "latency_ns",
    "path",
    "oracle",
    "cycles",
    "instructions",
    "cache_misses",
    "branch_misses",
    "llc_hitm",
    "cpu_cost_ns",
)
SUMMARY_COLUMNS = (
    "schema",
    "target",
    "repeat",
    "workload",
    "placement",
    "status",
    "count",
    "p50_ns",
    "p99_ns",
    "p999_ns",
    # Optional path/oracle and PMU/CPU-cost evidence.  An unavailable counter
    # stays empty; a fabricated zero would look like a real measurement.
    "path",
    "oracle",
    "cycles",
    "instructions",
    "cache_misses",
    "branch_misses",
    "llc_hitm",
    "cpu_cost_ns",
)
OPTIONAL_EVIDENCE_COLUMNS = (
    "path", "oracle", "cycles", "instructions", "cache_misses",
    "branch_misses", "llc_hitm", "cpu_cost_ns",
)
PMU_EVIDENCE_COLUMNS = (
    "cycles", "instructions", "cache_misses", "branch_misses", "llc_hitm",
    "cpu_cost_ns",
)
SAMPLE_CHECKSUM_OFFSET = 14695981039346656037
SAMPLE_CHECKSUM_PRIME = 1099511628211
UINT64_MASK = (1 << 64) - 1


class BaselineError(ValueError):
    """Raised for an invalid baseline contract or evidence stream."""


def scheduler_sample_checksum(samples: Iterable["Sample"]) -> int:
    """Compute the guest RESULT checksum from canonical raw samples.

    The old helper checksum was derived from a worker's private state and
    could not detect a serial stream splice.  The formal checksum is now
    FNV-1a over each ``worker, sample, latency_ns`` tuple in canonical order.
    Every field is an unsigned 64-bit integer encoded as eight little-endian
    bytes.  This explicit wire encoding lets the C guest and Python parser
    calculate the same value without sharing hidden worker state.
    """

    ordered = sorted(samples, key=lambda sample: (sample.worker, sample.sample))
    checksum = SAMPLE_CHECKSUM_OFFSET
    for sample in ordered:
        for value in (sample.worker, sample.sample, sample.latency_ns):
            encoded = int(value) & UINT64_MASK
            for _ in range(8):
                checksum ^= encoded & 0xFF
                checksum = (checksum * SAMPLE_CHECKSUM_PRIME) & UINT64_MASK
                encoded >>= 8
    return checksum


@dataclass(frozen=True)
class Sample:
    target: str
    repeat: int
    workload: str
    placement: str
    worker: int
    sample: int
    latency_ns: int
    attributes: tuple[tuple[str, str], ...] = ()


def scheduler_evidence_class(samples: Iterable[Sample]) -> str:
    """Require a per-sample CPU/PMU witness before calling a lane formal."""

    captured = tuple(samples)
    if not captured:
        return "not-measured"
    attributes = tuple(dict(sample.attributes) for sample in captured)
    if all("cpu_cost_ns" in witness for witness in attributes):
        return "cpu-cost-evidenced"
    hardware_pmu = ("cycles", "instructions", "cache_misses", "branch_misses", "llc_hitm")
    if all(any(metric in witness for metric in hardware_pmu) for witness in attributes):
        return "pmu-evidenced"
    return "measured-latency-only"


@dataclass(frozen=True)
class GuestRun:
    target: str
    repeat: int
    workload: str
    placement: str
    samples: tuple[Sample, ...]
    status: str
    reason: str | None = None
    result_status: str | None = None
    result_count: int | None = None
    result_checksum: int | None = None
    result_quantiles: tuple[int, int, int] | None = None
    exit_status: int | None = None


@dataclass(frozen=True)
class TargetImages:
    """Validated artifacts and boot policy for one comparison target."""

    kernel: Path
    rootfs: Path
    esp: Path | None
    direct_kernel: bool
    initrd: Path | None = None
    cmdline: str | None = None


def parse_cpu_list(value: str, *, allowed: set[int] | None = None) -> tuple[int, ...]:
    if not value:
        raise BaselineError("CPU list must not be empty")
    cpus: list[int] = []
    for item in value.split(","):
        bounds = item.split("-")
        if len(bounds) == 1:
            bounds.append(bounds[0])
        if len(bounds) != 2 or any(not bound.isdecimal() for bound in bounds):
            raise BaselineError(f"invalid CPU list: {value!r}")
        first, last = (int(bound, 10) for bound in bounds)
        if first > last or first < 0 or last > 1_048_575:
            raise BaselineError(f"invalid CPU list: {value!r}")
        cpus.extend(range(first, last + 1))
    if len(set(cpus)) != len(cpus):
        raise BaselineError(f"CPU list contains duplicates: {value!r}")
    result = tuple(sorted(cpus))
    if allowed is not None and not set(result).issubset(allowed):
        missing = sorted(set(result) - allowed)
        raise BaselineError(f"CPU list is outside runner affinity: {missing}")
    return result


def _pin_cpu_array(value: object, *, label: str) -> tuple[int, ...]:
    if not isinstance(value, list) or any(
        isinstance(cpu, bool) or not isinstance(cpu, int) or cpu < 0 for cpu in value
    ):
        raise BaselineError(f"pin report {label} must be a list of non-negative integers")
    result = tuple(value)
    if tuple(sorted(set(result))) != result:
        raise BaselineError(f"pin report {label} must be sorted and contain no duplicates")
    return result


def _pin_external_identity(
    value: object,
    *,
    label: str,
) -> tuple[int, int, str, int] | None:
    """Validate and canonicalize one runner-declared process identity."""

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


def validate_pin_report(
    path: Path,
    *,
    expected_pid: int | None,
    expected_vcpu_count: int,
    requested_vcpu: tuple[int, ...],
    requested_io: tuple[int, ...],
    requested_backend: tuple[int, ...] = (),
    expected_external_backends: tuple[Mapping[str, object], ...] = (),
) -> str | None:
    """Validate the pinner's complete, read-back affinity contract.

    The pinner writes this report only after QEMU exits, so the baseline cannot
    query the threads itself.  Every field is therefore checked, including the
    exact vCPU index set, real TIDs, and the affinity read back while each
    thread was alive.  A reason string is returned for manifest/reporting use;
    a valid report returns ``None``.
    """

    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        return f"invalid pin report: {error}"
    if not isinstance(payload, dict):
        return "invalid pin report: top-level value is not an object"
    schema = payload.get("schema")
    if schema is None:
        missing = sorted(PIN_REPORT_KEYS - set(payload))
        extra = sorted(set(payload) - PIN_REPORT_KEYS)
        return f"invalid pin report fields: missing={missing} extra={extra}"
    if schema not in (
        PIN_REPORT_SCHEMA, PIN_REPORT_PREVIOUS_SCHEMA, PIN_REPORT_LEGACY_SCHEMA
    ):
        return f"unsupported pin report schema: {schema!r}"
    # A v3 report is accepted only with the complete housekeeping and launcher
    # inheritance proof.  The legacy shape is accepted only when it declares
    # the legacy schema; a v3 report cannot silently downgrade to old evidence.
    is_v3 = schema in (PIN_REPORT_SCHEMA, PIN_REPORT_PREVIOUS_SCHEMA)
    is_v5 = schema == PIN_REPORT_SCHEMA
    expected_keys = (
        PIN_REPORT_V3_KEYS if is_v5
        else PIN_REPORT_V4_KEYS if is_v3
        else PIN_REPORT_KEYS
    )
    if set(payload) != expected_keys:
        missing = sorted(expected_keys - set(payload))
        extra = sorted(set(payload) - expected_keys)
        return f"invalid pin report fields: missing={missing} extra={extra}"
    if payload["schema"] not in (
        PIN_REPORT_SCHEMA, PIN_REPORT_PREVIOUS_SCHEMA, PIN_REPORT_LEGACY_SCHEMA
    ):
        return f"unsupported pin report schema: {payload['schema']!r}"
    if is_v3 and payload["proof_failures"]:
        failure = payload["proof_failures"][0]
        if (
            not isinstance(failure, dict)
            or not isinstance(failure.get("reason"), str)
            or not failure["reason"]
            or isinstance(failure.get("tid"), bool)
            or not isinstance(failure.get("tid"), int)
            or failure["tid"] <= 0
        ):
            return "invalid pin report proof_failures"
        return (
            "pin report contains proof failure: "
            f"{failure['reason']} tid={failure['tid']}"
        )

    pid = payload["pid"]
    if isinstance(pid, bool) or not isinstance(pid, int) or pid <= 0:
        return "invalid pin report pid"
    if expected_pid is not None and pid != expected_pid:
        return f"pin report pid mismatch: expected {expected_pid}, got {pid}"
    count = payload["expected_vcpu_count"]
    if (
        isinstance(count, bool)
        or not isinstance(count, int)
        or count <= 0
        or count != expected_vcpu_count
    ):
        return (
            "pin report vCPU count mismatch: "
            f"expected {expected_vcpu_count}, got {count!r}"
        )
    try:
        report_vcpu = _pin_cpu_array(payload["requested_vcpu_cpus"], label="requested_vcpu_cpus")
        report_io = _pin_cpu_array(payload["requested_io_cpus"], label="requested_io_cpus")
    except BaselineError as error:
        return str(error)
    if set(report_vcpu) != set(requested_vcpu):
        return f"pin report requested vCPU set mismatch: {report_vcpu!r}"
    if set(report_io) != set(requested_io):
        return f"pin report requested IO set mismatch: {report_io!r}"
    if not requested_vcpu:
        return "vCPU pinning was not requested"
    if not requested_io:
        return "dedicated IO pinning was not requested"
    # Validate every role container before dereferencing it.  A malformed
    # report is evidence failure, not an exception path in the validator.
    if not isinstance(payload["vcpu_threads"], dict):
        return "invalid pin report vcpu_threads is not an object"
    if not isinstance(payload["io_threads"], list):
        return "invalid pin report io_threads is not an array"
    if is_v3:
        for field in ("backend_threads", "unknown_threads", "external_processes"):
            if not isinstance(payload[field], list):
                return f"invalid pin report {field} is not an array"
        if not isinstance(payload["qemu_main"], dict):
            return "invalid pin report qemu_main is not an object"
        if not isinstance(payload["declared_external_backends"], list):
            return "invalid pin report declared_external_backends is not an array"
        expected_identity_tuples: set[tuple[int, int, str, int]] = set()
        for expected in expected_external_backends:
            identity = _pin_external_identity(
                expected, label="expected external backend identity"
            )
            if identity is None:
                return "invalid expected external backend identity"
            if identity[0] in {item[0] for item in expected_identity_tuples}:
                return "duplicate expected external backend identity"
            expected_identity_tuples.add(identity)
        declared_identity_tuples: set[tuple[int, int, str, int]] = set()
        for declaration in payload["declared_external_backends"]:
            identity = _pin_external_identity(
                declaration, label="declared external backend identity"
            )
            if identity is None:
                return "invalid declared external backend identity"
            if identity[0] in {item[0] for item in declared_identity_tuples}:
                return "duplicate declared external backend identity"
            declared_identity_tuples.add(identity)
        if declared_identity_tuples != expected_identity_tuples:
            return "declared external backend identities do not match runner contract"
    external_process_ids: set[int] = set()
    if is_v3:
        try:
            report_backend = _pin_cpu_array(
                payload["requested_backend_cpus"], label="requested_backend_cpus"
            )
            report_housekeeping = _pin_cpu_array(
                payload["housekeeping_cpus"], label="housekeeping_cpus"
            )
            report_measurement = _pin_cpu_array(
                payload["measurement_cpus"], label="measurement_cpus"
            )
            report_smt_siblings = _pin_cpu_array(
                payload["measurement_smt_siblings"], label="measurement_smt_siblings"
            )
        except BaselineError as error:
            return str(error)
        if set(report_backend) != set(requested_backend):
            return (
                "pin report requested backend set mismatch: "
                f"{report_backend!r}"
            )
        if set(report_measurement) != set(report_vcpu) | set(report_io) | set(report_backend):
            return "pin report measurement CPU set mismatch"
        if not set(report_measurement).issubset(set(report_smt_siblings)):
            return "pin report SMT sibling set omits a measurement CPU"
        if not report_housekeeping:
            return "pin report has no housekeeping CPU"
        if set(report_housekeeping) & set(report_smt_siblings):
            return "pin report housekeeping overlaps measurement CPUs or SMT siblings"
        try:
            launcher_affinity = _pin_cpu_array(
                payload["launcher_affinity"], label="launcher_affinity"
            )
        except BaselineError as error:
            return str(error)
        if launcher_affinity != report_housekeeping:
            return "launcher affinity does not match housekeeping CPUs"
        if payload["process_inherited_housekeeping"] is not True:
            return "launcher did not prove process housekeeping inheritance"
        if payload["new_threads_inherit_housekeeping"] is not True:
            return "launcher did not prove new-thread housekeeping inheritance"
        if payload["ptrace_clone_events"] is not True:
            return "clone-event tracing proof is absent"
        clone_count = payload["clone_event_count"]
        if isinstance(clone_count, bool) or not isinstance(clone_count, int) or clone_count <= 0:
            return "invalid clone-event count"
        if payload["unknown_thread_proof"] != "ptrace-clone-event":
            return "unknown-thread placement proof is unsupported"
        if payload["exit_readback_proof"] is not True:
            return "final affinity readback proof is absent"
        exit_readback_tids = payload["exit_readback_tids"]
        if (
            not isinstance(exit_readback_tids, list)
            or not exit_readback_tids
            or any(
                isinstance(tid, bool) or not isinstance(tid, int) or tid <= 0
                for tid in exit_readback_tids
            )
            or exit_readback_tids != sorted(set(exit_readback_tids))
        ):
            return "invalid final affinity readback task list"
        external_processes = payload["external_processes"]
        if not isinstance(external_processes, list):
            return "external process tracking is not a list"
        observed_tids: set[int] = set()
        role_tids: dict[int, tuple[int, ...]] = {}
        measurement_role_tids: set[int] = set()
        external_collision_tids: set[int] = set()
        external_backend_tids: set[int] = set()
        external_identity_by_pid: dict[int, tuple[int, int, str, int]] = {}
        for record in payload["vcpu_threads"].values():
            if isinstance(record, dict) and isinstance(record.get("tid"), int):
                observed_tids.add(record["tid"])
                measurement_role_tids.add(record["tid"])
                external_collision_tids.add(record["tid"])
                affinity = record.get("affinity")
                if isinstance(affinity, list) and all(isinstance(cpu, int) for cpu in affinity):
                    role_tids[record["tid"]] = tuple(affinity)
        for field in ("io_threads", "unknown_threads"):
            records = payload[field]
            if not isinstance(records, list):
                return "invalid thread records"
            for record in records:
                if isinstance(record, dict) and isinstance(record.get("tid"), int):
                    observed_tids.add(record["tid"])
                    if field == "io_threads":
                        measurement_role_tids.add(record["tid"])
                    external_collision_tids.add(record["tid"])
                    affinity = record.get("affinity")
                    if isinstance(affinity, list) and all(isinstance(cpu, int) for cpu in affinity):
                        role_tids[record["tid"]] = tuple(affinity)
        records = payload["backend_threads"]
        if not isinstance(records, list):
            return "invalid thread records"
        for record in records:
            if isinstance(record, dict) and isinstance(record.get("tid"), int):
                backend_tid = record["tid"]
                observed_tids.add(backend_tid)
                measurement_role_tids.add(backend_tid)
                backend_tgid = record.get("tgid")
                if backend_tgid == pid:
                    # Internal QEMU backends are still QEMU-owned TIDs; an
                    # external process leader may never alias them.
                    external_collision_tids.add(backend_tid)
                elif (
                    isinstance(backend_tgid, int)
                    and not isinstance(backend_tgid, bool)
                    and backend_tgid > 0
                    and backend_tid == backend_tgid
                ):
                    external_backend_tids.add(backend_tid)
                affinity = record.get("affinity")
                if isinstance(affinity, list) and all(isinstance(cpu, int) for cpu in affinity):
                    role_tids[backend_tid] = tuple(affinity)
        qemu_main_candidate = payload.get("qemu_main")
        if (
            isinstance(qemu_main_candidate, dict)
            and isinstance(qemu_main_candidate.get("tid"), int)
            and not isinstance(qemu_main_candidate.get("tid"), bool)
        ):
            external_collision_tids.add(qemu_main_candidate["tid"])
        for process in external_processes:
            if not isinstance(process, dict) or set(process) != PIN_EXTERNAL_PROCESS_V3_KEYS:
                return "invalid external process tracking record"
            external_pid = process["pid"]
            main_tid = process["main_tid"]
            name = process["name"]
            process_identity = _pin_external_identity(
                {
                    "pid": external_pid,
                    "tgid": process.get("tgid"),
                    "exe": process.get("exe"),
                    "starttime": process.get("starttime"),
                },
                label="external process identity",
            )
            if (
                isinstance(external_pid, bool)
                or not isinstance(external_pid, int)
                or external_pid <= 0
                or main_tid != external_pid
                or not isinstance(name, str)
                or not name
                or process_identity is None
                or process_identity[0] != external_pid
                or process_identity[1] != process["tgid"]
                or process_identity[1] != external_pid
                or external_pid == pid
                or not isinstance(process["backend_authorized"], bool)
            ):
                return "invalid external process identity"
            previous_identity = external_identity_by_pid.get(external_pid)
            if previous_identity is not None:
                return "duplicate external process PID or conflicting identity"
            external_identity_by_pid[external_pid] = process_identity
            if external_pid in external_collision_tids:
                return "external process PID collides with vCPU/IO/unknown role TID"
            if process_identity in declared_identity_tuples:
                if process["backend_authorized"] is not True:
                    return "declared external backend is not authorized in process record"
                if external_pid not in external_backend_tids:
                    return "declared external backend lacks matching leader backend record"
            elif process["backend_authorized"] is not False:
                return "undeclared external process cannot be backend-authorized"
            external_process_ids.add(external_pid)
            try:
                affinity = _pin_cpu_array(process["affinity"], label="external process affinity")
            except BaselineError as error:
                return str(error)
            if not affinity:
                return "external process affinity is empty"
            if set(affinity) & set(report_measurement):
                # A passt/QEMU helper may be classified as a requested
                # backend/iothread.  Permit that only when its readback is
                # exactly one of the already-proven measurement records;
                # otherwise external work must stay off measurement CPUs.
                proven = role_tids.get(external_pid)
                if proven is None or tuple(affinity) != proven:
                    return "external process affinity overlaps measurement CPUs"
            if external_pid not in observed_tids:
                return "external process main TID lacks thread proof"
        if not expected_identity_tuples.issubset(
            {
                _pin_external_identity(
                    {
                        "pid": process["pid"],
                        "tgid": process["tgid"],
                        "exe": process["exe"],
                        "starttime": process["starttime"],
                    },
                    label="external process identity",
                )
                for process in external_processes
                if isinstance(process, dict)
            }
        ):
            return "declared external backend process was not observed"
        if payload["backend_status"] != ("not_requested" if not report_backend else "ok"):
            return f"backend pinning status is not ok: {payload['backend_status']!r}"
        qemu_main = payload["qemu_main"]
        if payload["qemu_main_status"] != "ok" or not isinstance(qemu_main, dict):
            return "QEMU main thread was not observed"
        if set(qemu_main) != PIN_THREAD_V3_KEYS:
            return "invalid pin report QEMU main record"
        main_tid = qemu_main["tid"]
        if isinstance(main_tid, bool) or not isinstance(main_tid, int) or main_tid <= 0:
            return "invalid pin report QEMU main TID"
        if main_tid != pid:
            return "QEMU main TID does not match report pid"
        try:
            main_affinity = _pin_cpu_array(qemu_main["affinity"], label="QEMU main affinity")
        except BaselineError as error:
            return str(error)
        if not main_affinity or not set(main_affinity).issubset(set(report_housekeeping)):
            return "QEMU main affinity is not housekeeping-only"
        observed_tids.add(main_tid)
        role_tids[main_tid] = main_affinity
        external_collision_tids.add(main_tid)
        if not is_v5 and not observed_tids.issubset(set(exit_readback_tids)):
            return "pin report has a task without final affinity readback"
        if payload["housekeeping_status"] != "ok":
            return f"housekeeping pinning status is not ok: {payload['housekeeping_status']!r}"
        if payload["unknown_status"] != "ok" or payload["unknown_off_measurement"] is not True:
            return "unknown thread placement is not proven off measurement CPUs"

    if payload["vcpu_status"] != "ok":
        return f"vCPU pinning status is not ok: {payload['vcpu_status']!r}"
    if payload["io_status"] != "ok":
        return f"IO pinning status is not ok: {payload['io_status']!r}"

    vcpu_threads = payload["vcpu_threads"]
    if not isinstance(vcpu_threads, dict):
        return "pin report vcpu_threads is not an object"
    expected_indices = {str(index) for index in range(expected_vcpu_count)}
    if set(vcpu_threads) != expected_indices:
        return (
            "pin report vCPU indices mismatch: "
            f"expected={sorted(expected_indices)} got={sorted(vcpu_threads)}"
        )
    tids: set[int] = set()
    for index in range(expected_vcpu_count):
        record = vcpu_threads[str(index)]
        expected_record_keys = PIN_ROLE_THREAD_V3_KEYS if is_v3 else PIN_THREAD_KEYS
        if not isinstance(record, dict) or set(record) != expected_record_keys:
            return f"invalid pin report vCPU record for index {index}"
        tid = record["tid"]
        if isinstance(tid, bool) or not isinstance(tid, int) or tid <= 0 or tid in tids:
            return f"invalid or duplicate vCPU TID for index {index}"
        if is_v3 and record["tgid"] != pid:
            return f"vCPU TID {tid} is not in the QEMU thread group"
        if is_v3 and tid in external_process_ids:
            return f"external process TID {tid} cannot satisfy vCPU role"
        tids.add(tid)
        try:
            affinity = _pin_cpu_array(record["affinity"], label=f"vCPU {index} affinity")
        except BaselineError as error:
            return str(error)
        expected_affinity = (requested_vcpu[index % len(requested_vcpu)],)
        if affinity != expected_affinity:
            return (
                f"vCPU {index} affinity mismatch: "
                f"expected={list(expected_affinity)} got={list(affinity)}"
            )

    io_threads = payload["io_threads"]
    if not isinstance(io_threads, list) or not io_threads:
        return "pin report has no dedicated IO thread"
    expected_io_affinity = tuple(sorted(requested_io))
    for record in io_threads:
        expected_record_keys = PIN_ROLE_THREAD_V3_KEYS if is_v3 else PIN_THREAD_KEYS
        if not isinstance(record, dict) or set(record) != expected_record_keys:
            return "invalid pin report IO thread record"
        tid = record["tid"]
        if isinstance(tid, bool) or not isinstance(tid, int) or tid <= 0 or tid in tids:
            return "invalid or duplicate IO thread TID"
        if is_v3 and record["tgid"] != pid:
            return f"IO TID {tid} is not in the QEMU thread group"
        if is_v3 and tid in external_process_ids:
            return f"external process TID {tid} cannot satisfy IO role"
        tids.add(tid)
        try:
            affinity = _pin_cpu_array(record["affinity"], label="IO thread affinity")
        except BaselineError as error:
            return str(error)
        if affinity != expected_io_affinity:
            return (
                "IO thread affinity mismatch: "
                f"expected={list(expected_io_affinity)} got={list(affinity)}"
            )
    if is_v3:
        backend_threads = payload["backend_threads"]
        if not isinstance(backend_threads, list):
            return "pin report backend_threads is not an array"
        expected_backend_affinity = tuple(sorted(report_backend))
        backend_tids = tids.copy()
        if main_tid in backend_tids:
            return "duplicate QEMU main TID"
        backend_tids.add(main_tid)
        for record in backend_threads:
            if not isinstance(record, dict) or set(record) != PIN_BACKEND_THREAD_V3_KEYS:
                return "invalid pin report backend thread record"
            tid = record["tid"]
            if isinstance(tid, bool) or not isinstance(tid, int) or tid <= 0 or tid in backend_tids:
                return "invalid or duplicate backend TID"
            tgid = record["tgid"]
            if isinstance(tgid, bool) or not isinstance(tgid, int) or tgid <= 0:
                return "invalid backend thread TGID"
            if tgid != pid:
                if tid != tgid:
                    return "external backend thread is not its declared process leader"
                process = next(
                    (
                        item
                        for item in external_processes
                        if isinstance(item, dict) and item.get("pid") == tgid
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
                        },
                        label="external backend identity",
                    )
                    if isinstance(process, dict)
                    else None
                )
                if (
                    identity is None
                    or identity not in declared_identity_tuples
                    or process.get("backend_authorized") is not True
                ):
                    return "backend thread lacks exact declared external identity"
            backend_tids.add(tid)
            try:
                affinity = _pin_cpu_array(record["affinity"], label="backend affinity")
            except BaselineError as error:
                return str(error)
            backend_allowed = (
                set(expected_backend_affinity)
                if expected_backend_affinity
                else set(report_housekeeping)
            )
            if not set(affinity).issubset(backend_allowed):
                return "backend thread affinity mismatch"
            if not expected_backend_affinity and set(affinity) & set(report_measurement):
                return "unrequested backend overlaps measurement CPUs"
        unknown_threads = payload["unknown_threads"]
        if not isinstance(unknown_threads, list):
            return "pin report unknown_threads is not an array"
        for record in unknown_threads:
            if not isinstance(record, dict) or set(record) != PIN_THREAD_V3_KEYS:
                return "invalid pin report unknown thread record"
            try:
                affinity = _pin_cpu_array(record["affinity"], label="unknown affinity")
            except BaselineError as error:
                return str(error)
            if not affinity or not set(affinity).issubset(set(report_housekeeping)):
                return "unknown thread affinity is not housekeeping-only"
            if set(affinity) & set(report_measurement):
                return "unknown thread affinity overlaps measurement CPUs"
            tid = record["tid"]
            if isinstance(tid, bool) or not isinstance(tid, int) or tid <= 0 or tid in backend_tids:
                return "invalid or duplicate unknown TID"
            backend_tids.add(tid)
    if is_v5:
        terminal_records = payload["terminal_proofs"]
        if not isinstance(terminal_records, list):
            return "invalid terminal proof records"
        terminal_by_tid: dict[int, Mapping[str, object]] = {}
        ptrace_terminal_tids: set[int] = set()
        for proof in terminal_records:
            if not isinstance(proof, dict):
                return "invalid terminal proof record"
            tid = proof.get("tid")
            if (
                isinstance(tid, bool) or not isinstance(tid, int) or tid <= 0
                or tid in terminal_by_tid
            ):
                return "invalid or duplicate terminal proof TID"
            method = proof.get("method")
            if method == "ptrace-exit-stop":
                if set(proof) != PIN_PTRACE_TERMINAL_PROOF_KEYS:
                    return "invalid ptrace exit terminal proof"
                ptrace_terminal_tids.add(tid)
            elif method == "kvm-vhost-stop":
                if set(proof) != PIN_KVM_TERMINAL_PROOF_KEYS:
                    return "invalid KVM vhost terminal proof"
                if (
                    proof.get("comm") != "kvm-nx-lpage-re"
                    or proof.get("tgid") != pid
                    or proof.get("user_worker") is not True
                    or proof.get("prearm_vcpus_housekeeping") is not True
                    or proof.get("worker_housekeeping") is not True
                    or proof.get("left_qemu_tgid") is not True
                ):
                    return "KVM vhost terminal lifecycle proof is incomplete"
            else:
                return "unsupported terminal proof method"
            try:
                proof_affinity = _pin_cpu_array(proof.get("affinity"), label="terminal proof affinity")
            except BaselineError as error:
                return str(error)
            role_affinity = role_tids.get(tid)
            if role_affinity is None:
                return "terminal proof has no observed task"
            if method == "kvm-vhost-stop":
                if not set(proof_affinity).issubset(set(report_housekeeping)):
                    return "KVM vhost terminal affinity is not housekeeping-only"
            elif tid in measurement_role_tids:
                if (
                    proof_affinity != role_affinity
                    and not set(proof_affinity).issubset(set(report_housekeeping))
                ):
                    return "measurement role terminal affinity is neither role nor housekeeping"
            elif not set(proof_affinity).issubset(set(report_housekeeping)):
                return "housekeeping task terminal affinity is not housekeeping-only"
            terminal_by_tid[tid] = proof
        if set(terminal_by_tid) != observed_tids:
            return "pin report terminal proofs do not cover exactly all observed tasks"
        if set(exit_readback_tids) != ptrace_terminal_tids:
            return "ptrace exit readback list does not match terminal proofs"
    return None


def _current_pin_schema_reason(path: Path) -> str | None:
    """Historical reports remain parseable but cannot admit a new run."""

    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError):
        return None  # The complete validator reports malformed/missing input.
    if not isinstance(payload, dict):
        return None
    schema = payload.get("schema")
    if schema != PIN_REPORT_SCHEMA:
        return (
            "current scheduler run requires "
            f"{PIN_REPORT_SCHEMA}, got {schema!r}"
        )
    return None


def _pin_report_shape_valid(payload: object) -> bool:
    """Check report structure before interpreting capability proof fields."""

    def affinity_shape(record: Mapping[str, object]) -> bool:
        value = record.get("affinity")
        return isinstance(value, list) and all(
            isinstance(cpu, int) and not isinstance(cpu, bool) and cpu >= 0
            for cpu in value
        )

    if not isinstance(payload, dict) or set(payload) != PIN_REPORT_V3_KEYS:
        return False
    proof_failures = payload.get("proof_failures")
    if not isinstance(proof_failures, list) or any(
        not isinstance(failure, dict)
        or not isinstance(failure.get("reason"), str)
        or not failure["reason"]
        or isinstance(failure.get("tid"), bool)
        or not isinstance(failure.get("tid"), int)
        or failure["tid"] <= 0
        for failure in proof_failures
    ):
        return False
    if not isinstance(payload.get("vcpu_threads"), dict):
        return False
    for record in payload["vcpu_threads"].values():
        if (
            not isinstance(record, dict)
            or set(record) != PIN_ROLE_THREAD_V3_KEYS
            or not affinity_shape(record)
        ):
            return False
    for field in ("io_threads", "backend_threads"):
        records = payload.get(field)
        if not isinstance(records, list):
            return False
        for record in records:
            if (
                not isinstance(record, dict)
                or set(record) != PIN_BACKEND_THREAD_V3_KEYS
                or not affinity_shape(record)
            ):
                return False
    records = payload.get("unknown_threads")
    if not isinstance(records, list):
        return False
    for record in records:
        if (
            not isinstance(record, dict)
            or set(record) != PIN_THREAD_V3_KEYS
            or not affinity_shape(record)
        ):
            return False
    external_processes = payload.get("external_processes")
    declarations = payload.get("declared_external_backends")
    if not isinstance(external_processes, list) or not isinstance(declarations, list):
        return False
    external_pids: set[int] = set()
    for process in external_processes:
        if not isinstance(process, dict) or set(process) != PIN_EXTERNAL_PROCESS_V3_KEYS:
            return False
        identity = _pin_external_identity({
            "pid": process.get("pid"),
            "tgid": process.get("tgid"),
            "exe": process.get("exe"),
            "starttime": process.get("starttime"),
        }, label="external process identity")
        if (
            identity is None
            or identity[0] in external_pids
            or not isinstance(process.get("backend_authorized"), bool)
            or not isinstance(process.get("affinity"), list)
            or any(
                isinstance(cpu, bool) or not isinstance(cpu, int) or cpu < 0
                for cpu in process["affinity"]
            )
        ):
            return False
        external_pids.add(identity[0])
    declared_pids: set[int] = set()
    for declaration in declarations:
        identity = _pin_external_identity(
            declaration, label="declared external backend identity"
        )
        if identity is None or identity[0] in declared_pids:
            return False
        declared_pids.add(identity[0])
    qemu_main = payload.get("qemu_main")
    if payload.get("qemu_main_status") == "not_observed":
        if qemu_main is not None:
            return False
    elif (
        not isinstance(qemu_main, dict)
        or set(qemu_main) != PIN_THREAD_V3_KEYS
        or not affinity_shape(qemu_main)
    ):
        return False
    for field in (
        "measurement_cpus", "measurement_smt_siblings", "housekeeping_cpus",
        "launcher_affinity", "exit_readback_tids",
    ):
        if not isinstance(payload.get(field), list):
            return False
    exit_tids = payload["exit_readback_tids"]
    if any(
        isinstance(tid, bool) or not isinstance(tid, int) or tid <= 0
        for tid in exit_tids
    ) or exit_tids != sorted(set(exit_tids)):
        return False
    return True


def pin_report_failure_status(path: Path, reason: str | None) -> str:
    """Classify a failed pin report without treating missing proof as spoofing.

    A runner can complete with status zero while the pinner observed no
    required task or could not establish ptrace/readback proof.  Those are
    explicit capability boundaries (``unsupported``).  A present, malformed,
    conflicting, or affinity-spoofed report remains a ``pinning-error``.
    """

    if reason is None:
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
    if not isinstance(payload, dict):
        return "pinning-error"
    if reason.startswith("current scheduler run requires "):
        return "unsupported"
    if not _pin_report_shape_valid(payload):
        return "pinning-error"
    for field in ("process_inherited_housekeeping", "new_threads_inherit_housekeeping"):
        if not isinstance(payload.get(field), bool):
            return "pinning-error"
    if not isinstance(payload.get("housekeeping_status"), str):
        return "pinning-error"
    capability_reasons = (
        "clone-event tracing proof is absent",
        "unknown-thread placement proof is unsupported",
        "final affinity readback proof is absent",
        "pin report has a task without final affinity readback",
        "QEMU main thread was not observed",
        "pin report has no dedicated IO thread",
        "vCPU pinning status is not ok: 'not_observed'",
        "IO pinning status is not ok: 'not_observed'",
        "backend pinning status is not ok: 'not_observed'",
    )
    if reason in capability_reasons:
        return "unsupported"
    if reason.startswith("pin report contains proof failure: "):
        return "unsupported"
    if reason == "launcher did not prove process housekeeping inheritance":
        return (
            "unsupported"
            if payload["process_inherited_housekeeping"] is False
            else "pinning-error"
        )
    if reason == "launcher did not prove new-thread housekeeping inheritance":
        return (
            "unsupported"
            if payload["new_threads_inherit_housekeeping"] is False
            else "pinning-error"
        )
    if reason == "housekeeping pinning status is not ok: 'not_reported'":
        return (
            "unsupported"
            if payload["housekeeping_status"] == "not_reported"
            else "pinning-error"
        )
    if reason == "invalid final affinity readback task list":
        exit_tids = payload.get("exit_readback_tids")
        if isinstance(exit_tids, list) and not exit_tids:
            return "unsupported"
    if (
        reason == "unknown thread placement is not proven off measurement CPUs"
        and payload.get("unknown_status") == "unsupported"
    ):
        return "unsupported"
    if reason.startswith("invalid pin report qemu_main") and payload.get("qemu_main_status") == "not_observed":
        return "unsupported"
    if reason.startswith("pin report vCPU indices mismatch"):
        vcpus = payload.get("vcpu_threads")
        expected_count = payload.get("expected_vcpu_count")
        if (
            payload.get("vcpu_status") == "not_observed"
            and isinstance(vcpus, dict)
            and isinstance(expected_count, int)
            and not isinstance(expected_count, bool)
            and expected_count > 0
            and set(vcpus).issubset({str(index) for index in range(expected_count)})
        ):
            return "unsupported"
    return "pinning-error"


def nearest_rank(values: Iterable[int], permille: int) -> int:
    ordered = sorted(values)
    if not ordered:
        raise BaselineError("cannot calculate a quantile for zero samples")
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


def _fields(line: str, prefix: str) -> dict[str, str] | None:
    line = line.strip()
    if not line.startswith(prefix + " "):
        return None
    result: dict[str, str] = {}
    for token in line.split()[1:]:
        key, separator, value = token.partition("=")
        if not separator or not key or not value or key in result:
            raise BaselineError(f"invalid {prefix} record: {line!r}")
        result[key] = value
    return result


def _positive(fields: dict[str, str], name: str) -> int:
    value = fields.get(name)
    if value is None or not value.isdecimal() or int(value) <= 0:
        raise BaselineError(f"invalid positive {name}: {value!r}")
    return int(value)


def _index(fields: dict[str, str], name: str) -> int:
    value = fields.get(name)
    if value is None or not value.isdecimal():
        raise BaselineError(f"invalid non-negative {name}: {value!r}")
    return int(value)


def _validate_optional_evidence(fields: dict[str, str], marker: str) -> None:
    if "path" in fields and not fields["path"]:
        raise BaselineError(f"{marker} has an empty path")
    if "oracle" in fields and not fields["oracle"]:
        raise BaselineError(f"{marker} has an empty oracle")
    for key in PMU_EVIDENCE_COLUMNS:
        if key in fields and (
            not fields[key].isdecimal() or int(fields[key], 10) < 0
        ):
            raise BaselineError(f"{marker} has invalid optional evidence {key}: {fields[key]!r}")


def _optional_row_fields(samples: Iterable[Sample]) -> dict[str, object]:
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


def parse_guest_log(path: Path, *, target: str, repeat: int) -> GuestRun:
    """Parse one scheduler helper run and admit only complete evidence.

    The helper emits raw samples as well as a compact RESULT summary.  The
    summary is part of the protocol, rather than an optional convenience: it
    carries the helper's checksum and catches truncated or spliced serial
    output that could otherwise leave a plausible-looking sample subset.
    """

    samples: list[Sample] = []
    workload: str | None = None
    placement: str | None = None
    run_fields: dict[str, str] | None = None
    expected_count: int | None = None
    expected_iterations: int | None = None
    result_fields: dict[str, str] | None = None
    result_status: str | None = None
    result_count: int | None = None
    result_checksum: int | None = None
    saw_done = False
    exit_status: int | None = None
    failure_reason: str | None = None
    seen_samples: set[tuple[int, int]] = set()

    def parse_decimal(fields: dict[str, str], key: str, *, positive: bool = False) -> int:
        value = fields.get(key)
        if value is None or not value.isdecimal() or (positive and int(value) <= 0):
            raise BaselineError(f"invalid {'positive ' if positive else ''}{key}: {value!r}")
        return int(value, 10)

    def exact(fields: dict[str, str], allowed: set[str], marker: str) -> None:
        unknown = sorted(set(fields) - allowed)
        # Path/oracle/PMU fields are an optional forward-compatible extension
        # of every evidence record; all protocol identity/stat fields remain
        # exact and required.
        missing = sorted((allowed - set(fields)) - set(OPTIONAL_EVIDENCE_COLUMNS))
        if unknown or missing:
            raise BaselineError(f"invalid {marker} fields: missing={missing} extra={unknown}")

    try:
        lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
    except OSError as error:
        return GuestRun(target, repeat, "unknown", "unknown", (), "launch-error", str(error))

    markers = {
        "SCHED_BASELINE_RUN",
        "SCHED_BASELINE_SAMPLE",
        "SCHED_BASELINE_RESULT",
        "SCHED_BASELINE_DONE",
        "SCHED_BASELINE_EXIT",
    }
    for line_number, line in enumerate(lines, 1):
        stripped = line.strip()
        marker = stripped.split(" ", 1)[0] if stripped else ""
        if marker.startswith("SCHED_BASELINE_") and marker not in markers:
            raise BaselineError(f"unknown scheduler marker {marker!r} at line {line_number}")
        if marker in markers and " " not in stripped:
            raise BaselineError(f"malformed {marker} marker at line {line_number}")
        if saw_done and marker != "SCHED_BASELINE_EXIT" and marker in markers:
            raise BaselineError(f"scheduler marker appears after DONE at line {line_number}")
        fields = _fields(line, "SCHED_BASELINE_RUN")
        if fields is not None:
            if run_fields is not None:
                raise BaselineError(f"duplicate SCHED_BASELINE_RUN in {path}")
            exact(
                fields,
                {
                    "schema", "arch", "workload", "placement", "iterations", "warmup",
                    "cpus", "cpu_work", *OPTIONAL_EVIDENCE_COLUMNS,
                },
                "SCHED_BASELINE_RUN",
            )
            _validate_optional_evidence(fields, "SCHED_BASELINE_RUN")
            if fields.get("schema") != RUN_SCHEMA:
                raise BaselineError(f"unsupported guest run schema in {path}")
            workload = fields.get("workload")
            placement = fields.get("placement")
            if workload not in WORKLOADS or placement not in PLACEMENTS:
                raise BaselineError(f"invalid guest run identity in {path}")
            if fields.get("arch") != "x86_64":
                raise BaselineError(f"invalid guest run architecture in {path}")
            iterations = parse_decimal(fields, "iterations", positive=True)
            expected_iterations = iterations
            parse_decimal(fields, "warmup")
            cpus = parse_decimal(fields, "cpus")
            if cpus <= 0:
                raise BaselineError("SCHED_BASELINE_RUN requires positive cpus")
            parse_decimal(fields, "cpu_work", positive=True)
            expected_count = iterations * (2 if workload == "cpu-worker" else 1)
            run_fields = fields
            continue
        fields = _fields(line, "SCHED_BASELINE_SAMPLE")
        if fields is not None:
            if run_fields is None:
                raise BaselineError(f"SCHED_BASELINE_SAMPLE appears before RUN at line {line_number}")
            if result_fields is not None:
                raise BaselineError(
                    f"SCHED_BASELINE_SAMPLE appears after RESULT at line {line_number}"
                )
            exact(
                fields,
                {
                    "schema", "workload", "placement", "worker", "sample", "latency_ns",
                    *OPTIONAL_EVIDENCE_COLUMNS,
                },
                "SCHED_BASELINE_SAMPLE",
            )
            _validate_optional_evidence(fields, "SCHED_BASELINE_SAMPLE")
            if fields.get("schema") != SAMPLE_SCHEMA:
                raise BaselineError(f"unsupported guest sample schema in {path}")
            sample_workload = fields.get("workload")
            sample_placement = fields.get("placement")
            if sample_workload not in WORKLOADS or sample_placement not in PLACEMENTS:
                raise BaselineError(f"invalid guest sample identity in {path}")
            if workload is not None and (sample_workload != workload or sample_placement != placement):
                raise BaselineError(f"guest sample identity mismatch in {path}")
            workload, placement = sample_workload, sample_placement
            worker = _index(fields, "worker")
            sample_index = _index(fields, "sample")
            expected_workers = 2 if workload == "cpu-worker" else 1
            if worker >= expected_workers:
                raise BaselineError(f"scheduler worker index is out of range: {worker}")
            if expected_iterations is None or sample_index >= expected_iterations:
                raise BaselineError(f"scheduler sample index is out of range: {sample_index}")
            if (worker, sample_index) in seen_samples:
                raise BaselineError(f"duplicate scheduler sample at line {line_number}")
            seen_samples.add((worker, sample_index))
            samples.append(
                Sample(
                    target,
                    repeat,
                    sample_workload,
                    sample_placement,
                    worker,
                    sample_index,
                    _positive(fields, "latency_ns"),
                    tuple(
                        (key, fields[key])
                        for key in OPTIONAL_EVIDENCE_COLUMNS
                        if fields.get(key, "") != ""
                    ),
                )
            )
            continue
        fields = _fields(line, "SCHED_BASELINE_RESULT")
        if fields is not None:
            if run_fields is None:
                raise BaselineError(f"SCHED_BASELINE_RESULT appears before RUN at line {line_number}")
            if result_fields is not None:
                raise BaselineError(f"duplicate SCHED_BASELINE_RESULT in {path}")
            status = fields.get("status")
            if status == "ok":
                exact(
                    fields,
                    {
                        "schema", "workload", "placement", "status", "count", "p50_ns",
                        "p99_ns", "p999_ns", "checksum", *OPTIONAL_EVIDENCE_COLUMNS,
                    },
                    "SCHED_BASELINE_RESULT",
                )
                _validate_optional_evidence(fields, "SCHED_BASELINE_RESULT")
                count = parse_decimal(fields, "count", positive=True)
                p50 = parse_decimal(fields, "p50_ns", positive=True)
                p99 = parse_decimal(fields, "p99_ns", positive=True)
                p999 = parse_decimal(fields, "p999_ns", positive=True)
                if p99 < p50 or p999 < p99:
                    raise BaselineError("SCHED_BASELINE_RESULT quantiles are not monotonic")
                checksum = parse_decimal(fields, "checksum")
                result_quantiles = (p50, p99, p999)
            elif status == "missing":
                exact(
                    fields,
                    {
                        "schema", "workload", "placement", "status", "count", "p50_ns",
                        "p99_ns", "p999_ns", "reason", "errno", *OPTIONAL_EVIDENCE_COLUMNS,
                    },
                    "SCHED_BASELINE_RESULT",
                )
                _validate_optional_evidence(fields, "SCHED_BASELINE_RESULT")
                count = parse_decimal(fields, "count")
                if count != 0 or any(fields[key] != "missing" for key in ("p50_ns", "p99_ns", "p999_ns")):
                    raise BaselineError("missing SCHED_BASELINE_RESULT has non-empty statistics")
                parse_decimal(fields, "errno")
                checksum = None
                failure_reason = fields["reason"]
            else:
                raise BaselineError(f"invalid SCHED_BASELINE_RESULT status: {status!r}")
            if fields.get("schema") != RESULT_SCHEMA:
                raise BaselineError(f"unsupported guest result schema in {path}")
            if fields.get("workload") != workload or fields.get("placement") != placement:
                raise BaselineError(f"guest result identity mismatch in {path}")
            result_fields = fields
            result_status = status
            result_count = count
            result_checksum = checksum
            continue
        fields = _fields(line, "SCHED_BASELINE_DONE")
        if fields is not None:
            if run_fields is None or result_fields is None:
                raise BaselineError(f"SCHED_BASELINE_DONE appears before RESULT at line {line_number}")
            if saw_done:
                raise BaselineError(f"duplicate SCHED_BASELINE_DONE in {path}")
            exact(
                fields,
                {"schema", "workload", "placement", *OPTIONAL_EVIDENCE_COLUMNS},
                "SCHED_BASELINE_DONE",
            )
            _validate_optional_evidence(fields, "SCHED_BASELINE_DONE")
            if fields.get("schema") != RUN_SCHEMA or fields.get("workload") != workload or fields.get("placement") != placement:
                raise BaselineError(f"guest DONE identity mismatch in {path}")
            saw_done = True
        fields = _fields(line, "SCHED_BASELINE_EXIT")
        if fields is not None:
            if not saw_done:
                raise BaselineError(f"SCHED_BASELINE_EXIT appears before DONE at line {line_number}")
            if exit_status is not None:
                raise BaselineError(f"duplicate SCHED_BASELINE_EXIT in {path}")
            exact(fields, {"status"}, "SCHED_BASELINE_EXIT")
            exit_status = parse_decimal(fields, "status")

    run_optional = {
        key: run_fields[key]
        for key in OPTIONAL_EVIDENCE_COLUMNS
        if run_fields is not None and key in run_fields
    }
    result_optional = {
        key: result_fields[key]
        for key in OPTIONAL_EVIDENCE_COLUMNS
        if result_fields is not None and key in result_fields
    }
    if run_optional or result_optional:
        merged_samples: list[Sample] = []
        for sample in samples:
            attributes = {
                **run_optional,
                **result_optional,
                **dict(sample.attributes),
            }
            merged_samples.append(
                Sample(
                    sample.target, sample.repeat, sample.workload, sample.placement,
                    sample.worker, sample.sample, sample.latency_ns,
                    tuple(sorted(attributes.items())),
                )
            )
        samples = merged_samples

    if workload is None or placement is None or run_fields is None:
        return GuestRun(target, repeat, "unknown", "unknown", (), "missing", "no_run_record", exit_status=exit_status)
    if result_fields is None:
        return GuestRun(target, repeat, workload, placement, tuple(samples), "incomplete", "missing_result", exit_status=exit_status)
    if not saw_done:
        return GuestRun(target, repeat, workload, placement, tuple(samples), "incomplete", "missing_done_marker", result_status, result_count, result_checksum, exit_status)
    if exit_status is None:
        return GuestRun(target, repeat, workload, placement, tuple(samples), "incomplete", "missing_exit_marker", result_status, result_count, result_checksum, None)
    if exit_status != 0:
        return GuestRun(target, repeat, workload, placement, tuple(samples), "incomplete", "nonzero_exit", result_status, result_count, result_checksum, exit_status)
    if expected_count is None or result_count != len(samples) or result_count != expected_count:
        return GuestRun(target, repeat, workload, placement, tuple(samples), "incomplete", "result_count_mismatch", result_status, result_count, result_checksum, exit_status)
    if result_status == "ok" and result_checksum != scheduler_sample_checksum(samples):
        return GuestRun(
            target,
            repeat,
            workload,
            placement,
            tuple(samples),
            "incomplete",
            "result_checksum_mismatch",
            result_status,
            result_count,
            result_checksum,
            exit_status,
        )
    if result_status == "ok" and result_quantiles is not None:
        measured = stats(sample.latency_ns for sample in samples)
        if tuple(measured[key] for key in ("p50_ns", "p99_ns", "p999_ns")) != result_quantiles:
            return GuestRun(
                target, repeat, workload, placement, tuple(samples), "incomplete",
                "result_quantile_mismatch", result_status, result_count,
                result_checksum, exit_status,
            )
    if result_status == "missing":
        return GuestRun(target, repeat, workload, placement, tuple(samples), "missing", failure_reason or "guest_missing", result_status, result_count, result_checksum, exit_status)
    return GuestRun(target, repeat, workload, placement, tuple(samples), "ok", None, result_status, result_count, result_checksum, exit_status)


def _write_tsv(path: Path, columns: tuple[str, ...], rows: Iterable[dict[str, object]]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.tmp")
    with temporary.open("w", encoding="utf-8", newline="") as stream:
        writer = csv.DictWriter(
            stream, fieldnames=columns, delimiter="\t", lineterminator="\n"
        )
        writer.writeheader()
        writer.writerows(rows)
    temporary.replace(path)


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
                    samples.append(
                        Sample(
                            row["target"], int(row["repeat"]), row["workload"],
                            row["placement"], int(row["worker"]), int(row["sample"]),
                            int(row["latency_ns"]),
                            tuple(
                                (key, row[key])
                                for key in OPTIONAL_EVIDENCE_COLUMNS
                                if row.get(key, "") != ""
                            ),
                        )
                    )
                    _validate_optional_evidence(
                        dict(samples[-1].attributes), f"raw sample row {line_number}"
                    )
                except (KeyError, ValueError) as error:
                    raise BaselineError(f"invalid raw sample row {line_number}: {error}") from error
    except OSError as error:
        raise BaselineError(f"cannot read raw samples: {error}") from error
    return tuple(samples)


def summarize_samples(samples: Iterable[Sample]) -> list[dict[str, object]]:
    all_samples = tuple(samples)
    grouped: dict[tuple[str, int, str, str], list[int]] = defaultdict(list)
    for sample in all_samples:
        grouped[(sample.target, sample.repeat, sample.workload, sample.placement)].append(
            sample.latency_ns
        )
    rows: list[dict[str, object]] = []
    for key in sorted(grouped):
        target, repeat, workload, placement = key
        values = grouped[key]
        measured = stats(values)
        rows.append(
            {
                "schema": SCHEMA,
                "target": target,
                "repeat": repeat,
                "workload": workload,
                "placement": placement,
                "status": "ok",
                **measured,
                **_optional_row_fields(
                    sample for sample in all_samples
                    if sample.target == target
                    and sample.repeat == repeat
                    and sample.workload == workload
                    and sample.placement == placement
                ),
            }
        )
    return rows


def stats_command(input_path: Path, output_path: Path, summary_tsv: Path | None) -> int:
    samples = _read_raw(input_path)
    rows = summarize_samples(samples)
    payload = {
        "schema": SCHEMA,
        "quantile": "nearest-rank",
        "raw_sample_count": len(samples),
        "measurement_status": "measured" if samples else "not-measured",
        "evidence_class": scheduler_evidence_class(samples),
        "optional_cost_metrics": list(PMU_EVIDENCE_COLUMNS),
        "optional_cost_policy": "record-when-available; missing-is-empty-not-zero",
        "cost_capabilities": host_cost_capabilities(),
        "runs": rows,
    }
    output_path.parent.mkdir(parents=True, exist_ok=True)
    temporary = output_path.with_name(f".{output_path.name}.tmp")
    temporary.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    temporary.replace(output_path)
    if summary_tsv is not None:
        _write_tsv(summary_tsv, SUMMARY_COLUMNS, rows)
    return 0 if samples else 1


def _require_file(path: Path, label: str) -> Path:
    path = path.expanduser().resolve()
    if not path.is_file() or path.stat().st_size == 0:
        raise BaselineError(f"{label} is missing or empty: {path}")
    return path


def _build_commands(args: argparse.Namespace, workload: str, placement: str) -> bytes:
    command = [
        args.guest_program,
        "--workload", workload,
        "--placement", placement,
        "--iterations", str(args.iterations),
        "--warmup", str(args.warmup),
        "--cpus", str(args.cpus),
    ]
    if args.cpu_work is not None:
        command.extend(("--cpu-work", str(args.cpu_work)))
    # shell-init starts a real interactive shell after the ready marker.
    # Linux may otherwise interleave an asynchronous printk record with a
    # latency marker on the same serial line.  Lower the runtime console level
    # only after boot/ready diagnostics have completed; TheKernel skips the
    # absent Linux proc control.  The parser remains strict if any output is
    # still corrupted after this measurement boundary.
    text = "if [ -w /proc/sys/kernel/printk ]; then echo 1 > /proc/sys/kernel/printk; fi\n"
    text += " ".join(command) + "\n"
    text += "rc=$?\necho SCHED_BASELINE_EXIT status=$rc\n"
    # QEMU's serial backend may still have helper records queued when the
    # shell receives the next command.  Give it a bounded drain interval so a
    # poweroff printk cannot splice into a latency record; the strict parser
    # remains the final guard against a genuinely incomplete drain.
    text += "/bin/busybox sleep 1\n"
    text += f"{args.shutdown_command}\n"
    return text.encode("utf-8")


def _target_images(args: argparse.Namespace, target: str) -> TargetImages:
    """Validate the explicit artifacts required by one boot lane.

    Linux is intentionally not an OVMF image tuple.  A bzImage can be passed
    to QEMU's direct-kernel path and uses the same ext4 rootfs as the workload
    helper, so requiring or accepting an ESP here would make the comparison
    less useful and would silently select the wrong boot path.
    """

    prefix = "linux_" if target == "linux" else ""
    kernel = getattr(args, prefix + "kernel")
    rootfs = getattr(args, prefix + "rootfs")
    if kernel is None or rootfs is None:
        raise BaselineError(f"{target} kernel/rootfs are not supplied; no download is attempted")
    if target == "linux":
        return TargetImages(
            kernel=_require_file(kernel, "linux kernel"),
            rootfs=_require_file(rootfs, "linux rootfs"),
            esp=None,
            initrd=(
                _require_file(args.linux_initrd, "linux initrd")
                if getattr(args, "linux_initrd", None) is not None
                else None
            ),
            direct_kernel=True,
            cmdline=getattr(args, "linux_cmdline", DEFAULT_LINUX_CMDLINE),
        )

    esp = getattr(args, "esp")
    if esp is None:
        raise BaselineError("thekernel ESP is not supplied; no download is attempted")
    return TargetImages(
        kernel=_require_file(kernel, "thekernel kernel"),
        rootfs=_require_file(rootfs, "thekernel rootfs"),
        esp=_require_file(esp, "thekernel ESP"),
        initrd=None,
        direct_kernel=False,
    )


def kvm_available() -> bool:
    """Return whether the host exposes the KVM execution device."""

    return Path("/dev/kvm").exists()


def _write_run_pmu(
    path: Path,
    *,
    capture: UserProcessTreePmu | None,
    unavailable: str | None,
) -> dict[str, object]:
    """Finish one PMU window and retain its non-sample-scoped evidence.

    These counters are deliberately kept out of raw-samples.tsv: they cover
    the controller's complete QEMU launch/boot/workload/shutdown interval and
    inherited descendants, not an individual guest operation.
    """

    payload: dict[str, object] = {
        "schema": "thekernel-host-pmu-run-v1",
        "scope": (
            "inherited process tree from the scheduler controller "
            "while one QEMU run is active; includes controller/pinner/QEMU descendants, "
            "boot and shutdown; exclude_guest=false so guest-mode user execution may contribute; "
            "not guest-scoped, QEMU-PID-exclusive, or per-sample"
        ),
        "exclude_kernel": True,
        "exclude_hv": True,
        "exclude_guest": False,
        "events": ["cycles", "instructions", "cache_misses", "branch_misses"],
    }
    if capture is None:
        payload.update({"status": "unavailable", "reason": unavailable or "not-started"})
    else:
        try:
            reading = capture.stop()
        except PmuUnavailable as error:
            payload.update({"status": "unavailable", "reason": str(error)})
        else:
            payload.update({
                "status": "measured",
                "raw_counters": reading.counters,
                "time_enabled_ns": reading.time_enabled,
                "time_running_ns": reading.time_running,
                "multiplexed": reading.multiplexed,
                "scale": reading.scale,
                "counter_scaling": "raw_with_scale_factor",
            })
        finally:
            capture.close()
    path.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return payload


def _latency_derived_throughput(samples: Iterable[Sample]) -> dict[str, object]:
    """Report only an explicit serial estimate derived from raw latency sums."""

    captured = tuple(samples)
    total_ns = sum(sample.latency_ns for sample in captured)
    if not captured or total_ns <= 0:
        return {"status": "not-available", "reason": "no-complete-raw-latency-samples"}
    return {
        "status": "latency-sum-derived",
        "operations": len(captured),
        "latency_sum_ns": total_ns,
        "operations_per_second": len(captured) * 1_000_000_000 / total_ns,
        "scope": (
            "serial estimate from sum(raw guest operation latency); it is not "
            "wall-clock QEMU throughput and does not model concurrent workers"
        ),
    }


def _same_evidence(expected: object, observed: object) -> bool:
    return (
        isinstance(expected, dict)
        and isinstance(observed, dict)
        and all(observed.get(key) == expected.get(key) for key in ("path", "size_bytes", "sha256"))
    )


def _validate_source_identity(identity: object) -> str | None:
    """Require the exact declared clean source combination at launch time."""

    if not isinstance(identity, dict) or identity.get("schema") != 1:
        return "invalid source identity"
    sources = identity.get("sources")
    if not isinstance(sources, dict):
        return "source identity has no source map"
    try:
        declared = source_combination.load(
            Path(__file__).resolve().parents[1] / "config" / "source-combination.toml"
        )
    except source_combination.SourceCombinationError as error:
        return f"cannot load declared source combination: {error}"
    expected_names = {"thekernel", *declared}
    if set(sources) != expected_names:
        return "source identity sources do not match declaration"
    thekernel = sources.get("thekernel")
    if not isinstance(thekernel, dict):
        return "source identity has invalid TheKernel source"
    for name in sorted(expected_names):
        source = sources.get(name)
        if not isinstance(source, dict):
            return f"source identity has invalid {name} source"
        if source.get("worktree_dirty") is not False:
            return f"source {name} is dirty"
        if source.get("match_declared") is not True:
            return f"source {name} does not match declaration"
        for field in ("repository_root", "commit", "tree"):
            if not isinstance(source.get(field), str) or not source[field]:
                return f"source {name} has invalid {field}"
        expected_root = (
            Path(__file__).resolve().parents[1]
            if name == "thekernel"
            else Path(__file__).resolve().parents[1].parent / declared[name].path
        )
        if source.get("repository_root") != str(expected_root.resolve()):
            return f"source {name} repository root does not match declaration"
        if name in declared and source.get("commit") != declared[name].ref:
            return f"source {name} commit does not match declaration"
    commit = thekernel.get("commit")
    if not isinstance(commit, str):
        return "source identity TheKernel commit is invalid"
    expected_combination = source_combination.combination_id(declared, commit)
    if identity.get("combination_id") != expected_combination:
        return "source identity combination ID does not match declaration"
    return None


def _capture_clean_source_identity() -> tuple[dict[str, object] | None, str | None]:
    try:
        identity = source_identity()
    except ReceiptError as error:
        return None, f"cannot capture source identity: {error}"
    reason = _validate_source_identity(identity)
    # Retain an invalid postflight snapshot for the manifest instead of
    # silently falling back to the last clean identity.  Callers still reject
    # it through ``reason``.
    return identity, reason


def _validate_run_input_evidence(
    receipt_path: Path,
    target_inputs: Mapping[str, object],
    images: TargetImages,
    preflight_identity: Mapping[str, object],
) -> tuple[str | None, dict[str, object] | None]:
    """Bind each run receipt and post-run files to its target manifest input.

    QEMU's receipt captures launch-time kernel/rootfs/ESP evidence and
    inherited-FD bindings for every file-valued launch input.
    """

    # Capture completion identity before any receipt/artifact rejection can
    # return. Every attempted run therefore has an actual source postflight,
    # not only runs whose artifact evidence was already valid.
    postflight_identity, postflight_reason = _capture_clean_source_identity()
    if postflight_reason is not None or postflight_identity is None:
        return f"source postflight failed: {postflight_reason}", postflight_identity
    if postflight_identity != preflight_identity:
        return "source identity changed during QEMU run", postflight_identity
    try:
        receipt = json.loads(receipt_path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        return f"missing or invalid qemu receipt: {error}", postflight_identity
    if not isinstance(receipt, dict):
        return "invalid qemu receipt object", postflight_identity
    receipt_identity = receipt.get("source_identity")
    source_identity_reason = _validate_source_identity(receipt_identity)
    if source_identity_reason is not None:
        return f"qemu receipt {source_identity_reason}", postflight_identity
    if receipt_identity != preflight_identity:
        return "qemu receipt source identity differs from preflight", postflight_identity
    receipt_fields = {"kernel": "kernel", "rootfs": "rootfs_source", "esp": "esp_source"}
    for artifact, receipt_field in receipt_fields.items():
        expected = target_inputs.get(artifact)
        if expected is None:
            if receipt_field in receipt:
                return f"qemu receipt unexpectedly contains {artifact} evidence", postflight_identity
            continue
        if not _same_evidence(expected, receipt.get(receipt_field)):
            return f"qemu receipt {artifact} evidence does not match target manifest", postflight_identity
    if images.initrd is not None:
        expected_initrd = target_inputs.get("initrd")
        handles = receipt.get("launch_handles")
        if not isinstance(handles, dict) or not isinstance(expected_initrd, dict):
            return "invalid target initrd evidence", postflight_identity
        binding = handles.get("initrd")
        if not isinstance(binding, dict) or not _same_evidence(expected_initrd, binding.get("source")):
            return "qemu receipt initrd handle does not match target manifest", postflight_identity
    for artifact, path in (
        ("kernel", images.kernel), ("rootfs", images.rootfs),
        ("esp", images.esp), ("initrd", images.initrd),
    ):
        if path is not None and not _same_evidence(target_inputs.get(artifact), file_evidence(path)):
            return f"target {artifact} changed during or after QEMU run", postflight_identity
    return None, postflight_identity


def run_command(args: argparse.Namespace) -> int:
    if not kvm_available():
        print("kvm-scheduler-baseline: UNSUPPORTED: /dev/kvm is unavailable", file=sys.stderr)
        return 78
    qemu = shutil.which(args.qemu_binary or "qemu-system-x86_64")
    if qemu is None:
        raise BaselineError("qemu-system-x86_64 is unavailable")
    if not args.vcpu_cpus or not args.io_cpus:
        raise BaselineError(
            "formal scheduler lanes require explicit --vcpu-cpus and --io-cpus; "
            "automatic hybrid-CPU selection is not permitted"
        )
    topology = read_host_topology()
    allowed = set(os.sched_getaffinity(0)) & set(topology.online)
    vcpu_cpus = parse_cpu_list(args.vcpu_cpus, allowed=allowed)
    io_cpus = parse_cpu_list(args.io_cpus, allowed=allowed)
    backend_cpus = parse_cpu_list(args.backend_cpus, allowed=allowed) if args.backend_cpus else ()
    if len(vcpu_cpus) != args.cpus:
        raise BaselineError(
            f"--vcpu-cpus must contain exactly --cpus ({args.cpus}) CPUs"
        )
    validate_cpu_selection(vcpu_cpus, topology)
    validate_cpu_selection(io_cpus, topology)
    if backend_cpus:
        validate_cpu_selection(backend_cpus, topology)
    validate_cpu_roles(
        {
            "vCPU": vcpu_cpus,
            "IO": io_cpus,
            **({"backend": backend_cpus} if backend_cpus else {}),
        },
        topology,
    )
    measurement_cpus = set(vcpu_cpus) | set(io_cpus) | set(backend_cpus)
    housekeeping_cpus = choose_housekeeping_cpus(
        args.housekeeping_cpus,
        allowed=allowed,
        measurement=measurement_cpus,
        topology=topology,
    )
    if not housekeeping_cpus:
        raise BaselineError("no housekeeping CPU remains outside measurement CPUs")
    if set(housekeeping_cpus) & measurement_cpus:
        raise BaselineError("housekeeping CPUs overlap scheduler measurement CPUs")
    vcpu_class = _measurement_class(vcpu_cpus, topology)
    io_class = _measurement_class(io_cpus, topology)
    backend_class = _measurement_class(backend_cpus, topology) if backend_cpus else None
    housekeeping_classes = {
        record["selection_class"]
        for record in host_topology_manifest(topology)
        if record["cpu"] in housekeeping_cpus
    }
    output = args.output.expanduser().resolve()
    output.mkdir(parents=True, exist_ok=True)
    pinner = Path(__file__).with_name("kvm_scheduler_pinner.py").resolve()
    raw_samples: list[Sample] = []
    runs: list[dict[str, object]] = []
    target_manifests: dict[str, dict[str, object]] = {}
    overall_status = 0
    targets = ("thekernel", "linux") if args.target == "both" else (args.target,)
    images_by_target: dict[str, TargetImages] = {}
    for target in targets:
        try:
            images_by_target[target] = _target_images(args, target)
        except BaselineError as error:
            if target == "linux" and args.target == "both":
                # A requested comparison with no Linux artifact is a failed
                # comparison, not a successful TheKernel-only run.
                overall_status = 1
                reason = str(error)
                runs.append({"target": target, "status": "unavailable", "reason": reason})
                target_manifests[target] = {
                    "status": "unavailable",
                    "reason": reason,
                    "boot": None,
                    "kernel": None,
                    "rootfs": None,
                    "esp": None,
                }
                continue
            raise
    for target, images in images_by_target.items():
        target_manifest = {
            # Paths alone do not bind a repeat to immutable boot inputs.
            # Capture a stable, change-detecting content identity once before
            # the first target run; runner receipts retain their own runtime
            # drive evidence separately.
            "kernel": file_evidence(images.kernel),
            "rootfs": file_evidence(images.rootfs),
            "esp": file_evidence(images.esp) if images.esp is not None else None,
            "initrd": file_evidence(images.initrd) if images.initrd is not None else None,
            "boot": "direct-kernel" if images.direct_kernel else "uefi",
            "firmware": None if images.direct_kernel else "OVMF",
        }
        if images.cmdline is not None:
            target_manifest["cmdline"] = images.cmdline
        target_manifests[target] = target_manifest
    source_preflight, source_preflight_reason = _capture_clean_source_identity()
    if source_preflight_reason is not None or source_preflight is None:
        raise BaselineError(f"source preflight failed: {source_preflight_reason}")
    source_postflight: dict[str, object] | None = source_preflight
    source_identity_unchanged = True
    # Keep target ordering inside the repeat loop.  Running all repeats for one
    # target first creates a systematic time/thermal drift in ``--target both``
    # comparisons.
    for repeat in range(1, args.repeat + 1):
        for target in targets:
            images = images_by_target.get(target)
            if images is None:
                continue
            for workload in args.workloads:
                for placement in args.placements:
                    run_dir = output / target / f"repeat-{repeat:03d}-{workload}-{placement}"
                    run_dir.mkdir(parents=True, exist_ok=True)
                    commands_path = run_dir / "commands"
                    commands_path.write_bytes(_build_commands(args, workload, placement))
                    pin_report = run_dir / "thread-pinning.json"
                    env = os.environ.copy()
                    env.update({
                        "THEKERNEL_KVM_QEMU": qemu,
                        "THEKERNEL_KVM_VCPU_CPUS": ",".join(map(str, vcpu_cpus)),
                        "THEKERNEL_KVM_IO_CPUS": ",".join(map(str, io_cpus)),
                        "THEKERNEL_KVM_BACKEND_CPUS": ",".join(map(str, backend_cpus)),
                        "THEKERNEL_KVM_HOUSEKEEPING_CPUS": ",".join(map(str, housekeeping_cpus)),
                        "THEKERNEL_KVM_VCPU_COUNT": str(args.cpus),
                        "THEKERNEL_KVM_PIN_REPORT": str(pin_report),
                        # Confirmed pinner path: it keeps initial vCPUs on
                        # housekeeping until the KVM NX recovery worker has
                        # been observed and pinned, then releases them.
                        "THEKERNEL_KVM_PREARM_KVM_NX_WORKER": "1",
                    })
                    previous = {key: os.environ.get(key) for key in env if key.startswith("THEKERNEL_KVM_")}
                    os.environ.update({key: value for key, value in env.items() if key.startswith("THEKERNEL_KVM_")})
                    pmu_path = run_dir / "host-pmu.json"
                    pmu_capture: UserProcessTreePmu | None = None
                    pmu_unavailable: str | None = None
                    try:
                        extra_args = [
                            "-name",
                            "guest=scheduler-baseline,debug-threads=on",
                        ]
                        if images.direct_kernel:
                            # ``-kernel`` accepts Linux's bzImage directly;
                            # the rootfs remains the runner's explicit
                            # virtio-blk vda image.
                            extra_args[0:0] = [
                                "-append",
                                images.cmdline or DEFAULT_LINUX_CMDLINE,
                            ]
                            if images.initrd is not None:
                                extra_args[0:0] = ["-initrd", str(images.initrd)]
                        try:
                            pmu_capture = UserProcessTreePmu()
                            pmu_capture.open()
                            pmu_capture.start()
                        except PmuUnavailable as error:
                            if pmu_capture is not None:
                                pmu_capture.close()
                            pmu_capture = None
                            pmu_unavailable = str(error)
                        try:
                            result = run(
                                RunConfig(
                                arch="x86_64", kernel=images.kernel, rootfs=images.rootfs,
                                esp=images.esp, direct_kernel=images.direct_kernel,
                                workdir=run_dir, log_path=run_dir / "console.log",
                                memory=args.memory,
                                cpus=args.cpus, qemu_binary=str(pinner), accel="kvm",
                                cpu="host", iothread_id="baseline-io",
                                extra_args=tuple(extra_args),
                                receipt_path=run_dir / "qemu-receipt.json",
                                input_path=commands_path,
                                limits=RunLimits(total_timeout_secs=args.timeout,
                                                  ready_timeout_secs=args.ready_timeout),
                                interaction=Interaction(interactive=True,
                                                        input_after_marker=args.ready_marker),
                                ovmf_code=Path(args.ovmf_code).resolve() if args.ovmf_code else None,
                                ovmf_vars=Path(args.ovmf_vars).resolve() if args.ovmf_vars else None,
                                ),
                            )
                        finally:
                            pmu_evidence = _write_run_pmu(
                                pmu_path, capture=pmu_capture, unavailable=pmu_unavailable
                            )
                    finally:
                        for key in ("THEKERNEL_KVM_QEMU", "THEKERNEL_KVM_VCPU_CPUS",
                                    "THEKERNEL_KVM_IO_CPUS", "THEKERNEL_KVM_BACKEND_CPUS",
                                    "THEKERNEL_KVM_HOUSEKEEPING_CPUS", "THEKERNEL_KVM_VCPU_COUNT",
                                    "THEKERNEL_KVM_PIN_REPORT", "THEKERNEL_KVM_PREARM_KVM_NX_WORKER"):
                            if previous.get(key) is None:
                                os.environ.pop(key, None)
                            else:
                                os.environ[key] = previous[key]  # type: ignore[index]
                    try:
                        expected_external_backends = parse_external_backend_identities()
                    except BackendIdentityUnavailable as error:
                        pinning_reason = f"invalid external backend identity: {error}"
                    else:
                        pinning_reason = validate_pin_report(
                            pin_report,
                            expected_pid=None,
                            expected_vcpu_count=args.cpus,
                            requested_vcpu=vcpu_cpus,
                            requested_io=io_cpus,
                            requested_backend=backend_cpus,
                            expected_external_backends=expected_external_backends,
                        )
                        if pinning_reason is None:
                            pinning_reason = _current_pin_schema_reason(pin_report)
                    guest = parse_guest_log(run_dir / "console.log", target=target, repeat=repeat)
                    input_evidence_reason, run_postflight_identity = _validate_run_input_evidence(
                        run_dir / "qemu-receipt.json", target_manifests[target], images,
                        source_preflight,
                    )
                    source_postflight = run_postflight_identity
                    if run_postflight_identity != source_preflight:
                        source_identity_unchanged = False
                    measured = (
                        pinning_reason is None
                        and result.returncode == 0
                        and guest.status == "ok"
                        and input_evidence_reason is None
                    )
                    evidence_class = scheduler_evidence_class(
                        guest.samples if measured else ()
                    )
                    host_pmu_measured = pmu_evidence.get("status") == "measured"
                    # The PMU is run-scoped and never upgrades a per-sample
                    # witness.  Together these files are only a prerequisite
                    # raw-data collection record, never formal performance
                    # evidence or a performance-change gate.
                    collected = measured and host_pmu_measured
                    if measured:
                        raw_samples.extend(guest.samples)
                    pin_status = pin_report_failure_status(pin_report, pinning_reason)
                    if result.returncode == 78 and pin_status == "pinning-error":
                        status = "pinning-error"
                        reason = pinning_reason
                    elif result.returncode == 78:
                        # The pinner reserves 78 for a formal capability
                        # boundary (notably ptrace clone-event permission).
                        # Preserve that distinction in the manifest instead
                        # of reducing it to a generic runner failure.
                        status = "unsupported"
                        reason = pinning_reason or "thread-pinning-unsupported"
                    elif pinning_reason is not None:
                        status = pin_status
                        reason = pinning_reason
                    elif input_evidence_reason is not None:
                        status = "input-evidence-error"
                        reason = input_evidence_reason
                    else:
                        status = guest.status if result.returncode == 0 else "runner-error"
                        reason = guest.reason if status != "ok" else None
                    if collected:
                        status = "collected"
                        reason = None
                    elif measured:
                        status = "collection-incomplete"
                        reason = "missing-run-scoped-pmu-evidence"
                    runs.append({"target": target, "repeat": repeat, "workload": workload,
                                 "placement": placement, "status": status,
                                  "measurement_complete": measured,
                                 "guest_status": guest.status,
                                 "formal_performance_evidence": False,
                                 "performance_change_gate_eligible": False,
                                 "evidence_class": evidence_class,
                                 "combined_evidence_class": (
                                     "latency-plus-run-scoped-pmu-prerequisite"
                                     if collected else evidence_class
                                 ),
                                 "reason": reason,
                                 "returncode": result.returncode,
                                 "pin_report": str(pin_report.relative_to(output)),
                                 "qemu_receipt": str((run_dir / "qemu-receipt.json").relative_to(output)),
                                 "input_evidence": "ok" if input_evidence_reason is None else "failed",
                                 "host_pmu": str(pmu_path.relative_to(output)),
                                 "host_pmu_status": pmu_evidence.get("status"),
                                 "throughput": _latency_derived_throughput(
                                     guest.samples if measured else ()
                                 ),
                                 "log": str((run_dir / "console.log").relative_to(output))})
                    if status not in {"collected"}:
                        overall_status = 1
    _write_tsv(output / "raw-samples.tsv", RAW_COLUMNS, ({
        "schema": SCHEMA, "target": sample.target, "repeat": sample.repeat,
        "workload": sample.workload, "placement": sample.placement,
        "worker": sample.worker, "sample": sample.sample,
        "latency_ns": sample.latency_ns,
        **_optional_row_fields((sample,)),
    } for sample in raw_samples))
    if stats_command(
        output / "raw-samples.tsv", output / "summary.json", output / "summary.tsv"
    ) != 0:
        overall_status = 1
    cost_capabilities = host_cost_capabilities()
    collection_complete = bool(runs) and all(
        run.get("status") == "collected" for run in runs
    )
    manifest = {"schema": MANIFEST_SCHEMA, "qemu": qemu, "accel": "kvm", "cpu": "host",
                "machine": "q35", "cpus": args.cpus,
                "memory": args.memory, "warmup": args.warmup, "repeat": args.repeat,
                "vcpu_cpus": list(vcpu_cpus), "io_cpus": list(io_cpus),
                "backend_cpus": list(backend_cpus),
                "housekeeping_cpus": list(housekeeping_cpus),
                "cpu_selection": {
                    "policy": "explicit-core-cache-maxfreq-validation",
                    "vcpu_cpus_explicit": True,
                    "io_cpus_explicit": True,
                    "backend_cpus_explicit": bool(args.backend_cpus),
                    "housekeeping_cpus_explicit": bool(args.housekeeping_cpus),
                    "vcpu_class": vcpu_class,
                    "io_class": io_class,
                    "backend_class": backend_class,
                    "housekeeping_class": (
                        next(iter(housekeeping_classes))
                        if len(housekeeping_classes) == 1 else "mixed"
                    ),
                },
                "host_cpu_topology": host_topology_manifest(topology),
                "measurement_status": "measured" if raw_samples else "not-measured",
                "evidence_class": (
                    "latency-plus-run-scoped-pmu-prerequisite"
                    if collection_complete
                    else scheduler_evidence_class(raw_samples)
                ),
                "host_pmu_scope": (
                    "per-run inherited process-tree counters over the QEMU lifetime; "
                    "exclude_guest=false, so guest-mode user execution may contribute; "
                    "never copied into guest samples or summary quantiles"
                ),
                "formal_performance_evidence": False,
                "performance_change_gate_eligible": False,
                "collection_status": "collected" if collection_complete else "incomplete",
                "source_identity": {
                    "preflight": source_preflight,
                    "postflight": source_postflight,
                    "combination_id": source_preflight["combination_id"],
                    "status": (
                        "unchanged-clean"
                        if source_identity_unchanged else "changed-or-incomplete"
                    ),
                },
                "cost_capabilities": cost_capabilities,
                "boot_comparison": (
                    "TheKernel uses UEFI/OVMF; Linux uses direct bzImage when selected; "
                    "boot is outside measurement windows"
                ),
                "targets": target_manifests,
                "runs": runs}
    (output / "manifest.json").write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return overall_status


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="kvm-scheduler-baseline")
    subparsers = parser.add_subparsers(dest="command", required=True)
    run_parser = subparsers.add_parser("run", help="run explicit KVM guest baselines")
    run_parser.add_argument("--target", choices=("thekernel", "linux", "both"), default="thekernel")
    run_parser.add_argument("--kernel", type=Path)
    run_parser.add_argument("--rootfs", type=Path)
    run_parser.add_argument("--esp", type=Path)
    run_parser.add_argument("--linux-kernel", dest="linux_kernel", type=Path)
    run_parser.add_argument("--linux-rootfs", dest="linux_rootfs", type=Path)
    run_parser.add_argument("--linux-initrd", dest="linux_initrd", type=Path)
    run_parser.add_argument(
        "--linux-cmdline",
        default=DEFAULT_LINUX_CMDLINE,
        help="Linux direct-kernel command line (default: %(default)s)",
    )
    run_parser.add_argument("--output", type=Path, required=True)
    run_parser.add_argument("--guest-program", default="/opt/thekernel-tests/bin/thekernel-scheduler-baseline")
    run_parser.add_argument("--ready-marker", default="THEKERNEL_SHELL_READY")
    run_parser.add_argument("--shutdown-command", default="/bin/busybox poweroff -f")
    run_parser.add_argument("--workloads", nargs="+", choices=WORKLOADS, default=list(WORKLOADS))
    run_parser.add_argument("--placements", nargs="+", choices=PLACEMENTS, default=list(PLACEMENTS))
    run_parser.add_argument("--iterations", type=int, default=1000)
    run_parser.add_argument("--warmup", type=int, default=100)
    run_parser.add_argument("--repeat", type=int, default=3)
    run_parser.add_argument("--cpu-work", type=int)
    run_parser.add_argument("--cpus", type=int, default=4)
    run_parser.add_argument("--memory", default="128M")
    run_parser.add_argument("--timeout", type=float, default=120.0)
    run_parser.add_argument("--ready-timeout", type=float, default=60.0)
    run_parser.add_argument("--vcpu-cpus")
    run_parser.add_argument("--io-cpus")
    run_parser.add_argument("--backend-cpus")
    run_parser.add_argument("--housekeeping-cpus")
    run_parser.add_argument("--qemu-binary")
    run_parser.add_argument("--ovmf-code")
    run_parser.add_argument("--ovmf-vars")
    run_parser.set_defaults(func=run_command)
    stats_parser = subparsers.add_parser("stats", help="recompute quantiles from raw samples")
    stats_parser.add_argument("--input", type=Path, required=True)
    stats_parser.add_argument("--output", type=Path, required=True)
    stats_parser.add_argument("--summary-tsv", type=Path)
    stats_parser.set_defaults(func=lambda args: stats_command(args.input, args.output, args.summary_tsv))
    return parser


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    try:
        args = parser.parse_args(argv)
        if args.command == "run":
            if args.iterations <= 0 or args.warmup < 0 or args.repeat <= 0 or args.cpus <= 0:
                parser.error("iterations/repeat/cpus must be positive and warmup non-negative")
        return int(args.func(args))
    except TopologyUnavailable as error:
        print(f"kvm-scheduler-baseline: UNSUPPORTED: {error}", file=sys.stderr)
        return 78
    except (BaselineError, RunnerError, OSError) as error:
        print(f"kvm-scheduler-baseline: ERROR: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
