"""Focused tests for the q35-preview-v0 evidence gate."""

from __future__ import annotations

import json
import subprocess
import tempfile
import unittest
from pathlib import Path

from tools.qemu_runner import gate_manifest as gate


class GateManifestTests(unittest.TestCase):
    def make_root(self, directory: Path) -> Path:
        root = directory / "TheKernel"
        root.mkdir()
        (root / "config").mkdir()
        (root / "config/source-combination.toml").write_text(
            "schema = 1\n[source.ax]\nrepository = 'owner/ax'\nref = '1111111111111111111111111111111111111111'\npath = 'ax'\n[source.linux_abi]\nrepository = 'owner/abi'\nref = '2222222222222222222222222222222222222222'\npath = 'abi'\n"
        )
        for checkout, commit in ((root, "a" * 40), (directory / "ax", "1" * 40), (directory / "abi", "2" * 40)):
            subprocess.run(("git", "init", "-q", str(checkout)), check=True)
            subprocess.run(("git", "-C", str(checkout), "config", "user.email", "test@example.invalid"), check=True)
            subprocess.run(("git", "-C", str(checkout), "config", "user.name", "Test"), check=True)
            (checkout / "tracked").write_text(commit)
            subprocess.run(("git", "-C", str(checkout), "add", "."), check=True)
            subprocess.run(("git", "-C", str(checkout), "commit", "-qm", "fixture"), check=True)
            if checkout != root:
                subprocess.run(("git", "-C", str(checkout), "commit", "--amend", "--no-edit", "--date", "2020-01-01T00:00:00Z"), stdout=subprocess.DEVNULL, check=True)
                # refs in the TOML must be the actual fixture commits.
        config = root / "config/source-combination.toml"
        content = config.read_text()
        content = content.replace("1" * 40, subprocess.check_output(("git", "-C", str(directory / "ax"), "rev-parse", "HEAD"), text=True).strip())
        content = content.replace("2" * 40, subprocess.check_output(("git", "-C", str(directory / "abi"), "rev-parse", "HEAD"), text=True).strip())
        config.write_text(content)
        subprocess.run(("git", "-C", str(root), "add", "config/source-combination.toml"), check=True)
        subprocess.run(("git", "-C", str(root), "commit", "-qm", "declare siblings"), check=True)
        return root

    def runner(self, *, skip=False, marker_only=False, terminated=False, fail="", guest_body=None, remove_rootfs_after_launch=False, replace_kernel_after_launch=False, dirty_source_after_launch=False):
        def execute(command, cwd):
            action = command[1] if len(command) > 1 else command[0]
            if action == "system-test":
                workdir = Path(command[command.index("--workdir") + 1])
                workdir.mkdir(parents=True, exist_ok=True)
                if guest_body is not None:
                    body = guest_body
                elif marker_only:
                    body = "# THEKERNEL_SYSTEM_TEST_COMPLETE\n"
                else:
                    body = "KTAP version 1\n1..1\nok 1 - smoke" + (" # SKIP no ABI" if skip else "") + "\n# THEKERNEL_SYSTEM_TEST_COMPLETE\n"
                    if terminated:
                        body += "QEMU stopped after marker\n"
                (workdir / "console.log").write_text(body)
                receipt_path = Path(command[command.index("--receipt") + 1])
                artifact_dir = next(
                    parent for parent in workdir.parents if (parent / "kernel").is_file()
                )
                receipt_path.write_text(json.dumps({
                    "state": "recorded",
                    "returncode": 0,
                    "runner_terminated": False,
                    "workdir": str(workdir.resolve()),
                    "log_path": str((workdir / "console.log").resolve()),
                    "interaction": {
                        "interactive": True,
                        "input_after_marker": "# THEKERNEL_SYSTEM_TEST_COMPLETE",
                        "stop_after_marker": None,
                    },
                    "source_identity": {
                        "schema": 1,
                        "combination_id": gate.preflight(cwd)["combination_id"],
                        "sources": gate.preflight(cwd)["sources"],
                    },
                    "kernel": gate.file_evidence(artifact_dir / "kernel"),
                    "esp_source": gate.file_evidence(artifact_dir / "kernel.esp"),
                    "rootfs_source": gate.file_evidence(artifact_dir / "rootfs.img"),
                    "launch_handles": {
                        "esp": {"source": gate.file_evidence(artifact_dir / "kernel.esp")},
                        "rootfs": {"source": gate.file_evidence(artifact_dir / "rootfs.img")},
                    },
                }))
                if remove_rootfs_after_launch:
                    (artifact_dir / "rootfs.img").unlink()
                if replace_kernel_after_launch:
                    (artifact_dir / "kernel.esp").write_text("replaced")
                if dirty_source_after_launch:
                    (cwd / "source-untracked").write_text("dirty")
            return gate.CompletedCommand(1 if action == fail else 0, b"out", b"err")
        return execute

    def run_fixture(self, **kwargs):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        root = self.make_root(Path(directory.name))
        artifacts = [root.parent / name for name in ("kernel", "kernel.esp", "rootfs.img")]
        for artifact in artifacts:
            artifact.write_text("stable")
        output = root.parent / "gate/manifest.json"
        result = gate.run_gate(output, root=root, runner=self.runner(**kwargs), artifacts=artifacts)
        return result, json.loads(output.read_text()), root

    def test_pass_records_commands_logs_and_guest_evidence(self):
        result, manifest, _ = self.run_fixture()
        self.assertEqual(result, 0)
        self.assertEqual(manifest["state"], "passed")
        self.assertEqual([row["name"] for row in manifest["commands"]], ["build", "lint", "portable_differential", "system_test"])
        self.assertTrue(all(len(row["stdout"]["sha256"]) == 64 for row in manifest["commands"]))
        self.assertTrue(manifest["guest"]["valid"])
        self.assertTrue(manifest["guest"]["guest_clean_shutdown"])
        self.assertTrue(manifest["guest"]["ktap_complete"])
        self.assertEqual(manifest["guest"]["ktap_result_numbers"], [1])
        self.assertTrue(manifest["artifacts"]["complete"])
        self.assertEqual(manifest["artifacts"]["producer"], "system_test")
        self.assertEqual(set(manifest["artifacts"]["launch_inputs"]), {"esp", "rootfs"})
        self.assertTrue(all(len(row["sha256"]) == 64 for row in manifest["artifacts"]["launch_inputs"].values()))
        self.assertTrue(manifest["artifacts"]["unchanged_since_launch"])

    def test_dirty_and_declared_mismatch_fail_preflight(self):
        result, manifest, root = self.run_fixture()
        self.assertEqual(result, 0)
        (root.parent / "ax/untracked").write_text("dirty")
        output = root / "dirty.json"
        self.assertEqual(gate.run_gate(output, root=root, runner=self.runner()), 1)
        self.assertIn("source preflight rejected", json.loads(output.read_text())["failure"])

    def test_declared_commit_mismatch_fails_clean_preflight(self):
        with tempfile.TemporaryDirectory() as directory:
            root = self.make_root(Path(directory))
            config = root / "config/source-combination.toml"
            ax_commit = subprocess.check_output(
                ("git", "-C", str(root.parent / "ax"), "rev-parse", "HEAD"), text=True
            ).strip()
            config.write_text(config.read_text().replace(ax_commit, "0" * 40, 1))
            subprocess.run(("git", "-C", str(root), "add", "config/source-combination.toml"), check=True)
            subprocess.run(("git", "-C", str(root), "commit", "-qm", "mismatch"), check=True)
            output = root / "mismatch.json"
            self.assertEqual(gate.run_gate(output, root=root, runner=self.runner()), 1)
            self.assertIn("source preflight rejected: ax", json.loads(output.read_text())["failure"])

    def test_skip_marker_only_and_runner_termination_never_pass(self):
        for kwargs in ({"skip": True}, {"marker_only": True}, {"terminated": True}):
            result, manifest, _ = self.run_fixture(**kwargs)
            self.assertEqual(result, 1)
            self.assertEqual(manifest["state"], "failed")

    def test_timeout_words_in_successful_guest_diagnostics_are_not_runner_timeouts(self):
        body = "KTAP version 1\n1..1\nok 1 - timeout boundary\n# timeout exercised normally\n# THEKERNEL_SYSTEM_TEST_COMPLETE\n"
        result, manifest, _ = self.run_fixture(guest_body=body)
        self.assertEqual(result, 0)
        self.assertTrue(manifest["guest"]["guest_clean_shutdown"])
        self.assertTrue(manifest["guest"]["ktap_complete"])

    def test_ktap_plan_requires_unique_complete_all_ok_results(self):
        for body in (
            "KTAP version 1\n1..2\nok 1 - one\nok 1 - duplicate\n",
            "KTAP version 1\n1..2\nok 1 - one\n",
            "KTAP version 1\n1..2\nok 1 - one\nnot ok 2 - two\n",
        ):
            result, manifest, _ = self.run_fixture(guest_body=body)
            self.assertEqual(result, 1)
            self.assertFalse(manifest["guest"]["ktap_complete"])

    def test_complete_ktap_and_zero_returncode_without_marker_never_pass(self):
        body = "KTAP version 1\n1..1\nok 1 - smoke\n"
        result, manifest, _ = self.run_fixture(guest_body=body)
        self.assertEqual(result, 1)
        self.assertTrue(manifest["guest"]["ktap_complete"])
        self.assertFalse(manifest["guest"]["completion_marker_seen"])
        self.assertIn("missing post-suite completion marker", manifest["guest"]["failures"])

    def test_nonzero_system_test_returncode_is_not_clean_shutdown(self):
        result, manifest, _ = self.run_fixture(fail="system-test")
        self.assertEqual(result, 1)
        self.assertEqual(manifest["guest"]["system_test_returncode"], 1)
        self.assertFalse(manifest["guest"]["guest_clean_shutdown"])

    def test_command_failure_is_recorded_and_stops_gate(self):
        result, manifest, _ = self.run_fixture(fail="lint")
        self.assertEqual(result, 1)
        self.assertEqual([row["name"] for row in manifest["commands"]], ["build", "lint"])
        self.assertEqual(manifest["commands"][-1]["returncode"], 1)

    def test_missing_final_artifact_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = self.make_root(Path(directory))
            artifacts = [root.parent / name for name in ("kernel", "kernel.esp", "rootfs.img")]
            for artifact in artifacts:
                artifact.write_text("present")
            output = root.parent / "mutation.json"
            self.assertEqual(gate.run_gate(output, root=root, runner=self.runner(remove_rootfs_after_launch=True), artifacts=artifacts), 1)
            self.assertIn("required final rootfs artifact is missing", json.loads(output.read_text())["failure"])

    def test_replaced_launch_input_is_rejected_against_qemu_receipt(self):
        result, manifest, _ = self.run_fixture(replace_kernel_after_launch=True)
        self.assertEqual(result, 1)
        self.assertFalse(manifest["artifacts"]["unchanged_since_launch"])
        self.assertIn("launch inputs changed or were replaced", manifest["failure"])

    def test_source_dirty_after_command_is_rejected_by_postflight(self):
        result, manifest, _ = self.run_fixture(dirty_source_after_launch=True)
        self.assertEqual(result, 1)
        self.assertIn("source identity changed during gate", manifest["failure"])
        self.assertFalse(manifest["postflight"]["valid"])


if __name__ == "__main__":
    unittest.main()
