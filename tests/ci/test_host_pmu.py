from __future__ import annotations

import ctypes
import struct
import unittest

from tools.host_pmu import (
    PERF_EVENT_IOC_DISABLE,
    PERF_EVENT_IOC_ENABLE,
    PERF_EVENT_IOC_RESET,
    PmuUnavailable,
    UserProcessTreePmu,
)


class HostPmuTests(unittest.TestCase):
    def test_group_open_start_read_uses_user_only_inherited_events(self) -> None:
        opens: list[tuple[int, int, int, bytes]] = []
        ioctls: list[tuple[int, int, int]] = []
        closed: list[int] = []

        def syscall(number: int, attr: object, pid: int, cpu: int, group: int, flags: int) -> int:
            raw = ctypes.string_at(attr, 128)
            opens.append((pid, cpu, group, raw))
            return 40 + len(opens)

        def ioctl(fd: int, command: int, argument: int) -> int:
            ioctls.append((fd, command, argument))
            return 0

        payload = struct.pack("=" + "Q" * 7, 4, 100, 80, 11, 22, 33, 44)
        pmu = UserProcessTreePmu(
            syscall=syscall, ioctl=ioctl, read=lambda _fd, _size: payload,
            close=closed.append,
        )
        pmu.open()
        pmu.start()
        reading = pmu.stop()
        pmu.close()

        self.assertEqual([(pid, cpu) for pid, cpu, _, _ in opens], [(0, -1)] * 4)
        self.assertEqual(opens[0][2], -1)
        self.assertEqual([group for _, _, group, _ in opens[1:]], [41, 41, 41])
        attr_flags = struct.unpack_from("Q", opens[0][3], 40)[0]
        self.assertEqual(
            attr_flags & ((1 << 1) | (1 << 5) | (1 << 6)),
            (1 << 1) | (1 << 5) | (1 << 6),
        )
        self.assertEqual(attr_flags & (1 << 20), 0)  # exclude_guest=false
        self.assertEqual(
            [command for _, command, _ in ioctls],
            [PERF_EVENT_IOC_RESET, PERF_EVENT_IOC_ENABLE, PERF_EVENT_IOC_DISABLE],
        )
        self.assertEqual(reading.counters, {"cycles": 11, "instructions": 22, "cache_misses": 33, "branch_misses": 44})
        self.assertTrue(reading.multiplexed)
        self.assertEqual(reading.scale, 1.25)
        self.assertEqual(closed, [44, 43, 42, 41])

    def test_open_failure_is_unavailable_and_never_fabricates_values(self) -> None:
        def denied(*_args: object) -> int:
            ctypes.set_errno(1)
            return -1

        with self.assertRaises(PmuUnavailable):
            UserProcessTreePmu(syscall=denied).open()


if __name__ == "__main__":
    unittest.main()
