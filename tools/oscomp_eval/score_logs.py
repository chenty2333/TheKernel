"""Offline score runs from existing RV/LA console logs."""

from __future__ import annotations

from dataclasses import dataclass
from dataclasses import replace as dataclass_replace
from pathlib import Path

from .config import JUDGE_TIMEOUT_SECS, Libc
from .judge_runner import judge_log
from .paths import create_run_dir, prepare_run_dir
from .scoring import score_judge_summaries, write_score_summary
from .schemas import JudgeSummary, ScoreSummary


@dataclass(frozen=True)
class ScoreLogsResult:
    run_dir: Path
    judge_summaries: tuple[JudgeSummary, ...]
    score: ScoreSummary
    status: str


def score_logs_status(score: ScoreSummary) -> str:
    return "incomplete" if score.has_errors else "complete"


def score_logs(
    *,
    name: str,
    run_dir: Path | None = None,
    rv_log: Path | None = None,
    la_log: Path | None = None,
    judge_dir: Path | None = None,
    judge_timeout_secs: float = JUDGE_TIMEOUT_SECS,
    fail_fast: bool = False,
    replace: bool = False,
    group_libc_matrix: tuple[tuple[str, Libc], ...] | None = None,
) -> ScoreLogsResult:
    if rv_log is None and la_log is None:
        raise ValueError("score-logs requires at least one of rv_log or la_log")

    if run_dir is None:
        run_dir = create_run_dir(name, replace=replace)
    else:
        run_dir = prepare_run_dir(run_dir, replace=replace)

    judge_summaries: list[JudgeSummary] = []
    scored_arches: list[str] = []
    log_inputs: dict[str, str] = {}
    for arch, log_path in (("rv", rv_log), ("la", la_log)):
        if log_path is None:
            continue
        scored_arches.append(arch)
        log_inputs[f"{arch}_log"] = str(log_path)
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
    score = dataclass_replace(
        score,
        run={
            "name": name,
            "mode": "score-logs",
            "status": status,
            "arches": scored_arches,
            **log_inputs,
        },
    )
    write_score_summary(score, run_dir / "score.json")
    return ScoreLogsResult(
        run_dir=run_dir,
        judge_summaries=tuple(judge_summaries),
        score=score,
        status=status,
    )
