#!/usr/bin/env python3
"""Audit a Cargo metadata graph for exact TheKernel release artifacts."""

from __future__ import annotations

import argparse
import json
import sys
from collections import deque
from pathlib import Path
from typing import Any, NoReturn


LEGACY_PACKAGES = frozenset({"axpoll", "axsched", "starry-process"})


def fail(message: str) -> NoReturn:
    print(f"release dependency graph failed: {message}", file=sys.stderr)
    raise SystemExit(1)


def package_root(package: dict[str, Any]) -> Path:
    manifest_path = package.get("manifest_path")
    if not isinstance(manifest_path, str):
        fail(f"package {package.get('name')!r} has no manifest_path")
    return Path(manifest_path).resolve().parent


def parse_mapping(value: str) -> tuple[str, Path]:
    if "=" not in value:
        raise argparse.ArgumentTypeError("mapping must be PACKAGE=PATH")
    name, path = value.split("=", 1)
    if not name or not path:
        raise argparse.ArgumentTypeError("mapping values must be non-empty")
    return name, Path(path).resolve()


def is_within(path: Path, root: Path) -> bool:
    try:
        path.relative_to(root)
        return True
    except ValueError:
        return False


def reachable_package_ids(metadata: dict[str, Any]) -> set[str]:
    resolve = metadata.get("resolve")
    if not isinstance(resolve, dict):
        fail("metadata has no resolve graph")
    raw_nodes = resolve.get("nodes")
    if not isinstance(raw_nodes, list):
        fail("metadata resolve graph has no nodes")
    adjacency: dict[str, list[str]] = {}
    for node in raw_nodes:
        if not isinstance(node, dict) or not isinstance(node.get("id"), str):
            fail("metadata contains an invalid resolve node")
        dependencies = node.get("dependencies", [])
        if not isinstance(dependencies, list) or not all(
            isinstance(dependency, str) for dependency in dependencies
        ):
            fail(f"resolve node {node['id']!r} has invalid dependencies")
        adjacency[node["id"]] = dependencies

    workspace_members = metadata.get("workspace_members")
    if not isinstance(workspace_members, list) or not all(
        isinstance(member, str) for member in workspace_members
    ):
        fail("metadata has invalid workspace_members")
    pending = deque(workspace_members)
    reachable: set[str] = set()
    while pending:
        package_id = pending.popleft()
        if package_id in reachable:
            continue
        reachable.add(package_id)
        pending.extend(adjacency.get(package_id, []))
    return reachable


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--metadata", required=True, type=Path)
    parser.add_argument("--consumer-root", required=True, type=Path)
    parser.add_argument("--expect", action="append", default=[], type=parse_mapping)
    parser.add_argument(
        "--release-source-root", action="append", default=[], type=Path
    )
    parser.add_argument("--allowed-axtask-facade", required=True, type=Path)
    parser.add_argument("--allowed-process-adapter", required=True, type=Path)
    args = parser.parse_args()

    if not args.expect:
        fail("at least one expected release artifact is required")
    expected: dict[str, Path] = {}
    for name, path in args.expect:
        if name in expected:
            fail(f"duplicate expected package {name!r}")
        if not path.is_dir():
            fail(f"expected artifact directory does not exist for {name}")
        expected[name] = path

    try:
        metadata = json.loads(args.metadata.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        fail(f"cannot load metadata: {error}")
    packages = metadata.get("packages")
    if not isinstance(packages, list):
        fail("metadata has no package list")
    for package in packages:
        if not isinstance(package, dict) or not isinstance(package.get("id"), str):
            fail("metadata contains an invalid package")

    reachable = reachable_package_ids(metadata)
    consumer_root = args.consumer_root.resolve()
    facade_root = args.allowed_axtask_facade.resolve()
    process_adapter_root = args.allowed_process_adapter.resolve()
    release_source_roots = [path.resolve() for path in args.release_source_root]
    if not consumer_root.is_dir():
        fail("consumer root does not exist")
    if not facade_root.is_dir():
        fail("allowed axtask facade directory does not exist")
    if not process_adapter_root.is_dir():
        fail("allowed process adapter directory does not exist")
    if any(not path.is_dir() for path in release_source_roots):
        fail("a release source root does not exist")
    legacy_vendor_roots = (
        consumer_root / "third_party/rust-patches/axpoll",
        consumer_root / "third_party/rust-patches/axsched",
        consumer_root / "third_party/rust-patches/axtask",
        consumer_root / "third_party/rust-patches/starry-process",
    )

    errors: list[str] = []
    for package in packages:
        name = package.get("name")
        source = package.get("source")
        root = package_root(package)
        if name in LEGACY_PACKAGES:
            errors.append(
                f"legacy package {name} resolved from "
                f"{'a non-local source' if source is not None else root.as_posix()}"
            )
        if name == "axtask" and (source is not None or root != facade_root):
            errors.append(
                "legacy axtask resolved instead of the one-state compatibility "
                f"facade: {'non-local source' if source is not None else root.as_posix()}"
            )
        elif name == "axtask":
            dependencies = package.get("dependencies", [])
            core_dependencies = [
                dependency
                for dependency in dependencies
                if isinstance(dependency, dict)
                and dependency.get("name") == "thekernel-axtask"
                and dependency.get("rename") == "axtask-core"
                and dependency.get("req") == "=0.1.0"
            ]
            if package.get("publish") != [] or len(core_dependencies) != 1:
                errors.append(
                    "local axtask facade is publishable or does not re-export "
                    "the exact thekernel-axtask 0.1.0 package"
                )
        if any(is_within(root, legacy_root) for legacy_root in legacy_vendor_roots):
            errors.append(f"legacy vendored package is reachable: {root}")
        for source_root in release_source_roots:
            if is_within(root, source_root):
                errors.append(
                    f"release source workspace leaked into consumer graph: {root}"
                )

    process_adapters = [
        package
        for package in packages
        if package.get("name") == "thekernel-linux-process-adapter"
    ]
    if len(process_adapters) != 1:
        errors.append(
            "expected exactly one thekernel-linux-process-adapter package, "
            f"found {len(process_adapters)}"
        )
    else:
        process_adapter = process_adapters[0]
        adapter_dependencies = process_adapter.get("dependencies", [])
        if not isinstance(adapter_dependencies, list):
            adapter_dependencies = []
            errors.append("local process adapter has invalid dependency metadata")
        process_core_dependencies = [
            dependency
            for dependency in adapter_dependencies
            if isinstance(dependency, dict)
            and dependency.get("name") == "thekernel-linux-process"
            and dependency.get("rename") is None
            and dependency.get("req") == "=0.1.0"
            and dependency.get("uses_default_features") is False
        ]
        adapter_root = package_root(process_adapter)
        if (
            process_adapter.get("version") != "0.1.0"
            or process_adapter.get("source") is not None
            or adapter_root != process_adapter_root
            or process_adapter.get("publish") != []
            or len(process_core_dependencies) != 1
        ):
            errors.append(
                "local process adapter is not the exact unpublished 0.1.0 "
                "adapter over the unrenamed, no-default-features "
                "thekernel-linux-process =0.1.0 dependency"
            )
        if process_adapter["id"] not in reachable:
            errors.append(
                "local process adapter is present but not reachable from a "
                "workspace consumer"
            )

    for name, expected_root in expected.items():
        matches = [package for package in packages if package.get("name") == name]
        if len(matches) != 1:
            errors.append(
                f"expected exactly one {name} package, found {len(matches)}"
            )
            continue
        package = matches[0]
        actual_root = package_root(package)
        if package.get("version") != "0.1.0":
            errors.append(
                f"{name} resolved version {package.get('version')!r}, expected '0.1.0'"
            )
        if package.get("source") is not None:
            errors.append(
                f"{name} resolved a non-artifact registry or Git source"
            )
        if actual_root != expected_root:
            errors.append(
                f"{name} resolved from {actual_root}, expected exact artifact "
                f"{expected_root}"
            )
        if package["id"] not in reachable:
            errors.append(f"{name} is present but not reachable from a workspace consumer")

    if errors:
        fail("\n  " + "\n  ".join(errors))

    print(
        "release dependency graph: PASS "
        f"({len(expected)} exact artifacts, {len(packages)} resolved packages)"
    )


if __name__ == "__main__":
    main()
