#!/usr/bin/env python3
"""Focused negative tests for closure-cohorts-v1."""

from __future__ import annotations

import importlib.util
import json
import shutil
import sys
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("abi_cohorts", ROOT / "tools" / "abi_cohorts.py")
assert SPEC and SPEC.loader
abi_cohorts = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = abi_cohorts
SPEC.loader.exec_module(abi_cohorts)


class AbiCohortTests(unittest.TestCase):
    def copied_inputs(self) -> tuple[tempfile.TemporaryDirectory[str], Path]:
        temporary = tempfile.TemporaryDirectory()
        abi = Path(temporary.name) / "docs" / "linux-abi"
        abi.mkdir(parents=True)
        for name in (abi_cohorts.TABLE.name, abi_cohorts.MATRIX.name, abi_cohorts.COHORTS.name):
            shutil.copy2(ROOT / "docs" / "linux-abi" / name, abi / name)
        return temporary, abi

    def validate(self, abi: Path) -> dict[str, int]:
        return abi_cohorts.validate(abi / abi_cohorts.COHORTS.name, abi / abi_cohorts.TABLE.name, abi / abi_cohorts.MATRIX.name)

    def test_checked_in_document_is_complete_and_ordered(self) -> None:
        self.assertEqual(self.validate(ROOT / "docs" / "linux-abi"), {"native-ni": 17, "phase3": 87, "phase2": 271})

    def test_rejects_hash_drift(self) -> None:
        temporary, abi = self.copied_inputs()
        self.addCleanup(temporary.cleanup)
        path = abi / abi_cohorts.COHORTS.name
        document = json.loads(path.read_text())
        document["baseline"]["matrix"]["sha256"] = "0" * 64
        path.write_text(json.dumps(document))
        with self.assertRaisesRegex(abi_cohorts.CohortError, "matrix hash drift"):
            self.validate(abi)

    def test_rejects_member_order_and_alias_drift(self) -> None:
        temporary, abi = self.copied_inputs()
        self.addCleanup(temporary.cleanup)
        path = abi / abi_cohorts.COHORTS.name
        document = json.loads(path.read_text())
        document["cohorts"][2]["members"][:2] = reversed(document["cohorts"][2]["members"][:2])
        path.write_text(json.dumps(document))
        with self.assertRaisesRegex(abi_cohorts.CohortError, "phase2 membership or syscall order drift"):
            self.validate(abi)
        document = json.loads((ROOT / "docs" / "linux-abi" / abi_cohorts.COHORTS.name).read_text())
        document["alias_groups"][0]["members"].reverse()
        path.write_text(json.dumps(document))
        with self.assertRaisesRegex(abi_cohorts.CohortError, "alias groups drift"):
            self.validate(abi)


if __name__ == "__main__":
    unittest.main()
