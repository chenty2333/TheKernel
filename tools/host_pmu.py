"""Small, dependency-free host PMU capture for bounded benchmark runs.

This deliberately uses ``perf_event_open`` directly instead of the ``perf``
CLI.  The counter is attached to the controller process with inheritance, so
the window covers its QEMU/pinner descendants while it is enabled.  It is not
attributed to individual guest samples; because ``exclude_guest`` is false,
guest-mode user execution on inherited QEMU vCPU threads may contribute.
"""

from __future__ import annotations

import ctypes
import errno
import fcntl
import os
import struct
from dataclasses import dataclass
from typing import Callable


# x86_64 Linux syscall ABI.  TheKernel supports x86_64 only.
SYS_PERF_EVENT_OPEN = 298
PERF_TYPE_HARDWARE = 0
PERF_COUNT_HW_CPU_CYCLES = 0
PERF_COUNT_HW_INSTRUCTIONS = 1
PERF_COUNT_HW_CACHE_MISSES = 3
PERF_COUNT_HW_BRANCH_MISSES = 5
PERF_FORMAT_TOTAL_TIME_ENABLED = 1 << 0
PERF_FORMAT_TOTAL_TIME_RUNNING = 1 << 1
PERF_FORMAT_GROUP = 1 << 3
PERF_EVENT_IOC_ENABLE = 0x2400
PERF_EVENT_IOC_DISABLE = 0x2401
PERF_EVENT_IOC_RESET = 0x2403
PERF_IOC_FLAG_GROUP = 1
PERF_FLAG_FD_CLOEXEC = 1 << 3


class PmuUnavailable(RuntimeError):
    """The host declined the requested inherited PMU measurement."""


@dataclass(frozen=True)
class PmuReading:
    counters: dict[str, int]
    time_enabled: int
    time_running: int

    @property
    def multiplexed(self) -> bool:
        return self.time_running != self.time_enabled

    @property
    def scale(self) -> float | None:
        if self.time_running == 0:
            return None
        return self.time_enabled / self.time_running


_EVENTS = (
    ("cycles", PERF_COUNT_HW_CPU_CYCLES),
    ("instructions", PERF_COUNT_HW_INSTRUCTIONS),
    ("cache_misses", PERF_COUNT_HW_CACHE_MISSES),
    ("branch_misses", PERF_COUNT_HW_BRANCH_MISSES),
)
_LIBC = ctypes.CDLL(None, use_errno=True)


def _perf_event_attr(config: int) -> bytes:
    """Build the stable prefix of Linux ``struct perf_event_attr``.

    The 128-byte layout has been stable for all hosts this x86_64-only project
    supports.  Bit 1 asks child tasks to inherit; bits 5/6 exclude host kernel
    and KVM/hypervisor execution.  ``exclude_guest`` remains false, so guest
    user execution may be included when inherited QEMU vCPU threads run it.
    """

    attr = bytearray(128)
    struct.pack_into("IIQ", attr, 0, PERF_TYPE_HARDWARE, len(attr), config)
    struct.pack_into(
        "Q", attr, 32,
        PERF_FORMAT_GROUP | PERF_FORMAT_TOTAL_TIME_ENABLED | PERF_FORMAT_TOTAL_TIME_RUNNING,
    )
    flags = (1 << 0) | (1 << 1) | (1 << 5) | (1 << 6)  # disabled, inherit, exclude kernel/hv
    struct.pack_into("Q", attr, 40, flags)
    return bytes(attr)


class UserProcessTreePmu:
    """A grouped PMU window for this process and its inherited descendants."""

    def __init__(
        self,
        *,
        syscall: Callable[..., int] | None = None,
        ioctl: Callable[..., int] = fcntl.ioctl,
        read: Callable[[int, int], bytes] = os.read,
        close: Callable[[int], None] = os.close,
    ) -> None:
        self._syscall = syscall or _LIBC.syscall
        self._ioctl = ioctl
        self._read = read
        self._close = close
        self._fds: list[int] = []
        self._started = False

    def open(self) -> None:
        if self._fds:
            raise RuntimeError("PMU group is already open")
        try:
            for index, (_, config) in enumerate(_EVENTS):
                attr = _perf_event_attr(config)
                raw = ctypes.create_string_buffer(attr)
                fd = int(self._syscall(
                    SYS_PERF_EVENT_OPEN, ctypes.byref(raw), 0, -1,
                    -1 if index == 0 else self._fds[0], PERF_FLAG_FD_CLOEXEC,
                ))
                if fd < 0:
                    error = ctypes.get_errno()
                    raise OSError(error, os.strerror(error))
                self._fds.append(fd)
        except OSError as error:
            self.close()
            raise PmuUnavailable(f"perf_event_open inherited process-tree PMU unavailable: {error}") from error

    def start(self) -> None:
        if len(self._fds) != len(_EVENTS):
            raise RuntimeError("PMU group is not open")
        try:
            self._ioctl(self._fds[0], PERF_EVENT_IOC_RESET, PERF_IOC_FLAG_GROUP)
            self._ioctl(self._fds[0], PERF_EVENT_IOC_ENABLE, PERF_IOC_FLAG_GROUP)
        except OSError as error:
            raise PmuUnavailable(f"could not enable inherited PMU group: {error}") from error
        self._started = True

    def stop(self) -> PmuReading:
        if not self._started:
            raise RuntimeError("PMU group was not started")
        try:
            self._ioctl(self._fds[0], PERF_EVENT_IOC_DISABLE, PERF_IOC_FLAG_GROUP)
            payload = self._read(self._fds[0], 24 + 8 * len(_EVENTS))
        except OSError as error:
            raise PmuUnavailable(f"could not read inherited PMU group: {error}") from error
        finally:
            self._started = False
        if len(payload) != 24 + 8 * len(_EVENTS):
            raise PmuUnavailable(f"short PMU group read: got {len(payload)} bytes")
        values = struct.unpack("=" + "Q" * (3 + len(_EVENTS)), payload)
        count, enabled, running = values[:3]
        if count != len(_EVENTS):
            raise PmuUnavailable(f"PMU group returned {count} counters, expected {len(_EVENTS)}")
        if running == 0:
            raise PmuUnavailable("PMU group had zero time_running")
        return PmuReading(
            counters={name: value for (name, _), value in zip(_EVENTS, values[3:])},
            time_enabled=enabled,
            time_running=running,
        )

    def close(self) -> None:
        while self._fds:
            fd = self._fds.pop()
            try:
                self._close(fd)
            except OSError:
                pass
        self._started = False

    def __enter__(self) -> "UserProcessTreePmu":
        self.open()
        return self

    def __exit__(self, *_: object) -> None:
        self.close()
