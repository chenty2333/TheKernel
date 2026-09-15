#!/usr/bin/env python3
"""Validate declared crate layers and all local dependency edges."""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import subprocess
import sys
import tomllib

ALLOWED = {
    "mechanism": {"mechanism"},
    "platform": {"platform", "mechanism"},
    "linux_abi": {"linux_abi", "mechanism"},
    "integration": {"integration", "platform", "linux_abi", "mechanism"},
}


def violations(data: dict, root: Path) -> list[str]:
    packages = data["packages"]
    members = set(data["workspace_members"])
    local = {p["id"]: p for p in packages if p["id"] in members}
    errors = []
    layers = {}
    paths = {}
    names = {p["name"] for p in local.values()}
    libraries = {t["name"] for p in local.values() for t in p["targets"] if "lib" in t["kind"]}
    for identifier, package in local.items():
        layer = package.get("metadata", {}).get("thekernel", {}).get("layer")
        if layer not in ALLOWED:
            errors.append(f'{package["name"]}: missing or invalid package.metadata.thekernel.layer')
        else:
            layers[identifier] = layer
        paths[Path(package["manifest_path"]).resolve().parent] = identifier
    for package in packages:
        identifier = package["id"]
        if identifier not in local:
            targets = {t["name"] for t in package["targets"] if "lib" in t["kind"]}
            if package["name"] in names or targets & libraries:
                errors.append(f'external dependency duplicates controlled workspace package {package["name"]}')
            continue
        for dependency in package["dependencies"]:
            if "path" not in dependency:
                continue
            target = paths.get(Path(dependency["path"]).resolve())
            if target is None:
                errors.append(f'{package["name"]}: uncontrolled path dependency {dependency["name"]}')
            elif identifier in layers and target in layers and layers[target] not in ALLOWED[layers[identifier]]:
                errors.append(f'{package["name"]} ({layers[identifier]}) depends on {dependency["name"]} ({layers[target]})')
    return errors


def workspace_policy_violations(data: dict, root: Path) -> list[str]:
    """Keep controlled packages on one compiler and an explicit release policy."""
    policy = tomllib.loads((root / "Cargo.toml").read_text())["workspace"]["package"]
    errors = []
    members = set(data["workspace_members"])
    for package in data["packages"]:
        if package["id"] not in members:
            continue
        name = package["name"]
        if name != "thekernel" and not name.startswith("tk-"):
            errors.append(f"{name}: component package must use the tk- prefix")
        for field, expected in (("rust_version", policy["rust-version"]),
                                ("repository", policy["repository"])):
            if package.get(field) != expected:
                errors.append(f"{name}: {field} differs from workspace policy")
        # The default stays private. Release-ready packages explicitly opt in
        # to crates.io, never to an implicit or alternate registry.
        if package.get("publish") not in ([], ["crates-io"]):
            errors.append(f"{name}: publish differs from workspace policy")
        directory = Path(package["manifest_path"]).resolve().parent
        while directory != root and root in directory.parents:
            for filename in ("rust-toolchain", "rust-toolchain.toml"):
                if (directory / filename).exists():
                    errors.append(f"{name}: component-local {filename} overrides workspace compiler")
            directory = directory.parent
    return errors


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--thekernel", type=Path, default=Path.cwd())
    args = parser.parse_args(argv)
    try:
        result = subprocess.run([
            "cargo", "metadata", "--locked", "--format-version", "1",
            "--filter-platform", "x86_64-unknown-none", "--manifest-path",
            str(args.thekernel.resolve() / "Cargo.toml"),
        ], check=True, capture_output=True, text=True)
        data = json.loads(result.stdout)
        errors = violations(data, args.thekernel.resolve())
        errors += workspace_policy_violations(data, args.thekernel.resolve())
    except (OSError, ValueError, KeyError, subprocess.CalledProcessError) as exc:
        print(f"cargo dependency layers: {exc}", file=sys.stderr)
        return 1
    if errors:
        print("cargo dependency layer violations:", file=sys.stderr)
        print(*errors, sep="\n", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
