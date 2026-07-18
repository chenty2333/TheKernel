from __future__ import annotations

import hashlib
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from tools.qemu_runner.evidence import EvidenceError, file_evidence


class ReplaceOnRead:
    def __init__(self, source, replacement: Path, destination: Path) -> None:
        self.source = source
        self.replacement = replacement
        self.destination = destination
        self.replaced = False

    def __enter__(self):
        self.source.__enter__()
        return self

    def __exit__(self, *args):
        return self.source.__exit__(*args)

    def fileno(self) -> int:
        return self.source.fileno()

    def read(self, size: int = -1) -> bytes:
        if not self.replaced:
            self.replacement.replace(self.destination)
            self.replaced = True
        return self.source.read(size)


class EvidenceTests(unittest.TestCase):
    def test_file_evidence_records_canonical_content(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            artifact = Path(directory) / "artifact"
            artifact.write_bytes(b"stable")
            self.assertEqual(
                file_evidence(artifact),
                {
                    "path": str(artifact.resolve()),
                    "size_bytes": 6,
                    "sha256": hashlib.sha256(b"stable").hexdigest(),
                },
            )

    def test_file_evidence_rejects_atomic_path_replacement(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            artifact = Path(directory) / "artifact"
            replacement = Path(directory) / "replacement"
            artifact.write_bytes(b"old content")
            replacement.write_bytes(b"new content")
            original_open = Path.open

            def replacing_open(path: Path, *args, **kwargs):
                source = original_open(path, *args, **kwargs)
                if path.resolve() == artifact.resolve():
                    return ReplaceOnRead(source, replacement, artifact)
                return source

            with patch.object(Path, "open", new=replacing_open):
                with self.assertRaisesRegex(EvidenceError, "changed while hashing"):
                    file_evidence(artifact)
            self.assertEqual(artifact.read_bytes(), b"new content")


if __name__ == "__main__":
    unittest.main()
