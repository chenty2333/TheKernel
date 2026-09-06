from contextlib import ExitStack, nullcontext, redirect_stdout
from io import StringIO
from pathlib import Path
import unittest
from unittest.mock import patch

from tests.support import test_tmpdir
from tools.qemu_runner.abi_differential import (
    AbiConfig, COMPLETE_MARKER, CONTRACTS, PROGRAMS, PROGRAM_CASES, PROGRAM_SUCCESS, PROGRAM_COMPLETIONS, expected_records, parse_abi_log, run_abi_differential,
)
from tools.qemu_runner.kernel_benchmark import BenchmarkTarget
from tools.qemu_runner.model import RunResult
from tools.qemu_runner.runner import RunnerError


def transcript():
    lines = ["Linux version 7.2.3 (builder)"]
    for index, name in enumerate(PROGRAMS, 1):
        cases = {f"{case}.{CONTRACTS[case][0]}" for case in PROGRAM_CASES[name]}
        lines.append(f"# THEKERNEL_TEST_BEGIN {index} abi-{name} timeout_seconds=120")
        lines.extend(record for record in expected_records() if record.split()[1] in cases)
        lines.extend((PROGRAM_SUCCESS[name], f"# THEKERNEL_TEST_END {index} abi-{name} result=0"))
    return "\n".join(lines + [COMPLETE_MARKER]) + "\n"


class AbiDifferentialTests(unittest.TestCase):
    def test_registered_programs_own_every_case_and_completion_once(self):
        cases = [case for group in PROGRAM_CASES.values() for case in group]
        self.assertEqual(len(cases), len(set(cases)))
        self.assertEqual(set(cases), set(CONTRACTS))
        self.assertEqual(set(PROGRAMS), set(PROGRAM_SUCCESS))
        root = Path(__file__).resolve().parents[2]
        for program in PROGRAMS:
            source = root / "tests/guest/portable" / f"{program}-differential.c"
            source_text = source.read_text()
            self.assertIn(PROGRAM_SUCCESS[program], source_text)
            for case in PROGRAM_CASES[program]:
                for assertion in CONTRACTS[case][2].split():
                    self.assertIn(assertion, source_text, (program, case, assertion))

    def test_requires_every_assertion_and_exact_linux_version(self):
        with test_tmpdir() as temporary:
            path = Path(temporary) / "console.log"
            path.write_text(transcript())
            parse_abi_log(path, linux=True)
            invalid = [
                transcript().replace(expected_records()[1] + "\n", ""),
                transcript() + expected_records()[1] + "\n",
                transcript().replace("7.2.3", "7.2.30"),
                transcript().replace("7.2.3", "7.2.3-custom"),
                transcript().replace(COMPLETE_MARKER, ""),
                transcript().replace(" pass", " skip", 1),
                transcript().replace(" enosys", " pass", 1),
                transcript().replace(COMPLETE_MARKER, "# echo " + COMPLETE_MARKER),
                transcript().replace("Linux version", "echo Linux version"),
                "Linux version 7.1.0 (other)\n" + transcript(),
                COMPLETE_MARKER + "\n" + transcript().replace(COMPLETE_MARKER + "\n", ""),
                transcript().replace(expected_records()[0] + "\n" + expected_records()[1],
                                     expected_records()[1] + "\n" + expected_records()[0]),
                transcript().replace(PROGRAM_COMPLETIONS[0] + "\n", ""),
                transcript() + PROGRAM_COMPLETIONS[0] + "\n",
                transcript() + "THEKERNEL_FSATTRS_FAIL cleanup\n",
                transcript() + "THEKERNEL_TEST_SKIP unavailable\n",
                transcript().replace("result=0", "result=1", 1),
            ]
            for text in invalid:
                with self.subTest(text=text[-80:]):
                    path.write_text(text)
                    with self.assertRaises(RunnerError):
                        parse_abi_log(path, linux=True)

    def test_pair_uses_equivalent_private_rootfs_and_cleans_images(self):
        with test_tmpdir() as temporary:
            directory = Path(temporary)
            source = directory / "source.img"
            source.write_bytes(b"same rootfs")
            targets = []
            for name in ("baseline", "linux"):
                kernel, esp = directory / (name + ".kernel"), directory / (name + ".esp")
                kernel.write_bytes(b"kernel")
                esp.write_bytes(b"esp")
                targets.append(BenchmarkTarget(name, kernel, esp))
            calls = []
            def fake_run(config):
                calls.append(config)
                self.assertEqual(config.rootfs.read_bytes(), b"same rootfs")
                self.assertNotEqual(config.rootfs, source)
                self.assertEqual(config.kernel.read_bytes(), b"kernel")
                self.assertEqual(config.esp.read_bytes(), b"esp")
                if len(calls) == 1:
                    targets[1].kernel.write_bytes(b"source changed during trial")
                    targets[1].esp.write_bytes(b"source ESP changed during trial")
                config.rootfs.write_bytes(b"guest mutation")
                config.log_path.write_text(transcript())
                command_lines = config.input_path.read_text().splitlines()
                self.assertEqual(command_lines[0], "failed=0")
                programs = [line for line in command_lines if line.startswith("/opt/thekernel-tests/portable/")]
                self.assertEqual(len(programs), len(PROGRAMS))
                self.assertIn("/opt/thekernel-tests/portable/unix-write-credentials-differential --require-id-change; result=$?", programs)
                self.assertTrue(all(line.endswith("; result=$?") for line in programs))
                self.assertEqual(command_lines.count('[ "$result" = 0 ] || failed=1'), len(PROGRAMS))
                self.assertEqual(sum("THEKERNEL_TEST_BEGIN" in line for line in command_lines), len(PROGRAMS))
                self.assertEqual(sum("THEKERNEL_TEST_END" in line for line in command_lines), len(PROGRAMS))
                printk = "echo 3 > /proc/sys/kernel/printk || failed=1"
                if config.workdir.name == "linux":
                    self.assertEqual(command_lines[1], printk)
                else:
                    self.assertNotIn(printk, command_lines)
                self.assertIn('[ "$failed" = 0 ] && echo ' + COMPLETE_MARKER, command_lines)
                self.assertTrue(all(len(line) < 128 for line in command_lines))
                return RunResult(returncode=0, log_path=config.log_path)
            with patch("tools.qemu_runner.abi_differential.validate_linux_esp_kernel") as validate, patch("tools.qemu_runner.abi_differential.validate_thekernel_esp_kernel") as validate_tk, patch("tools.qemu_runner.abi_differential.run", side_effect=fake_run):
                output = run_abi_differential(AbiConfig(tuple(targets), source, directory))
            validate.assert_called_once_with(calls[1].kernel, calls[1].esp)
            validate_tk.assert_called_once_with(calls[0].kernel, calls[0].esp)
            self.assertNotEqual(calls[1].kernel, targets[1].kernel)
            self.assertEqual(source.read_bytes(), b"same rootfs")
            self.assertEqual(len(calls), 2)
            for field in ("cpus", "memory", "accel", "rootfs_transport", "rootfs_mode", "graphics_profile"):
                self.assertEqual(getattr(calls[0], field), getattr(calls[1], field))
            self.assertFalse((output / "rootfs-base.img").exists())
            self.assertTrue(all(not call.rootfs.exists() for call in calls))
            self.assertTrue(all(not call.kernel.exists() and not call.esp.exists() for call in calls))
            self.assertTrue(all(call.log_path.exists() for call in calls))

    def test_guest_failure_cannot_pass_with_success_markers(self):
        with test_tmpdir() as temporary:
            directory = Path(temporary)
            source = directory / "image"
            source.write_bytes(b"data")
            targets = tuple(BenchmarkTarget(name, source, source) for name in ("baseline", "linux"))
            outcomes = (
                {"returncode": -15},
                {"returncode": 124, "runner_terminated": True, "runner_termination_reason": "timeout"},
                {"returncode": 0, "runner_terminated": True, "runner_termination_reason": "stop-after-marker"},
                {"returncode": 0, "error_message": "runner failure after guest output"},
                {"returncode": 0, "runner_termination_reason": "unexpected-stop"},
            )
            for outcome in outcomes:
                def fake_run(config):
                    config.log_path.write_text(transcript())
                    return RunResult(log_path=config.log_path, **outcome)
                with self.subTest(outcome=outcome), patch("tools.qemu_runner.abi_differential.validate_linux_esp_kernel"), patch("tools.qemu_runner.abi_differential.validate_thekernel_esp_kernel"), patch("tools.qemu_runner.abi_differential.run", side_effect=fake_run):
                    with self.assertRaisesRegex(RunnerError, "guest failed"):
                        run_abi_differential(AbiConfig(targets, source, directory))
            self.assertFalse(list(directory.glob("abi-*/*/rootfs.img")))

    def test_stale_linux_payload_is_rejected_before_any_guest(self):
        with test_tmpdir() as temporary:
            directory = Path(temporary)
            source = directory / "image"
            source.write_bytes(b"data")
            targets = tuple(BenchmarkTarget(name, source, source) for name in ("baseline", "linux"))
            with patch("tools.qemu_runner.abi_differential.validate_thekernel_esp_kernel"), patch("tools.qemu_runner.abi_differential.validate_linux_esp_kernel", side_effect=RunnerError("stale Linux ESP")), patch("tools.qemu_runner.abi_differential.run") as execute:
                with self.assertRaisesRegex(RunnerError, "stale Linux ESP"):
                    run_abi_differential(AbiConfig(targets, source, directory))
                execute.assert_not_called()
            self.assertFalse(list(directory.glob("abi-*/*/kernel")))
            self.assertFalse(list(directory.glob("abi-*/*/boot.esp")))

    def test_no_build_never_rebuilds_linux_esp(self):
        from tools import thekernel as product
        with test_tmpdir() as temporary, ExitStack() as stack:
            directory = Path(temporary)
            kernel = directory / "linux"
            kernel.write_bytes(b"kernel")
            args = product.build_parser().parse_args([
                "test", "--suite", "abi", "--no-build", "--accel", "kvm",
                "--linux-kernel", str(kernel),
                "--rootfs", str(directory / "graphics.img"),
            ])
            stack.enter_context(patch.object(product, "state_root", return_value=directory))
            stack.enter_context(patch.object(product, "state_lock", side_effect=lambda *a, **k: nullcontext()))
            build = stack.enter_context(patch.object(product, "run_checked"))
            stack.enter_context(patch.object(product, "validate_artifact_config"))
            execute = stack.enter_context(patch.object(product, "run_abi_differential", return_value=directory))
            with self.assertRaisesRegex(product.ProductError, "existing Linux ESP"):
                product.abi_test_cmd(args)
            execute.assert_not_called()
            esp = directory / "out/linux-7.2.3/abi.esp"
            esp.parent.mkdir(parents=True)
            esp.write_bytes(b"esp")
            with redirect_stdout(StringIO()):
                self.assertEqual(product.abi_test_cmd(args), 0)
            build.assert_not_called()
            execute.assert_called_once()
            self.assertNotEqual(execute.call_args.args[0].rootfs, Path(args.rootfs))

    def test_all_invokes_each_suite_and_propagates_abi_failure(self):
        from tools import thekernel as product
        args = product.build_parser().parse_args([
            "test", "--suite", "all", "--rootfs", "graphics.img", "--screenshot", "screen.png",
        ])
        calls = []
        with ExitStack() as stack:
            for name in ("host_test_cmd", "system_test_cmd", "abi_test_cmd", "graphics_smoke_cmd", "cpu_test_cmd"):
                stack.enter_context(patch.object(product, name, side_effect=lambda *a, _name=name: calls.append(_name) or 0))
            stack.enter_context(patch.object(product, "run_checked", side_effect=lambda command: calls.append("static-abi")))
            self.assertEqual(product.test_cmd(args), 0)
            self.assertEqual(calls, ["host_test_cmd", "system_test_cmd", "static-abi", "abi_test_cmd", "graphics_smoke_cmd", "cpu_test_cmd"])
            calls.clear()
            with patch.object(product, "abi_test_cmd", return_value=7):
                self.assertEqual(product.test_cmd(args), 7)
            self.assertEqual(calls, ["host_test_cmd", "system_test_cmd", "static-abi"])
