#!/usr/bin/env python3
"""Focused tests for the Gate 0 UAPI surface and exposure inventories."""
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
SPEC = importlib.util.spec_from_file_location("abi_surfaces", ROOT / "tools/abi_surfaces.py")
assert SPEC and SPEC.loader
abi_surfaces = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = abi_surfaces
SPEC.loader.exec_module(abi_surfaces)


class AbiSurfaceTests(unittest.TestCase):
    def documents(self) -> tuple[dict, dict]:
        return (
            json.loads((ROOT / "docs/linux-abi/uapi-surfaces-v1.json").read_text()),
            json.loads((ROOT / "docs/linux-abi/exposure-inventory-v1.json").read_text()),
        )

    def write_document(self, path: Path, document: dict) -> None:
        document["canonical_hash"] = abi_surfaces.canonical(document)
        path.write_text(json.dumps(document))

    def test_checked_in_documents_are_complete_and_explicit(self) -> None:
        result = abi_surfaces.validate()
        surface, _ = self.documents()
        self.assertEqual(len(surface["syscalls"]), 375)
        self.assertEqual([row["nr"] for row in surface["syscalls"]], sorted(row["nr"] for row in surface["syscalls"]))
        self.assertEqual({row["name"] for row in surface["syscalls"] if row["applicability"] == "N/A"}, {
            "uselib", "_sysctl", "create_module", "get_kernel_syms", "query_module", "nfsservctl",
            "getpmsg", "putpmsg", "afs_syscall", "tuxcall", "security", "set_thread_area",
            "get_thread_area", "lookup_dcookie", "epoll_ctl_old", "epoll_wait_old", "vserver",
        })
        self.assertEqual(set(result["mapped"]), {
            "creat", "eventfd", "eventfd2", "uselib", "_sysctl", "create_module", "get_kernel_syms",
            "query_module", "nfsservctl", "getpmsg", "putpmsg", "afs_syscall", "tuxcall", "security",
            "set_thread_area", "get_thread_area", "lookup_dcookie", "epoll_ctl_old", "epoll_wait_old", "vserver",
        })
        self.assertTrue(all(set(row) == abi_surfaces.ROW_FIELDS for row in surface["syscalls"]))

    def test_closures_bind_the_real_exposure_rows(self) -> None:
        surface, exposure = self.documents()
        rows = {row["name"]: row for row in surface["syscalls"]}
        self.assertEqual(rows["eventfd"]["closure"]["exposures"], ["eventfd-object"])
        self.assertEqual(rows["eventfd2"]["closure"]["exposures"], ["eventfd-object"])
        self.assertEqual(rows["creat"]["closure"]["exposures"], ["regular-file"])
        with tempfile.TemporaryDirectory() as directory:
            old = Path(directory) / "old.json"
            new = Path(directory) / "new.json"
            self.write_document(old, copy.deepcopy(exposure))
            changed = copy.deepcopy(exposure)
            changed["exposures"][0]["class"] = "changed eventfd counter file descriptor"
            self.write_document(new, changed)
            self.assertEqual(abi_surfaces.affected_rows(abi_surfaces.SURFACES, old, new), ["eventfd", "eventfd2"])

    def test_rejects_unknown_reference_and_hash_or_source_drift(self) -> None:
        surface, exposure = self.documents()
        with tempfile.TemporaryDirectory() as directory:
            directory = Path(directory)
            bad_surface = copy.deepcopy(surface)
            bad_surface["syscalls"][0]["closure"]["exposures"] = ["not-an-exposure"]
            self.write_document(directory / "surface.json", bad_surface)
            self.write_document(directory / "exposure.json", copy.deepcopy(exposure))
            with self.assertRaisesRegex(abi_surfaces.AbiSurfaceError, "unknown exposure"):
                abi_surfaces.validate(directory / "surface.json", directory / "exposure.json")
            bad_exposure = copy.deepcopy(exposure)
            bad_exposure["sources"][0]["sha256"] = "0" * 64
            self.write_document(directory / "exposure.json", bad_exposure)
            with self.assertRaisesRegex(abi_surfaces.AbiSurfaceError, "source hash drift"):
                abi_surfaces.validate(abi_surfaces.SURFACES, directory / "exposure.json")
            bad_hash = copy.deepcopy(surface)
            bad_hash["canonical_hash"] = "0" * 64
            (directory / "surface.json").write_text(json.dumps(bad_hash))
            with self.assertRaisesRegex(abi_surfaces.AbiSurfaceError, "canonical hash drift"):
                abi_surfaces.validate(directory / "surface.json", abi_surfaces.EXPOSURES)

    def test_cli_reports_mapped_and_unmapped(self) -> None:
        completed = subprocess.run(
            (sys.executable, str(ROOT / "tools/abi_surfaces.py"), "validate"),
            check=True, capture_output=True, text=True,
        )
        result = json.loads(completed.stdout)
        self.assertIn("eventfd", result["mapped"])
        self.assertIn("read", result["unmapped"])


if __name__ == "__main__":
    unittest.main()
