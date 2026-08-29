#!/usr/bin/env python3
"""Focused checks for selecting host differential portable tests."""

from __future__ import annotations

import shutil
import subprocess
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "host-differential.sh"
NATIVE_NI = "tests/guest/portable/native-ni-differential.c"


@unittest.skipUnless(shutil.which("cc") and shutil.which("timeout"),
                     "host differential dependencies are unavailable")
class HostDifferentialSelectionTests(unittest.TestCase):
    def run_script(self, *arguments: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            (str(SCRIPT), *arguments),
            cwd=ROOT,
            capture_output=True,
            text=True,
            check=False,
        )

    def test_selects_native_ni_with_a_single_test_plan(self) -> None:
        completed = self.run_script(NATIVE_NI)

        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(completed.stdout.splitlines()[:3], [
            "KTAP version 1",
            "1..1",
            "ok 1 - native-ni-differential",
        ])
        self.assertIn(
            "# native-ni-differential: THEKERNEL_NATIVE_NI_OK",
            completed.stdout,
        )

    def test_rejects_invalid_and_duplicate_selections(self) -> None:
        for arguments in (
            ("tests/guest/portable/../system-init.c",),
            (NATIVE_NI, f"./{NATIVE_NI}"),
        ):
            with self.subTest(arguments=arguments):
                completed = self.run_script(*arguments)
                self.assertEqual(completed.returncode, 2)
                self.assertIn("host-differential:", completed.stderr)


if __name__ == "__main__":
    unittest.main()
