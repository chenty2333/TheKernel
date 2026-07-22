#!/usr/bin/env python3
"""Validate that the RFC index and RFC metadata describe the same state."""

from __future__ import annotations

import re
import sys
from pathlib import Path


VALID_STATUSES = {"draft", "accepted", "implemented", "superseded", "rejected"}
ENTRY_RE = re.compile(
    r"^- \[RFC (?P<number>\d{4}): .+\]"
    r"\((?P<filename>\d{4}-[^)]+\.md)\)$"
)
INDEX_METADATA_RE = re.compile(
    r"^  \(\`(?P<status>[a-z]+)\`"
    r"(?:; profile: \`(?P<profile>[^\`]+)\`)?"
    r"\)$"
)
HEADER_STATUS_RE = re.compile(r"^- Status: (?P<status>[a-z]+)$", re.MULTILINE)
HEADER_PROFILE_RE = re.compile(r"^- Profile: (?P<profile>[^\n]+)$", re.MULTILINE)


def fail(message: str) -> None:
    raise ValueError(message)


def exactly_one(pattern: re.Pattern[str], text: str, label: str) -> str | None:
    matches = pattern.findall(text)
    if len(matches) > 1:
        fail(f"{label}: metadata appears {len(matches)} times")
    return matches[0] if matches else None


def main() -> int:
    repo_root = Path(__file__).resolve().parents[2]
    rfc_dir = repo_root / "docs" / "rfcs"
    index_path = rfc_dir / "README.md"
    lines = index_path.read_text(encoding="utf-8").splitlines()

    indexed_files: set[str] = set()
    indexed_numbers: set[str] = set()
    entry_count = 0
    for line_number, line in enumerate(lines, start=1):
        entry = ENTRY_RE.fullmatch(line)
        if entry is None:
            continue
        entry_count += 1
        number = entry.group("number")
        filename = entry.group("filename")
        if number in indexed_numbers:
            fail(f"{index_path}:{line_number}: duplicate RFC number {number}")
        if filename in indexed_files:
            fail(f"{index_path}:{line_number}: duplicate RFC file {filename}")
        if not filename.startswith(f"{number}-"):
            fail(f"{index_path}:{line_number}: number and filename disagree")
        indexed_numbers.add(number)
        indexed_files.add(filename)

        if line_number >= len(lines):
            fail(f"{index_path}:{line_number}: missing index metadata line")
        index_metadata = INDEX_METADATA_RE.fullmatch(lines[line_number])
        if index_metadata is None:
            fail(f"{index_path}:{line_number + 1}: malformed index metadata")
        index_status = index_metadata.group("status")
        index_profile = index_metadata.group("profile")
        if index_status not in VALID_STATUSES:
            fail(f"{index_path}:{line_number + 1}: invalid status {index_status!r}")

        rfc_path = rfc_dir / filename
        if not rfc_path.is_file():
            fail(f"{index_path}:{line_number}: missing RFC file {filename}")
        rfc_text = rfc_path.read_text(encoding="utf-8")
        header_status = exactly_one(HEADER_STATUS_RE, rfc_text, str(rfc_path))
        if header_status is None:
            fail(f"{rfc_path}: missing standard '- Status:' metadata")
        if header_status not in VALID_STATUSES:
            fail(f"{rfc_path}: invalid status {header_status!r}")
        if index_status != header_status:
            fail(
                f"{rfc_path}: index status {index_status!r} disagrees with "
                f"header status {header_status!r}"
            )

        header_profile = exactly_one(HEADER_PROFILE_RE, rfc_text, str(rfc_path))
        if index_profile != header_profile:
            fail(
                f"{rfc_path}: index profile {index_profile!r} disagrees with "
                f"header profile {header_profile!r}"
            )

    disk_files = {
        path.name for path in rfc_dir.glob("[0-9][0-9][0-9][0-9]-*.md")
    }
    missing_from_index = sorted(disk_files - indexed_files)
    stale_index_entries = sorted(indexed_files - disk_files)
    if missing_from_index:
        fail(f"RFC files missing from index: {', '.join(missing_from_index)}")
    if stale_index_entries:
        fail(f"index entries without RFC files: {', '.join(stale_index_entries)}")
    if entry_count == 0:
        fail(f"{index_path}: no RFC entries found")

    print(f"rfc-index: PASS entries={entry_count}")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError) as error:
        print(f"rfc-index: FAIL: {error}", file=sys.stderr)
        raise SystemExit(1)
