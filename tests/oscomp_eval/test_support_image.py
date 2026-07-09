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
    def test_inspect_support_image_accepts_embedded_runner_and_timeout_tool(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "src").mkdir()
            (root / "src" / "init.sh").write_text("#!/bin/sh\necho ok\n", encoding="utf-8")
            image = root / "disk.img"
            image.write_bytes(b"fake ext image")

            def fake_debugfs(image_path: Path, command: str) -> subprocess.CompletedProcess[str]:
                if command == "stat /meta/init.sh":
                    return subprocess.CompletedProcess([], 1, stdout="", stderr="File not found")
                if command == "cat /meta/init.sh":
                    return subprocess.CompletedProcess([], 1, stdout="", stderr="File not found")
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

    def test_inspect_support_image_reports_stale_optional_runner_and_missing_timeout_tool(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "src").mkdir()
            (root / "src" / "init.sh").write_text("#!/bin/sh\necho current\n", encoding="utf-8")
            image = root / "disk-la.img"
            image.write_bytes(b"fake ext image")

            def fake_debugfs(image_path: Path, command: str) -> subprocess.CompletedProcess[str]:
                if command == "stat /meta/init.sh":
                    return subprocess.CompletedProcess([], 0, stdout="Inode: 1\n", stderr="")
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
            self.assertIn("optional /meta/init.sh does not match current src/init.sh", result.issues)
            self.assertIn(
                "missing /la/overlay/bin/oscomp-timeout: File not found",
                result.issues,
            )

    def test_build_support_image_uses_content_pool(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            ltp_list = root / "ltp_test.txt"
            plan = root / "plan.txt"
            ltp_list.write_text("fork06\n", encoding="utf-8")
            plan.write_text("/glibc ltp\n", encoding="utf-8")
            (root / "scripts" / "support-tools").mkdir(parents=True)
            (root / "scripts" / "support-overlay").mkdir(parents=True)
            (root / "scripts" / "build-oscomp-support-disk.sh").write_text("#!/bin/sh\n", encoding="utf-8")
            (root / "scripts" / "support-tools" / "t.c").write_text("x\n", encoding="utf-8")
            (root / "scripts" / "support-overlay" / "n").write_text("y\n", encoding="utf-8")
            (root / "tools").mkdir()
            (root / "tools" / "build.py").write_text("# build helper\n", encoding="utf-8")

            def fake_build(req, output: Path, **_: object):
                output.parent.mkdir(parents=True, exist_ok=True)
                output.write_bytes(b"support image")

            with patch(
                "tools.build.support_disk_build",
                side_effect=fake_build,
            ):
                result = build_support_image(
                    arch="rv",
                    run_dir=root / "run",
                    ltp_list=ltp_list,
                    plan=plan,
                    root=root,
                )

            self.assertEqual(result.arch, "rv")
            self.assertEqual(result.returncode, 0)
            self.assertFalse(result.hit)
            self.assertIn(".state/build-cache/support-disks", str(result.output_path))
            self.assertTrue(result.output_path.is_file())
            self.assertFalse((root / "run" / "inputs" / "support-rv.img").exists())
            self.assertTrue(result.identity)

    def test_missing_ltp_list_is_structured_error(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            with self.assertRaisesRegex(SupportImageError, "ltp list does not exist"):
                build_support_image(
                    arch="rv",
                    run_dir=root / "run",
                    ltp_list=root / "missing.txt",
                    root=root,
                )


if __name__ == "__main__":
    unittest.main()
