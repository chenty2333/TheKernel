"""QEMU process lifetime, serial capture, timeouts, and interactive input."""

from __future__ import annotations

import math
import os
import re
import select
import signal
import socket
import subprocess
import sys
import threading
import time
import json
from collections import deque
from dataclasses import dataclass
from contextlib import contextmanager
from pathlib import Path
from typing import BinaryIO, Iterator, Mapping

from .model import (
    INTENTIONAL_STOP_RETURN_CODE,
    KERNEL_CRASH_RETURN_CODE,
    KERNEL_CRASH_TERMINATION_REASON,
    Interaction,
    QmpColorBlock,
    QmpCheckpoint,
    QmpConsoleLine,
    QmpPciHotplug,
    QmpTextCells,
    RunLimits,
    RunResult,
)


class ProcessError(ValueError):
    """Raised when process interaction settings are inconsistent."""


class _ScreenshotColorMismatch(ProcessError):
    """A valid screendump may still show the previous scanout frame."""


MAX_PENDING_INPUT_BYTES = 64 * 1024

# Kernel log records carry ANSI color chunks that can interleave with guest
# userspace console output; markers must still match when a stray escape
# sequence lands on the marker's line.
_ANSI_ESCAPE_RE = re.compile(r"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b[@-Z\\-_]|[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]")

# A kernel that has given up, as one anchored alternation over the *whole* line
# family.  Every alternative below is a string a kernel actually prints, with
# the producer named next to it; nothing here is a guess at what a crash looks
# like, because a marker that never fires is worse than no marker -- it makes the
# timeout verdict look audited.
#
# Matching is anchored at the start of a line (after an optional printk
# `[ 1234.567890]` timestamp and an optional leftover log-level digit) rather
# than searched, so a guest that *quotes* a crash keeps its own verdict.  The
# nested-Linux case is exactly that case, and its transcript is prefixed
# (`THEKERNEL_<category>_INNER: ...` in tests/guest/tools/nested-linux-boot.c:669),
# so it cannot reach this rule.
_KERNEL_CRASH_RE = re.compile(
    r"^(?:\[\s*\d+\.\d+\]\s*)?[0-9]?\s*(?:"
    # Linux.  Reference tree: /home/ava/Desktop/linux-7.2.3, the ABI
    # differential's second target, booted with `console=ttyS0`
    # (config/x86_64/grub-linux-shell.cfg:10), so these reach the console.
    r"Kernel panic - not syncing:"               # kernel/panic.c:640
    r"|Attempted to kill (?:the idle task|init)!"  # kernel/exit.c:1073, :963
    r"|Oops[: ]"                                 # arch/x86/mm/fault.c:718 (__die("Oops"))
    r"|general protection fault"                 # arch/x86/kernel/traps.c:801 (GPFSTR)
    r"|double fault"                             # arch/x86/kernel/traps.c:599
    r"|kernel BUG at "                           # lib/bug.c:260
    r"|BUG: "                                    # arch/x86/mm/fault.c:542, :545;
    #                                             arch/x86/kernel/traps.c:553;
    #                                             kernel/watchdog.c:884 (soft lockup)
    # TheKernel.  `crates/ax/tk-axruntime/src/lang_items.rs:31` writes Rust's
    # `PanicInfo` display -- "panicked at <file>:<line>:<col>:" (the same literal
    # the early screen asserts at kernel/src/pseudofs/dev/early_screen/tests.rs:331)
    # -- and its payload for a CPU fault is one of the strings below, from
    # `crates/ax/tk-axcpu/src/x86_64/trap.rs:26,36,64,69,78,90`.
    r"|panicked at "
    r"|Unhandled (?:kernel )?#[A-Z]{2} @"        # trap.rs:26, :36, :64, :78
    r"|#GP @ "                                   # trap.rs:69
    r"|Unhandled (?:[Ee]xception|[Ii]nterrupt) " # trap.rs:90; x86 trap stubs
    r")"
)

# How far the runner reads into the diagnostic UART's file per poll.  The file
# is QEMU's own `chardev file` sink, so the runner never owns its write side.
_DIAGNOSTIC_READ_BYTES = 1 << 20


def _crash_line(line: str) -> re.Match[str] | None:
    """The crash pattern at the start of one already-normalized console line."""

    return _KERNEL_CRASH_RE.match(line)


def _validate_qmp_marker(name: str, marker: str | None) -> None:
    _validate_marker(name, marker)


def _validate_color_block(block: QmpColorBlock) -> None:
    if block.x < 0 or block.y < 0 or block.width <= 0 or block.height <= 0:
        raise ProcessError("QMP screenshot color block must have non-negative origin and positive size")
    if len(block.rgb) != 3 or any(channel < 0 or channel > 255 for channel in block.rgb):
        raise ProcessError("QMP screenshot color block RGB channels must be in 0..255")


def _validate_text_cells(cells: QmpTextCells) -> None:
    if cells.x < 0 or cells.y < 0:
        raise ProcessError("QMP screenshot text grid must have a non-negative origin")
    if cells.cell_width <= 0 or cells.cell_height <= 0:
        raise ProcessError("QMP screenshot text grid cell size must be positive")
    for name, extent in (("columns", cells.columns), ("rows", cells.rows)):
        if extent is not None and extent <= 0:
            raise ProcessError(f"QMP screenshot text grid {name} must be positive")
    for name, colour in (("ink", cells.ink), ("background", cells.background)):
        if len(colour) != 3 or any(channel < 0 or channel > 255 for channel in colour):
            raise ProcessError(f"QMP screenshot text grid {name} colour channels must be in 0..255")
    if cells.ink == cells.background:
        raise ProcessError("QMP screenshot text grid ink and background must differ")
    if cells.min_ink_pixels <= 0 or cells.min_inked_cells <= 0:
        raise ProcessError("QMP screenshot text grid ink floors must be positive")
    if cells.max_foreign_pixels < 0:
        raise ProcessError("QMP screenshot text grid foreign-pixel budget must be non-negative")
    for line in cells.expected_lines:
        if not line.label or not line.cells:
            raise ProcessError("QMP screenshot expected console line needs a label and cells")
        for cell in line.cells:
            if len(cell) != cells.cell_height:
                raise ProcessError(
                    f"QMP screenshot expected console line {line.label!r} has a cell of "
                    f"{len(cell)} rows, not the grid's {cells.cell_height}"
                )


def _read_ppm(screenshot: Path) -> tuple[int, int, bytes]:
    """Read one QEMU P6 screendump into (width, height, RGB pixels)."""

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
    return width, height, pixels


def _grid_extent(cells: QmpTextCells, width: int, height: int) -> tuple[int, int]:
    """Resolve the character grid, rejecting an image it cannot describe."""

    columns, rows = cells.columns, cells.rows
    if columns is None:
        # Deriving the extent makes the image itself the claim: a surface whose
        # width is not a whole number of character cells cannot be the console
        # grid this oracle describes, and no amount of waiting fixes it.
        if width % cells.cell_width:
            raise ProcessError(
                f"QMP screendump width {width} is not a whole number of "
                f"{cells.cell_width}-pixel character cells"
            )
        columns = width // cells.cell_width
    if rows is None:
        if height % cells.cell_height:
            raise ProcessError(
                f"QMP screendump height {height} is not a whole number of "
                f"{cells.cell_height}-pixel character cells"
            )
        rows = height // cells.cell_height
    if cells.x + columns * cells.cell_width > width or cells.y + rows * cells.cell_height > height:
        raise ProcessError(
            f"QMP screendump text grid {columns}x{rows} cells at ({cells.x},{cells.y}) "
            f"does not fit the {width}x{height} image"
        )
    return columns, rows


@dataclass(frozen=True)
class TextGridInk:
    """What one screendump actually contained, measured on its character grid."""

    columns: int
    rows: int
    ink_pixels: int
    inked_cells: int
    foreign_pixels: int
    outside_ink_pixels: int
    border_ink_pixels: int = 0
    missing_lines: tuple[str, ...] = ()
    # First offending pixel of each class, for a failure message a reader can
    # act on without opening the image.
    first_foreign: tuple[int, int] | None = None
    first_outside: tuple[int, int] | None = None
    first_border: tuple[int, int] | None = None

    def summary(self) -> str:
        return (
            f"{self.columns}x{self.rows} cells, {self.inked_cells} inked cells, "
            f"{self.ink_pixels} ink pixels, {self.foreign_pixels} foreign pixels, "
            f"{self.outside_ink_pixels} ink pixels outside the grid, "
            f"{self.border_ink_pixels} on a cell border"
        )


def _measure_text_grid(
    pixels: bytes,
    width: int,
    height: int,
    cells: QmpTextCells,
) -> TextGridInk:
    """Count ink, inked cells, and non-console pixels on the character grid."""

    columns, rows = _grid_extent(cells, width, height)
    grid_width = columns * cells.cell_width
    grid_height = rows * cells.cell_height
    ink = bytes(cells.ink)
    background = bytes(cells.background)
    # A blank cell row is 24 zero bytes on every real console, so one slice
    # comparison skips eight pixel comparisons.  The scan stays exact: the
    # fast path only ever skips pixels already known to be background.
    blank_row = background * cells.cell_width
    ink_pixels = 0
    inked_cells = 0
    foreign_pixels = 0
    border_ink_pixels = 0
    first_foreign: tuple[int, int] | None = None
    first_border: tuple[int, int] | None = None
    for row in range(rows):
        grid_row = cells.y + row * cells.cell_height
        for column in range(columns):
            grid_column = cells.x + column * cells.cell_width
            cell_ink = 0
            for y in range(grid_row, grid_row + cells.cell_height):
                offset = (y * width + grid_column) * 3
                segment = pixels[offset : offset + cells.cell_width * 3]
                if segment == blank_row:
                    continue
                border_row = (y == grid_row or y == grid_row + cells.cell_height - 1)
                for x in range(cells.cell_width):
                    pixel = segment[x * 3 : x * 3 + 3]
                    if pixel == ink:
                        cell_ink += 1
                        if cells.require_clear_cell_borders and (
                            border_row or x == cells.cell_width - 1
                        ):
                            border_ink_pixels += 1
                            if first_border is None:
                                first_border = (grid_column + x, y)
                    elif pixel != background:
                        foreign_pixels += 1
                        if first_foreign is None:
                            first_foreign = (grid_column + x, y)
            ink_pixels += cell_ink
            if cell_ink:
                inked_cells += 1

    # Ink outside the declared grid means the console wrote where this test
    # says nothing is drawn: a stale frame, a second writer, or a scanout the
    # console does not actually own.  When the grid covers the image there is
    # no outside, which is the common case and costs nothing here.
    outside_ink = 0
    first_outside: tuple[int, int] | None = None

    def scan(x_start: int, x_end: int, y_start: int, y_end: int) -> None:
        nonlocal outside_ink, first_outside
        for y in range(y_start, y_end):
            offset = (y * width + x_start) * 3
            for x in range(x_start, x_end):
                if pixels[offset : offset + 3] == ink:
                    outside_ink += 1
                    if first_outside is None:
                        first_outside = (x, y)
                offset += 3

    scan(0, width, 0, cells.y)
    scan(0, cells.x, cells.y, min(cells.y + grid_height, height))
    scan(cells.x + grid_width, width, cells.y, min(cells.y + grid_height, height))
    scan(0, width, min(cells.y + grid_height, height), height)

    # A line is a run of consecutive cells.  The console draws one byte per
    # cell from a font this test shares with it, so a line that was presented
    # matches cell for cell -- and searching from the last rows up finds the
    # newest text first, which is where a line the guest just wrote lands.
    missing: list[str] = []
    for line in cells.expected_lines:
        width_cells = len(line.cells)
        found = False
        for row in range(rows - 1, -1, -1):
            if found:
                break
            for column in range(0, columns - width_cells + 1):
                if all(
                    _cell_rows(pixels, width, cells, row, column + offset) == line.cells[offset]
                    for offset in range(width_cells)
                ):
                    found = True
                    break
        if not found:
            missing.append(line.label)

    return TextGridInk(
        columns,
        rows,
        ink_pixels,
        inked_cells,
        foreign_pixels,
        outside_ink,
        border_ink_pixels,
        tuple(missing),
        first_foreign,
        first_outside,
        first_border,
    )


def _cell_rows(
    pixels: bytes,
    width: int,
    cells: QmpTextCells,
    row: int,
    column: int,
) -> tuple[int, ...]:
    """One cell's 16 row bytes, in the font's bit order (MSB is leftmost)."""

    rows = []
    for dy in range(cells.cell_height):
        y = cells.y + row * cells.cell_height + dy
        bits = 0
        for dx in range(cells.cell_width):
            offset = (y * width + cells.x + column * cells.cell_width + dx) * 3
            if pixels[offset : offset + 3] == bytes(cells.ink):
                bits |= 0x80 >> dx
        rows.append(bits)
    return tuple(rows)


def _validate_text_grid(
    pixels: bytes,
    width: int,
    height: int,
    cells: QmpTextCells,
) -> None:
    """Assert that a screendump contains console glyph ink on its cell grid.

    Every threshold failure is raised as a retryable frame mismatch: a
    screendump may legitimately catch a repaint that has not reached the
    expected cells yet, and the caller polls until the deadline before
    reporting the last one.  A structural failure, where the image cannot be
    the grid at all, is fatal instead.
    """

    measured = _measure_text_grid(pixels, width, height, cells)
    if measured.missing_lines:
        raise _ScreenshotColorMismatch(
            f"QMP screendump does not show the expected console text: "
            f"{', '.join(repr(label) for label in measured.missing_lines)} "
            f"({measured.summary()})"
        )
    if measured.outside_ink_pixels:
        assert measured.first_outside is not None
        raise _ScreenshotColorMismatch(
            f"QMP screendump has {measured.outside_ink_pixels} ink pixels outside the "
            f"{measured.columns}x{measured.rows}-cell text grid "
            f"(first at {measured.first_outside[0]},{measured.first_outside[1]})"
        )
    if measured.foreign_pixels > cells.max_foreign_pixels:
        assert measured.first_foreign is not None
        raise _ScreenshotColorMismatch(
            f"QMP screendump has {measured.foreign_pixels} pixels inside the text grid which are "
            f"neither ink {cells.ink} nor background {cells.background} "
            f"(first at {measured.first_foreign[0]},{measured.first_foreign[1]}); a console "
            f"writing at the wrong stride for the surface's pixel depth looks exactly like this"
        )
    if measured.border_ink_pixels:
        assert measured.first_border is not None
        raise _ScreenshotColorMismatch(
            f"QMP screendump has {measured.border_ink_pixels} ink pixels on a character cell's "
            f"border (first at {measured.first_border[0]},{measured.first_border[1]}); the "
            f"console's glyphs never touch a cell edge, so text painted there was not drawn by "
            f"its renderer -- a shifted or wrongly strided render looks like this even when it "
            f"keeps the exact console colours"
        )
    if measured.ink_pixels < cells.min_ink_pixels:
        raise _ScreenshotColorMismatch(
            f"QMP screendump holds {measured.ink_pixels} ink pixels, below the "
            f"{cells.min_ink_pixels} that rendered text requires "
            f"(grid {measured.columns}x{measured.rows} cells)"
        )
    if measured.inked_cells < cells.min_inked_cells:
        raise _ScreenshotColorMismatch(
            f"QMP screendump holds ink in {measured.inked_cells} character cells, below the "
            f"{cells.min_inked_cells} that rendered text requires "
            f"({measured.ink_pixels} ink pixels)"
        )


def measure_text_cells(screenshot: Path, cells: QmpTextCells) -> TextGridInk:
    """Measure one screendump against a text-cell expectation, without a gate.

    This is the oracle's read-only half.  The acceptance suite calls it after
    a passing run so the report quotes what the screen actually held instead
    of restating the thresholds.
    """

    _validate_text_cells(cells)
    width, height, pixels = _read_ppm(screenshot)
    return _measure_text_grid(pixels, width, height, cells)


def _validate_ppm(
    screenshot: Path,
    expected_size: tuple[int, int] | None,
    color_blocks: tuple[QmpColorBlock, ...],
    text_cells: QmpTextCells | None = None,
) -> None:
    """Validate QEMU's P6 screendump before reporting graphics success."""

    width, height, pixels = _read_ppm(screenshot)
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
                raise _ScreenshotColorMismatch("QMP screenshot color block did not match")
    if text_cells is not None:
        _validate_text_grid(pixels, width, height, text_cells)


def _pin_vcpu_threads(qemu_pid: int | None, response: object,
                      host_cpus: tuple[int, ...]) -> tuple[tuple[int, int, int], ...]:
    """Change only verified threads of this paused QEMU process."""
    if qemu_pid is None or not isinstance(response, list) or len(response) != len(host_cpus):
        raise ProcessError("QMP returned an incomplete vCPU thread map")
    allowed = os.sched_getaffinity(qemu_pid)
    if len(set(host_cpus)) != len(host_cpus) or any(
        isinstance(cpu, bool) or not isinstance(cpu, int) or cpu not in allowed for cpu in host_cpus
    ):
        raise ProcessError("vCPU host CPUs must be distinct members of QEMU's inherited mask")
    threads = {}
    for cpu in response:
        if not isinstance(cpu, dict):
            raise ProcessError("QMP returned an invalid vCPU entry")
        index, tid = cpu.get("cpu-index"), cpu.get("thread-id")
        if (isinstance(index, bool) or not isinstance(index, int) or index not in range(len(host_cpus))
            or isinstance(tid, bool) or not isinstance(tid, int) or tid <= 0
            or index in threads or tid in threads.values() or tid == qemu_pid
            or not Path(f"/proc/{qemu_pid}/task/{tid}").is_dir()):
            raise ProcessError("QMP vCPU thread is duplicated, missing, or outside this QEMU")
        threads[index] = tid
    previous = {}
    try:
        for index, host_cpu in enumerate(host_cpus):
            tid = threads[index]
            previous[tid] = os.sched_getaffinity(tid)
            os.sched_setaffinity(tid, {host_cpu})
            if os.sched_getaffinity(tid) != {host_cpu}:
                raise ProcessError("vCPU affinity readback differs from requested CPU")
        if os.sched_getaffinity(qemu_pid) != allowed:
            raise ProcessError("QEMU main thread mask changed while pinning vCPUs")
    except (OSError, ProcessError):
        for tid, mask in previous.items():
            try:
                os.sched_setaffinity(tid, mask)
            except OSError:
                pass  # The runner terminates QEMU; never resume a failed map.
        raise
    return tuple((index, threads[index], host_cpu) for index, host_cpu in enumerate(host_cpus))


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
        checkpoints: tuple[QmpCheckpoint, ...],
        screenshot_text_cells: QmpTextCells | None = None,
        vcpu_host_cpus: tuple[int, ...] = (),
        qemu_pid: int | None = None,
    ) -> None:
        self.vcpu_host_cpus = vcpu_host_cpus
        self.qemu_pid = qemu_pid
        self.vcpu_affinity: tuple[tuple[int, int, int], ...] = ()
        self.socket_path = socket_path
        self.screenshot = screenshot
        self.input_events = input_events
        self.input_after_marker = input_after_marker
        self.screenshot_after_marker = screenshot_after_marker
        self.timeout_secs = timeout_secs
        self.screenshot_size = screenshot_size
        self.screenshot_color_blocks = screenshot_color_blocks
        self.screenshot_text_cells = screenshot_text_cells
        self.checkpoints = checkpoints or (
            QmpCheckpoint(
                input_after_marker=input_after_marker or "",
                input_events=input_events,
                screenshot=screenshot,
                screenshot_after_marker=screenshot_after_marker,
                screenshot_size=screenshot_size,
                screenshot_color_blocks=screenshot_color_blocks,
                screenshot_text_cells=screenshot_text_cells,
            ),
        )
        self._markers: set[str] = set()
        self._marker_condition = threading.Condition()
        self._cancelled = threading.Event()
        self._complete = threading.Event()
        self._finished = threading.Event()
        self._lock = threading.Lock()
        self._client: socket.socket | None = None
        self.latency_metrics: list[tuple[int, int]] = []
        self.error: ProcessError | None = None
        self._thread = threading.Thread(target=self._run, name="qmp-controls", daemon=True)

    def start(self) -> None:
        self._thread.start()

    @property
    def complete(self) -> bool:
        return self._complete.is_set()

    def observe_marker(self, line: str) -> None:
        with self._marker_condition:
            self._markers.add(line)
            self._marker_condition.notify_all()

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

    def _unsettled(self, mismatch: _ScreenshotColorMismatch) -> ProcessError:
        """A frame that never became what the oracle required, within budget."""

        if self._cancelled.is_set():
            return ProcessError("QMP controls cancelled")
        return ProcessError(
            f"QMP screenshot oracle did not settle within {self.timeout_secs:g}s: {mismatch}"
        )

    def _wait_marker(self, marker: str, description: str, deadline: float) -> None:
        with self._marker_condition:
            while marker not in self._markers:
                if self._cancelled.is_set():
                    raise ProcessError("QMP controls cancelled")
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise ProcessError(f"QMP timeout waiting for {description}")
                self._marker_condition.wait(timeout=min(0.05, remaining))

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
            try:
                chunk = client.recv(65_536)
            except socket.timeout:
                # A socket timeout can race the monotonic deadline by a small
                # amount.  Recheck it so callers retain their command-specific
                # timeout diagnostics instead of receiving an OSError wrapper.
                continue
            if not chunk:
                raise ProcessError("QMP closed before replying")
            buffer.extend(chunk)

    @staticmethod
    def _device_deleted_id(event: Mapping[str, object]) -> str | None:
        """Extract the device id from a well-formed DEVICE_DELETED event."""

        if event.get("event") != "DEVICE_DELETED":
            return None
        data = event.get("data")
        if not isinstance(data, Mapping):
            return None
        device_id = data.get("device")
        return device_id if isinstance(device_id, str) else None

    def _request(
        self,
        client: socket.socket,
        buffer: bytearray,
        command: str,
        arguments: dict[str, object] | None,
        sequence: int,
        deadline: float,
        device_deleted_events: deque[dict[str, object]],
    ) -> object:
        request_id = f"thekernel-qmp-{sequence}"
        request: dict[str, object] = {"execute": command, "id": request_id}
        if arguments is not None:
            request["arguments"] = arguments
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise ProcessError("QMP timeout sending a request")
        client.settimeout(remaining)
        client.sendall(json.dumps(request, separators=(",", ":")).encode() + b"\r\n")
        while True:
            response = self._read_json(client, buffer, deadline)
            if "event" in response:
                # QMP may emit DEVICE_DELETED before the device_del response.
                # Preserve it so the completion wait below remains correct for
                # either legal message ordering.
                if self._device_deleted_id(response) is not None:
                    device_deleted_events.append(response)
                continue
            if response.get("id") != request_id:
                continue
            if "error" in response:
                raise ProcessError(f"QMP rejected {command}: {response['error']}")
            if "return" in response:
                return response["return"]
            raise ProcessError(f"QMP sent an invalid response for {command}")

    def _wait_device_deleted(
        self,
        client: socket.socket,
        buffer: bytearray,
        device_id: str,
        deadline: float,
        device_deleted_events: deque[dict[str, object]],
    ) -> None:
        """Wait for QEMU to complete a successfully accepted device_del."""

        while True:
            while device_deleted_events:
                if self._device_deleted_id(device_deleted_events.popleft()) == device_id:
                    return
            try:
                response = self._read_json(client, buffer, deadline)
            except ProcessError as error:
                if str(error) == "QMP timeout waiting for a response":
                    raise ProcessError(f"QMP timeout waiting for DEVICE_DELETED for device {device_id}") from error
                raise
            if "event" in response:
                deleted_id = self._device_deleted_id(response)
                if deleted_id == device_id:
                    return
                if deleted_id is not None:
                    device_deleted_events.append(response)
                continue
            # QMP replies unrelated to this worker's serialized request may be
            # interleaved with events.  They cannot confirm this deletion.

    def _run(self) -> None:
        deadline = time.monotonic() + self.timeout_secs
        negotiated = False
        last_mismatch: _ScreenshotColorMismatch | None = None
        try:
            client = self._connect(deadline)
            buffer = bytearray()
            device_deleted_events: deque[dict[str, object]] = deque()
            while True:
                greeting = self._read_json(client, buffer, deadline)
                if "event" not in greeting:
                    break
            if not isinstance(greeting.get("QMP"), dict):
                raise ProcessError("QMP sent an invalid greeting")
            self._request(client, buffer, "qmp_capabilities", None, 0, deadline, device_deleted_events)
            sequence = 1
            negotiated = True
            if self.vcpu_host_cpus:
                status = self._request(client, buffer, "query-status", None, sequence, deadline, device_deleted_events)
                sequence += 1
                if not isinstance(status, dict) or status.get("running") is not False:
                    raise ProcessError("vCPU affinity requires QEMU to start paused")
                cpus = self._request(client, buffer, "query-cpus-fast", None, sequence, deadline, device_deleted_events)
                sequence += 1
                self.vcpu_affinity = _pin_vcpu_threads(self.qemu_pid, cpus, self.vcpu_host_cpus)
                self._request(client, buffer, "cont", None, sequence, deadline, device_deleted_events)
                sequence += 1
            for checkpoint in self.checkpoints:
                if checkpoint.input_events or checkpoint.pci_hotplug:
                    self._wait_marker(
                        checkpoint.input_after_marker,
                        f"checkpoint marker: {checkpoint.input_after_marker}",
                        deadline,
                    )
                for action in checkpoint.pci_hotplug:
                    if action.action == "add":
                        self._request(
                            client,
                            buffer,
                            "device_add",
                            {"driver": action.driver, "id": action.device_id, "bus": action.bus},
                            sequence,
                            deadline,
                            device_deleted_events,
                        )
                    else:
                        self._request(
                            client,
                            buffer,
                            "device_del",
                            {"id": action.device_id},
                            sequence,
                            deadline,
                            device_deleted_events,
                        )
                        self._wait_device_deleted(client, buffer, action.device_id, deadline, device_deleted_events)
                    sequence += 1
                if checkpoint.input_events:
                    latency_started_ns = time.monotonic_ns()
                    for events in checkpoint.input_events:
                        self._request(
                            client,
                            buffer,
                            "input-send-event",
                            {"events": list(events)},
                            sequence,
                            deadline,
                            device_deleted_events,
                        )
                        sequence += 1
                    if checkpoint.latency_after_marker is not None:
                        self._wait_marker(
                            checkpoint.latency_after_marker,
                            f"input-visible marker: {checkpoint.latency_after_marker}",
                            deadline,
                        )
                        assert checkpoint.latency_index is not None
                        self.latency_metrics.append(
                            (checkpoint.latency_index, time.monotonic_ns() - latency_started_ns)
                        )
                if checkpoint.screenshot is not None:
                    if checkpoint.screenshot_after_marker is not None:
                        self._wait_marker(
                            checkpoint.screenshot_after_marker,
                            f"screenshot marker: {checkpoint.screenshot_after_marker}",
                            deadline,
                        )
                    # Wayland frame callbacks can precede scanout. Keep the
                    # pixel oracle strict while waiting within this deadline.
                    while True:
                        checkpoint.screenshot.unlink(missing_ok=True)
                        self._request(
                            client,
                            buffer,
                            "screendump",
                            {"filename": str(checkpoint.screenshot)},
                            sequence,
                            deadline,
                            device_deleted_events,
                        )
                        sequence += 1
                        try:
                            _validate_ppm(
                                checkpoint.screenshot,
                                checkpoint.screenshot_size,
                                checkpoint.screenshot_color_blocks,
                                checkpoint.screenshot_text_cells,
                            )
                            last_mismatch = None
                            break
                        except _ScreenshotColorMismatch as mismatch:
                            last_mismatch = mismatch
                            # The screen may simply not show it yet: the guest
                            # records console cells and repaints behind the
                            # writes.  Retry until the deadline, then report
                            # what was still missing and how long was allowed,
                            # never a bare "did not match".
                            remaining = deadline - time.monotonic()
                            if remaining <= 0 or self._cancelled.wait(min(0.02, remaining)):
                                raise self._unsettled(mismatch) from mismatch
                            if time.monotonic() >= deadline:
                                raise self._unsettled(mismatch) from mismatch
            self._complete.set()
        except (OSError, ValueError, json.JSONDecodeError) as error:
            if not self._cancelled.is_set():
                failure = error if isinstance(error, ProcessError) else ProcessError(f"QMP control failed: {error}")
                # The controller owns the sole monitor connection. Capture
                # bounded, read-only CPU state before shutdown when the guest
                # never reaches a checkpoint; a second monitor cannot attach
                # to diagnose that stall while this connection is held.
                if negotiated and str(error).startswith("QMP timeout waiting for") and "marker:" in str(error):
                    try:
                        diagnostic_deadline = time.monotonic() + 1.0
                        registers = self._request(
                            client, buffer, "human-monitor-command",
                            {"command-line": "info registers -a"}, sequence,
                            diagnostic_deadline, device_deleted_events,
                        )
                        if isinstance(registers, str) and registers.strip():
                            failure = ProcessError(f"{failure}\nGuest CPU state at timeout:\n{registers[:32768]}")
                            # The x86 monitor defaults to CPU 0. A bounded
                            # kernel stack excerpt supplies return addresses
                            # when RIP alone lands in a generic lock helper.
                            first_cpu = registers.split("CPU#1", 1)[0]
                            stack_pointer = re.search(r"\bRSP=([0-9a-fA-F]{16})\b", first_cpu)
                            if stack_pointer is not None:
                                address = int(stack_pointer.group(1), 16)
                                if 0xffff800000000000 <= address <= 0xfffffffffffffc00:
                                    stack = self._request(
                                        client, buffer, "human-monitor-command",
                                        {"command-line": f"x /128gx 0x{address:x}"}, sequence + 1,
                                        diagnostic_deadline, device_deleted_events,
                                    )
                                    if isinstance(stack, str) and stack.strip():
                                        failure = ProcessError(f"{failure}\nCPU 0 kernel stack:\n{stack[:8192]}")
                        devices = self._request(
                            client, buffer, "x-query-virtio", None, sequence + 2,
                            diagnostic_deadline, device_deleted_events,
                        )
                        if isinstance(devices, list):
                            gpu = next((device for device in devices[:16]
                                        if isinstance(device, dict)
                                        and "gpu" in str(device.get("name", "")).lower()
                                        and isinstance(device.get("path"), str)), None)
                            if gpu is not None:
                                status = self._request(
                                    client, buffer, "x-query-virtio-status",
                                    {"path": gpu["path"]}, sequence + 3,
                                    diagnostic_deadline, device_deleted_events,
                                )
                                queue = self._request(
                                    client, buffer, "x-query-virtio-queue-status",
                                    {"path": gpu["path"], "queue": 0}, sequence + 4,
                                    diagnostic_deadline, device_deleted_events,
                                )
                                state = json.dumps({"device": status, "control_queue": queue})
                                failure = ProcessError(f"{failure}\nVirtIO GPU state at timeout:\n{state[:8192]}")
                    except (OSError, ValueError):
                        pass  # Diagnostics must not replace the original failure.
                # A disconnect can end the retry loop before its deadline.
                # Preserve the actual transport failure and the last reason
                # this checkpoint was still unaccepted, not an older frame
                # from a checkpoint which has already passed.
                if last_mismatch is not None and str(last_mismatch) not in str(failure):
                    failure = ProcessError(f"{failure}\nLast screenshot mismatch: {last_mismatch}")
                self.error = failure
        finally:
            with self._lock:
                if self._client is not None:
                    self._client.close()
                    self._client = None
            self._finished.set()


def _validate_marker(name: str, marker: str | None) -> None:
    if marker is not None and (not marker or "\n" in marker or "\r" in marker):
        raise ProcessError(f"{name} must be one non-empty console line")


_INPUT_HOTPLUG_DRIVERS = frozenset({"virtio-keyboard-pci", "virtio-mouse-pci", "virtio-tablet-pci"})
_INPUT_HOTPLUG_BUSES = frozenset({"rp-input-kbd", "rp-input-mouse", "rp-input-tablet"})


def _validate_pci_hotplug(action: QmpPciHotplug) -> None:
    if action.action not in {"add", "del"}:
        raise ProcessError("QMP PCI hotplug action must be add or del")
    if not action.device_id or any(char in action.device_id for char in ",\n\r"):
        raise ProcessError("QMP PCI hotplug device id must be QEMU-safe")
    if action.action == "add":
        if action.driver not in _INPUT_HOTPLUG_DRIVERS:
            raise ProcessError("QMP PCI add supports only VirtIO input drivers")
        if action.bus not in _INPUT_HOTPLUG_BUSES:
            raise ProcessError("QMP PCI add requires a reserved input root port")
    elif action.driver is not None or action.bus is not None:
        raise ProcessError("QMP PCI delete accepts only a device id")


def validate_interaction(interaction: Interaction, limits: RunLimits) -> None:
    _validate_marker("input-after marker", interaction.input_after_marker)
    _validate_marker("stop-after marker", interaction.stop_after_marker)
    if interaction.input_after_marker is not None and not interaction.interactive:
        raise ProcessError("input-after marker requires interactive mode")
    value = limits.total_timeout_secs
    if value is not None and (value <= 0 or not math.isfinite(value)):
        raise ProcessError("total timeout must be positive")


def validate_qmp_controls(
    *,
    screenshot: Path | None,
    input_events: tuple[tuple[Mapping[str, object], ...], ...],
    input_after_marker: str | None,
    screenshot_after_marker: str | None,
    timeout_secs: float,
    screenshot_size: tuple[int, int] | None,
    screenshot_color_blocks: tuple[QmpColorBlock, ...],
    checkpoints: tuple[QmpCheckpoint, ...],
    screenshot_text_cells: QmpTextCells | None = None,
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
    if screenshot is None and (
        screenshot_size is not None or screenshot_color_blocks or screenshot_text_cells is not None
    ):
        raise ProcessError("QMP screenshot oracle requires a screenshot")
    for block in screenshot_color_blocks:
        _validate_color_block(block)
    if screenshot_text_cells is not None:
        _validate_text_cells(screenshot_text_cells)
    for checkpoint in checkpoints:
        _validate_qmp_marker("QMP checkpoint input-after marker", checkpoint.input_after_marker)
        _validate_qmp_marker("QMP checkpoint screenshot-after marker", checkpoint.screenshot_after_marker)
        _validate_qmp_marker("QMP checkpoint latency-after marker", checkpoint.latency_after_marker)
        if (checkpoint.latency_after_marker is None) != (checkpoint.latency_index is None):
            raise ProcessError("QMP checkpoint latency marker and index must be specified together")
        if checkpoint.latency_index is not None:
            if checkpoint.latency_index < 0:
                raise ProcessError("QMP checkpoint latency index must be non-negative")
            if not checkpoint.input_events:
                raise ProcessError("QMP checkpoint latency measurement requires input events")
        if checkpoint.screenshot_after_marker is not None and checkpoint.screenshot is None:
            raise ProcessError("QMP checkpoint screenshot-after marker requires a screenshot")
        if checkpoint.screenshot is None and (
            checkpoint.screenshot_size is not None
            or checkpoint.screenshot_color_blocks
            or checkpoint.screenshot_text_cells is not None
        ):
            raise ProcessError("QMP checkpoint screenshot oracle requires a screenshot")
        for block in checkpoint.screenshot_color_blocks:
            _validate_color_block(block)
        if checkpoint.screenshot_text_cells is not None:
            _validate_text_cells(checkpoint.screenshot_text_cells)
        for action in checkpoint.pci_hotplug:
            _validate_pci_hotplug(action)


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


@contextmanager
def _defer_termination_signals() -> Iterator[list[int]]:
    """Defer termination until the child is reaped, then unwind caller cleanup.

    Recording instead of raising also covers signals delivered inside Popen,
    before the caller has received the child handle. Library callers running
    outside the main thread retain their application's signal policy.
    """
    pending: list[int] = []
    previous = {}

    def defer(signum: int, frame: object) -> None:
        if not pending:
            pending.append(signum)

    try:
        if threading.current_thread() is threading.main_thread():
            for signum in (signal.SIGTERM, signal.SIGHUP):
                handler = signal.getsignal(signum)
                if handler != signal.SIG_IGN:
                    previous[signum] = handler
                    signal.signal(signum, defer)
        yield pending
    finally:
        for signum, handler in previous.items():
            signal.signal(signum, handler)
        if pending:
            signum = pending[0]
            if previous[signum] == signal.SIG_DFL:
                # Terminating the interpreter with the OS signal would skip
                # outer finally blocks that own disks and other run artifacts.
                raise SystemExit(128 + signum)
            signal.raise_signal(signum)


def _write_stream(stream: BinaryIO | None, data: bytes) -> None:
    if stream is None:
        return
    stream.write(data)
    stream.flush()


class _ShellConsoleFilter:
    """Hide the prompt handshake and its extra blank separator."""

    marker = b"THEKERNEL_SHELL_READY"

    def __init__(self) -> None:
        self.pending = bytearray()
        self.blank_line = b""
        self.passthrough = False

    def feed(self, data: bytes, *, final: bool = False) -> bytes:
        output = bytearray()
        for byte in data:
            if self.passthrough:
                output.append(byte)
                if byte == 10:
                    self.passthrough = False
                continue
            self.pending.append(byte)
            if byte == 10:
                line = self.pending.strip(b"\r\n")
                if line == self.marker:
                    self.blank_line = b""
                elif not line:
                    # PS1 inserts a newline before the marker. Hold only the
                    # last empty line, preserving any command-generated ones.
                    output.extend(self.blank_line)
                    self.blank_line = bytes(self.pending)
                else:
                    output.extend(self.blank_line)
                    self.blank_line = b""
                    output.extend(self.pending)
                self.pending.clear()
                continue
            candidate = bytes(self.pending).lstrip(b"\r")
            if not (self.marker.startswith(candidate)
                    or candidate.rstrip(b"\r") == self.marker):
                output.extend(self.blank_line)
                self.blank_line = b""
                output.extend(self.pending)
                self.pending.clear()
                self.passthrough = True
        if final:
            output.extend(self.blank_line)
            self.blank_line = b""
            output.extend(self.pending)
            self.pending.clear()
        return bytes(output)


def _wait_for_process(
    process: subprocess.Popen[bytes],
    *,
    log_file: BinaryIO,
    limits: RunLimits,
    interaction: Interaction,
    input_stream: BinaryIO,
    console_stream: BinaryIO | None,
    forward_input: bool,
    termination_signals: list[int],
    qmp_controller: _QmpController | None = None,
    diagnostic_log_path: Path | None = None,
    initial_diagnostic_offset: int | None = None,
) -> tuple[int, str | None, bool, str | None]:
    started_at = time.monotonic()
    input_ready = interaction.input_after_marker is None
    line_ready = interaction.input_line_after_marker is None
    input_open = forward_input
    stdout_open = True
    console_filter = _ShellConsoleFilter()
    pending_output = bytearray()
    pending_input = bytearray()
    stop_pending = False
    active_case: tuple[str, float] | None = None
    last_case: str | None = None
    crash_watch = interaction.detect_kernel_crash
    # The kernel's own crash text never reaches the console: `lang_items.rs`
    # writes it to the diagnostic UART, which QEMU appends straight to
    # `kernel.log`.  Reading what that file has gained since this run started is
    # therefore the only way to see a TheKernel panic before the watchdog does,
    # and starting at the current end of file keeps a reused path's stale bytes
    # from condemning a healthy guest.
    diagnostic_pending = bytearray()
    diagnostic_offset = 0
    if crash_watch and diagnostic_log_path is not None:
        if initial_diagnostic_offset is not None:
            diagnostic_offset = initial_diagnostic_offset
        else:
            try:
                diagnostic_offset = diagnostic_log_path.stat().st_size
            except OSError:
                diagnostic_offset = 0
    assert process.stdout is not None

    def crash_verdict(line: str, stream: str) -> tuple[int, str, bool, str]:
        case = active_case[0] if active_case is not None else (last_case or "none")
        # A guest that took the crash with it is already reaped; signalling a
        # recycled process group here would report a verdict about the wrong
        # machine.
        if process.poll() is None:
            _terminate_with_grace(process)
        return (
            KERNEL_CRASH_RETURN_CODE,
            f"kernel crash while running test {case} ({stream} stream): {line}",
            False,
            KERNEL_CRASH_TERMINATION_REASON,
        )

    def scan_diagnostic_stream() -> tuple[int, str, bool, str] | None:
        """Feed this run's appended diagnostic-UART bytes to the crash rule.

        Only complete lines are classified: the panic header and its payload are
        separate writes, and a half line must not be matched twice.
        """

        nonlocal diagnostic_offset
        assert diagnostic_log_path is not None
        try:
            with diagnostic_log_path.open("rb") as handle:
                handle.seek(diagnostic_offset)
                chunk = handle.read(_DIAGNOSTIC_READ_BYTES)
        except OSError:
            return None  # Not created yet, or removed: nothing to classify.
        if not chunk:
            return None
        diagnostic_offset += len(chunk)
        diagnostic_pending.extend(chunk)
        while True:
            newline = diagnostic_pending.find(b"\n")
            if newline < 0:
                break
            raw_line = bytes(diagnostic_pending[:newline])
            del diagnostic_pending[: newline + 1]
            line = _ANSI_ESCAPE_RE.sub(
                "", raw_line.rstrip(b"\r").decode("utf-8", errors="replace")
            )
            if _crash_line(line) is not None:
                return crash_verdict(line, "diagnostic")
        return None

    def consume_lines(
        data: bytes, *, final: bool = False
    ) -> tuple[int, str, bool, str] | None:
        nonlocal input_ready, line_ready, stop_pending, active_case, last_case
        pending_output.extend(data)
        while True:
            newline = pending_output.find(b"\n")
            if newline < 0:
                break
            raw_line = bytes(pending_output[: newline + 1])
            del pending_output[: newline + 1]
            # Firmware can end a line with LF-CR, leaving its carriage return
            # before the next marker. Normalize boundary CRs only; spaces and
            # other text must still prevent an exact marker match.
            exact_line = raw_line.rstrip(b"\r\n").lstrip(b"\r").decode("utf-8", errors="replace")
            marker_line = _ANSI_ESCAPE_RE.sub("", exact_line)
            if any(marker_line == prefix or marker_line.startswith(prefix + " ")
                   for prefix in interaction.failure_prefixes):
                raise ProcessError(f"guest reported failure: {marker_line}")
            # Before the case bookkeeping: a crash line inside a case belongs to
            # that case, and reporting it as the `case-timeout` it would
            # otherwise become loses the only diagnosis the run produced.
            if crash_watch and _crash_line(marker_line) is not None:
                return crash_verdict(marker_line, "console")
            begin = re.fullmatch(r"# THEKERNEL_TEST_BEGIN (\d+) (\S+) timeout_seconds=(\d+)", marker_line)
            end = re.fullmatch(r"# THEKERNEL_TEST_END (\d+) (\S+) result=(-?\d+)", marker_line)
            if begin:
                if active_case is not None:
                    raise ProcessError(f"new test began before {active_case[0]} completed")
                last_case = f"{begin[1]} {begin[2]}"
                # Each workload declares its own bound (pressure tests include
                # deliberate pacing); the whole-run deadline remains in force.
                active_case = (last_case, time.monotonic() + int(begin[3]))
            elif end:
                case = f"{end[1]} {end[2]}"
                if active_case is None or active_case[0] != case:
                    raise ProcessError(f"test completion without matching begin: {case}")
                active_case = None
            if marker_line == interaction.input_line_after_marker:
                line_ready = True
            if marker_line == interaction.input_after_marker:
                input_ready = True
            if qmp_controller is not None:
                qmp_controller.observe_marker(marker_line)
            if marker_line == interaction.stop_after_marker:
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
        if termination_signals:
            signum = termination_signals[0]
            return 128 + signum, f"interrupted by {signal.Signals(signum).name}", False, "interrupted"
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
        if pending_input and line_ready:
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
                    _write_stream(console_stream, console_filter.feed(data))
                stopped = consume_lines(data)
                if stopped is not None:
                    return stopped
            else:
                stdout_open = False
                if interaction.interactive:
                    _write_stream(console_stream, console_filter.feed(b"", final=True))

        if crash_watch and diagnostic_log_path is not None:
            crashed = scan_diagnostic_stream()
            if crashed is not None:
                return crashed

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
                # Never queue the next command before its shell prompt.
                newline = pending_input.find(b"\n")
                limit = newline + 1 if interaction.input_line_after_marker is not None and newline >= 0 else len(pending_input)
                written = os.write(process.stdin.fileno(), pending_input[:limit])
                if written <= 0:
                    raise BrokenPipeError("QEMU stdin accepted zero bytes")
            except (BlockingIOError, InterruptedError):
                pass
            except (BrokenPipeError, OSError, ValueError):
                input_open = False
                pending_input.clear()
                process.stdin.close()
            else:
                if interaction.input_line_after_marker is not None and b"\n" in pending_input[:written]:
                    line_ready = False
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
            drained_crash: tuple[int, str, bool, str] | None = None
            if stdout_open:
                while True:
                    remainder = os.read(process.stdout.fileno(), 65_536)
                    if not remainder:
                        break
                    log_file.write(remainder)
                    if interaction.interactive:
                        _write_stream(console_stream, console_filter.feed(remainder))
                    stopped = consume_lines(remainder)
                    if stopped is not None and drained_crash is None:
                        drained_crash = stopped
                log_file.flush()
            # A guest that panics on its way out writes the crash line and exits
            # inside one poll interval, so the scan has to run once more with the
            # diagnostic file closed by QEMU; otherwise the panic that killed the
            # run is still reported as `incomplete-case`.
            if crash_watch and diagnostic_log_path is not None:
                final_crash = scan_diagnostic_stream()
                if final_crash is not None and drained_crash is None:
                    drained_crash = final_crash
            if interaction.interactive:
                _write_stream(console_stream, console_filter.feed(b"", final=True))
            consume_lines(b"", final=True)
            if qmp_controller is not None:
                qmp_controller.settle()
            if drained_crash is not None:
                return drained_crash
            if qmp_controller is not None:
                if qmp_controller.error is not None:
                    raise qmp_controller.error
            if qmp_controller is not None:
                if not qmp_controller.complete:
                    raise ProcessError(
                        f"QEMU exited before QMP graphics controls completed (returncode={returncode})"
                    )
            if interaction.input_after_marker is not None and not input_ready:
                return (
                    returncode if returncode != 0 else 4,
                    f"QEMU exited before input-ready marker: {interaction.input_after_marker}",
                    False,
                    None,
                )
            if active_case is not None:
                return returncode or 4, f"QEMU exited before test completed: {active_case[0]}", False, "incomplete-case"
            return returncode, None, False, None

        now = time.monotonic()
        if active_case is not None and now >= active_case[1]:
            message = f"QEMU test timed out: {active_case[0]}; see console log for last serial output"
            _terminate_with_grace(process)
            return 124, message, False, "case-timeout"
        if limits.total_timeout_secs is not None and now - started_at >= limits.total_timeout_secs:
            message = f"QEMU timed out after {limits.total_timeout_secs:g}s; last test={last_case or 'none'}"
            _terminate_with_grace(process)
            return 124, message, False, "total-timeout"


def run_process(
    *,
    command: tuple[str, ...],
    workdir: Path,
    log_path: Path,
    limits: RunLimits,
    interaction: Interaction,
    input_stream: BinaryIO | None = None,
    console_stream: BinaryIO | None = None,
    pass_fds: tuple[int, ...] = (),
    diagnostic_log_path: Path | None = None,
    qmp_socket: Path | None = None,
    qmp_vcpu_host_cpus: tuple[int, ...] = (),
    screenshot: Path | None = None,
    qmp_input_events: tuple[tuple[Mapping[str, object], ...], ...] = (),
    qmp_input_after_marker: str | None = None,
    qmp_screenshot_after_marker: str | None = None,
    qmp_timeout_secs: float = 5.0,
    qmp_screenshot_size: tuple[int, int] | None = None,
    qmp_screenshot_color_blocks: tuple[QmpColorBlock, ...] = (),
    qmp_screenshot_text_cells: QmpTextCells | None = None,
    qmp_checkpoints: tuple[QmpCheckpoint, ...] = (),
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
        screenshot_text_cells=qmp_screenshot_text_cells,
        checkpoints=qmp_checkpoints,
    )
    if qmp_socket is None and (
        screenshot is not None
        or qmp_input_events
        or qmp_input_after_marker is not None
            or qmp_screenshot_after_marker is not None
            or qmp_checkpoints
            or qmp_vcpu_host_cpus
    ):
        raise ProcessError("QMP controls require a QMP socket")
    if qmp_vcpu_host_cpus and "-S" not in command:
        raise ProcessError("vCPU affinity requires paused QEMU (-S)")
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
    process: subprocess.Popen[bytes] | None = None
    proxy_input = interaction.input_after_marker is not None
    launched = False
    error_message: str | None = None
    marker_success = False
    runner_terminated = False
    runner_termination_reason: str | None = None
    qmp_controller: _QmpController | None = None
    with _defer_termination_signals() as termination_signals:
        try:
            if proxy_input:
                process_stdin: int | BinaryIO | None = subprocess.PIPE
            elif interaction.interactive:
                process_stdin = input_stream
            else:
                process_stdin = subprocess.DEVNULL
            initial_diagnostic_offset: int | None = None
            if interaction.detect_kernel_crash and diagnostic_log_path is not None:
                try:
                    initial_diagnostic_offset = diagnostic_log_path.stat().st_size
                except OSError:
                    initial_diagnostic_offset = 0
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
                if qmp_socket is not None and (screenshot is not None or qmp_input_events or qmp_checkpoints or qmp_vcpu_host_cpus):
                    qmp_controller = _QmpController(
                        socket_path=qmp_socket,
                        vcpu_host_cpus=qmp_vcpu_host_cpus,
                        qemu_pid=process.pid,
                        screenshot=screenshot,
                        input_events=qmp_input_events,
                        input_after_marker=qmp_input_after_marker,
                        screenshot_after_marker=qmp_screenshot_after_marker,
                        timeout_secs=qmp_timeout_secs,
                        screenshot_size=qmp_screenshot_size,
                        screenshot_color_blocks=qmp_screenshot_color_blocks,
                        screenshot_text_cells=qmp_screenshot_text_cells,
                        checkpoints=qmp_checkpoints,
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
                    termination_signals=termination_signals,
                    qmp_controller=qmp_controller,
                    diagnostic_log_path=diagnostic_log_path,
                    initial_diagnostic_offset=initial_diagnostic_offset,
                )
                if qmp_controller is not None:
                    for index, latency_ns in qmp_controller.latency_metrics:
                        event = json.dumps(
                            {"kind": "input_to_visible", "index": index, "ns": latency_ns},
                            separators=(",", ":"),
                        )
                        log_file.write(f"THEKERNEL_GRAPHICS_METRIC {event}\n".encode())
                    log_file.flush()
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
            # A caller-supplied stream or a controller can raise an unexpected
            # exception (for example TypeError). Preserve that exception, but
            # never leave the child running after its owning invocation exits.
            if process is not None and process.poll() is None:
                _terminate_with_grace(process)
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

        if error_message is not None and diagnostic_log_path is not None:
            error_message += f"; console log: {log_path}; kernel log: {diagnostic_log_path}"
        return RunResult(
            returncode=returncode,
            log_path=log_path,
            diagnostic_log_path=diagnostic_log_path,
            error_message=error_message,
            marker_success=marker_success,
            runner_terminated=(
                runner_terminated or runner_termination_reason is not None
            ),
            runner_termination_reason=runner_termination_reason,
            vcpu_affinity=qmp_controller.vcpu_affinity if qmp_controller is not None else (),
        )
