"""LTP focused-lab plugin."""

from __future__ import annotations

from pathlib import Path

from ..base import GroupPlugin
from ...catalog import CaseCatalog
from ...filters import select_cases
from ...model import PayloadDraft, Selection, TestCase
from ...parsers import parse_command_list


class LtpGroupPlugin(GroupPlugin):
    group = "ltp"
    selector_help = "case-level: exact, prefix=..., regex=..."

    def apply(self, selection: Selection, draft: PayloadDraft, *, root: Path) -> None:
        draft.add_group("ltp", selection.libc)
        if selection.expr is None:
            return
        for case in expand_ltp_expr(selection, root=root):
            draft.cases.append(case)


def expand_ltp_expr(selection: Selection, *, root: Path) -> tuple[TestCase, ...]:
    catalog = CaseCatalog.from_cases(load_ltp_cases(selection, root=root))
    return select_cases(selection, catalog, allow_fallback_exact=True)


def load_ltp_cases(selection: Selection, *, root: Path) -> tuple[TestCase, ...]:
    path = root / "ltp_test.txt"
    if not path.is_file():
        return ()
    return parse_command_list(group="ltp", libc=selection.libc, path=path, root=root)
