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
