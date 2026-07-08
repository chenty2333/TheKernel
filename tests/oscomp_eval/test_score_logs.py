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
            run_dir = root / "run"

            result = score_logs(
                name="unit",
                run_dir=run_dir,
                rv_log=rv_log,
                judge_dir=judge_dir,
                group_libc_matrix=(("basic", "musl"),),
                fail_fast=True,
            )

            self.assertTrue((run_dir / "score.json").is_file())
            self.assertFalse((run_dir / "manifest.json").exists())
            self.assertFalse((run_dir / "artifact-index.json").exists())
            self.assertFalse((run_dir / "inputs").exists())
            self.assertFalse((run_dir / "report.md").exists())
            self.assertTrue((run_dir / "rv" / "judge-summary.json").is_file())
            self.assertEqual(result.score.non_ltp_score, 7.0)
            self.assertFalse(result.score.has_errors)
            self.assertEqual(result.status, "complete")
            score_json = json.loads((run_dir / "score.json").read_text())
            self.assertEqual(score_json["run"]["mode"], "score-logs")
            self.assertEqual(score_json["run"]["name"], "unit")
            self.assertEqual(score_json["run"]["arches"], ["rv"])
            self.assertEqual(
                score_json["group_totals"]["rv/basic-musl"]["json_path"],
                "rv/judges/basic-musl.json",
            )
            self.assertTrue((run_dir / "rv" / "marker-validation.json").is_file())
            self.assertTrue((run_dir / "rv" / "segments.jsonl").is_file())
            self.assertTrue((run_dir / "rv" / "segments" / "basic-musl.txt").is_file())
            self.assertTrue((run_dir / "rv" / "judges" / "basic-musl.json").is_file())

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
            self.assertFalse((root / "run" / "manifest.json").exists())
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
            self.assertFalse((run_dir / "artifact-index.json").exists())
            self.assertTrue((run_dir / "rv" / "judges" / "basic-musl.json").is_file())


if __name__ == "__main__":
    unittest.main()
