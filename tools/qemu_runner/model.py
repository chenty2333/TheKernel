"""Public data types for the product-level QEMU runner."""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import Literal, Mapping


Arch = Literal["x86_64"]
DriveMode = Literal["snapshot", "readonly", "rw"]
GraphicsProfile = Literal[
    "headless",
    "interactive",
    "virgl-headless",
    "virgl-interactive",
]

INTENTIONAL_STOP_RETURN_CODE = 75


@dataclass(frozen=True)
class Drive:
    """One block image and the write policy exposed to QEMU."""

    path: Path
    mode: DriveMode


@dataclass(frozen=True)
class RunLimits:
    """Independent wall-clock limits for one QEMU process."""

    total_timeout_secs: float | None = None
    idle_timeout_secs: float | None = None
    ready_timeout_secs: float | None = None


@dataclass(frozen=True)
class Interaction:
    """Serial input and exact-line marker behavior."""

    interactive: bool = False
    input_after_marker: str | None = None
    stop_after_marker: str | None = None


@dataclass(frozen=True)
class QmpControls:
    """Optional graphical QMP actions issued after QEMU has started.

    ``input_events`` contains the event arrays accepted by QMP's
    ``input-send-event`` command.  Keeping this as data rather than a host
    input-device abstraction makes the runner suitable for both keyboard and
    tablet injection without reintroducing legacy PS/2 devices.
    """

    socket: Path | None = None
    screenshot: Path | None = None
    input_events: tuple[tuple[Mapping[str, object], ...], ...] = ()
    input_after_marker: str | None = None
    screenshot_after_marker: str | None = None
    timeout_secs: float = 5.0
    screenshot_size: tuple[int, int] | None = None
    screenshot_color_blocks: tuple["QmpColorBlock", ...] = ()


@dataclass(frozen=True)
class QmpColorBlock:
    """An exact RGB rectangle expected in a QMP ``screendump`` PPM image."""

    x: int
    y: int
    width: int
    height: int
    rgb: tuple[int, int, int]


@dataclass(frozen=True)
class RunResult:
    """Process-level result without external result-aggregation policy."""

    arch: Arch
    command: tuple[str, ...]
    returncode: int
    duration_ms: int
    log_path: Path
    workdir: Path
    error_message: str | None = None
    marker_success: bool = False
    runner_terminated: bool = False
    runner_termination_reason: str | None = None

    @property
    def ok(self) -> bool:
        return self.returncode == 0

    @property
    def timed_out(self) -> bool:
        return self.returncode == 124

    @property
    def interrupted(self) -> bool:
        return self.returncode in (130, -2)

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

    @property
    def launch_failed(self) -> bool:
        return self.returncode == 3 and self.error_message is not None
