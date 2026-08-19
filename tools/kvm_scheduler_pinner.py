#!/usr/bin/env python3
"""Launch QEMU and make its thread placement auditable.

QEMU does not have a stable command-line CPU-affinity option.  With
``debug-threads=on`` its KVM, IO, and backend thread names are observable in
``/proc``; this wrapper applies Linux thread affinity after launch and records
the read-back mask for every thread that it sees.

The important distinction here is between *measurement* threads (vCPUs and
the explicitly requested IO/backend workers) and all of QEMU's other work.
The launcher, QEMU main thread, and unknown workers stay on housekeeping
CPUs.  The report records unknown workers and proves that their read-back
affinity is disjoint from the measurement CPUs.  A missing thread is reported
as ``not_observed`` rather than being presented as a successful pin.
Housekeeping selection also excludes every kernel-reported SMT sibling of a
measurement CPU, including when this standalone launcher is invoked without
the baseline wrapper.
"""

from __future__ import annotations

import json
import ctypes
import ctypes.util
import errno
import os
import re
import resource
import signal
import sys
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Mapping


PIN_REPORT_SCHEMA = "thekernel-kvm-thread-pinning-v4"
LEGACY_PIN_REPORT_SCHEMA = "thekernel-kvm-thread-pinning-v2"
CPU_RE = re.compile(r"(?:CPU\s+|vcpu[ -]?|VCPU[ -]?)(?P<index>\d+)", re.IGNORECASE)
PF_USER_WORKER = 0x00004000
KVM_NX_RECOVERY_COMM = "kvm-nx-lpage-re"


@dataclass
class KvmNxPrearm:
    """Two-phase vCPU placement while KVM creates its untraced NX worker."""

    enabled: bool
    deadline: float
    worker_tid: int | None = None

    @property
    def armed(self) -> bool:
        return not self.enabled or self.worker_tid is not None

    def vcpu_cpus(
        self, requested: tuple[int, ...], housekeeping: tuple[int, ...], index: int
    ) -> tuple[int, ...]:
        if self.enabled and not self.armed:
            return housekeeping
        return (requested[index % len(requested)],) if requested else housekeeping

    def observe_worker(
        self,
        tid: int,
        affinity: tuple[int, ...],
        *,
        housekeeping: tuple[int, ...],
        measurement: set[int],
    ) -> str | None:
        if not self.enabled:
            return None
        if self.worker_tid is not None and self.worker_tid != tid:
            return "new-worker-after-arm"
        if not affinity or not set(affinity).issubset(set(housekeeping)):
            return "worker-not-on-housekeeping"
        if set(affinity) & measurement:
            return "worker-overlaps-measurement"
        self.worker_tid = tid
        return None

    def timed_out(self, now: float) -> bool:
        return self.enabled and not self.armed and now >= self.deadline

# A 10ms /proc poll cannot prove where a short-lived QEMU worker ran.  The
# launcher therefore starts QEMU under ptrace and handles clone/fork/vfork
# stops.  Linux keeps a newly created task stopped until the tracer resumes
# it, which lets us put it on housekeeping CPUs before its first instruction.
PTRACE_TRACEME = 0
PTRACE_CONT = 7
PTRACE_SETOPTIONS = 0x4200
PTRACE_GETEVENTMSG = 0x4201
PTRACE_O_TRACEFORK = 1 << 1
PTRACE_O_TRACEVFORK = 1 << 2
PTRACE_O_TRACECLONE = 1 << 3
PTRACE_O_TRACEEXEC = 1 << 4
PTRACE_O_TRACEEXIT = 1 << 6
PTRACE_O_EXITKILL = 1 << 20
PTRACE_EVENT_FORK = 1
PTRACE_EVENT_VFORK = 2
PTRACE_EVENT_CLONE = 3
PTRACE_EVENT_EXEC = 4
PTRACE_EVENT_EXIT = 6
WALL = getattr(os, "__WALL", 0x40000000)
_LIBC = ctypes.CDLL(ctypes.util.find_library("c") or "libc.so.6", use_errno=True)
_LIBC.ptrace.argtypes = [ctypes.c_ulong, ctypes.c_long, ctypes.c_void_p, ctypes.c_void_p]
_LIBC.ptrace.restype = ctypes.c_long


class PtraceUnavailable(RuntimeError):
    """Raised when ptrace task-event tracing cannot establish the proof."""


class CpuTopologyUnavailable(ValueError):
    """Raised when SMT sibling topology cannot establish placement proof."""


class BackendIdentityUnavailable(ValueError):
    """Raised when an external backend identity contract is invalid."""


BACKEND_IDENTITY_KEYS = frozenset({"pid", "tgid", "exe", "starttime"})


class TracedProcess:
    """Small pid handle for a child launched before ``execve``.

    ``subprocess.Popen(preexec_fn=...)`` cannot be used here: Popen waits for
    the child to exec before returning, while clone-event setup requires the
    child to stop after ``PTRACE_TRACEME`` and before exec.  A direct fork
    gives the tracer that ordering without a polling window.
    """

    def __init__(self, pid: int):
        self.pid = pid

    def kill(self) -> None:
        os.kill(self.pid, signal.SIGKILL)

    def wait(self) -> int:
        waited, status = os.waitpid(self.pid, 0)
        if waited != self.pid:
            raise ChildProcessError(f"waited for unexpected pid {waited}")
        if os.WIFEXITED(status):
            return os.WEXITSTATUS(status)
        if os.WIFSIGNALED(status):
            return -os.WTERMSIG(status)
        return 78

    def poll(self) -> int | None:
        try:
            waited, status = os.waitpid(self.pid, WALL | os.WNOHANG)
        except ChildProcessError:
            return 78
        if waited == 0:
            return None
        if os.WIFEXITED(status):
            return os.WEXITSTATUS(status)
        if os.WIFSIGNALED(status):
            return -os.WTERMSIG(status)
        return None


def _ptrace(request: int, pid: int, data: int = 0) -> int:
    result = _LIBC.ptrace(
        ctypes.c_ulong(request),
        ctypes.c_long(pid),
        ctypes.c_void_p(0),
        ctypes.c_void_p(data),
    )
    if result == -1:
        error = ctypes.get_errno()
        raise OSError(error, os.strerror(error))
    return int(result)


def _ptrace_event_message(pid: int) -> int:
    message = ctypes.c_ulonglong(0)
    result = _LIBC.ptrace(
        ctypes.c_ulong(PTRACE_GETEVENTMSG),
        ctypes.c_long(pid),
        ctypes.c_void_p(0),
        ctypes.cast(ctypes.pointer(message), ctypes.c_void_p),
    )
    if result == -1:
        error = ctypes.get_errno()
        raise OSError(error, os.strerror(error))
    return int(message.value)


def _trace_exec_child() -> None:
    """Popen child hook: stop before exec so the parent can set ptrace opts."""

    try:
        _ptrace(PTRACE_TRACEME, 0)
        os.kill(os.getpid(), signal.SIGSTOP)
    except OSError:
        # There is no safe fallback: returning from this hook would execute
        # QEMU without clone-event coverage.
        os._exit(78)


def _start_traced(command: list[str], housekeeping: tuple[int, ...]) -> TracedProcess:
    process: TracedProcess | None = None
    try:
        pid = os.fork()
        if pid == 0:
            _trace_exec_child()
            try:
                os.execv(command[0], command)
            except OSError:
                os._exit(78)
        process = TracedProcess(pid)
        _, status = os.waitpid(process.pid, 0)
        if not os.WIFSTOPPED(status):
            process.wait()
            raise PtraceUnavailable("QEMU did not stop before exec for ptrace setup")
        _ptrace(
            PTRACE_SETOPTIONS,
            process.pid,
            PTRACE_O_TRACEFORK
            | PTRACE_O_TRACEVFORK
            | PTRACE_O_TRACECLONE
            | PTRACE_O_TRACEEXEC
            | PTRACE_O_TRACEEXIT
            | PTRACE_O_EXITKILL,
        )
        # The launcher itself was moved to housekeeping before this call; the
        # child inherited that mask.  Keep the explicit argument to make the
        # pre-exec invariant auditable at the call site.
        del housekeeping
        _ptrace(PTRACE_CONT, process.pid)
        return process
    except OSError as error:
        if process is not None and process.poll() is None:
            try:
                process.kill()
            except OSError:
                pass
            try:
                process.wait()
            except (ChildProcessError, OSError):
                pass
        raise PtraceUnavailable(f"ptrace clone-event setup unavailable: {error}") from error


def _parse_cpu_set(value: str, *, label: str) -> tuple[int, ...]:
    value = value.strip()
    if not value:
        return ()
    result: list[int] = []
    for item in value.split(","):
        item = item.strip()
        if not item:
            raise ValueError(f"invalid {label}: {value}")
        bounds = item.split("-")
        if len(bounds) == 1:
            bounds.append(bounds[0])
        if len(bounds) != 2 or any(not bound.isdecimal() for bound in bounds):
            raise ValueError(f"invalid {label}: {value}")
        first, last = (int(bound, 10) for bound in bounds)
        if first > last:
            raise ValueError(f"invalid {label}: {value}")
        result.extend(range(first, last + 1))
    if len(set(result)) != len(result) or any(cpu < 0 for cpu in result):
        raise ValueError(f"invalid {label}: {value}")
    return tuple(sorted(result))


def parse_cpu_set(name: str) -> tuple[int, ...]:
    """Parse a comma/range CPU list from an environment variable.

    The helper intentionally returns an empty tuple for an unset variable;
    callers decide whether a particular class is required for a lane.
    """

    return _parse_cpu_set(os.environ.get(name, ""), label=name)


def parse_cpu_set_alias(*names: str) -> tuple[int, ...]:
    """Return the first configured CPU set among plural/singular aliases."""

    for name in names:
        value = os.environ.get(name, "")
        if value.strip():
            return _parse_cpu_set(value, label=name)
    return ()


def _smt_siblings(cpu: int) -> frozenset[int]:
    """Return the kernel-reported SMT siblings for one logical CPU.

    The standalone pinner cannot borrow the baseline wrapper's topology
    object.  Read the kernel topology directly and fail closed when it is not
    available; treating an unreadable sibling list as a singleton would make
    an explicit housekeeping CPU look isolated without proof.
    """

    path = Path(f"/sys/devices/system/cpu/cpu{cpu}/topology/thread_siblings_list")
    try:
        raw = path.read_text(encoding="ascii")
    except (OSError, UnicodeDecodeError) as error:
        raise CpuTopologyUnavailable(
            f"cannot read SMT sibling topology for CPU {cpu}: {error}"
        ) from error
    try:
        siblings = frozenset(_parse_cpu_set(raw, label=str(path)))
    except ValueError as error:
        raise CpuTopologyUnavailable(
            f"invalid SMT sibling topology for CPU {cpu}: {error}"
        ) from error
    if cpu not in siblings:
        raise CpuTopologyUnavailable(
            f"SMT sibling topology for CPU {cpu} omits the CPU itself"
        )
    return siblings


def _read_smt_topology(cpus: set[int]) -> dict[int, frozenset[int]]:
    """Read and validate the complete sibling equivalence classes involved."""

    topology: dict[int, frozenset[int]] = {}
    pending = set(cpus)
    while pending:
        cpu = pending.pop()
        if cpu in topology:
            continue
        siblings = _smt_siblings(cpu)
        topology[cpu] = siblings
        pending.update(set(siblings) - set(topology))
    for cpu, siblings in topology.items():
        for sibling in siblings:
            if sibling not in topology or cpu not in topology[sibling]:
                raise CpuTopologyUnavailable(
                    f"SMT sibling topology is not reciprocal between CPUs {cpu} and {sibling}"
                )
        closure = set().union(*(set(topology[sibling]) for sibling in siblings))
        if closure != set(siblings):
            raise CpuTopologyUnavailable(
                f"SMT sibling topology is not an equivalence class for CPU {cpu}"
            )
    return topology


def _measurement_smt_siblings(
    measurement: set[int],
    topology: Mapping[int, frozenset[int]] | None = None,
) -> tuple[int, ...]:
    topology = _read_smt_topology(measurement) if topology is None else dict(topology)
    excluded: set[int] = set(measurement)
    for cpu in measurement:
        siblings = topology.get(cpu)
        if siblings is None:
            raise CpuTopologyUnavailable(
                f"SMT sibling topology has no record for measurement CPU {cpu}"
            )
        excluded.update(siblings)
    return tuple(sorted(excluded))


def thread_names(pid: int) -> dict[int, str]:
    result: dict[int, str] = {}
    task_root = Path(f"/proc/{pid}/task")
    try:
        entries = tuple(task_root.iterdir())
    except OSError:
        return result
    for entry in entries:
        try:
            result[int(entry.name)] = (entry / "comm").read_text(encoding="ascii").strip()
        except (OSError, UnicodeDecodeError, ValueError):
            continue
    return result


def task_name(tid: int) -> str | None:
    """Read one task's comm without relying on thread-group enumeration.

    A daemonizing process leader can reach its ptrace exit-stop while sibling
    tasks remain in the thread group.  At that point Linux may omit the
    exiting leader from ``/proc/<tgid>/task`` even though the stopped TID's
    own procfs entry and affinity are still readable.  The exit proof is
    about that exact TID, so read its comm directly.
    """

    try:
        return Path(f"/proc/{tid}/comm").read_text(encoding="ascii").strip()
    except (OSError, UnicodeDecodeError):
        return None


def process_tgid(pid: int) -> int | None:
    """Read a stopped task's thread-group identity from procfs."""

    try:
        status = Path(f"/proc/{pid}/status").read_text(encoding="ascii")
    except (OSError, UnicodeDecodeError):
        return None
    for line in status.splitlines():
        key, separator, value = line.partition(":")
        if key == "Tgid" and separator:
            value = value.strip()
            if value.isdecimal() and int(value, 10) > 0:
                return int(value, 10)
            return None
    return None


def task_kernel_flags(tid: int) -> int | None:
    """Read the kernel PF_* word from field 9 of /proc/<tid>/stat."""

    try:
        stat = Path(f"/proc/{tid}/stat").read_text(encoding="ascii")
    except (OSError, UnicodeDecodeError):
        return None
    closing = stat.rfind(")")
    if closing < 0:
        return None
    fields = stat[closing + 2 :].split()
    # The post-comm fields begin at stat field 3, so flags (field 9) is 6.
    if len(fields) <= 6 or not fields[6].isdecimal():
        return None
    return int(fields[6], 10)


def is_untraced_kvm_nx_worker(tid: int, qemu_tgid: int, name: str) -> bool:
    """Recognize KVM's CLONE_UNTRACED vhost recovery worker exactly."""

    flags = task_kernel_flags(tid)
    return (
        name == KVM_NX_RECOVERY_COMM
        and process_tgid(tid) == qemu_tgid
        and flags is not None
        and bool(flags & PF_USER_WORKER)
    )


def untraced_worker_left_qemu_group(tid: int, qemu_tgid: int) -> bool:
    """Return true only after the original untraced worker is gone."""

    return process_tgid(tid) != qemu_tgid


def exit_role_closed(prior_clone_proof: bool, role_observed: bool) -> bool:
    """Judge this exit-stop independently from earlier proof failures.

    ``prior_clone_proof`` is intentionally not used to decide whether the
    current task was recorded.  The caller retains that global failure for
    the final report, while continuing to collect exact terminal readbacks.
    """

    _ = prior_clone_proof
    return role_observed


def process_identity(pid: int) -> dict[str, object] | None:
    """Return a PID-reuse-resistant identity for a live process leader."""

    tgid = process_tgid(pid)
    if tgid is None:
        return None
    try:
        executable = str(Path(os.readlink(f"/proc/{pid}/exe")).resolve())
        stat = Path(f"/proc/{pid}/stat").read_text(encoding="ascii")
    except (OSError, UnicodeDecodeError, RuntimeError):
        return None
    closing = stat.rfind(")")
    if closing < 0:
        return None
    fields = stat[closing + 2 :].split()
    # The post-comm fields begin at stat field 3; starttime is field 22.
    if len(fields) <= 19 or not fields[19].isdecimal():
        return None
    return {
        "pid": pid,
        "tgid": tgid,
        "exe": executable,
        "starttime": int(fields[19], 10),
    }


def _external_process_record(
    pid: int,
    name: str,
    affinity: tuple[int, ...],
    *,
    backend_authorized: bool,
) -> dict[str, object] | None:
    """Record an external process together with its stable identity proof.

    A comm string is only descriptive.  The authorization bit is meaningful
    only when the pinner has just re-read the exact PID/TGID, executable, and
    procfs start-time identity against the runner declaration.
    """

    identity = process_identity(pid)
    if identity is None:
        return None
    return {
        **identity,
        "main_tid": pid,
        "name": name,
        "affinity": list(affinity),
        "backend_authorized": bool(backend_authorized),
    }


def parse_external_backend_identities(
    raw: str | None = None,
) -> tuple[dict[str, object], ...]:
    """Parse runner-declared external backend identities.

    The declaration is intentionally exact JSON rather than a comm/name
    allow-list.  A backend must match PID, TGID, executable and procfs
    starttime; descendants are not implicitly authorized.
    """

    if raw is None:
        raw = os.environ.get("THEKERNEL_KVM_BACKEND_IDENTITIES", "")
        if not raw:
            raw = os.environ.get("THEKERNEL_KVM_EXTERNAL_BACKEND_IDENTITIES", "")
    if not raw.strip():
        return ()
    try:
        value = json.loads(raw)
    except json.JSONDecodeError as error:
        raise BackendIdentityUnavailable(
            f"invalid external backend identity JSON: {error}"
        ) from error
    if not isinstance(value, list):
        raise BackendIdentityUnavailable("external backend identities must be an array")
    identities: list[dict[str, object]] = []
    seen: set[int] = set()
    for item in value:
        if not isinstance(item, dict) or set(item) != BACKEND_IDENTITY_KEYS:
            raise BackendIdentityUnavailable(
                "external backend identity must contain exactly pid,tgid,exe,starttime"
            )
        pid = item["pid"]
        tgid = item["tgid"]
        exe = item["exe"]
        starttime = item["starttime"]
        if (
            isinstance(pid, bool)
            or not isinstance(pid, int)
            or pid <= 0
            or isinstance(tgid, bool)
            or not isinstance(tgid, int)
            or tgid <= 0
            or not isinstance(exe, str)
            or not exe
            or not Path(exe).is_absolute()
            or isinstance(starttime, bool)
            or not isinstance(starttime, int)
            or starttime <= 0
            or pid in seen
        ):
            raise BackendIdentityUnavailable("invalid or duplicate external backend identity")
        seen.add(pid)
        try:
            canonical_exe = str(Path(exe).resolve())
        except (OSError, RuntimeError, ValueError) as error:
            raise BackendIdentityUnavailable(
                f"invalid external backend executable identity: {error}"
            ) from error
        identities.append({
            "pid": pid,
            "tgid": tgid,
            "exe": canonical_exe,
            "starttime": starttime,
        })
    return tuple(sorted(identities, key=lambda identity: int(identity["pid"])))


def vcpu_index(name: str) -> int | None:
    """Return the QEMU vCPU index encoded in a debug thread name."""

    lowered = name.lower()
    if not any(token in lowered for token in ("/kvm", "/tcg", "kvm", "tcg", "vcpu", "cpu ")):
        return None
    match = CPU_RE.search(name)
    return int(match.group("index")) if match else None


def is_io_thread(name: str) -> bool:
    lowered = name.lower()
    return (
        "iothread" in lowered
        or lowered.startswith("io ")
        or lowered.startswith("io-")
        or "io-thread" in lowered
    )


def is_backend_thread(name: str) -> bool:
    """Recognize host backend workers without classifying the QEMU main thread."""

    lowered = name.lower()
    if is_io_thread(lowered) or vcpu_index(name) is not None:
        return False
    return any(
        marker in lowered
        for marker in (
            "vhost",
            "backend",
            "passt",
            "slirp",
            "qemu-pr-helper",
            "virtio",
            "aio",
        )
    )


def classify_thread(tid: int, pid: int, name: str) -> str:
    """Classify one observed QEMU task.

    ``pid`` is the QEMU process' main TID on Linux.  Checking it before name
    parsing prevents a main thread called ``qemu-system-...`` from becoming an
    accidental backend record.
    """

    if tid == pid:
        return "qemu-main"
    if vcpu_index(name) is not None:
        return "vcpu"
    if is_io_thread(name):
        return "iothread"
    if is_backend_thread(name):
        return "backend"
    return "unknown"


def parse_vcpu_count() -> int:
    value = os.environ.get("THEKERNEL_KVM_VCPU_COUNT", "").strip()
    if not value or not value.isdecimal() or int(value) <= 0:
        raise ValueError("THEKERNEL_KVM_VCPU_COUNT must be a positive decimal count")
    return int(value)


def read_affinity(tid: int) -> tuple[int, ...] | None:
    try:
        return tuple(sorted(os.sched_getaffinity(tid)))
    except OSError:
        return None


def pin_thread(tid: int, cpus: tuple[int, ...]) -> tuple[int, ...] | None:
    if not cpus:
        return read_affinity(tid)
    try:
        os.sched_setaffinity(tid, set(cpus))
    except OSError:
        return None
    return read_affinity(tid)


def _pre_exit_affinity_safe(
    classification: str,
    affinity: tuple[int, ...],
    measurement: set[int],
    *,
    requested_backend: tuple[int, ...] = (),
) -> bool:
    """Reject housekeeping work that reached a measurement CPU before exit.

    vCPU, IO, and explicitly requested backend tasks are expected to use
    measurement CPUs.  Unknown work, unrequested backend helpers, and the
    QEMU/external main task must remain on housekeeping CPUs; if either class
    is observed on a measurement CPU at the ptrace exit-stop, the sample is
    already contaminated.  Re-pinning it after that observation cannot
    repair the interval that just elapsed.
    """

    if classification == "backend" and requested_backend:
        return True
    if classification not in {"unknown", "qemu-main", "backend"}:
        return True
    return not (set(affinity) & measurement)


def _record(tid: int, name: str, affinity: tuple[int, ...] | None) -> dict[str, object] | None:
    if affinity is None:
        return None
    return {"tid": tid, "name": name, "affinity": list(affinity)}


def _role_record(
    tid: int, name: str, affinity: tuple[int, ...] | None
) -> dict[str, object] | None:
    """Record a QEMU vCPU/IO task with its immutable thread-group identity."""

    record = _record(tid, name, affinity)
    if record is None:
        return None
    tgid = process_tgid(tid)
    if tgid is None:
        return None
    record["tgid"] = tgid
    return record


def _affinity(record: Mapping[str, object]) -> tuple[int, ...]:
    value = record.get("affinity")
    if not isinstance(value, list):
        return ()
    return tuple(value) if all(isinstance(cpu, int) for cpu in value) else ()


def _vcpu_status(
    vcpus: Mapping[str, Mapping[str, object]],
    expected_vcpu_count: int,
    requested_vcpu: tuple[int, ...],
) -> str:
    if not requested_vcpu:
        return "not_requested"
    expected = {str(index) for index in range(expected_vcpu_count)}
    if set(vcpus) != expected:
        return "not_observed"
    for index in range(expected_vcpu_count):
        if _affinity(vcpus[str(index)]) != (requested_vcpu[index % len(requested_vcpu)],):
            return "affinity_mismatch"
    return "ok"


def _class_status(
    records: list[Mapping[str, object]], requested: tuple[int, ...]
) -> str:
    if not requested:
        return "not_requested"
    if not records:
        return "not_observed"
    expected = set(requested)
    for record in records:
        affinity = _affinity(record)
        if not affinity or not set(affinity).issubset(expected):
            return "affinity_mismatch"
    return "ok"


def write_report(
    path: Path | None,
    *,
    pid: int,
    expected_vcpu_count: int,
    vcpus: dict[str, dict[str, object]],
    io_threads: list[dict[str, object]],
    requested_vcpu: tuple[int, ...],
    requested_io: tuple[int, ...],
    requested_backend: tuple[int, ...] = (),
    housekeeping: tuple[int, ...] = (),
    backend_threads: list[dict[str, object]] | None = None,
    external_processes: list[dict[str, object]] | None = None,
    qemu_main: dict[str, object] | None = None,
    unknown_threads: list[dict[str, object]] | None = None,
    ptrace_clone_events: bool = False,
    clone_event_count: int = 0,
    measurement_smt_siblings: tuple[int, ...] | None = None,
    exit_readback_tids: tuple[int, ...] = (),
    exit_readback_proof: bool = False,
    declared_external_backends: tuple[dict[str, object], ...] = (),
    proof_failures: list[dict[str, object]] | None = None,
) -> None:
    """Atomically write a v4 placement report.

    A caller that did not establish ptrace clone-event coverage is explicitly
    unsupported; the default is fail-closed rather than claiming proof from a
    polling snapshot.
    """

    backend_threads = list(backend_threads or [])
    external_processes = list(external_processes or [])
    unknown_threads = list(unknown_threads or [])
    proof_failures = list(proof_failures or [])
    # Formal vCPU/IO records carry the QEMU thread-group identity.  Keep the
    # writer usable by small synthetic tests that provide the older shape by
    # binding omitted role identities to the report's QEMU PID; live pinner
    # observations always supply the read-back TGID through _role_record().
    vcpus = {
        index: {**record, "tgid": record.get("tgid", pid)}
        for index, record in vcpus.items()
    }
    io_threads = [
        {**record, "tgid": record.get("tgid", pid)} for record in io_threads
    ]
    backend_threads = [
        {**record, "tgid": record.get("tgid", pid)} for record in backend_threads
    ]
    declared_external_backends = tuple(
        sorted((dict(identity) for identity in declared_external_backends),
               key=lambda identity: int(identity["pid"]))
    )
    if clone_event_count <= 0:
        # A nominal ptrace setup with no observed clone cannot prove the
        # requested QEMU thread classes.  Keep the report explicitly
        # unsupported rather than letting a caller accidentally claim proof.
        ptrace_clone_events = False
        clone_event_count = max(0, clone_event_count)
    exit_readback_tids = tuple(sorted(set(exit_readback_tids)))
    if not ptrace_clone_events or not exit_readback_tids:
        exit_readback_proof = False
    measurement = set(requested_vcpu) | set(requested_io) | set(requested_backend)
    if measurement_smt_siblings is None:
        measurement_smt_siblings = _measurement_smt_siblings(measurement)
    excluded_siblings = set(measurement_smt_siblings)
    unknown_off_measurement = (
        ptrace_clone_events
        and all(not (set(_affinity(record)) & measurement) for record in unknown_threads)
    )
    requested_housekeeping = tuple(sorted(housekeeping))
    expected_indices = {str(index) for index in range(expected_vcpu_count)}
    vcpu_status = _vcpu_status(vcpus, expected_vcpu_count, requested_vcpu)
    io_status = _class_status(io_threads, requested_io)
    backend_status = _class_status(backend_threads, requested_backend)
    qemu_main_status = "ok" if qemu_main is not None else "not_observed"
    unknown_status = "ok" if unknown_off_measurement else (
        "measurement_overlap" if any(set(_affinity(record)) & measurement for record in unknown_threads)
        else "unsupported"
    )
    if unknown_threads and any(not _affinity(record) for record in unknown_threads):
        unknown_status = "affinity_unread"
    if not requested_housekeeping:
        housekeeping_status = "not_reported"
    else:
        housekeeping_status = "ok"
        for record in [qemu_main, *unknown_threads]:
            if record is not None and not set(_affinity(record)).issubset(set(requested_housekeeping)):
                housekeeping_status = "affinity_mismatch"
                break
    payload = {
        "schema": PIN_REPORT_SCHEMA,
        "pid": pid,
        "expected_vcpu_count": expected_vcpu_count,
        "requested_vcpu_cpus": list(requested_vcpu),
        "requested_io_cpus": list(requested_io),
        "requested_backend_cpus": list(requested_backend),
        "housekeeping_cpus": list(requested_housekeeping),
        "measurement_cpus": sorted(measurement),
        "measurement_smt_siblings": sorted(excluded_siblings),
        "vcpu_threads": vcpus,
        "io_threads": io_threads,
        "backend_threads": backend_threads,
        "external_processes": external_processes,
        "declared_external_backends": list(declared_external_backends),
        "qemu_main": qemu_main,
        "unknown_threads": unknown_threads,
        "vcpu_status": vcpu_status,
        "io_status": io_status,
        "backend_status": backend_status,
        "qemu_main_status": qemu_main_status,
        "housekeeping_status": housekeeping_status,
        "unknown_status": unknown_status,
        "unknown_off_measurement": unknown_off_measurement,
        # This is true only for the ptrace clone-event path.  A /proc polling
        # loop is deliberately not accepted as evidence for short-lived
        # workers.
        "launcher_affinity": list(requested_housekeeping),
        "process_inherited_housekeeping": bool(requested_housekeeping),
        "new_threads_inherit_housekeeping": bool(ptrace_clone_events),
        "ptrace_clone_events": bool(ptrace_clone_events),
        "clone_event_count": clone_event_count,
        "unknown_thread_proof": "ptrace-clone-event" if ptrace_clone_events else "unsupported",
        "exit_readback_tids": list(exit_readback_tids),
        "exit_readback_proof": bool(exit_readback_proof),
        "proof_failures": proof_failures,
    }
    if path is None:
        return
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.tmp")
    with temporary.open("w", encoding="utf-8") as stream:
        stream.write(json.dumps(payload, sort_keys=True) + "\n")
        stream.flush()
        os.fsync(stream.fileno())
    temporary.replace(path)


def _housekeeping_cpus(
    requested: tuple[int, ...],
    measurement: set[int],
    allowed: set[int],
    *,
    measurement_smt_siblings: tuple[int, ...] | None = None,
) -> tuple[int, ...]:
    excluded = (
        set(measurement_smt_siblings)
        if measurement_smt_siblings is not None
        else set(_measurement_smt_siblings(measurement))
    )
    if not measurement.issubset(excluded):
        raise ValueError("SMT sibling proof does not include every measurement CPU")
    if requested:
        if not set(requested).issubset(allowed):
            raise ValueError("THEKERNEL_KVM_HOUSEKEEPING_CPUS is outside launcher affinity")
        result = requested
    else:
        result = tuple(sorted(allowed - excluded))
    if not result:
        raise ValueError("at least one housekeeping CPU is required")
    if set(result) & excluded:
        raise ValueError(
            "housekeeping CPUs overlap measurement CPUs or their SMT siblings"
        )
    return result


def main() -> int:
    qemu = os.environ.get("THEKERNEL_KVM_QEMU")
    if not qemu:
        print("kvm_scheduler_pinner: THEKERNEL_KVM_QEMU is required", file=sys.stderr)
        return 2
    try:
        declared_external_backends = parse_external_backend_identities()
        for identity in declared_external_backends:
            current = process_identity(int(identity["pid"]))
            if current != identity:
                raise BackendIdentityUnavailable(
                    f"declared external backend identity is not live or changed: "
                    f"PID {identity['pid']}"
                )
    except BackendIdentityUnavailable as error:
        print(f"KVM_PINNING_UNSUPPORTED reason=backend-identity:{error}", file=sys.stderr)
        return 78
    try:
        requested_vcpu = parse_cpu_set("THEKERNEL_KVM_VCPU_CPUS")
        requested_io = parse_cpu_set("THEKERNEL_KVM_IO_CPUS")
        requested_backend = parse_cpu_set_alias(
            "THEKERNEL_KVM_BACKEND_CPUS", "THEKERNEL_KVM_BACKEND_CPU"
        )
        expected_vcpu_count = parse_vcpu_count()
        allowed = set(os.sched_getaffinity(0))
        measurement = set(requested_vcpu) | set(requested_io) | set(requested_backend)
        if not measurement.issubset(allowed):
            raise ValueError("measurement CPU is outside launcher affinity")
        sibling_topology = _read_smt_topology(allowed)
        topology_cpus = set(sibling_topology)
        if topology_cpus - allowed:
            raise CpuTopologyUnavailable(
                "SMT sibling topology references CPUs outside launcher affinity: "
                f"{sorted(topology_cpus - allowed)}"
            )
        measurement_smt_siblings = _measurement_smt_siblings(
            measurement, sibling_topology
        )
        housekeeping = _housekeeping_cpus(
            parse_cpu_set_alias(
                "THEKERNEL_KVM_HOUSEKEEPING_CPUS", "THEKERNEL_KVM_HOUSEKEEPING_CPU"
            ),
            measurement,
            allowed,
            measurement_smt_siblings=measurement_smt_siblings,
        )
        prearm_raw = os.environ.get("THEKERNEL_KVM_PREARM_KVM_NX_WORKER", "0")
        if prearm_raw not in {"0", "1"}:
            raise ValueError(
                "THEKERNEL_KVM_PREARM_KVM_NX_WORKER must be 0 or 1"
            )
        prearm_timeout = float(
            os.environ.get("THEKERNEL_KVM_PREARM_KVM_NX_TIMEOUT", "2.0")
        )
        if not (0.0 < prearm_timeout <= 10.0):
            raise ValueError("KVM NX prearm timeout must be in (0, 10] seconds")
    except CpuTopologyUnavailable as error:
        print(f"KVM_PINNING_UNSUPPORTED reason=cpu-topology:{error}", file=sys.stderr)
        return 78
    except ValueError as error:
        print(f"kvm_scheduler_pinner: {error}", file=sys.stderr)
        return 2

    # Launching QEMU on housekeeping CPUs is intentional.  Every unclassified
    # worker inherits that mask, and the main loop keeps the read-back proof
    # current as QEMU creates or destroys helpers.
    try:
        os.sched_setaffinity(0, set(housekeeping))
    except OSError as error:
        print(
            f"KVM_PINNING_UNSUPPORTED reason=launcher-affinity:{error}",
            file=sys.stderr,
        )
        return 78

    # QEMU 10 may probe io_uring for its fd monitor.  A zero child memlock
    # limit makes that probe fail uniformly and uses the supported epoll
    # fallback; disk AIO is fixed to ``aio=threads`` in command.py.
    try:
        resource.setrlimit(resource.RLIMIT_MEMLOCK, (0, 0))
    except (OSError, ValueError) as error:
        print(f"KVM_PINNING_UNSUPPORTED reason=memlock-limit:{error}", file=sys.stderr)
        return 78
    try:
        process = _start_traced([qemu, *sys.argv[1:]], housekeeping)
    except PtraceUnavailable as error:
        # Formal callers must surface this as an explicit unsupported lane.
        # Starting QEMU without clone-event coverage would create an
        # uncheckable unknown-thread window, so fail before launching it.
        print(f"KVM_PINNING_UNSUPPORTED reason={error}", file=sys.stderr)
        return 78
    nx_prearm = KvmNxPrearm(
        enabled=prearm_raw == "1",
        deadline=time.monotonic() + prearm_timeout,
    )
    observed_vcpus: dict[str, dict[str, object]] = {}
    observed_io: dict[int, dict[str, object]] = {}
    observed_backend: dict[int, dict[str, object]] = {}
    observed_unknown: dict[int, dict[str, object]] = {}
    observed_main: dict[str, object] | None = None
    backend_slots: dict[int, int] = {}
    observed_external_processes: dict[int, dict[str, object]] = {}
    # Separate process leaders do not appear in /proc/<qemu-tgid>/task.  Keep
    # every process leader in this set until its exit event and scan its own
    # task directory.  Clone children that share the QEMU tgid are tracked in
    # ``tracked_task_ids`` as well, so a CLONE event cannot disappear into a
    # later /proc snapshot without an identity/readback record.
    tracked_pids: set[int] = {process.pid}
    tracked_task_ids: set[int] = {process.pid}
    external_pids: set[int] = set()
    transitional_external_pids: set[int] = set()
    transitional_external_identities: dict[int, dict[str, object]] = {}
    declared_backend_by_pid = {
        int(identity["pid"]): identity for identity in declared_external_backends
    }
    clone_event_count = 0
    fork_event_count = 0
    vfork_event_count = 0
    return_code: int | None = None
    root_exit_status: int | None = None
    root_exit_time: float | None = None
    clone_proof = True
    exit_stops: set[int] = set()
    exit_readback_tids: set[int] = set()
    exit_contamination_tids: set[int] = set()
    exit_readback_failures: dict[int, str] = {}
    deferred_poll_failures: set[int] = set()
    untraced_kvm_workers: set[int] = set()
    clone_failure_reasons: list[str] = []
    proof_failures: list[dict[str, object]] = []

    def record_proof_failure(
        reason: str, tid: int, **details: object
    ) -> None:
        failure = {"reason": reason, "tid": tid, **details}
        if failure not in proof_failures:
            proof_failures.append(failure)

    def external_backend_authorized(pid: int, *, at_exit: bool = False) -> bool:
        declared = declared_backend_by_pid.get(pid)
        if declared is None:
            return False
        current = process_identity(pid)
        if current == declared:
            return True
        if at_exit and current is None:
            previous = observed_external_processes.get(pid)
            if previous is not None:
                identity = {
                    key: previous.get(key)
                    for key in ("pid", "tgid", "exe", "starttime")
                }
                return identity == declared
        return False

    def observe_task(
        tid: int,
        name: str,
        classification: str,
        *,
        stopped_affinity: tuple[int, ...] | None = None,
        defer_poll_race: bool = False,
    ) -> bool:
        nonlocal observed_main, clone_proof

        def failed() -> None:
            nonlocal clone_proof
            if defer_poll_race:
                # /proc enumeration races with a short-lived worker entering
                # its ptrace exit-stop.  The clone-stop already placed it on
                # housekeeping; require a later clone/readback/exit event to
                # close this provisional observation instead of making the
                # proof irreversibly false here.
                deferred_poll_failures.add(tid)
            else:
                record_proof_failure(
                    "direct-role-record-failed",
                    tid,
                    classification=classification,
                )
                clone_failure_reasons.append(
                    f"direct-{classification}-record-failed-{tid}"
                )
                clone_proof = False

        def observed() -> None:
            deferred_poll_failures.discard(tid)

        if classification == "vcpu":
            index = vcpu_index(name)
            if index is None:
                failed()
                return False
            cpus = nx_prearm.vcpu_cpus(requested_vcpu, housekeeping, index)
            affinity = (
                stopped_affinity
                if stopped_affinity is not None
                else pin_thread(tid, cpus)
            )
            record = (
                _role_record(tid, name, affinity)
                if index < expected_vcpu_count
                else _record(tid, name, affinity)
            )
            if record is None:
                failed()
                return False
            elif index < expected_vcpu_count:
                observed()
                observed_unknown.pop(tid, None)
                observed_vcpus[str(index)] = record
            else:
                observed()
                # An unexpected vCPU-like task is still an observed task; do
                # not drop its identity merely because the requested vCPU
                # count was smaller.  Its measurement affinity will make the
                # formal report fail closed as an unknown overlap.
                observed_unknown[tid] = record
            return True
        elif classification == "iothread":
            affinity = (
                stopped_affinity
                if stopped_affinity is not None
                else pin_thread(tid, requested_io or housekeeping)
            )
            record = _role_record(tid, name, affinity)
            if record is None:
                failed()
                return False
            else:
                observed()
                observed_unknown.pop(tid, None)
                observed_io[tid] = record
            return True
        elif classification == "backend":
            slot = backend_slots.setdefault(tid, len(backend_slots))
            cpus = (
                (requested_backend[slot % len(requested_backend)],)
                if requested_backend
                else housekeeping
            )
            affinity = (
                stopped_affinity
                if stopped_affinity is not None
                else pin_thread(tid, cpus)
            )
            record = _role_record(tid, name, affinity)
            if record is None:
                failed()
                return False
            else:
                observed()
                observed_unknown.pop(tid, None)
                observed_backend[tid] = record
            return True
        elif classification == "qemu-main":
            affinity = (
                stopped_affinity
                if stopped_affinity is not None
                else pin_thread(tid, housekeeping)
            )
            record = _record(tid, name, affinity)
            if record is None:
                failed()
                return False
            else:
                observed()
                observed_main = record
            return True
        else:
            affinity = (
                stopped_affinity
                if stopped_affinity is not None
                else pin_thread(tid, housekeeping)
            )
            record = _record(tid, name, affinity)
            if record is None:
                failed()
                return False
            else:
                observed()
                observed_unknown[tid] = record
            return True

    def observe_process(pid: int, *, defer_poll_race: bool = False) -> None:
        nonlocal clone_proof
        external = pid in external_pids
        names = thread_names(pid)
        if external and pid in names:
            process_affinity = read_affinity(pid)
            if process_affinity is None:
                # A process in the tracked set must have a real main-TID
                # readback, even if it exits before role classification.
                if defer_poll_race:
                    deferred_poll_failures.add(pid)
                else:
                    record_proof_failure(
                        "external-affinity-read-failed", pid
                    )
                    clone_proof = False
            else:
                record = _external_process_record(
                    pid,
                    names[pid],
                    process_affinity,
                    backend_authorized=(
                        pid == process_tgid(pid)
                        and is_backend_thread(names[pid])
                        and external_backend_authorized(pid)
                    ),
                )
                if record is None:
                    # A process held at an exit stop can disappear from
                    # /proc before the outer snapshot, while its exact
                    # identity was already recorded while live.  Keep that
                    # stable proof for the exit-stop readback; an external
                    # process with no prior identity remains unsupported.
                    previous = observed_external_processes.get(pid)
                    if previous is None and pid in transitional_external_pids:
                        # A newly cloned process is already stopped, pinned,
                        # and tracked, but procfs may transiently withhold its
                        # executable identity before PTRACE_EVENT_EXEC.  Its
                        # identity must close at exec or at its exit-stop.
                        inherited = transitional_external_identities.get(pid)
                        if inherited is not None:
                            deferred_poll_failures.discard(pid)
                            observed_external_processes[pid] = {
                                **inherited,
                                "main_tid": pid,
                                "name": names[pid],
                                "affinity": list(process_affinity),
                                "backend_authorized": False,
                            }
                    elif previous is None:
                        if defer_poll_race:
                            deferred_poll_failures.add(pid)
                        else:
                            record_proof_failure(
                                "external-identity-read-failed", pid
                            )
                            clone_proof = False
                    else:
                        deferred_poll_failures.discard(pid)
                        record = {
                            **previous,
                            "main_tid": pid,
                            "name": names[pid],
                            "affinity": list(process_affinity),
                            "backend_authorized": (
                                is_backend_thread(names[pid])
                                and external_backend_authorized(pid, at_exit=True)
                            ),
                        }
                        observed_external_processes[pid] = record
                else:
                    deferred_poll_failures.discard(pid)
                    observed_external_processes[pid] = record
        for tid, name in names.items():
            if not external:
                classification = classify_thread(tid, process.pid, name)
            elif (
                tid == pid
                and is_backend_thread(name)
                and external_backend_authorized(pid)
            ):
                # Only an explicitly recognizable backend may retain the
                # backend role outside QEMU's own thread group.  The exact
                # runner-declared process identity is checked above and at
                # every observation; a comm string alone never confers role
                # ownership, and descendants do not inherit authority.
                classification = "backend"
            else:
                classification = "unknown"
            if is_untraced_kvm_nx_worker(tid, process.pid, name):
                # KVM creates this vhost task with CLONE_UNTRACED and
                # PF_USER_WORKER, so no ptrace clone/exit event exists.  It
                # inherits the already-housekeeping QEMU creator's mask.
                # Preserve the pre-enforcement readback: a measurement overlap
                # is contamination and cannot be repaired after the fact.
                untraced_kvm_workers.add(tid)
                pre_affinity = read_affinity(tid)
                if pre_affinity is None:
                    deferred_poll_failures.add(tid)
                elif nx_prearm.enabled:
                    prearm_error = nx_prearm.observe_worker(
                        tid,
                        pre_affinity,
                        housekeeping=housekeeping,
                        measurement=measurement,
                    )
                    if prearm_error is not None:
                        record_proof_failure(
                            f"kvm-nx-prearm-{prearm_error}",
                            tid,
                            affinity=list(pre_affinity),
                        )
                        clone_proof = False
                    else:
                        observe_task(
                            tid,
                            name,
                            "unknown",
                            stopped_affinity=pre_affinity,
                            defer_poll_race=defer_poll_race,
                        )
                elif not _pre_exit_affinity_safe(
                    "unknown", pre_affinity, measurement
                ):
                    record_proof_failure(
                        "untraced-kvm-worker-affinity-overlap",
                        tid,
                        affinity=list(pre_affinity),
                        measurement=sorted(measurement),
                    )
                    exit_contamination_tids.add(tid)
                    clone_proof = False
                else:
                    observe_task(
                        tid,
                        name,
                        "unknown",
                        stopped_affinity=pre_affinity,
                        defer_poll_race=defer_poll_race,
                    )
            else:
                observe_task(
                    tid,
                    name,
                    classification,
                    defer_poll_race=defer_poll_race,
                )

    def observe_final_task(tid: int) -> bool:
        """Prove a task's terminal affinity at its ptrace exit-stop."""

        nonlocal clone_proof
        tgid = process_tgid(tid)
        if tgid is None:
            exit_readback_failures[tid] = "cannot-read-tgid"
            clone_proof = False
            return False
        name = task_name(tid)
        if name is None:
            exit_readback_failures[tid] = "cannot-read-comm"
            clone_proof = False
            return False
        if tgid == process.pid:
            classification = classify_thread(tid, process.pid, name)
        elif tgid in external_pids:
            if (
                tid == tgid
                and is_backend_thread(name)
                and external_backend_authorized(tgid, at_exit=True)
            ):
                classification = "backend"
            else:
                classification = "unknown"
        else:
            exit_readback_failures[tid] = f"untracked-tgid-{tgid}"
            clone_proof = False
            return False
        # This is a live read from the exact TID at its ptrace exit-stop, not
        # a stale record from the last /proc poll.  The stopped task cannot
        # execute again, so a safe readback is already its terminal affinity
        # proof.  In particular, Linux may reject sched_setaffinity with
        # ESRCH for an exiting daemon leader even while sched_getaffinity is
        # still readable.
        pre_affinity = read_affinity(tid)
        if pre_affinity is None:
            exit_readback_failures[tid] = "cannot-read-pre-affinity"
            clone_proof = False
            return False
        contaminated = not _pre_exit_affinity_safe(
            classification,
            pre_affinity,
            measurement,
            requested_backend=requested_backend,
        )
        if contaminated:
            # Continue through the normal pin/readback path so the report
            # retains an exact post-enforcement observation, but never admit
            # this task as proof: its pre-pin affinity already contaminated
            # the measurement interval.
            exit_contamination_tids.add(tid)
        proof_before = clone_proof
        role_observed = observe_task(
            tid,
            name,
            classification,
            stopped_affinity=None if contaminated else pre_affinity,
        )
        if not exit_role_closed(proof_before, role_observed):
            exit_readback_failures[tid] = "cannot-record-final-role"
            return False
        final_affinity = pre_affinity
        if contaminated:
            post_affinity = read_affinity(tid)
            if post_affinity is not None:
                final_affinity = post_affinity
        if tgid in external_pids and tid == tgid:
            record = _external_process_record(
                tgid,
                name,
                final_affinity,
                backend_authorized=(
                    is_backend_thread(name)
                    and external_backend_authorized(tgid, at_exit=True)
                ),
            )
            if record is None:
                previous = observed_external_processes.get(tgid)
                if previous is None:
                    exit_readback_failures[tid] = "cannot-record-final-process-identity"
                    clone_proof = False
                    return False
                record = {
                    **previous,
                    "main_tid": tgid,
                    "name": name,
                    "affinity": list(final_affinity),
                    "backend_authorized": (
                        is_backend_thread(name)
                        and external_backend_authorized(tgid, at_exit=True)
                    ),
                }
            observed_external_processes[tgid] = record
        if contaminated:
            clone_proof = False
            return False
        exit_readback_tids.add(tid)
        deferred_poll_failures.discard(tid)
        return True

    # The traced task is stopped at clone/fork/vfork/exec/exit events.  We
    # always put a newly reported child on housekeeping before continuing
    # either the child or its parent, so it cannot execute in an inherited
    # measurement mask during the observation window.
    while return_code is None:
        if nx_prearm.timed_out(time.monotonic()):
            record_proof_failure("kvm-nx-prearm-timeout", process.pid)
            print(
                "kvm_scheduler_pinner: KVM NX worker prearm timed out",
                file=sys.stderr,
            )
            clone_proof = False
            return_code = 78
            break
        for pid in tuple(sorted(tracked_pids)):
            observe_process(pid, defer_poll_race=True)
        event_seen = False
        while True:
            try:
                waited, status = os.waitpid(-1, WALL | os.WNOHANG)
            except ChildProcessError:
                if root_exit_status is None or tracked_pids or tracked_task_ids:
                    print(
                        "kvm_scheduler_pinner: ptrace wait lost a tracked process",
                        file=sys.stderr,
                    )
                    clone_proof = False
                    return_code = 78
                waited = 0
                status = 0
            except OSError as error:
                if error.errno == errno.EINTR:
                    continue
                print(f"kvm_scheduler_pinner: ptrace wait failed: {error}", file=sys.stderr)
                clone_proof = False
                return_code = 78
                break
            if waited == 0:
                break
            event_seen = True
            if os.WIFEXITED(status):
                code = os.WEXITSTATUS(status)
                if waited in tracked_task_ids and waited not in exit_stops:
                    # PTRACE_O_TRACEEXIT must precede every accepted task
                    # exit.  Without that stop the last affinity snapshot may
                    # describe a task that moved immediately before exit.
                    clone_proof = False
                    return_code = 78
                if waited in deferred_poll_failures:
                    print(
                        "kvm_scheduler_pinner: polling observation exited "
                        f"without terminal readback for task {waited}",
                        file=sys.stderr,
                    )
                    clone_proof = False
                    return_code = 78
                deferred_poll_failures.discard(waited)
                tracked_task_ids.discard(waited)
                tracked_pids.discard(waited)
                external_pids.discard(waited)
                transitional_external_pids.discard(waited)
                transitional_external_identities.pop(waited, None)
                exit_stops.discard(waited)
                if waited == process.pid:
                    root_exit_status = code
                    root_exit_time = time.monotonic()
                continue
            if os.WIFSIGNALED(status):
                code = -os.WTERMSIG(status)
                if waited in tracked_task_ids and waited not in exit_stops:
                    clone_proof = False
                    return_code = 78
                if waited in deferred_poll_failures:
                    print(
                        "kvm_scheduler_pinner: polling observation was signaled "
                        f"without terminal readback for task {waited}",
                        file=sys.stderr,
                    )
                    clone_proof = False
                    return_code = 78
                deferred_poll_failures.discard(waited)
                tracked_task_ids.discard(waited)
                tracked_pids.discard(waited)
                external_pids.discard(waited)
                transitional_external_pids.discard(waited)
                transitional_external_identities.pop(waited, None)
                exit_stops.discard(waited)
                if waited == process.pid:
                    root_exit_status = code
                    root_exit_time = time.monotonic()
                continue
            if not os.WIFSTOPPED(status):
                continue
            event = status >> 16
            if event == PTRACE_EVENT_EXIT and waited in tracked_task_ids:
                exit_stops.add(waited)
                if not observe_final_task(waited):
                    if waited in exit_contamination_tids:
                        print(
                            "kvm_scheduler_pinner: terminal affinity contamination "
                            f"for task {waited}",
                            file=sys.stderr,
                        )
                    else:
                        print(
                            "kvm_scheduler_pinner: final affinity readback failed "
                            f"for {waited}: {exit_readback_failures.get(waited, 'unknown')} "
                            f"first-proof-failure={clone_failure_reasons[0] if clone_failure_reasons else 'none'}",
                            file=sys.stderr,
                        )
                    return_code = 78
                    break
            if event == PTRACE_EVENT_EXEC and waited in tracked_pids:
                # At an exec stop the new comm is visible while the process is
                # still stopped.  Pin an external helper (notably passt) to
                # its role before allowing its first post-exec instruction;
                # otherwise a later /proc poll would leave an unproven window.
                transitional_external_pids.discard(waited)
                transitional_external_identities.pop(waited, None)
                if not thread_names(waited):
                    print(
                        f"kvm_scheduler_pinner: cannot observe exec child {waited}",
                        file=sys.stderr,
                    )
                    clone_proof = False
                    return_code = 78
                    break
                if waited in external_pids and process_identity(waited) is None:
                    print(
                        f"kvm_scheduler_pinner: cannot identify exec child {waited}",
                        file=sys.stderr,
                    )
                    clone_proof = False
                    return_code = 78
                    break
                observe_process(waited)
            if event in (PTRACE_EVENT_CLONE, PTRACE_EVENT_FORK, PTRACE_EVENT_VFORK):
                try:
                    new_tid = _ptrace_event_message(waited)
                    if new_tid <= 0:
                        raise PtraceUnavailable(
                            f"ptrace reported invalid child {new_tid}"
                        )
                    # sched_setaffinity is valid while the child is held in
                    # its ptrace stop.  Record the event before role parsing.
                    child_affinity = pin_thread(new_tid, housekeeping)
                    if child_affinity is None:
                        raise PtraceUnavailable(
                            f"cannot place child {new_tid} on housekeeping CPUs"
                        )
                    child_tgid = process_tgid(new_tid)
                    if child_tgid is None:
                        raise PtraceUnavailable(
                            f"cannot read thread-group identity for child {new_tid}"
                        )
                    # Keep the v4 field name for report compatibility; it
                    # counts all ptrace task events (clone, fork and vfork).
                    clone_event_count += 1
                    tracked_task_ids.add(new_tid)
                    deferred_poll_failures.discard(new_tid)
                    if child_tgid == process.pid:
                        # CLONE_THREAD children are part of the QEMU task set.
                        # They still receive an immediate housekeeping pin,
                        # then role classification/readback while stopped.
                        child_name = thread_names(process.pid).get(
                            new_tid, f"clone-thread-{new_tid}"
                        )
                        child_classification = classify_thread(
                            new_tid, process.pid, child_name
                        )
                        observe_task(new_tid, child_name, child_classification)
                        if (
                            new_tid not in observed_unknown
                            and child_classification == "unknown"
                        ):
                            raise PtraceUnavailable(
                                f"cannot record affinity for clone thread {new_tid}"
                            )
                    else:
                        # A CLONE child can be a separate process when its
                        # flags omit CLONE_THREAD.  Track its leader exactly
                        # like fork/vfork and keep scanning its descendants'
                        # task set until its exit event.
                        tracked_pids.add(child_tgid)
                        external_pids.add(child_tgid)
                        transitional_external_pids.add(child_tgid)
                        parent_tgid = process_tgid(waited)
                        parent_identity = (
                            process_identity(parent_tgid)
                            if parent_tgid is not None
                            else None
                        )
                        child_identity = process_identity(child_tgid)
                        if child_identity is not None:
                            transitional_external_identities[child_tgid] = child_identity
                        elif parent_identity is None:
                            raise PtraceUnavailable(
                                f"cannot bind inherited identity for clone process {new_tid}"
                            )
                        else:
                            try:
                                stat = Path(f"/proc/{child_tgid}/stat").read_text(
                                    encoding="ascii"
                                )
                                closing = stat.rfind(")")
                                fields = stat[closing + 2 :].split()
                                starttime = int(fields[19], 10)
                            except (
                                OSError,
                                UnicodeDecodeError,
                                ValueError,
                                IndexError,
                            ) as error:
                                raise PtraceUnavailable(
                                    f"cannot bind starttime for clone process {new_tid}"
                                ) from error
                            transitional_external_identities[child_tgid] = {
                                "pid": child_tgid,
                                "tgid": child_tgid,
                                "exe": parent_identity["exe"],
                                "starttime": starttime,
                            }
                        observe_process(child_tgid)
                        # A clone/fork child normally still exposes QEMU's
                        # executable until its later PTRACE_EVENT_EXEC stop.
                        # It is already stopped and pinned here; require the
                        # stable external-process identity only after exec,
                        # when `/proc/<pid>/exe` names the actual helper.
                    if event == PTRACE_EVENT_FORK:
                        fork_event_count += 1
                    elif event == PTRACE_EVENT_VFORK:
                        vfork_event_count += 1
                    try:
                        _ptrace(PTRACE_CONT, new_tid)
                    except OSError as error:
                        if error.errno != errno.ESRCH:
                            raise
                except (OSError, PtraceUnavailable) as error:
                    print(
                        f"kvm_scheduler_pinner: child tracking proof failed: {error}",
                        file=sys.stderr,
                    )
                    clone_proof = False
                    return_code = 78
                    break
            try:
                _ptrace(PTRACE_CONT, waited)
            except OSError as error:
                if error.errno != errno.ESRCH:
                    print(
                        f"kvm_scheduler_pinner: ptrace continue failed: {error}",
                        file=sys.stderr,
                    )
                    clone_proof = False
                    return_code = 78
                    break
        if return_code is not None:
            break
        if root_exit_status is not None:
            if not tracked_pids and not tracked_task_ids:
                return_code = root_exit_status
                break
            # Helpers normally exit with QEMU.  A surviving untraceable child
            # must not make the harness hang forever or silently drop proof.
            if root_exit_time is not None and time.monotonic() - root_exit_time > 5.0:
                print(
                    "kvm_scheduler_pinner: tracked external helper did not exit",
                    file=sys.stderr,
                )
                clone_proof = False
                for child in tuple(external_pids):
                    try:
                        os.kill(child, signal.SIGKILL)
                    except OSError:
                        pass
                return_code = 78
                break
        if not event_seen:
            time.sleep(0.005)
    if return_code is None:
        return_code = root_exit_status if root_exit_status is not None else process.wait()
    if root_exit_status is not None:
        for tid in tuple(deferred_poll_failures & untraced_kvm_workers):
            # vhost_task_stop() waits for this CLONE_UNTRACED worker's
            # completion before KVM VM teardown returns.  Once the QEMU
            # thread group has exited and the original TID is no longer in
            # that group, its last safe readback is terminal evidence.
            if untraced_worker_left_qemu_group(tid, process.pid):
                deferred_poll_failures.discard(tid)
    if deferred_poll_failures:
        print(
            "kvm_scheduler_pinner: unresolved polling observations for tasks "
            f"{sorted(deferred_poll_failures)}",
            file=sys.stderr,
        )
        clone_proof = False
        return_code = 78
        for tid in sorted(deferred_poll_failures):
            record_proof_failure("unresolved-poll-observation", tid)
    if not clone_proof and proof_failures:
        print(
            "kvm_scheduler_pinner: proof failure "
            + json.dumps(proof_failures[0], sort_keys=True),
            file=sys.stderr,
        )
    write_report(
        Path(os.environ["THEKERNEL_KVM_PIN_REPORT"])
        if os.environ.get("THEKERNEL_KVM_PIN_REPORT")
        else None,
        pid=process.pid,
        expected_vcpu_count=expected_vcpu_count,
        vcpus=observed_vcpus,
        io_threads=list(observed_io.values()),
        requested_vcpu=requested_vcpu,
        requested_io=requested_io,
        requested_backend=requested_backend,
        housekeeping=housekeeping,
        backend_threads=list(observed_backend.values()),
        external_processes=list(observed_external_processes.values()),
        qemu_main=observed_main,
        unknown_threads=list(observed_unknown.values()),
        ptrace_clone_events=clone_proof,
        clone_event_count=clone_event_count,
        measurement_smt_siblings=measurement_smt_siblings,
        exit_readback_tids=tuple(sorted(exit_readback_tids)),
        exit_readback_proof=clone_proof and bool(exit_readback_tids),
        declared_external_backends=declared_external_backends,
        proof_failures=proof_failures,
    )
    # A formal lane needs observed ptrace task-event coverage, not just a
    # ptrace setup that happened to succeed.  QEMU normally creates vCPU/IO
    # workers, but a short-lived or capability-limited child can exit before
    # the first clone/fork/vfork; surface that boundary explicitly so the
    # harness records ``unsupported`` instead of treating a v4 report with no
    # proof as a runner success.
    if not clone_proof or clone_event_count <= 0:
        print(
            "KVM_PINNING_UNSUPPORTED reason=no-observed-ptrace-task-event",
            file=sys.stderr,
        )
        return 78
    return return_code


if __name__ == "__main__":
    raise SystemExit(main())
