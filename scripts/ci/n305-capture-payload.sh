#!/bin/sh
# N305 ground-truth capture payload.
#
# Runs unattended, as root, inside the Alpine live environment that
# scripts/ci/n305-capture-image.sh builds, started by the OpenRC "local"
# service.  It needs no console interaction: it writes one self-describing
# bundle to the USB stick it booted from and then powers the machine off, so
# "the machine turned itself off" is the success signal a screen-only machine
# can give the person standing next to it.  A machine that is still on is
# showing an error on the screen.
#
# What it is for: replacing TheKernel's assumptions about this machine with
# facts.  The headline fact is the PCI ECAM base, which the kernel currently
# takes from configuration; the bundle carries the raw ACPI MCFG table, an
# independent decode of the same bytes done here with od/awk, and the kernel's
# own ECAM line from dmesg, so the value can be corroborated three ways.
#
# Honesty rules, because this output will be trusted:
#   * every probe records OK / FAIL / UNAVAILABLE in capture-status.txt;
#   * a probe whose tool is missing writes an UNAVAILABLE file instead of
#     silently producing an empty one;
#   * anything that cannot be captured at all is named in README.txt with the
#     reason (see the "What this bundle cannot contain" section).
#
# This must run under BusyBox ash: POSIX sh only, no bashisms, and no `set -e`
# because one failing probe must never abandon the rest of the bundle.

set -u

PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
export PATH

PAYLOAD_VERSION="1"
MARKER_NAME="N305-CAPTURE-MARKER.txt"
MARKER_TEXT="thekernel-n305-capture-v1"
PROBE_TIMEOUT=300
BUNDLE_MIB_LIMIT=64

# ---------------------------------------------------------------------------
# Console reporting.  The screen is this machine's only output channel, so
# every milestone goes there as well as to the bundle.
# ---------------------------------------------------------------------------

SCREEN=""
for candidate in /dev/tty0 /dev/tty1 /dev/console; do
	if [ -w "$candidate" ]; then
		SCREEN="$candidate"
		break
	fi
done

say() {
	printf 'n305-capture: %s\n' "$*"
	if [ -n "$SCREEN" ]; then
		printf 'n305-capture: %s\n' "$*" > "$SCREEN" 2>/dev/null
	fi
	return 0
}

fatal() {
	say "FAILED: $*"
	say "the bundle was not completed; leaving the machine on so this message stays visible"
	return 1
}

bounded() {
	if command -v timeout >/dev/null 2>&1; then
		timeout "$PROBE_TIMEOUT" "$@"
	else
		"$@"
	fi
}

# ---------------------------------------------------------------------------
# Locate the payload partition.  The initramfs already mounted it read-only
# while looking for alpine.apkovl.tar.gz, which is how this script got here;
# it is identified by the marker file rather than by device name because the
# device name is not knowable in advance (sda3, nvme0n1p3, ...).
# ---------------------------------------------------------------------------

device_by_label() {
	_dev=""
	if command -v blkid >/dev/null 2>&1; then
		_dev=$(blkid -L "$1" 2>/dev/null)
	fi
	if [ -z "$_dev" ] && command -v findfs >/dev/null 2>&1; then
		_dev=$(findfs "LABEL=$1" 2>/dev/null)
	fi
	if [ -n "$_dev" ]; then
		printf '%s' "$_dev"
		return 0
	fi
	return 1
}

locate_payload() {
	for dir in /media/*; do
		if [ -f "$dir/$MARKER_NAME" ] && grep -q "$MARKER_TEXT" "$dir/$MARKER_NAME" 2>/dev/null; then
			printf '%s' "$dir"
			return 0
		fi
	done
	dev=$(device_by_label HWDUMP) || return 1
	mkdir -p /mnt/n305-dump
	mount -t vfat -o rw "$dev" /mnt/n305-dump 2>/dev/null || return 1
	printf '%s' /mnt/n305-dump
	return 0
}

MNT=$(locate_payload) || {
	fatal "no partition carrying $MARKER_NAME (the USB payload partition)"
	exit 1
}
case "$MNT" in
/media/*) mount -o remount,rw "$MNT" 2>/dev/null ;;
esac
if ! touch "$MNT/.n305-write-test" 2>/dev/null; then
	fatal "the payload partition $MNT is not writable"
	exit 1
fi
rm -f "$MNT/.n305-write-test"

STAMP=$(date -u +%Y%m%dT%H%M%SZ)
OUT="$MNT/dump/n305-$STAMP"
mkdir -p "$OUT" || {
	fatal "cannot create $OUT"
	exit 1
}
STATUS="$OUT/capture-status.txt"
: > "$STATUS"
say "writing bundle to $OUT"

record() {
	printf '%s\t%s\t%s\n' "$1" "$2" "${3:-}" >> "$STATUS"
}

# ---------------------------------------------------------------------------
# Probe helpers.  Each one keeps the capture going whatever happens and leaves
# a truthful record of what did not work.
# ---------------------------------------------------------------------------

cap() { # cap <relative-path> <command> [args...]
	_rel=$1
	shift
	_out="$OUT/$_rel"
	mkdir -p "$(dirname "$_out")" 2>/dev/null
	if ! command -v "$1" >/dev/null 2>&1; then
		printf 'UNAVAILABLE: %s is not installed in this live environment\n' "$1" > "$_out"
		record UNAVAILABLE "$_rel" "missing tool: $1"
		return 0
	fi
	if bounded "$@" > "$_out" 2> "$_out.stderr"; then
		record OK "$_rel" "$(wc -c < "$_out" | tr -d ' ') bytes"
	else
		record FAIL "$_rel" "exit $? running: $*"
	fi
	[ -s "$_out.stderr" ] || rm -f "$_out.stderr"
	return 0
}

cap_sh() { # cap_sh <relative-path> <shell command text>
	_rel=$1
	_text=$2
	_out="$OUT/$_rel"
	mkdir -p "$(dirname "$_out")" 2>/dev/null
	if bounded sh -c "$_text" > "$_out" 2> "$_out.stderr"; then
		record OK "$_rel" "$(wc -c < "$_out" | tr -d ' ') bytes"
	else
		record FAIL "$_rel" "exit $? running: $_text"
	fi
	[ -s "$_out.stderr" ] || rm -f "$_out.stderr"
	return 0
}

cap_file() { # cap_file <relative-path> <source path>
	_rel=$1
	_src=$2
	_out="$OUT/$_rel"
	mkdir -p "$(dirname "$_out")" 2>/dev/null
	if [ ! -e "$_src" ]; then
		printf 'UNAVAILABLE: %s does not exist on this machine\n' "$_src" > "$_out"
		record UNAVAILABLE "$_rel" "absent: $_src"
		return 0
	fi
	if bounded cp "$_src" "$_out" 2> "$_out.stderr"; then
		record OK "$_rel" "$(wc -c < "$_out" | tr -d ' ') bytes"
	else
		record FAIL "$_rel" "exit $? copying $_src"
	fi
	[ -s "$_out.stderr" ] || rm -f "$_out.stderr"
	return 0
}

copy_flat() { # copy the regular files of one directory, never following links
	_from=$1
	_to=$2
	_copied=0
	_failed=0
	for _entry in "$_from"/*; do
		# -f follows symlinks, so a link to a directory is skipped rather
		# than recursed into, and no symlink is ever created: the bundle
		# lives on FAT, which cannot hold one.
		[ -f "$_entry" ] || continue
		if cp "$_entry" "$_to/$(basename "$_entry")" 2>/dev/null; then
			_copied=$((_copied + 1))
		else
			printf 'could not copy %s\n' "$_entry" >&2
			_failed=$((_failed + 1))
		fi
	done
	printf '%s %s' "$_copied" "$_failed"
}

cap_tree() { # cap_tree <relative-directory> <source directory>
	_rel=$1
	_src=$2
	_out="$OUT/$_rel"
	if [ ! -d "$_src" ]; then
		mkdir -p "$(dirname "$_out")" 2>/dev/null
		printf 'UNAVAILABLE: %s is not a directory on this machine\n' "$_src" > "$_out.absent"
		record UNAVAILABLE "$_rel" "absent: $_src"
		return 0
	fi
	mkdir -p "$_out" 2>/dev/null
	# shellcheck disable=SC2046  # copy_flat prints exactly two counts.
	set -- $(copy_flat "$_src" "$_out" 2> "$_out.failed")
	copied=$1
	failed=$2
	# One level of subdirectories, which is all these trees have (the ACPI
	# table directory carries dynamic/).
	for subdir in "$_src"/*/; do
		[ -d "$subdir" ] || continue
		[ -L "${subdir%/}" ] && continue
		mkdir -p "$_out/$(basename "$subdir")" 2>/dev/null
		# shellcheck disable=SC2046  # copy_flat prints exactly two counts.
		set -- $(copy_flat "$subdir" "$_out/$(basename "$subdir")" 2>> "$_out.failed")
		copied=$((copied + $1))
		failed=$((failed + $2))
	done
	if [ "$failed" = "0" ]; then
		rm -f "$_out.failed"
		record OK "$_rel" "$copied files"
	else
		record FAIL "$_rel" "$failed of $((copied + failed)) files could not be copied; see $(basename "$_out").failed"
	fi
	return 0
}

cap_dd() { # cap_dd <relative-path> <source> <max MiB>
	_rel=$1
	_src=$2
	_limit=$3
	_out="$OUT/$_rel"
	mkdir -p "$(dirname "$_out")" 2>/dev/null
	if [ ! -r "$_src" ]; then
		printf 'UNAVAILABLE: %s is not readable\n' "$_src" > "$_out"
		record UNAVAILABLE "$_rel" "unreadable: $_src"
		return 0
	fi
	if bounded dd if="$_src" of="$_out" bs=1M count="$_limit" 2> "$_out.stderr"; then
		record OK "$_rel" "$(wc -c < "$_out" | tr -d ' ') bytes"
	else
		_status=$?
		record FAIL "$_rel" "exit $_status reading $_src: $(head -c 120 "$_out.stderr" | tr '\n' ' ')"
	fi
	return 0
}

record_file() { # record_file <relative-path> <status> <detail>
	printf '%s\n' "$3" > "$OUT/$1"
	record "$2" "$1" "$3"
	return 0
}

# ---------------------------------------------------------------------------
# Stage 0: install the offline tool set carried on the payload partition.
# The live environment ships no lspci, no acpidump, no dmidecode and no lscpu,
# and this machine may have no network at all, so the tools travel with the
# stick and are installed from local files.  Packages are tried with signature
# verification first; the unverified fallback is recorded rather than hidden.
# ---------------------------------------------------------------------------

say "stage 0/9: installing the offline tool set"
mkdir -p "$OUT/meta"
if [ ! -d "$MNT/apks" ]; then
	record_file meta/tool-install.txt UNAVAILABLE \
		"the payload partition has no apks/ directory, so no tools were installed"
elif [ -z "$(ls "$MNT"/apks/*.apk 2>/dev/null)" ]; then
	record_file meta/tool-install.txt UNAVAILABLE "the payload partition carries no .apk files"
elif ! command -v apk >/dev/null 2>&1; then
	record_file meta/tool-install.txt UNAVAILABLE "apk is not present in this live environment"
else
	# shellcheck disable=SC2086  # the package list must be split into words.
	set -- "$MNT"/apks/*.apk
	if apk add --quiet "$@" > "$OUT/meta/tool-install.txt" 2>&1; then
		record OK meta/tool-install.txt "installed $# packages, signatures verified"
	elif apk add --quiet --allow-untrusted "$@" >> "$OUT/meta/tool-install.txt" 2>&1; then
		printf '\ninstalled with --allow-untrusted after signature verification failed\n' \
			>> "$OUT/meta/tool-install.txt"
		record WARNING meta/tool-install.txt "installed $# packages WITHOUT signature verification"
	else
		record FAIL meta/tool-install.txt "apk could not install the offline tool set"
	fi
fi

# ---------------------------------------------------------------------------
# Prologue: provenance and tool availability.
# ---------------------------------------------------------------------------

say "stage 1/9: provenance"
mkdir -p "$OUT/meta"
{
	printf 'payload version: %s\n' "$PAYLOAD_VERSION"
	printf 'capture start (UTC): %s\n' "$(date -u)"
	printf 'payload sha256: %s\n' "$(sha256sum "$0" 2>/dev/null | cut -d' ' -f1)"
	printf 'mount point: %s\n' "$MNT"
	printf 'bundle: %s\n' "$OUT"
} > "$OUT/meta/payload.txt"
cap meta/uname.txt uname -a
cap meta/cmdline.txt cat /proc/cmdline
cap meta/os-release.txt cat /etc/os-release
cap meta/alpine-release.txt cat /etc/alpine-release
cap meta/mounts.txt cat /proc/mounts
cap meta/uptime.txt cat /proc/uptime
cap meta/partitions.txt cat /proc/partitions
cap meta/modules.txt cat /proc/modules
cap meta/devices.txt cat /proc/devices

: > "$OUT/meta/tool-versions.txt"
: > "$OUT/meta/tool-availability.txt"
for tool in lspci setpci acpidump iasl dmidecode lscpu lsblk modetest drm_info \
	edid-decode intel_reg cpuid i2cget nvme flashrom; do
	if command -v "$tool" >/dev/null 2>&1; then
		{
			printf '=== %s ===\n' "$tool"
			"$tool" --version 2>&1 | head -3
		} >> "$OUT/meta/tool-versions.txt"
		printf 'present: %s\n' "$tool" >> "$OUT/meta/tool-availability.txt"
	else
		printf 'MISSING: %s\n' "$tool" >> "$OUT/meta/tool-availability.txt"
	fi
done
record OK meta/tool-versions.txt ""
record OK meta/tool-availability.txt ""

# ---------------------------------------------------------------------------
# DMI / SMBIOS: who this machine is.
# ---------------------------------------------------------------------------

say "stage 2/9: DMI and firmware identity"
cap dmi/dmidecode-full.txt dmidecode
for type in 0 1 2 3 4 7 8 9 11 16 17 19 20 32 41 43; do
	cap "dmi/dmidecode-type-$type.txt" dmidecode -t "$type"
done
cap_tree dmi/sysfs-id /sys/class/dmi/id
cap_tree firmware/efi-vars /sys/firmware/efi/efivars
cap firmware/efi-systab.txt cat /sys/firmware/efi/systab
if [ -f /sys/firmware/efi/efivars/SecureBoot-8be4df61-93ca-11d2-aa0d-00e098032b8c ]; then
	cap_sh firmware/secure-boot.txt \
		"od -An -tu1 -j4 -N1 /sys/firmware/efi/efivars/SecureBoot-8be4df61-93ca-11d2-aa0d-00e098032b8c"
fi

# ---------------------------------------------------------------------------
# ACPI: the headline.  Raw tables, the kernel's decode, an independent decode
# of MCFG, and a checksum audit of every table.
# ---------------------------------------------------------------------------

say "stage 3/9: ACPI tables"
cap_tree acpi/tables /sys/firmware/acpi/tables
cap_tree acpi/dynamic /sys/firmware/acpi/tables/dynamic
cap_sh acpi/table-sizes.txt "for table in /sys/firmware/acpi/tables/*; do [ -f \"\$table\" ] || continue; printf '%8s  %s\n' \"\$(wc -c < \$table)\" \"\$(basename \$table)\"; done"
cap acpi/acpidump.txt acpidump
cap acpi/acpidump-mcfg.txt acpidump -n MCFG
mkdir -p "$OUT/acpi/iasl"
for table in "$OUT"/acpi/tables/DSDT "$OUT"/acpi/tables/SSDT*; do
	[ -f "$table" ] || continue
	name=$(basename "$table")
	cap_sh "acpi/iasl/$name.dsl" "iasl -d -p '$OUT/acpi/iasl/$name' '$table' >/dev/null 2>&1; cat '$OUT/acpi/iasl/$name.dsl' 2>/dev/null"
done

# ACPI checksum audit: every table's bytes must sum to 0 mod 256.  A table
# that fails this is evidence of a firmware bug and must not be trusted.
if [ -d "$OUT/acpi/tables" ]; then
	: > "$OUT/acpi/checksums.txt"
	for table in "$OUT"/acpi/tables/*; do
		[ -f "$table" ] || continue
		sum=$(od -An -tu1 -v "$table" 2>/dev/null |
			awk '{ for (i = 1; i <= NF; i++) total += $i } END { printf "%d", total % 256 }')
		bytes=$(wc -c < "$table" | tr -d ' ')
		if [ "$sum" = "0" ]; then
			printf 'ok    %-10s %8s bytes\n' "$(basename "$table")" "$bytes" >> "$OUT/acpi/checksums.txt"
		else
			printf 'BAD   %-10s %8s bytes  (sum mod 256 = %s)\n' "$(basename "$table")" "$bytes" "$sum" >> "$OUT/acpi/checksums.txt"
		fi
	done
	record OK acpi/checksums.txt ""
fi

# Independent MCFG decode.  struct acpi_table_mcfg is a 36-byte header plus 8
# reserved bytes, so the allocation list starts at offset 44 and each entry is
# 16 bytes: base address (u64 LE), PCI segment group (u16 LE), start bus (u8),
# end bus (u8).  This is decoded with od and awk only, deliberately not with
# the same code that writes the rest of the bundle.
if [ -f "$OUT/acpi/tables/MCFG" ]; then
	{
		printf 'MCFG decoded from %s\n' "$OUT/acpi/tables/MCFG"
		printf 'signature: %s\n' "$(od -An -tc -j0 -N4 "$OUT/acpi/tables/MCFG" | tr -d ' ')"
		printf 'length:    %s bytes\n' "$(od -An -tu4 -j4 -N4 "$OUT/acpi/tables/MCFG" | tr -d ' ')"
		printf 'revision:  %s\n' "$(od -An -tu1 -j8 -N1 "$OUT/acpi/tables/MCFG" | tr -d ' ')"
		printf '\n'
		printf 'allocation list (offset 44, 16 bytes per entry):\n'
		od -An -tu1 -v -j44 "$OUT/acpi/tables/MCFG" |
			awk '{
				for (i = 1; i <= NF; i++) b[++n] = $i
			} END {
				entries = int(n / 16)
				if (entries == 0) printf "  none\n"
				for (e = 0; e < entries; e++) {
					o = e * 16 + 1
					base = 0
					for (k = 7; k >= 0; k--) base = base * 256 + b[o + k]
					segment = b[o + 8] + b[o + 9] * 256
					printf "  entry %d: ecam_base=0x%x segment=%d bus=%02x-%02x\n", \
						e, base, segment, b[o + 10], b[o + 11]
				}
			}'
		printf '\n'
		printf 'first allocation bytes (offset 44..59):\n'
		od -An -tx1 -v -j44 -N16 "$OUT/acpi/tables/MCFG"
	} > "$OUT/acpi/mcfg-decoded.txt"
	record OK acpi/mcfg-decoded.txt ""
else
	record_file acpi/mcfg-decoded.txt UNAVAILABLE \
		"no MCFG table: this machine does not advertise ECAM via ACPI"
fi

# The kernel's own interpretation, and the memory windows it derived from it.
cap_sh acpi/kernel-ecam.txt "dmesg | grep -iE 'ECAM|MCFG|PCI:.*bus|pci_bus'"
cap_sh proc/pci-windows.txt "grep -iE 'PCI Bus|PCI ECAM|PCIe' /proc/iomem"

# ---------------------------------------------------------------------------
# Memory and I/O maps.
# ---------------------------------------------------------------------------

say "stage 4/9: memory and I/O maps"
cap proc/iomem.txt cat /proc/iomem
cap proc/ioports.txt cat /proc/ioports
cap proc/meminfo.txt cat /proc/meminfo
cap proc/buddyinfo.txt cat /proc/buddyinfo
cap proc/zoneinfo.txt cat /proc/zoneinfo
cap proc/vmallocinfo.txt cat /proc/vmallocinfo
cap proc/interrupts.txt cat /proc/interrupts
cap proc/softirqs.txt cat /proc/softirqs
cap proc/timer_list.txt cat /proc/timer_list
cap proc/pagetypeinfo.txt cat /proc/pagetypeinfo
cap_sh sysfs/memmap.txt "for entry in /sys/firmware/memmap/*; do [ -d \"\$entry\" ] || continue; printf '%s %s %s %s\n' \"\$(cat \$entry/start)\" \"\$(cat \$entry/end)\" \"\$(cat \$entry/type)\" \"\$entry\"; done"
cap_sh sysfs/memory-blocks.txt "cat /sys/devices/system/memory/block_size_bytes; grep -c . /sys/devices/system/memory/*/state 2>/dev/null | head -1"

# ---------------------------------------------------------------------------
# PCI: enumeration, configuration space, topology.
# ---------------------------------------------------------------------------

say "stage 5/9: PCI"
cap pci/lspci-nnvvv.txt lspci -nnvvv
cap pci/lspci-nn.txt lspci -nn
cap pci/lspci-tree.txt lspci -t
cap pci/lspci-drivers.txt lspci -nnk
cap pci/lspci-config-space.txt lspci -xxxx
mkdir -p "$OUT/pci/config"
: > "$OUT/pci/bars.txt"
for device in /sys/bus/pci/devices/*; do
	[ -d "$device" ] || continue
	bdf=$(basename "$device")
	# Raw configuration space as the kernel itself exposes it.  256-byte
	# reads step through the extended space correctly; a single 4 KiB read
	# is refused by some kernels.
	dd if="$device/config" of="$OUT/pci/config/$bdf.bin" bs=256 count=16 2>/dev/null
	{
		printf '===== %s =====\n' "$bdf"
		for attribute in class vendor device revision subsystem_vendor subsystem_device \
			irq numa_node enable broken_parity_status msi_bus d3cold_allowed \
			local_cpulist local_cpus modalias current_link_speed current_link_width \
			max_link_speed max_link_width secondary_bus_number subordinate_bus_number \
			ari_enabled resource; do
			if [ -r "$device/$attribute" ]; then
				printf '%-24s %s\n' "$attribute" "$(cat "$device/$attribute" 2>/dev/null | tr '\n' '|')"
			fi
		done
		printf '%-24s %s\n' driver "$(basename "$(readlink "$device/driver" 2>/dev/null)" 2>/dev/null)"
		printf '%-24s %s\n' iommu_group "$(basename "$(readlink "$device/iommu_group" 2>/dev/null)" 2>/dev/null)"
		printf '%-24s %s\n' numa_node "$(cat "$device/numa_node" 2>/dev/null)"
	} >> "$OUT/sysfs/pci-devices.txt" 2>/dev/null
	if [ -L "$device/driver" ]; then
		printf '%s %s\n' "$bdf" "$(basename "$(readlink "$device/driver")")" >> "$OUT/pci/drivers.txt"
	fi
done
[ -f "$OUT/pci/drivers.txt" ] || : > "$OUT/pci/drivers.txt"
record OK pci/config "$(find "$OUT/pci/config" -type f | wc -l | tr -d ' ') devices"

# ---------------------------------------------------------------------------
# CPU topology.
# ---------------------------------------------------------------------------

say "stage 6/9: CPU topology"
cap cpu/lscpu.txt lscpu
cap cpu/lscpu-parsable.txt lscpu -p
cap cpu/lscpu-extended.txt lscpu -e
cap proc/cpuinfo.txt cat /proc/cpuinfo
cap_sh cpu/topology.txt "for cpu in /sys/devices/system/cpu/cpu[0-9]*; do printf '===== %s =====\n' \"\$(basename \$cpu)\"; for f in topology/core_id topology/physical_package_id topology/die_id topology/cluster_id topology/thread_siblings_list topology/core_cpus_list topology/book_id topology/drawer_id cpu_capacity online; do [ -r \"\$cpu/\$f\" ] && printf '%-32s %s\n' \"\$f\" \"\$(cat \$cpu/\$f)\"; done; done"
cap_sh cpu/hybrid.txt "for pmu in /sys/bus/event_source/devices/*; do printf '%s\n' \"\$(basename \$pmu)\"; done"
cap_sh cpu/sysfs-summary.txt "for f in present possible online offline isolated smt/active; do printf '%-24s %s\n' \"\$f\" \"\$(cat /sys/devices/system/cpu/\$f 2>/dev/null)\"; done"
cap_sh cpu/cache.txt "for index in /sys/devices/system/cpu/cpu0/cache/index*; do [ -d \"\$index\" ] || continue; printf '=== %s ===\n' \"\$(basename \$index)\"; for f in level type size ways_of_associativity coherency_line_size shared_cpu_list; do printf '%-24s %s\n' \"\$f\" \"\$(cat \$index/\$f 2>/dev/null)\"; done; done"

# ---------------------------------------------------------------------------
# Displays: EDID of every connected sink, plus the kernel's framebuffer view.
# ---------------------------------------------------------------------------

say "stage 7/9: displays and framebuffer"
mkdir -p "$OUT/display/edid"
: > "$OUT/display/connectors.txt"
for connector in /sys/class/drm/card*-*; do
	[ -d "$connector" ] || continue
	name=$(basename "$connector")
	{
		printf '===== %s =====\n' "$name"
		for attribute in status enabled dpms modes connector_id subconnector; do
			if [ -r "$connector/$attribute" ]; then
				printf '%-16s %s\n' "$attribute" "$(cat "$connector/$attribute" 2>/dev/null | tr '\n' ' ')"
			fi
		done
	} >> "$OUT/display/connectors.txt"
	if [ -s "$connector/edid" ]; then
		cp "$connector/edid" "$OUT/display/edid/$name.edid" 2>/dev/null
		record OK "display/edid/$name.edid" "$(wc -c < "$connector/edid" | tr -d ' ') bytes"
		if command -v edid-decode >/dev/null 2>&1; then
			edid-decode "$connector/edid" > "$OUT/display/edid/$name.txt" 2>&1
			record OK "display/edid/$name.txt" "edid-decode"
		fi
	else
		record UNAVAILABLE "display/edid/$name.edid" "no EDID: nothing connected or no DDC"
	fi
done
if ! command -v edid-decode >/dev/null 2>&1; then
	record_file display/edid/README.txt UNAVAILABLE \
		"edid-decode is not packaged for this Alpine release; decode the .edid files on the development host"
fi
cap_sh display/sysfs-framebuffer.txt "for fb in /sys/class/graphics/fb*; do [ -d \"\$fb\" ] || continue; printf '===== %s =====\n' \"\$(basename \$fb)\"; for f in name modes virtual_size stride bits_per_pixel state blank; do printf '%-18s %s\n' \"\$f\" \"\$(cat \$fb/\$f 2>/dev/null | tr '\n' ' ')\"; done; done"
cap_dd display/fb0.raw /dev/fb0 32
cap display/modetest.txt modetest -a
cap graphics/drm-info.txt drm_info
cap graphics/drm-info.json drm_info -j

# ---------------------------------------------------------------------------
# Graphics: what the kernel's driver says about the integrated GPU.
# ---------------------------------------------------------------------------

say "stage 8/9: graphics driver state"
gpu_bdf=$(lspci -nn 2>/dev/null | awk '/VGA compatible controller|Display controller/ { print $1; exit }')
if [ -n "$gpu_bdf" ]; then
	cap graphics/lspci-gpu-vvv.txt lspci -vvv -s "$gpu_bdf"
	note graphics/gpu-bdf.txt "$gpu_bdf"
fi
cap graphics/dmesg-graphics.txt sh -c "dmesg | grep -iE 'i915|xe |drm|guc|huc|dmc|vbt|opregion|edid|hdmi|dp_|display|backlight'"
cap graphics/modinfo.txt modinfo i915
cap_sh graphics/module-parameters.txt "for parameter in /sys/module/i915/parameters/*; do [ -f \"\$parameter\" ] || continue; printf '%-28s %s\n' \"\$(basename \$parameter)\" \"\$(cat \$parameter 2>/dev/null)\"; done"
cap_sh graphics/driver-binding.txt "for driver in /sys/bus/pci/drivers/i915 /sys/bus/pci/drivers/xe; do printf '%s: ' \"\$driver\"; [ -d \"\$driver\" ] && echo present || echo absent; done; ls -l /sys/bus/pci/drivers/i915 2>/dev/null"
cap_sh graphics/backlight.txt "if [ ! -e /sys/class/backlight ]; then echo 'no backlight class on this machine'; exit 0; fi; for light in /sys/class/backlight/*; do [ -d \"\$light\" ] || continue; printf '===== %s =====\n' \"\$light\"; for f in brightness max_brightness actual_brightness type; do printf '%-20s %s\n' \"\$f\" \"\$(cat \$light/\$f 2>/dev/null)\"; done; done"

# The driver's own debugfs surface.  This is the closest thing to a register
# dump that is available: intel-gpu-tools (intel_reg) is not packaged for
# Alpine 3.24, so the bundle carries what i915 itself reports instead, which
# includes its display and engine state.
mkdir -p /sys/kernel/debug
if mount -t debugfs none /sys/kernel/debug 2>/dev/null || [ -d /sys/kernel/debug/dri ]; then
	for dir in /sys/kernel/debug/dri/*; do
		[ -d "$dir" ] || continue
		card=$(basename "$dir")
		cap_sh "graphics/debugfs-$card-listing.txt" "ls -l '$dir'"
		for entry in "$dir"/*; do
			[ -f "$entry" ] || continue
			cap_dd "graphics/debugfs-$card/$(basename "$entry")" "$entry" 4
		done
	done
	# The OpRegion carries the VBT, which is what a framebuffer driver needs.
	for opregion in /sys/kernel/debug/dri/*/i915_opregion; do
		[ -f "$opregion" ] || continue
		cap_dd "graphics/opregion-$(basename "$(dirname "$opregion")").bin" "$opregion" 1
	done
else
	record_file graphics/debugfs-UNAVAILABLE.txt UNAVAILABLE \
		"debugfs could not be mounted; the driver's debug state is not in this bundle"
fi

# ---------------------------------------------------------------------------
# Storage, network and kernel configuration.
# ---------------------------------------------------------------------------

say "stage 9/9: storage, network, kernel configuration"
cap storage/lsblk.txt lsblk -o NAME,SIZE,TYPE,FSTYPE,MOUNTPOINT,MODEL,SERIAL
cap_sh storage/block-devices.txt "for dev in /sys/block/*; do [ -d \"\$dev\" ] || continue; printf '===== %s =====\n' \"\$(basename \$dev)\"; for f in size removable rotational model state; do printf '%-14s %s\n' \"\$f\" \"\$(cat \$dev/\$f 2>/dev/null)\"; done; done"
cap_sh storage/nvme.txt "for nvme in /sys/class/nvme/nvme*; do [ -d \"\$nvme\" ] || continue; printf '===== %s =====\n' \"\$nvme\"; for f in model serial firmware_revision state; do printf '%-22s %s\n' \"\$f\" \"\$(cat \$nvme/\$f 2>/dev/null)\"; done; done"
cap_sh network/interfaces.txt "for netif in /sys/class/net/*; do [ -d \"\$netif\" ] || continue; printf '===== %s =====\n' \"\$(basename \$netif)\"; for f in address operstate carrier mtu type speed; do printf '%-14s %s\n' \"\$f\" \"\$(cat \$netif/\$f 2>/dev/null)\"; done; done"
cap_sh network/pci-net.txt "lspci -nn | grep -iE 'ethernet|network'"
cap_sh network/firmware.txt "dmesg | grep -iE 'iwlwifi|firmware|Direct firmware load'"
if [ -r /proc/config.gz ]; then
	cap_sh kernel/config.txt "zcat /proc/config.gz"
else
	found=""
	for candidate in /media/*/boot/config-*; do
		[ -f "$candidate" ] && found=$candidate && break
	done
	if [ -n "$found" ]; then
		cap_file kernel/config.txt "$found"
	else
		record_file kernel/config.txt UNAVAILABLE \
			"neither /proc/config.gz nor a boot-media config file is available"
	fi
fi
cap logs/dmesg.txt dmesg
cap_file logs/dmesg-file.txt /var/log/dmesg
cap_file logs/messages.txt /var/log/messages
cap_sh logs/kernel-warnings.txt "dmesg | grep -iE 'error|fail|warn|denied|unsupported|not found'"

# ---------------------------------------------------------------------------
# Finish: README, SUMMARY, manifest, tarball.
# ---------------------------------------------------------------------------

{
	printf 'N305 ground-truth capture bundle\n'
	printf '================================\n\n'
	printf 'Machine:   %s\n' "$(cat "$OUT/dmi/sysfs-id/product_name" 2>/dev/null)"
	printf 'Board:     %s\n' "$(cat "$OUT/dmi/sysfs-id/board_name" 2>/dev/null)"
	printf 'BIOS:      %s %s\n' "$(cat "$OUT/dmi/sysfs-id/bios_version" 2>/dev/null)" "$(cat "$OUT/dmi/sysfs-id/bios_date" 2>/dev/null)"
	printf 'Captured:  %s (UTC)\n' "$(date -u)"
	printf 'Payload:   version %s, sha256 %s\n\n' "$PAYLOAD_VERSION" "$(sha256sum "$0" 2>/dev/null | cut -d' ' -f1)"
	printf 'This directory was written by a throwaway Alpine Linux live environment\n'
	printf 'booted from the USB stick it lives on.  Nothing on the machine was\n'
	printf 'modified: every probe reads the kernel, /sys, /proc or ACPI.\n\n'
	printf 'Start here:\n'
	printf '  SUMMARY.txt          the headline facts, including the PCI ECAM base\n'
	printf '  capture-status.txt   OK / FAIL / UNAVAILABLE for every single probe\n'
	printf '  acpi/mcfg-decoded.txt  MCFG bytes decoded independently of any parser\n'
	printf '  acpi/checksums.txt   ACPI table checksum audit\n'
	printf '  MANIFEST.sha256      hashes of every file in this bundle\n\n'
	printf 'Reading it on the development host:\n'
	printf '  copy the whole n305-<timestamp> directory (or the .tar.gz beside it)\n'
	printf '  and run scripts/ci/hw_facts_bundle.py on it.\n\n'
	printf 'What this bundle cannot contain, and why:\n'
	printf '  * No serial console output: this machine has no serial port.  The\n'
	printf '    capture ran headless; the screen messages are not recorded here.\n'
	printf '  * No decoded EDID text: edid-decode is not packaged for Alpine 3.24.\n'
	printf '    The raw .edid bytes are here and are the authority.\n'
	printf '  * No intel_reg MMIO register dump: intel-gpu-tools is not packaged\n'
	printf '    for Alpine 3.24.  graphics/debugfs-*/ is what i915 reports itself.\n'
	printf '  * No i915 initialisation trace: tracing must be armed before the\n'
	printf '    driver loads, which the vendor boot configuration cannot do.\n'
	printf '    logs/messages.txt usually retains more early boot than dmesg.\n'
	printf '  * No SMBIOS table integrity check beyond dmidecode, and no ACPI\n'
	printf '    table beyond what the firmware published.\n'
} > "$OUT/README.txt"
record OK README.txt ""

{
	printf 'N305 capture summary\n'
	printf '====================\n\n'
	printf '== Identity ==\n'
	for field in sys_vendor product_name product_serial board_name bios_vendor bios_version bios_date; do
		value=$(cat "$OUT/dmi/sysfs-id/$field" 2>/dev/null)
		[ -n "$value" ] && printf '%-16s %s\n' "$field" "$value"
	done
	printf '\n== CPU ==\n'
	grep -m1 'model name' "$OUT/proc/cpuinfo.txt" 2>/dev/null
	printf 'logical cpus: %s\n' "$(grep -c '^processor' "$OUT/proc/cpuinfo.txt" 2>/dev/null)"
	printf '\n== PCI ECAM (from ACPI MCFG, decoded from raw bytes) ==\n'
	if [ -f "$OUT/acpi/mcfg-decoded.txt" ]; then
		grep -E 'ecam_base|none' "$OUT/acpi/mcfg-decoded.txt" 2>/dev/null
	else
		printf 'MCFG table absent\n'
	fi
	printf '\n== ECAM as the kernel reported it ==\n'
	grep -iE 'ECAM' "$OUT/acpi/kernel-ecam.txt" 2>/dev/null | head -5
	printf '\n== PCI Bus windows from /proc/iomem ==\n'
	head -20 "$OUT/proc/pci-windows.txt" 2>/dev/null
	printf '\n== Graphics ==\n'
	cat "$OUT/graphics/gpu-bdf.txt" 2>/dev/null
	lspci -nn -s "$(cat "$OUT/graphics/gpu-bdf.txt" 2>/dev/null)" 2>/dev/null
	printf 'driver: %s\n' "$(grep -m1 'Kernel driver in use' "$OUT/pci/lspci-nnvvv.txt" 2>/dev/null)"
	grep -iE 'dmc|guc|huc' "$OUT/graphics/dmesg-graphics.txt" 2>/dev/null | head -5
	printf '\n== Displays ==\n'
	cat "$OUT/display/connectors.txt" 2>/dev/null
	printf '\n'
	for edid in "$OUT"/display/edid/*.edid; do
		[ -f "$edid" ] || continue
		printf '%s: %s bytes of EDID\n' "$(basename "$edid")" "$(wc -c < "$edid" | tr -d ' ')"
	done
	printf '\n== Storage ==\n'
	grep -iE 'nvme|disk' "$OUT/storage/lsblk.txt" 2>/dev/null | head -5
	printf '\n== Network ==\n'
	cat "$OUT/network/pci-net.txt" 2>/dev/null
	printf '\n== ACPI table integrity ==\n'
	grep -c '^ok' "$OUT/acpi/checksums.txt" 2>/dev/null | sed 's/^/tables with a valid checksum: /'
	grep '^BAD' "$OUT/acpi/checksums.txt" 2>/dev/null
	printf '\n== Probe status counts ==\n'
	cut -f1 "$STATUS" | sort | uniq -c | sort -rn
	printf '\n== Probes that failed or were unavailable ==\n'
	awk -F'\t' '$1 != "OK" { printf "%-12s %s %s\n", $1, $2, $3 }' "$STATUS"
} > "$OUT/SUMMARY.txt"
record OK SUMMARY.txt ""

say "hashing the bundle"
if command -v sha256sum >/dev/null 2>&1; then
	( cd "$OUT" && find . -type f ! -name MANIFEST.sha256 | sort | xargs sha256sum ) \
		> "$OUT/MANIFEST.sha256" 2>/dev/null
	record OK MANIFEST.sha256 "$(wc -l < "$OUT/MANIFEST.sha256" | tr -d ' ') files"
else
	record UNAVAILABLE MANIFEST.sha256 "no sha256sum in this live environment"
fi

bundle_bytes=$(du -sk "$OUT" 2>/dev/null | cut -f1)
if [ "${bundle_bytes:-0}" -gt $((BUNDLE_MIB_LIMIT * 1024)) ]; then
	record WARNING bundle-size "bundle is ${bundle_bytes} KiB"
fi

if command -v tar >/dev/null 2>&1; then
	say "packing the tarball"
	if ( cd "$MNT/dump" && tar -czf "n305-$STAMP.tar.gz" "n305-$STAMP" ); then
		record OK tarball "n305-$STAMP.tar.gz"
	else
		record FAIL "../n305-$STAMP.tar.gz" "tar failed"
	fi
fi
printf '%s\n' "n305-$STAMP" > "$MNT/dump/LATEST.txt" 2>/dev/null

failures=$(awk -F'\t' '$1 == "FAIL"' "$STATUS" | wc -l | tr -d ' ')
unavailable=$(awk -F'\t' '$1 == "UNAVAILABLE"' "$STATUS" | wc -l | tr -d ' ')
ok=$(awk -F'\t' '$1 == "OK"' "$STATUS" | wc -l | tr -d ' ')
sync

say "CAPTURE COMPLETE: $ok probes ok, $unavailable unavailable, $failures failed"
say "bundle: $OUT"
if [ "$failures" != "0" ]; then
	say "some probes failed; see capture-status.txt in the bundle"
fi
say "powering off in 10 seconds - a machine that stays on has failed"

# Graceful poweroff first so the FAT is unmounted cleanly; force it if the
# init system does not cooperate.  The watchdog dies with the graceful path.
(
	sleep 60
	sync
	poweroff -f
) &
sleep 10
sync
umount "$MNT" 2>/dev/null
sync
poweroff
sleep 5
sync
poweroff -f
