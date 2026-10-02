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
        self.assertIn("DEV_RUN = ./scripts/dev-run.sh", makefile)
        self.assertNotIn("RESOURCE_SCOPE", makefile)
        self.assertNotIn("THEKERNEL_STATE_DIR=", makefile)
        targets = ("run:", "run-gui:", "build:", "lint:", "test:",
                   "bench:", "verify:", "clean:")
        for target in targets:
            section = makefile.split(target, 1)[1].split("\n\n", 1)[0]
            self.assertIn("$(DEV_RUN)", section, target)

    def test_dev_run_does_not_nest_containers(self) -> None:
        runner = (ROOT / "scripts/dev-run.sh").read_text(encoding="utf-8")
        self.assertIn('THEKERNEL_DEV_CONTAINER:-0', runner)
        self.assertIn('exec "$@"', runner)
        self.assertIn('scripts/dev-shell.sh" -- "$@"', runner)

    def test_device_passthrough_uses_a_compose_override(self) -> None:
        shell = (ROOT / "scripts/dev-shell.sh").read_text(encoding="utf-8")
        self.assertIn("device_override", shell)
        self.assertIn("compose_files+=(", shell)
        # These are docker run flags, not docker compose run flags. Keeping
        # them out prevents local KVM support from breaking CI invocation.
        self.assertNotIn("run_args+=(--device", shell)
        self.assertNotIn("run_args+=(--group-add", shell)

    def test_legacy_cleanup_is_narrow(self) -> None:
        cleanup = (ROOT / "scripts/host-cleanup.sh").read_text(encoding="utf-8")
        self.assertIn("thekernel-boot", cleanup)
        self.assertIn("thekernel-netboot", cleanup)
        self.assertIn("docker update --restart=no", cleanup)
        # A cleanup command must not turn into a broad Docker or systemd stop.
        self.assertNotRegex(cleanup, r"docker\s+(stop|rm)\s+\$\(")
        self.assertNotRegex(cleanup, r"systemctl\s+.*\s+stop\s+\*")


if __name__ == "__main__":
    unittest.main()
