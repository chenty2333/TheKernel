"""Check the Multiboot2 header of the kernel an image will actually boot.

`tests/test_multiboot_header.py` reads the header source and the constant that
describes its length, which catches the arithmetic mistakes.  This module reads
the *assembled bytes* instead, so it catches the mistakes the source cannot:
a header the linker placed somewhere the bootloader will not look, a length
field that does not match what was assembled, and -- the failure that cost real
time -- a tag GRUB cannot reach because the previous tag's size is not a
multiple of eight and the padding was written to the wrong boundary.

The check is deliberately dependency-free: no external tool, no `readelf`, and
no assumption about where in the image the header sits beyond "inside the first
loadable segment", which is where a Multiboot2 header has to be for a
bootloader to find it.
"""

from __future__ import annotations

from pathlib import Path
import struct

from .runner import RunnerError


MULTIBOOT2_HEADER_MAGIC = 0xE852_50D6
MULTIBOOT2_ARCHITECTURE_I386 = 0
MULTIBOOT2_END_TAG = 0
MULTIBOOT2_ENTRY_ADDRESS_TAG = 3
MULTIBOOT2_ADDRESS_TAG = 2
# GRUB walks the header with `ALIGN_UP(tag->size, MULTIBOOT_TAG_ALIGN)`, and
# `multiboot_mbi2.c` sets MULTIBOOT_TAG_ALIGN to 8.  The Multiboot2
# specification only asks for 4-byte alignment, which is why a tag of 20 bytes
# looks correct and is not.
TAG_ALIGN = 8
HEADER_ALIGN = 8
MASK = 0xFFFF_FFFF
# `find_header` scans at most 32768 bytes from the start of the image before it
# gives up, and stops at the first position whose first four words sum to zero.
# Searching further would find arithmetic coincidences in ordinary kernel data
# that the bootloader never reaches.
SEARCH_LIMIT = 32768


def _segments(image: bytes) -> list[tuple[int, int]]:
    """Return `(file_offset, file_size)` for every PT_LOAD segment."""
    if image[:4] != b"\x7fELF":
        raise RunnerError("kernel is not an ELF file")
    if image[4] != 2:
        raise RunnerError("kernel is not a 64-bit ELF file")
    program_offset, = struct.unpack_from("<Q", image, 0x20)
    program_size, program_count = struct.unpack_from("<HH", image, 0x36)
    if program_offset + program_size * program_count > len(image):
        raise RunnerError("kernel program header table lies outside the file")
    segments = []
    for index in range(program_count):
        base = program_offset + index * program_size
        kind, = struct.unpack_from("<I", image, base)
        file_offset, = struct.unpack_from("<Q", image, base + 0x08)
        file_size, = struct.unpack_from("<Q", image, base + 0x20)
        if kind == 1 and file_size:
            segments.append((file_offset, file_size))
    if not segments:
        raise RunnerError("kernel has no loadable segments")
    return segments


def _header(image: bytes, start: int, limit: int) -> tuple[int, int]:
    """Return `(header_offset, header_length)` for the Multiboot2 header.

    `find_header` in GRUB scans forward from the load address in 8-byte steps,
    stops after 32768 bytes, and accepts the first position whose first four
    words sum to zero.  This reproduces that search, so the check sees the same
    position the bootloader would -- including the case where the bootloader
    stops at something that is *not* a Multiboot2 header.
    """
    limit = min(limit, start + SEARCH_LIMIT)
    position = start
    while position + 16 <= limit:
        magic, architecture, length, checksum = struct.unpack_from("<IIII", image, position)
        if (magic + architecture + length + checksum) & MASK == 0:
            break
        position += HEADER_ALIGN
    else:
        raise RunnerError(
            f"kernel has no Multiboot2 header in the first {limit - start} bytes "
            "of its first loadable segment, which is as far as the bootloader looks"
        )
    if magic != MULTIBOOT2_HEADER_MAGIC:
        raise RunnerError(
            f"the first position the bootloader accepts (0x{position:x}) is not a "
            f"Multiboot2 header: magic is 0x{magic:08x}"
        )
    if architecture != MULTIBOOT2_ARCHITECTURE_I386:
        raise RunnerError(
            f"Multiboot2 header asks for architecture {architecture}, "
            "which is not the i386 entry this kernel starts with"
        )
    if length < 16 or position + length > len(image):
        raise RunnerError(f"Multiboot2 header length {length} does not fit the kernel")
    return position, length


def validate_multiboot2_header(kernel: Path) -> None:
    """Raise `RunnerError` unless `kernel` carries a header GRUB can walk."""
    image = kernel.read_bytes()
    file_offset, file_size = _segments(image)[0]
    offset, length = _header(image, file_offset, min(file_offset + file_size, len(image)))

    kinds: list[int] = []
    cursor = offset + 16
    end = offset + length
    while cursor < end:
        if cursor + 8 > end:
            raise RunnerError(
                f"Multiboot2 header is {length} bytes but a tag starts at "
                f"+{cursor - offset} with no room for its header"
            )
        tag_type, _flags, tag_size = struct.unpack_from("<HHI", image, cursor)
        if tag_size < 8:
            raise RunnerError(
                f"Multiboot2 tag type {tag_type} at +{cursor - offset} declares "
                f"size {tag_size}"
            )
        if cursor + tag_size > end:
            raise RunnerError(
                f"Multiboot2 tag type {tag_type} at +{cursor - offset} runs "
                f"{cursor + tag_size - end} bytes past the end of the header"
            )
        kinds.append(tag_type)
        if tag_type == MULTIBOOT2_END_TAG:
            break
        # This is the rule the bootloader uses and the specification does not
        # state: advance to the next 8-byte boundary, not to `cursor + size`.
        cursor = (cursor + tag_size + TAG_ALIGN - 1) // TAG_ALIGN * TAG_ALIGN
    if not kinds:
        raise RunnerError(
            f"Multiboot2 header declares {length} bytes, which leaves no room "
            "for even one tag after the 16-byte header"
        )
    if kinds[-1] != MULTIBOOT2_END_TAG:
        raise RunnerError(
            "Multiboot2 header has no end tag; read "
            f"{len(kinds)} tags: {kinds[:16]}{'...' if len(kinds) > 16 else ''}"
        )
    if cursor + 8 != end:
        raise RunnerError(
            f"Multiboot2 end tag at +{cursor - offset} does not end the "
            f"{length}-byte header it declares"
        )
    if MULTIBOOT2_ENTRY_ADDRESS_TAG not in kinds:
        raise RunnerError(
            "Multiboot2 header has no entry-address tag; read tags "
            f"{kinds[:16]}{'...' if len(kinds) > 16 else ''}"
        )
    # An address tag is how a Multiboot2 kernel states the physical addresses
    # the bootloader must load it at, which this kernel needs because it is
    # linked in the high half and cannot be relocated by a loader that guesses.
    if MULTIBOOT2_ADDRESS_TAG not in kinds:
        raise RunnerError(
            "Multiboot2 header has no address tag; read tags "
            f"{kinds[:16]}{'...' if len(kinds) > 16 else ''}"
        )


def validate_thekernel_multiboot2_header(kernel: Path) -> None:
    """Check the kernel payload before an expensive boot gets to try it."""
    try:
        validate_multiboot2_header(kernel)
    except RunnerError:
        raise
    except OSError as error:
        raise RunnerError(f"cannot read kernel {kernel}: {error}") from error
