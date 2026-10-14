"""Regression checks for the single build/container boundary."""

from __future__ import annotations

import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]


class BuildWorkflowTests(unittest.TestCase):
    def test_compose_has_one_non_daemon_development_service(self) -> None:
        compose = (ROOT / "dev-env/compose.yaml").read_text(encoding="utf-8")
        self.assertRegex(compose, r"(?m)^services:\s*$")
        self.assertRegex(compose, r"(?m)^  dev:\s*$")
        self.assertNotRegex(compose, r"(?m)^  (builder|boot|netboot):\s*$")
        self.assertIn('restart: "no"', compose)
        self.assertIn('THEKERNEL_DEV_CONTAINER: "1"', compose)
        self.assertNotIn("privileged:", compose)
        self.assertNotRegex(compose, r"(?im)network_mode:\s*host")

    def test_make_targets_use_the_single_runner(self) -> None:
        makefile = (ROOT / "Makefile").read_text(encoding="utf-8")
        self.assertIn("DEV_RUN = ./tools/dev/dev-run.sh", makefile)
        self.assertNotIn("RESOURCE_SCOPE", makefile)
        self.assertNotIn("THEKERNEL_STATE_DIR=", makefile)
        targets = ("run:", "run-gui:", "build:", "lint:", "test:",
                   "bench:", "verify:", "clean:")
        for target in targets:
            section = makefile.split(target, 1)[1].split("\n\n", 1)[0]
            self.assertIn("$(DEV_RUN)", section, target)

    def test_dev_run_does_not_nest_containers(self) -> None:
        runner = (ROOT / "tools/dev/dev-run.sh").read_text(encoding="utf-8")
        self.assertIn('THEKERNEL_DEV_CONTAINER:-0', runner)
        self.assertIn('exec "$@"', runner)
        self.assertIn('tools/dev/dev-shell.sh" -- "$@"', runner)

    def test_device_passthrough_uses_a_compose_override(self) -> None:
        shell = (ROOT / "tools/dev/dev-shell.sh").read_text(encoding="utf-8")
        self.assertIn("device_override", shell)
        self.assertIn("compose_files+=(", shell)
        # These are docker run flags, not docker compose run flags. Keeping
        # them out prevents local KVM support from breaking CI invocation.
        self.assertNotIn("run_args+=(--device", shell)
        self.assertNotIn("run_args+=(--group-add", shell)

    def test_legacy_cleanup_is_narrow(self) -> None:
        cleanup = (ROOT / "tools/dev/host-cleanup.sh").read_text(encoding="utf-8")
        self.assertIn("thekernel-boot", cleanup)
        self.assertIn("thekernel-netboot", cleanup)
        self.assertIn("docker update --restart=no", cleanup)
        # A cleanup command must not turn into a broad Docker or systemd stop.
        self.assertNotRegex(cleanup, r"docker\s+(stop|rm)\s+\$\(")
        self.assertNotRegex(cleanup, r"systemctl\s+.*\s+stop\s+\*")

    def test_n305_capture_prefers_the_hdmi_dongle_and_has_preview(self) -> None:
        capture = (ROOT / "tools/hardware/n305-screen-capture.sh").read_text(encoding="utf-8")
        self.assertIn("/dev/v4l/by-id/*eEver*video-index0", capture)
        self.assertIn("preview)", capture)
        self.assertIn("-input_format", capture)
        self.assertIn("mjpeg", capture)

    def test_setup_toolchain_executes_from_non_repo_cwd(self) -> None:
        import os
        import subprocess
        from tests.support import test_tmpdir

        with test_tmpdir() as tmp:
            tmp_path = Path(tmp)
            mock_bin = tmp_path / "bin"
            mock_bin.mkdir()
            for cmd in ("rustup", "cargo", "curl"):
                stub = mock_bin / cmd
                stub.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
                stub.chmod(0o755)

            state_dir = tmp_path / "state"
            env = {
                **os.environ,
                "PATH": f"{mock_bin}:{os.environ.get('PATH', '')}",
                "THEKERNEL_STATE_DIR": str(state_dir),
                "HOME": str(tmp_path),
            }
            script = ROOT / "tools/dev/setup-toolchain.sh"
            result = subprocess.run(
                ["bash", str(script)],
                cwd=str(tmp_path),
                env=env,
                capture_output=True,
                text=True,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertTrue((state_dir / "toolchain" / "tmp").is_dir())

    def test_owned_tools_do_not_construct_root_scripts_paths(self) -> None:
        import ast
        import re

        for rel_path in ("tools/thekernel.py", "tools/product_state.py", "tools/verification.py"):
            source = (ROOT / rel_path).read_text(encoding="utf-8")
            self.assertNotRegex(
                source,
                r'REPO_ROOT\s*/\s*["\']scripts["\']',
                f"{rel_path} still constructs paths through root scripts/",
            )
            self.assertNotIn(
                '"scripts/',
                source,
                f"{rel_path} still references scripts/ literally",
            )
            self.assertNotIn(
                "'scripts/",
                source,
                f"{rel_path} still references scripts/ literally",
            )

        for py_path in (ROOT / "tools").rglob("*.py"):
            tree = ast.parse(py_path.read_text(encoding="utf-8"), filename=str(py_path))
            for node in ast.walk(tree):
                if isinstance(node, ast.Import):
                    for alias in node.names:
                        self.assertFalse(
                            alias.name == "scripts" or alias.name.startswith("scripts."),
                            f"{py_path.relative_to(ROOT)} imports deprecated module '{alias.name}'",
                        )
                elif isinstance(node, ast.ImportFrom):
                    if node.module:
                        self.assertFalse(
                            node.module == "scripts" or node.module.startswith("scripts."),
                            f"{py_path.relative_to(ROOT)} imports from deprecated module '{node.module}'",
                        )


if __name__ == "__main__":
    unittest.main()
