"""Parse a scripts/hw-facts/capture-n305.sh capture into kernel-build facts.

The kernel currently hardcodes a PCIe ECAM base
(crates/ax/thekernel-axplat-x86-pc/axconfig.toml: ``pci-ecam-base =
0xb000_0000``, with the comment "should read from ACPI 'MCFG' table").  This
module reads the capture tarball produced on the target machine and reports the
*real* ECAM base and the PCI bus ranges the firmware actually describes, plus
the handful of other facts the bare-metal bring-up needs: CPU topology, VT-d
enablement, the HPET base, the iGPU identity, the firmware boot mode, and the
preferred mode of the attached monitor.

Design contract, in order of importance:

1.  Never invent a value.  Every fact is a :class:`Fact` that is either
    available with a value *and* the capture path it came from, or unavailable
    with the reason.  Callers must handle both.
2.  The raw bytes win.  MCFG is decoded from the ACPI table bytes, not from a
    decoded text summary, because a bug in the capture-side decoder must not be
    able to silently corrupt the value the kernel will compile in.
3.  Contradictory sources are reported, not resolved.  If lscpu and
    /proc/cpuinfo disagree about the CPU count, that is a fact about the
    capture, not something to paper over.

The module is stdlib-only (Python 3.11+) and is usable both as a library
(``from tools.hw_facts import load_facts``) and as a CLI::

    python3 tools/hw_facts.py capture.tar.zst
    python3 tools/hw_facts.py capture.tar.zst --json
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import struct
import sys
import tarfile
import tempfile
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Mapping, Sequence

__all__ = [
    "AcpiTable",
    "Capture",
    "CaptureError",
    "CpuFacts",
    "DisplayFacts",
    "EcamFacts",
    "EcamRange",
    "Fact",
    "FirmwareFacts",
    "HardwareFacts",
    "HpetFacts",
    "IommuFacts",
    "MemoryFacts",
    "PciFacts",
    "decode_madt",
    "hexdump_bytes",
    "load_facts",
    "parse_acpi_table",
    "parse_edid_preferred_mode",
    "parse_mcfg",
    "render_human",
    "to_jsonable",
]

# ACPI layout constants.  Anything we have not implemented is still indexed and
# counted, but we refuse to interpret bytes we do not have a decoder for.
ACPI_HEADER_LENGTH = 36
# The first table-specific payload offset for the tables we decode (MCFG's
# reserved field, MADT's flags field, ... all end here).  Every use below is
# from the ACPI specification, not from a guess about a particular machine.
ACPI_BODY_OFFSET = 44
MCFG_ENTRY_LENGTH = 16

# Signals that we look for in text artefacts.  These are deliberately narrow:
# a broad grep is a guess dressed up as evidence.
_IOMMU_ENABLED_PATTERNS = (
    re.compile(r"iommu: Default domain type: Translated", re.IGNORECASE),
)
_IOMMU_PASSTHROUGH_PATTERNS = (
    re.compile(r"iommu: Default domain type: Passthrough", re.IGNORECASE),
)


class CaptureError(RuntimeError):
    """The capture could not be opened at all (not a per-fact problem)."""


# ---------------------------------------------------------------------------
# Facts
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class Fact:
    """One decision-relevant value, present or explicitly missing.

    ``value`` is never populated when ``reason`` is set; ``contradiction`` is a
    third state for a value the capture contradicts itself about.
    """

    value: object | None
    sources: tuple[str, ...] = ()
    reason: str | None = None
    contradiction: str | None = None
    hex_width: int | None = None

    @property
    def available(self) -> bool:
        return self.reason is None and self.contradiction is None and self.value is not None

    @classmethod
    def ok(cls, value: object, *sources: str) -> "Fact":
        return cls(value=value, sources=tuple(sources))

    @classmethod
    def ok_hex(cls, value: int, *sources: str, width: int = 16) -> "Fact":
        """A fact whose value is an address or a register-sized number.

        Physical addresses are only ever read as hex by a human; a decimal BAR
        or ECAM base in the summary is a transcription bug waiting to happen.
        """

        return cls(value=value, sources=tuple(sources), hex_width=width)

    @classmethod
    def unavailable(cls, reason: str) -> "Fact":
        return cls(value=None, reason=reason)

    @classmethod
    def conflicted(cls, value: str, contradiction: str, *sources: str) -> "Fact":
        return cls(value=value, sources=tuple(sources), contradiction=contradiction)

    def display(self) -> str:
        if self.hex_width is not None and isinstance(self.value, int):
            return f"0x{self.value:0{self.hex_width}x}"
        return str(self.value)

    def render(self) -> str:
        if self.available:
            text = self.display()
            if self.sources:
                text += f"  [from {', '.join(self.sources)}]"
            return text
        if self.contradiction is not None:
            return f"CONTRADICTORY: {self.value} -- {self.contradiction}"
        return f"UNAVAILABLE: {self.reason}"

    def to_jsonable(self) -> dict[str, Any]:
        if self.available:
            payload: dict[str, Any] = {"status": "ok", "value": self.value}
            if self.hex_width is not None and isinstance(self.value, int):
                payload["value_hex"] = self.display()
            payload["sources"] = list(self.sources)
            return payload
        if self.contradiction is not None:
            return {
                "status": "contradictory",
                "value": self.value,
                "contradiction": self.contradiction,
                "sources": list(self.sources),
            }
        return {"status": "unavailable", "reason": self.reason, "sources": list(self.sources)}


@dataclass(frozen=True)
class EcamRange:
    """One MCFG memory-mapped configuration allocation."""

    segment: int
    first_bus: int
    last_bus: int
    base: int

    @property
    def bus_count(self) -> int:
        return self.last_bus - self.first_bus + 1

    def render(self) -> str:
        return (
            f"segment {self.segment} bus {self.first_bus:02x}-{self.last_bus:02x} "
            f"base=0x{self.base:016x} ({self.bus_count} buses)"
        )

    def to_jsonable(self) -> dict[str, Any]:
        return {
            "segment": self.segment,
            "first_bus": self.first_bus,
            "last_bus": self.last_bus,
            "bus_count": self.bus_count,
            "base": self.base,
            "base_hex": f"0x{self.base:016x}",
        }


@dataclass(frozen=True)
class EcamFacts:
    """The MCFG-derived ECAM facts, which is the whole point of the exercise."""

    base: Fact
    ranges: Fact
    table_present: bool
    table_source: str | None
    notes: tuple[str, ...] = ()

    def to_jsonable(self) -> dict[str, Any]:
        payload: dict[str, Any] = {
            "ecam_base": self.base.to_jsonable(),
            "ecam_ranges": self.ranges.to_jsonable(),
            "mcfg_present": self.table_present,
            "mcfg_source": self.table_source,
        }
        if self.notes:
            payload["notes"] = list(self.notes)
        return payload


@dataclass(frozen=True)
class CpuFacts:
    logical_count: Fact
    topology: Fact
    timer_flags: Mapping[str, bool]
    model_name: Fact

    def to_jsonable(self) -> dict[str, Any]:
        return {
            "logical_count": self.logical_count.to_jsonable(),
            "topology": self.topology.to_jsonable(),
            "timer_flags": dict(self.timer_flags),
            "model_name": self.model_name.to_jsonable(),
        }


@dataclass(frozen=True)
class IommuFacts:
    """DMAR presence, DMAR flags, and whether the kernel actually enabled it."""

    dmar_table_present: Fact
    kernel_enabled: Fact
    evidence: tuple[str, ...] = ()

    def to_jsonable(self) -> dict[str, Any]:
        return {
            "dmar_table_present": self.dmar_table_present.to_jsonable(),
            "kernel_enabled": self.kernel_enabled.to_jsonable(),
            "evidence": list(self.evidence),
        }


@dataclass(frozen=True)
class HpetFacts:
    base: Fact
    period_fs: Fact
    source_notes: tuple[str, ...] = ()

    def to_jsonable(self) -> dict[str, Any]:
        return {
            "base": self.base.to_jsonable(),
            "period_fs": self.period_fs.to_jsonable(),
            "source_notes": list(self.source_notes),
        }


@dataclass(frozen=True)
class IgpuFacts:
    bdf: Fact
    vendor_device: Fact
    revision: Fact
    kernel_driver: Fact
    expected_pci_id: str = "8086:46d0"
    notes: tuple[str, ...] = ()

    def to_jsonable(self) -> dict[str, Any]:
        payload: dict[str, Any] = {
            # The id we went looking for, *not* a claim about what is present:
            # vendor_device carries the identity actually observed.
            "expected_pci_id": self.expected_pci_id,
            "bdf": self.bdf.to_jsonable(),
            "vendor_device": self.vendor_device.to_jsonable(),
            "revision": self.revision.to_jsonable(),
            "kernel_driver": self.kernel_driver.to_jsonable(),
        }
        if self.notes:
            payload["notes"] = list(self.notes)
        return payload


@dataclass(frozen=True)
class FirmwareFacts:
    boot_mode: Fact
    firmware_vendor: Fact
    secure_boot: Fact

    def to_jsonable(self) -> dict[str, Any]:
        return {
            "boot_mode": self.boot_mode.to_jsonable(),
            "firmware_vendor": self.firmware_vendor.to_jsonable(),
            "secure_boot": self.secure_boot.to_jsonable(),
        }


@dataclass(frozen=True)
class DisplayFacts:
    preferred_mode: Fact
    connectors: Mapping[str, str]
    edid_sources: tuple[str, ...] = ()

    def to_jsonable(self) -> dict[str, Any]:
        return {
            "preferred_mode": self.preferred_mode.to_jsonable(),
            "connectors": dict(self.connectors),
            "edid_sources": list(self.edid_sources),
        }


@dataclass(frozen=True)
class MemoryFacts:
    e820: Fact
    ecam_inside_reserved: Fact
    notes: tuple[str, ...] = ()

    def to_jsonable(self) -> dict[str, Any]:
        return {
            "e820_regions": self.e820.to_jsonable(),
            "ecam_inside_reserved": self.ecam_inside_reserved.to_jsonable(),
            "notes": list(self.notes),
        }


@dataclass(frozen=True)
class PciFacts:
    device_count: Fact
    igpu_bdf_from_lspci: Fact

    def to_jsonable(self) -> dict[str, Any]:
        return {
            "device_count": self.device_count.to_jsonable(),
            "igpu_bdf_from_lspci": self.igpu_bdf_from_lspci.to_jsonable(),
        }


@dataclass(frozen=True)
class HardwareFacts:
    """Everything the kernel build needs from one capture."""

    source: str
    ecam: EcamFacts
    cpu: CpuFacts
    iommu: IommuFacts
    hpet: HpetFacts
    igpu: IgpuFacts
    firmware: FirmwareFacts
    display: DisplayFacts
    memory: MemoryFacts
    pci: PciFacts
    acpi_tables: Mapping[str, int] = field(default_factory=dict)

    def to_jsonable(self) -> dict[str, Any]:
        return {
            "source": self.source,
            "ecam": self.ecam.to_jsonable(),
            "cpu": self.cpu.to_jsonable(),
            "iommu": self.iommu.to_jsonable(),
            "hpet": self.hpet.to_jsonable(),
            "igpu": self.igpu.to_jsonable(),
            "firmware": self.firmware.to_jsonable(),
            "display": self.display.to_jsonable(),
            "memory": self.memory.to_jsonable(),
            "pci": self.pci.to_jsonable(),
            "acpi_tables": dict(sorted(self.acpi_tables.items())),
        }


# ---------------------------------------------------------------------------
# Hexdump and ACPI decoding
# ---------------------------------------------------------------------------


def hexdump_bytes(text: str) -> bytes:
    """Undo the hexdump flavours capture-n305.sh can produce.

    The capture writes ``hexdump -Cv``, ``xxd -g1``, or ``od -An -tx1 -v``
    depending on what the live image had.  All three are decoded here rather
    than one being privileged, because a target machine's tool set is not ours
    to choose.  Lines that do not carry hex bytes (comments, the trailing
    UNAVAILABLE note) are skipped: an UNAVAILABLE marker is detected separately
    by the caller so it can explain *why* the fact is missing.
    """
    data = bytearray()
    for line in text.splitlines():
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        if "|" in line:
            # hexdump -C: "00000000  4d 43 46 47 00 00 00 00  |MCFG....|"
            left = line.split("|", 1)[0]
            tokens = left.split()
            if not tokens:
                continue
            try:
                int(tokens[0], 16)
            except ValueError:
                continue
            tokens = tokens[1:]
        else:
            # xxd -g1 separates its byte field from ASCII with two spaces.
            # ASCII can itself be a hex byte (e.g. "ab"), so never tokenize it.
            xxd = re.match(r"^[0-9a-fA-F]+:\s(.*)$", stripped)
            if xxd:
                tokens = re.split(r"\s{2,}", xxd.group(1), maxsplit=1)[0].split()
            else:
                tokens = stripped.split()  # od -An -tx1 -v: bytes only
        for token in tokens:
            if len(token) > 2 or not re.fullmatch(r"[0-9a-fA-F]{1,2}", token):
                # Not a byte: xxd's grouped words have len 4, the ASCII column
                # has longer tokens, and od can emit offsets.
                continue
            data.append(int(token, 16))
    return bytes(data)


def _hexdump_is_missing(text: str) -> str | None:
    """Return the capture's own UNAVAILABLE reason, if it recorded one."""

    for line in text.splitlines():
        stripped = line.strip()
        if stripped.startswith("UNAVAILABLE:"):
            return stripped[len("UNAVAILABLE:") :].strip()
    return None


@dataclass(frozen=True)
class AcpiTable:
    """One decoded ACPI table blob plus the capture path it came from."""

    signature: str
    length: int
    revision: int
    oem_id: str
    declared_length: int
    body: bytes
    source: str

    @property
    def truncated(self) -> bool:
        """True when sysfs handed us fewer bytes than the header declares."""

        return len(self.body) < self.declared_length


def parse_acpi_table(blob: bytes, source: str) -> AcpiTable:
    """Validate length/checksum and expose only the declared ACPI table bytes."""

    if len(blob) < ACPI_HEADER_LENGTH:
        raise ValueError(
            f"ACPI table is {len(blob)} bytes, shorter than the {ACPI_HEADER_LENGTH}-byte header"
        )
    try:
        signature = blob[0:4].decode("ascii", "replace")
        declared_length = struct.unpack_from("<I", blob, 4)[0]
    except (struct.error, IndexError) as exc:  # pragma: no cover - guarded above
        raise ValueError(f"ACPI table header is not decodable: {exc}") from exc
    if declared_length < ACPI_HEADER_LENGTH:
        raise ValueError(f"ACPI declares {declared_length} bytes, shorter than its header")
    if declared_length > len(blob):
        raise ValueError(f"ACPI table truncated: declares {declared_length} bytes, captured {len(blob)}")
    body = blob[:declared_length]
    if sum(body) % 256:
        raise ValueError("ACPI table checksum is invalid")
    revision = blob[8]
    oem_id = blob[10:16].decode("ascii", "replace").rstrip("\x00 ")
    return AcpiTable(
        signature=signature,
        length=len(blob),
        revision=revision,
        oem_id=oem_id,
        declared_length=declared_length,
        body=body,
        source=source,
    )


def parse_mcfg(table: AcpiTable) -> tuple[int, tuple[EcamRange, ...]]:
    """Extract the ECAM base and per-segment bus ranges from an MCFG table.

    Raises ValueError when the table is not an MCFG table, is too short to hold
    the base address, or declares an allocation list that does not fit in the
    bytes we captured.  Those are exactly the cases where returning *some*
    address would be worse than returning none.
    """

    if table.signature != "MCFG":
        raise ValueError(f"expected an MCFG table, got signature {table.signature!r}")
    body = table.body
    if len(body) < ACPI_BODY_OFFSET + 8:
        raise ValueError(
            f"MCFG table is {len(body)} bytes; the 8-byte ECAM base address lives at offset "
            f"{ACPI_BODY_OFFSET}, so the table needs at least {ACPI_BODY_OFFSET + 8} bytes"
        )
    # An all-ones base is what an unprogrammed or absent ECAM looks like; it is
    # never a usable address, so we refuse to report it as one.
    base = struct.unpack_from("<Q", body, ACPI_BODY_OFFSET)[0]
    if base in (0, 0xFFFFFFFFFFFFFFFF):
        raise ValueError(f"MCFG ECAM base is 0x{base:016x}, which is not a usable address")

    if (len(body) - ACPI_BODY_OFFSET) % MCFG_ENTRY_LENGTH:
        raise ValueError("MCFG allocation list ends with an incomplete entry")
    ranges: list[EcamRange] = []
    offset = ACPI_BODY_OFFSET
    while offset + MCFG_ENTRY_LENGTH <= len(body):
        entry_base, segment, first_bus, last_bus = struct.unpack_from("<QHBB", body, offset)
        if entry_base in (0, 0xFFFFFFFFFFFFFFFF):
            raise ValueError(f"MCFG allocation at offset {offset} is not a usable address")
        if last_bus < first_bus:
            raise ValueError(
                f"MCFG allocation at offset {offset} has an inverted bus range "
                f"{first_bus:02x}-{last_bus:02x}"
            )
        ranges.append(
            EcamRange(segment=segment, first_bus=first_bus, last_bus=last_bus, base=entry_base)
        )
        offset += MCFG_ENTRY_LENGTH

    if not ranges:
        raise ValueError(
            "MCFG contains no allocation entries, so it does not name a usable ECAM region"
        )
    return base, tuple(ranges)


def decode_madt(table: AcpiTable) -> dict[str, Any]:
    """Summarise an MADT: local APIC address, IOAPICs, x2APICs, entry counts.

    Used for CPU enumeration sanity checks (does the MADT count match the
    logical CPU count Linux reported?) rather than as a value the kernel
    compiles in.
    """

    if table.signature != "APIC":
        raise ValueError(f"expected an MADT table (signature 'APIC'), got {table.signature!r}")
    body = table.body
    if len(body) < ACPI_BODY_OFFSET:
        raise ValueError(f"MADT table is only {len(body)} bytes; it has no entry list")
    local_apic_address = struct.unpack_from("<I", body, 36)[0]
    flags = struct.unpack_from("<I", body, 40)[0]
    offset = ACPI_BODY_OFFSET
    counts: dict[int, int] = {}
    ioapics: list[dict[str, int]] = []
    x2apics: list[int] = []
    while offset < len(body):
        if offset + 2 > len(body):
            raise ValueError(f"MADT entry header at offset {offset} is truncated")
        kind = body[offset]
        length = body[offset + 1]
        minimum = {0: 8, 1: 12, 9: 16}.get(kind, 2)
        if length < minimum:
            raise ValueError(f"MADT entry type {kind} at offset {offset} needs at least {minimum} bytes, declares {length}")
        if offset + length > len(body):
            raise ValueError(
                f"MADT entry at offset {offset} declares {length} bytes but the table ends "
                f"{len(body) - offset} bytes later"
            )
        counts[kind] = counts.get(kind, 0) + 1
        if kind == 1:  # IOAPIC
            ioapics.append(
                {
                    "id": body[offset + 2],
                    "address": struct.unpack_from("<I", body, offset + 4)[0],
                    "gsi_base": struct.unpack_from("<I", body, offset + 8)[0],
                }
            )
        elif kind == 9:  # x2APIC
            x2apics.append(struct.unpack_from("<I", body, offset + 4)[0])
        offset += length
    return {
        "local_apic_address": local_apic_address,
        "local_apic_address_hex": f"0x{local_apic_address:08x}",
        "pcat_compatibility": bool(flags & 1),
        "entry_counts": {str(key): value for key, value in sorted(counts.items())},
        "ioapics": ioapics,
        "x2apic_ids": x2apics,
        "parsed_bytes": offset,
    }


# ---------------------------------------------------------------------------
# EDID
# ---------------------------------------------------------------------------

EDID_HEADER = b"\x00\xff\xff\xff\xff\xff\xff\x00"
EDID_BASE_BLOCK = 128
EDID_DTD1_OFFSET = 54


def parse_edid_preferred_mode(blob: bytes) -> str:
    """Return "WxH @ R.RR Hz" for the EDID's first detailed timing descriptor.

    The first DTD is the monitor's preferred (native) mode by definition, which
    is what we must set when TheKernel brings the display up.  Raises
    ValueError with the reason when the bytes cannot support that conclusion
    (too short, wrong magic, or a DTD that is a monitor descriptor rather than
    a timing).
    """

    if len(blob) < EDID_BASE_BLOCK:
        raise ValueError(f"EDID is {len(blob)} bytes; the base block alone needs {EDID_BASE_BLOCK}")
    if blob[0:8] != EDID_HEADER:
        raise ValueError("EDID header magic is wrong; the bytes are not an EDID block")
    dtd = blob[EDID_DTD1_OFFSET : EDID_DTD1_OFFSET + 18]
    if len(dtd) < 18:
        raise ValueError("EDID base block is truncated before the first detailed timing")
    pixel_clock_10khz = dtd[1] << 8 | dtd[0]
    if pixel_clock_10khz == 0:
        raise ValueError(
            "the first EDID descriptor carries no pixel clock, so it is a monitor "
            "descriptor (e.g. a range limit) rather than the preferred timing"
        )
    # EDID 1.4 section 3.10.2: byte 4 carries the high bits of the horizontal
    # *active* count in bits 7-4 and of the horizontal *blanking* count in bits
    # 3-0; byte 7 likewise for the vertical counts.  Swapping those nibbles is
    # the classic decode bug, and it is invisible for any mode whose active
    # count is under 256 or whose high nibble happens to be zero.
    h_active = dtd[2] | (dtd[4] & 0xF0) << 4
    h_blank = dtd[3] | (dtd[4] & 0x0F) << 8
    v_active = dtd[5] | (dtd[7] & 0xF0) << 4
    v_blank = dtd[6] | (dtd[7] & 0x0F) << 8
    h_total = h_active + h_blank
    v_total = v_active + v_blank
    if h_active == 0 or v_active == 0:
        raise ValueError("the EDID preferred timing has a zero active area")
    if h_total == 0 or v_total == 0:
        raise ValueError("the EDID preferred timing has zero totals")
    refresh = pixel_clock_10khz * 10_000 / (h_total * v_total)
    return f"{h_active}x{v_active} @ {refresh:.2f} Hz"


# ---------------------------------------------------------------------------
# Capture access
# ---------------------------------------------------------------------------


class Capture:
    """Read-only, path-indexed view of a capture tree or tarball."""

    def __init__(self, root: Path, source: str, temporary: tempfile.TemporaryDirectory | None = None):
        self.root = root
        self.source = source
        self._temporary = temporary
        self._files: dict[str, tuple[Path, ...]] | None = None

    # -- lifecycle ---------------------------------------------------------

    @classmethod
    def open(cls, target: str | Path) -> "Capture":
        path = Path(target)
        if path.is_dir():
            root = _resolve_capture_root(path)
            return cls(root, str(path))
        if not path.exists():
            raise CaptureError(f"capture path does not exist: {path}")
        if not path.is_file():
            raise CaptureError(f"capture path is neither a directory nor a file: {path}")
        temporary, root = _extract_archive(path)
        return cls(root, str(path), temporary)

    def close(self) -> None:
        if self._temporary is not None:
            self._temporary.cleanup()
            self._temporary = None

    def __enter__(self) -> "Capture":
        return self

    def __exit__(self, *exc_info: object) -> None:
        self.close()

    # -- lookup ------------------------------------------------------------

    @property
    def files(self) -> Mapping[str, tuple[Path, ...]]:
        if self._files is None:
            index: dict[str, list[Path]] = {}
            for path in sorted(self.root.rglob("*")):
                if path.is_file() or path.is_symlink():
                    index.setdefault(path.name, []).append(path)
            self._files = {name: tuple(paths) for name, paths in index.items()}
        return self._files

    def find_all(self, name: str | None = None, pattern: str | None = None) -> tuple[Path, ...]:
        """Every captured file matching a basename and/or a tree-relative glob.

        ``name`` matches a basename anywhere in the tree, because capture
        directories move between releases; ``pattern`` matches a glob.  When
        both are given the basename is tried first and the glob is the fallback,
        so a caller can say "the canonical path, or anything that looks like it"
        without a second call.
        """

        matches: list[Path] = []
        if name is not None:
            matches = [path for path in self.files.get(name, ()) if path.is_file()]
        if not matches and pattern is not None:
            matches = [path for path in sorted(self.root.rglob(pattern)) if path.is_file()]
        return tuple(matches)

    def find(self, name: str | None = None, pattern: str | None = None) -> Path | None:
        matches = self.find_all(name, pattern)
        return matches[0] if matches else None

    def read_text(self, name: str | None = None, pattern: str | None = None) -> tuple[str, Path] | None:
        path = self.find(name, pattern)
        if path is None:
            return None
        try:
            return path.read_text(encoding="utf-8", errors="replace"), path
        except OSError:
            return None

    def read_bytes(self, name: str | None = None, pattern: str | None = None) -> tuple[bytes, Path] | None:
        path = self.find(name, pattern)
        if path is None:
            return None
        try:
            return path.read_bytes(), path
        except OSError:
            return None

    def rel(self, path: Path) -> str:
        try:
            return str(path.relative_to(self.root))
        except ValueError:
            return str(path)

    def all_text(self, pattern: str) -> str:
        """Concatenate every captured text file matching a glob.

        Used for the "search all of dmesg" style questions, where the capture
        may have split the kernel log across several artefacts.
        """

        chunks = []
        for path in sorted(self.root.rglob(pattern)):
            if not path.is_file():
                continue
            try:
                chunks.append(path.read_text(encoding="utf-8", errors="replace"))
            except OSError:
                continue
        return "\n".join(chunks)


def _resolve_capture_root(path: Path) -> Path:
    """Descend through a single wrapping directory (tarballs carry one)."""

    current = path
    for _ in range(4):
        entries = [entry for entry in current.iterdir()]
        if any(entry.name in {"MANIFEST.txt", "SUMMARY.txt", "acpi", "pci"} for entry in entries):
            return current
        directories = [entry for entry in entries if entry.is_dir()]
        if len(directories) == 1 and len(entries) <= 2:
            current = directories[0]
            continue
        return current
    return current


def _extract_archive(path: Path) -> tuple[tempfile.TemporaryDirectory, Path]:
    """Unpack .tar.zst / .tar.gz / .tar and return the extracted root.

    Extraction is intentionally narrow: no path traversal, no absolute member
    names, and a size ceiling, because the tarball crosses a trust boundary
    (it comes back from a machine we booted but do not control).
    """

    temporary = _make_extraction_directory()
    destination = Path(temporary.name)
    try:
        handle = _open_tar(path)
    except Exception:
        temporary.cleanup()
        raise
    try:
        with tarfile.open(fileobj=handle, mode="r|") as tar:
            total = 0
            for member in tar:
                staged = _safe_member_path(destination, member.name)
                if staged is None:
                    raise CaptureError(f"refusing to extract unsafe tar member: {member.name!r}")
                total += max(member.size, 0)
                if total > 2 * 1024 * 1024 * 1024:
                    raise CaptureError("capture archive expands beyond 2 GiB; refusing to extract")
                # Captures contain data files and directories, never links or
                # devices. Reject these before extraction on every interpreter.
                if not (member.isfile() or member.isdir()):
                    raise CaptureError(f"refusing non-data tar member: {member.name!r}")
                if member.isdir():
                    staged.mkdir(parents=True, exist_ok=True)
                else:
                    staged.parent.mkdir(parents=True, exist_ok=True)
                    with tar.extractfile(member) as source, staged.open("wb") as output:
                        shutil.copyfileobj(source, output)
    except (tarfile.TarError, OSError, EOFError) as exc:
        temporary.cleanup()
        raise CaptureError(f"cannot read {path} as a tar archive: {exc}") from exc
    except Exception:
        # Any other failure (including our own CaptureError) must not leave the
        # scratch tree behind: once we unwind, the caller has no handle on it.
        temporary.cleanup()
        raise
    finally:
        try:
            handle.close()
        except Exception:  # pragma: no cover - closing a pipe can fail benignly
            pass
    try:
        root = _resolve_capture_root(destination)
    except Exception:
        temporary.cleanup()
        raise
    return temporary, root


def _make_extraction_directory() -> tempfile.TemporaryDirectory:
    """A scratch directory for extraction, deliberately not on tmpfs.

    A capture is tens of megabytes of text and tables; the project convention is
    to keep large artefacts off /tmp when a real disk is available, so the home
    cache is required; failures must not silently redirect large data to RAM.
    """

    preferred = Path(
        os.environ.get("THEKERNEL_HW_FACTS_TMPDIR", Path.home() / ".cache" / "thekernel-hw-facts-tmp")
    )
    try:
        preferred.mkdir(parents=True, exist_ok=True)
        return tempfile.TemporaryDirectory(prefix="capture-", dir=preferred)
    except OSError as exc:
        raise CaptureError(f"cannot create capture directory in {preferred}: {exc}") from exc


def _open_tar(path: Path):
    """Open the tar stream, transparently decompressing zstd or gzip.

    The returned object owns the underlying file handle in every branch, so the
    caller's single ``close()`` releases the descriptor (a leaked handle here
    would be a ResourceWarning at best and a descriptor leak in a long-running
    tool at worst).
    """

    raw = path.open("rb")
    try:
        magic = raw.read(4)
        raw.seek(0)
        if magic[:2] == b"\x28\xb5":  # zstd frame magic
            try:
                import compression.zstd as zstd  # type: ignore[import-not-found]
            except ImportError:
                return _zstd_process_stream(path, raw)
            return _OwnedStream(zstd.ZstdFile(raw), raw)
        if magic[:2] == b"\x1f\x8b":
            import gzip

            return _OwnedStream(gzip.GzipFile(fileobj=raw), raw)
        return _OwnedStream(raw, None)
    except Exception:
        raw.close()
        raise


def _zstd_process_stream(path: Path, raw) -> "_ProcessStream":
    executable = shutil.which("zstd")
    if executable is None:
        raw.close()
        raise CaptureError(
            f"{path} is zstd-compressed but neither Python's compression.zstd "
            "(needs 3.14+) nor the zstd CLI is available"
        )
    process = subprocess.Popen(
        [executable, "-d", "-c", str(path)],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    raw.close()
    return _ProcessStream(process)


class _OwnedStream:
    """A decompressor stream plus the file object it must close with it."""

    def __init__(self, stream, owned) -> None:
        self._stream = stream
        self._owned = owned

    def read(self, size: int = -1) -> bytes:
        return self._stream.read(size)

    def close(self) -> None:
        try:
            self._stream.close()
        finally:
            if self._owned is not None:
                self._owned.close()
                self._owned = None


class _ProcessStream:
    """File-like wrapper over a decompressor subprocess' stdout."""

    def __init__(self, process: subprocess.Popen) -> None:
        self._process = process

    def read(self, size: int = -1) -> bytes:
        assert self._process.stdout is not None
        return self._process.stdout.read(size)

    def close(self) -> None:
        if self._process.stdout is not None:
            self._process.stdout.close()
        self._process.wait(timeout=60)


def _safe_member_path(destination: Path, name: str) -> Path | None:
    if name.startswith("/") or name.startswith("\\"):
        return None
    candidate = destination / name
    try:
        resolved = candidate.resolve()
    except OSError:
        return None
    if destination.resolve() not in resolved.parents and resolved != destination.resolve():
        return None
    return candidate


# ---------------------------------------------------------------------------
# Fact extraction
# ---------------------------------------------------------------------------


def _first_match(text: str, pattern: str, group: int = 1, flags: int = re.MULTILINE) -> str | None:
    match = re.search(pattern, text, flags)
    return match.group(group).strip() if match else None


def _plausible_text(value: str, minimum_letters: int = 3) -> bool:
    """Whether a sysfs "string" really is text and not a rendered pointer.

    On some kernels /sys/firmware/efi/fw_vendor holds the raw UTF-16 vendor blob
    rather than the decoded name, so a value like "0x66de8b98" -- whose letters
    are all hexadecimal digits -- arrives where a firmware vendor should be.
    A vendor name is letters, spaces, and punctuation; digits, underscores, and
    replacement characters mean we are looking at something else, and reporting
    it as the vendor would be a fabricated fact.
    """

    stripped = value.strip()
    if len(stripped) < minimum_letters:
        return False
    if not all(character.isprintable() for character in stripped):
        return False
    if any(character.isdigit() or character == "_" for character in stripped):
        return False
    letters = sum(1 for character in stripped if character.isalpha())
    return letters >= minimum_letters


def extract_ecam(capture: Capture) -> EcamFacts:
    """Decode MCFG from raw bytes; fall back to the capture's own decoder text.

    The fallback exists only so that an operator gets a value when the raw
    table is unreadable for an unrelated reason; it is always labelled as
    second-hand in ``notes``.
    """

    notes: list[str] = []
    candidates = capture.find_all("MCFG.hex")
    if not candidates:
        found = capture.find("MCFG.hex", "**/MCFG*")
        if found is not None:
            candidates = (found,)
    if not candidates:
        return EcamFacts(
            base=Fact.unavailable("no MCFG.hex artefact in the capture"),
            ranges=Fact.unavailable("no MCFG.hex artefact in the capture"),
            table_present=False,
            table_source=None,
            notes=(
                "Firmware did not publish an MCFG table, or /sys/firmware/acpi/tables "
                "did not expose it.  The ECAM base cannot be taken from ACPI on this "
                "machine and must be derived another way.",
            ),
        )

    path = candidates[0]
    source = capture.rel(path)
    text = path.read_text(encoding="utf-8", errors="replace")
    missing = _hexdump_is_missing(text)
    if missing is not None:
        return EcamFacts(
            base=Fact.unavailable(f"capture recorded MCFG as unavailable: {missing}"),
            ranges=Fact.unavailable(f"capture recorded MCFG as unavailable: {missing}"),
            table_present=False,
            table_source=source,
            notes=("An MCFG table that the capture could not read is not evidence that "
                   "the table is absent.",),
        )

    blob = hexdump_bytes(text)
    try:
        table = parse_acpi_table(blob, source)
        base, ranges = parse_mcfg(table)
    except ValueError as exc:
        return EcamFacts(
            base=Fact.unavailable(f"cannot decode {source}: {exc}"),
            ranges=Fact.unavailable(f"cannot decode {source}: {exc}"),
            table_present=True,
            table_source=source,
            notes=(f"{source} was captured but is not decodable: {exc}",),
        )

    if table.truncated:
        notes.append(
            f"{source} holds {table.length} bytes but its header declares "
            f"{table.declared_length}; downstream decodes may be incomplete"
        )
    # A second opinion from the capture-side decoder.  A disagreement means one
    # of the two decoders is wrong, which is worth more than a tie-break.  The
    # decoder prints the table name on one line and the base on the next, so the
    # search is scoped to the MCFG block rather than to a single line.
    decoded, decoded_path = capture.read_text("decoded_tables.txt") or ("", None)
    if decoded_path is not None:
        block = _first_match(
            decoded, r"^(MCFG:.*?)(?=^\w+:|\Z)", group=1, flags=re.MULTILINE | re.DOTALL
        )
        if block is not None:
            other = _first_match(block, r"ECAM base=0x([0-9a-fA-F]+)")
            if other is not None and int(other, 16) != base:
                notes.append(
                    f"the capture's own decoder ({capture.rel(decoded_path)}) reports ECAM "
                    f"base 0x{int(other, 16):016x}, which disagrees with the raw table"
                )

    return EcamFacts(
        base=Fact.ok_hex(base, source),
        ranges=Fact.ok("; ".join(item.render() for item in ranges), source),
        table_present=True,
        table_source=source,
        notes=tuple(notes),
    )


def extract_cpu(capture: Capture) -> CpuFacts:
    lscpu, lscpu_path = capture.read_text("lscpu.txt") or ("", None)
    cpuinfo, cpuinfo_path = capture.read_text("proc_cpuinfo.txt") or ("", None)
    topology_path = capture.find("count.txt", "**/cpu/topology/count.txt") or capture.find(
        "count.txt", "**/topology/count.txt"
    )

    lscpu_count: int | None = None
    if lscpu_path is not None:
        value = _first_match(lscpu, r"^CPU\(s\):\s+([0-9]+)")
        if value is not None:
            lscpu_count = int(value)
        elif "UNAVAILABLE" in lscpu:
            lscpu = ""

    cpuinfo_count: int | None = None
    if cpuinfo_path is not None:
        matches = re.findall(r"^processor\s*:", cpuinfo, re.MULTILINE)
        if matches:
            cpuinfo_count = len(matches)
        elif "UNAVAILABLE" in cpuinfo:
            cpuinfo = ""

    sysfs_count: int | None = None
    if topology_path is not None:
        value = _first_match(topology_path.read_text(errors="replace"), r"logical CPUs found in sysfs:\s*([0-9]+)")
        if value is not None:
            sysfs_count = int(value)

    known = {
        name: count
        for name, count in (
            ("lscpu.txt", lscpu_count),
            ("/proc/cpuinfo", cpuinfo_count),
            ("cpu/topology", sysfs_count),
        )
        if count is not None and count > 0
    }
    if not known:
        count_fact = Fact.unavailable(
            "no artefact in the capture reported a CPU count "
            "(looked for lscpu.txt, proc_cpuinfo.txt, cpu/topology/count.txt)"
        )
    elif len(set(known.values())) == 1:
        count_fact = Fact.ok(next(iter(known.values())), *known.keys())
    else:
        detail = ", ".join(f"{name}={value}" for name, value in known.items())
        count_fact = Fact.conflicted(
            detail,
            "capture sources disagree about the logical CPU count; treat neither as "
            "authoritative until the MADT is checked against them",
            *known.keys(),
        )

    topology_bits = []
    for line in lscpu.splitlines():
        if re.match(r"^(Thread\(s\) per core|Core\(s\) per socket|Socket\(s\)|NUMA|CPU\(s\)|Model name|Vendor ID)", line):
            topology_bits.append(line.strip())
    if topology_bits:
        topology = Fact.ok("; ".join(topology_bits), lscpu_path.name if lscpu_path else "lscpu.txt")
    else:
        topology = Fact.unavailable("lscpu.txt is missing, empty, or unavailable in this capture")

    flags_text, flags_path = capture.read_text("flags_summary.txt") or ("", None)
    timer_flags: dict[str, bool] = {}
    if flags_path is not None:
        for line in flags_text.splitlines():
            match = re.match(r"^(present|ABSENT)\s+(\S+)\s*$", line.strip())
            if match:
                timer_flags[match.group(2)] = match.group(1) == "present"

    model = _first_match(lscpu, r"^Model name:\s*(.+)$")
    if model is None and cpuinfo:
        model = _first_match(cpuinfo, r"^model name\s*:\s*(.+)$")
    if model is not None:
        model_fact = Fact.ok(model, "lscpu.txt" if lscpu else "proc_cpuinfo.txt")
    else:
        model_fact = Fact.unavailable("neither lscpu nor /proc/cpuinfo named the CPU model")

    return CpuFacts(
        logical_count=count_fact,
        topology=topology,
        timer_flags=dict(sorted(timer_flags.items())),
        model_name=model_fact,
    )


def extract_iommu(capture: Capture) -> IommuFacts:
    dmar = capture.find("DMAR.hex")
    dmesg = capture.all_text("**/dmesg*.txt")
    evidence: list[str] = []

    dmar_present: Fact
    if dmar is None:
        dmar_present = Fact.unavailable(
            "no DMAR.hex artefact: firmware published no DMAR table (VT-d is absent or "
            "disabled in firmware) or the capture could not read /sys/firmware/acpi/tables"
        )
    else:
        text = dmar.read_text(encoding="utf-8", errors="replace")
        missing = _hexdump_is_missing(text)
        if missing is not None:
            dmar_present = Fact.unavailable(f"capture recorded DMAR as unavailable: {missing}")
        else:
            try:
                table = parse_acpi_table(hexdump_bytes(text), capture.rel(dmar))
                if table.signature != "DMAR":
                    raise ValueError(f"table signature is {table.signature!r}, not 'DMAR'")
                flags = table.body[37] if len(table.body) > 37 else 0
                evidence.append(
                    f"DMAR table flags=0x{flags:02x} "
                    f"(INTR_REMAP={flags & 1}, X2APIC_OPT_OUT={flags >> 1 & 1})"
                )
                dmar_present = Fact.ok(
                    f"present ({len(table.body)} bytes, flags=0x{flags:02x})", capture.rel(dmar)
                )
            except ValueError as exc:
                dmar_present = Fact.unavailable(f"cannot decode {capture.rel(dmar)}: {exc}")

    iommu_entries = _iommu_entry_names(capture)
    if iommu_entries:
        evidence.append(
            f"/sys/class/iommu has {len(iommu_entries)} entr"
            f"{'y' if len(iommu_entries) == 1 else 'ies'}: " + ", ".join(iommu_entries[:8])
        )

    enabled_signals = [
        match.group(0).strip()
        for pattern in _IOMMU_ENABLED_PATTERNS
        for match in pattern.finditer(dmesg)
    ]
    passthrough_signals = [
        match.group(0).strip()
        for pattern in _IOMMU_PASSTHROUGH_PATTERNS
        for match in pattern.finditer(dmesg)
    ]
    for signal in enabled_signals[:4]:
        evidence.append(f"dmesg: {signal}")
    for signal in passthrough_signals[:4]:
        evidence.append(f"dmesg (passthrough): {signal}")

    # IRQ remapping and registered IOMMUs do not establish the DMA domain.
    # Conflicting domain reports must remain contradictory rather than choosing
    # whichever log pattern happens to be checked first.
    if enabled_signals and passthrough_signals:
        kernel_enabled = Fact.conflicted(
            "Translated and Passthrough", "capture reports conflicting default DMA domains", "dmesg"
        )
    elif enabled_signals:
        kernel_enabled = Fact.ok("enabled (default DMA domain translated)", "dmesg")
    elif passthrough_signals:
        kernel_enabled = Fact.ok(
            "present but in passthrough mode (no DMA translation for the kernel's devices)",
            "dmesg",
        )
    elif iommu_entries:
        kernel_enabled = Fact.unavailable(
            f"{len(iommu_entries)} IOMMU device(s) registered, but DMA translation state is unknown"
        )
    else:
        kernel_enabled = Fact.unavailable(
            "no evidence of an enabled IOMMU: /sys/class/iommu is empty or absent and no "
            "dmesg line shows remapping being turned on"
        )

    return IommuFacts(dmar_table_present=dmar_present, kernel_enabled=kernel_enabled, evidence=tuple(evidence))


_IOMMU_SYMLINK = re.compile(r"->\s*(\S+)\s*$", re.MULTILINE)


def _iommu_entry_names(capture: Capture) -> tuple[str, ...]:
    """Names of the IOMMUs the kernel registered, from either capture artefact.

    ``iommu/names.txt`` is the reliable source; ``iommu/sys_class_iommu.txt`` is
    the `ls -l` listing, whose column count varies with coreutils version, so
    only the symlink target is parsed and only as a fallback.
    """

    names, names_path = capture.read_text("names.txt", "**/iommu/names.txt") or ("", None)
    if names_path is not None:
        found = tuple(
            line.strip()
            for line in names.splitlines()
            if line.strip() and not line.startswith("#") and not line.startswith("UNAVAILABLE")
        )
        if found:
            return found

    listing = capture.find("sys_class_iommu.txt")
    if listing is None:
        return ()
    text = listing.read_text(encoding="utf-8", errors="replace")
    if "UNAVAILABLE" in text:
        return ()
    targets = []
    for target in _IOMMU_SYMLINK.findall(text):
        name = Path(target.rstrip("/")).name
        if name and name not in targets:
            targets.append(name)
    return tuple(targets)


def extract_hpet(capture: Capture) -> HpetFacts:
    notes: list[str] = []
    base: Fact = Fact.unavailable("no HPET base address found in any capture artefact")
    period: Fact = Fact.unavailable("HPET period requires the MMIO capabilities register; ACPI does not carry it")

    # 1. The ACPI HPET table is authoritative: it is what firmware tells the OS.
    hpet_table = capture.find("HPET.hex")
    if hpet_table is not None:
        text = hpet_table.read_text(encoding="utf-8", errors="replace")
        missing = _hexdump_is_missing(text)
        if missing is not None:
            notes.append(f"ACPI HPET table unreadable in the capture: {missing}")
        else:
            try:
                table = parse_acpi_table(hexdump_bytes(text), capture.rel(hpet_table))
                if table.signature != "HPET":
                    raise ValueError(f"table signature is {table.signature!r}, not 'HPET'")
                if len(table.body) < 56:
                    raise ValueError(f"HPET table is only {len(table.body)} bytes")
                base_address = struct.unpack_from("<Q", table.body, 44)[0]
                if base_address == 0:
                    raise ValueError("HPET table base address field is zero")
                base = Fact.ok_hex(base_address, capture.rel(hpet_table))
            except ValueError as exc:
                notes.append(f"ACPI HPET table is not decodable: {exc}")

    # 2. The PNP0103 sysfs resource, which is what the kernel actually mapped.
    if not base.available:
        resource = capture.find(pattern="**/PNP0103*/resource")
        if resource is not None:
            text = resource.read_text(encoding="utf-8", errors="replace")
            match = re.search(r"^0x([0-9a-fA-F]+)\s+0x([0-9a-fA-F]+)\s+0x", text, re.MULTILINE)
            if match:
                base = Fact.ok_hex(int(match.group(1), 16), capture.rel(resource))
            else:
                notes.append(f"{capture.rel(resource)} holds no decodable start/end/flags row")

    # 3. dmesg's own claim, kept separate because it is a claim, not a register.
    # Kernels print this as "hpet0: at MMIO 0xfed00000" or "HPET: at ..." and our
    # own capture-side decoder prints "HPET base address=0x...", so the pattern
    # tolerates the index, the separator, and both wordings.
    if not base.available:
        dmesg = capture.all_text("**/dmesg*.txt")
        match = re.search(
            r"HPET[0-9]*:?\s+base(?:\s+address)?[ =:]*0x([0-9a-fA-F]+)", dmesg, re.IGNORECASE
        ) or re.search(r"HPET[0-9]*:?\s+(?:at\s+)?(?:MMIO\s+)?0x([0-9a-fA-F]+)", dmesg, re.IGNORECASE)
        if match:
            base = Fact.ok_hex(int(match.group(1), 16), "dmesg")
    return HpetFacts(base=base, period_fs=period, source_notes=tuple(notes))


def extract_igpu(capture: Capture) -> IgpuFacts:
    expected_id = "8086:46d0"
    lspci, lspci_path = capture.read_text("lspci_nn.txt") or (None, None)
    bdf: Fact = Fact.unavailable("no 8086:46d0 device found in lspci_nn.txt")
    vendor_device: Fact = Fact.unavailable("no 8086:46d0 device found")
    revision: Fact = Fact.unavailable("no 8086:46d0 device found")
    driver: Fact = Fact.unavailable("no driver binding recorded for the iGPU")
    notes: list[str] = []

    if lspci_path is not None:
        # lspci prints "00:02.0" for domain 0 but "0000:00:02.0" when asked for
        # full domains, so the domain prefix (and everything between the BDF and
        # the bracketed PCI id) is optional here.
        pattern = re.compile(
            rf"^((?:[0-9a-f]{{4}}:)?[0-9a-f]{{2}}:[0-9a-f]{{2}}\.[0-9a-f])\s+.*?"
            rf"\[{expected_id}\](?:\s+\(rev\s+([0-9a-fA-F]+)\))?",
            re.MULTILINE,
        )
        match = pattern.search(lspci)
        if match:
            bdf = Fact.ok(match.group(1), capture.rel(lspci_path))
            vendor_device = Fact.ok(expected_id, capture.rel(lspci_path))
            if match.group(2):
                revision = Fact.ok(int(match.group(2), 16), capture.rel(lspci_path))

    # The capture records the BDF it looked at in gpu/igpu_bdf.txt, which is the
    # only place a *different* VGA device is named when the expected iGPU is
    # absent.  Accepting it (with a note) beats reporting "no iGPU" for a
    # machine that plainly has one.
    if not bdf.available:
        captured_bdf, captured_path = capture.read_text("igpu_bdf.txt", "**/gpu/igpu_bdf.txt") or ("", None)
        if captured_path is not None and not captured_bdf.startswith("UNAVAILABLE"):
            candidate = captured_bdf.split()[0]
            if re.fullmatch(r"(?:[0-9a-f]{4}:)?[0-9a-f]{2}:[0-9a-f]{2}\.[0-9a-f]", candidate):
                bdf = Fact.ok(candidate, capture.rel(captured_path))
                if "NOT 8086:46d0" in captured_bdf:
                    notes.append(
                        f"{capture.rel(captured_path)}: the capture found a VGA-class device that "
                        f"is not {expected_id}: {captured_bdf}"
                    )

    # Fall back to the sysfs view for the identity itself.  Both halves of the
    # vendor:device pair must match, because a capture with several Intel
    # devices would otherwise let any one of them stand in for the iGPU.
    if not bdf.available:
        bdf = _igpu_bdf_from_sysfs(capture)
        if bdf.available:
            vendor_device = Fact.ok(expected_id, *(bdf.sources or ("sysfs",)))

    # The capture records the bound driver in two places: gpu/driver.txt targets
    # the iGPU specifically, and each per-BDF sysfs snapshot carries a
    # driver_bound.txt.  Prefer the iGPU-specific one so a machine whose iGPU has
    # no driver (and whose NVMe does) cannot report nvme as the graphics driver.
    driver_file = capture.find(pattern="**/gpu/driver.txt")
    if driver_file is None and bdf.available:
        wanted = "pci/devices/" + str(bdf.value).replace(":", "_")
        candidate = capture.find_all(pattern=f"**/{wanted}/driver_bound.txt")
        driver_file = candidate[0] if candidate else None
    if driver_file is None:
        driver_file = capture.find(pattern="**/gpu/driver_bound.txt")
    if driver_file is not None:
        text = driver_file.read_text(encoding="utf-8", errors="replace").strip()
        if text and not text.startswith("UNAVAILABLE"):
            driver = Fact.ok(text, capture.rel(driver_file))
        else:
            driver = Fact.unavailable(text or f"{capture.rel(driver_file)} is empty")

    if not revision.available:
        # Only a revision captured for the BDF we actually identified counts;
        # taking another device's revision would be an invented fact.
        wanted = str(bdf.value).replace(":", "_") if bdf.available else None
        for candidate in capture.find_all("revision.txt"):
            parent = candidate.parent.name
            if wanted is not None and wanted not in parent:
                continue
            if wanted is None and "46d0" not in parent and "00_02" not in parent:
                continue
            text = candidate.read_text(encoding="utf-8", errors="replace").strip()
            if not text or text.startswith("UNAVAILABLE"):
                continue
            try:
                revision = Fact.ok(int(text, 16), capture.rel(candidate))
            except ValueError:
                revision = Fact.unavailable(
                    f"{capture.rel(candidate)} holds {text!r}, which is not a revision number"
                )
            break

    if notes and bdf.available and not vendor_device.available:
        # The capture identified *a* VGA device but not the one we expect.  The
        # identity stays unavailable on purpose; saying "8086:46d0" here would
        # be exactly the invented fact this module exists to prevent.
        vendor_device = Fact.unavailable(
            f"the identified device is not {expected_id}; see gpu/igpu_bdf.txt"
        )

    return IgpuFacts(
        bdf=bdf,
        vendor_device=vendor_device,
        revision=revision,
        kernel_driver=driver,
        expected_pci_id=expected_id,
        notes=tuple(notes),
    )


def _igpu_bdf_from_sysfs(capture: Capture) -> Fact:
    """Find 8086:46d0 through the capture's per-device sysfs vendor/device files."""

    vendor_files = {path.parent: path for path in capture.find_all("vendor.txt")}
    checked = 0
    for directory, vendor_file in sorted(vendor_files.items()):
        device_file = directory / "device.txt"
        if not device_file.is_file():
            continue
        checked += 1
        vendor = vendor_file.read_text(encoding="utf-8", errors="replace").strip()
        device = device_file.read_text(encoding="utf-8", errors="replace").strip()
        if not vendor.startswith("0x") or not device.startswith("0x"):
            continue
        if int(vendor, 16) == 0x8086 and int(device, 16) == 0x46D0:
            bdf_file = directory / "bdf.txt"
            bdf = (
                bdf_file.read_text(encoding="utf-8", errors="replace").strip()
                if bdf_file.is_file()
                else directory.name.replace("_", ":", 1)
            )
            return Fact.ok(bdf, capture.rel(directory))
    if checked == 0:
        return Fact.unavailable(
            "no 8086:46d0 in lspci_nn.txt and no per-device sysfs vendor/device files "
            "in the capture"
        )
    return Fact.unavailable(
        f"checked {checked} per-device sysfs vendor/device pairs and none was 8086:46d0"
    )


def extract_firmware(capture: Capture) -> FirmwareFacts:
    boot_mode_text, boot_mode_path = capture.read_text("boot_mode.txt") or ("", None)
    if boot_mode_path is not None and not boot_mode_text.startswith("UNAVAILABLE"):
        boot_mode = Fact.ok(boot_mode_text.strip(), capture.rel(boot_mode_path))
    else:
        efivars = capture.find("efivars.txt")
        if efivars is not None and "UNAVAILABLE" not in efivars.read_text(errors="replace"):
            boot_mode = Fact.ok("UEFI (efivars are present)", capture.rel(efivars))
        else:
            boot_mode = Fact.unavailable(
                "the capture has no boot_mode.txt and no readable efivars listing, so the "
                "boot mode cannot be established from it"
            )

    vendor_text, vendor_path = capture.read_text("fw_vendor.txt") or ("", None)
    if vendor_path is not None and _plausible_text(vendor_text):
        firmware_vendor = Fact.ok(vendor_text.strip(), capture.rel(vendor_path))
    else:
        # The EFI variable is unusable (absent, empty, or a raw blob).  Fall back
        # to SMBIOS, which is a different source of the same fact, and say why.
        if vendor_path is None:
            why = "no /sys/firmware/efi/fw_vendor artefact in the capture"
        elif not vendor_text.strip():
            why = f"{capture.rel(vendor_path)} is empty"
        else:
            why = (
                f"{capture.rel(vendor_path)} holds {vendor_text.strip()!r}, which is not a "
                "decoded vendor string (some kernels expose the raw UTF-16 blob here)"
            )
        dmi, dmi_path = capture.read_text("dmidecode_bios.txt") or capture.read_text("dmidecode.txt") or (None, None)
        vendor = _first_match(dmi, r"^\s*Vendor:\s*(.+)$") if dmi else None
        if vendor and _plausible_text(vendor):
            firmware_vendor = Fact.ok(f"{vendor}  (fw_vendor unusable: {why})", capture.rel(dmi_path))
        else:
            firmware_vendor = Fact.unavailable(why)

    secure = _secure_boot_fact(capture)
    return FirmwareFacts(boot_mode=boot_mode, firmware_vendor=firmware_vendor, secure_boot=secure)


def _secure_boot_fact(capture: Capture) -> Fact:
    summary, summary_path = capture.read_text("SUMMARY.txt") or ("", None)
    if summary_path is not None:
        value = _first_match(summary, r"^\s*Secure Boot:\s*(.+)$")
        if value is not None:
            if value.startswith("UNAVAILABLE"):
                return Fact.unavailable(value[len("UNAVAILABLE:") :].strip())
            return Fact.ok(value, capture.rel(summary_path))
    variable = capture.find("SecureBoot.hex")
    if variable is None:
        return Fact.unavailable(
            "no SecureBoot efivar captured; a machine booted in legacy mode has no efivars, "
            "and some kernels hide them from a non-root reader"
        )
    text = variable.read_text(encoding="utf-8", errors="replace")
    if _hexdump_is_missing(text) is not None:
        return Fact.unavailable(_hexdump_is_missing(text) or "SecureBoot efivar unavailable")
    blob = hexdump_bytes(text)
    if len(blob) < 5:
        return Fact.unavailable(
            f"SecureBoot efivar holds {len(blob)} bytes; the attribute word plus value need 5"
        )
    return Fact.ok(
        "enabled" if blob[4] else "disabled", capture.rel(variable)
    )


_CONNECTOR_LINE = re.compile(
    r"^\s*(card[0-9]+-[A-Za-z0-9-]+):\s*status=(\S+)\s+enabled=(\S+)\s+dpms=(\S+)\s*$",
    re.MULTILINE,
)
# Both of these read SUMMARY.txt, whose values are indented by two spaces, so
# every pattern anchored with ^ must allow leading whitespace.
_PREFERRED_LINE = re.compile(r"^\s*preferred:\s*(.+?)\s*$", re.MULTILINE)


def extract_display(capture: Capture) -> DisplayFacts:
    summary, summary_path = capture.read_text("SUMMARY.txt") or ("", None)
    connectors: dict[str, str] = {}
    if summary_path is not None:
        for match in _CONNECTOR_LINE.finditer(summary):
            connectors[match.group(1)] = f"status={match.group(2)} enabled={match.group(3)} dpms={match.group(4)}"

    preferred: Fact = Fact.unavailable(
        "no EDID artefact in the capture; either no display was attached, or the connector "
        "exposed no EDID bytes"
    )
    edid_sources: list[str] = []

    summary_preferred = _first_match(summary or "", r"^\s*preferred:\s*(.+?)\s*$")
    hexdumps = capture.find_all("edid.hex")
    for path in hexdumps:
        text = path.read_text(encoding="utf-8", errors="replace")
        if _hexdump_is_missing(text) is not None:
            continue
        edid_sources.append(capture.rel(path))
        try:
            mode = parse_edid_preferred_mode(hexdump_bytes(text))
        except ValueError:
            continue
        preferred = Fact.ok(mode, capture.rel(path))
        break

    # The connector's advertised mode list comes next, *before* the capture's own
    # summary: SUMMARY.txt is generated by the same EDID parser we are trying to
    # corroborate, so using it to overrule a failed raw parse would let one bug
    # hide behind another.  It is labelled as a mode list, not a preferred timing.
    if not preferred.available:
        for path in capture.find_all("modes.txt"):
            text = path.read_text(encoding="utf-8", errors="replace")
            if text.startswith("UNAVAILABLE") or not text.strip():
                continue
            first = text.split()[0]
            preferred = Fact.ok(
                first, f"{capture.rel(path)} (first advertised mode, not an EDID preferred timing)"
            )
            break

    if not preferred.available and summary_preferred is not None and not summary_preferred.startswith(
        "UNAVAILABLE"
    ):
        preferred = Fact.ok(summary_preferred, capture.rel(summary_path))

    if not preferred.available:
        for path in capture.find_all("edid.hex"):
            reason = _hexdump_is_missing(path.read_text(encoding="utf-8", errors="replace"))
            if reason:
                preferred = Fact.unavailable(f"{capture.rel(path)}: {reason}")
                break

    return DisplayFacts(preferred_mode=preferred, connectors=connectors, edid_sources=tuple(edid_sources))


_E820_LINE = re.compile(
    r"BIOS-e820:\s*\[mem\s+0x([0-9a-fA-F]+)-0x([0-9a-fA-F]+)\]\s*(.*)$", re.MULTILINE
)


def extract_memory(capture: Capture, ecam: EcamFacts) -> MemoryFacts:
    # The capture writes the firmware's e820 lines to memory/e820.txt, but an
    # older or partial capture may only carry the raw dmesg, so search both
    # rather than reporting "no memory map" for a capture that plainly has one.
    e820_text = capture.all_text("**/e820.txt")
    e820_source = "memory/e820.txt"
    if "BIOS-e820" not in e820_text:
        dmesg_text = capture.all_text("**/dmesg*.txt")
        if "BIOS-e820" in dmesg_text:
            e820_text = dmesg_text
            e820_source = "dmesg"
    regions = [
        f"0x{int(match.group(1), 16):016x}-0x{int(match.group(2), 16):016x} {match.group(3).strip()}"
        for match in _E820_LINE.finditer(e820_text)
    ]
    if regions:
        e820 = Fact.ok(f"{len(regions)} e820 regions", e820_source)
    else:
        e820 = Fact.unavailable("no 'BIOS-e820' lines in the capture's dmesg or memory artefacts")

    notes: list[str] = []
    inside: Fact = Fact.unavailable("the ECAM base is unknown, so overlap cannot be checked")
    if ecam.base.available and regions and isinstance(ecam.base.value, int):
        base = ecam.base.value
        for region in regions:
            low, high = region.split()[0].split("-")
            if int(low, 16) <= base <= int(high, 16):
                inside = Fact.ok(
                    f"yes: ECAM base 0x{base:016x} falls inside {region}", e820_source
                )
                break
        else:
            inside = Fact.ok(
                f"no: ECAM base 0x{base:016x} is outside every captured e820 region",
                e820_source,
            )
            notes.append(
                "The kernel must not map the ECAM region as normal RAM; an ECAM base "
                "outside the firmware's e820 map is expected, but check the reserved "
                "ranges in /proc/iomem before trusting the mapping."
            )
    return MemoryFacts(e820=e820, ecam_inside_reserved=inside, notes=tuple(notes))


_PCI_DEVICE_LINE = re.compile(
    r"^(?:[0-9a-f]{4}:)?[0-9a-f]{2}:[0-9a-f]{2}\.[0-9a-f]\s", re.MULTILINE | re.IGNORECASE
)


def extract_pci(capture: Capture) -> PciFacts:
    lspci, lspci_path = capture.read_text("lspci_nn.txt") or (None, None)
    if lspci_path is None:
        return PciFacts(
            device_count=Fact.unavailable("no lspci_nn.txt artefact in the capture"),
            igpu_bdf_from_lspci=Fact.unavailable("no lspci_nn.txt artefact in the capture"),
        )
    count = len(_PCI_DEVICE_LINE.findall(lspci))
    device_count = (
        Fact.ok(count, capture.rel(lspci_path))
        if count
        else Fact.unavailable(f"{capture.rel(lspci_path)} lists no PCI devices")
    )
    match = re.search(
        rf"^((?:[0-9a-f]{{4}}:)?[0-9a-f]{{2}}:[0-9a-f]{{2}}\.[0-9a-f])\s+.*?\[8086:46d0\]",
        lspci,
        re.MULTILINE,
    )
    igpu = (
        Fact.ok(match.group(1), capture.rel(lspci_path))
        if match
        else Fact.unavailable("no 8086:46d0 device line in lspci_nn.txt")
    )
    return PciFacts(device_count=device_count, igpu_bdf_from_lspci=igpu)


def _acpi_table_inventory(capture: Capture) -> dict[str, int]:
    inventory: dict[str, int] = {}
    for path in capture.find_all(pattern="**/acpi/tables/*.hex"):
        name = path.name[: -len(".hex")]
        text = path.read_text(encoding="utf-8", errors="replace")
        if _hexdump_is_missing(text) is not None:
            continue
        blob = hexdump_bytes(text)
        if len(blob) >= ACPI_HEADER_LENGTH:
            inventory[name] = len(blob)
    return inventory


def load_facts(target: str | Path) -> HardwareFacts:
    """Read a capture (tarball or extracted directory) and return its facts."""

    capture = Capture.open(target)
    try:
        ecam = extract_ecam(capture)
        return HardwareFacts(
            source=capture.source,
            ecam=ecam,
            cpu=extract_cpu(capture),
            iommu=extract_iommu(capture),
            hpet=extract_hpet(capture),
            igpu=extract_igpu(capture),
            firmware=extract_firmware(capture),
            display=extract_display(capture),
            memory=extract_memory(capture, ecam),
            pci=extract_pci(capture),
            acpi_tables=_acpi_table_inventory(capture),
        )
    finally:
        capture.close()


# ---------------------------------------------------------------------------
# Rendering
# ---------------------------------------------------------------------------


def to_jsonable(facts: HardwareFacts) -> dict[str, Any]:
    """The JSON payload, including a flat decision list for build tooling."""

    payload = facts.to_jsonable()
    payload["schema"] = "thekernel.hw_facts.v1"
    payload["required_facts"] = {
        "ecam_base": facts.ecam.base.to_jsonable(),
        "cpu_count": facts.cpu.logical_count.to_jsonable(),
        "vtd_enabled": facts.iommu.kernel_enabled.to_jsonable(),
        "hpet_base": facts.hpet.base.to_jsonable(),
        "igpu": facts.igpu.to_jsonable(),
        "boot_mode": facts.firmware.boot_mode.to_jsonable(),
        "monitor_preferred_mode": facts.display.preferred_mode.to_jsonable(),
    }
    return payload


def render_human(facts: HardwareFacts) -> str:
    lines: list[str] = []
    add = lines.append
    add(f"hardware facts from {facts.source}")
    add("")
    add("== PCIe ECAM (MCFG) ==")
    add(f"  ecam base      : {facts.ecam.base.render()}")
    add(f"  bus ranges     : {facts.ecam.ranges.render()}")
    add(f"  MCFG present   : {'yes' if facts.ecam.table_present else 'no'}")
    if facts.ecam.table_source:
        add(f"  decoded from   : {facts.ecam.table_source}")
    for note in facts.ecam.notes:
        add(f"  note           : {note}")
    add("")
    add("== CPU ==")
    add(f"  logical CPUs   : {facts.cpu.logical_count.render()}")
    add(f"  model          : {facts.cpu.model_name.render()}")
    add(f"  topology       : {facts.cpu.topology.render()}")
    if facts.cpu.timer_flags:
        present = [name for name, value in facts.cpu.timer_flags.items() if value]
        absent = [name for name, value in facts.cpu.timer_flags.items() if not value]
        add(f"  timer flags on : {', '.join(present) if present else '(none)'}")
        add(f"  timer flags off: {', '.join(absent) if absent else '(none)'}")
    add("")
    add("== IOMMU / VT-d ==")
    add(f"  DMAR in ACPI   : {facts.iommu.dmar_table_present.render()}")
    add(f"  kernel IOMMU   : {facts.iommu.kernel_enabled.render()}")
    for item in facts.iommu.evidence:
        add(f"    - {item}")
    add("")
    add("== HPET ==")
    add(f"  HPET base      : {facts.hpet.base.render()}")
    add(f"  HPET period    : {facts.hpet.period_fs.render()}")
    for note in facts.hpet.source_notes:
        add(f"  note           : {note}")
    add("")
    add("== iGPU ==")
    add(f"  expected id    : {facts.igpu.expected_pci_id}")
    add(f"  BDF            : {facts.igpu.bdf.render()}")
    add(f"  vendor:device  : {facts.igpu.vendor_device.render()}")
    add(f"  revision       : {facts.igpu.revision.render()}")
    add(f"  kernel driver  : {facts.igpu.kernel_driver.render()}")
    for note in facts.igpu.notes:
        add(f"  note           : {note}")
    add("")
    add("== firmware ==")
    add(f"  boot mode      : {facts.firmware.boot_mode.render()}")
    add(f"  firmware vendor: {facts.firmware.firmware_vendor.render()}")
    add(f"  secure boot    : {facts.firmware.secure_boot.render()}")
    add("")
    add("== display ==")
    add(f"  preferred mode : {facts.display.preferred_mode.render()}")
    for name, status in sorted(facts.display.connectors.items()):
        add(f"    {name}: {status}")
    if facts.display.edid_sources:
        add(f"  EDID sources   : {', '.join(facts.display.edid_sources)}")
    add("")
    add("== memory / PCI ==")
    add(f"  e820           : {facts.memory.e820.render()}")
    add(f"  ECAM in e820   : {facts.memory.ecam_inside_reserved.render()}")
    for note in facts.memory.notes:
        add(f"  note           : {note}")
    add(f"  PCI devices    : {facts.pci.device_count.render()}")
    add("")
    add("== ACPI tables captured ==")
    if facts.acpi_tables:
        for name, size in sorted(facts.acpi_tables.items()):
            add(f"  {name:<6} {size:>7} bytes")
    else:
        add("  UNAVAILABLE: no ACPI table hexdumps in the capture")
    return "\n".join(lines)


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="hw_facts.py",
        description=(
            "Extract kernel-build facts (notably the real PCIe ECAM base from ACPI MCFG) "
            "from a capture-n305.sh tarball or an extracted capture directory."
        ),
    )
    parser.add_argument(
        "capture",
        help="path to capture-*.tar.zst / *.tar.gz or to the extracted capture directory",
    )
    parser.add_argument("--json", action="store_true", help="emit the structured JSON summary")
    parser.add_argument(
        "--require",
        action="append",
        default=None,
        metavar="FACT",
        help=(
            "exit non-zero unless FACT is available; repeatable.  Known facts: "
            "ecam_base, cpu_count, vtd_enabled, hpet_base, igpu, boot_mode, monitor_preferred_mode"
        ),
    )
    return parser


_REQUIRED_LOOKUP = {
    "ecam_base": lambda facts: facts.ecam.base,
    "cpu_count": lambda facts: facts.cpu.logical_count,
    "vtd_enabled": lambda facts: facts.iommu.kernel_enabled,
    "hpet_base": lambda facts: facts.hpet.base,
    "igpu": lambda facts: facts.igpu.bdf,
    "boot_mode": lambda facts: facts.firmware.boot_mode,
    "monitor_preferred_mode": lambda facts: facts.display.preferred_mode,
}


def main(argv: Sequence[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        facts = load_facts(args.capture)
    except CaptureError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2

    if args.json:
        print(json.dumps(to_jsonable(facts), indent=2, sort_keys=False))
    else:
        print(render_human(facts))

    missing: list[str] = []
    for name in args.require or ():
        lookup = _REQUIRED_LOOKUP.get(name)
        if lookup is None:
            print(f"error: unknown --require fact: {name}", file=sys.stderr)
            return 2
        fact = lookup(facts)
        if not fact.available:
            missing.append(f"{name} ({fact.render()})")
    if missing:
        print("error: required facts are not available:", file=sys.stderr)
        for item in missing:
            print(f"  {item}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":  # pragma: no cover - exercised via the CLI test
    sys.exit(main())
