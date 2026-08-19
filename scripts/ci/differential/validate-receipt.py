#!/usr/bin/env python3
"""Validate a differential-host receipt against the contract-v0 schema."""

from __future__ import annotations

import argparse
import json
import re
from pathlib import Path

RECEIPT_SCHEMA = "thekernel-differential-receipt-v0"
CASE_PATTERN = re.compile(r"^[a-z][a-z0-9_-]*$")
GIT_REV_PATTERN = re.compile(r"^[0-9a-f]{40}$")
RANGE_CLAUSE_PATTERN = re.compile(r"^(>=|<=|==|>|<)?\d+(?:\.\d+){0,2}$")
SHA256_PATTERN = re.compile(r"^[0-9a-f]{64}$")


def require(condition: bool, message: str) -> None:
    if not condition:
        raise ValueError(message)


def nonempty_string(value: object, label: str) -> str:
    require(isinstance(value, str) and bool(value), f"{label} must be a non-empty string")
    assert isinstance(value, str)
    return value


def marker_count(manifest: Path) -> int:
    count = 0
    with manifest.open(encoding="utf-8") as source:
        for line in source:
            line = line.rstrip("\n")
            if not line.strip() or line.startswith("#"):
                continue
            count += 1
    return count


def validate(args: argparse.Namespace) -> None:
    receipt_path = Path(args.receipt).expanduser().resolve()
    with receipt_path.open(encoding="utf-8") as source:
        loaded: object = json.load(source)
    require(isinstance(loaded, dict), "differential receipt must be a JSON object")
    assert isinstance(loaded, dict)
    receipt: dict[str, object] = loaded

    require(receipt.get("schema") == RECEIPT_SCHEMA, "unsupported receipt schema")
    case = nonempty_string(receipt.get("case"), "case")
    require(CASE_PATTERN.fullmatch(case) is not None, f"invalid case name: {case!r}")
    if args.case is not None:
        require(case == args.case, f"receipt case mismatch: {case!r} != {args.case!r}")

    git_rev = nonempty_string(receipt.get("git_rev"), "git_rev")
    require(
        GIT_REV_PATTERN.fullmatch(git_rev) is not None,
        "git_rev must be a full 40-hex-digit revision",
    )

    reference = receipt.get("reference")
    require(isinstance(reference, dict), "reference must be a JSON object")
    assert isinstance(reference, dict)
    require(reference.get("kind") == "host-linux", "reference kind must be host-linux")
    nonempty_string(reference.get("kernel_release"), "reference.kernel_release")
    version_line = nonempty_string(
        reference.get("kernel_version_line"), "reference.kernel_version_line"
    )
    require(
        version_line.startswith("Linux version "),
        "reference.kernel_version_line must be a /proc/version line",
    )

    toolchain = receipt.get("toolchain")
    require(isinstance(toolchain, dict), "toolchain must be a JSON object")
    assert isinstance(toolchain, dict)
    nonempty_string(toolchain.get("cc"), "toolchain.cc")

    expected = receipt.get("markers_expected")
    matched = receipt.get("markers_matched")
    require(
        type(expected) is int and expected >= 0,
        "markers_expected must be a non-negative integer",
    )
    require(
        type(matched) is int and matched >= 0,
        "markers_matched must be a non-negative integer",
    )
    assert isinstance(expected, int) and isinstance(matched, int)
    require(matched <= expected, "markers_matched exceeds markers_expected")
    require(expected > 0, "a differential case must expect at least one marker")

    applied = receipt.get("allowlist_applied")
    require(isinstance(applied, list), "allowlist_applied must be a JSON array")
    assert isinstance(applied, list)
    applied_markers: set[str] = set()
    for index, entry in enumerate(applied):
        label = f"allowlist_applied[{index}]"
        require(isinstance(entry, dict), f"{label} must be a JSON object")
        assert isinstance(entry, dict)
        require(
            set(entry) == {"marker", "reason", "kernel_range"},
            f"{label} must contain exactly marker/reason/kernel_range",
        )
        marker = nonempty_string(entry.get("marker"), f"{label}.marker")
        nonempty_string(entry.get("reason"), f"{label}.reason")
        kernel_range = nonempty_string(entry.get("kernel_range"), f"{label}.kernel_range")
        for clause in kernel_range.split():
            require(
                RANGE_CLAUSE_PATTERN.fullmatch(clause) is not None,
                f"{label}.kernel_range has an unparseable clause: {clause!r}",
            )
        require(marker not in applied_markers, f"{label} repeats marker {marker!r}")
        applied_markers.add(marker)

    result = receipt.get("result")
    require(result in ("pass", "fail"), 'result must be "pass" or "fail"')
    if result == "pass":
        require(
            matched + len(applied_markers) == expected,
            "a pass receipt must account for every expected marker as matched "
            "or explicitly allowlisted",
        )
    if args.require_empty_allowlist:
        require(not applied, "receipt applied an allowlist but none was permitted")

    source_inputs = receipt.get("source_inputs")
    if source_inputs is not None:
        require(isinstance(source_inputs, list), "source_inputs must be a JSON array")
        assert isinstance(source_inputs, list)
        seen_paths: set[str] = set()
        for index, entry in enumerate(source_inputs):
            label = f"source_inputs[{index}]"
            require(isinstance(entry, dict), f"{label} must be a JSON object")
            assert isinstance(entry, dict)
            require(
                set(entry) == {"path", "sha256"},
                f"{label} must contain exactly path/sha256",
            )
            path = nonempty_string(entry.get("path"), f"{label}.path")
            digest = nonempty_string(entry.get("sha256"), f"{label}.sha256")
            require(
                not Path(path).is_absolute()
                and path != ".."
                and not path.startswith("../")
                and ".." not in Path(path).parts,
                f"{label}.path must be repository-relative",
            )
            require(
                SHA256_PATTERN.fullmatch(digest) is not None,
                f"{label}.sha256 is invalid",
            )
            require(path not in seen_paths, f"{label} repeats path {path!r}")
            seen_paths.add(path)
    if args.manifest is not None:
        manifest = Path(args.manifest).expanduser().resolve()
        require(
            expected == marker_count(manifest),
            "markers_expected does not match the manifest marker count",
        )
    if args.require_pass:
        require(result == "pass", "receipt result is not pass")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--receipt", required=True)
    parser.add_argument("--case", help="require this exact case name")
    parser.add_argument(
        "--manifest",
        help="require markers_expected to match this manifest's marker count",
    )
    parser.add_argument(
        "--require-empty-allowlist",
        action="store_true",
        help="reject receipts that applied any allowlist entry",
    )
    parser.add_argument(
        "--require-pass",
        action="store_true",
        help="reject receipts whose result is not pass",
    )
    args = parser.parse_args()
    try:
        validate(args)
    except (OSError, ValueError, json.JSONDecodeError) as error:
        parser.error(str(error))
    print(f"differential receipt: PASS receipt={Path(args.receipt).resolve()}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
