"""Host CPU selection contract for reproducible MM performance evidence."""

from __future__ import annotations

import os
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable


class CpuSelectionError(ValueError):
    """Raised when an explicit CPU selection or input is invalid."""


class CpuSelectionUnsupported(RuntimeError):
    """Raised when auto-selection cannot find a homogeneous CPU pool."""


@dataclass(frozen=True)
class CpuSelection:
    requested_cpus: int
    host_cpu_set: str
    selection: str
    cpu_class: str


def parse_cpu_list(value: str) -> tuple[int, ...]:
    if not value or any(character.isspace() for character in value):
        raise CpuSelectionError(f"invalid CPU list: {value!r}")
    cpus: set[int] = set()
    for item in value.split(","):
        if not item:
            raise CpuSelectionError(f"invalid CPU list: {value!r}")
        bounds = item.split("-")
        if len(bounds) == 1:
            bounds.append(bounds[0])
        if len(bounds) != 2 or any(
            not bound.isascii() or not bound.isdecimal() for bound in bounds
        ):
            raise CpuSelectionError(f"invalid CPU list item: {item!r}")
        first, last = (int(bound, 10) for bound in bounds)
        if first > last or last > 1_048_575:
            raise CpuSelectionError(f"invalid CPU list range: {item!r}")
        for cpu in range(first, last + 1):
            if cpu in cpus:
                raise CpuSelectionError(f"duplicate CPU in list: {cpu}")
            cpus.add(cpu)
    return tuple(sorted(cpus))


def format_cpu_list(cpus: Iterable[int]) -> str:
    values = sorted(set(cpus))
    if not values:
        raise CpuSelectionError("CPU set must not be empty")
    ranges: list[str] = []
    first = previous = values[0]
    for cpu in values[1:]:
        if cpu == previous + 1:
            previous = cpu
            continue
        ranges.append(str(first) if first == previous else f"{first}-{previous}")
        first = previous = cpu
    ranges.append(str(first) if first == previous else f"{first}-{previous}")
    return ",".join(ranges)


def read_int(path: Path, label: str) -> int:
    try:
        value = path.read_text(encoding="ascii").strip()
    except OSError as error:
        raise CpuSelectionUnsupported(f"cannot read {label}: {path}: {error}") from error
    if not value.isascii() or not value.isdecimal():
        raise CpuSelectionUnsupported(f"invalid {label}: {path}: {value!r}")
    return int(value, 10)


def cpu_class(cpu: int, sysfs: Path) -> tuple[int, int]:
    cpu_root = sysfs / f"cpu{cpu}"
    return (
        read_int(
            cpu_root / "topology" / "physical_package_id",
            f"CPU {cpu} physical package",
        ),
        read_int(
            cpu_root / "cpufreq" / "cpuinfo_max_freq",
            f"CPU {cpu} maximum frequency",
        ),
    )


def format_cpu_class(value: tuple[int, int]) -> str:
    return f"package:{value[0]},max_freq_khz:{value[1]}"


def validate_counts(counts: Iterable[int]) -> tuple[int, ...]:
    values = tuple(counts)
    if not values or any(count <= 0 or count > 64 for count in values):
        raise CpuSelectionError("CPU counts must contain values from 1 to 64")
    if len(set(values)) != len(values):
        raise CpuSelectionError("CPU counts must not contain duplicates")
    return values


def select_cpu_sets(
    counts: Iterable[int],
    *,
    explicit: str | None = None,
    allowed: set[int] | None = None,
    sysfs: Path = Path("/sys/devices/system/cpu"),
) -> tuple[CpuSelection, ...]:
    requested = validate_counts(counts)
    maximum = max(requested)
    allowed_cpus = set(os.sched_getaffinity(0) if allowed is None else allowed)
    if not allowed_cpus:
        raise CpuSelectionUnsupported("the runner has an empty CPU affinity mask")

    if explicit is not None:
        pool = parse_cpu_list(explicit)
        if not set(pool).issubset(allowed_cpus):
            unavailable = sorted(set(pool) - allowed_cpus)
            raise CpuSelectionError(
                f"explicit CPU set is outside runner affinity: {unavailable!r}"
            )
        if len(pool) < maximum:
            raise CpuSelectionError(
                f"explicit CPU set has {len(pool)} CPUs; {maximum} are required"
            )
        classes = {cpu_class(cpu, sysfs) for cpu in pool}
        if len(classes) != 1:
            details = ", ".join(
                f"cpu={cpu} {format_cpu_class(cpu_class(cpu, sysfs))}"
                for cpu in pool
            )
            raise CpuSelectionError(
                "explicit CPU set mixes host CPU classes: " + details
            )
        selected_class = next(iter(classes))
        selection = "explicit-homogeneous-v1"
        class_name = format_cpu_class(selected_class)
    else:
        groups: dict[tuple[int, int], list[int]] = {}
        for cpu in sorted(allowed_cpus):
            groups.setdefault(cpu_class(cpu, sysfs), []).append(cpu)
        eligible = [
            (key, tuple(cpus))
            for key, cpus in groups.items()
            if len(cpus) >= maximum
        ]
        if not eligible:
            summary = ", ".join(
                f"package={key[0]} max_khz={key[1]} cpus={format_cpu_list(cpus)}"
                for key, cpus in sorted(groups.items())
            )
            raise CpuSelectionUnsupported(
                "no homogeneous host CPU class can hold the requested matrix: "
                + (summary or "no CPU classes")
            )
        (package, max_frequency), pool = min(
            eligible,
            key=lambda item: (
                -len(item[1]),
                item[0][0],
                -item[0][1],
                item[1],
            ),
        )
        selection = "auto-homogeneous-v1"
        class_name = format_cpu_class((package, max_frequency))

    return tuple(
        CpuSelection(
            count,
            format_cpu_list(pool[:count]),
            selection,
            class_name,
        )
        for count in requested
    )
