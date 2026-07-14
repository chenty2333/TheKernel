from __future__ import annotations

import gzip
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from tools.qemu_runner.model import RunResult
from tools.qemu_runner.runner import RunConfig, RunnerError, run


class RunnerTests(unittest.TestCase):
    def test_explicit_artifacts_are_composed_without_discovery(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            kernel = root / "kernel"
            rootfs = root / "root.img"
            extra = root / "extra.img.gz"
            kernel.write_bytes(b"kernel")
            rootfs.write_bytes(b"rootfs")
            with gzip.open(extra, "wb") as output:
                output.write(b"extra")
            config = RunConfig(
                arch="rv",
                kernel=kernel,
                rootfs=rootfs,
                extra_block=extra,
                workdir=root / "run",
                log_path=root / "run" / "console.log",
                cache_dir=root / "cache",
            )
            expected = RunResult(
                arch="rv",
                command=("qemu",),
                returncode=0,
                duration_ms=1,
                log_path=config.log_path,
                workdir=config.workdir,
            )
            with patch("tools.qemu_runner.runner.run_process", return_value=expected) as mocked:
                result = run(config)
            self.assertIs(result, expected)
            command = " ".join(mocked.call_args.kwargs["command"])
            self.assertIn(str(rootfs.resolve()), command)
            self.assertIn("writable-images/extra-extra.img", command)
            self.assertIn("bus=virtio-mmio-bus.1", command)
            self.assertNotIn(str((root / "cache").resolve()) + ",if=none", command)

    def test_missing_kernel_is_rejected_before_image_preparation(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            rootfs = root / "root.img"
            rootfs.write_bytes(b"rootfs")
            config = RunConfig(
                arch="la",
                kernel=root / "missing-kernel",
                rootfs=rootfs,
                workdir=root / "run",
                log_path=root / "run" / "console.log",
                cache_dir=root / "cache",
            )
            with self.assertRaisesRegex(RunnerError, "kernel does not exist"):
                run(config)


if __name__ == "__main__":
    unittest.main()
