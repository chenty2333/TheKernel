#!/usr/bin/env python3
"""Focused tests for the vendored-source provenance gate."""

from __future__ import annotations

import hashlib
import importlib.util
import io
import json
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
VALIDATOR_PATH = REPO_ROOT / "scripts/ci/validate_vendor_provenance.py"
SPEC = importlib.util.spec_from_file_location("vendor_provenance", VALIDATOR_PATH)
assert SPEC is not None and SPEC.loader is not None
validator = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = validator
SPEC.loader.exec_module(validator)


def digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


class Fixture:
    def __init__(self, root: Path) -> None:
        self.root = root
        self.crate = root / "vendor/foo"
        self.archive_dir = root / "archives"
        self.crate.mkdir(parents=True)
        self.archive_dir.mkdir()

        self.manifest = (
            '[package]\n'
            'name = "foo"\n'
            'version = "1.2.3"\n'
            'license = "MIT"\n'
            'repository = "https://example.invalid/foo"\n'
        ).encode()
        self.original = self.manifest
        self.commit = "0123456789abcdef0123456789abcdef01234567"
        self.vcs = json.dumps(
            {"git": {"sha1": self.commit}, "path_in_vcs": ""},
            separators=(",", ":"),
        ).encode()

        archive = self.archive_dir / "foo-1.2.3.crate"
        with tarfile.open(archive, mode="w:gz") as tar:
            for relative, data in (
                ("Cargo.toml", self.manifest),
                ("Cargo.toml.orig", self.original),
                (".cargo_vcs_info.json", self.vcs),
            ):
                info = tarfile.TarInfo(f"foo-1.2.3/{relative}")
                info.size = len(data)
                info.mtime = 1
                tar.addfile(info, io.BytesIO(data))
        self.archive = archive
        self.archive_hash = digest(archive.read_bytes())

        (root / "Cargo.toml").write_text(
            '[patch.crates-io]\nfoo = { path = "vendor/foo" }\n',
            encoding="utf-8",
        )
        (self.crate / "Cargo.toml").write_bytes(self.manifest)
        (self.crate / "Cargo.toml.orig").write_bytes(self.original)
        (self.crate / ".cargo_vcs_info.json").write_bytes(self.vcs)
        (self.crate / "VENDOR.md").write_text(
            "\n".join(
                (
                    "# foo source record",
                    f"archive checksum {self.archive_hash}",
                    f"source commit {self.commit}",
                    "license MIT",
                    "upstream tests: none published",
                    "local patch ledger: fixture",
                )
            ),
            encoding="utf-8",
        )

        registry = root / "third_party/rust-patches/PROVENANCE.toml"
        registry.parent.mkdir(parents=True)
        registry.write_text(
            f"""schema = 1

[[package]]
patch = "foo"
path = "vendor/foo"
name = "foo"
version = "1.2.3"
source = "crates.io"
archive = "foo-1.2.3.crate"
archive_url = "https://static.crates.io/crates/foo/foo-1.2.3.crate"
archive_sha256 = "{self.archive_hash}"
repository = "https://example.invalid/foo"
upstream_tag = "not-recorded-in-published-archive"
source_commit = "{self.commit}"
source_commit_kind = "exact"
vcs_dirty = false
original_manifest = "Cargo.toml.orig"
original_manifest_sha256 = "{digest(self.original)}"
cargo_vcs_info = ".cargo_vcs_info.json"
cargo_vcs_info_sha256 = "{digest(self.vcs)}"
license_expression = "MIT"
license_status = "declared-only"
license_files = []
upstream_tests_status = "none-published"
upstream_test_files = []
vendor_record = "VENDOR.md"
patch_ledger = "VENDOR.md"
""",
            encoding="utf-8",
        )


class VendorProvenanceTests(unittest.TestCase):
    def make_fixture(self) -> tuple[tempfile.TemporaryDirectory[str], Fixture]:
        temporary = tempfile.TemporaryDirectory()
        fixture = Fixture(Path(temporary.name))
        return temporary, fixture

    def test_valid_archive_and_metadata_pass(self) -> None:
        temporary, fixture = self.make_fixture()
        self.addCleanup(temporary.cleanup)
        result = validator.validate_repository(
            fixture.root,
            archive_policy="require",
            archive_dirs=(fixture.archive_dir,),
        )
        self.assertEqual(result.errors, ())
        self.assertEqual(result.package_checks, 1)
        self.assertEqual(result.archive_checks, 1)

    def test_unrecorded_path_patch_fails(self) -> None:
        temporary, fixture = self.make_fixture()
        self.addCleanup(temporary.cleanup)
        with (fixture.root / "Cargo.toml").open("a", encoding="utf-8") as manifest:
            manifest.write('bar = { path = "vendor/bar" }\n')
        result = validator.validate_repository(fixture.root, archive_policy="skip")
        self.assertTrue(
            any("missing provenance records: bar" in error for error in result.errors),
            result.errors,
        )
        selected = validator.validate_repository(
            fixture.root,
            archive_policy="skip",
            selected=frozenset({"bar"}),
        )
        self.assertTrue(
            any("missing provenance records: bar" in error for error in selected.errors),
            selected.errors,
        )

    def test_modified_original_manifest_fails(self) -> None:
        temporary, fixture = self.make_fixture()
        self.addCleanup(temporary.cleanup)
        (fixture.crate / "Cargo.toml.orig").write_text("# modified\n", encoding="utf-8")
        result = validator.validate_repository(fixture.root, archive_policy="skip")
        self.assertTrue(
            any("original manifest differs" in error for error in result.errors),
            result.errors,
        )

    def test_wrong_archive_checksum_fails(self) -> None:
        temporary, fixture = self.make_fixture()
        self.addCleanup(temporary.cleanup)
        fixture.archive.write_bytes(fixture.archive.read_bytes() + b"tamper")
        result = validator.validate_repository(
            fixture.root,
            archive_policy="require",
            archive_dirs=(fixture.archive_dir,),
        )
        self.assertTrue(
            any("archive checksum mismatch" in error for error in result.errors),
            result.errors,
        )

    def test_repository_inventory_passes_without_archive_cache(self) -> None:
        result = validator.validate_repository(REPO_ROOT, archive_policy="skip")
        self.assertEqual(result.errors, ())
        self.assertEqual(result.package_checks, 26)


if __name__ == "__main__":
    unittest.main()
