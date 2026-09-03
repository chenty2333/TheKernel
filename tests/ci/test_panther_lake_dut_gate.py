"""Unit tests for the fail-closed Panther Lake DUT gate."""

from __future__ import annotations

import os
import unittest
from pathlib import Path
from unittest import mock

from tests.support import load_script_module, repo_root, test_tmpdir


REPO_ROOT = repo_root()


def load_gate():
    return load_script_module(
        "thekernel_panther_lake_dut_gate", "scripts/ci/panther_lake_dut_gate.py"
    )


class PantherLakeDutGateTests(unittest.TestCase):
    def temporary_directory(self):
        return test_tmpdir()

    def test_valid_ktap_requires_complete_plan_and_completion_marker(self) -> None:
        gate = load_gate()
        with self.temporary_directory() as directory:
            log = Path(directory) / "serial.log"
            log.write_text(
                "KTAP version 1\n1..2\nok 1 - first\nok 2 - second\n"
                "# THEKERNEL_SYSTEM_TEST_COMPLETE\n",
                encoding="utf-8",
            )
            gate.validate_ktap(log)

    def test_ktap_skip_and_missing_records_fail_closed(self) -> None:
        gate = load_gate()
        with self.temporary_directory() as directory:
            log = Path(directory) / "serial.log"
            log.write_text(
                "KTAP version 1\n1..2\nok 1 - first # SKIP unavailable\n"
                "# THEKERNEL_SYSTEM_TEST_COMPLETE\n",
                encoding="utf-8",
            )
            with self.assertRaisesRegex(gate.GateError, "SKIP"):
                gate.validate_ktap(log)

    def test_artifacts_must_be_exact_nonempty_regular_files(self) -> None:
        gate = load_gate()
        with self.temporary_directory() as directory:
            artifact_dir = Path(directory) / "artifact"
            artifact_dir.mkdir()
            for name in gate.REQUIRED_ARTIFACTS:
                (artifact_dir / name).write_bytes(b"product")
            self.assertEqual(set(gate.validate_artifacts(artifact_dir)), set(gate.REQUIRED_ARTIFACTS))
            (artifact_dir / "rootfs-x86.img").write_bytes(b"")
            with self.assertRaisesRegex(gate.GateError, "empty"):
                gate.validate_artifacts(artifact_dir)

    def test_only_three_cold_boots_are_accepted(self) -> None:
        gate = load_gate()
        with self.temporary_directory() as directory:
            root = Path(directory)
            artifact_dir = root / "artifact"
            artifact_dir.mkdir()
            for name in gate.REQUIRED_ARTIFACTS:
                (artifact_dir / name).write_bytes(b"product")
            with self.assertRaisesRegex(gate.GateError, "exactly three"):
                gate.run_gate(artifact_dir, root / "state", runs=2)

    def test_three_runs_require_the_serial_hook_to_attest_clean_shutdown(self) -> None:
        gate = load_gate()
        with self.temporary_directory() as directory:
            root = Path(directory)
            artifact_dir = root / "artifact"
            artifact_dir.mkdir()
            for name in gate.REQUIRED_ARTIFACTS:
                (artifact_dir / name).write_bytes(b"product")
            serial_hook = (
                "printf '%s\\n' 'KTAP version 1' '1..1' 'ok 1 - complete' "
                "'# THEKERNEL_SYSTEM_TEST_COMPLETE' > \"$THEKERNEL_DUT_SERIAL_LOG\"; "
                "printf clean > \"$THEKERNEL_DUT_SHUTDOWN_STATUS\""
            )
            hooks = {
                "THEKERNEL_DUT_POWER_CYCLE_CMD": "true",
                "THEKERNEL_DUT_BOOT_ONCE_CMD": "true",
                "THEKERNEL_DUT_SERIAL_CAPTURE_CMD": serial_hook,
            }
            with mock.patch.dict(os.environ, hooks, clear=False):
                gate.run_gate(artifact_dir, root / "state", runs=3)
            self.assertTrue((root / "state" / "cold-boot-3.serial.log").is_file())


if __name__ == "__main__":
    unittest.main()
