"""Offline score runs from existing RV/LA console logs."""

from __future__ import annotations

from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path

from .config import (
    Arch,
    Libc,
    effective_group_libc_matrix,
    expected_matrix_to_json,
    group_libc_matrix_to_json,
)
from .judge_runner import judge_log
from .markers import write_json
from .paths import create_run_dir, prepare_run_dir
from .report import generate_report
from .run_inputs import capture_input_file
from .run_metadata import common_manifest_fields
from .scoring import score_judge_summaries, write_score_summary
from .schemas import RUN_MANIFEST_SCHEMA, JudgeSummary, ScoreSummary


@dataclass(frozen=True)
class ScoreLogsResult:
    run_dir: Path
    judge_summaries: tuple[JudgeSummary, ...]
    score: ScoreSummary
    status: str


def score_logs_status(score: ScoreSummary) -> str:
    return "incomplete" if score.has_errors else "complete"


def _manifest(
    *,
    name: str,
    arches: tuple[Arch, ...],
    inputs: dict[str, str],
    status: str,
    judge_timeout_secs: float,
    fail_fast: bool,
    command: list[str] | None,
    group_libc_matrix: tuple[tuple[str, Libc], ...] | None,
) -> dict[str, object]:
    manifest = {
        "schema": RUN_MANIFEST_SCHEMA,
        "created_at": datetime.now(timezone.utc).isoformat(),
        "name": name,
        "mode": "score-logs",
        "status": status,
        "inputs": inputs,
        "judge_timeout_secs": judge_timeout_secs,
        "fail_fast": fail_fast,
    }
    effective_matrix = effective_group_libc_matrix(group_libc_matrix)
    manifest["group_libc_matrix"] = group_libc_matrix_to_json(effective_matrix)
    manifest["expected_matrix"] = expected_matrix_to_json(arches, effective_matrix)
    manifest.update(common_manifest_fields(command))
    return manifest


def score_logs(
    *,
    name: str,
    run_dir: Path | None = None,
    rv_log: Path | None = None,
    la_log: Path | None = None,
    judge_dir: Path | None = None,
    judge_timeout_secs: float = 30,
    fail_fast: bool = False,
    replace: bool = False,
    command: list[str] | None = None,
    group_libc_matrix: tuple[tuple[str, Libc], ...] | None = None,
    plan_path: Path | None = None,
) -> ScoreLogsResult:
    if rv_log is None and la_log is None:
        raise ValueError("score-logs requires at least one of rv_log or la_log")

    if run_dir is None:
        run_dir = create_run_dir(name, replace=replace)
    else:
        run_dir = prepare_run_dir(run_dir, replace=replace)

    inputs: dict[str, str] = {}
    if plan_path is not None:
        inputs["plan"] = str(plan_path)
        inputs["captured_plan"] = capture_input_file(
            run_dir=run_dir,
            source=plan_path,
            name="plan.txt",
        )
    arches: list[Arch] = []
    judge_summaries: list[JudgeSummary] = []
    for arch, log_path in (("rv", rv_log), ("la", la_log)):
        if log_path is None:
            continue
        arches.append(arch)
        inputs[f"{arch}_log"] = str(log_path)
        arch_dir = run_dir / arch
        summary = judge_log(
            log_path=log_path,
            arch=arch,
            out_dir=arch_dir,
            judge_dir=judge_dir,
            judge_timeout_secs=judge_timeout_secs,
            fail_fast=fail_fast,
            group_libc_matrix=group_libc_matrix,
        )
        judge_summaries.append(summary)

    score = score_judge_summaries(judge_summaries)
    status = score_logs_status(score)
    write_score_summary(score, run_dir / "score.json")
    write_json(
        run_dir / "manifest.json",
        _manifest(
            name=name,
            arches=tuple(arches),
            inputs=inputs,
            status=status,
            judge_timeout_secs=judge_timeout_secs,
            fail_fast=fail_fast,
            command=command,
            group_libc_matrix=group_libc_matrix,
        ),
    )
    generate_report(run_dir)
    return ScoreLogsResult(
        run_dir=run_dir,
        judge_summaries=tuple(judge_summaries),
        score=score,
        status=status,
    )
