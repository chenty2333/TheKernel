#!/usr/bin/env python3
"""Strict host compile and smoke coverage for the MM guest helper."""

from __future__ import annotations

import os
import re
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
SOURCE = REPO_ROOT / "tests" / "guest" / "tools" / "mm-performance.c"


class MmPerformanceGuestTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        compiler = shutil.which("cc")
        if compiler is None:
            raise unittest.SkipTest("host C compiler is unavailable")
        cls.temporary = tempfile.TemporaryDirectory()
        cls.binary = Path(cls.temporary.name) / "mm-performance"
        build = subprocess.run(
            [
                compiler,
                "-std=c11",
                "-O2",
                "-Wall",
                "-Wextra",
                "-Werror",
                "-pthread",
                str(SOURCE),
                "-o",
                str(cls.binary),
            ],
            cwd=REPO_ROOT,
            check=False,
            capture_output=True,
            text=True,
        )
        if build.returncode != 0:
            raise AssertionError(build.stderr)

    @classmethod
    def tearDownClass(cls) -> None:
        cls.temporary.cleanup()

    def run_helper(self, workers: int) -> subprocess.CompletedProcess[str]:
        if len(os.sched_getaffinity(0)) < workers:
            self.skipTest(f"host affinity exposes fewer than {workers} CPUs")
        return subprocess.run(
            [
                str(self.binary),
                "--iterations",
                "1",
                "--vmas",
                "4",
                "--pin-iterations",
                "1",
                "--pin-workers",
                str(workers),
            ],
            cwd=REPO_ROOT,
            check=False,
            capture_output=True,
            text=True,
            timeout=60,
        )

    def test_cross_address_space_workers_are_distinct_processes(self) -> None:
        result = self.run_helper(2)

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(
            "MM_PERF_RUN schema=thekernel-mm-performance-run-v3 arch=host "
            "iterations=1 vmas=4 pin_iterations=1 pin_workers=2 "
            f"page_size={os.sysconf('SC_PAGESIZE')}",
            result.stdout,
        )
        metric = next(
            line
            for line in result.stdout.splitlines()
            if line.startswith("MM_PERF metric=direct_io_pin_proxy_cross_as_contention ")
        )
        self.assertRegex(
            metric,
            r" status=ok count=2 .* throughput_bytes_per_sec=[1-9][0-9]* "
            r"requested_vmas=4 fixture_vmas=4$",
        )
        worker_lines = [
            line
            for line in result.stdout.splitlines()
            if line.startswith("MM_PERF_PIN_CROSS_AS_WORKER ")
        ]
        self.assertEqual(len(worker_lines), 2)
        workers = [
            re.fullmatch(
                r"MM_PERF_PIN_CROSS_AS_WORKER status=ok worker=([0-9]+) "
                r"pid=([1-9][0-9]*) cpu=([0-9]+) completed=1 "
                r"p99_ns=([1-9][0-9]*) fixture_before_vmas=4 "
                r"fixture_after_vmas=4 cow_isolated=1",
                line,
            )
            for line in worker_lines
        ]
        self.assertTrue(all(match is not None for match in workers))
        pids = {int(match.group(2)) for match in workers if match is not None}
        cpus = {int(match.group(3)) for match in workers if match is not None}
        self.assertEqual(len(pids), 2)
        self.assertEqual(len(cpus), 2)

    def test_single_worker_reports_cross_as_metric_as_missing(self) -> None:
        result = self.run_helper(1)

        self.assertEqual(result.returncode, 0, result.stderr)
        metric = next(
            line
            for line in result.stdout.splitlines()
            if line.startswith("MM_PERF metric=direct_io_pin_proxy_cross_as_contention ")
        )
        self.assertIn("status=missing count=0", metric)
        self.assertIn("reason=insufficient_online_cpus errno=0", metric)

    def test_disjoint_mremap_workers_overlap_without_slot_aliasing(self) -> None:
        if len(os.sched_getaffinity(0)) < 2:
            self.skipTest("host affinity exposes fewer than 2 CPUs")
        result = subprocess.run(
            [
                str(self.binary),
                "--iterations",
                "32",
                "--vmas",
                "4",
                "--pin-iterations",
                "1",
                "--pin-workers",
                "2",
            ],
            cwd=REPO_ROOT,
            check=False,
            capture_output=True,
            text=True,
            timeout=60,
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        metric = next(
            line
            for line in result.stdout.splitlines()
            if line.startswith(
                "MM_PERF metric=mremap_disjoint_same_as_contention "
            )
        )
        self.assertRegex(
            metric,
            r" status=ok count=64 .* requested_vmas=4 fixture_vmas=4$",
        )
        worker_lines = [
            line
            for line in result.stdout.splitlines()
            if line.startswith("MM_PERF_MREMAP_WORKER ")
        ]
        self.assertEqual(len(worker_lines), 2)
        workers = []
        for line in worker_lines:
            fields = dict(token.split("=", 1) for token in line.split()[1:])
            self.assertEqual(fields["status"], "ok")
            self.assertEqual(int(fields["completed"]), 32)
            self.assertEqual(int(fields["fixture_before_vmas"]), 4)
            self.assertEqual(int(fields["fixture_after_vmas"]), 4)
            self.assertGreater(int(fields["p99_ns"]), 0)
            workers.append({key: int(value) for key, value in fields.items()
                            if key != "status"})
        self.assertEqual({worker["worker"] for worker in workers}, {0, 1})
        self.assertEqual(len({worker["cpu"] for worker in workers}), 2)
        page_size = os.sysconf("SC_PAGESIZE")
        allowed_cpus = os.sched_getaffinity(0)
        for worker in workers:
            self.assertIn(worker["cpu"], allowed_cpus)
            self.assertEqual(worker["bytes"], page_size * 2)
            self.assertEqual(worker["slot_a"] % page_size, 0)
            self.assertEqual(worker["slot_b"] % page_size, 0)
        self.assertLess(
            max(worker["start_ns"] for worker in workers),
            min(worker["end_ns"] for worker in workers),
        )
        ranges = [
            (worker[slot], worker[slot] + worker["bytes"])
            for worker in workers
            for slot in ("slot_a", "slot_b")
        ]
        for index, left in enumerate(ranges):
            for right in ranges[index + 1 :]:
                self.assertFalse(left[0] < right[1] and right[0] < left[1])

    def test_child_setup_failure_reaps_every_forked_worker(self) -> None:
        compiler = shutil.which("cc")
        assert compiler is not None
        harness_source = Path(self.temporary.name) / "failure-harness.c"
        harness_binary = Path(self.temporary.name) / "failure-harness"
        harness_source.write_text(
            f"""
#define main thekernel_mm_performance_main
#include "{SOURCE.as_posix()}"
#undef main

int main(void)
{{
    const long page_size = sysconf(_SC_PAGESIZE);
    const int cpus[2] = {{CPU_SETSIZE - 1, CPU_SETSIZE - 1}};
    struct vma_fixture_report fixture;
    struct metric_result result;
    int status;

    if (page_size <= 0) {{
        return 10;
    }}
    result = run_direct_io_pin_proxy_cross_as_contention(2, 1, cpus, 4,
                                         (size_t)page_size, &fixture);
    if (result.ok || result.reason == NULL ||
        strcmp(result.reason, "cross_as_child_setup_failed") != 0) {{
        return 11;
    }}
    errno = 0;
    if (waitpid(-1, &status, WNOHANG) != -1 || errno != ECHILD) {{
        return 12;
    }}
    return 0;
}}
""",
            encoding="utf-8",
        )
        build = subprocess.run(
            [
                compiler,
                "-std=c11",
                "-O2",
                "-Wall",
                "-Wextra",
                "-Werror",
                "-pthread",
                str(harness_source),
                "-o",
                str(harness_binary),
            ],
            cwd=REPO_ROOT,
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertEqual(build.returncode, 0, build.stderr)

        result = subprocess.run(
            [str(harness_binary)],
            cwd=REPO_ROOT,
            check=False,
            capture_output=True,
            text=True,
            timeout=10,
        )
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_address_space_switch_child_exit_is_structured_and_restores_sigpipe(self) -> None:
        compiler = shutil.which("cc")
        assert compiler is not None
        harness_source = Path(self.temporary.name) / "switch-failure-harness.c"
        harness_binary = Path(self.temporary.name) / "switch-failure-harness"
        harness_source.write_text(
            f"""
#define main thekernel_mm_performance_main
#include "{SOURCE.as_posix()}"
#undef main

ssize_t __real_write(int fd, const void *buffer, size_t count);

static pid_t harness_parent;

static void harness_sigpipe_handler(int signal_number)
{{
    (void)signal_number;
}}

ssize_t __wrap_write(int fd, const void *buffer, size_t count)
{{
    ssize_t written;

    if (harness_parent > 0 && getpid() != harness_parent) {{
        int candidate;

        for (candidate = 3; candidate < fd; ++candidate) {{
            (void)close(candidate);
        }}
        written = __real_write(fd, buffer, count);
        _exit(written == (ssize_t)count ? 77 : 78);
    }}
    return __real_write(fd, buffer, count);
}}

int main(void)
{{
    cpu_set_t allowed;
    struct sigaction sentinel;
    struct sigaction observed;
    struct metric_result result;
    int cpu = -1;
    int status;

    harness_parent = getpid();
    if (sched_getaffinity(0, sizeof(allowed), &allowed) != 0) {{
        return 30;
    }}
    for (int candidate = 0; candidate < CPU_SETSIZE; ++candidate) {{
        if (CPU_ISSET(candidate, &allowed)) {{
            cpu = candidate;
            break;
        }}
    }}
    if (cpu < 0) {{
        return 31;
    }}
    memset(&sentinel, 0, sizeof(sentinel));
    sentinel.sa_handler = harness_sigpipe_handler;
    if (sigemptyset(&sentinel.sa_mask) != 0 ||
        sigaction(SIGPIPE, &sentinel, NULL) != 0) {{
        return 32;
    }}
    result = run_address_space_switch_ping_pong(1, cpu);
    if (result.ok || result.reason == NULL ||
        strcmp(result.reason, "address_space_switch_round_trip_failed") != 0 ||
        result.error_number != EPIPE) {{
        return 33;
    }}
    if (sigaction(SIGPIPE, NULL, &observed) != 0 ||
        observed.sa_handler != harness_sigpipe_handler) {{
        return 34;
    }}
    errno = 0;
    if (waitpid(-1, &status, WNOHANG) != -1 || errno != ECHILD) {{
        return 35;
    }}
    return 0;
}}
""",
            encoding="utf-8",
        )
        build = subprocess.run(
            [
                compiler,
                "-std=c11",
                "-O2",
                "-Wall",
                "-Wextra",
                "-Werror",
                "-pthread",
                str(harness_source),
                "-Wl,--wrap=write",
                "-o",
                str(harness_binary),
            ],
            cwd=REPO_ROOT,
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertEqual(build.returncode, 0, build.stderr)

        result = subprocess.run(
            [str(harness_binary)],
            cwd=REPO_ROOT,
            check=False,
            capture_output=True,
            text=True,
            timeout=10,
        )
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_cross_address_space_unlink_failure_invalidates_metric_and_cleans_up(self) -> None:
        if len(os.sched_getaffinity(0)) < 2:
            self.skipTest("host affinity exposes fewer than 2 CPUs")
        compiler = shutil.which("cc")
        assert compiler is not None
        harness_source = Path(self.temporary.name) / "unlink-failure-harness.c"
        harness_binary = Path(self.temporary.name) / "unlink-failure-harness"
        harness_source.write_text(
            f"""
#define main thekernel_mm_performance_main
#include "{SOURCE.as_posix()}"
#undef main

int __real_unlink(const char *path);

int __wrap_unlink(const char *path)
{{
    static int injected = 0;

    if (strstr(path, "/tmp/thekernel-mm-cross-as-") == path && !injected) {{
        injected = 1;
        errno = EIO;
        return -1;
    }}
    return __real_unlink(path);
}}

int main(void)
{{
    const long page_size = sysconf(_SC_PAGESIZE);
    cpu_set_t allowed;
    int cpus[2];
    size_t found = 0;
    struct vma_fixture_report fixture;
    struct metric_result result;

    if (page_size <= 0 || sched_getaffinity(0, sizeof(allowed), &allowed) != 0) {{
        return 20;
    }}
    for (int cpu = 0; cpu < CPU_SETSIZE && found < 2; ++cpu) {{
        if (CPU_ISSET(cpu, &allowed)) {{
            cpus[found++] = cpu;
        }}
    }}
    if (found != 2) {{
        return 21;
    }}
    result = run_direct_io_pin_proxy_cross_as_contention(
        2, 1, cpus, 4, (size_t)page_size, &fixture);
    if (result.ok || result.reason == NULL ||
        strcmp(result.reason, "cross_as_child_setup_failed") != 0 ||
        result.error_number != EIO) {{
        return 22;
    }}
    return 0;
}}
""",
            encoding="utf-8",
        )
        build = subprocess.run(
            [
                compiler,
                "-std=c11",
                "-O2",
                "-Wall",
                "-Wextra",
                "-Werror",
                "-pthread",
                str(harness_source),
                "-Wl,--wrap=unlink",
                "-o",
                str(harness_binary),
            ],
            cwd=REPO_ROOT,
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertEqual(build.returncode, 0, build.stderr)

        result = subprocess.run(
            [str(harness_binary)],
            cwd=REPO_ROOT,
            check=False,
            capture_output=True,
            text=True,
            timeout=10,
        )
        self.assertEqual(result.returncode, 0, result.stderr)


if __name__ == "__main__":
    unittest.main()
