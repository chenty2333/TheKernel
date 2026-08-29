#!/usr/bin/env python3
"""Focused checks for q35 Linux oracle identity verification."""
from __future__ import annotations

import hashlib
import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("verify_linux_oracles", ROOT / "scripts/ci/verify_linux_oracles.py")
assert SPEC and SPEC.loader
oracles = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = oracles
SPEC.loader.exec_module(oracles)


class LinuxOracleTests(unittest.TestCase):
    def fixture(self) -> tuple[tempfile.TemporaryDirectory[str], Path, Path]:
        temporary = tempfile.TemporaryDirectory()
        root = Path(temporary.name)
        config_dir = root / "config/linux-6.12.103"
        state = root / ".state/linux-6.12.103/build/arch/x86/boot"
        config_dir.mkdir(parents=True)
        state.mkdir(parents=True)
        current = "".join(f"{key}=y\n" for key in oracles.CURRENT_ALREADY_ON)
        disabled = "".join(f"# {key} is not set\n" for key in oracles.REQUIRED_FEATURES)
        (config_dir / "q35-product-identity.config").write_text(current + disabled)
        seed = config_dir / "q35-product-defconfig"
        seed.write_text(current + disabled)
        (config_dir / "q35-feature-witness.config").write_text("".join(f"{key}=y\n" for key in oracles.REQUIRED_FEATURES))
        config = root / ".state/linux-6.12.103/build/.config"
        config.write_text(current + disabled)
        bzimage = state / "bzImage"
        bzimage.write_bytes(b"q35 bzImage")
        tarball = root / ".state/linux-6.12.103/linux-6.12.103.tar.xz"
        tarball.parent.mkdir(parents=True, exist_ok=True)
        tarball.write_bytes(b"linux source tarball")
        manifest = {"schema": "thekernel-linux-oracle-configs-v1", "linux": {"version": "v6.12.103", "commit": "25c09b42358e73e1476e517b296edb6344f2e4bd", "architecture": "x86_64", "source": {"tarball_path": ".state/linux-6.12.103/linux-6.12.103.tar.xz", "tarball_sha256": hashlib.sha256(tarball.read_bytes()).hexdigest()}, "build_identity": oracles.BUILD_IDENTITY}, "oracles": [
            {"id": "q35-product", "machine": "q35", "configuration": {"identity_assertions": "config/linux-6.12.103/q35-product-identity.config", "seed": "config/linux-6.12.103/q35-product-defconfig", "seed_sha256": hashlib.sha256(seed.read_bytes()).hexdigest(), "materialized_path": ".state/linux-6.12.103/build/.config", "final_config_sha256": hashlib.sha256(config.read_bytes()).hexdigest()}, "artifact": {"path": ".state/linux-6.12.103/build/arch/x86/boot/bzImage", "sha256": hashlib.sha256(bzimage.read_bytes()).hexdigest()}},
            {"id": "q35-feature-witness", "machine": "q35", "configuration": {"fragment": "config/linux-6.12.103/q35-feature-witness.config", "materialized_path": None, "final_config_sha256": None}, "feature_witness": {"explicitly_enabled": oracles.REQUIRED_FEATURES, "current_already_on": sorted(oracles.CURRENT_ALREADY_ON)}, "artifact": {"path": None, "sha256": None}},
        ]}
        path = root / "oracle-configs.json"
        path.write_text(json.dumps(manifest))
        return temporary, root, path

    def materialize_witness(self, root: Path, manifest_path: Path) -> tuple[Path, Path]:
        config = root / ".state/linux-6.12.103/feature-build/.config"
        config.parent.mkdir(parents=True)
        config.write_text(
            "".join(f"{key}=y\n" for key in oracles.CURRENT_ALREADY_ON)
            + "".join(f"{key}=y\n" for key in oracles.REQUIRED_FEATURES)
        )
        bzimage = root / ".state/linux-6.12.103/feature-build/arch/x86/boot/bzImage"
        bzimage.parent.mkdir(parents=True)
        bzimage.write_bytes(b"feature witness bzImage")
        manifest = json.loads(manifest_path.read_text())
        witness = manifest["oracles"][1]
        witness["configuration"]["materialized_path"] = ".state/linux-6.12.103/feature-build/.config"
        witness["configuration"]["final_config_sha256"] = hashlib.sha256(config.read_bytes()).hexdigest()
        witness["artifact"]["path"] = ".state/linux-6.12.103/feature-build/arch/x86/boot/bzImage"
        witness["artifact"]["sha256"] = hashlib.sha256(bzimage.read_bytes()).hexdigest()
        manifest_path.write_text(json.dumps(manifest))
        return config, bzimage

    def test_checked_in_product_and_witness_states_are_verified(self) -> None:
        report = oracles.verify(ROOT / "docs/linux-abi/oracle-configs.json", ROOT)
        manifest = json.loads((ROOT / "docs/linux-abi/oracle-configs.json").read_text())
        witness = manifest["oracles"][1]
        expected_witness = "unmaterialized" if witness["configuration"]["materialized_path"] is None else "ok"
        self.assertEqual(report, {"manifest": "ok", "source": "ok", "q35-product.config": "ok", "q35-product.artifact": "ok", "q35-feature-witness.config": expected_witness, "q35-feature-witness.artifact": expected_witness})

    def test_all_null_witness_passes_as_unmaterialized(self) -> None:
        temporary, root, manifest = self.fixture()
        self.addCleanup(temporary.cleanup)
        report = oracles.verify(manifest, root)
        self.assertEqual(report["source"], "ok")
        self.assertEqual(report["q35-product.config"], "ok")
        self.assertEqual(report["q35-feature-witness.config"], "unmaterialized")
        self.assertEqual(oracles.main(["--manifest", str(manifest), "--root", str(root), "--require-materialized"]), 2)

    def test_materialized_witness_checks_hashes_required_symbols_and_require_flag(self) -> None:
        temporary, root, manifest = self.fixture()
        self.addCleanup(temporary.cleanup)
        config, bzimage = self.materialize_witness(root, manifest)
        report = oracles.verify(manifest, root)
        self.assertEqual(report["q35-feature-witness.config"], "ok")
        self.assertEqual(report["q35-feature-witness.artifact"], "ok")
        self.assertEqual(oracles.main(["--manifest", str(manifest), "--root", str(root), "--require-materialized"]), 0)
        config.write_text(config.read_text().replace("CONFIG_MODULES=y", "# CONFIG_MODULES is not set"))
        with self.assertRaisesRegex(oracles.OracleError, "feature witness config SHA-256 mismatch"):
            oracles.verify(manifest, root)
        data = json.loads(manifest.read_text())
        data["oracles"][1]["configuration"]["final_config_sha256"] = hashlib.sha256(config.read_bytes()).hexdigest()
        manifest.write_text(json.dumps(data))
        with self.assertRaisesRegex(oracles.OracleError, "feature witness config assertion mismatch: CONFIG_MODULES"):
            oracles.verify(manifest, root)
        config.write_text(config.read_text().replace("# CONFIG_MODULES is not set", "CONFIG_MODULES=y"))
        data = json.loads(manifest.read_text())
        data["oracles"][1]["configuration"]["final_config_sha256"] = hashlib.sha256(config.read_bytes()).hexdigest()
        manifest.write_text(json.dumps(data))
        bzimage.write_bytes(b"drifted feature witness bzImage")
        with self.assertRaisesRegex(oracles.OracleError, "feature witness bzImage SHA-256 mismatch"):
            oracles.verify(manifest, root)

    def test_rejects_fragment_conflicts_and_partial_witness_identity(self) -> None:
        temporary, root, manifest_path = self.fixture()
        self.addCleanup(temporary.cleanup)
        fragment = root / "config/linux-6.12.103/q35-feature-witness.config"
        fragment.write_text(fragment.read_text() + "CONFIG_BPF_SYSCALL=n\n")
        with self.assertRaisesRegex(oracles.OracleError, "conflicting assignment"):
            oracles.verify(manifest_path, root)
        fragment.write_text("".join(f"{key}=y\n" for key in oracles.REQUIRED_FEATURES))
        manifest = json.loads(manifest_path.read_text())
        manifest["oracles"][1]["artifact"]["sha256"] = "0" * 64
        manifest_path.write_text(json.dumps(manifest))
        with self.assertRaisesRegex(oracles.OracleError, "materialization must make all"):
            oracles.verify(manifest_path, root)

    def test_rejects_product_hash_or_config_assertion_drift(self) -> None:
        temporary, root, manifest = self.fixture()
        self.addCleanup(temporary.cleanup)
        config = root / ".state/linux-6.12.103/build/.config"
        config.write_text(config.read_text().replace("CONFIG_KEXEC=y", "# CONFIG_KEXEC is not set"))
        with self.assertRaisesRegex(oracles.OracleError, "product config SHA-256 mismatch"):
            oracles.verify(manifest, root)


if __name__ == "__main__":
    unittest.main()
