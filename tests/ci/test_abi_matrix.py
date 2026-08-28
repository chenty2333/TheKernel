#!/usr/bin/env python3
"""Focused checks for the Linux x86_64 ABI evidence matrix."""

from __future__ import annotations

import importlib.util
import json
import shutil
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
TOOL = ROOT / "tools" / "abi_matrix.py"
SPEC = importlib.util.spec_from_file_location("abi_matrix", TOOL)
assert SPEC and SPEC.loader
abi_matrix = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = abi_matrix
SPEC.loader.exec_module(abi_matrix)


class AbiMatrixTests(unittest.TestCase):
    def copy_abi(self) -> tuple[tempfile.TemporaryDirectory[str], Path]:
        temporary = tempfile.TemporaryDirectory()
        destination = Path(temporary.name) / "linux-abi"
        shutil.copytree(ROOT / "docs" / "linux-abi", destination)
        return temporary, destination

    def validate(self, directory: Path) -> dict[str, int]:
        return abi_matrix.validate_paths(
            directory / abi_matrix.SNAPSHOT.name,
            directory / "syscall-matrix.json",
            directory / "evidence-catalog.json",
            directory / "contracts",
        )

    def test_checked_in_matrix_covers_fixed_native_baseline(self) -> None:
        counts = self.validate(ROOT / "docs" / "linux-abi")
        self.assertEqual(counts["unknown"], 373)
        self.assertEqual(counts["implemented"], 2)
        self.assertEqual(sum(counts.values()), 375)
        matrix = json.loads((ROOT / "docs" / "linux-abi" / "syscall-matrix.json").read_text())
        eventfd_rows = {
            row["name"]: row for row in matrix["syscalls"]
            if row["name"] in {"eventfd", "eventfd2"}
        }
        self.assertEqual(
            eventfd_rows["eventfd"]["dispatch"],
            {"kind": "alias", "target": "sys_eventfd2"},
        )
        self.assertEqual(eventfd_rows["eventfd2"]["dispatch"]["kind"], "dispatch-arm")
        self.assertTrue(all(
            row["disposition"] == "implemented" and row["review"] == "reviewed"
            for row in eventfd_rows.values()
        ))
        self.assertTrue(all(
            row["disposition"] == "unknown" and row["review"] == "unreviewed"
            for row in matrix["syscalls"] if row["name"] not in eventfd_rows
        ))
        contracts = json.loads(
            (ROOT / "docs" / "linux-abi" / "contracts" / "eventfd.json").read_text()
        )
        self.assertTrue(all(
            cell["review"] == "reviewed" and cell["evidence"]
            for cell in contracts["cells"]
        ))

    def test_rejects_missing_row_and_unknown_evidence(self) -> None:
        temporary, directory = self.copy_abi()
        self.addCleanup(temporary.cleanup)
        matrix_path = directory / "syscall-matrix.json"
        document = json.loads(matrix_path.read_text())
        document["syscalls"].pop()
        matrix_path.write_text(json.dumps(document))
        with self.assertRaisesRegex(abi_matrix.MatrixError, "expected exactly"):
            self.validate(directory)

        document = json.loads((ROOT / "docs" / "linux-abi" / "syscall-matrix.json").read_text())
        document["syscalls"][0]["evidence"]["host-unit"] = ["does-not-exist"]
        matrix_path.write_text(json.dumps(document))
        with self.assertRaisesRegex(abi_matrix.MatrixError, "unknown evidence"):
            self.validate(directory)

    def test_rejects_baseline_drift_and_duplicate_syscall(self) -> None:
        temporary, directory = self.copy_abi()
        self.addCleanup(temporary.cleanup)
        matrix_path = directory / "syscall-matrix.json"
        document = json.loads(matrix_path.read_text())
        document["baseline"]["linux_tag"] = "v0"
        matrix_path.write_text(json.dumps(document))
        with self.assertRaisesRegex(abi_matrix.MatrixError, "baseline drift"):
            self.validate(directory)

        document = json.loads((ROOT / "docs" / "linux-abi" / "syscall-matrix.json").read_text())
        document["syscalls"][1]["name"] = document["syscalls"][0]["name"]
        matrix_path.write_text(json.dumps(document))
        with self.assertRaisesRegex(abi_matrix.MatrixError, "duplicate syscall"):
            self.validate(directory)

    def test_guest_ktap_evidence_is_present_hashed_and_complete(self) -> None:
        temporary, directory = self.copy_abi()
        self.addCleanup(temporary.cleanup)
        evidence_path = directory / "evidence" / "eventfd-systemtest.txt"
        evidence_path.unlink()
        with self.assertRaisesRegex(abi_matrix.MatrixError, "missing evidence"):
            self.validate(directory)

        shutil.copy(
            ROOT / "docs" / "linux-abi" / "evidence" / "eventfd-systemtest.txt",
            evidence_path,
        )
        evidence_path.write_text(
            evidence_path.read_text().replace("ok 17 - eventfd", "not ok 17 - eventfd")
        )
        with self.assertRaisesRegex(abi_matrix.MatrixError, "checksum drift"):
            self.validate(directory)

        catalog_path = directory / "evidence-catalog.json"
        catalog = json.loads(catalog_path.read_text())
        guest = next(item for item in catalog["evidence"] if item["lane"] == "guest-KTAP")
        guest["source_sha256"] = abi_matrix.hashlib.sha256(evidence_path.read_bytes()).hexdigest()
        catalog_path.write_text(json.dumps(catalog))
        with self.assertRaisesRegex(abi_matrix.MatrixError, "failing KTAP result"):
            self.validate(directory)


if __name__ == "__main__":
    unittest.main()
