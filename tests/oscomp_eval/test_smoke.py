from __future__ import annotations

import subprocess
import tempfile
import unittest
from pathlib import Path

from tools.oscomp_eval.paths import repo_root


class SmokeLibTests(unittest.TestCase):
    def run_lib(self, *lines: str) -> subprocess.CompletedProcess[str]:
        root = repo_root()
        script = "\n".join(
            [
                "set -euo pipefail",
                f'REPO_ROOT="{root}"',
                f'source "{root / "scripts" / "smoke" / "lib.sh"}"',
                *lines,
            ]
        )
        return subprocess.run(
            ["bash", "-c", script],
            cwd=root,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )

    def test_kernel_shell_targets(self) -> None:
        result = self.run_lib(
            'test "$(smoke_kernel_shell_make_target rv)" = kernel-rv-shell',
            'test "$(smoke_kernel_shell_make_target la)" = kernel-la-shell',
            'test "$(smoke_kernel_shell_path rv)" = .state/shell/kernel-rv',
        )

        self.assertEqual(result.returncode, 0, msg=result.stderr)

    def test_explicit_support_image_skips_rebuild(self) -> None:
        with tempfile.NamedTemporaryFile(suffix=".img", delete=False) as tmp:
            tmp.write(b"disk")
            tmp.flush()
            support_image = Path(tmp.name)

        try:
            result = self.run_lib(
                f'SUPPORT_IMAGE="{support_image}"',
                'smoke_build_support_image_if_needed rv "$SUPPORT_IMAGE" 1',
            )
        finally:
            support_image.unlink(missing_ok=True)

        self.assertEqual(result.returncode, 0, msg=result.stderr)


class SmokeDispatcherTests(unittest.TestCase):
    def test_smoke_list_documents_boot_shell_kernel(self) -> None:
        root = repo_root()
        result = subprocess.run(
            [str(root / "scripts" / "smoke.sh"), "--help"],
            cwd=root,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )

        self.assertEqual(result.returncode, 0)
        self.assertIn("lwext4-io-boost", result.stdout)
        self.assertIn("kernel-*-shell", result.stdout)


if __name__ == "__main__":
    unittest.main()
