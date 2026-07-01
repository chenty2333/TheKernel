"""Generate local OSComp evaluation reports."""

from __future__ import annotations

import json
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from .artifact_index import write_artifact_index
from .schemas import RUN_MANIFEST_SCHEMA, SCORE_SUMMARY_SCHEMA


class ReportError(RuntimeError):
    """Raised for report generation failures."""


@dataclass(frozen=True)
class ReportResult:
    run_dir: Path
    markdown_path: Path
    issue_count: int


def _load_json(path: Path) -> dict[str, Any]:
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except OSError as error:
        raise ReportError(f"could not read report input: {path}") from error
    except json.JSONDecodeError as error:
        raise ReportError(f"report input is not valid JSON: {path}") from error
    if not isinstance(data, dict):
        raise ReportError(f"report input must be a JSON object: {path}")
    return data


def _load_versioned_json(path: Path, *, expected_schema: str) -> dict[str, Any]:
    data = _load_json(path)
    schema = data.get("schema")
    if schema != expected_schema:
        actual = schema if schema is not None else "<missing>"
        raise ReportError(
            f"unsupported {path.name} schema: {actual} "
            f"(expected {expected_schema})"
        )
    return data


def _fmt_score(value: Any) -> str:
    try:
        return f"{float(value):.6g}"
    except (TypeError, ValueError):
        return str(value)


def _md_cell(value: Any) -> str:
    return str(value).replace("\n", "\\n").replace("|", "\\|")


def _markdown_table(headers: list[str], rows: list[list[str]]) -> list[str]:
    lines = [
        "| " + " | ".join(headers) + " |",
        "| " + " | ".join("---" for _ in headers) + " |",
    ]
    for row in rows:
        lines.append("| " + " | ".join(_md_cell(cell) for cell in row) + " |")
    return lines


def _issue_rows(score: dict[str, Any]) -> list[dict[str, str]]:
    issues = score.get("issues", [])
    rows: list[dict[str, str]] = []
    if not isinstance(issues, list):
        return rows
    for issue in issues:
        if isinstance(issue, dict):
            details: list[str] = []
            returncode = issue.get("returncode")
            if returncode is not None:
                details.append(f"returncode={returncode}")
            log_path = issue.get("log_path")
            if log_path:
                details.append(f"log={log_path}")
            error = issue.get("error")
            if error:
                details.append(f"error={error}")
            message = issue.get("message")
            if message:
                details.append(f"message={message}")
            rows.append(
                {
                    "kind": str(issue.get("kind", "issue")),
                    "arch": str(issue.get("arch", "")),
                    "group": str(issue.get("group_id", "<run>")),
                    "status": str(issue.get("status", "")),
                    "detail": " ".join(details),
                }
            )
        else:
            rows.append(
                {
                    "kind": "issue",
                    "arch": "",
                    "group": "<run>",
                    "status": str(issue),
                    "detail": "",
                }
            )
    return rows


def _arch_rows(score: dict[str, Any]) -> list[dict[str, str]]:
    arch_totals = score.get("arch_totals", {})
    rows: list[dict[str, str]] = []
    if not isinstance(arch_totals, dict):
        return rows
    for arch, totals in sorted(arch_totals.items()):
        if not isinstance(totals, dict):
            continue
        rows.append(
            {
                "arch": str(arch),
                "non_ltp": _fmt_score(totals.get("non_ltp_score", 0)),
                "ltp_raw": _fmt_score(totals.get("ltp_raw_total", 0)),
            }
        )
    return rows


def _group_rows(score: dict[str, Any]) -> list[dict[str, str]]:
    group_totals = score.get("group_totals", {})
    rows: list[dict[str, str]] = []
    if not isinstance(group_totals, dict):
        return rows
    for key, group in sorted(group_totals.items()):
        if not isinstance(group, dict):
            continue
        rows.append(
            {
                "group": str(key),
                "status": str(group.get("status", "")),
                "rows": str(group.get("row_count", 0)),
                "raw": _fmt_score(group.get("raw_score", 0)),
                "contribution": _fmt_score(group.get("score_contribution", 0)),
                "json": str(group.get("json_path") or ""),
            }
        )
    return rows


def _libc_rows(score: dict[str, Any]) -> list[dict[str, str]]:
    libc_totals = score.get("libc_totals", {})
    rows: list[dict[str, str]] = []
    if not isinstance(libc_totals, dict):
        return rows
    for libc, totals in sorted(libc_totals.items()):
        if not isinstance(totals, dict):
            continue
        rows.append(
            {
                "libc": str(libc),
                "non_ltp": _fmt_score(totals.get("non_ltp_score", 0)),
                "ltp_raw": _fmt_score(totals.get("ltp_raw_total", 0)),
                "ltp_score": _fmt_score(totals.get("ltp_score", 0)),
            }
        )
    return rows


def _ltp_group_rows(score: dict[str, Any]) -> list[dict[str, str]]:
    ltp_group_totals = score.get("ltp_group_totals", {})
    rows: list[dict[str, str]] = []
    if not isinstance(ltp_group_totals, dict):
        return rows
    for group, totals in sorted(ltp_group_totals.items()):
        if not isinstance(totals, dict):
            continue
        rows.append(
            {
                "group": str(group),
                "raw": _fmt_score(totals.get("raw_score", 0)),
                "contribution": _fmt_score(totals.get("score_contribution", 0)),
            }
        )
    return rows


def _coverage_rows(
    manifest: dict[str, Any],
    score: dict[str, Any],
) -> list[list[str]]:
    expected = manifest.get("expected_matrix", [])
    group_totals = score.get("group_totals", {})
    if not isinstance(expected, list) or not isinstance(group_totals, dict):
        return []

    by_arch: dict[str, dict[str, int]] = {}
    for cell in expected:
        if not isinstance(cell, dict):
            continue
        arch = str(cell.get("arch", ""))
        key = str(cell.get("key", ""))
        if not arch or not key:
            continue
        counts = by_arch.setdefault(
            arch,
            {
                "expected": 0,
                "ok": 0,
                "problem": 0,
                "unreported": 0,
            },
        )
        counts["expected"] += 1
        group = group_totals.get(key)
        if not isinstance(group, dict):
            counts["unreported"] += 1
            continue
        if str(group.get("status", "")) == "ok":
            counts["ok"] += 1
        else:
            counts["problem"] += 1

    return [
        [
            arch,
            str(counts["expected"]),
            str(counts["ok"]),
            str(counts["problem"]),
            str(counts["unreported"]),
        ]
        for arch, counts in sorted(by_arch.items())
    ]


def _problem_expected_cells(
    manifest: dict[str, Any],
    score: dict[str, Any],
) -> list[list[str]]:
    expected = manifest.get("expected_matrix", [])
    group_totals = score.get("group_totals", {})
    if not isinstance(expected, list) or not isinstance(group_totals, dict):
        return []

    rows: list[list[str]] = []
    for cell in expected:
        if not isinstance(cell, dict):
            continue
        key = str(cell.get("key", ""))
        if not key:
            continue
        group = group_totals.get(key)
        if not isinstance(group, dict):
            rows.append(
                [
                    str(cell.get("arch", "")),
                    str(cell.get("group_id", key)),
                    "unreported",
                    "",
                ]
            )
            continue
        status = str(group.get("status", ""))
        if status != "ok":
            rows.append(
                [
                    str(cell.get("arch", "")),
                    str(cell.get("group_id", key)),
                    status,
                    str(group.get("json_path") or ""),
                ]
            )
    return rows


def _optional_json(path: Path) -> dict[str, Any] | None:
    if not path.is_file():
        return None
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return None
    return data if isinstance(data, dict) else None


def _artifact_summaries(run_dir: Path | None) -> dict[str, list[list[str]]]:
    if run_dir is None:
        return {"markers": [], "stderr": []}

    marker_rows: list[list[str]] = []
    stderr_rows: list[list[str]] = []
    for marker_path in sorted(run_dir.glob("*/marker-validation.json")):
        marker = _optional_json(marker_path)
        if marker is None:
            continue
        issues = marker.get("issues", [])
        log_events = marker.get("log_events", [])
        marker_rows.append(
            [
                str(marker.get("arch", marker_path.parent.name)),
                str(marker.get("marker_count", 0)),
                str(marker.get("complete_group_count", 0)),
                str(len(issues) if isinstance(issues, list) else 0),
                str(len(log_events) if isinstance(log_events, list) else 0),
                str(marker_path.relative_to(run_dir)),
            ]
        )

    for summary_path in sorted(run_dir.glob("*/judge-summary.json")):
        summary = _optional_json(summary_path)
        if summary is None:
            continue
        arch_dir = summary_path.parent
        results = summary.get("results", [])
        if not isinstance(results, list):
            continue
        for result in results:
            if not isinstance(result, dict):
                continue
            stderr_rel = result.get("stderr_path")
            if not isinstance(stderr_rel, str) or not stderr_rel:
                continue
            stderr_path = arch_dir / stderr_rel
            try:
                snippet = stderr_path.read_text(
                    encoding="utf-8",
                    errors="replace",
                ).strip()
            except OSError:
                continue
            if not snippet:
                continue
            if len(snippet) > 240:
                snippet = snippet[:240] + "..."
            stderr_rows.append(
                [
                    str(result.get("arch", arch_dir.name)),
                    str(result.get("group_id", "")),
                    str(result.get("status", "")),
                    str(Path(arch_dir.name) / stderr_rel),
                    snippet,
                ]
            )

    return {"markers": marker_rows, "stderr": stderr_rows[:20]}


def render_markdown(
    manifest: dict[str, Any],
    score: dict[str, Any],
    *,
    run_dir: Path | None = None,
) -> str:
    issues = score.get("issues", [])
    artifact_summaries = _artifact_summaries(run_dir)

    lines = [
        "# Local OSComp Evaluation Report",
        "",
        f"- Run: `{manifest.get('name', '<unknown>')}`",
        f"- Mode: `{manifest.get('mode', '<unknown>')}`",
        f"- Status: `{manifest.get('status', '<unknown>')}`",
        f"- Created: `{manifest.get('created_at', '<unknown>')}`",
        f"- Total score: `{_fmt_score(score.get('total_score', 0))}`",
        f"- Non-LTP score: `{_fmt_score(score.get('non_ltp_score', 0))}`",
        f"- LTP raw total: `{_fmt_score(score.get('ltp_raw_total', 0))}`",
        f"- LTP transformed score: `{_fmt_score(score.get('ltp_score', 0))}`",
        f"- Issues: `{len(issues)}`",
        "",
        "## Run Metadata",
        "",
    ]

    command = manifest.get("command", [])
    if isinstance(command, list) and command:
        lines.append(f"- Command: `{' '.join(str(part) for part in command)}`")

    inputs = manifest.get("inputs", {})
    if isinstance(inputs, dict) and inputs:
        for key, value in sorted(inputs.items()):
            lines.append(f"- Input `{key}`: `{value}`")

    git = manifest.get("git", {})
    if isinstance(git, dict):
        lines.append(f"- Git commit: `{git.get('commit', '<unknown>')}`")
        lines.append(f"- Git dirty: `{git.get('dirty', '<unknown>')}`")
        status_lines = git.get("status_short", [])
        if isinstance(status_lines, list) and status_lines:
            lines.append("- Git status:")
            for status_line in status_lines[:40]:
                lines.append(f"  - `{status_line}`")
            if len(status_lines) > 40:
                lines.append(f"  - `... {len(status_lines) - 40} more lines`")

    official = manifest.get("official_snapshot", {})
    if isinstance(official, dict):
        source = official.get("source", {})
        if isinstance(source, dict):
            lines.append(f"- Official repo: `{source.get('repo', '<unknown>')}`")
            lines.append(f"- Official commit: `{source.get('commit', '<unknown>')}`")
            lines.append(
                f"- Official imported at: `{source.get('imported_at', '<unknown>')}`"
            )
        elif "error" in official:
            lines.append(f"- Official snapshot: `{official['error']}`")

    replays = manifest.get("replays", [])
    if isinstance(replays, list) and replays:
        replay_rows = []
        for replay in replays:
            if not isinstance(replay, dict):
                continue
            replay_rows.append(
                [
                    str(replay.get("arch", "")),
                    str(replay.get("returncode", "")),
                    str(replay.get("duration_ms", "")),
                    str(replay.get("timed_out", "")),
                    str(replay.get("launch_failed", False)),
                    str(replay.get("log_relpath") or replay.get("log_path", "")),
                    str(replay.get("error", "")),
                ]
            )
        lines.extend(["", "## Replay Summary", ""])
        lines.extend(
            _markdown_table(
                ["Arch", "Return", "Duration ms", "Timed out", "Launch failed", "Log", "Error"],
                replay_rows,
            )
        )

    lines.extend(["", "## Marker Summary", ""])
    if artifact_summaries["markers"]:
        lines.extend(
            _markdown_table(
                ["Arch", "Markers", "Complete", "Issues", "Log events", "Artifact"],
                artifact_summaries["markers"],
            )
        )
    else:
        lines.append("- No marker artifacts found.")

    lines.extend(["", "## Judge Stderr Snippets", ""])
    if artifact_summaries["stderr"]:
        lines.extend(
            _markdown_table(
                ["Arch", "Group", "Status", "Stderr", "Snippet"],
                artifact_summaries["stderr"],
            )
        )
    else:
        lines.append("- No non-empty judge stderr captured.")

    lines.extend(["", "## Coverage Summary", ""])
    coverage_rows = _coverage_rows(manifest, score)
    if coverage_rows:
        lines.extend(
            _markdown_table(
                ["Arch", "Expected", "OK", "Problem", "Unreported"],
                coverage_rows,
            )
        )
    else:
        lines.append("- No expected matrix recorded in manifest.")

    problem_cells = _problem_expected_cells(manifest, score)
    if problem_cells:
        lines.extend(["", "## Problem Expected Cells", ""])
        lines.extend(
            _markdown_table(
                ["Arch", "Group", "Status", "JSON"],
                problem_cells,
            )
        )

    lines.extend(["", "## Arch Totals", ""])
    arch_rows = []
    for row in _arch_rows(score):
        arch_rows.append([row["arch"], row["non_ltp"], row["ltp_raw"]])
    lines.extend(_markdown_table(["Arch", "Non-LTP", "LTP raw"], arch_rows))

    libc_rows = []
    for row in _libc_rows(score):
        libc_rows.append([row["libc"], row["non_ltp"], row["ltp_raw"], row["ltp_score"]])
    if libc_rows:
        lines.extend(["", "## Libc Totals", ""])
        lines.extend(
            _markdown_table(
                ["Libc", "Non-LTP", "LTP raw", "LTP score"],
                libc_rows,
            )
        )

    ltp_rows = []
    for row in _ltp_group_rows(score):
        ltp_rows.append([row["group"], row["raw"], row["contribution"]])
    if ltp_rows:
        lines.extend(["", "## LTP Contributions", ""])
        lines.extend(_markdown_table(["Group", "Raw", "Contribution"], ltp_rows))

    lines.extend(["", "## Issues", ""])
    issue_rows = _issue_rows(score)
    if issue_rows:
        for issue in issue_rows:
            arch_prefix = f"{issue['arch']}/" if issue["arch"] else ""
            suffix = f" status={issue['status']}" if issue["status"] else ""
            detail = f" {issue['detail']}" if issue["detail"] else ""
            lines.append(f"- `{issue['kind']}` `{arch_prefix}{issue['group']}`{suffix}{detail}")
    else:
        lines.append("- None")

    lines.extend(["", "## Group Totals", ""])
    group_rows = []
    for row in _group_rows(score):
        group_rows.append(
            [
                row["group"],
                row["status"],
                row["rows"],
                row["raw"],
                row["contribution"],
                row["json"],
            ]
        )
    lines.extend(
        _markdown_table(
            ["Group", "Status", "Rows", "Raw", "Contribution", "JSON"],
            group_rows,
        )
    )

    lines.extend(
        [
            "",
            "## Artifacts",
            "",
            "- `manifest.json`",
            "- `score.json`",
            "- `artifact-index.json`",
            "- `<arch>/marker-validation.json`",
            "- `<arch>/judge-summary.json`",
            "- `<arch>/segments.jsonl`",
            "- `<arch>/segments/*.txt`",
            "- `<arch>/judges/*.stdout`",
            "- `<arch>/judges/*.stderr`",
            "- `<arch>/judges/*.json`",
        ]
    )
    return "\n".join(lines) + "\n"


def generate_report(run_dir: Path) -> ReportResult:
    if not run_dir.is_dir():
        raise ReportError(f"run directory not found: {run_dir}")
    manifest = _load_versioned_json(
        run_dir / "manifest.json",
        expected_schema=RUN_MANIFEST_SCHEMA,
    )
    score = _load_versioned_json(
        run_dir / "score.json",
        expected_schema=SCORE_SUMMARY_SCHEMA,
    )
    markdown = render_markdown(manifest, score, run_dir=run_dir)

    markdown_path = run_dir / "report.md"
    markdown_path.write_text(markdown, encoding="utf-8")
    stale_html_path = run_dir / "report.html"
    if stale_html_path.exists():
        stale_html_path.unlink()
    write_artifact_index(run_dir)
    issues = score.get("issues", [])
    issue_count = len(issues) if isinstance(issues, list) else 0
    return ReportResult(
        run_dir=run_dir,
        markdown_path=markdown_path,
        issue_count=issue_count,
    )
