from __future__ import annotations

import gzip
import hashlib
import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from tools.qemu_runner.model import InputForwarding, Interaction, RunResult
from tools.qemu_runner.runner import RunConfig, RunnerError, run


class RunnerTests(unittest.TestCase):
    def test_initrd_is_bound_to_inherited_fd_across_path_replacement(self) -> None:
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
                qemu_launcher=(sys.executable,),
                direct_kernel=True,
                extra_args=("-initrd", str(initrd)),
            )
            expected = RunResult(
                arch="x86_64", command=("qemu",), returncode=0, duration_ms=1,
                log_path=config.log_path, workdir=config.workdir,
            )

            def complete_run(**kwargs):
                command = kwargs["command"]
                bound = command[command.index("-initrd") + 1]
                self.assertRegex(bound, r"^/proc/self/fd/[0-9]+$")
                inherited = tuple(int(value) for value in kwargs["environment"][
                    "THEKERNEL_QEMU_LAUNCH_FDS"
                ].split(","))
                self.assertEqual(set(inherited), set(kwargs["pass_fds"]))
                self.assertIn(int(bound.rsplit("/", 1)[1]), inherited)
                self.assertEqual(Path(bound).read_bytes(), b"trusted-initrd")

                saved = root / "initrd.saved"
                initrd.rename(saved)
                initrd.write_bytes(b"replacement")
                try:
                    self.assertEqual(Path(bound).read_bytes(), b"trusted-initrd")
                finally:
                    initrd.unlink()
                    saved.rename(initrd)
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
                kwargs["log_path"].write_bytes(b"guest console\n")
                return expected

            with patch(
                "tools.qemu_runner.runner.run_process", side_effect=complete_run
            ) as mocked:
                run(config)

            command = " ".join(mocked.call_args.kwargs["command"])
            self.assertNotIn("-kernel", command)
            self.assertIn(",if=ide,format=raw,snapshot=on", command)
            self.assertIn("virtio-blk-pci,drive=rootfs", command)
            vars_runtime = config.workdir / "firmware" / "OVMF_VARS.fd"
            self.assertEqual(vars_runtime.read_bytes(), ovmf_vars.read_bytes())
            self.assertNotEqual(vars_runtime.resolve(), ovmf_vars.resolve())

    def test_normal_run_uses_run_local_decompression_without_evidence(self) -> None:
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

            with (
                patch(
                    "tools.qemu_runner.runner.run_process", side_effect=complete_run
                ) as mocked,
                patch(
                    "tools.qemu_runner.evidence.file_evidence",
                    side_effect=AssertionError("normal run hashed an input"),
                ) as evidence,
                patch(
                    "tools.qemu_runner.receipt.command_stream_evidence",
                    side_effect=AssertionError("normal run hashed command input"),
                ) as command_evidence,
            ):
                self.assertIs(run(config), expected)

            command = " ".join(mocked.call_args.kwargs["command"])
            runtime_rootfs = config.workdir / "images" / "rootfs-root.img"
            self.assertIn("/proc/self/fd/", command)
            self.assertEqual(runtime_rootfs.read_bytes(), b"rootfs")
            evidence.assert_not_called()
            command_evidence.assert_not_called()
            self.assertFalse((config.workdir / "receipt.json").exists())

    def test_receipt_is_completed_from_runner_owned_input_file(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            kernel = root / "kernel"
            rootfs = root / "root.img"
            commands = root / "commands"
            receipt_path = root / "run" / "receipt.json"
            kernel.write_bytes(b"kernel")
            rootfs.write_bytes(b"rootfs")
            payload = b"first\nsecond\n"
            commands.write_bytes(payload)
            forwarding = InputForwarding(
                sha256=hashlib.sha256(payload).hexdigest(),
                bytes_forwarded=len(payload),
                line_count=2,
                source_eof=True,
                broken_pipe=False,
                relay_complete=True,
            )
            config = RunConfig(
                arch="x86_64",
                kernel=kernel,
                rootfs=rootfs,
                workdir=root / "run",
                log_path=root / "run" / "console.log",
                qemu_binary=sys.executable,
                direct_kernel=True,
                receipt_path=receipt_path,
                input_path=commands,
                interaction=Interaction(interactive=True),
            )
            expected = RunResult(
                arch="x86_64",
                command=("qemu",),
                returncode=0,
                duration_ms=1,
                log_path=config.log_path,
                workdir=config.workdir,
                input_forwarding=forwarding,
            )

            def complete_run(**kwargs):
                kwargs["log_path"].write_bytes(b"guest console\n")
                return expected

            with patch(
                "tools.qemu_runner.runner.run_process", side_effect=complete_run
            ) as mocked:
                run(config)
            self.assertTrue(mocked.call_args.kwargs["capture_input_evidence"])
            completed = json.loads(receipt_path.read_text(encoding="utf-8"))
            self.assertEqual(completed["schema_version"], 4)
            self.assertEqual(completed["state"], "recorded")
            self.assertEqual(
                set(completed["source_identity"]),
                {"schema", "combination_id", "sources"},
            )
            self.assertIn("thekernel", completed["source_identity"]["sources"])
            for source in completed["source_identity"]["sources"].values():
                self.assertEqual(
                    set(source),
                    {"repository_root", "commit", "tree", "worktree_dirty", "match_declared"},
                )
            self.assertTrue(completed["guest_clean_shutdown"])
            self.assertFalse(completed["marker_success"])
            self.assertFalse(completed["runner_terminated"])
            self.assertIsNone(completed["runner_termination_reason"])
            self.assertFalse(completed["physical_retirement_proven"])
            self.assertEqual(completed["stdin"]["source"]["sha256"], forwarding.sha256)
            self.assertEqual(completed["stdin"]["forwarded"]["sha256"], forwarding.sha256)
            self.assertTrue(completed["stdin"]["source_unchanged"])

    def test_receipt_records_partial_input_forwarding(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            kernel = root / "kernel"
            rootfs = root / "root.img"
            commands = root / "commands"
            receipt_path = root / "receipt.json"
            payload = b"first\nsecond\n"
            prefix = b"first\n"
            kernel.write_bytes(b"kernel")
            rootfs.write_bytes(b"rootfs")
            commands.write_bytes(payload)
            config = RunConfig(
                arch="x86_64",
                kernel=kernel,
                rootfs=rootfs,
                workdir=root / "run",
                log_path=root / "run" / "console.log",
                qemu_binary=sys.executable,
                direct_kernel=True,
                receipt_path=receipt_path,
                input_path=commands,
                interaction=Interaction(interactive=True, input_after_marker="READY"),
            )
            forwarding = InputForwarding(
                sha256=hashlib.sha256(prefix).hexdigest(),
                bytes_forwarded=len(prefix),
                line_count=1,
                source_eof=True,
                broken_pipe=False,
                relay_complete=True,
            )
            expected = RunResult(
                arch="x86_64", command=("qemu",), returncode=0, duration_ms=1,
                log_path=config.log_path, workdir=config.workdir, input_forwarding=forwarding,
            )

            def complete_run(**kwargs):
                kwargs["log_path"].write_bytes(b"guest console\n")
                return expected

            with patch("tools.qemu_runner.runner.run_process", side_effect=complete_run):
                run(config)
            completed = json.loads(receipt_path.read_text(encoding="utf-8"))
            self.assertEqual(completed["state"], "recorded")
            self.assertNotEqual(
                completed["stdin"]["source"]["sha256"],
                completed["stdin"]["forwarded"]["sha256"],
            )

    def test_receipt_records_a_changed_command_file(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            kernel = root / "kernel"
            rootfs = root / "root.img"
            commands = root / "commands"
            receipt_path = root / "receipt.json"
            payload = b"first\n"
            kernel.write_bytes(b"kernel")
            rootfs.write_bytes(b"rootfs")
            commands.write_bytes(payload)
            config = RunConfig(
                arch="x86_64",
                kernel=kernel,
                rootfs=rootfs,
                workdir=root / "run",
                log_path=root / "run" / "console.log",
                qemu_binary=sys.executable,
                direct_kernel=True,
                receipt_path=receipt_path,
                input_path=commands,
                interaction=Interaction(interactive=True, input_after_marker="READY"),
            )
            forwarding = InputForwarding(
                sha256=hashlib.sha256(payload).hexdigest(),
                bytes_forwarded=len(payload),
                line_count=1,
                source_eof=True,
                broken_pipe=False,
                relay_complete=True,
            )
            expected = RunResult(
                arch="x86_64", command=("qemu",), returncode=0, duration_ms=1,
                log_path=config.log_path, workdir=config.workdir, input_forwarding=forwarding,
            )

            def complete_run(**kwargs):
                commands.write_bytes(b"changed\n")
                kwargs["log_path"].write_bytes(b"guest console\n")
                return expected

            with patch("tools.qemu_runner.runner.run_process", side_effect=complete_run):
                run(config)
            completed = json.loads(receipt_path.read_text(encoding="utf-8"))
            self.assertFalse(completed["stdin"]["source_unchanged"])

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
                receipt_path=root / "run" / "receipt.json",
                input_path=commands,
                interaction=Interaction(interactive=True, input_after_marker="READY"),
            )
            forwarding = InputForwarding(
                sha256=hashlib.sha256(commands.read_bytes()).hexdigest(),
                bytes_forwarded=commands.stat().st_size,
                line_count=1,
                source_eof=True,
                broken_pipe=False,
                relay_complete=True,
            )
            expected = RunResult(
                arch="x86_64",
                command=("qemu",),
                returncode=0,
                duration_ms=1,
                log_path=config.log_path,
                workdir=config.workdir,
                input_forwarding=forwarding,
            )
            def complete_run(**kwargs):
                kwargs["log_path"].write_bytes(b"guest console\n")
                return expected

            with patch("tools.qemu_runner.runner.run_process", side_effect=complete_run) as mocked:
                result = run(config)
            self.assertIs(result, expected)
            command = " ".join(mocked.call_args.kwargs["command"])
            self.assertIn("/proc/self/fd/", command)
            self.assertGreaterEqual(command.count("/proc/self/fd/"), 3)
            self.assertIn("virtio-blk-pci,drive=extra", command)
            receipt = json.loads(config.receipt_path.read_text(encoding="utf-8"))
            self.assertEqual(receipt["state"], "recorded")
            self.assertEqual(receipt["returncode"], 0)
            self.assertEqual(receipt["command"][0], str(Path(sys.executable).resolve()))
            self.assertEqual(
                receipt["kernel"]["sha256"],
                "6923dd1bc0460082c5d55a831908c24a282860b7f1cd6c2b79cf1bc8857c639c",
            )
            self.assertEqual(list(config.receipt_path.parent.glob(".receipt.json.*.tmp")), [])

    def test_runner_error_does_not_publish_a_performance_receipt(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            kernel = root / "kernel"
            rootfs = root / "root.img"
            receipt_path = root / "run" / "receipt.json"
            commands = root / "commands"
            kernel.write_bytes(b"kernel")
            rootfs.write_bytes(b"rootfs")
            commands.write_bytes(b"exit\n")
            config = RunConfig(
                arch="x86_64",
                kernel=kernel,
                rootfs=rootfs,
                workdir=root / "run",
                log_path=root / "run" / "console.log",
                qemu_binary=sys.executable,
                direct_kernel=True,
                receipt_path=receipt_path,
                input_path=commands,
                interaction=Interaction(interactive=True, input_after_marker="READY"),
            )
            with patch(
                "tools.qemu_runner.runner.run_process",
                side_effect=RuntimeError("injected runner failure"),
            ):
                with self.assertRaisesRegex(RuntimeError, "injected runner failure"):
                    run(config)
            self.assertFalse(receipt_path.exists())

    def test_receipt_must_not_alias_a_run_input(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            kernel = root / "kernel"
            rootfs = root / "root.img"
            commands = root / "commands"
            kernel.write_bytes(b"kernel")
            rootfs.write_bytes(b"rootfs")
            commands.write_bytes(b"exit\n")
            config = RunConfig(
                arch="x86_64",
                kernel=kernel,
                rootfs=rootfs,
                workdir=root / "run",
                log_path=root / "run" / "console.log",
                qemu_binary=sys.executable,
                direct_kernel=True,
                receipt_path=kernel,
                input_path=commands,
                interaction=Interaction(interactive=True, input_after_marker="READY"),
            )
            with self.assertRaisesRegex(RunnerError, "receipt aliases"):
                run(config)
            self.assertEqual(kernel.read_bytes(), b"kernel")

    def test_receipt_must_not_hardlink_a_run_input(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            kernel = root / "kernel"
            rootfs = root / "root.img"
            receipt_path = root / "receipt.json"
            commands = root / "commands"
            kernel.write_bytes(b"kernel")
            rootfs.write_bytes(b"rootfs")
            commands.write_bytes(b"exit\n")
            receipt_path.hardlink_to(kernel)
            config = RunConfig(
                arch="x86_64",
                kernel=kernel,
                rootfs=rootfs,
                workdir=root / "run",
                log_path=root / "run" / "console.log",
                qemu_binary=sys.executable,
                direct_kernel=True,
                receipt_path=receipt_path,
                input_path=commands,
                interaction=Interaction(interactive=True, input_after_marker="READY"),
            )
            with self.assertRaisesRegex(RunnerError, "receipt aliases"):
                run(config)
            self.assertEqual(kernel.read_bytes(), b"kernel")

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

            with self.assertRaisesRegex(RunnerError, "OVMF vars runtime aliases"):
                run(config)
            self.assertEqual(kernel.read_bytes(), b"kernel")

    def test_log_must_not_alias_resolved_qemu_without_receipt(self) -> None:
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
