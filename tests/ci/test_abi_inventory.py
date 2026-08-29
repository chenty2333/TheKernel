#!/usr/bin/env python3
"""Focused checks for the source-only ABI route inventory."""
from __future__ import annotations

import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("abi_inventory", ROOT / "tools/abi_inventory.py")
assert SPEC and SPEC.loader
abi_inventory = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = abi_inventory
SPEC.loader.exec_module(abi_inventory)


class AbiInventoryTests(unittest.TestCase):
    def test_rejects_malformed_table_rows_with_inventory_error(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "syscall_64.tbl"
            path.write_text("999\n")
            with self.assertRaisesRegex(abi_inventory.InventoryError, "malformed syscall row"):
                abi_inventory.table_rows(path)
            path.write_text("not-a-number common read sys_read\n")
            with self.assertRaisesRegex(abi_inventory.InventoryError, "invalid syscall number"):
                abi_inventory.table_rows(path)

    def test_checked_in_inventory_is_deterministic_and_complete(self) -> None:
        generated = abi_inventory.generate()
        checked_in = json.loads((ROOT / "docs/linux-abi/static-inventory.json").read_text())
        self.assertEqual(generated, checked_in)
        self.assertEqual(generated["syscall_count"], 375)
        self.assertEqual(generated["linux_ni_count"], 17)
        self.assertEqual({row["linux_route"] for row in generated["syscalls"]}, {"direct", "conditional", "ni"})
        self.assertTrue(all(row["uapi_family"] != "unclassified" for row in generated["syscalls"]))
        self.assertTrue(all(gap["reason"] for gap in generated["static_gaps"]))
        self.assertEqual(generated["dispatcher_arm_count"], abi_inventory.EXPECTED_DISPATCH_ARM_COUNT)
        self.assertEqual(set(generated["missing_dispatch_syscalls"]), abi_inventory.EXPECTED_MISSING_DISPATCH)
        self.assertEqual(
            generated["sources"]["linux_cond_syscall"]["conditional_syscall_count"],
            abi_inventory.EXPECTED_COND_SYSCALL_COUNT,
        )

    def test_static_dispatch_forms_are_preserved(self) -> None:
        rows = {row["name"]: row for row in abi_inventory.generate()["syscalls"]}
        self.assertEqual(rows["eventfd"]["dispatch"]["kind"], "alias")
        self.assertEqual(rows["eventfd"]["implementation_root"], "sys_eventfd2")
        self.assertEqual(rows["bpf"]["dispatch"]["kind"], "feature")
        self.assertIn("feature=bpf", rows["bpf"]["static_gap"])
        self.assertEqual(rows["perf_event_open"]["dispatch"]["kind"], "fallback")
        self.assertIn("unsupported-fd", rows["perf_event_open"]["static_gap"])
        native_ni = [row for row in rows.values() if row["linux_route"] == "ni"]
        self.assertEqual(len(native_ni), abi_inventory.NI_COUNT)
        self.assertTrue(all(row["dispatch"] == {
            "kind": "native-ni", "target": "sys_ni_syscall"
        } for row in native_ni))
        self.assertEqual(rows["uretprobe"]["dispatch"]["kind"], "fallback")

    def test_fixed_cond_syscall_set_marks_known_config_gates(self) -> None:
        rows = {row["name"]: row for row in abi_inventory.generate()["syscalls"]}
        for name in ("init_module", "delete_module", "finit_module", "kexec_load",
                     "kexec_file_load", "perf_event_open", "keyctl", "bpf"):
            self.assertEqual(rows[name]["linux_route"], "conditional", name)
        self.assertEqual(rows["io_uring_setup"]["uapi_family"], "async-io")
        self.assertEqual(rows["io_setup"]["uapi_family"], "async-io")
        self.assertEqual(rows["mount"]["uapi_family"], "mount")
        self.assertEqual(rows["setns"]["uapi_family"], "namespace")
        self.assertEqual(rows["bpf"]["uapi_family"], "security")
        self.assertEqual(rows["kexec_load"]["uapi_family"], "admin")


if __name__ == "__main__":
    unittest.main()
