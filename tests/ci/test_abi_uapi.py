#!/usr/bin/env python3
"""Focused provenance checks for the Linux UAPI header gate."""
from __future__ import annotations

import hashlib
import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("abi_uapi", ROOT / "tools/abi_uapi.py")
assert SPEC and SPEC.loader
abi_uapi = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = abi_uapi
SPEC.loader.exec_module(abi_uapi)


class AbiUapiTests(unittest.TestCase):
    def manifest(self, root: Path) -> Path:
        tarball = root / ".state/linux-6.12.103/linux-6.12.103.tar.xz"
        tarball.parent.mkdir(parents=True)
        tarball.write_bytes(b"pinned source")
        data = {
            "schema": "thekernel-linux-uapi-headers-v1",
            "linux": {
                "version": abi_uapi.VERSION, "ref": abi_uapi.REF,
                "commit": abi_uapi.COMMIT, "architecture": abi_uapi.ARCH,
                "source": {"tarball_path": ".state/linux-6.12.103/linux-6.12.103.tar.xz", "tarball_sha256": hashlib.sha256(tarball.read_bytes()).hexdigest()},
            },
            "headers": {"materialized_path": abi_uapi.MATERIALIZED_PATH, "tree_sha256": abi_uapi.TREE_SHA256},
        }
        path = root / "uapi-headers.json"
        path.write_text(json.dumps(data))
        return path

    def test_tree_hash_is_sorted_and_content_sensitive(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            tree = Path(temporary) / "headers"
            (tree / "z").mkdir(parents=True)
            (tree / "z/b.h").write_bytes(b"b")
            (tree / "a.h").write_bytes(b"a")
            first = abi_uapi.tree_sha256(tree)
            (tree / "z/b.h").write_bytes(b"changed")
            self.assertNotEqual(first, abi_uapi.tree_sha256(tree))

    def test_manifest_binds_exact_linux_ref_and_commit(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            path = self.manifest(root)
            data = json.loads(path.read_text())
            data["linux"]["commit"] = "0" * 40
            path.write_text(json.dumps(data))
            with self.assertRaisesRegex(abi_uapi.UapiError, "ref and commit"):
                abi_uapi.load_manifest(path, root)

    def test_verify_rejects_materialized_tree_drift(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            path = self.manifest(root)
            tree = root / abi_uapi.MATERIALIZED_PATH
            tree.mkdir(parents=True)
            (tree / "unistd.h").write_text("drift")
            with self.assertRaisesRegex(abi_uapi.UapiError, "tree SHA-256 mismatch"):
                abi_uapi.verify(path, root)


if __name__ == "__main__":
    unittest.main()
