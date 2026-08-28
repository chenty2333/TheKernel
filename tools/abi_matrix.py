#!/usr/bin/env python3
"""Validate and regenerate the native x86_64 Linux ABI evidence matrix."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parents[1]
ABI_DIR = ROOT / "docs" / "linux-abi"
SNAPSHOT = ABI_DIR / "linux-v6.12.103-arch-x86-entry-syscalls-syscall_64.tbl"
MATRIX = ABI_DIR / "syscall-matrix.json"
CATALOG = ABI_DIR / "evidence-catalog.json"
CONTRACT_DIR = ABI_DIR / "contracts"
EXPECTED = {
    "linux_tag": "v6.12.103",
    "linux_commit": "25c09b42358e73e1476e517b296edb6344f2e4bd",
    "architecture": "x86_64",
    "abi_scope": ["common", "64"],
    "excluded_abi": ["x32"],
    "source": SNAPSHOT.name,
    "source_sha256": "980ce3115028c71c5618e7864d262017bde8103bcfe7b413147a14fd312c92ac",
    "syscall_count": 375,
}
DISPATCH_KINDS = {"unknown", "dispatch-arm", "alias", "feature", "fallback"}
DISPOSITIONS = {"implemented", "partial", "explicit-enosys", "unknown"}
REVIEWS = {"unreviewed", "in-review", "reviewed"}
EVIDENCE_LANES = {"host-unit", "host-linux-differential", "guest-KTAP"}


class MatrixError(ValueError):
    pass


def load_json(path: Path) -> Any:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise MatrixError(f"{path}: {error}") from error


def source_rows(path: Path) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    for line_number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        fields = line.split()
        if not fields or fields[0].startswith("#"):
            continue
        if len(fields) < 3:
            raise MatrixError(f"{path}:{line_number}: malformed syscall table row")
        nr, abi, name = fields[:3]
        # A deliberately blank table entry is Linux's explicit ni_syscall arm.
        entry = fields[3] if len(fields) >= 4 else "sys_ni_syscall"
        if abi not in {"common", "64"}:
            continue
        try:
            number = int(nr)
        except ValueError as error:
            raise MatrixError(f"{path}:{line_number}: invalid syscall number {nr}") from error
        rows.append({"nr": number, "abi": abi, "name": name, "entry": entry})
    if len(rows) != EXPECTED["syscall_count"]:
        raise MatrixError(f"{path}: expected {EXPECTED['syscall_count']} common/64 rows, found {len(rows)}")
    if len({row["nr"] for row in rows}) != len(rows):
        raise MatrixError(f"{path}: duplicate syscall number in native scope")
    return rows


def contract_ids(contract_dir: Path, known_evidence: set[str]) -> set[str]:
    ids: set[str] = set()
    for path in sorted(contract_dir.glob("*.json")):
        document = load_json(path)
        for cell in document.get("cells", []):
            identifier = cell.get("id")
            if not isinstance(identifier, str) or not identifier:
                raise MatrixError(f"{path}: invalid contract ID")
            if identifier in ids:
                raise MatrixError(f"{path}: duplicate contract ID {identifier}")
            references = cell.get("evidence")
            if (not isinstance(references, list)
                    or not all(isinstance(reference, str) for reference in references)):
                raise MatrixError(f"{path}: {identifier}: invalid evidence references")
            absent = set(references) - known_evidence
            if absent:
                raise MatrixError(
                    f"{path}: {identifier}: unknown evidence IDs {sorted(absent)}"
                )
            ids.add(identifier)
    return ids


def _validate_guest_ktap_evidence(catalog_path: Path, item: dict[str, Any]) -> None:
    source = item.get("source")
    if not isinstance(source, str) or not source:
        raise MatrixError(f"{catalog_path}: {item.get('id')}: invalid evidence source")
    relative = Path(source)
    if relative.is_absolute() or ".." in relative.parts or relative.parts[:1] != ("evidence",):
        raise MatrixError(
            f"{catalog_path}: {item.get('id')}: guest evidence must be a tracked evidence/ path"
        )
    evidence_root = catalog_path.parent.resolve()
    evidence_path = (catalog_path.parent / relative).resolve()
    if evidence_root not in evidence_path.parents:
        raise MatrixError(f"{catalog_path}: {item.get('id')}: evidence escapes ABI directory")
    try:
        payload = evidence_path.read_bytes()
    except OSError as error:
        raise MatrixError(f"{catalog_path}: {item.get('id')}: missing evidence: {error}") from error
    expected_hash = item.get("source_sha256")
    actual_hash = hashlib.sha256(payload).hexdigest()
    if not isinstance(expected_hash, str) or expected_hash != actual_hash:
        raise MatrixError(f"{catalog_path}: {item.get('id')}: evidence checksum drift")

    text = payload.decode("utf-8")
    if len(re.findall(r"(?m)^KTAP version 1$", text)) != 1:
        raise MatrixError(f"{evidence_path}: expected exactly one KTAP version line")
    plans = re.findall(r"(?m)^1\.\.(\d+)$", text)
    if plans != ["28"]:
        raise MatrixError(f"{evidence_path}: expected exactly one 1..28 plan")
    results = re.findall(r"(?m)^(ok|not ok) (\d+) - ([^\r\n]+)$", text)
    if len(results) != 28 or [int(number) for _, number, _ in results] != list(range(1, 29)):
        raise MatrixError(f"{evidence_path}: expected complete ordered 1..28 results")
    if any(status != "ok" for status, _, _ in results):
        raise MatrixError(f"{evidence_path}: failing KTAP result")
    if re.search(r"(?im)\bSKIP\b", text):
        raise MatrixError(f"{evidence_path}: skipped KTAP result")
    required = (
        "# eventfd: THEKERNEL_EVENTFD_OK",
        "ok 17 - eventfd",
        "# THEKERNEL_SYSTEM_TEST_COMPLETE",
        "# runner-returncode: 0",
        "# guest-clean-shutdown: true",
        "# runner-terminated: false",
    )
    for marker in required:
        if len(re.findall(rf"(?m)^{re.escape(marker)}$", text)) != 1:
            raise MatrixError(f"{evidence_path}: missing or duplicate marker {marker}")


def evidence_ids(catalog_path: Path) -> set[str]:
    document = load_json(catalog_path)
    if not isinstance(document, dict) or document.get("schema") != "thekernel-linux-abi-evidence-catalog-v1":
        raise MatrixError(f"{catalog_path}: unsupported evidence catalog schema")
    items = document.get("evidence")
    if not isinstance(items, list):
        raise MatrixError(f"{catalog_path}: evidence must be a list")
    ids: set[str] = set()
    for item in items:
        if not isinstance(item, dict):
            raise MatrixError(f"{catalog_path}: evidence entry is not an object")
        identifier = item.get("id")
        if not isinstance(identifier, str) or not identifier:
            raise MatrixError(f"{catalog_path}: invalid evidence ID")
        if identifier in ids:
            raise MatrixError(f"{catalog_path}: duplicate evidence ID {identifier}")
        lane = item.get("lane")
        if lane not in EVIDENCE_LANES:
            raise MatrixError(f"{catalog_path}: {identifier}: invalid evidence lane")
        if not isinstance(item.get("source"), str) or not item["source"]:
            raise MatrixError(f"{catalog_path}: {identifier}: invalid evidence source")
        if not isinstance(item.get("assertion"), str) or not item["assertion"]:
            raise MatrixError(f"{catalog_path}: {identifier}: invalid evidence assertion")
        if lane == "guest-KTAP":
            _validate_guest_ktap_evidence(catalog_path, item)
        ids.add(identifier)
    return ids


def validate_paths(snapshot: Path = SNAPSHOT, matrix_path: Path = MATRIX,
                   catalog_path: Path = CATALOG, contract_dir: Path = CONTRACT_DIR) -> dict[str, int]:
    facts = source_rows(snapshot)
    document = load_json(matrix_path)
    if document.get("schema") != "thekernel-linux-abi-matrix-v1":
        raise MatrixError(f"{matrix_path}: unsupported schema")
    baseline = document.get("baseline")
    if baseline != EXPECTED:
        raise MatrixError(f"{matrix_path}: baseline drift")
    if hashlib.sha256(snapshot.read_bytes()).hexdigest() != EXPECTED["source_sha256"]:
        raise MatrixError(f"{snapshot}: snapshot checksum drift")
    evidence = evidence_ids(catalog_path)
    contracts = contract_ids(contract_dir, evidence)
    rows = document.get("syscalls")
    if not isinstance(rows, list) or len(rows) != len(facts):
        raise MatrixError(f"{matrix_path}: expected exactly {len(facts)} syscall rows")
    expected_by_name = {row["name"]: row for row in facts}
    seen: set[str] = set()
    counts = {status: 0 for status in DISPOSITIONS}
    for index, row in enumerate(rows):
        if not isinstance(row, dict):
            raise MatrixError(f"{matrix_path}: row {index} is not an object")
        name = row.get("name")
        if name in seen:
            raise MatrixError(f"{matrix_path}: duplicate syscall {name}")
        seen.add(name)
        expected = expected_by_name.get(name)
        if expected is None:
            raise MatrixError(f"{matrix_path}: syscall {name!r} absent from snapshot")
        for field in ("nr", "abi", "entry"):
            if row.get(field) != expected[field]:
                raise MatrixError(f"{matrix_path}: {name}: {field} disagrees with snapshot")
        dispatch = row.get("dispatch")
        if not isinstance(dispatch, dict) or dispatch.get("kind") not in DISPATCH_KINDS:
            raise MatrixError(f"{matrix_path}: {name}: invalid dispatch kind")
        if not isinstance(dispatch.get("target"), str) or not dispatch["target"]:
            raise MatrixError(f"{matrix_path}: {name}: invalid dispatch target")
        status = row.get("disposition")
        if status not in DISPOSITIONS:
            raise MatrixError(f"{matrix_path}: {name}: invalid disposition")
        counts[status] += 1
        for field in ("handler", "uapi_family"):
            if not isinstance(row.get(field), str) or not row[field]:
                raise MatrixError(f"{matrix_path}: {name}: missing {field}")
        ids = row.get("contract_ids")
        if not isinstance(ids, list) or len(ids) != len(set(ids)):
            raise MatrixError(f"{matrix_path}: {name}: invalid contract IDs")
        unknown = set(ids) - contracts
        if unknown:
            raise MatrixError(f"{matrix_path}: {name}: unknown contract IDs {sorted(unknown)}")
        lanes = row.get("evidence")
        if not isinstance(lanes, dict) or set(lanes) != EVIDENCE_LANES:
            raise MatrixError(f"{matrix_path}: {name}: evidence lanes must be exactly {sorted(EVIDENCE_LANES)}")
        for lane, references in lanes.items():
            if not isinstance(references, list) or not all(isinstance(reference, str) for reference in references):
                raise MatrixError(f"{matrix_path}: {name}: invalid {lane} evidence")
            absent = set(references) - evidence
            if absent:
                raise MatrixError(f"{matrix_path}: {name}: unknown evidence IDs {sorted(absent)}")
        if row.get("review") not in REVIEWS:
            raise MatrixError(f"{matrix_path}: {name}: invalid review state")
    missing = set(expected_by_name) - seen
    if missing:
        raise MatrixError(f"{matrix_path}: missing snapshot syscalls {sorted(missing)}")
    return counts


def regenerate() -> None:
    facts = source_rows(SNAPSHOT)
    document = load_json(MATRIX)
    old = {row["name"]: row for row in document.get("syscalls", []) if isinstance(row, dict) and "name" in row}
    generated = []
    for fact in facts:
        row = old.get(fact["name"], {})
        prior_dispatch = row.get("dispatch")
        if not isinstance(prior_dispatch, dict):
            prior_dispatch = {}
        # Linux's entry symbol is baseline evidence only. It says nothing about
        # a TheKernel dispatcher branch, so unreviewed rows remain unknown.
        default_kind = "unknown"
        default_target = "unknown"
        if row.get("review") == "unreviewed":
            prior_dispatch = {}
        dispatch = {
            "kind": prior_dispatch.get("kind", default_kind),
            "target": prior_dispatch.get("target", default_target),
        }
        generated.append({
            **fact,
            "dispatch": dispatch,
            "disposition": row.get("disposition", "unknown"),
            "handler": row.get("handler", "unknown"),
            "uapi_family": row.get("uapi_family", "unclassified"),
            "contract_ids": row.get("contract_ids", []),
            "evidence": row.get("evidence", {lane: [] for lane in sorted(EVIDENCE_LANES)}),
            "review": row.get("review", "unreviewed"),
        })
    document["baseline"] = EXPECTED
    document["syscalls"] = generated
    MATRIX.write_text(json.dumps(document, indent=2) + "\n", encoding="utf-8")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("command", choices=("validate", "regenerate"))
    args = parser.parse_args()
    try:
        if args.command == "regenerate":
            regenerate()
        counts = validate_paths()
    except MatrixError as error:
        print(f"abi-matrix: {error}")
        return 1
    print("abi-matrix: valid " + ", ".join(f"{key}={counts[key]}" for key in sorted(counts)))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
