"""QEMU process lifetime, serial capture, timeouts, and interactive input."""

from __future__ import annotations

import math
import os
import select
import signal
import subprocess
import sys
import time
from pathlib import Path
from typing import BinaryIO

from .model import (
    Arch,
    INTENTIONAL_STOP_RETURN_CODE,
    Interaction,
    RunLimits,
    RunResult,
)


class ProcessError(ValueError):
    """Raised when process interaction settings are inconsistent."""


def _validate_marker(name: str, marker: str | None) -> None:
    if marker is not None and (not marker or "\n" in marker or "\r" in marker):
        raise ProcessError(f"{name} must be one non-empty console line")


def validate_interaction(interaction: Interaction, limits: RunLimits) -> None:
    _validate_marker("input-after marker", interaction.input_after_marker)
    _validate_marker("stop-after marker", interaction.stop_after_marker)
    if interaction.input_after_marker is not None and not interaction.interactive:
        raise ProcessError("input-after marker requires interactive mode")
    if limits.ready_timeout_secs is not None and interaction.input_after_marker is None:
        raise ProcessError("ready timeout requires an input-after marker")
    for name, value in (
        ("total timeout", limits.total_timeout_secs),
        ("idle timeout", limits.idle_timeout_secs),
        ("ready timeout", limits.ready_timeout_secs),
    ):
        if value is not None and (value <= 0 or not math.isfinite(value)):
            raise ProcessError(f"{name} must be positive")


def terminate_process_group(process: subprocess.Popen[bytes], sig: int) -> None:
    """Signal the complete QEMU session without targeting unrelated processes."""

    try:
        os.killpg(process.pid, sig)
    except ProcessLookupError:
        pass
    except OSError:
        try:
            process.send_signal(sig)
        except ProcessLookupError:
            pass


def _terminate_with_grace(process: subprocess.Popen[bytes], *, grace_secs: float = 5.0) -> None:
    terminate_process_group(process, signal.SIGTERM)
    try:
        process.wait(timeout=grace_secs)
    except subprocess.TimeoutExpired:
        terminate_process_group(process, signal.SIGKILL)
        process.wait()


def _write_stream(stream: BinaryIO | None, data: bytes) -> None:
    if stream is None:
        return
    stream.write(data)
    stream.flush()


def _wait_for_process(
    process: subprocess.Popen[bytes],
    *,
    log_file: BinaryIO,
    limits: RunLimits,
    interaction: Interaction,
    input_stream: BinaryIO,
    console_stream: BinaryIO | None,
) -> tuple[int, str | None]:
    started_at = time.monotonic()
    last_output_at = started_at
    input_ready = interaction.input_after_marker is None
    input_open = interaction.input_after_marker is not None
    stdout_open = True
    pending_output = bytearray()
    assert process.stdout is not None
    if interaction.input_after_marker is not None:
        assert process.stdin is not None

    def consume_lines(data: bytes, *, final: bool = False) -> tuple[int, str] | None:
        nonlocal input_ready
        pending_output.extend(data)
        while True:
            newline = pending_output.find(b"\n")
            if newline < 0:
                break
            raw_line = bytes(pending_output[: newline + 1])
            del pending_output[: newline + 1]
            exact_line = raw_line.rstrip(b"\r\n").decode("utf-8", errors="replace")
            if exact_line == interaction.input_after_marker:
                input_ready = True
            if exact_line == interaction.stop_after_marker:
                message = f"QEMU stopped after marker: {interaction.stop_after_marker}"
                terminate_process_group(process, signal.SIGKILL)
                process.wait()
                return INTENTIONAL_STOP_RETURN_CODE, message
        if final and pending_output:
            pending_output.clear()
        return None

    while True:
        readers: list[object] = []
        if stdout_open:
            readers.append(process.stdout)
        if input_ready and input_open:
            readers.append(input_stream)
        ready, _, _ = select.select(readers, [], [], 0.1)

        if stdout_open and process.stdout in ready:
            data = os.read(process.stdout.fileno(), 65_536)
            if data:
                log_file.write(data)
                log_file.flush()
                if interaction.interactive:
                    _write_stream(console_stream, data)
                last_output_at = time.monotonic()
                stopped = consume_lines(data)
                if stopped is not None:
                    return stopped
            else:
                stdout_open = False

        if input_ready and input_open and input_stream in ready:
            data = os.read(input_stream.fileno(), 65_536)
            if data:
                try:
                    assert process.stdin is not None
                    process.stdin.write(data)
                    process.stdin.flush()
                except (BrokenPipeError, OSError, ValueError):
                    input_open = False
            else:
                input_open = False
                assert process.stdin is not None
                process.stdin.close()

        returncode = process.poll()
        if returncode is not None:
            if stdout_open:
                while True:
                    remainder = os.read(process.stdout.fileno(), 65_536)
                    if not remainder:
                        break
                    log_file.write(remainder)
                    if interaction.interactive:
                        _write_stream(console_stream, remainder)
                    consume_lines(remainder)
                log_file.flush()
            consume_lines(b"", final=True)
            if interaction.input_after_marker is not None and not input_ready:
                return (
                    returncode if returncode != 0 else 4,
                    f"QEMU exited before input-ready marker: {interaction.input_after_marker}",
                )
            return returncode, None

        now = time.monotonic()
        if (
            not input_ready
            and limits.ready_timeout_secs is not None
            and now - started_at >= limits.ready_timeout_secs
        ):
            message = (
                f"QEMU input-ready timeout after {limits.ready_timeout_secs:g}s "
                f"waiting for marker: {interaction.input_after_marker}"
            )
            _terminate_with_grace(process)
            return 124, message
        if limits.total_timeout_secs is not None and now - started_at >= limits.total_timeout_secs:
            message = f"QEMU timed out after {limits.total_timeout_secs:g}s"
            _terminate_with_grace(process)
            return 124, message
        if (
            limits.idle_timeout_secs is not None
            and now - last_output_at >= limits.idle_timeout_secs
        ):
            message = (
                f"QEMU idle timeout after {limits.idle_timeout_secs:g}s "
                "without console output"
            )
            _terminate_with_grace(process)
            return 124, message


def run_process(
    *,
    arch: Arch,
    command: tuple[str, ...],
    workdir: Path,
    log_path: Path,
    limits: RunLimits,
    interaction: Interaction,
    input_stream: BinaryIO | None = None,
    console_stream: BinaryIO | None = None,
) -> RunResult:
    """Run one explicit QEMU command and capture its complete serial stream."""

    validate_interaction(interaction, limits)
    workdir = workdir.expanduser().resolve()
    log_path = log_path.expanduser().resolve()
    workdir.mkdir(parents=True, exist_ok=True)
    log_path.parent.mkdir(parents=True, exist_ok=True)
    input_stream = input_stream or getattr(sys.stdin, "buffer", sys.stdin)
    console_stream = console_stream or getattr(sys.stdout, "buffer", None)
    if interaction.interactive:
        try:
            input_stream.fileno()
        except (AttributeError, OSError, ValueError) as error:
            raise ProcessError("interactive input stream must expose a file descriptor") from error
    started_at = time.monotonic()
    process: subprocess.Popen[bytes] | None = None
    launched = False
    error_message: str | None = None
    try:
        if interaction.input_after_marker is not None:
            process_stdin: int | BinaryIO | None = subprocess.PIPE
        elif interaction.interactive:
            process_stdin = input_stream
        else:
            process_stdin = subprocess.DEVNULL
        with log_path.open("wb") as log_file:
            process = subprocess.Popen(
                command,
                cwd=workdir,
                stdin=process_stdin,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=False,
                bufsize=0,
                start_new_session=True,
            )
            launched = True
            returncode, error_message = _wait_for_process(
                process,
                log_file=log_file,
                limits=limits,
                interaction=interaction,
                input_stream=input_stream,
                console_stream=console_stream,
            )
    except KeyboardInterrupt:
        if process is not None:
            _terminate_with_grace(process)
        returncode = 130
        error_message = "interrupted"
    except OSError as error:
        if process is not None and process.poll() is None:
            _terminate_with_grace(process)
        if launched:
            returncode = 4
            error_message = f"QEMU process I/O failed: {error}"
        else:
            returncode = 3
            error_message = f"QEMU launch failed: {error}"
    finally:
        if process is not None:
            if process.stdin is not None and not process.stdin.closed:
                process.stdin.close()
            if process.stdout is not None and not process.stdout.closed:
                process.stdout.close()

    duration_ms = int((time.monotonic() - started_at) * 1000)
    return RunResult(
        arch=arch,
        command=command,
        returncode=returncode,
        duration_ms=duration_ms,
        log_path=log_path,
        workdir=workdir,
        error_message=error_message,
    )
