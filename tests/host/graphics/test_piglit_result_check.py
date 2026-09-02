#!/usr/bin/env python3
"""Behavioral tests for the bounded Piglit result checker."""

from __future__ import annotations

import bz2
import json
import pathlib
import subprocess
import sys
import tempfile
import unittest
from typing import Any


ROOT = pathlib.Path(__file__).resolve().parents[3]
CHECKER = (
    ROOT
    / "config/graphics/overlay/q35-software-desktop/usr/local/bin"
    / "q35-piglit-result-check"
)


class PiglitResultCheckTests(unittest.TestCase):
    def write_results(self, directory: pathlib.Path, document: Any) -> pathlib.Path:
        results = directory / "results.json.bz2"
        with bz2.open(results, "wt", encoding="utf-8") as output:
            json.dump(document, output)
        return results

    def run_checker(self, argument: pathlib.Path) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [sys.executable, str(CHECKER), str(argument)],
            check=False,
            text=True,
            capture_output=True,
        )

    def test_accepts_allowed_results_from_a_results_directory(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = pathlib.Path(temporary)
            self.write_results(
                directory,
                {
                    "tests": {
                        "quick/pass": {"result": "pass"},
                        "quick/skip": {"result": "skip"},
                        "quick/warn": {"result": "warn"},
                        "quick/notrun": {"result": "notrun"},
                    }
                },
            )
            completed = self.run_checker(directory)

        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(
            completed.stdout,
            "THEKERNEL_Q35_VIRGL_PIGLIT_RESULTS notrun=1 pass=1 skip=1 warn=1 errors=0\n",
        )
        self.assertEqual(completed.stderr, "")

    def test_rejects_failures_from_a_direct_compressed_result_file(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = pathlib.Path(temporary)
            results = self.write_results(
                directory,
                {
                    "tests": {
                        "quick/pass": {"result": "pass"},
                        "quick/fail": {"result": "fail"},
                        "quick/crash": {"result": "crash"},
                        "quick/timeout": {"result": "timeout"},
                    }
                },
            )
            completed = self.run_checker(results)

        self.assertEqual(completed.returncode, 1, completed.stderr)
        self.assertEqual(
            completed.stdout,
            "THEKERNEL_Q35_VIRGL_PIGLIT_RESULTS crash=1 fail=1 pass=1 timeout=1 errors=3\n",
        )
        self.assertEqual(completed.stderr, "")

    def test_rejects_missing_or_empty_test_mappings_as_invalid_input(self) -> None:
        for document in ({}, {"tests": {}}):
            with self.subTest(document=document), tempfile.TemporaryDirectory() as temporary:
                results = self.write_results(pathlib.Path(temporary), document)
                completed = self.run_checker(results)

            self.assertEqual(completed.returncode, 2)
            self.assertEqual(completed.stdout, "")
            self.assertIn("Piglit results must contain a nonempty tests mapping", completed.stderr)

    def test_rejects_malformed_result_records_as_invalid_input(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            results = self.write_results(
                pathlib.Path(temporary),
                {"tests": {"quick/bad": {"result": 1}}},
            )
            completed = self.run_checker(results)

        self.assertEqual(completed.returncode, 2)
        self.assertEqual(completed.stdout, "")
        self.assertIn("has no string result", completed.stderr)

    def test_rejects_malformed_json_inside_a_compressed_fixture(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            results = pathlib.Path(temporary) / "results.json.bz2"
            with bz2.open(results, "wt", encoding="utf-8") as output:
                output.write('{"tests":')
            completed = self.run_checker(results)

        self.assertEqual(completed.returncode, 2)
        self.assertEqual(completed.stdout, "")
        self.assertIn("cannot read Piglit results", completed.stderr)


if __name__ == "__main__":
    unittest.main()
