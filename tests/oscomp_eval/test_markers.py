from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

from tools.oscomp_eval.markers import (
    compatible_summary,
    parse_log,
    parse_text,
    split_group_name,
    write_artifacts,
)


FIXTURES = Path(__file__).resolve().parent / "fixtures"


class GroupNameTests(unittest.TestCase):
    def test_split_known_libc_suffix(self) -> None:
        base, libc, issue = split_group_name("basic-musl")
        self.assertEqual(base, "basic")
        self.assertEqual(libc, "musl")
        self.assertIsNone(issue)

    def test_split_unknown_libc_suffix_on_known_group(self) -> None:
        base, libc, issue = split_group_name("basic-uclibc")
        self.assertEqual(base, "basic")
        self.assertEqual(libc, "uclibc")
        self.assertIsNotNone(issue)
        self.assertEqual(issue.kind, "unknown-libc")


class MarkerParserTests(unittest.TestCase):
    def test_valid_log_has_two_complete_segments(self) -> None:
        result = parse_log(FIXTURES / "marker-valid.log", arch="rv")
        self.assertFalse(result.has_errors)
        self.assertEqual(result.marker_count, 4)
        self.assertEqual(result.complete_count, 2)
        self.assertTrue(result.conclusion_found)
        self.assertEqual([segment.group for segment in result.segments], ["basic-musl", "lua-glibc"])
        self.assertEqual(result.segments[0].body, "brk pass\nopen pass\n")

    def test_truncated_log_records_incomplete_segment(self) -> None:
        result = parse_log(FIXTURES / "marker-truncated.log", arch="rv")
        self.assertTrue(result.has_errors)
        self.assertEqual(result.complete_count, 0)
        self.assertEqual(result.segments[0].status, "incomplete")
        self.assertEqual(result.issues[0].kind, "start-without-end")
        self.assertTrue(any(issue.kind == "zero-complete-groups" for issue in result.issues))

    def test_nested_log_closes_open_segment_as_incomplete(self) -> None:
        result = parse_log(FIXTURES / "marker-nested.log", arch="rv")
        self.assertTrue(result.has_errors)
        self.assertEqual([segment.status for segment in result.segments], ["incomplete", "complete"])
        self.assertEqual(result.segments[0].group, "basic-musl")
        self.assertEqual(result.segments[1].group, "lua-musl")
        self.assertTrue(any(issue.kind == "nested-start" for issue in result.issues))

    def test_duplicate_segments_keep_sequence_numbers(self) -> None:
        result = parse_log(FIXTURES / "marker-duplicate.log", arch="la")
        self.assertFalse(result.has_errors)
        self.assertEqual([segment.sequence for segment in result.segments], [1, 2])
        self.assertEqual([segment.body for segment in result.segments], ["first\n", "second\n"])

    def test_unknown_group_and_panic_are_reported(self) -> None:
        result = parse_log(FIXTURES / "marker-unknown-panic.log", arch="rv")
        self.assertTrue(result.has_errors)
        self.assertEqual(result.complete_count, 1)
        self.assertTrue(any(issue.kind == "unknown-group" for issue in result.issues))
        self.assertEqual(result.log_events[0].kind, "panic")

    def test_require_conclusion_adds_issue(self) -> None:
        result = parse_text(
            "#### OS COMP TEST GROUP START basic-musl ####\n"
            "ok\n"
            "#### OS COMP TEST GROUP END basic-musl ####\n",
            arch="rv",
            require_conclusion=True,
        )
        self.assertTrue(result.has_errors)
        self.assertTrue(any(issue.kind == "missing-conclusion" for issue in result.issues))

    def test_ltp_case_timeout_text_is_not_runner_timeout_event(self) -> None:
        result = parse_text(
            "#### OS COMP TEST GROUP START ltp-musl ####\n"
            "tst_test.c:1617: TINFO: Timeout per run is 0h 00m 30s\n"
            "#### OS COMP TEST GROUP END ltp-musl ####\n"
            "shutdown\n",
            arch="rv",
        )
        self.assertEqual(result.log_events, ())

    def test_compatible_summary_shape(self) -> None:
        result = parse_log(FIXTURES / "marker-valid.log", arch="rv")
        summary = compatible_summary(result)
        self.assertIn("oscomp-output arch=rv markers=4 complete_groups=2 issues=0", summary)
        self.assertIn("complete basic-musl lines=2-5", summary)


class MarkerArtifactTests(unittest.TestCase):
    def test_write_artifacts_keeps_duplicate_segment_bodies(self) -> None:
        result = parse_log(FIXTURES / "marker-duplicate.log", arch="rv")
        with tempfile.TemporaryDirectory() as tmp:
            out_dir = Path(tmp)
            write_artifacts(result, out_dir)

            validation = json.loads((out_dir / "marker-validation.json").read_text())
            self.assertEqual(validation["schema"], "oscomp-eval.marker-artifacts.v1")
            self.assertEqual(validation["complete_group_count"], 2)

            rows = [
                json.loads(line)
                for line in (out_dir / "segments.jsonl").read_text().splitlines()
            ]
            self.assertEqual(rows[0]["schema"], "oscomp-eval.segment-record.v1")
            self.assertEqual(rows[1]["schema"], "oscomp-eval.segment-record.v1")
            self.assertEqual(rows[0]["body_path"], "segments/basic-musl.txt")
            self.assertEqual(rows[1]["body_path"], "segments/basic-musl.2.txt")
            self.assertEqual((out_dir / "segments" / "basic-musl.txt").read_text(), "first\n")
            self.assertEqual((out_dir / "segments" / "basic-musl.2.txt").read_text(), "second\n")


if __name__ == "__main__":
    unittest.main()
