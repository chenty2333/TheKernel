#!/usr/bin/env python3

import struct
import sys
from pathlib import Path


ADDR_DELTA = 0xFFFF800000000000
ENTRY_ADDR = 0x200040


def adjust_addr(value: int) -> int:
    return value - ADDR_DELTA if value >= ADDR_DELTA else value


def main() -> int:
    if len(sys.argv) != 3:
        print(f"usage: {sys.argv[0]} <input-elf> <output-elf>", file=sys.stderr)
        return 2

    src = Path(sys.argv[1]).resolve()
    dst = Path(sys.argv[2]).resolve()
    data = bytearray(src.read_bytes())

    if data[:4] != b"\x7fELF" or data[4] != 2 or data[5] != 1:
        raise ValueError("expected ELF64 little-endian input")

    struct.pack_into("<Q", data, 24, ENTRY_ADDR)

    phoff = struct.unpack_from("<Q", data, 32)[0]
    shoff = struct.unpack_from("<Q", data, 40)[0]
    phentsize = struct.unpack_from("<H", data, 54)[0]
    phnum = struct.unpack_from("<H", data, 56)[0]
    shentsize = struct.unpack_from("<H", data, 58)[0]
    shnum = struct.unpack_from("<H", data, 60)[0]

    for i in range(phnum):
        off = phoff + i * phentsize
        for field_off in (16, 24):
            value = struct.unpack_from("<Q", data, off + field_off)[0]
            struct.pack_into("<Q", data, off + field_off, adjust_addr(value))

    for i in range(shnum):
        off = shoff + i * shentsize
        value = struct.unpack_from("<Q", data, off + 16)[0]
        struct.pack_into("<Q", data, off + 16, adjust_addr(value))

    dst.parent.mkdir(parents=True, exist_ok=True)
    dst.write_bytes(data)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
