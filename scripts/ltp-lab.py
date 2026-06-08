#!/usr/bin/env python3
"""Local LTP experiment harness for the OSComp evaluator flow."""

from __future__ import annotations

import argparse
import datetime as _dt
import fnmatch
import json
import os
import random
import re
import shutil
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Iterable


REPO_ROOT = Path(__file__).resolve().parents[1]
STATE_DIR = REPO_ROOT / ".state" / "ltp-lab"
LIST_DIR = STATE_DIR / "lists"
PLAN_DIR = STATE_DIR / "plans"
RUN_DIR = STATE_DIR / "runs"
IMAGE_CACHE_DIR = STATE_DIR / "images"
REF_DIR = STATE_DIR / "refs"
INVENTORY_PATH = STATE_DIR / "inventory.json"
DEFAULT_TEST_LIST = REPO_ROOT / "ltp_test.txt"
DEFAULT_PLAN_NAME = "ltp-both"
DEFAULT_TESTSUITE_SOURCE = Path.home() / "testsuits-for-oskernel"
DEFAULT_TESTSUITE_REF = REF_DIR / "testsuits-for-oskernel"
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


def die(message: str) -> None:
    print(f"[ltp-lab] error: {message}", file=sys.stderr)
    raise SystemExit(1)


def log(message: str) -> None:
    print(f"[ltp-lab] {message}")


def ensure_dirs() -> None:
    for path in (STATE_DIR, LIST_DIR, PLAN_DIR, RUN_DIR, IMAGE_CACHE_DIR, REF_DIR):
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


def generate_list(args: argparse.Namespace) -> Path:
    ensure_dirs()
    inv = load_inventory(args.inventory)
    arches = canonical_arches(args.arch)
    libcs = canonical_libcs(args.libc)
    mode = args.mode
    current_list_path = inventory_repo_path(inv, inv.get("current_list"), DEFAULT_TEST_LIST)
    current_items = parse_test_list(current_list_path)
    if not current_items:
        current_items = inv["current"]["items"]
    current_markers = {item["marker"] for item in current_items}
    available = selected_available_names(inv, arches, libcs)
    lines: list[str]

    if mode == "current":
        lines = [item["line"].strip() for item in current_items]
    elif mode == "cases":
        raw_cases = split_csv(args.case)
        if not raw_cases:
            die("generate --mode cases requires --case")
        lines = raw_cases
    elif mode == "all-bins":
        lines = sorted(available)
    elif mode in ("runtest", "unopened-runtest"):
        entries = inv["source_runtest"].get("entries_data", [])
        runtest_filters = set(split_csv(args.runtest))
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


def build_support_image(args: argparse.Namespace, run_path: Path, arches: list[str], test_list: Path, plan: Path) -> Path:
    support_image = run_path / "support.img"
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
    env_path = env_file_for(args, run_path)
    if env_path:
        cmd.extend(["--env-override", str(env_path)])
    run_cmd(cmd, capture=True)
    return support_image


def run_experiment(args: argparse.Namespace) -> Path:
    ensure_dirs()
    arches = canonical_arches(args.arch)
    if args.image and len(arches) > 1:
        die("--image override is only valid for single-arch runs")
    run_id = args.name or now_id()

    if not args.skip_kernel_build:
        ensure_kernels(arches, args.rebuild_kernels)

    run_path = RUN_DIR / run_id
    if run_path.exists() and not args.replace:
        die(f"run already exists: {run_path}; pass --replace or choose --name")
    if run_path.exists():
        shutil.rmtree(run_path)
    run_path.mkdir(parents=True)

    test_list = Path(args.test_list).expanduser() if args.test_list else generate_list(
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
            output=str(run_path / "ltp_test.txt"),
        )
    )
    plan = Path(args.plan).expanduser() if args.plan else write_plan(
        argparse.Namespace(
            libc=args.libc,
            group=["ltp"],
            ltp_order=args.ltp_order,
            name=f"{run_id}-plan",
            output=str(run_path / "plan.txt"),
        )
    )
    support_image = build_support_image(args, run_path, arches, test_list, plan)
    manifest = {
        "run_id": run_id,
        "created_at": _dt.datetime.now().isoformat(timespec="seconds"),
        "repo_root": str(REPO_ROOT),
        "arches": arches,
        "libcs": canonical_libcs(args.libc),
        "test_list": str(test_list),
        "plan": str(plan),
        "support_image": str(support_image),
        "commands": {},
    }
    write_json(run_path / "manifest.json", manifest)
    if args.prepare_only:
        log(f"prepared run inputs in {run_path}")
        return run_path

    replay_failures: dict[str, int] = {}
    for arch in arches:
        arch_dir = run_path / arch
        arch_dir.mkdir()
        workdir = arch_dir / "work"
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
            str(args.timeout),
            "--skip-kernel-build",
        ]
        if args.image:
            cmd.extend(["--image", str(Path(args.image).expanduser())])
        manifest["commands"][arch] = cmd
        write_json(run_path / "manifest.json", manifest)
        log(f"running {arch}; log={arch_dir / 'console.log'}")
        proc = run_cmd(cmd, capture=False, check=False, log_path=arch_dir / "console.log")
        (arch_dir / "exit_code.txt").write_text(f"{proc.returncode}\n", encoding="utf-8")
        parse_log_file(arch_dir / "console.log", arch=arch, output_dir=arch_dir)
        if proc.returncode != 0:
            replay_failures[arch] = proc.returncode
            if args.fail_fast:
                summarize_run(run_path)
                die(f"{arch} replay failed with {proc.returncode}")
    summarize_run(run_path)
    if replay_failures:
        details = " ".join(f"{arch}={code}" for arch, code in sorted(replay_failures.items()))
        die(f"replay failed: {details}")
    return run_path


GROUP_START_RE = re.compile(r"#### OS COMP TEST GROUP START ([^ ]+) ####")
GROUP_END_RE = re.compile(r"#### OS COMP TEST GROUP END ([^ ]+) ####")
RUN_CASE_RE = re.compile(r"^RUN LTP CASE (.+)$")
END_CASE_RE = re.compile(r"^FAIL LTP CASE (.+?) : (-?\d+)$")
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


def summarize_run(run_path: Path) -> dict[str, Any]:
    run_path = run_path.expanduser()
    if not run_path.is_dir():
        die(f"run dir not found: {run_path}")
    summaries: dict[str, Any] = {}
    exit_codes: dict[str, int] = {}
    for arch in ARCHES:
        summary_path = run_path / arch / "summary.json"
        if summary_path.is_file():
            summaries[arch] = read_json(summary_path)
        exit_code_path = run_path / arch / "exit_code.txt"
        if exit_code_path.is_file():
            try:
                exit_codes[arch] = int(exit_code_path.read_text(encoding="utf-8").strip())
            except ValueError:
                exit_codes[arch] = -1
    total: dict[str, int] = {}
    for summary in summaries.values():
        for status, count in summary.get("by_status", {}).items():
            total[status] = total.get(status, 0) + int(count)
    failed_arches = {arch: code for arch, code in exit_codes.items() if code != 0}
    combined = {
        "run": str(run_path),
        "arches": summaries,
        "exit_codes": exit_codes,
        "failed_arches": failed_arches,
        "total_by_status": total,
    }
    write_json(run_path / "combined-summary.json", combined)
    print(f"run: {run_path}")
    for arch in sorted(summaries):
        if arch in exit_codes:
            print(f"replay_exit[{arch}]={exit_codes[arch]}")
        print_summary(summaries[arch])
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
        for case in read_cases_jsonl(run_path / arch / "cases.jsonl"):
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


def promote_cmd(args: argparse.Namespace) -> None:
    run_dirs = [Path(item).expanduser() for item in args.run_dir]
    required = split_csv(args.require) or [f"{arch}/{libc}" for arch in ARCHES for libc in LIBCS]
    passing_statuses = {"pass"}
    if args.allow_silent_pass:
        passing_statuses.add("silent-pass")
    pass_sets: dict[str, set[str]] = {key: set() for key in required}
    line_by_case: dict[str, str] = {}
    for run_dir in run_dirs:
        list_path = run_dir / "ltp_test.txt"
        for item in parse_test_list(list_path):
            line_by_case[item["marker"]] = item["line"].strip()
        for arch in ARCHES:
            for case in read_cases_jsonl(run_dir / arch / "cases.jsonl"):
                key = f"{arch}/{case.get('libc') or 'unknown'}"
                if key in pass_sets and case.get("status") in passing_statuses:
                    pass_sets[key].add(case["case"])
    if not pass_sets:
        die("no required combos selected")
    promoted = set.intersection(*pass_sets.values()) if pass_sets else set()
    base_path = Path(args.base).expanduser() if args.base else DEFAULT_TEST_LIST
    base_items = parse_test_list(base_path)
    existing = {item["marker"] for item in base_items}
    lines = [item["line"].strip() for item in base_items]
    added = 0
    for case in sorted(promoted):
        if case in existing:
            continue
        lines.append(line_by_case.get(case, case))
        added += 1
    output = Path(args.output).expanduser()
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text("\n".join(lines) + "\n", encoding="utf-8")
    log(f"promoted {added} new cases into {output} (base={len(base_items)} total={len(lines)})")


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
        for code in (data.get("exit_codes") or {}).values():
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


def clean_cmd(args: argparse.Namespace) -> None:
    targets: list[Path] = []
    requested = any(
        (
            args.lab,
            args.generated,
            args.runs,
            bool(args.run),
            args.failed_runs,
            args.empty_runs,
            args.lists,
            args.plans,
            args.inventory,
            args.images,
            args.cache,
            args.refs,
            args.support_images,
            args.workdirs,
            args.smoke,
            args.legacy_root,
            args.all,
        )
    )
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
    if args.images:
        for item in apply_time_filters(children_for_cleanup(IMAGE_CACHE_DIR), older_than=args.older_than, keep=None):
            add_cleanup_target(targets, item)
    if args.refs:
        for item in children_for_cleanup(REF_DIR):
            add_cleanup_target(targets, item)
    if args.support_images:
        for run_dir in children_for_cleanup(RUN_DIR, dirs_only=True):
            image = run_dir / "support.img"
            if image.exists():
                if args.older_than:
                    cutoff = _dt.datetime.now().timestamp() - parse_duration(args.older_than)
                    if newest_mtime(image) >= cutoff:
                        continue
                add_cleanup_target(targets, image)
    if args.workdirs:
        for run_dir in children_for_cleanup(RUN_DIR, dirs_only=True):
            for arch in ARCHES:
                workdir = run_dir / arch / "work"
                if not workdir.exists():
                    continue
                if args.older_than:
                    cutoff = _dt.datetime.now().timestamp() - parse_duration(args.older_than)
                    if newest_mtime(workdir) >= cutoff:
                        continue
                add_cleanup_target(targets, workdir)
    if args.smoke:
        for base in (LIST_DIR, PLAN_DIR, RUN_DIR):
            for item in children_for_cleanup(base):
                if "smoke" in item.name.lower():
                    add_cleanup_target(targets, item)

    if not targets and not requested:
        die(
            "clean requires a target such as --generated, --runs, --run NAME, "
            "--failed-runs, --empty-runs, --cache, --lab, --legacy-root, or --all"
        )
    if not targets:
        log("nothing to clean")
        return
    targets = collapse_cleanup_targets(targets)
    for target in targets:
        if not target.exists():
            if args.dry_run:
                print(f"{target} (missing)")
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
        checks.append(("lab_images", str(len(children_for_cleanup(IMAGE_CACHE_DIR)))))
    build_state = [path.name for path in BUILD_STATE_DIRS if path.exists()]
    checks.append(("build_state", ",".join(build_state) if build_state else "absent"))
    checks.append(("old_ltp_count_state", "present" if (REPO_ROOT / ".state" / "ltp-count-current").exists() else "absent"))
    legacy_root = [name for name in ("rv_.out", "la_.out", "score.txt") if (REPO_ROOT / name).exists()]
    checks.append(("legacy_root_outputs", ",".join(legacy_root) if legacy_root else "absent"))
    stale_docs = [str(path) for path in (REPO_ROOT / "docs").glob("*.md") if path.name in {"x11.md"}]
    checks.append(("stale_docs", ",".join(stale_docs) if stale_docs else "absent"))
    smoke_state: list[str] = []
    if STATE_DIR.exists():
        for base in (LIST_DIR, PLAN_DIR, RUN_DIR):
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

    p = sub.add_parser("run", help="build a support disk, replay QEMU, and parse logs")
    add_common_generation_args(p)
    p.add_argument("--test-list", help="existing LTP list path")
    p.add_argument("--plan", help="existing plan path")
    p.add_argument("--name", help="run id")
    p.add_argument("--replace", action="store_true")
    p.add_argument("--image", help="official image override, only for single-arch runs")
    p.add_argument("--timeout", type=int, default=7000)
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
    p.add_argument("--require", action="append", help="required combo such as rv/glibc, default all four")
    p.add_argument("--base", help="base list, default ltp_test.txt")
    p.add_argument("--output", required=True)
    p.add_argument("--allow-silent-pass", action="store_true", help="also promote silent-pass/TCONF-only cases")
    p.set_defaults(func=promote_cmd)

    p = sub.add_parser("clean", help="remove lab state or old root artifacts")
    p.add_argument("--lab", action="store_true", help="remove the whole .state/ltp-lab tree")
    p.add_argument("--generated", action="store_true", help="remove generated runs, lists, and plans")
    p.add_argument("--runs", action="store_true", help="remove run directories")
    p.add_argument("--run", action="append", help="specific run name or path, comma-separated or repeated")
    p.add_argument("--failed-runs", action="store_true", help="remove runs with nonzero replay exit codes")
    p.add_argument("--empty-runs", action="store_true", help="remove runs whose parsed summaries contain zero cases")
    p.add_argument("--keep-runs", type=int, help="when cleaning --runs, keep newest N runs")
    p.add_argument("--lists", action="store_true", help="remove generated LTP lists")
    p.add_argument("--plans", action="store_true", help="remove generated evaluation plans")
    p.add_argument("--inventory", action="store_true", help="remove inventory.json")
    p.add_argument("--images", action="store_true", help="remove cached decompressed official images")
    p.add_argument("--cache", action="store_true", help="remove cacheable lab data such as cached images")
    p.add_argument("--refs", action="store_true", help="remove optional reference checkouts under .state/ltp-lab/refs")
    p.add_argument("--support-images", action="store_true", help="remove per-run support.img files while keeping logs")
    p.add_argument("--workdirs", action="store_true", help="remove per-run QEMU workdirs while keeping parsed logs")
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
