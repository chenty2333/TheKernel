#!/usr/bin/env python3
"""Validate the checked-in, deliberately conservative UAPI exposure manifests."""
from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
ABI = ROOT / "docs/linux-abi"
TABLE = ABI / "linux-v6.12.103-arch-x86-entry-syscalls-syscall_64.tbl"
SURFACES = ABI / "uapi-surfaces-v1.json"
EXPOSURES = ABI / "exposure-inventory-v1.json"
SURFACE_SCHEMA = "thekernel-uapi-surfaces-v1"
EXPOSURE_SCHEMA = "thekernel-exposure-inventory-v1"
ROW_FIELDS = frozenset({"nr", "name", "flags", "commands", "structure_versions", "state_edges", "applicability", "closure"})


class AbiSurfaceError(ValueError):
    pass


def digest_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def digest_path(path: Path) -> str:
    return digest_bytes(path.read_bytes())


def canonical(value: Any) -> str:
    """The canonical hash omits only its own self-referential field."""
    value = dict(value)
    value.pop("canonical_hash", None)
    return digest_bytes(json.dumps(value, sort_keys=True, separators=(",", ":")).encode())


def load(path: Path) -> dict[str, Any]:
    try:
        result = json.loads(path.read_text())
    except (OSError, json.JSONDecodeError) as error:
        raise AbiSurfaceError(f"cannot load {path}: {error}") from error
    if not isinstance(result, dict):
        raise AbiSurfaceError(f"{path}: document must be an object")
    return result


def table() -> list[tuple[int, str]]:
    result = []
    for line_number, line in enumerate(TABLE.read_text().splitlines(), 1):
        fields = line.split()
        if not fields or fields[0].startswith("#"):
            continue
        if len(fields) < 3:
            raise AbiSurfaceError(f"{TABLE}:{line_number}: malformed syscall row")
        if fields[1] in {"common", "64"}:
            result.append((int(fields[0]), fields[2]))
    if len(result) != 375:
        raise AbiSurfaceError(f"{TABLE}: expected 375 common/64 syscalls, got {len(result)}")
    return result


def _check_sources(document: dict[str, Any]) -> None:
    sources = document.get("sources")
    if not isinstance(sources, list) or not sources:
        raise AbiSurfaceError("sources must be a non-empty list")
    seen = set()
    for source in sources:
        if set(source) != {"path", "sha256"} or not isinstance(source["path"], str):
            raise AbiSurfaceError("source must contain exactly path and sha256")
        path = ROOT / source["path"]
        if source["path"] in seen or not path.is_file():
            raise AbiSurfaceError(f"invalid source path {source['path']!r}")
        seen.add(source["path"])
        if source["sha256"] != digest_path(path):
            raise AbiSurfaceError(f"source hash drift: {source['path']}")


def _check_hash(document: dict[str, Any]) -> None:
    if document.get("canonical_hash") != canonical(document):
        raise AbiSurfaceError("canonical hash drift")


def validate_exposures(document: dict[str, Any]) -> dict[str, dict[str, Any]]:
    if document.get("schema") != EXPOSURE_SCHEMA:
        raise AbiSurfaceError("wrong exposure schema")
    _check_hash(document)
    _check_sources(document)
    rows = document.get("exposures")
    if not isinstance(rows, list) or not rows:
        raise AbiSurfaceError("exposures must be a non-empty list")
    result = {}
    for row in rows:
        required = {"id", "class", "user_obtainable", "external_uapi_surface", "support_set", "review"}
        if set(row) != required or not isinstance(row.get("id"), str):
            raise AbiSurfaceError("exposure row has an invalid schema")
        if row["id"] in result or row["review"] not in {"mapped", "unreviewed"}:
            raise AbiSurfaceError("duplicate exposure id or invalid review")
        if not isinstance(row["support_set"], list) or not isinstance(row["external_uapi_surface"], list):
            raise AbiSurfaceError("exposure support_set and external_uapi_surface must be lists")
        if row["review"] == "unreviewed" and row["support_set"]:
            raise AbiSurfaceError("unreviewed exposure cannot claim a support set")
        result[row["id"]] = row
    return result


def validate_surfaces(document: dict[str, Any], exposures: dict[str, dict[str, Any]]) -> list[dict[str, Any]]:
    if document.get("schema") != SURFACE_SCHEMA:
        raise AbiSurfaceError("wrong surface schema")
    _check_hash(document)
    _check_sources(document)
    known_source_paths = {source["path"] for source in document["sources"]}
    rows = document.get("syscalls")
    if not isinstance(rows, list) or len(rows) != 375:
        raise AbiSurfaceError("syscalls must contain exactly 375 rows")
    expected = table()
    actual = [(row.get("nr"), row.get("name")) for row in rows]
    if actual != expected:
        raise AbiSurfaceError("syscalls do not exactly match the pinned table order")
    for row in rows:
        if set(row) != ROW_FIELDS:
            raise AbiSurfaceError(f"{row.get('name')}: row fields must be explicit and exact")
        for field in ("flags", "commands", "structure_versions", "state_edges"):
            if not isinstance(row[field], list):
                raise AbiSurfaceError(f"{row['name']}: {field} must be a list")
        if row["applicability"] not in {"applicable", "N/A"}:
            raise AbiSurfaceError(f"{row['name']}: invalid applicability")
        closure = row["closure"]
        if set(closure) != {"status", "gap", "exposures", "source_paths", "hash"}:
            raise AbiSurfaceError(f"{row['name']}: invalid closure schema")
        if closure["status"] not in {"mapped", "unmapped"}:
            raise AbiSurfaceError(f"{row['name']}: invalid closure status")
        if not isinstance(closure["exposures"], list) or not isinstance(closure["source_paths"], list):
            raise AbiSurfaceError(f"{row['name']}: closure references must be lists")
        if any(item not in exposures for item in closure["exposures"]):
            raise AbiSurfaceError(f"{row['name']}: unknown exposure reference")
        if any(not isinstance(item, str) for item in closure["source_paths"]):
            raise AbiSurfaceError(f"{row['name']}: invalid source path reference")
        if any(item not in known_source_paths for item in closure["source_paths"]):
            raise AbiSurfaceError(f"{row['name']}: unknown source path reference")
        if closure["status"] == "unmapped" and not closure["gap"]:
            raise AbiSurfaceError(f"{row['name']}: unmapped syscall needs an explicit gap")
        if row["applicability"] == "N/A" and closure["status"] != "mapped":
            raise AbiSurfaceError(f"{row['name']}: N/A is reserved for mapped native-NI slots")
        projection = {"name": row["name"], "status": closure["status"], "exposures": closure["exposures"], "source_paths": closure["source_paths"], "exposure_hashes": {key: canonical(exposures[key]) for key in closure["exposures"]}}
        if closure["hash"] != canonical(projection):
            raise AbiSurfaceError(f"{row['name']}: resolved closure hash drift")
    return rows


def validate(surface_path: Path = SURFACES, exposure_path: Path = EXPOSURES) -> dict[str, Any]:
    exposure_doc = load(exposure_path)
    exposure_rows = validate_exposures(exposure_doc)
    surface_doc = load(surface_path)
    rows = validate_surfaces(surface_doc, exposure_rows)
    mapped = [row["name"] for row in rows if row["closure"]["status"] == "mapped"]
    return {"surface_hash": surface_doc["canonical_hash"], "exposure_hash": exposure_doc["canonical_hash"], "mapped": mapped, "unmapped": [row["name"] for row in rows if row["closure"]["status"] == "unmapped"]}


def affected_rows(surface_path: Path, old_exposure_path: Path, new_exposure_path: Path) -> list[str]:
    surface = load(surface_path)
    old, new = validate_exposures(load(old_exposure_path)), validate_exposures(load(new_exposure_path))
    changed = {key for key in set(old) | set(new) if key not in old or key not in new or canonical(old[key]) != canonical(new[key])}
    return [row["name"] for row in surface["syscalls"] if changed.intersection(row["closure"]["exposures"])]


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("validate", nargs="?", default="validate")
    parser.add_argument("--surfaces", type=Path, default=SURFACES)
    parser.add_argument("--exposures", type=Path, default=EXPOSURES)
    parser.add_argument("--baseline-exposures", type=Path)
    args = parser.parse_args()
    try:
        result = validate(args.surfaces, args.exposures)
        if args.baseline_exposures:
            result["affected_rows"] = affected_rows(args.surfaces, args.baseline_exposures, args.exposures)
    except AbiSurfaceError as error:
        raise SystemExit(f"abi-surfaces: {error}")
    print(json.dumps(result, sort_keys=True))


if __name__ == "__main__":
    main()
