from __future__ import annotations

import tempfile
import unittest
import json
import contextlib
import io
from pathlib import Path
from unittest.mock import patch

from tools.oscomp_eval import cli
from tools.oscomp_eval.evaluate import evaluate_replay, score_with_extra_issues
from tools.oscomp_eval.replay import run_replay
from tools.oscomp_eval.scoring import score_judge_summaries
from tools.oscomp_eval.cli import evaluate_exit_code


class ReplayTests(unittest.TestCase):
    def test_run_replay_passes_log_and_workdir_to_runner(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            runner = root / "runner.sh"
            runner.write_text(
                "#!/usr/bin/env bash\n"
                "set -euo pipefail\n"
                "log=\"\"\n"
                "while (($#)); do\n"
                "  case \"$1\" in\n"
                "    --log) log=\"$2\"; shift 2 ;;\n"
                "    *) shift ;;\n"
                "  esac\n"
                "done\n"
                "mkdir -p \"$(dirname \"$log\")\"\n"
                "printf '%s\\n' '#### OS COMP TEST GROUP START basic-musl ####' > \"$log\"\n"
                "printf '%s\\n' 'body' >> \"$log\"\n"
                "printf '%s\\n' '#### OS COMP TEST GROUP END basic-musl ####' >> \"$log\"\n",
                encoding="utf-8",
            )
            runner.chmod(0o755)

            result = run_replay(
                arch="rv",
                run_dir=root / "run",
                runner_path=runner,
                skip_kernel_build=True,
            )

            self.assertEqual(result.returncode, 0)
            self.assertTrue(result.log_path.is_file())
            self.assertIn("--log", result.command)
            self.assertIn("basic-musl", result.log_path.read_text())
            self.assertEqual(
                result.to_json_dict(base_dir=root / "run")["log_relpath"],
                "rv/console.log",
            )

    def test_run_replay_converts_keyboard_interrupt_to_result(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            runner = root / "runner.sh"
            runner.write_text("#!/usr/bin/env bash\nsleep 100\n", encoding="utf-8")
            runner.chmod(0o755)

            with patch("tools.oscomp_eval.replay.subprocess.Popen", side_effect=KeyboardInterrupt):
                result = run_replay(
                    arch="rv",
                    run_dir=root / "run",
                    runner_path=runner,
                    skip_kernel_build=True,
                )

            self.assertEqual(result.returncode, 130)
            self.assertTrue(result.interrupted)

    def test_run_replay_converts_launch_error_to_result(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            runner = root / "missing-runner.sh"

            with patch(
                "tools.oscomp_eval.replay.subprocess.Popen",
                side_effect=FileNotFoundError("missing runner"),
            ):
                result = run_replay(
                    arch="rv",
                    run_dir=root / "run",
                    runner_path=runner,
                    skip_kernel_build=True,
                )

            self.assertEqual(result.returncode, 3)
            self.assertFalse(result.ok)
            self.assertTrue(result.launch_failed)
            self.assertEqual(result.error_message, "replay launch failed: missing runner")
            self.assertIn(
                "replay launch failed: missing runner",
                result.log_path.read_text(encoding="utf-8"),
            )
            self.assertEqual(
                result.to_json_dict(base_dir=root / "run")["error"],
                "replay launch failed: missing runner",
            )

    def test_run_replay_converts_idle_console_to_timeout_result(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            runner = root / "idle-runner.py"
            runner.write_text(
                "#!/usr/bin/env python3\n"
                "from pathlib import Path\n"
                "import sys\n"
                "import time\n"
                "args = sys.argv[1:]\n"
                "log_path = Path(args[args.index('--log') + 1])\n"
                "log_path.parent.mkdir(parents=True, exist_ok=True)\n"
                "log_path.write_text('booted\\n', encoding='utf-8')\n"
                "time.sleep(30)\n",
                encoding="utf-8",
            )
            runner.chmod(0o755)

            result = run_replay(
                arch="rv",
                run_dir=root / "run",
                runner_path=runner,
                skip_kernel_build=True,
                idle_timeout_secs=1,
            )

            self.assertEqual(result.returncode, 124)
            self.assertTrue(result.timed_out)
            self.assertEqual(
                result.error_message,
                "replay idle timeout after 1s without console output",
            )
            self.assertIn(
                "replay idle timeout after 1s without console output",
                result.log_path.read_text(encoding="utf-8"),
            )


class EvaluateHelpersTests(unittest.TestCase):
    def test_score_with_extra_issues_preserves_scores(self) -> None:
        score = score_judge_summaries(
            [
                {
                    "schema": "oscomp-eval.judge-summary.v1",
                    "arch": "rv",
                    "results": [],
                }
            ]
        )

        updated = score_with_extra_issues(
            score,
            [{"kind": "replay-status", "arch": "rv", "returncode": 1}],
        )

        self.assertEqual(updated.total_score, score.total_score)
        self.assertEqual(updated.issues[-1]["kind"], "replay-status")

    def test_evaluate_replay_manifest_records_metadata_on_replay_failure(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            run_dir = root / "run"
            runner = root / "fail-runner.sh"
            runner.write_text(
                "#!/usr/bin/env bash\n"
                "set -euo pipefail\n"
                "exit 7\n",
                encoding="utf-8",
            )
            runner.chmod(0o755)

            result = evaluate_replay(
                name="unit-evaluate",
                arch="rv",
                run_dir=run_dir,
                skip_kernel_build=True,
                image=root / "sdcard-rv.img",
                support_image=root / "support-rv.img",
                idle_timeout_secs=7,
                judge_dir=root / "judge",
                judge_timeout_secs=0.25,
                fail_fast=True,
                replace=True,
                command=["tools.oscomp_eval", "evaluate", "--arch", "rv"],
                replay_runner_path=runner,
            )

            self.assertEqual(result.replay_failures, 1)
            self.assertEqual(result.status, "replay-error")
            manifest = json.loads((run_dir / "manifest.json").read_text())
            self.assertEqual(manifest["mode"], "evaluate-replay")
            self.assertEqual(manifest["status"], "replay-error")
            self.assertIn("created_at", manifest)
            self.assertIn("git", manifest)
            self.assertIn("official_snapshot", manifest)
            self.assertEqual(
                manifest["command"],
                ["tools.oscomp_eval", "evaluate", "--arch", "rv"],
            )
            self.assertEqual(manifest["inputs"]["image"], str(root / "sdcard-rv.img"))
            self.assertEqual(
                manifest["inputs"]["support_image"],
                str(root / "support-rv.img"),
            )
            self.assertEqual(manifest["idle_timeout_secs"], 7)
            self.assertEqual(manifest["inputs"]["judge_dir"], str(root / "judge"))
            self.assertEqual(manifest["inputs"]["replay_runner"], str(runner))
            self.assertEqual(manifest["judge_timeout_secs"], 0.25)
            self.assertTrue(manifest["fail_fast"])
            self.assertEqual(manifest["replays"][0]["log_relpath"], "rv/console.log")
            self.assertEqual(
                manifest["replays"][0]["workdir_relpath"],
                "rv/replay-workdir",
            )
            self.assertEqual(len(manifest["group_libc_matrix"]), 21)
            self.assertEqual(len(manifest["expected_matrix"]), 21)
            self.assertIn(
                {
                    "arch": "rv",
                    "group": "basic",
                    "libc": "musl",
                    "group_id": "basic-musl",
                    "key": "rv/basic-musl",
                },
                manifest["expected_matrix"],
            )
            self.assertEqual(evaluate_exit_code(result), 3)

    def test_evaluate_exit_code_preserves_replay_interrupt(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            run_dir = root / "run"
            runner = root / "interrupt-runner.sh"
            runner.write_text(
                "#!/usr/bin/env bash\n"
                "exit 130\n",
                encoding="utf-8",
            )
            runner.chmod(0o755)

            result = evaluate_replay(
                name="unit-interrupt",
                arch="rv",
                run_dir=run_dir,
                skip_kernel_build=True,
                fail_fast=True,
                replace=True,
                replay_runner_path=runner,
            )

            self.assertTrue(result.replays[0].interrupted)
            self.assertEqual(result.status, "interrupted")
            self.assertEqual(evaluate_exit_code(result), 130)
            manifest = json.loads((run_dir / "manifest.json").read_text())
            self.assertEqual(manifest["status"], "interrupted")
            self.assertTrue(manifest["replays"][0]["interrupted"])

    def test_evaluate_exit_code_preserves_replay_timeout(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            run_dir = root / "run"
            runner = root / "timeout-runner.sh"
            runner.write_text("#!/usr/bin/env bash\nexit 124\n", encoding="utf-8")
            runner.chmod(0o755)

            result = evaluate_replay(
                name="unit-timeout",
                arch="rv",
                run_dir=run_dir,
                skip_kernel_build=True,
                fail_fast=True,
                replace=True,
                replay_runner_path=runner,
            )

            self.assertTrue(result.replays[0].timed_out)
            self.assertEqual(result.status, "timeout")
            self.assertEqual(evaluate_exit_code(result), 124)
            manifest = json.loads((run_dir / "manifest.json").read_text())
            self.assertEqual(manifest["status"], "timeout")

    def test_evaluate_replay_writes_artifacts_when_replay_is_interrupted(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            run_dir = root / "run"
            runner = root / "runner.sh"
            runner.write_text("#!/usr/bin/env bash\nsleep 100\n", encoding="utf-8")
            runner.chmod(0o755)

            with patch("tools.oscomp_eval.replay.subprocess.Popen", side_effect=KeyboardInterrupt):
                with patch(
                    "tools.oscomp_eval.evaluate.common_manifest_fields",
                    return_value={
                        "command": ["tools.oscomp_eval", "evaluate", "--arch", "rv"]
                    },
                ):
                    result = evaluate_replay(
                        name="unit-keyboard-interrupt",
                        arch="rv",
                        run_dir=run_dir,
                        skip_kernel_build=True,
                        replace=True,
                        replay_runner_path=runner,
                        command=["tools.oscomp_eval", "evaluate", "--arch", "rv"],
                    )

            self.assertEqual(evaluate_exit_code(result), 130)
            self.assertEqual(result.status, "interrupted")
            self.assertTrue((run_dir / "manifest.json").is_file())
            self.assertTrue((run_dir / "score.json").is_file())
            self.assertTrue((run_dir / "artifact-index.json").is_file())
            self.assertTrue((run_dir / "report.md").is_file())
            manifest = json.loads((run_dir / "manifest.json").read_text())
            self.assertEqual(manifest["status"], "interrupted")
            self.assertTrue(manifest["replays"][0]["interrupted"])
            artifact_index = json.loads((run_dir / "artifact-index.json").read_text())
            artifact_paths = {
                artifact["path"] for artifact in artifact_index["artifacts"]
            }
            self.assertIn("manifest.json", artifact_paths)
            self.assertIn("score.json", artifact_paths)
            self.assertIn("report.md", artifact_paths)

    def test_evaluate_replay_writes_artifacts_when_replay_launch_fails(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            run_dir = root / "run"
            runner = root / "missing-runner.sh"

            with patch(
                "tools.oscomp_eval.replay.subprocess.Popen",
                side_effect=FileNotFoundError("missing runner"),
            ):
                result = evaluate_replay(
                    name="unit-launch-error",
                    arch="rv",
                    run_dir=run_dir,
                    skip_kernel_build=True,
                    replace=True,
                    replay_runner_path=runner,
                    command=["tools.oscomp_eval", "evaluate", "--arch", "rv"],
                )

            self.assertEqual(result.status, "replay-error")
            self.assertEqual(evaluate_exit_code(result), 3)
            self.assertTrue((run_dir / "manifest.json").is_file())
            self.assertTrue((run_dir / "score.json").is_file())
            self.assertTrue((run_dir / "artifact-index.json").is_file())
            self.assertTrue((run_dir / "report.md").is_file())
            manifest = json.loads((run_dir / "manifest.json").read_text())
            self.assertEqual(manifest["status"], "replay-error")
            self.assertEqual(
                manifest["replays"][0]["error"],
                "replay launch failed: missing runner",
            )
            self.assertTrue(manifest["replays"][0]["launch_failed"])
            self.assertEqual(manifest["replays"][0]["returncode"], 3)
            score = json.loads((run_dir / "score.json").read_text())
            self.assertEqual(score["issues"][0]["error"], "replay launch failed: missing runner")
            self.assertIn(
                "replay launch failed: missing runner",
                (run_dir / "rv" / "console.log").read_text(encoding="utf-8"),
            )
            artifact_index = json.loads((run_dir / "artifact-index.json").read_text())
            artifact_paths = {
                artifact["path"] for artifact in artifact_index["artifacts"]
            }
            self.assertIn("rv/console.log", artifact_paths)

    def test_evaluate_replay_builds_support_image_for_ltp_list(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            run_dir = root / "run"
            ltp_list = root / "ltp_test.txt"
            plan = root / "plan.txt"
            ltp_list.write_text("fork06\n", encoding="utf-8")
            plan.write_text("/glibc ltp\n", encoding="utf-8")
            runner = root / "fail-runner.sh"
            runner.write_text("#!/usr/bin/env bash\nexit 7\n", encoding="utf-8")
            runner.chmod(0o755)

            def fake_build(**kwargs):
                self.assertEqual(kwargs["ltp_list"], kwargs["run_dir"] / "inputs" / "ltp_test.txt")
                self.assertEqual(kwargs["plan"], kwargs["run_dir"] / "inputs" / "plan.txt")
                output = kwargs["run_dir"] / "inputs" / "support-rv.img"
                output.parent.mkdir(parents=True, exist_ok=True)
                output.write_bytes(b"support")
                from tools.oscomp_eval.support_image import SupportImageBuild

                return SupportImageBuild(
                    arch=kwargs["arch"],
                    command=("build-support", "--test-list", str(kwargs["ltp_list"])),
                    returncode=0,
                    duration_ms=1,
                    output_path=output,
                    ltp_list=kwargs["ltp_list"],
                    plan=kwargs["plan"],
                )

            with patch("tools.oscomp_eval.evaluate.build_support_image", side_effect=fake_build):
                result = evaluate_replay(
                    name="unit-ltp-list",
                    arch="rv",
                    run_dir=run_dir,
                    ltp_list=ltp_list,
                    plan_path=plan,
                    skip_kernel_build=True,
                    replace=True,
                    replay_runner_path=runner,
                    group_libc_matrix=(("ltp", "glibc"),),
                )

            manifest = json.loads((run_dir / "manifest.json").read_text())
            self.assertEqual(result.status, "replay-error")
            self.assertEqual(manifest["status"], "replay-error")
            self.assertEqual(result.support_image_build.output_path, run_dir / "inputs" / "support-rv.img")
            self.assertEqual(manifest["inputs"]["ltp_list"], str(ltp_list))
            self.assertEqual(manifest["inputs"]["captured_ltp_list"], "inputs/ltp_test.txt")
            self.assertEqual(manifest["inputs"]["plan"], str(plan))
            self.assertEqual(manifest["inputs"]["captured_plan"], "inputs/plan.txt")
            self.assertEqual(
                (run_dir / "inputs" / "ltp_test.txt").read_text(encoding="utf-8"),
                "fork06\n",
            )
            self.assertEqual(
                (run_dir / "inputs" / "plan.txt").read_text(encoding="utf-8"),
                "/glibc ltp\n",
            )
            self.assertEqual(
                manifest["inputs"]["support_image"],
                str(run_dir / "inputs" / "support-rv.img"),
            )
            self.assertEqual(manifest["support_image_build"]["arch"], "rv")
            self.assertEqual(
                manifest["support_image_build"]["ltp_list"],
                str(run_dir / "inputs" / "ltp_test.txt"),
            )
            self.assertEqual(
                manifest["support_image_build"]["plan"],
                str(run_dir / "inputs" / "plan.txt"),
            )
            self.assertIn("--support-image", manifest["replays"][0]["command"])
            self.assertEqual(
                manifest["group_libc_matrix"],
                [{"group": "ltp", "libc": "glibc"}],
            )
            self.assertEqual(
                manifest["expected_matrix"],
                [
                    {
                        "arch": "rv",
                        "group": "ltp",
                        "libc": "glibc",
                        "group_id": "ltp-glibc",
                        "key": "rv/ltp-glibc",
                    }
                ],
            )

    def test_evaluate_replay_runs_multiple_arches_in_parallel(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            run_dir = root / "run"
            event_dir = root / "events"
            runner = root / "parallel-runner.py"
            runner.write_text(
                "#!/usr/bin/env python3\n"
                "from pathlib import Path\n"
                "import sys\n"
                "import time\n"
                f"event_dir = Path({str(event_dir)!r})\n"
                "args = sys.argv[1:]\n"
                "arch = args[args.index('--arch') + 1]\n"
                "log_path = Path(args[args.index('--log') + 1])\n"
                "event_dir.mkdir(parents=True, exist_ok=True)\n"
                "(event_dir / f'{arch}-start').write_text(str(time.monotonic()), encoding='utf-8')\n"
                "time.sleep(0.4)\n"
                "log_path.parent.mkdir(parents=True, exist_ok=True)\n"
                "log_path.write_text(\n"
                "    '#### OS COMP TEST GROUP START basic-musl ####\\n'\n"
                "    'body\\n'\n"
                "    '#### OS COMP TEST GROUP END basic-musl ####\\n',\n"
                "    encoding='utf-8',\n"
                ")\n"
                "(event_dir / f'{arch}-end').write_text(str(time.monotonic()), encoding='utf-8')\n",
                encoding="utf-8",
            )
            runner.chmod(0o755)
            judge_dir = root / "judge"
            judge_dir.mkdir()
            judge = judge_dir / "judge_basic-musl.py"
            judge.write_text("#!/usr/bin/env python3\nprint('[]')\n", encoding="utf-8")
            judge.chmod(0o755)

            result = evaluate_replay(
                name="unit-parallel-both",
                arch="both",
                run_dir=run_dir,
                skip_kernel_build=True,
                replace=True,
                replay_runner_path=runner,
                judge_dir=judge_dir,
                group_libc_matrix=(("basic", "musl"),),
            )

            self.assertEqual([replay.arch for replay in result.replays], ["rv", "la"])
            self.assertEqual(result.status, "complete")
            manifest = json.loads((run_dir / "manifest.json").read_text())
            self.assertEqual(manifest["replay_concurrency"], 2)
            starts = [
                float((event_dir / f"{arch}-start").read_text(encoding="utf-8"))
                for arch in ("rv", "la")
            ]
            ends = [
                float((event_dir / f"{arch}-end").read_text(encoding="utf-8"))
                for arch in ("rv", "la")
            ]
            self.assertLess(max(starts), min(ends))


class CliTests(unittest.TestCase):
    def test_main_returns_130_on_keyboard_interrupt(self) -> None:
        class Args:
            @staticmethod
            def func(_args):
                raise KeyboardInterrupt

        class Parser:
            @staticmethod
            def parse_args(_argv):
                return Args()

        stderr = io.StringIO()
        with patch.object(cli, "build_parser", return_value=Parser()):
            with contextlib.redirect_stderr(stderr):
                self.assertEqual(cli.main(["evaluate"]), 130)
        self.assertIn("interrupted", stderr.getvalue())

    def test_main_returns_4_on_internal_error_with_traceback(self) -> None:
        class Args:
            @staticmethod
            def func(_args):
                raise RuntimeError("boom")

        class Parser:
            @staticmethod
            def parse_args(_argv):
                return Args()

        stderr = io.StringIO()
        with patch.object(cli, "build_parser", return_value=Parser()):
            with contextlib.redirect_stderr(stderr):
                self.assertEqual(cli.main(["evaluate"]), 4)
        self.assertIn("Traceback", stderr.getvalue())
        self.assertIn("RuntimeError: boom", stderr.getvalue())

    def test_missing_ltp_list_is_cli_usage_error(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            stderr = io.StringIO()
            with contextlib.redirect_stderr(stderr):
                result = cli.main(
                    [
                        "evaluate",
                        "--arch",
                        "rv",
                        "--ltp-list",
                        str(root / "missing-ltp.txt"),
                        "--out",
                        str(root / "run"),
                        "--skip-kernel-build",
                    ]
                )

        self.assertEqual(result, 2)
        self.assertIn("ltp list does not exist", stderr.getvalue())


if __name__ == "__main__":
    unittest.main()
