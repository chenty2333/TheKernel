"""Tier isolation, fail-fast execution, and manual runner availability."""
import argparse
import os
from pathlib import Path
import signal
import sys
import subprocess
import unittest
from unittest.mock import Mock, patch

from tests.support import load_script_module, test_tmpdir
from tools import verification as verify
from tools.product_state import ProductError


class VerificationTests(unittest.TestCase):
    def test_daily_is_bounded_and_does_not_build_desktop_or_run_comparisons(self):
        stages = verify.plan("daily", Path("/home/build"))
        commands = [stage.command for stage in stages]
        self.assertTrue(any("host" in command for command in commands))
        self.assertTrue(any("lint" in command for command in commands))
        guest = next(stage for stage in stages if stage.name == "guest-tcg")
        for name in ("build", "lint", "guest-tcg"):
            command = next(stage.command for stage in stages if stage.name == name)
            self.assertEqual(command[command.index("--memory") + 1], "512M")
        self.assertIn("--no-build", guest.command)
        self.assertIn("300", guest.command)
        self.assertLessEqual(guest.timeout, 360)
        self.assertFalse(any("bench" in command or "abi" in command or "--fetch-buildroot" in command for command in commands))

    def test_full_extends_daily_with_existing_seatd_pixel_test(self):
        state = Path("/home/build")
        daily = verify.plan("daily", state)
        full = verify.plan("full", state)
        self.assertEqual(full[:len(daily)], daily)
        self.assertEqual([stage.name for stage in full[len(daily):]], ["graphics-rootfs", "pixman"])
        self.assertIn("--fetch-buildroot", full[-2].command)
        self.assertIn("q35-graphics-seatd", full[-1].command)
        self.assertIn("--screenshot", full[-1].command)

    def test_hardware_is_explicit_cpu_correctness(self):
        stages = verify.plan("hardware", Path("/home/build"))
        self.assertEqual(len(stages), 1)
        self.assertIn("cpu", stages[0].command)
        self.assertIn("kvm", stages[0].command)

    def test_failed_environment_prevents_all_stages(self):
        with test_tmpdir() as directory, patch.object(verify, "state_root", return_value=Path(directory)), \
                patch.object(verify, "environment", side_effect=ProductError("missing tool")), \
                patch.object(verify, "execute") as execute:
            with self.assertRaisesRegex(ProductError, "missing tool"):
                verify.verify_cmd(argparse.Namespace(tier="daily"))
            execute.assert_not_called()

    def test_missing_pin_does_not_request_toolchain_installation(self):
        with patch.object(verify.shutil, "which", return_value="/usr/bin/tool"), \
                patch.object(verify, "execute", side_effect=[None, ProductError("not installed")]) as execute:
            with self.assertRaisesRegex(ProductError, "not installed"):
                verify.environment("daily", dict(os.environ))
            command = execute.call_args.args[0].command
            self.assertEqual(command[:2], ("rustup", "run"))
            self.assertNotIn("--install", command)

    def test_timeout_terminates_stage_process_group(self):
        process = Mock(pid=4321)
        process.wait.side_effect = [subprocess.TimeoutExpired("test", 1), 0, 0]
        stage = verify.Stage("guest", "test", ("test",), 1)
        with patch.object(verify.subprocess, "Popen", return_value=process) as popen, \
                patch.object(verify.os, "killpg") as kill:
            with self.assertRaisesRegex(ProductError, "type=timeout"):
                verify.execute(stage, {})
            self.assertTrue(popen.call_args.kwargs["start_new_session"])
            self.assertEqual(kill.call_args_list[0].args, (4321, signal.SIGTERM))
            self.assertEqual(kill.call_args_list[1].args, (4321, signal.SIGKILL))

    def test_real_setsid_descendant_is_reaped_after_leader_sigkill(self):
        with test_tmpdir() as directory:
            pidfile = Path(directory) / "descendant.pid"
            child = "import os,time; from pathlib import Path; Path(" + repr(str(pidfile)) + ").write_text(str(os.getpid())); time.sleep(60)"
            leader = ("import os,signal,subprocess,sys,time; from pathlib import Path; "
                      "subprocess.Popen([sys.executable,'-c'," + repr(child) + "],start_new_session=True); "
                      "p=Path(" + repr(str(pidfile)) + "); "
                      "exec('while not p.exists(): time.sleep(0.01)'); "
                      "os.kill(os.getpid(),signal.SIGKILL)")
            with self.assertRaisesRegex(ProductError, "exit=-9"):
                verify.execute(verify.Stage("orphan", "test", (sys.executable, "-c", leader), 5), dict(os.environ))
            pid = int(pidfile.read_text())
            self.assertFalse(Path(f"/proc/{pid}").exists(), "setsid child survived its killed stage leader")

    def test_real_timeout_reaps_setsid_descendant(self):
        with test_tmpdir() as directory:
            pidfile = Path(directory) / "descendant.pid"
            child = "import os,time; from pathlib import Path; Path(" + repr(str(pidfile)) + ").write_text(str(os.getpid())); time.sleep(60)"
            leader = ("import subprocess,sys,time; "
                      "subprocess.Popen([sys.executable,'-c'," + repr(child) + "],start_new_session=True); time.sleep(60)")
            with self.assertRaisesRegex(ProductError, "type=timeout"):
                verify.execute(verify.Stage("timeout-orphan", "test", (sys.executable, "-c", leader), 1), dict(os.environ))
            self.assertFalse(Path(f"/proc/{int(pidfile.read_text())}").exists())

    def test_failure_stops_following_stages(self):
        with test_tmpdir() as directory, patch.object(verify, "state_root", return_value=Path(directory)), \
                patch.object(verify, "environment"), patch.object(verify, "whitespace"), \
                patch.object(verify, "execute", side_effect=ProductError("failed")) as execute:
            with self.assertRaises(ProductError):
                verify.verify_cmd(argparse.Namespace(tier="daily"))
            self.assertEqual(execute.call_count, 1)

    def test_changed_commit_base_missing_fails_instead_of_silently_omitting_check(self):
        with patch.object(verify.subprocess, "run", return_value=Mock(returncode=1)):
            with self.assertRaisesRegex(ProductError, "CI_DIFF_BASE"):
                verify.whitespace({"CI_DIFF_BASE": "unavailable"})

    def test_nonzero_stage_preserves_failure_category(self):
        with patch.object(verify.subprocess, "Popen", return_value=Mock(wait=Mock(return_value=2))):
            with self.assertRaisesRegex(ProductError, "type=build exit=2"):
                verify.execute(verify.Stage("build", "build", ("build",), 1), {})


class RunnerAvailabilityTests(unittest.TestCase):
    def test_only_online_idle_matching_runner_is_available(self):
        module = load_script_module("hardware_runner", "scripts/ci/check_hardware_runner.py")
        runner = {"status": "online", "busy": False, "labels": [{"name": name} for name in module.REQUIRED]}
        self.assertTrue(module.available([{"runners": []}, {"runners": [runner]}]))
        self.assertFalse(module.available([{"runners": [{**runner, "busy": True}]}]))
        self.assertFalse(module.available([{"runners": [{**runner, "status": "offline"}]}]))
        self.assertFalse(module.available([{"runners": [{**runner, "labels": [{"name": "linux"}]}]}]))

    def test_inaccessible_inventory_is_not_reported_as_pass(self):
        module = load_script_module("hardware_runner", "scripts/ci/check_hardware_runner.py")
        with test_tmpdir() as directory:
            output = Path(directory) / "output"
            summary = Path(directory) / "summary"
            with patch.dict(os.environ, {"GITHUB_REPOSITORY": "owner/repo", "GITHUB_OUTPUT": str(output), "GITHUB_STEP_SUMMARY": str(summary)}), \
                    patch.object(module.subprocess, "run", return_value=Mock(returncode=1, stdout="")):
                self.assertEqual(module.main(), 1)
            self.assertEqual(output.read_text(), "available=false\n")
            self.assertIn("NOT RUN", summary.read_text())


class BuildrootProvisioningTests(unittest.TestCase):
    def invoke(self, directory, source, env=None):
        return subprocess.run(["scripts/build-graphics-rootfs.sh", "--flavor", "q35-graphics-seatd",
                               "--buildroot-dir", str(source), "--fetch-buildroot", "--source-only",
                               "--output", str(directory / "output"), "--download-dir", str(directory / "downloads")],
                              cwd=verify.REPO_ROOT, env=env, capture_output=True, text=True)

    def test_existing_tarball_is_preserved_and_wrong_version_rejected(self):
        pin = dict(line.split("=", 1) for line in (verify.REPO_ROOT / "config/graphics/pins.env").read_text().splitlines() if "=" in line)["BUILDROOT_VERSION"]
        with test_tmpdir() as temporary:
            directory = Path(temporary)
            source = directory / "source"
            source.mkdir()
            makefile = source / "Makefile"
            content = f"BR2_VERSION := {pin}\n# local edit retained\ndefconfig olddefconfig source:\n\t@true\n"
            makefile.write_text(content)
            result = self.invoke(directory, source)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(makefile.read_text(), content)
            makefile.write_text(content.replace(pin, "0.0.0"))
            result = self.invoke(directory, source)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("version mismatch", result.stderr)
            self.assertIn("0.0.0", makefile.read_text())

    def test_fresh_clone_checks_out_release_tag(self):
        pins = dict(line.split("=", 1) for line in (verify.REPO_ROOT / "config/graphics/pins.env").read_text().splitlines() if "=" in line)
        with test_tmpdir() as temporary:
            directory = Path(temporary)
            upstream = directory / "upstream"
            upstream.mkdir()
            def git(*args):
                subprocess.run(["git", "-C", str(upstream), *args], check=True, capture_output=True)
            git("init")
            git("config", "user.email", "test@example.invalid")
            git("config", "user.name", "Test")
            (upstream / "Makefile").write_text(f"BR2_VERSION := {pins['BUILDROOT_VERSION']}\ndefconfig olddefconfig source:\n\t@true\n")
            git("add", "Makefile")
            git("commit", "-m", "release")
            git("tag", pins["BUILDROOT_VERSION"])
            (upstream / "Makefile").write_text("BR2_VERSION := wrong-head\n")
            git("commit", "-am", "development")
            env = {**os.environ, "GIT_CONFIG_COUNT": "1",
                   "GIT_CONFIG_KEY_0": f"url.{upstream.as_uri()}.insteadOf", "GIT_CONFIG_VALUE_0": pins["BUILDROOT_URL"]}
            source = directory / "checkout"
            result = self.invoke(directory, source, env)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn(pins["BUILDROOT_VERSION"], (source / "Makefile").read_text())


if __name__ == "__main__":
    unittest.main()
