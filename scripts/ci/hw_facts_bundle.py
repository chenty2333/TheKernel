#!/usr/bin/env python3
"""Read a hardware-facts bundle captured from a DUT.

The capture image (``scripts/ci/n305-capture-image.sh``) writes a plain
directory of files; this reads it back on the development host and prints the
facts that TheKernel's configuration currently assumes, with the evidence for
each one.  It deliberately parses the *raw* ACPI bytes rather than trusting a
decoded summary, so that the ECAM base can be corroborated against what the
kernel itself reported in dmesg and against the PCI windows in /proc/iomem.

Two jobs it does that the DUT cannot do for itself:

* decode EDID, because ``edid-decode`` is not packaged for the Alpine release
  the capture image uses;
* say what the capture did *not* get, from the bundle's own
  ``capture-status.txt``, instead of quietly reporting fewer facts.

    python3 scripts/ci/hw_facts_bundle.py <bundle-dir> [--json]
    python3 scripts/ci/hw_facts_bundle.py <bundle-dir> --expect-ecam 0xe0000000
"""

from __future__ import annotations

import argparse
import json
import re
import shutil
import struct
import sys
import tarfile
from dataclasses import dataclass, field
from pathlib import Path

MCFG_HEADER_BYTES = 36
MCFG_ALLOCATION_OFFSET = MCFG_HEADER_BYTES + 8  # 36-byte header plus 8 reserved
MCFG_ALLOCATION_BYTES = 16

#: signature, length, revision, checksum, OEM id, OEM table id,
#: OEM revision, creator id, creator revision.
ACPI_HEADER = struct.Struct("<4sIBB6s8sI4sI")
assert ACPI_HEADER.size == 36, "an ACPI table header is 36 bytes"
#: base address (u64), PCI segment group (u16), start bus (u8),
#: end bus (u8), 4 reserved bytes.
MCFG_ALLOCATION = struct.Struct("<QHBBI")
assert MCFG_ALLOCATION.size == MCFG_ALLOCATION_BYTES == 16

DMESG_ECAM = re.compile(
    r"ECAM \[mem (?P<start>0x[0-9a-fA-F]+)-(?P<end>0x[0-9a-fA-F]+)\] "
    r"\(base (?P<base>0x[0-9a-fA-F]+)\) for domain (?P<domain>[0-9a-fA-F]{4}) "
    r"\[bus (?P<bus_start>[0-9a-fA-F]{2})-(?P<bus_end>[0-9a-fA-F]{2})\]"
)
IOMEM_PCI_BUS = re.compile(r"^\s*([0-9a-fA-F]{8,16})-([0-9a-fA-F]{8,16}) : (PCI Bus .*)$")


class BundleError(RuntimeError):
    """The bundle is missing something the caller required."""


@dataclass
class McfgAllocation:
    base: int
    segment: int
    bus_start: int
    bus_end: int

    def as_dict(self) -> dict[str, object]:
        return {
            "ecam_base": f"0x{self.base:x}",
            "segment": self.segment,
            "bus_start": f"{self.bus_start:02x}",
            "bus_end": f"{self.bus_end:02x}",
        }


@dataclass
class McfgTable:
    base: int
    bus_start: int
    bus_end: int
    segment: int
    length: int
    checksum_ok: bool
    allocations: list[McfgAllocation] = field(default_factory=list)


@dataclass
class Edid:
    manufacturer: str
    product: int
    serial: int
    year: int
    week: int
    version: str
    digital: bool
    size_cm: tuple[int, int] | None
    preferred: dict[str, int] | None
    checksum_ok: bool

    def as_dict(self) -> dict[str, object]:
        return {
            "manufacturer": self.manufacturer,
            "product": f"0x{self.product:04x}",
            "serial": self.serial,
            "manufactured": f"week {self.week} of {self.year}",
            "edid_version": self.version,
            "digital": self.digital,
            "size_cm": list(self.size_cm) if self.size_cm else None,
            "preferred_timing": self.preferred,
            "checksum_ok": self.checksum_ok,
        }


def parse_mcfg(data: bytes) -> McfgTable:
    """Parse a raw MCFG table exactly as ACPI defines it."""

    if len(data) < MCFG_ALLOCATION_OFFSET:
        raise BundleError(f"MCFG is too short to hold a table header: {len(data)} bytes")
    signature, length, revision, checksum, _oem, _oem_table, _oem_rev, _creator, _creator_rev = \
        ACPI_HEADER.unpack_from(data)
    if signature != b"MCFG":
        raise BundleError(f"expected an MCFG table, found {signature!r}")
    if length < MCFG_ALLOCATION_OFFSET or length > len(data):
        raise BundleError(f"MCFG declares {length} bytes but {len(data)} were captured")
    table = data[:length]
    if revision < 1:
        raise BundleError(f"MCFG revision {revision} does not define the allocation list")
    checksum_ok = sum(table) % 256 == 0
    allocations = []
    for offset in range(MCFG_ALLOCATION_OFFSET, length - MCFG_ALLOCATION_BYTES + 1, MCFG_ALLOCATION_BYTES):
        base, segment, bus_start, bus_end, _reserved = MCFG_ALLOCATION.unpack_from(data, offset)
        if base == 0 or base == 0xFFFF_FFFF_FFFF_FFFF:
            raise BundleError(f"MCFG allocation at offset {offset} has an impossible base 0x{base:x}")
        if bus_start > bus_end:
            raise BundleError(
                f"MCFG allocation at offset {offset} has an inverted bus range "
                f"{bus_start:02x}-{bus_end:02x}"
            )
        allocations.append(McfgAllocation(base, segment, bus_start, bus_end))
    if not allocations:
        raise BundleError("MCFG contains no allocation entries")
    first = allocations[0]
    return McfgTable(
        base=first.base,
        bus_start=first.bus_start,
        bus_end=first.bus_end,
        segment=first.segment,
        length=length,
        checksum_ok=checksum_ok,
        allocations=allocations,
    )


def decode_edid(data: bytes) -> Edid:
    """Decode the base EDID block: identity, size and the preferred timing."""

    if len(data) < 128:
        raise BundleError(f"EDID block is {len(data)} bytes, expected at least 128")
    block = data[:128]
    if block[:8] != b"\x00\xff\xff\xff\xff\xff\xff\x00":
        raise BundleError("EDID header is not the required 00 FF FF FF FF FF FF 00")
    manufacturer = "".join(
        chr(((block[8] << 8 | block[9]) >> shift & 0x1F) + 64) for shift in (10, 5, 0)
    )
    product = struct.unpack_from("<H", block, 10)[0]
    serial = struct.unpack_from("<I", block, 12)[0]
    week, year_offset = block[16], block[17]
    version = f"{block[18]}.{block[19]}"
    digital = bool(block[20] & 0x80)
    width_cm, height_cm = block[21], block[22]
    size_cm = (width_cm, height_cm) if width_cm and height_cm else None

    preferred = None
    for offset in range(54, 126, 18):
        descriptor = block[offset : offset + 18]
        if descriptor[0] == 0 and descriptor[1] == 0 and descriptor[2] == 0:
            break  # a monitor descriptor, not a detailed timing
        pixel_clock = struct.unpack_from("<H", descriptor, 0)[0]
        if pixel_clock == 0:
            continue
        hactive = descriptor[2] | ((descriptor[4] & 0xF0) << 4)
        hblank = descriptor[3] | ((descriptor[4] & 0x0F) << 8)
        vactive = descriptor[5] | ((descriptor[7] & 0xF0) << 4)
        vblank = descriptor[6] | ((descriptor[7] & 0x0F) << 8)
        # A total of zero means the descriptor contradicts itself: a real mode
        # has at least one pixel and one line.  Refusing it here keeps the
        # failure a decoded finding instead of a ZeroDivisionError that would
        # abandon every later EDID in the bundle.
        if hactive + hblank == 0 or vactive + vblank == 0:
            raise BundleError(
                "EDID detailed timing declares a zero total extent: "
                f"{hactive}+{hblank} by {vactive}+{vblank}"
            )
        preferred = {
            "pixel_clock_khz": pixel_clock * 10,
            "hactive": hactive,
            "hblank": hblank,
            "vactive": vactive,
            "vblank": vblank,
            "hfront": descriptor[8] | ((descriptor[11] & 0xC0) << 2),
            "hsync": descriptor[9] | ((descriptor[11] & 0x30) << 4),
            "vfront": (descriptor[10] >> 4) | ((descriptor[11] & 0x0C) << 2),
            "vsync": (descriptor[10] & 0x0F) | ((descriptor[11] & 0x03) << 4),
        }
        preferred["refresh_hz"] = round(
            preferred["pixel_clock_khz"] * 1000 / ((hactive + hblank) * (vactive + vblank))
        )
        break
    return Edid(
        manufacturer=manufacturer,
        product=product,
        serial=serial,
        year=1990 + year_offset,
        week=week,
        version=version,
        digital=digital,
        size_cm=size_cm,
        preferred=preferred,
        checksum_ok=sum(block) % 256 == 0,
    )


@dataclass
class Bundle:
    root: Path
    files: dict[str, Path]
    #: Directory this bundle unpacked a tarball into, or ``None`` when it reads
    #: a directory that belongs to the caller.  The bundle owns this one and
    #: removes it again, because a capture tarball can be hundreds of megabytes
    #: and leaves behind a full second copy of itself otherwise.
    extract_dir: Path | None = None

    @classmethod
    def open(cls, path: Path) -> Bundle:
        if path.is_file() and tarfile.is_tarfile(path):
            import tempfile

            extract_dir = Path(tempfile.mkdtemp(prefix="hw-facts-", dir=path.parent))
            try:
                with tarfile.open(path) as archive:
                    members = archive.getmembers()
                    for member in members:
                        if member.name.startswith("/") or ".." in Path(member.name).parts:
                            raise BundleError(f"refusing unsafe tar member: {member.name}")
                        if member.islnk() or member.issym():
                            raise BundleError(f"refusing linked tar member: {member.name}")
                        if not (member.isfile() or member.isdir()):
                            raise BundleError(f"refusing special tar member: {member.name}")
                    if hasattr(tarfile, "data_filter"):
                        archive.extractall(extract_dir, filter="data")
                    else:
                        # Extraction filters arrived in Python 3.12 and were
                        # backported only as far as 3.11.4; the CI image's
                        # Debian bookworm ships 3.11.2.  Everything the `data`
                        # filter adds beyond the checks above is the mode
                        # normalisation, so apply that by hand: no setuid,
                        # setgid or sticky bit, no group/other write, and the
                        # owner can always read and write what it unpacked.
                        for member in members:
                            member.mode = (member.mode & 0o755) | (0o700 if member.isdir() else 0o600)
                        archive.extractall(extract_dir, members=members)
                roots = [entry for entry in extract_dir.iterdir() if entry.is_dir()]
                root = roots[0] if len(roots) == 1 else extract_dir
                files = {
                    str(item.relative_to(root)): item for item in root.rglob("*") if item.is_file()
                }
            except BaseException:
                # A refusal half way through extraction still owes the disk the
                # space it took, and the caller cannot know the directory name.
                shutil.rmtree(extract_dir, ignore_errors=True)
                raise
            if not files:
                shutil.rmtree(extract_dir, ignore_errors=True)
                raise BundleError(f"bundle contains no files: {root}")
            return cls(root=root, files=files, extract_dir=extract_dir)
        if path.is_dir():
            root = path
        else:
            raise BundleError(f"not a bundle directory or tarball: {path}")
        files = {str(item.relative_to(root)): item for item in root.rglob("*") if item.is_file()}
        if not files:
            raise BundleError(f"bundle contains no files: {root}")
        return cls(root=root, files=files)

    def close(self) -> None:
        """Remove the unpacked copy, if this bundle made one.  Idempotent."""

        if self.extract_dir is not None:
            shutil.rmtree(self.extract_dir, ignore_errors=True)
            self.extract_dir = None

    def __enter__(self) -> Bundle:
        return self

    def __exit__(self, *exc_info: object) -> None:
        self.close()

    def __del__(self) -> None:
        # The command line reads one bundle and exits, so an explicit close is
        # wasted ceremony there; this covers that path.  It runs at interpreter
        # shutdown too, which is why ``close`` tolerates a partly-torn-down
        # ``shutil`` and refuses to raise.
        try:
            self.close()
        except Exception:
            pass

    def find(self, *candidates: str) -> Path | None:
        for candidate in candidates:
            if candidate in self.files:
                return self.files[candidate]
        return None

    def text(self, *candidates: str) -> str | None:
        path = self.find(*candidates)
        if path is None:
            return None
        try:
            return path.read_text(encoding="utf-8", errors="replace")
        except OSError:
            return None

    def status(self) -> dict[str, list[tuple[str, str]]]:
        text = self.text("capture-status.txt") or ""
        grouped: dict[str, list[tuple[str, str]]] = {}
        for line in text.splitlines():
            fields = line.split("\t")
            if len(fields) < 2:
                continue
            grouped.setdefault(fields[0], []).append(
                (fields[1], fields[2] if len(fields) > 2 else "")
            )
        return grouped

    def ecam(self) -> tuple[McfgTable | None, str]:
        """The ECAM base and the evidence it came from."""

        for name, source in (
            ("acpi/tables/MCFG", "raw ACPI MCFG table from /sys/firmware/acpi/tables"),
            ("acpi/acpidump-bin/MCFG", "acpidump binary table"),
        ):
            path = self.find(name)
            if path is not None:
                return parse_mcfg(path.read_bytes()), source
        return None, "no raw MCFG table in this bundle"


def kernel_ecam(text: str | None) -> dict[str, str] | None:
    if not text:
        return None
    match = DMESG_ECAM.search(text)
    if match is None:
        return None
    return {key: value for key, value in match.groupdict().items()}


def iomem_windows(text: str | None) -> list[str]:
    if not text:
        return []
    return [
        f"{match.group(1)}-{match.group(2)} {match.group(3)}"
        for match in (IOMEM_PCI_BUS.match(line) for line in text.splitlines())
        if match
    ]


def collect(bundle: Bundle) -> dict[str, object]:
    facts: dict[str, object] = {}

    def field_value(name: str) -> str | None:
        text = bundle.text(f"dmi/sysfs-id/{name}")
        return text.strip() if text else None

    facts["machine"] = {
        "vendor": field_value("sys_vendor"),
        "product": field_value("product_name"),
        "serial": field_value("product_serial"),
        "board": field_value("board_name"),
        "bios": " ".join(
            item for item in (field_value("bios_version"), field_value("bios_date")) if item
        )
        or None,
    }

    cpuinfo = bundle.text("proc/cpuinfo.txt") or bundle.text("cpu/lscpu.txt")
    if cpuinfo:
        model = next(
            (
                line.split(":", 1)[1].strip()
                for line in cpuinfo.splitlines()
                if line.lower().startswith("model name")
            ),
            None,
        )
        facts["cpu"] = {
            "model": model,
            "logical_cpus": cpuinfo.count("processor\t:")
            or cpuinfo.count("processor :")
            or len(re.findall(r"^processor\s*:", cpuinfo, re.MULTILINE)),
        }

    table: McfgTable | None = None
    try:
        table, source = bundle.ecam()
    except BundleError as error:
        facts["ecam_error"] = str(error)
        table, source = None, "unreadable MCFG table"
    dmesg = kernel_ecam(bundle.text("acpi/kernel-ecam.txt", "logs/dmesg.txt"))
    windows = iomem_windows(bundle.text("proc/pci-windows.txt", "proc/iomem.txt"))
    facts["ecam"] = {
        "source": source,
        "mcfg": table.allocations[0].as_dict() if table else None,
        "mcfg_all_allocations": [item.as_dict() for item in table.allocations] if table else [],
        "mcfg_length": table.length if table else None,
        "mcfg_checksum_ok": table.checksum_ok if table else None,
        "kernel": dmesg,
        "iomem_pci_windows": windows,
    }

    gpu_bdf = (bundle.text("graphics/gpu-bdf.txt") or "").strip()
    lspci = bundle.text("pci/lspci-nnvvv.txt") or ""
    driver = None
    for block in lspci.split("\n\n"):
        if re.match(rf"^{re.escape(gpu_bdf)}\s", block):
            match = re.search(r"Kernel driver in use: (\S+)", block)
            driver = match.group(1) if match else None
            break
    facts["graphics"] = {
        "bdf": gpu_bdf or None,
        "driver": driver,
        "dmesg": [
            line
            for line in (bundle.text("graphics/dmesg-graphics.txt") or "").splitlines()
            if re.search(r"dmc|guc|huc|vbt|opregion", line, re.IGNORECASE)
        ][:8],
    }

    displays = []
    for name, path in sorted(bundle.files.items()):
        if not name.startswith("display/edid/") or not name.endswith(".edid"):
            continue
        try:
            displays.append({"connector": Path(name).stem, **decode_edid(path.read_bytes()).as_dict()})
        except BundleError as error:
            displays.append({"connector": Path(name).stem, "error": str(error)})
    facts["displays"] = displays

    status = bundle.status()
    facts["capture"] = {
        "counts": {key: len(value) for key, value in sorted(status.items())},
        "problems": [
            f"{key}: {path} {detail}".strip()
            for key, entries in sorted(status.items())
            if key != "OK"
            for path, detail in entries
        ],
    }
    return facts


def render(facts: dict[str, object]) -> str:
    out: list[str] = []
    machine = facts["machine"]
    out.append("machine")
    for key, value in machine.items():
        if value:
            out.append(f"  {key:<10} {value}")

    if "cpu" in facts:
        out.append("cpu")
        out.append(f"  model      {facts['cpu']['model']}")
        out.append(f"  threads    {facts['cpu']['logical_cpus']}")

    ecam = facts["ecam"]
    out.append("pci ecam")
    out.append(f"  source     {ecam['source']}")
    if ecam["mcfg"]:
        entry = ecam["mcfg"]
        out.append(
            f"  MCFG       base {entry['ecam_base']}  segment {entry['segment']}  "
            f"bus {entry['bus_start']}-{entry['bus_end']}  "
            f"({ecam['mcfg_length']} bytes, checksum "
            f"{'ok' if ecam['mcfg_checksum_ok'] else 'INVALID'})"
        )
        if len(ecam["mcfg_all_allocations"]) > 1:
            out.append(f"  note       {len(ecam['mcfg_all_allocations'])} MCFG allocations:")
            for entry in ecam["mcfg_all_allocations"]:
                out.append(
                    f"             base {entry['ecam_base']} segment {entry['segment']} "
                    f"bus {entry['bus_start']}-{entry['bus_end']}"
                )
    else:
        out.append("  MCFG       not available")
    if ecam.get("mcfg_checksum_ok") is False:
        out.append("  WARNING    the MCFG table checksum is wrong; do not trust these values")
    kernel = ecam["kernel"]
    if kernel:
        out.append(
            f"  kernel     base {kernel['base']}  domain 0000  "
            f"bus {kernel['bus_start']}-{kernel['bus_end']}"
        )
        if ecam["mcfg"]:
            agree = kernel["base"].lower().lstrip("0x").lstrip("0") == ecam["mcfg"]["ecam_base"].lstrip("0x").lstrip("0")
            out.append(f"  agreement  {'MCFG and the kernel agree' if agree else 'MISMATCH'}")
    else:
        out.append("  kernel     no ECAM line found in dmesg")
    for window in ecam["iomem_pci_windows"]:
        out.append(f"  iomem      {window}")

    graphics = facts["graphics"]
    out.append("graphics")
    out.append(f"  device     {graphics['bdf'] or 'not identified'}")
    out.append(f"  driver     {graphics['driver'] or 'none bound'}")
    for line in graphics["dmesg"]:
        out.append(f"  dmesg      {line.strip()[:110]}")

    out.append("displays")
    if not facts["displays"]:
        out.append("  none captured")
    for display in facts["displays"]:
        if "error" in display:
            out.append(f"  {display['connector']:<14} unreadable: {display['error']}")
            continue
        preferred = display["preferred_timing"]
        mode = (
            f"{preferred['hactive']}x{preferred['vactive']}@{preferred['refresh_hz']}Hz"
            if preferred
            else "no detailed timing"
        )
        out.append(
            f"  {display['connector']:<14} {display['manufacturer']} "
            f"{display['product']} {display['manufactured']} "
            f"({'digital' if display['digital'] else 'analog'}) "
            f"{display['size_cm'] or '?'} cm  preferred {mode}  "
            f"checksum {'ok' if display['checksum_ok'] else 'INVALID'}"
        )

    capture = facts["capture"]
    out.append("capture")
    out.append(
        "  probes     "
        + ", ".join(f"{key}={value}" for key, value in capture["counts"].items())
    )
    if capture["problems"]:
        out.append("  gaps (what this bundle does not prove):")
        for problem in capture["problems"]:
            out.append(f"    {problem}")
    else:
        out.append("  gaps       none reported by the capture")
    return "\n".join(out)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("bundle", type=Path)
    parser.add_argument("--json", action="store_true", help="emit the facts as JSON")
    parser.add_argument(
        "--expect-ecam",
        help="fail if MCFG disagrees with this base address, e.g. the value the kernel assumes",
    )
    parser.add_argument(
        "--require",
        action="append",
        default=[],
        choices=["ecam", "display", "graphics"],
        help="fail if this fact could not be established",
    )
    args = parser.parse_args(argv)
    try:
        with Bundle.open(args.bundle) as bundle:
            facts = collect(bundle)
    except (BundleError, OSError, tarfile.TarError) as error:
        print(f"hw-facts-bundle: FAIL {error}", file=sys.stderr)
        return 1

    if args.json:
        print(json.dumps(facts, indent=2, sort_keys=True))
    else:
        print(render(facts))

    failures = []
    if "ecam" in args.require and not facts["ecam"]["mcfg"]:
        failures.append("no ECAM base could be established from the bundle")
    if "display" in args.require and not facts["displays"]:
        failures.append("no display EDID was captured")
    if "graphics" in args.require and not facts["graphics"]["bdf"]:
        failures.append("no graphics device was identified")
    if args.expect_ecam and facts["ecam"]["mcfg"]:
        expected = int(args.expect_ecam, 16)
        actual = int(facts["ecam"]["mcfg"]["ecam_base"], 16)
        if expected != actual:
            failures.append(
                f"the kernel assumes ECAM base 0x{expected:x} but MCFG reports 0x{actual:x}: "
                "the assumption is wrong on this machine"
            )
        else:
            print(f"\nECAM base 0x{expected:x} is confirmed by MCFG.")
    if failures:
        for failure in failures:
            print(f"hw-facts-bundle: FAIL {failure}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
