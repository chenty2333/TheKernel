from __future__ import annotations

import gzip
import hashlib
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from tools.qemu_runner.model import InputForwarding, Interaction, RunResult
from tools.qemu_runner.receipt import finalize_external_input_receipt
from tools.qemu_runner.runner import RunConfig, RunnerError, run


class RunnerTests(unittest.TestCase):
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
                cache_dir=root / "cache",
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
                cache_dir=root / "cache",
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

    def test_external_producer_receipt_is_finalized_from_forwarded_bytes(self) -> None:
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
                observed_bytes=len(payload),
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
                cache_dir=root / "cache",
                qemu_binary=sys.executable,
                direct_kernel=True,
                receipt_path=receipt_path,
                interaction=Interaction(interactive=True, input_after_marker="READY"),
                external_input_producer=True,
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

            with patch("tools.qemu_runner.runner.run_process", side_effect=complete_run):
                run(config)
            pending = json.loads(receipt_path.read_text(encoding="utf-8"))
            self.assertEqual(pending["schema_version"], 2)
            self.assertEqual(pending["state"], "awaiting_producer")
            self.assertNotIn("producer_status", pending["stdin"])

            self.assertTrue(
                finalize_external_input_receipt(
                    receipt_path=receipt_path,
                    commands_path=commands,
                    expected_sha256=forwarding.sha256,
                    expected_bytes=len(payload),
                    expected_line_count=2,
                    producer_status=0,
                )
            )
            completed = json.loads(receipt_path.read_text(encoding="utf-8"))
            self.assertEqual(completed["state"], "complete")
            self.assertTrue(completed["stdin"]["source_fully_relayed"])
            self.assertTrue(completed["stdin"]["producer_status_accepted"])
            self.assertEqual(completed["stdin"]["producer_status_kind"], "exit:0")

            validator = Path(__file__).resolve().parents[2] / "scripts/ci/validate-qemu-receipt.py"
            validator_args = [
                sys.executable,
                str(validator),
                "--receipt",
                str(receipt_path),
                "--arch",
                "x86_64",
                "--direct-kernel",
                "--cpus",
                "1",
                "--kernel",
                str(kernel),
                "--rootfs",
                str(rootfs),
                "--rootfs-mode",
                config.rootfs_mode,
                "--log",
                str(config.log_path),
                "--qemu-binary",
                sys.executable,
            ]
            self.assertNotEqual(
                subprocess.run(validator_args, check=False).returncode,
                0,
            )
            self.assertEqual(
                subprocess.run(
                    [*validator_args, "--commands", str(commands)], check=False
                ).returncode,
                0,
            )

    def test_external_producer_truncation_is_recorded_and_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            commands = root / "commands"
            receipt = root / "receipt.json"
            payload = b"first\nsecond\n"
            prefix = b"first\n"
            commands.write_bytes(payload)
            receipt.write_text(
                json.dumps(
                    {
                        "schema_version": 2,
                        "state": "awaiting_producer",
                        "interaction": {"external_input_producer": True},
                        "stdin": {
                            "state": "awaiting_producer",
                            "sha256": hashlib.sha256(prefix).hexdigest(),
                            "bytes": len(prefix),
                            "line_count": 1,
                            "observed_bytes": len(prefix),
                            "source_eof": True,
                            "broken_pipe": False,
                            "relay_complete": True,
                        },
                    }
                ),
                encoding="utf-8",
            )
            self.assertFalse(
                finalize_external_input_receipt(
                    receipt_path=receipt,
                    commands_path=commands,
                    expected_sha256=hashlib.sha256(payload).hexdigest(),
                    expected_bytes=len(payload),
                    expected_line_count=2,
                    producer_status=141,
                )
            )
            completed = json.loads(receipt.read_text(encoding="utf-8"))
            self.assertEqual(completed["state"], "complete")
            self.assertFalse(completed["stdin"]["source_fully_relayed"])
            self.assertFalse(completed["stdin"]["producer_status_accepted"])

    def test_exact_stream_does_not_turn_sigpipe_141_into_success(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            commands = root / "commands"
            receipt = root / "receipt.json"
            payload = b"first\nsecond\n"
            digest = hashlib.sha256(payload).hexdigest()
            commands.write_bytes(payload)
            receipt.write_text(
                json.dumps(
                    {
                        "schema_version": 2,
                        "state": "awaiting_producer",
                        "interaction": {"external_input_producer": True},
                        "stdin": {
                            "state": "awaiting_producer",
                            "sha256": digest,
                            "bytes": len(payload),
                            "line_count": 2,
                            "observed_bytes": len(payload),
                            "source_eof": True,
                            "broken_pipe": False,
                            "relay_complete": True,
                        },
                    }
                ),
                encoding="utf-8",
            )
            self.assertFalse(
                finalize_external_input_receipt(
                    receipt_path=receipt,
                    commands_path=commands,
                    expected_sha256=digest,
                    expected_bytes=len(payload),
                    expected_line_count=2,
                    producer_status=141,
                )
            )
            completed = json.loads(receipt.read_text(encoding="utf-8"))
            self.assertTrue(completed["stdin"]["source_fully_relayed"])
            self.assertFalse(completed["stdin"]["producer_status_accepted"])
            self.assertEqual(completed["stdin"]["producer_status_kind"], "signal:13")

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
                arch="x86_64",
                kernel=kernel,
                rootfs=rootfs,
                extra_block=extra,
                workdir=root / "run",
                log_path=root / "run" / "console.log",
                cache_dir=root / "cache",
                qemu_binary=sys.executable,
                direct_kernel=True,
                receipt_path=root / "run" / "receipt.json",
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

            with patch("tools.qemu_runner.runner.run_process", side_effect=complete_run) as mocked:
                result = run(config)
            self.assertIs(result, expected)
            command = " ".join(mocked.call_args.kwargs["command"])
            self.assertIn(str(rootfs.resolve()), command)
            self.assertIn("writable-images/extra-extra.img", command)
            self.assertIn("virtio-blk-pci,drive=extra", command)
            self.assertNotIn(str((root / "cache").resolve()) + ",if=none", command)
            receipt = json.loads(config.receipt_path.read_text(encoding="utf-8"))
            self.assertEqual(receipt["state"], "complete")
            self.assertEqual(receipt["returncode"], 0)
            self.assertEqual(receipt["command"][0], str(Path(sys.executable).resolve()))
            self.assertEqual(
                receipt["kernel"]["sha256"],
                "6923dd1bc0460082c5d55a831908c24a282860b7f1cd6c2b79cf1bc8857c639c",
            )
            self.assertEqual(list(config.receipt_path.parent.glob(".receipt.json.*.tmp")), [])

            validator = Path(__file__).resolve().parents[2] / "scripts/ci/validate-qemu-receipt.py"
            validator_args = [
                sys.executable,
                str(validator),
                "--receipt",
                str(config.receipt_path),
                "--arch",
                "x86_64",
                "--direct-kernel",
                "--cpus",
                "1",
                "--kernel",
                str(kernel),
                "--rootfs",
                str(rootfs),
                "--rootfs-mode",
                config.rootfs_mode,
                "--extra-block",
                str(extra),
                "--extra-block-mode",
                config.extra_block_mode,
                "--log",
                str(config.log_path),
                "--qemu-binary",
                sys.executable,
            ]
            self.assertEqual(subprocess.run(validator_args, check=False).returncode, 0)

            commands = root / "unexpected-commands"
            commands.write_bytes(b"echo should-not-be-present\n")
            self.assertNotEqual(
                subprocess.run(
                    [*validator_args, "--commands", str(commands)], check=False
                ).returncode,
                0,
            )

            wrong_mode_args = validator_args.copy()
            wrong_mode_args[wrong_mode_args.index("--rootfs-mode") + 1] = "readonly"
            self.assertNotEqual(subprocess.run(wrong_mode_args, check=False).returncode, 0)

            wrong_qemu_args = validator_args.copy()
            wrong_qemu_args[wrong_qemu_args.index("--qemu-binary") + 1] = "/bin/sh"
            self.assertNotEqual(subprocess.run(wrong_qemu_args, check=False).returncode, 0)

            original_receipt = config.receipt_path.read_text(encoding="utf-8")
            tampered = json.loads(original_receipt)
            tampered["command"][tampered["command"].index("-smp") + 1] = "2"
            config.receipt_path.write_text(json.dumps(tampered), encoding="utf-8")
            self.assertNotEqual(subprocess.run(validator_args, check=False).returncode, 0)
            config.receipt_path.write_text(original_receipt, encoding="utf-8")

            config.log_path.write_bytes(b"tampered console\n")
            self.assertNotEqual(subprocess.run(validator_args, check=False).returncode, 0)

    def test_validator_accepts_recorded_in_place_writable_images(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            kernel = root / "kernel"
            rootfs = root / "root.img"
            extra = root / "extra.img"
            kernel.write_bytes(b"kernel")
            rootfs.write_bytes(b"rootfs-before")
            extra.write_bytes(b"extra-before")
            config = RunConfig(
                arch="x86_64",
                kernel=kernel,
                rootfs=rootfs,
                rootfs_mode="rw",
                extra_block=extra,
                extra_block_mode="rw",
                workdir=root / "run",
                log_path=root / "run" / "console.log",
                cache_dir=root / "cache",
                qemu_binary=sys.executable,
                direct_kernel=True,
                receipt_path=root / "run" / "receipt.json",
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
                rootfs.write_bytes(b"rootfs-after")
                extra.write_bytes(b"extra-after")
                kwargs["log_path"].write_bytes(b"guest console\n")
                return expected

            with patch("tools.qemu_runner.runner.run_process", side_effect=complete_run):
                run(config)

            validator = Path(__file__).resolve().parents[2] / "scripts/ci/validate-qemu-receipt.py"
            validator_args = [
                sys.executable,
                str(validator),
                "--receipt",
                str(config.receipt_path),
                "--arch",
                "x86_64",
                "--direct-kernel",
                "--cpus",
                "1",
                "--kernel",
                str(kernel),
                "--rootfs",
                str(rootfs),
                "--rootfs-mode",
                "rw",
                "--extra-block",
                str(extra),
                "--extra-block-mode",
                "rw",
                "--log",
                str(config.log_path),
                "--qemu-binary",
                sys.executable,
            ]
            self.assertEqual(subprocess.run(validator_args, check=False).returncode, 0)

            tampered = json.loads(config.receipt_path.read_text(encoding="utf-8"))
            for key in (
                "rootfs_source",
                "rootfs_runtime_before",
                "extra_block_source",
                "extra_block_runtime_before",
            ):
                tampered[key] = {"path": tampered[key]["path"]}
            config.receipt_path.write_text(json.dumps(tampered), encoding="utf-8")
            self.assertNotEqual(subprocess.run(validator_args, check=False).returncode, 0)

    def test_prepared_receipt_survives_an_unexpected_runner_error(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            kernel = root / "kernel"
            rootfs = root / "root.img"
            receipt_path = root / "run" / "receipt.json"
            kernel.write_bytes(b"kernel")
            rootfs.write_bytes(b"rootfs")
            config = RunConfig(
                arch="x86_64",
                kernel=kernel,
                rootfs=rootfs,
                workdir=root / "run",
                log_path=root / "run" / "console.log",
                cache_dir=root / "cache",
                qemu_binary=sys.executable,
                direct_kernel=True,
                receipt_path=receipt_path,
            )
            with patch(
                "tools.qemu_runner.runner.run_process",
                side_effect=RuntimeError("injected runner failure"),
            ):
                with self.assertRaisesRegex(RuntimeError, "injected runner failure"):
                    run(config)
            receipt = json.loads(receipt_path.read_text(encoding="utf-8"))
            self.assertEqual(receipt["state"], "prepared")
            self.assertNotIn("returncode", receipt)

    def test_receipt_must_not_alias_a_run_input(self) -> None:
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
                cache_dir=root / "cache",
                qemu_binary=sys.executable,
                direct_kernel=True,
                receipt_path=kernel,
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
            kernel.write_bytes(b"kernel")
            rootfs.write_bytes(b"rootfs")
            receipt_path.hardlink_to(kernel)
            config = RunConfig(
                arch="x86_64",
                kernel=kernel,
                rootfs=rootfs,
                workdir=root / "run",
                log_path=root / "run" / "console.log",
                cache_dir=root / "cache",
                qemu_binary=sys.executable,
                direct_kernel=True,
                receipt_path=receipt_path,
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
                cache_dir=root / "cache",
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
                cache_dir=root / "cache",
                qemu_binary=sys.executable,
                direct_kernel=True,
            )
            with patch("tools.qemu_runner.runner.run_process") as mocked:
                with self.assertRaisesRegex(RunnerError, "log aliases"):
                    run(config)
            mocked.assert_not_called()
            self.assertEqual(kernel.read_bytes(), b"kernel")

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
                cache_dir=root / "cache",
            )
            with self.assertRaisesRegex(RunnerError, "kernel does not exist"):
                run(config)


if __name__ == "__main__":
    unittest.main()
