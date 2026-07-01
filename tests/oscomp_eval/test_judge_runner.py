from __future__ import annotations

import json
import tempfile
import textwrap
import unittest
from pathlib import Path

from tools.oscomp_eval.config import MatrixCell
from tools.oscomp_eval.judge_runner import discover_judges, run_judges
from tools.oscomp_eval.markers import parse_text


def write_fake_judge(path: Path, source: str) -> None:
    path.write_text(textwrap.dedent(source).lstrip(), encoding="utf-8")


class JudgeRunnerTests(unittest.TestCase):
    def test_discover_judges_by_group_id(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            judge_dir = Path(tmp)
            write_fake_judge(judge_dir / "judge_basic-musl.py", "print('[]')\n")
            judges = discover_judges(judge_dir)
            self.assertEqual(set(judges), {"basic-musl"})

    def test_run_successful_judge_captures_outputs_and_rows(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            judge_dir = root / "judge"
            out_dir = root / "out"
            judge_dir.mkdir()
            write_fake_judge(
                judge_dir / "judge_basic-musl.py",
                """
                import json
                import sys
                sys.stderr.write("diagnostic\\n")
                body = sys.stdin.read()
                print(json.dumps([{"name": "basic", "score": 1, "body": body.strip()}]))
                """,
            )
            marker_result = parse_text(
                "#### OS COMP TEST GROUP START basic-musl ####\n"
                "hello\n"
                "#### OS COMP TEST GROUP END basic-musl ####\n",
                arch="rv",
            )

            summary = run_judges(
                marker_result,
                out_dir=out_dir,
                judge_dir=judge_dir,
                fail_fast=True,
                expected_cells=(MatrixCell(arch="rv", group="basic", libc="musl"),),
            )

            self.assertFalse(summary.has_errors)
            result = summary.results[0]
            self.assertEqual(result.status, "ok")
            self.assertEqual(result.rows[0]["body"], "hello")
            self.assertEqual((out_dir / result.stderr_path).read_text(), "diagnostic\n")
            judge_json = json.loads((out_dir / result.json_path).read_text())
            self.assertEqual(judge_json["schema"], "oscomp-eval.judge-result.v1")
            self.assertEqual(judge_json["status"], "ok")
            self.assertEqual(judge_json["stdout_path"], result.stdout_path)
            self.assertEqual(judge_json["stderr_path"], result.stderr_path)
            self.assertEqual(judge_json["rows"][0]["body"], "hello")
            self.assertEqual(json.loads((out_dir / "judge-summary.json").read_text())["ok_count"], 1)

    def test_missing_segment_is_structured(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            judge_dir = root / "judge"
            judge_dir.mkdir()
            write_fake_judge(judge_dir / "judge_basic-musl.py", "print('[]')\n")
            marker_result = parse_text("", arch="rv")

            summary = run_judges(
                marker_result,
                out_dir=root / "out",
                judge_dir=judge_dir,
                fail_fast=True,
                expected_cells=(MatrixCell(arch="rv", group="basic", libc="musl"),),
            )

            self.assertTrue(summary.has_errors)
            self.assertEqual(summary.results[0].status, "missing-segment")

    def test_bad_json_is_structured_and_preserves_stdout(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            judge_dir = root / "judge"
            out_dir = root / "out"
            judge_dir.mkdir()
            write_fake_judge(judge_dir / "judge_basic-musl.py", "print('not json')\n")
            marker_result = parse_text(
                "#### OS COMP TEST GROUP START basic-musl ####\n"
                "body\n"
                "#### OS COMP TEST GROUP END basic-musl ####\n",
                arch="rv",
            )

            summary = run_judges(
                marker_result,
                out_dir=out_dir,
                judge_dir=judge_dir,
                fail_fast=True,
                expected_cells=(MatrixCell(arch="rv", group="basic", libc="musl"),),
            )

            result = summary.results[0]
            self.assertEqual(result.status, "bad-json")
            self.assertEqual((out_dir / result.stdout_path).read_text(), "not json\n")
            self.assertIsNone(result.json_path)

    def test_nonzero_exit_is_structured_and_keeps_parsed_rows(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            judge_dir = root / "judge"
            out_dir = root / "out"
            judge_dir.mkdir()
            write_fake_judge(
                judge_dir / "judge_basic-musl.py",
                """
                import json
                print(json.dumps([{"name": "case", "score": 3}]))
                raise SystemExit(7)
                """,
            )
            marker_result = parse_text(
                "#### OS COMP TEST GROUP START basic-musl ####\n"
                "body\n"
                "#### OS COMP TEST GROUP END basic-musl ####\n",
                arch="rv",
            )

            summary = run_judges(
                marker_result,
                out_dir=out_dir,
                judge_dir=judge_dir,
                fail_fast=True,
                expected_cells=(MatrixCell(arch="rv", group="basic", libc="musl"),),
            )

            result = summary.results[0]
            self.assertEqual(result.status, "nonzero-exit")
            self.assertEqual(result.exit_code, 7)
            self.assertEqual(result.rows[0]["score"], 3)
            self.assertTrue((out_dir / result.json_path).is_file())
            judge_json = json.loads((out_dir / result.json_path).read_text())
            self.assertEqual(judge_json["schema"], "oscomp-eval.judge-result.v1")
            self.assertEqual(judge_json["status"], "nonzero-exit")
            self.assertEqual(judge_json["rows"][0]["score"], 3)

    def test_stdout_diagnostics_can_recover_json_with_warning(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            judge_dir = root / "judge"
            out_dir = root / "out"
            judge_dir.mkdir()
            write_fake_judge(
                judge_dir / "judge_basic-musl.py",
                """
                import json
                print("diagnostic before json")
                print(json.dumps([{"name": "case", "score": 5}]))
                """,
            )
            marker_result = parse_text(
                "#### OS COMP TEST GROUP START basic-musl ####\n"
                "body\n"
                "#### OS COMP TEST GROUP END basic-musl ####\n",
                arch="rv",
            )

            summary = run_judges(
                marker_result,
                out_dir=out_dir,
                judge_dir=judge_dir,
                fail_fast=True,
                expected_cells=(MatrixCell(arch="rv", group="basic", libc="musl"),),
            )

            result = summary.results[0]
            self.assertEqual(result.status, "ok")
            self.assertEqual(result.rows[0]["score"], 5)
            self.assertIn("recovered JSON list", result.warnings[0])

    def test_timeout_is_structured(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            judge_dir = root / "judge"
            judge_dir.mkdir()
            write_fake_judge(
                judge_dir / "judge_basic-musl.py",
                """
                import time
                time.sleep(1)
                print("[]")
                """,
            )
            marker_result = parse_text(
                "#### OS COMP TEST GROUP START basic-musl ####\n"
                "body\n"
                "#### OS COMP TEST GROUP END basic-musl ####\n",
                arch="rv",
            )

            summary = run_judges(
                marker_result,
                out_dir=root / "out",
                judge_dir=judge_dir,
                judge_timeout_secs=0.05,
                fail_fast=True,
                expected_cells=(MatrixCell(arch="rv", group="basic", libc="musl"),),
            )

            self.assertEqual(summary.results[0].status, "timeout")


if __name__ == "__main__":
    unittest.main()
