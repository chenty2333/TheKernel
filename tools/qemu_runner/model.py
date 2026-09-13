"""Public data types for the product-level QEMU runner."""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import Literal, Mapping


Arch = Literal["x86_64"]
DriveMode = Literal["snapshot", "readonly", "rw"]
RootfsTransport = Literal["drive", "module"]
# The graphics profile list has exactly one source of truth:
# ``tools.qemu_runner.profiles.GRAPHICS_PROFILES``.  Annotations therefore
# use this plain alias instead of a second literal list.
GraphicsProfile = str

INTENTIONAL_STOP_RETURN_CODE = 75


@dataclass(frozen=True)
class Drive:
    """One block image and the write policy exposed to QEMU."""

    path: Path
    mode: DriveMode


@dataclass(frozen=True)
class RunLimits:
    """Wall-clock limit for one QEMU process."""

    total_timeout_secs: float | None = None


@dataclass(frozen=True)
class Interaction:
    """Serial input and exact-line marker behavior."""

    interactive: bool = False
    input_after_marker: str | None = None
    stop_after_marker: str | None = None
    # Configured protocol names; match a complete prefix token boundary.
    failure_prefixes: tuple[str, ...] = ()
    # Command-file input: one newline-terminated command per exact prompt.
    input_line_after_marker: str | None = None


@dataclass(frozen=True)
class QmpControls:
    """Optional graphical QMP actions issued after QEMU has started.

    ``input_events`` contains the event arrays accepted by QMP's
    ``input-send-event`` command.  Keeping this as data rather than a host
    input-device abstraction makes the runner suitable for both keyboard and
    tablet injection without reintroducing legacy PS/2 devices.
    """

    # Indexed by guest vCPU number; verified before resuming a paused guest.
    vcpu_host_cpus: tuple[int, ...] = ()
    socket: Path | None = None
    screenshot: Path | None = None
    input_events: tuple[tuple[Mapping[str, object], ...], ...] = ()
    input_after_marker: str | None = None
    screenshot_after_marker: str | None = None
    timeout_secs: float = 5.0
    screenshot_size: tuple[int, int] | None = None
    screenshot_color_blocks: tuple["QmpColorBlock", ...] = ()
    screenshot_text_cells: "QmpTextCells | None" = None
    checkpoints: tuple["QmpCheckpoint", ...] = ()


@dataclass(frozen=True)
class QmpColorBlock:
    """An exact RGB rectangle expected in a QMP ``screendump`` PPM image."""

    x: int
    y: int
    width: int
    height: int
    rgb: tuple[int, int, int]


@dataclass(frozen=True)
class QmpConsoleLine:
    """One line of console text expected to be readable in a screendump.

    ``cells`` holds one cell bitmap per character, rendered from the console's
    own font (``tools/qemu_runner/console_font.py``).  Matching bitmaps instead
    of a string keeps the assertion about pixels -- these characters, on this
    cell grid -- and lets a font change move both sides together instead of
    silently invalidating the expectation.
    """

    label: str
    cells: tuple[tuple[int, ...], ...]


@dataclass(frozen=True)
class QmpTextCells:
    """A structural ink expectation for one QMP ``screendump`` PPM image.

    The framebuffer console paints glyphs into a fixed character grid, so a
    frame it produced has exactly two colours and no ink outside the cells it
    wrote.  Asserting that structure proves "text was rendered, and only where
    text was expected" without re-encoding the guest's font table, which would
    have to be updated in lockstep with every font change and would still be
    satisfied by a frame nobody can read.

    It is deliberately not a "the image is not blank" check.  A console which
    walks the surface at the wrong stride for its pixel depth paints a busy,
    colourful frame that such a check accepts; that frame carries no pixel of
    exactly ``ink``, so the floors and the two-colour rule below reject it.
    """

    # Character grid origin and cell size, in pixels.
    x: int = 0
    y: int = 0
    cell_width: int = 8
    cell_height: int = 16
    # Grid extent in cells.  ``None`` derives it from the screendump, which
    # must then be an exact whole number of cells; an explicit value bounds
    # the region ink is allowed to appear in at all.
    columns: int | None = None
    rows: int | None = None
    # The only two colours the console paints, as canonical RGB triples.
    ink: tuple[int, int, int] = (0xD0, 0xD0, 0xD0)
    background: tuple[int, int, int] = (0, 0, 0)
    # Floors that separate a rendered screen from a blank or unreadable one.
    min_ink_pixels: int = 512
    min_inked_cells: int = 16
    # Pixels inside the grid allowed to be neither ink nor background.  A
    # correctly rendered console produces zero of them, so this is a budget
    # for a known exception, never a tolerance for a rendering bug.
    max_foreign_pixels: int = 0
    # Assert the console's cell-border invariant: no ink in a cell's last pixel
    # column, nor in its first or last pixel row.  The kernel's font guarantees
    # this for every glyph it can draw (``no_glyph_touches_the_cell_border`` in
    # kernel/src/pseudofs/dev/console_font/tests.rs), so a frame that violates
    # it did not come from the console's glyph renderer.  That is worth its own
    # rule because a misaligned or wrongly strided render can keep the colours
    # exact and still paint text a pixel off -- which no colour rule can see.
    # Off by default: it is a property of this console, not of text in general.
    require_clear_cell_borders: bool = False
    # Whole console lines that must be readable somewhere on the grid.  This is
    # what turns "there is ink" into "the kernel's log and the guest's own
    # output are on the screen": a caller gating a screendump on a serial
    # marker knows the guest has *written* the line, and this asserts the
    # console actually presented it.  A console that records cells and defers
    # the repaint is one frame behind, so a miss is retried, not fatal.
    expected_lines: tuple["QmpConsoleLine", ...] = ()


@dataclass(frozen=True)
class QmpCheckpoint:
    """One marker-gated QMP input and optional pixel checkpoint.

    Checkpoints are executed in declaration order.  This lets a guest client
    repaint and acknowledge pointer, keyboard, and absolute-tablet input
    independently instead of treating a mixed input burst as one event.
    """

    input_after_marker: str
    # Each entry is one QMP input-send-event batch.  Keeping the outer tuple
    # aligns checkpoints with QmpControls and prevents a mapping from being
    # accidentally iterated as its "type" and "data" keys.
    input_events: tuple[tuple[Mapping[str, object], ...], ...] = ()
    screenshot: Path | None = None
    screenshot_after_marker: str | None = None
    screenshot_size: tuple[int, int] | None = None
    screenshot_color_blocks: tuple[QmpColorBlock, ...] = ()
    screenshot_text_cells: "QmpTextCells | None" = None
    pci_hotplug: tuple["QmpPciHotplug", ...] = ()
    # When set, measure from immediately before QMP input submission until
    # the guest reports that the input-driven frame became visible.  The
    # controller appends the host-monotonic sample to the captured log.
    latency_after_marker: str | None = None
    latency_index: int | None = None


@dataclass(frozen=True)
class QmpPciHotplug:
    """One QMP PCI device_add/device_del action at a checkpoint.

    Only the three Q35 VirtIO input devices are accepted.  Keeping the
    topology typed prevents a graphics smoke run from accidentally exercising
    an unowned block, network, or MMIO removal path.
    """

    action: Literal["add", "del"]
    device_id: str
    driver: Literal["virtio-keyboard-pci", "virtio-mouse-pci", "virtio-tablet-pci"] | None = None
    bus: Literal["rp-input-kbd", "rp-input-mouse", "rp-input-tablet"] | None = None


@dataclass(frozen=True)
class RunResult:
    """Process-level result without external result-aggregation policy."""

    returncode: int
    log_path: Path
    error_message: str | None = None
    marker_success: bool = False
    runner_terminated: bool = False
    runner_termination_reason: str | None = None
    # (guest CPU index, QEMU host thread ID, pinned host CPU).
    vcpu_affinity: tuple[tuple[int, int, int], ...] = ()

    diagnostic_log_path: Path | None = None

    @property
    def intentionally_stopped(self) -> bool:
        return (
            self.returncode == INTENTIONAL_STOP_RETURN_CODE
            and self.marker_success
            and self.runner_terminated
            and self.runner_termination_reason == "stop-after-marker"
            and self.error_message is not None
            and self.error_message.startswith("QEMU stopped after marker: ")
        )

    @property
    def guest_clean_shutdown(self) -> bool:
        """Whether QEMU exited cleanly without a runner-initiated stop."""

        return self.returncode == 0 and not self.runner_terminated
