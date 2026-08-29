#!/usr/bin/env python3
"""Focused validation for the conditional-syscall catalog."""
from __future__ import annotations

import copy
import importlib.util
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
TOOL = ROOT / "tools" / "abi_conditions.py"
SPEC = importlib.util.spec_from_file_location("abi_conditions", TOOL)
assert SPEC and SPEC.loader
abi_conditions = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = abi_conditions
SPEC.loader.exec_module(abi_conditions)


class AbiConditionTests(unittest.TestCase):
    def document(self) -> dict:
        return json.loads((ROOT / "docs/linux-abi/conditional-syscalls-v1.json").read_text())

    def test_checked_in_catalog_is_exact_and_reports_unresolved_work(self) -> None:
        stats = abi_conditions.validate(self.document())
        self.assertEqual(stats, {"members": 162, "resolved": 0, "unresolved": 162})
        rows = {row["name"]: row for row in self.document()["members"]}
        self.assertIn("uselib", rows)
        self.assertEqual(rows["uselib"]["nr"], 134)
        self.assertEqual(rows["uselib"]["linux_source"]["mechanism"], "COND_SYSCALL")

    def test_cli_validate_prints_statistics(self) -> None:
        completed = subprocess.run(
            (sys.executable, str(TOOL), "--validate"), check=True,
            capture_output=True, text=True,
        )
        self.assertEqual(json.loads(completed.stdout), {"members": 162, "resolved": 0, "unresolved": 162})

    def test_rejects_membership_derived_from_route_and_empty_unresolved_gap(self) -> None:
        document = self.document()
        document["members"] = [row for row in document["members"] if row["name"] != "uselib"]
        with self.assertRaisesRegex(abi_conditions.CatalogError, "exactly 162"):
            abi_conditions.validate(document)

        document = self.document()
        document["members"][0]["predicate"]["gap"] = ""
        with self.assertRaisesRegex(abi_conditions.CatalogError, "nonempty gap"):
            abi_conditions.validate(document)

    def test_rejects_incomplete_resolved_evidence(self) -> None:
        document = copy.deepcopy(self.document())
        row = document["members"][0]
        for field in ("predicate", "product_expected_route", "positive_witness", "fixture"):
            row[field] = {"status": "resolved", "value": ""}
        with self.assertRaisesRegex(abi_conditions.CatalogError, "incomplete resolved"):
            abi_conditions.validate(document)

        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "catalog.json"
            path.write_text(json.dumps(document))
            completed = subprocess.run(
                (sys.executable, str(TOOL), "--validate", str(path)),
                capture_output=True, text=True,
            )
        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("incomplete resolved", completed.stderr)

    def test_rejects_unknown_resolved_fixture_reference(self) -> None:
        document = self.document()
        row = document["members"][0]
        row["predicate"] = {"status": "resolved", "value": "CONFIG_EXAMPLE"}
        row["product_expected_route"] = {"status": "resolved", "value": "implemented"}
        row["positive_witness"] = {"status": "resolved", "value": "q35-feature-witness"}
        row["fixture"] = {"status": "resolved", "value": "missing.case"}
        with self.assertRaisesRegex(abi_conditions.CatalogError, "fixture case reference"):
            abi_conditions.validate(document)


if __name__ == "__main__":
    unittest.main()
