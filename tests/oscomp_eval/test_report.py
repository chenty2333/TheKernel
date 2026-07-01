from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

from tools.oscomp_eval.report import ReportError, generate_report, render_markdown


class ReportTests(unittest.TestCase):
    def test_render_markdown_contains_score_and_issues(self) -> None:
        manifest = {
            "name": "unit",
            "mode": "score-logs",
            "status": "incomplete",
            "created_at": "2026-07-01T00:00:00+00:00",
            "command": ["tools.oscomp_eval", "score-logs"],
            "expected_matrix": [
                {
                    "arch": "rv",
                    "group": "basic",
                    "libc": "musl",
                    "group_id": "basic-musl",
                    "key": "rv/basic-musl",
                },
                {
                    "arch": "rv",
                    "group": "basic",
                    "libc": "glibc",
                    "group_id": "basic-glibc",
                    "key": "rv/basic-glibc",
                },
                {
                    "arch": "rv",
                    "group": "lua",
                    "libc": "musl",
                    "group_id": "lua-musl",
                    "key": "rv/lua-musl",
                },
            ],
            "replays": [
                {
                    "arch": "rv",
                    "returncode": 0,
                    "duration_ms": 12,
                    "timed_out": False,
                    "log_path": "/abs/run/rv/console.log",
                    "log_relpath": "rv/console.log",
                }
            ],
            "git": {
                "commit": "abc123",
                "dirty": True,
                "status_short": [" M scripts/oscomp.sh"],
            },
            "official_snapshot": {
                "source": {
                    "repo": "https://example.invalid/autotest.git",
                    "commit": "official123",
                    "imported_at": "2026-07-01",
                }
            },
        }
        score = {
            "total_score": 12.5,
            "non_ltp_score": 10,
            "ltp_raw_total": 100,
            "ltp_score": 2.5,
            "arch_totals": {"rv": {"non_ltp_score": 10, "ltp_raw_total": 100}},
            "libc_totals": {
                "musl": {
                    "non_ltp_score": 10,
                    "ltp_raw_total": 100,
                    "ltp_score": 18.5,
                }
            },
            "ltp_group_totals": {
                "ltp-musl": {
                    "raw_score": 100,
                    "score_contribution": 18.5,
                }
            },
            "group_totals": {
                "rv/basic-musl": {
                    "status": "ok",
                    "row_count": 1,
                    "raw_score": 10,
                    "score_contribution": 10,
                    "json_path": "rv/judges/basic-musl.json",
                },
                "rv/basic-glibc": {
                    "status": "missing-segment",
                    "row_count": 0,
                    "raw_score": 0,
                    "score_contribution": 0,
                    "json_path": None,
                }
            },
            "issues": [
                {
                    "kind": "judge-status",
                    "arch": "rv",
                    "group_id": "basic-glibc",
                    "status": "missing-segment",
                }
            ],
        }

        markdown = render_markdown(manifest, score)

        self.assertIn("# Local OSComp Evaluation Report", markdown)
        self.assertIn("Status: `incomplete`", markdown)
        self.assertIn("Total score: `12.5`", markdown)
        self.assertIn("Git commit: `abc123`", markdown)
        self.assertIn("Official commit: `official123`", markdown)
        self.assertIn("| rv | 0 | 12 | False | False | rv/console.log |  |", markdown)
        self.assertIn("## Coverage Summary", markdown)
        self.assertIn("| rv | 3 | 1 | 1 | 1 |", markdown)
        self.assertIn("## Problem Expected Cells", markdown)
        self.assertIn("| rv | basic-glibc | missing-segment |  |", markdown)
        self.assertIn("| rv | lua-musl | unreported |  |", markdown)
        self.assertIn("## Libc Totals", markdown)
        self.assertIn("## LTP Contributions", markdown)
        self.assertIn("`judge-status` `rv/basic-glibc` status=missing-segment", markdown)

    def test_render_markdown_exposes_replay_errors(self) -> None:
        manifest = {
            "name": "unit-launch-error",
            "mode": "evaluate-replay",
            "status": "replay-error",
            "created_at": "2026-07-01T00:00:00+00:00",
            "replays": [
                {
                    "arch": "rv",
                    "returncode": 3,
                    "duration_ms": 4,
                    "timed_out": False,
                    "launch_failed": True,
                    "log_relpath": "rv/console.log",
                    "error": "replay launch failed: missing runner",
                }
            ],
        }
        score = {
            "total_score": 0,
            "non_ltp_score": 0,
            "ltp_raw_total": 0,
            "ltp_score": 0,
            "arch_totals": {},
            "group_totals": {},
            "issues": [
                {
                    "kind": "replay-status",
                    "arch": "rv",
                    "returncode": 3,
                    "log_path": "rv/console.log",
                    "error": "replay launch failed: missing runner",
                }
            ],
        }

        markdown = render_markdown(manifest, score)

        self.assertIn(
            "| rv | 3 | 4 | False | True | rv/console.log | replay launch failed: missing runner |",
            markdown,
        )
        self.assertIn(
            "`replay-status` `rv/<run>` returncode=3 log=rv/console.log error=replay launch failed: missing runner",
            markdown,
        )

    def test_generate_report_writes_markdown_only(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            run_dir = Path(tmp)
            (run_dir / "manifest.json").write_text(
                json.dumps(
                    {
                        "schema": "oscomp-eval.run-manifest.v1",
                        "name": "unit",
                        "mode": "score-logs",
                    }
                ),
                encoding="utf-8",
            )
            (run_dir / "score.json").write_text(
                json.dumps(
                    {
                        "schema": "oscomp-eval.score-summary.v1",
                        "total_score": 0,
                        "non_ltp_score": 0,
                        "ltp_raw_total": 0,
                        "ltp_score": 0,
                        "arch_totals": {},
                        "group_totals": {},
                        "issues": [],
                    }
                ),
                encoding="utf-8",
            )
            (run_dir / "report.html").write_text("stale html", encoding="utf-8")
            (run_dir / "rv").mkdir()
            (run_dir / "rv" / "marker-validation.json").write_text(
                json.dumps(
                    {
                        "arch": "rv",
                        "marker_count": 2,
                        "complete_group_count": 1,
                        "issues": [],
                        "log_events": [],
                    }
                ),
                encoding="utf-8",
            )
            (run_dir / "rv" / "judge-summary.json").write_text(
                json.dumps(
                    {
                        "arch": "rv",
                        "results": [
                            {
                                "arch": "rv",
                                "group_id": "basic-musl",
                                "status": "ok",
                                "stderr_path": "judges/basic-musl.stderr",
                            }
                        ],
                    }
                ),
                encoding="utf-8",
            )
            (run_dir / "rv" / "judges").mkdir()
            (run_dir / "rv" / "judges" / "basic-musl.stderr").write_text(
                "diagnostic from judge\n",
                encoding="utf-8",
            )

            result = generate_report(run_dir)

            self.assertEqual(result.issue_count, 0)
            self.assertTrue(result.markdown_path.is_file())
            self.assertTrue((run_dir / "artifact-index.json").is_file())
            self.assertFalse((run_dir / "report.html").exists())
            self.assertIn(
                "# Local OSComp Evaluation Report",
                result.markdown_path.read_text(),
            )
            report = result.markdown_path.read_text()
            self.assertIn("## Marker Summary", report)
            self.assertIn("diagnostic from judge", report)
            self.assertIn("- `artifact-index.json`", report)
            artifact_index = json.loads((run_dir / "artifact-index.json").read_text())
            self.assertEqual(
                artifact_index["schema"],
                "oscomp-eval.artifact-index.v1",
            )
            artifact_paths = {
                artifact["path"] for artifact in artifact_index["artifacts"]
            }
            self.assertIn("report.md", artifact_paths)
            self.assertIn("artifact-index.json", artifact_paths)
            self.assertIn("rv/marker-validation.json", artifact_paths)
            self.assertIn("rv/judge-summary.json", artifact_paths)
            self.assertIn("rv/judges/basic-musl.stderr", artifact_paths)
            index_artifact = next(
                artifact
                for artifact in artifact_index["artifacts"]
                if artifact["path"] == "artifact-index.json"
            )
            self.assertEqual(index_artifact["schema"], "oscomp-eval.artifact-index.v1")
            self.assertEqual(
                index_artifact["size_bytes"],
                (run_dir / "artifact-index.json").stat().st_size,
            )

    def test_generate_report_rejects_unsupported_schema(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            run_dir = Path(tmp)
            (run_dir / "manifest.json").write_text(
                json.dumps(
                    {
                        "schema": "oscomp-eval.run-manifest.v2",
                        "name": "unit",
                        "mode": "score-logs",
                    }
                ),
                encoding="utf-8",
            )
            (run_dir / "score.json").write_text(
                json.dumps(
                    {
                        "schema": "oscomp-eval.score-summary.v1",
                        "total_score": 0,
                        "non_ltp_score": 0,
                        "ltp_raw_total": 0,
                        "ltp_score": 0,
                        "arch_totals": {},
                        "group_totals": {},
                        "issues": [],
                    }
                ),
                encoding="utf-8",
            )

            with self.assertRaisesRegex(
                ReportError,
                "unsupported manifest.json schema: oscomp-eval.run-manifest.v2",
            ):
                generate_report(run_dir)

    def test_generate_report_rejects_missing_schema(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            run_dir = Path(tmp)
            (run_dir / "manifest.json").write_text(
                json.dumps({"name": "unit", "mode": "score-logs"}),
                encoding="utf-8",
            )
            (run_dir / "score.json").write_text(
                json.dumps(
                    {
                        "schema": "oscomp-eval.score-summary.v1",
                        "total_score": 0,
                        "non_ltp_score": 0,
                        "ltp_raw_total": 0,
                        "ltp_score": 0,
                        "arch_totals": {},
                        "group_totals": {},
                        "issues": [],
                    }
                ),
                encoding="utf-8",
            )

            with self.assertRaisesRegex(
                ReportError,
                "unsupported manifest.json schema: <missing>",
            ):
                generate_report(run_dir)


if __name__ == "__main__":
    unittest.main()
