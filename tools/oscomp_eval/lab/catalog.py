"""Common focused-lab case catalog."""

from __future__ import annotations

from dataclasses import dataclass
from typing import Iterable

from .model import TestCase


@dataclass(frozen=True)
class CaseCatalog:
    cases: tuple[TestCase, ...]

    @classmethod
    def from_cases(cls, cases: Iterable[TestCase]) -> "CaseCatalog":
        deduped: list[TestCase] = []
        seen: set[tuple[str, str, str]] = set()
        for case in cases:
            key = (case.group, case.libc, case.name)
            if key in seen:
                continue
            seen.add(key)
            deduped.append(case)
        return cls(tuple(deduped))

    def names(self) -> tuple[str, ...]:
        return tuple(case.name for case in self.cases)

    def exact(self, name: str) -> TestCase | None:
        for case in self.cases:
            if case.name == name:
                return case
        return None

    def matching_prefix(self, prefix: str) -> tuple[TestCase, ...]:
        return tuple(case for case in self.cases if case.name.startswith(prefix))

    def matching_regex(self, pattern) -> tuple[TestCase, ...]:
        return tuple(case for case in self.cases if pattern.search(case.name))
