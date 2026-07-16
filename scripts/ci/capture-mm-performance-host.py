#!/usr/bin/env python3
"""Capture bounded, non-sensitive host diagnostics around one MM run."""

from __future__ import annotations

import argparse
import csv
import datetime as dt
import io
import os
from pathlib import Path

from mm_performance_host import CpuSelectionError, format_cpu_list, parse_cpu_list
from mm_performance_schema import HOST_DIAGNOSTIC_SCHEMA


SCHEMA = HOST_DIAGNOSTIC_SCHEMA
MAX_DIAGNOSTIC_BYTES = 64 * 1024


def read_optional(path: Path) -> str:
    try:
        return path.read_text(encoding="ascii", errors="replace").strip()
    except OSError:
        return "missing"


def current_cgroup() -> Path | None:
    try:
        lines = Path("/proc/self/cgroup").read_text(encoding="ascii").splitlines()
    except OSError:
        return None
    for line in lines:
        fields = line.split(":", 2)
        if len(fields) == 3 and fields[0] == "0":
            relative = fields[2].lstrip("/")
            return Path("/sys/fs/cgroup") / relative
    return None


def key_values(path: Path, prefix: str) -> list[tuple[str, str]]:
    value = read_optional(path)
    if value == "missing":
        return [(prefix, value)]
    rows: list[tuple[str, str]] = []
    for line in value.splitlines():
        fields = line.split()
        if not fields:
            continue
        if all("=" in field for field in fields[1:]):
            category = fields[0]
            for field in fields[1:]:
                name, number = field.split("=", 1)
                rows.append((f"{prefix}.{category}.{name}", number))
        elif len(fields) == 2:
            rows.append((f"{prefix}.{fields[0]}", fields[1]))
        else:
            rows.append((f"{prefix}.raw", "_".join(fields)))
    return rows


def collect(
    phase: str,
    cpus: tuple[int, ...],
    selection: str,
    cpu_class: str,
) -> list[tuple[str, str]]:
    actual_affinity = tuple(sorted(os.sched_getaffinity(0)))
    if actual_affinity != cpus:
        raise CpuSelectionError(
            "capture process affinity differs from selected CPU set: "
            f"selected={format_cpu_list(cpus)} "
            f"actual={format_cpu_list(actual_affinity)}"
        )
    rows = [
        ("schema", SCHEMA),
        ("phase", phase),
        ("timestamp_utc", dt.datetime.now(dt.UTC).isoformat()),
        ("selected_cpu_set", format_cpu_list(cpus)),
        ("host_cpu_selection", selection),
        ("host_cpu_class", cpu_class),
        ("online_cpu_set", read_optional(Path("/sys/devices/system/cpu/online"))),
        ("loadavg", read_optional(Path("/proc/loadavg"))),
    ]
    rows.extend(key_values(Path("/proc/pressure/cpu"), "psi.cpu"))
    cgroup = current_cgroup()
    if cgroup is None:
        rows.append(("cgroup.cpu_stat", "missing"))
    else:
        rows.extend(key_values(cgroup / "cpu.stat", "cgroup.cpu_stat"))
    cpu_root = Path("/sys/devices/system/cpu")
    for cpu in cpus:
        root = cpu_root / f"cpu{cpu}"
        online = "1" if cpu == 0 else read_optional(root / "online")
        rows.extend(
            (
                (f"cpu.{cpu}.online", online),
                (
                    f"cpu.{cpu}.package",
                    read_optional(root / "topology" / "physical_package_id"),
                ),
                (
                    f"cpu.{cpu}.max_freq_khz",
                    read_optional(root / "cpufreq" / "cpuinfo_max_freq"),
                ),
                (
                    f"cpu.{cpu}.current_freq_khz",
                    read_optional(root / "cpufreq" / "scaling_cur_freq"),
                ),
            )
        )
    return rows


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--phase", choices=("pre", "post"), required=True)
    parser.add_argument("--cpuset", required=True)
    parser.add_argument("--selection", required=True)
    parser.add_argument("--cpu-class", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    cpus = parse_cpu_list(args.cpuset)
    for name, value in (
        ("selection", args.selection),
        ("cpu-class", args.cpu_class),
    ):
        if not value or any(character in value for character in "\t\r\n"):
            parser.error(f"--{name} must be one non-empty TSV-safe field")
    rows = collect(args.phase, cpus, args.selection, args.cpu_class)
    buffer = io.StringIO(newline="")
    writer = csv.writer(buffer, delimiter="\t", lineterminator="\n")
    writer.writerow(("key", "value"))
    writer.writerows(rows)
    payload = buffer.getvalue().encode("utf-8")
    if len(payload) > MAX_DIAGNOSTIC_BYTES:
        raise RuntimeError(
            f"host diagnostics exceed {MAX_DIAGNOSTIC_BYTES} bytes: {len(payload)}"
        )
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_bytes(payload)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
