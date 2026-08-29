#!/usr/bin/env python3
"""Verify materialized q35 Linux oracle identities without building Linux."""
from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parents[2]
DEFAULT_MANIFEST = ROOT / "docs/linux-abi/oracle-configs.json"
HEX_40 = re.compile(r"^[0-9a-f]{40}$")
HEX_64 = re.compile(r"^[0-9a-f]{64}$")
SET_CONFIG = re.compile(r"^(CONFIG_[A-Za-z0-9_]+)=(.+)$")
UNSET_CONFIG = re.compile(r"^# (CONFIG_[A-Za-z0-9_]+) is not set$")
REQUIRED_FEATURES = {
    "CONFIG_MODULES": "y", "CONFIG_MODULE_UNLOAD": "y", "CONFIG_KEXEC_FILE": "y",
    "CONFIG_BPF_SYSCALL": "y", "CONFIG_USER_NS": "y",
    "CONFIG_SECURITY_LANDLOCK": "y", "CONFIG_USERFAULTFD": "y",
}
CURRENT_ALREADY_ON = {
    "CONFIG_KEXEC", "CONFIG_PERF_EVENTS", "CONFIG_KEYS", "CONFIG_SWAP", "CONFIG_QUOTA",
    "CONFIG_PID_NS", "CONFIG_NET_NS", "CONFIG_UTS_NS", "CONFIG_IPC_NS", "CONFIG_TIME_NS",
    "CONFIG_IO_URING", "CONFIG_SECCOMP", "CONFIG_SECURITY",
    "CONFIG_X86_INTEL_MEMORY_PROTECTION_KEYS",
}
BUILD_IDENTITY = {
    "user": "thekernel", "host": "q35-linux-oracle",
    "timestamp": "2026-08-10T00:00:00Z", "version": 1,
}


class OracleError(ValueError):
    pass


def sha256_file(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def load_manifest(path: Path) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as exc:
        raise OracleError(f"cannot read manifest {path}: {exc}") from exc
    if not isinstance(value, dict):
        raise OracleError("manifest root must be an object")
    return value


def parse_config(path: Path) -> dict[str, str]:
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except (OSError, UnicodeError) as exc:
        raise OracleError(f"cannot read Kconfig input {path}: {exc}") from exc
    settings: dict[str, str] = {}
    for number, line in enumerate(lines, start=1):
        line = line.strip()
        if not line or (line.startswith("#") and not line.endswith("is not set")):
            continue
        match = SET_CONFIG.fullmatch(line)
        if match:
            key, value = match.groups()
        else:
            match = UNSET_CONFIG.fullmatch(line)
            if not match:
                raise OracleError(f"{path}:{number}: invalid Kconfig assignment")
            key, value = match.group(1), "n"
        if key in settings:
            raise OracleError(f"{path}:{number}: {'duplicate' if settings[key] == value else 'conflicting'} assignment for {key}")
        settings[key] = value
    return settings


def object_(value: object, field: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise OracleError(f"{field} must be an object")
    return value


def path_(root: Path, value: object, field: str) -> Path:
    if not isinstance(value, str) or not value:
        raise OracleError(f"{field} must be a non-empty relative path")
    relative = Path(value)
    if relative.is_absolute() or ".." in relative.parts:
        raise OracleError(f"{field} must stay within the repository")
    return root / relative


def hash_(value: object, field: str) -> str:
    if not isinstance(value, str) or not HEX_64.fullmatch(value):
        raise OracleError(f"{field} must be a lowercase SHA-256")
    return value


def feature_witness_materialized(witness_config: dict[str, Any], witness_artifact: dict[str, Any], root: Path) -> bool:
    """Validate and return the feature witness's all-or-nothing materialization state."""
    config_path = witness_config.get("materialized_path")
    config_hash = witness_config.get("final_config_sha256")
    artifact_path = witness_artifact.get("path")
    artifact_hash = witness_artifact.get("sha256")
    values = (config_path, config_hash, artifact_path, artifact_hash)
    if all(value is None for value in values):
        return False
    if any(value is None for value in values):
        raise OracleError("feature witness materialization must make all config and artifact paths and hashes non-null")
    path_(root, config_path, "q35-feature-witness.configuration.materialized_path")
    hash_(config_hash, "q35-feature-witness.configuration.final_config_sha256")
    path_(root, artifact_path, "q35-feature-witness.artifact.path")
    hash_(artifact_hash, "q35-feature-witness.artifact.sha256")
    return True


def validate_manifest(manifest: dict[str, Any], root: Path) -> dict[str, dict[str, Any]]:
    if manifest.get("schema") != "thekernel-linux-oracle-configs-v1":
        raise OracleError("unsupported oracle manifest schema")
    linux = object_(manifest.get("linux"), "linux")
    if linux.get("version") != "v6.12.103" or linux.get("architecture") != "x86_64":
        raise OracleError("oracle baseline must be Linux v6.12.103 x86_64")
    if not isinstance(linux.get("commit"), str) or not HEX_40.fullmatch(linux["commit"]):
        raise OracleError("linux.commit must be a lowercase 40-character SHA-1")
    source = object_(linux.get("source"), "linux.source")
    path_(root, source.get("tarball_path"), "linux.source.tarball_path")
    hash_(source.get("tarball_sha256"), "linux.source.tarball_sha256")
    if linux.get("build_identity") != BUILD_IDENTITY:
        raise OracleError("linux.build_identity must match the fixed reproducible identity")
    entries = manifest.get("oracles")
    if not isinstance(entries, list) or len(entries) != 2:
        raise OracleError("manifest must contain exactly two q35 oracles")
    by_id: dict[str, dict[str, Any]] = {}
    for oracle in entries:
        if not isinstance(oracle, dict) or not isinstance(oracle.get("id"), str) or oracle.get("machine") != "q35":
            raise OracleError("each oracle needs a unique string id and q35 machine")
        if oracle["id"] in by_id:
            raise OracleError(f"duplicate oracle id {oracle['id']}")
        by_id[oracle["id"]] = oracle
    if set(by_id) != {"q35-product", "q35-feature-witness"}:
        raise OracleError("oracle ids must be q35-product and q35-feature-witness")
    product = by_id["q35-product"]
    config = object_(product.get("configuration"), "q35-product.configuration")
    assertions = parse_config(path_(root, config.get("identity_assertions"), "q35-product.configuration.identity_assertions"))
    if not assertions:
        raise OracleError("q35-product identity assertions must not be empty")
    seed_path = path_(root, config.get("seed"), "q35-product.configuration.seed")
    seed_hash = hash_(config.get("seed_sha256"), "q35-product.configuration.seed_sha256")
    if sha256_file(seed_path) != seed_hash:
        raise OracleError("q35 product defconfig seed SHA-256 mismatch")
    path_(root, config.get("materialized_path"), "q35-product.configuration.materialized_path")
    hash_(config.get("final_config_sha256"), "q35-product.configuration.final_config_sha256")
    artifact = object_(product.get("artifact"), "q35-product.artifact")
    path_(root, artifact.get("path"), "q35-product.artifact.path")
    hash_(artifact.get("sha256"), "q35-product.artifact.sha256")
    witness = by_id["q35-feature-witness"]
    witness_config = object_(witness.get("configuration"), "q35-feature-witness.configuration")
    fragment = parse_config(path_(root, witness_config.get("fragment"), "q35-feature-witness.configuration.fragment"))
    witness_artifact = object_(witness.get("artifact"), "q35-feature-witness.artifact")
    feature_witness_materialized(witness_config, witness_artifact, root)
    feature = object_(witness.get("feature_witness"), "q35-feature-witness.feature_witness")
    if feature.get("explicitly_enabled") != REQUIRED_FEATURES:
        raise OracleError("feature witness must explicitly enable the required feature set")
    current = feature.get("current_already_on")
    if not isinstance(current, list) or set(current) != CURRENT_ALREADY_ON or len(current) != len(CURRENT_ALREADY_ON):
        raise OracleError("feature witness current-already-on set does not match the product identity")
    for key, value in REQUIRED_FEATURES.items():
        if fragment.get(key) != value:
            raise OracleError(f"feature witness fragment must set {key}={value}")
    return by_id


def verify_file(path: Path, expected: str, label: str) -> str:
    if not path.is_file():
        return "unavailable"
    actual = sha256_file(path)
    if actual != expected:
        raise OracleError(f"{label} SHA-256 mismatch: expected {expected}, got {actual}")
    return "ok"


def verify(manifest_path: Path, root: Path) -> dict[str, str]:
    manifest = load_manifest(manifest_path)
    by_id = validate_manifest(manifest, root)
    source = manifest["linux"]["source"]
    product = by_id["q35-product"]
    config = product["configuration"]
    config_path = path_(root, config["materialized_path"], "q35-product.configuration.materialized_path")
    witness = by_id["q35-feature-witness"]
    witness_config = witness["configuration"]
    witness_artifact = witness["artifact"]
    witness_materialized = feature_witness_materialized(witness_config, witness_artifact, root)
    report: dict[str, str] = {
        "manifest": "ok",
        "source": verify_file(path_(root, source["tarball_path"], "linux.source.tarball_path"), source["tarball_sha256"], "Linux tarball"),
        "q35-product.config": verify_file(config_path, config["final_config_sha256"], "product config"),
        "q35-product.artifact": verify_file(path_(root, product["artifact"]["path"], "q35-product.artifact.path"), product["artifact"]["sha256"], "bzImage"),
        "q35-feature-witness.config": "unmaterialized",
        "q35-feature-witness.artifact": "unmaterialized",
    }
    if report["q35-product.config"] == "ok":
        actual = parse_config(config_path)
        assertions = parse_config(path_(root, config["identity_assertions"], "q35-product.configuration.identity_assertions"))
        for key, value in assertions.items():
            actual_value = actual.get(key, "n")
            if actual_value != value:
                raise OracleError(f"product config assertion mismatch: {key} expected {value}, got {actual_value!r}")
        for key in CURRENT_ALREADY_ON:
            if actual.get(key) != "y":
                raise OracleError(f"product config current-already-on assertion mismatch: {key}")
    if witness_materialized:
        witness_config_path = path_(root, witness_config["materialized_path"], "q35-feature-witness.configuration.materialized_path")
        report["q35-feature-witness.config"] = verify_file(
            witness_config_path, witness_config["final_config_sha256"], "feature witness config"
        )
        report["q35-feature-witness.artifact"] = verify_file(
            path_(root, witness_artifact["path"], "q35-feature-witness.artifact.path"),
            witness_artifact["sha256"], "feature witness bzImage"
        )
        if report["q35-feature-witness.config"] == "ok":
            actual = parse_config(witness_config_path)
            for key, value in REQUIRED_FEATURES.items():
                if actual.get(key, "n") != value:
                    raise OracleError(f"feature witness config assertion mismatch: {key} expected {value}, got {actual.get(key, 'n')!r}")
            for key in CURRENT_ALREADY_ON:
                if actual.get(key) != "y":
                    raise OracleError(f"feature witness config current-already-on assertion mismatch: {key}")
    return report


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, default=DEFAULT_MANIFEST)
    parser.add_argument("--root", type=Path, default=ROOT)
    parser.add_argument("--require-materialized", action="store_true")
    args = parser.parse_args(argv)
    try:
        report = verify(args.manifest, args.root)
        for item, status in report.items():
            print(f"linux-oracle: {item}={status}")
        if args.require_materialized and any(status != "ok" for item, status in report.items() if item != "manifest"):
            print("linux-oracle: required material is not fully materialized", file=sys.stderr)
            return 2
    except OracleError as exc:
        print(f"linux-oracle: {exc}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
