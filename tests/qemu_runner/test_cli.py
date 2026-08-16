from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from tools.qemu_runner.cli import main
from tools.qemu_runner.model import RunResult


class CliTests(unittest.TestCase):
    def test_run_requires_explicit_kernel_and_rootfs(self) -> None:
        with self.assertRaises(SystemExit) as context:
            main(["run", "--arch", "x86_64"])
        self.assertEqual(context.exception.code, 2)

    def test_cli_builds_explicit_configuration(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            kernel = root / "kernel"
            rootfs = root / "root.img"
            extra_block = root / "extra.img"
            kernel.write_bytes(b"kernel")
            rootfs.write_bytes(b"root")
            extra_block.write_bytes(b"extra")
            result = RunResult(
                arch="x86_64",
                command=("qemu",),
                returncode=0,
                duration_ms=1,
                log_path=root / "console.log",
                workdir=root / "run",
            )
            with patch("tools.qemu_runner.cli.run", return_value=result) as mocked_run:
                status = main(
                    [
                        "run",
                        "--arch",
                        "x86_64",
                        "--kernel",
                        str(kernel),
                        "--rootfs",
                        str(rootfs),
                        "--rootfs-mode",
                        "readonly",
                        "--extra-block",
                        str(extra_block),
                        "--extra-block-mode",
                        "readonly",
                        "--receipt",
                        str(root / "receipt.json"),
                    ]
                )
            self.assertEqual(status, 0)
            config = mocked_run.call_args.args[0]
            self.assertEqual(config.arch, "x86_64")
            self.assertEqual(config.kernel, kernel)
            self.assertEqual(config.rootfs, rootfs)
            self.assertEqual(config.rootfs_mode, "readonly")
            self.assertEqual(config.extra_block, extra_block)
            self.assertEqual(config.extra_block_mode, "readonly")
            self.assertEqual(config.receipt_path, root / "receipt.json")

    def test_x86_aliases_normalize_to_x86_64(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            kernel = root / "kernel"
            rootfs = root / "root.img"
            kernel.write_bytes(b"kernel")
            rootfs.write_bytes(b"root")
            for alias in ("x86", "x86_64"):
                result = RunResult(
                    arch="x86_64",
                    command=("qemu",),
                    returncode=0,
                    duration_ms=1,
                    log_path=root / f"{alias}.console.log",
                    workdir=root / alias,
                )
                with self.subTest(alias=alias), patch(
                    "tools.qemu_runner.cli.run", return_value=result
                ) as mocked_run:
                    status = main(
                        [
                            "run",
                            "--arch",
                            alias,
                            "--kernel",
                            str(kernel),
                            "--rootfs",
                            str(rootfs),
                        ]
                    )
                self.assertEqual(status, 0)
                self.assertEqual(mocked_run.call_args.args[0].arch, "x86_64")

    def test_input_marker_requires_interactive_mode(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            kernel = root / "kernel"
            rootfs = root / "root.img"
            kernel.write_bytes(b"kernel")
            rootfs.write_bytes(b"root")
            status = main(
                [
                    "run",
                    "--arch",
                    "x86_64",
                    "--kernel",
                    str(kernel),
                    "--rootfs",
                    str(rootfs),
                    "--input-after-marker",
                    "READY",
                ]
            )
            self.assertEqual(status, 2)


if __name__ == "__main__":
    unittest.main()
