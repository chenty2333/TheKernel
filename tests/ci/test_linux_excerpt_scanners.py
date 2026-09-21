"""The two verbatim-Linux scanners define every count in the `NOTICE` layer, so
their rules are pinned here rather than re-derived by hand each time a number is
questioned.

The fixtures are a miniature Linux tree and a miniature Rust scope, which lets one
test state a rule and its exception in the same place: what counts as a fenced
block, what a quote gutter costs, and when a citation range is a lie.
"""
import unittest
from pathlib import Path

from tests.support import load_script_module, test_tmpdir

scanner = load_script_module("scan_linux_excerpts", "scripts/ci/scan_linux_excerpts.py")
auditor = load_script_module("audit_linux_range_cites", "scripts/ci/audit_linux_range_cites.py")

LONG = "if (unlikely(flags & ~(GRND_NONBLOCK | GRND_RANDOM | GRND_INSECURE))) {"
SHORT = "if (nr_args > IO_RINGFD_REG_MAX)"
RULE = "--------------- ----------------"
ENUM = "MPOL_WEIGHTED_INTERLEAVE,"
LINUX_LINES = (LONG, SHORT, RULE, ENUM)


def fixture(tmp: Path, rust: str) -> tuple[Path, Path]:
    linux = tmp / "linux"
    (linux / "mm").mkdir(parents=True)
    (linux / "mm" / "fake.c").write_text("\n".join(LINUX_LINES) + "\n", encoding="utf-8")
    scope = tmp / "scope"
    scope.mkdir()
    (scope / "lib.rs").write_text(rust, encoding="utf-8")
    return linux, scope


class ScanTests(unittest.TestCase):
    def counts(self, rust: str, *, minimum: int = 40, include_rules: bool = False):
        """`(per-file counts, scope marks)` for one Rust file against `fake.c`."""
        with test_tmpdir() as tmp:
            linux, scope = fixture(Path(tmp), rust)
            hashes, _ = scanner.reference(linux, 1, True)
            return scanner.scan(scope, hashes, minimum, include_rules)

    def test_a_fenced_quote_counts_its_lines_and_exactly_one_block(self):
        results, _ = self.counts(
            "/// ```c\n"
            f"///  {LONG}\n"
            "///  /* prose that matches no Linux line */\n"
            "/// ```\n"
        )
        self.assertEqual(dict(results["lib.rs"]), {"fenced": 1, "blocks": 1, "marked_blocks": 0})

    def test_the_fence_delimiter_is_never_itself_a_candidate(self):
        """The open line would match the ` ```c ` text below it if it were scanned."""
        results, _ = self.counts("/// ```c\n" + f"///  {LONG}\n" + "/// ```\n", minimum=1)
        self.assertEqual(results["lib.rs"]["fenced"], 1)

    def test_category_follows_where_the_line_sits(self):
        cases = {
            "unfenced-doc": f"///  {LONG}\n",
            "line-comment": f"//  {LONG}\n",
            "code": f"    {ENUM}\n",
        }
        for expected, rust in cases.items():
            with self.subTest(category=expected):
                results, _ = self.counts(rust, minimum=1)
                self.assertEqual(dict(results["lib.rs"]).get(expected), 1, f"{expected} misfiled")

    def test_a_quote_gutter_is_not_part_of_the_quoted_text(self):
        results, _ = self.counts(f"/// ```c\n///   │  {LONG}\n///   └── branch\n/// ```\n")
        self.assertEqual(results["lib.rs"]["fenced"], 1)

    def test_drawing_lines_need_the_explicit_flag(self):
        rust = f"/// ```c\n///  {RULE}\n/// ```\n"
        self.assertEqual(self.counts(rust, minimum=1)[0], {})
        with_rules, _ = self.counts(rust, minimum=1, include_rules=True)
        self.assertEqual(with_rules["lib.rs"]["fenced"], 1)

    def test_threshold_decides_whether_a_crate_looks_clean(self):
        rust = f"/// ```c\n///  {SHORT}\n/// ```\n"
        self.assertEqual(self.counts(rust)[0], {})
        self.assertEqual(self.counts(rust, minimum=25)[0]["lib.rs"]["fenced"], 1)

    def test_a_marker_is_reported_even_when_its_hunk_is_all_short_lines(self):
        marked = (
            "/// Excerpt: Linux v7.2.3 `mm/fake.c:1-1` — GPL-2.0-only\n"
            f"/// ```c\n///  {LONG}\n/// ```\n"
        )
        results, marks = self.counts(marked)
        self.assertEqual(results["lib.rs"]["marked_blocks"], 1)
        self.assertEqual(marks["markers"], 1)

        invisible = (
            "/// Excerpt: Linux v7.2.3 `mm/fake.c:3-3` — GPL-2.0-only\n"
            f"/// ```c\n///  {RULE}\n/// ```\n"
        )
        results, marks = self.counts(invisible)
        self.assertEqual(results, {}, "a hunk of drawing lines is not an excerpt")
        self.assertEqual((marks["markers"], marks["marked_blocks"]), (1, 0))


class RangeCiteAuditTests(unittest.TestCase):
    def audit(self, rust: str):
        with test_tmpdir() as tmp:
            linux, scope = fixture(Path(tmp), rust)
            return auditor.audit(linux, scope, 14, True)

    def test_a_range_over_its_own_hunk_passes(self):
        rust = f"/// Quoted from `mm/fake.c:1-1`:\n/// ```c\n///  {LONG}\n/// ```\n"
        self.assertEqual(self.audit(rust), (1, 0))

    def test_a_range_that_misses_the_hunk_next_to_it_is_a_miss(self):
        rust = f"/// Quoted from `mm/fake.c:3-3`:\n/// ```c\n///  {LONG}\n/// ```\n"
        self.assertEqual(self.audit(rust), (1, 1))

    def test_a_cite_next_to_a_paraphrase_is_not_checkable(self):
        """A range attached to prose has nothing to compare, so it is not a row."""
        self.assertEqual(self.audit("/// The gate mirrors `mm/fake.c:9-9` in spirit.\n"), (0, 0))

    def test_a_bare_second_range_inherits_the_file_the_previous_cite_named(self):
        rust = f"/// From `mm/fake.c:1-1`, and again at `:1-1`.\n/// ```c\n///  {LONG}\n/// ```\n"
        self.assertEqual(self.audit(rust), (2, 0))


if __name__ == "__main__":
    unittest.main()
