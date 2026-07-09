from __future__ import annotations

import subprocess
import tempfile
import unittest
from contextlib import redirect_stderr
from io import StringIO
from pathlib import Path
from unittest.mock import patch

from tools.build import (
    FileDigestStore,
    SupportDiskRequest,
    ensure_support_disk,
    hash_params,
    kernel_params,
    main,
    KernelRequest,
)
from tools.oscomp_eval.paths import repo_root


def _minimal_support_tree(root: Path) -> None:
    (root / "scripts" / "support-tools").mkdir(parents=True)
    (root / "scripts" / "support-overlay").mkdir(parents=True)
    (root / "scripts" / "build-oscomp-support-disk.sh").write_text("#!/bin/sh\n", encoding="utf-8")
    (root / "scripts" / "support-tools" / "tool.c").write_text("int main(){}\n", encoding="utf-8")
    (root / "scripts" / "support-overlay" / "note").write_text("overlay\n", encoding="utf-8")
    (root / "tools").mkdir()
    (root / "tools" / "build.py").write_text("# build helper\n", encoding="utf-8")
    (root / "ltp_test.txt").write_text("fork06\n", encoding="utf-8")


class BuildSupportDiskCacheTests(unittest.TestCase):
    def test_params_change_causes_miss(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _minimal_support_tree(root)
            builds: list[Path] = []

            def fake_build(req: SupportDiskRequest, output: Path, **_: object) -> None:
                builds.append(output)
                output.parent.mkdir(parents=True, exist_ok=True)
                output.write_bytes(b"image-v1")

            with patch("tools.build.support_disk_build", side_effect=fake_build):
                first = ensure_support_disk(arch="rv", root=root, output=root / "disk.img")
                second = ensure_support_disk(arch="rv", root=root, output=root / "disk.img")
                third = ensure_support_disk(arch="la", root=root, output=root / "disk-la.img")

            self.assertFalse(first.hit)
            self.assertTrue(second.hit)
            self.assertFalse(third.hit)
            self.assertEqual(len(builds), 2)
            self.assertIn(".state/build-cache/support-disks", str(first.cache_path))
            self.assertTrue((root / "disk.img").is_file())
            self.assertFalse((root / ".state" / "build-cache" / "records").exists())

    def test_touch_same_content_is_hit(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _minimal_support_tree(root)
            builds: list[int] = []

            def fake_build(req: SupportDiskRequest, output: Path, **_: object) -> None:
                builds.append(1)
                output.parent.mkdir(parents=True, exist_ok=True)
                output.write_bytes(b"image")

            with patch("tools.build.support_disk_build", side_effect=fake_build):
                ensure_support_disk(arch="rv", root=root, output=root / "disk.img")
                (root / "scripts" / "support-tools" / "tool.c").touch()
                result = ensure_support_disk(arch="rv", root=root, output=root / "disk.img")

            self.assertTrue(result.hit)
            self.assertEqual(len(builds), 1)

    def test_overlay_content_change_is_miss(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _minimal_support_tree(root)
            builds: list[int] = []

            def fake_build(req: SupportDiskRequest, output: Path, **_: object) -> None:
                builds.append(1)
                output.parent.mkdir(parents=True, exist_ok=True)
                output.write_bytes(b"image")

            with patch("tools.build.support_disk_build", side_effect=fake_build):
                ensure_support_disk(arch="rv", root=root, output=root / "disk.img")
                (root / "scripts" / "support-overlay" / "note").write_text("changed\n", encoding="utf-8")
                result = ensure_support_disk(arch="rv", root=root, output=root / "disk.img")

            self.assertFalse(result.hit)
            self.assertEqual(len(builds), 2)

    def test_identity_ignores_materialized_output_path(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _minimal_support_tree(root)

            def fake_build(req: SupportDiskRequest, output: Path, **_: object) -> None:
                output.parent.mkdir(parents=True, exist_ok=True)
                output.write_bytes(b"image")

            with patch("tools.build.support_disk_build", side_effect=fake_build):
                a = ensure_support_disk(arch="rv", root=root, output=root / "a.img")
                b = ensure_support_disk(arch="rv", root=root, output=root / "b.img")

            self.assertEqual(a.identity, b.identity)
            self.assertEqual(a.cache_path, b.cache_path)
            self.assertTrue(b.hit)

    def test_file_digest_store_reuses_hash(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            db = root / "digests.sqlite"
            store = FileDigestStore(db)
            path = root / "f.txt"
            path.write_text("hello\n", encoding="utf-8")
            first = store.digest_file(path)
            second = store.digest_file(path)
            self.assertEqual(first, second)
            path.write_text("hello!\n", encoding="utf-8")
            third = store.digest_file(path)
            self.assertNotEqual(first, third)
            store.close()


class BuildKernelParamTests(unittest.TestCase):
    def test_kernel_params_include_fixed_profile(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            base = dict(
                name="kernel-rv",
                arch="riscv64",
                make_args=("BUS=mmio",),
                app_features="qemu",
                patch_script="scripts/patch-riscv-kernel-elf.py",
                root=root,
            )
            params = kernel_params(KernelRequest(**base))
            self.assertEqual(params["make.DEBUGINFO"], "y")
            self.assertEqual(params["make.LOG"], "off")
            self.assertEqual(params["strip"], "rust-objcopy --strip-all")
            self.assertEqual(hash_params(params), hash_params(dict(params)))


class BuildCliTests(unittest.TestCase):
    def test_help_documents_short_commands(self) -> None:
        root = repo_root()
        result = subprocess.run(
            ["python3", "tools/build.py", "--help"],
            cwd=root,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
            env={**__import__("os").environ, "PYTHONPATH": str(root)},
        )
        self.assertEqual(result.returncode, 0, msg=result.stderr)
        self.assertIn("kernel", result.stdout)
        self.assertIn("support", result.stdout)
        self.assertNotIn("eval-kernel", result.stdout)
        self.assertNotIn("support-disk", result.stdout)
        self.assertNotIn("--recipe", result.stdout)

    def test_main_rejects_missing_ltp_list(self) -> None:
        with patch("tools.build.find_repo_root", return_value=Path("/tmp/missing-thekernel")):
            stderr = StringIO()
            with redirect_stderr(stderr):
                code = main(["support", "rv", "--ltp-list", "/tmp/does-not-exist"])
        self.assertEqual(code, 2)
        self.assertIn("ltp list does not exist", stderr.getvalue())


if __name__ == "__main__":
    unittest.main()
