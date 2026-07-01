from __future__ import annotations

import json
import tempfile
import textwrap
import unittest
from pathlib import Path

from tools.oscomp_eval.score_logs import score_logs


class ScoreLogsTests(unittest.TestCase):
    def test_score_logs_writes_run_artifacts(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            judge_dir = root / "judge"
            judge_dir.mkdir()
            (judge_dir / "judge_basic-musl.py").write_text(
                textwrap.dedent(
                    """
                    import json
                    print(json.dumps([{"name": "case", "score": 7}]))
                    """
                ).lstrip(),
                encoding="utf-8",
            )
            rv_log = root / "rv.log"
            rv_log.write_text(
                "#### OS COMP TEST GROUP START basic-musl ####\n"
                "body\n"
                "#### OS COMP TEST GROUP END basic-musl ####\n"
                "shutdown\n",
                encoding="utf-8",
            )
            plan_path = root / "focused-plan.txt"
            plan_path.write_text("/musl basic\n", encoding="utf-8")
            run_dir = root / "run"

            result = score_logs(
                name="unit",
                run_dir=run_dir,
                rv_log=rv_log,
                judge_dir=judge_dir,
                group_libc_matrix=(("basic", "musl"),),
                plan_path=plan_path,
                fail_fast=True,
            )

            self.assertTrue((run_dir / "manifest.json").is_file())
            self.assertTrue((run_dir / "score.json").is_file())
            self.assertTrue((run_dir / "artifact-index.json").is_file())
            self.assertTrue((run_dir / "report.md").is_file())
            self.assertFalse((run_dir / "report.html").exists())
            self.assertTrue((run_dir / "rv" / "judge-summary.json").is_file())
            self.assertEqual(result.score.non_ltp_score, 7.0)
            self.assertFalse(result.score.has_errors)
            self.assertEqual(result.status, "complete")
            manifest = json.loads((run_dir / "manifest.json").read_text())
            self.assertEqual(manifest["schema"], "oscomp-eval.run-manifest.v1")
            self.assertEqual(manifest["mode"], "score-logs")
            self.assertEqual(manifest["status"], "complete")
            self.assertEqual(manifest["inputs"]["plan"], str(plan_path))
            self.assertEqual(manifest["inputs"]["captured_plan"], "inputs/plan.txt")
            self.assertEqual(
                (run_dir / "inputs" / "plan.txt").read_text(encoding="utf-8"),
                "/musl basic\n",
            )
            self.assertEqual(
                manifest["group_libc_matrix"],
                [{"group": "basic", "libc": "musl"}],
            )
            self.assertEqual(
                manifest["expected_matrix"],
                [
                    {
                        "arch": "rv",
                        "group": "basic",
                        "libc": "musl",
                        "group_id": "basic-musl",
                        "key": "rv/basic-musl",
                    }
                ],
            )
            self.assertIn("git", manifest)
            self.assertIn("official_snapshot", manifest)
            score_json = json.loads((run_dir / "score.json").read_text())
            self.assertEqual(
                score_json["group_totals"]["rv/basic-musl"]["json_path"],
                "rv/judges/basic-musl.json",
            )
            artifact_index = json.loads((run_dir / "artifact-index.json").read_text())
            self.assertEqual(
                artifact_index["schema"],
                "oscomp-eval.artifact-index.v1",
            )
            artifact_paths = {
                artifact["path"] for artifact in artifact_index["artifacts"]
            }
            self.assertIn("manifest.json", artifact_paths)
            self.assertIn("score.json", artifact_paths)
            self.assertIn("report.md", artifact_paths)
            self.assertIn("artifact-index.json", artifact_paths)
            self.assertIn("inputs/plan.txt", artifact_paths)
            self.assertIn("rv/marker-validation.json", artifact_paths)
            self.assertIn("rv/judge-summary.json", artifact_paths)
            self.assertIn("rv/segments/basic-musl.txt", artifact_paths)
            self.assertIn("rv/judges/basic-musl.json", artifact_paths)
            segments_jsonl_artifact = next(
                artifact
                for artifact in artifact_index["artifacts"]
                if artifact["path"] == "rv/segments.jsonl"
            )
            self.assertEqual(
                segments_jsonl_artifact["schema"],
                "oscomp-eval.segment-record.v1",
            )
            judge_json_artifact = next(
                artifact
                for artifact in artifact_index["artifacts"]
                if artifact["path"] == "rv/judges/basic-musl.json"
            )
            self.assertEqual(
                judge_json_artifact["schema"],
                "oscomp-eval.judge-result.v1",
            )
            index_artifact = next(
                artifact
                for artifact in artifact_index["artifacts"]
                if artifact["path"] == "artifact-index.json"
            )
            self.assertEqual(index_artifact["kind"], "artifact-index")
            self.assertEqual(index_artifact["schema"], "oscomp-eval.artifact-index.v1")
            self.assertEqual(
                index_artifact["size_bytes"],
                (run_dir / "artifact-index.json").stat().st_size,
            )

    def test_score_logs_promotes_marker_issues_to_score_issues(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            judge_dir = root / "judge"
            judge_dir.mkdir()
            (judge_dir / "judge_basic-musl.py").write_text(
                "import json\nprint(json.dumps([]))\n",
                encoding="utf-8",
            )
            rv_log = root / "rv.log"
            rv_log.write_text(
                "#### OS COMP TEST GROUP START basic-musl ####\n"
                "body without end\n",
                encoding="utf-8",
            )

            result = score_logs(
                name="unit-marker-issue",
                run_dir=root / "run",
                rv_log=rv_log,
                judge_dir=judge_dir,
                fail_fast=True,
            )

            kinds = [issue["kind"] for issue in result.score.issues]
            self.assertIn("marker-start-without-end", kinds)
            self.assertEqual(result.status, "incomplete")
            manifest = json.loads((root / "run" / "manifest.json").read_text())
            self.assertEqual(manifest["status"], "incomplete")
            self.assertEqual(len(manifest["group_libc_matrix"]), 21)
            self.assertEqual(len(manifest["expected_matrix"]), 21)
            self.assertIn(
                {
                    "arch": "rv",
                    "group": "ltp",
                    "libc": "glibc",
                    "group_id": "ltp-glibc",
                    "key": "rv/ltp-glibc",
                },
                manifest["expected_matrix"],
            )
            self.assertTrue((root / "run" / "rv" / "marker-validation.json").is_file())

    def test_replace_clears_stale_run_artifacts_only(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            judge_dir = root / "judge"
            judge_dir.mkdir()
            (judge_dir / "judge_basic-musl.py").write_text(
                'import json\nprint(json.dumps([{"name": "case", "score": 1}]))\n',
                encoding="utf-8",
            )
            rv_log = root / "rv.log"
            rv_log.write_text(
                "#### OS COMP TEST GROUP START basic-musl ####\n"
                "body\n"
                "#### OS COMP TEST GROUP END basic-musl ####\n"
                "shutdown\n",
                encoding="utf-8",
            )
            run_dir = root / "run"
            (run_dir / "la" / "judges").mkdir(parents=True)
            (run_dir / "la" / "judges" / "stale.json").write_text("stale\n", encoding="utf-8")
            (run_dir / "keep.txt").write_text("keep\n", encoding="utf-8")

            result = score_logs(
                name="unit-replace",
                run_dir=run_dir,
                rv_log=rv_log,
                judge_dir=judge_dir,
                group_libc_matrix=(("basic", "musl"),),
                replace=True,
            )

            self.assertEqual(result.status, "complete")
            self.assertFalse((run_dir / "la").exists())
            self.assertEqual((run_dir / "keep.txt").read_text(encoding="utf-8"), "keep\n")
            artifact_index = json.loads((run_dir / "artifact-index.json").read_text())
            artifact_paths = {
                artifact["path"] for artifact in artifact_index["artifacts"]
            }
            self.assertNotIn("la/judges/stale.json", artifact_paths)
            self.assertIn("rv/judges/basic-musl.json", artifact_paths)


if __name__ == "__main__":
    unittest.main()
