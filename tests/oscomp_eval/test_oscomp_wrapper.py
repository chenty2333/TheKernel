from __future__ import annotations

import subprocess
import unittest

from tools.oscomp_eval.paths import repo_root


class OscompWrapperTests(unittest.TestCase):
    def run_wrapper(self, *args: str) -> subprocess.CompletedProcess[str]:
        root = repo_root()
        return subprocess.run(
            [str(root / "scripts" / "oscomp.sh"), *args],
            cwd=root,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )

    def test_unknown_subcommand_is_usage_error(self) -> None:
        result = self.run_wrapper("unknown-command")

        self.assertEqual(result.returncode, 2)
        self.assertIn("unknown subcommand", result.stderr)

    def test_list_prints_current_plan(self) -> None:
        result = self.run_wrapper("list")

        self.assertEqual(result.returncode, 0)
        self.assertIn("arches:", result.stdout)
        self.assertIn("rv (riscv64)", result.stdout)
        self.assertIn("/musl basic", result.stdout)
        self.assertIn("groups in fixed plan:", result.stdout)

    def test_validate_output_fixture_succeeds(self) -> None:
        root = repo_root()
        result = self.run_wrapper(
            "validate-output",
            "--log",
            str(root / "tests" / "oscomp_eval" / "fixtures" / "marker-valid.log"),
            "--arch",
            "rv",
        )

        self.assertEqual(result.returncode, 0)
        self.assertIn("oscomp-output arch=rv", result.stdout)
        self.assertIn("issues=0", result.stdout)
        self.assertNotIn("compatibility shim", result.stderr)

    def test_direct_validate_script_prints_migration_hint(self) -> None:
        root = repo_root()
        result = subprocess.run(
            [
                "python3",
                str(root / "scripts" / "validate-oscomp-output.py"),
                "--log",
                str(root / "tests" / "oscomp_eval" / "fixtures" / "marker-valid.log"),
                "--arch",
                "rv",
            ],
            cwd=root,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )

        self.assertEqual(result.returncode, 0)
        self.assertIn("oscomp-output arch=rv", result.stdout)
        self.assertIn("compatibility shim", result.stderr)
        self.assertIn("scripts/oscomp.sh validate-output", result.stderr)

    def test_lab_help_remains_available(self) -> None:
        result = self.run_wrapper("lab", "--help")

        self.assertEqual(result.returncode, 0)
        self.assertIn("Local LTP experiment harness", result.stdout)
        self.assertIn("replay", result.stdout)

    def test_missing_option_value_is_usage_error(self) -> None:
        result = self.run_wrapper("evaluate", "--rv-log")

        self.assertEqual(result.returncode, 2)
        self.assertIn("missing value for --rv-log", result.stderr)

    def test_official_refresh_requires_explicit_source(self) -> None:
        result = self.run_wrapper("official-refresh")

        self.assertEqual(result.returncode, 2)
        self.assertIn("--source", result.stderr)

    def test_inspect_run_requires_directory_argument(self) -> None:
        result = self.run_wrapper("inspect-run")

        self.assertEqual(result.returncode, 2)
        self.assertIn("inspect-run requires exactly one RUN_DIR", result.stderr)

    def test_inspect_run_rejects_unknown_option(self) -> None:
        result = self.run_wrapper("inspect-run", "--bad")

        self.assertEqual(result.returncode, 2)
        self.assertIn("unknown inspect-run option: --bad", result.stderr)


if __name__ == "__main__":
    unittest.main()
