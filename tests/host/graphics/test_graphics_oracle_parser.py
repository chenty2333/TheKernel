#!/usr/bin/env python3
"""Parser and normalizer for the standalone graphics Linux ABI probes."""
from __future__ import annotations

import unittest

PREFIX = "TK_GRAPHICS "
VOLATILE = {"errno", "size", "pitch", "read", "type", "length", "data",
            "bustype", "vendor", "product", "version", "min", "max", "fuzz",
            "flat", "resolution", "value"}

def parse(output: str) -> dict[str, dict[str, str]]:
    records: dict[str, dict[str, str]] = {}
    for line in output.splitlines():
        if not line.startswith(PREFIX):
            continue
        fields = dict(field.split("=", 1) for field in line[len(PREFIX):].split() if "=" in field)
        kind = fields.pop("kind", None)
        if kind is None or "state" not in fields:
            raise ValueError(f"malformed graphics record: {line!r}")
        if kind in records:
            raise ValueError(f"duplicate graphics record: {kind}")
        records[kind] = fields
    if not records:
        raise ValueError("no graphics records")
    return records

def normalized(output: str) -> dict[str, dict[str, str]]:
    return {
        kind: {key: value for key, value in fields.items() if key not in VOLATILE}
        for kind, fields in parse(output).items()
        if kind.endswith(".uapi")
    }

class GraphicsOracleParserTests(unittest.TestCase):
    def test_normalizes_device_specific_values_but_preserves_abi(self) -> None:
        linux = """TK_GRAPHICS kind=drm.uapi state=OK card_res=48 create_dumb=32 getresources=0xc03064a0\nTK_GRAPHICS kind=drm.dumb_lifetime state=OK size=16384 pitch=256\n"""
        guest = """TK_GRAPHICS kind=drm.uapi state=OK card_res=48 create_dumb=32 getresources=0xc03064a0\nTK_GRAPHICS kind=drm.dumb_lifetime state=OK size=4096 pitch=64\n"""
        self.assertEqual(normalized(linux), normalized(guest))

    def test_reports_abi_difference(self) -> None:
        linux = "TK_GRAPHICS kind=evdev.uapi state=OK input_event=24 version_ioctl=0x80044501\n"
        guest = "TK_GRAPHICS kind=evdev.uapi state=OK input_event=16 version_ioctl=0x80044501\n"
        self.assertNotEqual(normalized(linux), normalized(guest))

    def test_rejects_malformed_and_duplicate_records(self) -> None:
        with self.assertRaises(ValueError): parse("TK_GRAPHICS state=OK\n")
        with self.assertRaises(ValueError): parse("TK_GRAPHICS kind=x state=OK\nTK_GRAPHICS kind=x state=OK\n")

if __name__ == "__main__":
    unittest.main()
