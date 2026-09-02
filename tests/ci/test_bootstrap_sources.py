#!/usr/bin/env python3
"""Focused safety tests for scripts/ci/bootstrap_sources.py."""

from __future__ import annotations

import importlib.util
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from typing import Dict

REPO_ROOT = Path(__file__).resolve().parents[2]
CI_DIR = REPO_ROOT / "scripts/ci"
sys.path.insert(0, str(CI_DIR))
SCRIPT_PATH = CI_DIR / "bootstrap_sources.py"
SPEC = importlib.util.spec_from_file_location("bootstrap_sources", SCRIPT_PATH)
assert SPEC is not None and SPEC.loader is not None
bootstrap_sources = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = bootstrap_sources
SPEC.loader.exec_module(bootstrap_sources)


def run_git(directory: Path, *arguments: str) -> str:
    return subprocess.check_output(["git", "-C", str(directory), *arguments], text=True).strip()


class BootstrapSourcesTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary_root = Path(
            os.environ.get(
                "THEKERNEL_TEST_TMPDIR",
                Path.home() / ".cache" / "thekernel-test-tmp",
            )
        )
        temporary_root.mkdir(parents=True, exist_ok=True)
        self.temporary = tempfile.TemporaryDirectory(dir=temporary_root)
        self.root = Path(self.temporary.name)
        self.parent = self.root / "parent"
        self.parent.mkdir()
        self.remotes = self.root / "remotes"
        self.sources: Dict[str, bootstrap_sources.source_combination.Source] = {}
        for name, path in (("ax", "thekernel-ax"), ("linux_abi", "thekernel-linux-abi")):
            commit = self.create_remote(name)
            self.sources[name] = bootstrap_sources.source_combination.Source(
                repository=f"example/{name}", ref=commit, path=path
            )

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def create_remote(self, name: str) -> str:
        worktree = self.root / f"{name}-worktree"
        remote = self.remotes / "example" / f"{name}.git"
        remote.parent.mkdir(parents=True, exist_ok=True)
        subprocess.run(
            ["git", "init", "--quiet", "--initial-branch=main", str(worktree)],
            check=True,
        )
        run_git(worktree, "config", "user.email", "tests@example.invalid")
        run_git(worktree, "config", "user.name", "TheKernel tests")
        (worktree / "source.txt").write_text(f"{name}\n", encoding="utf-8")
        run_git(worktree, "add", "source.txt")
        run_git(worktree, "commit", "--quiet", "-m", "initial")
        commit = run_git(worktree, "rev-parse", "HEAD")
        subprocess.run(["git", "clone", "--bare", "--quiet", str(worktree), str(remote)], check=True)
        return commit

    def test_creates_and_then_verifies_exact_clean_checkouts(self) -> None:
        bootstrap_sources.bootstrap(self.parent, self.sources, str(self.remotes))

        for name, source in self.sources.items():
            destination = self.parent / source.path
            self.assertEqual(run_git(destination, "rev-parse", "HEAD"), source.ref)
            self.assertEqual(run_git(destination, "branch", "--show-current"), "")
            self.assertEqual(
                (destination / "source.txt").read_text(encoding="utf-8"),
                f"{name}\n",
            )
            self.assertEqual(run_git(destination, "status", "--porcelain"), "")

        bootstrap_sources.bootstrap(self.parent, self.sources, str(self.remotes))

    def test_refuses_existing_checkout_at_a_different_commit_without_modifying_it(self) -> None:
        source = self.sources["ax"]
        destination = self.parent / source.path
        subprocess.run(
            ["git", "clone", "--quiet", str(self.remotes / "example" / "ax.git"), str(destination)],
            check=True,
        )
        run_git(destination, "config", "user.email", "tests@example.invalid")
        run_git(destination, "config", "user.name", "TheKernel tests")
        (destination / "second.txt").write_text("second\n", encoding="utf-8")
        run_git(destination, "add", "second.txt")
        run_git(destination, "commit", "--quiet", "-m", "second")
        run_git(destination, "checkout", "--quiet", "--detach")
        original_head = run_git(destination, "rev-parse", "HEAD")

        with self.assertRaisesRegex(bootstrap_sources.BootstrapError, "expected"):
            bootstrap_sources.bootstrap(self.parent, {"ax": source}, str(self.remotes))

        self.assertEqual(run_git(destination, "rev-parse", "HEAD"), original_head)

    def test_refuses_an_attached_branch_at_the_exact_commit(self) -> None:
        source = self.sources["ax"]
        destination = self.parent / source.path
        subprocess.run(
            ["git", "clone", "--quiet", str(self.remotes / "example" / "ax.git"), str(destination)],
            check=True,
        )
        self.assertEqual(run_git(destination, "rev-parse", "HEAD"), source.ref)
        with self.assertRaisesRegex(bootstrap_sources.BootstrapError, "not detached"):
            bootstrap_sources.bootstrap(self.parent, {"ax": source}, str(self.remotes))

    def test_refuses_dirty_existing_checkout(self) -> None:
        bootstrap_sources.bootstrap(self.parent, {"ax": self.sources["ax"]}, str(self.remotes))
        destination = self.parent / self.sources["ax"].path
        (destination / "untracked.txt").write_text("dirty\n", encoding="utf-8")

        with self.assertRaisesRegex(bootstrap_sources.BootstrapError, "dirty"):
            bootstrap_sources.bootstrap(self.parent, {"ax": self.sources["ax"]}, str(self.remotes))

    def test_refuses_existing_checkout_with_a_different_origin(self) -> None:
        source = self.sources["ax"]
        bootstrap_sources.bootstrap(self.parent, {"ax": source}, str(self.remotes))
        destination = self.parent / source.path
        run_git(destination, "remote", "set-url", "origin", "https://example.invalid/ax.git")

        with self.assertRaisesRegex(bootstrap_sources.BootstrapError, "origin"):
            bootstrap_sources.bootstrap(self.parent, {"ax": source}, str(self.remotes))

    def test_accepts_only_canonical_github_ssh_origin_for_github_source(self) -> None:
        source = self.sources["ax"]
        bootstrap_sources.bootstrap(self.parent, {"ax": source}, str(self.remotes))
        destination = self.parent / source.path
        run_git(destination, "remote", "set-url", "origin", "https://github.com/example/ax.git")

        bootstrap_sources.bootstrap(
            self.parent, {"ax": source}, "https://github.com"
        )

        run_git(destination, "remote", "set-url", "origin", "git@github.com:example/ax.git")

        bootstrap_sources.bootstrap(
            self.parent, {"ax": source}, "https://github.com"
        )

        for origin in (
            "git@github.com:example/ax",
            "https://token@github.com/example/ax.git",
            "ssh://git@github.com/example/ax.git",
            "file:///tmp/example/ax.git",
        ):
            run_git(destination, "remote", "set-url", "origin", origin)
            with self.subTest(origin=origin):
                with self.assertRaisesRegex(bootstrap_sources.BootstrapError, "origin"):
                    bootstrap_sources.bootstrap(
                        self.parent, {"ax": source}, "https://github.com"
                    )

    def test_refuses_dangling_sibling_symlink_without_replacing_it(self) -> None:
        source = self.sources["ax"]
        destination = self.parent / source.path
        destination.symlink_to(self.root / "missing-checkout")
        self.assertFalse(destination.exists())

        with self.assertRaisesRegex(bootstrap_sources.BootstrapError, "symlink"):
            bootstrap_sources.bootstrap(self.parent, {"ax": source}, str(self.remotes))

        self.assertTrue(destination.is_symlink())

    def test_atomic_install_does_not_replace_a_concurrently_created_destination(self) -> None:
        temporary = self.parent / "temporary-checkout"
        temporary.mkdir()
        destination = self.parent / self.sources["ax"].path
        destination.mkdir()
        marker = destination / "do-not-replace"
        marker.write_text("present\n", encoding="utf-8")

        with self.assertRaisesRegex(bootstrap_sources.BootstrapError, "concurrently"):
            bootstrap_sources.install_no_replace(temporary, destination)

        self.assertTrue(temporary.is_dir())
        self.assertEqual(marker.read_text(encoding="utf-8"), "present\n")


if __name__ == "__main__":
    unittest.main()
