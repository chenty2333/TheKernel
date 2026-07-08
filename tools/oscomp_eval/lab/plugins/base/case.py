"""Focused-lab case adapter interface."""

from __future__ import annotations

from abc import ABC

from ...model import TestCase


class CaseAdapter(ABC):
    """Base class for optional group-specific case adapters."""

    def applies(self, case: TestCase) -> bool:
        return False

    def adapt(self, case: TestCase) -> TestCase:
        return case

