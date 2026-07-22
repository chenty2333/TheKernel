from __future__ import annotations

import hashlib
import io
import os
import pty
import sys
import tempfile
import unittest
from pathlib import Path

from tools.qemu_runner.model import Interaction, RunLimits
from tools.qemu_runner.process import run_process


class ProcessTests(unittest.TestCase):
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
            arch="rv",
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
        self.assertTrue(result.ok)
        self.assertEqual(log, b"hello\n")

    def test_total_timeout_is_bounded(self) -> None:
        result, _, _ = self.run_child(
            "import time; time.sleep(5)",
            limits=RunLimits(total_timeout_secs=0.1),
        )
        self.assertEqual(result.returncode, 124)
        self.assertIn("timed out", result.error_message or "")

    def test_idle_timeout_tracks_console_activity(self) -> None:
        result, log, _ = self.run_child(
            "import time; print('started', flush=True); time.sleep(5)",
            limits=RunLimits(total_timeout_secs=2.0, idle_timeout_secs=0.15),
        )
        self.assertEqual(result.returncode, 124)
        self.assertIn(b"started", log)
        self.assertIn("idle timeout", result.error_message or "")

    def test_input_is_forwarded_only_after_exact_marker(self) -> None:
        script = (
            "import sys; "
            "print('READY', flush=True); "
            "line=sys.stdin.readline().strip(); "
            "print('GOT:'+line, flush=True)"
        )
        result, log, console = self.run_child(
            script,
            limits=RunLimits(total_timeout_secs=2.0, ready_timeout_secs=1.0),
            interaction=Interaction(interactive=True, input_after_marker="READY"),
            input_text=b"payload\n",
        )
        self.assertTrue(result.ok, result.error_message)
        self.assertIn(b"GOT:payload", log)
        self.assertEqual(console, log)
        self.assertIsNotNone(result.input_forwarding)
        forwarding = result.input_forwarding
        assert forwarding is not None
        self.assertEqual(forwarding.sha256, hashlib.sha256(b"payload\n").hexdigest())
        self.assertEqual(forwarding.bytes_forwarded, len(b"payload\n"))
        self.assertEqual(forwarding.line_count, 1)
        self.assertTrue(forwarding.source_eof)
        self.assertFalse(forwarding.broken_pipe)
        self.assertTrue(forwarding.relay_complete)

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
            arch="rv",
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

        self.assertTrue(result.ok, result.error_message)
        self.assertEqual((root / "console.log").read_bytes(), b"TTY=True\n")
        self.assertIsNone(result.input_forwarding)

    def test_early_guest_exit_does_not_claim_complete_input(self) -> None:
        payload = b"command-padding\n" * 100_000
        result, _, _ = self.run_child(
            "print('READY', flush=True)",
            limits=RunLimits(total_timeout_secs=2.0, ready_timeout_secs=1.0),
            interaction=Interaction(interactive=True, input_after_marker="READY"),
            input_text=payload,
        )
        self.assertTrue(result.ok, result.error_message)
        forwarding = result.input_forwarding
        assert forwarding is not None
        self.assertFalse(forwarding.relay_complete)
        self.assertLess(forwarding.bytes_forwarded, len(payload))
        self.assertFalse(forwarding.source_eof)

    def test_nonreading_guest_does_not_bypass_timeout_while_forwarding(self) -> None:
        result, _, _ = self.run_child(
            "import time; print('READY', flush=True); time.sleep(5)",
            limits=RunLimits(total_timeout_secs=0.25, ready_timeout_secs=1.0),
            interaction=Interaction(interactive=True, input_after_marker="READY"),
            input_text=b"command-padding\n" * 100_000,
        )
        self.assertEqual(result.returncode, 124)
        self.assertIn("timed out", result.error_message or "")
        forwarding = result.input_forwarding
        assert forwarding is not None
        self.assertFalse(forwarding.relay_complete)
        self.assertLess(forwarding.bytes_forwarded, forwarding.observed_bytes)

    def test_near_marker_does_not_open_input(self) -> None:
        result, _, _ = self.run_child(
            "print('READY ', flush=True)",
            limits=RunLimits(total_timeout_secs=2.0, ready_timeout_secs=1.0),
            interaction=Interaction(interactive=True, input_after_marker="READY"),
            input_text=b"payload\n",
        )
        self.assertEqual(result.returncode, 4)
        self.assertIn("before input-ready marker", result.error_message or "")

    def test_ready_timeout_terminates_guest(self) -> None:
        result, _, _ = self.run_child(
            "import time; print('booting', flush=True); time.sleep(5)",
            limits=RunLimits(total_timeout_secs=2.0, ready_timeout_secs=0.15),
            interaction=Interaction(interactive=True, input_after_marker="READY"),
        )
        self.assertEqual(result.returncode, 124)
        self.assertIn("input-ready timeout", result.error_message or "")

    def test_stop_after_exact_marker_returns_75(self) -> None:
        result, log, _ = self.run_child(
            "import time; print('STOP', flush=True); time.sleep(5)",
            interaction=Interaction(stop_after_marker="STOP"),
        )
        self.assertTrue(result.intentionally_stopped)
        self.assertIn(b"STOP", log)


if __name__ == "__main__":
    unittest.main()
