from __future__ import annotations

import importlib.util
import contextlib
import io
import json
import sys
import tempfile
import unittest
from pathlib import Path


def load_ltp_lab_module():
    script = Path(__file__).resolve().parents[2] / "scripts" / "ltp-lab.py"
    spec = importlib.util.spec_from_file_location("ltp_lab_for_tests", script)
    if spec is None or spec.loader is None:
        raise RuntimeError("could not load scripts/ltp-lab.py")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


class LtpLabMarkerIntegrationTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.ltp_lab = load_ltp_lab_module()

    def test_ltp_lab_uses_shared_group_marker_shape(self) -> None:
        parsed = self.ltp_lab.parse_log_text(
            "#### OS COMP TEST GROUP START ltp-musl ####\n"
            "RUN LTP CASE read01\n"
            "TPASS: read worked\n"
            "PASS LTP CASE read01 : 0\n"
            "#### OS COMP TEST GROUP END ltp-musl ####\n",
            arch="rv",
        )

        self.assertEqual(parsed["summary"]["cases"], 1)
        self.assertEqual(parsed["cases"][0]["group"], "ltp-musl")
        self.assertEqual(parsed["cases"][0]["libc"], "musl")
        self.assertEqual(parsed["cases"][0]["status"], "pass")

    def test_ltp_case_timeout_banner_is_not_global_runner_timeout(self) -> None:
        parsed = self.ltp_lab.parse_log_text(
            "#### OS COMP TEST GROUP START ltp-musl ####\n"
            "RUN LTP CASE read01\n"
            "tst_test.c:1617: TINFO: Timeout per run is 0h 00m 30s\n"
            "PASS LTP CASE read01 : 0\n"
            "#### OS COMP TEST GROUP END ltp-musl ####\n",
            arch="rv",
        )

        self.assertFalse(parsed["summary"]["global_timeout"])

    def test_parse_log_file_writes_to_explicit_output_dir(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            log_path = tmp_path / "console.log"
            out_dir = tmp_path / "parsed" / "rv"
            log_path.write_text(
                "#### OS COMP TEST GROUP START ltp-musl ####\n"
                "RUN LTP CASE read01\n"
                "TPASS: read worked\n"
                "PASS LTP CASE read01 : 0\n"
                "#### OS COMP TEST GROUP END ltp-musl ####\n",
                encoding="utf-8",
            )

            parsed = self.ltp_lab.parse_log_file(
                log_path,
                arch="rv",
                output_dir=out_dir,
            )

            self.assertEqual(parsed["summary"]["cases"], 1)
            self.assertTrue((out_dir / "summary.json").is_file())
            self.assertTrue((out_dir / "cases.jsonl").is_file())
            self.assertEqual(
                len((out_dir / "cases.jsonl").read_text(encoding="utf-8").splitlines()),
                1,
            )

    def test_summarize_run_can_link_score_report(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            ltp_run = tmp_path / "ltp-run"
            score_run = tmp_path / "score-run"
            (ltp_run / "rv").mkdir(parents=True)
            score_run.mkdir()
            (ltp_run / "manifest.json").write_text(
                json.dumps({"run_id": "ltp-run"}),
                encoding="utf-8",
            )
            (ltp_run / "rv" / "summary.json").write_text(
                json.dumps(
                    {
                        "arch": "rv",
                        "cases": 1,
                        "global_timeout": False,
                        "global_panic": False,
                        "by_status": {"pass": 1},
                        "by_libc": {"musl": {"pass": 1}},
                    }
                ),
                encoding="utf-8",
            )
            (score_run / "score.json").write_text(
                json.dumps({"total_score": 12.5, "issues": [{"kind": "missing"}]}),
                encoding="utf-8",
            )
            (score_run / "report.md").write_text("# report\n", encoding="utf-8")

            stdout = io.StringIO()
            with contextlib.redirect_stdout(stdout):
                combined = self.ltp_lab.summarize_run(
                    ltp_run,
                    score_run_dirs=[score_run],
                )

            self.assertEqual(combined["score_reports"][0]["total_score"], 12.5)
            self.assertEqual(combined["score_reports"][0]["issue_count"], 1)
            self.assertIn("score_reports:", stdout.getvalue())
            self.assertTrue((ltp_run / "combined-summary.json").is_file())


if __name__ == "__main__":
    unittest.main()
