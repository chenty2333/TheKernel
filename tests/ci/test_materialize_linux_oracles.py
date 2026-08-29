#!/usr/bin/env python3
"""Focused tests for the reproducible Linux oracle materializer."""
from __future__ import annotations

import hashlib
import importlib.util
import json
import os
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path
from unittest import mock


ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("materialize_linux_oracles", ROOT / "scripts/ci/materialize_linux_oracles.py")
assert SPEC and SPEC.loader
materializer = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = materializer
SPEC.loader.exec_module(materializer)


class MaterializeLinuxOraclesTests(unittest.TestCase):
    def fixture(self) -> tuple[tempfile.TemporaryDirectory[str], Path, Path]:
        temporary = tempfile.TemporaryDirectory()
        root = Path(temporary.name)
        config_dir = root / "config/linux-6.12.103"
        config_dir.mkdir(parents=True)
        product_config = "CONFIG_BASE=y\n# CONFIG_MODULES is not set\n"
        (config_dir / "q35-product-identity.config").write_text("CONFIG_BASE=y\n")
        seed = config_dir / "q35-product-defconfig"
        seed.write_text(product_config)
        (config_dir / "q35-feature-witness.config").write_text("CONFIG_MODULES=y\n")
        state = root / ".state/linux-6.12.103"
        product_dir = state / "build"
        witness_dir = state / "feature-witness/build-materialized"
        for directory, config, image in ((product_dir, product_config, b"old product"), (witness_dir, "CONFIG_BASE=y\nCONFIG_MODULES=y\n", b"old witness")):
            (directory / "arch/x86/boot").mkdir(parents=True)
            (directory / ".config").write_text(config)
            (directory / "arch/x86/boot/bzImage").write_bytes(image)
        source_tree = root / "linux-6.12.103"
        source_tree.mkdir()
        (source_tree / "Makefile").write_text("VERSION = 6\nPATCHLEVEL = 12\nSUBLEVEL = 103\n")
        tarball = state / "linux-6.12.103.tar.xz"
        with tarfile.open(tarball, "w:xz") as archive:
            archive.add(source_tree, arcname="linux-6.12.103")
        manifest = {
            "schema": "thekernel-linux-oracle-configs-v1",
            "linux": {"version": "v6.12.103", "architecture": "x86_64", "source": {"tarball_path": ".state/linux-6.12.103/linux-6.12.103.tar.xz", "tarball_sha256": hashlib.sha256(tarball.read_bytes()).hexdigest()}, "build_identity": materializer.BUILD_IDENTITY},
            "oracles": [
                {"id": "q35-product", "machine": "q35", "configuration": {"identity_assertions": "config/linux-6.12.103/q35-product-identity.config", "seed": "config/linux-6.12.103/q35-product-defconfig", "seed_sha256": hashlib.sha256(seed.read_bytes()).hexdigest(), "materialized_path": ".state/linux-6.12.103/build/.config", "final_config_sha256": hashlib.sha256((product_dir / ".config").read_bytes()).hexdigest()}, "artifact": {"path": ".state/linux-6.12.103/build/arch/x86/boot/bzImage", "sha256": hashlib.sha256((product_dir / "arch/x86/boot/bzImage").read_bytes()).hexdigest()}},
                {"id": "q35-feature-witness", "machine": "q35", "configuration": {"fragment": "config/linux-6.12.103/q35-feature-witness.config", "materialized_path": ".state/linux-6.12.103/feature-witness/build-materialized/.config", "final_config_sha256": hashlib.sha256((witness_dir / ".config").read_bytes()).hexdigest()}, "feature_witness": {"explicitly_enabled": {"CONFIG_MODULES": "y"}}, "artifact": {"path": ".state/linux-6.12.103/feature-witness/build-materialized/arch/x86/boot/bzImage", "sha256": hashlib.sha256((witness_dir / "arch/x86/boot/bzImage").read_bytes()).hexdigest()}},
            ],
        }
        manifest_path = root / "oracle-configs.json"
        manifest_path.write_text(json.dumps(manifest, indent=2) + "\n")
        return temporary, root, manifest_path

    def fake_make(self, command: list[str], check: bool) -> None:
        self.assertTrue(check)
        output = Path(next(value[2:] for value in command if value.startswith("O=")))
        target = command[-1]
        if target == "olddefconfig":
            with (output / ".config").open("a") as stream:
                stream.write("CONFIG_OLDDEFCONFIG_RAN=y\n")
        elif target == "bzImage":
            image = output / "arch/x86/boot/bzImage"
            image.parent.mkdir(parents=True, exist_ok=True)
            image.write_bytes((output / ".config").read_bytes())
        else:
            self.fail(f"unexpected make target: {target}")

    def test_default_refuses_generated_hash_drift_without_writing(self) -> None:
        temporary, root, manifest = self.fixture()
        self.addCleanup(temporary.cleanup)
        old_manifest = manifest.read_bytes()
        old_product = (root / ".state/linux-6.12.103/build/.config").read_bytes()
        with mock.patch.object(materializer.subprocess, "run", side_effect=self.fake_make):
            self.assertEqual(materializer.main(["--root", str(root), "--manifest", str(manifest)]), 2)
        self.assertEqual(manifest.read_bytes(), old_manifest)
        self.assertEqual((root / ".state/linux-6.12.103/build/.config").read_bytes(), old_product)

    def test_update_materializes_two_distinct_dirs_and_updates_all_hashes(self) -> None:
        temporary, root, manifest = self.fixture()
        self.addCleanup(temporary.cleanup)
        with mock.patch.object(materializer.subprocess, "run", side_effect=self.fake_make) as run:
            self.assertEqual(materializer.main(["--root", str(root), "--manifest", str(manifest), "--update-manifest"]), 0)
        self.assertEqual(run.call_count, 4)
        product = root / ".state/linux-6.12.103/build"
        witness = root / ".state/linux-6.12.103/feature-witness/build-materialized"
        self.assertNotEqual(product, witness)
        self.assertIn("CONFIG_MODULES=y", (witness / ".config").read_text())
        self.assertIn("# CONFIG_MODULES is not set", (product / ".config").read_text())
        recorded = json.loads(manifest.read_text())
        entries = {entry["id"]: entry for entry in recorded["oracles"]}
        self.assertEqual(entries["q35-product"]["configuration"]["final_config_sha256"], materializer.sha256(product / ".config"))
        self.assertEqual(entries["q35-product"]["artifact"]["sha256"], materializer.sha256(product / "arch/x86/boot/bzImage"))
        self.assertEqual(entries["q35-feature-witness"]["configuration"]["final_config_sha256"], materializer.sha256(witness / ".config"))
        self.assertEqual(entries["q35-feature-witness"]["artifact"]["sha256"], materializer.sha256(witness / "arch/x86/boot/bzImage"))

    def test_empty_materialized_state_builds_from_checked_in_seed(self) -> None:
        temporary, root, manifest = self.fixture()
        self.addCleanup(temporary.cleanup)
        for directory in (
            root / ".state/linux-6.12.103/build",
            root / ".state/linux-6.12.103/feature-witness/build-materialized",
        ):
            for path in sorted(directory.rglob("*"), reverse=True):
                if path.is_file():
                    path.unlink()
                elif path.is_dir():
                    path.rmdir()
            directory.rmdir()
        with mock.patch.object(materializer.subprocess, "run", side_effect=self.fake_make):
            self.assertEqual(materializer.main(["--root", str(root), "--manifest", str(manifest), "--update-manifest"]), 0)
        self.assertTrue((root / ".state/linux-6.12.103/build/.config").is_file())

    def test_modified_cached_source_is_replaced_from_the_verified_tarball(self) -> None:
        temporary, root, manifest = self.fixture()
        self.addCleanup(temporary.cleanup)
        with mock.patch.object(materializer.subprocess, "run", side_effect=self.fake_make):
            self.assertEqual(
                materializer.main([
                    "--root", str(root), "--manifest", str(manifest),
                    "--update-manifest",
                ]),
                0,
            )
            source = root / ".state/linux-6.12.103/src"
            (source / "poison").write_text("not from the pinned tarball")
            self.assertEqual(
                materializer.main(["--root", str(root), "--manifest", str(manifest)]),
                0,
            )
        self.assertFalse((source / "poison").exists())

    def test_safe_extract_allows_internal_symlink_and_rejects_escape(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name)
        safe = root / "safe.tar.xz"
        with tarfile.open(safe, "w:xz") as archive:
            directory = tarfile.TarInfo("linux-6.12.103")
            directory.type = tarfile.DIRTYPE
            archive.addfile(directory)
            target = tarfile.TarInfo("linux-6.12.103/target")
            target.size = 0
            archive.addfile(target)
            link = tarfile.TarInfo("linux-6.12.103/link")
            link.type = tarfile.SYMTYPE
            link.linkname = "target"
            archive.addfile(link)
        destination = root / "safe-output"
        destination.mkdir()
        materializer.safe_extract(safe, destination)
        self.assertEqual((destination / "linux-6.12.103/link").resolve(), (destination / "linux-6.12.103/target").resolve())

        unsafe = root / "unsafe.tar.xz"
        with tarfile.open(unsafe, "w:xz") as archive:
            link = tarfile.TarInfo("linux-6.12.103/link")
            link.type = tarfile.SYMTYPE
            link.linkname = "../../outside"
            archive.addfile(link)
        with self.assertRaisesRegex(materializer.MaterializeError, "unsafe tarball link"):
            materializer.safe_extract(unsafe, root / "unsafe-output")

    def test_publish_rolls_back_both_destinations_when_second_replace_fails(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name)
        staged_product = root / "staged-product"
        staged_witness = root / "staged-witness"
        product = root / "product"
        witness = root / "witness"
        for directory, marker in (
            (staged_product, "new-product"),
            (staged_witness, "new-witness"),
            (product, "old-product"),
            (witness, "old-witness"),
        ):
            directory.mkdir()
            (directory / "marker").write_text(marker)

        real_replace = materializer.os.replace

        def fail_second_publish(source: Path, destination: Path) -> None:
            if Path(source) == staged_witness and Path(destination) == witness:
                raise OSError("injected second publish failure")
            real_replace(source, destination)

        with mock.patch.object(materializer.os, "replace", side_effect=fail_second_publish):
            with self.assertRaisesRegex(materializer.MaterializeError, "could not publish"):
                materializer.publish(staged_product, staged_witness, product, witness)
        self.assertEqual((product / "marker").read_text(), "old-product")
        self.assertEqual((witness / "marker").read_text(), "old-witness")

    def test_publish_rolls_back_outputs_when_manifest_finalization_fails(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name)
        staged_product = root / "staged-product"
        staged_witness = root / "staged-witness"
        product = root / "product"
        witness = root / "witness"
        for directory, marker in (
            (staged_product, "new-product"),
            (staged_witness, "new-witness"),
            (product, "old-product"),
            (witness, "old-witness"),
        ):
            directory.mkdir()
            (directory / "marker").write_text(marker)

        def fail_manifest() -> None:
            raise OSError("injected manifest failure")

        with self.assertRaisesRegex(materializer.MaterializeError, "could not publish"):
            materializer.publish(
                staged_product, staged_witness, product, witness, fail_manifest
            )
        self.assertEqual((product / "marker").read_text(), "old-product")
        self.assertEqual((witness / "marker").read_text(), "old-witness")

    def test_rejects_preexisting_partial_state_tarball_drift_and_path_escape(self) -> None:
        temporary, root, manifest = self.fixture()
        self.addCleanup(temporary.cleanup)
        (root / ".state/linux-6.12.103/feature-witness/build-materialized/arch/x86/boot/bzImage").unlink()
        self.assertEqual(materializer.main(["--root", str(root), "--manifest", str(manifest)]), 2)
        temporary.cleanup()
        temporary, root, manifest = self.fixture()
        self.addCleanup(temporary.cleanup)
        data = json.loads(manifest.read_text())
        data["linux"]["source"]["tarball_sha256"] = "0" * 64
        manifest.write_text(json.dumps(data))
        self.assertEqual(materializer.main(["--root", str(root), "--manifest", str(manifest)]), 2)
        data["linux"]["source"]["tarball_sha256"] = hashlib.sha256((root / ".state/linux-6.12.103/linux-6.12.103.tar.xz").read_bytes()).hexdigest()
        data["oracles"][0]["configuration"]["materialized_path"] = "../outside/.config"
        manifest.write_text(json.dumps(data))
        self.assertEqual(materializer.main(["--root", str(root), "--manifest", str(manifest)]), 2)


if __name__ == "__main__":
    unittest.main()
