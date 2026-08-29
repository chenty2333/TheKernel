#!/usr/bin/env python3
"""Validate the pinned x86_64 Linux conditional-syscall catalog.

Membership is deliberately a fixed oracle, not a projection of a route field:
``uselib`` is a member of kernel/sys_ni.c's COND_SYSCALL list while its x86_64
table entry is a native ``sys_ni_syscall`` slot.
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parents[1]
CATALOG = ROOT / "docs/linux-abi/conditional-syscalls-v1.json"
TABLE = ROOT / "docs/linux-abi/linux-v6.12.103-arch-x86-entry-syscalls-syscall_64.tbl"
EXPECTED_COUNT = 162
FIXED_MEMBER_NAMES = frozenset("""
accept accept4 acct add_key alarm bind bpf cachestat capget capset clock_adjtime
connect copy_file_range delete_module epoll_create epoll_create1 epoll_ctl
epoll_pwait epoll_pwait2 epoll_wait eventfd eventfd2 execveat fadvise64
fanotify_init fanotify_mark finit_module flock futex futex_requeue futex_wait
futex_waitv futex_wake get_mempolicy get_robust_list getgroups getitimer
getpeername getresgid getresuid getsockname getsockopt init_module
inotify_add_watch inotify_init inotify_init1 inotify_rm_watch io_cancel
io_destroy io_getevents io_pgetevents io_setup io_submit io_uring_enter
io_uring_register io_uring_setup ioprio_get ioprio_set kcmp kexec_file_load
kexec_load keyctl landlock_add_rule landlock_create_ruleset landlock_restrict_self
listen lsm_get_self_attr lsm_list_modules lsm_set_self_attr madvise map_shadow_stack
mbind membarrier memfd_create memfd_secret migrate_pages mincore mlock mlock2 mlockall
modify_ldt move_pages mprotect mq_getsetattr mq_notify mq_open mq_timedreceive
mq_timedsend mq_unlink mremap mseal msgctl msgget msgrcv msgsnd msync munlock
munlockall name_to_handle_at open_by_handle_at perf_event_open pkey_alloc pkey_free
pkey_mprotect process_madvise process_mrelease process_vm_readv process_vm_writev
quotactl quotactl_fd recvfrom recvmmsg recvmsg remap_file_pages request_key rseq
seccomp semctl semget semop semtimedop sendmmsg sendmsg sendto set_mempolicy
set_mempolicy_home_node set_robust_list setfsgid setfsuid setgid setgroups setitimer
setregid setresgid setresuid setreuid setsockopt setuid shmat shmctl shmdt shmget
shutdown signalfd signalfd4 socket socketpair swapoff swapon sysfs syslog timer_create
timer_delete timer_getoverrun timer_gettime timer_settime timerfd_create
timerfd_gettime timerfd_settime uretprobe uselib userfaultfd
""".split())
PROFILE_IDS = frozenset({"q35-product", "q35-feature-witness"})


class CatalogError(ValueError):
    pass


def linux_table() -> dict[str, tuple[int, str]]:
    rows: dict[str, tuple[int, str]] = {}
    for line in TABLE.read_text().splitlines():
        fields = line.split()
        if not fields or fields[0].startswith("#") or fields[1] not in {"common", "64"}:
            continue
        rows[fields[2]] = (int(fields[0]), fields[3] if len(fields) > 3 else "sys_ni_syscall")
    return rows


def checked_in_case_ids() -> set[str]:
    document = json.loads((ROOT / "docs/linux-abi/abi-cases.json").read_text())
    return {case["id"] for case in document["cases"]}


def checked_in_profile_ids() -> set[str]:
    document = json.loads((ROOT / "docs/linux-abi/oracle-configs.json").read_text())
    return {profile["id"] for profile in document["oracles"]}


def _need(condition: bool, message: str) -> None:
    if not condition:
        raise CatalogError(message)


def validate(document: dict[str, Any]) -> dict[str, int]:
    _need(document.get("schema") == "thekernel-linux-conditional-syscalls-v1", "schema")
    _need(document.get("membership_oracle") == "fixed kernel/sys_ni.c COND_SYSCALL native-table intersection", "membership oracle")
    _need(PROFILE_IDS <= checked_in_profile_ids(), "required profile is not in oracle-configs")
    case_ids = checked_in_case_ids()
    rows = document.get("members")
    _need(isinstance(rows, list) and len(rows) == EXPECTED_COUNT, "expected exactly 162 members")
    table = linux_table()
    names = [row.get("name") for row in rows if isinstance(row, dict)]
    _need(len(names) == len(rows) and len(set(names)) == EXPECTED_COUNT, "duplicate or invalid member name")
    _need(set(names) == FIXED_MEMBER_NAMES, "membership differs from fixed 162-member oracle")
    resolved = unresolved = 0
    seen_numbers: set[int] = set()
    for row in rows:
        _need(set(row) == {"nr", "name", "linux_source", "predicate", "product_expected_route", "positive_witness", "fixture", "required_profiles"}, f"{row.get('name')}: schema")
        name, nr = row["name"], row["nr"]
        _need(table.get(name, (None, None))[0] == nr and nr not in seen_numbers, f"{name}: nr/table mismatch")
        seen_numbers.add(nr)
        source = row["linux_source"]
        _need(isinstance(source, dict) and set(source) == {"commit", "location", "mechanism"}, f"{name}: linux_source")
        _need(source["commit"] == "25c09b42358e73e1476e517b296edb6344f2e4bd" and source["location"] == f"kernel/sys_ni.c:COND_SYSCALL({name})" and source["mechanism"] == "COND_SYSCALL", f"{name}: pinned Linux source")
        _need(row["required_profiles"] == sorted(PROFILE_IDS), f"{name}: required profiles")
        fields = (row["predicate"], row["product_expected_route"], row["positive_witness"], row["fixture"])
        statuses = [field.get("status") if isinstance(field, dict) else None for field in fields]
        _need(all(status in {"resolved", "unresolved"} for status in statuses), f"{name}: status")
        for label, field, status in zip(("predicate", "product route", "positive witness", "fixture"), fields, statuses):
            if status == "resolved":
                _need(isinstance(field.get("value"), str) and field["value"], f"{name}: incomplete resolved {label}")
            else:
                _need(isinstance(field.get("gap"), str) and field["gap"], f"{name}: unresolved evidence requires a nonempty gap")
        if all(status == "resolved" for status in statuses):
            _need(row["positive_witness"]["value"] in PROFILE_IDS, f"{name}: positive witness profile reference")
            _need(row["fixture"]["value"] in case_ids, f"{name}: fixture case reference")
            resolved += 1
        else:
            unresolved += 1
    uselib = next(row for row in rows if row["name"] == "uselib")
    _need(table["uselib"][1] == "sys_ni_syscall", "uselib must remain a native NI table slot")
    _need(uselib["name"] in FIXED_MEMBER_NAMES, "uselib must remain a conditional member")
    return {"members": len(rows), "resolved": resolved, "unresolved": unresolved}


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--validate", type=Path, nargs="?", const=CATALOG)
    args = parser.parse_args()
    if args.validate is None:
        parser.error("use --validate [catalog]")
    stats = validate(json.loads(args.validate.read_text()))
    print(json.dumps(stats, sort_keys=True))


if __name__ == "__main__":
    main()
