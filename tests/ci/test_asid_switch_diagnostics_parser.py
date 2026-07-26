#!/usr/bin/env python3

from __future__ import annotations

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
PARSER = REPO_ROOT / "scripts" / "ci" / "parse-asid-switch-diagnostics.py"
VALID = (
    "ASID_SWITCH_DIAGNOSTICS "
    "schema=thekernel-asid-switch-diagnostics-v1 enabled=0 "
    "fast_path_avoided=12 fallback_asid_zero=1 fallback_invalid_width=0 "
    "fallback_exhausted=2 fallback_generation_mismatch=3 "
    "fallback_same_id_different_root=4 saturated=0\n"
)


class AsidSwitchDiagnosticsParserTests(unittest.TestCase):
    def run_parser(self, payload: str) -> subprocess.CompletedProcess[str]:
        with tempfile.TemporaryDirectory() as temporary:
            log = Path(temporary) / "qemu.log"
            log.write_text(payload, encoding="utf-8")
            return subprocess.run(
                [sys.executable, str(PARSER), str(log)],
                cwd=REPO_ROOT,
                check=False,
                capture_output=True,
                text=True,
            )

    def test_normalizes_disabled_nonempty_snapshot(self) -> None:
        result = self.run_parser("boot noise\n" + VALID + "shutdown noise\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        lines = result.stdout.splitlines()
        self.assertEqual(len(lines), 2)
        self.assertEqual(lines[0].split("\t")[0], "schema")
        self.assertIn("\t0\t12\t1\t0\t2\t3\t4\t0", lines[1])

    def test_rejects_enabled_or_empty_capture(self) -> None:
        enabled = self.run_parser(VALID.replace("enabled=0", "enabled=1"))
        self.assertEqual(enabled.returncode, 1)
        self.assertIn("must be disabled", enabled.stderr)

        empty = self.run_parser(
            VALID.replace("fast_path_avoided=12", "fast_path_avoided=0")
            .replace("fallback_asid_zero=1", "fallback_asid_zero=0")
            .replace("fallback_exhausted=2", "fallback_exhausted=0")
            .replace(
                "fallback_generation_mismatch=3",
                "fallback_generation_mismatch=0",
            )
            .replace(
                "fallback_same_id_different_root=4",
                "fallback_same_id_different_root=0",
            )
        )
        self.assertEqual(empty.returncode, 1)
        self.assertIn("recorded no switch decisions", empty.stderr)

    def test_rejects_duplicate_snapshot(self) -> None:
        result = self.run_parser(VALID + VALID)
        self.assertEqual(result.returncode, 1)
        self.assertIn("expected one", result.stderr)


if __name__ == "__main__":
    unittest.main()
