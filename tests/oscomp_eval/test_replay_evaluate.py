from __future__ import annotations

import contextlib
import gzip
import io
import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from tools.oscomp_eval import cli
from tools.oscomp_eval.cli import evaluate_exit_code
from tools.oscomp_eval.replay import (
    _compact_score_for_replay,
    _prune_replay_intermediates,
    build_qemu_command,
    evaluate_replay,
    prepare_image,
    run_replay,
    score_with_extra_issues,
)
from tools.oscomp_eval.scoring import score_judge_summaries


class ReplayImageTests(unittest.TestCase):
    def test_prepare_image_uses_cache_for_compressed_image(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            raw = root / "sdcard-rv.img"
            raw.write_bytes(b"abc")
            compressed = root / "sdcard-rv.img.gz"
            with gzip.open(compressed, "wb") as output:
                output.write(raw.read_bytes())

            with patch.dict("os.environ", {"OSCOMP_IMAGE_CACHE_DIR": str(root / "cache")}):
                first = prepare_image(compressed, root=root)
                second = prepare_image(compressed, root=root)

            self.assertTrue(first.cached)
            self.assertEqual(first.runtime, second.runtime)
            self.assertEqual(first.runtime.read_bytes(), b"abc")
            self.assertEqual(list((root / "cache").glob("*/*")), [first.runtime])

    def test_build_qemu_command_uses_snapshot_and_readonly_drives(self) -> None:
        command = build_qemu_command(
            arch="rv",
            kernel=Path("kernel-rv"),
            image=Path("sdcard-rv.img"),
            support_image=Path("disk.img"),
        )

        self.assertIn("file=sdcard-rv.img,if=none,format=raw,id=x0,snapshot=on", command)
        self.assertIn("file=disk.img,if=none,format=raw,id=x1,readonly=on", command)


class ReplayRunTests(unittest.TestCase):
    def test_run_replay_converts_keyboard_interrupt_to_result(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "kernel-rv").write_bytes(b"kernel")
            image = root / "sdcard-rv.img"
            image.write_bytes(b"image")

            with patch("tools.oscomp_eval.replay.repo_root", return_value=root):
                with patch("tools.oscomp_eval.replay.subprocess.Popen", side_effect=KeyboardInterrupt):
                    result = run_replay(
                        arch="rv",
                        run_dir=root / "run",
                        image=image,
                        skip_kernel_build=True,
                    )

            self.assertEqual(result.returncode, 130)
            self.assertTrue(result.interrupted)

    def test_run_replay_converts_launch_error_to_result(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "kernel-rv").write_bytes(b"kernel")
            image = root / "sdcard-rv.img"
            image.write_bytes(b"image")

            with patch("tools.oscomp_eval.replay.repo_root", return_value=root):
                with patch(
                    "tools.oscomp_eval.replay.subprocess.Popen",
                    side_effect=FileNotFoundError("missing qemu"),
                ):
                    result = run_replay(
                        arch="rv",
                        run_dir=root / "run",
                        image=image,
                        skip_kernel_build=True,
                    )

            self.assertEqual(result.returncode, 3)
            self.assertFalse(result.ok)
            self.assertTrue(result.launch_failed)
            self.assertEqual(result.error_message, "replay launch failed: missing qemu")
            self.assertIn("missing qemu", result.log_path.read_text(encoding="utf-8"))


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

    def test_evaluate_replay_writes_artifacts_when_qemu_launch_fails(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            run_dir = root / "run"
            (root / "kernel-rv").write_bytes(b"kernel")
            image = root / "sdcard-rv.img"
            image.write_bytes(b"image")

            with patch("tools.oscomp_eval.replay.repo_root", return_value=root):
                with patch(
                    "tools.oscomp_eval.replay.subprocess.Popen",
                    side_effect=FileNotFoundError("missing qemu"),
                ):
                    result = evaluate_replay(
                        name="unit-launch-error",
                        arch="rv",
                        run_dir=run_dir,
                        image=image,
                        skip_kernel_build=True,
                        replace=True,
                    )

            self.assertEqual(result.status, "replay-error")
            self.assertEqual(evaluate_exit_code(result), 3)
            self.assertTrue((run_dir / "score.json").is_file())
            self.assertFalse((run_dir / "manifest.json").exists())
            self.assertFalse((run_dir / "artifact-index.json").exists())
            self.assertFalse((run_dir / "report.md").exists())
            score = json.loads((run_dir / "score.json").read_text())
            self.assertEqual(score["issues"][0]["error"], "replay launch failed: missing qemu")
            self.assertEqual(score["run"]["mode"], "replay")
            self.assertEqual(score["run"]["name"], "unit-launch-error")
            self.assertEqual(score["run"]["arches"], ["rv"])
            self.assertEqual(score["run"]["status"], "replay-error")

    def test_replay_compaction_keeps_only_qemu_log_and_score_inputs(self) -> None:
        from tools.oscomp_eval.schemas import ScoreSummary

        with tempfile.TemporaryDirectory() as tmp:
            run_dir = Path(tmp)
            rv_dir = run_dir / "rv"
            (rv_dir / "segments").mkdir(parents=True)
            (rv_dir / "judges").mkdir()
            for path in (
                rv_dir / "marker-validation.json",
                rv_dir / "segments.jsonl",
                rv_dir / "segments" / "basic-musl.txt",
                rv_dir / "judges" / "basic-musl.json",
                rv_dir / "judge-summary.json",
                rv_dir / "qemu.log",
            ):
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text("{}\n", encoding="utf-8")

            score = ScoreSummary(
                total_score=1.0,
                non_ltp_score=1.0,
                ltp_raw_total=0.0,
                ltp_score=0.0,
                arch_totals={},
                libc_totals={},
                ltp_group_totals={},
                group_totals={
                    "rv/basic-musl": {
                        "arch": "rv",
                        "group": "basic",
                        "group_id": "basic-musl",
                        "json_path": "rv/judges/basic-musl.json",
                    }
                },
                issues=(),
            )

            compacted = _compact_score_for_replay(score)
            _prune_replay_intermediates(run_dir, ("rv",))

            self.assertNotIn("json_path", compacted.group_totals["rv/basic-musl"])
            self.assertTrue((rv_dir / "qemu.log").is_file())
            self.assertFalse((rv_dir / "marker-validation.json").exists())
            self.assertFalse((rv_dir / "segments.jsonl").exists())
            self.assertFalse((rv_dir / "segments").exists())
            self.assertFalse((rv_dir / "judges").exists())
            self.assertFalse((rv_dir / "judge-summary.json").exists())

    def test_evaluate_replay_runs_both_arches_in_parallel(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            run_dir = root / "run"
            for arch, kernel_name, image_name in (
                ("rv", "kernel-rv", "sdcard-rv.img"),
                ("la", "kernel-la", "sdcard-la.img"),
            ):
                (root / kernel_name).write_bytes(b"kernel")
                (root / image_name).write_bytes(b"image")

            calls: list[str] = []

            def fake_replay_and_judge(*, selected_arch, **kwargs):
                calls.append(selected_arch)
                from tools.oscomp_eval.replay import ReplayResult, _ArchReplayOutcome

                return _ArchReplayOutcome(
                    replay=ReplayResult(
                        arch=selected_arch,
                        command=("qemu", selected_arch),
                        returncode=0,
                        duration_ms=1,
                        log_path=run_dir / selected_arch / "qemu.log",
                        workdir=run_dir / selected_arch / "work",
                    ),
                    judge_summary={
                        "schema": "oscomp-eval.judge-summary.v1",
                        "arch": selected_arch,
                        "results": [],
                    },
                    replay_issue=None,
                )

            with patch("tools.oscomp_eval.replay.repo_root", return_value=root):
                with patch(
                    "tools.oscomp_eval.replay._replay_and_judge_arch",
                    side_effect=fake_replay_and_judge,
                ):
                    result = evaluate_replay(
                        name="unit-parallel",
                        arch="both",
                        run_dir=run_dir,
                        skip_kernel_build=True,
                        replace=True,
                    )

            self.assertEqual(sorted(calls), ["la", "rv"])
            self.assertEqual(len(result.replays), 2)
            score = json.loads((run_dir / "score.json").read_text())
            self.assertEqual(score["run"]["arches"], ["rv", "la"])


class CliTests(unittest.TestCase):
    def test_evaluate_cmd_uses_full_replay_timeout_by_default(self) -> None:
        from argparse import Namespace
        from tools.oscomp_eval.config import REPLAY_TIMEOUT_FULL_SECS

        captured: dict[str, object] = {}

        def fake_evaluate_replay(**kwargs):
            captured.update(kwargs)
            from tools.oscomp_eval.replay import ReplayRunResult
            from tools.oscomp_eval.schemas import ScoreSummary

            score = ScoreSummary(
                total_score=0.0,
                non_ltp_score=0.0,
                ltp_raw_total=0.0,
                ltp_score=0.0,
                arch_totals={},
                libc_totals={},
                ltp_group_totals={},
                group_totals={},
                issues=(),
            )
            return ReplayRunResult(
                run_dir=Path("."),
                replays=(),
                judge_summaries=(),
                score=score,
                status="complete",
            )

        args = Namespace(
            rv_log=None,
            la_log=None,
            ltp_list=None,
            support_image=None,
            arch="rv",
            timeout=None,
            idle_timeout=None,
            image=None,
            plan=None,
            skip_kernel_build=True,
            name="unit-timeout-default",
            out=None,
            judge_dir=None,
            judge_timeout=30,
            fail_fast=False,
            replace=False,
            verbose=False,
        )

        with patch("tools.oscomp_eval.cli.evaluate_replay", side_effect=fake_evaluate_replay):
            self.assertEqual(cli.evaluate_cmd(args), 0)

        self.assertEqual(captured["timeout_secs"], REPLAY_TIMEOUT_FULL_SECS)

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
