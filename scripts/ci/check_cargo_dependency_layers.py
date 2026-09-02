#!/usr/bin/env python3
"""Reject local Cargo dependency edges that cross component-layer boundaries."""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from collections.abc import Iterable
from pathlib import Path

TARGET = "x86_64-unknown-none"


class LayerError(ValueError):
    """A controlled path dependency violates the workspace layering policy."""


def metadata(manifest_path: Path) -> dict[str, object]:
    command = [
        "cargo",
        "metadata",
        "--locked",
        "--format-version",
        "1",
        "--filter-platform",
        TARGET,
        "--manifest-path",
        str(manifest_path),
    ]
    completed = subprocess.run(command, check=True, capture_output=True, text=True)
    return json.loads(completed.stdout)


def is_within(path: Path, root: Path) -> bool:
    try:
        path.relative_to(root)
    except ValueError:
        return False
    return True


def layer_for(path: Path, roots: dict[str, Path]) -> str | None:
    for layer, root in roots.items():
        if is_within(path, root):
            return layer
    return None


def allowed_layers(layer: str) -> frozenset[str]:
    if layer in {"ax", "linux_abi"}:
        return frozenset((layer,))
    return frozenset(("thekernel", "ax", "linux_abi"))


def package_targets(package: dict[str, object]) -> frozenset[str]:
    targets = package.get("targets")
    if not isinstance(targets, list):
        return frozenset()
    names: set[str] = set()
    for target in targets:
        if not isinstance(target, dict):
            continue
        name = target.get("name")
        kinds = target.get("kind")
        if isinstance(name, str) and isinstance(kinds, list) and "lib" in kinds:
            names.add(name)
    return frozenset(names)


def controlled_names(
    roots: dict[str, Path],
) -> tuple[frozenset[str], frozenset[str]]:
    package_names: set[str] = set()
    library_names: set[str] = set()
    for layer in ("ax", "linux_abi"):
        data = metadata(roots[layer] / "Cargo.toml")
        packages = data.get("packages")
        if not isinstance(packages, list):
            raise LayerError(f"cargo metadata returned no packages for {roots[layer]}")
        for package in packages:
            if not isinstance(package, dict):
                continue
            manifest = package.get("manifest_path")
            name = package.get("name")
            if not isinstance(manifest, str) or not is_within(
                Path(manifest).resolve(), roots[layer]
            ):
                continue
            if isinstance(name, str):
                package_names.add(name)
            library_names.update(package_targets(package))
    return frozenset(package_names), frozenset(library_names)


def violations(
    workspace: Path,
    roots: dict[str, Path],
    controlled_packages: frozenset[str] = frozenset(),
    controlled_libraries: frozenset[str] = frozenset(),
) -> Iterable[str]:
    data = metadata(workspace / "Cargo.toml")
    packages = data.get("packages")
    if not isinstance(packages, list):
        raise LayerError(f"cargo metadata returned no packages for {workspace}")

    for package in packages:
        if not isinstance(package, dict):
            raise LayerError(f"cargo metadata returned an invalid package for {workspace}")
        manifest = package.get("manifest_path")
        dependencies = package.get("dependencies")
        name = package.get("name")
        if not isinstance(manifest, str) or not isinstance(dependencies, list):
            raise LayerError(f"cargo metadata returned an invalid package for {workspace}")
        package_layer = layer_for(Path(manifest).resolve(), roots)
        if package_layer is None:
            external_targets = package_targets(package) & controlled_libraries
            if name in controlled_packages or external_targets:
                controlled_name = name if name in controlled_packages else min(external_targets)
                yield f"external dependency duplicates controlled sibling package {controlled_name}"
            continue
        for dependency in dependencies:
            if not isinstance(dependency, dict):
                raise LayerError(f"cargo metadata returned an invalid dependency for {name}")
            path = dependency.get("path")
            if not isinstance(path, str):
                continue
            dependency_path = Path(path).resolve()
            dependency_layer = layer_for(dependency_path, roots)
            if dependency_layer is None:
                yield (
                    f"{name} ({package_layer}) has uncontrolled path dependency "
                    f"{dependency.get('name', dependency_path)} at {dependency_path}"
                )
            elif dependency_layer not in allowed_layers(package_layer):
                yield (
                    f"{name} ({package_layer}) depends on "
                    f"{dependency.get('name', dependency_path)} ({dependency_layer})"
                )


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--thekernel", type=Path, default=Path.cwd())
    parser.add_argument("--ax", type=Path)
    parser.add_argument("--linux-abi", type=Path)
    args = parser.parse_args(argv)

    thekernel = args.thekernel.resolve()
    roots = {
        "thekernel": thekernel,
        "ax": (args.ax or thekernel.parent / "thekernel-ax").resolve(),
        "linux_abi": (
            args.linux_abi or thekernel.parent / "thekernel-linux-abi"
        ).resolve(),
    }
    missing = [str(root) for root in roots.values() if not (root / "Cargo.toml").is_file()]
    if missing:
        parser.error("missing workspace manifest: " + ", ".join(missing))

    try:
        controlled_packages, controlled_libraries = controlled_names(roots)
        errors = [
            error
            for root in roots.values()
            for error in violations(
                root,
                roots,
                controlled_packages,
                controlled_libraries,
            )
        ]
    except (LayerError, json.JSONDecodeError, subprocess.CalledProcessError) as exc:
        print(f"cargo dependency layers: {exc}", file=sys.stderr)
        return 1

    if errors:
        print("cargo dependency layer violations:", file=sys.stderr)
        print(*errors, sep="\n", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
