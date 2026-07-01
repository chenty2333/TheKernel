from __future__ import annotations

import math
import unittest

from tools.oscomp_eval.scoring import (
    ltp_curve,
    normalize_row,
    row_score,
    score_judge_summaries,
)


def judge_summary(arch: str, results: list[dict[str, object]]) -> dict[str, object]:
    return {
        "schema": "oscomp-eval.judge-summary.v1",
        "arch": arch,
        "judge_dir": "judge",
        "results": results,
    }


def official_postwork_total(non_ltp: float, *ltp_raw_groups: float) -> float:
    total = non_ltp
    for ltp_raw in ltp_raw_groups:
        raw = max(0.0, min(ltp_raw, 10000.0))
        total += 500.0 * math.log10(1.0 + 9.0 * raw / 10000.0)
    return total


class ScoringTests(unittest.TestCase):
    def test_row_score_prefers_explicit_score(self) -> None:
        self.assertEqual(row_score({"name": "case", "score": "2.5"}), (2.5, None))
        self.assertEqual(
            row_score({"name": "case", "pass": 3})[0],
            3.0,
        )
        self.assertIn("no explicit score", row_score({"name": "case", "pass": 3})[1])

    def test_normalize_row_converts_known_numeric_fields(self) -> None:
        row = normalize_row(
            {
                "name": "case",
                "pass": "1",
                "all": "2",
                "result": "3.5",
                "res": "4",
                "baseline": "-",
                "score": "5",
            }
        )

        self.assertEqual(row["pass"], 1.0)
        self.assertEqual(row["all"], 2.0)
        self.assertEqual(row["result"], 3.5)
        self.assertEqual(row["res"], 4.0)
        self.assertEqual(row["baseline"], "-")
        self.assertEqual(row["score"], 5.0)

    def test_ltp_curve_matches_official_formula(self) -> None:
        self.assertEqual(ltp_curve(0), 0.0)
        self.assertAlmostEqual(ltp_curve(10000), 500.0)
        self.assertAlmostEqual(
            ltp_curve(1500),
            500.0 * math.log10(1.0 + 9.0 * 1500.0 / 10000.0),
        )

    def test_score_judge_summaries_keeps_ltp_raw_separate(self) -> None:
        summary = judge_summary(
            "rv",
            [
                {
                    "arch": "rv",
                    "group": "basic",
                    "libc": "musl",
                    "group_id": "basic-musl",
                    "status": "ok",
                    "rows": [
                        {"name": "a", "score": 1},
                        {"name": "b", "score": 2},
                    ],
                    "json_path": "judges/basic-musl.json",
                },
                {
                    "arch": "rv",
                    "group": "ltp",
                    "libc": "musl",
                    "group_id": "ltp-musl",
                    "status": "ok",
                    "rows": [
                        {"name": "ltp-a", "score": 1000},
                        {"name": "ltp-b", "score": 500},
                    ],
                    "json_path": "judges/ltp-musl.json",
                },
            ],
        )

        score = score_judge_summaries([summary])

        self.assertEqual(score.non_ltp_score, 3.0)
        self.assertEqual(score.ltp_raw_total, 1500.0)
        self.assertAlmostEqual(score.ltp_score, ltp_curve(1500))
        self.assertAlmostEqual(score.total_score, 3.0 + ltp_curve(1500))
        self.assertEqual(score.group_totals["rv/ltp-musl"]["score_contribution"], 0.0)
        self.assertEqual(
            score.group_totals["rv/basic-musl"]["json_path"],
            "rv/judges/basic-musl.json",
        )
        self.assertEqual(
            score.group_totals["rv/ltp-musl"]["json_path"],
            "rv/judges/ltp-musl.json",
        )
        self.assertEqual(score.libc_totals["musl"]["ltp_raw_total"], 1500.0)
        self.assertAlmostEqual(
            score.ltp_group_totals["ltp-musl"]["score_contribution"],
            ltp_curve(1500),
        )

    def test_score_group_json_path_is_run_relative_once(self) -> None:
        summary = judge_summary(
            "la",
            [
                {
                    "arch": "la",
                    "group": "basic",
                    "libc": "musl",
                    "group_id": "basic-musl",
                    "status": "ok",
                    "rows": [{"name": "case", "score": 1}],
                    "json_path": "la/judges/basic-musl.json",
                }
            ],
        )

        score = score_judge_summaries([summary])

        self.assertEqual(
            score.group_totals["la/basic-musl"]["json_path"],
            "la/judges/basic-musl.json",
        )

    def test_judge_failures_become_score_issues(self) -> None:
        summary = judge_summary(
            "la",
            [
                {
                    "arch": "la",
                    "group": "basic",
                    "libc": "glibc",
                    "group_id": "basic-glibc",
                    "status": "missing-segment",
                    "rows": [],
                }
            ],
        )

        score = score_judge_summaries([summary])

        self.assertTrue(score.has_errors)
        self.assertEqual(score.issues[0]["kind"], "judge-status")
        self.assertEqual(score.group_totals["la/basic-glibc"]["raw_score"], 0.0)

    def test_score_total_matches_official_postwork_formula_fixture(self) -> None:
        rv = judge_summary(
            "rv",
            [
                {
                    "arch": "rv",
                    "group": "basic",
                    "libc": "musl",
                    "group_id": "basic-musl",
                    "status": "ok",
                    "rows": [{"name": "rv-basic", "score": 4}],
                },
                {
                    "arch": "rv",
                    "group": "busybox",
                    "libc": "musl",
                    "group_id": "busybox-musl",
                    "status": "ok",
                    "rows": [{"name": "rv-busybox", "score": 2}],
                },
                {
                    "arch": "rv",
                    "group": "ltp",
                    "libc": "musl",
                    "group_id": "ltp-musl",
                    "status": "ok",
                    "rows": [{"name": "rv-ltp", "score": 9000}],
                },
                {
                    "arch": "rv",
                    "group": "ltp",
                    "libc": "glibc",
                    "group_id": "ltp-glibc",
                    "status": "ok",
                    "rows": [{"name": "rv-ltp-glibc", "score": 1000}],
                },
            ],
        )
        la = judge_summary(
            "la",
            [
                {
                    "arch": "la",
                    "group": "basic",
                    "libc": "musl",
                    "group_id": "basic-musl",
                    "status": "ok",
                    "rows": [{"name": "la-basic", "score": 6}],
                },
                {
                    "arch": "la",
                    "group": "ltp",
                    "libc": "musl",
                    "group_id": "ltp-musl",
                    "status": "ok",
                    "rows": [{"name": "la-ltp", "score": 3000}],
                },
            ],
        )

        score = score_judge_summaries([rv, la])

        self.assertEqual(score.non_ltp_score, 12.0)
        self.assertEqual(score.ltp_raw_total, 13000.0)
        self.assertAlmostEqual(score.ltp_score, 500.0 + ltp_curve(1000))
        self.assertAlmostEqual(
            score.total_score,
            official_postwork_total(12.0, 12000.0, 1000.0),
        )
        self.assertEqual(score.arch_totals["rv"]["ltp_raw_total"], 10000.0)
        self.assertEqual(score.arch_totals["la"]["ltp_raw_total"], 3000.0)
        self.assertEqual(score.libc_totals["musl"]["ltp_raw_total"], 12000.0)
        self.assertEqual(score.libc_totals["glibc"]["ltp_raw_total"], 1000.0)
        self.assertEqual(score.ltp_group_totals["ltp-musl"]["raw_score"], 12000.0)
        self.assertEqual(score.ltp_group_totals["ltp-musl"]["score_contribution"], 500.0)


if __name__ == "__main__":
    unittest.main()
