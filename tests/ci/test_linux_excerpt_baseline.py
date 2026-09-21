"""The `NOTICE` layer promises that every verbatim-Linux number in it is output of
`scripts/ci/scan_linux_excerpts.py`, and that no `NOTICE` or table row cites a
range that `scripts/ci/audit_linux_range_cites.py` has not resolved.  A promise
about the last time someone ran a tool is worth no more than that run, so this
file re-runs both instruments over the repository and pins what they print.

A failure here means the prose and the tree disagree: Linux text was added or
removed without carrying `docs/upstream-provenance.md` and the affected `NOTICE`
files along with it, or a citation range drifted.  Reconcile the prose and this
baseline in the same commit, and say which numbers moved.

These are the only tests in `tests/ci` that read the whole Linux source tree --
one hash pass per threshold covers ~15M lines -- so they skip when no tree is
materialized, the same discovery rule and the same advisory standing the ABI
gate's citation check uses (`find_linux_tree`).
"""
from __future__ import annotations

import contextlib
import io
import unittest
from pathlib import Path

from tests.support import load_script_module, repo_root

scanner = load_script_module("scan_linux_excerpts", "scripts/ci/scan_linux_excerpts.py")
auditor = load_script_module("audit_linux_range_cites", "scripts/ci/audit_linux_range_cites.py")
gate = load_script_module("linux_abi_gate", "scripts/ci/linux_abi_gate.py")

SCOPES = ("crates/linux", "kernel/src", "crates/ax")
WINDOW = 14

# (fenced lines, fenced blocks, files holding one, lines outside fences, of those
# `code`, marked blocks, marker lines) -- the seven a scan's header line prints,
# for the scope and threshold named.
SCAN_BASELINE: dict[tuple[str, int], tuple[int, ...]] = {
    ("crates/linux", 40): (110, 49, 20, 25, 0, 18, 21),
    ("kernel/src", 40): (89, 43, 19, 132, 8, 0, 0),
    ("crates/ax", 40): (0, 0, 0, 15, 8, 0, 0),
    ("crates/linux", 25): (211, 59, 22, 41, 3, 20, 21),
    ("kernel/src", 25): (166, 53, 25, 302, 13, 0, 0),
    # `crates/ax` at 25 is 4 `nullfs.rs` lines plus 14 smoltcp RFC bit-ruler rows
    # that Linux headers reprint from the same IETF figures: read the `crates/ax`
    # section of `docs/upstream-provenance.md` before counting these as text.
    ("crates/ax", 25): (18, 15, 4, 28, 17, 0, 0),
}

# Cites the auditor reaches per scope, which is how many ranges sit next to their
# own quotation.
AUDIT_CHECKED = {"crates/linux": 55, "kernel/src": 17, "crates/ax": 0}

# Rows the auditor prints that were read against the reference tree and found
# innocent, for the two reasons `docs/upstream-provenance.md` states: a prose cite
# whose walk ran past a fence into the neighbouring cite's quotation, and a cite
# naming a callee body beside a call-site quote.
AUDIT_ALLOWLIST = {
    "crates/linux/mm/src/userfaultfd.rs:35",
    "crates/linux/net/src/lib.rs:332",
    "kernel/src/syscall/fs/io_uring.rs:773",
}


def totals(scope: Path, hashes: set[bytes], threshold: int) -> tuple[int, ...]:
    """The header line of a `scanner.scan()` run, as a tuple."""
    results, marks = scanner.scan(scope, hashes, threshold, False)
    fenced = sum(counter["fenced"] for counter in results.values())
    blocks = sum(counter["blocks"] for counter in results.values())
    files = sum(1 for counter in results.values() if counter["blocks"])
    outside = sum(counter[key] for counter in results.values()
                  for key in ("unfenced-doc", "line-comment", "code"))
    code = sum(counter["code"] for counter in results.values())
    return (fenced, blocks, files, outside, code, marks["marked_blocks"], marks["markers"])


class LinuxExcerptBaselineTests(unittest.TestCase):
    """The counts the `NOTICE` files quote, re-derived from the tree itself."""

    references: dict[int, set[bytes]] = {}
    tree: Path | None = None

    @classmethod
    def setUpClass(cls) -> None:
        cls.tree = gate.find_linux_tree()

    def setUp(self) -> None:
        if self.tree is None:
            self.skipTest("no materialized Linux reference tree")

    def reference(self, threshold: int) -> set[bytes]:
        if threshold not in type(self).references:
            hashes, _ = scanner.reference(self.tree, threshold, True)
            type(self).references[threshold] = hashes
        return type(self).references[threshold]

    def test_scanner_categories_are_the_ones_the_baseline_adds_up(self) -> None:
        """So a renamed counter key fails loudly instead of summing to a smaller number.

        `crates/linux` at 25 is the one run where every category has a member: at
        40 no match in that scope is Rust `code`, so an absent key would pass for
        the wrong reason.
        """
        results, marks = scanner.scan(repo_root() / "crates/linux", self.reference(25), 25, False)
        keys = set().union(*(set(counter) for counter in results.values()))
        self.assertEqual(keys, {"fenced", "unfenced-doc", "line-comment", "code",
                                "blocks", "marked_blocks"})
        self.assertEqual(set(marks), {"markers", "marked_blocks"})

    def test_scan_totals_match_the_documented_inventory(self) -> None:
        root = repo_root()
        for (scope, threshold), expected in SCAN_BASELINE.items():
            with self.subTest(scope=scope, threshold=threshold):
                got = totals(root / scope, self.reference(threshold), threshold)
                self.assertEqual(
                    got, expected,
                    f"{scope} at >={threshold} no longer prints {expected}: "
                    "docs/upstream-provenance.md, the NOTICE files, and this baseline "
                    "have to be reconciled with the tree",
                )

    def test_range_cites_print_no_row_outside_the_allowlist(self) -> None:
        root = repo_root()
        rows: set[str] = set()
        for scope in SCOPES:
            captured = io.StringIO()
            with contextlib.redirect_stdout(captured):
                checked, misses = auditor.audit(self.tree, root / scope, WINDOW, False)
            self.assertEqual(checked, AUDIT_CHECKED[scope],
                             f"{scope}: the number of cites reachable from a quotation moved")
            printed = [line[len("MISS "):].split(" cites ")[0].removeprefix(f"{root}/")
                       for line in captured.getvalue().splitlines() if line.startswith("MISS ")]
            self.assertEqual(len(printed), misses, f"{scope}: a printed row went uncounted")
            rows.update(printed)
        self.assertEqual(
            rows, AUDIT_ALLOWLIST,
            "a citation range drifted: fix the source, or add the row to "
            "AUDIT_ALLOWLIST with its reason written up in docs/upstream-provenance.md",
        )


if __name__ == "__main__":
    unittest.main()
