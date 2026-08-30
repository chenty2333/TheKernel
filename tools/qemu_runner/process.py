"""QEMU process lifetime, serial capture, timeouts, and interactive input."""

from __future__ import annotations

import math
import os
import select
import signal
import socket
import subprocess
import sys
import threading
import time
import json
from pathlib import Path
from typing import BinaryIO, Mapping

from .model import (
    Arch,
    INTENTIONAL_STOP_RETURN_CODE,
    Interaction,
    QmpColorBlock,
    RunLimits,
    RunResult,
)


class ProcessError(ValueError):
    """Raised when process interaction settings are inconsistent."""


MAX_PENDING_INPUT_BYTES = 64 * 1024


def _validate_qmp_marker(name: str, marker: str | None) -> None:
    _validate_marker(name, marker)


def _validate_color_block(block: QmpColorBlock) -> None:
    if block.x < 0 or block.y < 0 or block.width <= 0 or block.height <= 0:
        raise ProcessError("QMP screenshot color block must have non-negative origin and positive size")
    if len(block.rgb) != 3 or any(channel < 0 or channel > 255 for channel in block.rgb):
        raise ProcessError("QMP screenshot color block RGB channels must be in 0..255")


def _validate_ppm(
    screenshot: Path,
    expected_size: tuple[int, int] | None,
    color_blocks: tuple[QmpColorBlock, ...],
) -> None:
    """Validate QEMU's P6 screendump before reporting graphics success."""

    try:
        image = screenshot.read_bytes()
    except OSError as error:
        raise ProcessError(f"QMP screendump was not created: {screenshot}: {error}") from error
    if not image:
        raise ProcessError("QMP screendump is empty")
    tokens: list[bytes] = []
    offset = 0
    while len(tokens) < 4:
        while offset < len(image) and image[offset] in b" \t\r\n":
            offset += 1
        if offset < len(image) and image[offset] == ord("#"):
            newline = image.find(b"\n", offset)
            offset = len(image) if newline < 0 else newline + 1
            continue
        end = offset
        while end < len(image) and image[end] not in b" \t\r\n":
            end += 1
        if end == offset:
            break
        tokens.append(image[offset:end])
        offset = end
    if len(tokens) != 4 or tokens[0] != b"P6":
        raise ProcessError("QMP screendump is not a P6 PPM image")
    try:
        width, height, maximum = (int(token) for token in tokens[1:])
    except ValueError as error:
        raise ProcessError("QMP screendump has an invalid PPM header") from error
    if width <= 0 or height <= 0 or maximum != 255:
        raise ProcessError("QMP screendump has unsupported PPM dimensions or depth")
    if offset >= len(image) or image[offset] not in b" \t\r\n":
        raise ProcessError("QMP screendump PPM header has no pixel delimiter")
    delimiter = image[offset]
    offset += 1
    if delimiter == ord("\r") and offset < len(image) and image[offset] == ord("\n"):
        offset += 1
    pixels = image[offset:]
    if len(pixels) != width * height * 3:
        raise ProcessError("QMP screendump PPM pixel data is incomplete")
    if expected_size is not None and (width, height) != expected_size:
        raise ProcessError(
            f"QMP screendump dimensions are {width}x{height}, expected {expected_size[0]}x{expected_size[1]}"
        )
    for block in color_blocks:
        if block.x + block.width > width or block.y + block.height > height:
            raise ProcessError("QMP screenshot color block is outside the image")
        expected = bytes(block.rgb)
        for y in range(block.y, block.y + block.height):
            start = (y * width + block.x) * 3
            if pixels[start : start + block.width * 3] != expected * block.width:
                raise ProcessError("QMP screenshot color block did not match")


class _QmpController:
    """A cancellable QMP worker; serial output remains owned by the caller."""

    def __init__(
        self,
        *,
        socket_path: Path,
        screenshot: Path | None,
        input_events: tuple[tuple[Mapping[str, object], ...], ...],
        input_after_marker: str | None,
        screenshot_after_marker: str | None,
        timeout_secs: float,
        screenshot_size: tuple[int, int] | None,
        screenshot_color_blocks: tuple[QmpColorBlock, ...],
    ) -> None:
        self.socket_path = socket_path
        self.screenshot = screenshot
        self.input_events = input_events
        self.input_after_marker = input_after_marker
        self.screenshot_after_marker = screenshot_after_marker
        self.timeout_secs = timeout_secs
        self.screenshot_size = screenshot_size
        self.screenshot_color_blocks = screenshot_color_blocks
        self._input_marker = threading.Event()
        self._screenshot_marker = threading.Event()
        self._cancelled = threading.Event()
        self._complete = threading.Event()
        self._finished = threading.Event()
        self._lock = threading.Lock()
        self._client: socket.socket | None = None
        self.error: ProcessError | None = None
        self._thread = threading.Thread(target=self._run, name="qmp-controls", daemon=True)

    def start(self) -> None:
        self._thread.start()

    @property
    def complete(self) -> bool:
        return self._complete.is_set()

    def observe_marker(self, line: str) -> None:
        if line == self.input_after_marker:
            self._input_marker.set()
        if line == self.screenshot_after_marker:
            self._screenshot_marker.set()

    def close(self) -> None:
        self._cancelled.set()
        with self._lock:
            if self._client is not None:
                try:
                    self._client.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass
                self._client.close()
        self._thread.join(timeout=1.0)

    def settle(self) -> None:
        """Allow an in-flight final QMP response to win a guest-exit race."""

        self._finished.wait(timeout=0.2)

    def _wait(self, event: threading.Event, description: str, deadline: float) -> None:
        while not event.wait(timeout=min(0.05, max(0.0, deadline - time.monotonic()))):
            if self._cancelled.is_set():
                raise ProcessError("QMP controls cancelled")
            if time.monotonic() >= deadline:
                raise ProcessError(f"QMP timeout waiting for {description}")

    def _connect(self, deadline: float) -> socket.socket:
        client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        with self._lock:
            self._client = client
        while True:
            if self._cancelled.is_set():
                raise ProcessError("QMP controls cancelled")
            try:
                client.connect(str(self.socket_path))
                return client
            except (FileNotFoundError, ConnectionRefusedError):
                if time.monotonic() >= deadline:
                    raise ProcessError(f"QMP socket was not created: {self.socket_path}")
                time.sleep(0.02)

    @staticmethod
    def _read_json(client: socket.socket, buffer: bytearray, deadline: float) -> dict[str, object]:
        while True:
            newline = buffer.find(b"\n")
            if newline >= 0:
                raw = bytes(buffer[:newline]).strip()
                del buffer[: newline + 1]
                if not raw:
                    continue
                decoded = json.loads(raw)
                if not isinstance(decoded, dict):
                    raise ProcessError("QMP sent a non-object JSON message")
                return decoded
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise ProcessError("QMP timeout waiting for a response")
            client.settimeout(remaining)
            chunk = client.recv(65_536)
            if not chunk:
                raise ProcessError("QMP closed before replying")
            buffer.extend(chunk)

    def _request(
        self, client: socket.socket, buffer: bytearray, command: str, arguments: dict[str, object] | None, sequence: int, deadline: float
    ) -> None:
        request_id = f"thekernel-qmp-{sequence}"
        request: dict[str, object] = {"execute": command, "id": request_id}
        if arguments is not None:
            request["arguments"] = arguments
        client.sendall(json.dumps(request, separators=(",", ":")).encode() + b"\r\n")
        while True:
            response = self._read_json(client, buffer, deadline)
            if "event" in response or response.get("id") != request_id:
                continue
            if "error" in response:
                raise ProcessError(f"QMP rejected {command}: {response['error']}")
            if "return" in response:
                return
            raise ProcessError(f"QMP sent an invalid response for {command}")

    def _run(self) -> None:
        deadline = time.monotonic() + self.timeout_secs
        try:
            client = self._connect(deadline)
            buffer = bytearray()
            while True:
                greeting = self._read_json(client, buffer, deadline)
                if "event" not in greeting:
                    break
            if not isinstance(greeting.get("QMP"), dict):
                raise ProcessError("QMP sent an invalid greeting")
            self._request(client, buffer, "qmp_capabilities", None, 0, deadline)
            if self.input_events:
                if self.input_after_marker is not None:
                    self._wait(self._input_marker, f"input marker: {self.input_after_marker}", deadline)
                for index, events in enumerate(self.input_events, start=1):
                    self._request(client, buffer, "input-send-event", {"events": list(events)}, index, deadline)
            if self.screenshot is not None:
                if self.screenshot_after_marker is not None:
                    self._wait(self._screenshot_marker, f"screenshot marker: {self.screenshot_after_marker}", deadline)
                self.screenshot.unlink(missing_ok=True)
                self._request(client, buffer, "screendump", {"filename": str(self.screenshot)}, len(self.input_events) + 1, deadline)
                _validate_ppm(self.screenshot, self.screenshot_size, self.screenshot_color_blocks)
            self._complete.set()
        except (OSError, ValueError, json.JSONDecodeError) as error:
            if not self._cancelled.is_set():
                self.error = error if isinstance(error, ProcessError) else ProcessError(f"QMP control failed: {error}")
        finally:
            with self._lock:
                if self._client is not None:
                    self._client.close()
                    self._client = None
            self._finished.set()


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


def validate_qmp_controls(
    *,
    screenshot: Path | None,
    input_events: tuple[tuple[Mapping[str, object], ...], ...],
    input_after_marker: str | None,
    screenshot_after_marker: str | None,
    timeout_secs: float,
    screenshot_size: tuple[int, int] | None,
    screenshot_color_blocks: tuple[QmpColorBlock, ...],
) -> None:
    _validate_qmp_marker("QMP input-after marker", input_after_marker)
    _validate_qmp_marker("QMP screenshot-after marker", screenshot_after_marker)
    if input_after_marker is not None and not input_events:
        raise ProcessError("QMP input-after marker requires input events")
    if screenshot_after_marker is not None and screenshot is None:
        raise ProcessError("QMP screenshot-after marker requires a screenshot")
    if timeout_secs <= 0 or not math.isfinite(timeout_secs):
        raise ProcessError("QMP timeout must be positive")
    if screenshot_size is not None and (
        len(screenshot_size) != 2 or any(value <= 0 for value in screenshot_size)
    ):
        raise ProcessError("QMP screenshot dimensions must be positive")
    if screenshot is None and (screenshot_size is not None or screenshot_color_blocks):
        raise ProcessError("QMP screenshot oracle requires a screenshot")
    for block in screenshot_color_blocks:
        _validate_color_block(block)


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
    forward_input: bool,
    qmp_controller: _QmpController | None = None,
) -> tuple[int, str | None, bool, str | None]:
    started_at = time.monotonic()
    last_output_at = started_at
    input_ready = interaction.input_after_marker is None
    input_open = forward_input
    stdout_open = True
    pending_output = bytearray()
    pending_input = bytearray()
    stop_pending = False
    assert process.stdout is not None

    def consume_lines(
        data: bytes, *, final: bool = False
    ) -> tuple[int, str, bool, str] | None:
        nonlocal input_ready, stop_pending
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
            if qmp_controller is not None:
                qmp_controller.observe_marker(exact_line)
            if exact_line == interaction.stop_after_marker:
                message = f"QEMU stopped after marker: {interaction.stop_after_marker}"
                # A graphics smoke marker may be shared with a QMP action.
                # Keep the guest alive until the worker has acknowledged the
                # QMP request and validated its screenshot oracle; otherwise
                # the runner races its own screendump with SIGKILL.
                if qmp_controller is not None and not qmp_controller.complete:
                    stop_pending = True
                else:
                    terminate_process_group(process, signal.SIGKILL)
                    process.wait()
                    return (
                        INTENTIONAL_STOP_RETURN_CODE,
                        message,
                        True,
                        "stop-after-marker",
                    )
        if final and pending_output:
            pending_output.clear()
        return None

    while True:
        if qmp_controller is not None and qmp_controller.error is not None:
            raise qmp_controller.error
        if stop_pending:
            assert qmp_controller is not None
            if qmp_controller.complete:
                message = f"QEMU stopped after marker: {interaction.stop_after_marker}"
                terminate_process_group(process, signal.SIGKILL)
                process.wait()
                return (
                    INTENTIONAL_STOP_RETURN_CODE,
                    message,
                    True,
                    "stop-after-marker",
                )
        readers: list[object] = []
        writers: list[object] = []
        if stdout_open:
            readers.append(process.stdout)
        if (
            input_ready
            and input_open
            and len(pending_input) < MAX_PENDING_INPUT_BYTES
        ):
            readers.append(input_stream)
        if pending_input:
            assert process.stdin is not None
            if not process.stdin.closed:
                writers.append(process.stdin)
        ready, writable, _ = select.select(readers, writers, [], 0.1)

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
            data = os.read(
                input_stream.fileno(),
                MAX_PENDING_INPUT_BYTES - len(pending_input),
            )
            if data:
                pending_input.extend(data)
            else:
                input_open = False

        if process.stdin is not None and process.stdin in writable:
            try:
                written = os.write(process.stdin.fileno(), pending_input)
                if written <= 0:
                    raise BrokenPipeError("QEMU stdin accepted zero bytes")
            except (BlockingIOError, InterruptedError):
                pass
            except (BrokenPipeError, OSError, ValueError):
                input_open = False
                pending_input.clear()
                process.stdin.close()
            else:
                del pending_input[:written]

        if (
            not input_open
            and not pending_input
            and process.stdin is not None
            and not process.stdin.closed
        ):
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
            if qmp_controller is not None:
                qmp_controller.settle()
                if qmp_controller.error is not None:
                    raise qmp_controller.error
                if not qmp_controller.complete:
                    raise ProcessError("QEMU exited before QMP graphics controls completed")
            if interaction.input_after_marker is not None and not input_ready:
                return (
                    returncode if returncode != 0 else 4,
                    f"QEMU exited before input-ready marker: {interaction.input_after_marker}",
                    False,
                    None,
                )
            return returncode, None, False, None

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
            return 124, message, False, "ready-timeout"
        if limits.total_timeout_secs is not None and now - started_at >= limits.total_timeout_secs:
            message = f"QEMU timed out after {limits.total_timeout_secs:g}s"
            _terminate_with_grace(process)
            return 124, message, False, "total-timeout"
        if (
            limits.idle_timeout_secs is not None
            and now - last_output_at >= limits.idle_timeout_secs
        ):
            message = (
                f"QEMU idle timeout after {limits.idle_timeout_secs:g}s "
                "without console output"
            )
            _terminate_with_grace(process)
            return 124, message, False, "idle-timeout"


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
    pass_fds: tuple[int, ...] = (),
    qmp_socket: Path | None = None,
    screenshot: Path | None = None,
    qmp_input_events: tuple[tuple[Mapping[str, object], ...], ...] = (),
    qmp_input_after_marker: str | None = None,
    qmp_screenshot_after_marker: str | None = None,
    qmp_timeout_secs: float = 5.0,
    qmp_screenshot_size: tuple[int, int] | None = None,
    qmp_screenshot_color_blocks: tuple[QmpColorBlock, ...] = (),
) -> RunResult:
    """Run one explicit QEMU command and capture its complete serial stream."""

    validate_interaction(interaction, limits)
    validate_qmp_controls(
        screenshot=screenshot,
        input_events=qmp_input_events,
        input_after_marker=qmp_input_after_marker,
        screenshot_after_marker=qmp_screenshot_after_marker,
        timeout_secs=qmp_timeout_secs,
        screenshot_size=qmp_screenshot_size,
        screenshot_color_blocks=qmp_screenshot_color_blocks,
    )
    if qmp_socket is None and (
        screenshot is not None
        or qmp_input_events
        or qmp_input_after_marker is not None
        or qmp_screenshot_after_marker is not None
    ):
        raise ProcessError("QMP controls require a QMP socket")
    if any(fd < 0 for fd in pass_fds):
        raise ProcessError("passed file descriptors must be non-negative")
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
    proxy_input = interaction.input_after_marker is not None
    launched = False
    error_message: str | None = None
    marker_success = False
    runner_terminated = False
    runner_termination_reason: str | None = None
    qmp_controller: _QmpController | None = None
    try:
        if proxy_input:
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
                pass_fds=pass_fds,
            )
            launched = True
            if qmp_socket is not None and (screenshot is not None or qmp_input_events):
                qmp_controller = _QmpController(
                    socket_path=qmp_socket,
                    screenshot=screenshot,
                    input_events=qmp_input_events,
                    input_after_marker=qmp_input_after_marker,
                    screenshot_after_marker=qmp_screenshot_after_marker,
                    timeout_secs=qmp_timeout_secs,
                    screenshot_size=qmp_screenshot_size,
                    screenshot_color_blocks=qmp_screenshot_color_blocks,
                )
                qmp_controller.start()
            if proxy_input:
                assert process.stdin is not None
                os.set_blocking(process.stdin.fileno(), False)
            (
                returncode,
                error_message,
                marker_success,
                runner_termination_reason,
            ) = _wait_for_process(
                process,
                log_file=log_file,
                limits=limits,
                interaction=interaction,
                input_stream=input_stream,
                console_stream=console_stream,
                forward_input=proxy_input,
                qmp_controller=qmp_controller,
            )
    except KeyboardInterrupt:
        if process is not None:
            _terminate_with_grace(process)
            runner_terminated = True
            runner_termination_reason = "interrupted"
        returncode = 130
        error_message = "interrupted"
    except (OSError, ProcessError) as error:
        if process is not None and process.poll() is None:
            _terminate_with_grace(process)
            runner_terminated = True
            runner_termination_reason = "process-io-error"
        if launched:
            returncode = 4
            error_message = f"QEMU process I/O failed: {error}"
        else:
            returncode = 3
            error_message = f"QEMU launch failed: {error}"
    finally:
        if qmp_controller is not None:
            qmp_controller.close()
        if qmp_socket is not None:
            try:
                qmp_socket.unlink(missing_ok=True)
            except OSError:
                pass
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
        marker_success=marker_success,
        runner_terminated=(
            runner_terminated or runner_termination_reason is not None
        ),
        runner_termination_reason=runner_termination_reason,
    )
