#!/usr/bin/env python3
"""Validate the fixed, machine-readable Linux ABI closure cohorts."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
ABI_DIR = ROOT / "docs" / "linux-abi"
TABLE = ABI_DIR / "linux-v6.12.103-arch-x86-entry-syscalls-syscall_64.tbl"
MATRIX = ABI_DIR / "syscall-matrix.json"
COHORTS = ABI_DIR / "closure-cohorts-v1.json"
SCHEMA = "thekernel-linux-abi-closure-cohorts-v1"
FAMILIES = {"admin", "async-io", "mount", "namespace", "security"}
COMPLEX = {
    "ioctl", "ptrace", "personality", "modify_ldt", "prctl", "arch_prctl",
    "adjtimex", "chroot", "acct", "settimeofday", "sethostname", "setdomainname",
    "quotactl", "remap_file_pages", "clock_settime", "mbind", "set_mempolicy",
    "get_mempolicy", "migrate_pages", "move_pages", "perf_event_open", "fanotify_init",
    "fanotify_mark", "name_to_handle_at", "open_by_handle_at", "clock_adjtime", "kcmp",
    "userfaultfd", "uretprobe", "process_madvise", "quotactl_fd", "memfd_secret",
    "process_mrelease", "set_mempolicy_home_node", "map_shadow_stack", "mseal",
}
ALIASES = (
    ("eventfd", "eventfd2"), ("signalfd", "signalfd4"),
    ("inotify_init", "inotify_init1"), ("epoll_create", "epoll_create1"),
    ("accept", "accept4"), ("pipe", "pipe2"),
)


class CohortError(ValueError):
    pass


def load(path: Path) -> Any:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise CohortError(f"{path}: {error}") from error


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def table_rows(path: Path) -> list[tuple[int, str]]:
    rows = []
    for lineno, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        fields = line.split()
        if not fields or fields[0].startswith("#"):
            continue
        if len(fields) < 3 or fields[1] not in {"common", "64"}:
            continue
        try:
            rows.append((int(fields[0]), fields[2]))
        except ValueError as error:
            raise CohortError(f"{path}:{lineno}: invalid syscall number") from error
    if len(rows) != 375 or len(set(rows)) != 375:
        raise CohortError("fixed table must contain exactly 375 unique common/64 rows")
    return rows


def members(value: Any, label: str) -> list[tuple[int, str]]:
    if not isinstance(value, list):
        raise CohortError(f"{label}: members must be a list")
    result = []
    for item in value:
        if not isinstance(item, dict) or set(item) != {"nr", "name"}:
            raise CohortError(f"{label}: member must have exactly nr and name")
        if not isinstance(item["nr"], int) or not isinstance(item["name"], str) or not item["name"]:
            raise CohortError(f"{label}: invalid member")
        result.append((item["nr"], item["name"]))
    return result


def validate(document_path: Path = COHORTS, table_path: Path = TABLE,
             matrix_path: Path = MATRIX) -> dict[str, int]:
    document = load(document_path)
    if not isinstance(document, dict) or set(document) != {"schema", "baseline", "cohorts", "alias_groups"}:
        raise CohortError("unsupported closure cohort schema")
    if document["schema"] != SCHEMA:
        raise CohortError("unsupported closure cohort schema")
    baseline = document["baseline"]
    if not isinstance(baseline, dict) or set(baseline) != {"table", "matrix"}:
        raise CohortError("baseline must contain exactly table and matrix")
    expected_paths = {
        "table": "docs/linux-abi/linux-v6.12.103-arch-x86-entry-syscalls-syscall_64.tbl",
        "matrix": "docs/linux-abi/syscall-matrix.json",
    }
    for key, path in (("table", table_path), ("matrix", matrix_path)):
        binding = baseline[key]
        if not isinstance(binding, dict) or set(binding) != {"path", "sha256"} or not isinstance(binding["path"], str) or not isinstance(binding["sha256"], str):
            raise CohortError(f"invalid {key} binding")
        if binding["path"] != expected_paths[key] or binding["sha256"] != digest(path):
            raise CohortError(f"{key} hash drift")
    rows = table_rows(table_path)
    matrix = load(matrix_path)
    if not isinstance(matrix, dict) or set(matrix) != {"schema", "baseline", "syscalls"} or not isinstance(matrix["syscalls"], list):
        raise CohortError("unsupported matrix schema")
    matrix_rows = matrix["syscalls"]
    matrix_by_pair = {(row.get("nr"), row.get("name")): row for row in matrix_rows if isinstance(row, dict)}
    if set(matrix_by_pair) != set(rows):
        raise CohortError("matrix/table membership drift")
    cohorts = document["cohorts"]
    if not isinstance(cohorts, list) or len(cohorts) != 3:
        raise CohortError("cohorts must contain exactly three cohorts")
    expected_ids = ("native-ni", "phase3", "phase2")
    actual_ids = []
    actual: dict[str, list[tuple[int, str]]] = {}
    for cohort in cohorts:
        if not isinstance(cohort, dict) or set(cohort) != {"id", "members"} or not isinstance(cohort.get("id"), str):
            raise CohortError("invalid cohort")
        cohort_id = cohort["id"]
        actual_ids.append(cohort_id)
        actual[cohort_id] = members(cohort["members"], cohort_id)
    if tuple(actual_ids) != expected_ids:
        raise CohortError("cohort order or IDs drift")
    expected_ni = [pair for pair in rows if matrix_by_pair[pair].get("linux_route") == "ni"]
    expected_phase3 = [pair for pair in rows if matrix_by_pair[pair].get("linux_route") != "ni" and (matrix_by_pair[pair].get("uapi_family") in FAMILIES or pair[1] in COMPLEX)]
    expected_phase2 = [pair for pair in rows if pair not in expected_ni and pair not in expected_phase3]
    expected = {"native-ni": expected_ni, "phase3": expected_phase3, "phase2": expected_phase2}
    if set().union(*(set(value) for value in actual.values())) != set(rows) or sum(len(value) for value in actual.values()) != len(rows):
        raise CohortError("cohorts must be mutually exclusive and cover the fixed table")
    for cohort_id, expected_members in expected.items():
        if actual[cohort_id] != expected_members:
            raise CohortError(f"{cohort_id} membership or syscall order drift")
    if (len(actual["native-ni"]), len(actual["phase3"]), len(actual["phase2"])) != (17, 87, 271):
        raise CohortError("cohort counts drift")
    aliases = document["alias_groups"]
    if not isinstance(aliases, list) or len(aliases) != len(ALIASES):
        raise CohortError("alias groups drift")
    alias_names = []
    for group in aliases:
        if not isinstance(group, dict) or set(group) != {"members"}:
            raise CohortError("invalid alias group")
        alias_names.append(tuple(name for _, name in members(group["members"], "alias group")))
    if tuple(alias_names) != ALIASES:
        raise CohortError("alias groups drift")
    phase2_names = {name for _, name in actual["phase2"]}
    if any(not set(group) <= phase2_names for group in alias_names):
        raise CohortError("alias members must remain independently in phase2")
    return {cohort_id: len(actual[cohort_id]) for cohort_id in expected_ids}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--document", type=Path, default=COHORTS)
    parser.add_argument("--table", type=Path, default=TABLE)
    parser.add_argument("--matrix", type=Path, default=MATRIX)
    args = parser.parse_args()
    counts = validate(args.document, args.table, args.matrix)
    print("closure-cohorts-v1 valid: " + ", ".join(f"{key}={value}" for key, value in counts.items()))


if __name__ == "__main__":
    main()
