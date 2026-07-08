"""Focused-lab group plugin interface."""

from __future__ import annotations

from abc import ABC, abstractmethod
from pathlib import Path

from ...model import PayloadDraft, Selection


class GroupPluginError(ValueError):
    """Raised when a group plugin rejects a selection."""


class GroupPlugin(ABC):
    group: str
    selector_help: str = "group-level"

    @abstractmethod
    def apply(self, selection: Selection, draft: PayloadDraft, *, root: Path) -> None:
        """Add this selection to a payload draft."""
