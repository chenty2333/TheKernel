"""Configuration primitives for the local OSComp evaluator."""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import Iterable, Literal


Arch = Literal["rv", "la"]
Libc = Literal["musl", "glibc"]


class ConfigError(ValueError):
    """Raised for unsupported evaluator configuration."""


ARCH_ALIASES: dict[str, Arch] = {
    "rv": "rv",
    "riscv64": "rv",
    "la": "la",
    "loongarch64": "la",
}

DEFAULT_ARCHES: tuple[Arch, ...] = ("rv", "la")

JUDGE_TIMEOUT_SECS = 30
REPLAY_TIMEOUT_FULL_SECS = 7000
REPLAY_TIMEOUT_FOCUSED_SECS = 3600
REPLAY_TIMEOUT_SMOKE_SECS = 240
SHELL_TIMEOUT_SECS = 0

# This is the current local default plan. It intentionally excludes
# libctest-glibc because src/init.sh skips that group today.
DEFAULT_GROUP_LIBC_MATRIX: tuple[tuple[str, Libc], ...] = (
    ("basic", "musl"),
    ("basic", "glibc"),
    ("busybox", "musl"),
    ("busybox", "glibc"),
    ("libctest", "musl"),
    ("lua", "musl"),
    ("lua", "glibc"),
    ("iperf", "musl"),
    ("iperf", "glibc"),
    ("netperf", "musl"),
    ("netperf", "glibc"),
    ("libcbench", "musl"),
    ("libcbench", "glibc"),
    ("iozone", "musl"),
    ("iozone", "glibc"),
    ("lmbench", "musl"),
    ("lmbench", "glibc"),
    ("ltp", "glibc"),
    ("ltp", "musl"),
    ("cyclictest", "musl"),
    ("cyclictest", "glibc"),
)


@dataclass(frozen=True)
class MatrixCell:
    arch: Arch
    group: str
    libc: Libc

    @property
    def group_id(self) -> str:
        return f"{self.group}-{self.libc}"

    @property
    def key(self) -> str:
        return f"{self.arch}/{self.group_id}"

    def to_json_dict(self) -> dict[str, str]:
        return {
            "arch": self.arch,
            "group": self.group,
            "libc": self.libc,
            "group_id": self.group_id,
            "key": self.key,
        }


@dataclass(frozen=True)
class EvalConfig:
    arches: tuple[Arch, ...] = DEFAULT_ARCHES
    group_libc_matrix: tuple[tuple[str, Libc], ...] = DEFAULT_GROUP_LIBC_MATRIX
    judge_timeout_secs: int = JUDGE_TIMEOUT_SECS
    replay_timeout_secs: int = REPLAY_TIMEOUT_FULL_SECS
    strict_markers: bool = True

    def expected_matrix(self) -> tuple[MatrixCell, ...]:
        return expand_expected_matrix(self.arches, self.group_libc_matrix)

    def to_json_dict(self) -> dict[str, object]:
        return {
            "arches": list(self.arches),
            "group_libc_matrix": [
                {"group": group, "libc": libc}
                for group, libc in self.group_libc_matrix
            ],
            "judge_timeout_secs": self.judge_timeout_secs,
            "replay_timeout_secs": self.replay_timeout_secs,
            "strict_markers": self.strict_markers,
        }


def canonical_arch(value: str) -> Arch:
    try:
        return ARCH_ALIASES[value]
    except KeyError as error:
        raise ConfigError(f"unsupported arch: {value}") from error


def expand_arches(value: str | Iterable[str] | None) -> tuple[Arch, ...]:
    if value is None or value == "both":
        return DEFAULT_ARCHES
    if isinstance(value, str):
        return (canonical_arch(value),)
    return tuple(canonical_arch(item) for item in value)


def expand_expected_matrix(
    arches: Iterable[Arch] = DEFAULT_ARCHES,
    group_libc_matrix: Iterable[tuple[str, Libc]] = DEFAULT_GROUP_LIBC_MATRIX,
) -> tuple[MatrixCell, ...]:
    return tuple(
        MatrixCell(arch=arch, group=group, libc=libc)
        for arch in arches
        for group, libc in group_libc_matrix
    )


def effective_group_libc_matrix(
    group_libc_matrix: Iterable[tuple[str, Libc]] | None = None,
) -> tuple[tuple[str, Libc], ...]:
    if group_libc_matrix is None:
        return DEFAULT_GROUP_LIBC_MATRIX
    return tuple(group_libc_matrix)


def group_libc_matrix_to_json(
    group_libc_matrix: Iterable[tuple[str, Libc]],
) -> list[dict[str, str]]:
    return [
        {"group": group, "libc": libc}
        for group, libc in group_libc_matrix
    ]


def expected_matrix_to_json(
    arches: Iterable[Arch],
    group_libc_matrix: Iterable[tuple[str, Libc]],
) -> list[dict[str, str]]:
    return [
        cell.to_json_dict()
        for cell in expand_expected_matrix(arches, group_libc_matrix)
    ]


def parse_group_id(value: str) -> tuple[str, Libc]:
    if "-" not in value:
        raise ConfigError(f"group id must include libc suffix: {value}")
    group, libc = value.rsplit("-", 1)
    if libc not in ("musl", "glibc"):
        raise ConfigError(f"group id has unsupported libc suffix: {value}")
    if not group:
        raise ConfigError(f"group id has empty group: {value}")
    return group, libc  # type: ignore[return-value]


def group_libc_matrix_from_plan_text(text: str) -> tuple[tuple[str, Libc], ...]:
    matrix: list[tuple[str, Libc]] = []
    seen: set[tuple[str, Libc]] = set()

    def add(group: str, libc: Libc) -> None:
        item = (group, libc)
        if item not in seen:
            seen.add(item)
            matrix.append(item)

    for line_no, raw in enumerate(text.splitlines(), 1):
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        tokens = line.split()
        if len(tokens) >= 2 and tokens[0] in ("/musl", "/glibc"):
            add(tokens[1], tokens[0][1:])  # type: ignore[arg-type]
            continue
        if len(tokens) == 1:
            group, libc = parse_group_id(tokens[0])
            add(group, libc)
            continue
        raise ConfigError(f"unsupported plan line {line_no}: {raw}")

    if not matrix:
        raise ConfigError("plan does not define any group/libc entries")
    return tuple(matrix)


def group_libc_matrix_from_plan(path: Path) -> tuple[tuple[str, Libc], ...]:
    try:
        text = path.read_text(encoding="utf-8")
    except OSError as error:
        raise ConfigError(f"could not read plan: {path}") from error
    return group_libc_matrix_from_plan_text(text)
