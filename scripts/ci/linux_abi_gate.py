#!/usr/bin/env python3
"""Materialize and statically inventory Linux v7.2.3 x86_64 syscall routing.

This gate never treats a dispatch route as semantic handler evidence.  Deeper
handler ENOSYS behavior belongs to contract and differential gates.
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
import tomllib
from collections import Counter
from pathlib import Path
from typing import Sequence

ROOT = Path(__file__).resolve().parents[2]
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))
MANIFEST = ROOT / "config/linux-abi.toml"
CONTRACTS = ROOT / "config/linux-contracts.toml"
SOURCE = Path.home() / ".cache/thekernel-targets/linux-v7.2.3"
DISPATCH = ROOT / "kernel/src/syscall/dispatch.rs"
SYSNO = re.compile(r"\bSysno::([A-Za-z_][A-Za-z0-9_]*)\b")
WITNESS = 'cfg(feature = "bpf")'


class GateError(ValueError):
    pass


def load_manifest(path: Path) -> dict:
    try:
        data = tomllib.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, tomllib.TOMLDecodeError) as error:
        raise GateError(f"cannot read manifest {path}: {error}") from error
    if set(data) != {"schema", "linux", "routing_inventory", "routing_witness", "terminal"} or data.get("schema") != 3:
        raise GateError("manifest must contain schema = 3, linux, routing_inventory, routing_witness, and terminal")
    linux = data["linux"]
    expected = {"repository": "https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git", "tag": "v7.2.3", "table": "arch/x86/entry/syscalls/syscall_64.tbl"}
    if linux != expected:
        raise GateError("Linux release does not match the selected x86_64 baseline")
    if not isinstance(data["routing_inventory"], dict) or set(data["routing_inventory"]) != {"ordinary_explicit", "explicit_enosys", "native_fallback"}:
        raise GateError("routing inventory fields are invalid")
    if data["routing_witness"] != {"bpf": WITNESS}:
        raise GateError("routing_witness must declare only the exact BPF feature witness")
    terminal = data["terminal"]
    if (
        not isinstance(terminal, dict)
        or set(terminal) != {"ordinary_explicit", "explicit_enosys", "native_fallback"}
        or not all(isinstance(count, int) and count >= 0 for count in terminal.values())
    ):
        raise GateError("terminal routing expectation fields are invalid")
    for key, count in terminal.items():
        if len(numbers(data["routing_inventory"][key], key)) != count:
            raise GateError(f"terminal routing expectation {key}={count} does not match the routing inventory")
    return data


def numbers(values: object, label: str) -> set[int]:
    if not isinstance(values, list):
        raise GateError(f"routing_inventory.{label} must be an array")
    result: set[int] = set()
    for value in values:
        if isinstance(value, int) and value >= 0:
            expanded = range(value, value + 1)
        elif isinstance(value, str) and re.fullmatch(r"0|[1-9]\d*", value):
            expanded = range(int(value), int(value) + 1)
        elif isinstance(value, str) and re.fullmatch(r"(?:0|[1-9]\d*)-(?:0|[1-9]\d*)", value):
            first, last = map(int, value.split("-"))
            if first > last:
                raise GateError(f"routing_inventory.{label} has reversed range {value}")
            expanded = range(first, last + 1)
        else:
            raise GateError(f"routing_inventory.{label} has invalid number/range {value!r}")
        for number in expanded:
            if number in result:
                raise GateError(f"routing_inventory.{label} repeats syscall {number}")
            result.add(number)
    return result


def parse_table(path: Path) -> dict[int, str]:
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except (OSError, UnicodeError) as error:
        raise GateError(f"cannot read syscall table {path}: {error}") from error
    entries: dict[int, str] = {}
    for line in lines:
        fields = line.split("#", 1)[0].split()
        if not fields or len(fields) < 3 or fields[1] not in {"common", "64"}:
            continue
        if not fields[0].isdigit() or not re.fullmatch(r"[A-Za-z0-9_]+", fields[2]):
            raise GateError(f"invalid native syscall table line: {line}")
        if int(fields[0]) in entries:
            raise GateError(f"duplicate native syscall number {fields[0]}")
        entries[int(fields[0])] = fields[2]
    if len(entries) != 385:
        raise GateError(f"expected 385 native common+64 syscalls, found {len(entries)}")
    return entries


def states(manifest: dict, entries: dict[int, str]) -> dict[int, str]:
    result: dict[int, str] = {}
    for state, key in (("ordinary-explicit", "ordinary_explicit"), ("explicit-enosys", "explicit_enosys"), ("native-fallback", "native_fallback")):
        for number in numbers(manifest["routing_inventory"][key], key):
            if number not in entries or number in result:
                raise GateError(f"invalid routing inventory syscall {number}")
            result[number] = state
    if set(result) != set(entries):
        raise GateError(f"routing inventory does not cover table syscalls: {sorted(set(entries) - set(result))}")
    return result


def blank(text: str) -> str:
    return "".join("\n" if char == "\n" else " " for char in text)


def mask_rust_noncode(source: str) -> str:
    """Mask comments and normal/byte/raw/raw-byte strings and char literals."""
    result: list[str] = []
    index = 0
    block = 0
    while index < len(source):
        if block:
            if source.startswith("/*", index): block += 1; result.append("  "); index += 2
            elif source.startswith("*/", index): block -= 1; result.append("  "); index += 2
            else: result.append("\n" if source[index] == "\n" else " "); index += 1
            continue
        if source.startswith("//", index):
            end = source.find("\n", index); end = len(source) if end < 0 else end
            result.append(blank(source[index:end])); index = end; continue
        if source.startswith("/*", index): block = 1; result.append("  "); index += 2; continue
        prefix = "br" if source.startswith("br", index) else "r"
        quote = index + len(prefix)
        while quote < len(source) and source[quote] == "#": quote += 1
        if source.startswith(prefix, index) and quote < len(source) and source[quote] == '"':
            endmark = '"' + source[index + len(prefix):quote]
            end = source.find(endmark, quote + 1)
            if end < 0: raise GateError("unterminated raw string in dispatch")
            end += len(endmark); result.append(blank(source[index:end])); index = end; continue
        if source.startswith('b"', index) or source[index] == '"':
            start = index + 1 if source.startswith('b"', index) else index
            end = start + 1
            while end < len(source) and source[end] != '"': end += 2 if source[end] == "\\" else 1
            if end >= len(source): raise GateError("unterminated string in dispatch")
            end += 1; result.append(blank(source[index:end])); index = end; continue
        opening = index + 1 if source.startswith("b'", index) else index
        if source[index] == "'" or source.startswith("b'", index):
            end = opening + 1; limit = min(len(source), opening + 32)
            while end < limit and source[end] not in "\n'": end += 2 if source[end] == "\\" else 1
            if end < limit and source[end] == "'": end += 1; result.append(blank(source[index:end])); index = end; continue
        result.append(source[index]); index += 1
    if block: raise GateError("unterminated block comment in dispatch")
    return "".join(result)


def matching_end(masked: str, opening: int, context: str) -> int:
    depth = 0
    for index in range(opening, len(masked)):
        if masked[index] == "{": depth += 1
        elif masked[index] == "}":
            depth -= 1
            if depth == 0: return index
    raise GateError(f"{context} is unterminated")


def brace_depth(masked: str, end: int) -> int:
    return sum(1 if char == "{" else -1 if char == "}" else 0 for char in masked[:end])


def top_level_match(masked: str, beginning: int, finish: int, pattern: re.Pattern[str], context: str) -> re.Match[str]:
    candidates = [
        match for match in pattern.finditer(masked, beginning, finish)
        if brace_depth(masked[beginning:finish], match.start() - beginning) == 0
    ]
    if len(candidates) != 1:
        raise GateError(f"{context} must have exactly one top-level match")
    return candidates[0]


def arms(path: Path) -> list[tuple[str, str]]:
    source = path.read_text(encoding="utf-8")
    masked = mask_rust_noncode(source)
    function = top_level_match(
        masked, 0, len(masked),
        re.compile(r"(?m)^\s*(?:#\[[^\]]+\]\s*)*(?:pub(?:\s*\([^)]*\))?\s+)?fn\s+dispatch_syscall\s*\("),
        "dispatch_syscall function",
    )
    begin = masked.find("{", function.end())
    finish = matching_end(masked, begin, "dispatch_syscall function")
    match = top_level_match(
        masked, begin + 1, finish, re.compile(r"\bmatch\s+sysno\s*\{"),
        "dispatch_syscall match sysno",
    )
    begin = masked.find("{", match.start(), match.end())
    finish = matching_end(masked, begin, "dispatch_syscall match sysno")
    body, masked = source[begin + 1:finish], masked[begin + 1:finish]
    result: list[tuple[str, str]] = []; start = index = depth = 0
    while index < len(body):
        char = masked[index]
        if char in "{([": depth += 1
        elif char in "})]": depth -= 1
        elif depth == 0 and masked[index:index + 2] == "=>":
            pattern = body[start:index].strip(); expression = index + 2; index = expression; inner = 0
            while index < len(body):
                char = masked[index]
                if char in "{([": inner += 1
                elif char in "})]": inner -= 1
                elif char == "," and inner == 0:
                    result.append((pattern, body[expression:index].strip())); start = index + 1; break
                elif inner == 0 and masked.startswith("Sysno::", index) and body[expression:index].strip():
                    result.append((pattern, body[expression:index].strip())); start = index; break
                index += 1
            else:
                # A trailing block arm ends at the match boundary without a
                # comma (rustfmt drops it); flush it instead of losing it.
                if body[expression:index].strip():
                    result.append((pattern, body[expression:index].strip()))
        index += 1
    return result


def routes(path: Path, table: set[str], witness: str) -> tuple[set[str], set[str], list[tuple[str, str]]]:
    parsed = arms(path)
    raw = mask_rust_noncode(path.read_text(encoding="utf-8"))
    if path.resolve() == DISPATCH.resolve():
        entry = mask_rust_noncode(path.with_name("mod.rs").read_text(encoding="utf-8"))
        seccomp = entry.find("seccomp::enforce_syscall_seccomp(uctx)")
        explicit = entry.find("dispatch::dispatch_new_syscall(uctx.sysno())")
        decode = entry.find("Sysno::new(uctx.sysno())")
        if not 0 <= seccomp < explicit < decode:
            raise GateError("raw Linux dispatch must run after seccomp and before enum decoding")
    if {"listns", "rseq_slice_yield"} <= table and re.search(r"fn\s+dispatch_new_syscall\(number: usize\)\s*->\s*Option<AxResult<isize>>\s*\{\s*match number\s*\{\s*470\s*\|\s*471\s*=>\s*Some\(sys_ni_syscall\(\)\),\s*_\s*=>\s*None,?\s*\}\s*\}", raw) is None:
        raise GateError("missing explicit Linux 7.2 raw-number ENOSYS routing")
    for pattern, _ in parsed:
        masked_pattern = mask_rust_noncode(pattern)
        names = set(SYSNO.findall(masked_pattern)) & table
        if not names: continue
        if re.search(r"\bif\b", masked_pattern): raise GateError(f"native syscall route(s) {sorted(names)} may not use a match guard")
        # Attributes carry string literals (cfg(feature = "bpf")), so read them
        # from the raw pattern; the masked form would blank the feature name.
        attrs = [re.sub(r"\s+", " ", item.strip()) for item in re.findall(r"#\[([^]]+)\]", pattern, re.DOTALL)]
        if attrs and not (names == {"bpf"} and attrs == [witness]) and not (names == {"vfork"} and attrs == ['cfg(target_arch = "x86_64")']):
            raise GateError(f"native syscall route(s) {sorted(names)} have unsupported conditional attribute(s) {attrs}")
    ni_patterns = [pattern for pattern, expression in parsed if re.fullmatch(r"\s*sys_ni_syscall\s*\(\s*\)\s*", mask_rust_noncode(expression))]
    if len(ni_patterns) != 1: raise GateError("dispatch has no explicit sys_ni_syscall arm")
    raw_names = " | Sysno::listns | Sysno::rseq_slice_yield" if {"listns", "rseq_slice_yield"} <= table else ""
    parsed = [(pattern + raw_names if pattern == ni_patterns[0] else pattern, expression) for pattern, expression in parsed]
    ni_patterns[0] += raw_names
    ni = set(SYSNO.findall(mask_rust_noncode(ni_patterns[0]))); all_routes = [name for pattern, _ in parsed for name in SYSNO.findall(mask_rust_noncode(pattern))]
    repeats = sorted(name for name, count in Counter(all_routes).items() if count > 1)
    if repeats: raise GateError(f"dispatch repeats syscall route(s): {repeats}")
    found = set(all_routes)
    if found - table: raise GateError(f"dispatch names absent from selected Linux table: {sorted(found - table)}")
    if sum(re.sub(r"(?s)^\s*#\[[^]]+\]\s*", "", pattern).strip() == "_" for pattern, _ in parsed) != 1:
        raise GateError("dispatch must have exactly one default arm for table-external syscall numbers")
    return found, ni, parsed


def inventory(manifest_path: Path, source: Path, dispatch: Path) -> None:
    manifest = load_manifest(manifest_path); entries = parse_table(source / manifest["linux"]["table"]); matrix = states(manifest, entries)
    found, ni, _ = routes(dispatch, set(entries.values()), manifest["routing_witness"]["bpf"])
    expected_ni = {entries[number] for number, state in matrix.items() if state == "explicit-enosys"}; fallback = {entries[number] for number, state in matrix.items() if state == "native-fallback"}
    if ni != expected_ni or found != set(entries.values()) - fallback or found & fallback: raise GateError("explicit dispatch routes do not match routing inventory")
    counts = Counter(matrix.values()); print(f"linux-abi inventory: ordinary-explicit={counts['ordinary-explicit']} explicit-enosys={counts['explicit-enosys']} native-fallback={counts['native-fallback']}")


CONTRACT_FIELDS = {
    "id", "flags", "structs", "multiplexer_commands", "provider_ioctls",
    "errno_order", "usercopy", "state", "concurrency", "teardown",
}
CELL_FIELDS = {"number", "name", "status", "contract", "handler", "conditional", "tests", "validation_gaps", "limitations"}
DISPATCH_CALLS = {
    "kernel/src/syscall/task/wait.rs:sys_waitpid": "sys_waitpid",
    "kernel/src/syscall/task/wait.rs:sys_waitid": "sys_waitid",
    "kernel/src/syscall/task/thread.rs:sys_modify_ldt": "sys_modify_ldt",
    "kernel/src/syscall/task/ctl.rs:sys_prctl": "sys_prctl",
    "kernel/src/syscall/task/thread.rs:sys_arch_prctl": "sys_arch_prctl",
    "kernel/src/syscall/sync/rseq.rs:sys_rseq": "sys_rseq",
    "kernel/src/syscall/task/clone3.rs:sys_clone3": "sys_clone3",
    "kernel/src/syscall/mm/process_vm.rs:sys_process_mrelease": "sys_process_mrelease",

    "kernel/src/syscall/fs/mount.rs:sys_umount2": "sys_umount2",
    "kernel/src/syscall/fs/fd_ops.rs:sys_flock": "sys_flock",
    "kernel/src/syscall/fs/ctl.rs:sys_utimensat": "sys_utimensat",
    "kernel/src/syscall/fs/io.rs:sys_fallocate": "sys_fallocate",
    "kernel/src/syscall/fs/io.rs:sys_readahead": "sys_readahead",

    "kernel/src/syscall/task/acct.rs:sys_acct": "sys_acct",
    "kernel/src/syscall/mm/swap.rs:sys_swapon": "sys_swapon",
    "kernel/src/syscall/mm/swap.rs:sys_swapoff": "sys_swapoff",
    "kernel/src/syscall/task/module.rs:sys_init_module": "sys_init_module",
    "kernel/src/syscall/task/module.rs:sys_finit_module": "sys_finit_module",
    "kernel/src/syscall/task/module.rs:sys_delete_module": "sys_delete_module",
    "kernel/src/syscall/task/kexec.rs:sys_kexec_load": "sys_kexec_load",
    "kernel/src/syscall/task/kexec.rs:sys_kexec_file_load": "sys_kexec_file_load",
    "kernel/src/syscall/task/keys.rs:sys_add_key": "sys_add_key",
    "kernel/src/syscall/task/keys.rs:sys_request_key": "sys_request_key",
    "kernel/src/syscall/task/keys.rs:sys_keyctl": "sys_keyctl",
    "kernel/src/syscall/task/perf.rs:sys_perf_event_open": "sys_perf_event_open",

    "kernel/src/syscall/task/ptrace.rs:sys_ptrace": "sys_ptrace",

    "kernel/src/syscall/task/job.rs:sys_setpgid": "sys_setpgid",
    "kernel/src/syscall/dispatch.rs:compat_getpgrp": "compat_getpgrp",
    "kernel/src/syscall/task/job.rs:sys_setsid": "sys_setsid",
    "kernel/src/syscall/task/job.rs:sys_getpgid": "sys_getpgid",
    "kernel/src/syscall/task/job.rs:sys_getsid": "sys_getsid",
    "kernel/src/syscall/task/ctl.rs:sys_setreuid": "sys_setreuid",
    "kernel/src/syscall/task/ctl.rs:sys_setregid": "sys_setregid",
    "kernel/src/syscall/task/ctl.rs:sys_setresuid": "sys_setresuid",
    "kernel/src/syscall/task/ctl.rs:sys_setresgid": "sys_setresgid",
    "kernel/src/syscall/task/ctl.rs:sys_capget": "sys_capget",
    "kernel/src/syscall/task/ctl.rs:sys_capset": "sys_capset",
    "kernel/src/syscall/task/ctl.rs:sys_mbind": "sys_mbind",
    "kernel/src/syscall/task/ctl.rs:sys_set_mempolicy": "sys_set_mempolicy",
    "kernel/src/syscall/task/ctl.rs:sys_get_mempolicy": "sys_get_mempolicy",
    "kernel/src/syscall/task/ctl.rs:sys_migrate_pages": "sys_migrate_pages",
    "kernel/src/syscall/task/ctl.rs:sys_move_pages": "sys_move_pages",
    "kernel/src/syscall/task/ctl.rs:sys_set_mempolicy_home_node": "sys_set_mempolicy_home_node",
    "kernel/src/syscall/task/ctl.rs:sys_unshare": "sys_unshare",
    "kernel/src/syscall/task/ctl.rs:sys_setns": "sys_setns",
    "kernel/src/syscall/task/ctl.rs:sys_kcmp": "sys_kcmp",
    "kernel/src/syscall/landlock.rs:sys_lsm_get_self_attr": "sys_lsm_get_self_attr",
    "kernel/src/syscall/landlock.rs:sys_lsm_set_self_attr": "sys_lsm_set_self_attr",
    "kernel/src/syscall/landlock.rs:sys_lsm_list_modules": "sys_lsm_list_modules",

    "kernel/src/syscall/task/schedule.rs:sys_sched_yield": "sys_sched_yield",
    "kernel/src/syscall/task/schedule.rs:sys_nanosleep": "sys_nanosleep",
    "kernel/src/syscall/task/schedule.rs:sys_sched_setaffinity": "sys_sched_setaffinity",
    "kernel/src/syscall/task/schedule.rs:sys_sched_getaffinity": "sys_sched_getaffinity",
    "kernel/src/syscall/task/schedule.rs:sys_getcpu": "sys_getcpu",
    "kernel/src/syscall/task/schedule.rs:sys_sched_setparam": "sys_sched_setparam",
    "kernel/src/syscall/task/schedule.rs:sys_sched_setscheduler": "sys_sched_setscheduler",
    "kernel/src/syscall/task/schedule.rs:sys_sched_getparam": "sys_sched_getparam",
    "kernel/src/syscall/task/schedule.rs:sys_sched_getscheduler": "sys_sched_getscheduler",
    "kernel/src/syscall/task/schedule.rs:sys_sched_rr_get_interval": "sys_sched_rr_get_interval",
    "kernel/src/syscall/task/schedule.rs:sys_sched_get_priority_max": "sys_sched_get_priority_max",
    "kernel/src/syscall/task/schedule.rs:sys_sched_get_priority_min": "sys_sched_get_priority_min",
    "kernel/src/syscall/task/schedule.rs:sys_getpriority": "sys_getpriority",
    "kernel/src/syscall/task/schedule.rs:sys_setpriority": "sys_setpriority",
    "kernel/src/syscall/task/schedule.rs:sys_clock_nanosleep": "sys_clock_nanosleep",
    "kernel/src/syscall/task/schedule.rs:sys_ioprio_set": "sys_ioprio_set",
    "kernel/src/syscall/task/schedule.rs:sys_ioprio_get": "sys_ioprio_get",
    "kernel/src/syscall/task/schedule.rs:sys_sched_setattr": "sys_sched_setattr",
    "kernel/src/syscall/task/schedule.rs:sys_sched_getattr": "sys_sched_getattr",
    "kernel/src/syscall/task/ioport.rs:sys_ioperm": "sys_ioperm",
    "kernel/src/syscall/task/ioport.rs:sys_iopl": "sys_iopl",

    "kernel/src/syscall/dispatch.rs:compat_mknod": "compat_mknod",
    "kernel/src/syscall/dispatch.rs:compat_inotify_init": "compat_inotify_init",
    "kernel/src/syscall/fs/inotify.rs:sys_inotify_add_watch": "sys_inotify_add_watch",
    "kernel/src/syscall/fs/inotify.rs:sys_inotify_rm_watch": "sys_inotify_rm_watch",
    "kernel/src/syscall/fs/inotify.rs:sys_inotify_init1": "sys_inotify_init1",
    "kernel/src/syscall/dispatch.rs:compat_signalfd": "compat_signalfd",
    "kernel/src/syscall/fs/signalfd.rs:sys_signalfd4": "sys_signalfd4",
    "kernel/src/syscall/fs/timerfd.rs:sys_timerfd_create": "sys_timerfd_create",
    "kernel/src/syscall/fs/timerfd.rs:sys_timerfd_settime": "sys_timerfd_settime",
    "kernel/src/syscall/fs/timerfd.rs:sys_timerfd_gettime": "sys_timerfd_gettime",
    "kernel/src/syscall/fs/fanotify.rs:sys_fanotify_init": "sys_fanotify_init",
    "kernel/src/syscall/fs/fanotify.rs:sys_fanotify_mark": "sys_fanotify_mark",
    "kernel/src/syscall/fs/quota.rs:sys_quotactl": "sys_quotactl",
    "kernel/src/syscall/fs/quota.rs:sys_quotactl_fd": "sys_quotactl_fd",
    "kernel/src/syscall/fs/mount.rs:sys_mount": "sys_mount",
    "kernel/src/syscall/fs/mount.rs:sys_pivot_root": "sys_pivot_root",
    "kernel/src/syscall/fs/mount.rs:sys_open_tree": "sys_open_tree",
    "kernel/src/syscall/fs/mount.rs:sys_move_mount": "sys_move_mount",
    "kernel/src/syscall/fs/mount.rs:sys_fsopen": "sys_fsopen",
    "kernel/src/syscall/fs/mount.rs:sys_fsconfig": "sys_fsconfig",
    "kernel/src/syscall/fs/mount.rs:sys_fsmount": "sys_fsmount",
    "kernel/src/syscall/fs/mount.rs:sys_fspick": "sys_fspick",
    "kernel/src/syscall/fs/mount.rs:sys_statmount": "sys_statmount",
    "kernel/src/syscall/fs/mount.rs:sys_listmount": "sys_listmount",
    "kernel/src/syscall/fs/mount.rs:sys_mount_setattr": "sys_mount_setattr",

    "kernel/src/syscall/fs/fd_ops.rs:sys_open": "sys_open",
    "kernel/src/syscall/fs/fd_ops.rs:sys_openat": "sys_openat",
    "kernel/src/syscall/fs/fd_ops.rs:sys_openat2": "sys_openat2",
    "kernel/src/syscall/fs/fd_ops.rs:sys_name_to_handle_at": "sys_name_to_handle_at",
    "kernel/src/syscall/fs/fd_ops.rs:sys_open_by_handle_at": "sys_open_by_handle_at",
    "kernel/src/syscall/fs/fd_ops.rs:sys_close": "sys_close",
    "kernel/src/syscall/fs/fd_ops.rs:sys_close_range": "sys_close_range",
    "kernel/src/syscall/fs/fd_ops.rs:sys_dup": "sys_dup",
    "kernel/src/syscall/fs/fd_ops.rs:sys_dup2": "sys_dup2",
    "kernel/src/syscall/fs/fd_ops.rs:sys_dup3": "sys_dup3",
    "kernel/src/syscall/fs/fd_ops.rs:sys_fcntl": "sys_fcntl",
    "kernel/src/syscall/fs/ctl.rs:sys_sysfs": "sys_sysfs",
    "kernel/src/syscall/fs/ctl.rs:sys_chdir": "sys_chdir",
    "kernel/src/syscall/fs/ctl.rs:sys_fchdir": "sys_fchdir",
    "kernel/src/syscall/fs/ctl.rs:sys_chroot": "sys_chroot",
    "kernel/src/syscall/fs/ctl.rs:sys_mkdir": "sys_mkdir",
    "kernel/src/syscall/fs/ctl.rs:sys_mkdirat": "sys_mkdirat",
    "kernel/src/syscall/fs/ctl.rs:sys_mknodat": "sys_mknodat",
    "kernel/src/syscall/fs/ctl.rs:sys_getdents": "sys_getdents",
    "kernel/src/syscall/fs/ctl.rs:sys_getdents64": "sys_getdents64",
    "kernel/src/syscall/fs/ctl.rs:sys_link": "sys_link",
    "kernel/src/syscall/fs/ctl.rs:sys_linkat": "sys_linkat",
    "kernel/src/syscall/fs/ctl.rs:sys_unlink": "sys_unlink",
    "kernel/src/syscall/fs/ctl.rs:sys_unlinkat": "sys_unlinkat",
    "kernel/src/syscall/fs/ctl.rs:sys_rmdir": "sys_rmdir",
    "kernel/src/syscall/fs/ctl.rs:sys_symlink": "sys_symlink",
    "kernel/src/syscall/fs/ctl.rs:sys_symlinkat": "sys_symlinkat",
    "kernel/src/syscall/fs/ctl.rs:sys_readlink": "sys_readlink",
    "kernel/src/syscall/fs/ctl.rs:sys_readlinkat": "sys_readlinkat",
    "kernel/src/syscall/fs/ctl.rs:sys_chown": "sys_chown",
    "kernel/src/syscall/fs/ctl.rs:sys_lchown": "sys_lchown",
    "kernel/src/syscall/fs/ctl.rs:sys_fchown": "sys_fchown",
    "kernel/src/syscall/fs/ctl.rs:sys_fchownat": "sys_fchownat",
    "kernel/src/syscall/fs/ctl.rs:sys_chmod": "sys_chmod",
    "kernel/src/syscall/fs/ctl.rs:sys_fchmod": "sys_fchmod",
    "kernel/src/syscall/fs/ctl.rs:sys_fchmodat": "sys_fchmodat",
    "kernel/src/syscall/fs/ctl.rs:sys_utime": "sys_utime",
    "kernel/src/syscall/fs/ctl.rs:sys_utimes": "sys_utimes",
    "kernel/src/syscall/fs/ctl.rs:sys_futimesat": "sys_futimesat",
    "kernel/src/syscall/fs/ctl.rs:sys_rename": "sys_rename",
    "kernel/src/syscall/fs/ctl.rs:sys_renameat": "sys_renameat",
    "kernel/src/syscall/fs/ctl.rs:sys_renameat2": "sys_renameat2",
    "kernel/src/syscall/fs/ctl.rs:sys_sync": "sys_sync",
    "kernel/src/syscall/fs/ctl.rs:sys_vhangup": "sys_vhangup",
    "kernel/src/syscall/fs/ctl.rs:sys_syncfs": "sys_syncfs",
    "kernel/src/syscall/fs/ctl.rs:sys_getcwd": "sys_getcwd",
    "kernel/src/syscall/fs/ctl.rs:sys_reboot": "sys_reboot",
    "kernel/src/syscall/fs/io.rs:sys_read": "sys_read",
    "kernel/src/syscall/fs/io.rs:sys_write": "sys_write",
    "kernel/src/syscall/fs/io.rs:sys_readv": "sys_readv",
    "kernel/src/syscall/fs/io.rs:sys_writev": "sys_writev",
    "kernel/src/syscall/fs/io.rs:sys_lseek": "sys_lseek",
    "kernel/src/syscall/fs/io.rs:sys_pread64": "sys_pread64",
    "kernel/src/syscall/fs/io.rs:sys_pwrite64": "sys_pwrite64",
    "kernel/src/syscall/fs/io.rs:sys_preadv": "sys_preadv",
    "kernel/src/syscall/fs/io.rs:sys_pwritev": "sys_pwritev",
    "kernel/src/syscall/fs/io.rs:sys_sendfile": "sys_sendfile",
    "kernel/src/syscall/fs/io.rs:sys_splice": "sys_splice",
    "kernel/src/syscall/fs/io.rs:sys_copy_file_range": "sys_copy_file_range",
    "kernel/src/syscall/fs/io.rs:sys_fsync": "sys_fsync",
    "kernel/src/syscall/fs/io.rs:sys_fdatasync": "sys_fdatasync",
    "kernel/src/syscall/fs/io.rs:sys_truncate": "sys_truncate",
    "kernel/src/syscall/fs/io.rs:sys_ftruncate": "sys_ftruncate",
    "kernel/src/syscall/fs/io.rs:sys_fadvise64": "sys_fadvise64",
    "kernel/src/syscall/fs/io.rs:sys_sync_file_range": "sys_sync_file_range",
    "kernel/src/syscall/fs/io.rs:sys_preadv2": "sys_preadv2",
    "kernel/src/syscall/fs/io.rs:sys_pwritev2": "sys_pwritev2",
    "kernel/src/syscall/fs/io.rs:sys_tee": "sys_tee",
    "kernel/src/syscall/fs/io.rs:sys_vmsplice": "sys_vmsplice",
    "kernel/src/syscall/fs/aio.rs:sys_io_setup": "sys_io_setup",
    "kernel/src/syscall/fs/aio.rs:sys_io_submit": "sys_io_submit",
    "kernel/src/syscall/fs/aio.rs:sys_io_getevents": "sys_io_getevents",
    "kernel/src/syscall/fs/aio.rs:sys_io_pgetevents": "sys_io_pgetevents",
    "kernel/src/syscall/fs/aio.rs:sys_io_destroy": "sys_io_destroy",
    "kernel/src/syscall/fs/aio.rs:sys_io_cancel": "sys_io_cancel",
    "kernel/src/syscall/fs/pipe.rs:sys_pipe2": "sys_pipe2",
    "kernel/src/syscall/fs/pidfd.rs:sys_pidfd_open": "sys_pidfd_open",
    "kernel/src/syscall/fs/pidfd.rs:sys_pidfd_getfd": "sys_pidfd_getfd",
    "kernel/src/syscall/fs/pidfd.rs:sys_pidfd_send_signal": "sys_pidfd_send_signal",
    "kernel/src/syscall/fs/cachestat.rs:sys_cachestat": "sys_cachestat",

    "kernel/src/syscall/task/clone.rs:sys_clone": "sys_clone",
    "kernel/src/syscall/task/clone.rs:sys_fork": "sys_fork",
    "kernel/src/syscall/task/clone.rs:sys_vfork": "sys_vfork",
    "kernel/src/syscall/task/execve.rs:sys_execve": "sys_execve",
    "kernel/src/syscall/task/execve.rs:sys_execveat": "sys_execveat",

    "kernel/src/syscall/mm/mmap.rs:sys_mremap": "sys_mremap",
    "kernel/src/syscall/mm/mmap.rs:sys_msync": "sys_msync",
    "kernel/src/syscall/mm/mmap.rs:sys_mlock": "sys_mlock",
    "kernel/src/syscall/mm/mmap.rs:sys_mlock2": "sys_mlock2",
    "kernel/src/syscall/mm/mmap.rs:sys_munlock": "sys_munlock",
    "kernel/src/syscall/mm/mmap.rs:sys_mlockall": "sys_mlockall",
    "kernel/src/syscall/mm/mmap.rs:sys_munlockall": "sys_munlockall",
    "kernel/src/syscall/mm/mmap.rs:sys_remap_file_pages": "sys_remap_file_pages",
    "kernel/src/syscall/sync/futex.rs:sys_set_robust_list": "sys_set_robust_list",
    "kernel/src/syscall/sync/futex.rs:sys_get_robust_list": "sys_get_robust_list",
    "kernel/src/syscall/io_mpx/poll.rs:sys_poll": "sys_poll",
    "kernel/src/syscall/io_mpx/poll.rs:sys_ppoll": "sys_ppoll",
    "kernel/src/syscall/mm/mmap.rs:sys_pkey_mprotect": "sys_pkey_mprotect",
    "kernel/src/syscall/mm/mmap.rs:sys_pkey_free": "sys_pkey_free",
    "kernel/src/syscall/mm/process_vm.rs:sys_process_madvise": "sys_process_madvise",
    "kernel/src/syscall/mm/mmap.rs:sys_map_shadow_stack": "sys_map_shadow_stack",
    "kernel/src/syscall/dispatch.rs:compat_epoll_create": "compat_epoll_create",
    "kernel/src/syscall/io_mpx/epoll.rs:sys_epoll_create1": "sys_epoll_create1",
    "kernel/src/syscall/io_mpx/epoll.rs:sys_epoll_wait": "sys_epoll_wait",
    "kernel/src/syscall/io_mpx/epoll.rs:sys_epoll_pwait": "sys_epoll_pwait",
    "kernel/src/syscall/io_mpx/epoll.rs:sys_epoll_pwait2": "sys_epoll_pwait2",

    "kernel/src/syscall/task/thread.rs:sys_getpid": "sys_getpid",
    "kernel/src/syscall/task/thread.rs:sys_getppid": "sys_getppid",
    "kernel/src/syscall/task/thread.rs:sys_gettid": "sys_gettid",
    "kernel/src/syscall/task/thread.rs:sys_set_tid_address": "sys_set_tid_address",
    "kernel/src/syscall/signal.rs:sys_rt_sigaction": "sys_rt_sigaction",
    "kernel/src/syscall/signal.rs:sys_rt_sigprocmask": "sys_rt_sigprocmask",
    "kernel/src/syscall/signal.rs:sys_rt_sigreturn": "sys_rt_sigreturn",
    "kernel/src/syscall/signal.rs:sys_pause": "sys_pause",
    "kernel/src/syscall/signal.rs:sys_kill": "sys_kill",
    "kernel/src/syscall/signal.rs:sys_rt_sigpending": "sys_rt_sigpending",
    "kernel/src/syscall/signal.rs:sys_rt_sigtimedwait": "sys_rt_sigtimedwait",
    "kernel/src/syscall/signal.rs:sys_rt_sigqueueinfo": "sys_rt_sigqueueinfo",
    "kernel/src/syscall/signal.rs:sys_rt_sigsuspend": "sys_rt_sigsuspend",
    "kernel/src/syscall/signal.rs:sys_sigaltstack": "sys_sigaltstack",
    "kernel/src/syscall/signal.rs:sys_tkill": "sys_tkill",
    "kernel/src/syscall/signal.rs:sys_tgkill": "sys_tgkill",
    "kernel/src/syscall/signal.rs:sys_rt_tgsigqueueinfo": "sys_rt_tgsigqueueinfo",

    "kernel/src/syscall/time.rs:sys_getitimer": "sys_getitimer",
    "kernel/src/syscall/time.rs:sys_alarm": "sys_alarm",
    "kernel/src/syscall/time.rs:sys_setitimer": "sys_setitimer",
    "kernel/src/syscall/time.rs:sys_gettimeofday": "sys_gettimeofday",
    "kernel/src/syscall/time.rs:sys_times": "sys_times",
    "kernel/src/syscall/time.rs:sys_adjtimex": "sys_adjtimex",
    "kernel/src/syscall/time.rs:sys_settimeofday": "sys_settimeofday",
    "kernel/src/syscall/time.rs:sys_timer_create": "sys_timer_create",
    "kernel/src/syscall/time.rs:sys_timer_settime": "sys_timer_settime",
    "kernel/src/syscall/time.rs:sys_timer_gettime": "sys_timer_gettime",
    "kernel/src/syscall/time.rs:sys_timer_getoverrun": "sys_timer_getoverrun",
    "kernel/src/syscall/time.rs:sys_timer_delete": "sys_timer_delete",
    "kernel/src/syscall/time.rs:sys_clock_settime": "sys_clock_settime",
    "kernel/src/syscall/time.rs:sys_clock_gettime": "sys_clock_gettime",
    "kernel/src/syscall/time.rs:sys_clock_getres": "sys_clock_getres",
    "kernel/src/syscall/time.rs:sys_clock_adjtime": "sys_clock_adjtime",
    "kernel/src/syscall/sys.rs:sys_syslog": "sys_syslog",
    "kernel/src/syscall/sys.rs:sys_restart_syscall": "sys_restart_syscall",
    "kernel/src/syscall/sys.rs:sys_getrandom": "sys_getrandom",

    "kernel/src/syscall/ipc/mqueue.rs:sys_mq_open": "sys_mq_open",
    "kernel/src/syscall/ipc/mqueue.rs:sys_mq_unlink": "sys_mq_unlink",
    "kernel/src/syscall/ipc/mqueue.rs:sys_mq_timedsend": "sys_mq_timedsend",
    "kernel/src/syscall/ipc/mqueue.rs:sys_mq_timedreceive": "sys_mq_timedreceive",
    "kernel/src/syscall/ipc/mqueue.rs:sys_mq_notify": "sys_mq_notify",
    "kernel/src/syscall/ipc/mqueue.rs:sys_mq_getsetattr": "sys_mq_getsetattr",

    "kernel/src/syscall/ipc/msg.rs:sys_msgget": "sys_msgget",
    "kernel/src/syscall/ipc/msg.rs:sys_msgsnd": "sys_msgsnd",
    "kernel/src/syscall/ipc/msg.rs:sys_msgrcv": "sys_msgrcv",
    "kernel/src/syscall/ipc/msg.rs:sys_msgctl": "sys_msgctl",
    "kernel/src/syscall/ipc/sem.rs:sys_semget": "sys_semget",
    "kernel/src/syscall/ipc/sem.rs:sys_semop": "sys_semop",
    "kernel/src/syscall/ipc/sem.rs:sys_semctl": "sys_semctl",
    "kernel/src/syscall/ipc/sem.rs:sys_semtimedop": "sys_semtimedop",
    "kernel/src/syscall/ipc/shm.rs:sys_shmget": "sys_shmget",
    "kernel/src/syscall/ipc/shm.rs:sys_shmat": "sys_shmat",
    "kernel/src/syscall/ipc/shm.rs:sys_shmctl": "sys_shmctl",
    "kernel/src/syscall/ipc/shm.rs:sys_shmdt": "sys_shmdt",

    "kernel/src/syscall/sys.rs:sys_getuid": "sys_getuid",
    "kernel/src/syscall/sys.rs:sys_geteuid": "sys_geteuid",
    "kernel/src/syscall/sys.rs:sys_getresuid": "sys_getresuid",
    "kernel/src/syscall/sys.rs:sys_getgid": "sys_getgid",
    "kernel/src/syscall/sys.rs:sys_getegid": "sys_getegid",
    "kernel/src/syscall/sys.rs:sys_getresgid": "sys_getresgid",
    "kernel/src/syscall/sys.rs:sys_setuid": "sys_setuid",
    "kernel/src/syscall/sys.rs:sys_setgid": "sys_setgid",
    "kernel/src/syscall/sys.rs:sys_setfsuid": "sys_setfsuid",
    "kernel/src/syscall/sys.rs:sys_setfsgid": "sys_setfsgid",
    "kernel/src/syscall/sys.rs:sys_getgroups": "sys_getgroups",
    "kernel/src/syscall/sys.rs:sys_setgroups": "sys_setgroups",
    "kernel/src/syscall/sys.rs:sys_uname": "sys_uname",
    "kernel/src/syscall/sys.rs:sys_sethostname": "sys_sethostname",
    "kernel/src/syscall/sys.rs:sys_setdomainname": "sys_setdomainname",
    "kernel/src/syscall/sys.rs:sys_personality": "sys_personality",
    "kernel/src/syscall/sys.rs:sys_sysinfo": "sys_sysinfo",
    "kernel/src/syscall/resources.rs:sys_prlimit64": "sys_prlimit64",
    "kernel/src/syscall/resources.rs:sys_setrlimit": "sys_setrlimit",
    "kernel/src/syscall/resources.rs:sys_getrlimit": "sys_getrlimit",
    "kernel/src/syscall/resources.rs:sys_getrusage": "sys_getrusage",

    "kernel/src/syscall/net/socket.rs:sys_socket": "sys_socket",
    "kernel/src/syscall/net/socket.rs:sys_socketpair": "sys_socketpair",
    "kernel/src/syscall/net/socket.rs:sys_bind": "sys_bind",
    "kernel/src/syscall/net/socket.rs:sys_connect": "sys_connect",
    "kernel/src/syscall/net/name.rs:sys_getsockname": "sys_getsockname",
    "kernel/src/syscall/net/name.rs:sys_getpeername": "sys_getpeername",
    "kernel/src/syscall/net/socket.rs:sys_listen": "sys_listen",
    "kernel/src/syscall/net/socket.rs:sys_accept": "sys_accept",
    "kernel/src/syscall/net/socket.rs:sys_accept4": "sys_accept4",
    "kernel/src/syscall/net/socket.rs:sys_shutdown": "sys_shutdown",
    "kernel/src/syscall/net/io.rs:sys_sendto": "sys_sendto",
    "kernel/src/syscall/net/io.rs:sys_sendmsg": "sys_sendmsg",
    "kernel/src/syscall/net/io.rs:sys_sendmmsg": "sys_sendmmsg",
    "kernel/src/syscall/net/io.rs:sys_recvfrom": "sys_recvfrom",
    "kernel/src/syscall/net/io.rs:sys_recvmsg": "sys_recvmsg",
    "kernel/src/syscall/net/io.rs:sys_recvmmsg": "sys_recvmmsg",
    "kernel/src/syscall/net/opt.rs:sys_getsockopt": "sys_getsockopt",
    "kernel/src/syscall/net/opt.rs:sys_setsockopt": "sys_setsockopt",

    "kernel/src/syscall/task/exit.rs:sys_exit": "sys_exit",
    "kernel/src/syscall/task/exit.rs:sys_exit_group": "sys_exit_group",

    "kernel/src/syscall/fs/stat.rs:sys_stat": "sys_stat",
    "kernel/src/syscall/fs/stat.rs:sys_fstat": "sys_fstat",
    "kernel/src/syscall/fs/stat.rs:sys_lstat": "sys_lstat",
    "kernel/src/syscall/fs/stat.rs:sys_fstatat": "sys_fstatat",
    "kernel/src/syscall/fs/stat.rs:sys_access": "sys_access",
    "kernel/src/syscall/fs/stat.rs:sys_faccessat": "sys_faccessat",
    "kernel/src/syscall/fs/stat.rs:sys_faccessat2": "sys_faccessat2",
    "kernel/src/syscall/fs/stat.rs:sys_ustat": "sys_ustat",
    "kernel/src/syscall/fs/stat.rs:sys_statfs": "sys_statfs",
    "kernel/src/syscall/fs/stat.rs:sys_fstatfs": "sys_fstatfs",
    "kernel/src/syscall/fs/stat.rs:sys_statx": "sys_statx",

    "kernel/src/syscall/fs/xattr.rs:sys_setxattr": "sys_setxattr",
    "kernel/src/syscall/fs/xattr.rs:sys_lsetxattr": "sys_lsetxattr",
    "kernel/src/syscall/fs/xattr.rs:sys_fsetxattr": "sys_fsetxattr",
    "kernel/src/syscall/fs/xattr.rs:sys_getxattr": "sys_getxattr",
    "kernel/src/syscall/fs/xattr.rs:sys_lgetxattr": "sys_lgetxattr",
    "kernel/src/syscall/fs/xattr.rs:sys_fgetxattr": "sys_fgetxattr",
    "kernel/src/syscall/fs/xattr.rs:sys_listxattr": "sys_listxattr",
    "kernel/src/syscall/fs/xattr.rs:sys_llistxattr": "sys_llistxattr",
    "kernel/src/syscall/fs/xattr.rs:sys_flistxattr": "sys_flistxattr",
    "kernel/src/syscall/fs/xattr.rs:sys_removexattr": "sys_removexattr",
    "kernel/src/syscall/fs/xattr.rs:sys_lremovexattr": "sys_lremovexattr",
    "kernel/src/syscall/fs/xattr.rs:sys_fremovexattr": "sys_fremovexattr",

    "kernel/src/syscall/mm/mmap.rs:sys_pkey_alloc": "sys_pkey_alloc",
    "kernel/src/syscall/mm/mmap.rs:sys_mprotect": "sys_mprotect",
    "kernel/src/syscall/mm/mmap.rs:sys_munmap": "sys_munmap",
    "kernel/src/syscall/mm/mincore.rs:sys_mincore": "sys_mincore",
    "kernel/src/syscall/mm/process_vm.rs:sys_process_vm_readv": "sys_process_vm_readv",
    "kernel/src/syscall/mm/process_vm.rs:sys_process_vm_writev": "sys_process_vm_writev",
    "kernel/src/syscall/mm/mmap.rs:sys_mseal": "sys_mseal",

    "kernel/src/syscall/mm/brk.rs:sys_brk": "sys_brk",
    "kernel/src/syscall/io_mpx/epoll.rs:sys_epoll_ctl": "sys_epoll_ctl",
    "kernel/src/syscall/sync/futex.rs:sys_futex": "sys_futex",
    "kernel/src/syscall/sync/futex.rs:sys_futex_waitv": "sys_futex_waitv",
    "kernel/src/syscall/sync/futex.rs:sys_futex_wake": "sys_futex_wake",
    "kernel/src/syscall/sync/futex.rs:sys_futex_wait": "sys_futex_wait",
    "kernel/src/syscall/sync/futex.rs:sys_futex_requeue": "sys_futex_requeue",
    "kernel/src/syscall/sync/membarrier.rs:sys_membarrier": "sys_membarrier",

    "kernel/src/syscall/fs/io_uring.rs:sys_io_uring_setup": "sys_io_uring_setup",
    "kernel/src/syscall/fs/io_uring.rs:sys_io_uring_enter": "sys_io_uring_enter",
    "kernel/src/syscall/fs/io_uring.rs:sys_io_uring_register": "sys_io_uring_register",

    "kernel/src/syscall/mm/mmap.rs:sys_mmap": "sys_mmap",
    "kernel/src/syscall/mm/mmap.rs:sys_madvise": "sys_madvise",
    "kernel/src/syscall/io_mpx/select.rs:sys_select": "sys_select",
    "kernel/src/syscall/io_mpx/select.rs:sys_pselect6": "sys_pselect6",

    "kernel/src/syscall/seccomp.rs:sys_seccomp": "sys_seccomp",
    "kernel/src/syscall/fs/memfd.rs:sys_memfd_create": "sys_memfd_create",
    "kernel/src/syscall/fs/userfaultfd.rs:sys_userfaultfd": "sys_userfaultfd",
    "kernel/src/syscall/fs/secretmem.rs:sys_memfd_secret": "sys_memfd_secret",

    "kernel/src/syscall/landlock.rs:sys_landlock_create_ruleset": "sys_landlock_create_ruleset",
    "kernel/src/syscall/landlock.rs:sys_landlock_add_rule": "sys_landlock_add_rule",
    "kernel/src/syscall/landlock.rs:sys_landlock_restrict_self": "sys_landlock_restrict_self",

    "kernel/src/syscall/fs/ctl.rs:sys_ioctl": "sys_ioctl",
    "kernel/src/syscall/dispatch.rs:compat_eventfd": "compat_eventfd",
    "kernel/src/syscall/fs/event.rs:sys_eventfd2": "sys_eventfd2",
    "kernel/src/syscall/fs/fd_ops.rs:sys_creat": "sys_creat",
    "kernel/src/syscall/time.rs:sys_time": "sys_time",
    "kernel/src/syscall/task/ctl.rs:sys_umask": "sys_umask",
    "kernel/src/syscall/dispatch.rs:sys_ni_syscall": "sys_ni_syscall",
    "kernel/src/syscall/bpf/mod.rs:sys_bpf": "super::bpf::sys_bpf",
    "kernel/src/syscall/task/uprobe.rs:sys_uretprobe": "super::task::sys_uretprobe",
    "kernel/src/syscall/task/uprobe.rs:sys_uprobe": "super::task::sys_uprobe",
    "kernel/src/syscall/fs/xattr.rs:sys_setxattrat": "super::fs::sys_setxattrat",
    "kernel/src/syscall/fs/xattr.rs:sys_getxattrat": "super::fs::sys_getxattrat",
    "kernel/src/syscall/fs/xattr.rs:sys_listxattrat": "super::fs::sys_listxattrat",
    "kernel/src/syscall/fs/xattr.rs:sys_removexattrat": "super::fs::sys_removexattrat",
    "kernel/src/syscall/fs/mount.rs:sys_open_tree_attr": "super::fs::sys_open_tree_attr",
    "kernel/src/syscall/fs/fileattr.rs:sys_file_getattr": "super::fs::sys_file_getattr",
    "kernel/src/syscall/fs/fileattr.rs:sys_file_setattr": "super::fs::sys_file_setattr",
}


def load_toml(path: Path, label: str) -> dict:
    try:
        data = tomllib.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, tomllib.TOMLDecodeError) as error:
        raise GateError(f"cannot read {label} {path}: {error}") from error
    if not isinstance(data, dict):
        raise GateError(f"{label} must be a TOML table")
    return data


def repository_path(value: object, label: str, kind: str, require_exists: bool = True) -> Path:
    """Resolve an in-tree descriptor and optionally require it to exist."""
    if not isinstance(value, str) or not value:
        raise GateError(f"{label} path is invalid")
    path = (ROOT / value).resolve()
    try:
        path.relative_to(ROOT)
    except ValueError as error:
        raise GateError(f"{label} escapes the repository: {value}") from error
    if require_exists and kind == "file" and not path.is_file():
        raise GateError(f"{label} does not exist as a file: {value}")
    if require_exists and kind == "dir" and not path.is_dir():
        raise GateError(f"{label} does not exist as a directory: {value}")
    return path


def rust_function(path: Path, symbol: str, conditional: str) -> None:
    """Require a Rust function item, rather than a substring in source text."""
    if not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", symbol):
        raise GateError(f"contract handler symbol is invalid: {symbol!r}")
    raw_source = path.read_text(encoding="utf-8")
    source = mask_rust_noncode(raw_source)
    definition = re.compile(rf"(?m)^(?P<attrs>(?:\s*#\[[^\]]+\]\s*\n)*)\s*(?:pub(?:\s*\([^)]*\))?\s+)?(?:unsafe\s+)?(?:async\s+)?fn\s+{re.escape(symbol)}\s*(?:<[^{{;]*>)?\s*\(")
    if re.search(rf"\bfn\s+{re.escape(symbol)}\s*(?:<[^{{;]*>)?\s*\(", source) is None:
        raise GateError(f"contract handler is not a Rust function definition: {path.relative_to(ROOT)}:{symbol}")
    found = top_level_match(source, 0, len(source), definition, f"contract handler {path.relative_to(ROOT)}:{symbol}")
    # Array types contain semicolons (for example UserPtr<[i32; 2]>).
    # Only an item-level semicolon can turn this signature into a declaration.
    nesting = [")"]
    opening = -1
    for index in range(found.end(), len(source)):
        char = source[index]
        if char in "([":
            nesting.append({"(": ")", "[": "]"}[char])
        elif nesting and char == nesting[-1]:
            nesting.pop()
        elif not nesting and char in "{;":
            if char == "{":
                opening = index
            break
    if opening < 0:
        raise GateError(f"contract handler has no function body: {path.relative_to(ROOT)}:{symbol}")
    attrs = [re.sub(r"\s+", " ", attr.strip()) for attr in re.findall(r"#\[([^\]]+)\]", raw_source[found.start("attrs"):found.end("attrs")], re.DOTALL)]
    for cfg in (attr for attr in attrs if attr.startswith("cfg")):
        if cfg == 'cfg(target_arch = "x86_64")':
            continue  # The product and this syscall table support x86_64 only.
        feature = re.fullmatch(r'cfg\(feature\s*=\s*"([A-Za-z0-9_-]+)"\)', cfg)
        if feature is None or conditional != feature.group(1):
            raise GateError(f"contract handler cfg does not match cell conditional: {path.relative_to(ROOT)}:{symbol}")


def handler_route(value: object, conditional: str) -> str:
    if not isinstance(value, str):
        raise GateError("contract handler is invalid")
    relative, separator, symbol = value.rpartition(":")
    if not separator or not relative or not symbol:
        raise GateError("contract handler must be path:symbol")
    path = repository_path(relative, "contract handler", "file")
    if path.suffix != ".rs":
        raise GateError("contract handler must name Rust source")
    rust_function(path, symbol, conditional)
    return value


GRAPH_PREFIX = {"flags": "flag:", "structs": "struct:", "multiplexer_commands": "mux:", "provider_ioctls": "ioctl:", "errno_order": "errno:", "usercopy": "usercopy:", "state": "state:", "concurrency": "concurrency:", "teardown": "teardown:"}


def graph_field(value: object, contract: str, field: str) -> list[str]:
    if not isinstance(value, list) or not value or not all(isinstance(item, str) and item for item in value):
        raise GateError(f"contract {contract}.{field} must be a non-empty typed list")
    banned = re.compile(r"\b(?:contract-defined|Linux syscall-specific|todo|tbd|unknown|generic|handler-defined)\b", re.IGNORECASE)
    if any(banned.search(item) for item in value):
        raise GateError(f"contract {contract}.{field} uses a generic placeholder")
    if "explicit-none" in value and value != ["explicit-none"]:
        raise GateError(f"contract {contract}.{field} mixes explicit-none with content")
    if value != ["explicit-none"]:
        prefix = GRAPH_PREFIX[field]
        if any(not item.startswith(prefix) or not item[len(prefix):].strip() for item in value):
            raise GateError(f"contract {contract}.{field} has no non-empty typed grammar")
    return value


def contract_cells(contracts_path: Path, entries: dict[int, str], dispatch: Path | None = None) -> dict[int, dict]:
    data = load_toml(contracts_path, "contracts")
    if set(data) != {"schema", "linux_manifest", "progress", "contract", "cell"} or data["schema"] != 3:
        raise GateError("contracts schema is invalid")
    if data["linux_manifest"] != "linux-abi.toml":
        raise GateError("contracts must reference the pinned Linux manifest")
    definitions: dict[str, dict] = {}
    for item in data["contract"]:
        if not isinstance(item, dict) or set(item) != CONTRACT_FIELDS or not isinstance(item.get("id"), str) or item["id"] in definitions:
            raise GateError("contract definition is invalid or duplicate")
        for field in CONTRACT_FIELDS - {"id"}:
            graph_field(item[field], item["id"], field)
        definitions[item["id"]] = item
    cells: dict[int, dict] = {}
    used_implemented: set[str] = set()
    for item in data["cell"]:
        if not isinstance(item, dict) or set(item) != CELL_FIELDS:
            raise GateError("contract cell must bind exactly number, name, status, contract, and handler")
        number, name = item["number"], item["name"]
        if not isinstance(number, int) or number not in entries or item["name"] != entries[number] or number in cells:
            raise GateError("contract cell has duplicate number or does not match pinned syscall name")
        if item["status"] not in {"implemented", "explicit-enosys", "partial"} or item["contract"] not in definitions:
            raise GateError("contract cell has unknown status or contract")
        if item["status"] == "implemented" and (item["contract"] in used_implemented or item["contract"] != f"linux-{name}"):
            raise GateError("implemented cells require a name-bound, non-reused contract")
        used_implemented.add(item["contract"])
        if item["conditional"] != "explicit-none" and (not isinstance(item["conditional"], str) or not item["conditional"]):
            raise GateError("contract cell conditional is invalid")
        for field in ("validation_gaps", "limitations"):
            value = item[field]
            if not isinstance(value, list) or not value or not all(isinstance(entry, str) and entry.strip() for entry in value):
                raise GateError(f"contract cell {name}.{field} must describe its scope or explicit-none")
            if "explicit-none" in value and value != ["explicit-none"]:
                raise GateError(f"contract cell {name}.{field} mixes explicit-none with content")
        if item["status"] == "implemented" and item["limitations"] != ["explicit-none"]:
            raise GateError(f"implemented contract cell {name} cannot have implementation limitations")
        if item["status"] == "partial" and item["limitations"] == ["explicit-none"]:
            raise GateError(f"partial contract cell {name} needs a concrete implementation limitation")
        tests = item["tests"]
        if not isinstance(tests, list) or not all(isinstance(test, str) for test in tests) or len(tests) != len(set(tests)):
            raise GateError(f"contract cell {name} tests are invalid")
        for test in tests:
            relative, separator, symbol = test.rpartition(":")
            if not separator or not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", symbol):
                raise GateError(f"contract test must name path:symbol: {test}")
            path = repository_path(relative, "contract test", "file")
            text = path.read_text(encoding="utf-8")
            masked = mask_rust_noncode(text)
            definition = rf"\bfn\s+{re.escape(symbol)}\s*\(|\b(?:int|void|bool)\s+{re.escape(symbol)}\s*\([^;{{}}]*\)\s*{{"
            if not re.search(definition, masked):
                raise GateError(f"contract test symbol does not exist: {test}")
            if path.suffix == ".rs":
                attributed = rf"(?m)^\s*#\[test\]\s*(?:#\[[^\]]+\]\s*)*fn\s+{re.escape(symbol)}\s*\("
                if not re.search(attributed, masked):
                    raise GateError(f"contract Rust test must have #[test]: {test}")
            elif path.suffix != ".c":
                raise GateError(f"contract test source language is not supported: {test}")
        if not tests and item["validation_gaps"] == ["explicit-none"]:
            raise GateError(f"contract cell {name} without tests must report a validation gap")
        if item["status"] == "implemented" and item["validation_gaps"] == ["explicit-none"]:
            from tools.qemu_runner.abi_differential import CONTRACTS as runtime_cases, PROGRAMS, SYSCALL_CASES
            program, case = SYSCALL_CASES.get(number, (None, None))
            expected_test = f"tests/guest/portable/{program}-differential.c:main"
            if program not in PROGRAMS or case not in runtime_cases or expected_test not in tests:
                raise GateError(f"implemented contract cell {name} claims validation without a registered differential case")
        cells[number] = {**item, **{field: definitions[item["contract"]][field] for field in CONTRACT_FIELDS - {"id"}}}
        cells[number]["handler"] = handler_route(item["handler"], item["conditional"])
    counts = Counter(cell["status"] for cell in cells.values())
    progress = data["progress"]
    actual = {
        "reviewed": len(cells),
        "resolved": counts["implemented"] + counts["explicit-enosys"],
        "implemented": counts["implemented"],
        "explicit_enosys": counts["explicit-enosys"],
        "fallback": 0,
        "partial": counts["partial"],
        "unknown": len(entries) - len(cells),
    }
    if progress != actual:
        raise GateError(f"contract progress is invalid: {progress}, expected {actual}")
    if dispatch is not None:
        _, ni, parsed = routes(dispatch, set(entries.values()), WITNESS)
        bindings: dict[str, tuple[str, list[str]]] = {}
        for pattern, expression in parsed:
            masked_pattern = mask_rust_noncode(pattern)
            # As in routes(), attributes carry string literals and must be read
            # from the raw pattern rather than the masked form.
            attrs = [re.sub(r"\s+", " ", item.strip()) for item in re.findall(r"#\[([^]]+)\]", pattern, re.DOTALL)]
            for name in SYSNO.findall(masked_pattern):
                bindings[name] = (mask_rust_noncode(expression), attrs)
        for cell in cells.values():
            expected_call = DISPATCH_CALLS.get(cell["handler"])
            if expected_call is None:
                raise GateError(f"contract handler has no approved dispatch call binding: {cell['handler']}")
            if cell["status"] == "explicit-enosys":
                if (cell["handler"] != "kernel/src/syscall/dispatch.rs:sys_ni_syscall"
                        or cell["name"] not in ni
                        or bindings.get(cell["name"], ("", []))[0] != "sys_ni_syscall()"):
                    raise GateError(f"explicit ENOSYS cell is not bound to its actual NI arm: {cell['number']}:{cell['name']}")
                continue
            binding = bindings.get(cell["name"])
            call = re.compile(rf"(?<![A-Za-z0-9_:]){re.escape(expected_call)}\s*\(")
            if binding is None or call.search(binding[0]) is None:
                raise GateError(f"non-NI cell is not bound to its actual dispatch handler: {cell['number']}:{cell['name']}")
            expected_cfg = [] if cell["conditional"] == "explicit-none" else [f'cfg(feature = "{cell["conditional"]}")']
            # x86_64 is the sole product architecture, not an optional feature.
            actual_cfg = [cfg for cfg in binding[1] if cfg != 'cfg(target_arch = "x86_64")']
            if actual_cfg != expected_cfg:
                raise GateError(f"cell conditional does not match its dispatch profile: {cell['number']}:{cell['name']}")
    return cells


def schema(manifest_path: Path, contracts_path: Path, source: Path, dispatch: Path = DISPATCH, *, final: bool = False) -> None:
    manifest = load_manifest(manifest_path)
    entries = parse_table(source / manifest["linux"]["table"])
    cells = contract_cells(contracts_path, entries, dispatch)
    routing = states(manifest, entries)
    ni_mismatch = sorted(number for number, cell in cells.items()
                         if (cell["status"] == "explicit-enosys") != (routing[number] == "explicit-enosys"))
    if ni_mismatch:
        raise GateError(f"contract explicit ENOSYS set disagrees with routing inventory: {ni_mismatch}")
    counts = Counter(cell["status"] for cell in cells.values())
    print(f"linux-abi schema: reviewed={len(cells)} implemented={counts['implemented']} explicit-enosys={counts['explicit-enosys']} partial={counts['partial']} unknown={len(entries) - len(cells)}")
    unvalidated = [cell["name"] for cell in cells.values()
                   if cell["status"] == "implemented" and (not cell["tests"] or cell["validation_gaps"] != ["explicit-none"])]
    print(f"linux-abi static declarations: implemented-with-gaps={len(unvalidated)}; runtime execution is not established by this gate")
    if final and (len(cells) != len(entries) or unvalidated):
        raise GateError(f"final ABI static prerequisites incomplete: unknown={len(entries) - len(cells)}, implemented-with-gaps={unvalidated}")
    if final:
        print("linux-abi final static prerequisites satisfied; current-run guest results are still required")


def run_git(directory: Path, *args: str) -> str:
    result = subprocess.run(["git", "-C", str(directory), *args], text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False)
    if result.returncode: raise GateError(result.stderr.strip() or result.stdout.strip())
    return result.stdout.strip()


def materialize(manifest_path: Path, destination: Path) -> None:
    manifest = load_manifest(manifest_path); linux = manifest["linux"]; destination = destination.resolve()
    if str(destination).startswith(("/tmp/", "/dev/shm/")): raise GateError("Linux source may not be materialized on tmpfs")
    if not destination.exists():
        destination.parent.mkdir(parents=True, exist_ok=True)
        subprocess.run(["git", "clone", "--depth=1", "--filter=blob:none", "--no-checkout", "--branch", linux["tag"], linux["repository"], str(destination)], check=True)
        subprocess.run(["git", "-C", str(destination), "sparse-checkout", "init", "--no-cone"], check=True)
        subprocess.run(["git", "-C", str(destination), "sparse-checkout", "set", "--no-cone", linux["table"]], check=True)
        subprocess.run(["git", "-C", str(destination), "checkout", "--detach", linux["tag"]], check=True)
    if not (destination / ".git").is_dir() or run_git(destination, "remote", "get-url", "origin") != linux["repository"]: raise GateError("materialized Linux source origin is invalid")
    if run_git(destination, "rev-parse", "HEAD^{commit}") != run_git(destination, "rev-parse", f"{linux['tag']}^{{commit}}") or run_git(destination, "status", "--porcelain", "--untracked-files=all"):
        raise GateError("materialized Linux source is not the clean selected release")
    if not (destination / linux["table"]).is_file(): raise GateError("materialized Linux source does not contain syscall table")


def main(argv: Sequence[str] | None = None) -> int:
    arguments = list(sys.argv[1:] if argv is None else argv)
    parser = argparse.ArgumentParser(description=__doc__); parser.add_argument("command", choices=("materialize", "inventory", "schema", "all")); parser.add_argument("--manifest", type=Path, default=MANIFEST); parser.add_argument("--contracts", type=Path, default=CONTRACTS); parser.add_argument("--linux-src", type=Path, default=SOURCE); parser.add_argument("--dispatch", type=Path, default=DISPATCH)
    parser.add_argument("--final", action="store_true", help="require final static prerequisites; does not establish guest runtime acceptance")
    args = parser.parse_args(arguments)
    if args.final and args.command not in {"schema", "all"}:
        parser.error("--final requires schema or all")
    try:
        if args.command in {"materialize", "all"}: materialize(args.manifest, args.linux_src)
        if args.command in {"inventory", "all"}: inventory(args.manifest, args.linux_src, args.dispatch)
        if args.command in {"schema", "all"}: schema(args.manifest, args.contracts, args.linux_src, args.dispatch, final=args.final)
    except (GateError, OSError, subprocess.CalledProcessError) as error:
        print(f"linux-abi: {error}", file=sys.stderr); return 1
    return 0


if __name__ == "__main__": raise SystemExit(main())
