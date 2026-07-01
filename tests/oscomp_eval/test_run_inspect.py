from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

from tools.oscomp_eval.artifact_index import write_artifact_index
from tools.oscomp_eval.run_inspect import inspect_run


def write_minimal_run(run_dir: Path, *, status: str = "complete", issues: list[object] | None = None) -> None:
    run_dir.mkdir(parents=True, exist_ok=True)
    (run_dir / "manifest.json").write_text(
        json.dumps(
            {
                "schema": "oscomp-eval.run-manifest.v1",
                "name": "unit",
                "mode": "score-logs",
                "status": status,
            }
        )
        + "\n",
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
                "issues": issues or [],
            }
        )
        + "\n",
        encoding="utf-8",
    )
    (run_dir / "report.md").write_text("# report\n", encoding="utf-8")
    write_artifact_index(run_dir)


def write_run_with_nested_artifacts(run_dir: Path) -> None:
    write_minimal_run(run_dir)
    rv_dir = run_dir / "rv"
    rv_dir.mkdir()
    (rv_dir / "marker-validation.json").write_text(
        json.dumps(
            {
                "schema": "oscomp-eval.marker-artifacts.v1",
                "arch": "rv",
                "marker_count": 2,
                "complete_group_count": 1,
                "issues": [],
                "log_events": [],
            }
        )
        + "\n",
        encoding="utf-8",
    )
    (rv_dir / "segments.jsonl").write_text(
        json.dumps(
            {
                "schema": "oscomp-eval.segment-record.v1",
                "arch": "rv",
                "group": "basic-musl",
            }
        )
        + "\n",
        encoding="utf-8",
    )
    (rv_dir / "judge-summary.json").write_text(
        json.dumps(
            {
                "schema": "oscomp-eval.judge-summary.v1",
                "arch": "rv",
                "results": [],
            }
        )
        + "\n",
        encoding="utf-8",
    )
    judges_dir = rv_dir / "judges"
    judges_dir.mkdir()
    (judges_dir / "basic-musl.json").write_text(
        json.dumps(
            {
                "schema": "oscomp-eval.judge-result.v1",
                "arch": "rv",
                "group_id": "basic-musl",
                "status": "ok",
                "rows": [],
            }
        )
        + "\n",
        encoding="utf-8",
    )
    write_artifact_index(run_dir)


class RunInspectTests(unittest.TestCase):
    def test_inspect_complete_run_is_ok(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            run_dir = Path(tmp)
            write_minimal_run(run_dir)

            result = inspect_run(run_dir)

            self.assertTrue(result.ok)
            self.assertEqual(result.run_status, "complete")
            self.assertEqual(result.structural_issues, ())
            self.assertEqual(result.score_issue_count, 0)
            self.assertGreaterEqual(result.artifact_count, 4)
            data = result.to_json_dict()
            self.assertEqual(data["schema"], "oscomp-eval.run-inspection.v1")
            self.assertTrue(data["ok"])
            self.assertEqual(data["status"], "complete")
            self.assertEqual(data["structural_issue_count"], 0)
            self.assertEqual(data["score_issue_count"], 0)

    def test_inspect_reports_score_issues(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            run_dir = Path(tmp)
            write_minimal_run(
                run_dir,
                status="incomplete",
                issues=[{"kind": "judge-status"}],
            )

            result = inspect_run(run_dir)

            self.assertFalse(result.ok)
            self.assertEqual(result.run_status, "incomplete")
            self.assertEqual(result.score_issue_count, 1)
            self.assertEqual(result.structural_issues, ())

    def test_inspect_reports_index_mismatch(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            run_dir = Path(tmp)
            write_minimal_run(run_dir)
            (run_dir / "report.md").write_text("# report changed\n", encoding="utf-8")

            result = inspect_run(run_dir)

            self.assertFalse(result.ok)
            self.assertTrue(
                any("artifact size mismatch: report.md" in issue for issue in result.structural_issues)
            )

    def test_inspect_rejects_stale_html_report(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            run_dir = Path(tmp)
            write_minimal_run(run_dir)
            (run_dir / "report.html").write_text("stale html\n", encoding="utf-8")

            result = inspect_run(run_dir)

            self.assertFalse(result.ok)
            self.assertIn(
                "report.html is stale; report.md is the only supported human-readable report",
                result.structural_issues,
            )

    def test_inspect_rejects_html_report_in_artifact_index(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            run_dir = Path(tmp)
            write_minimal_run(run_dir)
            index = json.loads((run_dir / "artifact-index.json").read_text())
            index["artifacts"].append(
                {
                    "path": "report.html",
                    "kind": "html-report",
                    "size_bytes": 0,
                }
            )
            index["artifact_count"] = len(index["artifacts"])
            (run_dir / "artifact-index.json").write_text(
                json.dumps(index) + "\n",
                encoding="utf-8",
            )

            result = inspect_run(run_dir)

            self.assertFalse(result.ok)
            self.assertIn(
                "artifact-index contains unsupported HTML report: report.html",
                result.structural_issues,
            )

    def test_inspect_accepts_nested_artifact_schemas(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            run_dir = Path(tmp)
            write_run_with_nested_artifacts(run_dir)

            result = inspect_run(run_dir)

            self.assertTrue(result.ok)
            self.assertEqual(result.structural_issues, ())

    def test_inspect_reports_nested_artifact_schema_mismatch(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            run_dir = Path(tmp)
            write_run_with_nested_artifacts(run_dir)
            index = json.loads((run_dir / "artifact-index.json").read_text())
            for artifact in index["artifacts"]:
                if artifact["path"] == "rv/marker-validation.json":
                    artifact["schema"] = "oscomp-eval.marker-artifacts.v2"
                if artifact["path"] == "rv/judges/basic-musl.json":
                    artifact.pop("schema", None)
            (run_dir / "artifact-index.json").write_text(
                json.dumps(index) + "\n",
                encoding="utf-8",
            )

            result = inspect_run(run_dir)

            self.assertFalse(result.ok)
            self.assertIn(
                "artifact-index schema rv/marker-validation.json "
                "oscomp-eval.marker-artifacts.v2 != oscomp-eval.marker-artifacts.v1",
                result.structural_issues,
            )
            self.assertIn(
                "artifact-index schema rv/judges/basic-musl.json <missing> "
                "!= oscomp-eval.judge-result.v1",
                result.structural_issues,
            )

    def test_inspect_reports_actual_artifact_file_schema_mismatch(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            run_dir = Path(tmp)
            write_run_with_nested_artifacts(run_dir)
            (run_dir / "rv" / "marker-validation.json").write_text(
                json.dumps(
                    {
                        "schema": "oscomp-eval.marker-artifacts.v2",
                        "arch": "rv",
                        "marker_count": 2,
                        "complete_group_count": 1,
                        "issues": [],
                        "log_events": [],
                    }
                )
                + "\n",
                encoding="utf-8",
            )

            result = inspect_run(run_dir)

            self.assertFalse(result.ok)
            self.assertIn(
                "artifact file schema rv/marker-validation.json "
                "oscomp-eval.marker-artifacts.v2 != oscomp-eval.marker-artifacts.v1",
                result.structural_issues,
            )


if __name__ == "__main__":
    unittest.main()
