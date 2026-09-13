#!/usr/bin/env bash
# Build a bootable, unattended hardware-facts capture USB image for the
# Acer 蜂鸟 mini (SQM2270) / Intel i3-N305.
#
# The image is a stock Alpine Linux live ISO with an extra FAT partition
# appended to it.  Nothing in the vendor ISO is modified: Alpine's own
# bootloader, kernel and initramfs are used exactly as shipped, which is the
# least inventive way to get a bootable USB and the easiest to trust.  The
# appended partition carries:
#
#   alpine.apkovl.tar.gz   found automatically by Alpine's initramfs (its
#                          nlplug-findfs scans every partition for
#                          *.apkovl.tar.gz at the filesystem root), which is
#                          what makes the capture start with no kernel
#                          command line edit and no console interaction;
#   apks/                  the offline tool set (lspci, acpidump/iasl,
#                          dmidecode, lscpu, modetest, drm_info) with its
#                          dependency closure, resolved from the Alpine
#                          release branch that matches the ISO;
#   payload/               a readable copy of the capture script;
#   dump/                  where the capture writes its bundle.
#
# Why Alpine: it is the smallest mainstream live distribution (~350 MiB), it
# boots straight to a root shell with no installer and no prompts, and it has
# a documented unattended hook (the apkovl local overlay) that needs no
# remastering of the ISO's squashfs.  A Debian or Ubuntu live image would need
# its squashfs rebuilt to run anything unattended, and is several times
# larger.
#
# Usage:
#   scripts/ci/n305-capture-image.sh --out /path/to/n305-capture.img
#
# Build dependencies (all unprivileged; no loop devices, no root):
#   curl or wget, sha256sum, sfdisk, mkfs.vfat, mtools (mcopy/mmd),
#   truncate, tar, gzip, python3, and 7z/7zr/bsdtar to read the ISO's own
#   package index.
#
# This script does not write to a USB device.  It produces an image file; see
# docs/design/n305-bringup.md for the two commands that put it on a stick.

set -euo pipefail

ALPINE_VERSION_DEFAULT="3.24.1"
ALPINE_SHA256_DEFAULT="f4dd613206676c62949144c8ad75fc64582099f444dd1485bae104a60f51dd26"
ALPINE_BRANCH_DEFAULT="v3.24"
HOST_DEFAULT="https://dl-cdn.alpinelinux.org/alpine"

# The offline tool set.  Everything here is either required by the capture
# brief or is the closest available substitute for something that is not
# packaged for this Alpine release (see the "tooling gaps" note in the
# runbook): there is no edid-decode and no intel-gpu-tools in v3.24, so the
# bundle carries raw EDID and i915's own debugfs state instead.
TOOLS="pciutils acpica dmidecode util-linux-misc libdrm-tests drm_info linux-firmware-i915"
TOOLS_NO_FIRMWARE="pciutils acpica dmidecode util-linux-misc libdrm-tests drm_info"

MARKER_NAME="N305-CAPTURE-MARKER.txt"
MARKER_TEXT="thekernel-n305-capture-v1"

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)

OUT=""
CACHE="${HOME}/.cache/thekernel-n305-capture"
ISO=""
PAYLOAD="$SCRIPT_DIR/n305-capture-payload.sh"
PART_MIB=256
ALPINE_VERSION="$ALPINE_VERSION_DEFAULT"
ALPINE_SHA256="$ALPINE_SHA256_DEFAULT"
ALPINE_BRANCH="$ALPINE_BRANCH_DEFAULT"
HOST_URL="$HOST_DEFAULT"
WITH_FIRMWARE=yes
ASSEMBLE_ONLY=no
KEEP_INTERMEDIATE=no

die() {
	printf 'n305-capture-image: %s\n' "$*" >&2
	exit 1
}

note() {
	printf 'n305-capture-image: %s\n' "$*"
}

usage() {
	sed -n '2,48p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
	exit "${1:-0}"
}

while [ $# -gt 0 ]; do
	case "$1" in
	--out)
		OUT="$2"
		shift 2
		;;
	--cache)
		CACHE="$2"
		shift 2
		;;
	--iso)
		ISO="$2"
		shift 2
		;;
	--payload)
		PAYLOAD="$2"
		shift 2
		;;
	--part-mib)
		PART_MIB="$2"
		shift 2
		;;
	--alpine-version)
		ALPINE_VERSION="$2"
		ALPINE_SHA256=""
		shift 2
		;;
	--alpine-sha256)
		ALPINE_SHA256="$2"
		shift 2
		;;
	--alpine-branch)
		ALPINE_BRANCH="$2"
		shift 2
		;;
	--host)
		HOST_URL="$2"
		shift 2
		;;
	--no-firmware)
		WITH_FIRMWARE=no
		shift
		;;
	--assemble-only)
		ASSEMBLE_ONLY=yes
		shift
		;;
	--keep)
		KEEP_INTERMEDIATE=yes
		shift
		;;
	-h | --help)
		usage 0
		;;
	*)
		die "unknown argument: $1 (try --help)"
		;;
	esac
done

[ -n "$OUT" ] || die "--out is required"
OUT=$(readlink -m "$OUT")
case "$OUT" in
/tmp/* | /dev/shm/*) die "--out must not be under /tmp or /dev/shm" ;;
esac
mkdir -p "$CACHE/downloads" "$(dirname -- "$OUT")"
CACHE=$(readlink -m "$CACHE")
[ -f "$PAYLOAD" ] || die "payload script not found: $PAYLOAD"
[ "$PART_MIB" -ge 128 ] || die "--part-mib must be at least 128"

WORK="$CACHE/work"
mkdir -p "$WORK/apks" "$WORK/apkovl" "$WORK/fat"

# ---------------------------------------------------------------------------
# Build dependencies.
# ---------------------------------------------------------------------------

missing=()
for tool in curl sha256sum sfdisk mkfs.vfat mcopy mmd truncate tar gzip python3 dd cmp od install; do
	command -v "$tool" >/dev/null 2>&1 || missing+=("$tool")
done
if [ "${#missing[@]}" -gt 0 ]; then
	die "missing build dependencies: ${missing[*]}"
fi
if [ "$ASSEMBLE_ONLY" = no ]; then
	# Any one of these can unpack the ISO's embedded APKINDEX, which is how
	# the already-installed package set is determined exactly.
	ISO_EXTRACTOR=""
	for candidate in 7z 7zr 7za bsdtar; do
		if command -v "$candidate" >/dev/null 2>&1; then
			ISO_EXTRACTOR="$candidate"
			break
		fi
	done
fi

fetch() { # fetch <url> <destination>; returns non-zero if unavailable
	local url=$1 destination=$2
	if [ -s "$destination" ]; then
		return 0
	fi
	note "downloading ${url##*/}"
	if ! curl --fail --location --silent --show-error --retry 3 --output "$destination.part" "$url"; then
		rm -f "$destination.part"
		return 1
	fi
	mv "$destination.part" "$destination"
}

fetch_or_die() {
	fetch "$@" || die "download failed: $1"
}

# ---------------------------------------------------------------------------
# The Alpine live ISO, verified against the checksum published beside it.
# ---------------------------------------------------------------------------

ISO_NAME="alpine-standard-${ALPINE_VERSION}-x86_64.iso"
if [ -z "$ISO" ]; then
	ISO="$CACHE/downloads/$ISO_NAME"
	if [ ! -s "$ISO" ]; then
		mkdir -p "$CACHE/downloads"
		fetch_or_die "$HOST_URL/$ALPINE_BRANCH/releases/x86_64/$ISO_NAME" "$ISO"
	fi
else
	ISO=$(readlink -m "$ISO")
	[ -s "$ISO" ] || die "ISO not found: $ISO"
fi
[ -s "$ISO" ] || die "ISO not available: $ISO (use --assemble-only only with a populated cache)"

note "verifying $ISO_NAME"
published="$CACHE/downloads/$ISO_NAME.sha256"
if fetch "$HOST_URL/$ALPINE_BRANCH/releases/x86_64/$ISO_NAME.sha256" "$published"; then
	expected=$(awk '{print $1}' "$published")
	actual=$(sha256sum "$ISO" | awk '{print $1}')
	[ "$expected" = "$actual" ] || die "ISO checksum mismatch: published $expected, file $actual"
	if [ -n "$ALPINE_SHA256" ] && [ "$ALPINE_SHA256" != "$actual" ]; then
		die "ISO does not match the pinned checksum for Alpine $ALPINE_VERSION; the release was rebuilt or moved"
	fi
	note "ISO checksum matches the published value"
else
	[ -n "$ALPINE_SHA256" ] || die "cannot read the published checksum and no --alpine-sha256 was given"
	actual=$(sha256sum "$ISO" | awk '{print $1}')
	[ "$ALPINE_SHA256" = "$actual" ] || die "ISO checksum mismatch: pinned $ALPINE_SHA256, file $actual"
	note "ISO checksum matches the pinned value (published checksum unreachable)"
fi

# ---------------------------------------------------------------------------
# Offline tool set: resolve from the release branch that matches the ISO.
# ---------------------------------------------------------------------------

if [ "$ASSEMBLE_ONLY" = no ]; then
	note "resolving the offline tool set for $ALPINE_BRANCH"
	for repo in main community; do
		fetch_or_die "$HOST_URL/$ALPINE_BRANCH/$repo/x86_64/APKINDEX.tar.gz" "$CACHE/APKINDEX-$repo.tar.gz"
	done

	installed_list=""
	if [ -n "$ISO_EXTRACTOR" ]; then
		if [ "$ISO_EXTRACTOR" = bsdtar ]; then
			bsdtar -xOf "$ISO" apks/x86_64/APKINDEX.tar.gz > "$CACHE/APKINDEX-iso.tar.gz" 2>/dev/null || true
		else
			"$ISO_EXTRACTOR" e -y -o"$CACHE" "$ISO" apks/x86_64/APKINDEX.tar.gz >/dev/null 2>&1 || true
			[ -f "$CACHE/APKINDEX.tar.gz" ] && mv -f "$CACHE/APKINDEX.tar.gz" "$CACHE/APKINDEX-iso.tar.gz"
		fi
		[ -s "$CACHE/APKINDEX-iso.tar.gz" ] && installed_list="$CACHE/APKINDEX-iso.tar.gz"
	fi
	if [ -z "$installed_list" ]; then
		note "WARNING: no 7z/bsdtar to read the ISO's package index; the live system's"
		note "         installed set is approximated from the CDN index instead."
		note "         Install p7zip (or bsdtar) for an exact dependency closure."
	fi

	if [ "$WITH_FIRMWARE" = yes ]; then
		wanted="$TOOLS"
	else
		wanted="$TOOLS_NO_FIRMWARE"
	fi

	# Resolve the dependency closure, excluding anything the live environment
	# already installs, and print one "<repo>/<file>.apk" per line.
	python3 - "$CACHE" "$wanted" "$installed_list" > "$WORK/apk-list.txt" <<'PY'
import sys, tarfile

cache, wanted, installed_index = sys.argv[1], sys.argv[2].split(), sys.argv[3]

def parse(path, tag):
    with tarfile.open(path) as archive:
        member = [n for n in archive.getnames() if n.endswith("APKINDEX")][0]
        text = archive.extractfile(member).read().decode()
    packages = {}
    for block in text.strip().split("\n\n"):
        entry = {}
        for line in block.splitlines():
            if len(line) > 2 and line[1] == ":":
                entry[line[0]] = line[2:]
        if "P" in entry:
            entry["_repo"] = tag
            packages[entry["P"]] = entry
    return packages

index = {}
for repo in ("main", "community"):
    for name, entry in parse(f"{cache}/APKINDEX-{repo}.tar.gz", repo).items():
        index.setdefault(name, entry)
providers = {}
for name, entry in index.items():
    for token in entry.get("p", "").split():
        providers.setdefault(token.split("=")[0], []).append(name)

def dep_name(token):
    if token.startswith("!"):
        return None
    for separator in (">=", "<=", "=", "~", ">", "<"):
        if separator in token:
            return token.split(separator, 1)[0]
    return token

def closure(roots, pool, installed=frozenset()):
    """Every package the roots need, resolving so:/cmd:/path dependencies."""
    chosen, seen, unknown, queue = [], set(), [], list(roots)
    while queue:
        name = queue.pop(0)
        if name in seen:
            continue
        seen.add(name)
        if name in installed:
            continue
        if name in pool:
            package = name
        else:
            candidates = [c for c in providers.get(name, []) if c in pool]
            if not candidates:
                unknown.append(name)
                continue
            if any(c in installed for c in candidates):
                continue  # the live environment already provides this
            package = sorted(candidates, key=lambda n: (len(n), n))[0]
        if package in installed or package in chosen:
            continue
        chosen.append(package)
        for token in pool[package].get("D", "").split():
            dependency = dep_name(token)
            if dependency:
                queue.append(dependency)
    return chosen, unknown

installed = frozenset()
if installed_index:
    installed = frozenset(closure(["alpine-base"], parse(installed_index, "iso"))[0])

chosen, unknown = closure(wanted, index, installed)
if unknown:
    sys.stderr.write("unresolvable dependencies: %s\n" % ", ".join(sorted(set(unknown))))
    raise SystemExit(1)
for name in sorted(chosen):
    entry = index[name]
    sys.stdout.write("%s/%s-%s.apk\n" % (entry["_repo"], name, entry["V"]))
PY

	while read -r package; do
		[ -n "$package" ] || continue
		repo=${package%%/*}
		file=${package##*/}
		fetch_or_die "$HOST_URL/$ALPINE_BRANCH/$repo/x86_64/$file" "$WORK/apks/$file"
		# A truncated or HTML-error download is worse than a missing one.
		tar -tzf "$WORK/apks/$file" >/dev/null 2>&1 || die "downloaded package is not a valid .apk: $file"
	done < "$WORK/apk-list.txt"
	note "offline tool set: $(wc -l < "$WORK/apk-list.txt" | tr -d ' ') packages, $(du -sh "$WORK/apks" | cut -f1)"
	{
		printf '# packages vendored into this image from %s/%s\n' "$HOST_URL" "$ALPINE_BRANCH"
		(cd "$WORK/apks" && sha256sum ./*.apk)
	} > "$WORK/apks/MANIFEST.sha256"
fi

[ -s "$WORK/apks/MANIFEST.sha256" ] || die "no packages staged; run without --assemble-only first"

# ---------------------------------------------------------------------------
# The apkovl: Alpine's documented unattended hook.
# ---------------------------------------------------------------------------

note "building alpine.apkovl.tar.gz"
rm -rf "$WORK/apkovl"
mkdir -p "$WORK/apkovl/etc/local.d" "$WORK/apkovl/etc/runlevels/default"
# Without this, the initramfs skips the default boot services entirely
# because an apkovl was supplied.
: > "$WORK/apkovl/etc/.default_boot_services"
install -m 0755 "$PAYLOAD" "$WORK/apkovl/etc/local.d/n305-capture.start"
ln -sf /etc/init.d/local "$WORK/apkovl/etc/runlevels/default/local"
(
	cd "$WORK/apkovl"
	tar --numeric-owner --owner=0 --group=0 -czf "$WORK/alpine.apkovl.tar.gz" \
		etc/.default_boot_services etc/local.d etc/runlevels
)
tar -tzf "$WORK/alpine.apkovl.tar.gz" > "$WORK/apkovl-contents.txt"
grep -q 'etc/local.d/n305-capture.start' "$WORK/apkovl-contents.txt" ||
	die "apkovl does not contain the capture script"

# ---------------------------------------------------------------------------
# Payload partition: marker, apkovl, offline packages, readable payload copy.
# ---------------------------------------------------------------------------

iso_bytes=$(stat -c %s "$ISO")
sector=512
iso_sectors=$(((iso_bytes + sector - 1) / sector))
# The vendor ISO carries a GPT as well as the hybrid MBR (xorriso's
# -isohybrid-gpt-basdat), so the new partition has to be written into both
# tables: which one Linux or a firmware prefers is not something to guess at.
# A GPT spends its last sectors on the backup header and entry array, and the
# partition has to stop short of them.
gpt_reserved=$(python3 - "$ISO" <<'GPTEOF'
import struct, sys
with open(sys.argv[1], "rb") as iso:
    iso.seek(512 + 72)
    _entries_lba, count, size, _crc = struct.unpack("<QIII", iso.read(20))
print(1 + (count * size + 511) // 512)
GPTEOF
)
part_sectors=$((PART_MIB * 1024 * 1024 / sector - gpt_reserved))

note "building the payload filesystem (${PART_MIB} MiB partition less the backup GPT)"
rm -f "$WORK/payload.fat"
truncate -s "$((part_sectors * sector))" "$WORK/payload.fat"
mkfs.vfat -F 32 -n HWDUMP "$WORK/payload.fat" > /dev/null
mmd -i "$WORK/payload.fat" ::/apks ::/payload ::/dump
mcopy -i "$WORK/payload.fat" "$WORK/alpine.apkovl.tar.gz" ::/alpine.apkovl.tar.gz
mcopy -i "$WORK/payload.fat" "$WORK/apks/MANIFEST.sha256" ::/apks/MANIFEST.sha256
mcopy -i "$WORK/payload.fat" "$WORK/apks/"*.apk ::/apks/
mcopy -i "$WORK/payload.fat" "$PAYLOAD" ::/payload/n305-capture-payload.sh
printf '%s\n' "$MARKER_TEXT" > "$WORK/$MARKER_NAME"
mcopy -i "$WORK/payload.fat" "$WORK/$MARKER_NAME" ::/"$MARKER_NAME"

cat > "$WORK/README.txt" <<EOF
TheKernel N305 hardware-facts capture stick
===========================================

Boot the target machine from this stick.  Do not type anything: the capture
starts by itself, writes a bundle under dump/ on this partition, and then
powers the machine off.  A machine that stays switched on has hit an error and
is showing it on the screen.

Alpine Linux live ${ALPINE_VERSION} (unmodified vendor boot path).
Capture payload: payload/n305-capture-payload.sh
Offline tools:  apks/ ($(wc -l < "$WORK/apk-list.txt" 2>/dev/null || echo 0) packages, see apks/MANIFEST.sha256)

What comes back: dump/n305-<timestamp>/ (and a .tar.gz beside it) containing
the ACPI tables and their decode, /proc/iomem, /proc/ioports, every PCI
device's configuration space and lspci -vvv -nn, CPU topology, the EDID of
every connected display, dmesg, and the kernel's view of the integrated GPU.
dump/n305-<timestamp>/README.txt says what could not be captured and why.

This machine has no serial port.  The screen is its only output, which is why
the payload reports on screen and finishes by powering off.
EOF
mcopy -i "$WORK/payload.fat" "$WORK/README.txt" ::/README.txt

mdir -i "$WORK/payload.fat" ::/ > "$WORK/payload-listing.txt"

# ---------------------------------------------------------------------------
# Assemble: vendor ISO, plus one appended partition, and nothing else.
# ---------------------------------------------------------------------------

note "assembling $OUT"
rm -f "$OUT"
cp --reflink=auto "$ISO" "$OUT"
truncate -s $((iso_bytes + PART_MIB * 1024 * 1024)) "$OUT"
# Start after the ISO's own extent, never where sfdisk would put it: a dd'ed
# hybrid ISO declares only the ISO and its embedded ESP, so an automatic
# append would land inside the ISO9660 filesystem.
printf '%s,%s,0c\n' "$iso_sectors" "$part_sectors" | sfdisk --append --no-reread "$OUT" > /dev/null
python3 - "$OUT" "$iso_sectors" "$part_sectors" <<'GPTEOF'
"""Add the payload partition to the ISO's GPT, leaving the ESP entry alone."""
import os, struct, sys, zlib

image, start, sectors = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
SECTOR = 512
DATA_GUID = bytes.fromhex("a2a0d0ebe5b9334487c068b6b72699c7")  # Microsoft basic data
# Fixed rather than random so two builds of the same inputs are identical.
UNIQUE_GUID = bytes.fromhex("4e9f1c7a2d3b584c9a614f0d2e5b8c31")


def header_crc(raw):
    return zlib.crc32(bytes(raw[:16]) + b"\0\0\0\0" + bytes(raw[20:92])) & 0xFFFFFFFF


with open(image, "r+b") as disk:
    # The disk is the vendor ISO plus the payload partition plus the 63 sectors
    # a GPT reserves for its backup header and entry array at the very end.
    total = os.path.getsize(image) // SECTOR
    disk.seek(SECTOR)
    header = bytearray(disk.read(92))
    if header[:8] != b"EFI PART":
        raise SystemExit("assembled image has no GPT header where one was expected")
    if header_crc(header) != struct.unpack_from("<I", header, 16)[0]:
        raise SystemExit("GPT header checksum is already invalid; refusing to edit it")
    entries_lba, count, size = struct.unpack_from("<QII", header, 72)
    disk.seek(entries_lba * SECTOR)
    entries = bytearray(disk.read(count * size))
    if zlib.crc32(bytes(entries)) & 0xFFFFFFFF != struct.unpack_from("<I", header, 88)[0]:
        raise SystemExit("GPT entry array checksum is already invalid; refusing to edit it")

    reserved = 1 + (count * size + SECTOR - 1) // SECTOR
    last_lba = total - 1
    last_usable = last_lba - reserved
    end = start + sectors - 1
    if end > last_usable:
        raise SystemExit("payload partition overlaps the backup GPT area")

    slot = None
    for index in range(count):
        if entries[index * size:index * size + 16] == b"\0" * 16:
            slot = index
            break
    if slot is None:
        raise SystemExit("GPT has no free partition entry")
    entry = bytearray(size)
    entry[0:16] = DATA_GUID
    entry[16:32] = UNIQUE_GUID
    struct.pack_into("<QQQ", entry, 32, start, end, 0)
    name = "N305CAPTURE".encode("utf-16-le")
    entry[56:56 + len(name)] = name
    entries[slot * size:(slot + 1) * size] = entry

    entries_crc = zlib.crc32(bytes(entries)) & 0xFFFFFFFF
    disk.seek(entries_lba * SECTOR)
    disk.write(entries)

    def build_header(my_lba, alternate_lba):
        out = bytearray(header)
        struct.pack_into("<Q", out, 24, my_lba)
        struct.pack_into("<Q", out, 32, alternate_lba)
        struct.pack_into("<Q", out, 40, 64)
        struct.pack_into("<Q", out, 48, last_usable)
        struct.pack_into("<I", out, 88, entries_crc)
        struct.pack_into("<I", out, 16, 0)
        struct.pack_into("<I", out, 16, header_crc(out))
        return out

    disk.seek(SECTOR)
    disk.write(build_header(1, last_lba))
    backup_entries_lba = last_lba - (count * size + SECTOR - 1) // SECTOR
    disk.seek(backup_entries_lba * SECTOR)
    disk.write(entries)
    disk.seek(last_lba * SECTOR)
    disk.write(build_header(last_lba, 1))
    disk.flush()
print("gpt: partition %d..%d recorded in entry %d" % (start, end, slot))
GPTEOF
dd if="$WORK/payload.fat" of="$OUT" bs=512 seek="$iso_sectors" conv=notrunc status=none

# ---------------------------------------------------------------------------
# Verify what was built.
# ---------------------------------------------------------------------------

note "verifying the image"
# 1. The vendor ISO region is unchanged except for the two partition tables
#    that had to be extended: the MBR table at 446..511, and the GPT header and
#    entry array at the front of the disk.  Everything from the start of the
#    ISO9660 filesystem to the end of the vendor image must be byte-identical,
#    which is what makes reusing the vendor boot path trustworthy.
gpt_end=$(python3 - "$ISO" <<'GPTEOF'
import struct, sys
with open(sys.argv[1], "rb") as iso:
    iso.seek(512 + 72)
    entries_lba, count, size, _crc = struct.unpack("<QIII", iso.read(20))
print(entries_lba * 512 + count * size)
GPTEOF
)
cmp -n 446 "$ISO" "$OUT" > /dev/null || die "vendor boot code changed during assembly"
cmp -n "$((iso_bytes - gpt_end))" -i "$gpt_end:$gpt_end" "$ISO" "$OUT" > /dev/null ||
	die "vendor ISO contents changed during assembly"
python3 - "$OUT" "$iso_sectors" <<'GPTEOF'
"""Fail unless both partition tables agree that the payload partition exists."""
import struct, sys, zlib

image, start = sys.argv[1], int(sys.argv[2])
SECTOR = 512


def header_crc(raw):
    return zlib.crc32(bytes(raw[:16]) + b"\0\0\0\0" + bytes(raw[20:92])) & 0xFFFFFFFF


with open(image, "rb") as disk:
    disk.seek(SECTOR)
    header = disk.read(92)
    assert header[:8] == b"EFI PART", "GPT header missing"
    assert header_crc(header) == struct.unpack_from("<I", header, 16)[0], "GPT header CRC invalid"
    entries_lba, count, size = struct.unpack_from("<QII", header, 72)
    disk.seek(entries_lba * SECTOR)
    entries = disk.read(count * size)
    assert zlib.crc32(entries) & 0xFFFFFFFF == struct.unpack_from("<I", header, 88)[0], \
        "GPT entry array CRC invalid"
    found = []
    for index in range(count):
        raw = entries[index * size:(index + 1) * size]
        if raw[:16] == b"\0" * 16:
            continue
        first, last = struct.unpack_from("<QQ", raw, 32)
        found.append((first, last, raw[56:128].decode("utf-16-le").rstrip("\0")))
    assert any(item[0] == start and item[2] == "N305CAPTURE" for item in found), \
        "GPT does not describe the payload partition: %r" % (found,)
    assert any(item[2] == "ISOHybrid1" for item in found), "GPT lost the vendor ESP entry"
    disk.seek(struct.unpack_from("<Q", header, 32)[0] * SECTOR)
    backup = disk.read(92)
    assert backup[:8] == b"EFI PART", "backup GPT header missing"
    assert header_crc(backup) == struct.unpack_from("<I", backup, 16)[0], \
        "backup GPT header CRC invalid"
print("both partition tables carry the payload partition")
GPTEOF

# 2. The partition table says what it should.
fdisk -l "$OUT" > "$WORK/image-partitions.txt" 2>&1 || die "cannot read the assembled partition table"
sfdisk --dump "$OUT" > "$WORK/image-sfdisk.txt" 2>&1 || die "cannot dump the assembled partition table"
grep -q 'W95 FAT32' "$WORK/image-partitions.txt" || die "payload partition is missing from the image"

# 3. The payload partition really is readable and complete.
mdir -i "$OUT@@$((iso_sectors * sector))" ::/ > "$WORK/verify-listing.txt" 2>&1 ||
	die "cannot read the payload partition back out of the image"
# Read the files back out of the assembled image rather than trusting a
# directory listing: FAT shows an 8.3 name like README.txt as "README   txt",
# so a listing-based check would be checking the wrong thing.
offset="$((iso_sectors * sector))"
mcopy -i "$OUT@@$offset" "::/$MARKER_NAME" - | grep -qx "$MARKER_TEXT" ||
	die "payload marker file is missing or wrong in the assembled image"
mcopy -i "$OUT@@$offset" ::/README.txt - | grep -q 'TheKernel N305 hardware-facts capture stick' ||
	die "payload README is missing from the assembled image"
mcopy -i "$OUT@@$offset" ::/alpine.apkovl.tar.gz - | tar -tzf - |
	grep -q 'etc/local.d/n305-capture.start' || die "apkovl in the image is not the one that was built"
mcopy -i "$OUT@@$offset" "::/apks/MANIFEST.sha256" - | grep -q 'pciutils' ||
	die "offline package set is missing from the assembled image"

rm -rf "$WORK/fat"
if [ "$KEEP_INTERMEDIATE" = no ]; then
	rm -rf "$WORK/apkovl" "$WORK/payload.fat"
fi

note "image: $OUT"
note "size:  $(du -h "$OUT" | cut -f1) ($(stat -c %s "$OUT") bytes)"
note "sha256: $(sha256sum "$OUT" | awk '{print $1}')"
note "partitions:"
sed 's/^/    /' "$WORK/image-partitions.txt" | tail -5
note ""
note "write it to a USB stick of at least 1 GiB, then boot the N305 from it:"
note "    sudo dd if=$OUT of=/dev/sdX bs=4M conv=fsync status=progress"
note "    sync"
note "The payload contents are listed in $WORK/payload-listing.txt"
