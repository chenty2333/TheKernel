"""Public data types for the product-level QEMU runner."""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import Literal


Arch = Literal["rv", "la"]
DriveMode = Literal["snapshot", "readonly", "rw"]

INTENTIONAL_STOP_RETURN_CODE = 75


@dataclass(frozen=True)
class PreparedImage:
    """An image source and the uncompressed path passed to QEMU."""

    source: Path
    runtime: Path
    cached: bool = False


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
class RunResult:
    """Process-level result without external result-aggregation policy."""

    arch: Arch
    command: tuple[str, ...]
    returncode: int
    duration_ms: int
    log_path: Path
    workdir: Path
    error_message: str | None = None

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
            and self.error_message is not None
            and self.error_message.startswith("QEMU stopped after marker: ")
        )

    @property
    def launch_failed(self) -> bool:
        return self.returncode == 3 and self.error_message is not None
