#!/usr/bin/env python3
"""Focused tests for the CI sibling-source combination record."""

from __future__ import annotations

import importlib.util
import sys
import tempfile
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
SCRIPT_PATH = REPO_ROOT / "scripts/ci/source_combination.py"
SPEC = importlib.util.spec_from_file_location("source_combination", SCRIPT_PATH)
assert SPEC is not None and SPEC.loader is not None
source_combination = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = source_combination
SPEC.loader.exec_module(source_combination)


class SourceCombinationTests(unittest.TestCase):
    def test_repository_record_and_id_are_stable(self) -> None:
        sources = source_combination.load(
            REPO_ROOT / "config/source-combination.toml"
        )

        self.assertEqual(sources["ax"].repository, "chenty2333/thekernel-ax")
        self.assertEqual(
            sources["linux_abi"].ref,
            "f0721ef792ecd0c4826a00b90b88a524f6411d47",
        )
        self.assertEqual(
            source_combination.combination_id(sources, "a" * 40),
            source_combination.combination_id(
                dict(reversed(list(sources.items()))), "a" * 40
            ),
        )
        values = source_combination.outputs(sources, "a" * 40)
        self.assertEqual(values["ax_path"], "thekernel-ax")
        self.assertTrue(values["combination_id"].startswith("source-combination-v1-"))

    def test_rejects_non_commit_ref(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            record = Path(directory) / "source-combination.toml"
            record.write_text(
                """schema = 1

[source.ax]
repository = "chenty2333/thekernel-ax"
ref = "main"
path = "thekernel-ax"

[source.linux_abi]
repository = "chenty2333/thekernel-linux-abi"
ref = "f0721ef792ecd0c4826a00b90b88a524f6411d47"
path = "thekernel-linux-abi"
""",
                encoding="utf-8",
            )

            with self.assertRaisesRegex(
                source_combination.SourceCombinationError, "40-hex"
            ):
                source_combination.load(record)

    def test_rejects_invalid_repository(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            record = Path(directory) / "source-combination.toml"
            record.write_text(
                """schema = 1

[source.ax]
repository = "not a repository"
ref = "21582660f97c986d080615d51fca0accfd43fcb2"
path = "thekernel-ax"

[source.linux_abi]
repository = "chenty2333/thekernel-linux-abi"
ref = "f0721ef792ecd0c4826a00b90b88a524f6411d47"
path = "thekernel-linux-abi"
""",
                encoding="utf-8",
            )

            with self.assertRaisesRegex(
                source_combination.SourceCombinationError, "owner/repository"
            ):
                source_combination.load(record)

    def test_rejects_non_product_source(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            record = Path(directory) / "source-combination.toml"
            record.write_text(
                """schema = 1

[source.ax]
repository = "chenty2333/thekernel-ax"
ref = "21582660f97c986d080615d51fca0accfd43fcb2"
path = "thekernel-ax"

[source.linux_abi]
repository = "chenty2333/thekernel-linux-abi"
ref = "f0721ef792ecd0c4826a00b90b88a524f6411d47"
path = "thekernel-linux-abi"

[source.visa]
repository = "chenty2333/vISA"
ref = "198b1bdf7641717f52cd386752f0f25974db6d11"
path = "vISA"
""",
                encoding="utf-8",
            )

            with self.assertRaisesRegex(
                source_combination.SourceCombinationError, "exactly"
            ):
                source_combination.load(record)

    def test_id_requires_thekernel_full_commit(self) -> None:
        sources = source_combination.load(
            REPO_ROOT / "config/source-combination.toml"
        )
        with self.assertRaisesRegex(
            source_combination.SourceCombinationError, "TheKernel commit"
        ):
            source_combination.combination_id(sources, "not-a-commit")


if __name__ == "__main__":
    unittest.main()
