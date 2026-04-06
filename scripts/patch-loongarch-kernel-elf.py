#!/usr/bin/env python3

import shutil
import struct
import subprocess
import sys
from pathlib import Path


ADDR_DELTA = 0xFFFF800000000000
ENTRY_ADDR = 0x200040
OBJCOPY_CANDIDATES = (
    "loongarch64-linux-musl-objcopy",
    "loongarch64-linux-gnu-objcopy",
)


def find_objcopy() -> str:
    for name in OBJCOPY_CANDIDATES:
        path = shutil.which(name)
        if path:
            return path
    raise FileNotFoundError(
        "missing loongarch objcopy; tried: " + ", ".join(OBJCOPY_CANDIDATES)
    )


def main() -> int:
    if len(sys.argv) != 3:
        print(f"usage: {sys.argv[0]} <input-elf> <output-elf>", file=sys.stderr)
        return 2

    src = Path(sys.argv[1]).resolve()
    dst = Path(sys.argv[2]).resolve()
    dst.parent.mkdir(parents=True, exist_ok=True)
    dst.write_bytes(src.read_bytes())

    objcopy = find_objcopy()
    subprocess.run(
        [
            objcopy,
            f"--change-addresses=-0x{ADDR_DELTA:x}",
            str(dst),
        ],
        check=True,
    )

    data = bytearray(dst.read_bytes())
    struct.pack_into("<Q", data, 24, ENTRY_ADDR)
    dst.write_bytes(data)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
