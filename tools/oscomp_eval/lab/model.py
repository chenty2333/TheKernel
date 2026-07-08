"""Shared focused-lab data structures."""

from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path

from ..config import Arch, Libc


@dataclass(frozen=True)
class Selection:
    group: str
    libc: Libc
    expr: str | None = None

    @property
    def group_id(self) -> str:
        return f"{self.group}-{self.libc}"

    @property
    def text(self) -> str:
        suffix = f":{self.expr}" if self.expr else ""
        return f"{self.group_id}{suffix}"


@dataclass(frozen=True)
class TestCase:
    group: str
    libc: Libc
    name: str
    command: str | None = None
    args: tuple[str, ...] = ()
    suite: str | None = None
    tags: frozenset[str] = frozenset()
    source: str | None = None

    @property
    def group_id(self) -> str:
        return f"{self.group}-{self.libc}"

    @property
    def ltp_line(self) -> str:
        if self.command:
            parts = [self.name, self.command, *self.args]
        else:
            parts = [self.name]
        return " ".join(part for part in parts if part)


@dataclass(frozen=True)
class FocusPlan:
    arch: Arch
    selections: tuple[Selection, ...]
    group_matrix: tuple[tuple[str, Libc], ...]
    cases: tuple[TestCase, ...] = ()
    plan_path: Path | None = None
    cases_path: Path | None = None
    ltp_list_path: Path | None = None
    support_image: Path | None = None
    notes: tuple[str, ...] = ()


@dataclass
class PayloadDraft:
    group_matrix: list[tuple[str, Libc]] = field(default_factory=list)
    cases: list[TestCase] = field(default_factory=list)
    notes: list[str] = field(default_factory=list)

    def add_group(self, group: str, libc: Libc) -> None:
        item = (group, libc)
        if item not in self.group_matrix:
            self.group_matrix.append(item)

