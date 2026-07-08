"""Common focused-lab case list parsers."""

from __future__ import annotations

from pathlib import Path

from ..config import Libc
from .model import TestCase


def parse_command_list(*, group: str, libc: Libc, path: Path, root: Path | None = None) -> tuple[TestCase, ...]:
    cases: list[TestCase] = []
    seen: set[str] = set()
    source = str(path)
    if root is not None:
        try:
            source = str(path.relative_to(root))
        except ValueError:
            pass
    for raw in path.read_text(encoding="utf-8").splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        parts = line.split()
        name = parts[0]
        if name in seen:
            continue
        seen.add(name)
        command = parts[1] if len(parts) > 1 else name
        args = tuple(parts[2:]) if len(parts) > 2 else ()
        cases.append(
            TestCase(
                group=group,
                libc=libc,
                name=name,
                command=command,
                args=args,
                source=source,
            )
        )
    return tuple(cases)
