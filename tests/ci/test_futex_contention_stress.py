"""Unit tests for the Futex contention & multi-core stress driver."""

from __future__ import annotations

import math
import unittest
from fractions import Fraction

from tools import futex_contention_stress as stress
from tools.product_state import ProductError


def exact_cdf(k: int, n: int, p: float) -> float:
    q = Fraction(p).limit_denominator(10**12)
    total = Fraction(0)
    for i in range(k + 1):
        total += math.comb(n, i) * q**i * (1 - q) ** (n - i)
    return float(total)


def summary_line(variant: str, rounds: int = 1000, **overrides) -> str:
    counters = dict.fromkeys(stress.COUNTER_FIELDS, 0)
    counters["rounds"] = rounds
    counters["completed"] = rounds
    counters.update(overrides)
    body = " ".join(f"{field}={counters[field]}" for field in stress.COUNTER_FIELDS)
    return f"FUTEX-STRESS variant={variant} {body}"


class FutexBinomialTests(unittest.TestCase):
    def test_log_space_cdf_matches_exact_rational_arithmetic(self):
        for k, n, p in ((0, 40, 0.05), (1, 25, 0.1), (3, 16, 0.4), (2, 7, 0.75)):
            self.assertAlmostEqual(stress._binomial_cdf(k, n, p), exact_cdf(k, n, p), places=12)

    def test_upper_bound_at_zero_defects_is_the_rule_of_three(self):
        self.assertAlmostEqual(stress.upper_bound(0, 100000), 2.9957e-5, places=9)
        self.assertAlmostEqual(stress.upper_bound(0, 100000), 3 / 100000, places=6)

    def test_upper_bound_solves_the_defining_equation(self):
        for k, n in ((1, 1000), (3, 16), (7, 250)):
            bound = stress.upper_bound(k, n)
            self.assertAlmostEqual(stress._binomial_cdf(k, n, bound), 0.05, places=9)
            self.assertLess(stress._binomial_cdf(k, n, bound * 1.001), 0.05)
            self.assertGreater(stress._binomial_cdf(k, n, bound * 0.999), 0.05)

    def test_upper_bound_is_monotone_in_defects(self):
        bounds = [stress.upper_bound(k, 1000) for k in range(5)]
        self.assertEqual(bounds, sorted(bounds))


class FutexConsoleParserTests(unittest.TestCase):
    def test_clean_run_parses_successfully(self):
        output = "\n".join([
            summary_line("contention", 4000),
            "FUTEX-STRESS-OK contention",
            summary_line("requeue_pi", 4000),
            "FUTEX-STRESS-OK requeue_pi",
            stress.BOOT_MARKER,
        ])
        report = stress.parse_console_output(output)
        self.assertTrue(report.passed)
        self.assertFalse(report.unusable)
        self.assertFalse(report.timed_out)
        self.assertEqual(len(report.outcomes), 2)
        self.assertEqual(report.outcomes["contention"].rounds, 4000)
        self.assertEqual(report.outcomes["contention"].defects, 0)
        self.assertEqual(report.outcomes["requeue_pi"].rounds, 4000)
        self.assertEqual(report.outcomes["requeue_pi"].defects, 0)

    def test_defect_counter_and_first_fail_parsed(self):
        output = "\n".join([
            summary_line("requeue_pi", 4000, lost_wakeup=1),
            "FUTEX-STRESS-FIRST-FAIL variant=requeue_pi round=127 kind=lost_wakeup errno=-11 data=0",
            "FUTEX-STRESS-FAIL requeue_pi",
            stress.BOOT_MARKER,
        ])
        report = stress.parse_console_output(output)
        self.assertFalse(report.passed)
        outcome = report.outcomes["requeue_pi"]
        self.assertEqual(outcome.defects, 1)
        self.assertIsNotNone(outcome.first_fail)
        self.assertIn("round=127", outcome.first_fail)
        self.assertIn("kind=lost_wakeup", outcome.first_fail)

    def test_timeout_detected(self):
        output = "\n".join([
            summary_line("broadcast", 500),
            "FUTEX-STRESS-TIMEOUT",
        ])
        report = stress.parse_console_output(output)
        self.assertTrue(report.timed_out)
        self.assertFalse(report.passed)

    def test_empty_output_is_unusable(self):
        report = stress.parse_console_output("random console noise with no markers\n")
        self.assertTrue(report.unusable)
        self.assertFalse(report.passed)


class FutexVariantValidationTests(unittest.TestCase):
    def test_split_variants_all(self):
        self.assertEqual(stress.split_variants("all"), stress.VARIANTS)
        self.assertEqual(stress.split_variants(""), stress.VARIANTS)

    def test_split_variants_subset(self):
        self.assertEqual(
            stress.split_variants("contention,owner_death"),
            ("contention", "owner_death"),
        )

    def test_split_variants_unknown_rejected(self):
        with self.assertRaises(ProductError):
            stress.split_variants("contention,invalid_variant")


if __name__ == "__main__":
    unittest.main()
