#!/usr/bin/env python3
"""Focused checks for the Linux x86_64 ABI evidence matrix."""

from __future__ import annotations

import importlib.util
import hashlib
import json
import shutil
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

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
        destination = Path(temporary.name) / "docs" / "linux-abi"
        destination.parent.mkdir()
        shutil.copytree(ROOT / "docs" / "linux-abi", destination)
        for relative in (
            Path("kernel/src/syscall/dispatch.rs"),
            Path("kernel/src/syscall/fs/fd_ops.rs"),
            Path("kernel/src/file/event.rs"),
            Path("tests/guest/portable/creat-differential.c"),
            Path("tests/guest/portable/eventfd-differential.c"),
            Path("tests/guest/portable/native-ni-differential.c"),
        ):
            target = Path(temporary.name) / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(ROOT / relative, target)
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
        self.assertEqual(counts["unknown"], 0)
        self.assertEqual(counts["partial"], 355)
        self.assertEqual(counts["explicit-enosys"], 17)
        self.assertEqual(counts["implemented"], 3)
        self.assertEqual(counts["reviewed"], 375)
        self.assertEqual(counts["resolved"], 20)
        self.assertEqual(sum(counts[status] for status in abi_matrix.DISPOSITIONS), 375)
        matrix = json.loads((ROOT / "docs" / "linux-abi" / "syscall-matrix.json").read_text())
        eventfd_rows = {
            row["name"]: row for row in matrix["syscalls"]
            if row["name"] in {"eventfd", "eventfd2"}
        }
        inventory = json.loads((ROOT / "docs" / "linux-abi" / "static-inventory.json").read_text())
        inventory_by_name = {row["name"]: row for row in inventory["syscalls"]}
        self.assertEqual(eventfd_rows["eventfd"]["dispatch"], inventory_by_name["eventfd"]["dispatch"])
        self.assertEqual(eventfd_rows["eventfd2"]["dispatch"]["kind"], "dispatch-arm")
        self.assertTrue(all(
            row["disposition"] == "implemented" and row["review"] == "reviewed"
            for row in eventfd_rows.values()
        ))
        self.assertTrue(all(row["review"] == "reviewed" for row in matrix["syscalls"]))
        contracts = json.loads(
            (ROOT / "docs" / "linux-abi" / "contracts" / "eventfd.json").read_text()
        )
        self.assertTrue(all(
            cell["review"] == "reviewed" and cell["evidence"]
            for cell in contracts["cells"]
        ))

    def test_phase_gates_encode_review_and_linux_native_terminal_state(self) -> None:
        counts = self.validate(ROOT / "docs" / "linux-abi")
        abi_matrix.require_gate(counts, "phase1")
        with self.assertRaisesRegex(abi_matrix.MatrixError, "final gate failed"):
            abi_matrix.require_gate(counts, "final")

        terminal = {
            "reviewed": 375,
            "resolved": 375,
            "implemented": 358,
            "partial": 0,
            "explicit-enosys": 17,
            "unknown": 0,
        }
        abi_matrix.require_gate(terminal, "final")

    def test_regenerate_never_grants_review_to_an_unreviewed_row(self) -> None:
        temporary, directory = self.copy_abi()
        self.addCleanup(temporary.cleanup)
        matrix_path = directory / "syscall-matrix.json"
        document = json.loads(matrix_path.read_text())
        row = next(row for row in document["syscalls"] if row["disposition"] == "partial")
        syscall_name = row["name"]
        row["disposition"] = "unknown"
        row["review"] = "unreviewed"
        row["review_evidence"] = []
        matrix_path.write_text(json.dumps(document))

        with (
            mock.patch.object(abi_matrix, "ABI_DIR", directory),
            mock.patch.object(abi_matrix, "SNAPSHOT", directory / abi_matrix.SNAPSHOT.name),
            mock.patch.object(abi_matrix, "MATRIX", matrix_path),
        ):
            abi_matrix.regenerate()

        regenerated = json.loads(matrix_path.read_text())
        row = next(row for row in regenerated["syscalls"] if row["name"] == syscall_name)
        self.assertEqual(row["disposition"], "partial")
        self.assertEqual(row["review"], "unreviewed")
        self.assertEqual(row["review_evidence"], [])

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

    def test_rejects_static_inventory_source_binding_drift(self) -> None:
        temporary, directory = self.copy_abi()
        self.addCleanup(temporary.cleanup)
        inventory_path = directory / "static-inventory.json"
        inventory = json.loads(inventory_path.read_text())
        inventory["sources"]["dispatch"]["sha256"] = "0" * 64
        inventory_path.write_text(json.dumps(inventory))

        catalog_path = directory / "evidence-catalog.json"
        catalog = json.loads(catalog_path.read_text())
        audit = next(
            item for item in catalog["evidence"]
            if item["id"] == "matrix.static-audit.inventory-v1"
        )
        audit["source_sha256"] = hashlib.sha256(inventory_path.read_bytes()).hexdigest()
        catalog_path.write_text(json.dumps(catalog))
        with self.assertRaisesRegex(abi_matrix.MatrixError, "dispatch source hash drift"):
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

    def test_rejects_forged_host_evidence_and_cross_bound_contract(self) -> None:
        temporary, directory = self.copy_abi()
        self.addCleanup(temporary.cleanup)
        catalog_path = directory / "evidence-catalog.json"
        catalog = json.loads(catalog_path.read_text())
        host = next(item for item in catalog["evidence"] if item["lane"] == "host-unit")
        host["source_sha256"] = "0" * 64
        catalog_path.write_text(json.dumps(catalog))
        with self.assertRaisesRegex(abi_matrix.MatrixError, "checksum drift"):
            self.validate(directory)

        catalog = json.loads((ROOT / "docs/linux-abi/evidence-catalog.json").read_text())
        catalog_path.write_text(json.dumps(catalog))
        contract_path = directory / "contracts" / "eventfd.json"
        contract = json.loads(contract_path.read_text())
        contract["cells"][0]["syscalls"] = ["eventfd2"]
        contract_path.write_text(json.dumps(contract))
        with self.assertRaisesRegex(abi_matrix.MatrixError, "bound to another syscall"):
            self.validate(directory)

    def test_rejects_generic_ktap_plan_even_with_matching_hash(self) -> None:
        temporary, directory = self.copy_abi()
        self.addCleanup(temporary.cleanup)
        evidence_path = directory / "evidence" / "eventfd-systemtest.txt"
        text = evidence_path.read_text().replace("1..30", "1..1")
        text = text[:text.index("1..1")] + "1..1\nok 1 - generic-smoke\n"
        evidence_path.write_text(text)
        catalog_path = directory / "evidence-catalog.json"
        catalog = json.loads(catalog_path.read_text())
        guest = next(item for item in catalog["evidence"] if item["lane"] == "guest-KTAP")
        guest["source_sha256"] = hashlib.sha256(evidence_path.read_bytes()).hexdigest()
        guest["expected_plan"] = 1
        catalog_path.write_text(json.dumps(catalog))
        with self.assertRaisesRegex(abi_matrix.MatrixError, "declared KTAP case eventfd"):
            self.validate(directory)

    def test_rejects_review_and_disposition_invariant_violations(self) -> None:
        temporary, directory = self.copy_abi()
        self.addCleanup(temporary.cleanup)
        matrix_path = directory / "syscall-matrix.json"

        document = json.loads(matrix_path.read_text())
        row = next(row for row in document["syscalls"] if row["disposition"] == "partial")
        row["disposition"] = "unknown"
        row["gap_ids"] = []
        matrix_path.write_text(json.dumps(document))
        with self.assertRaisesRegex(abi_matrix.MatrixError, "cannot be unknown"):
            self.validate(directory)

        document = json.loads((ROOT / "docs" / "linux-abi" / "syscall-matrix.json").read_text())
        eventfd = next(row for row in document["syscalls"] if row["name"] == "eventfd")
        eventfd["handler"] = "unknown"
        matrix_path.write_text(json.dumps(document))
        with self.assertRaisesRegex(abi_matrix.MatrixError, "handler disagrees"):
            self.validate(directory)

        document = json.loads((ROOT / "docs" / "linux-abi" / "syscall-matrix.json").read_text())
        eventfd = next(row for row in document["syscalls"] if row["name"] == "eventfd")
        eventfd["review_evidence"] = []
        matrix_path.write_text(json.dumps(document))
        with self.assertRaisesRegex(abi_matrix.MatrixError, "lacks review evidence"):
            self.validate(directory)

        document = json.loads((ROOT / "docs" / "linux-abi" / "syscall-matrix.json").read_text())
        eventfd = next(row for row in document["syscalls"] if row["name"] == "eventfd")
        eventfd["disposition"] = "partial"
        eventfd["gap_ids"] = []
        matrix_path.write_text(json.dumps(document))
        with self.assertRaisesRegex(abi_matrix.MatrixError, "requires gap IDs"):
            self.validate(directory)

        document = json.loads((ROOT / "docs" / "linux-abi" / "syscall-matrix.json").read_text())
        partial = next(row for row in document["syscalls"] if row["disposition"] == "partial")
        partial["disposition"] = "implemented"
        partial["gap_ids"] = []
        matrix_path.write_text(json.dumps(document))
        with self.assertRaisesRegex(abi_matrix.MatrixError, "not evidence-resolved"):
            self.validate(directory)

    def test_rejects_enosys_direction_and_unresolved_contracts(self) -> None:
        temporary, directory = self.copy_abi()
        self.addCleanup(temporary.cleanup)
        matrix_path = directory / "syscall-matrix.json"

        document = json.loads(matrix_path.read_text())
        normal = next(row for row in document["syscalls"] if row["entry"] != "sys_ni_syscall")
        normal["disposition"] = "explicit-enosys"
        normal["gap_ids"] = []
        matrix_path.write_text(json.dumps(document))
        with self.assertRaisesRegex(abi_matrix.MatrixError, "must match sys_ni_syscall"):
            self.validate(directory)

        document = json.loads((ROOT / "docs" / "linux-abi" / "syscall-matrix.json").read_text())
        ni = next(row for row in document["syscalls"] if row["entry"] == "sys_ni_syscall")
        ni["dispatch"]["kind"] = "fallback"
        matrix_path.write_text(json.dumps(document))
        with self.assertRaisesRegex(abi_matrix.MatrixError, "native-ni dispatch must match"):
            self.validate(directory)

    def test_rejects_unknown_or_inapplicable_gap(self) -> None:
        temporary, directory = self.copy_abi()
        self.addCleanup(temporary.cleanup)
        matrix_path = directory / "syscall-matrix.json"
        document = json.loads(matrix_path.read_text())
        partial = next(row for row in document["syscalls"] if row["disposition"] == "partial")
        partial["gap_ids"].append("does-not-exist")
        matrix_path.write_text(json.dumps(document))
        with self.assertRaisesRegex(abi_matrix.MatrixError, "unknown gap IDs"):
            self.validate(directory)

        document = json.loads((ROOT / "docs" / "linux-abi" / "syscall-matrix.json").read_text())
        ni = next(row for row in document["syscalls"] if row["entry"] == "sys_ni_syscall")
        ni["gap_ids"] = ["review.dynamic-contract-evidence-unclosed"]
        matrix_path.write_text(json.dumps(document))
        with self.assertRaisesRegex(abi_matrix.MatrixError, "gap IDs do not apply"):
            self.validate(directory)

        document = json.loads((ROOT / "docs" / "linux-abi" / "syscall-matrix.json").read_text())
        ni = next(row for row in document["syscalls"] if row["entry"] == "sys_ni_syscall")
        ni["disposition"] = "unknown"
        ni["gap_ids"] = []
        matrix_path.write_text(json.dumps(document))
        with self.assertRaisesRegex(abi_matrix.MatrixError, "must match sys_ni_syscall"):
            self.validate(directory)


if __name__ == "__main__":
    unittest.main()
