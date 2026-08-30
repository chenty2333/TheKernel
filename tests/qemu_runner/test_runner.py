from __future__ import annotations

import gzip
import hashlib
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import MagicMock, patch

from tools.qemu_runner.model import Interaction, RunResult
from tools.qemu_runner.runner import (
    Q35_OVMF_CODE_SHA256,
    RunConfig,
    RunnerError,
    _resolve_ovmf_image,
    _parse_qemu_device_help,
    _parse_qemu_display_help,
    _probe_virgl_headless,
    _validate_virgl_capabilities,
    run,
)


class RunnerTests(unittest.TestCase):
    def test_qemu_graphics_help_parsers_identify_host_capabilities(self) -> None:
        self.assertEqual(
            _parse_qemu_device_help(
                'name "virtio-gpu-pci", bus PCI\n'
                'name "virtio-gpu-gl-pci", bus PCI, alias "virtio-gpu-gl"\n'
            ),
            frozenset({"virtio-gpu-pci", "virtio-gpu-gl-pci"}),
        )
        self.assertEqual(
            _parse_qemu_display_help(
                "Available display backend types:\nnone\ngtk\negl-headless\n\nMore help\n"
            ),
            frozenset({"none", "gtk", "egl-headless"}),
        )

    def test_virgl_headless_probe_accepts_a_running_qemu(self) -> None:
        process = MagicMock()
        process.communicate.side_effect = (
            subprocess.TimeoutExpired("qemu", 0.5),
            ("", ""),
        )
        with patch("tools.qemu_runner.runner.subprocess.Popen", return_value=process) as popen:
            _probe_virgl_headless(Path("/qemu"))
        self.assertEqual(
            popen.call_args.args[0],
            [
                "/qemu", "-machine", "q35", "-nodefaults", "-S",
                "-display", "egl-headless,gl=on", "-device",
                "virtio-gpu-gl-pci,max_outputs=1,xres=800,yres=600",
            ],
        )
        process.terminate.assert_called_once_with()

    def test_virgl_headless_probe_distinguishes_argv_rejection(self) -> None:
        process = MagicMock(returncode=1)
        process.communicate.return_value = ("", "Invalid parameter 'gl'")
        with patch("tools.qemu_runner.runner.subprocess.Popen", return_value=process):
            with self.assertRaisesRegex(RunnerError, "rejected virgl-headless argv"):
                _probe_virgl_headless(Path("/qemu"))

    def test_virgl_headless_capability_check_runs_the_probe(self) -> None:
        device_help = 'name "virtio-gpu-gl-pci", bus PCI\n'
        display_help = "Available display backend types:\negl-headless\n\n"
        with patch(
            "tools.qemu_runner.runner._qemu_help_output",
            side_effect=(device_help, display_help),
        ), patch("tools.qemu_runner.runner._probe_virgl_headless") as probe:
            _validate_virgl_capabilities("virgl-headless", Path("/qemu"))
        probe.assert_called_once_with(Path("/qemu"))

    def test_virgl_interactive_requires_gl_device_and_gtk_gl_syntax(self) -> None:
        device_help = 'name "virtio-gpu-gl-pci", bus PCI\n'
        display_help = "Available display backend types:\ngtk\n\n"
        with patch(
            "tools.qemu_runner.runner._qemu_help_output",
            side_effect=(device_help, display_help),
        ) as capability_help:
            _validate_virgl_capabilities("virgl-interactive", Path("/qemu"))
        self.assertEqual(
            [call.args[1:] for call in capability_help.call_args_list],
            [("-device", "help"), ("-display", "help")],
        )

    def test_implicit_ovmf_selection_rejects_incompatible_host_firmware(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            incompatible = root / "OVMF_CODE.fd"
            incompatible.write_bytes(b"host-specific-ecam-layout")
            with self.assertRaisesRegex(RunnerError, "requires the pinned OVMF code"):
                _resolve_ovmf_image(
                    None,
                    "THEKERNEL_TEST_OVMF_CODE",
                    (str(incompatible),),
                    "OVMF code",
                    Q35_OVMF_CODE_SHA256,
                )

    def test_explicit_ovmf_override_cannot_bypass_q35_pin(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            image = Path(directory) / "OVMF_CODE.fd"
            image.write_bytes(b"explicit")
            with self.assertRaisesRegex(RunnerError, "does not match q35 pin"):
                _resolve_ovmf_image(
                    image,
                    "THEKERNEL_TEST_OVMF_CODE",
                    (),
                    "OVMF code",
                    Q35_OVMF_CODE_SHA256,
                )

    def test_initrd_is_passed_to_qemu_after_validation(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            kernel = root / "kernel"
            rootfs = root / "root.img"
            initrd = root / "initrd.img"
            kernel.write_bytes(b"kernel")
            rootfs.write_bytes(b"rootfs")
            initrd.write_bytes(b"trusted-initrd")
            config = RunConfig(
                arch="x86_64",
                kernel=kernel,
                rootfs=rootfs,
                workdir=root / "run",
                log_path=root / "run" / "console.log",
                qemu_binary=sys.executable,
                direct_kernel=True,
                extra_args=("-initrd", str(initrd)),
            )
            expected = RunResult(
                arch="x86_64", command=("qemu",), returncode=0, duration_ms=1,
                log_path=config.log_path, workdir=config.workdir,
            )

            def complete_run(**kwargs):
                command = kwargs["command"]
                initrd_arg = command[command.index("-initrd") + 1]
                self.assertTrue(initrd_arg.startswith("/proc/self/fd/"))
                self.assertEqual(Path(initrd_arg).read_bytes(), b"trusted-initrd")
                self.assertIn(int(initrd_arg.rsplit("/", 1)[1]), kwargs["pass_fds"])
                kwargs["log_path"].write_bytes(b"guest console\n")
                return expected

            with patch("tools.qemu_runner.runner.run_process", side_effect=complete_run):
                self.assertIs(run(config), expected)

    def test_initrd_extra_argument_shape_is_strict(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            kernel = root / "kernel"
            rootfs = root / "root.img"
            initrd = root / "initrd.img"
            kernel.write_bytes(b"kernel")
            rootfs.write_bytes(b"rootfs")
            initrd.write_bytes(b"initrd")
            base = dict(
                arch="x86_64", kernel=kernel, rootfs=rootfs,
                workdir=root / "run", log_path=root / "run" / "console.log",
                qemu_binary=sys.executable, direct_kernel=True,
            )
            with self.assertRaisesRegex(RunnerError, "repeated -initrd"):
                run(RunConfig(**base, extra_args=("-initrd", str(initrd), "-initrd", str(initrd))))
            with self.assertRaisesRegex(RunnerError, "requires exactly one"):
                run(RunConfig(**base, extra_args=("-initrd",)))

    def test_x86_default_requires_esp_instead_of_falling_back_to_kernel(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            kernel = root / "kernel"
            rootfs = root / "root.img"
            kernel.write_bytes(b"kernel")
            rootfs.write_bytes(b"rootfs")
            config = RunConfig(
                arch="x86_64",
                kernel=kernel,
                rootfs=rootfs,
                workdir=root / "run",
                log_path=root / "run" / "console.log",
            )
            with self.assertRaisesRegex(RunnerError, "requires a GPT ESP"):
                run(config)

    def test_x86_uefi_copies_vars_and_attaches_snapshot_esp(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            kernel = root / "kernel"
            rootfs = root / "root.img"
            esp = root / "esp.img"
            ovmf_code = root / "OVMF_CODE.fd"
            ovmf_vars = root / "OVMF_VARS.fd"
            kernel.write_bytes(b"kernel")
            rootfs.write_bytes(b"rootfs")
            esp.write_bytes(b"esp")
            ovmf_code.write_bytes(b"code")
            ovmf_vars.write_bytes(b"vars")
            config = RunConfig(
                arch="x86_64",
                kernel=kernel,
                rootfs=rootfs,
                esp=esp,
                workdir=root / "run",
                log_path=root / "run" / "console.log",
                qemu_binary=sys.executable,
                ovmf_code=ovmf_code,
                ovmf_vars=ovmf_vars,
            )
            expected = RunResult(
                arch="x86_64",
                command=("qemu",),
                returncode=0,
                duration_ms=1,
                log_path=config.log_path,
                workdir=config.workdir,
            )

            def complete_run(**kwargs):
                command = kwargs["command"]
                vars_option = next(
                    value
                    for value in command
                    if value.startswith("if=pflash,format=raw,aio=threads,file=")
                )
                forwarded = Path(vars_option.split("file=", 1)[1])
                self.assertTrue(str(forwarded).startswith("/proc/self/fd/"))
                vars_runtime = config.workdir / "firmware" / "OVMF_VARS.fd"
                saved = config.workdir / "firmware" / "OVMF_VARS.saved"
                vars_runtime.rename(saved)
                vars_runtime.write_bytes(b"replacement")
                try:
                    self.assertEqual(forwarded.read_bytes(), b"vars")
                    with forwarded.open("r+b") as output:
                        output.seek(0)
                        output.write(b"updated")
                        output.truncate()
                    self.assertEqual(saved.read_bytes(), b"updated")
                finally:
                    vars_runtime.unlink()
                    saved.rename(vars_runtime)
                kwargs["log_path"].write_bytes(b"guest console\n")
                return expected

            with (
                patch(
                    "tools.qemu_runner.runner.Q35_OVMF_CODE_SHA256",
                    hashlib.sha256(b"code").hexdigest(),
                ),
                patch(
                    "tools.qemu_runner.runner.Q35_OVMF_VARS_SHA256",
                    hashlib.sha256(b"vars").hexdigest(),
                ),
                patch(
                    "tools.qemu_runner.runner.run_process", side_effect=complete_run
                ) as mocked,
            ):
                run(config)

            command = " ".join(mocked.call_args.kwargs["command"])
            self.assertNotIn("-kernel", command)
            self.assertIn(",if=ide,format=raw,snapshot=on", command)
            self.assertIn("virtio-blk-pci,drive=rootfs", command)
            self.assertIn("readonly=on,aio=threads,file=/proc/self/fd/", command)
            vars_runtime = config.workdir / "firmware" / "OVMF_VARS.fd"
            self.assertEqual(vars_runtime.read_bytes(), b"updated")
            self.assertNotEqual(vars_runtime.resolve(), ovmf_vars.resolve())

    def test_normal_run_uses_run_local_decompression(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            kernel = root / "kernel"
            rootfs = root / "root.img.gz"
            commands = root / "commands"
            kernel.write_bytes(b"kernel")
            commands.write_bytes(b"guest command\n")
            with gzip.open(rootfs, "wb") as output:
                output.write(b"rootfs")
            config = RunConfig(
                arch="x86_64",
                kernel=kernel,
                rootfs=rootfs,
                workdir=root / "run",
                log_path=root / "run" / "console.log",
                qemu_binary=sys.executable,
                direct_kernel=True,
                input_path=commands,
                interaction=Interaction(interactive=True, input_after_marker="READY"),
            )
            expected = RunResult(
                arch="x86_64",
                command=("qemu",),
                returncode=0,
                duration_ms=1,
                log_path=config.log_path,
                workdir=config.workdir,
            )

            def complete_run(**kwargs):
                kwargs["log_path"].write_bytes(b"guest console\n")
                return expected

            with patch(
                "tools.qemu_runner.runner.run_process", side_effect=complete_run
            ) as mocked:
                self.assertIs(run(config), expected)

            command = " ".join(mocked.call_args.kwargs["command"])
            runtime_rootfs = config.workdir / "images" / "rootfs-root.img"
            self.assertIn("/proc/self/fd/", command)
            self.assertEqual(runtime_rootfs.read_bytes(), b"rootfs")
    def test_explicit_artifacts_are_composed_without_discovery(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            kernel = root / "kernel"
            rootfs = root / "root.img"
            extra = root / "extra.img.gz"
            commands = root / "commands"
            kernel.write_bytes(b"kernel")
            rootfs.write_bytes(b"rootfs")
            commands.write_bytes(b"exit\n")
            with gzip.open(extra, "wb") as output:
                output.write(b"extra")
            config = RunConfig(
                arch="x86_64",
                kernel=kernel,
                rootfs=rootfs,
                extra_block=extra,
                workdir=root / "run",
                log_path=root / "run" / "console.log",
                qemu_binary=sys.executable,
                direct_kernel=True,
                input_path=commands,
                interaction=Interaction(interactive=True, input_after_marker="READY"),
            )
            expected = RunResult(
                arch="x86_64",
                command=("qemu",),
                returncode=0,
                duration_ms=1,
                log_path=config.log_path,
                workdir=config.workdir,
            )

            def complete_run(**kwargs):
                kwargs["log_path"].write_bytes(b"guest console\n")
                return expected

            with patch(
                "tools.qemu_runner.runner.run_process", side_effect=complete_run
            ) as mocked:
                run(config)
            command = " ".join(mocked.call_args.kwargs["command"])
            self.assertIn("virtio-blk-pci,drive=extra", command)

    def test_qemu_uses_opened_input_fds_after_paths_are_replaced(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            kernel = root / "kernel"
            rootfs = root / "root.img"
            extra = root / "extra.img"
            initrd = root / "initrd.img"
            kernel.write_bytes(b"trusted-kernel")
            rootfs.write_bytes(b"trusted-rootfs")
            extra.write_bytes(b"trusted-extra")
            initrd.write_bytes(b"trusted-initrd")
            config = RunConfig(
                arch="x86_64", kernel=kernel, rootfs=rootfs, extra_block=extra,
                workdir=root / "run", log_path=root / "run" / "console.log",
                qemu_binary=sys.executable, direct_kernel=True,
                extra_args=("-initrd", str(initrd)),
            )
            expected = RunResult(
                arch="x86_64", command=("qemu",), returncode=0, duration_ms=1,
                log_path=config.log_path, workdir=config.workdir,
            )

            def complete_run(**kwargs):
                for path in (kernel, rootfs, extra, initrd):
                    path.unlink()
                    path.write_bytes(b"replacement")
                command = kwargs["command"]
                forwarded = {
                    "kernel": Path(command[command.index("-kernel") + 1]),
                    "rootfs": Path(next(value for value in command if "id=rootfs" in value).split("file=", 1)[1].split(",", 1)[0]),
                    "extra": Path(next(value for value in command if "id=extra" in value).split("file=", 1)[1].split(",", 1)[0]),
                    "initrd": Path(command[command.index("-initrd") + 1]),
                }
                self.assertEqual(
                    {label: path.read_bytes() for label, path in forwarded.items()},
                    {
                        "kernel": b"trusted-kernel",
                        "rootfs": b"trusted-rootfs",
                        "extra": b"trusted-extra",
                        "initrd": b"trusted-initrd",
                    },
                )
                self.assertEqual(
                    {int(path.name) for path in forwarded.values()}, set(kwargs["pass_fds"])
                )
                kwargs["log_path"].write_bytes(b"guest console\n")
                return expected

            with patch("tools.qemu_runner.runner.run_process", side_effect=complete_run):
                self.assertIs(run(config), expected)

            self.assertEqual(kernel.read_bytes(), b"replacement")

    def test_log_must_not_alias_a_run_input(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            kernel = root / "kernel"
            rootfs = root / "root.img"
            kernel.write_bytes(b"kernel")
            rootfs.write_bytes(b"rootfs")
            config = RunConfig(
                arch="x86_64",
                kernel=kernel,
                rootfs=rootfs,
                workdir=root / "run",
                log_path=rootfs,
                qemu_binary=sys.executable,
                direct_kernel=True,
            )
            with patch("tools.qemu_runner.runner.run_process") as mocked:
                with self.assertRaisesRegex(RunnerError, "log aliases"):
                    run(config)
            mocked.assert_not_called()
            self.assertEqual(rootfs.read_bytes(), b"rootfs")

    def test_log_must_not_hardlink_a_run_input(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            kernel = root / "kernel"
            rootfs = root / "root.img"
            log_path = root / "console.log"
            kernel.write_bytes(b"kernel")
            rootfs.write_bytes(b"rootfs")
            log_path.hardlink_to(kernel)
            config = RunConfig(
                arch="x86_64",
                kernel=kernel,
                rootfs=rootfs,
                workdir=root / "run",
                log_path=log_path,
                qemu_binary=sys.executable,
                direct_kernel=True,
            )
            with patch("tools.qemu_runner.runner.run_process") as mocked:
                with self.assertRaisesRegex(RunnerError, "log aliases"):
                    run(config)
            mocked.assert_not_called()
            self.assertEqual(kernel.read_bytes(), b"kernel")

    def test_compressed_rootfs_runtime_must_not_overwrite_kernel(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            workdir = root / "run"
            kernel = workdir / "images" / "rootfs-root.img"
            rootfs = root / "root.img.gz"
            kernel.parent.mkdir(parents=True)
            kernel.write_bytes(b"kernel")
            with gzip.open(rootfs, "wb") as output:
                output.write(b"rootfs")
            config = RunConfig(
                arch="x86_64",
                kernel=kernel,
                rootfs=rootfs,
                workdir=workdir,
                log_path=workdir / "console.log",
                qemu_binary=sys.executable,
                direct_kernel=True,
            )

            with patch("tools.qemu_runner.runner.run_process") as mocked:
                with self.assertRaisesRegex(RunnerError, "rootfs runtime aliases"):
                    run(config)
            mocked.assert_not_called()
            self.assertEqual(kernel.read_bytes(), b"kernel")

    def test_compressed_rootfs_runtime_must_not_overwrite_hardlinked_input(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            workdir = root / "run"
            kernel = root / "kernel"
            runtime = workdir / "images" / "rootfs-root.img"
            rootfs = root / "root.img.gz"
            kernel.write_bytes(b"kernel")
            runtime.parent.mkdir(parents=True)
            runtime.hardlink_to(kernel)
            with gzip.open(rootfs, "wb") as output:
                output.write(b"rootfs")
            config = RunConfig(
                arch="x86_64",
                kernel=kernel,
                rootfs=rootfs,
                workdir=workdir,
                log_path=workdir / "console.log",
                qemu_binary=sys.executable,
                direct_kernel=True,
            )

            with self.assertRaisesRegex(RunnerError, "rootfs runtime aliases"):
                run(config)
            self.assertEqual(kernel.read_bytes(), b"kernel")

    def test_ovmf_vars_runtime_must_not_overwrite_kernel(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            workdir = root / "run"
            kernel = workdir / "firmware" / "OVMF_VARS.fd"
            rootfs = root / "root.img"
            esp = root / "esp.img"
            ovmf_code = root / "OVMF_CODE.fd"
            ovmf_vars = root / "source-OVMF_VARS.fd"
            kernel.parent.mkdir(parents=True)
            kernel.write_bytes(b"kernel")
            rootfs.write_bytes(b"rootfs")
            esp.write_bytes(b"esp")
            ovmf_code.write_bytes(b"code")
            ovmf_vars.write_bytes(b"vars")
            config = RunConfig(
                arch="x86_64",
                kernel=kernel,
                rootfs=rootfs,
                esp=esp,
                workdir=workdir,
                log_path=workdir / "console.log",
                qemu_binary=sys.executable,
                ovmf_code=ovmf_code,
                ovmf_vars=ovmf_vars,
            )

            with (
                patch(
                    "tools.qemu_runner.runner.Q35_OVMF_CODE_SHA256",
                    hashlib.sha256(b"code").hexdigest(),
                ),
                patch(
                    "tools.qemu_runner.runner.Q35_OVMF_VARS_SHA256",
                    hashlib.sha256(b"vars").hexdigest(),
                ),
                self.assertRaisesRegex(RunnerError, "OVMF vars runtime aliases"),
            ):
                run(config)
            self.assertEqual(kernel.read_bytes(), b"kernel")

    def test_log_must_not_alias_resolved_qemu(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            kernel = root / "kernel"
            rootfs = root / "root.img"
            qemu = root / "qemu-system-x86_64"
            kernel.write_bytes(b"kernel")
            rootfs.write_bytes(b"rootfs")
            qemu.write_bytes(b"qemu executable")
            qemu.chmod(0o755)
            config = RunConfig(
                arch="x86_64",
                kernel=kernel,
                rootfs=rootfs,
                workdir=root / "run",
                log_path=qemu,
                qemu_binary=str(qemu),
                direct_kernel=True,
            )

            with patch("tools.qemu_runner.runner.run_process") as mocked:
                with self.assertRaisesRegex(RunnerError, "log aliases"):
                    run(config)
            mocked.assert_not_called()
            self.assertEqual(qemu.read_bytes(), b"qemu executable")

    def test_missing_kernel_is_rejected_before_image_preparation(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            rootfs = root / "root.img"
            rootfs.write_bytes(b"rootfs")
            config = RunConfig(
                arch="x86_64",
                kernel=root / "missing-kernel",
                rootfs=rootfs,
                workdir=root / "run",
                log_path=root / "run" / "console.log",
            )
            with self.assertRaisesRegex(RunnerError, "kernel does not exist"):
                run(config)


if __name__ == "__main__":
    unittest.main()
