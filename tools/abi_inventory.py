#!/usr/bin/env python3
"""Generate a deterministic, source-only x86_64 syscall route inventory.

This is deliberately not implementation evidence: it records what the Linux
table and TheKernel dispatcher *say statically*, including finite fallbacks.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import re
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parents[1]
ABI_DIR = ROOT / "docs/linux-abi"
TABLE = ABI_DIR / "linux-v6.12.103-arch-x86-entry-syscalls-syscall_64.tbl"
DISPATCH = ROOT / "kernel/src/syscall/dispatch.rs"
OUTPUT = ABI_DIR / "static-inventory.json"
EXPECTED_ROWS = 375
NI_COUNT = 17
LINUX_V6_12_103_COMMIT = "25c09b42358e73e1476e517b296edb6344f2e4bd"
EXPECTED_COND_SYSCALL_COUNT = 162
# Native-table intersection of kernel/sys_ni.c's COND_SYSCALL entries at the
# pinned v6.12.103 commit.  Keep this literal: generation must not depend on a
# host Linux checkout or a mutable .state file.
LINUX_COND_SYSCALLS = frozenset({
    'accept', 'accept4', 'acct', 'add_key', 'alarm', 'bind', 'bpf', 'cachestat',
    'capget', 'capset', 'clock_adjtime', 'connect', 'copy_file_range',
    'delete_module', 'epoll_create', 'epoll_create1', 'epoll_ctl', 'epoll_pwait',
    'epoll_pwait2', 'epoll_wait', 'eventfd', 'eventfd2', 'execveat', 'fadvise64',
    'fanotify_init', 'fanotify_mark', 'finit_module', 'flock', 'futex',
    'futex_requeue', 'futex_wait', 'futex_waitv', 'futex_wake', 'get_mempolicy',
    'get_robust_list', 'getgroups', 'getitimer', 'getpeername', 'getresgid', 'getresuid',
    'getsockname', 'getsockopt', 'init_module', 'inotify_add_watch', 'inotify_init',
    'inotify_init1', 'inotify_rm_watch', 'io_cancel', 'io_destroy', 'io_getevents',
    'io_pgetevents', 'io_setup', 'io_submit', 'io_uring_enter', 'io_uring_register',
    'io_uring_setup', 'ioprio_get', 'ioprio_set', 'kcmp', 'kexec_file_load',
    'kexec_load', 'keyctl', 'landlock_add_rule', 'landlock_create_ruleset',
    'landlock_restrict_self', 'listen', 'lsm_get_self_attr', 'lsm_list_modules',
    'lsm_set_self_attr', 'madvise', 'map_shadow_stack', 'mbind', 'membarrier',
    'memfd_create', 'memfd_secret', 'mincore', 'mlock', 'mlock2', 'mlockall', 'modify_ldt',
    'move_pages', 'mprotect', 'mremap', 'mseal', 'migrate_pages', 'mlockall',
    'mq_getsetattr', 'mq_notify', 'mq_open', 'mq_timedreceive', 'mq_timedsend',
    'mq_unlink', 'mremap', 'msync', 'msgctl', 'msgget', 'msgrcv', 'msgsnd', 'munlock',
    'munlockall', 'name_to_handle_at',
    'open_by_handle_at', 'perf_event_open', 'pkey_alloc', 'pkey_free',
    'pkey_mprotect', 'process_madvise', 'process_mrelease', 'process_vm_readv',
    'process_vm_writev', 'quotactl', 'quotactl_fd', 'recvmmsg', 'recvfrom',
    'recvmsg', 'remap_file_pages', 'request_key', 'rseq', 'seccomp', 'semctl',
    'semget', 'semop', 'semtimedop', 'sendmmsg', 'sendmsg', 'sendto',
    'set_mempolicy', 'set_mempolicy_home_node', 'set_robust_list', 'setfsgid', 'setitimer',
    'setfsuid', 'setgid', 'setgroups', 'setregid', 'setresgid', 'setresuid',
    'setreuid', 'setsockopt', 'setuid', 'shmat', 'shmctl', 'shmdt', 'shmget', 'shutdown', 'signalfd',
    'signalfd4', 'socket', 'socketpair', 'swapoff', 'swapon', 'syslog', 'timer_create',
    'timer_delete', 'timer_getoverrun', 'timer_gettime', 'timer_settime',
    'sysfs', 'timerfd_create', 'timerfd_gettime', 'timerfd_settime', 'uretprobe', 'uselib',
    'userfaultfd',
})
EXPECTED_DISPATCH_ARM_COUNT = 346
EXPECTED_MISSING_DISPATCH = frozenset({
    'cachestat', 'futimesat', 'getdents', 'ioperm', 'iopl', 'kexec_file_load', 'kexec_load',
    'landlock_add_rule', 'landlock_create_ruleset', 'landlock_restrict_self', 'listmount',
    'lsm_get_self_attr', 'lsm_list_modules', 'lsm_set_self_attr', 'map_shadow_stack',
    'modify_ldt', 'mseal', 'pivot_root', 'pkey_alloc', 'pkey_free', 'pkey_mprotect',
    'process_mrelease', 'remap_file_pages', 'set_mempolicy_home_node', 'statmount', 'sysfs',
    'time', 'uretprobe', 'ustat',
})


class InventoryError(ValueError):
    pass


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def table_rows(path: Path = TABLE) -> list[dict[str, Any]]:
    rows = []
    for line_number, line in enumerate(path.read_text().splitlines(), 1):
        fields = line.split()
        if not fields or fields[0].startswith("#"):
            continue
        if len(fields) < 3:
            raise InventoryError(f"{path}:{line_number}: malformed syscall row")
        if fields[1] not in {"common", "64"}:
            continue
        entry = fields[3] if len(fields) > 3 else "sys_ni_syscall"
        try:
            number = int(fields[0])
        except ValueError as error:
            raise InventoryError(f"{path}:{line_number}: invalid syscall number {fields[0]!r}") from error
        rows.append({"nr": number, "abi": fields[1], "name": fields[2], "entry": entry})
    if len(rows) != EXPECTED_ROWS or len({row["nr"] for row in rows}) != EXPECTED_ROWS:
        raise InventoryError(f"{path}: expected {EXPECTED_ROWS} unique common/64 rows")
    if sum(row["entry"] == "sys_ni_syscall" for row in rows) != NI_COUNT:
        raise InventoryError(f"{path}: expected exactly {NI_COUNT} ni routes")
    return rows


def _match_body(text: str) -> str:
    """Return dispatch_syscall's match body, with balanced braces."""
    start = text.index("match sysno {") + len("match sysno {")
    depth, index = 1, start
    while depth:
        if text[index] == "{": depth += 1
        elif text[index] == "}": depth -= 1
        index += 1
    return text[start:index - 1]


def _arm_end(body: str, start: int) -> int:
    """Find the next top-level arm (or the end), preserving nested closures."""
    depth, index = 0, start
    while index < len(body):
        char = body[index]
        if char in "({[": depth += 1
        elif char in ")} ]".replace(" ", ""):
            depth -= 1
        elif char == "," and depth == 0:
            return index
        index += 1
    return len(body)


def dispatch_routes(path: Path = DISPATCH) -> dict[str, dict[str, str]]:
    text = path.read_text()
    body = _match_body(text)
    # The left side permits explicit Sysno arms and the single supported `|` arm.
    pattern = re.compile(r"(?P<cfg>#\[cfg\(feature = \"bpf\"\)\]\s*)?(?P<lhs>Sysno::[A-Za-z0-9_]+(?:\s*\|\s*Sysno::[A-Za-z0-9_]+)*)\s*=>")
    routes: dict[str, dict[str, str]] = {}
    for match in pattern.finditer(body):
        arm = body[match.end():_arm_end(body, match.end())]
        names = re.findall(r"Sysno::([A-Za-z0-9_]+)", match.group("lhs"))
        target_match = re.search(r"\b((?:compat_)?sys_[A-Za-z0-9_]+|compat_[A-Za-z0-9_]+)\s*\(", arm)
        if not target_match:
            raise InventoryError(f"{path}: cannot statically identify route for {names}")
        target = target_match.group(1)
        if match.group("cfg"):
            kind = "feature"
        elif target == "sys_ni_syscall":
            kind = "native-ni"
        elif len(names) > 1:
            kind = "fallback"
        elif target.startswith("compat_"):
            kind = "alias"
        else:
            kind = "dispatch-arm"
        for name in names:
            routes[name] = {"kind": kind, "target": target}
    return routes


def implementation_root(route: dict[str, str], text: str) -> str:
    target = route["target"]
    if route["kind"] != "alias":
        return target
    helper = re.search(rf"fn {re.escape(target)}\b[\s\S]*?\{{(?P<body>[\s\S]*?)\n\}}", text)
    if not helper:
        raise InventoryError(f"{DISPATCH}: alias helper {target} missing")
    resolved = re.search(r"\b(sys_[A-Za-z0-9_]+)\s*\(", helper.group("body"))
    if not resolved:
        raise InventoryError(f"{DISPATCH}: alias helper {target} has no sys_ root")
    return resolved.group(1)


def uapi_family(name: str) -> str:
    """Conservative lexical UAPI families; the final misc rule is intentional."""
    rules = (
        (r"^(io_(setup|destroy|submit|cancel|getevents|pgetevents|uring)|io_uring)", "async-io"),
        (r"^(socket|bind|connect|accept|listen|send|recv|get(sock|peer)|set(sock|peer)|shutdown)", "net"),
        (r"(mount|umount|fsopen|fsconfig|fsmount|fspick|open_tree|move_mount|mount_setattr|statmount|listmount|pivot_root)", "mount"),
        (r"^(setns|unshare)$", "namespace"),
        (r"(key|seccomp|bpf|landlock|lsm|cap)", "security"),
        (r"(reboot|kexec|module|syslog|iopl|ioperm|swapon|swapoff|vhangup)", "admin"),
        (r"(^creat$|open|close|read|write|stat|chmod|chown|mkdir|link|unlink|rename|fs|fd|file|xattr|dir|cwd|fcntl|flock|sync|truncate|access|umask|inotify|fanotify|epoll|poll|select|pipe|eventfd|timerfd|memfd|userfaultfd)", "fs"),
        (r"(mmap|munmap|mprotect|brk|mremap|madvise|mlock|munlock|mincore|mempolicy|mbind|migrate_pages|move_pages|process_vm)", "memory"),
        (r"(clone|fork|vfork|exec|exit|wait|sched|pid|tid|uid|gid|priority|rlimit|prctl|ptrace|setns|unshare|personality|acct|cap|ioprio)", "task"),
        (r"(sig|kill|tgkill|tkill|pause|signalfd)", "signal"),
        (r"(clock|time|timer|nanosleep|alarm|itimer|adjtimex|times)", "time"),
        (r"(futex|membarrier|rseq|robust)", "sync"),
        (r"(msg|sem|shm|mq)", "ipc"),
    )
    return next((family for pattern, family in rules if re.search(pattern, name)), "misc")


def generate(table: Path = TABLE, dispatch: Path = DISPATCH) -> dict[str, Any]:
    rows, routes, text = table_rows(table), dispatch_routes(dispatch), dispatch.read_text()
    if len(LINUX_COND_SYSCALLS) != EXPECTED_COND_SYSCALL_COUNT:
        raise InventoryError("fixed v6.12.103 COND_SYSCALL set drift")
    inventory = []
    for row in rows:
        route = routes.get(row["name"])
        linux_route = "ni" if row["entry"] == "sys_ni_syscall" else (
            "conditional" if row["name"] in LINUX_COND_SYSCALLS else "direct"
        )
        gap: str | None = None
        if route is None:
            route = {"kind": "fallback", "target": "AxError::Unsupported"}
            gap = "no explicit Sysno arm; dispatch_syscall fallback returns AxError::Unsupported"
        elif route["kind"] == "feature":
            gap = "route is compiled only with feature=bpf"
        elif route["kind"] == "fallback":
            gap = "shared explicit unsupported-fd route; runtime behavior requires evidence"
        inventory.append({
            "nr": row["nr"], "name": row["name"], "abi": row["abi"],
            "linux_route": linux_route,
            "dispatch": {"kind": route["kind"], "target": route["target"]},
            "implementation_root": implementation_root(route, text) if route["kind"] == "alias" else route["target"],
            "uapi_family": uapi_family(row["name"]),
            "static_gap": gap,
        })
    static_gaps = [
        {"nr": item["nr"], "name": item["name"], "reason": item["static_gap"]}
        for item in inventory if item["static_gap"] is not None
    ]
    return {
        "schema": "thekernel-linux-abi-static-inventory-v1",
        "scope": "x86_64 common+64 Linux syscall table; static routing only, not implementation evidence",
        "sources": {
            "syscall_64_tbl": {"path": str(table.relative_to(ROOT)), "sha256": sha256(table)},
            "dispatch": {"path": str(dispatch.relative_to(ROOT)), "sha256": sha256(dispatch)},
            "linux_cond_syscall": {
                "linux_commit": LINUX_V6_12_103_COMMIT, "path": "kernel/sys_ni.c",
                "mechanism": "fixed native-table intersection of COND_SYSCALL/COND_SYSCALL_COMPAT",
                "conditional_syscall_count": EXPECTED_COND_SYSCALL_COUNT,
            },
        },
        "syscall_count": len(inventory), "linux_ni_count": sum(item["linux_route"] == "ni" for item in inventory),
        "dispatcher_arm_count": len(routes),
        "missing_dispatch_syscalls": sorted({row["name"] for row in rows} - set(routes)),
        "static_gaps": static_gaps,
        "syscalls": inventory,
    }


def write(output: Path = OUTPUT) -> None:
    output.write_text(json.dumps(generate(), indent=2, sort_keys=True) + "\n")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", type=Path, default=OUTPUT)
    args = parser.parse_args()
    write(args.output)


if __name__ == "__main__":
    main()
