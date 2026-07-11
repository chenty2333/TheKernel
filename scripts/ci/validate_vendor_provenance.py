#!/usr/bin/env python3
"""Validate provenance for every local [patch.crates-io] package."""

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


@dataclass(frozen=True)
class ValidationResult:
    errors: tuple[str, ...]
    archive_checks: int
    package_checks: int


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
) -> ValidationResult:
    errors: list[str] = []
    root = root.resolve()
    try:
        patches = patched_paths(load_toml(root / "Cargo.toml"))
        registry = load_toml(root / REGISTRY_REL)
    except ValueError as exc:
        return ValidationResult((str(exc),), 0, 0)

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
        unknown = selected - patches.keys()
        if unknown:
            errors.append(f"unknown selected patch names: {', '.join(sorted(unknown))}")
        missing_selected = (selected & patches.keys()) - records.keys()
        if missing_selected:
            errors.append(
                "missing provenance records: " + ", ".join(sorted(missing_selected))
            )
        patches = {key: value for key, value in patches.items() if key in selected}
        records = {key: value for key, value in records.items() if key in selected}
    else:
        missing = patches.keys() - records.keys()
        extra = records.keys() - patches.keys()
        if missing:
            errors.append(f"missing provenance records: {', '.join(sorted(missing))}")
        if extra:
            errors.append(f"records without local patches: {', '.join(sorted(extra))}")

    search_dirs = archive_search_dirs(root, archive_dirs)
    archive_checks = 0
    package_checks = 0

    for patch_name, patch_path in sorted(patches.items()):
        label = f"{patch_name} ({patch_path})"
        record = records.get(patch_name)
        if record is None:
            continue
        package_checks += 1

        if record.get("path") != patch_path:
            errors.append(f"{label}: recorded path is {record.get('path')!r}")
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

    return ValidationResult(tuple(errors), archive_checks, package_checks)


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
    parser.add_argument("--quiet", action="store_true")
    return parser.parse_args(argv)


def main(argv: Sequence[str] = ()) -> int:
    args = parse_args(argv or sys.argv[1:])
    result = validate_repository(
        args.repo_root,
        archive_policy=args.archive_policy,
        archive_dirs=args.archive_dir,
        selected=frozenset(args.package),
    )
    if result.errors:
        for error in result.errors:
            print(f"vendor-provenance: ERROR: {error}", file=sys.stderr)
        print(
            f"vendor-provenance: FAIL ({len(result.errors)} errors, "
            f"{result.package_checks} packages, {result.archive_checks} archives)",
            file=sys.stderr,
        )
        return 1
    if not args.quiet:
        print(
            f"vendor-provenance: PASS ({result.package_checks} packages, "
            f"{result.archive_checks} archives)"
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
