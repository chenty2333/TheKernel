#!/usr/bin/env python3
"""Focused tests for the CI sibling-source configuration."""

from __future__ import annotations

import unittest
from pathlib import Path

from tests.support import load_script_module, repo_root, test_tmpdir

REPO_ROOT = repo_root()
source_combination = load_script_module("source_combination", "scripts/ci/source_combination.py")


class SourceCombinationTests(unittest.TestCase):
    def test_repository_configuration_produces_exact_checkout_outputs(self) -> None:
        sources = source_combination.load(
            REPO_ROOT / "config/source-combination.toml"
        )

        self.assertEqual(sources["ax"].repository, "chenty2333/thekernel-ax")
        self.assertEqual(
            sources["linux_abi"].repository,
            "chenty2333/thekernel-linux-abi",
        )
        self.assertEqual(
            source_combination.outputs(sources),
            {
                "ax_repository": "chenty2333/thekernel-ax",
                "ax_ref": "962defe2790c8cee6e699e66b1b4b7f8ba97e450",
                "ax_path": "thekernel-ax",
                "linux_abi_repository": "chenty2333/thekernel-linux-abi",
                "linux_abi_ref": "f21c02a03cd2355f18efb28e911976b9750c3e0f",
                "linux_abi_path": "thekernel-linux-abi",
            },
        )

    def test_rejects_non_exact_commit_ref(self) -> None:
        for ref in (
            "development",
            "v1",
            "main",
            "962DEFE2790C8CEE6E699E66B1B4B7F8BA97E450",
            "962defe2790c8cee6e699e66b1b4b7f8ba97e45",
        ):
            with self.subTest(ref=ref), test_tmpdir() as directory:
                config = Path(directory) / "source-combination.toml"
                config.write_text(
                    f'''schema = 1

[source.ax]
repository = "chenty2333/thekernel-ax"
ref = "{ref}"
path = "thekernel-ax"

[source.linux_abi]
repository = "chenty2333/thekernel-linux-abi"
ref = "f21c02a03cd2355f18efb28e911976b9750c3e0f"
path = "thekernel-linux-abi"
''',
                    encoding="utf-8",
                )

                with self.assertRaisesRegex(
                    source_combination.SourceCombinationError,
                    "lowercase 40-hex commit",
                ):
                    source_combination.load(config)

    def test_rejects_invalid_repository(self) -> None:
        with test_tmpdir() as directory:
            config = Path(directory) / "source-combination.toml"
            config.write_text(
                """schema = 1

[source.ax]
repository = "not a repository"
ref = "962defe2790c8cee6e699e66b1b4b7f8ba97e450"
path = "thekernel-ax"

[source.linux_abi]
repository = "chenty2333/thekernel-linux-abi"
ref = "f21c02a03cd2355f18efb28e911976b9750c3e0f"
path = "thekernel-linux-abi"
""",
                encoding="utf-8",
            )

            with self.assertRaisesRegex(
                source_combination.SourceCombinationError, "owner/repository"
            ):
                source_combination.load(config)

    def test_rejects_non_product_source(self) -> None:
        with test_tmpdir() as directory:
            config = Path(directory) / "source-combination.toml"
            config.write_text(
                """schema = 1

[source.ax]
repository = "chenty2333/thekernel-ax"
ref = "962defe2790c8cee6e699e66b1b4b7f8ba97e450"
path = "thekernel-ax"

[source.linux_abi]
repository = "chenty2333/thekernel-linux-abi"
ref = "f21c02a03cd2355f18efb28e911976b9750c3e0f"
path = "thekernel-linux-abi"

[source.extra]
repository = "chenty2333/extra"
ref = "0000000000000000000000000000000000000000"
path = "extra"
""",
                encoding="utf-8",
            )

            with self.assertRaisesRegex(
                source_combination.SourceCombinationError, "exactly"
            ):
                source_combination.load(config)


if __name__ == "__main__":
    unittest.main()
