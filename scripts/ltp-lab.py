#!/usr/bin/env python3
"""Local LTP experiment harness for the OSComp evaluator flow."""

from __future__ import annotations

import argparse
import concurrent.futures
import datetime as _dt
import fnmatch
import json
import os
import random
import re
import signal
import shutil
import subprocess
import sys
import threading
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Iterable


REPO_ROOT = Path(__file__).resolve().parents[1]
STATE_DIR = REPO_ROOT / ".state" / "ltp-lab"
BASELINE_DIR = REPO_ROOT / ".state" / "baseline"
LIST_DIR = STATE_DIR / "lists"
PLAN_DIR = STATE_DIR / "plans"
RUN_DIR = STATE_DIR / "runs"
IMAGE_CACHE_DIR = STATE_DIR / "images"
REF_DIR = STATE_DIR / "refs"
CAMPAIGN_DIR = STATE_DIR / "campaigns"
INVENTORY_PATH = STATE_DIR / "inventory.json"
DEFAULT_TEST_LIST = REPO_ROOT / "ltp_test.txt"
DEFAULT_PLAN_NAME = "ltp-both"
DEFAULT_TESTSUITE_SOURCE = Path.home() / "testsuits-for-oskernel"
DEFAULT_TESTSUITE_REF = REF_DIR / "testsuits-for-oskernel"
DEFAULT_LINUX_REF = REF_DIR / "linux"
DEFAULT_CAMPAIGN_LIMIT = 120
DEFAULT_CAMPAIGN_CASE_TIMEOUT = 90
DEFAULT_IMAGE_ROOTS = [
    os.environ.get("OSCOMP_TESTSUITE_DIR", ""),
    "/home/dia/kernel-image",
    str(Path.home() / "kernel-image"),
    str(Path.home() / "testsuits-for-oskernel"),
    "/coursegrader/testdata",
]
ARCHES = ("rv", "la")
LIBCS = ("glibc", "musl")
RESULT_KINDS = ("TPASS", "TFAIL", "TBROK", "TCONF", "TWARN")
BUILD_STATE_DIRS = (REPO_ROOT / ".state" / "riscv64", REPO_ROOT / ".state" / "loongarch64")
RUNTEST_PRESETS = {
    "fs": ["fs", "syscalls", "dio", "fcntl-locktests"],
    "vfs": ["fs", "syscalls", "dio", "fcntl-locktests", "fs_perms_simple", "fs_readonly"],
    "file": ["fs", "syscalls", "dio", "fcntl-locktests"],
    "proc": ["syscalls", "sched", "nptl"],
    "process": ["syscalls", "sched", "nptl"],
    "signal": ["syscalls"],
    "time": ["syscalls", "sched"],
    "futex": ["syscalls", "sched", "nptl"],
    "sched": ["sched", "syscalls"],
    "mm": ["mm", "syscalls", "numa"],
    "ipc": ["ipc", "syscalls-ipc", "syscalls"],
    "tty": ["pty", "syscalls"],
    "pty": ["pty", "syscalls"],
    "net": [
        "net.features",
        "net.ipv6",
        "net.multicast",
        "net.tcp_cmds",
        "net_stress.appl",
        "net_stress.interface",
    ],
    "all": [],
}


@dataclass
class ReplayTask:
    task_id: str
    arch: str
    libcs: list[str]
    plan_path: Path
    support_image: Path
    task_dir: Path
    workdir: Path
    console_log: Path
    timeout: int
    env_path: Path | None
    command: list[str]


@dataclass
class ReplayResult:
    task_id: str
    arch: str
    libcs: list[str]
    exit_code: int | None
    console_log: Path
    cases_jsonl: Path
    summary_json: Path
    started_at: str | None
    ended_at: str | None
    duration_secs: float
    status: str
    killed_by_fail_fast: bool = False
    error: str = ""


def die(message: str) -> None:
    print(f"[ltp-lab] error: {message}", file=sys.stderr)
    raise SystemExit(1)


def log(message: str) -> None:
    print(f"[ltp-lab] {message}")


def ensure_dirs() -> None:
    for path in (STATE_DIR, LIST_DIR, PLAN_DIR, RUN_DIR, IMAGE_CACHE_DIR, REF_DIR, CAMPAIGN_DIR):
        path.mkdir(parents=True, exist_ok=True)


def run_cmd(
    cmd: list[str],
    *,
    cwd: Path = REPO_ROOT,
    env: dict[str, str] | None = None,
    capture: bool = True,
    check: bool = True,
    log_path: Path | None = None,
) -> subprocess.CompletedProcess[str]:
    merged_env = os.environ.copy()
    if env:
        merged_env.update(env)
    if log_path:
        with log_path.open("w", encoding="utf-8", errors="replace") as out:
            proc = subprocess.run(
                cmd,
                cwd=cwd,
                env=merged_env,
                text=True,
                stdout=out,
                stderr=subprocess.STDOUT,
                check=False,
            )
        if check and proc.returncode != 0:
            die(f"command failed ({proc.returncode}): {' '.join(cmd)}; see {log_path}")
        return proc
    proc = subprocess.run(
        cmd,
        cwd=cwd,
        env=merged_env,
        text=True,
        stdout=subprocess.PIPE if capture else None,
        stderr=subprocess.PIPE if capture else None,
        check=False,
    )
    if check and proc.returncode != 0:
        detail = ""
        if capture:
            detail = (proc.stderr or proc.stdout or "").strip()
        die(f"command failed ({proc.returncode}): {' '.join(cmd)}{': ' + detail if detail else ''}")
    return proc


def require_tool(name: str) -> bool:
    return shutil.which(name) is not None


def compiler_libgcc_available(*compilers: str) -> bool:
    for compiler in compilers:
        if not require_tool(compiler):
            continue
        proc = run_cmd([compiler, "-print-file-name=libgcc_s.so.1"], capture=True, check=False)
        path = (proc.stdout or "").strip()
        if path and path != "libgcc_s.so.1" and Path(path).is_file():
            return True
    return False


def canonical_arch(value: str) -> str:
    if value in ("rv", "riscv64"):
        return "rv"
    if value in ("la", "loongarch64"):
        return "la"
    die(f"unsupported arch: {value}")
    return value


def canonical_arches(values: list[str] | None) -> list[str]:
    if not values or values == ["both"]:
        return list(ARCHES)
    result: list[str] = []
    for value in values:
        for item in value.split(","):
            item = item.strip()
            if not item:
                continue
            arch = canonical_arch(item)
            if arch not in result:
                result.append(arch)
    return result


def canonical_libcs(values: list[str] | None) -> list[str]:
    if not values or values == ["both"]:
        return list(LIBCS)
    result: list[str] = []
    for value in values:
        for item in value.split(","):
            item = item.strip()
            if item not in LIBCS:
                die(f"unsupported libc: {item}")
            if item not in result:
                result.append(item)
    return result


def now_id() -> str:
    return _dt.datetime.now().strftime("%Y%m%d-%H%M%S")


def iso_now() -> str:
    return _dt.datetime.now().isoformat(timespec="seconds")


def write_json(path: Path, data: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(data, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def read_json(path: Path) -> Any:
    return json.loads(path.read_text(encoding="utf-8"))


def path_size(path: Path) -> int:
    if not path.exists():
        return 0
    if path.is_file():
        return path.stat().st_size
    total = 0
    for item in path.rglob("*"):
        if item.is_file():
            try:
                total += item.stat().st_size
            except OSError:
                pass
    return total


def format_bytes(size: int) -> str:
    units = ("B", "KiB", "MiB", "GiB")
    value = float(size)
    for unit in units:
        if value < 1024 or unit == units[-1]:
            if unit == "B":
                return f"{int(value)}{unit}"
            return f"{value:.1f}{unit}"
        value /= 1024
    return f"{size}B"


def newest_mtime(path: Path) -> float:
    try:
        newest = path.stat().st_mtime
    except OSError:
        return 0.0
    if path.is_dir():
        for item in path.rglob("*"):
            try:
                newest = max(newest, item.stat().st_mtime)
            except OSError:
                pass
    return newest


def parse_duration(value: str) -> int:
    match = re.fullmatch(r"(\d+)([smhdw]?)", value.strip())
    if not match:
        die(f"invalid duration: {value}; use seconds or suffix s/m/h/d/w")
    amount = int(match.group(1))
    unit = match.group(2) or "s"
    scale = {"s": 1, "m": 60, "h": 3600, "d": 86400, "w": 604800}[unit]
    return amount * scale


def split_csv(values: list[str] | None) -> list[str]:
    if not values:
        return []
    result: list[str] = []
    for value in values:
        for item in value.split(","):
            item = item.strip()
            if item:
                result.append(item)
    return result


def split_words_or_csv(values: list[str] | None) -> list[str]:
    if not values:
        return []
    result: list[str] = []
    for value in values:
        for chunk in value.split(","):
            for item in chunk.split():
                item = item.strip()
                if item:
                    result.append(item)
    return result


def find_official_image(arch: str, roots: list[str] | None = None) -> Path | None:
    base = "sdcard-rv.img" if arch == "rv" else "sdcard-la.img"
    candidates = []
    for root in roots or DEFAULT_IMAGE_ROOTS:
        if not root:
            continue
        root_path = Path(root).expanduser()
        candidates.extend(
            [
                root_path / base,
                root_path / f"{base}.xz",
                root_path / f"{base}.gz",
            ]
        )
    for path in candidates:
        if path.is_file():
            return path
    return None


def plain_image_for(source: Path, *, refresh: bool = False) -> Path:
    ensure_dirs()
    if source.suffix not in (".xz", ".gz"):
        return source
    target_name = source.name
    if target_name.endswith(".xz"):
        target_name = target_name[:-3]
    elif target_name.endswith(".gz"):
        target_name = target_name[:-3]
    target = IMAGE_CACHE_DIR / target_name
    if (
        target.is_file()
        and not refresh
        and target.stat().st_size > 0
        and target.stat().st_mtime >= source.stat().st_mtime
    ):
        return target
    log(f"decompressing {source} -> {target}")
    target.parent.mkdir(parents=True, exist_ok=True)
    tmp = target.with_suffix(target.suffix + ".tmp")
    if source.suffix == ".xz":
        with tmp.open("wb") as out:
            proc = subprocess.run(["xz", "-dc", str(source)], stdout=out, check=False)
    else:
        with tmp.open("wb") as out:
            proc = subprocess.run(["gzip", "-dc", str(source)], stdout=out, check=False)
    if proc.returncode != 0:
        tmp.unlink(missing_ok=True)
        die(f"failed to decompress {source}")
    tmp.replace(target)
    return target


@dataclass
class DebugfsEntry:
    inode: str
    mode: str
    uid: str
    gid: str
    name: str
    size: int


def debugfs_ls(image: Path, path: str) -> list[DebugfsEntry]:
    proc = run_cmd(["debugfs", "-R", f"ls -p {path}", str(image)], capture=True)
    entries: list[DebugfsEntry] = []
    for line in proc.stdout.splitlines():
        if not line.startswith("/"):
            continue
        parts = line.split("/")
        if len(parts) < 7:
            continue
        inode, mode, uid, gid, name, size_text = parts[1:7]
        if name in (".", ".."):
            continue
        if not inode.isdigit():
            continue
        try:
            size = int(size_text or "0")
        except ValueError:
            size = 0
        entries.append(DebugfsEntry(inode, mode, uid, gid, name, size))
    return entries


def debugfs_cat(image: Path, path: str) -> str:
    proc = run_cmd(["debugfs", "-R", f"cat {path}", str(image)], capture=True)
    lines = [line for line in proc.stdout.splitlines() if not line.startswith("debugfs ")]
    return "\n".join(lines) + ("\n" if lines else "")


def executable_names(entries: Iterable[DebugfsEntry]) -> list[str]:
    return sorted({entry.name for entry in entries if entry.mode.startswith("100")})


def parse_test_list(path: Path) -> list[dict[str, Any]]:
    items: list[dict[str, Any]] = []
    if not path.is_file():
        return items
    for line_no, raw in enumerate(path.read_text(encoding="utf-8", errors="replace").splitlines(), 1):
        stripped = raw.strip()
        if not stripped or stripped.startswith("#"):
            continue
        tokens = stripped.split()
        items.append(
            {
                "line_no": line_no,
                "line": raw,
                "marker": tokens[0],
                "tokens": tokens,
            }
        )
    return items


def inventory_repo_path(inv: dict[str, Any], value: str | None, default: Path) -> Path:
    if not value:
        return default
    path = Path(value).expanduser()
    inv_root_text = inv.get("repo_root")
    if path.is_absolute() and inv_root_text:
        inv_root = Path(inv_root_text).expanduser()
        try:
            rel = path.relative_to(inv_root)
        except ValueError:
            pass
        else:
            return REPO_ROOT / rel
    return path


def parse_runtest_dir(path: Path) -> list[dict[str, Any]]:
    entries: list[dict[str, Any]] = []
    if not path.is_dir():
        return entries
    for file_path in sorted(path.iterdir()):
        if not file_path.is_file() or file_path.name == "Makefile":
            continue
        for line_no, raw in enumerate(file_path.read_text(encoding="utf-8", errors="replace").splitlines(), 1):
            stripped = raw.strip()
            if not stripped or stripped.startswith("#"):
                continue
            tokens = stripped.split()
            entries.append(
                {
                    "runtest": file_path.name,
                    "line_no": line_no,
                    "line": stripped,
                    "marker": tokens[0],
                    "exec": tokens[1] if len(tokens) > 1 else tokens[0],
                    "tokens": tokens,
                }
            )
    return entries


def candidate_testsuite_sources(source_root: Path | None) -> list[Path]:
    roots: list[Path] = []
    for candidate in (
        source_root,
        DEFAULT_TESTSUITE_SOURCE,
        DEFAULT_TESTSUITE_REF,
        Path(os.environ.get("OSCOMP_TESTSUITE_DIR", "")) if os.environ.get("OSCOMP_TESTSUITE_DIR") else None,
    ):
        if candidate is None:
            continue
        resolved = candidate.expanduser()
        if resolved not in roots:
            roots.append(resolved)
    return roots


def source_runtest_dir(source_root: Path | None) -> Path | None:
    for root in candidate_testsuite_sources(source_root):
        candidates = [
            root / "ltp-full-20240524" / "runtest",
            root / "ltp" / "runtest",
            root / "runtest",
        ]
        for candidate in candidates:
            if candidate.is_dir():
                return candidate
    return None


def resolve_current_items(
    current: list[dict[str, Any]],
    names_by_combo: dict[str, dict[str, set[str]]],
) -> dict[str, Any]:
    combos: dict[str, Any] = {}
    for arch in ARCHES:
        combos[arch] = {}
        for libc in LIBCS:
            available = names_by_combo.get(arch, {}).get(libc, set())
            resolved = 0
            unresolved: list[dict[str, str]] = []
            execs: set[str] = set()
            alias_lines = 0
            for item in current:
                tokens = item["tokens"]
                marker = tokens[0]
                exec_name = marker
                if len(tokens) > 1 and tokens[1] in available:
                    exec_name = tokens[1]
                if exec_name != marker:
                    alias_lines += 1
                if exec_name in available:
                    resolved += 1
                    execs.add(exec_name)
                else:
                    unresolved.append({"marker": marker, "exec": exec_name, "line": item["line"]})
            combos[arch][libc] = {
                "entries": len(current),
                "resolved": resolved,
                "unresolved": len(unresolved),
                "unique_execs": len(execs),
                "alias_lines": alias_lines,
                "unresolved_examples": unresolved[:50],
            }
    return combos


def build_inventory(args: argparse.Namespace) -> dict[str, Any]:
    ensure_dirs()
    if not require_tool("debugfs"):
        die("debugfs is required")
    if not require_tool("xz"):
        die("xz is required for compressed official images")
    image_roots = split_csv(args.image_root) or DEFAULT_IMAGE_ROOTS
    source_root = Path(args.testsuite_source).expanduser() if args.testsuite_source else None
    test_list_path = Path(args.current_list).expanduser() if args.current_list else DEFAULT_TEST_LIST

    inventory: dict[str, Any] = {
        "generated_at": _dt.datetime.now().isoformat(timespec="seconds"),
        "repo_root": str(REPO_ROOT),
        "image_roots": [root for root in image_roots if root],
        "testsuite_source_candidates": [str(path) for path in candidate_testsuite_sources(source_root)],
        "current_list": str(test_list_path),
        "images": {},
        "combos": {},
        "source_runtest": {},
        "current": {},
    }

    names_by_combo: dict[str, dict[str, set[str]]] = {arch: {} for arch in ARCHES}
    for arch in ARCHES:
        source = find_official_image(arch, image_roots)
        if not source:
            die(f"official image not found for {arch}")
        image = plain_image_for(source, refresh=args.refresh_images)
        inventory["images"][arch] = {
            "source": str(source),
            "plain": str(image),
            "source_size": source.stat().st_size,
            "plain_size": image.stat().st_size,
        }
        inventory["combos"][arch] = {}
        for libc in LIBCS:
            bin_entries = debugfs_ls(image, f"/{libc}/ltp/testcases/bin")
            script_entries = debugfs_ls(image, f"/{libc}/ltp/testscripts")
            runtest_entries = debugfs_ls(image, f"/{libc}/ltp/runtest")
            bins = executable_names(bin_entries)
            scripts = executable_names(script_entries)
            names_by_combo[arch][libc] = set(bins) | set(scripts)
            inventory["combos"][arch][libc] = {
                "ltp_bin_files": len(bins),
                "ltp_testscripts": len(scripts),
                "ltp_runtest_files": len(executable_names(runtest_entries)),
                "names": {
                    "bin": bins,
                    "testscripts": scripts,
                },
            }

    runtest_path = source_runtest_dir(source_root)
    runtest_entries = parse_runtest_dir(runtest_path) if runtest_path else []
    by_runtest: dict[str, int] = {}
    for item in runtest_entries:
        by_runtest[item["runtest"]] = by_runtest.get(item["runtest"], 0) + 1
    inventory["source_runtest"] = {
        "path": str(runtest_path) if runtest_path else "",
        "files": len(by_runtest),
        "entries": len(runtest_entries),
        "unique_markers": len({item["marker"] for item in runtest_entries}),
        "unique_execs": len({item["exec"] for item in runtest_entries}),
        "by_file": dict(sorted(by_runtest.items())),
        "entries_data": runtest_entries,
    }

    current = parse_test_list(test_list_path)
    inventory["current"] = {
        "entries": len(current),
        "unique_markers": len({item["marker"] for item in current}),
        "lines_with_args": sum(1 for item in current if len(item["tokens"]) > 1),
        "items": current,
        "resolution": resolve_current_items(current, names_by_combo),
    }
    out_path = Path(args.output).expanduser() if args.output else INVENTORY_PATH
    write_json(out_path, inventory)
    return inventory


def load_inventory(path: str | None = None) -> dict[str, Any]:
    inv_path = Path(path).expanduser() if path else INVENTORY_PATH
    if not inv_path.is_file():
        die(f"inventory not found: {inv_path}; run scripts/ltp-lab.py inventory first")
    return read_json(inv_path)


def print_inventory_summary(inv: dict[str, Any]) -> None:
    print(f"inventory: {inv.get('generated_at', '')}")
    print("official images:")
    for arch in ARCHES:
        image = inv["images"].get(arch, {})
        print(f"  {arch}: {image.get('source', 'missing')}")
    print("LTP packaged files:")
    for arch in ARCHES:
        for libc in LIBCS:
            combo = inv["combos"][arch][libc]
            print(
                f"  {arch}/{libc}: bin={combo['ltp_bin_files']} "
                f"testscripts={combo['ltp_testscripts']} runtest_files={combo['ltp_runtest_files']}"
            )
    source = inv["source_runtest"]
    print(
        "source runtest: "
        f"files={source.get('files', 0)} entries={source.get('entries', 0)} "
        f"unique_execs={source.get('unique_execs', 0)} path={source.get('path', '')}"
    )
    current = inv["current"]
    print(
        "current list: "
        f"entries={current['entries']} unique_markers={current['unique_markers']} "
        f"lines_with_args={current['lines_with_args']}"
    )
    for arch in ARCHES:
        for libc in LIBCS:
            res = current["resolution"][arch][libc]
            print(
                f"  resolve {arch}/{libc}: {res['resolved']}/{res['entries']} "
                f"unique_execs={res['unique_execs']} alias_lines={res['alias_lines']} "
                f"unresolved={res['unresolved']}"
            )


def selected_available_names(inv: dict[str, Any], arches: list[str], libcs: list[str]) -> set[str]:
    selected: list[set[str]] = []
    for arch in arches:
        for libc in libcs:
            names = inv["combos"][arch][libc]["names"]
            selected.append(set(names["bin"]) | set(names["testscripts"]))
    if not selected:
        return set()
    result = set(selected[0])
    for names in selected[1:]:
        result &= names
    return result


def filter_lines(lines: list[str], args: argparse.Namespace) -> list[str]:
    includes = split_csv(args.include)
    excludes = split_csv(args.exclude)
    if includes:
        lines = [line for line in lines if any(fnmatch.fnmatch(line.split()[0], pat) for pat in includes)]
    if excludes:
        lines = [line for line in lines if not any(fnmatch.fnmatch(line.split()[0], pat) for pat in excludes)]
    if args.shuffle:
        rng = random.Random(args.seed)
        rng.shuffle(lines)
    offset = args.offset or 0
    if offset:
        lines = lines[offset:]
    if args.limit is not None:
        lines = lines[: args.limit]
    return lines


def generation_mode_for(args: argparse.Namespace) -> str:
    if getattr(args, "mode", None) == "unopened-runtest" and split_csv(getattr(args, "case", None)):
        return "cases"
    return args.mode


def current_case_line_by_marker() -> dict[str, str]:
    return {item["marker"]: item["line"].strip() for item in parse_test_list(DEFAULT_TEST_LIST)}


def resolve_explicit_case_lines(cases: list[str]) -> list[str]:
    line_by_marker = current_case_line_by_marker()
    lines: list[str] = []
    for case in cases:
        stripped = case.strip()
        if not stripped:
            continue
        if len(stripped.split()) == 1:
            lines.append(line_by_marker.get(stripped, stripped))
        else:
            lines.append(stripped)
    return lines


def expanded_runtests(values: list[str] | None) -> list[str]:
    result: list[str] = []
    for value in split_words_or_csv(values):
        expanded = RUNTEST_PRESETS.get(value, [value])
        for item in expanded:
            if item and item not in result:
                result.append(item)
    return result


def generate_list(args: argparse.Namespace) -> Path:
    ensure_dirs()
    arches = canonical_arches(args.arch)
    libcs = canonical_libcs(args.libc)
    mode = generation_mode_for(args)
    lines: list[str]

    if mode == "cases":
        raw_cases = split_csv(args.case)
        if not raw_cases:
            die("generate --mode cases requires --case")
        lines = resolve_explicit_case_lines(raw_cases)
    elif mode == "current":
        inv = load_inventory(args.inventory)
        current_list_path = inventory_repo_path(inv, inv.get("current_list"), DEFAULT_TEST_LIST)
        current_items = parse_test_list(current_list_path)
        if not current_items:
            current_items = inv["current"]["items"]
        lines = [item["line"].strip() for item in current_items]
    elif mode == "all-bins":
        inv = load_inventory(args.inventory)
        available = selected_available_names(inv, arches, libcs)
        lines = sorted(available)
    elif mode in ("runtest", "unopened-runtest"):
        inv = load_inventory(args.inventory)
        current_list_path = inventory_repo_path(inv, inv.get("current_list"), DEFAULT_TEST_LIST)
        current_items = parse_test_list(current_list_path)
        if not current_items:
            current_items = inv["current"]["items"]
        current_markers = {item["marker"] for item in current_items}
        available = selected_available_names(inv, arches, libcs)
        entries = inv["source_runtest"].get("entries_data", [])
        runtest_filters = set(expanded_runtests(args.runtest))
        selected: list[str] = []
        seen: set[str] = set()
        for item in entries:
            if runtest_filters and item["runtest"] not in runtest_filters:
                continue
            marker = item["marker"]
            exec_name = item["exec"]
            if mode == "unopened-runtest" and marker in current_markers:
                continue
            if exec_name not in available:
                continue
            line = item["line"]
            if line in seen:
                continue
            seen.add(line)
            selected.append(line)
        lines = selected
    else:
        die(f"unsupported generate mode: {mode}")
        lines = []

    lines = filter_lines([line for line in lines if line.strip()], args)
    name = args.name or f"{mode}-{now_id()}"
    output = Path(args.output).expanduser() if args.output else LIST_DIR / f"{name}.txt"
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text("\n".join(lines) + ("\n" if lines else ""), encoding="utf-8")
    meta = {
        "generated_at": _dt.datetime.now().isoformat(timespec="seconds"),
        "mode": mode,
        "arches": arches,
        "libcs": libcs,
        "count": len(lines),
        "output": str(output),
    }
    write_json(output.with_suffix(output.suffix + ".json"), meta)
    log(f"wrote {len(lines)} LTP lines to {output}")
    return output


def write_plan(args: argparse.Namespace) -> Path:
    ensure_dirs()
    libcs = canonical_libcs(args.libc)
    groups = split_csv(args.group) or ["ltp"]
    roots = [f"/{libc}" for libc in libcs]
    lines: list[str] = []
    for group in groups:
        if group == "ltp" and args.ltp_order == "glibc-first":
            ordered_roots = [root for root in ("/glibc", "/musl") if root in roots]
        elif group == "ltp" and args.ltp_order == "musl-first":
            ordered_roots = [root for root in ("/musl", "/glibc") if root in roots]
        else:
            ordered_roots = roots
        for root in ordered_roots:
            lines.append(f"{root} {group}")
    name = args.name or DEFAULT_PLAN_NAME
    output = Path(args.output).expanduser() if args.output else PLAN_DIR / f"{name}.txt"
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text("\n".join(lines) + "\n", encoding="utf-8")
    log(f"wrote plan to {output}")
    return output


def env_file_for(args: argparse.Namespace, run_path: Path) -> Path | None:
    lines: list[str] = []
    for item in split_csv(args.env):
        if "=" not in item:
            die(f"--env must be KEY=VALUE: {item}")
        lines.append(item)
    if args.ltp_budget is not None:
        lines.append(f"OSCOMP_LTP_GROUP_BUDGET_SECS={args.ltp_budget}")
    if args.glibc_budget is not None:
        lines.append(f"OSCOMP_LTP_GLIBC_GROUP_BUDGET_SECS={args.glibc_budget}")
    if args.musl_budget is not None:
        lines.append(f"OSCOMP_LTP_MUSL_GROUP_BUDGET_SECS={args.musl_budget}")
    if getattr(args, "case_timeout", None) is not None:
        if args.case_timeout < 0:
            die("--case-timeout must be non-negative")
        lines.append(f"OSCOMP_LTP_CASE_TIMEOUT_SECS={args.case_timeout}")
    if not lines:
        return None
    path = run_path / "oscomp.env"
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return path


def ensure_kernels(arches: list[str], rebuild: bool) -> None:
    targets = {"rv": "kernel-rv", "la": "kernel-la"}
    missing = [arch for arch in arches if not (REPO_ROOT / targets[arch]).is_file()]
    build_arches = arches if rebuild else missing
    for arch in build_arches:
        log(f"building {targets[arch]}")
        run_cmd(["make", targets[arch]], capture=False)


def build_support_image(args: argparse.Namespace, image_dir: Path, arches: list[str], test_list: Path, plan: Path) -> tuple[Path, Path | None]:
    support_image = image_dir / "support.img"
    arch_arg = "both" if len(arches) > 1 else arches[0]
    cmd = [
        "bash",
        str(REPO_ROOT / "scripts" / "build-oscomp-support-disk.sh"),
        "--arch",
        arch_arg,
        "--output",
        str(support_image),
        "--test-list",
        str(test_list),
        "--plan-override",
        str(plan),
    ]
    env_path = env_file_for(args, image_dir)
    if env_path:
        cmd.extend(["--env-override", str(env_path)])
    run_cmd(cmd, capture=True)
    return support_image, env_path


def resolve_parallel_mode(args: argparse.Namespace) -> str:
    mode = args.parallel
    if args.split_combos:
        mode = "combo"
    if args.no_parallel:
        mode = "serial"
    return mode


def resolve_jobs(args: argparse.Namespace, mode: str, task_count: int) -> int:
    if mode == "serial":
        return 1
    value = str(args.jobs or "auto")
    if value == "auto":
        return max(1, task_count)
    try:
        jobs = int(value)
    except ValueError:
        die("--jobs must be a positive integer or auto")
    if jobs <= 0:
        die("--jobs must be a positive integer or auto")
    return min(jobs, max(1, task_count))


def prepare_run_test_list(args: argparse.Namespace, run_id: str, run_path: Path) -> Path:
    output = run_path / "ltp_test.txt"
    if args.test_list:
        source = Path(args.test_list).expanduser()
        if not source.is_file():
            die(f"test list not found: {source}")
        if source.resolve() != output.resolve():
            shutil.copyfile(source, output)
        return output
    return generate_list(
        argparse.Namespace(
            inventory=args.inventory,
            arch=args.arch,
            libc=args.libc,
            mode=args.mode,
            case=args.case,
            runtest=args.runtest,
            include=args.include,
            exclude=args.exclude,
            shuffle=args.shuffle,
            seed=args.seed,
            offset=args.offset,
            limit=args.limit,
            name=f"{run_id}-list",
            output=str(output),
        )
    )


def write_task_plan(args: argparse.Namespace, output: Path, libcs: list[str]) -> Path:
    return write_plan(
        argparse.Namespace(
            libc=libcs,
            group=["ltp"],
            ltp_order=args.ltp_order,
            name=output.stem,
            output=str(output),
        )
    )


def prepare_common_plan(args: argparse.Namespace, run_path: Path, run_id: str) -> Path:
    output = run_path / "plan.txt"
    if args.plan:
        source = Path(args.plan).expanduser()
        if not source.is_file():
            die(f"plan not found: {source}")
        if source.resolve() != output.resolve():
            shutil.copyfile(source, output)
        return output
    return write_task_plan(args, output, canonical_libcs(args.libc))


def replay_command(args: argparse.Namespace, arch: str, support_image: Path, workdir: Path, timeout: int) -> list[str]:
    cmd = [
        str(REPO_ROOT / "scripts" / "replay-oscomp-eval.sh"),
        "--arch",
        arch,
        "--support-image",
        str(support_image),
        "--workdir",
        str(workdir),
        "--keep-workdir",
        "--timeout",
        str(timeout),
        "--skip-kernel-build",
    ]
    if args.image:
        cmd.extend(["--image", str(Path(args.image).expanduser())])
    return cmd


def create_replay_tasks(
    args: argparse.Namespace,
    run_path: Path,
    run_id: str,
    arches: list[str],
    libcs: list[str],
    test_list: Path,
    mode: str,
) -> list[ReplayTask]:
    task_timeout = args.task_timeout if args.task_timeout is not None else args.timeout
    if task_timeout <= 0:
        die("--task-timeout/--timeout must be positive")
    tasks: list[ReplayTask] = []
    if not arches:
        die("no arches selected")
    if not libcs:
        die("no libcs selected")
    if mode not in ("arch", "combo", "serial"):
        die(f"unsupported parallel mode: {mode}")

    if mode == "combo":
        if args.plan:
            die("--plan cannot be used with --parallel combo/--split-combos; combo tasks need one-libc plans")
        for arch in arches:
            for libc in libcs:
                task_id = f"{arch}-{libc}"
                task_dir = run_path / "tasks" / task_id
                task_dir.mkdir(parents=True)
                plan = write_task_plan(args, task_dir / "plan.txt", [libc])
                support_image, env_path = build_support_image(args, task_dir, [arch], test_list, plan)
                workdir = task_dir / "work"
                command = replay_command(args, arch, support_image, workdir, task_timeout)
                tasks.append(
                    ReplayTask(
                        task_id=task_id,
                        arch=arch,
                        libcs=[libc],
                        plan_path=plan,
                        support_image=support_image,
                        task_dir=task_dir,
                        workdir=workdir,
                        console_log=task_dir / "console.log",
                        timeout=task_timeout,
                        env_path=env_path,
                        command=command,
                    )
                )
        return tasks

    plan = prepare_common_plan(args, run_path, run_id)
    support_image, env_path = build_support_image(args, run_path, arches, test_list, plan)
    for arch in arches:
        task_dir = run_path / arch
        task_dir.mkdir(exist_ok=True)
        workdir = task_dir / "work"
        command = replay_command(args, arch, support_image, workdir, task_timeout)
        tasks.append(
            ReplayTask(
                task_id=arch,
                arch=arch,
                libcs=libcs,
                plan_path=plan,
                support_image=support_image,
                task_dir=task_dir,
                workdir=workdir,
                console_log=task_dir / "console.log",
                timeout=task_timeout,
                env_path=env_path,
                command=command,
            )
        )
    return tasks


def task_manifest(task: ReplayTask, run_path: Path) -> dict[str, Any]:
    def rel(path: Path | None) -> str:
        if path is None:
            return ""
        try:
            return str(path.relative_to(run_path))
        except ValueError:
            return str(path)

    return {
        "task_id": task.task_id,
        "arch": task.arch,
        "libcs": task.libcs,
        "plan": rel(task.plan_path),
        "support_image": rel(task.support_image),
        "env": rel(task.env_path),
        "task_dir": rel(task.task_dir),
        "workdir": rel(task.workdir),
        "console_log": rel(task.console_log),
        "timeout": task.timeout,
        "command": task.command,
    }


def result_manifest(result: ReplayResult, run_path: Path) -> dict[str, Any]:
    def rel(path: Path) -> str:
        try:
            return str(path.relative_to(run_path))
        except ValueError:
            return str(path)

    return {
        "task_id": result.task_id,
        "arch": result.arch,
        "libcs": result.libcs,
        "exit_code": result.exit_code,
        "console_log": rel(result.console_log),
        "cases_jsonl": rel(result.cases_jsonl),
        "summary_json": rel(result.summary_json),
        "started_at": result.started_at,
        "ended_at": result.ended_at,
        "duration_secs": result.duration_secs,
        "status": result.status,
        "killed_by_fail_fast": result.killed_by_fail_fast,
        "error": result.error,
    }


def terminate_process_group(proc: subprocess.Popen[Any]) -> None:
    try:
        os.killpg(proc.pid, signal.SIGTERM)
    except ProcessLookupError:
        return
    except OSError:
        proc.terminate()
    deadline = time.monotonic() + 2.0
    while proc.poll() is None and time.monotonic() < deadline:
        time.sleep(0.1)
    if proc.poll() is None:
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except ProcessLookupError:
            return
        except OSError:
            proc.kill()


def run_replay_task(task: ReplayTask, cancel_event: threading.Event) -> ReplayResult:
    started_at = iso_now()
    start = time.monotonic()
    status = "completed"
    exit_code: int | None = None
    killed_by_fail_fast = False
    error = ""
    task.task_dir.mkdir(parents=True, exist_ok=True)
    try:
        with task.console_log.open("w", encoding="utf-8", errors="replace") as out:
            if cancel_event.is_set():
                status = "cancelled"
                killed_by_fail_fast = True
                out.write("[ltp-lab] task cancelled before start\n")
                exit_code = None
            else:
                proc = subprocess.Popen(
                    task.command,
                    cwd=REPO_ROOT,
                    stdout=out,
                    stderr=subprocess.STDOUT,
                    text=True,
                    start_new_session=True,
                )
                while True:
                    ret = proc.poll()
                    if ret is not None:
                        exit_code = ret
                        status = "completed" if ret == 0 else "failed"
                        break
                    if cancel_event.is_set():
                        killed_by_fail_fast = True
                        status = "cancelled"
                        terminate_process_group(proc)
                        ret = proc.wait()
                        exit_code = ret
                        out.write("\n[ltp-lab] task killed by fail-fast\n")
                        break
                    time.sleep(1.0)
    except Exception as exc:  # pragma: no cover - defensive for long-running subprocess orchestration
        status = "failed"
        error = str(exc)
        exit_code = -1

    ended_at = iso_now()
    duration_secs = round(time.monotonic() - start, 3)
    if exit_code is None:
        (task.task_dir / "exit_code.txt").write_text("cancelled\n", encoding="utf-8")
    else:
        (task.task_dir / "exit_code.txt").write_text(f"{exit_code}\n", encoding="utf-8")
    if task.console_log.is_file():
        parse_log_file(task.console_log, arch=task.arch, output_dir=task.task_dir)
    return ReplayResult(
        task_id=task.task_id,
        arch=task.arch,
        libcs=task.libcs,
        exit_code=exit_code,
        console_log=task.console_log,
        cases_jsonl=task.task_dir / "cases.jsonl",
        summary_json=task.task_dir / "summary.json",
        started_at=started_at,
        ended_at=ended_at,
        duration_secs=duration_secs,
        status=status,
        killed_by_fail_fast=killed_by_fail_fast,
        error=error,
    )


def write_cases_jsonl(path: Path, cases: list[dict[str, Any]]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8") as out:
        for case in cases:
            out.write(json.dumps(case, sort_keys=True) + "\n")


def summary_from_cases(cases: list[dict[str, Any]], arch: str = "") -> dict[str, Any]:
    summary: dict[str, Any] = {
        "arch": arch,
        "cases": len(cases),
        "global_timeout": any(case.get("timed_out") for case in cases),
        "global_panic": any(case.get("panic") for case in cases),
        "by_status": {},
        "by_libc": {},
    }
    for case in cases:
        status = case.get("status") or "unknown"
        summary["by_status"][status] = summary["by_status"].get(status, 0) + 1
        libc = case.get("libc") or "unknown"
        by_libc = summary["by_libc"].setdefault(libc, {})
        by_libc[status] = by_libc.get(status, 0) + 1
    slow_cases = [
        {
            "case": case.get("case", ""),
            "arch": arch,
            "libc": case.get("libc") or "unknown",
            "status": case.get("status") or "unknown",
            "duration_secs": case.get("duration_secs"),
        }
        for case in cases
        if case.get("duration_secs") is not None
    ]
    slow_cases.sort(key=lambda item: float(item.get("duration_secs") or 0), reverse=True)
    if slow_cases:
        summary["slow_cases"] = slow_cases[:50]
    return summary


def read_exit_code(path: Path) -> int | None:
    if not path.is_file():
        return None
    text = path.read_text(encoding="utf-8", errors="replace").strip()
    try:
        return int(text)
    except ValueError:
        return None


def aggregate_split_combo_results(run_path: Path, tasks: list[ReplayTask]) -> None:
    for arch in ARCHES:
        arch_tasks = [task for task in tasks if task.arch == arch]
        if not arch_tasks:
            continue
        arch_dir = run_path / arch
        arch_dir.mkdir(exist_ok=True)
        cases: list[dict[str, Any]] = []
        exit_codes: list[int] = []
        for task in arch_tasks:
            cases.extend(read_cases_jsonl(task.task_dir / "cases.jsonl"))
            code = read_exit_code(task.task_dir / "exit_code.txt")
            if code is not None:
                exit_codes.append(code)
        write_cases_jsonl(arch_dir / "cases.jsonl", cases)
        write_json(arch_dir / "summary.json", summary_from_cases(cases, arch=arch))
        aggregate_exit = 0
        for code in exit_codes:
            if code != 0:
                aggregate_exit = code
                break
        if exit_codes:
            (arch_dir / "exit_code.txt").write_text(f"{aggregate_exit}\n", encoding="utf-8")


def refresh_split_combo_aggregate(run_path: Path, manifest: dict[str, Any]) -> None:
    parallel = manifest.get("parallel") if isinstance(manifest.get("parallel"), dict) else {}
    if not parallel.get("split_combos"):
        return
    task_defs = manifest.get("tasks") if isinstance(manifest.get("tasks"), dict) else {}
    if not isinstance(task_defs, dict) or not task_defs:
        return

    for arch in ARCHES:
        cases: list[dict[str, Any]] = []
        exit_codes: list[int] = []
        for task_id, task_def in sorted(task_defs.items()):
            if not isinstance(task_def, dict):
                continue
            task_arch = str(task_def.get("arch") or "")
            if task_arch not in ARCHES:
                task_arch, _ = task_arch_libcs_from_id(str(task_id))
            if task_arch != arch:
                continue
            task_dir = run_path / str(task_def.get("task_dir") or f"tasks/{task_id}")
            cases.extend(read_cases_jsonl(task_dir / "cases.jsonl"))
            code = read_exit_code(task_dir / "exit_code.txt")
            if code is not None:
                exit_codes.append(code)
        if not cases and not exit_codes:
            continue
        arch_dir = run_path / arch
        arch_dir.mkdir(exist_ok=True)
        if cases:
            write_cases_jsonl(arch_dir / "cases.jsonl", cases)
            write_json(arch_dir / "summary.json", summary_from_cases(cases, arch=arch))
        if exit_codes:
            aggregate_exit = next((code for code in exit_codes if code != 0), 0)
            (arch_dir / "exit_code.txt").write_text(f"{aggregate_exit}\n", encoding="utf-8")


def run_tasks(tasks: list[ReplayTask], jobs: int, fail_fast: bool) -> list[ReplayResult]:
    cancel_event = threading.Event()
    results: list[ReplayResult] = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=jobs) as executor:
        futures = {executor.submit(run_replay_task, task, cancel_event): task for task in tasks}
        pending = set(futures)
        while pending:
            done, pending = concurrent.futures.wait(pending, return_when=concurrent.futures.FIRST_COMPLETED)
            for future in done:
                task = futures[future]
                if future.cancelled():
                    task.task_dir.mkdir(parents=True, exist_ok=True)
                    task.console_log.write_text("[ltp-lab] task cancelled before start\n", encoding="utf-8")
                    (task.task_dir / "exit_code.txt").write_text("cancelled\n", encoding="utf-8")
                    parse_log_file(task.console_log, arch=task.arch, output_dir=task.task_dir)
                    results.append(
                        ReplayResult(
                            task_id=task.task_id,
                            arch=task.arch,
                            libcs=task.libcs,
                            exit_code=None,
                            console_log=task.console_log,
                            cases_jsonl=task.task_dir / "cases.jsonl",
                            summary_json=task.task_dir / "summary.json",
                            started_at=None,
                            ended_at=iso_now(),
                            duration_secs=0.0,
                            status="cancelled",
                            killed_by_fail_fast=True,
                        )
                    )
                    continue
                try:
                    result = future.result()
                except Exception as exc:  # pragma: no cover - defensive
                    result = ReplayResult(
                        task_id=task.task_id,
                        arch=task.arch,
                        libcs=task.libcs,
                        exit_code=-1,
                        console_log=task.console_log,
                        cases_jsonl=task.task_dir / "cases.jsonl",
                        summary_json=task.task_dir / "summary.json",
                        started_at=None,
                        ended_at=iso_now(),
                        duration_secs=0.0,
                        status="failed",
                        error=str(exc),
                    )
                results.append(result)
                if fail_fast and result.exit_code not in (0, None) and not cancel_event.is_set():
                    cancel_event.set()
                    for pending_future in pending:
                        pending_future.cancel()
    results.sort(key=lambda item: item.task_id)
    return results


def run_experiment(args: argparse.Namespace) -> Path:
    ensure_dirs()
    arches = canonical_arches(args.arch)
    libcs = canonical_libcs(args.libc)
    if args.image and len(arches) > 1:
        die("--image override is only valid for single-arch runs")
    run_id = args.name or now_id()
    parallel_mode = resolve_parallel_mode(args)

    if not args.skip_kernel_build:
        ensure_kernels(arches, args.rebuild_kernels)

    run_path = RUN_DIR / run_id
    if run_path.exists() and not args.replace:
        die(f"run already exists: {run_path}; pass --replace or choose --name")
    if run_path.exists():
        shutil.rmtree(run_path)
    run_path.mkdir(parents=True)

    test_list = prepare_run_test_list(args, run_id, run_path)
    tasks = create_replay_tasks(args, run_path, run_id, arches, libcs, test_list, parallel_mode)
    jobs = resolve_jobs(args, parallel_mode, len(tasks))
    manifest = {
        "run_id": run_id,
        "created_at": iso_now(),
        "repo_root": str(REPO_ROOT),
        "arches": arches,
        "libcs": libcs,
        "test_list": str(test_list),
        "parallel": {
            "mode": parallel_mode,
            "jobs": jobs,
            "split_combos": parallel_mode == "combo",
        },
        "tasks": {task.task_id: task_manifest(task, run_path) for task in tasks},
        "results": {},
    }
    write_json(run_path / "manifest.json", manifest)
    if args.prepare_only:
        log(f"prepared run inputs in {run_path}")
        return run_path

    log(f"running {len(tasks)} task(s) mode={parallel_mode} jobs={jobs}")
    for task in tasks:
        log(f"task {task.task_id}: arch={task.arch} libc={','.join(task.libcs)} log={task.console_log}")
    results = run_tasks(tasks, jobs, args.fail_fast)
    manifest["results"] = {result.task_id: result_manifest(result, run_path) for result in results}
    write_json(run_path / "manifest.json", manifest)
    if parallel_mode == "combo":
        aggregate_split_combo_results(run_path, tasks)
    summarize_run(run_path)
    replay_failures = {result.task_id: result.exit_code for result in results if result.exit_code not in (0, None)}
    cancelled = [result.task_id for result in results if result.status == "cancelled"]
    if replay_failures or cancelled:
        details = " ".join(f"{task}={code}" for task, code in sorted(replay_failures.items()))
        if cancelled:
            details = (details + " " if details else "") + "cancelled=" + ",".join(sorted(cancelled))
        die(f"replay failed: {details}")
    return run_path


GROUP_START_RE = re.compile(r"#### OS COMP TEST GROUP START ([^ ]+) ####")
GROUP_END_RE = re.compile(r"#### OS COMP TEST GROUP END ([^ ]+) ####")
RUN_CASE_RE = re.compile(r"^RUN LTP CASE (.+)$")
END_CASE_RE = re.compile(r"^FAIL LTP CASE (.+?) : (-?\d+)$")
CASE_TIMEOUT_RE = re.compile(r"^#### OSCOMP RUNNER LTP CASE TIMEOUT (.+?) AFTER (\d+)s ####$")
CASE_DURATION_RE = re.compile(r"^#### OSCOMP RUNNER LTP CASE DURATION (.+?) ([0-9.]+)s ####$")
SUMMARY_COUNT_RE = re.compile(r"^(passed|failed|broken|skipped|warnings)\s+(\d+)\s*$")


def flavor_from_group(group: str | None) -> str:
    if not group:
        return ""
    if group.endswith("-glibc"):
        return "glibc"
    if group.endswith("-musl"):
        return "musl"
    return ""


def new_case(name: str, group: str | None, line_no: int) -> dict[str, Any]:
    return {
        "case": name.strip(),
        "group": group or "",
        "libc": flavor_from_group(group),
        "line_start": line_no,
        "line_end": None,
        "ret": None,
        "results": {kind: 0 for kind in RESULT_KINDS},
        "summary": {},
        "timed_out": False,
        "timeout_secs": None,
        "duration_secs": None,
        "panic": False,
        "status": "running",
    }


def classify_case(case: dict[str, Any]) -> str:
    if case.get("panic"):
        return "panic"
    if case.get("timed_out"):
        return "timeout"
    ret = case.get("ret")
    results = case.get("results", {})
    summary = case.get("summary", {})
    failed = results.get("TFAIL", 0) + results.get("TBROK", 0) + int(summary.get("failed", 0)) + int(summary.get("broken", 0))
    passed = results.get("TPASS", 0) + int(summary.get("passed", 0))
    skipped = results.get("TCONF", 0) + int(summary.get("skipped", 0))
    if failed == 0 and passed == 0 and skipped > 0:
        return "silent-pass"
    if ret == 0 and failed == 0:
        return "pass" if passed > 0 or not results else "silent-pass"
    if ret is None:
        return "incomplete"
    if failed > 0:
        return "fail"
    if ret != 0:
        return "nonzero"
    return "unknown"


def parse_log_text(text: str, arch: str = "") -> dict[str, Any]:
    cases: list[dict[str, Any]] = []
    current_group: str | None = None
    current_case: dict[str, Any] | None = None
    global_panic = False
    global_timeout = False

    for line_no, line in enumerate(text.splitlines(), 1):
        start = GROUP_START_RE.search(line)
        if start:
            current_group = start.group(1)
            current_case = None
            continue
        end = GROUP_END_RE.search(line)
        if end:
            if current_case:
                current_case["line_end"] = line_no
                current_case["status"] = classify_case(current_case)
                cases.append(current_case)
            current_case = None
            current_group = None
            continue
        if "QEMU timed out" in line or "timed out after" in line or "TIMEOUT" in line:
            global_timeout = True
            if current_case:
                current_case["timed_out"] = True
        if "Kernel panic" in line or "panicked at" in line or "PANIC" in line:
            global_panic = True
            if current_case:
                current_case["panic"] = True
        run_case = RUN_CASE_RE.match(line)
        if run_case:
            if current_case:
                current_case["line_end"] = line_no - 1
                current_case["status"] = classify_case(current_case)
                cases.append(current_case)
            current_case = new_case(run_case.group(1), current_group, line_no)
            continue
        if current_case:
            for kind in RESULT_KINDS:
                if kind in line:
                    current_case["results"][kind] += 1
            timeout = CASE_TIMEOUT_RE.match(line)
            if timeout:
                current_case["timed_out"] = True
                current_case["timeout_secs"] = int(timeout.group(2))
            duration = CASE_DURATION_RE.match(line)
            if duration:
                try:
                    value = float(duration.group(2))
                except ValueError:
                    value = None
                current_case["duration_secs"] = value
            summary = SUMMARY_COUNT_RE.match(line.strip())
            if summary:
                current_case["summary"][summary.group(1)] = int(summary.group(2))
            end_case = END_CASE_RE.match(line)
            if end_case:
                current_case["line_end"] = line_no
                current_case["ret"] = int(end_case.group(2))
                current_case["status"] = classify_case(current_case)
                cases.append(current_case)
                current_case = None
    if current_case:
        current_case["status"] = classify_case(current_case)
        cases.append(current_case)

    summary: dict[str, Any] = {
        "arch": arch,
        "cases": len(cases),
        "global_timeout": global_timeout,
        "global_panic": global_panic,
        "by_status": {},
        "by_libc": {},
    }
    for case in cases:
        status = case["status"]
        summary["by_status"][status] = summary["by_status"].get(status, 0) + 1
        libc = case.get("libc") or "unknown"
        by_libc = summary["by_libc"].setdefault(libc, {})
        by_libc[status] = by_libc.get(status, 0) + 1
    slow_cases = [
        {
            "case": case.get("case", ""),
            "arch": arch,
            "libc": case.get("libc") or "unknown",
            "status": case.get("status") or "unknown",
            "duration_secs": case.get("duration_secs"),
        }
        for case in cases
        if case.get("duration_secs") is not None
    ]
    slow_cases.sort(key=lambda item: float(item.get("duration_secs") or 0), reverse=True)
    if slow_cases:
        summary["slow_cases"] = slow_cases[:50]
    return {"summary": summary, "cases": cases}


def parse_log_file(path: Path, *, arch: str = "", output_dir: Path | None = None) -> dict[str, Any]:
    text = path.read_text(encoding="utf-8", errors="replace")
    parsed = parse_log_text(text, arch=arch)
    out_dir = output_dir or path.parent
    write_json(out_dir / "summary.json", parsed["summary"])
    with (out_dir / "cases.jsonl").open("w", encoding="utf-8") as out:
        for case in parsed["cases"]:
            out.write(json.dumps(case, sort_keys=True) + "\n")
    return parsed


def parse_cmd(args: argparse.Namespace) -> None:
    path = Path(args.log).expanduser()
    if not path.is_file():
        die(f"log not found: {path}")
    output = Path(args.output_dir).expanduser() if args.output_dir else path.parent
    parsed = parse_log_file(path, arch=args.arch or "", output_dir=output)
    print_summary(parsed["summary"])


def print_summary(summary: dict[str, Any]) -> None:
    arch = summary.get("arch") or "-"
    print(f"arch={arch} cases={summary.get('cases', 0)} timeout={summary.get('global_timeout')} panic={summary.get('global_panic')}")
    for status, count in sorted(summary.get("by_status", {}).items()):
        print(f"  {status}: {count}")
    for libc, statuses in sorted(summary.get("by_libc", {}).items()):
        details = " ".join(f"{k}={v}" for k, v in sorted(statuses.items()))
        print(f"  {libc}: {details}")


def add_counts(dst: dict[str, int], src: dict[str, Any]) -> None:
    for key, value in src.items():
        try:
            dst[key] = dst.get(key, 0) + int(value)
        except (TypeError, ValueError):
            pass


def summarize_run(run_path: Path) -> dict[str, Any]:
    run_path = run_path.expanduser()
    if not run_path.is_dir():
        die(f"run dir not found: {run_path}")
    manifest: dict[str, Any] = {}
    manifest_path = run_path / "manifest.json"
    if manifest_path.is_file():
        try:
            manifest = read_json(manifest_path)
        except json.JSONDecodeError:
            manifest = {}
    refresh_split_combo_aggregate(run_path, manifest)

    arch_summaries: dict[str, Any] = {}
    exit_codes: dict[str, int] = {}
    by_combo: dict[str, dict[str, int]] = {}
    by_arch: dict[str, dict[str, int]] = {}
    total: dict[str, int] = {}

    for arch in ARCHES:
        summary_path = run_path / arch / "summary.json"
        if summary_path.is_file():
            summary = read_json(summary_path)
            arch_summaries[arch] = summary
            by_arch[arch] = dict(summary.get("by_status", {}))
            add_counts(total, summary.get("by_status", {}))
            for libc, statuses in summary.get("by_libc", {}).items():
                key = f"{arch}/{libc}"
                by_combo[key] = dict(statuses)
        exit_code_path = run_path / arch / "exit_code.txt"
        if exit_code_path.is_file():
            code = read_exit_code(exit_code_path)
            exit_codes[arch] = code if code is not None else -1

    task_summaries: dict[str, Any] = {}
    task_exit_codes: dict[str, int | None] = {}
    task_results = (manifest.get("results") or {}) if isinstance(manifest.get("results"), dict) else {}
    task_defs = (manifest.get("tasks") or {}) if isinstance(manifest.get("tasks"), dict) else {}
    for task_id, task_def in sorted(task_defs.items()):
        task_dir = run_path / (task_def.get("task_dir") or "")
        summary_path = task_dir / "summary.json"
        exit_code_path = task_dir / "exit_code.txt"
        summary = read_json(summary_path) if summary_path.is_file() else {}
        result = task_results.get(task_id, {})
        code = result.get("exit_code")
        if code is None:
            code = read_exit_code(exit_code_path)
        task_exit_codes[task_id] = code
        task_summaries[task_id] = {
            "arch": task_def.get("arch", ""),
            "libcs": task_def.get("libcs", []),
            "exit_code": code,
            "duration_secs": result.get("duration_secs"),
            "status": result.get("status", "prepared" if not result else ""),
            "cases": summary.get("cases", 0),
            "by_status": summary.get("by_status", {}),
        }

    failed_arches = {arch: code for arch, code in exit_codes.items() if code != 0}
    failed_tasks = [
        task_id
        for task_id, task in task_summaries.items()
        if task.get("exit_code") not in (0, None)
    ]
    cancelled_tasks = [
        task_id
        for task_id, task in task_summaries.items()
        if task.get("status") == "cancelled"
    ]

    parallel = manifest.get("parallel") or {"mode": "serial", "jobs": 1, "split_combos": False}
    combined = {
        "run": str(run_path),
        "run_id": manifest.get("run_id", run_path.name),
        "parallel": parallel,
        "tasks": task_summaries,
        "arches": arch_summaries,
        "by_combo": by_combo,
        "by_arch": by_arch,
        "exit_codes": exit_codes,
        "task_exit_codes": task_exit_codes,
        "failed_arches": failed_arches,
        "failed_tasks": failed_tasks,
        "cancelled_tasks": cancelled_tasks,
        "total_by_status": total,
    }
    write_json(run_path / "combined-summary.json", combined)
    print(f"run: {run_path}")
    if parallel:
        print(
            "parallel: "
            f"mode={parallel.get('mode', 'serial')} jobs={parallel.get('jobs', 1)}"
        )
    if task_summaries:
        print("tasks:")
        for task_id, task in sorted(task_summaries.items()):
            statuses = task.get("by_status", {})
            details = " ".join(f"{k}={v}" for k, v in sorted(statuses.items()))
            duration = task.get("duration_secs")
            duration_text = f" duration={duration}s" if duration is not None else ""
            print(
                f"  {task_id} exit={task.get('exit_code')} cases={task.get('cases', 0)}"
                f"{duration_text}{' ' + details if details else ''}"
            )
    for arch in sorted(arch_summaries):
        if arch in exit_codes:
            print(f"replay_exit[{arch}]={exit_codes[arch]}")
        print_summary(arch_summaries[arch])
    if total:
        print("total: " + " ".join(f"{k}={v}" for k, v in sorted(total.items())))
    return combined


def summarize_cmd(args: argparse.Namespace) -> None:
    summarize_run(Path(args.run_dir).expanduser())


def failures_cmd(args: argparse.Namespace) -> None:
    run_path = Path(args.run_dir).expanduser()
    if not run_path.is_dir():
        die(f"run dir not found: {run_path}")
    statuses = set(split_csv(args.status) or ["fail", "nonzero", "timeout", "panic", "incomplete"])
    rows: list[dict[str, Any]] = []
    for arch in ARCHES:
        for case in run_cases_for_arch(run_path, arch):
            if case.get("status") not in statuses:
                continue
            rows.append(
                {
                    "arch": arch,
                    "libc": case.get("libc") or "unknown",
                    "case": case.get("case", ""),
                    "status": case.get("status", ""),
                    "ret": case.get("ret"),
                    "results": case.get("results", {}),
                    "summary": case.get("summary", {}),
                    "line_start": case.get("line_start"),
                    "line_end": case.get("line_end"),
                }
            )
    rows.sort(key=lambda item: (item["case"], item["arch"], item["libc"], item["status"]))
    if args.json:
        print(json.dumps(rows, indent=2, sort_keys=True))
        return
    print(f"run: {run_path}")
    print(f"matching cases: {len(rows)}")
    grouped: dict[str, list[dict[str, Any]]] = {}
    for row in rows:
        grouped.setdefault(row["case"], []).append(row)
    for case_name, items in sorted(grouped.items())[: args.limit]:
        details = []
        for item in items:
            detail = f"{item['arch']}/{item['libc']}:{item['status']}"
            if item["ret"] is not None:
                detail += f"(ret={item['ret']})"
            details.append(detail)
        print(f"{case_name}: {' '.join(details)}")


def read_cases_jsonl(path: Path) -> list[dict[str, Any]]:
    cases: list[dict[str, Any]] = []
    if not path.is_file():
        return cases
    for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
        if line.strip():
            cases.append(json.loads(line))
    return cases


def task_arch_libcs_from_id(task_id: str) -> tuple[str, list[str]]:
    for arch in ARCHES:
        prefix = f"{arch}-"
        if task_id.startswith(prefix):
            libc = task_id[len(prefix) :]
            if libc in LIBCS:
                return arch, [libc]
    if task_id in ARCHES:
        return task_id, list(LIBCS)
    return "", []


def task_case_sources(run_dir: Path) -> list[tuple[Path, str, list[str]]]:
    sources: list[tuple[Path, str, list[str]]] = []
    seen: set[Path] = set()
    manifest_path = run_dir / "manifest.json"
    if manifest_path.is_file():
        try:
            manifest = read_json(manifest_path)
        except json.JSONDecodeError:
            manifest = {}
        task_defs = manifest.get("tasks") if isinstance(manifest, dict) else {}
        if isinstance(task_defs, dict):
            for task_id, task_def in sorted(task_defs.items()):
                if not isinstance(task_def, dict):
                    continue
                task_dir_text = task_def.get("task_dir") or f"tasks/{task_id}"
                cases_path = run_dir / str(task_dir_text) / "cases.jsonl"
                if cases_path in seen or not cases_path.is_file():
                    continue
                arch = str(task_def.get("arch") or "")
                libcs = [str(item) for item in task_def.get("libcs", []) if str(item) in LIBCS]
                if arch not in ARCHES:
                    arch, inferred_libcs = task_arch_libcs_from_id(str(task_id))
                    if not libcs:
                        libcs = inferred_libcs
                seen.add(cases_path)
                sources.append((cases_path, arch, libcs))

    tasks_dir = run_dir / "tasks"
    if tasks_dir.is_dir():
        for cases_path in sorted(tasks_dir.glob("*/cases.jsonl")):
            if cases_path in seen:
                continue
            arch, libcs = task_arch_libcs_from_id(cases_path.parent.name)
            seen.add(cases_path)
            sources.append((cases_path, arch, libcs))
    return sources


def run_case_records(run_dir: Path) -> list[tuple[str, list[str], dict[str, Any], str]]:
    records: list[tuple[str, list[str], dict[str, Any], str]] = []
    for arch in ARCHES:
        cases_path = run_dir / arch / "cases.jsonl"
        for case in read_cases_jsonl(cases_path):
            records.append((arch, [], case, str(cases_path)))
    for cases_path, arch, libcs in task_case_sources(run_dir):
        if arch not in ARCHES:
            continue
        for case in read_cases_jsonl(cases_path):
            records.append((arch, libcs, case, str(cases_path)))
    return records


def run_cases_for_arch(run_dir: Path, arch: str) -> list[dict[str, Any]]:
    cases = read_cases_jsonl(run_dir / arch / "cases.jsonl")
    if cases:
        return cases
    fallback: list[dict[str, Any]] = []
    for cases_path, task_arch, task_libcs in task_case_sources(run_dir):
        if task_arch != arch:
            continue
        for case in read_cases_jsonl(cases_path):
            item = dict(case)
            if not item.get("libc") and len(task_libcs) == 1:
                item["libc"] = task_libcs[0]
            fallback.append(item)
    return fallback


def combo_key(arch: str, libc: str) -> str:
    return f"{arch}/{libc}"


def all_combos() -> list[str]:
    return [combo_key(arch, libc) for arch in ARCHES for libc in LIBCS]


def normalize_required_combos(values: list[str] | None) -> list[str]:
    raw_items = split_csv(values)
    if not raw_items:
        return all_combos()

    result: list[str] = []

    def add_combo(arch: str, libc: str) -> None:
        key = combo_key(arch, libc)
        if key not in result:
            result.append(key)

    for raw in raw_items:
        item = raw.strip()
        if not item:
            continue
        item = item.replace("-", "/").replace(":", "/")
        if item in ("all", "both", "matrix"):
            for arch in ARCHES:
                for libc in LIBCS:
                    add_combo(arch, libc)
            continue
        if item in ("rv", "riscv64", "la", "loongarch64"):
            arch = canonical_arch(item)
            for libc in LIBCS:
                add_combo(arch, libc)
            continue
        if item in LIBCS:
            for arch in ARCHES:
                add_combo(arch, item)
            continue
        if "/" in item:
            parts = item.split("/")
            if len(parts) != 2:
                die(f"invalid --require combo: {raw}")
            arch_text, libc_text = parts
            arches = list(ARCHES) if arch_text in ("all", "both") else [canonical_arch(arch_text)]
            if libc_text in ("all", "both"):
                libcs = list(LIBCS)
            elif libc_text in LIBCS:
                libcs = [libc_text]
            else:
                die(f"invalid --require libc: {raw}")
            for arch in arches:
                for libc in libcs:
                    add_combo(arch, libc)
            continue
        die(f"invalid --require selector: {raw}")

    if not result:
        die("no required combos selected")
    return result


def status_rank(status: str, passing_statuses: set[str]) -> int:
    if status in passing_statuses:
        return 100
    return {
        "silent-pass": 90,
        "fail": 60,
        "nonzero": 50,
        "timeout": 40,
        "panic": 30,
        "incomplete": 20,
        "unknown": 10,
    }.get(status or "", 0)


def load_run_list_lines(run_dirs: list[Path]) -> dict[str, str]:
    line_by_case: dict[str, str] = {}
    for run_dir in run_dirs:
        list_path = run_dir / "ltp_test.txt"
        for item in parse_test_list(list_path):
            line_by_case[item["marker"]] = item["line"].strip()
    return line_by_case


def collect_case_evidence(
    run_dirs: list[Path],
    required: list[str],
    passing_statuses: set[str],
) -> dict[str, dict[str, dict[str, Any]]]:
    evidence: dict[str, dict[str, dict[str, Any]]] = {}
    required_set = set(required)
    for run_dir in run_dirs:
        for arch, task_libcs, case, source in run_case_records(run_dir):
            libc = case.get("libc") or (task_libcs[0] if len(task_libcs) == 1 else "unknown")
            key = combo_key(arch, libc)
            if key not in required_set:
                continue
            name = case.get("case", "")
            if not name:
                continue
            by_combo = evidence.setdefault(name, {})
            existing = by_combo.get(key)
            if existing is None or status_rank(case.get("status", ""), passing_statuses) > status_rank(existing.get("status", ""), passing_statuses):
                item = dict(case)
                item["arch"] = arch
                item["libc"] = libc
                item["combo"] = key
                item["run_dir"] = str(run_dir)
                item["source"] = source
                by_combo[key] = item
    return evidence


def candidate_case_order(test_list: Path | None, evidence: dict[str, dict[str, dict[str, Any]]]) -> list[str]:
    if test_list:
        items = parse_test_list(test_list)
        if items:
            return [item["marker"] for item in items]
    return sorted(evidence)


def case_combo_status(
    evidence: dict[str, dict[str, dict[str, Any]]],
    case: str,
    combo: str,
) -> str:
    item = evidence.get(case, {}).get(combo)
    if not item:
        return "missing"
    return str(item.get("status") or "unknown")


def promotable_cases(
    evidence: dict[str, dict[str, dict[str, Any]]],
    required: list[str],
    passing_statuses: set[str],
) -> set[str]:
    return {
        case
        for case, by_combo in evidence.items()
        if all(by_combo.get(combo, {}).get("status") in passing_statuses for combo in required)
    }


def print_case_matrix(
    cases: list[str],
    evidence: dict[str, dict[str, dict[str, Any]]],
    required: list[str],
    *,
    only_missing: bool = False,
    limit: int | None = None,
) -> None:
    shown = 0
    for case in cases:
        statuses = {combo: case_combo_status(evidence, case, combo) for combo in required}
        if only_missing and all(status != "missing" for status in statuses.values()):
            continue
        details = " ".join(f"{combo}={status}" for combo, status in statuses.items())
        print(f"{case}: {details}")
        shown += 1
        if limit is not None and shown >= limit:
            break


def promote_cmd(args: argparse.Namespace) -> None:
    run_dirs = [Path(item).expanduser() for item in args.run_dir]
    required = normalize_required_combos(args.require)
    passing_statuses = {"pass"}
    if args.allow_silent_pass:
        passing_statuses.add("silent-pass")
    line_by_case = load_run_list_lines(run_dirs)
    if args.test_list:
        for item in parse_test_list(Path(args.test_list).expanduser()):
            line_by_case[item["marker"]] = item["line"].strip()
    evidence = collect_case_evidence(run_dirs, required, passing_statuses)
    promoted = promotable_cases(evidence, required, passing_statuses)
    base_path = Path(args.base).expanduser() if args.base else DEFAULT_TEST_LIST
    base_items = parse_test_list(base_path)
    existing = {item["marker"] for item in base_items}
    lines = [item["line"].strip() for item in base_items]
    added = 0
    candidates = candidate_case_order(Path(args.test_list).expanduser() if args.test_list else None, evidence)
    if not candidates:
        candidates = sorted(promoted)
    if args.explain or args.status_matrix:
        promotable = [case for case in candidates if case in promoted]
        blocked = [case for case in candidates if case not in promoted]
        print("promotable:")
        for case in promotable:
            details = " ".join(f"{combo} pass" for combo in required)
            print(f"  {case}: {details}")
        print("not promoted:")
        for case in blocked:
            statuses = {combo: case_combo_status(evidence, case, combo) for combo in required}
            if args.show_missing and all(status != "missing" for status in statuses.values()):
                continue
            details = " ".join(f"{combo} {status}" for combo, status in statuses.items())
            print(f"  {case}: {details}")
    for case in sorted(promoted):
        if case in existing:
            continue
        lines.append(line_by_case.get(case, case))
        added += 1
    output = Path(args.output).expanduser()
    if args.dry_run:
        log(f"dry-run: would promote {added} new cases into {output} (base={len(base_items)} total={len(lines)})")
    else:
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_text("\n".join(lines) + "\n", encoding="utf-8")
        log(f"promoted {added} new cases into {output} (base={len(base_items)} total={len(lines)})")


def matrix_status_cmd(args: argparse.Namespace) -> None:
    run_dirs = [Path(item).expanduser() for item in args.run_dir]
    required = normalize_required_combos(args.require)
    passing_statuses = {"pass", "silent-pass"}
    evidence = collect_case_evidence(run_dirs, required, passing_statuses)
    test_list = Path(args.test_list).expanduser() if args.test_list else None
    cases = candidate_case_order(test_list, evidence)
    print_case_matrix(cases, evidence, required, only_missing=args.only_missing, limit=args.limit)


def missing_combos_cmd(args: argparse.Namespace) -> None:
    run_dirs = [Path(item).expanduser() for item in args.run_dir]
    required = normalize_required_combos(args.require)
    passing_statuses = {"pass"}
    if args.allow_silent_pass:
        passing_statuses.add("silent-pass")
    evidence = collect_case_evidence(run_dirs, required, passing_statuses)
    test_list = Path(args.test_list).expanduser() if args.test_list else None
    cases = candidate_case_order(test_list, evidence)
    line_by_case = load_run_list_lines(run_dirs)
    if test_list:
        for item in parse_test_list(test_list):
            line_by_case[item["marker"]] = item["line"].strip()
    output_dir = Path(args.output).expanduser()
    output_dir.mkdir(parents=True, exist_ok=True)
    counts: dict[str, int] = {}
    for combo in required:
        missing_lines: list[str] = []
        for case in cases:
            status = case_combo_status(evidence, case, combo)
            if status in passing_statuses:
                continue
            missing_lines.append(line_by_case.get(case, case))
        path = output_dir / f"{combo.replace('/', '-')}.txt"
        path.write_text("\n".join(missing_lines) + ("\n" if missing_lines else ""), encoding="utf-8")
        counts[combo] = len(missing_lines)
        log(f"wrote {len(missing_lines)} missing cases for {combo} to {path}")
    write_json(output_dir / "summary.json", {"required": required, "counts": counts})


def reorder_cmd(args: argparse.Namespace) -> None:
    base_path = Path(args.base).expanduser()
    base_items = parse_test_list(base_path)
    if not base_items:
        die(f"base list is empty or missing: {base_path}")
    run_dirs = [Path(item).expanduser() for item in args.evidence]
    required = normalize_required_combos(args.require)
    passing_statuses = {"pass", "silent-pass"}
    evidence = collect_case_evidence(run_dirs, required, passing_statuses)

    def score_item(item: dict[str, Any]) -> tuple[int, float, int]:
        case = item["marker"]
        statuses = [case_combo_status(evidence, case, combo) for combo in required]
        durations = [
            float(ev.get("duration_secs") or 0)
            for ev in evidence.get(case, {}).values()
            if ev.get("duration_secs") is not None
        ]
        max_duration = max(durations) if durations else 0.0
        if statuses and all(status in passing_statuses for status in statuses):
            bucket = 0 if max_duration <= args.fast_threshold else 1
        elif any(status in {"timeout", "panic"} for status in statuses):
            bucket = 4
        elif any(status == "missing" for status in statuses):
            bucket = 3
        else:
            bucket = 2
        return (bucket, max_duration, int(item["line_no"]))

    ordered = sorted(base_items, key=score_item)
    output = Path(args.output).expanduser()
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text("\n".join(item["line"].strip() for item in ordered) + "\n", encoding="utf-8")
    write_json(
        output.with_suffix(output.suffix + ".json"),
        {
            "base": str(base_path),
            "evidence": [str(path) for path in run_dirs],
            "output": str(output),
            "count": len(ordered),
            "fast_threshold": args.fast_threshold,
        },
    )
    log(f"wrote reordered list with {len(ordered)} cases to {output}")


SEMANTIC_DEFS: list[dict[str, Any]] = [
    {
        "id": "fs-open-permission",
        "title": "Open, Access, Mode, And Ownership Semantics",
        "patterns": [
            "open*",
            "openat*",
            "creat*",
            "access*",
            "faccessat*",
            "chmod*",
            "fchmod*",
            "fchmodat*",
            "chown*",
            "fchown*",
            "lchown*",
            "umask*",
        ],
        "required": [
            "Validate Linux-compatible open/access mode, flag, O_PATH, and permission behavior.",
            "Return the errno expected by the testcase for invalid pointers, bad flags, missing files, and permission failures.",
            "Keep VFS metadata changes coherent across file handles, directory entries, and stat-family syscalls.",
        ],
        "linux_refs": ["fs/open.c", "fs/namei.c", "fs/stat.c", "include/uapi/asm-generic/fcntl.h"],
        "kernel_paths": ["kernel/src/syscall/fs", "kernel/src/file", "third_party/rust-patches/axfs-ng"],
    },
    {
        "id": "fs-link-rename-unlink",
        "title": "Link, Rename, Symlink, And Directory Entry Semantics",
        "patterns": [
            "link*",
            "linkat*",
            "unlink*",
            "unlinkat*",
            "rename*",
            "renameat*",
            "renameat2*",
            "symlink*",
            "symlinkat*",
            "readlink*",
            "readlinkat*",
        ],
        "required": [
            "Implement Linux-like link count, overwrite, exchange, no-replace, directory, and cross-directory behavior.",
            "Keep directory entries, inode cache state, and page-cache lifetime coherent after rename or unlink.",
            "Cross-check edge-case errno with test source and Linux VFS paths before editing kernel code.",
        ],
        "linux_refs": ["fs/namei.c", "fs/libfs.c", "fs/inode.c"],
        "kernel_paths": ["kernel/src/syscall/fs", "kernel/src/file", "kernel/src/pseudofs", "third_party/rust-patches/axfs-ng"],
    },
    {
        "id": "fs-truncate-fallocate",
        "title": "Truncate, Fallocate, Size, And EOF Semantics",
        "patterns": ["truncate*", "ftruncate*", "fallocate*", "posix_fallocate*"],
        "required": [
            "Update inode size, allocated storage, and cached pages consistently for growth and shrink paths.",
            "Honor keep-size and unsupported fallocate mode errno behavior where the official testcase checks it.",
            "Invalidate or zero page-cache ranges past EOF so later reads and stats observe Linux-compatible state.",
        ],
        "linux_refs": ["fs/open.c", "mm/truncate.c", "mm/filemap.c", "include/uapi/linux/falloc.h"],
        "kernel_paths": ["kernel/src/syscall/fs/io.rs", "kernel/src/file", "third_party/rust-patches/axfs-ng/src/highlevel/file.rs"],
    },
    {
        "id": "fs-read-write-copy",
        "title": "Read, Write, Vector IO, And File Copy Semantics",
        "patterns": [
            "read*",
            "write*",
            "pread*",
            "pwrite*",
            "readv*",
            "writev*",
            "preadv*",
            "pwritev*",
            "copy_file_range*",
            "sendfile*",
            "splice*",
            "tee*",
        ],
        "required": [
            "Preserve Linux-compatible short IO, offset update, bad fd, bad buffer, and vector validation behavior.",
            "Keep direct IO, buffered IO, and page-cache visibility coherent for later reads and stats.",
            "Check Linux fallback behavior for copy-like syscalls before implementing backend shortcuts.",
        ],
        "linux_refs": ["fs/read_write.c", "fs/splice.c", "mm/filemap.c"],
        "kernel_paths": ["kernel/src/syscall/fs/io.rs", "kernel/src/file", "third_party/rust-patches/axfs-ng"],
    },
    {
        "id": "fs-stat-directory-xattr",
        "title": "Stat, Directory, Filesystem Info, And Xattr Semantics",
        "patterns": [
            "stat*",
            "fstat*",
            "lstat*",
            "statx*",
            "newfstatat*",
            "getdents*",
            "readdir*",
            "statfs*",
            "fstatfs*",
            "*xattr*",
        ],
        "required": [
            "Return stable Linux-like metadata fields, masks, mode bits, nlink, timestamps, sizes, and filesystem info.",
            "Make directory iteration offsets and end-of-directory behavior repeatable across libc variants.",
            "Treat unsupported xattr and statx feature combinations with Linux-compatible errno or mask behavior.",
        ],
        "linux_refs": ["fs/stat.c", "fs/readdir.c", "fs/statfs.c", "fs/xattr.c"],
        "kernel_paths": ["kernel/src/syscall/fs", "kernel/src/file", "kernel/src/pseudofs"],
    },
    {
        "id": "fs-sync-cache-mmap",
        "title": "Sync, Page Cache, Readahead, And Mmap File Semantics",
        "patterns": ["fsync*", "fdatasync*", "sync*", "syncfs*", "msync*", "mmap*", "munmap*", "mincore*", "readahead*", "ioctl*"],
        "required": [
            "Keep dirty data, page-cache invalidation, mmap visibility, and sync ordering coherent.",
            "Avoid deadlocks or lost dirty state in global page-cache flush and reclaim paths.",
            "Map unsupported device or ioctl cases to Linux-compatible errno instead of panicking.",
        ],
        "linux_refs": ["fs/sync.c", "mm/filemap.c", "mm/mmap.c", "mm/readahead.c", "fs/ioctl.c"],
        "kernel_paths": ["kernel/src/mm", "kernel/src/syscall/fs", "kernel/src/file", "third_party/rust-patches/axfs-ng"],
    },
    {
        "id": "process-signal-time",
        "title": "Process, Signal, Time, And Scheduler-Visible Semantics",
        "patterns": [
            "clone*",
            "fork*",
            "exec*",
            "wait*",
            "kill*",
            "signal*",
            "sig*",
            "clock*",
            "timer*",
            "nanosleep*",
            "futex*",
            "sched*",
        ],
        "required": [
            "Preserve Linux-visible process, signal, wait, timer, and futex contracts across libc variants.",
            "Treat timeout, wakeup, restart, and errno behavior as correctness and performance-sensitive.",
        ],
        "linux_refs": ["kernel/fork.c", "kernel/signal.c", "kernel/time", "kernel/futex"],
        "kernel_paths": ["kernel/src/task", "kernel/src/syscall", "kernel/src/mm"],
    },
]


def rel_to_repo(path: Path) -> str:
    try:
        return str(path.relative_to(REPO_ROOT))
    except ValueError:
        return str(path)


def campaign_path(name_or_path: str) -> Path:
    path = Path(name_or_path).expanduser()
    if path.is_absolute() or path.parent != Path("."):
        return path
    return CAMPAIGN_DIR / name_or_path


def campaign_manifest_path(path: Path) -> Path:
    return path / "manifest.json"


def load_campaign(name_or_path: str) -> tuple[Path, dict[str, Any]]:
    path = campaign_path(name_or_path)
    manifest_path = campaign_manifest_path(path)
    if not manifest_path.is_file():
        die(f"campaign manifest not found: {manifest_path}")
    return path, read_json(manifest_path)


def save_campaign(path: Path, manifest: dict[str, Any]) -> None:
    manifest["updated_at"] = iso_now()
    write_json(campaign_manifest_path(path), manifest)


def testcase_source_dirs(source_root: Path | None) -> list[Path]:
    dirs: list[Path] = []
    for root in candidate_testsuite_sources(source_root):
        for candidate in (
            root / "ltp-full-20240524" / "testcases",
            root / "ltp" / "testcases",
            root / "testcases",
        ):
            if candidate.is_dir() and candidate not in dirs:
                dirs.append(candidate)
    return dirs


def build_test_source_index(source_root: Path | None) -> dict[str, list[Path]]:
    index: dict[str, list[Path]] = {}
    for base in testcase_source_dirs(source_root):
        for path in base.rglob("*"):
            if not path.is_file():
                continue
            if path.suffix and path.suffix not in {".c", ".h", ".sh", ".py", ".pl", ".txt"}:
                continue
            keys = {path.name, path.stem}
            for key in keys:
                index.setdefault(key, []).append(path)
    for key in list(index):
        index[key] = sorted(index[key])[:8]
    return index


def find_test_sources(index: dict[str, list[Path]], marker: str, exec_name: str) -> list[str]:
    found: list[Path] = []
    for key in (marker, exec_name, f"{marker}.c", f"{exec_name}.c", f"{marker}.sh", f"{exec_name}.sh"):
        for path in index.get(key, []):
            if path not in found:
                found.append(path)
    return [rel_to_repo(path) for path in found[:10]]


def semantic_for_case(marker: str, exec_name: str) -> dict[str, Any]:
    keys = [marker, exec_name]
    for semantic in SEMANTIC_DEFS:
        for pattern in semantic["patterns"]:
            if any(fnmatch.fnmatch(key, pattern) for key in keys):
                return semantic
    return {
        "id": "general-linux-abi",
        "title": "General Linux ABI Semantics",
        "patterns": [],
        "required": [
            "Read the testcase source and Linux behavior before implementation.",
            "Prefer subsystem-level Linux-compatible behavior over per-case special handling.",
            "Record the concrete syscall, errno, metadata, or timing contract that the case checks.",
        ],
        "linux_refs": ["kernel", "fs", "mm", "include/uapi"],
        "kernel_paths": ["kernel/src"],
    }


def linux_ref_paths(root: Path, refs: list[str]) -> list[str]:
    paths: list[str] = []
    for ref in refs:
        path = root / ref
        paths.append(rel_to_repo(path) if path.exists() else str(path))
    return paths


def campaign_candidate_records(campaign_dir: Path, inv: dict[str, Any] | None, source_root: Path | None) -> list[dict[str, Any]]:
    list_path = campaign_dir / "candidates.txt"
    items = parse_test_list(list_path)
    runtest_by_line: dict[str, dict[str, Any]] = {}
    if inv:
        for entry in inv.get("source_runtest", {}).get("entries_data", []):
            runtest_by_line.setdefault(entry.get("line", ""), entry)
    source_index = build_test_source_index(source_root)
    records: list[dict[str, Any]] = []
    for item in items:
        line = item["line"].strip()
        runtest_entry = runtest_by_line.get(line, {})
        exec_name = runtest_entry.get("exec") or (item["tokens"][1] if len(item["tokens"]) > 1 else item["marker"])
        semantic = semantic_for_case(item["marker"], exec_name)
        records.append(
            {
                "marker": item["marker"],
                "exec": exec_name,
                "line": line,
                "line_no": item["line_no"],
                "runtest": runtest_entry.get("runtest", ""),
                "runtest_line_no": runtest_entry.get("line_no"),
                "semantic": semantic["id"],
                "test_sources": find_test_sources(source_index, item["marker"], exec_name),
            }
        )
    return records


def write_campaign_readme(campaign_dir: Path, manifest: dict[str, Any]) -> None:
    name = manifest["name"]
    text = f"""# {name}

Goal: {manifest.get("goal", "")}

This campaign is a fixed LTP case ledger first and a validation record second.
Use it to drive real shared kernel behavior before spending time on replay. Do
not add or remove cases while implementing semantics; create a new campaign for
the next batch.

## Files

- `manifest.json`: campaign settings, associated runs, and finish status.
- `candidates.txt`: fixed LTP test list for this batch.
- `cases.jsonl`: candidate metadata, source pointers, and semantic bucket.
- `semantics/*.md`: short implementation prompts tied to testcase and Linux behavior.
- `implementation.md`: ledger for kernel changes and cross-check notes.
- `taxonomy.md`: static unfinished-work map before replay; observed failure taxonomy after analyze.
- `analysis.json`: generated after `campaign analyze` or `campaign finish`.
- `promotable.txt`: generated case lines that have all required pass evidence.

## Commands

Code-first phase:

```bash
make lab-status NAME={name}
make kernels
```

Before running replay, read `cases.jsonl`, `semantics/*.md`, testcase sources,
Linux reference code, and local kernel paths. Record shared kernel behavior,
covered buckets, expected candidate coverage, deferred validation groups, and
unresolved cases in `implementation.md` and `taxonomy.md`.

Validation phase, after a meaningful implementation pass:

```bash
make lab-run NAME={name}
make lab-review NAME={name}
make lab-apply NAME={name}
make lab-done NAME={name}
```

Promotion still requires all required rv/la x glibc/musl parser `pass` evidence.
Run these commands inside an already-open `make dev-shell`; use
`make dev-shell DEV_CMD='...'` only for one-off host-side execution.
"""
    (campaign_dir / "README.md").write_text(text, encoding="utf-8")


def write_semantic_cards(campaign_dir: Path, records: list[dict[str, Any]], linux_ref: Path) -> None:
    semantics_dir = campaign_dir / "semantics"
    semantics_dir.mkdir(parents=True, exist_ok=True)
    by_semantic: dict[str, list[dict[str, Any]]] = {}
    semantic_defs = {item["id"]: item for item in SEMANTIC_DEFS}
    for record in records:
        by_semantic.setdefault(record["semantic"], []).append(record)
    for semantic_id, items in sorted(by_semantic.items()):
        semantic = semantic_defs.get(semantic_id) or semantic_for_case("", "")
        case_lines = "\n".join(f"- `{item['marker']}`: `{item['line']}`" for item in items)
        source_lines = []
        seen_sources: set[str] = set()
        for item in items:
            for source in item.get("test_sources", []):
                if source in seen_sources:
                    continue
                seen_sources.add(source)
                source_lines.append(f"- `{source}`")
        if not source_lines:
            source_lines.append("- Fill after reading the testcase source.")
        required_lines = "\n".join(f"- {line}" for line in semantic["required"])
        linux_lines = "\n".join(f"- `{path}`" for path in linux_ref_paths(linux_ref, semantic["linux_refs"]))
        kernel_lines = "\n".join(f"- `{path}`" for path in semantic["kernel_paths"])
        text = f"""# {semantic['title']}

## Cases

{case_lines}

## Test Sources

{chr(10).join(source_lines)}

## Linux References

{linux_lines}

## Required Semantics

{required_lines}

## Kernel Paths To Read

{kernel_lines}

## Implementation Notes

- Write the real subsystem behavior that satisfies this semantic bucket.
- Cross-check testcase expectations against Linux behavior before editing shared code.
- Use cheap builds, static inspection, and tiny crash probes during implementation.
- Defer full matrix replay until a substantial shared behavior pass is ready.
- Keep promotion evidence separate from canary or partial runs.
"""
        (semantics_dir / f"{semantic_id}.md").write_text(text, encoding="utf-8")


def write_implementation_template(campaign_dir: Path, records: list[dict[str, Any]]) -> None:
    semantic_ids = sorted({record["semantic"] for record in records})
    sections = []
    for semantic_id in semantic_ids:
        sections.append(
            f"""## {semantic_id}

Changed Files:
- Fill in touched kernel/framework files.

Behavior Implemented:
- Fill in the Linux-visible behavior implemented for this semantic bucket.

Candidate Coverage:
- Fill in cases this implementation is expected to cover.

Testcase Cross-Checks:
- Fill in testcase source files and checked expectations.

Linux Reference Cross-Checks:
- Fill in Linux reference files/functions and observed behavior.

Deferred Validation:
- Fill in candidate groups that should be replayed after this code-first pass.

Residual Risk:
- Fill in remaining risk or follow-up cases.
"""
        )
    text = "# Implementation Ledger\n\n" + "\n".join(sections)
    (campaign_dir / "implementation.md").write_text(text, encoding="utf-8")


def write_initial_taxonomy(campaign_dir: Path) -> None:
    text = """# Failure Taxonomy

Before replay, use this file as the static semantic map for unfinished work:

- Shared semantic buckets to implement.
- Cases expected to be covered by each bucket.
- Cases deferred because they need a different subsystem or are risky.

After a matrix run, `./scripts/lab campaign analyze <name>` replaces
this with observed pass/fail/panic/timeout buckets.
"""
    (campaign_dir / "taxonomy.md").write_text(text, encoding="utf-8")


def campaign_create_cmd(args: argparse.Namespace) -> None:
    ensure_dirs()
    name = args.name
    campaign_dir = campaign_path(name)
    if campaign_dir.exists() and not args.replace:
        die(f"campaign already exists: {campaign_dir}; pass --replace or choose another name")
    if campaign_dir.exists():
        shutil.rmtree(campaign_dir)
    campaign_dir.mkdir(parents=True)

    list_path = campaign_dir / "candidates.txt"
    generate_list(
        argparse.Namespace(
            inventory=args.inventory,
            arch=args.arch,
            libc=args.libc,
            mode=args.mode,
            case=args.case,
            runtest=args.runtest,
            include=args.include,
            exclude=args.exclude,
            shuffle=args.shuffle,
            seed=args.seed,
            offset=args.offset,
            limit=args.limit,
            name=name,
            output=str(list_path),
        )
    )
    inv: dict[str, Any] | None = None
    if args.inventory or INVENTORY_PATH.is_file():
        inv = load_inventory(args.inventory)
    source_root = Path(args.testsuite_source).expanduser() if args.testsuite_source else None
    linux_ref = Path(args.linux_ref).expanduser() if args.linux_ref else DEFAULT_LINUX_REF
    records = campaign_candidate_records(campaign_dir, inv, source_root)
    write_cases_jsonl(campaign_dir / "cases.jsonl", records)
    semantic_counts: dict[str, int] = {}
    for record in records:
        semantic_counts[record["semantic"]] = semantic_counts.get(record["semantic"], 0) + 1
    manifest = {
        "name": name,
        "created_at": iso_now(),
        "updated_at": iso_now(),
        "goal": args.goal,
        "risk": args.risk,
        "repo_root": str(REPO_ROOT),
        "inventory": str(Path(args.inventory).expanduser()) if args.inventory else str(INVENTORY_PATH),
        "linux_ref": str(linux_ref),
        "testsuite_source": str(source_root) if source_root else "",
        "candidate_list": "candidates.txt",
        "candidate_count": len(records),
        "semantic_counts": semantic_counts,
        "generation": {
            "mode": generation_mode_for(args),
            "arches": canonical_arches(args.arch),
            "libcs": canonical_libcs(args.libc),
            "runtest": split_csv(args.runtest),
            "include": split_csv(args.include),
            "exclude": split_csv(args.exclude),
            "limit": args.limit,
            "offset": args.offset,
            "shuffle": args.shuffle,
            "seed": args.seed,
        },
        "runs": [],
        "status": "created",
    }
    save_campaign(campaign_dir, manifest)
    write_campaign_readme(campaign_dir, manifest)
    write_semantic_cards(campaign_dir, records, linux_ref)
    write_implementation_template(campaign_dir, records)
    write_initial_taxonomy(campaign_dir)
    log(f"created campaign {name} with {len(records)} cases at {campaign_dir}")


def campaign_new_cmd(args: argparse.Namespace) -> None:
    runtests = expanded_runtests([*(args.suite or []), *(args.runtest or [])])
    create_args = argparse.Namespace(
        name=args.name,
        inventory=None,
        arch=["both"],
        libc=["both"],
        mode="unopened-runtest",
        case=None,
        runtest=runtests,
        include=None,
        exclude=None,
        shuffle=False,
        seed=1,
        offset=args.offset,
        limit=args.limit,
        goal=args.goal or "LTP semantic expansion campaign",
        risk=args.risk,
        testsuite_source=None,
        linux_ref=None,
        replace=args.replace,
    )
    campaign_create_cmd(create_args)


def default_campaign_run_namespace(name: str) -> argparse.Namespace:
    return argparse.Namespace(
        name=name,
        run_name=None,
        inventory=None,
        arch=["both"],
        libc=["both"],
        plan=None,
        replace=False,
        image=None,
        timeout=7000,
        parallel="arch",
        split_combos=False,
        jobs="auto",
        no_parallel=False,
        case_timeout=DEFAULT_CAMPAIGN_CASE_TIMEOUT,
        task_timeout=None,
        ltp_order="glibc-first",
        ltp_budget=None,
        glibc_budget=None,
        musl_budget=None,
        env=None,
        skip_kernel_build=True,
        rebuild_kernels=False,
        prepare_only=False,
        fail_fast=False,
    )


def campaign_quick_run_cmd(args: argparse.Namespace) -> None:
    run_args = default_campaign_run_namespace(args.name)
    if args.run_name:
        run_args.run_name = args.run_name
    if args.replace:
        run_args.replace = True
    if args.build:
        run_args.rebuild_kernels = True
        run_args.skip_kernel_build = False
    if args.prepare:
        run_args.prepare_only = True
    campaign_run_cmd(run_args)


def latest_campaign_run(manifest: dict[str, Any]) -> str | None:
    runs = manifest.get("runs") or []
    if not runs:
        return None
    return str(runs[-1].get("run_id") or "")


def campaign_run_args(args: argparse.Namespace, campaign_dir: Path, manifest: dict[str, Any], run_id: str) -> argparse.Namespace:
    generation = manifest.get("generation", {})
    return argparse.Namespace(
        inventory=args.inventory or manifest.get("inventory") or None,
        arch=args.arch or generation.get("arches") or ["both"],
        libc=args.libc or generation.get("libcs") or ["both"],
        mode="cases",
        case=None,
        runtest=None,
        include=None,
        exclude=None,
        shuffle=False,
        seed=1,
        offset=0,
        limit=None,
        test_list=str(campaign_dir / manifest.get("candidate_list", "candidates.txt")),
        plan=args.plan,
        name=run_id,
        replace=args.replace,
        image=args.image,
        timeout=args.timeout,
        parallel=args.parallel,
        split_combos=args.split_combos,
        jobs=args.jobs,
        no_parallel=args.no_parallel,
        case_timeout=args.case_timeout if args.case_timeout is not None else DEFAULT_CAMPAIGN_CASE_TIMEOUT,
        task_timeout=args.task_timeout,
        ltp_order=args.ltp_order,
        ltp_budget=args.ltp_budget,
        glibc_budget=args.glibc_budget,
        musl_budget=args.musl_budget,
        env=args.env,
        skip_kernel_build=not args.rebuild_kernels,
        rebuild_kernels=args.rebuild_kernels,
        prepare_only=args.prepare_only,
        fail_fast=args.fail_fast,
    )


def campaign_run_cmd(args: argparse.Namespace) -> None:
    campaign_dir, manifest = load_campaign(args.name)
    run_id = args.run_name or f"{manifest['name']}-run-{len(manifest.get('runs') or []) + 1:04d}"
    run_entry = {
        "run_id": run_id,
        "started_at": iso_now(),
        "status": "started",
        "path": str(RUN_DIR / run_id),
    }
    manifest.setdefault("runs", []).append(run_entry)
    manifest["status"] = "running"
    save_campaign(campaign_dir, manifest)
    try:
        run_experiment(campaign_run_args(args, campaign_dir, manifest, run_id))
    except SystemExit as exc:
        run_entry["ended_at"] = iso_now()
        run_entry["status"] = "failed"
        run_entry["exit"] = exc.code
        save_campaign(campaign_dir, manifest)
        raise
    run_entry["ended_at"] = iso_now()
    run_entry["status"] = "completed"
    manifest["status"] = "ran"
    save_campaign(campaign_dir, manifest)
    log(f"campaign {manifest['name']} recorded run {run_id}")


def campaign_review_cmd(args: argparse.Namespace) -> None:
    campaign_analyze_cmd(
        argparse.Namespace(
            name=args.name,
            run=args.run,
            latest=args.latest,
            require=args.require,
            allow_silent_pass=args.allow_silent_pass,
        )
    )
    campaign_promote_cmd(
        argparse.Namespace(
            name=args.name,
            run=args.run,
            require=args.require,
            base=None,
            output=None,
            allow_silent_pass=args.allow_silent_pass,
            dry_run=True,
            explain=True,
            show_missing=args.show_missing,
            status_matrix=args.status_matrix,
            apply_root=False,
        )
    )


def campaign_apply_cmd(args: argparse.Namespace) -> None:
    campaign_promote_cmd(
        argparse.Namespace(
            name=args.name,
            run=args.run,
            require=args.require,
            base=None,
            output=None,
            allow_silent_pass=args.allow_silent_pass,
            dry_run=args.dry_run,
            explain=args.explain,
            show_missing=args.show_missing,
            status_matrix=args.status_matrix,
            apply_root=not args.dry_run,
        )
    )


def campaign_status_cmd(args: argparse.Namespace) -> None:
    campaign_dir, manifest = load_campaign(args.name)
    print(f"campaign: {manifest.get('name')}")
    print(f"path: {campaign_dir}")
    print(f"status: {manifest.get('status')}")
    print(f"cases: {manifest.get('candidate_count', 0)}")
    print(f"goal: {manifest.get('goal', '')}")
    print("semantics:")
    for semantic, count in sorted((manifest.get("semantic_counts") or {}).items()):
        print(f"  {semantic}: {count}")
    print("runs:")
    for run in manifest.get("runs") or []:
        print(f"  {run.get('run_id')}: {run.get('status')} {run.get('path')}")
    analysis_path = campaign_dir / "analysis.json"
    if analysis_path.is_file():
        analysis = read_json(analysis_path)
        print(
            "analysis: "
            f"promotable={analysis.get('promotable_count', 0)} "
            f"blocked={analysis.get('blocked_count', 0)} "
            f"runs={','.join(analysis.get('runs', []))}"
        )


def campaign_list_cmd(args: argparse.Namespace) -> None:
    ensure_dirs()
    campaigns = children_for_cleanup(CAMPAIGN_DIR, dirs_only=True)
    for path in sorted(campaigns, key=newest_mtime, reverse=True):
        manifest_path = campaign_manifest_path(path)
        if not manifest_path.is_file():
            continue
        manifest = read_json(manifest_path)
        print(
            f"{manifest.get('name', path.name)} "
            f"status={manifest.get('status', '')} "
            f"cases={manifest.get('candidate_count', 0)} "
            f"runs={len(manifest.get('runs') or [])} "
            f"updated={manifest.get('updated_at', '')}"
        )


def campaign_required_combos(args: argparse.Namespace) -> list[str]:
    return normalize_required_combos(getattr(args, "require", None))


def normalize_recorded_state_path(path: Path) -> Path:
    if not path.is_absolute() or path.exists():
        return path

    parts = path.parts
    marker = (".state", "ltp-lab")
    for index in range(len(parts) - len(marker) + 1):
        if parts[index : index + len(marker)] == marker:
            remapped = STATE_DIR.joinpath(*parts[index + len(marker) :])
            if remapped.exists():
                return remapped

    return path


def campaign_run_dirs(manifest: dict[str, Any], selected: list[str] | None = None) -> list[Path]:
    wanted = set(selected or [])
    run_dirs: list[Path] = []
    for run in manifest.get("runs") or []:
        run_id = str(run.get("run_id") or "")
        if wanted and run_id not in wanted:
            continue
        path = normalize_recorded_state_path(Path(run.get("path") or RUN_DIR / run_id).expanduser())
        if path.is_dir():
            run_dirs.append(path)
    return run_dirs


def resolve_lab_run_path(run: str) -> Path:
    path = Path(run).expanduser()
    if not path.is_absolute() and path.parent == Path("."):
        path = RUN_DIR / run
    return normalize_recorded_state_path(path)


def campaign_attach_run_cmd(args: argparse.Namespace) -> None:
    campaign_dir, manifest = load_campaign(args.name)
    runs = manifest.setdefault("runs", [])
    existing = {str(run.get("run_id") or "") for run in runs if isinstance(run, dict)}
    attached = 0
    for item in split_csv(args.run):
        run_path = resolve_lab_run_path(item)
        if not run_path.is_dir():
            die(f"run directory not found: {run_path}")
        manifest_path = run_path / "manifest.json"
        run_manifest: dict[str, Any] = {}
        if manifest_path.is_file():
            try:
                loaded = read_json(manifest_path)
                if isinstance(loaded, dict):
                    run_manifest = loaded
            except json.JSONDecodeError:
                run_manifest = {}
        run_id = str(run_manifest.get("run_id") or run_path.name)
        if run_id in existing:
            log(f"campaign {manifest['name']} already has run {run_id}")
            continue
        runs.append(
            {
                "run_id": run_id,
                "attached_at": iso_now(),
                "status": args.status,
                "path": str(run_path),
                "note": args.note or "",
            }
        )
        existing.add(run_id)
        attached += 1
    if attached:
        manifest["status"] = "ran"
        save_campaign(campaign_dir, manifest)
    log(f"campaign {manifest['name']} attached {attached} run(s)")


def failure_bucket_for(case: str, statuses: dict[str, str], semantic: str) -> str:
    values = set(statuses.values())
    if "panic" in values:
        return "panic-frontier"
    if "timeout" in values:
        return "timeout-or-heavy-io"
    if "missing" in values or "incomplete" in values:
        return "missing-or-incomplete-evidence"
    if any(value in {"fail", "nonzero"} for value in values):
        return semantic or "linux-abi-mismatch"
    if values <= {"pass", "silent-pass"}:
        return "promotable-or-silent"
    return "unknown"


def write_campaign_analysis(
    campaign_dir: Path,
    manifest: dict[str, Any],
    run_dirs: list[Path],
    required: list[str],
    *,
    allow_silent_pass: bool,
    persist: bool = True,
) -> dict[str, Any]:
    candidate_list = campaign_dir / manifest.get("candidate_list", "candidates.txt")
    candidates = parse_test_list(candidate_list)
    candidate_records = {record["marker"]: record for record in read_cases_jsonl(campaign_dir / "cases.jsonl")}
    passing_statuses = {"pass"}
    if allow_silent_pass:
        passing_statuses.add("silent-pass")
    evidence = collect_case_evidence(run_dirs, required, passing_statuses)
    promoted = promotable_cases(evidence, required, passing_statuses)
    line_by_case = load_run_list_lines(run_dirs)
    for item in candidates:
        line_by_case[item["marker"]] = item["line"].strip()
    cases: list[dict[str, Any]] = []
    bucket_counts: dict[str, int] = {}
    status_counts: dict[str, int] = {}
    promotable_lines: list[str] = []
    for item in candidates:
        marker = item["marker"]
        statuses = {combo: case_combo_status(evidence, marker, combo) for combo in required}
        for status in statuses.values():
            status_counts[status] = status_counts.get(status, 0) + 1
        semantic = (candidate_records.get(marker) or {}).get("semantic", "")
        bucket = failure_bucket_for(marker, statuses, semantic)
        bucket_counts[bucket] = bucket_counts.get(bucket, 0) + 1
        is_promotable = marker in promoted
        if is_promotable:
            promotable_lines.append(line_by_case.get(marker, marker))
        cases.append(
            {
                "case": marker,
                "line": line_by_case.get(marker, marker),
                "semantic": semantic,
                "statuses": statuses,
                "bucket": bucket,
                "promotable": is_promotable,
            }
        )
    analysis = {
        "campaign": manifest["name"],
        "generated_at": iso_now(),
        "runs": [path.name for path in run_dirs],
        "required": required,
        "allow_silent_pass": allow_silent_pass,
        "candidate_count": len(candidates),
        "promotable_count": len(promotable_lines),
        "blocked_count": len(candidates) - len(promotable_lines),
        "status_counts": status_counts,
        "bucket_counts": bucket_counts,
        "cases": cases,
    }
    if persist:
        write_json(campaign_dir / "analysis.json", analysis)
        (campaign_dir / "promotable.txt").write_text(
            "\n".join(promotable_lines) + ("\n" if promotable_lines else ""),
            encoding="utf-8",
        )
        write_campaign_taxonomy(campaign_dir, analysis)
    return analysis


def write_campaign_taxonomy(campaign_dir: Path, analysis: dict[str, Any]) -> None:
    grouped: dict[str, list[dict[str, Any]]] = {}
    for case in analysis.get("cases", []):
        grouped.setdefault(case.get("bucket", "unknown"), []).append(case)
    lines = [
        "# Failure Taxonomy",
        "",
        f"Campaign: `{analysis.get('campaign')}`",
        f"Runs: `{', '.join(analysis.get('runs', []))}`",
        f"Candidates: {analysis.get('candidate_count', 0)}",
        f"Promotable: {analysis.get('promotable_count', 0)}",
        f"Blocked: {analysis.get('blocked_count', 0)}",
        "",
        "## Buckets",
        "",
    ]
    for bucket, items in sorted(grouped.items(), key=lambda pair: (-len(pair[1]), pair[0])):
        lines.append(f"### {bucket} ({len(items)})")
        lines.append("")
        for item in items[:200]:
            statuses = " ".join(f"{combo}={status}" for combo, status in item.get("statuses", {}).items())
            lines.append(f"- `{item.get('case')}` [{item.get('semantic', '')}] {statuses}")
        if len(items) > 200:
            lines.append(f"- ... {len(items) - 200} more")
        lines.append("")
    (campaign_dir / "taxonomy.md").write_text("\n".join(lines), encoding="utf-8")


def campaign_analyze_cmd(args: argparse.Namespace) -> None:
    campaign_dir, manifest = load_campaign(args.name)
    selected = split_csv(args.run)
    if not selected and args.latest:
        latest = latest_campaign_run(manifest)
        selected = [latest] if latest else []
    run_dirs = campaign_run_dirs(manifest, selected or None)
    if not run_dirs:
        die("campaign has no matching run directories to analyze")
    analysis = write_campaign_analysis(
        campaign_dir,
        manifest,
        run_dirs,
        campaign_required_combos(args),
        allow_silent_pass=args.allow_silent_pass,
    )
    manifest["status"] = "analyzed"
    manifest["last_analysis"] = "analysis.json"
    save_campaign(campaign_dir, manifest)
    log(
        f"analyzed campaign {manifest['name']}: "
        f"promotable={analysis['promotable_count']} blocked={analysis['blocked_count']}"
    )


def campaign_promote_cmd(args: argparse.Namespace) -> None:
    campaign_dir, manifest = load_campaign(args.name)
    selected = split_csv(args.run)
    run_dirs = campaign_run_dirs(manifest, selected or None)
    if not run_dirs:
        die("campaign has no matching run directories to promote from")
    output = Path(args.output).expanduser() if args.output else campaign_dir / "promoted-ltp_test.txt"
    promote_cmd(
        argparse.Namespace(
            run_dir=[str(path) for path in run_dirs],
            require=args.require,
            test_list=str(campaign_dir / manifest.get("candidate_list", "candidates.txt")),
            base=args.base,
            output=str(output),
            allow_silent_pass=args.allow_silent_pass,
            dry_run=args.dry_run,
            explain=args.explain,
            show_missing=args.show_missing,
            status_matrix=args.status_matrix,
        )
    )
    if args.apply_root and not args.dry_run:
        shutil.copyfile(output, DEFAULT_TEST_LIST)
        log(f"applied promoted list to {DEFAULT_TEST_LIST}")
    if not args.dry_run:
        manifest["last_promoted_list"] = str(output)
        save_campaign(campaign_dir, manifest)


def cleanup_heavy_run_artifacts(run_dirs: list[Path], *, dry_run: bool) -> list[str]:
    targets: list[Path] = []
    for run_dir in run_dirs:
        for image in run_dir.rglob("support.img"):
            add_cleanup_target(targets, image)
        for workdir in run_dir.rglob("work"):
            if workdir.is_dir():
                add_cleanup_target(targets, workdir)
    removed: list[str] = []
    for target in collapse_cleanup_targets(targets):
        if not target.exists():
            continue
        removed.append(str(target))
        if dry_run:
            print(f"{target} [{'dir' if target.is_dir() else 'file'}, {format_bytes(path_size(target))}]")
        elif target.is_dir():
            shutil.rmtree(target)
            log(f"removed {target}")
        else:
            target.unlink()
            log(f"removed {target}")
    return removed


def campaign_finish_cmd(args: argparse.Namespace) -> None:
    campaign_dir, manifest = load_campaign(args.name)
    selected = split_csv(args.run)
    run_dirs = campaign_run_dirs(manifest, selected or None)
    if run_dirs:
        analysis = write_campaign_analysis(
            campaign_dir,
            manifest,
            run_dirs,
            campaign_required_combos(args),
            allow_silent_pass=args.allow_silent_pass,
            persist=not args.dry_run,
        )
    else:
        analysis = {}
    cleaned: list[str] = []
    if not args.no_clean:
        cleaned = cleanup_heavy_run_artifacts(run_dirs, dry_run=args.dry_run)
    cleanup_label = "Heavy artifacts selected for cleanup" if args.dry_run else "Heavy artifacts cleaned"
    final_lines = [
        "# Campaign Finish",
        "",
        f"Campaign: `{manifest.get('name')}`",
        f"Finished: `{iso_now()}`",
        f"Runs: `{', '.join(path.name for path in run_dirs)}`",
        f"Promotable: {analysis.get('promotable_count', 0)}",
        f"Blocked: {analysis.get('blocked_count', 0)}",
        f"{cleanup_label}: {len(cleaned)}",
        "",
        "Evidence files retained: `console.log`, `cases.jsonl`, `summary.json`, `combined-summary.json`, `analysis.json`, `taxonomy.md`.",
    ]
    if args.dry_run:
        print("\n".join(["# Campaign Finish Dry Run", *final_lines[1:], "", "No campaign metadata was updated."]))
        return
    (campaign_dir / "finish.md").write_text("\n".join(final_lines) + "\n", encoding="utf-8")
    manifest["status"] = "finished"
    manifest["finished_at"] = iso_now()
    manifest["last_analysis"] = "analysis.json" if analysis else manifest.get("last_analysis", "")
    manifest["cleaned_heavy_artifacts"] = cleaned
    save_campaign(campaign_dir, manifest)
    log(f"finished campaign {manifest['name']}")


def campaign_clean_cmd(args: argparse.Namespace) -> None:
    campaign_dir, manifest = load_campaign(args.name)
    targets: list[Path] = []
    if args.heavy:
        cleanup_heavy_run_artifacts(campaign_run_dirs(manifest, split_csv(args.run) or None), dry_run=args.dry_run)
    if args.runs:
        for run_dir in campaign_run_dirs(manifest, split_csv(args.run) or None):
            add_cleanup_target(targets, run_dir)
    if args.campaign:
        add_cleanup_target(targets, campaign_dir)
    for target in collapse_cleanup_targets(targets):
        if not target.exists():
            continue
        if args.dry_run:
            print(f"{target} [{'dir' if target.is_dir() else 'file'}, {format_bytes(path_size(target))}]")
        elif target.is_dir():
            shutil.rmtree(target)
            log(f"removed {target}")
        else:
            target.unlink()
            log(f"removed {target}")


def bootstrap_cmd(args: argparse.Namespace) -> None:
    ensure_dirs()
    checks = {
        "repo": REPO_ROOT.is_dir(),
        "debugfs": require_tool("debugfs"),
        "xz": require_tool("xz"),
        "mke2fs": require_tool("mke2fs"),
        "qemu-rv": require_tool("qemu-system-riscv64"),
        "qemu-la": require_tool("qemu-system-loongarch64"),
        "cc-rv": require_tool("riscv64-linux-musl-gcc") or require_tool("riscv64-linux-gnu-gcc"),
        "cc-la": require_tool("loongarch64-linux-musl-gcc") or require_tool("loongarch64-linux-gnu-gcc"),
        "libgcc-rv": compiler_libgcc_available("riscv64-linux-musl-gcc", "riscv64-linux-gnu-gcc"),
        "libgcc-la": compiler_libgcc_available("loongarch64-linux-musl-gcc", "loongarch64-linux-gnu-gcc"),
        "support-builder": (REPO_ROOT / "scripts" / "build-oscomp-support-disk.sh").is_file(),
    }
    for arch in ARCHES:
        checks[f"official-image-{arch}"] = find_official_image(arch) is not None
    for name, ok in checks.items():
        print(f"{name}: {'ok' if ok else 'missing'}")
    if args.fetch and not args.linux_ref and not args.testsuits_ref:
        args.linux_ref = str(DEFAULT_LINUX_REF)
        args.testsuits_ref = str(DEFAULT_TESTSUITE_REF)
    if args.linux_ref:
        target = Path(args.linux_ref).expanduser()
        if target.exists():
            print(f"linux-ref: exists {target}")
        else:
            if not args.fetch:
                print(f"linux-ref: missing {target} (pass --fetch to clone)")
            else:
                run_cmd(["git", "clone", "--depth=1", "https://github.com/torvalds/linux.git", str(target)], capture=False)
    if args.testsuits_ref:
        target = Path(args.testsuits_ref).expanduser()
        if target.exists():
            print(f"testsuits-ref: exists {target}")
        else:
            if not args.fetch:
                print(f"testsuits-ref: missing {target} (pass --fetch to clone)")
            else:
                run_cmd(
                    [
                        "git",
                        "clone",
                        "--depth=1",
                        "--branch",
                        "pre-2025",
                        "https://github.com/oscomp/testsuits-for-oskernel.git",
                        str(target),
                    ],
                    capture=False,
                )


def run_failed(run_dir: Path) -> bool:
    combined = run_dir / "combined-summary.json"
    if combined.is_file():
        try:
            data = read_json(combined)
        except json.JSONDecodeError:
            return True
        failed_arches = data.get("failed_arches") or {}
        if failed_arches:
            return True
        if data.get("failed_tasks") or data.get("cancelled_tasks"):
            return True
        for code in (data.get("exit_codes") or {}).values():
            try:
                if int(code) != 0:
                    return True
            except (TypeError, ValueError):
                return True
        for code in (data.get("task_exit_codes") or {}).values():
            if code is None:
                continue
            try:
                if int(code) != 0:
                    return True
            except (TypeError, ValueError):
                return True
    for arch in ARCHES:
        exit_code = run_dir / arch / "exit_code.txt"
        if exit_code.is_file():
            try:
                if int(exit_code.read_text(encoding="utf-8").strip()) != 0:
                    return True
            except ValueError:
                return True
    return False


def run_empty(run_dir: Path) -> bool:
    summaries = [run_dir / arch / "summary.json" for arch in ARCHES]
    present = [path for path in summaries if path.is_file()]
    if not present:
        return False
    cases = 0
    for path in present:
        try:
            cases += int(read_json(path).get("cases", 0))
        except (json.JSONDecodeError, TypeError, ValueError):
            return False
    return cases == 0


def children_for_cleanup(base: Path, *, dirs_only: bool = False) -> list[Path]:
    if not base.is_dir():
        return []
    result = []
    for item in base.iterdir():
        if dirs_only and not item.is_dir():
            continue
        result.append(item)
    return result


def apply_time_filters(paths: list[Path], *, older_than: str | None, keep: int | None) -> list[Path]:
    selected = list(paths)
    if keep is not None:
        ordered = sorted(selected, key=newest_mtime, reverse=True)
        selected = ordered[keep:]
    if older_than:
        cutoff = _dt.datetime.now().timestamp() - parse_duration(older_than)
        selected = [path for path in selected if newest_mtime(path) < cutoff]
    return selected


def add_cleanup_target(targets: list[Path], target: Path) -> None:
    if target not in targets:
        targets.append(target)


def collapse_cleanup_targets(targets: list[Path]) -> list[Path]:
    ordered = sorted(targets, key=lambda path: len(path.parts))
    collapsed: list[Path] = []
    for target in ordered:
        try:
            resolved = target.resolve()
        except OSError:
            resolved = target.absolute()
        covered = False
        for parent in collapsed:
            try:
                parent_resolved = parent.resolve()
            except OSError:
                parent_resolved = parent.absolute()
            if resolved == parent_resolved:
                covered = True
                break
            try:
                if resolved.is_relative_to(parent_resolved):
                    covered = True
                    break
            except ValueError:
                pass
        if not covered:
            collapsed.append(target)
    return collapsed


def baseline_heavy_artifacts() -> list[Path]:
    if not BASELINE_DIR.is_dir():
        return []
    patterns = ("sdcard-*.img", "disk*.img")
    artifacts: list[Path] = []
    for pattern in patterns:
        artifacts.extend(path for path in BASELINE_DIR.rglob(pattern) if path.is_file())
    return artifacts


def clean_cmd(args: argparse.Namespace) -> None:
    targets: list[Path] = []
    for preset in split_words_or_csv(getattr(args, "preset", None)):
        if preset in ("trim", "daily"):
            args.trim = True
        elif preset in ("gen", "generated"):
            args.generated = True
        elif preset == "runs":
            args.runs = True
        elif preset == "cache":
            args.cache = True
        elif preset == "refs":
            args.refs = True
        elif preset == "lab":
            args.lab = True
        elif preset in ("root", "legacy-root"):
            args.legacy_root = True
        elif preset == "smoke":
            args.smoke = True
        elif preset == "all":
            args.all = True
        else:
            die(f"unknown clean preset: {preset}")
    requested = any(
        (
            args.trim,
            args.lab,
            args.generated,
            args.runs,
            bool(args.run),
            args.failed_runs,
            args.empty_runs,
            args.lists,
            args.plans,
            args.campaigns,
            bool(args.campaign),
            args.inventory,
            args.images,
            args.cache,
            args.refs,
            args.support_images,
            args.workdirs,
            args.baseline_heavy,
            args.smoke,
            args.legacy_root,
            args.all,
        )
    )
    if args.trim:
        args.failed_runs = True
        args.empty_runs = True
        args.support_images = True
        args.workdirs = True
        args.baseline_heavy = True
        args.smoke = True
        args.legacy_root = True
    if args.lab or args.all:
        add_cleanup_target(targets, STATE_DIR)
    if args.legacy_root or args.all:
        for name in ("rv_.out", "la_.out", "score.txt"):
            add_cleanup_target(targets, REPO_ROOT / name)
    if args.inventory or args.all:
        add_cleanup_target(targets, INVENTORY_PATH)
    if args.generated or args.all:
        args.runs = True
        args.lists = True
        args.plans = True
    if args.cache or args.all:
        args.images = True
    if args.all:
        args.campaigns = True

    if args.runs:
        for run_dir in apply_time_filters(
            children_for_cleanup(RUN_DIR, dirs_only=True),
            older_than=args.older_than,
            keep=args.keep_runs,
        ):
            add_cleanup_target(targets, run_dir)
    for run_name in split_csv(args.run):
        run_path = Path(run_name).expanduser()
        if not run_path.is_absolute():
            run_path = RUN_DIR / run_name
        add_cleanup_target(targets, run_path)
    if args.failed_runs:
        for run_dir in children_for_cleanup(RUN_DIR, dirs_only=True):
            if run_failed(run_dir):
                add_cleanup_target(targets, run_dir)
    if args.empty_runs:
        for run_dir in children_for_cleanup(RUN_DIR, dirs_only=True):
            if run_empty(run_dir):
                add_cleanup_target(targets, run_dir)
    if args.lists:
        for item in apply_time_filters(children_for_cleanup(LIST_DIR), older_than=args.older_than, keep=None):
            add_cleanup_target(targets, item)
    if args.plans:
        for item in apply_time_filters(children_for_cleanup(PLAN_DIR), older_than=args.older_than, keep=None):
            add_cleanup_target(targets, item)
    if args.campaigns:
        for item in apply_time_filters(children_for_cleanup(CAMPAIGN_DIR, dirs_only=True), older_than=args.older_than, keep=None):
            add_cleanup_target(targets, item)
    for campaign_name in split_csv(args.campaign):
        add_cleanup_target(targets, campaign_path(campaign_name))
    if args.images:
        for item in apply_time_filters(children_for_cleanup(IMAGE_CACHE_DIR), older_than=args.older_than, keep=None):
            add_cleanup_target(targets, item)
    if args.refs:
        for item in children_for_cleanup(REF_DIR):
            add_cleanup_target(targets, item)
    if args.support_images:
        for run_dir in children_for_cleanup(RUN_DIR, dirs_only=True):
            images = [run_dir / "support.img"]
            tasks_dir = run_dir / "tasks"
            if tasks_dir.is_dir():
                images.extend(sorted(tasks_dir.glob("*/support.img")))
            for image in images:
                if not image.exists():
                    continue
                if args.older_than:
                    cutoff = _dt.datetime.now().timestamp() - parse_duration(args.older_than)
                    if newest_mtime(image) >= cutoff:
                        continue
                add_cleanup_target(targets, image)
    if args.workdirs:
        for run_dir in children_for_cleanup(RUN_DIR, dirs_only=True):
            workdirs = [run_dir / arch / "work" for arch in ARCHES]
            tasks_dir = run_dir / "tasks"
            if tasks_dir.is_dir():
                workdirs.extend(sorted(tasks_dir.glob("*/work")))
            for workdir in workdirs:
                if not workdir.exists():
                    continue
                if args.older_than:
                    cutoff = _dt.datetime.now().timestamp() - parse_duration(args.older_than)
                    if newest_mtime(workdir) >= cutoff:
                        continue
                add_cleanup_target(targets, workdir)
    if args.baseline_heavy:
        for artifact in baseline_heavy_artifacts():
            if args.older_than:
                cutoff = _dt.datetime.now().timestamp() - parse_duration(args.older_than)
                if newest_mtime(artifact) >= cutoff:
                    continue
            add_cleanup_target(targets, artifact)
    if args.smoke:
        for base in (LIST_DIR, PLAN_DIR, RUN_DIR, CAMPAIGN_DIR):
            for item in children_for_cleanup(base):
                if "smoke" in item.name.lower():
                    add_cleanup_target(targets, item)

    if not targets and not requested:
        die(
            "clean requires a target such as --trim, --generated, --runs, --run NAME, "
            "--failed-runs, --empty-runs, --campaigns, --cache, --lab, --legacy-root, or --all"
        )
    if not targets:
        log("nothing to clean")
        return
    targets = collapse_cleanup_targets(targets)
    for target in targets:
        if not target.exists():
            continue
        if args.dry_run:
            kind = "dir" if target.is_dir() else "file"
            print(f"{target} [{kind}, {format_bytes(path_size(target))}]")
        elif target.is_dir():
            shutil.rmtree(target)
            log(f"removed {target}")
        else:
            target.unlink()
            log(f"removed {target}")


def audit_cmd(args: argparse.Namespace) -> None:
    checks: list[tuple[str, str]] = []
    checks.append(("repo", str(REPO_ROOT)))
    checks.append(("git_dirty", "yes" if run_cmd(["git", "status", "--short"], capture=True, check=False).stdout.strip() else "no"))
    checks.append(("default_ltp_list", "ok" if DEFAULT_TEST_LIST.is_file() else "missing"))
    artifacts = [name for name in ("kernel-rv", "kernel-la", "disk.img", "disk-la.img") if (REPO_ROOT / name).is_file()]
    checks.append(("evaluator_artifacts", ",".join(artifacts) if artifacts else "missing"))
    for arch in ARCHES:
        image = find_official_image(arch)
        checks.append((f"official_image_{arch}", str(image) if image else "missing"))
    checks.append(("inventory", str(INVENTORY_PATH) if INVENTORY_PATH.is_file() else "missing"))
    checks.append(("lab_state", str(STATE_DIR) if STATE_DIR.exists() else "missing"))
    if STATE_DIR.exists():
        checks.append(("lab_state_size", format_bytes(path_size(STATE_DIR))))
        checks.append(("lab_runs", str(len(children_for_cleanup(RUN_DIR, dirs_only=True)))))
        checks.append(("lab_lists", str(len(children_for_cleanup(LIST_DIR)))))
        checks.append(("lab_plans", str(len(children_for_cleanup(PLAN_DIR)))))
        checks.append(("lab_campaigns", str(len(children_for_cleanup(CAMPAIGN_DIR, dirs_only=True)))))
        checks.append(("lab_images", str(len(children_for_cleanup(IMAGE_CACHE_DIR)))))
    baseline_heavy = baseline_heavy_artifacts()
    checks.append(("baseline_heavy_artifacts", str(len(baseline_heavy)) if baseline_heavy else "absent"))
    if baseline_heavy:
        checks.append(("baseline_heavy_size", format_bytes(sum(path_size(path) for path in baseline_heavy))))
    build_state = [path.name for path in BUILD_STATE_DIRS if path.exists()]
    checks.append(("build_state", ",".join(build_state) if build_state else "absent"))
    checks.append(("old_ltp_count_state", "present" if (REPO_ROOT / ".state" / "ltp-count-current").exists() else "absent"))
    legacy_root = [name for name in ("rv_.out", "la_.out", "score.txt") if (REPO_ROOT / name).exists()]
    checks.append(("legacy_root_outputs", ",".join(legacy_root) if legacy_root else "absent"))
    stale_docs = [str(path) for path in (REPO_ROOT / "docs").glob("*.md") if path.name in {"x11.md"}]
    checks.append(("stale_docs", ",".join(stale_docs) if stale_docs else "absent"))
    smoke_state: list[str] = []
    if STATE_DIR.exists():
        for base in (LIST_DIR, PLAN_DIR, RUN_DIR, CAMPAIGN_DIR):
            if not base.exists():
                continue
            for item in base.rglob("*"):
                if "smoke" in item.name.lower():
                    smoke_state.append(str(item))
                    if len(smoke_state) >= 5:
                        break
            if len(smoke_state) >= 5:
                break
    checks.append(("smoke_state", ",".join(smoke_state) if smoke_state else "absent"))
    if args.json:
        print(json.dumps(dict(checks), indent=2, sort_keys=True))
        return
    for name, value in checks:
        print(f"{name}: {value}")


def add_common_generation_args(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--inventory", help="inventory JSON path")
    parser.add_argument("--arch", action="append", choices=["rv", "la", "riscv64", "loongarch64", "both"], help="arch set")
    parser.add_argument("--libc", action="append", choices=["glibc", "musl", "both"], help="libc set")
    parser.add_argument("--mode", default="unopened-runtest", choices=["current", "cases", "all-bins", "runtest", "unopened-runtest"])
    parser.add_argument("--case", action="append", help="case line or comma-separated case lines")
    parser.add_argument("--runtest", action="append", help="runtest file filter")
    parser.add_argument("--include", action="append", help="marker glob include filter")
    parser.add_argument("--exclude", action="append", help="marker glob exclude filter")
    parser.add_argument("--limit", type=int)
    parser.add_argument("--offset", type=int, default=0)
    parser.add_argument("--shuffle", action="store_true")
    parser.add_argument("--seed", type=int, default=1)


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="cmd", required=True)

    p = sub.add_parser("audit", help="summarize repository lab state and stale local artifacts")
    p.add_argument("--json", action="store_true")
    p.set_defaults(func=audit_cmd)

    p = sub.add_parser("bootstrap", help="check or prepare local lab dependencies")
    p.add_argument("--linux-ref", nargs="?", const=str(REF_DIR / "linux"), help="optional shallow Linux reference checkout")
    p.add_argument("--testsuits-ref", nargs="?", const=str(REF_DIR / "testsuits-for-oskernel"), help="optional testsuits reference checkout")
    p.add_argument("--fetch", action="store_true", help="clone missing optional references")
    p.set_defaults(func=bootstrap_cmd)

    p = sub.add_parser("inventory", help="index official images, runtest files, and current LTP list")
    p.add_argument("--image-root", action="append", help="official image root, comma-separated or repeated")
    p.add_argument("--testsuite-source", help="testsuits-for-oskernel source checkout")
    p.add_argument("--current-list", help="current LTP list")
    p.add_argument("--output", help="inventory output path")
    p.add_argument("--refresh-images", action="store_true", help="re-decompress compressed official images")
    p.set_defaults(func=lambda args: print_inventory_summary(build_inventory(args)))

    p = sub.add_parser("summary", help="print inventory summary")
    p.add_argument("--inventory", help="inventory JSON path")
    p.set_defaults(func=lambda args: print_inventory_summary(load_inventory(args.inventory)))

    p = sub.add_parser("new", help="create a default campaign: unopened all-four batch, 120 cases")
    p.add_argument("name")
    p.add_argument("suite", nargs="*", help="runtest names or presets, e.g. fs, proc, mm, ipc, tty, net")
    p.add_argument("-n", "--limit", type=int, default=DEFAULT_CAMPAIGN_LIMIT)
    p.add_argument("-o", "--offset", type=int, default=0)
    p.add_argument("-g", "--goal")
    p.add_argument("--risk", choices=["low", "medium", "high"], default="medium")
    p.add_argument("--runtest", action="append", help="extra runtest names or presets")
    p.add_argument("--replace", action="store_true")
    p.set_defaults(func=campaign_new_cmd)

    p = sub.add_parser("run", help="run campaign with default all-four matrix")
    p.add_argument("name")
    p.add_argument("--run-name")
    p.add_argument("--replace", action="store_true")
    p.add_argument("--build", action="store_true", help="build kernels before replay")
    p.add_argument("--prepare", action="store_true", help="prepare list/plan/support image only")
    p.set_defaults(func=campaign_quick_run_cmd)

    p = sub.add_parser("review", help="analyze campaign evidence and print dry-run promotion status")
    p.add_argument("name")
    p.add_argument("--run", action="append", help="run id to review, comma-separated or repeated")
    p.add_argument("--latest", action="store_true", help="review only the latest recorded run")
    p.add_argument("--require", action="append", help="required selector, default all four")
    p.add_argument("--allow-silent-pass", action="store_true")
    p.add_argument("--show-missing", action="store_true")
    p.add_argument("--status-matrix", action="store_true")
    p.set_defaults(func=campaign_review_cmd)

    p = sub.add_parser("apply", help="apply all-four campaign promotions to ltp_test.txt")
    p.add_argument("name")
    p.add_argument("--run", action="append", help="run id to promote from, comma-separated or repeated")
    p.add_argument("--require", action="append", help="required selector, default all four")
    p.add_argument("--allow-silent-pass", action="store_true")
    p.add_argument("--dry-run", action="store_true")
    p.add_argument("--explain", action="store_true")
    p.add_argument("--show-missing", action="store_true")
    p.add_argument("--status-matrix", action="store_true")
    p.set_defaults(func=campaign_apply_cmd)

    p = sub.add_parser("done", help="finish campaign and clean heavy run artifacts")
    p.add_argument("name")
    p.add_argument("--run", action="append", help="run id to include, comma-separated or repeated")
    p.add_argument("--require", action="append", help="required selector, default all four")
    p.add_argument("--allow-silent-pass", action="store_true")
    p.add_argument("--no-clean", action="store_true")
    p.add_argument("--dry-run", action="store_true")
    p.set_defaults(func=campaign_finish_cmd)

    p = sub.add_parser("generate", help="generate an LTP test list")
    add_common_generation_args(p)
    p.add_argument("--name", help="list name under .state/ltp-lab/lists")
    p.add_argument("--output", help="output list path")
    p.set_defaults(func=lambda args: generate_list(args))

    p = sub.add_parser("plan", help="generate a focused evaluator plan")
    p.add_argument("--libc", action="append", choices=["glibc", "musl", "both"], help="libc roots")
    p.add_argument("--group", action="append", help="group name, default ltp")
    p.add_argument("--ltp-order", choices=["glibc-first", "musl-first"], default="glibc-first")
    p.add_argument("--name", help="plan name under .state/ltp-lab/plans")
    p.add_argument("--output", help="output plan path")
    p.set_defaults(func=lambda args: write_plan(args))

    p = sub.add_parser("replay", help="low-level focused replay with explicit list/plan controls")
    add_common_generation_args(p)
    p.add_argument("--test-list", help="existing LTP list path")
    p.add_argument("--plan", help="existing plan path")
    p.add_argument("--name", help="run id")
    p.add_argument("--replace", action="store_true")
    p.add_argument("--image", help="official image override, only for single-arch runs")
    p.add_argument("--timeout", type=int, default=7000)
    p.add_argument("--parallel", choices=["arch", "combo", "serial"], default="arch", help="matrix execution mode, default arch")
    p.add_argument("--split-combos", action="store_true", help="run each selected arch/libc combo as its own task")
    p.add_argument("--jobs", default="auto", help="parallel task slots: auto or a positive integer")
    p.add_argument("--no-parallel", action="store_true", help="equivalent to --parallel serial --jobs 1")
    p.add_argument("--case-timeout", type=int, help="per-LTP-case timeout inside the guest; 0 disables")
    p.add_argument("--task-timeout", type=int, help="per-QEMU-task timeout; defaults to --timeout")
    p.add_argument("--ltp-order", choices=["glibc-first", "musl-first"], default="glibc-first")
    p.add_argument("--ltp-budget", type=int)
    p.add_argument("--glibc-budget", type=int)
    p.add_argument("--musl-budget", type=int)
    p.add_argument("--env", action="append", help="guest env KEY=VALUE, comma-separated or repeated")
    p.add_argument("--skip-kernel-build", action="store_true")
    p.add_argument("--rebuild-kernels", action="store_true")
    p.add_argument("--prepare-only", action="store_true", help="build list, plan, and support image without QEMU replay")
    p.add_argument("--fail-fast", action="store_true")
    p.set_defaults(func=lambda args: run_experiment(args))

    p = sub.add_parser("parse", help="parse a replay console log")
    p.add_argument("--log", required=True)
    p.add_argument("--arch")
    p.add_argument("--output-dir")
    p.set_defaults(func=parse_cmd)

    p = sub.add_parser("summarize", help="summarize a run directory")
    p.add_argument("run_dir")
    p.set_defaults(func=summarize_cmd)

    p = sub.add_parser("failures", help="group failed or incomplete LTP cases from a run")
    p.add_argument("run_dir")
    p.add_argument("--status", action="append", help="status filter, comma-separated or repeated")
    p.add_argument("--limit", type=int, default=200)
    p.add_argument("--json", action="store_true")
    p.set_defaults(func=failures_cmd)

    p = sub.add_parser("promote", help="merge stable passing cases into a new LTP list")
    p.add_argument("run_dir", nargs="+")
    p.add_argument("--require", action="append", help="required selector: rv/glibc, rv, glibc, or both; default all four")
    p.add_argument("--test-list", help="candidate list used for explain/status output")
    p.add_argument("--base", help="base list, default ltp_test.txt")
    p.add_argument("--output", required=True)
    p.add_argument("--allow-silent-pass", action="store_true", help="also promote silent-pass/TCONF-only cases")
    p.add_argument("--dry-run", action="store_true", help="show what would be promoted without writing output")
    p.add_argument("--explain", action="store_true", help="print promotable and blocked cases with combo status")
    p.add_argument("--show-missing", action="store_true", help="with --explain, only show blocked cases that have missing evidence")
    p.add_argument("--status-matrix", action="store_true", help="alias for explain-style combo status output")
    p.set_defaults(func=promote_cmd)

    p = sub.add_parser("matrix-status", help="show per-case status across arch/libc combos")
    p.add_argument("run_dir", nargs="+")
    p.add_argument("--test-list", help="candidate list to order/filter output")
    p.add_argument("--require", action="append", help="combo selector: rv/glibc, rv, glibc, or both; default all four")
    p.add_argument("--only-missing", action="store_true", help="only show cases missing at least one selected combo")
    p.add_argument("--limit", type=int)
    p.set_defaults(func=matrix_status_cmd)

    p = sub.add_parser("missing-combos", help="write per-combo rerun lists for missing/nonpassing evidence")
    p.add_argument("run_dir", nargs="+")
    p.add_argument("--test-list", help="candidate list to check")
    p.add_argument("--require", action="append", help="combo selector: rv/glibc, rv, glibc, or both; default all four")
    p.add_argument("--output", required=True, help="output directory")
    p.add_argument("--allow-silent-pass", action="store_true", help="treat silent-pass as passing evidence")
    p.set_defaults(func=missing_combos_cmd)

    p = sub.add_parser("reorder", help="sort an LTP list using pass/fail/timing evidence")
    p.add_argument("--base", required=True, help="base LTP list")
    p.add_argument("--evidence", action="append", required=True, help="run dir, comma-separated or repeated")
    p.add_argument("--require", action="append", help="combo selector: rv/glibc, rv, glibc, or both; default all four")
    p.add_argument("--output", required=True)
    p.add_argument("--fast-threshold", type=float, default=30.0, help="max per-combo case duration considered fast")
    p.set_defaults(func=lambda args: reorder_cmd(argparse.Namespace(**{**vars(args), "evidence": split_csv(args.evidence)})))

    p = sub.add_parser("campaign", help="agentic batch workflow for LTP implementation campaigns")
    campaign_sub = p.add_subparsers(dest="campaign_cmd", required=True)

    c = campaign_sub.add_parser("create", help="create a fixed candidate batch with semantic prompts")
    c.add_argument("name")
    add_common_generation_args(c)
    c.add_argument("--goal", default="LTP semantic expansion campaign")
    c.add_argument("--risk", choices=["low", "medium", "high"], default="medium")
    c.add_argument("--testsuite-source", help="testsuits-for-oskernel source checkout")
    c.add_argument("--linux-ref", help="Linux behavior reference source tree")
    c.add_argument("--replace", action="store_true")
    c.set_defaults(func=campaign_create_cmd)

    c = campaign_sub.add_parser("list", help="list local campaigns")
    c.set_defaults(func=campaign_list_cmd)

    c = campaign_sub.add_parser("status", help="show campaign state")
    c.add_argument("name")
    c.set_defaults(func=campaign_status_cmd)

    c = campaign_sub.add_parser("run", help="run the campaign candidate list and record the run")
    c.add_argument("name")
    c.add_argument("--run-name", help="explicit run id under .state/ltp-lab/runs")
    c.add_argument("--inventory", help="inventory JSON path")
    c.add_argument("--arch", action="append", choices=["rv", "la", "riscv64", "loongarch64", "both"], help="arch set override")
    c.add_argument("--libc", action="append", choices=["glibc", "musl", "both"], help="libc set override")
    c.add_argument("--plan", help="existing plan path")
    c.add_argument("--replace", action="store_true")
    c.add_argument("--image", help="official image override, only for single-arch runs")
    c.add_argument("--timeout", type=int, default=7000)
    c.add_argument("--parallel", choices=["arch", "combo", "serial"], default="arch", help="matrix execution mode, default arch")
    c.add_argument("--split-combos", action="store_true", help="run each selected arch/libc combo as its own task")
    c.add_argument("--jobs", default="auto", help="parallel task slots: auto or a positive integer")
    c.add_argument("--no-parallel", action="store_true", help="equivalent to --parallel serial --jobs 1")
    c.add_argument("--case-timeout", type=int, default=DEFAULT_CAMPAIGN_CASE_TIMEOUT, help="per-LTP-case timeout inside the guest; 0 disables")
    c.add_argument("--task-timeout", type=int, help="per-QEMU-task timeout; defaults to --timeout")
    c.add_argument("--ltp-order", choices=["glibc-first", "musl-first"], default="glibc-first")
    c.add_argument("--ltp-budget", type=int)
    c.add_argument("--glibc-budget", type=int)
    c.add_argument("--musl-budget", type=int)
    c.add_argument("--env", action="append", help="guest env KEY=VALUE, comma-separated or repeated")
    c.add_argument("--build", dest="rebuild_kernels", action="store_true", help="build kernels before replay")
    c.add_argument("--rebuild-kernels", action="store_true", help=argparse.SUPPRESS)
    c.add_argument("--skip-kernel-build", action="store_true", help=argparse.SUPPRESS)
    c.add_argument("--prepare-only", action="store_true", help="build list, plan, and support image without QEMU replay")
    c.add_argument("--fail-fast", action="store_true")
    c.set_defaults(func=campaign_run_cmd)

    c = campaign_sub.add_parser("review", help="analyze evidence and dry-run promotion")
    c.add_argument("name")
    c.add_argument("--run", action="append", help="run id to review, comma-separated or repeated")
    c.add_argument("--latest", action="store_true", help="review only the latest recorded run")
    c.add_argument("--require", action="append", help="required selector: rv/glibc, rv, glibc, or both; default all four")
    c.add_argument("--allow-silent-pass", action="store_true")
    c.add_argument("--show-missing", action="store_true")
    c.add_argument("--status-matrix", action="store_true")
    c.set_defaults(func=campaign_review_cmd)

    c = campaign_sub.add_parser("attach-run", help="record an existing lab run as campaign evidence")
    c.add_argument("name")
    c.add_argument("run", nargs="+", help="run id/path to attach, comma-separated or repeated")
    c.add_argument("--status", default="completed", help="manifest status for the attached run")
    c.add_argument("--note", help="short note describing why the run was attached")
    c.set_defaults(func=campaign_attach_run_cmd)

    c = campaign_sub.add_parser("analyze", help="classify campaign run evidence")
    c.add_argument("name")
    c.add_argument("--run", action="append", help="run id to analyze, comma-separated or repeated")
    c.add_argument("--latest", action="store_true", help="analyze only the latest recorded run")
    c.add_argument("--require", action="append", help="required selector: rv/glibc, rv, glibc, or both; default all four")
    c.add_argument("--allow-silent-pass", action="store_true")
    c.set_defaults(func=campaign_analyze_cmd)

    c = campaign_sub.add_parser("promote", help="write or apply a promoted list from campaign evidence")
    c.add_argument("name")
    c.add_argument("--run", action="append", help="run id to promote from, comma-separated or repeated")
    c.add_argument("--require", action="append", help="required selector: rv/glibc, rv, glibc, or both; default all four")
    c.add_argument("--base", help="base list, default ltp_test.txt")
    c.add_argument("--output", help="output list path, default campaign promoted-ltp_test.txt")
    c.add_argument("--allow-silent-pass", action="store_true")
    c.add_argument("--dry-run", action="store_true")
    c.add_argument("--explain", action="store_true")
    c.add_argument("--show-missing", action="store_true")
    c.add_argument("--status-matrix", action="store_true")
    c.add_argument("--apply-root", action="store_true", help="replace root ltp_test.txt with the promoted output")
    c.set_defaults(func=campaign_promote_cmd)

    c = campaign_sub.add_parser("apply", help="apply all-four promoted cases to root ltp_test.txt")
    c.add_argument("name")
    c.add_argument("--run", action="append", help="run id to promote from, comma-separated or repeated")
    c.add_argument("--require", action="append", help="required selector: rv/glibc, rv, glibc, or both; default all four")
    c.add_argument("--allow-silent-pass", action="store_true")
    c.add_argument("--dry-run", action="store_true")
    c.add_argument("--explain", action="store_true")
    c.add_argument("--show-missing", action="store_true")
    c.add_argument("--status-matrix", action="store_true")
    c.set_defaults(func=campaign_apply_cmd)

    c = campaign_sub.add_parser("finish", help="analyze and clean heavy per-run artifacts while retaining evidence")
    c.add_argument("name")
    c.add_argument("--run", action="append", help="run id to include, comma-separated or repeated")
    c.add_argument("--require", action="append", help="required selector: rv/glibc, rv, glibc, or both; default all four")
    c.add_argument("--allow-silent-pass", action="store_true")
    c.add_argument("--no-clean", action="store_true", help="skip automatic support.img/workdir cleanup")
    c.add_argument("--dry-run", action="store_true")
    c.set_defaults(func=campaign_finish_cmd)

    c = campaign_sub.add_parser("clean", help="clean campaign-owned state")
    c.add_argument("name")
    c.add_argument("--run", action="append", help="run id, comma-separated or repeated")
    c.add_argument("--heavy", action="store_true", help="remove support.img and QEMU workdirs for campaign runs")
    c.add_argument("--runs", action="store_true", help="remove campaign run directories")
    c.add_argument("--campaign", action="store_true", help="remove the campaign directory itself")
    c.add_argument("--dry-run", action="store_true")
    c.set_defaults(func=campaign_clean_cmd)

    p = sub.add_parser("clean", help="remove lab state or old root artifacts")
    p.add_argument("preset", nargs="*", help="short cleanup preset: trim, generated, runs, cache, refs, lab, legacy-root, smoke, or all")
    p.add_argument("--trim", action="store_true", help="daily cleanup of disposable failed/empty runs, per-run heavy artifacts, baseline images, smoke state, and legacy root outputs")
    p.add_argument("--lab", action="store_true", help="remove the whole .state/ltp-lab tree")
    p.add_argument("--generated", action="store_true", help="remove generated runs, lists, and plans")
    p.add_argument("--runs", action="store_true", help="remove run directories")
    p.add_argument("--run", action="append", help="specific run name or path, comma-separated or repeated")
    p.add_argument("--failed-runs", action="store_true", help="remove runs with nonzero replay exit codes")
    p.add_argument("--empty-runs", action="store_true", help="remove runs whose parsed summaries contain zero cases")
    p.add_argument("--keep-runs", type=int, help="when cleaning --runs, keep newest N runs")
    p.add_argument("--lists", action="store_true", help="remove generated LTP lists")
    p.add_argument("--plans", action="store_true", help="remove generated evaluation plans")
    p.add_argument("--campaigns", action="store_true", help="remove campaign directories")
    p.add_argument("--campaign", action="append", help="specific campaign name or path, comma-separated or repeated")
    p.add_argument("--inventory", action="store_true", help="remove inventory.json")
    p.add_argument("--images", action="store_true", help="remove cached decompressed official images")
    p.add_argument("--cache", action="store_true", help="remove cacheable lab data such as cached images")
    p.add_argument("--refs", action="store_true", help="remove optional reference checkouts under .state/ltp-lab/refs")
    p.add_argument("--support-images", action="store_true", help="remove per-run support.img files while keeping logs")
    p.add_argument("--workdirs", action="store_true", help="remove per-run QEMU workdirs while keeping parsed logs")
    p.add_argument("--baseline-heavy", action="store_true", help="remove baseline replay sdcard/disk image copies while keeping logs and parsed summaries")
    p.add_argument("--smoke", action="store_true", help="remove lab entries whose names contain 'smoke'")
    p.add_argument("--older-than", help="only remove matching items older than a duration like 12h, 7d, or 2w")
    p.add_argument("--legacy-root", action="store_true", help="remove stale root score artifacts rv_.out/la_.out/score.txt")
    p.add_argument("--all", action="store_true", help="remove all lab state, cache, refs, inventory, and legacy root artifacts")
    p.add_argument("--dry-run", action="store_true")
    p.set_defaults(func=clean_cmd)

    return parser


def main(argv: list[str] | None = None) -> None:
    parser = build_parser()
    args = parser.parse_args(argv)
    args.func(args)


if __name__ == "__main__":
    main()
