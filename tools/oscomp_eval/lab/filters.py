"""Common focused-lab selector filtering."""

from __future__ import annotations

import re

from .catalog import CaseCatalog
from .model import Selection, TestCase


class CaseFilterError(ValueError):
    """Raised when a selector expression cannot be applied."""


def select_cases(selection: Selection, catalog: CaseCatalog, *, allow_fallback_exact: bool = False) -> tuple[TestCase, ...]:
    expr = selection.expr
    if expr is None:
        return ()

    if expr.startswith("prefix="):
        prefix = expr.removeprefix("prefix=")
        cases = catalog.matching_prefix(prefix)
        if not cases:
            raise CaseFilterError(f"no {selection.group} cases match prefix: {prefix}")
        return cases

    if expr.startswith("regex="):
        pattern_text = expr.removeprefix("regex=")
        try:
            pattern = re.compile(pattern_text)
        except re.error as error:
            raise CaseFilterError(f"invalid {selection.group} regex selector: {pattern_text}") from error
        cases = catalog.matching_regex(pattern)
        if not cases:
            raise CaseFilterError(f"no {selection.group} cases match regex: {pattern_text}")
        return cases

    if expr.startswith("suite=") or expr.startswith("tag="):
        raise CaseFilterError(f"{selection.group} selector expression is not supported yet: {expr}")

    case = catalog.exact(expr)
    if case is not None:
        return (case,)
    if allow_fallback_exact:
        return (TestCase(group=selection.group, libc=selection.libc, name=expr, command=expr, source="selector"),)
    raise CaseFilterError(f"unknown {selection.group} case: {expr}")
