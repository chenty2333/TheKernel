from __future__ import annotations

import io
import json
import os
import pty
import socket
import sys
import tempfile
import threading
import time
import unittest
from pathlib import Path

from tools.qemu_runner.model import Interaction, QmpCheckpoint, QmpColorBlock, QmpPciHotplug, RunLimits
from tools.qemu_runner.process import ProcessError, run_process


class ProcessTests(unittest.TestCase):
    def start_qmp_server(
        self,
        path: Path,
        *,
        reject: str | None = None,
        screendump_delay_secs: float = 0.0,
        device_deleted_device: str | None = None,
        device_deleted_before_reply: bool = False,
    ):
        listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        listener.bind(str(path))
        listener.listen(1)
        commands: list[dict[str, object]] = []
        ready = threading.Event()

        def server() -> None:
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
                                target.write_bytes(b"P6\n2 1\n255\n\x01\x02\x03\x01\x02\x03")
                            client.sendall(json.dumps({"id": request_id, "return": {}}).encode() + b"\r\n")
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

    def run_child(
        self,
        script: str,
        *,
        limits: RunLimits = RunLimits(total_timeout_secs=2.0),
        interaction: Interaction = Interaction(),
        input_text: bytes = b"",
    ):
        temporary = tempfile.TemporaryDirectory()
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

    def test_plain_interactive_mode_preserves_tty_stdin(self) -> None:
        master_fd, slave_fd = pty.openpty()
        self.addCleanup(os.close, master_fd)
        input_file = os.fdopen(slave_fd, "rb", buffering=0)
        self.addCleanup(input_file.close)
        temporary = tempfile.TemporaryDirectory()
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
        with tempfile.TemporaryDirectory() as directory:
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
        with tempfile.TemporaryDirectory() as directory:
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
        with tempfile.TemporaryDirectory() as directory:
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
        with tempfile.TemporaryDirectory() as directory:
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
        with tempfile.TemporaryDirectory() as directory:
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
        with tempfile.TemporaryDirectory() as directory:
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
        with tempfile.TemporaryDirectory() as directory:
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
        with tempfile.TemporaryDirectory() as directory:
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
        with tempfile.TemporaryDirectory() as directory:
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
        with tempfile.TemporaryDirectory() as directory:
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
        with tempfile.TemporaryDirectory() as directory:
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

    def test_qmp_marker_timeout_terminates_guest_and_cleans_socket(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            qmp_socket = root / "qmp.sock"
            self.start_qmp_server(qmp_socket)
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
            self.assertFalse(qmp_socket.exists())


if __name__ == "__main__":
    unittest.main()
