"""Check the Multiboot2 header of an assembled kernel image.

The image is the artifact that matters: the header in it is what the bootloader
walks, and no amount of source-level checking proves the linker put the tags
where the search finds them.  These tests build a small ELF here rather than
using a real kernel, so they run without a build and every rejection can be
attributed to one deliberate defect.
"""

from __future__ import annotations

from pathlib import Path
import struct
import unittest

from tests.support import (
    LOAD_SIZE,
    multiboot2_elf,
    multiboot2_header,
    multiboot2_tags,
    test_tmpdir,
)
from tools.qemu_runner.boot_artifacts import validate_thekernel_esp_kernel
from tools.qemu_runner.multiboot2 import MULTIBOOT2_HEADER_MAGIC, validate_multiboot2_header
from tools.qemu_runner.runner import RunnerError


def _validate(image: bytes) -> None:
    with test_tmpdir() as directory:
        kernel = Path(directory) / "kernel-x86_64"
        kernel.write_bytes(image)
        validate_multiboot2_header(kernel)


def _reject(image: bytes) -> str:
    with test_tmpdir() as directory:
        kernel = Path(directory) / "kernel-x86_64"
        kernel.write_bytes(image)
        with unittest.TestCase().assertRaises(RunnerError) as raised:
            validate_multiboot2_header(kernel)
        return str(raised.exception)


class Multiboot2ImageTests(unittest.TestCase):
    def test_a_correctly_built_image_is_accepted(self) -> None:
        _validate(multiboot2_elf(multiboot2_header()))

    def test_a_twenty_byte_tag_is_rejected(self) -> None:
        # The failure that motivated this check.  A 20-byte tag rounds up to 24
        # in GRUB, so the next tag has to start 48 bytes after this one starts,
        # not the 44 that `size` describes.  This image writes the four bytes
        # the specification's 4-byte rule wants instead of the eight GRUB's
        # 8-byte rule needs -- the exact mistake that cost a boot.
        tags = (
            struct.pack("<HHI", 2, 0, 24)
            + struct.pack("<IIII", 0x2000, 0x1000, 0x1800, 0x1800)
            + struct.pack("<HHI", 5, 0, 20)
            + struct.pack("<III", 0, 0, 0)
            # The next tag goes where `size` says, with four bytes of padding
            # after it -- so GRUB, rounding the 20 up to 24, reads the padding
            # as a tag header.
            + struct.pack("<HHI", 3, 0, 12)
            + struct.pack("<II", 0x1000, 0)
            + b"\x00\x00\x00\x00"
            + struct.pack("<HHI", 0, 0, 8)
        )
        message = _reject(multiboot2_elf(multiboot2_header(length=16 + len(tags), tags=tags)))
        self.assertIn("past the end of the header", message)

    def test_a_header_with_no_entry_address_tag_is_rejected(self) -> None:
        tags = (
            struct.pack("<HHI", 2, 0, 24)
            + struct.pack("<IIII", 0x2000, 0x1000, 0x1800, 0x1800)
            + struct.pack("<HHI", 0, 0, 8)
        )
        self.assertIn("entry-address tag", _reject(multiboot2_elf(multiboot2_header(tags=tags))))

    def test_a_header_with_no_address_tag_is_rejected(self) -> None:
        tags = struct.pack("<HHI", 3, 0, 12) + struct.pack("<II", 0x1000, 0) + struct.pack("<HHI", 0, 0, 8)
        self.assertIn("address tag", _reject(multiboot2_elf(multiboot2_header(tags=tags))))

    def test_a_length_that_does_not_cover_the_end_tag_is_rejected(self) -> None:
        # The length field is what the bootloader trusts; a header that declares
        # 16 bytes and then carries tags is a header whose tags are invisible.
        tags = multiboot2_tags()
        header = multiboot2_header(length=16 + len(tags), tags=tags)
        wrong = bytearray(header)
        struct.pack_into("<I", wrong, 8, 16)
        struct.pack_into("<I", wrong, 12, (-(MULTIBOOT2_HEADER_MAGIC + 0 + 16)) & 0xFFFF_FFFF)
        self.assertIn("leaves no room", _reject(multiboot2_elf(bytes(wrong))))

    def test_an_image_with_no_header_is_rejected(self) -> None:
        # An all-zero run is the dangerous case: it sums to zero, so the search
        # accepts the position and only the magic number distinguishes it.
        self.assertIn(
            "is not a Multiboot2 header",
            _reject(multiboot2_elf(b"\x00" * LOAD_SIZE)),
        )

    def test_a_file_that_is_not_an_elf_is_rejected(self) -> None:
        self.assertIn("not an ELF", _reject(b"not an elf at all"))

    def test_the_check_is_reached_from_the_esp_validator(self) -> None:
        # The point of the module is to stop a boot before QEMU runs, so the
        # path every TheKernel boot takes has to call it.
        with test_tmpdir() as directory:
            kernel = Path(directory) / "kernel-x86_64"
            kernel.write_bytes(multiboot2_elf(b"\x00" * LOAD_SIZE))
            with self.assertRaisesRegex(RunnerError, "is not a Multiboot2 header"):
                validate_thekernel_esp_kernel(kernel, Path(directory) / "absent.esp")


class RealKernelHeaderTests(unittest.TestCase):
    """Read the header of a built kernel when one happens to be available.

    Skipped rather than failing when there is no build, because the daily tier
    runs the host suite before the build stage; the point is that a developer
    who has built a kernel gets the check for free.
    """

    def test_the_most_recently_built_kernel_carries_a_header_grub_can_walk(self) -> None:
        # Deliberately the newest build rather than every build: the target
        # cache legitimately holds kernels built from older revisions of this
        # header, and failing on those would turn a regression check into a
        # complaint about build history.
        candidates = sorted(
            Path("/home/ava/.cache/thekernel-targets").glob(
                "*/out/x86_64/*/*/mem*/kernel-x86_64"
            ),
            key=lambda path: path.stat().st_mtime,
            reverse=True,
        )
        if not candidates:
            self.skipTest("no built kernel to inspect")
        validate_multiboot2_header(candidates[0])


if __name__ == "__main__":
    unittest.main()
