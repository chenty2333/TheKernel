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

    def make_linux_vfs_fixture(
        self,
    ) -> tuple[tempfile.TemporaryDirectory[str], Fixture]:
        temporary = tempfile.TemporaryDirectory()
        fixture = Fixture(Path(temporary.name) / "consumer")
        sibling = (
            Path(temporary.name)
            / "thekernel-linux-abi/crates/vfs/Cargo.toml"
        )
        sibling.parent.mkdir(parents=True)
        sibling.write_text(
            '[package]\nname = "thekernel-linux-vfs"\nversion = "0.1.0"\n',
            encoding="utf-8",
        )
        (fixture.root / "Cargo.toml").write_text(
            '[workspace]\n'
            '[workspace.dependencies]\n'
            'linux-vfs = { package = "thekernel-linux-vfs", version = "=0.1.0" }\n'
            '[patch.crates-io]\n'
            'foo = { path = "vendor/foo" }\n'
            'thekernel-linux-vfs = { path = '
            '"../thekernel-linux-abi/crates/vfs" }\n',
            encoding="utf-8",
        )
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

    def test_maintained_sibling_and_local_adapter_are_classified_separately(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        fixture = Fixture(Path(temporary.name) / "consumer")

        sibling = (
            Path(temporary.name)
            / "thekernel-ax/crates/thekernel-axtask/Cargo.toml"
        )
        sibling.parent.mkdir(parents=True)
        sibling.write_text(
            '[package]\nname = "thekernel-axtask"\nversion = "0.1.0"\n',
            encoding="utf-8",
        )
        adapter = fixture.root / "crates/axtask-compat/Cargo.toml"
        adapter.parent.mkdir(parents=True)
        adapter.write_text(
            '[package]\nname = "axtask"\nversion = "0.3.0-preview.2"\n'
            'publish = false\n',
            encoding="utf-8",
        )
        with (fixture.root / "Cargo.toml").open("a", encoding="utf-8") as manifest:
            manifest.write(
                'thekernel-axtask = { path = '
                '"../thekernel-ax/crates/thekernel-axtask" }\n'
            )
            manifest.write('axtask = { path = "crates/axtask-compat" }\n')

        result = validator.validate_repository(fixture.root, archive_policy="skip")
        self.assertEqual(result.errors, ())
        self.assertEqual(result.package_checks, 1)
        self.assertEqual(result.maintained_checks, 1)
        self.assertEqual(result.adapter_checks, 1)

    def test_maintained_sibling_exemption_requires_the_exact_path(self) -> None:
        temporary, fixture = self.make_fixture()
        self.addCleanup(temporary.cleanup)
        with (fixture.root / "Cargo.toml").open("a", encoding="utf-8") as manifest:
            manifest.write(
                'thekernel-axtask = { path = "vendor/not-thekernel-axtask" }\n'
            )

        result = validator.validate_repository(fixture.root, archive_policy="skip")
        self.assertTrue(
            any("maintained sibling patch thekernel-axtask" in error for error in result.errors),
            result.errors,
        )
        self.assertTrue(
            any("missing provenance records: thekernel-axtask" in error for error in result.errors),
            result.errors,
        )

    def test_linux_vfs_workspace_dependency_and_patch_are_classified(self) -> None:
        temporary, fixture = self.make_linux_vfs_fixture()
        self.addCleanup(temporary.cleanup)

        result = validator.validate_repository(fixture.root, archive_policy="skip")
        self.assertEqual(result.errors, ())
        self.assertEqual(result.maintained_checks, 1)

    def test_linux_vfs_maintained_patch_rejects_wrong_path(self) -> None:
        temporary, fixture = self.make_linux_vfs_fixture()
        self.addCleanup(temporary.cleanup)
        manifest = (fixture.root / "Cargo.toml").read_text(encoding="utf-8")
        (fixture.root / "Cargo.toml").write_text(
            manifest.replace(
                "../thekernel-linux-abi/crates/vfs",
                "../thekernel-linux-abi/crates/not-vfs",
            ),
            encoding="utf-8",
        )

        result = validator.validate_repository(fixture.root, archive_policy="skip")
        self.assertTrue(
            any(
                "maintained sibling patch thekernel-linux-vfs" in error
                for error in result.errors
            ),
            result.errors,
        )

    def test_linux_vfs_workspace_dependency_requires_exact_version(self) -> None:
        temporary, fixture = self.make_linux_vfs_fixture()
        self.addCleanup(temporary.cleanup)
        manifest = (fixture.root / "Cargo.toml").read_text(encoding="utf-8")
        (fixture.root / "Cargo.toml").write_text(
            manifest.replace('version = "=0.1.0"', 'version = "0.1.0"'),
            encoding="utf-8",
        )

        result = validator.validate_repository(fixture.root, archive_policy="skip")
        self.assertTrue(
            any(
                "workspace dependency linux-vfs version" in error
                for error in result.errors
            ),
            result.errors,
        )

    def test_linux_vfs_maintained_patch_rejects_wrong_package_version(self) -> None:
        temporary, fixture = self.make_linux_vfs_fixture()
        self.addCleanup(temporary.cleanup)
        sibling_manifest = (
            Path(temporary.name)
            / "thekernel-linux-abi/crates/vfs/Cargo.toml"
        )
        sibling_manifest.write_text(
            '[package]\nname = "thekernel-linux-vfs"\nversion = "0.1.1"\n',
            encoding="utf-8",
        )

        result = validator.validate_repository(fixture.root, archive_policy="skip")
        self.assertTrue(
            any(
                "maintained sibling version is '0.1.1', expected '0.1.0'" in error
                for error in result.errors
            ),
            result.errors,
        )

    def test_linux_vfs_workspace_dependency_requires_exact_package(self) -> None:
        temporary, fixture = self.make_linux_vfs_fixture()
        self.addCleanup(temporary.cleanup)
        manifest = (fixture.root / "Cargo.toml").read_text(encoding="utf-8")
        (fixture.root / "Cargo.toml").write_text(
            manifest.replace(
                'package = "thekernel-linux-vfs"',
                'package = "lookalike-linux-vfs"',
            ),
            encoding="utf-8",
        )

        result = validator.validate_repository(fixture.root, archive_policy="skip")
        self.assertTrue(
            any(
                "workspace dependency linux-vfs package" in error
                for error in result.errors
            ),
            result.errors,
        )

    def test_non_vendor_patch_rejects_a_fabricated_provenance_record(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        fixture = Fixture(Path(temporary.name) / "consumer")

        sibling = (
            Path(temporary.name)
            / "thekernel-ax/crates/thekernel-axtask/Cargo.toml"
        )
        sibling.parent.mkdir(parents=True)
        sibling.write_text(
            '[package]\nname = "thekernel-axtask"\nversion = "0.1.0"\n',
            encoding="utf-8",
        )
        with (fixture.root / "Cargo.toml").open("a", encoding="utf-8") as manifest:
            manifest.write(
                'thekernel-axtask = { path = '
                '"../thekernel-ax/crates/thekernel-axtask" }\n'
            )

        registry_path = fixture.root / validator.REGISTRY_REL
        registry = registry_path.read_text(encoding="utf-8")
        duplicate_record = registry[registry.index("[[package]]") :].replace(
            'patch = "foo"', 'patch = "thekernel-axtask"', 1
        )
        registry_path.write_text(
            registry + "\n" + duplicate_record,
            encoding="utf-8",
        )

        result = validator.validate_repository(fixture.root, archive_policy="skip")
        self.assertTrue(
            any(
                "thekernel-axtask: non-vendor patch has an unexpected provenance record"
                in error
                for error in result.errors
            ),
            result.errors,
        )

    def test_inactive_vendored_record_remains_audited(self) -> None:
        temporary, fixture = self.make_fixture()
        self.addCleanup(temporary.cleanup)
        (fixture.root / "Cargo.toml").write_text(
            "[patch.crates-io]\n", encoding="utf-8"
        )

        result = validator.validate_repository(fixture.root, archive_policy="skip")
        self.assertEqual(result.errors, ())
        self.assertEqual(result.package_checks, 1)
        self.assertEqual(result.maintained_checks, 0)
        self.assertEqual(result.adapter_checks, 0)

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
