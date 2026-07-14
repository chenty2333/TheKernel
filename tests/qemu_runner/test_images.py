from __future__ import annotations

import gzip
import lzma
import tempfile
import unittest
from pathlib import Path

from tools.qemu_runner.images import (
    ImageError,
    materialize_writable_image,
    prepare_image,
)


class ImageTests(unittest.TestCase):
    def test_plain_image_is_used_directly(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            image = root / "root.img"
            image.write_bytes(b"plain")
            prepared = prepare_image(image, cache_dir=root / "cache")
            self.assertFalse(prepared.cached)
            self.assertEqual(prepared.runtime, image.resolve())

    def test_empty_image_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            image = root / "empty.img"
            image.touch()
            with self.assertRaisesRegex(ImageError, "image is empty"):
                prepare_image(image, cache_dir=root / "cache")

    def test_compressed_image_is_decompressed_once(self) -> None:
        for suffix, opener in ((".gz", gzip.open), (".xz", lzma.open)):
            with self.subTest(suffix=suffix), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                source = root / f"root.img{suffix}"
                with opener(source, "wb") as output:
                    output.write(b"filesystem")
                first = prepare_image(source, cache_dir=root / "cache")
                second = prepare_image(source, cache_dir=root / "cache")
                self.assertTrue(first.cached)
                self.assertEqual(first.runtime, second.runtime)
                self.assertEqual(first.runtime.read_bytes(), b"filesystem")

    def test_writable_compressed_image_does_not_mutate_cache(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "extra.img.gz"
            with gzip.open(source, "wb") as output:
                output.write(b"original")
            prepared = prepare_image(source, cache_dir=root / "cache")
            writable = materialize_writable_image(
                prepared,
                destination_dir=root / "run" / "writable-images",
                label="extra",
            )
            writable.write_bytes(b"changed")
            self.assertEqual(prepared.runtime.read_bytes(), b"original")


if __name__ == "__main__":
    unittest.main()
