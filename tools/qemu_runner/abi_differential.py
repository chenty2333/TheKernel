"""Execute the structured portable ABI contracts on two equivalent guests.

This finite contract set is not an audit of the entire Linux syscall table.
"""
from __future__ import annotations

from collections import Counter
from dataclasses import dataclass
import os
import math
from pathlib import Path
import re
import shutil
import tempfile

from tools.product_state import validate_storage
from .boot_artifacts import validate_linux_boot, validate_linux_esp_kernel, validate_thekernel_esp_kernel
from .kernel_benchmark import BenchmarkTarget, SHELL_MARKER
from .model import Interaction, RunLimits
from .runner import RunConfig, RunnerError, run

COMPLETE_MARKER = "THEKERNEL_ABI_EXIT_ZERO"
# Explicit expectations prevent an accidentally deleted guest assertion from
# reducing acceptance coverage. Keep these aligned with tests/guest/portable.
CONTRACTS = {
    "unix-write-credentials": ("raw-differential", "pass", "WRITE_SENDER_PID_REAL_IDS WRITEV_SENDER_PID_REAL_IDS SENDMSG_SENDER_PID_REAL_IDS CHILD_EXIT_CLEAN REAL_EFFECTIVE_IDS"),
    "eventfd": ("portable-differential", "pass", "LEGACY_FLAGS IO_ERRNO_STATE COUNTER_POLL SEMAPHORE CLOEXEC_TEARDOWN"),
    "creat": ("raw-differential", "pass", "PROVIDER_EXT4 CREATE_UMASK_STATUS TRUNCATE_EXISTING BAD_PATH_EFAULT TEARDOWN"),
    "time": ("raw-differential", "pass", "NULL_EPOCH_ERRNO UNALIGNED_EIGHT_BYTES CROSS_WRITABLE_PAGE EFAULT_COPYOUT REALTIME_BRACKET"),
    "umask": ("raw-differential", "pass", "PROVIDER_EXT4 MASK_AND_CREATE FORK_COPIES_FS CLONE_FS_SHARES UNSHARE_FS_SEPARATES EXEC_PRESERVES_FS"),
    "native-ni": ("fixed-slots", "enosys", "NR_134_USELIB NR_156_SYSCTL NR_174_CREATE_MODULE NR_177_GET_KERNEL_SYMS NR_178_QUERY_MODULE NR_180_NFSSERVCTL NR_181_GETPMSG NR_182_PUTPMSG NR_183_AFS_SYSCALL NR_184_TUXCALL NR_185_SECURITY NR_205_SET_THREAD_AREA NR_211_GET_THREAD_AREA NR_212_LOOKUP_DCOOKIE NR_214_EPOLL_CTL_OLD NR_215_EPOLL_WAIT_OLD NR_236_VSERVER OUT_OF_RANGE_1024"),
}

CONTRACTS.update({
    "setxattrat": ("raw-differential", "pass", "CREATE_REPLACE_EMPTY_PATH FIFO_USER_EPERM VALIDATION_ORDER"),
    "getxattrat": ("raw-differential", "pass", "PROBE_RANGE_VALUE FIFO_USER_ENODATA VALIDATION_ORDER"),
    "listxattrat": ("raw-differential", "pass", "PROBE_RANGE_NAMES VALIDATION_ORDER"),
    "removexattrat": ("raw-differential", "pass", "REMOVE_ABSENT_STATE FIFO_USER_EPERM VALIDATION_ORDER"),
    "file-getattr": ("raw-differential", "pass", "ALLOCATED_EXTENT_FIELDS_ZERO GET_EMPTY_PATH_ZERO_TAIL VALIDATION_ORDER"),
    "file-setattr": ("raw-differential", "pass", "NODUMP_IGNORES_INPUT_NEXTENTS EXT4_IGNORES_UNFLAGGED_HINTS ERRORS_PRESERVE_STATE RESTORE"),
    "open-tree-attr": ("raw-differential", "pass", "CLONE_CLOEXEC_DIRECTORY READONLY_CLONE_SOURCE_UNCHANGED VALIDATION_ORDER"),
})
CONTRACTS.update({
    "mprotect": ("raw-differential", "pass", "HOLE_COMMITS_PREFIX_ONLY VALIDATION_RESTORE PROT_NONE_FORK_UNMAP_REUSE"),
    "munmap": ("raw-differential", "pass", "HOLE_NEIGHBORS_IDEMPOTENT VALIDATION_PRESERVES_NEIGHBORS"),
    "mincore": ("raw-differential", "pass", "TOUCHED_RESIDENCY_EXACT_OUTPUT VALIDATION_ORDER FILE_PAGE_RESIDENCY"),
    "process-vm-readv": ("raw-differential", "pass", "EXACT_COPY REMOTE_FAULT_PREFIX VALIDATION_EMPTY_LOCAL REMOTE_CONTENT_CONFIRMED PERMISSION_EPERM"),
    "process-vm-writev": ("raw-differential", "pass", "EXACT_COPY REMOTE_FAULT_PREFIX VALIDATION_EMPTY_LOCAL REMOTE_CONTENT_CONFIRMED PERMISSION_EPERM"),
    "mseal": ("raw-differential", "pass", "VALIDATION_AND_MAPPING_SEAL DISCARD_RESPECTS_WRITE_PERMISSION"),
})
CONTRACTS.update({
    "network_bind": ("raw-differential", "pass", "IPV4_OVERLONG_EINVAL IPV4_STORAGE_BOUNDARY IPV6_OVERLONG_EINVAL"),
    "network_connect": ("raw-differential", "pass", "IPV6_OVERLONG_EINVAL IPV4_OVERLONG_EINVAL NETLINK_SOCKET NETLINK_KERNEL_CONNECT NETLINK_AUTOBIND NETLINK_DISCONNECT NETLINK_DISCONNECTED_PEER NETLINK_BAD_FAMILY"),
    "network_getpeername": ("raw-differential", "pass", "NETLINK_UNCONNECTED_ZERO NETLINK_CONNECTED_ZERO NETLINK_PEER_POLICY NETLINK_PEER_STATE NETLINK_PEER_RESET NETLINK_TRUNCATED_LENGTH"),
    "network_sendto": ("raw-differential", "pass", "IPV4_OVERLONG_EINVAL IPV6_OVERLONG_EINVAL"),
})
CONTRACTS.update({
    "rt_tgsigqueueinfo": ("raw-differential", "pass", "COPY_BEFORE_INVALID_IDS INVALID_IDS_BEFORE_CODE COPY_BEFORE_INVALID_SIGNO"),
    "restart_syscall": ("raw-differential", "pass", "NO_PENDING_BLOCK_EINTR"),
    "rt_sigaction": ("raw-differential", "pass", "SIZE_BEFORE_COPY COPY_BEFORE_SIGNO INVALID_REPLACEMENT KILL_STOP_QUERY COMMIT_BEFORE_OLD_COPY"),
    "sigaltstack": ("raw-differential", "pass", "COMMIT_BEFORE_OLD_COPY BAD_NEW_PRESERVES ACCEPT_ONSTACK WRAPPING_GEOMETRY_STORED DISABLE_AUTODISARM OVERLAPPING_INPUT_OUTPUT INVALID_FLAGS_BEFORE_OLD_COPY"),
})
CONTRACTS.update({
    "access": ("raw-differential", "pass", "MODE_BEFORE_PATH EXISTS_FLAGS"),
    "faccessat": ("raw-differential", "pass", "MODE_BEFORE_PATH EXISTS_FLAGS"),
    "faccessat2": ("raw-differential", "pass", "MODE_BEFORE_PATH EXISTS_FLAGS"),
    "newfstatat": ("raw-differential", "pass", "PATH_FLAG_ORDER EMPTY_FD_IDENTITY NO_AUTOMOUNT_SYNC_FLAGS"),
    "statx": ("raw-differential", "pass", "EXTENSIBLE_MASK_VALIDATION EMPTY_FD_IDENTITY PROVIDER_OPTIONAL_FIELDS EXT4_OPTIONAL_FIELDS"),
})
CONTRACTS.update({
    "flock": ("raw-differential", "pass", "MANDATORY_BEFORE_FD_COMMAND COMMAND_BEFORE_FD VALID_COMMAND_BAD_FD"),
    "utimensat": ("raw-differential", "pass", "OMIT_BEFORE_PATH_FLAGS_FD COPY_BEFORE_FLAGS"),
    "fallocate": ("raw-differential", "pass", "FIFO_ESPIPE ACCESS_BEFORE_TYPE MODE_BEFORE_ACCESS_TYPE GEOMETRY_BEFORE_TYPE SOCKET_ENODEV"),
    "readahead": ("raw-differential", "pass", "PIDFD_EINVAL FD_BEFORE_OFFSET READ_PIPE_EINVAL ACCESS_BEFORE_TYPE_OFFSET PATH_FD_EBADF"),
})
CONTRACTS.update({
    "inotify_add_watch": ("raw-differential", "pass", "FD_BEFORE_MASK_CONFLICT MASK_BITS_BEFORE_FD MASK_CONFLICT_BEFORE_PATH"),
    "signalfd4": ("raw-differential", "pass", "COPY_BEFORE_FLAGS_FD SIZE_BEFORE_COPY FLAGS_BEFORE_FD VALID_MASK_FLAGS_BAD_FD"),
    "timerfd_settime": ("raw-differential", "pass", "COPY_BEFORE_FLAGS_FD FLAGS_BEFORE_FD VALUE_BEFORE_FD VALID_VALUE_FLAGS_BAD_FD"),
    "mlock": ("raw-differential", "pass", "HOLE_COMMITS_PREFIX"),
    "mlock2": ("raw-differential", "pass", "HOLE_COMMITS_PREFIX"),
    "munlock": ("raw-differential", "pass", "HOLE_COMMITS_PREFIX"),
    "process-madvise": ("raw-differential", "pass", "SELF_DESTRUCTIVE_ADVICE"),
    "mlockall": ("raw-differential", "pass", "POPULATE_FAILURE_IGNORED"),
})
CONTRACTS.update({
    "sched_getaffinity": ("raw-differential", "pass", "get-affinity get-unaligned-length get-low32-zero get-low32-length"),
    "sched_setaffinity": ("raw-differential", "pass", "set-low32-length set-low32-zero set-short-mask"),
    "getcpu": ("raw-differential", "pass", "getcpu-first-copy-fault getcpu-node-written-after-cpu-fault"),
    "sched_setparam": ("raw-differential", "pass", "setparam-negative-pid-before-copy setparam-null setparam-bad-pointer"),
    "sched_setscheduler": ("raw-differential", "pass", "setscheduler-negative-pid-before-copy setscheduler-negative-policy-before-copy setscheduler-positive-invalid-policy-after-copy setscheduler-null"),
    "sched_get_priority_max": ("raw-differential", "pass", "ext-priority-max"),
    "sched_get_priority_min": ("raw-differential", "pass", "ext-priority-min"),
})
PROGRAM_CASES = {
    "unix-write-credentials": ("unix-write-credentials",),
    "scheduler-basic": ('sched_getaffinity', 'sched_setaffinity', 'getcpu', 'sched_setparam', 'sched_setscheduler', 'sched_get_priority_max', 'sched_get_priority_min'),
    "fs-boundary": ("flock", "utimensat", "fallocate", "readahead", "inotify_add_watch", "signalfd4", "timerfd_settime"),
    "eventfd": ("eventfd",), "creat": ("creat",), "time": ("time",),
    "umask": ("umask",), "native-ni": ("native-ni",),
    "fsattrs": ("setxattrat", "getxattrat", "listxattrat", "removexattrat", "file-getattr", "file-setattr", "open-tree-attr"),
    "mm-contracts": ("mprotect", "munmap", "mincore", "process-vm-readv", "process-vm-writev", "mseal", "mlock", "mlock2", "munlock", "process-madvise", "mlockall"),
    "network-basic": ("network_bind", "network_connect", "network_getpeername", "network_sendto"),
    "signal-boundary": ("rt_sigaction", "sigaltstack", "rt_tgsigqueueinfo", "restart_syscall"),
    "stat-access": ("access", "faccessat", "faccessat2", "newfstatat", "statx"),
}
PROGRAMS = tuple(PROGRAM_CASES)
PROGRAM_SUCCESS = {
    "unix-write-credentials": "THEKERNEL_UNIX_WRITE_CREDENTIALS_OK",
    "scheduler-basic": "THEKERNEL_SCHEDULER_BASIC_DIFFERENTIAL_OK",
    "fs-boundary": "THEKERNEL_FS_BOUNDARY_PASS",
    "stat-access": "THEKERNEL_STAT_ACCESS_OK",
    "eventfd": "THEKERNEL_EVENTFD_OK", "creat": "THEKERNEL_CREAT_OK",
    "time": "THEKERNEL_TIME_OK", "umask": "THEKERNEL_UMASK_OK",
    "native-ni": "THEKERNEL_NATIVE_NI_OK", "fsattrs": "THEKERNEL_FSATTRS_OK",
    "mm-contracts": "THEKERNEL_MM_CONTRACTS_OK", "network-basic": "THEKERNEL_NETWORK_BASIC_PASS",
    "signal-boundary": "THEKERNEL_SIGNAL_BOUNDARY_PASS",
}
PROGRAM_COMPLETIONS = tuple(PROGRAM_SUCCESS.values())

# A test file existing is insufficient: a claimed syscall must name a case
# that this runner actually requires from the registered guest program.
SYSCALL_CASES = {
    204: ("scheduler-basic", "sched_getaffinity"),
    203: ("scheduler-basic", "sched_setaffinity"),
    309: ("scheduler-basic", "getcpu"),
    142: ("scheduler-basic", "sched_setparam"),
    144: ("scheduler-basic", "sched_setscheduler"),
    146: ("scheduler-basic", "sched_get_priority_max"),
    147: ("scheduler-basic", "sched_get_priority_min"),

    254: ("fs-boundary", "inotify_add_watch"), 289: ("fs-boundary", "signalfd4"),
    286: ("fs-boundary", "timerfd_settime"), 149: ("mm-contracts", "mlock"),
    325: ("mm-contracts", "mlock2"), 150: ("mm-contracts", "munlock"),
    440: ("mm-contracts", "process-madvise"), 151: ("mm-contracts", "mlockall"),
    73: ("fs-boundary", "flock"), 280: ("fs-boundary", "utimensat"),
    285: ("fs-boundary", "fallocate"), 187: ("fs-boundary", "readahead"),
    21: ("stat-access", "access"), 269: ("stat-access", "faccessat"),
    439: ("stat-access", "faccessat2"), 262: ("stat-access", "newfstatat"),
    332: ("stat-access", "statx"),
    297: ("signal-boundary", "rt_tgsigqueueinfo"), 219: ("signal-boundary", "restart_syscall"),
    13: ("signal-boundary", "rt_sigaction"), 131: ("signal-boundary", "sigaltstack"),
    85: ("creat", "creat"), 95: ("umask", "umask"), 201: ("time", "time"),
    284: ("eventfd", "eventfd"), 290: ("eventfd", "eventfd"),
    463: ("fsattrs", "setxattrat"), 464: ("fsattrs", "getxattrat"),
    465: ("fsattrs", "listxattrat"), 466: ("fsattrs", "removexattrat"),
    467: ("fsattrs", "open-tree-attr"), 468: ("fsattrs", "file-getattr"),
    469: ("fsattrs", "file-setattr"),
    10: ("mm-contracts", "mprotect"), 11: ("mm-contracts", "munmap"),
    27: ("mm-contracts", "mincore"), 310: ("mm-contracts", "process-vm-readv"),
    311: ("mm-contracts", "process-vm-writev"), 462: ("mm-contracts", "mseal"),
    49: ("network-basic", "network_bind"), 42: ("network-basic", "network_connect"),
    52: ("network-basic", "network_getpeername"), 44: ("network-basic", "network_sendto"),
}


@dataclass(frozen=True)
class AbiConfig:
    targets: tuple[BenchmarkTarget, ...]
    rootfs: Path
    workdir: Path
    cpus: int = 4
    memory: str = "4G"
    timeout: float = 1800.0


def expected_records() -> list[str]:
    records = []
    for name, (suffix, outcome, assertions) in CONTRACTS.items():
        case = f"{name}.{suffix}"
        records.append(f"THEKERNEL_ABI_CASE {case}")
        records.extend(f"THEKERNEL_ABI_ASSERT {case} {assertion} {outcome}" for assertion in assertions.split())
        records.append(f"THEKERNEL_ABI_RESULT {case} {outcome}")
    return records


def parse_abi_log(path: Path, *, linux: bool = False) -> Counter:
    text = path.read_text(encoding="utf-8", errors="replace")
    lines = text.splitlines()
    if lines.count(COMPLETE_MARKER) != 1:
        raise RunnerError(f"ABI guest did not complete exactly once: {path}")
    if any(lines.count(marker) != 1 for marker in PROGRAM_COMPLETIONS):
        raise RunnerError(f"ABI program completion is missing or duplicated: {path}")
    if any(lines.index(marker) > lines.index(COMPLETE_MARKER) for marker in PROGRAM_COMPLETIONS):
        raise RunnerError(f"ABI aggregate completion precedes a program completion: {path}")
    if re.search(r"^THEKERNEL_\S*(?:FAIL|SKIP)(?:\s|$)", text, re.MULTILINE):
        raise RunnerError(f"ABI guest reported a failure or skip: {path}")
    intervals = [line for line in lines if line.startswith("# THEKERNEL_TEST_")]
    if len(intervals) != 2 * len(PROGRAMS):
        raise RunnerError(f"ABI watchdog intervals are missing or duplicated: {path}")
    owned_record_count = 0
    for index, name in enumerate(PROGRAMS, 1):
        begin = rf"# THEKERNEL_TEST_BEGIN {index} abi-{re.escape(name)} timeout_seconds=[1-9][0-9]*"
        end = f"# THEKERNEL_TEST_END {index} abi-{name} result=0"
        if not re.fullmatch(begin, intervals[2 * (index - 1)]) or intervals[2 * index - 1] != end:
            raise RunnerError(f"ABI program lacks a unique successful watchdog interval: {name}: {path}")
        first, last = lines.index(intervals[2 * (index - 1)]), lines.index(end)
        if last >= lines.index(COMPLETE_MARKER):
            raise RunnerError(f"ABI watchdog interval outlives aggregate completion: {path}")
        expected_case_names = {f"{case}.{CONTRACTS[case][0]}" for case in PROGRAM_CASES[name]}
        owned = [line for line in lines[first + 1:last] if line.startswith("THEKERNEL_ABI_")]
        owned_record_count += len(owned)
        if (not owned or any(len(line.split()) < 2 or line.split()[1] not in expected_case_names for line in owned)
                or PROGRAM_SUCCESS[name] not in lines[first + 1:last]):
            raise RunnerError(f"ABI program records do not belong to its watchdog interval: {name}: {path}")
    if linux:
        validate_linux_boot(text, path)
    ordered = [line for line in lines if line.startswith("THEKERNEL_ABI_")]
    if owned_record_count != len(ordered) - 1:
        raise RunnerError(f"ABI case records escaped their program watchdog interval: {path}")
    records = Counter(ordered)
    expected = Counter(expected_records() + [COMPLETE_MARKER])
    if records != expected:
        raise RunnerError(f"ABI assertions missing, duplicated or unexpected: {path}; missing={list((expected - records).elements())}; unexpected={list((records - expected).elements())}")
    active = None
    for record in ordered[:-1]:
        fields = record.split()
        if fields[0] == "THEKERNEL_ABI_CASE" and active is None:
            active = fields[1]
        elif fields[0] == "THEKERNEL_ABI_ASSERT" and fields[1] == active:
            pass
        elif fields[0] == "THEKERNEL_ABI_RESULT" and fields[1] == active:
            active = None
        else:
            raise RunnerError(f"ABI record is outside its active case: {path}: {record}")
    if active is not None or ordered[-1] != COMPLETE_MARKER:
        raise RunnerError(f"ABI completion precedes finished cases: {path}")
    return records


def run_abi_differential(config: AbiConfig) -> Path:
    if sorted(target.name for target in config.targets) != ["baseline", "linux"]:
        raise RunnerError("ABI differential requires exactly baseline and linux targets")
    if config.cpus not in (1, 4) or not math.isfinite(config.timeout) or config.timeout <= 0:
        raise RunnerError("ABI differential requires 1/4 vCPUs and a positive timeout")
    validate_storage(config.workdir)
    for path in (config.rootfs, *(p for target in config.targets for p in (target.kernel, target.esp))):
        if not path.is_file() or not path.stat().st_size:
            raise RunnerError(f"ABI input is missing or empty: {path}")
    config.workdir.mkdir(parents=True, exist_ok=True)
    directory = Path(tempfile.mkdtemp(prefix="abi-", dir=config.workdir))
    base = directory / "rootfs-base.img"
    observations = []
    boot_copies = []
    try:
        targets = []
        for target in config.targets:
            current = directory / target.name
            current.mkdir()
            kernel, esp = current / "kernel", current / "boot.esp"
            boot_copies.extend((kernel, esp))
            shutil.copyfile(target.kernel, kernel)
            shutil.copyfile(target.esp, esp)
            validate = validate_linux_esp_kernel if target.name == "linux" else validate_thekernel_esp_kernel
            validate(kernel, esp)
            targets.append(BenchmarkTarget(target.name, kernel, esp))
        shutil.copyfile(config.rootfs, base)
        for target in targets:
            current = directory / target.name
            rootfs = current / "rootfs.img"
            commands = current / "commands"
            workloads = []
            case_timeout = min(120, max(1, math.ceil(config.timeout)))
            for index, name in enumerate(PROGRAMS, 1):
                command = f"/opt/thekernel-tests/portable/{name}-differential"
                if name == "unix-write-credentials":
                    command += " --require-id-change"
                workloads.extend((
                    f'echo "# THEKERNEL_TEST_BEGIN {index} abi-{name} timeout_seconds={case_timeout}"',
                    f"{command}; result=$?",
                    '[ "$result" = 0 ] || failed=1',
                    f'echo "# THEKERNEL_TEST_END {index} abi-{name} result=$result"',
                ))
            commands.write_text(
                "failed=0\n" +
                # Linux DEBUG_STACK_USAGE informational printk output can split
                # userspace assertion lines on the serial console. Keep errors
                # visible and retain the already emitted boot version banner.
                ("echo 3 > /proc/sys/kernel/printk || failed=1\n" if target.name == "linux" else "") +
                "\n".join(workloads) + "\n" +
                f'[ "$failed" = 0 ] && echo {COMPLETE_MARKER}\n'
                "/bin/busybox poweroff -f\nexit\n", encoding="utf-8")
            try:
                shutil.copyfile(base, rootfs)
                with rootfs.open("r+b") as image:
                    os.fsync(image.fileno())
                result = run(RunConfig(
                    arch="x86_64", kernel=target.kernel, esp=target.esp,
                    rootfs=rootfs, rootfs_transport="drive", rootfs_mode="rw",
                    workdir=current, log_path=current / "console.log", input_path=commands,
                    limits=RunLimits(total_timeout_secs=config.timeout),
                    interaction=Interaction(interactive=True, input_after_marker=SHELL_MARKER,
                                            input_line_after_marker=SHELL_MARKER),
                    memory=config.memory, cpus=config.cpus, accel="kvm", graphics_profile="headless",
                ))
                if (not result.guest_clean_shutdown or result.error_message is not None
                        or result.runner_termination_reason is not None):
                    raise RunnerError(f"ABI guest failed: {target.name}; log={result.log_path}")
                observations.append(parse_abi_log(result.log_path, linux=target.name == "linux"))
            finally:
                rootfs.unlink(missing_ok=True)
    finally:
        base.unlink(missing_ok=True)
        for path in boot_copies:
            path.unlink(missing_ok=True)
    if observations[0] != observations[1]:
        raise RunnerError("Linux and TheKernel ABI observations differ")
    return directory
