"""Unit tests for the SysV SHM exit-retirement stress driver.

The driver's arithmetic is the part a reader has to trust, so the tests pin
the exact binomial CDF against integer arithmetic, pin the Clopper-Pearson
bound against that CDF, and pin the console parser against the stress
program's real output format (including the ways a boot can fail to be
evidence).
"""

from __future__ import annotations

import math
import re
import unittest
from fractions import Fraction
from pathlib import Path

from tools import sysv_shm_stress as stress
from tools.product_state import ProductError


def summary(variant, rounds=100, **overrides):
    counters = dict.fromkeys(stress.COUNTER_FIELDS, 0)
    counters["rounds"] = rounds
    counters["completed"] = rounds
    counters.update(overrides)
    body = " ".join(f"{field}={counters[field]}" for field in stress.COUNTER_FIELDS)
    return f"SYSV-SHM-STRESS variant={variant} {body}"


def exact_cdf(k: int, n: int, p: float) -> float:
    """P(X <= k) in exact rational arithmetic, as an independent reference."""

    q = Fraction(p).limit_denominator(10**12)
    total = Fraction(0)
    for i in range(k + 1):
        total += math.comb(n, i) * q**i * (1 - q) ** (n - i)
    return float(total)


class BinomialTests(unittest.TestCase):
    def test_log_space_cdf_matches_exact_rational_arithmetic(self):
        for k, n, p in ((0, 40, 0.05), (1, 25, 0.1), (3, 16, 0.4), (2, 7, 0.75)):
            self.assertAlmostEqual(stress._binomial_cdf(k, n, p), exact_cdf(k, n, p), places=12)

    def test_upper_bound_at_zero_defects_is_the_rule_of_three(self):
        self.assertAlmostEqual(stress.upper_bound(0, 100000), 2.9957e-5, places=9)
        self.assertAlmostEqual(stress.upper_bound(0, 100000), 3 / 100000, places=6)
        # The brief's motivating example: "0 in 6" bounds a flake at ~39%.
        self.assertAlmostEqual(stress.upper_bound(0, 6), 0.3930, places=4)
        self.assertAlmostEqual(stress.upper_bound(0, 12), 0.2209, places=4)

    def test_upper_bound_solves_the_defining_equation(self):
        for k, n in ((1, 1000), (3, 16), (7, 250)):
            bound = stress.upper_bound(k, n)
            self.assertAlmostEqual(stress._binomial_cdf(k, n, bound), 0.05, places=9)
            # And it is the boundary of the accepted region: the next rate up
            # rejects, the next rate down does not.
            self.assertLess(stress._binomial_cdf(k, n, bound * 1.001), 0.05)
            self.assertGreater(stress._binomial_cdf(k, n, bound * 0.999), 0.05)

    def test_upper_bound_is_monotone_in_defects(self):
        bounds = [stress.upper_bound(k, 1000) for k in range(5)]
        self.assertEqual(bounds, sorted(bounds))


class ParserTests(unittest.TestCase):
    def test_clean_boot_is_valid(self):
        text = "\n".join(
            [summary("wait", 4000), "SYSV-SHM-STRESS-OK wait", stress.BOOT_MARKER]
        )
        parsed = stress.parse_boot_text(text, ["wait"], 4000)
        self.assertTrue(parsed.valid, parsed.problems)
        self.assertEqual(parsed.rounds, 4000)
        self.assertEqual(parsed.defects, 0)
        self.assertTrue(parsed.marker)

    def test_missing_marker_invalidates_the_boot(self):
        parsed = stress.parse_boot_text(summary("wait"), ["wait"], 100)
        self.assertFalse(parsed.valid)
        self.assertIn("completion marker", " ".join(parsed.problems))

    def test_missing_variant_is_reported(self):
        text = "\n".join([summary("wait"), "SYSV-SHM-STRESS-OK wait", stress.BOOT_MARKER])
        parsed = stress.parse_boot_text(text, ["wait", "pipe"], 100)
        self.assertEqual(parsed.missing, ("pipe",))
        self.assertFalse(parsed.valid)

    def test_short_round_count_is_not_evidence(self):
        """An alarm that cut the loop short must not count as a pass."""

        text = "\n".join(
            [summary("wait", 900), stress.BOOT_MARKER]
        )
        parsed = stress.parse_boot_text(text, ["wait"], 4000)
        self.assertFalse(parsed.valid)
        self.assertIn("ran 900 rounds", " ".join(parsed.problems))

    def test_timeout_marker_is_reported(self):
        text = "\n".join([stress.BOOT_MARKER, "SYSV-SHM-STRESS-TIMEOUT"])
        parsed = stress.parse_boot_text(text, ["shared"], 500)
        self.assertTrue(parsed.timed_out)
        self.assertFalse(parsed.valid)

    def test_round_count_can_be_left_to_the_log(self):
        """Re-analysis of stored runs trusts each summary's own denominator."""

        text = "\n".join([summary("wait", 900), "SYSV-SHM-STRESS-OK wait", stress.BOOT_MARKER])
        parsed = stress.parse_boot_text(text, ["wait"], None)
        self.assertTrue(parsed.valid, parsed.problems)
        self.assertEqual(parsed.rounds, 900)

    def test_the_requested_variants_are_recoverable_from_the_command_line(self):
        """A stored boot's commands file says which variants it asked for."""

        command = stress.guest_command(["wait", "shared"], 100, 300)
        self.assertEqual(stress.COMMAND_VARIANT_RE.findall(command), ["wait", "shared"])

    def test_defect_counters_and_first_fail_are_summed(self):
        text = "\n".join(
            [
                summary("multi", 250, attach_fail=2, child_fail=1),
                "SYSV-SHM-STRESS-FAIL multi",
                "SYSV-SHM-STRESS-FIRST-FAIL variant=multi round=41 "
                "kind=removed-id-still-attachable errno=0 data=0",
                stress.BOOT_MARKER,
            ]
        )
        parsed = stress.parse_boot_text(text, ["multi"], 250)
        invocation = parsed.invocations[0]
        self.assertEqual(invocation.defects, 3)
        self.assertFalse(invocation.ok)
        self.assertIn("round=41", invocation.first_fail)
        self.assertIn("round=41", " ".join(parsed.defect_notes))

    def test_a_defect_is_a_result_and_keeps_the_boot_in_the_pool(self):
        """A failing round is the finding, not a reason to discard the boot."""

        text = "\n".join(
            [summary("wait", 100, attach_fail=1), "SYSV-SHM-STRESS-FAIL wait", stress.BOOT_MARKER]
        )
        parsed = stress.parse_boot_text(text, ["wait"], 100)
        self.assertTrue(parsed.valid, parsed.problems)
        self.assertEqual(parsed.defects, 1)
        self.assertEqual(len(parsed.failed_invocations), 1)
        reported = stress.render_report(
            [stress.BootRun("a", 1, 1, Path("/log"), None, parsed)], 1, "heading"
        )
        self.assertIn("DEFECTS OBSERVED", reported)

    def test_a_failing_variant_does_not_hide_the_others(self):
        """The real shape of the observed anomaly: one variant fails, so the
        guest shell never prints the marker, but every variant did report."""

        lines = [summary("wait", 100), summary("pipe", 100)]
        lines.append(summary("shared", 100, child_fail=1))
        lines.append("SYSV-SHM-STRESS-OK wait")
        lines.append("SYSV-SHM-STRESS-OK pipe")
        lines.append("SYSV-SHM-STRESS-FAIL shared")
        parsed = stress.parse_boot_text("\n".join(lines), ["wait", "pipe", "shared"], 100)
        self.assertTrue(parsed.valid, parsed.problems)
        self.assertFalse(parsed.marker)
        self.assertEqual(parsed.defects, 1)
        self.assertEqual([item.variant for item in parsed.failed_invocations], ["shared"])
        totals = stress.totals_for([stress.BootRun("a", 1, 1, None, None, parsed)])
        self.assertEqual(totals["wait"].rounds, 100)
        self.assertEqual(totals["shared"].defects, 1)

    def test_older_summary_format_still_parses(self):
        """A log written before `grandchild_fail` existed is still evidence."""

        line = (
            "SYSV-SHM-STRESS variant=shared rounds=100 completed=100 setup_fail=0 "
            "child_fail=0 value_fail=0 attach_fail=0 errno_fail=0 "
            "retire_early_fail=0 alive_probe=0"
        )
        text = "\n".join([line, "SYSV-SHM-STRESS-OK shared", stress.BOOT_MARKER])
        parsed = stress.parse_boot_text(text, ["shared"], 100)
        self.assertTrue(parsed.valid, parsed.problems)
        self.assertEqual(parsed.invocations[0].counters["grandchild_fail"], 0)

    def test_alive_probe_is_descriptive_not_a_defect(self):
        """`nowait` legitimately probes while the child still owns the segment."""

        text = "\n".join(
            [summary("nowait", 500, alive_probe=498), "SYSV-SHM-STRESS-OK nowait", stress.BOOT_MARKER]
        )
        parsed = stress.parse_boot_text(text, ["nowait"], 500)
        self.assertTrue(parsed.valid, parsed.problems)
        self.assertEqual(parsed.defects, 0)
        self.assertEqual(parsed.invocations[0].alive_probe, 498)


class AggregationTests(unittest.TestCase):
    def boot(self, name, *lines, planned=None, rounds=100, rc=0):
        variants = [
            match.group(1)
            for match in (
                re.match(r"SYSV-SHM-STRESS variant=(\w+) ", line) for line in lines
            )
            if match
        ]
        # The real program always prints its verdict after the summary line.
        verdicts = [f"SYSV-SHM-STRESS-OK {variant}" for variant in variants]
        text = "\n".join([*lines, *verdicts, stress.BOOT_MARKER])
        parsed = stress.parse_boot_text(text, planned or ["wait", "pipe"], rounds)
        return stress.BootRun(name, 1, rc, Path("/nonexistent"), None, parsed)

    def test_totals_pool_only_what_the_logs_contain(self):
        first = self.boot("a", summary("wait"), summary("pipe"))
        second = self.boot("b", summary("wait"), summary("pipe"))
        totals = stress.totals_for([first, second])
        self.assertEqual(totals["wait"].boots, 2)
        self.assertEqual(totals["wait"].invocations, 2)
        self.assertEqual(totals["wait"].rounds, 200)
        self.assertEqual(totals["wait"].defects, 0)
        self.assertAlmostEqual(totals["wait"].upper, stress.upper_bound(0, 200))

    def test_invalid_boots_are_excluded_from_pooling(self):
        good = self.boot("a", summary("wait"), planned=["wait"])
        bad = self.boot("b", summary("wait", 100), planned=["wait"], rounds=101)
        self.assertFalse(bad.valid)
        totals = stress.totals_for([good])
        self.assertEqual(totals["wait"].rounds, 100)
        self.assertNotIn("pipe", totals)

    def test_report_marks_an_unusable_boot(self):
        bad = self.boot("b", summary("wait"), planned=["wait", "pipe"])
        report = stress.render_report([bad], 1, "heading")
        self.assertIn("boots valid: 0 of 1", report)
        self.assertIn("UNUSABLE", report)
        self.assertIn("no summary for pipe", report)

    def test_report_headline_numbers(self):
        runs = [self.boot("a", summary("wait", 4000), planned=["wait"], rounds=4000)]
        report = stress.render_report(runs, 1, "heading")
        self.assertIn("0/4000", report)
        self.assertIn("one-sided 95% upper bound", report)


class CommandTests(unittest.TestCase):
    def test_guest_command_runs_every_variant_and_gates_on_all_of_them(self):
        command = stress.guest_command(["wait", "pipe"], 4000, 300)
        self.assertEqual(
            command,
            "ok=0 ; "
            "/opt/thekernel-tests/portable/sysv-shm-exit-stress wait 4000 300 || ok=1 ; "
            "/opt/thekernel-tests/portable/sysv-shm-exit-stress pipe 4000 300 || ok=1 ; "
            "test $ok -eq 0",
        )
        # A failing variant must not stop the later ones: the last variant's
        # result is not allowed to depend on the first variant passing.
        self.assertNotIn("&&", command)

    def test_heavy_command_keeps_the_inner_command_as_one_argument(self):
        wrapped = stress.heavy_command(["python3", "-c", "a b"], Path("/h.sh"))
        self.assertEqual(wrapped[0], "/h.sh")
        self.assertEqual(len(wrapped), 2)
        self.assertIn("'a b'", wrapped[1])

    def test_split_variants_expands_all_and_rejects_typos(self):
        self.assertEqual(stress.split_variants("all"), stress.VARIANTS)
        self.assertEqual(stress.split_variants("wait,pipe"), ("wait", "pipe"))
        with self.assertRaises(ProductError):
            stress.split_variants("wait,nope")


if __name__ == "__main__":
    unittest.main()
