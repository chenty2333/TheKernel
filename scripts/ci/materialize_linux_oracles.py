#!/usr/bin/env python3
"""Reproducibly build the fixed Linux v6.12.103 x86_64 ABI oracles."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import posixpath
import shutil
import subprocess
import sys
import tarfile
import tempfile
import re
from pathlib import Path
from typing import Any, Callable


ROOT = Path(__file__).resolve().parents[2]
MANIFEST_NAME = "docs/linux-abi/oracle-configs.json"
VERSION = "v6.12.103"
ARCH = "x86_64"
SOURCE_TOPLEVEL = "linux-6.12.103"
BUILD_IDENTITY = {
    "user": "thekernel",
    "host": "q35-linux-oracle",
    "timestamp": "2026-08-10T00:00:00Z",
    "version": 1,
}
BUILD_JOBS = min(os.cpu_count() or 1, 16)
SET_CONFIG = re.compile(r"^(CONFIG_[A-Za-z0-9_]+)=(.+)$")
UNSET_CONFIG = re.compile(r"^# (CONFIG_[A-Za-z0-9_]+) is not set$")


class MaterializeError(RuntimeError):
    pass


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def repository_path(root: Path, value: object, field: str) -> Path:
    if not isinstance(value, str) or not value:
        raise MaterializeError(f"{field} must be a non-empty relative path")
    raw = Path(value)
    if raw.is_absolute() or ".." in raw.parts:
        raise MaterializeError(f"{field} escapes the repository")
    root = root.resolve()
    candidate = (root / raw).resolve(strict=False)
    try:
        candidate.relative_to(root)
    except ValueError as exc:
        raise MaterializeError(f"{field} escapes the repository through a symlink") from exc
    return candidate


def require_sha(value: object, field: str) -> str:
    if not isinstance(value, str) or len(value) != 64 or any(char not in "0123456789abcdef" for char in value):
        raise MaterializeError(f"{field} must be a lowercase SHA-256")
    return value


def obj(value: object, field: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise MaterializeError(f"{field} must be an object")
    return value


def load_manifest(root: Path, manifest_path: Path) -> tuple[dict[str, Any], dict[str, dict[str, Any]]]:
    try:
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as exc:
        raise MaterializeError(f"cannot read manifest: {exc}") from exc
    if not isinstance(manifest, dict) or manifest.get("schema") != "thekernel-linux-oracle-configs-v1":
        raise MaterializeError("unsupported oracle manifest schema")
    linux = obj(manifest.get("linux"), "linux")
    if linux.get("version") != VERSION or linux.get("architecture") != ARCH:
        raise MaterializeError("only Linux v6.12.103 x86_64 is supported")
    source = obj(linux.get("source"), "linux.source")
    repository_path(root, source.get("tarball_path"), "linux.source.tarball_path")
    require_sha(source.get("tarball_sha256"), "linux.source.tarball_sha256")
    if linux.get("build_identity") != BUILD_IDENTITY:
        raise MaterializeError("linux.build_identity must match the fixed reproducible identity")
    entries = manifest.get("oracles")
    if not isinstance(entries, list) or len(entries) != 2:
        raise MaterializeError("manifest must contain exactly two q35 oracles")
    by_id: dict[str, dict[str, Any]] = {}
    for entry in entries:
        if not isinstance(entry, dict) or entry.get("machine") != "q35" or not isinstance(entry.get("id"), str):
            raise MaterializeError("each oracle must have a string id and q35 machine")
        if entry["id"] in by_id:
            raise MaterializeError(f"duplicate oracle id {entry['id']}")
        by_id[entry["id"]] = entry
    if set(by_id) != {"q35-product", "q35-feature-witness"}:
        raise MaterializeError("manifest must contain q35-product and q35-feature-witness")
    for oracle_id in by_id:
        entry = by_id[oracle_id]
        config = obj(entry.get("configuration"), f"{oracle_id}.configuration")
        artifact = obj(entry.get("artifact"), f"{oracle_id}.artifact")
        repository_path(root, config.get("materialized_path"), f"{oracle_id}.configuration.materialized_path")
        require_sha(config.get("final_config_sha256"), f"{oracle_id}.configuration.final_config_sha256")
        repository_path(root, artifact.get("path"), f"{oracle_id}.artifact.path")
        require_sha(artifact.get("sha256"), f"{oracle_id}.artifact.sha256")
    product_config = obj(by_id["q35-product"]["configuration"], "q35-product.configuration")
    repository_path(root, product_config.get("identity_assertions"), "q35-product.configuration.identity_assertions")
    repository_path(root, product_config.get("seed"), "q35-product.configuration.seed")
    require_sha(product_config.get("seed_sha256"), "q35-product.configuration.seed_sha256")
    witness_config = obj(by_id["q35-feature-witness"]["configuration"], "q35-feature-witness.configuration")
    repository_path(root, witness_config.get("fragment"), "q35-feature-witness.configuration.fragment")
    return manifest, by_id


def require_recorded_state(root: Path, by_id: dict[str, dict[str, Any]]) -> None:
    """Allow an empty state, but reject partial or drifted prior material."""
    recorded: list[tuple[Path, str, str]] = []
    for oracle_id, entry in by_id.items():
        config = entry["configuration"]
        artifact = entry["artifact"]
        for value, expected, label in (
            (config["materialized_path"], config["final_config_sha256"], "config"),
            (artifact["path"], artifact["sha256"], "bzImage"),
        ):
            recorded.append((
                repository_path(root, value, f"{oracle_id}.{label}"), expected,
                f"{oracle_id} {label}",
            ))
    present = [path.is_file() for path, _, _ in recorded]
    if not any(present):
        return
    if not all(present):
        missing = next(label for (path, _, label), exists in zip(recorded, present) if not exists)
        raise MaterializeError(f"partial materialization: {missing} is missing")
    for path, expected, label in recorded:
        actual = sha256(path)
        if actual != expected:
            raise MaterializeError(f"hash drift: {label}: expected {expected}, got {actual}")


def parse_config(path: Path) -> dict[str, str]:
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except (OSError, UnicodeError) as exc:
        raise MaterializeError(f"cannot read Kconfig file {path}: {exc}") from exc
    result: dict[str, str] = {}
    for number, line in enumerate(lines, 1):
        line = line.strip()
        if not line or (line.startswith("#") and not line.endswith("is not set")):
            continue
        match = SET_CONFIG.fullmatch(line)
        if match:
            result[match.group(1)] = match.group(2)
            continue
        match = UNSET_CONFIG.fullmatch(line)
        if match:
            result[match.group(1)] = "n"
            continue
        raise MaterializeError(f"{path}:{number}: invalid Kconfig assignment")
    return result


def require_settings(actual: Path, expected: Path, label: str) -> None:
    settings, assertions = parse_config(actual), parse_config(expected)
    if not assertions:
        raise MaterializeError(f"{label} must not be empty")
    for key, value in assertions.items():
        if settings.get(key, "n") != value:
            raise MaterializeError(f"{label} mismatch: {key} expected {value}, got {settings.get(key, 'n')}")


def safe_extract(tarball: Path, destination: Path) -> None:
    with tarfile.open(tarball, "r:xz") as archive:
        members = archive.getmembers()
        if not members:
            raise MaterializeError("Linux tarball is empty")
        for member in members:
            name = Path(member.name)
            if name.is_absolute() or ".." in name.parts or not name.parts or name.parts[0] != SOURCE_TOPLEVEL:
                raise MaterializeError(f"unsafe tarball member: {member.name}")
            if member.isdev() or member.isfifo():
                raise MaterializeError(f"unsafe tarball member type: {member.name}")
            if member.issym() or member.islnk():
                if posixpath.isabs(member.linkname):
                    raise MaterializeError(f"unsafe tarball link: {member.name}")
                base = posixpath.dirname(member.name) if member.issym() else ""
                target = posixpath.normpath(posixpath.join(base, member.linkname))
                if target != SOURCE_TOPLEVEL and not target.startswith(SOURCE_TOPLEVEL + "/"):
                    raise MaterializeError(f"unsafe tarball link: {member.name}")
            elif not (member.isfile() or member.isdir()):
                raise MaterializeError(f"unsupported tarball member type: {member.name}")
        # The pinned development image currently carries Python 3.11, before
        # tarfile's filter= API.  The complete member/link validation above is
        # therefore the extraction boundary rather than a version-dependent
        # library filter.
        archive.extractall(destination, members=members)


def ensure_source(root: Path, source_config: dict[str, Any]) -> Path:
    tarball = repository_path(root, source_config["tarball_path"], "linux.source.tarball_path")
    expected = require_sha(source_config["tarball_sha256"], "linux.source.tarball_sha256")
    if not tarball.is_file():
        raise MaterializeError(f"Linux tarball is missing: {tarball}")
    if sha256(tarball) != expected:
        raise MaterializeError("Linux tarball SHA-256 mismatch")
    state = tarball.parent
    source = state / "src"
    # Never trust a writable cached source tree as an input to a pinned oracle.
    # Re-extract the already hash-verified tarball for every materialization;
    # Linux O= builds keep generated output outside this immutable input tree.
    with tempfile.TemporaryDirectory(prefix="linux-source-", dir=state) as temporary:
        temporary_path = Path(temporary)
        safe_extract(tarball, temporary_path)
        unpacked = temporary_path / SOURCE_TOPLEVEL
        if not (unpacked / "Makefile").is_file():
            raise MaterializeError("Linux tarball lacks its top-level Makefile")
        staged = state / f".src.new-{os.getpid()}"
        if staged.exists():
            raise MaterializeError(f"refusing to replace unexpected staging path {staged}")
        os.replace(unpacked, staged)
        backup = state / f".src.old-{os.getpid()}"
        try:
            if source.exists():
                os.replace(source, backup)
            os.replace(staged, source)
        except OSError:
            if backup.exists() and not source.exists():
                os.replace(backup, source)
            raise
        if backup.exists():
            shutil.rmtree(backup)
    return source


def run_make(source: Path, output: Path, target: str) -> None:
    subprocess.run([
        "make", f"-j{BUILD_JOBS}", "-C", str(source), f"O={output}", "ARCH=x86_64",
        f"KBUILD_BUILD_USER={BUILD_IDENTITY['user']}",
        f"KBUILD_BUILD_HOST={BUILD_IDENTITY['host']}",
        f"KBUILD_BUILD_TIMESTAMP={BUILD_IDENTITY['timestamp']}",
        f"KBUILD_BUILD_VERSION={BUILD_IDENTITY['version']}", target,
    ], check=True)


def append_fragment(config: Path, fragment: Path) -> None:
    try:
        content = fragment.read_text(encoding="utf-8")
    except (OSError, UnicodeError) as exc:
        raise MaterializeError(f"cannot read feature fragment: {exc}") from exc
    with config.open("a", encoding="utf-8") as stream:
        if content and not content.startswith("\n"):
            stream.write("\n")
        stream.write(content)


def build(source: Path, product_seed: Path, fragment: Path, staging_root: Path) -> tuple[Path, Path]:
    product = staging_root / "product"
    witness = staging_root / "witness"
    product.mkdir()
    shutil.copyfile(product_seed, product / ".config")
    run_make(source, product, "olddefconfig")
    run_make(source, product, "bzImage")
    # The witness is derived from this fresh product output, never by editing or
    # restoring the materialized product directory on disk.
    witness.mkdir()
    shutil.copyfile(product / ".config", witness / ".config")
    append_fragment(witness / ".config", fragment)
    run_make(source, witness, "olddefconfig")
    run_make(source, witness, "bzImage")
    return product, witness


def staged_hashes(product: Path, witness: Path) -> dict[str, str]:
    files = {
        "q35-product.config": product / ".config",
        "q35-product.artifact": product / "arch/x86/boot/bzImage",
        "q35-feature-witness.config": witness / ".config",
        "q35-feature-witness.artifact": witness / "arch/x86/boot/bzImage",
    }
    result: dict[str, str] = {}
    for label, path in files.items():
        if not path.is_file():
            raise MaterializeError(f"make did not produce {label}: {path}")
        result[label] = sha256(path)
    return result


def validate_generated_configs(product: Path, witness: Path, product_assertions: Path, fragment: Path, witness_entry: dict[str, Any]) -> None:
    require_settings(product / ".config", product_assertions, "product identity")
    require_settings(witness / ".config", fragment, "feature fragment")
    feature = obj(witness_entry.get("feature_witness"), "q35-feature-witness.feature_witness")
    enabled = obj(feature.get("explicitly_enabled"), "q35-feature-witness.feature_witness.explicitly_enabled")
    if not enabled or any(not isinstance(key, str) or value != "y" for key, value in enabled.items()):
        raise MaterializeError("feature witness explicitly_enabled must be non-empty CONFIG_*=y assertions")
    settings = parse_config(witness / ".config")
    for key in enabled:
        if not key.startswith("CONFIG_") or settings.get(key) != "y":
            raise MaterializeError(f"feature witness mismatch: {key} must be enabled")


def update_manifest_atomic(path: Path, manifest: dict[str, Any], hashes: dict[str, str]) -> None:
    entries = {entry["id"]: entry for entry in manifest["oracles"]}
    entries["q35-product"]["configuration"]["final_config_sha256"] = hashes["q35-product.config"]
    entries["q35-product"]["artifact"]["sha256"] = hashes["q35-product.artifact"]
    entries["q35-feature-witness"]["configuration"]["final_config_sha256"] = hashes["q35-feature-witness.config"]
    entries["q35-feature-witness"]["artifact"]["sha256"] = hashes["q35-feature-witness.artifact"]
    encoded = json.dumps(manifest, indent=2) + "\n"
    temporary = path.with_name(f".{path.name}.new-{os.getpid()}")
    try:
        temporary.write_text(encoded, encoding="utf-8")
        os.replace(temporary, path)
    finally:
        if temporary.exists():
            temporary.unlink()


def publish(
    staged_product: Path,
    staged_witness: Path,
    product_destination: Path,
    witness_destination: Path,
    finalize: Callable[[], None] | None = None,
) -> None:
    replacements = ((staged_product, product_destination), (staged_witness, witness_destination))
    backups: list[tuple[Path, Path]] = []
    published: list[Path] = []
    try:
        for staged, destination in replacements:
            destination.parent.mkdir(parents=True, exist_ok=True)
            backup = destination.with_name(f".{destination.name}.old-{os.getpid()}")
            if backup.exists():
                raise MaterializeError(f"refusing to replace unexpected backup path {backup}")
            if destination.exists():
                os.replace(destination, backup)
                backups.append((backup, destination))
            os.replace(staged, destination)
            published.append(destination)
        if finalize is not None:
            finalize()
    except OSError as exc:
        for destination in reversed(published):
            if destination.exists():
                shutil.rmtree(destination)
        for backup, destination in reversed(backups):
            os.replace(backup, destination)
        raise MaterializeError(f"could not publish materialized oracle directories: {exc}") from exc
    for backup, _ in backups:
        shutil.rmtree(backup)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT)
    parser.add_argument("--manifest", type=Path)
    parser.add_argument("--update-manifest", action="store_true", help="atomically record freshly built hashes")
    args = parser.parse_args(argv)
    root = args.root.resolve()
    manifest_path = (args.manifest if args.manifest is not None else root / MANIFEST_NAME).resolve()
    try:
        manifest, by_id = load_manifest(root, manifest_path)
        require_recorded_state(root, by_id)
        source = ensure_source(root, manifest["linux"]["source"])
        product_config = by_id["q35-product"]["configuration"]
        witness_config = by_id["q35-feature-witness"]["configuration"]
        product_destination = repository_path(root, product_config["materialized_path"], "q35-product.configuration.materialized_path").parent
        witness_destination = repository_path(root, witness_config["materialized_path"], "q35-feature-witness.configuration.materialized_path").parent
        with tempfile.TemporaryDirectory(prefix="oracle-build-", dir=product_destination.parent) as temporary:
            product_seed = repository_path(root, product_config["seed"], "q35-product.configuration.seed")
            if not product_seed.is_file() or sha256(product_seed) != product_config["seed_sha256"]:
                raise MaterializeError("q35 product defconfig seed SHA-256 mismatch")
            product, witness = build(source, product_seed, repository_path(root, witness_config["fragment"], "q35-feature-witness.configuration.fragment"), Path(temporary))
            validate_generated_configs(product, witness,
                                       repository_path(root, product_config["identity_assertions"], "q35-product.configuration.identity_assertions"),
                                       repository_path(root, witness_config["fragment"], "q35-feature-witness.configuration.fragment"),
                                       by_id["q35-feature-witness"])
            hashes = staged_hashes(product, witness)
            for label, digest in hashes.items():
                print(f"linux-oracle: {label}.sha256={digest}")
            expected = {
                "q35-product.config": product_config["final_config_sha256"], "q35-product.artifact": by_id["q35-product"]["artifact"]["sha256"],
                "q35-feature-witness.config": witness_config["final_config_sha256"], "q35-feature-witness.artifact": by_id["q35-feature-witness"]["artifact"]["sha256"],
            }
            drifted = [label for label in hashes if hashes[label] != expected[label]]
            if drifted and not args.update_manifest:
                raise MaterializeError("hash drift after build (rerun with --update-manifest to accept): " + ", ".join(drifted))
            finalize = None
            if args.update_manifest:
                finalize = lambda: update_manifest_atomic(manifest_path, manifest, hashes)
            publish(product, witness, product_destination, witness_destination, finalize)
    except (MaterializeError, subprocess.CalledProcessError, OSError) as exc:
        print(f"linux-oracle-materialize: {exc}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
