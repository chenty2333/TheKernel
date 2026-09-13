"""Unit tests for tools/hw_facts.py against synthetic capture fixtures.

These tests never need a real capture: every byte they parse is constructed
here, so the parsing contract (including the failure modes, which matter more
than the happy path for a value the kernel compiles in) is checked on any
machine.

Run from the repository root:

    python3 -m unittest discover -s tests -t .
    python3 -m unittest tests.ci.test_hw_facts
"""

from __future__ import annotations

import contextlib
import io
import json
import struct
import tarfile
import unittest
from pathlib import Path

from tests.support import test_tmpdir
from tools import hw_facts


# ---------------------------------------------------------------------------
# Fixture builders
# ---------------------------------------------------------------------------


def acpi_table(signature: bytes, body: bytes, revision: int = 1) -> bytes:
    """Build an ACPI table: the 36-byte common header (checksum 0), then body.

    The header layout is fixed by struct acpi_table_header: signature[4],
    length, revision, checksum, oem_id[6], oem_table_id[8], oem_revision,
    creator_id[4], creator_revision.  Nothing here verifies the checksum, and
    neither does hw_facts.py, so zero is fine.
    """

    assert len(signature) == 4, "an ACPI signature is exactly four bytes"
    header = (
        signature
        + struct.pack("<I", 36 + len(body))
        + bytes([revision, 0])  # checksum 0: nothing in hw_facts.py verifies it
        + b"TKRNL "  # OEM id, six bytes
        + b"THEKERNL"  # OEM table id, eight bytes
        + struct.pack("<I", 1)  # OEM revision
        + b"INTL"  # creator id
        + struct.pack("<I", 1)  # creator revision
    )
    assert len(header) == 36, len(header)
    return header + body


def mcfg_table(ranges: list[tuple[int, int, int, int]]) -> bytes:
    """An MCFG table laid out the way firmware publishes it.

    Per the ACPI/PCI Firmware specification (struct acpi_table_mcfg plus struct
    acpi_mcfg_allocation in include/acpi/actbl2.h): acpi_table() supplies the
    36-byte header plus the 8 reserved bytes, the base address therefore lands at
    the real offset 44, and each allocation is 16 bytes (address, segment, start
    bus, end bus, reserved).  A one-entry table is exactly 60 bytes, which is
    what a real /sys/firmware/acpi/tables/MCFG reports.

    ``ranges`` items are (base, segment, first_bus, last_bus).
    """

    body = bytes(8)  # reserved; allocations begin at the real offset 44
    for base, segment, first_bus, last_bus in ranges:
        body += struct.pack("<Q", base)  # base address, 8 bytes
        body += struct.pack("<H", segment)  # PCI segment group, 2 bytes
        body += bytes([first_bus, last_bus])  # start / end bus, 1 byte each
        body += bytes(4)  # reserved
    return acpi_table(b"MCFG", body)


def dmar_table(flags: int = 0x07) -> bytes:
    """A DMAR table: a 12-byte DRHD unit follows the 36-byte header.

    ACPI puts Host Address Base (8 bytes) then Segment (2 bytes) at offset 36,
    which lands the flags byte at offset 37 where hw_facts.py reads it.
    """

    body = struct.pack("<QH", 0xFED90000, 0)  # host address base, segment
    body += bytes([flags, 0, 0, 0])  # flags, reserved[3]
    body += struct.pack("<HH", 0, 16) + bytes(12)  # one DRHD unit: type 0, len 16
    return acpi_table(b"DMAR", body)


def hpet_table(base: int = 0xFED00000, period_fs: int = 14_318_180) -> bytes:
    """An ACPI HPET table: base address at offset 44, period at offset 52."""

    body = bytes(8)  # hardware revision / comparator count / capabilities
    body += struct.pack("<Q", base)
    body += struct.pack("<I", period_fs)
    return acpi_table(b"HPET", body)


def madt_table(local_apic: int = 0xFEE00000) -> bytes:
    """A small MADT: local APIC address, flags, one IOAPIC, two x2APICs."""

    body = struct.pack("<II", local_apic, 1)
    # IOAPIC entry: type 1, length 12, id, reserved, address, gsi base.
    body += bytes([1, 12, 0, 0]) + struct.pack("<II", 0xFEC00000, 0)
    # x2APIC entry (ACPI 6.5): type 9, length 16, reserved[2], x2APIC id, flags,
    # then the ACPI-id (UID) word that makes the entry 16 bytes rather than 12.
    for apic_id in (0x20, 0x21):
        body += bytes([9, 16, 0, 0]) + struct.pack("<III", apic_id, 0, apic_id)
    return acpi_table(b"APIC", body)


def edid_block(
    h_active: int = 1920,
    v_active: int = 1080,
    pixel_clock_10khz: int = 14850,
    h_total: int = 2200,
    v_total: int = 1125,
) -> bytes:
    """A 128-byte EDID base block whose first descriptor is a preferred timing.

    Totals are given rather than blanking intervals so that the encoded refresh
    rate (pixel clock / (h_total * v_total)) can be stated directly in a test.
    """

    h_blank = h_total - h_active
    v_blank = v_total - v_active
    blob = bytearray(128)
    blob[0:8] = b"\x00\xff\xff\xff\xff\xff\xff\x00"
    blob[8:10] = b"\x04\x1c"  # manufacturer "ABC"
    blob[10:12] = struct.pack("<H", 0x1234)
    blob[16] = 1  # manufacture week
    blob[17] = 34  # manufacture year 2024 (1990 + 34)
    blob[18] = 1
    blob[19] = 4
    blob[20] = 0xA5
    blob[21] = 0x34
    blob[22] = 0x21
    blob[23] = 0x78
    dtd = bytearray(18)
    dtd[0:2] = struct.pack("<H", pixel_clock_10khz)
    dtd[2] = h_active & 0xFF
    dtd[3] = h_blank & 0xFF
    dtd[4] = ((h_active >> 8) & 0x0F) << 4 | ((h_blank >> 8) & 0x0F)
    dtd[5] = v_active & 0xFF
    dtd[6] = v_blank & 0xFF
    dtd[7] = ((v_active >> 8) & 0x0F) << 4 | ((v_blank >> 8) & 0x0F)
    dtd[8] = 30  # h front porch
    dtd[9] = 8  # h sync pulse
    dtd[10] = 0x60
    dtd[11] = 0x30  # v front porch / sync pulse packed
    dtd[12] = 40
    dtd[13] = 60
    dtd[14] = 20
    dtd[15] = 20
    dtd[16] = 30
    dtd[17] = 30
    blob[54:72] = dtd
    # A display descriptor is exactly 18 bytes: a 5-byte header and 13 bytes of
    # text (the 13th is a newline).  Getting this wrong silently shifts every
    # following byte, so the length assertion below is not optional.
    blob[72:90] = bytes([0, 0, 0, 0xFC, 0]) + b"SYNTHETIC-13\n"
    blob[126] = 0  # one extension block count is deliberately zero
    assert len(blob) == 128, f"EDID base block must be 128 bytes, got {len(blob)}"
    return bytes(blob)


def hexdump_C(data: bytes, label: str = "fixture") -> str:
    """Format bytes exactly like `hexdump -Cv`, the capture's preferred tool."""

    lines = [f"# hexdump of {label}", f"# bytes: {len(data)}"]
    for offset in range(0, len(data), 16):
        chunk = data[offset : offset + 16]
        left = " ".join(f"{byte:02x}" for byte in chunk[:8])
        right = " ".join(f"{byte:02x}" for byte in chunk[8:])
        hex_part = f"{left:<23}  {right:<23}"
        text = "".join(chr(byte) if 32 <= byte < 127 else "." for byte in chunk)
        lines.append(f"{offset:08x}  {hex_part}  |{text}|")
    lines.append(f"{len(data):08x}")
    return "\n".join(lines) + "\n"


class CaptureFixture:
    """A synthetic capture tree that mirrors capture-n305.sh's layout."""

    def __init__(self, root: Path) -> None:
        self.root = root

    def write(self, relative: str, text: str) -> Path:
        path = self.root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")
        return path

    def write_hexdump(self, relative: str, data: bytes) -> Path:
        return self.write(relative, hexdump_C(data, relative))

    def minimal(self) -> "CaptureFixture":
        """A capture of a machine that looks like the N305 target."""

        self.write("SUMMARY.txt", MINIMAL_SUMMARY)
        self.write("MANIFEST.txt", "# size_bytes\tpath\n")
        self.write("meta/uname.txt", "Linux n305 6.11.0-generic #1 SMP x86_64 GNU/Linux\n")
        self.write_hexdump("acpi/tables/MCFG.hex", mcfg_table([(0xE0000000, 0, 0, 0xFF)]))
        self.write_hexdump("acpi/tables/DMAR.hex", dmar_table(0x07))
        self.write_hexdump("acpi/tables/APIC.hex", madt_table())
        self.write_hexdump("acpi/tables/HPET.hex", hpet_table())
        self.write(
            "acpi/decoded_tables.txt",
            "MCFG: signature='MCFG' bytes=60\n"
            "    ECAM base=0x00000000e0000000\n"
            "    allocation: segment=0 bus=00-ff base=0x00000000e0000000\n"
            "APIC: signature='APIC' bytes=88\n"
            "    local_apic_address=0xfee00000\n"
            "HPET: signature='HPET' bytes=56\n"
            "    HPET base address=0x00000000fed00000\n",
        )
        self.write(
            "cpu/lscpu.txt",
            "Architecture:            x86_64\n"
            "CPU(s):                  8\n"
            "On-line CPU(s) list:     0-7\n"
            "Vendor ID:               GenuineIntel\n"
            "Model name:              Intel(R) Core(TM) i3-N305\n"
            "Thread(s) per core:      1\n"
            "Core(s) per socket:      8\n"
            "Socket(s):               1\n",
        )
        self.write(
            "cpu/proc_cpuinfo.txt",
            "".join(f"processor\t: {index}\nmodel name\t: Intel(R) Core(TM) i3-N305\n" for index in range(8)),
        )
        self.write("cpu/topology/count.txt", "logical CPUs found in sysfs: 8\n")
        self.write(
            "cpu/flags_summary.txt",
            "present  constant_tsc\npresent  nonstop_tsc\npresent  tsc_deadline_timer\n"
            "present  x2apic\npresent  apic\nABSENT   arat\npresent  hpet\n",
        )
        self.write(
            "pci/lspci_nn.txt",
            "00:00.0 Host bridge [0600]: Intel Corporation Device [8086:4610]\n"
            "00:02.0 VGA compatible controller [0300]: Intel Corporation Alder Lake-N "
            "[UHD Graphics] [8086:46d0] (rev 0c)\n"
            "00:14.0 USB controller [0c03]: Intel Corporation Device [8086:54ed]\n",
        )
        self.write("gpu/driver.txt", "/sys/bus/pci/drivers/i915\n")
        self.write_hexdump("gpu/drm/card0-eDP-1/edid.hex", edid_block())
        self.write("gpu/drm/card0-eDP-1/status.txt", "connected\n")
        self.write("gpu/drm/card0-eDP-1/modes.txt", "1920x1080\n1280x720\n")
        self.write_hexdump("gpu/config_space.hex", bytes(64))
        self.write(
            "iommu/sys_class_iommu.txt",
            "total 0\n"
            "lrwxrwxrwx. 1 root root 0 Sep 13 15:12 dmar0 -> ../../devices/virtual/iommu/dmar0\n"
            "lrwxrwxrwx. 1 root root 0 Sep 13 15:12 dmar1 -> ../../devices/virtual/iommu/dmar1\n",
        )
        self.write("iommu/names.txt", "# names of the IOMMU devices\ndmar0\ndmar1\n")
        self.write(
            "firmware/dmidecode_bios.txt",
            "BIOS Information\n\tVendor: American Megatrends International, LLC.\n"
            "\tVersion: 5.27\n\tRelease Date: 03/14/2024\n",
        )
        self.write(
            "dmesg/dmesg.txt",
            "[    0.000000] BIOS-e820: [mem 0x0000000000000000-0x000000000009efff] usable\n"
            "[    0.000000] BIOS-e820: [mem 0x00000000e0000000-0x00000000efffffff] reserved\n"
            "[    0.012000] DMAR: Intel(R) Virtualization Technology for Directed I/O\n"
            "[    0.013000] DMAR-IR: Enabled IRQ remapping in x2apic mode\n"
            "[    0.021000] hpet0: at MMIO 0xfed00000, IRQs 2, 8, 0\n"
            "[    0.030000] tsc: Refined TSC clocksource calibration: 1996.800 MHz\n",
        )
        self.write(
            "memory/e820.txt",
            "# dmesg lines matching: BIOS-e820\n"
            "[BIOS-e820] [    0.000000] BIOS-e820: [mem 0x0000000000000000-0x000000000009efff] usable\n"
            "[BIOS-e820] [    0.000000] BIOS-e820: [mem 0x00000000e0000000-0x00000000efffffff] reserved\n",
        )
        self.write("uefi/boot_mode.txt", "UEFI (the kernel found /sys/firmware/efi)\n")
        self.write("uefi/fw_vendor.txt", "American Megatrends International, LLC.\n")
        self.write("uefi/efivars/SecureBoot.hex", hexdump_C(bytes([7, 0, 0, 0, 0])))
        return self


MINIMAL_SUMMARY = """TheKernel hardware fact summary
captured by scripts/hw-facts/capture-n305.sh at 20260101T000000Z

--- boot mode ---
UEFI: yes (the kernel found /sys/firmware/efi)
firmware vendor: American Megatrends International, LLC.
Secure Boot: disabled

--- connected displays ---
  card0-eDP-1: status=connected enabled=enabled dpms=On
    modes: 1920x1080 1280x720
    preferred: 1920x1080 @ 60.00 Hz (preferred timing)
"""


def fixture(test_case: unittest.TestCase) -> tuple[CaptureFixture, contextlib.ExitStack]:
    """A CaptureFixture inside a temporary directory removed at test end."""

    stack = contextlib.ExitStack()
    test_case.addCleanup(stack.close)
    directory = stack.enter_context(test_tmpdir())
    return CaptureFixture(Path(directory)), stack


# ---------------------------------------------------------------------------
# Hexdump decoding
# ---------------------------------------------------------------------------


class HexdumpTests(unittest.TestCase):
    def test_hexdump_C_is_decoded(self) -> None:
        data = bytes(range(64))
        self.assertEqual(hw_facts.hexdump_bytes(hexdump_C(data)), data)

    def test_xxd_g1_is_decoded(self) -> None:
        data = bytes(range(48))
        lines = []
        for offset in range(0, len(data), 16):
            chunk = data[offset : offset + 16]
            left = " ".join(f"{byte:02x}" for byte in chunk[:8])
            right = " ".join(f"{byte:02x}" for byte in chunk[8:])
            text = "".join(chr(byte) if 32 <= byte < 127 else "." for byte in chunk)
            lines.append(f"{offset:08x}: {left} {right}  {text}")
        self.assertEqual(hw_facts.hexdump_bytes("\n".join(lines)), data)

    def test_od_an_tx1_is_decoded(self) -> None:
        data = bytes(range(32))
        lines = []
        for offset in range(0, len(data), 16):
            lines.append(" ".join(f"{byte:02x}" for byte in data[offset : offset + 16]))
        self.assertEqual(hw_facts.hexdump_bytes("\n".join(lines)), data)

    def test_ascii_column_never_leaks_into_the_bytes(self) -> None:
        # "|MCFG....|" contains the letters M, C, F, G: a naive parser that
        # scanned the whole line for hex pairs would take "CF" out of the ASCII
        # column and shift every following byte.
        data = b"MCFG" + bytes(28)
        self.assertEqual(hw_facts.hexdump_bytes(hexdump_C(data)), data)

    def test_an_unavailable_marker_is_reported_not_decoded(self) -> None:
        text = "# hexdump of /sys/firmware/acpi/tables/MCFG\nUNAVAILABLE: not present or not readable\n"
        self.assertEqual(hw_facts.hexdump_bytes(text), b"")
        self.assertEqual(
            hw_facts._hexdump_is_missing(text), "not present or not readable"
        )


# ---------------------------------------------------------------------------
# MCFG / ECAM: the value the kernel currently hardcodes
# ---------------------------------------------------------------------------


class McfgTests(unittest.TestCase):
    def parse(self, blob: bytes):
        table = hw_facts.parse_acpi_table(blob, "fixture")
        return hw_facts.parse_mcfg(table)

    def test_single_allocation_is_decoded(self) -> None:
        base, ranges = self.parse(mcfg_table([(0xE0000000, 0, 0, 0xFF)]))
        self.assertEqual(base, 0xE0000000)
        self.assertEqual(len(ranges), 1)
        self.assertEqual(ranges[0].segment, 0)
        self.assertEqual(ranges[0].first_bus, 0)
        self.assertEqual(ranges[0].last_bus, 0xFF)
        self.assertEqual(ranges[0].base, 0xE0000000)
        self.assertEqual(ranges[0].bus_count, 256)

    def test_several_allocations_are_all_reported(self) -> None:
        base, ranges = self.parse(
            mcfg_table([(0xB0000000, 0, 0, 0x7F), (0xC0000000, 0, 0x80, 0xFF)])
        )
        self.assertEqual(base, 0xB0000000)
        self.assertEqual([item.render() for item in ranges], [
            "segment 0 bus 00-7f base=0x00000000b0000000 (128 buses)",
            "segment 0 bus 80-ff base=0x00000000c0000000 (128 buses)",
        ])

    def test_a_non_mcfg_signature_is_refused(self) -> None:
        with self.assertRaises(ValueError) as caught:
            self.parse(dmar_table())
        self.assertIn("MCFG", str(caught.exception))

    def test_a_zero_base_is_refused(self) -> None:
        # An unprogrammed ECAM base must never become a compiled-in address.
        with self.assertRaises(ValueError) as caught:
            self.parse(mcfg_table([(0, 0, 0, 0xFF)]))
        self.assertIn("not a usable address", str(caught.exception))

    def test_an_all_ones_base_is_refused(self) -> None:
        with self.assertRaises(ValueError):
            self.parse(mcfg_table([(0xFFFFFFFFFFFFFFFF, 0, 0, 0xFF)]))

    def test_an_inverted_bus_range_is_refused(self) -> None:
        with self.assertRaises(ValueError) as caught:
            self.parse(mcfg_table([(0xE0000000, 0, 0xFF, 0x00)]))
        self.assertIn("inverted bus range", str(caught.exception))

    def test_a_table_with_no_allocations_is_refused(self) -> None:
        # A base address with an empty allocation list names no bus range, so it
        # cannot be used to build an ECAM walk.  The appended dword is the
        # reserved field that closes a real MCFG table, which makes this a
        # structurally valid table with nothing in it rather than a stub.
        # Both guards live on this path: a table that stops before the base
        # address is too short, and a table of zeroes has no usable base *and* no
        # allocation list.  Neither may be decoded into a compiled-in address.
        too_short = acpi_table(b"MCFG", struct.pack("<Q", 0xE0000000) + bytes(4))
        self.assertEqual(len(too_short), 48, "36-byte header, base address, reserved dword")
        with self.assertRaises(ValueError) as caught:
            self.parse(too_short)
        self.assertIn("needs at least 52 bytes", str(caught.exception))

        nothing_in_it = acpi_table(b"MCFG", bytes(16))
        with self.assertRaises(ValueError) as caught:
            self.parse(nothing_in_it)
        self.assertIn("not a usable address", str(caught.exception))

    def test_a_truncated_table_is_refused_rather_than_partially_decoded(self) -> None:
        full = mcfg_table([(0xE0000000, 0, 0, 0xFF), (0xC0000000, 0, 0, 0xFF)])
        truncated = full[:60]  # the header still declares both entries
        with self.assertRaises(ValueError) as caught:
            self.parse(truncated)
        self.assertIn("truncated", str(caught.exception))

    def test_a_table_shorter_than_the_base_field_is_refused(self) -> None:
        with self.assertRaises(ValueError):
            self.parse(acpi_table(b"MCFG", b"\x00" * 4))

    def test_parse_acpi_table_rejects_a_stub(self) -> None:
        with self.assertRaises(ValueError):
            hw_facts.parse_acpi_table(b"MCFG", "fixture")


# ---------------------------------------------------------------------------
# EDID
# ---------------------------------------------------------------------------


class EdidTests(unittest.TestCase):
    def test_preferred_timing_is_the_first_detailed_descriptor(self) -> None:
        self.assertEqual(
            hw_facts.parse_edid_preferred_mode(edid_block(1920, 1080, 14850, 2200, 1125)),
            "1920x1080 @ 60.00 Hz",
        )

    def test_high_resolution_uses_the_upper_bits(self) -> None:
        # 3840 needs the upper nibble of DTD byte 4 (bits 7-4 are the *active*
        # count's high bits, 3-0 are the blanking count's), and 2160 needs the
        # same in byte 7.  CEA-861 4K@60: 4400x2250 totals at a 594 MHz pixel
        # clock.  A decoder that dropped either high nibble would read this as a
        # 0x0 resolution; one that swapped the nibbles would read the totals as
        # 4464x2325 and report 59.05 Hz.
        mode = hw_facts.parse_edid_preferred_mode(edid_block(3840, 2160, 59400, 4400, 2250))
        self.assertEqual(mode, "3840x2160 @ 60.00 Hz")

    def test_the_fixture_round_trips_through_the_decoder(self) -> None:
        # Guards the fixture itself: blanking must be positive, or the encoder
        # silently packs wrapped bytes and a test would be measuring nonsense.
        for h_active, v_active, h_total, v_total in (
            (1920, 1080, 2200, 1125),
            (3840, 2160, 4400, 2250),
            (1366, 768, 1792, 798),
        ):
            with self.subTest(mode=f"{h_active}x{v_active}"):
                self.assertTrue(h_total > h_active and v_total > v_active)
                mode = hw_facts.parse_edid_preferred_mode(
                    edid_block(h_active, v_active, 14850, h_total, v_total)
                )
                self.assertTrue(mode.startswith(f"{h_active}x{v_active} @ "), mode)

    def test_a_bad_header_magic_is_refused(self) -> None:
        blob = bytearray(edid_block())
        blob[0] = 0x11
        with self.assertRaises(ValueError) as caught:
            hw_facts.parse_edid_preferred_mode(bytes(blob))
        self.assertIn("magic", str(caught.exception))

    def test_a_short_block_is_refused(self) -> None:
        with self.assertRaises(ValueError) as caught:
            hw_facts.parse_edid_preferred_mode(edid_block()[:64])
        self.assertIn("128", str(caught.exception))

    def test_a_monitor_descriptor_in_slot_one_is_refused(self) -> None:
        # A zero pixel clock means descriptor 1 is a range limit, not a timing.
        with self.assertRaises(ValueError) as caught:
            hw_facts.parse_edid_preferred_mode(edid_block(pixel_clock_10khz=0))
        self.assertIn("pixel clock", str(caught.exception))


# ---------------------------------------------------------------------------
# MADT
# ---------------------------------------------------------------------------


class MadtTests(unittest.TestCase):
    def test_ioapic_and_x2apic_entries_are_summarised(self) -> None:
        table = hw_facts.parse_acpi_table(madt_table(), "fixture")
        decoded = hw_facts.decode_madt(table)
        self.assertEqual(decoded["local_apic_address"], 0xFEE00000)
        self.assertTrue(decoded["pcat_compatibility"])
        self.assertEqual(decoded["ioapics"], [{"id": 0, "address": 0xFEC00000, "gsi_base": 0}])
        self.assertEqual(decoded["x2apic_ids"], [0x20, 0x21])
        self.assertEqual(decoded["entry_counts"], {"1": 1, "9": 2})

    def test_a_non_madt_signature_is_refused(self) -> None:
        table = hw_facts.parse_acpi_table(mcfg_table([(0xE0000000, 0, 0, 0xFF)]), "fixture")
        with self.assertRaises(ValueError):
            hw_facts.decode_madt(table)

    def test_an_entry_that_overruns_the_table_is_refused(self) -> None:
        blob = acpi_table(b"APIC", struct.pack("<II", 0xFEE00000, 1) + bytes([9, 200, 0, 0]))
        table = hw_facts.parse_acpi_table(blob, "fixture")
        with self.assertRaises(ValueError) as caught:
            hw_facts.decode_madt(table)
        self.assertIn("table ends", str(caught.exception))


# ---------------------------------------------------------------------------
# End-to-end extraction over a synthetic capture
# ---------------------------------------------------------------------------


class FactExtractionTests(unittest.TestCase):
    def test_ecam_base_comes_from_the_raw_mcfg_bytes(self) -> None:
        capture, _ = fixture(self)
        capture.minimal()
        # The decode text deliberately lies: the raw table must win.
        capture.write(
            "acpi/decoded_tables.txt",
            "MCFG: signature='MCFG' bytes=60\n    ECAM base=0x00000000deadbeef\n",
        )
        facts = hw_facts.load_facts(capture.root)
        self.assertTrue(facts.ecam.base.available)
        self.assertEqual(facts.ecam.base.value, 0xE0000000)
        self.assertIn("acpi/tables/MCFG.hex", facts.ecam.base.sources[0])
        self.assertTrue(any("disagree" in note for note in facts.ecam.notes), facts.ecam.notes)

    def test_ecam_ranges_name_every_bus_range(self) -> None:
        capture, _ = fixture(self)
        capture.minimal()
        facts = hw_facts.load_facts(capture.root)
        self.assertIn("bus 00-ff", str(facts.ecam.ranges.value))
        self.assertIn("0x00000000e0000000", str(facts.ecam.ranges.value))

    def test_a_missing_mcfg_is_unavailable_with_a_reason(self) -> None:
        capture, _ = fixture(self)
        capture.minimal()
        (capture.root / "acpi/tables/MCFG.hex").unlink()
        facts = hw_facts.load_facts(capture.root)
        self.assertFalse(facts.ecam.base.available)
        self.assertFalse(facts.ecam.table_present)
        self.assertIn("no MCFG.hex", facts.ecam.base.reason or "")

    def test_an_unreadable_mcfg_is_not_reported_as_absent(self) -> None:
        capture, _ = fixture(self)
        capture.minimal()
        capture.write(
            "acpi/tables/MCFG.hex",
            "# hexdump of /sys/firmware/acpi/tables/MCFG\nUNAVAILABLE: not present or not readable\n",
        )
        facts = hw_facts.load_facts(capture.root)
        self.assertFalse(facts.ecam.base.available)
        self.assertFalse(facts.ecam.table_present)
        self.assertIn("not readable", facts.ecam.base.reason or "")

    def test_cpu_count_agrees_across_three_sources(self) -> None:
        capture, _ = fixture(self)
        capture.minimal()
        facts = hw_facts.load_facts(capture.root)
        self.assertEqual(facts.cpu.logical_count.value, 8)
        self.assertEqual(len(facts.cpu.logical_count.sources), 3)
        self.assertTrue(facts.cpu.timer_flags["tsc_deadline_timer"])
        self.assertTrue(facts.cpu.timer_flags["x2apic"])
        self.assertFalse(facts.cpu.timer_flags["arat"])
        self.assertEqual(facts.cpu.model_name.value, "Intel(R) Core(TM) i3-N305")

    def test_disagreeing_cpu_counts_are_reported_as_contradictory(self) -> None:
        capture, _ = fixture(self)
        capture.minimal()
        capture.write("cpu/topology/count.txt", "logical CPUs found in sysfs: 6\n")
        facts = hw_facts.load_facts(capture.root)
        self.assertFalse(facts.cpu.logical_count.available)
        self.assertIsNotNone(facts.cpu.logical_count.contradiction)
        self.assertIn("lscpu.txt=8", str(facts.cpu.logical_count.value))
        self.assertIn("cpu/topology=6", str(facts.cpu.logical_count.value))

    def test_a_missing_cpu_count_is_unavailable(self) -> None:
        capture, _ = fixture(self)
        capture.minimal()
        for relative in ("cpu/lscpu.txt", "cpu/proc_cpuinfo.txt", "cpu/topology/count.txt"):
            (capture.root / relative).unlink()
        facts = hw_facts.load_facts(capture.root)
        self.assertFalse(facts.cpu.logical_count.available)
        self.assertIn("no artefact", facts.cpu.logical_count.reason or "")

    def test_vtd_enabled_is_proven_by_dmesg_and_dmar(self) -> None:
        capture, _ = fixture(self)
        capture.minimal()
        facts = hw_facts.load_facts(capture.root)
        self.assertTrue(facts.iommu.dmar_table_present.available)
        self.assertIn("present", str(facts.iommu.dmar_table_present.value))
        self.assertIn("enabled", str(facts.iommu.kernel_enabled.value))

    def test_a_dmar_table_in_passthrough_mode_is_not_reported_as_enabled(self) -> None:
        capture, _ = fixture(self)
        capture.minimal()
        capture.write_hexdump("acpi/tables/DMAR.hex", dmar_table(0x00))
        capture.write(
            "dmesg/dmesg.txt",
            "[    0.012000] DMAR: Intel(R) Virtualization Technology for Directed I/O\n"
            "[    0.012100] iommu: Default domain type: Passthrough\n",
        )
        capture.write("iommu/sys_class_iommu.txt", "UNAVAILABLE: /sys/class/iommu is absent\n")
        facts = hw_facts.load_facts(capture.root)
        self.assertTrue(facts.iommu.dmar_table_present.available)
        self.assertIn("passthrough", str(facts.iommu.kernel_enabled.value))
        self.assertTrue(any("passthrough" in item for item in facts.iommu.evidence))

    def test_no_iommu_evidence_is_unavailable(self) -> None:
        capture, _ = fixture(self)
        capture.minimal()
        (capture.root / "acpi/tables/DMAR.hex").unlink()
        capture.write("dmesg/dmesg.txt", "[    0.000000] Linux version 6.11.0\n")
        capture.write("iommu/sys_class_iommu.txt", "UNAVAILABLE: absent\n")
        capture.write("iommu/names.txt", "# names of the IOMMU devices\n")
        facts = hw_facts.load_facts(capture.root)
        self.assertFalse(facts.iommu.dmar_table_present.available)
        self.assertFalse(facts.iommu.kernel_enabled.available)
        self.assertIn("no evidence", facts.iommu.kernel_enabled.reason or "")

    def test_hpet_base_prefers_the_acpi_table(self) -> None:
        capture, _ = fixture(self)
        capture.minimal()
        facts = hw_facts.load_facts(capture.root)
        self.assertEqual(facts.hpet.base.value, 0xFED00000)
        self.assertEqual(facts.hpet.base.render().split()[0], "0x00000000fed00000")
        self.assertIn("HPET.hex", facts.hpet.base.sources[0])
        self.assertEqual(facts.hpet.period_fs.value, 14_318_180)

    def test_hpet_base_falls_back_to_dmesg_when_the_table_is_gone(self) -> None:
        capture, _ = fixture(self)
        capture.minimal()
        (capture.root / "acpi/tables/HPET.hex").unlink()
        facts = hw_facts.load_facts(capture.root)
        self.assertEqual(facts.hpet.base.value, 0xFED00000)
        self.assertEqual(facts.hpet.base.sources, ("dmesg",))

    def test_hpet_is_unavailable_with_no_source_at_all(self) -> None:
        capture, _ = fixture(self)
        capture.minimal()
        (capture.root / "acpi/tables/HPET.hex").unlink()
        capture.write("dmesg/dmesg.txt", "[    0.000000] Linux version 6.11.0\n")
        facts = hw_facts.load_facts(capture.root)
        self.assertFalse(facts.hpet.base.available)
        self.assertIn("no HPET base address found", facts.hpet.base.reason or "")

    def test_igpu_identity_comes_from_lspci(self) -> None:
        capture, _ = fixture(self)
        capture.minimal()
        facts = hw_facts.load_facts(capture.root)
        self.assertEqual(facts.igpu.bdf.value, "00:02.0")
        self.assertEqual(facts.igpu.revision.value, 0x0C)
        self.assertEqual(facts.igpu.kernel_driver.value, "/sys/bus/pci/drivers/i915")

    def test_igpu_falls_back_to_the_sysfs_device_tree(self) -> None:
        capture, _ = fixture(self)
        capture.minimal()
        (capture.root / "pci/lspci_nn.txt").unlink()
        capture.write("pci/devices/0000_00_02.0/vendor.txt", "0x8086\n")
        capture.write("pci/devices/0000_00_02.0/device.txt", "0x46d0\n")
        capture.write("pci/devices/0000_00_02.0/bdf.txt", "0000:00:02.0\n")
        capture.write("pci/devices/0000_00_02.0/revision.txt", "12\n")
        capture.write("pci/devices/0000_00_00.0/vendor.txt", "0x8086\n")
        capture.write("pci/devices/0000_00_00.0/device.txt", "0x4610\n")
        facts = hw_facts.load_facts(capture.root)
        self.assertEqual(facts.igpu.bdf.value, "0000:00:02.0")
        self.assertEqual(facts.igpu.revision.value, 0x12)
        self.assertIn("pci/devices/0000_00_02.0", facts.igpu.bdf.sources[0])

    def test_a_machine_without_the_igpu_says_so(self) -> None:
        capture, _ = fixture(self)
        capture.minimal()
        capture.write("pci/lspci_nn.txt", "00:00.0 Host bridge [0600]: Intel Corporation Device [8086:4610]\n")
        facts = hw_facts.load_facts(capture.root)
        self.assertFalse(facts.igpu.bdf.available)
        self.assertIn("no 8086:46d0", facts.igpu.bdf.reason or "")
        self.assertFalse(facts.igpu.revision.available)

    def test_boot_mode_and_secure_boot(self) -> None:
        capture, _ = fixture(self)
        capture.minimal()
        facts = hw_facts.load_facts(capture.root)
        self.assertIn("UEFI", str(facts.firmware.boot_mode.value))
        self.assertEqual(facts.firmware.secure_boot.value, "disabled")
        self.assertIn("American Megatrends", str(facts.firmware.firmware_vendor.value))

    def test_secure_boot_enabled_is_read_from_the_efivar_bytes(self) -> None:
        capture, _ = fixture(self)
        capture.minimal()
        capture.write("uefi/efivars/SecureBoot.hex", hexdump_C(bytes([7, 0, 0, 0, 1])))
        (capture.root / "SUMMARY.txt").write_text("no secure boot line here\n", encoding="utf-8")
        facts = hw_facts.load_facts(capture.root)
        self.assertEqual(facts.firmware.secure_boot.value, "enabled")

    def test_legacy_boot_is_reported_as_legacy(self) -> None:
        capture, _ = fixture(self)
        capture.minimal()
        capture.write(
            "uefi/boot_mode.txt",
            "legacy BIOS/CSM (no /sys/firmware/efi directory: the kernel found no EFI system table)\n",
        )
        facts = hw_facts.load_facts(capture.root)
        self.assertIn("legacy", str(facts.firmware.boot_mode.value))

    def test_monitor_preferred_mode_comes_from_the_edid_bytes(self) -> None:
        capture, _ = fixture(self)
        capture.minimal()
        facts = hw_facts.load_facts(capture.root)
        self.assertEqual(facts.display.preferred_mode.value, "1920x1080 @ 60.00 Hz")
        self.assertIn("edid.hex", facts.display.preferred_mode.sources[0])
        self.assertEqual(facts.display.connectors["card0-eDP-1"], "status=connected enabled=enabled dpms=On")

    def test_a_corrupt_edid_falls_back_to_the_advertised_mode_list(self) -> None:
        capture, _ = fixture(self)
        capture.minimal()
        capture.write_hexdump("gpu/drm/card0-eDP-1/edid.hex", bytes(128))  # zeroed: bad magic
        facts = hw_facts.load_facts(capture.root)
        self.assertEqual(facts.display.preferred_mode.value, "1920x1080")
        self.assertIn("first advertised mode", facts.display.preferred_mode.sources[0])

    def test_no_display_at_all_is_unavailable(self) -> None:
        capture, _ = fixture(self)
        capture.minimal()
        (capture.root / "gpu/drm/card0-eDP-1/edid.hex").unlink()
        (capture.root / "gpu/drm/card0-eDP-1/modes.txt").unlink()
        (capture.root / "SUMMARY.txt").write_text("no displays\n", encoding="utf-8")
        facts = hw_facts.load_facts(capture.root)
        self.assertFalse(facts.display.preferred_mode.available)
        self.assertIn("no EDID artefact", facts.display.preferred_mode.reason or "")

    def test_e820_regions_are_listed_and_checked_against_the_ecam_base(self) -> None:
        capture, _ = fixture(self)
        capture.minimal()
        facts = hw_facts.load_facts(capture.root)
        self.assertEqual(facts.memory.e820.value, "2 e820 regions")
        self.assertIn("falls inside", str(facts.memory.ecam_inside_reserved.value))

    def test_pci_device_count_and_igpu_bdf(self) -> None:
        capture, _ = fixture(self)
        capture.minimal()
        facts = hw_facts.load_facts(capture.root)
        self.assertEqual(facts.pci.device_count.value, 3)
        self.assertEqual(facts.pci.igpu_bdf_from_lspci.value, "00:02.0")

    def test_acpi_inventory_reports_captured_tables(self) -> None:
        capture, _ = fixture(self)
        capture.minimal()
        facts = hw_facts.load_facts(capture.root)
        self.assertEqual(
            sorted(facts.acpi_tables), ["APIC", "DMAR", "HPET", "MCFG"]
        )
        self.assertEqual(facts.acpi_tables["MCFG"], 60)

    def test_an_empty_capture_directory_yields_unavailable_everywhere(self) -> None:
        capture, _ = fixture(self)
        facts = hw_facts.load_facts(capture.root)
        self.assertFalse(facts.ecam.base.available)
        self.assertFalse(facts.cpu.logical_count.available)
        self.assertFalse(facts.hpet.base.available)
        self.assertFalse(facts.igpu.bdf.available)
        self.assertFalse(facts.firmware.boot_mode.available)
        self.assertFalse(facts.display.preferred_mode.available)
        for fact in (
            facts.ecam.base,
            facts.cpu.logical_count,
            facts.hpet.base,
            facts.igpu.bdf,
            facts.firmware.boot_mode,
            facts.display.preferred_mode,
        ):
            self.assertTrue(fact.reason, "an unavailable fact must always carry a reason")


    def test_an_unrelated_vga_device_is_reported_with_a_note(self) -> None:
        # A capture from a machine that is not the N305 still names its VGA
        # device in gpu/igpu_bdf.txt; reporting that (plus the mismatch) is far
        # more useful than claiming the machine has no GPU.
        capture, _ = fixture(self)
        capture.minimal()
        capture.write("pci/lspci_nn.txt", "00:00.0 Host bridge [0600]: Intel Corporation Device [8086:4610]\n")
        capture.write(
            "gpu/igpu_bdf.txt",
            "0000:00:02.0 (NOT 8086:46d0: this is the machine's VGA-class device, "
            "id 0x8086:0xb080)\n",
        )
        facts = hw_facts.load_facts(capture.root)
        self.assertEqual(facts.igpu.bdf.value, "0000:00:02.0")
        self.assertTrue(any("not 8086:46d0" in note for note in facts.igpu.notes), facts.igpu.notes)

    def test_a_pointer_valued_fw_vendor_is_not_reported_as_a_vendor(self) -> None:
        # Some kernels expose the raw fw_vendor blob, which reads as a hex
        # pointer.  That is not a vendor name and must not be passed through.
        capture, _ = fixture(self)
        capture.minimal()
        capture.write("uefi/fw_vendor.txt", "0x66de8b98")
        facts = hw_facts.load_facts(capture.root)
        self.assertIn("American Megatrends", str(facts.firmware.firmware_vendor.value))
        self.assertIn("fw_vendor unusable", str(facts.firmware.firmware_vendor.value))
        self.assertIn("dmidecode_bios.txt", facts.firmware.firmware_vendor.sources[0])

    def test_an_unusable_fw_vendor_without_dmidecode_is_unavailable(self) -> None:
        capture, _ = fixture(self)
        capture.minimal()
        capture.write("uefi/fw_vendor.txt", "0x66de8b98")
        (capture.root / "firmware/dmidecode_bios.txt").unlink()
        facts = hw_facts.load_facts(capture.root)
        self.assertFalse(facts.firmware.firmware_vendor.available)
        self.assertIn("not a decoded vendor string", facts.firmware.firmware_vendor.reason or "")

    def test_iommu_entries_are_read_from_the_name_list(self) -> None:
        capture, _ = fixture(self)
        capture.minimal()
        facts = hw_facts.load_facts(capture.root)
        self.assertTrue(
            any("2 entries" in item and "dmar0" in item for item in facts.iommu.evidence),
            facts.iommu.evidence,
        )

    def test_iommu_entries_fall_back_to_parsing_an_ls_listing(self) -> None:
        # The column count of `ls -l` varies with coreutils, so only the symlink
        # target may be trusted; this listing carries the modern date column.
        capture, _ = fixture(self)
        capture.minimal()
        (capture.root / "iommu/names.txt").unlink()
        facts = hw_facts.load_facts(capture.root)
        self.assertTrue(
            any("2 entries" in item and "dmar1" in item for item in facts.iommu.evidence),
            facts.iommu.evidence,
        )

    def test_iommu_registration_alone_counts_as_enabled(self) -> None:
        # With no dmesg evidence at all, a registered IOMMU device is still
        # proof that DMA remapping is on, and the fact must say which evidence
        # it rests on.
        capture, _ = fixture(self)
        capture.minimal()
        capture.write("dmesg/dmesg.txt", "[    0.000000] Linux version 6.11.0\n")
        facts = hw_facts.load_facts(capture.root)
        self.assertIn("enabled", str(facts.iommu.kernel_enabled.value))
        self.assertTrue(
            any("sys_class_iommu" in source or "names.txt" in source
                for source in facts.iommu.kernel_enabled.sources),
            facts.iommu.kernel_enabled.sources,
        )


# ---------------------------------------------------------------------------
# Fact rendering and JSON
# ---------------------------------------------------------------------------


class FactRenderingTests(unittest.TestCase):
    def test_available_fact_names_its_source(self) -> None:
        fact = hw_facts.Fact.ok_hex(0xE0000000, "acpi/tables/MCFG.hex")
        self.assertTrue(fact.available)
        self.assertEqual(fact.render(), "0x00000000e0000000  [from acpi/tables/MCFG.hex]")
        self.assertEqual(fact.to_jsonable()["value"], 0xE0000000)
        self.assertEqual(fact.to_jsonable()["value_hex"], "0x00000000e0000000")

    def test_a_plain_integer_fact_renders_as_decimal(self) -> None:
        # Not every integer is an address: a PCI revision reads better as 12.
        self.assertEqual(hw_facts.Fact.ok(12, "lspci_nn.txt").render(), "12  [from lspci_nn.txt]")
        self.assertNotIn("value_hex", hw_facts.Fact.ok(12).to_jsonable())

    def test_unavailable_fact_renders_its_reason(self) -> None:
        self.assertEqual(
            hw_facts.Fact.unavailable("no MCFG table").render(),
            "UNAVAILABLE: no MCFG table",
        )

    def test_contradictory_fact_is_not_available(self) -> None:
        fact = hw_facts.Fact.conflicted("lscpu=8, cpuinfo=6", "sources disagree", "lscpu.txt")
        self.assertFalse(fact.available)
        self.assertIn("CONTRADICTORY", fact.render())
        self.assertEqual(fact.to_jsonable()["status"], "contradictory")

    def test_json_never_hides_an_unavailable_fact(self) -> None:
        capture, _ = fixture(self)
        facts = hw_facts.load_facts(capture.root)
        payload = hw_facts.to_jsonable(facts)
        self.assertEqual(payload["schema"], "thekernel.hw_facts.v1")
        self.assertEqual(payload["required_facts"]["ecam_base"]["status"], "unavailable")
        self.assertTrue(payload["required_facts"]["ecam_base"]["reason"])
        # A round trip must not turn "unknown" into null or an empty string.
        self.assertEqual(json.loads(json.dumps(payload)), payload)

    def test_json_reports_the_ecam_base_as_a_number_and_a_hex_string(self) -> None:
        capture, _ = fixture(self)
        capture.minimal()
        facts = hw_facts.load_facts(capture.root)
        payload = hw_facts.to_jsonable(facts)
        self.assertEqual(payload["ecam"]["ecam_base"]["value"], 0xE0000000)
        self.assertEqual(payload["ecam"]["ecam_ranges"]["status"], "ok")
        self.assertEqual(payload["required_facts"]["ecam_base"]["value"], 0xE0000000)
        self.assertTrue(payload["ecam"]["mcfg_present"])

    def test_human_rendering_names_every_required_fact(self) -> None:
        capture, _ = fixture(self)
        capture.minimal()
        rendered = hw_facts.render_human(hw_facts.load_facts(capture.root))
        for label in (
            "ecam base",
            "logical CPUs",
            "DMAR in ACPI",
            "kernel IOMMU",
            "HPET base",
            "revision",
            "boot mode",
            "preferred mode",
        ):
            self.assertIn(label, rendered)


# ---------------------------------------------------------------------------
# Input handling: directories, tarballs, and the CLI
# ---------------------------------------------------------------------------


class InputHandlingTests(unittest.TestCase):
    def make_tarball(self, capture: CaptureFixture, name: str, mode: str) -> Path:
        archive = capture.root.parent / name
        with tarfile.open(archive, mode) as tar:
            tar.add(capture.root, arcname="thekernel-hw-facts-n305")
        return archive

    def test_a_gzipped_tarball_is_read(self) -> None:
        capture, _ = fixture(self)
        capture.minimal()
        archive = self.make_tarball(capture, "capture.tar.gz", "w:gz")
        facts = hw_facts.load_facts(archive)
        self.assertEqual(facts.ecam.base.value, 0xE0000000)

    def test_an_uncompressed_tarball_is_read(self) -> None:
        capture, _ = fixture(self)
        capture.minimal()
        archive = self.make_tarball(capture, "capture.tar", "w")
        facts = hw_facts.load_facts(archive)
        self.assertEqual(facts.ecam.base.value, 0xE0000000)

    def test_a_tarball_without_a_wrapping_directory_is_read(self) -> None:
        capture, _ = fixture(self)
        capture.minimal()
        archive = capture.root.parent / "flat.tar"
        with tarfile.open(archive, "w") as tar:
            for path in sorted(capture.root.rglob("*")):
                tar.add(path, arcname=str(path.relative_to(capture.root)), recursive=False)
        facts = hw_facts.load_facts(archive)
        self.assertEqual(facts.ecam.base.value, 0xE0000000)

    def test_a_traversal_member_is_refused(self) -> None:
        capture, _ = fixture(self)
        capture.minimal()
        archive = capture.root.parent / "evil.tar"
        payload = capture.root / "meta" / "uname.txt"
        with tarfile.open(archive, "w") as tar:
            tar.add(payload, arcname="../../escaped.txt", recursive=False)
        with self.assertRaises(hw_facts.CaptureError) as caught:
            hw_facts.Capture.open(archive)
        self.assertIn("unsafe tar member", str(caught.exception))

    def test_a_missing_path_is_a_capture_error(self) -> None:
        capture, _ = fixture(self)
        with self.assertRaises(hw_facts.CaptureError):
            hw_facts.load_facts(capture.root / "does-not-exist")

    def test_a_non_archive_file_is_a_capture_error(self) -> None:
        capture, _ = fixture(self)
        bogus = capture.write("not-a-tar.bin", "this is not a tar archive at all\n")
        with self.assertRaises(hw_facts.CaptureError):
            hw_facts.load_facts(bogus)

    def test_cli_json_mode_prints_one_json_object(self) -> None:
        capture, _ = fixture(self)
        capture.minimal()
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            status = hw_facts.main([str(capture.root), "--json"])
        self.assertEqual(status, 0)
        payload = json.loads(out.getvalue())
        self.assertEqual(payload["required_facts"]["ecam_base"]["value"], 0xE0000000)

    def test_cli_require_fails_when_a_fact_is_missing(self) -> None:
        capture, _ = fixture(self)
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            status = hw_facts.main([str(capture.root), "--require", "ecam_base"])
        self.assertEqual(status, 1)
        self.assertIn("ecam_base", err.getvalue())

    def test_cli_require_rejects_an_unknown_fact_name(self) -> None:
        capture, _ = fixture(self)
        capture.minimal()
        err = io.StringIO()
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(err):
            status = hw_facts.main([str(capture.root), "--require", "nonsense"])
        self.assertEqual(status, 2)
        self.assertIn("unknown --require fact", err.getvalue())

    def test_cli_reports_a_capture_error_on_stderr(self) -> None:
        capture, _ = fixture(self)
        err = io.StringIO()
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(err):
            status = hw_facts.main([str(capture.root / "missing")])
        self.assertEqual(status, 2)
        self.assertIn("error:", err.getvalue())


if __name__ == "__main__":
    unittest.main()
