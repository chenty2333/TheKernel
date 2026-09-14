"""Unit tests for the development-host side of a hardware-facts bundle.

The bundle is captured on the DUT, so these tests build their own bytes.  They
are written against the ACPI and EDID specifications rather than against this
repository's parser: the fixture sizes are asserted explicitly, because a
fixture that quietly disagrees with the specification makes every test that
uses it worthless.
"""

from __future__ import annotations

import io
import struct
import tarfile
import unittest
from pathlib import Path

from tests.support import load_script_module, test_tmpdir


def load_reader():
    return load_script_module("thekernel_hw_facts_bundle", "scripts/ci/hw_facts_bundle.py")


#: signature, length, revision, checksum, OEM id, OEM table id, OEM revision,
#: creator id, creator revision: 36 bytes, as ACPI defines it.
ACPI_HEADER = struct.Struct("<4sIBB6s8sI4sI")


def acpi_table(signature: bytes, body: bytes, revision: int = 1) -> bytes:
    """A complete ACPI table: 36-byte header, declared length, valid checksum."""
    length = ACPI_HEADER.size + len(body)
    header = ACPI_HEADER.pack(signature, length, revision, 0, b"TEST  ", b"TESTTB", 1, b"TEST", 1)
    table = bytearray(header + body)
    table[9] = (-sum(table)) % 256
    return bytes(table)


def mcfg_table(allocations: list[tuple[int, int, int, int]]) -> bytes:
    """An MCFG table: 36-byte header, 8 reserved bytes, then 16-byte entries."""
    body = b"\x00" * 8
    for base, segment, bus_start, bus_end in allocations:
        body += struct.pack("<QHBBI", base, segment, bus_start, bus_end, 0)
    return acpi_table(b"MCFG", body)


def edid_block() -> bytes:
    block = bytearray(128)
    block[0:8] = b"\x00\xff\xff\xff\xff\xff\xff\x00"
    block[8:10] = struct.pack(">H", (4 << 10) | (5 << 5) | 12)  # "DEL"
    block[10:12] = struct.pack("<H", 0x1234)
    block[12:16] = struct.pack("<I", 0x00000001)
    block[16], block[17] = 12, 36  # week 12 of 2026
    block[18], block[19] = 1, 4
    block[20] = 0x80  # digital input
    block[21], block[22] = 51, 29
    # Detailed timing descriptor 1: 1920x1080 at 148.5 MHz, 2200x1125 total.
    block[54:56] = struct.pack("<H", 14850)
    block[56], block[57] = 0x80, 0x18
    block[58] = (0x7 << 4) | 0x1
    block[59], block[60] = 0x38, 0x2D
    block[61] = (0x4 << 4) | 0x0
    block[62], block[63] = 88, 44
    block[64] = (4 << 4) | 5
    block[65] = 0x00  # high bits of the front porch and sync widths
    block[66:72] = b"\x00" * 6  # image size, borders and flags
    block[126] = 0  # no extension blocks
    block[127] = (-sum(block[:127])) % 256
    return bytes(block)


class McfgTests(unittest.TestCase):
    def setUp(self) -> None:
        self.reader = load_reader()

    def test_a_one_entry_table_is_sixty_bytes(self) -> None:
        table = mcfg_table([(0xE000_0000, 0, 0x00, 0xFF)])
        self.assertEqual(len(table), 60, "header 36 + reserved 8 + one 16-byte entry")
        parsed = self.reader.parse_mcfg(table)
        self.assertEqual(parsed.base, 0xE000_0000)
        self.assertEqual(parsed.segment, 0)
        self.assertEqual((parsed.bus_start, parsed.bus_end), (0x00, 0xFF))
        self.assertTrue(parsed.checksum_ok)

    def test_the_acpi_header_is_thirty_six_bytes(self) -> None:
        self.assertEqual(len(acpi_table(b"FACP", b"")), 36)
        self.assertEqual(len(mcfg_table([])), 44, "header 36 + reserved 8")

    def test_the_bus_range_is_not_read_out_of_the_base_address(self) -> None:
        # The parked work shipped a fixture with an extra 8-byte field here,
        # which made 0xe0000000 decode as "bus 00-e0".  It must decode to ff.
        parsed = self.reader.parse_mcfg(mcfg_table([(0xE000_0000, 0, 0x00, 0xFF)]))
        self.assertEqual(parsed.bus_end, 0xFF)

    def test_multiple_allocations_are_all_reported(self) -> None:
        parsed = self.reader.parse_mcfg(
            mcfg_table([(0xE000_0000, 0, 0x00, 0x7F), (0xF000_0000, 1, 0x80, 0xFF)])
        )
        self.assertEqual(len(parsed.allocations), 2)
        self.assertEqual(parsed.allocations[1].segment, 1)
        self.assertEqual(parsed.base, 0xE000_0000, "the first allocation is the headline value")

    def test_a_bad_checksum_is_reported_rather_than_hidden(self) -> None:
        table = bytearray(mcfg_table([(0xE000_0000, 0, 0x00, 0xFF)]))
        table[20] ^= 0xFF
        self.assertFalse(self.reader.parse_mcfg(bytes(table)).checksum_ok)

    def test_impossible_and_inverted_allocations_are_refused(self) -> None:
        for allocation, message in (
            ((0, 0, 0, 0xFF), "impossible base"),
            ((0xFFFF_FFFF_FFFF_FFFF, 0, 0, 0xFF), "impossible base"),
            ((0xE000_0000, 0, 0xFF, 0x00), "inverted bus range"),
        ):
            with self.subTest(allocation=allocation):
                with self.assertRaisesRegex(self.reader.BundleError, message):
                    self.reader.parse_mcfg(mcfg_table([allocation]))

    def test_a_truncated_table_is_refused(self) -> None:
        table = mcfg_table([(0xE000_0000, 0, 0x00, 0xFF)])
        with self.assertRaisesRegex(self.reader.BundleError, "declares 60 bytes"):
            self.reader.parse_mcfg(table[:52])

    def test_the_wrong_signature_is_refused(self) -> None:
        with self.assertRaisesRegex(self.reader.BundleError, "expected an MCFG table"):
            self.reader.parse_mcfg(acpi_table(b"FACP", b"\x00" * 24))


class EdidTests(unittest.TestCase):
    def setUp(self) -> None:
        self.reader = load_reader()

    def test_identity_and_preferred_timing(self) -> None:
        edid = self.reader.decode_edid(edid_block())
        self.assertEqual(edid.manufacturer, "DEL")
        self.assertEqual(edid.product, 0x1234)
        self.assertEqual((edid.week, edid.year), (12, 2026))
        self.assertTrue(edid.digital)
        self.assertEqual(edid.size_cm, (51, 29))
        self.assertTrue(edid.checksum_ok)
        self.assertEqual(edid.preferred["hactive"], 1920)
        self.assertEqual(edid.preferred["vactive"], 1080)
        self.assertEqual(edid.preferred["pixel_clock_khz"], 148500)
        self.assertEqual(edid.preferred["refresh_hz"], 60)

    def test_a_bad_header_is_refused(self) -> None:
        broken = bytearray(edid_block())
        broken[0] = 0x01
        with self.assertRaisesRegex(self.reader.BundleError, "header"):
            self.reader.decode_edid(bytes(broken))

    def test_a_short_block_is_refused(self) -> None:
        with self.assertRaisesRegex(self.reader.BundleError, "expected at least 128"):
            self.reader.decode_edid(edid_block()[:64])

    def test_no_detailed_timing_is_reported_as_none(self) -> None:
        block = bytearray(edid_block())
        block[54:56] = b"\x00\x00"
        block[56:72] = b"\x00" * 16
        block[127] = (-sum(block[:127])) % 256
        self.assertIsNone(self.reader.decode_edid(bytes(block)).preferred)

    def test_a_timing_with_no_pixels_is_a_finding_not_a_crash(self) -> None:
        # A malformed descriptor can declare a zero total extent: a real mode
        # holds at least one pixel and one line.  That is a finding about the
        # capture, and it must not become a ZeroDivisionError that abandons the
        # facts from every other file in the bundle.
        for fields, decoded in (
            ((2, 3, 4), "0+0 by 1080+45"),  # horizontal active and blanking
            ((5, 6, 7), "1920+280 by 0+0"),  # vertical active and blanking
        ):
            block = bytearray(edid_block())
            for field in fields:
                block[54 + field] = 0
            block[127] = (-sum(block[:127])) % 256
            with self.assertRaises(self.reader.BundleError) as raised:
                self.reader.decode_edid(bytes(block))
            self.assertIn(f"zero total extent: {decoded}", str(raised.exception))


class BundleTests(unittest.TestCase):
    """End-to-end reading of a synthetic bundle that looks like a real capture."""

    def setUp(self) -> None:
        self.reader = load_reader()
        self._tmp = test_tmpdir()
        self.addCleanup(self._tmp.cleanup)
        self.root = Path(self._tmp.name) / "n305-20260101T000000Z"
        self.root.mkdir(parents=True)

    def write(self, relative: str, body: bytes | str) -> None:
        path = self.root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(body.encode() if isinstance(body, str) else body)

    def populate(self, *, include_mcfg: bool = True) -> None:
        self.write("dmi/sysfs-id/sys_vendor", "Acer\n")
        self.write("dmi/sysfs-id/product_name", "Revo Mini\n")
        self.write("dmi/sysfs-id/bios_version", "V1.02\n")
        self.write("proc/cpuinfo.txt", "processor\t: 0\nmodel name\t: Intel(R) Core(TM) i3-N305\n")
        self.write("proc/cpuinfo.txt", (self.root / "proc/cpuinfo.txt").read_text() + "processor\t: 1\n")
        self.write("acpi/kernel-ecam.txt", "PCI: ECAM [mem 0xe0000000-0xefffffff] (base 0xe0000000) for domain 0000 [bus 00-ff]\n")
        self.write("proc/pci-windows.txt", "00000000-00000000 : PCI Bus 0000:00\n")
        self.write("graphics/gpu-bdf.txt", "0000:00:02.0\n")
        self.write(
            "pci/lspci-nnvvv.txt",
            "0000:00:02.0 VGA compatible controller [0300]: Intel Corporation Device [8086:46d0]\n"
            "\tKernel driver in use: i915\n\tKernel modules: i915\n",
        )
        self.write(
            "graphics/dmesg-graphics.txt",
            "i915 0000:00:02.0: [drm] Finished loading DMC firmware i915/adlp_dmc.bin (v2.20)\n",
        )
        self.write("display/edid/card0-HDMI-A-1.edid", edid_block())
        self.write("capture-status.txt", "OK\tproc/iomem.txt\t\nFAIL\tdisplay/modetest.txt\texit 1\n")
        if include_mcfg:
            self.write("acpi/tables/MCFG", mcfg_table([(0xE000_0000, 0, 0x00, 0xFF)]))

    def test_the_headline_facts_are_collected(self) -> None:
        self.populate()
        facts = self.reader.collect(self.reader.Bundle.open(self.root))
        self.assertEqual(facts["machine"]["vendor"], "Acer")
        self.assertEqual(facts["cpu"]["model"], "Intel(R) Core(TM) i3-N305")
        self.assertEqual(facts["ecam"]["mcfg"]["ecam_base"], "0xe0000000")
        self.assertEqual(facts["ecam"]["kernel"]["base"], "0xe0000000")
        self.assertEqual(facts["graphics"]["bdf"], "0000:00:02.0")
        self.assertEqual(facts["graphics"]["driver"], "i915")
        self.assertEqual(facts["displays"][0]["preferred_timing"]["vactive"], 1080)
        self.assertIn("FAIL", [key for key in facts["capture"]["counts"]])

    def test_the_report_names_the_gaps(self) -> None:
        self.populate()
        report = self.reader.render(self.reader.collect(self.reader.Bundle.open(self.root)))
        self.assertIn("MCFG and the kernel agree", report)
        self.assertIn("display/modetest.txt", report)
        self.assertIn("1920x1080@60Hz", report)

    def test_expect_ecam_turns_an_assumption_into_a_finding(self) -> None:
        self.populate()
        bundle = self.reader.Bundle.open(self.root)
        facts = self.reader.collect(bundle)
        self.assertEqual(facts["ecam"]["mcfg"]["ecam_base"], "0xe0000000")
        self.assertEqual(self.reader.main([str(self.root), "--expect-ecam", "0xe0000000"]), 0)
        self.assertEqual(self.reader.main([str(self.root), "--expect-ecam", "0xb0000000"]), 1)

    def test_a_required_fact_that_is_absent_fails(self) -> None:
        self.populate(include_mcfg=False)
        self.assertEqual(self.reader.main([str(self.root), "--require", "ecam"]), 1)
        self.assertEqual(self.reader.main([str(self.root)]), 0)

    def test_a_tarball_bundle_reads_the_same_way(self) -> None:
        self.populate()
        with self.reader.Bundle.open(self.tarball()) as bundle:
            facts = self.reader.collect(bundle)
        self.assertEqual(facts["ecam"]["mcfg"]["ecam_base"], "0xe0000000")

    def test_a_tarball_leaves_no_unpacked_copy_behind(self) -> None:
        # A capture tarball is hundreds of megabytes, so unpacking it and
        # walking away leaves a second copy on the host per invocation.
        self.populate()
        with self.reader.Bundle.open(self.tarball()) as bundle:
            self.assertTrue(bundle.files, "the unpacked copy is readable while open")
            extracted = bundle.extract_dir
            self.assertIsNotNone(extracted)
            self.assertTrue(extracted.is_dir())
        self.assertFalse(extracted.exists(), "closing the bundle removes the copy")
        self.assertEqual(
            sorted(path.name for path in Path(self._tmp.name).iterdir()),
            ["bundle.tar.gz", self.root.name],
        )

    def test_closing_a_directory_bundle_does_not_delete_it(self) -> None:
        # The directory belongs to the caller: a capture directory on the
        # development host is not the reader's to remove.
        self.populate()
        with self.reader.Bundle.open(self.root) as bundle:
            self.assertIsNone(bundle.extract_dir)
        self.assertTrue(self.root.is_dir())
        bundle.close()  # idempotent, and still not the reader's directory

    def test_a_tarball_that_tries_to_escape_is_refused(self) -> None:
        with self.assertRaisesRegex(self.reader.BundleError, "unsafe tar member"):
            self.reader.Bundle.open(self.evil_tarball())

    def test_a_refused_tarball_leaves_no_unpacked_copy_behind(self) -> None:
        # The refusal happens part way through extraction, so the copy is
        # already on disk when it is raised.
        with self.assertRaisesRegex(self.reader.BundleError, "unsafe tar member"):
            self.reader.Bundle.open(self.evil_tarball())
        self.assertEqual(
            sorted(path.name for path in Path(self._tmp.name).iterdir()),
            ["evil.tar", self.root.name],
            "no hw-facts-* extraction directory survives the refusal",
        )

    def tarball(self) -> Path:
        """Pack the populated bundle the way the capture image would."""

        archive = Path(self._tmp.name) / "bundle.tar.gz"
        buffer = io.BytesIO()
        with tarfile.open(fileobj=buffer, mode="w:gz") as tar:
            tar.add(self.root, arcname=self.root.name)
        archive.write_bytes(buffer.getvalue())
        return archive

    def evil_tarball(self) -> Path:
        evil = Path(self._tmp.name) / "evil.tar"
        with tarfile.open(evil, "w") as tar:
            info = tarfile.TarInfo("../../escaped")
            info.size = 3
            tar.addfile(info, io.BytesIO(b"bad"))
        return evil


if __name__ == "__main__":
    unittest.main()
