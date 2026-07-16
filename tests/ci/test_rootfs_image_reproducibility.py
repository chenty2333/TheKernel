#!/usr/bin/env python3
"""Fast byte-reproducibility test for the production ext4 image helper."""

from __future__ import annotations

import hashlib
import os
import subprocess
import tempfile
import time
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
IMAGE_BUILDER = REPO_ROOT / "scripts" / "create-rootfs-image.sh"


def digest(path: Path) -> str:
    result = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(1024 * 1024):
            result.update(chunk)
    return result.hexdigest()


class RootfsImageReproducibilityTests(unittest.TestCase):
    def populate(self, stage: Path, *, reverse: bool, timestamp: int) -> None:
        entries = (
            ("etc/value", b"fixture\n"),
            ("opt/tools/probe", b"#!/bin/sh\nexit 0\n"),
            ("usr/share/doc/readme", b"reproducible rootfs\n"),
            ("var/lib/back\\slash", b"literal backslash\n"),
        )
        for relative, content in reversed(entries) if reverse else entries:
            path = stage / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(content)
            os.utime(path, (timestamp, timestamp))
        (stage / "bin").mkdir()
        (stage / "bin" / "probe").symlink_to("../opt/tools/probe")
        for directory in sorted(
            (path for path in stage.rglob("*") if path.is_dir()), reverse=reverse
        ):
            os.utime(directory, (timestamp, timestamp))
        os.utime(stage, (timestamp, timestamp))

    def build(self, stage: Path, output: Path) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [
                str(IMAGE_BUILDER),
                "--arch",
                "rv",
                "--stage",
                str(stage),
                "--output",
                str(output),
                "--size-mb",
                "32",
                "--owner-mode",
                "preserve",
            ],
            cwd=REPO_ROOT,
            env={**os.environ, "SOURCE_DATE_EPOCH": "1704067200"},
            check=False,
            capture_output=True,
            text=True,
        )

    def test_logically_equal_trees_build_byte_identical_images(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            first_stage = root / "first-stage"
            second_stage = root / "second-stage"
            first_stage.mkdir()
            second_stage.mkdir()
            self.populate(first_stage, reverse=False, timestamp=1700000000)
            self.populate(second_stage, reverse=True, timestamp=1800000000)
            first = root / "outputs-a" / "rootfs.img"
            second = root / "outputs-b" / "rootfs.img"

            first_result = self.build(first_stage, first)
            # Source ctime cannot be assigned with utime(2). Cross a whole
            # filesystem timestamp tick so this test catches mke2fs -d leaking
            # wall-clock source metadata into otherwise equal images.
            time.sleep(1.1)
            second_result = self.build(second_stage, second)

            self.assertEqual(first_result.returncode, 0, first_result.stderr)
            self.assertEqual(second_result.returncode, 0, second_result.stderr)
            self.assertEqual(first.stat().st_size, second.stat().st_size)
            self.assertEqual(digest(first), digest(second))
            self.assertEqual(first.read_bytes(), second.read_bytes())

    def test_unrepresentable_debugfs_path_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            stage = root / "stage"
            stage.mkdir()
            (stage / 'unsupported"name').write_bytes(b"fixture\n")
            output = root / "rootfs.img"

            result = self.build(stage, output)

            self.assertEqual(result.returncode, 1)
            self.assertIn("cannot be represented by debugfs", result.stderr)
            self.assertFalse(output.exists())

    def test_invalid_source_date_epoch_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            stage = root / "stage"
            stage.mkdir()
            result = subprocess.run(
                [
                    str(IMAGE_BUILDER),
                    "--arch",
                    "rv",
                    "--stage",
                    str(stage),
                    "--output",
                    str(root / "rootfs.img"),
                ],
                cwd=REPO_ROOT,
                env={**os.environ, "SOURCE_DATE_EPOCH": "not-a-time"},
                check=False,
                capture_output=True,
                text=True,
            )
            self.assertEqual(result.returncode, 2)
            self.assertIn("SOURCE_DATE_EPOCH must be", result.stderr)


if __name__ == "__main__":
    unittest.main()
