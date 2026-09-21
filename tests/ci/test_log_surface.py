"""The raw-print ratchet must count what the kernel builds, and nothing else.

`scripts/ci/check_log_surface.py` holds two promises that are easy to get wrong
in opposite directions: a print the compiler would build has to be seen, and a
print that only exists in a documentation example, a host test module or a
string does not. Both are tested here against synthetic sources, and the
committed baseline is tested against the real tree so a file that quietly drops
its last `println!` is reported rather than left in the ledger.
"""
import contextlib
import io
import unittest
from unittest import mock

from tests.support import load_script_module, test_tmpdir
from pathlib import Path

log_surface = load_script_module("check_log_surface", "scripts/ci/check_log_surface.py")


def run() -> tuple[int, str]:
    captured = io.StringIO()
    with contextlib.redirect_stdout(captured), contextlib.redirect_stderr(captured):
        status = log_surface.main()
    return status, captured.getvalue()


def counted(source: str) -> int:
    masked = log_surface.mask_host_code(log_surface.mask_rust_noncode(source))
    return len(log_surface.RAW_RE.findall(masked))


class LogSurfaceCountingTests(unittest.TestCase):
    def test_a_bare_print_in_kernel_code_is_counted(self) -> None:
        self.assertEqual(counted("fn f() { println!(\"up\"); }\n"), 1)

    def test_a_log_call_is_not_a_print(self) -> None:
        self.assertEqual(
            counted('fn f() { log::info!("up"); warn!("down"); }\n'), 0)

    def test_a_print_named_in_a_message_or_example_is_not_counted(self) -> None:
        """Documentation examples and quoted text are not calls."""
        self.assertEqual(counted('/// ```\n/// println!("example");\n/// ```\n'), 0)
        self.assertEqual(counted('fn f() { info!("println! in a message"); }\n'), 0)
        self.assertEqual(counted('// println!("retired");\n'), 0)

    def test_a_print_in_a_host_test_module_is_not_counted(self) -> None:
        self.assertEqual(counted(
            '#[cfg(test)]\nmod tests {\n    fn t() { println!("noise"); }\n}\n'
            'fn real() { println!("counts"); }\n'), 1)
        self.assertEqual(counted(
            'mod tests { fn t() { println!("noise"); } }\n'), 0)

    def test_kernel_code_behind_a_test_named_attribute_is_counted(self) -> None:
        """`not(test)` and `cfg_attr(test, …)` guard kernel code, not host code."""
        self.assertEqual(counted(
            '#[cfg(not(test))]\nfn real() { println!("counts"); }\n'), 1)
        self.assertEqual(counted(
            '#[cfg_attr(test, allow(dead_code))]\nfn real() { println!("counts"); }\n'), 1)

    def test_a_test_guard_on_a_one_line_item_ends_at_its_semicolon(self) -> None:
        """`#[cfg(test)] use …;` must not swallow the kernel function after it."""
        self.assertEqual(counted(
            '#[cfg(test)]\nuse std::println;\n'
            'fn real() { println!("counts"); }\n'), 1)
        # A `;` inside a signature's brackets is not the end of the item.
        self.assertEqual(counted(
            '#[cfg(test)]\nfn t() -> [u8; 4] { println!("noise"); [0; 4] }\n'
            'fn real() { println!("counts"); }\n'), 1)

    def test_the_recursion_guard_on_a_nested_block_still_ends_the_block(self) -> None:
        source = ('#[cfg(test)]\nmod tests {\n'
                  '    fn t() { if true { println!("x"); } }\n}\n'
                  'fn after() { println!("y"); }\n')
        self.assertEqual(counted(source), 1)


class LogSurfaceBaselineTests(unittest.TestCase):
    def test_the_committed_baseline_matches_the_kernel_sources(self) -> None:
        status, output = run()
        self.assertEqual(status, 0, output)
        self.assertIn("baseline holds", output)

    def test_a_new_raw_print_fails_the_run(self) -> None:
        """A subsystem that starts writing a terminal is a review decision."""
        measured = log_surface.occurrences

        def widened(vendored):
            found = measured(vendored)
            found["kernel/src/entry.rs"] = found.get("kernel/src/entry.rs", 0) + 1
            return found

        with mock.patch.object(log_surface, "occurrences", widened):
            status, output = run()
        self.assertEqual(status, 1, output)
        self.assertIn("kernel/src/entry.rs", output)

    def test_an_unbaselined_file_names_the_alternatives(self) -> None:
        measured = log_surface.occurrences

        def new_offender(vendored):
            found = measured(vendored)
            found["kernel/src/pseudofs/newcomer.rs"] = 3
            return found

        with mock.patch.object(log_surface, "occurrences", new_offender):
            status, output = run()
        self.assertEqual(status, 1, output)
        self.assertIn("no baseline entry", output)
        self.assertIn("diagnostic_println!", output)

    def test_a_file_that_stopped_printing_is_reported(self) -> None:
        """The ledger shrinks or it is not a ratchet."""
        # `[allow]` is the committed file's last table, so adding a key to it is
        # all this needs: every real entry stays, and one invented file is
        # baselined above what the tree now holds.
        committed = Path(log_surface.BASELINE).read_text(encoding="utf-8")
        with test_tmpdir() as directory:
            baseline = Path(directory) / "log-surface.toml"
            baseline.write_text(
                committed + '"kernel/src/entry.rs" = 9\n', encoding="utf-8")
            with mock.patch.object(log_surface, "BASELINE", baseline):
                status, output = run()
        self.assertEqual(status, 0, output)
        self.assertIn("SHRUNK kernel/src/entry.rs", output)


if __name__ == "__main__":  # pragma: no cover
    unittest.main()
