from __future__ import annotations

import io
import json
import os
import pty
import signal
import subprocess
import socket
import sys
import tempfile
from tests.support import test_tmpdir
import threading
import time
import unittest
from pathlib import Path
from unittest.mock import patch

from tools.qemu_runner.model import Interaction, QmpCheckpoint, QmpColorBlock, QmpPciHotplug, RunLimits
from tools.qemu_runner.process import ProcessError, run_process, _pin_vcpu_threads, _QmpController


class ProcessTests(unittest.TestCase):
    def test_diagnostics_cannot_satisfy_console_success_marker(self):
        with test_tmpdir() as directory:
            root = Path(directory)
            diagnostic = root / "kernel.log"
            console = io.BytesIO()
            code = ("from pathlib import Path; import time; "
                    f"Path({str(diagnostic)!r}).write_text('SUCCESS\\n'); "
                    "print('shell output', flush=True); time.sleep(10)")
            result = run_process(command=(sys.executable, "-c", code),
                workdir=root, log_path=root / "console.log",
                diagnostic_log_path=diagnostic, limits=RunLimits(total_timeout_secs=0.3),
                interaction=Interaction(stop_after_marker="SUCCESS"), console_stream=console)
            self.assertEqual(result.returncode, 124)
            self.assertFalse(result.marker_success)
            self.assertEqual(result.diagnostic_log_path, diagnostic)
            self.assertNotIn(b"SUCCESS", console.getvalue())
            self.assertNotIn("SUCCESS", result.log_path.read_text())
            self.assertIn(str(diagnostic), result.error_message)
            self.assertIn(str(result.log_path), result.error_message)


    def test_termination_signals_reap_child_and_unwind_caller_cleanup(self) -> None:
        for signum in (signal.SIGTERM, signal.SIGHUP):
            with self.subTest(signum=signum), test_tmpdir() as directory:
                root = Path(directory)
                pid_path = root / "child.pid"
                cleanup_path = root / "cleaned"
                child_code = (
                    "import os, pathlib, time; "
                    f"pathlib.Path({str(pid_path)!r}).write_text(str(os.getpid())); "
                    "time.sleep(60)"
                )
                runner_code = (
                    "from pathlib import Path; "
                    "from tools.qemu_runner.process import run_process; "
                    "from tools.qemu_runner.model import RunLimits, Interaction\n"
                    "try:\n"
                    f"    run_process(command=({sys.executable!r}, '-c', {child_code!r}), "
                    f"workdir=Path({str(root)!r}), log_path=Path({str(root / 'serial.log')!r}), "
                    "limits=RunLimits(), interaction=Interaction())\n"
                    "finally:\n"
                    f"    Path({str(cleanup_path)!r}).write_text('cleaned')\n"
                )
                runner = subprocess.Popen(
                    (sys.executable, "-c", runner_code),
                    cwd=Path(__file__).resolve().parents[2],
                    stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
                )
                child_pid = None
                try:
                    deadline = time.monotonic() + 5
                    while time.monotonic() < deadline:
                        if pid_path.exists() and pid_path.read_text():
                            child_pid = int(pid_path.read_text())
                            break
                        self.assertIsNone(runner.poll())
                        time.sleep(0.01)
                    self.assertIsNotNone(child_pid, "child did not start")
                    runner.send_signal(signum)
                    _, stderr = runner.communicate(timeout=8)
                    self.assertEqual(runner.returncode, 128 + signum, stderr.decode())
                    self.assertEqual(cleanup_path.read_text(), "cleaned")
                    with self.assertRaises(ProcessLookupError):
                        os.killpg(child_pid, 0)
                finally:
                    if runner.poll() is None:
                        runner.kill()
                    runner.communicate()
                    if child_pid is not None:
                        try:
                            os.killpg(child_pid, signal.SIGKILL)
                        except ProcessLookupError:
                            pass

    def test_signal_during_launch_is_deferred_until_child_cleanup(self) -> None:
        original_popen = subprocess.Popen
        children = []
        handled = []

        def previous_handler(signum, frame):
            handled.append((signum, children[0].poll()))

        def launch_then_signal(*args, **kwargs):
            child = original_popen(*args, **kwargs)
            children.append(child)
            signal.raise_signal(signal.SIGTERM)
            return child

        previous = signal.signal(signal.SIGTERM, previous_handler)
        try:
            with test_tmpdir() as directory:
                root = Path(directory)
                with patch("tools.qemu_runner.process.subprocess.Popen", launch_then_signal):
                    result = run_process(
                        command=(sys.executable, "-c", "import time; time.sleep(60)"),
                        workdir=root, log_path=root / "serial.log",
                        limits=RunLimits(), interaction=Interaction(),
                    )
                self.assertEqual(result.returncode, 128 + signal.SIGTERM)
                self.assertEqual(result.runner_termination_reason, "interrupted")
                self.assertEqual(handled, [(signal.SIGTERM, -signal.SIGTERM)])
                self.assertIs(signal.getsignal(signal.SIGTERM), previous_handler)
        finally:
            signal.signal(signal.SIGTERM, previous)

    def test_ignored_signal_stays_ignored_and_handlers_are_restored(self) -> None:
        original_term = signal.getsignal(signal.SIGTERM)
        original_hup = signal.signal(signal.SIGHUP, signal.SIG_IGN)
        try:
            with test_tmpdir() as directory:
                root = Path(directory)
                result = run_process(
                    command=(sys.executable, "-c",
                             "import os, signal; os.kill(os.getppid(), signal.SIGHUP)"),
                    workdir=root, log_path=root / "serial.log",
                    limits=RunLimits(total_timeout_secs=2), interaction=Interaction(),
                )
                self.assertEqual(result.returncode, 0)
                self.assertEqual(signal.getsignal(signal.SIGTERM), original_term)
                self.assertEqual(signal.getsignal(signal.SIGHUP), signal.SIG_IGN)
        finally:
            signal.signal(signal.SIGHUP, original_hup)

    def test_library_worker_thread_does_not_install_signal_handlers(self) -> None:
        results = []
        errors = []
        with test_tmpdir() as directory:
            root = Path(directory)

            def run():
                try:
                    results.append(run_process(
                        command=(sys.executable, "-c", "print('done')"),
                        workdir=root, log_path=root / "serial.log",
                        limits=RunLimits(total_timeout_secs=2), interaction=Interaction(),
                    ))
                except BaseException as error:
                    errors.append(error)

            with patch("tools.qemu_runner.process.signal.signal") as install:
                thread = threading.Thread(target=run)
                thread.start()
                thread.join(timeout=5)
                self.assertFalse(thread.is_alive())
                install.assert_not_called()
            self.assertEqual(errors, [])
            self.assertEqual([result.returncode for result in results], [0])

    def test_configured_guest_failure_stops_and_preserves_reason(self) -> None:
        marker = "THEKERNEL_Q35_WESTON_READY state=FAIL"
        for tail in ("", "; import time; time.sleep(60)"):
            with self.subTest(tail=tail), test_tmpdir() as directory:
                root = Path(directory)
                result = run_process(
                    command=(sys.executable, "-c",
                             "print('" + marker + " reason=input_udev_settle', flush=True)" + tail),
                    workdir=root, log_path=root / "console.log",
                    limits=RunLimits(total_timeout_secs=2),
                    interaction=Interaction(failure_prefixes=(marker,)),
                )
                self.assertEqual(result.returncode, 4)
                self.assertIn(marker + " reason=input_udev_settle", result.error_message)
                self.assertFalse(result.marker_success)

    def test_guest_failure_prefix_requires_token_boundary(self) -> None:
        marker = "THEKERNEL_Q35_WESTON_READY state=FAIL"
        with test_tmpdir() as directory:
            root = Path(directory)
            result = run_process(
                command=(sys.executable, "-c", "print('" + marker + "URE')"),
                workdir=root, log_path=root / "console.log",
                limits=RunLimits(total_timeout_secs=2),
                interaction=Interaction(failure_prefixes=(marker,)),
            )
            self.assertEqual(result.returncode, 0)
            self.assertIsNone(result.error_message)

    def test_unexpected_wait_exception_reaps_child(self) -> None:
        children = []

        def broken_wait(process, **kwargs):
            children.append(process)
            self.assertIsNone(process.poll())
            raise TypeError("console requires bytes")

        with test_tmpdir() as directory:
            root = Path(directory)
            with patch("tools.qemu_runner.process._wait_for_process", broken_wait):
                with self.assertRaisesRegex(TypeError, "console requires bytes"):
                    run_process(
                        command=(sys.executable, "-c", "import time; time.sleep(60)"),
                        workdir=root,
                        log_path=root / "console.log",
                        limits=RunLimits(),
                        interaction=Interaction(),
                    )
            self.assertEqual(len(children), 1)
            self.assertIsNotNone(children[0].poll())
            self.assertTrue(children[0].stdout.closed)

    def start_qmp_server(
        self,
        path: Path,
        *,
        reject: str | None = None,
        screendump_delay_secs: float = 0.0,
        device_deleted_device: str | None = None,
        device_deleted_before_reply: bool = False,
        responses: dict | None = None,
        screenshots: tuple[bytes, ...] | None = None,
    ):
        listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        listener.bind(str(path))
        listener.listen(1)
        commands: list[dict[str, object]] = []
        ready = threading.Event()

        def server() -> None:
            screenshot_index = 0
            ready.set()
            try:
                client, _ = listener.accept()
                with client:
                    client.sendall(b'{"event":"RESET"}\r\n')
                    client.sendall(b'{"QMP":{"version":{},"capabilities":[]}}\r\n')
                    buffer = bytearray()
                    while True:
                        chunk = client.recv(4096)
                        if not chunk:
                            return
                        buffer.extend(chunk)
                        while b"\n" in buffer:
                            raw, _, remainder = buffer.partition(b"\n")
                            buffer[:] = remainder
                            request = json.loads(raw)
                            commands.append(request)
                            command = request["execute"]
                            request_id = request["id"]
                            client.sendall(b'{"event":"DEVICE_DELETED"}\r\n')
                            client.sendall(b'{"id":"unrelated","return":{}}\r\n')
                            if command == reject:
                                client.sendall(json.dumps({"id": request_id, "error": {"class": "GenericError"}}).encode() + b"\r\n")
                                return
                            if command == "device_del" and device_deleted_device is not None and device_deleted_before_reply:
                                client.sendall(
                                    json.dumps({"event": "DEVICE_DELETED", "data": {"device": device_deleted_device}}).encode()
                                    + b"\r\n"
                                )
                            if command == "screendump":
                                if screendump_delay_secs:
                                    time.sleep(screendump_delay_secs)
                                target = Path(request["arguments"]["filename"])
                                target.write_bytes(
                                    screenshots[min(screenshot_index, len(screenshots) - 1)] if screenshots
                                    else b"P6\n2 1\n255\n\x01\x02\x03\x01\x02\x03"
                                )
                                screenshot_index += 1
                            client.sendall(json.dumps({"id": request_id, "return": (responses or {}).get(command, {})}).encode() + b"\r\n")
                            if command == "device_del" and device_deleted_device is not None and not device_deleted_before_reply:
                                client.sendall(
                                    json.dumps({"event": "DEVICE_DELETED", "data": {"device": device_deleted_device}}).encode()
                                    + b"\r\n"
                                )
            finally:
                listener.close()

        thread = threading.Thread(target=server, daemon=True)
        thread.start()
        self.addCleanup(thread.join, 1.0)
        self.addCleanup(listener.close)
        self.assertTrue(ready.wait(1.0))
        return commands

    def test_vcpu_thread_map_pins_only_verified_members_and_reads_back(self):
        masks = {100: {2, 4, 6}, 101: {2, 4, 6}, 102: {2, 4, 6}}
        calls = []
        def set_mask(tid, mask):
            calls.append((tid, mask))
            masks[tid] = mask
        response = [{"cpu-index": 1, "thread-id": 102}, {"cpu-index": 0, "thread-id": 101}]
        with patch("tools.qemu_runner.process.Path.is_dir", return_value=True), \
             patch("tools.qemu_runner.process.os.sched_getaffinity", side_effect=lambda tid: masks[tid]), \
             patch("tools.qemu_runner.process.os.sched_setaffinity", side_effect=set_mask):
            self.assertEqual(_pin_vcpu_threads(100, response, (4, 2)), ((0, 101, 4), (1, 102, 2)))
        self.assertEqual(calls, [(101, {4}), (102, {2})])
        self.assertEqual(masks[100], {2, 4, 6})

    def test_vcpu_thread_map_rejects_foreign_missing_and_duplicate_threads(self):
        valid = [{"cpu-index": 0, "thread-id": 101}, {"cpu-index": 1, "thread-id": 102}]
        invalid = [valid[:1], [valid[0], valid[0]],
                   [valid[0], {"cpu-index": 1, "thread-id": 101}],
                   [valid[0], {"cpu-index": 1, "thread-id": 100}]]
        with patch("tools.qemu_runner.process.Path.is_dir", return_value=True), \
             patch("tools.qemu_runner.process.os.sched_getaffinity", return_value={2, 4}), \
             patch("tools.qemu_runner.process.os.sched_setaffinity") as setter:
            for response in invalid:
                with self.assertRaises(ProcessError):
                    _pin_vcpu_threads(100, response, (2, 4))
            setter.assert_not_called()
        with patch("tools.qemu_runner.process.Path.is_dir", return_value=False), \
             patch("tools.qemu_runner.process.os.sched_getaffinity", return_value={2, 4}):
            with self.assertRaisesRegex(ProcessError, "outside"):
                _pin_vcpu_threads(100, valid, (2, 4))

    def test_vcpu_affinity_readback_failure_restores_prior_mask(self):
        with patch("tools.qemu_runner.process.Path.is_dir", return_value=True), \
             patch("tools.qemu_runner.process.os.sched_getaffinity", return_value={2, 4}), \
             patch("tools.qemu_runner.process.os.sched_setaffinity") as setter:
            with self.assertRaisesRegex(ProcessError, "readback"):
                _pin_vcpu_threads(100, [{"cpu-index": 0, "thread-id": 101}], (2,))
            self.assertEqual(setter.call_args_list[0].args, (101, {2}))
            self.assertEqual(setter.call_args_list[1].args, (101, {2, 4}))

    def test_screenshot_polling_only_retries_valid_color_mismatches(self):
        desired = b"P6\n2 1\n255\n" + bytes((1, 2, 3)) * 2
        previous = b"P6\n2 1\n255\n" + bytes((9, 8, 7)) * 2
        for images, success, expected_error, count in (
            ((previous, desired), True, None, 2),
            ((previous,), False, "color block did not match", None),
            ((b"not a ppm",), False, "not a P6", 1),
            ((b"P6\n1 1\n255\n" + bytes((1, 2, 3)),), False, "dimensions", 1),
        ):
            with self.subTest(images=images), test_tmpdir() as directory:
                root = Path(directory)
                commands = self.start_qmp_server(root / "qmp.sock", screenshots=images)
                controller = _QmpController(
                    socket_path=root / "qmp.sock", screenshot=root / "screen.ppm",
                    input_events=(), input_after_marker=None, screenshot_after_marker=None,
                    timeout_secs=0.2, screenshot_size=(2, 1),
                    screenshot_color_blocks=(QmpColorBlock(0, 0, 2, 1, (1, 2, 3)),),
                    checkpoints=(), vcpu_host_cpus=(), qemu_pid=None,
                )
                controller.start()
                self.assertTrue(controller._finished.wait(2))
                controller.close()
                self.assertEqual(controller.complete, success)
                if expected_error:
                    self.assertIn(expected_error, str(controller.error))
                else:
                    self.assertIsNone(controller.error)
                dumps = [entry for entry in commands if entry["execute"] == "screendump"]
                if count is not None:
                    self.assertEqual(len(dumps), count)
                else:
                    self.assertGreater(len(dumps), 1)

    def test_qmp_failed_pinning_never_resumes_guest(self):
        with test_tmpdir() as temporary:
            path = Path(temporary) / "qmp.sock"
            commands = self.start_qmp_server(path, responses={"query-status": {"running": False},
                "query-cpus-fast": [{"cpu-index": 0, "thread-id": 101}]})
            controller = _QmpController(socket_path=path, screenshot=None, input_events=(),
                input_after_marker=None, screenshot_after_marker=None, timeout_secs=2,
                screenshot_size=None, screenshot_color_blocks=(), checkpoints=(),
                vcpu_host_cpus=(2,), qemu_pid=100)
            with patch("tools.qemu_runner.process._pin_vcpu_threads", side_effect=ProcessError("readback")):
                controller.start()
                self.assertTrue(controller._finished.wait(2))
                controller.close()
            self.assertIsNotNone(controller.error)
            self.assertFalse(controller.complete)
            self.assertNotIn("cont", [entry["execute"] for entry in commands])

    def test_qmp_resumes_only_after_verified_pinning(self):
        with test_tmpdir() as temporary:
            path = Path(temporary) / "qmp.sock"
            commands = self.start_qmp_server(path, responses={"query-status": {"running": False},
                "query-cpus-fast": [{"cpu-index": 0, "thread-id": 101}]})
            controller = _QmpController(socket_path=path, screenshot=None, input_events=(),
                input_after_marker=None, screenshot_after_marker=None, timeout_secs=2,
                screenshot_size=None, screenshot_color_blocks=(), checkpoints=(),
                vcpu_host_cpus=(2,), qemu_pid=100)
            def pin(pid, response, cpus):
                self.assertEqual([entry["execute"] for entry in commands],
                                 ["qmp_capabilities", "query-status", "query-cpus-fast"])
                return ((0, 101, 2),)
            with patch("tools.qemu_runner.process._pin_vcpu_threads", side_effect=pin):
                controller.start()
                self.assertTrue(controller._finished.wait(2))
                controller.close()
            self.assertIsNone(controller.error)
            self.assertTrue(controller.complete)
            self.assertEqual(commands[-1]["execute"], "cont")
            self.assertEqual(controller.vcpu_affinity, ((0, 101, 2),))

    def run_child(
        self,
        script: str,
        *,
        limits: RunLimits = RunLimits(total_timeout_secs=2.0),
        interaction: Interaction = Interaction(),
        input_text: bytes = b"",
    ):
        temporary = test_tmpdir()
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name)
        input_file = tempfile.TemporaryFile()
        self.addCleanup(input_file.close)
        input_file.write(input_text)
        input_file.seek(0)
        console = io.BytesIO()
        result = run_process(
            command=(sys.executable, "-c", script),
            workdir=root,
            log_path=root / "console.log",
            limits=limits,
            interaction=interaction,
            input_stream=input_file,
            console_stream=console,
        )
        return result, (root / "console.log").read_bytes(), console.getvalue()

    def test_serial_output_is_logged(self) -> None:
        result, log, _ = self.run_child("print('hello', flush=True)")
        self.assertEqual(result.returncode, 0, result.error_message)
        self.assertEqual(log, b"hello\n")

    def test_total_timeout_is_bounded(self) -> None:
        result, _, _ = self.run_child(
            "import time; time.sleep(5)",
            limits=RunLimits(total_timeout_secs=0.1),
        )
        self.assertEqual(result.returncode, 124)
        self.assertIn("timed out", result.error_message or "")

    def test_case_watchdog_names_the_hung_case(self) -> None:
        result, log, _ = self.run_child(
            "import time; print('# THEKERNEL_TEST_BEGIN 3 hung timeout_seconds=0', flush=True); time.sleep(5)",
            limits=RunLimits(total_timeout_secs=2),
        )
        self.assertEqual(result.returncode, 124)
        self.assertIn("3 hung", result.error_message or "")
        self.assertIn(b"THEKERNEL_TEST_BEGIN", log)

    def test_exit_with_unfinished_case_fails(self) -> None:
        result, _, _ = self.run_child("print('# THEKERNEL_TEST_BEGIN 1 unfinished timeout_seconds=60')")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("1 unfinished", result.error_message or "")

    def test_completed_case_cancels_watchdog(self) -> None:
        result, _, _ = self.run_child(
            "print('# THEKERNEL_TEST_BEGIN 3 fast timeout_seconds=0'); "
            "print('# THEKERNEL_TEST_END 3 fast result=0', flush=True)",
        )
        self.assertEqual(result.returncode, 0, result.error_message)

    def test_input_is_forwarded_only_after_exact_marker(self) -> None:
        script = (
            "import sys; "
            "print('READY', flush=True); "
            "line=sys.stdin.readline().strip(); "
            "print('GOT:'+line, flush=True)"
        )
        result, log, console = self.run_child(
            script,
            limits=RunLimits(total_timeout_secs=2.0),
            interaction=Interaction(interactive=True, input_after_marker="READY"),
            input_text=b"payload\n",
        )
        self.assertEqual(result.returncode, 0, result.error_message)
        self.assertIn(b"GOT:payload", log)
        self.assertEqual(console, log)

    def test_command_file_releases_one_complete_line_per_prompt(self) -> None:
        script = """
import os, select
for index in range(3):
    assert not select.select([0], [], [], 0.05)[0], 'input before prompt'
    print('READY', flush=True)
    received = bytearray()
    while not received.endswith(b'\\n'):
        chunk = os.read(0, 4096)
        assert chunk, 'truncated command'
        received.extend(chunk)
    expected = (str(index) + ':' + 'x' * 700 + '\\n').encode()
    assert received == expected, (len(received), len(expected))
    assert not select.select([0], [], [], 0.05)[0], 'next command sent early'
print('DONE', flush=True)
"""
        payload = b"".join((str(i) + ":" + "x" * 700 + "\n").encode() for i in range(4))
        result, log, _ = self.run_child(
            script,
            interaction=Interaction(interactive=True, input_after_marker="READY",
                                    input_line_after_marker="READY"),
            input_text=payload,
        )
        self.assertEqual(result.returncode, 0, result.error_message or log.decode())
        self.assertIn(b"DONE", log)

    def test_plain_interactive_mode_preserves_tty_stdin(self) -> None:
        master_fd, slave_fd = pty.openpty()
        self.addCleanup(os.close, master_fd)
        input_file = os.fdopen(slave_fd, "rb", buffering=0)
        self.addCleanup(input_file.close)
        temporary = test_tmpdir()
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name)
        console = io.BytesIO()

        result = run_process(
            command=(
                sys.executable,
                "-c",
                "import os; print('TTY='+str(os.isatty(0)), flush=True)",
            ),
            workdir=root,
            log_path=root / "console.log",
            limits=RunLimits(total_timeout_secs=2.0),
            interaction=Interaction(interactive=True),
            input_stream=input_file,
            console_stream=console,
        )

        self.assertEqual(result.returncode, 0, result.error_message)
        self.assertEqual((root / "console.log").read_bytes(), b"TTY=True\n")

    def test_early_guest_exit_does_not_claim_complete_input(self) -> None:
        payload = b"command-padding\n" * 100_000
        result, _, _ = self.run_child(
            "print('READY', flush=True)",
            limits=RunLimits(total_timeout_secs=2.0),
            interaction=Interaction(interactive=True, input_after_marker="READY"),
            input_text=payload,
        )
        self.assertEqual(result.returncode, 0, result.error_message)

    def test_nonreading_guest_does_not_bypass_timeout_while_forwarding(self) -> None:
        result, _, _ = self.run_child(
            "import time; print('READY', flush=True); time.sleep(5)",
            limits=RunLimits(total_timeout_secs=0.25),
            interaction=Interaction(interactive=True, input_after_marker="READY"),
            input_text=b"command-padding\n" * 100_000,
        )
        self.assertEqual(result.returncode, 124)
        self.assertIn("timed out", result.error_message or "")

    def test_near_marker_does_not_open_input(self) -> None:
        result, _, _ = self.run_child(
            "print('READY ', flush=True)",
            limits=RunLimits(total_timeout_secs=2.0),
            interaction=Interaction(interactive=True, input_after_marker="READY"),
            input_text=b"payload\n",
        )
        self.assertEqual(result.returncode, 4)
        self.assertIn("before input-ready marker", result.error_message or "")

    def test_stop_after_exact_marker_returns_75(self) -> None:
        result, log, _ = self.run_child(
            "import time; print('STOP', flush=True); time.sleep(5)",
            interaction=Interaction(stop_after_marker="STOP"),
        )
        self.assertTrue(result.intentionally_stopped)
        self.assertTrue(result.marker_success)
        self.assertTrue(result.runner_terminated)
        self.assertEqual(result.runner_termination_reason, "stop-after-marker")
        self.assertFalse(result.guest_clean_shutdown)
        self.assertIn(b"STOP", log)

    def test_rejects_negative_passed_descriptor(self) -> None:
        with test_tmpdir() as directory:
            root = Path(directory)
            with self.assertRaisesRegex(ProcessError, "non-negative"):
                run_process(
                    command=(sys.executable, "-c", "pass"),
                    workdir=root,
                    log_path=root / "console.log",
                    limits=RunLimits(),
                    interaction=Interaction(),
                    pass_fds=(-1,),
                )

    def test_passed_descriptor_is_available_to_child(self) -> None:
        with test_tmpdir() as directory:
            root = Path(directory)
            image = root / "image"
            image.write_bytes(b"opened-once")
            fd = os.open(image, os.O_RDONLY)
            self.addCleanup(os.close, fd)
            result = run_process(
                command=(
                    sys.executable,
                    "-c",
                    "import os, sys; os.write(1, os.read(int(sys.argv[1]), 64))",
                    str(fd),
                ),
                workdir=root,
                log_path=root / "console.log",
                limits=RunLimits(),
                interaction=Interaction(),
                pass_fds=(fd,),
            )
            self.assertEqual(result.returncode, 0, result.error_message)
            self.assertEqual((root / "console.log").read_bytes(), b"opened-once")

    def test_qmp_waits_for_serial_markers_and_ignores_events_and_other_ids(self) -> None:
        with test_tmpdir() as directory:
            root = Path(directory)
            qmp_socket = root / "qmp.sock"
            screenshot = root / "screen.ppm"
            commands = self.start_qmp_server(qmp_socket)
            result = run_process(
                command=(
                    sys.executable,
                    "-c",
                    "import time; print('INPUT_READY', flush=True); time.sleep(.15); print('VISUAL_READY', flush=True); time.sleep(.3)",
                ),
                workdir=root,
                log_path=root / "console.log",
                limits=RunLimits(total_timeout_secs=2.0),
                interaction=Interaction(),
                qmp_socket=qmp_socket,
                screenshot=screenshot,
                qmp_input_events=(({"type": "key", "data": {"down": True, "key": {"type": "qcode", "data": "a"}}},),),
                qmp_input_after_marker="INPUT_READY",
                qmp_screenshot_after_marker="VISUAL_READY",
                qmp_screenshot_size=(2, 1),
                qmp_screenshot_color_blocks=(QmpColorBlock(0, 0, 2, 1, (1, 2, 3)),),
            )
            self.assertEqual(result.returncode, 0, result.error_message)
            self.assertEqual([command["execute"] for command in commands], ["qmp_capabilities", "input-send-event", "screendump"])
            self.assertFalse(qmp_socket.exists())

    def test_qmp_device_del_waits_for_async_matching_device_deleted_before_readd(self) -> None:
        with test_tmpdir() as directory:
            root = Path(directory)
            qmp_socket = root / "qmp.sock"
            commands = self.start_qmp_server(qmp_socket, device_deleted_device="input-kbd")
            result = run_process(
                command=(sys.executable, "-c", "import time; print('READY', flush=True); time.sleep(.3)"),
                workdir=root,
                log_path=root / "console.log",
                limits=RunLimits(total_timeout_secs=2.0),
                interaction=Interaction(),
                qmp_socket=qmp_socket,
                qmp_checkpoints=(
                    QmpCheckpoint(
                        input_after_marker="READY",
                        pci_hotplug=(
                            QmpPciHotplug("del", "input-kbd"),
                            QmpPciHotplug("add", "input-kbd", "virtio-keyboard-pci", "rp-input-kbd"),
                        ),
                    ),
                ),
            )
            self.assertEqual(result.returncode, 0, result.error_message)
            self.assertEqual([command["execute"] for command in commands], ["qmp_capabilities", "device_del", "device_add"])

    def test_qmp_hotplug_protocol_waits_for_guest_remove_and_add_readiness_before_input(self) -> None:
        with test_tmpdir() as directory:
            root = Path(directory)
            qmp_socket = root / "qmp.sock"
            commands = self.start_qmp_server(qmp_socket, device_deleted_device="input-tablet")
            result = run_process(
                command=(
                    sys.executable,
                    "-c",
                    "import time; print('READY', flush=True); time.sleep(.1); "
                    "print('TABLET_REMOVED', flush=True); time.sleep(.1); "
                    "print('TABLET_READY', flush=True); time.sleep(.3)",
                ),
                workdir=root,
                log_path=root / "console.log",
                limits=RunLimits(total_timeout_secs=2.0),
                interaction=Interaction(),
                qmp_socket=qmp_socket,
                qmp_checkpoints=(
                    QmpCheckpoint(
                        input_after_marker="READY",
                        pci_hotplug=(QmpPciHotplug("del", "input-tablet"),),
                    ),
                    QmpCheckpoint(
                        input_after_marker="TABLET_REMOVED",
                        pci_hotplug=(
                            QmpPciHotplug("add", "input-tablet", "virtio-tablet-pci", "rp-input-tablet"),
                        ),
                    ),
                    QmpCheckpoint(
                        input_after_marker="TABLET_READY",
                        input_events=(({"type": "abs", "data": {"axis": "x", "value": 320}},),),
                    ),
                ),
            )
            self.assertEqual(result.returncode, 0, result.error_message)
            self.assertEqual(
                [command["execute"] for command in commands],
                ["qmp_capabilities", "device_del", "device_add", "input-send-event"],
            )

    def test_qmp_records_host_monotonic_input_to_visible_metrics(self) -> None:
        with test_tmpdir() as directory:
            root = Path(directory)
            qmp_socket = root / "qmp.sock"
            commands = self.start_qmp_server(qmp_socket)
            event = (({"type": "key", "data": {"down": True, "key": {"type": "qcode", "data": "a"}}},),)
            result = run_process(
                command=(
                    sys.executable,
                    "-c",
                    "import time; print('READY', flush=True); time.sleep(.05); "
                    "print('VISIBLE_000', flush=True); time.sleep(.05); "
                    "print('VISIBLE_001', flush=True); time.sleep(.1)",
                ),
                workdir=root,
                log_path=root / "console.log",
                limits=RunLimits(total_timeout_secs=2.0),
                interaction=Interaction(),
                qmp_socket=qmp_socket,
                qmp_timeout_secs=1.0,
                qmp_checkpoints=(
                    QmpCheckpoint(
                        input_after_marker="READY",
                        input_events=event,
                        latency_after_marker="VISIBLE_000",
                        latency_index=0,
                    ),
                    QmpCheckpoint(
                        input_after_marker="VISIBLE_000",
                        input_events=event,
                        latency_after_marker="VISIBLE_001",
                        latency_index=1,
                    ),
                ),
            )
            self.assertEqual(result.returncode, 0, result.error_message)
            metrics = [
                json.loads(line.split(" ", 1)[1])
                for line in (root / "console.log").read_text().splitlines()
                if line.startswith("THEKERNEL_GRAPHICS_METRIC ")
            ]
            self.assertEqual([metric["index"] for metric in metrics], [0, 1])
            self.assertTrue(all(metric["kind"] == "input_to_visible" for metric in metrics))
            self.assertTrue(all(metric["ns"] > 0 for metric in metrics))
            self.assertEqual(
                [command["execute"] for command in commands],
                ["qmp_capabilities", "input-send-event", "input-send-event"],
            )

    def test_qmp_device_del_times_out_after_unrelated_device_deleted_event(self) -> None:
        with test_tmpdir() as directory:
            root = Path(directory)
            qmp_socket = root / "qmp.sock"
            commands = self.start_qmp_server(qmp_socket, device_deleted_device="other-input")
            result = run_process(
                command=(sys.executable, "-c", "import time; print('READY', flush=True); time.sleep(5)"),
                workdir=root,
                log_path=root / "console.log",
                limits=RunLimits(total_timeout_secs=2.0),
                interaction=Interaction(),
                qmp_socket=qmp_socket,
                qmp_timeout_secs=0.15,
                qmp_checkpoints=(
                    QmpCheckpoint(
                        input_after_marker="READY",
                        pci_hotplug=(
                            QmpPciHotplug("del", "input-kbd"),
                            QmpPciHotplug("add", "input-kbd", "virtio-keyboard-pci", "rp-input-kbd"),
                        ),
                    ),
                ),
            )
            self.assertEqual(result.returncode, 4)
            self.assertIn("QMP timeout waiting for DEVICE_DELETED for device input-kbd", result.error_message or "")
            self.assertEqual([command["execute"] for command in commands], ["qmp_capabilities", "device_del"])

    def test_qmp_device_del_rejection_does_not_wait_or_readd(self) -> None:
        with test_tmpdir() as directory:
            root = Path(directory)
            qmp_socket = root / "qmp.sock"
            commands = self.start_qmp_server(qmp_socket, reject="device_del")
            result = run_process(
                command=(sys.executable, "-c", "import time; print('READY', flush=True); time.sleep(5)"),
                workdir=root,
                log_path=root / "console.log",
                limits=RunLimits(total_timeout_secs=2.0),
                interaction=Interaction(),
                qmp_socket=qmp_socket,
                qmp_checkpoints=(
                    QmpCheckpoint(
                        input_after_marker="READY",
                        pci_hotplug=(
                            QmpPciHotplug("del", "input-kbd"),
                            QmpPciHotplug("add", "input-kbd", "virtio-keyboard-pci", "rp-input-kbd"),
                        ),
                    ),
                ),
            )
            self.assertEqual(result.returncode, 4)
            self.assertIn("QMP rejected device_del", result.error_message or "")
            self.assertEqual([command["execute"] for command in commands], ["qmp_capabilities", "device_del"])

    def test_stop_after_graphics_marker_waits_for_screendump_oracle(self) -> None:
        with test_tmpdir() as directory:
            root = Path(directory)
            qmp_socket = root / "qmp.sock"
            screenshot = root / "screen.ppm"
            commands = self.start_qmp_server(qmp_socket, screendump_delay_secs=0.1)
            result = run_process(
                command=(
                    sys.executable,
                    "-c",
                    "import time; print('GRAPHICS_READY', flush=True); time.sleep(5)",
                ),
                workdir=root,
                log_path=root / "console.log",
                limits=RunLimits(total_timeout_secs=2.0),
                interaction=Interaction(stop_after_marker="GRAPHICS_READY"),
                qmp_socket=qmp_socket,
                screenshot=screenshot,
                qmp_input_events=(({
                    "type": "key",
                    "data": {"down": True, "key": {"type": "qcode", "data": "a"}},
                },),),
                qmp_input_after_marker="GRAPHICS_READY",
                qmp_screenshot_after_marker="GRAPHICS_READY",
                qmp_screenshot_size=(2, 1),
                qmp_screenshot_color_blocks=(QmpColorBlock(0, 0, 2, 1, (1, 2, 3)),),
            )
            self.assertTrue(result.intentionally_stopped, result.error_message)
            self.assertEqual(
                [command["execute"] for command in commands],
                ["qmp_capabilities", "input-send-event", "screendump"],
            )
            self.assertTrue(screenshot.is_file())

    def test_stop_after_graphics_marker_returns_qmp_oracle_failure(self) -> None:
        with test_tmpdir() as directory:
            root = Path(directory)
            qmp_socket = root / "qmp.sock"
            self.start_qmp_server(qmp_socket, reject="screendump")
            result = run_process(
                command=(
                    sys.executable,
                    "-c",
                    "import time; print('GRAPHICS_READY', flush=True); time.sleep(5)",
                ),
                workdir=root,
                log_path=root / "console.log",
                limits=RunLimits(total_timeout_secs=2.0),
                interaction=Interaction(stop_after_marker="GRAPHICS_READY"),
                qmp_socket=qmp_socket,
                screenshot=root / "screen.ppm",
                qmp_screenshot_after_marker="GRAPHICS_READY",
            )
            self.assertEqual(result.returncode, 4)
            self.assertFalse(result.intentionally_stopped)
            self.assertIn("rejected screendump", result.error_message or "")

    def test_qmp_error_terminates_guest_and_cleans_socket(self) -> None:
        with test_tmpdir() as directory:
            root = Path(directory)
            qmp_socket = root / "qmp.sock"
            self.start_qmp_server(qmp_socket, reject="input-send-event")
            result = run_process(
                command=(sys.executable, "-c", "import time; print('READY', flush=True); time.sleep(5)"),
                workdir=root,
                log_path=root / "console.log",
                limits=RunLimits(total_timeout_secs=2.0),
                interaction=Interaction(),
                qmp_socket=qmp_socket,
                qmp_input_events=(({"type": "key", "data": {"down": True, "key": {"type": "qcode", "data": "a"}}},),),
                qmp_input_after_marker="READY",
            )
            self.assertEqual(result.returncode, 4)
            self.assertIn("rejected input-send-event", result.error_message or "")
            self.assertFalse(qmp_socket.exists())

    def test_exit_before_qmp_marker_reports_raw_returncode(self) -> None:
        for script, expected in (
            ("import sys; sys.exit(0)", 0),
            ("import os, signal; os.kill(os.getpid(), signal.SIGKILL)", -9),
        ):
            with self.subTest(returncode=expected), test_tmpdir() as directory:
                root = Path(directory)
                qmp_socket = root / "qmp.sock"
                self.start_qmp_server(qmp_socket)
                result = run_process(
                    command=(sys.executable, "-c", script),
                    workdir=root,
                    log_path=root / "console.log",
                    limits=RunLimits(total_timeout_secs=2.0),
                    interaction=Interaction(),
                    qmp_socket=qmp_socket,
                    screenshot=root / "screen.ppm",
                    qmp_screenshot_after_marker="NEVER",
                )
                self.assertEqual(result.returncode, 4)
                self.assertFalse(result.intentionally_stopped)
                self.assertIn(f"returncode={expected}", result.error_message or "")

    def test_qmp_marker_timeout_terminates_guest_and_cleans_socket(self) -> None:
        with test_tmpdir() as directory:
            root = Path(directory)
            qmp_socket = root / "qmp.sock"
            commands = self.start_qmp_server(qmp_socket, responses={
                "human-monitor-command": "CPU#0\nRSP=ffff800001234000\nRIP=ffffffff81234567\n",
                "x-query-virtio": [{"name": "virtio-gpu", "path": "/gpu"}],
                "x-query-virtio-queue-status": {"used-idx": 7},
            })
            result = run_process(
                command=(sys.executable, "-c", "import time; print('BOOTING', flush=True); time.sleep(5)"),
                workdir=root,
                log_path=root / "console.log",
                limits=RunLimits(total_timeout_secs=2.0),
                interaction=Interaction(),
                qmp_socket=qmp_socket,
                qmp_input_events=(({"type": "key", "data": {"down": True, "key": {"type": "qcode", "data": "a"}}},),),
                qmp_input_after_marker="NEVER",
                qmp_timeout_secs=0.15,
            )
            self.assertEqual(result.returncode, 4)
            self.assertIn("QMP timeout waiting for checkpoint marker: NEVER", result.error_message or "")
            self.assertIn("RIP=ffffffff81234567", result.error_message or "")
            monitor = [command for command in commands if command["execute"] == "human-monitor-command"]
            self.assertEqual(monitor[0]["arguments"], {"command-line": "info registers -a"})
            self.assertEqual(monitor[1]["arguments"], {"command-line": "x /128gx 0xffff800001234000"})
            self.assertIn("CPU 0 kernel stack:", result.error_message or "")
            self.assertIn('"used-idx": 7', result.error_message or "")
            self.assertFalse(qmp_socket.exists())


if __name__ == "__main__":
    unittest.main()
