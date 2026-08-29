#!/usr/bin/env python3
"""Declarative Linux ABI case manifests, guest markers, and receipts.

This module deliberately has no QEMU or shell integration.  Its small API is
used by the eventual guest runner and by CI to keep case selection and evidence
validation deterministic.
"""

from __future__ import annotations

import hashlib
import json
import re
import subprocess
import sys
from pathlib import Path
from typing import Any, Iterable

ROOT = Path(__file__).resolve().parents[1]
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

from scripts.ci import source_combination


MANIFEST = ROOT / "docs" / "linux-abi" / "abi-cases.json"
RECEIPT_CLOSURE_INPUTS = (
    "docs/linux-abi/abi-cases.json",
    "docs/linux-abi/closure-cohorts-v1.json",
    "docs/linux-abi/conditional-syscalls-v1.json",
    "docs/linux-abi/evidence-catalog.json",
    "docs/linux-abi/exposure-inventory-v1.json",
    "docs/linux-abi/gap-catalog.json",
    "docs/linux-abi/oracle-configs.json",
    "docs/linux-abi/static-inventory.json",
    "docs/linux-abi/syscall-matrix.json",
    "docs/linux-abi/uapi-headers.json",
    "docs/linux-abi/uapi-surfaces-v1.json",
)
SCHEMA = "thekernel-linux-abi-cases-v1"
EXPECTED = frozenset({"pass", "enosys", "skip-permitted"})
TIERS = frozenset({"smoke", "differential", "nightly"})
TARGETS = frozenset({"linux-product", "linux-feature-witness", "thekernel"})
CASE_ID = re.compile(r"^[a-z0-9][a-z0-9._-]*$")
MARKER_ID = re.compile(r"^[A-Z][A-Z0-9_]*$")
SHA256 = re.compile(r"^[0-9a-f]{64}$")
CASE_MARKER = re.compile(r"^THEKERNEL_ABI_CASE ([a-z0-9][a-z0-9._-]*)$")
ASSERT_MARKER = re.compile(
    r"^THEKERNEL_ABI_ASSERT ([a-z0-9][a-z0-9._-]*) ([A-Z][A-Z0-9_]*) (pass|fail|skip|enosys)$"
)
RESULT_MARKER = re.compile(r"^THEKERNEL_ABI_RESULT ([a-z0-9][a-z0-9._-]*) (pass|fail|skip|enosys)$")


class AbiCaseError(ValueError):
    pass


def _canonical(value: Any) -> bytes:
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=True).encode()


def sha256_bytes(payload: bytes) -> str:
    return hashlib.sha256(payload).hexdigest()


def _safe_relative(value: Any, field: str) -> str:
    if not isinstance(value, str) or not value:
        raise AbiCaseError(f"{field}: expected a non-empty path")
    path = Path(value)
    if path.is_absolute() or ".." in path.parts or path == Path("."):
        raise AbiCaseError(f"{field}: unsafe path {value!r}")
    return path.as_posix()


def _string_list(value: Any, field: str, *, nonempty: bool = False) -> list[str]:
    if not isinstance(value, list) or (nonempty and not value) or not all(isinstance(item, str) and item for item in value):
        raise AbiCaseError(f"{field}: expected {'a non-empty ' if nonempty else 'a '}list of strings")
    if len(value) != len(set(value)):
        raise AbiCaseError(f"{field}: duplicate values")
    return value


def _sha256(value: Any, field: str) -> str:
    if not isinstance(value, str) or not SHA256.fullmatch(value):
        raise AbiCaseError(f"{field}: expected a 64-hex SHA-256")
    return value


def _validate_oracle_config(identifier: str, target: str, config: Any) -> None:
    if not isinstance(config, dict):
        raise AbiCaseError(f"{identifier}.oracle_configs.{target}: expected an object")
    config_id = config.get("config_id")
    if not isinstance(config_id, str) or not config_id:
        raise AbiCaseError(f"{identifier}.oracle_configs.{target}: missing config_id")
    _sha256(config.get("config_sha256"), f"{identifier}.oracle_configs.{target}.config_sha256")
    kernel = config.get("kernel_sha256")
    source_combination_id = config.get("source_combination_id")
    if target == "thekernel":
        if kernel is not None or source_combination_id is not None:
            raise AbiCaseError(f"{identifier}.oracle_configs.thekernel: must not pin a runtime source identity")
        return
    if kernel is None or source_combination_id is not None:
        raise AbiCaseError(f"{identifier}.oracle_configs.{target}: requires kernel_sha256 only")
    if kernel is not None:
        _sha256(kernel, f"{identifier}.oracle_configs.{target}.kernel_sha256")


def _expected_oracle_configs(repo_root: Path) -> dict[str, dict[str, str]]:
    path = repo_root / "docs" / "linux-abi" / "oracle-configs.json"
    try:
        document = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise AbiCaseError(f"{path}: {error}") from error
    oracles = document.get("oracles") if isinstance(document, dict) else None
    if not isinstance(oracles, list):
        raise AbiCaseError(f"{path}: missing oracle profiles")
    by_id = {
        oracle.get("id"): oracle
        for oracle in oracles
        if isinstance(oracle, dict) and isinstance(oracle.get("id"), str)
    }
    expected: dict[str, dict[str, str]] = {}
    for target, profile_id in (
        ("linux-product", "q35-product"),
        ("linux-feature-witness", "q35-feature-witness"),
    ):
        oracle = by_id.get(profile_id)
        if not isinstance(oracle, dict):
            raise AbiCaseError(f"{path}: missing {profile_id}")
        configuration, artifact = oracle.get("configuration"), oracle.get("artifact")
        if not isinstance(configuration, dict) or not isinstance(artifact, dict):
            raise AbiCaseError(f"{path}: invalid {profile_id}")
        expected[target] = {
            "config_id": profile_id,
            "config_sha256": _sha256(
                configuration.get("final_config_sha256"),
                f"{profile_id}.configuration.final_config_sha256",
            ),
            "kernel_sha256": _sha256(
                artifact.get("sha256"), f"{profile_id}.artifact.sha256"
            ),
        }
    combination = repo_root / "config" / "source-combination.toml"
    try:
        combination_hash = sha256_bytes(combination.read_bytes())
    except OSError as error:
        raise AbiCaseError(f"{combination}: {error}") from error
    expected["thekernel"] = {
        "config_id": "source-combination",
        "config_sha256": combination_hash,
    }
    return expected


def load_manifest(path: Path = MANIFEST, *, repo_root: Path = ROOT) -> list[dict[str, Any]]:
    try:
        document = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise AbiCaseError(f"{path}: {error}") from error
    if not isinstance(document, dict) or document.get("schema") != SCHEMA:
        raise AbiCaseError(f"{path}: unsupported ABI case schema")
    cases = document.get("cases")
    if not isinstance(cases, list) or not cases:
        raise AbiCaseError(f"{path}: cases must be a non-empty list")
    ids: set[str] = set()
    expected_configs = _expected_oracle_configs(repo_root)
    for index, case in enumerate(cases):
        prefix = f"{path}: case {index}"
        if not isinstance(case, dict):
            raise AbiCaseError(f"{prefix}: case is not an object")
        required = {"id", "syscalls", "source", "binary", "args", "targets", "oracle_configs", "tier", "resources", "timeout", "expected", "required_markers"}
        if set(case) != required:
            raise AbiCaseError(f"{prefix}: fields must be exactly {sorted(required)}")
        identifier = case["id"]
        if not isinstance(identifier, str) or not CASE_ID.fullmatch(identifier):
            raise AbiCaseError(f"{prefix}: invalid id")
        if identifier in ids:
            raise AbiCaseError(f"{prefix}: duplicate id {identifier}")
        ids.add(identifier)
        _string_list(case["syscalls"], f"{identifier}.syscalls", nonempty=True)
        sources = _string_list(case["source"], f"{identifier}.source", nonempty=True)
        for source in sources:
            source_path = repo_root / _safe_relative(source, f"{identifier}.source")
            if not source_path.is_file():
                raise AbiCaseError(f"{identifier}.source: missing {source}")
        _safe_relative(case["binary"], f"{identifier}.binary")
        _string_list(case["args"], f"{identifier}.args")
        targets = _string_list(case["targets"], f"{identifier}.targets", nonempty=True)
        if not set(targets) <= TARGETS:
            raise AbiCaseError(f"{identifier}.targets: unsupported target")
        configs = case["oracle_configs"]
        if not isinstance(configs, dict) or set(configs) != set(targets):
            raise AbiCaseError(f"{identifier}.oracle_configs: require one object per target")
        for target in targets:
            _validate_oracle_config(identifier, target, configs[target])
            if configs[target] != expected_configs[target]:
                raise AbiCaseError(
                    f"{identifier}.oracle_configs.{target}: differs from checked-in oracle identity"
                )
        if case["tier"] not in TIERS:
            raise AbiCaseError(f"{identifier}.tier: invalid tier")
        if not isinstance(case["resources"], dict) or not case["resources"]:
            raise AbiCaseError(f"{identifier}.resources: expected a non-empty object")
        resources = case["resources"]
        if set(resources) != {"cpus", "memory_mib", "profile_timeout_seconds"}:
            raise AbiCaseError(
                f"{identifier}.resources: require exactly cpus, memory_mib, and profile_timeout_seconds"
            )
        for field in ("cpus", "memory_mib", "profile_timeout_seconds"):
            value = resources[field]
            if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
                raise AbiCaseError(f"{identifier}.resources.{field}: must be a positive integer")
        timeout = case["timeout"]
        if isinstance(timeout, bool) or not isinstance(timeout, (int, float)) or not 0 < timeout <= 3600:
            raise AbiCaseError(f"{identifier}.timeout: must be within (0, 3600]")
        if timeout > resources["profile_timeout_seconds"]:
            raise AbiCaseError(
                f"{identifier}.timeout: cannot exceed resources.profile_timeout_seconds"
            )
        if case["expected"] not in EXPECTED:
            raise AbiCaseError(f"{identifier}.expected: invalid expected result")
        markers = _string_list(case["required_markers"], f"{identifier}.required_markers")
        if not all(MARKER_ID.fullmatch(marker) for marker in markers):
            raise AbiCaseError(f"{identifier}.required_markers: invalid marker")
    return cases


def is_gate_eligible(case: dict[str, Any]) -> bool:
    """Whether a resolved case may participate in a no-skip gate."""
    return case.get("expected") in {"pass", "enosys"}


def selected_resources(cases: Iterable[dict[str, Any]]) -> dict[str, int]:
    """Return the one declared q35 resource profile shared by selected cases."""
    selected = list(cases)
    if not selected:
        raise AbiCaseError("resource selection requires at least one case")
    profiles = [case.get("resources") for case in selected]
    first = profiles[0]
    if not isinstance(first, dict) or set(first) != {"cpus", "memory_mib", "profile_timeout_seconds"}:
        raise AbiCaseError("case resources are not a declared q35 profile")
    canonical = {name: first[name] for name in sorted(first)}
    for name, value in canonical.items():
        if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
            raise AbiCaseError(f"case resources.{name}: must be a positive integer")
    if any(profile != canonical for profile in profiles[1:]):
        raise AbiCaseError("selected ABI cases require incompatible resource profiles")
    return canonical


def shard_cases(cases: Iterable[dict[str, Any]], shard: int, shards: int) -> list[dict[str, Any]]:
    if not isinstance(shards, int) or isinstance(shards, bool) or shards < 1:
        raise AbiCaseError("shards must be a positive integer")
    if not isinstance(shard, int) or isinstance(shard, bool) or not 0 <= shard < shards:
        raise AbiCaseError("shard must be in [0, shards)")
    return [case for case in cases if int.from_bytes(hashlib.sha256(case["id"].encode()).digest(), "big") % shards == shard]


def parse_transcript(transcript: str) -> dict[str, Any]:
    if not isinstance(transcript, str):
        raise AbiCaseError("transcript must be text")
    parsed: dict[str, Any] = {"cases": [], "assertions": [], "results": [], "markers": []}
    for number, line in enumerate(transcript.splitlines(), 1):
        if match := CASE_MARKER.fullmatch(line):
            parsed["cases"].append((match.group(1), number))
        elif match := ASSERT_MARKER.fullmatch(line):
            parsed["assertions"].append((match.group(1), match.group(2), match.group(3), number))
        elif match := RESULT_MARKER.fullmatch(line):
            parsed["results"].append((match.group(1), match.group(2), number))
        elif MARKER_ID.fullmatch(line):
            parsed["markers"].append((line, number))
    return parsed


def validate_transcript(transcript: str, cases: Iterable[dict[str, Any]]) -> list[dict[str, Any]]:
    selected = list(cases)
    expected = {case["id"]: case for case in selected}
    if len(expected) != len(selected):
        raise AbiCaseError("selected cases contain duplicate IDs")
    parsed = parse_transcript(transcript)
    begins: dict[str, list[int]] = {}
    results: dict[str, list[tuple[str, int]]] = {}
    assertions: dict[str, list[tuple[str, str, int]]] = {}
    markers: dict[str, list[int]] = {}
    for identifier, line in parsed["cases"]:
        begins.setdefault(identifier, []).append(line)
    for identifier, result, line in parsed["results"]:
        results.setdefault(identifier, []).append((result, line))
    for identifier, name, result, line in parsed["assertions"]:
        if any(prior_name == name for prior_name, _, _ in assertions.get(identifier, [])):
            raise AbiCaseError(f"duplicate assertion {identifier}/{name} at line {line}")
        assertions.setdefault(identifier, []).append((name, result, line))
        if result == "fail":
            raise AbiCaseError(f"failed assertion {identifier}/{name} at line {line}")
        if result == "skip" and expected.get(identifier, {}).get("expected") != "skip-permitted":
            raise AbiCaseError(f"unallowed skip assertion for {identifier} at line {line}")
    for marker, line in parsed["markers"]:
        markers.setdefault(marker, []).append(line)
    seen = set(begins) | set(results) | set(assertions)
    unknown = seen - set(expected)
    if unknown:
        raise AbiCaseError(f"transcript contains unknown cases {sorted(unknown)}")
    receipts: list[dict[str, Any]] = []
    for identifier, case in expected.items():
        if len(begins.get(identifier, [])) != 1:
            raise AbiCaseError(f"{identifier}: expected exactly one case marker")
        if len(results.get(identifier, [])) != 1:
            raise AbiCaseError(f"{identifier}: expected exactly one result marker")
        outcome, line = results[identifier][0]
        begin = begins[identifier][0]
        if line <= begin:
            raise AbiCaseError(f"{identifier}: result must follow case marker")
        if not assertions.get(identifier):
            raise AbiCaseError(f"{identifier}: expected at least one assertion")
        if len(assertions[identifier]) < len(case["syscalls"]):
            raise AbiCaseError(f"{identifier}: expected at least one assertion per syscall")
        expected_assertion = {
            "pass": "pass",
            "enosys": "enosys",
            "skip-permitted": "skip",
        }[case["expected"]]
        for name, assertion_outcome, assertion_line in assertions[identifier]:
            if not begin < assertion_line < line:
                raise AbiCaseError(f"{identifier}: assertion must be between case and result")
            if assertion_outcome != expected_assertion:
                raise AbiCaseError(
                    f"{identifier}: assertion {name} expected {expected_assertion}, "
                    f"got {assertion_outcome}"
                )
        if outcome == "skip" and case["expected"] != "skip-permitted":
            raise AbiCaseError(f"{identifier}: unallowed skip at line {line}")
        expected_outcome = "skip" if case["expected"] == "skip-permitted" else case["expected"]
        if outcome != expected_outcome:
            raise AbiCaseError(f"{identifier}: expected {case['expected']}, got {outcome}")
        for marker in case["required_markers"]:
            marker_lines = markers.get(marker, [])
            if len(marker_lines) != 1:
                raise AbiCaseError(f"{identifier}: required marker {marker} missing or duplicate")
            if not begin < marker_lines[0] < line:
                raise AbiCaseError(f"{identifier}: required marker {marker} must be between case and result")
        receipts.append({"id": identifier, "outcome": outcome})
    ordered = sorted((line, identifier, "case") for identifier, lines in begins.items() for line in lines)
    ordered += sorted((line, identifier, "result") for identifier, values in results.items() for _, line in values)
    active: str | None = None
    for _, identifier, kind in sorted(ordered):
        if kind == "case":
            if active is not None:
                raise AbiCaseError(f"{identifier}: case marker before {active} result")
            active = identifier
        else:
            if active != identifier:
                raise AbiCaseError(f"{identifier}: result is outside its case boundary")
            active = None
    return receipts


def ktap_plan(results: Iterable[dict[str, Any]]) -> str:
    entries = list(results)
    lines = ["KTAP version 1", f"1..{len(entries)}"]
    for number, entry in enumerate(entries, 1):
        identifier, outcome = entry["id"], entry["outcome"]
        if outcome == "pass":
            lines.append(f"ok {number} - {identifier}")
        elif outcome == "skip":
            lines.append(f"ok {number} - {identifier} # SKIP declared")
        elif outcome == "enosys":
            lines.append(f"ok {number} - {identifier} # ENOSYS expected")
        else:
            lines.append(f"not ok {number} - {identifier}")
    return "\n".join(lines) + "\n"


def _file_hash(repo_root: Path, relative: str) -> str:
    path = repo_root / _safe_relative(relative, "receipt path")
    try:
        return sha256_bytes(path.read_bytes())
    except OSError as error:
        raise AbiCaseError(f"receipt path missing {relative}: {error}") from error


def _closure_inputs(repo_root: Path) -> dict[str, Any]:
    files = {relative: _file_hash(repo_root, relative) for relative in RECEIPT_CLOSURE_INPUTS}
    contract_root = repo_root / "docs" / "linux-abi" / "contracts"
    contracts = sorted(contract_root.glob("*.json"))
    if not contracts:
        raise AbiCaseError("receipt inputs contain no ABI contracts")
    for path in contracts:
        try:
            relative = path.relative_to(repo_root).as_posix()
        except ValueError as error:
            raise AbiCaseError(f"contract path escapes repository: {path}") from error
        files[relative] = _file_hash(repo_root, relative)
    return {
        "files": files,
        "sha256": sha256_bytes(_canonical(files)),
    }


def _git(repo: Path, *args: str) -> str:
    try:
        return subprocess.run(
            ("git", "-C", str(repo), *args), check=True, capture_output=True,
            text=True,
        ).stdout.strip()
    except (OSError, subprocess.CalledProcessError) as error:
        raise AbiCaseError(f"source_identity: cannot inspect checkout {repo}: {error}") from error


def capture_source_identity(repo_root: Path) -> dict[str, Any]:
    """Capture the clean, declared three-checkout source state at execution time."""
    repo_root = Path(repo_root).resolve()
    if _git(repo_root, "rev-parse", "--show-toplevel") != str(repo_root):
        raise AbiCaseError(f"source_identity: repo_root is not a checkout top-level: {repo_root}")
    try:
        declared = source_combination.load(repo_root / "config" / "source-combination.toml")
    except source_combination.SourceCombinationError as error:
        raise AbiCaseError(f"source_identity: cannot load declared source combination: {error}") from error
    checkouts = {"thekernel": repo_root}
    checkouts.update({name: repo_root.parent / source.path for name, source in declared.items()})
    sources: dict[str, dict[str, Any]] = {}
    for name, checkout in checkouts.items():
        checkout = checkout.resolve()
        if _git(checkout, "rev-parse", "--show-toplevel") != str(checkout):
            raise AbiCaseError(f"source_identity.sources.{name}: checkout is not a top-level: {checkout}")
        clean = not _git(checkout, "status", "--porcelain=v1", "--untracked-files=all")
        if not clean:
            raise AbiCaseError(f"source_identity.sources.{name}.clean: must be true")
        sources[name] = {
            "commit": _git(checkout, "rev-parse", "HEAD^{commit}"),
            "tree": _git(checkout, "rev-parse", "HEAD^{tree}"),
            "clean": True,
        }
    for name in ("ax", "linux_abi"):
        if sources[name]["commit"] != declared[name].ref:
            raise AbiCaseError(
                f"source_identity.sources.{name}.commit: differs from declared source combination"
            )
    identity = {
        "schema": 1,
        "combination_id": source_combination.combination_id(declared, sources["thekernel"]["commit"]),
        "sources": sources,
    }
    return _validate_source_identity(identity, repo_root=repo_root)


def _validate_source_identity(identity: Any, *, repo_root: Path) -> dict[str, Any]:
    """Require the actual three-checkout identity, never file-content stand-ins."""
    if not isinstance(identity, dict) or set(identity) != {"schema", "combination_id", "sources"}:
        raise AbiCaseError("source_identity: require schema, combination_id, and sources")
    if identity["schema"] != 1:
        raise AbiCaseError("source_identity.schema: expected 1")
    combination_id = identity["combination_id"]
    if not isinstance(combination_id, str) or not re.fullmatch(r"source-combination-v1-[0-9a-f]{64}", combination_id):
        raise AbiCaseError("source_identity.combination_id: invalid source combination")
    sources = identity["sources"]
    if not isinstance(sources, dict) or set(sources) != {"thekernel", "ax", "linux_abi"}:
        raise AbiCaseError("source_identity.sources: expected thekernel, ax, and linux_abi")
    canonical_sources: dict[str, dict[str, Any]] = {}
    for name in sorted(sources):
        source = sources[name]
        if not isinstance(source, dict) or set(source) != {"commit", "tree", "clean"}:
            raise AbiCaseError(f"source_identity.sources.{name}: require commit, tree, and clean")
        commit, tree, clean = source["commit"], source["tree"], source["clean"]
        if not isinstance(commit, str) or not re.fullmatch(r"[0-9a-f]{40}", commit):
            raise AbiCaseError(f"source_identity.sources.{name}.commit: expected a 40-hex commit")
        if not isinstance(tree, str) or not re.fullmatch(r"[0-9a-f]{40}", tree):
            raise AbiCaseError(f"source_identity.sources.{name}.tree: expected a 40-hex tree")
        if not isinstance(clean, bool) or not clean:
            raise AbiCaseError(f"source_identity.sources.{name}.clean: must be true")
        canonical_sources[name] = {"commit": commit, "tree": tree, "clean": clean}
    try:
        declared = source_combination.load(Path(repo_root) / "config" / "source-combination.toml")
        for name in ("ax", "linux_abi"):
            if canonical_sources[name]["commit"] != declared[name].ref:
                raise AbiCaseError(
                    f"source_identity.sources.{name}.commit: differs from declared source combination"
                )
        recomputed = source_combination.combination_id(
            declared, canonical_sources["thekernel"]["commit"]
        )
    except source_combination.SourceCombinationError as error:
        raise AbiCaseError(f"source_identity: cannot load declared source combination: {error}") from error
    if combination_id != recomputed:
        raise AbiCaseError("source_identity.combination_id: does not match declared source combination")
    return {"schema": 1, "combination_id": combination_id, "sources": canonical_sources}


def build_receipt(case: dict[str, Any], *, repo_root: Path, command: list[str], target: str,
                  exit_code: int, transcript: str) -> dict[str, Any]:
    if target not in case["targets"]:
        raise AbiCaseError(f"{case['id']}: undeclared target {target}")
    if not isinstance(command, list) or not all(isinstance(part, str) for part in command):
        raise AbiCaseError("command must be a list of strings")
    if not isinstance(exit_code, int) or isinstance(exit_code, bool):
        raise AbiCaseError("exit_code must be an integer")
    if exit_code != 0:
        raise AbiCaseError(f"{case['id']}: successful ABI evidence requires exit code zero")
    results = validate_transcript(transcript, [case])
    expected_outcome = "skip" if case["expected"] == "skip-permitted" else case["expected"]
    if results != [{"id": case["id"], "outcome": expected_outcome}]:
        raise AbiCaseError(f"{case['id']}: transcript result does not match the case contract")
    canonical_identity = capture_source_identity(repo_root)
    oracle = case["oracle_configs"][target]
    _validate_oracle_config(case["id"], target, oracle)
    return {
        "schema": "thekernel-abi-case-receipt-v1",
        "case_id": case["id"],
        "case_manifest_sha256": sha256_bytes(_canonical(case)),
        "source_identity": canonical_identity,
        "source_identity_sha256": sha256_bytes(_canonical(canonical_identity)),
        "closure_inputs": _closure_inputs(repo_root),
        "binary": case["binary"], "binary_sha256": _file_hash(repo_root, case["binary"]),
        "command": command, "command_sha256": sha256_bytes(_canonical(command)), "target": target,
        "target_sha256": sha256_bytes(target.encode()),
        "oracle_config_sha256": sha256_bytes(_canonical(case["oracle_configs"][target])),
        "exit_code": exit_code, "exit_sha256": sha256_bytes(str(exit_code).encode()),
        "case_result": results[0],
        "transcript_sha256": sha256_bytes(transcript.encode()),
    }


def verify_receipt(receipt: dict[str, Any], case: dict[str, Any], *, repo_root: Path,
                   command: list[str], target: str, exit_code: int, transcript: str,
                   ) -> None:
    if not isinstance(receipt, dict) or receipt.get("schema") != "thekernel-abi-case-receipt-v1":
        raise AbiCaseError("unsupported receipt schema")
    expected = build_receipt(case, repo_root=repo_root, command=command, target=target,
                             exit_code=exit_code, transcript=transcript)
    if receipt != expected:
        raise AbiCaseError("receipt does not bind the supplied execution inputs")


def main() -> int:
    try:
        cases = load_manifest()
    except AbiCaseError as error:
        print(f"abi-cases: {error}", file=sys.stderr)
        return 1
    print(f"abi-cases: valid cases={len(cases)} gate_eligible={sum(map(is_gate_eligible, cases))}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
