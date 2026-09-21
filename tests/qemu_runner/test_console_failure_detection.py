"""A crashed kernel must be reported as a crash, not as the stall it imitates.

Three of the gating lanes hang on this verdict.  `test --suite guest` runs the
KTAP shell workload, `test --suite abi` boots Linux as the ABI oracle, and
`test --suite fbcon` accepts a framebuffer console by screenshot.  Until now a
panic in the middle of any of them looked exactly like a slow test: the guest
stops making progress, the case deadline or the watchdog expires, and the
verdict is `124 case-timeout` -- a false statement about a kernel that is
already dead, and one that costs an operator the whole run budget to disprove.
The benchmark lane already refused that lie (`tools/qemu_runner/kernel_benchmark.py`
turns `Kernel panic|scheduler timeout` into a failure); this is the same idea
for the lanes that decide whether a commit is good.

Detection covers both captured streams on purpose.  TheKernel's panic handler
writes to the *diagnostic* UART (`crates/ax/tk-axruntime/src/lang_items.rs`,
whose `emergency_diagnostic_print` is its only writer for this), which QEMU
appends straight to `kernel.log` -- not to the console the runner consumes line
by line -- so console-only detection would not fix the guest lane at all.  The
patterns are the strings those producers actually emit.  Every test here feeds
a synthetic console or diagnostic log to `run_process`, the same entry point
the product harness uses; nothing boots.
"""

from __future__ import annotations

import io
import sys
import unittest
from pathlib import Path

from tests.support import test_tmpdir
from tools.qemu_runner.model import (
    Interaction,
    KERNEL_CRASH_RETURN_CODE,
    KERNEL_CRASH_TERMINATION_REASON,
    RunLimits,
)
from tools.qemu_runner.process import _crash_line, run_process

INTERPRETER = sys.executable


def child(code: str) -> tuple[str, ...]:
    return (INTERPRETER, "-c", code)


def run(root: Path, code: str, *, interaction: Interaction = Interaction(),
        limits: RunLimits = RunLimits(total_timeout_secs=60),
        diagnostic: Path | None = None):
    """One `run_process` over a pretend guest, exactly as the harness calls it."""

    return run_process(
        command=child(code),
        workdir=root,
        log_path=root / "console.log",
        diagnostic_log_path=diagnostic,
        limits=limits,
        interaction=interaction,
        console_stream=io.BytesIO(),
    )


# The header a `panic = "abort"` kernel prints, in the shape
# `lang_items.rs` emits it: the panic message over the diagnostic UART, then
# the `Backtrace:` line the unwinder prints next, then the halt.
PANIC_CONSOLE = (
    "[    0.023456] Kernel panic - not syncing: kernel_stack_overflow\n"
    "[    0.023460] CPU: 1 UID: 0 PID: 1 Comm: init\n"
    "[    0.023461] RIP: 0033:0x4016f0\n"
)


class KernelCrashLineRuleTests(unittest.TestCase):
    """The pattern set, and the output it must not take offence to."""

    def test_matches_every_form_a_panic_printer_actually_emits(self) -> None:
        for line in (
            # kernel/panic.c: `panic()` and `barrier_error()`.
            "Kernel panic - not syncing: Attempted to kill init! exitcode=0x00000009",
            "[    0.023456] Kernel panic - not syncing: VFS: Unable to mount root fs",
            # kernel/exit.c: the two unrecoverable task states.
            "Kernel panic - not syncing: Attempted to kill the idle task!",
            "Attempted to kill init! exitcode=0x0000000b",
            # arch/x86/kernel/traps.c: `__die()`'s `%s: %04lx [%04d]` header.
            "Oops: 0000 [#1] SMP PTI",
            "[   12.345678] general protection fault, probably for non-canonical address",
            "double fault: 08, flags in process init (pid=1)",
            # lib/bug.c and the fault trap's OOM report.
            "kernel BUG at mm/mmap.c:3451!",
            "BUG: unable to handle page fault for address: 0000000000000010",
            # kernel/printk: the `printk_bug()` banner and the watchdog.
            "BUG: soft lockup - CPU#3 stuck for 23s! [stress:412]",
            "BUG: workqueue lockup - CPUS 1-7 stuck for 26s",
            # tk-axruntime's own `#[panic_handler]` output.
            "panicked at crates/ax/tk-axruntime/src/lang_items.rs:64:5:",
            # tk-axcpu's x86 trap stubs.
            "[    0.006120] Unhandled Exception at 0x000000000042619f",
            "[    0.006120] Unhandled #PF @ 0x0000000000425678, code=0x00000002",
            "[    0.006120] #GP @ 0x00000000004261a8, code=0x00000000",
            "[    0.006120] Unhandled interrupt 0x20 at 0x00000000004261a8",
        ):
            with self.subTest(line=line):
                self.assertIsNotNone(_crash_line(line))

    def test_ignores_healthy_and_quoted_output(self) -> None:
        for line in (
            "ok 1 - basic socket pair",
            "# Subtest: futex wait",
            "1..14",
            "not ok 1 - planned to have 3, got 2",
            # A line that merely mentions the words is not a kernel giving up.
            "KTAP-TAP-14: no BUG: in this harness",
            "# debug: socket.c:12 BUG: not a kernel bug at all",
            "  Failed to open /dev/null: No such file or directory",
            "smp: Brought up 1 node, 4 CPUs",
            "Scheduler: timer interrupt at 100 Hz",
            # A differential run replays a Linux transcript *through* the guest
            # console.  `tools/guest-test/ktap.c` prefixes each quoted line
            # with `THEKERNEL_<category>_INNER: `, so a panic on the *other*
            # side of the comparison is data about Linux, not this guest dying;
            # the prefix disqualifies it here.
            "THEKERNEL_abi_INNER: Kernel panic - not syncing: Attempted to kill init!",
        ):
            with self.subTest(line=line):
                self.assertIsNone(_crash_line(line))

    def test_one_line_yields_one_verdict(self) -> None:
        # The rule is anchored at column zero: a panic that quotes a `BUG:`
        # location in its own message is one crash, not a cascade of them.
        matched = _crash_line("Kernel panic - not syncing: kernel BUG at fs/super.c:1148!")
        self.assertIsNotNone(matched)
        self.assertEqual(matched.string.count("Kernel panic"), 1)


class CrashVerdictTests(unittest.TestCase):
    """The verdict a lane receives, and the verdicts it must still receive."""

    def test_console_panic_is_a_crash_attributed_to_the_running_case(self) -> None:
        with test_tmpdir() as directory:
            root = Path(directory)
            result = run(root, (
                "print('TAP version 14', flush=True)\n"
                "print('# THEKERNEL_TEST_BEGIN 1 shell-boot timeout_seconds=60', flush=True)\n"
                "print('Booting Linux on consolidated node 0', flush=True)\n"
                f"print({PANIC_CONSOLE!r}, flush=True)\n"
                "import time; time.sleep(60)\n"
            ))
            self.assertEqual(result.returncode, KERNEL_CRASH_RETURN_CODE,
                             result.error_message)
            self.assertEqual(result.returncode, 70)
            self.assertEqual(result.runner_termination_reason, "kernel-crash")
            self.assertFalse(result.marker_success)
            self.assertIn("while running test 1 shell-boot", result.error_message)
            self.assertIn("Kernel panic - not syncing: kernel_stack_overflow",
                          result.error_message)
            self.assertIn("(console stream)", result.error_message)
            # Not a timeout, in either direction: no watchdog wording, and the
            # watchdog's own exit code is not what the lane reports.
            self.assertNotIn("timeout", result.error_message)
            self.assertNotEqual(result.returncode, 124)

    def test_a_panic_that_only_reaches_the_diagnostic_uart_is_still_a_crash(self) -> None:
        # This is the shape the guest suite really produces, and the reason the
        # runner reads a second stream: without it the run dies as
        # `case-timeout` exactly as before.
        with test_tmpdir() as directory:
            root = Path(directory)
            diagnostic = root / "kernel.log"
            result = run(root, (
                "from pathlib import Path\n"
                "print('# THEKERNEL_TEST_BEGIN 2 desktop-boot timeout_seconds=60', flush=True)\n"
                f"Path({str(diagnostic)!r}).write_text("
                "'panicked at crates/ax/tk-axruntime/src/lang_items.rs:70:5:\\n'\n"
                ")\n"
                "import time; time.sleep(60)\n"
            ), diagnostic=diagnostic)
            self.assertEqual(result.returncode, 70, result.error_message)
            self.assertEqual(result.runner_termination_reason, "kernel-crash")
            self.assertIn("while running test 2 desktop-boot", result.error_message)
            self.assertIn("(diagnostic stream)", result.error_message)
            self.assertIn(str(diagnostic), result.error_message)

    def test_a_crash_before_the_first_case_is_attributed_to_no_case(self) -> None:
        with test_tmpdir() as directory:
            root = Path(directory)
            diagnostic = root / "kernel.log"
            result = run(root, (
                "from pathlib import Path\n"
                f"Path({str(diagnostic)!r}).write_text('Oops: 0002 [#1] SMP PTI\\n')\n"
                "import time; time.sleep(60)\n"
            ), diagnostic=diagnostic)
            self.assertEqual(result.returncode, 70, result.error_message)
            self.assertIn("while running test none", result.error_message)

    def test_a_stalled_guest_is_still_the_watchdogs_to_report(self) -> None:
        # The reverse regression matters as much: making a crash visible must
        # not turn an ordinary hang into a crash.
        with test_tmpdir() as directory:
            root = Path(directory)
            result = run(root, (
                "print('# THEKERNEL_TEST_BEGIN 1 hung timeout_seconds=60', flush=True)\n"
                "import time; time.sleep(60)\n"
            ), limits=RunLimits(total_timeout_secs=2))
            self.assertEqual(result.returncode, 124, result.error_message)
            self.assertEqual(result.runner_termination_reason, "total-timeout")
            self.assertIn("QEMU timed out", result.error_message)

    def test_a_clean_guest_is_unaffected_by_the_new_rule(self) -> None:
        with test_tmpdir() as directory:
            root = Path(directory)
            result = run(root, (
                "print('# THEKERNEL_TEST_BEGIN 1 shell-boot timeout_seconds=60', flush=True)\n"
                "print('# THEKERNEL_TEST_END 1 shell-boot result=0', flush=True)\n"
                "print('ok 1 - shell boot', flush=True)\n"
                "print('1..1', flush=True)\n"
            ))
            self.assertEqual(result.returncode, 0, result.error_message)
            self.assertIsNone(result.runner_termination_reason)

    def test_configured_failure_prefixes_still_win(self) -> None:
        # Lanes that name their own fatal lines -- the benchmark lane's
        # `Kernel panic|scheduler timeout` -- keep their existing behaviour:
        # that path raises and surfaces as the I/O failure it already was.
        with test_tmpdir() as directory:
            root = Path(directory)
            result = run(root, (
                "print('Kernel panic - not syncing: scheduled by the benchmark', flush=True)\n"
                "import time; time.sleep(30)\n"
            ), interaction=Interaction(failure_prefixes=("Kernel panic",)))
            self.assertEqual(result.returncode, 4, result.error_message)
            self.assertIn("guest reported failure", result.error_message)

    def test_a_lane_can_opt_out_of_crash_detection(self) -> None:
        with test_tmpdir() as directory:
            root = Path(directory)
            result = run(root, f"print({PANIC_CONSOLE!r}, flush=True)",
                         interaction=Interaction(detect_kernel_crash=False))
            self.assertEqual(result.returncode, 0, result.error_message)
            self.assertIsNone(result.runner_termination_reason)

    def test_the_verdict_is_distinguishable_from_every_other_outcome(self) -> None:
        # 124 is the watchdog's and 75 is the intentional-stop marker's; the
        # crash code has to be neither, or a caller that already branches on
        # them cannot tell a dead kernel from a slow one or a finished one.
        self.assertEqual(KERNEL_CRASH_RETURN_CODE, 70)
        self.assertEqual(KERNEL_CRASH_TERMINATION_REASON, "kernel-crash")
        self.assertNotIn(KERNEL_CRASH_RETURN_CODE, (0, 75, 124))
        self.assertTrue(Interaction().detect_kernel_crash)


if __name__ == "__main__":
    unittest.main()
