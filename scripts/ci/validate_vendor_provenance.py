#!/usr/bin/env python3
"""Validate vendored patches and classify maintained/local path overrides."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import sys
import tarfile
import tomllib
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Iterable, Mapping, Sequence

REGISTRY_REL = Path("third_party/rust-patches/PROVENANCE.toml")
HEX_40 = re.compile(r"^[0-9a-f]{40}$")
HEX_64 = re.compile(r"^[0-9a-f]{64}$")
LICENSE_STATUSES = {"archive-files", "declared-only", "recovered-after-release"}
TEST_STATUSES = {"none-published", "restored-exact", "restored-adapted"}
COMMIT_KINDS = {"exact", "context"}

# These path patches are maintained source inputs, not vendored crates.io
# forks. Exact path matching is intentional: a typo or a same-name substitute
# must fall back to the normal provenance requirement instead of inheriting an
# exemption by package name alone.
MAINTAINED_SIBLING_PATCHES = {
    "thekernel-axsched": ("../thekernel-ax/crates/thekernel-axsched", "0.1.0"),
    "thekernel-axpoll": ("../thekernel-ax/crates/thekernel-axpoll", "0.1.0"),
    "thekernel-axcbpf": ("../thekernel-ax/crates/thekernel-axcbpf", "0.1.0"),
    "thekernel-axfault": ("../thekernel-ax/crates/thekernel-axfault", "0.1.0"),
    "thekernel-axtask": ("../thekernel-ax/crates/thekernel-axtask", "0.1.0"),
    "thekernel-axtlb": ("../thekernel-ax/crates/thekernel-axtlb", "0.1.0"),
    "thekernel-linux-vfs": ("../thekernel-linux-abi/crates/vfs", "0.1.0"),
    "thekernel-linux-fd": ("../thekernel-linux-abi/crates/fd", "0.1.0"),
    "thekernel-linux-process": ("../thekernel-linux-abi/crates/process", "0.1.0"),
    "thekernel-linux-cred": ("../thekernel-linux-abi/crates/cred", "0.1.0"),
    "thekernel-linux-mm": ("../thekernel-linux-abi/crates/mm", "0.1.0"),
    "thekernel-linux-io-uring": ("../thekernel-linux-abi/crates/io-uring", "0.1.0"),
    "thekernel-linux-seccomp": ("../thekernel-linux-abi/crates/seccomp", "0.1.0"),
}
MAINTAINED_SIBLING_REPO_PATHS = {
    "thekernel-axsched": ("ax", Path("crates/thekernel-axsched")),
    "thekernel-axpoll": ("ax", Path("crates/thekernel-axpoll")),
    "thekernel-axcbpf": ("ax", Path("crates/thekernel-axcbpf")),
    "thekernel-axfault": ("ax", Path("crates/thekernel-axfault")),
    "thekernel-axtask": ("ax", Path("crates/thekernel-axtask")),
    "thekernel-axtlb": ("ax", Path("crates/thekernel-axtlb")),
    "thekernel-linux-vfs": ("linux-abi", Path("crates/vfs")),
    "thekernel-linux-fd": ("linux-abi", Path("crates/fd")),
    "thekernel-linux-process": ("linux-abi", Path("crates/process")),
    "thekernel-linux-cred": ("linux-abi", Path("crates/cred")),
    "thekernel-linux-mm": ("linux-abi", Path("crates/mm")),
    "thekernel-linux-io-uring": ("linux-abi", Path("crates/io-uring")),
    "thekernel-linux-seccomp": ("linux-abi", Path("crates/seccomp")),
}
MAINTAINED_WORKSPACE_DEPENDENCIES = {
    "axcbpf": ("thekernel-axcbpf", "=0.1.0"),
    "axfault": ("thekernel-axfault", "=0.1.0"),
    "axtlb": ("thekernel-axtlb", "=0.1.0"),
    "linux-vfs": ("thekernel-linux-vfs", "=0.1.0"),
    "thekernel-linux-cred": ("thekernel-linux-cred", "=0.1.0"),
    "thekernel-linux-mm": ("thekernel-linux-mm", "=0.1.0"),
    "thekernel-linux-io-uring": ("thekernel-linux-io-uring", "=0.1.0"),
    "thekernel-linux-seccomp": ("thekernel-linux-seccomp", "=0.1.0"),
}
MAINTAINED_SIBLING_LIB_NAMES = {
    "thekernel-axcbpf": "axcbpf",
    "thekernel-axfault": "axfault",
    "thekernel-axtlb": "axtlb",
}
LOCAL_ADAPTER_PATCHES = {
    "axtask": ("crates/axtask-compat", "0.3.0-preview.2"),
    "thekernel-readiness-adapter": ("crates/readiness-adapter", "0.1.0"),
}

# The active `axtask` patch is now a local facade, while the former crates.io
# source remains intentionally retained for historical baseline auditing.
# No other maintained sibling or adapter may acquire a provenance record merely
# to bypass its exact non-vendor classification.
RETAINED_NON_VENDOR_BASELINES = {
    "axtask": "third_party/rust-patches/axtask",
}


@dataclass(frozen=True)
class ValidationResult:
    errors: tuple[str, ...]
    archive_checks: int
    package_checks: int
    maintained_checks: int = 0
    adapter_checks: int = 0


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def load_toml(path: Path) -> dict:
    try:
        return tomllib.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, tomllib.TOMLDecodeError) as exc:
        raise ValueError(f"cannot read TOML {path}: {exc}") from exc


def safe_file(root: Path, crate_dir: Path, value: object, field: str) -> Path:
    if not isinstance(value, str) or not value:
        raise ValueError(f"{field} must be a non-empty relative path")
    relative = PurePosixPath(value)
    if relative.is_absolute() or ".." in relative.parts:
        raise ValueError(f"{field} escapes the package directory: {value!r}")
    candidate = (crate_dir / Path(*relative.parts)).resolve()
    try:
        candidate.relative_to(root.resolve())
    except ValueError as exc:
        raise ValueError(f"{field} escapes the repository: {value!r}") from exc
    return candidate


def patched_paths(root_manifest: Mapping[str, object]) -> dict[str, str]:
    patch_root = root_manifest.get("patch", {})
    if not isinstance(patch_root, Mapping):
        raise ValueError("root [patch] table is not a mapping")
    crates_io = patch_root.get("crates-io", {})
    if not isinstance(crates_io, Mapping):
        raise ValueError("root [patch.crates-io] table is not a mapping")

    result: dict[str, str] = {}
    for patch_name, spec in crates_io.items():
        if not isinstance(spec, Mapping) or "path" not in spec:
            continue
        path = spec["path"]
        if not isinstance(path, str) or not path:
            raise ValueError(f"patch {patch_name!r} has an invalid path")
        result[str(patch_name)] = PurePosixPath(path).as_posix()
    return result


def validate_maintained_workspace_dependencies(
    root_manifest: Mapping[str, object],
    patches: Mapping[str, str],
    errors: list[str],
) -> None:
    workspace = root_manifest.get("workspace", {})
    if not isinstance(workspace, Mapping):
        errors.append("root [workspace] table is not a mapping")
        return
    dependencies = workspace.get("dependencies", {})
    if not isinstance(dependencies, Mapping):
        errors.append("root [workspace.dependencies] table is not a mapping")
        return

    for alias, (expected_package, expected_version) in (
        MAINTAINED_WORKSPACE_DEPENDENCIES.items()
    ):
        spec = dependencies.get(alias)
        if spec is None:
            if expected_package in patches:
                errors.append(
                    f"maintained sibling patch {expected_package} requires "
                    f"workspace dependency {alias}"
                )
            continue
        if not isinstance(spec, Mapping):
            errors.append(
                f"workspace dependency {alias} must use an explicit dependency table"
            )
            continue
        declared_package = spec.get("package", alias)
        if declared_package != expected_package:
            errors.append(
                f"workspace dependency {alias} package is {declared_package!r}, "
                f"expected {expected_package!r}"
            )
        if spec.get("version") != expected_version:
            errors.append(
                f"workspace dependency {alias} version is {spec.get('version')!r}, "
                f"expected exact {expected_version!r}"
            )
        if "path" in spec:
            errors.append(
                f"workspace dependency {alias} must use the maintained sibling patch, "
                "not a direct path"
            )
        if expected_package not in patches:
            errors.append(
                f"workspace dependency {alias} requires maintained sibling patch "
                f"{expected_package}"
            )


def classify_patches(
    patches: Mapping[str, str], errors: list[str]
) -> tuple[dict[str, str], dict[str, str], dict[str, str]]:
    vendored: dict[str, str] = {}
    maintained: dict[str, str] = {}
    adapters: dict[str, str] = {}
    for name, path in patches.items():
        if name in MAINTAINED_SIBLING_PATCHES:
            expected, _ = MAINTAINED_SIBLING_PATCHES[name]
            if path == expected:
                maintained[name] = path
            else:
                errors.append(
                    f"maintained sibling patch {name} uses {path!r}, expected {expected!r}"
                )
                vendored[name] = path
            continue
        if name in LOCAL_ADAPTER_PATCHES:
            expected, _ = LOCAL_ADAPTER_PATCHES[name]
            if path == expected:
                adapters[name] = path
            else:
                errors.append(
                    f"local adapter patch {name} uses {path!r}, expected {expected!r}"
                )
                vendored[name] = path
            continue
        vendored[name] = path
    return vendored, maintained, adapters


def validate_non_vendor_patch(
    root: Path,
    name: str,
    path: str,
    *,
    expected_version: str,
    expected_lib_name: str | None = None,
    local_adapter: bool,
    crate_dir_override: Path | None = None,
    errors: list[str],
) -> None:
    kind = "local adapter" if local_adapter else "maintained sibling"
    label = f"{name} ({path})"
    crate_dir = (
        crate_dir_override.resolve()
        if crate_dir_override is not None
        else (root / path).resolve()
    )
    if not crate_dir.is_dir():
        errors.append(f"{label}: {kind} directory does not exist")
        return
    try:
        package = current_package(crate_dir)
    except ValueError as exc:
        errors.append(str(exc))
        return
    if package.get("name") != name:
        errors.append(
            f"{label}: {kind} package name is {package.get('name')!r}"
        )
    if package.get("version") != expected_version:
        errors.append(
            f"{label}: {kind} version is {package.get('version')!r}, "
            f"expected {expected_version!r}"
        )
    if expected_lib_name is not None:
        try:
            manifest = load_toml(crate_dir / "Cargo.toml")
        except ValueError as exc:
            errors.append(str(exc))
            return
        lib = manifest.get("lib")
        actual_lib_name = lib.get("name") if isinstance(lib, Mapping) else None
        if actual_lib_name != expected_lib_name:
            errors.append(
                f"{label}: {kind} lib name is {actual_lib_name!r}, "
                f"expected {expected_lib_name!r}"
            )
    if local_adapter:
        try:
            crate_dir.relative_to(root)
        except ValueError:
            errors.append(f"{label}: local adapter escapes the repository")
        if package.get("publish") is not False:
            errors.append(f"{label}: local adapter must set publish = false")
    else:
        try:
            crate_dir.relative_to(root)
        except ValueError:
            pass
        else:
            errors.append(f"{label}: maintained sibling resolves inside the repository")


def archive_search_dirs(root: Path, explicit: Sequence[Path]) -> list[Path]:
    dirs: list[Path] = []
    dirs.extend(path.expanduser() for path in explicit)

    env_dirs = os.environ.get("THEKERNEL_VENDOR_ARCHIVE_DIR", "")
    dirs.extend(Path(value).expanduser() for value in env_dirs.split(os.pathsep) if value)

    cargo_home = Path(os.environ.get("CARGO_HOME", Path.home() / ".cargo")).expanduser()
    cache_root = cargo_home / "registry" / "cache"
    if cache_root.is_dir():
        dirs.extend(path for path in cache_root.iterdir() if path.is_dir())

    dirs.append(root / ".state")
    unique: list[Path] = []
    seen: set[Path] = set()
    for path in dirs:
        resolved = path.resolve()
        if resolved not in seen:
            seen.add(resolved)
            unique.append(resolved)
    return unique


def find_archive(name: str, directories: Iterable[Path]) -> list[Path]:
    found: list[Path] = []
    for directory in directories:
        candidate = directory / name
        if candidate.is_file():
            found.append(candidate)
    return found


def canonical_license(data: bytes) -> bytes:
    return data.replace(b"\r\n", b"\n").rstrip(b"\n") + b"\n"


def archive_files(archive: Path) -> tuple[dict[str, bytes], str]:
    try:
        with tarfile.open(archive, mode="r:*") as tar:
            regular = [member for member in tar.getmembers() if member.isfile()]
            roots = {PurePosixPath(member.name).parts[0] for member in regular}
            if len(roots) != 1:
                raise ValueError(f"archive has {len(roots)} top-level roots")
            archive_root = roots.pop()
            files: dict[str, bytes] = {}
            prefix = archive_root + "/"
            for member in regular:
                if not member.name.startswith(prefix):
                    raise ValueError(f"member outside archive root: {member.name}")
                relative = member.name[len(prefix) :]
                source = tar.extractfile(member)
                if source is None:
                    raise ValueError(f"cannot read archive member: {member.name}")
                files[relative] = source.read()
            return files, archive_root
    except (OSError, tarfile.TarError, ValueError) as exc:
        raise ValueError(f"cannot inspect archive {archive}: {exc}") from exc


def current_package(crate_dir: Path) -> dict:
    manifest = load_toml(crate_dir / "Cargo.toml")
    package = manifest.get("package")
    if not isinstance(package, dict):
        raise ValueError(f"{crate_dir}/Cargo.toml has no [package] table")
    return package


def require_string(record: Mapping[str, object], key: str, errors: list[str], label: str) -> str:
    value = record.get(key)
    if not isinstance(value, str) or not value:
        errors.append(f"{label}: {key} must be a non-empty string")
        return ""
    return value


def validate_archive(
    record: Mapping[str, object],
    crate_dir: Path,
    archive: Path,
    errors: list[str],
    label: str,
) -> None:
    archive_name = require_string(record, "archive", errors, label)
    expected_archive_hash = require_string(record, "archive_sha256", errors, label)
    raw_archive = archive.read_bytes()
    if sha256(raw_archive) != expected_archive_hash:
        errors.append(
            f"{label}: archive checksum mismatch for {archive_name}: "
            f"got {sha256(raw_archive)}, expected {expected_archive_hash}"
        )
        return

    try:
        files, archive_root = archive_files(archive)
    except ValueError as exc:
        errors.append(f"{label}: {exc}")
        return

    expected_root = f"{record.get('name')}-{record.get('version')}"
    if archive_root != expected_root:
        errors.append(f"{label}: archive root {archive_root!r}, expected {expected_root!r}")

    for field, default_name, hash_field in (
        ("original_manifest", "Cargo.toml.orig", "original_manifest_sha256"),
        ("cargo_vcs_info", ".cargo_vcs_info.json", "cargo_vcs_info_sha256"),
    ):
        relative = record.get(field, default_name)
        if not isinstance(relative, str) or relative not in files:
            errors.append(f"{label}: archive is missing {relative!r}")
            continue
        expected = record.get(hash_field)
        actual = sha256(files[relative])
        if actual != expected:
            errors.append(f"{label}: {hash_field} mismatch: got {actual}, expected {expected}")

    try:
        published_manifest = tomllib.loads(files["Cargo.toml"].decode("utf-8"))
        published_package = published_manifest["package"]
    except (KeyError, UnicodeError, tomllib.TOMLDecodeError) as exc:
        errors.append(f"{label}: invalid published Cargo.toml: {exc}")
        published_package = {}

    for key in ("name", "version", "repository", "license"):
        record_key = "license_expression" if key == "license" else key
        expected = record.get(record_key)
        if key == "repository" and expected == "not-declared-in-published-manifest":
            expected = None
        if published_package.get(key) != expected:
            errors.append(
                f"{label}: published Cargo.toml {key}={published_package.get(key)!r}, "
                f"record says {expected!r}"
            )

    vcs_name = record.get("cargo_vcs_info", ".cargo_vcs_info.json")
    if isinstance(vcs_name, str) and vcs_name in files:
        try:
            vcs_info = json.loads(files[vcs_name])
            git = vcs_info["git"]
            dirty = bool(git.get("dirty", False))
            if git.get("sha1") != record.get("source_commit"):
                errors.append(f"{label}: archive VCS commit does not match the record")
            if dirty != record.get("vcs_dirty"):
                errors.append(f"{label}: archive VCS dirty flag does not match the record")
        except (KeyError, TypeError, ValueError) as exc:
            errors.append(f"{label}: invalid archive VCS record: {exc}")

    archive_licenses = sorted(
        path
        for path in files
        if "/" not in path and path.upper().startswith(("LICENSE", "COPYING"))
    )
    recorded_licenses = record.get("license_files", [])
    status = record.get("license_status")
    if status == "archive-files":
        if archive_licenses != recorded_licenses:
            errors.append(
                f"{label}: archive license inventory {archive_licenses!r}, "
                f"record says {recorded_licenses!r}"
            )
        for relative in archive_licenses:
            current = crate_dir / relative
            if current.is_file() and canonical_license(current.read_bytes()) != canonical_license(
                files[relative]
            ):
                errors.append(f"{label}: retained license differs from archive: {relative}")
    elif archive_licenses:
        errors.append(
            f"{label}: license_status={status!r} but archive contains {archive_licenses!r}"
        )

    archive_tests = sorted(path for path in files if path.startswith("tests/"))
    recorded_tests = record.get("upstream_test_files", [])
    if archive_tests != recorded_tests:
        errors.append(
            f"{label}: archive test inventory differs: {archive_tests!r} != {recorded_tests!r}"
        )
    if record.get("upstream_tests_status") == "restored-exact":
        for relative in archive_tests:
            current = crate_dir / relative
            if current.is_file() and current.read_bytes() != files[relative]:
                errors.append(f"{label}: exact restored test differs from archive: {relative}")


def validate_repository(
    root: Path,
    *,
    archive_policy: str = "if-present",
    archive_dirs: Sequence[Path] = (),
    selected: frozenset[str] = frozenset(),
    ax_repo: Path | None = None,
    linux_abi_repo: Path | None = None,
) -> ValidationResult:
    errors: list[str] = []
    root = root.resolve()
    try:
        root_manifest = load_toml(root / "Cargo.toml")
        patches = patched_paths(root_manifest)
        registry = load_toml(root / REGISTRY_REL)
    except ValueError as exc:
        return ValidationResult((str(exc),), 0, 0)

    validate_maintained_workspace_dependencies(root_manifest, patches, errors)

    if registry.get("schema") != 1:
        errors.append(f"{REGISTRY_REL}: schema must be 1")

    raw_records = registry.get("package")
    if not isinstance(raw_records, list):
        return ValidationResult(
            tuple(errors + [f"{REGISTRY_REL}: [[package]] records are missing"]), 0, 0
        )

    records: dict[str, Mapping[str, object]] = {}
    for index, record in enumerate(raw_records):
        if not isinstance(record, Mapping):
            errors.append(f"{REGISTRY_REL}: package record {index} is not a table")
            continue
        patch = record.get("patch")
        if not isinstance(patch, str) or not patch:
            errors.append(f"{REGISTRY_REL}: package record {index} has no patch name")
            continue
        if patch in records:
            errors.append(f"{REGISTRY_REL}: duplicate record for patch {patch}")
            continue
        records[patch] = record

    if selected:
        unknown = selected - (patches.keys() | records.keys())
        if unknown:
            errors.append(f"unknown selected patch names: {', '.join(sorted(unknown))}")
        patches = {key: value for key, value in patches.items() if key in selected}
        records = {key: value for key, value in records.items() if key in selected}

    vendored_patches, maintained_patches, adapter_patches = classify_patches(
        patches, errors
    )
    non_vendor_records = (
        maintained_patches.keys() | adapter_patches.keys()
    ) & records.keys()
    for patch_name in sorted(non_vendor_records):
        recorded_path = records[patch_name].get("path")
        allowed_path = RETAINED_NON_VENDOR_BASELINES.get(patch_name)
        if recorded_path != allowed_path:
            errors.append(
                f"{patch_name}: non-vendor patch has an unexpected provenance record"
            )
    missing = vendored_patches.keys() - records.keys()
    if missing:
        errors.append(f"missing provenance records: {', '.join(sorted(missing))}")

    search_dirs = archive_search_dirs(root, archive_dirs)
    archive_checks = 0
    package_checks = 0
    maintained_checks = 0
    adapter_checks = 0

    for patch_name, patch_path in sorted(maintained_patches.items()):
        _, expected_version = MAINTAINED_SIBLING_PATCHES[patch_name]
        repo_kind, repo_relative = MAINTAINED_SIBLING_REPO_PATHS[patch_name]
        sibling_repo = ax_repo if repo_kind == "ax" else linux_abi_repo
        crate_dir_override = (
            sibling_repo.resolve() / repo_relative
            if sibling_repo is not None
            else None
        )
        validate_non_vendor_patch(
            root,
            patch_name,
            patch_path,
            expected_version=expected_version,
            expected_lib_name=MAINTAINED_SIBLING_LIB_NAMES.get(patch_name),
            local_adapter=False,
            crate_dir_override=crate_dir_override,
            errors=errors,
        )
        maintained_checks += 1

    for patch_name, patch_path in sorted(adapter_patches.items()):
        _, expected_version = LOCAL_ADAPTER_PATCHES[patch_name]
        validate_non_vendor_patch(
            root,
            patch_name,
            patch_path,
            expected_version=expected_version,
            local_adapter=True,
            errors=errors,
        )
        adapter_checks += 1

    for patch_name, record in sorted(records.items()):
        patch_path = record.get("path")
        if not isinstance(patch_path, str) or not patch_path:
            errors.append(f"{patch_name}: recorded path must be a non-empty string")
            continue
        label = f"{patch_name} ({patch_path})"
        package_checks += 1

        active_path = vendored_patches.get(patch_name)
        if active_path is not None and active_path != patch_path:
            errors.append(f"{label}: active patch path is {active_path!r}")
        if record.get("source") != "crates.io":
            errors.append(f"{label}: source must be 'crates.io'")

        crate_dir = (root / patch_path).resolve()
        try:
            crate_dir.relative_to(root)
        except ValueError:
            errors.append(f"{label}: patch path escapes the repository")
            continue
        if not crate_dir.is_dir():
            errors.append(f"{label}: patch directory does not exist")
            continue

        try:
            package = current_package(crate_dir)
        except ValueError as exc:
            errors.append(str(exc))
            continue
        for key in ("name", "version"):
            if package.get(key) != record.get(key):
                errors.append(
                    f"{label}: current package {key}={package.get(key)!r}, "
                    f"record says {record.get(key)!r}"
                )
        if record.get("name") != patch_name:
            errors.append(f"{label}: record name must match the patched package name")
        if package.get("license") != record.get("license_expression"):
            errors.append(f"{label}: current license expression differs from the record")

        name = require_string(record, "name", errors, label)
        version = require_string(record, "version", errors, label)
        archive_name = require_string(record, "archive", errors, label)
        expected_archive = f"{name}-{version}.crate"
        if archive_name != expected_archive:
            errors.append(f"{label}: archive must be {expected_archive!r}")
        expected_url = f"https://static.crates.io/crates/{name}/{expected_archive}"
        if record.get("archive_url") != expected_url:
            errors.append(f"{label}: archive_url must be {expected_url!r}")
        if not HEX_64.fullmatch(str(record.get("archive_sha256", ""))):
            errors.append(f"{label}: archive_sha256 is not lowercase SHA-256")
        if not HEX_40.fullmatch(str(record.get("source_commit", ""))):
            errors.append(f"{label}: source_commit is not a 40-character Git object ID")
        if record.get("source_commit_kind") not in COMMIT_KINDS:
            errors.append(f"{label}: invalid source_commit_kind")
        dirty = record.get("vcs_dirty")
        if not isinstance(dirty, bool):
            errors.append(f"{label}: vcs_dirty must be a boolean")
        elif dirty != (record.get("source_commit_kind") == "context"):
            errors.append(f"{label}: dirty releases must use context commit identity")
        if not isinstance(record.get("upstream_tag"), str) or not record.get("upstream_tag"):
            errors.append(f"{label}: upstream_tag status is missing")

        for field, hash_field in (
            ("original_manifest", "original_manifest_sha256"),
            ("cargo_vcs_info", "cargo_vcs_info_sha256"),
        ):
            try:
                asset = safe_file(root, crate_dir, record.get(field), field)
            except ValueError as exc:
                errors.append(f"{label}: {exc}")
                continue
            if not asset.is_file():
                errors.append(f"{label}: missing {field}: {asset.relative_to(root)}")
                continue
            expected_hash = str(record.get(hash_field, ""))
            if not HEX_64.fullmatch(expected_hash):
                errors.append(f"{label}: {hash_field} is not lowercase SHA-256")
            if field == "original_manifest" and sha256(asset.read_bytes()) != expected_hash:
                errors.append(f"{label}: original manifest differs from the published baseline")

        vcs_path = crate_dir / str(record.get("cargo_vcs_info", ".cargo_vcs_info.json"))
        if vcs_path.is_file():
            try:
                vcs_info = json.loads(vcs_path.read_text(encoding="utf-8"))
                git = vcs_info["git"]
                if git.get("sha1") != record.get("source_commit"):
                    errors.append(f"{label}: retained VCS commit differs from the record")
                if bool(git.get("dirty", False)) != record.get("vcs_dirty"):
                    errors.append(f"{label}: retained VCS dirty flag differs from the record")
            except (OSError, UnicodeError, ValueError, KeyError, TypeError) as exc:
                errors.append(f"{label}: invalid retained VCS record: {exc}")

        license_status = record.get("license_status")
        license_files = record.get("license_files")
        if license_status not in LICENSE_STATUSES:
            errors.append(f"{label}: invalid license_status")
        if not isinstance(license_files, list) or not all(
            isinstance(value, str) for value in license_files
        ):
            errors.append(f"{label}: license_files must be a string array")
            license_files = []
        if license_files != sorted(set(license_files)):
            errors.append(f"{label}: license_files must be sorted and unique")
        if license_status == "declared-only" and license_files:
            errors.append(f"{label}: declared-only license status cannot list files")
        for relative in license_files:
            try:
                license_path = safe_file(root, crate_dir, relative, "license_files")
            except ValueError as exc:
                errors.append(f"{label}: {exc}")
                continue
            if not license_path.is_file():
                errors.append(f"{label}: missing retained license file: {relative}")

        tests_status = record.get("upstream_tests_status")
        test_files = record.get("upstream_test_files")
        if tests_status not in TEST_STATUSES:
            errors.append(f"{label}: invalid upstream_tests_status")
        if not isinstance(test_files, list) or not all(isinstance(value, str) for value in test_files):
            errors.append(f"{label}: upstream_test_files must be a string array")
            test_files = []
        if tests_status == "none-published" and test_files:
            errors.append(f"{label}: none-published test status cannot list files")
        if tests_status != "none-published" and not test_files:
            errors.append(f"{label}: restored test status requires an inventory")
        if test_files != sorted(set(test_files)):
            errors.append(f"{label}: upstream_test_files must be sorted and unique")
        for relative in test_files:
            try:
                test_path = safe_file(root, crate_dir, relative, "upstream_test_files")
            except ValueError as exc:
                errors.append(f"{label}: {exc}")
                continue
            if not test_path.is_file():
                errors.append(f"{label}: missing upstream test asset: {relative}")

        vendor_name = record.get("vendor_record")
        ledger_name = record.get("patch_ledger")
        if vendor_name != ledger_name:
            errors.append(f"{label}: vendor_record and patch_ledger must identify one record")
        try:
            vendor_path = safe_file(root, crate_dir, vendor_name, "vendor_record")
        except ValueError as exc:
            errors.append(f"{label}: {exc}")
            vendor_path = None
        if vendor_path is not None:
            if not vendor_path.is_file():
                errors.append(f"{label}: missing VENDOR.md")
            else:
                text = vendor_path.read_text(encoding="utf-8")
                for needle, description in (
                    (str(record.get("archive_sha256")), "archive checksum"),
                    (str(record.get("source_commit")), "source commit"),
                    (str(record.get("license_expression")), "license expression"),
                ):
                    if needle not in text:
                        errors.append(f"{label}: VENDOR.md omits {description}")
                lowered = text.lower()
                if "test" not in lowered:
                    errors.append(f"{label}: VENDOR.md omits upstream test status")
                if "patch" not in lowered:
                    errors.append(f"{label}: VENDOR.md omits the local patch ledger")

        candidates = find_archive(archive_name, search_dirs)
        if not candidates:
            if archive_policy == "require":
                errors.append(
                    f"{label}: archive {archive_name} was not found in "
                    + ", ".join(str(path) for path in search_dirs)
                )
        elif archive_policy != "skip":
            matching = [
                archive
                for archive in candidates
                if sha256(archive.read_bytes()) == record.get("archive_sha256")
            ]
            archive_to_check = matching[0] if matching else candidates[0]
            validate_archive(record, crate_dir, archive_to_check, errors, label)
            archive_checks += 1

    return ValidationResult(
        tuple(errors),
        archive_checks,
        package_checks,
        maintained_checks,
        adapter_checks,
    )


def parse_args(argv: Sequence[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--repo-root",
        type=Path,
        default=Path(__file__).resolve().parents[2],
        help="TheKernel repository root",
    )
    parser.add_argument(
        "--archive-policy",
        choices=("skip", "if-present", "require"),
        default="if-present",
        help="whether registry .crate archives must be available",
    )
    parser.add_argument(
        "--archive-dir",
        action="append",
        default=[],
        type=Path,
        help="additional directory containing name-version.crate archives",
    )
    parser.add_argument(
        "--package",
        action="append",
        default=[],
        help="validate only this patched package (repeatable)",
    )
    parser.add_argument(
        "--ax-repo",
        type=Path,
        help=(
            "maintained thekernel-ax workspace to inspect while retaining "
            "canonical manifest patch paths"
        ),
    )
    parser.add_argument(
        "--linux-abi-repo",
        type=Path,
        help=(
            "maintained thekernel-linux-abi workspace to inspect while "
            "retaining canonical manifest patch paths"
        ),
    )
    parser.add_argument("--quiet", action="store_true")
    return parser.parse_args(argv)


def main(argv: Sequence[str] = ()) -> int:
    args = parse_args(argv or sys.argv[1:])
    result = validate_repository(
        args.repo_root,
        archive_policy=args.archive_policy,
        archive_dirs=args.archive_dir,
        selected=frozenset(args.package),
        ax_repo=args.ax_repo,
        linux_abi_repo=args.linux_abi_repo,
    )
    if result.errors:
        for error in result.errors:
            print(f"vendor-provenance: ERROR: {error}", file=sys.stderr)
        print(
            f"vendor-provenance: FAIL ({len(result.errors)} errors, "
            f"{result.package_checks} vendored packages, "
            f"{result.maintained_checks} maintained siblings, "
            f"{result.adapter_checks} local adapters, "
            f"{result.archive_checks} archives)",
            file=sys.stderr,
        )
        return 1
    if not args.quiet:
        print(
            f"vendor-provenance: PASS ({result.package_checks} vendored packages, "
            f"{result.maintained_checks} maintained siblings, "
            f"{result.adapter_checks} local adapters, "
            f"{result.archive_checks} archives)"
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
