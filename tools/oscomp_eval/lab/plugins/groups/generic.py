"""Generic whole-group focused-lab plugin."""

from __future__ import annotations

from pathlib import Path

from ..base import GroupPlugin, GroupPluginError
from ...model import PayloadDraft, Selection


class GenericGroupPlugin(GroupPlugin):
    def __init__(self, group: str) -> None:
        self.group = group

    def apply(self, selection: Selection, draft: PayloadDraft, *, root: Path) -> None:
        if selection.expr is not None:
            raise GroupPluginError(
                f"group {selection.group} does not support case-level selection yet: {selection.text}"
            )
        draft.add_group(selection.group, selection.libc)

