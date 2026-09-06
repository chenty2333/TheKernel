from pathlib import Path
from types import SimpleNamespace
import subprocess
import unittest
from unittest.mock import patch

from tests.support import test_tmpdir
from tools.qemu_runner.boot_artifacts import validate_linux_esp_kernel, validate_thekernel_esp_kernel
from tools.qemu_runner.runner import RunnerError


class BootArtifactTests(unittest.TestCase):
    def test_exact_embedded_kernel_and_stale_payload_rejection(self):
        with test_tmpdir() as temporary:
            kernel = Path(temporary) / "vmlinuz"
            esp = Path(temporary) / "linux.esp"
            kernel.write_bytes(b"kernel\x00\xff\r\n")
            with patch("tools.qemu_runner.boot_artifacts.subprocess.run") as read:
                read.return_value = SimpleNamespace(returncode=0, stdout=kernel.read_bytes(), stderr=b"")
                validate_linux_esp_kernel(kernel, esp)
                self.assertEqual(read.call_args.args[0], ["mtype", "-i", f"{esp}@@1M", "::/vmlinuz"])
                validate_thekernel_esp_kernel(kernel, esp)
                self.assertEqual(read.call_args.args[0][-1], "::/TheKernel.elf")
                read.return_value.stdout = b"older kernel"
                with self.assertRaisesRegex(RunnerError, "requested kernel"):
                    validate_linux_esp_kernel(kernel, esp)

    def test_unreadable_missing_tool_and_timeout_fail_closed(self):
        with test_tmpdir() as temporary:
            kernel = Path(temporary) / "vmlinuz"
            kernel.write_bytes(b"kernel")
            esp = Path(temporary) / "linux.esp"
            with patch("tools.qemu_runner.boot_artifacts.subprocess.run") as read:
                read.return_value = SimpleNamespace(returncode=1, stdout=b"kernel", stderr=b"bad FAT")
                with self.assertRaisesRegex(RunnerError, "cannot read"):
                    validate_linux_esp_kernel(kernel, esp)
                for error in (FileNotFoundError("mtype"), subprocess.TimeoutExpired("mtype", 30)):
                    read.side_effect = error
                    with self.assertRaisesRegex(RunnerError, "cannot verify"):
                        validate_linux_esp_kernel(kernel, esp)
