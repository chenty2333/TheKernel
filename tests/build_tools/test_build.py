from __future__ import annotations

import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from tools.build import (
    FileDigestStore,
    RootfsRequest,
    ensure_rootfs,
    hash_params,
    kernel_params,
    KernelRequest,
    make_kernel_request,
    rootfs_params,
)
from tools.project_paths import repo_root


class BuildCacheTests(unittest.TestCase):
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

    def test_rootfs_identity_ignores_materialized_output_path(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "scripts").mkdir()
            (root / "tests" / "guest").mkdir(parents=True)
            (root / "tests" / "rootfs").mkdir(parents=True)
            (root / "tools").mkdir()
            (root / "dev-env").mkdir()
            (root / "scripts" / "build-rootfs.sh").write_text(
                "#!/bin/sh\n", encoding="utf-8"
            )
            (root / "tests" / "guest" / "shell-init.sh").write_text(
                "#!/bin/sh\n", encoding="utf-8"
            )
            busybox_config = root / "tests" / "rootfs" / "busybox-1.36.1.config"
            busybox_config.write_text("CONFIG_STATIC=y\n", encoding="utf-8")
            (root / "tools" / "build.py").write_text(
                "# builder\n", encoding="utf-8"
            )
            for metadata in ("LICENSE", "NOTICE", "PROVENANCE.md"):
                (root / metadata).write_text(f"{metadata}\n", encoding="utf-8")
            (root / "dev-env" / "Dockerfile").write_text(
                "FROM scratch\n", encoding="utf-8"
            )
            (root / "dev-env" / "versions.env").write_text(
                "TOOLCHAIN_VERSION=test\n", encoding="utf-8"
            )

            builds: list[Path] = []

            def fake_build(req: RootfsRequest, output: Path, **_: object) -> None:
                builds.append(output)
                output.parent.mkdir(parents=True, exist_ok=True)
                output.write_bytes(b"rootfs")

            with patch("tools.build.rootfs_build", side_effect=fake_build):
                first = ensure_rootfs(arch="rv", root=root, output=root / "a.img")
                second = ensure_rootfs(arch="rv", root=root, output=root / "b.img")
                busybox_config.write_text(
                    "CONFIG_STATIC=y\nCONFIG_FEATURE_TEST=y\n", encoding="utf-8"
                )
                third = ensure_rootfs(arch="rv", root=root, output=root / "c.img")

            self.assertEqual(first.identity, second.identity)
            self.assertEqual(first.cache_path, second.cache_path)
            self.assertFalse(first.hit)
            self.assertTrue(second.hit)
            self.assertNotEqual(second.identity, third.identity)
            self.assertFalse(third.hit)
            self.assertEqual(len(builds), 2)


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

    def test_rootfs_params_normalize_source_date_epoch(self) -> None:
        with tempfile.TemporaryDirectory() as tmp, patch(
            "tools.build.capture", return_value="tool-version"
        ):
            request = RootfsRequest(arch="rv", root=Path(tmp))
            with patch.dict("os.environ", {"SOURCE_DATE_EPOCH": ""}):
                self.assertEqual(rootfs_params(request)["source_date_epoch"], "1704067200")
            with patch.dict("os.environ", {"SOURCE_DATE_EPOCH": "123456789"}):
                self.assertEqual(rootfs_params(request)["source_date_epoch"], "123456789")

    def test_io_test_control_is_absent_from_product_kernel_profiles(self) -> None:
        root = repo_root()
        for mode in ("release", "shell"):
            request = make_kernel_request(mode, "rv", root)
            self.assertNotIn("test-io-control", request.app_features.split())

        test_request = make_kernel_request("io-test-shell", "rv", root)
        self.assertIn("test-io-control", test_request.app_features.split())
        self.assertNotEqual(
            test_request.name, make_kernel_request("shell", "rv", root).name
        )


class ProductBootBoundaryTests(unittest.TestCase):
    def test_proc_io_stats_is_read_only_and_test_controls_are_feature_gated(self) -> None:
        root = repo_root()
        proc_source = (root / "kernel" / "src" / "pseudofs" / "proc.rs").read_text(
            encoding="utf-8"
        )
        io_stats = proc_source.index('"io_stats"')
        io_test = proc_source.index('"io_test_control"')
        self.assertIn("new_regular_with_permission", proc_source[io_stats:io_test])
        self.assertIn("0o444", proc_source[io_stats:io_test])
        self.assertNotIn("SimpleFileOperation::Write", proc_source[io_stats:io_test])
        self.assertIn(
            '#[cfg(feature = "test-io-control")]\n    root.add(',
            proc_source[io_stats:io_test],
        )

        control_source = (
            root / "kernel" / "src" / "pseudofs" / "io_test_control.rs"
        ).read_text(encoding="utf-8")
        self.assertIn("0o600", control_source)
        self.assertIn("async_block_selftest_rw_scratch", control_source)
        self.assertNotIn('"user_direct_async=on" =>', control_source)

    def test_release_and_shell_kernels_have_distinct_init_processes(self) -> None:
        root = repo_root()
        source = (root / "src" / "main.rs").read_text(encoding="utf-8")
        self.assertIn('SYSTEM_CMDLINE: &[&str] = &["/sbin/init"]', source)
        self.assertIn(
            'SHELL_CMDLINE: &[&str] = &["/bin/busybox", "sh", '
            '"/etc/thekernel/shell-init.sh"]',
            source,
        )
        self.assertNotIn("THEKERNEL_BOOT_MODE", source)

    def test_rootfs_installs_a_real_system_init_after_busybox(self) -> None:
        root = repo_root()
        source = (root / "scripts" / "build-rootfs.sh").read_text(encoding="utf-8")
        remove = source.index('rm -f "$STAGE/sbin/init"')
        compile_init = source.index('-o "$STAGE/sbin/init"')
        self.assertLess(remove, compile_init)
        self.assertIn("tests/guest/shell-init.sh", source)
        self.assertNotIn("tests/guest/init.sh", source)
        self.assertIn("tests/rootfs/busybox-${BUSYBOX_VERSION}.config", source)
        self.assertIn("silentoldconfig", source)
        self.assertNotIn(" defconfig", source)
        self.assertIn("mke2fs -q -F -t ext4 -b 4096", source)

    def test_smoke_scripts_define_a_default_workdir(self) -> None:
        root = repo_root()
        for script in sorted((root / "scripts" / "smoke").glob("*-smoke.sh")):
            source = script.read_text(encoding="utf-8")
            if 'WORKDIR=""' in source:
                self.assertIn('if [ -z "$WORKDIR" ]', source, msg=script.name)

    def test_smoke_scripts_validate_cli_before_building(self) -> None:
        root = repo_root()
        scripts = sorted((root / "scripts" / "smoke").glob("*-smoke.sh"))
        for script in scripts:
            for args, error in (
                (("--arch", "invalid"), "--arch must be rv or la"),
                (("--timeout", "invalid"), "--timeout must be a non-negative integer"),
            ):
                result = subprocess.run(
                    [str(script), *args],
                    cwd=root,
                    text=True,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    check=False,
                )
                self.assertEqual(result.returncode, 1, msg=script.name)
                self.assertIn(error, result.stderr, msg=script.name)

        for name in ("lwext4-async-read-smoke.sh",):
            script = root / "scripts" / "smoke" / name
            result = subprocess.run(
                [str(script), "--wait-policy", "invalid"],
                cwd=root,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
            )
            self.assertEqual(result.returncode, 1, msg=name)
            self.assertIn("--wait-policy must be hybrid or irq_first", result.stderr)

    def test_smoke_kernel_path_excludes_build_stdout(self) -> None:
        root = repo_root()
        with tempfile.TemporaryDirectory() as tmp:
            temp = Path(tmp)
            fake_bin = temp / "bin"
            fake_bin.mkdir()
            fake_make = fake_bin / "make"
            fake_make.write_text(
                "#!/bin/sh\n"
                "printf 'fake make stdout\\n'\n"
                "printf 'fake make stderr\\n' >&2\n",
                encoding="utf-8",
            )
            fake_make.chmod(0o755)
            fake_repo = temp / "repo"
            env = dict(__import__("os").environ)
            env["PATH"] = f"{fake_bin}:{env['PATH']}"
            result = subprocess.run(
                [
                    "bash",
                    "-c",
                    'set -euo pipefail; source "$1"; REPO_ROOT="$2"; '
                    "smoke_runner_artifact_args rv 0",
                    "smoke-helper-test",
                    str(root / "scripts" / "smoke" / "lib.sh"),
                    str(fake_repo),
                ],
                cwd=root,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
                env=env,
            )
            self.assertEqual(result.returncode, 0, msg=result.stderr.decode())
            self.assertEqual(
                result.stdout,
                b"--kernel\0"
                + str(fake_repo / ".state/io-test-shell/kernel-rv").encode()
                + b"\0",
            )
            self.assertIn(b"fake make stdout", result.stderr)
            self.assertIn(b"fake make stderr", result.stderr)

    def test_smoke_kernel_build_failure_is_propagated(self) -> None:
        root = repo_root()
        with tempfile.TemporaryDirectory() as tmp:
            temp = Path(tmp)
            fake_bin = temp / "bin"
            fake_bin.mkdir()
            fake_make = fake_bin / "make"
            fake_make.write_text(
                "#!/bin/sh\n"
                "printf 'fake make stdout before failure\\n'\n"
                "printf 'fake make stderr before failure\\n' >&2\n"
                "exit 23\n",
                encoding="utf-8",
            )
            fake_make.chmod(0o755)
            fake_repo = temp / "repo"
            env = dict(__import__("os").environ)
            env["PATH"] = f"{fake_bin}:{env['PATH']}"
            result = subprocess.run(
                [
                    "bash",
                    "-c",
                    'set -euo pipefail; source "$1"; REPO_ROOT="$2"; '
                    "smoke_runner_artifact_args rv 0",
                    "smoke-helper-test",
                    str(root / "scripts" / "smoke" / "lib.sh"),
                    str(fake_repo),
                ],
                cwd=root,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
                env=env,
            )
            self.assertEqual(result.returncode, 23, msg=result.stderr.decode())
            self.assertEqual(result.stdout, b"")
            self.assertIn(b"fake make stdout before failure", result.stderr)
            self.assertIn(b"fake make stderr before failure", result.stderr)


class BuildCliTests(unittest.TestCase):
    def test_help_documents_product_commands(self) -> None:
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
        self.assertIn("shell", result.stdout)
        self.assertIn("rootfs", result.stdout)

    def test_clean_dry_run_removes_outputs_but_preserves_caches(self) -> None:
        root = repo_root()
        result = subprocess.run(
            ["make", "--no-print-directory", "-n", "clean"],
            cwd=root,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        self.assertEqual(result.returncode, 0, msg=result.stderr)
        for path in (
            root / ".state" / "ci",
            root / ".state" / "rootfs",
            root / ".state" / "rootfs-build",
            root / ".state" / "system-test",
            root / ".state" / "riscv64" / ".axconfig.toml",
            root / ".state" / "riscv64" / ".axconfig.old.toml",
            root / ".state" / "loongarch64" / ".axconfig.toml",
            root / ".state" / "loongarch64" / ".axconfig.old.toml",
        ):
            self.assertIn(str(path), result.stdout)
        self.assertIn(str(root / ".state" / "*-current"), result.stdout)
        for path in (
            root / ".state" / "build-cache",
            root / ".state" / "source-cache",
            root / ".state" / "riscv64" / "target",
            root / ".state" / "loongarch64" / "target",
        ):
            self.assertNotIn(str(path), result.stdout)

    def test_clean_all_dry_run_does_not_require_an_application(self) -> None:
        root = repo_root()
        result = subprocess.run(
            ["make", "--no-print-directory", "-n", "clean-all"],
            cwd=root,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        self.assertEqual(result.returncode, 0, msg=result.stderr)
        self.assertIn(f'rm -rf "{root / ".state"}"', result.stdout)


if __name__ == "__main__":
    unittest.main()
