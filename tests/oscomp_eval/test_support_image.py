from __future__ import annotations

import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from tools.oscomp_eval.support_image import (
    SupportImageError,
    build_support_image,
    inspect_support_image,
)


class SupportImageTests(unittest.TestCase):
    def test_inspect_support_image_accepts_current_runner_and_timeout_tool(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "src").mkdir()
            (root / "src" / "init.sh").write_text("#!/bin/sh\necho ok\n", encoding="utf-8")
            image = root / "disk-rv.img"
            image.write_bytes(b"fake ext image")

            def fake_debugfs(image_path: Path, command: str) -> subprocess.CompletedProcess[str]:
                if command == "cat /meta/init.sh":
                    return subprocess.CompletedProcess([], 0, stdout="#!/bin/sh\necho ok\n", stderr="")
                if command in (
                    "stat /meta/ltp_test.txt",
                    "stat /rv/overlay/bin/oscomp-timeout",
                ):
                    return subprocess.CompletedProcess([], 0, stdout="Inode: 1\n", stderr="")
                return subprocess.CompletedProcess([], 1, stdout="", stderr="missing")

            with patch("tools.oscomp_eval.support_image._debugfs_capture", side_effect=fake_debugfs):
                result = inspect_support_image(arch="rv", image=image, root=root)

            self.assertTrue(result.ok)
            self.assertEqual(result.issues, ())

    def test_inspect_support_image_reports_stale_runner_and_missing_timeout_tool(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "src").mkdir()
            (root / "src" / "init.sh").write_text("#!/bin/sh\necho current\n", encoding="utf-8")
            image = root / "disk-la.img"
            image.write_bytes(b"fake ext image")

            def fake_debugfs(image_path: Path, command: str) -> subprocess.CompletedProcess[str]:
                if command == "cat /meta/init.sh":
                    return subprocess.CompletedProcess([], 0, stdout="#!/bin/sh\necho old\n", stderr="")
                if command == "stat /meta/ltp_test.txt":
                    return subprocess.CompletedProcess([], 0, stdout="Inode: 1\n", stderr="")
                if command == "stat /la/overlay/bin/oscomp-timeout":
                    return subprocess.CompletedProcess([], 1, stdout="", stderr="File not found")
                return subprocess.CompletedProcess([], 1, stdout="", stderr="missing")

            with patch("tools.oscomp_eval.support_image._debugfs_capture", side_effect=fake_debugfs):
                result = inspect_support_image(arch="la", image=image, root=root)

            self.assertFalse(result.ok)
            self.assertIn("/meta/init.sh does not match current src/init.sh", result.issues)
            self.assertIn(
                "missing /la/overlay/bin/oscomp-timeout: File not found",
                result.issues,
            )

    def test_build_support_image_wires_ltp_list_and_plan(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            ltp_list = root / "ltp_test.txt"
            plan = root / "plan.txt"
            ltp_list.write_text("fork06\n", encoding="utf-8")
            plan.write_text("/glibc ltp\n", encoding="utf-8")
            (root / "scripts").mkdir()
            builder = root / "scripts" / "build-oscomp-support-disk.sh"
            builder.write_text("#!/usr/bin/env bash\n", encoding="utf-8")

            def fake_run(command: list[str], check: bool) -> subprocess.CompletedProcess[str]:
                self.assertFalse(check)
                output = Path(command[command.index("--output") + 1])
                output.write_bytes(b"support image")
                return subprocess.CompletedProcess(command, 0)

            with patch("tools.oscomp_eval.support_image.repo_root", return_value=root):
                with patch("tools.oscomp_eval.support_image.subprocess.run", side_effect=fake_run):
                    result = build_support_image(
                        arch="rv",
                        run_dir=root / "run",
                        ltp_list=ltp_list,
                        plan=plan,
                    )

            self.assertEqual(result.arch, "rv")
            self.assertEqual(result.returncode, 0)
            self.assertEqual(result.output_path, root / "run" / "inputs" / "support-rv.img")
            self.assertIn("--test-list", result.command)
            self.assertIn(str(ltp_list), result.command)
            self.assertIn("--plan-override", result.command)
            self.assertIn(str(plan), result.command)

    def test_missing_ltp_list_is_structured_error(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            with self.assertRaisesRegex(SupportImageError, "ltp list does not exist"):
                build_support_image(
                    arch="rv",
                    run_dir=root / "run",
                    ltp_list=root / "missing.txt",
                )


if __name__ == "__main__":
    unittest.main()
