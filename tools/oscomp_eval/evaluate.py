"""Full local evaluation orchestration."""

from __future__ import annotations

from concurrent.futures import ThreadPoolExecutor, as_completed
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path

from .config import (
    Libc,
    effective_group_libc_matrix,
    expand_arches,
    expected_matrix_to_json,
    group_libc_matrix_to_json,
)
from .judge_runner import judge_log
from .markers import write_json
from .paths import create_run_dir, prepare_run_dir
from .report import generate_report
from .replay import ReplayResult, run_replay
from .run_inputs import capture_input_file
from .run_metadata import common_manifest_fields
from .schemas import RUN_MANIFEST_SCHEMA, JudgeSummary, ScoreSummary
from .scoring import score_judge_summaries, write_score_summary
from .support_image import SupportImageBuild, build_support_image


@dataclass(frozen=True)
class EvaluateResult:
    run_dir: Path
    replays: tuple[ReplayResult, ...]
    judge_summaries: tuple[JudgeSummary, ...]
    score: ScoreSummary
    status: str
    support_image_build: SupportImageBuild | None = None

    @property
    def replay_failures(self) -> int:
        return sum(1 for replay in self.replays if not replay.ok)

    @property
    def timed_out(self) -> bool:
        return any(replay.timed_out for replay in self.replays)

    @property
    def interrupted(self) -> bool:
        return any(replay.interrupted for replay in self.replays)


def score_with_extra_issues(
    score: ScoreSummary,
    extra_issues: list[dict[str, object]],
) -> ScoreSummary:
    if not extra_issues:
        return score
    return ScoreSummary(
        total_score=score.total_score,
        non_ltp_score=score.non_ltp_score,
        ltp_raw_total=score.ltp_raw_total,
        ltp_score=score.ltp_score,
        arch_totals=score.arch_totals,
        libc_totals=score.libc_totals,
        ltp_group_totals=score.ltp_group_totals,
        group_totals=score.group_totals,
        issues=score.issues + tuple(extra_issues),
    )


def evaluate_run_status(
    replays: tuple[ReplayResult, ...],
    score: ScoreSummary,
) -> str:
    if any(replay.interrupted for replay in replays):
        return "interrupted"
    if any(replay.timed_out for replay in replays):
        return "timeout"
    if any(not replay.ok for replay in replays):
        return "replay-error"
    if score.has_errors:
        return "incomplete"
    return "complete"


def evaluate_replay(
    *,
    name: str,
    arch: str = "both",
    run_dir: Path | None = None,
    timeout_secs: int | None = None,
    idle_timeout_secs: int | None = None,
    image: Path | None = None,
    support_image: Path | None = None,
    ltp_list: Path | None = None,
    plan_path: Path | None = None,
    skip_kernel_build: bool = False,
    keep_workdir: bool = False,
    judge_dir: Path | None = None,
    judge_timeout_secs: float = 30,
    fail_fast: bool = False,
    replace: bool = False,
    command: list[str] | None = None,
    replay_runner_path: Path | None = None,
    group_libc_matrix: tuple[tuple[str, Libc], ...] | None = None,
) -> EvaluateResult:
    if run_dir is None:
        run_dir = create_run_dir(name, replace=replace)
    else:
        run_dir = prepare_run_dir(run_dir, replace=replace)

    arches = expand_arches(arch)
    effective_matrix = effective_group_libc_matrix(group_libc_matrix)
    if support_image is not None and ltp_list is not None:
        raise ValueError("--support-image and --ltp-list cannot be combined")
    if ltp_list is not None and not ltp_list.is_file():
        raise ValueError(f"ltp list does not exist: {ltp_list}")
    if plan_path is not None and not plan_path.is_file():
        raise ValueError(f"plan does not exist: {plan_path}")

    replays: list[ReplayResult] = []
    judge_summaries: list[JudgeSummary] = []
    replay_issues: list[dict[str, object]] = []
    inputs: dict[str, str] = {}
    if image is not None:
        inputs["image"] = str(image)
    captured_plan_path: Path | None = None
    captured_ltp_list_path: Path | None = None
    if ltp_list is not None:
        inputs["ltp_list"] = str(ltp_list)
        captured_ltp_list = capture_input_file(
            run_dir=run_dir,
            source=ltp_list,
            name="ltp_test.txt",
        )
        inputs["captured_ltp_list"] = captured_ltp_list
        captured_ltp_list_path = run_dir / captured_ltp_list
    if plan_path is not None:
        inputs["plan"] = str(plan_path)
        captured_plan = capture_input_file(
            run_dir=run_dir,
            source=plan_path,
            name="plan.txt",
        )
        inputs["captured_plan"] = captured_plan
        captured_plan_path = run_dir / captured_plan
    if judge_dir is not None:
        inputs["judge_dir"] = str(judge_dir)
    if replay_runner_path is not None:
        inputs["replay_runner"] = str(replay_runner_path)

    support_image_build: SupportImageBuild | None = None
    if ltp_list is not None:
        support_arch = "both" if len(arches) > 1 else arches[0]
        support_image_build = build_support_image(
            arch=support_arch,
            run_dir=run_dir,
            ltp_list=captured_ltp_list_path or ltp_list,
            plan=captured_plan_path,
        )
        support_image = support_image_build.output_path
    if support_image is not None:
        inputs["support_image"] = str(support_image)

    replay_concurrency = len(arches) if len(arches) > 1 and not fail_fast else 1
    if replay_concurrency > 1:
        replay_by_arch: dict[str, ReplayResult] = {}
        with ThreadPoolExecutor(max_workers=replay_concurrency) as executor:
            future_by_arch = {
                executor.submit(
                    run_replay,
                    arch=selected_arch,
                    run_dir=run_dir,
                    timeout_secs=timeout_secs,
                    idle_timeout_secs=idle_timeout_secs,
                    image=image,
                    support_image=support_image,
                    skip_kernel_build=skip_kernel_build,
                    keep_workdir=keep_workdir,
                    runner_path=replay_runner_path,
                ): selected_arch
                for selected_arch in arches
            }
            for future in as_completed(future_by_arch):
                selected_arch = future_by_arch[future]
                replay_by_arch[selected_arch] = future.result()
        replay_results = [replay_by_arch[selected_arch] for selected_arch in arches]
    else:
        replay_results = []
        for selected_arch in arches:
            replay = run_replay(
                arch=selected_arch,
                run_dir=run_dir,
                timeout_secs=timeout_secs,
                idle_timeout_secs=idle_timeout_secs,
                image=image,
                support_image=support_image,
                skip_kernel_build=skip_kernel_build,
                keep_workdir=keep_workdir,
                runner_path=replay_runner_path,
            )
            replay_results.append(replay)
            if fail_fast and not replay.ok:
                break

    for replay in replay_results:
        selected_arch = replay.arch
        replays.append(replay)
        if not replay.ok:
            issue: dict[str, object] = {
                "kind": "replay-status",
                "arch": selected_arch,
                "returncode": replay.returncode,
                "log_path": str(replay.log_path),
            }
            if replay.error_message is not None:
                issue["error"] = replay.error_message
            replay_issues.append(issue)
            if fail_fast:
                break

        if replay.log_path.is_file() and not replay.launch_failed:
            summary = judge_log(
                log_path=replay.log_path,
                arch=selected_arch,
                out_dir=run_dir / selected_arch,
                judge_dir=judge_dir,
                judge_timeout_secs=judge_timeout_secs,
                fail_fast=fail_fast,
                group_libc_matrix=effective_matrix,
            )
            judge_summaries.append(summary)

    score = score_judge_summaries(judge_summaries)
    score = score_with_extra_issues(score, replay_issues)
    status = evaluate_run_status(tuple(replays), score)
    write_score_summary(score, run_dir / "score.json")
    manifest = {
        "schema": RUN_MANIFEST_SCHEMA,
        "created_at": datetime.now(timezone.utc).isoformat(),
        "name": name,
        "mode": "evaluate-replay",
        "status": status,
        "arches": list(arches),
        "inputs": inputs,
        "timeout_secs": timeout_secs,
        "idle_timeout_secs": idle_timeout_secs,
        "judge_timeout_secs": judge_timeout_secs,
        "replay_concurrency": replay_concurrency,
        "fail_fast": fail_fast,
        "skip_kernel_build": skip_kernel_build,
        "keep_workdir": keep_workdir,
        "replays": [replay.to_json_dict(base_dir=run_dir) for replay in replays],
        "group_libc_matrix": group_libc_matrix_to_json(effective_matrix),
        "expected_matrix": expected_matrix_to_json(arches, effective_matrix),
    }
    if support_image_build is not None:
        manifest["support_image_build"] = support_image_build.to_json_dict()
    manifest.update(common_manifest_fields(command))
    write_json(run_dir / "manifest.json", manifest)
    generate_report(run_dir)
    return EvaluateResult(
        run_dir=run_dir,
        replays=tuple(replays),
        judge_summaries=tuple(judge_summaries),
        score=score,
        status=status,
        support_image_build=support_image_build,
    )
