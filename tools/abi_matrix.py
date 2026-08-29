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
GAP_CATALOG = ABI_DIR / "gap-catalog.json"
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
DISPATCH_KINDS = {"unknown", "dispatch-arm", "alias", "feature", "fallback", "native-ni"}
DISPOSITIONS = {"implemented", "partial", "explicit-enosys", "unknown"}
REVIEWS = {"unreviewed", "in-review", "reviewed"}
EVIDENCE_LANES = {"static-audit", "host-unit", "host-linux-differential", "guest-KTAP"}
EVIDENCE_STATUSES = {"pass", "not-applicable"}
EVIDENCE_OPTIONAL_FIELDS = {"symbol", "command", "required_markers", "case_name", "expected_plan", "reason"}
EVIDENCE_REQUIRED_FIELDS = {"id", "lane", "source", "source_sha256", "assertion", "status"}
GAP_REQUIRED_FIELDS = {"id", "dispositions", "description"}
NATIVE_LINUX_ROUTE = "native"
PLACEHOLDERS = {"", "unknown", "unclassified", "placeholder", "tbd", "todo", "n/a"}
NATIVE_SYS_NI_COUNT = 17
PHASE_GATES = {
    "phase1": {"reviewed": 375, "unknown": 0},
    # Linux v6.12.103 intentionally routes 17 native x86_64 table slots to
    # sys_ni_syscall.  They are resolved explicit-ENOSYS coverage, not missing
    # implementations, so the attainable final implementation count is 358.
    "final": {
        "reviewed": 375,
        "resolved": 375,
        "implemented": 358,
        "partial": 0,
        "explicit-enosys": 17,
        "unknown": 0,
    },
}


class MatrixError(ValueError):
    pass


def require_gate(counts: dict[str, int], gate: str) -> None:
    expected = PHASE_GATES[gate]
    mismatches = [
        f"{field}={counts.get(field)} (expected {value})"
        for field, value in expected.items()
        if counts.get(field) != value
    ]
    if mismatches:
        raise MatrixError(f"{gate} gate failed: " + ", ".join(mismatches))


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
    if sum(row["entry"] == "sys_ni_syscall" for row in rows) != NATIVE_SYS_NI_COUNT:
        raise MatrixError(
            f"{path}: expected exactly {NATIVE_SYS_NI_COUNT} native sys_ni_syscall rows"
        )
    return rows


def contract_cells(contract_dir: Path, evidence: dict[str, dict[str, Any]]) -> dict[str, dict[str, Any]]:
    cells_by_id: dict[str, dict[str, Any]] = {}
    for path in sorted(contract_dir.glob("*.json")):
        document = load_json(path)
        if (not isinstance(document, dict)
                or set(document) != {"schema", "family", "cells"}
                or document.get("schema") != "thekernel-linux-abi-contracts-v2"
                or not isinstance(document.get("family"), str)
                or not document["family"]):
            raise MatrixError(f"{path}: unsupported contract schema")
        if not isinstance(document.get("cells"), list):
            raise MatrixError(f"{path}: cells must be a list")
        for cell in document.get("cells", []):
            if not isinstance(cell, dict) or set(cell) != {
                "id", "syscalls", "subject", "review", "required_lanes", "evidence"
            }:
                raise MatrixError(f"{path}: invalid contract cell fields")
            identifier = cell.get("id")
            if not isinstance(identifier, str) or not identifier:
                raise MatrixError(f"{path}: invalid contract ID")
            if identifier in cells_by_id:
                raise MatrixError(f"{path}: duplicate contract ID {identifier}")
            references = cell.get("evidence")
            if (not isinstance(references, list)
                    or not all(isinstance(reference, str) for reference in references)):
                raise MatrixError(f"{path}: {identifier}: invalid evidence references")
            absent = set(references) - set(evidence)
            if absent:
                raise MatrixError(
                    f"{path}: {identifier}: unknown evidence IDs {sorted(absent)}"
                )
            syscalls = cell.get("syscalls")
            if (not isinstance(syscalls, list) or not syscalls
                    or len(syscalls) != len(set(syscalls))
                    or not all(isinstance(syscall, str) and syscall for syscall in syscalls)):
                raise MatrixError(f"{path}: {identifier}: invalid contract syscalls")
            if not isinstance(cell.get("subject"), str) or not cell["subject"]:
                raise MatrixError(f"{path}: {identifier}: invalid contract subject")
            if cell.get("review") not in REVIEWS:
                raise MatrixError(f"{path}: {identifier}: invalid contract review state")
            required_lanes = cell.get("required_lanes")
            if (not isinstance(required_lanes, list) or not required_lanes
                    or len(required_lanes) != len(set(required_lanes))
                    or not set(required_lanes) <= EVIDENCE_LANES):
                raise MatrixError(f"{path}: {identifier}: invalid required lanes")
            covered = {evidence[reference]["lane"] for reference in references
                       if evidence[reference]["status"] == "pass"}
            if not set(required_lanes) <= covered:
                raise MatrixError(f"{path}: {identifier}: evidence does not cover required lanes")
            cells_by_id[identifier] = cell
    return cells_by_id


def _validate_guest_ktap_evidence(source_path: Path, item: dict[str, Any]) -> None:
    try:
        text = source_path.read_text(encoding="utf-8")
    except UnicodeDecodeError as error:
        raise MatrixError(f"{source_path}: KTAP evidence is not UTF-8") from error
    if len(re.findall(r"(?m)^KTAP version 1$", text)) != 1:
        raise MatrixError(f"{source_path}: expected exactly one KTAP version line")
    expected_plan = item["expected_plan"]
    plans = re.findall(r"(?m)^1\.\.(\d+)$", text)
    if plans != [str(expected_plan)]:
        raise MatrixError(f"{source_path}: expected exactly one declared KTAP plan")
    results = re.findall(r"(?m)^(ok|not ok) (\d+) - ([^\r\n]+)$", text)
    if len(results) != expected_plan or [int(number) for _, number, _ in results] != list(range(1, expected_plan + 1)):
        raise MatrixError(f"{source_path}: expected complete ordered declared KTAP results")
    if any(status != "ok" for status, _, _ in results):
        raise MatrixError(f"{source_path}: failing KTAP result")
    if re.search(r"(?im)\bSKIP\b", text):
        raise MatrixError(f"{source_path}: skipped KTAP result")
    case_name = item["case_name"]
    case_results = [result for result in results if result[2] == case_name]
    if len(case_results) != 1:
        raise MatrixError(f"{source_path}: expected exactly one declared KTAP case {case_name}")
    for marker in item["required_markers"]:
        if len(re.findall(rf"(?m)^{re.escape(marker)}$", text)) != 1:
            raise MatrixError(f"{source_path}: missing or duplicate declared marker {marker}")


def evidence_ids(catalog_path: Path) -> dict[str, dict[str, Any]]:
    document = load_json(catalog_path)
    if (not isinstance(document, dict) or set(document) != {"schema", "evidence"}
            or document.get("schema") != "thekernel-linux-abi-evidence-catalog-v2"):
        raise MatrixError(f"{catalog_path}: unsupported evidence catalog schema")
    items = document.get("evidence")
    if not isinstance(items, list):
        raise MatrixError(f"{catalog_path}: evidence must be a list")
    entries: dict[str, dict[str, Any]] = {}
    repo_root = catalog_path.parents[2] if catalog_path.parent.name == "linux-abi" and catalog_path.parent.parent.name == "docs" else ROOT
    for item in items:
        if not isinstance(item, dict):
            raise MatrixError(f"{catalog_path}: evidence entry is not an object")
        fields = set(item)
        if not EVIDENCE_REQUIRED_FIELDS <= fields or not fields <= EVIDENCE_REQUIRED_FIELDS | EVIDENCE_OPTIONAL_FIELDS:
            raise MatrixError(f"{catalog_path}: invalid evidence fields")
        identifier = item.get("id")
        if not isinstance(identifier, str) or not identifier:
            raise MatrixError(f"{catalog_path}: invalid evidence ID")
        if identifier in entries:
            raise MatrixError(f"{catalog_path}: duplicate evidence ID {identifier}")
        lane = item.get("lane")
        if lane not in EVIDENCE_LANES:
            raise MatrixError(f"{catalog_path}: {identifier}: invalid evidence lane")
        if not isinstance(item.get("source"), str) or not item["source"]:
            raise MatrixError(f"{catalog_path}: {identifier}: invalid evidence source")
        if not isinstance(item.get("assertion"), str) or not item["assertion"]:
            raise MatrixError(f"{catalog_path}: {identifier}: invalid evidence assertion")
        if item.get("status") not in EVIDENCE_STATUSES:
            raise MatrixError(f"{catalog_path}: {identifier}: invalid evidence status")
        if item["status"] == "not-applicable" and (not isinstance(item.get("reason"), str) or not item["reason"]):
            raise MatrixError(f"{catalog_path}: {identifier}: not-applicable evidence requires reason")
        if item["status"] == "pass" and "reason" in item:
            raise MatrixError(f"{catalog_path}: {identifier}: pass evidence cannot have reason")
        relative = Path(item["source"])
        if relative.is_absolute() or ".." in relative.parts:
            raise MatrixError(f"{catalog_path}: {identifier}: source must be repository-relative")
        source_path = repo_root / relative
        try:
            payload = source_path.read_bytes()
        except OSError as error:
            raise MatrixError(f"{catalog_path}: {identifier}: missing evidence source: {error}") from error
        actual_hash = hashlib.sha256(payload).hexdigest()
        if not isinstance(item.get("source_sha256"), str) or item["source_sha256"] != actual_hash:
            raise MatrixError(f"{catalog_path}: {identifier}: evidence checksum drift")
        for field in ("symbol", "command", "case_name"):
            if field in item and (not isinstance(item[field], str) or not item[field]):
                raise MatrixError(f"{catalog_path}: {identifier}: invalid {field}")
        if "required_markers" in item and (not isinstance(item["required_markers"], list)
                                           or not item["required_markers"]
                                           or not all(isinstance(marker, str) and marker for marker in item["required_markers"])):
            raise MatrixError(f"{catalog_path}: {identifier}: invalid required markers")
        if lane == "host-linux-differential" and ("command" not in item or "required_markers" not in item):
            raise MatrixError(f"{catalog_path}: {identifier}: differential evidence requires command and markers")
        if lane == "guest-KTAP":
            if (not isinstance(item.get("expected_plan"), int) or item["expected_plan"] < 1
                    or "case_name" not in item or "required_markers" not in item):
                raise MatrixError(f"{catalog_path}: {identifier}: guest-KTAP requires case, plan, and markers")
            _validate_guest_ktap_evidence(source_path, item)
        entries[identifier] = item
    return entries


def _is_placeholder(value: Any) -> bool:
    return not isinstance(value, str) or value.strip().lower() in PLACEHOLDERS


def inventory_rows(path: Path) -> dict[str, dict[str, Any]]:
    document = load_json(path)
    if (not isinstance(document, dict)
            or document.get("schema") != "thekernel-linux-abi-static-inventory-v1"
            or not isinstance(document.get("syscalls"), list)):
        raise MatrixError(f"{path}: unsupported static inventory schema")
    sources = document.get("sources")
    if not isinstance(sources, dict) or set(sources) != {
        "dispatch", "linux_cond_syscall", "syscall_64_tbl"
    }:
        raise MatrixError(f"{path}: invalid static inventory sources")
    repo_root = path.parents[2]
    for label in ("dispatch", "syscall_64_tbl"):
        binding = sources[label]
        if (not isinstance(binding, dict)
                or set(binding) != {"path", "sha256"}
                or not isinstance(binding.get("path"), str)
                or not isinstance(binding.get("sha256"), str)):
            raise MatrixError(f"{path}: invalid static inventory {label} binding")
        relative = Path(binding["path"])
        if relative.is_absolute() or ".." in relative.parts:
            raise MatrixError(f"{path}: invalid static inventory {label} path")
        source_path = repo_root / relative
        try:
            digest = hashlib.sha256(source_path.read_bytes()).hexdigest()
        except OSError as error:
            raise MatrixError(f"{path}: missing static inventory {label} source: {error}") from error
        if binding["sha256"] != digest:
            raise MatrixError(f"{path}: static inventory {label} source hash drift")
    rows: dict[str, dict[str, Any]] = {}
    for row in document["syscalls"]:
        if not isinstance(row, dict) or not isinstance(row.get("name"), str) or row["name"] in rows:
            raise MatrixError(f"{path}: invalid static inventory syscall row")
        for field in ("linux_route", "dispatch", "implementation_root", "uapi_family"):
            if field not in row:
                raise MatrixError(f"{path}: {row['name']}: missing {field}")
        rows[row["name"]] = row
    if document.get("syscall_count") != len(rows):
        raise MatrixError(f"{path}: static inventory syscall count drift")
    return rows


def gap_catalog_ids(catalog_path: Path) -> dict[str, dict[str, Any]]:
    document = load_json(catalog_path)
    if (not isinstance(document, dict) or set(document) != {"schema", "gaps"}
            or document.get("schema") != "thekernel-linux-abi-gap-catalog-v1"
            or not isinstance(document.get("gaps"), list)):
        raise MatrixError(f"{catalog_path}: unsupported gap catalog schema")
    entries: dict[str, dict[str, Any]] = {}
    for item in document["gaps"]:
        if not isinstance(item, dict) or set(item) != GAP_REQUIRED_FIELDS:
            raise MatrixError(f"{catalog_path}: invalid gap entry fields")
        identifier = item.get("id")
        if not isinstance(identifier, str) or _is_placeholder(identifier) or identifier in entries:
            raise MatrixError(f"{catalog_path}: invalid or duplicate gap ID")
        dispositions = item.get("dispositions")
        if (not isinstance(dispositions, list) or not dispositions
                or len(dispositions) != len(set(dispositions))
                or not set(dispositions) <= DISPOSITIONS):
            raise MatrixError(f"{catalog_path}: {identifier}: invalid applicable dispositions")
        if _is_placeholder(item.get("description")):
            raise MatrixError(f"{catalog_path}: {identifier}: placeholder description")
        entries[identifier] = item
    return entries


def validate_paths(snapshot: Path = SNAPSHOT, matrix_path: Path = MATRIX,
                   catalog_path: Path = CATALOG, contract_dir: Path = CONTRACT_DIR) -> dict[str, int]:
    facts = source_rows(snapshot)
    document = load_json(matrix_path)
    if document.get("schema") != "thekernel-linux-abi-matrix-v2":
        raise MatrixError(f"{matrix_path}: unsupported schema")
    baseline = document.get("baseline")
    if baseline != EXPECTED:
        raise MatrixError(f"{matrix_path}: baseline drift")
    if hashlib.sha256(snapshot.read_bytes()).hexdigest() != EXPECTED["source_sha256"]:
        raise MatrixError(f"{snapshot}: snapshot checksum drift")
    evidence = evidence_ids(catalog_path)
    contracts = contract_cells(contract_dir, evidence)
    gaps = gap_catalog_ids(catalog_path.parent / GAP_CATALOG.name)
    inventory = inventory_rows(catalog_path.parent / "static-inventory.json")
    rows = document.get("syscalls")
    if not isinstance(rows, list) or len(rows) != len(facts):
        raise MatrixError(f"{matrix_path}: expected exactly {len(facts)} syscall rows")
    expected_by_name = {row["name"]: row for row in facts}
    seen: set[str] = set()
    counts = {status: 0 for status in DISPOSITIONS}
    counts.update({"reviewed": 0, "resolved": 0})
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
        static = inventory.get(name)
        if static is None:
            raise MatrixError(f"{matrix_path}: {name}: absent from static inventory")
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
        if (status == "explicit-enosys") != (expected["entry"] == "sys_ni_syscall"):
            raise MatrixError(
                f"{matrix_path}: {name}: explicit-enosys must match sys_ni_syscall entry"
            )
        if (dispatch["kind"] == "native-ni") != (expected["entry"] == "sys_ni_syscall"):
            raise MatrixError(
                f"{matrix_path}: {name}: native-ni dispatch must match sys_ni_syscall entry"
            )
        for field in ("handler", "uapi_family", "implementation_root"):
            if not isinstance(row.get(field), str) or not row[field]:
                raise MatrixError(f"{matrix_path}: {name}: missing {field}")
        ids = row.get("contract_ids")
        if not isinstance(ids, list) or len(ids) != len(set(ids)):
            raise MatrixError(f"{matrix_path}: {name}: invalid contract IDs")
        unknown = set(ids) - set(contracts)
        if unknown:
            raise MatrixError(f"{matrix_path}: {name}: unknown contract IDs {sorted(unknown)}")
        lanes = row.get("evidence")
        if not isinstance(lanes, dict) or set(lanes) != EVIDENCE_LANES:
            raise MatrixError(f"{matrix_path}: {name}: evidence lanes must be exactly {sorted(EVIDENCE_LANES)}")
        for lane, references in lanes.items():
            if not isinstance(references, list) or not all(isinstance(reference, str) for reference in references):
                raise MatrixError(f"{matrix_path}: {name}: invalid {lane} evidence")
            absent = set(references) - set(evidence)
            if absent:
                raise MatrixError(f"{matrix_path}: {name}: unknown evidence IDs {sorted(absent)}")
            wrong_lane = [identifier for identifier in references if evidence[identifier]["lane"] != lane]
            if wrong_lane:
                raise MatrixError(f"{matrix_path}: {name}: evidence IDs do not match {lane} lane")
        if row.get("review") not in REVIEWS:
            raise MatrixError(f"{matrix_path}: {name}: invalid review state")
        gap_ids = row.get("gap_ids")
        if (not isinstance(gap_ids, list) or len(gap_ids) != len(set(gap_ids))
                or not all(isinstance(identifier, str) and identifier for identifier in gap_ids)):
            raise MatrixError(f"{matrix_path}: {name}: invalid gap IDs")
        unknown_gaps = set(gap_ids) - set(gaps)
        if unknown_gaps:
            raise MatrixError(f"{matrix_path}: {name}: unknown gap IDs {sorted(unknown_gaps)}")
        inapplicable_gaps = [identifier for identifier in gap_ids if status not in gaps[identifier]["dispositions"]]
        if inapplicable_gaps:
            raise MatrixError(f"{matrix_path}: {name}: gap IDs do not apply to {status}")
        review_evidence = row.get("review_evidence")
        if (not isinstance(review_evidence, list)
                or len(review_evidence) != len(set(review_evidence))
                or not all(isinstance(identifier, str) and identifier for identifier in review_evidence)):
            raise MatrixError(f"{matrix_path}: {name}: invalid review evidence")
        absent = set(review_evidence) - set(evidence)
        if absent:
            raise MatrixError(f"{matrix_path}: {name}: unknown review evidence IDs {sorted(absent)}")
        row_evidence = {identifier for references in lanes.values() for identifier in references}
        if not set(review_evidence) <= row_evidence:
            raise MatrixError(f"{matrix_path}: {name}: review evidence is not bound to row evidence")

        is_reviewed = row["review"] == "reviewed"
        if is_reviewed or not all(_is_placeholder(row[field]) for field in (
            "linux_route", "implementation_root", "uapi_family"
        )) or dispatch["kind"] != "unknown" or not _is_placeholder(dispatch["target"]):
            for field in ("linux_route", "dispatch", "implementation_root", "uapi_family"):
                if row[field] != static[field]:
                    raise MatrixError(f"{matrix_path}: {name}: {field} disagrees with static inventory")
        if is_reviewed and row["handler"] != static["dispatch"]["target"]:
            raise MatrixError(f"{matrix_path}: {name}: handler disagrees with static dispatch target")
        if is_reviewed:
            counts["reviewed"] += 1
            if status == "unknown":
                raise MatrixError(f"{matrix_path}: {name}: reviewed syscall cannot be unknown")
            if (_is_placeholder(dispatch["target"])
                    or any(_is_placeholder(row[field]) for field in (
                        "handler", "uapi_family", "implementation_root"))):
                raise MatrixError(f"{matrix_path}: {name}: reviewed syscall has placeholder metadata")
            if not review_evidence:
                raise MatrixError(f"{matrix_path}: {name}: reviewed syscall lacks review evidence")
            if not any(evidence[identifier]["lane"] == "static-audit"
                       and evidence[identifier]["status"] == "pass"
                       for identifier in review_evidence):
                raise MatrixError(f"{matrix_path}: {name}: review evidence lacks static-audit pass")
        if status == "partial" and not gap_ids:
            raise MatrixError(f"{matrix_path}: {name}: partial syscall requires gap IDs")
        if status in {"implemented", "explicit-enosys"} and gap_ids:
            raise MatrixError(f"{matrix_path}: {name}: {status} syscall cannot have gap IDs")
        resolved = (
            is_reviewed
            and status in {"implemented", "explicit-enosys"}
            and bool(ids)
            and bool(review_evidence)
            and all(
                name in contract["syscalls"]
                and contract["review"] == "reviewed"
                and bool(contract["evidence"])
                for identifier in ids
                for contract in (contracts[identifier],)
            )
        )
        if ids:
            for identifier in ids:
                if name not in contracts[identifier]["syscalls"]:
                    raise MatrixError(f"{matrix_path}: {name}: contract {identifier} is bound to another syscall")
        if status == "implemented" and not resolved:
            raise MatrixError(
                f"{matrix_path}: {name}: implemented syscall is not evidence-resolved"
            )
        if resolved:
            for lane in EVIDENCE_LANES:
                references = lanes[lane]
                if not any(evidence[identifier]["lane"] == lane and evidence[identifier]["status"] == "pass"
                           for identifier in references):
                    raise MatrixError(f"{matrix_path}: {name}: resolved syscall lacks {lane} pass evidence")
        if resolved:
            counts["resolved"] += 1
    missing = set(expected_by_name) - seen
    if missing:
        raise MatrixError(f"{matrix_path}: missing snapshot syscalls {sorted(missing)}")
    rows_by_name = {row["name"]: row for row in rows}
    for identifier, contract in contracts.items():
        for name in contract["syscalls"]:
            row = rows_by_name.get(name)
            if row is None or identifier not in row["contract_ids"]:
                raise MatrixError(f"{matrix_path}: contract {identifier} is not bound by syscall {name}")
    if counts["resolved"] > counts["reviewed"]:
        raise MatrixError(f"{matrix_path}: resolved count exceeds reviewed count")
    return counts


def regenerate() -> None:
    facts = source_rows(SNAPSHOT)
    document = load_json(MATRIX)
    static = inventory_rows(ABI_DIR / "static-inventory.json")
    old = {row["name"]: row for row in document.get("syscalls", []) if isinstance(row, dict) and "name" in row}
    generated = []
    for fact in facts:
        row = old.get(fact["name"], {})
        static_row = static.get(fact["name"])
        if static_row is None:
            raise MatrixError(f"{ABI_DIR / 'static-inventory.json'}: missing {fact['name']}")
        # Regeneration refreshes static facts but never grants review.  Review
        # is a human/evidence state and must survive only when it was already
        # present on the row.  New non-NI rows remain conservative partials
        # until their runtime contract is closed.
        dispatch = static_row["dispatch"]
        if fact["entry"] == "sys_ni_syscall":
            disposition = "explicit-enosys"
        else:
            prior = row.get("disposition")
            disposition = prior if prior in {"implemented", "partial"} else "partial"
        gaps = []
        if disposition == "partial":
            gaps = [
                "review.dynamic-contract-evidence-unclosed",
                f"family.{static_row['uapi_family']}-semantics-unclosed",
            ]
            if dispatch["kind"] == "fallback":
                gaps.append("dispatch.fallback-not-linux-ni")
            elif dispatch["kind"] == "feature":
                gaps.append("dispatch.feature-path-unverified")
        lanes = row.get("evidence", {})
        lanes = {lane: list(lanes.get(lane, [])) for lane in EVIDENCE_LANES}
        if "matrix.static-audit.inventory-v1" not in lanes["static-audit"]:
            lanes["static-audit"].append("matrix.static-audit.inventory-v1")
        review = row.get("review")
        if review not in REVIEWS:
            review = "unreviewed"
        review_evidence = row.get("review_evidence")
        if not isinstance(review_evidence, list):
            review_evidence = []
        generated.append({
            **fact,
            "linux_route": static_row["linux_route"],
            "dispatch": dispatch,
            "disposition": disposition,
            "handler": dispatch["target"],
            "uapi_family": static_row["uapi_family"],
            "implementation_root": static_row["implementation_root"],
            "contract_ids": row.get("contract_ids", []),
            "evidence": lanes,
            "gap_ids": gaps,
            "review_evidence": review_evidence,
            "review": review,
        })
    document["schema"] = "thekernel-linux-abi-matrix-v2"
    document["baseline"] = EXPECTED
    document["syscalls"] = generated
    MATRIX.write_text(json.dumps(document, indent=2) + "\n", encoding="utf-8")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "command", choices=("validate", "regenerate", "phase1-gate", "final-gate")
    )
    args = parser.parse_args()
    try:
        if args.command == "regenerate":
            regenerate()
        counts = validate_paths()
        if args.command.endswith("-gate"):
            require_gate(counts, args.command.removesuffix("-gate"))
    except MatrixError as error:
        print(f"abi-matrix: {error}")
        return 1
    print("abi-matrix: valid " + ", ".join(f"{key}={counts[key]}" for key in sorted(counts)))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
