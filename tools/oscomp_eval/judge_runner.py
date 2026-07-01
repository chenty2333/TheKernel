"""Run official-compatible judge scripts against parsed group segments."""

from __future__ import annotations

import json
import subprocess
import sys
import time
from pathlib import Path

from .config import DEFAULT_GROUP_LIBC_MATRIX, Libc, MatrixCell
from .markers import parse_log, write_artifacts, write_json
from .paths import official_judge_dir
from .schemas import (
    JUDGE_RESULT_SCHEMA,
    JudgeGroupResult,
    JudgeSummary,
    MarkerParseResult,
    Segment,
)


class JudgeRunnerError(RuntimeError):
    """Raised for invalid judge runner inputs."""


def discover_judges(judge_dir: Path) -> dict[str, Path]:
    if not judge_dir.is_dir():
        raise JudgeRunnerError(f"judge directory not found: {judge_dir}")

    judges: dict[str, Path] = {}
    for path in judge_dir.glob("judge_*.py"):
        stem = path.stem
        group_id = stem.removeprefix("judge_")
        judges[group_id] = path
    return judges


def _expected_cells_for_arch(arch: str) -> tuple[MatrixCell, ...]:
    if arch not in {"rv", "la"}:
        raise JudgeRunnerError(f"unsupported arch for judge run: {arch}")
    return tuple(
        MatrixCell(arch=arch, group=group, libc=libc)
        for group, libc in DEFAULT_GROUP_LIBC_MATRIX
    )


def _segments_by_group(result: MarkerParseResult) -> dict[str, list[Segment]]:
    grouped: dict[str, list[Segment]] = {}
    for segment in result.segments:
        if segment.status != "complete":
            continue
        grouped.setdefault(segment.group, []).append(segment)
    return grouped


def _parse_judge_stdout(stdout: str) -> tuple[tuple[object, ...], tuple[str, ...]]:
    warnings: list[str] = []
    stripped = stdout.strip()
    if not stripped:
        raise ValueError("judge stdout is empty")

    try:
        data = json.loads(stripped)
    except json.JSONDecodeError:
        data = None
    else:
        if not isinstance(data, list):
            raise ValueError("judge stdout JSON is not a list")
        return tuple(data), ()

    for line in reversed(stdout.splitlines()):
        candidate = line.strip()
        if not candidate.startswith("["):
            continue
        try:
            data = json.loads(candidate)
        except json.JSONDecodeError:
            continue
        if not isinstance(data, list):
            continue
        warnings.append("recovered JSON list from last JSON-looking stdout line")
        return tuple(data), tuple(warnings)

    start = stdout.rfind("[")
    end = stdout.rfind("]")
    if start != -1 and end != -1 and end > start:
        candidate = stdout[start : end + 1]
        try:
            data = json.loads(candidate)
        except json.JSONDecodeError as error:
            raise ValueError("judge stdout is not valid JSON") from error
        if isinstance(data, list):
            warnings.append("recovered JSON list from stdout substring")
            return tuple(data), tuple(warnings)

    raise ValueError("judge stdout is not valid JSON")


def _write_text(path: Path, text: str) -> None:
    tmp_path = path.with_name(f".{path.name}.tmp")
    tmp_path.write_text(text, encoding="utf-8")
    tmp_path.replace(path)


def _relative(path: Path, base: Path) -> str:
    return str(path.relative_to(base))


def _judge_result_artifact(
    *,
    cell: MatrixCell,
    status: str,
    command: tuple[str, ...],
    judge_path: Path,
    segment: Segment,
    exit_code: int,
    duration_ms: int,
    stdout_path: str,
    stderr_path: str,
    rows: tuple[object, ...],
    warnings: tuple[str, ...],
) -> dict[str, object]:
    return {
        "schema": JUDGE_RESULT_SCHEMA,
        "arch": cell.arch,
        "group": cell.group,
        "libc": cell.libc,
        "group_id": cell.group_id,
        "status": status,
        "command": list(command),
        "judge_path": str(judge_path),
        "segment_sequence": segment.sequence,
        "exit_code": exit_code,
        "duration_ms": duration_ms,
        "stdout_path": stdout_path,
        "stderr_path": stderr_path,
        "rows": list(rows),
        "warnings": list(warnings),
    }


def run_judges(
    marker_result: MarkerParseResult,
    *,
    out_dir: Path,
    judge_dir: Path | None = None,
    judge_timeout_secs: float = 30,
    fail_fast: bool = False,
    expected_cells: tuple[MatrixCell, ...] | None = None,
    marker_issues: tuple[dict[str, object], ...] = (),
) -> JudgeSummary:
    judge_dir = judge_dir or official_judge_dir()
    judges = discover_judges(judge_dir)
    grouped_segments = _segments_by_group(marker_result)
    judges_out = out_dir / "judges"
    judges_out.mkdir(parents=True, exist_ok=True)

    results: list[JudgeGroupResult] = []
    cells = expected_cells or _expected_cells_for_arch(marker_result.arch)
    for cell in cells:
        warnings: list[str] = []
        group_segments = grouped_segments.get(cell.group_id, [])
        segment = group_segments[0] if group_segments else None
        if len(group_segments) > 1:
            warnings.append(
                f"multiple complete segments for {cell.group_id}; using sequence {segment.sequence}"
            )

        judge_path = judges.get(cell.group_id)
        if segment is None:
            results.append(
                JudgeGroupResult(
                    arch=cell.arch,
                    group=cell.group,
                    libc=cell.libc,
                    group_id=cell.group_id,
                    status="missing-segment",
                    command=(),
                    judge_path=str(judge_path) if judge_path else None,
                    segment_sequence=None,
                    exit_code=None,
                    duration_ms=0,
                    stdout_path=None,
                    stderr_path=None,
                    json_path=None,
                    rows=(),
                    warnings=tuple(warnings),
                )
            )
            if fail_fast:
                break
            continue

        if judge_path is None:
            results.append(
                JudgeGroupResult(
                    arch=cell.arch,
                    group=cell.group,
                    libc=cell.libc,
                    group_id=cell.group_id,
                    status="missing-judge",
                    command=(),
                    judge_path=None,
                    segment_sequence=segment.sequence,
                    exit_code=None,
                    duration_ms=0,
                    stdout_path=None,
                    stderr_path=None,
                    json_path=None,
                    rows=(),
                    warnings=tuple(warnings),
                )
            )
            if fail_fast:
                break
            continue

        stdout_path = judges_out / f"{cell.group_id}.stdout"
        stderr_path = judges_out / f"{cell.group_id}.stderr"
        json_path = judges_out / f"{cell.group_id}.json"
        command = (sys.executable, str(judge_path))
        start = time.monotonic()
        try:
            completed = subprocess.run(
                command,
                input=segment.body,
                text=True,
                capture_output=True,
                timeout=judge_timeout_secs,
                check=False,
            )
        except subprocess.TimeoutExpired as error:
            duration_ms = int((time.monotonic() - start) * 1000)
            stdout = error.stdout if isinstance(error.stdout, str) else ""
            stderr = error.stderr if isinstance(error.stderr, str) else ""
            _write_text(stdout_path, stdout)
            _write_text(stderr_path, stderr)
            result = JudgeGroupResult(
                arch=cell.arch,
                group=cell.group,
                libc=cell.libc,
                group_id=cell.group_id,
                status="timeout",
                command=command,
                judge_path=str(judge_path),
                segment_sequence=segment.sequence,
                exit_code=None,
                duration_ms=duration_ms,
                stdout_path=_relative(stdout_path, out_dir),
                stderr_path=_relative(stderr_path, out_dir),
                json_path=None,
                rows=(),
                warnings=tuple(warnings),
            )
            results.append(result)
            if fail_fast:
                break
            continue

        duration_ms = int((time.monotonic() - start) * 1000)
        _write_text(stdout_path, completed.stdout)
        _write_text(stderr_path, completed.stderr)
        stdout_rel = _relative(stdout_path, out_dir)
        stderr_rel = _relative(stderr_path, out_dir)

        rows: tuple[object, ...] = ()
        status = "ok"
        try:
            rows, parse_warnings = _parse_judge_stdout(completed.stdout)
            warnings.extend(parse_warnings)
        except ValueError as error:
            status = "bad-json"
            warnings.append(str(error))

        if status == "ok" and completed.returncode != 0:
            status = "nonzero-exit"

        if status != "bad-json":
            write_json(
                json_path,
                _judge_result_artifact(
                    cell=cell,
                    status=status,
                    command=command,
                    judge_path=judge_path,
                    segment=segment,
                    exit_code=completed.returncode,
                    duration_ms=duration_ms,
                    stdout_path=stdout_rel,
                    stderr_path=stderr_rel,
                    rows=rows,
                    warnings=tuple(warnings),
                ),
            )

        result = JudgeGroupResult(
            arch=cell.arch,
            group=cell.group,
            libc=cell.libc,
            group_id=cell.group_id,
            status=status,  # type: ignore[arg-type]
            command=command,
            judge_path=str(judge_path),
            segment_sequence=segment.sequence,
            exit_code=completed.returncode,
            duration_ms=duration_ms,
            stdout_path=stdout_rel,
            stderr_path=stderr_rel,
            json_path=_relative(json_path, out_dir) if json_path.exists() else None,
            rows=rows,
            warnings=tuple(warnings),
        )
        results.append(result)
        if fail_fast and not result.ok:
            break

    summary = JudgeSummary(
        arch=marker_result.arch,
        judge_dir=str(judge_dir),
        results=tuple(results),
        marker_issues=marker_issues,
    )
    write_json(out_dir / "judge-summary.json", summary.to_json_dict())
    return summary


def judge_log(
    *,
    log_path: Path,
    arch: str,
    out_dir: Path,
    judge_dir: Path | None = None,
    judge_timeout_secs: float = 30,
    fail_fast: bool = False,
    group_libc_matrix: tuple[tuple[str, Libc], ...] | None = None,
) -> JudgeSummary:
    marker_result = parse_log(log_path, arch=arch)
    write_artifacts(marker_result, out_dir)
    expected_cells = None
    if group_libc_matrix is not None:
        expected_cells = tuple(
            MatrixCell(arch=arch, group=group, libc=libc)
            for group, libc in group_libc_matrix
        )
    return run_judges(
        marker_result,
        out_dir=out_dir,
        judge_dir=judge_dir,
        judge_timeout_secs=judge_timeout_secs,
        fail_fast=fail_fast,
        expected_cells=expected_cells,
        marker_issues=tuple(issue.to_json_dict() for issue in marker_result.issues),
    )
