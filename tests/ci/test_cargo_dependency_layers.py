#!/usr/bin/env python3
"""Focused policy tests for the Cargo dependency-layer CI guard."""

from __future__ import annotations

import importlib.util
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

REPO_ROOT = Path(__file__).resolve().parents[2]
SCRIPT_PATH = REPO_ROOT / "scripts/ci/check_cargo_dependency_layers.py"
SPEC = importlib.util.spec_from_file_location("cargo_dependency_layers", SCRIPT_PATH)
assert SPEC is not None and SPEC.loader is not None
cargo_dependency_layers = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = cargo_dependency_layers
SPEC.loader.exec_module(cargo_dependency_layers)


class CargoDependencyLayersTests(unittest.TestCase):
    def setUp(self) -> None:
        self.directory = tempfile.TemporaryDirectory()
        root = Path(self.directory.name)
        self.roots = {
            "thekernel": root / "TheKernel",
            "ax": root / "thekernel-ax",
            "linux_abi": root / "thekernel-linux-abi",
        }
        for workspace in self.roots.values():
            workspace.mkdir()

    def tearDown(self) -> None:
        self.directory.cleanup()

    def check(self, layer: str, dependency_layer: str) -> list[str]:
        workspace = self.roots[layer]
        package = {
            "name": "package",
            "manifest_path": str(workspace / "crate" / "Cargo.toml"),
            "dependencies": [
                {
                    "name": "dependency",
                    "path": str(self.roots[dependency_layer] / "crate"),
                }
            ],
        }
        with patch.object(cargo_dependency_layers, "metadata", return_value={"packages": [package]}):
            return list(cargo_dependency_layers.violations(workspace, self.roots))

    def test_thekernel_can_depend_on_sibling_layers(self) -> None:
        self.assertEqual(self.check("thekernel", "ax"), [])
        self.assertEqual(self.check("thekernel", "linux_abi"), [])

    def test_linux_abi_cannot_depend_on_ax(self) -> None:
        self.assertEqual(
            self.check("linux_abi", "ax"),
            ["package (linux_abi) depends on dependency (ax)"],
        )

    def test_ax_cannot_depend_on_thekernel(self) -> None:
        self.assertEqual(
            self.check("ax", "thekernel"),
            ["package (ax) depends on dependency (thekernel)"],
        )

    def test_registry_package_cannot_duplicate_controlled_package(self) -> None:
        package = {
            "name": "thekernel-axio",
            "manifest_path": "/cargo/registry/thekernel-axio/Cargo.toml",
            "dependencies": [],
            "targets": [{"name": "axio", "kind": ["lib"]}],
        }
        with patch.object(
            cargo_dependency_layers, "metadata", return_value={"packages": [package]}
        ):
            self.assertEqual(
                list(
                    cargo_dependency_layers.violations(
                        self.roots["thekernel"],
                        self.roots,
                        frozenset(("thekernel-axio",)),
                        frozenset(("axio",)),
                    )
                ),
                ["external dependency duplicates controlled sibling package thekernel-axio"],
            )

    def test_registry_library_cannot_shadow_controlled_target(self) -> None:
        package = {
            "name": "axio",
            "manifest_path": "/cargo/registry/axio/Cargo.toml",
            "dependencies": [],
            "targets": [{"name": "axio", "kind": ["lib"]}],
        }
        with patch.object(
            cargo_dependency_layers, "metadata", return_value={"packages": [package]}
        ):
            self.assertEqual(
                list(
                    cargo_dependency_layers.violations(
                        self.roots["thekernel"],
                        self.roots,
                        frozenset(),
                        frozenset(("axio",)),
                    )
                ),
                ["external dependency duplicates controlled sibling package axio"],
            )


if __name__ == "__main__":
    unittest.main()
