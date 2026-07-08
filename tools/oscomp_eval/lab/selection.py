"""Parse the common focused-lab selector shell."""

from __future__ import annotations

from ..config import Libc
from .model import Selection


class SelectionError(ValueError):
    """Raised for invalid focused-lab selectors."""


def parse_selection(value: str) -> Selection:
    text = value.strip()
    if not text:
        raise SelectionError("empty selector")
    head, sep, expr = text.partition(":")
    if "-" not in head:
        raise SelectionError(f"selector must be GROUP-LIBC[:EXPR]: {value}")
    group, libc = head.rsplit("-", 1)
    if not group:
        raise SelectionError(f"selector has empty group: {value}")
    if libc not in ("musl", "glibc"):
        raise SelectionError(f"selector has unsupported libc: {value}")
    if sep and not expr:
        raise SelectionError(f"selector has empty case expression: {value}")
    return Selection(group=group, libc=libc, expr=expr or None)  # type: ignore[arg-type]


def parse_selections(values: list[str]) -> tuple[Selection, ...]:
    if not values:
        raise SelectionError("at least one --select is required")
    return tuple(parse_selection(value) for value in values)

